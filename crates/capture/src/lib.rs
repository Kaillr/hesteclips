//! Capture backend abstraction.
//!
//! macOS uses `sck` (ScreenCaptureKit + VideoToolbox), Windows uses `win`
//! (Windows Graphics Capture + Media Foundation + WASAPI), Linux uses `linux`
//! (the desktop portal + PipeWire, encoded by ffmpeg), all behind the same
//! `Recorder` trait so the app never depends on which one is live. Mics come
//! from cpal on macOS and Windows, from PipeWire on Linux, and are mixed in
//! `mixer`.

use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
mod aac;
pub mod audio;
#[cfg(target_os = "macos")]
mod avwriter;
pub mod colours;
pub mod h264;
pub mod mixer;
/// Hearing a microphone live: through PipeWire on Linux (`linux::monitor`).
#[cfg(not(target_os = "linux"))]
pub mod monitor;
#[cfg(target_os = "linux")]
pub use linux::monitor;
pub mod mp4meta;
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
pub(crate) mod mp4read;
#[cfg(any(target_os = "windows", target_os = "linux"))]
mod mp4file;
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
mod mp4mux;
pub mod output;
pub mod preview;
pub mod webcam;
pub mod sources;
mod shutter;
pub mod export;
mod steady;
pub mod stores;

#[cfg(target_os = "macos")]
pub mod mac;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "macos")]
pub mod sck;
#[cfg(target_os = "windows")]
pub mod win;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
mod writer;

/// The in-process hardware video decoder, for playback (same API on both).
pub use shutter::BlendShape;
#[cfg(target_os = "macos")]
pub use mac::decode;
#[cfg(target_os = "windows")]
pub use win::decode;
#[cfg(target_os = "linux")]
pub use linux::decode;

#[cfg(target_os = "macos")]
pub use sck::SckRecorder;
#[cfg(target_os = "windows")]
pub use win::WinRecorder;
#[cfg(target_os = "linux")]
pub use linux::LinuxRecorder;

/// The capture backend for this platform. `live` gets the meters and supplies
/// each source's volume.
pub fn default_recorder(live: std::sync::Arc<mixer::LiveAudio>) -> Box<dyn Recorder> {
    #[cfg(target_os = "macos")]
    {
        Box::new(SckRecorder::new(live))
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(WinRecorder::new(live))
    }
    #[cfg(target_os = "linux")]
    {
        Box::new(LinuxRecorder::new(live))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = live;
        Box::new(Unsupported)
    }
}

/// Stand-in where there's no backend yet, so the app runs (library, editor)
/// and says why capture won't start.
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
struct Unsupported;

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
impl Recorder for Unsupported {
    fn start(&mut self, _: Mode, _: &EncodeSettings) -> anyhow::Result<()> {
        anyhow::bail!("recording isn't available on this platform yet")
    }
    fn save_clip(&mut self, _: &Path) -> anyhow::Result<PendingClip> {
        anyhow::bail!("recording isn't available on this platform yet")
    }
    fn stop(&mut self, _: Option<&Path>) -> anyhow::Result<Option<PathBuf>> {
        Ok(None)
    }
    fn is_running(&self) -> bool {
        false
    }
}

/// The largest side the H.264 hardware encoders take: VideoToolbox refuses
/// anything wider or taller than 4096 (error -12903; measured with
/// `examples/vt_limits`), as do NVENC, AMF, Quick Sync and VA-API for H.264.
pub const MAX_SIDE: u32 = 4096;

/// The recording's size for a source `w`×`h`: its short side downscaled to
/// `short_side` if that's smaller (1080 makes a 16:9 picture 1920×1080 and
/// a 9:16 one 1080×1920), and to fit [`MAX_SIDE`] (a 5K2K ultrawide,
/// 6720×2836 natively, records at 4096×1728). Keeps the shape; even
/// dimensions, as H.264 4:2:0 requires.
pub fn output_size(w: u32, h: u32, short_side: Option<u32>) -> (u32, u32) {
    let (w, h) = (w.max(2) as f64, h.max(2) as f64);
    let fits = (MAX_SIDE as f64 / w.max(h)).min(1.0);
    let scale = short_side.map_or(1.0, |t| (t as f64 / w.min(h)).min(1.0)).min(fits);
    let even = |v: f64| ((v.round() as u32) & !1).max(2);
    (even(w * scale), even(h * scale))
}

/// How the screen goes into a recording of another shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Framing {
    /// All of it, with black bars where the shapes differ.
    #[default]
    Fit,
    /// Its middle, as much as fills the shape: what's past that is cut off
    /// (an ultrawide screen's sides, recorded 16:9).
    Fill,
    /// All of it, stretched or squeezed into the shape.
    Stretch,
}

/// A recording's shape when it isn't the screen's: `w`:`h`, and how the
/// screen goes into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    pub w: u32,
    pub h: u32,
    pub framing: Framing,
}

impl Shape {
    /// A `w`×`h` screen in this shape, at the screen's own scale: the part
    /// of it kept (Fill), all of it with bars (Fit), or as tall (Stretch).
    pub fn of(&self, w: u32, h: u32) -> (u32, u32) {
        let shape = self.w.max(1) as f64 / self.h.max(1) as f64;
        let (w, h) = (w as f64, h as f64);
        let wider = w / h > shape;
        let (w, h) = match self.framing {
            Framing::Fill if wider => (h * shape, h),
            Framing::Fill => (w, w / shape),
            Framing::Fit if wider => (w, w / shape),
            Framing::Fit => (h * shape, h),
            Framing::Stretch => (h * shape, h),
        };
        (w.round() as u32, h.round() as u32)
    }
}

/// The size recordings are made at, whatever size the screen turns out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VideoSize {
    /// The short side brought down to this (never up): 1080 is "1080p", a
    /// 9:16 recording's too. `None` keeps the screen's own size.
    pub short_side: Option<u32>,
    /// A shape to record in, when it isn't the screen's.
    pub shape: Option<Shape>,
    /// Exactly this size, whatever the screen's: `short_side` and the size
    /// `shape` gives don't apply (its framing still does).
    pub exact: Option<(u32, u32)>,
}

impl VideoSize {
    /// The size a `w`×`h` screen is recorded at (all of it, cropped or not):
    /// exactly the size asked for, or in `shape` if there is one, then as
    /// [`output_size`] makes it.
    pub fn of(&self, w: u32, h: u32) -> (u32, u32) {
        if let Some((w, h)) = self.exact {
            return output_size(w, h, None);
        }
        let (w, h) = self.shape.map_or((w, h), |s| s.of(w, h));
        output_size(w, h, self.short_side)
    }

    /// How the picture goes into the frame: as the shape says, else all of
    /// it (a window that isn't the screen's shape goes in whole).
    pub fn framing(&self) -> Framing {
        self.shape.map_or(Framing::Fit, |s| s.framing)
    }
}

/// Where the screen's picture goes in the frame: into a box (all of the
/// frame, unless it's been placed by hand), as `framing` says, in front of
/// the webcam or behind it, and the part of it shown. Shared with the app
/// ([`SharedScreen`]), which changes it as you drag the screen around the
/// preview or crop it; the capture reads it every frame, so it changes at
/// once, while recording too. None of it changes the frame's size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenPlacement {
    /// The box: left, top, width, height as fractions of the frame. It can
    /// reach past the frame's edges (a screen bigger than the video, part
    /// of it showing): only what's in the frame is drawn.
    pub rect: [f32; 4],
    /// How the picture goes into the box.
    pub framing: Framing,
    /// Drawn over the webcam, instead of under it.
    pub in_front: bool,
    /// Not drawn at all: the webcam shows on black.
    pub hidden: bool,
    /// The part of a display shown, in its pixels, if not all of it (a game
    /// at a lower resolution than the display, black around it): it goes
    /// into the box as all of the picture would. Never a window's.
    pub crop: Option<Crop>,
    /// Quarter turns clockwise, then mirrored (left and right swapped) or
    /// flipped upside down, as the webcam's are (`webcam::Placement`): the
    /// picture as it shows, which is what goes into the box.
    pub turns: u8,
    pub flip_h: bool,
    pub flip_v: bool,
}

/// The screen's placement, shared between the app and a running capture.
pub type SharedScreen = std::sync::Arc<std::sync::Mutex<ScreenPlacement>>;

impl ScreenPlacement {
    /// All of the frame, the picture put in as `framing` says, behind the webcam.
    pub fn whole(framing: Framing) -> Self {
        Self { rect: [0.0, 0.0, 1.0, 1.0], framing, in_front: false, hidden: false, crop: None, turns: 0, flip_h: false, flip_v: false }
    }

    /// Whether its picture's turned, mirrored or flipped: then a compositor
    /// draws it, never the capture as it comes.
    pub fn transformed(&self) -> bool {
        self.turns % 4 != 0 || self.flip_h || self.flip_v
    }

    /// A `w`×`h` picture's size as it's turned.
    pub fn turned(&self, (w, h): (u32, u32)) -> (u32, u32) {
        if self.turns % 2 == 1 { (h, w) } else { (w, h) }
    }

    /// A point of where the picture's drawn (from 0 to 1 across and down
    /// it) in the part of the picture drawn there (from 0 to 1 across and
    /// down that, in its own pixels): flipped back, then turned back, as
    /// it's turned and then flipped. Every compositor samples it so.
    pub fn unturn(&self, (mut u, mut v): (f32, f32)) -> (f32, f32) {
        if self.flip_h {
            u = 1.0 - u;
        }
        if self.flip_v {
            v = 1.0 - v;
        }
        match self.turns % 4 {
            1 => (v, 1.0 - u),
            2 => (1.0 - u, 1.0 - v),
            3 => (1.0 - v, u),
            _ => (u, v),
        }
    }

    /// Whether its box is all of the frame, no more.
    pub fn is_whole(&self) -> bool {
        let [x, y, w, h] = self.rect;
        let on = |v: f32, edge: f32| (v - edge).abs() <= 1e-4;
        on(x, 0.0) && on(y, 0.0) && on(x + w, 1.0) && on(y + h, 1.0)
    }

    /// Whether its box covers all of the frame: all of it, or past its edges.
    pub fn covers(&self) -> bool {
        let [x, y, w, h] = self.rect;
        x <= 1e-4 && y <= 1e-4 && x + w >= 1.0 - 1e-4 && y + h >= 1.0 - 1e-4
    }

    /// Whether its box is inside the frame.
    fn inside(&self) -> bool {
        let [x, y, w, h] = self.rect;
        x >= -1e-4 && y >= -1e-4 && x + w <= 1.0 + 1e-4 && y + h <= 1.0 + 1e-4
    }

    /// Its box in a `fw`×`fh` frame's pixels, inside the frame: (left, top,
    /// width, height), its edges on even pixels for the encoder's 4:2:0
    /// colour (the frame's own edges as they are).
    pub fn rect_in(&self, fw: u32, fh: u32) -> [u32; 4] {
        let edge = |v: f32, n: u32| if v >= 1.0 - 1e-4 { n } else { ((v.max(0.0) * n as f32).round() as u32 & !1).min(n) };
        let [x, y, w, h] = self.rect;
        let (x0, y0) = (edge(x, fw), edge(y, fh));
        let (x1, y1) = (edge(x + w, fw).max(x0), edge(y + h, fh).max(y0));
        [x0, y0, x1 - x0, y1 - y0]
    }

    /// Where a picture of `content`'s size (its crop, if any) goes in a
    /// `frame`-size frame: see [`place_picture`](Self::place_picture).
    /// Nothing of it when hidden.
    // macOS has ScreenCaptureKit cut the crop out (`mac::video::stream_config`).
    #[cfg_attr(not(any(target_os = "windows", target_os = "linux", test)), allow(dead_code))]
    pub(crate) fn place(&self, content: (u32, u32), frame: (u32, u32)) -> ([u32; 4], [u32; 4]) {
        if self.hidden {
            return ([0; 4], [0; 4]);
        }
        let Some(c) = self.crop else { return self.place_turned(content, frame) };
        let [x, y, w, h] = c.rect(content.0, content.1);
        let ([sx, sy, sw, sh], dest) = self.place_turned((w, h), frame);
        ([x + sx, y + sy, sw, sh], dest)
    }

    /// [`place_picture`](Self::place_picture) for the picture as it shows,
    /// turned and flipped: the part of it used, in its own (unturned)
    /// pixels, and where it's drawn. A compositor maps each point of where
    /// it's drawn into that part with [`unturn`](Self::unturn).
    pub(crate) fn place_turned(&self, content: (u32, u32), frame: (u32, u32)) -> ([u32; 4], [u32; 4]) {
        let shown = self.turned(content);
        let ([sx, sy, sw, sh], dest) = self.place_picture(shown, frame);
        if !self.transformed() || sw == 0 || sh == 0 {
            return ([sx, sy, sw, sh], dest);
        }
        // The part used, as fractions of the picture as it shows, back in its own.
        let (tw, th) = (shown.0.max(1) as f32, shown.1.max(1) as f32);
        let a = self.unturn((sx as f32 / tw, sy as f32 / th));
        let b = self.unturn(((sx + sw) as f32 / tw, (sy + sh) as f32 / th));
        let px = |f: f32, n: u32| (f * n as f32).round().clamp(0.0, n as f32) as u32;
        let (x0, x1) = (px(a.0.min(b.0), content.0), px(a.0.max(b.0), content.0));
        let (y0, y1) = (px(a.1.min(b.1), content.1), px(a.1.max(b.1), content.1));
        ([x0, y0, (x1 - x0).max(1), (y1 - y0).max(1)], dest)
    }

    /// Where a picture of `content`'s size goes in a `frame`-size frame,
    /// into the box as `framing` says (see [`placement_in`]): the part of it
    /// used, and where it's drawn. A box reaching past the frame is cut at
    /// its edges, and the picture with it (see [`placement_past`]).
    pub(crate) fn place_picture(&self, content: (u32, u32), (fw, fh): (u32, u32)) -> ([u32; 4], [u32; 4]) {
        if self.inside() {
            return placement_in(content, self.rect_in(fw, fh), self.framing);
        }
        placement_past(content, self.rect, (fw, fh), self.framing)
    }
}

/// Where a picture of `content`'s size goes in a box reaching past the
/// edges of a `fw`×`fh` frame (`rect`, as fractions of it): put in as
/// `framing` says, as if the frame went on (see [`placement_in`]), then cut
/// at the frame's edges, the part of the picture used with them. Where it's
/// drawn is even, for the encoder's 4:2:0 colour (the frame's own edges as
/// they are); nothing, when none of it is in the frame.
fn placement_past((cw, ch): (u32, u32), [x, y, w, h]: [f32; 4], (fw, fh): (u32, u32), framing: Framing) -> ([u32; 4], [u32; 4]) {
    let (cw, ch, fw, fh) = (cw as f64, ch as f64, fw as f64, fh as f64);
    let bx = [x as f64 * fw, y as f64 * fh, w as f64 * fw, h as f64 * fh];
    if cw <= 0.0 || ch <= 0.0 || bx[2] <= 0.0 || bx[3] <= 0.0 {
        return ([0; 4], [0; 4]);
    }
    // The part of the picture used, and where it goes, in all of the box.
    let (src, dest) = match framing {
        Framing::Stretch => ([0.0, 0.0, cw, ch], bx),
        Framing::Fit => {
            let k = (bx[2] / cw).min(bx[3] / ch);
            let (w, h) = (cw * k, ch * k);
            ([0.0, 0.0, cw, ch], [bx[0] + (bx[2] - w) / 2.0, bx[1] + (bx[3] - h) / 2.0, w, h])
        }
        Framing::Fill => {
            let k = (bx[2] / cw).max(bx[3] / ch);
            let (w, h) = (bx[2] / k, bx[3] / k);
            ([(cw - w) / 2.0, (ch - h) / 2.0, w, h], bx)
        }
    };
    // Cut at the frame's edges, on even pixels inside it.
    let even = |v: f64, n: f64| if v >= n - 0.5 { n } else { ((v.max(0.0).round() as u32) & !1) as f64 };
    let (x0, y0, x1, y1) = (even(dest[0], fw), even(dest[1], fh), even(dest[0] + dest[2], fw), even(dest[1] + dest[3], fh));
    if x1 <= x0 || y1 <= y0 {
        return ([0; 4], [0; 4]);
    }
    // The picture's part under that, as near as its pixels go (a pixel of
    // it at least).
    let part = |from: f64, size: f64, d0: f64, d1: f64, dsize: f64, most: f64| {
        let at = |d: f64| (from + d * size / dsize).round().clamp(0.0, most) as u32;
        let n = at(d1).saturating_sub(at(d0)).max(1);
        (at(d0).min((most as u32).saturating_sub(n)), n)
    };
    let (sx, sw) = part(src[0], src[2], x0 - dest[0], x1 - dest[0], dest[2], cw);
    let (sy, sh) = part(src[1], src[3], y0 - dest[1], y1 - dest[1], dest[3], ch);
    ([sx, sy, sw, sh], [x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32])
}

/// Where a picture of `content`'s size goes in a frame of `frame`'s: see
/// [`placement_in`], the box being all of the frame.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
pub(crate) fn placement(content: (u32, u32), (fw, fh): (u32, u32), framing: Framing) -> ([u32; 4], [u32; 4]) {
    placement_in(content, [0, 0, fw, fh], framing)
}

/// Where a picture of `content`'s size goes in the box `bx` (each as left,
/// top, width, height, the box in the frame's pixels): the part of the
/// picture used, and where it's drawn. Fit uses all of it, with nothing
/// around it in the box (what a recording without a shape does with a
/// window that isn't the screen's shape); Fill cuts off what's past the
/// box's shape; Stretch draws all of it over all of the box. Where it's
/// drawn is even, for the encoder's 4:2:0 colour.
#[cfg_attr(not(any(target_os = "windows", target_os = "linux", target_os = "macos")), allow(dead_code))]
pub(crate) fn placement_in((cw, ch): (u32, u32), [bx, by, bw, bh]: [u32; 4], framing: Framing) -> ([u32; 4], [u32; 4]) {
    let (all, whole) = ([0, 0, cw, ch], [bx, by, bw, bh]);
    if cw == 0 || ch == 0 || bw == 0 || bh == 0 {
        return (all, whole);
    }
    // Wider than the box, as shapes go.
    let wider = cw as u64 * bh as u64 > ch as u64 * bw as u64;
    match framing {
        Framing::Stretch => (all, whole),
        Framing::Fit => {
            let (w, h) = if wider { (bw, (ch as u64 * bw as u64 / cw as u64) as u32) } else { ((cw as u64 * bh as u64 / ch as u64) as u32, bh) };
            let (w, h) = (w & !1, h & !1);
            (all, [bx + (((bw - w) / 2) & !1), by + (((bh - h) / 2) & !1), w, h])
        }
        Framing::Fill => {
            let (w, h) = if wider { ((ch as u64 * bw as u64 / bh as u64) as u32, ch) } else { (cw, (cw as u64 * bh as u64 / bw as u64) as u32) };
            ([(cw - w) / 2, (ch - h) / 2, w, h], whole)
        }
    }
}

/// The size a recording of `source` will be, without starting anything
/// (cheap: asks the system for display sizes): `size` of the display's
/// (a crop doesn't change it). Games and apps record at the main display's
/// size. `None` if the display isn't known (on Linux, until the screen has
/// been picked in the system's dialog once).
pub fn frame_size(source: &VideoSource, size: VideoSize) -> Option<(u32, u32)> {
    let id = match source {
        VideoSource::Screen { id, .. } => Some(id.as_str()),
        VideoSource::Apps { .. } => None,
    };
    #[cfg(target_os = "macos")]
    let native = mac::display_pixels(id);
    #[cfg(target_os = "windows")]
    let native = win::display_pixels(id);
    #[cfg(target_os = "linux")]
    let native = linux::display_pixels(id);
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    let native: Option<(u32, u32)> = {
        let _ = id;
        None
    };
    native.map(|(w, h)| size.of(w, h))
}

/// The fastest a recording of `source` can look at the screen for frame
/// blending, if there's a limit: on macOS and Linux the display's refresh
/// rate (games and apps: the main display's on macOS, the fastest's on
/// Linux), as ScreenCaptureKit and the compositor only see what the display
/// shows. Windows takes the game's own pictures: no limit.
pub fn max_blend_rate(source: &VideoSource) -> Option<u32> {
    #[cfg(target_os = "macos")]
    {
        let id = match source {
            VideoSource::Screen { id, .. } => Some(id.as_str()),
            VideoSource::Apps { .. } => None,
        };
        mac::display_refresh(id)
    }
    #[cfg(target_os = "linux")]
    {
        display_refresh(source)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = source;
        None
    }
}

/// Whether frame blending can be done here: Windows and macOS, and Linux
/// where the graphics card takes the screen's pictures (Vulkan that imports
/// DMA-BUFs). Quick after the first time.
pub fn frame_blending() -> bool {
    #[cfg(target_os = "linux")]
    {
        linux::frame_blending()
    }
    #[cfg(not(target_os = "linux"))]
    {
        cfg!(any(target_os = "windows", target_os = "macos"))
    }
}

/// The refresh rate of the display a recording shows (the fastest display's
/// when recording games and apps), if known: what the frame rate presets go
/// up to.
pub fn display_refresh(source: &VideoSource) -> Option<u32> {
    let id = match source {
        VideoSource::Screen { id, .. } => Some(id.as_str()),
        VideoSource::Apps { .. } => None,
    };
    #[cfg(target_os = "macos")]
    {
        mac::display_refresh(id)
    }
    #[cfg(target_os = "windows")]
    {
        win::system::display_refresh(id)
    }
    #[cfg(target_os = "linux")]
    {
        linux::display_refresh(id)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = id;
        None
    }
}

/// The part of a display kept, in its pixels: `w`×`h`, its top-left corner
/// at `at`, or in the middle. Kept inside the display, and even, for the
/// encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Crop {
    pub w: u32,
    pub h: u32,
    pub at: Option<(u32, u32)>,
}

impl Crop {
    /// Where it is on a `cw`×`ch` display: (left, top, width, height).
    pub fn rect(&self, cw: u32, ch: u32) -> [u32; 4] {
        let w = self.w.clamp(2, cw.max(2)) & !1;
        let h = self.h.clamp(2, ch.max(2)) & !1;
        let (x, y) = self.at.unwrap_or((cw.saturating_sub(w) / 2, ch.saturating_sub(h) / 2));
        [x.min(cw.saturating_sub(w)), y.min(ch.saturating_sub(h)), w, h]
    }
}

/// Whether a screen can be cropped ([`ScreenPlacement`]'s `crop`) here.
pub const SCREEN_CROP: bool = cfg!(any(target_os = "linux", target_os = "windows", target_os = "macos"));

/// A `w`×`h` picture's size once `crop` is cut out of it.
pub fn cropped_size(crop: Option<Crop>, (w, h): (u32, u32)) -> (u32, u32) {
    match crop {
        Some(c) => {
            let [_, _, w, h] = c.rect(w, h);
            (w, h)
        }
        None => (w, h),
    }
}

/// What a recording's video shows. This is the bottom layer of the picture;
/// overlays (a webcam) will go on top of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoSource {
    /// A whole display, by backend id (see [`list_screens`]), captured the
    /// way `method` says (Windows). A game that needs the game capture hook,
    /// fullscreen and in focus on it, is recorded through the hook while it is.
    /// The part of it shown is its placement's (see [`ScreenPlacement`]).
    Screen { id: String, hook: GameHook, method: ScreenCapture },
    /// Games and apps, by executable name (see [`list_windowed_apps`]): records
    /// whichever of them was last in focus, for as long as it's showing, even
    /// while you click into something else. When it stops showing (minimized,
    /// as games are when you alt-tab), it shows the away screen
    /// (`away_when_unfocused`) or keeps its last picture. Fitted into a frame
    /// the size of the main display. The away screen also shows while none of
    /// them is open; each is picked up as soon as it opens.
    Apps { ids: Vec<String>, away_when_unfocused: bool, hook: GameHook },
}

/// When a game is recorded through the game capture hook (Windows) instead
/// of Windows' own capture (whether recording games and apps or the screen), which can't see some games (exclusive fullscreen
/// OpenGL records one frozen frame) and loses the cursor in others. The hook
/// is OBS Studio's, loaded into the game; it's never used on a game with
/// anti-cheat unless it's in `allowed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameHook {
    /// Where it's needed: games known to need it, and games that turn out to
    /// be exclusive fullscreen. Never on a game with anti-cheat.
    pub auto: bool,
    /// Games with anti-cheat (by executable name) it may be used on anyway,
    /// where they need it: the user accepted the risk.
    pub allowed: Vec<String>,
}

impl Default for GameHook {
    fn default() -> Self {
        Self { auto: true, allowed: Vec::new() }
    }
}

/// How a whole display is captured on Windows. Windows Graphics Capture draws
/// a yellow border around the display on Windows 10 (Windows 11 lets us turn
/// it off); Desktop Duplication draws none, but can only capture a display on
/// the graphics card HasteClip uses. Games and apps are always recorded with
/// Windows Graphics Capture (or the game capture hook): Desktop Duplication
/// only captures whole displays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScreenCapture {
    /// Desktop Duplication on Windows 10, where it can capture the display;
    /// Windows Graphics Capture otherwise.
    #[default]
    Auto,
    Duplication,
    Wgc,
}

/// Which way a display is being captured, and whether that's what was asked
/// for, to say so beside the setting: "Desktop Duplication", or "Windows
/// Graphics Capture" when it couldn't. Windows only; `None` while nothing
/// captures a display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenCaptureInUse {
    pub duplication: bool,
    /// Desktop Duplication was chosen, or picked by Automatic, but couldn't
    /// start or stopped: Windows Graphics Capture is used until it can (it's
    /// tried again now and then).
    pub fell_back: bool,
}

/// How the display being captured now is captured (see [`ScreenCaptureInUse`]).
pub fn screen_capture_in_use() -> Option<ScreenCaptureInUse> {
    #[cfg(target_os = "windows")]
    {
        win::screen_capture_in_use()
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

/// What Automatic picks on this computer, for the setting's note: true when
/// it's Desktop Duplication (Windows 10). Whether a display is on another
/// graphics card is only known when it starts.
pub fn screen_capture_auto_duplicates() -> bool {
    #[cfg(target_os = "windows")]
    {
        win::screen_capture_auto_duplicates()
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

/// Until when playback's decoder was last behind, in ms on [`clock_ms`]:
/// background decoding (the filmstrip, exports) keeps off the decoder
/// playback uses meanwhile. A card has one (NVIDIA's RTX 4060 Ti decodes
/// ~600 1440p frames a second), and a 1440p 540 fps clip needs nearly all
/// of it: the filmstrip decoding there too held playback to 400 frames a
/// second.
static PLAYBACK_BEHIND_UNTIL_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Milliseconds since the first time this was asked.
fn clock_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

/// Playback's decoder can't keep ahead of the clock: background decoding
/// keeps off its decoder for the next moment (said again while it lasts).
pub fn playback_behind() {
    PLAYBACK_BEHIND_UNTIL_MS.fetch_max(clock_ms() + 500, std::sync::atomic::Ordering::Relaxed);
}

/// Whether playback's decoder is behind, or was a moment ago.
pub fn playback_short() -> bool {
    clock_ms() < PLAYBACK_BEHIND_UNTIL_MS.load(std::sync::atomic::Ordering::Relaxed)
}

/// How much memory this computer has, in bytes.
pub fn total_memory() -> Option<u64> {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut status = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
        unsafe { GlobalMemoryStatusEx(&mut status) }.ok()?;
        Some(status.ullTotalPhys)
    }
    #[cfg(target_os = "macos")]
    {
        let mut bytes = 0u64;
        let mut len = std::mem::size_of::<u64>();
        let ok = unsafe { libc::sysctlbyname(c"hw.memsize".as_ptr(), (&mut bytes as *mut u64).cast(), &mut len, std::ptr::null_mut(), 0) } == 0;
        ok.then_some(bytes)
    }
    #[cfg(target_os = "linux")]
    {
        let info = std::fs::read_to_string("/proc/meminfo").ok()?;
        let kb: u64 = info.lines().find_map(|l| l.strip_prefix("MemTotal:"))?.trim().trim_end_matches("kB").trim().parse().ok()?;
        Some(kb * 1024)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// Whether the game capture hook can be used here (Windows, with its files
/// next to the app).
pub fn game_hook_available() -> bool {
    #[cfg(target_os = "windows")]
    {
        win::hook::available()
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

/// The running apps with a window that use anti-cheat (as far as can be
/// told), and which ("BattlEye").
pub fn running_anticheat_apps() -> Vec<(Device, String)> {
    #[cfg(target_os = "windows")]
    {
        list_windowed_apps()
            .into_iter()
            .filter_map(|app| {
                let ac = win::anticheat::anticheat(&win::system::running_app_path(&app.id)?)?;
                Some((app, ac))
            })
            .collect()
    }
    #[cfg(not(target_os = "windows"))]
    {
        Vec::new()
    }
}

/// A still picture shown instead of an app (see [`EncodeSettings::away_screen`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StillImage {
    pub width: u32,
    pub height: u32,
    /// Rows top to bottom, 4 bytes per pixel in B, G, R, A order, opaque.
    pub bgra: Vec<u8>,
}

/// Before the app exits, once capture has stopped: end what still runs in
/// the background (on Linux a screen cast kept a moment for reuse), so none
/// of it is using the graphics card while the process exits.
pub fn shutdown() {
    #[cfg(target_os = "linux")]
    linux::shutdown();
}

/// Whether games and apps can be recorded here ([`VideoSource::Apps`]):
/// on Windows and macOS, and on Linux on KDE Plasma's Wayland session so far.
/// Asked once; quick after that.
pub fn app_capture() -> bool {
    #[cfg(target_os = "linux")]
    {
        static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *AVAILABLE.get_or_init(linux::app_capture)
    }
    #[cfg(not(target_os = "linux"))]
    {
        cfg!(any(target_os = "windows", target_os = "macos"))
    }
}

/// Running apps with a window, for [`VideoSource::Apps`], sorted by name.
pub fn list_windowed_apps() -> Vec<Device> {
    #[cfg(target_os = "windows")]
    {
        win::list_windowed_apps()
    }
    #[cfg(target_os = "macos")]
    {
        mac::windows::list_windowed_apps()
    }
    #[cfg(target_os = "linux")]
    {
        linux::list_windowed_apps()
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        Vec::new()
    }
}

/// The icon of an app source's app ([`sources::SourceKind::App`]'s id: a
/// bundle id on macOS, an executable on Windows, a program's file name on
/// Linux), as `size`×`size` RGBA with straight alpha. `None` where there
/// isn't one to show: a program without an app bundle, on Windows an app
/// that isn't running, on Linux one the icon theme doesn't have. Slow-ish
/// (it reads files): call it off the UI thread.
pub fn app_icon(id: &str, size: u32) -> Option<Vec<u8>> {
    #[cfg(target_os = "windows")]
    {
        win::system::app_icon(id, size)
    }
    #[cfg(target_os = "macos")]
    {
        mac::windows::app_icon(id, size)
    }
    #[cfg(target_os = "linux")]
    {
        linux::app_icon(id, size)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        let _ = (id, size);
        None
    }
}

/// The app in focus, where the platform can tell: its executable on Windows
/// (`osu!.exe`), its bundle id on macOS (else its executable).
pub fn foreground_exe() -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        win::foreground_exe()
    }
    #[cfg(target_os = "macos")]
    {
        mac::windows::foreground_exe()
    }
    #[cfg(target_os = "linux")]
    {
        linux::focus::foreground_app_path().map(|(id, _)| id)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// Whether Windows shows the cursor, and its image's handle (for a debug log).
#[cfg(target_os = "windows")]
pub fn win_cursor_state() -> Option<(bool, usize)> {
    win::cursor_state()
}

/// The app in focus: its id (as [`foreground_exe`]) and where it's installed,
/// to tell a game by its folder (`…/steamapps/common/<game>/…`).
pub fn foreground_app_path() -> Option<(String, PathBuf)> {
    #[cfg(target_os = "windows")]
    {
        win::foreground_app_path()
    }
    #[cfg(target_os = "macos")]
    {
        mac::windows::foreground_app_path()
    }
    #[cfg(target_os = "linux")]
    {
        linux::focus::foreground_app_path()
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// The file name of a running app's bundle (`World of Warcraft.app`), by
/// its id (bundle id, or executable for an app without one).
#[cfg(target_os = "macos")]
pub fn app_file_name(id: &str) -> Option<String> {
    mac::windows::bundle_file_name(id)
}

/// Whether [`list_screens`] lists the actual screens, to pick from in the
/// app. Not on Linux under Wayland, where the desktop's own dialog picks one
/// (see [`choose_screen_again`]).
pub fn screens_listed() -> bool {
    #[cfg(target_os = "linux")]
    {
        linux::screens_listed()
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

/// Have the desktop's dialog ask again which screen to record (Linux under
/// Wayland; elsewhere screens are picked in the app, and this does nothing).
pub fn choose_screen_again() {
    #[cfg(target_os = "linux")]
    linux::choose_screen_again();
}

/// Screens the backend can capture; their ids go in [`VideoSource::Screen`].
pub fn list_screens() -> Vec<Device> {
    #[cfg(target_os = "macos")]
    {
        macos::list_shareable().screens
    }
    #[cfg(target_os = "windows")]
    {
        win::list_screens()
    }
    #[cfg(target_os = "linux")]
    {
        linux::list_screens()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        Vec::new()
    }
}

/// Running apps the user could add as an audio source, sorted by name. Their
/// ids go in `SourceKind::App` (a bundle id on macOS, an executable name on
/// Windows, the program's name on Linux). On Linux only apps that have
/// opened their sound are listed: PipeWire knows nothing of the rest.
pub fn list_apps() -> Vec<Device> {
    #[cfg(target_os = "macos")]
    {
        macos::list_apps()
    }
    #[cfg(target_os = "windows")]
    {
        win::list_apps()
    }
    #[cfg(target_os = "linux")]
    {
        linux::audio::list_apps()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        Vec::new()
    }
}

/// Screen-capture permission state, so the UI can guide the user instead of
/// silently failing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    Granted,
    Denied,
    /// Platform doesn't gate screen capture this way (or not yet implemented).
    NotApplicable,
}

/// Current screen-recording permission for this process.
pub fn screen_permission() -> Permission {
    #[cfg(target_os = "macos")]
    {
        if macos::screen_recording_authorized() {
            Permission::Granted
        } else {
            Permission::Denied
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        Permission::NotApplicable
    }
}

/// Ask the OS to prompt for screen-recording permission (macOS: first run only).
pub fn request_screen_permission() {
    #[cfg(target_os = "macos")]
    {
        let _ = macos::request_screen_recording();
    }
}

/// How the recorder is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Continuously buffer the last N seconds; `save_clip` flushes it to a file.
    ReplayBuffer,
    /// Record straight to disk until stopped.
    Record,
}

/// A capture device (screen, app, or audio device) as seen by the backend.
#[derive(Debug, Clone)]
pub struct Device {
    /// Backend-specific id (display id, bundle id or executable, device name).
    pub id: String,
    pub name: String,
}

/// Displays and apps the backend can capture.
#[derive(Debug, Clone, Default)]
pub struct Devices {
    pub screens: Vec<Device>,
    /// Running applications (for per-app audio capture on macOS/Windows).
    pub apps: Vec<Device>,
}

/// One second of the running capture: frames handed to the video encoder
/// and frames skipped, and (frame blending) the screen's new pictures and
/// those that missed their frame.
#[derive(Clone, Copy)]
struct Second {
    encoded: u32,
    skipped: u32,
    pictures: u32,
    missed: u32,
}

impl Second {
    const NONE: Self = Self { encoded: 0, skipped: 0, pictures: 0, missed: 0 };
}

/// The running capture's second under way and the whole one before it, as
/// a frame rate counter would count them: from the capture's start, or its
/// pacer's once that says so ([`blending`]).
struct History {
    started: Option<std::time::Instant>,
    /// Frames a second, and looks at the screen each blends (one: it doesn't
    /// blend; none before the pacer begins).
    fps: u32,
    looks: u32,
    /// The second under way (counted from the start) and its counts.
    at: u64,
    now: Second,
    /// The whole second before it (none until one has gone by).
    last: Option<Second>,
}

impl History {
    const NONE: Self = Self { started: None, fps: 0, looks: 0, at: 0, now: Second::NONE, last: None };

    /// The second under way: once another begins, the one before is whole
    /// (a quiet one when seconds went by with nothing counted). None between
    /// captures.
    fn now(&mut self) -> Option<&mut Second> {
        let at = self.started?.elapsed().as_secs();
        if at > self.at {
            self.last = Some(if at == self.at + 1 { self.now } else { Second::NONE });
            (self.at, self.now) = (at, Second::NONE);
        }
        Some(&mut self.now)
    }
}

static HISTORY: std::sync::Mutex<History> = std::sync::Mutex::new(History::NONE);

/// Count toward the second under way.
fn count(add: impl FnOnce(&mut Second)) {
    if let Some(s) = HISTORY.lock().unwrap().now() {
        add(s);
    }
}

/// A capture starts: how it keeps up is counted from nothing (not blending
/// until its pacer says so).
pub(crate) fn capture_started() {
    *HISTORY.lock().unwrap() = History { started: Some(std::time::Instant::now()), ..History::NONE };
}

/// The capture's pacer begins: `fps` frames a second, each blending `looks`
/// looks at the screen (one doesn't blend: Windows times its frames so
/// without blending). Its seconds are counted from here, so the first whole
/// one is all the pacer's.
pub(crate) fn blending(fps: u32, looks: u32) {
    *HISTORY.lock().unwrap() = History { started: Some(std::time::Instant::now()), fps, looks, ..History::NONE };
}

/// A frame handed to the video encoder.
pub(crate) fn count_encoded() {
    count(|s| s.encoded += 1);
}

/// `n` frames skipped: the encoder couldn't keep up (or the pacer fell far
/// behind), and the file shows the one before them longer.
pub(crate) fn count_skipped(n: u64) {
    count(|s| s.skipped = s.skipped.saturating_add(n.min(u32::MAX as u64) as u32));
}

/// Frame blending got one of the screen's pictures.
pub(crate) fn count_picture() {
    count(|s| s.pictures += 1);
}

/// `n` of the screen's pictures missed their frame: one came too late for it,
/// or was let go of waiting while frame blending fell behind.
pub(crate) fn count_missed(n: u64) {
    count(|s| s.missed = s.missed.saturating_add(n.min(u32::MAX as u64) as u32));
}

/// How the running capture keeps up ([`health`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct CaptureHealth {
    /// Its last whole second, as a frame rate counter would have it (none
    /// until one has gone by).
    pub last_second: Option<LastSecond>,
    /// Frame blending, if the capture blends.
    pub blend: Option<BlendHealth>,
}

/// One whole second of the running capture ([`CaptureHealth::last_second`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LastSecond {
    /// Frames handed to the video encoder, and frames skipped because it (or
    /// the graphics card making them) couldn't keep up: the file shows the one
    /// before a skipped frame longer.
    pub encoded: u32,
    pub skipped: u32,
}

/// How frame blending keeps up ([`CaptureHealth::blend`]).
#[derive(Debug, Clone, Copy)]
pub struct BlendHealth {
    /// Looks at the screen a second, and per frame.
    pub rate: u32,
    pub looks: u32,
    /// Its last whole second (none until one has gone by).
    pub last_second: Option<BlendSecond>,
}

/// One whole second of frame blending ([`BlendHealth::last_second`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BlendSecond {
    /// Looks blended into the frames that went to the video: every look,
    /// whether or not the screen had changed for it, but a skipped frame's.
    pub blended: u32,
    /// The screen's new pictures: it only shows one when something on it
    /// changes (as often as the game draws, or the display refreshes), and
    /// each look takes the newest one up.
    pub pictures: u32,
    /// Pictures that missed their frame (came too late for it, or were let go
    /// of waiting): those frames reused the picture before.
    pub missed: u32,
}

/// How the running capture keeps up: its last whole second, counted each
/// second from its start, as a frame rate counter would. None before a
/// capture starts.
pub fn health() -> Option<CaptureHealth> {
    HISTORY.lock().unwrap().summary()
}

impl History {
    /// [`health`] of this history.
    fn summary(&mut self) -> Option<CaptureHealth> {
        self.now()?;
        let last_second = self.last.map(|s| LastSecond { encoded: s.encoded, skipped: s.skipped });
        let (fps, looks) = (self.fps, self.looks);
        let blend = (looks > 1).then(|| BlendHealth {
            rate: fps.saturating_mul(looks),
            looks,
            last_second: self.last.map(|s| BlendSecond {
                // Every frame made blends all its looks; a skipped one's go
                // with it. Told by the frames skipped, not those made: the
                // pacer makes exactly `fps` a second on its own clock, but a
                // second here can catch one more or one fewer of them as
                // they're finished. None made or skipped: it was held up.
                blended: if s.encoded + s.skipped == 0 { 0 } else { fps.saturating_sub(s.skipped).saturating_mul(looks) },
                pictures: s.pictures,
                missed: s.missed,
            }),
        });
        Some(CaptureHealth { last_second, blend })
    }
}

#[cfg(test)]
mod health_tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// A capture whose pacer began `ms` milliseconds ago, blending 9 looks
    /// into each of 60 frames a second.
    fn began(ms: u64) -> History {
        History { started: Some(Instant::now() - Duration::from_millis(ms)), fps: 60, looks: 9, ..History::NONE }
    }

    /// Counts go to the second they happened in: the last whole second's are
    /// as they were, not the one under way's.
    #[test]
    fn the_last_whole_second() {
        let mut h = began(0);
        let s = h.summary().unwrap();
        assert_eq!((s.last_second, s.blend.map(|b| (b.rate, b.looks, b.last_second))), (None, Some((540, 9, None))), "no whole second yet");
        // Second 0: 57 frames made and 3 skipped, 500 new pictures, 2 of them too late.
        *h.now().unwrap() = Second { encoded: 57, skipped: 3, pictures: 500, missed: 2 };
        // Half a second into second 1: 40 pictures so far.
        h.started = Some(Instant::now() - Duration::from_millis(1500));
        h.now().unwrap().pictures = 40;
        let s = h.summary().unwrap();
        assert_eq!(s.last_second, Some(LastSecond { encoded: 57, skipped: 3 }));
        assert_eq!(s.blend.unwrap().last_second, Some(BlendSecond { blended: 57 * 9, pictures: 500, missed: 2 }), "a skipped frame's looks aren't blended");
    }

    /// Every look of a frame made is blended, whether or not the screen
    /// showed a new picture for it: a still screen is blended at the full
    /// rate. A second that skips more frames than it has (catching up)
    /// blended none.
    #[test]
    fn every_look_counts() {
        let mut h = began(1500);
        (h.at, h.last) = (1, Some(Second { encoded: 60, ..Second::NONE }));
        assert_eq!(h.summary().unwrap().blend.unwrap().last_second, Some(BlendSecond { blended: 540, pictures: 0, missed: 0 }));
        // Catching up after falling behind: more than a second's frames skipped at once.
        h.last = Some(Second { skipped: 75, ..Second::NONE });
        assert_eq!(h.summary().unwrap().blend.unwrap().last_second.map(|b| b.blended), Some(0));
    }

    /// Seconds with nothing counted still go by: the last whole second is a
    /// quiet one, not the last one anything was counted in.
    #[test]
    fn quiet_seconds_go_by() {
        let mut h = began(0);
        *h.now().unwrap() = Second { encoded: 60, skipped: 0, pictures: 500, missed: 5 };
        // Ten seconds later, nothing more counted: the pacer was held up.
        h.started = Some(Instant::now() - Duration::from_secs(10));
        let s = h.summary().unwrap();
        assert_eq!(s.last_second, Some(LastSecond::default()));
        assert_eq!(s.blend.unwrap().last_second, Some(BlendSecond::default()), "nothing made, nothing blended");
    }

    /// One look a frame isn't blending (Windows times its frames so without
    /// it), and a capture whose pacer hasn't begun isn't blending yet.
    #[test]
    fn not_blending() {
        let mut h = History { looks: 1, ..began(0) };
        assert!(h.summary().is_some_and(|s| s.blend.is_none()));
        let mut h = History { started: Some(Instant::now()), ..History::NONE };
        assert!(h.summary().is_some_and(|s| s.blend.is_none()));
        let mut none = History::NONE;
        assert!(none.summary().is_none(), "no capture");
    }
}

/// AAC bitrate per audio track, bits per second.
pub const AUDIO_BITRATE: u32 = 192_000;

/// Frames from one keyframe to the next when recording at `fps`: one a
/// second, but never more than 120 frames apart. A clip saved from the replay
/// buffer starts on a keyframe (so at most a second early), and scrubbing and
/// saving an edit decode from the keyframe before a frame: the gap in frames is
/// what they cost (at 240 fps, a keyframe every 2 s made scrubbing back
/// stall). Measured with the NVIDIA hardware encoder at 15 Mbps, 5 s of osu!
/// and of CS2: at 240 fps, 120 frames instead of 480 made files 0.4-4%
/// bigger and VMAF at most 0.5 lower; at 60 fps, a second instead of two,
/// about 1% bigger and at most 0.13 lower.
pub fn keyframe_every(fps: u32) -> u32 {
    fps.clamp(1, 120)
}

/// Everything the backend needs to start a capture. The app builds this from its
/// own richer `RecordSettings`.
#[derive(Debug, Clone)]
pub struct EncodeSettings {
    pub output_dir: PathBuf,
    /// Container extension for finished files, e.g. "mp4" or "mkv".
    pub container_ext: String,
    pub fps: u32,
    /// Windows and macOS: look at the picture this many times per frame and record the
    /// average (smooth, like motion blur). 1 records the newest picture as is.
    pub blend_samples: u32,
    /// How the looks in a blended frame are weighed.
    pub blend_shape: BlendShape,
    pub video_bitrate_kbps: u32,
    /// The size to record at.
    pub size: VideoSize,
    /// Use a hardware encoder (VideoToolbox, or the GPU's Media Foundation
    /// encoder on Windows) vs. software.
    pub use_hardware: bool,
    pub replay_seconds: u32,
    /// What the video shows.
    pub video: VideoSource,
    /// Shown while recording games and apps and there's nothing to show (none
    /// is open, or you've tabbed out); black if `None`. Scaled to the frame.
    pub away_screen: Option<std::sync::Arc<StillImage>>,
    /// A webcam drawn over the picture (or under it, as `screen` says).
    pub webcam: Option<webcam::Webcam>,
    /// Where the screen's picture goes in the frame, read every frame.
    pub screen: SharedScreen,
    /// Audio sources, in track order. Track 1 is the mix of every source with
    /// `in_mix` (so the file sounds right in any player); each source with
    /// `own_track` follows on its own track, for rebalancing later.
    pub sources: Vec<sources::AudioSource>,
}

/// A replay clip being saved: what it shows is settled, the file is still
/// being written. [`PendingClip::finish`] waits for it (seconds, for a long
/// buffer), so call it off the UI and capture threads.
pub struct PendingClip(Box<dyn FnOnce() -> anyhow::Result<PathBuf> + Send>);

impl PendingClip {
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows", target_os = "linux")), allow(dead_code))]
    pub(crate) fn new(finish: impl FnOnce() -> anyhow::Result<PathBuf> + Send + 'static) -> Self {
        Self(Box::new(finish))
    }

    /// Wait for the clip to be written; its path.
    pub fn finish(self) -> anyhow::Result<PathBuf> {
        (self.0)()
    }
}

/// Whether [`Recorder::screenshot`] can take one here (Windows and Linux, so far).
pub const SCREENSHOTS: bool = cfg!(any(target_os = "windows", target_os = "linux"));

/// A screenshot being taken: its picture is the one up when it was asked
/// for, copied aside on the graphics card; [`PendingShot::finish`] reads it
/// back (a few ms), so call it off the capture thread.
pub struct PendingShot(Box<dyn FnOnce() -> anyhow::Result<StillImage> + Send>);

impl PendingShot {
    #[cfg_attr(not(any(target_os = "windows", target_os = "linux")), allow(dead_code))]
    pub(crate) fn new(finish: impl FnOnce() -> anyhow::Result<StillImage> + Send + 'static) -> Self {
        Self(Box::new(finish))
    }

    /// The picture, opaque.
    pub fn finish(self) -> anyhow::Result<StillImage> {
        (self.0)()
    }
}

/// A capture backend. Implemented by `SckRecorder` (macOS), `WinRecorder`
/// (Windows) and `LinuxRecorder` (Linux).
pub trait Recorder {
    /// Begin capturing in the given mode with the given settings.
    fn start(&mut self, mode: Mode, settings: &EncodeSettings) -> anyhow::Result<()>;

    /// In `ReplayBuffer` mode, save the buffered window as a clip in `dir` (a
    /// game's folder in the library, say). Its moment is taken right away; the
    /// returned [`PendingClip`] finishes writing it, which can take a while — so
    /// another can be saved meanwhile.
    fn save_clip(&mut self, dir: &Path) -> anyhow::Result<PendingClip>;

    /// Stop capturing. In `Record` mode, returns the finished file, moved into
    /// `dir` if given (it's written in the output folder: which game it was is
    /// only known at the end).
    fn stop(&mut self, dir: Option<&Path>) -> anyhow::Result<Option<PathBuf>>;

    /// Whether a capture is currently running.
    fn is_running(&self) -> bool;

    /// Change what the running capture records, without restarting it, if the
    /// change allows (a different list of games and apps). Returns whether it
    /// was applied; otherwise it takes effect on the next start.
    fn update_video(&mut self, _video: &VideoSource) -> bool {
        false
    }

    /// Change how far back the running replay buffer reaches (and Save clip
    /// saves), without restarting it.
    fn set_replay_seconds(&mut self, _seconds: u32) {}

    /// Switch the running capture's microphone source `id` to `device`
    /// (its track stays). Returns whether it was applied.
    fn set_mic(&mut self, _id: &str, _device: &str) -> bool {
        false
    }

    /// A screenshot of `video` now, as it's captured: every pixel of the
    /// screen or the game's window, whatever shape and size recordings are
    /// made in, without the webcam. From the running capture, or one of its
    /// own when nothing runs. See [`SCREENSHOTS`].
    fn screenshot(&mut self, _video: &VideoSource) -> anyhow::Result<PendingShot> {
        anyhow::bail!("screenshots aren't available on this platform yet")
    }
}

#[cfg(test)]
mod size_tests {
    use super::{Crop, Framing, ScreenPlacement, Shape, VideoSize, output_size, placement};

    #[test]
    fn output_size_keeps_aspect_and_even() {
        assert_eq!(output_size(2560, 1440, None), (2560, 1440));
        assert_eq!(output_size(2560, 1440, Some(1080)), (1920, 1080));
        assert_eq!(output_size(1920, 1080, Some(1440)), (1920, 1080));
        assert_eq!(output_size(3440, 1440, Some(721)), (1722, 720));
        // The short side: upright, it's the width.
        assert_eq!(output_size(1440, 2560, Some(1080)), (1080, 1920));
    }

    #[test]
    fn output_size_fits_the_encoder() {
        assert_eq!(output_size(6720, 2836, None), (4096, 1728));
        assert_eq!(output_size(7680, 4320, Some(2160)), (3840, 2160));
        assert_eq!(output_size(5120, 2880, None), (4096, 2304));
        // Asked for more than fits: still no wider than it takes.
        assert_eq!(output_size(7680, 4320, Some(2592)), (4096, 2304));
        // Portrait.
        assert_eq!(output_size(2160, 7680, None), (1152, 4096));
        assert_eq!(output_size(4096, 2304, None), (4096, 2304));
    }

    /// An ultrawide screen recorded 16:9 three ways, and at 1080p.
    #[test]
    fn shapes() {
        let wide = |framing| VideoSize { shape: Some(Shape { w: 16, h: 9, framing }), ..Default::default() };
        assert_eq!(wide(Framing::Fill).of(3440, 1440), (2560, 1440));
        assert_eq!(wide(Framing::Fit).of(3440, 1440), (3440, 1934));
        assert_eq!(wide(Framing::Stretch).of(3440, 1440), (2560, 1440));
        assert_eq!(VideoSize { short_side: Some(1080), ..wide(Framing::Fill) }.of(3440, 1440), (1920, 1080));
        // Upright from a 16:9 screen: its middle, 1080p on its short side.
        let upright = VideoSize { shape: Some(Shape { w: 9, h: 16, framing: Framing::Fill }), ..Default::default() };
        assert_eq!(upright.of(1920, 1080), (608, 1080));
        assert_eq!(VideoSize { short_side: Some(480), ..upright }.of(1920, 1080), (480, 852));
        assert_eq!(VideoSize::default().of(3440, 1440), (3440, 1440));
    }

    /// An exact size is kept whatever the screen, made even and small
    /// enough for the encoder.
    #[test]
    fn exact_sizes() {
        let exact = |w, h| VideoSize { exact: Some((w, h)), short_side: Some(720), shape: None };
        assert_eq!(exact(1920, 1080).of(3440, 1440), (1920, 1080));
        assert_eq!(exact(1080, 1920).of(2560, 1440), (1080, 1920));
        assert_eq!(exact(1281, 721).of(2560, 1440), (1280, 720));
        assert_eq!(exact(8192, 4096).of(2560, 1440), (4096, 2048));
    }

    #[test]
    fn where_the_picture_goes() {
        // Cropped: the middle 1600×900 of 2560×1440, then as usual.
        let crop = Some(Crop { w: 1600, h: 900, at: None });
        assert_eq!(ScreenPlacement { crop, ..ScreenPlacement::whole(Framing::Fit) }.place((2560, 1440), (1600, 900)), ([480, 270, 1600, 900], [0, 0, 1600, 900]));
        // Placed past the edge: moved back in; odd sizes made even.
        assert_eq!(Crop { w: 1601, h: 901, at: Some((2000, 1000)) }.rect(2560, 1440), [960, 540, 1600, 900]);
        // Fill: the middle 2560 of 3440, over all of the frame.
        assert_eq!(placement((3440, 1440), (2560, 1440), Framing::Fill), ([440, 0, 2560, 1440], [0, 0, 2560, 1440]));
        // Fit: all of it, bars above and below.
        assert_eq!(placement((3440, 1440), (1920, 1080), Framing::Fit), ([0, 0, 3440, 1440], [0, 138, 1920, 802]));
        assert_eq!(placement((1600, 1200), (1920, 1080), Framing::Fit), ([0, 0, 1600, 1200], [240, 0, 1440, 1080]));
        assert_eq!(placement((2560, 1440), (1920, 1080), Framing::Fit), ([0, 0, 2560, 1440], [0, 0, 1920, 1080]));
        // Stretch: all of it everywhere.
        assert_eq!(placement((3440, 1440), (1920, 1080), Framing::Stretch), ([0, 0, 3440, 1440], [0, 0, 1920, 1080]));
        // Nothing to place yet.
        assert_eq!(placement((0, 0), (1920, 1080), Framing::Fit), ([0, 0, 0, 0], [0, 0, 1920, 1080]));
    }

    /// The screen in a box of its own: the box in pixels, even, inside the
    /// frame, and the picture filling it (cut to its shape) or fitted in it.
    #[test]
    fn the_screen_in_a_box() {
        let whole = ScreenPlacement::whole(Framing::Fill);
        assert!(whole.is_whole());
        assert_eq!(whole.rect_in(1921, 1081), [0, 0, 1921, 1081]);
        let boxed = ScreenPlacement { rect: [0.5, 0.25, 0.25, 0.5], ..whole };
        assert!(!boxed.is_whole());
        assert_eq!(boxed.rect_in(1920, 1080), [960, 270, 480, 540]);
        // Odd edges land on even pixels; past the frame, it's cut at its edge.
        assert_eq!(ScreenPlacement { rect: [0.1, 0.1, 0.3, 0.3], ..whole }.rect_in(1000, 1000), [100, 100, 300, 300]);
        assert_eq!(ScreenPlacement { rect: [0.1003, 0.0, 0.2, 0.2], ..whole }.rect_in(1000, 1000), [100, 0, 200, 200]);
        assert_eq!(ScreenPlacement { rect: [0.8, 0.9, 0.4, 0.4], ..whole }.rect_in(1000, 1000), [800, 900, 200, 100]);
        // A 16:9 screen in a square box: its middle (Fill), or all of it in a band (Fit).
        let square = ScreenPlacement { rect: [0.0, 0.0, 0.5, 1.0], ..whole };
        assert_eq!(square.place((1920, 1080), (2160, 1080)), ([420, 0, 1080, 1080], [0, 0, 1080, 1080]));
        let fit = ScreenPlacement { framing: Framing::Fit, ..square };
        assert_eq!(fit.place((1920, 1080), (2160, 1080)), ([0, 0, 1920, 1080], [0, 236, 1080, 606]));
        // In a box placed further in, the picture goes where the box is.
        let inset = ScreenPlacement { rect: [0.25, 0.5, 0.5, 0.5], framing: Framing::Fit, ..whole };
        assert_eq!(inset.place((1920, 1080), (1920, 1080)), ([0, 0, 1920, 1080], [480, 540, 960, 540]));
        // A crop is cut out first.
        let crop = Some(Crop { w: 1600, h: 900, at: None });
        assert_eq!(ScreenPlacement { crop, ..inset }.place((2560, 1440), (1920, 1080)), ([480, 270, 1600, 900], [480, 540, 960, 540]));
        // Hidden: nothing of it anywhere.
        assert_eq!(ScreenPlacement { hidden: true, ..whole }.place((1920, 1080), (1920, 1080)), ([0; 4], [0; 4]));
    }

    /// A box reaching past the frame: the picture placed in it as if the
    /// frame went on, and only what's in the frame drawn, the picture cut
    /// with it.
    #[test]
    fn the_screen_past_the_frame() {
        let whole = ScreenPlacement::whole(Framing::Stretch);
        // A 16:10 screen covering a 16:9 frame: 1920×1200 of it, 60 above
        // and below. Its middle shows, at its own scale.
        let over = ScreenPlacement { rect: [0.0, -60.0 / 1080.0, 1.0, 1200.0 / 1080.0], ..whole };
        assert!(over.covers() && !over.is_whole());
        assert!(whole.covers() && whole.is_whole());
        assert_eq!(over.place((2560, 1600), (1920, 1080)), ([0, 80, 2560, 1440], [0, 0, 1920, 1080]));
        // Moved down as far as it goes: its top.
        let top = ScreenPlacement { rect: [0.0, 0.0, 1.0, 1200.0 / 1080.0], ..whole };
        assert_eq!(top.place((2560, 1600), (1920, 1080)), ([0, 0, 2560, 1440], [0, 0, 1920, 1080]));
        // Half off the right edge: its left half, in the frame's right quarter.
        let half = ScreenPlacement { rect: [0.75, 0.25, 0.5, 0.5], ..whole };
        assert_eq!(half.place((1920, 1080), (1920, 1080)), ([0, 0, 960, 1080], [1440, 270, 480, 540]));
        // A crop is cut out first, then cut at the frame with the rest.
        let crop = Some(Crop { w: 1600, h: 900, at: None });
        assert_eq!(ScreenPlacement { crop, ..half }.place((2560, 1440), (1920, 1080)), ([480, 270, 800, 900], [1440, 270, 480, 540]));
        // Fit and Fill in such a box: as big as fits, or covers, then cut.
        assert_eq!(ScreenPlacement { framing: Framing::Fit, rect: [0.75, 0.0, 0.5, 1.0], ..whole }.place((1920, 1080), (1920, 1080)), ([0, 0, 960, 1080], [1440, 270, 480, 540]));
        assert_eq!(ScreenPlacement { framing: Framing::Fill, rect: [0.75, 0.0, 0.5, 1.0], ..whole }.place((1920, 1080), (1920, 1080)), ([480, 0, 480, 1080], [1440, 0, 480, 1080]));
        // None of it in the frame: nothing drawn.
        assert_eq!(ScreenPlacement { rect: [1.2, 0.0, 0.5, 0.5], ..whole }.place((1920, 1080), (1920, 1080)), ([0; 4], [0; 4]));
    }

    /// Turned, mirrored or flipped: placed as it shows (a turned 16:9
    /// screen is 9:16), the part used given back in its own pixels, and
    /// each point of its box sampled from the right place in it.
    #[test]
    fn the_screen_turned_and_flipped() {
        let whole = ScreenPlacement::whole(Framing::Stretch);
        // Turned a quarter clockwise into an upright 1080×1920 frame: all of
        // it, all of the frame; the frame's top-left is the screen's bottom-left.
        let turned = ScreenPlacement { turns: 1, ..whole };
        assert_eq!(turned.turned((1920, 1080)), (1080, 1920));
        assert_eq!(turned.place((1920, 1080), (1080, 1920)), ([0, 0, 1920, 1080], [0, 0, 1080, 1920]));
        assert_eq!(turned.unturn((0.0, 0.0)), (0.0, 1.0));
        assert_eq!(turned.unturn((1.0, 0.0)), (0.0, 0.0));
        assert_eq!(ScreenPlacement { turns: 3, ..whole }.unturn((0.0, 0.0)), (1.0, 0.0));
        assert_eq!(ScreenPlacement { turns: 2, ..whole }.unturn((0.25, 0.0)), (0.75, 1.0));
        // Fitted in a wide frame, upright: a band in its middle.
        let fit = ScreenPlacement { framing: Framing::Fit, ..turned };
        assert_eq!(fit.place((1920, 1080), (1920, 1080)), ([0, 0, 1920, 1080], [656, 0, 606, 1080]));
        // Mirrored, half off the frame's right edge: what shows (its left
        // half, as mirrored) is the screen's right half.
        let half = ScreenPlacement { rect: [0.75, 0.25, 0.5, 0.5], flip_h: true, ..whole };
        assert_eq!(half.place((1920, 1080), (1920, 1080)), ([960, 0, 960, 1080], [1440, 270, 480, 540]));
        assert_eq!(half.unturn((0.0, 0.5)), (1.0, 0.5));
        // Flipped upside down and cropped: the crop's, the same part of it.
        let crop = Some(Crop { w: 1600, h: 900, at: None });
        let flipped = ScreenPlacement { flip_v: true, crop, ..whole };
        assert_eq!(flipped.place((2560, 1440), (1600, 900)), ([480, 270, 1600, 900], [0, 0, 1600, 900]));
        assert!(!whole.transformed() && turned.transformed() && flipped.transformed());
    }
}

#[cfg(test)]
mod app_icon_tests {
    /// An app that's installed on every Mac has an icon; a made-up one doesn't.
    #[test]
    #[cfg(target_os = "macos")]
    fn finder_has_an_icon() {
        let px = super::app_icon("com.apple.finder", 32).expect("Finder's icon");
        assert_eq!(px.len(), 32 * 32 * 4);
        assert!(px.chunks_exact(4).any(|p| p[3] == 255), "some of it is opaque");
        assert!(px.chunks_exact(4).filter(|p| p[3] > 0).count() > 200, "it covers most of the square");
        assert!(super::app_icon("no.such.app.hasteclip", 32).is_none());
    }
}
