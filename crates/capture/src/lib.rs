//! Capture backend abstraction.
//!
//! macOS uses `sck` (ScreenCaptureKit + VideoToolbox), Windows uses `win`
//! (Windows Graphics Capture + Media Foundation + WASAPI), both behind the same
//! `Recorder` trait so the app never depends on which one is live. Mics come
//! from cpal everywhere and are mixed in `mixer`. Linux has no recorder yet.

use std::path::PathBuf;

#[cfg(target_os = "macos")]
mod aac;
pub mod audio;
#[cfg(target_os = "macos")]
mod avwriter;
pub mod mixer;
pub mod mp4meta;
#[cfg(any(target_os = "windows", test))]
mod mp4mux;
pub mod output;
pub mod preview;
pub mod webcam;
pub mod sources;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "macos")]
pub mod sck;
#[cfg(target_os = "windows")]
pub mod win;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod writer;

#[cfg(target_os = "macos")]
pub use sck::SckRecorder;
#[cfg(target_os = "windows")]
pub use win::WinRecorder;

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
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = live;
        Box::new(Unsupported)
    }
}

/// Stand-in where there's no backend yet, so the app runs (library, editor)
/// and says why capture won't start.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
struct Unsupported;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl Recorder for Unsupported {
    fn start(&mut self, _: Mode, _: &EncodeSettings) -> anyhow::Result<()> {
        anyhow::bail!("recording isn't available on this platform yet")
    }
    fn save_clip(&mut self) -> anyhow::Result<PendingClip> {
        anyhow::bail!("recording isn't available on this platform yet")
    }
    fn stop(&mut self) -> anyhow::Result<Option<PathBuf>> {
        Ok(None)
    }
    fn is_running(&self) -> bool {
        false
    }
}

/// What a recording's video shows. This is the bottom layer of the picture;
/// overlays (a webcam) will go on top of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoSource {
    /// A whole display, by backend id (see [`list_screens`]).
    Screen { id: String },
    /// Games and apps, by executable name (see [`list_windowed_apps`]): records
    /// whichever of them was last in focus, for as long as it's showing, even
    /// while you click into something else. When it stops showing (minimized,
    /// as games are when you alt-tab), it shows the away screen
    /// (`away_when_unfocused`) or keeps its last picture. Fitted into a frame
    /// the size of the main display. The away screen also shows while none of
    /// them is open; each is picked up as soon as it opens.
    Apps { ids: Vec<String>, away_when_unfocused: bool },
}

/// A still picture shown instead of an app (see [`EncodeSettings::away_screen`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StillImage {
    pub width: u32,
    pub height: u32,
    /// Rows top to bottom, 4 bytes per pixel in B, G, R, A order, opaque.
    pub bgra: Vec<u8>,
}

/// Whether this platform can record games and apps ([`VideoSource::Apps`]).
pub const APP_CAPTURE: bool = cfg!(target_os = "windows");

/// Running apps with a window, for [`VideoSource::Apps`], sorted by name.
pub fn list_windowed_apps() -> Vec<Device> {
    #[cfg(target_os = "windows")]
    {
        win::list_windowed_apps()
    }
    #[cfg(not(target_os = "windows"))]
    {
        Vec::new()
    }
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
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Vec::new()
    }
}

/// Running apps the user could add as an audio source, sorted by name. Their
/// ids go in `SourceKind::App` (a bundle id on macOS, an executable name on
/// Windows).
pub fn list_apps() -> Vec<Device> {
    #[cfg(target_os = "macos")]
    {
        macos::list_apps()
    }
    #[cfg(target_os = "windows")]
    {
        win::list_apps()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
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

/// AAC bitrate per audio track, bits per second.
pub const AUDIO_BITRATE: u32 = 192_000;

/// Everything the backend needs to start a capture. The app builds this from its
/// own richer `RecordSettings`.
#[derive(Debug, Clone)]
pub struct EncodeSettings {
    pub output_dir: PathBuf,
    /// Container extension for finished files, e.g. "mp4" or "mkv".
    pub container_ext: String,
    pub fps: u32,
    pub video_bitrate_kbps: u32,
    /// Downscale target height, or `None` to keep native resolution.
    pub target_height: Option<u32>,
    pub keyframe_interval_secs: u32,
    /// Use a hardware encoder (VideoToolbox, or the GPU's Media Foundation
    /// encoder on Windows) vs. software.
    pub use_hardware: bool,
    pub replay_seconds: u32,
    /// What the video shows.
    pub video: VideoSource,
    /// Shown while recording games and apps and there's nothing to show (none
    /// is open, or you've tabbed out); black if `None`. Scaled to the frame.
    pub away_screen: Option<std::sync::Arc<StillImage>>,
    /// A webcam drawn over the picture.
    pub webcam: Option<webcam::Webcam>,
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
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    pub(crate) fn new(finish: impl FnOnce() -> anyhow::Result<PathBuf> + Send + 'static) -> Self {
        Self(Box::new(finish))
    }

    /// Wait for the clip to be written; its path.
    pub fn finish(self) -> anyhow::Result<PathBuf> {
        (self.0)()
    }
}

/// A capture backend. Implemented by `SckRecorder` (macOS) and `WinRecorder` (Windows).
pub trait Recorder {
    /// Begin capturing in the given mode with the given settings.
    fn start(&mut self, mode: Mode, settings: &EncodeSettings) -> anyhow::Result<()>;

    /// In `ReplayBuffer` mode, save the buffered window as a clip. Its moment is
    /// taken right away; the returned [`PendingClip`] finishes writing it,
    /// which can take a while — so another can be saved meanwhile.
    fn save_clip(&mut self) -> anyhow::Result<PendingClip>;

    /// Stop capturing. In `Record` mode, returns the finished file.
    fn stop(&mut self) -> anyhow::Result<Option<PathBuf>>;

    /// Whether a capture is currently running.
    fn is_running(&self) -> bool;

    /// Change what the running capture records, without restarting it, if the
    /// change allows (a different list of games and apps). Returns whether it
    /// was applied; otherwise it takes effect on the next start.
    fn update_video(&mut self, _video: &VideoSource) -> bool {
        false
    }
}
