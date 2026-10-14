//! The app's building blocks, drawn from the tokens in `theme`: buttons,
//! switches, segmented controls, menus, setting rows, keycaps. Every page is
//! made of these, so the same kind of choice looks and behaves the same
//! everywhere.
//!
//! - **One control per kind of choice**: a switch for on and off, a segmented
//!   control for two to four short choices, a menu for longer lists, ⓘ for
//!   the why behind a setting.
//! - **Buttons**: main (one per screen, the brightest), normal, quiet
//!   (ghost), destructive. 30 px tall, corners of 6.
//! - **Space** in steps of 4; corners 4 for small marks, 6 for controls, 8
//!   for groups and menus. Only switches, the play button and dots are round.
//! - **Motion**: hover fades in, a chosen option slides to its place, a
//!   button that changes its words changes its width smoothly.

use egui::{Align2, Color32, CornerRadius, FontId, Margin, Pos2, Rect, Response, Sense, Stroke, StrokeKind, TextStyle, vec2};



use crate::theme::{self, Pal, W, icon};

pub const CONTROL_H: f32 = 30.0;
pub const SMALL_H: f32 = 26.0;
/// Buttons, fields, rows.
pub const RADIUS: u8 = 6;
/// Groups, menus, popovers.
pub const CARD_RADIUS: u8 = 8;
/// Space between a page's edge and its content.
pub const PAGE_MARGIN: i8 = 28;

/// Set the look for both themes. Called once at start-up.
pub fn apply(ctx: &egui::Context) {
    theme::install_fonts(ctx);
    for (theme, p) in [(egui::Theme::Dark, &theme::DARK), (egui::Theme::Light, &theme::LIGHT)] {
        ctx.style_mut_of(theme, |style| style_for(style, p));
    }
}

fn style_for(style: &mut egui::Style, p: &Pal) {
    style.animation_time = theme::FAST;
    let s = &mut style.spacing;
    s.item_spacing = vec2(8.0, 6.0);
    s.button_padding = vec2(12.0, 4.0);
    s.interact_size = vec2(30.0, 26.0);
    s.menu_margin = Margin::same(5);
    s.window_margin = Margin::same(18);
    s.combo_height = 300.0;
    s.scroll = egui::style::ScrollStyle { bar_width: 6.0, floating_allocated_width: 0.0, ..egui::style::ScrollStyle::floating() };

    use TextStyle::*;
    style.text_styles = [
        (Small, theme::font(12.5, W::Regular)),
        (Body, theme::font(14.0, W::Regular)),
        (Button, theme::font(13.0, W::Medium)),
        (Heading, theme::font(20.0, W::Semibold)),
        (Monospace, FontId::new(13.0, egui::FontFamily::Monospace)),
    ]
    .into();

    let v = &mut style.visuals;
    *v = if p.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    v.panel_fill = p.base;
    v.window_fill = p.pop;
    v.faint_bg_color = p.raise;
    v.extreme_bg_color = p.control;
    v.code_bg_color = p.control;
    v.override_text_color = None;
    v.weak_text_color = Some(p.text3);
    v.hyperlink_color = p.text;
    v.warn_fg_color = p.warn;
    v.error_fg_color = p.danger;
    v.selection.bg_fill = if p.dark { Color32::from_rgba_unmultiplied(170, 182, 214, 70) } else { Color32::from_rgba_unmultiplied(70, 84, 120, 50) };
    // egui uses this for the text on a selected option *and* a focused
    // field's outline.
    v.selection.stroke = Stroke::new(1.5, p.text2);
    v.text_cursor.stroke = Stroke::new(1.5, p.text);
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(CARD_RADIUS);
    v.window_stroke = Stroke::new(1.0, if p.dark { Color32::from_white_alpha(18) } else { Color32::from_black_alpha(20) });
    let shadow = egui::Shadow { offset: [0, 14], blur: 40, spread: 0, color: Color32::from_black_alpha(if p.dark { 120 } else { 40 }) };
    v.popup_shadow = shadow;
    v.window_shadow = shadow;
    v.slider_trailing_fill = true;
    v.handle_shape = egui::style::HandleShape::Circle;
    v.striped = false;
    // Controls: filled, no outline; hover lightens, nothing grows.
    for (w, fill) in [
        (&mut v.widgets.inactive, p.control),
        (&mut v.widgets.hovered, p.control_h),
        (&mut v.widgets.active, p.control_h),
        (&mut v.widgets.open, p.control_h),
    ] {
        w.weak_bg_fill = fill;
        w.bg_fill = fill;
        w.bg_stroke = Stroke::NONE;
        w.corner_radius = CornerRadius::same(RADIUS);
        w.expansion = 0.0;
        w.fg_stroke = Stroke::new(1.5, p.text);
    }
    v.widgets.inactive.fg_stroke = Stroke::new(1.5, p.text);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.noninteractive.bg_fill = p.raise;
    v.widgets.noninteractive.weak_bg_fill = p.raise;
    v.widgets.noninteractive.corner_radius = CornerRadius::same(RADIUS);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.line);
}

/// Draw again in `delay`. Use this, not `request_repaint_after`, for short
/// delays: egui takes a frame's time (`predicted_dt`, always 1/60 s here)
/// off every delayed repaint, so anything under ~17 ms came at once, and a
/// page asking for one every frame redrew on every display refresh (300 times
/// a second on a fast screen).
pub fn repaint_after(ctx: &egui::Context, delay: std::time::Duration) {
    let frame = ctx.input(|i| i.predicted_dt).max(0.0);
    ctx.request_repaint_after(delay + std::time::Duration::from_secs_f32(frame));
}

/// A group: content on the raised surface, padded.
pub fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .fill(theme::pal(ui).raise)
        .corner_radius(CARD_RADIUS)
        .inner_margin(Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// Scrolling content with a slim scrollbar in the page's right margin.
pub fn scroll<R>(ui: &mut egui::Ui, id: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let reset = ui.ctx().data(|d| d.get_temp::<bool>(scroll_reset_id())).unwrap_or(false);
    let gutter = PAGE_MARGIN as f32;
    let mut rect = ui.available_rect_before_wrap();
    rect.max.x += gutter;
    let mut area = ui.new_child(egui::UiBuilder::new().max_rect(rect));
    area.spacing_mut().scroll = egui::style::ScrollStyle { bar_outer_margin: 8.0, bar_inner_margin: 0.0, bar_width: 6.0, floating_allocated_width: 0.0, ..egui::style::ScrollStyle::floating() };
    let mut scroll = egui::ScrollArea::vertical().id_salt(id).auto_shrink([false, false]);
    if reset {
        scroll = scroll.vertical_scroll_offset(0.0);
    }
    let out = scroll.show(&mut area, |ui| {
        ui.set_max_width(ui.available_width() - gutter);
        add(ui)
    });
    // How far it's scrolled, for anything that follows it (Settings' section list).
    ui.ctx().data_mut(|d| d.insert_temp(egui::Id::new(("scroll_offset", id)), out.state.offset.y));
    out.inner
}

/// How far the [`scroll`] area `id` was scrolled last frame.
pub fn scroll_offset(ctx: &egui::Context, id: &str) -> f32 {
    ctx.data(|d| d.get_temp::<f32>(egui::Id::new(("scroll_offset", id)))).unwrap_or(0.0)
}

fn overlay_id() -> egui::Id {
    egui::Id::new("ui_kit_overlay_open")
}

/// Whether a dialog, menu or dropdown is open this frame: it gets the
/// keyboard, and the page under it must leave keys alone (Esc closing a
/// dialog mustn't also leave the player). Set once a frame by the app.
pub fn set_overlay_open(ctx: &egui::Context, open: bool) {
    ctx.data_mut(|d| d.insert_temp(overlay_id(), open));
}

/// See [`set_overlay_open`]: pages check this before handling shortcuts.
pub fn overlay_open(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<bool>(overlay_id())).unwrap_or(false)
}

fn scroll_reset_id() -> egui::Id {
    egui::Id::new("ui_kit_scroll_reset")
}

/// This frame shows a different view than the last (another page, game or
/// collection): pages scroll back to the top.
pub fn set_scroll_reset(ctx: &egui::Context, reset: bool) {
    ctx.data_mut(|d| d.insert_temp(scroll_reset_id(), reset));
}

fn page_arrived_id() -> egui::Id {
    egui::Id::new("ui_kit_page_arrived")
}

/// Another page (or game, or collection) is shown from this frame: its
/// content arrives with [`arrive`]. Set by the app.
pub fn page_changed(ctx: &egui::Context) {
    let now = ctx.input(|i| i.time);
    ctx.data_mut(|d| d.insert_temp(page_arrived_id(), now));
}

/// Whether the page changed this very frame (see [`page_changed`]).
pub fn just_arrived(ctx: &egui::Context) -> bool {
    let now = ctx.input(|i| i.time);
    ctx.data(|d| d.get_temp::<f64>(page_arrived_id())) == Some(now)
}

/// A page's content, under its heading: just after the page changes it
/// fades in and rises a few points. The fade is a veil in the page's colour
/// over what's drawn, so pictures the GPU draws (the player, the preview)
/// fade too; content with one of those only fades (they can't move with it).
pub fn arrive<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let bg = theme::pal(ui).base;
    arrive_on(ui, bg, add)
}

/// [`arrive`] on a page whose background is `bg`.
pub fn arrive_on<R>(ui: &mut egui::Ui, bg: Color32, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    const RISE: f32 = 10.0;
    let ctx = ui.ctx().clone();
    let Some(at) = ctx.data(|d| d.get_temp::<f64>(page_arrived_id())) else { return add(ui) };
    let t = theme::since(&ctx, at, theme::SLOW);
    if t >= 1.0 {
        return add(ui);
    }
    let layer = ui.layer_id();
    let start = ctx.graphics(|g| g.get(layer).map_or(0, |l| l.next_idx().0));
    // Its scroll bar too, in the page's margin.
    let mut area = ui.available_rect_before_wrap();
    area.max.x += PAGE_MARGIN as f32;
    let r = add(ui);
    let dy = RISE * (1.0 - t);
    let veil = theme::fade(bg, 1.0 - t);
    ctx.graphics_mut(|g| {
        let list = g.entry(layer);
        let end = list.next_idx().0;
        let gpu = list.all_entries().skip(start).any(|cs| matches!(cs.shape, egui::Shape::Callback(_)));
        if !gpu {
            for i in start..end {
                list.mutate_shape(egui::layers::ShapeIdx(i), |cs| cs.shape.translate(vec2(0.0, dy)));
            }
        }
        list.add(area.intersect(ui.clip_rect()), egui::Shape::rect_filled(area, 0, veil));
    });
    r
}

/// A page of settings-like content: scrolls, left-aligned on the page's
/// margin, at most `max_width` wide.
pub fn page(ui: &mut egui::Ui, id: &str, max_width: f32, add: impl FnOnce(&mut egui::Ui)) {
    arrive(ui, |ui| {
        scroll(ui, id, |ui| {
            let w = ui.available_width().min(max_width);
            ui.vertical(|ui| {
                ui.set_width(w);
                add(ui);
                ui.add_space(28.0);
            });
        })
    });
}

/// A short line of help: smaller and quieter than what it explains, wrapping.
pub fn hint(ui: &mut egui::Ui, text: &str) -> Response {
    let c = theme::pal(ui).text3;
    ui.add(egui::Label::new(theme::text(text, 12.5, W::Regular).color(c)).wrap())
}

// --- Buttons ---

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The screen's main action: the brightest thing on it.
    Primary,
    Normal,
    /// Quiet: no fill until hovered.
    Ghost,
    /// Recording's stop, destructive confirmations: red.
    Stop,
    /// A destructive action that isn't the point of the screen: red text.
    DangerText,
    /// On, like a toggled tool (snapping on).
    On,
}

/// A button: an optional icon, a label, an optional trailing icon or keycap.
/// Its width follows its content smoothly when the words change.
pub struct Btn<'a> {
    kind: Kind,
    icon: Option<&'a str>,
    label: String,
    trail: Option<&'a str>,
    keys: Option<Vec<String>>,
    small: bool,
    min_w: f32,
    tip: Option<String>,
    enabled: bool,
    corners: CornerRadius,
    id: Option<egui::Id>,
    progress: Option<f32>,
}

impl<'a> Btn<'a> {
    pub fn new(kind: Kind, label: impl Into<String>) -> Self {
        Self {
            kind,
            icon: None,
            label: label.into(),
            trail: None,
            keys: None,
            small: false,
            min_w: 0.0,
            tip: None,
            enabled: true,
            corners: CornerRadius::same(RADIUS),
            id: None,
            progress: None,
        }
    }
    pub fn primary(label: impl Into<String>) -> Self {
        Self::new(Kind::Primary, label)
    }
    pub fn normal(label: impl Into<String>) -> Self {
        Self::new(Kind::Normal, label)
    }
    pub fn ghost(label: impl Into<String>) -> Self {
        Self::new(Kind::Ghost, label)
    }
    /// A square button with just an icon.
    pub fn icon(kind: Kind, icon: &'a str) -> Self {
        Self::new(kind, "").with_icon(icon)
    }
    pub fn with_icon(mut self, icon: &'a str) -> Self {
        self.icon = Some(icon);
        self
    }
    pub fn trail(mut self, icon: &'a str) -> Self {
        self.trail = Some(icon);
        self
    }
    pub fn keys(mut self, keys: Option<Vec<String>>) -> Self {
        self.keys = keys;
        self
    }
    pub fn small(mut self) -> Self {
        self.small = true;
        self
    }
    pub fn min_width(mut self, w: f32) -> Self {
        self.min_w = w;
        self
    }
    pub fn tip(mut self, tip: impl Into<String>) -> Self {
        self.tip = Some(tip.into());
        self
    }
    pub fn enabled(mut self, on: bool) -> Self {
        self.enabled = on;
        self
    }
    pub fn corners(mut self, c: CornerRadius) -> Self {
        self.corners = c;
        self
    }
    /// A stable id, so the width animates across frames even when the
    /// button's place in the layout changes.
    pub fn id(mut self, id: impl std::hash::Hash + std::fmt::Debug) -> Self {
        self.id = Some(egui::Id::new(id));
        self
    }

    /// How wide it's drawn this frame (its width eases to fit its words).
    /// Needs an [`Btn::id`] to match what [`Btn::show`] draws.
    pub fn width(&self, ui: &egui::Ui) -> f32 {
        let h = if self.small { SMALL_H } else { CONTROL_H };
        let target = self.target_width(ui, h);
        match self.id {
            Some(id) => theme::value(ui.ctx(), id.with("w"), target, theme::NORMAL),
            None => target,
        }
    }

    fn target_width(&self, ui: &egui::Ui, h: f32) -> f32 {
        let size = if self.small { 12.5 } else { 13.0 };
        let icon_size = if self.small { 14.0 } else { 15.5 };
        let text_w = |t: &str, f: FontId| ui.painter().layout_no_wrap(t.to_owned(), f, Color32::PLACEHOLDER).size().x;
        let label_w = (!self.label.is_empty()).then(|| text_w(&self.label, theme::font(size, W::Medium)));
        let icon_w = self.icon.map(|i| text_w(i, FontId::proportional(icon_size)));
        let trail_w = self.trail.map(|i| text_w(i, FontId::proportional(icon_size - 1.5)));
        let keys_w = self.keys.as_ref().map(|k| key_hint_width(ui, k, 11.0));
        let only_icon = label_w.is_none() && trail_w.is_none() && keys_w.is_none();
        if only_icon {
            return h;
        }
        let pad_l = if self.icon.is_some() { 10.0 } else if self.small { 9.0 } else { 12.0 };
        let pad_r = if self.keys.is_some() || self.trail.is_some() { 8.0 } else if self.small { 9.0 } else { 12.0 };
        let parts: Vec<f32> = [icon_w, label_w, trail_w, keys_w].into_iter().flatten().collect();
        let content = parts.iter().sum::<f32>() + 7.0 * (parts.len().max(1) - 1) as f32;
        (pad_l + content + pad_r).max(self.min_w)
    }

    pub fn show(self, ui: &mut egui::Ui) -> Response {
        let p = theme::pal(ui);
        let h = if self.small { SMALL_H } else { CONTROL_H };
        let size = if self.small { 12.5 } else { 13.0 };
        let font = theme::font(size, W::Medium);
        let icon_size = if self.small { 14.0 } else { 15.5 };
        let galley = (!self.label.is_empty()).then(|| ui.painter().layout_no_wrap(self.label.clone(), font.clone(), Color32::PLACEHOLDER));
        let icon_g = self.icon.map(|i| ui.painter().layout_no_wrap(i.to_owned(), FontId::proportional(icon_size), Color32::PLACEHOLDER));
        let trail_g = self.trail.map(|i| ui.painter().layout_no_wrap(i.to_owned(), FontId::proportional(icon_size - 1.5), Color32::PLACEHOLDER));
        let keys_w = self.keys.as_ref().map_or(0.0, |k| key_hint_width(ui, k, 11.0));
        let gap = 7.0;
        let only_icon = galley.is_none() && trail_g.is_none() && self.keys.is_none();
        let pad_l = if only_icon { 0.0 } else if self.icon.is_some() { 10.0 } else if self.small { 9.0 } else { 12.0 };
        let pad_r = if only_icon { 0.0 } else if self.keys.is_some() || self.trail.is_some() { 8.0 } else if self.small { 9.0 } else { 12.0 };
        let mut content = 0.0;
        let mut parts = 0;
        for w in [icon_g.as_ref().map(|g| g.size().x), galley.as_ref().map(|g| g.size().x), trail_g.as_ref().map(|g| g.size().x), (keys_w > 0.0).then_some(keys_w)].into_iter().flatten() {
            content += w;
            parts += 1;
        }
        content += gap * (parts.max(1) - 1) as f32;
        let target_w = if only_icon { h } else { (pad_l + content + pad_r).max(self.min_w) };
        let id = self.id.unwrap_or_else(|| ui.next_auto_id());
        let w = theme::value(ui.ctx(), id.with("w"), target_w, theme::NORMAL);
        let sense = if self.enabled { Sense::click() } else { Sense::hover() };
        let (rect, mut resp) = ui.allocate_exact_size(vec2(w, h), sense);
        if ui.is_rect_visible(rect) {
            let hovered = self.enabled && resp.hovered();
            let pressed = self.enabled && resp.is_pointer_button_down_on();
            let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), hovered, theme::FAST);
            let (fill, fg) = match self.kind {
                Kind::Primary => (theme::mix(p.primary, if p.dark { Color32::from_gray(255) } else { Color32::from_gray(60) }, hv * 0.25), p.primary_fg),
                Kind::Normal => (theme::mix(p.control, p.control_h, hv), p.text),
                Kind::Ghost => (theme::fade(p.sel, hv * if p.dark { 1.0 } else { 0.0 }).blend(theme::fade(p.control, if p.dark { 0.0 } else { hv })), theme::mix(p.text2, p.text, hv)),
                Kind::Stop => (theme::mix(p.rec, Color32::WHITE, hv * 0.12), Color32::WHITE),
                Kind::DangerText => (theme::fade(p.sel, hv).blend(theme::fade(p.control, if p.dark { 0.0 } else { hv })), p.danger),
                Kind::On => (p.seg_on, p.text),
            };
            let fg = if self.enabled { fg } else { theme::fade(fg, 0.38) };
            let fill = if self.enabled || self.kind == Kind::Primary || self.kind == Kind::Stop { fill } else { Color32::TRANSPARENT };
            let rect_d = if pressed { rect.shrink(0.5) } else { rect };
            ui.painter().rect_filled(rect_d, self.corners, if self.enabled { fill } else { theme::fade(fill, 0.4) });
            // Content: centred when it fits, clipped when the width is still
            // catching up.
            let painter = ui.painter_at(rect);
            // Centred: in the button's width as it is (a minimum width, or
            // one still catching up with new words).
            let mut x = if only_icon { rect.center().x - icon_g.as_ref().map_or(0.0, |g| g.size().x) / 2.0 } else { rect.center().x - (pad_l + content + pad_r) / 2.0 + pad_l };
            let cy = rect.center().y;
            if let Some(g) = icon_g {
                painter.galley(Pos2::new(x, cy - g.size().y / 2.0), g.clone(), fg);
                x += g.size().x + gap;
            }
            if let Some(g) = galley {
                painter.galley(Pos2::new(x, cy - g.size().y / 2.0), g.clone(), fg);
                x += g.size().x + gap;
            }
            if let Some(g) = trail_g {
                painter.galley(Pos2::new(x, cy - g.size().y / 2.0 + 0.5), g.clone(), theme::fade(fg, 0.75));
                x += g.size().x + gap;
            }
            if let Some(k) = &self.keys {
                key_hint(&painter, Pos2::new(x, cy), k, 11.0, fg);
            }
            if let Some(f) = self.progress {
                let bar = Rect::from_min_max(Pos2::new(rect.left(), rect.bottom() - 2.0), Pos2::new(rect.left() + rect.width() * f.clamp(0.0, 1.0), rect.bottom()));
                painter.rect_filled(bar, 0, theme::fade(fg, 0.6));
            }
        }
        if let Some(tip) = &self.tip {
            resp = if self.enabled { resp.on_hover_text(tip) } else { resp.on_disabled_hover_text(tip) };
        }
        if self.enabled {
            resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        }
        resp
    }
}

/// A button at the standard height; `primary` is the screen's main action.
/// (For dialogs and older call sites; new code uses [`Btn`].)
pub fn button(text: impl Into<String>, primary: bool) -> ButtonWidget {
    ButtonWidget(Btn::new(if primary { Kind::Primary } else { Kind::Normal }, text))
}

/// A destructive action's button (delete for good, stop recording).
pub fn danger_button(text: impl Into<String>) -> ButtonWidget {
    ButtonWidget(Btn::new(Kind::Stop, text))
}

/// A [`Btn`] usable with `ui.add`.
pub struct ButtonWidget(pub Btn<'static>);

impl ButtonWidget {
    pub fn ghost(self) -> Self {
        Self(Btn { kind: Kind::Ghost, ..self.0 })
    }
}

impl egui::Widget for ButtonWidget {
    fn ui(self, ui: &mut egui::Ui) -> Response {
        self.0.show(ui)
    }
}

impl egui::Widget for Btn<'_> {
    fn ui(self, ui: &mut egui::Ui) -> Response {
        self.show(ui)
    }
}

// --- Keys ---

/// How wide [`key_hint`] draws `keys`.
pub fn key_hint_width(ui: &egui::Ui, keys: &[String], size: f32) -> f32 {
    let g = ui.painter().layout_no_wrap(key_text(keys), theme::font(size, W::Semibold), Color32::PLACEHOLDER);
    g.size().x + 10.0
}

/// A shortcut as one compact key: "⌥F10" on a Mac, "Alt F10" elsewhere,
/// with Phosphor's ⌥ and ⌘ (the text font has neither).
pub fn key_text(keys: &[String]) -> String {
    let mut s = String::new();
    for (i, k) in keys.iter().enumerate() {
        let sym = match k.as_str() {
            "Option" => icon::OPTION,
            "Cmd" => icon::COMMAND,
            "Shift" => icon::ARROW_FAT_UP,
            "Control" => icon::CONTROL,
            other => other,
        };
        let is_sym = sym != k.as_str();
        if i > 0 && !is_sym && !keys[i - 1].is_empty() && !matches!(keys[i - 1].as_str(), "Option" | "Cmd" | "Shift" | "Control") {
            s.push(' ');
        }
        s.push_str(sym);
        if !is_sym && i + 1 < keys.len() && !cfg!(target_os = "macos") {
            s.push(' ');
        }
    }
    s
}

/// A small key box with a shortcut in it, its left edge at `left`, centred
/// on `left.y`. Returns its width.
pub fn key_hint(painter: &egui::Painter, left: Pos2, keys: &[String], size: f32, color: Color32) -> f32 {
    let g = painter.layout_no_wrap(key_text(keys), theme::font(size, W::Semibold), color);
    let r = Rect::from_min_size(Pos2::new(left.x, left.y - 9.0), vec2(g.size().x + 10.0, 18.0));
    painter.rect_filled(r, 4, Color32::from_rgba_unmultiplied(127, 127, 135, 46));
    painter.galley(Pos2::new(r.left() + 5.0, r.center().y - g.size().y / 2.0), g, theme::fade(color, 0.85));
    r.width()
}

/// Keys as keycaps, one per key (Settings' shortcuts): [⌥] [F10].
pub fn keycaps(ui: &mut egui::Ui, keys: &[String], listening: bool) -> Response {
    let p = theme::pal(ui);
    let font = theme::font(12.5, W::Semibold);
    let caps: Vec<String> = if listening { vec!["Press keys…".into()] } else { keys.iter().map(|k| key_text(std::slice::from_ref(k))).collect() };
    let galleys: Vec<_> = caps.iter().map(|k| ui.painter().layout_no_wrap(k.clone(), font.clone(), p.text)).collect();
    let cap_w = |g: &std::sync::Arc<egui::Galley>| (g.size().x + 10.0).max(22.0);
    let inner: f32 = galleys.iter().map(cap_w).sum::<f32>() + 4.0 * galleys.len().saturating_sub(1) as f32;
    let target = inner + 12.0;
    let id = ui.next_auto_id();
    let w = theme::value(ui.ctx(), id.with("w"), target, theme::NORMAL);
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 28.0), Sense::click());
    let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered(), theme::FAST);
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, RADIUS, theme::mix(p.control, p.control_h, hv));
    if listening {
        painter.rect_stroke(rect, RADIUS, Stroke::new(1.5, p.text), StrokeKind::Inside);
    }
    let mut x = rect.left() + 6.0;
    for g in galleys {
        let cw = cap_w(&g);
        let cap = Rect::from_min_size(Pos2::new(x, rect.center().y - 10.0), vec2(cw, 20.0));
        if !listening {
            painter.rect_filled(cap.translate(vec2(0.0, 1.0)), 4, p.line2);
            painter.rect_filled(cap, 4, if p.dark { p.raise } else { Color32::WHITE });
        }
        painter.galley(cap.center() - g.size() / 2.0, g, if listening { p.text2 } else { p.text });
        x += cw + 4.0;
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

// --- Switch, segmented, select ---

/// On or off. The knob slides; the track fills.
pub fn switch(ui: &mut egui::Ui, on: &mut bool) -> Response {
    let p = theme::pal(ui);
    let (rect, mut resp) = ui.allocate_exact_size(vec2(34.0, 20.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = theme::toggle(ui.ctx(), resp.id, *on, theme::NORMAL);
    let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered(), theme::FAST);
    let enabled = ui.is_enabled();
    let off_track = theme::mix(p.control_h, if p.dark { Color32::from_gray(70) } else { Color32::from_gray(200) }, hv * 0.4);
    let track = theme::mix(off_track, p.primary, t);
    let track = if enabled { track } else { theme::fade(track, 0.5) };
    ui.painter().rect_filled(rect, 10, track);
    let knob = theme::mix(Color32::WHITE, p.primary_fg, t);
    let c = Pos2::new(rect.left() + 10.0 + 14.0 * t, rect.center().y);
    ui.painter().circle_filled(c + vec2(0.0, 1.0), 8.0, Color32::from_black_alpha(50));
    ui.painter().circle_filled(c, 8.0, if enabled { knob } else { theme::fade(knob, 0.7) });
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A box to tick, with its words: for options in a dialog.
pub fn checkbox(ui: &mut egui::Ui, on: &mut bool, label: &str) -> Response {
    let p = theme::pal(ui);
    let g = ui.painter().layout_no_wrap(label.to_owned(), theme::font(13.5, W::Regular), p.text);
    let (rect, mut resp) = ui.allocate_exact_size(vec2(16.0 + 9.0 + g.size().x, 28.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = theme::toggle(ui.ctx(), resp.id, *on, theme::FAST);
    let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered(), theme::FAST);
    let b = Rect::from_min_size(Pos2::new(rect.left(), rect.center().y - 8.0), vec2(16.0, 16.0));
    if t < 1.0 {
        ui.painter().rect_stroke(b, 4, Stroke::new(1.5, theme::mix(p.text3, p.text2, hv)), StrokeKind::Inside);
    }
    if t > 0.0 {
        ui.painter().rect_filled(b, 4, theme::fade(p.primary, t));
        let c = b.center();
        let tick = vec![c + vec2(-3.5, 0.2), c + vec2(-1.0, 2.8), c + vec2(3.8, -2.6)];
        ui.painter().add(egui::Shape::line(tick, Stroke::new(1.8, theme::fade(p.primary_fg, t))));
    }
    ui.painter().galley(Pos2::new(b.right() + 9.0, rect.center().y - g.size().y / 2.0), g, p.text);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// An option of a [`segmented`] control: an icon and/or words.
pub struct Seg<'a> {
    pub icon: Option<&'a str>,
    pub label: &'a str,
    pub tip: Option<&'a str>,
}

impl<'a> From<&'a str> for Seg<'a> {
    fn from(label: &'a str) -> Self {
        Seg { icon: None, label, tip: None }
    }
}

/// Space between a [`segmented`] control's edge and its options.
pub const SEGMENT_PAD: f32 = 2.0;

/// Two to four short choices side by side; the chosen one is a raised pill
/// that slides between them. `full`: the options share the width given.
/// Returns the option clicked.
pub fn segmented_opts(ui: &mut egui::Ui, id: impl std::hash::Hash + std::fmt::Debug, opts: &[Seg], chosen: usize, full: Option<f32>, enabled: bool) -> Option<usize> {
    let p = theme::pal(ui);
    let font = theme::font(13.0, W::Medium);
    let h = SMALL_H + 2.0 * SEGMENT_PAD;
    let widths: Vec<f32> = opts
        .iter()
        .map(|o| {
            let t = ui.painter().layout_no_wrap(o.label.to_owned(), font.clone(), Color32::PLACEHOLDER).size().x;
            let i = if o.icon.is_some() { 14.0 + if o.label.is_empty() { 0.0 } else { 6.0 } } else { 0.0 };
            t + i + 22.0
        })
        .collect();
    let n = opts.len().max(1) as f32;
    let natural: f32 = widths.iter().sum::<f32>() + 2.0 * (n - 1.0) + 2.0 * SEGMENT_PAD;
    let total = full.unwrap_or(natural).max(natural.min(full.unwrap_or(natural)));
    let (rect, _) = ui.allocate_exact_size(vec2(total, h), Sense::hover());
    let painter = ui.painter_at(rect.expand(2.0));
    painter.rect_filled(rect, RADIUS, p.control);
    let inner = rect.shrink(SEGMENT_PAD);
    let ws: Vec<f32> = if full.is_some() { vec![(inner.width() - 2.0 * (n - 1.0)) / n; opts.len()] } else { widths };
    let mut xs = Vec::with_capacity(opts.len());
    let mut x = inner.left();
    for w in &ws {
        xs.push(x);
        x += w + 2.0;
    }
    let base_id = egui::Id::new(id);
    // The chosen pill slides to its option and to its width.
    if let Some(&cx) = xs.get(chosen) {
        let px = theme::value(ui.ctx(), base_id.with("x"), cx - inner.left(), theme::NORMAL);
        let pw = theme::value(ui.ctx(), base_id.with("w"), ws[chosen], theme::NORMAL);
        let pill = Rect::from_min_size(Pos2::new(inner.left() + px, inner.top()), vec2(pw, inner.height()));
        if p.dark {
            painter.rect_filled(pill.translate(vec2(0.0, 1.0)), 4, Color32::from_black_alpha(56));
        } else {
            painter.rect_filled(pill.translate(vec2(0.0, 1.0)), 4, Color32::from_black_alpha(18));
        }
        painter.rect_filled(pill, 4, p.seg_on);
    }
    let mut clicked = None;
    for (i, o) in opts.iter().enumerate() {
        let r = Rect::from_min_size(Pos2::new(xs[i], inner.top()), vec2(ws[i], inner.height()));
        let resp = ui.interact(r, base_id.with(i), if enabled { Sense::click() } else { Sense::hover() });
        let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered() && enabled, theme::FAST);
        let on = i == chosen;
        let color = if on { p.text } else { theme::mix(p.text2, p.text, hv) };
        let color = if enabled { color } else { theme::fade(color, 0.55) };
        let label = (!o.label.is_empty()).then(|| ui.painter().layout_no_wrap(o.label.to_owned(), font.clone(), color));
        let icon_g = o.icon.map(|ic| ui.painter().layout_no_wrap(ic.to_owned(), FontId::proportional(14.0), color));
        let cw = label.as_ref().map_or(0.0, |g| g.size().x) + icon_g.as_ref().map_or(0.0, |g| g.size().x) + if label.is_some() && icon_g.is_some() { 6.0 } else { 0.0 };
        let mut x = r.center().x - cw / 2.0;
        if let Some(g) = icon_g {
            painter.galley(Pos2::new(x, r.center().y - g.size().y / 2.0), g.clone(), color);
            x += g.size().x + 6.0;
        }
        if let Some(g) = label {
            painter.galley(Pos2::new(x, r.center().y - g.size().y / 2.0), g, color);
        }
        let resp = match o.tip {
            Some(t) => resp.on_hover_text(t),
            None => resp,
        };
        if enabled && !on {
            let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
            if resp.clicked() {
                clicked = Some(i);
            }
        }
    }
    clicked
}

/// A menu's trigger: the current value and a ∨. Open it with
/// [`menu_popup`].
pub fn select(ui: &mut egui::Ui, value: &str, min_w: f32) -> Response {
    let p = theme::pal(ui);
    let font = theme::font(13.0, W::Medium);
    let g = ui.painter().layout_no_wrap(value.to_owned(), font, p.text);
    let target = (g.size().x + 11.0 + 10.0 + 14.0 + 8.0).max(min_w);
    let id = ui.next_auto_id();
    let w = theme::value(ui.ctx(), id.with("w"), target, theme::NORMAL);
    let (rect, resp) = ui.allocate_exact_size(vec2(w, CONTROL_H), Sense::click());
    let open = egui::Popup::is_id_open(ui.ctx(), resp.id.with("menu"));
    let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered() || open, theme::FAST);
    let enabled = ui.is_enabled();
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, RADIUS, theme::mix(p.control, p.control_h, hv));
    let tc = if enabled { p.text } else { theme::fade(p.text, 0.55) };
    painter.galley(Pos2::new(rect.left() + 11.0, rect.center().y - g.size().y / 2.0), g, tc);
    let rot = theme::toggle(ui.ctx(), resp.id.with("open"), open, theme::NORMAL);
    chevron(&painter, Pos2::new(rect.right() - 15.0, rect.center().y), 4.0, rot, p.text3);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A small ∨ (turned to ∧ as `turn` goes to 1).
pub fn chevron(painter: &egui::Painter, c: Pos2, size: f32, turn: f32, color: Color32) {
    let dy = size * 0.5 * (1.0 - 2.0 * turn);
    let pts = vec![c + vec2(-size, -dy), c + vec2(0.0, dy), c + vec2(size, -dy)];
    painter.add(egui::Shape::line(pts, Stroke::new(1.6, color)));
}

/// ⓘ: the why behind a setting, in a popover on click.
pub fn info(ui: &mut egui::Ui, title: &str, body: &str) -> Response {
    let p = theme::pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::click());
    let id = resp.id.with("tip");
    let open = egui::Popup::is_id_open(ui.ctx(), id);
    let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered() || open, theme::FAST);
    if hv > 0.0 {
        ui.painter().circle_filled(rect.center(), 9.0, theme::fade(p.sel, hv * 1.4));
    }
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon::INFO, FontId::proportional(14.0), theme::mix(p.text3, p.text, hv));
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    egui::Popup::from_toggle_button_response(&resp)
        .id(id)
        .gap(6.0)
        .frame(pop_frame(ui))
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.set_width(290.0);
            let p = theme::pal(ui);
            ui.label(theme::text(title, 13.5, W::Semibold).color(p.text));
            ui.add_space(2.0);
            ui.add(egui::Label::new(theme::text(body, 13.0, W::Regular).color(p.text2)).wrap());
        });
    resp
}

/// The frame of menus, popovers and dialogs.
pub fn pop_frame(ui: &egui::Ui) -> egui::Frame {
    let p = theme::pal(ui);
    egui::Frame::new()
        .fill(p.pop)
        .corner_radius(CARD_RADIUS)
        .stroke(Stroke::new(1.0, if p.dark { Color32::from_white_alpha(18) } else { Color32::from_black_alpha(20) }))
        .shadow(ui.visuals().popup_shadow)
        .inner_margin(Margin::symmetric(12, 10))
}

// --- Menus ---

/// The frame of a menu.
pub fn menu_frame(ui: &egui::Ui) -> egui::Frame {
    pop_frame(ui).inner_margin(Margin::same(5))
}

/// A menu's look, for `ui.menu_button`, context menus and submenus.
pub fn menu_style(style: &mut egui::Style) {
    egui::containers::menu::menu_style(style);
    let p = theme::of(&style.visuals);
    style.spacing.item_spacing = vec2(0.0, 1.0);
    style.spacing.button_padding = vec2(9.0, 0.0);
    style.spacing.interact_size.y = 30.0;
    style.visuals.widgets.hovered.weak_bg_fill = if p.dark { p.sel } else { p.control };
    style.visuals.widgets.open.weak_bg_fill = if p.dark { p.sel } else { p.control };
    style.visuals.widgets.active.weak_bg_fill = if p.dark { p.sel } else { p.control };
    for w in [&mut style.visuals.widgets.hovered, &mut style.visuals.widgets.open, &mut style.visuals.widgets.active, &mut style.visuals.widgets.inactive] {
        w.corner_radius = CornerRadius::same(5);
        w.fg_stroke = Stroke::new(1.0, p.text);
    }
    style.visuals.window_fill = p.pop;
    style.visuals.menu_corner_radius = CornerRadius::same(CARD_RADIUS);
}

/// A row in a menu: a leading icon (or a check), its words, and on the right
/// a key, a value or a ›. Returns its response; a click closes the menu.
pub struct Item<'a> {
    icon: Option<&'a str>,
    check: Option<bool>,
    label: String,
    right: Option<String>,
    keys: Option<Vec<String>>,
    danger: bool,
    enabled: bool,
    tip: Option<String>,
}

impl<'a> Item<'a> {
    pub fn new(label: impl Into<String>) -> Self {
        Self { icon: None, check: None, label: label.into(), right: None, keys: None, danger: false, enabled: true, tip: None }
    }
    pub fn icon(mut self, icon: &'a str) -> Self {
        self.icon = Some(icon);
        self
    }
    pub fn check(mut self, on: bool) -> Self {
        self.check = Some(on);
        self
    }
    pub fn value(mut self, v: impl Into<String>) -> Self {
        self.right = Some(v.into());
        self
    }
    pub fn keys(mut self, k: Option<Vec<String>>) -> Self {
        self.keys = k;
        self
    }
    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }
    pub fn enabled(mut self, on: bool) -> Self {
        self.enabled = on;
        self
    }
    pub fn tip(mut self, t: impl Into<String>) -> Self {
        self.tip = Some(t.into());
        self
    }

    fn natural_width(&self, ui: &egui::Ui) -> f32 {
        let g = ui.painter().layout_no_wrap(self.label.clone(), theme::font(13.5, W::Regular), Color32::PLACEHOLDER);
        let lead = if self.icon.is_some() || self.check.is_some() { 24.0 } else { 0.0 };
        let right = self.right.as_ref().map_or(0.0, |r| ui.painter().layout_no_wrap(r.clone(), theme::font(12.5, W::Regular), Color32::PLACEHOLDER).size().x + 18.0)
            + self.keys.as_ref().map_or(0.0, |k| key_hint_width(ui, k, 11.0) + 18.0);
        9.0 + lead + g.size().x + right + 10.0
    }

    pub fn show(self, ui: &mut egui::Ui) -> Response {
        let natural = self.natural_width(ui);
        menu_saw(ui, natural);
        let w = natural.max(menu_min_width(ui));
        self.paint(ui, w, false)
    }

    fn paint(self, ui: &mut egui::Ui, w: f32, sub: bool) -> Response {
        let p = theme::pal(ui);
        let (rect, resp) = ui.allocate_exact_size(vec2(w, 30.0), if self.enabled { Sense::click() } else { Sense::hover() });
        let open = sub && egui::Popup::is_id_open(ui.ctx(), egui::containers::menu::SubMenu::id_from_widget_id(resp.id));
        let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), self.enabled && (resp.hovered() || open), theme::FAST * 0.7);
        if hv > 0.0 {
            ui.painter().rect_filled(rect, 5, theme::fade(if p.dark { p.sel } else { p.control }, hv));
        }
        let base = if self.danger { p.danger } else { p.text };
        let color = if self.enabled { base } else { theme::fade(base, 0.4) };
        let icon_color = if self.danger { color } else if self.enabled { p.text2 } else { theme::fade(p.text2, 0.4) };
        let painter = ui.painter_at(rect);
        let mut x = rect.left() + 9.0;
        if let Some(on) = self.check {
            if on {
                painter.text(Pos2::new(x + 7.0, rect.center().y), Align2::CENTER_CENTER, icon::CHECK, FontId::proportional(13.0), p.text);
            }
            x += 24.0;
        } else if let Some(i) = self.icon {
            painter.text(Pos2::new(x + 7.5, rect.center().y), Align2::CENTER_CENTER, i, FontId::proportional(15.0), icon_color);
            x += 24.0;
        }
        let mut right_x = rect.right() - 9.0;
        if sub {
            painter.text(Pos2::new(right_x, rect.center().y), Align2::RIGHT_CENTER, icon::CARET_RIGHT, FontId::proportional(12.0), p.text3);
            right_x -= 18.0;
        } else if let Some(k) = &self.keys {
            let kw = key_hint_width(ui, k, 11.0);
            key_hint(&painter, Pos2::new(right_x - kw, rect.center().y), k, 11.0, p.text3);
            right_x -= kw + 10.0;
        } else if let Some(v) = &self.right {
            let g = painter.layout_no_wrap(v.clone(), theme::font(12.5, W::Regular), p.text3);
            right_x -= g.size().x;
            painter.galley(Pos2::new(right_x, rect.center().y - g.size().y / 2.0), g, p.text3);
            right_x -= 12.0;
        }
        let g = painter.layout_job(single_line_job(&self.label, theme::font(13.5, W::Regular), color, (right_x - x).max(10.0)));
        painter.galley(Pos2::new(x, rect.center().y - g.size().y / 2.0), g, color);
        let mut resp = resp;
        if let Some(t) = &self.tip {
            resp = resp.on_hover_text(t);
        }
        if !sub && resp.clicked() {
            ui.close();
        }
        resp
    }
}

/// A menu's width and its widest row, kept by the menu's own `Ui`: as wide
/// as asked, or as its widest row last frame when that's wider (a row can't
/// see the ones after it). Never as wide as the room around it: a popup
/// keeps the size its first frame had, and rows that filled the room made
/// every menu as wide as the window.
#[derive(Clone, Copy, Default)]
struct MenuWidth {
    asked: f32,
    widest: f32,
    /// The widest row so far this frame.
    seen: f32,
}

/// The menu `ui` is in: its key, and its width. Rows sit in the menu's own
/// `Ui` or a few levels inside it (a horizontal group, a scroll area).
fn menu_of(ui: &egui::Ui) -> Option<(egui::Id, MenuWidth)> {
    ui.stack().iter().take(6).find_map(|frame| {
        let key = frame.id.with("ui_kit_menu_w");
        ui.data(|d| d.get_temp::<MenuWidth>(key)).map(|m| (key, m))
    })
}

/// The width rows of the menu `ui` is in are laid out at.
fn menu_min_width(ui: &egui::Ui) -> f32 {
    menu_of(ui).map_or(0.0, |(_, m)| m.asked.max(m.widest))
}

/// A row that needs `w` was laid out in the menu `ui` is in.
fn menu_saw(ui: &egui::Ui, w: f32) {
    if let Some((key, _)) = menu_of(ui) {
        ui.data_mut(|d| {
            let m = d.get_temp_mut_or_default::<MenuWidth>(key);
            m.seen = m.seen.max(w);
        });
    }
}

/// Lay out a menu's content `w` wide (wider when a row needs it).
pub fn menu_width(ui: &mut egui::Ui, w: f32) {
    let key = ui.id().with("ui_kit_menu_w");
    let m = ui.data_mut(|d| {
        let m = d.get_temp_mut_or_default::<MenuWidth>(key);
        // Last frame's rows: the widest is the width now.
        if m.seen > 0.0 {
            m.widest = m.seen;
        }
        m.seen = 0.0;
        m.asked = w;
        *m
    });
    ui.set_width(m.asked.max(m.widest));
}

/// A row that opens a submenu to its right on hover.
pub fn submenu<R>(ui: &mut egui::Ui, item: Item, content: impl FnOnce(&mut egui::Ui) -> R) -> Option<R> {
    let natural = item.natural_width(ui) + 18.0;
    menu_saw(ui, natural);
    let w = natural.max(menu_min_width(ui));
    let resp = item.paint(ui, w, true);
    let style = std::sync::Arc::new({
        let mut s = (*ui.ctx().global_style()).clone();
        menu_style(&mut s);
        s
    });
    egui::containers::menu::SubMenu::new()
        .config(egui::containers::menu::MenuConfig::new().style(egui::style::StyleModifier::new(move |s: &mut egui::Style| *s = (*style).clone())))
        .show(ui, &resp, |ui| {
            menu_width(ui, menu_floor(220.0));
            content(ui)
        })
        .map(|r| r.inner)
}

/// A divider between a menu's groups.
pub fn menu_sep(ui: &mut egui::Ui) {
    let p = theme::pal(ui);
    ui.add_space(4.0);
    let w = menu_min_width(ui).max(1.0);
    let (r, _) = ui.allocate_exact_size(vec2(w, 1.0), Sense::hover());
    ui.painter().hline(r.x_range().shrink(4.0), r.center().y, Stroke::new(1.0, p.line));
    ui.add_space(4.0);
}

/// A quiet line of words in a menu (what the items under it are).
pub fn menu_head(ui: &mut egui::Ui, text: &str) {
    let p = theme::pal(ui);
    ui.add_space(3.0);
    ui.horizontal(|ui| {
        ui.add_space(9.0);
        ui.add(egui::Label::new(theme::text(text, 12.0, W::Regular).color(p.text3)).wrap());
    });
    ui.add_space(3.0);
}

/// A menu opened by clicking `resp`, under it (aligned to its `left` or
/// right edge).
pub fn menu_popup<R>(resp: &Response, left: bool, width: f32, content: impl FnOnce(&mut egui::Ui) -> R) -> Option<R> {
    let id = resp.id.with("menu");
    // Dev aid: `HESTECLIPS_OPEN_MENU=<n>` opens the n-th menu button drawn
    // (from 0) once, to see it without clicking.
    #[cfg(debug_assertions)]
    if let Some(n) = std::env::var("HESTECLIPS_OPEN_MENU").ok().and_then(|s| s.parse::<u64>().ok()) {
        let key = egui::Id::new("ui_kit_dev_menu");
        let (pass, count, done) = resp.ctx.data(|d| d.get_temp::<(u64, u64, bool)>(key)).unwrap_or_default();
        let now = resp.ctx.cumulative_pass_nr();
        let count = if pass == now { count } else { 0 };
        let open = !done && count == n && resp.ctx.input(|i| i.time) > 2.5;
        resp.ctx.data_mut(|d| d.insert_temp(key, (now, count + 1, done || open)));
        if open {
            egui::Popup::open_id(&resp.ctx, id);
        }
    }
    let ui_frame = menu_frame_ctx(&resp.ctx);
    egui::Popup::menu(resp)
        .id(id)
        .gap(6.0)
        .align(if left { egui::RectAlign::BOTTOM_START } else { egui::RectAlign::BOTTOM_END })
        .frame(ui_frame)
        .style(menu_style)
        .show(|ui| {
            // Never narrower than what opened it (a dropdown hangs from its field).
            menu_width(ui, menu_floor(width).max(resp.rect.width() - 12.0));
            content(ui)
        })
        .map(|r| r.inner)
}

/// A right-click menu on `resp`, at the pointer.
pub fn context_menu<R>(resp: &Response, width: f32, content: impl FnOnce(&mut egui::Ui) -> R) -> Option<R> {
    let frame = menu_frame_ctx(&resp.ctx);
    egui::Popup::context_menu(resp)
        .frame(frame)
        .style(menu_style)
        .show(|ui| {
            menu_width(ui, menu_floor(width));
            content(ui)
        })
        .map(|r| r.inner)
}

/// How narrow a menu may be: it's as wide as its widest row, so `asked`
/// only keeps a short one from looking cramped (up to a point: a list of
/// short values stays small).
fn menu_floor(asked: f32) -> f32 {
    asked.min(180.0)
}

fn menu_frame_ctx(ctx: &egui::Context) -> egui::Frame {
    let p = theme::ctx_pal(ctx);
    egui::Frame::new()
        .fill(p.pop)
        .corner_radius(CARD_RADIUS)
        .stroke(Stroke::new(1.0, if p.dark { Color32::from_white_alpha(18) } else { Color32::from_black_alpha(20) }))
        .shadow(ctx.global_style().visuals.popup_shadow)
        .inner_margin(Margin::same(5))
}

pub fn single_line_job(text: &str, font: FontId, color: Color32, max_width: f32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_width);
    job
}

// --- Settings rows ---

/// A setting: its label (with ⓘ and a lock when given) and a live note under
/// it on the left, its control on the right. Rows in a group are separated by
/// hairlines; see [`list`].
pub struct Row<'a> {
    pub label: &'a str,
    pub note: Option<String>,
    pub warn: bool,
    pub tip: Option<(&'a str, &'a str)>,
    pub locked: Option<String>,
    /// A row inside a well ([`well`]): a little smaller.
    pub kid: bool,
}

impl<'a> Row<'a> {
    pub fn new(label: &'a str) -> Self {
        Self { label, note: None, warn: false, tip: None, locked: None, kid: false }
    }
    pub fn note(mut self, n: impl Into<String>) -> Self {
        let n = n.into();
        self.note = (!n.is_empty()).then_some(n);
        self
    }
    pub fn warn(mut self, w: bool) -> Self {
        self.warn = w;
        self
    }
    pub fn tip(mut self, title: &'a str, body: &'a str) -> Self {
        self.tip = Some((title, body));
        self
    }
    pub fn locked(mut self, why: Option<String>) -> Self {
        self.locked = why;
        self
    }
    pub fn kid(mut self) -> Self {
        self.kid = true;
        self
    }

    /// Draw it; the control is laid out right to left from the row's right
    /// edge. Returns what the control returns, and whether the locked control
    /// was clicked (to say why it's locked).
    pub fn show<R>(self, ui: &mut egui::Ui, control: impl FnOnce(&mut egui::Ui) -> R) -> (R, bool) {
        let p = theme::pal(ui);
        let (pad_x, min_h) = if self.kid { (12.0, 44.0) } else { (16.0, 52.0) };
        let locked = self.locked.is_some();
        let mut clicked_locked = false;
        let width = ui.available_width();
        let r = ui
            .allocate_ui_with_layout(vec2(width, min_h), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.set_min_height(min_h);
                ui.add_space(pad_x);
                let label_w = (width * 0.58).min(width - pad_x * 2.0 - 120.0).max(120.0);
                ui.allocate_ui_with_layout(vec2(label_w, min_h), egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.add_space(if self.note.is_some() { 9.0 } else { 0.0 });
                    ui.spacing_mut().item_spacing.y = 2.0;
                    if self.note.is_none() {
                        ui.add_space(((min_h - 20.0) / 2.0).max(0.0));
                    }
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 5.0;
                        let size = if self.kid { 13.5 } else { 14.0 };
                        ui.label(theme::text(self.label, size, W::Regular).color(if locked { p.text2 } else { p.text }));
                        if let Some((t, b)) = self.tip {
                            info(ui, t, b);
                        }
                        if let Some(why) = &self.locked {
                            ui.label(theme::ic(icon::LOCK_SIMPLE, 12.5).color(p.text3)).on_hover_text(why);
                        }
                    });
                    if let Some(n) = &self.note {
                        ui.add(egui::Label::new(theme::text(n, 12.5, W::Regular).color(if self.warn { p.warn } else { p.text3 })).wrap());
                        ui.add_space(8.0);
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(pad_x);
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if locked {
                        let inner = ui.add_enabled_ui(false, |ui| {
                            ui.multiply_opacity(0.7);
                            control(ui)
                        });
                        // Named after its row: every locked row's control
                        // column has the same id.
                        let resp = ui.interact(inner.response.rect, ui.id().with(("locked", self.label)), Sense::click()).on_hover_cursor(egui::CursorIcon::NotAllowed);
                        if let Some(why) = &self.locked {
                            let resp = resp.on_hover_text(why);
                            clicked_locked = resp.clicked();
                        }
                        inner.inner
                    } else {
                        control(ui)
                    }
                })
                .inner
            })
            .inner;
        (r, clicked_locked)
    }
}

/// A group of rows on the raised surface, hairlines between them.
pub fn list<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut List) -> R) -> R {
    let p = theme::pal(ui);
    egui::Frame::new()
        .fill(p.raise)
        .corner_radius(CARD_RADIUS)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            let mut l = List { ui, rows: 0 };
            add(&mut l)
        })
        .inner
}

/// Rows of a [`list`]: call [`List::row`] for each, [`List::well`] for the
/// settings that open under one.
pub struct List<'u> {
    pub ui: &'u mut egui::Ui,
    rows: usize,
}

impl List<'_> {
    pub fn row<R>(&mut self, row: Row, control: impl FnOnce(&mut egui::Ui) -> R) -> (R, bool) {
        if self.rows > 0 {
            divider(self.ui, 16.0);
        }
        self.rows += 1;
        row.show(self.ui, control)
    }

    /// Settings that only matter while the row above's switch is on: in a
    /// recessed well right under it, at full contrast. It opens and closes
    /// with the switch (the height slides).
    pub fn well(&mut self, id: impl std::hash::Hash + std::fmt::Debug, open: bool, add: impl FnOnce(&mut List)) {
        well(self.ui, id, open, add);
    }
}

/// A hairline across a group, inset `inset` from each side.
pub fn divider(ui: &mut egui::Ui, inset: f32) {
    let p = theme::pal(ui);
    let w = ui.available_width();
    let (r, _) = ui.allocate_exact_size(vec2(w, 1.0), Sense::hover());
    ui.painter().hline(r.x_range().shrink(inset), r.center().y, Stroke::new(1.0, p.line));
}

/// See [`List::well`].
pub fn well(ui: &mut egui::Ui, id: impl std::hash::Hash + std::fmt::Debug, open: bool, add: impl FnOnce(&mut List)) {
    let id = egui::Id::new(id);
    let t = theme::toggle(ui.ctx(), id, open, theme::NORMAL);
    if t <= 0.0 {
        return;
    }
    let p = theme::pal(ui);
    let full_h = ui.data(|d| d.get_temp::<f32>(id.with("h"))).unwrap_or(0.0);
    let h = if t >= 1.0 { None } else { Some(full_h * t) };
    let avail = ui.available_width();
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_min_size(ui.cursor().min, vec2(avail, 10_000.0))).layout(egui::Layout::top_down(egui::Align::Min)));
    child.set_clip_rect(match h {
        Some(h) => Rect::from_min_size(ui.cursor().min, vec2(avail, h)).intersect(ui.clip_rect()),
        None => ui.clip_rect(),
    });
    child.multiply_opacity(t);
    let inner = egui::Frame::new().outer_margin(Margin { left: 8, right: 8, top: 0, bottom: 8 }).fill(p.well).corner_radius(RADIUS).inner_margin(Margin::symmetric(0, 2)).show(&mut child, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 0.0;
        let mut l = List { ui, rows: 0 };
        add(&mut l);
    });
    let real_h = inner.response.rect.height() + 8.0;
    ui.data_mut(|d| d.insert_temp(id.with("h"), real_h));
    ui.allocate_space(vec2(avail, h.unwrap_or(real_h)));
}

// --- Dialogs ---

/// A dialog: a scrim over the window, the box rising in.
pub fn dialog<R>(ctx: &egui::Context, id: &str, width: f32, add: impl FnOnce(&mut egui::Ui) -> R) -> egui::ModalResponse<R> {
    let p = theme::ctx_pal(ctx);
    let id = egui::Id::new(id);
    // Shown last frame too: it carries on; else it rises in from now.
    // (The time is read first: egui's locks don't nest.)
    let pass = ctx.cumulative_pass_nr();
    let now = ctx.input(|i| i.time);
    let (seen, born) = ctx.data(|d| (d.get_temp::<u64>(id.with("seen")), d.get_temp::<f64>(id.with("born"))));
    let born = match (seen, born) {
        (Some(s), Some(b)) if s + 1 >= pass => b,
        _ => now,
    };
    ctx.data_mut(|d| {
        d.insert_temp(id.with("seen"), pass);
        d.insert_temp(id.with("born"), born);
    });
    let t = theme::since(ctx, born, theme::NORMAL);
    let frame = egui::Frame::new()
        .fill(p.pop)
        .corner_radius(10)
        .stroke(Stroke::new(1.0, if p.dark { Color32::from_white_alpha(18) } else { Color32::from_black_alpha(20) }))
        .shadow(ctx.global_style().visuals.popup_shadow)
        .inner_margin(Margin { left: 18, right: 18, top: 18, bottom: 14 });
    let modal = egui::Modal::new(id)
        .backdrop_color(theme::fade(p.scrim, t))
        .frame(frame)
        .area(egui::Modal::default_area(id).anchor(Align2::CENTER_CENTER, vec2(0.0, 8.0 * (1.0 - t))).fade_in(false));
    modal.show(ctx, |ui| {
        ui.multiply_opacity(t);
        ui.set_width(width);
        add(ui)
    })
}

/// A dialog's title.
pub fn dialog_title(ui: &mut egui::Ui, text: &str) {
    let p = theme::pal(ui);
    ui.label(theme::text(text, 15.0, W::Semibold).color(p.text));
    ui.add_space(6.0);
}

/// A dialog's body text.
pub fn dialog_text(ui: &mut egui::Ui, text: &str) {
    let p = theme::pal(ui);
    ui.add(egui::Label::new(theme::text(text, 13.5, W::Regular).color(p.text2)).wrap());
    ui.add_space(4.0);
}

/// A dialog's buttons, right-aligned, the main one last (given first, as
/// they're laid out right to left).
pub fn dialog_buttons(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(14.0);
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        add(ui);
    });
}

/// A dialog's ways out besides its buttons: Esc, or a click outside it
/// (which closes it and does nothing else: the page behind isn't clicked).
pub trait Dismissed {
    fn dismissed(&self) -> bool;
}

impl<T> Dismissed for egui::ModalResponse<T> {
    fn dismissed(&self) -> bool {
        self.should_close()
    }
}

/// A text field in the app's look: filled, outlined when focused.
pub fn text_field(ui: &mut egui::Ui, text: &mut String, hint: &str) -> egui::text_edit::TextEditOutput {
    let p = theme::pal(ui);
    let id = ui.next_auto_id();
    let focused = ui.memory(|m| m.has_focus(id));
    let fill = p.control;
    let out = egui::Frame::new()
        .fill(fill)
        .corner_radius(RADIUS)
        .stroke(Stroke::new(1.5, if focused { p.text2 } else { Color32::TRANSPARENT }))
        .inner_margin(Margin::symmetric(10, 7))
        .show(ui, |ui| {
            egui::TextEdit::singleline(text)
                .id(id)
                .frame(egui::Frame::NONE)
                .hint_text(theme::text(hint, 14.0, W::Regular).color(p.text3))
                .font(theme::font(14.0, W::Regular))
                .text_color(p.text)
                .desired_width(f32::INFINITY)
                .margin(Margin::ZERO)
                .show(ui)
        })
        .inner;
    out
}

// --- Value fields ---

/// A number you type in place ("90", "2:30", "1080", "Native"), with its unit
/// after it and the common values under its ∨. Enter or clicking away
/// applies; Esc puts it back; arrow keys step through the common values. A
/// value that isn't one of them just shows as itself.
pub struct ValueField<'a> {
    pub id: egui::Id,
    /// What it shows now.
    pub text: String,
    pub unit: &'a str,
    pub width: f32,
    pub hint: &'a str,
    /// The common values: (label, its value as text, a note).
    pub presets: Vec<(String, String, String)>,
    /// What the field's text currently is, among the presets, if any.
    pub current: Option<usize>,
    /// What "Type your own…" says about the range.
    pub range: &'a str,
}

/// What happened to a [`ValueField`] this frame.
pub enum Typed {
    None,
    /// Typed and applied: parse it.
    Text(String),
    /// A common value was picked (or stepped to with the arrows).
    Preset(usize),
}

impl ValueField<'_> {
    pub fn show(self, ui: &mut egui::Ui) -> Typed {
        let p = theme::pal(ui);
        let mut out = Typed::None;
        let edit_id = self.id.with("text");
        let editing_key = self.id.with("editing");
        let mut editing: Option<String> = ui.data(|d| d.get_temp(editing_key));
        let (rect, _) = ui.allocate_exact_size(vec2(self.width, CONTROL_H), Sense::hover());
        let focused = ui.memory(|m| m.has_focus(edit_id));
        let hovered = ui.rect_contains_pointer(rect);
        let menu_open = egui::Popup::is_id_open(ui.ctx(), self.id.with("dd").with("menu"));
        let hv = theme::toggle(ui.ctx(), self.id.with("hv"), (hovered || menu_open) && !focused, theme::FAST);
        let fc = theme::toggle(ui.ctx(), self.id.with("fc"), focused, theme::FAST);
        ui.painter().rect_filled(rect, RADIUS, theme::mix(p.control, p.control_h, hv));
        if fc > 0.0 {
            ui.painter().rect_stroke(rect, RADIUS, Stroke::new(1.5, theme::fade(p.text2, fc)), StrokeKind::Inside);
        }
        let dd = Rect::from_min_max(Pos2::new(rect.right() - 26.0, rect.top()), rect.max);
        let field = Rect::from_min_max(Pos2::new(rect.left() + 10.0, rect.top()), Pos2::new(dd.left(), rect.bottom()));
        let mut text = editing.clone().unwrap_or_else(|| self.text.clone());
        let font = theme::font(13.0, W::Medium);
        let tw = ui.painter().layout_no_wrap(if text.is_empty() { " ".into() } else { text.clone() }, font.clone(), p.text).size().x;
        let unit_w = if self.unit.is_empty() { 0.0 } else { ui.painter().layout_no_wrap(self.unit.to_owned(), font.clone(), p.text3).size().x + 4.0 };
        let input_w = (tw + 1.0).min(field.width() - unit_w).max(14.0);
        let mut fui = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_min_size(field.min, vec2(input_w, field.height()))).layout(egui::Layout::left_to_right(egui::Align::Center)));
        let resp = egui::TextEdit::singleline(&mut text)
            .id(edit_id)
            .frame(egui::Frame::NONE)
            .font(font.clone())
            .text_color(p.text)
            .desired_width(input_w)
            .margin(Margin::ZERO)
            .show(&mut fui)
            .response
            .response;
        let resp = resp.on_hover_text(self.hint).on_hover_cursor(egui::CursorIcon::Text);
        if !self.unit.is_empty() {
            ui.painter().text(Pos2::new(field.left() + input_w + 2.0, rect.center().y), Align2::LEFT_CENTER, self.unit, font, p.text3);
        }
        // Clicking anywhere in the box (not just on the digits) types.
        let box_resp = ui.interact(field, self.id.with("box"), Sense::click());
        if box_resp.clicked() {
            resp.request_focus();
        }
        if resp.gained_focus() {
            editing = Some(self.text.clone());
        }
        if resp.has_focus() {
            editing = Some(text.clone());
            let (up, down, esc) = ui.input_mut(|i| (i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp), i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown), i.key_pressed(egui::Key::Escape)));
            if esc {
                editing = None;
                resp.surrender_focus();
            } else if (up || down) && !self.presets.is_empty() {
                // Up goes to the next bigger one: presets are listed biggest
                // first or smallest first, so step by their order.
                let n = self.presets.len();
                let at = self.current.unwrap_or(0);
                let next = if up { at.saturating_sub(1) } else { (at + 1).min(n - 1) };
                out = Typed::Preset(next);
                editing = Some(self.presets[next].1.clone());
            }
        }
        if resp.lost_focus() {
            if let Some(t) = editing.take() {
                if t.trim() != self.text.trim() {
                    out = Typed::Text(t);
                }
            }
        }
        ui.data_mut(|d| match &editing {
            Some(t) => {
                d.insert_temp(editing_key, t.clone());
            }
            None => d.remove::<String>(editing_key),
        });
        // The ∨: common values, and "Type your own…".
        let ddr = ui.interact(dd, self.id.with("dd"), Sense::click());
        let dhv = theme::toggle(ui.ctx(), ddr.id.with("hv"), ddr.hovered() || menu_open, theme::FAST);
        if dhv > 0.0 {
            ui.painter().rect_filled(dd, CornerRadius { nw: 0, sw: 0, ne: RADIUS, se: RADIUS }, theme::fade(p.control_h, dhv));
        }
        let turn = theme::toggle(ui.ctx(), ddr.id.with("turn"), menu_open, theme::NORMAL);
        chevron(ui.painter(), dd.center(), 3.5, turn, theme::mix(p.text3, p.text, dhv));
        let ddr = ddr.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Common values");
        let mut focus = false;
        // As wide as the field (wider when a value's note needs it).
        menu_popup(&ddr, false, self.width, |ui| {
            for (i, (label, _, note)) in self.presets.iter().enumerate() {
                let mut item = Item::new(label.clone()).check(self.current == Some(i));
                if !note.is_empty() {
                    item = item.value(note.clone());
                }
                if item.show(ui).clicked() {
                    out = Typed::Preset(i);
                }
            }
            if self.current.is_none() {
                menu_sep(ui);
                Item::new(format!("{}{}{}", self.text, if self.unit.is_empty() { "" } else { " " }, self.unit)).check(true).value("yours").show(ui);
            }
            menu_sep(ui);
            if Item::new("Type your own…").icon(icon::TEXT_T).value(self.range.to_owned()).show(ui).clicked() {
                focus = true;
            }
        });
        if focus {
            ui.memory_mut(|m| m.request_focus(edit_id));
        }
        out
    }
}

// --- Pointer ---

/// The pointing hand over anything that can be clicked. egui only does it
/// for plain buttons; this covers every other widget (checkboxes, dropdowns,
/// options, sliders, drawn rows and tiles). Call once a frame, after the UI:
/// a cursor something else asked for (text, resize, hidden) is kept. Large
/// areas (a dialog's backdrop, a video picture) are left alone.
pub fn pointer_cursor(ctx: &egui::Context) {
    if ctx.output(|o| o.cursor_icon) != egui::CursorIcon::Default {
        return;
    }
    let screen = ctx.content_rect().area();
    let hovered: Vec<egui::Id> = ctx.interaction_snapshot(|s| s.hovered.iter().copied().collect());
    let clickable = hovered.into_iter().filter_map(|id| ctx.read_response(id)).any(|r| r.enabled() && r.sense.senses_click() && r.rect.area() < screen / 4.0);
    if clickable {
        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
    }
}

/// A press that should still count as a click: pressed on a button (or any
/// click-only control) and let go over it, but moved more than egui allows
/// for a click (6 points) on the way. Browsers and the OS click then; egui
/// doesn't, so buttons felt unresponsive. Found after the frame by
/// [`lenient_clicks`], replayed as a clean click next frame by
/// [`replay_click`] (from the app's `raw_input_hook`). Drags are untouched:
/// only controls that can't be dragged are considered.
#[derive(Clone, Copy)]
struct Press {
    widget: egui::Id,
    /// Where it'll be clicked: where it was let go.
    at: Option<Pos2>,
}

fn press_id() -> egui::Id {
    egui::Id::new("ui_kit_lenient_press")
}

/// Call once a frame, after the UI.
pub fn lenient_clicks(ctx: &egui::Context) {
    let (pressed, released, pos) = ctx.input(|i| (i.pointer.primary_pressed(), i.pointer.primary_released(), i.pointer.latest_pos()));
    let click_only = |id: egui::Id| ctx.read_response(id).filter(|r| r.enabled() && r.sense.senses_click() && !r.sense.senses_drag());
    if pressed {
        let hovered: Vec<egui::Id> = ctx.interaction_snapshot(|s| s.hovered.iter().copied().collect());
        let widget = hovered.into_iter().find(|id| click_only(*id).is_some());
        ctx.data_mut(|d| match widget {
            Some(widget) => {
                d.insert_temp(press_id(), Press { widget, at: None });
            }
            None => d.remove::<Press>(press_id()),
        });
    }
    if released {
        let Some(press) = ctx.data(|d| d.get_temp::<Press>(press_id())) else { return };
        let clicked = ctx.interaction_snapshot(|s| s.clicked == Some(press.widget));
        let over = pos.zip(click_only(press.widget)).is_some_and(|(p, r)| r.rect.contains(p));
        ctx.data_mut(|d| match (clicked, over, pos) {
            (false, true, Some(at)) => {
                d.insert_temp(press_id(), Press { at: Some(at), ..press });
            }
            _ => d.remove::<Press>(press_id()),
        });
        if !clicked && over {
            ctx.request_repaint();
        }
    }
}

/// Call from the app's `raw_input_hook`: a click [`lenient_clicks`] found
/// becomes a press and release in place, which egui clicks.
pub fn replay_click(ctx: &egui::Context, raw: &mut egui::RawInput) {
    let Some(Press { at: Some(at), .. }) = ctx.data(|d| d.get_temp::<Press>(press_id())) else { return };
    ctx.data_mut(|d| d.remove::<Press>(press_id()));
    let modifiers = ctx.input(|i| i.modifiers);
    raw.events.push(egui::Event::PointerMoved(at));
    for pressed in [true, false] {
        raw.events.push(egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A click outside a dialog closes it without clicking the page behind.
    #[test]
    fn a_click_outside_closes_a_dialog() {
        let ctx = egui::Context::default();
        let (mut clicks, mut closed) = (0, 0);
        let frame = |events: Vec<egui::Event>, clicks: &mut i32, closed: &mut i32| {
            let mut raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0))), events, ..Default::default() };
            replay_click(&ctx, &mut raw);
            let mut out = ctx.run_ui(raw, |ui| {
                if ui.put(egui::Rect::from_min_size(egui::pos2(10.0, 10.0), egui::vec2(60.0, 30.0)), egui::Button::new("Back")).clicked() {
                    *clicks += 1;
                }
                if egui::Modal::new(egui::Id::new("dialog")).show(ui.ctx(), |ui| ui.label("A dialog")).dismissed() {
                    *closed += 1;
                }
                lenient_clicks(ui.ctx());
            });
            out.textures_delta.clear();
        };
        let at = egui::pos2(30.0, 25.0);
        let button = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
        for events in [vec![egui::Event::PointerMoved(at)], vec![], vec![button(true)], vec![button(false)], vec![], vec![]] {
            frame(events, &mut clicks, &mut closed);
        }
        assert_eq!(clicks, 0, "the click behind it doesn't land");
        assert_eq!(closed, 1, "it closes the dialog");
    }

    /// Pressed on a button, moved 20 points, let go over it: egui alone
    /// doesn't click; with the replay, it does (once).
    /// Shown for the first time and after: no lock taken inside another
    /// (egui's locks don't nest; it hung the app on the first dialog).
    #[test]
    fn a_dialog_shows() {
        let ctx = egui::Context::default();
        apply(&ctx);
        let mut shown = 0;
        for _ in 0..3 {
            let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0))), ..Default::default() };
            let mut out = ctx.run_ui(raw, |ui| {
                dialog(ui.ctx(), "test_dialog", 300.0, |ui| {
                    dialog_title(ui, "A dialog");
                    shown += 1;
                });
            });
            out.textures_delta.clear();
        }
        assert_eq!(shown, 3);
    }

    #[test]
    fn a_wobbly_click_still_clicks() {
        let ctx = egui::Context::default();
        let mut clicks = 0;
        let frame = |events: Vec<egui::Event>, clicks: &mut i32| {
            let mut raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 300.0))), events, ..Default::default() };
            replay_click(&ctx, &mut raw);
            let mut out = ctx.run_ui(raw, |ui| {
                if ui.put(egui::Rect::from_min_size(egui::pos2(20.0, 20.0), egui::vec2(160.0, 30.0)), egui::Button::new("Save")).clicked() {
                    *clicks += 1;
                }
                lenient_clicks(ui.ctx());
            });
            out.textures_delta.clear();
        };
        let at = |x: f32| egui::pos2(x, 35.0);
        let button = |x: f32, pressed: bool| egui::Event::PointerButton { pos: at(x), button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
        frame(vec![egui::Event::PointerMoved(at(40.0))], &mut clicks);
        frame(vec![egui::Event::PointerMoved(at(40.0))], &mut clicks);
        frame(vec![button(40.0, true)], &mut clicks);
        frame(vec![egui::Event::PointerMoved(at(60.0))], &mut clicks);
        frame(vec![button(60.0, false)], &mut clicks);
        assert_eq!(clicks, 0, "egui alone doesn't click after that much movement");
        frame(vec![], &mut clicks);
        assert_eq!(clicks, 1, "the replay clicks");
        frame(vec![], &mut clicks);
        assert_eq!(clicks, 1, "once");
    }

    /// Shortcuts read as compact keys.
    #[test]
    fn keys_read_compactly() {
        let k = |v: &[&str]| key_text(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        if cfg!(target_os = "macos") {
            assert_eq!(k(&["Option", "F10"]), format!("{}F10", icon::OPTION));
        }
        assert_eq!(k(&["F8"]), "F8");
    }
}
