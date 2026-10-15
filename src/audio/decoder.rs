//! FFmpeg-backed decoding to interleaved f32.

use std::path::Path;

use anyhow::{Context as _, Result, anyhow};
use ff::ChannelLayout;
use ff::format::Sample;
use ff::format::sample::Type as SampleType;
use ff::software::resampling;
use ffmpeg_next as ff;

/// What the decoder should produce.
#[derive(Clone, Copy, Debug)]
pub enum OutputSpec {
    /// Stereo at the given rate, for playback.
    Stereo { rate: u32 },
    /// The file's own channel count and sample rate, for analysis.
    Native,
}

#[derive(Clone, Copy, Debug)]
pub struct StreamInfo {
    /// Output channel count (2 for `Stereo`).
    pub channels: usize,
    /// Output sample rate.
    pub rate: u32,
    /// Best-effort duration in seconds; 0 when the container does not say.
    pub duration: f64,
}

pub enum Read {
    /// Samples were appended.
    Data,
    /// The stream is exhausted; no more samples will follow.
    Eof,
}

pub struct Decoder {
    input: ff::format::context::Input,
    stream_index: usize,
    time_base: f64,
    decoder: ff::decoder::Audio,
    resampler: resampling::Context,
    info: StreamInfo,
    /// After a seek, output before this time (seconds) is discarded.
    skip_until: Option<f64>,
    input_done: bool,
    decoder_done: bool,
    frame: ff::frame::Audio,
}

impl Decoder {
    pub fn open(path: &Path, spec: OutputSpec) -> Result<Self> {
        let input = ff::format::input(path).with_context(|| format!("opening {}", path.display()))?;
        let stream = input
            .streams()
            .best(ff::media::Type::Audio)
            .ok_or_else(|| anyhow!("no audio stream"))?;
        let stream_index = stream.index();
        let time_base = f64::from(stream.time_base());
        let decoder = ff::codec::context::Context::from_parameters(stream.parameters())?
            .decoder()
            .audio()
            .context("unsupported codec")?;

        let in_layout = layout_or_default(decoder.channel_layout(), decoder.channels());
        let (out_layout, out_rate) = match spec {
            OutputSpec::Stereo { rate } => (ChannelLayout::STEREO, rate),
            OutputSpec::Native => (ChannelLayout::default(in_layout.channels()), decoder.rate()),
        };
        let resampler = resampling::Context::get(
            decoder.format(),
            in_layout,
            decoder.rate(),
            Sample::F32(SampleType::Packed),
            out_layout,
            out_rate,
        )
        .context("creating resampler")?;

        let duration = duration_secs(&input, &stream);
        let info = StreamInfo {
            channels: out_layout.channels().max(1) as usize,
            rate: out_rate,
            duration,
        };
        Ok(Self {
            input,
            stream_index,
            time_base,
            decoder,
            resampler,
            info,
            skip_until: None,
            input_done: false,
            decoder_done: false,
            frame: ff::frame::Audio::empty(),
        })
    }

    pub fn info(&self) -> StreamInfo {
        self.info
    }

    /// Seeks so the next samples produced start exactly at `secs`.
    pub fn seek(&mut self, secs: f64) -> Result<()> {
        let secs = secs.max(0.0);
        let ts = (secs * f64::from(ff::ffi::AV_TIME_BASE)) as i64;
        self.input.seek(ts, ..ts).context("seek")?;
        self.decoder.flush();
        // Start from a fresh resampler so no pre-seek samples leak through.
        let input = *self.resampler.input();
        self.resampler = resampling::Context::get(
            input.format,
            input.channel_layout,
            input.rate,
            Sample::F32(SampleType::Packed),
            self.out_layout(),
            self.info.rate,
        )?;
        self.skip_until = Some(secs);
        self.input_done = false;
        self.decoder_done = false;
        Ok(())
    }

    /// Appends at least one frame of interleaved samples to `out`, or reports EOF.
    pub fn read(&mut self, out: &mut Vec<f32>) -> Result<Read> {
        loop {
            if self.decoder_done {
                return Ok(Read::Eof);
            }
            match self.decoder.receive_frame(&mut self.frame) {
                Ok(()) => {
                    if self.convert_frame(out)? {
                        return Ok(Read::Data);
                    }
                }
                Err(ff::Error::Other { errno }) if errno == ff::util::error::EAGAIN => self.feed()?,
                Err(ff::Error::Eof) => {
                    self.decoder_done = true;
                    let before = out.len();
                    self.drain_resampler(out)?;
                    return Ok(if out.len() > before { Read::Data } else { Read::Eof });
                }
                // Corrupt frames: skip and keep going, like most players do.
                Err(_) => continue,
            }
        }
    }

    fn feed(&mut self) -> Result<()> {
        if self.input_done {
            return Ok(());
        }
        loop {
            match self.input.packets().next() {
                Some((stream, packet)) if stream.index() == self.stream_index => {
                    // A bad packet shouldn't end playback; the decoder resyncs.
                    let _ = self.decoder.send_packet(&packet);
                    return Ok(());
                }
                Some(_) => continue,
                None => {
                    self.input_done = true;
                    self.decoder.send_eof()?;
                    return Ok(());
                }
            }
        }
    }

    /// Converts the decoded frame; returns whether anything was appended.
    fn convert_frame(&mut self, out: &mut Vec<f32>) -> Result<bool> {
        let start = self.frame.timestamp().map(|ts| ts as f64 * self.time_base);
        // Files without a channel mask decode with an unspecified layout, which the
        // resampler rejects as a format change; give such frames the default layout.
        let layout = layout_or_default(self.frame.channel_layout(), self.frame.channels());
        self.frame.set_channel_layout(layout);
        // The frame's format can also differ from the stream header (e.g. mid-stream changes).
        if layout != self.resampler.input().channel_layout
            || self.frame.rate() != self.resampler.input().rate
            || self.frame.format() != self.resampler.input().format
        {
            self.resampler = resampling::Context::get(
                self.frame.format(),
                layout,
                self.frame.rate(),
                Sample::F32(SampleType::Packed),
                self.out_layout(),
                self.info.rate,
            )?;
        }
        let in_samples = self.frame.samples();
        let capacity = in_samples * self.info.rate as usize / self.frame.rate().max(1) as usize + 64;
        let mut converted = self.new_output_frame(capacity);
        self.resampler.run(&self.frame, &mut converted)?;
        let before = out.len();
        append_packed(&converted, self.info.channels, out);

        if let (Some(target), Some(start)) = (self.skip_until, start) {
            let produced = (out.len() - before) / self.info.channels;
            let end = start + produced as f64 / self.info.rate as f64;
            if end <= target {
                out.truncate(before);
                return Ok(false);
            }
            let skip = ((target - start).max(0.0) * self.info.rate as f64).round() as usize;
            out.drain(before..before + skip.min(produced) * self.info.channels);
        }
        self.skip_until = None;
        Ok(out.len() > before)
    }

    fn drain_resampler(&mut self, out: &mut Vec<f32>) -> Result<()> {
        loop {
            let mut converted = self.new_output_frame(4096);
            let more = self.resampler.flush(&mut converted)?;
            append_packed(&converted, self.info.channels, out);
            if more.is_none() || converted.samples() == 0 {
                return Ok(());
            }
        }
    }

    // Rebuilt on demand: ChannelLayout isn't Send, and the decoder moves between threads.
    fn out_layout(&self) -> ChannelLayout {
        ChannelLayout::default(self.info.channels as i32)
    }

    fn new_output_frame(&self, samples: usize) -> ff::frame::Audio {
        ff::frame::Audio::new(Sample::F32(SampleType::Packed), samples, self.out_layout())
    }
}

fn append_packed(frame: &ff::frame::Audio, channels: usize, out: &mut Vec<f32>) {
    let n = frame.samples() * channels;
    if n == 0 {
        return;
    }
    let bytes = &frame.data(0)[..n * 4];
    out.extend(
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_ne_bytes([b[0], b[1], b[2], b[3]])),
    );
}

fn layout_or_default(layout: ChannelLayout, channels: u16) -> ChannelLayout {
    if layout.is_empty() || layout.channels() != i32::from(channels) {
        ChannelLayout::default(i32::from(channels.max(1)))
    } else {
        layout
    }
}

pub(crate) fn duration_secs(input: &ff::format::context::Input, stream: &ff::format::stream::Stream) -> f64 {
    if stream.duration() > 0 {
        return stream.duration() as f64 * f64::from(stream.time_base());
    }
    if input.duration() > 0 {
        return input.duration() as f64 / f64::from(ff::ffi::AV_TIME_BASE);
    }
    0.0
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Writes a 16-bit stereo WAV whose left channel encodes the frame index
    /// (frame % 32768) and whose right channel is its negation.
    pub(crate) fn write_test_wav(path: &Path, rate: u32, frames: u32) {
        let data_len = frames * 4;
        let mut b = Vec::with_capacity(44 + data_len as usize);
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data_len).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes()); // PCM
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * 4).to_le_bytes());
        b.extend_from_slice(&4u16.to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..frames {
            let v = (i % 32768) as i16;
            b.extend_from_slice(&v.to_le_bytes());
            b.extend_from_slice(&(-v).to_le_bytes());
        }
        std::fs::write(path, b).unwrap();
    }

    pub(crate) fn temp_wav(name: &str, rate: u32, frames: u32) -> std::path::PathBuf {
        ff::init().unwrap();
        let path = std::env::temp_dir().join(format!("waveshelf-{}-{name}.wav", std::process::id()));
        write_test_wav(&path, rate, frames);
        path
    }

    fn decode_all(d: &mut Decoder) -> Vec<f32> {
        let mut out = Vec::new();
        while let Read::Data = d.read(&mut out).unwrap() {}
        out
    }

    fn frame_value(i: u32) -> f32 {
        (i % 32768) as f32 / 32768.0
    }

    #[test]
    fn native_decode_is_lossless_and_complete() {
        let path = temp_wav("native", 44_100, 44_100);
        let mut d = Decoder::open(&path, OutputSpec::Native).unwrap();
        assert_eq!(d.info().channels, 2);
        assert_eq!(d.info().rate, 44_100);
        assert!((d.info().duration - 1.0).abs() < 1e-3);
        let out = decode_all(&mut d);
        assert_eq!(out.len(), 44_100 * 2);
        for i in [0u32, 1, 1000, 44_099] {
            assert_eq!(out[i as usize * 2], frame_value(i));
            assert_eq!(out[i as usize * 2 + 1], -frame_value(i));
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn seek_is_sample_accurate() {
        let path = temp_wav("seek", 44_100, 44_100);
        let mut d = Decoder::open(&path, OutputSpec::Native).unwrap();
        d.seek(0.5).unwrap();
        let out = decode_all(&mut d);
        assert_eq!(out[0], frame_value(22_050));
        assert_eq!(out.len(), 22_050 * 2);
        // Seeking backwards after EOF works too.
        d.seek(0.25).unwrap();
        let out = decode_all(&mut d);
        assert_eq!(out[0], frame_value(11_025));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn stereo_output_resamples_to_requested_rate() {
        let path = temp_wav("resample", 44_100, 44_100);
        let mut d = Decoder::open(&path, OutputSpec::Stereo { rate: 48_000 }).unwrap();
        assert_eq!(d.info().rate, 48_000);
        let frames = decode_all(&mut d).len() / 2;
        assert!((47_950..=48_050).contains(&frames), "got {frames} frames");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn missing_file_is_an_error() {
        ff::init().unwrap();
        assert!(Decoder::open(Path::new("/nonexistent/x.wav"), OutputSpec::Native).is_err());
    }

    /// Decodes every audio file in $WAVESHELF_MEDIA_DIR (playback and native
    /// paths, plus a seek). Run with: cargo test -- --ignored media_dir
    #[test]
    #[ignore]
    fn media_dir_decodes() {
        ff::init().unwrap();
        let dir = std::env::var("WAVESHELF_MEDIA_DIR").expect("set WAVESHELF_MEDIA_DIR");
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if !crate::library::is_audio(&path) {
                continue;
            }
            let mut d = Decoder::open(&path, OutputSpec::Stereo { rate: 48_000 }).unwrap();
            let duration = d.info().duration;
            let frames = decode_all(&mut d).len() / 2;
            let decoded_secs = frames as f64 / 48_000.0;
            d.seek(duration / 2.0).unwrap();
            let after_seek = decode_all(&mut d).len() as f64 / 2.0 / 48_000.0;
            let peaks = decode_all(&mut Decoder::open(&path, OutputSpec::Native).unwrap()).len();
            println!(
                "{:<14} duration={duration:.3}s decoded={decoded_secs:.3}s after_seek={after_seek:.3}s native_samples={peaks}",
                path.file_name().unwrap().to_string_lossy()
            );
            assert!((decoded_secs - duration).abs() < 0.1, "{}", path.display());
            assert!((after_seek - duration / 2.0).abs() < 0.1, "{}", path.display());
        }
    }
}
