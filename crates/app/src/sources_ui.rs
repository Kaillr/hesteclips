//! The Sources page: what goes into your clips. Today that's audio — your mic,
//! everything the computer plays, or single apps — each with a live meter and a
//! volume fader, plus the clip's mix meter. Video sources (webcam, overlays)
//! will live here too.
//!
//! Deliberately says nothing about "tracks": a source is either part of what the
//! clip sounds like ("In the clip"), kept separately for editing, or both.

use std::time::{Duration, Instant};

use capture::mixer::{Channel, SourceStatus};
use egui::{Color32, Rect, RichText, Sense, Stroke, vec2};

use crate::App;
use crate::settings::{AudioSourceCfg, SourceKind};

/// Fader range in dB.
const FADER_MIN_DB: f32 = -60.0;
const FADER_MAX_DB: f32 = 12.0;
/// Meter range in dBFS.
const METER_FLOOR_DB: f32 = -60.0;
/// How long a peak marker / clip warning stays lit.
const PEAK_HOLD: Duration = Duration::from_millis(1500);
const CLIP_HOLD: Duration = Duration::from_secs(3);

const METER_GREEN: Color32 = Color32::from_rgb(70, 190, 110);
const METER_YELLOW: Color32 = Color32::from_rgb(230, 190, 60);
const METER_RED: Color32 = Color32::from_rgb(230, 70, 60);

/// Smoothed meter state for one source (or the mix), kept between frames.
#[derive(Default, Clone)]
pub(crate) struct MeterView {
    /// Displayed level (dB), falling smoothly.
    level_db: f32,
    peak_db: f32,
    peak_at: Option<Instant>,
    clipped_at: Option<Instant>,
}

impl MeterView {
    fn update(&mut self, peak: f32, now: Instant, dt: f32) {
        let db = to_db(peak);
        // Instant rise, ~20 dB/s fall: readable without hiding transients.
        self.level_db = if db > self.level_db { db } else { (self.level_db - 20.0 * dt).max(db) };
        if db >= self.peak_db || self.peak_at.is_none_or(|t| now - t > PEAK_HOLD) {
            self.peak_db = db;
            self.peak_at = Some(now);
        }
        if peak >= 0.999 {
            self.clipped_at = Some(now);
        }
    }

    fn clipping(&self, now: Instant) -> bool {
        self.clipped_at.is_some_and(|t| now - t < CLIP_HOLD)
    }
}

/// UI state for the Sources page.
#[derive(Default)]
pub(crate) struct SourcesView {
    meters: std::collections::HashMap<String, MeterView>,
    master: MeterView,
    limiting_at: Option<Instant>,
    last_frame: Option<Instant>,
    /// Running apps for the "Add app" menu, refreshed when it opens.
    apps: Vec<capture::Device>,
    renaming: Option<String>,
}

impl App {
    pub(crate) fn sources_page(&mut self, ui: &mut egui::Ui) {
        let now = Instant::now();
        let dt = self.sources_view.last_frame.map_or(0.0, |t| (now - t).as_secs_f32()).min(0.2);
        self.sources_view.last_frame = Some(now);
        // Meters move: keep repainting while this page is open.
        ui.ctx().request_repaint_after(Duration::from_millis(33));

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.set_max_width(760.0);
            ui.add_space(12.0);
            ui.label(RichText::new("Audio").size(16.0).strong());
            ui.weak("Set levels here before you play; the meters are live.");
            ui.add_space(8.0);

            let mut remove = None;
            let mut move_up = None;
            let n = self.settings.audio_sources.len();
            for i in 0..n {
                let channel = self.live_audio.channel(&self.settings.audio_sources[i].id);
                let (peak, _) = channel.meter.take();
                let id = self.settings.audio_sources[i].id.clone();
                let view = self.sources_view.meters.entry(id).or_default();
                view.update(peak, now, dt);
                let view = view.clone();
                let has_app_sources = self.settings.audio_sources.iter().any(|s| matches!(s.kind, SourceKind::App { .. }));
                let action = source_card(
                    ui,
                    &mut self.settings.audio_sources[i],
                    &channel,
                    &view,
                    now,
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
                ui.add_space(6.0);
            }
            if let Some(i) = remove {
                self.settings.audio_sources.remove(i);
            }
            if let Some(i) = move_up {
                self.settings.audio_sources.swap(i, i - 1);
            }

            ui.add_space(4.0);
            self.add_source_buttons(ui);

            ui.add_space(16.0);
            self.master_meter(ui, now, dt);

            if self.rec_state != crate::RecState::Idle {
                ui.add_space(10.0);
                ui.weak("Volume changes apply right away. Adding or removing sources applies the next time you start the buffer or a recording.");
            }
        });
        self.live_audio.set_limiter(self.settings.limiter);
    }

    fn add_source_buttons(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("➕ Microphone").clicked() {
                let n = self.settings.audio_sources.iter().filter(|s| matches!(s.kind, SourceKind::Microphone { .. })).count();
                let name = if n == 0 { "Microphone".to_owned() } else { format!("Microphone {}", n + 1) };
                self.settings.audio_sources.push(AudioSourceCfg::new(
                    &name,
                    SourceKind::Microphone { device: capture::audio::DEFAULT_DEVICE.into() },
                ));
            }
            let menu = ui.menu_button("➕ App", |ui| {
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
            if !has_desktop && ui.button("➕ Desktop sound").clicked() {
                self.settings
                    .audio_sources
                    .push(AudioSourceCfg::new("Desktop", SourceKind::Desktop { exclude_apps: true }));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("⟳ Refresh devices").clicked() {
                    self.refresh_audio_devices();
                }
            });
        });
    }

    fn master_meter(&mut self, ui: &mut egui::Ui, now: Instant, dt: f32) {
        let (peak, _) = self.live_audio.master.take();
        self.sources_view.master.update(peak, now, dt);
        if self.live_audio.take_reduction_db() > 0.5 {
            self.sources_view.limiting_at = Some(now);
        }
        let limiting = self.sources_view.limiting_at.is_some_and(|t| now - t < CLIP_HOLD);
        let view = self.sources_view.master.clone();
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("🔊 Clip audio").size(15.0).strong())
                    .on_hover_text("Everything marked \"In the clip\", mixed — what people hear when you share a clip.");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.checkbox(&mut self.settings.limiter, "Prevent clipping")
                        .on_hover_text("Turns the mix down for a moment when sources add up too loud, instead of distorting.");
                });
            });
            meter(ui, &view, now, true);
            let hint = if view.clipping(now) {
                Some((METER_RED, "Too loud — turn some sources down."))
            } else if limiting {
                Some((METER_YELLOW, "Loud peaks are being held back. Turn sources down a little for the cleanest sound."))
            } else {
                None
            };
            if let Some((color, text)) = hint {
                ui.colored_label(color, text);
            }
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

#[allow(clippy::too_many_arguments)]
fn source_card(
    ui: &mut egui::Ui,
    source: &mut AudioSourceCfg,
    channel: &Channel,
    view: &MeterView,
    now: Instant,
    devices: &capture::audio::AudioDevices,
    has_app_sources: bool,
    renaming: &mut Option<String>,
) -> CardAction {
    let mut action = CardAction::None;
    card(ui, |ui| {
        {
            // --- Header: on/off, name, what it captures, status, menu ---
            ui.horizontal(|ui| {
                ui.checkbox(&mut source.enabled, "").on_hover_text(if source.enabled { "Turn off" } else { "Turn on" });
                if renaming.as_deref() == Some(source.id.as_str()) {
                    let r = ui.add(egui::TextEdit::singleline(&mut source.name).desired_width(180.0));
                    r.request_focus();
                    if r.lost_focus() {
                        *renaming = None;
                    }
                } else {
                    let r = ui.add(egui::Label::new(RichText::new(format!("{} {}", source.icon(), source.name)).size(15.0).strong()).sense(Sense::click()));
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
                    source_picker(ui, source, devices);
                });
            });
            if source.enabled {
            // --- Meter + fader + mute ---
            ui.horizontal(|ui| {
                let mute_label = if source.muted { "🔇" } else { "🔊" };
                if ui
                    .add(egui::Button::new(mute_label).selected(source.muted).min_size(vec2(28.0, 24.0)))
                    .on_hover_text(if source.muted { "Unmute" } else { "Mute" })
                    .clicked()
                {
                    source.muted = !source.muted;
                }
                // Fixed widths so every card lines up, whatever its value label.
                let fader_w = 220.0;
                let meter_w = (ui.available_width() - fader_w - 90.0).max(120.0);
                ui.allocate_ui(vec2(meter_w, 24.0), |ui| meter(ui, view, now, false));
                ui.spacing_mut().slider_width = fader_w;
                let slider = egui::Slider::new(&mut source.volume_db, FADER_MIN_DB..=FADER_MAX_DB)
                    .show_value(false)
                    .clamping(egui::SliderClamping::Always);
                let r = ui.add(slider).on_hover_text("Volume. Double-click to reset.");
                if r.double_clicked() {
                    source.volume_db = 0.0;
                }
                ui.add_sized(vec2(70.0, 20.0), egui::Label::new(RichText::new(format_db(source.volume_db)).monospace()));
            });

            // --- Where it goes, in plain words ---
            ui.horizontal(|ui| {
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
                ui.colored_label(ui.visuals().warn_fg_color, "This source isn't recorded anywhere — tick at least one box.");
            }
            }
        }
    });
    action
}

/// What a source captures: the device dropdown for mics, the app name for apps.
fn source_picker(ui: &mut egui::Ui, source: &mut AudioSourceCfg, audio: &capture::audio::AudioDevices) {
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
                .width(230.0)
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

/// A horizontal level meter: green → yellow → red, a held peak line, and a
/// clip light on the right.
fn meter(ui: &mut egui::Ui, view: &MeterView, now: Instant, big: bool) {
    let h = if big { 18.0 } else { 10.0 };
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, if big { 34.0 } else { 24.0 }), Sense::hover());
    let clip_w = 10.0;
    let bar = Rect::from_min_size(rect.left_center() - vec2(0.0, h / 2.0), vec2(w - clip_w - 4.0, h));
    let p = ui.painter();
    p.rect_filled(bar, 3.0, ui.visuals().extreme_bg_color);
    let x_of = |db: f32| bar.left() + bar.width() * ((db - METER_FLOOR_DB) / -METER_FLOOR_DB).clamp(0.0, 1.0);
    let lvl_x = x_of(view.level_db);
    // Zones: green up to -18 dB, yellow to -6, red above.
    for (from, to, color) in [(METER_FLOOR_DB, -18.0, METER_GREEN), (-18.0, -6.0, METER_YELLOW), (-6.0, 0.0, METER_RED)] {
        let (x0, x1) = (x_of(from), x_of(to).min(lvl_x));
        if x1 > x0 {
            p.rect_filled(Rect::from_x_y_ranges(x0..=x1, bar.y_range()), 2.0, color);
        }
    }
    if view.peak_db > METER_FLOOR_DB {
        let x = x_of(view.peak_db);
        p.line_segment([egui::pos2(x, bar.top()), egui::pos2(x, bar.bottom())], Stroke::new(2.0, ui.visuals().strong_text_color()));
    }
    if big {
        // dB scale under the mix meter.
        for db in [-48.0, -36.0, -24.0, -12.0, -6.0, 0.0] {
            let x = x_of(db);
            p.text(
                egui::pos2(x, bar.bottom() + 2.0),
                egui::Align2::CENTER_TOP,
                format!("{db:.0}"),
                egui::FontId::proportional(10.0),
                ui.visuals().weak_text_color(),
            );
        }
    }
    let clip = Rect::from_min_size(egui::pos2(bar.right() + 4.0, bar.top()), vec2(clip_w, h));
    let lit = view.clipping(now);
    p.rect_filled(clip, 2.0, if lit { METER_RED } else { ui.visuals().extreme_bg_color });
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

fn format_db(db: f32) -> String {
    if db <= FADER_MIN_DB + 0.05 { "  -∞ dB".to_owned() } else { format!("{db:+5.1} dB") }
}

fn to_db(x: f32) -> f32 {
    if x <= 1e-6 { -120.0 } else { 20.0 * x.log10() }
}

pub(crate) fn from_db(db: f32) -> f32 {
    if db <= FADER_MIN_DB + 0.05 { 0.0 } else { 10f32.powf(db / 20.0) }
}
