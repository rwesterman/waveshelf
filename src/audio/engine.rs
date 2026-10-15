//! The playback engine thread: owns the output device and the current
//! decoder, and keeps the ring buffer topped up.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, unbounded};

use super::decoder::{Decoder, OutputSpec, Read};
use super::output::{Output, Shared};
use super::timeline::Timeline;

pub enum Command {
    Load {
        id: u64,
        path: PathBuf,
        start: f64,
        paused: bool,
    },
    Play,
    Pause,
    Seek(f64),
    /// Pause and rewind to the start, keeping the file loaded.
    Stop,
    Unload,
    SetLoop(bool),
}

#[derive(Debug)]
pub enum Event {
    Loaded {
        id: u64,
        duration: f64,
    },
    Finished {
        id: u64,
    },
    Failed {
        id: u64,
        error: String,
    },
    Device {
        name: Option<String>,
        error: Option<String>,
    },
}

/// UI-side handle to the engine thread.
pub struct EngineHandle {
    commands: Sender<Command>,
    pub events: Receiver<Event>,
    shared: Arc<Shared>,
    timeline: Arc<Mutex<Timeline>>,
}

impl EngineHandle {
    pub fn spawn(wake: impl Fn() + Send + 'static) -> Self {
        let (commands, command_rx) = unbounded();
        let (event_tx, events) = unbounded();
        let shared = Arc::new(Shared::new());
        let timeline = Arc::new(Mutex::new(Timeline::default()));
        let engine = Engine {
            shared: shared.clone(),
            timeline: timeline.clone(),
            events: event_tx,
            wake: Box::new(wake),
            output: None,
            current: None,
            pushed: 0,
            looping: false,
        };
        thread::Builder::new()
            .name("audio-engine".into())
            .spawn(move || engine.run(command_rx))
            .expect("spawning audio engine");
        Self {
            commands,
            events,
            shared,
            timeline,
        }
    }

    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    pub fn set_gain(&self, gain: f32) {
        self.shared.set_gain(gain);
    }

    pub fn is_paused(&self) -> bool {
        self.shared.paused.load(Ordering::Relaxed)
    }

    /// Position of what is currently audible, in seconds.
    pub fn position(&self) -> Option<f64> {
        let consumed = self.shared.consumed.load(Ordering::Acquire);
        self.timeline.lock().unwrap().position(consumed)
    }
}

struct OutputState {
    output: Output,
    producer: rtrb::Producer<f32>,
}

struct Current {
    id: u64,
    path: PathBuf,
    decoder: Decoder,
    pending: Vec<f32>,
    pending_pos: usize,
    eof: bool,
    finished_sent: bool,
}

struct Engine {
    shared: Arc<Shared>,
    timeline: Arc<Mutex<Timeline>>,
    events: Sender<Event>,
    wake: Box<dyn Fn() + Send>,
    output: Option<OutputState>,
    current: Option<Current>,
    /// Frames written to the ring buffer since the output was opened.
    pushed: u64,
    looping: bool,
}

impl Engine {
    fn run(mut self, commands: Receiver<Command>) {
        loop {
            let idle = !self.fill();
            self.check_finished();
            if self.shared.device_error.swap(false, Ordering::AcqRel) {
                self.recover_device();
            }
            let timeout = if idle {
                Duration::from_millis(20)
            } else {
                Duration::from_millis(4)
            };
            match commands.recv_timeout(timeout) {
                Ok(cmd) => self.handle(cmd),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            while let Ok(cmd) = commands.try_recv() {
                self.handle(cmd);
            }
        }
    }

    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
        (self.wake)();
    }

    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Load {
                id,
                path,
                start,
                paused,
            } => self.load(id, path, start, paused),
            Command::Play => {
                if self.current.is_some() {
                    self.shared.paused.store(false, Ordering::Relaxed);
                }
            }
            Command::Pause => self.shared.paused.store(true, Ordering::Relaxed),
            Command::Seek(secs) => self.seek(secs),
            Command::Stop => {
                self.shared.paused.store(true, Ordering::Relaxed);
                self.seek(0.0);
            }
            Command::Unload => {
                self.shared.paused.store(true, Ordering::Relaxed);
                self.flush();
                self.current = None;
                self.timeline.lock().unwrap().reset(0);
            }
            Command::SetLoop(on) => self.looping = on,
        }
    }

    fn ensure_output(&mut self) -> Option<u32> {
        if self.output.is_none() {
            match Output::open(self.shared.clone()) {
                Ok((output, producer)) => {
                    let name = output.device_name.clone();
                    self.pushed = self.shared.consumed.load(Ordering::Acquire);
                    self.output = Some(OutputState { output, producer });
                    self.emit(Event::Device {
                        name: Some(name),
                        error: None,
                    });
                }
                Err(e) => {
                    self.emit(Event::Device {
                        name: None,
                        error: Some(format!("{e:#}")),
                    });
                    return None;
                }
            }
        }
        self.output.as_ref().map(|o| o.output.rate)
    }

    fn load(&mut self, id: u64, path: PathBuf, start: f64, paused: bool) {
        let Some(rate) = self.ensure_output() else {
            self.emit(Event::Failed {
                id,
                error: "no audio output device".into(),
            });
            return;
        };
        self.flush();
        self.current = None;
        let opened = Decoder::open(&path, OutputSpec::Stereo { rate }).and_then(|mut d| {
            if start > 0.0 {
                d.seek(start)?;
            }
            Ok(d)
        });
        match opened {
            Ok(decoder) => {
                let duration = decoder.info().duration;
                {
                    let mut tl = self.timeline.lock().unwrap();
                    tl.reset(rate);
                    tl.push(self.pushed, start);
                }
                self.current = Some(Current {
                    id,
                    path,
                    decoder,
                    pending: Vec::with_capacity(16 * 1024),
                    pending_pos: 0,
                    eof: false,
                    finished_sent: false,
                });
                self.shared.paused.store(paused, Ordering::Relaxed);
                self.emit(Event::Loaded { id, duration });
            }
            Err(e) => {
                self.timeline.lock().unwrap().reset(0);
                self.shared.paused.store(true, Ordering::Relaxed);
                self.emit(Event::Failed {
                    id,
                    error: format!("{e:#}"),
                });
            }
        }
    }

    fn seek(&mut self, secs: f64) {
        if self.current.is_none() {
            return;
        }
        self.flush();
        let cur = self.current.as_mut().unwrap();
        if let Err(e) = cur.decoder.seek(secs) {
            let id = cur.id;
            self.current = None;
            self.emit(Event::Failed {
                id,
                error: format!("{e:#}"),
            });
            return;
        }
        cur.pending.clear();
        cur.pending_pos = 0;
        cur.eof = false;
        cur.finished_sent = false;
        self.timeline.lock().unwrap().push(self.pushed, secs);
    }

    /// Discards everything buffered, waiting for the device callback to do it.
    fn flush(&mut self) {
        if self.output.is_none() {
            return;
        }
        self.shared.flush.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_millis(250);
        while self.shared.flush.load(Ordering::Acquire) {
            if Instant::now() > deadline {
                // The device stopped calling back; rebuild it on next use.
                self.shared.flush.store(false, Ordering::Release);
                self.output = None;
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        self.pushed = self.shared.consumed.load(Ordering::Acquire);
    }

    /// Tops up the ring buffer. Returns whether there was anything to do.
    fn fill(&mut self) -> bool {
        let (Some(out), Some(cur)) = (self.output.as_mut(), self.current.as_mut()) else {
            return false;
        };
        let mut worked = false;
        let mut failed = None;
        loop {
            if cur.pending_pos >= cur.pending.len() {
                if cur.eof {
                    break;
                }
                cur.pending.clear();
                cur.pending_pos = 0;
                match cur.decoder.read(&mut cur.pending) {
                    Ok(Read::Data) => {}
                    Ok(Read::Eof) if self.looping => {
                        if cur.decoder.seek(0.0).is_err() {
                            cur.eof = true;
                        } else {
                            self.timeline.lock().unwrap().push(self.pushed, 0.0);
                        }
                        continue;
                    }
                    Ok(Read::Eof) => {
                        cur.eof = true;
                        break;
                    }
                    Err(e) => {
                        failed = Some((cur.id, format!("{e:#}")));
                        break;
                    }
                }
            }
            let free = out.producer.slots() & !1;
            if free == 0 {
                break;
            }
            let n = free.min(cur.pending.len() - cur.pending_pos);
            if let Ok(chunk) = out.producer.write_chunk_uninit(n) {
                chunk.fill_from_iter(cur.pending[cur.pending_pos..cur.pending_pos + n].iter().copied());
            }
            cur.pending_pos += n;
            self.pushed += (n / 2) as u64;
            worked = true;
        }
        if let Some((id, error)) = failed {
            self.current = None;
            self.emit(Event::Failed { id, error });
            return true;
        }
        worked
    }

    fn check_finished(&mut self) {
        let Some(cur) = self.current.as_mut() else { return };
        if cur.eof
            && !cur.finished_sent
            && cur.pending_pos >= cur.pending.len()
            && self.shared.consumed.load(Ordering::Acquire) >= self.pushed
        {
            cur.finished_sent = true;
            let id = cur.id;
            self.shared.paused.store(true, Ordering::Relaxed);
            self.emit(Event::Finished { id });
        }
    }

    /// Reopens the (possibly new) default device and resumes where we were.
    fn recover_device(&mut self) {
        let resume = self.current.as_ref().map(|c| (c.id, c.path.clone()));
        let position = {
            let consumed = self.shared.consumed.load(Ordering::Acquire);
            self.timeline.lock().unwrap().position(consumed).unwrap_or(0.0)
        };
        let paused = self.shared.paused.load(Ordering::Relaxed);
        self.output = None;
        self.current = None;
        if let Some((id, path)) = resume {
            self.load(id, path, position, paused);
        } else {
            self.ensure_output();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::decoder::tests::temp_wav;

    fn wait_for(events: &Receiver<Event>, secs: f64, mut pred: impl FnMut(&Event) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs_f64(secs);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match events.recv_timeout(left) {
                Ok(e) if pred(&e) => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
        false
    }

    /// Plays through the real default output device at zero gain (silent).
    /// Run with: cargo test -- --ignored engine_plays
    #[test]
    #[ignore]
    fn engine_plays_seeks_and_finishes_silently() {
        let path = temp_wav("engine", 44_100, 44_100 * 2); // 2 s
        let engine = EngineHandle::spawn(|| {});
        engine.set_gain(0.0);
        engine.send(Command::Load {
            id: 1,
            path: path.clone(),
            start: 0.0,
            paused: false,
        });
        assert!(wait_for(&engine.events, 3.0, |e| matches!(
            e,
            Event::Loaded { id: 1, .. }
        )));

        thread::sleep(Duration::from_millis(400));
        let p = engine.position().unwrap();
        assert!((0.2..0.7).contains(&p), "position after 400 ms: {p}");

        engine.send(Command::Pause);
        thread::sleep(Duration::from_millis(100));
        let paused_at = engine.position().unwrap();
        thread::sleep(Duration::from_millis(200));
        assert!(
            (engine.position().unwrap() - paused_at).abs() < 0.02,
            "position moved while paused"
        );

        engine.send(Command::Seek(1.5));
        thread::sleep(Duration::from_millis(50));
        let p = engine.position().unwrap();
        assert!((1.5..1.56).contains(&p), "position after seek while paused: {p}");

        engine.send(Command::Play);
        assert!(
            wait_for(&engine.events, 2.0, |e| matches!(e, Event::Finished { id: 1 })),
            "no Finished event"
        );
        let end = engine.position().unwrap();
        assert!((1.95..2.1).contains(&end), "position at end: {end}");
        std::fs::remove_file(path).unwrap();
    }
}
