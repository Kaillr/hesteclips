//! The Sources page: what goes into your clips. Today that's audio — your mic,
//! everything the computer plays, or single apps — each with a live meter and a
//! volume fader, plus the clip's mix meter. Video sources (webcam, overlays)
//! will live here too.
//!
//! Deliberately says nothing about "tracks": a source is either part of what the
//! clip sounds like ("In the clip"), kept separately for editing, or both.

use std::time::{Duration, Instant};

use capture::mixer::{Channel, SourceStatus};
use egui::{Color32, RichText, Sense, vec2};

use crate::App;
use crate::meter::{self, MeterState};
use crate::library::ACCENT;
use crate::settings::{AudioSourceCfg, CaptureApp, CaptureTarget, SourceKind};

/// Fader range in dB.
const FADER_MIN_DB: f32 = -60.0;
const FADER_MAX_DB: f32 = 12.0;
/// How long the "limiting" note stays after the limiter last worked.
const LIMIT_HOLD: Duration = Duration::from_secs(3);
/// Below this content width, a source's controls stack under its meter.
const NARROW: f32 = 620.0;
/// The page's content column never gets wider than this.
const MAX_WIDTH: f32 = 880.0;

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

/// Why the whole-screen / games-and-apps switch is locked.
const STOP_TO_SWITCH: &str = "Stop capturing to switch. The list of games and apps can change while you capture.";

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
        // Meters and the preview move: keep repainting while this page is open —
        // every display refresh when there's a preview to keep up with.
        if capture::preview::AVAILABLE {
            ui.ctx().request_repaint();
        } else {
            ui.ctx().request_repaint_after(Duration::from_millis(16));
        }

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            // Centred column that follows the window, up to a comfortable width.
            let width = ui.available_width().min(MAX_WIDTH);
            let pad = ((ui.available_width() - width) / 2.0).max(0.0);
            ui.horizontal(|ui| {
                ui.add_space(pad);
                ui.vertical(|ui| {
                    ui.set_width(width);
                    self.sources_column(ui, now, dt);
                });
            });
        });
        self.live_audio.set_limiter(self.settings.limiter);
    }

    fn sources_column(&mut self, ui: &mut egui::Ui, now: Instant, dt: f32) {
        ui.add_space(12.0);
        if capture::preview::AVAILABLE {
            self.video_card(ui);
            ui.add_space(12.0);
            if capture::webcam::AVAILABLE {
                self.webcam_card(ui);
                ui.add_space(12.0);
            }
        }
        // The mix first: it's what people hear, and where clipping matters most.
        self.master_strip(ui, now, dt);
        ui.add_space(18.0);

        ui.horizontal(|ui| {
            ui.label(RichText::new("Audio sources").size(16.0).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("⟳ Refresh devices").on_hover_text("Look again for microphones you've plugged in.").clicked() {
                    self.refresh_audio_devices();
                }
            });
        });
        ui.weak("Aim for speech peaking around -12 dB and staying out of the red.");
        ui.add_space(8.0);

        let mut remove = None;
        let mut move_up = None;
        let n = self.settings.audio_sources.len();
        let has_app_sources = self.settings.audio_sources.iter().any(|s| matches!(s.kind, SourceKind::App { .. }));
        for i in 0..n {
            let channel = self.live_audio.channel(&self.settings.audio_sources[i].id);
            let id = self.settings.audio_sources[i].id.clone();
            let id_for_listen = id.clone();
            let meters = self.sources_view.meters.entry(id).or_default();
            meters.input.update(channel.input.take(), now, dt);
            meters.output.update(channel.meter.take(), now, dt);
            let listening = self.listening.as_ref().filter(|l| l.source == id_for_listen).map(|l| l.monitor.latency_ms());
            let action = source_card(
                ui,
                &mut self.settings.audio_sources[i],
                &channel,
                meters,
                &self.audio,
                has_app_sources,
                &mut self.sources_view.renaming,
                listening,
            );
            match action {
                CardAction::Remove => remove = Some(i),
                CardAction::MoveUp if i > 0 => move_up = Some(i),
                CardAction::Listen => self.toggle_listening(i),
                _ => {}
            }
            // Volume applies live, even mid-recording.
            let s = &self.settings.audio_sources[i];
            channel.set_volume(from_db(s.volume_db), s.muted);
            ui.add_space(8.0);
        }
        if let Some(i) = remove {
            self.settings.audio_sources.remove(i);
        }
        if let Some(i) = move_up {
            self.settings.audio_sources.swap(i, i - 1);
        }

        self.keep_listening();
        self.add_source_buttons(ui);

        if self.rec_state != crate::RecState::Idle {
            ui.add_space(10.0);
            ui.weak("Volume changes apply right away. Adding or removing sources applies the next time you start the buffer or a recording.");
        }
        ui.add_space(16.0);
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
        // Wraps onto two lines in a narrow window.
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Add").strong());
            if ui.button("🎤 Microphone").clicked() {
                let n = self.settings.audio_sources.iter().filter(|s| matches!(s.kind, SourceKind::Microphone { .. })).count();
                let name = if n == 0 { "Microphone".to_owned() } else { format!("Microphone {}", n + 1) };
                self.settings.audio_sources.push(AudioSourceCfg::new(
                    &name,
                    SourceKind::Microphone { device: capture::audio::DEFAULT_DEVICE.into() },
                ));
            }
            let menu = ui.menu_button("🎮 App", |ui| {
                ui.set_min_width(240.0);
                ui.weak("Record one app on its own — a game, Discord, music.");
                ui.separator();
                let taken: Vec<String> = self
                    .settings
                    .audio_sources
                    .iter()
                    .filter_map(|s| match &s.kind {
                        SourceKind::App { bundle_id, .. } => Some(bundle_id.clone()),
                        _ => None,
                    })
                    .collect();
                egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                    for app in self.sources_view.apps.clone() {
                        let added = taken.contains(&app.id);
                        if ui.add_enabled(!added, egui::Button::new(&app.name)).on_disabled_hover_text("Already added").clicked() {
                            self.settings.audio_sources.push(AudioSourceCfg::new(
                                &app.name,
                                SourceKind::App { bundle_id: app.id.clone(), app_name: app.name.clone() },
                            ));
                            ui.close();
                        }
                    }
                });
                if self.sources_view.apps.is_empty() {
                    // PipeWire only knows apps by their sound.
                    ui.weak(if cfg!(target_os = "linux") { "No apps are playing sound. Start one, then look again." } else { "No apps found." });
                }
            });
            if menu.response.clicked() {
                self.sources_view.apps = capture::list_apps();
            }
            let has_desktop = self.settings.audio_sources.iter().any(|s| matches!(s.kind, SourceKind::Desktop { .. }));
            if !has_desktop && ui.button("🖥 Desktop sound").clicked() {
                self.settings
                    .audio_sources
                    .push(AudioSourceCfg::new("Desktop", SourceKind::Desktop { exclude_apps: true }));
            }
        });
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

        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("🖵 Video").size(16.0).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(f) = &frame {
                        let rate = self.sources_view.preview_rate.2;
                        ui.weak(format!("{}×{} · {rate} fps", f.recorded.0, f.recorded.1)).on_hover_text(
                            "What's recorded: its size, and how many frames the preview is showing per second \
                             (it can't show more than your display refreshes). Change them in Settings → Video quality.",
                        );
                    }
                });
            });
            ui.add_space(6.0);

            // What to record. Switching kinds needs a fresh start, so it waits
            // while capturing; the app list below changes live.
            if capture::APP_CAPTURE {
                ui.add_enabled_ui(idle, |ui| {
                    let (screen, apps) = segmented(ui, ["🖥  Whole screen", "🎮  Games and apps"], usize::from(apps_mode));
                    {
                        if screen.on_hover_text("Everything on one display.").on_disabled_hover_text(STOP_TO_SWITCH).clicked() && apps_mode {
                            let apps = std::mem::replace(&mut self.settings.capture, CaptureTarget::Screen);
                            if matches!(apps, CaptureTarget::Apps { .. }) {
                                self.settings.idle_apps = Some(apps);
                            }
                        }
                        if apps
                            .on_hover_text("Only the games and apps you pick, following whichever you're using.")
                            .on_disabled_hover_text(STOP_TO_SWITCH)
                            .clicked()
                            && !apps_mode
                        {
                            self.settings.capture = self
                                .settings
                                .idle_apps
                                .take()
                                .unwrap_or(CaptureTarget::Apps { apps: Vec::new(), away_screen: true });
                            self.windowed_apps = capture::list_windowed_apps();
                        }
                    }
                });
                ui.add_space(10.0);
            }

            self.preview_picture(ui, frame.as_deref());
            ui.add_space(8.0);

            match self.settings.capture.clone() {
                CaptureTarget::Apps { apps, away_screen } if capture::APP_CAPTURE => self.app_list(ui, apps, away_screen, frame.as_deref()),
                // Linux on Wayland: the desktop's own dialog picks the screen (once; it's remembered).
                _ if cfg!(target_os = "linux") && !capture::screens_listed() => {
                    ui.add_enabled_ui(idle, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("Screen");
                            ui.weak("picked in your desktop's sharing dialog");
                            if ui.button("Choose another…").on_hover_text("Your desktop asks again which screen to record").clicked() {
                                capture::choose_screen_again();
                                self.video_preview = None; // restarts, and asks
                            }
                        })
                        .response
                        .on_disabled_hover_text("Stop capturing to switch screens.");
                    });
                }
                _ => {
                    ui.add_enabled_ui(idle, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("Display");
                            let names: Vec<String> = self.screens.iter().map(|d| d.name.clone()).collect();
                            let current = names.get(self.settings.display_index).cloned().unwrap_or_else(|| "Main display".to_owned());
                            egui::ComboBox::from_id_salt("display").selected_text(current).truncate().show_ui(ui, |ui| {
                                for (i, name) in names.iter().enumerate() {
                                    ui.selectable_value(&mut self.settings.display_index, i, name);
                                }
                            });
                        })
                        .response
                        .on_disabled_hover_text("Stop capturing to switch displays.");
                    });
                }
            }
        });
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
        let aspect = frame.map_or(16.0 / 9.0, |f| f.width as f32 / f.height.max(1) as f32);
        let max_h = (ui.ctx().content_rect().height() / 3.0).max(120.0);
        let w = ui.available_width().min(max_h * aspect);
        ui.vertical_centered(|ui| {
            let (rect, response) = ui.allocate_exact_size(egui::vec2(w, w / aspect), Sense::click_and_drag());
            let p = ui.painter();
            p.rect_filled(rect, 4.0, Color32::BLACK);
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
                (true, Some(f)) => {
                    if f.waiting && matches!(&self.settings.capture, CaptureTarget::Apps { apps, .. } if apps.is_empty()) {
                        let band = egui::Rect::from_center_size(rect.center_bottom() - egui::vec2(0.0, 22.0), egui::vec2(rect.width(), 28.0));
                        p.rect_filled(band, 0.0, Color32::from_black_alpha(180));
                        p.text(band.center(), egui::Align2::CENTER_CENTER, "Add a game or app below to start.", egui::FontId::proportional(13.0), Color32::from_gray(220));
                    }
                }
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
    /// add more. Changes apply right away, even while capturing.
    fn app_list(&mut self, ui: &mut egui::Ui, apps: Vec<CaptureApp>, away_screen: bool, frame: Option<&capture::preview::PreviewFrame>) {
        let showing = frame.filter(|f| !f.waiting).and_then(|f| f.app.clone());
        let capturing = self.rec_state != crate::RecState::Idle;
        let mut list = apps.clone();
        let mut away = away_screen;
        let mut remove = None;
        let weak = ui.visuals().weak_text_color();
        egui::Frame::new()
            .fill(ui.visuals().extreme_bg_color)
            .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
            .corner_radius(6)
            .inner_margin(egui::Margin::symmetric(4, 4))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                if list.is_empty() {
                    ui.add_space(10.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new("No games or apps yet").strong());
                        ui.weak("Add the games you play, and any apps you want in your clips.");
                    });
                    ui.add_space(10.0);
                }
                for (i, app) in list.iter().enumerate() {
                    let open = self.windowed_apps.iter().any(|a| a.id.eq_ignore_ascii_case(&app.id));
                    let active = showing.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(&app.id));
                    let (color, status) = if active {
                        (ACCENT, if capturing { "Recording now" } else { "In the preview" })
                    } else if open {
                        (meter::GREEN, "Open")
                    } else {
                        (weak, "Not open")
                    };
                    // Painted under the row once its size is known.
                    let hover_bg = ui.painter().add(egui::Shape::Noop);
                    let row = ui.horizontal(|ui| {
                        ui.set_min_height(34.0);
                        ui.add_space(8.0);
                        let (dot, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), Sense::hover());
                        ui.painter().circle_filled(dot.center(), 4.5, color);
                        ui.add_space(4.0);
                        ui.label(RichText::new(&app.name).size(14.0).strong()).on_hover_text(&app.id);
                        ui.add_space(4.0);
                        status_tag(ui, status, color);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add_space(4.0);
                            if remove_button(ui).on_hover_text(format!("Stop recording {}", app.name)).clicked() {
                                remove = Some(i);
                            }
                        });
                    });
                    // Rows divided by a hairline, highlighted under the pointer.
                    let rect = row.response.rect;
                    if ui.rect_contains_pointer(rect) {
                        let fill = ui.visuals().widgets.hovered.weak_bg_fill.gamma_multiply(0.5);
                        ui.painter().set(hover_bg, egui::epaint::RectShape::filled(rect, 4.0, fill));
                    }
                    if i + 1 < list.len() {
                        let y = rect.bottom() + ui.spacing().item_spacing.y / 2.0;
                        ui.painter().hline(rect.x_range().shrink(8.0), y, ui.visuals().widgets.noninteractive.bg_stroke);
                    }
                }
            });
        if let Some(i) = remove {
            list.remove(i);
        }
        ui.add_space(8.0);
        let add = ui.menu_button(RichText::new("➕  Add game or app").size(14.0), |ui| {
            ui.set_min_width(260.0);
            ui.weak("Open apps with a window");
            ui.separator();
            let mut any = false;
            egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                for app in self.windowed_apps.clone() {
                    if list.iter().any(|a| a.id.eq_ignore_ascii_case(&app.id)) {
                        continue;
                    }
                    any = true;
                    if ui.button(&app.name).on_hover_text(&app.id).clicked() {
                        list.push(CaptureApp { id: app.id, name: app.name });
                        ui.close();
                    }
                }
            });
            if !any {
                ui.weak("Nothing else is open. Start the game or app, then add it here.");
            }
        });
        if add.response.clicked() {
            self.windowed_apps = capture::list_windowed_apps();
        }
        // Their sound usually belongs with their picture: a suggestion, not
        // another button that looks like adding a game.
        let silent: Vec<CaptureApp> = list
            .iter()
            .filter(|a| {
                !self
                    .settings
                    .audio_sources
                    .iter()
                    .any(|s| matches!(&s.kind, SourceKind::App { bundle_id, .. } if bundle_id.eq_ignore_ascii_case(&a.id)))
            })
            .cloned()
            .collect();
        if !silent.is_empty() {
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                let names = if silent.len() == 1 { format!("{}'s sound isn't", silent[0].name) } else { "Their sound isn't".to_owned() };
                ui.label(RichText::new(format!("🔈 {names} in your clips yet.")).color(weak));
                if ui.link("Add it to the audio sources").clicked() {
                    for a in &silent {
                        self.settings
                            .audio_sources
                            .push(AudioSourceCfg::new(&a.name, SourceKind::App { bundle_id: a.id.clone(), app_name: a.name.clone() }));
                    }
                }
            });
        }
        ui.add_space(8.0);
        ui.checkbox(&mut away, "Show \u{201c}Tabbed out\u{201d} when it's minimized").on_hover_text(
            "When the game or app you were in stops showing (most games minimize when you alt-tab), \
             clips show the HesteClips logo with \u{201c}Tabbed out\u{201d}. Off: they keep its last picture. \
             A window that's still on screen keeps being recorded either way, \
             and that screen also shows while none of them is open.",
        );
        if list.len() > 1 {
            ui.label(RichText::new("Records the one you're using, and keeps it while you click into something else as long as it's on screen.").size(12.0).weak());
        }
        if list != apps || away != away_screen {
            self.settings.capture = CaptureTarget::Apps { apps: list, away_screen: away };
        }
    }

    fn master_strip(&mut self, ui: &mut egui::Ui, now: Instant, dt: f32) {
        let view = &mut self.sources_view;
        view.master.update(self.live_audio.master.take(), now, dt);
        let reduction = self.live_audio.take_reduction_db();
        view.reduction_db = if reduction > view.reduction_db { reduction } else { (view.reduction_db - 12.0 * dt).max(reduction) };
        if reduction > 0.5 {
            view.limiting_at = Some(now);
        }
        let limiting = self.settings.limiter && view.limiting_at.is_some_and(|t| now - t < LIMIT_HOLD);
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("🔊 Clip mix").size(16.0).strong())
                    .on_hover_text("Every source marked \"In the clip\", mixed: what people hear when you share a clip.");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    meter::fixed_label(ui, 64.0, &format!("{} dB", view.master.readout()), view.master.readout_color(ui.visuals()), "Highest peak in the last 3 seconds (dBFS)");
                    ui.weak("Peak");
                });
            });
            ui.add_space(4.0);
            let style = meter::Style { bar_h: 12.0, gap: 3.0, scale: true, dimmed: false };
            if meter::stereo(ui, egui::Id::new("master_meter"), &view.master, &style) {
                view.master.reset_clip();
            }
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut self.settings.limiter, "Limiter")
                    .on_hover_text("Turns the mix down for a moment when sources add up too loud, instead of distorting. Ceiling -1 dBFS.");
                if self.settings.limiter {
                    meter::reduction(ui, view.reduction_db, 90.0)
                        .on_hover_text("How much the limiter is turning the mix down right now (0–12 dB).");
                    let gr = if view.reduction_db >= 0.1 { format!("-{:.1} dB", view.reduction_db) } else { "0.0 dB".to_owned() };
                    ui.label(RichText::new(gr).monospace().size(12.0));
                }
                let note = if view.master.clipped() {
                    Some((meter::RED, "Clipped: turn some sources down."))
                } else if limiting {
                    Some((meter::YELLOW, "Limiting: turn sources down a little for the cleanest sound."))
                } else {
                    None
                };
                if let Some((color, text)) = note {
                    ui.add_space(8.0);
                    ui.colored_label(color, text);
                }
            });
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
}

/// Hearing a microphone live: which source, from which device.
pub(crate) struct Listening {
    pub source: String,
    pub device: String,
    pub monitor: capture::monitor::MicMonitor,
}

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
) -> CardAction {
    let mut action = CardAction::None;
    card(ui, |ui| {
        let narrow = ui.available_width() < NARROW;
        // --- Header: on/off, name, status · what it captures, menu ---
        ui.horizontal(|ui| {
            ui.checkbox(&mut source.enabled, "").on_hover_text(if source.enabled { "Turn off" } else { "Turn on" });
            if renaming.as_deref() == Some(source.id.as_str()) {
                let r = ui.add(egui::TextEdit::singleline(&mut source.name).desired_width(160.0));
                r.request_focus();
                if r.lost_focus() {
                    *renaming = None;
                }
            } else {
                let title = RichText::new(format!("{} {}", source.icon(), source.name)).size(15.0).strong();
                let title = if source.enabled { title } else { title.weak() };
                let r = ui.add(egui::Label::new(title).truncate().sense(Sense::click()));
                if r.on_hover_text("Double-click to rename").double_clicked() {
                    *renaming = Some(source.id.clone());
                }
            }
            if source.enabled {
                status_badge(ui, channel.status());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.menu_button(RichText::new("…").size(16.0), |ui| {
                    if ui.button("Rename").clicked() {
                        *renaming = Some(source.id.clone());
                        ui.close();
                    }
                    if ui.button("Move up").clicked() {
                        action = CardAction::MoveUp;
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Remove").clicked() {
                        action = CardAction::Remove;
                        ui.close();
                    }
                });
                if !narrow {
                    source_picker(ui, source, devices, 220.0);
                }
            });
        });
        if narrow {
            source_picker(ui, source, devices, ui.available_width().min(320.0));
        }
        if !source.enabled {
            return;
        }
        ui.add_space(4.0);

        // --- Meter, then mute + fader + value; side by side when there's room ---
        let id = egui::Id::new(("source_meter", source.id.as_str()));
        let draw_meter = |ui: &mut egui::Ui, meters: &mut SourceMeters, muted: bool| {
            // Muted: show what the source delivers, dimmed, so you can see the mic
            // is live without it being recorded. Otherwise what goes into the clip.
            let state = if muted { &mut meters.input } else { &mut meters.output };
            let style = meter::Style { bar_h: 6.0, gap: 2.0, scale: false, dimmed: muted };
            if meter::stereo(ui, id, state, &style) {
                state.reset_clip();
            }
        };
        let controls = |ui: &mut egui::Ui, source: &mut AudioSourceCfg, fader_w: f32| {
            let mute = egui::Button::new(RichText::new("M").strong().color(if source.muted { Color32::BLACK } else { ui.visuals().text_color() }))
                .fill(if source.muted { meter::YELLOW } else { ui.visuals().widgets.inactive.weak_bg_fill })
                .min_size(vec2(26.0, 22.0));
            if ui.add(mute).on_hover_text(if source.muted { "Unmute" } else { "Mute: keep the source but record silence" }).clicked() {
                source.muted = !source.muted;
            }
            ui.spacing_mut().slider_width = fader_w;
            let slider = egui::Slider::new(&mut source.volume_db, FADER_MIN_DB..=FADER_MAX_DB)
                .show_value(false)
                .clamping(egui::SliderClamping::Always);
            let r = ui.add(slider).on_hover_text("Volume (gain). Double-click to reset to 0 dB.");
            // A slider only senses drags, so it never reports a double-click itself.
            let double = ui.input(|i| i.pointer.button_double_clicked(egui::PointerButton::Primary));
            if double && r.hovered() {
                source.volume_db = 0.0;
            }
            // Type an exact value: drag or double-click the number.
            ui.add(
                egui::DragValue::new(&mut source.volume_db)
                    .range(FADER_MIN_DB..=FADER_MAX_DB)
                    .speed(0.1)
                    .fixed_decimals(1)
                    .custom_formatter(|v, _| if v <= (FADER_MIN_DB + 0.05) as f64 { "-∞".to_owned() } else { format!("{v:+.1}") })
                    .suffix(" dB"),
            )
            .on_hover_text("Drag or click to type a value");
        };
        let readout = |ui: &mut egui::Ui, meters: &SourceMeters, muted: bool| {
            let state = if muted { &meters.input } else { &meters.output };
            meter::fixed_label(ui, 46.0, &state.readout(), state.readout_color(ui.visuals()), "Highest peak in the last 3 seconds (dBFS)");
        };
        if narrow {
            ui.horizontal(|ui| {
                let w = ui.available_width() - 46.0 - ui.spacing().item_spacing.x;
                ui.allocate_ui(vec2(w, 14.0), |ui| draw_meter(ui, meters, source.muted));
                readout(ui, meters, source.muted);
            });
            ui.horizontal(|ui| {
                let fader_w = (ui.available_width() - 26.0 - 90.0 - 2.0 * ui.spacing().item_spacing.x).max(80.0);
                controls(ui, source, fader_w);
            });
        } else {
            ui.horizontal(|ui| {
                let controls_w = 26.0 + 180.0 + 84.0 + 3.0 * ui.spacing().item_spacing.x;
                let w = ui.available_width() - controls_w - 46.0 - 12.0 - 2.0 * ui.spacing().item_spacing.x;
                ui.allocate_ui(vec2(w, 14.0), |ui| draw_meter(ui, meters, source.muted));
                readout(ui, meters, source.muted);
                ui.add_space(12.0);
                controls(ui, source, 180.0);
            });
        }

        // --- Where it goes, in plain words ---
        ui.add_space(2.0);
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut source.in_mix, "In the clip")
                .on_hover_text("Part of what your clips sound like when you play or share them.");
            ui.checkbox(&mut source.own_track, "Separate track for editing")
                .on_hover_text("Also kept on its own, so you can change its volume later in the editor.");
            if let SourceKind::Desktop { exclude_apps } = &mut source.kind {
                ui.add_enabled(has_app_sources, egui::Checkbox::new(exclude_apps, "Leave out apps added below"))
                    .on_hover_text("Apps you add as their own source won't also be heard here, so nothing plays twice.")
                    .on_disabled_hover_text("Add an app as its own source to use this.");
            }
            if matches!(source.kind, SourceKind::Microphone { .. }) {
                ui.add_space(8.0);
                let on = listening.is_some();
                let r = ui.selectable_label(on, "🎧 Listen").on_hover_text(
                    "Hear this microphone live, at its volume, to check it works and how it sounds. Use headphones: speakers would feed back into it.",
                );
                if r.clicked() {
                    action = CardAction::Listen;
                }
                if let Some(Some(ms)) = listening {
                    ui.weak(format!("≈ {ms:.0} ms delay")).on_hover_text("From the microphone to your ears, as the devices report it (their hardware adds a little).");
                }
            }
        });
        if !source.in_mix && !source.own_track {
            ui.colored_label(ui.visuals().warn_fg_color, "This source isn't recorded anywhere: tick at least one box.");
        }
    });
    action
}

/// What a source captures: the device dropdown for mics, the app name for apps.
fn source_picker(ui: &mut egui::Ui, source: &mut AudioSourceCfg, audio: &capture::audio::AudioDevices, width: f32) {
    use capture::audio::DEFAULT_DEVICE;
    match &mut source.kind {
        SourceKind::Microphone { device } => {
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
            egui::ComboBox::from_id_salt(("mic", source.id.as_str()))
                .width(width)
                .truncate()
                .selected_text(selected)
                .show_ui(ui, |ui| {
                    ui.selectable_value(device, DEFAULT_DEVICE.to_owned(), default_label);
                    ui.separator();
                    for d in &audio.inputs {
                        ui.selectable_value(device, d.id.clone(), &d.name);
                    }
                });
        }
        SourceKind::Desktop { .. } => {
            ui.weak("Everything your computer plays");
        }
        SourceKind::App { app_name, .. } => {
            ui.weak(app_name.as_str());
        }
    }
}

fn status_badge(ui: &mut egui::Ui, status: SourceStatus) {
    let (text, color, tip) = match status {
        SourceStatus::Live | SourceStatus::Off => return,
        SourceStatus::WaitingForApp => {
            let tip = if cfg!(target_os = "linux") {
                "Starts by itself as soon as the app plays sound."
            } else {
                "Starts by itself as soon as the app opens."
            };
            ("waiting for app", ui.visuals().weak_text_color(), tip)
        }
        SourceStatus::Unavailable => ("unavailable", ui.visuals().warn_fg_color, "The device isn't connected, or couldn't be opened."),
    };
    ui.label(RichText::new(text).size(12.0).color(color)).on_hover_text(tip);
}

/// A rounded panel around one source.
/// A two-way switch: equal halves, the chosen one filled. Returns both
/// halves' responses.
pub(crate) fn segmented(ui: &mut egui::Ui, labels: [&str; 2], chosen: usize) -> (egui::Response, egui::Response) {
    let gap = 4.0;
    let w = ((ui.available_width() - gap) / 2.0).max(60.0);
    let mut out = Vec::new();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        for (i, label) in labels.iter().enumerate() {
            let on = i == chosen;
            let text = RichText::new(*label).size(14.0).color(if on { Color32::WHITE } else { ui.visuals().text_color() });
            let mut b = egui::Button::new(text).min_size(egui::vec2(w, 32.0)).corner_radius(6);
            b = if on { b.fill(ACCENT.gamma_multiply(0.8)) } else { b.fill(ui.visuals().extreme_bg_color) };
            out.push(ui.add(b));
        }
    });
    let second = out.pop().unwrap();
    (out.pop().unwrap(), second)
}

/// An on/off switch.
pub(crate) fn toggle(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let size = egui::vec2(36.0, 20.0);
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    let t = ui.ctx().animate_bool_with_time(response.id, *on, 0.12);
    let off_fill = ui.visuals().widgets.inactive.bg_fill;
    let fill = Color32::from_rgb(
        egui::lerp(off_fill.r() as f32..=ACCENT.r() as f32, t) as u8,
        egui::lerp(off_fill.g() as f32..=ACCENT.g() as f32, t) as u8,
        egui::lerp(off_fill.b() as f32..=ACCENT.b() as f32, t) as u8,
    );
    let r = rect.height() / 2.0;
    ui.painter().rect_filled(rect, r, fill);
    let x = egui::lerp(rect.left() + r..=rect.right() - r, t);
    ui.painter().circle_filled(egui::pos2(x, rect.center().y), r - 3.0, Color32::WHITE);
    response
}

/// A small coloured label in a pill, its own height whatever the row's.
pub(crate) fn status_tag(ui: &mut egui::Ui, text: &str, color: Color32) {
    let galley = ui.painter().layout_no_wrap(text.to_owned(), egui::FontId::proportional(11.5), color);
    let size = galley.size() + egui::vec2(14.0, 4.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect_filled(rect, size.y / 2.0, color.gamma_multiply(0.18));
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
}

/// A remove button drawn as an ✕ (the font has no ✕): grey, red under the pointer.
pub(crate) fn remove_button(ui: &mut egui::Ui) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(26.0, 26.0), Sense::click());
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    let hovered = response.hovered() && ui.is_enabled();
    let red = Color32::from_rgb(235, 87, 87);
    if hovered {
        ui.painter().rect_filled(rect, 6.0, red.gamma_multiply(0.2));
    }
    let color = if !ui.is_enabled() {
        ui.visuals().weak_text_color().gamma_multiply(0.5)
    } else if hovered {
        red
    } else {
        ui.visuals().weak_text_color()
    };
    let c = rect.center();
    let r = 4.5;
    let stroke = egui::Stroke::new(1.6, color);
    ui.painter().line_segment([c + egui::vec2(-r, -r), c + egui::vec2(r, r)], stroke);
    ui.painter().line_segment([c + egui::vec2(-r, r), c + egui::vec2(r, -r)], stroke);
    response
}

pub(crate) fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style()).inner_margin(egui::Margin::same(12)).corner_radius(8).show(ui, |ui| {
        ui.set_width(ui.available_width());
        add(ui);
    });
}

pub(crate) fn from_db(db: f32) -> f32 {
    if db <= FADER_MIN_DB + 0.05 { 0.0 } else { 10f32.powf(db / 20.0) }
}
