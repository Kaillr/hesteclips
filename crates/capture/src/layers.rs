//! The pictures a frame is made of, bottom first: the recording's own (the
//! screen, or games and apps), webcams and capture cards, other displays.
//!
//! The app keeps the list in [`SharedLayers`] up to date every frame, as it
//! does the screen's and the webcam's placements; a running capture reads it
//! for every frame it makes, so a layer added, taken out, moved in front or
//! behind, or placed somewhere else shows at once, while recording too.
//! Each layer has an id of its own that stays with it while it's changed, so
//! a capture keeps what it opened for it (a camera, a display's capture) for
//! as long as it's there.
//!
//! Windows draws every layer ([`MANY`]). macOS and Linux draw the recording's
//! own picture and one webcam (`EncodeSettings::webcam`), in front or behind
//! as `ScreenPlacement::in_front` says.

use std::sync::{Arc, Mutex};

use crate::ScreenPlacement;
use crate::webcam::{Format, Placement};

/// Whether this platform draws more than the recording's own picture and
/// one webcam: several webcams, other displays, in any order.
pub const MANY: bool = cfg!(target_os = "windows");

/// What a layer shows, and where.
#[derive(Debug, Clone, PartialEq)]
pub enum LayerSource {
    /// The recording's own picture (`EncodeSettings::video`), placed as
    /// `EncodeSettings::screen` says.
    Main,
    /// A webcam or capture card (`webcam::list_cameras`), in `format` (the
    /// best one when `None`), cropped, placed and turned as `placement` says.
    Camera { device: String, format: Option<Format>, placement: Placement },
    /// A whole display (`list_screens`), placed as `placement` says (its
    /// box, its framing, its crop, turned and flipped; `in_front` unused:
    /// the list's order says what's in front).
    Display { id: String, placement: ScreenPlacement },
    /// One app's window (its main one, by executable: `list_windowed_apps`),
    /// its inside without the title bar and borders, placed as a display is.
    /// While it isn't open, nothing; minimized, its last picture.
    Window { app: String, placement: ScreenPlacement },
    /// A picture (an image file, words drawn by the app), blended over
    /// what's under it by its alpha, placed as a display is.
    Picture { picture: Picture, placement: ScreenPlacement },
    /// All of its box one colour (R, G, B, A: blended by A).
    Colour { rgba: [u8; 4], placement: ScreenPlacement },
    /// A web page (alerts, a chat overlay), `width`×`height` big, `css`
    /// added to it, blended by its transparency, placed as a display is
    /// (Windows: see [`BROWSER`]).
    Browser { url: String, width: u32, height: u32, css: String, placement: ScreenPlacement },
    /// A picture that moves (an animated GIF): each of its frames for its
    /// time, over and over, blended as a picture is.
    Animation { animation: Animation, placement: ScreenPlacement },
    /// A video file, `width`×`height` its own size, `fps` its frame rate
    /// (as its index says: the decoder's own can be far off): played from
    /// the start again at its end (`looped`; else its last picture stays),
    /// waiting while it's hidden and going on when it's shown again, or
    /// starting over (`restart`). Its sound plays into the audio source
    /// `sound` names, if it does (`sources::SourceKind::Video`). Placed as
    /// a display is (Windows: see [`VIDEO`]).
    Video { path: std::path::PathBuf, width: u32, height: u32, fps: f64, looped: bool, restart: bool, sound: Option<String>, placement: ScreenPlacement },
}

/// Whether this platform can show a web page as a layer.
pub const BROWSER: bool = cfg!(target_os = "windows");

/// Whether this platform can play a video file as a layer.
pub const VIDEO: bool = cfg!(target_os = "windows");

/// A video file's picture size, if it can be played (see [`VIDEO`]).
pub fn video_size(path: &std::path::Path) -> Option<(u32, u32)> {
    #[cfg(target_os = "windows")]
    {
        crate::win::decode::Decoder::open_rgba(path, 64).ok().map(|d| d.source_size())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
        None
    }
}

/// An animated picture's frames, each with how long it shows, shared as
/// a [`Picture`] is (the same animation is the same `Arc`).
#[derive(Debug, Clone)]
pub struct Animation(pub Arc<Vec<(crate::StillImage, std::time::Duration)>>);

impl PartialEq for Animation {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// A picture for a layer, shared: the same picture is the same `Arc`, so
/// the list is compared every frame without comparing pixels.
#[derive(Debug, Clone)]
pub struct Picture(pub Arc<crate::StillImage>);

impl PartialEq for Picture {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl LayerSource {
    /// Whether it's blended by alpha over what's under it, rather than
    /// covering its place.
    pub fn blended(&self) -> bool {
        matches!(self, LayerSource::Picture { .. } | LayerSource::Colour { .. } | LayerSource::Browser { .. } | LayerSource::Animation { .. })
    }
}

/// Whether this platform can capture one app's window as a layer.
pub const WINDOWS: bool = cfg!(target_os = "windows");

/// App `exe`'s main window's inside, in pixels, if it's open (Windows).
pub fn app_window_size(exe: &str) -> Option<(u32, u32)> {
    #[cfg(target_os = "windows")]
    {
        crate::win::app_window_size(exe)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = exe;
        None
    }
}

/// One picture in the frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Layer {
    /// Its own, kept while it's changed, never another layer's.
    pub id: u64,
    pub source: LayerSource,
}

impl Layer {
    /// Whether it's drawn: a camera or display shown (hidden placements
    /// draw nothing). The main picture's own placement says for it.
    pub fn shown(&self) -> bool {
        match &self.source {
            LayerSource::Main => true,
            LayerSource::Camera { placement, .. } => !placement.is_hidden(),
            LayerSource::Display { placement, .. }
            | LayerSource::Window { placement, .. }
            | LayerSource::Picture { placement, .. }
            | LayerSource::Colour { placement, .. }
            | LayerSource::Browser { placement, .. }
            | LayerSource::Animation { placement, .. }
            | LayerSource::Video { placement, .. } => !placement.hidden,
        }
    }
}

/// The layers, bottom first, shared between the app and a running capture.
pub type SharedLayers = Arc<Mutex<Vec<Layer>>>;

/// Only the recording's own picture.
pub fn main_only() -> SharedLayers {
    Arc::new(Mutex::new(vec![Layer { id: 0, source: LayerSource::Main }]))
}

/// The recording's own picture and webcam `w` (as it's placed now), the
/// picture in front when `main_in_front`: what one webcam and the screen's
/// `in_front` say, as layers.
pub fn with_webcam(w: Option<&crate::webcam::Webcam>, main_in_front: bool) -> SharedLayers {
    let main = Layer { id: 0, source: LayerSource::Main };
    let camera = w.map(|w| Layer { id: 1, source: LayerSource::Camera { device: w.device.clone(), format: w.format, placement: *w.placement.lock().unwrap() } });
    let list = match camera {
        Some(c) if main_in_front => vec![c, main],
        Some(c) => vec![main, c],
        None => vec![main],
    };
    Arc::new(Mutex::new(list))
}
