//! The app's building blocks, drawn from the tokens in `theme`: buttons,
//! switches, segmented controls, menus, setting rows, keycaps. Every page is
//! made of these, so the same kind of choice looks and behaves the same
//! everywhere.
//!
//! - **One control per kind of choice**: a switch for on and off, a segmented
//!   control for two to four short choices, a menu for longer lists, ⓘ for
//!   the why behind a setting.
//! - **Inputs guide**: what kind of value it is decides its input, so the
//!   common values are offered and nothing that makes no sense can be
//!   typed. Copy how Settings does the same kind of value.
//!   - *Picked, or partly words* ("Original", "Auto", "Off", "1080p"): a
//!     menu, a [`ChoiceField`] (or [`select`] with [`menu_popup`]); a
//!     segmented control when it's two to four short ones. A word is only
//!     ever picked: it's never in a box that can be clicked into and typed
//!     over.
//!   - *Picked, or a number of your own*: a [`ChoiceField`] with its
//!     [`Custom`]: Custom… at the end of the menu puts a box for the number
//!     beside it, its unit after it (Settings' Resolution and Aspect ratio).
//!   - *Always a number* (a frame rate, a bitrate, a size in MB): a
//!     [`ValueField`]: typed straight in, its unit after it, the common
//!     values under its ∨; a length is [`ValueField::show_time`], a clock.
//!     Two numbers that go together (a size, a ratio) are a [`number_box`]
//!     each ([`size_field`], [`ratio_field`]).
//!   - *Free text* (a name, a search): [`text_field`]. The only box where
//!     letters go in.
//!   - Numbers only take digits (and a point where they take decimals), and
//!     one beyond its range is kept to it, so typing can't go wrong: no
//!     error to read afterwards. Never egui's own `DragValue` or `Slider`,
//!     or a bare `TextEdit` for a value: `clippy.toml` turns them away.
//! - **Buttons**: main (one per screen, the brightest), normal, quiet
//!   (ghost), destructive. 30 px tall, corners of 6.
//! - **Can't be used now**: a control with nothing to do (an empty undo, a
//!   reset with nothing to reset) is dimmed, keeping its shape
//!   ([`Btn::enabled`]). One held back by what the app's doing (clipping,
//!   recording) is locked: dimmed, with a lock, saying why on hover and in a
//!   toast when clicked ([`Btn::locked`], [`Row::locked`]). Anything that
//!   works now is left usable, even when it changes something not running.
//! - **Space** in steps of 4; corners 4 for small marks, 6 for controls, 8
//!   for groups and menus. Only switches, the play button and dots are round.
//! - **Motion**: hover fades in, a chosen option slides to its place, a
//!   button that changes its words changes its width smoothly.

use egui::{Align2, Color32, CornerRadius, FontId, Margin, Pos2, Rect, Response, Sense, Stroke, StrokeKind, TextStyle, Vec2, vec2};



use crate::theme::{self, Pal, W, icon};

pub const CONTROL_H: f32 = 30.0;
pub const SMALL_H: f32 = 26.0;
/// Buttons, fields, rows.
pub const RADIUS: u8 = 6;
/// Groups, menus, popovers.
pub const CARD_RADIUS: u8 = 8;
/// A setting row: its padding at each end, and its least height.
pub const ROW_PAD: f32 = 16.0;
pub const ROW_H: f32 = 52.0;
/// A row in a well ([`well`]): a little smaller.
pub const WELL_ROW_PAD: f32 = 12.0;
pub const WELL_ROW_H: f32 = 44.0;
/// A well's gap from its group's sides and bottom, and above and below
/// its rows.
pub const WELL_GAP: f32 = 8.0;
pub const WELL_INSET: f32 = 2.0;
/// The hairline between rows, in from each side of what they're in.
pub const ROW_LINE_INSET: f32 = 16.0;
/// Space between a page's edge and its content.
pub const PAGE_MARGIN: i8 = 24;
/// The search field's width, the same on every page that has one.
pub const SEARCH_W: f32 = 160.0;
/// Between cards stacked in a column, on every page.
pub const STACK_GAP: f32 = 16.0;
/// Above a heading that starts another part of a page.
pub const SECTION_GAP: f32 = 32.0;

/// Leave exactly `px` between what's above and what comes next (egui adds
/// its item spacing between the two on top of any space added).
pub fn gap(ui: &mut egui::Ui, px: f32) {
    ui.add_space((px - ui.spacing().item_spacing.y).max(0.0));
}

/// A heading over a part of a page (Settings' groups, Voice samples'
/// sharing): small and quiet, [`SECTION_GAP`] below what's above it (less
/// at the top of a page), with anything `extra` adds beside it.
pub fn section_head(ui: &mut egui::Ui, text: &str, first: bool, extra: impl FnOnce(&mut egui::Ui)) -> Response {
    let p = theme::pal(ui);
    gap(ui, if first { 10.0 } else { SECTION_GAP });
    let r = ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            ui.label(theme::text(text, 13.0, W::Semibold).color(p.text2));
            extra(ui);
        })
        .response;
    ui.add_space(6.0);
    r
}

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

// --- Size changes ---

#[derive(Clone, Copy)]
struct Eased {
    /// The content's own height and the width last frame.
    natural: f32,
    width: f32,
    /// Where the height was and is going, and since when.
    from: f32,
    to: f32,
    start: f64,
    pass: u64,
}

fn moving_id() -> egui::Id {
    egui::Id::new("ui_kit_eased_moving")
}

/// Something in an [`eased_frame`] is changing size smoothly by itself this
/// frame (a well opening, a [`reveal`]): the frame follows it exactly
/// instead of easing again behind it.
fn moving(ctx: &egui::Context) {
    let pass = ctx.cumulative_pass_nr();
    ctx.data_mut(|d| d.insert_temp(moving_id(), pass));
}

/// `frame` with `add` in it, its height easing to the content's whenever it
/// changes (a line of text appearing, a note going away), so what's below
/// slides instead of jumping; the content is clipped to it meanwhile. At once
/// when it first appears, when the width changes (the window resizing) and
/// while what's in it moves smoothly by itself. `id` must stay the same while
/// things around it change (egui's own ids shift when something above
/// appears).
pub fn eased_frame<R>(ui: &mut egui::Ui, id: egui::Id, frame: egui::Frame, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let ctx = ui.ctx().clone();
    let (now, pass) = (ctx.input(|i| i.time), ctx.cumulative_pass_nr());
    let under = ui.painter().add(egui::Shape::Noop);
    let outer = ui.available_rect_before_wrap();
    let margin = egui::epaint::MarginF32::from(frame.inner_margin) + egui::epaint::MarginF32::from(frame.stroke.width) + egui::epaint::MarginF32::from(frame.outer_margin);
    let width = outer.width();
    let inner_max = Rect::from_min_max(outer.min + margin.left_top(), Pos2::new(outer.max.x - margin.right, outer.min.y + 100_000.0));
    let mut content = ui.new_child(egui::UiBuilder::new().max_rect(inner_max).layout(*ui.layout()));
    let before = ctx.data(|d| d.get_temp::<Eased>(id));
    let clip_to = |content: &mut egui::Ui, h: f32| {
        let r = Rect::from_min_size(outer.min, vec2(width, h)) - egui::epaint::MarginF32::from(frame.outer_margin);
        content.set_clip_rect(r.intersect(ui.clip_rect()));
    };
    if let Some(e) = before {
        clip_to(&mut content, eased_now(&e, now));
    }
    let was_moving = ctx.data(|d| d.get_temp::<u64>(moving_id()));
    let r = add(&mut content);
    let inside_moved = ctx.data(|d| d.get_temp::<u64>(moving_id())) != was_moving;
    let natural = content.min_rect().height() + margin.sum().y;
    let next = match before {
        Some(e) if e.natural == natural && e.width == width => Eased { pass, ..e },
        // Moving by itself, or after a while unseen, or another width: there at once.
        Some(e) if !inside_moved && e.width == width && e.pass + 1 >= pass => Eased { natural, width, from: eased_now(&e, now), to: natural, start: now, pass },
        _ => Eased { natural, width, from: natural, to: natural, start: now, pass },
    };
    ctx.data_mut(|d| d.insert_temp(id, next));
    let h = eased_now(&next, now);
    if h != natural {
        ctx.request_repaint();
        // A frame inside another: that one follows this one.
        moving(&ctx);
    }
    clip_to(&mut content, h);
    let rect = Rect::from_min_size(outer.min, vec2(width, h));
    let fill = rect - egui::epaint::MarginF32::from(frame.outer_margin);
    if ui.is_rect_visible(fill) {
        let mut f = frame;
        f.inner_margin = Margin::ZERO;
        f.outer_margin = Margin::ZERO;
        ui.painter().set(under, f.paint(fill));
    }
    ui.allocate_rect(rect, Sense::hover());
    r
}

fn eased_now(e: &Eased, now: f64) -> f32 {
    let t = ((now - e.start) as f32 / theme::NORMAL).clamp(0.0, 1.0);
    e.from + (e.to - e.from) * theme::ease_out(t)
}

/// A group: content on the raised surface, padded. Its height eases when
/// what's in it changes (see [`eased_frame`]); `id` names it on its page.
pub fn card<R>(ui: &mut egui::Ui, id: impl std::hash::Hash + std::fmt::Debug, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let frame = egui::Frame::new().fill(theme::pal(ui).raise).corner_radius(CARD_RADIUS).inner_margin(Margin::symmetric(16, 12));
    eased_frame(ui, egui::Id::new(("card", id)), frame, |ui| {
        ui.set_width(ui.available_width());
        add(ui)
    })
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
    } else if let Some(y) = dev_scroll(ui.ctx(), id) {
        scroll = scroll.vertical_scroll_offset(y);
    }
    scroll
        .show(&mut area, |ui| {
            ui.set_max_width(ui.available_width() - gutter);
            add(ui)
        })
        .inner
}

/// Dev aid: `HASTECLIP_DEV_SCROLL=<area>:<from s>:<secs>:<points>` scrolls
/// the [`scroll`] area `<area>` steadily down by `<points>` over `<secs>`,
/// starting `<from s>` after launch, as a trackpad would: to measure
/// scrolling without touching it (with `HASTECLIP_DEV_LAPS`).
fn dev_scroll(ctx: &egui::Context, id: &str) -> Option<f32> {
    static PLAN: std::sync::OnceLock<Option<(String, f64, f64, f32)>> = std::sync::OnceLock::new();
    let plan = PLAN.get_or_init(|| {
        let spec = std::env::var("HASTECLIP_DEV_SCROLL").ok()?;
        let mut it = spec.split(':');
        Some((it.next()?.to_owned(), it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?))
    });
    let (area, from, secs, points) = plan.as_ref()?;
    if area != id {
        return None;
    }
    let t = ((ctx.input(|i| i.time) - from) / secs) as f32;
    if !(0.0..=1.0).contains(&t) {
        return None;
    }
    ctx.request_repaint();
    Some(points * t)
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

/// Another page is shown from this frame: its content arrives with
/// [`arrive`]. Set by the app.
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
    // Quick and small: a page that's drawn at once shouldn't look like it's
    // still coming (a slower one read as a slow page).
    const RISE: f32 = 6.0;
    let ctx = ui.ctx().clone();
    let Some(at) = ctx.data(|d| d.get_temp::<f64>(page_arrived_id())) else { return add(ui) };
    let t = theme::since(&ctx, at, theme::FAST + 0.03);
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
    /// Going live, and the stream's end: streaming's purple.
    Live,
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
    /// The icon is from the filled set (`egui_phosphor::fill`), so it's drawn in that family.
    filled: bool,
    label: String,
    trail: Option<&'a str>,
    keys: Option<Vec<String>>,
    small: bool,
    min_w: f32,
    tip: Option<String>,
    enabled: bool,
    /// Held back by what the app's doing, and why.
    locked: Option<String>,
    corners: CornerRadius,
    /// Drawn this wide, whatever's in it.
    exact_w: Option<f32>,
    id: Option<egui::Id>,
    progress: Option<f32>,
}

impl<'a> Btn<'a> {
    pub fn new(kind: Kind, label: impl Into<String>) -> Self {
        Self {
            kind,
            icon: None,
            filled: false,
            label: label.into(),
            trail: None,
            keys: None,
            small: false,
            min_w: 0.0,
            tip: None,
            enabled: true,
            locked: None,
            corners: CornerRadius::same(RADIUS),
            exact_w: None,
            id: None,
            progress: None,
        }
    }
    /// Its corners, to join it to a button beside it (a split button: Save
    /// and its ▾).
    pub fn corners(mut self, c: CornerRadius) -> Self {
        self.corners = c;
        self
    }
    /// Drawn this wide (a split button's narrow ▾).
    pub fn exact_width(mut self, w: f32) -> Self {
        self.exact_w = Some(w);
        self
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
    pub fn filled(mut self) -> Self {
        self.filled = true;
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
    /// Off while there's nothing for it to do (an empty undo): dimmed,
    /// keeping its shape. For one held back by what the app's doing, see
    /// [`Btn::locked`].
    pub fn enabled(mut self, on: bool) -> Self {
        self.enabled = on;
        self
    }
    /// Held back by what the app's doing (`Some(why)`: "Stop clipping
    /// first"), as a setting row's [`Row::locked`]: dimmed, a lock in place
    /// of its icon, why on hover, and in a toast when it's clicked (the app
    /// shows it: [`take_locked_click`]). It never reports a click.
    pub fn locked(mut self, why: Option<String>) -> Self {
        self.locked = why;
        self
    }
    /// A stable id, so the width animates across frames even when the
    /// button's place in the layout changes.
    pub fn id(mut self, id: impl std::hash::Hash + std::fmt::Debug) -> Self {
        self.id = Some(egui::Id::new(id));
        self
    }

    fn icon_font(&self, size: f32) -> FontId {
        if self.filled && self.locked.is_none() { FontId::new(size, theme::fill_family()) } else { FontId::proportional(size) }
    }

    /// Its icon as drawn: a lock while it's locked.
    fn shown_icon(&self) -> Option<&'a str> {
        if self.locked.is_some() { Some(icon::LOCK_SIMPLE) } else { self.icon }
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
        let icon_w = self.shown_icon().map(|i| text_w(i, self.icon_font(icon_size)));
        let trail_w = self.trail.map(|i| text_w(i, FontId::proportional(icon_size - 1.5)));
        let keys_w = self.keys.as_ref().map(|k| key_hint_width(ui, k, 11.0));
        let only_icon = label_w.is_none() && trail_w.is_none() && keys_w.is_none();
        if let Some(w) = self.exact_w {
            return w;
        }
        if only_icon {
            return h;
        }
        let pad_l = if self.shown_icon().is_some() { 10.0 } else if self.small { 9.0 } else { 12.0 };
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
        let icon_g = self.shown_icon().map(|i| ui.painter().layout_no_wrap(i.to_owned(), self.icon_font(icon_size), Color32::PLACEHOLDER));
        let trail_g = self.trail.map(|i| ui.painter().layout_no_wrap(i.to_owned(), FontId::proportional(icon_size - 1.5), Color32::PLACEHOLDER));
        let keys_w = self.keys.as_ref().map_or(0.0, |k| key_hint_width(ui, k, 11.0));
        let gap = 7.0;
        let only_icon = galley.is_none() && trail_g.is_none() && self.keys.is_none();
        let pad_l = if only_icon { 0.0 } else if self.shown_icon().is_some() { 10.0 } else if self.small { 9.0 } else { 12.0 };
        let pad_r = if only_icon { 0.0 } else if self.keys.is_some() || self.trail.is_some() { 8.0 } else if self.small { 9.0 } else { 12.0 };
        let mut content = 0.0;
        let mut parts = 0;
        for w in [icon_g.as_ref().map(|g| g.size().x), galley.as_ref().map(|g| g.size().x), trail_g.as_ref().map(|g| g.size().x), (keys_w > 0.0).then_some(keys_w)].into_iter().flatten() {
            content += w;
            parts += 1;
        }
        content += gap * (parts.max(1) - 1) as f32;
        let target_w = self.exact_w.unwrap_or(if only_icon { h } else { (pad_l + content + pad_r).max(self.min_w) });
        let id = self.id.unwrap_or_else(|| ui.next_auto_id());
        let w = theme::value(ui.ctx(), id.with("w"), target_w, theme::NORMAL);
        let usable = self.enabled && self.locked.is_none();
        let sense = if usable { Sense::click() } else { Sense::hover() };
        let (rect, mut resp) = ui.allocate_exact_size(vec2(w, h), sense);
        if ui.is_rect_visible(rect) {
            let hovered = usable && resp.hovered();
            let pressed = usable && resp.is_pointer_button_down_on();
            let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), hovered, theme::FAST);
            let (fill, fg) = match self.kind {
                Kind::Primary => (theme::mix(p.primary, if p.dark { Color32::from_gray(255) } else { Color32::from_gray(60) }, hv * 0.25), p.primary_fg),
                Kind::Normal => (theme::mix(p.control, p.control_h, hv), p.text),
                Kind::Ghost => (theme::fade(p.sel, hv * if p.dark { 1.0 } else { 0.0 }).blend(theme::fade(p.control, if p.dark { 0.0 } else { hv })), theme::mix(p.text2, p.text, hv)),
                Kind::Stop => (theme::mix(p.rec, Color32::WHITE, hv * 0.12), Color32::WHITE),
                Kind::Live => (theme::mix(p.stream, Color32::WHITE, hv * 0.12), Color32::WHITE),
                Kind::DangerText => (theme::fade(p.sel, hv).blend(theme::fade(p.control, if p.dark { 0.0 } else { hv })), p.danger),
                Kind::On => (p.seg_on, p.text),
            };
            // Not usable now, it keeps its shape, dimmed: still a button, not
            // text (a quiet one has no fill to keep).
            let fg = if usable { fg } else { theme::fade(fg, 0.38) };
            let fill = if usable { fill } else { theme::fade(fill, 0.6) };
            let rect_d = if pressed { rect.shrink(0.5) } else { rect };
            ui.painter().rect_filled(rect_d, self.corners, fill);
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
        crate::demo::mark(ui.ctx(), || format!("button:{}", if self.label.is_empty() { self.tip.as_deref().unwrap_or("") } else { &self.label }), resp.rect);
        if let Some(why) = &self.locked {
            // Its own click, apart from the button's: it says why, and the
            // button reports none.
            let lock = ui.interact(rect, resp.id.with("locked"), Sense::click()).on_hover_cursor(egui::CursorIcon::NotAllowed).on_hover_text(why);
            if lock.clicked() {
                ui.ctx().data_mut(|d| d.insert_temp(egui::Id::new(LOCKED_CLICK), why.clone()));
                ui.ctx().request_repaint();
            }
            return resp;
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

/// Where a locked button's reason waits for the app, once it's clicked.
const LOCKED_CLICK: &str = "locked_click";

/// Whether the [`Row::go`] button of the row named `label` was clicked,
/// once.
pub fn take_row_go(ctx: &egui::Context, label: &str) -> bool {
    ctx.data_mut(|d| d.remove_temp::<bool>(egui::Id::new(("row_go", label)))).is_some()
}

/// Why a locked button that was clicked is locked, once: the app shows it
/// in a toast, wherever the button is ([`Btn::locked`]).
pub fn take_locked_click(ctx: &egui::Context) -> Option<String> {
    ctx.data_mut(|d| {
        let why = d.get_temp::<String>(egui::Id::new(LOCKED_CLICK));
        d.remove::<String>(egui::Id::new(LOCKED_CLICK));
        why
    })
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
    // No keys: "Off", as words, not on a key (it isn't one).
    let off = !listening && keys.is_empty();
    let plain = listening || off;
    let caps: Vec<String> = if listening {
        vec!["Press keys…".into()]
    } else if off {
        vec!["Off".into()]
    } else {
        keys.iter().map(|k| key_text(std::slice::from_ref(k))).collect()
    };
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
        if !plain {
            painter.rect_filled(cap.translate(vec2(0.0, 1.0)), 4, p.line2);
            painter.rect_filled(cap, 4, if p.dark { p.raise } else { Color32::WHITE });
        }
        painter.galley(cap.center() - g.size() / 2.0, g, if plain { p.text2 } else { p.text });
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
    // The chosen pill slides to its option and to its width. What eases is
    // which option it's on (a fraction of the way between them), not points:
    // the control stretching with the window takes the pill along at once.
    if xs.get(chosen).is_some() {
        let at = theme::value(ui.ctx(), base_id.with("at"), chosen as f32, theme::NORMAL);
        let (lo, hi) = (at.floor() as usize, (at.ceil() as usize).min(xs.len() - 1));
        let f = at - lo as f32;
        let px = xs[lo] + (xs[hi] - xs[lo]) * f - inner.left();
        let pw = ws[lo] + (ws[hi] - ws[lo]) * f;
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
        crate::demo::mark(ui.ctx(), || format!("button:{}", if o.label.is_empty() { o.tip.unwrap_or("") } else { o.label }), r);
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
    // Never wider than there's room for: a long value is cut short (…).
    let chrome = 11.0 + 10.0 + 14.0 + 8.0;
    let room = ui.available_width().max(min_w.min(ui.available_width()));
    let g = ui.painter().layout_job(single_line_job(value, font, p.text, (room - chrome).max(20.0)));
    let target = (g.size().x + chrome).max(min_w).min(room);
    let id = ui.next_auto_id();
    let w = theme::value(ui.ctx(), id.with("w"), target, theme::NORMAL);
    let (rect, resp) = ui.allocate_exact_size(vec2(w, CONTROL_H), Sense::click());
    crate::demo::mark(ui.ctx(), || format!("select:{value}"), rect);
    let open = egui::Popup::is_id_open(ui.ctx(), resp.id.with("menu"));
    let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered() || open, theme::FAST);
    let enabled = ui.is_enabled();
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, RADIUS, theme::mix(p.control, p.control_h, hv));
    let tc = if enabled { p.text } else { theme::fade(p.text, 0.55) };
    let elided = g.elided;
    painter.galley(Pos2::new(rect.left() + 11.0, rect.center().y - g.size().y / 2.0), g, tc);
    let resp = if elided { resp.on_hover_text(value) } else { resp };
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
    /// A picture in the icon's place (an app's own icon).
    image: Option<egui::TextureId>,
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
        Self { icon: None, image: None, check: None, label: label.into(), right: None, keys: None, danger: false, enabled: true, tip: None }
    }
    pub fn icon(mut self, icon: &'a str) -> Self {
        self.icon = Some(icon);
        self
    }
    pub fn image(mut self, tex: Option<egui::TextureId>) -> Self {
        self.image = tex;
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
        let lead = if self.icon.is_some() || self.image.is_some() || self.check.is_some() { 24.0 } else { 0.0 };
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
        crate::demo::mark(ui.ctx(), || format!("button:{}", self.label), rect);
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
        } else if let Some(tex) = self.image {
            let r = Rect::from_center_size(Pos2::new(x + 7.5, rect.center().y), vec2(17.0, 17.0));
            egui::Image::from_texture((tex, r.size())).tint(if self.enabled { Color32::WHITE } else { Color32::from_white_alpha(100) }).paint_at(ui, r);
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

/// The menu being laid out: its width, and the widest row seen so far this
/// frame (by its id). Its rows are all as wide as its widest; a popup keeps
/// the size of its first frame, so a row may not fill the room around it
/// (that made every menu as wide as the window). Menus inside menus
/// (submenus) stack.
#[derive(Clone, Copy, Default)]
struct MenuWidth {
    asked: f32,
    widest: f32,
    seen: f32,
}

thread_local! {
    static MENUS: std::cell::RefCell<Vec<egui::Id>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The width rows of the menu being laid out are drawn at.
fn menu_min_width(ui: &egui::Ui) -> f32 {
    let Some(key) = MENUS.with(|m| m.borrow().last().copied()) else { return 0.0 };
    ui.data(|d| d.get_temp::<MenuWidth>(key)).map_or(0.0, |m| m.asked.max(m.widest))
}

/// A row that needs `w` was laid out in the menu being laid out.
fn menu_saw(ui: &egui::Ui, w: f32) {
    let Some(key) = MENUS.with(|m| m.borrow().last().copied()) else { return };
    ui.data_mut(|d| {
        let m = d.get_temp_mut_or_default::<MenuWidth>(key);
        m.seen = m.seen.max(w);
    });
}

/// Lay out a menu's content: `add` at `w` wide, or as wide as its widest row
/// (from last frame) when that's wider.
pub fn menu_body<R>(ui: &mut egui::Ui, w: f32, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let key = ui.id().with("ui_kit_menu_w");
    let m = ui.data_mut(|d| {
        let m = d.get_temp_mut_or_default::<MenuWidth>(key);
        if m.seen > 0.0 {
            m.widest = m.seen;
        }
        m.seen = 0.0;
        m.asked = w;
        *m
    });
    ui.set_width(m.asked.max(m.widest));
    MENUS.with(|s| s.borrow_mut().push(key));
    let r = add(ui);
    MENUS.with(|s| s.borrow_mut().pop());
    r
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
        .show(ui, &resp, |ui| menu_body(ui, menu_floor(220.0), content))
        .map(|r| r.inner)
}

/// A divider between a menu's groups.
pub fn menu_sep(ui: &mut egui::Ui) {
    let p = theme::pal(ui);
    // One band, the line in its middle, from the rows' text on one side to
    // the other's (as macOS draws them).
    let w = menu_min_width(ui).max(ui.available_width()).max(1.0);
    let (r, _) = ui.allocate_exact_size(vec2(w, 9.0), Sense::hover());
    let y = (r.center().y * ui.ctx().pixels_per_point()).round() / ui.ctx().pixels_per_point() + 0.5 / ui.ctx().pixels_per_point();
    ui.painter().hline(r.x_range().shrink(9.0), y, Stroke::new(1.0, p.line2));
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
    // Dev aid: `HASTECLIP_OPEN_MENU=<n>` opens the n-th menu button drawn
    // (from 0) once, to see it without clicking.
    #[cfg(debug_assertions)]
    if let Some(n) = std::env::var("HASTECLIP_OPEN_MENU").ok().and_then(|s| s.parse::<u64>().ok()) {
        let key = egui::Id::new("ui_kit_dev_menu");
        let (pass, count, done) = resp.ctx.data(|d| d.get_temp::<(u64, u64, bool)>(key)).unwrap_or_default();
        let now = resp.ctx.cumulative_pass_nr();
        let count = if pass == now { count } else { 0 };
        let open = !done && count == n && resp.ctx.input(|i| i.time) > std::env::var("HASTECLIP_OPEN_MENU_AT").ok().and_then(|s| s.parse().ok()).unwrap_or(2.5);
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
        // Never narrower than what opened it (a dropdown hangs from its field).
        .show(|ui| menu_body(ui, menu_floor(width).max(resp.rect.width() - 12.0), content))
        .map(|r| r.inner)
}

/// A right-click menu on `resp`, at the pointer.
pub fn context_menu<R>(resp: &Response, width: f32, content: impl FnOnce(&mut egui::Ui) -> R) -> Option<R> {
    let frame = menu_frame_ctx(&resp.ctx);
    egui::Popup::context_menu(resp)
        .frame(frame)
        .style(menu_style)
        .show(|ui| menu_body(ui, menu_floor(width), content))
        .map(|r| r.inner)
}

/// A right-click menu, as [`context_menu`], that a click inside doesn't
/// close (only its items do): for one with a field to type into.
pub fn context_menu_typed<R>(resp: &Response, width: f32, content: impl FnOnce(&mut egui::Ui) -> R) -> Option<R> {
    let frame = menu_frame_ctx(&resp.ctx);
    egui::Popup::context_menu(resp)
        .frame(frame)
        .style(menu_style)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| menu_body(ui, menu_floor(width), content))
        .map(|r| r.inner)
}

/// A volume as typed: "-6", "−6", "+3.5", "4,5 dB", "-inf", "−∞". Unclamped;
/// silence is `f32::NEG_INFINITY`. None when it isn't one.
pub fn parse_db(s: &str) -> Option<f32> {
    let s = s.trim().replace('−', "-").replace(',', ".").to_lowercase();
    let s = s.strip_suffix("db").unwrap_or(&s).trim();
    if matches!(s, "-∞" | "-inf" | "-infinity") {
        return Some(f32::NEG_INFINITY);
    }
    s.parse::<f32>().ok().filter(|v| v.is_finite())
}

/// A volume for typing over: "+3.0", "−12.5", "0.0", and "−∞" at or below
/// `silent`.
pub fn db_edit_text(db: f32, silent: f32) -> String {
    if db <= silent + 0.05 {
        "−∞".to_owned()
    } else if db.abs() < 0.05 {
        "0.0".to_owned()
    } else {
        format!("{}{:.1}", if db > 0.0 { "+" } else { "−" }, db.abs())
    }
}

/// A row in a menu ([`context_menu_typed`]) to type a volume into: its name,
/// then a box with `shown` in it, chosen all through and ready to type over
/// as the menu opens. Enter sets it and closes the menu; Esc closes it.
/// Returns the dB typed, unclamped (see [`parse_db`]).
pub fn menu_db_field(ui: &mut egui::Ui, label: &str, shown: &str) -> Option<f32> {
    let p = theme::pal(ui);
    const BOX_W: f32 = 86.0;
    let g = ui.painter().layout_no_wrap(label.to_owned(), theme::font(13.5, W::Regular), p.text);
    let natural = 9.0 + g.size().x + 16.0 + BOX_W + 4.0;
    menu_saw(ui, natural);
    let w = natural.max(menu_min_width(ui));
    let (rect, _) = ui.allocate_exact_size(vec2(w, 32.0), Sense::hover());
    ui.painter().galley(Pos2::new(rect.left() + 9.0, rect.center().y - g.size().y / 2.0), g, p.text);
    let id = ui.id().with(("menu_db_field", label));
    let edit_id = id.with("edit");
    // What's typed, and the pass it was last shown in: a menu opened
    // again starts from `shown`.
    let key = id.with("typed");
    let now = ui.ctx().cumulative_pass_nr();
    let kept = ui.data(|d| d.get_temp::<(u64, String)>(key)).filter(|(seen, _)| seen + 1 >= now);
    let fresh = kept.is_none();
    let mut text = kept.map_or_else(|| shown.to_owned(), |(_, t)| t);
    let fbox = Rect::from_min_size(Pos2::new(rect.right() - 4.0 - BOX_W, rect.center().y - 12.0), vec2(BOX_W, 24.0));
    let focused = ui.memory(|m| m.has_focus(edit_id));
    ui.painter().rect_filled(fbox, RADIUS, p.control);
    let font = theme::font(13.0, W::Medium);
    let unit = ui.painter().layout_no_wrap("dB".to_owned(), font.clone(), p.text3);
    let inner = Rect::from_min_max(Pos2::new(fbox.left() + 8.0, fbox.top()), Pos2::new(fbox.right() - 12.0 - unit.size().x, fbox.bottom()));
    let mut fui = ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(egui::Layout::left_to_right(egui::Align::Center)));
    // A volume as it's said: "-7.5", "+3", "4,5 dB", and "−∞" or "-inf"
    // for silence (read by `parse_db`, #26).
    #[allow(clippy::disallowed_methods)]
    let out = egui::TextEdit::singleline(&mut text)
        .id(edit_id)
        .frame(egui::Frame::NONE)
        .font(font)
        .text_color(p.text)
        .desired_width(inner.width())
        .margin(Margin::ZERO)
        .show(&mut fui);
    ui.painter().galley(Pos2::new(fbox.right() - 8.0 - unit.size().x, fbox.center().y - unit.size().y / 2.0), unit, p.text3);
    if focused {
        ui.painter().rect_stroke(fbox, RADIUS, Stroke::new(1.5, p.text2), StrokeKind::Inside);
    }
    // Clicking anywhere in the box (the unit too) types.
    if ui.interact(fbox, id.with("box"), Sense::click()).on_hover_cursor(egui::CursorIcon::Text).clicked() {
        out.response.request_focus();
    }
    if fresh {
        out.response.request_focus();
        let mut state = out.state.clone();
        state.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(text.chars().count()))));
        state.store(ui.ctx(), edit_id);
    }
    let mut typed = None;
    if out.response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
        match parse_db(&text) {
            Some(db) => {
                typed = Some(db);
                ui.close();
            }
            // Not a volume: still there to fix.
            None => out.response.request_focus(),
        }
    }
    ui.data_mut(|d| d.insert_temp(key, (now, text)));
    typed
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
    /// Where what it says is changed, when that's elsewhere: a quiet button
    /// under its note going there ([`Row::go`]).
    pub go: Option<&'a str>,
}

impl<'a> Row<'a> {
    pub fn new(label: &'a str) -> Self {
        Self { label, note: None, warn: false, tip: None, locked: None, kid: false, go: None }
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
    /// A quiet button under its note, named `text`, to where what it says
    /// is changed (another page): [`take_row_go`] says it was clicked.
    pub fn go(mut self, text: Option<&'a str>) -> Self {
        self.go = text;
        self
    }

    /// Draw it; the control is laid out right to left from the row's right
    /// edge. Returns what the control returns, and whether the locked control
    /// was clicked (to say why it's locked).
    pub fn show<R>(self, ui: &mut egui::Ui, control: impl FnOnce(&mut egui::Ui) -> R) -> (R, bool) {
        let p = theme::pal(ui);
        let (pad_x, min_h) = if self.kid { (WELL_ROW_PAD, WELL_ROW_H) } else { (ROW_PAD, ROW_H) };
        let locked = self.locked.is_some();
        let width = ui.available_width();
        // How wide the control was last frame: beside the label when both
        // fit, else under it (narrow windows), never over it.
        let key = ui.id().with(("row_control_w", self.label));
        let known_w = ui.data(|d| d.get_temp::<f32>(key));
        let control_w = known_w.unwrap_or(0.0);
        // The label's height as last drawn, to centre it on the control (its
        // line is as tall as a control can be, not just its text).
        let label_key = ui.id().with(("row_label_h", self.label));
        let known_h = ui.data(|d| d.get_temp::<f32>(label_key));
        let label_h = known_h.unwrap_or(20.0);
        let mut drawn_label_h = 0.0;
        let inner_w = width - pad_x * 2.0;
        let stacked = control_w > 0.0 && control_w + 16.0 + 140.0 > inner_w;
        let label_w = if stacked { inner_w } else { (inner_w - control_w - 16.0).max(120.0) };
        let label = |ui: &mut egui::Ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.horizontal_wrapped(|ui| {
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
            }
            // Its words in line with the note's (the button's padding
            // to their left).
            if let Some(go) = self.go {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.add_space(-9.0);
                    if Btn::ghost(go).trail(icon::CARET_RIGHT).small().show(ui).clicked() {
                        ui.data_mut(|d| d.insert_temp(egui::Id::new(("row_go", self.label)), true));
                    }
                });
            }
        };
        let mut clicked_locked = false;
        let mut control_rect = Rect::NOTHING;
        let place_control = |ui: &mut egui::Ui| -> R {
            ui.spacing_mut().item_spacing.x = 8.0;
            if locked {
                let inner = ui.add_enabled_ui(false, |ui| {
                    ui.multiply_opacity(0.7);
                    control(ui)
                });
                control_rect = inner.response.rect;
                // Named after its row: every locked row's control column has the same id.
                let resp = ui.interact(inner.response.rect, ui.id().with(("locked", self.label)), Sense::click()).on_hover_cursor(egui::CursorIcon::NotAllowed);
                if let Some(why) = &self.locked {
                    let resp = resp.on_hover_text(why);
                    clicked_locked = resp.clicked();
                }
                inner.inner
            } else {
                let inner = ui.scope(control);
                control_rect = inner.response.rect;
                inner.inner
            }
        };
        let r = if stacked {
            // The label, then the control on its own line, right-aligned.
            ui.allocate_ui_with_layout(vec2(width, min_h), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.add_space(pad_x);
                    ui.allocate_ui_with_layout(vec2(label_w, 0.0), egui::Layout::top_down(egui::Align::Min), label);
                });
                ui.add_space(6.0);
                let r = ui.allocate_ui_with_layout(vec2(width, CONTROL_H), egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(pad_x);
                    place_control(ui)
                });
                ui.add_space(10.0);
                r.inner
            })
            .inner
        } else {
            ui.allocate_ui_with_layout(vec2(width, min_h), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.set_min_height(min_h);
                ui.add_space(pad_x);
                ui.allocate_ui_with_layout(vec2(label_w, min_h), egui::Layout::top_down(egui::Align::Min), |ui| {
                    if self.note.is_some() || self.go.is_some() {
                        ui.add_space(9.0);
                    } else {
                        ui.add_space(((min_h - label_h) / 2.0).max(0.0));
                    }
                    drawn_label_h = ui.scope(label).response.rect.height();
                    if self.note.is_some() || self.go.is_some() {
                        ui.add_space(8.0);
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(pad_x);
                    place_control(ui)
                })
                .inner
            })
            .inner
        };
        // Laid out by last frame's sizes. The first time, the frame is drawn
        // again with the real ones; after that a change (a control growing as
        // it animates) is just used next frame: discarding every frame of an
        // animation made egui warn, and a frame's lag doesn't show.
        if control_rect.is_positive() && (control_rect.width() - control_w).abs() > 0.5 {
            ui.data_mut(|d| d.insert_temp(key, control_rect.width()));
            if known_w.is_none() {
                ui.ctx().request_discard("a settings row's control was measured");
            }
        }
        if self.note.is_none() && drawn_label_h > 0.0 && (drawn_label_h - label_h).abs() > 0.5 {
            ui.data_mut(|d| d.insert_temp(label_key, drawn_label_h));
            if known_h.is_none() {
                ui.ctx().request_discard("a settings row's label was measured");
            }
        }
        (r, clicked_locked)
    }
}

/// Round `rect`'s corners by `radius` over something drawn square (a video
/// from the GPU, which egui can only cut to rectangles): the bit outside
/// each corner's curve painted over in `bg`, the colour behind it.
pub fn round_corners(painter: &egui::Painter, rect: Rect, radius: f32, bg: Color32) {
    const STEPS: usize = 8;
    let corners = [
        (rect.left_top(), Pos2::new(rect.left() + radius, rect.top() + radius), 180.0f32),
        (rect.right_top(), Pos2::new(rect.right() - radius, rect.top() + radius), 270.0),
        (rect.right_bottom(), Pos2::new(rect.right() - radius, rect.bottom() - radius), 0.0),
        (rect.left_bottom(), Pos2::new(rect.left() + radius, rect.bottom() - radius), 90.0),
    ];
    for (corner, centre, start) in corners {
        // The corner point, then the curve from one edge to the other.
        let mut pts = vec![corner];
        for k in 0..=STEPS {
            let a = (start + 90.0 * k as f32 / STEPS as f32).to_radians();
            pts.push(centre + vec2(a.cos(), a.sin()) * radius);
        }
        let mut mesh = egui::Mesh::default();
        for p in &pts {
            mesh.colored_vertex(*p, bg);
        }
        for i in 1..pts.len() as u32 - 1 {
            mesh.add_triangle(0, i, i + 1);
        }
        painter.add(egui::Shape::mesh(mesh));
    }
}

/// A group of rows on the raised surface, hairlines between them. Its
/// height eases when rows or notes come and go (see [`eased_frame`]); `id`
/// names it on its page.
pub fn list<R>(ui: &mut egui::Ui, id: impl std::hash::Hash + std::fmt::Debug, add: impl FnOnce(&mut List) -> R) -> R {
    let frame = egui::Frame::new().fill(theme::pal(ui).raise).corner_radius(CARD_RADIUS);
    eased_frame(ui, egui::Id::new(("list", id)), frame, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 0.0;
        let mut l = List { ui, rows: 0 };
        add(&mut l)
    })
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
            divider(self.ui, ROW_LINE_INSET);
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
    if t < 1.0 {
        moving(ui.ctx());
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
    let gap = WELL_GAP as i8;
    let inner = egui::Frame::new().outer_margin(Margin { left: gap, right: gap, top: 0, bottom: gap }).fill(p.well).corner_radius(RADIUS).inner_margin(Margin::symmetric(0, WELL_INSET as i8)).show(&mut child, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 0.0;
        let mut l = List { ui, rows: 0 };
        add(&mut l);
    });
    let real_h = inner.response.rect.height() + WELL_GAP;
    ui.data_mut(|d| d.insert_temp(id.with("h"), real_h));
    ui.allocate_space(vec2(avail, h.unwrap_or(real_h)));
}

// --- Dialogs ---

// --- Coming into focus ---

/// How long a dialog or toast takes to come into focus.
pub const FOCUS_SECS: f32 = 0.18;
/// How blurred it starts, in points.
const FOCUS_BLUR: f32 = 5.0;
/// The copies that blur what's in the box: an inner and an outer ring, at
/// these fractions of the blur.
const FOCUS_RINGS: [(f32, usize); 2] = [(0.45, 6), (1.0, 8)];

/// How far something that appeared at `born` (egui time) is into coming
/// into focus: 0 to 1, not eased (see [`focus_in`]).
pub fn focus_since(ctx: &egui::Context, born: f64) -> f32 {
    let x = ((ctx.input(|i| i.time) - born) as f32 / FOCUS_SECS).clamp(0.0, 1.0);
    if x < 1.0 {
        ctx.request_repaint();
    }
    x
}

/// A box (`frame`, no outer margin) with `add` in it, coming into focus as
/// `x` goes from 0 to 1 (see [`focus_since`]): blurred and faint at first,
/// then sharp. Dialogs and toasts arrive like this; menus and tooltips open
/// at once (they're opened all the time).
///
/// egui can't blur, so the box is drawn once, its edge softened (egui's
/// feathering) and fading in, so its colour is always its own. What's in it is
/// drawn in two small rings of faint copies with itself on top, together
/// as visible as a plain fade; the rings close in and fade out. Nothing
/// changes size (scaled text is soft until it lands, then visibly snaps), and
/// it's drawn the same way when it's sharp, so nothing changes at the end.
/// A floating area centred on a point (a toast, the selection bar) is placed
/// by how big it was the frame before. When that changed (another toast, a
/// button more), the frame is laid out again before it's shown, so it's
/// never off centre for a frame. `size`: how big it is now.
pub fn keep_centred(ctx: &egui::Context, id: egui::Id, size: Vec2) {
    let key = id.with("drawn_size");
    let was: Option<Vec2> = ctx.data(|d| d.get_temp(key));
    ctx.data_mut(|d| d.insert_temp(key, size));
    if was.is_some_and(|w| (w - size).length() > 0.5) {
        ctx.request_discard("a centred area changed size");
    }
}

pub fn focus_in<R>(ui: &mut egui::Ui, x: f32, frame: egui::Frame, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let ctx = ui.ctx().clone();
    let x = x.clamp(0.0, 1.0);
    // Seen as fast as a plain fade; the blur goes more gently, so it's seen.
    let shown = theme::ease_out(x);
    let blur = FOCUS_BLUR * (1.0 - x) * (1.0 - x);
    // The box goes under what's in it: its place first, filled in after.
    let under = ui.painter().add(egui::Shape::Noop);
    let layer = ui.layer_id();
    let start = ctx.graphics(|g| g.get(layer).map_or(0, |l| l.next_idx().0));
    // Laid out as the frame would be (its stroke is inside its margin).
    let (m, sw) = (frame.inner_margin, frame.stroke.width.round() as i8);
    let margin = Margin { left: m.left + sw, right: m.right + sw, top: m.top + sw, bottom: m.bottom + sw };
    let mut content = ui.new_child(egui::UiBuilder::new().max_rect(ui.available_rect_before_wrap() - egui::epaint::MarginF32::from(margin)));
    let r = add(&mut content);
    let rect = content.min_rect() + egui::epaint::MarginF32::from(margin);
    ui.allocate_rect(rect, Sense::hover());
    let mut boxed = egui::epaint::RectShape::new(rect, frame.corner_radius, theme::fade(frame.fill, shown), Stroke::new(frame.stroke.width, theme::fade(frame.stroke.color, shown)), StrokeKind::Inside);
    boxed.blur_width = blur;
    let mut shapes = vec![egui::Shape::Rect(boxed)];
    if frame.shadow != egui::Shadow::NONE {
        let mut shadow = frame.shadow.as_shape(rect, frame.corner_radius);
        shadow.fill = theme::fade(shadow.fill, shown);
        shadow.blur_width += blur;
        shapes.insert(0, egui::Shape::Rect(shadow));
    }
    ui.painter().set(under, egui::Shape::Vec(shapes));
    if x >= 1.0 {
        return r;
    }
    // How much of what's seen is the copies: all of it at first, none at the
    // end. Each copy's opacity and the one on top's are such that together
    // they show `shown`.
    let copies: usize = FOCUS_RINGS.iter().map(|(_, n)| n).sum();
    let soft = shown * (1.0 - x);
    let each = 1.0 - (1.0 - soft).powf(1.0 / copies as f32);
    let top = 1.0 - (1.0 - shown) / (1.0 - soft).max(1e-3);
    ctx.graphics_mut(|g| {
        let list = g.entry(layer);
        let end = list.next_idx().0;
        if end <= start {
            return;
        }
        // Taken out, then put back over the copies, so none of them covers it.
        let content: Vec<egui::epaint::ClippedShape> = list.all_entries().skip(start).cloned().collect();
        for i in start..end {
            list.mutate_shape(egui::layers::ShapeIdx(i), |cs| cs.shape = egui::Shape::Noop);
        }
        for (frac, n) in FOCUS_RINGS {
            for k in 0..n {
                // The rings turned against each other, so no copies line up.
                let a = std::f32::consts::TAU * (k as f32 + frac) / n as f32;
                let offset = vec2(a.cos(), a.sin()) * blur * frac;
                // (A picture the GPU draws is drawn once.)
                for cs in content.iter().filter(|cs| !matches!(cs.shape, egui::Shape::Callback(_))) {
                    let mut shape = cs.shape.clone();
                    shift(&mut shape, offset, each);
                    list.add(cs.clip_rect.translate(offset), shape);
                }
            }
        }
        for cs in content {
            let mut shape = cs.shape;
            shift(&mut shape, Vec2::ZERO, top);
            list.add(cs.clip_rect, shape);
        }
    });
    r
}

/// A shape moved by `offset` at `opacity` of its own. Text is moved by its
/// place and faded by its opacity (transforming it copies its whole layout,
/// every frame, for every copy).
fn shift(shape: &mut egui::Shape, offset: Vec2, opacity: f32) {
    match shape {
        egui::Shape::Text(t) => {
            t.pos += offset;
            t.opacity_factor *= opacity;
        }
        egui::Shape::Vec(shapes) => {
            for s in shapes {
                shift(s, offset, opacity);
            }
        }
        s => {
            if offset != Vec2::ZERO {
                s.translate(offset);
            }
            egui::epaint::shape_transform::adjust_colors(s, move |c| {
                if *c != Color32::PLACEHOLDER {
                    *c = c.gamma_multiply(opacity);
                }
            });
        }
    }
}

/// A dialog: a scrim over the window, the box coming into focus.
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
    let x = focus_since(ctx, born);
    let frame = egui::Frame::new()
        .fill(p.pop)
        .corner_radius(10)
        .stroke(Stroke::new(1.0, if p.dark { Color32::from_white_alpha(18) } else { Color32::from_black_alpha(20) }))
        .shadow(ctx.global_style().visuals.popup_shadow)
        .inner_margin(Margin { left: 18, right: 18, top: 18, bottom: 14 });
    // The scrim comes in by itself; the box (drawn on it, in the same layer)
    // comes into focus.
    let modal = egui::Modal::new(id).backdrop_color(theme::fade(p.scrim, theme::ease_out(x))).frame(egui::Frame::NONE).area(egui::Modal::default_area(id).fade_in(false));
    modal.show(ctx, |ui| {
        focus_in(ui, x, frame, |ui| {
            ui.set_width(width);
            add(ui)
        })
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
/// they're laid out right to left). In a row as tall as a button: a
/// dialog's box is laid out in the size it had last frame, and buttons
/// centred in all the height left kept a box that was taller once (a frame
/// whose text wrapped narrow) that tall for good, its buttons halfway down.
pub fn dialog_buttons(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(14.0);
    ui.allocate_ui_with_layout(vec2(ui.available_width(), CONTROL_H), egui::Layout::right_to_left(egui::Align::Center), |ui| {
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

/// A text field in the app's look: filled, outlined when focused. Only for
/// free text (a name, a search): a number is a [`ValueField`], a choice a
/// [`ChoiceField`] (see "Inputs guide" above).
#[allow(clippy::disallowed_methods)] // the free-text box itself
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

/// A [`text_field`] for a secret (a stream key): free text, shown as dots
/// unless `shown`. As wide as `width`.
#[allow(clippy::disallowed_methods)] // the free-text box itself
pub fn secret_field(ui: &mut egui::Ui, id: egui::Id, text: &mut String, hint: &str, shown: bool, width: f32) -> Response {
    let p = theme::pal(ui);
    let focused = ui.memory(|m| m.has_focus(id));
    ui.allocate_ui(vec2(width, CONTROL_H), |ui| {
        egui::Frame::new()
            .fill(p.control)
            .corner_radius(RADIUS)
            .stroke(Stroke::new(1.5, if focused { p.text2 } else { Color32::TRANSPARENT }))
            .inner_margin(Margin::symmetric(10, 7))
            .show(ui, |ui| {
                egui::TextEdit::singleline(text)
                    .id(id)
                    .password(!shown)
                    .frame(egui::Frame::NONE)
                    .hint_text(theme::text(hint, 13.0, W::Regular).color(p.text3))
                    .font(theme::font(13.0, W::Regular))
                    .text_color(p.text)
                    .desired_width(f32::INFINITY)
                    .margin(Margin::ZERO)
                    .show(ui)
                    .response
                    .response
            })
            .inner
    })
    .inner
}

// --- Inputs ---
//
// Which input a value gets is decided by what kind of value it is: see
// "Inputs" in this module's notes. [`ValueField`] is for one that's always
// a number; [`ChoiceField`] for one picked from a list (a number of your
// own beside it, where it can be one); [`number_box`] for each number of a
// pair (a size, a ratio); [`text_field`] only for free text.

/// A common value of a [`ValueField`]: what it's called in its menu, the
/// number, and a note on the right.
pub struct Preset {
    pub label: String,
    pub value: f64,
    pub note: String,
}

impl Preset {
    pub fn new(label: impl Into<String>, value: f64) -> Self {
        Self { label: label.into(), value, note: String::new() }
    }

    pub fn note(mut self, note: impl Into<String>) -> Self {
        self.note = note.into();
        self
    }
}

/// A value that's always a number ("60", "15", "2:30"), typed in place,
/// with its unit after it and the common values under its ∨. It only ever
/// shows a number, and only digits go in (and a point, where it takes
/// decimals): a value that can be a word ("Original", "Auto", "Off") is a
/// [`ChoiceField`] instead. Clicking it chooses all of it, so typing
/// replaces it; Enter, Tab or clicking away applies; Esc puts it back; ↑ ↓
/// step through the common values. A number beyond its range is kept to it.
pub struct ValueField<'a> {
    pub id: egui::Id,
    /// The number now.
    pub value: f64,
    /// Places after the point (0: whole numbers).
    pub decimals: usize,
    /// The least and most it can be.
    pub range: std::ops::RangeInclusive<f64>,
    pub unit: &'a str,
    pub width: f32,
    /// What to type, on hover.
    pub hint: &'a str,
    /// The common values, in the order ↑ ↓ step through them.
    pub presets: Vec<Preset>,
}

impl ValueField<'_> {
    /// The new number, once it's typed or picked.
    pub fn show(self, ui: &mut egui::Ui) -> Option<f64> {
        self.show_inner(ui, false)
    }

    /// A length, in seconds: the box is a clock ([`crate::time_field`]),
    /// ↑ ↓ stepping the part chosen.
    pub fn show_time(self, ui: &mut egui::Ui) -> Option<f64> {
        self.show_inner(ui, true)
    }

    /// Which of the common values it is, if one.
    fn current(&self) -> Option<usize> {
        let near = 0.5 * 10f64.powi(-(self.decimals as i32));
        self.presets.iter().position(|p| (p.value - self.value).abs() < near)
    }

    /// `v` as it can be: to its places, within its range.
    fn kept(&self, v: f64) -> f64 {
        let scale = 10f64.powi(self.decimals as i32);
        ((v * scale).round() / scale).clamp(*self.range.start(), *self.range.end())
    }

    fn show_inner(self, ui: &mut egui::Ui, time: bool) -> Option<f64> {
        let p = theme::pal(ui);
        let mut out = None;
        let current = self.current();
        let shown = number_text(self.value, self.decimals);
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
        if time {
            let font = theme::font(13.0, W::Medium);
            let tf = crate::time_field::TimeField { id: edit_id, secs: self.value, min: *self.range.start(), max: *self.range.end(), decimals: 0, font, align: egui::Align::Min, hint: self.hint };
            match tf.show(ui, field) {
                crate::time_field::Timed::Stepped(v) | crate::time_field::Timed::Typed(v) => out = Some(self.kept(v)),
                // Not a length: it shows what it was.
                crate::time_field::Timed::None | crate::time_field::Timed::Unreadable => {}
            }
        } else {
            let (point, minus) = (self.decimals > 0, *self.range.start() < 0.0);
            keep_to_number(ui, edit_id, point, minus);
            let mut text = editing.clone().unwrap_or_else(|| shown.clone());
            let font = theme::font(13.0, W::Medium);
            let tw = ui.painter().layout_no_wrap(if text.is_empty() { " ".into() } else { text.clone() }, font.clone(), p.text).size().x;
            let unit_w = if self.unit.is_empty() { 0.0 } else { ui.painter().layout_no_wrap(self.unit.to_owned(), font.clone(), p.text3).size().x + 4.0 };
            let input_w = (tw + 1.0).min(field.width() - unit_w).max(14.0);
            // Clicking anywhere in the box (not just on the digits) types:
            // under the digits' own box, so clicks on them place the caret
            // and choose, as in any text box.
            let box_resp = ui.interact(field, self.id.with("box"), Sense::click());
            let mut fui = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_min_size(field.min, vec2(input_w, field.height()))).layout(egui::Layout::left_to_right(egui::Align::Center)));
            // The box itself: what goes in is kept to a number, above and below.
            #[allow(clippy::disallowed_methods)]
            let edited = egui::TextEdit::singleline(&mut text)
                .id(edit_id)
                .frame(egui::Frame::NONE)
                .font(font.clone())
                .text_color(p.text)
                .desired_width(input_w)
                .margin(Margin::ZERO)
                .show(&mut fui);
            text = number_typing(&text, point, minus);
            let resp = edited.response.response.clone().on_hover_text(self.hint).on_hover_cursor(egui::CursorIcon::Text);
            if !self.unit.is_empty() {
                ui.painter().text(Pos2::new(field.left() + input_w + 2.0, rect.center().y), Align2::LEFT_CENTER, self.unit, font, p.text3);
            }
            if box_resp.clicked() {
                resp.request_focus();
            }
            if resp.gained_focus() {
                editing = Some(shown.clone());
                // All of it chosen: what's typed replaces it.
                let mut state = edited.state.clone();
                state.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(shown.chars().count()))));
                state.store(ui.ctx(), edit_id);
            }
            if resp.has_focus() {
                editing = Some(text.clone());
                let (up, down, esc) = ui.input_mut(|i| (i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp), i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown), i.key_pressed(egui::Key::Escape)));
                if esc {
                    editing = None;
                    resp.surrender_focus();
                } else if (up || down) && !self.presets.is_empty() {
                    // Up goes to the one before in the list: they're listed
                    // biggest first or smallest first, so step by their order.
                    let n = self.presets.len();
                    let at = current.unwrap_or(0);
                    let next = if up { at.saturating_sub(1) } else { (at + 1).min(n - 1) };
                    out = Some(self.presets[next].value);
                    editing = Some(number_text(self.presets[next].value, self.decimals));
                }
            }
            // Enter, Tab or clicking away: what's typed counts. Esc (the box
            // lets go of the keys by itself): it's put back.
            if resp.lost_focus() {
                let esc = ui.input(|i| i.key_pressed(egui::Key::Escape));
                if let Some(v) = editing.take().filter(|_| !esc).and_then(|t| t.parse::<f64>().ok()) {
                    out = Some(self.kept(v));
                }
            }
            ui.data_mut(|d| match &editing {
                Some(t) => {
                    d.insert_temp(editing_key, t.clone());
                }
                None => d.remove::<String>(editing_key),
            });
        }
        // The ∨: the common values, and "Type your own…".
        let ddr = ui.interact(dd, self.id.with("dd"), Sense::click());
        let dhv = theme::toggle(ui.ctx(), ddr.id.with("hv"), ddr.hovered() || menu_open, theme::FAST);
        if dhv > 0.0 {
            ui.painter().rect_filled(dd, CornerRadius { nw: 0, sw: 0, ne: RADIUS, se: RADIUS }, theme::fade(p.control_h, dhv));
        }
        let turn = theme::toggle(ui.ctx(), ddr.id.with("turn"), menu_open, theme::NORMAL);
        chevron(ui.painter(), dd.center(), 3.5, turn, theme::mix(p.text3, p.text, dhv));
        let ddr = ddr.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Common values");
        let mut focus = false;
        let (lo, hi) = (*self.range.start(), *self.range.end());
        let range = if time {
            format!("{} – {}", length_short(lo), length_short(hi))
        } else {
            format!("{}–{} {}", number_text(lo, self.decimals), number_text(hi, self.decimals), self.unit).trim_end().to_owned()
        };
        // As wide as the field (wider when a value's note needs it).
        menu_popup(&ddr, false, self.width, |ui| {
            for (i, preset) in self.presets.iter().enumerate() {
                let mut item = Item::new(preset.label.clone()).check(current == Some(i));
                if !preset.note.is_empty() {
                    item = item.value(preset.note.clone());
                }
                if item.show(ui).clicked() {
                    out = Some(preset.value);
                }
            }
            if current.is_none() && !time {
                menu_sep(ui);
                Item::new(format!("{shown}{}{}", if self.unit.is_empty() { "" } else { " " }, self.unit)).check(true).value("yours").show(ui);
            }
            menu_sep(ui);
            if Item::new("Type your own…").icon(icon::TEXT_T).value(range).show(ui).clicked() {
                focus = true;
            }
        });
        if focus {
            ui.memory_mut(|m| m.request_focus(edit_id));
        }
        out.filter(|v| (v - self.value).abs() > 1e-9)
    }
}

/// A number as a field shows it: to `decimals` places, without the zeros
/// that end them ("8", "8.5").
pub fn number_text(v: f64, decimals: usize) -> String {
    let s = format!("{v:.decimals$}");
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_owned() } else { s }
}

/// While the box `id` has the keyboard, only what a number has goes into
/// it: digits, a point where it takes decimals (`point`; a comma is one),
/// and a minus where it can go below zero (`minus`). Letters typed or
/// pasted are left out as they come. [`ValueField`] does this itself; a box
/// of a page's own that takes a number calls it before its `TextEdit`.
pub fn keep_to_number(ui: &egui::Ui, id: egui::Id, point: bool, minus: bool) {
    if !ui.memory(|m| m.has_focus(id)) {
        return;
    }
    ui.input_mut(|i| {
        i.events.retain_mut(|e| match e {
            egui::Event::Text(t) | egui::Event::Paste(t) => {
                *t = t.chars().map(|c| if c == ',' { '.' } else { c }).filter(|&c| c.is_ascii_digit() || (point && c == '.') || (minus && c == '-')).collect();
                // Nothing left: no typing at all (an empty one would
                // still replace what's chosen in the box).
                !t.is_empty()
            }
            _ => true,
        });
    });
}

/// What's typed in a number's box, kept to what a number can be: digits,
/// one point (`point`), a minus first (`minus`).
fn number_typing(t: &str, point: bool, minus: bool) -> String {
    let mut seen_point = false;
    t.chars()
        .enumerate()
        .filter(|&(i, c)| c.is_ascii_digit() || (point && c == '.' && !std::mem::replace(&mut seen_point, true)) || (minus && c == '-' && i == 0))
        .map(|(_, c)| c)
        .collect()
}

/// A length in short words, for a range: "5 s", "10 min", "1 h".
fn length_short(secs: f64) -> String {
    let s = secs.round() as u64;
    if s < 60 {
        format!("{s} s")
    } else if s < 3600 {
        format!("{} min", s / 60)
    } else {
        format!("{} h", s / 3600)
    }
}

/// One of a [`ChoiceField`]'s choices.
pub struct Choice {
    /// Its name in the menu.
    pub label: String,
    /// What the field shows while it's chosen, when that's more than its
    /// name ("Original (1440p)").
    pub shown: Option<String>,
    /// On its right in the menu.
    pub note: String,
}

impl Choice {
    pub fn new(label: impl Into<String>) -> Self {
        Self { label: label.into(), shown: None, note: String::new() }
    }

    pub fn shown(mut self, shown: impl Into<String>) -> Self {
        self.shown = Some(shown.into());
        self
    }

    pub fn note(mut self, note: impl Into<String>) -> Self {
        self.note = note.into();
        self
    }
}

/// A [`ChoiceField`]'s number of your own: Custom… at the end of its menu,
/// then a box for it beside the menu.
pub struct Custom<'a> {
    /// After the box: "p", "fps", "Mbps".
    pub unit: &'a str,
    /// The least and most it can be: a number typed beyond is kept to it.
    pub range: std::ops::RangeInclusive<u32>,
    /// On Custom…'s right in the menu: "any height".
    pub note: &'a str,
}

/// What a [`ChoiceField`] was set to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Chosen {
    /// One of its choices, picked from the menu.
    Choice(usize),
    /// A number of your own, typed in its box.
    Number(u32),
}

/// The width of a [`ChoiceField`]'s box for a number of your own, and of its
/// menu while the box is beside it (saying Custom), as Settings' custom
/// size has them.
const CHOICE_BOX_W: f32 = 58.0;
const CHOICE_CUSTOM_W: f32 = 110.0;

/// A value picked from a menu of choices, and, with [`Custom`], a number of
/// your own: Custom… at the end of the menu puts a box beside it (on its
/// left: the menu stays where it was), its unit after it and the number
/// chosen, ready to type over, as Settings' Resolution does with a custom
/// size. For a value that's a word or a number ("Original", "Auto", "Off",
/// "1080p", or 900): the words are only ever picked, and the box only takes
/// digits. Picking a choice puts the menu back on its own.
pub struct ChoiceField<'a> {
    pub id: &'a str,
    pub choices: Vec<Choice>,
    pub custom: Option<Custom<'a>>,
    /// The menu's width (narrower, saying Custom, while the box is beside it).
    pub width: f32,
}

impl ChoiceField<'_> {
    /// Shown as `now` (the choice it is, if it's one) and `number` (what it
    /// is as a number: what the box shows, and starts from). What it was
    /// set to, when that changes.
    pub fn show(self, ui: &mut egui::Ui, now: Option<usize>, number: u32) -> Option<Chosen> {
        let p = theme::pal(ui);
        let typed_key = egui::Id::new((self.id, "custom"));
        let box_id = egui::Id::new((self.id, "box"));
        let type_in = egui::Id::new((self.id, "type_in"));
        // Custom… just picked: typing goes into the box.
        if ui.data(|d| d.get_temp::<bool>(type_in)).is_some() && !ui.input(|i| i.pointer.any_click()) {
            ui.data_mut(|d| d.remove::<bool>(type_in));
            ui.memory_mut(|m| m.request_focus(box_id));
        }
        // The number given last in the box: while it's still the value, the
        // box stays, even when that's one of the choices (a value changed
        // elsewhere puts the menu back on its own).
        let typed: Option<u32> = ui.data(|d| d.get_temp(typed_key));
        let custom = self.custom.as_ref().filter(|_| now.is_none() || typed == Some(number));
        let unit = custom.map(|c| ui.painter().layout_no_wrap(c.unit.to_owned(), theme::font(12.5, W::Regular), p.text3));
        let menu_w = if custom.is_some() { CHOICE_CUSTOM_W } else { self.width };
        let width = menu_w + unit.as_ref().map_or(0.0, |g| CHOICE_BOX_W + g.size().x + 6.0 * 2.0);
        let label = match (custom, now.and_then(|i| self.choices.get(i))) {
            (Some(_), _) => "Custom".to_owned(),
            (None, Some(c)) => c.shown.clone().unwrap_or_else(|| c.label.clone()),
            (None, None) => String::new(),
        };
        let mut out = None;
        let mut to_custom = false;
        ui.allocate_ui_with_layout(vec2(width, CONTROL_H), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            if let (Some(c), Some(unit)) = (custom, unit) {
                if let Some(v) = number_box(ui, box_id, number, c.range.clone(), CHOICE_BOX_W) {
                    out = Some(Chosen::Number(v));
                }
                let (r, _) = ui.allocate_exact_size(unit.size(), Sense::hover());
                ui.painter().galley(r.min, unit, p.text3);
            }
            let r = select(ui, &label, menu_w);
            menu_popup(&r, false, self.width.max(200.0), |ui| {
                for (i, c) in self.choices.iter().enumerate() {
                    let mut item = Item::new(c.label.clone()).check(custom.is_none() && now == Some(i));
                    if !c.note.is_empty() {
                        item = item.value(c.note.clone());
                    }
                    if item.show(ui).clicked() {
                        out = Some(Chosen::Choice(i));
                    }
                }
                if let Some(c) = &self.custom {
                    menu_sep(ui);
                    if Item::new("Custom…").check(custom.is_some()).value(c.note).show(ui).clicked() {
                        to_custom = true;
                    }
                }
            });
        });
        ui.data_mut(|d| match out {
            Some(Chosen::Choice(_)) => d.remove::<u32>(typed_key),
            Some(Chosen::Number(v)) => {
                d.insert_temp(typed_key, v);
            }
            // The box from the next frame, its number ready to type over.
            None if to_custom => {
                d.insert_temp(typed_key, number);
                d.insert_temp(type_in, true);
            }
            None => {}
        });
        out
    }
}

/// A size's width and height, each in its own box (Tab goes from one to the
/// other), with a link between them that keeps its shape: changing one
/// changes the other. Sides are even (video needs them so) and kept within
/// their ranges; one typed between two goes the way it was changed. The new
/// size, when it changes. The link is kept under `id`, for as long as the
/// app runs ([`size_field_keeps`]); [`type_in_size`] puts the typing in its
/// width.
pub fn size_field(ui: &mut egui::Ui, id: &str, (w, h): (u32, u32), ws: std::ops::RangeInclusive<u32>, hs: std::ops::RangeInclusive<u32>) -> Option<(u32, u32)> {
    const BOX: f32 = 58.0;
    let p = theme::pal(ui);
    let type_in = egui::Id::new((id, "type_in"));
    if ui.data(|d| d.get_temp::<bool>(type_in)).is_some() && !ui.input(|i| i.pointer.any_click()) {
        ui.data_mut(|d| d.remove::<bool>(type_in));
        ui.memory_mut(|m| m.request_focus(egui::Id::new((id, "w"))));
    }
    let keep_id = egui::Id::new((id, "keep"));
    let mut keep: Option<f64> = ui.data(|d| d.get_temp(keep_id));
    let mut out = None;
    ui.allocate_ui_with_layout(vec2(BOX * 2.0 + 28.0 + 16.0 + 6.0 * 3.0, CONTROL_H), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        if let Some(v) = number_box(ui, egui::Id::new((id, "w")), w, ws.clone(), BOX) {
            let nw = typed_side(v, w, &ws);
            out = Some((nw, keep.map_or(h, |shape| following_side(nw as f64 / shape, &hs))));
        }
        // The link goes between them, but is made after the height: Tab
        // goes from the width to the height.
        let (slot, _) = ui.allocate_exact_size(vec2(28.0, CONTROL_H), Sense::hover());
        if let Some(v) = number_box(ui, egui::Id::new((id, "h")), h, hs.clone(), BOX) {
            let nh = typed_side(v, h, &hs);
            out = Some((keep.map_or(w, |shape| following_side(nh as f64 * shape, &ws)), nh));
        }
        ui.label(theme::text("px", 12.5, W::Regular).color(p.text3));
        let (glyph, tip) = if keep.is_some() {
            (icon::LINK_SIMPLE_HORIZONTAL, "The shape's kept: changing one changes the other. Click to set them on their own.")
        } else {
            (icon::LINK_SIMPLE_HORIZONTAL_BREAK, "Keep the shape: changing one changes the other")
        };
        let kind = if keep.is_some() { Kind::Normal } else { Kind::Ghost };
        let link = ui.scope_builder(egui::UiBuilder::new().max_rect(slot).layout(egui::Layout::centered_and_justified(egui::Direction::LeftToRight)), |ui| Btn::icon(kind, glyph).small().tip(tip).show(ui));
        if link.inner.clicked() {
            keep = if keep.is_some() { None } else { Some(w as f64 / h.max(1) as f64) };
        }
    });
    ui.data_mut(|d| {
        match keep {
            Some(k) => {
                d.insert_temp(keep_id, k);
            }
            None => {
                d.remove::<f64>(keep_id);
            }
        };
    });
    out.filter(|s| *s != (w, h))
}

/// A side typed or stepped (from `was`), made even: up when it went up, so
/// ↑ from 1080 reaches 1082 rather than staying. Within `r`.
fn typed_side(v: u32, was: u32, r: &std::ops::RangeInclusive<u32>) -> u32 {
    (if v > was { (v + 1) & !1 } else { v & !1 }).clamp(*r.start(), *r.end() & !1)
}

/// A side following the other, its shape kept: the nearest even, within `r`.
fn following_side(v: f64, r: &std::ops::RangeInclusive<u32>) -> u32 {
    (((v / 2.0).round() as u32) * 2).clamp(*r.start(), *r.end() & !1)
}

/// The shape a [`size_field`] keeps (width over height), if its link is on.
pub fn size_field_keeps(ctx: &egui::Context, id: &str) -> Option<f64> {
    ctx.data(|d| d.get_temp(egui::Id::new((id, "keep"))))
}

/// Have a [`size_field`] keep this shape (width over height), its link on,
/// or none: an aspect ratio picked beside it.
pub fn set_size_field_keeps(ctx: &egui::Context, id: &str, keep: Option<f64>) {
    let key = egui::Id::new((id, "keep"));
    ctx.data_mut(|d| match keep {
        Some(k) => {
            d.insert_temp(key, k);
        }
        None => d.remove::<f64>(key),
    });
}

/// The next frame, typing goes into the [`size_field`] `id`'s width (it's
/// just been shown, from a menu).
pub fn type_in_size(ctx: &egui::Context, id: &str) {
    ctx.data_mut(|d| d.insert_temp(egui::Id::new((id, "type_in")), true));
}

/// The common aspect ratios, width and height, and what each is for: every
/// aspect ratio chosen in the app is one of these or typed ([`ratio_field`]).
pub const RATIOS: [(u32, u32, &str); 7] = [
    (16, 9, "widescreen"),
    (21, 9, "ultrawide"),
    (32, 9, "super ultrawide"),
    (16, 10, ""),
    (4, 3, "older games"),
    (1, 1, "square"),
    (9, 16, "upright, for phones"),
];

/// Which of [`RATIOS`] `w`:`h` is (to half a percent), if one.
pub fn common_ratio(w: u32, h: u32) -> Option<usize> {
    let r = w as f64 / h.max(1) as f64;
    RATIOS.iter().position(|&(rw, rh, _)| (r / (rw as f64 / rh as f64) - 1.0).abs() < 0.005)
}

/// A shape (width over height) as a ratio in small numbers, each up to 99:
/// the nearest, in the smallest numbers that are (1.7778 is 16:9).
pub fn ratio_of(shape: f64) -> [u32; 2] {
    let shape = if shape.is_finite() && shape > 0.0 { shape } else { 1.0 };
    let mut best = ([1, 1], f64::MAX);
    for h in 1..=99u32 {
        let w = (h as f64 * shape).round().clamp(1.0, 99.0) as u32;
        let off = (w as f64 / h as f64 / shape - 1.0).abs();
        if off < best.1 - 1e-9 {
            best = ([w, h], off);
        }
    }
    best.0
}

/// An aspect ratio: a menu of `first` (what none is called: "Same as
/// screen", "Any"; its value beside it, if any), the common ones
/// ([`RATIOS`]), and Custom…, which shows the ratio's width and height in
/// two boxes on the menu's left (as Resolution's Custom size… shows its
/// size), to type any (up to 99 each). Custom starts
/// from `now`, or `custom_from` while it's none. The one chosen or typed,
/// when it changes (`Some(None)`: `first`). Settings' Aspect ratio and every
/// crop's are this.
pub fn ratio_field(ui: &mut egui::Ui, id: &str, (first, first_value): (&str, &str), now: Option<[u32; 2]>, custom_from: [u32; 2]) -> Option<Option<[u32; 2]>> {
    let p = theme::pal(ui);
    let custom_id = egui::Id::new((id, "custom"));
    let w_id = egui::Id::new((id, "w"));
    // Custom just picked: typing goes into its width.
    let type_in = egui::Id::new((id, "type_in"));
    if ui.data(|d| d.get_temp::<bool>(type_in)).is_some() && !ui.input(|i| i.pointer.any_click()) {
        ui.data_mut(|d| d.remove::<bool>(type_in));
        ui.memory_mut(|m| m.request_focus(w_id));
    }
    let custom = ratio_custom(ui.ctx(), id, now);
    let label = match now {
        _ if custom => "Custom".to_owned(),
        None => first.to_owned(),
        Some([w, h]) => format!("{w}:{h}"),
    };
    let (select_w, width) = (if custom { 110.0 } else { 150.0 }, ratio_field_width(ui.ctx(), id, now));
    let mut out = None;
    let mut from_menu = false;
    let mut to_custom = false;
    // The boxes on the menu's left, as a custom size's are beside
    // Resolution's (the menu stays where it was), in reading order for Tab.
    ui.allocate_ui_with_layout(vec2(width, CONTROL_H), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        if custom {
            let [w, h] = now.unwrap_or(custom_from);
            if let Some(v) = number_box(ui, w_id, w, 1..=99, RATIO_BOX) {
                out = Some(Some([v, h]));
            }
            // In a slot of its own width, so the menu ends where it does without the boxes.
            let (slot, _) = ui.allocate_exact_size(vec2(8.0, CONTROL_H), Sense::hover());
            ui.painter().text(slot.center(), Align2::CENTER_CENTER, ":", theme::font(13.0, W::Regular), p.text3);
            if let Some(v) = number_box(ui, egui::Id::new((id, "h")), h, 1..=99, RATIO_BOX) {
                out = Some(Some([w, v]));
            }
        }
        let r = select(ui, &label, select_w);
        menu_popup(&r, false, 250.0, |ui| {
            let mut item = Item::new(first).check(now.is_none() && !custom);
            if !first_value.is_empty() {
                item = item.value(first_value);
            }
            if item.show(ui).clicked() {
                (out, from_menu) = (Some(None), true);
            }
            for &(w, h, note) in &RATIOS {
                let on = !custom && now.is_some_and(|[nw, nh]| common_ratio(nw, nh) == common_ratio(w, h));
                if Item::new(format!("{w}:{h}")).check(on).value(note).show(ui).clicked() {
                    (out, from_menu) = (Some(Some([w, h])), true);
                }
            }
            menu_sep(ui);
            if Item::new("Custom…").check(custom).value("any ratio").show(ui).clicked() {
                to_custom = true;
            }
        });
    });
    if to_custom {
        // Its boxes from the next frame, the width ready to type over.
        ui.data_mut(|d| {
            d.insert_temp(custom_id, true);
            d.insert_temp(type_in, true);
        });
        if now.is_none() {
            out = Some(Some(custom_from));
        }
    } else if from_menu {
        ui.data_mut(|d| d.remove::<bool>(custom_id));
    }
    out
}

/// A [`ratio_field`]'s boxes' width.
const RATIO_BOX: f32 = 46.0;

/// Whether a [`ratio_field`] shows its boxes: Custom was picked, or its
/// ratio isn't a common one.
fn ratio_custom(ctx: &egui::Context, id: &str, now: Option<[u32; 2]>) -> bool {
    ctx.data(|d| d.get_temp::<bool>(egui::Id::new((id, "custom")))).unwrap_or(false) || now.is_some_and(|[w, h]| common_ratio(w, h).is_none())
}

/// How wide a [`ratio_field`] is drawn, showing `now`: its menu, and its
/// boxes when it's custom.
pub fn ratio_field_width(ctx: &egui::Context, id: &str, now: Option<[u32; 2]>) -> f32 {
    if ratio_custom(ctx, id, now) { 110.0 + RATIO_BOX * 2.0 + 8.0 + 6.0 * 3.0 } else { 150.0 }
}

/// A whole number in a box of its own, typed or stepped: clicking it
/// chooses all of it, so the digits typed replace it; ↑ ↓ (and the wheel
/// over it) add or take one, with Shift ten. Enter, Tab or clicking away
/// applies what's typed; Esc puts it back. Kept within `range`. A text box
/// as any other besides (the caret, choosing with the pointer, copy, cut
/// and paste, Home and End), that only takes what a number has. The new
/// number, when it changes.
pub fn number_box(ui: &mut egui::Ui, id: egui::Id, value: u32, range: std::ops::RangeInclusive<u32>, width: f32) -> Option<u32> {
    signed_box(ui, id, i64::from(value), i64::from(*range.start())..=i64::from(*range.end()), width).map(|v| v as u32)
}

/// A [`number_box`] that can go below zero (a minus typed first), where
/// `range` does.
pub fn signed_box(ui: &mut egui::Ui, id: egui::Id, value: i64, range: std::ops::RangeInclusive<i64>, width: f32) -> Option<i64> {
    use egui::Key;
    let p = theme::pal(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(width, CONTROL_H), Sense::hover());
    let key = id.with("typed");
    let mut typed: Option<String> = ui.data(|d| d.get_temp(key));
    let kept = |v: i64| v.clamp(*range.start(), *range.end());
    let minus = *range.start() < 0;
    // As many digits as the longest number it can be, and its minus.
    let longest = range.start().unsigned_abs().max(range.end().unsigned_abs()).to_string().len() + usize::from(minus);
    let mut out = None;
    let focused = ui.memory(|m| m.has_focus(id));
    // While it's typed in: ↑ ↓ add or take one (Shift: ten), and the wheel
    // over it, before the box sees them; the number then all chosen, so
    // what's typed next replaces it.
    if focused {
        // Read off as they come (egui's own matching would take Shift+↑
        // for ↑ too).
        let mut by = ui.input_mut(|i| {
            let mut by = 0i64;
            i.events.retain(|e| match e {
                egui::Event::Key { key: k @ (Key::ArrowUp | Key::ArrowDown), pressed: true, modifiers, .. } if !modifiers.command && !modifiers.alt => {
                    let n = if modifiers.shift { 10 } else { 1 };
                    by += if *k == Key::ArrowUp { n } else { -n };
                    false
                }
                _ => true,
            });
            by
        });
        if ui.rect_contains_pointer(rect) {
            let dy = ui.input(|i| i.smooth_scroll_delta.y);
            if dy != 0.0 {
                ui.input_mut(|i| i.smooth_scroll_delta = Vec2::ZERO);
                let wheel = ui.data(|d| d.get_temp::<f32>(id.with("wheel"))).unwrap_or(0.0) + dy;
                let steps = (wheel / 40.0).trunc();
                by += steps as i64;
                ui.data_mut(|d| d.insert_temp(id.with("wheel"), wheel - steps * 40.0));
            }
        }
        if by != 0 {
            let now = typed.as_deref().and_then(|t| t.parse::<i64>().ok()).unwrap_or(value);
            let v = kept(now + by);
            out = Some(v);
            typed = Some(v.to_string());
            choose_all(ui.ctx(), id, &v.to_string());
        }
    }
    keep_to_number(ui, id, false, minus);
    let hv = theme::toggle(ui.ctx(), id.with("hv"), ui.rect_contains_pointer(rect) && !focused, theme::FAST);
    let fc = theme::toggle(ui.ctx(), id.with("fc"), focused, theme::FAST);
    ui.painter().rect_filled(rect, RADIUS, theme::mix(p.control, p.control_h, hv));
    if fc > 0.0 {
        ui.painter().rect_stroke(rect, RADIUS, Stroke::new(1.5, theme::fade(p.text2, fc)), StrokeKind::Inside);
    }
    let mut text = typed.clone().unwrap_or_else(|| value.to_string());
    let inner = rect.shrink2(vec2(6.0, 0.0));
    let mut tui = ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(egui::Layout::left_to_right(egui::Align::Center)));
    // A number's box: what goes in is kept to a number (`keep_to_number`).
    #[allow(clippy::disallowed_methods)]
    let edited = egui::TextEdit::singleline(&mut text)
        .id(id)
        .frame(egui::Frame::NONE)
        .font(theme::font(13.0, W::Medium))
        .text_color(p.text)
        .horizontal_align(egui::Align::Center)
        .desired_width(inner.width())
        .char_limit(longest)
        .margin(Margin::ZERO)
        .show(&mut tui);
    let text = number_typing(&text, false, minus);
    let resp = edited.response.response;
    if resp.gained_focus() {
        // All of it chosen: what's typed replaces it.
        typed = Some(value.to_string());
        choose_all(ui.ctx(), id, &value.to_string());
    } else if resp.has_focus() {
        typed = Some(text);
    }
    // Enter, Tab or clicking away: what's typed counts. Esc: put back.
    if resp.lost_focus() {
        let esc = ui.input(|i| i.key_pressed(Key::Escape));
        if let Some(v) = typed.take().filter(|_| !esc).and_then(|t| t.parse::<i64>().ok()) {
            out = Some(kept(v));
        }
    }
    ui.data_mut(|d| match &typed {
        Some(t) => {
            d.insert_temp(key, t.clone());
        }
        None => d.remove::<String>(key),
    });
    out.filter(|v| *v != value)
}

/// All of text box `id`'s `text` chosen, so what's typed next replaces it.
fn choose_all(ctx: &egui::Context, id: egui::Id, text: &str) {
    let mut state = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
    state.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(text.chars().count()))));
    state.store(ctx, id);
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
    // Buttons and other things that are only clicked; not what's also
    // dragged (the window's drag areas, backgrounds, the timeline), where a
    // hand looked as if any empty space were a button.
    let clickable = hovered.into_iter().filter_map(|id| ctx.read_response(id)).any(|r| r.enabled() && r.sense.senses_click() && !r.sense.senses_drag() && r.rect.area() < screen / 4.0);
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

    /// A dialog's box is as tall as what's in it, even after a frame in
    /// which it was taller (it's laid out in the size it had last frame):
    /// its buttons used to keep it that tall, halfway down it.
    #[test]
    fn a_dialog_comes_back_to_its_height() {
        let ctx = egui::Context::default();
        apply(&ctx);
        let height = |tall: bool, frame: u32| {
            let raw = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1040.0, 700.0))), max_texture_side: Some(8192), time: Some(f64::from(frame)), ..Default::default() };
            let mut out = ctx.run_ui(raw, |ui| {
                let ctx = ui.ctx().clone();
                let _ = dialog(&ctx, "grows_once", 440.0, |ui| {
                    dialog_title(ui, "A dialog");
                    dialog_text(ui, "Its words, wrapped to its width.");
                    if tall {
                        ui.add_space(400.0);
                    }
                    dialog_buttons(ui, |ui| {
                        let _ = ui.add(button("Yes", true));
                        let _ = ui.add(button("Cancel", false).ghost());
                    });
                });
            });
            out.textures_delta.clear();
            ctx.memory(|m| m.area_rect(egui::Id::new("grows_once"))).map_or(0.0, |r| r.height())
        };
        let fits = height(false, 0);
        assert!(fits > 100.0 && fits < 200.0, "{fits}");
        assert!(height(true, 1) > 500.0);
        height(false, 2);
        assert_eq!(height(false, 3), fits, "back to what's in it");
    }

    /// A volume is read however it's typed: either minus sign, a comma for
    /// the point, with or without "dB"; silence as −∞ or -inf.
    #[test]
    fn volumes_typed() {
        assert_eq!(parse_db("-6"), Some(-6.0));
        assert_eq!(parse_db("−6"), Some(-6.0));
        assert_eq!(parse_db("+3.5"), Some(3.5));
        assert_eq!(parse_db(" 4,5 dB "), Some(4.5));
        assert_eq!(parse_db("−12.5dB"), Some(-12.5));
        assert_eq!(parse_db("0"), Some(0.0));
        assert_eq!(parse_db("−∞"), Some(f32::NEG_INFINITY));
        assert_eq!(parse_db("-inf"), Some(f32::NEG_INFINITY));
        assert_eq!(parse_db("inf"), None);
        assert_eq!(parse_db("nan"), None);
        assert_eq!(parse_db("loud"), None);
        assert_eq!(parse_db(""), None);
        // What's shown to type over reads back as itself.
        for db in [-96.0, -12.5, 0.0, 3.0] {
            let back = parse_db(&db_edit_text(db, -96.0)).unwrap();
            assert!(back == db || (db <= -96.0 && back == f32::NEG_INFINITY));
        }
    }

    /// A size's sides are even: typed, rounded the way they were changed;
    /// following the other (its shape kept), the nearest; within range.
    #[test]
    fn sizes_are_even() {
        let r = 144..=4096;
        assert_eq!(typed_side(1081, 1080, &r), 1082);
        assert_eq!(typed_side(1079, 1080, &r), 1078);
        assert_eq!(typed_side(1351, 1920, &r), 1350);
        assert_eq!(typed_side(4096, 4096, &r), 4096);
        assert_eq!(typed_side(145, 144, &r), 146);
        assert_eq!(following_side(1080.0 * 16.0 / 9.0, &r), 1920);
        assert_eq!(following_side(607.5, &r), 608);
        assert_eq!(following_side(9000.0, &r), 4096);
        assert_eq!(following_side(64.0, &(64..=3600)), 64);
    }

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

    /// Coming into focus: the box is drawn once, at the fade's opacity;
    /// what's in it half way is copies with itself on top (so none covers
    /// it), together as visible as the fade; sharp, it's drawn once, whole.
    #[test]
    fn a_dialog_comes_into_focus() {
        let ctx = egui::Context::default();
        let frame = egui::Frame::new().fill(Color32::BLACK).inner_margin(10);
        let draw = |x: f32| {
            let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 300.0))), ..Default::default() };
            let mut shapes = Vec::new();
            let mut out = ctx.run_ui(raw, |ui| {
                let layer = ui.layer_id();
                let start = ui.ctx().graphics(|g| g.get(layer).map_or(0, |l| l.next_idx().0));
                focus_in(ui, x, frame, |ui| ui.painter().rect_filled(egui::Rect::from_min_size(egui::pos2(100.0, 100.0), egui::vec2(100.0, 50.0)), 0, Color32::WHITE));
                shapes = ui.ctx().graphics(|g| g.get(layer).map(|l| l.all_entries().skip(start).map(|cs| cs.shape.clone()).filter(|s| !matches!(s, egui::Shape::Noop)).collect::<Vec<_>>()).unwrap_or_default());
            });
            out.textures_delta.clear();
            shapes
        };
        let rect = |s: &egui::Shape| match s {
            egui::Shape::Rect(r) => r.clone(),
            egui::Shape::Vec(v) if v.len() == 1 => if let egui::Shape::Rect(r) = &v[0] { r.clone() } else { panic!("a rect") },
            _ => panic!("a rect: {s:?}"),
        };
        let half = draw(0.5);
        let copies: usize = FOCUS_RINGS.iter().map(|(_, n)| n).sum();
        assert_eq!(half.len(), 1 + copies + 1, "the box, the copies, itself");
        let boxed = rect(&half[0]);
        assert!((boxed.fill.a() as f32 / 255.0 - theme::ease_out(0.5)).abs() < 0.02, "the box fades, once");
        let content: Vec<_> = half[1..].iter().map(rect).collect();
        assert_eq!(content.last().unwrap().rect.min, egui::pos2(100.0, 100.0), "itself on top, in place");
        let left = content.iter().fold(1.0, |left, r| left * (1.0 - r.fill.a() as f32 / 255.0));
        assert!(((1.0 - left) - theme::ease_out(0.5)).abs() < 0.02, "as visible as a fade: {}", 1.0 - left);
        let done = draw(1.0);
        assert_eq!(done.len(), 2);
        assert_eq!((rect(&done[0]).fill, rect(&done[0]).blur_width), (Color32::BLACK, 0.0));
        assert_eq!(rect(&done[1]).fill, Color32::WHITE);
    }

    /// A card's height: there at once the first time and at another width;
    /// a line appearing in it eases it to its new height.
    #[test]
    fn a_card_eases_its_height() {
        let ctx = egui::Context::default();
        apply(&ctx);
        let frame_at = |time: f64, width: f32, lines: usize| {
            let raw = egui::RawInput { time: Some(time), screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 600.0))), ..Default::default() };
            let mut h = 0.0;
            let mut out = ctx.run_ui(raw, |ui| {
                let top = ui.cursor().min.y;
                card(ui, "test", |ui| {
                    for _ in 0..lines {
                        ui.allocate_space(egui::vec2(10.0, 20.0));
                    }
                });
                h = ui.cursor().min.y - top;
            });
            out.textures_delta.clear();
            h
        };
        let one = frame_at(0.0, 400.0, 1);
        assert_eq!(frame_at(0.1, 400.0, 1), one, "steady");
        let mid = frame_at(1.0, 400.0, 2);
        assert_eq!(mid, one, "starts where it was");
        let half = frame_at(1.0 + f64::from(theme::NORMAL) / 2.0, 400.0, 2);
        assert!(half > one && half < one + 20.0 + 8.0, "half way: {half}");
        let two = frame_at(2.0, 400.0, 2);
        assert!(two > half);
        assert_eq!(frame_at(3.0, 500.0, 1), one, "another width: at once");
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

    /// Frames of a ui with just `show` in it, at (10, 10), 150 wide: run
    /// with each frame's events as a person's input.
    fn run_frames(frames: Vec<Vec<egui::Event>>, mut show: impl FnMut(&mut egui::Ui)) {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        for events in frames {
            let raw = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(400.0, 300.0))), events, ..Default::default() };
            let mut out = ctx.run_ui(raw, |ui| {
                let mut at = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_min_size(Pos2::new(10.0, 10.0), vec2(300.0, CONTROL_H))).layout(egui::Layout::left_to_right(egui::Align::Center)));
                show(&mut at);
            });
            out.textures_delta.clear();
        }
    }

    /// A height's value field (1440 now, 144 to 2160, to `decimals`
    /// places), clicked and typed into as a person would: what it gave back.
    fn type_into(decimals: usize, frames: Vec<Vec<egui::Event>>) -> Vec<f64> {
        let mut got = Vec::new();
        run_frames(frames, |ui| {
            let presets = vec![Preset::new("1080p", 1080.0), Preset::new("720p", 720.0)];
            let field = ValueField { id: egui::Id::new("vf"), value: 1440.0, decimals, range: 144.0..=2160.0, unit: "p", width: 150.0, hint: "", presets };
            got.extend(field.show(ui));
        });
        got
    }

    /// A resolution's choice field, now `now` (Original, or 1080p) or a
    /// height of its own (`None`, 900), clicked and typed into as a person
    /// would: what it was set to.
    fn choose(now: Option<usize>, frames: Vec<Vec<egui::Event>>) -> Vec<Chosen> {
        let mut got = Vec::new();
        run_frames(frames, |ui| {
            let field = ChoiceField { id: "cf", choices: vec![Choice::new("Original"), Choice::new("1080p")], custom: Some(Custom { unit: "p", range: 144..=2160, note: "" }), width: 150.0 };
            got.extend(field.show(ui, now, if now == Some(1) { 1080 } else { 900 }));
        });
        got
    }

    /// Clicked, typed into, Enter.
    fn click_type_enter(text: &str) -> Vec<Vec<egui::Event>> {
        let at = Pos2::new(40.0, 10.0 + CONTROL_H / 2.0);
        let button = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
        let key = |key| egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() };
        vec![vec![egui::Event::PointerMoved(at)], vec![button(true)], vec![button(false)], vec![], vec![egui::Event::Text(text.to_owned())], vec![key(egui::Key::Enter)], vec![]]
    }

    /// Clicked, typing replaces what was there; only a number goes in, and
    /// one beyond its range is kept to it.
    #[test]
    fn a_value_field_only_takes_a_number() {
        assert_eq!(type_into(0, click_type_enter("1080")), [1080.0]);
        assert_eq!(type_into(0, click_type_enter("9a0b0p")), [900.0], "the letters don't go in");
        assert!(type_into(0, click_type_enter("Native")).is_empty(), "a word isn't typing: it stays as it was");
        assert_eq!(type_into(0, click_type_enter("99999")), [2160.0], "kept to its most");
        assert_eq!(type_into(0, click_type_enter("5")), [144.0], "kept to its least");
        assert_eq!(type_into(1, click_type_enter("900,5")), [900.5], "a comma is a point");
        assert_eq!(type_into(0, click_type_enter("900,5")), [2160.0], "no point without decimals: 9005, kept to 2160");
    }

    /// A choice field's words are only ever picked: clicked where they show,
    /// it opens its menu, and typing changes nothing. A number of its own is
    /// in a box beside the menu, where only digits go in.
    #[test]
    fn a_choice_field_types_only_numbers() {
        assert!(choose(Some(0), click_type_enter("1080")).is_empty(), "Original can't be typed over");
        assert!(choose(Some(1), click_type_enter("900")).is_empty(), "nor 1080p");
        assert_eq!(choose(None, click_type_enter("720")), [Chosen::Number(720)]);
        assert!(choose(None, click_type_enter("abc")).is_empty(), "letters don't go in its box");
    }

    /// Two number boxes, clicked and typed into as a person would: each
    /// one's number at the end.
    fn number_boxes(frames: Vec<Vec<egui::Event>>) -> [u32; 2] {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        let mut nums = [16, 9];
        for events in frames {
            let raw = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(400.0, 300.0))), events, ..Default::default() };
            let mut out = ctx.run_ui(raw, |ui| {
                let mut at = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_min_size(Pos2::new(10.0, 10.0), vec2(200.0, CONTROL_H))).layout(egui::Layout::left_to_right(egui::Align::Center)));
                for (i, n) in nums.iter_mut().enumerate() {
                    if let Some(v) = number_box(&mut at, egui::Id::new(("nb", i)), *n, 1..=99, 46.0) {
                        *n = v;
                    }
                }
            });
            out.textures_delta.clear();
        }
        nums
    }

    fn key(key: egui::Key, shift: bool) -> egui::Event {
        egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers { shift, ..Default::default() } }
    }

    /// The first box clicked, then what `then` does, a frame each.
    fn first_box_then(then: Vec<egui::Event>) -> Vec<Vec<egui::Event>> {
        let at = Pos2::new(30.0, 10.0 + CONTROL_H / 2.0);
        let button = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
        let mut frames = vec![vec![egui::Event::PointerMoved(at)], vec![button(true)], vec![button(false)], vec![]];
        frames.extend(then.into_iter().map(|e| vec![e]));
        frames.push(vec![]);
        frames
    }

    /// A shape as a ratio in small numbers: exact ones in their smallest
    /// numbers, any other the nearest with both up to 99; and which are common.
    #[test]
    fn ratios_in_small_numbers() {
        assert_eq!(ratio_of(16.0 / 9.0), [16, 9]);
        assert_eq!(ratio_of(1920.0 / 1080.0), [16, 9]);
        assert_eq!(ratio_of(5.0 / 4.0), [5, 4]);
        assert_eq!(ratio_of(9.0 / 16.0), [9, 16]);
        // Any typed comes back as typed (it's kept as a number): 2.35:1.
        assert_eq!(ratio_of(47.0 / 20.0), [47, 20]);
        assert_eq!(ratio_of(3600.0 / 2338.0), [97, 63]);
        assert_eq!(ratio_of(1000.0), [99, 1]);
        assert_eq!(common_ratio(1920, 1080), Some(0));
        assert_eq!(common_ratio(1081, 1920), Some(6));
        assert_eq!(common_ratio(5, 4), None);
    }

    /// Typing replaces the number; arrows add or take one (Shift: ten); Tab
    /// goes on to the next box; Esc puts it back. Pasted, a number goes in as
    /// typed, what isn't one left out.
    #[test]
    fn number_boxes_type_and_step() {
        use egui::{Event, Key};
        assert_eq!(number_boxes(first_box_then(vec![Event::Text("21".into()), key(Key::Enter, false)])), [21, 9]);
        assert_eq!(number_boxes(first_box_then(vec![Event::Paste("4 px2".into()), key(Key::Enter, false)])), [42, 9]);
        assert_eq!(number_boxes(first_box_then(vec![key(Key::ArrowUp, false), key(Key::ArrowUp, true)])), [27, 9]);
        assert_eq!(number_boxes(first_box_then(vec![key(Key::ArrowDown, false)])), [15, 9]);
        assert_eq!(number_boxes(first_box_then(vec![Event::Text("21".into()), key(Key::Tab, false), Event::Text("10".into()), key(Key::Enter, false)])), [21, 10]);
        assert_eq!(number_boxes(first_box_then(vec![Event::Text("5".into()), key(Key::Escape, false)])), [16, 9]);
        // Kept within its range.
        assert_eq!(number_boxes(first_box_then(vec![key(Key::ArrowUp, true); 10])), [99, 9]);
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
