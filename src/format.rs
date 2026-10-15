//! Display formatting helpers.

use std::time::SystemTime;

/// `m:ss.mmm`, or `h:mm:ss.mmm` past an hour.
pub fn time_precise(secs: f64) -> String {
    let ms = (secs.max(0.0) * 1000.0).round() as u64;
    let (h, m, s, ms) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}.{ms:03}")
    } else {
        format!("{m}:{s:02}.{ms:03}")
    }
}

/// `m:ss`, with tenths/hundredths for sub-second values (ruler labels).
pub fn time_short(secs: f64) -> String {
    let secs = secs.max(0.0);
    if secs > 0.0 && secs.fract().abs() > 1e-6 && secs < 60.0 {
        return format!("{secs:.2}");
    }
    let total = secs.round() as u64;
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

pub fn sample_rate(rate: u32) -> String {
    if rate == 0 {
        String::new()
    } else if rate.is_multiple_of(1000) {
        format!("{} kHz", rate / 1000)
    } else {
        format!("{:.1} kHz", f64::from(rate) / 1000.0)
    }
}

pub fn channels(n: u16) -> String {
    match n {
        0 => String::new(),
        1 => "Mono".into(),
        2 => "Stereo".into(),
        n => n.to_string(),
    }
}

/// "Today 14:03", "Yesterday 09:12", or "2024-05-01 18:22", in local time.
pub fn modified(t: Option<SystemTime>) -> String {
    let Some(t) = t else { return String::new() };
    let t: chrono::DateTime<chrono::Local> = t.into();
    let today = chrono::Local::now().date_naive();
    let date = t.date_naive();
    let time = t.format("%H:%M");
    if date == today {
        format!("Today {time}")
    } else if Some(date) == today.pred_opt() {
        format!("Yesterday {time}")
    } else {
        t.format("%Y-%m-%d %H:%M").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precise_times() {
        assert_eq!(time_precise(0.0), "0:00.000");
        assert_eq!(time_precise(65.4321), "1:05.432");
        assert_eq!(time_precise(3725.0), "1:02:05.000");
        assert_eq!(time_precise(-1.0), "0:00.000");
    }

    #[test]
    fn short_times() {
        assert_eq!(time_short(0.0), "0:00");
        assert_eq!(time_short(90.0), "1:30");
        assert_eq!(time_short(0.25), "0.25");
        assert_eq!(time_short(7322.0), "2:02:02");
    }

    #[test]
    fn rates_and_channels() {
        assert_eq!(sample_rate(48_000), "48 kHz");
        assert_eq!(sample_rate(44_100), "44.1 kHz");
        assert_eq!(sample_rate(0), "");
        assert_eq!(channels(1), "Mono");
        assert_eq!(channels(6), "6");
    }
}
