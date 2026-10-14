//! Level meters: stereo bars on a dBFS scale, the way a mixing desk shows them.
//!
//! Each channel draws its peak level as a bright bar (instant rise, smooth fall),
//! the RMS (average loudness) as a solid bar inside it, and a held peak line.
//! The scale gives the top 18 dB half the bar, where levels actually get set.
//! Colours are zones on a fixed scale — green up to -18 dBFS (where speech and
//! game audio should sit), yellow up to -6, red above — clipped to the bar, so
//! the bar changes colour at exact dB marks with square edges. Each channel
//! has a clip light that lights when a sample reaches full scale, stays a few
//! seconds after the last time and then fades: no need to reset it.

use std::time::{Duration, Instant};

use capture::mixer::Levels;
use egui::{Align2, Color32, Rect, Sense, Stroke, pos2, vec2};

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
/// How long a clip light stays lit after the last sample that distorted, and
/// how long it then takes to fade.
const CLIP_HOLD: Duration = Duration::from_secs(3);
const CLIP_FADE: Duration = Duration::from_millis(600);

pub const GREEN: Color32 = crate::theme::DARK.meter_g;
pub const YELLOW: Color32 = crate::theme::DARK.meter_y;
pub const RED: Color32 = crate::theme::DARK.meter_r;
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

/// One meter's state: two channels, their clip lights and the numeric peak
/// readout.
#[derive(Clone)]
pub struct MeterState {
    ch: [Ballistics; 2],
    /// When each channel (left, right) last distorted.
    clipped_at: [Option<Instant>; 2],
    /// Highest peak over the last few seconds, for the readout.
    readout_db: f32,
    readout_at: Option<Instant>,
}

impl Default for MeterState {
    /// Silent. (A derived default would read 0 dB: a full meter that takes
    /// seconds to fall.)
    fn default() -> Self {
        Self { ch: [Ballistics::default(); 2], clipped_at: [None; 2], readout_db: SILENCE_DB, readout_at: None }
    }
}

impl MeterState {
    pub fn update(&mut self, levels: Levels, now: Instant, dt: f32) {
        for c in 0..2 {
            self.ch[c].update(levels.peak[c], levels.rms[c], now, dt);
        }
        let peak = levels.max_peak();
        for c in 0..2 {
            if levels.peak[c] >= 0.999 {
                self.clipped_at[c] = Some(now);
            }
        }
        let db = to_db(peak);
        if db >= self.readout_db || self.readout_at.is_none_or(|t| now - t > READOUT_HOLD) {
            self.readout_db = db;
            self.readout_at = Some(now);
        }
    }

    /// How lit a channel's clip light is: 1 for a few seconds after it
    /// distorted, then fading to 0.
    fn clip_light(&self, c: usize) -> f32 {
        let Some(at) = self.clipped_at[c] else { return 0.0 };
        let since = at.elapsed().saturating_sub(CLIP_HOLD);
        1.0 - (since.as_secs_f32() / CLIP_FADE.as_secs_f32()).min(1.0)
    }

    /// Either channel distorted in the last few seconds (its light is lit).
    pub fn clipped(&self) -> bool {
        (0..2).any(|c| self.clip_light(c) > 0.0)
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

/// Draw a stereo meter filling the available width, each channel's clip
/// light at the end of its bar.
pub fn stereo(ui: &mut egui::Ui, id: egui::Id, state: &MeterState, style: &Style) {
    let clip_w = style.bar_h.max(7.0);
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
        p.rect_filled(bar, 2.0, crate::theme::of(v).meter_bg);
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
        let font = crate::theme::font(10.5, crate::theme::W::Regular);
        for db in TICKS {
            let x = x_of(db);
            let label = if db == 0.0 { "0".to_owned() } else { format!("{db:.0}") };
            let align = if db == FLOOR_DB { Align2::LEFT_TOP } else if db == 0.0 { Align2::RIGHT_TOP } else { Align2::CENTER_TOP };
            p.text(pos2(x, bars.bottom() + 3.0), align, label.replace('-', "−"), font.clone(), v.weak_text_color());
        }
    }

    // Each channel's clip light, at the end of its bar.
    let lights = Rect::from_min_size(pos2(bars.right() + 6.0, bars.top()), vec2(clip_w, bars_h));
    for c in 0..2 {
        let light = Rect::from_min_size(pos2(lights.left(), bars.top() + c as f32 * (style.bar_h + style.gap)), vec2(clip_w, style.bar_h));
        clip_light(&p, light, state.clip_light(c), crate::theme::of(v).meter_bg);
    }
    ui.interact(lights.expand(3.0), id.with("clip"), Sense::hover()).on_hover_text(clip_tip(state));
    if state.clipped() {
        ui.ctx().request_repaint_after(Duration::from_millis(16));
    }
}

/// A clip light, lit by `lit` (0 to 1): red, or a faint red outline when out.
fn clip_light(p: &egui::Painter, light: Rect, lit: f32, bg: Color32) {
    p.rect_filled(light, 2.0, crate::theme::mix(bg, RED, lit));
    if lit < 1.0 {
        p.rect_stroke(light, 2.0, Stroke::new(1.0, RED.gamma_multiply(0.35 * (1.0 - lit))), egui::StrokeKind::Inside);
    }
}

fn clip_tip(state: &MeterState) -> &'static str {
    if state.clipped() { "Too loud: a sample hit full scale and distorted (left, right)." } else { "Light up if the sound gets too loud and distorts, left and right." }
}

/// A stereo meter standing up in `rect`: the readout of the recent peak on
/// top, the clip light under it, then two bars (left, right) with the dB
/// scale beside them; peak, average and held peak as on [`stereo`].
pub fn stereo_vertical(ui: &mut egui::Ui, rect: Rect, id: egui::Id, state: &MeterState) {
    let v = ui.visuals().clone();
    let pal = crate::theme::of(&v);
    let p = ui.painter_at(rect.expand(2.0));
    // The readout, then the clip light.
    let readout = Rect::from_min_size(rect.min, vec2(rect.width(), 16.0));
    p.text(readout.center(), Align2::CENTER_CENTER, state.readout().replace('-', "−"), crate::theme::font(11.0, crate::theme::W::Medium), state.readout_color(&v));
    // The bars, on the right, each with its clip light on top; the scale's
    // labels on their left.
    let (bar_w, gap) = (7.0, 3.0);
    let lights_y = readout.bottom() + 4.0;
    let bars = Rect::from_min_max(pos2(rect.right() - 5.0 - bar_w * 2.0 - gap, lights_y + bar_w + 4.0), pos2(rect.right() - 5.0, rect.bottom() - 2.0));
    let lights = Rect::from_min_max(pos2(bars.left(), lights_y), pos2(bars.right(), lights_y + bar_w));
    for c in 0..2 {
        let light = Rect::from_min_size(pos2(bars.left() + c as f32 * (bar_w + gap), lights_y), vec2(bar_w, bar_w));
        clip_light(&p, light, state.clip_light(c), pal.meter_bg);
    }
    ui.interact(lights.expand(3.0), id.with("clip"), Sense::hover()).on_hover_text(clip_tip(state));
    if state.clipped() {
        ui.ctx().request_repaint_after(Duration::from_millis(16));
    }
    if bars.height() < 20.0 {
        return;
    }
    let y_of = |db: f32| bars.bottom() - bars.height() * scale_pos(db);
    for c in 0..2 {
        let bar = Rect::from_min_size(pos2(bars.left() + c as f32 * (bar_w + gap), bars.top()), vec2(bar_w, bars.height()));
        p.rect_filled(bar, 2.0, pal.meter_bg);
        for (from, to, color) in zones() {
            p.rect_filled(Rect::from_x_y_ranges(bar.x_range(), y_of(to)..=y_of(from)), 0.0, color.gamma_multiply(0.10));
        }
        let ch = &state.ch[c];
        let fill = |db: f32, alpha: f32| {
            if db <= FLOOR_DB {
                return;
            }
            let top = y_of(db);
            for (from, to, color) in zones() {
                let (y0, y1) = (y_of(to).max(top), y_of(from));
                if y1 > y0 {
                    p.rect_filled(Rect::from_x_y_ranges(bar.x_range(), y0..=y1), 0.0, color.gamma_multiply(alpha));
                }
            }
        };
        fill(ch.level_db, 0.55);
        fill(ch.rms_db, 1.0);
        if ch.hold_db > FLOOR_DB {
            let y = y_of(ch.hold_db).max(bar.top() + 1.0);
            p.rect_filled(Rect::from_x_y_ranges(bar.x_range(), y - 2.0..=y), 0.0, zone_color(ch.hold_db));
        }
    }
    // The scale: ticks across both bars, labels to the left where they fit.
    let font = crate::theme::font(10.0, crate::theme::W::Regular);
    let mut last_y = f32::INFINITY;
    for db in TICKS.iter().rev() {
        let y = y_of(*db).round();
        p.line_segment([pos2(bars.left(), y), pos2(bars.right(), y)], Stroke::new(1.0, v.panel_fill.gamma_multiply(0.55)));
        if (last_y - y).abs() >= 12.0 || last_y.is_infinite() {
            let label = if *db == 0.0 { "0".to_owned() } else { format!("{db:.0}").replace('-', "−") };
            p.text(pos2(bars.left() - 5.0, y), Align2::RIGHT_CENTER, label, font.clone(), v.weak_text_color());
            last_y = y;
        }
    }
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

/// A source's level drawn along its fader's track (`x_of`: where a dB value
/// sits on it): peak and average in the meter's colours, a held peak line.
pub fn fader_meter(p: &egui::Painter, track: Rect, state: &MeterState, dimmed: bool, x_of: impl Fn(f32) -> f32, pal: &crate::theme::Pal) {
    p.rect_filled(track, 3, pal.meter_bg);
    let painter = p.with_clip_rect(track);
    let alpha = if dimmed { 0.35 } else { 1.0 };
    let zones = [(FLOOR_DB, YELLOW_FROM, pal.meter_g), (YELLOW_FROM, RED_FROM, pal.meter_y), (RED_FROM, 12.0, pal.meter_r)];
    // Both channels as one bar: the louder of the two.
    let level = state.ch[0].level_db.max(state.ch[1].level_db);
    let rms = state.ch[0].rms_db.max(state.ch[1].rms_db);
    let hold = state.ch[0].hold_db.max(state.ch[1].hold_db);
    for (from, to, color) in zones {
        painter.rect_filled(Rect::from_x_y_ranges(x_of(from)..=x_of(to), track.y_range()), 0, color.gamma_multiply(0.14));
    }
    for (db, a) in [(level, 0.5 * alpha), (rms, alpha)] {
        if db <= FLOOR_DB {
            continue;
        }
        let end = x_of(db);
        for (from, to, color) in zones {
            let (x0, x1) = (x_of(from).max(track.left()), x_of(to).min(end));
            if x1 > x0 {
                painter.rect_filled(Rect::from_x_y_ranges(x0..=x1, track.y_range()), 0, color.gamma_multiply(a));
            }
        }
    }
    if hold > FLOOR_DB {
        let x = x_of(hold);
        let color = zones.iter().find(|(f, t, _)| hold >= *f && hold < *t).map_or(pal.meter_r, |z| z.2);
        painter.rect_filled(Rect::from_x_y_ranges(x..=x + 2.0, track.y_range()), 0, color.gamma_multiply(alpha.max(0.6)));
    }
}

/// A small single-bar gain-reduction meter for the limiter: how many dB it's
/// pulling the mix down, growing from the right.
pub fn reduction(ui: &mut egui::Ui, db: f32, width: f32) -> egui::Response {
    let h = 8.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 14.0), Sense::hover());
    let bar = Rect::from_min_size(rect.left_center() - vec2(0.0, h / 2.0), vec2(width, h));
    let p = ui.painter();
    p.rect_filled(bar, 2.0, crate::theme::pal(ui).meter_bg);
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
    ui.painter().text(rect.right_center(), Align2::RIGHT_CENTER, text, crate::theme::font(12.5, crate::theme::W::Medium), color);
    resp.on_hover_text(tip);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clip light lights per channel, stays lit a few seconds after the
    /// last time and then fades out by itself.
    #[test]
    fn a_clip_light_goes_out_by_itself() {
        let mut m = MeterState::default();
        let now = Instant::now();
        m.update(Levels { peak: [1.0, 0.5], rms: [0.5, 0.2] }, now, 0.016);
        assert_eq!((m.clip_light(0), m.clip_light(1)), (1.0, 0.0));
        m.clipped_at[0] = Some(now - CLIP_HOLD + Duration::from_millis(100));
        assert_eq!(m.clip_light(0), 1.0, "still held");
        m.clipped_at[0] = Some(now - CLIP_HOLD - CLIP_FADE / 2);
        assert!(m.clip_light(0) > 0.2 && m.clip_light(0) < 0.8, "fading");
        m.clipped_at[0] = Some(now - CLIP_HOLD - CLIP_FADE);
        assert_eq!(m.clip_light(0), 0.0);
        assert!(!m.clipped());
    }

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
