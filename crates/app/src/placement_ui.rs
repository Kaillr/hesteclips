//! Where a source is in the video, as numbers, and what of it shows: the
//! Transform half of its settings' dialog, with Crop (see `inspector_ui`),
//! the same for every source. Its position and size in the video's pixels,
//! how it's turned and flipped, and how much of its picture is cut off each
//! side, in the picture's own pixels. What's typed shows at once (in the
//! dialog's look at the video, and on the preview behind it): the
//! preview's drags, its menu and these are three ways to the same place.

use egui::{Pos2, Rect, vec2};

use crate::App;
use crate::canvas_ui::Layer;
use crate::settings::ScreenCrop;
use crate::theme::{self, W};
use crate::ui_kit::{self, Btn, Kind, Row};

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

/// A crop as how much is cut off each side (left, top, right, bottom), of
/// a picture `size` big, from the part kept (left, top, width, height).
pub(crate) fn sides_of([x, y, w, h]: [u32; 4], (sw, sh): (u32, u32)) -> [u32; 4] {
    [x, y, sw.saturating_sub(x + w), sh.saturating_sub(y + h)]
}

/// The part kept of a picture `size` big with `sides` cut off (left, top,
/// right, bottom), at least `least` pixels each way, on even pixels: `None`
/// when nothing's cut off.
pub(crate) fn crop_of([l, t, r, b]: [u32; 4], (sw, sh): (u32, u32), least: u32) -> Option<ScreenCrop> {
    let even = |v: u32| v & !1;
    let (l, t) = (even(l.min(sw.saturating_sub(least))), even(t.min(sh.saturating_sub(least))));
    let w = even(sw.saturating_sub(l).saturating_sub(r).max(least).min(sw - l));
    let h = even(sh.saturating_sub(t).saturating_sub(b).max(least).min(sh - t));
    ((l, t, w, h) != (0, 0, sw & !1, sh & !1)).then_some(ScreenCrop { w, h, at: Some([l, t]) })
}

impl App {
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

    /// Where a source is: its position and size in the video's pixels, how
    /// it's turned and flipped, and putting its position and size back.
    pub(crate) fn transform_section(&mut self, ui: &mut egui::Ui, layer: Layer) {
        let pal = theme::pal(ui);
        // The half of the dialog it's in is called Transform.
        ui_kit::gap(ui, 10.0);
        let Some(((vw, vh), aspect, rect)) = self.placed_in(layer) else {
            ui.label(theme::text("Its place shows once the video's size is known.", 12.5, W::Regular).color(pal.text3));
            return;
        };
        // The screen and other boxed sources can reach past the video's edges.
        let past_edges = matches!(layer, Layer::Screen | Layer::Visual(_));
        let [x, y, w, h] = to_pixels(rect, (vw, vh));
        let (can_place, can_size) = self.layer_resettable(layer, aspect);
        let turns = self.layer_turns(layer);
        let (mirrored, upside_down) = self.layer_flips(layer);
        let size_id = match layer {
            Layer::Screen => "transform_screen".to_owned(),
            Layer::Webcam(key) => format!("transform_webcam_{key}"),
            Layer::Visual(key) => format!("transform_visual_{key}"),
        };
        let size_id = size_id.as_str();
        // Linked at first: typed, the size keeps its shape, as dragging does
        // (unlinked, it stretches, as Shift does).
        let linked_once = egui::Id::new((size_id, "linked"));
        if ui.data(|d| d.get_temp::<bool>(linked_once)).is_none() {
            ui.data_mut(|d| d.insert_temp(linked_once, true));
            ui_kit::set_size_field_keeps(ui.ctx(), size_id, Some(w as f64 / h.max(1) as f64));
        }
        let small = |ui: &mut egui::Ui, t: &str| ui.label(theme::text(t, 12.5, W::Regular).color(pal.text3));
        const BOX: f32 = 60.0;
        let mut moved: Option<[i64; 4]> = None;
        let (mut reset_place, mut reset_size, mut turn) = (false, false, None);
        let (mut m, mut u) = (mirrored, upside_down);
        // Its top-left corner. What can reach past the video's edges can be
        // put there, some of it still in it.
        let (xs, ys) = if past_edges { (2 - w..=vw as i64 - 2, 2 - h..=vh as i64 - 2) } else { (0..=(vw as i64 - w).max(0), 0..=(vh as i64 - h).max(0)) };
        ui_kit::list(ui, ("transform", layer), |l| {
            l.row(Row::new("Position"), |ui| {
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
            l.row(Row::new("Size"), |ui| {
                let most = if past_edges { (vw.max(vh) * 8).min(capture::MAX_SIDE * 8) } else { vw.max(vh) };
                if let Some((nw, nh)) = ui_kit::size_field(ui, size_id, (w.max(2) as u32, h.max(2) as u32), 16..=most, 16..=most) {
                    moved = Some([x, y, nw as i64, nh as i64]);
                }
            });
            // A quarter at a time, as it's seen; how far, beside them.
            l.row(Row::new("Turn"), |ui| {
                if Btn::icon(Kind::Normal, egui_phosphor::regular::ARROW_CLOCKWISE).small().tip("Turn clockwise").show(ui).clicked() {
                    turn = Some(1);
                }
                if Btn::icon(Kind::Normal, egui_phosphor::regular::ARROW_COUNTER_CLOCKWISE).small().tip("Turn anticlockwise").show(ui).clicked() {
                    turn = Some(-1);
                }
                ui.add_space(4.0);
                small(ui, &format!("{}°", u32::from(turns % 4) * 90));
            });
            l.row(Row::new("Mirror").note("Left and right swapped"), |ui| {
                ui_kit::switch(ui, &mut m);
            });
            l.row(Row::new("Upside down"), |ui| {
                ui_kit::switch(ui, &mut u);
            });
        });
        // Its first place and size again, each on its own.
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let size_tip = match layer {
                    Layer::Screen if self.video_shaped() => "Back to the size the video gives it, where it is",
                    Layer::Screen => "As big as the video again, where it is",
                    Layer::Webcam(_) => "A quarter of the video wide again, unstretched, where it is",
                    Layer::Visual(_) => "Its own size again, unstretched (a pixel of it a pixel of the video), where it is",
                };
                reset_size = Btn::ghost("Reset size").small().enabled(can_size).tip(size_tip).show(ui).clicked();
                let place_tip = if matches!(layer, Layer::Webcam(_)) { "Back to the bottom-right corner, its size kept" } else { "Back in the middle of the video, its size kept" };
                reset_place = Btn::ghost("Reset position").small().enabled(can_place).tip(place_tip).show(ui).clicked();
            });
        });
        if reset_place {
            self.layer_reset_position(layer, aspect);
        }
        if reset_size {
            self.layer_reset_size(layer, aspect);
        }
        if let Some(px) = moved {
            self.set_layer_rect(layer, from_pixels(px, (vw, vh)), aspect);
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

    /// A source put in the box `r` (fractions of a video of shape
    /// `aspect`): the screen and boxed sources placed so; a webcam too, as
    /// much more stretched as the box is another shape, and kept in the
    /// video.
    pub(crate) fn set_layer_rect(&mut self, layer: Layer, r: Rect, aspect: f32) {
        match layer {
            Layer::Screen => self.place_or_frame(r, aspect),
            Layer::Visual(key) => self.place_visual(key, r, aspect),
            Layer::Webcam(key) => {
                if let Some(cam) = self.settings.webcam_mut(key) {
                    let p: capture::webcam::Placement = cam.placement.into();
                    let stretch = p.stretch * (r.width() / r.height().max(1e-6)) / (p.w / p.h.max(1e-6));
                    let p = crate::webcam_ui::contain(capture::webcam::Placement { x: r.min.x, y: r.min.y, w: r.width(), h: r.height(), stretch, ..p });
                    cam.placement = crate::settings::PlacementCfg::laid_out(p, aspect);
                }
            }
        }
    }

    /// What of a source's picture shows: how much is cut off each side, in
    /// its own pixels; all of it again. Alt and a handle on the preview do
    /// the same.
    pub(crate) fn crop_section(&mut self, ui: &mut egui::Ui, layer: Layer, aspect: f32) {
        let pal = theme::pal(ui);
        ui_kit::section_head(ui, "Crop", false, |_| {});
        // Its picture's size, and how much is cut off each side.
        let now: Option<([u32; 4], (u32, u32))> = match layer {
            Layer::Screen => self.crop_whole().map(|size| {
                let rect = self.settings.screen_crop.filter(|c| !c.is_none()).map_or([0, 0, size.0 & !1, size.1 & !1], |c| capture::Crop::from(c).rect(size.0, size.1));
                (sides_of(rect, size), size)
            }),
            Layer::Visual(key) => self.visual_crop_rect(key).map(|(rect, size)| (sides_of(rect, size), size)),
            Layer::Webcam(key) => self.camera_size(key).zip(self.webcam_placement(key)).map(|((cw, ch), p)| {
                let px = |f: f32, n: u32| (f * n as f32).round() as u32;
                ([px(p.crop[0], cw), px(p.crop[1], ch), px(p.crop[2], cw), px(p.crop[3], ch)], (cw, ch))
            }),
        };
        let Some((sides, (sw, sh))) = now else {
            let why = if matches!(layer, Layer::Webcam(_)) { "Its crop shows here once the camera's on." } else { "Its crop shows here once its size is known." };
            ui.label(theme::text(why, 12.5, W::Regular).color(pal.text3));
            return;
        };
        let [cl, ct, cr, cb] = sides;
        let mut set: Option<[u32; 4]> = None;
        let small = |ui: &mut egui::Ui, t: &str| ui.label(theme::text(t, 12.5, W::Regular).color(pal.text3));
        const BOX: f32 = 60.0;
        let least = 16u32;
        let id = format!("crop_{layer:?}");
        ui_kit::list(ui, ("crop", layer), |l| {
            l.row(Row::new("Left, right"), |ui| {
                ui.allocate_ui_with_layout(vec2(BOX * 2.0 + 6.0 + 16.0 + 6.0 * 3.0, ui_kit::CONTROL_H), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if let Some(v) = ui_kit::number_box(ui, egui::Id::new((&id, "l")), cl, 0..=sw.saturating_sub(cr + least), BOX) {
                        set = Some([v, ct, cr, cb]);
                    }
                    small(ui, ",");
                    if let Some(v) = ui_kit::number_box(ui, egui::Id::new((&id, "r")), cr, 0..=sw.saturating_sub(cl + least), BOX) {
                        set = Some([cl, ct, v, cb]);
                    }
                    small(ui, "px");
                });
            });
            l.row(Row::new("Top, bottom"), |ui| {
                ui.allocate_ui_with_layout(vec2(BOX * 2.0 + 6.0 + 16.0 + 6.0 * 3.0, ui_kit::CONTROL_H), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if let Some(v) = ui_kit::number_box(ui, egui::Id::new((&id, "t")), ct, 0..=sh.saturating_sub(cb + least), BOX) {
                        set = Some([cl, v, cr, cb]);
                    }
                    small(ui, ",");
                    if let Some(v) = ui_kit::number_box(ui, egui::Id::new((&id, "b")), cb, 0..=sh.saturating_sub(ct + least), BOX) {
                        set = Some([cl, ct, cr, v]);
                    }
                    small(ui, "px");
                });
            });
        });
        let cropped = sides.iter().any(|s| *s > 0);
        let mut whole_page = false;
        let mut reset = false;
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            // On the preview, the dialog closed: its handles crop till
            // Done (as Alt and a handle do, now and then).
            let alt = crate::shortcuts::alt_name();
            let b = Btn::normal("Crop on the preview").with_icon(egui_phosphor::regular::CROP).small();
            if b.tip(format!("Drag its handles to cut its edges off, till Done or Enter ({alt} and a handle do it too)")).show(ui).clicked() {
                self.canvas.crop_mode = Some(layer);
                self.sources_view.editing = None;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                reset = Btn::ghost("Reset").small().enabled(cropped).tip("All of its picture again").show(ui).clicked();
                // The screen's and a webcam's crop also have a page of their
                // own: all of the picture, the crop dragged on it.
                if matches!(layer, Layer::Screen | Layer::Webcam(_)) {
                    whole_page = Btn::ghost("Whole picture…").small().tip("Crop on all of its picture, in a page of its own").show(ui).clicked();
                }
            });
        });
        if reset {
            self.layer_uncrop(layer, aspect);
        }
        if whole_page {
            self.sources_view.editing = None;
            match layer {
                Layer::Screen => self.sources_view.crop_open = true,
                Layer::Webcam(key) => {
                    if let Some(w) = self.settings.webcam(key) {
                        self.webcam_view.crop = Some(crate::webcam_ui::CameraPanel::cropping(&w.id));
                    }
                }
                Layer::Visual(_) => {}
            }
        }
        let Some(s) = set else { return };
        match layer {
            Layer::Screen => self.settings.screen_crop = crop_of(s, (sw, sh), least),
            Layer::Visual(key) => {
                if let Some(v) = self.settings.visual_mut(key) {
                    v.crop = crop_of(s, (sw, sh), least);
                }
            }
            Layer::Webcam(key) => {
                let f = |v: u32, n: u32| v as f32 / n.max(1) as f32;
                self.webcam_set_crop(key, [f(s[0], sw), f(s[1], sh), f(s[2], sw), f(s[3], sh)]);
            }
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

    /// A crop as its sides and back: nothing cut off is no crop, and what's
    /// kept is never less than the least, on even pixels.
    #[test]
    fn crop_sides() {
        let size = (1920, 1080);
        assert_eq!(sides_of([100, 50, 1600, 900], size), [100, 50, 220, 130]);
        assert_eq!(crop_of([100, 50, 220, 130], size, 16), Some(ScreenCrop { w: 1600, h: 900, at: Some([100, 50]) }));
        assert_eq!(crop_of([0, 0, 0, 0], size, 16), None);
        let tiny = crop_of([1900, 0, 100, 0], size, 16).unwrap();
        assert!(tiny.w >= 16 && tiny.w.is_multiple_of(2), "{tiny:?}");
    }
}
