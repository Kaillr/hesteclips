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
pub mod mixer;
pub mod monitor;
pub mod mp4meta;
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) mod mp4read;
#[cfg(any(target_os = "windows", target_os = "linux"))]
mod mp4file;
#[cfg(any(target_os = "windows", target_os = "linux", test))]
mod mp4mux;
pub mod output;
pub mod preview;
pub mod webcam;
pub mod sources;

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
#[cfg(target_os = "macos")]
pub use mac::decode;
#[cfg(target_os = "windows")]
pub use win::decode;

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

/// The recording's size for a source `w`×`h`: downscaled to `target_height`
/// if that's smaller, and to fit [`MAX_SIDE`] (a 5K2K ultrawide, 6720×2836
/// natively, records at 4096×1728). Keeps the shape; even dimensions, as
/// H.264 4:2:0 requires.
pub fn output_size(w: u32, h: u32, target_height: Option<u32>) -> (u32, u32) {
    let (w, h) = (w.max(2) as f64, h.max(2) as f64);
    let mut scale = target_height.map_or(1.0, |t| (t as f64 / h).min(1.0));
    scale = scale.min(MAX_SIDE as f64 / (w * scale).max(1.0)).min(1.0).min(MAX_SIDE as f64 / h);
    let even = |v: f64| ((v.round() as u32) & !1).max(2);
    (even(w * scale), even(h * scale))
}

/// The size a recording of `source` will be, without starting anything
/// (cheap: asks the system for display sizes). Games and apps record at the
/// main display's size. `None` if the display isn't known (on Linux, until
/// the screen has been picked in the system's dialog once).
pub fn frame_size(source: &VideoSource, target_height: Option<u32>) -> Option<(u32, u32)> {
    let id = match source {
        VideoSource::Screen { id } => Some(id.as_str()),
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
    native.map(|(w, h)| output_size(w, h, target_height))
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
pub const APP_CAPTURE: bool = cfg!(any(target_os = "windows", target_os = "macos"));

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
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        Vec::new()
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
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
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
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
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
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows", target_os = "linux")), allow(dead_code))]
    pub(crate) fn new(finish: impl FnOnce() -> anyhow::Result<PathBuf> + Send + 'static) -> Self {
        Self(Box::new(finish))
    }

    /// Wait for the clip to be written; its path.
    pub fn finish(self) -> anyhow::Result<PathBuf> {
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
}

#[cfg(test)]
mod size_tests {
    use super::output_size;

    #[test]
    fn output_size_keeps_aspect_and_even() {
        assert_eq!(output_size(2560, 1440, None), (2560, 1440));
        assert_eq!(output_size(2560, 1440, Some(1080)), (1920, 1080));
        assert_eq!(output_size(1920, 1080, Some(1440)), (1920, 1080));
        assert_eq!(output_size(3440, 1440, Some(721)), (1722, 720));
    }

    #[test]
    fn output_size_fits_the_encoder() {
        assert_eq!(output_size(6720, 2836, None), (4096, 1728));
        assert_eq!(output_size(7680, 4320, Some(2160)), (3840, 2160));
        assert_eq!(output_size(5120, 2880, None), (4096, 2304));
        // Portrait.
        assert_eq!(output_size(2160, 7680, None), (1152, 4096));
        assert_eq!(output_size(4096, 2304, None), (4096, 2304));
    }
}
