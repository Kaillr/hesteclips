//! The webcam on the Sources page: picking a camera, and how it's placed on
//! the live preview the way OBS does (`canvas_ui` takes the pointer) — drag
//! to move, any handle to resize (always keeping its shape: it's never
//! stretched), past the opposite side to flip it, Alt with a handle to crop,
//! right-click to flip or turn it a quarter. Moves snap to the frame's edges
//! and centre (Ctrl to place freely).
//!
//! Placement is fractions of the frame (see `capture::webcam::Placement`); it's
//! shared with the capture, so what you drag is what's recorded, live. So is
//! everything else here: a running capture follows whichever camera is open,
//! so the camera, its format, and adding or removing it all apply at once.

use capture::webcam::{Placement, Status};
use egui::{Color32, CursorIcon, Pos2, Rect, Stroke, vec2};

use crate::App;
use capture::webcam::{CameraControl, ControlKind};
use crate::settings::{PlacementCfg, WebcamCfg};

/// How wide the webcam's box is at first, as a fraction of the frame (see
/// `Placement::default_for`).
const FIRST_WIDTH: f32 = 0.25;

/// Smallest the webcam's box gets, as a fraction of the frame.
const MIN_SIZE: f32 = 0.03;
/// Snap distance, as a fraction of the frame.
const SNAP: f32 = 0.012;
/// How much of a box (of the video, across and down) stays on the video
/// however far it's moved past its edges, so it can always be taken hold
/// of again.
pub(crate) const SEEN: f32 = 0.03;

/// Where a box `size` big, at `at`, goes along one axis to keep [`SEEN`]
/// of it on the video.
pub(crate) fn seen(at: f32, size: f32) -> f32 {
    let s = SEEN.min(size);
    at.clamp(s - size, 1.0 - s)
}

/// The nearest of the video's edges and middle and `lines` to `v`, if one
/// is close enough to snap to.
fn snap_to(v: f32, lines: &[f32]) -> Option<f32> {
    [0.0, 0.5, 1.0].iter().chain(lines).copied().filter(|l| (l - v).abs() < SNAP).min_by(|a, b| (a - v).abs().total_cmp(&(b - v).abs()))
}

/// `r`, resized with its moving sides `[left, top, right, bottom]`, one of
/// them on a line near it (the video's edges and middle, `lines`): each on
/// its own, or (`keep`, its shape kept) the box scaled from its other sides
/// (its middle, across an axis not resized) so the nearest one is.
pub(crate) fn snap_sides(r: Rect, [l, t, rt, b]: [bool; 4], keep: bool, lines: &Lines) -> Rect {
    if !keep {
        let mut s = r;
        if l && let Some(v) = snap_to(s.min.x, &lines.xs).filter(|v| *v < s.max.x) {
            s.min.x = v;
        }
        if rt && let Some(v) = snap_to(s.max.x, &lines.xs).filter(|v| *v > s.min.x) {
            s.max.x = v;
        }
        if t && let Some(v) = snap_to(s.min.y, &lines.ys).filter(|v| *v < s.max.y) {
            s.min.y = v;
        }
        if b && let Some(v) = snap_to(s.max.y, &lines.ys).filter(|v| *v > s.min.y) {
            s.max.y = v;
        }
        return s;
    }
    let (w, h) = (r.width().max(1e-6), r.height().max(1e-6));
    let ax = if l { r.max.x } else if rt { r.min.x } else { r.center().x };
    let ay = if t { r.max.y } else if b { r.min.y } else { r.center().y };
    // Each moving side's nearest line: how far, and the scale it takes.
    let mut best: Option<(f32, f32)> = None;
    let mut consider = |edge: f32, ls: &[f32], k: &dyn Fn(f32) -> f32| {
        if let Some(v) = snap_to(edge, ls) {
            let d = (v - edge).abs();
            if best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, k(v)));
            }
        }
    };
    if l {
        consider(r.min.x, &lines.xs, &|v| (ax - v) / w);
    }
    if rt {
        consider(r.max.x, &lines.xs, &|v| (v - ax) / w);
    }
    if t {
        consider(r.min.y, &lines.ys, &|v| (ay - v) / h);
    }
    if b {
        consider(r.max.y, &lines.ys, &|v| (v - ay) / h);
    }
    let Some((_, k)) = best.filter(|(_, k)| *k > 0.01) else { return r };
    let (nw, nh) = (w * k, h * k);
    let x = if l { ax - nw } else if rt { ax } else { ax - nw / 2.0 };
    let y = if t { ay - nh } else if b { ay } else { ay - nh / 2.0 };
    Rect::from_min_size(Pos2::new(x, y), vec2(nw, nh))
}

#[derive(Default)]
pub(crate) struct WebcamView {
    /// Cameras for the picker, refreshed when it opens.
    cameras: Vec<capture::Device>,
    /// Just added, or a new camera or format: size this webcam's box to the
    /// camera's shape once the camera delivers it.
    fit_pending: Option<u64>,
    /// The camera's size when that was asked for: until it changes, the
    /// camera is still the old one (it takes a moment to reopen), and fitting
    /// to it would shape the box for the format just left.
    fit_from: Option<(u32, u32)>,
    /// The crop's page, open: the camera's own picture, to crop on.
    pub(crate) crop: Option<CameraPanel>,
    /// The selected webcam's camera settings, as last read (see
    /// `App::camera_settings_sections`).
    inline: Option<CameraPanel>,
}

impl WebcamView {
    /// Fit webcam `key`'s box to its camera's shape once the camera
    /// (`device`, as it was) reopens.
    fn ask_fit(&mut self, key: u64, device: &str) {
        self.fit_pending = Some(key);
        self.fit_from = match capture::webcam::status_of(device) {
            Status::Live { width, height } => Some((width, height)),
            _ => None,
        };
    }
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
    pub(crate) const ALL: [Handle; 8] = [Handle::NW, Handle::N, Handle::NE, Handle::E, Handle::SE, Handle::S, Handle::SW, Handle::W];

    /// Which sides it moves: left, top, right, bottom.
    pub(crate) fn sides(self) -> [bool; 4] {
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
    pub(crate) fn pos(self, r: Rect) -> Pos2 {
        let [l, t, rr, b] = self.sides();
        let x = if l { r.left() } else if rr { r.right() } else { r.center().x };
        let y = if t { r.top() } else if b { r.bottom() } else { r.center().y };
        Pos2::new(x, y)
    }

    pub(crate) fn cursor(self) -> CursorIcon {
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
    /// Shift: cropping, keep the box's shape; resizing, stretch it.
    pub keep: bool,
}

/// More lines a move snaps to besides the frame's edges and centre: the
/// other sources' edges and middles, as fractions of the frame across (`xs`)
/// and down (`ys`).
#[derive(Debug, Clone, Default)]
pub(crate) struct Lines {
    pub xs: Vec<f32>,
    pub ys: Vec<f32>,
}

/// The placement after dragging `handle` by `(dx, dy)` (fractions of the
/// frame) from where it was at `start`, moves snapping to `lines` too.
///
/// Resizing scales the whole picture, from the opposite corner or edge (an
/// edge handle keeps the other axis centred), as stretched as it was; with
/// Shift (`mods.keep`) each side dragged goes its own way, stretching it,
/// as the screen's do. Dragging past that opposite side flips the picture
/// on that axis.
///
/// The box goes past the frame's edges as far as it's dragged, [`SEEN`]
/// of it staying on; its moving sides snap, as a move does (`mods.snap`).
pub(crate) fn drag(start: Placement, handle: Handle, dx: f32, dy: f32, mods: Mods, lines: &Lines) -> Placement {
    let mut p = start;
    if handle == Handle::Move {
        p.x = start.x + dx;
        p.y = start.y + dy;
        if mods.snap {
            p.x += snap_offset(&[p.x, p.x + p.w], p.x + p.w / 2.0, &lines.xs);
            p.y += snap_offset(&[p.y, p.y + p.h], p.y + p.h / 2.0, &lines.ys);
        }
        p.x = seen(p.x, p.w);
        p.y = seen(p.y, p.h);
        return p;
    }
    let [l, t, r, b] = handle.sides();
    if mods.crop {
        return crop(start, [l, t, r, b], dx, dy, mods.keep, mods.snap.then_some(lines));
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
    // One uniform scale: for a corner, the pointer's place along the box's
    // diagonal (so it scales the same whichever way it's moved; each axis
    // still flips on its own), or the only axis dragged (an edge).
    let k = match (kx, ky) {
        (Some((_, a)), Some((_, b))) => {
            let (w2, h2) = (start.w * start.w, start.h * start.h);
            (a.abs() * w2 + b.abs() * h2) / (w2 + h2).max(1e-9)
        }
        (Some((_, a)), None) | (None, Some((_, a))) => a.abs(),
        (None, None) => 1.0,
    };
    // As big as it's dragged, past the video's edges too (up to a few
    // times the video).
    let (most_x, most_y) = (GROW_MOST / start.w, GROW_MOST / start.h);
    // Shift: each axis as far as it's dragged (one not dragged stays), and
    // the picture as much more stretched.
    let (ax, ay) = if mods.keep {
        let each = |ka: Option<(f32, f32)>, most: f32, size: f32| ka.map_or(1.0, |(_, k)| k.abs().min(most).max(MIN_SIZE / size));
        (each(kx, most_x, start.w), each(ky, most_y, start.h))
    } else {
        let k = k.min(most_x.min(most_y)).max(MIN_SIZE / start.w.min(start.h));
        (k, k)
    };
    let (w, h) = (start.w * ax, start.h * ay);
    p.stretch = start.stretch * ax / ay;
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
    // Its moving sides snap to the video's edges and middle and the other
    // sources (but with Ctrl): which sides moved, as it's ended up.
    if mods.snap {
        let r0 = Rect::from_min_size(Pos2::new(p.x, p.y), vec2(p.w, p.h));
        let moving = |ka: Option<(f32, f32)>, pos: f32| ka.map_or((false, false), |(anchor, _)| if (pos - anchor).abs() < 1e-6 { (false, true) } else { (true, false) });
        let ((ml, mr), (mt, mb)) = (moving(kx, p.x), moving(ky, p.y));
        let r1 = snap_sides(r0, [ml, mt, mr, mb], !mods.keep, lines);
        if r1.width() > MIN_SIZE && r1.height() > MIN_SIZE {
            if mods.keep {
                p.stretch *= (r1.width() / r0.width()) / (r1.height() / r0.height());
            }
            (p.x, p.y, p.w, p.h) = (r1.min.x, r1.min.y, r1.width(), r1.height());
        }
    }
    p
}

/// How big a box can be dragged, in videos (across and down).
const GROW_MOST: f32 = 8.0;

/// Crop the sides in `sides` (as seen: a flipped picture's left side is the
/// camera's right) as they're dragged: the picture keeps its scale, the box
/// shrinks (or grows back) with the crop, like OBS's Alt-drag. With `keep`,
/// the box keeps its shape.
fn crop(start: Placement, sides: [bool; 4], dx: f32, dy: f32, keep: bool, lines: Option<&Lines>) -> Placement {
    let mut p = start;
    let c = start.crop;
    // Which camera side each visible side is.
    let (left, right) = if start.flip_h { (2, 0) } else { (0, 2) };
    let (top, bottom) = if start.flip_v { (3, 1) } else { (1, 3) };
    // Frame fraction per camera fraction, across and down.
    let sx = start.w / (1.0 - c[0] - c[2]).max(0.01);
    let sy = start.h / (1.0 - c[1] - c[3]).max(0.01);
    // The whole camera picture, on the frame: the box stays inside it (it
    // can't uncrop past it) and inside the frame, showing at least 5% of it.
    let whole = Rect::from_min_max(Pos2::new(start.x - c[left] * sx, start.y - c[top] * sy), Pos2::new(start.x + start.w + c[right] * sx, start.y + start.h + c[bottom] * sy));
    let from = Rect::from_min_size(Pos2::new(start.x, start.y), vec2(start.w, start.h));
    let to = resize_within(from, sides, vec2(dx, dy), keep, whole, vec2(0.05 * sx, 0.05 * sy), lines);
    p.crop[left] = (to.min.x - whole.min.x) / sx;
    p.crop[right] = (whole.max.x - to.max.x) / sx;
    p.crop[top] = (to.min.y - whole.min.y) / sy;
    p.crop[bottom] = (whole.max.y - to.max.y) / sy;
    // Uncropped sides stay exactly uncropped.
    p.crop = p.crop.map(|v| if v < 1e-6 { 0.0 } else { v });
    (p.x, p.y, p.w, p.h) = (to.min.x, to.min.y, to.width(), to.height());
    p
}

/// `start` with the sides in `sides` dragged by `d`, inside `bounds`, at
/// least `least` across and down. With `keep`, its shape is kept: scaled
/// from the opposite corner or side (centred on an axis not dragged), a
/// corner following the pointer's place along the diagonal. With `lines`,
/// a side dragged snaps to one near it (or the video's edges or middle).
pub(crate) fn resize_within(start: Rect, sides: [bool; 4], d: egui::Vec2, keep: bool, bounds: Rect, least: egui::Vec2, lines: Option<&Lines>) -> Rect {
    let to = resize_free(start, sides, d, keep, bounds, least);
    let Some(lines) = lines else { return to };
    let snapped = snap_sides(to, sides, keep, lines);
    let inside = bounds.expand(1e-5).contains_rect(snapped) && snapped.width() >= least.x - 1e-5 && snapped.height() >= least.y - 1e-5;
    if inside { snapped } else { to }
}

/// [`resize_within`], without snapping.
fn resize_free(start: Rect, [l, t, r, b]: [bool; 4], d: egui::Vec2, keep: bool, bounds: Rect, least: egui::Vec2) -> Rect {
    if !keep {
        let mut s = start;
        if l {
            s.min.x = (s.min.x + d.x).clamp(bounds.min.x, (s.max.x - least.x).max(bounds.min.x));
        }
        if r {
            s.max.x = (s.max.x + d.x).clamp((s.min.x + least.x).min(bounds.max.x), bounds.max.x);
        }
        if t {
            s.min.y = (s.min.y + d.y).clamp(bounds.min.y, (s.max.y - least.y).max(bounds.min.y));
        }
        if b {
            s.max.y = (s.max.y + d.y).clamp((s.min.y + least.y).min(bounds.max.y), bounds.max.y);
        }
        return s;
    }
    let (w, h) = (start.width().max(1e-6), start.height().max(1e-6));
    let kx = (l || r).then(|| (w + if l { -d.x } else { d.x }) / w);
    let ky = (t || b).then(|| (h + if t { -d.y } else { d.y }) / h);
    // A corner: the pointer's place along the diagonal.
    let k = match (kx, ky) {
        (Some(a), Some(b)) => (a * w * w + b * h * h) / (w * w + h * h),
        (Some(a), None) | (None, Some(a)) => a,
        (None, None) => 1.0,
    };
    // How far it can grow, per axis: from its anchor to the edge it grows
    // toward, or both ways from its middle.
    let room = |near: bool, far: bool, lo: f32, hi: f32, blo: f32, bhi: f32| -> f32 {
        if near {
            hi - blo
        } else if far {
            bhi - lo
        } else {
            let c = (lo + hi) / 2.0;
            2.0 * (c - blo).min(bhi - c)
        }
    };
    let most = (room(l, r, start.min.x, start.max.x, bounds.min.x, bounds.max.x) / w).min(room(t, b, start.min.y, start.max.y, bounds.min.y, bounds.max.y) / h);
    let k = k.max((least.x / w).max(least.y / h)).min(most);
    let (nw, nh) = (w * k, h * k);
    let x = if l { start.max.x - nw } else if r { start.min.x } else { start.center().x - nw / 2.0 };
    let y = if t { start.max.y - nh } else if b { start.min.y } else { start.center().y - nh / 2.0 };
    Rect::from_min_size(Pos2::new(x, y), vec2(nw, nh))
}

/// All of a cropped picture: its box where the whole picture is now (at the
/// same scale, what showed where it was), then kept inside the frame.
pub(crate) fn uncrop(p: Placement) -> Placement {
    let c = p.crop;
    let sx = p.w / (1.0 - c[0] - c[2]).max(0.01);
    let sy = p.h / (1.0 - c[1] - c[3]).max(0.01);
    // The sides as they show: a mirrored picture's left is its right.
    let left = if p.flip_h { c[2] } else { c[0] };
    let top = if p.flip_v { c[3] } else { c[1] };
    contain(Placement { x: p.x - left * sx, y: p.y - top * sy, w: sx, h: sy, crop: [0.0; 4], ..p })
}

/// The box with [`SEEN`] of it on the video at least (moved, not shrunk):
/// where a box typed, or a group's, may go.
pub(crate) fn on_video(mut p: Placement) -> Placement {
    p.x = seen(p.x, p.w);
    p.y = seen(p.y, p.h);
    p
}

/// The box inside the frame: shrunk about its centre if it's bigger than the
/// frame (keeping its shape), then moved in. For a box that may have left it
/// (turned a quarter, which swaps its sides).
pub(crate) fn contain(mut p: Placement) -> Placement {
    let k = (1.0 / p.w).min(1.0 / p.h).min(1.0);
    if k < 1.0 {
        let (cx, cy) = (p.x + p.w / 2.0, p.y + p.h / 2.0);
        p.w *= k;
        p.h *= k;
        p.x = cx - p.w / 2.0;
        p.y = cy - p.h / 2.0;
    }
    p.x = p.x.clamp(0.0, 1.0 - p.w);
    p.y = p.y.clamp(0.0, 1.0 - p.h);
    p
}

/// How far to move so the nearest of `edges` (to 0 or 1) or `center` (to 0.5)
/// lands on it, or any of them on one of `lines`, if one is close enough.
pub(crate) fn snap_offset(edges: &[f32], center: f32, lines: &[f32]) -> f32 {
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
    for &l in lines {
        for &e in edges.iter().chain([&center]) {
            consider(l - e);
        }
    }
    best.unwrap_or(0.0)
}

impl App {
    /// Just added, or a new camera or format: once its shape is known, give
    /// the webcam's box that shape (same width, same top-left; crop kept),
    /// in a video of shape `frame_aspect`.
    pub(crate) fn webcam_fit_pending(&mut self, ctx: &egui::Context, frame_aspect: f32) {
        let Some(key) = self.webcam_view.fit_pending else { return };
        let Some(device) = self.settings.webcam(key).map(|w| w.id.clone()) else {
            self.webcam_view.fit_pending = None;
            return;
        };
        let status = capture::webcam::status_of(&device);
        {
            let live = match &status {
                Status::Live { width, height } => Some((*width, *height)),
                _ => None,
            };
            let wanted = self.settings.webcam(key).and_then(|w| w.format).map(|f| (f.width, f.height));
            // The new camera or format is open: its size is the one asked
            // for, or (Automatic, another camera) not the one before.
            let reopened = live.is_some_and(|l| match wanted {
                Some(want) => l == want,
                None => Some(l) != self.webcam_view.fit_from,
            });
            if !reopened {
                ctx.request_repaint_after(std::time::Duration::from_millis(50));
            }
            if let (true, Status::Live { width, height }, Some(w)) = (reopened, &status, self.settings.webcam_mut(key)) {
                let p = &mut w.placement;
                let (cw, ch) = if p.turns % 2 == 1 { (*height, *width) } else { (*width, *height) };
                let camera_aspect = cw as f32 / ch.max(1) as f32;
                let visible = (1.0 - p.crop[0] - p.crop[2]).max(0.05) / (1.0 - p.crop[1] - p.crop[3]).max(0.05);
                p.h = p.w * frame_aspect / (camera_aspect * visible);
                p.frame_aspect = Some(frame_aspect);
                self.webcam_view.fit_pending = None;
                self.webcam_view.fit_from = None;
            }
        }
    }

    /// Cameras for the Add menu and the pickers, looked for again.
    pub(crate) fn refresh_cameras(&mut self) {
        self.webcam_view.cameras = capture::webcam::list_cameras();
    }

    /// The cameras found when last looked (see `refresh_cameras`).
    pub(crate) fn cameras(&self) -> &[capture::Device] {
        &self.webcam_view.cameras
    }

    /// Put camera `c` in the video, in front of the rest, in the
    /// bottom-right corner of a video of shape `frame_aspect`. Its key.
    pub(crate) fn add_webcam(&mut self, c: capture::Device, frame_aspect: f32) -> u64 {
        let key = self.settings.new_key();
        self.webcam_view.ask_fit(key, &c.id);
        self.settings.webcams.push(WebcamCfg {
            key,
            id: c.id,
            name: c.name,
            title: None,
            format: None,
            placement: PlacementCfg::laid_out(Placement::default_for(16.0 / 9.0, frame_aspect), frame_aspect),
            enabled: true,
        });
        self.settings.layer_order.insert(0, crate::canvas_ui::Layer::Webcam(key));
        key
    }

    /// Webcam `key`'s box takes its camera's shape once the camera (`device`,
    /// as it was before a change) opens again: after another camera or
    /// format is picked.
    pub(crate) fn webcam_refit_later(&mut self, key: u64, device: &str) {
        self.webcam_view.ask_fit(key, device);
    }

    /// Webcam `key` cropped to `c` (its sides cut off, as `Placement::crop`),
    /// its box taking the shape of what's left.
    pub(crate) fn webcam_set_crop(&mut self, key: u64, c: [f32; 4]) {
        let Some(p) = self.settings.webcam(key).map(|w| w.placement) else { return };
        let orient = Orientation { flip_h: p.flip_h, flip_v: p.flip_v, turns: p.turns };
        self.webcam_recrop(key, orient, c);
    }

    /// A webcam's own settings, under its other settings: the camera's
    /// controls in groups (exposure, white balance, focus…), as its Camera
    /// page had them, read from the camera as it opens; how fast it really
    /// goes; and putting them back to its defaults, or (Windows) its
    /// driver's own window. They apply at once, in every app, and are put
    /// back each time the camera's opened.
    pub(crate) fn camera_settings_sections(&mut self, ui: &mut egui::Ui, key: u64) {
        use crate::theme::{self, W};
        let pal = theme::pal(ui);
        let Some(cam) = self.settings.webcam(key).cloned() else { return };
        if !capture::webcam::HAS_CONTROLS && !capture::webcam::HAS_SETTINGS {
            return;
        }
        // The camera's settings as last read: again for another camera, and
        // as it opens (on Windows they're only there while it's open).
        if self.webcam_view.inline.as_ref().is_none_or(|p| p.device != cam.id) {
            self.webcam_view.inline = Some(CameraPanel::open(&cam.id));
        }
        let live = matches!(capture::webcam::status_of(&cam.id), Status::Live { .. });
        let format_fps = cam.format.map(capture::webcam::Format::from).or_else(|| capture::webcam::formats(&cam.id).into_iter().max_by_key(|f| f.auto_rank())).map(|f| f.fps());
        let Some(panel) = self.webcam_view.inline.as_mut() else { return };
        if live && !panel.live {
            panel.controls = capture::webcam::camera_controls(&panel.device);
        }
        panel.live = live;
        measure_rate(ui.ctx(), panel);
        let mut set: Vec<(u32, i64)> = Vec::new();
        let mut reset = false;
        crate::ui_kit::section_head(ui, "Camera settings", false, |_| {});
        // What it really sends, when that's less than it should.
        let status = match (capture::webcam::status_of(&panel.device), panel.fps) {
            (Status::Live { width, height }, Some(fps)) => format!("{width}×{height} · {fps:.0} new pictures a second"),
            (Status::Live { width, height }, None) => format!("{width}×{height}"),
            _ => "The camera's settings show once it's on.".to_owned(),
        };
        ui.add(egui::Label::new(theme::text(&status, 12.5, W::Regular).color(pal.text3)).wrap());
        if let Some(why) = slowdown(panel.fps, format_fps) {
            ui.add(egui::Label::new(theme::text(why, 12.5, W::Regular).color(pal.warn)).wrap());
        }
        if capture::webcam::HAS_CONTROLS && live {
            controls_view(ui, panel, format_fps, &mut set);
        }
        let resettable = !panel.controls.is_empty();
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
            if resettable && crate::ui_kit::Btn::normal("Reset to defaults").small().tip("Back to the camera's own defaults").show(ui).clicked() {
                reset = true;
            }
            // What only its driver has, in the driver's own window.
            if capture::webcam::HAS_SETTINGS && crate::ui_kit::Btn::normal("Driver settings…").small().tip("The camera driver's own settings window, for anything not here").show(ui).clicked() {
                capture::webcam::open_settings(&panel.device, &cam.name);
            }
        });
        ui.add(egui::Label::new(theme::text("They apply right away, in every app, and are put back each time HasteClip opens this camera.", 12.0, W::Regular).color(pal.text3)).wrap());
        let device = panel.device.clone();
        if reset {
            // Manual values first, while they're still taken, then the automatic modes.
            let (autos, rest): (Vec<_>, Vec<_>) = panel.controls.iter().filter(|c| c.value != c.default).partition(|c| !matches!(c.kind, ControlKind::Int { .. }));
            set = rest.into_iter().chain(autos).map(|c| (c.id, c.default)).collect();
            self.settings.camera_controls.remove(&device);
        }
        if !set.is_empty() {
            let saved = self.settings.camera_controls.entry(device.clone()).or_default();
            for (id, value) in &set {
                if let Err(e) = capture::webcam::set_camera_control(&device, *id, *value) {
                    eprintln!("camera setting: {e:#}");
                }
                // Kept only while it isn't the camera's default.
                match panel.controls.iter().find(|c| c.id == *id) {
                    Some(c) if c.default == *value || reset => {
                        saved.remove(id);
                    }
                    _ => {
                        saved.insert(*id, *value);
                    }
                }
            }
            if saved.is_empty() {
                self.settings.camera_controls.remove(&device);
            }
            let values = self.settings.camera_controls.get(&device).map(|m| m.iter().map(|(k, v)| (*k, *v)).collect()).unwrap_or_default();
            capture::webcam::remember_controls(&device, values);
            panel.controls = capture::webcam::camera_controls(&device);
        }
    }
    /// Whether webcam `key`'s position, and its size, can be reset: it's not
    /// in the bottom-right corner, or not a quarter of the video wide, or
    /// stretched.
    pub(crate) fn webcam_resettable(&self, key: u64) -> (bool, bool) {
        let Some(p) = self.settings.webcam(key).map(|w| w.placement) else { return (false, false) };
        let at_corner = (p.x - (1.0 - p.w)).abs() < 1e-3 && (p.y - (1.0 - p.h)).abs() < 1e-3;
        (!at_corner, (p.w - FIRST_WIDTH).abs() > 1e-3 || (p.stretch - 1.0).abs() > 1e-3)
    }

    /// The webcam back in the bottom-right corner, its size kept.
    pub(crate) fn webcam_reset_position(&mut self, key: u64, frame_aspect: f32) {
        let Some(w) = self.settings.webcam_mut(key) else { return };
        let mut p: Placement = w.placement.into();
        (p.x, p.y) = (1.0 - p.w, 1.0 - p.h);
        w.placement = PlacementCfg::laid_out(contain(p), frame_aspect);
    }

    /// The webcam a quarter of the video wide again, in its own shape
    /// (unstretched), where it is (in a corner, it stays in it).
    pub(crate) fn webcam_reset_size(&mut self, key: u64, frame_aspect: f32) {
        let Some(w) = self.settings.webcam_mut(key) else { return };
        let mut p: Placement = w.placement.into();
        if p.w <= 0.0 || p.h <= 0.0 {
            return;
        }
        let tall = p.h / p.w * p.stretch.max(1e-3);
        p.stretch = 1.0;
        let r = crate::canvas_ui::resized_in_place(Rect::from_min_size(Pos2::new(p.x, p.y), vec2(p.w, p.h)), vec2(FIRST_WIDTH, FIRST_WIDTH * tall));
        (p.x, p.y, p.w, p.h) = (r.min.x, r.min.y, r.width(), r.height());
        w.placement = PlacementCfg::laid_out(contain(p), frame_aspect);
    }

    /// All of the webcam's picture again: its box grows back around what
    /// showed, at the same scale, kept in the video.
    pub(crate) fn webcam_uncropped(&mut self, key: u64, frame_aspect: f32) {
        if let Some(w) = self.settings.webcam_mut(key) {
            w.placement = PlacementCfg::laid_out(uncrop(w.placement.into()), frame_aspect);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(x: f32, y: f32, w: f32, h: f32) -> Placement {
        Placement { x, y, w, h, crop: [0.0; 4], flip_h: false, flip_v: false, turns: 0, stretch: 1.0 }
    }

    const PLAIN: Mods = Mods { crop: false, snap: false, keep: false };

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn move_and_snap() {
        let p = drag(at(0.5, 0.5, 0.2, 0.2), Handle::Move, 0.1, -0.1, PLAIN, &Lines::default());
        assert!(close(p.x, 0.6) && close(p.y, 0.4) && close(p.w, 0.2));
        // Right edge lands 0.005 from the frame's edge: snaps onto it.
        let p = drag(at(0.5, 0.5, 0.2, 0.2), Handle::Move, 0.295, 0.0, Mods { snap: true, ..PLAIN }, &Lines::default());
        assert!(close(p.x + p.w, 1.0));
        // Centre near the middle: snaps to it.
        let p = drag(at(0.0, 0.0, 0.2, 0.2), Handle::Move, 0.405, 0.0, Mods { snap: true, ..PLAIN }, &Lines::default());
        assert!(close(p.x + p.w / 2.0, 0.5));
    }

    /// The box stays in the frame: moving stops at the edge, growing stops at
    /// it (shape kept), uncropping stops at it, and a turned box is put back.
    #[test]
    fn goes_past_the_frame() {
        let in_frame = |p: Placement| p.x >= -1e-4 && p.y >= -1e-4 && p.x + p.w <= 1.0 + 1e-4 && p.y + p.h <= 1.0 + 1e-4;
        // Moved out, a sliver of it stays on.
        let p = drag(at(0.7, 0.7, 0.2, 0.2), Handle::Move, 0.5, -0.9, PLAIN, &Lines::default());
        assert!(close(p.x, 0.97) && close(p.y, -0.17), "{p:?}");
        // From the top-left, the bottom-right corner grows as far as it's dragged.
        let p = drag(at(0.6, 0.7, 0.2, 0.1), Handle::SE, 0.9, 0.9, PLAIN, &Lines::default());
        assert!(close(p.x, 0.6) && close(p.y, 0.7) && close(p.w / p.h, 2.0) && p.x + p.w > 1.0, "{p:?}");
        // Its corner snaps onto the frame's, with snapping on.
        let p = drag(at(0.6, 0.7, 0.2, 0.1), Handle::SE, 0.195, 0.0975, Mods { snap: true, ..PLAIN }, &Lines::default());
        assert!(close(p.x + p.w, 1.0), "{p:?}");
        // Flipped past its anchor: still flipped.
        let p = drag(at(0.3, 0.3, 0.2, 0.2), Handle::E, -0.9, 0.0, PLAIN, &Lines::default());
        assert!(p.flip_h, "{p:?}");
        // Uncropping the left side goes past the frame's edge, as far as the picture.
        let cropped = Placement { crop: [0.5, 0.0, 0.0, 0.0], ..at(0.05, 0.5, 0.2, 0.2) };
        let p = drag(cropped, Handle::W, -0.5, 0.0, Mods { crop: true, ..PLAIN }, &Lines::default());
        assert!(close(p.x, -0.15) && p.crop[0] == 0.0, "{p:?}");
        // Turned near the edge, a tall box is moved back in.
        let p = contain(at(0.8, 0.0, 0.3, 1.2));
        assert!(in_frame(p) && close(p.w / p.h, 0.25), "{p:?}");
    }

    /// With Shift, a side goes its own way, stretching the picture (the
    /// other axis stays); a corner stretches both ways; without Shift, a
    /// stretched webcam scales as stretched as it is.
    #[test]
    fn shift_stretches_the_webcam() {
        let shift = Mods { keep: true, ..PLAIN };
        let p = drag(at(0.5, 0.5, 0.2, 0.1), Handle::E, 0.1, 0.0, shift, &Lines::default());
        assert!(close(p.w, 0.3) && close(p.h, 0.1) && close(p.x, 0.5) && close(p.y, 0.5) && close(p.stretch, 1.5), "{p:?}");
        let q = drag(at(0.5, 0.5, 0.2, 0.1), Handle::SE, 0.2, 0.0, shift, &Lines::default());
        assert!(close(q.w, 0.4) && close(q.h, 0.1) && close(q.stretch, 2.0), "{q:?}");
        let r = drag(p, Handle::SE, 0.15, 0.05, PLAIN, &Lines::default());
        assert!(close(r.w / r.h, 3.0) && close(r.stretch, 1.5), "{r:?}");
    }

    #[test]
    fn every_handle_keeps_the_shape() {
        let start = at(0.5, 0.5, 0.2, 0.1);
        let p = drag(start, Handle::SE, 0.1, 0.05, PLAIN, &Lines::default());
        assert!(close(p.w, 0.3) && close(p.h, 0.15) && close(p.x, 0.5) && close(p.y, 0.5));
        // Off the diagonal, its nearest place on it: no jump when the pointer
        // crosses from mostly across to mostly down.
        let (a, b) = (drag(start, Handle::SE, 0.1, 0.049, PLAIN, &Lines::default()), drag(start, Handle::SE, 0.1, 0.051, PLAIN, &Lines::default()));
        assert!((a.w - b.w).abs() < 0.002, "{a:?} {b:?}");
        // From the top-left corner: the bottom-right stays put.
        let p = drag(start, Handle::NW, -0.1, 0.0, PLAIN, &Lines::default());
        assert!(close(p.x + p.w, 0.7) && close(p.y + p.h, 0.6) && close(p.w / p.h, 2.0));
        // An edge scales the whole picture too, from the opposite edge, centred
        // on the other axis.
        let p = drag(start, Handle::E, 0.1, 0.3, PLAIN, &Lines::default());
        assert!(close(p.w, 0.3) && close(p.h, 0.15) && close(p.x, 0.5) && close(p.y + p.h / 2.0, 0.55));
        let p = drag(start, Handle::N, 0.0, -0.05, PLAIN, &Lines::default());
        assert!(close(p.h, 0.15) && close(p.w, 0.3) && close(p.y + p.h, 0.6) && close(p.x + p.w / 2.0, 0.6));
        assert!(!p.flip_h && !p.flip_v);
    }

    #[test]
    fn dragging_past_the_other_side_flips() {
        let start = at(0.5, 0.5, 0.2, 0.1);
        // Right edge dragged 0.3 left: past the left edge, the same size, mirrored.
        let p = drag(start, Handle::E, -0.4, 0.0, PLAIN, &Lines::default());
        assert!(p.flip_h && !p.flip_v);
        assert!(close(p.w, 0.2) && close(p.x + p.w, 0.5));
        // Bottom-right corner up past the top only: flipped vertically.
        let p = drag(start, Handle::SE, 0.0, -0.2, PLAIN, &Lines::default());
        assert!(p.flip_v && !p.flip_h && close(p.y + p.h, 0.5));
        // Past both: both.
        let p = drag(start, Handle::SE, -0.4, -0.2, PLAIN, &Lines::default());
        assert!(p.flip_h && p.flip_v);
        // Flipping back undoes it.
        let back = drag(p, Handle::NW, 0.4, 0.2, PLAIN, &Lines::default());
        assert!(!back.flip_h && !back.flip_v);
    }

    #[test]
    fn crop_keeps_the_scale() {
        // Box 0.2 wide shows the whole camera: frame fraction 0.2 per camera width.
        let start = at(0.5, 0.5, 0.2, 0.2);
        let crop = Mods { crop: true, ..PLAIN };
        let p = drag(start, Handle::W, 0.05, 0.0, crop, &Lines::default());
        assert!(close(p.crop[0], 0.25) && close(p.x, 0.55) && close(p.w, 0.15));
        // Dragging the right edge outward past the uncropped size doesn't uncrop
        // beyond the picture.
        let p = drag(start, Handle::E, 0.1, 0.0, crop, &Lines::default());
        assert!(close(p.crop[2], 0.0) && close(p.w, 0.2));
        // Crop the right, then pull it back: the box grows back.
        let cropped = drag(start, Handle::E, -0.05, 0.0, crop, &Lines::default());
        assert!(close(cropped.crop[2], 0.25) && close(cropped.w, 0.15));
        let back = drag(cropped, Handle::E, 0.05, 0.0, crop, &Lines::default());
        assert!(close(back.crop[2], 0.0) && close(back.w, 0.2));
    }

    #[test]
    fn crop_keeping_the_shape() {
        let start = at(0.4, 0.4, 0.2, 0.1);
        let keep = Mods { crop: true, keep: true, ..PLAIN };
        // The right edge in: the box keeps 2:1, cropped top and bottom alike.
        let p = drag(start, Handle::E, -0.05, 0.0, keep, &Lines::default());
        assert!(close(p.w / p.h, 2.0) && close(p.w, 0.15) && close(p.crop[1], p.crop[3]) && p.crop[1] > 0.0, "{p:?}");
        // A corner: the opposite corner stays put.
        let p = drag(start, Handle::SE, -0.1, -0.05, keep, &Lines::default());
        assert!(close(p.w / p.h, 2.0) && close(p.x, 0.4) && close(p.y, 0.4) && close(p.w, 0.1), "{p:?}");
    }

    /// Uncropped, the picture keeps its scale and what showed stays where it
    /// was; too big for the frame, it's made smaller and moved in.
    #[test]
    fn uncropping_keeps_the_scale() {
        let p = Placement { crop: [0.5, 0.0, 0.0, 0.25], ..at(0.6, 0.1, 0.2, 0.3) };
        let u = uncrop(p);
        assert!(close(u.w, 0.4) && close(u.h, 0.4) && close(u.x, 0.4) && close(u.y, 0.1), "{u:?}");
        assert_eq!(u.crop, [0.0; 4]);
        // Mirrored: the camera's left shows on the right.
        let u = uncrop(Placement { flip_h: true, ..p });
        assert!(close(u.x, 0.6) && close(u.w, 0.4), "{u:?}");
        // Past the frame's edge: moved in.
        let u = uncrop(Placement { crop: [0.0, 0.0, 0.5, 0.0], ..at(0.8, 0.0, 0.2, 0.2) });
        assert!(close(u.x, 0.6) && close(u.w, 0.4), "{u:?}");
    }

    #[test]
    fn crop_follows_a_flipped_picture() {
        // Mirrored: the visible left side is the camera's right side.
        let start = Placement { flip_h: true, ..at(0.5, 0.5, 0.2, 0.2) };
        let p = drag(start, Handle::W, 0.05, 0.0, Mods { crop: true, ..PLAIN }, &Lines::default());
        assert!(close(p.crop[2], 0.25) && close(p.crop[0], 0.0) && close(p.x, 0.55));
    }
}

// ---------------------------------------------------------------------------
// Camera page (Linux, Windows), and the webcam's crop page
// ---------------------------------------------------------------------------

/// The Camera page's state (or the crop page's): the camera's settings as
/// last read, and its picture.
pub(crate) struct CameraPanel {
    pub(crate) device: String,
    controls: Vec<CameraControl>,
    picture: Option<egui::TextureHandle>,
    seq: u64,
    /// The camera's real frame rate, measured: new pictures (not repeats)
    /// since when, from how many, and the last second's.
    new_pictures: u64,
    counted: Option<(std::time::Instant, u64)>,
    fps: Option<f32>,
    /// Whether the camera was live when last looked: its settings are read
    /// again as it opens (on Windows they're only there while it's open).
    live: bool,
    /// Cropping on the picture: the whole of it shown, its edges dragged.
    cropping: bool,
    /// The crop's corner or edge (or all of it) being dragged, the crop as
    /// shown when it started, and where the pointer was.
    crop_drag: Option<(Handle, egui::Rect, Pos2)>,
}

impl CameraPanel {
    /// The Camera page: its settings, read from the camera, and its picture.
    pub(crate) fn open(device: &str) -> Self {
        Self { controls: capture::webcam::camera_controls(device), ..Self::picture_of(device) }
    }

    /// The crop's page: the camera's picture, all of it, cropped on.
    pub(crate) fn cropping(device: &str) -> Self {
        Self { cropping: true, ..Self::picture_of(device) }
    }

    fn picture_of(device: &str) -> Self {
        let live = matches!(capture::webcam::status_of(device), Status::Live { .. });
        Self { device: device.to_owned(), controls: Vec::new(), picture: None, seq: 0, new_pictures: 0, counted: None, fps: None, live, cropping: false, crop_drag: None }
    }
}

/// The control ids grouped and named here (V4L2's; Windows' take the same).
use capture::webcam::cid;

/// One row of the panel.
#[derive(Debug, PartialEq)]
enum PanelRow<'a> {
    /// An automatic mode as a switch, and the setting it takes over (shown while it's off).
    Auto { label: &'static str, auto: &'a CameraControl, manual: Option<(&'static str, &'a CameraControl)> },
    Control { label: String, control: &'a CameraControl },
}

/// The heading of the camera's own settings, not named here.
const OTHER: &str = "Other";

/// The camera's controls in groups (heading, rows), all shown: named for
/// people, each automatic mode with the setting it takes over, and the
/// camera's own under Other.
fn panel_groups(controls: &[CameraControl]) -> Vec<(&'static str, Vec<PanelRow<'_>>)> {
    use cid::*;
    let find = |id: u32| controls.iter().find(|c| c.id == id);
    let named = |id: u32, label: &str| find(id).map(|control| PanelRow::Control { label: label.to_owned(), control });
    let auto = |auto: u32, label: &'static str, manual: u32, manual_label: &'static str| {
        // No automatic mode: the manual setting stands alone.
        find(auto).map(|a| PanelRow::Auto { label, auto: a, manual: find(manual).map(|m| (manual_label, m)) }).or_else(|| named(manual, manual_label))
    };
    let groups: Vec<(&'static str, Vec<Option<PanelRow>>)> = vec![
        (
            "Image",
            vec![named(BRIGHTNESS, "Brightness"), named(CONTRAST, "Contrast"), named(SATURATION, "Saturation"), named(SHARPNESS, "Sharpness"), named(HUE, "Hue"), named(GAMMA, "Gamma")],
        ),
        (
            "Exposure",
            vec![
                // A time, or the camera's own steps (no time to it).
                if find(EXPOSURE).is_some() { auto(AUTO_EXPOSURE, "Automatic exposure", EXPOSURE, "Exposure time") } else { auto(AUTO_EXPOSURE, "Automatic exposure", EXPOSURE_STEPS, "Exposure") },
                named(GAIN, "Gain"),
                named(BACKLIGHT, "Backlight compensation"),
                named(EXPOSURE_DYNAMIC_FPS, "Lower frame rate in the dark"),
                named(POWER_LINE, "Flicker (mains frequency)"),
            ],
        ),
        ("White balance", vec![auto(AUTO_WHITE_BALANCE, "Automatic white balance", WHITE_BALANCE, "Temperature")]),
        ("Focus", vec![auto(AUTO_FOCUS, "Autofocus", FOCUS, "Focus distance")]),
        ("Zoom and framing", vec![named(ZOOM, "Zoom"), named(PAN, "Pan"), named(TILT, "Tilt")]),
        (OTHER, Vec::new()),
    ];
    let known = [BRIGHTNESS, CONTRAST, SATURATION, HUE, AUTO_WHITE_BALANCE, GAMMA, GAIN, POWER_LINE, WHITE_BALANCE, SHARPNESS, BACKLIGHT, AUTO_EXPOSURE, EXPOSURE, EXPOSURE_STEPS, EXPOSURE_DYNAMIC_FPS, PAN, TILT, FOCUS, AUTO_FOCUS, ZOOM];
    let own = controls.iter().filter(|c| !known.contains(&c.id)).map(|c| Some(PanelRow::Control { label: c.name.clone(), control: c }));
    groups
        .into_iter()
        .map(|(h, mut rows)| {
            if h == OTHER {
                rows.extend(own.clone());
            }
            (h, rows.into_iter().flatten().collect::<Vec<_>>())
        })
        .filter(|(_, rows)| !rows.is_empty())
        .collect()
}

/// A control's widget, the same width for each kind so they line up: a
/// slider with its value, a switch (also for a slider that's only 0 or 1),
/// or a menu. What's changed goes in `set`.
fn control_widget(ui: &mut egui::Ui, c: &CameraControl, set: &mut Vec<(u32, i64)>) {
    use crate::theme::{self, W};
    let pal = theme::pal(ui);
    match &c.kind {
        ControlKind::Bool | ControlKind::Int { min: 0, max: 1, .. } => {
            let mut on = c.value != 0;
            if crate::ui_kit::switch(ui, &mut on).changed() {
                set.push((c.id, i64::from(on)));
            }
        }
        ControlKind::Int { min, max, step } => {
            // Right to left: the value, then the slider.
            let shown = if c.id == cid::WHITE_BALANCE { format!("{} K", c.value) } else { c.value.to_string() };
            ui.add_sized([46.0, 20.0], egui::Label::new(theme::text(&shown, 12.5, W::Regular).color(pal.text3)));
            let span = (max - min).max(1) as f32;
            let mut v = (c.value - min) as f32 / span;
            // On the camera's own steps (focus goes by 5 on a C922).
            let stops = u32::try_from((max - min) / step.max(&1)).unwrap_or(0);
            let slider = crate::transport::stepped_slider(ui, &mut v, 150.0, stops);
            if slider.changed() {
                let raw = *min as f32 + v * span;
                let stepped = (min + ((raw - *min as f32) / *step as f32).round() as i64 * step).clamp(*min, *max);
                if stepped != c.value {
                    set.push((c.id, stepped));
                }
            }
            if let Some(d) = arrow_steps(ui, &slider) {
                let to = (c.value + d * step).clamp(*min, *max);
                if to != c.value {
                    set.push((c.id, to));
                }
            }
            if slider.double_clicked() {
                set.push((c.id, c.default));
            }
            slider.on_hover_text("Double-click for the camera's default");
        }
        ControlKind::Menu(items) => {
            let current = items.iter().find(|(v, _)| *v == c.value).map_or_else(|| c.value.to_string(), |(_, n)| n.clone());
            let r = crate::ui_kit::select(ui, &current, 150.0);
            crate::ui_kit::menu_popup(&r, false, 200.0, |ui| {
                for (v, name) in items {
                    if crate::ui_kit::Item::new(name.clone()).check(*v == c.value).show(ui).clicked() {
                        set.push((c.id, *v));
                    }
                }
            });
        }
    }
}

/// Whether an automatic mode is on, and the value that turns it on or off.
fn auto_on(c: &CameraControl) -> bool {
    match c.kind {
        // Automatic exposure is a menu: anything but manual.
        ControlKind::Menu(_) => c.value != cid::EXPOSURE_MANUAL,
        _ => c.value != 0,
    }
}

fn auto_value(c: &CameraControl, on: bool) -> i64 {
    match &c.kind {
        ControlKind::Menu(items) if on => {
            if c.default != cid::EXPOSURE_MANUAL {
                c.default
            } else {
                items.iter().map(|(v, _)| *v).find(|v| *v != cid::EXPOSURE_MANUAL).unwrap_or(c.default)
            }
        }
        ControlKind::Menu(_) => cid::EXPOSURE_MANUAL,
        _ => i64::from(on),
    }
}

impl App {
    /// The Camera page, in Sources' place while it's open: the camera's own
    /// picture, held still, beside (or above) its settings, which scroll.
    pub(crate) fn camera_page(&mut self, ui: &mut egui::Ui) {
        use crate::theme::{self, W};
        use crate::ui_kit::Btn;
        // The webcam showing its camera: gone (taken out, another camera
        // picked), it's closed.
        let Some(key) = self.camera_panel.as_ref().and_then(|p| self.settings.webcam_of(&p.device)).map(|w| w.key) else {
            self.camera_panel = None;
            return;
        };
        // Back to Sources: the arrow, or Esc (as Settings' own pages).
        if !crate::ui_kit::overlay_open(ui.ctx()) && !ui.ctx().egui_wants_keyboard_input() && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            self.camera_panel = None;
            return;
        }
        let mut back = false;
        self.page_head(ui, "", |_, ui| {
            back = Btn::icon(crate::ui_kit::Kind::Ghost, egui_phosphor::regular::CARET_LEFT).tip("Back to Sources  (Esc)").show(ui).clicked();
            let p = theme::pal(ui);
            ui.label(theme::text("Camera", 20.0, W::Semibold).color(p.text));
        });
        let name = self.settings.webcam(key).map(|w| w.name.clone()).unwrap_or_default();
        let current = self.settings.webcam(key).and_then(|w| w.format).map(capture::webcam::Format::from);
        let mut pick: Option<Option<capture::webcam::Format>> = None;
        // How it's turned and mirrored in the clips (the webcam's placement).
        let orient = self.settings.webcam(key).map_or_else(Orientation::default, |w| Orientation { flip_h: w.placement.flip_h, flip_v: w.placement.flip_v, turns: w.placement.turns });
        let mut reorient: Option<Orientation> = None;
        // Its crop (the camera's sides cut off), set on its own page.
        let crop = self.settings.webcam(key).map_or([0.0; 4], |w| w.placement.crop);
        // The frame rate the camera runs at: as picked, or Automatic's pick.
        let fps = current.or_else(|| self.camera_panel.as_ref().and_then(|p| capture::webcam::formats(&p.device).into_iter().max_by_key(|f| f.auto_rank()))).map(|f| f.fps());
        let Some(panel) = self.camera_panel.as_mut() else { return };
        let live = matches!(capture::webcam::status_of(&panel.device), Status::Live { .. });
        if live && !panel.live {
            panel.controls = capture::webcam::camera_controls(&panel.device);
        }
        panel.live = live;
        let mut set: Vec<(u32, i64)> = Vec::new();
        let mut reset = false;
        ui.add_space(4.0);
        let w = ui.available_width();
        let h = ui.available_height();
        if w >= crate::sources_ui::TWO_COLUMNS {
            let col = (w - 28.0) / 2.0;
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 28.0;
                ui.allocate_ui_with_layout(vec2(col, h), egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.set_width(col);
                    ui.add_space(4.0);
                    picture_side(ui, panel, &name, fps, orient, crop, h * 0.7, &mut reset);
                });
                ui.allocate_ui_with_layout(vec2(col, h), egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.set_width(col);
                    crate::ui_kit::scroll(ui, "camera_controls", |ui| {
                        format_view(ui, &panel.device, current, &mut pick);
                        orientation_view(ui, orient, &mut reorient);
                        controls_view(ui, panel, fps, &mut set);
                        ui.add_space(28.0);
                    });
                });
            });
        } else {
            picture_side(ui, panel, &name, fps, orient, crop, h * 0.4, &mut reset);
            crate::ui_kit::gap(ui, crate::ui_kit::STACK_GAP);
            crate::ui_kit::scroll(ui, "camera_controls", |ui| {
                format_view(ui, &panel.device, current, &mut pick);
                orientation_view(ui, orient, &mut reorient);
                controls_view(ui, panel, fps, &mut set);
                ui.add_space(28.0);
            });
        }

        let device = panel.device.clone();
        if reset {
            // Manual values first, while they're still taken, then the automatic modes.
            let (autos, rest): (Vec<_>, Vec<_>) = panel.controls.iter().filter(|c| c.value != c.default).partition(|c| !matches!(c.kind, ControlKind::Int { .. }));
            set = rest.into_iter().chain(autos).map(|c| (c.id, c.default)).collect();
            self.settings.camera_controls.remove(&device);
        }
        if !set.is_empty() {
            let saved = self.settings.camera_controls.entry(device.clone()).or_default();
            for (id, value) in &set {
                if let Err(e) = capture::webcam::set_camera_control(&device, *id, *value) {
                    eprintln!("camera setting: {e:#}");
                }
                // Kept only while it isn't the camera's default.
                match panel.controls.iter().find(|c| c.id == *id) {
                    Some(c) if c.default == *value || reset => {
                        saved.remove(id);
                    }
                    _ => {
                        saved.insert(*id, *value);
                    }
                }
            }
            if saved.is_empty() {
                self.settings.camera_controls.remove(&device);
            }
            let values = self.settings.camera_controls.get(&device).map(|m| m.iter().map(|(k, v)| (*k, *v)).collect()).unwrap_or_default();
            capture::webcam::remember_controls(&device, values);
            panel.controls = capture::webcam::camera_controls(&device);
        }
        if let Some(p) = pick {
            self.webcam_view.ask_fit(key, &device);
            if let Some(w) = self.settings.webcam_mut(key) {
                w.format = p.map(Into::into);
            }
        }
        // Turned or mirrored: the placement, as the preview's right-click does
        // it (a quarter turn swaps the box's sides).
        if let (Some(o), Some(w)) = (reorient, self.settings.webcam_mut(key)) {
            let aspect = w.placement.frame_aspect.unwrap_or(16.0 / 9.0);
            let mut p: Placement = w.placement.into();
            p.flip_h = o.flip_h;
            p.flip_v = o.flip_v;
            // Turned a quarter at a time, as seen (`Placement::turn`), from how
            // it's seen now to how it's wanted.
            let seen = Orientation { flip_h: p.flip_h, flip_v: p.flip_v, turns: p.turns }.seen_turns();
            match (o.turns as i8 - seen as i8).rem_euclid(4) {
                1 => p = p.turn(1, aspect),
                2 => p = p.turn(1, aspect).turn(1, aspect),
                3 => p = p.turn(-1, aspect),
                _ => {}
            }
            w.placement = PlacementCfg::laid_out(contain(p), aspect);
        }
        if back {
            self.camera_panel = None;
        }
    }
}

impl App {
    /// The webcam cropped to `c` (its sides cut off, as `Placement::crop`):
    /// its box takes the shape of what's left, as stretched as it was (its
    /// width and middle kept),
    /// so nothing more is cut off to fill it; inside the video.
    fn webcam_recrop(&mut self, key: u64, orient: Orientation, c: [f32; 4]) {
        let Some(w) = self.settings.webcam_mut(key) else { return };
        let aspect = w.placement.frame_aspect.unwrap_or(16.0 / 9.0);
        let mut p: Placement = w.placement.into();
        if let Status::Live { width, height } = capture::webcam::status_of(&w.id) {
            let raw = width as f32 / height.max(1) as f32;
            let full = if p.turns % 2 == 1 { 1.0 / raw } else { raw };
            let kept = crop_shown(orient, c);
            let visible = full * kept.width() / kept.height().max(0.01);
            let (cx, cy) = (p.x + p.w / 2.0, p.y + p.h / 2.0);
            p.h = p.w * aspect / (visible * p.stretch.max(1e-3));
            p.x = cx - p.w / 2.0;
            p.y = cy - p.h / 2.0;
        }
        p.crop = c;
        w.placement = PlacementCfg::laid_out(contain(p), aspect);
    }

    /// The webcam's crop page, in Sources' place (as the screen's crop has
    /// one): the camera's own picture, all of it as it shows in the clips
    /// (turned and mirrored), what's kept dragged on it, and its size and
    /// place in the camera's pixels. It goes into the clips at once, the
    /// webcam's box taking the shape of what's kept.
    pub(crate) fn webcam_crop_page(&mut self, ui: &mut egui::Ui) {
        use crate::theme::{self, W};
        use crate::ui_kit::Btn;
        let pal = theme::pal(ui);
        // Closed with the arrow or Esc, or when there's no webcam showing
        // its camera to crop. Off, its camera's open while this is (see
        // `App::logic`).
        let Some(cam) = self.webcam_view.crop.as_ref().and_then(|p| self.settings.webcam_of(&p.device)).cloned() else {
            self.webcam_view.crop = None;
            return;
        };
        if !crate::ui_kit::overlay_open(ui.ctx()) && !ui.ctx().egui_wants_keyboard_input() && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            self.webcam_view.crop = None;
            return;
        }
        let mut back = false;
        self.page_head(ui, "", |_, ui| {
            back = Btn::icon(crate::ui_kit::Kind::Ghost, egui_phosphor::regular::CARET_LEFT).tip("Back to Sources  (Esc)").show(ui).clicked();
            ui.label(theme::text("Crop", 20.0, W::Semibold).color(pal.text));
            ui.label(theme::text(&cam.name, 13.0, W::Regular).color(pal.text3));
        });
        if back {
            self.webcam_view.crop = None;
            return;
        }
        if !cam.enabled {
            crate::ui_kit::hint(ui, "Your webcam's off: its camera's on while you crop it, and it stays out of your clips.");
        }
        let orient = Orientation { flip_h: cam.placement.flip_h, flip_v: cam.placement.flip_v, turns: cam.placement.turns };
        let crop = cam.placement.crop;
        // Its size's link keeps the shape as its edges are dragged too.
        let keep = crate::ui_kit::size_field_keeps(ui.ctx(), "webcam_crop").is_some();
        let size = self.camera_size(cam.key);
        let mut recrop: Option<[f32; 4]> = None;
        let Some(panel) = self.webcam_view.crop.as_mut() else { return };
        let picture = |ui: &mut egui::Ui, panel: &mut CameraPanel, max_h: f32, recrop: &mut Option<[f32; 4]>| {
            if let Some(c) = picture_view(ui, panel, orient, crop, max_h, keep) {
                *recrop = Some(c);
            }
            ui.add_space(8.0);
            ui.vertical_centered(|ui| {
                let hint = if keep {
                    "Drag the edges or corners (the shape's kept), or the middle to move it."
                } else {
                    "Drag the edges or corners (Shift keeps the shape), or the middle to move it."
                };
                ui.label(theme::text(hint, 12.5, W::Regular).color(pal.text3));
            });
        };
        ui.add_space(4.0);
        let (avail_w, avail_h) = (ui.available_width(), ui.available_height());
        if avail_w >= crate::sources_ui::TWO_COLUMNS {
            let col = (avail_w - 28.0) / 2.0;
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 28.0;
                ui.allocate_ui_with_layout(vec2(col, avail_h), egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.set_width(col);
                    ui.add_space(4.0);
                    picture(ui, panel, avail_h * 0.7, &mut recrop);
                });
                ui.allocate_ui_with_layout(vec2(col, avail_h), egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.set_width(col);
                    crop_numbers(ui, orient, crop, size, &mut recrop);
                });
            });
        } else {
            crate::ui_kit::scroll(ui, "webcam_crop_page", |ui| {
                picture(ui, panel, avail_h * 0.4, &mut recrop);
                ui.add_space(16.0);
                crop_numbers(ui, orient, crop, size, &mut recrop);
            });
        }
        if let Some(c) = recrop {
            self.webcam_recrop(cam.key, orient, c);
        }
    }
}

/// The webcam crop's size and place, in the camera's pixels as its picture
/// shows (`size`, turned; none until it's live), as the screen's crop has
/// them; then what's kept of it, and Reset. A new crop goes in `recrop`.
fn crop_numbers(ui: &mut egui::Ui, orient: Orientation, crop: [f32; 4], size: Option<(u32, u32)>, recrop: &mut Option<[f32; 4]>) {
    use crate::theme::{self, W};
    use crate::ui_kit::{Btn, Row};
    const BOX: f32 = 64.0;
    let pal = theme::pal(ui);
    let cropped = crop.iter().any(|c| *c > 0.0);
    let shown = crop_shown(orient, crop);
    // What's kept, in pixels.
    let kept = size.map(|(cw, ch)| {
        let px = |v: f32, n: u32| ((v * n as f32).round().max(0.0) as u32).min(n);
        ([px(shown.min.x, cw), px(shown.min.y, ch), px(shown.width(), cw).max(1), px(shown.height(), ch).max(1)], (cw, ch))
    });
    if let Some(([x, y, w, h], (cw, ch))) = kept {
        // Back to a crop, from the part kept in pixels.
        let to_crop = |x: u32, y: u32, w: u32, h: u32| {
            crop_from_shown(orient, Rect::from_min_size(Pos2::new(x as f32 / cw as f32, y as f32 / ch as f32), vec2(w as f32 / cw as f32, h as f32 / ch as f32)))
        };
        // As little as dragging on the picture keeps: a tenth of it.
        let least = ((cw / 10).max(2), (ch / 10).max(2));
        let centred = x.abs_diff((cw - w) / 2) <= 1 && y.abs_diff((ch - h) / 2) <= 1;
        crate::ui_kit::list(ui, "webcam_crop", |l| {
            // Its shape: the biggest of it there's room for, where it is, and
            // kept as it's sized (it's the size's link).
            l.row(Row::new("Aspect ratio"), |ui| {
                let now = crate::ui_kit::size_field_keeps(ui.ctx(), "webcam_crop").map(crate::ui_kit::ratio_of);
                let from = crate::ui_kit::ratio_of(w as f64 / h.max(1) as f64);
                match crate::ui_kit::ratio_field(ui, "webcam_crop_ratio", ("Any", ""), now, from) {
                    Some(None) => crate::ui_kit::set_size_field_keeps(ui.ctx(), "webcam_crop", None),
                    Some(Some([aw, ah])) => {
                        crate::ui_kit::set_size_field_keeps(ui.ctx(), "webcam_crop", Some(aw as f64 / ah as f64));
                        let c = media::Crop::largest(cw, ch, aw, ah, (x as f64 + w as f64 / 2.0, y as f64 + h as f64 / 2.0));
                        *recrop = Some(to_crop(c.x, c.y, c.w, c.h));
                    }
                    None => {}
                }
            });
            // Its size: its middle stays put (with the shape kept, one side
            // follows the other).
            l.row(Row::new("Size").note("What's kept of the camera's picture"), |ui| {
                if let Some((nw, nh)) = crate::ui_kit::size_field(ui, "webcam_crop", (w, h), least.0..=cw, least.1..=ch) {
                    let at = |p: u32, s: u32, ns: u32, n: u32| (p + s / 2).saturating_sub(ns / 2).min(n.saturating_sub(ns));
                    *recrop = Some(to_crop(at(x, w, nw, cw), at(y, h, nh, ch), nw, nh));
                }
            });
            // Where it is: its top-left corner, or the middle.
            let note = if centred { "In the middle of the picture" } else { "Its top-left corner, from the picture's" };
            l.row(Row::new("Position").note(note), |ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                if Btn::normal("Centre").small().enabled(!centred).tip("Put it in the middle of the picture").show(ui).clicked() {
                    *recrop = Some(to_crop((cw - w) / 2, (ch - h) / 2, w, h));
                }
                // Left to right at the row's end, so Tab goes through them in reading order.
                ui.allocate_ui_with_layout(vec2(BOX * 2.0 + 6.0 + 16.0 + 6.0 * 3.0, crate::ui_kit::CONTROL_H), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let small = |ui: &mut egui::Ui, t: &str| ui.label(theme::text(t, 12.5, W::Regular).color(pal.text3));
                    if let Some(v) = crate::ui_kit::number_box(ui, egui::Id::new("webcam_crop_x"), x, 0..=cw - w, BOX) {
                        *recrop = Some(to_crop(v, y, w, h));
                    }
                    small(ui, ",");
                    if let Some(v) = crate::ui_kit::number_box(ui, egui::Id::new("webcam_crop_y"), y, 0..=ch - h, BOX) {
                        *recrop = Some(to_crop(x, v, w, h));
                    }
                    small(ui, "px");
                });
            });
        });
        ui.add_space(8.0);
    }
    ui.horizontal(|ui| {
        ui.add_space(2.0);
        let what = match kept {
            Some(([_, _, w, h], (cw, ch))) if cropped => format!("{w}×{h} of {cw}×{ch}"),
            Some((_, (cw, ch))) => format!("All of the picture, {cw}×{ch}"),
            None => "Waiting for the camera…".to_owned(),
        };
        ui.label(theme::text(what, 12.5, W::Regular).color(pal.text3));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if cropped && Btn::ghost("Reset").small().tip("All of the camera's picture again").show(ui).clicked() {
                *recrop = Some([0.0; 4]);
            }
        });
    });
    // Where the camera zooms itself (its Camera page), that's sharper.
    if capture::webcam::HAS_CONTROLS {
        ui.add_space(6.0);
        ui.add(egui::Label::new(theme::text("The camera's own Zoom, in Camera settings, crops inside the camera instead, from its sensor, which can be sharper.", 12.0, W::Regular).color(pal.text3)).wrap());
    }
}

/// The next exposure time up (`up`) or down from `value`, a step on.
fn exposure_step(value: i64, up: bool, step: i64, min: i64, max: i64) -> i64 {
    (value + if up { step } else { -step }).clamp(min, max)
}

/// Arrow keys on a camera slider: clicking or dragging it gives it the keys,
/// then ← and → step it, ten at a time with Shift (↑ and ↓ move between the
/// page's controls, as egui has them). How many steps, if any were pressed.
fn arrow_steps(ui: &egui::Ui, slider: &egui::Response) -> Option<i64> {
    if slider.clicked() || slider.drag_started() {
        slider.request_focus();
    }
    if !slider.has_focus() {
        return None;
    }
    let mut d = 0;
    ui.input_mut(|i| {
        let shift = i.modifiers.shift;
        let by = if shift { 10 } else { 1 };
        let mods = if shift { egui::Modifiers::SHIFT } else { egui::Modifiers::NONE };
        for (key, sign) in [(egui::Key::ArrowLeft, -1), (egui::Key::ArrowRight, 1)] {
            while i.consume_key(mods, key) {
                d += sign * by;
            }
        }
    });
    (d != 0).then_some(d)
}

/// Film's guides for an exposure time at `fps`, in V4L2's units of 100 µs:
/// the 180° shutter (half a frame, for motion blur that looks natural), and
/// the whole frame (longer, and the frame rate drops).
fn exposure_guides(fps: f32) -> (i64, i64) {
    let frame = 10_000.0 / fps as f64;
    ((frame / 2.0).round() as i64, frame.round() as i64)
}

/// The exposure time: − and + (or the arrow keys) to step it finely, to
/// where lights' flicker leaves no bands, found by eye; the slider; its time in ms.
fn exposure_widget(ui: &mut egui::Ui, c: &CameraControl, set: &mut Vec<(u32, i64)>) {
    use crate::theme::{self, W};
    if let ControlKind::Menu(times) = &c.kind {
        return exposure_steps(ui, c, times, set);
    }
    let ControlKind::Int { min, max, step } = c.kind else { return control_widget(ui, c, set) };
    let pal = theme::pal(ui);
    // Right to left: the time, +, the slider, −.
    ui.add_sized([56.0, 20.0], egui::Label::new(theme::text(&format!("{:.1} ms", c.value as f64 / 10.0), 12.5, W::Regular).color(pal.text3)));
    let next = |up: bool| Some(exposure_step(c.value, up, step, min, max)).filter(|v| *v != c.value);
    if crate::ui_kit::Btn::icon(crate::ui_kit::Kind::Ghost, egui_phosphor::regular::PLUS).small().tip("Longer").show(ui).clicked()
        && let Some(v) = next(true)
    {
        set.push((c.id, v));
    }
    // On a curve: the times worth having get the slider's room, not the
    // shortest (linear squeezed them into its first fifth; stops gave the
    // too-dark ones as much as the rest).
    let mut v = curve_pos(c.value, min, max);
    let slider = crate::transport::slider(ui, &mut v, 130.0);
    if slider.changed() {
        let to = curve_value(v, min, max);
        if to != c.value {
            set.push((c.id, to));
        }
    }
    if let Some(d) = arrow_steps(ui, &slider) {
        let to = (c.value + d * step).clamp(min, max);
        if to != c.value {
            set.push((c.id, to));
        }
    }
    if crate::ui_kit::Btn::icon(crate::ui_kit::Kind::Ghost, egui_phosphor::regular::MINUS).small().tip("Shorter").show(ui).clicked()
        && let Some(v) = next(false)
    {
        set.push((c.id, v));
    }
}

/// The exposure time as a slider over the few times a camera has (Windows:
/// every doubling, as its driver offers them; no finer time can be set),
/// as the slider over a continuous one looks: the time, +, the slider, −.
fn exposure_steps(ui: &mut egui::Ui, c: &CameraControl, times: &[(i64, String)], set: &mut Vec<(u32, i64)>) {
    use crate::theme::{self, W};
    let pal = theme::pal(ui);
    let last = times.len().saturating_sub(1);
    let at = times.iter().position(|(v, _)| *v == c.value);
    let label = at.map_or_else(|| c.value.to_string(), |i| times[i].1.clone());
    ui.add_sized([56.0, 20.0], egui::Label::new(theme::text(&label, 12.5, W::Regular).color(pal.text3)));
    let mut go = |i: usize| {
        if Some(i) != at
            && let Some((v, _)) = times.get(i)
        {
            set.push((c.id, *v));
        }
    };
    let i = at.unwrap_or(0);
    if crate::ui_kit::Btn::icon(crate::ui_kit::Kind::Ghost, egui_phosphor::regular::PLUS).small().enabled(i < last).tip("Longer").show(ui).clicked() {
        go((i + 1).min(last));
    }
    let mut v = if last == 0 { 0.0 } else { i as f32 / last as f32 };
    let slider = crate::transport::stepped_slider(ui, &mut v, 130.0, last as u32);
    if slider.changed() {
        go((v * last as f32).round() as usize);
    }
    if let Some(d) = arrow_steps(ui, &slider) {
        go((i as i64 + d).clamp(0, last as i64) as usize);
    }
    if crate::ui_kit::Btn::icon(crate::ui_kit::Kind::Ghost, egui_phosphor::regular::MINUS).small().enabled(i > 0).tip("Shorter").show(ui).clicked() {
        go(i.saturating_sub(1));
    }
    slider.on_hover_text("Each step doubles the time");
}

/// How steep the exposure slider's curve is: near linear at the bottom (the
/// shortest times, too dark indoors, get little room), each step up longer.
/// 5 puts a C922's 0.3–205 ms at: 1 ms 8% along, 16.7 ms half way, 100 ms 85%.
const EXPOSURE_CURVE: f64 = 5.0;

/// Where `value` sits along the exposure slider from `min` to `max` (0 to 1).
fn curve_pos(value: i64, min: i64, max: i64) -> f32 {
    let t = (value - min) as f64 / (max - min).max(1) as f64;
    ((t * (EXPOSURE_CURVE.exp() - 1.0)).ln_1p() / EXPOSURE_CURVE).clamp(0.0, 1.0) as f32
}

/// The value at `pos` (0 to 1) along the exposure slider from `min` to `max`.
fn curve_value(pos: f32, min: i64, max: i64) -> i64 {
    let t = (EXPOSURE_CURVE * pos as f64).exp_m1() / EXPOSURE_CURVE.exp_m1();
    (min + (t * (max - min) as f64).round() as i64).clamp(min, max)
}

/// The Camera page's still side: the picture, which camera it is and in what
/// size, and resetting its settings.
#[allow(clippy::too_many_arguments)]
fn picture_side(ui: &mut egui::Ui, panel: &mut CameraPanel, name: &str, fps: Option<f32>, orient: Orientation, crop: [f32; 4], max_h: f32, reset: &mut bool) {
    use crate::theme::{self, W};
    let pal = theme::pal(ui);
    picture_view(ui, panel, orient, crop, max_h, false);
    crate::ui_kit::gap(ui, crate::ui_kit::STACK_GAP);
    let status = match (capture::webcam::status_of(&panel.device), panel.fps) {
        (capture::webcam::Status::Live { width, height }, Some(fps)) => format!("{name} · {width}×{height} · {fps:.0} new pictures/s"),
        (capture::webcam::Status::Live { width, height }, None) => format!("{name} · {width}×{height}"),
        _ => name.to_owned(),
    };
    // Which camera, on a line of its own: a long name wraps, not under the buttons.
    ui.add(egui::Label::new(theme::text(&status, 13.0, W::Regular).color(pal.text2)).wrap());
    // Slower than its format: said, with what can cause it (the camera
    // doesn't say which).
    if let Some(why) = slowdown(panel.fps, fps) {
        ui.add(egui::Label::new(theme::text(why, 12.5, W::Regular).color(pal.warn)).wrap());
    }
    ui.add(egui::Label::new(theme::text("Changes apply right away, in every app, and are put back each time HasteClip opens this camera.", 12.0, W::Regular).color(pal.text3)).wrap());
    // What's done to all of them, under what's said about them.
    let resettable = !panel.controls.is_empty();
    if resettable || capture::webcam::HAS_SETTINGS {
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
            if resettable && crate::ui_kit::Btn::normal("Reset to defaults").small().tip("Back to the camera's own defaults").show(ui).clicked() {
                *reset = true;
            }
            // What only its driver has, in the driver's own window.
            if capture::webcam::HAS_SETTINGS && crate::ui_kit::Btn::normal("Driver settings…").small().tip("The camera driver's own settings window, for anything not here").show(ui).clicked() {
                capture::webcam::open_settings(&panel.device, name);
            }
        });
    }
}

/// When the camera sends clearly fewer new pictures (`measured` a second)
/// than its format's `fps`: saying so, and what can cause it. Which one, the
/// camera doesn't say, and its own limits aren't a frame's length (a C922 at
/// 60 fps kept up at 17.2 ms and slowed at 16.8 in dimmer light).
/// How many new pictures (not the camera repeating one, as it does with a
/// long exposure) the camera sent in the last second, measured into
/// `panel`, without showing its picture (one a few pixels wide is asked for).
fn measure_rate(ctx: &egui::Context, panel: &mut CameraPanel) {
    if let Some(p) = capture::webcam::camera_picture(&panel.device, 8, panel.seq) {
        panel.seq = p.seq;
        panel.new_pictures = p.new_pictures;
    }
    let now = std::time::Instant::now();
    match panel.counted {
        Some((at, from)) if now.duration_since(at).as_secs_f32() >= 1.0 => {
            panel.fps = Some((panel.new_pictures.saturating_sub(from)) as f32 / now.duration_since(at).as_secs_f32());
            panel.counted = Some((now, panel.new_pictures));
        }
        None if panel.new_pictures > 0 => panel.counted = Some((now, panel.new_pictures)),
        _ => {}
    }
    ctx.request_repaint_after(std::time::Duration::from_millis(250));
}

fn slowdown(measured: Option<f32>, fps: Option<f32>) -> Option<String> {
    let (measured, fps) = (measured?, fps?);
    (measured < fps * 0.85).then(|| {
        format!("Sending {measured:.0} of {fps:.0} pictures a second. Cameras slow down when too little light reaches them (brighter light in front of you helps; gain doesn't), or when the exposure is longer than they can fit in a frame.")
    })
}

/// The camera's own picture, live, as wide as there's room for and at most
/// `max_h` tall: cropped, or (on the crop's page) all of it, the crop
/// dragged on it, its shape kept with Shift or `keep`. The new crop, when
/// it's dragged.
fn picture_view(ui: &mut egui::Ui, panel: &mut CameraPanel, orient: Orientation, crop: [f32; 4], max_h: f32, keep: bool) -> Option<[f32; 4]> {
    let pal = crate::theme::pal(ui);
    let w = ui.available_width();
    if let Some(p) = capture::webcam::camera_picture(&panel.device,(w * ui.ctx().pixels_per_point()) as u32, panel.seq) {
        let image = egui::ColorImage::from_rgba_unmultiplied([p.width as usize, p.height as usize], &p.rgba);
        match &mut panel.picture {
            Some(t) if t.size() == [p.width as usize, p.height as usize] => t.set(image, egui::TextureOptions::LINEAR),
            t => *t = Some(ui.ctx().load_texture("camera page", image, egui::TextureOptions::LINEAR)),
        }
        panel.seq = p.seq;
        panel.new_pictures = p.new_pictures;
    }
    // How many new pictures (not the camera repeating one, as it does with a
    // long exposure) came in the last second: its real frame rate.
    let now = std::time::Instant::now();
    match panel.counted {
        Some((at, from)) if now.duration_since(at).as_secs_f32() >= 1.0 => {
            panel.fps = Some((panel.new_pictures.saturating_sub(from)) as f32 / now.duration_since(at).as_secs_f32());
            panel.counted = Some((now, panel.new_pictures));
        }
        None if panel.new_pictures > 0 => panel.counted = Some((now, panel.new_pictures)),
        _ => {}
    }
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
    // As it is in the clips: turned, then mirrored or flipped; cropped,
    // unless the crop's being set (then all of it, the rest dimmed).
    let raw = panel.picture.as_ref().map_or(16.0 / 9.0, |t| t.size()[0] as f32 / t.size()[1].max(1) as f32);
    let sideways = orient.turns % 2 == 1;
    let full_aspect = if sideways { 1.0 / raw } else { raw };
    // The crop, as shown: its sides along and down the shown picture (0 to 1).
    let shown = crop_shown(orient, crop);
    let aspect = if panel.cropping { full_aspect } else { full_aspect * shown.width() / shown.height().max(0.01) };
    let pw = w.min(max_h.max(120.0) * aspect);
    let mut changed = None;
    ui.vertical_centered(|ui| {
        let sense = if panel.cropping { egui::Sense::click_and_drag() } else { egui::Sense::hover() };
        let (rect, resp) = ui.allocate_exact_size(vec2(pw, pw / aspect), sense);
        let Some(t) = &panel.picture else {
            ui.painter().rect_filled(rect, crate::ui_kit::RADIUS, pal.control);
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "Waiting for the camera…", egui::FontId::proportional(13.0), pal.text3);
            return;
        };
        // The whole picture's place: the rect itself while cropping, else as
        // much bigger as puts the crop on the rect (drawn clipped to it).
        let full = if panel.cropping {
            rect
        } else {
            let fw = rect.width() / shown.width().max(0.01);
            let fh = rect.height() / shown.height().max(0.01);
            egui::Rect::from_min_size(egui::pos2(rect.left() - shown.left() * fw, rect.top() - shown.top() * fh), vec2(fw, fh))
        };
        // Drawn unturned, then turned about its middle; the mirror and flip
        // are the clip's (after turning), so sideways they swap axes.
        let unturned = if sideways { egui::Rect::from_center_size(full.center(), vec2(full.height(), full.width())) } else { full };
        let (fu, fv) = if sideways { (orient.flip_v, orient.flip_h) } else { (orient.flip_h, orient.flip_v) };
        let uv = egui::Rect::from_min_max(egui::pos2(if fu { 1.0 } else { 0.0 }, if fv { 1.0 } else { 0.0 }), egui::pos2(if fu { 0.0 } else { 1.0 }, if fv { 0.0 } else { 1.0 }));
        let mut image = egui::Image::from_texture((t.id(), unturned.size())).uv(uv);
        if orient.turns % 4 != 0 {
            image = image.rotate(orient.turns as f32 * std::f32::consts::FRAC_PI_2, vec2(0.5, 0.5));
        }
        let mut clipped = ui.new_child(egui::UiBuilder::new().max_rect(rect));
        clipped.set_clip_rect(rect.intersect(ui.clip_rect()));
        image.paint_at(&mut clipped, unturned);
        if !panel.cropping {
            return;
        }
        // The crop on the whole picture: the rest dimmed, its edges dragged.
        let unit = egui::Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
        if let Some(s) = crop_editor(ui, &resp, rect, unit, shown, vec2(0.1, 0.1), keep, &mut panel.crop_drag) {
            changed = Some(crop_from_shown(orient, s));
        }
    });
    changed
}

/// A crop being set on a picture drawn at `area`: `bounds` is all of the
/// picture and `kept` the part kept, in the same units (fractions of it, or
/// its pixels). Drawn over it with the rest dimmed; its corners, edges or
/// middle dragged (Shift, or `keep`, keeps its shape), down to `least` across
/// and down.
/// `drag`: what's held, kept from frame to frame. The new part kept, when
/// it's dragged.
pub(crate) fn crop_editor(ui: &egui::Ui, resp: &egui::Response, area: Rect, bounds: Rect, kept: Rect, least: egui::Vec2, keep: bool, drag: &mut Option<(Handle, Rect, Pos2)>) -> Option<Rect> {
    let scale = vec2(area.width() / bounds.width().max(1e-6), area.height() / bounds.height().max(1e-6));
    let at = |n: Pos2| area.min + (n - bounds.min) * scale;
    let c = Rect::from_min_max(at(kept.min), at(kept.max));
    let dim = Color32::from_black_alpha(150);
    let p = ui.painter();
    p.rect_filled(Rect::from_min_max(area.min, egui::pos2(area.right(), c.top())), 0.0, dim);
    p.rect_filled(Rect::from_min_max(egui::pos2(area.left(), c.bottom()), area.max), 0.0, dim);
    p.rect_filled(Rect::from_min_max(egui::pos2(area.left(), c.top()), egui::pos2(c.left(), c.bottom())), 0.0, dim);
    p.rect_filled(Rect::from_min_max(egui::pos2(c.right(), c.top()), egui::pos2(area.right(), c.bottom())), 0.0, dim);
    crop_outline(p, c, true);
    // What's near the pointer: a corner, else an edge, else (inside) all of it.
    let grip_at = |at: Pos2| -> Option<Handle> {
        let near = 12.0;
        let corner = [Handle::NW, Handle::NE, Handle::SW, Handle::SE].into_iter().find(|h| h.pos(c).distance(at) < near);
        corner.or_else(|| {
            let between_y = at.y > c.top() && at.y < c.bottom();
            let between_x = at.x > c.left() && at.x < c.right();
            if between_y && (at.x - c.left()).abs() < near {
                Some(Handle::W)
            } else if between_y && (at.x - c.right()).abs() < near {
                Some(Handle::E)
            } else if between_x && (at.y - c.top()).abs() < near {
                Some(Handle::N)
            } else if between_x && (at.y - c.bottom()).abs() < near {
                Some(Handle::S)
            } else {
                c.contains(at).then_some(Handle::Move)
            }
        })
    };
    let hover = resp.hover_pos().and_then(grip_at);
    if resp.drag_started() {
        let origin = ui.input(|i| i.pointer.press_origin()).or(resp.interact_pointer_pos());
        *drag = origin.and_then(|at| grip_at(at).map(|g| (g, kept, at)));
    }
    if let Some(g) = drag.map(|(g, _, _)| g).or(hover) {
        ui.ctx().set_cursor_icon(if g == Handle::Move && drag.is_some() { CursorIcon::Grabbing } else { g.cursor() });
    }
    let mut out = None;
    if let (Some((g, start, from)), true, Some(now)) = (*drag, resp.dragged(), resp.interact_pointer_pos()) {
        let d = vec2((now.x - from.x) / scale.x, (now.y - from.y) / scale.y);
        out = Some(if g == Handle::Move {
            start.translate(vec2(d.x.clamp(bounds.min.x - start.min.x, bounds.max.x - start.max.x), d.y.clamp(bounds.min.y - start.min.y, bounds.max.y - start.max.y)))
        } else {
            // Shift keeps its shape.
            resize_within(start, g.sides(), d, keep || ui.input(|i| i.modifiers.shift), bounds, least, None)
        });
    }
    if resp.drag_stopped() {
        *drag = None;
    }
    out
}

/// A crop's outline on the picture, `c`, with its grips (edge bars, corner
/// brackets) when it can be dragged: all on the one line, its edge.
pub(crate) fn crop_outline(p: &egui::Painter, c: Rect, grips: bool) {
    p.rect_stroke(c, 0.0, Stroke::new(1.5, Color32::WHITE), egui::StrokeKind::Middle);
    if !grips {
        return;
    }
    let thick = Stroke::new(4.0, Color32::WHITE);
    let bar = 12.0_f32.min(c.width() / 4.0).min(c.height() / 4.0);
    for (mid, along) in [(c.left_center(), vec2(0.0, bar)), (c.right_center(), vec2(0.0, bar)), (c.center_top(), vec2(bar, 0.0)), (c.center_bottom(), vec2(bar, 0.0))] {
        p.line_segment([mid - along, mid + along], thick);
    }
    // Brackets from each corner along its two edges, squared off where they meet.
    let arm = 16.0_f32.min(c.width() / 3.0).min(c.height() / 3.0);
    for (corner, sx, sy) in [(c.left_top(), 1.0, 1.0), (c.right_top(), -1.0, 1.0), (c.left_bottom(), 1.0, -1.0), (c.right_bottom(), -1.0, -1.0)] {
        let out = vec2(-sx, -sy) * thick.width / 2.0;
        p.line_segment([corner + vec2(out.x, 0.0), corner + vec2(sx * arm, 0.0)], thick);
        p.line_segment([corner + vec2(0.0, out.y), corner + vec2(0.0, sy * arm)], thick);
    }
}

/// The crop (left, top, right, bottom cut off) as the part of the picture
/// as shown that's kept. A placement's crop is of the turned picture before
/// it's mirrored or flipped (as the recording draws it, `Canvas::draw_camera`,
/// and as turning moves it round, `Placement::turn`): shown, a mirrored
/// picture's left is the crop's right.
fn crop_shown(o: Orientation, [l, t, r, b]: [f32; 4]) -> egui::Rect {
    let (left, right) = if o.flip_h { (r, l) } else { (l, r) };
    let (top, bottom) = if o.flip_v { (b, t) } else { (t, b) };
    egui::Rect::from_min_max(egui::pos2(left, top), egui::pos2(1.0 - right, 1.0 - bottom))
}

/// The crop that keeps this part of the picture as shown.
fn crop_from_shown(o: Orientation, s: egui::Rect) -> [f32; 4] {
    let (left, right) = (s.min.x.max(0.0), (1.0 - s.max.x).max(0.0));
    let (top, bottom) = (s.min.y.max(0.0), (1.0 - s.max.y).max(0.0));
    let (l, r) = if o.flip_h { (right, left) } else { (left, right) };
    let (t, b) = if o.flip_v { (bottom, top) } else { (top, bottom) };
    [l, t, r, b]
}

/// How the webcam is turned (quarter turns clockwise) and mirrored or
/// flipped in the clips: its placement's.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Orientation {
    flip_h: bool,
    flip_v: bool,
    turns: u8,
}

impl Orientation {
    /// Quarter turns clockwise as you see it: a mirrored picture's turns go
    /// the other way under its mirror (`Placement::turn`).
    fn seen_turns(self) -> u8 {
        let t = self.turns % 4;
        if self.flip_h != self.flip_v { (4 - t) % 4 } else { t }
    }
}

/// The webcam's orientation in the clips, after its format: the same as the
/// preview's right-click. What's wanted goes in `reorient`, its `turns` as
/// seen (`Orientation::seen_turns`). Its crop is on its card, as on every
/// system.
fn orientation_view(ui: &mut egui::Ui, now: Orientation, reorient: &mut Option<Orientation>) {
    use crate::ui_kit::{Item, Row};
    crate::ui_kit::section_head(ui, "Orientation", false, |_| {});
    crate::ui_kit::list(ui, "camera_orientation", |l| {
        l.row(Row::new("Mirror").note("Left and right swapped, in your clips too"), |ui| {
            let mut on = now.flip_h;
            if crate::ui_kit::switch(ui, &mut on).changed() {
                *reorient = Some(Orientation { flip_h: on, turns: now.seen_turns(), ..now });
            }
        });
        l.row(Row::new("Upside down"), |ui| {
            let mut on = now.flip_v;
            if crate::ui_kit::switch(ui, &mut on).changed() {
                *reorient = Some(Orientation { flip_v: on, turns: now.seen_turns(), ..now });
            }
        });
        l.row(Row::new("Rotation"), |ui| {
            let label = |t: u8| format!("{}°", t as u32 % 4 * 90);
            let seen = now.seen_turns();
            let r = crate::ui_kit::select(ui, &label(seen), 150.0);
            crate::ui_kit::menu_popup(&r, false, 160.0, |ui| {
                for t in 0..4u8 {
                    if Item::new(label(t)).check(seen == t).show(ui).clicked() && seen != t {
                        *reorient = Some(Orientation { turns: t, ..now });
                    }
                }
            });
        });
    });
}

/// The camera's format, first on the Camera page: its resolution, then the
/// frame rates that size comes at (once one is picked). What's picked goes in `pick`.
fn format_view(ui: &mut egui::Ui, device: &str, current: Option<capture::webcam::Format>, pick: &mut Option<Option<capture::webcam::Format>>) {
    use crate::ui_kit::{Item, Row};
    let formats = capture::webcam::formats(device);
    crate::ui_kit::section_head(ui, "Format", true, |_| {});
    crate::ui_kit::list(ui, "camera_format", |l| {
        l.row(Row::new("Resolution"), |ui| {
            let label = current.map_or_else(|| "Automatic".to_owned(), |f| size_label(f.width, f.height));
            let r = crate::ui_kit::select(ui, &label, 150.0);
            crate::ui_kit::menu_popup(&r, false, 250.0, |ui| {
                egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                    if Item::new("Automatic").check(current.is_none()).tip("The sharpest picture up to 1080p at 30 fps or more").show(ui).clicked() {
                        *pick = Some(None);
                    }
                    if formats.is_empty() {
                        crate::ui_kit::menu_head(ui, "The camera's sizes show once it's open.");
                    }
                    for (w, h) in sizes(&formats) {
                        let on = current.is_some_and(|f| (f.width, f.height) == (w, h));
                        // How fast it goes at most: some sizes only come slowly.
                        let fastest = formats.iter().filter(|f| (f.width, f.height) == (w, h)).map(|f| f.fps()).fold(0.0, f32::max);
                        if Item::new(size_label(w, h)).value(format!("up to {}", fps_label(fastest))).check(on).show(ui).clicked() && !on {
                            *pick = Some(rate_for(&formats, (w, h), current));
                        }
                    }
                });
            });
        });
        if let Some(f) = current {
            l.row(Row::new("Frame rate"), |ui| {
                let r = crate::ui_kit::select(ui, &fps_label(f.fps()), 150.0);
                crate::ui_kit::menu_popup(&r, false, 200.0, |ui| {
                    egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                        for o in formats.iter().filter(|o| (o.width, o.height) == (f.width, f.height)) {
                            if Item::new(fps_label(o.fps())).check(*o == f).show(ui).clicked() && *o != f {
                                *pick = Some(Some(*o));
                            }
                        }
                    });
                });
            });
        }
    });
}

/// The camera's settings in groups, each a card of rows. What's changed goes in `set`.
fn controls_view(ui: &mut egui::Ui, panel: &CameraPanel, fps: Option<f32>, set: &mut Vec<(u32, i64)>) {
    use crate::theme::{self, W};
    let pal = theme::pal(ui);
    if panel.controls.is_empty() {
        ui.label(theme::text("This camera has no settings to change, or they couldn't be read.", 13.0, W::Regular).color(pal.text3));
    }
    for (heading, rows) in panel_groups(&panel.controls) {
        crate::ui_kit::section_head(ui, heading, false, |_| {});
        crate::ui_kit::list(ui, ("camera", heading), |l| {
            for r in &rows {
                match r {
                    PanelRow::Control { label, control } => {
                        l.row(crate::ui_kit::Row::new(label), |ui| control_widget(ui, control, set));
                    }
                    PanelRow::Auto { label, auto, manual } => {
                        let on = auto_on(auto);
                        l.row(crate::ui_kit::Row::new(label), |ui| {
                            let mut v = on;
                            if crate::ui_kit::switch(ui, &mut v).changed() {
                                set.push((auto.id, auto_value(auto, v)));
                            }
                        });
                        // The setting it takes over, while it's off.
                        if let Some((label, m)) = manual {
                            l.well(("camera_manual", auto.id), !on, |l| {
                                if m.id == cid::EXPOSURE {
                                    let tip = fps.map(|fps| {
                                        let (half, whole) = exposure_guides(fps);
                                        // Windows has only the doublings: Flicker does the rest.
                                        let lamps = if matches!(m.kind, ControlKind::Menu(_)) {
                                            "Under lamps, step it with − and + (or ← and →) to where no bands roll through the picture; Windows only has every doubling of the time, so setting Flicker to your mains frequency may be what does it."
                                        } else {
                                            "Under lamps, step it with − and + (or ← and →) to where no bands roll through the picture."
                                        };
                                        format!(
                                            "How long each picture takes in light. Film's rule of thumb is half a frame, a 180° shutter: {:.1} ms at {fps:.0} fps, for motion blur that looks natural. About a whole frame ({:.1} ms) or longer, a camera can't keep its frame rate; where exactly is the camera's own, as is how much light it wants (watch the pictures a second under the picture). {lamps}",
                                            half as f64 / 10.0,
                                            whole as f64 / 10.0
                                        )
                                    });
                                    let row = crate::ui_kit::Row::new(label).kid();
                                    let row = match &tip {
                                        Some(t) => row.tip("Exposure time", t),
                                        None => row,
                                    };
                                    l.row(row, |ui| exposure_widget(ui, m, set));
                                } else {
                                    let row = crate::ui_kit::Row::new(label).kid();
                                    let row = if m.id == cid::EXPOSURE_STEPS { row.tip("Exposure", "In the camera's own steps: its driver doesn't say how long they are. Higher is longer, and brighter.") } else { row };
                                    l.row(row, |ui| control_widget(ui, m, set));
                                }
                            });
                        }
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod panel_tests {
    use super::*;

    fn ctl(id: u32, name: &str, kind: ControlKind, value: i64) -> CameraControl {
        CameraControl { id, name: name.into(), kind, value, default: value, inactive: false }
    }

    /// A Logitech C922's controls: named and grouped, automatic modes paired
    /// with the setting they take over, the rest under More.
    /// Half a frame and a whole one: at 30 fps 16.7 and 33.3 ms, at 60 fps 8.3 and 16.7 ms.
    /// On a C922's 0.3–204.7 ms: 1 ms near the start, 16.7 ms (a frame at
    /// 60 fps) half way, 100 ms near the end; and back again.
    #[test]
    fn exposure_slider_curve() {
        let at = |ms: i64| curve_pos(ms * 10, 3, 2047);
        assert!(at(1) < 0.1, "{}", at(1));
        assert!((0.45..0.56).contains(&at(17)), "{}", at(17));
        assert!((0.8..0.9).contains(&at(100)), "{}", at(100));
        assert_eq!(curve_value(curve_pos(167, 3, 2047), 3, 2047), 167);
        assert_eq!(curve_value(0.0, 3, 2047), 3);
        assert_eq!(curve_value(1.0, 3, 2047), 2047);
    }

    /// Only clearly fewer than the format's rate is a slowdown, said with
    /// both causes (which one, the camera doesn't say).
    #[test]
    fn says_when_the_camera_slows() {
        assert_eq!(slowdown(Some(58.0), Some(60.0)), None);
        let said = slowdown(Some(30.0), Some(60.0)).unwrap();
        assert!(said.starts_with("Sending 30 of 60") && said.contains("light") && said.contains("exposure"));
        assert_eq!(slowdown(None, Some(60.0)), None);
    }

    /// A crop set on the picture as shown is the same crop shown again, in
    /// every orientation; mirrored, the shown left is the crop's right (as
    /// the recording draws it).
    #[test]
    fn crop_through_every_orientation() {
        let kept = egui::Rect::from_min_max(egui::pos2(0.1, 0.2), egui::pos2(0.6, 0.9));
        for turns in 0..4u8 {
            for (flip_h, flip_v) in [(false, false), (true, false), (false, true), (true, true)] {
                let o = Orientation { flip_h, flip_v, turns };
                let again = crop_shown(o, crop_from_shown(o, kept));
                assert!((again.min - kept.min).length() < 1e-5 && (again.max - kept.max).length() < 1e-5, "{o:?}");
            }
        }
        let close = |c: [f32; 4], want: [f32; 4]| c.iter().zip(want).all(|(a, b)| (a - b).abs() < 1e-5);
        assert!(close(crop_from_shown(Orientation::default(), kept), [0.1, 0.2, 0.4, 0.1]));
        let mirrored = Orientation { flip_h: true, ..Default::default() };
        assert!(close(crop_from_shown(mirrored, kept), [0.4, 0.2, 0.1, 0.1]));
    }

    #[test]
    fn exposure_guides_by_frame_rate() {
        assert_eq!(exposure_guides(30.0), (167, 333));
        assert_eq!(exposure_guides(60.0), (83, 167));
    }

    #[test]
    fn groups_a_webcams_controls() {
        use cid::*;
        let int = ControlKind::Int { min: 0, max: 255, step: 1 };
        let menu = |items: &[(i64, &str)]| ControlKind::Menu(items.iter().map(|(v, n)| (*v, n.to_string())).collect());
        let controls = vec![
            ctl(BRIGHTNESS, "Brightness", int.clone(), 128),
            ctl(AUTO_WHITE_BALANCE, "White Balance, Automatic", ControlKind::Bool, 1),
            ctl(POWER_LINE, "Power Line Frequency", menu(&[(0, "Disabled"), (1, "50 Hz"), (2, "60 Hz")]), 2),
            ctl(WHITE_BALANCE, "White Balance Temperature", ControlKind::Int { min: 2000, max: 6500, step: 1 }, 4000),
            ctl(AUTO_EXPOSURE, "Auto Exposure", menu(&[(1, "Manual Mode"), (3, "Aperture Priority Mode")]), 3),
            ctl(EXPOSURE, "Exposure Time, Absolute", ControlKind::Int { min: 3, max: 2047, step: 1 }, 250),
            ctl(0x9a0921, "Something New", int.clone(), 0),
        ];
        let groups = panel_groups(&controls);
        let headings: Vec<_> = groups.iter().map(|(h, _)| *h).collect();
        assert_eq!(headings, ["Image", "Exposure", "White balance", OTHER]);
        let exposure = &groups[1].1;
        assert!(matches!(exposure[0], PanelRow::Auto { label: "Automatic exposure", manual: Some(("Exposure time", _)), .. }));
        assert!(matches!(&exposure[1], PanelRow::Control { label, .. } if label == "Flicker (mains frequency)"));
        assert!(matches!(&groups[3].1[0], PanelRow::Control { label, .. } if label == "Something New"));
        // Aperture priority is automatic; turning it off picks manual, on picks the default.
        let ae = &controls[4];
        assert!(auto_on(ae));
        assert_eq!(auto_value(ae, false), cid::EXPOSURE_MANUAL);
        assert_eq!(auto_value(ae, true), 3);
        // An exposure in the camera's own steps (no time): under automatic
        // exposure too, as "Exposure".
        let steps = vec![
            ctl(AUTO_EXPOSURE, "Auto Exposure", menu(&[(1, "Manual Mode"), (3, "Aperture Priority Mode")]), 3),
            ctl(cid::EXPOSURE_STEPS, "Exposure", ControlKind::Int { min: 0, max: 255, step: 1 }, 120),
        ];
        let groups = panel_groups(&steps);
        assert!(matches!(groups[0].1[0], PanelRow::Auto { manual: Some(("Exposure", _)), .. }), "{:?}", groups.iter().map(|(h, _)| *h).collect::<Vec<_>>());
        assert_eq!(groups.len(), 1, "nothing under Other");
    }
}

/// The sizes a camera offers, each once, biggest first.
pub(crate) fn sizes(formats: &[capture::webcam::Format]) -> Vec<(u32, u32)> {
    let mut out: Vec<(u32, u32)> = Vec::new();
    for f in formats {
        if !out.contains(&(f.width, f.height)) {
            out.push((f.width, f.height));
        }
    }
    out.sort_by_key(|(w, h)| std::cmp::Reverse((w * h, *w)));
    out
}

/// The format to take at size `size`: the rate it had, if offered there,
/// else the fastest.
pub(crate) fn rate_for(formats: &[capture::webcam::Format], size: (u32, u32), current: Option<capture::webcam::Format>) -> Option<capture::webcam::Format> {
    let at = formats.iter().filter(|f| (f.width, f.height) == size);
    let same = current.and_then(|c| at.clone().find(|f| (f.fps() - c.fps()).abs() < 0.05));
    same.or_else(|| at.max_by(|a, b| a.fps().total_cmp(&b.fps()))).copied()
}

/// A size: "1920×1080".
pub(crate) fn size_label(w: u32, h: u32) -> String {
    format!("{w}×{h}")
}

pub(crate) fn fps_label(fps: f32) -> String {
    if (fps - fps.round()).abs() < 0.05 { format!("{} fps", fps.round()) } else { format!("{fps:.2} fps") }
}

#[cfg(test)]
mod format_tests {
    use super::*;
    use capture::webcam::Format;

    fn f(width: u32, height: u32, fps: u32) -> Format {
        Format { width, height, fps_num: fps, fps_den: 1 }
    }

    #[test]
    fn sizes_once_biggest_first_and_rates_kept() {
        let formats = [f(640, 480, 30), f(1920, 1080, 30), f(1920, 1080, 60), f(640, 480, 15), f(1280, 720, 60)];
        assert_eq!(sizes(&formats), [(1920, 1080), (1280, 720), (640, 480)]);
        // 30 fps is kept where the new size has it; else the fastest.
        assert_eq!(rate_for(&formats, (640, 480), Some(f(1920, 1080, 30))), Some(f(640, 480, 30)));
        assert_eq!(rate_for(&formats, (1280, 720), Some(f(1920, 1080, 30))), Some(f(1280, 720, 60)));
        assert_eq!(size_label(1920, 1080), "1920×1080");
        // As `Format::label`: within 0.05 of a whole number reads as it.
        assert_eq!(fps_label(29.97), "30 fps");
        assert_eq!(fps_label(7.5), "7.50 fps");
    }
}
