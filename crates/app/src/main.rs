//! HesteClips — a fast, no-nonsense clip recorder.
//!
//! The app opens on your clips. A slim capture bar on top always shows what's
//! happening (buffer armed, recording, idle) and holds the one or two buttons that
//! make sense right now; the global hotkeys do the same from inside a game.
//! Settings is a separate page you rarely visit, reached from the gear.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // no console window on release Windows

mod away;
mod clips;
mod webcam_ui;
mod cloud;
mod cloud_ui;
mod discord;
mod editor;
mod filmstrip;
mod library;
mod meter;
mod player;
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
mod update;
#[cfg(hw_decode)]
mod gpu_frames;
#[cfg(hw_decode)]
mod video;
mod viewer;
mod waveform;
mod wheel;

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use egui::{Color32, RichText};
use global_hotkey::{GlobalHotKeyEvent, HotKeyState};
use library::{ACCENT, REC_RED};
use service::{CaptureService, Evt};
use settings::{Encoder, RecordSettings, SourceKind};

fn main() -> eframe::Result<()> {
    // The installer's hooks (install, update, uninstall) run the app with special
    // arguments and exit here; a downloaded update left uninstalled is put in
    // place now. Does nothing in a development build.
    #[cfg(windows)]
    velopack::VelopackApp::build().run();

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
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(size)
            .with_min_inner_size([560.0, 420.0])
            .with_title("HesteClips")
            .with_icon(app_icon()),
        wgpu_options: wgpu_options(),
        ..Default::default()
    };

    eframe::run_native(
        "HesteClips",
        options,
        Box::new(|cc| {
            #[cfg(hw_decode)]
            gpu_frames::init(cc.wgpu_render_state.as_ref());
            Ok(Box::new(App::new(cc.egui_ctx.clone())))
        }),
    )
}

/// The renderer's setup. On Windows, D3D12, so decoded video frames can be
/// shared with it straight from the GPU (`gpu_frames.rs`); wgpu's default
/// pick could be Vulkan, which can't open them as simply.
/// `HESTECLIPS_WGPU_BACKEND` (e.g. "vulkan") overrides it, to compare.
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
    #[cfg(windows)]
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(new) = &mut options.wgpu_setup {
        new.instance_descriptor.backends = match std::env::var("HESTECLIPS_WGPU_BACKEND") {
            Ok(b) => eframe::wgpu::Backends::from_comma_list(&b),
            Err(_) => eframe::wgpu::Backends::DX12,
        };
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
fn app_icon() -> egui::IconData {
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

/// egui's bundled fonts have no ⌘ ⌥ ⌃ ⇧ ⌫, which macOS writes every shortcut
/// with. Use the system's symbol font as a fallback for those (it's only
/// consulted for characters the bundled fonts lack).
fn add_symbol_font(ctx: &egui::Context) {
    #[cfg(target_os = "macos")]
    if let Ok(bytes) = std::fs::read("/System/Library/Fonts/Apple Symbols.ttf") {
        use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};
        let families = [egui::FontFamily::Proportional, egui::FontFamily::Monospace]
            .map(|family| InsertFontFamily { family, priority: FontPriority::Lowest })
            .to_vec();
        ctx.add_font(FontInsert::new("apple-symbols", egui::FontData::from_owned(bytes), families));
    }
    #[cfg(not(target_os = "macos"))]
    let _ = ctx;
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
    /// Adding a custom clip-saved sound failed: why.
    pub(crate) sound_error: Option<String>,
    /// Screen-recording permission, re-checked each poll so the banner clears the
    /// moment the user grants it.
    permission: capture::Permission,
    /// Screens detected by the capture backend.
    screens: Vec<capture::Device>,
    /// Apps with a window, for the games-and-apps list: to add, and to show
    /// which are open. Refreshed while the Sources page shows.
    pub(crate) windowed_apps: Vec<capture::Device>,
    /// What the running capture records, to send it list changes live.
    capturing_video: Option<capture::VideoSource>,
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
    clips: Vec<clips::Clip>,
    thumbs: thumbs::Thumbs,
    /// Most recently saved clip, highlighted in the library for a few seconds.
    last_saved: Option<(PathBuf, Instant)>,
    cloud: cloud::Cloud,
    /// Open modal, if any.
    dialog: Option<cloud_ui::Dialog>,
    toast: Option<Toast>,
    editor: Option<editor::Editor>,
    viewer: Option<viewer::Viewer>,
    /// Edits being rendered in the background, with live progress.
    pub(crate) renders: Vec<RenderJob>,
    render_tx: std::sync::mpsc::Sender<(u64, Result<PathBuf, String>)>,
    render_rx: std::sync::mpsc::Receiver<(u64, Result<PathBuf, String>)>,
    next_render_id: u64,
    /// Open "Rename clip" dialog.
    pub(crate) rename: Option<library::Rename>,
    /// Clips selected in the library for a bulk action.
    pub(crate) selection: library::Selection,
    /// Library auto-refresh: last folder poll + last-seen folder mtime.
    last_poll: Option<Instant>,
    dir_mtime: Option<SystemTime>,
    pub(crate) updater: update::Updater,
    pub(crate) presence: discord::Presence,
    /// The game being clipped, for Discord: its executable, and when we last
    /// checked it's still open.
    clipped_game: Option<(String, Instant)>,
    /// When the current capture started, as Discord's "elapsed" timer counts.
    presence_since: Option<SystemTime>,
}

impl App {
    fn new(ctx: egui::Context) -> Self {
        add_symbol_font(&ctx);
        let settings = RecordSettings::load();
        sound::preload(&settings.save_sound.sound);
        let recovered = capture::output::recover_unfinished(&settings.output_dir);
        let saved_settings = settings.to_json();
        let clips = clips::scan(&settings.output_dir);
        // Assets of clips deleted in Finder go to the Bin.
        store::sweep_orphans(&settings.output_dir, &clips);
        let hotkeys = shortcuts::Registered::new().map_err(|e| e.to_string()).map(|mut h| {
            h.sync(&settings.shortcuts);
            h
        });
        let (render_tx, render_rx) = std::sync::mpsc::channel();
        let live_audio = capture::mixer::LiveAudio::new();
        live_audio.set_limiter(settings.limiter);
        for s in &settings.audio_sources {
            live_audio.channel(&s.id).set_volume(sources_ui::from_db(s.volume_db), s.muted);
        }
        let updater = update::Updater::new(ctx.clone(), settings.auto_update);
        let mut app = Self {
            page: Page::Clips,
            rec_state: RecState::Idle,
            rec_started: None,
            saving: 0,
            settings,
            saved_settings,
            persist_settings: !std::env::vars_os().any(|(k, _)| {
                let k = k.to_string_lossy();
                ["HESTECLIPS_OPEN_", "HESTECLIPS_DEMO_", "HESTECLIPS_LIBRARY"].iter().any(|p| k.starts_with(p))
            }),
            service: CaptureService::new(live_audio.clone()),
            hotkeys,
            recording_shortcut: None,
            confirm_reset: false,
            sound_error: None,
            reveal_clip: None,
            permission: capture::screen_permission(),
            screens: capture::list_screens(),
            windowed_apps: Vec::new(),
            capturing_video: None,
            away_screen: away::screen(),
            webcam_placement: std::sync::Arc::new(std::sync::Mutex::new(capture::webcam::Placement::default_for(16.0 / 9.0, 16.0 / 9.0))),
            webcam_view: Default::default(),
            kept_camera: None,
            audio: capture::audio::list_audio_devices(),
            live_audio,
            level_monitor: None,
            video_preview: None,
            sources_view: Default::default(),
            clips,
            thumbs: thumbs::Thumbs::new(ctx.clone()),
            last_saved: None,
            cloud: cloud::Cloud::new(ctx),
            dialog: None,
            toast: None,
            editor: None,
            viewer: None,
            renders: Vec::new(),
            render_tx,
            render_rx,
            next_render_id: 0,
            rename: None,
            selection: library::Selection::default(),
            last_poll: None,
            dir_mtime: None,
            updater,
            presence: discord::Presence::new(),
            clipped_game: None,
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
                app.start_render(target.clone(), info.clone(), edit.clone(), None);
                app.start_render(target, info, edit, Some("Demo highlight".into()));
            }
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
        self.clips = clips::scan(&self.settings.output_dir);
        self.selection.retain(&self.clips);
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
            webcam: self.webcam_source(),
            sources: self.capture_sources(),
        }
    }

    /// What the video shows, for the capture backend.
    /// The webcam, for the capture backend.
    pub(crate) fn webcam_source(&self) -> Option<capture::webcam::Webcam> {
        let w = self.settings.webcam.as_ref().filter(|_| capture::webcam::AVAILABLE)?;
        Some(capture::webcam::Webcam { device: w.id.clone(), format: w.format.map(Into::into), placement: self.webcam_placement.clone() })
    }

    pub(crate) fn video_source(&self) -> capture::VideoSource {
        match &self.settings.capture {
            settings::CaptureTarget::Apps { apps, away_screen } if capture::APP_CAPTURE => capture::VideoSource::Apps {
                ids: apps.iter().map(|a| a.id.clone()).collect(),
                away_when_unfocused: *away_screen,
            },
            _ => capture::VideoSource::Screen {
                id: self.screens.get(self.settings.display_index).map(|d| d.id.clone()).unwrap_or_else(|| "0".to_owned()),
            },
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

        // Global shortcuts: these fire even while a game is focused. Paused while
        // the Settings recorder listens, so pressing the current key rebinds it
        // instead of starting a recording.
        if let Ok(h) = &mut self.hotkeys {
            h.sync(&self.settings.shortcuts);
        }
        while let Ok(ev) = GlobalHotKeyEvent::receiver().try_recv() {
            if ev.state != HotKeyState::Pressed || self.recording_shortcut.is_some() {
                continue;
            }
            match self.hotkeys.as_ref().ok().and_then(|h| h.action_for(ev.id)) {
                Some(settings::ShortcutAction::ToggleBuffer) => self.toggle_buffer(),
                Some(settings::ShortcutAction::ToggleRecord) => self.toggle_record(),
                Some(settings::ShortcutAction::SaveClip) => self.save_clip(),
                None => {}
            }
        }

        self.pump_capture_events();
        self.updater.set_auto(self.settings.auto_update);
        self.cloud.poll();
        self.pump_uploads();
        self.pump_renders();
        // Every frame, not just while the Sources page draws: leaving the page must
        // stop the meters' capture, or macOS keeps showing its recording indicator.
        self.ensure_level_monitor();
        self.ensure_video_preview();
        self.sync_capture_video();
        if let Some(w) = &self.settings.webcam {
            *self.webcam_placement.lock().unwrap() = if w.enabled { w.placement.into() } else { capture::webcam::Placement::hidden() };
        }
        // Keep the webcam open whenever one is set up and on, previewed or
        // recorded or not: closing a camera can reset its own settings. Off,
        // it's closed (its light goes out); a capture takes it up again.
        let on = self.settings.webcam.as_ref().is_some_and(|w| w.enabled);
        let camera = self.webcam_source().filter(|_| on).map(|w| (w.device, w.format));
        if camera != self.kept_camera {
            capture::webcam::keep_open(camera.clone());
            self.kept_camera = camera;
        }

        // Left the viewer some other way (a hotkey that shows the library): stop it.
        if self.page != Page::View && self.viewer.is_some() {
            self.viewer = None;
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        }

        // The editor and viewer get the whole window; capture keeps running
        // underneath and the hotkeys still work.
        if !matches!(self.page, Page::Edit | Page::View) {
            egui::Panel::top("capture_bar")
                .frame(
                    egui::Frame::new()
                        .fill(ui.visuals().panel_fill)
                        .inner_margin(egui::Margin::symmetric(16, 10))
                        .stroke(egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color)),
                )
                .show(ui, |ui| self.capture_bar(ui));
        }

        if let Some(toast) = &self.toast {
            if toast.at.elapsed() < TOAST_FOR {
                let (text, error) = (toast.text.clone(), toast.error);
                egui::Panel::bottom("toast").show(ui, |ui| {
                    ui.add_space(2.0);
                    let color = if error { ui.visuals().error_fg_color } else { ui.visuals().text_color() };
                    ui.colored_label(color, text);
                    ui.add_space(2.0);
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
            egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin::symmetric(16, 4))
        };
        egui::CentralPanel::default()
            .frame(central)
            .show(ui, |ui| match self.page {
                Page::Clips => self.library(ui, frame),
                Page::Sources => self.sources_page(ui),
                Page::Settings => self.settings_page(ui),
                Page::Edit => self.editor_page(ui),
                Page::View => self.viewer_page(ui),
            });

        self.dialogs(&ctx);
        self.rename_dialog(&ctx);

        // Library auto-refresh: poll the output folder ~once a second and rescan only
        // when it actually changed (cheap, no watcher thread).
        let now = Instant::now();
        if self.last_poll.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(1)) {
            self.last_poll = Some(now);
            self.permission = capture::screen_permission();
            let mtime = std::fs::metadata(&self.settings.output_dir)
                .and_then(|m| m.modified())
                .ok();
            if mtime != self.dir_mtime {
                self.dir_mtime = mtime;
                self.refresh_clips();
            }
            self.update_presence();
            // Persist settings as they change; no Save button to forget.
            let json = self.settings.to_json();
            if json != self.saved_settings && self.persist_settings {
                RecordSettings::save_json(&json);
                self.saved_settings = json;
            }
        }

        // Keep repainting: every frame while capturing (live timer), else a steady
        // tick so capture-thread events and folder changes are picked up promptly.
        if self.rec_state != RecState::Idle || self.toast.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        } else {
            ctx.request_repaint_after(Duration::from_millis(300));
        }
    }

    fn on_exit(&mut self) {
        // A downloaded update goes in once we're gone: the next launch is the new version.
        self.updater.install_on_exit(false);
    }
}

// --- Header: where you are on the left, capture status and actions on the right ---
impl App {
    fn capture_bar(&mut self, ui: &mut egui::Ui) {
        // ⌘, opens Settings, as in every Mac app.
        if !ui.ctx().egui_wants_keyboard_input() && ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Comma)) {
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

    /// Clips · Sources · Settings, as one segmented control. Icons only when narrow.
    fn nav_tabs(&mut self, ui: &mut egui::Ui, compact: bool) {
        let tabs = [
            (Page::Clips, "🎬", "Clips", "Your clips".to_owned()),
            (Page::Sources, "🎤", "Sources", "What goes into your clips: mic, desktop sound, apps, with live levels".to_owned()),
            (Page::Settings, "⚙", "Settings", format!("Settings ({})", hotkey_label_cmd(","))),
        ];
        let v = ui.visuals().clone();
        egui::Frame::new()
            .fill(v.extreme_bg_color)
            .corner_radius(9)
            .inner_margin(egui::Margin::same(3))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for (page, icon, label, tip) in tabs {
                    // Painted, not an egui Button: a button grows a hover stroke,
                    // which made the tabs change width under the pointer.
                    let selected = self.page == page;
                    let text = if compact { icon.to_owned() } else { format!("{icon}  {label}") };
                    let galley = ui.painter().layout_no_wrap(text, egui::FontId::proportional(14.0), Color32::WHITE);
                    let w = if compact { 34.0 } else { galley.size().x + 22.0 };
                    let (rect, mut r) = ui.allocate_exact_size(egui::vec2(w, 28.0), egui::Sense::click());
                    let color = if selected {
                        v.strong_text_color()
                    } else if r.hovered() {
                        v.text_color()
                    } else {
                        v.weak_text_color()
                    };
                    if selected {
                        ui.painter().rect_filled(rect, 7, v.widgets.active.weak_bg_fill);
                    } else if r.hovered() {
                        ui.painter().rect_filled(rect, 7, v.widgets.hovered.weak_bg_fill.gamma_multiply(0.5));
                    }
                    ui.painter().galley_with_override_text_color(rect.center() - galley.size() / 2.0, galley, color);
                    if compact || page == Page::Settings {
                        r = r.on_hover_text(tip);
                    }
                    if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        self.page = page;
                    }
                }
            });
    }

    /// What capture is doing: a dot and a short line, with the hotkey hint below
    /// when there's room. Shown on every page so it's never a surprise.
    fn capture_status(&self, ui: &mut egui::Ui, compact: bool) {
        let elapsed = self.rec_started.map_or(Duration::ZERO, |t| t.elapsed());
        let replay = thumbs::format_duration(Duration::from_secs(self.settings.replay_seconds.into()));
        use settings::ShortcutAction as A;
        // The hint: the shortcut (drawn as keycaps) and what it does.
        let (live, color, title, hint_keys, hint) = match self.rec_state {
            RecState::Idle => (false, ui.visuals().weak_text_color(), "Not recording".to_owned(), self.shortcut_keys(A::ToggleBuffer), "starts the replay buffer".to_owned()),
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
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(RichText::new(&title).size(14.0).strong().color(if live { color } else { ui.visuals().text_color() }));
                if !compact {
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
        let button = |text: &str, fill: Option<Color32>| {
            let mut rt = RichText::new(text.to_owned()).size(14.0);
            if fill.is_some() {
                rt = rt.color(Color32::WHITE);
            }
            let mut b = egui::Button::new(rt).min_size(egui::vec2(0.0, 32.0)).corner_radius(8);
            if let Some(f) = fill {
                b = b.fill(f);
            }
            b
        };
        match self.rec_state {
            RecState::Idle => {
                let start = if compact { "⏺  Replay buffer" } else { "⏺  Start replay buffer" };
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
                    .add(button("💾  Save clip", Some(ACCENT)))
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
                let stop = if compact { "⏹  Stop" } else { "⏹  Stop recording" };
                if ui.add(button(stop, Some(REC_RED))).on_hover_text(self.shortcut_label(settings::ShortcutAction::ToggleRecord)).clicked() {
                    self.stop();
                }
            }
        }
    }

    /// Discord shows what's being clipped while capture runs (when turned on).
    /// The game is the last one in focus from the games-and-apps list, or any
    /// game Discord knows.
    fn update_presence(&mut self) {
        if !self.settings.discord_presence || self.rec_state == RecState::Idle {
            self.presence_since = None;
            self.clipped_game = None;
            self.presence.set(None);
            return;
        }
        let apps = match &self.settings.capture {
            settings::CaptureTarget::Apps { apps, .. } => Some(apps),
            _ => None,
        };
        if let Some(exe) = capture::foreground_exe() {
            // Asks for Discord's games list the first time, so it's there
            // by the next check.
            let clipped = apps.is_some_and(|apps| apps.iter().any(|a| a.id.eq_ignore_ascii_case(&exe))) || discord::known_game(&exe).is_some();
            if clipped {
                self.clipped_game = Some((exe, Instant::now()));
            }
        }
        // Not in focus for a while: is it still open?
        if let Some((exe, checked)) = &mut self.clipped_game
            && checked.elapsed() > Duration::from_secs(5)
        {
            if capture::list_windowed_apps().iter().any(|a| a.id.eq_ignore_ascii_case(exe)) {
                *checked = Instant::now();
            } else {
                self.clipped_game = None;
            }
        }
        let game = self.clipped_game.as_ref().and_then(|(exe, _)| {
            discord::known_game(exe).or_else(|| {
                let app = apps?.iter().find(|a| a.id.eq_ignore_ascii_case(exe))?;
                Some(discord::Game { name: app.name.clone(), icon: None })
            })
        });
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
        let text = if compact { "⬆" } else { "⬆  Update ready" };
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
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn permission_banner(&mut self, ui: &mut egui::Ui) {
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
        self.viewer = Some(viewer::Viewer::open(&self.ctx(), &clip));
        self.page = Page::View;
    }

    fn viewer_page(&mut self, ui: &mut egui::Ui) {
        let Some(v) = &mut self.viewer else {
            self.page = Page::Clips;
            return;
        };
        // Neighbours in the library's order (newest first), to step through.
        let at = self.clips.iter().position(|c| c.path == v.clip());
        let nav = viewer::Nav {
            previous: at.and_then(|i| i.checked_sub(1)).map(|i| self.clips[i].path.clone()),
            next: at.and_then(|i| self.clips.get(i + 1)).map(|c| c.path.clone()),
            position: at.map(|i| (i + 1, self.clips.len())),
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
                self.open_editor(clip);
            }
            viewer::ViewerOutcome::Open(clip) => self.viewer = Some(viewer::Viewer::open(&self.ctx(), &clip)),
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
            editor::EditorOutcome::Saved { target, info, edit, new_name } => {
                self.start_render(target, info, edit, new_name);
                self.close_editor();
            }
        }
    }

    fn close_editor(&mut self) {
        self.editor = None; // drops the player: stops audio and decoders
        self.page = Page::Clips;
        self.refresh_clips();
    }

    /// Render an edit in the background. `new_name`: save as a separate clip with
    /// that name instead of updating this clip's edit.
    fn start_render(&mut self, target: store::EditTarget, info: media::ClipInfo, edit: media::Edit, new_name: Option<String>) {
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
            Some(name) => match clips::path_for_name(target.library(), name, &ext, None) {
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
            let _ = tx.send((id, result));
            ctx.request_repaint();
        });
        self.refresh_clips();
    }

    /// Tell the user how background uploads ended.
    fn pump_uploads(&mut self) {
        for done in self.cloud.take_finished() {
            match done {
                cloud::UploadDone::Uploaded { clip, to } => {
                    self.toast(format!("Uploaded {} to {}", file_name(&clip), to.display()));
                }
                cloud::UploadDone::Failed { clip, error } => {
                    self.toast_error(format!("Couldn't upload {}: {error}", file_name(&clip)));
                }
                cloud::UploadDone::Cancelled { clip } => self.toast(format!("Upload of {} cancelled", file_name(&clip))),
            }
        }
    }

    fn pump_renders(&mut self) {
        while let Ok((id, result)) = self.render_rx.try_recv() {
            let Some(i) = self.renders.iter().position(|j| j.id == id) else { continue };
            let job = self.renders.remove(i);
            match result {
                Ok(path) => {
                    proxy::prebuild(&self.ctx(), &path);
                    // Highlight the card that changed: the new clip, or the edited one.
                    let card = if job.as_new { path } else { job.source };
                    self.last_saved = Some((card, Instant::now()));
                    if job.as_new {
                        self.toast(format!("Saved as new clip “{}”", file_stem(&job.dest)));
                    }
                }
                Err(e) => {
                    self.toast_error(format!("Couldn't save the edit: {e}"));
                }
            }
            self.refresh_clips();
        }
    }
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
                self.service.stop();
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
        // Optimistic state; a State/Error event confirms or corrects it.
        self.service.start(capture::Mode::ReplayBuffer, self.encode_settings());
        self.rec_state = RecState::Buffering;
        self.rec_started = Some(Instant::now());
    }

    fn start_recording(&mut self) {
        self.refresh_audio_devices();
        self.capturing_video = Some(self.video_source());
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
        self.service.save_clip();
        sound::play_saved(&self.settings.save_sound);
        self.saving += 1;
        self.page = Page::Clips;
    }

    fn stop(&mut self) {
        if self.rec_state == RecState::Recording {
            self.saving += 1; // finishing the file
        }
        self.service.stop();
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
                    self.toast(format!("Saved {}", file_name(&path)));
                    // Scrub frames ready before it's opened.
                    proxy::prebuild(&self.ctx(), &path);
                    self.last_saved = Some((path, Instant::now()));
                    self.refresh_clips();
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

/// Just the file name of a path, for status messages.
fn file_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}
