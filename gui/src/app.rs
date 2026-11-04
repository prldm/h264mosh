use crate::edit::{CardId, Cards, EffectId, RenderSnapshot};
use crate::export::{self, ExportSettings};
use crate::history::Gesture;
use crate::jobs::{Feed, Job, Stage};
use crate::panel::{self, Action};
use crate::preview::Preview;
use crate::project::{self, Encoding, Imported, PROJECT_EXTENSION, Project, RenderRequest, Rendered};
use crate::settings::Settings;
use crate::timeline::{Event, Strip, TimelineView};
use crate::tr;
use eframe::egui::{self, Align, Key, Layout, Modifiers, RichText, Sense, Vec2};
use h264mosh::pipeline::{BPyramid, Cancelled, Tools};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// seconds without edits before an automatic render
const AUTO_RENDER_DELAY: f64 = 0.4;
const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mov", "mkv", "avi", "webm", "m4v", "264", "h264"];
/// memory for decoded preview frames: the render, and the original for comparison
const PREVIEW_CACHE: usize = 512 << 20;
const ORIGINAL_CACHE: usize = 128 << 20;
const MAX_SPEED: f64 = 8.0;

enum Running {
    Open(Job<Imported>),
    Render(Job<Rendered>, RenderSnapshot),
    Export(Job<()>, PathBuf),
}

enum Finished {
    Open(anyhow::Result<Box<Imported>>),
    Render(anyhow::Result<Box<Rendered>>),
    Export(anyhow::Result<()>),
}

/// What waits for an answer about unsaved changes.
enum AfterSave {
    Open(Vec<PathBuf>),
    ChooseFile,
    Quit,
}

enum Answer {
    Save,
    Discard,
    Cancel,
}

struct Notice {
    error: bool,
    text: String,
}

/// Playing: the output frame at a moment, and the speed (negative: backwards).
struct Playback {
    speed: f64,
    since: f64,
    from: f64,
}

pub struct App {
    tools: Tools,
    /// where imported clips are encoded to
    cache_dir: PathBuf,
    settings: Settings,
    project: Option<Project>,
    preview: Option<Preview>,
    /// the unedited source, for comparing
    original: Option<Preview>,
    thumbs: Vec<Option<egui::TextureHandle>>,
    thumb_feed: Option<Feed<(usize, Vec<u8>)>>,
    timeline: TimelineView,
    active_effect: Option<EffectId>,
    /// output frame of the last render shown in the preview
    playhead: usize,
    playback: Option<Playback>,
    show_original: bool,
    job: Option<Running>,
    last_edit: f64,
    notice: Option<Notice>,
    /// open while Some
    export_dialog: Option<ExportSettings>,
    unsaved_question: Option<AfterSave>,
    allow_close: bool,
    show_shortcuts: bool,
    /// the empty view's Open button was clicked
    want_open: bool,
    title: String,
}

impl App {
    pub fn new(_cc: &eframe::CreationContext) -> Self {
        Self {
            tools: Tools::find(None),
            cache_dir: project::cache_dir(),
            settings: Settings::load(),
            project: None,
            preview: None,
            original: None,
            thumbs: Vec::new(),
            thumb_feed: None,
            timeline: TimelineView::default(),
            active_effect: None,
            playhead: 0,
            playback: None,
            show_original: false,
            job: None,
            last_edit: 0.0,
            notice: None,
            export_dialog: None,
            unsaved_question: None,
            allow_close: false,
            show_shortcuts: false,
            want_open: false,
            title: String::new(),
        }
    }

    fn error(&mut self, text: String) {
        self.notice = Some(Notice { error: true, text });
    }

    fn info(&mut self, text: String) {
        self.notice = Some(Notice { error: false, text });
    }

    fn busy(&self) -> bool {
        self.job.is_some()
    }

    fn dirty(&self) -> bool {
        self.project.as_ref().is_some_and(Project::is_dirty)
    }

    // ---- opening, saving ----

    /// Open a project file or clips, asking first about unsaved changes.
    fn request_open(&mut self, ctx: &egui::Context, paths: Vec<PathBuf>) {
        if paths.is_empty() || self.busy() {
            return;
        }
        if self.dirty() {
            self.unsaved_question = Some(AfterSave::Open(paths));
        } else {
            self.start_open(ctx, paths);
        }
    }

    fn start_open(&mut self, ctx: &egui::Context, paths: Vec<PathBuf>) {
        let (tools, cache_dir) = (self.tools.clone(), self.cache_dir.clone());
        let job = match paths.as_slice() {
            [file] if project::is_project_file(file) => {
                let file = file.clone();
                Job::spawn(ctx, move |r| project::open(&tools, &file, &cache_dir, r))
            }
            _ => {
                let s = &self.settings;
                let encoding = (!s.import_as_coded).then(|| Encoding::for_import(s.import_refs, s.import_bframes, s.import_b_pyramid));
                Job::spawn(ctx, move |r| project::import(&tools, &paths, &cache_dir, encoding, r))
            }
        };
        self.job = Some(Running::Open(job));
    }

    fn choose_file(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        if self.dirty() {
            self.unsaved_question = Some(AfterSave::ChooseFile);
            return;
        }
        let mut all = VIDEO_EXTENSIONS.to_vec();
        all.push(PROJECT_EXTENSION);
        let picked = rfd::FileDialog::new()
            .set_parent(frame)
            .add_filter(tr!("filter-all"), &all)
            .add_filter(tr!("filter-project"), &[PROJECT_EXTENSION])
            .add_filter(tr!("filter-video"), VIDEO_EXTENSIONS)
            .pick_files();
        if let Some(files) = picked {
            self.start_open(ctx, files);
        }
    }

    /// Save to the project's file, or ask for one. Returns whether it was saved.
    fn save(&mut self, frame: &eframe::Frame) -> bool {
        match self.project.as_ref().and_then(|p| p.file.clone()) {
            Some(file) => self.save_to(&file),
            None => self.save_as(frame),
        }
    }

    fn save_as(&mut self, frame: &eframe::Frame) -> bool {
        let Some(p) = &self.project else { return false };
        let stem = Path::new(&p.name).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let dialog = rfd::FileDialog::new()
            .set_parent(frame)
            .add_filter(tr!("filter-project"), &[PROJECT_EXTENSION])
            .set_file_name(format!("{stem}.{PROJECT_EXTENSION}"));
        match dialog.save_file() {
            Some(file) => self.save_to(&file.with_extension(PROJECT_EXTENSION)),
            None => false,
        }
    }

    fn save_to(&mut self, file: &Path) -> bool {
        let Some(p) = &mut self.project else { return false };
        match p.save(file) {
            Ok(()) => {
                self.settings.add_recent(file);
                true
            }
            Err(e) => {
                self.error(tr!("save-failed", reason = format!("{e:#}")));
                false
            }
        }
    }

    fn answer_unsaved(&mut self, ctx: &egui::Context, frame: &eframe::Frame, save: bool) {
        let Some(after) = self.unsaved_question.take() else { return };
        if save && !self.save(frame) {
            return;
        }
        match after {
            AfterSave::Open(paths) => self.start_open(ctx, paths),
            AfterSave::ChooseFile => {
                // nothing unsaved any more, so this goes straight to the file dialog
                if let Some(p) = &mut self.project {
                    p.saved = Some(p.cards.state());
                }
                self.choose_file(ctx, frame);
            }
            AfterSave::Quit => {
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    // ---- rendering, exporting ----

    fn start_render(&mut self, ctx: &egui::Context) {
        let Some(p) = &self.project else { return };
        if self.busy() {
            return;
        }
        let Some(first) = p.cards.cards.iter().find(|c| !c.deleted) else {
            self.error(tr!("error-nothing-to-render"));
            return;
        };
        if !p.frames[first.src].idr {
            self.error(tr!("error-first-frame-idr", frame = first.src));
            return;
        }
        let RenderRequest { source, timeline, cards, snapshot } = RenderRequest::new(p);
        let job = Job::spawn(ctx, move |r| {
            r.set(Stage::Rendering, 0, None);
            project::render(&source, &timeline, cards)
        });
        self.job = Some(Running::Render(job, snapshot));
    }

    fn start_export(&mut self, ctx: &egui::Context, frame: &eframe::Frame, settings: ExportSettings) {
        self.settings.export = settings;
        self.settings.save();
        let (Some(p), Some(preview)) = (&self.project, &self.preview) else { return };
        let stem = Path::new(&p.name).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let ext = settings.extension();
        let filter = if ext == "264" { tr!("filter-raw") } else { tr!("filter-mp4") };
        let dialog = rfd::FileDialog::new().set_parent(frame).add_filter(filter, &[ext]).set_file_name(format!("{stem}-mosh.{ext}"));
        let Some(path) = dialog.save_file() else { return };
        let (tools, rendered, fps, out) = (self.tools.clone(), preview.rendered().clone(), p.fps.clone(), path.clone());
        let job = Job::spawn(ctx, move |r| project::export(&tools, &rendered, &fps, &out, &settings, r));
        self.job = Some(Running::Export(job, path));
    }

    /// Take the result of a finished job.
    fn poll_job(&mut self, ctx: &egui::Context) {
        let finished = match &self.job {
            Some(Running::Open(j)) => j.poll().map(|r| Finished::Open(r.map(Box::new))),
            Some(Running::Render(j, _)) => j.poll().map(|r| Finished::Render(r.map(Box::new))),
            Some(Running::Export(j, _)) => j.poll().map(Finished::Export),
            None => None,
        };
        let Some(finished) = finished else { return };
        let job = self.job.take();
        let cancelled = |e: &anyhow::Error| e.downcast_ref::<Cancelled>().is_some();
        match (finished, job) {
            (Finished::Open(Ok(imported)), _) => self.apply_open(ctx, *imported),
            (Finished::Render(Ok(rendered)), Some(Running::Render(_, snapshot))) => self.apply_render(*rendered, snapshot),
            (Finished::Export(Ok(())), Some(Running::Export(_, path))) => self.info(tr!("export-done", path = path.display().to_string())),
            (Finished::Open(Err(e)), _) if !cancelled(&e) => self.error(tr!("import-failed", reason = format!("{e:#}"))),
            (Finished::Render(Err(e)), _) if !cancelled(&e) => self.error(tr!("render-failed", reason = format!("{e:#}"))),
            (Finished::Export(Err(e)), _) if !cancelled(&e) => self.error(tr!("export-failed", reason = format!("{e:#}"))),
            _ => {}
        }
    }

    fn apply_open(&mut self, ctx: &egui::Context, imported: Imported) {
        let Imported { mut project, rendered, snapshot } = imported;
        if let Some(snapshot) = snapshot {
            // tombstones in the file are gone after this render
            let was_saved = project.saved.is_some();
            project.cards.commit(snapshot);
            if was_saved {
                project.saved = Some(project.cards.state());
            }
        }
        let (tools, source, data, size) = (self.tools.clone(), project.source.clone(), project.data.clone(), project.thumb_size());
        self.thumb_feed = Some(Feed::spawn(ctx, move |send| project::thumbnails(&tools, &source, &data, size, send)));
        self.thumbs = vec![None; project.source.len()];
        self.preview = Some(Preview::new(self.tools.clone(), rendered, project.preview_size(), PREVIEW_CACHE));
        self.original = Some(Preview::new(self.tools.clone(), project.original.clone(), project.preview_size(), ORIGINAL_CACHE));
        if let Some(file) = &project.file {
            self.settings.add_recent(file);
        }
        self.project = Some(project);
        self.timeline = TimelineView::default();
        self.active_effect = None;
        self.playhead = 0;
        self.playback = None;
        self.notice = None;
    }

    fn apply_render(&mut self, rendered: Rendered, snapshot: RenderSnapshot) {
        let card = self.playhead_card();
        let Some(p) = &mut self.project else { return };
        p.cards.commit(snapshot);
        let report = &rendered.report;
        let warnings: Vec<String> = report.warnings.iter().chain(report.failed_effects.iter().map(|(_, r)| r)).cloned().collect();
        self.playhead = card.and_then(|c| rendered.frame_of(c)).unwrap_or(self.playhead).min(rendered.len().saturating_sub(1));
        self.preview = Some(Preview::new(self.tools.clone(), Arc::new(rendered), p.preview_size(), PREVIEW_CACHE));
        // ids of removed cards may linger in the selection
        let alive: HashSet<CardId> = p.cards.cards.iter().map(|c| c.id).collect();
        self.timeline.selection.retain(|id| alive.contains(id));
        if let Some(w) = warnings.first() {
            self.info(tr!("render-warning", text = w.clone(), count = warnings.len()));
        }
    }

    fn playhead_card(&self) -> Option<CardId> {
        self.preview.as_ref().and_then(|p| p.rendered().cards.get(self.playhead).copied())
    }

    // ---- editing ----

    /// Make an edit, recorded for undo; edits of one `gesture` make one undo step.
    fn edit(&mut self, ctx: &egui::Context, gesture: Option<Gesture>, f: impl FnOnce(&mut Cards)) {
        let Some(p) = &mut self.project else { return };
        let before = p.cards.state();
        f(&mut p.cards);
        if !p.cards.is_state(&before) {
            let now = ctx.input(|i| i.time);
            p.history.record(before, gesture, now);
            self.last_edit = now;
        }
    }

    fn undo(&mut self, ctx: &egui::Context, redo: bool) {
        let Some(p) = &mut self.project else { return };
        let current = p.cards.state();
        let state = if redo { p.history.redo(current) } else { p.history.undo(current) };
        if let Some(s) = state {
            p.cards.restore(s);
            let alive: HashSet<CardId> = p.cards.cards.iter().map(|c| c.id).collect();
            self.timeline.selection.retain(|id| alive.contains(id));
            if self.active_effect.is_some_and(|id| p.cards.effect(id).is_none()) {
                self.active_effect = None;
            }
            self.last_edit = ctx.input(|i| i.time);
        }
    }

    fn selection(&self) -> HashSet<CardId> {
        self.timeline.selection.clone()
    }

    /// Delete the keyframes in the selection; without a selection, every keyframe but
    /// the first frame (like `--drop-keyframes`).
    fn drop_keyframes(&mut self, ctx: &egui::Context) {
        let Some(p) = &self.project else { return };
        let sel = &self.timeline.selection;
        let first = p.cards.cards.iter().find(|c| !c.deleted).map(|c| c.id);
        let ids: HashSet<CardId> = p
            .cards
            .cards
            .iter()
            .filter(|c| !c.deleted && p.frames[c.src].kind == 'I')
            .filter(|c| if sel.is_empty() { Some(c.id) != first } else { sel.contains(&c.id) })
            .map(|c| c.id)
            .collect();
        if ids.is_empty() {
            self.info(tr!("no-keyframes"));
        } else {
            self.edit(ctx, None, |c| c.set_deleted(&ids, true));
        }
    }

    fn timeline_event(&mut self, ctx: &egui::Context, event: Event) {
        match event {
            Event::Seek(card) => {
                if let Some(k) = self.preview.as_ref().and_then(|v| v.rendered().frame_of(card)) {
                    self.playhead = k;
                }
            }
            Event::Move { ids, before } => self.edit(ctx, None, |c| {
                c.move_to(&ids, before);
            }),
            Event::Copies { ids, delta, gesture } => self.edit(ctx, Some(gesture), |c| c.add_copies(&ids, delta)),
            Event::Restore(id) => self.edit(ctx, None, |c| c.set_deleted(&HashSet::from([id]), false)),
            Event::SelectEffect(id) => self.active_effect = id,
            Event::EffectSpan { id, cards, on } => self.edit(ctx, None, |c| c.apply_effect(id, &cards, on)),
        }
    }

    fn panel_action(&mut self, ctx: &egui::Context, action: Action) {
        let sel = self.selection();
        match action {
            Action::Add(effect) => {
                let mut added = None;
                self.edit(ctx, None, |c| added = Some(c.add_effect(effect, &sel)));
                self.active_effect = added;
            }
            Action::Select(id) => self.active_effect = id,
            Action::Params(id, effect) => self.edit(ctx, Some(Gesture::EffectParams(id)), |c| c.set_effect(id, effect)),
            Action::Mask(id, mask) => self.edit(ctx, Some(Gesture::EffectParams(id)), |c| c.set_effect_mask(id, mask)),
            Action::Enabled(id, on) => self.edit(ctx, None, |c| c.set_effect_enabled(id, on)),
            Action::Apply(id, on) => self.edit(ctx, None, |c| c.apply_effect(id, &sel, on)),
            Action::Delete(id) => {
                self.edit(ctx, None, |c| c.delete_effect(id));
                self.active_effect = None;
            }
            Action::Reorder(id, delta) => self.edit(ctx, None, |c| c.move_effect(id, delta)),
            Action::Seed(seed) => self.edit(ctx, Some(Gesture::Seed), |c| c.set_seed(seed)),
        }
    }

    // ---- playback ----

    fn play(&mut self, ctx: &egui::Context, speed: f64) {
        let now = ctx.input(|i| i.time);
        self.playback = Some(Playback { speed, since: now, from: self.playhead as f64 });
    }

    /// L/J: play forwards/backwards, faster on each press.
    fn shuttle(&mut self, ctx: &egui::Context, forward: bool) {
        let sign = if forward { 1.0 } else { -1.0 };
        let speed = match &self.playback {
            Some(p) if p.speed * sign > 0.0 => (p.speed * 2.0).clamp(-MAX_SPEED, MAX_SPEED),
            _ => sign,
        };
        self.play(ctx, speed);
    }

    fn toggle_play(&mut self, ctx: &egui::Context) {
        if self.playback.take().is_none() {
            self.play(ctx, 1.0);
        }
    }

    /// Move the playhead along while playing. Waits when decoding falls behind.
    fn tick(&mut self, ctx: &egui::Context) {
        let (Some(pb), Some(preview), Some(p)) = (&mut self.playback, &self.preview, &self.project) else { return };
        let fps = h264mosh::pipeline::fps_value(&p.fps).unwrap_or(25.0);
        let now = ctx.input(|i| i.time);
        let last = preview.rendered().len().saturating_sub(1) as f64;
        let target = pb.from + (now - pb.since) * fps * pb.speed;
        if target.round() as i64 != self.playhead as i64 {
            if preview.shows(self.playhead) {
                self.playhead = target.clamp(0.0, last).round() as usize;
            } else {
                (pb.from, pb.since) = (self.playhead as f64, now);
            }
        }
        if !(0.0..=last).contains(&target) {
            self.playback = None;
        }
        self.reveal_playhead();
        ctx.request_repaint();
    }

    fn step(&mut self, delta: i64) {
        let Some(preview) = &self.preview else { return };
        let last = preview.rendered().len().saturating_sub(1) as i64;
        self.playhead = (self.playhead as i64).saturating_add(delta).clamp(0, last) as usize;
        self.playback = None;
        self.reveal_playhead();
    }

    fn reveal_playhead(&mut self) {
        if let (Some(card), Some(p)) = (self.playhead_card(), &self.project)
            && let Some(i) = p.cards.index_of(card)
        {
            self.timeline.reveal(i);
        }
    }

    // ---- keyboard ----

    fn keyboard(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        if ctx.egui_wants_keyboard_input() || self.export_dialog.is_some() || self.unsaved_question.is_some() {
            return;
        }
        // single keys act on the press, not on key repeat
        let once = |key: Key, shift: bool| {
            ctx.input(|i| {
                !i.modifiers.command
                    && !i.modifiers.alt
                    && i.modifiers.shift == shift
                    && i.events.iter().any(|e| matches!(e, egui::Event::Key { key: k, pressed: true, repeat: false, .. } if *k == key))
            })
        };
        let command = |key: Key, shift: bool| ctx.input_mut(|i| i.consume_key(Modifiers { shift, ..Modifiers::COMMAND }, key));
        if command(Key::Z, true) || command(Key::Y, false) {
            self.undo(ctx, true);
        } else if command(Key::Z, false) {
            self.undo(ctx, false);
        }
        if command(Key::S, true) {
            self.save_as(frame);
        } else if command(Key::S, false) {
            self.save(frame);
        }
        if command(Key::O, false) && !self.busy() {
            self.choose_file(ctx, frame);
        }
        if command(Key::E, false) {
            self.open_export();
        }
        if command(Key::A, false)
            && let Some(p) = &self.project
        {
            self.timeline.select_all(&p.cards);
        }
        if self.project.is_none() {
            return;
        }
        let sel = self.selection();
        if (once(Key::Delete, false) || once(Key::Backspace, false)) && !sel.is_empty() {
            self.edit(ctx, None, |c| c.set_deleted(&sel, true));
        }
        if once(Key::Escape, false) {
            self.timeline.selection.clear();
            self.active_effect = None;
        }
        if once(Key::D, false) {
            self.drop_keyframes(ctx);
        }
        for (shift, delta) in [(false, 1), (true, -1)] {
            if once(Key::B, shift) && !sel.is_empty() {
                self.edit(ctx, Some(Gesture::SelectionCopies), |c| c.add_copies(&sel, delta));
            }
        }
        if once(Key::R, false) && self.project.as_ref().is_some_and(|p| p.cards.pending_count() > 0) {
            self.start_render(ctx);
        }
        if once(Key::Space, false) {
            self.toggle_play(ctx);
        }
        if once(Key::L, false) {
            self.shuttle(ctx, true);
        }
        if once(Key::J, false) {
            self.shuttle(ctx, false);
        }
        if once(Key::K, false) {
            self.playback = None;
        }
        // arrows repeat while held
        let pressed = |key: Key| ctx.input_mut(|i| i.consume_key(Modifiers::NONE, key));
        for (key, delta) in [(Key::ArrowLeft, -1), (Key::ArrowRight, 1), (Key::Home, i64::MIN), (Key::End, i64::MAX)] {
            if pressed(key) {
                self.step(delta);
            }
        }
    }

    fn open_export(&mut self) {
        if self.project.as_ref().is_some_and(|p| p.cards.pending_count() == 0) && !self.busy() {
            self.export_dialog = Some(self.settings.export);
        }
    }

    // ---- UI ----

    fn menu_bar(&mut self, ui: &mut egui::Ui, frame: &eframe::Frame) {
        let ctx = ui.ctx().clone();
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button(tr!("menu-file"), |ui| {
                if ui.add_enabled(!self.busy(), egui::Button::new(tr!("menu-open")).shortcut_text(shortcut(&ctx, "O"))).clicked() {
                    self.choose_file(&ctx, frame);
                }
                ui.add_enabled_ui(!self.busy() && !self.settings.recent.is_empty(), |ui| {
                    ui.menu_button(tr!("menu-recent"), |ui| {
                        for file in self.settings.recent.clone() {
                            if ui.button(file.display().to_string()).clicked() {
                                self.request_open(&ctx, vec![file]);
                                ui.close();
                            }
                        }
                    });
                });
                ui.menu_button(tr!("menu-new-clips"), |ui| {
                    let s = &mut self.settings;
                    // the encoding choices do not apply to clips that keep their coding
                    ui.add_enabled_ui(!s.import_as_coded, |ui| {
                        for refs in [1, 2, 4] {
                            let label = tr!("import-refs", count = refs);
                            let hint = if refs == 1 { tr!("import-refs-one-tooltip") } else { tr!("import-refs-more-tooltip") };
                            if ui.radio_value(&mut s.import_refs, refs, label).on_hover_text(hint).changed() {
                                s.save();
                            }
                        }
                        ui.separator();
                        let choices = [
                            (0, BPyramid::None, tr!("import-bframes-none"), tr!("import-bframes-none-tooltip")),
                            (3, BPyramid::None, tr!("import-bframes", count = 3), tr!("import-bframes-tooltip")),
                            (3, BPyramid::Normal, tr!("import-bframes-pyramid", count = 3), tr!("import-bframes-pyramid-tooltip")),
                        ];
                        for (bframes, pyramid, label, hint) in choices {
                            let chosen = s.import_bframes == bframes && (bframes == 0 || s.import_b_pyramid == pyramid);
                            if ui.radio(chosen, label).on_hover_text(hint).clicked() && !chosen {
                                (s.import_bframes, s.import_b_pyramid) = (bframes, pyramid);
                                s.save();
                            }
                        }
                    });
                    ui.separator();
                    if ui.checkbox(&mut s.import_as_coded, tr!("import-as-coded")).on_hover_text(tr!("import-as-coded-tooltip")).changed() {
                        s.save();
                    }
                });
                ui.separator();
                let has = self.project.is_some();
                if ui.add_enabled(has, egui::Button::new(tr!("menu-save")).shortcut_text(shortcut(&ctx, "S"))).clicked() {
                    self.save(frame);
                }
                if ui.add_enabled(has, egui::Button::new(tr!("menu-save-as")).shortcut_text(shortcut(&ctx, "Shift+S"))).clicked() {
                    self.save_as(frame);
                }
                ui.separator();
                if ui.add_enabled(has, egui::Button::new(tr!("menu-export")).shortcut_text(shortcut(&ctx, "E"))).clicked() {
                    self.open_export();
                }
            });
            ui.menu_button(tr!("menu-edit"), |ui| {
                let (undo, redo) = self.project.as_ref().map_or((false, false), |p| (p.history.can_undo(), p.history.can_redo()));
                if ui.add_enabled(undo, egui::Button::new(tr!("menu-undo")).shortcut_text(shortcut(&ctx, "Z"))).clicked() {
                    self.undo(&ctx, false);
                }
                if ui.add_enabled(redo, egui::Button::new(tr!("menu-redo")).shortcut_text(shortcut(&ctx, "Shift+Z"))).clicked() {
                    self.undo(&ctx, true);
                }
                ui.separator();
                if ui.add_enabled(self.project.is_some(), egui::Button::new(tr!("menu-drop-keyframes")).shortcut_text("D")).clicked() {
                    self.drop_keyframes(&ctx);
                }
            });
            ui.menu_button(tr!("menu-help"), |ui| {
                if ui.button(tr!("menu-shortcuts")).clicked() {
                    self.show_shortcuts = true;
                }
            });
        });
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let Some(p) = &self.project else { return };
        let (name, pending) = (p.name.clone(), p.cards.pending_count());
        ui.horizontal(|ui| {
            ui.label(RichText::new(name).strong());
            ui.separator();
            if pending > 0 {
                ui.label(RichText::new(tr!("changes-pending", count = pending)).color(ui.visuals().warn_fg_color));
            } else {
                ui.label(RichText::new(tr!("all-rendered")).weak());
            }
            let render = egui::Button::new(tr!("render-button")).selected(pending > 0);
            if ui.add_enabled(pending > 0 && !self.busy(), render).on_hover_text(tr!("render-tooltip")).clicked() {
                self.start_render(&ctx);
            }
            if ui.checkbox(&mut self.settings.auto_render, tr!("auto-render")).on_hover_text(tr!("auto-render-tooltip")).changed() {
                self.settings.save();
            }
            ui.separator();
            let export = ui
                .add_enabled(pending == 0 && !self.busy(), egui::Button::new(tr!("export-button")))
                .on_hover_text(tr!("export-tooltip"))
                .on_disabled_hover_text(if pending > 0 { tr!("export-pending-tooltip") } else { tr!("export-tooltip") });
            if export.clicked() {
                self.open_export();
            }
        });
    }

    fn job_bar(&mut self, ui: &mut egui::Ui) {
        let Some(job) = &self.job else { return };
        let progress = match job {
            Running::Open(j) => j.progress(),
            Running::Render(j, _) => j.progress(),
            Running::Export(j, _) => j.progress(),
        };
        let text = match progress.stage {
            Stage::Starting => tr!("stage-starting"),
            Stage::Encoding { clip, clips } => tr!("stage-encoding", clip = clip + 1, clips = clips),
            Stage::Copying { clip, clips } => tr!("stage-copying", clip = clip + 1, clips = clips),
            Stage::Reading => tr!("stage-reading"),
            Stage::Rendering => tr!("stage-rendering"),
            Stage::Exporting => tr!("stage-exporting"),
        };
        let mut cancel = false;
        ui.horizontal(|ui| {
            cancel = ui.button(tr!("cancel-button")).clicked();
            let bar = match progress.fraction() {
                Some(f) => egui::ProgressBar::new(f).show_percentage(),
                None => egui::ProgressBar::new(0.0).animate(true),
            };
            ui.add(bar.text(text));
        });
        if cancel {
            match job {
                Running::Open(j) => j.cancel(),
                Running::Render(j, _) => j.cancel(),
                Running::Export(j, _) => j.cancel(),
            }
        }
    }

    fn notice_bar(&mut self, ui: &mut egui::Ui) {
        let Some(n) = &self.notice else { return };
        let color = if n.error { ui.visuals().error_fg_color } else { ui.visuals().warn_fg_color };
        let mut close = false;
        ui.horizontal(|ui| {
            close = ui.small_button("✕").on_hover_text(tr!("dismiss")).clicked();
            ui.add(egui::Label::new(RichText::new(&n.text).color(color)).wrap());
        });
        if close {
            self.notice = None;
        }
    }

    fn edit_tools(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let Some(p) = &self.project else { return };
        let sel = self.selection();
        let any = !sel.is_empty();
        let deleted = p.cards.cards.iter().any(|c| c.deleted && sel.contains(&c.id));
        // (delete or restore, copies to add)
        let mut edit: Option<(Option<bool>, i64)> = None;
        ui.horizontal(|ui| {
            if ui.add_enabled(any, egui::Button::new(tr!("delete-button"))).on_hover_text(tr!("delete-tooltip")).clicked() {
                edit = Some((Some(true), 0));
            }
            if ui.add_enabled(deleted, egui::Button::new(tr!("restore-button"))).on_hover_text(tr!("restore-tooltip")).clicked() {
                edit = Some((Some(false), 0));
            }
            if ui.add_enabled(any, egui::Button::new(tr!("duplicate-button"))).on_hover_text(tr!("duplicate-tooltip")).clicked() {
                edit = Some((None, 1));
            }
            if ui.add_enabled(any, egui::Button::new(tr!("unduplicate-button"))).on_hover_text(tr!("unduplicate-tooltip")).clicked() {
                edit = Some((None, -1));
            }
            ui.separator();
            if any {
                ui.label(tr!("selection-count", count = sel.len()));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let range = self.timeline.zoom_range();
                ui.add(egui::Slider::new(&mut self.timeline.zoom, range).logarithmic(true).show_value(false).text(tr!("zoom-label")))
                    .on_hover_text(tr!("zoom-tooltip"));
            });
        });
        match edit {
            Some((Some(deleted), _)) => self.edit(&ctx, None, |c| c.set_deleted(&sel, deleted)),
            Some((None, copies)) => self.edit(&ctx, None, |c| c.add_copies(&sel, copies)),
            None => {}
        }
    }

    fn timeline_strip(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let Some(p) = &self.project else { return };
        let (w, h) = p.size;
        let strip = Strip {
            cards: &p.cards,
            frames: &p.frames,
            display: &p.source.display,
            thumbs: &self.thumbs,
            aspect: h as f32 / w as f32,
            playhead: self.playhead_card(),
            active_effect: self.active_effect,
            bloom: ctx.input(|i| i.key_down(Key::B)) && !ctx.egui_wants_keyboard_input(),
        };
        for event in self.timeline.show(ui, strip) {
            self.timeline_event(&ctx, event);
        }
    }

    fn effects_panel(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let Some(p) = &self.project else { return };
        let context = panel::Context {
            frames: &p.frames,
            display: &p.source.display,
            selection: &self.timeline.selection,
            active: self.active_effect,
            macroblocks: p.macroblocks,
            refs: p.refs,
        };
        let actions = panel::show(ui, &p.cards, &context);
        for action in actions {
            self.panel_action(&ctx, action);
        }
    }

    fn preview_area(&mut self, ui: &mut egui::Ui) {
        let Some(preview) = &self.preview else {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() / 2.0 - 40.0).max(0.0));
                ui.label(RichText::new(tr!("empty-hint")).heading().weak());
                ui.add_space(8.0);
                let open = egui::Button::new(RichText::new(tr!("open-button")).heading());
                if ui.add_enabled(!self.busy(), open).on_hover_text(tr!("open-tooltip")).clicked() {
                    self.want_open = true;
                }
            });
            return;
        };
        let total = preview.rendered().len();
        let source = preview.rendered().report.frames.get(self.playhead).map(|f| f.src);
        // the area the active effect is limited to
        let area = self.project.as_ref().and_then(|p| {
            let lane = p.cards.effects.iter().position(|e| Some(e.id) == self.active_effect)?;
            Some((p.cards.effects[lane].mask.clone()?, panel::effect_color(lane)))
        });
        let playing = self.playback.as_ref().map(|p| p.speed);
        let mut step: Option<i64> = None;
        let (mut toggle, mut hold_original) = (false, false);
        ui.with_layout(Layout::bottom_up(Align::Center), |ui| {
            ui.horizontal(|ui| {
                if ui.button("⏮").on_hover_text(tr!("first-frame")).clicked() {
                    step = Some(i64::MIN);
                }
                if ui.button("⏴").on_hover_text(tr!("previous-frame")).clicked() {
                    step = Some(-1);
                }
                let play = if playing.is_some() { "⏸" } else { "▶" };
                toggle = ui.button(play).on_hover_text(tr!("play-tooltip")).clicked();
                if ui.button("⏵").on_hover_text(tr!("next-frame")).clicked() {
                    step = Some(1);
                }
                if ui.button("⏭").on_hover_text(tr!("last-frame")).clicked() {
                    step = Some(i64::MAX);
                }
                ui.label(tr!("preview-position", frame = self.playhead, total = total));
                if let Some(speed) = playing.filter(|s| *s != 1.0) {
                    ui.label(RichText::new(tr!("play-speed", speed = speed)).weak());
                }
                if let Some(s) = source {
                    ui.separator();
                    ui.label(RichText::new(tr!("preview-source", frame = s)).weak());
                }
                ui.separator();
                let original = ui.add(egui::Button::new(tr!("original-button")).sense(Sense::click_and_drag()));
                hold_original = original.is_pointer_button_down_on();
                original.on_hover_text(tr!("original-tooltip"));
            });
            let show_original = self.show_original || hold_original;
            let (view, k) = match (&mut self.original, source) {
                (Some(o), Some(src)) if show_original => (o, src),
                _ => (self.preview.as_mut().unwrap(), self.playhead),
            };
            let (texture, current) = view.frame(ui.ctx(), k);
            let (rect, _) = ui.allocate_exact_size(ui.available_size(), Sense::hover());
            if let Some(t) = texture {
                let size = t.size_vec2();
                let scale = (rect.width() / size.x).min(rect.height() / size.y);
                let image = egui::Rect::from_center_size(rect.center(), size * scale);
                let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                ui.painter().image(t.id(), image, uv, egui::Color32::WHITE);
                if !show_original && let Some((mask, color)) = &area {
                    paint_area(ui.painter(), image, mask, *color);
                }
                if show_original {
                    let label = egui::RichText::new(tr!("original-label")).strong().background_color(egui::Color32::from_black_alpha(160));
                    ui.put(egui::Rect::from_min_size(image.min + egui::vec2(8.0, 8.0), egui::vec2(200.0, 20.0)), egui::Label::new(label));
                }
            }
            if !current && view.error.is_none() {
                let spinner = egui::Rect::from_center_size(rect.center(), Vec2::splat(32.0));
                ui.put(spinner, egui::Spinner::new().size(32.0));
            }
        });
        for view in [&mut self.preview, &mut self.original].into_iter().flatten() {
            if let Some(e) = view.error.take() {
                self.notice = Some(Notice { error: true, text: tr!("preview-failed", reason = e) });
            }
        }
        if toggle {
            self.toggle_play(ui.ctx());
        }
        if let Some(s) = step {
            self.step(s);
        }
    }

    fn unsaved_dialog(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        if self.unsaved_question.is_none() {
            return;
        }
        let name = self.project.as_ref().map(|p| p.name.clone()).unwrap_or_default();
        let modal = egui::Modal::new(egui::Id::new("unsaved")).show(ctx, |ui| {
            ui.set_min_width(320.0);
            ui.heading(tr!("unsaved-title"));
            ui.label(tr!("unsaved-text", project = name));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(tr!("unsaved-save")).clicked() {
                    return Some(Answer::Save);
                }
                if ui.button(tr!("unsaved-discard")).clicked() {
                    return Some(Answer::Discard);
                }
                ui.button(tr!("cancel-button")).clicked().then_some(Answer::Cancel)
            })
            .inner
        });
        let close = modal.should_close();
        match modal.inner.or(close.then_some(Answer::Cancel)) {
            Some(Answer::Save) => self.answer_unsaved(ctx, frame, true),
            Some(Answer::Discard) => self.answer_unsaved(ctx, frame, false),
            Some(Answer::Cancel) => self.unsaved_question = None,
            None => {}
        }
    }

    fn shortcuts_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_shortcuts;
        egui::Window::new(tr!("shortcuts-title")).open(&mut open).resizable(false).collapsible(false).show(ctx, |ui| {
            egui::Grid::new("shortcuts").num_columns(2).spacing([16.0, 4.0]).striped(true).show(ui, |ui| {
                let rows = [
                    ("Space", tr!("shortcut-play")),
                    ("J / K / L", tr!("shortcut-shuttle")),
                    ("← / →, Home / End", tr!("shortcut-step")),
                    ("\\", tr!("shortcut-original")),
                    ("Del", tr!("shortcut-delete")),
                    ("D", tr!("shortcut-drop-keyframes")),
                    ("B / Shift+B", tr!("shortcut-bloom")),
                    ("R", tr!("shortcut-render")),
                    ("Esc", tr!("shortcut-deselect")),
                ];
                for (key, what) in rows {
                    ui.label(RichText::new(key).monospace());
                    ui.label(what);
                    ui.end_row();
                }
                for (key, what) in [
                    ("A", tr!("shortcut-select-all")),
                    ("Z", tr!("menu-undo")),
                    ("Shift+Z", tr!("menu-redo")),
                    ("S", tr!("menu-save")),
                    ("O", tr!("menu-open")),
                    ("E", tr!("menu-export")),
                ] {
                    ui.label(RichText::new(shortcut(ctx, key)).monospace());
                    ui.label(what);
                    ui.end_row();
                }
            });
        });
        self.show_shortcuts = open;
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let title = match &self.project {
            Some(p) if p.is_dirty() => tr!("window-title-unsaved", project = p.name.clone()),
            Some(p) => tr!("window-title", project = p.name.clone()),
            None => tr!("app-title"),
        };
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }
}

/// Outline the area of `mask` on the picture at `image`, and shade the part the effect
/// leaves alone.
fn paint_area(painter: &egui::Painter, image: egui::Rect, mask: &h264mosh::effects::Mask, color: egui::Color32) {
    let shade = egui::Color32::from_black_alpha(110);
    let to_screen = |[x, y, w, h]: [f32; 4]| {
        let min = image.min + egui::vec2(x * image.width(), y * image.height());
        egui::Rect::from_min_size(min, egui::vec2(w * image.width(), h * image.height())).intersect(image)
    };
    let rects: Vec<egui::Rect> = mask.rects.iter().map(|&r| to_screen(r)).collect();
    if mask.invert {
        for r in &rects {
            painter.rect_filled(*r, 0.0, shade);
        }
    } else {
        // shade around the rectangle (one rectangle is all the panel makes)
        if let [r] = rects[..] {
            for band in [
                egui::Rect::from_min_max(image.min, egui::pos2(image.max.x, r.min.y)),
                egui::Rect::from_min_max(egui::pos2(image.min.x, r.max.y), image.max),
                egui::Rect::from_min_max(egui::pos2(image.min.x, r.min.y), egui::pos2(r.min.x, r.max.y)),
                egui::Rect::from_min_max(egui::pos2(r.max.x, r.min.y), egui::pos2(image.max.x, r.max.y)),
            ] {
                painter.rect_filled(band, 0.0, shade);
            }
        }
    }
    for r in rects {
        painter.rect_stroke(r, 0.0, egui::Stroke::new(2.0, color), egui::StrokeKind::Inside);
    }
}

/// "Ctrl+S" or "⌘S"
fn shortcut(ctx: &egui::Context, key: &str) -> String {
    if ctx.os() == egui::os::OperatingSystem::Mac { format!("⌘{key}") } else { format!("Ctrl+{key}") }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_job(&ctx);
        if let Some(feed) = &self.thumb_feed {
            for (i, pixels) in feed.drain() {
                if let (Some(slot), Some(p)) = (self.thumbs.get_mut(i), &self.project) {
                    let (w, h) = p.thumb_size();
                    let image = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &pixels);
                    *slot = Some(ctx.load_texture(format!("thumb{i}"), image, egui::TextureOptions::LINEAR));
                }
            }
        }
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if !dropped.is_empty() {
            self.request_open(&ctx, dropped);
        }
        if ctx.input(|i| i.viewport().close_requested()) && self.dirty() && !self.allow_close {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.unsaved_question = Some(AfterSave::Quit);
        }
        self.keyboard(&ctx, frame);
        self.show_original = ctx.input(|i| i.key_down(Key::Backslash)) && !ctx.egui_wants_keyboard_input();
        self.tick(&ctx);
        if self.settings.auto_render && !self.busy() && self.project.as_ref().is_some_and(|p| p.cards.pending_count() > 0) {
            let wait = self.last_edit + AUTO_RENDER_DELAY - ctx.input(|i| i.time);
            if wait <= 0.0 {
                self.start_render(&ctx);
            } else {
                ctx.request_repaint_after(std::time::Duration::from_secs_f64(wait));
            }
        }
        self.update_title(&ctx);

        egui::Panel::top("header").show(ui, |ui| {
            self.menu_bar(ui, frame);
            self.header(ui);
            self.job_bar(ui);
            self.notice_bar(ui);
            ui.add_space(2.0);
        });
        if self.project.is_some() {
            egui::Panel::bottom("timeline").resizable(true).default_size(190.0).show(ui, |ui| {
                ui.add_space(4.0);
                self.edit_tools(ui);
                self.timeline_strip(ui);
            });
            egui::Panel::right("effects").resizable(true).default_size(260.0).show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.effects_panel(ui));
            });
        }
        egui::CentralPanel::default().show(ui, |ui| self.preview_area(ui));

        if let Some(mut settings) = self.export_dialog {
            let answer = export::dialog(&ctx, &mut settings);
            self.export_dialog = Some(settings);
            if let Some(go) = answer {
                self.export_dialog = None;
                if go {
                    self.start_export(&ctx, frame, settings);
                }
            }
        }
        self.unsaved_dialog(&ctx, frame);
        self.shortcuts_window(&ctx);
        if std::mem::take(&mut self.want_open) {
            self.choose_file(&ctx, frame);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::{ExportFormat, Quality};
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;
    use h264mosh::effects::{Mask, MbEffect};
    use std::process::Command;
    use std::time::Duration;

    fn test_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("h264mosh-gui-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A clip of `secs` seconds (25 fps, keyframe every 25 frames, CAVLC, no B-frames):
    /// a raw stream for a .264 name, otherwise a container. None without ffmpeg.
    fn test_clip(name: &str, secs: u32, size: &str) -> Option<PathBuf> {
        let path = test_dir().join(name);
        let mut cmd = Command::new(Tools::find(None).ffmpeg);
        cmd.args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i"])
            .arg(format!("testsrc2=size={size}:rate=25"))
            .args(["-t", &secs.to_string(), "-c:v", "libx264", "-pix_fmt", "yuv420p", "-g", "25", "-bf", "0", "-refs", "1", "-coder", "0"])
            .args(["-x264-params", "weightp=0:scenecut=0"]);
        if name.ends_with(".264") {
            cmd.args(["-f", "h264"]);
        }
        if !cmd.arg(&path).status().is_ok_and(|s| s.success()) {
            eprintln!("ffmpeg not available, skipping");
            return None;
        }
        Some(path)
    }

    /// A two-second CABAC clip, coded as most encoders do (`test_clip` makes CAVLC).
    fn cabac_clip(name: &str) -> PathBuf {
        let path = test_dir().join(name);
        let mut cmd = Command::new(Tools::find(None).ffmpeg);
        cmd.args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=25", "-t", "2"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-g", "25", "-bf", "0"]);
        if name.ends_with(".264") {
            cmd.args(["-f", "h264"]);
        }
        assert!(cmd.arg(&path).status().unwrap().success());
        path
    }

    fn harness(gpu: bool) -> Harness<'static, App> {
        let builder = Harness::builder().with_size([1280.0, 800.0]);
        let builder = if gpu { builder.wgpu() } else { builder };
        let mut h = builder.build_eframe(|cc| App::new(cc));
        h.state_mut().settings = Settings::in_memory();
        h.state_mut().cache_dir = test_dir().join("cache");
        h
    }

    /// Step the UI until `done` holds.
    fn wait(h: &mut Harness<App>, done: impl Fn(&App) -> bool) {
        for _ in 0..2000 {
            h.step();
            if done(h.state()) {
                h.run_steps(2);
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out");
    }

    fn open(h: &mut Harness<App>, path: PathBuf) {
        open_all(h, vec![path]);
        assert!(h.state().notice.is_none(), "{}", h.state().notice.as_ref().unwrap().text);
    }

    /// Open clips; the notice, if any, tells how it went.
    fn open_all(h: &mut Harness<App>, paths: Vec<PathBuf>) {
        let ctx = h.ctx.clone();
        h.state_mut().notice = None;
        h.state_mut().start_open(&ctx, paths);
        wait(h, |a| a.job.is_none());
    }

    fn render(h: &mut Harness<App>) {
        let ctx = h.ctx.clone();
        h.state_mut().start_render(&ctx);
        wait(h, |a| a.job.is_none());
    }

    fn project<'a>(h: &'a Harness<'_, App>) -> &'a Project {
        h.state().project.as_ref().unwrap()
    }

    fn cards<'a>(h: &'a mut Harness<'_, App>) -> &'a mut Cards {
        &mut h.state_mut().project.as_mut().unwrap().cards
    }

    fn rendered(h: &Harness<App>) -> Arc<Rendered> {
        h.state().preview.as_ref().unwrap().rendered().clone()
    }

    #[test]
    fn import_edit_render_export() {
        let Some(clip) = test_clip("edit.264", 3, "320x180") else { return };
        let mut h = harness(false);
        open(&mut h, clip.clone());
        assert_eq!(project(&h).cards.len(), 75);
        cards(&mut h).set_deleted(&(30..35).collect(), true);
        cards(&mut h).add_copies(&HashSet::from([40]), 2);
        cards(&mut h).move_to(&HashSet::from([60]), 10);
        assert_eq!(project(&h).cards.pending_count(), 7);
        render(&mut h);
        assert_eq!((project(&h).cards.len(), project(&h).cards.pending_count()), (70, 0));
        let out = rendered(&h);
        assert_eq!(out.len(), 72);
        assert_eq!(out.report.frames[10].src, 60);

        // every export format
        let formats =
            [("export.264", ExportFormat::Raw), ("export-copy.mp4", ExportFormat::Copy), ("export-baked.mp4", ExportFormat::Baked)];
        for (name, format) in formats {
            let path = clip.with_file_name(name);
            let settings = ExportSettings { format, quality: Quality::Small };
            let (tools, input, file) = (Tools::find(None), out.clone(), path.clone());
            let job = Job::spawn(&h.ctx, move |r| project::export(&tools, &input, "25", &file, &settings, r));
            let result = loop {
                match job.poll() {
                    Some(r) => break r,
                    None => std::thread::sleep(Duration::from_millis(10)),
                }
            };
            result.unwrap();
            let data = h264mosh::pipeline::load_annexb(&Tools::find(None), &path).unwrap();
            if format == ExportFormat::Raw {
                assert!(data == out.bytes);
            }
            let frames = h264mosh::mosh::Source::new(h264mosh::stream::Stream::parse(&data).unwrap()).len();
            assert_eq!(frames, 72, "{name}");
        }

        // the output has to start with a keyframe
        cards(&mut h).set_deleted(&HashSet::from([0]), true);
        render(&mut h);
        assert!(h.state().notice.as_ref().is_some_and(|n| n.error));
        assert_eq!(rendered(&h).len(), 72);
    }

    fn mouse(h: &mut Harness<App>, pos: egui::Pos2, pressed: bool, modifiers: Modifiers) {
        h.event(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers });
    }

    /// Click the middle of card `i`.
    fn click(h: &mut Harness<App>, i: usize, modifiers: Modifiers) {
        let pos = card(h, i).center();
        h.event(egui::Event::ModifiersChanged(modifiers));
        h.event(egui::Event::PointerMoved(pos));
        mouse(h, pos, true, modifiers);
        mouse(h, pos, false, modifiers);
        h.event(egui::Event::ModifiersChanged(Modifiers::NONE));
        h.run_steps(2);
    }

    fn drag(h: &mut Harness<App>, from: egui::Pos2, to: egui::Pos2) {
        h.event(egui::Event::PointerMoved(from));
        h.drag_at(from);
        for k in 1..=10 {
            h.hover_at(from + (to - from) * k as f32 / 10.0);
            h.step();
        }
        h.drop_at(to);
        h.run_steps(2);
    }

    fn key(h: &mut Harness<App>, key: Key, modifiers: Modifiers) {
        h.key_press_modifiers(modifiers, key);
        h.run_steps(2);
    }

    fn card(h: &Harness<App>, i: usize) -> egui::Rect {
        h.state().timeline.card_rect(i)
    }

    fn order(h: &Harness<App>) -> Vec<usize> {
        project(h).cards.cards.iter().map(|c| c.src).collect()
    }

    fn selected(h: &Harness<App>) -> Vec<CardId> {
        let mut s: Vec<CardId> = h.state().timeline.selection.iter().copied().collect();
        s.sort();
        s
    }

    /// Click, Shift-click, dragging, the wheel over a copy badge, restore, and undo.
    #[test]
    fn timeline_mouse_editing_and_undo() {
        let Some(clip) = test_clip("mouse.264", 2, "320x180") else { return };
        let mut h = harness(false);
        open(&mut h, clip);
        h.state_mut().timeline.zoom = 60.0;
        h.run_steps(2);

        click(&mut h, 2, Modifiers::NONE);
        click(&mut h, 4, Modifiers::SHIFT);
        assert_eq!(selected(&h), [2, 3, 4]);

        // drag the selection between cards 7 and 8
        let (from, to) = (card(&h, 3).center(), card(&h, 8).left_center() - egui::vec2(1.0, 0.0));
        drag(&mut h, from, to);
        assert_eq!(order(&h)[..10], [0, 1, 5, 6, 7, 2, 3, 4, 8, 9]);
        assert_eq!(project(&h).cards.pending_count(), 3);

        // scrolling up over the copy badge of card 0 adds copies
        h.hover_at(card(&h, 0).right_top() + egui::vec2(-8.0, 7.0));
        h.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, 120.0),
            phase: egui::TouchPhase::Move,
            modifiers: Modifiers::NONE,
        });
        h.run_steps(20);
        assert!(project(&h).cards.cards[0].copies > 1);
        assert_eq!(order(&h)[0], 0, "the strip did not scroll");

        // delete with the key, restore with the button on the empty slot
        click(&mut h, 9, Modifiers::NONE);
        key(&mut h, Key::Delete, Modifiers::NONE);
        assert!(project(&h).cards.cards[9].deleted);
        click(&mut h, 9, Modifiers::NONE);
        assert!(!project(&h).cards.cards[9].deleted);

        // undo walks back: restore, delete, copies (one step for the wheel), move
        for _ in 0..4 {
            key(&mut h, Key::Z, Modifiers::COMMAND);
        }
        assert_eq!(order(&h)[..10], [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(project(&h).cards.cards[0].copies, 1);
        assert_eq!(project(&h).cards.pending_count(), 0);
        key(&mut h, Key::Z, Modifiers { shift: true, ..Modifiers::COMMAND });
        assert_eq!(order(&h)[5], 2, "redo the move");
    }

    /// Effects from the panel, rendered; a lane edge dragged with the mouse; D and B.
    #[test]
    fn effects_and_shortcuts() {
        let Some(clip) = test_clip("fx.264", 2, "320x180") else { return };
        let mut h = harness(false);
        open(&mut h, clip);
        assert!(project(&h).macroblocks);
        h.state_mut().timeline.zoom = 40.0;
        h.state_mut().timeline.selection = (5..10).collect();
        let ctx = h.ctx.clone();
        h.state_mut().panel_action(&ctx, Action::Add(MbEffect::MvScale(4.0, -2.0)));
        h.run_steps(2);
        let fx = h.state().active_effect.unwrap();
        assert_eq!(project(&h).cards.pending_count(), 5);
        render(&mut h);
        let out = rendered(&h);
        assert_eq!(out.report.effects.slices, 5);
        assert!(out.report.failed_effects.is_empty());

        // drag the right edge of the block from card 9 to card 14
        let lane = |h: &Harness<App>, i| h.state().timeline.lane_rect(0, i);
        let (from, to) = (lane(&h, 9).right_center() - egui::vec2(1.0, 0.0), lane(&h, 14).center());
        drag(&mut h, from, to);
        let with: Vec<usize> = project(&h).cards.cards.iter().filter(|c| c.effects.contains(&fx)).map(|c| c.src).collect();
        assert_eq!(with, (5..15).collect::<Vec<_>>());
        // and the left edge from card 5 back to 7
        let (from, to) = (lane(&h, 5).left_center() + egui::vec2(1.0, 0.0), lane(&h, 7).center());
        drag(&mut h, from, to);
        let with: Vec<usize> = project(&h).cards.cards.iter().filter(|c| c.effects.contains(&fx)).map(|c| c.src).collect();
        assert_eq!(with, (7..15).collect::<Vec<_>>());
        // parameter changes made by a slider gesture are one undo step
        for x in [5.0, 6.0, 7.0] {
            h.state_mut().panel_action(&ctx, Action::Params(fx, MbEffect::MvScale(x, -2.0)));
        }
        key(&mut h, Key::Z, Modifiers::COMMAND);
        assert_eq!(project(&h).cards.effect(fx).unwrap().effect, MbEffect::MvScale(4.0, -2.0));

        // D drops every keyframe after the first; B adds a copy to the selection
        h.state_mut().timeline.selection.clear();
        key(&mut h, Key::D, Modifiers::NONE);
        let dropped: Vec<usize> = project(&h).cards.cards.iter().filter(|c| c.deleted).map(|c| c.src).collect();
        assert_eq!(dropped, [25]);
        h.state_mut().timeline.selection = HashSet::from([3]);
        key(&mut h, Key::B, Modifiers::NONE);
        assert_eq!(project(&h).cards.cards[3].copies, 2);
    }

    /// New effects from the panel, one limited to an area; macroblock and picture effects
    /// on a CABAC stream; clips imported with several reference frames.
    #[test]
    fn effects_with_areas_and_picture_effects() {
        let Some(clip) = test_clip("areas.264", 2, "320x180") else { return };
        let mut h = harness(false);
        open(&mut h, clip);
        h.state_mut().timeline.selection = (0..30).collect();
        let ctx = h.ctx.clone();
        let mut lanes = Vec::new();
        for e in [
            MbEffect::IntraMode(4),
            MbEffect::QpAdd(12),
            MbEffect::Coeffs(0.25, 0.0),
            MbEffect::Transplant(40, 0.5),
            MbEffect::Deblock(6, 6),
            MbEffect::Weight(1.1, 4),
        ] {
            h.state_mut().panel_action(&ctx, Action::Add(e));
            lanes.push(h.state().active_effect.unwrap());
        }
        let mask = Mask::rect(0.0, 0.0, 0.5, 0.5);
        h.state_mut().panel_action(&ctx, Action::Mask(lanes[0], Some(mask.clone())));
        h.state_mut().active_effect = Some(lanes[0]);
        h.run_steps(2);
        assert_eq!(project(&h).cards.effect(lanes[0]).unwrap().mask, Some(mask));
        render(&mut h);
        let out = rendered(&h);
        assert!(out.report.failed_effects.is_empty(), "{:?}", out.report.failed_effects);
        assert_eq!(out.report.effects.slices, 30);
        // the area is one undo step
        key(&mut h, Key::Z, Modifiers::COMMAND);
        assert_eq!(project(&h).cards.effect(lanes[0]).unwrap().mask, None);

        // a CABAC stream that keeps its coding takes macroblock and picture effects
        let cabac = cabac_clip("cabac.264");
        let mut g = harness(false);
        g.state_mut().settings.import_as_coded = true;
        open(&mut g, cabac.clone());
        assert!(project(&g).encoding.is_none() && project(&g).stream_path == cabac);
        assert!(project(&g).macroblocks);
        g.state_mut().timeline.selection = (10..20).collect();
        let ctx = g.ctx.clone();
        g.state_mut().panel_action(&ctx, Action::Add(MbEffect::MvScale(2.0, 1.0)));
        g.state_mut().panel_action(&ctx, Action::Add(MbEffect::IntraMode(1)));
        g.state_mut().panel_action(&ctx, Action::Add(MbEffect::Tint(6, -6)));
        g.state_mut().panel_action(&ctx, Action::Add(MbEffect::Deblock(-6, -6)));
        render(&mut g);
        assert!(g.state().notice.is_none(), "{}", g.state().notice.as_ref().unwrap().text);
        assert_eq!(rendered(&g).report.effects.slices, 10);

        // new clips with more reference frames
        let mp4 = test_clip("refs.mp4", 1, "320x180").unwrap();
        let mut k = harness(false);
        k.state_mut().settings.import_refs = 2;
        open(&mut k, mp4);
        assert_eq!(project(&k).refs, 2);
        assert_eq!(project(&k).encoding.as_ref().unwrap().refs, 2);
    }

    /// Clips imported with B-frames: the timeline shows them in display order, with
    /// their thumbnails, and a B-frame moved elsewhere renders.
    #[test]
    fn bframe_clips_edit_in_display_order() {
        let Some(mp4) = test_clip("bframes.mp4", 2, "320x180") else { return };
        let mut h = harness(false);
        h.state_mut().settings.import_bframes = 3;
        open(&mut h, mp4);
        let p = project(&h);
        assert_eq!(p.encoding.as_ref().unwrap().bframes, 3);
        let srcs: Vec<usize> = p.cards.cards.iter().map(|c| c.src).collect();
        assert_eq!(srcs, p.source.display);
        assert_ne!(srcs, (0..srcs.len()).collect::<Vec<_>>(), "B-frames are decoded after the frame they precede");
        let b = p.cards.cards.iter().find(|c| p.frames[c.src].kind == 'B').unwrap().clone();
        assert!(b.src < 19);
        wait(&mut h, |a| a.thumbs.iter().all(Option::is_some));
        h.state_mut().project.as_mut().unwrap().cards.move_to(&HashSet::from([b.id]), 20);
        render(&mut h);
        assert!(h.state().notice.is_none(), "{}", h.state().notice.as_ref().unwrap().text);
        let out = rendered(&h);
        assert_eq!(out.len(), srcs.len());
        assert_eq!(out.frame_of(b.id), Some(19));
        assert_eq!(out.report.frames[19].src, b.src);
    }

    /// By default clips are encoded to CAVLC (raw CAVLC streams are used as they are);
    /// clips that keep their coding are copied out of their files, joined only if they
    /// are coded alike, and copied again when a project is opened without its stream.
    #[test]
    fn clips_keep_their_coding_when_asked() {
        let Some(cavlc) = test_clip("coding.264", 1, "320x180") else { return };
        let (cabac, mp4) = (cabac_clip("coding_cabac.264"), cabac_clip("coding_cabac.mp4"));
        let cabac_slices = |p: &Project| {
            let stream = h264mosh::stream::Stream::parse(&p.data).unwrap();
            stream.nals.iter().filter_map(|n| n.slice()).all(|s| s.pps.entropy_coding_mode_flag)
        };
        let mut h = harness(false);
        open(&mut h, cavlc.clone());
        assert!(project(&h).encoding.is_none() && project(&h).stream_path == cavlc);
        for clip in [&cabac, &mp4] {
            open(&mut h, clip.clone());
            assert!(project(&h).encoding.is_some() && !cabac_slices(project(&h)), "{}", clip.display());
        }

        h.state_mut().settings.import_as_coded = true;
        open(&mut h, cabac.clone());
        assert!(project(&h).encoding.is_none() && project(&h).stream_path == cabac);
        open(&mut h, mp4.clone());
        let tools = Tools::find(None);
        let copied = h264mosh::pipeline::load_annexb(&tools, &mp4).unwrap();
        let p = project(&h);
        assert!(p.encoding.is_none() && cabac_slices(p) && p.macroblocks);
        assert!(*p.data == copied, "the stream is copied bit for bit");
        assert!(p.stream_path.starts_with(test_dir().join("cache")));
        open_all(&mut h, vec![mp4.clone(), mp4.clone()]);
        assert_eq!(project(&h).source.len(), 100);
        open_all(&mut h, vec![mp4.clone(), cavlc.clone()]);
        assert!(h.state().notice.as_ref().is_some_and(|n| n.error && n.text.contains("coded differently")));

        // a project without its stream copies it again
        open(&mut h, mp4.clone());
        let file = test_dir().join("coding.h264mosh");
        assert!(h.state_mut().save_to(&file));
        std::fs::remove_file(&project(&h).stream_path).unwrap();
        let mut g = harness(false);
        open(&mut g, file);
        assert!(*project(&g).data == copied && project(&g).encoding.is_none());
        assert!(g.state().dirty(), "the new stream is worth saving");
    }

    #[test]
    fn project_files_save_and_open() {
        let Some(clip) = test_clip("saved.mp4", 2, "320x180") else { return };
        let mut h = harness(false);
        open(&mut h, clip);
        assert!(project(&h).encoding.is_some(), "an mp4 is encoded for moshing");
        cards(&mut h).set_deleted(&HashSet::from([25]), true);
        let fx = cards(&mut h).add_effect(MbEffect::MvJitter(12), &(30..40).collect());
        render(&mut h);
        cards(&mut h).add_copies(&HashSet::from([44]), 3);
        assert!(h.state().dirty());
        let file = test_dir().join("saved.h264mosh");
        assert!(h.state_mut().save_to(&file));
        assert!(!h.state().dirty());
        let state = project(&h).cards.state();

        // opening it again renders the pending change and leaves nothing to save
        let mut g = harness(false);
        open(&mut g, file.clone());
        assert_eq!(project(&g).cards.state().cards.len(), state.cards.len());
        assert_eq!(project(&g).cards.effect(fx).unwrap().effect, MbEffect::MvJitter(12));
        assert_eq!(project(&g).cards.cards.iter().find(|c| c.src == 44).unwrap().copies, 4);
        assert_eq!(project(&g).cards.pending_count(), 0);
        assert!(!g.state().dirty());
        render(&mut h);
        assert!(rendered(&g).bytes == rendered(&h).bytes, "the same edit renders the same");

        // without its stream, the clips are encoded again
        std::fs::remove_file(&project(&g).stream_path).unwrap();
        let mut k = harness(false);
        open(&mut k, file);
        assert_eq!(project(&k).source.len(), 50);
        assert_eq!(project(&k).cards.effect(fx).unwrap().effect, MbEffect::MvJitter(12));
        assert!(k.state().dirty(), "the new stream is worth saving");

        // unsaved changes stop opening something else until answered
        let other = test_clip("other.264", 1, "320x180").unwrap();
        cards(&mut k).add_copies(&HashSet::from([1]), 1);
        let ctx = k.ctx.clone();
        k.state_mut().request_open(&ctx, vec![other]);
        assert!(k.state().unsaved_question.is_some() && k.state().job.is_none());
    }

    /// Playing moves the playhead at the frame rate, as far as frames are decoded.
    #[test]
    fn playback() {
        let Some(clip) = test_clip("play.264", 2, "320x180") else { return };
        let mut h = harness(false);
        open(&mut h, clip);
        key(&mut h, Key::L, Modifiers::NONE);
        assert!(h.state().playback.is_some());
        wait(&mut h, |a| a.playhead >= 20);
        key(&mut h, Key::K, Modifiers::NONE);
        let at = h.state().playhead;
        h.run_steps(10);
        assert_eq!(h.state().playhead, at, "stopped");
        key(&mut h, Key::J, Modifiers::NONE);
        wait(&mut h, |a| a.playhead + 10 <= at);
    }

    /// Writes screenshots to target/ui-shots for looking at the UI:
    /// `cargo test -p h264mosh-gui screenshots -- --ignored`.
    /// $H264MOSH_SHOT_INPUT may name a clip to use instead of a test pattern.
    #[test]
    #[ignore]
    fn screenshots() {
        let clip = match std::env::var_os("H264MOSH_SHOT_INPUT") {
            Some(p) => PathBuf::from(p),
            None => match test_clip("shots.264", 8, "640x360") {
                Some(p) => p,
                None => return,
            },
        };
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/ui-shots");
        std::fs::create_dir_all(&dir).unwrap();
        let mut h = harness(true);
        let shot = |h: &mut Harness<App>, name: &str| {
            h.run_steps(3);
            h.render().unwrap().save(dir.join(format!("{name}.png"))).unwrap();
        };
        shot(&mut h, "1_empty");
        let ctx = h.ctx.clone();
        h.state_mut().start_open(&ctx, vec![clip]);
        // catch the progress bar of clips that need encoding
        for _ in 0..3000 {
            h.step();
            let encoding = |a: &App| match &a.job {
                Some(Running::Open(j)) => matches!(j.progress().stage, Stage::Encoding { .. }) && j.progress().done > 20,
                _ => false,
            };
            if h.state().job.is_none() || encoding(h.state()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if h.state().job.is_some() {
            shot(&mut h, "1b_importing");
        }
        wait(&mut h, |a| a.job.is_none());
        wait(&mut h, |a| a.thumbs.iter().all(Option::is_some));
        shot(&mut h, "2_opened");

        cards(&mut h).set_deleted(&(3..5).collect(), true);
        cards(&mut h).add_copies(&HashSet::from([6]), 2);
        cards(&mut h).move_to(&HashSet::from([1]), 9);
        h.state_mut().timeline.selection = (8..14).collect();
        let ctx = h.ctx.clone();
        h.state_mut().panel_action(&ctx, Action::Add(MbEffect::MvScale(3.0, 1.5)));
        h.state_mut().timeline.selection = (10..30).collect();
        h.state_mut().panel_action(&ctx, Action::Add(MbEffect::KillIntra));
        shot(&mut h, "3_edited");

        render(&mut h);
        h.state_mut().playhead = 12;
        wait(&mut h, |a| a.preview.as_ref().is_some_and(|p| p.shows(12)));
        shot(&mut h, "4_rendered");

        // an effect limited to an area, shown on the preview while selected
        h.state_mut().timeline.selection = (0..20).collect();
        h.state_mut().panel_action(&ctx, Action::Add(MbEffect::IntraMode(4)));
        let fx = h.state().active_effect.unwrap();
        h.state_mut().panel_action(&ctx, Action::Mask(fx, Some(Mask::rect(0.1, 0.2, 0.5, 0.6))));
        render(&mut h);
        h.state_mut().playhead = 2;
        wait(&mut h, |a| a.preview.as_ref().is_some_and(|p| p.shows(2)));
        shot(&mut h, "4b_area");
        let add = h.get_by_label(&tr!("effect-add")).rect().center();
        h.hover_at(add);
        mouse(&mut h, add, true, Modifiers::NONE);
        mouse(&mut h, add, false, Modifiers::NONE);
        h.run_steps(3);
        let group = h.get_by_label_contains(&tr!("group-intra")).rect().center();
        h.hover_at(group);
        shot(&mut h, "4c_add_menu");
        h.key_press(Key::Escape);
        h.run_steps(3);

        h.state_mut().show_shortcuts = true;
        h.state_mut().export_dialog = Some(ExportSettings::default());
        shot(&mut h, "5_dialogs");
        h.state_mut().show_shortcuts = false;
        h.state_mut().export_dialog = None;

        h.state_mut().timeline.zoom = 6.0;
        shot(&mut h, "6_zoomed_out");
    }
}
