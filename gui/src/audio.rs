//! Sound while playing: the clips' audio (the first track of each, joined, as
//! `pipeline::decode_audio` gives it), decoded in the background and played on the
//! default output device at the time of the playhead, at normal speed forwards only.
//! It keeps the output's time, as an exported video keeps the clips' tracks, whatever
//! frames were cut, repeated or moved.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use h264mosh::pipeline::{self, Tools};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// how far the sound may drift from the picture before it jumps to it
const MAX_DRIFT: f64 = 0.1;

#[derive(Default)]
struct Shared {
    /// interleaved stereo, once decoded
    samples: OnceLock<Vec<i16>>,
    /// the next sample frame to play
    position: AtomicU64,
    playing: AtomicBool,
}

impl Shared {
    /// Fill an output buffer of `channels` channels: stereo on the first two (mono
    /// devices get the mix), silence elsewhere and while not playing.
    fn fill<T: SizedSample + FromSample<f32>>(&self, data: &mut [T], channels: usize) {
        let playing = self.playing.load(Ordering::Relaxed);
        let start = self.position.load(Ordering::Relaxed);
        let samples = self.samples.get().map_or(&[][..], Vec::as_slice);
        let mut pos = start as usize;
        for frame in data.chunks_mut(channels) {
            let (l, r) = match samples.get(2 * pos..2 * pos + 2) {
                Some(&[l, r]) if playing => (l as f32 / 32768.0, r as f32 / 32768.0),
                _ => (0.0, 0.0),
            };
            if playing {
                pos += 1;
            }
            for (c, out) in frame.iter_mut().enumerate() {
                *out = T::from_sample(match (channels, c) {
                    (1, _) => (l + r) / 2.0,
                    (_, 0) => l,
                    (_, 1) => r,
                    _ => 0.0,
                });
            }
        }
        // unless the playhead moved it meanwhile
        let _ = self.position.compare_exchange(start, pos as u64, Ordering::Relaxed, Ordering::Relaxed);
    }
}

pub struct Audio {
    shared: Arc<Shared>,
    sample_rate: u32,
    /// the output stream; dropping it stops the sound
    _stream: cpal::Stream,
    /// why the clips' audio could not be decoded
    error: Arc<Mutex<Option<String>>>,
}

impl Audio {
    /// Sound for `clips` (with the frames each gives at `fps`) on the default output
    /// device, decoded in the background; None without a device.
    pub fn new(tools: Tools, clips: Vec<(PathBuf, usize)>, fps: String) -> Option<Self> {
        let device = cpal::default_host().default_output_device()?;
        let config = device.default_output_config().ok()?;
        let (sample_rate, channels) = (config.sample_rate(), config.channels() as usize);
        let shared = Arc::new(Shared::default());
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => output::<f32>(&device, config.config(), channels, shared.clone()),
            cpal::SampleFormat::I16 => output::<i16>(&device, config.config(), channels, shared.clone()),
            cpal::SampleFormat::U16 => output::<u16>(&device, config.config(), channels, shared.clone()),
            cpal::SampleFormat::I32 => output::<i32>(&device, config.config(), channels, shared.clone()),
            _ => return None,
        }?;
        stream.play().ok()?;
        let error = Arc::new(Mutex::new(None));
        let (decoded, failed) = (shared.clone(), error.clone());
        std::thread::spawn(move || match pipeline::decode_audio(&tools, &clips, &fps, sample_rate) {
            Ok(samples) => {
                let _ = decoded.samples.set(samples);
            }
            Err(e) => *failed.lock().unwrap() = Some(format!("{e:#}")),
        });
        Some(Self { shared, sample_rate, _stream: stream, error })
    }

    /// Play from `seconds` on, unless it is playing about there already; or be silent.
    pub fn follow(&self, on: bool, seconds: f64) {
        let s = &self.shared;
        if !on {
            s.playing.store(false, Ordering::Relaxed);
            return;
        }
        let rate = self.sample_rate as f64;
        let at = s.position.load(Ordering::Relaxed) as f64 / rate;
        if !s.playing.load(Ordering::Relaxed) || (at - seconds).abs() > MAX_DRIFT {
            s.position.store((seconds.max(0.0) * rate) as u64, Ordering::Relaxed);
        }
        s.playing.store(true, Ordering::Relaxed);
    }

    /// Why there is no sound, once.
    pub fn take_error(&self) -> Option<String> {
        self.error.lock().unwrap().take()
    }
}

fn output<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    channels: usize,
    shared: Arc<Shared>,
) -> Option<cpal::Stream> {
    device.build_output_stream(config, move |data: &mut [T], _| shared.fill(data, channels), |_| {}, None).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_stereo_from_the_position_while_playing() {
        let shared = Shared::default();
        shared.samples.set(vec![0, 0, 16384, -16384, 32767, 0]).unwrap();
        let mut out = [1.0f32; 6];
        shared.fill(&mut out, 2);
        assert_eq!(out, [0.0; 6], "silent while stopped");
        shared.position.store(1, Ordering::Relaxed);
        shared.playing.store(true, Ordering::Relaxed);
        let mut out = [1.0f32; 9];
        shared.fill(&mut out, 3);
        assert_eq!(out[..3], [0.5, -0.5, 0.0]);
        assert!((out[3] - 1.0).abs() < 1e-4 && out[4] == 0.0);
        assert_eq!(out[6..], [0.0; 3], "silence after the end");
        assert_eq!(shared.position.load(Ordering::Relaxed), 4);
        let mut mono = [1.0f32; 1];
        shared.position.store(1, Ordering::Relaxed);
        shared.fill(&mut mono, 1);
        assert_eq!(mono, [0.0], "mono mixes both channels");
    }
}
