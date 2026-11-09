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
//! Windows, macOS and Linux draw every layer ([`MANY`]). Where it can't be
//! done, the recording's own picture and one webcam
//! (`EncodeSettings::webcam`) are drawn, in front or behind as
//! `ScreenPlacement::in_front` says.

use std::sync::{Arc, Mutex};

use crate::ScreenPlacement;
use crate::webcam::{Format, Placement};

/// Whether this platform draws more than the recording's own picture and
/// one webcam: several webcams, other displays, in any order.
pub const MANY: bool = cfg!(any(target_os = "windows", target_os = "macos", target_os = "linux"));

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
    /// (see [`BROWSER`]).
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
    /// a display is (see [`VIDEO`]).
    Video { path: std::path::PathBuf, width: u32, height: u32, fps: f64, looped: bool, restart: bool, sound: Option<String>, placement: ScreenPlacement },
}

/// Whether this platform can show a web page as a layer: WebView2 on
/// Windows, WebKit on macOS (both part of the system), Chromium's engine
/// on Linux, fetched the first time a page is shown ([`browser_engine`]).
pub const BROWSER: bool = cfg!(any(target_os = "windows", target_os = "macos", target_os = "linux"));

/// Where the engine that draws web pages stands.
#[derive(Debug, Clone, PartialEq)]
pub enum BrowserEngine {
    /// Here: pages draw.
    Ready,
    /// Being downloaded (Linux, the first time a page is shown): bytes so
    /// far, of how many.
    Fetching { got: u64, total: Option<u64> },
    /// Downloaded, being unpacked.
    Unpacking,
    /// It couldn't be fetched, or doesn't start: why.
    Failed(String),
}

/// Where the engine that draws web pages stands. On Linux, asking starts
/// fetching it, if it isn't here yet (a page is being shown).
pub fn browser_engine() -> BrowserEngine {
    #[cfg(target_os = "linux")]
    {
        crate::linux::browser_engine()
    }
    #[cfg(not(target_os = "linux"))]
    {
        BrowserEngine::Ready
    }
}

/// After a failure: fetch the engine again, or start it again.
pub fn retry_browser_engine() {
    #[cfg(target_os = "linux")]
    crate::linux::retry_browser_engine();
}

/// The script adding a web page's own style (`LayerSource::Browser`'s
/// `css`) as it loads: a style sheet at the end of its head, so it goes
/// over the page's own, once (run again, it adds nothing); none without a
/// style.
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
pub(crate) fn style_script(css: &str) -> Option<String> {
    if css.trim().is_empty() {
        return None;
    }
    let text = serde_json::to_string(css).ok()?;
    Some(format!(
        "(() => {{ const add = () => {{ if (document.getElementById('hasteclip-style')) return; const s = document.createElement('style'); s.id = 'hasteclip-style'; s.textContent = {text}; (document.head || document.documentElement).appendChild(s); }}; if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', add); else add(); }})();"
    ))
}

/// Whether this platform can play a video file as a layer.
pub const VIDEO: bool = cfg!(any(target_os = "windows", target_os = "macos", target_os = "linux"));

/// A video file's picture size, if it can be played (see [`VIDEO`]).
pub fn video_size(path: &std::path::Path) -> Option<(u32, u32)> {
    #[cfg(target_os = "windows")]
    {
        crate::win::decode::Decoder::open_rgba(path, 64).ok().map(|d| d.source_size())
    }
    #[cfg(target_os = "macos")]
    {
        crate::mac::layers::video_size(path)
    }
    #[cfg(target_os = "linux")]
    {
        crate::linux::video_size(path)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
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

/// Whether this platform can capture one app's window as a layer (on
/// Linux where its games and apps can be recorded: `list_windowed_apps`
/// lists none elsewhere).
pub const WINDOWS: bool = cfg!(any(target_os = "windows", target_os = "macos", target_os = "linux"));

/// App `exe`'s main window's inside, in pixels, if it's open (Windows; on
/// macOS an app by its bundle id: its frontmost window; Linux as its layer
/// last captured it, or as KDE Plasma says).
pub fn app_window_size(exe: &str) -> Option<(u32, u32)> {
    #[cfg(target_os = "windows")]
    {
        crate::win::app_window_size(exe)
    }
    #[cfg(target_os = "macos")]
    {
        crate::mac::layers::app_window_size(exe)
    }
    #[cfg(target_os = "linux")]
    {
        crate::linux::app_window_size(exe)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
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

#[cfg(test)]
mod tests {
    /// No style, no script; a style's quotes and lines kept, as a string.
    #[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_style_sheet_goes_in_as_written() {
        assert_eq!(super::style_script("  \n"), None);
        let s = super::style_script("body { font-family: \"Inter\"; }\n.x{}").unwrap();
        assert!(s.contains(r#""body { font-family: \"Inter\"; }\n.x{}""#), "{s}");
    }
}
