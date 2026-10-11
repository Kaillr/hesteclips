//! The page where the snippets the voice detector kept
//! ([`crate::voice_samples`]) are listened to, trimmed to what matters and
//! judged: should the highlighted part save a clip? It should when "hashtag
//! HesteClip that" is said well enough that you'd want it to; a muddled try is a
//! no.
//! Nothing is assumed from what made the detector keep one: it may have fired
//! on a laugh, and a near miss may be the phrase said in a way it didn't take.
//!
//! Sound and picture are the editor's: the same player engine (so dragging
//! over the waveform scrubs, keeping the pitch) and the same waveform.
//!
//! Judged snippets can be exported as one zip file, to send to whoever
//! trains the detector.

use std::time::{Duration, Instant};

use egui::{Color32, Key, Rect, RichText, Sense, Stroke, pos2, vec2};
use media::PREVIEW_RATE;

use crate::player::SoundPlayer;
use crate::ui_kit;
use crate::voice_samples::{self, Kind, Label, RATE, Sample};
use crate::waveform::Waveform;
use crate::{App, Page};

/// The default part of a snippet that has a moment: this long before it and
/// after it (guesses: the phrase takes about two seconds).
const BEFORE_MOMENT: f32 = 3.0;
const AFTER_MOMENT: f32 = 0.5;
/// The closest the trim's ends can be.
const MIN_TRIM: f32 = 0.3;

#[derive(Default)]
pub struct Review {
    list: Vec<Sample>,
    loaded: Option<Instant>,
    selected: Option<String>,
    wave: Option<Waveform>,
    sound: Option<SoundPlayer>,
    /// The selected snippet's highlighted part.
    trim: (f32, f32),
    confirm_delete_all: bool,
}

impl Review {
    /// (to listen to, in all), from a listing at most a few seconds old.
    pub fn summary(&mut self) -> (usize, usize) {
        if self.loaded.is_none_or(|t| t.elapsed() > Duration::from_secs(3)) {
            self.list = voice_samples::list();
            self.loaded = Some(Instant::now());
        }
        (self.list.iter().filter(|s| s.meta.label == Label::Unreviewed).count(), self.list.len())
    }

    /// Read what's kept, and start with the newest not yet judged.
    fn open(&mut self) {
        *self = Self::default();
        self.list = voice_samples::list();
        self.loaded = Some(Instant::now());
        if let Some(id) = self.list.iter().find(|s| s.meta.label == Label::Unreviewed).map(|s| s.meta.id.clone()) {
            self.select(&id);
        }
    }

    fn current(&self) -> Option<&Sample> {
        let id = self.selected.as_deref()?;
        self.list.iter().find(|s| s.meta.id == id)
    }

    fn index(&self) -> Option<usize> {
        let id = self.selected.as_deref()?;
        self.list.iter().position(|s| s.meta.id == id)
    }

    fn deselect(&mut self) {
        self.selected = None;
        self.wave = None;
        self.sound = None;
    }

    fn select(&mut self, id: &str) {
        self.deselect();
        self.selected = Some(id.to_owned());
        let Some(audio) = voice_samples::load_audio(id) else { return };
        let Some(s) = self.current() else { return };
        self.trim = match (s.meta.trim, s.meta.moment) {
            (Some(t), _) => t,
            (None, Some(m)) => ((m - BEFORE_MOMENT).max(0.0), (m + AFTER_MOMENT).min(s.secs)),
            (None, None) => (0.0, s.secs),
        };
        let stereo: Vec<f32> = audio.iter().flat_map(|s| [*s, *s]).collect();
        self.wave = Some(Waveform::new(&stereo, RATE));
        // Quiet mics are made audible, and a loud laugh no louder than a voice.
        let peak = audio.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let gain = if peak > 0.0 { (0.5 / peak).min(12.0) } else { 1.0 };
        let sound = SoundPlayer::new(media::pcm::Pcm::from_tracks(vec![to_preview(&audio, gain)]), audio.len() as f64 / RATE as f64);
        sound.seek(self.trim.0 as f64);
        self.sound = Some(sound);
    }

    fn select_index(&mut self, i: usize) {
        if let Some(id) = self.list.get(i).map(|s| s.meta.id.clone()) {
            self.select(&id);
        }
    }

    /// Judge the selected snippet, then on to the next one not yet judged
    /// (none left: back to the list).
    fn judge(&mut self, label: Label) {
        let Some(i) = self.index() else { return };
        let trim = self.trim;
        let meta = &mut self.list[i].meta;
        meta.label = label;
        meta.trim = Some(trim);
        voice_samples::save_meta(meta);
        match (i + 1..self.list.len()).chain(0..i).find(|&j| self.list[j].meta.label == Label::Unreviewed) {
            Some(j) => self.select_index(j),
            None => self.deselect(),
        }
    }

    /// Move on without answering: the next one not yet answered, else the next.
    fn skip(&mut self) {
        let Some(i) = self.index() else { return };
        let n = self.list.len();
        let target = (i + 1..n).chain(0..i).find(|&j| self.list[j].meta.label == Label::Unreviewed).or((n > 1).then_some((i + 1) % n));
        if let Some(j) = target {
            self.select_index(j);
        }
    }

    fn remove(&mut self) {
        let Some(i) = self.index() else { return };
        voice_samples::delete(&self.list[i].meta.id);
        self.list.remove(i);
        self.deselect();
        // On to the next one not yet judged, if any.
        if let Some(j) = (i..self.list.len()).chain(0..i.min(self.list.len())).find(|&j| self.list[j].meta.label == Label::Unreviewed) {
            self.select_index(j);
        }
    }

    /// Play the highlighted part (from the playhead if it's inside), or stop.
    fn play_or_stop(&mut self) {
        let Some(sound) = &self.sound else { return };
        if sound.is_playing() {
            sound.pause();
            return;
        }
        let (a, b) = (self.trim.0 as f64, self.trim.1 as f64);
        if !(a..b - 0.05).contains(&sound.time()) {
            sound.seek(a);
        }
        sound.play(b);
    }
}

/// The sound at the player's rate, as stereo.
fn to_preview(audio: &[f32], gain: f32) -> Vec<f32> {
    let ratio = PREVIEW_RATE as f64 / RATE as f64;
    let n = (audio.len() as f64 * ratio) as usize;
    let mut out = Vec::with_capacity(n * 2);
    for i in 0..n {
        let x = i as f64 / ratio;
        let k = x as usize;
        let f = (x - k as f64) as f32;
        let (a, b) = (audio[k.min(audio.len() - 1)], audio[(k + 1).min(audio.len() - 1)]);
        let s = (a + (b - a) * f) * gain;
        out.extend([s, s]);
    }
    out
}

fn when(s: &Sample) -> String {
    chrono::DateTime::from_timestamp(s.meta.at, 0).map(|t| t.with_timezone(&chrono::Local).format("%d %b %H:%M").to_string()).unwrap_or_default()
}

impl App {
    pub(crate) fn open_voice_review(&mut self) {
        self.voice_review.open();
        self.page = Page::VoiceReview;
    }

    pub(crate) fn voice_review_page(&mut self, ui: &mut egui::Ui) {
        let mut r = std::mem::take(&mut self.voice_review);
        let mut back = false;
        let mut say: Option<(String, bool)> = None;
        let collecting = self.settings.voice_samples;
        let ctx = ui.ctx().clone();

        // Keys, unless a dialog or a text field has them.
        let keys = !ctx.egui_wants_keyboard_input() && !ui_kit::overlay_open(&ctx);
        let press = |key: Key| keys && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, key));
        let (yes, no, play) = (press(Key::Y), press(Key::N), press(Key::Space));
        let delete = press(Key::Delete) || press(Key::Backspace);
        let (previous, next) = (press(Key::ArrowLeft), press(Key::ArrowRight));
        if press(Key::Escape) {
            back = true;
        }
        if let Some(i) = r.index() {
            if previous && i > 0 {
                r.select_index(i - 1);
            }
            if next && i + 1 < r.list.len() {
                r.select_index(i + 1);
            }
        }
        if play {
            r.play_or_stop();
        }
        if yes {
            r.judge(Label::Trigger);
        }
        if no {
            r.judge(Label::Ignore);
        }
        if delete {
            r.remove();
        }

        ui_kit::page(ui, "voice_review", 720.0, |ui| {
            let top = ui.allocate_space(vec2(1.0, 1.0)).1;
            ui.add_space(8.0);
            let total = r.list.len();
            let answered = r.list.iter().filter(|s| s.meta.label != Label::Unreviewed).count();

            // Where you are, and how far along.
            ui.horizontal(|ui| {
                if ui.add(egui::Button::new(RichText::new("\u{2039}  Settings").color(ui.visuals().weak_text_color())).frame(false)).clicked() {
                    back = true;
                }
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Voice samples").size(22.0).strong());
                if total > 0 {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.weak(format!("{answered} of {total} answered"));
                    });
                }
            });
            if total > 0 {
                ui.add_space(8.0);
                ui.add(egui::ProgressBar::new(answered as f32 / total as f32).desired_height(4.0).fill(ui_kit::ACCENT).corner_radius(2));
            }
            ui.add_space(16.0);

            if total == 0 {
                ui_kit::card(ui, |ui| {
                    ui.add_space(8.0);
                    ui.label(RichText::new("No samples yet").size(16.0).strong());
                    ui.add_space(4.0);
                    if collecting {
                        ui.weak("They appear while the replay buffer runs, when the voice command saves a clip or nearly does.");
                    } else {
                        ui.weak("Turn on \"Keep samples to improve it\" under Voice in Settings, and they appear while the replay buffer runs.");
                    }
                    ui.add_space(8.0);
                });
                return;
            }

            if let Some(s) = r.current().cloned() {
                ui_kit::card(ui, |ui| self.review_card(ui, &mut r, &s, &mut say));
            } else {
                ui_kit::card(ui, |ui| {
                    ui.add_space(8.0);
                    ui.label(RichText::new("All caught up").size(18.0).strong());
                    ui.add_space(4.0);
                    ui.weak("Every sample has an answer. Pick one below to change it, or share them to help improve the detector.");
                    ui.add_space(8.0);
                });
            }

            // Sharing.
            let ready = r.list.iter().filter(|s| s.shareable()).count();
            ui.add_space(24.0);
            ui.label(RichText::new("Help improve the detector").size(15.0).strong());
            ui.add_space(8.0);
            ui_kit::card(ui, |ui| {
                ui_kit::hint(
                    ui,
                    "Export your answers as one zip file and send it to the developer yourself. It holds only the highlighted part of each sample and your answer, with no name. Nothing is uploaded. Delete any sample you don't want to share.",
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if r.confirm_delete_all {
                        if ui.add(ui_kit::danger_button(format!("Delete all {total}"))).clicked() {
                            voice_samples::delete_all();
                            r.list.clear();
                            r.deselect();
                            r.confirm_delete_all = false;
                        }
                        if ui.add(ui_kit::button("Cancel", false)).clicked() {
                            r.confirm_delete_all = false;
                        }
                    } else if ui.add(egui::Button::new(RichText::new("Delete all").color(ui_kit::DANGER)).frame(false)).clicked() {
                        r.confirm_delete_all = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let label = if ready == 0 { "Export".to_owned() } else { format!("Export {ready}") };
                        if ui.add_enabled(ready > 0, ui_kit::button(label, true)).clicked() {
                            let picked = rfd::FileDialog::new().set_file_name("hesteclips-voice-samples.zip").add_filter("Zip file", &["zip"]).save_file();
                            if let Some(path) = picked {
                                say = Some(match voice_samples::export(&path, &r.list) {
                                    Ok(n) => (format!("Saved {n} samples to {}", path.display()), false),
                                    Err(e) => (e, true),
                                });
                            }
                        }
                    });
                });
            });

            // All of them, out of the way until wanted.
            ui.add_space(24.0);
            let mut pick = None;
            egui::CollapsingHeader::new(RichText::new(format!("All samples ({total})")).size(15.0).strong()).default_open(false).show(ui, |ui| {
                ui.add_space(4.0);
                for s in &r.list {
                    let selected = r.selected.as_deref() == Some(&s.meta.id);
                    let row = ui.horizontal(|ui| {
                        let (dot, _) = ui.allocate_exact_size(vec2(14.0, 18.0), Sense::hover());
                        let center = dot.center();
                        let weak = ui.visuals().weak_text_color();
                        match s.meta.label {
                            Label::Trigger => ui.painter().circle_filled(center, 4.5, ui_kit::ACCENT),
                            Label::Ignore => ui.painter().circle_filled(center, 4.5, weak),
                            Label::Unreviewed => ui.painter().circle_stroke(center, 4.0, Stroke::new(1.5, weak)),
                        };
                        let answer = match s.meta.label {
                            Label::Unreviewed => "Not answered",
                            Label::Trigger => "Should save a clip",
                            Label::Ignore => "Shouldn't save a clip",
                        };
                        ui.selectable_label(selected, format!("{}   {}   ·   {answer}", when(s), s.meta.kind.describe())).clicked()
                    });
                    if row.inner {
                        pick = Some(s.meta.id.clone());
                    }
                }
            });
            if let Some(id) = pick {
                r.select(&id);
                ui.scroll_to_rect(top, Some(egui::Align::TOP));
            }
        });

        self.voice_review = r;
        if let Some((text, error)) = say {
            if error {
                self.toast_error(text);
            } else {
                self.toast(text);
            }
        }
        if back {
            self.page = Page::Settings;
        }
    }

    /// The task: hear it, decide.
    fn review_card(&mut self, ui: &mut egui::Ui, r: &mut Review, s: &Sample, say: &mut Option<(String, bool)>) {
        let ctx = ui.ctx().clone();
        ui.horizontal(|ui| {
            ui.label(RichText::new(s.meta.kind.describe()).size(16.0).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.weak(when(s));
            });
        });
        if s.meta.kind != Kind::ByHand {
            ui.weak(format!("The detector was {:.0}% sure", s.meta.score * 100.0));
        }
        ui.add_space(12.0);

        let mut toggle = false;
        if let (Some(wave), Some(sound)) = (&r.wave, &r.sound) {
            if sound.is_playing() {
                ctx.request_repaint();
            }
            let before = r.trim;
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.add_space((WAVE_H - 48.0) / 2.0);
                    toggle = play_button(ui, sound.is_playing()).clicked();
                });
                ui.add_space(4.0);
                waveform(ui, wave, s.secs, &mut r.trim, s.meta.moment, sound);
            });
            if r.trim != before && s.meta.label != Label::Unreviewed {
                if let Some(i) = r.index() {
                    r.list[i].meta.trim = Some(r.trim);
                    voice_samples::save_meta(&r.list[i].meta);
                }
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.weak(format!("Highlighted {:.1} s of {:.1} s. Drag its ends to choose what to judge, or drag over the sound to scrub.", r.trim.1 - r.trim.0, s.secs));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(egui::Button::new(RichText::new("Select all").color(ui_kit::ACCENT)).frame(false)).clicked() {
                        r.trim = (0.0, s.secs);
                    }
                });
            });
            if let Some(e) = &sound.error {
                ui.colored_label(ui.visuals().warn_fg_color, format!("Can't play: {e}"));
            }
        } else {
            ui.colored_label(ui.visuals().warn_fg_color, "This sample's sound can't be read.");
        }
        if toggle {
            r.play_or_stop();
        }

        ui.add_space(20.0);
        ui.label(RichText::new("Should this save a clip?").size(18.0).strong());
        ui.add_space(10.0);
        let gap = 12.0;
        let w = (ui.available_width() - gap) / 2.0;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            let chosen = |l: Label| s.meta.label == l;
            let answer = |ui: &mut egui::Ui, text: &str, primary: bool, on: bool| {
                let mut b = egui::Button::new(RichText::new(text).size(16.0).color(if primary { Color32::WHITE } else { ui.visuals().text_color() }));
                if primary {
                    b = b.fill(ui_kit::ACCENT);
                }
                if on {
                    b = b.stroke(Stroke::new(2.0, if primary { Color32::WHITE } else { ui_kit::ACCENT }));
                }
                ui.add_sized([w, 48.0], b).clicked()
            };
            if answer(ui, "No", false, chosen(Label::Ignore)) {
                r.judge(Label::Ignore);
            }
            if answer(ui, "Yes, save a clip", true, chosen(Label::Trigger)) {
                r.judge(Label::Trigger);
            }
        });
        ui.add_space(6.0);
        ui_kit::hint(ui, "Yes means a clip should be saved here, because the phrase was said clearly enough. Anything else is a no.");

        ui.add_space(14.0);
        ui.horizontal(|ui| {
            for (key, what) in [("Y", "Yes"), ("N", "No"), ("Space", "Play"), ("\u{2190} \u{2192}", "Move"), ("Del", "Delete")] {
                keycap(ui, key);
                ui.add_space(-2.0);
                ui.label(RichText::new(what).size(12.0).color(ui.visuals().weak_text_color()));
                ui.add_space(8.0);
            }
        });
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui.add(egui::Button::new(RichText::new("Delete sample").color(ui_kit::DANGER)).frame(false)).clicked() {
                r.remove();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(ui_kit::button("Skip", false)).clicked() {
                    r.skip();
                }
            });
        });
        let _ = say;
    }
}

/// A key as it looks on a keyboard.
fn keycap(ui: &mut egui::Ui, key: &str) {
    let v = ui.visuals().clone();
    egui::Frame::new()
        .fill(ui_kit::hover(&v))
        .stroke(Stroke::new(1.0, ui_kit::line(v.dark_mode)))
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(6, 1))
        .show(ui, |ui| ui.label(RichText::new(key).size(11.0).monospace().color(v.strong_text_color())));
}

/// A round play/stop button.
fn play_button(ui: &mut egui::Ui, playing: bool) -> egui::Response {
    let (rect, r) = ui.allocate_exact_size(vec2(48.0, 48.0), Sense::click());
    let c = rect.center();
    let p = ui.painter();
    p.circle_filled(c, 24.0, if r.hovered() { ui_kit::ACCENT.gamma_multiply(0.85) } else { ui_kit::ACCENT });
    if playing {
        for dx in [-6.0, 2.0] {
            p.rect_filled(Rect::from_min_size(pos2(c.x + dx, c.y - 8.0), vec2(4.0, 16.0)), 1.0, Color32::WHITE);
        }
    } else {
        p.add(egui::Shape::convex_polygon(vec![pos2(c.x - 6.0, c.y - 10.0), pos2(c.x - 6.0, c.y + 10.0), pos2(c.x + 10.0, c.y)], Color32::WHITE, Stroke::NONE));
    }
    r.on_hover_text("Play the highlighted part (Space)")
}

/// The waveform's height.
const WAVE_H: f32 = 120.0;

/// A snippet drawn by the editor's waveform, the highlighted part bright and
/// the rest dim, with a handle on each end of the highlight to drag, a mark
/// where the detector's moment was, and the playhead. Dragging anywhere else
/// scrubs: the sound follows, at its own pitch.
fn waveform(ui: &mut egui::Ui, wave: &Waveform, secs: f32, trim: &mut (f32, f32), moment: Option<f32>, sound: &SoundPlayer) {
    let secs = secs.max(0.01);
    let (rect, body) = ui.allocate_exact_size(vec2(ui.available_width(), WAVE_H), Sense::click_and_drag());
    let v = ui.visuals().clone();
    let p = ui.painter_at(rect);
    p.rect_filled(rect, ui_kit::RADIUS, v.extreme_bg_color);
    let x_of = |t: f32| rect.left() + t / secs * rect.width();
    let t_of = |x: f32| ((x - rect.left()) / rect.width() * secs).clamp(0.0, secs);

    // The whole of it dim, the highlighted part over it in colour.
    let lane = rect.shrink2(vec2(0.0, 4.0));
    let dim = v.weak_text_color().gamma_multiply(0.6);
    wave.paint(&p, lane, (0.0, secs as f64), |_| 1.0, |_, _| dim);
    let bright = Rect::from_x_y_ranges(x_of(trim.0)..=x_of(trim.1), rect.y_range());
    p.rect_filled(bright, 0.0, ui_kit::ACCENT.gamma_multiply(0.12));
    wave.paint(&p.with_clip_rect(bright), lane, (0.0, secs as f64), |_| 1.0, |_, _| ui_kit::ACCENT);

    if let Some(m) = moment {
        let (x, top) = (x_of(m), rect.top() + 2.0);
        p.add(egui::Shape::convex_polygon(vec![pos2(x - 5.0, top), pos2(x + 5.0, top), pos2(x, top + 7.0)], v.strong_text_color(), Stroke::NONE));
    }

    // Handles (made after the body, so they win where they overlap it).
    let mut on_handle = false;
    for end in 0..2 {
        let t = if end == 0 { trim.0 } else { trim.1 };
        let grab = Rect::from_center_size(pos2(x_of(t), rect.center().y), vec2(16.0, rect.height()));
        let r = ui.interact(grab, egui::Id::new(("voice_trim", end)), Sense::drag());
        on_handle |= r.hovered() || r.dragged();
        if r.hovered() || r.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
        if r.dragged()
            && let Some(pointer) = r.interact_pointer_pos()
        {
            let t = t_of(pointer.x);
            if end == 0 {
                trim.0 = t.min(trim.1 - MIN_TRIM).max(0.0);
            } else {
                trim.1 = t.max(trim.0 + MIN_TRIM).min(secs);
            }
        }
        let x = x_of(if end == 0 { trim.0 } else { trim.1 });
        let color = if r.hovered() || r.dragged() { Color32::WHITE } else { ui_kit::ACCENT };
        p.line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())], Stroke::new(2.0, color));
        p.rect_filled(Rect::from_center_size(pos2(x, rect.center().y), vec2(8.0, 24.0)), 3.0, color);
    }

    // Scrubbing.
    let scrubbing = !on_handle && (body.dragged() || body.is_pointer_button_down_on());
    if scrubbing && let Some(pos) = body.interact_pointer_pos() {
        sound.seek(t_of(pos.x) as f64);
    }
    sound.scrub(scrubbing);
    let x = x_of(sound.time() as f32);
    p.line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())], Stroke::new(1.5, v.strong_text_color()));
}
