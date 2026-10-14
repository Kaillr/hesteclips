//! What the player and the editor share, so they read as one screen: the
//! header (back and the clip's name on the left, anything centred, actions on
//! the right), the stage (the picture on a dark surround that picks up its
//! colours), the transport bar (play between previous and next moment, time,
//! tools), and the ruler of the timeline.
//!
//! Watching and editing put the same pieces in the same order: picture,
//! transport, ruler, filmstrip, sound. Editing adds a lane per source and the
//! trim handles; nothing moves.

use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2, vec2};

use crate::theme::{self, W, icon};
use crate::ui_kit::{self, Btn, Kind};

/// The header's height.
pub const HEAD_H: f32 = 52.0;
/// The transport bar's height.
pub const BAR_H: f32 = 52.0;
/// The timeline ruler's height.
pub const RULER_H: f32 = 26.0;

/// A part of the header or the transport bar, for the closure that fills it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Left,
    Centre,
    Right,
}

/// The header. `title`: the clip's name, renamed by clicking it (or F2).
/// `part` fills the centre (on the window's true centre: previous / next
/// clip) and the right (the actions, laid out right to left). Returns whether
/// back or the name were clicked.
pub fn header(ui: &mut egui::Ui, title: &str, unsaved: bool, centre_w: f32, mut part: impl FnMut(&mut egui::Ui, Part)) -> (bool, bool) {
    let p = theme::pal(ui);
    let rect = Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), HEAD_H));
    ui.allocate_rect(rect, Sense::hover());
    crate::shell::drag_window_area(ui, rect, "viewer_head");
    // Clear of the traffic lights on macOS.
    let left_pad = if crate::shell::under_titlebar() { 82.0 } else { 12.0 };
    // The actions first: they're pinned right and capped to their half, so the
    // name gets what's left.
    let half = rect.width() / 2.0;
    let right_max = if centre_w > 0.0 { half - centre_w / 2.0 - 16.0 } else { rect.width() - left_pad - 120.0 };
    let right_rect = Rect::from_min_max(Pos2::new(rect.right() - 12.0 - right_max.max(60.0), rect.top()), Pos2::new(rect.right() - 12.0, rect.bottom()));
    let mut rui = ui.new_child(egui::UiBuilder::new().max_rect(right_rect).layout(egui::Layout::right_to_left(egui::Align::Center)));
    rui.spacing_mut().item_spacing.x = 6.0;
    part(&mut rui, Part::Right);
    let used_right = rui.min_rect().width();
    if centre_w > 0.0 {
        let c = Rect::from_center_size(rect.center(), vec2(centre_w, HEAD_H));
        let mut cui = ui.new_child(egui::UiBuilder::new().max_rect(c).layout(egui::Layout::left_to_right(egui::Align::Center)));
        part(&mut cui, Part::Centre);
    }
    // Back and the name: the name runs to the centre (or the actions) and only
    // then shortens.
    let name_right = if centre_w > 0.0 { rect.center().x - centre_w / 2.0 - 16.0 } else { rect.right() - 12.0 - used_right - 16.0 };
    let left = Rect::from_min_max(Pos2::new(rect.left() + left_pad, rect.top()), Pos2::new(name_right.max(rect.left() + left_pad + 60.0), rect.bottom()));
    let mut lui = ui.new_child(egui::UiBuilder::new().max_rect(left).layout(egui::Layout::left_to_right(egui::Align::Center)));
    lui.spacing_mut().item_spacing.x = 6.0;
    let back = Btn::icon(Kind::Ghost, icon::CARET_LEFT).tip("Back  (Esc)").show(&mut lui).clicked();
    // The name: a quiet box on hover, a pencil appearing.
    let font = theme::font(14.5, W::Semibold);
    let room = (lui.available_width() - 12.0 - 20.0 - if unsaved { 13.0 } else { 0.0 }).max(30.0);
    let g = lui.painter().layout_job(ui_kit::single_line_job(title, font, p.text, room));
    let w = g.size().x + 12.0 + 19.0 + if unsaved { 13.0 } else { 0.0 };
    let (r, resp) = lui.allocate_exact_size(vec2(w, 30.0), Sense::click());
    let hv = theme::toggle(lui.ctx(), resp.id.with("hv"), resp.hovered(), theme::FAST);
    lui.painter().rect_filled(r, 5, theme::fade(p.sel, hv));
    lui.painter().galley(Pos2::new(r.left() + 6.0, r.center().y - g.size().y / 2.0), g.clone(), p.text);
    let mut x = r.left() + 6.0 + g.size().x + 6.0;
    if unsaved {
        lui.painter().circle_filled(Pos2::new(x + 3.5, r.center().y), 3.5, p.text2);
        x += 13.0;
    }
    lui.painter().text(Pos2::new(x + 6.0, r.center().y), Align2::CENTER_CENTER, icon::PENCIL_SIMPLE, FontId::proportional(13.0), theme::fade(p.text3, hv));
    let renamed = resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(format!("{title}\nRename  (F2)")).clicked();
    (back, renamed)
}

/// The stage: the picture fitted into `rect` on whole pixels, with its own
/// colours glowing softly in the surround (an ambient TV). `glow`: a small
/// picture of the frame, stretched and faded behind. `paint` draws the
/// picture into the rect it's given. Returns where the picture is.
pub fn stage(ui: &egui::Ui, rect: Rect, size: Option<Vec2>, glow: Option<&egui::TextureHandle>, round: bool, paint: impl FnOnce(Rect)) -> Option<Rect> {
    let p = theme::pal(ui);
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, if round { 0 } else { 0 }, p.stage);
    let size = size?;
    let scale = (rect.width() / size.x).min(rect.height() / size.y);
    let ppp = ui.ctx().pixels_per_point();
    let r = Rect::from_center_size(rect.center(), size * scale);
    let r = Rect::from_min_size(((r.min.to_vec2() * ppp).round() / ppp).to_pos2(), r.size());
    if let Some(tex) = glow {
        // The frame's colours, spread wide: the picture stretched past the
        // stage and laid down at low opacity in rings that fade outwards
        // (egui can't blur; spreading a tiny picture does nearly the same).
        let opacity = if p.dark { 0.55 } else { 0.4 };
        for (grow, a) in [(1.25, 0.35), (1.12, 0.45), (1.03, 0.6)] {
            let g = Rect::from_center_size(rect.center(), rect.size() * grow);
            painter.image(tex.id(), g, Rect::from_min_max(Pos2::new(0.1, 0.1), Pos2::new(0.9, 0.9)), Color32::from_white_alpha((255.0 * a * opacity) as u8));
        }
        // Darken towards the edges so the glow sits behind, not on top.
        let edge = theme::fade(p.stage, 0.55);
        painter.rect_filled(rect, 0, edge);
        // A soft shadow under the picture.
        let shadow = egui::Shadow { offset: [0, 10], blur: 40, spread: 0, color: Color32::from_black_alpha(115) };
        painter.add(shadow.as_shape(r, 2));
    }
    painter.rect_filled(r, 0, Color32::BLACK);
    paint(r);
    Some(r)
}

/// A moment on the timeline: a kill or your own marker.
#[derive(Clone)]
pub struct Moment {
    pub t: f64,
    pub label: String,
    pub marker: bool,
}

/// What the transport bar's centre asks for.
#[derive(Default)]
pub struct Centre {
    pub play: bool,
    pub prev: bool,
    pub next: bool,
}

/// Play between "previous moment" and "next moment" (greyed when there's
/// none; their tips name the moment).
pub fn centre(ui: &mut egui::Ui, playing: bool, prev: Option<&Moment>, next: Option<&Moment>) -> Centre {
    let p = theme::pal(ui);
    let mut c = Centre::default();
    let tip = |m: Option<&Moment>, dir: &str, key: &str| match m {
        Some(m) => format!("{dir} to {}  ({key})", m.label.to_lowercase()),
        None => format!("No {} moment", if dir == "Back" { "earlier" } else { "later" }),
    };
    c.prev = Btn::icon(Kind::Ghost, egui_phosphor::fill::SKIP_BACK).enabled(prev.is_some()).tip(tip(prev, "Back", "[")).show(ui).clicked();
    // Play: a round button in the main action's colour; the glyph cross-fades.
    let (r, resp) = ui.allocate_exact_size(vec2(36.0, 36.0), Sense::click());
    let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered(), theme::FAST);
    let down = resp.is_pointer_button_down_on();
    let rr = if down { r.shrink(1.0) } else { r };
    ui.painter().circle_filled(rr.center(), rr.width() / 2.0, theme::mix(p.primary, if p.dark { Color32::WHITE } else { Color32::from_gray(60) }, hv * 0.3));
    let t = theme::toggle(ui.ctx(), resp.id.with("playing"), playing, theme::FAST);
    let fg = p.primary_fg;
    if t < 1.0 {
        // ▶, nudged right to look centred.
        ui.painter().text(rr.center() + vec2(1.5, 0.0), Align2::CENTER_CENTER, egui_phosphor::fill::PLAY, FontId::new(16.0 * (1.0 - 0.3 * t), theme::fill_family()), theme::fade(fg, 1.0 - t));
    }
    if t > 0.0 {
        ui.painter().text(rr.center(), Align2::CENTER_CENTER, egui_phosphor::fill::PAUSE, FontId::new(16.0 * (0.7 + 0.3 * t), theme::fill_family()), theme::fade(fg, t));
    }
    c.play = resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Play or pause  (Space)").clicked();
    c.next = Btn::icon(Kind::Ghost, egui_phosphor::fill::SKIP_FORWARD).enabled(next.is_some()).tip(tip(next, "Ahead", "]")).show(ui).clicked();
    c
}

/// The time: "0:22.13 / 0:59.80", or frames. Click to switch. Fixed-width
/// digits, so it doesn't jitter.
pub fn timecode(ui: &mut egui::Ui, t: f64, total: f64, fps: f64, frames: &mut bool) {
    let p = theme::pal(ui);
    let (now, all) = if *frames {
        let f = |s: f64| (s * fps).round() as u64;
        (f(t).to_string(), format!("/ {} frames", f(total)))
    } else {
        (clock(t), format!("/ {}", clock(total)))
    };
    let a = ui.painter().layout_no_wrap(now, theme::font(14.0, W::Medium), p.text);
    let b = ui.painter().layout_no_wrap(all, theme::font(14.0, W::Regular), p.text3);
    let w = a.size().x + 6.0 + b.size().x + 16.0;
    let id = ui.next_auto_id();
    let w = theme::value(ui.ctx(), id.with("w"), w, theme::NORMAL);
    let (r, resp) = ui.allocate_exact_size(vec2(w, 30.0), Sense::click());
    let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered(), theme::FAST);
    let painter = ui.painter_at(r);
    painter.rect_filled(r, 5, theme::fade(p.sel, hv));
    painter.galley(Pos2::new(r.left() + 8.0, r.center().y - a.size().y / 2.0), a.clone(), p.text);
    painter.galley(Pos2::new(r.left() + 8.0 + a.size().x + 6.0, r.center().y - b.size().y / 2.0), b, p.text3);
    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Click to switch between time and frames").clicked() {
        *frames = !*frames;
    }
}

/// "0:22.13" (minutes, seconds and hundredths), "1:02:33.10" past an hour.
pub fn clock(t: f64) -> String {
    let t = t.max(0.0);
    let cs = (t * 100.0).floor() as u64;
    let (h, m, s, c) = (cs / 360_000, cs / 6000 % 60, cs / 100 % 60, cs % 100);
    if h > 0 { format!("{h}:{m:02}:{s:02}.{c:02}") } else { format!("{m}:{s:02}.{c:02}") }
}

/// The ruler over the timeline: ticks and times for `view` (start, length in
/// seconds), and the moments as pins. Returns a moment clicked.
pub fn ruler(ui: &mut egui::Ui, rect: Rect, view: (f64, f64), fps: f64, frames: bool, moments: &[Moment]) -> Option<f64> {
    let p = theme::pal(ui);
    let painter = ui.painter_at(rect.expand2(vec2(10.0, 0.0)));
    let (v0, span) = view;
    let x_of = |t: f64| rect.left() + ((t - v0) / span.max(1e-9)) as f32 * rect.width();
    // A label every so many seconds, at least ~70 points apart.
    let px_per_s = rect.width() as f64 / span.max(1e-9);
    let steps = [1.0 / fps, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0];
    let step = steps.into_iter().find(|s| s * px_per_s >= 70.0).unwrap_or(1200.0);
    let minor = if step >= 1.0 { 5 } else { 2 };
    let mut t = (v0 / step).floor() * step;
    while t <= v0 + span + 1e-9 {
        let x = x_of(t);
        if x >= rect.left() - 1.0 {
            painter.vline(x, rect.bottom() - 8.0..=rect.bottom() - 4.0, Stroke::new(1.0, theme::fade(p.text3, 0.6)));
            let label = if frames { format!("{}", (t * fps).round() as u64) } else { short_clock(t, step) };
            painter.text(Pos2::new(x + 4.0, rect.bottom() - 13.0), Align2::LEFT_BOTTOM, label, theme::font(11.0, W::Regular), p.text3);
        }
        for k in 1..minor {
            let xm = x + (step * px_per_s) as f32 * k as f32 / minor as f32;
            if xm > rect.left() && xm < rect.right() {
                painter.vline(xm, rect.bottom() - 6.0..=rect.bottom() - 4.0, Stroke::new(1.0, p.line2));
            }
        }
        t += step;
    }
    // Pins: a round mark per moment, raised on hover; click to go there.
    let mut clicked = None;
    for (i, m) in moments.iter().enumerate() {
        let x = x_of(m.t);
        if x < rect.left() - 10.0 || x > rect.right() + 10.0 {
            continue;
        }
        let c = Pos2::new(x, rect.bottom() - 13.0);
        let resp = ui.interact(Rect::from_center_size(c, Vec2::splat(20.0)), ui.id().with(("pin", i)), Sense::click());
        let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered(), theme::FAST);
        let r = 10.0 * (1.0 + 0.12 * hv);
        let (fill, fg) = if m.marker { (p.warn, Color32::from_rgb(42, 30, 6)) } else { (p.text, p.base) };
        painter.circle_filled(c, r + 2.0, p.base);
        painter.circle_filled(c, r, fill);
        let glyph = if m.marker { egui_phosphor::fill::FLAG } else { icon::CROSSHAIR_SIMPLE };
        painter.text(c, Align2::CENTER_CENTER, glyph, FontId::new(11.0, if m.marker { theme::fill_family() } else { egui::FontFamily::Proportional }), fg);
        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(&m.label).clicked() {
            clicked = Some(m.t);
        }
    }
    clicked
}

fn short_clock(t: f64, step: f64) -> String {
    let t = t.max(0.0);
    if step < 1.0 {
        format!("{}:{:05.2}", (t / 60.0).floor() as u64, t % 60.0)
    } else {
        let s = t.round() as u64;
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// The playhead: a line with a tab at its top.
pub fn playhead(painter: &egui::Painter, x: f32, top: f32, bottom: f32, color: Color32) {
    painter.vline(x, top + 4.0..=bottom, Stroke::new(2.0, color));
    let pts = vec![Pos2::new(x - 6.0, top - 2.0), Pos2::new(x + 6.0, top - 2.0), Pos2::new(x + 6.0, top + 3.5), Pos2::new(x, top + 8.0), Pos2::new(x - 6.0, top + 3.5)];
    painter.add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
}

/// A thin vertical divider in a row of buttons.
pub fn vrule(ui: &mut egui::Ui) {
    let p = theme::pal(ui);
    let (r, _) = ui.allocate_exact_size(vec2(17.0, 18.0), Sense::hover());
    ui.painter().vline(r.center().x, r.y_range(), Stroke::new(1.0, p.line2));
}

/// The transport bar's frame: a hairline on top, three columns (left, the
/// centre on the bar's true centre, right).
pub fn bar(ui: &mut egui::Ui, rect: Rect, centre_w: f32, mut part: impl FnMut(&mut egui::Ui, Part)) {
    let p = theme::pal(ui);
    ui.painter().hline(rect.x_range(), rect.top(), Stroke::new(1.0, p.line));
    let inner = rect.shrink2(vec2(14.0, 0.0));
    let c = Rect::from_center_size(inner.center(), vec2(centre_w, rect.height()));
    let l = Rect::from_min_max(inner.min, Pos2::new(c.left() - 8.0, inner.bottom()));
    let r = Rect::from_min_max(Pos2::new(c.right() + 8.0, inner.top()), inner.max);
    let mut lui = ui.new_child(egui::UiBuilder::new().max_rect(l).layout(egui::Layout::left_to_right(egui::Align::Center)));
    lui.spacing_mut().item_spacing.x = 4.0;
    part(&mut lui, Part::Left);
    let mut cui = ui.new_child(egui::UiBuilder::new().max_rect(c).layout(egui::Layout::left_to_right(egui::Align::Center)));
    cui.spacing_mut().item_spacing.x = 6.0;
    part(&mut cui, Part::Centre);
    let mut rui = ui.new_child(egui::UiBuilder::new().max_rect(r).layout(egui::Layout::right_to_left(egui::Align::Center)));
    rui.spacing_mut().item_spacing.x = 4.0;
    part(&mut rui, Part::Right);
}

/// A slim slider: a track filled to `value` (0..=1) and a round knob.
pub fn slider(ui: &mut egui::Ui, value: &mut f32, width: f32) -> egui::Response {
    let p = theme::pal(ui);
    let (rect, mut resp) = ui.allocate_exact_size(vec2(width, 20.0), Sense::click_and_drag());
    if let Some(pos) = resp.interact_pointer_pos()
        && (resp.dragged() || resp.clicked() || resp.drag_started())
    {
        let v = ((pos.x - rect.left() - 7.0) / (rect.width() - 14.0)).clamp(0.0, 1.0);
        if v != *value {
            *value = v;
            resp.mark_changed();
        }
    }
    let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered() || resp.dragged(), theme::FAST);
    let track = Rect::from_min_max(Pos2::new(rect.left() + 7.0, rect.center().y - 2.0), Pos2::new(rect.right() - 7.0, rect.center().y + 2.0));
    ui.painter().rect_filled(track, 2, p.control_h);
    let x = track.left() + track.width() * value.clamp(0.0, 1.0);
    ui.painter().rect_filled(Rect::from_min_max(track.min, Pos2::new(x, track.bottom())), 2, theme::mix(p.text2, p.text, hv));
    let k = 7.0 + hv;
    ui.painter().circle_filled(Pos2::new(x, rect.center().y + 1.0), k, Color32::from_black_alpha(60));
    ui.painter().circle_filled(Pos2::new(x, rect.center().y), k, Color32::WHITE);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_reads_naturally() {
        assert_eq!(clock(0.0), "0:00.00");
        assert_eq!(clock(65.344), "1:05.34");
        assert_eq!(clock(59.999), "0:59.99");
        assert_eq!(clock(3725.5), "1:02:05.50");
    }
}
