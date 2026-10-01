//! Capture backend abstraction.
//!
//! macOS uses `sck` (ScreenCaptureKit + VideoToolbox; mics via cpal, mixed in
//! `mixer`). Windows/Linux will get a libobs backend behind this same `Recorder`
//! trait so the app never depends on which one is live; until then there is no
//! recorder there.

use std::path::PathBuf;

#[cfg(target_os = "macos")]
mod aac;
pub mod audio;
pub mod mixer;
pub mod mp4meta;
pub mod output;
pub mod sources;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "macos")]
pub mod sck;
#[cfg(target_os = "macos")]
mod writer;

#[cfg(target_os = "macos")]
pub use sck::SckRecorder;

/// The capture backend for this platform. `live` gets the meters and supplies
/// each source's volume.
pub fn default_recorder(live: std::sync::Arc<mixer::LiveAudio>) -> Box<dyn Recorder> {
    #[cfg(target_os = "macos")]
    {
        Box::new(SckRecorder::new(live))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = live;
        Box::new(Unsupported)
    }
}

/// Stand-in until the libobs backend exists, so the app runs (library, editor)
/// and says why capture won't start.
#[cfg(not(target_os = "macos"))]
struct Unsupported;

#[cfg(not(target_os = "macos"))]
impl Recorder for Unsupported {
    fn start(&mut self, _: Mode, _: &EncodeSettings) -> anyhow::Result<()> {
        anyhow::bail!("recording isn't available on this platform yet")
    }
    fn save_clip(&mut self) -> anyhow::Result<PathBuf> {
        anyhow::bail!("recording isn't available on this platform yet")
    }
    fn stop(&mut self) -> anyhow::Result<Option<PathBuf>> {
        Ok(None)
    }
    fn is_running(&self) -> bool {
        false
    }
}

/// Screens the backend can capture; their ids go in `EncodeSettings::screen_id`.
pub fn list_screens() -> Vec<Device> {
    #[cfg(target_os = "macos")]
    {
        macos::list_shareable().screens
    }
    #[cfg(not(target_os = "macos"))]
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
    /// Backend-specific id (display id, bundle id, device name).
    pub id: String,
    pub name: String,
}

/// Displays and apps ScreenCaptureKit can capture.
#[derive(Debug, Clone, Default)]
pub struct Devices {
    pub screens: Vec<Device>,
    /// Running applications (for per-app audio capture on macOS/Windows).
    pub apps: Vec<Device>,
}

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
    /// Use a hardware encoder (VideoToolbox) vs. software (x264).
    pub use_hardware: bool,
    pub replay_seconds: u32,
    /// Backend id of the screen to capture.
    pub screen_id: String,
    /// Audio sources, in track order. Track 1 is the mix of every source with
    /// `in_mix` (so the file sounds right in any player); each source with
    /// `own_track` follows on its own track, for rebalancing later.
    pub sources: Vec<sources::AudioSource>,
}

/// A capture backend. Implemented by `SckRecorder` (and, later, a libobs backend).
pub trait Recorder {
    /// Begin capturing in the given mode with the given settings.
    fn start(&mut self, mode: Mode, settings: &EncodeSettings) -> anyhow::Result<()>;

    /// In `ReplayBuffer` mode, flush the buffered window to a file and return its path.
    fn save_clip(&mut self) -> anyhow::Result<PathBuf>;

    /// Stop capturing. In `Record` mode, returns the finished file.
    fn stop(&mut self) -> anyhow::Result<Option<PathBuf>>;

    /// Whether a capture is currently running.
    fn is_running(&self) -> bool;
}
