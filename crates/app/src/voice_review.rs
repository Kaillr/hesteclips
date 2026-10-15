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

use egui::{Color32, Key, Rect, Sense, Stroke, pos2, vec2};
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

        let total = r.list.len();
        let answered = r.list.iter().filter(|s| s.meta.label != Label::Unreviewed).count();
        self.page_head(
            ui,
            "",
            |_, ui| {
                back |= ui_kit::Btn::icon(ui_kit::Kind::Ghost, egui_phosphor::regular::CARET_LEFT).tip("Back to Settings  (Esc)").show(ui).clicked();
                let p = crate::theme::pal(ui);
                ui.label(crate::theme::text("Voice samples", 20.0, crate::theme::W::Semibold).color(p.text));
                if total > 0 {
                    ui.add_space(4.0);
                    ui.label(crate::theme::text(format!("{answered} of {total} answered"), 13.5, crate::theme::W::Regular).color(p.text3));
                }
            },
        );
        ui_kit::page(ui, "voice_review", 720.0, |ui| {
            let top = ui.allocate_space(vec2(1.0, 1.0)).1;
            if total > 0 {
                // How far along: the bar eases as answers come in.
                let p = crate::theme::pal(ui);
                let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 3.0), Sense::hover());
                ui.painter().rect_filled(bar, 2, p.control);
                let f = crate::theme::value(ui.ctx(), egui::Id::new("voice_progress"), answered as f32 / total as f32, crate::theme::NORMAL);
                ui.painter().rect_filled(Rect::from_min_max(bar.min, pos2(bar.left() + bar.width() * f, bar.bottom())), 2, p.text2);
            }
            ui.add_space(16.0);

            if total == 0 {
                ui_kit::card(ui, "voice_main", |ui| {
                    let p = crate::theme::pal(ui);
                    ui.add_space(8.0);
                    ui.label(crate::theme::text("No samples yet", 15.0, crate::theme::W::Semibold).color(p.text));
                    ui.add_space(4.0);
                    let text = if collecting {
                        "They appear while clipping, when the voice command saves a clip or nearly does."
                    } else {
                        "Turn on “Keep voice samples” under Voice in Settings, and they appear while clipping."
                    };
                    ui_kit::hint(ui, text);
                    ui.add_space(8.0);
                });
                return;
            }

            if let Some(s) = r.current().cloned() {
                ui_kit::card(ui, "voice_main", |ui| self.review_card(ui, &mut r, &s, &mut say));
            } else {
                ui_kit::card(ui, "voice_main", |ui| {
                    let p = crate::theme::pal(ui);
                    ui.add_space(8.0);
                    ui.label(crate::theme::text("All caught up", 15.0, crate::theme::W::Semibold).color(p.text));
                    ui.add_space(4.0);
                    ui_kit::hint(ui, "Every sample has an answer. Pick one below to change it, or export them to help improve the detector.");
                    ui.add_space(8.0);
                });
            }

            // Sharing.
            let ready = r.list.iter().filter(|s| s.shareable()).count();
            ui_kit::section_head(ui, "Help improve the detector", false, |_| {});
            ui_kit::card(ui, "voice_export", |ui| {
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
                    } else if ui_kit::Btn::new(ui_kit::Kind::DangerText, "Delete all").small().show(ui).clicked() {
                        r.confirm_delete_all = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let label = if ready == 0 { "Export".to_owned() } else { format!("Export {ready}") };
                        if ui_kit::Btn::normal(label).enabled(ready > 0).show(ui).clicked() {
                            let picked = rfd::FileDialog::new().set_file_name("hasteclip-voice-samples.zip").add_filter("Zip file", &["zip"]).save_file();
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
            ui.add_space(26.0);
            let mut pick = None;
            let p = crate::theme::pal(ui);
            let open_id = egui::Id::new("voice_all_open");
            let open = ui.data(|d| d.get_temp::<bool>(open_id)).unwrap_or(false);
            let head = ui.horizontal(|ui| {
                let turn = crate::theme::toggle(ui.ctx(), open_id.with("turn"), !open, crate::theme::NORMAL);
                let (r, _) = ui.allocate_exact_size(vec2(14.0, 18.0), Sense::hover());
                let a = -std::f32::consts::FRAC_PI_2 * turn;
                let rot = |v: egui::Vec2| vec2(v.x * a.cos() - v.y * a.sin(), v.x * a.sin() + v.y * a.cos());
                let c = r.center();
                ui.painter().add(egui::Shape::line(vec![c + rot(vec2(-4.0, -2.0)), c + rot(vec2(0.0, 2.0)), c + rot(vec2(4.0, -2.0))], Stroke::new(1.6, p.text3)));
                ui.label(crate::theme::text(format!("All samples ({total})"), 13.0, crate::theme::W::Semibold).color(p.text2));
            });
            if ui.interact(head.response.rect, open_id.with("click"), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                ui.data_mut(|d| d.insert_temp(open_id, !open));
            }
            let shown = crate::theme::toggle(ui.ctx(), open_id.with("shown"), open, crate::theme::NORMAL);
            if shown > 0.0 {
                ui.add_space(6.0);
                ui.scope(|ui| {
                    ui.multiply_opacity(shown);
                    egui::Frame::new().fill(p.raise).corner_radius(8).inner_margin(egui::Margin::same(5)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.spacing_mut().item_spacing.y = 1.0;
                        for s in &r.list {
                            let selected = r.selected.as_deref() == Some(&s.meta.id);
                            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 32.0), Sense::click());
                            let hv = crate::theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered(), crate::theme::FAST);
                            if selected {
                                ui.painter().rect_filled(rect, 5, p.seg_on);
                            } else if hv > 0.0 {
                                ui.painter().rect_filled(rect, 5, crate::theme::fade(p.sel, hv));
                            }
                            let center = pos2(rect.left() + 14.0, rect.center().y);
                            match s.meta.label {
                                Label::Trigger => ui.painter().circle_filled(center, 4.0, p.text),
                                Label::Ignore => ui.painter().circle_filled(center, 4.0, p.text3),
                                Label::Unreviewed => ui.painter().circle_stroke(center, 3.5, Stroke::new(1.5, p.text3)),
                            };
                            let answer = match s.meta.label {
                                Label::Unreviewed => "Not answered",
                                Label::Trigger => "Should save a clip",
                                Label::Ignore => "Shouldn't save a clip",
                            };
                            ui.painter().text(pos2(rect.left() + 28.0, rect.center().y), egui::Align2::LEFT_CENTER, format!("{}  ·  {}", when(s), s.meta.kind.describe()), crate::theme::font(13.0, crate::theme::W::Regular), p.text);
                            ui.painter().text(pos2(rect.right() - 10.0, rect.center().y), egui::Align2::RIGHT_CENTER, answer, crate::theme::font(12.5, crate::theme::W::Regular), p.text3);
                            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                pick = Some(s.meta.id.clone());
                            }
                        }
                    });
                });
            }
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
        let p = crate::theme::pal(ui);
        ui.horizontal(|ui| {
            ui.label(crate::theme::text(s.meta.kind.describe(), 15.0, crate::theme::W::Semibold).color(p.text));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(crate::theme::text(when(s), 12.5, crate::theme::W::Regular).color(p.text3));
            });
        });
        if s.meta.kind != Kind::ByHand {
            ui_kit::hint(ui, &format!("The detector was {:.0}% sure", s.meta.score * 100.0));
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
                ui.add(egui::Label::new(crate::theme::text(format!("Highlighted {:.1} s of {:.1} s. Drag its ends to choose what to judge, or drag over the sound to scrub.", r.trim.1 - r.trim.0, s.secs), 12.5, crate::theme::W::Regular).color(p.text3)).wrap());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui_kit::Btn::ghost("Select all").small().show(ui).clicked() {
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
        ui.label(crate::theme::text("Should this save a clip?", 15.0, crate::theme::W::Semibold).color(p.text));
        ui.add_space(10.0);
        let gap = 8.0;
        let w = (ui.available_width() - gap) / 2.0;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            let chosen = |l: Label| s.meta.label == l;
            // The answer already given shows as chosen (a check before it).
            let no = ui_kit::Btn::normal("No").min_width(w).id("voice_no");
            let no = if chosen(Label::Ignore) { no.with_icon(egui_phosphor::regular::CHECK) } else { no };
            if no.show(ui).clicked() {
                r.judge(Label::Ignore);
            }
            let yes = ui_kit::Btn::primary("Yes, save a clip").min_width(w).id("voice_yes");
            let yes = if chosen(Label::Trigger) { yes.with_icon(egui_phosphor::regular::CHECK) } else { yes };
            if yes.show(ui).clicked() {
                r.judge(Label::Trigger);
            }
        });
        ui.add_space(6.0);
        ui_kit::hint(ui, "Yes means a clip should be saved here, because the phrase was said clearly enough. Anything else is a no.");

        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            for (key, what) in [("Y", "Yes"), ("N", "No"), ("Space", "Play"), ("\u{2190} \u{2192}", "Move"), ("Del", "Delete")] {
                let w = ui_kit::key_hint_width(ui, &[key.to_owned()], 11.0);
                let (r, _) = ui.allocate_exact_size(vec2(w, 20.0), Sense::hover());
                ui_kit::key_hint(ui.painter(), r.left_center(), &[key.to_owned()], 11.0, p.text2);
                ui.label(crate::theme::text(what, 12.0, crate::theme::W::Regular).color(p.text3));
                ui.add_space(8.0);
            }
        });
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui_kit::Btn::new(ui_kit::Kind::DangerText, "Delete sample").with_icon(egui_phosphor::regular::TRASH).small().show(ui).clicked() {
                r.remove();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui_kit::Btn::ghost("Skip").show(ui).clicked() {
                    r.skip();
                }
            });
        });
        let _ = say;
    }
}

/// A round play/stop button.
fn play_button(ui: &mut egui::Ui, playing: bool) -> egui::Response {
    let (rect, r) = ui.allocate_exact_size(vec2(48.0, 48.0), Sense::click());
    let c = rect.center();
    let pal = crate::theme::pal(ui);
    let hv = crate::theme::toggle(ui.ctx(), r.id.with("hv"), r.hovered(), crate::theme::FAST);
    let p = ui.painter();
    p.circle_filled(c, 22.0, crate::theme::mix(pal.primary, if pal.dark { Color32::WHITE } else { Color32::from_gray(60) }, hv * 0.3));
    let glyph = if playing { egui_phosphor::fill::PAUSE } else { egui_phosphor::fill::PLAY };
    p.text(c + vec2(if playing { 0.0 } else { 1.5 }, 0.0), egui::Align2::CENTER_CENTER, glyph, egui::FontId::new(18.0, crate::theme::fill_family()), pal.primary_fg);
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
    let pal = crate::theme::pal(ui);
    let p = ui.painter_at(rect);
    p.rect_filled(rect, ui_kit::RADIUS, pal.stage);
    let x_of = |t: f32| rect.left() + t / secs * rect.width();
    let t_of = |x: f32| ((x - rect.left()) / rect.width() * secs).clamp(0.0, secs);

    // The whole of it dim, the highlighted part over it in colour.
    let lane = rect.shrink2(vec2(0.0, 4.0));
    let dim = Color32::from_rgba_unmultiplied(170, 182, 214, 70);
    wave.paint(&p, lane, (0.0, secs as f64), |_| 1.0, |_, _| dim);
    let bright = Rect::from_x_y_ranges(x_of(trim.0)..=x_of(trim.1), rect.y_range());
    p.rect_filled(bright, 0.0, Color32::from_white_alpha(10));
    let hi = crate::theme::DARK.wave_hi;
    wave.paint(&p.with_clip_rect(bright), lane, (0.0, secs as f64), |_| 1.0, |_, _| hi);

    if let Some(m) = moment {
        let (x, top) = (x_of(m), rect.top() + 2.0);
        p.add(egui::Shape::convex_polygon(vec![pos2(x - 5.0, top), pos2(x + 5.0, top), pos2(x, top + 7.0)], Color32::WHITE, Stroke::NONE));
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
        let color = if r.hovered() || r.dragged() { Color32::WHITE } else { Color32::from_gray(210) };
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
    p.line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())], Stroke::new(1.5, Color32::WHITE));
    let _ = (v, pal);
}
