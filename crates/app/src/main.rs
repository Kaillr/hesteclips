//! HasteClip — a fast, no-nonsense clip recorder.
//!
//! The app opens on your clips. A slim capture bar on top always shows what's
//! happening (buffer armed, recording, idle) and holds the one or two buttons that
//! make sense right now; the global hotkeys do the same from inside a game.
//! Settings is a separate page you rarely visit, reached from the gear.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // no console window on release Windows
// egui's own inputs, turned away by clippy.toml: values are entered with
// ui_kit's controls ("Inputs guide" in ui_kit.rs).
#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

mod app_icons;
mod archive;
mod autostart;
mod away;
mod canvas_ui;
mod clips;
mod collections;
mod crop_ui;
mod demo;
mod fonts;
mod inspector_ui;
mod visuals_ui;
mod webcam_ui;
mod discord;
mod editor;
mod export_ui;
mod filmstrip;
mod free_up;
mod logfile;
mod game_events;
mod games;
mod osu_plays;
mod library;
mod library_view;
mod library_grid;
mod links;
mod glass;
mod go_live;
mod handoff;
mod hover_play;
#[cfg(target_os = "macos")]
mod key_tap;
#[cfg(target_os = "linux")]
mod kwin_window;
#[cfg(target_os = "linux")]
mod linux_desktop;
mod meter;
mod moments;
mod nav;
mod picture;
mod placement_ui;
mod player;
#[cfg(target_os = "linux")]
mod portal_shortcuts;
mod proxy;
mod screenshot;
mod service;
mod sessions;
mod settings;
mod settings_ui;
#[cfg(target_os = "macos")]
mod setup;
mod share;
mod transport;
#[cfg(target_os = "linux")]
mod wayland_drag;
mod shell;
mod shortcuts;
mod sound;
mod sources_ui;
mod store;
mod thumbs;
mod tray;
mod theme;
mod transfer;
mod time_field;
mod ui_kit;
mod update;
#[cfg(target_os = "macos")]
mod update_mac;
mod upload_watch;
#[cfg(target_os = "linux")]
mod update_linux;
mod gpu_frames;
#[cfg(hw_decode)]
mod scrub;
#[cfg(hw_decode)]
mod video;
mod voice;
mod voice_review;
mod voice_samples;
mod web_images;
mod viewer;
mod waveform;
mod wheel;

use crate::ui_kit::Dismissed as _;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use egui::Color32;
use handoff::Handoff;
use service::{CaptureService, Evt};
use settings::{Encoder, RecordSettings, SourceKind};

/// `HASTECLIP_DEBUG_GPU=1`: print the renderer's (wgpu's) warnings and
/// errors, which are otherwise silent: a failed draw just leaves the window blank.
struct GpuLog;

impl log::Log for GpuLog {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Warn && (m.target().starts_with("wgpu") || m.target().starts_with("naga") || m.target().starts_with("egui"))
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            eprintln!("[{} {}] {}", r.level(), r.target(), r.args());
        }
    }
    fn flush(&self) {}
}

fn main() -> eframe::Result<()> {
    // Started again by itself to ask KDE for the screen (so its own windows
    // are in the recording): does only that, and exits.
    #[cfg(target_os = "linux")]
    if capture::linux::kde_stream_helper() {
        return Ok(());
    }
    if std::env::var_os("HASTECLIP_DEBUG_GPU").is_some() {
        static LOGGER: GpuLog = GpuLog;
        let _ = log::set_logger(&LOGGER).map(|()| log::set_max_level(log::LevelFilter::Warn));
    }
    // A development test instance (opened on a page or clip by a dev hook)
    // runs beside the real one, and leaves its single-instance lock, its log
    // and its kept files alone.
    let test_instance = std::env::vars_os().any(|(k, _)| {
        let k = k.to_string_lossy();
        ["HASTECLIP_OPEN_", "HASTECLIP_DEMO_", "HASTECLIP_LIBRARY", "HASTECLIP_SETTINGS"].iter().any(|p| k.starts_with(p))
    });
    // An installed app has no console: what it prints goes to its log file
    // (`HASTECLIP_LOG=1` does the same in a development build, or a test
    // instance, to check it).
    #[cfg(any(windows, target_os = "linux"))]
    if (cfg!(not(debug_assertions)) && !test_instance) || std::env::var_os("HASTECLIP_LOG").is_some() {
        logfile::start();
    }
    #[cfg(windows)]
    logfile::log_cursor();
    // The installer's hooks (install, update, uninstall) run the app with special
    // arguments and exit here; a downloaded update left uninstalled is put in
    // place now. Does nothing in a development build.
    #[cfg(windows)]
    velopack::VelopackApp::build().run();
    #[cfg(target_os = "linux")]
    if update_linux::apply_pending_at_launch() {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    if update_mac::apply_pending_at_launch() {
        return Ok(());
    }
    // One HasteClip at a time: another launch shows the running one and
    // exits, and a build of your own takes over from it. (Not for
    // development test instances, which run beside it.)
    let instance = if test_instance {
        None
    } else {
        match tray::claim() {
            Some(listener) => listener,
            None => return Ok(()),
        }
    };
    // Scrub frames kept from before scrubbing decoded live. (Not from a test
    // instance: the real one beside it may be an older build using them.)
    if !test_instance {
        proxy::forget_kept();
    }
    // Started with the computer: straight to the tray.
    let background = std::env::args().any(|a| a == autostart::BACKGROUND) && tray::available();
    // Before anything talks to the desktop portal (screen capture, shortcuts),
    // which wants to know who we are, and needs our `.desktop` file for that.
    #[cfg(target_os = "linux")]
    {
        linux_desktop::integrate();
        portal_shortcuts::register_app();
    }

    // Killed from outside (Ctrl+C, SIGTERM, logout): stop capture and finish the
    // file before exiting, since destructors don't run on a signal.
    let _ = ctrlc::set_handler(quit_now);

    // Dev aid: `HASTECLIP_WINDOW=WxH` opens at that size, to check layouts.
    let size = std::env::var("HASTECLIP_WINDOW")
        .ok()
        .and_then(|s| s.split_once('x').and_then(|(w, h)| Some([w.parse().ok()?, h.parse().ok()?])))
        .unwrap_or([1040.0, 700.0]);
    let viewport = egui::ViewportBuilder::default()
        .with_visible(!background)
        // Windows: started with the computer, it takes the focus from nothing
        // as it's drawn on its way to the tray (`App::start_in_tray`).
        .with_active(!(background && cfg!(windows)))
        .with_inner_size(size)
        .with_min_inner_size([560.0, 420.0])
        .with_title("HasteClip")
        .with_icon(app_icon());
    // macOS: no title bar; the rail runs to the top, under the traffic lights.
    #[cfg(target_os = "macos")]
    let viewport = viewport.with_fullsize_content_view(true).with_titlebar_shown(false).with_title_shown(false);
    // Wayland names the window's app by this (its icon and name come from
    // the `.desktop` file of the same name).
    #[cfg(target_os = "linux")]
    let viewport = viewport.with_app_id(portal_shortcuts::APP_ID);
    let options = eframe::NativeOptions {
        viewport,
        wgpu_options: wgpu_options(),
        // macOS, started with the computer: the app doesn't take the focus
        // as it starts, on its way to the tray (winit makes it the active
        // one by default, over whatever's in front). `with_active` above
        // doesn't do it here: on macOS it only counts for a window made
        // visible, and eframe makes it hidden.
        #[cfg(target_os = "macos")]
        event_loop_builder: background.then(|| {
            let hook: eframe::EventLoopBuilderHook = Box::new(|builder| {
                use winit::platform::macos::EventLoopBuilderExtMacOS;
                builder.with_activate_ignoring_other_apps(false);
            });
            hook
        }),
        ..Default::default()
    };

    let result = eframe::run_native(
        "HasteClip",
        options,
        Box::new(move |cc| {
            // Started with the computer, the window goes straight to the
            // tray. eframe shows it once its first frame is drawn, whatever
            // `with_visible` says: cloaked till it's there (Windows; on
            // macOS, transparent; see `App::start_in_tray`).
            #[cfg(any(windows, target_os = "macos"))]
            if background {
                shell::cloak(cc, true);
            }
            gpu_frames::init(cc.wgpu_render_state.as_ref(), &cc.egui_ctx);
            #[cfg(target_os = "macos")]
            shell::centre_window_buttons(cc);
            ui_kit::apply(&cc.egui_ctx);
            Ok(Box::new(App::new(cc.egui_ctx.clone(), instance, background)))
        }),
    );
    // The window couldn't open because a system library is missing: say
    // which, in words (the error itself is a wall of text). Desktops have
    // them all; a minimal install may not.
    #[cfg(target_os = "linux")]
    if let Err(e) = &result {
        // The library is named in the inner error, which only the debug form shows.
        let text = format!("{e:?}");
        if let Some(lib) = text.split(|c: char| c.is_whitespace() || c == '(' || c == ':').find(|w| w.starts_with("lib") && w.contains(".so")) {
            eprintln!(
                "HasteClip can't open its window: {lib} is missing. Install your distro's package for it \
                 (libXcursor, libXrandr, libXi, libxkbcommon-x11 and Vulkan or OpenGL drivers are needed on X11)."
            );
        }
    }
    // The app is gone by now (capture stopped, files finished): end what
    // capture keeps running in the background, then an update can go in.
    capture::shutdown();
    update::apply_queued();
    result
}

/// The renderer's setup. On Windows, D3D12, so decoded video frames can be
/// shared with it straight from the GPU (`gpu_frames.rs`); wgpu's default
/// pick could be Vulkan, which can't open them as simply. Elsewhere wgpu's
/// pick. `HASTECLIP_WGPU_BACKEND` (e.g. "vulkan", "gl") overrides it, to
/// compare (or on Linux, where a GPU without a Vulkan driver gets Mesa's
/// software one, "gl" draws on the GPU instead).
fn wgpu_options() -> eframe::egui_wgpu::WgpuConfiguration {
    let mut options = eframe::egui_wgpu::WgpuConfiguration::default();
    // A hidden window (covered, minimized, or not shown yet) gets no frame to
    // draw into, and egui skips the frame and tries again at once: a page that
    // repaints continuously (Sources, with its meters and preview) spun at
    // ~7000 tries a second, and right after launch macOS never got to show the
    // window at all, so it stayed blank. Wait a display refresh first.
    // (D3D12 hands a hidden window its frames all the same: in the tray it's
    // minimized as well, see `hide_window`.)
    let default = options.on_surface_status.clone();
    options.on_surface_status = std::sync::Arc::new(move |status| {
        if matches!(status, eframe::wgpu::CurrentSurfaceTexture::Occluded) {
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
        default(status)
    });
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(new) = &mut options.wgpu_setup {
        match std::env::var("HASTECLIP_WGPU_BACKEND") {
            Ok(b) => new.instance_descriptor.backends = eframe::wgpu::Backends::from_comma_list(&b),
            Err(_) if cfg!(windows) => new.instance_descriptor.backends = eframe::wgpu::Backends::DX12,
            Err(_) => {}
        }
        // A development build has the GPU driver check every call (wgpu's
        // default), but what it finds is only printed with HASTECLIP_DEBUG_GPU:
        // without that, the checks just cost 170-190 MB of memory.
        // (`WGPU_VALIDATION=1` still turns them on.)
        if std::env::var_os("HASTECLIP_DEBUG_GPU").is_none() {
            let checks = eframe::wgpu::InstanceFlags::VALIDATION | eframe::wgpu::InstanceFlags::GPU_BASED_VALIDATION;
            new.instance_descriptor.flags = eframe::wgpu::InstanceFlags::from_build_config().difference(checks).with_env();
        }
        // wgpu's default ("Performance") reserves memory in big blocks: for data
        // on its way to the GPU (pictures, thumbnails), 64 MB and then 128 MB of
        // RAM, kept while anything in them is in use. Small blocks are enough
        // for what this app sends: ~150 MB less, drawing just as fast.
        let device = new.device_descriptor.clone();
        new.device_descriptor = std::sync::Arc::new(move |adapter| eframe::wgpu::DeviceDescriptor {
            memory_hints: eframe::wgpu::MemoryHints::MemoryUsage,
            ..device(adapter)
        });
    }
    options
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Page {
    Clips,
    Sources,
    Settings,
    /// Listening through the voice detector's samples (opened from Settings).
    VoiceReview,
    Edit,
    View,
}

/// What capture is doing: clipping, recording and streaming, any of them at
/// once, from the same capture (what it was started with stays while any of
/// them runs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct RecState {
    pub clipping: bool,
    pub recording: bool,
    pub streaming: bool,
}

impl RecState {
    /// Nothing runs.
    pub(crate) fn idle(self) -> bool {
        !(self.clipping || self.recording || self.streaming)
    }

    fn of(r: capture::Running) -> Self {
        Self { clipping: r.replay, recording: r.record, streaming: r.stream }
    }

    pub(crate) fn has(self, mode: capture::Mode) -> bool {
        match mode {
            capture::Mode::ReplayBuffer => self.clipping,
            capture::Mode::Record => self.recording,
            capture::Mode::Stream => self.streaming,
        }
    }

    fn with(mut self, mode: capture::Mode, on: bool) -> Self {
        match mode {
            capture::Mode::ReplayBuffer => self.clipping = on,
            capture::Mode::Record => self.recording = on,
            capture::Mode::Stream => self.streaming = on,
        }
        self
    }

    /// What a setting fixed while capturing waits for, in words: "clipping",
    /// "recording and streaming".
    pub(crate) fn words(self) -> String {
        let parts: Vec<&str> = [(self.clipping, "clipping"), (self.recording, "recording"), (self.streaming, "streaming")].into_iter().filter(|(on, _)| *on).map(|(_, w)| w).collect();
        match parts.as_slice() {
            [] => String::new(),
            [one] => (*one).to_owned(),
            [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
        }
    }
}

/// A scene or preset to show: done at the top of a frame's background work
/// (see `App::apply_switch`), never while a page draws or mid-change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Switch {
    Scene(u64),
    /// A preset. `restart`: clipping (all that runs) stops first and starts
    /// again with it.
    Preset { id: u64, restart: bool },
}

/// Each sound source's volume, mute, noise reduction and mono, and the
/// limiter, into the live audio: at launch and on a preset switch, all at
/// once (the Sources page keeps them in step while it's drawn).
fn push_live_audio(live_audio: &capture::mixer::LiveAudio, settings: &settings::RecordSettings) {
    live_audio.set_limiter(settings.limiter);
    for s in &settings.audio_sources {
        let channel = live_audio.channel(&s.id);
        channel.set_volume(sources_ui::from_db(s.volume_db), s.muted);
        channel.set_denoise(s.noise_reduction.into());
        channel.set_mono(s.mono);
    }
}

/// Quit from outside the UI's loop (a signal; the tray while the window's
/// hidden, when the loop doesn't run): stop capturing, finish the file, and
/// exit.
pub(crate) fn quit_now() {
    service::stop_all_and_wait(std::time::Duration::from_secs(10));
    // Without running the C library's exit handlers: they unload the
    // graphics driver while the UI thread may still be drawing with it,
    // which crashed (NVIDIA on Linux). The recording is already written.
    #[cfg(unix)]
    // SAFETY: ends the process at once; nothing runs after it.
    unsafe {
        libc::_exit(0)
    };
    #[cfg(not(unix))]
    std::process::exit(0);
}

/// The app icon (`assets/icon-mac.svg` rendered to PNG), shown in the Dock and taskbar
/// while running. Release bundles use `assets/hasteclip.icns` instead. On
/// Windows and Linux, where app icons aren't drawn on a plate, the glyph
/// without the macOS one: with the plate, it looked small.
pub(crate) fn app_icon() -> egui::IconData {
    // Linux: the icon the desktop has for the app (scripts/icons.sh).
    #[cfg(not(any(windows, target_os = "macos")))]
    let png = include_bytes!("../assets/glyph-icon/256.png");
    // Windows: as the .exe's icon has it at the sizes a window's icon is
    // shown (the taskbar's, 24-48 px): the glyph with its keyline.
    #[cfg(windows)]
    let png = include_bytes!("../assets/tray/glyph-64.png");
    #[cfg(target_os = "macos")]
    let png = include_bytes!("../assets/icon-1024.png");
    let img = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .expect("bundled icon is a valid PNG")
        .to_rgba8();
    egui::IconData { width: img.width(), height: img.height(), rgba: img.into_raw() }
}

impl App {
    /// The shortcut for `action` as key names, for keycaps.
    pub(crate) fn shortcut_keys(&self, action: settings::ShortcutAction) -> Option<Vec<String>> {
        shortcuts::keys(self.settings.shortcuts.get(action))
    }
}

/// egui's bundled fonts have no arrows (← →), nor the ⌘ ⌥ ⌃ ⇧ ⌫ macOS writes
/// every shortcut with. Use the system's symbol font as a fallback (it's only
/// consulted for characters the bundled fonts lack).
fn add_symbol_font(ctx: &egui::Context) {
    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &["/System/Library/Fonts/Apple Symbols.ttf"]
    } else if cfg!(target_os = "windows") {
        &[r"C:\Windows\Fonts\seguisym.ttf"]
    } else {
        &["/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", "/usr/share/fonts/TTF/DejaVuSans.ttf", "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf"]
    };
    let Some(bytes) = candidates.iter().find_map(|p| std::fs::read(p).ok()) else { return };
    use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};
    let families = [egui::FontFamily::Proportional, egui::FontFamily::Monospace]
        .map(|family| InsertFontFamily { family, priority: FontPriority::Lowest })
        .to_vec();
    ctx.add_font(FontInsert::new("system-symbols", egui::FontData::from_owned(bytes), families));
}

/// An in-app shortcut with the platform's command key, as text: "Cmd + A" / "Ctrl + A".
fn hotkey_label_cmd(key: &str) -> String {
    shortcuts::command(key).join(" + ")
}

/// The OS's file manager's name: "Finder", "Explorer", "the file manager".
pub(crate) fn file_manager() -> &'static str {
    if cfg!(target_os = "macos") {
        "Finder"
    } else if cfg!(target_os = "windows") {
        "Explorer"
    } else {
        "the file manager"
    }
}

/// What the OS calls its file manager.
fn reveal_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Show in Finder"
    } else if cfg!(target_os = "windows") {
        "Show in Explorer"
    } else {
        "Show in folder"
    }
}

const TOAST_FOR: Duration = Duration::from_millis(3200);

struct App {
    pub(crate) page: Page,
    rec_state: RecState,
    /// When the recording, and the stream, started (for their timers).
    rec_started: Option<Instant>,
    stream_started: Option<Instant>,
    /// How each place streamed to is doing, shared with the stream, and what
    /// it was last seen as (to say when that changes).
    pub(crate) stream_status: std::sync::Arc<capture::stream::Status>,
    stream_seen: Vec<capture::stream::DestinationStatus>,
    /// The "Go live?" dialog is open.
    pub(crate) go_live_open: bool,
    /// How the capture keeps up (frames skipped and pictures missed in its
    /// last whole second, frame blending), for the status by the recorder,
    /// and when that was last looked up.
    health: Option<capture::CaptureHealth>,
    health_at: Option<Instant>,
    /// A replay clip is being written; shows a placeholder card until it lands.
    /// Clips being written: replay clips still saving, and a stopped recording
    /// being finished. Each shows a placeholder card until it lands.
    saving: usize,
    settings: RecordSettings,
    /// Last-saved settings JSON, to persist only when something changed.
    saved_settings: String,
    /// False when launched through a dev hook (`HASTECLIP_OPEN_*`, `_DEMO_*`,
    /// `_LIBRARY`): a test instance next to the real one must never write its
    /// settings over the real ones.
    persist_settings: bool,
    service: CaptureService,
    /// Global shortcuts, kept in step with `settings.shortcuts`. `Err` when the OS
    /// refused shortcuts altogether.
    pub(crate) hotkeys: Result<shortcuts::Registered, String>,
    /// The Settings shortcut recorder: which action is listening for keys, and
    /// the last problem with what was pressed.
    pub(crate) recording_shortcut: Option<(settings::ShortcutAction, Option<&'static str>)>,
    /// The "Reset all settings?" confirmation is open.
    pub(crate) confirm_reset: bool,
    /// The camera panel (Linux), in place of the Sound column while it's open.
    pub(crate) camera_panel: Option<crate::webcam_ui::CameraPanel>,
    /// Scroll the library to this clip next time it's shown.
    pub(crate) reveal_clip: Option<PathBuf>,
    /// Days folded away in the library.
    pub(crate) folded: library::FoldedDays,
    /// Adding a custom clip-saved sound failed: why.
    /// Screen-recording permission, re-checked each poll so the banner clears the
    /// moment the user grants it.
    permission: capture::Permission,
    /// What's wrong with ffmpeg, once checked (in the background, at launch).
    ffmpeg_problem: std::sync::Arc<std::sync::OnceLock<Option<String>>>,
    /// Screens detected by the capture backend.
    screens: Vec<capture::Device>,
    /// The recording's frame size for (what's recorded, resolution), cached:
    /// for keeping the webcam's box its shape. Kept while capturing; cleared
    /// when a capture starts or stops.
    frame_size: Option<((capture::VideoSource, capture::VideoSize), Option<(u32, u32)>, Instant)>,
    /// Apps with a window, for the games-and-apps list: to add, and to show
    /// which are open. Refreshed while the Sources page shows.
    pub(crate) windowed_apps: Vec<capture::Device>,
    /// Running games with anti-cheat, for adding to game capture's allowed
    /// list (looked up when its menu opens).
    pub(crate) anticheat_apps: Vec<(capture::Device, String)>,
    /// What the running capture records, to send it list changes live.
    capturing_video: Option<capture::VideoSource>,
    /// The audio sources the running capture has: those it started with,
    /// mics following device changes, and where it can be
    /// (`capture::SOURCES_WHILE_CAPTURING`), those added and taken out since.
    capturing_sources: Vec<capture::sources::AudioSource>,
    /// The audio sources the running capture started with: its file's
    /// tracks are theirs.
    pub(crate) started_sources: Vec<capture::sources::AudioSource>,
    /// The replay length the running capture keeps, to send it changes live.
    capturing_replay: Option<u32>,
    /// Frame blending as the running capture does it (looks a frame, how
    /// they're weighed), to send it changes live.
    capturing_blend: Option<(u32, capture::BlendShape)>,
    /// Shown instead of a game or app while you're tabbed out.
    pub(crate) away_screen: std::sync::Arc<capture::StillImage>,
    /// Where the first webcam sits (where only one is drawn:
    /// `capture::layers::MANY`), shared with a running capture so dragging
    /// it takes effect at once. Mirrors the settings every frame.
    webcam_placement: capture::webcam::SharedPlacement,
    /// Every source in the video, in order, as placed: shared with the
    /// capture like the placements (kept in step with the settings each frame).
    pub(crate) capture_layers: capture::SharedLayers,
    /// Where the screen's picture goes in the video, shared with the capture
    /// like the webcam's placement (kept in step with the settings each frame).
    pub(crate) screen_placement: capture::SharedScreen,
    pub(crate) webcam_view: webcam_ui::WebcamView,
    /// The picture's sources on the Sources page's preview: dragging them.
    pub(crate) canvas: canvas_ui::CanvasView,
    /// The cameras last asked to stay open (each stays open while its
    /// webcam is set up, so its own settings don't reset).
    kept_cameras: Vec<(String, Option<capture::webcam::Format>)>,
    /// A scene or preset to show next (see `apply_switch`).
    pending_switch: Option<Switch>,
    /// System audio inputs/outputs (+ current OS defaults).
    audio: capture::audio::AudioDevices,
    /// Live per-source volume and meters, shared with the capture thread.
    live_audio: std::sync::Arc<capture::mixer::LiveAudio>,
    /// Meters-only capture while the Sources page is open and nothing records,
    /// with the sources it was started for.
    level_monitor: Option<(Vec<capture::sources::AudioSource>, sources_ui::Background)>,
    /// When the pointer was last on Sources' place in the rail: its captures
    /// start then (see `wants_sources_capture`).
    pub(crate) sources_warm: Option<Instant>,
    /// Apps' own icons, for app sources and the games and apps followed.
    pub(crate) app_icons: app_icons::AppIcons,
    /// Preview-only capture while the Sources page is open and nothing records,
    /// with the source, height and frame rate it was started for.
    pub(crate) video_preview: Option<(sources_ui::PreviewSetup, capture::preview::VideoPreview)>,
    sources_view: sources_ui::SourcesView,
    /// A microphone heard live ("Listen" on the Sources page).
    pub(crate) listening: Option<sources_ui::Listening>,
    clips: Vec<clips::Clip>,
    thumbs: thumbs::Thumbs,
    /// Most recently saved clip, highlighted in the library for a few seconds.
    last_saved: Option<(PathBuf, Instant)>,
    toast: Option<shell::Toast>,
    /// Where toasts sit: how far above the window's bottom, and how far
    /// right of its centre (the middle of the page, beside the rail).
    toast_bottom: f32,
    toast_x_offset: f32,
    editor: Option<editor::Editor>,
    viewer: Option<viewer::Viewer>,
    /// Edits being rendered in the background, with live progress.
    pub(crate) renders: Vec<RenderJob>,
    render_tx: std::sync::mpsc::Sender<RenderDone>,
    render_rx: std::sync::mpsc::Receiver<RenderDone>,
    /// Clips' sound being saved as MP3s, when done.
    mp3_tx: std::sync::mpsc::Sender<Result<PathBuf, String>>,
    mp3_rx: std::sync::mpsc::Receiver<Result<PathBuf, String>>,
    /// Screenshots being put on the clipboard (Copy image), when done.
    pub(crate) copy_tx: std::sync::mpsc::Sender<Result<(), String>>,
    pub(crate) copy_rx: std::sync::mpsc::Receiver<Result<(), String>>,
    /// An export or import of clips running (see `transfer.rs`).
    transfer: Option<transfer::Transfer>,
    /// The library's shared clips, the uploads and unshares running, the
    /// server being asked about links, and the Share with a link dialog
    /// (see `links.rs`).
    links: links::Links,
    link_jobs: Vec<links::Job>,
    link_checks: Vec<links::Check>,
    pub(crate) link_dialog: Option<links::Dialog>,
    next_render_id: u64,
    /// Open "Rename clip" dialog.
    pub(crate) rename: Option<library::Rename>,
    /// Clips selected in the library for a bulk action.
    pub(crate) selection: library::Selection,
    /// What's typed in the library's search.
    pub(crate) library_query: String,
    /// What's typed in Settings' search, the pane shown, and whether the
    /// Clips pane's Clip names page is open.
    pub(crate) settings_query: String,
    pub(crate) settings_pane: settings_ui::Pane,
    pub(crate) naming_open: bool,
    /// New clips whose dot has been seen (pointed at).
    pub(crate) seen_new: std::collections::HashSet<PathBuf>,
    /// A clip brought back into view, and when: it flashes.
    pub(crate) flash_clip: Option<(PathBuf, f64)>,
    /// The clip playing in its card under the pointer (see `hover_play`).
    pub(crate) hover_play: Option<hover_play::HoverPlay>,
    /// A card was under the pointer this frame (else `hover_play` stops).
    pub(crate) hover_seen: bool,
    pub(crate) app_icon_tex: Option<(u32, egui::TextureHandle)>,
    /// The grid's clips (in the grid's content coordinates) and where that
    /// content is on screen now, for the panel's glass.
    pub(crate) glass_tiles: Vec<glass::Tile>,
    pub(crate) glass_origin: egui::Pos2,
    /// The grid's part on screen, in its content (top, bottom).
    pub(crate) glass_view: (f32, f32),
    pub(crate) glass: glass::Glass,
    /// What the library shows and where, worked out once per change (see
    /// `library_grid`), and the library's list's version.
    pub(crate) shown: library_grid::Shown,
    pub(crate) moves: library_grid::Moves,
    pub(crate) library_version: u64,
    /// The keyboard shortcuts dialog, for this screen.
    pub(crate) show_keys: Option<shell::Keys>,
    /// Clips being dragged inside the window, to a game or collection.
    pub(crate) card_drag: Option<library::CardDrag>,
    /// Clips waiting for "Delete permanently?" to be answered.
    pub(crate) confirm_delete: Option<Vec<PathBuf>>,
    /// The library's Free up space dialog, and originals being freed (see
    /// `free_up.rs`).
    pub(crate) free_up: Option<free_up::Ask>,
    pub(crate) free_up_batches: Vec<free_up::Batch>,
    /// The editor was opened from the player on this clip: closing it goes
    /// back there, not to the library (so cleaning up clips flows on).
    edit_return: Option<PathBuf>,
    /// The tray icon, where there is one: closing the window hides it there.
    tray: Option<tray::Tray>,
    /// The window's hidden in the tray, until it's shown again.
    in_tray: bool,
    /// Windows and macOS, started with the computer: to go to the tray once
    /// its first frame is drawn, and cloaked till it's there (see
    /// `start_in_tray`).
    #[cfg(any(windows, target_os = "macos"))]
    to_tray: bool,
    #[cfg(any(windows, target_os = "macos"))]
    cloaked: bool,
    /// Windows: it was maximized as it went to the tray, to be again.
    #[cfg(windows)]
    tray_maximized: bool,
    /// Windows: shown from the tray, to be brought to the front once it's
    /// back (see `come_to_front`).
    #[cfg(windows)]
    to_front: bool,
    /// Since when the window hasn't been used: another has had the focus,
    /// and nothing's been scrolled or pressed over this one.
    unused_since: Option<Instant>,
    /// The frame it went to the tray in: a later one drawn while it's used
    /// says it's out (see `ui`).
    tray_frame: u64,
    /// Until when the renderer's settled while out of sight, after the
    /// player or the editor let go of decoders (see `keep_open`).
    settle_until: Option<Instant>,
    /// Quitting for real (closing mustn't just hide the window then).
    quitting: bool,
    /// The first close: ask whether to keep running in the tray.
    close_dialog: bool,
    /// macOS: the permissions screen is up (once, at the first launch).
    setup_open: bool,
    /// What was shown last frame (page, library filter, Settings' pane and
    /// whether Clip names was open), to scroll a new view back to the top and
    /// bring its content in.
    last_view: Option<(Page, library::Filter, Option<(settings_ui::Pane, bool)>)>,
    /// Library auto-refresh: last folder poll + when the library's folders
    /// last changed, as last seen.
    last_poll: Option<Instant>,
    library_stamp: Vec<Option<SystemTime>>,
    /// Which of the library's folders (games) the library shows.
    pub(crate) library_filter: library::Filter,
    /// And whether clips, screenshots or both.
    pub(crate) library_kind: library::ShowKind,
    /// Your own groups of clips, in the library's `.hasteclip`.
    pub(crate) collections: collections::Collections,
    pub(crate) collection_dialog: Option<library::CollectionDialog>,
    /// Where the mouse's back/forward buttons go.
    pub(crate) nav: nav::History,
    /// Games' icons.
    pub(crate) web_images: web_images::WebImages,
    pub(crate) updater: update::Updater,
    /// A demo being recorded (`HASTECLIP_DEMO_SCRIPT`), see `demo.rs`.
    demo: Option<demo::Demo>,
    pub(crate) presence: discord::Presence,
    /// Saves a clip as Save clip is pressed, frames or not.
    quick_save: service::QuickSave,
    /// Listens for "HasteClip that".
    pub(crate) voice: voice::Voice,
    pub(crate) voice_review: voice_review::Review,
    /// The game being clipped, for Discord: its app's id, the game, and when
    /// we last checked it's still open.
    clipped_game: Option<(String, discord::Game, Instant)>,
    /// What's in focus while capturing, for the folder clips go in.
    games: games::Tracker,
    /// Record whole sessions: when a game on the list opens and closes.
    sessions: sessions::Sessions,
    /// The game Discord shows, and since when, as its "elapsed" timer counts.
    presence_since: Option<(String, SystemTime)>,
}

impl App {
    /// `background`: started with the computer, for the tray.
    fn new(ctx: egui::Context, instance: Option<std::net::TcpListener>, background: bool) -> Self {
        add_symbol_font(&ctx);
        let settings = RecordSettings::load();
        let screen_placement = std::sync::Arc::new(std::sync::Mutex::new(settings.screen_placement()));
        // egui keeps the window's frame in step (on Wayland, drawn by winit).
        ctx.set_theme(settings.theme.preference());
        // Dev aid: `HASTECLIP_THEME=light|dark` for this launch only.
        match std::env::var("HASTECLIP_THEME").as_deref() {
            Ok("light") => ctx.set_theme(egui::ThemePreference::Light),
            Ok("dark") => ctx.set_theme(egui::ThemePreference::Dark),
            _ => {}
        }
        sound::preload(&settings.save_sound.sound);
        store::set_library(&settings.output_dir);
        let recovered = capture::output::recover_unfinished(&settings.output_dir);
        let saved_settings = settings.to_json();
        let mut clips = clips::scan(&settings.output_dir);
        if store::restore_edit_dates(&clips) > 0 {
            clips = clips::scan(&settings.output_dir);
        }
        let mut collections = collections::Collections::load(&settings.output_dir);
        let folded = library::FoldedDays::load(&settings.output_dir);
        let links = links::Links::load(&settings.output_dir);
        collections.reconnect(&clips);
        // Assets of clips deleted in Finder go to the Bin.
        store::sweep_orphans(&settings.output_dir, &clips);
        let live_audio = capture::mixer::LiveAudio::new();
        let service = CaptureService::new(live_audio.clone());
        let games = games::Tracker::new();
        let quick_save = service.quick_save(games.clone());
        let hotkeys = shortcuts::Registered::new(ctx.clone(), quick_save.clone()).map(|mut h| {
            h.sync(&settings.shortcuts);
            h
        });
        let (render_tx, render_rx) = std::sync::mpsc::channel();
        let (mp3_tx, mp3_rx) = std::sync::mpsc::channel();
        let (copy_tx, copy_rx) = std::sync::mpsc::channel();
        push_live_audio(&live_audio, &settings);
        // macOS: what it needs allowed is shown once, before clipping starts
        // (and once more when a release needs it allowed again).
        #[cfg(target_os = "macos")]
        let setup_open = settings.setup_shown < setup::SETUP_ROUND && !demo::on();
        #[cfg(not(target_os = "macos"))]
        let setup_open = false;
        let updater = update::Updater::new(ctx.clone(), settings.auto_update);
        let (tray_tx, tray_rx) = std::sync::mpsc::channel();
        if let Some(listener) = instance {
            tray::serve(listener, &ctx, tray_tx.clone());
        }
        let tray = tray::Tray::new(&ctx, tray_tx, tray_rx, quick_save.clone());
        // No tray icon to open it from: it shows after all.
        #[cfg(any(windows, target_os = "macos"))]
        let to_tray = background && tray.is_some();
        #[cfg(not(any(windows, target_os = "macos")))]
        let _ = background;
        let mut app = Self {
            page: Page::Clips,
            rec_state: RecState::default(),
            rec_started: None,
            stream_started: None,
            stream_status: capture::stream::Status::new(),
            stream_seen: Vec::new(),
            go_live_open: false,
            health: None,
            health_at: None,
            saving: 0,
            settings,
            saved_settings,
            persist_settings: !std::env::vars_os().any(|(k, _)| {
                let k = k.to_string_lossy();
                ["HASTECLIP_OPEN_", "HASTECLIP_DEMO_", "HASTECLIP_LIBRARY", "HASTECLIP_SETTINGS"].iter().any(|p| k.starts_with(p))
            }),
            service,
            voice: voice::Voice::new(quick_save.clone(), live_audio.clone(), ctx.clone()),
            voice_review: Default::default(),
            quick_save,
            hotkeys,
            recording_shortcut: None,
            confirm_reset: false,
            camera_panel: None,
            reveal_clip: None,
            folded,
            permission: capture::screen_permission(),
            ffmpeg_problem: {
                let problem = std::sync::Arc::new(std::sync::OnceLock::new());
                let (p, ctx) = (problem.clone(), ctx.clone());
                std::thread::spawn(move || {
                    let found = media::problem();
                    if found.is_some() {
                        ctx.request_repaint();
                    }
                    let _ = p.set(found);
                });
                problem
            },
            screens: capture::list_screens(),
            frame_size: None,
            windowed_apps: Vec::new(),
            anticheat_apps: Vec::new(),
            capturing_video: None,
            capturing_sources: Vec::new(),
            started_sources: Vec::new(),
            capturing_replay: None,
            capturing_blend: None,
            away_screen: away::screen(),
            webcam_placement: std::sync::Arc::new(std::sync::Mutex::new(capture::webcam::Placement::default_for(16.0 / 9.0, 16.0 / 9.0))),
            capture_layers: capture::layers::main_only(),
            screen_placement,
            webcam_view: Default::default(),
            canvas: Default::default(),
            kept_cameras: Vec::new(),
            pending_switch: None,
            audio: capture::audio::list_audio_devices(),
            live_audio,
            level_monitor: None,
            sources_warm: None,
            app_icons: Default::default(),
            video_preview: None,
            sources_view: Default::default(),
            listening: None,
            clips,
            thumbs: thumbs::Thumbs::new(ctx.clone()),
            last_saved: None,
            toast: None,
            toast_bottom: 22.0,
            toast_x_offset: 0.0,
            editor: None,
            viewer: None,
            renders: Vec::new(),
            render_tx,
            render_rx,
            mp3_tx,
            mp3_rx,
            copy_tx,
            copy_rx,
            transfer: None,
            links,
            link_jobs: Vec::new(),
            link_checks: Vec::new(),
            link_dialog: None,
            next_render_id: 0,
            rename: None,
            selection: library::Selection::default(),
            library_query: String::new(),
            settings_query: String::new(),
            settings_pane: settings_ui::Pane::default(),
            naming_open: false,
            seen_new: Default::default(),
            flash_clip: None,
            hover_play: None,
            hover_seen: false,
            app_icon_tex: None,
            glass_tiles: Vec::new(),
            glass_origin: egui::Pos2::ZERO,
            glass_view: (0.0, 0.0),
            glass: Default::default(),
            shown: Default::default(),
            moves: Default::default(),
            library_version: 0,
            show_keys: None,
            last_poll: None,
            library_stamp: Vec::new(),
            library_filter: library::Filter::All,
            library_kind: library::ShowKind::All,
            collections,
            collection_dialog: None,
            card_drag: None,
            confirm_delete: None,
            free_up: None,
            free_up_batches: Vec::new(),
            last_view: None,
            tray,
            in_tray: false,
            #[cfg(any(windows, target_os = "macos"))]
            to_tray,
            #[cfg(any(windows, target_os = "macos"))]
            cloaked: background,
            #[cfg(windows)]
            tray_maximized: false,
            #[cfg(windows)]
            to_front: false,
            unused_since: None,
            tray_frame: 0,
            settle_until: None,
            quitting: false,
            close_dialog: false,
            setup_open,
            edit_return: None,
            nav: nav::History::default(),
            web_images: Default::default(),
            updater,
            demo: demo::Demo::from_env(&ctx),
            presence: discord::Presence::new(),
            clipped_game: None,
            games,
            sessions: Default::default(),
            presence_since: None,
        };
        // Dev aid: `HASTECLIP_OPEN_EDITOR=<clip>` opens the editor at launch, so the
        // editor can be checked without clicking through the library.
        if let Some(clip) = std::env::var_os("HASTECLIP_OPEN_EDITOR") {
            app.settings.auto_start_buffer = false;
            app.open_editor(PathBuf::from(clip));
        }
        if let Some(clip) = std::env::var_os("HASTECLIP_OPEN_VIEWER") {
            app.settings.auto_start_buffer = false;
            app.open_viewer(PathBuf::from(clip));
        }
        // Dev aid: `HASTECLIP_DEMO_RENDER=<clip>` shows the library with a save
        // in progress (that clip's edit + a "save as new"), to check the progress UI.
        // Point HASTECLIP_LIBRARY at a scratch folder holding a copy of the clip.
        if let Some(dir) = std::env::var_os("HASTECLIP_LIBRARY") {
            app.settings.auto_start_buffer = false;
            app.settings.output_dir = PathBuf::from(dir);
            // A demo's deletes are for good: the copy it runs on goes away
            // anyway, and nothing should land in the real Bin.
            app.settings.delete_permanently = demo::on();
            app.refresh_clips();
        }
        if let Some(clip) = std::env::var_os("HASTECLIP_DEMO_RENDER").map(PathBuf::from) {
            let target = store::EditTarget::of(&clip);
            if let Ok(info) = media::probe(&target.source) {
                let mut edit = media::Edit::new(&info);
                edit.start = info.snap(info.duration * 0.1);
                edit.end = info.snap(info.duration * 0.9);
                app.start_render(target.clone(), info.clone(), edit.clone(), None, None);
                app.start_render(target, info, edit, Some("Demo highlight".into()), None);
            }
        }
        // `HASTECLIP_DEMO_COLLECTION=<name>` shows a collection; `…_NEW` adds
        // the "New collection" dialog.
        if let Some(name) = std::env::var("HASTECLIP_DEMO_COLLECTION").ok() {
            if let Some(c) = app.collections.list().iter().find(|c| c.name == name) {
                app.library_filter = library::Filter::Collection(c.id.clone());
            }
            if std::env::var_os("HASTECLIP_DEMO_COLLECTION_NEW").is_some() {
                let first = app.clips.first().map(|c| c.path.clone()).into_iter().collect();
                app.new_collection_with(first);
            }
        }
        // `HASTECLIP_DEV_BUFFER_QUIT=<secs>`: start the replay buffer, quit
        // after that long (to check quitting while capturing).
        if std::env::var_os("HASTECLIP_DEV_BUFFER_QUIT").is_some() {
            app.start_replay_buffer();
        }
        // `HASTECLIP_DEMO_SAVING=<clip>` shows the player waiting for its edit.
        if let Some(clip) = std::env::var_os("HASTECLIP_DEMO_SAVING").map(PathBuf::from) {
            app.viewer = Some(viewer::Viewer::saving(&clip, None));
            app.page = Page::View;
        }
        // `HASTECLIP_DEMO_DELETE=<clip>` asks to delete it (nothing is deleted
        // unless the dialog is answered).
        if let Some(clip) = std::env::var_os("HASTECLIP_DEMO_DELETE").map(PathBuf::from) {
            app.confirm_delete = Some(vec![clip]);
        }
        // `HASTECLIP_DEMO_FREE_UP=<clips>` (separated as in PATH) asks how to free up their
        // originals' space (nothing changes unless the dialog is answered).
        if let Some(clips) = std::env::var_os("HASTECLIP_DEMO_FREE_UP") {
            let paths: Vec<PathBuf> = std::env::split_paths(&clips).collect();
            app.ask_free_up(&paths);
        }
        if let Some(clip) = std::env::var_os("HASTECLIP_DEMO_RENAME").map(PathBuf::from) {
            app.rename_clip(clip);
        }
        // `HASTECLIP_DEMO_LINK=<clip>` opens its Share with a link dialog.
        if let Some(clip) = std::env::var_os("HASTECLIP_DEMO_LINK").map(PathBuf::from) {
            app.open_link_dialog(clip);
        }
        // `HASTECLIP_DEMO_PREBUILD=<clip>` builds its scrub frames in the
        // background at launch, as after saving it.
        if let Some(clip) = std::env::var_os("HASTECLIP_DEMO_PREBUILD").map(PathBuf::from) {
            proxy::prebuild(&app.ctx(), &clip);
        }
        // `HASTECLIP_OPEN_SOURCES=1` opens on the Sources page; naming
        // sources (`=screen,webcam`) opens their cards too, `crop` the crop's page.
        let open_sources = std::env::var_os("HASTECLIP_OPEN_SOURCES").is_some();
        if let Some(v) = std::env::var_os("HASTECLIP_OPEN_SOURCES") {
            app.page = Page::Sources;
            sources_ui::open_dev(&mut app, &v.to_string_lossy());
        }
        // `HASTECLIP_OPEN_VOICE_REVIEW=1` opens the voice samples review.
        let open_review = std::env::var_os("HASTECLIP_OPEN_VOICE_REVIEW").is_some();
        if open_review {
            app.open_voice_review();
        }
        // `HASTECLIP_OPEN_SETTINGS=1` opens on Settings; `=video` (any
        // pane's name) on that pane; `=names` on the Clips pane's
        // Clip names page, any other value searches for it.
        if let Some(v) = std::env::var_os("HASTECLIP_OPEN_SETTINGS") {
            app.page = Page::Settings;
            match v.to_string_lossy().as_ref() {
                "1" | "" => {}
                "names" => {
                    app.settings_pane = settings_ui::Pane::Clips;
                    app.naming_open = true;
                }
                q => match settings_ui::Pane::ALL.into_iter().find(|p| p.label().eq_ignore_ascii_case(q)) {
                    Some(pane) => app.settings_pane = pane,
                    None => app.settings_query = q.to_owned(),
                },
            }
        }
        if !recovered.is_empty() {
            app.toast("Recovered a recording that was cut off last time");
        }
        // Input Monitoring allowed: shortcuts work while a game holds the screen too.
        #[cfg(target_os = "macos")]
        app.start_key_tap();
        // Never miss a moment: arm the buffer on launch (unless it can't work
        // yet, or the permissions screen is up: it starts when that closes).
        if app.settings.auto_start_buffer && !open_sources && !open_review && !app.setup_open && app.permission != capture::Permission::Denied {
            app.start_replay_buffer();
        }
        if demo::on() && demo::clipping_from_start() {
            app.start_replay_buffer();
        }
        app
    }

    fn refresh_clips(&mut self) {
        store::set_library(&self.settings.output_dir);
        self.clips = clips::scan(&self.settings.output_dir);
        self.library_version += 1;
        if self.collections.library() != self.settings.output_dir {
            self.collections = collections::Collections::load(&self.settings.output_dir);
        }
        if self.links.library() != self.settings.output_dir {
            self.links = links::Links::load(&self.settings.output_dir);
        }
        if self.folded.library() != self.settings.output_dir {
            self.folded = library::FoldedDays::load(&self.settings.output_dir);
        }
        self.collections.reconnect(&self.clips);
        self.selection.retain(&self.clips);
        self.library_filter.retain(&self.clips, &self.collections, &self.links);
        // The last screenshot gone: the choice of clips or screenshots goes too.
        if !self.clips.iter().any(clips::Clip::is_image) {
            self.library_kind = library::ShowKind::All;
        }
    }

    fn toast(&mut self, text: impl Into<String>) {
        self.toast_icon(egui_phosphor::regular::CHECK_CIRCLE, text);
    }

    /// A toast with an icon that says what kind of thing happened.
    pub(crate) fn toast_icon(&mut self, icon: &'static str, text: impl Into<String>) {
        self.toast = Some(shell::Toast { text: text.into(), icon, error: false, at: Instant::now(), action: None, for_: TOAST_FOR });
    }

    /// A toast with a button (Show), up a little longer.
    pub(crate) fn toast_action(&mut self, icon: &'static str, text: impl Into<String>, label: &str, action: shell::ToastAction) {
        self.toast = Some(shell::Toast { text: text.into(), icon, error: false, at: Instant::now(), action: Some((label.to_owned(), action)), for_: Duration::from_secs(6) });
    }

    fn toast_error(&mut self, text: impl Into<String>) {
        self.toast = Some(shell::Toast { text: text.into(), icon: egui_phosphor::regular::WARNING_CIRCLE, error: true, at: Instant::now(), action: None, for_: Duration::from_secs(5) });
    }

    /// Translate the UI's `RecordSettings` into the backend's `EncodeSettings`.
    fn encode_settings(&self) -> capture::EncodeSettings {
        capture::EncodeSettings {
            output_dir: self.settings.output_dir.clone(),
            container_ext: self.settings.container.ext().to_owned(),
            fps: self.settings.fps,
            blend_samples: self.settings.blend_samples(),
            blend_shape: self.settings.blend_shape.into(),
            video_bitrate_kbps: self.settings.video_bitrate_mbps * 1000,
            size: self.settings.video_size(),
            use_hardware: self.settings.encoder != Encoder::Software,
            replay_seconds: self.settings.replay_seconds,
            video: self.video_source(),
            away_screen: Some(self.away_screen.clone()),
            // Always a webcam layer, hidden while there's no webcam: one can
            // then be added, changed or removed while capturing.
            webcam: Some(self.webcam_source().unwrap_or_else(|| capture::webcam::Webcam {
                device: String::new(),
                format: None,
                placement: self.webcam_placement.clone(),
            })),
            // Kept in step every frame: sources added, taken out, moved or
            // reordered show while capturing.
            layers: self.capture_layers.clone(),
            screen: self.screen_placement.clone(),
            sources: self.capture_sources(),
            stream: self.stream_settings(),
        }
    }

    /// Where Go live sends the stream, and how: the destinations turned on
    /// and set up, none if there are none.
    fn stream_settings(&self) -> Option<capture::stream::StreamSettings> {
        if !capture::STREAMING || self.settings.stream.ready().next().is_none() {
            return None;
        }
        // Over RTMPS where a service has it and ffmpeg can (asked once,
        // and only for a service that has it).
        let rtmps = self.settings.stream.ready().any(|d| d.service.server(true) != d.service.server(false)) && capture::stream::rtmps();
        let destinations: Vec<_> = self.settings.stream.ready().filter_map(|d| d.destination(rtmps)).collect();
        (!destinations.is_empty()).then(|| capture::stream::StreamSettings { destinations, status: self.stream_status.clone() })
    }

    /// Keep the webcam's box, and the screen's when it's placed by hand,
    /// their own shape in the frame being recorded: a box is fractions of
    /// the frame, so switching to a display of another shape would stretch
    /// it into that shape (a strip on an ultrawide).
    fn refit_boxes(&mut self) {
        if self.settings.webcams.is_empty() && self.settings.visuals.is_empty() && self.settings.screen.placed.is_none() {
            return;
        }
        let key = (self.video_source(), self.settings.video_size());
        // Asked again now and then too: a display's mode can change, or it
        // can be plugged in, under the same choice. Not while capturing: its
        // frame stays the size it started at, whatever's chosen meanwhile.
        let capturing = !self.rec_state.idle();
        if self.frame_size.as_ref().is_none_or(|(k, _, at)| !capturing && (*k != key || at.elapsed() > Duration::from_secs(2))) {
            let size = capture::frame_size(&key.0, key.1);
            self.frame_size = Some((key, size, Instant::now()));
        }
        let Some(frame_aspect) = self.frame_aspect() else { return };
        let screen_aspect = self.screen_picture().map(|(w, h)| w as f32 / h.max(1) as f32);
        if let Some(b) = self.settings.screen.placed.as_mut() {
            b.refit(frame_aspect, screen_aspect);
        }
        for key in self.settings.webcams.iter().map(|w| w.key).collect::<Vec<_>>() {
            let camera_aspect = self.camera_size(key).map(|(w, h)| w as f32 / h.max(1) as f32);
            if let Some(w) = self.settings.webcam_mut(key) {
                w.placement.refit(frame_aspect, camera_aspect);
            }
        }
        for key in self.settings.visuals.iter().map(|d| d.key).collect::<Vec<_>>() {
            let aspect = self.visual_size(key).map(|(w, h)| w as f32 / h.max(1) as f32);
            if let Some(b) = self.settings.visual_mut(key).and_then(|d| d.placed.as_mut()) {
                b.refit(frame_aspect, aspect);
            }
        }
    }

    /// The shape (width / height) of the frame being recorded, when it's
    /// known (see `refit_boxes`).
    pub(crate) fn frame_aspect(&self) -> Option<f32> {
        let (_, size, _) = self.frame_size.as_ref()?;
        size.map(|(w, h)| w as f32 / h.max(1) as f32)
    }

    /// Webcam `key`'s picture size as it's turned, once it's live.
    pub(crate) fn camera_size(&self, key: u64) -> Option<(u32, u32)> {
        let w = self.settings.webcam(key)?;
        let turned = w.placement.turns % 2 == 1;
        match capture::webcam::status_of(&w.id) {
            capture::webcam::Status::Live { width, height } if turned => Some((height, width)),
            capture::webcam::Status::Live { width, height } => Some((width, height)),
            _ => None,
        }
    }

    /// The first webcam, for where only one is drawn (`capture::layers::MANY`):
    /// the scene shown's, else one another scene shows (hidden here, but
    /// kept, so switching scenes doesn't start the preview again).
    pub(crate) fn webcam_source(&self) -> Option<capture::webcam::Webcam> {
        let w = self.settings.first_webcam().or_else(|| self.settings.cameras_in_use().first().copied()).filter(|_| capture::webcam::AVAILABLE)?;
        Some(capture::webcam::Webcam { device: w.id.clone(), format: w.format.map(Into::into), placement: self.webcam_placement.clone() })
    }

    /// The video's sources for the capture, bottom first, as they're placed
    /// now (see `capture::layers`): the screen, each webcam (hidden while
    /// it's off), each other display. Under them, hidden, the preset's
    /// sources the scene shown doesn't have: they keep going, so switching
    /// to a scene with them shows them at once (taken out, a window's
    /// capture would start again, a page load again, a camera reopen).
    fn layers_for_capture(&self) -> Vec<capture::Layer> {
        use capture::{Layer, LayerSource};
        use settings::LayerKey;
        let mut layers = Vec::new();
        let outside = self.settings.outside_scene().into_iter().map(|l| (l, true));
        for (l, out) in outside.chain(self.settings.layer_order().into_iter().rev().map(|l| (l, false))) {
            match l {
                LayerKey::Screen => layers.push(Layer { id: 0, source: LayerSource::Main }),
                LayerKey::Webcam(key) if capture::webcam::AVAILABLE => {
                    if let Some(w) = self.settings.webcam(key) {
                        let placement = if w.enabled && !out { w.placement.into() } else { capture::webcam::Placement::hidden() };
                        layers.push(Layer { id: 1 << 32 | key, source: LayerSource::Camera { device: w.id.clone(), format: w.format.map(Into::into), placement } });
                    }
                }
                LayerKey::Visual(key) if capture::layers::MANY => {
                    use settings::Visual;
                    let Some(d) = self.settings.visual(key) else { continue };
                    let mut placement = self.settings.visual_placement(d);
                    placement.hidden |= out;
                    let source = match &d.visual {
                        Visual::Display { id, .. } => LayerSource::Display { id: id.clone(), placement },
                        Visual::Window { app, .. } => LayerSource::Window { app: app.clone(), placement },
                        Visual::Colour { rgba } => LayerSource::Colour { rgba: *rgba, placement },
                        Visual::Browser { url, width, height, .. } if capture::layers::BROWSER && url.len() > "https://".len() => LayerSource::Browser { url: url.clone(), width: *width, height: *height, placement },
                        Visual::Browser { .. } => continue,
                        // Its picture, once it's made (`refresh_visual_pictures`).
                        Visual::Image { .. } | Visual::Text { .. } => match self.visual_picture(key) {
                            Some(p) => LayerSource::Picture { picture: capture::layers::Picture(p), placement },
                            None => continue,
                        },
                    };
                    // Loaded again (a web page's Reload): another layer, so
                    // the capture makes it afresh.
                    let again = match &d.visual {
                        Visual::Browser { reload, .. } => u64::from(*reload) << 48,
                        _ => 0,
                    };
                    layers.push(Layer { id: (2 << 32 | key) ^ again, source });
                }
                LayerKey::Visual(_) => {}
                LayerKey::Webcam(_) => {}
            }
        }
        layers
    }

    /// Show scene `id` (see `apply_switch`): live while capturing too.
    pub(crate) fn show_scene(&mut self, id: u64) {
        self.pending_switch = Some(Switch::Scene(id));
    }

    /// Use preset `id` (see `apply_switch`). `restart`: clipping, all that
    /// runs, stops first and starts again with it; a capture keeps what it
    /// started with, so otherwise it's only done while nothing runs.
    pub(crate) fn use_preset(&mut self, id: u64, restart: bool) {
        self.pending_switch = Some(Switch::Preset { id, restart });
    }

    /// The scene or preset asked for, into the live settings, and what
    /// belonged to the one before let go.
    fn apply_switch(&mut self) {
        let Some(switch) = self.pending_switch.take() else { return };
        match switch {
            Switch::Scene(id) => {
                if !self.settings.switch_scene(id) {
                    return;
                }
                eprintln!("scene: {}", self.settings.scene().map_or("?", |s| s.name.as_str()));
            }
            Switch::Preset { id, restart } => {
                let restart = restart && self.rec_state == RecState { clipping: true, ..Default::default() };
                if !self.rec_state.idle() && !restart {
                    return;
                }
                if restart {
                    self.stop_clipping();
                }
                let switched = self.settings.switch_preset(id);
                if switched {
                    push_live_audio(&self.live_audio, &self.settings);
                    self.canvas.pictures.clear();
                    self.canvas.sizes.clear();
                    self.listening = None;
                    eprintln!("preset: {}", self.settings.preset().map_or("?", |p| p.name.as_str()));
                }
                if restart {
                    self.start_mode(capture::Mode::ReplayBuffer);
                }
                if !switched {
                    return;
                }
            }
        }
        // What belonged to the scene before: its undo steps (they'd put its
        // places into this one), a drag, the crop's and camera's pages, and
        // the selection if it isn't here.
        self.forget_canvas_steps();
        self.sources_view.row_drag = None;
        self.sources_view.crop_open = false;
        self.webcam_view.crop = None;
        self.camera_panel = None;
        if let Some(canvas_ui::Selected::Layer(l)) = self.canvas.selected
            && !self.settings.layer_order().contains(&l)
        {
            self.canvas.selected = None;
        }
    }

    /// Where the chosen display is in `screens`: found by its id, by its
    /// place for settings from before ids were kept, else the main display
    /// (listed first).
    pub(crate) fn display_at(&self) -> usize {
        match &self.settings.display_id {
            Some(id) => self.screens.iter().position(|d| &d.id == id).unwrap_or(0),
            None if self.settings.display_index < self.screens.len() => self.settings.display_index,
            None => 0,
        }
    }

    pub(crate) fn video_source(&self) -> capture::VideoSource {
        match &self.settings.capture {
            settings::CaptureTarget::Apps { apps, away_screen } if capture::app_capture() => capture::VideoSource::Apps {
                ids: apps.iter().map(|a| a.id.clone()).collect(),
                away_when_unfocused: *away_screen,
                hook: self.game_hook(),
            },
            _ => self.display_source(&self.screens.get(self.display_at()).map(|d| d.id.clone()).unwrap_or_else(|| "0".to_owned())),
        }
    }

    /// Recording the display `id`.
    pub(crate) fn display_source(&self, id: &str) -> capture::VideoSource {
        capture::VideoSource::Screen { id: id.to_owned(), hook: self.game_hook(), method: self.settings.screen_capture.into() }
    }

    /// When to use the game capture hook: the setting, and the games with
    /// anti-cheat it's allowed on.
    fn game_hook(&self) -> capture::GameHook {
        capture::GameHook {
            auto: self.settings.game_capture,
            allowed: self.settings.game_capture_allowed.iter().map(|g| g.id.clone()).collect(),
        }
    }

    /// The enabled audio sources, resolved for the capture backend ("default"
    /// mic → the OS default device right now). Two sources on the same mic would
    /// record it twice; only the first is kept.
    pub(crate) fn capture_sources(&self) -> Vec<capture::sources::AudioSource> {
        use capture::sources::{AudioSource, SourceKind as Cap};
        let mut out: Vec<AudioSource> = Vec::new();
        for s in self.settings.audio_sources.iter().filter(|s| s.enabled && (s.in_mix || s.own_track)) {
            let kind = match &s.kind {
                SourceKind::Microphone { device } => match self.audio.resolve_input(device) {
                    Some(device) => Cap::Microphone { device },
                    None => continue,
                },
                SourceKind::Desktop { exclude_apps } => Cap::Desktop { exclude_app_sources: *exclude_apps },
                SourceKind::App { bundle_id, .. } => Cap::App { bundle_id: bundle_id.clone() },
            };
            if out.iter().any(|o| o.kind == kind) {
                continue;
            }
            out.push(AudioSource { id: s.id.clone(), name: s.name.clone(), kind, in_mix: s.in_mix, own_track: s.own_track });
        }
        out
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        #[cfg(target_os = "macos")]
        shell::keep_window_buttons(frame);
        let ctx = ui.ctx().clone();
        let mut laps = Laps::start();

        // Out of the tray, however it was shown (a dock; a desktop that
        // minimized it instead, or wouldn't): drawn after the frame that sent
        // it there, and used (the focus, or the pointer on it). On Windows
        // it isn't drawn while it's there.
        if self.in_tray
            && ctx.cumulative_frame_nr() > self.tray_frame
            && ctx.input(|i| {
                i.viewport().focused == Some(true)
                    || i.events.iter().any(|e| matches!(e, egui::Event::PointerMoved(_) | egui::Event::MouseWheel { .. } | egui::Event::PointerButton { .. }))
            })
        {
            self.in_tray = false;
        }

        self.navigate(&ctx);
        if let Some(action) = self.demo.as_mut().and_then(demo::Demo::take_action) {
            self.demo_action(action);
        }

        // Left the viewer some other way (a hotkey that shows the library): stop it.
        if self.page != Page::View && self.viewer.is_some() {
            self.viewer = None;
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        }

        // The rail on every page but the player and editor, which get the
        // whole window (capture keeps running underneath and the shortcuts
        // still work).
        let full = matches!(self.page, Page::Edit | Page::View);
        if !full {
            egui::Panel::left("rail")
                .resizable(false)
                .show_separator_line(false)
                .exact_size(shell::RAIL_W)
                .frame(egui::Frame::NONE)
                .show(ui, |ui| self.rail(ui));
        }
        // ⌘, opens Settings, as in every Mac app.
        if !full && !ctx.egui_wants_keyboard_input() && !ui_kit::overlay_open(&ctx) && ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Comma)) {
            self.page = Page::Settings;
        }
        self.track_health();
        // Toasts sit over the middle of the page, clear of the rail, and above
        // the player's controls.
        self.toast_x_offset = if full { 0.0 } else { shell::RAIL_W / 2.0 };
        self.toast_bottom = match self.page {
            Page::View => 210.0,
            Page::Edit => 300.0,
            _ => 22.0,
        };
        // Clips: over the clips, not the panel beside them (as the
        // selection bar is), and above the selection bar while it's up.
        if self.page == Page::Clips
            && let Some((centre, bottom)) = ctx.data(|d| d.get_temp::<(f32, f32)>(egui::Id::new("toasts_over_clips")))
        {
            self.toast_x_offset = centre - ctx.content_rect().center().x;
            self.toast_bottom = bottom;
        }
        self.toasts(&ctx, frame);

        // The viewer in fullscreen: the picture from edge to edge.
        let edge_to_edge = self.page == Page::View && ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
        let p = theme::pal(ui);
        // The title bar is the rail's colour, framing the page with it; with
        // no rail (the player, editor) it's the page's, so it reads as the
        // page's own top. It changes as the rail goes and comes back.
        #[cfg(target_os = "windows")]
        {
            let t = theme::toggle(&ctx, egui::Id::new("title_bar_page"), full, theme::NORMAL);
            shell::tint_title_bar(frame, theme::mix(p.ground, p.base, t), p.dark);
        }
        let framed = !full && shell::framed_page();
        let page_rect = ui.available_rect_before_wrap();
        // The frame's colour shows round the page's rounded corner.
        if framed {
            ui.painter().rect_filled(page_rect, 0, p.ground);
        }
        let central = if edge_to_edge || full {
            egui::Frame::NONE.fill(if edge_to_edge { Color32::BLACK } else { p.base })
        } else {
            // Pages that scroll run to the window's bottom edge.
            egui::Frame::new()
                .fill(p.base)
                .corner_radius(egui::CornerRadius { nw: if framed { shell::PAGE_CORNER } else { 0 }, ..Default::default() })
                .inner_margin(egui::Margin { left: ui_kit::PAGE_MARGIN, right: ui_kit::PAGE_MARGIN, top: if shell::under_titlebar() { 0 } else { 4 }, bottom: 0 })
        };
        laps.lap("bars");
        // A dialog or menu open (now, or drawn last frame): it gets the keys.
        let overlay = self.rename.is_some()
            || self.collection_dialog.is_some()
            || self.confirm_delete.is_some()
            || self.free_up.is_some()
            || self.link_dialog.is_some()
            || self.confirm_reset
            || self.close_dialog
            || self.setup_open
            || self.show_keys.is_some()
            || ctx.memory(|m| m.top_modal_layer().is_some())
            || ctx.any_popup_open();
        ui_kit::set_overlay_open(&ctx, overlay);
        // Another page, game, collection or Settings pane starts at the top.
        // Back from the player or editor, the library stays where it was (and
        // shows the clip).
        let view = (self.page, self.library_filter.clone(), (self.page == Page::Settings).then_some((self.settings_pane, self.naming_open)));
        let reset = self.last_view.as_ref().is_some_and(|(page, filter, settings)| {
            (*page != view.0 && !matches!(page, Page::Edit | Page::View))
                || (*filter != view.1 && view.0 == Page::Clips)
                || (settings.map(|s| s.0) != view.2.map(|s| s.0) && view.0 == Page::Settings)
        });
        ui_kit::set_scroll_reset(&ctx, reset);
        // Another page's content fades in and rises, and so does another
        // Settings pane or Clip names. (Another game or collection doesn't:
        // the grid rearranges itself.)
        let arrived = self.last_view.as_ref().is_some_and(|(page, _, settings)| *page != view.0 || *settings != view.2);
        if arrived {
            ui_kit::page_changed(&ctx);
        }
        self.last_view = Some(view);
        egui::CentralPanel::default()
            .frame(central)
            .show(ui, |ui| match self.page {
                Page::Clips => self.library(ui, frame),
                Page::Sources => self.sources_page(ui),
                Page::Settings => self.settings_page(ui),
                Page::VoiceReview => self.voice_review_page(ui),
                // The player and editor have no rail or shared heading: all of
                // them arrives.
                Page::Edit => ui_kit::arrive(ui, |ui| self.editor_page(ui)),
                Page::View => ui_kit::arrive_on(ui, if edge_to_edge { Color32::BLACK } else { p.base }, |ui| self.viewer_page(ui, frame)),
            });
        if framed {
            shell::page_edge(ui.painter(), page_rect, p.line);
        }

        laps.lap(match self.page {
            Page::Clips => "library page",
            Page::Sources => "sources page",
            Page::Settings => "settings page",
            Page::VoiceReview => "voice samples page",
            Page::Edit => "editor page",
            Page::View => "player page",
        });
        self.rename_dialog(&ctx);
        self.collection_dialog(&ctx);
        self.delete_dialog(&ctx);
        self.free_up_dialog(&ctx);
        self.link_dialog(&ctx);
        self.close_dialog(&ctx);
        self.keys_dialog(&ctx);
        self.go_live_dialog(&ctx);
        #[cfg(target_os = "macos")]
        self.setup_dialog(&ctx);
        ui_kit::pointer_cursor(&ctx);
        ui_kit::lenient_clicks(&ctx);
        laps.lap("dialogs");
        laps.report();

        // Keep repainting: every frame while capturing (live timer), else a steady
        // tick so capture-thread events and folder changes are picked up promptly.
        if !self.rec_state.idle() || self.toast.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        } else {
            ctx.request_repaint_after(Duration::from_millis(300));
        }
        // Last: a demo records every frame.
        if let Some(demo) = &mut self.demo {
            demo.after(&ctx);
        }
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        if let Some(demo) = &mut self.demo {
            demo.input(ctx, raw_input);
        }
        ui_kit::replay_click(ctx, raw_input);
    }

    /// What runs every frame whether the window shows or not (it's hidden
    /// in the tray): shortcuts, voice, capture events, saves and uploads,
    /// and once a second the library, the game, Discord and the settings.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // From the first frame, so the press that starts a drag out is heard.
        #[cfg(target_os = "linux")]
        crate::wayland_drag::prepare(_frame);
        static LAUNCHED: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        let launched = *LAUNCHED.get_or_init(Instant::now);
        if let Some(secs) = std::env::var("HASTECLIP_DEV_BUFFER_QUIT").ok().and_then(|s| s.parse::<f64>().ok())
            && !self.quitting
            && launched.elapsed().as_secs_f64() > secs
        {
            eprintln!("dev: quitting");
            self.quit(ctx);
        }
        // `HASTECLIP_DEV_SAVE=<secs>`: save a clip once, that long after
        // opening (with the replay buffer running, as `HASTECLIP_DEV_BUFFER_QUIT`
        // starts it), to check what's recorded without a key press.
        static SAVED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if let Some(secs) = std::env::var("HASTECLIP_DEV_SAVE").ok().and_then(|s| s.parse::<f64>().ok())
            && launched.elapsed().as_secs_f64() > secs
            && self.rec_state.clipping
            && !SAVED.swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            eprintln!("dev: saving a clip");
            self.save_clip();
        }
        // `HASTECLIP_DEV_CAMERA_PANEL`: open on Sources with the camera panel
        // (to look at it without clicking there); `=crop`, the webcam's crop page.
        static CAMERA_PANEL: std::sync::Once = std::sync::Once::new();
        if std::env::var_os("HASTECLIP_DEV_CAMERA_PANEL").is_some()
            && let Some(w) = self.settings.first_webcam()
        {
            let id = w.id.clone();
            CAMERA_PANEL.call_once(|| {
                self.page = Page::Sources;
                if std::env::var("HASTECLIP_DEV_CAMERA_PANEL").is_ok_and(|v| v == "crop") {
                    self.webcam_view.crop = Some(crate::webcam_ui::CameraPanel::cropping(&id));
                } else {
                    self.camera_panel = Some(crate::webcam_ui::CameraPanel::open(&id));
                }
            });
        }
        // `HASTECLIP_DEV_PAGES=<secs>`: go round Clips, Sources and Settings,
        // one every so many seconds (with `HASTECLIP_DEV_LAPS` to time it).
        if let Some(every) = std::env::var("HASTECLIP_DEV_PAGES").ok().and_then(|s| s.parse::<f64>().ok()) {
            static STEP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let step = (launched.elapsed().as_secs_f64() / every) as u64;
            if STEP.swap(step, std::sync::atomic::Ordering::Relaxed) != step && step > 0 {
                self.page = [Page::Sources, Page::Settings, Page::Clips][(step as usize - 1) % 3];
                eprintln!("dev: to {:?}", self.page);
            }
        }
        // `HASTECLIP_DEV_SCREENSHOT=<secs>`: take a screenshot every so many
        // seconds, as its shortcut does (to check them without a key press).
        if let Some(every) = std::env::var("HASTECLIP_DEV_SCREENSHOT").ok().and_then(|s| s.parse::<f64>().ok()) {
            static STEP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let step = (launched.elapsed().as_secs_f64() / every) as u64;
            if STEP.swap(step, std::sync::atomic::Ordering::Relaxed) != step && step > 0 {
                eprintln!("dev: taking a screenshot");
                if !self.quick_save.screenshot() {
                    self.take_screenshot();
                }
            }
        }
        // `HASTECLIP_DEV_SWITCH=<secs>` (with the above): switch between the
        // whole screen and games and apps every so many seconds while
        // clipping, and save a clip a second before quitting (to check
        // switching mid-clip).
        if let Some(every) = std::env::var("HASTECLIP_DEV_SWITCH").ok().and_then(|s| s.parse::<f64>().ok())
            && self.rec_state.clipping
        {
            static STEP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let step = (launched.elapsed().as_secs_f64() / every) as u64;
            if STEP.swap(step, std::sync::atomic::Ordering::Relaxed) != step && step > 0 {
                let apps = matches!(self.settings.capture, settings::CaptureTarget::Apps { .. });
                eprintln!("dev: switching to {}", if apps { "the whole screen" } else { "games and apps" });
                self.settings.capture = if apps { settings::CaptureTarget::Screen } else { settings::CaptureTarget::Apps { apps: vec![settings::CaptureApp { id: "com.apple.Terminal".into(), name: "Terminal".into() }], away_screen: true } };
            }
            let quit_at = std::env::var("HASTECLIP_DEV_BUFFER_QUIT").ok().and_then(|s| s.parse::<f64>().ok()).unwrap_or(f64::MAX);
            static SAVED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if launched.elapsed().as_secs_f64() > quit_at - 1.5 && !SAVED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                eprintln!("dev: saving a clip");
                self.save_clip();
            }
        }
        // `HASTECLIP_DEV_SCENES=<secs>`: show the next scene every so many
        // seconds while capturing (to check switching live: each switch is
        // said here, and no camera should open again). With
        // `HASTECLIP_SETTINGS=<file>` it saves nothing.
        if let Some(every) = std::env::var("HASTECLIP_DEV_SCENES").ok().and_then(|s| s.parse::<f64>().ok())
            && !self.rec_state.idle()
            && self.settings.scenes.len() > 1
        {
            static STEP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let step = (launched.elapsed().as_secs_f64() / every) as u64;
            if STEP.swap(step, std::sync::atomic::Ordering::Relaxed) != step && step > 0 {
                let scenes = &self.settings.scenes;
                let at = scenes.iter().position(|s| s.id == self.settings.active_scene).unwrap_or(0);
                let next = scenes[(at + 1) % scenes.len()].id;
                eprintln!("dev: the next scene");
                self.show_scene(next);
            }
        }
        // A clip plays in its card on Clips only, and hearing a microphone is
        // for the Sources page only: never left on. Here, not in the UI: in
        // the tray only this runs, and going there leaves Sources for Clips.
        if self.page != Page::Clips {
            self.hover_play = None;
        }
        if self.page != Page::Sources {
            self.listening = None;
        }
        #[cfg(any(windows, target_os = "macos"))]
        self.start_in_tray(ctx, _frame);
        #[cfg(windows)]
        self.come_to_front(ctx, _frame);
        self.background(ctx);
        self.pump_tray(ctx);
        self.handle_close(ctx);
        self.every_second();
        self.keep_open(ctx);
        // A steady tick, also while hidden (nothing else wakes it then).
        ctx.request_repaint_after(if !self.rec_state.idle() { Duration::from_millis(100) } else { Duration::from_millis(300) });
    }

    fn on_exit(&mut self) {
        eprintln!("quitting");
        // A downloaded update goes in once we're gone: the next launch is the new version.
        self.updater.install_on_exit(false);
    }
}

/// Not used this long (another window has the focus, a game say, and
/// nothing's scrolled or pressed over this one), the player and the editor
/// let go of the scrub engine (`App::keep_open`): long enough that looking
/// something up and coming back finds it open, and it's opened again in a
/// moment anyway (scrubbing goes through the playback decoder meanwhile).
const BEHIND_FOR: Duration = Duration::from_secs(10);

// --- Running in the background: the tray, closing the window ---
impl App {
    /// Windows, started with the computer: the window opened cloaked (see
    /// `main`), kept off the screen and out of the taskbar while eframe
    /// draws its first frame and shows it (as it does whatever was asked);
    /// it's hidden in the tray at the end of that frame, then uncloaked. Shown
    /// from the tray, it comes up with that frame, not blank: minimized
    /// before its first frame instead, it had never been drawn, and making a
    /// hidden window minimized showed it a moment (a taskbar button at
    /// login). Without a tray icon to open it from, it's uncloaked at once,
    /// and shows.
    ///
    /// macOS the same way, the window transparent for its cloak: shown by
    /// eframe after its first frame, it was on screen 3.4 s after a launch
    /// with `--background` (measured with a development build). And winit
    /// makes an app the active one as it starts, over whatever's in front:
    /// started with the computer, it isn't (see `main`).
    #[cfg(any(windows, target_os = "macos"))]
    fn start_in_tray(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        if std::mem::take(&mut self.to_tray) {
            self.hide_window(ctx);
            return;
        }
        // In the tray now (minimized there on Windows; on macOS, ordered
        // out), or not going there.
        #[cfg(windows)]
        let there = ctx.input(|i| i.viewport().minimized == Some(true));
        #[cfg(target_os = "macos")]
        let there = !shell::window_shown(frame);
        if self.cloaked && (!self.in_tray || there) {
            shell::cloak(frame, false);
            self.cloaked = false;
        }
    }

    /// About once a second, shown or not (in the tray only `logic` runs):
    /// the library rescanned when its folders changed, the game being
    /// clipped and what Discord shows, and the settings saved.
    fn every_second(&mut self) {
        let now = Instant::now();
        if self.last_poll.is_some_and(|t| now.duration_since(t) < Duration::from_secs(1)) {
            return;
        }
        let mut laps = Laps::start();
        self.last_poll = Some(now);
        self.permission = permission_now();
        laps.lap("permission");
        // Rescanned only when a folder actually changed (cheap, no watcher thread).
        let stamp = clips::stamp(&self.settings.output_dir);
        if stamp != self.library_stamp {
            self.library_stamp = stamp;
            self.refresh_clips();
        }
        laps.lap("library scan");
        self.track_game();
        self.update_presence();
        laps.lap("game, discord");
        // Persist settings as they change; no Save button to forget.
        let json = self.settings.to_json();
        if json != self.saved_settings && self.persist_settings {
            RecordSettings::save_json(&json);
            self.saved_settings = json;
        }
        laps.lap("settings");
        laps.report();
    }

    /// What the player and the editor keep open, by what the window is
    /// doing (see `player::Window`): everything while it's used; the scrub
    /// engine goes once it hasn't been for [`BEHIND_FOR`] (a game in front);
    /// out of sight (minimized, in the tray) the playback decoder too, and a
    /// card's preview in the library.
    fn keep_open(&mut self, ctx: &egui::Context) {
        let (minimized, used) = ctx.input(|i| {
            // The focus, or the wheel or a button over it: Windows scrolls the
            // window under the pointer, focused or not (a clip on another
            // screen scrubbed with the wheel, a game in focus). A pointer
            // only passing over it on its way elsewhere doesn't count.
            let touched = i.events.iter().any(|e| matches!(e, egui::Event::MouseWheel { .. } | egui::Event::PointerButton { pressed: true, .. }));
            (i.viewport().minimized == Some(true), i.viewport().focused == Some(true) || touched)
        });
        if used {
            self.unused_since = None;
        } else if self.unused_since.is_none() {
            self.unused_since = Some(Instant::now());
        }
        // Not a development test instance's: its window is often behind the
        // terminal it was started from while it's measured.
        let behind = self.persist_settings && self.unused_since.is_some_and(|t| t.elapsed() >= BEHIND_FOR);
        let window = if self.in_tray || minimized {
            player::Window::Hidden
        } else if behind {
            player::Window::Behind
        } else {
            player::Window::InUse
        };
        let mut let_go = window == player::Window::Hidden && self.hover_play.take().is_some();
        if let Some(v) = &mut self.viewer {
            let_go |= v.set_window(ctx, window);
        }
        if let Some(e) = &mut self.editor {
            let_go |= e.set_window(ctx, window);
        }
        // Out of sight nothing's drawn, so nothing has the renderer (or the
        // driver) let go of what was let go of: settled for a few seconds,
        // as the decoders close (each finishes the frame it's on first).
        let now = Instant::now();
        if let_go {
            if player::debug() {
                eprintln!("{:>8.3} app: decoders let go of, settling the renderer", player::uptime());
            }
            self.settle_until = Some(now + Duration::from_secs(3));
        }
        if window == player::Window::Hidden && self.settle_until.is_some_and(|t| now < t) {
            gpu_frames::settle();
        }
    }

    fn background(&mut self, ctx: &egui::Context) {
        // Global shortcuts: these fire even while a game is focused. Paused while
        // the Settings recorder listens, so pressing the current key rebinds it
        // instead of starting a recording.
        let pressed = match &mut self.hotkeys {
            Ok(h) => {
                h.sync(&self.settings.shortcuts);
                // Changed in the desktop's own settings: ours follow.
                for (action, keys) in h.adopted() {
                    self.settings.shortcuts.set(action, keys);
                }
                h.pressed()
            }
            Err(_) => Vec::new(),
        };
        for (action, saved) in pressed {
            if self.recording_shortcut.is_some() {
                continue;
            }
            match action {
                settings::ShortcutAction::ToggleBuffer => self.toggle_buffer(),
                settings::ShortcutAction::ToggleRecord => self.toggle_record(),
                // Saved already, the moment the key went down.
                settings::ShortcutAction::SaveClip if saved => self.clip_saving(),
                settings::ShortcutAction::SaveClip => self.save_clip(),
                // Taken already, the moment the key went down.
                settings::ShortcutAction::Screenshot if saved => {}
                settings::ShortcutAction::Screenshot => self.take_screenshot(),
            }
        }
        // A scene or preset asked for: before the capture's kept in step
        // below (and the boxes refitted), never mid-change on a page.
        self.apply_switch();

        // "HasteClip that": saved already, as it was said.
        for _ in 0..self.voice.heard() {
            self.clip_saving();
        }
        // Not while samples are played on the speakers: the detector would hear them.
        // (Never in a demo recording: it clips nothing, and the mic stays shut.)
        // While recording, it marks the moment.
        let listen = self.settings.voice_clip && (self.rec_state.clipping || self.rec_state.recording) && self.recording_shortcut.is_none() && self.page != Page::VoiceReview && self.demo.is_none();
        self.voice.collect(self.settings.voice_samples);
        let mic = listen.then(|| self.voice_mic()).flatten();
        self.voice.listen(mic.as_deref());
        self.pump_capture_events();
        self.follow_sessions();
        let capturing = games::Capturing { buffer: self.rec_state.clipping.then_some(self.settings.replay_seconds), record: self.rec_state.recording };
        self.games.set(capturing, &self.listed_apps());
        self.games.events.set_options(self.settings.game_details.then(|| self.settings.game_titles.clone()));
        let armed = (self.rec_state.clipping && self.recording_shortcut.is_none()).then(|| service::Armed {
            library: self.settings.output_dir.clone(),
            folder_per_game: self.settings.folder_per_game,
            game_folders: self.settings.game_folders.clone(),
            sound: self.settings.save_sound.clone(),
        });
        self.quick_save.arm(armed);
        let marking = (self.rec_state.recording && self.recording_shortcut.is_none()).then(|| self.settings.save_sound.clone());
        self.quick_save.arm_marking(marking);
        let shot = (capture::SCREENSHOTS && self.recording_shortcut.is_none() && self.demo.is_none()).then(|| self.shot_armed());
        self.quick_save.arm_screenshot(shot);
        self.updater.set_auto(self.settings.auto_update);
        self.pump_renders();
        self.pump_mp3s();
        self.import_dropped(ctx);
        self.pump_transfer();
        self.pump_links();
        // Every frame, not just while the Sources page draws: leaving the page must
        // stop the meters' capture, or macOS keeps showing its recording indicator.
        self.ensure_level_monitor();
        self.ensure_video_preview();
        self.sync_capture_video();
        self.sync_capture_sources();
        self.sync_capture_settings();
        self.sync_capture_blend();
        self.watch_stream();
        self.refit_boxes();
        // A locked button clicked anywhere: why, in a toast, as a locked
        // setting says it.
        if let Some(why) = ui_kit::take_locked_click(ctx) {
            self.toast_icon(egui_phosphor::regular::LOCK_SIMPLE, why);
        }
        *self.webcam_placement.lock().unwrap() = match self.settings.first_webcam() {
            Some(w) if w.enabled => w.placement.into(),
            _ => capture::webcam::Placement::hidden(),
        };
        *self.screen_placement.lock().unwrap() = self.settings.screen_placement();
        self.refresh_visual_pictures();
        let layers = self.layers_for_capture();
        {
            let mut shared = self.capture_layers.lock().unwrap();
            if *shared != layers {
                *shared = layers;
            }
        }
        // Keep each webcam open whenever a scene of the preset shows it (the
        // one shown, or another: switching to it then shows it at once),
        // previewed or recorded or not: closing a camera can reset its own
        // settings. Hidden in every scene, it's closed (its light goes out),
        // but while its crop's set (its picture's cropped on) or its Camera
        // page is open (its picture, and on Windows its settings, are the
        // open camera's); a capture takes it up again. Where only one camera
        // can be open, the one drawn (see `webcam_source`).
        let paged = self.webcam_view.crop.as_ref().or(self.camera_panel.as_ref()).map(|p| p.device.clone());
        let mut webcams: Vec<&settings::WebcamCfg> = if capture::webcam::MANY {
            self.settings.cameras_in_use()
        } else {
            let drawn = self.settings.first_webcam().filter(|w| w.enabled).or_else(|| self.settings.cameras_in_use().first().copied());
            drawn.into_iter().collect()
        };
        if let Some(w) = paged.as_deref().and_then(|d| self.settings.webcam_of(d))
            && !webcams.iter().any(|x| x.id == w.id)
            && (capture::webcam::MANY || webcams.is_empty())
        {
            webcams.push(w);
        }
        let cameras: Vec<(String, Option<capture::webcam::Format>)> = webcams.into_iter().filter(|_| capture::webcam::AVAILABLE).map(|w| (w.id.clone(), w.format.map(Into::into))).collect();
        if cameras != self.kept_cameras {
            // Their settings go back on as each opens.
            for (device, _) in &cameras {
                capture::webcam::remember_controls(device, self.settings.camera_controls.get(device).map(|m| m.iter().map(|(k, v)| (*k, *v)).collect()).unwrap_or_default());
            }
            capture::webcam::keep_open(cameras.clone());
            self.kept_cameras = cameras;
        }
    }

    /// Do what the tray (or another launch) asked.
    fn pump_tray(&mut self, ctx: &egui::Context) {
        let Some(tray) = &self.tray else { return };
        tray.update(self.rec_state.clipping);
        for cmd in tray.take() {
            match cmd {
                tray::Cmd::Open => self.show_window(ctx),
                tray::Cmd::SaveClip => self.save_clip(),
                tray::Cmd::ToggleBuffer => self.toggle_buffer(),
                tray::Cmd::Quit => self.quit(ctx),
            }
        }
    }

    /// Closing the window: hidden in the tray (as chosen; asked the first
    /// time), or quit.
    fn handle_close(&mut self, ctx: &egui::Context) {
        if self.quitting || self.tray.is_none() || !ctx.input(|i| i.viewport().close_requested()) {
            return;
        }
        if self.settings.close_asked && !self.settings.close_to_tray {
            // Closes: out of sight while it shuts down.
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        if self.settings.close_asked {
            self.hide_window(ctx);
        } else {
            self.close_dialog = true;
        }
    }

    fn hide_window(&mut self, ctx: &egui::Context) {
        // Nothing to watch while hidden: stop playing, close the preview.
        // (The editor keeps its edit, and lets go of its decoders: `keep_open`.)
        if matches!(self.page, Page::View | Page::Sources) {
            self.viewer = None;
            self.page = Page::Clips;
        }
        self.in_tray = true;
        self.tray_frame = ctx.cumulative_frame_nr();
        // Wayland can't hide a window: KWin is asked to; elsewhere on
        // Wayland it can only be minimized.
        #[cfg(target_os = "linux")]
        if crate::portal_shortcuts::wanted() {
            if kwin_window::available() {
                kwin_window::hide();
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            }
            return;
        }
        // Windows: minimized as well. Windows never tells eframe a hidden
        // window is hidden, so it went on drawing it, every 100 ms while
        // anything asked for a frame (all the time while clipping); a
        // minimized one gets no drawing at all, only `logic`. Minimized
        // first: hidden first, winit would show a maximized window again
        // to minimize it.
        #[cfg(windows)]
        {
            self.tray_maximized = ctx.input(|i| i.viewport().maximized == Some(true));
            self.to_front = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }

    fn show_window(&mut self, ctx: &egui::Context) {
        self.in_tray = false;
        // Nor bring one to the front: KWin is asked to.
        #[cfg(target_os = "linux")]
        if kwin_window::available() {
            kwin_window::raise();
            ctx.request_repaint();
            return;
        }
        // Restored too, where the tray minimizes it (`hide_window`).
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        // Maximized again if it was: opened started with the computer, it's
        // shown without taking the focus (see `main`), which Windows does by
        // restoring it to its normal size.
        #[cfg(windows)]
        if std::mem::take(&mut self.tray_maximized) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        // Windows: back from minimized, that leaves it behind the others; it's
        // brought to the front in the next frame, once it's back.
        #[cfg(windows)]
        {
            self.to_front = true;
            ctx.request_repaint();
        }
    }

    /// Windows: shown from the tray (`show_window`), brought to the front
    /// once it's no longer minimized. It came back behind the other windows
    /// otherwise, so it looked as if it hadn't opened.
    #[cfg(windows)]
    fn come_to_front(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        if self.to_front && ctx.input(|i| i.viewport().minimized != Some(true)) {
            self.to_front = false;
            shell::to_front(frame);
        }
    }

    /// Quit for real. The window goes at once: stopping capture and finishing
    /// a recording can take a moment, and a window that doesn't respond
    /// meanwhile looks frozen.
    pub(crate) fn quit(&mut self, ctx: &egui::Context) {
        self.quitting = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// The first close, with a tray: keep running there, or quit?
    fn close_dialog(&mut self, ctx: &egui::Context) {
        if !self.close_dialog {
            return;
        }
        let (mut keep, mut quit, mut cancel) = (false, false, false);
        let modal = ui_kit::dialog(ctx, "close_dialog", 420.0, |ui| {
            let place = if cfg!(target_os = "macos") { "menu bar" } else { "tray" };
            ui_kit::dialog_title(ui, "Keep HasteClip running?");
            ui_kit::dialog_text(ui, &format!("In the {place}, clipping and shortcuts keep working, so you can still save clips. You can change this in Settings."));
            ui_kit::dialog_buttons(ui, |ui| {
                keep = ui.add(ui_kit::button("Keep running", true)).clicked();
                quit = ui.add(ui_kit::button("Quit", false).ghost()).clicked();
                cancel = ui.add(ui_kit::button("Cancel", false).ghost()).clicked();
            });
        });
        if keep || quit {
            self.close_dialog = false;
            self.settings.close_asked = true;
            self.settings.close_to_tray = keep;
            if keep { self.hide_window(ctx) } else { self.quit(ctx) }
        } else if cancel || modal.dismissed() {
            // Nothing decided: the window stays, and it asks again next time.
            self.close_dialog = false;
        }
    }
}

// --- Capture status beside the recorder ---
impl App {
    /// How the capture kept up in its last whole second, for the status by
    /// the recorder: looked up a few times a second while capturing.
    fn track_health(&mut self) {
        if self.rec_state.idle() {
            (self.health, self.health_at) = (None, None);
            return;
        }
        if self.health_at.is_some_and(|t| t.elapsed() < Duration::from_millis(200)) {
            return;
        }
        self.health = capture::health();
        self.health_at = Some(Instant::now());
    }

    /// Shown while frames are being skipped, as a frame rate counter would
    /// say it: how many of the last whole second's ("Skipping 30 of 60
    /// fps"), what that does to the clip, and what the settings are; a click
    /// opens Settings. `compact`: a narrow window, just the count. Laid out
    /// right to left, before the recorder.
    pub(crate) fn skipped_warning(&mut self, ui: &mut egui::Ui, compact: bool) {
        let Some(s) = self.health.and_then(|h| h.last_second).filter(|s| s.skipped > 0) else { return };
        let warn = theme::pal(ui).warn;
        let r = &self.settings;
        // Caught up after a stall, more than a second's frames go at once.
        let lost = s.skipped.min(r.fps);
        let size = match r.resolution {
            settings::OutputResolution::Native => "native resolution".to_owned(),
            res => res.label(),
        };
        let blending = r.blend_samples() > 1;
        let text = format!("Skipping {lost} of {} fps", r.fps);
        let frames = if lost == 1 { "frame was" } else { "frames were" };
        let tip = format!(
            "{text}: in the last second, {lost} of its {} {frames} skipped. \
             The video encoder couldn't keep up: where a frame is skipped, the one before stays on screen longer, so the clip stutters there.\n\n\
             Recording at {size}, {} fps{}. A lower frame rate or resolution gives it less to do{}. Click to open Settings.",
            r.fps,
            r.fps,
            if blending { format!(", blended from {} fps", r.fps * r.blend_samples()) } else { String::new() },
            if blending { ", and so does turning off frame blending" } else { "" },
        );
        let shown = if compact { format!("{}  {lost} of {} skipped", egui_phosphor::regular::WARNING, r.fps) } else { format!("{}  {text}", egui_phosphor::regular::WARNING) };
        let label = egui::Label::new(theme::text(shown, 13.0, theme::W::Semibold).color(warn)).sense(egui::Sense::click());
        ui.add_space(6.0);
        let response = ui.add(label).on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand);
        if response.clicked() {
            self.open_settings(settings_ui::Pane::Video);
        }
    }

    /// Frame blending, while the capture blends, as a frame rate counter
    /// would say it: how many looks at the screen the last whole second's
    /// frames blended, every look whether or not the screen had changed
    /// ("Blending 540 fps", or "Blending 495 of 540 fps" when frames were
    /// skipped, their looks with them), and on hover how many new pictures
    /// the screen showed in it. A warning instead while pictures miss their
    /// frame (blending fell behind: those frames reused the picture before),
    /// which a click takes to Settings. `compact`: a narrow window, only the
    /// warning, as a count. Laid out right to left, before the recorder.
    pub(crate) fn blend_status(&mut self, ui: &mut egui::Ui, compact: bool) {
        // A whole second has to go by to say.
        let Some((b, s)) = self.health.and_then(|h| h.blend).and_then(|b| b.last_second.map(|s| (b, s))) else { return };
        let p = theme::pal(ui);
        let fps = b.rate / b.looks.max(1);
        let missed = s.missed.min(b.rate);
        if missed > 0 {
            let text = format!("Blending missed {missed} of {} fps", b.rate);
            let tip = format!(
                "{text}: in the last second, {missed} of the screen's pictures came too late for their frame, or were dropped while the computer was busy. \
                 Those frames reused the picture before: their motion blur is made of fewer pictures.\n\n\
                 Each {fps} fps frame blends {} looks at the screen, {} a second. Fewer looks, or a lower resolution, give it less to do. Click to open Settings.",
                b.looks, b.rate,
            );
            let shown = if compact { format!("{}  {missed} missed", egui_phosphor::regular::WARNING) } else { format!("{}  {text}", egui_phosphor::regular::WARNING) };
            let label = egui::Label::new(theme::text(shown, 13.0, theme::W::Semibold).color(p.warn)).sense(egui::Sense::click());
            ui.add_space(6.0);
            let response = ui.add(label).on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand);
            if response.clicked() {
                self.open_settings(settings_ui::Pane::Video);
            }
            return;
        }
        if compact {
            return;
        }
        let (text, blended) = if s.blended < b.rate {
            (format!("Blending {} of {} fps", s.blended, b.rate), format!("{} of {}, as skipped frames take their looks with them", s.blended, b.rate))
        } else {
            (format!("Blending {} fps", b.rate), format!("all {}", b.rate))
        };
        let pictures = match s.pictures {
            0 => "no new pictures".to_owned(),
            1 => "1 new picture".to_owned(),
            n => format!("{n} new pictures"),
        };
        let each = if s.pictures as f64 >= b.rate as f64 * 0.95 {
            "every look had one of its own.".to_owned()
        } else {
            format!(
                "it only shows one when something changes, at most as often as the game draws or the display refreshes, and each look takes the newest one up.\n\n\
                 Where things move, a game frame limit of {} fps or more gives every look a picture of its own.",
                b.rate
            )
        };
        let tip = format!(
            "Each {fps} fps frame blends {} looks at the screen, whether or not anything on it changed: in the last second, {blended}. The screen showed {pictures} in it: {each}",
            b.looks
        );
        ui.add_space(6.0);
        ui.add(egui::Label::new(theme::text(text, 13.0, theme::W::Medium).color(p.text3)).sense(egui::Sense::hover())).on_hover_text(tip);
    }

    /// Follow the game being clipped, for Discord: the last one in focus, for
    /// as long as it's open.
    fn track_game(&mut self) {
        if self.rec_state.idle() {
            self.clipped_game = None;
            return;
        }
        if let Some((id, game)) = self.games.focused() {
            self.clipped_game = Some((id, game, Instant::now()));
        }
        // Not in focus for a while: is it still open?
        if let Some((id, _, checked)) = &mut self.clipped_game
            && checked.elapsed() > Duration::from_secs(5)
        {
            if capture::list_windowed_apps().iter().any(|a| a.id.eq_ignore_ascii_case(id)) {
                *checked = Instant::now();
            } else {
                self.clipped_game = None;
            }
        }
    }

    /// The microphone "HasteClip that" is heard through: the first
    /// one on the Sources page that's being captured.
    fn voice_mic(&self) -> Option<String> {
        self.settings.audio_sources.iter().find(|s| s.enabled && matches!(s.kind, settings::SourceKind::Microphone { .. })).map(|s| s.id.clone())
    }

    /// The games and apps list, as (id, name).
    fn listed_apps(&self) -> Vec<(String, String)> {
        match &self.settings.capture {
            settings::CaptureTarget::Apps { apps, .. } => apps.iter().map(|a| (a.id.clone(), a.name.clone())).collect(),
            _ => Vec::new(),
        }
    }

    /// Where a clip saved now goes: the folder of the game in focus longest
    /// in it (Desktop without one), or the library itself with folders
    /// turned off.
    fn clip_folder(&self) -> PathBuf {
        games::folder_for(&self.settings.output_dir, self.settings.folder_per_game, &self.settings.game_folders, self.games.clip_game().as_deref())
    }

    /// Where the recording stopped now goes: as a clip, by the game in focus
    /// longest while it ran.
    fn recording_folder(&self) -> PathBuf {
        games::folder_for(&self.settings.output_dir, self.settings.folder_per_game, &self.settings.game_folders, self.games.recording_game().as_deref())
    }

    /// Discord shows the game being clipped while capture runs (when turned
    /// on), and nothing without a game (see `discord.rs`).
    fn update_presence(&mut self) {
        let game = self.clipped_game.as_ref().map(|(_, g, _)| g.clone()).filter(|_| self.settings.discord_presence && !self.rec_state.idle());
        let Some(game) = game else {
            self.presence_since = None;
            self.presence.set(None);
            return;
        };
        // Timed from when this game showed up: another game starts again.
        if self.presence_since.as_ref().is_none_or(|(name, _)| *name != game.name) {
            self.presence_since = Some((game.name.clone(), SystemTime::now()));
        }
        let since = self.presence_since.as_ref().map_or_else(SystemTime::now, |(_, at)| *at);
        self.presence.set(Some(discord::Status { recording: self.rec_state.recording, streaming: self.rec_state.streaming, game, since }));
    }

    /// "Update ready", once a new version is downloaded: restarts into it. It
    /// installs on quit anyway, so this is only for the impatient — and never
    /// cuts a recording short. Laid out right to left, before the recorder.
    pub(crate) fn update_button(&mut self, ui: &mut egui::Ui) {
        let update::Status::Ready(version) = self.updater.status() else {
            return;
        };
        ui.add_space(4.0);
        let recording = self.rec_state.recording || self.rec_state.streaming;
        let tip = if self.rec_state.streaming {
            format!("HasteClip {version} is ready. It installs when you quit, or restart once your stream is over.")
        } else if recording {
            format!("HasteClip {version} is ready. It installs when you quit, or restart once your recording is done.")
        } else if self.rec_state.clipping {
            let after = if self.settings.auto_start_buffer { "starts again, empty" } else { "stops" };
            format!("HasteClip {version} is ready. Click to restart into it now (clipping {after}), or it installs when you quit.")
        } else {
            format!("HasteClip {version} is ready. Click to restart into it now, or it installs when you quit.")
        };
        let b = ui_kit::Btn::ghost("Update ready").with_icon(egui_phosphor::regular::ARROW_CIRCLE_UP).tip(tip.clone()).locked(recording.then_some(tip));
        if b.show(ui).clicked() && self.updater.install_on_exit(true) {
            // Really quit (not to the tray): the update goes in, then it starts again.
            self.quit(ui.ctx());
        }
    }

    fn permission_banner(&mut self, ui: &mut egui::Ui) {
        if let Some(Some(problem)) = self.ffmpeg_problem.get() {
            ui.add_space(8.0);
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.colored_label(ui.visuals().warn_fg_color, problem);
            });
        }
        if self.permission != capture::Permission::Denied {
            return;
        }
        ui.add_space(8.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "HasteClip needs Screen Recording permission before it can capture.",
            );
            ui.horizontal(|ui| {
                if ui.button("Grant permission").clicked() {
                    capture::request_screen_permission();
                    self.permission = capture::screen_permission();
                }
                if ui.button("Open System Settings").clicked() {
                    open_screen_settings();
                }
                ui.weak("Already enabled it? Quit and reopen the app.");
            });
        });
    }
}

// --- Editing: open the editor, render saved edits in the background ---
impl App {
    pub(crate) fn open_editor(&mut self, clip: PathBuf) {
        self.open_editor_at(clip, None, Handoff::default());
    }

    /// Open the editor on `clip`; from the player, `from` where it was, with
    /// what it hands over (refused while recording: then that's dropped).
    pub(crate) fn open_editor_at(&mut self, clip: PathBuf, from: Option<viewer::Resume>, handoff: Handoff) {
        if self.rec_state.recording {
            self.toast_error("Stop recording before editing a clip.");
            return;
        }
        self.listen_as_set();
        self.editor = Some(editor::Editor::open(&self.ctx(), &clip, self.settings.editor_picture_share, self.settings.editor_snap, from, handoff));
        self.page = Page::Edit;
    }

    /// The volume and mute you listen at, as last set (the player and the
    /// editor share them), the first time either opens.
    fn listen_as_set(&self) {
        if viewer::volume(&self.ctx()).is_none() {
            viewer::set_volume(&self.ctx(), (self.settings.player_volume.clamp(0.0, 1.0), self.settings.player_muted));
        }
    }

    /// Play a clip in the app's own viewer.
    pub(crate) fn open_viewer(&mut self, clip: PathBuf) {
        self.open_viewer_at(clip, None, Handoff::default());
    }

    /// Play `clip`: at `resume`, with what the editor hands over.
    pub(crate) fn open_viewer_at(&mut self, clip: PathBuf, resume: Option<viewer::Resume>, handoff: Handoff) {
        self.listen_as_set();
        self.viewer = Some(self.viewer_for(&clip, resume, handoff));
        self.page = Page::View;
    }

    /// A viewer for this clip: its "Saving your edit" screen while an edit
    /// of it is still being written (what was handed over goes: the clip's
    /// file is about to be replaced), the clip otherwise.
    fn viewer_for(&self, clip: &std::path::Path, resume: Option<viewer::Resume>, handoff: Handoff) -> viewer::Viewer {
        if self.renders.iter().any(|j| !j.as_new && j.source == clip) {
            viewer::Viewer::saving(clip, resume)
        } else {
            viewer::Viewer::open(&self.ctx(), clip, resume, handoff)
        }
    }

    fn viewer_page(&mut self, ui: &mut egui::Ui, frame: &eframe::Frame) {
        let delete_label = self.delete_label(None);
        let saving = self.viewer.as_ref().and_then(|v| self.renders.iter().find(|j| !j.as_new && j.source == v.clip())).map(|j| j.progress());
        let link = self.viewer.as_ref().and_then(|v| self.link_state(v.clip())).map(|s| {
            use egui_phosphor::regular as icon;
            let pct = |f: f32| ((f * 100.0) as u32).min(99);
            match s {
                links::LinkState::Uploading(f) => (icon::CLOUD_ARROW_UP, format!("Uploading {}%", pct(f)), "Uploading for a link"),
                links::LinkState::Updating(f) => (icon::CLOUD_ARROW_UP, format!("Updating {}%", pct(f)), "Updating its link to this version"),
                links::LinkState::Behind => (icon::ARROWS_CLOCKWISE, "Shared".to_owned(), "Shared with a link, which still shows an older version of it: it's brought up to date once HasteClip's server can be reached"),
                links::LinkState::Shared => (icon::LINK, "Shared".to_owned(), "Shared with a link: its link, and who's watched it"),
            }
        });
        let Some(v) = &mut self.viewer else {
            self.page = Page::Clips;
            return;
        };
        // Neighbours among the clips shown in the library (a game's, a
        // collection's; clips, screenshots or both), newest first, to step
        // through; all clips when this one isn't among them (taken out of the
        // collection meanwhile).
        let shown: Vec<&clips::Clip> = self.clips.iter().filter(|c| self.library_filter.matches(c, &self.collections, &self.links) && self.library_kind.matches(c)).collect();
        let list: Vec<&clips::Clip> = if shown.iter().any(|c| c.path == v.clip()) { shown } else { self.clips.iter().collect() };
        let at = list.iter().position(|c| c.path == v.clip());
        let mut collections: Vec<(String, String, bool)> =
            self.collections.list().iter().map(|c| (c.id.clone(), c.name.clone(), self.collections.contains(&c.id, v.clip()))).collect();
        collections.sort_by_key(|(_, name, _)| name.to_lowercase());
        let nav = viewer::Nav {
            previous: at.and_then(|i| i.checked_sub(1)).map(|i| list[i].path.clone()),
            next: at.and_then(|i| list.get(i + 1)).map(|c| c.path.clone()),
            position: at.map(|i| (i + 1, list.len())),
            collections,
            delete_label,
            saving,
            linked: self.links.get(v.clip()).is_some(),
            link,
        };
        let from = v.at();
        let outcome = v.ui(ui, &nav);
        if let Some((volume, muted)) = viewer::volume(ui.ctx()) {
            (self.settings.player_volume, self.settings.player_muted) = (volume, muted);
        }
        match outcome {
            viewer::ViewerOutcome::Stay => {}
            viewer::ViewerOutcome::Close => {
                // Back in the library, bring the clip just watched into view.
                self.reveal_clip = Some(v.clip().to_path_buf());
                self.viewer = None; // drops the player: stops audio and decoders
                self.page = Page::Clips;
            }
            viewer::ViewerOutcome::Edit => {
                let clip = v.clip().to_path_buf();
                self.reveal_clip = Some(clip.clone());
                let handoff = self.viewer.take().map(viewer::Viewer::into_handoff).unwrap_or_default();
                self.page = Page::Clips;
                self.open_editor_at(clip.clone(), from, handoff);
                if self.page == Page::Edit {
                    self.edit_return = Some(clip);
                }
            }
            viewer::ViewerOutcome::Open(clip) => self.viewer = Some(self.viewer_for(&clip, None, Handoff::default())),
            viewer::ViewerOutcome::InCollection(id, add) => {
                let clip = v.clip().to_path_buf();
                self.put_in_collection(&[clip], &id, add);
            }
            viewer::ViewerOutcome::NewCollection => {
                let clip = v.clip().to_path_buf();
                self.new_collection_with(vec![clip]);
            }
            viewer::ViewerOutcome::Share(choice) => {
                let clip = v.clip().to_path_buf();
                self.share(frame, clip, choice);
            }
            viewer::ViewerOutcome::Delete => {
                let clip = v.clip().to_path_buf();
                self.delete_clips(&[clip]);
            }
            viewer::ViewerOutcome::FreeUp => {
                let clip = v.clip().to_path_buf();
                self.ask_free_up(&[clip]);
            }
            viewer::ViewerOutcome::Rename => {
                let clip = v.clip().to_path_buf();
                self.rename_clip(clip);
            }
            viewer::ViewerOutcome::OpenExternal => {
                let clip = v.clip().to_path_buf();
                if let Err(e) = clips::open_in_default_app(&clip) {
                    self.toast_error(format!("Couldn't open the clip: {e}"));
                }
            }
            viewer::ViewerOutcome::Keys => self.show_keys = Some(shell::Keys::Player),
            viewer::ViewerOutcome::DragOut => {
                let clip = v.clip().to_path_buf();
                let preview = self.clips.iter().find(|c| c.path == clip).and_then(thumbs::cached_jpeg);
                self.drag_out(ui.ctx(), frame, vec![clip], preview);
            }
        }
    }

    fn ctx(&self) -> egui::Context {
        self.thumbs.ctx()
    }

    fn editor_page(&mut self, ui: &mut egui::Ui) {
        let Some(ed) = &mut self.editor else {
            self.page = Page::Clips;
            return;
        };
        let outcome = ed.ui(ui, self.settings.delete_permanently);
        self.settings.editor_picture_share = ed.preview_share();
        self.settings.editor_snap = ed.snap();
        if let Some((volume, muted)) = viewer::volume(ui.ctx()) {
            (self.settings.player_volume, self.settings.player_muted) = (volume, muted);
        }
        match outcome {
            editor::EditorOutcome::Stay => {}
            editor::EditorOutcome::Keys => self.show_keys = Some(shell::Keys::Editor),
            editor::EditorOutcome::Close => self.close_editor(true),
            editor::EditorOutcome::Reverted(target) => {
                match store::revert(&target) {
                    Ok(()) => self.toast("Reverted to the original clip"),
                    Err(e) => self.toast_error(format!("Couldn't revert: {e}")),
                }
                // The original is the clip again: nothing the editor read of
                // either file is what's there now.
                self.close_editor(false);
            }
            editor::EditorOutcome::Rename => {
                if let Some(ed) = &self.editor {
                    let clip = ed.clip().to_path_buf();
                    self.rename_clip(clip);
                }
            }
            editor::EditorOutcome::Saved { target, info, edit, new_name, then } => {
                // As new: the editor stays open, on this clip, for the next
                // one (cutting several out of one recording).
                let as_new = new_name.as_ref().map(|n| n.trim().to_owned());
                self.start_render(target, info, edit, new_name, then);
                match as_new {
                    Some(name) => self.toast(format!("Saving “{name}” as a new clip")),
                    None => self.close_editor(true),
                }
            }
            editor::EditorOutcome::FreeUp(target, how) => {
                // Closed first: the editor reads the original (about to
                // change, so nothing of it is handed on).
                self.close_editor(false);
                self.free_up_original(target, how);
            }
        }
    }

    /// Close the editor. Opened from the player: back to it, which takes over
    /// what the editor had (`hand_on`; not when the clip's files are about to
    /// change).
    fn close_editor(&mut self, hand_on: bool) {
        let Some(ed) = self.editor.take() else {
            self.page = Page::Clips;
            return;
        };
        let clip = ed.clip().to_path_buf();
        // Where its playhead is, to put the player there too.
        let resume = ed.resume();
        let handoff = if hand_on {
            ed.into_handoff()
        } else {
            drop(ed); // stops its audio and decoders
            Handoff::default()
        };
        self.page = Page::Clips;
        self.refresh_clips();
        // Opened from the player: back to it, on this clip (renamed meanwhile,
        // it's the new name). While its edit saves, the player waits for it.
        if self.edit_return.take().is_some() {
            self.open_viewer_at(clip, resume, handoff);
        }
    }

    /// Render an edit in the background. `new_name`: save as a separate clip with
    /// that name instead of updating this clip's edit. `then`: once it's
    /// saved, free up the space the original takes this way.
    fn start_render(&mut self, target: store::EditTarget, info: media::ClipInfo, edit: media::Edit, new_name: Option<String>, then: Option<store::FreeUp>) {
        let ext = target.clip.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or("mp4".into());
        // As new: straight to the new file, no history, next to the clip in
        // the same game's folder. In place: see `render_in_place`.
        let new_dest = match &new_name {
            None => None,
            Some(name) => match clips::path_for_name(target.clip.parent().unwrap_or(target.library()), name, &ext, None) {
                // Another render that isn't done yet may be making it.
                Ok(p) if self.renders.iter().any(|j| j.dest == p) => {
                    self.toast_error("A clip with that name is already being saved.");
                    return;
                }
                Ok(p) => Some(p),
                Err(e) => {
                    self.toast_error(e);
                    return;
                }
            },
        };
        let id = self.next_render_id;
        self.next_render_id += 1;
        let progress = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        self.renders.push(RenderJob {
            id,
            source: target.clip.clone(),
            dest: new_dest.clone().unwrap_or_else(|| target.clip.clone()),
            as_new: new_dest.is_some(),
            progress: progress.clone(),
        });
        let tx = self.render_tx.clone();
        let ctx = self.ctx();
        let permanently = self.settings.delete_permanently;
        std::thread::spawn(move || {
            let report = |f: f32| {
                progress.store(f.to_bits(), std::sync::atomic::Ordering::Relaxed);
                ctx.request_repaint();
            };
            let result = match &new_dest {
                None => render_in_place(&target, &info, &edit, report),
                Some(dest) => {
                    let made = std::fs::metadata(&target.clip).and_then(|m| m.modified()).ok();
                    media::render_with_progress(&target.source, &info, &edit, dest, None, report).map_err(|e| e.to_string()).map(|()| {
                        // A new clip of part of this one: the moments in that part.
                        moments::rebase(&target.clip, dest, info.duration, &edit.kept());
                        keep_date(dest, made);
                        dest.clone()
                    })
                }
            };
            // Saved: now the original it keeps can be trimmed or deleted.
            let freed = match (&result, then) {
                (Ok(_), Some(how)) => Some((how, store::free_up(&store::EditTarget::of(&target.clip), how, permanently))),
                _ => None,
            };
            let _ = tx.send(RenderDone { id, result, freed });
            ctx.request_repaint();
        });
        self.refresh_clips();
    }

    fn pump_renders(&mut self) {
        while let Ok(RenderDone { id, result, freed }) = self.render_rx.try_recv() {
            let Some(i) = self.renders.iter().position(|j| j.id == id) else { continue };
            // The player showing it (Free up space from its ⋯) reads where
            // it sits in the recording again.
            if freed.is_some()
                && let Some(v) = &mut self.viewer
            {
                v.original_changed(&self.renders[i].source);
            }
            // Freed with others: their batch's toast says how it went, once
            // they're all done. After saving an edit: this one's.
            if let Some((how, freed)) = freed
                && !self.freed_in_batch(id, &freed)
            {
                match freed {
                    Ok(bytes) => self.freed_toast(how, 1, bytes, usize::from(bytes == 0 && how == store::FreeUp::Trim), &[]),
                    Err(e) => self.freed_toast(how, 1, 0, 0, &[e]),
                }
            }
            let job = self.renders.remove(i);
            match result {
                Ok(path) => {
                    proxy::prebuild(&self.ctx(), &path);
                    if !job.as_new && self.viewer.as_ref().is_some_and(|v| v.is_saving() && v.clip() == job.source) {
                        let resume = self.viewer.as_ref().and_then(|v| v.resume());
                        self.open_viewer_at(job.source.clone(), resume, Handoff::default());
                    }
                    // Highlight the card that changed: the new clip, or the edited one.
                    let card = if job.as_new { path } else { job.source };
                    self.last_saved = Some((card, Instant::now()));
                    if job.as_new {
                        self.folded.set(chrono::Local::now().date_naive(), false);
                        self.toast(format!("Saved as new clip “{}”", file_stem(&job.dest)));
                    }
                }
                Err(e) => {
                    self.toast_error(format!("Couldn't save the edit: {e}"));
                    // The player waiting for it shows the clip as it was.
                    if self.viewer.as_ref().is_some_and(|v| v.is_saving() && v.clip() == job.source) {
                        let resume = self.viewer.as_ref().and_then(|v| v.resume());
                        self.open_viewer_at(job.source.clone(), resume, Handoff::default());
                    }
                }
            }
            self.refresh_clips();
        }
    }
}

/// A background render (or freeing up an original) finished: the clip, and
/// how freeing up space after it went.
struct RenderDone {
    id: u64,
    result: Result<PathBuf, String>,
    freed: Option<(store::FreeUp, Result<u64, String>)>,
}

/// A background render, shown in the library with a progress bar.
pub(crate) struct RenderJob {
    id: u64,
    /// The clip being edited, as named in the library.
    pub source: PathBuf,
    /// Where the result goes (the rendered edit, or the new clip).
    pub dest: PathBuf,
    /// Saving as a separate new clip (shows its own placeholder card).
    pub as_new: bool,
    /// 0..=1 as f32 bits, written by the render thread.
    progress: std::sync::Arc<std::sync::atomic::AtomicU32>,
}

impl RenderJob {
    pub fn progress(&self) -> f32 {
        f32::from_bits(self.progress.load(std::sync::atomic::Ordering::Relaxed))
    }
}

/// Render `edit` of `target` over the clip: into its asset folder, then
/// swapped in, the recording kept beside it as its original (see
/// `store::commit_render`); a failed render leaves the clip as it was. The
/// clip keeps its date, so it stays on its day in the library. The clip,
/// once it's done.
pub(crate) fn render_in_place(target: &store::EditTarget, info: &media::ClipInfo, edit: &media::Edit, report: impl Fn(f32)) -> Result<PathBuf, String> {
    let ext = target.clip.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or("mp4".into());
    let clip_id = target.id.clone().unwrap_or_else(store::new_id);
    store::create_assets_dir(target.library(), &clip_id).map_err(|e| e.to_string())?;
    let staged = store::staging_path(target.library(), &clip_id, &ext);
    let made = std::fs::metadata(&target.clip).and_then(|m| m.modified()).ok();
    let rendered = media::render_with_progress(&target.source, info, edit, &staged, Some(&clip_id), report).map_err(|e| e.to_string());
    let committed = rendered.and_then(|()| store::commit_render(target, &staged, &clip_id, edit).map_err(|e| e.to_string()));
    if committed.is_err() {
        store::abandon_render(target.library(), &clip_id, &staged);
    }
    committed?;
    keep_date(&target.clip, made);
    Ok(target.clip.clone())
}

/// Give `path` back the modified time `made` (when the clip was made).
fn keep_date(path: &std::path::Path, made: Option<std::time::SystemTime>) {
    if let Some(made) = made
        && let Err(e) = std::fs::File::options().write(true).open(path).and_then(|f| f.set_modified(made))
    {
        eprintln!("couldn't keep the clip's date on {}: {e}", path.display());
    }
}

/// Names in a list as said: "Twitch", "Twitch and YouTube", "Twitch,
/// YouTube and Kick".
pub(crate) fn words_and(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

fn file_stem(path: &std::path::Path) -> String {
    path.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

// --- Capture actions ---
impl App {
    /// Clipping on or off; whatever else runs goes on.
    fn toggle_buffer(&mut self) {
        if self.rec_state.clipping {
            self.stop_clipping();
        } else {
            self.start_replay_buffer();
        }
    }

    /// Recording on or off; clipping (and a stream) go on beside it.
    fn toggle_record(&mut self) {
        if self.rec_state.recording {
            self.stop_recording();
        } else {
            self.start_recording();
        }
    }

    /// Re-read audio devices right before capture so "Default" follows whatever the
    /// OS default is *now* (e.g. headphones plugged in since launch).
    fn refresh_audio_devices(&mut self) {
        self.audio = capture::audio::list_audio_devices();
    }

    /// A demo step that's more than input (see `demo.rs`).
    fn demo_action(&mut self, action: demo::Action) {
        match action {
            demo::Action::Unsave => {
                demo::take_back_saves();
                self.refresh_clips();
            }
            demo::Action::EmptyCollections => {
                let lib = self.settings.output_dir.clone();
                let all: Vec<(String, Vec<PathBuf>)> = self.collections.list().iter().map(|c| (c.id.clone(), c.clips.iter().map(|k| lib.join(k)).collect())).collect();
                for (id, clips) in all {
                    self.collections.remove(&id, &clips);
                }
            }
            demo::Action::Clipping(on) => {
                if on && !self.rec_state.clipping {
                    self.start_replay_buffer();
                } else if !on && self.rec_state.clipping {
                    self.stop_clipping();
                }
            }
            demo::Action::Shortcut(settings::ShortcutAction::ToggleBuffer) => self.toggle_buffer(),
            demo::Action::Shortcut(settings::ShortcutAction::ToggleRecord) => self.toggle_record(),
            demo::Action::Shortcut(settings::ShortcutAction::SaveClip) => self.save_clip(),
            demo::Action::Shortcut(settings::ShortcutAction::Screenshot) => self.take_screenshot(),
        }
    }

    fn start_replay_buffer(&mut self) {
        self.start_mode(capture::Mode::ReplayBuffer);
    }

    fn start_recording(&mut self) {
        self.start_mode(capture::Mode::Record);
    }

    /// Go live, to every destination set up (once it's been confirmed:
    /// see `go_live_dialog`).
    pub(crate) fn go_live(&mut self) {
        if !capture::STREAMING || self.settings.stream.ready().next().is_none() {
            return;
        }
        self.stream_seen.clear();
        self.start_mode(capture::Mode::Stream);
    }

    /// Start `mode`. With nothing running, the capture starts with it, with
    /// the settings as they are now (they stay while any of it runs).
    fn start_mode(&mut self, mode: capture::Mode) {
        if self.rec_state.has(mode) {
            return;
        }
        if self.rec_state.idle() {
            self.refresh_audio_devices();
            self.capturing_video = Some(self.video_source());
            self.capturing_sources = self.capture_sources();
            self.started_sources = self.capturing_sources.clone();
            self.capturing_blend = Some((self.settings.blend_samples(), self.settings.blend_shape.into()));
            self.frame_size = None;
        }
        if mode == capture::Mode::ReplayBuffer {
            self.capturing_replay = Some(self.settings.replay_seconds);
        }
        self.service.start(mode, self.encode_settings());
        // Optimistic state; a State/Error event confirms or corrects it.
        self.rec_state = self.rec_state.with(mode, true);
        match mode {
            capture::Mode::Record => self.rec_started = Some(Instant::now()),
            capture::Mode::Stream => self.stream_started = Some(Instant::now()),
            capture::Mode::ReplayBuffer => {}
        }
    }

    fn save_clip(&mut self) {
        if !self.rec_state.clipping {
            return;
        }
        // Its moment is taken at once; the Saved event lands it in the library.
        // Another can be saved while it's still being written.
        let dir = self.clip_folder();
        self.service.save_clip(dir, self.games.clip_details());
        sound::play_saved(&self.settings.save_sound);
        self.voice.saved_by_hand();
        self.clip_saving();
    }

    /// How a screenshot is taken now: of what's recorded, into the library,
    /// as Settings says.
    fn shot_armed(&self) -> service::ShotArmed {
        service::ShotArmed {
            video: self.video_source(),
            library: self.settings.output_dir.clone(),
            folder_per_game: self.settings.folder_per_game,
            game_folders: self.settings.game_folders.clone(),
            options: screenshot::Options { format: self.settings.screenshot_format, copy: self.settings.copy_screenshots },
            sound: self.settings.save_sound.clone(),
        }
    }

    /// Take a screenshot (when the shortcut's own event couldn't): into the
    /// folder of the game in focus.
    fn take_screenshot(&mut self) {
        if !capture::SCREENSHOTS {
            return;
        }
        let a = self.shot_armed();
        let game = self.games.focused().map(|(_, g)| g.name);
        let dir = games::folder_for(&a.library, a.folder_per_game, &a.game_folders, game.as_deref());
        self.service.screenshot(dir, a.video, a.options);
        sound::play_saved(&a.sound);
    }

    /// A clip is being saved: a card for it in the library (the page you're
    /// on stays).
    fn clip_saving(&mut self) {
        self.saving += 1;
    }

    pub(crate) fn stop_clipping(&mut self) {
        self.stop_mode(capture::Mode::ReplayBuffer);
    }

    pub(crate) fn stop_recording(&mut self) {
        self.stop_mode(capture::Mode::Record);
    }

    pub(crate) fn end_stream(&mut self) {
        self.stop_mode(capture::Mode::Stream);
    }

    /// Stop `mode`; the rest goes on (the capture stops with the last).
    fn stop_mode(&mut self, mode: capture::Mode) {
        if !self.rec_state.has(mode) {
            return;
        }
        let (mut dir, mut details) = (None, games::Details::default());
        if mode == capture::Mode::Record {
            self.saving += 1; // finishing the file
            dir = Some(self.recording_folder());
            details = self.games.recording_details();
            self.sessions.let_go();
        }
        self.service.stop(mode, dir, details);
        self.rec_state = self.rec_state.with(mode, false);
        match mode {
            capture::Mode::Record => self.rec_started = None,
            capture::Mode::Stream => self.stream_started = None,
            capture::Mode::ReplayBuffer => {}
        }
    }

    /// Record whole sessions: a game on the list opening starts a recording,
    /// the last one closing stops it (see `sessions.rs`). Never in a test
    /// instance, or before capture can work.
    fn follow_sessions(&mut self) {
        let watching = self.settings.record_sessions && self.persist_settings && self.demo.is_none() && !self.setup_open && self.permission != capture::Permission::Denied;
        self.games.watch(watching);
        match self.sessions.step(watching, self.games.listed_open(), self.rec_state.recording, Instant::now()) {
            // Clipping goes on beside it.
            sessions::Do::Record => self.start_recording(),
            sessions::Do::Stop => self.stop_recording(),
            sessions::Do::Nothing => {}
        }
    }

    /// What to record (games and apps, the whole screen, which display)
    /// reaches a running capture right away; it switches without restarting.
    fn sync_capture_video(&mut self) {
        if self.rec_state.idle() {
            self.capturing_video = None;
            return;
        }
        let now = self.video_source();
        if self.capturing_video.as_ref() != Some(&now) {
            self.service.update_video(now.clone());
            self.capturing_video = Some(now);
        }
    }

    /// The audio sources reach the running capture right away. Where it can
    /// (`capture::SOURCES_WHILE_CAPTURING`), one added or turned on goes into
    /// it, one removed or turned off leaves it, and a microphone switched to
    /// another device switches; its tracks stay as they started. Elsewhere
    /// only a microphone's device: its track stays, only where its sound
    /// comes from changes, and anything else waits for the next start.
    fn sync_capture_sources(&mut self) {
        if self.rec_state.idle() {
            return;
        }
        if capture::SOURCES_WHILE_CAPTURING {
            let now = self.live_sources();
            if now != self.capturing_sources {
                self.service.set_sources(now.clone());
                self.capturing_sources = now;
            }
            return;
        }
        use capture::sources::SourceKind::Microphone;
        let now = self.capture_sources();
        if now.len() != self.capturing_sources.len() {
            return;
        }
        let mut switched = Vec::new();
        for (new, old) in now.iter().zip(&self.capturing_sources) {
            if new.id != old.id || new.in_mix != old.in_mix || new.own_track != old.own_track {
                return;
            }
            match (&new.kind, &old.kind) {
                (Microphone { device: a }, Microphone { device: b }) if a != b => switched.push((new.id.clone(), a.clone())),
                (a, b) if a != b => return,
                _ => {}
            }
        }
        for (id, device) in switched {
            self.service.set_mic(id, device);
        }
        for (old, new) in self.capturing_sources.iter_mut().zip(now) {
            old.kind = new.kind;
        }
    }

    /// The audio sources a running capture is to have now: those on,
    /// resolved as at its start (`capture_sources`). One whose microphone
    /// can't be resolved now (it's on the default, and there's none) stays
    /// as the capture has it: it isn't taken out (it's opened again when
    /// it's back), nor added until it can be.
    fn live_sources(&self) -> Vec<capture::sources::AudioSource> {
        let resolved = self.capture_sources();
        self.settings
            .audio_sources
            .iter()
            .filter(|s| s.enabled && (s.in_mix || s.own_track))
            .filter_map(|s| resolved.iter().chain(&self.capturing_sources).find(|r| r.id == s.id).cloned())
            .collect()
    }

    /// The replay length reaches a running replay buffer right away. Not
    /// cleared while idle: the next start sends its own.
    fn sync_capture_settings(&mut self) {
        if self.rec_state.clipping && self.capturing_replay != Some(self.settings.replay_seconds) {
            self.service.set_replay_seconds(self.settings.replay_seconds);
            self.capturing_replay = Some(self.settings.replay_seconds);
        }
    }

    /// Frame blending reaches a running capture right away, where it can
    /// change while capturing (`capture::BLEND_WHILE_CAPTURING`).
    fn sync_capture_blend(&mut self) {
        if self.rec_state.idle() {
            self.capturing_blend = None;
            return;
        }
        let now = (self.settings.blend_samples(), self.settings.blend_shape.into());
        if capture::BLEND_WHILE_CAPTURING && self.capturing_blend != Some(now) {
            self.service.set_blend(now.0, now.1);
            self.capturing_blend = Some(now);
        }
    }

    /// Say when a place streamed to goes live, drops, or can't be reached.
    fn watch_stream(&mut self) {
        use capture::stream::Link;
        if !self.rec_state.streaming {
            self.stream_seen.clear();
            return;
        }
        let now = self.stream_status.get();
        let was = |i: usize| self.stream_seen.get(i).map(|d| d.link);
        let mut live = Vec::new();
        let mut lost = None;
        for (i, d) in now.iter().enumerate() {
            match (was(i), d.link) {
                (Some(Link::Live { .. }), Link::Live { .. }) => {}
                (_, Link::Live { .. }) => live.push(d.name.clone()),
                (Some(Link::Live { .. }), Link::Waiting { .. }) => lost = Some(format!("Lost the stream to {}. Trying again…", d.name)),
                // Couldn't connect, the first time it tried.
                (Some(Link::Connecting { attempt: 0 }) | None, Link::Waiting { .. }) => {
                    let why = d.problem.as_deref().map(|p| format!(": {p}")).unwrap_or_default();
                    lost = Some(format!("Couldn't reach {}{why}. Trying again…", d.name));
                }
                _ => {}
            }
        }
        if let Some(text) = lost {
            self.toast_error(text);
        } else if !live.is_empty() {
            self.toast_icon(egui_phosphor::regular::BROADCAST, format!("Live on {}", words_and(&live)));
        }
        self.stream_seen = now;
    }

    /// Drain capture-thread events each frame.
    fn pump_capture_events(&mut self) {
        while let Some(evt) = self.service.poll() {
            match evt {
                // Authoritative state from the capture thread — keeps the UI honest.
                Evt::State(running) => {
                    let new = RecState::of(running);
                    // Timed from when each started (as it was asked, if it was).
                    if !new.recording {
                        self.rec_started = None;
                    } else if self.rec_started.is_none() {
                        self.rec_started = Some(Instant::now());
                    }
                    if !new.streaming {
                        self.stream_started = None;
                    } else if self.stream_started.is_none() {
                        self.stream_started = Some(Instant::now());
                    }
                    if new.idle() != self.rec_state.idle() {
                        self.frame_size = None;
                    }
                    let stopped_recording = self.rec_state.recording && !new.recording;
                    self.rec_state = new;
                    if new.idle() || stopped_recording {
                        self.refresh_clips();
                    }
                }
                Evt::Saved(path) => {
                    self.saving = self.saving.saturating_sub(1);
                    // A new clip is never hidden in a folded Today.
                    self.folded.set(chrono::Local::now().date_naive(), false);
                    // Scrub frames ready before it's opened.
                    proxy::prebuild(&self.ctx(), &path);
                    self.last_saved = Some((path.clone(), Instant::now()));
                    self.refresh_clips();
                    let title = self.clips.iter().find(|c| c.path == path).map(|c| c.title()).unwrap_or_else(|| file_stem(&path));
                    self.toast_action(egui_phosphor::regular::CHECK_CIRCLE, format!("Saved {title}"), "Play", shell::ToastAction::Show(path.clone()));
                    // Showing another game's clips: show this one too.
                    self.library_filter.reveal(&self.clips, &self.collections, &self.links, &path);
                }
                Evt::Shot(path, copied) => {
                    self.folded.set(chrono::Local::now().date_naive(), false);
                    self.last_saved = Some((path.clone(), Instant::now()));
                    self.refresh_clips();
                    let copied = if copied { " and copied" } else { "" };
                    self.toast_action(egui_phosphor::regular::CAMERA, format!("Screenshot saved{copied}"), "View", shell::ToastAction::Show(path.clone()));
                    self.library_filter.reveal(&self.clips, &self.collections, &self.links, &path);
                    if self.library_kind == library::ShowKind::Clips {
                        self.library_kind = library::ShowKind::All;
                    }
                }
                Evt::Error(e) => {
                    // A start failure is followed by Evt::State(None); a save failure
                    // leaves us still buffering. So just surface the message.
                    self.saving = self.saving.saturating_sub(1);
                    self.toast_error(e);
                }
                Evt::Notice(e) => self.toast_error(e),
            }
        }
    }
}

/// Open the macOS Screen Recording privacy pane directly.
pub(crate) fn open_screen_settings() {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
            .spawn();
    }
}

/// Times the parts of one UI update, and logs them when it was slow: the
/// window shows nothing new (a player's picture stays black) until it ends.
struct Laps {
    start: Instant,
    last: Instant,
    laps: Vec<(&'static str, Duration)>,
}

impl Laps {
    /// A UI update this slow is logged (a guess: well past a dropped frame or
    /// two, short of anything you'd notice).
    const SLOW: Duration = Duration::from_millis(250);

    fn start() -> Self {
        let now = Instant::now();
        Self { start: now, last: now, laps: Vec::new() }
    }

    fn lap(&mut self, what: &'static str) {
        let now = Instant::now();
        self.laps.push((what, now - self.last));
        self.last = now;
    }

    fn report(self) {
        let total = self.start.elapsed();
        // `HASTECLIP_DEV_LAPS=<ms>`: log every update at least that slow.
        let slow = std::env::var("HASTECLIP_DEV_LAPS").ok().and_then(|s| s.parse::<u64>().ok()).map_or(Self::SLOW, Duration::from_millis);
        if total < slow {
            return;
        }
        let parts: Vec<String> = self.laps.iter().filter(|(_, d)| *d >= Duration::from_millis(5)).map(|(w, d)| format!("{w} {} ms", d.as_millis())).collect();
        eprintln!("{} slow UI update: {} ms ({})", chrono::Local::now().format("%H:%M:%S"), total.as_millis(), parts.join(", "));
    }
}

/// Whether the app may record the screen, as last checked. Asking macOS
/// takes about 15 ms, so it's asked on a thread of its own, once a second,
/// not in the middle of drawing a frame.
fn permission_now() -> capture::Permission {
    static NOW: std::sync::Mutex<Option<capture::Permission>> = std::sync::Mutex::new(None);
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        std::thread::Builder::new()
            .name("screen permission".into())
            .spawn(|| loop {
                let p = capture::screen_permission();
                *NOW.lock().unwrap() = Some(p);
                std::thread::sleep(Duration::from_secs(1));
            })
            .expect("spawn permission thread");
    });
    NOW.lock().unwrap().unwrap_or_else(capture::screen_permission)
}

#[cfg(test)]
mod state_tests {
    use super::*;

    /// What runs, said as a lock says it, and the state as the capture
    /// reports it.
    #[test]
    fn what_runs_in_words() {
        let s = |clipping, recording, streaming| RecState { clipping, recording, streaming };
        assert!(s(false, false, false).idle());
        assert_eq!(s(true, false, false).words(), "clipping");
        assert_eq!(s(true, false, true).words(), "clipping and streaming");
        assert_eq!(s(true, true, true).words(), "clipping, recording and streaming");
        let r = RecState::of(capture::Running { replay: true, record: false, stream: true });
        assert!(r.has(capture::Mode::Stream) && !r.has(capture::Mode::Record));
        assert_eq!(r.with(capture::Mode::Stream, false), s(true, false, false));
        assert_eq!(words_and(&["Twitch".into(), "YouTube".into(), "Kick".into()]), "Twitch, YouTube and Kick");
    }
}
