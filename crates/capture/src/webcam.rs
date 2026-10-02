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
    /// right, bottom, as fractions of its width or height.
    pub crop: [f32; 4],
}

impl Placement {
    /// The bottom-right corner, a quarter of the frame wide, for a camera whose
    /// picture is `camera_aspect` (width / height) in a frame of `frame_aspect`.
    pub fn default_for(camera_aspect: f32, frame_aspect: f32) -> Self {
        let w = 0.25;
        let h = w * frame_aspect / camera_aspect;
        let margin_x = 0.02;
        let margin_y = margin_x * frame_aspect;
        Self { x: 1.0 - w - margin_x, y: 1.0 - h - margin_y, w, h, crop: [0.0; 4] }
    }
}

/// Placement shared between the app and a running capture.
pub type SharedPlacement = Arc<Mutex<Placement>>;

/// A webcam to draw over the recording.
#[derive(Debug, Clone)]
pub struct Webcam {
    /// Backend id from [`list_cameras`].
    pub device: String,
    pub placement: SharedPlacement,
}

impl PartialEq for Webcam {
    /// Same camera; where it sits changes live and doesn't count.
    fn eq(&self, other: &Self) -> bool {
        self.device == other.device
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
