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

/// A place to stream to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum StreamService {
    #[default]
    Twitch,
    YouTube,
    Kick,
    /// Any RTMP or RTMPS server: its address and key given.
    Custom,
}

impl StreamService {
    pub const ALL: [StreamService; 4] = [StreamService::Twitch, StreamService::YouTube, StreamService::Kick, StreamService::Custom];

    pub fn label(self) -> &'static str {
        match self {
            StreamService::Twitch => "Twitch",
            StreamService::YouTube => "YouTube",
            StreamService::Kick => "Kick",
            StreamService::Custom => "Another server",
        }
    }

    /// The address everyone streams to, for a service that has one (Kick's
    /// is your own, from its dashboard). Over RTMPS where ffmpeg can.
    pub fn server(self, rtmps: bool) -> Option<&'static str> {
        match self {
            StreamService::Twitch => Some("rtmp://live.twitch.tv/app"),
            StreamService::YouTube if rtmps => Some("rtmps://a.rtmps.youtube.com:443/live2"),
            StreamService::YouTube => Some("rtmp://a.rtmp.youtube.com/live2"),
            StreamService::Kick | StreamService::Custom => None,
        }
    }

    /// Where the stream key is found.
    pub fn key_from(self) -> &'static str {
        match self {
            StreamService::Twitch => "Twitch's Creator Dashboard, in Settings → Stream",
            StreamService::YouTube => "YouTube Studio, under Go live → Stream",
            StreamService::Kick => "Kick's Creator Dashboard, in Channel → Stream URL and Key (the URL too)",
            StreamService::Custom => "your server",
        }
    }
}

/// One place to stream to, whether Go live goes there, and what it's sent:
/// each place its own picture (Twitch takes less than YouTube).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamDestination {
    pub service: StreamService,
    /// The stream key: a password, never shown unless asked.
    pub key: String,
    /// The server, for Kick and another server (Twitch's and YouTube's are
    /// their own).
    pub server: String,
    /// What another server's called ("Restream").
    pub name: String,
    pub enabled: bool,
    /// The picture's short side brought down to this (never up), in p; 0
    /// keeps the size clips are recorded at.
    pub height: u32,
    /// Frames a second, at most: a whole fraction of the recording's frame
    /// rate is sent (`capture::stream::stream_rate`).
    pub fps: u32,
    pub kbps: u32,
}

impl Default for StreamDestination {
    fn default() -> Self {
        Self::new(StreamService::Twitch)
    }
}

/// Its key never printed.
impl std::fmt::Debug for StreamDestination {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("StreamDestination")
            .field("service", &self.service)
            .field("server", &self.server)
            .field("enabled", &self.enabled)
            .field("height", &self.height)
            .field("fps", &self.fps)
            .field("kbps", &self.kbps)
            .finish_non_exhaustive()
    }
}

impl StreamDestination {
    /// A place on `service`, sent what it takes: 1080p at 60 fps, at
    /// Twitch's 6,000 kbps (its most, for most channels), YouTube's 12,000
    /// (what it suggests for 1080p at 60) or Kick's 8,000 (its most).
    pub fn new(service: StreamService) -> Self {
        let kbps = match service {
            StreamService::Twitch | StreamService::Custom => 6000,
            StreamService::YouTube => 12_000,
            StreamService::Kick => 8000,
        };
        Self { service, key: String::new(), server: String::new(), name: String::new(), enabled: true, height: 1080, fps: 60, kbps }
    }

    /// What it's sent, for the capture.
    pub fn quality(&self) -> capture::stream::Quality {
        capture::stream::Quality { short_side: (self.height > 0).then_some(self.height), fps: self.fps.max(1), kbps: self.kbps.max(100) }
    }

    /// What it's called: the service, or another server's own name.
    pub fn name(&self) -> String {
        match self.service {
            StreamService::Custom if !self.name.trim().is_empty() => self.name.trim().to_owned(),
            s => s.label().to_owned(),
        }
    }

    /// Its server: the service's own, or the one given.
    pub fn server(&self, rtmps: bool) -> String {
        self.service.server(rtmps).map_or_else(|| self.server.trim().to_owned(), str::to_owned)
    }

    /// What's missing before it can be streamed to, if anything.
    pub fn missing(&self) -> Option<&'static str> {
        let server = self.server(false);
        if self.service.server(false).is_none() && server.is_empty() {
            return Some("its server");
        }
        if self.service.server(false).is_none() && !(server.starts_with("rtmp://") || server.starts_with("rtmps://")) {
            return Some("a server starting rtmp:// or rtmps://");
        }
        // Another server's key may be in its address already.
        if self.key.trim().is_empty() && self.service != StreamService::Custom {
            return Some("its stream key");
        }
        None
    }

    /// Where the capture sends it, when it's set up.
    pub fn destination(&self, rtmps: bool) -> Option<capture::stream::Destination> {
        if self.missing().is_some() {
            return None;
        }
        let key = self.key.trim().to_owned();
        Some(capture::stream::Destination { name: self.name(), url: capture::stream::join(&self.server(rtmps), &key), secret: key, quality: self.quality() })
    }
}

/// Going live: where to, each place with its own picture (clips and
/// recordings keep theirs).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamCfg {
    pub destinations: Vec<StreamDestination>,
}

impl StreamCfg {
    /// The destinations Go live sends to: turned on, and set up.
    pub fn ready(&self) -> impl Iterator<Item = &StreamDestination> {
        self.destinations.iter().filter(|d| d.enabled && d.missing().is_none())
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
}

/// File format for screenshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScreenshotFormat {
    /// Every pixel as it was. The default.
    Png,
    /// About a quarter of the size, for sharing.
    Jpg,
}

impl ScreenshotFormat {
    pub fn ext(self) -> &'static str {
        match self {
            ScreenshotFormat::Png => "png",
            ScreenshotFormat::Jpg => "jpg",
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

/// What [`RecordSettings::size_version`] is for settings made now.
pub const SIZE_VERSION: u32 = 1;
/// What [`RecordSettings::framing_version`] is for settings made now.
pub const FRAMING_VERSION: u32 = 1;

/// Output resolution. `Native` records at the capture source's resolution;
/// the p ones bring its short side down to that many pixels on encode (never
/// up: a smaller display stays its size), so 1080p is 1920×1080 widescreen
/// and 1080×1920 upright. `Exact` is that size whatever the display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputResolution {
    Native,
    P480,
    P720,
    P900,
    P1080,
    P1440,
    P2160,
    /// Any short side, in pixels (the other follows the recording's shape).
    Custom(u32),
    /// Exactly this width and height.
    Exact(u32, u32),
}

impl OutputResolution {
    pub fn label(self) -> String {
        match self {
            OutputResolution::Native => "Native".into(),
            OutputResolution::P2160 => "2160p (4K)".into(),
            OutputResolution::Custom(h) => format!("{h}p (custom)"),
            OutputResolution::Exact(w, h) => format!("{w}×{h}"),
            r => format!("{}p", r.short_side().unwrap_or(0)),
        }
    }

    /// What it's good for, next to it in the list.
    pub fn note(self) -> &'static str {
        match self {
            OutputResolution::Native => "every pixel of your display",
            OutputResolution::P2160 => "for 4K displays",
            OutputResolution::P1440 => "sharp, bigger files",
            OutputResolution::P1080 => "sharp, good for sharing",
            OutputResolution::P900 => "",
            OutputResolution::P720 => "small files, quick to upload",
            OutputResolution::P480 => "smallest files",
            OutputResolution::Custom(_) | OutputResolution::Exact(..) => "",
        }
    }

    /// The short side to bring the recording down to, in pixels; `None` for
    /// native (no rescale) and for an exact size.
    pub fn short_side(self) -> Option<u32> {
        match self {
            OutputResolution::Native | OutputResolution::Exact(..) => None,
            OutputResolution::P480 => Some(480),
            OutputResolution::P720 => Some(720),
            OutputResolution::P900 => Some(900),
            OutputResolution::P1080 => Some(1080),
            OutputResolution::P1440 => Some(1440),
            OutputResolution::P2160 => Some(2160),
            OutputResolution::Custom(h) => Some(h),
        }
    }

    /// The exact size asked for, if it's one.
    pub fn exact(self) -> Option<(u32, u32)> {
        match self {
            OutputResolution::Exact(w, h) => Some((w, h)),
            _ => None,
        }
    }

    /// The choices in the list, best first (a custom height is set apart).
    pub const PRESETS: [OutputResolution; 7] = [
        OutputResolution::Native,
        OutputResolution::P2160,
        OutputResolution::P1440,
        OutputResolution::P1080,
        OutputResolution::P900,
        OutputResolution::P720,
        OutputResolution::P480,
    ];
}

/// The screen (or games and apps) as a source in the video: whether it's
/// in it, in front of the webcam or behind it, and where it sits: over all
/// of the video, or in a box of its own showing all of it (in its own
/// shape, as cropped), placed on the Sources page's preview.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScreenCfg {
    /// In the video (its switch). Off, the webcam shows on black.
    pub enabled: bool,
    /// In front of the webcam, rather than behind it.
    pub in_front: bool,
    /// Placed by hand: its box. `None`: it fills the video (its middle,
    /// with a shape of the video's own).
    pub placed: Option<BoxCfg>,
    /// Quarter turns clockwise, then mirrored or flipped upside down, as the
    /// webcam's: see `capture::ScreenPlacement`.
    pub turns: u8,
    pub flip_h: bool,
    pub flip_v: bool,
}

impl Default for ScreenCfg {
    fn default() -> Self {
        Self { enabled: true, in_front: false, placed: None, turns: 0, flip_h: false, flip_v: false }
    }
}

/// The video's size as it's chosen, saved apart from `resolution` and
/// `shape` themselves, which are saved as older versions read them (see
/// [`RecordSettings::to_json`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizeCfg {
    pub resolution: OutputResolution,
    pub shape: Option<[u32; 2]>,
}

/// A box in the video: left, top, width, height as fractions of the video
/// (it can reach past the video's edges: a screen bigger than it), the
/// shape (width / height) of the video it was laid out in, to keep its own
/// shape in a video of another, and of the picture it was laid out for: a
/// box of another shape stretches it (see [`BoxCfg::refit`]).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BoxCfg {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub frame_aspect: f32,
    /// `None` in a box from before boxes could stretch: it has the picture's shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picture_aspect: Option<f32>,
}

/// What the video shows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CaptureTarget {
    /// The display chosen by `display_id`.
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

/// One of the video's sources, as its order names it: the screen (or games
/// and apps), a webcam, another display (by their own `key`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayerKey {
    Screen,
    Webcam(u64),
    Visual(u64),
}

/// What a source in a box of its own shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Visual {
    /// Another display, all of it (`capture::Device::id`; `name` while it
    /// isn't connected).
    Display { id: String, name: String },
    /// One app's main window, by executable.
    Window { app: String, name: String },
    /// An image file (its transparency kept).
    Image { path: PathBuf },
    /// All of its box one colour (R, G, B, A).
    Colour { rgba: [u8; 4] },
    /// Words, drawn as their style says.
    Text(TextStyle),
    /// A web page (alerts, a chat overlay), drawn `width`×`height` big,
    /// with `css` added to it. `reload` goes up to load it again.
    Browser {
        url: String,
        width: u32,
        height: u32,
        #[serde(default, skip_serializing)]
        reload: u32,
        /// A style sheet of its own, added to every page it loads.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        css: String,
    },
    /// A video file: from the start again when it ends (`looped`; else its
    /// last picture stays), and each time it's shown again (`restart`; else
    /// it goes on where it was); its sound in the clip (`sound`: a source of
    /// its own in the sound's list, its volume there).
    Video {
        path: PathBuf,
        #[serde(default = "yes")]
        looped: bool,
        #[serde(default = "yes")]
        restart: bool,
        #[serde(default = "yes")]
        sound: bool,
    },
}

/// How a line of words lines up with the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextAlign {
    Left,
    #[default]
    Centre,
    Right,
}

/// Words in the video, and how they look: a line for each line, in a font
/// installed on the computer (or the app's own), `size` pixels tall in a
/// 1080p video, with an outline round the letters and a shadow under them
/// if wanted. All colours are R, G, B, A.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TextStyle {
    pub text: String,
    /// A font family's name; empty: the app's own (Hanken Grotesk).
    pub font: String,
    /// 100 (thin) to 900 (black); 400 is regular, 700 bold.
    pub weight: u16,
    pub italic: bool,
    pub size: u32,
    pub rgba: [u8; 4],
    pub align: TextAlign,
    /// Between lines, in percent of the font's own spacing.
    pub line_spacing: u32,
    pub outline: bool,
    /// How thick the outline is, in pixels at 1080p.
    pub outline_width: u32,
    pub outline_rgba: [u8; 4],
    pub shadow: bool,
    /// How far the shadow falls, right and down, in pixels at 1080p.
    pub shadow_x: i32,
    pub shadow_y: i32,
    /// How soft its edge is, in pixels at 1080p.
    pub shadow_blur: u32,
    pub shadow_rgba: [u8; 4],
}

impl Default for TextStyle {
    fn default() -> Self {
        Self {
            text: "Your text".to_owned(),
            font: String::new(),
            weight: 700,
            italic: false,
            size: 72,
            rgba: [255, 255, 255, 255],
            align: TextAlign::Centre,
            line_spacing: 100,
            outline: false,
            outline_width: 4,
            outline_rgba: [0, 0, 0, 255],
            shadow: true,
            shadow_x: 3,
            shadow_y: 4,
            shadow_blur: 6,
            shadow_rgba: [0, 0, 0, 160],
        }
    }
}

impl Visual {
    /// What it's called on its card.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Visual::Display { .. } => "Display",
            Visual::Window { .. } => "Window",
            Visual::Image { .. } => "Image",
            Visual::Colour { .. } => "Colour",
            Visual::Text { .. } => "Text",
            Visual::Browser { .. } => "Web page",
            Visual::Video { .. } => "Video",
        }
    }

    /// Whether its box takes any shape (a colour has none of its own).
    pub fn shapeless(&self) -> bool {
        matches!(self, Visual::Colour { .. })
    }

    /// Whether part of its picture can be cut off (not a colour or words:
    /// their box is all there is).
    pub fn croppable(&self) -> bool {
        !matches!(self, Visual::Colour { .. } | Visual::Text(_))
    }
}

/// A source in the video in a box of its own, beside the screen: another
/// display, an app's window, an image, a colour, words. All of it, in its
/// box (or the video's middle, fitted, until it's placed; a colour all of
/// the video), turned and flipped. On the platforms that draw more than one
/// source (`capture::layers::MANY`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VisualCfg {
    /// Its own, for the video's order; never another source's.
    pub key: u64,
    /// What you called it, if you did (else its kind's name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(flatten)]
    pub visual: Visual,
    /// In the clips. Off keeps its place for later.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Placed by hand: its box. `None`: fitted in the middle of the video.
    #[serde(default)]
    pub placed: Option<BoxCfg>,
    #[serde(default)]
    pub turns: u8,
    #[serde(default)]
    pub flip_h: bool,
    #[serde(default)]
    pub flip_v: bool,
    /// The part of its picture shown, in its own pixels, if not all of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crop: Option<ScreenCrop>,
}

impl VisualCfg {
    /// `visual`, on, where it goes at first.
    pub fn new(key: u64, visual: Visual) -> Self {
        Self { key, title: None, visual, enabled: true, placed: None, turns: 0, flip_h: false, flip_v: false, crop: None }
    }
}

// --- Scenes and presets ---

/// A scene: which of the preset's video sources it shows, the one in front
/// first, each placed as it is there (see [`SceneItem`]). The sources are
/// the preset's: a webcam set up once shows in every scene it's put in,
/// placed its own way in each. The scene shown keeps its placements in the
/// live fields (`layer_order`, the screen's, each webcam's and source's), so
/// its own `items` are empty.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SceneCfg {
    pub id: u64,
    pub name: String,
    #[serde(default, deserialize_with = "lenient_list")]
    pub items: Vec<SceneItem>,
    /// Its sources locked: they can't be moved on the preview, and clicks
    /// go through them. Empty for the scene shown (see `locked`).
    #[serde(default, skip_serializing_if = "Vec::is_empty", deserialize_with = "lenient_list")]
    pub locked: Vec<LayerKey>,
    /// Its groups. Empty for the scene shown (see `groups`).
    #[serde(default, skip_serializing_if = "Vec::is_empty", deserialize_with = "lenient_list")]
    pub groups: Vec<GroupCfg>,
}

impl SceneCfg {
    pub fn new(id: u64, name: &str) -> Self {
        Self { id, name: name.to_owned(), items: Vec::new(), locked: Vec::new(), groups: Vec::new() }
    }
}

/// Sources of a scene kept together, as one: next to each other in its
/// order, moved, resized, hidden and locked together, folded away in its
/// list. Each scene's own, as where its sources are.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupCfg {
    pub id: u64,
    pub name: String,
    /// Front first, as they are in the scene's order.
    #[serde(default, deserialize_with = "lenient_list")]
    pub members: Vec<LayerKey>,
    /// Folded in the list: only its own row shows.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub collapsed: bool,
}

/// One of a stored scene's sources, and how it's placed there: what of the
/// screen's, a webcam's or a source's settings goes with the scene (its
/// switch, box, turns, flips and crop). The rest is the source's own, the
/// same in every scene.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum SceneItem {
    Screen {
        #[serde(default = "yes")]
        enabled: bool,
        #[serde(default)]
        placed: Option<BoxCfg>,
        #[serde(default)]
        turns: u8,
        #[serde(default)]
        flip_h: bool,
        #[serde(default)]
        flip_v: bool,
        #[serde(default = "filled")]
        framing: Framing,
        #[serde(default)]
        crop: Option<ScreenCrop>,
    },
    Webcam {
        key: u64,
        #[serde(default = "yes")]
        enabled: bool,
        placement: PlacementCfg,
    },
    Visual {
        key: u64,
        #[serde(default = "yes")]
        enabled: bool,
        #[serde(default)]
        placed: Option<BoxCfg>,
        #[serde(default)]
        turns: u8,
        #[serde(default)]
        flip_h: bool,
        #[serde(default)]
        flip_v: bool,
        #[serde(default)]
        crop: Option<ScreenCrop>,
    },
}

impl SceneItem {
    /// The source it places.
    pub fn layer(&self) -> LayerKey {
        match self {
            SceneItem::Screen { .. } => LayerKey::Screen,
            SceneItem::Webcam { key, .. } => LayerKey::Webcam(*key),
            SceneItem::Visual { key, .. } => LayerKey::Visual(*key),
        }
    }

    /// A webcam's or a source's key.
    fn key(&self) -> Option<u64> {
        match self {
            SceneItem::Screen { .. } => None,
            SceneItem::Webcam { key, .. } | SceneItem::Visual { key, .. } => Some(*key),
        }
    }

    /// The screen filling the video: what a new scene starts with (`on`),
    /// or a scene found without it gets (hidden: it never had it showing).
    fn screen(on: bool) -> Self {
        SceneItem::Screen { enabled: on, placed: None, turns: 0, flip_h: false, flip_v: false, framing: Framing::Fit, crop: None }
    }
}

/// A setup for a task (streaming, plain clipping): its own video sources,
/// scenes and sound. The one in use keeps its setup in the live fields, so
/// its own `setup` is `None`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetCfg {
    pub id: u64,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<Box<PresetSetup>>,
}

/// What a preset holds, as the live fields hold it for the one in use (see
/// [`RecordSettings::take_setup`]): what the screen records and what it's
/// called, the webcams and other sources, the scene shown (in the screen's,
/// the sources' and `layer_order`'s placements) and the others, and the
/// sound.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PresetSetup {
    pub capture: CaptureTarget,
    pub idle_apps: Option<CaptureTarget>,
    pub display_id: Option<String>,
    pub display_index: usize,
    pub screen: ScreenCfg,
    pub screen_title: Option<String>,
    pub framing: Framing,
    pub screen_crop: Option<ScreenCrop>,
    pub webcams: Vec<WebcamCfg>,
    pub visuals: Vec<VisualCfg>,
    pub layer_order: Vec<LayerKey>,
    #[serde(deserialize_with = "lenient_list")]
    pub locked: Vec<LayerKey>,
    #[serde(deserialize_with = "lenient_list")]
    pub groups: Vec<GroupCfg>,
    #[serde(deserialize_with = "lenient_list")]
    pub scenes: Vec<SceneCfg>,
    pub active_scene: u64,
    pub audio_sources: Vec<AudioSourceCfg>,
    pub limiter: bool,
}

impl Default for PresetSetup {
    /// The screen filling the video, with your mic and your computer's
    /// sound, in one scene.
    fn default() -> Self {
        RecordSettings::default().take_setup()
    }
}

impl PresetSetup {
    /// Every source it has: the screen, each webcam, each other.
    fn pool(&self) -> Vec<LayerKey> {
        std::iter::once(LayerKey::Screen).chain(self.webcams.iter().map(|w| LayerKey::Webcam(w.key))).chain(self.visuals.iter().map(|v| LayerKey::Visual(v.key))).collect()
    }
}

/// A list read one element at a time: one this version can't read (written
/// by a newer one, or damaged) is left out, rather than every setting going
/// back to its default.
fn lenient_list<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::Array(items) => items.into_iter().filter_map(|i| serde_json::from_value(i).ok()).collect(),
        _ => Vec::new(),
    })
}

/// Sources copied (Ctrl+C) or cut on Sources, each as it was then, to
/// paste (Ctrl+V) into another scene or another preset's: front first.
#[derive(Debug, Clone, PartialEq)]
pub struct CopiedSources {
    pub sources: Vec<CopiedSource>,
    /// The group they were, by its name, when a group was copied.
    pub group: Option<String>,
}

/// One source copied: the screen as it was placed (each preset has its
/// own: pasted, that one's placed so), a webcam or another source with all
/// its settings, placed as it was.
#[derive(Debug, Clone, PartialEq)]
pub enum CopiedSource {
    Screen(SceneItem),
    Webcam(WebcamCfg),
    Visual(VisualCfg),
}

/// How a paste went: the sources put in the scene, and those that
/// weren't (by name): ones it has already (a camera's in a scene once, as
/// the screen is), webcams where a scene shows one and it has one.
#[derive(Debug, Default, PartialEq)]
pub struct Pasted {
    pub layers: Vec<LayerKey>,
    pub there: Vec<String>,
    pub full: Vec<String>,
    /// The group made of them, when a group was pasted.
    pub group: Option<u64>,
}

/// `name`, or with a number after it (2, 3, …) if `taken` says it's used.
fn unused_name(name: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(name) {
        return name.to_owned();
    }
    (2u32..).map(|i| format!("{name} {i}")).find(|n| !taken(n)).unwrap_or_else(|| name.to_owned())
}

/// A preset's scenes as they should be (see `upgrade_scenes`), given the
/// sources it has (`pool`): one at least, ids each once, the scene shown
/// there with its items in the live fields (none of its own), the others'
/// items only sources there are, each once, and the screen in each.
fn check_scenes(scenes: &mut Vec<SceneCfg>, active: &mut u64, pool: &[LayerKey]) {
    if scenes.is_empty() {
        scenes.push(SceneCfg::new(1, "Scene 1"));
    }
    let mut next = scenes.iter().map(|s| s.id).max().unwrap_or(0) + 1;
    let mut seen: Vec<u64> = Vec::new();
    for s in scenes.iter_mut() {
        if s.id == 0 || seen.contains(&s.id) {
            s.id = next;
            next += 1;
        }
        seen.push(s.id);
    }
    if !scenes.iter().any(|s| s.id == *active) {
        *active = scenes[0].id;
    }
    for s in scenes.iter_mut() {
        if s.id == *active {
            s.items.clear();
            continue;
        }
        let mut items: Vec<SceneItem> = Vec::new();
        for i in std::mem::take(&mut s.items) {
            if pool.contains(&i.layer()) && !items.iter().any(|x| x.layer() == i.layer()) {
                items.push(i);
            }
        }
        s.items = items;
    }
}

/// A webcam drawn over the recording.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebcamCfg {
    /// Its own, for the video's order; never another webcam's. 0 in one
    /// from before there could be several (given one on load).
    #[serde(default)]
    pub key: u64,
    /// Backend id (`capture::webcam::list_cameras`).
    pub id: String,
    /// What to call it while it isn't connected.
    pub name: String,
    /// What you called it, if you did (else "Webcam").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
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
    /// How stretched its picture is (Shift on the preview): its box's shape
    /// over what's shown of the camera's. 1, not at all; kept through
    /// another crop, camera or video shape.
    #[serde(default = "unstretched", skip_serializing_if = "is_unstretched")]
    pub stretch: f32,
}

fn unstretched() -> f32 {
    1.0
}

fn is_unstretched(s: &f32) -> bool {
    (s - 1.0).abs() < 1e-6
}

impl PlacementCfg {
    /// Keep the box right for a frame of shape `frame_aspect`: rescaled to
    /// keep its shape in pixels, as wide a share of the frame (narrower if
    /// that won't fit), at the same place in the room it has to move in (so
    /// a box in a corner stays in it). Its shape is what's left of the
    /// camera's picture once cropped (as stretched as it's been), as every
    /// box is made: given the camera's shape (`camera_aspect`, as turned,
    /// when known), a box of
    /// another shape gets it back, one laid out before the frame's shape was
    /// kept or squashed by a frame that changed under it. Returns whether it
    /// changed.
    pub fn refit(&mut self, frame_aspect: f32, camera_aspect: Option<f32>) -> bool {
        if frame_aspect <= 0.0 || self.w <= 0.0 || self.h <= 0.0 {
            return false;
        }
        let wanted = camera_aspect.filter(|&cam| cam > 0.0).map(|cam| {
            cam * (1.0 - self.crop[0] - self.crop[2]).max(0.05) / (1.0 - self.crop[1] - self.crop[3]).max(0.05) * self.stretch
        });
        // Its shape in pixels, in the frame it was made for.
        let had = self.frame_aspect.map(|was| self.w * was / self.h);
        let same_frame = self.frame_aspect.is_some_and(|was| (was - frame_aspect).abs() < 1e-3);
        let box_aspect = match (had, wanted) {
            // Off by more than the preview's rounding.
            (Some(had), Some(wanted)) if (had / wanted - 1.0).abs() > 0.02 => wanted,
            (Some(_), _) if same_frame => return false,
            (Some(had), _) => had,
            (None, Some(wanted)) => wanted,
            (None, None) => return false, // wait for the camera's shape
        };
        [self.x, self.y, self.w, self.h] = refit_box([self.x, self.y, self.w, self.h], box_aspect, frame_aspect);
        self.frame_aspect = Some(frame_aspect);
        true
    }
}

/// A box (left, top, width, height as fractions of the frame) of shape
/// `box_aspect` in pixels, in a frame of shape `frame_aspect`: as wide a
/// share of the frame (narrower if that won't fit, for a box that was in
/// it), at the same place in the room it has to move in (so a box in a
/// corner stays in it, and one bigger than the frame covers as much of it).
fn refit_box([x, y, w, h]: [f32; 4], box_aspect: f32, frame_aspect: f32) -> [f32; 4] {
    // From 0 against the left or top edge to 1 against the other.
    let along = |at: f32, size: f32| if (1.0 - size).abs() > 1e-4 { (at / (1.0 - size)).clamp(0.0, 1.0) } else { 0.5 };
    let (ax, ay) = (along(x, w), along(y, h));
    let inside = w <= 1.0 + 1e-4 && h <= 1.0 + 1e-4;
    let mut w = if inside { w.min(1.0) } else { w };
    let mut h = w * frame_aspect / box_aspect;
    if inside && h > 1.0 {
        w *= 1.0 / h;
        h = 1.0;
    }
    [ax * (1.0 - w), ay * (1.0 - h), w, h]
}

impl BoxCfg {
    /// `rect` (left, top, width, height as fractions of the frame), laid out
    /// in a frame of shape `frame_aspect` for a picture of shape
    /// `picture_aspect`, when it's known.
    pub fn new([x, y, w, h]: [f32; 4], frame_aspect: f32, picture_aspect: Option<f32>) -> Self {
        Self { x, y, w, h, frame_aspect, picture_aspect }
    }

    pub fn rect(&self) -> [f32; 4] {
        [self.x, self.y, self.w, self.h]
    }

    /// Keep it right for a frame of shape `frame_aspect`: its shape in
    /// pixels kept, at the same place (see [`refit_box`]). For a picture of
    /// another shape (`picture_aspect`: another display, another crop) it
    /// gets the picture's, as stretched as it was, keeping its width and
    /// place; a box from before boxes could stretch, the picture's own.
    /// Returns whether it changed.
    pub fn refit(&mut self, frame_aspect: f32, picture_aspect: Option<f32>) -> bool {
        if frame_aspect <= 0.0 || self.w <= 0.0 || self.h <= 0.0 {
            return false;
        }
        let had = self.w * self.frame_aspect / self.h;
        let now = picture_aspect.filter(|&a| a > 0.0);
        // Off by more than rounding.
        let off = |a: f32, b: f32| (a / b - 1.0).abs() > 0.02;
        let box_aspect = match (now, self.picture_aspect) {
            (Some(now), Some(was)) if off(now, was) => had * now / was,
            (Some(now), None) if off(had, now) => now,
            _ if (self.frame_aspect - frame_aspect).abs() < 1e-3 => {
                let known = self.picture_aspect.is_some() || now.is_none();
                self.picture_aspect = self.picture_aspect.or(now);
                return !known;
            }
            _ => had,
        };
        [self.x, self.y, self.w, self.h] = refit_box(self.rect(), box_aspect, frame_aspect);
        self.frame_aspect = frame_aspect;
        if now.is_some_and(|now| self.picture_aspect.is_none_or(|was| off(now, was))) {
            self.picture_aspect = now;
        }
        true
    }

    /// Whether it's another shape than its picture, by more than rounding:
    /// stretched on purpose (Shift on the preview).
    pub fn stretched(&self) -> bool {
        let shape = self.w * self.frame_aspect / self.h.max(1e-6);
        self.picture_aspect.is_some_and(|p| p > 0.0 && (shape / p - 1.0).abs() > 0.02)
    }
}

impl From<PlacementCfg> for capture::webcam::Placement {
    fn from(p: PlacementCfg) -> Self {
        Self { x: p.x, y: p.y, w: p.w, h: p.h, crop: p.crop, flip_h: p.flip_h, flip_v: p.flip_v, turns: p.turns, stretch: p.stretch }
    }
}

impl PlacementCfg {
    /// `p`, laid out in a frame of shape `frame_aspect`.
    pub fn laid_out(p: capture::webcam::Placement, frame_aspect: f32) -> Self {
        Self { x: p.x, y: p.y, w: p.w, h: p.h, crop: p.crop, flip_h: p.flip_h, flip_v: p.flip_v, turns: p.turns, frame_aspect: Some(frame_aspect), stretch: p.stretch }
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
    /// A video source's sound (the source `key`): there while the video is,
    /// and its sound's on (see `App::sync_video_sounds`).
    Video { key: u64 },
}

/// The order the library shows clips in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum LibrarySort {
    #[default]
    Newest,
    Oldest,
    Longest,
    Largest,
}

impl LibrarySort {
    pub const ALL: [LibrarySort; 4] = [LibrarySort::Newest, LibrarySort::Oldest, LibrarySort::Longest, LibrarySort::Largest];

    pub fn label(self) -> &'static str {
        match self {
            LibrarySort::Newest => "Newest first",
            LibrarySort::Oldest => "Oldest first",
            LibrarySort::Longest => "Longest first",
            LibrarySort::Largest => "Largest first",
        }
    }

    /// Grouped by day (newest or oldest first), or one list.
    pub fn by_day(self) -> bool {
        matches!(self, LibrarySort::Newest | LibrarySort::Oldest)
    }
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
    /// How much of a microphone's background noise is taken out. Off unless
    /// chosen.
    #[serde(default)]
    pub noise_reduction: NoiseReduction,
    /// A microphone heard in the middle, its two sides made one: for one
    /// heard on one side only. Off unless turned on.
    #[serde(default)]
    pub mono: bool,
    /// From before there were two: on, which is Strong now. Read, not written.
    #[serde(default, skip_serializing, rename = "noise_removal")]
    old_noise_removal: bool,
}

/// How the looks in a blended frame are weighed (see `capture::BlendShape`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlendShape {
    /// Each look in the frame's own time the same.
    #[default]
    Even,
    /// The middle of the frame's time most, fading into the frames either side.
    Soft,
}

impl From<BlendShape> for capture::BlendShape {
    fn from(b: BlendShape) -> Self {
        match b {
            BlendShape::Even => Self::Even,
            BlendShape::Soft => Self::Soft,
        }
    }
}

/// How a whole display is captured on Windows (see `capture::ScreenCapture`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenCapture {
    /// Desktop Duplication on Windows 10 (no yellow border), Windows Graphics
    /// Capture on Windows 11.
    #[default]
    Auto,
    Duplication,
    Wgc,
}

impl ScreenCapture {
    pub const ALL: [ScreenCapture; 3] = [ScreenCapture::Auto, ScreenCapture::Duplication, ScreenCapture::Wgc];

    pub fn label(self) -> &'static str {
        match self {
            ScreenCapture::Auto => "Automatic",
            ScreenCapture::Duplication => "Desktop Duplication",
            ScreenCapture::Wgc => "Windows Graphics Capture",
        }
    }
}

impl From<ScreenCapture> for capture::ScreenCapture {
    fn from(m: ScreenCapture) -> Self {
        match m {
            ScreenCapture::Auto => capture::ScreenCapture::Auto,
            ScreenCapture::Duplication => capture::ScreenCapture::Duplication,
            ScreenCapture::Wgc => capture::ScreenCapture::Wgc,
        }
    }
}

/// How the screen goes into a recording of another shape (see
/// `capture::Framing`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Framing {
    /// All of it, with black bars.
    #[default]
    Fit,
    /// Its middle, the edges past the shape cut off.
    Fill,
    /// All of it, stretched or squeezed.
    Stretch,
}

impl From<Framing> for capture::Framing {
    fn from(f: Framing) -> Self {
        match f {
            Framing::Fill => Self::Fill,
            Framing::Fit => Self::Fit,
            Framing::Stretch => Self::Stretch,
        }
    }
}

/// How much of a microphone's background noise is taken out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoiseReduction {
    #[default]
    Off,
    /// Steady noise (fans, hum, hiss): a keyboard stays.
    Light,
    /// Everything but a voice, a keyboard too.
    Strong,
}

impl From<NoiseReduction> for capture::mixer::NoiseReduction {
    fn from(n: NoiseReduction) -> Self {
        match n {
            NoiseReduction::Off => Self::Off,
            NoiseReduction::Light => Self::Light,
            NoiseReduction::Strong => Self::Strong,
        }
    }
}

fn yes() -> bool {
    true
}

/// How the screen went into a shape of the video's own before Fit, Fill
/// and Stretch could be chosen.
fn filled() -> Framing {
    Framing::Fill
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
            noise_reduction: NoiseReduction::Off,
            mono: false,
            old_noise_removal: false,
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

/// The part of the screen kept, in its pixels: `w`×`h`, its top-left
/// corner at `at`, or in the middle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ScreenCrop {
    pub w: u32,
    pub h: u32,
    pub at: Option<[u32; 2]>,
}

impl ScreenCrop {
    /// No size: none of it cut off.
    pub fn is_none(&self) -> bool {
        self.w == 0 || self.h == 0
    }
}

impl From<ScreenCrop> for capture::Crop {
    fn from(c: ScreenCrop) -> Self {
        capture::Crop { w: c.w, h: c.h, at: c.at.map(|[x, y]| (x, y)) }
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

/// Persisted to `<config_dir>/hasteclip/settings.json`. `#[serde(default)]` lets
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
    /// The first webcam, as older versions read it (they had one): written
    /// on save, moved into `webcams` on load, `None` in between.
    pub webcam: Option<WebcamCfg>,
    /// The webcams over the picture (or under it).
    pub webcams: Vec<WebcamCfg>,
    /// Other displays in the video, besides the screen.
    pub visuals: Vec<VisualCfg>,
    /// The video's sources, the one in front first (see `layer_order`).
    /// Empty in settings from before it was kept: the screen's `in_front`
    /// says then.
    pub layer_order: Vec<LayerKey>,
    /// The scene shown's sources locked: they can't be moved on the
    /// preview, and clicks go through them to what's under them.
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "lenient_list")]
    pub locked: Vec<LayerKey>,
    /// The scene shown's groups (see [`GroupCfg`]).
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "lenient_list")]
    pub groups: Vec<GroupCfg>,
    /// The screen as a source in the video: in front or behind, its box.
    pub screen: ScreenCfg,
    /// What you called the screen, if you did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_title: Option<String>,
    /// The preset in use's scenes, in order. The one shown (`active_scene`)
    /// keeps its placements in the live fields above: its own items are
    /// empty. None in settings from before scenes (see `upgrade_scenes`).
    #[serde(default, deserialize_with = "lenient_list")]
    pub scenes: Vec<SceneCfg>,
    pub active_scene: u64,
    /// Every preset, in order. The one in use (`active_preset`) keeps its
    /// setup in the live fields (the sources, scenes and sound): its own
    /// is `None`.
    #[serde(default, deserialize_with = "lenient_list")]
    pub presets: Vec<PresetCfg>,
    pub active_preset: u64,
    /// Each camera's settings changed from its defaults (control id, as
    /// V4L2 numbers them, to value), by camera id: put back each time it's
    /// opened, as cameras forget them when unplugged.
    pub camera_controls: std::collections::BTreeMap<String, std::collections::BTreeMap<u32, i64>>,
    /// The chosen display's id (`capture::Device::id`), so the choice stays
    /// with that display when others are plugged in or listed in another
    /// order. The main display when it isn't connected.
    pub display_id: Option<String>,
    /// Where the chosen display was in the list: what settings from before
    /// `display_id` have, and still written for them.
    pub display_index: usize,
    pub fps: u32,
    /// Live frame blending: look at the screen this many times a second and
    /// record the average of the looks in each frame (a whole multiple of
    /// `fps`), for smoother, motion-blurred video. 0 is off.
    pub blend_fps: u32,
    /// How the looks in a blended frame are weighed.
    pub blend_shape: BlendShape,
    /// The video's size: Native, in p on its short side, or a width and
    /// height of its own. Saved as older versions read it (p was the
    /// height, there was no size of its own), the size itself in
    /// `saved_size`.
    pub resolution: OutputResolution,
    /// What `resolution`'s p means in a file without `saved_size`: 0 from
    /// before it was the short side (it was the height), 1 in one saved by
    /// a version that had it be the short side. Read once, on load.
    #[serde(default, skip_serializing)]
    pub size_version: u32,
    /// The shape recordings are made in, width to height, when it isn't the
    /// screen's (16:9 out of an ultrawide screen). Saved as `resolution` is.
    pub shape: Option<[u32; 2]>,
    /// `resolution` and `shape` as they are, in the file (see `to_json`):
    /// read instead of them when it's there. Older versions don't know it,
    /// and leave it out when they save.
    #[serde(rename = "video_size", skip_serializing_if = "Option::is_none")]
    saved_size: Option<SizeCfg>,
    /// How the screen goes into the video while it isn't placed by hand,
    /// when they aren't the same shape: all of it with bars, its middle
    /// covering it, or all of it stretched. In a video of a shape of its
    /// own (an aspect ratio, or a size of its own), and in the screen's own
    /// shape too: a display's resolution can change while it's recorded (a
    /// game at a stretched resolution), and a game's window needn't be the
    /// screen's shape. A file from before it could be chosen says nothing:
    /// it's Fill, as the screen went in then.
    #[serde(default = "filled")]
    pub framing: Framing,
    /// What `framing` covers in a file: 0 from before it was used in the
    /// screen's own shape (it was always Fit there), 1 since. Read once,
    /// on load (see `upgrade_framing`).
    #[serde(default)]
    pub framing_version: u32,
    /// The part of the screen kept (a game at a lower resolution than the
    /// display, black around it): set on Sources. It's recorded as a display
    /// that size would be.
    pub screen_crop: Option<ScreenCrop>,
    pub video_bitrate_mbps: u32,

    pub replay_seconds: u32,
    /// Start the replay buffer as soon as the app opens, so you never miss a moment.
    pub auto_start_buffer: bool,
    /// Record a game on the games and apps list the whole time it's open
    /// (see `sessions.rs`).
    pub record_sessions: bool,
    /// Download new versions in the background and install them on quit.
    pub auto_update: bool,
    /// Show what you're clipping on your Discord profile (Rich Presence).
    pub discord_presence: bool,
    /// Going live: where to, and how.
    pub stream: StreamCfg,
    /// Saying "HasteClip that" saves a clip (while the replay buffer runs).
    pub voice_clip: bool,
    /// Keep short snippets of the microphone around what the voice detector fires on or
    /// nearly fires on, to listen through and label (never sent anywhere by itself).
    pub voice_samples: bool,

    // --- Advanced (collapsible; sane defaults) ---
    pub encoder: Encoder,
    pub container: Container,
    pub rate_control: RateControl,
    pub output_dir: PathBuf,
    /// Save each clip in a folder named after the game it's of (`osu!/`,
    /// `Desktop/` when there's no game), shown as categories in the library.
    pub folder_per_game: bool,
    /// Game folders renamed in the library: the folder a game's clips would
    /// go in (its name, as `clips::folder_name` makes it) → the one they go in.
    pub game_folders: std::collections::BTreeMap<String, String>,
    /// Light, dark, or the system's.
    pub theme: Theme,
    /// Closing the window keeps HasteClip running in the tray (where
    /// there is one) instead of quitting.
    pub close_to_tray: bool,
    /// The first close asked which (then it just does it).
    pub close_asked: bool,
    /// macOS: which time the permissions screen was last shown at launch
    /// ([`crate::setup::SETUP_ROUND`]): it's shown again, once, when a
    /// release needs everyone to allow them again.
    pub setup_shown: u32,
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
    /// How a whole display is captured (Windows).
    pub screen_capture: ScreenCapture,

    // --- Screenshots ---
    pub screenshot_format: ScreenshotFormat,
    /// A screenshot also goes on the clipboard, to paste right away.
    pub copy_screenshots: bool,

    // --- Shortcuts ---
    pub shortcuts: Shortcuts,
    /// The sound played when a clip (or a screenshot) is saved.
    pub save_sound: SaveSound,
    /// The editor's preview height, as a share of the window (the timeline
    /// gets the rest), as last dragged; none: fitted, the timeline as tall
    /// as its lanes need. (It was `editor_preview_share`, from a divider
    /// that barely moved: what that says isn't anyone's choice.)
    pub editor_picture_share: Option<f32>,
    /// The editor's snapping (the magnet, N): trim edges and the playhead
    /// snap to each other, the clip's ends and its moments. Off unless you
    /// turn it on (it caught on kills while trimming), then kept as you left it.
    pub editor_snap: bool,
    /// The clip player's volume (0..=1 on its slider) and mute, as last set,
    /// so a clip never opens at full blast after you'd turned it down.
    pub player_volume: f32,
    pub player_muted: bool,
    /// The library's order.
    pub library_sort: LibrarySort,
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
            webcams: Vec::new(),
            visuals: Vec::new(),
            layer_order: Vec::new(),
            locked: Vec::new(),
            groups: Vec::new(),
            screen: ScreenCfg::default(),
            screen_title: None,
            scenes: vec![SceneCfg::new(1, "Scene 1")],
            active_scene: 1,
            presets: vec![PresetCfg { id: 1, name: "Default".to_owned(), setup: None }],
            active_preset: 1,
            camera_controls: Default::default(),
            display_id: None,
            display_index: 0,
            fps: 60,
            blend_fps: 0,
            blend_shape: BlendShape::Even,
            resolution: OutputResolution::Native,
            size_version: SIZE_VERSION,
            shape: None,
            saved_size: None,
            framing: Framing::Fit,
            framing_version: FRAMING_VERSION,
            screen_crop: None,
            video_bitrate_mbps: 40,

            replay_seconds: 60,
            auto_start_buffer: true,
            record_sessions: false,
            auto_update: true,
            // On to begin with: friends see what you clip, and where to get
            // HasteClip. Settings saved before keep theirs (off).
            discord_presence: true,
            stream: StreamCfg::default(),
            voice_clip: true,
            voice_samples: false,
            encoder: Encoder::Auto,
            container: Container::Mp4,
            rate_control: RateControl::Cbr,
            output_dir: default_output_dir(),
            folder_per_game: true,
            game_folders: Default::default(),
            game_details: true,
            delete_permanently: false,
            library_sort: LibrarySort::Newest,
            theme: Theme::System,
            close_to_tray: true,
            close_asked: false,
            setup_shown: 0,
            game_titles: GameTitles::default(),
            audio_sources: default_sources(),
            limiter: true,
            game_capture: true,
            game_capture_allowed: Vec::new(),
            screen_capture: ScreenCapture::Auto,
            screenshot_format: ScreenshotFormat::Png,
            copy_screenshots: true,
            shortcuts: Shortcuts::default(),
            save_sound: SaveSound::default(),
            editor_picture_share: None,
            editor_snap: false,
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
    /// Sounds you've added, copied into HasteClip' own folder.
    pub custom: Vec<CustomSound>,
    /// Recorded like any other app's sound (in what's recorded after it).
    pub recorded: bool,
}

impl Default for SaveSound {
    fn default() -> Self {
        Self { enabled: true, sound: "horse".into(), volume: 0.5, custom: Vec::new(), recorded: true }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomSound {
    pub name: String,
    pub file: PathBuf,
}

/// The global shortcuts, as `global_hotkey` strings ("alt+F10"). Empty = off.
/// Only Save clip has one to begin with; the rest are off until chosen:
/// pressed by accident, Start or stop clipping turns clipping off without a
/// word (the clip you wanted isn't there), recording fills the disk, and
/// any key taken is one a game or another app may want.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Shortcuts {
    pub save_clip: String,
    pub toggle_buffer: String,
    pub toggle_record: String,
    pub screenshot: String,
}

impl Default for Shortcuts {
    fn default() -> Self {
        Self { save_clip: "alt+F10".into(), toggle_buffer: String::new(), toggle_record: String::new(), screenshot: String::new() }
    }
}

/// One of the actions a global shortcut can trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutAction {
    SaveClip,
    ToggleBuffer,
    ToggleRecord,
    Screenshot,
}

impl ShortcutAction {
    pub const ALL: [ShortcutAction; 4] = [ShortcutAction::SaveClip, ShortcutAction::Screenshot, ShortcutAction::ToggleBuffer, ShortcutAction::ToggleRecord];

    pub fn label(self) -> &'static str {
        match self {
            ShortcutAction::SaveClip => "Save clip",
            ShortcutAction::ToggleBuffer => "Start or stop clipping",
            ShortcutAction::ToggleRecord => "Start or stop recording",
            ShortcutAction::Screenshot => "Take a screenshot",
        }
    }

    /// Whether it does anything here: screenshots aren't on every platform yet.
    pub fn available(self) -> bool {
        self != ShortcutAction::Screenshot || capture::SCREENSHOTS
    }
}

impl Shortcuts {
    pub fn get(&self, a: ShortcutAction) -> &str {
        match a {
            ShortcutAction::SaveClip => &self.save_clip,
            ShortcutAction::ToggleBuffer => &self.toggle_buffer,
            ShortcutAction::ToggleRecord => &self.toggle_record,
            ShortcutAction::Screenshot => &self.screenshot,
        }
    }

    pub fn set(&mut self, a: ShortcutAction, value: String) {
        match a {
            ShortcutAction::SaveClip => self.save_clip = value,
            ShortcutAction::ToggleBuffer => self.toggle_buffer = value,
            ShortcutAction::ToggleRecord => self.toggle_record = value,
            ShortcutAction::Screenshot => self.screenshot = value,
        }
    }
}

/// The blend rates offered for a frame rate: whole multiples of it, up to 16
/// times and [`MAX_BLEND`], or `max` where the system can't look faster
/// (macOS: the display's refresh rate).
pub fn blend_choices(fps: u32, max: Option<u32>) -> Vec<u32> {
    (2..=16).map(|k| fps * k).filter(|&r| r <= max.unwrap_or(MAX_BLEND).min(MAX_BLEND)).collect()
}

/// The fastest blend rate offered: 60 fps from 1920. Measured on an RTX
/// 4060 Ti: the cost follows the game's pictures, not the looks (repeated
/// looks are added in one pass; the game capture's looks copy only new
/// pictures), and looks at 1200/s timed a 600 fps game's pictures exactly.
/// Beyond, a guess.
pub const MAX_BLEND: u32 = 2000;

impl RecordSettings {
    /// Looks at the screen per recorded frame: the blend rate over the frame
    /// rate, 1 (no blending) when off or no longer a multiple above it.
    /// The shape recordings are made in, if it isn't the screen's: an exact
    /// size's own, else the aspect ratio chosen.
    pub fn capture_shape(&self) -> Option<capture::Shape> {
        match self.resolution.exact() {
            Some((w, h)) => (w > 0 && h > 0).then_some(capture::Shape { w, h, framing: capture::Framing::Fill }),
            None => self.aspect_shape(),
        }
    }

    /// The aspect ratio chosen, if it isn't the screen's (an exact size
    /// aside: it's what a resolution in p records in). The screen fills it,
    /// its middle.
    pub fn aspect_shape(&self) -> Option<capture::Shape> {
        self.shape.filter(|[w, h]| *w > 0 && *h > 0).map(|[w, h]| capture::Shape { w, h, framing: capture::Framing::Fill })
    }

    /// The size recordings are made at.
    pub fn video_size(&self) -> capture::VideoSize {
        capture::VideoSize { short_side: self.resolution.short_side(), shape: self.capture_shape(), exact: self.resolution.exact() }
    }

    /// Where the screen's picture goes in the video, for the capture: its
    /// box when it's been placed by hand, all of the box (stretched, if
    /// it's been stretched; a game's window that isn't, whatever its shape,
    /// all of it, as big as fits); else all of the video, as `framing`
    /// says. A display's crop, if it's cropped.
    pub fn screen_placement(&self) -> capture::ScreenPlacement {
        let s = &self.screen;
        let apps = matches!(self.capture, CaptureTarget::Apps { .. });
        // A game's or app's window can change shape: fitted in its box, so
        // it's never squashed, unless the box was stretched on purpose.
        let (rect, framing) = match s.placed {
            Some(b) => ([b.x, b.y, b.w, b.h], if apps && !b.stretched() { capture::Framing::Fit } else { capture::Framing::Stretch }),
            None => ([0.0, 0.0, 1.0, 1.0], self.framing.into()),
        };
        let crop = self.screen_crop.filter(|c| capture::SCREEN_CROP && !apps && !c.is_none()).map(Into::into);
        // A scene without the screen: it's captured on, hidden (switching
        // to one with it shows it at once).
        let hidden = !s.enabled || !self.layer_order().contains(&LayerKey::Screen);
        capture::ScreenPlacement { rect, framing, in_front: self.screen_in_front(), hidden, crop, turns: s.turns % 4, flip_h: s.flip_h, flip_v: s.flip_v }
    }

    pub fn blend_samples(&self) -> u32 {
        if self.blend_fps == 0 { 1 } else { (self.blend_fps as f64 / self.fps.max(1) as f64).round().max(1.0) as u32 }
    }
}

/// A shape `w`:`h` in small numbers, as older versions' Aspect ratio takes
/// them (up to 99 each): 1920×1080 is 16:9, 1366×768 about that.
fn small_ratio(w: u32, h: u32) -> [u32; 2] {
    let gcd = |mut a: u32, mut b: u32| {
        while b != 0 {
            (a, b) = (b, a % b);
        }
        a.max(1)
    };
    let g = gcd(w, h);
    let (w, h) = (w / g, h / g);
    if w.max(h) <= 99 {
        return [w.max(1), h.max(1)];
    }
    // The nearest with both up to 99.
    let ratio = w as f64 / h.max(1) as f64;
    let candidates = (1..=99u32).flat_map(|q| [(((q as f64 * ratio).round() as u32), q), (q, ((q as f64 / ratio).round() as u32))]);
    let ok = candidates.filter(|&(a, b)| (1..=99).contains(&a) && (1..=99).contains(&b));
    let best = ok.min_by(|x, y| ((x.0 as f64 / x.1 as f64 - ratio).abs()).total_cmp(&(y.0 as f64 / y.1 as f64 - ratio).abs()));
    best.map_or([16, 9], |(a, b)| {
        let g = gcd(a, b);
        [a / g, b / g]
    })
}

/// Common frame rates for the picker; those above the display's refresh
/// rate (and 240) aren't offered.
pub const FPS_CHOICES: [u32; 9] = [30, 60, 120, 144, 165, 240, 360, 480, 540];

/// The frame rate presets for a display refreshing `hz` times a second
/// (None: not known): up to it (and at least up to 240), and it itself.
pub fn fps_choices(hz: Option<u32>) -> Vec<u32> {
    let top = hz.unwrap_or(240).max(240);
    let mut out: Vec<u32> = FPS_CHOICES.iter().copied().filter(|&f| f <= top).collect();
    if let Some(hz) = hz.filter(|hz| (10..=MAX_FPS).contains(hz) && !out.contains(hz)) {
        out.push(hz);
        out.sort_unstable();
    }
    out
}

/// The highest frame rate that can be typed. Measured: an RTX 4060 Ti's
/// encoder takes 1080p at up to ~540 fps (2 ms a frame, one at a time).
pub const MAX_FPS: u32 = 600;

/// `~/Movies/hasteclip` on macOS, the platform video dir elsewhere, falling back
/// to the current dir if none is known.
pub fn default_output_dir() -> PathBuf {
    dirs::video_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("hasteclip")
}

fn settings_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("hasteclip").join("settings.json"))
}

/// Dev aid: `HASTECLIP_SETTINGS=<file>` loads the settings from that file
/// instead (to check a setup without touching the real one; dev-hook
/// launches never save).
fn load_path() -> Option<PathBuf> {
    std::env::var_os("HASTECLIP_SETTINGS").map(PathBuf::from).or_else(settings_path)
}

impl RecordSettings {
    /// Last saved settings, or defaults on first run / unreadable file.
    pub fn load() -> Self {
        Self::from_saved(load_path().and_then(|p| std::fs::read(p).ok()).as_deref())
    }

    /// Settings from a saved file (defaults without one), brought up to date.
    fn from_saved(bytes: Option<&[u8]>) -> Self {
        let mut s: Self = bytes.and_then(Self::from_json).unwrap_or_default();
        s.upgrade_capture();
        s.upgrade_webcams();
        s.upgrade_scenes();
        s.upgrade_save_sound();
        s.upgrade_noise_reduction();
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

    /// The one webcam of settings from before there could be several
    /// becomes the first of the list, in front of the screen or behind it
    /// as it was; every webcam and display gets a key of its own.
    fn upgrade_webcams(&mut self) {
        if let Some(w) = self.webcam.take()
            && self.webcams.is_empty()
        {
            self.webcams.push(w);
        }
        let mut used: Vec<u64> = Vec::new();
        for w in &mut self.webcams {
            if w.key == 0 || used.contains(&w.key) {
                w.key = used.iter().copied().max().unwrap_or(0) + 1;
            }
            used.push(w.key);
        }
        let mut used: Vec<u64> = Vec::new();
        for d in &mut self.visuals {
            if d.key == 0 || used.contains(&d.key) {
                d.key = used.iter().copied().max().unwrap_or(0) + 1;
            }
            used.push(d.key);
        }
        if self.layer_order.is_empty() {
            let cams = self.webcams.iter().map(|w| LayerKey::Webcam(w.key));
            self.layer_order = if self.screen.in_front { std::iter::once(LayerKey::Screen).chain(cams).collect() } else { cams.chain(std::iter::once(LayerKey::Screen)).collect() };
        }
        self.layer_order = self.layer_order();
    }

    /// The scene shown's sources, the one in front first: each once, those
    /// gone left out. A source of the preset's the scene doesn't have
    /// (the screen too: a scene can do without it) isn't in it (see
    /// `outside_scene`).
    pub fn layer_order(&self) -> Vec<LayerKey> {
        let exists = |k: &LayerKey| match k {
            LayerKey::Screen => true,
            LayerKey::Webcam(key) => self.webcams.iter().any(|w| w.key == *key),
            LayerKey::Visual(key) => self.visuals.iter().any(|d| d.key == *key),
        };
        let mut order: Vec<LayerKey> = Vec::new();
        for k in &self.layer_order {
            if exists(k) && !order.contains(k) {
                order.push(*k);
            }
        }
        order
    }

    /// A key no webcam or source has, in any scene or preset (so one never
    /// stands for another, in the picture caches or the capture's layers).
    pub fn new_key(&self) -> u64 {
        let live = self.webcams.iter().map(|w| w.key).chain(self.visuals.iter().map(|d| d.key));
        let stored = self.scenes.iter().flat_map(|s| s.items.iter()).filter_map(SceneItem::key);
        let others = self.presets.iter().filter_map(|p| p.setup.as_deref()).flat_map(|s| {
            let scenes = s.scenes.iter().flat_map(|c| c.items.iter()).filter_map(SceneItem::key);
            s.webcams.iter().map(|w| w.key).chain(s.visuals.iter().map(|v| v.key)).chain(scenes)
        });
        live.chain(stored).chain(others).max().unwrap_or(0) + 1
    }

    pub fn webcam(&self, key: u64) -> Option<&WebcamCfg> {
        self.webcams.iter().find(|w| w.key == key)
    }

    pub fn webcam_mut(&mut self, key: u64) -> Option<&mut WebcamCfg> {
        self.webcams.iter_mut().find(|w| w.key == key)
    }

    /// The webcam showing camera `device`, if one does.
    pub fn webcam_of(&self, device: &str) -> Option<&WebcamCfg> {
        self.webcams.iter().find(|w| w.id == device)
    }

    pub fn visual(&self, key: u64) -> Option<&VisualCfg> {
        self.visuals.iter().find(|d| d.key == key)
    }

    pub fn visual_mut(&mut self, key: u64) -> Option<&mut VisualCfg> {
        self.visuals.iter_mut().find(|d| d.key == key)
    }

    /// Take a webcam or display out (the screen stays).
    pub fn remove_layer(&mut self, layer: LayerKey) {
        match layer {
            LayerKey::Screen => {}
            LayerKey::Webcam(key) => self.webcams.retain(|w| w.key != key),
            LayerKey::Visual(key) => self.visuals.retain(|d| d.key != key),
        }
        self.layer_order.retain(|l| *l != layer);
    }

    /// The first webcam in the order (the one in front), if any: the one
    /// drawn where only one is (`capture::layers::MANY`).
    pub fn first_webcam(&self) -> Option<&WebcamCfg> {
        self.layer_order().into_iter().find_map(|l| match l {
            LayerKey::Webcam(key) => self.webcam(key),
            _ => None,
        })
    }

    /// Whether the screen's in front of the first webcam (see `first_webcam`).
    pub fn screen_in_front(&self) -> bool {
        let order = self.layer_order();
        let first = self.first_webcam().map(|w| LayerKey::Webcam(w.key));
        let at = |k: LayerKey| order.iter().position(|l| *l == k);
        first.is_none_or(|w| at(LayerKey::Screen) < at(w))
    }

    /// Where another display goes in the video (`frame_aspect`: the video's
    /// shape; `picture`: the display's size, when known): its box, all of
    /// it fitted in; off, hidden.
    pub fn visual_placement(&self, d: &VisualCfg) -> capture::ScreenPlacement {
        let (rect, framing) = match d.placed {
            Some(b) => ([b.x, b.y, b.w, b.h], if b.stretched() || d.visual.shapeless() { capture::Framing::Stretch } else { capture::Framing::Fit }),
            None => ([0.0, 0.0, 1.0, 1.0], if d.visual.shapeless() { capture::Framing::Stretch } else { capture::Framing::Fit }),
        };
        let crop = d.crop.filter(|c| !c.is_none() && d.visual.croppable()).map(Into::into);
        capture::ScreenPlacement { rect, framing, in_front: false, hidden: !d.enabled, crop, turns: d.turns % 4, flip_h: d.flip_h, flip_v: d.flip_v }
    }

    /// What a source is called: what you called it, else its kind's name.
    pub fn layer_title(&self, layer: LayerKey) -> String {
        match layer {
            LayerKey::Screen => self.screen_title.clone().unwrap_or_else(|| if matches!(self.capture, CaptureTarget::Apps { .. }) { "Games and apps".to_owned() } else { "Screen".to_owned() }),
            LayerKey::Webcam(key) => self.webcam(key).and_then(|w| w.title.clone()).unwrap_or_else(|| "Webcam".to_owned()),
            LayerKey::Visual(key) => self.visual(key).map(|v| v.title.clone().unwrap_or_else(|| v.visual.kind_name().to_owned())).unwrap_or_default(),
        }
    }

    /// Call a source `title` (empty: its kind's name again).
    pub fn set_layer_title(&mut self, layer: LayerKey, title: &str) {
        let title = title.trim();
        let title = (!title.is_empty()).then(|| title.to_owned());
        match layer {
            LayerKey::Screen => self.screen_title = title,
            LayerKey::Webcam(key) => {
                if let Some(w) = self.webcam_mut(key) {
                    w.title = title;
                }
            }
            LayerKey::Visual(key) => {
                if let Some(v) = self.visual_mut(key) {
                    v.title = title;
                }
            }
        }
    }

    // --- Scenes: the live fields are the scene shown ---

    /// How `layer` is placed in the scene shown, as a stored scene keeps it.
    pub fn item_of(&self, layer: LayerKey) -> Option<SceneItem> {
        match layer {
            LayerKey::Screen => {
                let s = &self.screen;
                Some(SceneItem::Screen { enabled: s.enabled, placed: s.placed, turns: s.turns, flip_h: s.flip_h, flip_v: s.flip_v, framing: self.framing, crop: self.screen_crop })
            }
            LayerKey::Webcam(key) => self.webcam(key).map(|w| SceneItem::Webcam { key, enabled: w.enabled, placement: w.placement }),
            LayerKey::Visual(key) => self.visual(key).map(|v| SceneItem::Visual { key, enabled: v.enabled, placed: v.placed, turns: v.turns, flip_h: v.flip_h, flip_v: v.flip_v, crop: v.crop }),
        }
    }

    /// Place a source in the scene shown as `item` has it: false if its
    /// source is gone.
    fn put_item(&mut self, item: &SceneItem) -> bool {
        match item {
            SceneItem::Screen { enabled, placed, turns, flip_h, flip_v, framing, crop } => {
                let s = &mut self.screen;
                (s.enabled, s.placed, s.turns, s.flip_h, s.flip_v) = (*enabled, *placed, *turns, *flip_h, *flip_v);
                (self.framing, self.screen_crop) = (*framing, *crop);
                true
            }
            SceneItem::Webcam { key, enabled, placement } => match self.webcam_mut(*key) {
                Some(w) => {
                    (w.enabled, w.placement) = (*enabled, *placement);
                    true
                }
                None => false,
            },
            SceneItem::Visual { key, enabled, placed, turns, flip_h, flip_v, crop } => match self.visual_mut(*key) {
                Some(v) => {
                    (v.enabled, v.placed, v.turns, v.flip_h, v.flip_v, v.crop) = (*enabled, *placed, *turns, *flip_h, *flip_v, *crop);
                    true
                }
                None => false,
            },
        }
    }

    /// The scene shown, as a stored scene keeps it.
    pub fn live_items(&self) -> Vec<SceneItem> {
        self.layer_order().into_iter().filter_map(|l| self.item_of(l)).collect()
    }

    /// Every source the preset has: the screen, each webcam, each other.
    fn pool(&self) -> Vec<LayerKey> {
        std::iter::once(LayerKey::Screen).chain(self.webcams.iter().map(|w| LayerKey::Webcam(w.key))).chain(self.visuals.iter().map(|v| LayerKey::Visual(v.key))).collect()
    }

    /// The preset's sources the scene shown doesn't have: kept going hidden
    /// in the capture, so switching to a scene with them shows them at once.
    pub fn outside_scene(&self) -> Vec<LayerKey> {
        let order = self.layer_order();
        self.pool().into_iter().filter(|k| !order.contains(k)).collect()
    }

    /// Whether a scene other than the one shown has `layer`.
    fn stored_scene_has(&self, layer: LayerKey) -> bool {
        self.scenes.iter().any(|s| s.id != self.active_scene && s.items.iter().any(|i| i.layer() == layer))
    }

    /// The scene shown.
    pub fn scene(&self) -> Option<&SceneCfg> {
        self.scenes.iter().find(|s| s.id == self.active_scene)
    }

    /// Show scene `id`: the one shown is put away as it is, and `id`'s
    /// placements come out into the live fields. False if it's shown
    /// already, or isn't there.
    pub fn switch_scene(&mut self, id: u64) -> bool {
        if id == self.active_scene {
            return false;
        }
        let Some(to) = self.scenes.iter().position(|s| s.id == id) else { return false };
        let items = self.live_items();
        let shown = self.active_scene;
        let locked = std::mem::take(&mut self.locked);
        let groups = std::mem::take(&mut self.groups);
        if let Some(from) = self.scenes.iter_mut().find(|s| s.id == shown) {
            from.items = items;
            from.locked = locked;
            from.groups = groups;
        }
        self.locked = std::mem::take(&mut self.scenes[to].locked);
        self.groups = std::mem::take(&mut self.scenes[to].groups);
        let items = std::mem::take(&mut self.scenes[to].items);
        self.active_scene = id;
        self.layer_order.clear();
        for item in &items {
            if !self.layer_order.contains(&item.layer()) && self.put_item(item) {
                self.layer_order.push(item.layer());
            }
        }
        self.pack_groups();
        true
    }

    /// A new scene after the one shown: a copy of it, or just the screen.
    /// Its id (it isn't shown yet).
    pub fn add_scene(&mut self, copy_of_shown: bool) -> u64 {
        let id = self.new_scene_id();
        let (name, items, locked, groups) = match self.scene().filter(|_| copy_of_shown) {
            Some(s) => (format!("{} copy", s.name), self.live_items(), self.locked.clone(), self.groups.clone()),
            None => (format!("Scene {}", self.scenes.len() + 1), vec![SceneItem::screen(true)], Vec::new(), Vec::new()),
        };
        let name = self.unused_scene_name(&name);
        let at = self.scenes.iter().position(|s| s.id == self.active_scene).map_or(self.scenes.len(), |i| i + 1);
        self.scenes.insert(at, SceneCfg { id, name, items, locked, groups });
        id
    }

    /// A copy of scene `id`, after it: its id.
    pub fn duplicate_scene(&mut self, id: u64) -> Option<u64> {
        let at = self.scenes.iter().position(|s| s.id == id)?;
        let (items, locked, groups) = if id == self.active_scene {
            (self.live_items(), self.locked.clone(), self.groups.clone())
        } else {
            (self.scenes[at].items.clone(), self.scenes[at].locked.clone(), self.scenes[at].groups.clone())
        };
        let name = self.unused_scene_name(&format!("{} copy", self.scenes[at].name));
        let new = self.new_scene_id();
        self.scenes.insert(at + 1, SceneCfg { id: new, name, items, locked, groups });
        Some(new)
    }

    /// Take scene `id` out (never the last): the one after it (else before)
    /// is shown if it was. Sources no scene has any more go too.
    pub fn delete_scene(&mut self, id: u64) -> bool {
        if self.scenes.len() < 2 {
            return false;
        }
        let Some(at) = self.scenes.iter().position(|s| s.id == id) else { return false };
        if id == self.active_scene {
            let next = self.scenes.get(at + 1).or_else(|| at.checked_sub(1).and_then(|i| self.scenes.get(i))).map(|s| s.id);
            let Some(next) = next else { return false };
            self.switch_scene(next);
        }
        self.scenes.retain(|s| s.id != id);
        self.tidy_pool();
        true
    }

    /// Call scene `id` `name` (an empty name leaves it as it was).
    pub fn rename_scene(&mut self, id: u64, name: &str) {
        let name = name.trim();
        if let Some(s) = self.scenes.iter_mut().find(|s| s.id == id).filter(|_| !name.is_empty()) {
            s.name = name.to_owned();
        }
    }

    /// Move scene `id` one place up the list (`up`) or down.
    pub fn move_scene(&mut self, id: u64, up: bool) {
        let Some(at) = self.scenes.iter().position(|s| s.id == id) else { return };
        let to = if up { at.checked_sub(1) } else { (at + 1 < self.scenes.len()).then_some(at + 1) };
        if let Some(to) = to {
            self.scenes.swap(at, to);
        }
    }

    /// Put one of the preset's sources the scene shown doesn't have into
    /// it, in front, showing, where it was last placed. False if it's there
    /// already, or gone.
    pub fn add_to_scene(&mut self, layer: LayerKey) -> bool {
        if self.layer_order().contains(&layer) || !self.pool().contains(&layer) {
            return false;
        }
        match layer {
            LayerKey::Webcam(key) => self.webcam_mut(key).into_iter().for_each(|w| w.enabled = true),
            LayerKey::Visual(key) => self.visual_mut(key).into_iter().for_each(|v| v.enabled = true),
            LayerKey::Screen => self.screen.enabled = true,
        }
        self.layer_order = self.layer_order();
        self.layer_order.insert(0, layer);
        true
    }

    /// Take a source out of the scene shown (never the screen, which every
    /// scene has): gone for good once no other scene has it. Whether it went
    /// for good.
    pub fn remove_from_scene(&mut self, layer: LayerKey) -> bool {
        self.layer_order.retain(|l| *l != layer);
        self.locked.retain(|l| *l != layer);
        self.pack_groups();
        // The screen is the preset's own: never gone, only out of scenes.
        let elsewhere = self.stored_scene_has(layer) || layer == LayerKey::Screen;
        if !elsewhere {
            self.remove_layer(layer);
        }
        !elsewhere
    }

    /// The other scenes that have `layer`, in their order: by id and name.
    pub fn scenes_with(&self, layer: LayerKey) -> Vec<(u64, String)> {
        self.scenes.iter().filter(|s| s.id != self.active_scene && s.items.iter().any(|i| i.layer() == layer)).map(|s| (s.id, s.name.clone())).collect()
    }

    /// `layer` as it is in the scene shown (where it is, its size, crop and
    /// turn, shown or hidden) into scene `id` as well: the same source,
    /// shared, put in front of what's behind it here (else at the back);
    /// one `id` has already is placed as here. False if `id` is the scene
    /// shown or isn't there, or the scene shown hasn't `layer`.
    pub fn copy_to_scene(&mut self, layer: LayerKey, id: u64) -> bool {
        if id == self.active_scene {
            return false;
        }
        let order = self.layer_order();
        let Some(at) = order.iter().position(|l| *l == layer) else { return false };
        let Some(item) = self.item_of(layer) else { return false };
        let Some(scene) = self.scenes.iter_mut().find(|s| s.id == id) else { return false };
        match scene.items.iter().position(|i| i.layer() == layer) {
            Some(i) => scene.items[i] = item,
            None => {
                let behind = &order[at + 1..];
                let to = scene.items.iter().position(|i| behind.contains(&i.layer())).unwrap_or(scene.items.len());
                scene.items.insert(to, item);
            }
        }
        true
    }

    /// Make `layer`, in the scene shown, its own: a copy of the source (what
    /// it shows too, changed here only from now on) in its place, the other
    /// scenes keeping the one they share. Not the screen, the preset's one
    /// capture, nor a webcam: a camera opens once, in one format. The copy, or
    /// `None` if it can't be or no other scene has it.
    pub fn unlink(&mut self, layer: LayerKey) -> Option<LayerKey> {
        let LayerKey::Visual(key) = layer else { return None };
        if !self.stored_scene_has(layer) {
            return None;
        }
        let mut order = self.layer_order();
        let at = order.iter().position(|l| *l == layer)?;
        let mut copy = self.visual(key)?.clone();
        copy.key = self.new_key();
        let new = LayerKey::Visual(copy.key);
        self.visuals.push(copy);
        order[at] = new;
        self.layer_order = order;
        for l in self.locked.iter_mut().chain(self.groups.iter_mut().flat_map(|g| g.members.iter_mut())).filter(|l| **l == layer) {
            *l = new;
        }
        Some(new)
    }

    /// The group `layer` is in, in the scene shown.
    pub fn group_of(&self, layer: LayerKey) -> Option<&GroupCfg> {
        self.groups.iter().find(|g| g.members.contains(&layer))
    }

    /// Group `id` of the scene shown.
    pub fn group(&self, id: u64) -> Option<&GroupCfg> {
        self.groups.iter().find(|g| g.id == id)
    }

    pub fn group_mut(&mut self, id: u64) -> Option<&mut GroupCfg> {
        self.groups.iter_mut().find(|g| g.id == id)
    }

    /// `layers` of the scene shown in a new group (out of any they were
    /// in), together where the one in front of them is. Its id; `None`
    /// with none of them in the scene.
    pub fn make_group(&mut self, layers: &[LayerKey]) -> Option<u64> {
        let order = self.layer_order();
        let members: Vec<LayerKey> = order.iter().filter(|l| layers.contains(l)).copied().collect();
        if members.is_empty() {
            return None;
        }
        for g in &mut self.groups {
            g.members.retain(|m| !members.contains(m));
        }
        let others = self.presets.iter().filter_map(|p| p.setup.as_deref()).flat_map(|s| s.groups.iter().chain(s.scenes.iter().flat_map(|c| c.groups.iter())));
        let id = self.groups.iter().chain(self.scenes.iter().flat_map(|c| c.groups.iter())).chain(others).map(|g| g.id).max().unwrap_or(0) + 1;
        let name = unused_name(&format!("Group {}", self.groups.len() + 1), |n| self.groups.iter().any(|g| g.name == n));
        self.groups.push(GroupCfg { id, name, members, collapsed: false });
        self.pack_groups();
        Some(id)
    }

    /// Group `id`'s sources on their own again, where they are.
    pub fn ungroup(&mut self, id: u64) {
        self.groups.retain(|g| g.id != id);
    }

    /// `layer` into group `id` (out of the one it was in), after its
    /// others; or out of any group, with `None`.
    pub fn set_group(&mut self, layer: LayerKey, id: Option<u64>) {
        for g in &mut self.groups {
            g.members.retain(|m| *m != layer);
        }
        if let Some(g) = id.and_then(|id| self.group_mut(id)) {
            g.members.push(layer);
            // At the back of the group: just behind its last one.
            if let Some(last) = g.members.iter().rev().nth(1).copied() {
                let mut order = self.layer_order();
                order.retain(|l| *l != layer);
                let at = order.iter().position(|l| *l == last).map_or(order.len(), |i| i + 1);
                order.insert(at, layer);
                self.layer_order = order;
            }
        }
        self.pack_groups();
    }

    /// Group `id` moved as one to `to` in the order of the rest (0: in
    /// front).
    pub fn move_group(&mut self, id: u64, to: usize) {
        let Some(members) = self.group(id).map(|g| g.members.clone()) else { return };
        let mut order = self.layer_order();
        order.retain(|l| !members.contains(l));
        let to = to.min(order.len());
        order.splice(to..to, members);
        self.layer_order = order;
        self.pack_groups();
    }

    /// `layers` moved together, in their order, to `to` in the order of the
    /// scene's other sources (0: in front of them all), in group `group`
    /// (`None`: in none): dragged in the layers, past others, into a group
    /// or out of one. A group left with none of its sources is gone.
    pub fn move_layers_to(&mut self, layers: &[LayerKey], to: usize, group: Option<u64>) {
        let order = self.layer_order();
        let moving: Vec<LayerKey> = order.iter().copied().filter(|l| layers.contains(l)).collect();
        let mut rest: Vec<LayerKey> = order.into_iter().filter(|l| !layers.contains(l)).collect();
        let to = to.min(rest.len());
        rest.splice(to..to, moving.iter().copied());
        self.layer_order = rest;
        for g in &mut self.groups {
            g.members.retain(|m| !moving.contains(m));
        }
        if let Some(g) = group.and_then(|id| self.group_mut(id)) {
            g.members.extend(moving.iter().copied());
        }
        self.pack_groups();
    }

    /// Each group's sources together in the scene shown's order (where its
    /// one in front is), front first; what isn't in the scene out of them,
    /// and a group left with none gone.
    pub fn pack_groups(&mut self) {
        let mut order = self.layer_order();
        for g in &mut self.groups {
            g.members.retain(|m| order.contains(m));
        }
        self.groups.retain(|g| !g.members.is_empty());
        for g in &mut self.groups {
            let Some(at) = order.iter().position(|l| g.members.contains(l)) else { continue };
            let members: Vec<LayerKey> = order.iter().filter(|l| g.members.contains(l)).copied().collect();
            order.retain(|l| !members.contains(l));
            order.splice(at..at, members.iter().copied());
            g.members = members;
        }
        self.layer_order = order;
    }

    /// Group `id` as it is in the scene shown into scene `to` too: each of
    /// its sources placed as here, and the group (as here) there.
    pub fn copy_group_to_scene(&mut self, id: u64, to: u64) -> bool {
        let Some(group) = self.group(id).cloned() else { return false };
        if !group.members.iter().all(|m| self.copy_to_scene(*m, to)) {
            return false;
        }
        let Some(scene) = self.scenes.iter_mut().find(|s| s.id == to) else { return false };
        for g in &mut scene.groups {
            g.members.retain(|m| !group.members.contains(m));
        }
        scene.groups.retain(|g| g.id != id && !g.members.is_empty());
        scene.groups.push(group);
        true
    }

    /// `layers` of the scene shown as they are now (front first), to paste;
    /// with group `group`'s name when they're its. `None` with none of them
    /// in the scene.
    pub fn copy_sources(&self, layers: &[LayerKey], group: Option<u64>) -> Option<CopiedSources> {
        let sources: Vec<CopiedSource> = self
            .layer_order()
            .into_iter()
            .filter(|l| layers.contains(l))
            .filter_map(|l| match l {
                LayerKey::Screen => self.item_of(l).map(CopiedSource::Screen),
                LayerKey::Webcam(key) => self.webcam(key).cloned().map(CopiedSource::Webcam),
                LayerKey::Visual(key) => self.visual(key).cloned().map(CopiedSource::Visual),
            })
            .collect();
        let group = group.and_then(|id| self.group(id)).map(|g| g.name.clone());
        (!sources.is_empty()).then_some(CopiedSources { sources, group })
    }

    /// What a copied source is called (see [`layer_title`](Self::layer_title)).
    pub fn copied_title(&self, source: &CopiedSource) -> String {
        match source {
            CopiedSource::Screen(_) => self.layer_title(LayerKey::Screen),
            CopiedSource::Webcam(w) => w.title.clone().unwrap_or_else(|| "Webcam".to_owned()),
            CopiedSource::Visual(v) => v.title.clone().unwrap_or_else(|| v.visual.kind_name().to_owned()),
        }
    }

    /// `copied` into the scene shown, in their order, in front of `before`
    /// (all of its group, when it's in one), else in front of everything;
    /// placed as they were. A source the preset has goes in as itself,
    /// shared with the scenes that have it (a webcam: the preset's of the
    /// same camera); one it hasn't (cut, or another preset's) as a source
    /// of its own. With `own_copies`, or when the scene has it already,
    /// another source (not a webcam, which opens once, nor the screen) is
    /// its own copy: what it shows changes there alone, and one this scene
    /// has goes a little down and right of it (`aspect`: the video's shape).
    /// A webcam or the screen the scene has stays as it is, as does a
    /// webcam where a scene shows one (`!many`) and it has another. A
    /// group's go in a group of its name.
    pub fn paste_sources(&mut self, copied: &CopiedSources, before: Option<LayerKey>, own_copies: bool, many: bool, aspect: f32) -> Pasted {
        let mut out = Pasted::default();
        let mut order = self.layer_order();
        let anchor = before.map(|b| self.group_of(b).and_then(|g| g.members.first().copied()).unwrap_or(b));
        let mut at = anchor.and_then(|a| order.iter().position(|l| *l == a)).unwrap_or(0);
        for source in &copied.sources {
            let title = self.copied_title(source);
            let layer = match source {
                CopiedSource::Screen(item) => {
                    if order.contains(&LayerKey::Screen) {
                        out.there.push(title);
                        continue;
                    }
                    self.put_item(item);
                    LayerKey::Screen
                }
                CopiedSource::Webcam(w) => {
                    let same = self.webcam(w.key).or_else(|| self.webcam_of(&w.id)).map(|c| c.key);
                    if same.is_some_and(|key| order.contains(&LayerKey::Webcam(key))) {
                        out.there.push(title);
                        continue;
                    }
                    if !many && order.iter().any(|l| matches!(l, LayerKey::Webcam(_))) {
                        out.full.push(title);
                        continue;
                    }
                    let key = match same {
                        Some(key) => key,
                        None => {
                            let key = self.new_key();
                            self.webcams.push(WebcamCfg { key, ..w.clone() });
                            key
                        }
                    };
                    self.put_item(&SceneItem::Webcam { key, enabled: w.enabled, placement: w.placement });
                    LayerKey::Webcam(key)
                }
                CopiedSource::Visual(v) => {
                    let here = order.contains(&LayerKey::Visual(v.key));
                    let pooled = self.visual(v.key).cloned();
                    if pooled.is_some() && !here && !own_copies {
                        self.put_item(&SceneItem::Visual { key: v.key, enabled: v.enabled, placed: v.placed, turns: v.turns, flip_h: v.flip_h, flip_v: v.flip_v, crop: v.crop });
                        LayerKey::Visual(v.key)
                    } else {
                        // What it shows as it is now, if the preset has it;
                        // placed as it was copied.
                        let mut own = pooled.unwrap_or_else(|| v.clone());
                        (own.enabled, own.placed, own.turns, own.flip_h, own.flip_v, own.crop) = (v.enabled, v.placed, v.turns, v.flip_h, v.flip_v, v.crop);
                        own.key = self.new_key();
                        if here {
                            own.title = own.title.map(|t| format!("{t} copy"));
                            if let Some(b) = own.placed.as_mut() {
                                b.x += 0.02;
                                b.y += 0.02 * aspect;
                            }
                        }
                        let key = own.key;
                        self.visuals.push(own);
                        LayerKey::Visual(key)
                    }
                }
            };
            order.insert(at.min(order.len()), layer);
            at += 1;
            out.layers.push(layer);
        }
        self.layer_order = order;
        if let Some(name) = copied.group.as_deref().filter(|_| !out.layers.is_empty()) {
            let name = unused_name(name, |n| self.groups.iter().any(|g| g.name == n));
            out.group = self.make_group(&out.layers);
            if let Some(g) = out.group.and_then(|id| self.group_mut(id)) {
                g.name = name;
            }
        }
        self.pack_groups();
        out
    }

    /// Each video source whose sound is on (and whose file has some, as
    /// `has_sound` says) has a source of its own in the sound's list (its
    /// volume, mute, track), named as the video is; one whose video is gone,
    /// or its sound off, goes.
    pub fn sync_video_sounds(&mut self, has_sound: impl Fn(&std::path::Path) -> bool) {
        let wanted: Vec<(u64, String)> = self
            .visuals
            .iter()
            .filter_map(|v| match &v.visual {
                Visual::Video { path, sound: true, .. } if has_sound(path) => {
                    let name = v.title.clone().unwrap_or_else(|| path.file_stem().map_or_else(|| "Video".to_owned(), |n| n.to_string_lossy().into_owned()));
                    Some((v.key, name))
                }
                _ => None,
            })
            .collect();
        self.audio_sources.retain(|s| !matches!(s.kind, SourceKind::Video { key } if !wanted.iter().any(|(k, _)| *k == key)));
        for (key, name) in wanted {
            match self.audio_sources.iter_mut().find(|s| s.kind == SourceKind::Video { key }) {
                Some(s) => {
                    if s.name != name {
                        s.name = name;
                    }
                }
                None => {
                    let mut s = AudioSourceCfg::new(&name, SourceKind::Video { key });
                    s.id = format!("video-{key}");
                    self.audio_sources.push(s);
                }
            }
        }
    }

    /// Whether `layer` is locked in the scene shown.
    pub fn is_locked(&self, layer: LayerKey) -> bool {
        self.locked.contains(&layer)
    }

    /// Lock `layer` in the scene shown, or unlock it.
    pub fn set_locked(&mut self, layer: LayerKey, on: bool) {
        self.locked.retain(|l| *l != layer);
        if on {
            self.locked.push(layer);
        }
    }

    /// Sources no scene has any more (one deleted, an undo) go.
    pub fn tidy_pool(&mut self) {
        let order = self.layer_order();
        let gone: Vec<LayerKey> = self.pool().into_iter().filter(|k| !order.contains(k) && !self.stored_scene_has(*k)).collect();
        for k in gone {
            self.remove_layer(k);
        }
    }

    /// The webcams to keep open: showing in the scene shown, or in another
    /// of the preset's (switching to it shows them at once). One hidden in
    /// every scene is closed.
    pub fn cameras_in_use(&self) -> Vec<&WebcamCfg> {
        let order = self.layer_order();
        let stored = |key: u64| self.scenes.iter().any(|s| s.items.iter().any(|i| matches!(i, SceneItem::Webcam { key: k, enabled: true, .. } if *k == key)));
        self.webcams.iter().filter(|w| (w.enabled && order.contains(&LayerKey::Webcam(w.key))) || stored(w.key)).collect()
    }

    /// An id no scene has, in any preset.
    fn new_scene_id(&self) -> u64 {
        let others = self.presets.iter().filter_map(|p| p.setup.as_deref()).flat_map(|s| s.scenes.iter().map(|c| c.id));
        self.scenes.iter().map(|s| s.id).chain(others).max().unwrap_or(0) + 1
    }

    /// `name`, or with a number after it if a scene's called that already.
    fn unused_scene_name(&self, name: &str) -> String {
        unused_name(name, |n| self.scenes.iter().any(|s| s.name == n))
    }

    // --- Presets: the live fields are the preset in use ---

    /// The preset in use.
    pub fn preset(&self) -> Option<&PresetCfg> {
        self.presets.iter().find(|p| p.id == self.active_preset)
    }

    /// The preset in use's setup, out of the live fields (left as they are
    /// when new): to store it, or carry it over (Reset settings).
    pub fn take_setup(&mut self) -> PresetSetup {
        PresetSetup {
            capture: std::mem::take(&mut self.capture),
            idle_apps: self.idle_apps.take(),
            display_id: self.display_id.take(),
            display_index: std::mem::take(&mut self.display_index),
            screen: std::mem::take(&mut self.screen),
            screen_title: self.screen_title.take(),
            framing: std::mem::take(&mut self.framing),
            screen_crop: self.screen_crop.take(),
            webcams: std::mem::take(&mut self.webcams),
            visuals: std::mem::take(&mut self.visuals),
            layer_order: std::mem::take(&mut self.layer_order),
            locked: std::mem::take(&mut self.locked),
            groups: std::mem::take(&mut self.groups),
            scenes: std::mem::take(&mut self.scenes),
            active_scene: std::mem::take(&mut self.active_scene),
            audio_sources: std::mem::take(&mut self.audio_sources),
            limiter: self.limiter,
        }
    }

    /// `setup` into the live fields (see `take_setup`).
    pub fn put_setup(&mut self, s: PresetSetup) {
        self.capture = s.capture;
        self.idle_apps = s.idle_apps;
        (self.display_id, self.display_index) = (s.display_id, s.display_index);
        self.screen = s.screen;
        self.screen_title = s.screen_title;
        (self.framing, self.screen_crop) = (s.framing, s.screen_crop);
        self.webcams = s.webcams;
        self.visuals = s.visuals;
        self.layer_order = s.layer_order;
        self.locked = s.locked;
        self.groups = s.groups;
        (self.scenes, self.active_scene) = (s.scenes, s.active_scene);
        self.audio_sources = s.audio_sources;
        self.limiter = s.limiter;
    }

    /// `old`'s presets, with the one in use's setup, carried over into these
    /// settings (Reset settings keeps your sources, scenes and sound).
    pub fn keep_presets_from(&mut self, old: &mut RecordSettings) {
        self.put_setup(old.take_setup());
        self.presets = std::mem::take(&mut old.presets);
        self.active_preset = old.active_preset;
    }

    /// Use preset `id`: the one in use is stored as it is, and `id`'s setup
    /// comes out into the live fields. False if it's in use already, or
    /// isn't there. (A capture keeps what it started with: switch while
    /// it's stopped.)
    pub fn switch_preset(&mut self, id: u64) -> bool {
        if id == self.active_preset {
            return false;
        }
        let Some(setup) = self.presets.iter_mut().find(|p| p.id == id).and_then(|p| p.setup.take()) else { return false };
        let current = self.take_setup();
        let using = self.active_preset;
        if let Some(from) = self.presets.iter_mut().find(|p| p.id == using) {
            from.setup = Some(Box::new(current));
        }
        self.put_setup(*setup);
        self.active_preset = id;
        true
    }

    /// A new preset after the one in use, called `name`: a copy of it (with
    /// sources of its own, so changing one leaves the other alone), or just
    /// the screen with your mic and your computer's sound. Its id (it isn't
    /// in use yet).
    pub fn add_preset(&mut self, name: &str, copy: bool) -> u64 {
        let setup = if copy { self.copied_setup() } else { self.fresh_setup() };
        let id = self.presets.iter().map(|p| p.id).max().unwrap_or(0) + 1;
        let name = unused_name(name.trim(), |n| self.presets.iter().any(|p| p.name == n));
        let at = self.presets.iter().position(|p| p.id == self.active_preset).map_or(self.presets.len(), |i| i + 1);
        self.presets.insert(at, PresetCfg { id, name, setup: Some(Box::new(setup)) });
        id
    }

    /// The preset in use's setup, copied, its sources, scenes and sounds
    /// given keys and ids of their own.
    fn copied_setup(&self) -> PresetSetup {
        let mut s = self.clone().take_setup();
        // Webcams and sources share one key space: old key → new.
        let mut keys: Vec<(u64, u64)> = Vec::new();
        let olds = s.webcams.iter_mut().map(|w| &mut w.key).chain(s.visuals.iter_mut().map(|v| &mut v.key));
        for (next, key) in (self.new_key()..).zip(olds) {
            keys.push((*key, next));
            *key = next;
        }
        let new_key = |k: u64| keys.iter().find(|(was, _)| *was == k).map_or(k, |(_, now)| *now);
        let grouped = s.groups.iter_mut().chain(s.scenes.iter_mut().flat_map(|c| c.groups.iter_mut())).flat_map(|g| g.members.iter_mut());
        for l in s.layer_order.iter_mut().chain(s.locked.iter_mut()).chain(grouped) {
            if let LayerKey::Webcam(k) | LayerKey::Visual(k) = l {
                *k = new_key(*k);
            }
        }
        for l in s.scenes.iter_mut().flat_map(|c| c.locked.iter_mut()) {
            if let LayerKey::Webcam(k) | LayerKey::Visual(k) = l {
                *k = new_key(*k);
            }
        }
        let shown = s.active_scene;
        for (id, scene) in (self.new_scene_id()..).zip(s.scenes.iter_mut()) {
            if scene.id == shown {
                s.active_scene = id;
            }
            scene.id = id;
            for item in &mut scene.items {
                if let SceneItem::Webcam { key, .. } | SceneItem::Visual { key, .. } = item {
                    *key = new_key(*key);
                }
            }
        }
        for a in &mut s.audio_sources {
            // A video's sound goes with its copy (its volume kept).
            a.id = match &mut a.kind {
                SourceKind::Video { key } => {
                    *key = new_key(*key);
                    format!("video-{key}")
                }
                _ => new_source_id(),
            };
        }
        s
    }

    /// A new preset's setup: the screen (the display in use now) filling the
    /// video, your mic and your computer's sound, in one scene.
    fn fresh_setup(&self) -> PresetSetup {
        let mut s = PresetSetup { display_id: self.display_id.clone(), display_index: self.display_index, ..PresetSetup::default() };
        let id = self.new_scene_id();
        s.scenes = vec![SceneCfg::new(id, "Scene 1")];
        s.active_scene = id;
        s
    }

    /// Take preset `id` out (never the last): the one after it (else before)
    /// is used if it was in use.
    pub fn delete_preset(&mut self, id: u64) -> bool {
        if self.presets.len() < 2 {
            return false;
        }
        let Some(at) = self.presets.iter().position(|p| p.id == id) else { return false };
        if id == self.active_preset {
            let next = self.presets.get(at + 1).or_else(|| at.checked_sub(1).and_then(|i| self.presets.get(i))).map(|p| p.id);
            if !next.is_some_and(|next| self.switch_preset(next)) {
                return false;
            }
        }
        self.presets.retain(|p| p.id != id);
        true
    }

    /// Call preset `id` `name` (an empty name leaves it as it was).
    pub fn rename_preset(&mut self, id: u64, name: &str) {
        let name = name.trim();
        if let Some(p) = self.presets.iter_mut().find(|p| p.id == id).filter(|_| !name.is_empty()) {
            p.name = name.to_owned();
        }
    }

    /// Scenes and presets as they should be. Settings from before them get
    /// one preset with one scene showing every source (those not in the
    /// order in front, as they showed then); with them, they're checked (see
    /// `check_scenes`), the preset in use there without a setup of its own,
    /// the others each with theirs.
    fn upgrade_scenes(&mut self) {
        if self.scenes.is_empty() {
            let order = self.layer_order.clone();
            let mut all: Vec<LayerKey> = self.pool().into_iter().filter(|k| !order.contains(k)).collect();
            all.extend(order);
            self.layer_order = all;
            let id = self.new_scene_id();
            self.scenes.push(SceneCfg::new(id, "Scene 1"));
            self.active_scene = id;
        }
        let pool = self.pool();
        check_scenes(&mut self.scenes, &mut self.active_scene, &pool);
        self.layer_order = self.layer_order();
        let using = self.active_preset;
        let mut seen: Vec<u64> = Vec::new();
        self.presets.retain(|p| {
            let keep = (p.setup.is_some() || p.id == using) && !seen.contains(&p.id);
            seen.push(p.id);
            keep
        });
        if !self.presets.iter().any(|p| p.id == using) {
            let id = self.presets.iter().map(|p| p.id).max().unwrap_or(0) + 1;
            self.presets.insert(0, PresetCfg { id, name: "Default".to_owned(), setup: None });
            self.active_preset = id;
        }
        for p in &mut self.presets {
            if p.id == self.active_preset {
                p.setup = None;
            } else if let Some(s) = p.setup.as_deref_mut() {
                let pool = s.pool();
                check_scenes(&mut s.scenes, &mut s.active_scene, &pool);
            }
        }
    }

    /// A built-in sound that's gone (Silverfish) becomes the default, so the
    /// picker names what plays.
    fn upgrade_save_sound(&mut self) {
        let s = &self.save_sound.sound;
        if !s.starts_with("file:") && !crate::sound::BUILTIN.iter().any(|(id, _)| id == s) {
            self.save_sound.sound = SaveSound::default().sound;
        }
    }

    /// Settings as saved (see `to_json`): the video's size as it is, from
    /// a file this version saved; else moved over from an older one's.
    fn from_json(bytes: &[u8]) -> Option<Self> {
        let mut s: Self = serde_json::from_slice(bytes).ok()?;
        match s.saved_size.take() {
            Some(size) => (s.resolution, s.shape) = (size.resolution, size.shape),
            None => s.upgrade_sizes(),
        }
        s.upgrade_framing();
        Some(s)
    }

    /// How the screen goes in, from a version that only used it in a video
    /// of a shape of its own: in the screen's own shape it was always Fit,
    /// whatever was chosen (Fill, unless it was changed), so it's Fit there
    /// now too. In a shape of the video's own it stays as it was.
    fn upgrade_framing(&mut self) {
        if self.framing_version < 1 && self.capture_shape().is_none() {
            self.framing = Framing::Fit;
        }
        self.framing_version = FRAMING_VERSION;
    }

    /// The video's size from a version before it was its own, making clips
    /// the size they were:
    /// - p was the height: upright, 1080p was 608×1080, so it's 608p now
    ///   (wider shapes' height is their short side already).
    /// - A crop was recorded as a display that size: the video is the size
    ///   that made, now the crop is only what's shown of the screen.
    fn upgrade_sizes(&mut self) {
        if self.size_version < 1
            && let (Some([w, h]), Some(p)) = (self.shape.filter(|[w, h]| *w > 0 && w < h), self.resolution.short_side())
        {
            let short = ((p as f64 * w as f64 / h as f64 / 2.0).round() as u32 * 2).max(2);
            self.resolution = OutputResolution::PRESETS.iter().copied().find(|r| r.short_side() == Some(short)).unwrap_or(OutputResolution::Custom(short));
        }
        self.size_version = SIZE_VERSION;
        let screen = matches!(self.capture, CaptureTarget::Screen) && capture::SCREEN_CROP;
        if let Some(c) = self.screen_crop.filter(|c| screen && !c.is_none() && self.resolution.exact().is_none()) {
            let shape = self.shape.filter(|[w, h]| *w > 0 && *h > 0).map(|[w, h]| capture::Shape { w, h, framing: self.framing.into() });
            let (w, h) = capture::VideoSize { short_side: self.resolution.short_side(), shape, exact: None }.of(c.w & !1, c.h & !1);
            self.resolution = OutputResolution::Exact(w, h);
        }
    }

    /// `resolution` and `shape` as versions from before they changed read
    /// them, for about the same size there: p was the height (an upright
    /// 1080p is 1920p there), and a size of its own is a height in a shape
    /// of its own.
    fn size_for_older(&self) -> (OutputResolution, Option<[u32; 2]>) {
        let height = |h: u32| OutputResolution::PRESETS.iter().copied().find(|r| r.short_side() == Some(h)).unwrap_or(OutputResolution::Custom(h));
        match (self.resolution, self.resolution.short_side(), self.shape) {
            (OutputResolution::Exact(w, h), ..) => (height(h), Some(small_ratio(w, h))),
            (_, Some(short), Some([w, h])) if w > 0 && w < h => (height(((short as f64 * h as f64 / w as f64 / 2.0).round() as u32 * 2).max(2)), self.shape),
            (r, ..) => (r, self.shape),
        }
    }

    /// Noise removal that was on, from before there were two, is the one it
    /// was: Strong.
    fn upgrade_noise_reduction(&mut self) {
        for s in &mut self.audio_sources {
            if std::mem::take(&mut s.old_noise_removal) && s.noise_reduction == NoiseReduction::Off {
                s.noise_reduction = NoiseReduction::Strong;
            }
        }
    }

    /// Serialized form, used both to save and to detect changes cheaply.
    /// The video's size is saved in `video_size`, and as older versions read
    /// it where it always was (see `size_for_older`): going back to one
    /// keeps every setting, and about the same size.
    pub fn to_json(&self) -> String {
        let mut s = self.clone();
        s.saved_size = Some(SizeCfg { resolution: self.resolution, shape: self.shape });
        (s.resolution, s.shape) = self.size_for_older();
        // Older versions have one webcam, in front of the screen or behind it:
        // the scene shown's, else the preset's first, off (so its camera
        // and place aren't lost there).
        s.webcam = self.first_webcam().cloned().or_else(|| self.webcams.first().map(|w| WebcamCfg { enabled: false, ..w.clone() }));
        s.screen.in_front = self.screen_in_front();
        s.layer_order = self.layer_order();
        serde_json::to_string_pretty(&s).unwrap_or_default()
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
mod stream_tests {
    use super::*;

    /// Settings saved before streaming load with nowhere to stream to;
    /// what's set is kept. A place added is sent what its service takes.
    #[test]
    fn older_settings_load_and_streams_are_kept() {
        let s: RecordSettings = serde_json::from_str(r#"{"fps": 60}"#).unwrap();
        assert!(s.stream.destinations.is_empty());
        let q = |s: StreamService| {
            let d = StreamDestination::new(s);
            (d.height, d.fps, d.kbps)
        };
        assert_eq!([q(StreamService::Twitch), q(StreamService::YouTube), q(StreamService::Kick)], [(1080, 60, 6000), (1080, 60, 12_000), (1080, 60, 8000)]);
        let mut s = RecordSettings::default();
        s.stream.destinations.push(StreamDestination { key: "live_1".into(), ..StreamDestination::new(StreamService::Twitch) });
        let back: RecordSettings = serde_json::from_str(&s.to_json()).unwrap();
        assert_eq!(back.stream, s.stream);
    }

    /// A place is streamed to once it has what it needs: Twitch and YouTube
    /// a key, Kick its server too, another server its address (its key may
    /// be on it).
    #[test]
    fn destinations_need_what_they_need() {
        let twitch = |key: &str| StreamDestination { key: key.into(), ..StreamDestination::new(StreamService::Twitch) };
        assert_eq!(twitch("").missing(), Some("its stream key"));
        let d = twitch(" live_123 ").destination(true).unwrap();
        assert_eq!((d.name.as_str(), d.url.as_str(), d.secret.as_str()), ("Twitch", "rtmp://live.twitch.tv/app/live_123", "live_123"));
        let youtube = StreamDestination { key: "abcd-efgh".into(), ..StreamDestination::new(StreamService::YouTube) };
        assert_eq!(youtube.destination(true).unwrap().url, "rtmps://a.rtmps.youtube.com:443/live2/abcd-efgh");
        assert_eq!(youtube.destination(false).unwrap().url, "rtmp://a.rtmp.youtube.com/live2/abcd-efgh");
        let mut kick = StreamDestination { key: "sk_us".into(), ..StreamDestination::new(StreamService::Kick) };
        assert_eq!(kick.missing(), Some("its server"));
        kick.server = "rtmps://abc.global-contribute.live-video.net/app".into();
        assert_eq!(kick.destination(true).unwrap().url, "rtmps://abc.global-contribute.live-video.net/app/sk_us");
        let mut own = StreamDestination::new(StreamService::Custom);
        own.server = "srt://nope".into();
        assert!(own.missing().is_some(), "only RTMP and RTMPS");
        own.server = "rtmp://my.server/live/stream".into();
        assert_eq!(own.destination(true).unwrap().url, "rtmp://my.server/live/stream");
        assert_eq!(own.name(), "Another server");
        own.name = "Restream".into();
        assert_eq!(own.name(), "Restream");
        // Off: Go live leaves it out.
        let mut cfg = StreamCfg { destinations: vec![twitch("k"), twitch("")], ..Default::default() };
        assert_eq!(cfg.ready().count(), 1);
        cfg.destinations[0].enabled = false;
        assert_eq!(cfg.ready().count(), 0);
    }
}

#[cfg(test)]
mod screen_capture_tests {
    use super::*;

    /// Settings saved before the choice was there load as Automatic; a choice
    /// is saved by name.
    #[test]
    fn screen_capture_defaults_to_automatic_and_is_saved() {
        let s: RecordSettings = serde_json::from_str(r#"{"fps": 60}"#).unwrap();
        assert_eq!(s.screen_capture, ScreenCapture::Auto);
        let s: RecordSettings = serde_json::from_str(r#"{"screen_capture": "duplication"}"#).unwrap();
        assert_eq!(capture::ScreenCapture::from(s.screen_capture), capture::ScreenCapture::Duplication);
        assert!(s.to_json().contains(r#""screen_capture": "duplication""#));
    }
}

#[cfg(test)]
mod shape_tests {
    use super::*;

    /// No shape unless chosen; chosen, it's saved and goes to the capture
    /// (its size), with how the screen goes into it, which is saved too.
    #[test]
    fn shape_is_saved_and_given_to_the_capture() {
        let s = RecordSettings::default();
        assert_eq!(s.capture_shape(), None);
        let s: RecordSettings = serde_json::from_str(r#"{"shape":[16,9],"framing":"fit"}"#).unwrap();
        assert_eq!(s.capture_shape(), Some(capture::Shape { w: 16, h: 9, framing: capture::Framing::Fill }));
        assert_eq!(s.framing, Framing::Fit);
        assert!(s.to_json().contains(r#""shape": ["#) && s.to_json().contains(r#""framing": "fit""#));
    }

    /// The screen goes into the video as Fit, Fill or Stretch say, unless
    /// it's placed by hand: in a video of its own shape, and in the
    /// screen's too (for a display whose resolution changes while it's
    /// recorded, a game at a stretched one).
    #[test]
    fn how_the_screen_goes_in() {
        let framed = |shape: Option<[u32; 2]>, framing, placed: bool| {
            let screen = ScreenCfg { placed: placed.then(|| BoxCfg::new([0.1, -0.1, 0.5, 1.5], 16.0 / 9.0, None)), ..Default::default() };
            RecordSettings { shape, framing, screen, ..Default::default() }.screen_placement()
        };
        assert_eq!(framed(Some([4, 3]), Framing::Stretch, false).framing, capture::Framing::Stretch);
        assert_eq!(framed(Some([4, 3]), Framing::Fit, false).framing, capture::Framing::Fit);
        assert_eq!(framed(None, Framing::Stretch, false).framing, capture::Framing::Stretch);
        assert_eq!(framed(None, Framing::Fill, false).framing, capture::Framing::Fill);
        assert_eq!(RecordSettings::default().screen_placement().framing, capture::Framing::Fit);
        // In its box, past the video's edge too: all of it over all of the
        // box, stretched as the box is.
        let boxed = framed(Some([4, 3]), Framing::Fill, true);
        assert_eq!((boxed.framing, boxed.rect), (capture::Framing::Stretch, [0.1, -0.1, 0.5, 1.5]));
    }

    /// Settings from before Fit, Fill and Stretch were used in the screen's
    /// own shape go in as they did: Fit there, whatever they say (Fill, the
    /// default then), and as they say in a shape of the video's own. Once
    /// saved since, what's chosen is kept.
    #[test]
    fn older_settings_go_in_as_they_did() {
        let load = |json: &str| RecordSettings::from_json(json.as_bytes()).unwrap();
        assert_eq!(load(r#"{"framing":"fill"}"#).framing, Framing::Fit);
        assert_eq!(load(r#"{"framing":"stretch","video_size":{"resolution":"Native","shape":null}}"#).framing, Framing::Fit);
        assert_eq!(load(r#"{"framing":"fill","shape":[16,9]}"#).framing, Framing::Fill);
        assert_eq!(load(r#"{"shape":[16,9]}"#).framing, Framing::Fill, "from before it could be chosen");
        assert_eq!(load(r#"{"framing":"stretch","resolution":{"Exact":[1920,1080]},"size_version":1}"#).framing, Framing::Stretch);
        let chosen = RecordSettings { framing: Framing::Stretch, ..Default::default() };
        assert!(chosen.to_json().contains(r#""framing_version": 1"#));
        assert_eq!(load(&chosen.to_json()).framing, Framing::Stretch);
        assert_eq!(load("{}").framing, Framing::Fit);
    }

    /// A game in a box of its own shape fits in it (its window can change
    /// shape); stretched with Shift, it's stretched, as a display is.
    #[test]
    fn a_game_stretched_by_hand() {
        let (wide, four_three) = (16.0 / 9.0, 4.0 / 3.0);
        let game = |placed: BoxCfg| {
            let screen = ScreenCfg { placed: Some(placed), ..Default::default() };
            let capture = CaptureTarget::Apps { apps: Vec::new(), away_screen: true };
            RecordSettings { shape: Some([16, 9]), capture, screen, ..Default::default() }.screen_placement().framing
        };
        // A 4:3 game in a 4:3 box in a 16:9 video.
        let own = BoxCfg::new([0.125, 0.0, 0.75, 1.0], wide, Some(four_three));
        assert!(!own.stretched());
        assert_eq!(game(own), capture::Framing::Fit);
        // Stretched over all of the video.
        let stretched = BoxCfg::new([0.0, 0.0, 1.0, 1.0], wide, Some(four_three));
        assert!(stretched.stretched());
        assert_eq!(game(stretched), capture::Framing::Stretch);
        // From before boxes could stretch: the picture's own shape.
        assert_eq!(game(BoxCfg::new([0.0, 0.0, 1.0, 1.0], wide, None)), capture::Framing::Fit);
    }

    /// An exact size is its own shape, whatever aspect ratio was chosen, and
    /// is kept as it is.
    #[test]
    fn an_exact_size_is_the_shape() {
        let s: RecordSettings = serde_json::from_str(r#"{"shape":[4,3],"resolution":{"Exact":[1080,1350]}}"#).unwrap();
        assert_eq!(s.capture_shape(), Some(capture::Shape { w: 1080, h: 1350, framing: capture::Framing::Fill }));
        assert_eq!(s.video_size().of(2560, 1440), (1080, 1350));
        assert!(s.to_json().contains(r#""Exact": ["#));
    }

    /// From when 1080p was the height, an upright recording keeps its size
    /// (1080p 9:16 was 608×1080); wider ones, new settings and settings
    /// already moved over aren't touched.
    #[test]
    fn old_upright_resolutions_keep_their_size() {
        let load = |json: &str| RecordSettings::from_json(json.as_bytes()).unwrap();
        let s = load(r#"{"shape":[9,16],"resolution":"P1080"}"#);
        assert_eq!(s.resolution, OutputResolution::Custom(608));
        assert_eq!(s.size_version, SIZE_VERSION);
        assert_eq!(s.video_size().of(2560, 1440), (608, 1080));
        assert_eq!(load(r#"{"shape":[9,16],"resolution":"P1440"}"#).resolution, OutputResolution::Custom(810));
        assert_eq!(load(r#"{"shape":[3,4],"resolution":"P1440"}"#).resolution, OutputResolution::P1080);
        assert_eq!(load(r#"{"shape":[9,16],"resolution":"Native"}"#).resolution, OutputResolution::Native);
        assert_eq!(load(r#"{"shape":[21,9],"resolution":"P1080"}"#).resolution, OutputResolution::P1080);
        assert_eq!(load(r#"{"shape":[9,16],"resolution":"P1080","size_version":1}"#).resolution, OutputResolution::P1080);
        let mut fresh = RecordSettings { shape: Some([9, 16]), resolution: OutputResolution::P1080, ..Default::default() };
        fresh.upgrade_sizes();
        assert_eq!(fresh.resolution, OutputResolution::P1080);
    }

    /// A crop was recorded as a display that size: the video keeps being
    /// that size (in the shape and resolution it was in), now a crop
    /// doesn't change it. Not for games and apps, which weren't cropped, or
    /// a size of its own.
    #[test]
    fn a_crop_keeps_its_clips_size() {
        let load = |json: &str| RecordSettings::from_json(json.as_bytes()).unwrap().resolution;
        let crop = r#""screen_crop":{"w":1600,"h":900,"at":null}"#;
        assert_eq!(load(&format!(r#"{{{crop},"resolution":"Native"}}"#)), OutputResolution::Exact(1600, 900));
        assert_eq!(load(&format!(r#"{{{crop},"resolution":"P720"}}"#)), OutputResolution::Exact(1280, 720));
        // A 4:3 game in a 16:9 recording: its middle (Fill), or all of it with bars (Fit).
        let four_three = r#""screen_crop":{"w":1440,"h":1080,"at":[240,0]},"shape":[16,9]"#;
        assert_eq!(load(&format!(r#"{{{four_three},"resolution":"P1080"}}"#)), OutputResolution::Exact(1440, 810));
        assert_eq!(load(&format!(r#"{{{four_three},"resolution":"P1080","framing":"fit"}}"#)), OutputResolution::Exact(1920, 1080));
        assert_eq!(load(&format!(r#"{{{crop},"resolution":"Native","capture":{{"type":"apps","apps":[]}}}}"#)), OutputResolution::Native);
        assert_eq!(load(&format!(r#"{{{crop},"resolution":{{"Exact":[1920,1080]}},"size_version":1}}"#)), OutputResolution::Exact(1920, 1080));
        assert_eq!(load(r#"{"resolution":"Native"}"#), OutputResolution::Native);
    }

    /// A crop saved by this version is only what's shown of the screen:
    /// loaded again and again, the size stays as chosen.
    #[test]
    fn a_crop_from_now_leaves_the_size_alone() {
        let s = RecordSettings { resolution: OutputResolution::P1080, screen_crop: Some(ScreenCrop { w: 2400, h: 1600, at: None }), ..Default::default() };
        let once = RecordSettings::from_saved(Some(s.to_json().as_bytes()));
        let twice = RecordSettings::from_saved(Some(once.to_json().as_bytes()));
        assert_eq!((once.resolution, twice.resolution), (OutputResolution::P1080, OutputResolution::P1080));
    }

    /// Saved and read again, the size is as it was.
    #[test]
    fn the_size_is_saved_as_it_is() {
        let sizes = [
            (OutputResolution::Exact(1080, 1350), Some([4, 3])),
            (OutputResolution::P1080, Some([9, 16])),
            (OutputResolution::Custom(608), Some([9, 16])),
            (OutputResolution::P720, None),
            (OutputResolution::Native, Some([21, 9])),
        ];
        for (resolution, shape) in sizes {
            let s = RecordSettings { resolution, shape, ..Default::default() };
            let back = RecordSettings::from_json(s.to_json().as_bytes()).unwrap();
            assert_eq!((back.resolution, back.shape), (resolution, shape));
        }
    }

    /// What's saved, read the way the version before a size of its own
    /// reads it (its own settings, in miniature): every setting is there
    /// (one value it doesn't know would lose them all), at about the size,
    /// p being the height there.
    #[test]
    fn older_versions_read_what_is_saved() {
        #[derive(Debug, PartialEq, Deserialize)]
        enum Old {
            Native,
            P480,
            P720,
            P900,
            P1080,
            P1440,
            P2160,
            Custom(u32),
        }
        #[derive(Deserialize)]
        struct Older {
            resolution: Old,
            shape: Option<[u32; 2]>,
            fps: u32,
        }
        let read = |resolution, shape| {
            let s = RecordSettings { resolution, shape, fps: 120, ..Default::default() };
            let old: Older = serde_json::from_str(&s.to_json()).expect("an older version reads it");
            assert_eq!(old.fps, 120);
            (old.resolution, old.shape)
        };
        assert_eq!(read(OutputResolution::P1080, None), (Old::P1080, None));
        assert_eq!(read(OutputResolution::P1080, Some([9, 16])), (Old::Custom(1920), Some([9, 16])));
        assert_eq!(read(OutputResolution::Custom(608), Some([9, 16])), (Old::P1080, Some([9, 16])));
        assert_eq!(read(OutputResolution::Exact(1080, 1350), None), (Old::Custom(1350), Some([4, 5])));
        assert_eq!(read(OutputResolution::Exact(1920, 1080), Some([1, 1])), (Old::P1080, Some([16, 9])));
        assert_eq!(read(OutputResolution::Exact(1366, 768), None).1, Some([16, 9]));
        // Saved again by an older version (without the size as it is), it's
        // moved over: the same size.
        for (resolution, shape) in [(OutputResolution::P1080, Some([9, 16])), (OutputResolution::Custom(608), Some([9, 16]))] {
            let s = RecordSettings { resolution, shape, ..Default::default() };
            let mut json: serde_json::Value = serde_json::from_str(&s.to_json()).unwrap();
            json.as_object_mut().unwrap().remove("video_size");
            let back = RecordSettings::from_json(json.to_string().as_bytes()).unwrap();
            assert_eq!((back.resolution, back.shape), (resolution, shape));
        }
    }
}

#[cfg(test)]
mod noise_reduction_tests {
    use super::*;

    /// Noise removal that was on is Strong, and the old switch isn't written back.
    #[test]
    fn old_noise_removal_is_strong() {
        let mut s = RecordSettings::default();
        let old = |on: bool| serde_json::from_str(&format!(r#"{{"id":"a","name":"Mic","kind":{{"type":"microphone","device":"default"}},"noise_removal":{on}}}"#)).unwrap();
        s.audio_sources = vec![old(true), old(false)];
        s.upgrade_noise_reduction();
        assert_eq!(s.audio_sources[0].noise_reduction, NoiseReduction::Strong);
        assert_eq!(s.audio_sources[1].noise_reduction, NoiseReduction::Off);
        assert!(!s.to_json().contains("noise_removal"));
        assert!(s.to_json().contains(r#""noise_reduction": "strong""#));
    }
}

#[cfg(test)]
mod placement_tests {
    use super::{BoxCfg, PlacementCfg};

    fn cfg(x: f32, y: f32, w: f32, h: f32, frame_aspect: Option<f32>) -> PlacementCfg {
        PlacementCfg { x, y, w, h, crop: [0.0; 4], flip_h: false, flip_v: false, turns: 0, frame_aspect, stretch: 1.0 }
    }

    /// A stretched webcam stays as stretched: twice as wide as its 16:9
    /// camera is left alone, and a 4:3 camera gets a box twice as wide as
    /// 4:3; saved, the stretch is kept, and a file without it is unstretched.
    #[test]
    fn a_stretched_webcam_stays_stretched() {
        let wide = 16.0 / 9.0;
        let pixels = |p: &PlacementCfg| p.w * wide / p.h;
        let mut p = PlacementCfg { stretch: 2.0, ..cfg(0.5, 0.5, 0.5, 0.25, Some(wide)) };
        assert!(!p.refit(wide, Some(wide)), "{p:?}");
        assert!(p.refit(wide, Some(4.0 / 3.0)));
        assert!((pixels(&p) - 8.0 / 3.0).abs() < 1e-3, "{p:?}");
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<PlacementCfg>(&json).unwrap().stretch, 2.0);
        let plain = serde_json::to_string(&cfg(0.0, 0.0, 0.25, 0.25, None)).unwrap();
        assert!(!plain.contains("stretch") && serde_json::from_str::<PlacementCfg>(&plain).unwrap().stretch == 1.0);
    }

    /// The screen's box keeps its shape in pixels when the video changes
    /// shape, in its corner; the same video leaves it alone.
    #[test]
    fn the_screens_box_keeps_its_shape() {
        let wide = 16.0 / 9.0;
        let mut b = BoxCfg::new([0.6, 0.0, 0.4, 0.4], wide, Some(wide));
        assert!(!b.refit(wide, Some(wide)));
        assert!(b.refit(9.0 / 16.0, Some(wide)));
        let pixels = |b: &BoxCfg, frame: f32| b.w * frame / b.h;
        assert!((pixels(&b, 9.0 / 16.0) - 16.0 / 9.0).abs() < 1e-4, "{b:?}");
        assert!((b.x + b.w - 1.0).abs() < 1e-4 && b.y.abs() < 1e-4, "still top right: {b:?}");
        assert!(b.h <= 1.0 && b.w <= 1.0);
        // Another screen (an ultrawide): its shape, the box's width and
        // place kept.
        let mut b = BoxCfg::new([0.6, 0.0, 0.4, 0.4], wide, Some(wide));
        assert!(b.refit(wide, Some(21.0 / 9.0)));
        assert!((pixels(&b, wide) - 21.0 / 9.0).abs() < 1e-4 && (b.w - 0.4).abs() < 1e-4, "{b:?}");
        assert_eq!(b.picture_aspect, Some(21.0 / 9.0));
    }

    /// A box stretched by hand stays stretched: in a video of another shape,
    /// and for another screen (as much wider as that screen is). One from
    /// before boxes could stretch is given the screen's shape, once.
    #[test]
    fn a_stretched_box_stays_stretched() {
        let (wide, mac) = (16.0 / 9.0, 3024.0 / 1964.0);
        let pixels = |b: &BoxCfg, frame: f32| b.w * frame / b.h;
        // A 16:10 screen stretched over all of a 16:9 video.
        let mut b = BoxCfg::new([0.0, 0.0, 1.0, 1.0], wide, Some(mac));
        assert!(!b.refit(wide, Some(mac)));
        assert_eq!(b.rect(), [0.0, 0.0, 1.0, 1.0]);
        // The video goes 4:3: the box keeps its own shape (as wide as the video).
        assert!(b.refit(4.0 / 3.0, Some(mac)));
        assert!((pixels(&b, 4.0 / 3.0) - wide).abs() < 1e-4 && (b.w - 1.0).abs() < 1e-4, "{b:?}");
        // An ultrawide screen instead: as stretched, wider.
        let mut b = BoxCfg::new([0.0, 0.0, 1.0, 1.0], wide, Some(mac));
        assert!(b.refit(wide, Some(21.0 / 9.0)));
        assert!((pixels(&b, wide) - wide * (21.0 / 9.0) / mac).abs() < 1e-3, "{b:?}");
        // From before: the screen's shape, and then it's known.
        let mut old = BoxCfg::new([0.0, 0.0, 1.0, 1.0], wide, None);
        assert!(old.refit(wide, Some(mac)));
        assert!((pixels(&old, wide) - mac).abs() < 1e-4 && old.picture_aspect == Some(mac), "{old:?}");
        assert!(!old.refit(wide, Some(mac)));
    }

    /// A screen bigger than the video (its middle covering it) keeps
    /// covering as much of it in a video of another shape, its share of the
    /// width kept; one in the video stays in it.
    #[test]
    fn a_box_past_the_video() {
        let (wide, mac) = (16.0 / 9.0, 3024.0 / 1964.0);
        // Covering a 16:9 video, moved to its top.
        let mut b = BoxCfg::new([0.0, 0.0, 1.0, wide / mac], wide, Some(mac));
        assert!(b.refit(21.0 / 9.0, Some(mac)));
        let [x, y, w, h] = b.rect();
        assert!(x.abs() < 1e-4 && (w - 1.0).abs() < 1e-4 && (h - (21.0 / 9.0) / mac).abs() < 1e-4, "{b:?}");
        assert!(y.abs() < 1e-4, "still at its top: {b:?}");
        // Upright, it's narrower than tall: as wide as the video, in it.
        assert!(b.refit(9.0 / 16.0, Some(mac)));
        assert!(b.y >= -1e-4 && b.y + b.h <= 1.0 + 1e-4 && (b.w - 1.0).abs() < 1e-4, "{b:?}");
    }

    fn pixel_aspect(p: &PlacementCfg, frame: f32) -> f32 {
        p.w * frame / p.h
    }

    #[test]
    fn same_frame_shape_leaves_it_alone() {
        let mut p = cfg(0.7, 0.7, 0.25, 0.25, Some(16.0 / 9.0));
        assert!(!p.refit(16.0 / 9.0, Some(16.0 / 9.0)));
    }

    /// Where it is in the room it has to move in, across and down.
    fn along(p: &PlacementCfg) -> (f32, f32) {
        (p.x / (1.0 - p.w), p.y / (1.0 - p.h))
    }

    fn inside(p: &PlacementCfg) -> bool {
        p.x >= -1e-6 && p.y >= -1e-6 && p.x + p.w <= 1.0 + 1e-6 && p.y + p.h <= 1.0 + 1e-6
    }

    #[test]
    fn moving_to_an_ultrawide_keeps_its_shape() {
        // A 16:9 box on a 16:9 display, then a 6720x2836 ultrawide.
        let mut p = cfg(0.7, 0.7, 0.25, 0.25, Some(16.0 / 9.0));
        let (ax, ay) = along(&p);
        let uw = 6720.0 / 2836.0;
        assert!(p.refit(uw, None));
        assert!((pixel_aspect(&p, uw) - 16.0 / 9.0).abs() < 1e-4);
        let (bx, by) = along(&p);
        assert!((bx - ax).abs() < 1e-5 && (by - ay).abs() < 1e-5);
        assert!(inside(&p));
        assert_eq!(p.frame_aspect, Some(uw));
    }

    #[test]
    fn a_box_in_a_corner_stays_there_on_a_turned_display() {
        // Bottom right on a 16:9 display, then a portrait one, then back.
        let start = cfg(0.75, 0.75, 0.25, 0.25, Some(16.0 / 9.0));
        let mut p = start;
        assert!(p.refit(9.0 / 16.0, Some(16.0 / 9.0)));
        assert!((p.x + p.w - 1.0).abs() < 1e-5 && (p.y + p.h - 1.0).abs() < 1e-5);
        assert!((pixel_aspect(&p, 9.0 / 16.0) - 16.0 / 9.0).abs() < 1e-4);
        assert!(p.refit(16.0 / 9.0, Some(16.0 / 9.0)));
        for (a, b) in [(p.x, start.x), (p.y, start.y), (p.w, start.w), (p.h, start.h)] {
            assert!((a - b).abs() < 1e-5, "{p:?}");
        }
    }

    #[test]
    fn a_squashed_box_gets_the_cameras_shape_back() {
        // The saved box from the bug report: a thin strip of a cropped 16:9
        // camera, squashed by a frame that changed shape under it. On the
        // display it was laid out for, and on another.
        let squashed = PlacementCfg { crop: [0.137, 0.180, 0.105, 0.051], ..cfg(0.4073, 0.4962, 0.2372, 0.00762, Some(0.5625)) };
        let visible = 16.0 / 9.0 * (1.0 - 0.137 - 0.105) / (1.0 - 0.180 - 0.051);
        for frame in [0.5625, 16.0 / 9.0] {
            let mut p = squashed;
            assert!(p.refit(frame, Some(16.0 / 9.0)));
            assert!((pixel_aspect(&p, frame) - visible).abs() < 1e-3, "{frame}: {p:?}");
            assert!((p.w - squashed.w).abs() < 1e-6 && inside(&p), "{frame}: {p:?}");
            assert!(!p.refit(frame, Some(16.0 / 9.0)), "then it's left alone");
        }
    }

    #[test]
    fn rounding_isnt_squashing() {
        // A 16:9 box laid out on a preview a pixel off the frame's shape.
        let mut p = cfg(0.7, 0.7, 0.25, 0.25 * 1.005, Some(16.0 / 9.0));
        assert!(!p.refit(16.0 / 9.0, Some(16.0 / 9.0)));
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

    /// Presets go up to the display's refresh rate (it included), and to 240
    /// on slower or unknown displays.
    #[test]
    fn fps_presets_follow_the_display() {
        assert_eq!(fps_choices(Some(540)), [30, 60, 120, 144, 165, 240, 360, 480, 540]);
        assert_eq!(fps_choices(Some(175)), [30, 60, 120, 144, 165, 175, 240]);
        assert_eq!(fps_choices(Some(60)), [30, 60, 120, 144, 165, 240]);
        assert_eq!(fps_choices(None), [30, 60, 120, 144, 165, 240]);
    }

    #[test]
    fn blend_rates_go_to_sixteen_times_within_the_limit() {
        let at60 = blend_choices(60, None);
        assert_eq!((at60.first(), at60.last(), at60.len()), (Some(&120), Some(&960), 15));
        assert_eq!(blend_choices(300, None).last(), Some(&1800));
        // macOS: no faster than the display.
        assert_eq!(blend_choices(60, Some(120)), [120]);
    }

    #[test]
    fn a_webcam_from_before_the_switch_is_on() {
        let s: RecordSettings = serde_json::from_str(
            r#"{"webcam": {"id": "cam", "name": "Cam", "placement": {"x": 0.7, "y": 0.7, "w": 0.25, "h": 0.25}}}"#,
        )
        .unwrap();
        assert!(s.webcam.unwrap().enabled);
    }

    /// The one webcam of older settings becomes the first of the list, in
    /// front of the screen or behind it as it was; saved, older versions
    /// read their one webcam and which is in front back.
    #[test]
    fn one_webcam_becomes_a_list_and_back() {
        let cam = r#""webcam": {"id": "cam", "name": "Cam", "placement": {"x": 0.7, "y": 0.7, "w": 0.25, "h": 0.25}}"#;
        let behind = RecordSettings::from_saved(Some(format!("{{{cam}}}").as_bytes()));
        assert!(behind.webcam.is_none());
        assert_eq!(behind.webcams.len(), 1);
        let key = behind.webcams[0].key;
        assert!(key != 0);
        assert_eq!(behind.layer_order(), vec![LayerKey::Webcam(key), LayerKey::Screen]);
        let front = RecordSettings::from_saved(Some(format!(r#"{{{cam}, "screen": {{"in_front": true}}}}"#).as_bytes()));
        assert_eq!(front.layer_order(), vec![LayerKey::Screen, LayerKey::Webcam(front.webcams[0].key)]);
        // Saved: the first webcam (the one in front) where older versions look.
        let mut two = behind.clone();
        two.webcams.push(WebcamCfg { key: 9, id: "other".into(), ..two.webcams[0].clone() });
        two.layer_order = vec![LayerKey::Screen, LayerKey::Webcam(9), LayerKey::Webcam(key)];
        let old: serde_json::Value = serde_json::from_str(&two.to_json()).unwrap();
        assert_eq!(old["webcam"]["id"], "other");
        assert_eq!(old["screen"]["in_front"], true);
        // And read back as it was.
        let again = RecordSettings::from_saved(Some(two.to_json().as_bytes()));
        assert_eq!(again.webcams.len(), 2);
        assert_eq!(again.layer_order(), two.layer_order());
    }

    /// The order is the scene shown's: each source once, gone ones left out.
    /// New settings start with the screen; one of the preset's the scene
    /// doesn't have (the screen too) isn't in it.
    #[test]
    fn the_order_keeps_up_with_the_sources() {
        let mut s = RecordSettings::from_saved(None);
        assert_eq!(s.layer_order(), vec![LayerKey::Screen]);
        s.visuals.push(VisualCfg::new(3, Visual::Display { id: "d".into(), name: "D".into() }));
        s.layer_order = vec![LayerKey::Webcam(7), LayerKey::Screen];
        assert_eq!(s.layer_order(), vec![LayerKey::Screen]);
        assert_eq!(s.outside_scene(), vec![LayerKey::Visual(3)]);
        assert_eq!(s.new_key(), 4);
        s.remove_layer(LayerKey::Visual(3));
        assert!(s.outside_scene().is_empty());
        assert!(s.screen_in_front());
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

#[cfg(test)]
mod scene_tests {
    use super::*;

    fn cam(key: u64, id: &str) -> WebcamCfg {
        let placement = PlacementCfg { x: 0.7, y: 0.7, w: 0.25, h: 0.25, crop: [0.0; 4], flip_h: false, flip_v: false, turns: 0, frame_aspect: Some(16.0 / 9.0), stretch: 1.0 };
        WebcamCfg { key, id: id.into(), name: id.into(), title: None, format: None, placement, enabled: true }
    }

    /// A webcam and an image in front of the screen, in the one scene.
    fn setup() -> RecordSettings {
        let mut s = RecordSettings::default();
        s.webcams.push(cam(1, "cam"));
        s.visuals.push(VisualCfg::new(2, Visual::Image { path: "logo.png".into() }));
        s.layer_order = vec![LayerKey::Webcam(1), LayerKey::Visual(2), LayerKey::Screen];
        s
    }

    fn placed(x: f32) -> Option<BoxCfg> {
        Some(BoxCfg::new([x, 0.1, 0.2, 0.2], 16.0 / 9.0, None))
    }

    #[test]
    fn settings_from_before_scenes_become_one_preset_with_one_scene() {
        // Sources, and an order without the colour (as before scenes).
        let json = r#"{"webcams": [{"key": 1, "id": "cam", "name": "Cam", "placement": {"x": 0.7, "y": 0.7, "w": 0.25, "h": 0.25}}],
            "visuals": [{"key": 2, "type": "colour", "rgba": [1, 2, 3, 255]}],
            "layer_order": ["screen", {"webcam": 1}]}"#;
        let s = RecordSettings::from_saved(Some(json.as_bytes()));
        assert_eq!((s.presets.len(), s.scenes.len()), (1, 1));
        assert_eq!(s.preset().map(|p| (p.name.as_str(), p.setup.is_none())), Some(("Default", true)));
        assert_eq!(s.scene().map(|c| (c.name.as_str(), c.items.len())), Some(("Scene 1", 0)));
        // Every source showing, as they did then: the one not in the order in front.
        assert_eq!(s.layer_order(), vec![LayerKey::Visual(2), LayerKey::Screen, LayerKey::Webcam(1)]);
        assert!(s.outside_scene().is_empty());
        let again = RecordSettings::from_saved(Some(s.to_json().as_bytes()));
        assert_eq!(again.to_json(), s.to_json());
    }

    #[test]
    fn scenes_and_presets_are_saved_and_read_back() {
        let mut s = setup();
        let b = s.add_scene(false);
        s.rename_scene(b, "BRB");
        s.add_preset("Streaming", true);
        let json = s.to_json();
        let back = RecordSettings::from_saved(Some(json.as_bytes()));
        assert_eq!(back.scenes, s.scenes);
        assert_eq!(back.active_scene, s.active_scene);
        let presets = |s: &RecordSettings| s.presets.iter().map(|p| (p.id, p.name.clone(), p.setup.is_some())).collect::<Vec<_>>();
        assert_eq!(presets(&back), presets(&s));
        assert_eq!(back.to_json(), json);
    }

    /// A version from before scenes reads the scene shown: its one webcam
    /// there, and the screen's place.
    #[test]
    fn older_versions_read_the_scene_shown() {
        let mut s = setup();
        let b = s.add_scene(false);
        s.switch_scene(b);
        let old: serde_json::Value = serde_json::from_str(&s.to_json()).unwrap();
        // No webcam in this scene: the preset's, off, so it isn't lost there.
        assert_eq!(old["webcam"]["id"], "cam");
        assert_eq!(old["webcam"]["enabled"], false);
        assert_eq!(old["screen"]["enabled"], true);
    }

    #[test]
    fn switching_scenes_keeps_each_ones_placements() {
        let mut s = setup();
        s.screen.placed = placed(0.1);
        let a = s.active_scene;
        let b = s.duplicate_scene(a).unwrap();
        assert!(s.switch_scene(b));
        // In B: the screen elsewhere, the image hidden, the webcam behind
        // and moved.
        s.screen.placed = placed(0.5);
        s.visual_mut(2).unwrap().enabled = false;
        s.layer_order = vec![LayerKey::Visual(2), LayerKey::Screen, LayerKey::Webcam(1)];
        s.webcam_mut(1).unwrap().placement.x = 0.05;
        // A source's own settings are the same in every scene.
        s.webcam_mut(1).unwrap().title = Some("Face".into());
        assert!(s.switch_scene(a));
        assert_eq!(s.screen.placed, placed(0.1));
        assert!(s.visual(2).unwrap().enabled);
        assert_eq!(s.layer_order(), vec![LayerKey::Webcam(1), LayerKey::Visual(2), LayerKey::Screen]);
        assert_eq!(s.webcam(1).unwrap().placement.x, 0.7);
        assert_eq!(s.webcam(1).unwrap().title.as_deref(), Some("Face"));
        // And B as it was left.
        assert!(s.switch_scene(b));
        assert_eq!(s.screen.placed, placed(0.5));
        assert!(!s.visual(2).unwrap().enabled);
        assert_eq!(s.layer_order(), vec![LayerKey::Visual(2), LayerKey::Screen, LayerKey::Webcam(1)]);
        assert_eq!(s.webcam(1).unwrap().placement.x, 0.05);
        assert!(!s.switch_scene(b), "shown already");
        assert!(!s.switch_scene(999), "no such scene");
    }

    #[test]
    fn a_source_another_scene_has_is_hidden_here_not_gone() {
        let mut s = setup();
        let b = s.add_scene(false);
        s.switch_scene(b);
        assert_eq!(s.layer_order(), vec![LayerKey::Screen]);
        assert_eq!(s.outside_scene(), vec![LayerKey::Webcam(1), LayerKey::Visual(2)]);
        assert!(s.webcam(1).is_some() && s.visual(2).is_some());
    }

    #[test]
    fn taking_out_keeps_what_another_scene_has() {
        let mut s = setup();
        let a = s.active_scene;
        let b = s.duplicate_scene(a).unwrap();
        // Out of A, but B has it: still the preset's.
        assert!(!s.remove_from_scene(LayerKey::Webcam(1)));
        assert!(s.webcam(1).is_some());
        assert_eq!(s.outside_scene(), vec![LayerKey::Webcam(1)]);
        // Out of both: gone.
        assert!(!s.remove_from_scene(LayerKey::Visual(2)));
        s.switch_scene(b);
        assert!(s.remove_from_scene(LayerKey::Visual(2)));
        assert!(s.visual(2).is_none());
        // The screen: out of the scene, never gone.
        assert!(!s.remove_from_scene(LayerKey::Screen));
        assert!(!s.layer_order().contains(&LayerKey::Screen));
        assert_eq!(s.outside_scene(), vec![LayerKey::Screen]);
    }

    #[test]
    fn putting_in_from_another_scene_shares_the_source() {
        let mut s = setup();
        s.webcam_mut(1).unwrap().enabled = false;
        let b = s.add_scene(false);
        s.switch_scene(b);
        assert!(s.add_to_scene(LayerKey::Webcam(1)));
        // The same webcam, where it was last placed, in front, showing.
        assert_eq!(s.webcams.len(), 1);
        assert_eq!(s.layer_order(), vec![LayerKey::Webcam(1), LayerKey::Screen]);
        assert_eq!(s.webcam(1).unwrap().placement.x, 0.7);
        assert!(s.webcam(1).unwrap().enabled);
        assert!(!s.add_to_scene(LayerKey::Webcam(1)), "there already");
        assert!(!s.add_to_scene(LayerKey::Webcam(9)), "no such source");
    }

    #[test]
    fn sources_no_scene_has_go_with_the_last_scene_they_were_in() {
        let mut s = setup();
        let a = s.active_scene;
        let b = s.add_scene(false);
        s.switch_scene(b);
        assert!(s.delete_scene(a));
        assert!(s.webcams.is_empty() && s.visuals.is_empty());
        assert_eq!(s.scenes.len(), 1);
        assert!(!s.delete_scene(b), "never the last");
        // Deleting the one shown shows another.
        let c = s.add_scene(false);
        assert!(s.delete_scene(b));
        assert_eq!(s.active_scene, c);
    }

    #[test]
    fn new_keys_are_no_other_presets() {
        let mut s = setup();
        let p = s.add_preset("Other", false);
        s.presets.iter_mut().find(|x| x.id == p).unwrap().setup.as_mut().unwrap().webcams.push(cam(50, "x"));
        assert_eq!(s.new_key(), 51);
    }

    #[test]
    fn cameras_open_are_those_a_scene_shows() {
        let mut s = setup();
        let a = s.active_scene;
        let ids = |s: &RecordSettings| s.cameras_in_use().iter().map(|w| w.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&s), ["cam"]);
        let b = s.add_scene(false);
        s.switch_scene(b);
        assert_eq!(ids(&s), ["cam"], "A shows it: kept open for going back");
        s.switch_scene(a);
        s.webcam_mut(1).unwrap().enabled = false;
        assert!(ids(&s).is_empty(), "hidden in every scene: closed");
    }

    #[test]
    fn switching_presets_and_back() {
        let mut s = setup();
        s.fps = 144;
        let first = s.active_preset;
        let json = s.to_json();
        let other = s.add_preset("Clipping", false);
        assert!(s.switch_preset(other));
        // Its own: the screen, a mic and the computer's sound, one scene.
        assert!(s.webcams.is_empty() && s.visuals.is_empty());
        assert_eq!((s.audio_sources.len(), s.scenes.len()), (2, 1));
        assert_eq!(s.fps, 144, "output settings are every preset's");
        assert!(s.preset().unwrap().setup.is_none());
        assert!(s.switch_preset(first));
        let without_presets = |j: &str| {
            let mut v: serde_json::Value = serde_json::from_str(j).unwrap();
            v.as_object_mut().unwrap().remove("presets");
            v
        };
        assert_eq!(without_presets(&s.to_json()), without_presets(&json));
        assert!(!s.switch_preset(first), "in use already");
        assert!(!s.switch_preset(999), "no such preset");
    }

    #[test]
    fn a_copied_preset_has_sources_of_its_own() {
        let mut s = setup();
        s.add_scene(true);
        let p = s.add_preset("Streaming", true);
        let setup = s.presets.iter().find(|x| x.id == p).unwrap().setup.clone().unwrap();
        let key = setup.webcams[0].key;
        assert_ne!(key, 1);
        assert_ne!(setup.audio_sources[0].id, s.audio_sources[0].id);
        assert!(setup.layer_order.contains(&LayerKey::Webcam(key)));
        let stored = setup.scenes.iter().find(|c| c.id != setup.active_scene).unwrap();
        assert!(stored.items.iter().any(|i| i.layer() == LayerKey::Webcam(key)));
        assert!(setup.scenes.iter().all(|c| !s.scenes.iter().any(|x| x.id == c.id)), "scene ids of its own");
    }

    #[test]
    fn a_damaged_file_is_put_right() {
        let mut s = setup();
        s.add_scene(true);
        let mut v: serde_json::Value = serde_json::from_str(&s.to_json()).unwrap();
        // Two scenes with one id, the one shown not there, an item of a
        // source that isn't, one this version can't read, and a preset not
        // in use without a setup.
        let scenes = v["scenes"].as_array_mut().unwrap();
        scenes[1]["id"] = scenes[0]["id"].clone();
        let items = scenes[1]["items"].as_array_mut().unwrap();
        items.push(serde_json::json!({"source": "webcam", "key": 77, "placement": {"x": 0, "y": 0, "w": 0.1, "h": 0.1}}));
        items.push(serde_json::json!({"source": "hologram"}));
        v["active_scene"] = serde_json::json!(4242);
        v["presets"] = serde_json::json!([{"id": 5, "name": "Ghost"}]);
        let fixed = RecordSettings::from_saved(Some(v.to_string().as_bytes()));
        assert_eq!(fixed.fps, s.fps, "the rest of the settings kept");
        assert_eq!(fixed.webcams.len(), 1);
        let ids: Vec<u64> = fixed.scenes.iter().map(|c| c.id).collect();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]);
        assert!(fixed.scene().is_some_and(|c| c.items.is_empty()));
        for c in fixed.scenes.iter().filter(|c| c.id != fixed.active_scene) {
            assert!(c.items.iter().all(|i| i.layer() != LayerKey::Webcam(77)));
            assert!(c.items.iter().any(|i| i.layer() == LayerKey::Screen));
        }
        assert_eq!(fixed.presets.len(), 1);
        assert!(fixed.preset().is_some_and(|p| p.setup.is_none()));
    }

    /// Copied to another scene, a webcam's there as it's here (its crop
    /// too), in front of what's behind it here; then each scene's place is
    /// its own, and the camera's settings are shared.
    #[test]
    fn a_source_copied_to_another_scene_is_placed_as_here() {
        let mut s = setup();
        let a = s.active_scene;
        if let Some(w) = s.webcam_mut(1) {
            w.placement.crop = [0.1, 0.0, 0.2, 0.05];
            w.placement.x = 0.05;
        }
        let b = s.add_scene(false);
        assert!(s.copy_to_scene(LayerKey::Webcam(1), b));
        assert!(!s.copy_to_scene(LayerKey::Webcam(1), a), "not into the scene shown");
        assert_eq!(s.scenes_with(LayerKey::Webcam(1)), vec![(b, s.scenes[1].name.clone())]);
        let here = s.webcam(1).map(|w| w.placement);
        s.switch_scene(b);
        assert_eq!(s.layer_order(), vec![LayerKey::Webcam(1), LayerKey::Screen], "in front of the screen, as there");
        assert_eq!(s.webcam(1).map(|w| w.placement), here);
        // Moved here, it stays where it was there; another resolution is both's.
        if let Some(w) = s.webcam_mut(1) {
            w.placement.x = 0.6;
            w.format = Some(FormatCfg { width: 1280, height: 720, fps_num: 30, fps_den: 1 });
        }
        s.switch_scene(a);
        assert_eq!(s.webcam(1).map(|w| w.placement), here);
        assert!(s.webcam(1).is_some_and(|w| w.format.is_some()));
    }

    /// Copied to a scene that has it, it's placed there as here, where it
    /// was in that scene's order; one at the back here goes to the back.
    #[test]
    fn copying_to_a_scene_that_has_it_places_it_as_here() {
        let mut s = setup();
        let b = s.add_scene(true);
        s.visuals.push(VisualCfg::new(3, Visual::Colour { rgba: [0, 0, 0, 255] }));
        s.layer_order.push(LayerKey::Visual(3));
        if let Some(v) = s.visual_mut(2) {
            v.placed = placed(0.4);
        }
        assert!(s.copy_to_scene(LayerKey::Visual(2), b));
        assert!(s.copy_to_scene(LayerKey::Visual(3), b));
        s.switch_scene(b);
        assert_eq!(s.layer_order(), vec![LayerKey::Webcam(1), LayerKey::Visual(2), LayerKey::Screen, LayerKey::Visual(3)]);
        assert_eq!(s.visual(2).and_then(|v| v.placed), placed(0.4));
    }

    /// Unlinked, a source's the scene's own: a copy in its place, changed
    /// there alone; the other scenes keep the one they share. The screen
    /// and a webcam stay linked, and one in no other scene has nothing to
    /// unlink from.
    #[test]
    fn an_unlinked_source_is_the_scenes_own() {
        let mut s = setup();
        let a = s.active_scene;
        assert_eq!(s.unlink(LayerKey::Visual(2)), None, "in no other scene");
        let b = s.add_scene(true);
        assert_eq!(s.unlink(LayerKey::Screen), None);
        assert_eq!(s.unlink(LayerKey::Webcam(1)), None);
        let Some(LayerKey::Visual(own)) = s.unlink(LayerKey::Visual(2)) else { panic!("unlinked") };
        assert_ne!(own, 2);
        assert_eq!(s.layer_order(), vec![LayerKey::Webcam(1), LayerKey::Visual(own), LayerKey::Screen]);
        if let Some(v) = s.visual_mut(own) {
            v.visual = Visual::Image { path: "other.png".into() };
        }
        assert!(s.scenes_with(LayerKey::Visual(own)).is_empty());
        s.switch_scene(b);
        assert_eq!(s.layer_order(), vec![LayerKey::Webcam(1), LayerKey::Visual(2), LayerKey::Screen]);
        assert_eq!(s.visual(2).map(|v| v.visual.clone()), Some(Visual::Image { path: "logo.png".into() }));
        s.switch_scene(a);
        assert_eq!(s.visual(own).map(|v| v.visual.clone()), Some(Visual::Image { path: "other.png".into() }));
    }

    /// Locks are each scene's: switching away and back keeps them, a
    /// copied scene has them, one taken out is unlocked.
    #[test]
    fn locks_are_each_scenes_own() {
        let mut s = setup();
        let a = s.active_scene;
        s.set_locked(LayerKey::Visual(2), true);
        let b = s.add_scene(true);
        let c = s.add_scene(false);
        s.switch_scene(b);
        assert!(s.is_locked(LayerKey::Visual(2)), "the copy's locked too");
        s.set_locked(LayerKey::Visual(2), false);
        s.switch_scene(c);
        assert!(!s.is_locked(LayerKey::Visual(2)));
        s.switch_scene(a);
        assert!(s.is_locked(LayerKey::Visual(2)));
        let back = RecordSettings::from_saved(Some(s.to_json().as_bytes()));
        assert!(back.is_locked(LayerKey::Visual(2)));
        s.remove_from_scene(LayerKey::Visual(2));
        assert!(!s.is_locked(LayerKey::Visual(2)));
    }

    /// A group's sources are together where the one in front of them was,
    /// stay so when one moves, and leave it when they leave the scene; the
    /// group is each scene's, and copied to another scene with its sources.
    #[test]
    fn groups_keep_their_sources_together() {
        let mut s = setup();
        s.visuals.push(VisualCfg::new(3, Visual::Colour { rgba: [0, 0, 0, 255] }));
        s.layer_order = vec![LayerKey::Webcam(1), LayerKey::Visual(2), LayerKey::Screen, LayerKey::Visual(3)];
        let g = s.make_group(&[LayerKey::Webcam(1), LayerKey::Visual(3)]).unwrap();
        assert_eq!(s.layer_order(), vec![LayerKey::Webcam(1), LayerKey::Visual(3), LayerKey::Visual(2), LayerKey::Screen]);
        assert_eq!(s.group_of(LayerKey::Visual(3)).map(|x| x.id), Some(g));
        s.move_group(g, 1);
        assert_eq!(s.layer_order(), vec![LayerKey::Visual(2), LayerKey::Webcam(1), LayerKey::Visual(3), LayerKey::Screen]);
        s.set_group(LayerKey::Visual(2), Some(g));
        assert_eq!(s.group(g).map(|x| x.members.clone()), Some(vec![LayerKey::Webcam(1), LayerKey::Visual(3), LayerKey::Visual(2)]));
        let a = s.active_scene;
        let b = s.add_scene(false);
        assert!(s.copy_group_to_scene(g, b));
        s.remove_from_scene(LayerKey::Visual(3));
        assert_eq!(s.group(g).map(|x| x.members.len()), Some(2));
        s.switch_scene(b);
        assert_eq!(s.group(g).map(|x| x.members.len()), Some(3), "there it has all three");
        assert_eq!(&s.layer_order()[..3], &[LayerKey::Webcam(1), LayerKey::Visual(3), LayerKey::Visual(2)]);
        s.switch_scene(a);
        s.ungroup(g);
        assert!(s.groups.is_empty());
        let back = RecordSettings::from_saved(Some(s.to_json().as_bytes()));
        assert_eq!(back.scenes, s.scenes);
    }

    /// Sources moved in the scene's order go into a group, or out of one,
    /// with it; the group they were all of is gone.
    #[test]
    fn sources_move_into_a_group_and_out() {
        let mut s = setup();
        s.visuals.push(VisualCfg::new(3, Visual::Colour { rgba: [0, 0, 0, 255] }));
        s.layer_order = vec![LayerKey::Visual(3), LayerKey::Webcam(1), LayerKey::Visual(2), LayerKey::Screen];
        let g = s.make_group(&[LayerKey::Webcam(1), LayerKey::Visual(2)]).unwrap();
        let members = |s: &RecordSettings| s.group(g).map(|x| x.members.clone()).unwrap_or_default();
        // Into it, between its two (the others: webcam, logo, screen).
        s.move_layers_to(&[LayerKey::Visual(3)], 1, Some(g));
        assert_eq!(s.layer_order(), vec![LayerKey::Webcam(1), LayerKey::Visual(3), LayerKey::Visual(2), LayerKey::Screen]);
        assert_eq!(members(&s), vec![LayerKey::Webcam(1), LayerKey::Visual(3), LayerKey::Visual(2)]);
        // Out, behind it.
        s.move_layers_to(&[LayerKey::Webcam(1)], 2, None);
        assert_eq!(s.layer_order(), vec![LayerKey::Visual(3), LayerKey::Visual(2), LayerKey::Webcam(1), LayerKey::Screen]);
        assert_eq!(members(&s), vec![LayerKey::Visual(3), LayerKey::Visual(2)]);
        // Both out, where they are: the group's gone.
        s.move_layers_to(&[LayerKey::Visual(3), LayerKey::Visual(2)], 0, None);
        assert!(s.groups.is_empty());
        assert_eq!(s.layer_order(), vec![LayerKey::Visual(3), LayerKey::Visual(2), LayerKey::Webcam(1), LayerKey::Screen]);
    }

    /// A scene can do without the screen (a webcam alone, say): taken out
    /// it's hidden in the capture, never gone, and comes back from another
    /// scene; switching keeps a scene without it so.
    #[test]
    fn a_scene_can_do_without_the_screen() {
        let mut s = setup();
        let a = s.active_scene;
        let b = s.add_scene(true);
        assert!(!s.remove_from_scene(LayerKey::Screen), "never gone for good");
        assert_eq!(s.layer_order(), vec![LayerKey::Webcam(1), LayerKey::Visual(2)]);
        assert!(s.screen_placement().hidden);
        assert_eq!(s.outside_scene(), vec![LayerKey::Screen]);
        s.switch_scene(b);
        assert!(s.layer_order().contains(&LayerKey::Screen));
        assert!(!s.screen_placement().hidden);
        s.switch_scene(a);
        assert!(!s.layer_order().contains(&LayerKey::Screen), "still without it");
        let back = RecordSettings::from_saved(Some(s.to_json().as_bytes()));
        assert!(!back.layer_order().contains(&LayerKey::Screen));
        assert!(s.add_to_scene(LayerKey::Screen));
        assert!(!s.screen_placement().hidden);
    }

    /// Copied and pasted into another scene, sources are the same ones
    /// there, front first, placed as they were; pasted where they are
    /// already, a picture is a copy of its own, a little down and right, in
    /// front of the source selected, and the camera stays once.
    #[test]
    fn pasted_sources_go_in_as_they_were_copied() {
        let mut s = setup();
        let a = s.active_scene;
        if let Some(v) = s.visual_mut(2) {
            v.placed = placed(0.4);
        }
        let copied = s.copy_sources(&[LayerKey::Visual(2), LayerKey::Webcam(1)], None).unwrap();
        assert_eq!(copied.sources.len(), 2);
        let b = s.add_scene(false);
        s.switch_scene(b);
        let pasted = s.paste_sources(&copied, None, false, true, 16.0 / 9.0);
        assert_eq!(pasted.layers, vec![LayerKey::Webcam(1), LayerKey::Visual(2)]);
        assert_eq!(s.layer_order(), vec![LayerKey::Webcam(1), LayerKey::Visual(2), LayerKey::Screen]);
        assert_eq!(s.visual(2).and_then(|v| v.placed), placed(0.4));
        assert_eq!((s.webcams.len(), s.visuals.len()), (1, 1), "the same sources, shared");
        let again = s.paste_sources(&copied, Some(LayerKey::Screen), false, true, 16.0 / 9.0);
        assert_eq!(again.there, vec!["Webcam".to_owned()]);
        let [LayerKey::Visual(own)] = again.layers[..] else { panic!("{again:?}") };
        assert_ne!(own, 2);
        assert_eq!(s.layer_order(), vec![LayerKey::Webcam(1), LayerKey::Visual(2), LayerKey::Visual(own), LayerKey::Screen]);
        assert!(s.visual(own).and_then(|v| v.placed).is_some_and(|b| (b.x - 0.42).abs() < 1e-4));
        s.switch_scene(a);
        assert_eq!(s.layer_order(), vec![LayerKey::Webcam(1), LayerKey::Visual(2), LayerKey::Screen], "where they were copied from, as they were");
    }

    /// Cut (gone from the preset) or another preset's, a pasted source is
    /// one of its own, as it was; a webcam is the preset's of the same
    /// camera where it has one, else one of its own; where a scene shows
    /// one webcam, none goes beside another.
    #[test]
    fn a_source_cut_or_from_another_preset_pastes_as_its_own() {
        let mut s = setup();
        let copied = s.copy_sources(&[LayerKey::Webcam(1), LayerKey::Visual(2)], None).unwrap();
        assert!(s.remove_from_scene(LayerKey::Visual(2)), "cut: in no other scene, it's gone");
        let pasted = s.paste_sources(&copied, None, false, true, 16.0 / 9.0);
        assert_eq!(pasted.there, vec!["Webcam".to_owned()]);
        let [LayerKey::Visual(own)] = pasted.layers[..] else { panic!("{pasted:?}") };
        assert_eq!(s.visual(own).map(|v| v.visual.clone()), Some(Visual::Image { path: "logo.png".into() }));
        let p = s.add_preset("Copy", true);
        s.switch_preset(p);
        let cams = s.webcams.len();
        let pasted = s.paste_sources(&copied, None, false, true, 16.0 / 9.0);
        assert_eq!(pasted.there.len(), 1, "its own of that camera is in the scene");
        assert_eq!(s.webcams.len(), cams);
        let q = s.add_preset("Fresh", false);
        s.switch_preset(q);
        assert!(s.webcams.is_empty());
        let pasted = s.paste_sources(&copied, None, false, true, 16.0 / 9.0);
        assert_eq!(pasted.layers.len(), 2);
        assert!(s.webcams.len() == 1 && s.webcams[0].key != 1 && s.webcams[0].id == "cam");
        let other = CopiedSources { sources: vec![CopiedSource::Webcam(cam(1, "usb"))], group: None };
        assert_eq!(s.paste_sources(&other, None, false, false, 16.0 / 9.0).full, vec!["Webcam".to_owned()]);
    }

    /// A copied group pastes as a group of its name, in front of the group
    /// of the source selected.
    #[test]
    fn a_copied_group_pastes_as_a_group() {
        let mut s = setup();
        let g = s.make_group(&[LayerKey::Webcam(1), LayerKey::Visual(2)]).unwrap();
        if let Some(x) = s.group_mut(g) {
            x.name = "Cam and logo".into();
        }
        let copied = s.copy_sources(&[LayerKey::Webcam(1), LayerKey::Visual(2)], Some(g)).unwrap();
        let b = s.add_scene(false);
        s.switch_scene(b);
        s.visuals.push(VisualCfg::new(3, Visual::Colour { rgba: [0, 0, 0, 255] }));
        s.layer_order.insert(0, LayerKey::Visual(3));
        let h = s.make_group(&[LayerKey::Visual(3), LayerKey::Screen]).unwrap();
        let pasted = s.paste_sources(&copied, Some(LayerKey::Screen), false, true, 16.0 / 9.0);
        let id = pasted.group.unwrap();
        assert_ne!(id, h);
        assert_eq!(s.group(id).map(|x| (x.name.clone(), x.members.clone())), Some(("Cam and logo".to_owned(), vec![LayerKey::Webcam(1), LayerKey::Visual(2)])));
        assert_eq!(s.layer_order(), vec![LayerKey::Webcam(1), LayerKey::Visual(2), LayerKey::Visual(3), LayerKey::Screen], "in front of the other group, all of it");
    }

    /// A video with sound has a sound source of its own, once, named as it
    /// is; it goes with its video, with its sound turned off, and isn't
    /// there for a video without sound.
    #[test]
    fn a_videos_sound_is_a_source_while_the_video_has_it() {
        let mut s = setup();
        s.visuals.push(VisualCfg::new(5, Visual::Video { path: "clips/intro.mp4".into(), looped: true, restart: true, sound: true }));
        s.layer_order.insert(0, LayerKey::Visual(5));
        let video = |s: &RecordSettings| s.audio_sources.iter().filter(|a| a.kind == SourceKind::Video { key: 5 }).map(|a| (a.id.clone(), a.name.clone())).collect::<Vec<_>>();
        s.sync_video_sounds(|_| false);
        assert!(video(&s).is_empty(), "its file has none");
        s.sync_video_sounds(|_| true);
        s.sync_video_sounds(|_| true);
        assert_eq!(video(&s), vec![("video-5".to_owned(), "intro".to_owned())]);
        if let Some(v) = s.visual_mut(5) {
            v.title = Some("Intro".into());
        }
        s.sync_video_sounds(|_| true);
        assert_eq!(video(&s)[0].1, "Intro");
        if let Some(Visual::Video { sound, .. }) = s.visual_mut(5).map(|v| &mut v.visual) {
            *sound = false;
        }
        s.sync_video_sounds(|_| true);
        assert!(video(&s).is_empty(), "its sound off");
        if let Some(Visual::Video { sound, .. }) = s.visual_mut(5).map(|v| &mut v.visual) {
            *sound = true;
        }
        s.sync_video_sounds(|_| true);
        // A copied preset's video has its sound, as it was set.
        if let Some(a) = s.audio_sources.iter_mut().find(|a| a.kind == SourceKind::Video { key: 5 }) {
            a.volume_db = -9.0;
        }
        let a = s.active_preset;
        let p = s.add_preset("Copy", true);
        s.switch_preset(p);
        let copy = s.visuals.iter().find(|v| matches!(v.visual, Visual::Video { .. })).map(|v| v.key).unwrap();
        s.sync_video_sounds(|_| true);
        let sound = s.audio_sources.iter().find(|a| a.kind == SourceKind::Video { key: copy }).map(|a| (a.id.clone(), a.volume_db));
        assert_eq!(sound, Some((format!("video-{copy}"), -9.0)));
        assert_eq!(s.audio_sources.iter().filter(|a| matches!(a.kind, SourceKind::Video { .. })).count(), 1);
        s.switch_preset(a);
        assert!(s.remove_from_scene(LayerKey::Visual(5)));
        s.sync_video_sounds(|_| true);
        assert!(video(&s).is_empty(), "its video gone");
    }

    #[test]
    fn reset_keeps_the_presets() {
        let mut s = setup();
        s.add_scene(true);
        let p = s.add_preset("Streaming", true);
        let mut fresh = RecordSettings::default();
        fresh.keep_presets_from(&mut s);
        assert_eq!((fresh.scenes.len(), fresh.webcams.len()), (2, 1));
        assert!(fresh.presets.iter().any(|x| x.id == p));
        assert!(fresh.preset().is_some_and(|x| x.setup.is_none()));
    }
}
