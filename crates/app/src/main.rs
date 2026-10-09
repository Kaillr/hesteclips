//! HesteClips — a fast, no-nonsense clip recorder.
//!
//! The app opens on your clips. A slim capture bar on top always shows what's
//! happening (buffer armed, recording, idle) and holds the one or two buttons that
//! make sense right now; the global hotkeys do the same from inside a game.
//! Settings is a separate page you rarely visit, reached from the gear.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // no console window on release Windows

mod autostart;
mod away;
mod clips;
mod collections;
mod webcam_ui;
mod cloud;
mod cloud_ui;
mod discord;
mod editor;
mod export_ui;
mod filmstrip;
mod logfile;
mod game_events;
mod games;
mod header;
mod osu_plays;
mod library;
#[cfg(target_os = "linux")]
mod linux_desktop;
mod meter;
mod nav;
mod player;
#[cfg(target_os = "linux")]
mod portal_shortcuts;
mod proxy;
mod service;
mod settings;
mod settings_ui;
mod share;
mod shortcuts;
mod sound;
mod sources_ui;
mod store;
mod thumbs;
mod tray;
mod ui_kit;
mod update;
#[cfg(target_os = "linux")]
mod update_linux;
#[cfg(hw_decode)]
mod gpu_frames;
#[cfg(hw_decode)]
mod video;
mod voice;
mod web_images;
mod viewer;
mod waveform;
mod wheel;

use crate::ui_kit::Dismissed as _;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use egui::{Color32, RichText};
use library::{ACCENT, REC_RED};
use service::{CaptureService, Evt};
use settings::{Encoder, RecordSettings, SourceKind};

/// `HESTECLIPS_DEBUG_GPU=1`: print the renderer's (wgpu's) warnings and
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
    if std::env::var_os("HESTECLIPS_DEBUG_GPU").is_some() {
        static LOGGER: GpuLog = GpuLog;
        let _ = log::set_logger(&LOGGER).map(|()| log::set_max_level(log::LevelFilter::Warn));
    }
    // An installed app has no console: what it prints goes to its log file
    // (`HESTECLIPS_LOG=1` does the same in a development build, to check it).
    #[cfg(any(windows, target_os = "linux"))]
    if cfg!(not(debug_assertions)) || std::env::var_os("HESTECLIPS_LOG").is_some() {
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
    // One HesteClips at a time: another launch shows the running one and
    // exits. (Not for development test instances, which run beside it.)
    let test_instance = std::env::vars_os().any(|(k, _)| {
        let k = k.to_string_lossy();
        ["HESTECLIPS_OPEN_", "HESTECLIPS_DEMO_", "HESTECLIPS_LIBRARY", "HESTECLIPS_SETTINGS"].iter().any(|p| k.starts_with(p))
    });
    let instance = if test_instance {
        None
    } else {
        match tray::claim() {
            Some(listener) => listener,
            None => return Ok(()),
        }
    };
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
    let _ = ctrlc::set_handler(|| {
        service::stop_all_and_wait(std::time::Duration::from_secs(10));
        std::process::exit(0);
    });

    // Dev aid: `HESTECLIPS_WINDOW=WxH` opens at that size, to check layouts.
    let size = std::env::var("HESTECLIPS_WINDOW")
        .ok()
        .and_then(|s| s.split_once('x').and_then(|(w, h)| Some([w.parse().ok()?, h.parse().ok()?])))
        .unwrap_or([1040.0, 700.0]);
    let viewport = egui::ViewportBuilder::default()
        .with_visible(!background)
        .with_inner_size(size)
        .with_min_inner_size([560.0, 420.0])
        .with_title("HesteClips")
        .with_icon(app_icon());
    // Wayland names the window's app by this (its icon and name come from
    // the `.desktop` file of the same name).
    #[cfg(target_os = "linux")]
    let viewport = viewport.with_app_id(portal_shortcuts::APP_ID);
    let options = eframe::NativeOptions {
        viewport,
        wgpu_options: wgpu_options(),
        ..Default::default()
    };

    let result = eframe::run_native(
        "HesteClips",
        options,
        Box::new(|cc| {
            #[cfg(hw_decode)]
            gpu_frames::init(cc.wgpu_render_state.as_ref(), &cc.egui_ctx);
            ui_kit::apply(&cc.egui_ctx);
            Ok(Box::new(App::new(cc.egui_ctx.clone(), instance)))
        }),
    );
    // The app is gone by now (capture stopped, files finished): an update
    // can go in.
    update::apply_queued();
    result
}

/// The renderer's setup. On Windows, D3D12, so decoded video frames can be
/// shared with it straight from the GPU (`gpu_frames.rs`); wgpu's default
/// pick could be Vulkan, which can't open them as simply. Elsewhere wgpu's
/// pick. `HESTECLIPS_WGPU_BACKEND` (e.g. "vulkan", "gl") overrides it, to
/// compare (or on Linux, where a GPU without a Vulkan driver gets Mesa's
/// software one, "gl" draws on the GPU instead).
fn wgpu_options() -> eframe::egui_wgpu::WgpuConfiguration {
    let mut options = eframe::egui_wgpu::WgpuConfiguration::default();
    // A hidden window (covered, minimized, or not shown yet) gets no frame to
    // draw into, and egui skips the frame and tries again at once: a page that
    // repaints continuously (Sources, with its meters and preview) spun at
    // ~7000 tries a second, and right after launch macOS never got to show the
    // window at all, so it stayed blank. Wait a display refresh first.
    let default = options.on_surface_status.clone();
    options.on_surface_status = std::sync::Arc::new(move |status| {
        if matches!(status, eframe::wgpu::CurrentSurfaceTexture::Occluded) {
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
        default(status)
    });
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(new) = &mut options.wgpu_setup {
        match std::env::var("HESTECLIPS_WGPU_BACKEND") {
            Ok(b) => new.instance_descriptor.backends = eframe::wgpu::Backends::from_comma_list(&b),
            Err(_) if cfg!(windows) => new.instance_descriptor.backends = eframe::wgpu::Backends::DX12,
            Err(_) => {}
        }
    }
    options
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Page {
    Clips,
    Sources,
    Settings,
    Edit,
    View,
}

/// What capture is currently doing. Replay buffer and manual recording are mutually
/// exclusive in this simple model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecState {
    Idle,
    Buffering,
    Recording,
}

/// The app icon (`assets/icon.svg` rendered to PNG), shown in the Dock and taskbar
/// while running. Release bundles use `assets/hesteclips.icns` instead. On
/// Windows, where icons are small, the glyph without the macOS plate
/// (`assets/icon-windows.svg`).
pub(crate) fn app_icon() -> egui::IconData {
    #[cfg(windows)]
    let png = include_bytes!("../assets/icon-windows-256.png");
    #[cfg(not(windows))]
    let png = include_bytes!("../assets/icon-1024.png");
    let img = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .expect("bundled icon is a valid PNG")
        .to_rgba8();
    egui::IconData { width: img.width(), height: img.height(), rgba: img.into_raw() }
}

impl App {
    /// The shortcut for `action` as text ("Option + F10"), or "no shortcut".
    pub(crate) fn shortcut_label(&self, action: settings::ShortcutAction) -> String {
        shortcuts::keys(self.settings.shortcuts.get(action)).map_or_else(|| "no shortcut".into(), |k| k.join(" + "))
    }

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

/// A short-lived message shown at the bottom of the window.
struct Toast {
    text: String,
    error: bool,
    at: Instant,
}

const TOAST_FOR: Duration = Duration::from_secs(4);

struct App {
    pub(crate) page: Page,
    rec_state: RecState,
    /// When the current recording/buffer started (for the live timer).
    rec_started: Option<Instant>,
    /// A replay clip is being written; shows a placeholder card until it lands.
    /// Clips being written: replay clips still saving, and a stopped recording
    /// being finished. Each shows a placeholder card until it lands.
    saving: usize,
    settings: RecordSettings,
    /// Last-saved settings JSON, to persist only when something changed.
    saved_settings: String,
    /// False when launched through a dev hook (`HESTECLIPS_OPEN_*`, `_DEMO_*`,
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
    /// Scroll the library to this clip next time it's shown.
    pub(crate) reveal_clip: Option<PathBuf>,
    /// Days folded away in the library.
    pub(crate) folded: library::FoldedDays,
    /// Adding a custom clip-saved sound failed: why.
    pub(crate) sound_error: Option<String>,
    /// Screen-recording permission, re-checked each poll so the banner clears the
    /// moment the user grants it.
    permission: capture::Permission,
    /// What's wrong with ffmpeg, once checked (in the background, at launch).
    ffmpeg_problem: std::sync::Arc<std::sync::OnceLock<Option<String>>>,
    /// Screens detected by the capture backend.
    screens: Vec<capture::Device>,
    /// The recording's frame size for (what's recorded, resolution), cached:
    /// for keeping the webcam's box its shape.
    frame_size: Option<((capture::VideoSource, Option<u32>), Option<(u32, u32)>, Instant)>,
    /// Apps with a window, for the games-and-apps list: to add, and to show
    /// which are open. Refreshed while the Sources page shows.
    pub(crate) windowed_apps: Vec<capture::Device>,
    /// Running games with anti-cheat, for adding to game capture's allowed
    /// list (looked up when its menu opens).
    pub(crate) anticheat_apps: Vec<(capture::Device, String)>,
    /// What the running capture records, to send it list changes live.
    capturing_video: Option<capture::VideoSource>,
    /// The audio sources the running capture started with (mics follow
    /// device changes).
    capturing_sources: Vec<capture::sources::AudioSource>,
    /// The replay length the running capture keeps, to send it changes live.
    capturing_replay: Option<u32>,
    /// Shown instead of a game or app while you're tabbed out.
    pub(crate) away_screen: std::sync::Arc<capture::StillImage>,
    /// Where the webcam sits, shared with a running capture so dragging it
    /// takes effect at once. Mirrors `settings.webcam` every frame.
    webcam_placement: capture::webcam::SharedPlacement,
    pub(crate) webcam_view: webcam_ui::WebcamView,
    /// The camera last asked to stay open (it stays open while a webcam is set
    /// up, so its own settings don't reset).
    kept_camera: Option<(String, Option<capture::webcam::Format>)>,
    /// System audio inputs/outputs (+ current OS defaults).
    audio: capture::audio::AudioDevices,
    /// Live per-source volume and meters, shared with the capture thread.
    live_audio: std::sync::Arc<capture::mixer::LiveAudio>,
    /// Meters-only capture while the Sources page is open and nothing records,
    /// with the sources it was started for.
    level_monitor: Option<(Vec<capture::sources::AudioSource>, capture::sources::LevelMonitor)>,
    /// Preview-only capture while the Sources page is open and nothing records,
    /// with the source, height and frame rate it was started for.
    pub(crate) video_preview: Option<((capture::VideoSource, Option<u32>, u32, Option<capture::webcam::Webcam>), capture::preview::VideoPreview)>,
    sources_view: sources_ui::SourcesView,
    /// A microphone heard live ("Listen" on the Sources page).
    pub(crate) listening: Option<sources_ui::Listening>,
    clips: Vec<clips::Clip>,
    thumbs: thumbs::Thumbs,
    /// Most recently saved clip, highlighted in the library for a few seconds.
    last_saved: Option<(PathBuf, Instant)>,
    cloud: cloud::Cloud,
    /// Open modal, if any.
    dialog: Option<cloud_ui::Dialog>,
    /// Public links of finished uploads, each shown in its own dialog once
    /// nothing else is open: (clip, link).
    public_links: std::collections::VecDeque<(PathBuf, String)>,
    toast: Option<Toast>,
    editor: Option<editor::Editor>,
    viewer: Option<viewer::Viewer>,
    /// Edits being rendered in the background, with live progress.
    pub(crate) renders: Vec<RenderJob>,
    render_tx: std::sync::mpsc::Sender<RenderDone>,
    render_rx: std::sync::mpsc::Receiver<RenderDone>,
    /// Clips' sound being saved as MP3s, when done.
    mp3_tx: std::sync::mpsc::Sender<Result<PathBuf, String>>,
    mp3_rx: std::sync::mpsc::Receiver<Result<PathBuf, String>>,
    next_render_id: u64,
    /// Open "Rename clip" dialog.
    pub(crate) rename: Option<library::Rename>,
    /// Clips selected in the library for a bulk action.
    pub(crate) selection: library::Selection,
    /// Clips being dragged inside the window, to a game or collection.
    pub(crate) card_drag: Option<library::CardDrag>,
    /// Clips waiting for "Delete permanently?" to be answered.
    pub(crate) confirm_delete: Option<Vec<PathBuf>>,
    /// The editor was opened from the player on this clip: closing it goes
    /// back there, not to the library (so cleaning up clips flows on).
    edit_return: Option<PathBuf>,
    /// The tray icon, where there is one: closing the window hides it there.
    tray: Option<tray::Tray>,
    /// Quitting for real (closing mustn't just hide the window then).
    quitting: bool,
    /// The first close: ask whether to keep running in the tray.
    close_dialog: bool,
    /// What was shown last frame, to scroll a new view back to the top.
    last_view: Option<(Page, library::Filter)>,
    /// Library auto-refresh: last folder poll + when the library's folders
    /// last changed, as last seen.
    last_poll: Option<Instant>,
    library_stamp: Vec<Option<SystemTime>>,
    /// Which of the library's folders (games) the library shows.
    pub(crate) library_filter: library::Filter,
    /// Your own groups of clips, in the library's `.hesteclips`.
    pub(crate) collections: collections::Collections,
    pub(crate) collection_dialog: Option<library::CollectionDialog>,
    /// Where the mouse's back/forward buttons go.
    pub(crate) nav: nav::History,
    /// Games' icons and the HesteFiles profile picture.
    pub(crate) web_images: web_images::WebImages,
    pub(crate) updater: update::Updater,
    pub(crate) presence: discord::Presence,
    /// Saves a clip as Save clip is pressed, frames or not.
    quick_save: service::QuickSave,
    /// Listens for "hashtag HesteClip that".
    pub(crate) voice: voice::Voice,
    /// The game being clipped, for Discord: its app's id, the game, and when
    /// we last checked it's still open.
    clipped_game: Option<(String, discord::Game, Instant)>,
    /// What's in focus while capturing, for the folder clips go in.
    games: games::Tracker,
    /// When the current capture started, as Discord's "elapsed" timer counts.
    presence_since: Option<SystemTime>,
}

impl App {
    fn new(ctx: egui::Context, instance: Option<std::net::TcpListener>) -> Self {
        add_symbol_font(&ctx);
        let settings = RecordSettings::load();
        // egui keeps the window's frame in step (on Wayland, drawn by winit).
        ctx.set_theme(settings.theme.preference());
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
        live_audio.set_limiter(settings.limiter);
        for s in &settings.audio_sources {
            let channel = live_audio.channel(&s.id);
            channel.set_volume(sources_ui::from_db(s.volume_db), s.muted);
            channel.set_denoise(s.noise_removal);
        }
        let updater = update::Updater::new(ctx.clone(), settings.auto_update);
        let (tray_tx, tray_rx) = std::sync::mpsc::channel();
        if let Some(listener) = instance {
            tray::serve(listener, &ctx, tray_tx.clone());
        }
        let tray = tray::Tray::new(&ctx, &app_icon(), tray_tx, tray_rx);
        let mut app = Self {
            page: Page::Clips,
            rec_state: RecState::Idle,
            rec_started: None,
            saving: 0,
            settings,
            saved_settings,
            persist_settings: !std::env::vars_os().any(|(k, _)| {
                let k = k.to_string_lossy();
                ["HESTECLIPS_OPEN_", "HESTECLIPS_DEMO_", "HESTECLIPS_LIBRARY", "HESTECLIPS_SETTINGS"].iter().any(|p| k.starts_with(p))
            }),
            service,
            voice: voice::Voice::new(quick_save.clone(), live_audio.clone(), ctx.clone()),
            quick_save,
            hotkeys,
            recording_shortcut: None,
            confirm_reset: false,
            sound_error: None,
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
            capturing_replay: None,
            away_screen: away::screen(),
            webcam_placement: std::sync::Arc::new(std::sync::Mutex::new(capture::webcam::Placement::default_for(16.0 / 9.0, 16.0 / 9.0))),
            webcam_view: Default::default(),
            kept_camera: None,
            audio: capture::audio::list_audio_devices(),
            live_audio,
            level_monitor: None,
            video_preview: None,
            sources_view: Default::default(),
            listening: None,
            clips,
            thumbs: thumbs::Thumbs::new(ctx.clone()),
            last_saved: None,
            cloud: cloud::Cloud::new(ctx),
            dialog: None,
            public_links: Default::default(),
            toast: None,
            editor: None,
            viewer: None,
            renders: Vec::new(),
            render_tx,
            render_rx,
            mp3_tx,
            mp3_rx,
            next_render_id: 0,
            rename: None,
            selection: library::Selection::default(),
            last_poll: None,
            library_stamp: Vec::new(),
            library_filter: library::Filter::All,
            collections,
            collection_dialog: None,
            card_drag: None,
            confirm_delete: None,
            last_view: None,
            tray,
            quitting: false,
            close_dialog: false,
            edit_return: None,
            nav: nav::History::default(),
            web_images: Default::default(),
            updater,
            presence: discord::Presence::new(),
            clipped_game: None,
            games,
            presence_since: None,
        };
        // Dev aid: `HESTECLIPS_OPEN_EDITOR=<clip>` opens the editor at launch, so the
        // editor can be checked without clicking through the library.
        if let Some(clip) = std::env::var_os("HESTECLIPS_OPEN_EDITOR") {
            app.settings.auto_start_buffer = false;
            app.open_editor(PathBuf::from(clip));
        }
        if let Some(clip) = std::env::var_os("HESTECLIPS_OPEN_VIEWER") {
            app.settings.auto_start_buffer = false;
            app.open_viewer(PathBuf::from(clip));
        }
        // Dev aid: `HESTECLIPS_DEMO_RENDER=<clip>` shows the library with a save
        // in progress (that clip's edit + a "save as new"), to check the progress UI.
        // Point HESTECLIPS_LIBRARY at a scratch folder holding a copy of the clip.
        if let Some(dir) = std::env::var_os("HESTECLIPS_LIBRARY") {
            app.settings.auto_start_buffer = false;
            app.settings.output_dir = PathBuf::from(dir);
            app.refresh_clips();
        }
        if let Some(clip) = std::env::var_os("HESTECLIPS_DEMO_RENDER").map(PathBuf::from) {
            let target = store::EditTarget::of(&clip);
            if let Ok(info) = media::probe(&target.source) {
                let mut edit = media::Edit::new(&info);
                edit.start = info.snap(info.duration * 0.1);
                edit.end = info.snap(info.duration * 0.9);
                app.start_render(target.clone(), info.clone(), edit.clone(), None, None);
                app.start_render(target, info, edit, Some("Demo highlight".into()), None);
            }
        }
        // `HESTECLIPS_DEMO_COLLECTION=<name>` shows a collection; `…_NEW` adds
        // the "New collection" dialog.
        if let Some(name) = std::env::var("HESTECLIPS_DEMO_COLLECTION").ok() {
            if let Some(c) = app.collections.list().iter().find(|c| c.name == name) {
                app.library_filter = library::Filter::Collection(c.id.clone());
            }
            if std::env::var_os("HESTECLIPS_DEMO_COLLECTION_NEW").is_some() {
                let first = app.clips.first().map(|c| c.path.clone()).into_iter().collect();
                app.new_collection_with(first);
            }
        }
        // `HESTECLIPS_DEV_BUFFER_QUIT=<secs>`: start the replay buffer, quit
        // after that long (to check quitting while capturing).
        if std::env::var_os("HESTECLIPS_DEV_BUFFER_QUIT").is_some() {
            app.start_replay_buffer();
        }
        // `HESTECLIPS_DEMO_SAVING=<clip>` shows the player waiting for its edit.
        if let Some(clip) = std::env::var_os("HESTECLIPS_DEMO_SAVING").map(PathBuf::from) {
            app.viewer = Some(viewer::Viewer::saving(&clip));
            app.page = Page::View;
        }
        // `HESTECLIPS_DEMO_DELETE=<clip>` asks to delete it (nothing is deleted
        // unless the dialog is answered).
        if let Some(clip) = std::env::var_os("HESTECLIPS_DEMO_DELETE").map(PathBuf::from) {
            app.confirm_delete = Some(vec![clip]);
        }
        // `HESTECLIPS_DEMO_SHARE=<clip>` opens the HesteFiles upload dialog for a clip.
        if let Some(clip) = std::env::var_os("HESTECLIPS_DEMO_SHARE").map(PathBuf::from) {
            app.open_share_dialog(clip);
        }
        if let Some(clip) = std::env::var_os("HESTECLIPS_DEMO_RENAME").map(PathBuf::from) {
            app.rename_clip(clip);
        }
        // `HESTECLIPS_DEMO_PREBUILD=<clip>` builds its scrub frames in the
        // background at launch, as after saving it.
        if let Some(clip) = std::env::var_os("HESTECLIPS_DEMO_PREBUILD").map(PathBuf::from) {
            proxy::prebuild(&app.ctx(), &clip);
        }
        // `HESTECLIPS_OPEN_SOURCES=1` opens on the Sources page.
        let open_sources = std::env::var_os("HESTECLIPS_OPEN_SOURCES").is_some();
        if open_sources {
            app.page = Page::Sources;
        }
        // `HESTECLIPS_OPEN_SETTINGS=1` opens on Settings.
        if std::env::var_os("HESTECLIPS_OPEN_SETTINGS").is_some() {
            app.page = Page::Settings;
        }
        if !recovered.is_empty() {
            app.toast("Recovered a recording that was cut off last time");
        }
        // Never miss a moment: arm the buffer on launch (unless it can't work yet).
        if app.settings.auto_start_buffer && !open_sources && app.permission != capture::Permission::Denied {
            app.start_replay_buffer();
        }
        app
    }

    fn refresh_clips(&mut self) {
        store::set_library(&self.settings.output_dir);
        self.clips = clips::scan(&self.settings.output_dir);
        if self.collections.library() != self.settings.output_dir {
            self.collections = collections::Collections::load(&self.settings.output_dir);
        }
        if self.folded.library() != self.settings.output_dir {
            self.folded = library::FoldedDays::load(&self.settings.output_dir);
        }
        self.collections.reconnect(&self.clips);
        self.selection.retain(&self.clips);
        self.library_filter.retain(&self.clips, &self.collections);
    }

    fn toast(&mut self, text: impl Into<String>) {
        self.toast = Some(Toast { text: text.into(), error: false, at: Instant::now() });
    }

    fn toast_error(&mut self, text: impl Into<String>) {
        self.toast = Some(Toast { text: text.into(), error: true, at: Instant::now() });
    }

    /// Translate the UI's `RecordSettings` into the backend's `EncodeSettings`.
    fn encode_settings(&self) -> capture::EncodeSettings {
        capture::EncodeSettings {
            output_dir: self.settings.output_dir.clone(),
            container_ext: self.settings.container.ext().to_owned(),
            fps: self.settings.fps,
            video_bitrate_kbps: self.settings.video_bitrate_mbps * 1000,
            target_height: self.settings.resolution.height(),
            keyframe_interval_secs: self.settings.keyframe_interval_secs,
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
            sources: self.capture_sources(),
        }
    }

    /// What the video shows, for the capture backend.
    /// The webcam, for the capture backend.
    /// Keep the webcam's box its own shape in the frame being recorded: the
    /// box is fractions of the frame, so switching to a display of another
    /// shape would stretch it into that shape (a strip on an ultrawide).
    fn refit_webcam(&mut self) {
        if self.settings.webcam.is_none() {
            return;
        }
        let key = (self.video_source(), self.settings.resolution.height());
        // Asked again now and then too: a display's mode can change, or it
        // can be plugged in, under the same choice.
        if self.frame_size.as_ref().is_none_or(|(k, _, at)| *k != key || at.elapsed() > Duration::from_secs(2)) {
            let size = capture::frame_size(&key.0, key.1);
            self.frame_size = Some((key, size, Instant::now()));
        }
        let Some((_, Some((fw, fh)), _)) = &self.frame_size else { return };
        let frame_aspect = *fw as f32 / (*fh).max(1) as f32;
        let turned = self.settings.webcam.as_ref().is_some_and(|w| w.placement.turns % 2 == 1);
        let camera_aspect = match capture::webcam::status() {
            capture::webcam::Status::Live { width, height } if turned => Some(height as f32 / width.max(1) as f32),
            capture::webcam::Status::Live { width, height } => Some(width as f32 / height.max(1) as f32),
            _ => None,
        };
        if let Some(w) = self.settings.webcam.as_mut() {
            w.placement.refit(frame_aspect, camera_aspect);
        }
    }

    pub(crate) fn webcam_source(&self) -> Option<capture::webcam::Webcam> {
        let w = self.settings.webcam.as_ref().filter(|_| capture::webcam::AVAILABLE)?;
        Some(capture::webcam::Webcam { device: w.id.clone(), format: w.format.map(Into::into), placement: self.webcam_placement.clone() })
    }

    pub(crate) fn video_source(&self) -> capture::VideoSource {
        match &self.settings.capture {
            settings::CaptureTarget::Apps { apps, away_screen } if capture::APP_CAPTURE => capture::VideoSource::Apps {
                ids: apps.iter().map(|a| a.id.clone()).collect(),
                away_when_unfocused: *away_screen,
                hook: self.game_hook(),
            },
            _ => capture::VideoSource::Screen {
                id: self.screens.get(self.settings.display_index).map(|d| d.id.clone()).unwrap_or_else(|| "0".to_owned()),
                hook: self.game_hook(),
            },
        }
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
        let ctx = ui.ctx().clone();
        let mut laps = Laps::start();

        self.navigate(&ctx);

        // Hearing a microphone is for the Sources page only: never left on.
        if self.page != Page::Sources {
            self.listening = None;
        }

        // Left the viewer some other way (a hotkey that shows the library): stop it.
        if self.page != Page::View && self.viewer.is_some() {
            self.viewer = None;
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        }

        // The editor and viewer get the whole window; capture keeps running
        // underneath and the hotkeys still work.
        if !matches!(self.page, Page::Edit | Page::View) {
            // Flush with the window; a hairline under it, nothing around it.
            let bar = egui::Panel::top("capture_bar")
                .show_separator_line(false)
                .frame(egui::Frame::new().fill(ui.visuals().panel_fill).inner_margin(egui::Margin::symmetric(ui_kit::PAGE_MARGIN, 10)))
                .show(ui, |ui| self.capture_bar(ui));
            let r = bar.response.rect;
            ui.painter().hline(r.x_range(), r.bottom(), egui::Stroke::new(1.0, ui_kit::line(ui.visuals().dark_mode)));
        }

        // A message floats over the bottom of the page for a few seconds,
        // without pushing anything around.
        if let Some(toast) = &self.toast {
            if toast.at.elapsed() < TOAST_FOR {
                let (text, error) = (toast.text.clone(), toast.error);
                egui::Area::new(egui::Id::new("toast"))
                    .order(egui::Order::Foreground)
                    .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -20.0))
                    .interactable(false)
                    .show(&ctx, |ui| {
                        let v = ui.visuals().clone();
                        egui::Frame::new()
                            .fill(v.window_fill)
                            .stroke(egui::Stroke::new(1.0, ui_kit::line(v.dark_mode)))
                            .corner_radius(ui_kit::CARD_RADIUS)
                            .shadow(v.popup_shadow)
                            .inner_margin(egui::Margin::symmetric(16, 10))
                            .show(ui, |ui| {
                                let color = if error { ui_kit::DANGER } else { v.strong_text_color() };
                                ui.add(egui::Label::new(RichText::new(text).color(color)).wrap_mode(egui::TextWrapMode::Extend));
                            });
                    });
            } else {
                self.toast = None;
            }
        }

        // The viewer in fullscreen: the picture from edge to edge.
        let edge_to_edge = self.page == Page::View && ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
        let central = if edge_to_edge {
            egui::Frame::NONE.fill(egui::Color32::BLACK)
        } else {
            // Pages that scroll run to the window's bottom edge; the player
            // and editor keep a margin under their controls.
            let bottom = if matches!(self.page, Page::Edit | Page::View) { 12 } else { 0 };
            egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin { left: ui_kit::PAGE_MARGIN, right: ui_kit::PAGE_MARGIN, top: 0, bottom })
        };
        laps.lap("bars");
        // A dialog or menu open (now, or drawn last frame): it gets the keys.
        let overlay = self.rename.is_some()
            || self.dialog.is_some()
            || self.collection_dialog.is_some()
            || self.confirm_delete.is_some()
            || self.confirm_reset
            || self.close_dialog
            || ctx.memory(|m| m.top_modal_layer().is_some())
            || ctx.any_popup_open();
        ui_kit::set_overlay_open(&ctx, overlay);
        // Another page, game or collection starts at the top. Back from the
        // player or editor, the library stays where it was (and shows the clip).
        let view = (self.page, self.library_filter.clone());
        let reset = self.last_view.as_ref().is_some_and(|(page, filter)| {
            (*page != view.0 && !matches!(page, Page::Edit | Page::View)) || (*filter != view.1 && view.0 == Page::Clips)
        });
        ui_kit::set_scroll_reset(&ctx, reset);
        self.last_view = Some(view);
        egui::CentralPanel::default()
            .frame(central)
            .show(ui, |ui| match self.page {
                Page::Clips => self.library(ui, frame),
                Page::Sources => self.sources_page(ui),
                Page::Settings => self.settings_page(ui),
                Page::Edit => self.editor_page(ui),
                Page::View => self.viewer_page(ui, frame),
            });

        laps.lap(match self.page {
            Page::Clips => "library page",
            Page::Sources => "sources page",
            Page::Settings => "settings page",
            Page::Edit => "editor page",
            Page::View => "player page",
        });
        self.dialogs(&ctx);
        self.rename_dialog(&ctx);
        self.collection_dialog(&ctx);
        self.delete_dialog(&ctx);
        self.close_dialog(&ctx);
        ui_kit::pointer_cursor(&ctx);
        ui_kit::lenient_clicks(&ctx);
        laps.lap("dialogs");

        // Library auto-refresh: poll the output folder ~once a second and rescan only
        // when it actually changed (cheap, no watcher thread).
        let now = Instant::now();
        if self.last_poll.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(1)) {
            self.last_poll = Some(now);
            self.permission = capture::screen_permission();
            laps.lap("permission");
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
        }
        laps.report();

        // Keep repainting: every frame while capturing (live timer), else a steady
        // tick so capture-thread events and folder changes are picked up promptly.
        if self.rec_state != RecState::Idle || self.toast.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        } else {
            ctx.request_repaint_after(Duration::from_millis(300));
        }
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        ui_kit::replay_click(ctx, raw_input);
    }

    /// What runs every frame whether the window shows or not (it's hidden
    /// in the tray): shortcuts, voice, capture events, saves and uploads.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        static LAUNCHED: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        let launched = *LAUNCHED.get_or_init(Instant::now);
        if let Some(secs) = std::env::var("HESTECLIPS_DEV_BUFFER_QUIT").ok().and_then(|s| s.parse::<f64>().ok())
            && !self.quitting
            && launched.elapsed().as_secs_f64() > secs
        {
            eprintln!("dev: quitting");
            self.quit(ctx);
        }
        self.background(ctx);
        self.pump_tray(ctx);
        self.handle_close(ctx);
        // A steady tick, also while hidden (nothing else wakes it then).
        ctx.request_repaint_after(if self.rec_state != RecState::Idle { Duration::from_millis(100) } else { Duration::from_millis(300) });
    }

    fn on_exit(&mut self) {
        eprintln!("quitting");
        // A downloaded update goes in once we're gone: the next launch is the new version.
        self.updater.install_on_exit(false);
    }
}

// --- Running in the background: the tray, closing the window ---
impl App {
    fn background(&mut self, _ctx: &egui::Context) {
        // Global shortcuts: these fire even while a game is focused. Paused while
        // the Settings recorder listens, so pressing the current key rebinds it
        // instead of starting a recording.
        let pressed = match &mut self.hotkeys {
            Ok(h) => {
                h.sync(&self.settings.shortcuts);
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
            }
        }

        // "Hashtag HesteClip that": saved already, as it was said.
        for _ in 0..self.voice.heard() {
            self.clip_saving();
        }
        let listen = self.settings.voice_clip && self.rec_state == RecState::Buffering && self.recording_shortcut.is_none();
        let mic = listen.then(|| self.voice_mic()).flatten();
        self.voice.listen(mic.as_deref());
        self.pump_capture_events();
        let capturing = match self.rec_state {
            RecState::Idle => games::Capturing::Off,
            RecState::Buffering => games::Capturing::Buffer(self.settings.replay_seconds),
            RecState::Recording => games::Capturing::Record,
        };
        self.games.set(capturing, &self.listed_apps());
        self.games.events.set_options(self.settings.game_details.then(|| self.settings.game_titles.clone()));
        let armed = (self.rec_state == RecState::Buffering && self.recording_shortcut.is_none()).then(|| service::Armed {
            library: self.settings.output_dir.clone(),
            folder_per_game: self.settings.folder_per_game,
            game_folders: self.settings.game_folders.clone(),
            sound: self.settings.save_sound.clone(),
        });
        self.quick_save.arm(armed);
        self.updater.set_auto(self.settings.auto_update);
        self.cloud.poll();
        self.pump_uploads();
        self.pump_renders();
        self.pump_mp3s();
        // Every frame, not just while the Sources page draws: leaving the page must
        // stop the meters' capture, or macOS keeps showing its recording indicator.
        self.ensure_level_monitor();
        self.ensure_video_preview();
        self.sync_capture_video();
        self.sync_capture_mics();
        self.sync_capture_settings();
        self.refit_webcam();
        *self.webcam_placement.lock().unwrap() = match &self.settings.webcam {
            Some(w) if w.enabled => w.placement.into(),
            _ => capture::webcam::Placement::hidden(),
        };
        // Keep the webcam open whenever one is set up and on, previewed or
        // recorded or not: closing a camera can reset its own settings. Off,
        // it's closed (its light goes out); a capture takes it up again.
        let on = self.settings.webcam.as_ref().is_some_and(|w| w.enabled);
        let camera = self.webcam_source().filter(|_| on).map(|w| (w.device, w.format));
        if camera != self.kept_camera {
            capture::webcam::keep_open(camera.clone());
            self.kept_camera = camera;
        }
    }

    /// Do what the tray (or another launch) asked.
    fn pump_tray(&mut self, ctx: &egui::Context) {
        let Some(tray) = &self.tray else { return };
        tray.update(self.rec_state == RecState::Buffering);
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
        if matches!(self.page, Page::View | Page::Sources) {
            self.viewer = None;
            self.page = Page::Clips;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }

    fn show_window(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
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
        let modal = egui::Modal::new(egui::Id::new("close_dialog")).show(ctx, |ui| {
            ui.set_width(400.0);
            ui.heading("Keep HesteClips running?");
            ui.add_space(6.0);
            let place = if cfg!(target_os = "macos") { "menu bar" } else { "tray" };
            ui.label(format!("In the {place}, your replay buffer and shortcuts keep working, so you can still save clips."));
            ui_kit::hint(ui, "You can change this in Settings.");
            ui.add_space(12.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                keep = ui.add(ui_kit::button(format!("Keep running in the {place}"), true)).clicked();
                quit = ui.add(ui_kit::button("Quit", false)).clicked();
                cancel = ui.add(ui_kit::button("Cancel", false)).clicked();
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

// --- Header: where you are on the left, capture status and actions on the right ---
impl App {
    fn capture_bar(&mut self, ui: &mut egui::Ui) {
        // ⌘, opens Settings, as in every Mac app.
        if !ui.ctx().egui_wants_keyboard_input() && !ui_kit::overlay_open(ui.ctx()) && ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Comma)) {
            self.page = Page::Settings;
        }
        let compact = ui.available_width() < 820.0;
        ui.horizontal(|ui| {
            ui.set_min_height(36.0);
            self.nav_tabs(ui, compact);
            ui.add_space(14.0);
            self.capture_status(ui, compact);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.capture_actions(ui, compact);
                self.update_button(ui, compact);
            });
        });
    }

    /// Clips · Sources · Settings, as one segmented control: words, which
    /// fit even narrow (icons were guesswork).
    fn nav_tabs(&mut self, ui: &mut egui::Ui, compact: bool) {
        let tabs = [
            (Page::Clips, "Clips", None),
            (Page::Sources, "Sources", Some("What goes into your clips: the picture, your mic, the game's sound".to_owned())),
            (Page::Settings, "Settings", Some(hotkey_label_cmd(","))),
        ];
        // The same control as the switches on the Sources page.
        let _ = compact;
        let labels: Vec<&str> = tabs.iter().map(|(_, l, _)| *l).collect();
        let chosen = tabs.iter().position(|(p, _, _)| *p == self.page).unwrap_or(usize::MAX);
        // As wide as the library's sidebar below, the two lined up.
        let responses = ui
            .allocate_ui(egui::vec2(library::SIDEBAR_W, ui_kit::CONTROL_H), |ui| {
                ui.set_width(library::SIDEBAR_W);
                ui_kit::segmented(ui, &labels, chosen)
            })
            .inner;
        for ((page, _, tip), mut r) in tabs.into_iter().zip(responses) {
            if let Some(tip) = tip {
                r = r.on_hover_text(tip);
            }
            if r.clicked() {
                self.page = page;
            }
        }
    }

    /// What capture is doing: a dot and a short line, with the hotkey hint below
    /// when there's room. Shown on every page so it's never a surprise.
    fn capture_status(&self, ui: &mut egui::Ui, compact: bool) {
        let elapsed = self.rec_started.map_or(Duration::ZERO, |t| t.elapsed());
        let replay = thumbs::format_duration(Duration::from_secs(self.settings.replay_seconds.into()));
        use settings::ShortcutAction as A;
        // The hint: the shortcut (drawn as keycaps) and what it does.
        let (live, color, title, hint_keys, hint) = match self.rec_state {
            RecState::Idle => (false, ui.visuals().weak_text_color(), "Not recording".to_owned(), None, String::new()),
            RecState::Buffering if self.saving > 0 => (
                true,
                ACCENT,
                if self.saving > 1 { format!("Saving {} clips…", self.saving) } else { "Saving clip…".to_owned() },
                self.shortcut_keys(A::SaveClip),
                format!("saves another of the last {replay}"),
            ),
            RecState::Buffering => (true, ACCENT, "Replay buffer on".to_owned(), self.shortcut_keys(A::SaveClip), format!("saves the last {replay}")),
            RecState::Recording => (true, REC_RED, format!("Recording  {}", thumbs::format_duration(elapsed)), self.shortcut_keys(A::ToggleRecord), "stops".to_owned()),
        };
        // The line under the status is a shortcut and what it does: without a
        // shortcut set it would be a stray word ("stops").
        let hint = if hint_keys.is_some() { hint } else { String::new() };
        let tip = match &hint_keys {
            Some(k) => format!("{} {hint}", k.join(" + ")),
            None => hint.clone(),
        };
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let (rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
            if live {
                // A soft pulse so it reads as "on" at a glance.
                let t = ui.input(|i| i.time);
                let halo = 0.25 + 0.2 * (t * 2.5).sin() as f32;
                ui.painter().circle_filled(rect.center(), 8.0, color.gamma_multiply(halo));
                ui.painter().circle_filled(rect.center(), 5.0, color);
            } else {
                ui.painter().circle_stroke(rect.center(), 5.0, egui::Stroke::new(1.5, color));
            }
            let title = RichText::new(&title).size(14.0).strong().color(if live { color } else { ui.visuals().text_color() });
            if compact || hint.is_empty() {
                ui.label(title).on_hover_text(&tip);
                return;
            }
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(title);
                {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 5.0;
                        if let Some(k) = &hint_keys {
                            shortcuts::keycaps(ui, k, 10.5);
                        }
                        ui.label(RichText::new(&hint).size(11.5).weak());
                    });
                }
            })
            .response
            .on_hover_text(&tip);
        });
    }

    /// The one or two buttons that make sense right now; the primary one rightmost.
    fn capture_actions(&mut self, ui: &mut egui::Ui, compact: bool) {
        let button = |text: &str, fill: Option<Color32>| match fill {
            Some(c) if c == REC_RED => ui_kit::danger_button(text),
            Some(_) => ui_kit::button(text, true),
            None => ui_kit::button(text, false),
        };
        match self.rec_state {
            RecState::Idle => {
                let start = if compact { "Replay buffer" } else { "Start replay buffer" };
                if ui
                    .add(button(start, Some(ACCENT)))
                    .on_hover_text(format!("Keep the last {} ready to save ({})", thumbs::format_duration(Duration::from_secs(self.settings.replay_seconds.into())), self.shortcut_label(settings::ShortcutAction::ToggleBuffer)))
                    .clicked()
                {
                    self.start_replay_buffer();
                }
                if ui.add(button("Record", None)).on_hover_text(format!("Record until you stop ({})", self.shortcut_label(settings::ShortcutAction::ToggleRecord))).clicked() {
                    self.start_recording();
                }
            }
            RecState::Buffering => {
                if ui
                    .add(button("Save clip", Some(ACCENT)))
                    .on_hover_text(self.shortcut_label(settings::ShortcutAction::SaveClip))
                    .clicked()
                {
                    self.save_clip();
                }
                if ui.add(button("Stop", None)).on_hover_text(format!("Stop the replay buffer ({})", self.shortcut_label(settings::ShortcutAction::ToggleBuffer))).clicked() {
                    self.stop();
                }
            }
            RecState::Recording => {
                let stop = if compact { "Stop" } else { "Stop recording" };
                if ui.add(button(stop, Some(REC_RED))).on_hover_text(self.shortcut_label(settings::ShortcutAction::ToggleRecord)).clicked() {
                    self.stop();
                }
            }
        }
    }

    /// Follow the game being clipped, for Discord: the last one in focus, for
    /// as long as it's open.
    fn track_game(&mut self) {
        if self.rec_state == RecState::Idle {
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

    /// The microphone "hashtag HesteClip that" is heard through: the first
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
        let game = if self.rec_state == RecState::Recording { self.games.recording_game() } else { self.games.clip_game() };
        games::folder_for(&self.settings.output_dir, self.settings.folder_per_game, &self.settings.game_folders, game.as_deref())
    }

    /// Discord shows what's being clipped while capture runs (when turned on).
    fn update_presence(&mut self) {
        if !self.settings.discord_presence || self.rec_state == RecState::Idle {
            self.presence_since = None;
            self.presence.set(None);
            return;
        }
        let game = self.clipped_game.as_ref().map(|(_, g, _)| g.clone());
        let since = *self.presence_since.get_or_insert_with(SystemTime::now);
        self.presence.set(Some(discord::Status { recording: self.rec_state == RecState::Recording, game, since }));
    }

    /// "Update ready", once a new version is downloaded: restarts into it. It
    /// installs on quit anyway, so this is only for the impatient — and never
    /// cuts a recording short.
    fn update_button(&mut self, ui: &mut egui::Ui, compact: bool) {
        let update::Status::Ready(version) = self.updater.status() else {
            return;
        };
        ui.add_space(6.0);
        let text = if compact { "⬆" } else { "Update ready" };
        let button = egui::Button::new(RichText::new(text).size(13.0).color(ACCENT))
            .fill(ACCENT.gamma_multiply(0.15))
            .stroke(egui::Stroke::NONE)
            .min_size(egui::vec2(0.0, 28.0))
            .corner_radius(14);
        let recording = self.rec_state == RecState::Recording;
        let tip = if self.rec_state == RecState::Buffering {
            let after = if self.settings.auto_start_buffer { "starts again, empty" } else { "stops" };
            format!("HesteClips {version} is ready. Click to restart into it now (the replay buffer {after}), or it installs when you quit.")
        } else {
            format!("HesteClips {version} is ready. Click to restart into it now, or it installs when you quit.")
        };
        let r = ui
            .add_enabled(!recording, button)
            .on_hover_text(tip)
            .on_disabled_hover_text(format!("HesteClips {version} is ready. It installs when you quit, or restart once your recording is done."));
        if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && self.updater.install_on_exit(true) {
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
                "HesteClips needs Screen Recording permission before it can capture.",
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
        if self.rec_state == RecState::Recording {
            self.toast_error("Stop recording before editing a clip.");
            return;
        }
        self.editor = Some(editor::Editor::open(&self.ctx(), &clip, self.settings.editor_preview_share));
        self.page = Page::Edit;
    }

    /// Play a clip in the app's own viewer.
    pub(crate) fn open_viewer(&mut self, clip: PathBuf) {
        if viewer::volume(&self.ctx()).is_none() {
            viewer::set_volume(&self.ctx(), (self.settings.player_volume.clamp(0.0, 1.0), self.settings.player_muted));
        }
        self.viewer = Some(self.viewer_for(&clip));
        self.page = Page::View;
    }

    /// A viewer for this clip: its "Saving your edit" screen while an edit
    /// of it is still being written, the clip otherwise.
    fn viewer_for(&self, clip: &std::path::Path) -> viewer::Viewer {
        if self.renders.iter().any(|j| !j.as_new && j.source == clip) {
            viewer::Viewer::saving(clip)
        } else {
            viewer::Viewer::open(&self.ctx(), clip)
        }
    }

    fn viewer_page(&mut self, ui: &mut egui::Ui, frame: &eframe::Frame) {
        let delete_label = self.delete_label(None);
        let saving = self.viewer.as_ref().and_then(|v| self.renders.iter().find(|j| !j.as_new && j.source == v.clip())).map(|j| j.progress());
        let Some(v) = &mut self.viewer else {
            self.page = Page::Clips;
            return;
        };
        // Neighbours among the clips shown in the library (a game's, a
        // collection's), newest first, to step through; all clips when this one
        // isn't among them (taken out of the collection meanwhile).
        let shown: Vec<&clips::Clip> = self.clips.iter().filter(|c| self.library_filter.matches(c, &self.collections)).collect();
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
        };
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
                self.viewer = None;
                self.page = Page::Clips;
                self.open_editor(clip.clone());
                if self.page == Page::Edit {
                    self.edit_return = Some(clip);
                }
            }
            viewer::ViewerOutcome::Open(clip) => self.viewer = Some(self.viewer_for(&clip)),
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
                // Its button says just "Delete", and the key nothing: ask.
                let clip = v.clip().to_path_buf();
                self.delete_clips(&[clip], true);
            }
            viewer::ViewerOutcome::Rename => {
                let clip = v.clip().to_path_buf();
                self.rename_clip(clip);
            }
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
        let outcome = ed.ui(ui);
        self.settings.editor_preview_share = ed.preview_share();
        match outcome {
            editor::EditorOutcome::Stay => {}
            editor::EditorOutcome::Close => self.close_editor(),
            editor::EditorOutcome::Reverted(target) => {
                match store::revert(&target) {
                    Ok(()) => self.toast("Reverted to the original clip"),
                    Err(e) => self.toast_error(format!("Couldn't revert: {e}")),
                }
                self.close_editor();
            }
            editor::EditorOutcome::Rename => {
                if let Some(ed) = &self.editor {
                    let clip = ed.clip().to_path_buf();
                    self.rename_clip(clip);
                }
            }
            editor::EditorOutcome::Saved { target, info, edit, new_name, then } => {
                self.start_render(target, info, edit, new_name, then);
                self.close_editor();
            }
            editor::EditorOutcome::FreeUp(target, how) => {
                // Closed first: the editor reads the original.
                self.close_editor();
                self.start_free_up(target, how);
            }
        }
    }

    fn close_editor(&mut self) {
        let clip = self.editor.as_ref().map(|e| e.clip().to_path_buf());
        self.editor = None; // drops the player: stops audio and decoders
        self.page = Page::Clips;
        self.refresh_clips();
        // Opened from the player: back to it, on this clip (renamed meanwhile,
        // it's the new name). While its edit saves, the player waits for it.
        if self.edit_return.take().is_some()
            && let Some(clip) = clip
        {
            if self.renders.iter().any(|j| !j.as_new && j.source == clip) {
                self.viewer = Some(viewer::Viewer::saving(&clip));
                self.page = Page::View;
            } else {
                self.open_viewer(clip);
            }
        }
    }

    /// Render an edit in the background. `new_name`: save as a separate clip with
    /// that name instead of updating this clip's edit.
    fn start_render(&mut self, target: store::EditTarget, info: media::ClipInfo, edit: media::Edit, new_name: Option<String>, then: Option<store::FreeUp>) {
        let ext = target.clip.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or("mp4".into());
        // In place: render into the clip's asset folder, then swap it in (see
        // `store::commit_render`). As new: straight to the new file, no history.
        let (dest, clip_id) = match &new_name {
            None => {
                let clip_id = target.id.clone().unwrap_or_else(store::new_id);
                let staged = store::staging_path(target.library(), &clip_id, &ext);
                if let Err(e) = store::create_assets_dir(target.library(), &clip_id) {
                    self.toast_error(format!("Couldn't save the edit: {e}"));
                    return;
                }
                (staged, Some(clip_id))
            }
            // Next to the clip, in the same game's folder.
            Some(name) => match clips::path_for_name(target.clip.parent().unwrap_or(target.library()), name, &ext, None) {
                Ok(p) => (p, None),
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
            dest: if new_name.is_some() { dest.clone() } else { target.clip.clone() },
            as_new: new_name.is_some(),
            progress: progress.clone(),
        });
        let tx = self.render_tx.clone();
        let ctx = self.ctx();
        // When the clip was made: the saved edit (or new clip) keeps it, so it
        // stays on its day in the library instead of jumping to today.
        let made = std::fs::metadata(&target.clip).and_then(|m| m.modified()).ok();
        let permanently = self.settings.delete_permanently;
        std::thread::spawn(move || {
            let report = |f: f32| {
                progress.store(f.to_bits(), std::sync::atomic::Ordering::Relaxed);
                ctx.request_repaint();
            };
            let rendered = media::render_with_progress(&target.source, &info, &edit, &dest, clip_id.as_deref(), report).map_err(|e| e.to_string());
            let result = match &clip_id {
                None => rendered.map(|()| dest),
                Some(clip_id) => {
                    let committed = rendered.and_then(|()| store::commit_render(&target, &dest, clip_id, &edit).map_err(|e| e.to_string()));
                    if committed.is_err() {
                        store::abandon_render(target.library(), clip_id, &dest);
                    }
                    committed.map(|()| target.clip.clone())
                }
            };
            if let (Ok(path), Some(made)) = (&result, made) {
                if let Err(e) = std::fs::File::options().write(true).open(path).and_then(|f| f.set_modified(made)) {
                    eprintln!("couldn't keep the clip's date on {}: {e}", path.display());
                }
            }
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

    /// Trim or delete an edited clip's original, in the background. The clip
    /// shows as saving meanwhile.
    fn start_free_up(&mut self, target: store::EditTarget, how: store::FreeUp) {
        let id = self.next_render_id;
        self.next_render_id += 1;
        self.renders.push(RenderJob {
            id,
            source: target.clip.clone(),
            dest: target.clip.clone(),
            as_new: false,
            progress: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
        });
        let tx = self.render_tx.clone();
        let ctx = self.ctx();
        let permanently = self.settings.delete_permanently;
        std::thread::spawn(move || {
            let freed = store::free_up(&target, how, permanently);
            let _ = tx.send(RenderDone { id, result: Ok(target.clip.clone()), freed: Some((how, freed)) });
            ctx.request_repaint();
        });
    }

    /// Tell the user how background uploads ended.
    fn pump_uploads(&mut self) {
        for done in self.cloud.take_finished() {
            match done {
                cloud::UploadDone::Uploaded { clip, to, link: None } => {
                    self.toast(format!("Uploaded {} to {}", file_name(&clip), to.display()));
                }
                cloud::UploadDone::Uploaded { clip, link: Some(link), .. } => self.public_links.push_back((clip, link)),
                cloud::UploadDone::Failed { clip, error } => {
                    self.toast_error(format!("Couldn't upload {}: {error}", file_name(&clip)));
                }
                cloud::UploadDone::Cancelled { clip } => self.toast(format!("Upload of {} cancelled", file_name(&clip))),
            }
        }
    }

    fn pump_renders(&mut self) {
        while let Ok(RenderDone { id, result, freed }) = self.render_rx.try_recv() {
            let Some(i) = self.renders.iter().position(|j| j.id == id) else { continue };
            match freed {
                Some((how, Ok(bytes))) => {
                    let what = if how == store::FreeUp::Trim { "Trimmed the original" } else { "Deleted the original" };
                    let bin = if how == store::FreeUp::Delete && !self.settings.delete_permanently {
                        format!(" (empty the {} to get the space back)", store::bin_name())
                    } else {
                        String::new()
                    };
                    if bytes == 0 && how == store::FreeUp::Trim {
                        self.toast("The original is already as short as this edit");
                    } else {
                        self.toast(format!("{what}: freed {}{bin}", export_ui::human_bytes(bytes)));
                    }
                }
                Some((_, Err(e))) => self.toast_error(format!("Couldn't free up the space: {e}")),
                None => {}
            }
            let job = self.renders.remove(i);
            match result {
                Ok(path) => {
                    proxy::prebuild(&self.ctx(), &path);
                    if !job.as_new && self.viewer.as_ref().is_some_and(|v| v.is_saving() && v.clip() == job.source) {
                        self.open_viewer(job.source.clone());
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
                        self.open_viewer(job.source.clone());
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

fn file_stem(path: &std::path::Path) -> String {
    path.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

// --- Capture actions ---
impl App {
    fn toggle_buffer(&mut self) {
        match self.rec_state {
            RecState::Idle => self.start_replay_buffer(),
            RecState::Buffering => self.stop(),
            RecState::Recording => {} // busy recording; ignore
        }
    }

    fn toggle_record(&mut self) {
        match self.rec_state {
            RecState::Idle => self.start_recording(),
            RecState::Recording => self.stop(),
            // One keypress should do what the user means: stop buffering, record.
            RecState::Buffering => {
                self.service.stop(None, None);
                self.start_recording();
            }
        }
    }

    /// Re-read audio devices right before capture so "Default" follows whatever the
    /// OS default is *now* (e.g. headphones plugged in since launch).
    fn refresh_audio_devices(&mut self) {
        self.audio = capture::audio::list_audio_devices();
    }

    fn start_replay_buffer(&mut self) {
        self.refresh_audio_devices();
        self.capturing_video = Some(self.video_source());
        self.capturing_sources = self.capture_sources();
        self.capturing_replay = Some(self.settings.replay_seconds);
        // Optimistic state; a State/Error event confirms or corrects it.
        self.service.start(capture::Mode::ReplayBuffer, self.encode_settings());
        self.rec_state = RecState::Buffering;
        self.rec_started = Some(Instant::now());
    }

    fn start_recording(&mut self) {
        self.refresh_audio_devices();
        self.capturing_video = Some(self.video_source());
        self.capturing_sources = self.capture_sources();
        self.capturing_replay = Some(self.settings.replay_seconds);
        self.service.start(capture::Mode::Record, self.encode_settings());
        self.rec_state = RecState::Recording;
        self.rec_started = Some(Instant::now());
    }

    fn save_clip(&mut self) {
        if self.rec_state != RecState::Buffering {
            return;
        }
        // Its moment is taken at once; the Saved event lands it in the library.
        // Another can be saved while it's still being written.
        let dir = self.clip_folder();
        self.service.save_clip(dir, self.games.clip_details());
        sound::play_saved(&self.settings.save_sound);
        self.clip_saving();
    }

    /// A clip is being saved: a card for it in the library (the page you're
    /// on stays).
    fn clip_saving(&mut self) {
        self.saving += 1;
    }

    fn stop(&mut self) {
        let (mut dir, mut details) = (None, None);
        if self.rec_state == RecState::Recording {
            self.saving += 1; // finishing the file
            dir = Some(self.clip_folder());
            details = self.games.recording_details();
        }
        self.service.stop(dir, details);
        self.rec_state = RecState::Idle;
        self.rec_started = None;
    }

    /// A changed list of games and apps reaches a running capture right away;
    /// it starts following the new list without restarting.
    fn sync_capture_video(&mut self) {
        if self.rec_state == RecState::Idle {
            self.capturing_video = None;
            return;
        }
        let now = self.video_source();
        let both_apps = matches!((&now, &self.capturing_video), (capture::VideoSource::Apps { .. }, Some(capture::VideoSource::Apps { .. })));
        if both_apps && self.capturing_video.as_ref() != Some(&now) {
            self.service.update_video(now.clone());
            self.capturing_video = Some(now);
        }
    }

    /// A microphone source switched to another device reaches the running
    /// capture right away: its track stays, only where its sound comes from
    /// changes. Anything else about the sources waits for the next start.
    fn sync_capture_mics(&mut self) {
        if self.rec_state == RecState::Idle {
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

    /// The replay length reaches a running replay buffer right away. Not
    /// cleared while idle: the next start sends its own.
    fn sync_capture_settings(&mut self) {
        if self.rec_state == RecState::Buffering && self.capturing_replay != Some(self.settings.replay_seconds) {
            self.service.set_replay_seconds(self.settings.replay_seconds);
            self.capturing_replay = Some(self.settings.replay_seconds);
        }
    }

    /// Drain capture-thread events each frame.
    fn pump_capture_events(&mut self) {
        while let Some(evt) = self.service.poll() {
            match evt {
                // Authoritative state from the capture thread — keeps the UI honest.
                Evt::State(state) => {
                    let new = match state {
                        None => RecState::Idle,
                        Some(capture::Mode::ReplayBuffer) => RecState::Buffering,
                        Some(capture::Mode::Record) => RecState::Recording,
                    };
                    if new != self.rec_state {
                        self.rec_started = (new != RecState::Idle).then(Instant::now);
                    }
                    self.rec_state = new;
                    if new == RecState::Idle {
                        self.refresh_clips();
                    }
                }
                Evt::Saved(path) => {
                    self.saving = self.saving.saturating_sub(1);
                    // A new clip is never hidden in a folded Today.
                    self.folded.set(chrono::Local::now().date_naive(), false);
                    self.toast(format!("Saved {}", file_name(&path)));
                    // Scrub frames ready before it's opened.
                    proxy::prebuild(&self.ctx(), &path);
                    self.last_saved = Some((path.clone(), Instant::now()));
                    self.refresh_clips();
                    // Showing another game's clips: show this one too.
                    self.library_filter.reveal(&self.clips, &self.collections, &path);
                }
                Evt::Error(e) => {
                    // A start failure is followed by Evt::State(None); a save failure
                    // leaves us still buffering. So just surface the message.
                    self.saving = self.saving.saturating_sub(1);
                    self.toast_error(e);
                }
            }
        }
    }
}

/// Open the macOS Screen Recording privacy pane directly.
fn open_screen_settings() {
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
        if total < Self::SLOW {
            return;
        }
        let parts: Vec<String> = self.laps.iter().filter(|(_, d)| *d >= Duration::from_millis(5)).map(|(w, d)| format!("{w} {} ms", d.as_millis())).collect();
        eprintln!("{} slow UI update: {} ms ({})", chrono::Local::now().format("%H:%M:%S"), total.as_millis(), parts.join(", "));
    }
}

/// Just the file name of a path, for status messages.
fn file_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

