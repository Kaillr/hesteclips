//! The page where the snippets the voice detector kept
//! ([`crate::voice_samples`]) are listened to, trimmed to what matters and
//! judged: was "hashtag HesteClip that" said in the highlighted part, or not?
//! Nothing is assumed from what made the detector keep one: it may have fired
//! on a laugh, and a near miss may be the phrase said in a way it didn't take.
//!
//! Judged snippets that aren't private can be exported as one zip file, to
//! send to whoever trains the detector.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use egui::{Color32, Key, Rect, RichText, Sense, Stroke, pos2, vec2};

use crate::ui_kit;
use crate::voice_samples::{self, Kind, Label, RATE, Sample};
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
    audio: Option<(String, Arc<Vec<f32>>)>,
    /// The selected snippet's highlighted part.
    trim: (f32, f32),
    play: Option<Playback>,
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

    /// Read what's kept, and pick where to start: the newest not yet judged.
    fn open(&mut self) {
        *self = Self::default();
        self.list = voice_samples::list();
        self.loaded = Some(Instant::now());
        let first = self.list.iter().find(|s| s.meta.label == Label::Unreviewed).or(self.list.first()).map(|s| s.meta.id.clone());
        if let Some(id) = first {
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

    fn select(&mut self, id: &str) {
        self.play = None;
        self.selected = Some(id.to_owned());
        self.audio = voice_samples::load_audio(id).map(|a| (id.to_owned(), Arc::new(a)));
        if let Some(s) = self.current() {
            self.trim = match (s.meta.trim, s.meta.moment) {
                (Some(t), _) => t,
                (None, Some(m)) => ((m - BEFORE_MOMENT).max(0.0), (m + AFTER_MOMENT).min(s.secs)),
                (None, None) => (0.0, s.secs),
            };
        }
    }

    fn select_index(&mut self, i: usize) {
        if let Some(id) = self.list.get(i).map(|s| s.meta.id.clone()) {
            self.select(&id);
        }
    }

    /// Judge the selected snippet, then on to the next one not yet judged.
    fn judge(&mut self, label: Label) {
        let Some(i) = self.index() else { return };
        let trim = self.trim;
        let meta = &mut self.list[i].meta;
        meta.label = label;
        meta.trim = Some(trim);
        voice_samples::save_meta(meta);
        let next = (i + 1..self.list.len()).chain(0..i).find(|&j| self.list[j].meta.label == Label::Unreviewed);
        if let Some(j) = next {
            self.select_index(j);
        }
    }

    fn remove(&mut self) {
        let Some(i) = self.index() else { return };
        voice_samples::delete(&self.list[i].meta.id);
        self.list.remove(i);
        self.play = None;
        self.audio = None;
        self.selected = None;
        if !self.list.is_empty() {
            self.select_index(i.min(self.list.len() - 1));
        }
    }

    fn play_or_stop(&mut self) -> Result<(), String> {
        if self.play.take().is_some() {
            return Ok(());
        }
        let Some((_, audio)) = &self.audio else { return Ok(()) };
        self.play = Some(Playback::start(audio, self.trim.0, self.trim.1)?);
        Ok(())
    }
}

/// A snippet playing on the default output.
struct Playback {
    stop: Arc<AtomicBool>,
    started: Instant,
    from: f32,
    secs: f32,
}

impl Playback {
    fn start(audio: &[f32], from: f32, to: f32) -> Result<Self, String> {
        let (a, b) = ((from * RATE as f32) as usize, ((to * RATE as f32) as usize).min(audio.len()));
        if b <= a {
            return Err("Nothing is selected to play".into());
        }
        let peak = audio[a..b].iter().fold(0.0f32, |m, s| m.max(s.abs()));
        // Quiet mics are made audible, loud laughs not louder than a normal voice.
        let gain = if peak > 0.0 { (0.5 / peak).min(12.0) } else { 1.0 };
        let data: Vec<f32> = audio[a..b].iter().map(|s| s * gain).collect();
        let secs = data.len() as f32 / RATE as f32;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        std::thread::spawn(move || {
            if let Err(e) = output(data, &flag, secs) {
                eprintln!("voice samples: {e}");
            }
        });
        Ok(Self { stop, started: Instant::now(), from, secs })
    }

    /// Where the sound is now (seconds into the snippet), while it plays.
    fn at(&self) -> Option<f32> {
        let t = self.started.elapsed().as_secs_f32();
        (t < self.secs).then_some(self.from + t)
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn output(data: Vec<f32>, stop: &AtomicBool, secs: f32) -> Result<(), String> {
    let device = cpal::default_host().default_output_device().ok_or("no audio output device")?;
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    let channels = supported.channels() as usize;
    let step = RATE as f64 / supported.sample_rate() as f64;
    let config = cpal::StreamConfig { channels: supported.channels(), sample_rate: supported.sample_rate(), buffer_size: cpal::BufferSize::Default };
    let mut pos = 0.0f64;
    let stream = device
        .build_output_stream::<f32, _, _>(
            config,
            move |out: &mut [f32], _| {
                for frame in out.chunks_mut(channels) {
                    let i = pos as usize;
                    let s = match (data.get(i), data.get(i + 1)) {
                        (Some(a), Some(b)) => a + (b - a) * (pos - i as f64) as f32,
                        (Some(a), None) => *a,
                        _ => 0.0,
                    };
                    frame.fill(s);
                    pos += step;
                }
            },
            |e| eprintln!("voice samples output: {e}"),
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    let end = Instant::now() + Duration::from_secs_f32(secs + 0.2);
    while Instant::now() < end && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
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
        let (yes, no, play, private, delete) = (press(Key::Y), press(Key::N), press(Key::Space), press(Key::P), press(Key::Delete) || press(Key::Backspace));
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
        if play && let Err(e) = r.play_or_stop() {
            say = Some((e, true));
        }
        if yes {
            r.judge(Label::Phrase);
        }
        if no {
            r.judge(Label::Other);
        }
        if private && let Some(i) = r.index() {
            r.list[i].meta.private ^= true;
            voice_samples::save_meta(&r.list[i].meta);
        }
        if delete {
            r.remove();
        }

        ui_kit::page(ui, "voice_review", 760.0, |ui| {
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.add(ui_kit::button("Back to settings", false)).clicked() {
                    back = true;
                }
                ui.add_space(8.0);
                ui.label(RichText::new("Voice samples").size(20.0).strong());
            });
            ui.add_space(6.0);
            ui_kit::hint(
                ui,
                "Short sounds from your microphone, kept when \"hashtag HesteClip that\" saved a clip or nearly did, \
                 and around clips you saved by hand. Listen, trim each to the part that matters, and say whether you said the phrase. \
                 They stay on this computer, and the ones nobody has listened to are deleted after two weeks.",
            );
            if !collecting {
                ui.add_space(4.0);
                ui_kit::hint(ui, "Keeping new samples is off. Turn it on under Voice in Settings.");
            }

            if r.list.is_empty() {
                ui.add_space(16.0);
                ui_kit::card(ui, |ui| ui.weak("No samples yet. They appear as you use the voice command while the replay buffer runs."));
                return;
            }

            let to_hear = r.list.iter().filter(|s| s.meta.label == Label::Unreviewed).count();
            let said = r.list.iter().filter(|s| s.meta.label == Label::Phrase).count();
            let other = r.list.iter().filter(|s| s.meta.label == Label::Other).count();
            ui.add_space(14.0);
            ui.weak(format!("{to_hear} to listen to · {said} said the phrase · {other} not the phrase"));
            ui.add_space(8.0);

            // The one being judged.
            if let Some(s) = r.current().cloned() {
                ui_kit::card(ui, |ui| {
                    let when = chrono::DateTime::from_timestamp(s.meta.at, 0).map(|t| t.with_timezone(&chrono::Local).format("%d %b %H:%M").to_string()).unwrap_or_default();
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(s.meta.kind.describe()).strong());
                        ui.weak(format!("· {when}"));
                        if s.meta.kind != Kind::ByHand {
                            ui.weak(format!("· the detector was {:.0}% sure", s.meta.score * 100.0));
                        }
                    });
                    ui.add_space(8.0);
                    match r.audio.clone() {
                        Some((_, audio)) => {
                            let playhead = r.play.as_ref().and_then(Playback::at);
                            if r.play.is_some() && playhead.is_none() {
                                r.play = None;
                            }
                            if playhead.is_some() {
                                ctx.request_repaint();
                            }
                            let before = r.trim;
                            waveform(ui, &audio, &mut r.trim, s.meta.moment, playhead);
                            if r.trim != before && s.meta.label != Label::Unreviewed {
                                if let Some(i) = r.index() {
                                    r.list[i].meta.trim = Some(r.trim);
                                    voice_samples::save_meta(&r.list[i].meta);
                                }
                            }
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                let label = if r.play.is_some() { "Stop" } else { "Play highlighted part" };
                                if ui.add(ui_kit::button(label, false)).clicked()
                                    && let Err(e) = r.play_or_stop()
                                {
                                    say = Some((e, true));
                                }
                                ui.weak(format!("{:.1} s of {:.1} s", r.trim.1 - r.trim.0, s.secs));
                                if ui.small_button("Highlight all").clicked() {
                                    r.trim = (0.0, s.secs);
                                }
                            });
                            ui.add_space(2.0);
                            ui_kit::hint(ui, "Drag the ends of the highlight so it holds just the phrase, or just the sound that fooled it. Only the highlighted part is ever shared.");
                        }
                        None => {
                            ui.colored_label(ui.visuals().warn_fg_color, "This sample's sound can't be read.");
                        }
                    }

                    ui.add_space(12.0);
                    ui.label("Do you say \u{201c}hashtag HesteClip that\u{201d} in the highlighted part?");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.add(ui_kit::button("Said the phrase", true)).on_hover_text("Y").clicked() {
                                r.judge(Label::Phrase);
                            }
                            if ui.add(ui_kit::button("Not the phrase", false)).on_hover_text("N").clicked() {
                                r.judge(Label::Other);
                            }
                            if ui.add(ui_kit::button("Delete", false)).on_hover_text("Delete").clicked() {
                                r.remove();
                            }
                            let mut private = s.meta.private;
                            if ui.checkbox(&mut private, "Keep private").on_hover_text("Never exported, whatever you say about it (P)").changed()
                                && let Some(i) = r.index()
                            {
                                r.list[i].meta.private = private;
                                voice_samples::save_meta(&r.list[i].meta);
                            }
                            let verdict = match s.meta.label {
                                Label::Unreviewed => "",
                                Label::Phrase => "Marked: said the phrase",
                                Label::Other => "Marked: not the phrase",
                            };
                            ui.weak(verdict);
                        });
                    });
                    ui.add_space(4.0);
                    ui_kit::hint(ui, "Space plays · Y said the phrase · N not the phrase · P keep private · arrows move between samples");
                });
            }

            // Sharing.
            ui_kit::section(ui, "Share", |ui| {
                let ready = r.list.iter().filter(|s| s.shareable()).count();
                let private = r.list.iter().filter(|s| s.meta.private).count();
                ui_kit::hint(
                    ui,
                    "To help improve the detector, export what you've judged to one zip file and send it yourself; nothing is uploaded. \
                     It holds the highlighted part of each sample and your verdict, with no name and nothing else. \
                     Samples marked private and ones you haven't judged are left out.",
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.weak(format!("{ready} ready to share · {private} private"));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add_enabled(ready > 0, ui_kit::button("Export…", true)).clicked() {
                            let picked = rfd::FileDialog::new().set_file_name("hesteclips-voice-samples.zip").add_filter("Zip file", &["zip"]).save_file();
                            if let Some(path) = picked {
                                say = Some(match voice_samples::export(&path, &r.list) {
                                    Ok(n) => (format!("Saved {n} samples to {}", path.display()), false),
                                    Err(e) => (e, true),
                                });
                            }
                        }
                        if r.confirm_delete_all {
                            if ui.add(ui_kit::danger_button(format!("Delete all {}", r.list.len()))).clicked() {
                                voice_samples::delete_all();
                                r.list.clear();
                                r.selected = None;
                                r.audio = None;
                                r.play = None;
                                r.confirm_delete_all = false;
                            }
                            if ui.add(ui_kit::button("Cancel", false)).clicked() {
                                r.confirm_delete_all = false;
                            }
                        } else if ui.add(ui_kit::button("Delete all…", false)).clicked() {
                            r.confirm_delete_all = true;
                        }
                    });
                });
            });

            // All of them.
            ui_kit::section(ui, "All samples", |ui| {
                let mut pick = None;
                for s in &r.list {
                    let when = chrono::DateTime::from_timestamp(s.meta.at, 0).map(|t| t.with_timezone(&chrono::Local).format("%d %b %H:%M").to_string()).unwrap_or_default();
                    let verdict = match s.meta.label {
                        Label::Unreviewed => "not listened to",
                        Label::Phrase => "said the phrase",
                        Label::Other => "not the phrase",
                    };
                    let private = if s.meta.private { " · private" } else { "" };
                    let text = format!("{when}   {}   ·   {verdict}{private}", s.meta.kind.describe());
                    if ui.selectable_label(r.selected.as_deref() == Some(&s.meta.id), text).clicked() {
                        pick = Some(s.meta.id.clone());
                    }
                }
                if let Some(id) = pick {
                    r.select(&id);
                }
            });
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
}

/// A snippet drawn as its loudness over time (a dB scale, so quiet speech
/// shows), the highlighted part bright, with a handle on each end to drag; a
/// mark where the detector's moment was, and the playhead.
fn waveform(ui: &mut egui::Ui, audio: &[f32], trim: &mut (f32, f32), moment: Option<f32>, playhead: Option<f32>) {
    let secs = (audio.len() as f32 / RATE as f32).max(0.01);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 110.0), Sense::hover());
    let p = ui.painter_at(rect);
    let v = ui.visuals().clone();
    p.rect_filled(rect, ui_kit::RADIUS, v.extreme_bg_color);
    let x_of = |t: f32| rect.left() + t / secs * rect.width();
    let t_of = |x: f32| ((x - rect.left()) / rect.width() * secs).clamp(0.0, secs);
    let columns = rect.width() as usize;
    let (mid, half) = (rect.center().y, rect.height() / 2.0 - 8.0);
    let dim = v.weak_text_color().gamma_multiply(0.45);
    for i in 0..columns {
        let a = i * audio.len() / columns;
        let b = ((i + 1) * audio.len() / columns).clamp(a + 1, audio.len().max(a + 1));
        let peak = audio.get(a..b.min(audio.len())).map_or(0.0, |s| s.iter().fold(0.0f32, |m, x| m.max(x.abs())));
        let h = (((20.0 * peak.max(1e-6).log10()) + 54.0) / 54.0).clamp(0.0, 1.0) * half;
        let x = rect.left() + i as f32 + 0.5;
        let t = (i as f32 + 0.5) / columns as f32 * secs;
        let color = if t >= trim.0 && t <= trim.1 { ui_kit::ACCENT } else { dim };
        p.line_segment([pos2(x, mid - h.max(0.5)), pos2(x, mid + h.max(0.5))], Stroke::new(1.0, color));
    }
    if let Some(m) = moment {
        let x = x_of(m);
        let top = rect.top() + 2.0;
        p.add(egui::Shape::convex_polygon(vec![pos2(x - 5.0, top), pos2(x + 5.0, top), pos2(x, top + 7.0)], v.strong_text_color(), Stroke::NONE));
    }
    if let Some(t) = playhead {
        let x = x_of(t);
        p.line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())], Stroke::new(1.5, v.strong_text_color()));
    }
    // Handles.
    for (end, id) in [(0, "voice_trim_start"), (1, "voice_trim_end")] {
        let t = if end == 0 { trim.0 } else { trim.1 };
        let x = x_of(t);
        let grab = Rect::from_center_size(pos2(x, rect.center().y), vec2(16.0, rect.height()));
        let r = ui.interact(grab, egui::Id::new(id), Sense::drag());
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
        let active = r.hovered() || r.dragged();
        let x = x_of(if end == 0 { trim.0 } else { trim.1 });
        let color = if active { Color32::WHITE } else { ui_kit::ACCENT };
        p.line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())], Stroke::new(2.0, color));
        let tab = Rect::from_center_size(pos2(x, rect.center().y), vec2(8.0, 24.0));
        p.rect_filled(tab, 3.0, color);
    }
}
