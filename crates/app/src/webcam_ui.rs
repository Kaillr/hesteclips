//! The webcam on the Sources page: picking a camera and its format, its own
//! settings window, and placing it on the live preview the way OBS does — drag
//! to move, any handle to resize (always keeping its shape: it's never
//! stretched), past the opposite side to flip it, Alt with a handle to crop.
//! No rotation. Moves snap to the frame's edges and centre (Ctrl to place
//! freely).
//!
//! Placement is fractions of the frame (see `capture::webcam::Placement`); it's
//! shared with the capture, so what you drag is what's recorded, live.

use capture::webcam::{Placement, Status};
use egui::{Color32, CursorIcon, Pos2, Rect, RichText, Stroke, vec2};

use crate::App;
use crate::library::ACCENT;
use crate::settings::WebcamCfg;

/// Smallest the webcam's box gets, as a fraction of the frame.
const MIN_SIZE: f32 = 0.03;
/// Snap distance, as a fraction of the frame.
const SNAP: f32 = 0.012;
/// Handle size, in points.
const HANDLE: f32 = 8.0;
/// OBS-ish crop colour.
const CROP: Color32 = Color32::from_rgb(80, 220, 120);

#[derive(Default)]
pub(crate) struct WebcamView {
    /// Cameras for the picker, refreshed when it opens.
    cameras: Vec<capture::Device>,
    /// A drag in progress: what's held, where it started, and the placement then.
    drag: Option<(Handle, Pos2, Placement)>,
    /// Just added: size its box to the camera's shape once that's known.
    fit_pending: bool,
}

/// What part of the webcam's box is being dragged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Handle {
    Move,
    N,
    S,
    E,
    W,
    NE,
    NW,
    SE,
    SW,
}

impl Handle {
    const ALL: [Handle; 8] = [Handle::NW, Handle::N, Handle::NE, Handle::E, Handle::SE, Handle::S, Handle::SW, Handle::W];

    /// Which sides it moves: left, top, right, bottom.
    fn sides(self) -> [bool; 4] {
        match self {
            Handle::Move => [false; 4],
            Handle::N => [false, true, false, false],
            Handle::S => [false, false, false, true],
            Handle::E => [false, false, true, false],
            Handle::W => [true, false, false, false],
            Handle::NE => [false, true, true, false],
            Handle::NW => [true, true, false, false],
            Handle::SE => [false, false, true, true],
            Handle::SW => [true, false, false, true],
        }
    }

    /// Where it sits on a box.
    fn pos(self, r: Rect) -> Pos2 {
        let [l, t, rr, b] = self.sides();
        let x = if l { r.left() } else if rr { r.right() } else { r.center().x };
        let y = if t { r.top() } else if b { r.bottom() } else { r.center().y };
        Pos2::new(x, y)
    }

    fn cursor(self) -> CursorIcon {
        match self {
            Handle::Move => CursorIcon::Grab,
            Handle::N | Handle::S => CursorIcon::ResizeVertical,
            Handle::E | Handle::W => CursorIcon::ResizeHorizontal,
            Handle::NE | Handle::SW => CursorIcon::ResizeNeSw,
            Handle::NW | Handle::SE => CursorIcon::ResizeNwSe,
        }
    }
}

/// How a drag behaves.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Mods {
    /// Crop instead of resizing (Alt).
    pub crop: bool,
    /// Snap to the frame's edges and centre (unless Ctrl).
    pub snap: bool,
}

/// The placement after dragging `handle` by `(dx, dy)` (fractions of the
/// frame) from where it was at `start`.
///
/// Resizing never stretches: every handle scales the whole picture, from the
/// opposite corner or edge (an edge handle keeps the other axis centred).
/// Dragging past that opposite side flips the picture on that axis.
pub(crate) fn drag(start: Placement, handle: Handle, dx: f32, dy: f32, mods: Mods) -> Placement {
    let mut p = start;
    if handle == Handle::Move {
        p.x = start.x + dx;
        p.y = start.y + dy;
        if mods.snap {
            p.x += snap_offset(&[p.x, p.x + p.w], p.x + p.w / 2.0);
            p.y += snap_offset(&[p.y, p.y + p.h], p.y + p.h / 2.0);
        }
        return p;
    }
    let [l, t, r, b] = handle.sides();
    if mods.crop {
        return crop(start, [l, t, r, b], dx, dy);
    }
    // Per axis: the fixed side (anchor), and how far the dragged side is from
    // it, signed, before and after. A sign change means it crossed: a flip.
    let axis = |near: bool, far: bool, pos: f32, size: f32, d: f32| -> Option<(f32, f32)> {
        if !near && !far {
            return None;
        }
        let (anchor, moving) = if near { (pos + size, pos) } else { (pos, pos + size) };
        let before = moving - anchor;
        Some((anchor, (before + d) / before))
    };
    let kx = axis(l, r, start.x, start.w, dx);
    let ky = axis(t, b, start.y, start.h, dy);
    // One uniform scale: the axis that moved more decides (a corner), or the
    // only one dragged (an edge).
    let k = match (kx, ky) {
        (Some((_, a)), Some((_, b))) => {
            if (a.abs() - 1.0).abs() >= (b.abs() - 1.0).abs() { a.abs() } else { b.abs() }
        }
        (Some((_, a)), None) | (None, Some((_, a))) => a.abs(),
        (None, None) => 1.0,
    };
    let k = k.max(MIN_SIZE / start.w.min(start.h));
    let (w, h) = (start.w * k, start.h * k);
    // Place each axis: from its anchor, on the side the drag ended up on; an
    // axis that wasn't dragged stays centred.
    let place = |near: bool, ka: Option<(f32, f32)>, pos: f32, size: f32, new: f32| -> (f32, bool) {
        match ka {
            Some((anchor, k)) => {
                let flipped = k < 0.0;
                // The dragged side was before the anchor (near) or after it.
                let before = near != flipped;
                (if before { anchor - new } else { anchor }, flipped)
            }
            None => (pos + size / 2.0 - new / 2.0, false),
        }
    };
    let (x, fx) = place(l, kx, start.x, start.w, w);
    let (y, fy) = place(t, ky, start.y, start.h, h);
    p.x = x;
    p.y = y;
    p.w = w;
    p.h = h;
    p.flip_h = start.flip_h ^ fx;
    p.flip_v = start.flip_v ^ fy;
    p
}

/// Crop the sides in `sides` (as seen: a flipped picture's left side is the
/// camera's right) as they're dragged: the picture keeps its scale, the box
/// shrinks (or grows back) with the crop, like OBS's Alt-drag.
fn crop(start: Placement, [l, t, r, b]: [bool; 4], dx: f32, dy: f32) -> Placement {
    let mut p = start;
    let c = start.crop;
    // Which camera side each visible side is.
    let (left, right) = if start.flip_h { (2, 0) } else { (0, 2) };
    let (top, bottom) = if start.flip_v { (3, 1) } else { (1, 3) };
    // Frame fraction per camera fraction, across and down.
    let sx = start.w / (1.0 - c[0] - c[2]).max(0.01);
    let sy = start.h / (1.0 - c[1] - c[3]).max(0.01);
    let most = 0.95;
    if l {
        let new = (c[left] + dx / sx).clamp(0.0, (most - c[right]).max(0.0));
        let d = (new - c[left]) * sx;
        p.crop[left] = new;
        p.x = start.x + d;
        p.w = start.w - d;
    }
    if r {
        let new = (c[right] - dx / sx).clamp(0.0, (most - c[left]).max(0.0));
        p.crop[right] = new;
        p.w += (c[right] - new) * sx;
    }
    if t {
        let new = (c[top] + dy / sy).clamp(0.0, (most - c[bottom]).max(0.0));
        let d = (new - c[top]) * sy;
        p.crop[top] = new;
        p.y = start.y + d;
        p.h = start.h - d;
    }
    if b {
        let new = (c[bottom] - dy / sy).clamp(0.0, (most - c[top]).max(0.0));
        p.crop[bottom] = new;
        p.h += (c[bottom] - new) * sy;
    }
    p
}

/// How far to move so the nearest of `edges` (to 0 or 1) or `center` (to 0.5)
/// lands on it, if one is close enough.
fn snap_offset(edges: &[f32], center: f32) -> f32 {
    let mut best: Option<f32> = None;
    let mut consider = |d: f32| {
        if d.abs() < SNAP && best.is_none_or(|b| d.abs() < b.abs()) {
            best = Some(d);
        }
    };
    for &e in edges {
        consider(0.0 - e);
        consider(1.0 - e);
    }
    consider(0.5 - center);
    best.unwrap_or(0.0)
}

impl App {
    /// The webcam controls under the preview: add one, pick the camera, reset
    /// it, remove it.
    pub(crate) fn webcam_row(&mut self, ui: &mut egui::Ui, frame: Option<&capture::preview::PreviewFrame>) {
        let idle = self.rec_state == crate::RecState::Idle;
        let status = capture::webcam::status();
        let frame_aspect = frame.map_or(16.0 / 9.0, |f| f.width as f32 / f.height.max(1) as f32);

        // Just added, or a new camera or format: once its shape is known, give
        // the box that shape (same width, same top-left; crop kept).
        if self.webcam_view.fit_pending {
            if let (Status::Live { width, height }, Some(w)) = (&status, self.settings.webcam.as_mut()) {
                let camera_aspect = *width as f32 / (*height).max(1) as f32;
                let p = &mut w.placement;
                let visible = (1.0 - p.crop[0] - p.crop[2]).max(0.05) / (1.0 - p.crop[1] - p.crop[3]).max(0.05);
                p.h = p.w * frame_aspect / (camera_aspect * visible);
                self.webcam_view.fit_pending = false;
            }
        }

        let Some(cam) = self.settings.webcam.clone() else {
            ui.horizontal(|ui| {
                ui.add_enabled_ui(idle, |ui| {
                    let add = ui.menu_button("📷 Add webcam", |ui| {
                        ui.set_min_width(240.0);
                        if self.webcam_view.cameras.is_empty() {
                            ui.weak("No cameras found.");
                        }
                        for c in self.webcam_view.cameras.clone() {
                            if ui.button(&c.name).clicked() {
                                self.settings.webcam = Some(WebcamCfg {
                                    id: c.id,
                                    name: c.name,
                                    format: None,
                                    placement: Placement::default_for(16.0 / 9.0, frame_aspect).into(),
                                });
                                self.webcam_view.fit_pending = true;
                                ui.close();
                            }
                        }
                    });
                    if add.response.clicked() {
                        self.webcam_view.cameras = capture::webcam::list_cameras();
                    }
                    add.response.on_disabled_hover_text("Stop capturing to add a webcam.");
                });
                ui.weak("Put your camera in your clips.");
            });
            return;
        };

        ui.horizontal(|ui| {
            ui.label("📷 Webcam");
            ui.add_enabled_ui(idle, |ui| {
                let combo = egui::ComboBox::from_id_salt("webcam_device").selected_text(&cam.name).truncate().show_ui(ui, |ui| {
                    for c in self.webcam_view.cameras.clone() {
                        if ui.selectable_label(c.id == cam.id, &c.name).clicked() && c.id != cam.id {
                            if let Some(w) = self.settings.webcam.as_mut() {
                                w.id = c.id;
                                w.name = c.name;
                                w.format = None; // that camera's own formats
                            }
                            self.webcam_view.fit_pending = true;
                        }
                    }
                });
                if combo.response.clicked() {
                    self.webcam_view.cameras = capture::webcam::list_cameras();
                }
                combo.response.on_disabled_hover_text("Stop capturing to switch cameras.");
            });
            if ui.button("Reset").on_hover_text("Back to the bottom-right corner, uncropped").clicked() {
                let camera_aspect = match status {
                    Status::Live { width, height } => width as f32 / height.max(1) as f32,
                    _ => 16.0 / 9.0,
                };
                if let Some(w) = self.settings.webcam.as_mut() {
                    w.placement = Placement::default_for(camera_aspect, frame_aspect).into();
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_enabled_ui(idle, |ui| {
                    if ui
                        .small_button("✕")
                        .on_hover_text("Remove the webcam")
                        .on_disabled_hover_text("Stop capturing to remove the webcam.")
                        .clicked()
                    {
                        self.settings.webcam = None;
                    }
                });
            });
        });
        match &status {
            Status::Opening => {
                ui.weak("Opening the camera…");
            }
            Status::Unavailable(why) => {
                ui.colored_label(ui.visuals().warn_fg_color, format!("The webcam isn't working: {why}"));
            }
            _ => {}
        }
        // The camera's own settings: its format here, the rest in its own window.
        ui.horizontal(|ui| {
            ui.label("Format");
            let formats = capture::webcam::formats(&cam.id);
            let current = cam.format.map(capture::webcam::Format::from);
            let label = current.map_or_else(|| "Automatic".to_owned(), |f| f.label());
            ui.add_enabled_ui(idle, |ui| {
                egui::ComboBox::from_id_salt("webcam_format")
                    .selected_text(label)
                    .show_ui(ui, |ui| {
                        if ui.selectable_label(current.is_none(), "Automatic").on_hover_text("The sharpest picture up to 1080p at 30 fps or more").clicked() {
                            if let Some(w) = self.settings.webcam.as_mut() {
                                w.format = None;
                            }
                            self.webcam_view.fit_pending = true;
                        }
                        if formats.is_empty() {
                            ui.weak("The camera's formats show once it's open.");
                        }
                        for f in formats {
                            if ui.selectable_label(current == Some(f), f.label()).clicked() && current != Some(f) {
                                if let Some(w) = self.settings.webcam.as_mut() {
                                    w.format = Some(f.into());
                                }
                                self.webcam_view.fit_pending = true;
                            }
                        }
                    })
                    .response
                    .on_disabled_hover_text("Stop capturing to change the camera's format.");
            });
            if ui
                .button("⚙ Camera settings…")
                .on_hover_text("The camera's own settings: exposure, focus, white balance and more. They apply to the camera right away, in every app.")
                .clicked()
            {
                capture::webcam::open_settings(&cam.id, &cam.name);
            }
        });
        ui.label(
            RichText::new(
                "Drag it on the preview to move it. Drag a corner or edge to resize it, and past the opposite side to flip it. \
                 Hold Alt to crop, Ctrl to stop snapping.",
            )
            .size(12.0)
            .weak(),
        );
    }

    /// The webcam's box on the preview, its handles, and dragging them.
    pub(crate) fn webcam_on_preview(&mut self, ui: &mut egui::Ui, frame_rect: Rect, response: &egui::Response) {
        let Some(cam) = self.settings.webcam.as_ref() else { return };
        let p: Placement = cam.placement.into();
        let to_screen = |x: f32, y: f32| Pos2::new(frame_rect.left() + x * frame_rect.width(), frame_rect.top() + y * frame_rect.height());
        let bx = Rect::from_min_max(to_screen(p.x, p.y), to_screen(p.x + p.w, p.y + p.h));
        let modifiers = ui.input(|i| i.modifiers);
        let pointer = ui.input(|i| i.pointer.hover_pos());

        // What's under the pointer: a handle, else the box itself.
        let hit = |at: Pos2| -> Option<Handle> {
            Handle::ALL
                .into_iter()
                .find(|h| h.pos(bx).distance(at) <= HANDLE)
                .or_else(|| bx.expand(2.0).contains(at).then_some(Handle::Move))
        };

        // A drag only starts once the pointer has moved a little, so what was
        // grabbed is judged by where the button went down, not where it is now.
        if response.drag_started() {
            let origin = ui.input(|i| i.pointer.press_origin()).or(response.interact_pointer_pos());
            if let Some((h, at)) = origin.and_then(|at| hit(at).map(|h| (h, at))) {
                self.webcam_view.drag = Some((h, at, p));
            }
        }
        if let Some((handle, from, start)) = self.webcam_view.drag {
            if response.dragged() {
                if let Some(now) = response.interact_pointer_pos() {
                    let mods = Mods { crop: modifiers.alt, snap: !modifiers.ctrl };
                    let (dx, dy) = ((now.x - from.x) / frame_rect.width(), (now.y - from.y) / frame_rect.height());
                    let moved = drag(start, handle, dx, dy, mods);
                    if let Some(w) = self.settings.webcam.as_mut() {
                        w.placement = moved.into();
                    }
                }
                ui.ctx().set_cursor_icon(if handle == Handle::Move { CursorIcon::Grabbing } else { handle.cursor() });
            }
            if response.drag_stopped() || !response.dragged() && !ui.input(|i| i.pointer.any_down()) {
                self.webcam_view.drag = None;
            }
        } else if let Some(h) = pointer.filter(|at| frame_rect.contains(*at)).and_then(hit) {
            ui.ctx().set_cursor_icon(h.cursor());
        }

        // The box and its handles, over the picture.
        let p: Placement = self.settings.webcam.as_ref().map_or(p, |c| c.placement.into());
        let bx = Rect::from_min_max(to_screen(p.x, p.y), to_screen(p.x + p.w, p.y + p.h));
        let painter = ui.painter_at(frame_rect);
        let cropping = modifiers.alt;
        let color = if cropping { CROP } else { ACCENT };
        // Outlined only while it's held or the pointer is over it.
        let bx_hit = |at: Pos2| Handle::ALL.iter().any(|h| h.pos(bx).distance(at) <= HANDLE) || bx.expand(2.0).contains(at);
        let active = self.webcam_view.drag.is_some() || pointer.is_some_and(|at| frame_rect.contains(at) && bx_hit(at));
        if active {
            painter.rect_stroke(bx, 0.0, Stroke::new(2.0, color), egui::StrokeKind::Middle);
            for h in Handle::ALL {
                let c = h.pos(bx);
                let r = Rect::from_center_size(c, vec2(HANDLE, HANDLE));
                painter.rect_filled(r, 1.0, Color32::WHITE);
                painter.rect_stroke(r, 1.0, Stroke::new(1.0, color), egui::StrokeKind::Inside);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(x: f32, y: f32, w: f32, h: f32) -> Placement {
        Placement { x, y, w, h, crop: [0.0; 4], flip_h: false, flip_v: false }
    }

    const PLAIN: Mods = Mods { crop: false, snap: false };

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn move_and_snap() {
        let p = drag(at(0.5, 0.5, 0.2, 0.2), Handle::Move, 0.1, -0.1, PLAIN);
        assert!(close(p.x, 0.6) && close(p.y, 0.4) && close(p.w, 0.2));
        // Right edge lands 0.005 from the frame's edge: snaps onto it.
        let p = drag(at(0.5, 0.5, 0.2, 0.2), Handle::Move, 0.295, 0.0, Mods { snap: true, ..PLAIN });
        assert!(close(p.x + p.w, 1.0));
        // Centre near the middle: snaps to it.
        let p = drag(at(0.0, 0.0, 0.2, 0.2), Handle::Move, 0.405, 0.0, Mods { snap: true, ..PLAIN });
        assert!(close(p.x + p.w / 2.0, 0.5));
    }

    #[test]
    fn every_handle_keeps_the_shape() {
        let start = at(0.5, 0.5, 0.2, 0.1);
        let p = drag(start, Handle::SE, 0.1, 0.0, PLAIN);
        assert!(close(p.w, 0.3) && close(p.h, 0.15) && close(p.x, 0.5) && close(p.y, 0.5));
        // From the top-left corner: the bottom-right stays put.
        let p = drag(start, Handle::NW, -0.1, 0.0, PLAIN);
        assert!(close(p.x + p.w, 0.7) && close(p.y + p.h, 0.6) && close(p.w / p.h, 2.0));
        // An edge scales the whole picture too, from the opposite edge, centred
        // on the other axis.
        let p = drag(start, Handle::E, 0.1, 0.3, PLAIN);
        assert!(close(p.w, 0.3) && close(p.h, 0.15) && close(p.x, 0.5) && close(p.y + p.h / 2.0, 0.55));
        let p = drag(start, Handle::N, 0.0, -0.05, PLAIN);
        assert!(close(p.h, 0.15) && close(p.w, 0.3) && close(p.y + p.h, 0.6) && close(p.x + p.w / 2.0, 0.6));
        assert!(!p.flip_h && !p.flip_v);
    }

    #[test]
    fn dragging_past_the_other_side_flips() {
        let start = at(0.5, 0.5, 0.2, 0.1);
        // Right edge dragged 0.3 left: past the left edge, the same size, mirrored.
        let p = drag(start, Handle::E, -0.4, 0.0, PLAIN);
        assert!(p.flip_h && !p.flip_v);
        assert!(close(p.w, 0.2) && close(p.x + p.w, 0.5));
        // Bottom-right corner up past the top only: flipped vertically.
        let p = drag(start, Handle::SE, 0.0, -0.2, PLAIN);
        assert!(p.flip_v && !p.flip_h && close(p.y + p.h, 0.5));
        // Past both: both.
        let p = drag(start, Handle::SE, -0.4, -0.2, PLAIN);
        assert!(p.flip_h && p.flip_v);
        // Flipping back undoes it.
        let back = drag(p, Handle::NW, 0.4, 0.2, PLAIN);
        assert!(!back.flip_h && !back.flip_v);
    }

    #[test]
    fn crop_keeps_the_scale() {
        // Box 0.2 wide shows the whole camera: frame fraction 0.2 per camera width.
        let start = at(0.5, 0.5, 0.2, 0.2);
        let crop = Mods { crop: true, ..PLAIN };
        let p = drag(start, Handle::W, 0.05, 0.0, crop);
        assert!(close(p.crop[0], 0.25) && close(p.x, 0.55) && close(p.w, 0.15));
        // Dragging the right edge outward past the uncropped size doesn't uncrop
        // beyond the picture.
        let p = drag(start, Handle::E, 0.1, 0.0, crop);
        assert!(close(p.crop[2], 0.0) && close(p.w, 0.2));
        // Crop the right, then pull it back: the box grows back.
        let cropped = drag(start, Handle::E, -0.05, 0.0, crop);
        assert!(close(cropped.crop[2], 0.25) && close(cropped.w, 0.15));
        let back = drag(cropped, Handle::E, 0.05, 0.0, crop);
        assert!(close(back.crop[2], 0.0) && close(back.w, 0.2));
    }

    #[test]
    fn crop_follows_a_flipped_picture() {
        // Mirrored: the visible left side is the camera's right side.
        let start = Placement { flip_h: true, ..at(0.5, 0.5, 0.2, 0.2) };
        let p = drag(start, Handle::W, 0.05, 0.0, Mods { crop: true, ..PLAIN });
        assert!(close(p.crop[2], 0.25) && close(p.crop[0], 0.0) && close(p.x, 0.55));
    }
}
