//! The header shared by the player and the editor, so they read the same:
//!
//! ```text
//! ‹ Clips  [‹][›] 5 of 74   Clip name…  ✏           [actions…] [Primary]
//! ```
//!
//! Back on the left, then anything that steps between clips (fixed, so it
//! never moves with the name), then the clip's name filling what's left —
//! shortened to fit, renamed with the pencil, a double-click or F2 — and the
//! actions on the right, the main one last and blue.

use egui::{Color32, RichText, Sense, Vec2};

use crate::library::ACCENT;

pub const HEIGHT: f32 = 32.0;

/// "‹ Clips": back to the library.
pub fn back(ui: &mut egui::Ui) -> egui::Response {
    ui.add(button("‹ Clips", false)).on_hover_text("Back to your clips  (Esc)")
}

/// A header button; `primary` is the screen's main action.
pub fn button(text: &str, primary: bool) -> egui::Button<'static> {
    let b = egui::Button::new(if primary { RichText::new(text).size(14.0).color(Color32::WHITE) } else { RichText::new(text).size(14.0) })
        .min_size(Vec2::new(0.0, 30.0))
        .corner_radius(8);
    if primary { b.fill(ACCENT).min_size(Vec2::new(80.0, 30.0)) } else { b }
}

/// A small square button with one symbol, like the arrows between clips.
pub fn icon(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text).size(16.0)).min_size(Vec2::new(30.0, 30.0)).corner_radius(8)
}

/// The clip's name in what room is left, with a pencil to rename it. Put it
/// last, inside the right-to-left part holding the actions. `tag`: a small
/// label before it (the editor's "Editing"). Whether a rename was asked for.
pub fn title(ui: &mut egui::Ui, name: &str, tag: Option<&str>) -> bool {
    let mut rename = false;
    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
        if let Some(tag) = tag {
            egui::Frame::new()
                .fill(ACCENT.gamma_multiply(0.22))
                .corner_radius(6)
                .inner_margin(egui::Margin::symmetric(8, 3))
                .show(ui, |ui| ui.label(RichText::new(tag).size(13.0).color(ACCENT)));
            ui.add_space(4.0);
        }
        // Leave room for the pencil after the (possibly shortened) name.
        let room = (ui.available_width() - 36.0).max(40.0);
        let label = ui
            .allocate_ui(Vec2::new(room, HEIGHT), |ui| {
                ui.add(egui::Label::new(RichText::new(name).size(18.0).strong()).truncate().sense(Sense::click()))
            })
            .inner;
        if label.on_hover_text(format!("{name}\nDouble-click to rename")).double_clicked() {
            rename = true;
        }
        let pencil = egui::Button::new(RichText::new("✏").size(14.0)).fill(Color32::TRANSPARENT).min_size(Vec2::splat(28.0)).corner_radius(6);
        if ui.add(pencil).on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Rename  (F2)").clicked() {
            rename = true;
        }
    });
    rename
}
