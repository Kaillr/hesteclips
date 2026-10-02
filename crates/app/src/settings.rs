//! Recording configuration, edited on the Settings and Sources pages and
//! persisted as JSON.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Which encoder to drive. `Auto` picks the best hardware encoder for the platform
/// (VideoToolbox on macOS, NVENC/AMF/QSV on Windows, VAAPI/NVENC on Linux).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Encoder {
    Auto,
    Hardware,
    Software,
}

impl Encoder {
    pub fn label(self) -> &'static str {
        match self {
            Encoder::Auto => "Auto (best hardware)",
            Encoder::Hardware => "Hardware",
            Encoder::Software => "Software (x264)",
        }
    }
}

/// File format for recordings. Both survive a crash mid-recording (written as
/// fragmented files). MKV used to be offered; old settings load as MP4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Container {
    /// Plays everywhere. The default.
    #[serde(alias = "Mkv")]
    Mp4,
    /// QuickTime; handy for Final Cut / Apple workflows.
    Mov,
}

impl Container {
    pub fn ext(self) -> &'static str {
        match self {
            Container::Mp4 => "mp4",
            Container::Mov => "mov",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Container::Mp4 => "MP4 (plays everywhere)",
            Container::Mov => "MOV (QuickTime)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RateControl {
    /// Constant bitrate — predictable file size, best for streaming/replay.
    Cbr,
    /// Constant quality — best quality per byte, variable size.
    Cqp,
}

impl RateControl {
    pub fn label(self) -> &'static str {
        match self {
            RateControl::Cbr => "CBR (constant bitrate)",
            RateControl::Cqp => "CQP (constant quality)",
        }
    }
}

/// Output resolution. `Native` records at the capture source's resolution;
/// the others downscale on encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputResolution {
    Native,
    P720,
    P1080,
    P1440,
    P2160,
}

impl OutputResolution {
    pub fn label(self) -> &'static str {
        match self {
            OutputResolution::Native => "Native (match source)",
            OutputResolution::P720 => "720p",
            OutputResolution::P1080 => "1080p",
            OutputResolution::P1440 => "1440p",
            OutputResolution::P2160 => "2160p (4K)",
        }
    }
    /// Target height in pixels, or `None` for native (no rescale).
    pub fn height(self) -> Option<u32> {
        match self {
            OutputResolution::Native => None,
            OutputResolution::P720 => Some(720),
            OutputResolution::P1080 => Some(1080),
            OutputResolution::P1440 => Some(1440),
            OutputResolution::P2160 => Some(2160),
        }
    }

    pub const ALL: [OutputResolution; 5] = [
        OutputResolution::Native,
        OutputResolution::P720,
        OutputResolution::P1080,
        OutputResolution::P1440,
        OutputResolution::P2160,
    ];
}

/// Where an audio source's sound comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SourceKind {
    /// A mic or audio interface. `device` is a device name, or
    /// `capture::audio::DEFAULT_DEVICE` to follow the OS default.
    Microphone { device: String },
    /// Everything the computer plays. `exclude_apps` leaves out apps that are
    /// added as their own source, so they aren't heard twice.
    Desktop { exclude_apps: bool },
    /// One application, by bundle id (macOS) / executable (Windows). `app_name`
    /// is what to call it while it isn't running.
    App { bundle_id: String, app_name: String },
}

/// One audio source on the Sources page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioSourceCfg {
    /// Stable id: keys the live volume/meter and survives renames.
    pub id: String,
    pub name: String,
    pub kind: SourceKind,
    /// Captured at all. Off = as if it weren't there (but keeps its settings).
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Part of what the clip sounds like (track 1).
    #[serde(default = "yes")]
    pub in_mix: bool,
    /// Also on its own track, to rebalance in the editor.
    #[serde(default = "yes")]
    pub own_track: bool,
    #[serde(default)]
    pub volume_db: f32,
    #[serde(default)]
    pub muted: bool,
}

fn yes() -> bool {
    true
}

impl AudioSourceCfg {
    pub fn new(name: &str, kind: SourceKind) -> Self {
        Self {
            id: new_source_id(),
            name: name.into(),
            kind,
            enabled: true,
            in_mix: true,
            own_track: true,
            volume_db: 0.0,
            muted: false,
        }
    }

    pub fn icon(&self) -> &'static str {
        match self.kind {
            SourceKind::Microphone { .. } => "🎤",
            SourceKind::Desktop { .. } => "🖥",
            SourceKind::App { .. } => "🎮",
        }
    }
}

/// A short random-ish id for a new source.
fn new_source_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    format!("src-{:x}-{}", t.as_millis(), N.fetch_add(1, Ordering::Relaxed))
}

/// The audio sources a new install starts with: your mic and your computer.
pub fn default_sources() -> Vec<AudioSourceCfg> {
    vec![
        AudioSourceCfg::new("Microphone", SourceKind::Microphone { device: capture::audio::DEFAULT_DEVICE.into() }),
        AudioSourceCfg::new("Desktop", SourceKind::Desktop { exclude_apps: true }),
    ]
}

/// The audio list before sources (≤ 2026-10-01): inputs/outputs, each always
/// both mixed and on its own track. Read only to migrate.
#[derive(Debug, Clone, Deserialize)]
pub struct LegacyAudioTrack {
    name: String,
    kind: LegacyKind,
    device_id: String,
    enabled: bool,
}

#[derive(Debug, Clone, Copy, Deserialize)]
enum LegacyKind {
    Input,
    Output,
}

fn migrate(old: &[LegacyAudioTrack]) -> Vec<AudioSourceCfg> {
    let mut out: Vec<AudioSourceCfg> = Vec::new();
    for t in old {
        let kind = match t.kind {
            LegacyKind::Input => SourceKind::Microphone { device: t.device_id.clone() },
            // There's one desktop capture whatever the output device; keep one.
            LegacyKind::Output if out.iter().any(|s| matches!(s.kind, SourceKind::Desktop { .. })) => continue,
            LegacyKind::Output => SourceKind::Desktop { exclude_apps: true },
        };
        let mut s = AudioSourceCfg::new(&t.name, kind);
        s.enabled = t.enabled;
        out.push(s);
    }
    out
}

/// Persisted to `<config_dir>/hesteclips/settings.json`. `#[serde(default)]` lets
/// older files load after new fields are added.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordSettings {
    // --- Common (always visible on the Settings tab) ---
    /// Index into the runtime-detected display list.
    pub display_index: usize,
    pub fps: u32,
    pub resolution: OutputResolution,
    pub video_bitrate_mbps: u32,

    pub replay_seconds: u32,
    /// Start the replay buffer as soon as the app opens, so you never miss a moment.
    pub auto_start_buffer: bool,

    // --- Advanced (collapsible; sane defaults) ---
    pub encoder: Encoder,
    pub container: Container,
    pub rate_control: RateControl,
    /// Seconds between keyframes. Lower = more seekable & cleaner replay cuts,
    /// larger files. 2s is a sane default.
    pub keyframe_interval_secs: u32,
    pub output_dir: PathBuf,

    // --- Sources page ---
    pub audio_sources: Vec<AudioSourceCfg>,
    /// Keep the clip's mix from clipping when sources add up too loud.
    pub limiter: bool,

    // --- Shortcuts ---
    pub shortcuts: Shortcuts,
    /// Pre-sources audio list; migrated into `audio_sources` on load.
    #[serde(skip_serializing)]
    audio_tracks: Option<Vec<LegacyAudioTrack>>,
}

impl Default for RecordSettings {
    fn default() -> Self {
        Self {
            display_index: 0,
            fps: 60,
            resolution: OutputResolution::Native,
            video_bitrate_mbps: 40,

            replay_seconds: 60,
            auto_start_buffer: true,
            encoder: Encoder::Auto,
            container: Container::Mp4,
            rate_control: RateControl::Cbr,
            keyframe_interval_secs: 2,
            output_dir: default_output_dir(),
            audio_sources: default_sources(),
            limiter: true,
            shortcuts: Shortcuts::default(),
            audio_tracks: None,
        }
    }
}

/// The global shortcuts, as `global_hotkey` strings ("alt+F10"). Empty = off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Shortcuts {
    pub save_clip: String,
    pub toggle_buffer: String,
    pub toggle_record: String,
}

impl Default for Shortcuts {
    fn default() -> Self {
        Self { save_clip: "alt+F10".into(), toggle_buffer: "alt+F8".into(), toggle_record: "alt+F9".into() }
    }
}

/// One of the actions a global shortcut can trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutAction {
    SaveClip,
    ToggleBuffer,
    ToggleRecord,
}

impl ShortcutAction {
    pub const ALL: [ShortcutAction; 3] = [ShortcutAction::SaveClip, ShortcutAction::ToggleBuffer, ShortcutAction::ToggleRecord];

    pub fn label(self) -> &'static str {
        match self {
            ShortcutAction::SaveClip => "Save clip",
            ShortcutAction::ToggleBuffer => "Start / stop replay buffer",
            ShortcutAction::ToggleRecord => "Start / stop recording",
        }
    }
}

impl Shortcuts {
    pub fn get(&self, a: ShortcutAction) -> &str {
        match a {
            ShortcutAction::SaveClip => &self.save_clip,
            ShortcutAction::ToggleBuffer => &self.toggle_buffer,
            ShortcutAction::ToggleRecord => &self.toggle_record,
        }
    }

    pub fn set(&mut self, a: ShortcutAction, value: String) {
        match a {
            ShortcutAction::SaveClip => self.save_clip = value,
            ShortcutAction::ToggleBuffer => self.toggle_buffer = value,
            ShortcutAction::ToggleRecord => self.toggle_record = value,
        }
    }
}

/// Common frame-rate choices for the picker.
pub const FPS_CHOICES: [u32; 5] = [30, 60, 120, 144, 240];

/// `~/Movies/hesteclips` on macOS, the platform video dir elsewhere, falling back
/// to the current dir if none is known.
pub fn default_output_dir() -> PathBuf {
    dirs::video_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("hesteclips")
}

fn settings_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("hesteclips").join("settings.json"))
}

impl RecordSettings {
    /// Last saved settings, or defaults on first run / unreadable file.
    pub fn load() -> Self {
        let mut s: Self = settings_path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        if let Some(old) = s.audio_tracks.take() {
            s.audio_sources = migrate(&old);
            // Persist now: sources get fresh ids, and their volumes key on them.
            Self::save_json(&s.to_json());
        }
        s
    }

    /// Serialized form, used both to save and to detect changes cheaply.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    pub fn save_json(json: &str) {
        if let Some(path) = settings_path() {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(path, json);
        }
    }
}
