//! The clip editor: frame-exact trim and a per-track audio mix with live meters.
//!
//! Layout, top to bottom: the preview; transport (play, frame step, frame
//! counter); a Premiere-style timeline where a frame ruler, the video lane and
//! one lane per audio track share a time axis, with volume edited directly on
//! each audio lane; the master meter for the mix that becomes track 1.
//!
//! Edits are non-destructive (see the `media` crate): Done saves the sidecar and
//! renders the edited file in the background; the original is never touched.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

use egui::{Align2, Color32, FontId, Key, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Vec2};
use media::{ClipInfo, Edit};

use crate::library::{ACCENT, REC_RED};
use crate::player::Player;

/// Filmstrip thumbnail height in pixels (the lane is 56 pt; 2× for Retina).
const STRIP_HEIGHT: u32 = 112;
/// Waveform resolution: peak buckets across the full clip.
const WAVE_BUCKETS: usize = 600;
/// Gain slider range in dB.
const MIN_DB: f32 = -30.0;
const MAX_DB: f32 = 12.0;
/// Meter scale floor in dB.
const METER_FLOOR: f32 = -60.0;
/// Meter colours: healthy / hot / clipping.
const OK_GREEN: Color32 = Color32::from_rgb(80, 200, 120);
const HOT_YELLOW: Color32 = Color32::from_rgb(235, 200, 70);

/// What the editor asks the app to do after a frame.
pub enum EditorOutcome {
    Stay,
    Close,
    /// Saved: render `edit` of `source` in the background.
    /// Saved: render `edit` of `source` in the background — as this clip's edit,
    /// or (`new_name`) as a separate new clip, leaving this clip as it was.
    Saved { source: PathBuf, info: ClipInfo, edit: Edit, new_name: Option<String> },
    Reverted(PathBuf),
}

struct Loaded {
    info: ClipInfo,
    pcm: Vec<Vec<f32>>,
    /// Per source track: peak per bucket, for the waveform.
    waves: Vec<Vec<f32>>,
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
    waves: Vec<Vec<f32>>,
    /// Keyframe thumbnails, sorted by time, as they arrive.
    strip: Vec<(f64, egui::TextureHandle)>,
    strip_rx: Receiver<(f64, media::Frame)>,
    /// Smoothed meter values (peak, rms) per track, then master; and peak holds.
    meters: Vec<Meter>,
    master: Meter,
    dragging: Option<Drag>,
    confirm_discard: bool,
    /// "Save as new clip" dialog: the name being typed, and any problem with it.
    save_as: Option<(String, Option<String>)>,
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
    pub source: PathBuf,
    state: State,
}

impl Editor {
    pub fn open(ctx: &egui::Context, source: &Path) -> Self {
        let (tx, rx) = mpsc::channel();
        let src = source.to_path_buf();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            let result = (|| -> Result<Loaded, String> {
                let info = media::probe(&src).map_err(|e| e.to_string())?;
                let mut pcm = Vec::new();
                let mut waves = Vec::new();
                for track in info.source_tracks() {
                    let samples = media::decode_audio(&src, track.index).map_err(|e| e.to_string())?;
                    waves.push(waveform(&samples));
                    pcm.push(samples);
                }
                Ok(Loaded { info, pcm, waves })
            })();
            let _ = tx.send(result);
            repaint.request_repaint();
        });
        Self { source: source.to_path_buf(), state: State::Loading(rx) }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) -> EditorOutcome {
        if let State::Loading(rx) = &self.state {
            if let Ok(result) = rx.try_recv() {
                self.state = match result {
                    Ok(l) => State::Ready(Box::new(Ready::new(ui.ctx(), &self.source, l))),
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
            State::Ready(r) => r.ui(ui, &self.source),
        }
    }
}

impl Ready {
    fn new(ctx: &egui::Context, source: &Path, l: Loaded) -> Self {
        let mut edit = media::load_edit(source).unwrap_or_else(|| Edit::new(&l.info));
        // Sidecar from an older layout or a hand-edited file: make it fit.
        if edit.tracks.len() != l.pcm.len() {
            edit.tracks = Edit::new(&l.info).tracks;
        }
        edit.end = edit.end.min(l.info.duration);
        let gains = edit.tracks.clone();
        let n = l.pcm.len();
        let mut player = Player::new(ctx, source, l.info.clone(), l.pcm, gains);
        player.seek(edit.start);

        // Filmstrip: every keyframe, decoded in parallel; cells fill in as they arrive.
        let (tx, strip_rx) = mpsc::channel();
        let (src, repaint) = (source.to_path_buf(), ctx.clone());
        std::thread::spawn(move || {
            let _ = media::keyframe_strip(&src, STRIP_HEIGHT, |t, f| {
                repaint.request_repaint();
                tx.send((t, f)).is_ok() // stops decoding if the editor closed
            });
        });

        Self {
            info: l.info,
            player,
            saved: edit.clone(),
            edit,
            waves: l.waves,
            strip: Vec::new(),
            strip_rx,
            meters: vec![Meter::default(); n],
            master: Meter::default(),
            dragging: None,
            confirm_discard: false,
            save_as: None,
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, source: &Path) -> EditorOutcome {
        let ctx = ui.ctx().clone();
        while let Ok((t, f)) = self.strip_rx.try_recv() {
            let img = egui::ColorImage::from_rgba_unmultiplied([f.width as usize, f.height as usize], &f.rgba);
            let tex = ctx.load_texture(format!("strip{t:.3}"), img, egui::TextureOptions::LINEAR);
            let at = self.strip.partition_point(|(k, _)| *k < t);
            self.strip.insert(at, (t, tex));
        }

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

        // --- Header ---
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button(RichText::new("‹ Clips").size(15.0)).clicked() {
                if self.dirty() {
                    self.confirm_discard = true;
                } else {
                    outcome = EditorOutcome::Close;
                }
            }
            ui.add_space(6.0);
            ui.label(RichText::new("Edit clip").size(18.0).strong());
            ui.weak(crate::file_name(source));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let changed = !self.edit.is_identity(&self.info);
                let done = egui::Button::new(RichText::new("Done").size(15.0).color(Color32::WHITE))
                    .fill(ACCENT)
                    .min_size(Vec2::new(80.0, 30.0))
                    .corner_radius(8);
                if ui.add(done).on_hover_text("Save the edit to this clip. The original recording is kept, so you can change it later.").clicked() {
                    self.player.pause();
                    outcome = self.save(source, None);
                }
                // A real button, not hidden in a menu: keeping the original clip and
                // making a second one is a common need (two highlights from one recording).
                let save_new = egui::Button::new(RichText::new("Save as new clip…").size(14.0)).min_size(Vec2::new(0.0, 30.0)).corner_radius(8);
                if ui
                    .add_enabled(changed, save_new)
                    .on_hover_text("Keep this clip unchanged and save the edit as a separate clip")
                    .on_disabled_hover_text("Trim or change the audio first")
                    .clicked()
                {
                    self.player.pause();
                    let base = source.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    let suggested = crate::clips::sanitize_name(&format!("{} (edit)", crate::clips::title_for_stem(&base)));
                    self.save_as = Some((suggested, None));
                }
                let has_saved_edit = media::load_edit(source).is_some();
                if (has_saved_edit || changed)
                    && ui.button(RichText::new("↺ Revert").size(14.0)).on_hover_text("Undo every edit and go back to the original recording").clicked()
                {
                    self.player.pause();
                    outcome = EditorOutcome::Reverted(source.to_path_buf());
                }
                if self.dirty() {
                    ui.weak("Unsaved changes");
                }
            });
        });
        ui.add_space(6.0);

        // --- Preview: everything left over after the fixed-height controls below ---
        let tracks_h = self.edit.tracks.len() as f32 * 66.0;
        let controls_h = 40.0 + 22.0 + 56.0 + tracks_h + 40.0 + 30.0;
        let preview_h = (ui.available_height() - controls_h).max(140.0);
        let (preview_rect, preview_resp) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), preview_h), Sense::click());
        ui.painter().rect_filled(preview_rect, 8, Color32::BLACK);
        if let Some(tex) = self.player.update(&ctx, self.dragging.is_some()) {
            let size = tex.size_vec2();
            let scale = (preview_rect.width() / size.x).min(preview_rect.height() / size.y);
            let r = Rect::from_center_size(preview_rect.center(), size * scale);
            egui::Image::from_texture((tex.id(), r.size())).paint_at(ui, r);
        } else {
            ui.painter().text(preview_rect.center(), Align2::CENTER_CENTER, "…", FontId::proportional(24.0), Color32::GRAY);
        }
        if preview_resp.clicked() {
            self.toggle_play();
        }

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
                        outcome = self.save(source, Some(name));
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
                        outcome = self.save(source, None);
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

    fn save(&mut self, source: &Path, new_name: Option<String>) -> EditorOutcome {
        if new_name.is_none() && self.edit.is_identity(&self.info) {
            // Nothing changed from the original — saving means "no edit".
            return if media::load_edit(source).is_some() {
                EditorOutcome::Reverted(source.to_path_buf())
            } else {
                EditorOutcome::Close
            };
        }
        self.saved = self.edit.clone();
        EditorOutcome::Saved { source: source.to_path_buf(), info: self.info.clone(), edit: self.edit.clone(), new_name }
    }

    fn keyboard(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return;
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
        let ruler_h = 22.0;
        let video_h = 56.0;
        let audio_h = 64.0;
        let n_audio = self.edit.tracks.len();
        let total_h = ruler_h + video_h + n_audio as f32 * (audio_h + 2.0) + 4.0;

        let (outer, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), total_h), Sense::hover());
        let lanes_x = outer.left() + header_w..=outer.right() - meter_w - 8.0;
        let lanes = Rect::from_x_y_ranges(lanes_x.clone(), outer.y_range());
        let v = ui.visuals().clone();
        let p = ui.painter_at(outer);

        let dur = self.info.duration.max(1e-6);
        let x_of = |t: f64| lanes.left() + (t / dur) as f32 * lanes.width();
        let t_of = |x: f32| ((x - lanes.left()) / lanes.width()).clamp(0.0, 1.0) as f64 * dur;
        let fd = self.info.frame_duration();

        // --- Ruler: frame numbers, ticks every N frames depending on zoom ---
        let ruler = Rect::from_min_size(Pos2::new(lanes.left(), outer.top()), Vec2::new(lanes.width(), ruler_h));
        let total_frames = (dur * self.info.fps).round().max(1.0);
        let px_per_frame = lanes.width() as f64 / total_frames;
        let step = [1.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1200.0, 3000.0]
            .into_iter()
            .find(|s| s * px_per_frame >= 70.0)
            .unwrap_or(6000.0);
        let mut f = 0.0;
        while f <= total_frames {
            let x = lanes.left() + (f * px_per_frame) as f32;
            p.vline(x, ruler.bottom() - 6.0..=ruler.bottom(), Stroke::new(1.0, v.weak_text_color()));
            p.text(Pos2::new(x + 3.0, ruler.top() + 2.0), Align2::LEFT_TOP, format!("{}", f as u64), FontId::monospace(10.5), v.weak_text_color());
            // Minor ticks.
            for k in 1..5 {
                let xm = x + (step * px_per_frame) as f32 * k as f32 / 5.0;
                if xm < lanes.right() {
                    p.vline(xm, ruler.bottom() - 3.0..=ruler.bottom(), Stroke::new(1.0, v.weak_text_color().gamma_multiply(0.5)));
                }
            }
            f += step;
        }

        // --- Video lane ---
        let video = Rect::from_min_size(Pos2::new(lanes.left(), ruler.bottom()), Vec2::new(lanes.width(), video_h));
        lane_header(&p, &v, Rect::from_min_max(Pos2::new(outer.left(), video.top()), Pos2::new(lanes.left() - 6.0, video.bottom())), "🎬 Video", None);
        p.rect_filled(video, 4, v.extreme_bg_color);
        // As many uncropped thumbnails as fit; each cell shows the keyframe nearest
        // its middle (so the strip is complete early, then sharpens in time).
        if let Some((_, first)) = self.strip.first() {
            let aspect = first.size_vec2().x / first.size_vec2().y;
            let cells = ((video.width() / (video_h * aspect)).ceil() as usize).max(1);
            let cell_w = video.width() / cells as f32;
            let painter = ui.painter_at(video);
            for i in 0..cells {
                let t = dur * (i as f64 + 0.5) / cells as f64;
                let at = self.strip.partition_point(|(k, _)| *k < t);
                let near = [at.checked_sub(1), Some(at)]
                    .into_iter()
                    .flatten()
                    .filter_map(|j| self.strip.get(j))
                    .min_by(|a, b| (a.0 - t).abs().total_cmp(&(b.0 - t).abs()));
                if let Some((_, tex)) = near {
                    let cell = Rect::from_min_size(Pos2::new(video.left() + i as f32 * cell_w, video.top()), Vec2::new(cell_w, video_h));
                    let uv = crop_uv(tex.size_vec2(), cell.size());
                    painter.image(tex.id(), cell, uv, Color32::WHITE);
                }
            }
        }
        // Scrub-proxy progress: a thin bar until every frame is scrubbable.
        let prog = self.player.proxy_progress();
        if prog < 1.0 {
            let bar = Rect::from_min_size(Pos2::new(video.left(), video.bottom() - 3.0), Vec2::new(video.width() * prog, 3.0));
            p.rect_filled(bar, 0, ACCENT);
            ui.ctx().request_repaint();
        }

        // --- Audio lanes ---
        let mut lane_rects = Vec::new();
        let labels: Vec<String> = self.info.source_tracks().iter().map(|a| a.label()).collect();
        let mut y = video.bottom() + 2.0;
        for i in 0..n_audio {
            let lane = Rect::from_min_size(Pos2::new(lanes.left(), y), Vec2::new(lanes.width(), audio_h));
            lane_rects.push(lane);
            y += audio_h + 2.0;
            let track = &self.edit.tracks[i];
            p.rect_filled(lane, 4, v.extreme_bg_color);
            // Waveform drawn with the volume curve applied: what you'll hear.
            draw_wave(&p, lane, self.waves.get(i).map(Vec::as_slice).unwrap_or(&[]), dur, |t| track.gain_at(t), track.muted);
            draw_envelope(&p, lane, track, dur, x_of);
            let meter = Rect::from_min_size(Pos2::new(outer.right() - meter_w, lane.top()), Vec2::new(meter_w - 4.0, audio_h));
            draw_vmeter(&p, &v, meter, self.meters.get(i).copied().unwrap_or_default());
        }

        // --- Trim shading + handles + playhead across all lanes ---
        let body = Rect::from_x_y_ranges(lanes_x, video.top()..=y - 2.0);
        let (xi, xo) = (x_of(self.edit.start), x_of(self.edit.end));
        let shade = Color32::from_black_alpha(160);
        p.rect_filled(Rect::from_min_max(body.min, Pos2::new(xi, body.bottom())), 0, shade);
        p.rect_filled(Rect::from_min_max(Pos2::new(xo, body.top()), body.max), 0, shade);
        let handle_w = 10.0;
        let in_handle = Rect::from_min_max(Pos2::new(xi, body.top()), Pos2::new(xi + handle_w, video.bottom()));
        let out_handle = Rect::from_min_max(Pos2::new(xo - handle_w, body.top()), Pos2::new(xo, video.bottom()));
        p.rect_stroke(Rect::from_min_max(Pos2::new(xi, video.top()), Pos2::new(xo, video.bottom())), 3, Stroke::new(2.5, HOT_YELLOW), StrokeKind::Inside);
        p.vline(xi, body.y_range(), Stroke::new(1.5, HOT_YELLOW));
        p.vline(xo, body.y_range(), Stroke::new(1.5, HOT_YELLOW));
        for h in [in_handle, out_handle] {
            p.rect_filled(h, 2, HOT_YELLOW);
            p.vline(h.center().x, h.center().y - 7.0..=h.center().y + 7.0, Stroke::new(2.0, Color32::from_black_alpha(160)));
        }
        let xp = x_of(self.player.time());
        p.vline(xp, ruler.top()..=body.bottom(), Stroke::new(1.5, Color32::WHITE));
        p.add(egui::Shape::convex_polygon(
            vec![Pos2::new(xp - 6.0, ruler.top()), Pos2::new(xp + 6.0, ruler.top()), Pos2::new(xp, ruler.top() + 8.0)],
            Color32::WHITE,
            Stroke::NONE,
        ));

        // --- Lane headers (drawn after the shading so they stay readable) ---
        for (i, lane) in lane_rects.iter().enumerate() {
            let head = Rect::from_min_max(Pos2::new(outer.left(), lane.top()), Pos2::new(lanes.left() - 6.0, lane.bottom()));
            let track = &mut self.edit.tracks[i];
            let name = labels.get(i).cloned().unwrap_or_default();
            let db_text = if track.muted {
                "Muted".to_owned()
            } else if track.points.is_empty() {
                format!("{:+.1} dB", media::to_db(track.gain))
            } else {
                format!("{} keyframes", track.points.len())
            };
            lane_header(&p, &v, head, &format!("🔊 {name}"), Some(&db_text));
            // Detached child Uis so these overlay buttons don't affect layout.
            let mut put = |rect: Rect, b: egui::Button| ui.new_child(egui::UiBuilder::new().max_rect(rect)).add_sized(rect.size(), b);
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
                if !lane.contains(pos) {
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
            if let Some(i) = lane_rects.iter().position(|l| l.contains(pos)) {
                tip += &format!("  ·  {:+.1} dB", self.edit.tracks[i].db_at(t));
            }
            p.text(Pos2::new(pos.x + 10.0, ruler.top() + 2.0), Align2::LEFT_TOP, tip, FontId::proportional(11.0), v.text_color());
        }

        if resp.drag_started() {
            // Hit-test where the button went down, not where the pointer is now: a
            // drag only starts after a few pixels of movement, by which time a
            // vertical drag has already left the thin volume line or keyframe.
            if let Some(pos) = ui.input(|i| i.pointer.press_origin()).or(resp.interact_pointer_pos()) {
                self.dragging = Some(hit(pos, self).unwrap_or(Drag::Playhead));
                self.player.pause();
            }
        }
        if let (Some(pos), Some(d)) = (resp.interact_pointer_pos(), self.dragging) {
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
                        let delta_db = -dy / lane.height() * (MAX_DB - MIN_DB);
                        let track = &mut self.edit.tracks[i];
                        if track.points.is_empty() {
                            track.gain = media::from_db((media::to_db(track.gain) + delta_db).clamp(MIN_DB, MAX_DB));
                        } else {
                            for pt in &mut track.points {
                                pt.db = (pt.db + delta_db).clamp(MIN_DB, MAX_DB);
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
                        self.player.seek(track.points[k].t + 1e-6);
                    }
                }
            }
        }
        if resp.drag_stopped() {
            // Snap a whole-track level to 0 dB when it lands close, so "unchanged" is easy.
            if let Some(Drag::Line(i)) = self.dragging {
                let track = &mut self.edit.tracks[i];
                if track.points.is_empty() && media::to_db(track.gain).abs() < 0.75 {
                    track.gain = 1.0;
                }
            }
            self.dragging = None;
        }

        if resp.double_clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                if let Some(i) = lane_rects.iter().position(|l| l.contains(pos)) {
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
            if let Some(pos) = resp.interact_pointer_pos() {
                self.player.pause();
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
                ui.weak("Drag the yellow line to set volume · double-click to add a keyframe · right-click a keyframe to remove it");
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

/// Volume lanes map MIN_DB..MAX_DB to bottom..top (0 dB sits ~70% up).
fn y_of_db(lane: Rect, db: f32) -> f32 {
    let f = (db.clamp(MIN_DB, MAX_DB) - MIN_DB) / (MAX_DB - MIN_DB);
    lane.bottom() - 4.0 - f * (lane.height() - 8.0)
}

fn db_of_y(lane: Rect, y: f32) -> f32 {
    let f = (lane.bottom() - 4.0 - y) / (lane.height() - 8.0);
    let db = MIN_DB + f.clamp(0.0, 1.0) * (MAX_DB - MIN_DB);
    // Gentle detent at 0 dB.
    if db.abs() < 0.75 { 0.0 } else { db }
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
    (a.start - b.start).abs() < 1e-6
        && (a.end - b.end).abs() < 1e-6
        && a.tracks.len() == b.tracks.len()
        && a.tracks.iter().zip(&b.tracks).all(|(x, y)| {
            x.muted == y.muted && (x.gain - y.gain).abs() < 1e-4 && x.points == y.points
        })
}

/// Peak per bucket across the whole track (interleaved stereo input).
fn waveform(samples: &[f32]) -> Vec<f32> {
    let frames = samples.len() / 2;
    if frames == 0 {
        return Vec::new();
    }
    let per = frames.div_ceil(WAVE_BUCKETS).max(1);
    samples
        .chunks(per * 2)
        .map(|c| c.iter().fold(0.0f32, |m, s| m.max(s.abs())))
        .collect()
}

/// Mirrored waveform, dB-scaled (like the meters) so quiet audio is still
/// visible, with the volume curve applied.
fn draw_wave(p: &egui::Painter, lane: Rect, wave: &[f32], dur: f64, gain_at: impl Fn(f64) -> f32, muted: bool) {
    if wave.is_empty() {
        return;
    }
    let mid = lane.center().y;
    let half = lane.height() / 2.0 - 3.0;
    let n = wave.len();
    let height_of = |a: f32| ((media::to_db(a) - METER_FLOOR) / -METER_FLOOR).clamp(0.0, 1.0);
    let cols = lane.width().max(1.0) as usize;
    let base = if muted { Color32::from_gray(90) } else { Color32::from_rgb(70, 130, 220) };
    for c in 0..cols {
        let frac = c as f64 / cols as f64;
        let a = wave[((frac * n as f64) as usize).min(n - 1)] * gain_at(frac * dur);
        let h = (height_of(a) * half).max(0.5);
        let color = if a >= 0.999 { REC_RED } else { base };
        p.vline(lane.left() + c as f32 + 0.5, mid - h..=mid + h, Stroke::new(1.0, color));
    }
}

/// The volume "rubber band": a yellow line at the track's level, with keyframes.
fn draw_envelope(p: &egui::Painter, lane: Rect, track: &media::TrackEdit, dur: f64, x_of: impl Fn(f64) -> f32) {
    let color = if track.muted { Color32::from_gray(120) } else { HOT_YELLOW };
    // Faint 0 dB reference line.
    p.hline(lane.x_range(), y_of_db(lane, 0.0), Stroke::new(1.0, Color32::from_white_alpha(25)));
    let mut pts = vec![Pos2::new(lane.left(), y_of_db(lane, track.db_at(0.0)))];
    for pt in &track.points {
        pts.push(Pos2::new(x_of(pt.t), y_of_db(lane, pt.db)));
    }
    pts.push(Pos2::new(lane.right(), y_of_db(lane, track.db_at(dur))));
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

/// UV rect that center-crops a texture of `tex` size to fill `target`'s aspect.
fn crop_uv(tex: Vec2, target: Vec2) -> Rect {
    let (ta, ra) = (tex.x / tex.y, target.x / target.y);
    if ta > ra {
        let w = ra / ta;
        Rect::from_min_max(Pos2::new((1.0 - w) / 2.0, 0.0), Pos2::new((1.0 + w) / 2.0, 1.0))
    } else {
        let h = ta / ra;
        Rect::from_min_max(Pos2::new(0.0, (1.0 - h) / 2.0), Pos2::new(1.0, (1.0 + h) / 2.0))
    }
}
