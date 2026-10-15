//! Audio device output via cpal.
//!
//! The device callback pops stereo f32 frames from a lock-free ring buffer,
//! applies a smoothed gain, and maps them onto however many channels the
//! device has. It never blocks or allocates.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use anyhow::{Context as _, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};

/// Gain ramp length, so pause, resume, mute and dim never click.
const RAMP_SECS: f32 = 0.008;

/// State shared between the engine thread, the device callback and the UI.
#[derive(Default)]
pub struct Shared {
    /// Frames taken from the ring buffer, including frames discarded on flush.
    pub consumed: AtomicU64,
    /// Set by the engine to ask the callback to discard everything buffered.
    pub flush: AtomicBool,
    pub paused: AtomicBool,
    /// Target linear gain as f32 bits.
    gain: AtomicU32,
    /// Set when the device reports an error (e.g. it was unplugged).
    pub device_error: AtomicBool,
}

impl Shared {
    pub fn new() -> Self {
        let s = Self::default();
        s.set_gain(1.0);
        s.paused.store(true, Ordering::Relaxed);
        s
    }

    pub fn set_gain(&self, gain: f32) {
        self.gain.store(gain.to_bits(), Ordering::Relaxed);
    }

    fn gain(&self) -> f32 {
        f32::from_bits(self.gain.load(Ordering::Relaxed))
    }
}

pub struct Output {
    _stream: cpal::Stream,
    pub rate: u32,
    pub device_name: String,
}

impl Output {
    /// Opens the default output device. Returns the output and the ring
    /// buffer producer the engine writes stereo frames into.
    pub fn open(shared: Arc<Shared>) -> Result<(Self, rtrb::Producer<f32>)> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow!("no output device"))?;
        let device_name = device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| "Output device".into());
        let supported = device.default_output_config().context("querying output format")?;
        let config = supported.config();
        let rate = config.sample_rate;
        // ~300 ms of stereo audio: plenty of headroom, and flushes make its length irrelevant to seeks.
        let (producer, consumer) = rtrb::RingBuffer::new(rate as usize * 2 * 3 / 10);

        let stream = match supported.sample_format() {
            SampleFormat::F32 => build::<f32>(&device, config, consumer, shared)?,
            SampleFormat::F64 => build::<f64>(&device, config, consumer, shared)?,
            SampleFormat::I16 => build::<i16>(&device, config, consumer, shared)?,
            SampleFormat::I32 => build::<i32>(&device, config, consumer, shared)?,
            SampleFormat::U16 => build::<u16>(&device, config, consumer, shared)?,
            other => return Err(anyhow!("unsupported device sample format {other}")),
        };
        stream.play().context("starting output stream")?;
        Ok((
            Self {
                _stream: stream,
                rate,
                device_name,
            },
            producer,
        ))
    }
}

fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut consumer: rtrb::Consumer<f32>,
    shared: Arc<Shared>,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let out_channels = usize::from(config.channels.max(1));
    let step = 1.0 / (RAMP_SECS * config.sample_rate as f32);
    let mut gain = 0.0f32;
    let err_shared = shared.clone();

    let stream = device.build_output_stream::<T, _, _>(
        config,
        move |data: &mut [T], _| {
            let frames = data.len() / out_channels;

            if shared.flush.load(Ordering::Acquire) {
                let n = consumer.slots();
                if let Ok(chunk) = consumer.read_chunk(n) {
                    chunk.commit_all();
                }
                shared.consumed.fetch_add((n / 2) as u64, Ordering::AcqRel);
                shared.flush.store(false, Ordering::Release);
                gain = 0.0;
                data.fill(T::from_sample(0.0));
                return;
            }

            let target = if shared.paused.load(Ordering::Relaxed) {
                0.0
            } else {
                shared.gain()
            };
            // Once a pause has faded out, stop consuming so playback resumes where it stopped.
            if target == 0.0 && gain == 0.0 && shared.paused.load(Ordering::Relaxed) {
                data.fill(T::from_sample(0.0));
                return;
            }

            let available = (consumer.slots() / 2).min(frames);
            let mut written = 0;
            if let Ok(chunk) = consumer.read_chunk(available * 2) {
                let (a, b) = chunk.as_slices();
                let mut samples = a.iter().chain(b.iter());
                for frame in data.chunks_exact_mut(out_channels).take(available) {
                    gain += (target - gain).clamp(-step, step);
                    let l = *samples.next().unwrap_or(&0.0) * gain;
                    let r = *samples.next().unwrap_or(&0.0) * gain;
                    write_frame(frame, l, r);
                    written += 1;
                }
                chunk.commit_all();
            }
            for frame in data.chunks_exact_mut(out_channels).skip(written) {
                frame.fill(T::from_sample(0.0));
            }
            shared.consumed.fetch_add(written as u64, Ordering::AcqRel);
        },
        move |_err| err_shared.device_error.store(true, Ordering::Release),
        None,
    )?;
    Ok(stream)
}

/// Writes a stereo frame to a device frame of any width.
fn write_frame<T: SizedSample + FromSample<f32>>(frame: &mut [T], l: f32, r: f32) {
    match frame.len() {
        1 => frame[0] = T::from_sample(0.5 * (l + r)),
        _ => {
            frame[0] = T::from_sample(l);
            frame[1] = T::from_sample(r);
            for s in &mut frame[2..] {
                *s = T::from_sample(0.0);
            }
        }
    }
}
