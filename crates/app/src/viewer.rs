//! The clip viewer: clicking a clip plays it here instead of in another app.
//!
//! Built for finding a moment fast. The timeline under the picture is the
//! filmstrip over the clip's audio levels, so a shot, a shout or a quiet patch
//! is visible before you get there. Hovering shows that moment's picture; click
//! or drag to go there; scroll anywhere over the picture or the timeline to
//! scrub. Scrubbing shows every frame straight from the in-memory proxy (see
//! `proxy.rs`), the same as in the editor, and sharpens when you stop.
//!
//! It plays the clip as it's shared: the saved edit if there is one, and the
//! mix (track 1).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use egui::{Align2, Color32, FontId, Key, Pos2, Rect, RichText, Sense, Stroke, Vec2};
use media::{ClipInfo, TrackEdit};

use crate::filmstrip::Filmstrip;
use crate::library::ACCENT;
use crate::meter;
use crate::player::Player;

/// Audio level resolution: one peak/RMS pair per this many seconds.
const LEVEL_STEP: f64 = 0.02;
/// The level lane's scale floor, dBFS.
const LEVEL_FLOOR: f32 = -60.0;
/// Scrolling keeps showing quick proxy frames until it's been still this long.
const SCROLL_SETTLE: Duration = Duration::from_millis(200);
/// Arrow keys jump this far.
const JUMP: f64 = 5.0;

pub enum ViewerOutcome {
    Stay,
    Close,
    Edit,
}

struct Loaded {
    info: ClipInfo,
    pcm: Vec<f32>,
    /// Peak and RMS per [`LEVEL_STEP`] of the mix.
    levels: Vec<(f32, f32)>,
}

enum State {
    Loading(Receiver<Result<Loaded, String>>),
    Failed(String),
    Ready(Box<Ready>),
}

pub struct Viewer {
    clip: PathBuf,
    state: State,
}

struct Ready {
    info: ClipInfo,
    player: Player,
    strip: Filmstrip,
    levels: Vec<(f32, f32)>,
    /// Dragging on the timeline, and whether it was playing when the drag began.
    dragging: Option<bool>,
    /// Last scroll-scrub, and whether it was playing before scrolling began.
    scrolled: Option<(Instant, bool)>,
    /// Hover preview: the frame shown and its texture.
    hover: Option<(u64, egui::TextureHandle)>,
    volume: f32,
    muted: bool,
}

impl Viewer {
    pub fn open(ctx: &egui::Context, clip: &Path) -> Self {
        let (tx, rx) = mpsc::channel();
        let src = clip.to_path_buf();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            let result = (|| -> Result<Loaded, String> {
                let info = media::probe(&src).map_err(|e| e.to_string())?;
                // Track 1 is the mix in our clips; in any other file it's simply the first.
                let pcm = match info.audio.first() {
                    Some(a) => media::decode_audio(&src, a.index).map_err(|e| e.to_string())?,
                    None => Vec::new(),
                };
                let levels = levels(&pcm);
                Ok(Loaded { info, pcm, levels })
            })();
            let _ = tx.send(result);
            repaint.request_repaint();
        });
        Self { clip: clip.to_path_buf(), state: State::Loading(rx) }
    }

    pub fn clip(&self) -> &Path {
        &self.clip
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) -> ViewerOutcome {
        if let State::Loading(rx) = &self.state {
            if let Ok(result) = rx.try_recv() {
                self.state = match result {
                    Ok(l) => State::Ready(Box::new(Ready::new(ui.ctx(), &self.clip, l))),
                    Err(e) => State::Failed(e),
                };
            }
        }
        let mut out = self.header(ui);
        match &mut self.state {
            State::Loading(_) => {
                let rect = ui.available_rect_before_wrap();
                ui.put(Rect::from_center_size(rect.center(), Vec2::splat(24.0)), egui::Spinner::new().size(24.0));
            }
            State::Failed(e) => {
                ui.vertical_centered(|ui| {
                    ui.add_space(80.0);
                    ui.colored_label(ui.visuals().error_fg_color, format!("Couldn't play this clip: {e}"));
                });
            }
            State::Ready(r) => r.ui(ui),
        }
        if ui.ctx().input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape)) {
            out = ViewerOutcome::Close;
        }
        if matches!(out, ViewerOutcome::Close | ViewerOutcome::Edit) {
            if let State::Ready(r) = &mut self.state {
                r.player.pause();
            }
        }
        out
    }

    fn header(&mut self, ui: &mut egui::Ui) -> ViewerOutcome {
        let mut out = ViewerOutcome::Stay;
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button(RichText::new("‹ Clips").size(15.0)).on_hover_text("Back to your clips  (Esc)").clicked() {
                out = ViewerOutcome::Close;
            }
            ui.add_space(6.0);
            let stem = self.clip.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            ui.label(RichText::new(crate::clips::title_for_stem(&stem)).size(18.0).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let edit = egui::Button::new(RichText::new("✂ Edit").size(14.0).color(Color32::WHITE))
                    .fill(ACCENT)
                    .min_size(Vec2::new(76.0, 30.0))
                    .corner_radius(8);
                if ui.add(edit).on_hover_text("Trim it and adjust its audio").clicked() {
                    out = ViewerOutcome::Edit;
                }
                let other = egui::Button::new(RichText::new("↗").size(15.0)).min_size(Vec2::new(30.0, 30.0)).corner_radius(8);
                if ui.add(other).on_hover_text("Open in your default video player").clicked() {
                    if let State::Ready(r) = &mut self.state {
                        r.player.pause();
                    }
                    let _ = crate::clips::open_in_default_app(&self.clip);
                }
            });
        });
        ui.add_space(6.0);
        out
    }
}

impl Ready {
    fn new(ctx: &egui::Context, clip: &Path, l: Loaded) -> Self {
        let volume = ctx.data(|d| d.get_temp::<(f32, bool)>(volume_id())).unwrap_or((1.0, false));
        let tracks = if l.pcm.is_empty() { Vec::new() } else { vec![l.pcm] };
        let mut player = Player::new(ctx, clip, l.info.clone(), tracks, Vec::new());
        // Opened to watch: start right away.
        player.play(l.info.duration);
        let r = Self {
            strip: Filmstrip::build(ctx, clip),
            info: l.info,
            player,
            levels: l.levels,
            dragging: None,
            scrolled: None,
            hover: None,
            volume: volume.0,
            muted: volume.1,
        };
        r.apply_volume(ctx);
        r
    }

    fn apply_volume(&self, ctx: &egui::Context) {
        let gain = if self.muted { 0.0 } else { self.volume * self.volume }; // a squared slider feels even
        self.player.set_mix(vec![TrackEdit { index: 0, gain, muted: false, points: Vec::new() }]);
        ctx.data_mut(|d| d.insert_temp(volume_id(), (self.volume, self.muted)));
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.keyboard(&ctx);

        // A scroll-scrub has settled: sharpen, and carry on playing if it was.
        if let Some((at, was_playing)) = self.scrolled {
            if at.elapsed() >= SCROLL_SETTLE {
                self.scrolled = None;
                if was_playing {
                    self.play();
                }
            } else {
                ctx.request_repaint_after(SCROLL_SETTLE);
            }
        }
        let scrubbing = self.dragging.is_some() || self.scrolled.is_some();

        // --- Picture: everything above the timeline and controls ---
        let timeline_h = 52.0 + 2.0 + 40.0;
        let controls_h = 8.0 + timeline_h + 8.0 + 34.0 + 6.0;
        let preview_h = (ui.available_height() - controls_h).max(160.0);
        let (preview, preview_resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), preview_h), Sense::click());
        ui.painter().rect_filled(preview, 8, Color32::BLACK);
        if let Some(tex) = self.player.update(&ctx, scrubbing) {
            let size = tex.size_vec2();
            let scale = (preview.width() / size.x).min(preview.height() / size.y);
            let r = Rect::from_center_size(preview.center(), size * scale);
            egui::Image::from_texture((tex.id(), r.size())).paint_at(ui, r);
        }
        if preview_resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            self.toggle_play();
        }

        // --- Timeline ---
        ui.add_space(8.0);
        let (outer, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), timeline_h), Sense::click_and_drag());
        self.timeline(ui, outer, &resp, preview);

        // Scroll over the picture or the timeline to scrub through the clip.
        let pointer = ctx.pointer_hover_pos();
        if pointer.is_some_and(|p| preview.contains(p) || outer.contains(p)) {
            let d = ctx.input(|i| i.smooth_scroll_delta);
            // Down or left (towards you, or swiping left) goes forward, like reading on.
            let px = -(d.x + d.y);
            if px.abs() > 0.0 {
                let was = match self.scrolled {
                    Some((_, was)) => was,
                    None => self.player.is_playing(),
                };
                self.player.pause();
                let dt = px as f64 / outer.width().max(1.0) as f64 * self.info.duration;
                self.seek(self.player.time() + dt);
                self.scrolled = Some((Instant::now(), was));
            }
        }

        // --- Transport ---
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.set_min_height(34.0);
            let label = if self.player.is_playing() { "⏸" } else { "▶" };
            let play = egui::Button::new(RichText::new(label).size(18.0)).min_size(Vec2::new(44.0, 32.0)).corner_radius(8);
            if ui.add(play).on_hover_text("Play / pause  (Space)").clicked() {
                self.toggle_play();
            }
            ui.add_space(8.0);
            ui.label(RichText::new(clock(self.player.time())).monospace().size(15.0));
            ui.weak(format!("/ {}", clock(self.info.duration)));

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut changed = false;
                let slider = egui::Slider::new(&mut self.volume, 0.0..=1.0).show_value(false);
                changed |= ui.add_sized(Vec2::new(100.0, 20.0), slider).on_hover_text("Volume").changed();
                let icon = if self.muted || self.volume == 0.0 { "🔇" } else { "🔊" };
                if ui.add(egui::Button::new(icon).frame(false)).on_hover_text(if self.muted { "Unmute  (M)" } else { "Mute  (M)" }).clicked() {
                    self.muted = !self.muted;
                    changed = true;
                }
                if changed {
                    if self.volume > 0.0 && ui.input(|i| i.pointer.any_down()) {
                        self.muted = false;
                    }
                    self.apply_volume(ui.ctx());
                }
                ui.add_space(12.0);
                ui.weak("Scroll to scrub · arrow keys jump 5 s · , and . step one frame");
            });
        });
        if let Some(e) = &self.player.audio_error {
            ui.colored_label(ui.visuals().warn_fg_color, format!("No sound: {e}"));
        }
    }

    /// Filmstrip over the audio levels, with the playhead through both. Hover to
    /// see a moment; click or drag to go there.
    fn timeline(&mut self, ui: &mut egui::Ui, outer: Rect, resp: &egui::Response, preview: Rect) {
        let v = ui.visuals().clone();
        let p = ui.painter_at(outer.expand(8.0));
        let dur = self.info.duration.max(1e-6);
        let video = Rect::from_min_size(outer.min, Vec2::new(outer.width(), 52.0));
        let audio = Rect::from_min_max(Pos2::new(outer.left(), video.bottom() + 2.0), outer.max);
        let x_of = |t: f64| outer.left() + (t / dur) as f32 * outer.width();
        let t_of = |x: f32| ((x - outer.left()) / outer.width()).clamp(0.0, 1.0) as f64 * dur;

        p.rect_filled(video, 4, v.extreme_bg_color);
        self.strip.paint(ui, video, dur);
        p.rect_filled(audio, 4, v.extreme_bg_color);
        draw_levels(&p, audio, &self.levels, dur);

        // Already-played part of the levels, lightly marked, so where you are reads at a glance.
        let xp = x_of(self.player.time());
        p.rect_filled(Rect::from_min_max(audio.min, Pos2::new(xp, audio.bottom())), 4, Color32::from_white_alpha(10));

        // Scrub proxy still building: a thin bar until every frame is scrubbable.
        let prog = self.player.proxy_progress();
        if prog < 1.0 {
            p.rect_filled(Rect::from_min_size(Pos2::new(video.left(), video.bottom() - 3.0), Vec2::new(video.width() * prog, 3.0)), 0, ACCENT);
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }

        // Hover: a line, and that moment's picture and time above the timeline.
        if let Some(pos) = resp.hover_pos().filter(|_| self.dragging.is_none()) {
            let t = self.info.snap(t_of(pos.x));
            p.vline(pos.x, outer.y_range(), Stroke::new(1.0, Color32::from_white_alpha(120)));
            self.hover_preview(ui, t, pos.x, outer, preview);
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        } else {
            self.hover = None;
        }

        // Playhead.
        p.vline(xp, outer.top() - 4.0..=outer.bottom(), Stroke::new(2.0, Color32::WHITE));
        p.add(egui::Shape::convex_polygon(
            vec![Pos2::new(xp - 6.0, outer.top() - 6.0), Pos2::new(xp + 6.0, outer.top() - 6.0), Pos2::new(xp, outer.top() + 2.0)],
            Color32::WHITE,
            Stroke::NONE,
        ));

        // Click or drag to go there. Playing pauses while you drag and carries on after.
        if resp.drag_started() {
            self.dragging = Some(self.player.is_playing());
            self.player.pause();
        }
        if let Some(pos) = resp.interact_pointer_pos() {
            if resp.dragged() || resp.drag_started() || resp.clicked() {
                self.seek(t_of(pos.x));
            }
        }
        if resp.drag_stopped() {
            if self.dragging.take() == Some(true) {
                self.play();
            }
        }
    }

    /// The picture at `t` in a small frame over the bottom of the preview, above
    /// the pointer, with its time.
    fn hover_preview(&mut self, ui: &egui::Ui, t: f64, x: f32, timeline: Rect, preview: Rect) {
        let idx = self.info.frame_index(t);
        if self.hover.as_ref().is_none_or(|(i, _)| *i != idx) {
            // The exact frame from the scrub proxy, else the nearest filmstrip picture.
            if let Some(img) = self.player.proxy_frame(idx) {
                match &mut self.hover {
                    Some((i, tex)) => {
                        tex.set(img, egui::TextureOptions::LINEAR);
                        *i = idx;
                    }
                    None => self.hover = Some((idx, ui.ctx().load_texture("hover_preview", img, egui::TextureOptions::LINEAR))),
                }
            }
        }
        let tex = match &self.hover {
            Some((i, tex)) if *i == idx => Some(tex.clone()),
            _ => self.strip.near(t).cloned(),
        };
        let aspect = self.info.width.max(1) as f32 / self.info.height.max(1) as f32;
        let w = 240.0f32.min(preview.width() * 0.4);
        let size = Vec2::new(w, w / aspect);
        let left = (x - size.x / 2.0).clamp(timeline.left(), timeline.right() - size.x);
        let frame = Rect::from_min_size(Pos2::new(left, timeline.top() - 14.0 - size.y - 20.0), size);
        let p = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("viewer_hover")));
        p.rect_filled(frame.expand(2.0), 6, Color32::from_black_alpha(230));
        if let Some(tex) = tex {
            p.image(tex.id(), frame, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
        }
        let label = Rect::from_min_size(Pos2::new(frame.left() - 2.0, frame.bottom() + 2.0), Vec2::new(frame.width() + 4.0, 20.0));
        p.rect_filled(label, 6, Color32::from_black_alpha(230));
        p.text(label.center(), Align2::CENTER_CENTER, clock(t), FontId::monospace(12.0), Color32::WHITE);
    }

    fn keyboard(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let none = egui::Modifiers::NONE;
        let (space, k, left, right, j, l, comma, period, home, end, m) = ctx.input_mut(|i| {
            (
                i.consume_key(none, Key::Space),
                i.consume_key(none, Key::K),
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::ArrowRight),
                i.consume_key(none, Key::J),
                i.consume_key(none, Key::L),
                i.consume_key(none, Key::Comma),
                i.consume_key(none, Key::Period),
                i.consume_key(none, Key::Home),
                i.consume_key(none, Key::End),
                i.consume_key(none, Key::M),
            )
        });
        if space || k {
            self.toggle_play();
        }
        let now = self.player.time();
        if left {
            self.seek(now - JUMP);
        }
        if right {
            self.seek(now + JUMP);
        }
        if j {
            self.seek(now - 2.0 * JUMP);
        }
        if l {
            self.seek(now + 2.0 * JUMP);
        }
        if comma || period {
            self.player.pause();
            let fd = self.info.frame_duration();
            self.seek(self.info.snap(now) + if period { fd } else { -fd });
        }
        if home {
            self.seek(0.0);
        }
        if end {
            self.player.pause();
            self.seek(self.info.duration - self.info.frame_duration());
        }
        if m {
            self.muted = !self.muted;
            self.apply_volume(ctx);
        }
    }

    /// Go to `t`, on a frame. Keeps playing if it was.
    fn seek(&mut self, t: f64) {
        let last = (self.info.duration - self.info.frame_duration()).max(0.0);
        self.player.seek(self.info.snap(t.clamp(0.0, last)) + 1e-6);
    }

    fn play(&mut self) {
        self.player.play(self.info.duration);
    }

    fn toggle_play(&mut self) {
        if self.player.is_playing() {
            self.player.pause();
            return;
        }
        // At the end: start over.
        if self.player.time() >= self.info.duration - self.info.frame_duration() * 1.5 {
            self.player.seek(0.0);
        }
        self.play();
    }
}

fn volume_id() -> egui::Id {
    egui::Id::new("viewer_volume")
}

/// Peak and RMS of every [`LEVEL_STEP`] of interleaved stereo at the preview rate.
fn levels(pcm: &[f32]) -> Vec<(f32, f32)> {
    let per = ((media::PREVIEW_RATE as f64 * LEVEL_STEP) as usize).max(1) * 2;
    pcm.chunks(per)
        .map(|c| {
            let peak = c.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            let rms = (c.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / c.len() as f64).sqrt() as f32;
            (peak, rms)
        })
        .collect()
}

/// The mix's loudness over time, like a level meter laid along the timeline:
/// a faint bar to the peak and a solid one to the average, coloured by the same
/// zones as the meters (green, yellow from -18 dB, red from -6 dB), so loud
/// moments stand out.
fn draw_levels(p: &egui::Painter, lane: Rect, levels: &[(f32, f32)], dur: f64) {
    if levels.is_empty() {
        p.text(lane.center(), Align2::CENTER_CENTER, "No sound", FontId::proportional(12.0), Color32::from_gray(110));
        return;
    }
    let inner = lane.shrink2(Vec2::new(0.0, 3.0));
    let y_of = |db: f32| inner.bottom() - ((db - LEVEL_FLOOR) / -LEVEL_FLOOR).clamp(0.0, 1.0) * inner.height();
    let zones = [(LEVEL_FLOOR, -18.0, meter::GREEN), (-18.0, -6.0, meter::YELLOW), (-6.0, 0.0, meter::RED)];
    // Solid from the bottom up to `db`, split at the zone marks.
    let column = |x: f32, db: f32, alpha: f32| {
        for (lo, hi, color) in zones {
            if db <= lo {
                break;
            }
            let (y0, y1) = (y_of(lo), y_of(db.min(hi)));
            p.vline(x, y1..=y0, Stroke::new(1.0, color.gamma_multiply(alpha)));
        }
    };
    let n = levels.len();
    let per_px = n as f64 / lane.width().max(1.0) as f64;
    let covered = (n as f64 * LEVEL_STEP / dur).min(1.0);
    for c in 0..(lane.width() * covered as f32) as usize {
        let a = ((c as f64 * per_px) as usize).min(n - 1);
        let b = (((c + 1) as f64 * per_px) as usize).clamp(a + 1, n);
        let (peak, rms) = levels[a..b].iter().fold((0.0f32, 0.0f32), |(p, r), l| (p.max(l.0), r.max(l.1)));
        let x = lane.left() + c as f32 + 0.5;
        column(x, meter::to_db(peak), 0.35);
        column(x, meter::to_db(rms), 0.9);
    }
    for db in [-18.0, -6.0] {
        p.hline(lane.x_range(), y_of(db), Stroke::new(1.0, Color32::from_white_alpha(18)));
    }
}

/// "1:05.3": minutes, seconds and tenths.
fn clock(t: f64) -> String {
    let tenths = (t.max(0.0) * 10.0 + 1e-6).floor() as u64;
    format!("{}:{:02}.{}", tenths / 600, tenths % 600 / 10, tenths % 10)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_reads_naturally() {
        assert_eq!(clock(0.0), "0:00.0");
        assert_eq!(clock(65.34), "1:05.3");
        assert_eq!(clock(59.96), "0:59.9");
        assert_eq!(clock(600.0), "10:00.0");
    }
}
