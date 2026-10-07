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
        // The outline of a focused text field.
        v.selection.stroke = Stroke::new(1.0, ACCENT);
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

/// A page of settings-like content: scrolls, centred, at most `max_width`
/// wide, with the page's margins.
pub fn page(ui: &mut egui::Ui, id: &str, max_width: f32, add: impl FnOnce(&mut egui::Ui)) {
    egui::ScrollArea::vertical().id_salt(id).auto_shrink([false, false]).show(ui, |ui| {
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

/// A choice of a few options, side by side in a sunken track, the chosen one
/// raised: the same look as the page tabs. Equal widths filling the row.
/// Returns each option's response.
pub fn segmented(ui: &mut egui::Ui, labels: &[&str], chosen: usize) -> Vec<egui::Response> {
    let v = ui.visuals().clone();
    let mut out = Vec::new();
    egui::Frame::new().fill(v.extreme_bg_color).corner_radius(8).inner_margin(Margin::same(3)).show(ui, |ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        let n = labels.len().max(1) as f32;
        let w = ((ui.available_width() - 2.0 * (n - 1.0)) / n).max(60.0);
        ui.horizontal(|ui| {
            for (i, label) in labels.iter().enumerate() {
                let (rect, r) = ui.allocate_exact_size(Vec2::new(w, CONTROL_H - 6.0), egui::Sense::click());
                let on = i == chosen;
                if on {
                    ui.painter().rect_filled(rect, RADIUS, v.widgets.active.weak_bg_fill);
                } else if r.hovered() && ui.is_enabled() {
                    ui.painter().rect_filled(rect, RADIUS, hover(&v));
                }
                let color = if on { v.strong_text_color() } else if r.hovered() { v.text_color() } else { v.weak_text_color() };
                ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, *label, FontId::proportional(14.0), color);
                out.push(r.on_hover_cursor(egui::CursorIcon::PointingHand));
            }
        });
    });
    out
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
