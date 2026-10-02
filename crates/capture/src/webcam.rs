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
pub const AVAILABLE: bool = cfg!(target_os = "windows");

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

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn set_formats(device: &str, formats: Vec<Format>) {
    *FORMATS.lock().unwrap() = Some((device.to_owned(), formats));
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

/// Cameras on this machine.
pub fn list_cameras() -> Vec<Device> {
    #[cfg(target_os = "windows")]
    {
        crate::win::list_cameras()
    }
    #[cfg(not(target_os = "windows"))]
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

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn set_status(s: Status) {
    *STATUS.lock().unwrap() = s;
}
