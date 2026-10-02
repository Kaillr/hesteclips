//! The webcam on the Sources page: picking a camera, and placing it on the live
//! preview the way OBS does — drag to move, corner handles to resize (keeping
//! its shape; Shift to stretch), edge handles to stretch one side, Alt with any
//! handle to crop. No rotation. Moves snap to the frame's edges and centre
//! (Ctrl to place freely).
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

    fn is_corner(self) -> bool {
        matches!(self, Handle::NE | Handle::NW | Handle::SE | Handle::SW)
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
    /// Corners stretch instead of keeping the shape (Shift).
    pub free: bool,
    /// Snap to the frame's edges and centre (unless Ctrl).
    pub snap: bool,
}

/// The placement after dragging `handle` by `(dx, dy)` (fractions of the
/// frame) from where it was at `start`.
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
    // Edges as they'd be, each side following the pointer.
    let (mut x0, mut y0, mut x1, mut y1) = (start.x, start.y, start.x + start.w, start.y + start.h);
    if l {
        x0 = (start.x + dx).min(x1 - MIN_SIZE);
    }
    if r {
        x1 = (x1 + dx).max(x0 + MIN_SIZE);
    }
    if t {
        y0 = (start.y + dy).min(y1 - MIN_SIZE);
    }
    if b {
        y1 = (y1 + dy).max(y0 + MIN_SIZE);
    }
    if mods.snap && !handle.is_corner() {
        if l {
            x0 = snap_to(x0, &[0.0, 1.0]).min(x1 - MIN_SIZE);
        }
        if r {
            x1 = snap_to(x1, &[0.0, 1.0]).max(x0 + MIN_SIZE);
        }
        if t {
            y0 = snap_to(y0, &[0.0, 1.0]).min(y1 - MIN_SIZE);
        }
        if b {
            y1 = snap_to(y1, &[0.0, 1.0]).max(y0 + MIN_SIZE);
        }
    }
    if handle.is_corner() && !mods.free {
        // Keep its shape: scale by whichever side moved more, from the opposite corner.
        let kx = (x1 - x0) / start.w;
        let ky = (y1 - y0) / start.h;
        let k = if (kx - 1.0).abs() >= (ky - 1.0).abs() { kx } else { ky };
        let k = k.max(MIN_SIZE / start.w.min(start.h));
        let (w, h) = (start.w * k, start.h * k);
        p.x = if l { start.x + start.w - w } else { start.x };
        p.y = if t { start.y + start.h - h } else { start.y };
        p.w = w;
        p.h = h;
    } else {
        p.x = x0;
        p.y = y0;
        p.w = x1 - x0;
        p.h = y1 - y0;
    }
    p
}

/// Crop the sides in `sides` as they're dragged: the picture keeps its scale,
/// the box shrinks (or grows back) with the crop, like OBS's Alt-drag.
fn crop(start: Placement, [l, t, r, b]: [bool; 4], dx: f32, dy: f32) -> Placement {
    let mut p = start;
    let [cl, ct, cr, cb] = start.crop;
    // Frame fraction per camera fraction, across and down.
    let sx = start.w / (1.0 - cl - cr).max(0.01);
    let sy = start.h / (1.0 - ct - cb).max(0.01);
    let most = 0.95;
    if l {
        let c = (cl + dx / sx).clamp(0.0, (most - cr).max(0.0));
        let d = (c - cl) * sx;
        p.crop[0] = c;
        p.x = start.x + d;
        p.w = start.w - d;
    }
    if r {
        let c = (cr - dx / sx).clamp(0.0, (most - cl).max(0.0));
        p.crop[2] = c;
        p.w = p.w + (cr - c) * sx;
    }
    if t {
        let c = (ct + dy / sy).clamp(0.0, (most - cb).max(0.0));
        let d = (c - ct) * sy;
        p.crop[1] = c;
        p.y = start.y + d;
        p.h = start.h - d;
    }
    if b {
        let c = (cb - dy / sy).clamp(0.0, (most - ct).max(0.0));
        p.crop[3] = c;
        p.h = p.h + (cb - c) * sy;
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

fn snap_to(v: f32, targets: &[f32]) -> f32 {
    targets.iter().copied().find(|t| (t - v).abs() < SNAP).unwrap_or(v)
}

impl App {
    /// The webcam controls under the preview: add one, pick the camera, reset
    /// it, remove it.
    pub(crate) fn webcam_row(&mut self, ui: &mut egui::Ui, frame: Option<&capture::preview::PreviewFrame>) {
        let idle = self.rec_state == crate::RecState::Idle;
        let status = capture::webcam::status();
        let frame_aspect = frame.map_or(16.0 / 9.0, |f| f.width as f32 / f.height.max(1) as f32);

        // Just added: once the camera's shape is known, give it a box of that shape.
        if self.webcam_view.fit_pending {
            if let (Status::Live { width, height }, Some(w)) = (&status, self.settings.webcam.as_mut()) {
                w.placement = Placement::default_for(*width as f32 / (*height).max(1) as f32, frame_aspect).into();
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
        ui.label(
            RichText::new(
                "Drag it on the preview to move it. Drag a corner to resize, an edge to stretch. \
                 Hold Alt to crop, Shift to resize freely, Ctrl to stop snapping.",
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

        if response.drag_started() {
            if let Some(h) = response.interact_pointer_pos().and_then(hit) {
                self.webcam_view.drag = Some((h, response.interact_pointer_pos().unwrap(), p));
            }
        }
        if let Some((handle, from, start)) = self.webcam_view.drag {
            if response.dragged() {
                if let Some(now) = response.interact_pointer_pos() {
                    let mods = Mods { crop: modifiers.alt, free: modifiers.shift, snap: !modifiers.ctrl };
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
        let active = self.webcam_view.drag.is_some() || pointer.is_some_and(|at| frame_rect.contains(at));
        painter.rect_stroke(bx, 0.0, Stroke::new(if active { 2.0 } else { 1.0 }, color.gamma_multiply(if active { 1.0 } else { 0.6 })), egui::StrokeKind::Middle);
        if active {
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
        Placement { x, y, w, h, crop: [0.0; 4] }
    }

    const FREE: Mods = Mods { crop: false, free: false, snap: false };

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn move_and_snap() {
        let p = drag(at(0.5, 0.5, 0.2, 0.2), Handle::Move, 0.1, -0.1, FREE);
        assert!(close(p.x, 0.6) && close(p.y, 0.4) && close(p.w, 0.2));
        // Right edge lands 0.005 from the frame's edge: snaps onto it.
        let p = drag(at(0.5, 0.5, 0.2, 0.2), Handle::Move, 0.295, 0.0, Mods { snap: true, ..FREE });
        assert!(close(p.x + p.w, 1.0));
        // Centre near the middle: snaps to it.
        let p = drag(at(0.0, 0.0, 0.2, 0.2), Handle::Move, 0.405, 0.0, Mods { snap: true, ..FREE });
        assert!(close(p.x + p.w / 2.0, 0.5));
    }

    #[test]
    fn corners_keep_the_shape_unless_free() {
        let start = at(0.5, 0.5, 0.2, 0.1);
        let p = drag(start, Handle::SE, 0.1, 0.0, FREE);
        assert!(close(p.w, 0.3) && close(p.h, 0.15) && close(p.x, 0.5) && close(p.y, 0.5));
        // From the top-left corner: the bottom-right stays put.
        let p = drag(start, Handle::NW, -0.1, 0.0, FREE);
        assert!(close(p.x + p.w, 0.7) && close(p.y + p.h, 0.6) && close(p.w / p.h, 2.0));
        let p = drag(start, Handle::SE, 0.1, 0.0, Mods { free: true, ..FREE });
        assert!(close(p.w, 0.3) && close(p.h, 0.1));
    }

    #[test]
    fn edges_stretch_one_side() {
        let p = drag(at(0.5, 0.5, 0.2, 0.1), Handle::E, 0.1, 0.3, FREE);
        assert!(close(p.w, 0.3) && close(p.h, 0.1) && close(p.y, 0.5));
        // Never smaller than the minimum.
        let p = drag(at(0.5, 0.5, 0.2, 0.1), Handle::W, 0.5, 0.0, FREE);
        assert!(close(p.w, MIN_SIZE));
    }

    #[test]
    fn crop_keeps_the_scale() {
        // Box 0.2 wide shows the whole camera: frame fraction 0.2 per camera width.
        let start = at(0.5, 0.5, 0.2, 0.2);
        let crop = Mods { crop: true, ..FREE };
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
}
