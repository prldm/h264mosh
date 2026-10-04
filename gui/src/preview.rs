//! The preview: output frames of the last render, decoded in the background at a
//! reduced size by ffmpeg (the decoder of the export too) and cached around the
//! playhead.
//!
//! A decode run starts at the last IDR frame at or before the wanted frame and keeps
//! going while the playhead follows. When it gets too far ahead it waits, and ffmpeg
//! waits on its full pipe.

use crate::project::Rendered;
use eframe::egui;
use h264mosh::mosh::segment;
use h264mosh::pipeline::{self, Tools};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// a run decodes at most this many frames ahead of the playhead
const MAX_AHEAD: usize = 240;

struct Cache {
    frames: HashMap<usize, Arc<Vec<u8>>>,
    /// the output frame the UI wants
    wanted: usize,
    capacity: usize,
}

impl Cache {
    fn insert(&mut self, k: usize, pixels: Vec<u8>) {
        self.frames.insert(k, Arc::new(pixels));
        while self.frames.len() > self.capacity {
            let wanted = self.wanted;
            let far = *self.frames.keys().max_by_key(|&&f| f.abs_diff(wanted)).unwrap();
            self.frames.remove(&far);
        }
    }
}

type Shared = Arc<(Mutex<Cache>, Condvar)>;

struct Run {
    start: usize,
    /// the next output frame it delivers
    next: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    /// finished, with the error if it failed
    done: Arc<Mutex<Option<Result<(), String>>>>,
}

pub struct Preview {
    tools: Tools,
    rendered: Arc<Rendered>,
    size: (u32, u32),
    shared: Shared,
    run: Option<Run>,
    texture: Option<egui::TextureHandle>,
    shown: Option<usize>,
    /// a frame the decoder did not deliver; not retried
    unavailable: Option<usize>,
    pub error: Option<String>,
}

impl Preview {
    /// Frames of `rendered` at `size`, keeping up to `cache_bytes` of decoded frames.
    pub fn new(tools: Tools, rendered: Arc<Rendered>, size: (u32, u32), cache_bytes: usize) -> Self {
        let frame_bytes = size.0 as usize * size.1 as usize * 4;
        let capacity = (cache_bytes / frame_bytes).max(8);
        let cache = Cache { frames: HashMap::new(), wanted: 0, capacity };
        Self {
            tools,
            rendered,
            size,
            shared: Arc::new((Mutex::new(cache), Condvar::new())),
            run: None,
            texture: None,
            shown: None,
            unavailable: None,
            error: None,
        }
    }

    pub fn rendered(&self) -> &Arc<Rendered> {
        &self.rendered
    }

    /// Whether output frame `k` is the one on screen.
    pub fn shows(&self, k: usize) -> bool {
        self.shown == Some(k)
    }

    /// The texture to draw for output frame `k`, and whether it shows `k` already
    /// (otherwise it is the previous frame, or None, while `k` is being decoded).
    pub fn frame(&mut self, ctx: &egui::Context, k: usize) -> (Option<&egui::TextureHandle>, bool) {
        let k = k.min(self.rendered.len().saturating_sub(1));
        let pixels = {
            let (lock, wake) = &*self.shared;
            let mut cache = lock.lock().unwrap();
            if cache.wanted != k {
                cache.wanted = k;
                wake.notify_all();
            }
            if self.shown == Some(k) { None } else { cache.frames.get(&k).cloned() }
        };
        if let Some(px) = pixels {
            let image = egui::ColorImage::from_rgba_unmultiplied([self.size.0 as usize, self.size.1 as usize], &px);
            match &mut self.texture {
                Some(t) => t.set(image, egui::TextureOptions::LINEAR),
                None => self.texture = Some(ctx.load_texture("preview", image, egui::TextureOptions::LINEAR)),
            }
            self.shown = Some(k);
        }
        if self.shown != Some(k) {
            self.ensure_run(ctx, k);
        }
        (self.texture.as_ref(), self.shown == Some(k))
    }

    /// Start a decode run towards `k` unless the current one gets there first.
    fn ensure_run(&mut self, ctx: &egui::Context, k: usize) {
        if self.unavailable == Some(k) {
            return;
        }
        let restart = self.rendered.report.restart_point(k);
        if let Some(run) = &self.run {
            let next = run.next.load(Ordering::Acquire);
            let ahead_of_k = run.start <= k && next <= k;
            match &*run.done.lock().unwrap() {
                Some(Err(e)) => {
                    self.error = Some(e.clone());
                    self.unavailable = Some(k);
                    return;
                }
                // ended before reaching k: the decoder dropped frames
                Some(Ok(())) if ahead_of_k => {
                    self.unavailable = Some(k);
                    return;
                }
                None if ahead_of_k && restart <= next => return,
                _ => {}
            }
        }
        self.start_run(ctx, restart);
    }

    fn start_run(&mut self, ctx: &egui::Context, start: usize) {
        self.stop_run();
        let run = Run {
            start,
            next: Arc::new(AtomicUsize::new(start)),
            stop: Arc::new(AtomicBool::new(false)),
            done: Arc::new(Mutex::new(None)),
        };
        let (tools, rendered, shared, size, ctx) = (self.tools.clone(), self.rendered.clone(), self.shared.clone(), self.size, ctx.clone());
        let (next, stop, done) = (run.next.clone(), run.stop.clone(), run.done.clone());
        std::thread::spawn(move || {
            let ahead = (shared.0.lock().unwrap().capacity / 2).min(MAX_AHEAD);
            let data = if start == 0 {
                Ok(Cow::Borrowed(&rendered.bytes[..]))
            } else {
                segment(&rendered.stream, &rendered.report.frames, start..rendered.len()).map(Cow::Owned)
            };
            let result = data.and_then(|data| {
                pipeline::decode_rgba(&tools, &data, size, |i, pixels| {
                    let k = start + i;
                    let (lock, wake) = &*shared;
                    let mut cache = lock.lock().unwrap();
                    cache.insert(k, pixels);
                    next.store(k + 1, Ordering::Release);
                    if k == cache.wanted {
                        ctx.request_repaint();
                    }
                    while !stop.load(Ordering::Relaxed) && k >= cache.wanted + ahead {
                        cache = wake.wait(cache).unwrap();
                    }
                    !stop.load(Ordering::Relaxed)
                })
            });
            *done.lock().unwrap() = Some(result.map(|_| ()).map_err(|e| format!("{e:#}")));
            ctx.request_repaint();
        });
        self.run = Some(run);
    }

    fn stop_run(&mut self) {
        if let Some(run) = self.run.take() {
            run.stop.store(true, Ordering::Relaxed);
            self.shared.1.notify_all();
        }
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        self.stop_run();
    }
}
