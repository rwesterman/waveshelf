//! The big waveform: painting, ruler, playhead and mouse interaction.

use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Ui, pos2, vec2};

use crate::format::time_short;
use crate::waveform::{Peaks, columns};

const RULER_HEIGHT: f32 = 16.0;
const PLAYHEAD: Color32 = Color32::from_rgb(255, 150, 40);
const HOVER: Color32 = Color32::from_rgb(90, 160, 255);

#[derive(Default)]
pub struct WaveformResponse {
    pub seek_to: Option<f64>,
    /// Right-click: stop playback.
    pub stop: bool,
    /// Wheel over the waveform (volume), in notches.
    pub volume_steps: f32,
    /// Alt+wheel over the waveform (seek), in notches.
    pub seek_steps: f32,
}

pub fn show(ui: &mut Ui, peaks: Option<&Peaks>, duration: f64, position: Option<f64>) -> WaveformResponse {
    let rect = ui.available_rect_before_wrap();
    let response = ui.allocate_rect(rect, Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    let visuals = ui.visuals();
    let dark = visuals.dark_mode;
    let background = if dark {
        Color32::from_gray(22)
    } else {
        Color32::from_gray(245)
    };
    let wave = if dark {
        Color32::from_rgb(120, 170, 200)
    } else {
        Color32::from_rgb(40, 100, 150)
    };
    let played_tint = if dark {
        Color32::from_rgba_unmultiplied(255, 255, 255, 22)
    } else {
        Color32::from_rgba_unmultiplied(0, 0, 0, 18)
    };
    let text = visuals.weak_text_color();
    painter.rect_filled(rect, 0.0, background);

    let ruler = Rect::from_min_size(rect.min, vec2(rect.width(), RULER_HEIGHT));
    let body = Rect::from_min_max(pos2(rect.left(), ruler.bottom()), rect.max);
    let duration = if duration > 0.0 {
        duration
    } else {
        peaks.map_or(0.0, |p| p.duration)
    };
    let mut out = WaveformResponse::default();

    if duration <= 0.0 {
        let message = match peaks {
            Some(p) if p.error.is_some() => "Could not read waveform",
            Some(_) => "",
            None => "Select a file to play",
        };
        painter.text(
            body.center(),
            Align2::CENTER_CENTER,
            message,
            FontId::proportional(14.0),
            text,
        );
        return out;
    }

    let x_of = |t: f64| body.left() + (t / duration).clamp(0.0, 1.0) as f32 * body.width();
    let t_of = |x: f32| ((x - body.left()) / body.width()).clamp(0.0, 1.0) as f64 * duration;

    draw_ruler(&painter, ruler, duration, text);
    if let Some(p) = peaks {
        draw_peaks(&painter, body, p, duration, wave, dark);
    }
    if let Some(pos) = position {
        let x = x_of(pos);
        painter.rect_filled(Rect::from_min_max(body.min, pos2(x, body.bottom())), 0.0, played_tint);
        painter.line_segment(
            [pos2(x, rect.top()), pos2(x, rect.bottom())],
            Stroke::new(1.5, PLAYHEAD),
        );
    }
    if let Some(hover) = response.hover_pos() {
        painter.line_segment(
            [pos2(hover.x, body.top()), pos2(hover.x, body.bottom())],
            Stroke::new(1.0, HOVER),
        );
        let label = time_short(t_of(hover.x));
        let anchor = if hover.x > body.right() - 60.0 {
            Align2::RIGHT_TOP
        } else {
            Align2::LEFT_TOP
        };
        painter.text(
            pos2(
                hover.x + if anchor == Align2::LEFT_TOP { 4.0 } else { -4.0 },
                body.top() + 2.0,
            ),
            anchor,
            label,
            FontId::monospace(11.0),
            HOVER,
        );
    }

    if (response.clicked() || response.dragged())
        && let Some(p) = response.interact_pointer_pos()
    {
        out.seek_to = Some(t_of(p.x));
    }
    if response.secondary_clicked() {
        out.stop = true;
    }
    if response.hovered() {
        let (steps, alt) = ui.input(|i| (wheel_steps(&i.raw.events), i.modifiers.alt));
        if steps != 0.0 {
            if alt {
                out.seek_steps = steps
            } else {
                out.volume_steps = steps
            }
        }
    }
    out
}

/// Wheel movement this frame in notches; trackpad pixels count fractionally.
fn wheel_steps(events: &[egui::Event]) -> f32 {
    events
        .iter()
        .map(|e| match e {
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta,
                ..
            } => delta.y / 40.0,
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Line,
                delta,
                ..
            } => delta.y,
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Page,
                delta,
                ..
            } => delta.y * 10.0,
            _ => 0.0,
        })
        .sum()
}

fn draw_ruler(painter: &egui::Painter, ruler: Rect, duration: f64, color: Color32) {
    const STEPS: [f64; 14] = [
        0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0,
    ];
    let px_per_sec = ruler.width() as f64 / duration;
    let step = STEPS.iter().copied().find(|s| s * px_per_sec >= 70.0).unwrap_or(1800.0);
    let mut t = 0.0;
    while t <= duration {
        let x = ruler.left() + (t * px_per_sec) as f32;
        painter.line_segment(
            [pos2(x, ruler.bottom() - 5.0), pos2(x, ruler.bottom())],
            Stroke::new(1.0, color),
        );
        if x < ruler.right() - 30.0 {
            painter.text(
                pos2(x + 3.0, ruler.top() + 1.0),
                Align2::LEFT_TOP,
                time_short(t),
                FontId::monospace(10.0),
                color,
            );
        }
        t += step;
    }
    painter.line_segment(
        [ruler.left_bottom(), ruler.right_bottom()],
        Stroke::new(1.0, color.gamma_multiply(0.4)),
    );
}

fn draw_peaks(painter: &egui::Painter, body: Rect, peaks: &Peaks, duration: f64, color: Color32, dark: bool) {
    if peaks.channels == 0 || peaks.bucket_secs() == 0.0 {
        return;
    }
    let width = body.width().max(1.0) as usize;
    let total_buckets = (duration / peaks.bucket_secs()).ceil() as usize;
    let lane_height = body.height() / peaks.channels as f32;
    let separator = if dark {
        Color32::from_gray(45)
    } else {
        Color32::from_gray(215)
    };

    for (c, buckets) in peaks.buckets.iter().enumerate() {
        let top = body.top() + c as f32 * lane_height;
        let mid = top + lane_height / 2.0;
        let half = lane_height / 2.0 - 1.0;
        if c > 0 {
            painter.line_segment(
                [pos2(body.left(), top), pos2(body.right(), top)],
                Stroke::new(1.0, separator),
            );
        }
        painter.line_segment(
            [pos2(body.left(), mid), pos2(body.right(), mid)],
            Stroke::new(1.0, separator),
        );
        let mut segments: Vec<[Pos2; 2]> = Vec::with_capacity(width);
        for (x, col) in columns(buckets, total_buckets, width).into_iter().enumerate() {
            let Some((lo, hi)) = col else { continue };
            let x = body.left() + x as f32 + 0.5;
            let y0 = mid - hi.clamp(-1.0, 1.0) * half;
            let y1 = (mid - lo.clamp(-1.0, 1.0) * half).max(y0 + 1.0);
            segments.push([pos2(x, y0), pos2(x, y1)]);
        }
        for s in segments {
            painter.line_segment(s, Stroke::new(1.0, color));
        }
    }
}
