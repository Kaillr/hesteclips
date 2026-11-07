//! Where a source is in the video, as numbers: its position and size in
//! the video's pixels, and how it's turned and flipped. Under its card's
//! Placement row, right under the preview, so what's typed shows there at
//! once and what's dragged there shows here: the preview's drags, its menu
//! and these are three ways to the same place.

use egui::{Pos2, Rect, vec2};

use crate::App;
use crate::canvas_ui::Layer;
use crate::theme::{self, W};
use crate::ui_kit::{self, Btn, Kind};

/// A box (fractions of the video) in a `vw`×`vh` video's pixels: left,
/// top, width, height.
pub(crate) fn to_pixels(r: Rect, (vw, vh): (u32, u32)) -> [i64; 4] {
    let px = |v: f32, n: u32| (v as f64 * n as f64).round() as i64;
    [px(r.min.x, vw), px(r.min.y, vh), px(r.width(), vw), px(r.height(), vh)]
}

/// A box in a `vw`×`vh` video's pixels back to fractions of it.
pub(crate) fn from_pixels([x, y, w, h]: [i64; 4], (vw, vh): (u32, u32)) -> Rect {
    let f = |v: i64, n: u32| (v as f64 / n.max(1) as f64) as f32;
    Rect::from_min_size(Pos2::new(f(x, vw), f(y, vh)), vec2(f(w, vw), f(h, vh)))
}

/// Where a source is, for its card: its size and place in the video's
/// pixels (`at`, when that's said), then what's been done to it. Empty
/// when there's nothing to say.
pub(crate) fn summary(at: Option<&str>, stretched: bool, turns: u8, mirrored: bool, upside_down: bool) -> String {
    let turned = match turns % 4 {
        1 => Some("turned 90°"),
        2 => Some("turned 180°"),
        3 => Some("turned 270°"),
        _ => None,
    };
    let parts: Vec<&str> = [at, stretched.then_some("stretched"), turned, mirrored.then_some("mirrored"), upside_down.then_some("upside down")].into_iter().flatten().collect();
    let mut text = parts.join(" · ");
    if let Some(first) = text.get(..1) {
        text.replace_range(..1, &first.to_uppercase());
    }
    text
}

/// A box's size and place in the video's pixels, as a card says it.
fn size_at([x, y, w, h]: [i64; 4]) -> String {
    format!("{w}×{h} at {x}, {y}")
}

impl App {
    /// A source's card open, its Placement numbers open under it, and
    /// brought into view (its menu's Edit placement).
    pub(crate) fn open_placement(&mut self, layer: Layer) {
        let v = &mut self.sources_view;
        for open in [&mut v.open_cards, &mut v.placement_open] {
            if !open.contains(&layer) {
                open.push(layer);
            }
        }
        v.placement_shown = Some(layer);
    }

    /// The video's size in pixels, its shape, and where a source is in it
    /// (fractions of it), when they're known.
    fn placed_in(&mut self, layer: Layer) -> Option<((u32, u32), f32, Rect)> {
        let (vw, vh) = capture::frame_size(&self.video_source(), self.settings.video_size())?;
        let aspect = vw as f32 / vh.max(1) as f32;
        let rect = match layer {
            Layer::Screen => self.screen_rect(aspect),
            Layer::Webcam(key) => self.webcam_placement(key).map(|p| Rect::from_min_size(Pos2::new(p.x, p.y), vec2(p.w, p.h)))?,
            Layer::Visual(key) => self.visual_rect(key, aspect),
        };
        Some(((vw, vh), aspect, rect))
    }

    /// A source's Placement row on its card: where it is and what's been
    /// done to it (while its numbers are shut), how the video puts the
    /// screen in, and its numbers, opening under it.
    pub(crate) fn placement_row(&mut self, ui: &mut egui::Ui, layer: Layer) {
        let pal = theme::pal(ui);
        let screen = layer == Layer::Screen;
        let open = self.sources_view.placement_open.contains(&layer);
        let at = self.placed_in(layer).map(|(video, _, r)| size_at(to_pixels(r, video)));
        let text = match layer {
            _ if open => String::new(),
            Layer::Screen => {
                let s = self.settings.screen;
                // Placed by hand, or put in by Fit, Fill or Stretch (beside it).
                let at = s.placed.and(at.as_deref());
                summary(at, s.placed.is_some_and(|b| b.stretched()), s.turns, s.flip_h, s.flip_v)
            }
            Layer::Webcam(key) => {
                let Some(p) = self.webcam_placement(key) else { return };
                summary(at.as_deref(), (p.stretch - 1.0).abs() > 0.02, p.turns, p.flip_h, p.flip_v)
            }
            Layer::Visual(key) => {
                let Some(d) = self.settings.visual(key).cloned() else { return };
                summary(d.placed.and(at.as_deref()), d.placed.is_some_and(|b| b.stretched()), d.turns, d.flip_h, d.flip_v)
            }
        };
        let was = self.arrangement();
        let mut toggle = false;
        let row = ui.horizontal(|ui| {
            ui.set_min_height(crate::sources_ui::GROUP_ROW_H);
            ui.spacing_mut().item_spacing.x = 5.0;
            let label = theme::text("Placement", 13.5, W::Regular).color(pal.text);
            if text.is_empty() {
                ui.label(label);
            } else {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.add_space(6.0);
                    ui.label(label);
                    ui.label(theme::text(&text, 12.0, W::Regular).color(pal.text3));
                    ui.add_space(6.0);
                });
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                let caret = if open { egui_phosphor::regular::CARET_UP } else { egui_phosphor::regular::CARET_DOWN };
                let tip = if open { "Hide its numbers" } else { "Its position, size, turn and flip, as numbers" };
                toggle = Btn::normal("Adjust").trail(caret).small().tip(tip).show(ui).clicked();
                if screen && let Some(f) = crate::canvas_ui::framing_choice(ui, "screen_card_framing", self.settings.screen.placed.is_none().then_some(self.settings.framing)) {
                    self.screen_framed(f);
                }
            });
        });
        // Opened from its menu: in view, with room for the numbers under it.
        if self.sources_view.placement_shown == Some(layer) {
            self.sources_view.placement_shown = None;
            ui.scroll_to_rect(row.response.rect, Some(egui::Align::TOP));
        }
        if toggle {
            let v = &mut self.sources_view.placement_open;
            if open { v.retain(|l| *l != layer) } else { v.push(layer) }
        }
        let open = self.sources_view.placement_open.contains(&layer);
        ui_kit::reveal(ui, ("placement_numbers", layer), open, |ui| self.placement_numbers(ui, layer));
        if self.arrangement() != was {
            self.keep_for_undo(was);
        }
    }

    /// A source's position and size in the video's pixels, how it's turned
    /// and flipped, and putting its position and size back: in a well under
    /// its Placement row, as settings that belong to the one above them are
    /// in Settings (`ui_kit::well`), its edges and text where theirs are.
    fn placement_numbers(&mut self, ui: &mut egui::Ui, layer: Layer) {
        use egui_phosphor::regular as icon;
        let Some(((vw, vh), aspect, rect)) = self.placed_in(layer) else { return };
        // The screen and other displays can reach past the video's edges.
        let screen = matches!(layer, Layer::Screen | Layer::Visual(_));
        let [x, y, w, h] = to_pixels(rect, (vw, vh));
        let (can_place, can_size) = self.layer_resettable(layer, aspect);
        let (turns, mirrored, upside_down) = match layer {
            Layer::Screen => (self.settings.screen.turns, self.settings.screen.flip_h, self.settings.screen.flip_v),
            Layer::Webcam(key) => self.webcam_placement(key).map_or((0, false, false), |p| (p.turns, p.flip_h, p.flip_v)),
            Layer::Visual(key) => self.settings.visual(key).map_or((0, false, false), |d| (d.turns, d.flip_h, d.flip_v)),
        };
        let size_id = match layer {
            Layer::Screen => "placement_screen_size".to_owned(),
            Layer::Webcam(key) => format!("placement_webcam_size_{key}"),
            Layer::Visual(key) => format!("placement_display_size_{key}"),
        };
        let size_id = size_id.as_str();
        // Linked at first: typed, the size keeps its shape, as dragging does
        // (unlinked, it stretches, as Shift does).
        let linked_once = egui::Id::new((size_id, "linked"));
        if ui.data(|d| d.get_temp::<bool>(linked_once)).is_none() {
            ui.data_mut(|d| d.insert_temp(linked_once, true));
            ui_kit::set_size_field_keeps(ui.ctx(), size_id, Some(w as f64 / h.max(1) as f64));
        }
        let pal = theme::pal(ui);
        let small = |ui: &mut egui::Ui, t: &str| ui.label(theme::text(t, 12.5, W::Regular).color(pal.text3));
        // The well, as a Settings well is: as far from the card's sides as
        // that is from its list's (into the card's padding), its rows padded
        // as that's are, the same height, with the same hairlines. Painted
        // under them once they're laid out.
        let reach = crate::sources_ui::GROUP_PAD - ui_kit::WELL_GAP;
        let pad = ui_kit::WELL_ROW_PAD - reach;
        let (well, top) = (ui.painter().add(egui::Shape::Noop), ui.cursor().min.y);
        ui.add_space(ui_kit::WELL_INSET);
        let mut first = true;
        // A row of the well: its name, then what's at its end.
        let mut row = |ui: &mut egui::Ui, label: &str, add: &mut dyn FnMut(&mut egui::Ui)| {
            if !std::mem::take(&mut first) {
                ui_kit::divider(ui, ui_kit::ROW_LINE_INSET - reach);
            }
            ui.horizontal(|ui| {
                ui.set_min_height(ui_kit::WELL_ROW_H);
                ui.add_space(pad);
                ui.label(theme::text(label, 13.5, W::Regular).color(pal.text));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.add_space(pad);
                    add(ui);
                });
            });
        };
        const BOX: f32 = 64.0;

        let mut moved: Option<[i64; 4]> = None;
        let (mut reset_place, mut reset_size, mut turn) = (false, false, None);
        let (mut m, mut u) = (mirrored, upside_down);
        // Its top-left corner. The screen can reach past the video's edges
        // (a part of it over all of the video), some of it in it.
        let (xs, ys) = if screen { (2 - w..=vw as i64 - 2, 2 - h..=vh as i64 - 2) } else { (0..=(vw as i64 - w).max(0), 0..=(vh as i64 - h).max(0)) };
        row(ui, "Position", &mut |ui| {
            // Left to right at the row's end, so Tab goes through them in order.
            ui.allocate_ui_with_layout(vec2(BOX * 2.0 + 6.0 + 16.0 + 6.0 * 3.0, ui_kit::CONTROL_H), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if let Some(v) = ui_kit::signed_box(ui, egui::Id::new((size_id, "x")), x, xs.clone(), BOX) {
                    moved = Some([v, y, w, h]);
                }
                small(ui, ",");
                if let Some(v) = ui_kit::signed_box(ui, egui::Id::new((size_id, "y")), y, ys.clone(), BOX) {
                    moved = Some([x, v, w, h]);
                }
                small(ui, "px");
            });
        });
        // Its size, its top-left corner staying put.
        row(ui, "Size", &mut |ui| {
            let most = if screen { (vw.max(vh) * 8).min(capture::MAX_SIDE * 8) } else { vw.max(vh) };
            if let Some((nw, nh)) = ui_kit::size_field(ui, size_id, (w.max(2) as u32, h.max(2) as u32), 16..=most, 16..=most) {
                moved = Some([x, y, nw as i64, nh as i64]);
            }
        });
        // A quarter at a time, as it's seen; how far, beside them.
        row(ui, "Turn", &mut |ui| {
            if Btn::icon(Kind::Normal, icon::ARROW_CLOCKWISE).small().tip("Turn clockwise").show(ui).clicked() {
                turn = Some(1);
            }
            if Btn::icon(Kind::Normal, icon::ARROW_COUNTER_CLOCKWISE).small().tip("Turn anticlockwise").show(ui).clicked() {
                turn = Some(-1);
            }
            ui.add_space(4.0);
            small(ui, &format!("{}°", u32::from(turns % 4) * 90));
        });
        row(ui, "Mirror", &mut |ui| {
            ui_kit::switch(ui, &mut m).on_hover_text("Left and right swapped");
        });
        row(ui, "Upside down", &mut |ui| {
            ui_kit::switch(ui, &mut u);
        });
        // Its first place and size again, each on its own.
        ui_kit::divider(ui, ui_kit::ROW_LINE_INSET - reach);
        ui.horizontal(|ui| {
            ui.set_min_height(ui_kit::WELL_ROW_H);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.add_space(pad);
                let size_tip = match layer {
                    Layer::Screen if self.video_shaped() => "Back to the size the video gives it, where it is",
                    Layer::Screen => "As big as the video again, where it is",
                    Layer::Webcam(_) => "A quarter of the video wide again, unstretched, where it is",
                    Layer::Visual(_) => "As big as fits in the video again, where it is",
                };
                reset_size = Btn::ghost("Reset size").small().enabled(can_size).tip(size_tip).show(ui).clicked();
                let place_tip = if screen { "Back in the middle of the video, its size kept" } else { "Back to the bottom-right corner, its size kept" };
                reset_place = Btn::ghost("Reset position").small().enabled(can_place).tip(place_tip).show(ui).clicked();
            });
        });
        ui.add_space(ui_kit::WELL_INSET);
        let r = Rect::from_x_y_ranges(ui.max_rect().x_range().expand(reach), top..=ui.cursor().min.y);
        ui.painter().set(well, egui::epaint::RectShape::filled(r, ui_kit::RADIUS, pal.well));
        // As far from the card's bottom as from its sides.
        ui.add_space((ui_kit::WELL_GAP - (crate::sources_ui::GROUP_PAD - (crate::sources_ui::GROUP_ROW_H - ui_kit::CONTROL_H) / 2.0)).max(0.0));

        if reset_place {
            self.layer_reset_position(layer, aspect);
        }
        if reset_size {
            self.layer_reset_size(layer, aspect);
        }
        if let Some(px) = moved {
            let r = from_pixels(px, (vw, vh));
            match layer {
                Layer::Screen => self.place_or_frame(r, aspect),
                Layer::Visual(key) => self.place_visual(key, r, aspect),
                Layer::Webcam(key) => {
                    if let Some(cam) = self.settings.webcam_mut(key) {
                        let p: capture::webcam::Placement = cam.placement.into();
                        // Another shape typed (unlinked): as much more stretched.
                        let stretch = p.stretch * (r.width() / r.height().max(1e-6)) / (p.w / p.h.max(1e-6));
                        let p = crate::webcam_ui::contain(capture::webcam::Placement { x: r.min.x, y: r.min.y, w: r.width(), h: r.height(), stretch, ..p });
                        cam.placement = crate::settings::PlacementCfg::laid_out(p, aspect);
                    }
                }
            }
        }
        if let Some(by) = turn {
            self.turn_layer(layer, by, aspect);
        }
        if m != mirrored {
            self.flip_layer(layer, true, aspect);
        } else if u != upside_down {
            self.flip_layer(layer, false, aspect);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A box in the video's pixels and back, past its edges too.
    #[test]
    fn boxes_in_pixels() {
        let video = (1920, 1080);
        let r = Rect::from_min_size(Pos2::new(0.75, 0.75), vec2(0.25, 0.25));
        assert_eq!(to_pixels(r, video), [1440, 810, 480, 270]);
        let past = Rect::from_min_size(Pos2::new(-0.1, 0.0), vec2(1.2, 1.0));
        assert_eq!(to_pixels(past, video), [-192, 0, 2304, 1080]);
        let back = from_pixels([-192, 0, 2304, 1080], video);
        assert!((back.min.x + 0.1).abs() < 1e-6 && (back.width() - 1.2).abs() < 1e-6, "{back:?}");
    }

    /// What a card says: where it is, then only what's been done, the
    /// first word capitalised.
    #[test]
    fn what_a_card_says() {
        assert_eq!(summary(None, false, 0, false, false), "");
        assert_eq!(summary(Some("fills the video"), false, 0, false, false), "Fills the video");
        let at = size_at([1426, 996, 790, 444]);
        assert_eq!(summary(Some(&at), true, 1, true, false), "790×444 at 1426, 996 · stretched · turned 90° · mirrored");
        assert_eq!(summary(None, false, 2, false, true), "Turned 180° · upside down");
    }
}
