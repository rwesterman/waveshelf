//! Maps the output callback's consumed-frame counter to a track position.
//!
//! The engine records a segment whenever the sample stream jumps (load, seek,
//! loop restart): "from consumed frame N on, audio starts at track time T".
//! Because frames discarded on flush are counted as consumed too, the counter
//! never goes backwards and segments stay ordered.

use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Segment {
    start_frame: u64,
    start_secs: f64,
}

#[derive(Debug, Default)]
pub struct Timeline {
    rate: u32,
    segments: VecDeque<Segment>,
}

impl Timeline {
    pub fn reset(&mut self, rate: u32) {
        self.rate = rate;
        self.segments.clear();
    }

    pub fn push(&mut self, start_frame: u64, start_secs: f64) {
        // A new segment at the same frame supersedes the previous one.
        while self.segments.back().is_some_and(|s| s.start_frame >= start_frame) {
            self.segments.pop_back();
        }
        self.segments.push_back(Segment {
            start_frame,
            start_secs,
        });
    }

    /// Track position for the given consumed-frame count, or `None` when nothing is loaded.
    pub fn position(&mut self, consumed: u64) -> Option<f64> {
        // Drop segments the playhead has fully passed.
        while self.segments.len() > 1 && self.segments[1].start_frame <= consumed {
            self.segments.pop_front();
        }
        let seg = self.segments.front()?;
        if self.rate == 0 {
            return None;
        }
        let into = consumed.saturating_sub(seg.start_frame);
        Some(seg.start_secs + into as f64 / f64::from(self.rate))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_has_no_position() {
        let mut t = Timeline::default();
        t.reset(48_000);
        assert_eq!(t.position(100), None);
    }

    #[test]
    fn advances_from_segment_start() {
        let mut t = Timeline::default();
        t.reset(1000);
        t.push(500, 2.0);
        assert_eq!(t.position(500), Some(2.0));
        assert_eq!(t.position(1500), Some(3.0));
    }

    #[test]
    fn holds_at_start_until_playhead_reaches_pending_segment() {
        let mut t = Timeline::default();
        t.reset(1000);
        t.push(0, 0.0);
        // Seek queued at frame 2000 while the playhead is still at 1500.
        t.push(2000, 30.0);
        assert_eq!(t.position(1500), Some(1.5));
        assert_eq!(t.position(2000), Some(30.0));
        assert_eq!(t.position(2250), Some(30.25));
    }

    #[test]
    fn loop_restart_wraps_to_zero() {
        let mut t = Timeline::default();
        t.reset(1000);
        t.push(0, 0.0);
        t.push(4000, 0.0); // 4 s file looped
        assert_eq!(t.position(3999), Some(3.999));
        assert_eq!(t.position(4500), Some(0.5));
    }

    #[test]
    fn segment_at_same_frame_replaces_previous() {
        let mut t = Timeline::default();
        t.reset(1000);
        t.push(100, 1.0);
        t.push(100, 5.0);
        assert_eq!(t.position(100), Some(5.0));
    }
}
