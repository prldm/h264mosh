use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use h264mosh::effects::MbEffect;
use h264mosh::mb::{self, MbContext, MbType};
use h264mosh::mosh::{MoshPlan, decode_pocs, mosh};
use h264mosh::motion;
use h264mosh::nal::nal_type_name;
use h264mosh::pipeline::{self, EncodeSettings, PrepProgress, Tools};
use h264mosh::range::{DupSpec, RangeSet};
use h264mosh::slice::SliceType;
use h264mosh::stream::{AccessUnit, NalKind, Stream, group_access_units};
use h264mosh::verify;
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// many small, short-lived allocations on all threads (macroblock data): the system
// allocator serializes them, mimalloc scales
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(name = "h264mosh", version, about = "H.264 bitstream-level datamoshing")]
struct Cli {
    /// ffmpeg binary (default: $H264MOSH_FFMPEG, else ffmpeg on PATH; ffprobe is taken from the same folder)
    #[arg(long, global = true, value_name = "PATH")]
    ffmpeg: Option<PathBuf>,
    /// worker threads for parsing and effects (default: all logical CPUs)
    #[arg(long, global = true, value_name = "N")]
    threads: Option<usize>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Encode clips with mosh-friendly settings (ffmpeg/libx264) and join them into one raw stream
    Prep {
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        #[arg(short, long)]
        output: PathBuf,
        /// output size WxH (default: size of the first input)
        #[arg(long, value_parser = parse_size)]
        size: Option<(u32, u32)>,
        /// frame rate, e.g. 25 or 24000/1001 (default: rate of the first input)
        #[arg(long)]
        fps: Option<String>,
        #[arg(long, default_value_t = 18.0)]
        crf: f32,
        #[arg(long, default_value = "medium")]
        preset: String,
        /// maximum keyframe interval ("infinite" allowed)
        #[arg(long, default_value = "250")]
        keyint: String,
        /// no keyframes at scene cuts (only every --keyint frames)
        #[arg(long)]
        no_scenecut: bool,
        /// CAVLC instead of CABAC entropy coding
        #[arg(long)]
        cavlc: bool,
        /// extra libx264 options in x264-params syntax, e.g. "deblock=0:me=dia"
        #[arg(long, value_name = "K=V:K=V")]
        x264_params: Option<String>,
    },
    /// Edit frames at the bitstream level (frame numbers are 0-based, decode order)
    Mosh {
        input: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        /// drop I-frames in FRAMES (default without value: every keyframe after the first)
        #[arg(long, value_name = "FRAMES", num_args = 0..=1, default_missing_value = "1-")]
        drop_keyframes: Option<RangeSet>,
        /// drop any frames in FRAMES
        #[arg(long, value_name = "FRAMES")]
        drop: Option<RangeSet>,
        /// repeat each frame in FRAMES COUNT extra times, e.g. 120:30 (repeatable)
        #[arg(long, value_name = "FRAMES:COUNT")]
        dup: Vec<DupSpec>,
        /// switch the deblocking filter off in every slice
        #[arg(long)]
        no_deblock: bool,
        #[command(flatten)]
        fx: FxArgs,
    },
    /// Decode a (moshed) raw stream and encode it to a normal video file (ffmpeg/libx264)
    Render {
        input: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        /// frame rate (default: from the stream's SPS timing info, else 25)
        #[arg(long)]
        fps: Option<String>,
        #[arg(long, default_value_t = 16.0)]
        crf: f32,
        #[arg(long, default_value = "slow")]
        preset: String,
    },
    /// Show stream structure (raw .264 or any container ffmpeg can read)
    Info {
        input: PathBuf,
        /// print one line per frame
        #[arg(long)]
        frames: bool,
        /// print macroblock statistics per frame (CAVLC streams)
        #[arg(long)]
        mbs: bool,
    },
    /// Debug: one CSV line per slice (for comparing against other parsers)
    #[command(hide = true)]
    Slices { input: PathBuf },
    /// Check that parsing and re-writing reproduces the input bit for bit
    Roundtrip {
        input: PathBuf,
        /// also write the fully re-serialized stream here
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// also check the macroblock layer of CAVLC slices
        #[arg(long)]
        mb: bool,
    },
}

/// Macroblock effects (need a CAVLC stream: `prep --cavlc`). Motion vector effects
/// apply to P-frames.
#[derive(Args)]
struct FxArgs {
    /// multiply motion vectors by X (or X:Y)
    #[arg(long, value_name = "X[:Y]", value_parser = parse_f32_pair, allow_hyphen_values = true)]
    mv_scale: Option<(f32, f32)>,
    /// add DX:DY pixels to motion vectors (quarter pixels allowed, e.g. 2.25:-1)
    #[arg(long, value_name = "DX:DY", value_parser = parse_f32_pair, allow_hyphen_values = true)]
    mv_add: Option<(f32, f32)>,
    /// random motion vector offset of up to PX pixels per macroblock
    #[arg(long, value_name = "PX")]
    mv_jitter: Option<f32>,
    /// multiply the coded motion vector differences by X (or X:Y); errors cascade through prediction
    #[arg(long, value_name = "X[:Y]", value_parser = parse_f32_pair, allow_hyphen_values = true)]
    mvd_scale: Option<(f32, f32)>,
    /// replace intra macroblocks in P-frames by skipped ones, so moshed areas never heal
    #[arg(long)]
    kill_intra: bool,
    /// drop all residual data (pictures from prediction only)
    #[arg(long)]
    zero_residual: bool,
    /// frames the macroblock effects apply to (default: all)
    #[arg(long, value_name = "FRAMES")]
    fx_frames: Option<RangeSet>,
    /// seed for --mv-jitter
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// debug: code skipped macroblocks explicitly (must not change the picture)
    #[arg(long, hide = true)]
    explicit_skips: bool,
    /// debug: re-partition macroblocks as P_8x8 (must not change the picture)
    #[arg(long, hide = true)]
    split_8x8: bool,
}

impl FxArgs {
    fn effects(&self) -> Vec<MbEffect> {
        let quarter = |v: f32| (v * 4.0).round() as i32;
        let mut fx = Vec::new();
        if self.explicit_skips {
            fx.push(MbEffect::ExplicitSkips);
        }
        if self.split_8x8 {
            fx.push(MbEffect::SplitTo8x8);
        }
        if self.kill_intra {
            fx.push(MbEffect::KillIntra);
        }
        if self.zero_residual {
            fx.push(MbEffect::ZeroResidual);
        }
        if let Some((x, y)) = self.mv_scale {
            fx.push(MbEffect::MvScale(x, y));
        }
        if let Some((x, y)) = self.mv_add {
            fx.push(MbEffect::MvAdd(quarter(x), quarter(y)));
        }
        if let Some(px) = self.mv_jitter {
            fx.push(MbEffect::MvJitter(quarter(px)));
        }
        if let Some((x, y)) = self.mvd_scale {
            fx.push(MbEffect::MvdScale(x, y));
        }
        fx
    }
}

/// "X" (used for both) or "X:Y"
fn parse_f32_pair(s: &str) -> Result<(f32, f32)> {
    Ok(match s.split_once(':') {
        Some((x, y)) => (x.trim().parse()?, y.trim().parse()?),
        None => {
            let v = s.trim().parse()?;
            (v, v)
        }
    })
}

fn parse_size(s: &str) -> Result<(u32, u32)> {
    let (w, h) = s.split_once(['x', 'X']).context("expected WxH, e.g. 1280x720")?;
    let (w, h): (u32, u32) = (w.parse()?, h.parse()?);
    if w == 0 || h == 0 || w % 2 == 1 || h % 2 == 1 {
        bail!("size must be non-zero and even");
    }
    Ok((w, h))
}

/// "1, 5-9, 20"
fn compact_list(v: &[usize]) -> String {
    let mut parts = Vec::new();
    let mut i = 0;
    while i < v.len() {
        let mut j = i;
        while j + 1 < v.len() && v[j + 1] == v[j] + 1 {
            j += 1;
        }
        parts.push(if j > i { format!("{}-{}", v[i], v[j]) } else { v[i].to_string() });
        i = j + 1;
    }
    parts.join(", ")
}

fn load(tools: &Tools, input: &Path) -> Result<(Vec<u8>, Stream)> {
    let data = pipeline::load_annexb(tools, input)?;
    let stream = Stream::parse(&data)?;
    Ok((data, stream))
}

fn print_warnings(w: &[String]) {
    for x in w.iter().take(20) {
        eprintln!("warning: {x}");
    }
    if w.len() > 20 {
        eprintln!("warning: ... {} more", w.len() - 20);
    }
}

fn cmd_info(tools: &Tools, input: &Path, frames: bool, mbs: bool) -> Result<()> {
    let (data, stream) = load(tools, input)?;
    print_warnings(&stream.warnings);
    let mut types: BTreeMap<u8, usize> = BTreeMap::new();
    for n in &stream.nals {
        *types.entry(n.nal_type()).or_default() += 1;
    }
    println!("{}: {} bytes, {} NAL units", input.display(), data.len(), stream.nals.len());
    let nal_summary: Vec<String> = types.iter().map(|(t, c)| format!("{c} {}", nal_type_name(*t))).collect();
    println!("  NAL units: {}", nal_summary.join(", "));

    let mut seen_sps = Vec::new();
    let mut seen_pps = Vec::new();
    for n in &stream.nals {
        match &n.kind {
            NalKind::Sps(s) if !seen_sps.contains(s) => {
                let (w, h) = s.size();
                let fps = s.frame_rate().map_or("?".into(), |(n, d)| format!("{:.3}", n as f64 / d as f64));
                println!(
                    "  SPS {}: {} level {}.{}, {w}x{h}, {fps} fps, {} bit, POC type {}, frame_num {} bits, max refs {}{}",
                    s.sps_id,
                    s.profile_name(),
                    s.level_idc / 10,
                    s.level_idc % 10,
                    s.bit_depth_luma_minus8 + 8,
                    s.pic_order_cnt_type,
                    s.log2_max_frame_num(),
                    s.max_num_ref_frames,
                    if s.frame_mbs_only_flag { "" } else { ", interlaced" }
                );
                seen_sps.push(s.clone());
            }
            NalKind::Pps(p) if !seen_pps.contains(p) => {
                println!(
                    "  PPS {}: {}, 8x8 transform {}, weighted P {}, weighted B idc {}, deblocking control {}",
                    p.pps_id,
                    if p.entropy_coding_mode_flag { "CABAC" } else { "CAVLC" },
                    if p.ext.as_ref().is_some_and(|e| e.transform_8x8_mode_flag) { "on" } else { "off" },
                    if p.weighted_pred_flag { "on" } else { "off" },
                    p.weighted_bipred_idc,
                    if p.deblocking_filter_control_present_flag { "yes" } else { "no" },
                );
                seen_pps.push(p.clone());
            }
            _ => {}
        }
    }

    let aus = group_access_units(stream.nals);
    let pocs = decode_pocs(&aus);
    let mut counts: BTreeMap<char, usize> = BTreeMap::new();
    for au in &aus {
        *counts.entry(au.picture_type()).or_default() += 1;
    }
    let count_str: Vec<String> = counts.iter().map(|(t, c)| format!("{t}: {c}")).collect();
    println!("  frames: {} ({})", aus.len(), count_str.join(", "));
    let keyframes: Vec<usize> = (0..aus.len()).filter(|&i| aus[i].is_keyframe()).collect();
    println!("  keyframes: {}", compact_list(&keyframes));
    let reordered = pocs.windows(2).zip(aus.iter().skip(1)).any(|(p, au)| !au.is_idr() && matches!(p, [Some(a), Some(b)] if b <= a));
    println!("  frame reordering: {}", if reordered { "yes (B-frames)" } else { "no" });
    let bad = verify::frame_num_violations(&aus);
    if bad.is_empty() {
        println!("  frame_num: ok");
    } else {
        println!("  frame_num: {} violation(s) at frames {}", bad.len(), compact_list(&bad));
    }
    let gop: String = aus.iter().take(100).map(|a| a.picture_type()).collect();
    println!("  first frames: {gop}{}", if aus.len() > 100 { "..." } else { "" });

    if mbs {
        print_mb_stats(&aus);
    }
    if frames {
        println!(
            "\n  {:>6} {:>4} {:>4} {:>3} {:>9} {:>7} {:>6} {:>8}",
            "frame", "type", "idr", "ref", "frame_num", "poc", "slices", "bytes"
        );
        for (i, au) in aus.iter().enumerate() {
            let (fnum, poc) = au
                .first_slice()
                .map_or(("-".into(), "-".into()), |s| (s.header.frame_num.to_string(), pocs[i].map_or("-".into(), |p| p.to_string())));
            println!(
                "  {:>6} {:>4} {:>4} {:>3} {:>9} {:>7} {:>6} {:>8}",
                i,
                au.picture_type(),
                if au.is_idr() { "yes" } else { "" },
                au.nal_ref_idc(),
                fnum,
                poc,
                au.slices().count(),
                au.byte_size()
            );
        }
    }
    Ok(())
}

#[derive(Default)]
struct MbStats {
    intra: usize,
    inter: usize,
    skip: usize,
    /// mean motion vector length in pixels over inter macroblocks
    mean_mv: Option<f64>,
}

fn frame_mb_stats(au: &AccessUnit) -> Result<MbStats> {
    let mut st = MbStats::default();
    let (mut mv_sum, mut mv_n) = (0.0, 0);
    for s in au.slices() {
        let ctx = MbContext::new(s)?;
        let data = mb::parse_slice_data(s)?;
        for m in &data.mbs {
            match m.mb_type {
                MbType::Skip => st.skip += 1,
                t if t.is_intra() => st.intra += 1,
                _ => st.inter += 1,
            }
        }
        if ctx.slice_type == SliceType::P {
            for m in motion::decode_motion(&ctx, &data)?.iter().filter(|m| !m.is_intra()) {
                mv_sum += m.mv.iter().map(|v| ((v[0] * v[0] + v[1] * v[1]) as f64).sqrt() / 4.0).sum::<f64>() / 16.0;
                mv_n += 1;
            }
        }
    }
    st.mean_mv = (mv_n > 0).then(|| mv_sum / mv_n as f64);
    Ok(st)
}

/// Per frame: intra / inter / skipped macroblocks and mean motion of P-frames.
fn print_mb_stats(aus: &[AccessUnit]) {
    let stats: Vec<Result<MbStats>> = aus.par_iter().map(frame_mb_stats).collect();
    println!();
    println!("  {:>6} {:>4} {:>7} {:>7} {:>7} {:>9}", "frame", "type", "intra", "inter", "skip", "mean |mv|");
    for (i, (au, st)) in aus.iter().zip(stats).enumerate() {
        match st {
            Ok(st) => {
                let mean = st.mean_mv.map_or("-".into(), |m| format!("{m:.2} px"));
                println!("  {i:>6} {:>4} {:>7} {:>7} {:>7} {mean:>9}", au.picture_type(), st.intra, st.inter, st.skip);
            }
            Err(e) => {
                println!("  {i:>6} {:>4} {e:#}", au.picture_type());
                if i == 0 {
                    return;
                }
            }
        }
    }
}

fn cmd_roundtrip(tools: &Tools, input: &Path, output: Option<&PathBuf>, mb: bool) -> Result<()> {
    let (data, stream) = load(tools, input)?;
    print_warnings(&stream.warnings);
    let rep = verify::roundtrip(&data, &stream)?;
    println!("{}: {} NAL units", input.display(), stream.nals.len());
    for (name, (ok, bad)) in &rep.by_type {
        println!("  {name:>6}: {ok} identical, {bad} different");
    }
    println!("  passed through unparsed: {}", rep.passthrough);
    if rep.raw_tails > 0 {
        println!("  parameter sets with unparsed trailing data: {}", rep.raw_tails);
    }
    for (i, m) in &rep.mismatches {
        println!("  mismatch NAL #{i}: {m}");
    }
    println!("  passthrough write identical: {}", rep.file_identical);
    println!("  full re-serialization identical: {}", rep.reserialized_identical);
    if let Some(out) = output {
        std::fs::write(out, stream.write(true)?)?;
    }
    let mut ok = rep.file_identical && rep.reserialized_identical;
    if mb {
        let m = verify::mb_roundtrip(&stream);
        println!(
            "  macroblock layer: {} slices identical, {} different, {} unsupported (CABAC/MBAFF), {} macroblocks",
            m.identical, m.different, m.skipped, m.macroblocks
        );
        for (i, f) in &m.failures {
            println!("  mb mismatch NAL #{i}: {f}");
        }
        ok &= m.different == 0;
    }
    if !ok {
        bail!("roundtrip failed");
    }
    Ok(())
}

fn cmd_mosh(tools: &Tools, input: &Path, output: &Path, plan: MoshPlan) -> Result<()> {
    let (_, stream) = load(tools, input)?;
    let (out, rep) = mosh(stream, &plan)?;
    std::fs::write(output, out.write(false)?).with_context(|| format!("writing {}", output.display()))?;
    print_warnings(&rep.warnings);
    print_warnings(&rep.render.warnings);
    let failed: Vec<String> = rep.render.failed_effects.iter().map(|(_, r)| format!("effects skipped, {r}")).collect();
    print_warnings(&failed);
    println!("frames: {} -> {}", rep.frames_in, rep.render.frames_out);
    if !rep.dropped.is_empty() {
        println!("dropped: {}", compact_list(&rep.dropped));
    }
    for (f, c) in &rep.duplicated {
        println!("duplicated: frame {f} x{c}");
    }
    let fx = &rep.render.effects;
    if fx.slices > 0 {
        println!("macroblock effects: {} slices", fx.slices);
        if fx.slices_without_motion > 0 {
            println!("  motion effects skipped in {} I/B slices", fx.slices_without_motion);
        }
        if fx.intra_killed > 0 {
            println!("  intra macroblocks replaced: {}", fx.intra_killed);
        }
    }
    println!("frames rewritten: {}", rep.render.rewritten);
    println!("written {}", output.display());
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(n) = cli.threads {
        rayon::ThreadPoolBuilder::new().num_threads(n).build_global()?;
    }
    let tools = Tools::find(cli.ffmpeg.as_deref());
    match cli.cmd {
        Cmd::Prep { inputs, output, size, fps, crf, preset, keyint, no_scenecut, cavlc, x264_params } => {
            let enc = EncodeSettings { preset, crf, keyint, scenecut: !no_scenecut, cavlc, x264_params };
            let clips = pipeline::prep(&tools, &inputs, &output, size, fps, &enc, &mut |p| {
                match p {
                    PrepProgress::Start { width, height, fps, clips } => eprintln!("encoding {clips} clip(s) at {width}x{height}, {fps} fps"),
                    PrepProgress::Clip { index, path, .. } => eprintln!("  [{}/{}] {}", index + 1, inputs.len(), path.display()),
                    PrepProgress::Frames(_) => {}
                }
                true
            })?;
            println!("written {}", output.display());
            for (i, c) in clips.iter().enumerate() {
                println!(
                    "  clip {i}: frames {}-{} ({} frames) keyframes [{}]  {}",
                    c.first_frame,
                    c.first_frame + c.frames - 1,
                    c.frames,
                    compact_list(&c.keyframes),
                    c.input.display()
                );
            }
        }
        Cmd::Mosh { input, output, drop_keyframes, drop, dup, no_deblock, fx } => {
            let plan = MoshPlan {
                drop_keyframes,
                drop,
                dup,
                effects: fx.effects(),
                effect_frames: fx.fx_frames.clone(),
                no_deblock,
                seed: fx.seed,
            };
            if !plan.no_deblock && plan.drop_keyframes.is_none() && plan.drop.is_none() && plan.dup.is_empty() && plan.effects.is_empty() {
                bail!("nothing to do: give frame edits (--drop-keyframes, --drop, --dup), effects or --no-deblock");
            }
            cmd_mosh(&tools, &input, &output, plan)?;
        }
        Cmd::Render { input, output, fps, crf, preset } => {
            let data = std::fs::read(&input).with_context(|| format!("reading {}", input.display()))?;
            let fps = fps.unwrap_or_else(|| pipeline::stream_fps(&data));
            eprintln!("rendering {} -> {} ({fps} fps)", input.display(), output.display());
            pipeline::render(&tools, &data, &output, &fps, &preset, crf, &mut |_| true)?;
            println!("written {}", output.display());
        }
        Cmd::Info { input, frames, mbs } => cmd_info(&tools, &input, frames, mbs)?,
        Cmd::Slices { input } => {
            let (_, stream) = load(&tools, &input)?;
            println!(
                "nal_type,header_end_bit,first_mb_in_slice,slice_type,frame_num,pic_order_cnt_lsb,slice_qp_delta,disable_deblocking_filter_idc"
            );
            for n in &stream.nals {
                if let Some(s) = n.slice() {
                    let h = &s.header;
                    println!(
                        "{},{},{},{},{},{},{},{}",
                        n.nal_type(),
                        8 + s.header_bits(),
                        h.first_mb_in_slice,
                        h.slice_type_raw,
                        h.frame_num,
                        h.pic_order_cnt_lsb,
                        h.slice_qp_delta,
                        h.disable_deblocking_filter_idc
                    );
                }
            }
        }
        Cmd::Roundtrip { input, output, mb } => cmd_roundtrip(&tools, &input, output.as_ref(), mb)?,
    }
    Ok(())
}
