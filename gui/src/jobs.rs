//! Background work. A job runs on its own thread, reports progress and can be
//! cancelled; the UI polls it every frame. Dropping a job cancels it.

use anyhow::{Result, anyhow};
use eframe::egui;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

/// What a job is doing, for its progress bar.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Stage {
    Starting,
    /// encoding clip `clip` (0-based) of `clips` with mosh-friendly settings
    Encoding {
        clip: usize,
        clips: usize,
    },
    /// copying the H.264 stream of clip `clip` out of its file
    Copying {
        clip: usize,
        clips: usize,
    },
    Reading,
    Rendering,
    Exporting,
}

#[derive(Clone, Copy, Debug)]
pub struct Progress {
    pub stage: Stage,
    pub done: u64,
    pub total: Option<u64>,
}

impl Progress {
    pub fn fraction(&self) -> Option<f32> {
        self.total.filter(|&t| t > 0).map(|t| (self.done as f32 / t as f32).min(1.0))
    }
}

/// The job's side: report progress, notice cancellation.
pub struct Reporter {
    ctx: egui::Context,
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<Progress>>,
}

impl Reporter {
    /// Report progress; returns false once the job is cancelled.
    pub fn set(&self, stage: Stage, done: u64, total: Option<u64>) -> bool {
        *self.progress.lock().unwrap() = Progress { stage, done, total };
        self.ctx.request_repaint();
        !self.cancelled()
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

pub struct Job<T> {
    result: mpsc::Receiver<Result<T>>,
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<Progress>>,
}

impl<T: Send + 'static> Job<T> {
    pub fn spawn(ctx: &egui::Context, work: impl FnOnce(&Reporter) -> Result<T> + Send + 'static) -> Self {
        let (tx, result) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(Progress { stage: Stage::Starting, done: 0, total: None }));
        let reporter = Reporter { ctx: ctx.clone(), cancel: cancel.clone(), progress: progress.clone() };
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| work(&reporter))).unwrap_or_else(|panic| {
                let msg = panic.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| panic.downcast_ref::<String>().cloned());
                Err(anyhow!("internal error: {}", msg.unwrap_or_else(|| "panic".into())))
            });
            let _ = tx.send(result);
            reporter.ctx.request_repaint();
        });
        Self { result, cancel, progress }
    }

    /// The result, once the job is done.
    pub fn poll(&self) -> Option<Result<T>> {
        match self.result.try_recv() {
            Ok(r) => Some(r),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err(anyhow!("internal error: the job ended without a result"))),
        }
    }

    pub fn progress(&self) -> Progress {
        *self.progress.lock().unwrap()
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl<T> Drop for Job<T> {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// A thread producing items for the UI (e.g. thumbnails), stopped when dropped.
pub struct Feed<T> {
    items: mpsc::Receiver<T>,
    stop: Arc<AtomicBool>,
}

impl<T: Send + 'static> Feed<T> {
    /// `work` gets a send function that returns false once the feed was dropped.
    pub fn spawn(ctx: &egui::Context, work: impl FnOnce(&mut dyn FnMut(T) -> bool) + Send + 'static) -> Self {
        let (tx, items) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let (ctx, stopped) = (ctx.clone(), stop.clone());
        std::thread::spawn(move || {
            work(&mut |item| {
                let ok = !stopped.load(Ordering::Relaxed) && tx.send(item).is_ok();
                ctx.request_repaint();
                ok
            })
        });
        Self { items, stop }
    }

    /// Items produced since the last call.
    pub fn drain(&self) -> impl Iterator<Item = T> + '_ {
        self.items.try_iter()
    }
}

impl<T> Drop for Feed<T> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
