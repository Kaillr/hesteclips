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
            Encoder::Software => "Software (slower, uses the CPU)",
        }
    }
}

/// Light or dark look: the system's, or one chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

impl Theme {
    pub const ALL: [Theme; 3] = [Theme::System, Theme::Light, Theme::Dark];
    pub fn label(self) -> &'static str {
        match self {
            Theme::System => "System",
            Theme::Light => "Light",
            Theme::Dark => "Dark",
        }
    }
    pub fn preference(self) -> egui::ThemePreference {
        match self {
            Theme::System => egui::ThemePreference::System,
            Theme::Light => egui::ThemePreference::Light,
            Theme::Dark => egui::ThemePreference::Dark,
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

/// Kept so saved settings still load; not offered in the UI, since the
/// VideoToolbox encoder only does average-bitrate today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RateControl {
    /// Constant bitrate — predictable file size, best for streaming/replay.
    Cbr,
    /// Constant quality — best quality per byte, variable size.
    Cqp,
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

/// What the video shows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CaptureTarget {
    /// The display chosen by `display_index`.
    #[default]
    Screen,
    /// Games and apps: whichever is in focus is recorded (see
    /// `capture::VideoSource::Apps`). With `away_screen`, tabbing out to
    /// anything else shows the away screen instead of the last one.
    Apps {
        apps: Vec<CaptureApp>,
        #[serde(default = "yes")]
        away_screen: bool,
    },
    /// One app, from before several could be chosen; read only, turned into
    /// `Apps` on load.
    App { id: String, name: String },
}

/// A webcam drawn over the recording.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebcamCfg {
    /// Backend id (`capture::webcam::list_cameras`).
    pub id: String,
    /// What to call it while it isn't connected.
    pub name: String,
    /// The format to open it in; `None` picks the best.
    #[serde(default)]
    pub format: Option<FormatCfg>,
    pub placement: PlacementCfg,
    /// In the clips. Off keeps the camera, format and placement for later
    /// (and closes the camera).
    #[serde(default = "yes")]
    pub enabled: bool,
}

/// A camera format: see `capture::webcam::Format`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormatCfg {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
}

impl From<FormatCfg> for capture::webcam::Format {
    fn from(f: FormatCfg) -> Self {
        Self { width: f.width, height: f.height, fps_num: f.fps_num, fps_den: f.fps_den }
    }
}

impl From<capture::webcam::Format> for FormatCfg {
    fn from(f: capture::webcam::Format) -> Self {
        Self { width: f.width, height: f.height, fps_num: f.fps_num, fps_den: f.fps_den }
    }
}

/// Where the webcam sits: see `capture::webcam::Placement`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PlacementCfg {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    #[serde(default)]
    pub crop: [f32; 4],
    #[serde(default)]
    pub flip_h: bool,
    #[serde(default)]
    pub flip_v: bool,
    /// Quarter turns clockwise.
    #[serde(default)]
    pub turns: u8,
    /// The shape (width / height) of the frame this was laid out in. The box
    /// is fractions of the frame, so in a frame of another shape (another
    /// display, an ultrawide) the same fractions are another shape: it's
    /// refitted then (`refit`), keeping its own shape in pixels.
    #[serde(default)]
    pub frame_aspect: Option<f32>,
}

impl PlacementCfg {
    /// Keep the box right for a frame of shape `frame_aspect`: rescaled to
    /// keep its shape in pixels, around the same centre, as wide a share of
    /// the frame (narrower if that won't fit). Laid out before the frame's
    /// shape was kept: given the camera's shape (`camera_aspect`, when
    /// known), as every box was meant to have. Returns whether it changed.
    pub fn refit(&mut self, frame_aspect: f32, camera_aspect: Option<f32>) -> bool {
        if frame_aspect <= 0.0 || self.w <= 0.0 || self.h <= 0.0 {
            return false;
        }
        let box_aspect = match self.frame_aspect {
            Some(was) if (was - frame_aspect).abs() < 1e-3 => return false,
            // Its shape in pixels, in the frame it was made for.
            Some(was) => self.w * was / self.h,
            None => match camera_aspect {
                Some(cam) => {
                    let visible = (1.0 - self.crop[0] - self.crop[2]).max(0.05) / (1.0 - self.crop[1] - self.crop[3]).max(0.05);
                    cam * visible
                }
                None => return false, // wait for the camera's shape
            },
        };
        let (cx, cy) = (self.x + self.w / 2.0, self.y + self.h / 2.0);
        let mut w = self.w;
        let mut h = w * frame_aspect / box_aspect;
        if h > 1.0 {
            w *= 1.0 / h;
            h = 1.0;
        }
        self.w = w;
        self.h = h;
        self.x = cx - w / 2.0;
        self.y = cy - h / 2.0;
        self.frame_aspect = Some(frame_aspect);
        true
    }
}

impl From<PlacementCfg> for capture::webcam::Placement {
    fn from(p: PlacementCfg) -> Self {
        Self { x: p.x, y: p.y, w: p.w, h: p.h, crop: p.crop, flip_h: p.flip_h, flip_v: p.flip_v, turns: p.turns }
    }
}

impl PlacementCfg {
    /// `p`, laid out in a frame of shape `frame_aspect`.
    pub fn laid_out(p: capture::webcam::Placement, frame_aspect: f32) -> Self {
        Self { x: p.x, y: p.y, w: p.w, h: p.h, crop: p.crop, flip_h: p.flip_h, flip_v: p.flip_v, turns: p.turns, frame_aspect: Some(frame_aspect) }
    }
}

/// A game or app to record, by executable. `name` is what to call it while it
/// isn't running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureApp {
    pub id: String,
    pub name: String,
}

/// A game with anti-cheat that game capture may be used on anyway (the user
/// accepted the risk), by executable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowedGame {
    pub id: String,
    pub name: String,
    /// The anti-cheat it was found to use, to show.
    pub anticheat: String,
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
    /// A microphone's background noise (fans, keyboard, hum) removed. Off
    /// unless turned on.
    #[serde(default)]
    pub noise_removal: bool,
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
            noise_removal: false,
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

/// Which games name clips after what happened, and what osu!'s names show.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GameTitles {
    pub cs2: bool,
    pub dota2: bool,
    pub league: bool,
    pub osu: bool,
    /// Accuracy and FC (and the combo mid-play).
    pub osu_accuracy: bool,
    pub osu_misses: bool,
    pub osu_pp: bool,
    pub osu_mods: bool,
    pub osu_stars: bool,
    pub osu_map: bool,
    pub osu_artist: bool,
    /// The order of the parts of an osu! name, as dragged in Settings.
    pub osu_order: Vec<OsuPart>,
}

impl Default for GameTitles {
    fn default() -> Self {
        Self {
            cs2: true,
            dota2: true,
            league: true,
            osu: true,
            osu_accuracy: true,
            osu_misses: true,
            osu_pp: true,
            osu_mods: true,
            osu_stars: false,
            osu_map: true,
            osu_artist: false,
            osu_order: OsuPart::ALL.to_vec(),
        }
    }
}

/// A part of an osu! clip's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OsuPart {
    Pp,
    Misses,
    Accuracy,
    Mods,
    Stars,
    Map,
}

impl OsuPart {
    pub const ALL: [OsuPart; 6] = [OsuPart::Pp, OsuPart::Misses, OsuPart::Accuracy, OsuPart::Mods, OsuPart::Stars, OsuPart::Map];

    pub fn label(self) -> &'static str {
        match self {
            OsuPart::Pp => "pp",
            OsuPart::Misses => "Misses",
            OsuPart::Accuracy => "Accuracy",
            OsuPart::Mods => "Mods",
            OsuPart::Stars => "Star rating",
            OsuPart::Map => "Map",
        }
    }
}

impl GameTitles {
    /// The parts in their order: each once, any missing (an older settings
    /// file) at the end.
    pub fn osu_parts(&self) -> Vec<OsuPart> {
        let mut parts: Vec<OsuPart> = Vec::new();
        for p in self.osu_order.iter().chain(OsuPart::ALL.iter()) {
            if !parts.contains(p) {
                parts.push(*p);
            }
        }
        parts
    }

    pub fn osu_on(&self, part: OsuPart) -> bool {
        match part {
            OsuPart::Pp => self.osu_pp,
            OsuPart::Misses => self.osu_misses,
            OsuPart::Accuracy => self.osu_accuracy,
            OsuPart::Mods => self.osu_mods,
            OsuPart::Stars => self.osu_stars,
            OsuPart::Map => self.osu_map,
        }
    }

    pub fn osu_on_mut(&mut self, part: OsuPart) -> &mut bool {
        match part {
            OsuPart::Pp => &mut self.osu_pp,
            OsuPart::Misses => &mut self.osu_misses,
            OsuPart::Accuracy => &mut self.osu_accuracy,
            OsuPart::Mods => &mut self.osu_mods,
            OsuPart::Stars => &mut self.osu_stars,
            OsuPart::Map => &mut self.osu_map,
        }
    }
}

/// Persisted to `<config_dir>/hesteclips/settings.json`. `#[serde(default)]` lets
/// older files load after new fields are added.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordSettings {
    // --- Common (always visible on the Settings tab) ---
    /// What the video shows: a whole display or one app's window.
    pub capture: CaptureTarget,
    /// The games and apps list while the whole screen is recorded, so
    /// switching back to games and apps brings it back.
    pub idle_apps: Option<CaptureTarget>,
    /// A webcam over the picture, if any.
    pub webcam: Option<WebcamCfg>,
    /// Index into the runtime-detected display list.
    pub display_index: usize,
    pub fps: u32,
    pub resolution: OutputResolution,
    pub video_bitrate_mbps: u32,

    pub replay_seconds: u32,
    /// Start the replay buffer as soon as the app opens, so you never miss a moment.
    pub auto_start_buffer: bool,
    /// Download new versions in the background and install them on quit.
    pub auto_update: bool,
    /// Show what you're clipping on your Discord profile (Rich Presence).
    pub discord_presence: bool,
    /// Saying "hashtag HesteClip that" saves a clip (while the replay buffer runs).
    pub voice_clip: bool,

    // --- Advanced (collapsible; sane defaults) ---
    pub encoder: Encoder,
    pub container: Container,
    pub rate_control: RateControl,
    /// Seconds between keyframes. Lower = more seekable & cleaner replay cuts,
    /// larger files. 2s is a sane default.
    pub keyframe_interval_secs: u32,
    pub output_dir: PathBuf,
    /// Save each clip in a folder named after the game it's of (`osu!/`,
    /// `Desktop/` when there's no game), shown as categories in the library.
    pub folder_per_game: bool,
    /// Game folders renamed in the library: the folder a game's clips would
    /// go in (its name, as `clips::folder_name` makes it) → the one they go in.
    pub game_folders: std::collections::BTreeMap<String, String>,
    /// Light, dark, or the system's.
    pub theme: Theme,
    /// Closing the window keeps HesteClips running in the tray (where
    /// there is one) instead of quitting.
    pub close_to_tray: bool,
    /// The first close asked which (then it just does it).
    pub close_asked: bool,
    /// Deleting a clip removes it for good instead of moving it to the
    /// Recycle Bin / Trash (then it asks first).
    pub delete_permanently: bool,
    /// Name clips after what happened in the game ("3 kills on Mirage"), for
    /// games that report it (CS2, Dota 2, League of Legends, osu!).
    pub game_details: bool,
    /// Which games, and what goes in osu!'s names.
    pub game_titles: GameTitles,

    // --- Sources page ---
    pub audio_sources: Vec<AudioSourceCfg>,
    /// Keep the clip's mix from clipping when sources add up too loud.
    pub limiter: bool,
    /// Record games Windows' own capture can't see properly (exclusive
    /// fullscreen, a lost cursor) through the game capture hook, where it's
    /// safe: never on games with anti-cheat unless allowed below (see
    /// `capture::GameHook`). Recording the screen or games and
    /// apps alike.
    pub game_capture: bool,
    /// Games with anti-cheat game capture may be used on anyway.
    pub game_capture_allowed: Vec<AllowedGame>,

    // --- Shortcuts ---
    pub shortcuts: Shortcuts,
    /// The sound played when a clip is saved.
    pub save_sound: SaveSound,
    /// The editor's preview height, as a share of the window (the timeline
    /// gets the rest), as last dragged.
    pub editor_preview_share: f32,
    /// The clip player's volume (0..=1 on its slider) and mute, as last set,
    /// so a clip never opens at full blast after you'd turned it down.
    pub player_volume: f32,
    pub player_muted: bool,
    /// Pre-sources audio list; migrated into `audio_sources` on load.
    #[serde(skip_serializing)]
    audio_tracks: Option<Vec<LegacyAudioTrack>>,
}

impl Default for RecordSettings {
    fn default() -> Self {
        Self {
            capture: CaptureTarget::Screen,
            idle_apps: None,
            webcam: None,
            display_index: 0,
            fps: 60,
            resolution: OutputResolution::Native,
            video_bitrate_mbps: 40,

            replay_seconds: 60,
            auto_start_buffer: true,
            auto_update: true,
            discord_presence: false,
            voice_clip: true,
            encoder: Encoder::Auto,
            container: Container::Mp4,
            rate_control: RateControl::Cbr,
            keyframe_interval_secs: 2,
            output_dir: default_output_dir(),
            folder_per_game: true,
            game_folders: Default::default(),
            game_details: true,
            delete_permanently: false,
            theme: Theme::System,
            close_to_tray: true,
            close_asked: false,
            game_titles: GameTitles::default(),
            audio_sources: default_sources(),
            limiter: true,
            game_capture: true,
            game_capture_allowed: Vec::new(),
            shortcuts: Shortcuts::default(),
            save_sound: SaveSound::default(),
            editor_preview_share: 0.5,
            player_volume: 1.0,
            player_muted: false,
            audio_tracks: None,
        }
    }
}

/// The sound played when a clip is saved (see `sound.rs`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SaveSound {
    pub enabled: bool,
    /// A built-in sound's id, or `file:<path>` for one of `custom`.
    pub sound: String,
    /// 0..=1, applied on a curve (see `sound::gain`).
    pub volume: f32,
    /// Sounds you've added, copied into HesteClips' own folder.
    pub custom: Vec<CustomSound>,
}

impl Default for SaveSound {
    fn default() -> Self {
        Self { enabled: true, sound: "horse".into(), volume: 0.6, custom: Vec::new() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomSound {
    pub name: String,
    pub file: PathBuf,
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

/// Dev aid: `HESTECLIPS_SETTINGS=<file>` loads the settings from that file
/// instead (to check a setup without touching the real one; dev-hook
/// launches never save).
fn load_path() -> Option<PathBuf> {
    std::env::var_os("HESTECLIPS_SETTINGS").map(PathBuf::from).or_else(settings_path)
}

impl RecordSettings {
    /// Last saved settings, or defaults on first run / unreadable file.
    pub fn load() -> Self {
        let mut s: Self = load_path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        s.upgrade_capture();
        if let Some(old) = s.audio_tracks.take() {
            s.audio_sources = migrate(&old);
            // Persist now: sources get fresh ids, and their volumes key on them.
            Self::save_json(&s.to_json());
        }
        s
    }

    /// A single chosen app (from before several could be) becomes a list of one.
    fn upgrade_capture(&mut self) {
        if let CaptureTarget::App { id, name } = &self.capture {
            self.capture = CaptureTarget::Apps {
                apps: vec![CaptureApp { id: id.clone(), name: name.clone() }],
                away_screen: true,
            };
        }
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

#[cfg(test)]
mod placement_tests {
    use super::PlacementCfg;

    fn cfg(x: f32, y: f32, w: f32, h: f32, frame_aspect: Option<f32>) -> PlacementCfg {
        PlacementCfg { x, y, w, h, crop: [0.0; 4], flip_h: false, flip_v: false, turns: 0, frame_aspect }
    }

    fn pixel_aspect(p: &PlacementCfg, frame: f32) -> f32 {
        p.w * frame / p.h
    }

    #[test]
    fn same_frame_shape_leaves_it_alone() {
        let mut p = cfg(0.7, 0.7, 0.25, 0.25, Some(16.0 / 9.0));
        assert!(!p.refit(16.0 / 9.0, Some(16.0 / 9.0)));
    }

    #[test]
    fn moving_to_an_ultrawide_keeps_its_shape() {
        // A 16:9 box on a 16:9 display, then a 6720x2836 ultrawide.
        let mut p = cfg(0.7, 0.7, 0.25, 0.25, Some(16.0 / 9.0));
        let (cx, cy) = (p.x + p.w / 2.0, p.y + p.h / 2.0);
        let uw = 6720.0 / 2836.0;
        assert!(p.refit(uw, None));
        assert!((pixel_aspect(&p, uw) - 16.0 / 9.0).abs() < 1e-4);
        assert!((p.x + p.w / 2.0 - cx).abs() < 1e-5 && (p.y + p.h / 2.0 - cy).abs() < 1e-5);
        assert_eq!(p.frame_aspect, Some(uw));
    }

    #[test]
    fn the_stretched_box_from_before_gets_the_cameras_shape() {
        // The saved box from the bug report: w = h = 0.2718 (the frame's
        // shape on any display), no frame shape kept, a 16:9 camera.
        let mut p = cfg(0.728, 0.707, 0.2718, 0.2718, None);
        let mac = 3024.0 / 1964.0;
        assert!(!p.refit(mac, None), "waits for the camera");
        assert!(p.refit(mac, Some(16.0 / 9.0)));
        assert!((pixel_aspect(&p, mac) - 16.0 / 9.0).abs() < 1e-4);
    }

    #[test]
    fn never_taller_than_the_frame() {
        // A portrait-shaped box moved to a very wide frame.
        let mut p = cfg(0.4, 0.0, 0.2, 1.0, Some(1.0));
        p.refit(4.0, None);
        assert!(p.h <= 1.0 + 1e-6);
        assert!((pixel_aspect(&p, 4.0) - 0.2).abs() < 1e-4);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_webcam_from_before_the_switch_is_on() {
        let s: RecordSettings = serde_json::from_str(
            r#"{"webcam": {"id": "cam", "name": "Cam", "placement": {"x": 0.7, "y": 0.7, "w": 0.25, "h": 0.25}}}"#,
        )
        .unwrap();
        assert!(s.webcam.unwrap().enabled);
    }

    #[test]
    fn single_app_setting_becomes_a_list() {
        let mut s: RecordSettings =
            serde_json::from_str(r#"{"fps": 144, "capture": {"type": "app", "id": "game.exe", "name": "Game"}}"#).unwrap();
        s.upgrade_capture();
        assert_eq!(s.fps, 144, "the rest of the settings survive");
        assert_eq!(
            s.capture,
            CaptureTarget::Apps {
                apps: vec![CaptureApp { id: "game.exe".into(), name: "Game".into() }],
                away_screen: true,
            }
        );
        let round: RecordSettings = serde_json::from_str(&s.to_json()).unwrap();
        assert_eq!(round.capture, s.capture);
    }
}
