use crate::edit::{CardId, RenderSnapshot};
use crate::jobs::{Feed, Job, Stage};
use crate::preview::Preview;
use crate::project::{self, Imported, Project, RenderRequest, Rendered};
use crate::timeline::{Event, TimelineView};
use crate::tr;
use eframe::egui::{self, Align, Key, Layout, Modifiers, RichText, Sense, Vec2};
use h264mosh::pipeline::{Cancelled, Tools};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// seconds without edits before an automatic render
const AUTO_RENDER_DELAY: f64 = 0.4;
const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mov", "mkv", "avi", "webm", "m4v", "264", "h264"];

enum Running {
    Import(Job<Imported>),
    Render(Job<Rendered>, RenderSnapshot),
    Export(Job<()>, PathBuf),
}

enum Finished {
    Import(anyhow::Result<Box<Imported>>),
    Render(anyhow::Result<Box<Rendered>>),
    Export(anyhow::Result<()>),
}

struct Notice {
    error: bool,
    text: String,
}

pub struct App {
    tools: Tools,
    /// where imported clips are encoded to
    cache_dir: PathBuf,
    project: Option<Project>,
    preview: Option<Preview>,
    thumbs: Vec<Option<egui::TextureHandle>>,
    thumb_feed: Option<Feed<(usize, Vec<u8>)>>,
    timeline: TimelineView,
    /// output frame of the last render shown in the preview
    playhead: usize,
    job: Option<Running>,
    auto_render: bool,
    last_edit: f64,
    notice: Option<Notice>,
}

impl App {
    pub fn new(_cc: &eframe::CreationContext) -> Self {
        Self {
            tools: Tools::find(None),
            cache_dir: project::cache_dir(),
            project: None,
            preview: None,
            thumbs: Vec::new(),
            thumb_feed: None,
            timeline: TimelineView::default(),
            playhead: 0,
            job: None,
            auto_render: false,
            last_edit: 0.0,
            notice: None,
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

    fn start_import(&mut self, ctx: &egui::Context, inputs: Vec<PathBuf>) {
        if inputs.is_empty() || self.busy() {
            return;
        }
        let (tools, cache_dir) = (self.tools.clone(), self.cache_dir.clone());
        self.job = Some(Running::Import(Job::spawn(ctx, move |r| project::import(&tools, &inputs, &cache_dir, r))));
    }

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

    fn start_export(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        let (Some(p), Some(preview)) = (&self.project, &self.preview) else { return };
        let stem = Path::new(&p.name).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let dialog = rfd::FileDialog::new()
            .set_parent(frame)
            .add_filter(tr!("filter-mp4"), &["mp4"])
            .add_filter(tr!("filter-raw"), &["264"])
            .set_file_name(format!("{stem}-mosh.mp4"));
        let Some(path) = dialog.save_file() else { return };
        let (tools, rendered, fps, out) = (self.tools.clone(), preview.rendered().clone(), p.fps.clone(), path.clone());
        let job = Job::spawn(ctx, move |r| project::export(&tools, &rendered, &fps, &out, r));
        self.job = Some(Running::Export(job, path));
    }

    fn open_dialog(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        let picked = rfd::FileDialog::new().set_parent(frame).add_filter(tr!("filter-video"), VIDEO_EXTENSIONS).pick_files();
        if let Some(files) = picked {
            self.start_import(ctx, files);
        }
    }

    /// Take the result of a finished job.
    fn poll_job(&mut self, ctx: &egui::Context) {
        let finished = match &self.job {
            Some(Running::Import(j)) => j.poll().map(|r| Finished::Import(r.map(Box::new))),
            Some(Running::Render(j, _)) => j.poll().map(|r| Finished::Render(r.map(Box::new))),
            Some(Running::Export(j, _)) => j.poll().map(Finished::Export),
            None => None,
        };
        let Some(finished) = finished else { return };
        let job = self.job.take();
        let cancelled = |e: &anyhow::Error| e.downcast_ref::<Cancelled>().is_some();
        match (finished, job) {
            (Finished::Import(Ok(imported)), _) => self.apply_import(ctx, *imported),
            (Finished::Render(Ok(rendered)), Some(Running::Render(_, snapshot))) => self.apply_render(*rendered, snapshot),
            (Finished::Export(Ok(())), Some(Running::Export(_, path))) => self.info(tr!("export-done", path = path.display().to_string())),
            (Finished::Import(Err(e)), _) if !cancelled(&e) => self.error(tr!("import-failed", reason = format!("{e:#}"))),
            (Finished::Render(Err(e)), _) if !cancelled(&e) => self.error(tr!("render-failed", reason = format!("{e:#}"))),
            (Finished::Export(Err(e)), _) if !cancelled(&e) => self.error(tr!("export-failed", reason = format!("{e:#}"))),
            _ => {}
        }
    }

    fn apply_import(&mut self, ctx: &egui::Context, imported: Imported) {
        let Imported { project, rendered } = imported;
        let (tools, data, size) = (self.tools.clone(), project.data.clone(), project.thumb_size());
        self.thumb_feed = Some(Feed::spawn(ctx, move |send| project::thumbnails(&tools, &data, size, send)));
        self.thumbs = vec![None; project.source.len()];
        self.preview = Some(Preview::new(self.tools.clone(), Arc::new(rendered), project.preview_size()));
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(tr!("window-title", project = project.name.clone())));
        self.project = Some(project);
        self.timeline = TimelineView::default();
        self.playhead = 0;
        self.notice = None;
    }

    fn apply_render(&mut self, rendered: Rendered, snapshot: RenderSnapshot) {
        let card = self.playhead_card();
        let Some(p) = &mut self.project else { return };
        p.cards.commit(snapshot);
        let warnings: Vec<String> =
            rendered.report.warnings.iter().chain(rendered.report.failed_effects.iter().map(|(_, r)| r)).cloned().collect();
        self.playhead = card.and_then(|c| rendered.frame_of(c)).unwrap_or(self.playhead).min(rendered.len().saturating_sub(1));
        self.preview = Some(Preview::new(self.tools.clone(), Arc::new(rendered), p.preview_size()));
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

    fn edited(&mut self, ctx: &egui::Context) {
        self.last_edit = ctx.input(|i| i.time);
    }

    fn header(&mut self, ui: &mut egui::Ui, frame: &eframe::Frame) {
        let ctx = ui.ctx().clone();
        ui.horizontal(|ui| {
            if ui.add_enabled(!self.busy(), egui::Button::new(tr!("open-button"))).on_hover_text(tr!("open-tooltip")).clicked() {
                self.open_dialog(&ctx, frame);
            }
            let Some(p) = &self.project else { return };
            ui.separator();
            ui.label(RichText::new(&p.name).strong());
            ui.separator();
            let pending = p.cards.pending_count();
            if pending > 0 {
                ui.label(RichText::new(tr!("changes-pending", count = pending)).color(ui.visuals().warn_fg_color));
            } else {
                ui.label(RichText::new(tr!("all-rendered")).weak());
            }
            let render = egui::Button::new(tr!("render-button")).selected(pending > 0);
            if ui.add_enabled(pending > 0 && !self.busy(), render).on_hover_text(tr!("render-tooltip")).clicked() {
                self.start_render(&ctx);
            }
            ui.checkbox(&mut self.auto_render, tr!("auto-render")).on_hover_text(tr!("auto-render-tooltip"));
            ui.separator();
            let export = ui
                .add_enabled(pending == 0 && !self.busy(), egui::Button::new(tr!("export-button")))
                .on_hover_text(tr!("export-tooltip"))
                .on_disabled_hover_text(if pending > 0 { tr!("export-pending-tooltip") } else { tr!("export-tooltip") });
            if export.clicked() {
                self.start_export(&ctx, frame);
            }
        });
    }

    fn job_bar(&mut self, ui: &mut egui::Ui) {
        let Some(job) = &self.job else { return };
        let progress = match job {
            Running::Import(j) => j.progress(),
            Running::Render(j, _) => j.progress(),
            Running::Export(j, _) => j.progress(),
        };
        let text = match progress.stage {
            Stage::Starting => tr!("stage-starting"),
            Stage::Encoding { clip, clips } => tr!("stage-encoding", clip = clip + 1, clips = clips),
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
                Running::Import(j) => j.cancel(),
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
        let Some(p) = &mut self.project else { return };
        let sel = &self.timeline.selection.clone();
        let mut edited = false;
        ui.horizontal(|ui| {
            let any = !sel.is_empty();
            let deleted = p.cards.cards.iter().any(|c| c.deleted && sel.contains(&c.id));
            if ui.add_enabled(any, egui::Button::new(tr!("delete-button"))).on_hover_text(tr!("delete-tooltip")).clicked() {
                p.cards.set_deleted(sel, true);
                edited = true;
            }
            if ui.add_enabled(deleted, egui::Button::new(tr!("restore-button"))).on_hover_text(tr!("restore-tooltip")).clicked() {
                p.cards.set_deleted(sel, false);
                edited = true;
            }
            if ui.add_enabled(any, egui::Button::new(tr!("duplicate-button"))).on_hover_text(tr!("duplicate-tooltip")).clicked() {
                p.cards.add_copies(sel, 1);
                edited = true;
            }
            if ui.add_enabled(any, egui::Button::new(tr!("unduplicate-button"))).on_hover_text(tr!("unduplicate-tooltip")).clicked() {
                p.cards.add_copies(sel, -1);
                edited = true;
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
        if edited {
            self.edited(ui.ctx());
        }
    }

    fn timeline_strip(&mut self, ui: &mut egui::Ui) {
        let Some(p) = &mut self.project else { return };
        let (w, h) = p.size;
        let playhead = self.preview.as_ref().and_then(|v| v.rendered().cards.get(self.playhead).copied());
        let event = self.timeline.show(ui, &mut p.cards, &p.frames, &self.thumbs, h as f32 / w as f32, playhead);
        match event {
            Some(Event::Seek(card)) => {
                if let Some(k) = self.preview.as_ref().and_then(|v| v.rendered().frame_of(card)) {
                    self.playhead = k;
                }
            }
            Some(Event::Edited) => self.edited(ui.ctx()),
            None => {}
        }
    }

    fn preview_area(&mut self, ui: &mut egui::Ui) {
        let Some(preview) = &mut self.preview else {
            ui.centered_and_justified(|ui| ui.label(RichText::new(tr!("empty-hint")).heading().weak()));
            return;
        };
        let total = preview.rendered().len();
        let source = preview.rendered().report.frames.get(self.playhead).map(|f| f.src);
        let mut step: Option<i64> = None;
        ui.with_layout(Layout::bottom_up(Align::Center), |ui| {
            ui.horizontal(|ui| {
                if ui.button("⏮").on_hover_text(tr!("first-frame")).clicked() {
                    step = Some(i64::MIN);
                }
                if ui.button("◀").on_hover_text(tr!("previous-frame")).clicked() {
                    step = Some(-1);
                }
                ui.label(tr!("preview-position", frame = self.playhead, total = total));
                if ui.button("▶").on_hover_text(tr!("next-frame")).clicked() {
                    step = Some(1);
                }
                if ui.button("⏭").on_hover_text(tr!("last-frame")).clicked() {
                    step = Some(i64::MAX);
                }
                if let Some(s) = source {
                    ui.separator();
                    ui.label(RichText::new(tr!("preview-source", frame = s)).weak());
                }
            });
            let (texture, current) = preview.frame(ui.ctx(), self.playhead);
            let (rect, _) = ui.allocate_exact_size(ui.available_size(), Sense::hover());
            if let Some(t) = texture {
                let size = t.size_vec2();
                let scale = (rect.width() / size.x).min(rect.height() / size.y);
                let image = egui::Rect::from_center_size(rect.center(), size * scale);
                ui.painter().image(
                    t.id(),
                    image,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
            if !current && preview.error.is_none() {
                let spinner = egui::Rect::from_center_size(rect.center(), Vec2::splat(32.0));
                ui.put(spinner, egui::Spinner::new().size(32.0));
            }
        });
        if let Some(e) = preview.error.take() {
            self.error(tr!("preview-failed", reason = e));
        }
        if let Some(s) = step {
            self.step(s);
        }
    }

    fn step(&mut self, delta: i64) {
        let Some(preview) = &self.preview else { return };
        let last = preview.rendered().len().saturating_sub(1) as i64;
        self.playhead = (self.playhead as i64).saturating_add(delta).clamp(0, last) as usize;
        if let (Some(card), Some(p)) = (self.playhead_card(), &self.project)
            && let Some(i) = p.cards.index_of(card)
        {
            self.timeline.reveal(i);
        }
    }

    fn keyboard(&mut self, ctx: &egui::Context) {
        if ctx.memory(|m| m.focused().is_some()) {
            return;
        }
        let (delete, all, escape, left, right, home, end) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::Delete) || i.consume_key(Modifiers::NONE, Key::Backspace),
                i.consume_key(Modifiers::COMMAND, Key::A),
                i.consume_key(Modifiers::NONE, Key::Escape),
                i.consume_key(Modifiers::NONE, Key::ArrowLeft),
                i.consume_key(Modifiers::NONE, Key::ArrowRight),
                i.consume_key(Modifiers::NONE, Key::Home),
                i.consume_key(Modifiers::NONE, Key::End),
            )
        });
        if let Some(p) = &mut self.project {
            if delete && !self.timeline.selection.is_empty() {
                p.cards.set_deleted(&self.timeline.selection, true);
                self.last_edit = ctx.input(|i| i.time);
            }
            if all {
                self.timeline.select_all(&p.cards);
            }
            if escape {
                self.timeline.selection.clear();
            }
        }
        for (pressed, delta) in [(left, -1), (right, 1), (home, i64::MIN), (end, i64::MAX)] {
            if pressed {
                self.step(delta);
            }
        }
    }
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
            self.start_import(&ctx, dropped);
        }
        self.keyboard(&ctx);
        if self.auto_render && !self.busy() && self.project.as_ref().is_some_and(|p| p.cards.pending_count() > 0) {
            let wait = self.last_edit + AUTO_RENDER_DELAY - ctx.input(|i| i.time);
            if wait <= 0.0 {
                self.start_render(&ctx);
            } else {
                ctx.request_repaint_after(std::time::Duration::from_secs_f64(wait));
            }
        }

        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(4.0);
            self.header(ui, frame);
            self.job_bar(ui);
            self.notice_bar(ui);
            ui.add_space(2.0);
        });
        if self.project.is_some() {
            egui::Panel::bottom("timeline").resizable(true).default_size(150.0).show(ui, |ui| {
                ui.add_space(4.0);
                self.edit_tools(ui);
                self.timeline_strip(ui);
            });
        }
        egui::CentralPanel::default().show(ui, |ui| self.preview_area(ui));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::Harness;
    use std::process::Command;
    use std::time::Duration;

    /// A raw stream of `secs` seconds (25 fps, keyframe every 25 frames, CAVLC, no
    /// B-frames), or None without ffmpeg.
    fn test_clip(name: &str, secs: u32, size: &str) -> Option<PathBuf> {
        let dir = std::env::temp_dir().join(format!("h264mosh-gui-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let status = Command::new(Tools::find(None).ffmpeg)
            .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i"])
            .arg(format!("testsrc2=size={size}:rate=25"))
            .args(["-t", &secs.to_string(), "-c:v", "libx264", "-pix_fmt", "yuv420p", "-g", "25", "-bf", "0", "-refs", "1", "-coder", "0"])
            .args(["-x264-params", "weightp=0:scenecut=0", "-f", "h264"])
            .arg(&path)
            .status();
        if !status.is_ok_and(|s| s.success()) {
            eprintln!("ffmpeg not available, skipping");
            return None;
        }
        Some(path)
    }

    fn harness(gpu: bool) -> Harness<'static, App> {
        let builder = Harness::builder().with_size([1280.0, 800.0]);
        let builder = if gpu { builder.wgpu() } else { builder };
        builder.build_eframe(|cc| App::new(cc))
    }

    /// Step the UI until `done` holds.
    fn wait(h: &mut Harness<App>, done: impl Fn(&App) -> bool) {
        for _ in 0..1000 {
            h.step();
            if done(h.state()) {
                h.run_steps(2);
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out");
    }

    fn open(h: &mut Harness<App>, clip: PathBuf) {
        let ctx = h.ctx.clone();
        h.state_mut().start_import(&ctx, vec![clip]);
        wait(h, |a| a.job.is_none());
        assert!(h.state().notice.is_none(), "{}", h.state().notice.as_ref().unwrap().text);
    }

    fn render(h: &mut Harness<App>) {
        let ctx = h.ctx.clone();
        h.state_mut().start_render(&ctx);
        wait(h, |a| a.job.is_none());
    }

    #[test]
    fn import_edit_render() {
        let Some(clip) = test_clip("edit.264", 3, "320x180") else { return };
        let mut h = harness(false);
        open(&mut h, clip.clone());
        let p = h.state_mut().project.as_mut().unwrap();
        assert_eq!(p.cards.len(), 75);
        p.cards.set_deleted(&(30..35).collect(), true);
        p.cards.add_copies(&HashSet::from([40]), 2);
        p.cards.move_to(&HashSet::from([60]), 10);
        assert_eq!(p.cards.pending_count(), 7);
        render(&mut h);
        let app = h.state();
        let p = app.project.as_ref().unwrap();
        assert_eq!((p.cards.len(), p.cards.pending_count()), (70, 0));
        let rendered = app.preview.as_ref().unwrap().rendered();
        assert_eq!(rendered.len(), 72);
        assert_eq!(rendered.report.frames[10].src, 60);

        // export: a raw stream as it is, anything else encoded by ffmpeg
        let rendered = rendered.clone();
        for name in ["export.264", "export.mp4"] {
            let path = clip.with_file_name(name);
            let (tools, job_input, out) = (Tools::find(None), rendered.clone(), path.clone());
            let job = Job::spawn(&h.ctx, move |r| project::export(&tools, &job_input, "25", &out, r));
            let result = loop {
                match job.poll() {
                    Some(r) => break r,
                    None => std::thread::sleep(Duration::from_millis(10)),
                }
            };
            result.unwrap();
            let data = h264mosh::pipeline::load_annexb(&Tools::find(None), &path).unwrap();
            if name.ends_with(".264") {
                assert!(data == rendered.bytes);
            }
            let frames = h264mosh::mosh::Source::new(h264mosh::stream::Stream::parse(&data).unwrap()).len();
            assert_eq!(frames, 72, "{name}");
        }

        // the output has to start with a keyframe
        h.state_mut().project.as_mut().unwrap().cards.set_deleted(&HashSet::from([0]), true);
        render(&mut h);
        assert!(h.state().notice.as_ref().is_some_and(|n| n.error));
        assert_eq!(h.state().preview.as_ref().unwrap().rendered().len(), 72);
    }

    /// Click the middle of card `i`.
    fn click(h: &mut Harness<App>, i: usize, modifiers: Modifiers) {
        let pos = card(h, i).center();
        h.event(egui::Event::ModifiersChanged(modifiers));
        h.event(egui::Event::PointerMoved(pos));
        let button = |pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers };
        h.event(button(true));
        h.event(button(false));
        h.event(egui::Event::ModifiersChanged(Modifiers::NONE));
        h.run_steps(2);
    }

    fn card(h: &Harness<App>, i: usize) -> egui::Rect {
        h.state().timeline.card_rect(i)
    }

    fn order(h: &Harness<App>) -> Vec<usize> {
        h.state().project.as_ref().unwrap().cards.cards.iter().map(|c| c.src).collect()
    }

    /// Click, Shift-click, dragging and the wheel over a copy badge.
    #[test]
    fn timeline_mouse_editing() {
        let Some(clip) = test_clip("mouse.264", 2, "320x180") else { return };
        let mut h = harness(false);
        open(&mut h, clip);
        h.state_mut().timeline.zoom = 60.0;
        h.run_steps(2);

        click(&mut h, 2, Modifiers::NONE);
        click(&mut h, 4, Modifiers::SHIFT);
        let mut selected: Vec<CardId> = h.state().timeline.selection.iter().copied().collect();
        selected.sort();
        assert_eq!(selected, [2, 3, 4]);

        // drag the selection between cards 7 and 8
        let (from, to) = (card(&h, 3).center(), card(&h, 8).left_center() - egui::vec2(1.0, 0.0));
        h.drag_at(from);
        for k in 1..=10 {
            h.hover_at(from + (to - from) * k as f32 / 10.0);
            h.step();
        }
        h.drop_at(to);
        h.run_steps(2);
        assert_eq!(order(&h)[..10], [0, 1, 5, 6, 7, 2, 3, 4, 8, 9]);
        assert_eq!(h.state().project.as_ref().unwrap().cards.pending_count(), 3);

        // scrolling up over the copy badge of card 0 adds copies
        let badge = card(&h, 0).right_top() + egui::vec2(-8.0, 7.0);
        h.hover_at(badge);
        h.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, 120.0),
            phase: egui::TouchPhase::Move,
            modifiers: Modifiers::NONE,
        });
        h.run_steps(20);
        assert!(h.state().project.as_ref().unwrap().cards.cards[0].copies > 1);
        assert_eq!(order(&h)[0], 0, "the strip did not scroll");

        // a deleted card's restore button brings it back
        h.state_mut().project.as_mut().unwrap().cards.set_deleted(&HashSet::from([9]), true);
        h.run_steps(2);
        click(&mut h, 9, Modifiers::NONE);
        assert!(!h.state().project.as_ref().unwrap().cards.cards[9].deleted);
    }

    /// Writes screenshots to target/ui-shots for looking at the UI:
    /// `cargo test -p h264mosh-gui screenshots -- --ignored`.
    /// $H264MOSH_SHOT_INPUT may name a raw stream to use instead of a test pattern.
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
        h.state_mut().start_import(&ctx, vec![clip]);
        // catch the progress bar of clips that need encoding
        for _ in 0..3000 {
            h.step();
            let encoding = |a: &App| match &a.job {
                Some(Running::Import(j)) => matches!(j.progress().stage, Stage::Encoding { .. }) && j.progress().done > 20,
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

        let p = h.state_mut().project.as_mut().unwrap();
        p.cards.set_deleted(&(3..5).collect(), true);
        p.cards.add_copies(&HashSet::from([6]), 2);
        p.cards.move_to(&HashSet::from([1]), 9);
        h.state_mut().timeline.selection = HashSet::from([8, 9]);
        shot(&mut h, "3_edited");

        render(&mut h);
        h.state_mut().playhead = 5;
        wait(&mut h, |a| a.preview.as_ref().is_some_and(|p| p.error.is_none()) && a.job.is_none());
        std::thread::sleep(Duration::from_millis(500));
        shot(&mut h, "4_rendered");

        h.state_mut().timeline.zoom = 6.0;
        shot(&mut h, "5_zoomed_out");
    }
}
