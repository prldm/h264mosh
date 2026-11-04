//! An opened clip set, its project file and its renders. The functions here do the
//! heavy work and run on job threads.

use crate::edit::{CardId, Cards, EditState, RenderSnapshot};
use crate::export::{ExportFormat, ExportSettings};
use crate::history::History;
use crate::jobs::{Reporter, Stage};
use anyhow::{Context, Result, bail};
use h264mosh::mb::MbContext;
use h264mosh::mosh::{self, RenderOptions, RenderReport, Source, Timeline};
use h264mosh::pipeline::{self, BPyramid, Cancelled, EncodeSettings, PrepProgress, Tools};
use h264mosh::stream::{NalKind, Stream};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Thumbnails are this tall (width from the aspect ratio).
pub const THUMB_HEIGHT: u32 = 72;
/// Previews fit in this box.
pub const PREVIEW_BOX: (u32, u32) = (640, 360);
/// File name extension of project files (JSON inside).
pub const PROJECT_EXTENSION: &str = "h264mosh";
const FORMAT: &str = "h264mosh project";
const VERSION: u32 = 1;

#[derive(Clone, Copy, Debug)]
pub struct FrameInfo {
    /// 'I', 'P', 'B' or '?'
    pub kind: char,
    pub idr: bool,
}

/// How clips were encoded into the stream the edit is based on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Encoding {
    pub preset: String,
    pub crf: f32,
    pub keyint: String,
    pub scenecut: bool,
    pub cavlc: bool,
    /// reference frames
    #[serde(default = "one")]
    pub refs: u32,
    #[serde(default)]
    pub bframes: u32,
    #[serde(default)]
    pub b_pyramid: BPyramid,
}

fn one() -> u32 {
    1
}

impl Encoding {
    /// The `prep` defaults (CAVLC), and the reference and B-frames chosen for new clips.
    pub fn for_import(refs: u32, bframes: u32, b_pyramid: BPyramid) -> Self {
        let (preset, keyint) = ("medium".into(), "250".into());
        Self { preset, crf: 18.0, keyint, scenecut: true, cavlc: true, refs: refs.max(1), bframes, b_pyramid }
    }

    fn settings(&self) -> EncodeSettings {
        EncodeSettings {
            preset: self.preset.clone(),
            crf: self.crf,
            keyint: self.keyint.clone(),
            scenecut: self.scenecut,
            cavlc: self.cavlc,
            refs: self.refs,
            bframes: self.bframes,
            b_pyramid: self.b_pyramid,
            x264_params: None,
        }
    }
}

pub struct Project {
    /// shown in the title bar
    pub name: String,
    /// where it was saved
    pub file: Option<PathBuf>,
    /// the clips it was made from
    pub inputs: Vec<PathBuf>,
    /// the raw stream the edit is based on, its hash, and how it was made
    pub stream_path: PathBuf,
    pub stream_hash: u64,
    pub encoding: Option<Encoding>,
    pub data: Arc<Vec<u8>>,
    pub source: Arc<Source>,
    pub frames: Vec<FrameInfo>,
    /// picture size
    pub size: (u32, u32),
    pub fps: String,
    /// the macroblock layer can be edited (macroblock effects need it): not with MBAFF,
    /// slice groups or SP/SI slices
    pub macroblocks: bool,
    /// reference frames of the stream (max_num_ref_frames)
    pub refs: u32,
    pub cards: Cards,
    pub history: History,
    /// the edit state last saved, if any
    pub saved: Option<EditState>,
    /// every source frame once, unchanged (for showing the original)
    pub original: Arc<Rendered>,
}

impl Project {
    pub fn thumb_size(&self) -> (u32, u32) {
        fit(self.size, (u32::MAX, THUMB_HEIGHT))
    }

    pub fn preview_size(&self) -> (u32, u32) {
        fit(self.size, PREVIEW_BOX)
    }

    /// Edited since it was last saved (or never saved).
    pub fn is_dirty(&self) -> bool {
        self.saved.as_ref().is_none_or(|s| !self.cards.is_state(s))
    }

    /// Write the project file.
    pub fn save(&mut self, path: &Path) -> Result<()> {
        let file = ProjectFile {
            format: FORMAT.into(),
            version: VERSION,
            inputs: self.inputs.clone(),
            stream: self.stream_path.clone(),
            stream_hash: format!("{:016x}", self.stream_hash),
            encoding: self.encoding.clone(),
            frames: self.source.len(),
            edit: self.cards.state(),
        };
        let json = serde_json::to_string_pretty(&file)?;
        // write next to it first, so a failed write keeps the old file
        let tmp = path.with_extension(format!("{PROJECT_EXTENSION}.tmp"));
        std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
        self.saved = Some(file.edit);
        self.file = Some(path.to_path_buf());
        self.name = name_of(path);
        Ok(())
    }
}

fn name_of(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// The largest even size with the aspect ratio of `size` inside `bounds`.
fn fit((w, h): (u32, u32), (bw, bh): (u32, u32)) -> (u32, u32) {
    let scale = (bw as f64 / w as f64).min(bh as f64 / h as f64).min(1.0);
    let even = |v: f64| ((v / 2.0).round() as u32 * 2).max(2);
    (even(w as f64 * scale), even(h as f64 * scale))
}

/// FNV-1a, to notice when a stream file was replaced.
fn hash(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3))
}

#[derive(Serialize, Deserialize)]
struct ProjectFile {
    format: String,
    version: u32,
    inputs: Vec<PathBuf>,
    /// the raw stream the edit is based on
    stream: PathBuf,
    stream_hash: String,
    /// how the inputs were encoded into the stream; none if the stream is the input
    encoding: Option<Encoding>,
    frames: usize,
    edit: EditState,
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

/// An opened project: its last render, and the render's snapshot to commit.
pub struct Imported {
    pub project: Project,
    pub rendered: Arc<Rendered>,
    pub snapshot: Option<RenderSnapshot>,
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

pub fn is_project_file(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case(PROJECT_EXTENSION))
}

/// Encode `inputs` into a new stream file in `cache_dir`.
fn prepare(tools: &Tools, inputs: &[PathBuf], cache_dir: &Path, encoding: &Encoding, r: &Reporter) -> Result<(PathBuf, Vec<u8>)> {
    std::fs::create_dir_all(cache_dir).with_context(|| format!("creating {}", cache_dir.display()))?;
    let stem = inputs[0].file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let output = cache_dir.join(format!("{stem}-{stamp}.264"));
    let (mut clip, mut clip_frames) = (0, None);
    pipeline::prep(tools, inputs, &output, None, None, &encoding.settings(), &mut |p| {
        match p {
            PrepProgress::Start { .. } => {}
            PrepProgress::Clip { index, frames, .. } => (clip, clip_frames) = (index, frames),
            PrepProgress::Frames(n) => return r.set(Stage::Encoding { clip, clips: inputs.len() }, n, clip_frames),
        }
        r.set(Stage::Encoding { clip, clips: inputs.len() }, 0, clip_frames)
    })?;
    r.set(Stage::Reading, 0, None);
    let data = std::fs::read(&output).with_context(|| format!("reading {}", output.display()))?;
    Ok((output, data))
}

/// Parse a stream into a project with an unedited timeline.
fn load(
    name: String,
    inputs: Vec<PathBuf>,
    stream_path: PathBuf,
    encoding: Option<Encoding>,
    data: Vec<u8>,
    r: &Reporter,
) -> Result<Project> {
    if r.cancelled() {
        return Err(Cancelled.into());
    }
    r.set(Stage::Reading, 0, None);
    let stream = Stream::parse(&data)?;
    let sps = stream.nals.iter().find_map(|n| match &n.kind {
        NalKind::Sps(s) => Some(s.clone()),
        _ => None,
    });
    let Some(sps) = sps else { bail!("{name}: no H.264 sequence parameter set found") };
    let macroblocks = stream.nals.iter().filter_map(|n| n.slice()).all(|s| MbContext::new(s).is_ok());
    let source = Source::new(stream);
    if source.is_empty() {
        bail!("{name}: no frames found");
    }
    let frames = source.aus.iter().map(|au| FrameInfo { kind: au.picture_type(), idr: au.is_idr() }).collect();
    let fps = pipeline::stream_fps(&data);
    let cards = Cards::new(source.display.iter().copied());
    r.set(Stage::Rendering, 0, None);
    let (timeline, _) = cards.timeline();
    let original = Arc::new(render(&source, &timeline, cards.output().into_iter().map(|(c, _)| c).collect())?);
    Ok(Project {
        name,
        file: None,
        inputs,
        stream_path,
        stream_hash: hash(&data),
        encoding,
        data: Arc::new(data),
        source: Arc::new(source),
        frames,
        size: sps.size(),
        fps,
        macroblocks,
        refs: sps.max_num_ref_frames,
        cards,
        history: History::default(),
        saved: None,
        original,
    })
}

/// Open clips: a single raw H.264 stream is used as it is; anything else is encoded
/// with mosh-friendly settings (`encoding`) into `cache_dir` first.
pub fn import(tools: &Tools, inputs: &[PathBuf], cache_dir: &Path, encoding: Encoding, r: &Reporter) -> Result<Imported> {
    let name = inputs.first().map(|p| name_of(p)).unwrap_or_default();
    let project = if let [single] = inputs
        && pipeline::is_raw_h264(single)
    {
        r.set(Stage::Reading, 0, None);
        let data = std::fs::read(single).with_context(|| format!("reading {}", single.display()))?;
        load(name, inputs.to_vec(), single.clone(), None, data, r)?
    } else {
        let (stream, data) = prepare(tools, inputs, cache_dir, &encoding, r)?;
        load(name, inputs.to_vec(), stream, Some(encoding), data, r)?
    };
    let rendered = project.original.clone();
    Ok(Imported { project, rendered, snapshot: None })
}

/// Open a project file. If its stream is gone, the clips are encoded again.
pub fn open(tools: &Tools, path: &Path, cache_dir: &Path, r: &Reporter) -> Result<Imported> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let file: ProjectFile = serde_json::from_str(&text).with_context(|| format!("{} is not a h264mosh project", path.display()))?;
    if file.format != FORMAT || file.version > VERSION {
        bail!("{} is not a h264mosh project this version can read", path.display());
    }
    r.set(Stage::Reading, 0, None);
    let stored = std::fs::read(&file.stream).ok().filter(|d| format!("{:016x}", hash(d)) == file.stream_hash);
    let reencoded = stored.is_none();
    let (stream_path, data) = match (stored, &file.encoding) {
        (Some(data), _) => (file.stream.clone(), data),
        (None, Some(encoding)) => {
            if let Some(missing) = file.inputs.iter().find(|p| !p.is_file()) {
                bail!("the stream {} is gone, and so is the clip {}", file.stream.display(), missing.display());
            }
            prepare(tools, &file.inputs, cache_dir, encoding, r)?
        }
        (None, None) => bail!("the stream {} is missing or was changed", file.stream.display()),
    };
    let mut project = load(name_of(path), file.inputs, stream_path, file.encoding, data, r)?;
    if project.source.len() != file.frames {
        bail!("the clips now give {} frames instead of {}; the edit does not fit them", project.source.len(), file.frames);
    }
    let mut edit = file.edit;
    let effects: HashSet<_> = edit.effects.iter().map(|e| e.id).collect();
    let mut ids = HashSet::new();
    for card in &mut edit.cards {
        if card.src >= file.frames || !ids.insert(card.id) {
            bail!("{}: the edit refers to frames that do not exist", path.display());
        }
        card.effects.retain(|e| effects.contains(e));
    }
    project.cards = Cards::from_state(edit);
    if !reencoded {
        // a re-encoded stream is a change worth saving
        project.saved = Some(project.cards.state());
    }
    project.file = Some(path.to_path_buf());
    let RenderRequest { source, timeline, cards, snapshot } = RenderRequest::new(&project);
    r.set(Stage::Rendering, 0, None);
    let rendered = Arc::new(render(&source, &timeline, cards)?);
    Ok(Imported { project, rendered, snapshot: Some(snapshot) })
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
        let (timeline, snapshot) = project.cards.timeline();
        let cards = project.cards.output().into_iter().map(|(c, _)| c).collect();
        Self { source: project.source.clone(), timeline, cards, snapshot }
    }
}

/// Write the rendered stream as the settings say.
pub fn export(tools: &Tools, rendered: &Rendered, fps: &str, path: &Path, settings: &ExportSettings, r: &Reporter) -> Result<()> {
    let total = rendered.len() as u64;
    let mut progress = |n| r.set(Stage::Exporting, n, Some(total));
    progress(0);
    match settings.format {
        ExportFormat::Raw => std::fs::write(path, &rendered.bytes).with_context(|| format!("writing {}", path.display())),
        ExportFormat::Copy => pipeline::remux(tools, &rendered.bytes, path, fps, &mut progress),
        ExportFormat::Baked => {
            let (preset, crf) = settings.quality.encoder();
            pipeline::render(tools, &rendered.bytes, path, fps, preset, crf, &mut progress)
        }
    }
}

/// Decode the source at thumbnail size, sending (source frame, RGBA pixels) until
/// `send` returns false. Errors end the feed: thumbnails are optional.
pub fn thumbnails(tools: &Tools, source: &Source, data: &[u8], size: (u32, u32), send: &mut dyn FnMut((usize, Vec<u8>)) -> bool) {
    // the decoder gives the pictures in display order
    let _ = pipeline::decode_rgba(tools, data, size, |i, px| source.display.get(i).is_some_and(|&src| send((src, px))));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_aspect_and_even_sizes() {
        assert_eq!(fit((1920, 1080), (640, 360)), (640, 360));
        assert_eq!(fit((1280, 720), (u32::MAX, 72)), (128, 72));
        assert_eq!(fit((1080, 1920), (640, 360)), (202, 360));
        assert_eq!(fit((320, 240), (640, 360)), (320, 240));
    }

    #[test]
    fn fnv_hash_is_stable() {
        // reference values of FNV-1a 64
        assert_eq!(hash(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(hash(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
