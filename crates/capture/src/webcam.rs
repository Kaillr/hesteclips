//! A webcam drawn over the recording: which camera, and where it sits.
//!
//! Placement is in fractions — of the recording's frame for where it goes, of
//! the camera's picture for the crop — so it survives a change of resolution or
//! camera. It's shared ([`SharedPlacement`]) between the app, which changes it
//! as you drag the webcam around the preview, and the capture, which reads it
//! every frame: moving it takes effect at once, even while recording.

use std::sync::{Arc, Mutex};

use crate::Device;

/// Whether this platform can record a webcam.
pub const AVAILABLE: bool = cfg!(any(target_os = "windows", target_os = "macos", target_os = "linux"));

/// Where the webcam goes and how much of it shows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    /// Its box in the frame: left, top, width, height as fractions of the
    /// frame's width and height. May reach past the frame's edges.
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// How much of the camera's picture is cut off at each side: left, top,
    /// right, bottom, as fractions of its width or height (the sides of the
    /// picture as turned, before any flip).
    pub crop: [f32; 4],
    /// Mirrored left to right, and upside down (after turning).
    pub flip_h: bool,
    pub flip_v: bool,
    /// Quarter turns clockwise, 0 to 3: the camera's picture is turned first,
    /// then cropped and flipped.
    pub turns: u8,
    /// How stretched the picture is: the box's shape over the shape of
    /// what's shown of it (1 not at all, 2 twice as wide as it should be).
    pub stretch: f32,
}

impl Placement {
    /// Right in the bottom-right corner, a quarter of the frame wide, for a
    /// camera whose picture is `camera_aspect` (width / height) in a frame of
    /// `frame_aspect`.
    pub fn default_for(camera_aspect: f32, frame_aspect: f32) -> Self {
        let w = 0.25;
        let h = w * frame_aspect / camera_aspect;
        Self { x: 1.0 - w, y: 1.0 - h, w, h, crop: [0.0; 4], flip_h: false, flip_v: false, turns: 0, stretch: 1.0 }
    }

    /// The webcam switched off: a box of no size, which is never drawn, so a
    /// running capture drops it at once. The camera itself is closed too
    /// ([`keep_open`]) and taken up again when it's switched back on.
    pub fn hidden() -> Self {
        Self { x: 0.0, y: 0.0, w: 0.0, h: 0.0, crop: [0.0; 4], flip_h: false, flip_v: false, turns: 0, stretch: 1.0 }
    }

    pub fn is_hidden(&self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }

    /// The size of a camera picture of `size` once turned.
    pub fn turned(&self, (w, h): (u32, u32)) -> (u32, u32) {
        if self.turns % 2 == 1 { (h, w) } else { (w, h) }
    }

    /// Turned a quarter more, clockwise (`by` 1) or anticlockwise (`by` -1),
    /// in a frame of shape `frame_aspect`: the picture as you see it turns,
    /// mirrored or not, around the box's centre, and the box takes its new
    /// shape. The crop stays on the same part of the picture, and a stretch
    /// turns with it (wider is taller).
    pub fn turn(&self, by: i8, frame_aspect: f32) -> Self {
        let mut p = *self;
        p.stretch = 1.0 / self.stretch.max(1e-3);
        // A mirrored picture turns the other way under its mirror.
        let by = if self.flip_h != self.flip_v { -by } else { by };
        p.turns = (self.turns as i8 + by).rem_euclid(4) as u8;
        let [l, t, r, b] = self.crop;
        p.crop = if by > 0 { [b, l, t, r] } else { [t, r, b, l] };
        let (cx, cy) = (self.x + self.w / 2.0, self.y + self.h / 2.0);
        p.w = self.h / frame_aspect;
        p.h = self.w * frame_aspect;
        p.x = cx - p.w / 2.0;
        p.y = cy - p.h / 2.0;
        p
    }

    /// Where a point of the turned picture (`u`, `v`, in its pixels, edges at
    /// whole numbers) is in the camera's own picture of `size`.
    pub fn unturn(&self, (u, v): (f32, f32), (w, h): (f32, f32)) -> (f32, f32) {
        match self.turns % 4 {
            1 => (v, h - u),
            2 => (w - u, h - v),
            3 => (w - v, u),
            _ => (u, v),
        }
    }

    /// The crop actually drawn, for a camera of `camera` pixels in a frame
    /// of `frame` pixels: the chosen crop, plus as much more off both sides
    /// of the long direction as it takes for what's left to have the box's
    /// exact shape (unstretched, see `stretch`). The picture fills its box,
    /// stretched only as much as it's been, however the box and the
    /// camera's shape (a format change, a portrait camera) differ.
    pub fn fill_crop(&self, camera: (u32, u32), frame: (u32, u32)) -> [f32; 4] {
        let [l, t, r, b] = self.crop;
        let vis_w = (1.0 - l - r).max(1e-3) * camera.0 as f32;
        let vis_h = (1.0 - t - b).max(1e-3) * camera.1 as f32;
        let box_w = self.w * frame.0 as f32;
        let box_h = self.h * frame.1 as f32;
        if vis_w <= 0.0 || vis_h <= 0.0 || box_w <= 0.0 || box_h <= 0.0 {
            return self.crop;
        }
        let (want, have) = (box_w / box_h / self.stretch.max(1e-3), vis_w / vis_h);
        if have > want {
            // Too wide: trim both sides evenly.
            let extra = (1.0 - want / have) * (1.0 - l - r) / 2.0;
            [l + extra, t, r + extra, b]
        } else {
            let extra = (1.0 - have / want) * (1.0 - t - b) / 2.0;
            [l, t + extra, r, b + extra]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Placement;

    fn p(w: f32, h: f32) -> Placement {
        Placement { x: 0.0, y: 0.0, w, h, crop: [0.0; 4], flip_h: false, flip_v: false, turns: 0, stretch: 1.0 }
    }

    #[test]
    fn automatic_prefers_landscape_16_9() {
        use super::Format;
        let f = |w, h| Format { width: w, height: h, fps_num: 30, fps_den: 1 };
        // The MacBook Pro camera's formats.
        let offered = [f(1552, 1552), f(1760, 1328), f(1328, 1760), f(1920, 1080), f(1080, 1920), f(1280, 720), f(640, 480)];
        let best = offered.iter().max_by_key(|f| f.auto_rank()).unwrap();
        assert_eq!((best.width, best.height), (1920, 1080));
        // Only 4:3 and portrait on offer: the landscape one.
        let best = [f(1328, 1760), f(1760, 1328)].into_iter().max_by_key(|f| f.auto_rank()).unwrap();
        assert_eq!((best.width, best.height), (1760, 1328));
        // 30 fps at 720p beats 15 fps at 1080p.
        let slow = Format { fps_num: 15, ..f(1920, 1080) };
        let best = [slow, f(1280, 720)].into_iter().max_by_key(|f| f.auto_rank()).unwrap();
        assert_eq!((best.width, best.height), (1280, 720));
    }

    #[test]
    fn turning_swaps_the_box_and_comes_back() {
        let mut q = p(0.25, 0.25);
        q.x = 0.5;
        q.y = 0.5;
        q.crop = [0.1, 0.0, 0.0, 0.0];
        let t = q.turn(1, 16.0 / 9.0);
        assert_eq!(t.turns, 1);
        // The left crop is now at the top; the box is as tall as it was wide, in pixels.
        assert_eq!(t.crop, [0.0, 0.1, 0.0, 0.0]);
        assert!((t.h * 1080.0 - q.w * 1920.0).abs() < 1e-3);
        assert!((t.x + t.w / 2.0 - 0.625).abs() < 1e-6);
        let back = t.turn(-1, 16.0 / 9.0);
        assert_eq!(back.turns, 0);
        assert_eq!(back.crop, q.crop);
        assert!((back.w - q.w).abs() < 1e-6 && (back.h - q.h).abs() < 1e-6);
        // Mirrored, a clockwise turn as seen is an anticlockwise one under the mirror.
        let m = Placement { flip_h: true, ..q };
        assert_eq!(m.turn(1, 1.0).turns, 3);
    }

    #[test]
    fn unturn_maps_corners() {
        let mut q = p(1.0, 1.0);
        // 4x2 camera; turned clockwise it's 2x4: its top-right corner is the camera's top-left.
        q.turns = 1;
        assert_eq!(q.unturn((2.0, 0.0), (4.0, 2.0)), (0.0, 0.0));
        assert_eq!(q.unturn((0.0, 4.0), (4.0, 2.0)), (4.0, 2.0));
        q.turns = 3;
        assert_eq!(q.unturn((0.0, 4.0), (4.0, 2.0)), (0.0, 0.0));
    }

    #[test]
    fn same_shape_keeps_the_crop() {
        // A 16:9 box (a quarter of a 16:9 frame) for a 16:9 camera.
        assert_eq!(p(0.25, 0.25).fill_crop((1920, 1080), (1920, 1080)), [0.0; 4]);
    }

    #[test]
    fn portrait_camera_in_a_landscape_box_is_trimmed_top_and_bottom() {
        // 1080x1920 into a 480x270 box: keep the middle 1080x607.5 rows.
        let c = p(0.25, 0.25).fill_crop((1080, 1920), (1920, 1080));
        assert!(c[0] == 0.0 && c[2] == 0.0);
        let kept = (1.0 - c[1] - c[3]) * 1920.0;
        assert!((kept - 607.5).abs() < 0.5, "{c:?}");
        assert!((c[1] - c[3]).abs() < 1e-6);
    }

    #[test]
    fn four_three_camera_in_a_wide_box_keeps_the_middle() {
        // 640x480 into 16:9: rows trimmed to 360.
        let c = p(0.25, 0.25).fill_crop((640, 480), (1920, 1080));
        assert!(((1.0 - c[1] - c[3]) * 480.0 - 360.0).abs() < 0.5, "{c:?}");
    }

    /// Stretched, a 4:3 camera fills a 16:9 box whole (not trimmed to it),
    /// and is trimmed only as much as a box of another shape needs; turned
    /// a quarter, it's stretched the other way.
    #[test]
    fn a_stretched_picture_fills_its_box() {
        let wide = Placement { stretch: (16.0 / 9.0) / (4.0 / 3.0), ..p(0.25, 0.25) };
        assert_eq!(wide.fill_crop((640, 480), (1920, 1080)), [0.0; 4]);
        // Twice as wide a box: stretched no more, so the middle half of
        // its rows fills it (960x270 at 4:3 stretched 4:3 is 640x240).
        let wider = Placement { w: 0.5, ..wide };
        let c = wider.fill_crop((640, 480), (1920, 1080));
        assert!(c[0] == 0.0 && c[2] == 0.0 && ((1.0 - c[1] - c[3]) * 480.0 - 240.0).abs() < 0.5, "{c:?}");
        assert!((wide.turn(1, 16.0 / 9.0).stretch * wide.stretch - 1.0).abs() < 1e-6);
    }

    #[test]
    fn the_users_crop_stays_inside() {
        // Left quarter cropped off a 16:9 camera, box still 16:9: the rest
        // (1440x1080) is too narrow, so top and bottom go.
        let mut q = p(0.25, 0.25);
        q.crop = [0.25, 0.0, 0.0, 0.0];
        let c = q.fill_crop((1920, 1080), (1920, 1080));
        assert_eq!(c[0], 0.25);
        let (w, h) = ((1.0 - c[0] - c[2]) * 1920.0, (1.0 - c[1] - c[3]) * 1080.0);
        assert!((w / h - 16.0 / 9.0).abs() < 1e-3, "{c:?}");
    }
}

/// Placement shared between the app and a running capture.
pub type SharedPlacement = Arc<Mutex<Placement>>;

/// A picture size and rate a camera can deliver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Format {
    pub width: u32,
    pub height: u32,
    /// Frames per second, as a ratio (29.97 is 30000/1001).
    pub fps_num: u32,
    pub fps_den: u32,
}

impl Format {
    pub fn fps(&self) -> f32 {
        self.fps_num as f32 / self.fps_den.max(1) as f32
    }

    /// How good a pick it is for "Automatic" (higher is better): at most
    /// 1080p at 30 fps or more, then landscape (a MacBook camera also offers
    /// 1080×1920, as many pixels as 1920×1080: it won the tie and the
    /// webcam came out portrait), then nearest 16:9, the biggest, the fastest.
    pub fn auto_rank(&self) -> (bool, bool, bool, i32, u32, u32) {
        let fits = self.width * self.height <= 1920 * 1080;
        let aspect = self.width as f32 / self.height.max(1) as f32;
        // Closeness to 16:9, in thousandths (negated: nearer ranks higher).
        let off = -((aspect - 16.0 / 9.0).abs() * 1000.0) as i32;
        (fits && self.fps() >= 29.5, fits, self.width > self.height, off, self.width * self.height, (self.fps() * 100.0) as u32)
    }

    /// "1920×1080 · 30 fps".
    pub fn label(&self) -> String {
        let fps = self.fps();
        let fps = if (fps - fps.round()).abs() < 0.05 { format!("{}", fps.round()) } else { format!("{fps:.2}") };
        format!("{}×{} · {fps} fps", self.width, self.height)
    }
}

/// A webcam to draw over the recording.
#[derive(Debug, Clone)]
pub struct Webcam {
    /// Backend id from [`list_cameras`].
    pub device: String,
    /// The format to open it in, or `None` for the best one (the biggest up to
    /// 1080p that does at least 30 fps).
    pub format: Option<Format>,
    pub placement: SharedPlacement,
}

impl PartialEq for Webcam {
    /// Same camera in the same format; where it sits changes live and doesn't count.
    fn eq(&self, other: &Self) -> bool {
        self.device == other.device && self.format == other.format
    }
}

/// Each camera's formats, by device, as last opened.
static FORMATS: Mutex<Option<std::collections::HashMap<String, Vec<Format>>>> = Mutex::new(None);

/// The formats camera `device` offers, biggest first — known once it has
/// been opened (the preview opens it).
pub fn formats(device: &str) -> Vec<Format> {
    FORMATS.lock().unwrap().as_ref().and_then(|m| m.get(device).cloned()).unwrap_or_default()
}

#[cfg_attr(not(any(target_os = "windows", target_os = "macos", target_os = "linux")), allow(dead_code))]
pub(crate) fn set_formats(device: &str, formats: Vec<Format>) {
    FORMATS.lock().unwrap().get_or_insert_with(Default::default).insert(device.to_owned(), formats);
}

/// Whether several cameras can be open (and drawn) at once: on Windows and
/// Linux. On macOS one is, the first asked for ([`keep_open`]).
pub const MANY: bool = cfg!(any(target_os = "windows", target_os = "linux"));

/// Whether cameras have their own settings window ([`open_settings`]): on
/// Windows, from the driver. macOS has none (its controls are in Control
/// Centre's Video Effects), nor does Linux (apps like `cameractrls` and
/// `guvcview` set a camera's controls for every app).
pub const HAS_SETTINGS: bool = cfg!(target_os = "windows");

/// Whether the app shows a camera's controls itself ([`camera_controls`]):
/// on Linux, where there's no settings window of the driver's to open, and
/// on Windows (the driver's window is still there, for what only it has).
pub const HAS_CONTROLS: bool = cfg!(any(target_os = "linux", target_os = "windows"));

/// Control ids, as V4L2 numbers them; Windows' own settings take the same
/// ids, so a setting is the same one everywhere (each camera's are saved).
pub mod cid {
    pub const BRIGHTNESS: u32 = 0x980900;
    pub const CONTRAST: u32 = 0x980901;
    pub const SATURATION: u32 = 0x980902;
    pub const HUE: u32 = 0x980903;
    pub const AUTO_WHITE_BALANCE: u32 = 0x98090c;
    pub const GAMMA: u32 = 0x980910;
    pub const GAIN: u32 = 0x980913;
    pub const POWER_LINE: u32 = 0x980918;
    pub const WHITE_BALANCE: u32 = 0x98091a;
    pub const SHARPNESS: u32 = 0x98091b;
    pub const BACKLIGHT: u32 = 0x98091c;
    pub const AUTO_EXPOSURE: u32 = 0x9a0901;
    pub const EXPOSURE: u32 = 0x9a0902;
    pub const EXPOSURE_DYNAMIC_FPS: u32 = 0x9a0903;
    pub const PAN: u32 = 0x9a0908;
    pub const TILT: u32 = 0x9a0909;
    pub const FOCUS: u32 = 0x9a090a;
    pub const AUTO_FOCUS: u32 = 0x9a090c;
    pub const ZOOM: u32 = 0x9a090d;
    pub const IRIS: u32 = 0x9a0911;
    /// `V4L2_EXPOSURE_MANUAL`, among the automatic exposure menu's items.
    pub const EXPOSURE_MANUAL: i64 = 1;
    /// `V4L2_CID_EXPOSURE`: an exposure in the camera's own steps, not a
    /// time (the PS3 Eye's; on Windows, one whose driver doesn't give it in
    /// DirectShow's powers of two of a second).
    pub const EXPOSURE_STEPS: u32 = 0x980911;
}

/// A setting a camera offers: a V4L2 control (Linux), or one of DirectShow's
/// camera and picture settings (Windows), by [`cid`].
#[derive(Debug, Clone, PartialEq)]
pub struct CameraControl {
    pub id: u32,
    pub name: String,
    pub kind: ControlKind,
    pub value: i64,
    pub default: i64,
    /// It does nothing now (exposure while it's automatic, say).
    pub inactive: bool,
}

/// What a camera control takes.
#[derive(Debug, Clone, PartialEq)]
pub enum ControlKind {
    Int { min: i64, max: i64, step: i64 },
    Bool,
    /// Its items, by value.
    Menu(Vec<(i64, String)>),
}

/// The settings camera `device` offers, with their values now (Linux; on
/// Windows, once the camera's open; none elsewhere). Reads the camera: call
/// it off the UI thread, or seldom.
pub fn camera_controls(device: &str) -> Vec<CameraControl> {
    #[cfg(target_os = "linux")]
    {
        crate::linux::camera_controls(device)
    }
    #[cfg(target_os = "windows")]
    {
        crate::win::camera_controls(device)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = device;
        Vec::new()
    }
}

/// The open camera's own picture, for showing it alone (its crop, and its
/// settings): R, G, B, A rows.
pub struct CameraPreview {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// Its number: pass it back to only get newer ones.
    pub seq: u64,
    /// How many of the camera's pictures so far were new, not repeats of
    /// the one before: over time, its real frame rate.
    pub new_pictures: u64,
}

/// Camera `device`'s newest picture, shrunk to at most `max_width` wide,
/// if it's open and has one newer than picture `newer_than`.
pub fn camera_picture(device: &str, max_width: u32, newer_than: u64) -> Option<CameraPreview> {
    #[cfg(target_os = "windows")]
    {
        crate::win::camera_picture(device, max_width, newer_than)
    }
    #[cfg(target_os = "linux")]
    {
        crate::linux::camera_picture(device, max_width, newer_than)
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        // One camera's open: it, if it's `device`.
        if SINGLE.lock().unwrap().as_deref() != Some(device) {
            return None;
        }
        #[cfg(target_os = "macos")]
        {
            crate::mac::camera::picture(max_width, newer_than)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (max_width, newer_than);
            None
        }
    }
}

/// A quick fingerprint of a camera's picture: a few thousand of its bytes,
/// spread over it. A repeated picture has the same; a new one (sensor noise
/// alone) a different one. Cameras repeat pictures when their exposure is
/// longer than a frame: counting only new ones gives their real frame rate.
#[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
pub(crate) fn fingerprint(bytes: &[u8]) -> u64 {
    let step = (bytes.len() / 4096).max(1);
    bytes.iter().step_by(step).fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x100_0000_01b3))
}

/// Settings set for each camera, by device: put back each time it's opened.
static REMEMBERED: Mutex<Option<std::collections::HashMap<String, Vec<(u32, i64)>>>> = Mutex::new(None);

/// Remember these settings for camera `device` (control id, value), to put
/// back each time it's opened: cameras forget theirs when unplugged. Replaces
/// what was remembered for it.
pub fn remember_controls(device: &str, values: Vec<(u32, i64)>) {
    REMEMBERED.lock().unwrap().get_or_insert_with(Default::default).insert(device.to_owned(), values);
}

#[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
pub(crate) fn remembered_controls(device: &str) -> Vec<(u32, i64)> {
    REMEMBERED.lock().unwrap().as_ref().and_then(|m| m.get(device).cloned()).unwrap_or_default()
}

/// Put back the settings remembered for camera `device`, as it opens:
/// switches and menus first (a manual exposure time is only taken once
/// automatic exposure is off), then the rest.
#[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
pub(crate) fn apply_remembered(device: &str) {
    let saved = remembered_controls(device);
    if saved.is_empty() {
        return;
    }
    let all = camera_controls(device);
    let kind_first = |id: u32| all.iter().find(|c| c.id == id).is_some_and(|c| !matches!(c.kind, ControlKind::Int { .. }));
    let (first, rest): (Vec<_>, Vec<_>) = saved.into_iter().partition(|(id, _)| kind_first(*id));
    for (id, value) in first {
        if let Err(e) = set_camera_control(device, id, value) {
            eprintln!("camera setting {id:#x}: {e:#}");
        }
    }
    // The rest, unless the switches just set leave them doing nothing (a
    // white balance temperature while white balance is automatic): the camera
    // refuses those.
    let now = camera_controls(device);
    for (id, value) in rest {
        if now.iter().any(|c| c.id == id && c.inactive) {
            continue;
        }
        if let Err(e) = set_camera_control(device, id, value) {
            eprintln!("camera setting {id:#x}: {e:#}");
        }
    }
}

/// Set a camera setting; it applies at once, in every app (Linux, Windows).
pub fn set_camera_control(device: &str, id: u32, value: i64) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        crate::linux::set_camera_control(device, id, value)
    }
    #[cfg(target_os = "windows")]
    {
        crate::win::set_camera_control(device, id, value)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = (device, id, value);
        anyhow::bail!("this camera's settings can't be changed here")
    }
}

/// Open the camera's own settings window (exposure, focus, white balance…, as
/// its driver offers them). Changes apply to the camera straight away, in
/// every app. Returns at once; the window runs by itself.
pub fn open_settings(device: &str, name: &str) {
    #[cfg(target_os = "windows")]
    {
        crate::win::open_camera_settings(device.to_owned(), name.to_owned());
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (device, name);
    }
}

/// Keep these cameras (device, format) open, and close the rest. Call it
/// whenever the webcams set up change: a webcam stays open for as long as
/// it's set up, previewed and recorded or not, because closing a camera can
/// reset what was set in its own settings window. Doesn't wait. Where only
/// one can be open ([`MANY`]), the first is.
pub fn keep_open(want: Vec<(String, Option<Format>)>) {
    #[cfg(target_os = "windows")]
    {
        crate::win::keep_camera_open(want);
    }
    #[cfg(target_os = "linux")]
    {
        crate::linux::keep_camera_open(want);
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let first = want.into_iter().next();
        let mut single = SINGLE.lock().unwrap();
        if *single != first.as_ref().map(|(d, _)| d.clone()) {
            // Another camera: what was said of the last isn't of this one.
            *single = first.as_ref().map(|(d, _)| d.clone());
            STATUS.lock().unwrap().take();
        }
        drop(single);
        #[cfg(target_os = "macos")]
        {
            crate::mac::camera::keep_open(first);
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = first;
        }
    }
}

/// The one camera kept open, where only one can be ([`MANY`]).
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
static SINGLE: Mutex<Option<String>> = Mutex::new(None);

/// Cameras on this machine.
pub fn list_cameras() -> Vec<Device> {
    #[cfg(target_os = "windows")]
    {
        crate::win::list_cameras()
    }
    #[cfg(target_os = "macos")]
    {
        crate::mac::camera::list_cameras()
    }
    #[cfg(target_os = "linux")]
    {
        crate::linux::list_cameras()
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        Vec::new()
    }
}

/// How the camera is doing, for the UI.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Status {
    /// No webcam is being captured.
    #[default]
    Off,
    Opening,
    /// Delivering pictures this big.
    Live { width: u32, height: u32 },
    /// It couldn't be opened (unplugged, or in use by another app on Windows 10).
    Unavailable(String),
}

/// Each camera kept open's state, by device.
static STATUS: Mutex<Option<std::collections::HashMap<String, Status>>> = Mutex::new(None);

/// Camera `device`'s state right now: off unless it's kept open.
pub fn status_of(device: &str) -> Status {
    STATUS.lock().unwrap().as_ref().and_then(|m| m.get(device).cloned()).unwrap_or_default()
}

/// The state of the camera kept open, if one is (any, where several are).
pub fn status() -> Status {
    STATUS.lock().unwrap().as_ref().and_then(|m| m.values().next().cloned()).unwrap_or_default()
}

/// Camera `device` is in state `s` now; off forgets it.
#[cfg_attr(not(any(target_os = "windows", target_os = "linux")), allow(dead_code))]
pub(crate) fn set_status_of(device: &str, s: Status) {
    let mut all = STATUS.lock().unwrap();
    let all = all.get_or_insert_with(Default::default);
    if s == Status::Off {
        all.remove(device);
    } else {
        all.insert(device.to_owned(), s);
    }
}

/// The one camera open is in state `s` now (macOS: see [`MANY`]).
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn set_status(s: Status) {
    let single = SINGLE.lock().unwrap().clone();
    let mut all = STATUS.lock().unwrap();
    let all = all.get_or_insert_with(Default::default);
    all.clear();
    if let Some(device) = single.filter(|_| s != Status::Off) {
        all.insert(device, s);
    }
}

/// For checks: the size of the newest picture the open camera delivered.
pub fn frame_size() -> Option<(usize, usize)> {
    #[cfg(target_os = "macos")]
    {
        crate::mac::camera::frame_size()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}
