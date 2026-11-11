//! Cropping the picture in the editor: keeping a part of it, of a shape (16:9
//! out of an ultrawide screen, a square, 9:16 for phones) or of any.
//!
//! While the tool is open the whole picture shows, what's cut off dimmed:
//! the kept part's corners size it (of any shape, its sides too), and
//! dragging inside it moves it. The rest of the time the editor shows just
//! the kept part, as the saved clip will be. The crop is an export setting
//! (`media::Output::crop`): saved with the edit, and undone like one.

use egui::{Color32, CursorIcon, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2, pos2, vec2};
use media::Crop;

use crate::theme::{self, W};

/// The tool's bar, above the picture.
const BAR_H: f32 = 48.0;
/// Room around the picture for the corners.
const PAD: f32 = 16.0;
/// How near a corner or side the pointer takes hold of it, in points.
const REACH: f32 = 10.0;
/// Moved within this many points of the picture's middle, a crop snaps to it.
const SNAP: f32 = 8.0;
/// The biggest the whole picture is drawn, in pixels a side: the graphics
/// card draws nothing bigger (egui-wgpu asks for 8192), and a small crop
/// shown big takes the whole picture bigger still. Past that, the crop is
/// shown smaller.
const MAX_PX: f32 = 8000.0;

/// What a drag holds: inside the crop (to move it), or a corner or side (to
/// size it): `x` is -1 for the left, 1 for the right and 0 for neither, `y`
/// the same for the top and bottom.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Grip {
    Move,
    Size { x: i8, y: i8 },
}

/// The crop tool, while it's open.
pub struct CropTool {
    /// The shape kept while sizing (an aspect ratio); `None`: any.
    shape: Option<[u32; 2]>,
    /// While dragging: what's held, the crop then, and where the pointer
    /// took hold.
    drag: Option<(Grip, Crop, Pos2)>,
}

/// What the tool did in a frame.
#[derive(Default)]
pub struct Did {
    /// A drag took hold of the crop: what it changes is one step to undo...
    pub took_hold: bool,
    /// ...once it lets go.
    pub let_go: bool,
    /// The bar changed the crop: a shape, or the whole picture again.
    pub changed: bool,
    /// Done: close the tool.
    pub done: bool,
}

impl CropTool {
    /// Opened on `crop`: its shape, if it's a common one.
    pub fn new(crop: Option<Crop>) -> Self {
        let common = crop.and_then(|c| crate::ui_kit::common_ratio(c.w, c.h)).map(|i| [crate::ui_kit::RATIOS[i].0, crate::ui_kit::RATIOS[i].1]);
        Self { shape: common, drag: None }
    }

    /// The tool in `rect` (where the editor's picture goes): its bar along
    /// the top and the whole picture under it (`size` as the player gives
    /// it, drawn by `paint`), the crop over that. `frame`: the recording's
    /// size in pixels.
    pub fn ui(&mut self, ui: &mut egui::Ui, rect: Rect, size: Option<Vec2>, frame: (u32, u32), crop: &mut Option<Crop>, paint: impl FnOnce(&egui::Painter, Rect)) -> Did {
        let mut did = Did::default();
        ui.painter().rect_filled(rect, 0, theme::pal(ui).stage);
        let area = Rect::from_min_max(rect.min + vec2(PAD, BAR_H), rect.max - vec2(PAD, PAD));
        // No light around the picture: a tool's plain ground.
        if let Some(pic) = crate::transport::stage(ui, area, size, |_, _| false, |r| paint(ui.painter(), r)) {
            self.picture(ui, pic, frame, crop, &mut did);
        }
        self.bar(ui, Rect::from_min_size(rect.min, vec2(rect.width(), BAR_H)), frame, crop, &mut did);
        did
    }

    /// The crop over the whole picture, shown at `pic`: dragged to move it
    /// and size it.
    fn picture(&mut self, ui: &mut egui::Ui, pic: Rect, (fw, fh): (u32, u32), crop: &mut Option<Crop>, did: &mut Did) {
        let scale = pic.width() / fw.max(1) as f32;
        let whole = Crop::whole(fw, fh);
        let shown = |c: Option<Crop>| {
            let c = c.unwrap_or(whole);
            Rect::from_min_size(pic.min + vec2(c.x as f32, c.y as f32) * scale, vec2(c.w as f32, c.h as f32) * scale)
        };
        let sides = self.shape.is_none();
        let resp = ui.interact(pic.expand(REACH), ui.id().with("crop_tool"), Sense::drag());
        if resp.drag_started()
            && let Some(at) = ui.input(|i| i.pointer.press_origin())
            && let Some(grip) = grip_at(shown(*crop), at, sides)
        {
            self.drag = Some((grip, crop.unwrap_or(whole), at));
            did.took_hold = true;
        }
        if let (Some((grip, from, at)), Some(p)) = (self.drag, resp.interact_pointer_pos()) {
            let d = (p - at) / scale;
            let shape = self.shape.map(|[w, h]| (w, h));
            *crop = dragged(grip, from, (d.x as f64, d.y as f64), (fw, fh), shape, (SNAP / scale) as f64).within(fw, fh);
        }
        if resp.drag_stopped() && self.drag.take().is_some() {
            did.let_go = true;
        }
        let r = shown(*crop);
        // The pointer shows what a drag would do.
        if let Some(g) = self.drag.map(|d| d.0).or_else(|| resp.hover_pos().and_then(|p| grip_at(r, p, sides))) {
            ui.ctx().set_cursor_icon(cursor(g, self.drag.is_some()));
        }

        let painter = ui.painter_at(pic.expand(REACH));
        // What's cut off, dimmed.
        for part in [
            Rect::from_min_max(pic.min, pos2(pic.max.x, r.min.y)),
            Rect::from_min_max(pos2(pic.min.x, r.max.y), pic.max),
            Rect::from_min_max(pos2(pic.min.x, r.min.y), pos2(r.min.x, r.max.y)),
            Rect::from_min_max(pos2(r.max.x, r.min.y), pos2(pic.max.x, r.max.y)),
        ] {
            if part.is_positive() {
                painter.rect_filled(part, 0, Color32::from_black_alpha(150));
            }
        }
        painter.rect_stroke(r, 0, Stroke::new(1.0, Color32::from_white_alpha(230)), StrokeKind::Inside);
        // Thirds while it's dragged, to frame by.
        if self.drag.is_some() {
            let faint = Stroke::new(1.0, Color32::from_white_alpha(70));
            for k in [1.0, 2.0] {
                painter.vline(r.left() + r.width() * k / 3.0, r.y_range(), faint);
                painter.hline(r.x_range(), r.top() + r.height() * k / 3.0, faint);
            }
        }
        // What can be taken hold of: the corners, and of any shape the sides.
        let held = Stroke::new(3.0, Color32::WHITE);
        let len = (r.width().min(r.height()) / 3.0).min(18.0);
        for (x, y) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
            let c = pos2(if x < 0.0 { r.left() } else { r.right() }, if y < 0.0 { r.top() } else { r.bottom() });
            painter.line(vec![c - vec2(0.0, y * len), c, c - vec2(x * len, 0.0)], held);
        }
        if sides {
            let half = len * 0.6;
            let (cx, cy) = (r.center().x, r.center().y);
            for ends in [
                [pos2(cx - half, r.top()), pos2(cx + half, r.top())],
                [pos2(cx - half, r.bottom()), pos2(cx + half, r.bottom())],
                [pos2(r.left(), cy - half), pos2(r.left(), cy + half)],
                [pos2(r.right(), cy - half), pos2(r.right(), cy + half)],
            ] {
                painter.line_segment(ends, held);
            }
        }
    }

    /// Along the top: the crop's size, its aspect ratio, and Reset and Done.
    fn bar(&mut self, ui: &mut egui::Ui, rect: Rect, (fw, fh): (u32, u32), crop: &mut Option<Crop>, did: &mut Did) {
        use crate::ui_kit::Btn;
        let pal = theme::pal(ui);
        let c = crop.unwrap_or(Crop::whole(fw, fh));
        let field_w = crate::ui_kit::ratio_field_width(ui.ctx(), "crop_shape", self.shape);
        let middle = Rect::from_center_size(rect.center(), vec2(field_w, rect.height()));
        if middle.left() - rect.left() - PAD >= 90.0 {
            let size = format!("{} × {}", c.w, c.h);
            ui.painter().text(pos2(rect.left() + PAD, rect.center().y), egui::Align2::LEFT_CENTER, size, theme::font(13.0, W::Medium), pal.text2);
        }
        let mut mid = ui.new_child(egui::UiBuilder::new().max_rect(middle).layout(egui::Layout::left_to_right(egui::Align::Center)));
        let from = crate::ui_kit::ratio_of(c.w as f64 / c.h.max(1) as f64);
        if let Some(pick) = crate::ui_kit::ratio_field(&mut mid, "crop_shape", ("Any", "its sides drag too"), self.shape, from) {
            self.shape = pick;
            // The biggest of the shape there's room for, where the crop is.
            if let Some([w, h]) = self.shape {
                *crop = Crop::largest(fw, fh, w, h, c.centre()).within(fw, fh);
                did.changed = true;
            }
        }
        let right = Rect::from_min_max(pos2(middle.right() + 8.0, rect.top()), pos2(rect.right() - PAD, rect.bottom()));
        let mut rui = ui.new_child(egui::UiBuilder::new().max_rect(right).layout(egui::Layout::right_to_left(egui::Align::Center)));
        rui.spacing_mut().item_spacing.x = 6.0;
        if Btn::primary("Done").small().tip("Done cropping  (Enter)").show(&mut rui).clicked() {
            did.done = true;
        }
        if Btn::ghost("Reset").small().enabled(crop.is_some()).tip("Keep the whole picture").show(&mut rui).clicked() {
            *crop = None;
            self.shape = None;
            did.changed = true;
        }
    }
}

/// The size `crop` of a picture shown at `size` is shown at.
pub fn cropped_size(size: Vec2, c: &Crop, (fw, fh): (u32, u32)) -> Vec2 {
    vec2(size.x * c.w as f32 / fw.max(1) as f32, size.y * c.h as f32 / fh.max(1) as f32)
}

/// Draw just `c` of the picture at `shown` (`paint` draws the whole picture
/// where it's told, cut to the painter's clip): the whole of it placed so
/// that part lands there, the rest cut off.
pub fn paint_cropped(ui: &egui::Ui, shown: Rect, c: &Crop, frame: (u32, u32), paint: impl FnOnce(&egui::Painter, Rect)) {
    let mut shown = shown;
    let mut whole = whole_rect(shown, c, frame);
    // Too big to draw: smaller, in the middle of its place.
    let over = whole.width().max(whole.height()) * ui.ctx().pixels_per_point() / MAX_PX;
    if over > 1.0 {
        shown = Rect::from_center_size(shown.center(), shown.size() / over);
        whole = whole_rect(shown, c, frame);
    }
    paint(&ui.painter_at(shown), whole);
}

/// Where the whole picture goes for its part `c` to be at `shown`.
fn whole_rect(shown: Rect, c: &Crop, (fw, fh): (u32, u32)) -> Rect {
    let (sx, sy) = (shown.width() / c.w.max(1) as f32, shown.height() / c.h.max(1) as f32);
    Rect::from_min_size(shown.min - vec2(c.x as f32 * sx, c.y as f32 * sy), vec2(fw as f32 * sx, fh as f32 * sy))
}

/// What's under `p` of a crop shown at `r`: a corner, a side (`sides`: of
/// any shape), or its inside. The nearer of two sides, for a small one.
fn grip_at(r: Rect, p: Pos2, sides: bool) -> Option<Grip> {
    let near = |p: f32, lo: f32, hi: f32| {
        let (a, b) = ((p - lo).abs(), (p - hi).abs());
        if a.min(b) > REACH { 0 } else if a <= b { -1 } else { 1 }
    };
    let (x, y) = (near(p.x, r.left(), r.right()), near(p.y, r.top(), r.bottom()));
    let along_x = (r.left()..=r.right()).contains(&p.x);
    let along_y = (r.top()..=r.bottom()).contains(&p.y);
    match (x, y) {
        (0, 0) => r.contains(p).then_some(Grip::Move),
        (x, y) if x != 0 && y != 0 => Some(Grip::Size { x, y }),
        (x, 0) if sides && along_y => Some(Grip::Size { x, y: 0 }),
        (0, y) if sides && along_x => Some(Grip::Size { x: 0, y }),
        _ => r.contains(p).then_some(Grip::Move),
    }
}

fn cursor(g: Grip, holding: bool) -> CursorIcon {
    match g {
        Grip::Move if holding => CursorIcon::Grabbing,
        Grip::Move => CursorIcon::Grab,
        Grip::Size { x: 0, .. } => CursorIcon::ResizeVertical,
        Grip::Size { y: 0, .. } => CursorIcon::ResizeHorizontal,
        Grip::Size { x, y } if x == y => CursorIcon::ResizeNwSe,
        Grip::Size { .. } => CursorIcon::ResizeNeSw,
    }
}

/// `from` with what `grip` holds moved `d` pixels, inside an `fw`×`fh`
/// picture, keeping `shape` (width, height) if there is one. Moved, its
/// middle snaps to the picture's within `snap` pixels; sized, the corner or
/// side across from the one held stays where it is.
fn dragged(grip: Grip, from: Crop, d: (f64, f64), (fw, fh): (u32, u32), shape: Option<(u32, u32)>, snap: f64) -> Crop {
    let (fw, fh) = (fw as f64, fh as f64);
    let (x0, y0, w0, h0) = (from.x as f64, from.y as f64, from.w as f64, from.h as f64);
    match grip {
        Grip::Move => {
            let moved = |at: f64, size: f64, room: f64| {
                let at = at.clamp(0.0, (room - size).max(0.0));
                if (at + size / 2.0 - room / 2.0).abs() < snap { (room - size) / 2.0 } else { at }
            };
            Crop { x: moved(x0 + d.0, w0, fw).round() as u32, y: moved(y0 + d.1, h0, fh).round() as u32, ..from }
        }
        Grip::Size { x, y } => {
            // The side or corner that stays, and the room from it the way
            // it's pulled.
            let (ax, ay) = (if x < 0 { x0 + w0 } else { x0 }, if y < 0 { y0 + h0 } else { y0 });
            let (room_w, room_h) = (if x < 0 { ax } else { fw - ax }, if y < 0 { ay } else { fh - ay });
            let min = Crop::MIN as f64;
            let mut w = if x == 0 { w0 } else { (w0 + d.0 * x as f64).clamp(min, room_w.max(min)) };
            let mut h = if y == 0 { h0 } else { (h0 + d.1 * y as f64).clamp(min, room_h.max(min)) };
            if let Some((sw, sh)) = shape {
                let r = sw as f64 / sh.max(1) as f64;
                // The pointer, along the corner's diagonal (so it follows
                // a pull either way), then back inside.
                h = ((w * r + h) / (r * r + 1.0)).max(min).max(min / r);
                w = h * r;
                if w > room_w {
                    (w, h) = (room_w, room_w / r);
                }
                if h > room_h {
                    (w, h) = (room_h * r, room_h);
                }
            }
            let left = match x {
                -1 => ax - w,
                1 => ax,
                _ => x0,
            };
            let top = match y {
                -1 => ay - h,
                1 => ay,
                _ => y0,
            };
            Crop { x: left.round().max(0.0) as u32, y: top.round().max(0.0) as u32, w: w.round() as u32, h: h.round() as u32 }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UW: (u32, u32) = (3440, 1440);

    #[test]
    fn shapes_are_known() {
        use crate::ui_kit::{RATIOS, common_ratio};
        let shape = |c: Crop| common_ratio(c.w, c.h).map(|i| (RATIOS[i].0, RATIOS[i].1));
        assert_eq!(shape(Crop::largest(3440, 1440, 16, 9, (1720.0, 720.0))), Some((16, 9)));
        // 9:16 out of 1080p rounds to 608 wide: still 9:16.
        assert_eq!(shape(Crop::largest(1920, 1080, 9, 16, (960.0, 540.0)).within(1920, 1080).unwrap()), Some((9, 16)));
        assert_eq!(shape(Crop { x: 0, y: 0, w: 1000, h: 700 }), None);
    }

    #[test]
    fn moving_stays_inside_and_snaps_to_the_middle() {
        let c = Crop { x: 440, y: 0, w: 2560, h: 1440 };
        // Pulled past the left edge: at it.
        assert_eq!(dragged(Grip::Move, c, (-1000.0, 50.0), UW, None, 10.0), Crop { x: 0, y: 0, ..c });
        // Near the middle: in it.
        assert_eq!(dragged(Grip::Move, Crop { x: 100, ..c }, (335.0, 0.0), UW, None, 10.0).x, 440);
        assert_eq!(dragged(Grip::Move, Crop { x: 100, ..c }, (300.0, 0.0), UW, None, 10.0).x, 400);
    }

    #[test]
    fn sizing_keeps_the_far_corner_and_the_shape() {
        let c = Crop { x: 440, y: 0, w: 2560, h: 1440 };
        // The top left corner in by 160 across: smaller, the bottom right
        // where it was, 16:9.
        let s = dragged(Grip::Size { x: -1, y: -1 }, c, (160.0, 0.0), UW, Some((16, 9)), 0.0);
        assert_eq!((s.x + s.w, s.y + s.h), (3000, 1440));
        assert!(s.w < 2560 && (s.w as f64 / s.h as f64 / (16.0 / 9.0) - 1.0).abs() < 0.002, "{s:?}");
        // Pulled out past the picture: as big as there's room for.
        let s = dragged(Grip::Size { x: 1, y: 1 }, c, (2000.0, 2000.0), UW, Some((16, 9)), 0.0);
        assert_eq!((s.x, s.w, s.h), (440, 2560, 1440));
        // Any shape: one side alone.
        let s = dragged(Grip::Size { x: 1, y: 0 }, c, (-500.0, 0.0), UW, None, 0.0);
        assert_eq!((s.x, s.y, s.w, s.h), (440, 0, 2060, 1440));
        // Never smaller than the smallest.
        let s = dragged(Grip::Size { x: 1, y: 1 }, c, (-5000.0, -5000.0), UW, None, 0.0);
        assert_eq!((s.w, s.h), (Crop::MIN, Crop::MIN));
    }

    #[test]
    fn corners_sides_and_inside() {
        let r = Rect::from_min_size(pos2(100.0, 100.0), vec2(200.0, 100.0));
        assert_eq!(grip_at(r, pos2(102.0, 98.0), true), Some(Grip::Size { x: -1, y: -1 }));
        assert_eq!(grip_at(r, pos2(305.0, 150.0), true), Some(Grip::Size { x: 1, y: 0 }));
        // A shape kept: no sides, so near one inside it is a move.
        assert_eq!(grip_at(r, pos2(295.0, 150.0), false), Some(Grip::Move));
        assert_eq!(grip_at(r, pos2(305.0, 150.0), false), None);
        assert_eq!(grip_at(r, pos2(200.0, 150.0), true), Some(Grip::Move));
        assert_eq!(grip_at(r, pos2(50.0, 50.0), true), None);
    }

    #[test]
    fn the_whole_picture_lands_the_crop_where_it_shows() {
        let c = Crop { x: 440, y: 0, w: 2560, h: 1440 };
        let shown = Rect::from_min_size(pos2(10.0, 20.0), vec2(1280.0, 720.0));
        let whole = whole_rect(shown, &c, UW);
        assert_eq!(whole.min, pos2(10.0 - 220.0, 20.0));
        assert_eq!(whole.size(), vec2(1720.0, 720.0));
        assert_eq!(cropped_size(vec2(1720.0, 720.0), &c, UW), vec2(1280.0, 720.0));
    }
}
