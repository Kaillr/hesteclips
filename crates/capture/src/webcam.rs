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
    /// right, bottom, as fractions of its width or height (the camera's own
    /// sides, before any flip).
    pub crop: [f32; 4],
    /// Mirrored left to right, and upside down.
    pub flip_h: bool,
    pub flip_v: bool,
}

impl Placement {
    /// The bottom-right corner, a quarter of the frame wide, for a camera whose
    /// picture is `camera_aspect` (width / height) in a frame of `frame_aspect`.
    pub fn default_for(camera_aspect: f32, frame_aspect: f32) -> Self {
        let w = 0.25;
        let h = w * frame_aspect / camera_aspect;
        let margin_x = 0.02;
        let margin_y = margin_x * frame_aspect;
        Self { x: 1.0 - w - margin_x, y: 1.0 - h - margin_y, w, h, crop: [0.0; 4], flip_h: false, flip_v: false }
    }

    /// The webcam switched off: a box of no size, which is never drawn, so a
    /// running capture drops it at once. The camera itself is closed too
    /// ([`keep_open`]) and taken up again when it's switched back on.
    pub fn hidden() -> Self {
        Self { x: 0.0, y: 0.0, w: 0.0, h: 0.0, crop: [0.0; 4], flip_h: false, flip_v: false }
    }

    pub fn is_hidden(&self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }

    /// The crop actually drawn, for a camera of `camera` pixels in a frame
    /// of `frame` pixels: the chosen crop, plus as much more off both sides
    /// of the long direction as it takes for what's left to have the box's
    /// exact shape. The picture fills its box and is never stretched, however
    /// the box and the camera's shape (a format change, a portrait camera)
    /// differ.
    pub fn fill_crop(&self, camera: (u32, u32), frame: (u32, u32)) -> [f32; 4] {
        let [l, t, r, b] = self.crop;
        let vis_w = (1.0 - l - r).max(1e-3) * camera.0 as f32;
        let vis_h = (1.0 - t - b).max(1e-3) * camera.1 as f32;
        let box_w = self.w * frame.0 as f32;
        let box_h = self.h * frame.1 as f32;
        if vis_w <= 0.0 || vis_h <= 0.0 || box_w <= 0.0 || box_h <= 0.0 {
            return self.crop;
        }
        let (want, have) = (box_w / box_h, vis_w / vis_h);
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
        Placement { x: 0.0, y: 0.0, w, h, crop: [0.0; 4], flip_h: false, flip_v: false }
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

static FORMATS: Mutex<Option<(String, Vec<Format>)>> = Mutex::new(None);

/// The formats camera `device` offers, biggest first — known once it has
/// been opened (the preview opens it).
pub fn formats(device: &str) -> Vec<Format> {
    FORMATS.lock().unwrap().as_ref().filter(|(d, _)| d == device).map(|(_, f)| f.clone()).unwrap_or_default()
}

#[cfg_attr(not(any(target_os = "windows", target_os = "macos", target_os = "linux")), allow(dead_code))]
pub(crate) fn set_formats(device: &str, formats: Vec<Format>) {
    *FORMATS.lock().unwrap() = Some((device.to_owned(), formats));
}

/// Whether cameras have their own settings window ([`open_settings`]): on
/// Windows, from the driver. macOS has none (its controls are in Control
/// Centre's Video Effects), nor does Linux (apps like `cameractrls` and
/// `guvcview` set a camera's controls for every app).
pub const HAS_SETTINGS: bool = cfg!(target_os = "windows");

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

/// Keep this camera (device, format) open, or close it with `None`. Call it
/// whenever the chosen webcam changes: a webcam stays open for as long as it's
/// set up, previewed and recorded or not, because closing a camera can reset
/// what was set in its own settings window. Doesn't wait.
pub fn keep_open(want: Option<(String, Option<Format>)>) {
    #[cfg(target_os = "windows")]
    {
        crate::win::keep_camera_open(want);
    }
    #[cfg(target_os = "macos")]
    {
        crate::mac::camera::keep_open(want);
    }
    #[cfg(target_os = "linux")]
    {
        crate::linux::keep_camera_open(want);
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        let _ = want;
    }
}

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

static STATUS: Mutex<Status> = Mutex::new(Status::Off);

/// The webcam's state right now.
pub fn status() -> Status {
    STATUS.lock().unwrap().clone()
}

#[cfg_attr(not(any(target_os = "windows", target_os = "macos", target_os = "linux")), allow(dead_code))]
pub(crate) fn set_status(s: Status) {
    *STATUS.lock().unwrap() = s;
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
