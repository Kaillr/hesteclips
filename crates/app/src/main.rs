//! HesteClips — a fast, no-nonsense clip recorder.
//!
//! The app opens on your clips. A slim capture bar on top always shows what's
//! happening (buffer armed, recording, idle) and holds the one or two buttons that
//! make sense right now; the global hotkeys do the same from inside a game.
//! Settings is a separate page you rarely visit, reached from the gear.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // no console window on release Windows

mod clips;
mod cloud;
mod cloud_ui;
mod editor;
mod library;
mod player;
mod proxy;
mod service;
mod settings;
mod settings_ui;
mod share;
mod sources_ui;
mod store;
mod thumbs;

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use egui::{Color32, RichText};
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use library::{ACCENT, REC_RED};
use service::{CaptureService, Evt};
use settings::{Encoder, RecordSettings, SourceKind};

fn main() -> eframe::Result<()> {
    // Killed from outside (Ctrl+C, SIGTERM, logout): stop capture and finish the
    // file before exiting, since destructors don't run on a signal.
    let _ = ctrlc::set_handler(|| {
        service::stop_all_and_wait(std::time::Duration::from_secs(10));
        std::process::exit(0);
    });

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1040.0, 700.0])
            .with_min_inner_size([560.0, 420.0])
            .with_title("HesteClips")
            .with_icon(app_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "HesteClips",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc.egui_ctx.clone())))),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Page {
    Clips,
    Sources,
    Settings,
    Edit,
}

/// What capture is currently doing. Replay buffer and manual recording are mutually
/// exclusive in this simple model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecState {
    Idle,
    Buffering,
    Recording,
}

/// Global hotkeys so you can control capture without leaving your game.
/// The manager must stay alive for the bindings to keep working.
struct Hotkeys {
    _mgr: GlobalHotKeyManager,
    buffer_id: u32,
    record_id: u32,
    save_id: u32,
}

impl Hotkeys {
    /// Alt+F8 = toggle replay buffer, Alt+F9 = toggle recording, Alt+F10 = save clip.
    fn setup() -> global_hotkey::Result<Self> {
        let mgr = GlobalHotKeyManager::new()?;
        let buffer = HotKey::new(Some(Modifiers::ALT), Code::F8);
        let record = HotKey::new(Some(Modifiers::ALT), Code::F9);
        let save = HotKey::new(Some(Modifiers::ALT), Code::F10);
        mgr.register(buffer)?;
        mgr.register(record)?;
        mgr.register(save)?;
        Ok(Self {
            _mgr: mgr,
            buffer_id: buffer.id(),
            record_id: record.id(),
            save_id: save.id(),
        })
    }
}

/// The app icon (`assets/icon.svg` rendered to PNG), shown in the Dock and taskbar
/// while running. Release bundles use `assets/hesteclips.icns` instead.
fn app_icon() -> egui::IconData {
    let img = image::load_from_memory_with_format(include_bytes!("../assets/icon-1024.png"), image::ImageFormat::Png)
        .expect("bundled icon is a valid PNG")
        .to_rgba8();
    egui::IconData { width: img.width(), height: img.height(), rgba: img.into_raw() }
}

/// "Option+F10" on macOS, "Alt+F10" elsewhere — the modifier's name on each OS.
/// (Spelled out because egui's bundled font has no ⌥ glyph.)
fn hotkey_label(key: &str) -> String {
    if cfg!(target_os = "macos") { format!("Option+{key}") } else { format!("Alt+{key}") }
}

/// An in-app shortcut with the platform's command key, e.g. "⌘A" / "Ctrl+A".
fn hotkey_label_cmd(key: &str) -> String {
    if cfg!(target_os = "macos") { format!("⌘{key}") } else { format!("Ctrl+{key}") }
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
    saving: bool,
    settings: RecordSettings,
    /// Last-saved settings JSON, to persist only when something changed.
    saved_settings: String,
    service: CaptureService,
    hotkeys: Option<Hotkeys>,
    hotkey_error: Option<String>,
    /// Screen-recording permission, re-checked each poll so the banner clears the
    /// moment the user grants it.
    permission: capture::Permission,
    /// Screens detected by the capture backend.
    screens: Vec<capture::Device>,
    /// System audio inputs/outputs (+ current OS defaults).
    audio: capture::audio::AudioDevices,
    /// Live per-source volume and meters, shared with the capture thread.
    live_audio: std::sync::Arc<capture::mixer::LiveAudio>,
    /// Meters-only capture while the Sources page is open and nothing records,
    /// with the sources it was started for.
    level_monitor: Option<(Vec<capture::sources::AudioSource>, capture::sources::LevelMonitor)>,
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
}

impl App {
    fn new(ctx: egui::Context) -> Self {
        let settings = RecordSettings::load();
        let recovered = capture::output::recover_unfinished(&settings.output_dir);
        let saved_settings = settings.to_json();
        let clips = clips::scan(&settings.output_dir);
        // Assets of clips deleted in Finder go to the Bin.
        store::sweep_orphans(&settings.output_dir, &clips);
        let (hotkeys, hotkey_error) = match Hotkeys::setup() {
            Ok(h) => (Some(h), None),
            Err(e) => (None, Some(e.to_string())),
        };
        let (render_tx, render_rx) = std::sync::mpsc::channel();
        let live_audio = capture::mixer::LiveAudio::new();
        live_audio.set_limiter(settings.limiter);
        for s in &settings.audio_sources {
            live_audio.channel(&s.id).set_volume(sources_ui::from_db(s.volume_db), s.muted);
        }
        let mut app = Self {
            page: Page::Clips,
            rec_state: RecState::Idle,
            rec_started: None,
            saving: false,
            settings,
            saved_settings,
            service: CaptureService::new(live_audio.clone()),
            hotkeys,
            hotkey_error,
            permission: capture::screen_permission(),
            screens: capture::list_screens(),
            audio: capture::audio::list_audio_devices(),
            live_audio,
            level_monitor: None,
            sources_view: Default::default(),
            clips,
            thumbs: thumbs::Thumbs::new(ctx.clone()),
            last_saved: None,
            cloud: cloud::Cloud::new(ctx),
            dialog: None,
            toast: None,
            editor: None,
            renders: Vec::new(),
            render_tx,
            render_rx,
            next_render_id: 0,
            rename: None,
            selection: library::Selection::default(),
            last_poll: None,
            dir_mtime: None,
        };
        // Dev aid: `HESTECLIPS_OPEN_EDITOR=<clip>` opens the editor at launch, so the
        // editor can be checked without clicking through the library.
        if let Some(clip) = std::env::var_os("HESTECLIPS_OPEN_EDITOR") {
            app.settings.auto_start_buffer = false;
            app.open_editor(PathBuf::from(clip));
        }
        // Dev aid: `HESTECLIPS_DEMO_RENDER=<clip>` shows the library with a save
        // in progress (that clip's edit + a "save as new"), to check the progress UI.
        // Point HESTECLIPS_LIBRARY at a scratch folder holding a copy of the clip.
        if let Some(dir) = std::env::var_os("HESTECLIPS_LIBRARY") {
            app.settings.auto_start_buffer = false;
            app.settings.output_dir = PathBuf::from(dir);
            app.saved_settings = app.settings.to_json(); // never persist the scratch dir
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
        // `HESTECLIPS_OPEN_SOURCES=1` opens on the Sources page.
        let open_sources = std::env::var_os("HESTECLIPS_OPEN_SOURCES").is_some();
        if open_sources {
            app.page = Page::Sources;
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
        let screen_id = self
            .screens
            .get(self.settings.display_index)
            .map(|d| d.id.clone())
            .unwrap_or_else(|| "0".to_owned());
        capture::EncodeSettings {
            output_dir: self.settings.output_dir.clone(),
            container_ext: self.settings.container.ext().to_owned(),
            fps: self.settings.fps,
            video_bitrate_kbps: self.settings.video_bitrate_mbps * 1000,
            target_height: self.settings.resolution.height(),
            keyframe_interval_secs: self.settings.keyframe_interval_secs,
            use_hardware: self.settings.encoder != Encoder::Software,
            replay_seconds: self.settings.replay_seconds,
            screen_id,
            sources: self.capture_sources(),
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

        // Handle global hotkeys — these fire even while a game is focused.
        let ids = self.hotkeys.as_ref().map(|h| (h.buffer_id, h.record_id, h.save_id));
        if let Some((buffer_id, record_id, save_id)) = ids {
            while let Ok(ev) = GlobalHotKeyEvent::receiver().try_recv() {
                if ev.state != HotKeyState::Pressed {
                    continue;
                }
                if ev.id == buffer_id {
                    self.toggle_buffer();
                } else if ev.id == record_id {
                    self.toggle_record();
                } else if ev.id == save_id {
                    self.save_clip();
                }
            }
        }

        self.pump_capture_events();
        self.cloud.poll();
        self.pump_renders();

        // The editor gets the whole window; capture keeps running underneath and the
        // hotkeys still work.
        if self.page != Page::Edit {
            egui::Panel::top("capture_bar")
                .frame(
                    egui::Frame::new()
                        .fill(ui.visuals().panel_fill)
                        .inner_margin(egui::Margin::symmetric(16, 10)),
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

        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin::symmetric(16, 4)))
            .show(ui, |ui| match self.page {
                Page::Clips => self.library(ui, frame),
                Page::Sources => self.sources_page(ui),
                Page::Settings => self.settings_page(ui),
                Page::Edit => self.editor_page(ui),
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
            // Persist settings as they change; no Save button to forget.
            let json = self.settings.to_json();
            if json != self.saved_settings {
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
}

// --- Capture bar: status on the left, the actions that make sense right now on the right ---
impl App {
    fn capture_bar(&mut self, ui: &mut egui::Ui) {
        let elapsed = self.rec_started.map_or(Duration::ZERO, |t| t.elapsed());
        ui.horizontal(|ui| {
            ui.set_min_height(36.0);

            if matches!(self.page, Page::Settings | Page::Sources) {
                if ui.button(RichText::new("‹ Clips").size(15.0)).clicked() {
                    self.page = Page::Clips;
                }
                ui.add_space(6.0);
                let title = if self.page == Page::Settings { "Settings" } else { "Sources" };
                ui.label(RichText::new(title).size(18.0).strong());
            } else {
                let (filled, color, title, sub) = match self.rec_state {
                    RecState::Idle => (
                        false,
                        ui.visuals().weak_text_color(),
                        "Not recording".to_owned(),
                        "Start the replay buffer to be ready for the next highlight".to_owned(),
                    ),
                    RecState::Buffering => (
                        true,
                        ACCENT,
                        "Replay buffer on".to_owned(),
                        format!(
                            "{} saves the last {}",
                            hotkey_label("F10"),
                            thumbs::format_duration(Duration::from_secs(self.settings.replay_seconds.into()))
                        ),
                    ),
                    RecState::Recording => (
                        true,
                        REC_RED,
                        format!("Recording  {}", thumbs::format_duration(elapsed)),
                        format!("{} to stop", hotkey_label("F9")),
                    ),
                };
                // Painted rather than a text glyph so it renders identically everywhere.
                let (rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
                if filled {
                    // Live: a soft pulse so it reads as "on" at a glance.
                    let t = ui.input(|i| i.time);
                    let halo = 0.25 + 0.2 * (t * 2.5).sin() as f32;
                    ui.painter().circle_filled(rect.center(), 8.0, color.gamma_multiply(halo));
                    ui.painter().circle_filled(rect.center(), 5.0, color);
                } else {
                    ui.painter().circle_stroke(rect.center(), 5.0, egui::Stroke::new(1.5, color));
                }
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.label(RichText::new(title).size(16.0).strong());
                    ui.label(RichText::new(sub).size(12.0).weak());
                });
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.page == Page::Clips {
                    if ui
                        .add(egui::Button::new(RichText::new("⚙").size(18.0)).frame(false))
                        .on_hover_text("Settings")
                        .clicked()
                    {
                        self.page = Page::Settings;
                    }
                    if ui
                        .add(egui::Button::new(RichText::new("🎤 Sources").size(14.0)).frame(false))
                        .on_hover_text("What goes into your clips: mic, desktop sound, apps — with live levels")
                        .clicked()
                    {
                        self.page = Page::Sources;
                    }
                    ui.add_space(8.0);
                }
                let big = |text: &str, fill: Option<Color32>| {
                    let mut rt = RichText::new(text.to_owned()).size(15.0);
                    if fill.is_some() {
                        rt = rt.color(Color32::WHITE);
                    }
                    let mut b = egui::Button::new(rt).min_size(egui::vec2(0.0, 34.0)).corner_radius(8);
                    if let Some(f) = fill {
                        b = b.fill(f);
                    }
                    b
                };
                // Buttons are laid out right-to-left: primary action is rightmost.
                match self.rec_state {
                    RecState::Idle => {
                        if ui
                            .add(big("⏺  Start replay buffer", Some(ACCENT)))
                            .on_hover_text(hotkey_label("F8"))
                            .clicked()
                        {
                            self.start_replay_buffer();
                        }
                        if ui.add(big("Record", None)).on_hover_text(hotkey_label("F9")).clicked() {
                            self.start_recording();
                        }
                    }
                    RecState::Buffering => {
                        if ui
                            .add_enabled(!self.saving, big("💾  Save clip", Some(ACCENT)))
                            .on_hover_text(hotkey_label("F10"))
                            .clicked()
                        {
                            self.save_clip();
                        }
                        if ui.add(big("Stop", None)).on_hover_text(hotkey_label("F8")).clicked() {
                            self.stop();
                        }
                    }
                    RecState::Recording => {
                        if ui
                            .add(big("⏹  Stop recording", Some(REC_RED)))
                            .on_hover_text(hotkey_label("F9"))
                            .clicked()
                        {
                            self.stop();
                        }
                    }
                }
            });
        });
    }

    /// Warn + guide when screen-recording permission is missing (macOS). Without it,
    /// capture produces empty files, so we surface it up front instead of failing later.
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
        self.editor = Some(editor::Editor::open(&self.ctx(), &clip));
        self.page = Page::Edit;
    }

    fn ctx(&self) -> egui::Context {
        self.thumbs.ctx()
    }

    fn editor_page(&mut self, ui: &mut egui::Ui) {
        let Some(ed) = &mut self.editor else {
            self.page = Page::Clips;
            return;
        };
        match ed.ui(ui) {
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
                if let Err(e) = std::fs::create_dir_all(staged.parent().unwrap_or(target.library())) {
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

    fn pump_renders(&mut self) {
        while let Ok((id, result)) = self.render_rx.try_recv() {
            let Some(i) = self.renders.iter().position(|j| j.id == id) else { continue };
            let job = self.renders.remove(i);
            match result {
                Ok(path) => {
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
        // Optimistic state; a State/Error event confirms or corrects it.
        self.service.start(capture::Mode::ReplayBuffer, self.encode_settings());
        self.rec_state = RecState::Buffering;
        self.rec_started = Some(Instant::now());
    }

    fn start_recording(&mut self) {
        self.refresh_audio_devices();
        self.service.start(capture::Mode::Record, self.encode_settings());
        self.rec_state = RecState::Recording;
        self.rec_started = Some(Instant::now());
        self.page = Page::Clips;
    }

    fn save_clip(&mut self) {
        if self.rec_state != RecState::Buffering || self.saving {
            return;
        }
        // Runs on the capture thread; the Saved event lands the clip in the library.
        self.service.save_clip();
        self.saving = true;
        self.page = Page::Clips;
    }

    fn stop(&mut self) {
        if self.rec_state == RecState::Recording {
            self.saving = true; // finalizing the file
        }
        self.service.stop();
        self.rec_state = RecState::Idle;
        self.rec_started = None;
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
                        self.saving = false;
                        self.refresh_clips();
                    }
                }
                Evt::Saved(path) => {
                    self.saving = false;
                    self.toast(format!("Saved {}", file_name(&path)));
                    self.last_saved = Some((path, Instant::now()));
                    self.refresh_clips();
                }
                Evt::Error(e) => {
                    // A start failure is followed by Evt::State(None); a save failure
                    // leaves us still buffering. So just surface the message.
                    self.saving = false;
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
