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
use crate::settings::{AudioSourceCfg, SourceKind};

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
        // Meters move: keep repainting while this page is open.
        ui.ctx().request_repaint_after(Duration::from_millis(16));

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
        // The mix first: it's what people hear, and where clipping matters most.
        self.master_strip(ui, now, dt);
        ui.add_space(18.0);

        ui.horizontal(|ui| {
            ui.label(RichText::new("Sources").size(16.0).strong());
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
            let meters = self.sources_view.meters.entry(id).or_default();
            meters.input.update(channel.input.take(), now, dt);
            meters.output.update(channel.meter.take(), now, dt);
            let action = source_card(
                ui,
                &mut self.settings.audio_sources[i],
                &channel,
                meters,
                &self.audio,
                has_app_sources,
                &mut self.sources_view.renaming,
            );
            match action {
                CardAction::Remove => remove = Some(i),
                CardAction::MoveUp if i > 0 => move_up = Some(i),
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

        self.add_source_buttons(ui);

        if self.rec_state != crate::RecState::Idle {
            ui.add_space(10.0);
            ui.weak("Volume changes apply right away. Adding or removing sources applies the next time you start the buffer or a recording.");
        }
        ui.add_space(16.0);
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
                    ui.weak("No apps found.");
                }
            });
            if menu.response.clicked() {
                self.sources_view.apps = list_apps();
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
}

fn source_card(
    ui: &mut egui::Ui,
    source: &mut AudioSourceCfg,
    channel: &Channel,
    meters: &mut SourceMeters,
    devices: &capture::audio::AudioDevices,
    has_app_sources: bool,
    renaming: &mut Option<String>,
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
            if r.double_clicked() {
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
            ("waiting for app", ui.visuals().weak_text_color(), "Starts by itself as soon as the app opens.")
        }
        SourceStatus::Unavailable => ("unavailable", ui.visuals().warn_fg_color, "The device isn't connected, or couldn't be opened."),
    };
    ui.label(RichText::new(text).size(12.0).color(color)).on_hover_text(tip);
}

/// A rounded panel around one source.
fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style()).inner_margin(egui::Margin::same(12)).corner_radius(8).show(ui, |ui| {
        ui.set_width(ui.available_width());
        add(ui);
    });
}

fn list_apps() -> Vec<capture::Device> {
    #[cfg(target_os = "macos")]
    {
        capture::macos::list_apps()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Vec::new()
    }
}

pub(crate) fn from_db(db: f32) -> f32 {
    if db <= FADER_MIN_DB + 0.05 { 0.0 } else { 10f32.powf(db / 20.0) }
}
