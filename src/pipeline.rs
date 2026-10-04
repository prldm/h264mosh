//! External tool: ffmpeg (with libx264) for decoding, scaling and encoding.
//!
//! prep:   input clip --ffmpeg (scale, fps, libx264 with mosh-friendly settings)--> raw .264, concatenated
//! render: moshed .264 --ffmpeg (decode, libx264)--> .mp4

use crate::sps::Sps;
use crate::stream::{NalKind, Stream, group_access_units};
use anyhow::{Context, Result, bail};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const RAW_EXTENSIONS: &[&str] = &["264", "h264", "avc", "26l", "jsv", "bin"];

pub fn is_raw_h264(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| RAW_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Locations of ffmpeg and ffprobe.
#[derive(Clone, Debug)]
pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

impl Tools {
    /// ffmpeg: `explicit`, else $H264MOSH_FFMPEG, else `ffmpeg` on PATH. ffprobe is
    /// taken from the same directory when ffmpeg is given as a path.
    pub fn find(explicit: Option<&Path>) -> Self {
        let ffmpeg = explicit.map(Path::to_path_buf).or_else(|| std::env::var_os("H264MOSH_FFMPEG").map(PathBuf::from));
        let Some(ffmpeg) = ffmpeg else { return Self { ffmpeg: "ffmpeg".into(), ffprobe: "ffprobe".into() } };
        let ext = ffmpeg.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
        let sibling = ffmpeg.with_file_name(format!("ffprobe{ext}"));
        let ffprobe = if sibling.is_file() { sibling } else { "ffprobe".into() };
        Self { ffmpeg, ffprobe }
    }

    fn ffmpeg(&self) -> Command {
        let mut c = Command::new(&self.ffmpeg);
        c.args(["-hide_banner", "-nostdin", "-v", "error"]);
        c
    }

    fn run(&self, mut cmd: Command) -> Result<Output> {
        let program = cmd.get_program().to_string_lossy().into_owned();
        let out = cmd.output().with_context(|| format!("could not run {program} (install ffmpeg or pass --ffmpeg PATH)"))?;
        if !out.status.success() {
            bail!("{program} failed:\n{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(out)
    }

    fn check_libx264(&self) -> Result<()> {
        let mut cmd = self.ffmpeg();
        cmd.arg("-encoders");
        let out = self.run(cmd)?;
        if !String::from_utf8_lossy(&out.stdout).contains("libx264") {
            bail!("{} was built without libx264", self.ffmpeg.display());
        }
        Ok(())
    }
}

/// Read a raw Annex B stream, or extract the H.264 track of a container with ffmpeg.
pub fn load_annexb(tools: &Tools, path: &Path) -> Result<Vec<u8>> {
    if is_raw_h264(path) {
        return std::fs::read(path).with_context(|| format!("reading {}", path.display()));
    }
    let mut cmd = tools.ffmpeg();
    cmd.arg("-i").arg(path).args(["-map", "0:v:0", "-c:v", "copy", "-bsf:v", "h264_mp4toannexb", "-f", "h264", "-"]);
    let out = tools.run(cmd).with_context(|| format!("extracting the H.264 stream from {}", path.display()))?;
    if out.stdout.is_empty() {
        bail!("{} contains no H.264 video", path.display());
    }
    Ok(out.stdout)
}

#[derive(Clone, Debug)]
pub struct EncodeSettings {
    pub preset: String,
    pub crf: f32,
    /// maximum keyframe interval ("infinite" allowed)
    pub keyint: String,
    pub scenecut: bool,
    pub cavlc: bool,
    /// extra libx264 options, x264-params syntax (key=value:key=value)
    pub x264_params: Option<String>,
}

impl EncodeSettings {
    fn x264_params(&self) -> String {
        // mosh-friendly: no reordering, one reference, no weighted prediction
        let mut p = vec!["bframes=0".to_string(), "ref=1".into(), "weightp=0".into(), format!("keyint={}", self.keyint)];
        if !self.scenecut {
            p.push("scenecut=0".into());
        }
        if self.cavlc {
            p.push("cabac=0".into());
        }
        p.extend(self.x264_params.iter().filter(|s| !s.is_empty()).cloned());
        p.join(":")
    }
}

pub struct ProbeInfo {
    pub width: u32,
    pub height: u32,
    pub fps: String,
}

pub fn probe(tools: &Tools, path: &Path) -> Result<ProbeInfo> {
    let mut cmd = Command::new(&tools.ffprobe);
    cmd.args(["-v", "error", "-select_streams", "v:0", "-show_entries"])
        .arg("stream=width,height,r_frame_rate,avg_frame_rate:stream_side_data=rotation")
        .args(["-of", "default=nw=1"])
        .arg(path);
    let out = tools.run(cmd)?;
    let text = String::from_utf8_lossy(&out.stdout);
    let get = |k: &str| text.lines().find_map(|l| l.strip_prefix(&format!("{k}="))).map(str::trim);
    let mut width: u32 = get("width").and_then(|v| v.parse().ok()).with_context(|| format!("{}: no video stream", path.display()))?;
    let mut height: u32 = get("height").and_then(|v| v.parse().ok()).with_context(|| format!("{}: no video stream", path.display()))?;
    if get("rotation").and_then(|r| r.parse::<i32>().ok()).is_some_and(|r| r.abs() == 90 || r.abs() == 270) {
        std::mem::swap(&mut width, &mut height);
    }
    let rate = |k: &str| {
        get(k).and_then(|v| {
            let (n, d) = v.split_once('/')?;
            let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
            (n > 0.0 && d > 0.0).then(|| (v.to_string(), n / d))
        })
    };
    // phone footage often reports a bogus r_frame_rate (e.g. 90000/1); fall back to the average
    let fps = match (rate("r_frame_rate"), rate("avg_frame_rate")) {
        (Some((r, f)), _) if f <= 120.0 => r,
        (_, Some((a, _))) => a,
        (Some((r, _)), None) => r,
        _ => "25".into(),
    };
    Ok(ProbeInfo { width: width & !1, height: height & !1, fps })
}

pub struct ClipInfo {
    pub input: PathBuf,
    pub first_frame: usize,
    pub frames: usize,
    pub keyframes: Vec<usize>,
}

/// Encode every input with identical settings and concatenate them into one stream.
pub fn prep(
    tools: &Tools,
    inputs: &[PathBuf],
    output: &Path,
    size: Option<(u32, u32)>,
    fps: Option<String>,
    enc: &EncodeSettings,
) -> Result<Vec<ClipInfo>> {
    tools.check_libx264()?;
    let first = probe(tools, &inputs[0])?;
    let (w, h) = size.unwrap_or((first.width, first.height));
    let fps = fps.unwrap_or(first.fps);
    eprintln!("encoding {} clip(s) at {w}x{h}, {fps} fps", inputs.len());
    let vf = format!(
        "scale={w}:{h}:force_original_aspect_ratio=decrease:flags=lanczos,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1,fps={fps},format=yuv420p"
    );
    let crf = enc.crf.to_string();
    let params = enc.x264_params();
    let mut joined = Vec::new();
    let mut clips = Vec::new();
    let mut ref_sps: Option<Sps> = None;
    let mut frame_base = 0;
    for (i, input) in inputs.iter().enumerate() {
        eprintln!("  [{}/{}] {}", i + 1, inputs.len(), input.display());
        let mut cmd = tools.ffmpeg();
        cmd.arg("-i").arg(input).args(["-map", "0:v:0", "-vf", &vf, "-c:v", "libx264", "-preset", &enc.preset, "-crf", &crf]);
        cmd.args(["-x264-params", &params, "-f", "h264", "-"]);
        let data = tools.run(cmd).with_context(|| format!("encoding {}", input.display()))?.stdout;
        let stream = Stream::parse(&data)?;
        let sps = stream.nals.iter().find_map(|n| match &n.kind {
            NalKind::Sps(s) => Some((**s).clone()),
            _ => None,
        });
        let sps = sps.context("encoder output has no SPS")?;
        match &ref_sps {
            None => ref_sps = Some(sps),
            Some(r) if *r != sps => {
                bail!("clip {} produced a different SPS than the first clip ({})", input.display(), r.differences(&sps))
            }
            _ => {}
        }
        let aus = group_access_units(stream.nals);
        let keyframes = aus.iter().enumerate().filter(|(_, a)| a.is_keyframe()).map(|(k, _)| frame_base + k).collect();
        clips.push(ClipInfo { input: input.clone(), first_frame: frame_base, frames: aus.len(), keyframes });
        frame_base += aus.len();
        joined.extend_from_slice(&data);
    }
    std::fs::write(output, &joined).with_context(|| format!("writing {}", output.display()))?;
    Ok(clips)
}

/// Frame rate of a raw stream from its SPS timing info, else 25.
pub fn stream_fps(data: &[u8]) -> String {
    let fps = Stream::parse(data).ok().and_then(|s| {
        s.nals.iter().find_map(|n| match &n.kind {
            NalKind::Sps(sps) => sps.frame_rate(),
            _ => None,
        })
    });
    match fps {
        Some((n, 1)) => n.to_string(),
        Some((n, d)) => format!("{n}/{d}"),
        None => "25".into(),
    }
}

/// Decode a (moshed) raw stream and encode the result as a normal video file.
pub fn render(tools: &Tools, input: &Path, output: &Path, fps: Option<String>, preset: &str, crf: f32) -> Result<()> {
    tools.check_libx264()?;
    let data = std::fs::read(input).with_context(|| format!("reading {}", input.display()))?;
    let fps = fps.unwrap_or_else(|| stream_fps(&data));
    eprintln!("rendering {} -> {} ({fps} fps)", input.display(), output.display());
    let mut cmd = tools.ffmpeg();
    cmd.args(["-y", "-f", "h264", "-framerate", &fps, "-i"]).arg(input);
    cmd.args(["-c:v", "libx264", "-preset", preset, "-crf", &crf.to_string(), "-pix_fmt", "yuv420p"]);
    if output.extension().is_some_and(|e| e == OsStr::new("mp4") || e == OsStr::new("mov")) {
        cmd.args(["-movflags", "+faststart"]);
    }
    cmd.arg(output);
    tools.run(cmd)?;
    Ok(())
}
