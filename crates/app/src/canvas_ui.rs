//! The picture's sources on the Sources page's preview, placed the way OBS
//! places them: the screen, the webcams and other displays, each dragged to
//! move it and by a handle to resize it (keeping its shape; Shift stretches
//! it), in front of or behind each other, and cropped with Alt and a handle
//! (the picture keeps its scale, its box shrinks with it; not another
//! display yet). A webcam also flips (past its opposite side) and turns
//! (its right-click menu): see `webcam_ui`. Moves snap to the video's edges
//! and middle and to the other sources' edges (Ctrl to place freely).
//!
//! The screen goes into the video as Fill, Fit or Stretch say (its menu)
//! until it's taken hold of: then it's in a box of its own where it was, so
//! nothing jumps (`settings::ScreenCfg::placed`), and Reset puts it back.
//! Bigger than the video (Fill's), its box reaches past the video's edges,
//! and moving it shows another part of it. Its crop is the crop's page's
//! (`settings::ScreenCrop`). Both placements are shared with the capture,
//! so what's dragged is what's recorded, live. Which is in front is the
//! order of their cards under the preview (see `sources_ui`), or their
//! menus' Bring to front.

use capture::webcam::Placement;
use egui::{Color32, CursorIcon, Pos2, Rect, Stroke, vec2};
use egui_phosphor::regular as icon;

use crate::App;
use crate::settings::{BoxCfg, Framing, PlacementCfg, ScreenCfg, ScreenCrop};
use crate::ui_kit::Item;
use crate::webcam_ui::{self, Handle, Lines, Mods};

/// Handle size, in points.
const HANDLE: f32 = 8.0;

/// How the screen can fill a video of another shape: the choices, as
/// they're named side by side and in a menu, and what each does.
pub(crate) const FRAMINGS: [(Framing, &str, &str, &str); 3] = [
    (Framing::Fit, "Fit", "Fit in the video", "All of it, as big as fits, with black bars where the shapes differ"),
    (Framing::Fill, "Fill", "Fill the video", "Its middle covers all of the video: what's past the video's shape is cut off"),
    (Framing::Stretch, "Stretch", "Stretch to the video", "All of it over all of the video, stretched or squeezed to its shape"),
];

/// Fit, Fill or Stretch, side by side: the one clicked. None is lit while
/// the screen's placed by hand (`now` is `None`).
pub(crate) fn framing_choice(ui: &mut egui::Ui, id: &str, now: Option<Framing>) -> Option<Framing> {
    let opts: Vec<crate::ui_kit::Seg> = FRAMINGS.iter().map(|(_, label, _, tip)| crate::ui_kit::Seg { icon: None, label, tip: Some(tip) }).collect();
    let chosen = now.and_then(|f| FRAMINGS.iter().position(|(g, ..)| *g == f)).unwrap_or(usize::MAX);
    let enabled = ui.is_enabled();
    crate::ui_kit::segmented_opts(ui, id, &opts, chosen, None, enabled).map(|i| FRAMINGS[i].0)
}
/// A source's outline on the preview.
const ACCENT: Color32 = Color32::WHITE;
/// OBS-ish crop colour.
const CROP: Color32 = Color32::from_rgb(80, 220, 120);
/// Smallest the screen's box gets, as a fraction of the video.
const MIN_SIZE: f32 = 0.05;

/// One of the picture's sources.
pub(crate) use crate::settings::LayerKey as Layer;

/// What's selected on the Sources page: a source in the video, on the
/// preview or in the layers. It's outlined, with handles, and the keys act
/// on it; its settings are in a dialog (see `inspector_ui`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Selected {
    Layer(Layer),
}

/// A drag on the preview.
#[derive(Debug, Clone, Copy)]
enum Drag {
    /// A webcam: which, what's held, where it was taken hold of, its placement then.
    Webcam(u64, Handle, Pos2, Placement),
    /// A boxed source (a display, a window, a picture…): which, and as the
    /// screen's is held (moved, resized, or with Alt cropped).
    Visual(u64, ScreenHold),
    /// The screen: moved, resized, or (Alt) cropped.
    Screen(ScreenHold),
    /// A source filling the video, held in its middle: it has nowhere to move.
    Stuck(Layer),
}

/// The screen, held: what's held, where, and how it was then, to resize it
/// from (its box, as fractions of the video, its crop and its picture's
/// shape) and, held by a handle, to crop it from (its box, the part of the
/// display it showed, in its pixels, and the display's size; none for games
/// and apps). Each frame is worked out from these as the keys held say, so
/// Alt (crop) and Shift (stretch, or keep the crop's shape) can be pressed
/// or let go mid-drag.
#[derive(Debug, Clone, Copy)]
struct ScreenHold {
    handle: Handle,
    from: Pos2,
    start: Rect,
    crop: Option<ScreenCrop>,
    picture: Option<f32>,
    /// As it shows: turned and flipped (the part and the display's size
    /// are as they show; its crop is kept in the display's own pixels, this big).
    cropping_from: Option<(Rect, Rect, (u32, u32))>,
    turned: Turned,
    display: (u32, u32),
}

impl ScreenHold {
    /// Dragged by `(dx, dy)` (fractions of the video) with these keys: its
    /// box, its crop, and its picture's shape. Cropping (`mods.crop`, by a
    /// handle of a display), from how it was: the picture keeps its scale
    /// (`mods.keep`: the crop its shape). Else moved or resized from how it
    /// was (`mods.keep`: stretched), its crop as it was.
    fn dragged(&self, dx: f32, dy: f32, mods: Mods, lines: &Lines) -> (Rect, Option<ScreenCrop>, Option<f32>) {
        match self.cropping_from.filter(|_| mods.crop) {
            Some((start, part, display)) => {
                let (b, crop) = screen_crop_drag(start, part, display, self.handle, dx, dy, mods.keep);
                // Its picture's what's kept of the display now, as it shows.
                let (w, h) = crop.map_or(display, |c| (c.w, c.h));
                (b, crop.map(|c| self.own_crop(c, display)), Some(w as f32 / h.max(1) as f32))
            }
            None => (screen_drag(self.start, self.handle, dx, dy, mods.snap, mods.keep, lines), self.crop, self.picture),
        }
    }

    /// A crop as it shows (of a display `shown` big, as it shows) in the
    /// display's own pixels, on even ones.
    fn own_crop(&self, c: ScreenCrop, shown: (u32, u32)) -> ScreenCrop {
        let [x, y] = c.at.unwrap_or([shown.0.saturating_sub(c.w) / 2, shown.1.saturating_sub(c.h) / 2]);
        let r = self.turned.part(Rect::from_min_size(Pos2::new(x as f32, y as f32), vec2(c.w as f32, c.h as f32)), self.display, true);
        let even = |v: f32| ((v.max(0.0) / 2.0).round() as u32) * 2;
        ScreenCrop { w: even(r.width()), h: even(r.height()), at: Some([even(r.min.x), even(r.min.y)]) }
    }
}

/// The preview's own state on the Sources page.
#[derive(Default)]
pub(crate) struct CanvasView {
    /// What's selected (see [`Selected`]).
    pub(crate) selected: Option<Selected>,
    /// Cropping this source on the preview (its Crop button): its handles
    /// crop, as with Alt held, till Done, Enter or Esc.
    pub(crate) crop_mode: Option<Layer>,
    /// When the last undo step was kept, and what for: changes to the same
    /// thing in quick succession (typing, a number stepped) are one step.
    last_step: Option<(String, std::time::Instant)>,
    drag: Option<Drag>,
    /// The right-click menu is open, on this source.
    menu: Option<Layer>,
    /// What was selected before the last click on the preview (which may
    /// have gone on to the source under it): a double-click's for that.
    click_was: Option<Layer>,
    /// The screen's picture size (what's kept of it), asked for now and then.
    picture: Option<(capture::VideoSource, Option<(u32, u32)>, std::time::Instant)>,
    /// Each boxed source's size, by key, asked for now and then, and its
    /// picture (an image's, words drawn), made again when it changes: see
    /// `visuals_ui`.
    pub(crate) sizes: std::collections::HashMap<u64, (Option<(u32, u32)>, std::time::Instant)>,
    pub(crate) pictures: std::collections::HashMap<u64, (crate::settings::Visual, Option<std::sync::Arc<capture::StillImage>>)>,
    /// Whether the computer's fonts were found when words were last drawn.
    pub(crate) fonts_found: bool,
    /// Arrangements to go back to (Ctrl+Z), and forward again.
    undo: Vec<Arrangement>,
    redo: Vec<Arrangement>,
    /// The source whose card (under the preview) the pointer is on: it's
    /// outlined on the preview, so it's clear which it is.
    pub(crate) card_hover: Option<Layer>,
}

/// The video's sources as undo keeps them: the screen (its box or how it
/// fills the video, its crop, its name), each webcam and boxed source as it
/// is (so one taken out comes back), and which is in front.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Arrangement {
    screen: ScreenCfg,
    screen_title: Option<String>,
    framing: Framing,
    crop: Option<ScreenCrop>,
    webcams: Vec<crate::settings::WebcamCfg>,
    visuals: Vec<crate::settings::VisualCfg>,
    order: Vec<Layer>,
}

/// What's at `p` on the preview (`shown` puts a box there, as much of it as
/// is on the preview: its handles are on that): a handle of the source
/// selected, else that source if it's there (under others too: it's the
/// one dragged), else the source in front there.
fn hit(layers: &[(Layer, Rect)], shown: &dyn Fn(Rect) -> Rect, p: Pos2, selected: Option<Layer>) -> Option<(Layer, Handle)> {
    // Only the one selected has handles.
    if let Some((l, r)) = layers.iter().find(|(l, _)| Some(*l) == selected) {
        let bx = shown(*r);
        if let Some(h) = Handle::ALL.into_iter().find(|h| h.pos(bx).distance(p) <= HANDLE) {
            return Some((*l, h));
        }
        if bx.expand(2.0).contains(p) {
            return Some((*l, Handle::Move));
        }
    }
    under(layers, shown, p).first().map(|l| (*l, Handle::Move))
}

/// The sources at `p` on the preview, the one in front first.
fn under(layers: &[(Layer, Rect)], shown: &dyn Fn(Rect) -> Rect, p: Pos2) -> Vec<Layer> {
    layers.iter().filter(|(_, r)| shown(*r).expand(2.0).contains(p)).map(|(l, _)| *l).collect()
}

/// What a click at a place on the preview selects, of `under` (the sources
/// there, front first): the one in front, or, with one of them selected,
/// the next one down (round to the front again from the back).
fn clicked_through(under: &[Layer], selected: Option<Layer>) -> Option<Layer> {
    match selected.and_then(|s| under.iter().position(|l| *l == s)) {
        Some(i) => under.get((i + 1) % under.len()).copied(),
        None => under.first().copied(),
    }
}

/// Words in a dark pill along the top of the preview (`frame`): what the
/// keys do. Left out where it doesn't fit.
fn pill(painter: &egui::Painter, frame: Rect, text: String) {
    let galley = painter.layout_no_wrap(text, egui::FontId::proportional(11.5), Color32::from_gray(235));
    let size = galley.size() + vec2(14.0, 6.0);
    if size.x > frame.width() - 8.0 {
        return;
    }
    let pill = Rect::from_center_size(Pos2::new(frame.center().x, frame.top() + 8.0 + size.y / 2.0), size);
    painter.rect_filled(pill, pill.height() / 2.0, Color32::from_black_alpha(170));
    painter.galley(pill.center() - galley.size() / 2.0, galley, Color32::WHITE);
}

/// What a move of `me` snaps to besides the video's edges and middle: the
/// other sources' edges and middles (not one that fills the video: its
/// edges are the video's).
fn lines_of(layers: &[(Layer, Rect)], me: Layer) -> Lines {
    let mut lines = Lines::default();
    for (_, r) in layers.iter().filter(|(l, r)| *l != me && !fills(*r)) {
        lines.xs.extend([r.min.x, r.center().x, r.max.x]);
        lines.ys.extend([r.min.y, r.center().y, r.max.y]);
    }
    lines
}

/// Whether a box covers all of the video (all of it, or past its edges).
fn fills(r: Rect) -> bool {
    r.min.x <= 1e-4 && r.min.y <= 1e-4 && r.max.x >= 1.0 - 1e-4 && r.max.y >= 1.0 - 1e-4
}

/// Whether a box is all of the video, no more: it has nowhere to move.
fn stuck(r: Rect) -> bool {
    let on = |v: f32, edge: f32| (v - edge).abs() <= 1e-4;
    on(r.min.x, 0.0) && on(r.min.y, 0.0) && on(r.max.x, 1.0) && on(r.max.y, 1.0)
}

/// The lines a box moved onto, to draw: the video's middle, or another
/// source's edge or middle, across (`true`) or down, at.
fn snapped(r: Rect, lines: &Lines) -> Vec<(bool, f32)> {
    let on = |v: f32, at: f32| (v - at).abs() < 1e-4;
    let mut out = Vec::new();
    for at in lines.xs.iter().copied().chain([0.5]) {
        if [r.min.x, r.center().x, r.max.x].iter().any(|v| on(*v, at)) {
            out.push((true, at));
        }
    }
    for at in lines.ys.iter().copied().chain([0.5]) {
        if [r.min.y, r.center().y, r.max.y].iter().any(|v| on(*v, at)) {
            out.push((false, at));
        }
    }
    out
}

/// Where a picture of shape `picture_aspect` goes in a frame of shape
/// `frame_aspect` as big as fits, in the middle (fractions of the frame).
pub(crate) fn fitted(picture_aspect: f32, frame_aspect: f32) -> Rect {
    let size = if picture_aspect > frame_aspect { vec2(1.0, frame_aspect / picture_aspect) } else { vec2(picture_aspect / frame_aspect, 1.0) };
    Rect::from_center_size(Pos2::new(0.5, 0.5), size)
}

/// Where a picture of shape `picture_aspect` goes in a frame of shape
/// `frame_aspect` as small as covers it, in the middle (fractions of the
/// frame): past two of its edges.
pub(crate) fn covering(picture_aspect: f32, frame_aspect: f32) -> Rect {
    let size = if picture_aspect > frame_aspect { vec2(picture_aspect / frame_aspect, 1.0) } else { vec2(1.0, frame_aspect / picture_aspect) };
    Rect::from_center_size(Pos2::new(0.5, 0.5), size)
}

/// Where a picture of shape `picture_aspect` is in a video of shape
/// `frame_aspect` (fractions of it), put in as `framing` says: its middle
/// covering it (past its edges), all of it as big as fits, or all of it
/// stretched over all of it. In the video's own shape (but for rounding),
/// all of the video.
pub(crate) fn framed(framing: capture::Framing, picture_aspect: f32, frame_aspect: f32) -> Rect {
    let whole = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
    if (picture_aspect / frame_aspect - 1.0).abs() < 0.005 {
        return whole;
    }
    match framing {
        capture::Framing::Fill => covering(picture_aspect, frame_aspect),
        capture::Framing::Fit => fitted(picture_aspect, frame_aspect),
        capture::Framing::Stretch => whole,
    }
}

/// The part of a box (`shown`, fractions of the video) that's in the
/// video, and the part of its picture (`part`, in the picture's pixels)
/// that's there. All of it, for a box in the video.
pub(crate) fn in_video(shown: Rect, part: Rect) -> (Rect, Rect) {
    let seen = shown.intersect(Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)));
    if seen == shown || !seen.is_positive() {
        return (shown, part);
    }
    let at = |p: Pos2| part.min + vec2((p.x - shown.min.x) / shown.width() * part.width(), (p.y - shown.min.y) / shown.height() * part.height());
    (seen, Rect::from_min_max(at(seen.min), at(seen.max)))
}

/// How the screen's picture is turned and flipped (its settings'), to map
/// a part of the display between its own pixels (where its crop is kept)
/// and the picture as it shows (what's dragged on the preview).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct Turned {
    turns: u8,
    flip_h: bool,
    flip_v: bool,
}

impl Turned {
    pub(crate) fn of(s: &crate::settings::ScreenCfg) -> Self {
        Self { turns: s.turns % 4, flip_h: s.flip_h, flip_v: s.flip_v }
    }

    pub(crate) fn new(turns: u8, flip_h: bool, flip_v: bool) -> Self {
        Self { turns: turns % 4, flip_h, flip_v }
    }

    fn placement(self) -> capture::ScreenPlacement {
        capture::ScreenPlacement { turns: self.turns, flip_h: self.flip_h, flip_v: self.flip_v, ..capture::ScreenPlacement::whole(capture::Framing::Stretch) }
    }

    /// A picture `size` big, as it shows.
    pub(crate) fn size(self, size: (u32, u32)) -> (u32, u32) {
        self.placement().turned(size)
    }

    /// A point of the picture (0 to 1 across and down it, its own way
    /// round) where it shows: turned, then flipped (`unturn` the other way).
    fn show(self, (s, t): (f32, f32)) -> (f32, f32) {
        let (mut u, mut v) = match self.turns % 4 {
            1 => (1.0 - t, s),
            2 => (1.0 - s, 1.0 - t),
            3 => (t, 1.0 - s),
            _ => (s, t),
        };
        if self.flip_h {
            u = 1.0 - u;
        }
        if self.flip_v {
            v = 1.0 - v;
        }
        (u, v)
    }

    /// Part `r` of a picture `own` pixels big (its own pixels) in the
    /// picture as it shows (its pixels), or (`back`) the other way.
    pub(crate) fn part(self, r: Rect, own: (u32, u32), back: bool) -> Rect {
        let (from, to) = if back { (self.size(own), own) } else { (own, self.size(own)) };
        let map = |p: Pos2| {
            let q = (p.x / from.0.max(1) as f32, p.y / from.1.max(1) as f32);
            let (u, v) = if back { self.placement().unturn(q) } else { self.show(q) };
            Pos2::new(u * to.0 as f32, v * to.1 as f32)
        };
        Rect::from_two_pos(map(r.min), map(r.max))
    }
}

/// A box in the middle of the video, its size kept.
pub(crate) fn centred(r: Rect) -> Rect {
    Rect::from_min_size(Pos2::new((1.0 - r.width()) / 2.0, (1.0 - r.height()) / 2.0), r.size())
}

/// A box made `size`, in the same place in the room it has to move in (one
/// in a corner stays in it; one bigger than the video covers as much of it).
pub(crate) fn resized_in_place(r: Rect, size: egui::Vec2) -> Rect {
    let along = |at: f32, s: f32| if (1.0 - s).abs() > 1e-4 { (at / (1.0 - s)).clamp(0.0, 1.0) } else { 0.5 };
    let (ax, ay) = (along(r.min.x, r.width()), along(r.min.y, r.height()));
    Rect::from_min_size(Pos2::new(ax * (1.0 - size.x), ay * (1.0 - size.y)), size)
}

/// Whether two boxes are the same, but for rounding.
fn same(a: Rect, b: Rect) -> bool {
    (a.min - b.min).length() < 1e-3 && (a.max - b.max).length() < 1e-3
}

/// The screen's box after dragging `handle` by `(dx, dy)` (fractions of the
/// video) from `start`: moved (snapping to the video and to `lines`, unless
/// `snap` is off), or resized from the opposite corner or side, keeping its
/// shape unless it's to `stretch`. Moved, it stays in the video while it
/// fits in it, and over all of it (across or down) while it's bigger,
/// never further out than it was; resized, it goes as far as the video's
/// edges, or as far past them as it was.
pub(crate) fn screen_drag(start: Rect, handle: Handle, dx: f32, dy: f32, snap: bool, stretch: bool, lines: &Lines) -> Rect {
    if handle == Handle::Move {
        let mut r = start.translate(vec2(dx, dy));
        if snap {
            let sx = webcam_ui::snap_offset(&[r.min.x, r.max.x], r.center().x, &lines.xs);
            let sy = webcam_ui::snap_offset(&[r.min.y, r.max.y], r.center().y, &lines.ys);
            r = r.translate(vec2(sx, sy));
        }
        let keep = |at: f32, size: f32, was: f32| {
            let (lo, hi) = ((1.0 - size).min(0.0), (1.0 - size).max(0.0));
            at.clamp(lo.min(was), hi.max(was))
        };
        let (x, y) = (keep(r.min.x, r.width(), start.min.x), keep(r.min.y, r.height(), start.min.y));
        return Rect::from_min_size(Pos2::new(x, y), r.size());
    }
    let bounds = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)).union(start);
    webcam_ui::resize_within(start, handle.sides(), vec2(dx, dy), !stretch, bounds, vec2(MIN_SIZE, MIN_SIZE))
}

/// The screen's box and crop after cropping it by dragging `handle` by
/// `(dx, dy)` (fractions of the video), from its box `start` showing `part`
/// of a `dw`×`dh` display (in its pixels): the picture keeps its scale and
/// place, the box follows the crop's edges, inside the video and the
/// display (it can't be uncropped past it), at least 64 of its pixels each
/// way, on even pixels. With `keep`, the box keeps its shape. `None`: all
/// of the display.
pub(crate) fn screen_crop_drag(start: Rect, part: Rect, (dw, dh): (u32, u32), handle: Handle, dx: f32, dy: f32, keep: bool) -> (Rect, Option<ScreenCrop>) {
    let (sx, sy) = (start.width() / part.width().max(1.0), start.height() / part.height().max(1.0));
    // All of the display, on the video.
    let whole = Rect::from_min_size(Pos2::new(start.min.x - part.min.x * sx, start.min.y - part.min.y * sy), vec2(dw as f32 * sx, dh as f32 * sy));
    let video = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
    let to = webcam_ui::resize_within(start, handle.sides(), vec2(dx, dy), keep, whole.intersect(video), vec2(64.0 * sx, 64.0 * sy));
    // In the display's pixels, even.
    let even = |v: f32, most: u32| (((v.max(0.0) / 2.0).round() as u32) * 2).min(most & !1);
    let (x0, x1) = (even((to.min.x - whole.min.x) / sx, dw), even((to.max.x - whole.min.x) / sx, dw));
    let (y0, y1) = (even((to.min.y - whole.min.y) / sy, dh), even((to.max.y - whole.min.y) / sy, dh));
    let at = |x: u32, y: u32| Pos2::new(whole.min.x + x as f32 * sx, whole.min.y + y as f32 * sy);
    let b = Rect::from_min_max(at(x0, y0), at(x1, y1)).intersect(video);
    let all = (x0, y0, x1, y1) == (0, 0, dw & !1, dh & !1);
    (b, (!all).then_some(ScreenCrop { w: x1 - x0, h: y1 - y0, at: Some([x0, y0]) }))
}

impl App {
    /// The screen's picture's size: what's kept of it, cropped (the main
    /// display's, for games and apps), as it shows (turned). The display's
    /// is asked for now and then.
    pub(crate) fn screen_picture(&mut self) -> Option<(u32, u32)> {
        let source = self.video_source();
        if self.canvas.picture.as_ref().is_none_or(|(s, _, at)| *s != source || at.elapsed() > std::time::Duration::from_secs(2)) {
            let size = capture::frame_size(&source, Default::default());
            self.canvas.picture = Some((source, size, std::time::Instant::now()));
        }
        let crop = self.settings.screen_placement().crop;
        let turned = Turned::of(&self.settings.screen);
        self.canvas.picture.as_ref().and_then(|(_, size, _)| *size).map(|size| turned.size(capture::cropped_size(crop, size)))
    }

    /// The screen's picture's shape (width / height), as `screen_picture`.
    fn screen_aspect(&mut self) -> Option<f32> {
        self.screen_picture().map(|(w, h)| w as f32 / h.max(1) as f32)
    }

    /// Where the screen's picture is in the video (fractions of it, past its
    /// edges for a screen bigger than it): its box when it's been placed by
    /// hand, else where Fill, Fit or Stretch put it (see [`framed`]).
    pub(crate) fn screen_rect(&mut self, frame_aspect: f32) -> Rect {
        if let Some(b) = self.settings.screen.placed {
            return Rect::from_min_size(Pos2::new(b.x, b.y), vec2(b.w, b.h));
        }
        self.screen_first_rect(frame_aspect)
    }

    /// Where the screen goes as the video puts it, its first place and size:
    /// Fit, Fill or Stretch's (see [`framed`]).
    pub(crate) fn screen_first_rect(&mut self, frame_aspect: f32) -> Rect {
        let framing = self.settings.framing.into();
        self.screen_aspect().map_or(Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), |picture| framed(framing, picture, frame_aspect))
    }

    /// The screen in box `r`, or put in as the video puts it when that's
    /// where `r` is (so it follows the video's shape again).
    pub(crate) fn place_or_frame(&mut self, r: Rect, aspect: f32) {
        if same(r, self.screen_first_rect(aspect)) {
            self.settings.screen.placed = None;
        } else {
            self.place_screen(r, aspect);
        }
    }

    /// Whether the screen's position, and its size, can be reset: it's
    /// placed by hand, and not in the middle, or not its first size.
    pub(crate) fn screen_resettable(&mut self, aspect: f32) -> (bool, bool) {
        if self.settings.screen.placed.is_none() {
            return (false, false);
        }
        let (r, first) = (self.screen_rect(aspect), self.screen_first_rect(aspect));
        (!same(r, centred(r)), (r.size() - first.size()).length() > 1e-3)
    }

    /// The screen back in the middle of the video, its size kept.
    pub(crate) fn screen_reset_position(&mut self, aspect: f32) {
        if self.settings.screen.placed.is_some() {
            let r = centred(self.screen_rect(aspect));
            self.place_or_frame(r, aspect);
        }
    }

    /// The screen back to its first size (as the video puts it), where it is.
    pub(crate) fn screen_reset_size(&mut self, aspect: f32) {
        if self.settings.screen.placed.is_some() {
            let r = resized_in_place(self.screen_rect(aspect), self.screen_first_rect(aspect).size());
            self.place_or_frame(r, aspect);
        }
    }

    /// The screen in the box `r` (fractions of a video of shape `aspect`),
    /// for the picture its box was laid out for.
    fn place_screen(&mut self, r: Rect, aspect: f32) {
        let picture = self.settings.screen.placed.and_then(|b| b.picture_aspect).or_else(|| self.screen_aspect());
        self.settings.screen.placed = Some(BoxCfg::new([r.min.x, r.min.y, r.width(), r.height()], aspect, picture));
    }

    pub(crate) fn arrangement(&self) -> Arrangement {
        Arrangement {
            screen: self.settings.screen,
            screen_title: self.settings.screen_title.clone(),
            framing: self.settings.framing,
            crop: self.settings.screen_crop,
            webcams: self.settings.webcams.clone(),
            visuals: self.settings.visuals.clone(),
            order: self.settings.layer_order(),
        }
    }

    /// `was` (how the sources were before a change just made) can be gone
    /// back to.
    pub(crate) fn keep_for_undo(&mut self, was: Arrangement) {
        self.canvas.last_step = None;
        self.push_undo(was);
    }

    /// [`keep_for_undo`](Self::keep_for_undo), but a change to the same
    /// thing (`what`) less than a second after the last is part of that
    /// step: typing a word is one step, not one a letter.
    pub(crate) fn keep_for_undo_as(&mut self, was: Arrangement, what: String) {
        let now = std::time::Instant::now();
        let same = self.canvas.last_step.as_ref().is_some_and(|(w, at)| *w == what && now.duration_since(*at) < std::time::Duration::from_millis(1200));
        self.canvas.last_step = Some((what, now));
        if same {
            self.canvas.redo.clear();
            return;
        }
        self.push_undo(was);
    }

    fn push_undo(&mut self, was: Arrangement) {
        if self.canvas.undo.last() != Some(&was) {
            self.canvas.undo.push(was);
            if self.canvas.undo.len() > 100 {
                self.canvas.undo.remove(0);
            }
        }
        self.canvas.redo.clear();
    }

    /// The sources as `a` has them.
    fn arrange(&mut self, a: Arrangement) {
        self.settings.screen = a.screen;
        self.settings.screen_title = a.screen_title;
        (self.settings.framing, self.settings.screen_crop) = (a.framing, a.crop);
        self.settings.webcams = a.webcams;
        self.settings.visuals = a.visuals;
        self.settings.layer_order = a.order;
        // A source no scene has any more (one deleted meanwhile) goes.
        self.settings.tidy_pool();
        // What's selected may be gone.
        if let Some(Selected::Layer(l)) = self.canvas.selected
            && !self.settings.layer_order().contains(&l)
        {
            self.canvas.selected = None;
        }
    }

    /// What belonged to the scene shown before a switch: its undo steps
    /// (they'd put its sources' places into the one shown now), a drag, the
    /// right-click menu.
    pub(crate) fn forget_canvas_steps(&mut self) {
        self.canvas.undo.clear();
        self.canvas.redo.clear();
        self.canvas.last_step = None;
        self.canvas.drag = None;
        self.canvas.menu = None;
        self.canvas.card_hover = None;
        self.canvas.crop_mode = None;
    }

    /// The video source selected, if one is.
    pub(crate) fn selected_layer(&self) -> Option<Layer> {
        match self.canvas.selected {
            Some(Selected::Layer(l)) => Some(l),
            _ => None,
        }
    }

    /// Select `layer` (on the preview and in its list; its settings beside them).
    pub(crate) fn select_layer(&mut self, layer: Layer) {
        self.canvas.selected = Some(Selected::Layer(layer));
    }

    /// The arrangement before the last change (Ctrl+Z on Sources).
    pub(crate) fn undo_arrangement(&mut self) {
        if let Some(a) = self.canvas.undo.pop() {
            let now = self.arrangement();
            self.canvas.redo.push(now);
            self.arrange(a);
        }
    }

    /// The change undone last, again (Ctrl+Shift+Z).
    pub(crate) fn redo_arrangement(&mut self) {
        if let Some(a) = self.canvas.redo.pop() {
            let now = self.arrangement();
            self.canvas.undo.push(now);
            self.arrange(a);
        }
    }

    /// Undo and redo, over the preview: dimmed while there's nothing to undo
    /// (or redo).
    pub(crate) fn undo_buttons(&mut self, ui: &mut egui::Ui) {
        use crate::ui_kit::{Btn, Kind};
        let ctrl = crate::shortcuts::ctrl_name();
        let undo = Btn::icon(Kind::Ghost, icon::ARROW_COUNTER_CLOCKWISE).small().enabled(!self.canvas.undo.is_empty()).tip(format!("Undo where your sources are  ({ctrl}+Z)"));
        if undo.show(ui).clicked() {
            self.undo_arrangement();
        }
        let redo = Btn::icon(Kind::Ghost, icon::ARROW_CLOCKWISE).small().enabled(!self.canvas.redo.is_empty()).tip(format!("Redo  ({ctrl}+Shift+Z)"));
        if redo.show(ui).clicked() {
            self.redo_arrangement();
        }
    }

    /// Where webcam `key` goes.
    pub(crate) fn webcam_placement(&self, key: u64) -> Option<Placement> {
        self.settings.webcam(key).map(|w| w.placement.into())
    }

    /// Where a source is in a video of shape `frame_aspect` (fractions of
    /// it), hidden or not.
    pub(crate) fn layer_box(&mut self, layer: Layer, frame_aspect: f32) -> Option<Rect> {
        match layer {
            Layer::Screen => Some(self.screen_rect(frame_aspect)),
            Layer::Webcam(key) => self.webcam_placement(key).map(|p| Rect::from_min_size(Pos2::new(p.x, p.y), vec2(p.w, p.h))),
            Layer::Visual(key) => self.settings.visual(key).is_some().then(|| self.visual_rect(key, frame_aspect)),
        }
    }

    /// The picture's sources, front first, for their cards: the screen, and
    /// each webcam and other display (off too).
    pub(crate) fn layer_order(&self) -> Vec<Layer> {
        self.settings.layer_order()
    }

    /// The sources in this order, front first.
    pub(crate) fn set_layer_order(&mut self, order: &[Layer]) {
        self.settings.layer_order = order.to_vec();
    }

    /// The sources in the video, front first, and where each is (fractions
    /// of the video).
    fn layers(&mut self, frame_aspect: f32) -> Vec<(Layer, Rect)> {
        let mut out = Vec::new();
        for l in self.layer_order() {
            let r = match l {
                Layer::Screen => self.settings.screen.enabled.then(|| self.screen_rect(frame_aspect)),
                Layer::Webcam(key) => self.settings.webcam(key).filter(|w| w.enabled).map(|w| Rect::from_min_size(Pos2::new(w.placement.x, w.placement.y), vec2(w.placement.w, w.placement.h))),
                Layer::Visual(key) => self.settings.visual(key).is_some_and(|d| d.enabled).then(|| self.visual_rect(key, frame_aspect)),
            };
            if let Some(r) = r {
                out.push((l, r));
            }
        }
        out
    }

    /// The sources on the preview (`frame_rect`, whose `response` takes the
    /// pointer): a click selects the one in front under it (or nothing), and
    /// clicks again there the ones under it in turn; the one selected has
    /// its outline and handles, to move, resize and (Alt) crop it, and it's
    /// what's dragged where it is, under others too; any other can be
    /// dragged, which selects it. Others show their outline under the
    /// pointer, and right-click opens a source's menu. A locked source can't
    /// be moved here, and clicks go through it.
    pub(crate) fn canvas_on_preview(&mut self, ui: &mut egui::Ui, frame_rect: Rect, response: &egui::Response) {
        let aspect = frame_rect.width() / frame_rect.height().max(1.0);
        let at = move |p: Pos2| Pos2::new(frame_rect.left() + p.x * frame_rect.width(), frame_rect.top() + p.y * frame_rect.height());
        let to_screen = move |r: Rect| Rect::from_min_max(at(r.min), at(r.max));
        let layers = self.layers(aspect);
        // As much of a box as is on the preview: a screen bigger than the
        // video reaches past it, and its outline and handles stay on it.
        let shown = move |r: Rect| to_screen(r).intersect(frame_rect);
        let modifiers = ui.input(|i| i.modifiers);
        let pointer = ui.input(|i| i.pointer.hover_pos()).filter(|p| frame_rect.expand(HANDLE).contains(*p));
        // What can be taken hold of: not what's locked.
        let free: Vec<(Layer, Rect)> = layers.iter().filter(|(l, _)| !self.settings.is_locked(*l)).copied().collect();
        let selected = self.selected_layer().filter(|l| free.iter().any(|(x, _)| x == l));
        let hit_at = |p: Pos2| hit(&free, &shown, p, selected);
        // Its handles crop: Alt held, or its Crop button on (that one only).
        if self.canvas.crop_mode.is_some() && self.canvas.crop_mode != selected {
            self.canvas.crop_mode = None;
        }
        let crop_on = modifiers.alt || self.canvas.crop_mode.is_some();

        // A click selects what's under it, in front, and again there the one
        // under that, and so on; on nothing, nothing. On a handle, the one
        // selected stays. Twice, the settings open of what the first click
        // found selected (it went on to the next one).
        if let Some(p) = response.interact_pointer_pos() {
            let there = under(&free, &shown, p);
            if response.double_clicked() {
                let was = self.canvas.click_was.filter(|l| there.contains(l));
                if let Some(l) = was.or(selected) {
                    self.edit_layer(l);
                }
            } else if response.clicked() {
                self.canvas.click_was = selected;
                let on_handle = hit_at(p).is_some_and(|(l, h)| Some(l) == selected && h != Handle::Move);
                if !on_handle {
                    self.canvas.selected = clicked_through(&there, selected).map(Selected::Layer);
                }
            }
        }
        // A drag only starts once the pointer has moved a little, so what was
        // grabbed is judged by where the button went down, not where it is now.
        if response.drag_started() {
            let origin = ui.input(|i| i.pointer.press_origin()).or(response.interact_pointer_pos());
            if let Some(((layer, handle), from)) = origin.and_then(|p| hit_at(p).map(|h| (h, p))) {
                self.select_layer(layer);
                let was = self.arrangement();
                let drag = match layer {
                    Layer::Webcam(key) => Drag::Webcam(key, handle, from, self.webcam_placement(key).unwrap_or_else(Placement::hidden)),
                    Layer::Screen => self.take_screen(handle, from, aspect),
                    Layer::Visual(key) => self.take_visual(key, handle, from, aspect),
                };
                // Something that moves can be put back (one held that can't, can't).
                if !matches!(drag, Drag::Stuck(_)) {
                    self.keep_for_undo(was);
                }
                self.canvas.drag = Some(drag);
            }
        }
        let mut guides = Vec::new();
        if let Some(drag) = self.canvas.drag {
            if response.dragged()
                && let Some(now) = response.interact_pointer_pos()
            {
                let snap = !modifiers.ctrl;
                let d = |from: Pos2| ((now.x - from.x) / frame_rect.width(), (now.y - from.y) / frame_rect.height());
                match drag {
                    Drag::Webcam(key, handle, from, start) => {
                        let lines = lines_of(&layers, Layer::Webcam(key));
                        let (dx, dy) = d(from);
                        let moved = webcam_ui::drag(start, handle, dx, dy, Mods { crop: crop_on, snap, keep: modifiers.shift }, &lines);
                        if let Some(w) = self.settings.webcam_mut(key) {
                            w.placement = PlacementCfg::laid_out(moved, aspect);
                        }
                        if handle == Handle::Move && snap {
                            guides = snapped(Rect::from_min_size(Pos2::new(moved.x, moved.y), vec2(moved.w, moved.h)), &lines);
                        }
                    }
                    Drag::Visual(key, h) => {
                        let (dx, dy) = d(h.from);
                        let lines = lines_of(&layers, Layer::Visual(key));
                        // A colour has no shape of its own to keep.
                        let shapeless = self.settings.visual(key).is_some_and(|v| v.visual.shapeless());
                        let (r, crop, picture) = h.dragged(dx, dy, Mods { crop: crop_on, snap, keep: modifiers.shift || shapeless }, &lines);
                        if let Some(v) = self.settings.visual_mut(key) {
                            v.placed = Some(BoxCfg::new([r.min.x, r.min.y, r.width(), r.height()], aspect, if shapeless { None } else { picture }));
                            v.crop = crop;
                        }
                        if h.handle == Handle::Move && snap {
                            guides = snapped(r, &lines);
                        }
                    }
                    Drag::Screen(h) => {
                        let (dx, dy) = d(h.from);
                        let lines = lines_of(&layers, Layer::Screen);
                        let (r, crop, picture) = h.dragged(dx, dy, Mods { crop: crop_on, snap, keep: modifiers.shift }, &lines);
                        self.settings.screen.placed = Some(BoxCfg::new([r.min.x, r.min.y, r.width(), r.height()], aspect, picture));
                        self.settings.screen_crop = crop;
                        if h.handle == Handle::Move && snap {
                            guides = snapped(r, &lines);
                        }
                    }
                    Drag::Stuck(_) => {}
                }
                ui.ctx().set_cursor_icon(match drag {
                    Drag::Webcam(_, Handle::Move, ..) | Drag::Visual(_, ScreenHold { handle: Handle::Move, .. }) | Drag::Screen(ScreenHold { handle: Handle::Move, .. }) => CursorIcon::Grabbing,
                    Drag::Webcam(_, h, ..) | Drag::Visual(_, ScreenHold { handle: h, .. }) | Drag::Screen(ScreenHold { handle: h, .. }) => h.cursor(),
                    Drag::Stuck(_) => CursorIcon::NotAllowed,
                });
            }
            if response.drag_stopped() || !response.dragged() && !ui.input(|i| i.pointer.any_down()) {
                self.canvas.drag = None;
            }
        } else if let Some((l, h)) = pointer.and_then(hit_at) {
            // A handle resizes only the one selected; anywhere else on a
            // source moves it.
            let _ = l;
            ui.ctx().set_cursor_icon(h.cursor());
        }

        // Right-click on a source: it's selected, and its menu opens (tied
        // to the click that opened it).
        if response.secondary_clicked() {
            self.canvas.menu = ui.input(|i| i.pointer.interact_pos()).and_then(hit_at).map(|(l, _)| l);
            if let Some(l) = self.canvas.menu {
                self.select_layer(l);
            }
        }
        if let Some(layer) = self.canvas.menu {
            let was = self.arrangement();
            let open = crate::ui_kit::context_menu(response, 230.0, |ui| self.layer_menu(ui, layer, aspect));
            if self.arrangement() != was {
                self.keep_for_undo(was);
            }
            if open.is_none() {
                self.canvas.menu = None;
            }
        }

        let layers = self.layers(aspect);
        let free: Vec<(Layer, Rect)> = layers.iter().filter(|(l, _)| !self.settings.is_locked(*l)).copied().collect();
        let selected = self.selected_layer().filter(|l| layers.iter().any(|(x, _)| x == l));
        let locked = selected.is_some_and(|l| self.settings.is_locked(l));
        let held = self.canvas.drag.map(|d| match d {
            Drag::Webcam(key, h, ..) => (Layer::Webcam(key), Some(h)),
            Drag::Visual(key, h) => (Layer::Visual(key), Some(h.handle)),
            Drag::Screen(h) => (Layer::Screen, Some(h.handle)),
            Drag::Stuck(l) => (l, None),
        });
        let painter = ui.painter().clone();
        let over = pointer.and_then(|p| hit(&free, &shown, p, selected.filter(|_| !locked)));
        // Under the pointer (or its row in the list), not selected: a thin
        // outline, so it's clear what a click would pick.
        let hovered = held.is_none().then(|| over.map(|(l, _)| l).or(self.canvas.card_hover)).flatten().filter(|l| Some(*l) != selected);
        if let Some(r) = hovered.and_then(|l| layers.iter().find(|(x, _)| *x == l)).map(|(_, r)| shown(*r)).filter(|r| r.is_positive()) {
            painter.rect_stroke(r.shrink(0.5), 0.0, Stroke::new(1.0, ACCENT.gamma_multiply(0.6)), egui::StrokeKind::Middle);
        }
        // The one held, else the one selected: its outline and handles.
        let active = held.or(selected.map(|l| (l, over.filter(|(o, _)| *o == l).map(|(_, h)| h))));
        let Some((active, handle)) = active else {
            return;
        };
        let Some(rect) = layers.iter().find(|(l, _)| *l == active).map(|(_, r)| *r) else { return };
        let bx = shown(rect);
        if !bx.is_positive() {
            return;
        }
        let webcam: Option<Placement> = match active {
            Layer::Webcam(key) => self.webcam_placement(key),
            _ => None,
        };
        // The box, the part of the picture it shows and the picture's size,
        // when it can be cropped (a display, a window, a picture; not games
        // and apps, a colour or words).
        let boxed = match active {
            Layer::Screen => self.screen_crop_hold(aspect),
            Layer::Visual(key) => self.visual_crop_hold(key, aspect),
            Layer::Webcam(_) => None,
        };
        let croppable = webcam.is_some() || boxed.is_some();
        let cropping = crop_on && croppable && handle.is_some_and(|h| h != Handle::Move);

        // Cropping: where the whole camera picture (or display) would be,
        // dashed, so it's clear what's cut off and how far it can grow back.
        let whole = match (webcam, boxed) {
            (Some(p), _) => {
                let sx = p.w / (1.0 - p.crop[0] - p.crop[2]).max(0.01);
                let sy = p.h / (1.0 - p.crop[1] - p.crop[3]).max(0.01);
                let (cl, cr) = if p.flip_h { (p.crop[2], p.crop[0]) } else { (p.crop[0], p.crop[2]) };
                let (ct, cb) = if p.flip_v { (p.crop[3], p.crop[1]) } else { (p.crop[1], p.crop[3]) };
                Some(Rect::from_min_max(at(Pos2::new(p.x - cl * sx, p.y - ct * sy)), at(Pos2::new(p.x + p.w + cr * sx, p.y + p.h + cb * sy))))
            }
            (None, Some((b, part, (dw, dh)))) => {
                let (sx, sy) = (b.width() / part.width().max(1.0), b.height() / part.height().max(1.0));
                Some(to_screen(Rect::from_min_size(Pos2::new(b.min.x - part.min.x * sx, b.min.y - part.min.y * sy), vec2(dw as f32 * sx, dh as f32 * sy))))
            }
            _ => None,
        };
        if let Some(whole) = whole.filter(|w| cropping && *w != bx) {
            // Kept to the preview: all of a display can be far bigger than it.
            let ghost = Stroke::new(1.0, CROP.gamma_multiply(0.8));
            let c = [whole.left_top(), whole.right_top(), whole.right_bottom(), whole.left_bottom(), whole.left_top()];
            painter.with_clip_rect(frame_rect.expand(1.0)).extend(egui::Shape::dashed_line(&c, ghost, 4.0, 3.0));
        }

        // Moved onto the video's middle, or another source's edge: a guide.
        let guide = Stroke::new(1.0, ACCENT.gamma_multiply(0.6));
        for (across, v) in guides {
            let line = if across {
                let x = frame_rect.left() + v * frame_rect.width();
                [Pos2::new(x, frame_rect.top()), Pos2::new(x, frame_rect.bottom())]
            } else {
                let y = frame_rect.top() + v * frame_rect.height();
                [Pos2::new(frame_rect.left(), y), Pos2::new(frame_rect.right(), y)]
            };
            painter.extend(egui::Shape::dashed_line(&line, guide, 5.0, 4.0));
        }

        // The outline, just inside the box, over a soft dark edge so it shows
        // on a bright picture. A cropped side (as it shows: a mirrored
        // picture's left is its right) is green (not one past the video:
        // the video's edge is drawn there).
        let [cl, ct, cr, cb] = match (webcam, boxed.map(|(_, part, (dw, dh))| (part, (dw, dh)))) {
            (Some(p), _) => p.crop.map(|c| c > 0.0),
            (None, Some(_)) => {
                let shown_crop = match active {
                    Layer::Screen => self.screen_crop_shown(),
                    Layer::Visual(key) => self.visual_crop_shown(key),
                    Layer::Webcam(_) => None,
                };
                shown_crop.map_or([false; 4], |(r, (dw, dh))| [r.min.x > 0.5, r.min.y > 0.5, r.max.x < (dw & !1) as f32 - 0.5, r.max.y < (dh & !1) as f32 - 0.5])
            }
            _ => [false; 4],
        };
        let (flip_h, flip_v) = webcam.map_or((false, false), |p| (p.flip_h, p.flip_v));
        let (left, right) = if flip_h { (cr, cl) } else { (cl, cr) };
        let (top, bottom) = if flip_v { (cb, ct) } else { (ct, cb) };
        let (left, top) = (left && rect.min.x >= -1e-4, top && rect.min.y >= -1e-4);
        let (right, bottom) = (right && rect.max.x <= 1.0 + 1e-4, bottom && rect.max.y <= 1.0 + 1e-4);
        let line = bx.shrink(1.0);
        painter.rect_stroke(bx.shrink(0.5), 0.0, Stroke::new(3.0, Color32::from_black_alpha(110)), egui::StrokeKind::Middle);
        let side = |cropped: bool| Stroke::new(2.0, if cropped || cropping { CROP } else { ACCENT });
        painter.line_segment([line.left_top(), line.right_top()], side(top));
        painter.line_segment([line.left_bottom(), line.right_bottom()], side(bottom));
        painter.line_segment([line.left_top(), line.left_bottom()], side(left));
        painter.line_segment([line.right_top(), line.right_bottom()], side(right));

        // Handles: round, white, green while Alt is held on something that
        // crops (they'll crop); the one held or under the pointer a little bigger.
        // Locked: its outline, no handles.
        for h in Handle::ALL.into_iter().filter(|_| !locked) {
            let r = if Some(h) == handle { HANDLE * 0.65 } else { HANDLE * 0.5 };
            let fill = if crop_on && croppable || cropping { CROP } else { Color32::WHITE };
            painter.circle(h.pos(line), r, fill, Stroke::new(1.0, Color32::from_black_alpha(140)));
        }

        // No keys said over the picture (they're in the menus, and the
        // tooltips): only why a drag that can't move doesn't, while it's held.
        if matches!(self.canvas.drag, Some(Drag::Stuck(_))) {
            pill(&painter, frame_rect, "It fills the video: drag a corner to make it smaller".to_owned());
        }
    }
    /// Take hold of the screen: in its box as it is; put in by Fill, Fit or
    /// Stretch, into a box of its own where it shows, so nothing jumps
    /// (Fill's past the video's edges, Stretch's all of it, stretched). Over
    /// all of the video exactly, it has nowhere to move: held in the
    /// middle, it stays. Held by a handle, a display can be cropped too
    /// (Alt), from how it shows (see `screen_crop_hold`).
    fn take_screen(&mut self, handle: Handle, from: Pos2, aspect: f32) -> Drag {
        let shown = self.screen_rect(aspect);
        if self.settings.screen.placed.is_none() {
            if stuck(shown) && handle == Handle::Move {
                return Drag::Stuck(Layer::Screen);
            }
            self.place_screen(shown, aspect);
        }
        let picture = self.settings.screen.placed.and_then(|b| b.picture_aspect);
        let cropping_from = (handle != Handle::Move).then(|| self.screen_crop_hold(aspect)).flatten();
        let (turned, display) = (Turned::of(&self.settings.screen), self.crop_whole().unwrap_or_default());
        Drag::Screen(ScreenHold { handle, from, start: shown, crop: self.settings.screen_crop, picture, cropping_from, turned, display })
    }

    /// Take hold of a boxed source, as the screen is (see `take_screen`):
    /// one not placed yet goes into a box of its own where it shows; one
    /// that's all of the video exactly, held in its middle, stays. Held by a
    /// handle, its picture can be cropped too (Alt), but a colour's or words'.
    fn take_visual(&mut self, key: u64, handle: Handle, from: Pos2, aspect: f32) -> Drag {
        let start = self.visual_rect(key, aspect);
        let Some(d) = self.settings.visual(key).cloned() else { return Drag::Stuck(Layer::Visual(key)) };
        if d.placed.is_none() {
            if stuck(start) && handle == Handle::Move {
                return Drag::Stuck(Layer::Visual(key));
            }
            let picture = (!d.visual.shapeless()).then(|| self.visual_size(key).map(|(w, h)| w as f32 / h.max(1) as f32)).flatten();
            if let Some(v) = self.settings.visual_mut(key) {
                v.placed = Some(BoxCfg::new([start.min.x, start.min.y, start.width(), start.height()], aspect, picture));
            }
        }
        let picture = self.settings.visual(key).and_then(|v| v.placed).and_then(|b| b.picture_aspect);
        let cropping_from = (handle != Handle::Move && d.visual.croppable()).then(|| self.visual_crop_hold(key, aspect)).flatten();
        let (turned, display) = (Turned::new(d.turns, d.flip_h, d.flip_v), self.visual_own_size(key).unwrap_or_default());
        Drag::Visual(key, ScreenHold { handle, from, start, crop: d.crop, picture, cropping_from, turned, display })
    }

    /// The screen's crop as it is (left, top, width, height in the
    /// display's pixels; all of it when it isn't cropped), and the display's
    /// size: `None` for games and apps, and until the display's size is known.
    fn screen_crop_rect(&self) -> Option<([u32; 4], (u32, u32))> {
        let (dw, dh) = self.crop_whole()?;
        let rect = self.settings.screen_crop.filter(|c| !c.is_none()).map_or([0, 0, dw & !1, dh & !1], |c| capture::Crop::from(c).rect(dw, dh));
        Some((rect, (dw, dh)))
    }

    /// [`screen_crop_rect`](Self::screen_crop_rect) as the screen shows,
    /// turned and flipped: the part kept, and the display's size.
    fn screen_crop_shown(&self) -> Option<(Rect, (u32, u32))> {
        let ([x, y, w, h], display) = self.screen_crop_rect()?;
        let turned = Turned::of(&self.settings.screen);
        let part = Rect::from_min_size(Pos2::new(x as f32, y as f32), vec2(w as f32, h as f32));
        Some((turned.part(part, display, false), turned.size(display)))
    }

    /// The screen as it's taken hold of to crop it (Alt with a handle): its
    /// box, the part of the display it shows, and the display's size. Past
    /// the video's edges (Fill's middle, a screen bigger than the video),
    /// that's the part showing, so nothing moves as it's taken hold of and
    /// what's cropped is what's seen. `None` when it can't be cropped (see
    /// `screen_crop_rect`).
    fn screen_crop_hold(&mut self, aspect: f32) -> Option<(Rect, Rect, (u32, u32))> {
        let (part, display) = self.screen_crop_shown()?;
        let (shown, part) = in_video(self.screen_rect(aspect), part);
        Some((shown, part, display))
    }

    /// The screen turned a quarter more, clockwise (`by` 1) or anticlockwise
    /// (-1), as it's seen (mirrored, it turns the other way under its
    /// mirror, as the webcam's does). Its box takes the new shape around its
    /// middle, shrunk to stay in the video if it was in it; put in by Fill,
    /// Fit or Stretch, it's put in again. Its crop stays on the same part of
    /// the display.
    pub(crate) fn screen_turn(&mut self, by: i8, aspect: f32) {
        let s = &mut self.settings.screen;
        let by = if s.flip_h != s.flip_v { -by } else { by };
        s.turns = (s.turns as i8 + by).rem_euclid(4) as u8;
        s.placed = s.placed.map(|b| turned_box(b, aspect));
    }

    /// Whether the video has a shape of its own (an aspect ratio, or a size
    /// of its own) rather than the screen's.
    pub(crate) fn video_shaped(&self) -> bool {
        self.settings.capture_shape().is_some()
    }

    /// The screen over all of the video again, put in as `framing` says.
    pub(crate) fn screen_framed(&mut self, framing: Framing) {
        self.settings.framing = framing;
        self.settings.screen.placed = None;
    }

    /// A source's menu (right-click it on the preview or in the list, or the
    /// ⋯ in its settings' dialog), the same for each as far as each goes:
    /// its settings, how the video puts the screen in, cropping it on the
    /// preview, its position, size and crop back, turning, mirroring and
    /// flipping it, which is in front, duplicating it, copying it to other
    /// scenes (placed as here) or unlinking it from them, hiding and
    /// removing it, with the keys that do the same.
    pub(crate) fn layer_menu(&mut self, ui: &mut egui::Ui, layer: Layer, aspect: f32) {
        let screen = layer == Layer::Screen;
        let ctrl = crate::shortcuts::ctrl_name();
        let keys = |k: &[&str]| Some(k.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>());
        // Its settings, but in their dialog (whose ⋯ this is).
        let in_dialog = self.sources_view.editing.is_some();
        if !in_dialog {
            if Item::new("Settings…").icon(icon::SLIDERS_HORIZONTAL).tip("What it shows, and where: in a dialog").show(ui).clicked() {
                self.edit_layer(layer);
                ui.close();
            }
            crate::ui_kit::menu_sep(ui);
        }
        // How the video puts the screen in: its position and size both. In
        // the screen's own shape too, for when it changes (a game at a
        // stretched resolution).
        if screen {
            let now = self.settings.screen.placed.is_none().then_some(self.settings.framing);
            for (framing, _, label, tip) in FRAMINGS {
                if Item::new(label).check(now == Some(framing)).tip(tip).show(ui).clicked() {
                    self.screen_framed(framing);
                    ui.close();
                }
            }
            crate::ui_kit::menu_sep(ui);
        }
        // Each on its own; both are its first place and size.
        let (position, size) = self.layer_resettable(layer, aspect);
        let (place_tip, size_tip) = match layer {
            Layer::Screen if self.video_shaped() => ("Back in the middle of the video, its size kept", "Back to the size the video gives it, where it is"),
            Layer::Screen => ("Back in the middle of the video, its size kept", "As big as the video again, where it is"),
            Layer::Webcam(_) => ("Back to the bottom-right corner, its size kept", "A quarter of the video wide again, where it is"),
            Layer::Visual(_) => ("Back in the middle of the video, its size kept", "Its first size again, where it is"),
        };
        if Item::new("Reset position").icon(icon::ARROWS_OUT_CARDINAL).enabled(position).tip(place_tip).show(ui).clicked() {
            self.layer_reset_position(layer, aspect);
            ui.close();
        }
        if Item::new("Reset size").icon(icon::RESIZE).enabled(size).tip(size_tip).show(ui).clicked() {
            self.layer_reset_size(layer, aspect);
            ui.close();
        }
        if let Some(cropped) = self.layer_cropped(layer) {
            // Its handles crop till Done or Enter, on the preview (the
            // dialog closes for it).
            let tip = format!("Drag its handles to cut its edges off, till Done or Enter ({} and a handle do it too)", crate::shortcuts::alt_name());
            if Item::new("Crop").icon(icon::CROP).tip(tip).show(ui).clicked() {
                self.select_layer(layer);
                self.canvas.crop_mode = Some(layer);
                self.sources_view.editing = None;
                ui.close();
            }
            if Item::new("Reset crop").icon(icon::ARROWS_OUT).enabled(cropped).tip("All of its picture again").show(ui).clicked() {
                self.layer_uncrop(layer, aspect);
                ui.close();
            }
        }
        crate::ui_kit::menu_sep(ui);
        // Turned a quarter at a time, as it's seen.
        for (label, ic, by) in [("Turn clockwise", icon::ARROW_CLOCKWISE, 1), ("Turn anticlockwise", icon::ARROW_COUNTER_CLOCKWISE, -1)] {
            if Item::new(label).icon(ic).show(ui).clicked() {
                self.turn_layer(layer, by, aspect);
                ui.close();
            }
        }
        let (mirrored, upside_down) = self.layer_flips(layer);
        if Item::new("Mirror").icon(icon::FLIP_HORIZONTAL).check(mirrored).tip("Flip left to right").show(ui).clicked() {
            self.flip_layer(layer, true, aspect);
            ui.close();
        }
        if Item::new("Flip upside down").icon(icon::FLIP_VERTICAL).check(upside_down).show(ui).clicked() {
            self.flip_layer(layer, false, aspect);
            ui.close();
        }
        // Which is in front: with others to go in front of or behind.
        let order = self.layer_order();
        if order.len() > 1 {
            crate::ui_kit::menu_sep(ui);
            let at = order.iter().position(|l| *l == layer).unwrap_or(0);
            let last = order.len() - 1;
            let mut to = None;
            if Item::new("Bring to front").icon(icon::SELECTION_FOREGROUND).enabled(at > 0).keys(keys(&[ctrl, "Shift", "]"])).show(ui).clicked() {
                to = Some(0);
            }
            if order.len() > 2 && Item::new("Bring forward").icon(icon::ARROW_UP).enabled(at > 0).keys(keys(&[ctrl, "]"])).show(ui).clicked() {
                to = Some(at.saturating_sub(1));
            }
            if order.len() > 2 && Item::new("Send backward").icon(icon::ARROW_DOWN).enabled(at < last).keys(keys(&[ctrl, "["])).show(ui).clicked() {
                to = Some(at + 1);
            }
            if Item::new("Send to back").icon(icon::SELECTION_BACKGROUND).enabled(at < last).keys(keys(&[ctrl, "Shift", "["])).show(ui).clicked() {
                to = Some(last);
            }
            if let Some(to) = to {
                self.move_layer(layer, to);
                ui.close();
            }
        }
        crate::ui_kit::menu_sep(ui);
        if let Layer::Visual(_) = layer
            && Item::new("Duplicate").icon(icon::COPY).keys(keys(&[ctrl, "D"])).show(ui).clicked()
        {
            self.duplicate_layer(layer, aspect);
            ui.close();
        }
        self.copy_menu(ui, layer);
        let locked = self.settings.is_locked(layer);
        let lock = Item::new(if locked { "Unlock" } else { "Lock" }).icon(if locked { icon::LOCK_SIMPLE_OPEN } else { icon::LOCK_SIMPLE }).keys(keys(&[ctrl, "Shift", "L"]));
        let lock = lock.tip(if locked { "It moves on the preview again" } else { "It can't be moved on the preview, and clicks go through it" });
        if lock.show(ui).clicked() {
            self.settings.set_locked(layer, !locked);
            ui.close();
        }
        let on = self.layer_enabled(layer);
        if Item::new(if on { "Hide" } else { "Show" }).icon(if on { icon::EYE_SLASH } else { icon::EYE }).keys(keys(&["H"])).tip(if on { "Out of your clips, its settings kept" } else { "Back in your clips" }).show(ui).clicked() {
            self.set_layer_enabled(layer, !on);
            ui.close();
        }
        if !screen && Item::new("Remove").icon(icon::TRASH).danger().keys(keys(&["Del"])).tip("Take it out of your clips").show(ui).clicked() {
            self.remove_layer(layer);
            ui.close();
        }
    }

    /// Copy to scene (the same source into another scene, or every one,
    /// placed as here: one that has it already is placed so), and Unlink
    /// from other scenes (a copy of its own here; never a webcam, a camera
    /// opening once, nor the screen).
    fn copy_menu(&mut self, ui: &mut egui::Ui, layer: Layer) {
        let title = self.settings.layer_title(layer);
        // Each other scene: whether it has it, and whether it can't take it
        // (where a scene draws one webcam, one that has another).
        let others: Vec<(u64, String, bool, bool)> = self
            .settings
            .scenes
            .iter()
            .filter(|s| s.id != self.settings.active_scene)
            .map(|s| {
                let has = s.items.iter().any(|i| i.layer() == layer);
                let full = !capture::layers::MANY && matches!(layer, Layer::Webcam(_)) && s.items.iter().any(|i| matches!(i.layer(), Layer::Webcam(_)) && i.layer() != layer);
                (s.id, s.name.clone(), has, full)
            })
            .collect();
        if !others.is_empty() {
            let mut to: Vec<(u64, String)> = Vec::new();
            crate::ui_kit::submenu(ui, Item::new("Copy to scene").icon(icon::COPY_SIMPLE), |ui| {
                for (id, name, has, full) in &others {
                    let item = Item::new(name.clone()).check(*has).enabled(!*full);
                    let item = if *full {
                        item.tip("It has a webcam: a scene shows one here")
                    } else if *has {
                        item.tip("It's there: placed as here (where it is, its size and crop)")
                    } else {
                        item.tip("The same source, placed as here")
                    };
                    if item.show(ui).clicked() {
                        to = vec![(*id, name.clone())];
                    }
                }
                if others.len() > 1 {
                    crate::ui_kit::menu_sep(ui);
                    if Item::new("Every scene").tip("In each, placed as here").show(ui).clicked() {
                        to = others.iter().filter(|o| !o.3).map(|o| (o.0, o.1.clone())).collect();
                    }
                }
            });
            if !to.is_empty() {
                for (id, _) in &to {
                    self.settings.copy_to_scene(layer, *id);
                }
                let where_ = if to.len() == 1 { to[0].1.clone() } else { "every scene".to_owned() };
                self.toast_icon(icon::COPY_SIMPLE, format!("Copied {title} to {where_}"));
                ui.close();
            }
        }
        if layer == Layer::Screen || self.settings.scenes_with(layer).is_empty() {
            return;
        }
        let cam = matches!(layer, Layer::Webcam(_));
        let item = Item::new("Unlink from other scenes").icon(icon::LINK_BREAK).enabled(!cam);
        let item = if cam {
            item.tip("A camera opens once, in one format: what it shows is the same in every scene. Where it is, and its crop, are each scene's own")
        } else {
            item.tip("A copy of its own here: what it shows changes in this scene alone")
        };
        if item.show(ui).clicked()
            && let Some(new) = self.settings.unlink(layer)
        {
            if self.selected_layer() == Some(layer) {
                self.select_layer(new);
            }
            if let Some(crate::inspector_ui::Editing::Layer { layer: open, .. }) = &mut self.sources_view.editing
                && *open == layer
            {
                *open = new;
            }
            self.toast_icon(icon::LINK_BREAK, format!("{title} is this scene's own now"));
            ui.close();
        }
    }

    /// Whether a source's in the video (its switch, its eye in the list).
    pub(crate) fn layer_enabled(&self, layer: Layer) -> bool {
        match layer {
            Layer::Screen => self.settings.screen.enabled,
            Layer::Webcam(key) => self.settings.webcam(key).is_some_and(|w| w.enabled),
            Layer::Visual(key) => self.settings.visual(key).is_some_and(|v| v.enabled),
        }
    }

    pub(crate) fn set_layer_enabled(&mut self, layer: Layer, on: bool) {
        match layer {
            Layer::Screen => self.settings.screen.enabled = on,
            Layer::Webcam(key) => {
                if let Some(w) = self.settings.webcam_mut(key) {
                    w.enabled = on;
                }
            }
            Layer::Visual(key) => {
                if let Some(v) = self.settings.visual_mut(key) {
                    v.enabled = on;
                }
            }
        }
    }

    /// The Sources page's keys, while nothing's being typed and no menu or
    /// dialog is open: Ctrl+Z and Ctrl+Shift+Z (or Ctrl+Y) undo and redo,
    /// and for the source selected, Delete removes it, the arrows nudge it a
    /// pixel (Shift: ten), H hides or shows it, Ctrl+D duplicates it,
    /// Ctrl+[ and Ctrl+] send it back or forward (Shift: all the way), and
    /// Esc lets go of it. `aspect`: the video's shape.
    pub(crate) fn source_keys(&mut self, ctx: &egui::Context, aspect: f32) {
        if ctx.egui_wants_keyboard_input() || crate::ui_kit::overlay_open(ctx) || egui::Popup::is_any_open(ctx) {
            return;
        }
        use egui::{Key, Modifiers};
        let (redo, undo) = ctx.input_mut(|i| (i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z) || i.consume_key(Modifiers::COMMAND, Key::Y), i.consume_key(Modifiers::COMMAND, Key::Z)));
        if undo {
            self.undo_arrangement();
        }
        if redo {
            self.redo_arrangement();
        }
        let Some(layer) = self.selected_layer() else {
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                self.canvas.selected = None;
            }
            return;
        };
        // Cropping on the preview: Esc or Enter is done (it stays selected).
        if self.canvas.crop_mode.is_some() && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape) || i.consume_key(Modifiers::NONE, Key::Enter)) {
            self.canvas.crop_mode = None;
            return;
        }
        let was = self.arrangement();
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::L)) {
            let locked = self.settings.is_locked(layer);
            self.settings.set_locked(layer, !locked);
        }
        let (remove, hide, dup, esc, back, forward, mods) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::Delete) || i.consume_key(Modifiers::NONE, Key::Backspace),
                i.consume_key(Modifiers::NONE, Key::H),
                i.consume_key(Modifiers::COMMAND, Key::D),
                i.consume_key(Modifiers::NONE, Key::Escape),
                i.key_pressed(Key::OpenBracket) && i.modifiers.command,
                i.key_pressed(Key::CloseBracket) && i.modifiers.command,
                i.modifiers,
            )
        });
        let step = if mods.shift { 10.0 } else { 1.0 };
        let nudge = ctx.input(|i| {
            let mut d = vec2(0.0, 0.0);
            if !i.modifiers.command && !i.modifiers.alt {
                if i.key_pressed(Key::ArrowLeft) {
                    d.x -= step;
                }
                if i.key_pressed(Key::ArrowRight) {
                    d.x += step;
                }
                if i.key_pressed(Key::ArrowUp) {
                    d.y -= step;
                }
                if i.key_pressed(Key::ArrowDown) {
                    d.y += step;
                }
            }
            d
        });
        if esc {
            self.canvas.selected = None;
            return;
        }
        if remove {
            if layer == Layer::Screen {
                self.toast_icon(icon::INFO, "The screen stays in the list: H hides it");
            } else {
                self.remove_layer(layer);
            }
        }
        if hide {
            let on = self.layer_enabled(layer);
            self.set_layer_enabled(layer, !on);
        }
        if dup {
            match layer {
                Layer::Visual(_) => self.duplicate_layer(layer, aspect),
                Layer::Webcam(_) => self.toast_icon(icon::INFO, "A camera can be in a scene once"),
                Layer::Screen => {}
            }
        }
        if back || forward {
            let order = self.layer_order();
            let at = order.iter().position(|l| *l == layer).unwrap_or(0);
            let to = match (forward, mods.shift) {
                (true, true) => 0,
                (true, false) => at.saturating_sub(1),
                (false, true) => order.len().saturating_sub(1),
                (false, false) => (at + 1).min(order.len().saturating_sub(1)),
            };
            self.move_layer(layer, to);
        }
        if nudge != vec2(0.0, 0.0) && !self.settings.is_locked(layer) {
            self.nudge_layer(layer, nudge, aspect);
        }
        if self.arrangement() != was {
            // Nudges in a row are one step.
            if nudge != vec2(0.0, 0.0) && !remove && !hide && !dup {
                self.keep_for_undo_as(was, format!("nudge {layer:?}"));
            } else {
                self.keep_for_undo(was);
            }
        }
    }

    /// A source moved by `by` of the video's pixels (one not placed by hand
    /// yet goes into a box of its own where it is, first).
    pub(crate) fn nudge_layer(&mut self, layer: Layer, by: egui::Vec2, aspect: f32) {
        let Some((vw, vh)) = capture::frame_size(&self.video_source(), self.settings.video_size()) else { return };
        let d = vec2(by.x / vw.max(1) as f32, by.y / vh.max(1) as f32);
        match layer {
            Layer::Screen => {
                let r = self.screen_rect(aspect).translate(d);
                self.place_screen(r, aspect);
            }
            Layer::Webcam(key) => {
                if let Some(w) = self.settings.webcam_mut(key) {
                    w.placement.x += d.x;
                    w.placement.y += d.y;
                }
            }
            Layer::Visual(key) => {
                let r = self.visual_rect(key, aspect).translate(d);
                let picture = self.settings.visual(key).and_then(|v| v.placed).and_then(|b| b.picture_aspect).or_else(|| self.visual_size(key).map(|(w, h)| w as f32 / h.max(1) as f32));
                let shapeless = self.settings.visual(key).is_some_and(|v| v.visual.shapeless());
                if let Some(v) = self.settings.visual_mut(key) {
                    v.placed = Some(BoxCfg::new([r.min.x, r.min.y, r.width(), r.height()], aspect, if shapeless { None } else { picture }));
                }
            }
        }
    }

    /// A source put at place `to` in the order (0: in front).
    pub(crate) fn move_layer(&mut self, layer: Layer, to: usize) {
        let mut order = self.layer_order();
        let Some(at) = order.iter().position(|l| *l == layer) else { return };
        order.remove(at);
        order.insert(to.min(order.len()), layer);
        self.set_layer_order(&order);
    }

    /// A webcam or boxed source taken out of the scene shown (the screen
    /// stays: it's hidden instead); gone for good once no other scene has
    /// it. Ctrl+Z puts it back.
    pub(crate) fn remove_layer(&mut self, layer: Layer) {
        if layer == Layer::Screen {
            return;
        }
        let name = self.settings.layer_title(layer);
        let gone = self.settings.remove_from_scene(layer);
        if self.selected_layer() == Some(layer) {
            self.canvas.selected = None;
        }
        let ctrl = crate::shortcuts::ctrl_name();
        let what = if gone { format!("Removed {name}") } else { format!("Took {name} out of this scene") };
        self.toast_icon(icon::TRASH, format!("{what} · {ctrl}+Z puts it back"));
    }

    /// A copy of a boxed source, in front of it, a little down and right,
    /// selected.
    pub(crate) fn duplicate_layer(&mut self, layer: Layer, aspect: f32) {
        let Layer::Visual(key) = layer else { return };
        let r = self.visual_rect(key, aspect);
        let Some(mut copy) = self.settings.visual(key).cloned() else { return };
        let new = self.settings.new_key();
        copy.key = new;
        copy.title = copy.title.map(|t| format!("{t} copy"));
        let picture = copy.placed.and_then(|b| b.picture_aspect);
        let moved = r.translate(vec2(0.02, 0.02 * aspect));
        copy.placed = Some(BoxCfg::new([moved.min.x, moved.min.y, moved.width(), moved.height()], aspect, picture));
        self.settings.visuals.push(copy);
        let at = self.layer_order().iter().position(|l| *l == layer).unwrap_or(0);
        let mut order = self.layer_order();
        order.retain(|l| *l != Layer::Visual(new));
        order.insert(at, Layer::Visual(new));
        self.set_layer_order(&order);
        self.select_layer(Layer::Visual(new));
    }

    /// How a source is mirrored and flipped.
    pub(crate) fn layer_flips(&self, layer: Layer) -> (bool, bool) {
        match layer {
            Layer::Screen => (self.settings.screen.flip_h, self.settings.screen.flip_v),
            Layer::Webcam(key) => self.webcam_placement(key).map_or((false, false), |p| (p.flip_h, p.flip_v)),
            Layer::Visual(key) => self.settings.visual(key).map_or((false, false), |d| (d.flip_h, d.flip_v)),
        }
    }

    /// How a source is turned, in quarters clockwise as it's seen.
    pub(crate) fn layer_turns(&self, layer: Layer) -> u8 {
        match layer {
            Layer::Screen => self.settings.screen.turns % 4,
            Layer::Webcam(key) => self.webcam_placement(key).map_or(0, |p| p.turns % 4),
            Layer::Visual(key) => self.settings.visual(key).map_or(0, |d| d.turns % 4),
        }
    }

    /// Whether a source is cropped, when it can be (`None`: it can't, as
    /// games and apps, a colour or words can't).
    pub(crate) fn layer_cropped(&self, layer: Layer) -> Option<bool> {
        match layer {
            Layer::Screen => self.crop_whole().map(|_| self.settings.screen_crop.is_some_and(|c| !c.is_none())),
            Layer::Webcam(key) => Some(self.webcam_placement(key).is_some_and(|p| p.crop.iter().any(|c| *c > 0.0))),
            Layer::Visual(key) => self.settings.visual(key).filter(|v| v.visual.croppable()).map(|v| v.crop.is_some_and(|c| !c.is_none())),
        }
    }

    /// All of a source's picture again.
    pub(crate) fn layer_uncrop(&mut self, layer: Layer, aspect: f32) {
        match layer {
            Layer::Screen => self.settings.screen_crop = None,
            Layer::Webcam(key) => self.webcam_uncropped(key, aspect),
            Layer::Visual(key) => self.visual_uncrop(key, aspect),
        }
    }

    /// Whether a source's position, and its size, can be reset.
    pub(crate) fn layer_resettable(&mut self, layer: Layer, aspect: f32) -> (bool, bool) {
        match layer {
            Layer::Screen => self.screen_resettable(aspect),
            Layer::Webcam(key) => self.webcam_resettable(key),
            Layer::Visual(key) => {
                if self.settings.visual(key).is_none_or(|d| d.placed.is_none()) {
                    return (false, false);
                }
                let (r, first) = (self.visual_rect(key, aspect), self.visual_first_rect(key, aspect));
                (!same(r, centred(r)), (r.size() - first.size()).length() > 1e-3)
            }
        }
    }

    /// A source back in the middle of the video (a webcam: its corner), its size kept.
    pub(crate) fn layer_reset_position(&mut self, layer: Layer, aspect: f32) {
        match layer {
            Layer::Screen => self.screen_reset_position(aspect),
            Layer::Webcam(key) => self.webcam_reset_position(key, aspect),
            Layer::Visual(key) => {
                let r = centred(self.visual_rect(key, aspect));
                self.place_visual(key, r, aspect);
            }
        }
    }

    /// A source back to its first size, where it is.
    pub(crate) fn layer_reset_size(&mut self, layer: Layer, aspect: f32) {
        match layer {
            Layer::Screen => self.screen_reset_size(aspect),
            Layer::Webcam(key) => self.webcam_reset_size(key, aspect),
            Layer::Visual(key) => {
                let r = resized_in_place(self.visual_rect(key, aspect), self.visual_first_rect(key, aspect).size());
                self.place_visual(key, r, aspect);
            }
        }
    }

    /// A source turned a quarter more, clockwise (`by` 1) or
    /// anticlockwise, as it's seen.
    pub(crate) fn turn_layer(&mut self, layer: Layer, by: i8, aspect: f32) {
        match layer {
            Layer::Screen => self.screen_turn(by, aspect),
            Layer::Webcam(key) => {
                if let Some(w) = self.settings.webcam_mut(key) {
                    let p: Placement = w.placement.into();
                    w.placement = PlacementCfg::laid_out(webcam_ui::contain(p.turn(by, aspect)), aspect);
                }
            }
            Layer::Visual(key) => {
                if let Some(d) = self.settings.visual_mut(key) {
                    let by = if d.flip_h != d.flip_v { -by } else { by };
                    d.turns = (d.turns as i8 + by).rem_euclid(4) as u8;
                    d.placed = d.placed.map(|b| turned_box(b, aspect));
                }
            }
        }
    }

    /// A source mirrored (`across`: left and right swapped), or flipped
    /// upside down; again, back.
    pub(crate) fn flip_layer(&mut self, layer: Layer, across: bool, aspect: f32) {
        match layer {
            Layer::Screen => {
                let s = &mut self.settings.screen;
                if across { s.flip_h = !s.flip_h } else { s.flip_v = !s.flip_v }
            }
            Layer::Webcam(key) => {
                if let Some(w) = self.settings.webcam_mut(key) {
                    let p: Placement = w.placement.into();
                    let p = if across { Placement { flip_h: !p.flip_h, ..p } } else { Placement { flip_v: !p.flip_v, ..p } };
                    w.placement = PlacementCfg::laid_out(p, aspect);
                }
            }
            Layer::Visual(key) => {
                if let Some(d) = self.settings.visual_mut(key) {
                    if across { d.flip_h = !d.flip_h } else { d.flip_v = !d.flip_v }
                }
            }
        }
    }
}

/// A box turned a quarter, for its picture turned: the new shape around its
/// middle, shrunk to stay in the video if it was in it.
fn turned_box(b: BoxCfg, aspect: f32) -> BoxCfg {
    let r = Rect::from_min_size(Pos2::new(b.x, b.y), vec2(b.w, b.h));
    let mut size = vec2(r.height() / aspect, r.width() * aspect);
    if r.width() <= 1.0 + 1e-4 && r.height() <= 1.0 + 1e-4 {
        size *= (1.0 / size.x).min(1.0 / size.y).min(1.0);
    }
    let mut t = Rect::from_center_size(r.center(), size);
    if size.x <= 1.0 && size.y <= 1.0 {
        t = t.translate(vec2((-t.min.x).max(0.0) - (t.max.x - 1.0).max(0.0), (-t.min.y).max(0.0) - (t.max.y - 1.0).max(0.0)));
    }
    BoxCfg::new([t.min.x, t.min.y, t.width(), t.height()], aspect, b.picture_aspect.map(|a| 1.0 / a))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::from_min_size(Pos2::new(x, y), vec2(w, h))
    }

    fn close(a: Rect, b: Rect) -> bool {
        (a.min - b.min).length() < 1e-4 && (a.max - b.max).length() < 1e-4
    }

    /// The screen's box moves inside the video, snapping to its middle and
    /// to the webcam's edges; a corner scales it from the opposite one,
    /// keeping its shape, no smaller than the least.
    /// Clicks at one place select the sources there in turn, front first,
    /// round again from the back; the one selected is what's dragged there.
    #[test]
    fn clicks_go_down_through_the_sources() {
        let (a, b, c) = (Layer::Webcam(1), Layer::Visual(2), Layer::Screen);
        assert_eq!(clicked_through(&[a, b, c], None), Some(a));
        assert_eq!(clicked_through(&[a, b, c], Some(a)), Some(b));
        assert_eq!(clicked_through(&[a, b, c], Some(c)), Some(a));
        assert_eq!(clicked_through(&[b, c], Some(a)), Some(b), "one selected elsewhere: the one in front");
        assert_eq!(clicked_through(&[], Some(a)), None);
        let layers = [(a, rect(0.5, 0.5, 0.3, 0.3)), (c, rect(0.0, 0.0, 1.0, 1.0))];
        let shown = |r: Rect| Rect::from_min_max((r.min.to_vec2() * 1000.0).to_pos2(), (r.max.to_vec2() * 1000.0).to_pos2());
        let p = Pos2::new(600.0, 600.0);
        assert_eq!(hit(&layers, &shown, p, None), Some((a, Handle::Move)));
        assert_eq!(hit(&layers, &shown, p, Some(c)), Some((c, Handle::Move)), "the screen, selected, under the webcam");
    }

    #[test]
    fn the_screens_box() {
        let none = Lines::default();
        let start = rect(0.1, 0.1, 0.4, 0.4);
        assert!(close(screen_drag(start, Handle::Move, 0.1, 0.0, false, false, &none), rect(0.2, 0.1, 0.4, 0.4)));
        assert!(close(screen_drag(start, Handle::Move, 0.9, 0.9, false, false, &none), rect(0.6, 0.6, 0.4, 0.4)));
        // Its middle onto the video's, from close by.
        assert!(close(screen_drag(start, Handle::Move, 0.195, 0.0, true, false, &none), rect(0.3, 0.1, 0.4, 0.4)));
        // Its right edge onto the webcam's left.
        let webcam = Lines { xs: vec![0.75], ys: vec![] };
        assert!(close(screen_drag(start, Handle::Move, 0.245, 0.0, true, false, &webcam), rect(0.35, 0.1, 0.4, 0.4)));
        // The bottom-right corner, dragged out: twice as big from the top-left.
        let r = screen_drag(start, Handle::SE, 0.4, 0.4, false, false, &none);
        assert!(close(r, rect(0.1, 0.1, 0.8, 0.8)), "{r:?}");
        // Only as big as fits.
        let r = screen_drag(start, Handle::SE, 2.0, 2.0, false, false, &none);
        assert!(close(r, rect(0.1, 0.1, 0.9, 0.9)), "{r:?}");
        // Never smaller than the least, the other corner where it was.
        let r = screen_drag(start, Handle::NW, 1.0, 1.0, false, false, &none);
        assert!((r.width() - MIN_SIZE).abs() < 1e-4 && (r.max - start.max).length() < 1e-4, "{r:?}");
        // Shift: stretched, each side on its own, as far as the video's edges.
        let r = screen_drag(start, Handle::SE, 0.4, 2.0, false, true, &none);
        assert!(close(r, rect(0.1, 0.1, 0.8, 0.9)), "{r:?}");
        let r = screen_drag(start, Handle::E, 0.2, 0.3, false, true, &none);
        assert!(close(r, rect(0.1, 0.1, 0.6, 0.4)), "{r:?}");
    }

    /// Fill, Fit and Stretch as boxes: Fill's middle covering the video,
    /// past its edges; Fit's all of it, as big as fits; Stretch's all of
    /// the video. In the video's own shape (but for rounding), all of it.
    /// Taken hold of to crop, Fill's starts from the part showing.
    #[test]
    fn where_the_screen_is_put() {
        let (mac, wide) = (3024.0 / 1964.0, 16.0 / 9.0);
        let fill = framed(capture::Framing::Fill, mac, wide);
        assert!(fills(fill) && !stuck(fill), "{fill:?}");
        assert!((fill.width() - 1.0).abs() < 1e-4 && (fill.height() - wide / mac).abs() < 1e-4 && (fill.center().y - 0.5).abs() < 1e-4, "{fill:?}");
        assert!(close(framed(capture::Framing::Fit, mac, wide), fitted(mac, wide)));
        assert!(stuck(framed(capture::Framing::Stretch, mac, wide)));
        // 1662×1080: the display at 1080p, its shape but for rounding.
        assert!(stuck(framed(capture::Framing::Fill, mac, 1662.0 / 1080.0)));
        let (seen, part) = in_video(fill, rect(0.0, 0.0, 3024.0, 1964.0));
        assert!(stuck(seen), "{seen:?}");
        assert!((part.width() - 3024.0).abs() < 0.5 && (part.height() - 3024.0 / wide).abs() < 0.5 && (part.center().y - 982.0).abs() < 0.5, "{part:?}");
        let inside = rect(0.1, 0.1, 0.5, 0.5);
        assert_eq!(in_video(inside, rect(0.0, 0.0, 100.0, 100.0)), (inside, rect(0.0, 0.0, 100.0, 100.0)));
    }

    /// A screen bigger than the video (Fill's) moves to show another part of
    /// it, always over all of the video; a box hanging past an edge doesn't
    /// jump in as it's taken hold of, nor go further out; and taking hold
    /// of a handle doesn't make either jump.
    #[test]
    fn a_screen_bigger_than_the_video() {
        let none = Lines::default();
        // A 16:10 screen over a 16:9 video: past its top and bottom.
        let over = covering(1.6, 16.0 / 9.0);
        assert!(close(over, rect(0.0, -1.0 / 18.0, 1.0, 10.0 / 9.0)), "{over:?}");
        // Down as far as it goes, its top at the video's; not sideways.
        let r = screen_drag(over, Handle::Move, 0.3, 0.5, false, false, &none);
        assert!(close(r, rect(0.0, 0.0, 1.0, 10.0 / 9.0)), "{r:?}");
        let r = screen_drag(over, Handle::Move, 0.0, -0.5, false, false, &none);
        assert!(close(r, rect(0.0, -1.0 / 9.0, 1.0, 10.0 / 9.0)), "{r:?}");
        // Its middle snaps back onto the video's.
        assert!(close(screen_drag(over, Handle::Move, 0.0, 0.005, true, false, &none), over));
        // Hanging past the top (made smaller from below): it moves in, not out.
        let hanging = rect(0.0, -0.05, 0.9, 1.0);
        assert!(close(screen_drag(hanging, Handle::Move, 0.0, 0.0, false, false, &none), hanging));
        assert!(close(screen_drag(hanging, Handle::Move, 0.0, -0.1, false, false, &none), hanging));
        assert!(close(screen_drag(hanging, Handle::Move, 0.0, 0.2, false, false, &none), rect(0.0, 0.0, 0.9, 1.0)));
        // Its side handle: where it was, then smaller, keeping its shape.
        assert!(close(screen_drag(over, Handle::E, 0.0, 0.0, false, false, &none), over));
        let r = screen_drag(over, Handle::E, -0.5, 0.0, false, false, &none);
        assert!(close(r, rect(0.0, 0.5 - 5.0 / 18.0, 0.5, 5.0 / 9.0)), "{r:?}");
    }

    /// Cropped with Alt: the picture keeps its scale and place, the box
    /// follows the crop's edge; it can't be uncropped past the display or
    /// out of the video, and all of the display again is no crop.
    #[test]
    fn cropping_the_screen() {
        let crop = |w, h, x, y| Some(ScreenCrop { w, h, at: Some([x, y]) });
        // Filling the video: the middle 1920×1080 of a 2560×1440 display,
        // its left edge dragged a quarter of the video in.
        let (b, c) = screen_crop_drag(rect(0.0, 0.0, 1.0, 1.0), rect(320.0, 180.0, 1920.0, 1080.0), (2560, 1440), Handle::W, 0.25, 0.0, false);
        assert!(close(b, rect(0.25, 0.0, 0.75, 1.0)), "{b:?}");
        assert_eq!(c, crop(1440, 1080, 800, 180));
        // Out past the video's edge: the box stays in the video.
        let (b, c) = screen_crop_drag(rect(0.0, 0.0, 1.0, 1.0), rect(320.0, 180.0, 1920.0, 1080.0), (2560, 1440), Handle::W, -0.5, 0.0, false);
        assert!(close(b, rect(0.0, 0.0, 1.0, 1.0)), "{b:?}");
        assert_eq!(c, crop(1920, 1080, 320, 180));
        // In a box of its own: its right edge out to the display's, then all of it.
        let (b, c) = screen_crop_drag(rect(0.1, 0.1, 0.4, 0.4), rect(0.0, 0.0, 1280.0, 1440.0), (2560, 1440), Handle::E, 0.9, 0.0, false);
        assert!(close(b, rect(0.1, 0.1, 0.8, 0.4)), "{b:?}");
        assert_eq!(c, None);
        // Never less than 64 of its pixels, on even ones.
        let (_, c) = screen_crop_drag(rect(0.0, 0.0, 1.0, 1.0), rect(0.0, 0.0, 2560.0, 1440.0), (2560, 1440), Handle::SE, -2.0, -2.0, false);
        assert_eq!(c, crop(64, 64, 0, 0));
        let (_, c) = screen_crop_drag(rect(0.0, 0.0, 1.0, 1.0), rect(0.0, 0.0, 2560.0, 1440.0), (2560, 1440), Handle::N, 0.0, 0.10011, false);
        assert_eq!(c, crop(2560, 1296, 0, 144));
    }

    /// Alt pressed while a corner's resizing the screen crops it instead,
    /// from how it was when it was taken hold of (the picture at its own
    /// scale), and let go, it's resized again, its crop as it was; as Shift
    /// stretches it and lets it keep its shape again.
    #[test]
    fn alt_mid_drag_crops_and_lets_go() {
        let none = Lines::default();
        let start = rect(0.0, 0.0, 1.0, 1.0);
        let hold = ScreenHold { handle: Handle::SE, from: Pos2::ZERO, start, crop: None, picture: Some(16.0 / 9.0), cropping_from: Some((start, rect(0.0, 0.0, 1920.0, 1080.0), (1920, 1080))), turned: Turned::default(), display: (1920, 1080) };
        let mods = |crop: bool, keep: bool| Mods { crop, snap: false, keep };
        // Made half as big, from its top-left corner.
        let (r, crop, picture) = hold.dragged(-0.5, -0.5, mods(false, false), &none);
        assert!(close(r, rect(0.0, 0.0, 0.5, 0.5)) && crop.is_none() && picture == Some(16.0 / 9.0), "{r:?}");
        // Alt: the same corner crops instead: its right and bottom halves cut off.
        let (r, crop, picture) = hold.dragged(-0.5, -0.5, mods(true, false), &none);
        assert!(close(r, rect(0.0, 0.0, 0.5, 0.5)), "{r:?}");
        assert_eq!(crop, Some(ScreenCrop { w: 960, h: 540, at: Some([0, 0]) }));
        assert_eq!(picture, Some(960.0 / 540.0));
        // Alt let go: resized again, uncropped.
        let (r, crop, _) = hold.dragged(-0.3, -0.3, mods(false, false), &none);
        assert!(close(r, rect(0.0, 0.0, 0.7, 0.7)) && crop.is_none(), "{r:?}");
        // Shift: stretched; and cropping, it keeps the crop's shape.
        let (r, _, _) = hold.dragged(-0.5, -0.2, mods(false, true), &none);
        assert!(close(r, rect(0.0, 0.0, 0.5, 0.8)), "{r:?}");
        let (r, _, _) = hold.dragged(-0.5, -0.2, mods(true, true), &none);
        assert!((r.width() / r.height() - 1.0).abs() < 0.01, "{r:?}");
        // Games and apps can't be cropped: Alt resizes.
        let apps = ScreenHold { cropping_from: None, ..hold };
        let (r, crop, _) = apps.dragged(-0.5, -0.5, mods(true, false), &none);
        assert!(close(r, rect(0.0, 0.0, 0.5, 0.5)) && crop.is_none(), "{r:?}");
    }

    /// Reset position: in the middle, its size kept (one bigger than the
    /// video covering it evenly). Reset size: where it is, in its corner or
    /// edge or the middle, as it was.
    #[test]
    fn resetting_position_or_size_alone() {
        assert!(close(centred(rect(0.1, 0.2, 0.4, 0.4)), rect(0.3, 0.3, 0.4, 0.4)));
        assert!(close(centred(rect(0.0, 0.0, 1.0, 1.2)), rect(0.0, -0.1, 1.0, 1.2)));
        let quarter = vec2(0.25, 0.25);
        // Bottom-right corner, top-left corner, the middle: still there.
        assert!(close(resized_in_place(rect(0.6, 0.6, 0.4, 0.4), quarter), rect(0.75, 0.75, 0.25, 0.25)));
        assert!(close(resized_in_place(rect(0.0, 0.0, 0.4, 0.4), quarter), rect(0.0, 0.0, 0.25, 0.25)));
        assert!(close(resized_in_place(rect(0.3, 0.3, 0.4, 0.4), quarter), rect(0.375, 0.375, 0.25, 0.25)));
        // All of the video, at Fill's size (bigger): over it, in the middle.
        assert!(close(resized_in_place(rect(0.0, 0.0, 1.0, 1.0), vec2(1.0, 1.2)), rect(0.0, -0.1, 1.0, 1.2)));
    }

    /// A part of a turned or flipped screen, between the display's own
    /// pixels (where its crop is kept) and the picture as it shows: turned a
    /// quarter clockwise, its left half is the top half; back, it's the left
    /// half again; mirrored, the left is the right.
    #[test]
    fn a_part_of_a_turned_screen() {
        let turned = Turned { turns: 1, ..Default::default() };
        assert_eq!(turned.size((1920, 1080)), (1080, 1920));
        let left = rect(0.0, 0.0, 960.0, 1080.0);
        let top = turned.part(left, (1920, 1080), false);
        assert!(close(top, rect(0.0, 0.0, 1080.0, 960.0)), "{top:?}");
        assert!(close(turned.part(top, (1920, 1080), true), left));
        let mirrored = Turned { flip_h: true, ..Default::default() };
        assert!(close(mirrored.part(left, (1920, 1080), false), rect(960.0, 0.0, 960.0, 1080.0)));
        // A crop dragged as it shows (its top half), kept in the display's pixels.
        let hold = ScreenHold { handle: Handle::S, from: Pos2::ZERO, start: rect(0.0, 0.0, 1.0, 1.0), crop: None, picture: None, cropping_from: None, turned, display: (1920, 1080) };
        assert_eq!(hold.own_crop(ScreenCrop { w: 1080, h: 960, at: Some([0, 0]) }, (1080, 1920)), ScreenCrop { w: 960, h: 1080, at: Some([0, 0]) });
        // Every way round, there and back.
        for turns in 0..4 {
            for (flip_h, flip_v) in [(false, false), (true, false), (false, true), (true, true)] {
                let t = Turned { turns, flip_h, flip_v };
                let part = rect(100.0, 200.0, 300.0, 400.0);
                assert!(close(t.part(t.part(part, (1920, 1080), false), (1920, 1080), true), part), "{t:?}");
            }
        }
    }

    /// Guides show for the lines a box is on.
    #[test]
    fn guides_for_what_it_snapped_to() {
        let webcam = Lines { xs: vec![0.75], ys: vec![0.2] };
        assert_eq!(snapped(rect(0.35, 0.3, 0.4, 0.4), &webcam), vec![(true, 0.75), (false, 0.5)]);
        assert!(snapped(rect(0.31, 0.31, 0.2, 0.2), &webcam).is_empty());
    }
}
