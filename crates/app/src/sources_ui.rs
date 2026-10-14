//! The Sources page: what goes into your clips. Today that's audio — your mic,
//! everything the computer plays, or single apps — each with a live meter and a
//! volume fader, plus the clip's mix meter. Video sources (webcam, overlays)
//! will live here too.
//!
//! Deliberately says nothing about "tracks": a source is either part of what the
//! clip sounds like ("In the clip"), kept separately for editing, or both.

use std::time::{Duration, Instant};

use capture::mixer::{Channel, SourceStatus};
use egui::{Color32, Sense};

use crate::App;
use crate::meter::{self, MeterState};
use crate::settings::{AudioSourceCfg, CaptureApp, CaptureTarget, SourceKind};

/// Fader range in dB.
const FADER_MIN_DB: f32 = -60.0;
const FADER_MAX_DB: f32 = 12.0;
/// How long the "limiting" note stays after the limiter last worked.
const LIMIT_HOLD: Duration = Duration::from_secs(3);
/// One column never gets wider than this.
const MAX_WIDTH: f32 = 680.0;
/// Picture and sound side by side from this page width.
const TWO_COLUMNS: f32 = 860.0;

/// Meters for one source: what it delivers (before volume) and what goes into
/// the clip (after volume and mute).
#[derive(Default, Clone)]
struct SourceMeters {
    input: MeterState,
    output: MeterState,
}

/// UI state for the Sources page.
#[derive(Default)]
pub(crate) struct SourcesView {
    meters: std::collections::HashMap<String, SourceMeters>,
    master: MeterState,
    /// Limiter gain reduction shown, dB (falls smoothly), and when it last worked.
    reduction_db: f32,
    limiting_at: Option<Instant>,
    last_frame: Option<Instant>,
    /// Running apps for the "Add app" menu, refreshed when it opens.
    apps: Vec<capture::Device>,
    /// The preview picture, and which frame it shows.
    preview: Option<(egui::TextureHandle, u64)>,
    /// macOS: the preview frame drawn straight from the GPU, and the frames
    /// opened for it (the capture's buffers, each opened once).
    #[cfg(target_os = "macos")]
    preview_gpu: Option<(crate::gpu_frames::Shown, u64)>,
    #[cfg(target_os = "macos")]
    preview_frames: crate::gpu_frames::Frames,
    /// Preview frames shown in the current second, when it started, and the
    /// rate over the last full second.
    preview_rate: (u32, Option<Instant>, u32),
    /// When the open apps were last checked, for the games-and-apps statuses.
    apps_checked: Option<Instant>,
    renaming: Option<String>,
}


impl App {
    pub(crate) fn sources_page(&mut self, ui: &mut egui::Ui) {
        let now = Instant::now();
        // Opening the page: start the meters from silence. Their last state is
        // stale, and the peak held since the last read could be from minutes ago
        // (meters keep collecting while a recording runs).
        if self.sources_view.last_frame.is_none_or(|t| now - t > Duration::from_millis(500)) {
            self.sources_view.meters.clear();
            self.sources_view.master = MeterState::default();
            for s in &self.settings.audio_sources {
                let ch = self.live_audio.channel(&s.id);
                ch.meter.take();
                ch.input.take();
            }
            self.live_audio.master.take();
            self.live_audio.take_reduction_db();
        }
        let dt = self.sources_view.last_frame.map_or(0.0, |t| (now - t).as_secs_f32()).min(0.2);
        self.sources_view.last_frame = Some(now);
        // Meters and the preview move: keep repainting while this page is open,
        // twice per frame of the preview (the recording's frame rate: once
        // per frame missed some, the two not lining up), else about 60 times
        // a second.
        let every = if capture::preview::AVAILABLE { Duration::from_secs_f64(0.5 / self.settings.fps.max(1) as f64) } else { Duration::from_millis(16) };
        crate::ui_kit::repaint_after(ui.ctx(), every);

        self.page_head(ui, "Sources", |_, _| {});
        crate::ui_kit::arrive(ui, |ui| crate::ui_kit::scroll(ui, "sources_page", |ui| {
            let w = ui.available_width();
            // Picture on the left, sound on the right, when there's room;
            // one above the other when there isn't.
            if w >= TWO_COLUMNS && capture::preview::AVAILABLE {
                let col = (w - 28.0) / 2.0;
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 28.0;
                    ui.allocate_ui_with_layout(egui::vec2(col, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                        ui.set_width(col);
                        self.picture_column(ui);
                    });
                    ui.allocate_ui_with_layout(egui::vec2(col, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                        ui.set_width(col);
                        self.sound_column(ui, now, dt);
                    });
                });
            } else {
                let w = w.min(MAX_WIDTH);
                ui.vertical(|ui| {
                    ui.set_width(w);
                    if capture::preview::AVAILABLE {
                        self.picture_column(ui);
                        ui.add_space(24.0);
                    }
                    self.sound_column(ui, now, dt);
                });
            }
            ui.add_space(28.0);
        }));
        self.live_audio.set_limiter(self.settings.limiter);
    }

    /// What you see: the whole screen or your games and apps, its preview,
    /// and the webcam.
    fn picture_column(&mut self, ui: &mut egui::Ui) {
        self.video_card(ui);
        if capture::webcam::AVAILABLE {
            ui.add_space(22.0);
            self.webcam_card(ui);
        }
    }

    /// What you hear: the clip's mix, then each source, then adding one.
    fn sound_column(&mut self, ui: &mut egui::Ui, now: Instant, dt: f32) {
        use crate::ui_kit::Btn;
        let pal = crate::theme::pal(ui);
        column_head(ui, "Sound", |ui| {
            if Btn::ghost("Refresh").with_icon(egui_phosphor::regular::ARROWS_CLOCKWISE).small().tip("Look again for microphones you've plugged in").show(ui).clicked() {
                self.refresh_audio_devices();
                self.toast_icon(egui_phosphor::regular::ARROWS_CLOCKWISE, "Looked for microphones again");
            }
        });
        // The mix first: it's what people hear, and where it gets too loud.
        self.master_strip(ui, now, dt);
        ui.add_space(14.0);

        let mut remove = None;
        let mut move_up = None;
        let n = self.settings.audio_sources.len();
        let has_app_sources = self.settings.audio_sources.iter().any(|s| matches!(s.kind, SourceKind::App { .. }));
        let locked = (self.rec_state != crate::RecState::Idle).then(|| self.stop_first());
        for i in 0..n {
            let channel = self.live_audio.channel(&self.settings.audio_sources[i].id);
            let id = self.settings.audio_sources[i].id.clone();
            let id_for_listen = id.clone();
            let meters = self.sources_view.meters.entry(id).or_default();
            meters.input.update(channel.input.take(), now, dt);
            meters.output.update(channel.meter.take(), now, dt);
            let listening = self.listening.as_ref().filter(|l| l.source == id_for_listen).map(|l| l.monitor.latency_ms());
            let action = source_card(ui, &mut self.settings.audio_sources[i], &channel, meters, &self.audio, has_app_sources, &mut self.sources_view.renaming, listening, locked, i == 0);
            match action {
                CardAction::Remove => remove = Some(i),
                CardAction::MoveUp if i > 0 => move_up = Some(i),
                CardAction::Listen => self.toggle_listening(i),
                CardAction::Locked => self.toast_icon(egui_phosphor::regular::LOCK_SIMPLE, locked.unwrap_or_default().trim_end_matches('.')),
                CardAction::NoiseRemoval(on) => self.toast_icon(egui_phosphor::regular::MAGIC_WAND, if on { "Background noise removal on" } else { "Background noise removal off" }),
                _ => {}
            }
            // Volume applies live, even mid-recording.
            let s = &self.settings.audio_sources[i];
            channel.set_volume(from_db(s.volume_db), s.muted);
            channel.set_denoise(s.noise_removal);
            ui.add_space(10.0);
        }
        if let Some(i) = remove {
            let name = self.settings.audio_sources.remove(i).name;
            self.toast_icon(egui_phosphor::regular::TRASH, format!("Removed {name}"));
        }
        if let Some(i) = move_up {
            self.settings.audio_sources.swap(i, i - 1);
        }

        self.keep_listening();
        self.add_source_buttons(ui);

        if self.rec_state != crate::RecState::Idle {
            ui.add_space(12.0);
            let mic = if cfg!(target_os = "linux") { ", noise removal" } else { ", noise removal and which microphone" };
            let stop = if self.rec_state == crate::RecState::Recording { "Stop recording" } else { "Stop clipping" };
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(crate::theme::ic(egui_phosphor::regular::LOCK_SIMPLE, 12.0).color(pal.text3));
                ui.add(egui::Label::new(crate::theme::text(format!("Volume, mute{mic} change right away. {stop} to add or remove sources."), 12.5, crate::theme::W::Regular).color(pal.text3)).wrap());
            });
        }
    }

    /// "Listen" on source `i`: hear it live, or stop.
    fn toggle_listening(&mut self, i: usize) {
        let source = &self.settings.audio_sources[i];
        if self.listening.as_ref().is_some_and(|l| l.source == source.id) {
            self.listening = None;
            return;
        }
        self.listening = None;
        if let Some(err) = self.start_listening(i) {
            self.toast_error(format!("Couldn't play the microphone: {err}"));
        }
    }

    fn start_listening(&mut self, i: usize) -> Option<String> {
        let source = &self.settings.audio_sources[i];
        let SourceKind::Microphone { device } = &source.kind else { return None };
        let Some(device) = self.audio.resolve_input(device) else { return Some("no microphone".into()) };
        match capture::monitor::MicMonitor::start(&device, self.live_audio.channel(&source.id)) {
            Ok(monitor) => {
                self.listening = Some(Listening { source: source.id.clone(), device, monitor });
                None
            }
            Err(e) => Some(e.to_string()),
        }
    }

    /// Stop listening to a microphone that's gone, off or removed; follow it
    /// to another device.
    fn keep_listening(&mut self) {
        let Some(l) = &self.listening else { return };
        let found = self.settings.audio_sources.iter().position(|s| s.id == l.source && s.enabled);
        let Some(i) = found else {
            self.listening = None;
            return;
        };
        let SourceKind::Microphone { device } = &self.settings.audio_sources[i].kind else { return };
        if self.audio.resolve_input(device).as_deref() != Some(l.device.as_str()) {
            self.listening = None;
            if let Some(err) = self.start_listening(i) {
                self.toast_error(format!("Couldn't play the microphone: {err}"));
            }
        }
    }

    fn add_source_buttons(&mut self, ui: &mut egui::Ui) {
        use crate::ui_kit::{Btn, Item};
        use egui_phosphor::regular as icon;
        // Which sources are recorded is set when capture starts.
        let idle = self.rec_state == crate::RecState::Idle;
        let stop_first = self.stop_first();
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let mic = Btn::normal("Microphone").with_icon(icon::PLUS).small().tip(if idle { "Add a microphone or audio interface" } else { stop_first }).show(ui);
            if mic.clicked() {
                if idle {
                    let n = self.settings.audio_sources.iter().filter(|s| matches!(s.kind, SourceKind::Microphone { .. })).count();
                    let name = if n == 0 { "Microphone".to_owned() } else { format!("Microphone {}", n + 1) };
                    self.settings.audio_sources.push(AudioSourceCfg::new(&name, SourceKind::Microphone { device: capture::audio::DEFAULT_DEVICE.into() }));
                } else {
                    self.toast_icon(icon::LOCK_SIMPLE, stop_first.trim_end_matches('.'));
                }
            }
            let app = Btn::normal("App").with_icon(icon::PLUS).trail(icon::CARET_DOWN).small().id("add_app_source").tip(if idle { "Record one app on its own: a game, Discord, music" } else { stop_first }).show(ui);
            if app.clicked() {
                if idle {
                    self.sources_view.apps = capture::list_apps();
                } else {
                    self.toast_icon(icon::LOCK_SIMPLE, stop_first.trim_end_matches('.'));
                    egui::Popup::close_id(ui.ctx(), app.id.with("menu"));
                }
            }
            if idle {
                crate::ui_kit::menu_popup(&app, true, 260.0, |ui| {
                    crate::ui_kit::menu_head(ui, "Record one app on its own");
                    let taken: Vec<String> = self.settings.audio_sources.iter().filter_map(|s| match &s.kind {
                        SourceKind::App { bundle_id, .. } => Some(bundle_id.clone()),
                        _ => None,
                    }).collect();
                    egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                        for app in self.sources_view.apps.clone() {
                            let added = taken.contains(&app.id);
                            let item = Item::new(app.name.clone()).enabled(!added);
                            let item = if added { item.tip("Already added") } else { item };
                            if item.show(ui).clicked() {
                                self.settings.audio_sources.push(AudioSourceCfg::new(&app.name, SourceKind::App { bundle_id: app.id.clone(), app_name: app.name.clone() }));
                            }
                        }
                    });
                    if self.sources_view.apps.is_empty() {
                        // PipeWire only knows apps by their sound.
                        crate::ui_kit::menu_head(ui, if cfg!(target_os = "linux") { "No apps are playing sound. Start one, then look again." } else { "No apps found." });
                    }
                });
            }
            let has_desktop = self.settings.audio_sources.iter().any(|s| matches!(s.kind, SourceKind::Desktop { .. }));
            if !has_desktop && Btn::normal("Desktop sound").with_icon(icon::PLUS).small().show(ui).clicked() {
                if idle {
                    self.settings.audio_sources.push(AudioSourceCfg::new("Desktop sound", SourceKind::Desktop { exclude_apps: true }));
                } else {
                    self.toast_icon(icon::LOCK_SIMPLE, stop_first.trim_end_matches('.'));
                }
            }
        });
    }

    /// Why something that's set when capture starts is locked now.
    pub(crate) fn stop_first(&self) -> &'static str {
        match self.rec_state {
            crate::RecState::Recording => "Stop recording to change this.",
            _ => "Stop clipping to change this.",
        }
    }

    /// The clip's mix: a big stereo meter with scale, peak readout, clip light,
    /// and the limiter with its gain-reduction meter.
    /// The video source: what to record (the whole screen, or games and apps
    /// following focus), with a live preview of exactly what gets recorded.
    fn video_card(&mut self, ui: &mut egui::Ui) {
        capture::preview::request();
        let frame = capture::preview::latest();
        self.update_preview_texture(ui.ctx(), frame.as_deref());
        // Which listed apps are open, for their status; cheap, so every 2 s.
        let apps_mode = matches!(self.settings.capture, CaptureTarget::Apps { .. });
        if apps_mode && self.sources_view.apps_checked.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
            self.sources_view.apps_checked = Some(Instant::now());
            self.windowed_apps = capture::list_windowed_apps();
        }
        let idle = self.rec_state == crate::RecState::Idle;
        let pal = crate::theme::pal(ui);
        let size_text = frame.as_ref().map(|f| format!("{}×{} at {} fps", f.recorded.0, f.recorded.1, self.settings.fps));
        column_head(ui, "Picture", |ui| {
            if let Some(t) = &size_text {
                ui.label(crate::theme::text(t, 12.5, crate::theme::W::Regular).color(pal.text3))
                    .on_hover_text(format!("What's recorded. Change it in Settings, Video. The preview shows {} frames a second.", self.sources_view.preview_rate.2));
            }
        });

        // What to record: changes right away, while capturing too (the clip
        // switches mid-way; nothing restarts). The display too, except on
        // Linux, where the desktop picks it when capture starts.
        let stop_first = self.stop_first();
        let display_locked = !idle && cfg!(target_os = "linux");
        if capture::APP_CAPTURE {
            let opts = [
                crate::ui_kit::Seg { icon: Some(egui_phosphor::regular::MONITOR), label: "Whole screen", tip: Some("Everything on one display") },
                crate::ui_kit::Seg { icon: Some(egui_phosphor::regular::APP_WINDOW), label: "Games and apps", tip: Some("Only the games and apps you pick, following whichever you're using") },
            ];
            let w = ui.available_width();
            if let Some(i) = crate::ui_kit::segmented_opts(ui, "capture_kind", &opts, usize::from(apps_mode), Some(w), true) {
                if i == 0 && apps_mode {
                    let apps = std::mem::replace(&mut self.settings.capture, CaptureTarget::Screen);
                    if matches!(apps, CaptureTarget::Apps { .. }) {
                        self.settings.idle_apps = Some(apps);
                    }
                } else if i == 1 && !apps_mode {
                    self.settings.capture = self.settings.idle_apps.take().unwrap_or(CaptureTarget::Apps { apps: Vec::new(), away_screen: true });
                    self.windowed_apps = capture::list_windowed_apps();
                }
            }
            ui.add_space(12.0);
        }

        self.preview_picture(ui, frame.as_deref());
        ui.add_space(12.0);

        match self.settings.capture.clone() {
            CaptureTarget::Apps { apps, away_screen } if capture::APP_CAPTURE => self.app_list(ui, apps, away_screen, frame.as_deref()),
            // Linux on Wayland: the desktop's own dialog picks the screen (once; it's remembered).
            _ if cfg!(target_os = "linux") && !capture::screens_listed() => {
                group(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(crate::theme::text("Screen", 13.5, crate::theme::W::Regular).color(pal.text));
                        ui.label(crate::theme::text("picked in your desktop's sharing dialog", 12.5, crate::theme::W::Regular).color(pal.text3));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let b = crate::ui_kit::Btn::normal("Choose another…").small().enabled(idle).tip(if idle { "Your desktop asks again which screen to record" } else { "Stop capturing to switch screens" });
                            if b.show(ui).clicked() {
                                capture::choose_screen_again();
                                self.video_preview = None; // restarts, and asks
                            }
                        });
                    });
                });
            }
            _ => {
                group(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(crate::theme::text("Display", 13.5, crate::theme::W::Medium).color(pal.text));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let names: Vec<String> = self.screens.iter().map(|d| d.name.clone()).collect();
                            let current = names.get(self.settings.display_index).cloned().unwrap_or_else(|| "Main display".to_owned());
                            let r = crate::ui_kit::select(ui, &current, 180.0).on_hover_text(if display_locked { stop_first } else { "" });
                            if display_locked && r.clicked() {
                                egui::Popup::close_id(ui.ctx(), r.id.with("menu"));
                                self.toast_icon(egui_phosphor::regular::LOCK_SIMPLE, stop_first.trim_end_matches('.'));
                            } else if !display_locked {
                                crate::ui_kit::menu_popup(&r, false, 240.0, |ui| {
                                    for (i, name) in names.iter().enumerate() {
                                        if crate::ui_kit::Item::new(name.clone()).check(self.settings.display_index == i).show(ui).clicked() {
                                            self.settings.display_index = i;
                                        }
                                    }
                                });
                            }
                        });
                    });
                });
            }
        }
    }

    fn update_preview_texture(&mut self, ctx: &egui::Context, frame: Option<&capture::preview::PreviewFrame>) {
        let Some(f) = frame else { return };
        if self.sources_view.preview.as_ref().is_some_and(|(_, seq)| *seq == f.seq) {
            return;
        }
        #[cfg(target_os = "macos")]
        if self.sources_view.preview_gpu.as_ref().is_some_and(|(_, seq)| *seq == f.seq) {
            return;
        }
        self.count_preview_frame();
        // macOS: the frame itself, on the GPU, drawn as it is.
        #[cfg(target_os = "macos")]
        if let Some(s) = &f.surface {
            if let Some(shown) = self.sources_view.preview_frames.show(s, f.width, f.height) {
                self.sources_view.preview_gpu = Some((shown, f.seq));
            }
            return;
        }
        if f.rgba.is_empty() {
            return;
        }
        // Opaque, so it's already premultiplied: one copy, no per-pixel work.
        let pixels: Vec<egui::Color32> = bytemuck::cast_slice(&f.rgba).to_vec();
        let image = egui::ColorImage::new([f.width as usize, f.height as usize], pixels);
        match &mut self.sources_view.preview {
            Some((tex, seq)) => {
                tex.set(image, egui::TextureOptions::LINEAR);
                *seq = f.seq;
            }
            None => self.sources_view.preview = Some((ctx.load_texture("capture_preview", image, egui::TextureOptions::LINEAR), f.seq)),
        }
    }

    /// Count a new preview frame, for the rate shown.
    fn count_preview_frame(&mut self) {
        let (count, since, rate) = &mut self.sources_view.preview_rate;
        *count += 1;
        match since {
            Some(t) if t.elapsed() >= Duration::from_secs(1) => {
                *rate = *count;
                *count = 0;
                *since = Some(Instant::now());
            }
            None => *since = Some(Instant::now()),
            _ => {}
        }
    }

    /// The preview itself: as wide as the card, never taller than a third of
    /// the window so the rest stays in view.
    fn preview_picture(&mut self, ui: &mut egui::Ui, frame: Option<&capture::preview::PreviewFrame>) {
        let aspect = frame.map_or(16.0 / 10.0, |f| f.width as f32 / f.height.max(1) as f32);
        let max_h = (ui.ctx().content_rect().height() * 0.45).max(140.0);
        let w = ui.available_width().min(max_h * aspect);
        ui.vertical_centered(|ui| {
            let (rect, response) = ui.allocate_exact_size(egui::vec2(w, w / aspect), Sense::click_and_drag());
            let p = ui.painter();
            p.rect_filled(rect, 6.0, crate::theme::pal(ui).stage);
            let message = |text: &str| {
                p.text(rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(13.0), Color32::from_gray(170));
            };
            #[cfg(target_os = "macos")]
            let gpu = self.sources_view.preview_gpu.as_ref().map(|(s, _)| s.clone());
            #[cfg(not(target_os = "macos"))]
            let gpu: Option<()> = None;
            let drawn = match (&gpu, &self.sources_view.preview) {
                #[cfg(target_os = "macos")]
                (Some(shown), _) => {
                    shown.paint(ui, rect);
                    true
                }
                (_, Some((tex, _))) => {
                    p.image(tex.id(), rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
                    true
                }
                _ => false,
            };
            match (drawn, frame) {
                (true, Some(_)) => {}
                _ => match self.video_preview.as_ref().and_then(|(_, p)| p.error()) {
                    Some(e) => message(&format!("No preview: {e}")),
                    None => message("Starting preview…"),
                },
            }
            if self.settings.webcam.is_some() && frame.is_some() {
                self.webcam_on_preview(ui, rect, &response);
            }
        });
    }

    /// The games and apps to record: a framed list, each with whether it's
    /// open (and which is being recorded) and a remove button, then a way to
    /// add more. Right-clicking one has its game capture option. Changes apply
    /// right away, even while capturing.
    fn app_list(&mut self, ui: &mut egui::Ui, apps: Vec<CaptureApp>, away_screen: bool, frame: Option<&capture::preview::PreviewFrame>) {
        use crate::theme::{self, W};
        use crate::ui_kit::{Btn, Item, Kind};
        use egui_phosphor::regular as icon;
        let pal = theme::pal(ui);
        let showing = frame.filter(|f| !f.waiting).and_then(|f| f.app.clone());
        let capturing = self.rec_state != crate::RecState::Idle;
        let mut list = apps.clone();
        let mut away = away_screen;
        let mut remove = None;
        group(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            if list.is_empty() {
                ui.add_space(4.0);
                ui.label(theme::text("No games or apps yet. Add the games you play.", 13.0, W::Regular).color(pal.text3));
                ui.add_space(8.0);
            }
            for (i, app) in list.iter().enumerate() {
                let open = self.windowed_apps.iter().any(|a| a.id.eq_ignore_ascii_case(&app.id));
                let active = showing.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(&app.id));
                let (dot, status) = if active {
                    (Some(pal.live), if capturing { "Recording now" } else { "In the preview" })
                } else if open {
                    (None, "Open")
                } else {
                    (None, "Not open")
                };
                ui.horizontal(|ui| {
                    ui.set_min_height(42.0);
                    ui.spacing_mut().item_spacing.x = 10.0;
                    // The game's picture, when it's one Discord knows.
                    let (lead, _) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), Sense::hover());
                    let tex = crate::discord::game_for_folder(&crate::clips::folder_name(&app.name)).and_then(|g| g.icon).and_then(|url| self.web_images.icon(ui.ctx(), &url));
                    match tex {
                        Some(t) => {
                            egui::Image::from_texture((t.id(), lead.size())).corner_radius(4).paint_at(ui, lead);
                        }
                        None => {
                            ui.painter().text(lead.center(), egui::Align2::CENTER_CENTER, icon::APP_WINDOW, egui::FontId::proportional(16.0), pal.text3);
                        }
                    }
                    ui.label(theme::text(&app.name, 13.5, W::Medium).color(pal.text)).on_hover_text(&app.id);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        if Btn::icon(Kind::Ghost, icon::X).small().tip(format!("Stop recording {}", app.name)).show(ui).clicked() {
                            remove = Some(i);
                        }
                        ui.label(theme::text(status, 12.5, W::Regular).color(pal.text3));
                        if let Some(c) = dot {
                            let (r, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), Sense::hover());
                            crate::shell::status_dot(ui.painter(), r.center(), c, ui.input(|i| i.time));
                        }
                    });
                });
                crate::ui_kit::divider(ui, 0.0);
            }
            ui.add_space(6.0);
            let add = Btn::ghost("Add game or app").with_icon(icon::PLUS).small().id("add_capture_app").show(ui);
            if add.clicked() {
                self.windowed_apps = capture::list_windowed_apps();
            }
            crate::ui_kit::menu_popup(&add, true, 260.0, |ui| {
                crate::ui_kit::menu_head(ui, "Open apps with a window");
                let mut any = false;
                egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                    for app in self.windowed_apps.clone() {
                        if list.iter().any(|a| a.id.eq_ignore_ascii_case(&app.id)) {
                            continue;
                        }
                        any = true;
                        if Item::new(app.name.clone()).tip(app.id.clone()).show(ui).clicked() {
                            list.push(CaptureApp { id: app.id, name: app.name });
                        }
                    }
                });
                if !any {
                    crate::ui_kit::menu_head(ui, "Nothing else is open. Start the game or app, then add it here.");
                }
            });
        });
        if list.len() > 1 {
            ui.add_space(6.0);
            crate::ui_kit::hint(ui, "Records the one you're using, and keeps it while you click into something else as long as it's on screen.");
        }
        ui.add_space(10.0);
        group(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(theme::text("Show “Tabbed out” when it's minimized", 13.5, W::Regular).color(pal.text));
                crate::ui_kit::info(ui, "Tabbed out", "When the game you were in stops showing (most games minimize when you alt-tab), clips show the HesteClips logo with “Tabbed out”. Off: they keep the last picture. A window that's still on screen keeps being recorded either way.");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    crate::ui_kit::switch(ui, &mut away);
                });
            });
        });
        if let Some(i) = remove {
            let name = list.remove(i).name;
            self.toast_icon(icon::X_CIRCLE, format!("Stopped recording {name}"));
        }
        if list != apps || away != away_screen {
            self.settings.capture = CaptureTarget::Apps { apps: list, away_screen: away };
        }
    }

    /// The clip's mix: a stereo meter with its scale and peak, and the
    /// limiter with how much it's turning the mix down.
    fn master_strip(&mut self, ui: &mut egui::Ui, now: Instant, dt: f32) {
        use crate::theme::{self, W};
        let pal = theme::pal(ui);
        let view = &mut self.sources_view;
        view.master.update(self.live_audio.master.take(), now, dt);
        let reduction = self.live_audio.take_reduction_db();
        view.reduction_db = if reduction > view.reduction_db { reduction } else { (view.reduction_db - 12.0 * dt).max(reduction) };
        if reduction > 0.5 {
            view.limiting_at = Some(now);
        }
        let limiting = self.settings.limiter && view.limiting_at.is_some_and(|t| now - t < LIMIT_HOLD);
        group(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(theme::text("Clip mix", 14.0, W::Semibold).color(pal.text));
                crate::ui_kit::info(ui, "Clip mix", "Every source that's part of the clip sound, mixed. This is what people hear when you share a clip. Aim for speech peaking around −12 dB, out of the red.");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let readout = view.master.readout().replace('-', "−");
                    meter::fixed_label(ui, 52.0, &format!("{readout} dB"), view.master.readout_color(ui.visuals()), "The highest peak in the last 3 seconds (dBFS)");
                    ui.label(theme::text("Peak", 12.5, W::Regular).color(pal.text3));
                });
            });
            ui.add_space(8.0);
            let style = meter::Style { bar_h: 7.0, gap: 3.0, scale: true, dimmed: false };
            if meter::stereo(ui, egui::Id::new("master_meter"), &view.master, &style) {
                view.master.reset_clip();
            }
            ui.add_space(10.0);
            crate::ui_kit::divider(ui, 0.0);
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                crate::ui_kit::switch(ui, &mut self.settings.limiter);
                ui.label(theme::text("Limiter", 13.5, W::Regular).color(pal.text));
                crate::ui_kit::info(ui, "Limiter", "When sources add up too loud, the mix is turned down for a moment instead of distorting. Ceiling −1 dB.");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let shown = theme::toggle(ui.ctx(), egui::Id::new("limiter_shown"), self.settings.limiter, theme::NORMAL);
                    if shown > 0.0 {
                        ui.multiply_opacity(shown);
                        let gr = if view.reduction_db >= 0.1 { format!("−{:.1} dB", view.reduction_db) } else { "0.0 dB".to_owned() };
                        ui.label(theme::text(gr, 12.5, W::Medium).color(pal.text2));
                        meter::reduction(ui, view.reduction_db, 60.0).on_hover_text("How much the limiter is turning the mix down right now (0–12 dB)");
                        ui.label(theme::text("Turning down", 12.0, W::Regular).color(pal.text3));
                    }
                });
            });
            let note = if view.master.clipped() {
                Some((pal.meter_r, "Too loud: it distorted. Turn some sources down."))
            } else if limiting {
                Some((pal.warn, "The limiter is working: turn sources down a little for the cleanest sound."))
            } else {
                None
            };
            if let Some((color, text)) = note {
                ui.add_space(6.0);
                ui.add(egui::Label::new(theme::text(text, 12.5, W::Regular).color(color)).wrap());
            }
        });
    }

    /// Run a level-only capture while this page is open and nothing is recording,
    /// so the meters work before you start; stop it otherwise. Restarts when the
    /// sources change. Called every frame from the app loop.
    /// Like the meters' capture: a preview-only video capture while this page is
    /// open and nothing records (a running capture feeds the preview itself).
    /// Restarts when what to record changes.
    pub(crate) fn ensure_video_preview(&mut self) {
        let want = capture::preview::AVAILABLE && self.page == crate::Page::Sources && self.rec_state == crate::RecState::Idle;
        if !want {
            self.video_preview = None;
            return;
        }
        // What a recording would be: same picture, size and frame rate.
        let webcam = self.webcam_source();
        let wanted = (self.video_source(), self.settings.resolution.height(), self.settings.fps, webcam.clone());
        // A changed app list reaches the running preview without a restart.
        if let Some((have, preview)) = &mut self.video_preview {
            if *have != wanted && (&have.1, have.2, &have.3) == (&wanted.1, wanted.2, &wanted.3) && preview.update(&wanted.0) {
                have.0 = wanted.0.clone();
            }
        }
        if self.video_preview.as_ref().is_none_or(|(w, _)| *w != wanted) {
            let (source, height, fps, _) = &wanted;
            // Both happen in the background: the old capture closes as the new
            // one opens, and only the new one's frames are shown.
            let preview = capture::preview::VideoPreview::start(source, *height, *fps, Some(self.away_screen.clone()), webcam);
            self.video_preview = Some((wanted, preview));
        }
    }

    pub(crate) fn ensure_level_monitor(&mut self) {
        let want = self.page == crate::Page::Sources && self.rec_state == crate::RecState::Idle;
        if !want {
            self.level_monitor = None;
            return;
        }
        let sources = self.capture_sources();
        let current = self.level_monitor.as_ref().map(|(s, _)| s);
        if current != Some(&sources) {
            self.level_monitor = None; // stop the old one before opening devices again
            match capture::sources::LevelMonitor::start(&sources, self.live_audio.clone()) {
                Ok(m) => self.level_monitor = Some((sources, m)),
                Err(e) => self.toast_error(format!("Couldn't open audio: {e}")),
            }
        }
    }
}

enum CardAction {
    None,
    Remove,
    MoveUp,
    /// "Listen" on a microphone: hear it live, or stop.
    Listen,
    /// Something locked while capturing was clicked: say why.
    Locked,
    NoiseRemoval(bool),
}

/// Hearing a microphone live: which source, from which device.
pub(crate) struct Listening {
    pub source: String,
    pub device: String,
    pub monitor: capture::monitor::MicMonitor,
}

/// Where a source goes, as one choice.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Route {
    Both,
    Mix,
    Track,
}

impl Route {
    fn of(s: &AudioSourceCfg) -> Self {
        match (s.in_mix, s.own_track) {
            (true, false) => Route::Mix,
            (false, true) => Route::Track,
            _ => Route::Both,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Route::Both => "Clip sound and own track",
            Route::Mix => "Clip sound only",
            Route::Track => "Own track only",
        }
    }
    fn tip(self) -> &'static str {
        match self {
            Route::Both => "Heard in the clip, and kept on its own to rebalance later",
            Route::Mix => "Heard in the clip, not kept separately",
            Route::Track => "Not heard in the clip until you turn it up in the editor",
        }
    }
}

/// One source: its icon, name and device; its level and volume as one
/// control (the meter runs along the fader's track, the knob sits on it); the
/// number to type a value; mute; where it goes; and for a microphone, noise
/// removal and Listen.
#[allow(clippy::too_many_arguments)]
fn source_card(
    ui: &mut egui::Ui,
    source: &mut AudioSourceCfg,
    channel: &Channel,
    meters: &mut SourceMeters,
    devices: &capture::audio::AudioDevices,
    has_app_sources: bool,
    renaming: &mut Option<String>,
    // A microphone's "Listen": `Some(latency in ms, if known)` while on.
    listening: Option<Option<f32>>,
    // While capturing: why what's recorded can't change (it's set at the start).
    locked: Option<&str>,
    first: bool,
) -> CardAction {
    use crate::theme::{self, W};
    use crate::ui_kit::{Btn, Item, Kind};
    use egui_phosphor::regular as icon;
    let pal = theme::pal(ui);
    let mut action = CardAction::None;
    // A mic can switch device while capturing (its track stays); on Linux
    // mics are set when capture starts.
    let picker_locked = locked.filter(|_| cfg!(target_os = "linux") || !matches!(source.kind, SourceKind::Microphone { .. }));
    group(ui, |ui| {
        ui.multiply_opacity(if source.enabled { 1.0 } else { 0.6 });
        // --- Its icon, name, what it is; device; ⋯ ---
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            let (r, _) = ui.allocate_exact_size(egui::vec2(30.0, 30.0), Sense::hover());
            ui.painter().rect_filled(r, 6, pal.control);
            let glyph = match &source.kind {
                SourceKind::Microphone { .. } => icon::MICROPHONE,
                SourceKind::Desktop { .. } => icon::SPEAKER_HIFI,
                SourceKind::App { app_name, .. } if app_name.to_lowercase().contains("discord") => icon::DISCORD_LOGO,
                SourceKind::App { .. } => icon::APP_WINDOW,
            };
            ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, glyph, egui::FontId::proportional(16.0), pal.text2);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                if renaming.as_deref() == Some(source.id.as_str()) {
                    let r = ui.add(egui::TextEdit::singleline(&mut source.name).font(theme::font(14.0, W::Medium)).desired_width(170.0));
                    r.request_focus();
                    if r.lost_focus() {
                        *renaming = None;
                    }
                } else {
                    let r = ui.add(egui::Label::new(theme::text(&source.name, 14.0, W::Medium).color(if source.muted { pal.text2 } else { pal.text })).truncate().sense(Sense::click()));
                    if r.on_hover_text("Double-click to rename").double_clicked() {
                        *renaming = Some(source.id.clone());
                    }
                }
                let what = match &source.kind {
                    SourceKind::Desktop { exclude_apps: true } if has_app_sources => Some("Everything except apps added below".to_owned()),
                    SourceKind::Desktop { .. } => Some("Everything your computer plays".to_owned()),
                    SourceKind::App { .. } => Some("Only this app".to_owned()),
                    SourceKind::Microphone { .. } => None,
                };
                let status = status_text(channel.status());
                match (status, what) {
                    (Some((t, c)), _) => {
                        ui.label(theme::text(t, 12.5, W::Regular).color(c(pal)));
                    }
                    (None, Some(w)) => {
                        ui.label(theme::text(w, 12.5, W::Regular).color(pal.text3));
                    }
                    _ => {}
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let more = Btn::icon(Kind::Ghost, icon::DOTS_THREE).small().tip("Rename, move or remove").show(ui);
                crate::ui_kit::menu_popup(&more, false, 200.0, |ui| {
                    if Item::new("Rename…").icon(icon::PENCIL_SIMPLE).show(ui).clicked() {
                        *renaming = Some(source.id.clone());
                    }
                    if Item::new("Move up").icon(icon::CARET_UP).enabled(!first && locked.is_none()).show(ui).clicked() {
                        action = CardAction::MoveUp;
                    }
                    if Item::new(if source.muted { "Unmute" } else { "Mute" }).icon(icon::SPEAKER_X).show(ui).clicked() {
                        source.muted = !source.muted;
                    }
                    let on = Item::new(if source.enabled { "Turn off" } else { "Turn on" }).icon(icon::POWER).enabled(locked.is_none());
                    if on.show(ui).clicked() {
                        source.enabled = !source.enabled;
                    }
                    if let SourceKind::Desktop { exclude_apps } = &mut source.kind {
                        crate::ui_kit::menu_sep(ui);
                        let item = Item::new("Leave out apps added below").check(*exclude_apps).enabled(has_app_sources && locked.is_none());
                        let item = item.tip("Apps you add as their own source won't also be heard here, so nothing plays twice");
                        if item.show(ui).clicked() {
                            *exclude_apps = !*exclude_apps;
                        }
                    }
                    crate::ui_kit::menu_sep(ui);
                    let rm = Item::new("Remove").icon(icon::TRASH).danger();
                    if rm.show(ui).clicked() {
                        action = if locked.is_some() { CardAction::Locked } else { CardAction::Remove };
                    }
                });
                if matches!(source.kind, SourceKind::Microphone { .. }) {
                    if let Some(a) = mic_picker(ui, source, devices, picker_locked.is_some()) {
                        action = a;
                    }
                }
            });
        });
        if !source.enabled {
            return;
        }
        ui.add_space(10.0);

        // --- Level and volume, as one control; the value; mute ---
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            let w = ui.available_width() - 66.0 - 26.0 - 2.0 * 10.0;
            // Muted: what the source delivers, dimmed, so you see the mic is
            // live without it being recorded. Otherwise what goes into the clip.
            let state = if source.muted { &meters.input } else { &meters.output };
            fader(ui, &mut source.volume_db, state, source.muted, w.max(80.0), &source.id);
            let (r, resp) = ui.allocate_exact_size(egui::vec2(66.0, 22.0), Sense::click());
            let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered(), theme::FAST);
            ui.painter().rect_filled(r, 4, theme::fade(pal.sel, hv));
            ui.painter().text(r.right_center() - egui::vec2(6.0, 0.0), egui::Align2::RIGHT_CENTER, db_text(source.volume_db), theme::font(12.5, W::Medium), theme::mix(pal.text2, pal.text, hv));
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Click for 0 dB").clicked() {
                source.volume_db = 0.0;
            }
            let (r, resp) = ui.allocate_exact_size(egui::vec2(26.0, 24.0), Sense::click());
            let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered(), theme::FAST);
            let on = theme::toggle(ui.ctx(), resp.id.with("on"), source.muted, theme::FAST);
            ui.painter().rect_filled(r, 4, theme::mix(theme::mix(pal.control, pal.control_h, hv), pal.warn, on));
            ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, "M", theme::font(11.0, W::Bold), theme::mix(pal.text2, Color32::from_rgb(20, 21, 24), on));
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(if source.muted { "Unmute" } else { "Mute: keep the source but record silence" }).clicked() {
                source.muted = !source.muted;
            }
        });

        // --- Where it goes; mic tools ---
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let route = Route::of(source);
            let chip = Btn::normal(route.label()).trail(icon::CARET_DOWN).small().id(("route", source.id.as_str())).tip(locked.unwrap_or("Where this source goes")).show(ui);
            if chip.clicked() && locked.is_some() {
                action = CardAction::Locked;
                egui::Popup::close_id(ui.ctx(), chip.id.with("menu"));
            }
            if locked.is_none() {
                crate::ui_kit::menu_popup(&chip, true, 260.0, |ui| {
                    for r in [Route::Both, Route::Mix, Route::Track] {
                        if Item::new(r.label()).check(route == r).tip(r.tip()).show(ui).clicked() {
                            (source.in_mix, source.own_track) = match r {
                                Route::Both => (true, true),
                                Route::Mix => (true, false),
                                Route::Track => (false, true),
                            };
                        }
                    }
                });
            }
            crate::ui_kit::info(ui, "Where it goes", "Clip sound: part of what everyone hears when you play or share a clip. Own track: also kept on its own, so you can change its volume later in the editor.");
            if matches!(source.kind, SourceKind::Microphone { .. }) {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let on = listening.is_some();
                    let tip = if on { "Stop listening" } else { "Listen through your headphones, to check it works and how it sounds" };
                    if Btn::icon(if on { Kind::On } else { Kind::Ghost }, icon::HEADPHONES).small().tip(tip).show(ui).clicked() {
                        action = CardAction::Listen;
                    }
                    let nr = source.noise_removal;
                    let tip = format!("Remove background noise: {}. Takes out steady sounds behind your voice (fans, a keyboard, hum) and keeps your voice.", if nr { "on" } else { "off" });
                    if Btn::icon(if nr { Kind::On } else { Kind::Ghost }, icon::MAGIC_WAND).small().tip(tip).show(ui).clicked() {
                        source.noise_removal = !nr;
                        action = CardAction::NoiseRemoval(!nr);
                    }
                });
            }
        });
        if let Some(latency) = listening {
            ui.add_space(6.0);
            let ms = latency.map_or(String::new(), |ms| format!(", {ms:.0} ms delay"));
            ui.label(theme::text(format!("Listening{ms}. Use headphones, or it feeds back."), 12.0, W::Regular).color(pal.warn));
        }
    });
    action
}

/// A source's volume and level as one control: the meter runs along the
/// track (green, yellow from −18, red from −6), the knob sits at the volume,
/// and a tick marks 0 dB. Drag the knob, or anywhere on the track; double-click
/// for 0 dB.
fn fader(ui: &mut egui::Ui, db: &mut f32, state: &MeterState, dimmed: bool, width: f32, id: &str) -> egui::Response {
    let pal = crate::theme::pal(ui);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 22.0), Sense::click_and_drag());
    let track = egui::Rect::from_min_max(egui::pos2(rect.left(), rect.top() + 7.0), egui::pos2(rect.right(), rect.top() + 15.0));
    let pos = |d: f32| ((d - FADER_MIN_DB) / (FADER_MAX_DB - FADER_MIN_DB)).clamp(0.0, 1.0);
    // The meter, on the same scale as the knob (dB along the fader).
    meter::fader_meter(ui.painter(), track, state, dimmed, |d| track.left() + track.width() * pos(d), pal);
    // 0 dB.
    let zero = track.left() + track.width() * pos(0.0);
    ui.painter().vline(zero, rect.top() + 4.0..=rect.bottom() - 4.0, egui::Stroke::new(1.0, crate::theme::fade(pal.text3, 0.5)));
    if let Some(p) = resp.interact_pointer_pos()
        && (resp.dragged() || resp.drag_started())
    {
        let f = ((p.x - track.left()) / track.width()).clamp(0.0, 1.0);
        let mut v = FADER_MIN_DB + f * (FADER_MAX_DB - FADER_MIN_DB);
        // A small detent at 0 dB.
        if (v.abs()) < 0.6 {
            v = 0.0;
        }
        *db = (v * 10.0).round() / 10.0;
    }
    if resp.double_clicked() {
        *db = 0.0;
    }
    let hv = crate::theme::toggle(ui.ctx(), egui::Id::new(("fader_hv", id)), resp.hovered() || resp.dragged(), crate::theme::FAST);
    let x = crate::theme::value(ui.ctx(), egui::Id::new(("fader_x", id)), track.left() + track.width() * pos(*db) - rect.left(), if resp.dragged() { 0.0 } else { crate::theme::FAST }) + rect.left();
    let knob = egui::Rect::from_center_size(egui::pos2(x, rect.center().y), egui::vec2(8.0 + 2.0 * hv, 20.0));
    let shadow = egui::Shadow { offset: [0, 1], blur: 4, spread: 0, color: Color32::from_black_alpha(110) };
    ui.painter().add(shadow.as_shape(knob, 3));
    ui.painter().rect_filled(knob, 3, Color32::WHITE);
    resp.on_hover_cursor(egui::CursorIcon::ResizeHorizontal).on_hover_text("Volume: drag the knob. Double-click for 0 dB")
}

fn db_text(db: f32) -> String {
    if db <= FADER_MIN_DB + 0.05 {
        "−∞ dB".to_owned()
    } else if db.abs() < 0.05 {
        "0.0 dB".to_owned()
    } else {
        format!("{}{:.1} dB", if db > 0.0 { "+" } else { "−" }, db.abs())
    }
}

/// A microphone's device: the current one, a menu of the others.
fn mic_picker(ui: &mut egui::Ui, source: &mut AudioSourceCfg, audio: &capture::audio::AudioDevices, locked: bool) -> Option<CardAction> {
    use crate::ui_kit::Item;
    use capture::audio::DEFAULT_DEVICE;
    let SourceKind::Microphone { device } = &mut source.kind else { return None };
    let default_label = match &audio.default_input {
        Some(name) => format!("Default ({name})"),
        None => "Default".to_owned(),
    };
    let selected = if device == DEFAULT_DEVICE {
        default_label.clone()
    } else if audio.inputs.iter().any(|d| &d.id == device) {
        device.clone()
    } else {
        format!("{device} (disconnected)")
    };
    let w = ui.available_width().min(210.0);
    let shown = truncate_to(ui, &selected, w - 40.0);
    let r = crate::ui_kit::select(ui, &shown, 0.0).on_hover_text(&selected);
    if locked {
        if r.clicked() {
            egui::Popup::close_id(ui.ctx(), r.id.with("menu"));
            return Some(CardAction::Locked);
        }
        return None;
    }
    crate::ui_kit::menu_popup(&r, false, 260.0, |ui| {
        if Item::new(default_label.clone()).check(device == DEFAULT_DEVICE).show(ui).clicked() {
            *device = DEFAULT_DEVICE.to_owned();
        }
        crate::ui_kit::menu_sep(ui);
        for d in &audio.inputs {
            if Item::new(d.name.clone()).check(&d.id == device).show(ui).clicked() {
                *device = d.id.clone();
            }
        }
    });
    None
}

/// `text`, shortened with … to fit `width` at the select's size.
fn truncate_to(ui: &egui::Ui, text: &str, width: f32) -> String {
    let font = crate::theme::font(13.0, crate::theme::W::Medium);
    let fits = |t: &str| ui.painter().layout_no_wrap(t.to_owned(), font.clone(), Color32::PLACEHOLDER).size().x <= width;
    if fits(text) {
        return text.to_owned();
    }
    let mut chars: Vec<char> = text.chars().collect();
    while !chars.is_empty() {
        chars.pop();
        let t: String = chars.iter().collect::<String>() + "…";
        if fits(&t) {
            return t;
        }
    }
    "…".into()
}

/// A source's state when it isn't simply live: waiting for its app, or
/// unavailable.
fn status_text(status: SourceStatus) -> Option<(&'static str, fn(&crate::theme::Pal) -> Color32)> {
    match status {
        SourceStatus::Live | SourceStatus::Off => None,
        SourceStatus::WaitingForApp => Some((if cfg!(target_os = "linux") { "Starts when the app plays sound" } else { "Starts when the app opens" }, |p| p.text3)),
        SourceStatus::Unavailable => Some(("Not connected, or couldn't be opened", |p| p.warn)),
    }
}

/// A group on the page: the raised surface, padded.
pub(crate) fn group(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let pal = crate::theme::pal(ui);
    egui::Frame::new().fill(pal.raise).corner_radius(8).inner_margin(egui::Margin { left: 14, right: 12, top: 12, bottom: 12 }).show(ui, |ui| {
        ui.set_width(ui.available_width());
        add(ui);
    });
}

/// A column's heading ("Picture", "Sound"), with whatever goes on its right.
pub(crate) fn column_head(ui: &mut egui::Ui, title: &str, right: impl FnOnce(&mut egui::Ui)) {
    let pal = crate::theme::pal(ui);
    ui.horizontal(|ui| {
        ui.set_min_height(30.0);
        ui.label(crate::theme::text(title, 15.0, crate::theme::W::Semibold).color(pal.text));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), right);
    });
    ui.add_space(10.0);
}

pub(crate) fn from_db(db: f32) -> f32 {
    if db <= FADER_MIN_DB + 0.05 { 0.0 } else { 10f32.powf(db / 20.0) }
}
