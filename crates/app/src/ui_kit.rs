//! The app's look, in one place: spacing, sizes, colours and the few building
//! blocks every page is made of, so the pages read as one app.
//!
//! - **Type**: 20 page titles, 15 section titles, 14 body and buttons, 12
//!   hints and details. Hints say what isn't obvious, never what the label
//!   already says.
//! - **Space**: steps of 4 (4, 8, 12, 16, 24).
//! - **Surfaces**: the page, and cards a shade lighter, without borders.
//! - **Controls**: 30 px tall, corners of 6; the screen's main action is blue
//!   and last. Selected things are blue everywhere.
//! - **No emoji** in labels: they render unevenly and add noise. Pictures
//!   that carry meaning are drawn (sidebar icons) or are the thing itself
//!   (a game's logo, a clip's frame).

use egui::{Color32, CornerRadius, FontFamily, FontId, Margin, RichText, Stroke, TextStyle, Vec2};

pub const ACCENT: Color32 = Color32::from_rgb(90, 150, 255);
pub const DANGER: Color32 = Color32::from_rgb(235, 72, 72);

pub const CONTROL_H: f32 = 30.0;
/// Buttons, fields, rows.
pub const RADIUS: u8 = 6;
/// Cards and the sidebar.
pub const CARD_RADIUS: u8 = 10;
/// Space between a page's edge and its content.
pub const PAGE_MARGIN: i8 = 16;

/// Set the look for every theme. Called once at start-up.
pub fn apply(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        let dark = style.visuals.dark_mode;
        let s = &mut style.spacing;
        s.item_spacing = Vec2::new(8.0, 6.0);
        s.button_padding = Vec2::new(10.0, 4.0);
        s.interact_size = Vec2::new(30.0, 26.0);
        s.menu_margin = Margin::same(6);
        s.window_margin = Margin::same(16);
        s.combo_height = 300.0;

        use TextStyle::*;
        style.text_styles = [
            (Small, FontId::new(12.0, FontFamily::Proportional)),
            (Body, FontId::new(14.0, FontFamily::Proportional)),
            (Button, FontId::new(14.0, FontFamily::Proportional)),
            (Heading, FontId::new(20.0, FontFamily::Proportional)),
            (Monospace, FontId::new(13.0, FontFamily::Monospace)),
        ]
        .into();

        let v = &mut style.visuals;
        // Text that reads easily: body clearly light, hints a step quieter
        // but still legible (egui's defaults made both dim).
        let (text, weak) = if dark { (Color32::from_gray(214), Color32::from_gray(150)) } else { (Color32::from_gray(30), Color32::from_gray(105)) };
        v.widgets.noninteractive.fg_stroke.color = text;
        v.widgets.inactive.fg_stroke.color = text;
        v.weak_text_color = Some(weak);
        v.selection.bg_fill = ACCENT.gamma_multiply(if dark { 0.55 } else { 0.35 });
        // egui uses this for the text on a selected option *and* a focused
        // field's outline: light, so selected text reads on the blue
        // (an accent colour here made it blue on blue).
        v.selection.stroke = Stroke::new(1.0, if dark { Color32::from_gray(235) } else { Color32::from_gray(20) });
        v.hyperlink_color = ACCENT;
        v.window_corner_radius = CornerRadius::same(CARD_RADIUS);
        v.menu_corner_radius = CornerRadius::same(8);
        v.window_stroke = Stroke::new(1.0, line(dark));
        v.slider_trailing_fill = true;
        // Controls: filled, no outline; hover lightens, nothing grows.
        let fill = if dark { Color32::from_white_alpha(16) } else { Color32::from_black_alpha(14) };
        let hover = if dark { Color32::from_white_alpha(26) } else { Color32::from_black_alpha(22) };
        let press = if dark { Color32::from_white_alpha(34) } else { Color32::from_black_alpha(30) };
        for (w, f) in [
            (&mut v.widgets.inactive, fill),
            (&mut v.widgets.hovered, hover),
            (&mut v.widgets.active, press),
            (&mut v.widgets.open, hover),
        ] {
            w.weak_bg_fill = f;
            w.bg_fill = f;
            w.bg_stroke = Stroke::NONE;
            w.corner_radius = CornerRadius::same(RADIUS);
            w.expansion = 0.0;
        }
        v.widgets.noninteractive.corner_radius = CornerRadius::same(RADIUS);
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, line(dark));
    });
}

/// Hairlines (dividers).
pub fn line(dark: bool) -> Color32 {
    if dark { Color32::from_white_alpha(18) } else { Color32::from_black_alpha(20) }
}

/// Cards (and the sidebar): a shade off the page.
pub fn surface(v: &egui::Visuals) -> Color32 {
    if v.dark_mode { Color32::from_white_alpha(7) } else { Color32::from_black_alpha(9) }
}

/// A row or tile under the pointer.
pub fn hover(v: &egui::Visuals) -> Color32 {
    if v.dark_mode { Color32::from_white_alpha(10) } else { Color32::from_black_alpha(12) }
}

/// A card: content on a surface, padded.
pub fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .fill(surface(ui.visuals()))
        .corner_radius(CARD_RADIUS)
        .inner_margin(Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// A titled group: its title above, its content in a card.
pub fn section<R>(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.add_space(20.0);
    ui.label(RichText::new(title).size(15.0).strong());
    ui.add_space(8.0);
    card(ui, add)
}

/// Scrolling content with its scrollbar in the page's right margin, between
/// the content and the window's edge: never over the content, and the content
/// doesn't move when the bar widens under the pointer.
pub fn scroll<R>(ui: &mut egui::Ui, id: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let reset = ui.ctx().data(|d| d.get_temp::<bool>(scroll_reset_id())).unwrap_or(false);
    let gutter = PAGE_MARGIN as f32;
    let mut rect = ui.available_rect_before_wrap();
    rect.max.x += gutter;
    let mut area = ui.new_child(egui::UiBuilder::new().max_rect(rect));
    area.spacing_mut().scroll = egui::style::ScrollStyle { bar_outer_margin: 3.0, bar_inner_margin: 0.0, ..egui::style::ScrollStyle::floating() };
    let mut scroll = egui::ScrollArea::vertical().id_salt(id).auto_shrink([false, false]);
    if reset {
        scroll = scroll.vertical_scroll_offset(0.0);
    }
    scroll
        .show(&mut area, |ui| {
            ui.set_max_width(ui.available_width() - gutter);
            add(ui)
        })
        .inner
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

/// A page of settings-like content: scrolls, centred, at most `max_width`
/// wide, with the page's margins.
pub fn page(ui: &mut egui::Ui, id: &str, max_width: f32, add: impl FnOnce(&mut egui::Ui)) {
    scroll(ui, id, |ui| {
        let w = ui.available_width().min(max_width);
        let side = ((ui.available_width() - w) / 2.0).max(0.0);
        ui.horizontal(|ui| {
            ui.add_space(side);
            ui.vertical(|ui| {
                ui.set_width(w);
                add(ui);
                ui.add_space(24.0);
            });
        });
    });
}

/// A short line of help: smaller and quieter than what it explains, wrapping.
pub fn hint(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(egui::Label::new(RichText::new(text).size(12.0).color(ui.visuals().weak_text_color())).wrap())
}

/// A button at the standard height; `primary` is the screen's main action.
pub fn button(text: impl Into<String>, primary: bool) -> egui::Button<'static> {
    let text = text.into();
    let b = egui::Button::new(if primary { RichText::new(text).color(Color32::WHITE) } else { RichText::new(text) }).min_size(Vec2::new(0.0, CONTROL_H));
    if primary { b.fill(ACCENT) } else { b }
}

/// A destructive action's button (delete for good, stop recording).
pub fn danger_button(text: impl Into<String>) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.into()).color(Color32::WHITE)).fill(DANGER).min_size(Vec2::new(0.0, CONTROL_H))
}

/// A choice of a few options side by side: the page tabs, and two-way
/// switches like Whole screen / Games and apps. The same height as a button
/// (30), on a card's surface, the chosen one filled like a button; hover as
/// everywhere. `equal`: options share the row's width; else each fits its
/// text. Returns each option's response.
/// Space between a [`segmented`] control's edge and its options.
pub const SEGMENT_PAD: f32 = 3.0;

pub fn segmented_with(ui: &mut egui::Ui, labels: &[&str], chosen: usize, equal: bool) -> Vec<egui::Response> {
    let v = ui.visuals().clone();
    let pad = SEGMENT_PAD;
    let mut out = Vec::new();
    egui::Frame::new().fill(surface(&v)).corner_radius(8).inner_margin(Margin::same(pad as i8)).show(ui, |ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        let n = labels.len().max(1) as f32;
        let share = ((ui.available_width() - 2.0 * (n - 1.0)) / n).max(60.0);
        ui.horizontal(|ui| {
            for (i, label) in labels.iter().enumerate() {
                let galley = ui.painter().layout_no_wrap(label.to_string(), FontId::proportional(14.0), Color32::WHITE);
                let w = if equal { share } else { galley.size().x + 24.0 };
                let (rect, r) = ui.allocate_exact_size(Vec2::new(w, CONTROL_H - 2.0 * pad), egui::Sense::click());
                let on = i == chosen;
                if on {
                    ui.painter().rect_filled(rect, RADIUS, v.widgets.inactive.weak_bg_fill);
                } else if r.hovered() && ui.is_enabled() {
                    ui.painter().rect_filled(rect, RADIUS, hover(&v));
                }
                let color = if on { v.strong_text_color() } else if r.hovered() { v.text_color() } else { v.weak_text_color() };
                ui.painter().galley_with_override_text_color(rect.center() - galley.size() / 2.0, galley, color);
                out.push(r);
            }
        });
    });
    out
}

/// [`segmented_with`], the options sharing the row's width.
pub fn segmented(ui: &mut egui::Ui, labels: &[&str], chosen: usize) -> Vec<egui::Response> {
    segmented_with(ui, labels, chosen, true)
}

/// A small "?" that opens the screen's keyboard and mouse shortcuts: what a
/// line of tiny grey text used to say, readable and out of the way. `rows`:
/// (keys, what they do).
pub fn shortcuts_button(ui: &mut egui::Ui, rows: &[(&str, &str)]) -> egui::Response {
    let (rect, r) = ui.allocate_exact_size(Vec2::splat(26.0), egui::Sense::click());
    let v = ui.visuals().clone();
    let open = egui::Popup::is_id_open(ui.ctx(), ui.id().with("shortcuts"));
    let color = if r.hovered() || open { v.strong_text_color() } else { v.weak_text_color() };
    if r.hovered() || open {
        ui.painter().circle_filled(rect.center(), 12.0, hover(&v));
    }
    ui.painter().circle_stroke(rect.center(), 9.0, Stroke::new(1.3, color));
    ui.painter().text(rect.center() + Vec2::new(0.0, 0.5), egui::Align2::CENTER_CENTER, "?", FontId::proportional(12.0), color);
    let r = r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Shortcuts");
    egui::Popup::menu(&r).id(ui.id().with("shortcuts")).show(|ui| {
        ui.set_min_width(300.0);
        ui.label(RichText::new("Shortcuts").strong());
        ui.add_space(6.0);
        egui::Grid::new("shortcuts_grid").num_columns(2).spacing(Vec2::new(16.0, 6.0)).show(ui, |ui| {
            for (keys, what) in rows {
                ui.label(RichText::new(*keys).color(ui.visuals().strong_text_color()));
                ui.label(*what);
                ui.end_row();
            }
        });
    });
    r
}

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
    at: Option<egui::Pos2>,
}

fn press_id() -> egui::Id {
    egui::Id::new("ui_kit_lenient_press")
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
}

