//! External tool: ffmpeg (with libx264) for decoding, scaling and encoding.
//!
//! prep:   input clip --ffmpeg (scale, fps, libx264 with mosh-friendly settings)--> raw .264, concatenated
//! render: moshed .264 --ffmpeg (decode, libx264)--> .mp4
//! decode: .264 --ffmpeg (decode, scale)--> RGBA frames (previews)
//!
//! Long jobs report progress through a callback, which can also cancel them.

use crate::sps::Sps;
use crate::stream::{NalKind, Stream, group_access_units};
use anyhow::{Context, Result, bail};
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::mpsc;
use std::thread::Scope;

const RAW_EXTENSIONS: &[&str] = &["264", "h264", "avc", "26l", "jsv", "bin"];

pub fn is_raw_h264(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| RAW_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Returned (inside the `anyhow::Error`) when a progress callback stopped a job.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// A command that opens no console window on Windows (the GUI has none).
fn command(program: &Path) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
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
        let mut c = command(&self.ffmpeg);
        c.args(["-hide_banner", "-nostdin", "-v", "error"]);
        c
    }

    /// ffmpeg reporting its progress on stderr, for `run_with_progress`
    fn ffmpeg_with_progress(&self) -> Command {
        let mut c = self.ffmpeg();
        c.args(["-nostats", "-progress", "pipe:2"]);
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

    fn spawn(&self, mut cmd: Command) -> Result<(Child, String)> {
        let program = cmd.get_program().to_string_lossy().into_owned();
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let child = cmd.spawn().with_context(|| format!("could not run {program} (install ffmpeg or pass --ffmpeg PATH)"))?;
        Ok((child, program))
    }

    /// Run `cmd` (from `ffmpeg_with_progress`) with `input` on stdin and return its
    /// stdout. `progress` gets the number of frames done and returns false to cancel.
    fn run_with_progress(&self, cmd: Command, input: &[u8], progress: &mut dyn FnMut(u64) -> bool) -> Result<Vec<u8>> {
        let (mut child, program) = self.spawn(cmd)?;
        let (stdin, mut stdout, stderr) = (child.stdin.take(), child.stdout.take().unwrap(), child.stderr.take().unwrap());
        let (frames_tx, frames) = mpsc::channel();
        std::thread::scope(|s| {
            feed(s, stdin, input);
            let output = s.spawn(move || {
                let mut v = Vec::new();
                stdout.read_to_end(&mut v).map(|_| v)
            });
            let messages = s.spawn(move || read_stderr(stderr, Some(frames_tx)));
            let mut cancelled = false;
            for n in frames.iter() {
                if !progress(n) {
                    cancelled = true;
                    let _ = child.kill();
                    break;
                }
            }
            drop(frames);
            let status = child.wait()?;
            let output = output.join().expect("stdout reader")?;
            let messages = messages.join().expect("stderr reader");
            if cancelled {
                return Err(Cancelled.into());
            }
            if !status.success() {
                bail!("{program} failed:\n{}", messages.trim());
            }
            Ok(output)
        })
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

/// Write `input` to the child's stdin from a thread, then close it. A write error
/// means the child stopped reading (it failed or was stopped); its exit status tells.
fn feed<'scope>(s: &'scope Scope<'scope, '_>, stdin: Option<ChildStdin>, input: &'scope [u8]) {
    if let Some(mut stdin) = stdin {
        s.spawn(move || {
            let _ = stdin.write_all(input);
        });
    }
}

/// Read ffmpeg's stderr: `-progress` frame counts go to `frames`; the last other
/// lines are returned (error messages).
fn read_stderr(stderr: impl Read, frames: Option<mpsc::Sender<u64>>) -> String {
    let mut messages = VecDeque::new();
    for line in BufReader::new(stderr).split(b'\n') {
        let Ok(line) = line else { break };
        let line = String::from_utf8_lossy(&line).trim_end().to_string();
        // progress lines are key=value without spaces
        if let Some((key, value)) = line.split_once('=')
            && !key.contains(' ')
            && !value.contains(' ')
        {
            if key == "frame"
                && let (Some(tx), Ok(n)) = (&frames, value.parse())
            {
                let _ = tx.send(n);
            }
            continue;
        }
        messages.push_back(line);
        if messages.len() > 40 {
            messages.pop_front();
        }
    }
    Vec::from(messages).join("\n")
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
    /// seconds, if the container knows
    pub duration: Option<f64>,
}

pub fn probe(tools: &Tools, path: &Path) -> Result<ProbeInfo> {
    let mut cmd = command(&tools.ffprobe);
    cmd.args(["-v", "error", "-select_streams", "v:0", "-show_entries"])
        .arg("stream=width,height,r_frame_rate,avg_frame_rate:stream_side_data=rotation:format=duration")
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
    let rate = |k: &str| get(k).and_then(|v| Some((v.to_string(), fps_value(v)?)));
    // phone footage often reports a bogus r_frame_rate (e.g. 90000/1); fall back to the average
    let fps = match (rate("r_frame_rate"), rate("avg_frame_rate")) {
        (Some((r, f)), _) if f <= 120.0 => r,
        (_, Some((a, _))) => a,
        (Some((r, _)), None) => r,
        _ => "25".into(),
    };
    let duration = get("duration").and_then(|d| d.parse().ok()).filter(|d: &f64| *d > 0.0);
    Ok(ProbeInfo { width: width & !1, height: height & !1, fps, duration })
}

/// "30000/1001" or "25" as frames per second
pub fn fps_value(fps: &str) -> Option<f64> {
    let (n, d) = fps.split_once('/').unwrap_or((fps, "1"));
    let (n, d): (f64, f64) = (n.trim().parse().ok()?, d.trim().parse().ok()?);
    (n > 0.0 && d > 0.0).then(|| n / d)
}

#[derive(Clone, Debug)]
pub struct ClipInfo {
    pub input: PathBuf,
    pub first_frame: usize,
    pub frames: usize,
    pub keyframes: Vec<usize>,
}

/// What `prep` is doing.
pub enum PrepProgress<'a> {
    /// every clip is encoded at this size and frame rate
    Start { width: u32, height: u32, fps: &'a str, clips: usize },
    /// encoding clip `index`; `frames` is estimated from its duration
    Clip { index: usize, path: &'a Path, frames: Option<u64> },
    /// frames of the current clip encoded so far
    Frames(u64),
}

/// Encode every input with identical settings and concatenate them into one stream.
/// `progress` returns false to cancel (the error is then [`Cancelled`]).
pub fn prep(
    tools: &Tools,
    inputs: &[PathBuf],
    output: &Path,
    size: Option<(u32, u32)>,
    fps: Option<String>,
    enc: &EncodeSettings,
    progress: &mut dyn FnMut(PrepProgress) -> bool,
) -> Result<Vec<ClipInfo>> {
    tools.check_libx264()?;
    let first = probe(tools, &inputs[0])?;
    let (w, h) = size.unwrap_or((first.width, first.height));
    let fps = fps.unwrap_or(first.fps);
    if !progress(PrepProgress::Start { width: w, height: h, fps: &fps, clips: inputs.len() }) {
        return Err(Cancelled.into());
    }
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
        let duration = if i == 0 { first.duration } else { probe(tools, input)?.duration };
        let frames = duration.zip(fps_value(&fps)).map(|(d, f)| (d * f).round() as u64);
        if !progress(PrepProgress::Clip { index: i, path: input, frames }) {
            return Err(Cancelled.into());
        }
        let mut cmd = tools.ffmpeg_with_progress();
        cmd.arg("-i").arg(input).args(["-map", "0:v:0", "-vf", &vf, "-c:v", "libx264", "-preset", &enc.preset, "-crf", &crf]);
        cmd.args(["-x264-params", &params, "-f", "h264", "-"]);
        let data = tools
            .run_with_progress(cmd, &[], &mut |n| progress(PrepProgress::Frames(n)))
            .with_context(|| format!("encoding {}", input.display()))?;
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
/// `progress` gets the frames done and returns false to cancel ([`Cancelled`]).
pub fn render(
    tools: &Tools,
    data: &[u8],
    output: &Path,
    fps: &str,
    preset: &str,
    crf: f32,
    progress: &mut dyn FnMut(u64) -> bool,
) -> Result<()> {
    tools.check_libx264()?;
    let mut cmd = tools.ffmpeg_with_progress();
    cmd.args(["-y", "-f", "h264", "-framerate", fps, "-i", "pipe:0"]);
    cmd.args(["-c:v", "libx264", "-preset", preset, "-crf", &crf.to_string(), "-pix_fmt", "yuv420p"]);
    if output.extension().is_some_and(|e| e == OsStr::new("mp4") || e == OsStr::new("mov")) {
        cmd.args(["-movflags", "+faststart"]);
    }
    cmd.arg(output);
    tools.run_with_progress(cmd, data, progress)?;
    Ok(())
}

/// Decode a raw stream into RGBA frames of `width`x`height` (bilinear scaling), with
/// the same software decoder as `render`. `on_frame(index, pixels)` gets them in order
/// and returns false to stop early. Returns the number of frames decoded.
pub fn decode_rgba(tools: &Tools, data: &[u8], (width, height): (u32, u32), mut on_frame: impl FnMut(usize, Vec<u8>) -> bool) -> Result<usize> {
    let mut cmd = tools.ffmpeg();
    cmd.args(["-f", "h264", "-i", "pipe:0", "-vf"]).arg(format!("scale={width}:{height}:flags=bilinear"));
    cmd.args(["-pix_fmt", "rgba", "-fps_mode", "passthrough", "-f", "rawvideo", "pipe:1"]);
    let (mut child, program) = tools.spawn(cmd)?;
    let (stdin, mut stdout, stderr) = (child.stdin.take(), child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let frame_size = width as usize * height as usize * 4;
    std::thread::scope(|s| {
        feed(s, stdin, data);
        let messages = s.spawn(move || read_stderr(stderr, None));
        let mut read = || -> Result<(usize, bool)> {
            let mut count = 0;
            loop {
                let mut frame = vec![0; frame_size];
                match read_full(&mut stdout, &mut frame)? {
                    0 => return Ok((count, false)),
                    n if n < frame_size => bail!("{program}: incomplete frame {count}"),
                    _ => {}
                }
                count += 1;
                if !on_frame(count - 1, frame) {
                    return Ok((count, true));
                }
            }
        };
        let result = read();
        // stopped early or failed: ffmpeg may still be writing (and the feeder writing to it)
        if !matches!(result, Ok((_, false))) {
            let _ = child.kill();
        }
        let status = child.wait()?;
        let messages = messages.join().expect("stderr reader");
        let (count, stopped) = result?;
        if !stopped && !status.success() {
            bail!("{program} failed:\n{}", messages.trim());
        }
        Ok(count)
    })
}

/// Fill `buf` unless the reader ends first; returns the bytes read.
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}
