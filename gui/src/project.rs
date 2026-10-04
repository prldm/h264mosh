//! An opened clip set and its renders. The functions here do the heavy work and run
//! on job threads.

use crate::edit::{CardId, Cards, RenderSnapshot};
use crate::jobs::{Reporter, Stage};
use anyhow::{Context, Result, bail};
use h264mosh::mosh::{self, RenderOptions, RenderReport, Source, Timeline};
use h264mosh::pipeline::{self, Cancelled, EncodeSettings, PrepProgress, Tools};
use h264mosh::stream::{NalKind, Stream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Thumbnails are this tall (width from the aspect ratio).
pub const THUMB_HEIGHT: u32 = 72;
/// Previews fit in this box.
pub const PREVIEW_BOX: (u32, u32) = (640, 360);

#[derive(Clone, Copy, Debug)]
pub struct FrameInfo {
    /// 'I', 'P', 'B' or '?'
    pub kind: char,
    pub idr: bool,
}

pub struct Project {
    /// shown in the title bar
    pub name: String,
    /// the (prepared) raw stream the edit starts from
    pub data: Arc<Vec<u8>>,
    pub source: Arc<Source>,
    pub frames: Vec<FrameInfo>,
    /// picture size
    pub size: (u32, u32),
    pub fps: String,
    pub cards: Cards,
    /// base seed of randomized effects
    pub seed: u64,
}

impl Project {
    pub fn thumb_size(&self) -> (u32, u32) {
        fit(self.size, (u32::MAX, THUMB_HEIGHT))
    }

    pub fn preview_size(&self) -> (u32, u32) {
        fit(self.size, PREVIEW_BOX)
    }
}

/// The largest even size with the aspect ratio of `size` inside `bounds`.
fn fit((w, h): (u32, u32), (bw, bh): (u32, u32)) -> (u32, u32) {
    let scale = (bw as f64 / w as f64).min(bh as f64 / h as f64).min(1.0);
    let even = |v: f64| ((v / 2.0).round() as u32 * 2).max(2);
    (even(w as f64 * scale), even(h as f64 * scale))
}

/// A rendered timeline.
pub struct Rendered {
    pub stream: Stream,
    pub report: RenderReport,
    /// card of each output frame
    pub cards: Vec<CardId>,
    pub bytes: Vec<u8>,
}

impl Rendered {
    pub fn len(&self) -> usize {
        self.report.frames.len()
    }

    /// First output frame of `card`.
    pub fn frame_of(&self, card: CardId) -> Option<usize> {
        self.cards.iter().position(|&c| c == card)
    }
}

pub struct Imported {
    pub project: Project,
    pub rendered: Rendered,
}

/// Where imported clips are encoded to: the user's cache folder (not roaming).
pub fn cache_dir() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::home_dir().map(|h| h.join("Library").join("Caches"))
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::home_dir().map(|h| h.join(".cache")))
    };
    base.unwrap_or_else(std::env::temp_dir).join("h264mosh").join("imports")
}

/// Encoder settings for imported clips: the `prep` defaults, with CAVLC so that
/// macroblock effects work.
fn import_settings() -> EncodeSettings {
    EncodeSettings { preset: "medium".into(), crf: 18.0, keyint: "250".into(), scenecut: true, cavlc: true, x264_params: None }
}

/// Open clips: a single raw H.264 stream is used as it is; anything else is encoded
/// with mosh-friendly settings into `cache_dir` first.
pub fn import(tools: &Tools, inputs: &[PathBuf], cache_dir: &Path, r: &Reporter) -> Result<Imported> {
    let name = inputs.first().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let data = if let [single] = inputs
        && pipeline::is_raw_h264(single)
    {
        r.set(Stage::Reading, 0, None);
        std::fs::read(single).with_context(|| format!("reading {}", single.display()))?
    } else {
        std::fs::create_dir_all(cache_dir).with_context(|| format!("creating {}", cache_dir.display()))?;
        let stem = inputs[0].file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let output = cache_dir.join(format!("{stem}-{stamp}.264"));
        let (mut clip, mut clip_frames) = (0, None);
        pipeline::prep(tools, inputs, &output, None, None, &import_settings(), &mut |p| {
            match p {
                PrepProgress::Start { .. } => {}
                PrepProgress::Clip { index, frames, .. } => (clip, clip_frames) = (index, frames),
                PrepProgress::Frames(n) => {
                    return r.set(Stage::Encoding { clip, clips: inputs.len() }, n, clip_frames);
                }
            }
            r.set(Stage::Encoding { clip, clips: inputs.len() }, 0, clip_frames)
        })?;
        r.set(Stage::Reading, 0, None);
        std::fs::read(&output).with_context(|| format!("reading {}", output.display()))?
    };
    if r.cancelled() {
        return Err(Cancelled.into());
    }
    let stream = Stream::parse(&data)?;
    let sps = stream.nals.iter().find_map(|n| match &n.kind {
        NalKind::Sps(s) => Some(s.clone()),
        _ => None,
    });
    let Some(sps) = sps else { bail!("{name}: no H.264 sequence parameter set found") };
    let source = Source::new(stream);
    if source.is_empty() {
        bail!("{name}: no frames found");
    }
    let frames = source.aus.iter().map(|au| FrameInfo { kind: au.picture_type(), idr: au.is_idr() }).collect();
    let fps = pipeline::stream_fps(&data);
    let cards = Cards::new(source.len());
    let project = Project { name, data: Arc::new(data), source: Arc::new(source), frames, size: sps.size(), fps, cards, seed: 1 };
    let (timeline, _) = project.cards.timeline(project.seed);
    r.set(Stage::Rendering, 0, None);
    let rendered = render(&project.source, &timeline, project.cards.output().into_iter().map(|(c, _)| c).collect())?;
    Ok(Imported { project, rendered })
}

pub fn render(source: &Source, timeline: &Timeline, cards: Vec<CardId>) -> Result<Rendered> {
    let (stream, report) = mosh::render(source, timeline, &RenderOptions::default())?;
    let bytes = stream.write(false)?;
    Ok(Rendered { stream, report, cards, bytes })
}

/// A render job's input, taken on the UI thread.
pub struct RenderRequest {
    pub source: Arc<Source>,
    pub timeline: Timeline,
    pub cards: Vec<CardId>,
    pub snapshot: RenderSnapshot,
}

impl RenderRequest {
    pub fn new(project: &Project) -> Self {
        let (timeline, snapshot) = project.cards.timeline(project.seed);
        let cards = project.cards.output().into_iter().map(|(c, _)| c).collect();
        Self { source: project.source.clone(), timeline, cards, snapshot }
    }
}

/// Write the rendered stream: raw H.264 as it is, anything else encoded by ffmpeg.
pub fn export(tools: &Tools, rendered: &Rendered, fps: &str, path: &Path, r: &Reporter) -> Result<()> {
    let total = rendered.len() as u64;
    r.set(Stage::Exporting, 0, Some(total));
    if pipeline::is_raw_h264(path) {
        return std::fs::write(path, &rendered.bytes).with_context(|| format!("writing {}", path.display()));
    }
    // the command line `render` defaults
    pipeline::render(tools, &rendered.bytes, path, fps, "slow", 16.0, &mut |n| r.set(Stage::Exporting, n, Some(total)))
}

/// Decode the source at thumbnail size, sending (frame, RGBA pixels) until `send`
/// returns false. Errors end the feed: thumbnails are optional.
pub fn thumbnails(tools: &Tools, data: &[u8], size: (u32, u32), send: &mut dyn FnMut((usize, Vec<u8>)) -> bool) {
    let _ = pipeline::decode_rgba(tools, data, size, |i, px| send((i, px)));
}

#[cfg(test)]
mod tests {
    use super::fit;

    #[test]
    fn fit_keeps_aspect_and_even_sizes() {
        assert_eq!(fit((1920, 1080), (640, 360)), (640, 360));
        assert_eq!(fit((1280, 720), (u32::MAX, 72)), (128, 72));
        assert_eq!(fit((1080, 1920), (640, 360)), (202, 360));
        assert_eq!(fit((320, 240), (640, 360)), (320, 240));
    }
}
