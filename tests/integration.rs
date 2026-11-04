//! End-to-end tests. Streams are generated with ffmpeg/libx264 and decoded with ffmpeg;
//! tests are skipped (with a message) when ffmpeg is not available.

use h264mosh::effects::{Fx, Mask, MbEffect};
use h264mosh::mb::MbContext;
use h264mosh::mosh::{MoshPlan, RenderOptions, Source, Timeline, TimelineEntry, mosh, render, segment};
use h264mosh::pipeline::{BPyramid, Cancelled, EncodeSettings, PrepProgress, Tools, decode_rgba, prep, remux};
use h264mosh::pps::Pps;
use h264mosh::residual::Scaling;
use h264mosh::slice::SliceType;
use h264mosh::stream::{NalKind, Stream, group_access_units};
use h264mosh::verify;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

/// ffmpeg binary: $H264MOSH_FFMPEG or `ffmpeg` on PATH (same rule as the tool).
fn ffmpeg() -> String {
    std::env::var("H264MOSH_FFMPEG").unwrap_or_else(|_| "ffmpeg".into())
}

fn ffmpeg_available() -> bool {
    let ok = Command::new(ffmpeg()).arg("-version").output().is_ok_and(|o| o.status.success());
    if !ok {
        eprintln!("ffmpeg not found, skipping");
    }
    ok
}

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("h264mosh-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// Encode `secs` seconds of a lavfi source with libx264 into a raw stream.
fn encode(source: &str, secs: u32, x264: &[&str]) -> Vec<u8> {
    let out = Command::new(ffmpeg())
        .args(["-v", "error", "-nostdin", "-f", "lavfi", "-i"])
        .arg(format!("{source}=size=320x192:rate=25"))
        .args(["-t", &secs.to_string(), "-c:v", "libx264"])
        .args(x264)
        .args(["-f", "h264", "-"])
        .output()
        .expect("ffmpeg");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

fn mosh_friendly(source: &str, secs: u32, extra: &[&str]) -> Vec<u8> {
    let mut args = vec!["-pix_fmt", "yuv420p", "-bf", "0", "-refs", "1", "-x264-params", "weightp=0:scenecut=0", "-g", "25"];
    args.extend_from_slice(extra);
    encode(source, secs, &args)
}

/// Decode with ffmpeg; returns per-frame MD5 lines and stderr.
fn decode(data: &[u8], name: &str) -> (Vec<String>, String) {
    let path = tmp(name);
    std::fs::write(&path, data).unwrap();
    let out =
        Command::new(ffmpeg()).args(["-v", "error", "-nostdin", "-i"]).arg(&path).args(["-f", "framemd5", "-"]).output().expect("ffmpeg");
    let md5s = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| l.rsplit(',').next().unwrap().trim().to_string())
        .collect();
    (md5s, String::from_utf8_lossy(&out.stderr).into_owned())
}

fn assert_bit_exact(data: &[u8]) {
    let stream = Stream::parse(data).unwrap();
    assert!(stream.warnings.is_empty(), "{:?}", stream.warnings);
    let rep = verify::roundtrip(data, &stream).unwrap();
    assert!(rep.file_identical && rep.reserialized_identical, "mismatches: {:?}", rep.mismatches);
    assert_eq!(rep.raw_tails, 0);
}

#[test]
fn roundtrip_encoder_variants() {
    if !ffmpeg_available() {
        return;
    }
    let variants: &[&[&str]] = &[
        &["-bf", "3", "-refs", "4"],
        &["-coder", "0", "-bf", "2"],
        &["-flags", "+ildct+ilme", "-bf", "2"],
        &["-pix_fmt", "yuv444p"],
        &["-slices", "4", "-coder", "0"],
        &["-x264-params", "cqm=jvt:open-gop=1:keyint=10"],
    ];
    for v in variants {
        assert_bit_exact(&encode("testsrc2", 2, v));
    }
}

/// Rewrite headers so that their length changes but their meaning does not; the
/// decoded pictures must stay identical. With CAVLC this exercises the bit shift of
/// the slice data, with CABAC the re-alignment.
#[test]
fn length_changing_header_rewrite_keeps_pixels() {
    if !ffmpeg_available() {
        return;
    }
    for cavlc in [false, true] {
        let extra: &[&str] = if cavlc { &["-coder", "0"] } else { &[] };
        let data = mosh_friendly("testsrc2", 2, extra);
        let (orig_md5, _) = decode(&data, "orig.264");
        assert_eq!(orig_md5.len(), 50);

        let mut stream = Stream::parse(&data).unwrap();
        // 1) drop deblocking_filter_control_present_flag (x264's default 0:0 deblock
        //    makes the per-slice fields redundant) -> every slice header shrinks
        let mut stripped: Option<Arc<Pps>> = None;
        for n in stream.nals.iter_mut() {
            match &mut n.kind {
                NalKind::Pps(p) => {
                    let new = Arc::new(Pps { deblocking_filter_control_present_flag: false, ..(**p).clone() });
                    stripped = Some(new.clone());
                    *p = new;
                    n.dirty = true;
                }
                NalKind::Slice(s) => {
                    assert_eq!(
                        (s.header.disable_deblocking_filter_idc, s.header.slice_alpha_c0_offset_div2, s.header.slice_beta_offset_div2),
                        (0, 0, 0)
                    );
                    s.pps = stripped.clone().unwrap();
                    // 2) explicit num_ref_idx override with the default value -> P headers grow
                    if s.header.slice_type() == SliceType::P {
                        s.header.num_ref_idx_active_override_flag = true;
                    }
                    n.dirty = true;
                }
                _ => {}
            }
        }
        let rewritten = stream.write(false).unwrap();
        assert_ne!(rewritten, data);
        let (md5, err) = decode(&rewritten, "rewritten.264");
        assert!(err.is_empty(), "decoder complained: {err}");
        assert_eq!(md5, orig_md5, "decoded pictures changed (cavlc={cavlc})");
        assert_bit_exact(&rewritten);

        // 3) --no-deblock on a PPS without deblocking control: the PPS gets the flag back
        let (out, _) = mosh(Stream::parse(&rewritten).unwrap(), &MoshPlan { no_deblock: true, ..Default::default() }).unwrap();
        let nodeblock = out.write(false).unwrap();
        let reparsed = Stream::parse(&nodeblock).unwrap();
        for n in &reparsed.nals {
            match &n.kind {
                NalKind::Pps(p) => assert!(p.deblocking_filter_control_present_flag),
                NalKind::Slice(s) => assert_eq!(s.header.disable_deblocking_filter_idc, 1),
                _ => {}
            }
        }
        let (md5, err) = decode(&nodeblock, "nodeblock.264");
        assert!(err.is_empty(), "decoder complained: {err}");
        assert_eq!(md5.len(), 50);
        assert_ne!(md5, orig_md5);
    }
}

#[test]
fn mosh_two_clips() {
    if !ffmpeg_available() {
        return;
    }
    let mut data = mosh_friendly("testsrc2", 2, &[]);
    data.extend(mosh_friendly("mandelbrot", 2, &[]));
    let frames_in = group_access_units(Stream::parse(&data).unwrap().nals).len();
    assert_eq!(frames_in, 100);

    let plan = MoshPlan {
        drop_keyframes: Some("1-".parse().unwrap()),
        dup: vec!["60:10".parse().unwrap(), "70-71:2".parse().unwrap()],
        ..Default::default()
    };
    let (out, rep) = mosh(Stream::parse(&data).unwrap(), &plan).unwrap();
    // keyframes at 25, 50 (clip 2 IDR) and 75 are dropped; 10 + 2*2 copies added
    assert_eq!(rep.dropped, vec![25, 50, 75]);
    assert_eq!(rep.render.frames_out, 100 - 3 + 14);
    let moshed = out.write(false).unwrap();

    let aus = group_access_units(Stream::parse(&moshed).unwrap().nals);
    assert_eq!(aus.len(), rep.render.frames_out);
    assert_eq!(aus.iter().filter(|a| a.is_keyframe()).count(), 1);
    assert!(verify::frame_num_violations(&aus).is_empty());

    let (md5, err) = decode(&moshed, "moshed.264");
    assert!(err.is_empty(), "decoder complained: {err}");
    assert_eq!(md5.len(), rep.render.frames_out);
    // duplicated P-frames keep applying their motion, so the copies differ from each other
    assert_ne!(md5[60], md5[61]);
}

#[test]
fn mosh_without_edits_is_identity() {
    if !ffmpeg_available() {
        return;
    }
    let data = mosh_friendly("testsrc2", 2, &[]);
    let plan = MoshPlan { drop_keyframes: Some("1000-".parse().unwrap()), ..Default::default() };
    let (out, rep) = mosh(Stream::parse(&data).unwrap(), &plan).unwrap();
    assert_eq!(rep.render.rewritten, 0);
    assert_eq!(out.write(false).unwrap(), data);
}

#[test]
fn mosh_edits_bframes_and_keeps_first_frame() {
    if !ffmpeg_available() {
        return;
    }
    let data = encode("testsrc2", 1, &["-bf", "2"]);
    let plan = MoshPlan { drop: Some("3".parse().unwrap()), ..Default::default() };
    let (out, rep) = mosh(Stream::parse(&data).unwrap(), &plan).unwrap();
    assert_eq!(rep.dropped, vec![3]);
    let (md5, err) = decode(&out.write(false).unwrap(), "bframe_drop.264");
    assert!(err.is_empty(), "{err}");
    assert_eq!(md5.len(), 24);
    // frame numbers are display positions: frame 5 three more times, frame 3 gone
    let src = Source::new(Stream::parse(&data).unwrap());
    assert_ne!(src.display, (0..src.len()).collect::<Vec<_>>());
    let plan = MoshPlan { drop: Some("3".parse().unwrap()), dup: vec!["5:3".parse().unwrap()], ..Default::default() };
    let (_, rep) = mosh(Stream::parse(&data).unwrap(), &plan).unwrap();
    let shown: Vec<usize> = rep.render.frames.iter().map(|f| f.src).collect();
    let want: Vec<usize> = [0, 1, 2, 4, 5, 5, 5, 5].into_iter().chain(6..25).map(|k| src.display[k]).collect();
    assert_eq!(shown, want);

    let data = mosh_friendly("testsrc2", 1, &[]);
    let plan = MoshPlan { drop: Some("0-1".parse().unwrap()), ..Default::default() };
    let (out, rep) = mosh(Stream::parse(&data).unwrap(), &plan).unwrap();
    assert_eq!(rep.dropped, vec![1]);
    let aus = group_access_units(out.nals);
    assert!(aus[0].is_idr());
}

/// Apply `effects` to every frame of the stream.
fn with_effects(data: &[u8], effects: &[MbEffect]) -> Vec<u8> {
    let src = Source::new(Stream::parse(data).unwrap());
    let mut timeline = Timeline::identity(src.len());
    timeline.entries.iter_mut().for_each(|e| e.effects = effects.iter().cloned().map(Fx::from).collect());
    let (out, rep) = render(&src, &timeline, &RenderOptions::default()).unwrap();
    assert_eq!(rep.effects.slices, src.aus.iter().map(|a| a.slices().count()).sum::<usize>());
    out.write(false).unwrap()
}

/// Restructuring macroblocks while keeping their motion must not change a single
/// pixel. This checks motion vector prediction (skip, median, directional, C->D
/// fallback, multiple references, slice boundaries) independently of our own decoder.
#[test]
fn motion_prediction_restructure_keeps_pixels() {
    if !ffmpeg_available() {
        return;
    }
    let configs = [
        ("testsrc2", "yuv420p", "bframes=0:ref=1:weightp=0:partitions=all"),
        ("mandelbrot", "yuv420p", "bframes=0:ref=3:partitions=all:subme=9"),
        ("testsrc2", "yuv420p", "bframes=0:ref=4:weightp=2:slices=3:partitions=all"),
        ("mandelbrot", "yuv444p", "bframes=0:ref=2:8x8dct=0"),
        ("mandelbrot", "yuv420p", "bframes=0:ref=2:partitions=all"),
    ];
    for (source, pix, params) in configs {
        for coder in ["0", "1"] {
            let args = ["-coder", coder, "-pix_fmt", pix, "-x264-params", params];
            restructure_keeps_pixels(source, &args);
        }
    }
}

fn restructure_keeps_pixels(source: &str, args: &[&str]) {
    {
        let data = encode(source, 2, args);
        let (orig, _) = decode(&data, "restructure_orig.264");
        for effects in [
            &[MbEffect::ExplicitSkips][..],
            &[MbEffect::SplitTo8x8],
            &[MbEffect::ExplicitSkips, MbEffect::SplitTo8x8],
            &[MbEffect::MvScale(1.0, 1.0)],
        ] {
            let out = with_effects(&data, effects);
            if effects != [MbEffect::MvScale(1.0, 1.0)] {
                assert_ne!(out, data, "{effects:?} should change the bitstream");
            } else {
                assert_eq!(out, data, "identity motion edit must be bit exact");
            }
            let (md5, err) = decode(&out, "restructure.264");
            assert!(err.is_empty(), "{source} {args:?} {effects:?}: decoder complained: {err}");
            assert_eq!(md5, orig, "{source} {args:?} {effects:?}: pictures changed");
            assert_bit_exact(&out);
        }
    }
}

#[test]
fn macroblock_effects_decode_cleanly() {
    if !ffmpeg_available() {
        return;
    }
    for coder in ["0", "1"] {
        macroblock_effects_decode_cleanly_with(coder);
    }
}

fn macroblock_effects_decode_cleanly_with(coder: &str) {
    let data = encode("mandelbrot", 2, &["-coder", coder, "-pix_fmt", "yuv420p", "-x264-params", "bframes=0:ref=2:partitions=all"]);
    let (orig, _) = decode(&data, "fx_orig.264");
    let cases: &[&[MbEffect]] = &[
        &[MbEffect::MvScale(2.0, -1.0)],
        &[MbEffect::MvAdd(37, -9)],
        &[MbEffect::MvJitter(24)],
        &[MbEffect::MvdScale(3.0, 3.0)],
        &[MbEffect::KillIntra],
        &[MbEffect::ZeroResidual],
        &[MbEffect::KillIntra, MbEffect::ZeroResidual, MbEffect::MvScale(1.5, 1.5), MbEffect::MvAdd(-1000, 4000)],
    ];
    for fx in cases {
        let out = with_effects(&data, fx);
        let (md5, err) = decode(&out, "fx.264");
        assert!(err.is_empty(), "{fx:?}: decoder complained: {err}");
        assert_eq!(md5.len(), orig.len());
        assert_ne!(md5, orig, "{fx:?} had no visible effect");
        assert_bit_exact(&out);
    }
    // kill-intra leaves no intra macroblocks in P-frames
    let out = Stream::parse(&with_effects(&data, &[MbEffect::KillIntra])).unwrap();
    for n in &out.nals {
        if let Some(s) = n.slice().filter(|s| s.header.slice_type() == SliceType::P) {
            let d = h264mosh::mb::parse_slice_data(s).unwrap();
            assert!(d.mbs.iter().all(|m| !m.mb_type.is_intra()));
        }
    }
}

#[test]
fn timeline_reorder_and_repeat() {
    if !ffmpeg_available() {
        return;
    }
    let data = mosh_friendly("testsrc2", 2, &["-coder", "0"]);
    let src = Source::new(Stream::parse(&data).unwrap());
    // frames 0..9, then 30..20 backwards, then the IDR at 25 twice (freeze), then 5
    let order: Vec<usize> = (0..10).chain((20..=30).rev()).chain([25, 25, 5]).collect();
    let mut timeline = Timeline::identity(0);
    for &s in &order {
        timeline.entries.push(TimelineEntry::new(s));
    }
    timeline.entries[12].effects = vec![MbEffect::MvScale(4.0, 4.0).into()];
    let (out, rep) = render(&src, &timeline, &RenderOptions::default()).unwrap();
    assert_eq!(rep.frames_out, order.len());
    let bytes = out.write(false).unwrap();
    let aus = group_access_units(Stream::parse(&bytes).unwrap().nals);
    assert!(verify::frame_num_violations(&aus).is_empty());
    let (md5, err) = decode(&bytes, "reorder.264");
    assert!(err.is_empty(), "decoder complained: {err}");
    assert_eq!(md5.len(), order.len());
    // the two consecutive copies of IDR frame 25 show the same picture
    let j = order.len() - 3;
    assert_eq!((order[j], order[j + 1]), (25, 25));
    assert_eq!(md5[j], md5[j + 1]);

    let bad = Timeline { entries: vec![TimelineEntry::new(3)] };
    assert!(render(&src, &bad, &RenderOptions::default()).is_err(), "must start with an IDR frame");
}

/// A GUI re-decodes only from the last IDR before an edit: decoding that segment
/// alone must give exactly the frames of a full decode.
#[test]
fn segments_decode_like_the_full_stream() {
    if !ffmpeg_available() {
        return;
    }
    let data = mosh_friendly("mandelbrot", 3, &["-coder", "0"]);
    let src = Source::new(Stream::parse(&data).unwrap());
    // drop the IDR at 25, freeze-repeat the IDR at 50, scale motion of a few frames
    let mut order: Vec<usize> = (0..25).chain(26..50).collect();
    order.extend([50, 50, 50]);
    order.extend(51..75);
    let mut timeline = Timeline { entries: order.iter().map(|&s| TimelineEntry::new(s)).collect() };
    for e in &mut timeline.entries[30..35] {
        e.effects = vec![MbEffect::MvScale(3.0, 3.0).into()];
    }
    let (out, rep) = render(&src, &timeline, &RenderOptions::default()).unwrap();
    assert_eq!(rep.frames.iter().map(|f| f.src).collect::<Vec<_>>(), order);
    let (bytes, offsets) = out.write_with_offsets(false).unwrap();
    assert_eq!(bytes, out.write(false).unwrap());
    assert_eq!(offsets.len(), out.nals.len());
    let (full, err) = decode(&bytes, "segment_full.264");
    assert!(err.is_empty(), "{err}");

    for k in [10, 40, 50, 51, 60, order.len() - 1] {
        let r = rep.restart_point(k);
        assert!(rep.frames[r].idr && r <= k);
        let (md5, err) = decode(&segment(&out, &rep.frames, r..k + 1).unwrap(), "segment.264");
        assert!(err.is_empty(), "segment {r}..={k}: {err}");
        assert_eq!(md5, full[r..=k], "segment {r}..={k}");
    }
    // the freeze copies of IDR 50 are restart points themselves (VCL only, sets prepended)
    let last_copy = order.iter().rposition(|&s| s == 50).unwrap();
    assert_eq!(rep.restart_point(last_copy), last_copy);
    assert_eq!(rep.restart_point(last_copy + 1), last_copy);
    assert!(segment(&out, &rep.frames, 0..order.len() + 1).is_err());
}

/// A frame whose effects fail is left unchanged; the rest of the render goes on.
#[test]
fn failing_frame_effects_are_skipped() {
    if !ffmpeg_available() {
        return;
    }
    let data = mosh_friendly("testsrc2", 1, &["-coder", "0"]);
    let mut src = Source::new(Stream::parse(&data).unwrap());
    // corrupt the slice data of frame 10: all zero bits are no valid Exp-Golomb code
    for n in src.aus[10].nals.iter_mut() {
        if let Some(s) = n.slice_mut() {
            s.set_slice_data(vec![0; 8], 64).unwrap();
        }
    }
    let mut timeline = Timeline::identity(src.len());
    timeline.entries.iter_mut().for_each(|e| e.effects = vec![MbEffect::MvScale(2.0, 2.0).into()]);
    let (_, rep) = render(&src, &timeline, &RenderOptions::default()).unwrap();
    assert_eq!(rep.failed_effects.len(), 1, "{:?}", rep.failed_effects);
    assert_eq!(rep.failed_effects[0].0, 10);
    assert_eq!(rep.effects.slices, src.len() - 1);
}

/// Randomized effects are tied to timeline entries: inserting a frame earlier in the
/// timeline must not change the jitter of later frames.
#[test]
fn jitter_is_stable_when_the_timeline_changes() {
    if !ffmpeg_available() {
        return;
    }
    let data = mosh_friendly("mandelbrot", 1, &["-coder", "0"]);
    let src = Source::new(Stream::parse(&data).unwrap());
    let mut a = Timeline::identity(src.len());
    a.entries.iter_mut().for_each(|e| e.effects = vec![MbEffect::MvJitter(40).into()]);
    let mut b = a.clone();
    b.entries.insert(3, TimelineEntry { src: 2, effects: vec![MbEffect::MvJitter(40).into()], seed: 99 });
    let slice_data = |tl: &Timeline, k: usize| {
        let (out, rep) = render(&src, tl, &RenderOptions::default()).unwrap();
        let slice = out.nals[rep.frames[k].nals.clone()].iter().find_map(|n| n.slice()).unwrap().clone();
        h264mosh::mb::parse_slice_data(&slice).unwrap().mbs.iter().map(|m| m.mvd).collect::<Vec<_>>()
    };
    for k in [5, 10, 20] {
        assert_eq!(slice_data(&a, k), slice_data(&b, k + 1), "frame {k} changed after an insert before it");
    }
    // and different seeds do give different jitter
    let mut c = a.clone();
    c.entries[10].seed ^= 1;
    assert_ne!(slice_data(&a, 10), slice_data(&c, 10));
}

/// Previews are decoded with `pipeline::decode_rgba`: one picture per frame, in order,
/// also for moshed streams, and a segment gives the same pictures as the full stream.
#[test]
fn rgba_decode_gives_one_picture_per_frame() {
    if !ffmpeg_available() {
        return;
    }
    let tools = Tools::find(None);
    let data = mosh_friendly("mandelbrot", 3, &["-coder", "0"]);
    let src = Source::new(Stream::parse(&data).unwrap());
    // keyframe 25 dropped, frame 60 repeated, strong effects on a stretch of P-frames
    let mut order: Vec<usize> = (0..75).filter(|&s| s != 25).collect();
    let at = order.iter().position(|&s| s == 60).unwrap();
    order.splice(at..at, [60; 5]);
    let mut timeline = Timeline { entries: order.iter().map(|&s| TimelineEntry::new(s)).collect() };
    for e in &mut timeline.entries[30..40] {
        e.effects = vec![MbEffect::MvScale(5.0, -3.0).into(), MbEffect::KillIntra.into()];
    }
    let (out, rep) = render(&src, &timeline, &RenderOptions::default()).unwrap();
    let size = (64, 40);
    let decode_all = |bytes: &[u8]| {
        let mut pictures = Vec::new();
        let n = decode_rgba(&tools, bytes, size, |i, px| {
            assert_eq!((i, px.len()), (pictures.len(), 64 * 40 * 4));
            pictures.push(px);
            true
        })
        .unwrap();
        assert_eq!(n, pictures.len());
        pictures
    };
    let full = decode_all(&out.write(false).unwrap());
    assert_eq!(full.len(), order.len());
    let k = order.len() - 1;
    let r = rep.restart_point(k);
    assert!(r > 0);
    assert!(decode_all(&segment(&out, &rep.frames, r..k + 1).unwrap()) == full[r..=k], "segment {r}..={k}");

    // stopping early ends the decoder without an error
    let mut seen = 0;
    assert_eq!(
        decode_rgba(&tools, &data, size, |_, _| {
            seen += 1;
            seen < 3
        })
        .unwrap(),
        3
    );
}

/// `prep` reports progress and can be cancelled.
#[test]
fn prep_reports_progress_and_cancels() {
    if !ffmpeg_available() {
        return;
    }
    let tools = Tools::find(None);
    let clip = tmp("prep_in.mp4");
    let status = Command::new(ffmpeg())
        .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i", "testsrc2=size=320x192:rate=25", "-t", "2", "-c:v", "libx264"])
        .arg(&clip)
        .status()
        .unwrap();
    assert!(status.success());
    let enc = EncodeSettings {
        preset: "ultrafast".into(),
        crf: 23.0,
        keyint: "250".into(),
        scenecut: true,
        cavlc: true,
        refs: 1,
        bframes: 0,
        b_pyramid: BPyramid::None,
        x264_params: None,
    };
    let inputs = [clip.clone(), clip];
    let out = tmp("prep_out.264");
    let mut events = Vec::new();
    let clips = prep(&tools, &inputs, &out, None, None, &enc, &mut |p| {
        events.push(match p {
            PrepProgress::Start { width, height, clips, .. } => format!("start {width}x{height} {clips}"),
            PrepProgress::Clip { index, frames, .. } => format!("clip {index} {frames:?}"),
            PrepProgress::Frames(n) => format!("frames {n}"),
        });
        true
    })
    .unwrap();
    assert_eq!(clips.iter().map(|c| c.frames).collect::<Vec<_>>(), [50, 50]);
    assert_eq!(events[..2], ["start 320x192 2", "clip 0 Some(50)"]);
    assert!(events.contains(&"frames 50".to_string()), "{events:?}");
    assert!(events.contains(&"clip 1 Some(50)".to_string()), "{events:?}");

    let err = prep(&tools, &inputs, &out, None, None, &enc, &mut |p| !matches!(p, PrepProgress::Frames(_))).unwrap_err();
    assert!(err.downcast_ref::<Cancelled>().is_some(), "{err:#}");
}

/// A moshed stream put into mp4 without re-encoding decodes to the same pictures.
#[test]
fn remux_keeps_the_moshed_pictures() {
    if !ffmpeg_available() {
        return;
    }
    let data = mosh_friendly("mandelbrot", 3, &["-coder", "0"]);
    let src = Source::new(Stream::parse(&data).unwrap());
    // keyframes 25 and 50 dropped, a freeze, motion effects: parameter sets get re-inserted mid-stream
    let mut order: Vec<usize> = (0..75).filter(|&s| s != 25 && s != 50).collect();
    order.splice(30..30, [30; 4]);
    let mut timeline = Timeline { entries: order.iter().map(|&s| TimelineEntry::new(s)).collect() };
    for e in &mut timeline.entries[40..50] {
        e.effects = vec![MbEffect::MvScale(3.0, -2.0).into()];
    }
    let (out, _) = render(&src, &timeline, &RenderOptions::default()).unwrap();
    let bytes = out.write(false).unwrap();
    let (raw, err) = decode(&bytes, "remux.264");
    assert!(err.is_empty(), "{err}");
    let mp4 = tmp("remux.mp4");
    remux(&Tools::find(None), &bytes, &mp4, "25", &mut |_| true).unwrap();
    let out = Command::new(ffmpeg()).args(["-v", "error", "-nostdin", "-i"]).arg(&mp4).args(["-f", "framemd5", "-"]).output().unwrap();
    let md5: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| l.rsplit(',').next().unwrap().trim().to_string())
        .collect();
    assert_eq!(md5.len(), order.len());
    assert!(md5 == raw, "remuxed pictures differ");
}

/// Streams covering what the new macroblock effects depend on: 4x4 and 8x8 transforms,
/// slices, constrained intra prediction, chroma formats, bit depth, scaling matrices
/// and several reference frames, each with CAVLC and with CABAC.
fn mb_configs() -> Vec<(String, Vec<u8>)> {
    let x = |coder: &'static str, params: &str, extra: &[&'static str]| {
        let mut args = vec!["-coder", coder, "-g", "25", "-x264-params"];
        let params = format!("bframes=0:weightp=0:scenecut=0:{params}");
        args.push(&params);
        args.extend_from_slice(extra);
        let pix = if extra.contains(&"-pix_fmt") { vec![] } else { vec!["-pix_fmt", "yuv420p"] };
        args.extend(pix);
        encode("testsrc2", 2, &args)
    };
    let mut v = Vec::new();
    for (coder, entropy) in [("0", "cavlc"), ("1", "cabac")] {
        for (name, params, extra) in [
            ("8x8", "ref=1", &[][..]),
            ("4x4", "ref=1:8x8dct=0", &[]),
            ("slices+constrained", "ref=1:slices=3:constrained-intra=1", &[]),
            ("ref3", "ref=3", &[]),
            ("cqm", "ref=1:cqm=jvt", &[]),
            ("i444", "ref=1", &["-pix_fmt", "yuv444p"]),
            ("i422", "ref=1", &["-pix_fmt", "yuv422p"]),
            ("10bit", "ref=1", &["-pix_fmt", "yuv420p10le"]),
        ] {
            v.push((format!("{name} {entropy}"), x(coder, params, extra)));
        }
    }
    v
}

/// Every new macroblock effect, on every frame: the result decodes without a single
/// complaint, has all its frames, looks different, and parses back bit for bit.
#[test]
fn new_effects_decode_cleanly() {
    if !ffmpeg_available() {
        return;
    }
    let effects = [
        MbEffect::IntraMode(0),
        MbEffect::IntraMode(4),
        MbEffect::IntraMode(8),
        MbEffect::IntraRotate(3),
        MbEffect::IntraRandom(0.7),
        MbEffect::IntraCascade(5),
        MbEffect::QpAdd(18),
        MbEffect::QpAdd(-12),
        MbEffect::QpSet(51),
        MbEffect::Coeffs(0.0, 6.0),
        MbEffect::Coeffs(0.3, 0.0),
        MbEffect::Coeffs(0.0, -1.0),
        MbEffect::DcShift(30, 20, -20),
        MbEffect::Transplant(30, 0.6),
        MbEffect::MotionTransplant(40, 1.0),
        MbEffect::MotionTransplant(12, 0.5),
    ];
    for (name, data) in mb_configs() {
        let (orig, _) = decode(&data, "new_fx_orig.264");
        let mut cases: Vec<Vec<MbEffect>> = effects.iter().map(|e| vec![e.clone()]).collect();
        cases.push(vec![MbEffect::QpAdd(30), MbEffect::Coeffs(0.0, 8.0), MbEffect::DcShift(60, 60, 60)]);
        if name.starts_with("ref3") {
            cases.push(vec![MbEffect::RefShift(2)]);
        }
        for fx in cases {
            let out = with_effects(&data, &fx);
            let (md5, err) = decode(&out, "new_fx.264");
            assert!(err.is_empty(), "{name} {fx:?}: decoder complained: {err}");
            assert_eq!(md5.len(), orig.len(), "{name} {fx:?}");
            assert_ne!(md5, orig, "{name} {fx:?} had no visible effect");
            assert_bit_exact(&out);
        }
    }
}

/// Effects that change nothing must not change a bit: this re-codes intra directions,
/// quantizers, reference indices and (transplanting a frame into itself) whole
/// macroblocks against their neighbours.
#[test]
fn identity_effects_are_bit_exact() {
    if !ffmpeg_available() {
        return;
    }
    for (name, data) in mb_configs() {
        for fx in
            [MbEffect::IntraRotate(0), MbEffect::QpAdd(0), MbEffect::Coeffs(0.0, 1.0), MbEffect::RefShift(0), MbEffect::DcShift(0, 0, 0)]
        {
            assert!(with_effects(&data, std::slice::from_ref(&fx)) == data, "{name} {fx:?} changed the stream");
        }
        let src = Source::new(Stream::parse(&data).unwrap());
        let mut timeline = Timeline::identity(src.len());
        for k in [0, 7, 30] {
            timeline.entries[k].effects = vec![MbEffect::Transplant(k, 1.0).into()];
        }
        for k in [8, 31] {
            timeline.entries[k].effects = vec![MbEffect::MotionTransplant(k, 1.0).into()];
        }
        let (out, rep) = render(&src, &timeline, &RenderOptions::default()).unwrap();
        assert!(rep.failed_effects.is_empty(), "{:?}", rep.failed_effects);
        assert!(out.write(false).unwrap() == data, "{name}: transplanting frames into themselves changed them");
    }
}

/// Every intra direction, also where a block lacks the samples it needs (picture and
/// slice edges, inter neighbours with constrained intra prediction): ffmpeg rejects
/// such directions, so a clean decode shows they were replaced.
#[test]
fn intra_directions_respect_available_samples() {
    if !ffmpeg_available() {
        return;
    }
    for coder in ["0", "1"] {
        let all_intra = encode("testsrc2", 1, &["-coder", coder, "-pix_fmt", "yuv420p", "-x264-params", "keyint=1:slices=4"]);
        let constrained =
            encode("mandelbrot", 1, &["-coder", coder, "-pix_fmt", "yuv420p", "-x264-params", "bframes=0:constrained-intra=1:8x8dct=0"]);
        for (name, data) in [("all intra", &all_intra), ("constrained", &constrained)] {
            for d in 0..=8 {
                let out = with_effects(data, &[MbEffect::IntraMode(d)]);
                let (_, err) = decode(&out, "intra_dirs.264");
                assert!(err.is_empty(), "{name}, coder {coder}, direction {d}: {err}");
            }
        }
    }
}

/// Decode to 8-bit luma planes.
fn decode_luma(data: &[u8], name: &str) -> Vec<Vec<u8>> {
    let path = tmp(name);
    std::fs::write(&path, data).unwrap();
    let out = Command::new(ffmpeg())
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(&path)
        .args(["-pix_fmt", "gray", "-f", "rawvideo", "-"])
        .output()
        .expect("ffmpeg");
    assert!(out.stderr.is_empty(), "{}", String::from_utf8_lossy(&out.stderr));
    out.stdout.chunks(320 * 192).map(<[u8]>::to_vec).collect()
}

/// A masked effect changes the masked area and leaves the rest of the picture alone.
#[test]
fn masked_effects_leave_the_rest_alone() {
    if !ffmpeg_available() {
        return;
    }
    for coder in ["0", "1"] {
        masked_effects_leave_the_rest_alone_with(coder);
    }
}

fn masked_effects_leave_the_rest_alone_with(coder: &str) {
    let data = mosh_friendly("mandelbrot", 1, &["-coder", coder]);
    let src = Source::new(Stream::parse(&data).unwrap());
    let orig = decode_luma(&data, "mask_orig.264");
    let left_half = Mask::rect(0.0, 0.0, 0.5, 1.0);
    for effect in [MbEffect::ZeroResidual, MbEffect::MvScale(3.0, -2.0), MbEffect::QpAdd(12), MbEffect::Coeffs(0.0, 4.0)] {
        let mut timeline = Timeline::identity(src.len());
        timeline.entries[5].effects = vec![Fx { effect: effect.clone(), mask: Some(left_half.clone()) }];
        let (out, _) = render(&src, &timeline, &RenderOptions::default()).unwrap();
        let luma = decode_luma(&out.write(false).unwrap(), "mask.264");
        assert_eq!(luma[..5], orig[..5], "{effect:?}");
        let cols = |p: &[u8], from: usize, to: usize| -> Vec<u8> { p.chunks(320).flat_map(|row| row[from..to].to_vec()).collect() };
        // the right quarter, away from the edge of the mask and its deblocking
        assert_eq!(cols(&luma[5], 240, 320), cols(&orig[5], 240, 320), "{effect:?} changed pixels outside the mask");
        assert_ne!(cols(&luma[5], 0, 150), cols(&orig[5], 0, 150), "{effect:?} changed nothing inside the mask");
    }
    // uncoded macroblocks outside keep their quantizer, and with it their deblocking:
    // only the edge of the mask is filtered differently
    let mut timeline = Timeline::identity(src.len());
    timeline.entries[5].effects = vec![Fx { effect: MbEffect::QpAdd(20), mask: Some(left_half.clone()) }];
    let (out, _) = render(&src, &timeline, &RenderOptions::default()).unwrap();
    let luma = decode_luma(&out.write(false).unwrap(), "mask_qp.264");
    let cols = |p: &[u8]| -> Vec<u8> { p.chunks(320).flat_map(|row| row[164..].to_vec()).collect() };
    assert_eq!(cols(&luma[5]), cols(&orig[5]));
    // an inverted mask spares the inside
    let mut timeline = Timeline::identity(src.len());
    timeline.entries[5].effects = vec![Fx { effect: MbEffect::ZeroResidual, mask: Some(Mask { invert: true, ..left_half }) }];
    let (out, _) = render(&src, &timeline, &RenderOptions::default()).unwrap();
    let luma = decode_luma(&out.write(false).unwrap(), "mask_inv.264");
    let cols = |p: &[u8]| -> Vec<u8> { p.chunks(320).flat_map(|row| row[..80].to_vec()).collect() };
    assert_eq!(cols(&luma[5]), cols(&orig[5]));
}

/// Encoders keep their levels within the value ranges of the inverse transform; the
/// check effects rely on must agree, also at extreme quantizers, with scaling
/// matrices and in 4:4:4, and what effects produce must pass it.
#[test]
fn residual_ranges_accept_encoder_output() {
    if !ffmpeg_available() {
        return;
    }
    let mut streams = mb_configs();
    for (name, params, pix) in [
        ("qp1", "qp=1", "yuv420p"),
        ("qp1 444", "qp=1:8x8dct=1", "yuv444p"),
        ("lossless", "qp=0", "yuv420p"),
        ("cqm 444", "cqm=jvt:qp=4", "yuv444p"),
        ("qp1 10bit", "qp=1", "yuv422p10le"),
    ] {
        let params = format!("bframes=0:{params}");
        for coder in ["0", "1"] {
            streams.push((
                format!("{name} coder {coder}"),
                encode("testsrc2", 1, &["-coder", coder, "-pix_fmt", pix, "-x264-params", &params]),
            ));
        }
    }
    let check = |name: &str, data: &[u8]| {
        let stream = Stream::parse(data).unwrap();
        let mut checked = 0;
        for s in stream.nals.iter().filter_map(|n| n.slice()) {
            let ctx = MbContext::new(s).unwrap();
            let scaling = Scaling::new(&ctx);
            for (i, mb) in h264mosh::mb::parse_slice_data(s).unwrap().mbs.iter().enumerate().filter(|(_, m)| m.has_residual()) {
                assert!(scaling.fits(mb, mb.qp), "{name}: macroblock {i} of a slice does not fit at QP {}", mb.qp);
                checked += 1;
            }
        }
        assert!(checked > 100, "{name}");
    };
    for (name, data) in &streams {
        check(name, data);
        check(name, &with_effects(data, &[MbEffect::QpAdd(40), MbEffect::Coeffs(0.0, 8.0), MbEffect::DcShift(200, -200, 200)]));
    }
}

/// Deblocking and weighted prediction only rewrite slice headers: they work on CABAC
/// streams, and switching the PPS flags they need on does not change the other frames.
#[test]
fn slice_level_effects_work_on_any_stream() {
    if !ffmpeg_available() {
        return;
    }
    for extra in [&[][..], &["-coder", "0"]] {
        let data = mosh_friendly("testsrc2", 2, extra);
        let (orig, _) = decode(&data, "slice_fx_orig.264");
        let src = Source::new(Stream::parse(&data).unwrap());
        let mut timeline = Timeline::identity(src.len());
        for e in &mut timeline.entries[10..20] {
            e.effects = vec![MbEffect::Weight(1.2, 6).into(), MbEffect::Tint(4, -4).into()];
        }
        for e in &mut timeline.entries[30..40] {
            e.effects = vec![MbEffect::Deblock(6, 6).into()];
        }
        let (out, rep) = render(&src, &timeline, &RenderOptions::default()).unwrap();
        assert!(rep.failed_effects.is_empty(), "{:?}", rep.failed_effects);
        let bytes = out.write(false).unwrap();
        let (md5, err) = decode(&bytes, "slice_fx.264");
        assert!(err.is_empty(), "{extra:?}: {err}");
        assert_eq!(md5[..10], orig[..10], "{extra:?}: frames without effects changed");
        assert_ne!(md5[10], orig[10]);
        // the drift lasts until the next keyframe (25)
        assert_ne!(md5[24], orig[24]);
        assert_eq!(md5[25..30], orig[25..30]);
        assert_ne!(md5[35], orig[35]);
        assert_bit_exact(&bytes);
        let stream = Stream::parse(&bytes).unwrap();
        for n in &stream.nals {
            if let NalKind::Pps(p) = &n.kind {
                assert!(p.weighted_pred_flag && p.deblocking_filter_control_present_flag);
            }
        }
    }
}

/// With several reference frames, a frame moved next to a keyframe may refer to more
/// frames than there are; the reference list is filled up, so the stream stays valid.
#[test]
fn multi_reference_streams_survive_frame_edits() {
    if !ffmpeg_available() {
        return;
    }
    for extra in [&["-coder", "0"][..], &[]] {
        let mut args = vec!["-pix_fmt", "yuv420p", "-bf", "0", "-refs", "3", "-x264-params", "weightp=0:scenecut=0", "-g", "25"];
        args.extend_from_slice(extra);
        let data = encode("mandelbrot", 2, &args);
        let src = Source::new(Stream::parse(&data).unwrap());
        // frames 1-9 dropped, frame 40 right after keyframe 25, frame 0 frozen
        let order: Vec<usize> = [0, 0].into_iter().chain(10..25).chain([25, 40]).chain(26..40).chain(41..50).collect();
        let mut timeline = Timeline { entries: order.iter().map(|&s| TimelineEntry::new(s)).collect() };
        for e in &mut timeline.entries[3..12] {
            e.effects = vec![MbEffect::RefShift(2).into()];
        }
        let (out, rep) = render(&src, &timeline, &RenderOptions::default()).unwrap();
        assert!(rep.failed_effects.is_empty(), "{:?}", rep.failed_effects);
        let bytes = out.write(false).unwrap();
        let (md5, err) = decode(&bytes, "multi_ref.264");
        assert!(err.is_empty(), "{extra:?}: {err}");
        assert_eq!(md5.len(), order.len());
        let aus = group_access_units(Stream::parse(&bytes).unwrap().nals);
        assert!(verify::frame_num_violations(&aus).is_empty());
    }
}

/// Macroblocks copied from another frame show up, also intra ones in a keyframe; a
/// frame that cannot be copied from fails only the frames that use it.
#[test]
fn transplant_copies_macroblocks() {
    if !ffmpeg_available() {
        return;
    }
    for coder in ["0", "1"] {
        transplant_copies_macroblocks_with(coder);
    }
}

fn transplant_copies_macroblocks_with(coder: &str) {
    let data = mosh_friendly("mandelbrot", 2, &["-coder", coder]);
    let (orig, _) = decode(&data, "transplant_orig.264");
    let src = Source::new(Stream::parse(&data).unwrap());
    let mut timeline = Timeline::identity(src.len());
    timeline.entries[0].effects = vec![MbEffect::Transplant(25, 1.0).into()];
    timeline.entries[10].effects = vec![MbEffect::Transplant(40, 0.5).into()];
    timeline.entries[30].effects = vec![MbEffect::Transplant(999, 1.0).into()];
    // weighted prediction switched on in the PPS of every frame does not stop it
    timeline.entries[45].effects = vec![MbEffect::Weight(1.1, 0).into()];
    let (out, rep) = render(&src, &timeline, &RenderOptions::default()).unwrap();
    assert_eq!(rep.failed_effects.iter().map(|f| f.0).collect::<Vec<_>>(), [30], "{:?}", rep.failed_effects);
    let (md5, err) = decode(&out.write(false).unwrap(), "transplant.264");
    assert!(err.is_empty(), "{err}");
    // keyframe 25 copied over keyframe 0 entirely
    assert_eq!(md5[0], orig[25]);
    assert_ne!(md5[10], orig[10]);
}

#[test]
fn prep_can_use_several_reference_frames() {
    if !ffmpeg_available() {
        return;
    }
    let clip = tmp("prep_refs_in.mp4");
    let status = Command::new(ffmpeg())
        .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i", "mandelbrot=size=320x192:rate=25", "-t", "1", "-c:v", "libx264"])
        .arg(&clip)
        .status()
        .unwrap();
    assert!(status.success());
    let enc = EncodeSettings {
        preset: "ultrafast".into(),
        crf: 23.0,
        keyint: "250".into(),
        scenecut: true,
        cavlc: true,
        refs: 3,
        bframes: 0,
        b_pyramid: BPyramid::None,
        x264_params: None,
    };
    let out = tmp("prep_refs_out.264");
    prep(&Tools::find(None), &[clip], &out, None, None, &enc, &mut |_| true).unwrap();
    let stream = Stream::parse(&std::fs::read(&out).unwrap()).unwrap();
    let sps = stream.nals.iter().find_map(|n| n.slice()).unwrap().sps.clone();
    assert_eq!(sps.max_num_ref_frames, 3);
    let used = stream.nals.iter().filter_map(|n| n.slice()).any(|s| s.header.num_ref_idx_l0_active_minus1 == 2);
    assert!(used);
}

/// Streams with B-frames: x264 without and with reference B-frames (b-pyramid, which
/// brings MMCO and reference list modifications), CABAC and CAVLC, spatial and temporal
/// direct prediction. The first one predicts B-frames without weights from picture
/// order count distances, so its pictures can be compared after moving B-frames.
fn bframe_configs() -> Vec<(&'static str, Vec<u8>)> {
    [
        ("b_none", "bframes=3:b-pyramid=none:weightb=0:scenecut=0"),
        ("b_none_cavlc", "bframes=3:b-pyramid=none:scenecut=0:cabac=0"),
        ("b_normal", "bframes=3:b-pyramid=normal:scenecut=0"),
        ("b_strict", "bframes=4:b-pyramid=strict:ref=3:scenecut=0"),
        ("b_temporal_cavlc", "bframes=3:b-pyramid=normal:direct=temporal:scenecut=0:cabac=0"),
    ]
    .into_iter()
    .map(|(name, params)| (name, encode("mandelbrot", 2, &["-pix_fmt", "yuv420p", "-g", "25", "-x264-params", params])))
    .collect()
}

/// Render `timeline` and check the result as a decoder sees it: one picture per entry
/// and no complaints, frame_num and picture order counts in order, and the display
/// order is the timeline's. Returns the pictures' MD5s.
fn render_checked(src: &Source, timeline: &Timeline, name: &str) -> Vec<String> {
    let (out, rep) = render(src, timeline, &RenderOptions::default()).unwrap();
    let bytes = out.write(false).unwrap();
    let aus = group_access_units(Stream::parse(&bytes).unwrap().nals);
    assert_eq!(aus.len(), timeline.entries.len(), "{name}");
    assert!(verify::frame_num_violations(&aus).is_empty(), "{name}");
    let poc = verify::poc_check(&aus);
    assert!(poc.duplicates.is_empty() && poc.reordered_too_far.is_empty(), "{name}: {poc:?}");
    // the output frame decoded as each access unit
    let mut by_start: Vec<(usize, usize)> = rep.frames.iter().enumerate().map(|(k, f)| (f.nals.start, k)).collect();
    by_start.sort();
    let shown: Vec<usize> = poc.display.iter().map(|&i| by_start[i].1).collect();
    assert_eq!(shown, (0..timeline.entries.len()).collect::<Vec<_>>(), "{name}: display order");
    let (md5, err) = decode(&bytes, &format!("{name}.264"));
    assert!(err.is_empty(), "{name}: {err}");
    assert_eq!(md5.len(), timeline.entries.len(), "{name}");
    md5
}

/// A timeline of input frames given by their display positions.
fn shown(src: &Source, positions: impl IntoIterator<Item = usize>) -> Timeline {
    Timeline { entries: positions.into_iter().map(|p| TimelineEntry::new(src.display[p])).collect() }
}

#[test]
fn bframe_renumbering_keeps_the_pictures() {
    if !ffmpeg_available() {
        return;
    }
    for (name, data) in bframe_configs() {
        let src = Source::new(Stream::parse(&data).unwrap());
        assert!(src.aus.iter().any(|a| a.picture_type() == 'B'), "{name}");
        let (orig, _) = decode(&data, "b_orig.264");
        assert_eq!(orig.len(), src.len());
        // unchanged: the input as it is
        let (out, rep) = render(&src, &src.timeline(), &RenderOptions::default()).unwrap();
        assert_eq!(rep.rewritten, 0);
        assert_eq!(out.write(false).unwrap(), data, "{name}");
        // renumbered (the first frame once more at the end): every header comes out the
        // same, and so do the pictures
        let mut timeline = src.timeline();
        timeline.entries.push(TimelineEntry::new(0));
        let (_, rep) = render(&src, &timeline, &RenderOptions::default()).unwrap();
        assert_eq!(rep.rewritten, 0, "{name}");
        let md5 = render_checked(&src, &timeline, name);
        assert_eq!(md5[..orig.len()], orig[..], "{name}");
        assert_eq!(md5.last(), orig.first(), "{name}");
    }
}

#[test]
fn bframe_edits_decode_cleanly() {
    if !ffmpeg_available() {
        return;
    }
    for (name, data) in bframe_configs() {
        let src = Source::new(Stream::parse(&data).unwrap());
        let n = src.len();
        let kind = |p: usize| src.aus[src.display[p]].picture_type();
        let is_ref = |p: usize| src.aus[src.display[p]].is_reference();
        let b = (1..n).find(|&p| kind(p) == 'B').unwrap();
        let p_frame = (n / 2..n).find(|&p| kind(p) == 'P').unwrap();
        let idr = (1..n).find(|&p| src.aus[src.display[p]].is_idr()).unwrap();
        let bs: Vec<usize> = (0..n).filter(|&p| kind(p) == 'B').collect();
        let cases: Vec<(&str, Vec<usize>)> = vec![
            // a B-frame shown among other frames
            ("b_moved", (0..b).chain(b + 1..b + 12).chain([b]).chain(b + 12..n).collect()),
            // a P-frame shown earlier, its B-frames left behind
            ("p_moved", (0..5).chain([p_frame]).chain(5..p_frame).chain(p_frame + 1..n).collect()),
            ("reversed", (0..4).chain((4..30).rev()).chain(30..n).collect()),
            ("repeated", (0..b).chain([b; 8]).chain(b..p_frame).chain([p_frame; 5]).chain(p_frame..n).collect()),
            ("idr_dropped", (0..idr).chain(idr + 1..n).collect()),
            ("no_b", (0..n).filter(|&p| kind(p) != 'B').collect()),
            ("only_b", [0].into_iter().chain(bs.iter().copied()).collect()),
            // B-frames of the next GOP before its IDR frame
            ("across_idr", (0..idr).chain(idr + 1..idr + 4).chain([idr]).chain(idr + 4..n).collect()),
            ("twice", (0..n).chain(0..n).collect()),
        ];
        for (case, order) in cases {
            render_checked(&src, &shown(&src, order), &format!("{name}_{case}"));
        }
        // two non-reference B-frames swapped within their group show each other's picture
        if name == "b_none" {
            let (orig, _) = decode(&data, "b_swap_orig.264");
            let p = (1..n - 1).find(|&p| kind(p) == 'B' && kind(p + 1) == 'B' && !is_ref(p) && !is_ref(p + 1)).unwrap();
            let md5 = render_checked(&src, &shown(&src, (0..p).chain([p + 1, p]).chain(p + 2..n)), "b_swapped");
            assert_eq!((&md5[p], &md5[p + 1]), (&orig[p + 1], &orig[p]));
            assert_eq!(md5[..p], orig[..p]);
            assert_eq!(md5[p + 2..], orig[p + 2..]);
        }
    }
}

/// The GUI decodes from the last IDR frame before the frame it shows; with B-frames
/// that segment must show the same pictures as the full stream.
#[test]
fn bframe_segments_decode_like_the_full_stream() {
    if !ffmpeg_available() {
        return;
    }
    let (_, data) = bframe_configs().swap_remove(2);
    let src = Source::new(Stream::parse(&data).unwrap());
    let n = src.len();
    let timeline = shown(&src, (0..10).chain((10..40).rev()).chain(40..n));
    let full = render_checked(&src, &timeline, "b_segment_full");
    let (out, rep) = render(&src, &timeline, &RenderOptions::default()).unwrap();
    for k in [5, 20, 30, 45] {
        let r = rep.restart_point(k);
        assert!(rep.frames[r].idr && r <= k);
        let (md5, err) = decode(&segment(&out, &rep.frames, r..n).unwrap(), "b_segment.264");
        assert!(err.is_empty(), "{err}");
        assert_eq!(md5, full[r..], "segment from {r}");
    }
}

#[test]
fn prep_can_use_bframes() {
    if !ffmpeg_available() {
        return;
    }
    let clip = tmp("prep_bframes_in.mp4");
    let status = Command::new(ffmpeg())
        .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i", "mandelbrot=size=320x192:rate=25", "-t", "1", "-c:v", "libx264"])
        .arg(&clip)
        .status()
        .unwrap();
    assert!(status.success());
    let enc = EncodeSettings {
        preset: "ultrafast".into(),
        crf: 23.0,
        keyint: "250".into(),
        scenecut: true,
        cavlc: true,
        refs: 1,
        bframes: 3,
        b_pyramid: BPyramid::None,
        x264_params: None,
    };
    let out = tmp("prep_bframes_out.264");
    prep(&Tools::find(None), &[clip], &out, None, None, &enc, &mut |_| true).unwrap();
    let src = Source::new(Stream::parse(&std::fs::read(&out).unwrap()).unwrap());
    let bs: Vec<_> = src.aus.iter().filter(|a| a.picture_type() == 'B').collect();
    assert!(!bs.is_empty());
    assert!(bs.iter().all(|a| !a.is_reference()), "b-pyramid none: no reference B-frames");
    let n = src.len();
    render_checked(&src, &shown(&src, (0..3).chain((3..n).rev())), "prep_bframes");
}

/// Copied into a container, a stream with B-frames keeps every picture in its place.
#[test]
fn remux_keeps_bframe_pictures() {
    if !ffmpeg_available() {
        return;
    }
    for (name, data) in bframe_configs().into_iter().skip(1).take(2) {
        let src = Source::new(Stream::parse(&data).unwrap());
        let n = src.len();
        for (case, timeline) in [("orig", src.timeline()), ("edit", shown(&src, (0..10).chain((10..40).rev()).chain(40..n)))] {
            let (out, _) = render(&src, &timeline, &RenderOptions::default()).unwrap();
            let bytes = out.write(false).unwrap();
            let (raw, _) = decode(&bytes, "remux_b.264");
            let mp4 = tmp("remux_b.mp4");
            remux(&Tools::find(None), &bytes, &mp4, "24000/1001", &mut |_| true).unwrap();
            let (md5, err) = decode(&std::fs::read(&mp4).unwrap(), "remux_b.mp4");
            assert!(err.is_empty(), "{name} {case}: {err}");
            assert!(md5 == raw, "{name} {case}: remuxed pictures differ ({} vs {})", md5.len(), raw.len());
        }
    }
}

/// A B-frame with temporal direct prediction takes motion from the first picture of
/// list 1 and needs that picture's references; moved elsewhere it switches to spatial
/// direct prediction, while B-frames left in place keep temporal.
#[test]
fn moved_bframes_leave_temporal_direct() {
    if !ffmpeg_available() {
        return;
    }
    let (name, data) = bframe_configs().swap_remove(4);
    assert_eq!(name, "b_temporal_cavlc");
    let src = Source::new(Stream::parse(&data).unwrap());
    let n = src.len();
    // x264 codes some B-frames with spatial direct prediction anyway
    let temporal = |au: &h264mosh::stream::AccessUnit| {
        au.slices().any(|s| s.header.slice_type() == SliceType::B && !s.header.direct_spatial_mv_pred_flag)
    };
    let b = (1..n).find(|&p| temporal(&src.aus[src.display[p]])).unwrap();
    let timeline = shown(&src, (0..b).chain(b + 1..b + 12).chain([b]).chain(b + 12..n));
    let (out, rep) = render(&src, &timeline, &RenderOptions::default()).unwrap();
    let au = |k: usize| h264mosh::stream::AccessUnit { nals: out.nals[rep.frames[k].nals.clone()].to_vec() };
    assert!(!temporal(&au(b + 11)), "the moved B-frame");
    let last = (0..n).rev().find(|&p| temporal(&src.aus[src.display[p]])).unwrap();
    assert!(temporal(&au(last)), "a B-frame far from the edit");
    render_checked(&src, &timeline, "temporal_moved");
}

/// The same macroblocks written with CAVLC instead of CABAC: PPS switched, every
/// slice parsed by the CABAC layer and written by the CAVLC one.
fn to_cavlc(data: &[u8]) -> Vec<u8> {
    let mut stream = Stream::parse(data).unwrap();
    let cavlc = |p: &Pps| Arc::new(Pps { entropy_coding_mode_flag: false, ..p.clone() });
    for n in stream.nals.iter_mut() {
        match &mut n.kind {
            NalKind::Pps(p) => *p = cavlc(p),
            NalKind::Slice(s) => {
                let data = h264mosh::mb::parse_slice_data(s).unwrap();
                s.pps = cavlc(&s.pps);
                s.header.cabac_init_idc = 0;
                let (bytes, bits) = h264mosh::mb::write_slice_data(s, &data).unwrap();
                s.set_slice_data(bytes, bits).unwrap();
            }
            _ => continue,
        }
        n.dirty = true;
    }
    stream.write(false).unwrap()
}

/// CABAC streams written again with CAVLC from the parsed macroblocks decode to the
/// same pictures: an independent check of the CABAC parser. Edited CABAC streams
/// decode like their macroblocks written with CAVLC, which checks the CABAC writer, and
/// effects on a CABAC stream and on its CAVLC twin give the same pictures.
///
/// The exception are 8x8 blocks an effect empties: CABAC cannot code them, so the
/// writer codes the macroblock differently (same coefficients, QP and motion), while
/// CAVLC keeps them, and ffmpeg's CAVLC decoder then deblocks those blocks as if they
/// had coefficients (bS 2, against 8.7.2.1). For Coeffs only the first two checks hold.
#[test]
fn cabac_macroblocks_match_cavlc() {
    if !ffmpeg_available() {
        return;
    }
    let x = |source: &str, pix: &str, params: &str| encode(source, 2, &["-pix_fmt", pix, "-g", "25", "-x264-params", params]);
    let streams = [
        ("default", x("mandelbrot", "yuv420p", "scenecut=0")),
        ("pyramid", x("testsrc2", "yuv420p", "bframes=4:b-pyramid=normal:ref=3:weightb=1:partitions=all")),
        ("temporal", x("mandelbrot", "yuv420p", "bframes=3:direct=temporal:8x8dct=0")),
        ("i444", x("testsrc2", "yuv444p", "bframes=2")),
        ("i422 10bit", x("mandelbrot", "yuv422p10le", "bframes=2")),
        ("gray", x("mandelbrot", "gray", "bframes=0")),
        ("intra", x("testsrc2", "yuv420p", "keyint=1:cqm=jvt")),
        ("lossless", x("testsrc2", "yuv420p", "qp=0:bframes=1")),
        ("slices", x("mandelbrot", "yuv420p", "slices=3:constrained-intra=1")),
    ];
    let effects = [
        vec![MbEffect::Coeffs(0.4, 0.0)],
        vec![MbEffect::Coeffs(0.0, 0.0)],
        vec![MbEffect::QpAdd(9), MbEffect::IntraMode(2)],
        vec![MbEffect::MvScale(2.0, 0.5), MbEffect::KillIntra],
        vec![MbEffect::ZeroResidual],
    ];
    for (name, data) in &streams {
        let (orig, _) = decode(data, "cabac_orig.264");
        let twin = to_cavlc(data);
        let (md5, err) = decode(&twin, "cabac_twin.264");
        assert!(err.is_empty(), "{name}: {err}");
        assert_eq!(md5, orig, "{name}: the CAVLC twin decodes differently");
        assert_bit_exact(&twin);
        for fx in &effects {
            let a = with_effects(data, fx);
            let (md5_a, err) = decode(&a, "cabac_fx.264");
            assert!(err.is_empty(), "{name} {fx:?}: {err}");
            assert_bit_exact(&a);
            let (md5, _) = decode(&to_cavlc(&a), "cabac_fx_twin.264");
            assert_eq!(md5_a, md5, "{name} {fx:?}: the edited CABAC stream and its CAVLC twin differ");
            if !matches!(fx[0], MbEffect::Coeffs(..)) {
                let (md5, _) = decode(&with_effects(&twin, fx), "cavlc_fx.264");
                assert_eq!(md5_a, md5, "{name} {fx:?}: the effect on CABAC and on CAVLC differs");
            }
        }
    }
}

/// Macroblock effects that work on B slices (all but the motion ones) on B-frame
/// streams, CABAC and CAVLC.
#[test]
fn bframe_streams_take_macroblock_effects() {
    if !ffmpeg_available() {
        return;
    }
    let effects = [
        vec![MbEffect::IntraMode(4)],
        vec![MbEffect::QpAdd(12)],
        vec![MbEffect::Coeffs(0.3, 0.0), MbEffect::DcShift(20, -20, 20)],
        vec![MbEffect::ZeroResidual],
        vec![MbEffect::MvScale(1.5, 1.5), MbEffect::Transplant(10, 0.5)],
    ];
    for (name, data) in bframe_configs() {
        let (orig, _) = decode(&data, "b_fx_orig.264");
        for fx in &effects {
            let out = with_effects(&data, fx);
            let (md5, err) = decode(&out, "b_fx.264");
            assert!(err.is_empty(), "{name} {fx:?}: {err}");
            assert_eq!(md5.len(), orig.len(), "{name} {fx:?}");
            assert_ne!(md5, orig, "{name} {fx:?} had no visible effect");
            assert_bit_exact(&out);
        }
    }
}
