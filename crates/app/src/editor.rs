//! The clip editor: frame-exact trim and a per-track audio mix with live meters.
//!
//! Layout, top to bottom: the preview; transport (play, frame step, frame
//! counter); a Premiere-style timeline where a frame ruler, the video lane and
//! one lane per audio track share a time axis, with volume edited directly on
//! each audio lane; the master meter for the mix that becomes track 1.
//!
//! Edits are non-destructive (see the `media` crate): Done saves the sidecar and
//! renders the edited file in the background; the original is never touched.

use std::path::Path;
use std::sync::mpsc::{self, Receiver};

use egui::{Align2, Color32, FontId, Key, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Vec2};
use media::{ClipInfo, Edit};

use crate::library::{ACCENT, REC_RED};
use crate::filmstrip::Filmstrip;
use crate::player::Player;
use crate::waveform::Waveform;
use crate::store::{self, EditTarget};

const WAVE_BLUE: Color32 = Color32::from_rgb(70, 130, 220);
/// Timeline geometry, in points.
const RULER_H: f32 = 22.0;
const VIDEO_H: f32 = 56.0;
const AUDIO_H: f32 = 64.0;
const MASTER_H: f32 = 26.0;
const DIVIDER_H: f32 = 12.0;
const MIN_PREVIEW_H: f32 = 120.0;
/// Deepest timeline zoom: this many frames across.
const MIN_VIEW_FRAMES: f64 = 24.0;
/// Gain slider range in dB.
/// Meter scale floor in dB.
const METER_FLOOR: f32 = -60.0;
/// Meter colours: healthy / hot / clipping.
const OK_GREEN: Color32 = Color32::from_rgb(80, 200, 120);
const HOT_YELLOW: Color32 = Color32::from_rgb(235, 200, 70);

/// What the editor asks the app to do after a frame.
pub enum EditorOutcome {
    Stay,
    Close,
    /// Saved: render `edit` of the clip in the background — as this clip's edit,
    /// or (`new_name`) as a separate new clip, leaving this clip as it was.
    Saved { target: EditTarget, info: ClipInfo, edit: Edit, new_name: Option<String> },
    Reverted(EditTarget),
    /// Rename the clip (the edit stays open).
    Rename,
}

struct Loaded {
    info: ClipInfo,
    pcm: Vec<Vec<f32>>,
    /// Per source track, for drawing.
    waves: Vec<Waveform>,
}

enum State {
    Loading(Receiver<Result<Loaded, String>>),
    Failed(String),
    Ready(Box<Ready>),
}

struct Ready {
    info: ClipInfo,
    player: Player,
    edit: Edit,
    /// What was last saved, to know whether there are unsaved changes.
    saved: Edit,
    waves: Vec<Waveform>,
    strip: Filmstrip,
    /// Smoothed meter values (peak, rms) per track, then master; and peak holds.
    meters: Vec<Meter>,
    master: Meter,
    dragging: Option<Drag>,
    /// Was playing when a playhead or trim drag began: carry on after it.
    resume: bool,
    /// How far the audio tracks are scrolled up when they don't all fit.
    lane_scroll: f32,
    /// Wheel scrolling and Ctrl+wheel zoom, eased out over a few frames.
    glide: crate::wheel::Glide,
    zoom_glide: crate::wheel::Glide,
    /// The part of the clip the timeline shows: start and length, in seconds.
    /// The whole clip until you zoom in.
    view: (f64, f64),
    confirm_discard: bool,
    /// "Save as new clip" dialog: the name being typed, and any problem with it.
    save_as: Option<(String, Option<String>)>,
    /// F2 was pressed.
    rename_requested: bool,
    /// The export settings were opened by the dev hook.
    export_shown: bool,
    /// The export settings popup is open.
    export_open: bool,
}

#[derive(Default, Clone, Copy)]
struct Meter {
    peak: f32,
    rms: f32,
    hold: f32,
    hold_age: f32,
}

impl Meter {
    /// Fast attack, smooth release — reads like a real meter instead of flicker.
    fn feed(&mut self, (peak, rms): (f32, f32), dt: f32) {
        let fall = (-dt / 0.25).exp();
        self.peak = peak.max(self.peak * fall);
        self.rms = rms.max(self.rms * fall);
        if peak >= self.hold {
            self.hold = peak;
            self.hold_age = 0.0;
        } else {
            self.hold_age += dt;
            if self.hold_age > 1.2 {
                self.hold *= (-dt / 0.4).exp();
            }
        }
    }
}

pub struct Editor {
    target: EditTarget,
    state: State,
    /// How much of the height the preview gets; the timeline has the rest.
    preview_share: f32,
}

impl Editor {
    /// Edit the clip at `clip`. An edited clip is decoded from its original, so
    /// every earlier decision can still be changed.
    pub fn open(ctx: &egui::Context, clip: &Path, preview_share: f32) -> Self {
        let target = EditTarget::of(clip);
        let (tx, rx) = mpsc::channel();
        let src = target.source.clone();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            let result = (|| -> Result<Loaded, String> {
                let info = media::probe(&src).map_err(|e| e.to_string())?;
                // Every track at once: each is its own ffmpeg.
                let jobs: Vec<_> = info
                    .source_tracks()
                    .iter()
                    .map(|track| {
                        let (src, index) = (src.clone(), track.index);
                        std::thread::spawn(move || {
                            let samples = media::decode_audio(&src, index).map_err(|e| e.to_string())?;
                            let wave = Waveform::new(&samples, media::PREVIEW_RATE);
                            Ok::<_, String>((samples, wave))
                        })
                    })
                    .collect();
                let mut pcm = Vec::new();
                let mut waves = Vec::new();
                for job in jobs {
                    let (samples, wave) = job.join().map_err(|_| "decoding the audio failed".to_owned())??;
                    pcm.push(samples);
                    waves.push(wave);
                }
                Ok(Loaded { info, pcm, waves })
            })();
            let _ = tx.send(result);
            repaint.request_repaint();
        });
        Self { target, state: State::Loading(rx), preview_share }
    }

    /// The clip was renamed from `from` to `to`: saved under the new name.
    /// A clip never edited is its own original, so it's read from there too
    /// (the edit in progress is kept).
    pub fn renamed(&mut self, ctx: &egui::Context, from: &Path, to: &Path) {
        if self.target.clip != from {
            return;
        }
        self.target.clip = to.to_path_buf();
        if self.target.source != from {
            return;
        }
        self.target.source = to.to_path_buf();
        match &mut self.state {
            State::Ready(r) => {
                r.player = r.player.reopen(ctx, to);
                r.strip = Filmstrip::build(ctx, to, &r.info);
            }
            _ => *self = Self::open(ctx, to, self.preview_share),
        }
    }

    /// The clip as named in the library.
    pub fn clip(&self) -> &Path {
        &self.target.clip
    }

    /// The preview's share of the height, as last dragged (to remember it).
    pub fn preview_share(&self) -> f32 {
        self.preview_share
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) -> EditorOutcome {
        if let State::Loading(rx) = &self.state {
            if let Ok(result) = rx.try_recv() {
                self.state = match result {
                    Ok(l) => State::Ready(Box::new(Ready::new(ui.ctx(), &self.target, l))),
                    Err(e) => State::Failed(e),
                };
            }
        }
        match &mut self.state {
            State::Loading(_) => {
                // A horizontal row fills the width and starts at the left, so
                // size the spinner + label and place them in the middle ourselves.
                let text = "Opening clip…";
                let galley = ui.painter().layout_no_wrap(text.into(), egui::TextStyle::Body.resolve(ui.style()), ui.visuals().text_color());
                let spinner = ui.spacing().interact_size.y;
                let gap = ui.spacing().item_spacing.x;
                let size = Vec2::new(spinner + gap + galley.size().x, spinner.max(galley.size().y));
                let rect = Rect::from_center_size(ui.max_rect().center(), size);
                ui.put(Rect::from_min_size(rect.min, Vec2::splat(spinner)), egui::Spinner::new().size(spinner));
                ui.put(
                    Rect::from_min_size(Pos2::new(rect.min.x + spinner + gap, rect.min.y), Vec2::new(galley.size().x, size.y)),
                    egui::Label::new(text),
                );
                EditorOutcome::Stay
            }
            State::Failed(e) => {
                let mut out = EditorOutcome::Stay;
                ui.vertical_centered(|ui| {
                    ui.add_space(80.0);
                    ui.colored_label(ui.visuals().error_fg_color, format!("Couldn't open this clip: {e}"));
                    if ui.button("Back to clips").clicked() {
                        out = EditorOutcome::Close;
                    }
                });
                out
            }
            State::Ready(r) => r.ui(ui, &self.target, &mut self.preview_share),
        }
    }
}

impl Ready {
    fn new(ctx: &egui::Context, target: &EditTarget, l: Loaded) -> Self {
        let source = &target.source;
        let mut edit = store::load_edit(target).unwrap_or_else(|| Edit::new(&l.info));
        // Sidecar from an older layout or a hand-edited file: make it fit.
        if edit.tracks.len() != l.pcm.len() {
            edit.tracks = Edit::new(&l.info).tracks;
        }
        edit.end = edit.end.min(l.info.duration);
        let gains = edit.tracks.clone();
        let n = l.pcm.len();
        let mut player = Player::new(ctx, source, l.info.clone(), l.pcm, gains);
        player.seek(edit.start);

        let full = l.info.duration.max(1e-6);
        let strip = Filmstrip::build(ctx, source, &l.info);
        Self {
            info: l.info,
            player,
            saved: edit.clone(),
            edit,
            waves: l.waves,
            strip,
            meters: vec![Meter::default(); n],
            master: Meter::default(),
            dragging: None,
            resume: false,
            lane_scroll: 0.0,
            glide: Default::default(),
            zoom_glide: Default::default(),
            view: (0.0, full),
            confirm_discard: false,
            save_as: None,
            rename_requested: false,
            export_shown: false,
            export_open: false,
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, target: &EditTarget, preview_share: &mut f32) -> EditorOutcome {
        // The clip as named in the library; decoding uses `target.source`.
        let source = target.clip.as_path();
        let ctx = ui.ctx().clone();
        self.keyboard(&ctx);
        self.player.set_mix(self.edit.tracks.clone());

        let dt = ctx.input(|i| i.stable_dt).min(0.1);
        let (track_levels, master_level) = self.player.levels();
        let playing = self.player.is_playing();
        for (m, l) in self.meters.iter_mut().zip(track_levels.iter().chain(std::iter::repeat(&(0.0, 0.0)))) {
            m.feed(if playing { *l } else { (0.0, 0.0) }, dt);
        }
        self.master.feed(if playing { master_level } else { (0.0, 0.0) }, dt);
        // Keep animating until the meters have fallen back to rest.
        if self.meters.iter().chain([&self.master]).any(|m| m.peak > 1e-3 || m.hold > 1e-3) {
            ctx.request_repaint();
        }

        let mut outcome = EditorOutcome::Stay;
        if std::mem::take(&mut self.rename_requested) {
            outcome = EditorOutcome::Rename;
        }

        // --- Header ---
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.set_min_height(crate::header::HEIGHT);
            if crate::header::back(ui).clicked() {
                if self.dirty() {
                    self.confirm_discard = true;
                } else {
                    outcome = EditorOutcome::Close;
                }
            }
            ui.add_space(10.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let changed = !self.edit.is_identity(&self.info);
                if ui.add(crate::header::button("Done", true)).on_hover_text("Save the edit to this clip. The original recording is kept, so you can change it later.").clicked() {
                    self.player.pause();
                    outcome = self.save(target, None);
                }
                // A real button, not hidden in a menu: keeping the original clip and
                // making a second one is a common need (two highlights from one recording).
                if ui
                    .add_enabled(changed, crate::header::button("Save as new clip…", false))
                    .on_hover_text("Keep this clip unchanged and save the edit as a separate clip")
                    .on_disabled_hover_text("Trim, change the audio or the export settings first")
                    .clicked()
                {
                    self.player.pause();
                    let base = source.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    let suggested = crate::clips::sanitize_name(&format!("{} (edit)", crate::clips::title_for_stem(&base)));
                    self.save_as = Some((suggested, None));
                }
                // How the saved file is made: applies to Done and Save as new.
                let export = crate::header::button(&format!("⚙ Export: {}", crate::export_ui::summary(&self.info, &self.edit)), false)
                    .selected(self.edit.output != media::Output::default());
                let export = ui.add(export).on_hover_text("Export settings: resolution, frame rate, quality, a size to fit under, audio tracks");
                if export.clicked() {
                    self.export_open = !self.export_open;
                }
                // Dev aid: `HESTECLIPS_OPEN_EXPORT=1` opens it at launch.
                if std::env::var_os("HESTECLIPS_OPEN_EXPORT").is_some() && !self.export_shown {
                    self.export_shown = true;
                    self.export_open = true;
                }
                // Its dropdowns open popups of their own, and egui remembers
                // only one open popup: left to egui, opening a dropdown closed
                // this. So it's kept open here, and a click closes it only
                // outside it, and not when it was for an open dropdown.
                let dropdown_open = egui::Popup::is_any_open(ui.ctx());
                let shown = egui::Popup::from_response(&export)
                    .open_bool(&mut self.export_open)
                    .close_behavior(egui::PopupCloseBehavior::IgnoreClicks)
                    .show(|ui| crate::export_ui::ui(ui, &self.info, &mut self.edit));
                if shown.is_some_and(|r| r.response.clicked_elsewhere()) && !dropdown_open && !export.clicked() {
                    self.export_open = false;
                }
                let has_saved_edit = target.source != target.clip;
                if (has_saved_edit || changed)
                    && ui.add(crate::header::button("↺ Revert", false)).on_hover_text("Undo every edit and go back to the original recording").clicked()
                {
                    self.player.pause();
                    outcome = EditorOutcome::Reverted(target.clone());
                }
                if self.dirty() {
                    ui.weak("Unsaved changes");
                }
                ui.add_space(12.0);
                let stem = source.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                if crate::header::title(ui, &crate::clips::title_for_stem(&stem), Some("✂ Editing")) {
                    outcome = EditorOutcome::Rename;
                }
            });
        });
        ui.add_space(6.0);

        // --- Preview: everything left over after the fixed-height controls below ---
        // The divider under it sets the split; the timeline always keeps room for
        // the ruler, the video lane and one audio track (the rest scroll).
        let total = ui.available_height();
        let below_min = DIVIDER_H + 6.0 + 30.0 + 6.0 + RULER_H + VIDEO_H + 2.0 + AUDIO_H + 6.0 + MASTER_H;
        let preview_h = (total * *preview_share).min(total - below_min).max(MIN_PREVIEW_H);
        let (preview_rect, preview_resp) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), preview_h), Sense::click());
        ui.painter().rect_filled(preview_rect, 8, Color32::BLACK);
        if let Some(size) = self.player.update(&ctx, self.dragging.is_some()) {
            let scale = (preview_rect.width() / size.x).min(preview_rect.height() / size.y);
            // On whole screen pixels: a half-pixel offset alone blurs text.
            let ppp = ctx.pixels_per_point();
            let r = Rect::from_center_size(preview_rect.center(), size * scale);
            let r = Rect::from_min_size(((r.min.to_vec2() * ppp).round() / ppp).to_pos2(), r.size());
            self.player.paint(ui, r);
        } else {
            ui.painter().text(preview_rect.center(), Align2::CENTER_CENTER, "…", FontId::proportional(24.0), Color32::GRAY);
        }
        if preview_resp.clicked() {
            self.toggle_play();
        }
        divider(ui, preview_h, total, preview_share);

        // --- Transport ---
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.set_min_height(30.0);
            let icon = |s: &str| egui::Button::new(RichText::new(s).size(16.0)).min_size(Vec2::new(34.0, 28.0));
            if ui.add(icon("⏮")).on_hover_text("Go to start of trim").clicked() {
                self.player.seek(self.edit.start);
            }
            if ui.add(icon("◀")).on_hover_text("Previous frame  (←)").clicked() {
                self.step(-1);
            }
            let play_label = if self.player.is_playing() { "⏸" } else { "▶" };
            if ui.add(icon(play_label).min_size(Vec2::new(48.0, 28.0))).on_hover_text("Play / pause  (Space)").clicked() {
                self.toggle_play();
            }
            if ui.add(icon("▶")).on_hover_text("Next frame  (→)").clicked() {
                self.step(1);
            }
            if ui.add(icon("⏭")).on_hover_text("Go to end of trim").clicked() {
                self.player.seek((self.edit.end - self.info.frame_duration()).max(self.edit.start));
            }
            ui.add_space(10.0);
            // Frames, like an editor: the unit every trim and step works in.
            let total = (self.info.duration * self.info.fps).round() as u64;
            let frame = self.info.frame_index(self.player.time());
            ui.label(RichText::new(frame_code(frame, total)).monospace().size(15.0))
                .on_hover_text(format!("{:.3} s at {} fps", self.player.time(), self.info.fps.round()));
            ui.weak(format!("/ {total} frames"));

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let kept = (self.edit.duration() * self.info.fps).round() as u64;
                ui.label(RichText::new(format!("Clip length {kept} frames · {:.2}s", self.edit.duration())).strong());
                ui.add_space(12.0);
                if ui.button("Set end  (O)").on_hover_text("Trim the end to the playhead").clicked() {
                    self.set_out();
                }
                if ui.button("Set start  (I)").on_hover_text("Trim the start to the playhead").clicked() {
                    self.set_in();
                }
            });
        });

        // --- Timeline: ruler, video and audio lanes on one time axis ---
        ui.add_space(6.0);
        self.timeline(ui);
        ui.add_space(4.0);
        if let Some(e) = &self.player.audio_error {
            ui.colored_label(ui.visuals().warn_fg_color, format!("No sound preview: {e}"));
        }

        if let Some((name, err)) = &mut self.save_as {
            let mut close = false;
            let mut submit = false;
            let modal = egui::Modal::new(egui::Id::new("save_as_new")).show(&ctx, |ui| {
                ui.set_width(380.0);
                ui.heading("Save as new clip");
                ui.label("This clip stays as it is. Your edit is saved as a separate clip:");
                ui.add_space(8.0);
                let field = ui.add(egui::TextEdit::singleline(name).desired_width(f32::INFINITY));
                // Open with the name selected and ready to type over.
                if !field.has_focus() && err.is_none() && !ui.memory(|m| m.has_focus(field.id)) && ui.input(|i| !i.pointer.any_down()) {
                    field.request_focus();
                }
                submit = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                if let Some(e) = err.as_ref() {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let save = egui::Button::new(RichText::new("Save new clip").color(Color32::WHITE)).fill(ACCENT);
                    if ui.add(save).clicked() {
                        submit = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
            if submit {
                let ext = source.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or("mp4".into());
                let dir = source.parent().unwrap_or(Path::new("."));
                match crate::clips::path_for_name(dir, name, &ext, None) {
                    Ok(_) => {
                        let name = name.trim().to_owned();
                        self.save_as = None;
                        outcome = self.save(target, Some(name));
                    }
                    Err(e) => *err = Some(e),
                }
            } else if close || modal.should_close() {
                self.save_as = None;
            }
        }

        if self.confirm_discard {
            let modal = egui::Modal::new(egui::Id::new("discard_edit")).show(&ctx, |ui| {
                ui.set_width(340.0);
                ui.heading("Discard changes?");
                ui.label("Your changes to this clip haven't been saved.");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        self.player.pause();
                        outcome = self.save(target, None);
                        self.confirm_discard = false;
                    }
                    if ui.button("Discard").clicked() {
                        outcome = EditorOutcome::Close;
                        self.confirm_discard = false;
                    }
                    if ui.button("Keep editing").clicked() {
                        self.confirm_discard = false;
                    }
                });
            });
            if modal.should_close() {
                self.confirm_discard = false;
            }
        }

        outcome
    }

    fn dirty(&self) -> bool {
        !same_edit(&self.edit, &self.saved)
    }

    fn save(&mut self, target: &EditTarget, new_name: Option<String>) -> EditorOutcome {
        // The clip already shows exactly this edit: nothing to render.
        let has_saved_edit = target.source != target.clip;
        if new_name.is_none() && has_saved_edit && !self.dirty() {
            return EditorOutcome::Close;
        }
        if new_name.is_none() && self.edit.is_identity(&self.info) {
            // Nothing changed from the original — saving means "no edit".
            return if target.source != target.clip {
                EditorOutcome::Reverted(target.clone())
            } else {
                EditorOutcome::Close
            };
        }
        self.saved = self.edit.clone();
        EditorOutcome::Saved { target: target.clone(), info: self.info.clone(), edit: self.edit.clone(), new_name }
    }

    fn keyboard(&mut self, ctx: &egui::Context) {
        if !ctx.egui_wants_keyboard_input() && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::F2)) {
            self.rename_requested = true;
        }
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let (zoom_in, zoom_out, fit) = ctx.input_mut(|inp| {
            (
                inp.consume_key(egui::Modifiers::NONE, Key::Equals) || inp.consume_key(egui::Modifiers::NONE, Key::Plus),
                inp.consume_key(egui::Modifiers::NONE, Key::Minus),
                inp.consume_key(egui::Modifiers::NONE, Key::Backslash),
            )
        });
        if zoom_in {
            self.zoom_at_playhead(2.0);
        }
        if zoom_out {
            self.zoom_at_playhead(0.5);
        }
        if fit {
            self.view = (0.0, self.info.duration.max(1e-6));
        }
        let (space, left, right, i, o, home, end, shift) = ctx.input_mut(|inp| {
            (
                inp.consume_key(egui::Modifiers::NONE, Key::Space),
                inp.key_pressed(Key::ArrowLeft),
                inp.key_pressed(Key::ArrowRight),
                inp.consume_key(egui::Modifiers::NONE, Key::I),
                inp.consume_key(egui::Modifiers::NONE, Key::O),
                inp.consume_key(egui::Modifiers::NONE, Key::Home),
                inp.consume_key(egui::Modifiers::NONE, Key::End),
                inp.modifiers.shift,
            )
        });
        // Shift+arrow jumps a second; plain arrows step one frame.
        let n = if shift { self.info.fps.round() as i64 } else { 1 };
        if space {
            self.toggle_play();
        }
        if left {
            self.step(-n);
        }
        if right {
            self.step(n);
        }
        if i {
            self.set_in();
        }
        if o {
            self.set_out();
        }
        if home {
            self.player.seek(self.edit.start);
        }
        if end {
            self.player.seek((self.edit.end - self.info.frame_duration()).max(self.edit.start));
        }
    }

    /// Ctrl+scroll (or pinch) zooms around the pointer by `zoom`; scrolling
    /// pans by `pan` points once zoomed in. While playing, the view pages
    /// along to keep the playhead in sight.
    fn zoom_and_pan(&mut self, lanes: Rect, dur: f64, pointer: Option<Pos2>, pan: f32, zoom: f32) {
        if zoom != 1.0 {
            let frac = pointer.map_or(0.5, |p| ((p.x - lanes.left()) / lanes.width()).clamp(0.0, 1.0) as f64);
            self.zoom(zoom as f64, frac);
        }
        if pan != 0.0 && self.view.1 < dur {
            self.view.0 += pan as f64 / lanes.width() as f64 * self.view.1;
        }
        if self.player.is_playing() {
            let t = self.player.time();
            if t < self.view.0 || t > self.view.0 + self.view.1 {
                self.view.0 = t - self.view.1 * 0.05;
            }
        }
        self.view.1 = self.view.1.clamp(self.min_span(), dur);
        self.view.0 = self.view.0.clamp(0.0, (dur - self.view.1).max(0.0));
    }

    /// Zoom by `factor` (> 1 = closer), keeping the time at `frac` across the
    /// timeline where it is.
    fn zoom(&mut self, factor: f64, frac: f64) {
        let (v0, span) = self.view;
        let at = v0 + frac * span;
        let new = (span / factor).clamp(self.min_span(), self.info.duration.max(1e-6));
        self.view = (at - frac * new, new);
    }

    fn min_span(&self) -> f64 {
        (MIN_VIEW_FRAMES * self.info.frame_duration()).min(self.info.duration.max(1e-6))
    }

    /// Zoom around the playhead if it's in view, else around the middle.
    fn zoom_at_playhead(&mut self, factor: f64) {
        let (v0, span) = self.view;
        let frac = (self.player.time() - v0) / span;
        let frac = if (0.0..=1.0).contains(&frac) { frac } else { 0.5 };
        self.zoom(factor, frac);
        let dur = self.info.duration.max(1e-6);
        self.view.0 = self.view.0.clamp(0.0, (dur - self.view.1).max(0.0));
    }

    fn toggle_play(&mut self) {
        if self.player.is_playing() {
            self.player.pause();
            return;
        }
        // Play within the trim; starting from outside it (or the end) restarts at in.
        let t = self.player.time();
        if t < self.edit.start || t >= self.edit.end - self.info.frame_duration() * 0.5 {
            self.player.seek(self.edit.start);
        }
        self.player.play(self.edit.end);
    }

    fn step(&mut self, frames: i64) {
        self.player.pause();
        let f = self.info.frame_index(self.player.time()) as i64 + frames;
        let max = (self.info.duration * self.info.fps).floor() as i64 - 1;
        self.player.seek(f.clamp(0, max.max(0)) as f64 / self.info.fps + 1e-6);
    }

    fn set_in(&mut self) {
        let t = self.info.snap(self.player.time());
        if t < self.edit.end - self.info.frame_duration() {
            self.edit.start = t;
        }
    }

    fn set_out(&mut self) {
        // Out is exclusive: include the frame under the playhead.
        let t = (self.info.snap(self.player.time()) + self.info.frame_duration()).min(self.info.duration);
        if t > self.edit.start + self.info.frame_duration() {
            self.edit.end = t;
        }
    }

    /// One shared time axis for everything, like Premiere / Resolve: a frame
    /// ruler, the video lane (filmstrip) and one lane per audio track, all lined
    /// up so a sound sits directly under the frames it belongs to. The trim
    /// handles and playhead run through every lane.
    ///
    /// Audio volume is edited on the lane itself: drag the yellow line up/down
    /// to change the whole track, double-click it to add a keyframe, drag a
    /// keyframe to shape a ramp, right-click a keyframe to delete it.
    fn timeline(&mut self, ui: &mut egui::Ui) {
        let header_w = 132.0;
        let meter_w = 14.0;
        let (ruler_h, video_h, audio_h) = (RULER_H, VIDEO_H, AUDIO_H);
        let n_audio = self.edit.tracks.len();
        let lanes_h = n_audio as f32 * (audio_h + 2.0);
        // As tall as it needs, up to what's left above the master meter; the
        // audio tracks scroll inside whatever height that leaves them.
        let room = (ui.available_height() - MASTER_H - 6.0).max(ruler_h + video_h + 2.0 + audio_h);
        let total_h = (ruler_h + video_h + 2.0 + lanes_h).min(room);

        let (outer, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), total_h), Sense::hover());
        let audio_view = Rect::from_x_y_ranges(outer.x_range(), outer.top() + ruler_h + video_h + 2.0..=outer.bottom());
        let max_scroll = (lanes_h - 2.0 - audio_view.height()).max(0.0);
        let lanes_x = outer.left() + header_w..=outer.right() - meter_w - 8.0;
        let lanes = Rect::from_x_y_ranges(lanes_x.clone(), outer.y_range());
        let v = ui.visuals().clone();
        let p = ui.painter_at(outer);

        let dur = self.info.duration.max(1e-6);
        let fd = self.info.frame_duration();
        // The wheel over the audio tracks scrolls them when they don't all fit;
        // sideways (Shift+wheel, trackpad) still pans the timeline.
        // Wheel input glides (see `wheel.rs`): read raw, eased here.
        let ctx = ui.ctx().clone();
        let pointer = ctx.pointer_hover_pos();
        let input = if pointer.is_some_and(|p| outer.contains(p)) { crate::wheel::read(&ctx) } else { crate::wheel::Input { pinch: 1.0, ..Default::default() } };
        let scroll = self.glide.step(&ctx, input.scroll);
        let zoom_pts = self.zoom_glide.step1(&ctx, input.zoom);
        let zoom = (ctx.options(|o| o.input_options.scroll_zoom_speed) * zoom_pts).exp() * input.pinch;
        let over_audio = pointer.is_some_and(|p| audio_view.contains(p));
        let lanes_take_vertical = over_audio && max_scroll > 0.0;
        if lanes_take_vertical {
            self.lane_scroll -= scroll.y;
        }
        self.lane_scroll = self.lane_scroll.clamp(0.0, max_scroll);
        let pan = -(scroll.x + if lanes_take_vertical { 0.0 } else { scroll.y });
        self.zoom_and_pan(lanes, dur, pointer, pan, zoom);
        let (v0, span) = self.view;
        let x_of = |t: f64| lanes.left() + ((t - v0) / span) as f32 * lanes.width();
        let t_of = |x: f32| (v0 + ((x - lanes.left()) / lanes.width()).clamp(0.0, 1.0) as f64 * span).clamp(0.0, dur);
        // Everything on the time axis is clipped to the lanes, so a zoomed-in
        // timeline never draws into the headers or meters.
        let lp = ui.painter_at(lanes);

        // --- Ruler: frame numbers, ticks every N frames depending on zoom ---
        let ruler = Rect::from_min_size(Pos2::new(lanes.left(), outer.top()), Vec2::new(lanes.width(), ruler_h));
        let px_per_frame = lanes.width() as f64 / (span * self.info.fps);
        let step = [1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1200.0, 3000.0]
            .into_iter()
            .find(|s| s * px_per_frame >= 70.0)
            .unwrap_or(6000.0);
        let last_frame = ((v0 + span) * self.info.fps).ceil();
        let mut f = (v0 * self.info.fps / step).floor() * step;
        while f <= last_frame {
            let x = x_of(f / self.info.fps);
            lp.vline(x, ruler.bottom() - 6.0..=ruler.bottom(), Stroke::new(1.0, v.weak_text_color()));
            lp.text(Pos2::new(x + 3.0, ruler.top() + 2.0), Align2::LEFT_TOP, format!("{}", f as u64), FontId::monospace(10.5), v.weak_text_color());
            // Minor ticks.
            let minor = if step >= 5.0 { 5 } else { step as usize };
            for k in 1..minor {
                let xm = x + (step * px_per_frame) as f32 * k as f32 / minor as f32;
                lp.vline(xm, ruler.bottom() - 3.0..=ruler.bottom(), Stroke::new(1.0, v.weak_text_color().gamma_multiply(0.5)));
            }
            f += step;
        }
        // Zoomed in: which part of the clip is showing, along the top of the ruler.
        if span < dur - 1e-9 {
            let track = Rect::from_min_size(ruler.min, Vec2::new(ruler.width(), 3.0));
            lp.rect_filled(track, 1, Color32::from_white_alpha(20));
            let a = track.left() + (v0 / dur) as f32 * track.width();
            let b = track.left() + ((v0 + span) / dur) as f32 * track.width();
            lp.rect_filled(Rect::from_x_y_ranges(a..=b.max(a + 4.0), track.y_range()), 1, Color32::from_white_alpha(110));
        }

        // --- Video lane ---
        let video = Rect::from_min_size(Pos2::new(lanes.left(), ruler.bottom()), Vec2::new(lanes.width(), video_h));
        lane_header(&p, &v, Rect::from_min_max(Pos2::new(outer.left(), video.top()), Pos2::new(lanes.left() - 6.0, video.bottom())), "🎬 Video", None);
        p.rect_filled(video, 4, v.extreme_bg_color);
        self.strip.paint(ui, video, v0, v0 + span, dur);
        // Scrub-proxy progress: a thin bar until every frame is scrubbable.
        crate::player::paint_proxy_bar(ui, &self.player, video, v0, v0 + span);

        // --- Audio lanes ---
        // Scrolled lanes are clipped to the space under the video lane.
        let ap = ui.painter_at(audio_view);
        let alp = ui.painter_at(lanes.intersect(audio_view));
        let mut lane_rects = Vec::new();
        let labels: Vec<String> = self.info.source_tracks().iter().map(|a| a.label()).collect();
        let mut y = audio_view.top() - self.lane_scroll;
        for i in 0..n_audio {
            let lane = Rect::from_min_size(Pos2::new(lanes.left(), y), Vec2::new(lanes.width(), audio_h));
            lane_rects.push(lane);
            y += audio_h + 2.0;
            if !lane.intersects(audio_view) {
                continue;
            }
            let track = &self.edit.tracks[i];
            ap.rect_filled(lane, 4, v.extreme_bg_color);
            // Drawn with the volume curve applied: what you'll hear.
            if let Some(w) = self.waves.get(i) {
                let base = if track.muted { Color32::from_gray(90) } else { WAVE_BLUE };
                w.paint(&alp, lane, (v0, span), |t| track.gain_at(t), |_, _| base);
            }
            draw_envelope(&alp, lane, track, dur, x_of);
            let meter = Rect::from_min_size(Pos2::new(outer.right() - meter_w, lane.top()), Vec2::new(meter_w - 4.0, audio_h));
            draw_vmeter(&ap, &v, meter, self.meters.get(i).copied().unwrap_or_default());
        }
        // Scrollbar, between the lanes and their meters, when they don't all fit.
        if max_scroll > 0.0 {
            let track = Rect::from_x_y_ranges(lanes.right() + 2.0..=lanes.right() + 6.0, audio_view.y_range());
            let view_h = audio_view.height();
            let thumb_h = (view_h / (lanes_h - 2.0) * view_h).max(24.0);
            let thumb_y = track.top() + self.lane_scroll / max_scroll * (view_h - thumb_h);
            let bar = ui.interact(track.expand2(Vec2::new(3.0, 0.0)), ui.id().with("lane_scrollbar"), Sense::drag());
            if bar.dragged() {
                self.lane_scroll = (self.lane_scroll + bar.drag_delta().y / (view_h - thumb_h).max(1.0) * max_scroll).clamp(0.0, max_scroll);
            }
            let alpha = if bar.hovered() || bar.dragged() { 160 } else { 90 };
            p.rect_filled(Rect::from_x_y_ranges(track.x_range(), thumb_y..=thumb_y + thumb_h), 2, Color32::from_white_alpha(alpha));
        }

        // --- Trim shading + handles + playhead across all lanes ---
        let body = Rect::from_x_y_ranges(lanes_x, video.top()..=(y - 2.0).min(audio_view.bottom()));
        let (xi, xo) = (x_of(self.edit.start), x_of(self.edit.end));
        let shade = Color32::from_black_alpha(160);
        lp.rect_filled(Rect::from_min_max(body.min, Pos2::new(xi, body.bottom())), 0, shade);
        lp.rect_filled(Rect::from_min_max(Pos2::new(xo, body.top()), body.max), 0, shade);
        let handle_w = 10.0;
        let in_handle = Rect::from_min_max(Pos2::new(xi, body.top()), Pos2::new(xi + handle_w, video.bottom()));
        let out_handle = Rect::from_min_max(Pos2::new(xo - handle_w, body.top()), Pos2::new(xo, video.bottom()));
        lp.rect_stroke(Rect::from_min_max(Pos2::new(xi, video.top()), Pos2::new(xo, video.bottom())), 3, Stroke::new(2.5, HOT_YELLOW), StrokeKind::Inside);
        lp.vline(xi, body.y_range(), Stroke::new(1.5, HOT_YELLOW));
        lp.vline(xo, body.y_range(), Stroke::new(1.5, HOT_YELLOW));
        for h in [in_handle, out_handle] {
            lp.rect_filled(h, 2, HOT_YELLOW);
            lp.vline(h.center().x, h.center().y - 7.0..=h.center().y + 7.0, Stroke::new(2.0, Color32::from_black_alpha(160)));
        }
        // Playhead on the start of the frame showing, with that frame's width
        // marked (Premiere-style) once zoomed in far enough to see it.
        let frame_start = self.info.snap(self.player.time());
        let xp = x_of(frame_start);
        let frame_w = x_of(frame_start + fd) - xp;
        if frame_w >= 3.0 {
            lp.rect_filled(Rect::from_x_y_ranges(xp..=xp + frame_w, ruler.y_range()), 0, Color32::from_white_alpha(70));
            lp.rect_filled(Rect::from_x_y_ranges(xp..=xp + frame_w, body.y_range()), 0, Color32::from_white_alpha(16));
        }
        lp.vline(xp, ruler.top()..=body.bottom(), Stroke::new(1.5, Color32::WHITE));
        let head = xp + frame_w.max(0.0) / 2.0;
        lp.add(egui::Shape::convex_polygon(
            vec![Pos2::new(head - 6.0, ruler.top()), Pos2::new(head + 6.0, ruler.top()), Pos2::new(head, ruler.top() + 8.0)],
            Color32::WHITE,
            Stroke::NONE,
        ));

        // --- Lane headers (drawn after the shading so they stay readable) ---
        for (i, lane) in lane_rects.iter().enumerate() {
            if !lane.intersects(audio_view) {
                continue;
            }
            let head = Rect::from_min_max(Pos2::new(outer.left(), lane.top()), Pos2::new(lanes.left() - 6.0, lane.bottom()));
            let track = &mut self.edit.tracks[i];
            let name = labels.get(i).cloned().unwrap_or_default();
            let db_text = if track.muted {
                "Muted".to_owned()
            } else if track.points.is_empty() {
                db_label(media::to_db(track.gain))
            } else {
                format!("{} keyframes", track.points.len())
            };
            lane_header(&ap, &v, head, &format!("🔊 {name}"), Some(&db_text));
            // Detached child Uis so these overlay buttons don't affect layout,
            // clipped like the lanes when scrolled partly out of view.
            let mut put = |rect: Rect, b: egui::Button| {
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
                child.set_clip_rect(child.clip_rect().intersect(audio_view));
                if rect.intersects(audio_view) { child.add_sized(rect.size(), b) } else { child.allocate_response(Vec2::ZERO, Sense::hover()) }
            };
            let mute_rect = Rect::from_min_size(Pos2::new(head.left() + 4.0, head.bottom() - 25.0), Vec2::new(28.0, 21.0));
            let mute = put(mute_rect, egui::Button::new(if track.muted { "🔇" } else { "M" }).selected(track.muted));
            if mute.on_hover_text(if track.muted { "Unmute" } else { "Mute" }).clicked() {
                track.muted = !track.muted;
            }
            if track.points.len() > 0 || (track.gain - 1.0).abs() > 0.005 {
                let reset_rect = mute_rect.translate(Vec2::new(32.0, 0.0));
                if put(reset_rect, egui::Button::new("↺")).on_hover_text("Reset volume to 0 dB").clicked() {
                    track.points.clear();
                    track.gain = 1.0;
                }
            }
        }

        // --- Interaction ---
        let resp = ui.interact(Rect::from_x_y_ranges(lanes.x_range(), ruler.top()..=body.bottom()), ui.id().with("timeline"), Sense::click_and_drag());
        let pointer = resp.hover_pos();

        // What's under the pointer, in priority order.
        let hit = |pos: Pos2, this: &Self| -> Option<Drag> {
            if in_handle.expand(5.0).contains(pos) {
                return Some(Drag::In);
            }
            if out_handle.expand(5.0).contains(pos) {
                return Some(Drag::Out);
            }
            for (i, lane) in lane_rects.iter().enumerate() {
                if !lane.contains(pos) || !audio_view.contains(pos) {
                    continue;
                }
                let track = &this.edit.tracks[i];
                if let Some(k) = track.points.iter().position(|pt| Pos2::new(x_of(pt.t), y_of_db(*lane, pt.db)).distance(pos) < 7.0) {
                    return Some(Drag::Point(i, k));
                }
                let line_y = y_of_db(*lane, track.db_at(t_of(pos.x)));
                if (pos.y - line_y).abs() < 6.0 {
                    return Some(Drag::Line(i));
                }
            }
            None
        };

        if let Some(pos) = pointer {
            match hit(pos, self) {
                Some(Drag::In | Drag::Out) => ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal),
                Some(Drag::Point(..)) => ui.ctx().set_cursor_icon(egui::CursorIcon::Grab),
                Some(Drag::Line(_)) => ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical),
                _ => {}
            }
            // Hover readout: frame number at the pointer, and the volume on a lane.
            let t = self.info.snap(t_of(pos.x));
            let mut tip = format!("Frame {}", self.info.frame_index(t));
            if let Some(i) = lane_rects.iter().position(|l| l.contains(pos) && audio_view.contains(pos)) {
                tip += &format!("  ·  {}", db_label(self.edit.tracks[i].db_at(t)));
            }
            p.text(Pos2::new(pos.x + 10.0, ruler.top() + 2.0), Align2::LEFT_TOP, tip, FontId::proportional(11.0), v.text_color());
        }

        if resp.drag_started() {
            // Hit-test where the button went down, not where the pointer is now: a
            // drag only starts after a few pixels of movement, by which time a
            // vertical drag has already left the thin volume line or keyframe.
            if let Some(pos) = ui.input(|i| i.pointer.press_origin()).or(resp.interact_pointer_pos()) {
                let d = hit(pos, self).unwrap_or(Drag::Playhead);
                self.dragging = Some(d);
                // Moving the playhead or a trim pauses while you drag and carries on
                // after; shaping the volume never interrupts playback.
                if matches!(d, Drag::In | Drag::Out | Drag::Playhead) {
                    self.resume = self.player.is_playing();
                    self.player.pause();
                }
            }
        }
        if let (Some(pos), Some(d)) = (resp.interact_pointer_pos(), self.dragging) {
            // Dragging the playhead or a trim past either end scrolls a zoomed timeline.
            if matches!(d, Drag::In | Drag::Out | Drag::Playhead) && resp.dragged() {
                let over = if pos.x > lanes.right() { pos.x - lanes.right() } else if pos.x < lanes.left() { pos.x - lanes.left() } else { 0.0 };
                if over != 0.0 {
                    self.view.0 = (self.view.0 + (over / lanes.width()) as f64 * self.view.1 * 0.1).clamp(0.0, (dur - self.view.1).max(0.0));
                    ui.ctx().request_repaint();
                }
            }
            if resp.dragged() || resp.drag_started() {
                let t = self.info.snap(t_of(pos.x));
                match d {
                    Drag::In => {
                        self.edit.start = t.min(self.edit.end - fd);
                        self.player.seek(self.edit.start);
                    }
                    Drag::Out => {
                        self.edit.end = (t + fd).clamp(self.edit.start + fd, self.info.duration);
                        self.player.seek(self.edit.end - fd);
                    }
                    Drag::Playhead => self.player.seek(t + 1e-6),
                    Drag::Line(i) => {
                        // Move the whole curve up/down by the drag distance.
                        let lane = lane_rects[i];
                        // On the first frame, include the movement before the drag registered.
                        let dy = match ui.input(|i| i.pointer.press_origin()) {
                            Some(origin) if resp.drag_started() => pos.y - origin.y,
                            _ => resp.drag_delta().y,
                        };
                        // Moves along the fader, so it follows the pointer exactly.
                        let delta = -dy / (lane.height() - 8.0);
                        let shift = |db: f32| media::fader_db(media::fader_pos(db) + delta);
                        let track = &mut self.edit.tracks[i];
                        if track.points.is_empty() {
                            track.gain = media::from_db(shift(media::to_db(track.gain)));
                        } else {
                            for pt in &mut track.points {
                                pt.db = shift(pt.db);
                            }
                        }
                    }
                    Drag::Point(i, k) => {
                        let lane = lane_rects[i];
                        let track = &mut self.edit.tracks[i];
                        // Keep keyframes in time order: clamp between neighbours.
                        let lo = if k > 0 { track.points[k - 1].t + fd } else { 0.0 };
                        let hi = track.points.get(k + 1).map_or(self.info.duration, |n| n.t - fd);
                        track.points[k].t = t.clamp(lo, hi.max(lo));
                        track.points[k].db = db_of_y(lane, pos.y);
                        if !self.player.is_playing() {
                            self.player.seek(track.points[k].t + 1e-6);
                        }
                    }
                }
            }
        }
        if resp.drag_stopped() {
            // Snap a whole-track level to 0 dB when it lands close, so "unchanged" is easy.
            if let Some(Drag::Line(i)) = self.dragging {
                let track = &mut self.edit.tracks[i];
                if track.points.is_empty() && media::to_db(track.gain).abs() < 2.0 {
                    track.gain = 1.0;
                }
            }
            self.dragging = None;
            if std::mem::take(&mut self.resume) && self.player.time() < self.edit.end - fd {
                self.player.play(self.edit.end);
            }
        }

        if resp.double_clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                if let Some(i) = lane_rects.iter().position(|l| l.contains(pos) && audio_view.contains(pos)) {
                    // Add a keyframe on the curve at this time (keeps the sound unchanged
                    // until it's dragged). The first one adopts the track's flat level.
                    let t = self.info.snap(t_of(pos.x));
                    let track = &mut self.edit.tracks[i];
                    let db = track.db_at(t);
                    if track.points.is_empty() {
                        track.gain = 1.0;
                    }
                    if !track.points.iter().any(|pt| (pt.t - t).abs() < fd / 2.0) {
                        let at = track.points.partition_point(|pt| pt.t < t);
                        track.points.insert(at, media::VolumePoint { t, db });
                    }
                }
            }
        } else if resp.clicked() {
            // Jump there; playing carries on from the new spot.
            if let Some(pos) = resp.interact_pointer_pos() {
                self.player.seek(self.info.snap(t_of(pos.x)) + 1e-6);
            }
        }
        if resp.secondary_clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                if let Some(Drag::Point(i, k)) = hit(pos, self) {
                    let track = &mut self.edit.tracks[i];
                    let removed = track.points.remove(k);
                    // Removing the last keyframe leaves its level as the flat level.
                    if track.points.is_empty() {
                        track.gain = media::from_db(removed.db);
                    }
                }
            }
        }

        // Master meter + hint under the lanes.
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add_space(header_w - 132.0);
            ui.label(RichText::new("Master").strong()).on_hover_text("Track 1 in the saved clip: every unmuted track mixed together");
            let (mrect, _) = ui.allocate_exact_size(Vec2::new(220.0, 12.0), Sense::hover());
            draw_hmeter(ui, mrect, self.master);
            let db = media::to_db(self.master.hold);
            let (txt, color) = if self.master.hold >= 0.999 {
                ("Clipping — turn a track down".to_owned(), REC_RED)
            } else if db > -3.0 {
                (format!("Peak {db:.0} dB · hot"), HOT_YELLOW)
            } else if db > METER_FLOOR {
                (format!("Peak {db:.0} dB"), ui.visuals().weak_text_color())
            } else {
                ("Play to see levels".to_owned(), ui.visuals().weak_text_color())
            };
            ui.label(RichText::new(txt).size(12.0).color(color));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.weak("Ctrl+scroll to zoom, Shift+scroll to pan · drag the yellow line to set volume · double-click to add a keyframe · right-click one to remove it");
            });
        });
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Drag {
    In,
    Out,
    Playhead,
    /// Whole volume line of track i.
    Line(usize),
    /// Keyframe k of track i.
    Point(usize, usize),
}

/// The bar between the preview and the timeline: drag it to give either more
/// room. `share` is the preview's fraction of `total`.
fn divider(ui: &mut egui::Ui, preview_h: f32, total: f32, share: &mut f32) {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), DIVIDER_H), Sense::drag());
    let resp = resp.on_hover_cursor(egui::CursorIcon::ResizeVertical).on_hover_text("Drag to resize the preview and the timeline");
    if resp.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
        *share = ((preview_h + resp.drag_delta().y) / total.max(1.0)).clamp(0.1, 0.95);
    }
    let active = resp.hovered() || resp.dragged();
    let grip = Rect::from_center_size(rect.center(), Vec2::new(44.0, 4.0));
    ui.painter().rect_filled(grip, 2, Color32::from_white_alpha(if active { 150 } else { 50 }));
}

/// Volume lanes are a fader (`media::fader_pos`): silence at the bottom,
/// +24 dB at the top, 0 dB about two thirds up.
fn y_of_db(lane: Rect, db: f32) -> f32 {
    lane.bottom() - 4.0 - media::fader_pos(db) * (lane.height() - 8.0)
}

/// "+3.0 dB", "−12.5 dB", "−∞ dB".
fn db_label(db: f32) -> String {
    if db <= media::SILENT_DB {
        "−∞ dB".to_owned()
    } else if db < -0.05 {
        format!("−{:.1} dB", -db)
    } else {
        format!("+{:.1} dB", db.max(0.0))
    }
}

fn db_of_y(lane: Rect, y: f32) -> f32 {
    // Detents a few pixels wide at 0 dB (unchanged) and at the bottom (silent).
    if (y - y_of_db(lane, 0.0)).abs() <= 3.0 {
        return 0.0;
    }
    let f = (lane.bottom() - 4.0 - y) / (lane.height() - 8.0);
    if f <= 0.02 { media::SILENT_DB } else { media::fader_db(f) }
}

fn lane_header(p: &egui::Painter, v: &egui::Visuals, rect: Rect, title: &str, sub: Option<&str>) {
    p.rect_filled(rect, 4, v.faint_bg_color);
    let title_rect = Rect::from_min_size(rect.min + Vec2::new(6.0, 5.0), Vec2::new(rect.width() - 12.0, 16.0));
    let job = single_line(title, FontId::proportional(12.5), v.strong_text_color(), title_rect.width());
    p.galley(title_rect.min, p.layout_job(job), v.text_color());
    if let Some(sub) = sub {
        p.text(rect.min + Vec2::new(6.0, 22.0), Align2::LEFT_TOP, sub, FontId::monospace(11.0), v.weak_text_color());
    }
}

fn single_line(text: &str, font: FontId, color: Color32, max_width: f32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_width);
    job
}

fn same_edit(a: &Edit, b: &Edit) -> bool {
    a.output == b.output
        && (a.start - b.start).abs() < 1e-6
        && (a.end - b.end).abs() < 1e-6
        && a.tracks.len() == b.tracks.len()
        && a.tracks.iter().zip(&b.tracks).all(|(x, y)| {
            x.muted == y.muted && (x.gain - y.gain).abs() < 1e-4 && x.points == y.points
        })
}

/// The volume "rubber band": a yellow line at the track's level, with keyframes.
fn draw_envelope(p: &egui::Painter, lane: Rect, track: &media::TrackEdit, dur: f64, x_of: impl Fn(f64) -> f32) {
    let color = if track.muted { Color32::from_gray(120) } else { HOT_YELLOW };
    // Faint 0 dB reference line.
    p.hline(lane.x_range(), y_of_db(lane, 0.0), Stroke::new(1.0, Color32::from_white_alpha(25)));
    let mut pts = vec![Pos2::new(x_of(0.0), y_of_db(lane, track.db_at(0.0)))];
    for pt in &track.points {
        pts.push(Pos2::new(x_of(pt.t), y_of_db(lane, pt.db)));
    }
    pts.push(Pos2::new(x_of(dur), y_of_db(lane, track.db_at(dur))));
    p.add(egui::Shape::line(pts, Stroke::new(1.5, color)));
    for pt in &track.points {
        let c = Pos2::new(x_of(pt.t), y_of_db(lane, pt.db));
        p.circle_filled(c, 4.5, color);
        p.circle_stroke(c, 4.5, Stroke::new(1.0, Color32::from_black_alpha(180)));
    }
}

/// Vertical meter beside each lane: RMS bar, peak shadow, peak-hold tick.
fn draw_vmeter(p: &egui::Painter, v: &egui::Visuals, rect: Rect, m: Meter) {
    p.rect_filled(rect, 2, v.extreme_bg_color);
    let y_of = |lin: f32| {
        let db = media::to_db(lin).max(METER_FLOOR);
        rect.bottom() - (1.0 - db / METER_FLOOR) * rect.height()
    };
    if m.peak > 0.0 {
        p.rect_filled(Rect::from_min_max(Pos2::new(rect.left(), y_of(m.peak)), rect.max), 2, zone(m.peak).gamma_multiply(0.45));
    }
    if m.rms > 0.0 {
        p.rect_filled(Rect::from_min_max(Pos2::new(rect.left(), y_of(m.rms)), rect.max), 2, zone(m.rms));
    }
    if m.hold > 0.0 {
        p.hline(rect.x_range(), y_of(m.hold), Stroke::new(2.0, zone(m.hold)));
    }
    for db in [-12.0f32, -3.0] {
        let y = rect.bottom() - (1.0 - db / METER_FLOOR) * rect.height();
        p.hline(rect.x_range(), y, Stroke::new(1.0, Color32::from_white_alpha(40)));
    }
}

/// Horizontal meter (master).
fn draw_hmeter(ui: &egui::Ui, rect: Rect, m: Meter) {
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 3, ui.visuals().extreme_bg_color);
    let x_of = |lin: f32| {
        let db = media::to_db(lin).max(METER_FLOOR);
        rect.left() + (1.0 - db / METER_FLOOR) * rect.width()
    };
    if m.peak > 0.0 {
        p.rect_filled(Rect::from_min_max(rect.min, Pos2::new(x_of(m.peak), rect.bottom())), 3, zone(m.peak).gamma_multiply(0.45));
    }
    if m.rms > 0.0 {
        p.rect_filled(Rect::from_min_max(rect.min, Pos2::new(x_of(m.rms), rect.bottom())), 3, zone(m.rms));
    }
    if m.hold > 0.0 {
        p.vline(x_of(m.hold), rect.y_range(), Stroke::new(2.0, zone(m.hold)));
    }
    for db in [-12.0f32, -3.0] {
        let x = rect.left() + (1.0 - db / METER_FLOOR) * rect.width();
        p.vline(x, rect.y_range(), Stroke::new(1.0, Color32::from_white_alpha(40)));
    }
}

/// Meter colour zones: green to -12 dB, yellow to -3 dB, red above.
fn zone(lin: f32) -> Color32 {
    let db = media::to_db(lin);
    if db >= -3.0 {
        REC_RED
    } else if db >= -12.0 {
        HOT_YELLOW
    } else {
        OK_GREEN
    }
}

/// Frame number, zero-padded to the clip's frame count width: "0142".
fn frame_code(frame: u64, total: u64) -> String {
    let width = total.max(1).to_string().len();
    format!("{frame:0width$}")
}
