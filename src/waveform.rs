//! Waveform peak extraction (background) and per-pixel reduction (UI).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::audio::decoder::{Decoder, OutputSpec, Read};

/// Frames summarised by one stored peak bucket.
pub const FRAMES_PER_BUCKET: usize = 256;
/// Channels beyond this are not drawn.
pub const MAX_CHANNELS: usize = 8;

#[derive(Default)]
pub struct Peaks {
    pub channels: usize,
    pub rate: u32,
    /// Container-reported duration; the drawing width maps to this.
    pub duration: f64,
    /// Per channel, per bucket (min, max).
    pub buckets: Vec<Vec<(f32, f32)>>,
    pub done: bool,
    pub error: Option<String>,
}

impl Peaks {
    pub fn bucket_secs(&self) -> f64 {
        if self.rate == 0 {
            0.0
        } else {
            FRAMES_PER_BUCKET as f64 / f64::from(self.rate)
        }
    }
}

/// A running (or finished) peak computation for one file.
pub struct PeakJob {
    pub peaks: Arc<Mutex<Peaks>>,
    cancel: Arc<AtomicBool>,
}

impl PeakJob {
    pub fn start(path: PathBuf, wake: impl Fn() + Send + 'static) -> Self {
        let peaks = Arc::new(Mutex::new(Peaks::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let (p, c, file) = (peaks.clone(), cancel.clone(), path);
        thread::Builder::new()
            .name("peaks".into())
            .spawn(move || {
                if let Err(e) = compute(&file, &p, &c, &wake) {
                    p.lock().unwrap().error = Some(format!("{e:#}"));
                }
                p.lock().unwrap().done = true;
                wake();
            })
            .expect("spawning peak worker");
        Self { peaks, cancel }
    }
}

impl Drop for PeakJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

fn compute(path: &std::path::Path, out: &Mutex<Peaks>, cancel: &AtomicBool, wake: &dyn Fn()) -> anyhow::Result<()> {
    let mut decoder = Decoder::open(path, OutputSpec::Native)?;
    let info = decoder.info();
    let channels = info.channels.min(MAX_CHANNELS);
    {
        let mut p = out.lock().unwrap();
        p.channels = channels;
        p.rate = info.rate;
        p.duration = info.duration;
        p.buckets = vec![Vec::new(); channels];
    }

    let mut acc = BucketAccumulator::new(info.channels, channels);
    let mut samples = Vec::with_capacity(64 * 1024);
    let mut batch: Vec<Vec<(f32, f32)>> = vec![Vec::new(); channels];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        samples.clear();
        let eof = matches!(decoder.read(&mut samples)?, Read::Eof);
        acc.push(&samples, &mut batch);
        if eof {
            acc.finish(&mut batch);
        }
        // Publish in batches so the UI fills in progressively without lock churn.
        if eof || batch[0].len() >= 512 {
            let mut p = out.lock().unwrap();
            for (dst, src) in p.buckets.iter_mut().zip(batch.iter_mut()) {
                dst.append(src);
            }
            drop(p);
            wake();
        }
        if eof {
            return Ok(());
        }
    }
}

/// Folds interleaved samples into fixed-size min/max buckets.
struct BucketAccumulator {
    stride: usize,
    channels: usize,
    filled: usize,
    current: Vec<(f32, f32)>,
}

impl BucketAccumulator {
    fn new(stride: usize, channels: usize) -> Self {
        Self {
            stride,
            channels,
            filled: 0,
            current: vec![(f32::MAX, f32::MIN); channels],
        }
    }

    fn push(&mut self, interleaved: &[f32], out: &mut [Vec<(f32, f32)>]) {
        for frame in interleaved.chunks_exact(self.stride) {
            for (c, &s) in frame.iter().take(self.channels).enumerate() {
                let b = &mut self.current[c];
                b.0 = b.0.min(s);
                b.1 = b.1.max(s);
            }
            self.filled += 1;
            if self.filled == FRAMES_PER_BUCKET {
                self.emit(out);
            }
        }
    }

    fn finish(&mut self, out: &mut [Vec<(f32, f32)>]) {
        if self.filled > 0 {
            self.emit(out);
        }
    }

    fn emit(&mut self, out: &mut [Vec<(f32, f32)>]) {
        for (c, b) in self.current.iter_mut().enumerate() {
            out[c].push(*b);
            *b = (f32::MAX, f32::MIN);
        }
        self.filled = 0;
    }
}

/// Reduces one channel's buckets to `width` columns spanning `total_buckets`
/// (which may exceed what has been computed so far). Columns past the
/// computed data are `None`.
pub fn columns(buckets: &[(f32, f32)], total_buckets: usize, width: usize) -> Vec<Option<(f32, f32)>> {
    let total = total_buckets.max(buckets.len()).max(1);
    (0..width)
        .map(|x| {
            let start = x * total / width;
            let end = ((x + 1) * total / width).max(start + 1);
            let slice = buckets.get(start..end.min(buckets.len()))?;
            slice.iter().copied().reduce(|a, b| (a.0.min(b.0), a.1.max(b.1)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulator_buckets_per_channel() {
        let mut acc = BucketAccumulator::new(2, 2);
        let mut out = vec![Vec::new(), Vec::new()];
        // One full bucket: left ramps 0..1, right is constant -0.5.
        let frames: Vec<f32> = (0..FRAMES_PER_BUCKET)
            .flat_map(|i| [i as f32 / (FRAMES_PER_BUCKET - 1) as f32, -0.5])
            .collect();
        acc.push(&frames, &mut out);
        assert_eq!(out[0], vec![(0.0, 1.0)]);
        assert_eq!(out[1], vec![(-0.5, -0.5)]);
        // A partial bucket is only emitted on finish.
        acc.push(&[0.25, 0.75], &mut out);
        assert_eq!(out[0].len(), 1);
        acc.finish(&mut out);
        assert_eq!(out[0][1], (0.25, 0.25));
        assert_eq!(out[1][1], (0.75, 0.75));
    }

    #[test]
    fn accumulator_ignores_channels_beyond_limit() {
        let mut acc = BucketAccumulator::new(3, 1);
        let mut out = vec![Vec::new()];
        acc.push(&[0.1, 9.0, 9.0], &mut out);
        acc.finish(&mut out);
        assert_eq!(out[0], vec![(0.1, 0.1)]);
    }

    #[test]
    fn columns_reduce_min_max() {
        let b = [(-0.1, 0.1), (-0.5, 0.2), (-0.2, 0.9), (0.0, 0.0)];
        assert_eq!(columns(&b, 4, 2), vec![Some((-0.5, 0.2)), Some((-0.2, 0.9))]);
    }

    #[test]
    fn columns_beyond_computed_data_are_empty() {
        let b = [(-0.1, 0.1), (-0.2, 0.2)];
        let cols = columns(&b, 8, 4);
        assert_eq!(cols[0], Some((-0.2, 0.2)));
        assert_eq!(cols[1..], [None, None, None]);
    }

    #[test]
    fn columns_wider_than_data_repeat_buckets() {
        let b = [(-1.0, 1.0)];
        assert_eq!(columns(&b, 1, 3), vec![Some((-1.0, 1.0)); 3]);
    }
}
