//! Level meters: stereo bars on a dBFS scale, the way a mixing desk shows them.
//!
//! Each channel draws its peak level as a bright bar (instant rise, smooth fall),
//! the RMS (average loudness) as a solid bar inside it, and a held peak line.
//! The scale gives the top 18 dB half the bar, where levels actually get set.
//! Colours are zones on a fixed scale — green up to -18 dBFS (where speech and
//! game audio should sit), yellow up to -6, red above — clipped to the bar, so
//! the bar changes colour at exact dB marks with square edges. A clip light on
//! the right latches when a sample reaches full scale; click it to reset.

use std::time::{Duration, Instant};

use capture::mixer::Levels;
use egui::{Align2, Color32, FontId, Rect, Sense, Stroke, pos2, vec2};

/// Bottom of the scale, dBFS.
pub const FLOOR_DB: f32 = -60.0;
/// What silence reads as.
pub const SILENCE_DB: f32 = -120.0;
/// Where the colour zones change, dBFS.
const YELLOW_FROM: f32 = -18.0;
const RED_FROM: f32 = -6.0;
/// Peak fall-off, dB per second (IEC 60268-18 PPM-like: readable, not jumpy).
const FALL_DB_PER_S: f32 = 20.0;
/// How long the peak line holds before it falls.
const PEAK_HOLD: Duration = Duration::from_millis(1500);
/// How long the numeric peak readout holds its maximum.
const READOUT_HOLD: Duration = Duration::from_secs(3);

pub const GREEN: Color32 = Color32::from_rgb(64, 190, 105);
pub const YELLOW: Color32 = Color32::from_rgb(232, 192, 56);
pub const RED: Color32 = Color32::from_rgb(232, 64, 56);
/// Scale labels, chosen so they never crowd: denser at the top where it matters.
const TICKS: [f32; 9] = [-60.0, -48.0, -36.0, -24.0, -18.0, -12.0, -6.0, -3.0, 0.0];

/// Per-channel smoothed meter state, kept between frames.
#[derive(Clone, Copy)]
struct Ballistics {
    level_db: f32,
    rms_db: f32,
    hold_db: f32,
    hold_at: Option<Instant>,
}

impl Default for Ballistics {
    fn default() -> Self {
        Self { level_db: SILENCE_DB, rms_db: SILENCE_DB, hold_db: SILENCE_DB, hold_at: None }
    }
}

impl Ballistics {
    fn update(&mut self, peak: f32, rms: f32, now: Instant, dt: f32) {
        let db = to_db(peak);
        self.level_db = if db > self.level_db { db } else { (self.level_db - FALL_DB_PER_S * dt).max(db) };
        // RMS already averages over the frame; a light smoothing keeps it steady.
        let r = to_db(rms);
        self.rms_db = if r > self.rms_db { r } else { (self.rms_db - FALL_DB_PER_S * dt).max(r) };
        if db >= self.hold_db || self.hold_at.is_none_or(|t| now - t > PEAK_HOLD) {
            self.hold_db = db;
            self.hold_at = Some(now);
        }
    }
}

/// One meter's state: two channels, a clip latch and the numeric peak readout.
#[derive(Clone)]
pub struct MeterState {
    ch: [Ballistics; 2],
    /// Latched until clicked.
    clipped: bool,
    /// Highest peak over the last few seconds, for the readout.
    readout_db: f32,
    readout_at: Option<Instant>,
}

impl Default for MeterState {
    /// Silent. (A derived default would read 0 dB: a full meter that takes
    /// seconds to fall.)
    fn default() -> Self {
        Self { ch: [Ballistics::default(); 2], clipped: false, readout_db: SILENCE_DB, readout_at: None }
    }
}

impl MeterState {
    pub fn update(&mut self, levels: Levels, now: Instant, dt: f32) {
        for c in 0..2 {
            self.ch[c].update(levels.peak[c], levels.rms[c], now, dt);
        }
        let peak = levels.max_peak();
        if peak >= 0.999 {
            self.clipped = true;
        }
        let db = to_db(peak);
        if db >= self.readout_db || self.readout_at.is_none_or(|t| now - t > READOUT_HOLD) {
            self.readout_db = db;
            self.readout_at = Some(now);
        }
    }

    pub fn clipped(&self) -> bool {
        self.clipped
    }

    pub fn reset_clip(&mut self) {
        self.clipped = false;
    }

    /// The recent peak as text: "-12.4" / "-∞".
    pub fn readout(&self) -> String {
        if self.readout_db <= FLOOR_DB { "-∞".to_owned() } else { format!("{:.1}", self.readout_db) }
    }

    pub fn readout_color(&self, v: &egui::Visuals) -> Color32 {
        if self.readout_db >= -0.05 {
            RED
        } else if self.readout_db >= RED_FROM {
            YELLOW
        } else {
            v.text_color()
        }
    }
}

/// How a meter is drawn.
pub struct Style {
    /// Height of each channel bar.
    pub bar_h: f32,
    /// Gap between the L and R bars.
    pub gap: f32,
    /// Draw dB labels under the bars.
    pub scale: bool,
    /// Dim the bars (source muted: shows the signal is there, but not recorded).
    pub dimmed: bool,
}

/// Draw a stereo meter filling the available width. Returns whether the clip
/// light was clicked (to reset it).
pub fn stereo(ui: &mut egui::Ui, id: egui::Id, state: &MeterState, style: &Style) -> bool {
    let clip_w = 12.0;
    let scale_h = if style.scale { 14.0 } else { 0.0 };
    let bars_h = style.bar_h * 2.0 + style.gap;
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), bars_h + scale_h), Sense::hover());
    let bars = Rect::from_min_size(rect.min, vec2(rect.width() - clip_w - 6.0, bars_h));
    let p = ui.painter_at(rect.expand(2.0));
    let v = ui.visuals();
    let x_of = |db: f32| bars.left() + bars.width() * scale_pos(db);

    // Faint zone backgrounds and tick lines, so the scale reads even when silent.
    for c in 0..2 {
        let bar = Rect::from_min_size(pos2(bars.left(), bars.top() + c as f32 * (style.bar_h + style.gap)), vec2(bars.width(), style.bar_h));
        p.rect_filled(bar, 0.0, v.extreme_bg_color);
        for (from, to, color) in zones() {
            let zone = Rect::from_x_y_ranges(x_of(from)..=x_of(to), bar.y_range());
            p.rect_filled(zone, 0.0, color.gamma_multiply(0.10));
        }
        let ch = &state.ch[c];
        let alpha = if style.dimmed { 0.35 } else { 1.0 };
        // Peak (bright), then RMS (solid) on top, both clipped to the zones with
        // square edges.
        fill_zones(&p, bar, x_of, ch.level_db, 0.55 * alpha);
        fill_zones(&p, bar, x_of, ch.rms_db, alpha);
        if ch.hold_db > FLOOR_DB {
            let x = x_of(ch.hold_db).min(bar.right() - 1.0);
            let color = zone_color(ch.hold_db).gamma_multiply(alpha.max(0.6));
            p.rect_filled(Rect::from_x_y_ranges(x..=x + 2.0, bar.y_range()), 0.0, color);
        }
    }
    // Tick marks across both bars at the labelled points.
    for db in TICKS {
        let x = x_of(db).round();
        p.line_segment([pos2(x, bars.top()), pos2(x, bars.bottom())], Stroke::new(1.0, v.panel_fill.gamma_multiply(0.55)));
    }
    if style.scale {
        let font = FontId::proportional(10.0);
        for db in TICKS {
            let x = x_of(db);
            let label = if db == 0.0 { "0".to_owned() } else { format!("{db:.0}") };
            let align = if db == FLOOR_DB { Align2::LEFT_TOP } else if db == 0.0 { Align2::RIGHT_TOP } else { Align2::CENTER_TOP };
            p.text(pos2(x, bars.bottom() + 2.0), align, label, font.clone(), v.weak_text_color());
        }
    }

    // Clip light: latches until clicked.
    let clip = Rect::from_min_size(pos2(bars.right() + 6.0, bars.top()), vec2(clip_w, bars_h));
    let r = ui.interact(clip, id.with("clip"), Sense::click());
    let fill = if state.clipped { RED } else { v.extreme_bg_color };
    p.rect_filled(clip, 2.0, fill);
    if !state.clipped {
        p.rect_stroke(clip, 2.0, Stroke::new(1.0, RED.gamma_multiply(0.35)), egui::StrokeKind::Inside);
    }
    let tip = if state.clipped { "Clipped: a sample hit full scale and distorted. Click to reset." } else { "Lights up if the audio clips (distorts)." };
    r.on_hover_text(tip).clicked() && state.clipped
}

/// Where `db` sits along a meter, 0..=1. Piecewise, like hardware meters: the top
/// 18 dB, where levels are set, gets half the length; the quiet end is compressed.
fn scale_pos(db: f32) -> f32 {
    const POINTS: [(f32, f32); 9] =
        [(-60.0, 0.0), (-48.0, 0.1), (-36.0, 0.22), (-24.0, 0.38), (-18.0, 0.5), (-12.0, 0.64), (-6.0, 0.8), (-3.0, 0.9), (0.0, 1.0)];
    if db <= POINTS[0].0 {
        return 0.0;
    }
    for w in POINTS.windows(2) {
        let ((d0, p0), (d1, p1)) = (w[0], w[1]);
        if db <= d1 {
            return p0 + (p1 - p0) * (db - d0) / (d1 - d0);
        }
    }
    1.0
}

/// Paint a bar from the floor up to `db`, split into the zone colours at exact
/// dB marks. Square corners: rounded segments make the colour joins look notched.
fn fill_zones(p: &egui::Painter, bar: Rect, x_of: impl Fn(f32) -> f32, db: f32, alpha: f32) {
    if db <= FLOOR_DB {
        return;
    }
    let end = x_of(db);
    for (from, to, color) in zones() {
        let (x0, x1) = (x_of(from), x_of(to).min(end));
        if x1 > x0 {
            p.rect_filled(Rect::from_x_y_ranges(x0..=x1, bar.y_range()), 0.0, color.gamma_multiply(alpha));
        }
    }
}

fn zones() -> [(f32, f32, Color32); 3] {
    [(FLOOR_DB, YELLOW_FROM, GREEN), (YELLOW_FROM, RED_FROM, YELLOW), (RED_FROM, 0.0, RED)]
}

fn zone_color(db: f32) -> Color32 {
    if db >= RED_FROM {
        RED
    } else if db >= YELLOW_FROM {
        YELLOW
    } else {
        GREEN
    }
}

/// A small single-bar gain-reduction meter for the limiter: how many dB it's
/// pulling the mix down, growing from the right.
pub fn reduction(ui: &mut egui::Ui, db: f32, width: f32) -> egui::Response {
    let h = 8.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 14.0), Sense::hover());
    let bar = Rect::from_min_size(rect.left_center() - vec2(0.0, h / 2.0), vec2(width, h));
    let p = ui.painter();
    p.rect_filled(bar, 0.0, ui.visuals().extreme_bg_color);
    // 0..12 dB of reduction across the bar.
    let f = (db / 12.0).clamp(0.0, 1.0);
    if f > 0.0 {
        let x = bar.right() - bar.width() * f;
        p.rect_filled(Rect::from_x_y_ranges(x..=bar.right(), bar.y_range()), 0.0, YELLOW);
    }
    resp
}

pub fn to_db(x: f32) -> f32 {
    if x <= 1e-6 { SILENCE_DB } else { 20.0 * x.log10() }
}

/// Paint a label right-aligned in a fixed-width slot, so changing digits don't
/// shift anything.
pub fn fixed_label(ui: &mut egui::Ui, width: f32, text: &str, color: Color32, tip: &str) {
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 18.0), Sense::hover());
    ui.painter().text(rect.right_center(), Align2::RIGHT_CENTER, text, FontId::monospace(12.0), color);
    resp.on_hover_text(tip);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_is_monotonic_and_anchored() {
        assert_eq!(scale_pos(-80.0), 0.0);
        assert_eq!(scale_pos(0.0), 1.0);
        assert_eq!(scale_pos(3.0), 1.0);
        assert!((scale_pos(-18.0) - 0.5).abs() < 1e-6);
        let mut last = -1.0;
        for i in 0..=600 {
            let p = scale_pos(-60.0 + i as f32 * 0.1);
            assert!(p >= last);
            last = p;
        }
    }
}
