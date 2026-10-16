//! Spectrogram computation (background) and per-pixel reduction (UI).

use std::ops::Range;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

use crate::audio::decoder::{Decoder, OutputSpec, Read};
use crate::waveform::MAX_CHANNELS;

/// Frequency bands per column, log-spaced from `MIN_HZ` up to Nyquist.
pub const BANDS: usize = 256;
/// Bottom edge of the lowest band.
pub const MIN_HZ: f64 = 30.0;
/// Level 0. Level 255 is a full-scale sine (0 dBFS).
pub const FLOOR_DB: f32 = -120.0;
/// The whole file fits in this many columns; the view always shows all of it.
const MAX_COLUMNS: u64 = 8192;
/// Narrowest column, reached by files shorter than about 6 s.
const MIN_COLUMN_FRAMES: usize = 32;
/// Columns computed between publishes.
const PUBLISH_COLUMNS: usize = 128;

#[derive(Default)]
pub struct Spectrogram {
    pub rate: u32,
    /// Container-reported duration; the drawing width maps to this.
    pub duration: f64,
    /// Frames summarised by one column.
    pub column_frames: usize,
    /// `BANDS` levels per column, lowest band first: 0 is `FLOOR_DB` or
    /// quieter, 255 is 0 dBFS or louder.
    pub levels: Vec<u8>,
    pub done: bool,
    pub error: Option<String>,
}

impl Spectrogram {
    pub fn column_secs(&self) -> f64 {
        if self.rate == 0 {
            0.0
        } else {
            self.column_frames as f64 / f64::from(self.rate)
        }
    }

    pub fn columns(&self) -> usize {
        self.levels.len() / BANDS
    }
}

/// Where `hz` sits on the band axis: 0 at `MIN_HZ`, 1 at Nyquist.
pub fn band_position(hz: f64, rate: u32) -> f64 {
    (hz / MIN_HZ).ln() / (f64::from(rate) / 2.0 / MIN_HZ).ln()
}

/// The frequency at `position` on the band axis (the inverse of `band_position`).
pub fn band_frequency(position: f64, rate: u32) -> f64 {
    MIN_HZ * (f64::from(rate) / 2.0 / MIN_HZ).powf(position)
}

/// A running (or finished) spectrogram computation for one file.
pub struct SpectrogramJob {
    pub spectrogram: Arc<Mutex<Spectrogram>>,
    cancel: Arc<AtomicBool>,
}

impl SpectrogramJob {
    pub fn start(path: PathBuf, wake: impl Fn() + Send + 'static) -> Self {
        let spectrogram = Arc::new(Mutex::new(Spectrogram::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let (s, c, file) = (spectrogram.clone(), cancel.clone(), path);
        thread::Builder::new()
            .name("spectrogram".into())
            .spawn(move || {
                if let Err(e) = compute(&file, &s, &c, &wake) {
                    s.lock().unwrap().error = Some(format!("{e:#}"));
                }
                s.lock().unwrap().done = true;
                wake();
            })
            .expect("spawning spectrogram worker");
        Self { spectrogram, cancel }
    }
}

impl Drop for SpectrogramJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

fn compute(
    path: &std::path::Path,
    out: &Mutex<Spectrogram>,
    cancel: &AtomicBool,
    wake: &dyn Fn(),
) -> anyhow::Result<()> {
    let mut decoder = Decoder::open(path, OutputSpec::Native)?;
    let info = decoder.info();
    let frames = (info.duration * f64::from(info.rate)).round() as u64;
    let mut analyzer = Analyzer::new(info.rate, info.channels, frames);
    {
        let mut s = out.lock().unwrap();
        s.rate = info.rate;
        s.duration = info.duration;
        s.column_frames = analyzer.column_frames();
    }

    let mut samples = Vec::with_capacity(64 * 1024);
    let mut batch = Vec::new();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        samples.clear();
        let eof = matches!(decoder.read(&mut samples)?, Read::Eof);
        analyzer.push(&samples, &mut batch);
        if eof {
            analyzer.finish(&mut batch);
        }
        // Publish in batches so the UI fills in progressively without lock churn.
        if eof || batch.len() >= PUBLISH_COLUMNS * BANDS {
            out.lock().unwrap().levels.append(&mut batch);
            wake();
        }
        if eof {
            return Ok(());
        }
    }
}

/// How a band's power is read from the FFT bins.
#[derive(Debug, PartialEq)]
enum Band {
    /// Narrower than a bin: interpolate between bin `k` and `k + 1`.
    Between(usize, f32),
    /// Spans whole bins: take the strongest, so a tone keeps its level.
    Bins(Range<usize>),
}

impl Band {
    fn power(&self, bins: &[f32]) -> f32 {
        match self {
            Band::Between(k, t) => bins[*k] * (1.0 - t) + bins[k + 1] * t,
            Band::Bins(range) => bins[range.clone()].iter().copied().fold(0.0, f32::max),
        }
    }
}

/// Short-time Fourier analysis of interleaved samples into columns of band
/// levels. Each column averages the power of `windows_per_column` Hann
/// windows, spread evenly across the column, and of every analysed channel.
struct Analyzer {
    stride: usize,
    channels: usize,
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    bands: Vec<Band>,
    /// Frames between successive windows.
    hop: usize,
    windows_per_column: usize,
    /// Bin power of a full-scale sine.
    reference: f32,
    /// Per channel: the not yet analysed samples, starting at the next window.
    input: Vec<Vec<f32>>,
    /// Frames received.
    frames: u64,
    /// Windows analysed.
    windows: u64,
    buffer: Vec<Complex32>,
    scratch: Vec<Complex32>,
    /// Bin power summed over the current column's windows and channels.
    power: Vec<f32>,
    /// Windows summed into `power`.
    pending: usize,
}

impl Analyzer {
    /// `frames` is the expected length, or 0 when unknown.
    fn new(rate: u32, stride: usize, frames: u64) -> Self {
        let channels = stride.min(MAX_CHANNELS);
        let size = fft_size(rate, frames);
        let window: Vec<f32> = (0..size)
            .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / size as f32).cos())
            .collect();
        let peak = window.iter().sum::<f32>() / 2.0;
        let (hop, windows_per_column) = hops(size, frames);
        let fft = FftPlanner::new().plan_fft_forward(size);
        let scratch = vec![Complex32::default(); fft.get_inplace_scratch_len()];
        // Pad the start so window k is centred in the k-th hop of the file.
        let lead = size / 2 - hop / 2;
        Self {
            stride,
            channels,
            fft,
            window,
            bands: bands(rate, size),
            hop,
            windows_per_column,
            reference: peak * peak,
            input: vec![vec![0.0; lead]; channels],
            frames: 0,
            windows: 0,
            buffer: vec![Complex32::default(); size],
            scratch,
            power: vec![0.0; size / 2 + 1],
            pending: 0,
        }
    }

    fn column_frames(&self) -> usize {
        self.hop * self.windows_per_column
    }

    fn push(&mut self, interleaved: &[f32], out: &mut Vec<u8>) {
        for frame in interleaved.chunks_exact(self.stride) {
            for (input, &s) in self.input.iter_mut().zip(frame) {
                input.push(s);
            }
            self.frames += 1;
        }
        self.analyse(out);
    }

    fn finish(&mut self, out: &mut Vec<u8>) {
        // Silence after the end lets the last windows run past it.
        let size = self.window.len();
        for input in &mut self.input {
            input.resize(input.len() + size, 0.0);
        }
        self.analyse(out);
        if self.pending > 0 {
            self.emit(out);
        }
    }

    fn analyse(&mut self, out: &mut Vec<u8>) {
        let size = self.window.len();
        let mut start = 0;
        while start + size <= self.input[0].len() && self.windows * (self.hop as u64) < self.frames {
            for input in &self.input {
                for ((dst, &s), &w) in self
                    .buffer
                    .iter_mut()
                    .zip(&input[start..start + size])
                    .zip(&self.window)
                {
                    *dst = Complex32::new(s * w, 0.0);
                }
                self.fft.process_with_scratch(&mut self.buffer, &mut self.scratch);
                for (p, x) in self.power.iter_mut().zip(&self.buffer) {
                    *p += x.norm_sqr();
                }
            }
            start += self.hop;
            self.windows += 1;
            self.pending += 1;
            if self.pending == self.windows_per_column {
                self.emit(out);
            }
        }
        for input in &mut self.input {
            input.drain(..start);
        }
    }

    fn emit(&mut self, out: &mut Vec<u8>) {
        let scale = 1.0 / (self.reference * (self.pending * self.channels) as f32);
        out.extend(self.bands.iter().map(|band| level(band.power(&self.power) * scale)));
        self.power.fill(0.0);
        self.pending = 0;
    }
}

/// About 40 ms of audio rounded up to a power of two (2048 at 44.1 and
/// 48 kHz), shorter for files under a second so transients stay sharp.
fn fft_size(rate: u32, frames: u64) -> usize {
    let full = ((f64::from(rate) * 0.04) as usize).next_power_of_two();
    if frames == 0 {
        return full.max(256);
    }
    // The largest power of two at most frames / 16.
    let short = ((frames / 16) as usize + 1).next_power_of_two() / 2;
    full.min(short).max(256)
}

/// Frames between windows, and windows per column: columns are as narrow as
/// `MIN_COLUMN_FRAMES` and `MAX_COLUMNS` allow, and windows overlap by at
/// least half so no audio is skipped.
fn hops(fft_size: usize, frames: u64) -> (usize, usize) {
    let column = if frames == 0 {
        fft_size / 2
    } else {
        (frames.div_ceil(MAX_COLUMNS) as usize).max(MIN_COLUMN_FRAMES)
    };
    let per_column = column.div_ceil(fft_size / 2);
    (column.div_ceil(per_column), per_column)
}

fn bands(rate: u32, fft_size: usize) -> Vec<Band> {
    let bin_hz = f64::from(rate) / fft_size as f64;
    let last = fft_size / 2;
    (0..BANDS)
        .map(|b| {
            let lo = band_frequency(b as f64 / BANDS as f64, rate) / bin_hz;
            let hi = band_frequency((b + 1) as f64 / BANDS as f64, rate) / bin_hz;
            let bins = lo.ceil() as usize..(hi.floor() as usize + 1).min(last + 1);
            if bins.is_empty() {
                let centre = (lo * hi).sqrt().min(last as f64);
                let k = (centre as usize).min(last - 1);
                Band::Between(k, (centre - k as f64) as f32)
            } else {
                Band::Bins(bins)
            }
        })
        .collect()
}

/// Power relative to a full-scale sine, as a level.
fn level(relative_power: f32) -> u8 {
    let db = 10.0 * relative_power.max(1e-30).log10();
    ((1.0 - db / FLOOR_DB) * 255.0).round().clamp(0.0, 255.0) as u8
}

/// Reduces stored columns (`BANDS` levels each) to `width` columns spanning
/// `total_columns` (which may exceed what has been computed so far), keeping
/// the loudest level per band. Columns past the computed data are `None`.
pub fn columns(levels: &[u8], total_columns: usize, width: usize) -> Vec<Option<[u8; BANDS]>> {
    let computed = levels.len() / BANDS;
    let total = total_columns.max(computed).max(1);
    (0..width)
        .map(|x| {
            let start = x * total / width;
            let end = ((x + 1) * total / width).max(start + 1).min(computed);
            if start >= end {
                return None;
            }
            let mut out = [0u8; BANDS];
            for column in levels[start * BANDS..end * BANDS].as_chunks::<BANDS>().0 {
                for (o, &l) in out.iter_mut().zip(column) {
                    *o = (*o).max(l);
                }
            }
            Some(out)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs `signal` (frame index -> sample, same on every channel) through an
    /// analyser in decoder-sized chunks.
    fn analyse(rate: u32, channels: usize, frames: u64, signal: impl Fn(u64) -> f32) -> (Analyzer, Vec<u8>) {
        let mut a = Analyzer::new(rate, channels, frames);
        let mut out = Vec::new();
        let samples: Vec<f32> = (0..frames)
            .flat_map(|i| std::iter::repeat_n(signal(i), channels))
            .collect();
        for chunk in samples.chunks(4096 * channels) {
            a.push(chunk, &mut out);
        }
        a.finish(&mut out);
        (a, out)
    }

    fn sine(rate: u32, hz: f64, amplitude: f32) -> impl Fn(u64) -> f32 {
        move |i| amplitude * (std::f64::consts::TAU * hz * i as f64 / f64::from(rate)).sin() as f32
    }

    fn band_of(hz: f64, rate: u32) -> usize {
        (band_position(hz, rate) * BANDS as f64) as usize
    }

    #[test]
    fn full_scale_sine_peaks_at_its_band() {
        let rate = 48_000;
        let (_, out) = analyse(rate, 2, 48_000, sine(rate, 1000.0, 1.0));
        let middle = &out[out.len() / 2 / BANDS * BANDS..][..BANDS];
        let loudest = (0..BANDS).max_by_key(|&b| middle[b]).unwrap();
        assert!(loudest.abs_diff(band_of(1000.0, rate)) <= 1, "loudest band {loudest}");
        assert!(middle[loudest] >= 250, "level {}", middle[loudest]);
        assert!(middle[band_of(100.0, rate)] < 64);
        assert!(middle[band_of(10_000.0, rate)] < 64);
    }

    #[test]
    fn level_follows_decibels() {
        let rate = 44_100;
        // -60 dBFS is halfway between FLOOR_DB and 0 dB.
        let (_, out) = analyse(rate, 1, 44_100, sine(rate, 3000.0, 0.001));
        let middle = &out[out.len() / 2 / BANDS * BANDS..][..BANDS];
        let peak = *middle.iter().max().unwrap();
        assert!((124..=130).contains(&peak), "level {peak}");
    }

    #[test]
    fn silence_is_floor() {
        let (_, out) = analyse(44_100, 2, 10_000, |_| 0.0);
        assert!(!out.is_empty());
        assert!(out.iter().all(|&l| l == 0));
    }

    #[test]
    fn columns_cover_every_frame() {
        // The last is long enough to average two windows per column (kept
        // at a low rate so its windows are small and the test fast).
        for (rate, frames) in [
            (44_100, 1),
            (44_100, 100),
            (44_100, 12_345),
            (48_000, 48_000),
            (8_000, 2_200_000),
        ] {
            let (a, out) = analyse(rate, 1, frames, |_| 0.0);
            let expected = frames.div_ceil(a.column_frames() as u64) as usize;
            assert_eq!(out.len(), expected * BANDS, "{frames} frames");
            assert!(expected as u64 <= MAX_COLUMNS);
            assert_eq!(a.windows_per_column > 1, rate == 8_000);
        }
    }

    #[test]
    fn unknown_length_still_produces_columns() {
        let mut a = Analyzer::new(44_100, 2, 0);
        let mut out = Vec::new();
        a.push(&vec![0.0; 44_100 * 2], &mut out);
        a.finish(&mut out);
        assert_eq!(out.len(), 44_100usize.div_ceil(a.column_frames()) * BANDS);
    }

    #[test]
    fn windows_cover_long_columns() {
        // An hour at 44.1 kHz: many windows per column, never more than half a window apart.
        let size = fft_size(44_100, 3600 * 44_100);
        let (hop, per_column) = hops(size, 3600 * 44_100);
        assert_eq!(size, 2048);
        assert!(hop <= size / 2);
        assert!((3600 * 44_100u64).div_ceil((hop * per_column) as u64) <= MAX_COLUMNS);
    }

    #[test]
    fn short_files_use_short_windows() {
        assert_eq!(fft_size(44_100, 44_100), 2048);
        assert_eq!(fft_size(44_100, 8_820), 512);
        assert_eq!(fft_size(44_100, 100), 256);
        assert_eq!(fft_size(96_000, 0), 4096);
    }

    #[test]
    fn bands_rise_and_stay_within_bins() {
        let bands = bands(44_100, 2048);
        assert_eq!(bands.len(), BANDS);
        assert!(matches!(bands[0], Band::Between(1, _)));
        assert!(
            matches!(&bands[BANDS - 1], Band::Bins(r) if r.end == 1025),
            "top band must reach Nyquist"
        );
        let start = |b: &Band| match b {
            Band::Between(k, t) => *k as f32 + t,
            Band::Bins(r) => r.start as f32,
        };
        assert!(bands.windows(2).all(|w| start(&w[0]) <= start(&w[1])));
    }

    #[test]
    fn band_position_round_trips() {
        assert!((band_position(MIN_HZ, 48_000)).abs() < 1e-9);
        assert!((band_position(24_000.0, 48_000) - 1.0).abs() < 1e-9);
        assert!((band_frequency(band_position(1234.0, 44_100), 44_100) - 1234.0).abs() < 1e-6);
    }

    fn column(level: u8) -> [u8; BANDS] {
        [level; BANDS]
    }

    #[test]
    fn columns_keep_the_loudest_level() {
        let mut levels = [column(10), column(200), column(30), column(40)].concat();
        levels[5] = 250;
        let cols = columns(&levels, 4, 2);
        let mut first = column(200);
        first[5] = 250;
        assert_eq!(cols, vec![Some(first), Some(column(40))]);
    }

    #[test]
    fn columns_beyond_computed_data_are_empty() {
        let levels = [column(1), column(2)].concat();
        let cols = columns(&levels, 8, 4);
        assert_eq!(cols[0], Some(column(2)));
        assert_eq!(cols[1..], [None, None, None]);
    }

    #[test]
    fn columns_wider_than_data_repeat_columns() {
        let levels = column(7);
        assert_eq!(columns(&levels, 1, 3), vec![Some(column(7)); 3]);
    }

    #[test]
    fn job_analyses_a_file() {
        let path = crate::audio::decoder::tests::temp_wav("spectrogram", 44_100, 44_100);
        let out = Mutex::new(Spectrogram::default());
        compute(&path, &out, &AtomicBool::new(false), &|| {}).unwrap();
        let s = out.into_inner().unwrap();
        assert_eq!(s.rate, 44_100);
        assert!(s.column_frames >= MIN_COLUMN_FRAMES);
        assert_eq!(s.columns(), 44_100usize.div_ceil(s.column_frames));
        assert!(s.levels.iter().any(|&l| l > 0));
        std::fs::remove_file(path).unwrap();
    }
}
