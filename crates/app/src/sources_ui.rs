//! The Sources page: what goes into your clips. The picture: a live preview
//! of the video, its sources placed on it (`canvas_ui`), and under it a card
//! for each (the screen or your games and apps, the webcam), the one in front
//! first, opened for its settings and dragged to put it in front or behind.
//! The sound: your mic, everything the computer plays, or single apps, each
//! with a live meter and a volume fader, plus the clip's mix meter.
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
/// Picture and sound side by side from this page width.
pub(crate) const TWO_COLUMNS: f32 = 860.0;

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
    /// The clip mix's note last shown (too loud, limiting), kept while it fades out.
    last_note: Option<(egui::Color32, &'static str)>,
    /// The last preview frame and what it was of (picture, size, rate,
    /// webcam): shown again at once when the page opens on the same, while
    /// the preview's capture starts (that takes a moment).
    last_preview: Option<(PreviewSetup, std::sync::Arc<capture::preview::PreviewFrame>)>,
    /// The crop's page is open (in Sources' place), and what of the crop is
    /// held there.
    pub(crate) crop_open: bool,
    crop_drag: Option<(crate::webcam_ui::Handle, egui::Rect, egui::Pos2)>,
    /// The picture's sources whose cards are open, showing their settings.
    pub(crate) open_cards: Vec<crate::canvas_ui::Layer>,
    /// The sources whose Placement numbers are open, under that row of
    /// their cards; and one to bring into view (opened from its menu).
    pub(crate) placement_open: Vec<crate::canvas_ui::Layer>,
    pub(crate) placement_shown: Option<crate::canvas_ui::Layer>,
    /// A card dragged by its handle to put it in front or behind: which,
    /// how far below its top it was taken hold of, and how the sources were
    /// before (to undo it).
    reorder: Option<(crate::canvas_ui::Layer, f32, crate::canvas_ui::Arrangement)>,
    /// The cards' order as last shown, and when it last changed: they slide
    /// to their new places for a moment after.
    card_order: (Vec<crate::canvas_ui::Layer>, Option<Instant>),
}

impl SourcesView {
    /// Dev aid: the cards named open (`screen`, `webcam`), their Placement
    /// numbers (`placement`), or the crop's page (`crop`).
    pub(crate) fn open_dev(&mut self, names: &str) {
        use crate::canvas_ui::Layer;
        for name in names.split(',').map(str::trim) {
            match name {
                "screen" => self.open_cards.push(Layer::Screen),
                "webcam" => self.open_cards.push(Layer::Webcam),
                "placement" => self.placement_open.extend([Layer::Screen, Layer::Webcam]),
                "crop" => self.crop_open = true,
                _ => {}
            }
        }
    }
}

/// What a preview shows: the picture, its height and shape, the frame rate,
/// the webcam, and whether it's all of the screen, uncropped (to crop on).
pub(crate) type PreviewSetup = (capture::VideoSource, capture::VideoSize, u32, Option<capture::webcam::Webcam>, bool);


impl App {
    /// Sources, with the screen's card open (its own page, not a crop's):
    /// where it's put in the video.
    pub(crate) fn show_screen_card(&mut self) {
        use crate::canvas_ui::Layer;
        self.page = crate::Page::Sources;
        self.sources_view.crop_open = false;
        self.webcam_view.crop = None;
        if !self.sources_view.open_cards.contains(&Layer::Screen) {
            self.sources_view.open_cards.push(Layer::Screen);
        }
    }

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

        // Adjusting the camera, or a crop: its own page, in Sources' place.
        if self.camera_panel.is_some() {
            self.camera_page(ui);
            return;
        }
        if self.webcam_view.crop.is_some() {
            self.webcam_crop_page(ui);
            return;
        }
        if self.sources_view.crop_open {
            self.screen_crop_page(ui);
            return;
        }
        // Where the sources are, undone and done again from the keyboard (not
        // while typing, or with a menu open).
        if !ui.ctx().egui_wants_keyboard_input() && !crate::ui_kit::overlay_open(ui.ctx()) {
            let (redo, undo) = ui.input_mut(|i| (i.consume_key(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, egui::Key::Z), i.consume_key(egui::Modifiers::COMMAND, egui::Key::Z)));
            if undo {
                self.undo_arrangement();
            }
            if redo {
                self.redo_arrangement();
            }
        }
        self.page_head(ui, "Sources", |_, _| {});
        // The preview stays in view: what's under it scrolls (each column on
        // its own), so the cards' settings show on it as they change.
        crate::ui_kit::arrive(ui, |ui| {
            // The headings' tops level with the other pages' first heading
            // (these are bigger, in a taller row).
            ui.add_space(4.0);
            let (w, h) = (ui.available_width(), ui.available_height());
            // Picture on the left, sound on the right, when there's room;
            // one above the other when there isn't.
            if w >= TWO_COLUMNS && capture::preview::AVAILABLE {
                let col = (w - 28.0) / 2.0;
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 28.0;
                    ui.allocate_ui_with_layout(egui::vec2(col, h), egui::Layout::top_down(egui::Align::Min), |ui| {
                        ui.set_width(col);
                        let frame = self.picture_top(ui);
                        crate::ui_kit::scroll(ui, "sources_cards", |ui| {
                            self.layer_cards(ui, frame.as_deref());
                            ui.add_space(28.0);
                        });
                    });
                    ui.allocate_ui_with_layout(egui::vec2(col, h), egui::Layout::top_down(egui::Align::Min), |ui| {
                        ui.set_width(col);
                        self.sound_head(ui);
                        crate::ui_kit::scroll(ui, "sources_sound", |ui| {
                            self.sound_column(ui, now, dt);
                            ui.add_space(28.0);
                        });
                    });
                });
            } else {
                ui.vertical(|ui| {
                    ui.set_width(w);
                    let frame = capture::preview::AVAILABLE.then(|| self.picture_top(ui)).flatten();
                    crate::ui_kit::scroll(ui, "sources_page", |ui| {
                        if capture::preview::AVAILABLE {
                            self.layer_cards(ui, frame.as_deref());
                            ui.add_space(24.0);
                        }
                        self.sound_head(ui);
                        self.sound_column(ui, now, dt);
                        ui.add_space(28.0);
                    });
                });
            }
        });
        self.live_audio.set_limiter(self.settings.limiter);
    }

    /// What you see: the preview of the video, under its heading (its
    /// sources' cards go under it, see `layer_cards`). The frame shown.
    fn picture_top(&mut self, ui: &mut egui::Ui) -> Option<std::sync::Arc<capture::preview::PreviewFrame>> {
        capture::preview::request();
        // The capture's newest frame; while it's starting, the last one of the
        // same picture (the page looks as it did straight away).
        let setup = self.video_preview.as_ref().map(|(w, _)| w.clone());
        let frame = match capture::preview::latest() {
            Some(f) => {
                if let Some(setup) = setup {
                    self.sources_view.last_preview = Some((setup, f.clone()));
                }
                Some(f)
            }
            None => self.sources_view.last_preview.as_ref().filter(|(was, _)| Some(was) == setup.as_ref()).map(|(_, f)| f.clone()),
        };
        self.update_preview_texture(ui.ctx(), frame.as_deref());
        // Which listed apps are open, for their status; cheap, so every 2 s.
        let apps_mode = matches!(self.settings.capture, CaptureTarget::Apps { .. });
        if apps_mode && self.sources_view.apps_checked.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
            self.sources_view.apps_checked = Some(Instant::now());
            self.windowed_apps = capture::list_windowed_apps();
        }
        // What's recorded; while a preview of another size starts, the size
        // it will be (the button stays, and its choices stay open).
        let size = frame.as_ref().map(|f| f.recorded).or_else(|| capture::frame_size(&self.video_source(), self.settings.video_size()));
        let size_text = size.map_or_else(|| format!("{} fps", self.settings.fps), |(w, h)| format!("{w}×{h} at {} fps", self.settings.fps));
        column_head(ui, "Picture", |ui| {
            self.size_button(ui, &size_text);
        });
        self.preview_picture(ui, frame.as_deref());
        crate::ui_kit::gap(ui, crate::ui_kit::STACK_GAP);
        frame
    }

    /// What you hear's heading, with Refresh.
    fn sound_head(&mut self, ui: &mut egui::Ui) {
        use crate::ui_kit::Btn;
        column_head(ui, "Sound", |ui| {
            if Btn::ghost("Refresh").with_icon(egui_phosphor::regular::ARROWS_CLOCKWISE).small().tip("Look again for microphones you've plugged in").show(ui).clicked() {
                self.refresh_audio_devices();
                self.toast_icon(egui_phosphor::regular::ARROWS_CLOCKWISE, "Looked for microphones again");
            }
        });
    }

    /// What you hear (under `sound_head`): the clip's mix, then each
    /// source, then adding one.
    fn sound_column(&mut self, ui: &mut egui::Ui, now: Instant, dt: f32) {
        let pal = crate::theme::pal(ui);
        // The mix first: it's what people hear, and where it gets too loud.
        self.master_strip(ui, now, dt);
        crate::ui_kit::gap(ui, crate::ui_kit::STACK_GAP);

        let mut remove = None;
        let mut move_up = None;
        let n = self.settings.audio_sources.len();
        let has_app_sources = self.settings.audio_sources.iter().any(|s| matches!(s.kind, SourceKind::App { .. }));
        let locked = (!self.rec_state.idle()).then(|| self.stop_first());
        for i in 0..n {
            let channel = self.live_audio.channel(&self.settings.audio_sources[i].id);
            let id = self.settings.audio_sources[i].id.clone();
            let id_for_listen = id.clone();
            let note = self.added_note(&self.settings.audio_sources[i]);
            let meters = self.sources_view.meters.entry(id).or_default();
            meters.input.update(channel.input.take(), now, dt);
            meters.output.update(channel.meter.take(), now, dt);
            let listening = self.listening.as_ref().filter(|l| l.source == id_for_listen).map(|l| l.monitor.latency_ms());
            let action = source_card(ui, &mut self.settings.audio_sources[i], &channel, meters, &self.audio, has_app_sources, &mut self.sources_view.renaming, listening, locked.as_deref(), note, i == 0, &mut self.app_icons);
            match action {
                CardAction::Remove => remove = Some(i),
                CardAction::MoveUp if i > 0 => move_up = Some(i),
                CardAction::Listen => self.toggle_listening(i),
                CardAction::Locked => self.toast_icon(egui_phosphor::regular::LOCK_SIMPLE, locked.as_deref().unwrap_or_default().trim_end_matches('.').to_owned()),
                _ => {}
            }
            // Volume applies live, even mid-recording.
            let s = &self.settings.audio_sources[i];
            channel.set_volume(from_db(s.volume_db), s.muted);
            channel.set_denoise(s.noise_reduction.into());
            channel.set_mono(s.mono);
            crate::ui_kit::gap(ui, crate::ui_kit::STACK_GAP);
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

        let capturing = !self.rec_state.idle();
        crate::ui_kit::reveal(ui, "sources_locked", capturing, |ui| {
            ui.add_space(12.0);
            let mic = if cfg!(target_os = "linux") { ", noise removal" } else { ", noise removal and which microphone" };
            let stop = format!("Stop {}", self.rec_state.words());
            let note = if capture::SOURCES_WHILE_CAPTURING {
                format!("Volume, mute{mic} change right away, as do sources added, turned off or removed. {stop} to move one or change where it goes.")
            } else {
                format!("Volume, mute{mic} change right away. {stop} to add or remove sources.")
            };
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(crate::theme::ic(egui_phosphor::regular::LOCK_SIMPLE, 12.0).color(pal.text3));
                ui.add(egui::Label::new(crate::theme::text(note, 12.5, crate::theme::W::Regular).color(pal.text3)).wrap());
            });
        });
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
        // Which sources are recorded is set when capture starts, but where
        // one can be added to it as it runs (`capture::SOURCES_WHILE_CAPTURING`).
        let idle = self.rec_state.idle();
        let can_add = idle || capture::SOURCES_WHILE_CAPTURING;
        let stop_first = self.stop_first();
        let stop_first = stop_first.as_str();
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let mic = Btn::normal("Microphone").with_icon(icon::PLUS).small().tip(if can_add { "Add a microphone or audio interface" } else { stop_first }).show(ui);
            if mic.clicked() {
                if can_add {
                    // Captured at once: the default microphone as it is now
                    // (the list is looked at again when capturing starts).
                    if !idle {
                        self.refresh_audio_devices();
                    }
                    let n = self.settings.audio_sources.iter().filter(|s| matches!(s.kind, SourceKind::Microphone { .. })).count();
                    let name = if n == 0 { "Microphone".to_owned() } else { format!("Microphone {}", n + 1) };
                    self.settings.audio_sources.push(AudioSourceCfg::new(&name, SourceKind::Microphone { device: capture::audio::DEFAULT_DEVICE.into() }));
                } else {
                    self.toast_icon(icon::LOCK_SIMPLE, stop_first.trim_end_matches('.'));
                }
            }
            let app = Btn::normal("App").with_icon(icon::PLUS).trail(icon::CARET_DOWN).small().id("add_app_source").tip(if can_add { "Record one app on its own: a game, Discord, music" } else { stop_first }).show(ui);
            if app.clicked() {
                if can_add {
                    self.sources_view.apps = capture::list_apps();
                } else {
                    self.toast_icon(icon::LOCK_SIMPLE, stop_first.trim_end_matches('.'));
                    egui::Popup::close_id(ui.ctx(), app.id.with("menu"));
                }
            }
            if can_add {
                crate::ui_kit::menu_popup(&app, true, 260.0, |ui| {
                    crate::ui_kit::menu_head(ui, "Record one app on its own");
                    let taken: Vec<String> = self.settings.audio_sources.iter().filter_map(|s| match &s.kind {
                        SourceKind::App { bundle_id, .. } => Some(bundle_id.clone()),
                        _ => None,
                    }).collect();
                    egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                        for app in self.sources_view.apps.clone() {
                            let added = taken.contains(&app.id);
                            let tex = self.app_icons.get(ui.ctx(), &app.id).map(|t| t.id());
                            let item = Item::new(app.name.clone()).image(tex).enabled(!added);
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
                if can_add {
                    self.settings.audio_sources.push(AudioSourceCfg::new("Desktop sound", SourceKind::Desktop { exclude_apps: true }));
                } else {
                    self.toast_icon(icon::LOCK_SIMPLE, stop_first.trim_end_matches('.'));
                }
            }
        });
    }

    /// Why something that's set when capture starts is locked now.
    pub(crate) fn stop_first(&self) -> String {
        format!("Stop {} to change this.", if self.rec_state.idle() { "clipping".to_owned() } else { self.rec_state.words() })
    }

    /// What a source added while capturing doesn't have yet, said on its
    /// card, and whether that leaves something out (a warning). The file's
    /// tracks are set when the capture starts: a source added since is in
    /// the clip's sound at once, with no track of its own, and in the track
    /// of the rest of it if there is one. Without, the editor, which makes
    /// the clip's sound again from the tracks, leaves it out. None for a
    /// source the capture started with (its own track goes on, though it's
    /// turned off and on), one it doesn't have, or one that's all there (as
    /// it is while only streaming, once there's sound: a stream sends the
    /// clip's sound alone).
    fn added_note(&self, source: &AudioSourceCfg) -> Option<(String, bool)> {
        let has = |list: &[capture::sources::AudioSource]| list.iter().any(|s| s.id == source.id);
        let added = capture::SOURCES_WHILE_CAPTURING && !self.rec_state.idle() && has(&self.capturing_sources) && !has(&self.started_sources);
        if !added {
            return None;
        }
        added_source_note(self.rec_state, &self.started_sources, source)
    }

    /// The video's sources, one card each, the one in front first: a card's
    /// line says what it is, clicking it opens its settings, and dragging it
    /// by its handle puts it in front of or behind the others. Each card is
    /// laid out in a place of its own (its switch and menus stay its own as
    /// cards change places) and slides to its new place when the order
    /// changes; the one dragged follows the pointer, over the others.
    fn layer_cards(&mut self, ui: &mut egui::Ui, frame: Option<&capture::preview::PreviewFrame>) {
        use crate::canvas_ui::Layer;
        use crate::theme::{self, W};
        use egui::{pos2, vec2};
        let pal = theme::pal(ui);
        let aspect = frame.map_or(16.0 / 9.0, |f| f.width as f32 / f.height.max(1) as f32);
        if self.settings.webcam.is_some() {
            self.webcam_fit_pending(ui.ctx(), aspect);
        }
        // Over the cards: undo and redo on the left (where the sources are),
        // which is in front on the right. Its height is kept while the
        // buttons come and go, so the cards don't move.
        let order = self.layer_order();
        if order.len() > 1 || self.arrangement_history() {
            ui.horizontal(|ui| {
                ui.set_min_height(crate::ui_kit::SMALL_H);
                ui.spacing_mut().item_spacing.x = 2.0;
                self.undo_buttons(ui);
                if order.len() > 1 {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(theme::text("The top one's in front", 12.5, W::Regular).color(pal.text3));
                    });
                }
            });
            ui.add_space(2.0);
        }
        if self.sources_view.card_order.0 != order {
            let moved = !self.sources_view.card_order.0.is_empty();
            self.sources_view.card_order = (order.clone(), moved.then(Instant::now));
        }
        // Slides for a moment after the order changed (dragged, or put in
        // front from a menu); else a card's place follows the ones above it
        // at once (one opening pushes the others down as it opens).
        let held = self.sources_view.reorder.map(|(l, grab, _)| (l, grab));
        let sliding = held.is_some() || self.sources_view.card_order.1.is_some_and(|t| t.elapsed() < Duration::from_millis(400));
        const GAP: f32 = 8.0;
        let w = ui.available_width();
        let ctx = ui.ctx().clone();
        let height = |l: Layer| ctx.data(|d| d.get_temp::<f32>(egui::Id::new(("layer_card_h", l)))).unwrap_or(CARD_HEAD_H + 12.0);
        let heights: Vec<f32> = order.iter().map(|l| height(*l)).collect();
        let total = heights.iter().sum::<f32>() + GAP * order.len().saturating_sub(1) as f32;
        let (area, _) = ui.allocate_exact_size(vec2(w, total), Sense::hover());
        let pointer = ui.input(|i| i.pointer.latest_pos());
        // Each card's place in the order, and where it's drawn now.
        let mut slots = Vec::new();
        let mut top = area.top();
        for (layer, h) in order.iter().zip(&heights) {
            let slot = egui::Rect::from_min_size(pos2(area.left(), top), vec2(w, *h));
            top += h + GAP;
            let id = egui::Id::new(("layer_card_y", *layer));
            let y = match (held, pointer) {
                // Held: where the pointer has it, and from there to its place once let go.
                (Some((l, grab)), Some(p)) if l == *layer => {
                    let y = (p.y - grab).clamp(area.top(), (area.bottom() - h).max(area.top()));
                    theme::value(ui.ctx(), id, y - area.top(), 0.0);
                    y
                }
                _ => area.top() + theme::value(ui.ctx(), id, slot.top() - area.top(), if sliding { theme::NORMAL } else { 0.0 }),
            };
            slots.push((*layer, slot, y));
        }
        // The one held last, over the others.
        self.canvas.card_hover = None;
        let mut drawn = slots.clone();
        drawn.sort_by_key(|(l, ..)| held.is_some_and(|(h, _)| h == *l));
        for (layer, _, y) in drawn {
            let lifted = held.is_some_and(|(h, _)| h == layer);
            let layer_id = if lifted { egui::LayerId::new(egui::Order::Foreground, egui::Id::new("layer_card_held")) } else { ui.layer_id() };
            let at = egui::Rect::from_min_size(pos2(area.left(), y), vec2(w, 10_000.0));
            let mut cui = ui.new_child(egui::UiBuilder::new().max_rect(at).layout(egui::Layout::top_down(egui::Align::Min)).id_salt(("layer_card", layer)).layer_id(layer_id));
            if lifted {
                let shadow = egui::Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(90) };
                let r = egui::Rect::from_min_size(at.min, vec2(w, height(layer)));
                cui.painter().add(shadow.as_shape(r, 8));
            }
            self.layer_card(&mut cui, layer, order.len() > 1, frame, aspect, y);
            let shown = egui::Rect::from_min_size(at.min, vec2(w, cui.min_rect().height()));
            ui.data_mut(|d| d.insert_temp(egui::Id::new(("layer_card_h", layer)), shown.height()));
            if held.map_or_else(|| ui.rect_contains_pointer(shown), |(h, _)| h == layer) {
                self.canvas.card_hover = Some(layer);
            }
        }
        self.reorder_cards(ui, &slots);
        // Adding sources.
        if capture::webcam::AVAILABLE && self.settings.webcam.is_none() {
            ui.add_space(12.0);
            let add = crate::ui_kit::Btn::normal("Webcam").with_icon(egui_phosphor::regular::PLUS).small().tip("Put your camera in your clips, placed and sized on the preview");
            if add.show(ui).clicked() {
                self.add_webcam(aspect);
                if self.settings.webcam.is_some() && !self.sources_view.open_cards.contains(&Layer::Webcam) {
                    self.sources_view.open_cards.push(Layer::Webcam);
                }
            }
        }
    }

    /// Dragging a card by its handle, live (the preview shows it): it goes
    /// above a card once its top (following the pointer) is past that
    /// card's middle, below one once its bottom is. Let go, it can be undone.
    fn reorder_cards(&mut self, ui: &egui::Ui, slots: &[(crate::canvas_ui::Layer, egui::Rect, f32)]) {
        let Some((held, grab, was)) = self.sources_view.reorder else { return };
        let down = ui.input(|i| i.pointer.primary_down());
        if down
            && let Some(p) = ui.input(|i| i.pointer.interact_pos())
            && let Some((_, me, _)) = slots.iter().find(|(l, ..)| *l == held)
        {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            let (top, bottom) = (p.y - grab, p.y - grab + me.height());
            let above = |r: &egui::Rect| if r.center().y < me.center().y { top >= r.center().y } else { bottom > r.center().y };
            let mut order: Vec<_> = slots.iter().map(|(l, ..)| *l).filter(|l| *l != held).collect();
            let at = slots.iter().filter(|(l, r, _)| *l != held && above(r)).count();
            order.insert(at.min(order.len()), held);
            self.set_layer_order(&order);
        }
        if !down {
            if self.arrangement() != was {
                self.keep_for_undo(was);
            }
            self.sources_view.reorder = None;
            // Let go: it slides from there into its place.
            self.sources_view.card_order.1 = Some(Instant::now());
        }
    }

    /// One source's card, its top at `top`: its handle (`movable`: with
    /// others to go in front of or behind), what it is and its state on one
    /// line (with its switch), its settings below when it's open.
    fn layer_card(&mut self, ui: &mut egui::Ui, layer: crate::canvas_ui::Layer, movable: bool, frame: Option<&capture::preview::PreviewFrame>, aspect: f32, top: f32) {
        use crate::canvas_ui::Layer;
        use crate::theme::{self, W};
        use egui::{Align2, FontId, pos2, vec2};
        use egui_phosphor::regular as icon;
        let pal = theme::pal(ui);
        let open = self.sources_view.open_cards.contains(&layer);
        let held = self.sources_view.reorder.is_some_and(|(l, ..)| l == layer);
        // Its line: what it is, then what's wrong with it, if anything.
        let (glyph, name, line, state, on) = match layer {
            Layer::Screen => {
                let apps = matches!(self.settings.capture, CaptureTarget::Apps { .. }) && capture::app_capture();
                let line = if apps {
                    match frame.filter(|f| !f.waiting).and_then(|f| f.app.clone()) {
                        Some(app) => format!("Following {app}"),
                        None => "None of them showing".to_owned(),
                    }
                } else {
                    let display = self.screens.get(self.display_at()).map_or_else(|| "Main display".to_owned(), |d| d.name.clone());
                    // Open, its rows say how it's cropped.
                    let cropped = self.crop_whole().zip(self.settings.screen_crop.filter(|c| !c.is_none() && !open)).map(|((w, h), c)| capture::Crop::from(c).rect(w, h));
                    match cropped {
                        Some([_, _, w, h]) => format!("{display} · cropped to {w}×{h}"),
                        None => display,
                    }
                };
                (if apps { icon::APP_WINDOW } else { icon::MONITOR }, if apps { "Games and apps" } else { "Screen" }, line, None, self.settings.screen.enabled)
            }
            Layer::Webcam => {
                let (state, warn) = self.webcam_state();
                let cam = self.settings.webcam.as_ref();
                let state = (!state.is_empty()).then_some((state, if warn { pal.warn } else { pal.text3 }));
                (icon::WEBCAM, "Webcam", cam.map(|w| w.name.clone()).unwrap_or_default(), state, cam.is_some_and(|w| w.enabled))
            }
        };
        let state = state.or_else(|| (!on).then_some(("Off", pal.text3)));
        group_rows(ui, ("layer_card", layer), |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            // All of its line opens and closes it, but for its handle and switch.
            let (head, resp) = ui.allocate_exact_size(vec2(ui.available_width(), CARD_HEAD_H), Sense::click());
            let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
            crate::demo::mark(ui.ctx(), || format!("card:{name}"), head);
            let painter = ui.painter().clone();
            let cy = head.center().y;
            let mut x = head.left();
            if movable {
                let grip_rect = egui::Rect::from_min_max(pos2(x - 6.0, head.top()), pos2(x + 16.0, head.bottom()));
                let grip = ui.interact(grip_rect, resp.id.with("grip"), Sense::drag());
                crate::demo::mark(ui.ctx(), || format!("grip:{name}"), grip_rect);
                let grip = if held { grip } else { grip.on_hover_text("Drag up or down: the top one's in front").on_hover_cursor(egui::CursorIcon::Grab) };
                let ink = if held || grip.hovered() { pal.text } else { pal.text3 };
                painter.text(pos2(x + 5.0, cy), Align2::CENTER_CENTER, icon::DOTS_SIX_VERTICAL, FontId::proportional(16.0), ink);
                if grip.drag_started() {
                    let grab = ui.input(|i| i.pointer.press_origin()).map_or(0.0, |p| p.y - top);
                    self.sources_view.reorder = Some((layer, grab, self.arrangement()));
                }
                x += 20.0;
            }
            // What it is: its icon and name (dimmer while it's off), then its line.
            let mut put = |text: &str, font: FontId, color: egui::Color32, gap: f32| {
                let g = painter.layout_no_wrap(text.to_owned(), font, color);
                let w = g.size().x;
                painter.galley(pos2(x, cy - g.size().y / 2.0), g, color);
                x += w + gap;
            };
            put(glyph, FontId::proportional(16.0), if on { pal.text2 } else { pal.text3 }, 8.0);
            put(name, theme::font(13.5, W::Medium), if on { pal.text } else { pal.text2 }, 10.0);
            // On the right: which way it opens, the webcam's switch, and its
            // menu (the preview's right-click one; right-clicking here too).
            let mut rui = ui.new_child(egui::UiBuilder::new().max_rect(head).layout(egui::Layout::right_to_left(egui::Align::Center)));
            rui.spacing_mut().item_spacing.x = 10.0;
            let (c, _) = rui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
            let turn = theme::toggle(ui.ctx(), resp.id.with("turn"), open, theme::NORMAL);
            crate::ui_kit::chevron(&painter, c.center(), 3.5, turn, if resp.hovered() { pal.text } else { pal.text3 });
            if layer == Layer::Webcam
                && let Some(w) = self.settings.webcam.as_mut()
            {
                let tip = if w.enabled { "Turn the webcam off: it leaves your clips, and its place, size, crop and format are kept" } else { "Put your webcam back in your clips" };
                crate::ui_kit::switch(&mut rui, &mut w.enabled).on_hover_text(tip);
            }
            if layer == Layer::Screen {
                let what = if name == "Screen" { "your screen" } else { "your games and apps" };
                let tip = if self.settings.screen.enabled { format!("Turn {what} off: it leaves your clips, and its place and crop are kept") } else { format!("Put {what} back in your clips") };
                crate::ui_kit::switch(&mut rui, &mut self.settings.screen.enabled).on_hover_text(tip);
            }
            rui.add_space(-4.0);
            let more = crate::ui_kit::Btn::icon(crate::ui_kit::Kind::Ghost, icon::DOTS_THREE).small().tip(if layer == Layer::Webcam { "Reset, turn, flip, put in front or behind, remove" } else { "Fill or fit, reset, turn, flip, put in front or behind" }).show(&mut rui);
            let was = self.arrangement();
            crate::ui_kit::menu_popup(&more, false, 230.0, |ui| self.layer_menu(ui, layer, aspect));
            crate::ui_kit::context_menu(&resp, 230.0, |ui| self.layer_menu(ui, layer, aspect));
            if self.arrangement() != was {
                self.keep_for_undo(was);
            }
            let right = rui.min_rect().left() - 10.0;
            let mut job = crate::ui_kit::single_line_job(&line, theme::font(12.5, W::Regular), pal.text3, (right - x).max(0.0));
            if let Some((state, color)) = state {
                job.append(&format!(" · {state}"), 0.0, egui::TextFormat::simple(theme::font(12.5, W::Regular), color));
            }
            let g = ui.fonts_mut(|f| f.layout_job(job));
            painter.galley(pos2(x, cy - g.size().y / 2.0), g, pal.text3);
            if resp.clicked() {
                if open {
                    self.sources_view.open_cards.retain(|l| *l != layer);
                } else {
                    self.sources_view.open_cards.push(layer);
                }
            }
            if open {
                crate::ui_kit::divider(ui, 0.0);
                // Off, its settings are kept (and can be changed): dimmed.
                ui.scope(|ui| {
                    if !on {
                        ui.multiply_opacity(0.55);
                    }
                    match layer {
                        Layer::Screen => self.screen_rows(ui, frame),
                        Layer::Webcam => self.webcam_rows(ui),
                    }
                });
            }
        });
    }

    /// The screen's settings, in its card: what to record (the whole screen
    /// or your games and apps), which display and how much of it, and where
    /// it sits.
    fn screen_rows(&mut self, ui: &mut egui::Ui, frame: Option<&capture::preview::PreviewFrame>) {
        let idle = self.rec_state.idle();
        let pal = crate::theme::pal(ui);
        let apps_mode = matches!(self.settings.capture, CaptureTarget::Apps { .. });
        let row = |ui: &mut egui::Ui, add: &mut dyn FnMut(&mut egui::Ui)| {
            ui.horizontal(|ui| {
                ui.set_min_height(GROUP_ROW_H);
                ui.spacing_mut().item_spacing.x = 5.0;
                add(ui);
            });
        };
        // What to record: changes right away, while capturing too (the clip
        // switches mid-way; nothing restarts), but for a display that would
        // make the video another size (see below; where the desktop's dialog
        // picks the screen, that waits too).
        if capture::app_capture() {
            let opts = [
                crate::ui_kit::Seg { icon: Some(egui_phosphor::regular::MONITOR), label: "Whole screen", tip: Some("Everything on one display") },
                crate::ui_kit::Seg { icon: Some(egui_phosphor::regular::APP_WINDOW), label: "Games and apps", tip: Some("Only the games and apps you pick, following whichever you're using") },
            ];
            let mut picked = None;
            row(ui, &mut |ui| {
                ui.label(crate::theme::text("Record", 13.5, crate::theme::W::Regular).color(pal.text));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    picked = crate::ui_kit::segmented_opts(ui, "capture_kind", &opts, usize::from(apps_mode), None, true);
                });
            });
            if let Some(i) = picked {
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
            crate::ui_kit::divider(ui, 0.0);
        }
        match self.settings.capture.clone() {
            CaptureTarget::Apps { apps, away_screen } if capture::app_capture() => self.app_rows(ui, apps, away_screen, frame),
            // Linux on Wayland: the desktop's own dialog picks the screen (once; it's remembered).
            _ if cfg!(target_os = "linux") && !capture::screens_listed() => {
                row(ui, &mut |ui| {
                    ui.label(crate::theme::text("Display", 13.5, crate::theme::W::Regular).color(pal.text));
                    ui.label(crate::theme::text("picked in your desktop's sharing dialog", 12.5, crate::theme::W::Regular).color(pal.text3));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let b = crate::ui_kit::Btn::normal("Choose another…").small().tip("Your desktop asks again which screen to record").locked((!idle).then(|| "Stop capturing to switch screens".to_owned()));
                        if b.show(ui).clicked() {
                            capture::choose_screen_again();
                            self.video_preview = None; // restarts, and asks
                        }
                    });
                });
            }
            _ => {
                let names: Vec<String> = self.screens.iter().map(|d| d.name.clone()).collect();
                let at = self.display_at();
                // While clipping, the video stays the size it started at: a
                // display that would make it another size (it's the size of
                // the display, in p or Native) waits until clipping stops.
                // Any other is shown in it as the one before was.
                let recording = frame.map(|f| f.recorded).filter(|_| !idle);
                let sized: Vec<Option<(u32, u32)>> = match recording {
                    Some(_) => self.screens.iter().map(|d| capture::frame_size(&self.display_source(&d.id), self.settings.video_size())).collect(),
                    None => Vec::new(),
                };
                let waits = |i: usize| recording.is_some_and(|now| sized.get(i).copied().flatten().is_some_and(|size| size != now));
                let stop = format!("Stop {}", self.rec_state.words());
                let stop = stop.as_str();
                row(ui, &mut |ui| {
                    ui.label(crate::theme::text("Display", 13.5, crate::theme::W::Regular).color(pal.text));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let current = names.get(at).cloned().unwrap_or_else(|| "Main display".to_owned());
                        let r = crate::ui_kit::select(ui, &current, 180.0);
                        crate::ui_kit::menu_popup(&r, false, 240.0, |ui| {
                            for (i, name) in names.iter().enumerate() {
                                let mut item = crate::ui_kit::Item::new(name.clone()).check(at == i);
                                if waits(i) {
                                    let (w, h) = sized[i].unwrap_or_default();
                                    item = item.enabled(false).value("Stop clipping first").tip(format!("It would make your video {w}×{h}, and its size is set when clipping starts. {stop} to switch, or give your video a size of its own."));
                                }
                                if item.show(ui).clicked() {
                                    self.settings.display_id = Some(self.screens[i].id.clone());
                                    self.settings.display_index = i;
                                }
                            }
                        });
                    });
                });
                if capture::SCREEN_CROP {
                    crate::ui_kit::divider(ui, 0.0);
                    self.crop_row(ui);
                }
            }
        }
        // Where it is in the video, in a few words (and how the video puts
        // it in, in a shape of its own); its numbers under it.
        crate::ui_kit::divider(ui, 0.0);
        self.placement_row(ui, crate::canvas_ui::Layer::Screen);
    }

    /// The video's size, as recorded, over the preview: a button opening the
    /// size's own choices (the same setting as Settings' Resolution and
    /// Aspect ratio), so the video can change shape while it's arranged.
    fn size_button(&mut self, ui: &mut egui::Ui, text: &str) {
        use crate::theme::{self, W};
        let pal = theme::pal(ui);
        let locked = !self.rec_state.idle();
        let galley = ui.painter().layout_no_wrap(text.to_owned(), theme::font(12.5, W::Regular), pal.text3);
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(galley.size().x + 30.0, 24.0), Sense::click());
        crate::demo::mark(ui.ctx(), || "button:Video size".into(), rect);
        let id = resp.id.with("size");
        let open = egui::Popup::is_id_open(ui.ctx(), id);
        let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered() || open, theme::FAST);
        if hv > 0.0 {
            ui.painter().rect_filled(rect, crate::ui_kit::RADIUS, theme::fade(pal.control, hv));
        }
        let ink = theme::mix(pal.text3, pal.text, hv);
        ui.painter().galley_with_override_text_color(egui::pos2(rect.left() + 8.0, rect.center().y - galley.size().y / 2.0), galley, ink);
        let mark = egui::pos2(rect.right() - 11.0, rect.center().y);
        if locked {
            ui.painter().text(mark, egui::Align2::CENTER_CENTER, egui_phosphor::regular::LOCK_SIMPLE, egui::FontId::proportional(12.0), ink);
        } else {
            crate::ui_kit::chevron(ui.painter(), mark, 3.5, theme::value(ui.ctx(), resp.id.with("turn"), if open { 1.0 } else { 0.0 }, theme::NORMAL), ink);
        }
        let tip = if locked { format!("Locked while {}: the video's size is set when the capture starts", self.rec_state.words()) } else { "The video's size: what your clips are".to_owned() };
        let resp = resp.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand);
        egui::Popup::from_toggle_button_response(&resp)
            .id(id)
            .gap(6.0)
            .align(egui::RectAlign::BOTTOM_END)
            .frame(crate::ui_kit::pop_frame(ui))
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| self.size_panel(ui, locked));
    }

    /// The video size's choices: a resolution and an aspect ratio, each one
    /// click (an exact size, and the rest, in Settings).
    fn size_panel(&mut self, ui: &mut egui::Ui, locked: bool) {
        use crate::settings::OutputResolution as R;
        use crate::theme::{self, W};
        use crate::ui_kit::Seg;
        let pal = theme::pal(ui);
        ui.set_width(380.0);
        ui.spacing_mut().item_spacing.y = 6.0;
        ui.horizontal(|ui| {
            ui.label(theme::text("Video size", 13.5, W::Semibold).color(pal.text));
            ui.label(theme::text("what your clips are", 12.5, W::Regular).color(pal.text3));
        });
        if locked {
            ui.horizontal(|ui| {
                ui.label(theme::ic(egui_phosphor::regular::LOCK_SIMPLE, 12.5).color(pal.text3));
                ui.label(theme::text("Stop clipping to change it: the size is set when clipping starts", 12.5, W::Regular).color(pal.text2));
            });
        }
        ui.add_enabled_ui(!locked, |ui| {
            let label = |ui: &mut egui::Ui, t: &str| {
                ui.add_space(4.0);
                ui.label(theme::text(t, 12.0, W::Medium).color(pal.text2));
            };
            label(ui, "Resolution");
            // The common ones in p, or a custom size: its width and height
            // in boxes under them.
            const SIZES: [R; 5] = [R::Native, R::P1440, R::P1080, R::P720, R::P480];
            let names = SIZES.map(|r| r.label());
            let mut opts: Vec<Seg> = names.iter().map(|n| Seg { icon: None, label: n.as_str(), tip: None }).collect();
            opts.push(Seg { icon: None, label: "Custom", tip: Some("Any width and height") });
            let exact = self.settings.resolution.exact();
            let now = if exact.is_some() { SIZES.len() } else { SIZES.iter().position(|r| *r == self.settings.resolution).unwrap_or(usize::MAX) };
            match crate::ui_kit::segmented_opts(ui, "size_res", &opts, now, None, !locked) {
                Some(i) if i < SIZES.len() => self.settings.resolution = SIZES[i],
                // From the size it records at now, the width chosen to type over.
                Some(_) if exact.is_none() => {
                    let (w, h) = capture::frame_size(&self.video_source(), self.settings.video_size()).unwrap_or((1920, 1080));
                    self.settings.resolution = R::Exact(w & !1, h & !1);
                    crate::ui_kit::type_in_size(ui.ctx(), "res_size");
                }
                _ => {}
            }
            // The same boxes as Settings' (and their link).
            if let Some(size) = self.settings.resolution.exact()
                && let Some((w, h)) = crate::ui_kit::size_field(ui, "res_size", size, 144..=capture::MAX_SIDE, 144..=capture::MAX_SIDE)
            {
                self.settings.resolution = R::Exact(w, h);
            }
            label(ui, "Aspect ratio");
            const SHAPES: [Option<[u32; 2]>; 6] = [None, Some([16, 9]), Some([9, 16]), Some([1, 1]), Some([4, 3]), Some([21, 9])];
            let names: Vec<String> = SHAPES.iter().map(|s| s.map_or("Same as screen".to_owned(), |[w, h]| format!("{w}:{h}"))).collect();
            let opts: Vec<Seg> = names.iter().map(|n| Seg { icon: None, label: n.as_str(), tip: None }).collect();
            let exact = self.settings.resolution.exact();
            let now = if exact.is_some() { usize::MAX } else { SHAPES.iter().position(|s| *s == self.settings.shape).unwrap_or(usize::MAX) };
            if let Some(i) = crate::ui_kit::segmented_opts(ui, "size_shape", &opts, now, None, !locked) {
                self.settings.shape = SHAPES[i];
                // A custom size is its own shape: a resolution in p, as near it, instead.
                if let Some((w, h)) = exact {
                    let short = w.min(h);
                    self.settings.resolution = R::PRESETS.iter().copied().find(|r| r.short_side() == Some(short)).unwrap_or(R::Custom(short));
                }
            }
            // Cropped (on its page, or with Alt on the preview), filling the
            // video: the crop's size, for every pixel of it as it is.
            let video = capture::frame_size(&self.video_source(), self.settings.video_size());
            if let Some((w, h)) = self.crop_size().filter(|m| Some(*m) != video) {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.label(theme::text(format!("Your screen's cropped to {w}×{h}"), 12.5, W::Regular).color(pal.text2));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let b = crate::ui_kit::Btn::normal("Match the crop").small().tip(format!("Make your video {w}×{h}, the crop's size: every pixel of it as it is"));
                        if b.show(ui).clicked() {
                            self.settings.resolution = R::Exact(w, h);
                        }
                    });
                });
            }
        });
        ui.add_space(4.0);
        crate::ui_kit::divider(ui, 0.0);
        let size = capture::frame_size(&self.video_source(), self.settings.video_size());
        let native = capture::frame_size(&self.video_source(), capture::VideoSize { shape: self.settings.capture_shape(), ..Default::default() });
        let note = match (size, native) {
            (Some((w, h)), Some((nw, nh))) if self.settings.resolution.short_side().is_some_and(|t| t > nw.min(nh)) => format!("Records at {w}×{h}: as big as your screen allows"),
            (Some((w, h)), _) => format!("Records at {w}×{h}"),
            (None, _) => String::new(),
        };
        ui.horizontal(|ui| {
            ui.label(theme::text(note, 12.5, W::Regular).color(pal.text2));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let more = crate::ui_kit::Btn::ghost("More in Settings").trail(egui_phosphor::regular::CARET_RIGHT).small().tip("Every resolution, frame rate and quality");
                if more.show(ui).clicked() {
                    self.open_settings(crate::settings_ui::Pane::Video);
                    ui.close();
                }
            });
        });
    }

    /// The Crop row: what's kept of the screen, and a way to its page.
    fn crop_row(&mut self, ui: &mut egui::Ui) {
        use crate::ui_kit::Btn;
        let pal = crate::theme::pal(ui);
        let kept = self.crop_whole().zip(self.settings.screen_crop.filter(|c| !c.is_none())).map(|((w, h), c)| capture::Crop::from(c).rect(w, h));
        ui.horizontal(|ui| {
            ui.set_min_height(GROUP_ROW_H);
            ui.spacing_mut().item_spacing.x = 5.0;
            ui.label(crate::theme::text("Crop", 13.5, crate::theme::W::Regular).color(pal.text));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                let b = Btn::normal("Adjust").trail(egui_phosphor::regular::CARET_RIGHT).small().tip("Show only part of your screen: a game at a lower resolution than your display, with black around it");
                if b.show(ui).clicked() {
                    self.sources_view.crop_open = true;
                }
                let text = kept.map_or("Off".to_owned(), |[_, _, w, h]| format!("{w}×{h}"));
                ui.label(crate::theme::text(text, 12.5, crate::theme::W::Regular).color(pal.text3));
            });
        });
    }

    /// The size a video would be to show the screen's crop as it is (even,
    /// as big as a video can be; turned, as it shows), when it's cropped and
    /// fills the video.
    fn crop_size(&self) -> Option<(u32, u32)> {
        let (sw, sh) = self.crop_whole()?;
        let crop = self.settings.screen_crop.filter(|c| !c.is_none() && self.settings.screen.placed.is_none())?;
        let [_, _, w, h] = capture::Crop::from(crop).rect(sw, sh);
        let (w, h) = crate::canvas_ui::Turned::of(&self.settings.screen).size((w, h));
        ((w, h) != crate::canvas_ui::Turned::of(&self.settings.screen).size((sw & !1, sh & !1))).then(|| capture::output_size(w, h, None))
    }

    /// The display's own size, uncropped, when it's known.
    pub(crate) fn crop_whole(&self) -> Option<(u32, u32)> {
        match self.video_source() {
            source @ capture::VideoSource::Screen { .. } => capture::frame_size(&source, Default::default()),
            capture::VideoSource::Apps { .. } => None,
        }
    }

    /// The crop's page, in Sources' place: all of the screen with what's
    /// kept drawn on it to drag, and its size and place as numbers. It starts
    /// centred, and stays so as its size changes, until it's put somewhere.
    /// It's only what's shown of the screen, so it changes while clipping
    /// too; the video's size can be made the crop's, for every pixel of it.
    fn screen_crop_page(&mut self, ui: &mut egui::Ui) {
        use crate::settings::ScreenCrop;
        use crate::theme::{self, W};
        use crate::ui_kit::{Btn, Row};
        let pal = theme::pal(ui);
        // Back to Sources: the arrow, or Esc.
        let back_key = !crate::ui_kit::overlay_open(ui.ctx()) && !ui.ctx().egui_wants_keyboard_input() && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        if back_key {
            self.sources_view.crop_open = false;
            return;
        }
        // Its size's link keeps the shape as its edges are dragged too.
        let keep = crate::ui_kit::size_field_keeps(ui.ctx(), "screen_crop").is_some();
        let mut back = false;
        self.page_head(ui, "", |_, ui| {
            back = Btn::icon(crate::ui_kit::Kind::Ghost, egui_phosphor::regular::CARET_LEFT).tip("Back to Sources  (Esc)").show(ui).clicked();
            ui.label(theme::text("Crop", 20.0, W::Semibold).color(pal.text));
        });
        if back {
            self.sources_view.crop_open = false;
            return;
        }
        capture::preview::request();
        let frame = capture::preview::latest();
        self.update_preview_texture(ui.ctx(), frame.as_deref());
        crate::ui_kit::repaint_after(ui.ctx(), Duration::from_millis(33));
        let Some((sw, sh)) = self.crop_whole() else {
            ui.label(theme::text("The display's size isn't known yet.", 13.0, W::Regular).color(pal.text3));
            return;
        };
        // None set: all of the screen, centred.
        let crop = self.settings.screen_crop.filter(|c| !c.is_none()).unwrap_or(ScreenCrop { w: sw, h: sh, at: None });
        let [x, y, w, h] = capture::Crop::from(crop).rect(sw, sh);
        let centred = crop.at.is_none();
        let mut set: Option<ScreenCrop> = None;
        // What the video does with it, when it fills the video: the video's
        // size, the crop's (as a video can be), and why that can't change now.
        let video = capture::frame_size(&self.video_source(), self.settings.video_size());
        // The crop as it shows in the video: turned, if the screen is.
        let (tw, th) = crate::canvas_ui::Turned::of(&self.settings.screen).size((w, h));
        let matched = capture::output_size(tw, th, None);
        let filling = self.settings.screen.placed.is_none() && (w, h) != (sw & !1, sh & !1);
        let size_lock = (!self.rec_state.idle()).then(|| format!("Stop {} to change your video's size: it's set when the capture starts", self.rec_state.words()));
        let mut match_crop = false;
        // The least kept each way.
        let least = 64u32;

        let picture = |ui: &mut egui::Ui, this: &mut Self, max_h: f32, set: &mut Option<ScreenCrop>| {
            let aspect = sw as f32 / sh.max(1) as f32;
            let pw = ui.available_width().min(max_h * aspect);
            ui.vertical_centered(|ui| {
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(pw, pw / aspect), Sense::click_and_drag());
                let p = ui.painter();
                p.rect_filled(rect, 6.0, pal.stage);
                // Only a picture of the whole screen is cropped on (one of
                // the video shows for a moment as the preview starts over).
                let whole = frame.as_ref().is_some_and(|f| (f.recorded.0 as f32 / f.recorded.1.max(1) as f32 - aspect).abs() < 0.01);
                if !(whole && this.paint_preview(p, rect)) {
                    let text = match this.video_preview.as_ref().and_then(|(_, v)| v.error()) {
                        Some(e) => format!("No preview: {e}"),
                        None => "Starting preview…".to_owned(),
                    };
                    p.text(rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(13.0), Color32::from_gray(170));
                }
                let bounds = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(sw as f32, sh as f32));
                let kept = egui::Rect::from_min_size(egui::pos2(x as f32, y as f32), egui::vec2(w as f32, h as f32));
                // Dragged: where it's put (no longer centred).
                if let Some(k) = crate::webcam_ui::crop_editor(ui, &resp, rect, bounds, kept, egui::vec2(least as f32, least as f32), keep, &mut this.sources_view.crop_drag) {
                    let r = |v: f32| v.round().max(0.0) as u32;
                    *set = Some(ScreenCrop { w: r(k.width()), h: r(k.height()), at: Some([r(k.min.x), r(k.min.y)]) });
                }
            });
            ui.add_space(8.0);
            ui.vertical_centered(|ui| {
                let hint = if keep {
                    "Drag the edges or corners (the shape's kept), or the middle to move it."
                } else {
                    "Drag the edges or corners (Shift keeps the shape), or the middle to move it."
                };
                ui.label(theme::text(hint, 12.5, W::Regular).color(pal.text3));
            });
        };
        let mut locked_click = false;
        let numbers = |ui: &mut egui::Ui, set: &mut Option<ScreenCrop>, locked_click: &mut bool, match_crop: &mut bool| {
            let small = |ui: &mut egui::Ui, t: &str| ui.label(theme::text(t, 12.5, W::Regular).color(pal.text3));
            // Laid out left to right at the row's end, so Tab goes through
            // them in reading order: `width` across, made by `add`.
            let boxes = |ui: &mut egui::Ui, width: f32, add: &mut dyn FnMut(&mut egui::Ui)| {
                ui.allocate_ui_with_layout(egui::vec2(width, crate::ui_kit::CONTROL_H), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    add(ui);
                });
            };
            const BOX: f32 = 64.0;
            crate::ui_kit::list(ui, "screen_crop", |l| {
                // Its shape: the biggest of it there's room for, where it is,
                // and kept as it's sized (it's the size's link).
                let (_, c) = l.row(Row::new("Aspect ratio"), |ui| {
                    let now = crate::ui_kit::size_field_keeps(ui.ctx(), "screen_crop").map(crate::ui_kit::ratio_of);
                    let from = crate::ui_kit::ratio_of(w as f64 / h.max(1) as f64);
                    match crate::ui_kit::ratio_field(ui, "screen_crop_ratio", ("Any", ""), now, from) {
                        Some(None) => crate::ui_kit::set_size_field_keeps(ui.ctx(), "screen_crop", None),
                        Some(Some([aw, ah])) => {
                            crate::ui_kit::set_size_field_keeps(ui.ctx(), "screen_crop", Some(aw as f64 / ah as f64));
                            let centre = (x as f64 + w as f64 / 2.0, y as f64 + h as f64 / 2.0);
                            // All of the display, when it's that shape.
                            *set = Some(match media::Crop::largest(sw, sh, aw, ah, centre).within(sw, sh) {
                                Some(c) => ScreenCrop { w: c.w, h: c.h, at: (!centred).then_some([c.x, c.y]) },
                                None => ScreenCrop::default(),
                            });
                        }
                        None => {}
                    }
                });
                *locked_click |= c;
                // Its size: the game's resolution. Centred, it stays in the
                // middle; else its top-left corner stays put. With the shape
                // kept, one follows the other.
                let (_, c) = l.row(Row::new("Size").note("The game's resolution, like 1600 × 900"), |ui| {
                    if let Some((w, h)) = crate::ui_kit::size_field(ui, "screen_crop", (w, h), least..=sw, least..=sh) {
                        *set = Some(ScreenCrop { w, h, ..crop });
                    }
                });
                *locked_click |= c;
                // Where it is: its top-left corner, or the middle.
                let note = if centred { "Centred on the screen".to_owned() } else { "Its top-left corner, from the screen's".to_owned() };
                let (_, c) = l.row(Row::new("Position").note(note), |ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    let b = Btn::normal("Centre").small().enabled(!centred).tip("Put it in the middle of the screen, and keep it there");
                    if b.show(ui).clicked() {
                        *set = Some(ScreenCrop { at: None, ..crop });
                    }
                    boxes(ui, BOX * 2.0 + 6.0 + 16.0 + 6.0 * 3.0, &mut |ui| {
                        if let Some(v) = crate::ui_kit::number_box(ui, egui::Id::new("screen_crop_x"), x, 0..=sw - w, BOX) {
                            *set = Some(ScreenCrop { w, h, at: Some([v, y]) });
                        }
                        small(ui, ",");
                        if let Some(v) = crate::ui_kit::number_box(ui, egui::Id::new("screen_crop_y"), y, 0..=sh - h, BOX) {
                            *set = Some(ScreenCrop { w, h, at: Some([x, v]) });
                        }
                        small(ui, "px");
                    });
                });
                *locked_click |= c;
                // The video: as big as the crop, every pixel as it is; else
                // what it does to it, and a way to make it the crop's size.
                if let (true, Some((vw, vh))) = (filling, video) {
                    let note = if (vw, vh) == matched {
                        "The crop's size: every pixel as it is".to_owned()
                    } else {
                        let (crop_shape, video_shape) = (tw as f32 / th.max(1) as f32, vw as f32 / vh.max(1) as f32);
                        let scale = vh as f32 / capture::Shape { w: vw, h: vh, framing: capture::Framing::Fill }.of(tw, th).1.max(1) as f32;
                        let does = if (crop_shape / video_shape - 1.0).abs() > 0.01 {
                            if crop_shape > video_shape { "the crop's sides are cut off to fit it" } else { "the crop's top and bottom are cut off to fit it" }
                        } else if scale > 1.01 {
                            "the crop is made bigger to fill it, a little softer"
                        } else if scale < 0.99 {
                            "the crop is made smaller to fit it"
                        } else {
                            "about the crop's size"
                        };
                        format!("{vw}×{vh}: {does}")
                    };
                    let lock = size_lock.clone().filter(|_| (vw, vh) != matched);
                    let (_, c) = l.row(Row::new("Video size").note(note).locked(lock), |ui| {
                        if (vw, vh) != matched {
                            let (mw, mh) = matched;
                            let b = Btn::normal("Match the crop").small().tip(format!("Make your video {mw}×{mh}, the crop's size: every pixel of it as it is"));
                            *match_crop = b.show(ui).clicked();
                        } else {
                            ui.label(theme::ic(egui_phosphor::regular::CHECK, 14.0).color(pal.text3));
                        }
                    });
                    *locked_click |= c;
                }
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add_space(2.0);
                let what = if (w, h) == (sw, sh) { format!("All of the screen, {sw}×{sh}") } else { format!("{w}×{h} of {sw}×{sh}") };
                ui.label(theme::text(what, 12.5, W::Regular).color(pal.text3));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if (w, h) != (sw, sh) && Btn::ghost("Reset").small().tip("All of the screen again").show(ui).clicked() {
                        *set = Some(ScreenCrop::default());
                    }
                });
            });
        };

        ui.add_space(4.0);
        let avail_w = ui.available_width();
        let avail_h = ui.available_height();
        if avail_w >= TWO_COLUMNS {
            let col = (avail_w - 28.0) / 2.0;
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 28.0;
                ui.allocate_ui_with_layout(egui::vec2(col, avail_h), egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.set_width(col);
                    ui.add_space(4.0);
                    picture(ui, self, avail_h * 0.7, &mut set);
                });
                ui.allocate_ui_with_layout(egui::vec2(col, avail_h), egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.set_width(col);
                    numbers(ui, &mut set, &mut locked_click, &mut match_crop);
                });
            });
        } else {
            crate::ui_kit::scroll(ui, "screen_crop_page", |ui| {
                picture(ui, self, avail_h * 0.4, &mut set);
                ui.add_space(16.0);
                numbers(ui, &mut set, &mut locked_click, &mut match_crop);
            });
        }
        if locked_click && let Some(why) = &size_lock {
            self.toast_icon(egui_phosphor::regular::LOCK_SIMPLE, why.clone());
        }
        if match_crop && size_lock.is_none() {
            self.settings.resolution = crate::settings::OutputResolution::Exact(matched.0, matched.1);
        }
        if let Some(c) = set {
            let all = c.is_none() || capture::Crop::from(c).rect(sw, sh) == [0, 0, sw & !1, sh & !1];
            self.settings.screen_crop = (!all).then_some(c);
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

    /// Draw the preview picture held (macOS: the frame on the GPU; else the
    /// texture) into `rect`. False if there's none yet.
    fn paint_preview(&self, p: &egui::Painter, rect: egui::Rect) -> bool {
        #[cfg(target_os = "macos")]
        if let Some((shown, _)) = &self.sources_view.preview_gpu {
            shown.paint(p, rect);
            return true;
        }
        match &self.sources_view.preview {
            Some((tex, _)) => {
                p.image(tex.id(), rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
                true
            }
            None => false,
        }
    }

    /// The size of the preview picture held, in pixels.
    fn preview_size(&self) -> Option<(u32, u32)> {
        #[cfg(target_os = "macos")]
        if let Some((shown, _)) = &self.sources_view.preview_gpu {
            return Some(shown.size());
        }
        self.sources_view.preview.as_ref().map(|(t, _)| t.size()).map(|[w, h]| (w as u32, h as u32))
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
        // The frame's shape; between captures (none has a frame for a moment,
        // starting to clip) the shape of the picture still shown, so it isn't
        // drawn stretched.
        let shown = self.preview_size().map(|(w, h)| w as f32 / h.max(1) as f32);
        let aspect = frame.map(|f| f.width as f32 / f.height.max(1) as f32).or(shown).unwrap_or(16.0 / 10.0);
        let max_h = (ui.ctx().content_rect().height() * 0.45).max(140.0);
        let w = ui.available_width().min(max_h * aspect);
        ui.vertical_centered(|ui| {
            let (rect, response) = ui.allocate_exact_size(egui::vec2(w, w / aspect), Sense::click_and_drag());
            crate::demo::mark(ui.ctx(), || "preview".into(), rect);
            let p = ui.painter();
            p.rect_filled(rect, 6.0, crate::theme::pal(ui).stage);
            let message = |text: &str| {
                p.text(rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(13.0), Color32::from_gray(170));
            };
            let drawn = self.paint_preview(p, rect);
            // While a new preview starts (another frame rate, size, webcam)
            // the last picture stays up; the message is only for when there's
            // none to show. With every source off, the video is black: said.
            let webcam_on = self.settings.webcam.as_ref().is_some_and(|w| w.enabled);
            match self.video_preview.as_ref().and_then(|(_, p)| p.error()) {
                Some(e) => message(&format!("No preview: {e}")),
                None if !drawn => message("Starting preview…"),
                None if !self.settings.screen.enabled && !webcam_on => message("Nothing's in your video: turn a source on below"),
                None => {}
            }
            if frame.is_some() {
                self.canvas_on_preview(ui, rect, &response);
            }
        });
    }

    /// The games and apps to record, in the screen's card: each with whether
    /// it's open (and which is being recorded) and a remove button, then a
    /// way to add more, and the Tabbed out switch. Changes apply right away,
    /// even while capturing.
    fn app_rows(&mut self, ui: &mut egui::Ui, apps: Vec<CaptureApp>, away_screen: bool, frame: Option<&capture::preview::PreviewFrame>) {
        use crate::theme::{self, W};
        use crate::ui_kit::{Btn, Item, Kind};
        use egui_phosphor::regular as icon;
        let pal = theme::pal(ui);
        let showing = frame.filter(|f| !f.waiting).and_then(|f| f.app.clone());
        let capturing = !self.rec_state.idle();
        let mut list = apps.clone();
        let mut away = away_screen;
        let mut remove = None;
        {
            if list.is_empty() {
                ui.add_space(10.0);
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
                        // Else the app's own icon, else a symbol.
                        None if crate::app_icons::paint(ui, &mut self.app_icons, &app.id, lead.expand(1.0)) => {}
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
            ui.add_space(6.0);
        }
        if list.len() > 1 {
            crate::ui_kit::hint(ui, "Records the one you're using, and keeps it while you click into something else as long as it's on screen.");
            ui.add_space(6.0);
        }
        crate::ui_kit::divider(ui, 0.0);
        ui.horizontal(|ui| {
            ui.set_min_height(GROUP_ROW_H);
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.label(theme::text("Show “Tabbed out” when you leave it", 13.5, W::Regular).color(pal.text));
            crate::ui_kit::info(ui, "Tabbed out", "When the game you were in stops showing (most games minimize when you alt-tab), or none of yours is open, clips show the HasteClip logo with “Tabbed out”. Off: they keep the game's last picture, or show black when none is open. A window that's still on screen keeps being recorded either way.");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                crate::ui_kit::switch(ui, &mut away);
            });
        });
        crate::ui_kit::divider(ui, 0.0);
        ui.horizontal(|ui| {
            ui.set_min_height(GROUP_ROW_H);
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.label(theme::text("Record the whole time a game is open", 13.5, W::Regular).color(pal.text));
            crate::ui_kit::info(ui, "Whole sessions", "When a game or app on this list opens, HasteClip records until it closes, then saves the recording. Clipping waits meanwhile, and starts again after. While it records, Save clip and “HasteClip that” mark the moment in the recording. Stop the recording yourself and it waits until the game opens again.");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                crate::ui_kit::switch(ui, &mut self.settings.record_sessions);
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
        group(ui, "clip_mix", |ui| {
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
            meter::stereo(ui, egui::Id::new("master_meter"), &view.master, &style);
            ui.add_space(10.0);
            crate::ui_kit::divider(ui, 0.0);
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                labelled_switch(ui, &mut self.settings.limiter, "Limiter", "");
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
            // Comes and goes with the sound: slides in and out, not jumping.
            let note = if view.master.clipped() {
                Some((pal.meter_r, "Too loud: it distorted. Turn some sources down."))
            } else if limiting {
                Some((pal.warn, "The limiter is working: turn sources down a little for the cleanest sound."))
            } else {
                None
            };
            if note.is_some() {
                view.last_note = note;
            }
            let shown = view.last_note;
            crate::ui_kit::reveal(ui, "clip_mix_note", note.is_some(), |ui| {
                if let Some((color, text)) = shown {
                    ui.add_space(6.0);
                    ui.add(egui::Label::new(theme::text(text, 12.5, W::Regular).color(color)).wrap());
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
        // Off the page: the capture's buffers opened for drawing are let go
        // too (each texture keeps its whole frame alive). They were kept until
        // 64 had been opened: every visit's captures stayed, ~5 MB a visit at
        // 1080p. The last frame stays, to show at once when the page opens.
        #[cfg(target_os = "macos")]
        if !self.wants_sources_capture() && self.sources_view.preview_gpu.is_some() {
            self.sources_view.preview_gpu = None;
            self.sources_view.preview_frames = Default::default();
        }
        // While clipping, the capture shows what it records; the crop's page
        // has one of its own, of all of the screen (shown over it till it closes).
        let want = capture::preview::AVAILABLE && self.wants_sources_capture() && (self.rec_state.idle() || self.sources_view.crop_open);
        if !want {
            self.video_preview = None;
            return;
        }
        // The meters' capture first: started together, the first time in a
        // session, the two waited on each other five seconds (the meters
        // stood still all that time).
        if self.video_preview.is_none() && self.level_monitor.as_ref().is_some_and(|(_, m)| !m.ready()) {
            return;
        }
        // What a recording would be: same picture, size and frame rate. While
        // the crop's set: all of the screen as it is, to crop on.
        let webcam = self.webcam_source();
        let wanted = match self.video_source() {
            source @ capture::VideoSource::Screen { .. } if self.sources_view.crop_open => (source, Default::default(), self.settings.fps, None, true),
            source => (source, self.settings.video_size(), self.settings.fps, webcam.clone(), false),
        };
        let webcam = wanted.3.clone();
        // A changed app list reaches the running preview without a restart,
        // as does anything else its size. Another size (another display)
        // starts it again at that size: switched in place, it kept the old
        // display's size, the new one in black borders and the webcam drawn
        // in a frame of the wrong shape (until clipping started a new capture).
        if let Some((have, preview)) = &mut self.video_preview {
            if *have != wanted
                && (&have.1, have.2, &have.3, have.4) == (&wanted.1, wanted.2, &wanted.3, wanted.4)
                && capture::frame_size(&have.0, have.1) == capture::frame_size(&wanted.0, wanted.1)
                && preview.update(&wanted.0)
            {
                have.0 = wanted.0.clone();
            }
        }
        if self.video_preview.as_ref().is_none_or(|(w, _)| *w != wanted) {
            let (source, size, fps, _, whole) = &wanted;
            // All of the screen: placed on its own, filling the picture.
            let screen = if *whole { std::sync::Arc::new(std::sync::Mutex::new(capture::ScreenPlacement::whole(capture::Framing::Fit))) } else { self.screen_placement.clone() };
            // Both happen in the background: the old capture closes as the new
            // one opens, and only the new one's frames are shown.
            let preview = capture::preview::VideoPreview::start(source, *size, *fps, Some(self.away_screen.clone()), webcam, screen);
            self.video_preview = Some((wanted, preview));
        }
    }

    pub(crate) fn ensure_level_monitor(&mut self) {
        let want = self.wants_sources_capture() && self.rec_state.idle();
        if !want {
            self.level_monitor = None;
            return;
        }
        let sources = self.capture_sources();
        let current = self.level_monitor.as_ref().map(|(s, _)| s);
        if current != Some(&sources) {
            self.level_monitor = None; // stop the old one before opening devices again
            self.level_monitor = Some((sources.clone(), Background::start(sources, self.live_audio.clone())));
        }
        if let Some(e) = self.level_monitor.as_ref().and_then(|(_, m)| m.take_error()) {
            self.toast_error(format!("Couldn't open audio: {e}"));
        }
    }

    /// The Sources page is open, or about to be (the pointer is on its place
    /// in the rail): its preview and meters start then, so they're running
    /// by the time it shows.
    pub(crate) fn wants_sources_capture(&self) -> bool {
        self.page == crate::Page::Sources || self.sources_warm.is_some_and(|t| t.elapsed() < Duration::from_secs(2))
    }
}

/// The meters' capture, opened and closed on a thread of its own: opening
/// every audio device took a third of a second, and the page froze for it
/// each time it opened.
pub(crate) struct Background {
    /// The monitor once open; `None` in it after it's let go of, so one that
    /// opens only then is closed again.
    monitor: std::sync::Arc<std::sync::Mutex<Option<Option<capture::sources::LevelMonitor>>>>,
    error: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    started: Instant,
}

impl Background {
    fn start(sources: Vec<capture::sources::AudioSource>, live: std::sync::Arc<capture::mixer::LiveAudio>) -> Self {
        let monitor = std::sync::Arc::new(std::sync::Mutex::new(Some(None)));
        let error = std::sync::Arc::new(std::sync::Mutex::new(None));
        let (m, e) = (monitor.clone(), error.clone());
        std::thread::spawn(move || match capture::sources::LevelMonitor::start(&sources, live) {
            Ok(started) => {
                let mut slot = m.lock().unwrap();
                match slot.as_mut() {
                    Some(open) => *open = Some(started),
                    // Let go of meanwhile: closed here, on this thread.
                    None => {
                        drop(slot);
                        drop(started);
                    }
                }
            }
            Err(err) => *e.lock().unwrap() = Some(format!("{err:#}")),
        });
        Self { monitor, error, started: Instant::now() }
    }

    /// Open (or failed, or taking very long): what waits for it may start.
    fn ready(&self) -> bool {
        self.started.elapsed() > Duration::from_secs(8) || self.error.lock().unwrap().is_some() || self.monitor.lock().unwrap().as_ref().is_some_and(|m| m.is_some())
    }

    fn take_error(&self) -> Option<String> {
        self.error.lock().unwrap().take()
    }
}

impl Drop for Background {
    /// Closed on a thread too (stopping waits for the devices).
    fn drop(&mut self) {
        let open = self.monitor.lock().unwrap().take().flatten();
        if let Some(open) = open {
            std::thread::spawn(move || drop(open));
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

/// A name edited in place: focused as it opens (once: taking the focus
/// back every frame left no way out of it). Enter or clicking away keeps
/// what's typed, trimmed; Esc, or leaving it empty, puts back what it was.
/// Returns whether editing is over.
fn name_field(ui: &mut egui::Ui, id: egui::Id, name: &mut String, font: egui::FontId, width: f32) -> bool {
    let was_id = id.with("was");
    let was = ui.data(|d| d.get_temp::<String>(was_id));
    let opening = was.is_none();
    if opening {
        ui.data_mut(|d| d.insert_temp(was_id, name.clone()));
    }
    // Free text: a name.
    #[allow(clippy::disallowed_methods)]
    let r = ui.add(egui::TextEdit::singleline(name).id(id).font(font).desired_width(width));
    if opening {
        r.request_focus();
        return false;
    }
    if !r.lost_focus() {
        return false;
    }
    let was = was.unwrap_or_default();
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) || name.trim().is_empty() {
        *name = was;
    } else {
        *name = name.trim().to_owned();
    }
    forget_name_field(ui, id);
    true
}

/// A [`name_field`] that isn't open: the next one starts afresh.
fn forget_name_field(ui: &egui::Ui, id: egui::Id) {
    ui.data_mut(|d| d.remove::<String>(id.with("was")));
}

/// What `source`, added while `state` runs, doesn't have yet, on a capture
/// that started with the sources `started` (see [`App::added_note`]).
fn added_source_note(state: crate::RecState, started: &[capture::sources::AudioSource], source: &AudioSourceCfg) -> Option<(String, bool)> {
    // The capture starts again once all it does has stopped.
    let again = format!("you stop {} and start again", state.words());
    let (tracks, _, rest) = capture::sources::track_layout(started);
    if tracks.is_empty() {
        // Nothing made has a track for sound: "clips or the stream".
        let made: Vec<&str> = [(state.clipping, "clips"), (state.recording, "the recording"), (state.streaming, "the stream")].into_iter().filter(|(on, _)| *on).map(|(_, w)| w).collect();
        let made = match made.as_slice() {
            [first @ .., last] if !first.is_empty() => format!("{} or {last}", first.join(", ")),
            _ => made.concat(),
        };
        return Some((format!("Not in {made} until {again}: this started without sound."), true));
    }
    // Streaming alone writes no file, and a stream sends the mix: a source
    // added goes into it at once (and one left out of the mix never does).
    if !(state.clipping || state.recording) {
        return None;
    }
    if !source.in_mix {
        return Some((format!("Not recorded until {again}: its own track starts then."), true));
    }
    // In a track apart: the rest's, or there are no tracks apart.
    let kept = rest || tracks.len() == 1;
    match (source.own_track, kept) {
        (true, true) => Some((format!("In the clip's sound now. Its own track starts once {again}."), false)),
        (true, false) => Some((format!("In the clip's sound now. Its own track starts once {again}; until then, changing track volumes in the editor leaves it out."), true)),
        (false, true) => None,
        (false, false) => Some((format!("In the clip's sound now, but until {again}, changing track volumes in the editor leaves it out."), true)),
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
    // Added while capturing: what it doesn't have yet, and whether that
    // leaves something out (see `App::added_note`).
    note: Option<(String, bool)>,
    first: bool,
    app_icons: &mut crate::app_icons::AppIcons,
) -> CardAction {
    use crate::theme::{self, W};
    use crate::ui_kit::{Btn, Item, Kind};
    use egui_phosphor::regular as icon;
    let pal = theme::pal(ui);
    let mut action = CardAction::None;
    // A mic can switch device while capturing (its track stays).
    let picker_locked = locked.filter(|_| !matches!(source.kind, SourceKind::Microphone { .. }));
    // And where the capture can take it, a source can be turned off or
    // removed (its tracks going silent), and on again.
    let fixed = locked.filter(|_| !capture::SOURCES_WHILE_CAPTURING);
    let card_id = source.id.clone();
    group(ui, ("source", card_id), |ui| {
        ui.multiply_opacity(if source.enabled { 1.0 } else { 0.6 });
        // --- Its icon, name, what it is; device; ⋯ ---
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            let (r, _) = ui.allocate_exact_size(egui::vec2(30.0, 30.0), Sense::hover());
            // An app's own icon, where there is one, in place of a symbol.
            let own = match &source.kind {
                SourceKind::App { bundle_id, .. } => crate::app_icons::paint(ui, app_icons, bundle_id, r.shrink(1.0)),
                _ => false,
            };
            if !own {
                ui.painter().rect_filled(r, 6, pal.control);
                let glyph = match &source.kind {
                    SourceKind::Microphone { .. } => icon::MICROPHONE,
                    SourceKind::Desktop { .. } => icon::SPEAKER_HIFI,
                    SourceKind::App { app_name, .. } if app_name.to_lowercase().contains("discord") => icon::DISCORD_LOGO,
                    SourceKind::App { .. } => icon::APP_WINDOW,
                };
                ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, glyph, egui::FontId::proportional(16.0), pal.text2);
            }
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                let name_id = egui::Id::new(("source_name", &source.id));
                if renaming.as_deref() == Some(source.id.as_str()) {
                    if name_field(ui, name_id, &mut source.name, theme::font(14.0, W::Medium), 170.0) {
                        *renaming = None;
                    }
                } else {
                    forget_name_field(ui, name_id);
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
                    let on = Item::new(if source.enabled { "Turn off" } else { "Turn on" }).icon(icon::POWER).enabled(fixed.is_none());
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
                        action = if fixed.is_some() { CardAction::Locked } else { CardAction::Remove };
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
            let fresp = fader(ui, &mut source.volume_db, state, source.muted, w.max(80.0), &source.id);
            volume_value(ui, &mut source.volume_db, fresp.secondary_clicked(), &source.id);
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
                    // Listen: something you do for a moment, so a button, with
                    // its word; lit while you're listening.
                    let on = listening.is_some();
                    let tip = if on { "Stop listening" } else { "Listen through your headphones, to check it works and how it sounds" };
                    let btn = if on { Btn::new(Kind::On, "Listening") } else { Btn::ghost("Listen") };
                    if btn.with_icon(icon::HEADPHONES).small().id(("listen", source.id.as_str())).tip(tip).show(ui).clicked() {
                        action = CardAction::Listen;
                    }
                });
            }
        });
        // Added while capturing: where its sound is until it starts again.
        if let Some((note, leaves_out)) = &note {
            ui.add_space(6.0);
            ui.add(egui::Label::new(theme::text(note, 12.0, W::Regular).color(if *leaves_out { pal.warn } else { pal.text3 })).wrap());
        }
        // Noise reduction: a setting of the mic that stays as set, of three,
        // so a segmented control, right after its name, not across the card
        // from it: there, among the card's other controls, the two read as
        // unrelated things.
        if matches!(source.kind, SourceKind::Microphone { .. }) {
            use crate::settings::NoiseReduction;
            use crate::ui_kit::Seg;
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                ui.label(theme::text("Noise reduction", 13.5, W::Regular).color(pal.text));
                const MODES: [NoiseReduction; 3] = [NoiseReduction::Off, NoiseReduction::Light, NoiseReduction::Strong];
                let opts = [
                    Seg::from("Off"),
                    Seg { icon: None, label: "Light", tip: Some("Takes out steady noise: fans, hum, hiss. Your keyboard stays") },
                    Seg { icon: None, label: "Strong", tip: Some("Takes out everything but your voice") },
                ];
                let chosen = MODES.iter().position(|m| *m == source.noise_reduction).unwrap_or(0);
                if let Some(i) = crate::ui_kit::segmented_opts(ui, ("noise_reduction", source.id.as_str()), &opts, chosen, None, true) {
                    source.noise_reduction = MODES[i];
                }
                ui.spacing_mut().item_spacing.x = 6.0;
                crate::ui_kit::info(ui, "Noise reduction", "Light takes out steady noise behind your voice, like a fan, hum or hiss, and keeps sounds that come and go, like your keyboard. Strong takes out everything but your voice. Changes apply straight away.");
            });
            // Mono: on or off, so a switch, right before its name (as the
            // Limiter's).
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let tip = if source.mono { "On: you're heard in the middle" } else { "Off: as the mic gives it" };
                labelled_switch(ui, &mut source.mono, "Mono", tip);
                crate::ui_kit::info(ui, "Mono", "For a mic heard on one side only, like one in an audio interface's first input (its left): you're heard in the middle, on both sides, as loud as before.");
            });
        }
        if let Some(latency) = listening {
            ui.add_space(6.0);
            let ms = latency.map_or(String::new(), |ms| format!(", {ms:.0} ms delay"));
            ui.label(theme::text(format!("Listening{ms}. Use headphones, or it feeds back."), 12.0, W::Regular).color(pal.warn));
        }
    });
    action
}

/// A switch with its name right after it, in a row laid out left to right:
/// clicking the name switches it too. `tip`: on the switch, if any.
/// Returns whether it was switched.
fn labelled_switch(ui: &mut egui::Ui, on: &mut bool, label: &str, tip: &str) -> bool {
    use crate::theme::{self, W};
    let pal = theme::pal(ui);
    ui.spacing_mut().item_spacing.x = 10.0;
    let sw = crate::ui_kit::switch(ui, on);
    let sw = if tip.is_empty() { sw } else { sw.on_hover_text(tip) };
    let name = ui.add(egui::Label::new(theme::text(label, 13.5, W::Regular).color(pal.text)).sense(Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand);
    if name.clicked() {
        *on = !*on;
    }
    ui.spacing_mut().item_spacing.x = 6.0;
    sw.clicked() || name.clicked()
}

/// A source's volume and level as one control: the meter runs along the
/// track (green, yellow from −18, red from −6), the knob sits at the volume,
/// and a tick marks 0 dB. Drag the knob, or anywhere on the track (Shift:
/// finer); double-click for 0 dB. A right-click on it types one (see
/// [`volume_value`]).
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
    // Taken hold of, the knob goes to the pointer; then it follows it, a
    // fifth as far with Shift held, for fine steps. `raw`: where it's been
    // dragged to, past the ends too (so it comes back with the pointer).
    let raw_key = egui::Id::new(("fader_raw", id));
    let fine = ui.input(|i| i.modifiers.shift);
    let raw = if resp.drag_started() {
        resp.interact_pointer_pos().map(|p| FADER_MIN_DB + ((p.x - track.left()) / track.width()).clamp(0.0, 1.0) * (FADER_MAX_DB - FADER_MIN_DB))
    } else if resp.dragged() {
        let was = ui.data(|d| d.get_temp::<f32>(raw_key)).unwrap_or(*db);
        Some(was + resp.drag_delta().x / track.width() * (FADER_MAX_DB - FADER_MIN_DB) * if fine { 0.2 } else { 1.0 })
    } else {
        None
    };
    if let Some(raw) = raw {
        ui.data_mut(|d| d.insert_temp(raw_key, raw));
        let mut v = raw.clamp(FADER_MIN_DB, FADER_MAX_DB);
        // A small detent at 0 dB (not in fine steps, which go past it).
        if !fine && v.abs() < 0.6 {
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
    resp.on_hover_cursor(egui::CursorIcon::ResizeHorizontal).on_hover_text("Volume: drag the knob (Shift: finer). Double-click for 0 dB, right-click to type one")
}

/// A source's volume, shown, and typed over on a click (or a right-click,
/// or `start`: one on its fader). Enter or clicking away sets it, Esc
/// leaves it as it was.
fn volume_value(ui: &mut egui::Ui, db: &mut f32, start: bool, id: &str) {
    use crate::theme::{self, W};
    let pal = theme::pal(ui);
    let key = egui::Id::new(("volume_typing", id));
    let edit_id = key.with("edit");
    let mut typing: Option<String> = ui.data(|d| d.get_temp(key));
    let (r, resp) = ui.allocate_exact_size(egui::vec2(66.0, 22.0), Sense::click());
    let started = typing.is_none() && (start || resp.clicked() || resp.secondary_clicked());
    if started {
        typing = Some(crate::ui_kit::db_edit_text(*db, FADER_MIN_DB));
    }
    match typing.as_mut() {
        None => {
            let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered(), theme::FAST);
            ui.painter().rect_filled(r, 4, theme::fade(pal.sel, hv));
            ui.painter().text(r.right_center() - egui::vec2(6.0, 0.0), egui::Align2::RIGHT_CENTER, db_text(*db), theme::font(12.5, W::Medium), theme::mix(pal.text2, pal.text, hv));
            resp.on_hover_cursor(egui::CursorIcon::Text).on_hover_text("Click to type a volume");
        }
        Some(text) => {
            ui.painter().rect_filled(r, 4, pal.control);
            let font = theme::font(12.5, W::Medium);
            let unit = ui.painter().layout_no_wrap("dB".to_owned(), font.clone(), pal.text3);
            let field = egui::Rect::from_min_max(r.left_top() + egui::vec2(5.0, 0.0), egui::pos2(r.right() - 6.0 - unit.size().x - 4.0, r.bottom()));
            let mut fui = ui.new_child(egui::UiBuilder::new().max_rect(field).layout(egui::Layout::right_to_left(egui::Align::Center)));
            // A volume as it's said: "-7.5", "+3", "4,5 dB", and "−∞" or
            // "-inf" for silence (read by `parse_db`, #26).
            #[allow(clippy::disallowed_methods)]
            let out = egui::TextEdit::singleline(text)
                .id(edit_id)
                .frame(egui::Frame::NONE)
                .font(font)
                .text_color(pal.text)
                .horizontal_align(egui::Align::Max)
                .desired_width(field.width())
                .margin(egui::Margin::ZERO)
                .show(&mut fui);
            ui.painter().galley(egui::pos2(r.right() - 6.0 - unit.size().x, r.center().y - unit.size().y / 2.0), unit, pal.text3);
            ui.painter().rect_stroke(r, 4, egui::Stroke::new(1.5, pal.text2), egui::StrokeKind::Inside);
            if started {
                // All of it chosen: what's typed replaces it.
                out.response.request_focus();
                let mut state = out.state.clone();
                state.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(text.chars().count()))));
                state.store(ui.ctx(), edit_id);
            } else if !out.response.has_focus() {
                if !ui.input(|i| i.key_pressed(egui::Key::Escape))
                    && let Some(v) = crate::ui_kit::parse_db(text)
                {
                    *db = (v.clamp(FADER_MIN_DB, FADER_MAX_DB) * 10.0).round() / 10.0;
                }
                typing = None;
            }
        }
    }
    ui.data_mut(|d| match typing {
        Some(t) => {
            d.insert_temp(key, t);
        }
        None => d.remove::<String>(key),
    });
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
    // Devices by their names ("AT2020USB+ Analog Stereo"), not their ids
    // (on Linux, PipeWire's "alsa_input.usb-…").
    let name_of = |id: &str| audio.inputs.iter().find(|d| d.id == id).map(|d| d.name.clone());
    let default_label = match &audio.default_input {
        Some(id) => format!("Default ({})", name_of(id).unwrap_or_else(|| id.clone())),
        None => "Default".to_owned(),
    };
    let selected = if device == DEFAULT_DEVICE {
        default_label.clone()
    } else if let Some(name) = name_of(device) {
        name
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
/// A group on the Sources page: its height eases when what's in it changes
/// (see [`crate::ui_kit::eased_frame`]); `id` names it on the page.
pub(crate) fn group(ui: &mut egui::Ui, id: impl std::hash::Hash + std::fmt::Debug, add: impl FnOnce(&mut egui::Ui)) {
    framed(ui, id, egui::Margin::same(GROUP_PAD as i8), add);
}

/// Space between a group's edge and what's in it.
pub(crate) const GROUP_PAD: f32 = 12.0;
/// How tall a row in a group of rows is (`group_rows`).
pub(crate) const GROUP_ROW_H: f32 = 42.0;

/// A picture source's card's line: as tall as its menu button and a little
/// room (its settings below are rows of `GROUP_ROW_H`).
const CARD_HEAD_H: f32 = 34.0;

/// A group of rows (`GROUP_ROW_H` tall, their control centred): less above
/// and below, so the controls are as far from its edges as from its sides.
pub(crate) fn group_rows(ui: &mut egui::Ui, id: impl std::hash::Hash + std::fmt::Debug, add: impl FnOnce(&mut egui::Ui)) {
    let y = (GROUP_PAD - (GROUP_ROW_H - crate::ui_kit::CONTROL_H) / 2.0).max(0.0) as i8;
    framed(ui, id, egui::Margin { left: GROUP_PAD as i8, right: GROUP_PAD as i8, top: y, bottom: y }, add);
}

fn framed(ui: &mut egui::Ui, id: impl std::hash::Hash + std::fmt::Debug, margin: egui::Margin, add: impl FnOnce(&mut egui::Ui)) {
    let pal = crate::theme::pal(ui);
    let frame = egui::Frame::new().fill(pal.raise).corner_radius(8).inner_margin(margin);
    crate::ui_kit::eased_frame(ui, egui::Id::new(("sources_group", id)), frame, |ui| {
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
    // As far below as Settings' headings are from their groups.
    ui.add_space(4.0);
}

pub(crate) fn from_db(db: f32) -> f32 {
    if db <= FADER_MIN_DB + 0.05 { 0.0 } else { 10f32.powf(db / 20.0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name field being edited, frame by frame, with what was pressed.
    struct Field {
        ctx: egui::Context,
        name: String,
        done: bool,
    }

    impl Field {
        fn open(name: &str) -> Self {
            let mut f = Self { ctx: egui::Context::default(), name: name.to_owned(), done: false };
            crate::theme::install_fonts(&f.ctx);
            f.frame(Vec::new());
            f.frame(Vec::new());
            f
        }

        fn frame(&mut self, events: Vec<egui::Event>) {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(600.0, 400.0))), events, ..Default::default() };
            let (name, done) = (&mut self.name, &mut self.done);
            self.ctx
                .run_ui(input, |ui| {
                    if !*done {
                        *done = name_field(ui, egui::Id::new("name"), name, egui::FontId::proportional(14.0), 170.0);
                    }
                })
                .textures_delta
                .clear();
        }

        fn key(&mut self, key: egui::Key) {
            let press = |pressed| egui::Event::Key { key, physical_key: None, pressed, repeat: false, modifiers: Default::default() };
            self.frame(vec![press(true), press(false)]);
            self.frame(Vec::new());
        }

        fn typed(&mut self, text: &str) {
            self.frame(vec![egui::Event::Text(text.to_owned())]);
        }
    }

    #[test]
    fn enter_keeps_the_new_name() {
        let mut f = Field::open("Mic");
        f.typed(" 2  ");
        assert!(!f.done, "still editing while typing");
        f.key(egui::Key::Enter);
        assert!(f.done, "Enter ends editing");
        assert_eq!(f.name, "Mic 2");
    }

    #[test]
    fn escape_or_nothing_puts_the_name_back() {
        let mut f = Field::open("Mic");
        f.typed(" 2");
        f.key(egui::Key::Escape);
        assert!(f.done, "Esc ends editing");
        assert_eq!(f.name, "Mic");

        let mut f = Field::open("Mic");
        for _ in 0..3 {
            f.key(egui::Key::Backspace);
        }
        assert_eq!(f.name, "");
        f.key(egui::Key::Enter);
        assert_eq!((f.done, f.name.as_str()), (true, "Mic"));
    }

    #[test]
    fn clicking_away_keeps_it() {
        let mut f = Field::open("Desktop");
        f.typed(" sound");
        let at = egui::pos2(500.0, 300.0);
        let click = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
        f.frame(vec![egui::Event::PointerMoved(at), click(true)]);
        f.frame(vec![click(false)]);
        f.frame(Vec::new());
        assert!(f.done, "clicking away ends editing");
        assert_eq!(f.name, "Desktop sound");
    }

    fn doing(clipping: bool, recording: bool, streaming: bool) -> crate::RecState {
        crate::RecState { clipping, recording, streaming }
    }

    /// A source a capture started with.
    fn started(id: &str, in_mix: bool, own_track: bool) -> capture::sources::AudioSource {
        let kind = capture::sources::SourceKind::Desktop { exclude_app_sources: true };
        capture::sources::AudioSource { id: id.to_owned(), name: id.to_owned(), kind, in_mix, own_track }
    }

    /// A source added since.
    fn added(in_mix: bool, own_track: bool) -> AudioSourceCfg {
        let mut s = AudioSourceCfg::new("Game", SourceKind::Desktop { exclude_apps: true });
        (s.in_mix, s.own_track) = (in_mix, own_track);
        s
    }

    #[test]
    fn a_source_added_while_only_streaming_is_all_there() {
        let started = [started("mic", true, true)];
        // In the mix (the stream's sound), with a track or without; or kept
        // out of it, as it would be from any stream.
        for (in_mix, own_track) in [(true, true), (true, false), (false, true)] {
            assert_eq!(added_source_note(doing(false, false, true), &started, &added(in_mix, own_track)), None);
        }
    }

    #[test]
    fn a_source_added_to_a_capture_without_sound_says_what_it_isnt_in() {
        let note = |state| added_source_note(state, &[], &added(true, true));
        let stream = "Not in the stream until you stop streaming and start again: this started without sound.";
        assert_eq!(note(doing(false, false, true)), Some((stream.to_owned(), true)));
        let both = "Not in clips or the stream until you stop clipping and streaming and start again: this started without sound.";
        assert_eq!(note(doing(true, false, true)), Some((both.to_owned(), true)));
        let all = "Not in clips, the recording or the stream until you stop clipping, recording and streaming and start again: this started without sound.";
        assert_eq!(note(doing(true, true, true)), Some((all.to_owned(), true)));
    }

    #[test]
    fn a_source_added_while_clipping_says_what_it_waits_for() {
        let clipping = doing(true, false, false);
        // A mic on its own track and the desktop in the mix alone: the rest
        // track keeps a source added in the mix.
        let with_rest = [started("mic", true, true), started("desktop", true, false)];
        assert_eq!(added_source_note(clipping, &with_rest, &added(true, false)), None);
        let (text, warn) = added_source_note(clipping, &with_rest, &added(true, true)).unwrap();
        assert!(!warn && text.ends_with("Its own track starts once you stop clipping and start again."), "{text}");
        // Every source on its own track: none keeps it, so the editor's
        // track volumes leave it out.
        let own_tracks = [started("mic", true, true)];
        let (text, warn) = added_source_note(clipping, &own_tracks, &added(true, false)).unwrap();
        assert!(warn && text.ends_with("changing track volumes in the editor leaves it out."), "{text}");
        // Out of the mix: only its own track takes it, once the capture
        // starts again.
        let note = added_source_note(doing(true, false, true), &own_tracks, &added(false, true));
        let wait = "Not recorded until you stop clipping and streaming and start again: its own track starts then.";
        assert_eq!(note, Some((wait.to_owned(), true)));
    }
}
