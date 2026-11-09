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
    // What's under it starts right at its edge: a gap made it look taller
    // than the pages' headings, its contents high in it.
    let gap = ui.spacing().item_spacing.y;
    ui.allocate_rect(rect, Sense::hover());
    ui.add_space(-gap);
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

/// The stage: the picture fitted into `rect` on whole pixels, the surround
/// lit by its colours, blurred very wide and kept dark (an ambient TV), live
/// with the video. `glow` draws that light over the rect it's given (the
/// picture's shape grown to cover the stage) and says whether it could;
/// `paint` draws the picture. Returns where the picture is.
pub fn stage(ui: &egui::Ui, rect: Rect, size: Option<Vec2>, glow: impl FnOnce(&egui::Painter, Rect) -> bool, paint: impl FnOnce(Rect)) -> Option<Rect> {
    let p = theme::pal(ui);
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0, p.stage);
    let size = size?;
    let scale = (rect.width() / size.x).min(rect.height() / size.y);
    let ppp = ui.ctx().pixels_per_point();
    let r = Rect::from_center_size(rect.center(), size * scale);
    let r = Rect::from_min_size(((r.min.to_vec2() * ppp).round() / ppp).to_pos2(), r.size());
    // The light: the picture's shape grown past the stage on every side, so
    // what's at its edges spreads out to the stage's.
    let cover = (rect.width() / size.x).max(rect.height() / size.y) * 1.15;
    if glow(&painter, Rect::from_center_size(rect.center(), size * cover)) {
        // Kept dark, so it sits behind the picture.
        painter.rect_filled(rect, 0, theme::fade(p.stage, if p.dark { 0.82 } else { 0.7 }));
        // A soft shadow under the picture.
        let shadow = egui::Shadow { offset: [0, 10], blur: 48, spread: 0, color: Color32::from_black_alpha(140) };
        painter.add(shadow.as_shape(r, 2));
    }
    painter.rect_filled(r, 0, Color32::BLACK);
    paint(r);
    Some(r)
}

/// A picture's colours as the stage's light, for one that isn't on the GPU
/// (the same look the GPU makes from a frame there): averaged down to a few
/// dozen pixels in linear light, blurred wide, a little more colourful.
pub fn glow_image(img: &egui::ColorImage) -> egui::ColorImage {
    const W: usize = 64;
    const H: usize = 36;
    let [iw, ih] = img.size;
    let lin = |v: u8| {
        let c = v as f32 / 255.0;
        if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    let mut buf = vec![[0f32; 4]; W * H];
    if iw > 0 && ih > 0 {
        // Each cell the average of 3×3 looks across the part it covers.
        for (i, cell) in buf.iter_mut().enumerate() {
            let (cx, cy) = (i % W, i / W);
            let mut sum = [0f32; 3];
            for j in 0..3 {
                for k in 0..3 {
                    let x = ((cx as f32 + (k as f32 + 0.5) / 3.0) / W as f32 * iw as f32) as usize;
                    let y = ((cy as f32 + (j as f32 + 0.5) / 3.0) / H as f32 * ih as f32) as usize;
                    let p = img.pixels[y.min(ih - 1) * iw + x.min(iw - 1)];
                    sum[0] += lin(p.r());
                    sum[1] += lin(p.g());
                    sum[2] += lin(p.b());
                }
            }
            *cell = [sum[0] / 9.0, sum[1] / 9.0, sum[2] / 9.0, 1.0];
        }
    }
    for _ in 0..3 {
        crate::glass::box_blur(&mut buf, W, H, 4, true);
        crate::glass::box_blur(&mut buf, W, H, 4, false);
    }
    let srgb = |c: f32| {
        let c = c.clamp(0.0, 1.0);
        ((if c <= 0.003_130_8 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 }) * 255.0).round() as u8
    };
    let pixels = buf
        .iter()
        .map(|c| {
            let grey = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
            let s = |v: f32| (grey + (v - grey) * 1.35).max(0.0) * 0.8;
            Color32::from_rgb(srgb(s(c[0])), srgb(s(c[1])), srgb(s(c[2])))
        })
        .collect();
    egui::ColorImage { size: [W, H], pixels, source_size: Vec2::new(W as f32, H as f32) }
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
    let mut c = Centre::default();
    let tip = |m: Option<&Moment>, dir: &str, key: &str| match m {
        Some(m) => format!("{dir} to {}  ({key})", m.label.to_lowercase()),
        None => format!("No {} moment", if dir == "Back" { "earlier" } else { "later" }),
    };
    c.prev = Btn::icon(Kind::Ghost, egui_phosphor::fill::SKIP_BACK).enabled(prev.is_some()).tip(tip(prev, "Back", "[")).show(ui).clicked();
    c.play = play_button(ui, playing).play;
    c.next = Btn::icon(Kind::Ghost, egui_phosphor::fill::SKIP_FORWARD).enabled(next.is_some()).tip(tip(next, "Ahead", "]")).show(ui).clicked();
    c
}

/// Play: a round button in the main action's colour; the glyph cross-fades.
pub fn play_button(ui: &mut egui::Ui, playing: bool) -> Centre {
    let p = theme::pal(ui);
    let mut c = Centre::default();
    let (r, resp) = ui.allocate_exact_size(vec2(36.0, 36.0), Sense::click());
    crate::demo::mark(ui.ctx(), || "play".into(), r);
    crate::demo::playing(ui.ctx(), playing);
    let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered(), theme::FAST);
    let down = resp.is_pointer_button_down_on();
    let rr = if down { r.shrink(1.0) } else { r };
    ui.painter().circle_filled(rr.center(), rr.width() / 2.0, theme::mix(p.primary, if p.dark { Color32::WHITE } else { Color32::from_gray(60) }, hv * 0.3));
    let t = theme::toggle(ui.ctx(), resp.id.with("playing"), playing, theme::FAST);
    let fg = p.primary_fg;
    if t < 1.0 {
        // ▶ centred by its shape: its ink's middle, nudged right a sixth of
        // its width (a triangle looks centred on its centroid).
        glyph_centred(ui.painter(), rr.center(), egui_phosphor::fill::PLAY, 16.0 * (1.0 - 0.3 * t), theme::fade(fg, 1.0 - t), 1.0 / 6.0);
    }
    if t > 0.0 {
        glyph_centred(ui.painter(), rr.center(), egui_phosphor::fill::PAUSE, 16.0 * (0.7 + 0.3 * t), theme::fade(fg, t), 0.0);
    }
    c.play = resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Play or pause  (Space)").clicked();
    c
}

/// A glyph with the middle of its ink at `c` (a font's line box isn't
/// centred on what it draws), moved right by `nudge` of its width.
fn glyph_centred(painter: &egui::Painter, c: Pos2, glyph: &str, size: f32, color: Color32, nudge: f32) {
    let g = painter.layout_no_wrap(glyph.to_owned(), FontId::new(size, theme::fill_family()), color);
    let ink = g.rows.iter().map(|r| r.visuals.mesh_bounds.translate(r.pos.to_vec2())).fold(Rect::NOTHING, |a, b| a.union(b));
    if !ink.is_positive() {
        return;
    }
    let pos = c - ink.center().to_vec2() + vec2(ink.width() * nudge, 0.0);
    painter.galley(pos, g, color);
}

/// The time: "0:22.13 / 0:59.80", or frames. Click to switch. Fixed-width
/// digits, so it doesn't jitter. `total` 0: just the time (narrow windows).
pub fn timecode(ui: &mut egui::Ui, t: f64, total: f64, fps: f64, frames: &mut bool) {
    let p = theme::pal(ui);
    let (now, all) = if *frames {
        let f = |s: f64| (s * fps).round() as u64;
        (f(t).to_string(), if total > 0.0 { format!("/ {} frames", f(total)) } else { String::new() })
    } else {
        (clock(t), if total > 0.0 { format!("/ {}", clock(total)) } else { String::new() })
    };
    let a = ui.painter().layout_no_wrap(now, theme::font(14.0, W::Medium), p.text);
    let b = ui.painter().layout_no_wrap(all, theme::font(14.0, W::Regular), p.text3);
    let w = a.size().x + if b.size().x > 0.0 { 6.0 + b.size().x } else { 0.0 } + 16.0;
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

/// What was done on the ruler's pins.
pub enum Pinned {
    /// Clicked: go there.
    Go(f64),
    /// Your marker there removed (from its right-click menu).
    Remove(f64),
}

/// The ruler over the timeline: ticks and times for `view` (start, length in
/// seconds), and the moments as pins. `removable`: your markers can be taken
/// away from their right-click menu.
pub fn ruler(ui: &mut egui::Ui, rect: Rect, view: (f64, f64), fps: f64, frames: bool, moments: &[Moment], removable: bool) -> Option<Pinned> {
    crate::demo::timeline(ui.ctx(), rect, view, moments.iter().filter(|m| m.marker).map(|m| m.t));
    // Your markers are cues for the person making a demo, not part of it.
    let shown: Vec<Moment>;
    let moments = if crate::demo::on() {
        shown = moments.iter().filter(|m| !m.marker).cloned().collect();
        &shown[..]
    } else {
        moments
    };
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
            let g = painter.layout_no_wrap(label, theme::font(11.0, W::Regular), p.text3);
            // A pin over its time hides it: the pin is what matters there.
            // One that doesn't fit before the edge isn't drawn (cut, the end's
            // "0:05.00" showed as a lone "0").
            let span_x = x + 4.0 - 12.0..=x + 4.0 + g.size().x + 12.0;
            let fits = x + 4.0 + g.size().x <= rect.right() + 10.0;
            if fits && !moments.iter().any(|m| span_x.contains(&x_of(m.t))) {
                painter.galley(Pos2::new(x + 4.0, rect.bottom() - 13.0 - g.size().y), g, p.text3);
            }
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
        let glyph = if m.marker { egui_phosphor::fill::BOOKMARK_SIMPLE } else { icon::CROSSHAIR_SIMPLE };
        painter.text(c, Align2::CENTER_CENTER, glyph, FontId::new(11.0, if m.marker { theme::fill_family() } else { egui::FontFamily::Proportional }), fg);
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(&m.label);
        if resp.clicked() {
            clicked = Some(Pinned::Go(m.t));
        }
        if m.marker && removable {
            let t = m.t;
            if crate::ui_kit::context_menu(&resp, 200.0, |ui| crate::ui_kit::Item::new("Remove marker").icon(icon::TRASH).show(ui).clicked()).unwrap_or(false) {
                clicked = Some(Pinned::Remove(t));
            }
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
    // The line is drawn on whole pixels (egui puts it there); the tab goes
    // where the line goes. Drawn where it fell, it shimmered beside the line
    // as it moved.
    let line = Stroke::new(2.0, color);
    let mut x = x;
    line.round_center_to_pixel(painter.pixels_per_point(), &mut x);
    painter.vline(x, top + 4.0..=bottom, line);
    let pts = vec![Pos2::new(x - 6.0, top - 2.0), Pos2::new(x + 6.0, top - 2.0), Pos2::new(x + 6.0, top + 3.5), Pos2::new(x, top + 8.0), Pos2::new(x - 6.0, top + 3.5)];
    painter.add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
}

/// A thin vertical divider in a row of buttons.
pub fn vrule(ui: &mut egui::Ui) {
    let p = theme::pal(ui);
    let (r, _) = ui.allocate_exact_size(vec2(17.0, 18.0), Sense::hover());
    ui.painter().vline(r.center().x, r.y_range(), Stroke::new(1.0, p.line2));
}

/// Space between the bar's ends and its buttons; the editor's timeline
/// keeps the same at its right.
pub const BAR_PAD: f32 = 14.0;

/// The transport bar's frame: a hairline on top (`hairline`: the editor's,
/// under its divider; the player's bar starts at the picture, whose edge is
/// line enough), three columns (left, the centre on the bar's true centre,
/// right).
pub fn bar(ui: &mut egui::Ui, rect: Rect, hairline: bool, centre_w: f32, mut part: impl FnMut(&mut egui::Ui, Part)) {
    if hairline {
        ui.painter().hline(rect.x_range(), rect.top(), Stroke::new(1.0, theme::pal(ui).line));
    }
    let inner = rect.shrink2(vec2(BAR_PAD, 0.0));
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

/// How loud you listen: the volume (0..=1 on its slider) and mute the
/// player and the editor share, kept between runs (see `viewer::volume`).
pub fn listening(ctx: &egui::Context) -> (f32, bool) {
    crate::viewer::volume(ctx).unwrap_or((1.0, false))
}

/// The gain to listen at: 0 muted, else the slider squared (a squared
/// slider feels even).
pub fn listening_gain(ctx: &egui::Context) -> f32 {
    let (volume, muted) = listening(ctx);
    if muted { 0.0 } else { volume * volume }
}

/// Nothing to hear: muted, or the volume all the way down.
fn silent(ctx: &egui::Context) -> bool {
    let (volume, muted) = listening(ctx);
    muted || volume == 0.0
}

/// Mute (M), or unmute: back to the volume it was, or halfway if that was
/// all the way down.
pub fn toggle_mute(ctx: &egui::Context) {
    let (volume, _) = listening(ctx);
    let now = if silent(ctx) { (if volume == 0.0 { 0.5 } else { volume }, false) } else { (volume, true) };
    crate::viewer::set_volume(ctx, now);
}

/// The speaker that mutes (M) and, given room (`slider`), the volume slider
/// beside it: the player's and the editor's alike, in a bar's right part
/// (laid out right to left).
pub fn volume(ui: &mut egui::Ui, slider_w: Option<f32>) {
    let ctx = ui.ctx().clone();
    let (volume, muted) = listening(&ctx);
    if let Some(w) = slider_w {
        let mut v = if muted { 0.0 } else { volume };
        if slider(ui, &mut v, w).on_hover_text("Volume").changed() {
            crate::viewer::set_volume(&ctx, (v, false));
        }
    }
    let speaker = if silent(&ctx) { icon::SPEAKER_X } else { icon::SPEAKER_HIGH };
    if Btn::icon(Kind::Ghost, speaker).small().tip(if silent(&ctx) { "Unmute  (M)" } else { "Mute  (M)" }).show(ui).clicked() {
        toggle_mute(&ctx);
    }
}

/// "Muted" on a picture that's playing with nothing to hear (top left),
/// which is otherwise easy to take for a clip with no sound. Click to unmute.
pub fn muted_note(ui: &mut egui::Ui, picture: Rect) {
    let ctx = ui.ctx().clone();
    let shown = theme::toggle(&ctx, egui::Id::new("muted_note"), silent(&ctx), theme::FAST);
    if shown <= 0.0 {
        return;
    }
    let painter = ui.painter_at(picture);
    let g = painter.layout_no_wrap(format!("{}  Muted", icon::SPEAKER_X), theme::font(12.5, W::Semibold), Color32::WHITE);
    let r = Rect::from_min_size(picture.left_top() + vec2(12.0, 12.0), g.size() + vec2(18.0, 10.0));
    let resp = ui.interact(r, egui::Id::new("muted_note_click"), Sense::click());
    let hv = theme::toggle(&ctx, resp.id.with("hv"), resp.hovered(), theme::FAST);
    painter.rect_filled(r, ui_kit::RADIUS, Color32::from_black_alpha(((150.0 + 60.0 * hv) * shown) as u8));
    painter.galley(r.min + vec2(9.0, 5.0), g, theme::fade(Color32::WHITE, shown));
    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Unmute  (M)").clicked() {
        toggle_mute(&ctx);
    }
}

/// A slim slider: a track filled to `value` (0..=1) and a round knob.
pub fn slider(ui: &mut egui::Ui, value: &mut f32, width: f32) -> egui::Response {
    stepped_slider(ui, value, width, 0)
}

/// Most stops a slider shows as marks on its track: more are too close.
const MARKED_STOPS: u32 = 24;

/// [`slider`] that snaps to `stops` even steps (0 for none): the knob
/// jumps from one to the next as it's dragged, and up to a couple of dozen
/// show as marks on the track.
pub fn stepped_slider(ui: &mut egui::Ui, value: &mut f32, width: f32, stops: u32) -> egui::Response {
    let p = theme::pal(ui);
    let snap = |v: f32| if stops == 0 { v } else { (v * stops as f32).round() / stops as f32 };
    let (rect, mut resp) = ui.allocate_exact_size(vec2(width, 20.0), Sense::click_and_drag());
    if let Some(pos) = resp.interact_pointer_pos()
        && (resp.dragged() || resp.clicked() || resp.drag_started())
    {
        let v = snap(((pos.x - rect.left() - 7.0) / (rect.width() - 14.0)).clamp(0.0, 1.0));
        if v != *value {
            *value = v;
            resp.mark_changed();
        }
    }
    // Lit while it has the keys too (see `arrow_steps`).
    let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered() || resp.dragged() || resp.has_focus(), theme::FAST);
    let track = Rect::from_min_max(Pos2::new(rect.left() + 7.0, rect.center().y - 2.0), Pos2::new(rect.right() - 7.0, rect.center().y + 2.0));
    ui.painter().rect_filled(track, 2, p.control_h);
    let x = track.left() + track.width() * value.clamp(0.0, 1.0);
    ui.painter().rect_filled(Rect::from_min_max(track.min, Pos2::new(x, track.bottom())), 2, theme::mix(p.text2, p.text, hv));
    // Where it can stop, past the knob's end of the fill (where it shows).
    if (1..=MARKED_STOPS).contains(&stops) {
        for i in 0..=stops {
            let at = track.left() + track.width() * i as f32 / stops as f32;
            if at > x + 1.0 {
                ui.painter().circle_filled(Pos2::new(at, track.center().y), 1.5, p.text3);
            }
        }
    }
    let k = 7.0 + hv;
    ui.painter().circle_filled(Pos2::new(x, rect.center().y + 1.0), k, Color32::from_black_alpha(60));
    ui.painter().circle_filled(Pos2::new(x, rect.center().y), k, Color32::WHITE);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Arrow keys on a slider: clicking or dragging it gives it the keys, then
/// ← and → step it, ten at a time with Shift, and stay with it (↑, ↓, Tab
/// and Esc go on through the page as before). How many steps, if any were
/// pressed. Not for the players' bars, whose arrows go through the video.
pub fn arrow_steps(ui: &egui::Ui, slider: &egui::Response) -> Option<i64> {
    arrow_keys(ui, slider).map(|(d, shift)| if shift { d * 10 } else { d })
}

/// [`arrow_steps`]' presses, one each, and whether Shift was held: for a
/// slider whose Shift is its own (a sound's fader's, finer).
pub fn arrow_keys(ui: &egui::Ui, slider: &egui::Response) -> Option<(i64, bool)> {
    if (slider.clicked() || slider.drag_started()) && !slider.has_focus() {
        slider.request_focus();
    }
    if !slider.has_focus() {
        return None;
    }
    // Without this egui took ← and → to the next control (the slider
    // stepping once on the way). It's only taken from the frame after the
    // slider got the keys, so that one's drawn at once, before an arrow.
    ui.memory_mut(|m| m.set_focus_lock_filter(slider.id, egui::EventFilter { horizontal_arrows: true, ..Default::default() }));
    if !ui.memory(|m| m.had_focus_last_frame(slider.id)) {
        ui.ctx().request_repaint();
    }
    let mut d = 0;
    let shift = ui.input_mut(|i| {
        let shift = i.modifiers.shift;
        let mods = if shift { egui::Modifiers::SHIFT } else { egui::Modifiers::NONE };
        for (key, sign) in [(egui::Key::ArrowLeft, -1), (egui::Key::ArrowRight, 1)] {
            while i.consume_key(mods, key) {
                d += sign;
            }
        }
        shift
    });
    (d != 0).then_some((d, shift))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Clicked, a slider has ← and →: each steps it and they stay with it
    /// (egui took them on to the next slider, this one stepping once). Tab
    /// still goes on.
    #[test]
    fn arrows_stay_with_a_slider() {
        let ctx = egui::Context::default();
        // Each slider's steps, place and id.
        let mut seen = [(0i64, Rect::NOTHING, egui::Id::NULL); 3];
        let mut frame = |events: Vec<egui::Event>| {
            let raw = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(800.0, 200.0))), events, ..Default::default() };
            let mut out = ctx.run_ui(raw, |ui| {
                ui.horizontal(|ui| {
                    for s in &mut seen {
                        let mut v = 0.5;
                        let r = slider(ui, &mut v, 150.0);
                        if let Some(d) = arrow_steps(ui, &r) {
                            s.0 += d;
                        }
                        (s.1, s.2) = (r.rect, r.id);
                    }
                });
            });
            out.textures_delta.clear();
            seen
        };
        let middle = frame(vec![])[1].1.center();
        let button = |pressed| egui::Event::PointerButton { pos: middle, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
        let key = |key| egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
        // The click, then the frame it asks for.
        for events in [vec![egui::Event::PointerMoved(middle)], vec![button(true)], vec![button(false)], vec![]] {
            frame(events);
        }
        frame(vec![key(egui::Key::ArrowRight)]);
        let s = frame(vec![key(egui::Key::ArrowRight)]);
        assert_eq!(s.map(|s| s.0), [0, 2, 0]);
        assert_eq!(ctx.memory(|m| m.focused()), Some(s[1].2), "the keys stay with it");
        let s = frame(vec![key(egui::Key::ArrowLeft)]);
        assert_eq!(s.map(|s| s.0), [0, 1, 0]);
        frame(vec![key(egui::Key::Tab)]);
        assert_eq!(ctx.memory(|m| m.focused()), Some(s[2].2), "Tab goes on");
    }

    #[test]
    fn clock_reads_naturally() {
        assert_eq!(clock(0.0), "0:00.00");
        assert_eq!(clock(65.344), "1:05.34");
        assert_eq!(clock(59.999), "0:59.99");
        assert_eq!(clock(3725.5), "1:02:05.50");
    }
}
