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
//!
//! Previous clip / Next clip (P / N) step through the library in its order
//! without going back to it, and F, double-click or ⛶ go fullscreen, so a
//! session's clips can be reviewed one after another.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use egui::{Align2, Color32, FontId, Key, Pos2, Rect, RichText, Sense, Stroke, Vec2};
use media::{ClipInfo, TrackEdit};

use crate::filmstrip::Filmstrip;
use crate::library::ACCENT;
use crate::meter;
use crate::player::Player;
use crate::waveform::Waveform;

/// Scrolling keeps showing quick proxy frames until it's been still this long.
const SCROLL_SETTLE: Duration = Duration::from_millis(200);
/// Arrow keys jump this far.
const JUMP: f64 = 5.0;

pub enum ViewerOutcome {
    Stay,
    Close,
    Edit,
    /// Show this clip instead (the next or previous one).
    Open(PathBuf),
}

/// Where this clip sits in the library, for stepping through clips.
#[derive(Default)]
pub struct Nav {
    /// The clips before and after this one in the library.
    pub previous: Option<PathBuf>,
    pub next: Option<PathBuf>,
    /// This clip's place (from 1) and how many there are.
    pub position: Option<(usize, usize)>,
}

struct Loaded {
    info: ClipInfo,
    pcm: Vec<f32>,
    /// The mix, for the timeline.
    wave: Waveform,
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
    wave: Waveform,
    /// Dragging on the timeline, and whether it was playing when the drag began.
    dragging: Option<bool>,
    /// Last scroll-scrub, and whether it was playing before scrolling began.
    scrolled: Option<(Instant, bool)>,
    /// When the mouse last moved, scrolled or clicked (fullscreen controls).
    last_activity: Instant,
    /// Wheel movement, eased out over the next frames.
    glide: crate::wheel::Glide,
    /// Where the wheel has taken the playhead, unsnapped: the glide's last
    /// small steps add up instead of each being rounded back to its frame.
    scroll_to: Option<f64>,
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
                let wave = Waveform::new(&pcm, media::PREVIEW_RATE);
                Ok(Loaded { info, pcm, wave })
            })();
            let _ = tx.send(result);
            repaint.request_repaint();
        });
        Self { clip: clip.to_path_buf(), state: State::Loading(rx) }
    }

    pub fn clip(&self) -> &Path {
        &self.clip
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, nav: &Nav) -> ViewerOutcome {
        if let State::Loading(rx) = &self.state {
            if let Ok(result) = rx.try_recv() {
                self.state = match result {
                    Ok(l) => State::Ready(Box::new(Ready::new(ui.ctx(), &self.clip, l))),
                    Err(e) => State::Failed(e),
                };
            }
        }
        let ctx = ui.ctx().clone();
        let mut out = if is_fullscreen(&ctx) { ViewerOutcome::Stay } else { self.header(ui, nav) };
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
        if !ctx.egui_wants_keyboard_input() {
            let none = egui::Modifiers::NONE;
            let (esc, prev, next, f) = ctx.input_mut(|i| {
                (i.consume_key(none, Key::Escape), i.consume_key(none, Key::P), i.consume_key(none, Key::N), i.consume_key(none, Key::F))
            });
            if esc {
                // Out of fullscreen first; out of the viewer after that.
                if is_fullscreen(&ctx) {
                    set_fullscreen(&ctx, false);
                } else {
                    out = ViewerOutcome::Close;
                }
            }
            let step = if prev { nav.previous.as_ref() } else if next { nav.next.as_ref() } else { None };
            if let Some(p) = step {
                out = ViewerOutcome::Open(p.clone());
            }
            if f {
                set_fullscreen(&ctx, !is_fullscreen(&ctx));
            }
        }
        if !matches!(out, ViewerOutcome::Stay) {
            if let State::Ready(r) = &mut self.state {
                r.player.pause();
            }
        }
        if matches!(out, ViewerOutcome::Close | ViewerOutcome::Edit) {
            set_fullscreen(&ctx, false);
        }
        out
    }

    fn header(&mut self, ui: &mut egui::Ui, nav: &Nav) -> ViewerOutcome {
        let mut out = ViewerOutcome::Stay;
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button(RichText::new("‹ Clips").size(15.0)).on_hover_text("Back to your clips  (Esc)").clicked() {
                out = ViewerOutcome::Close;
            }
            ui.add_space(6.0);
            let stem = self.clip.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            ui.add(egui::Label::new(RichText::new(crate::clips::title_for_stem(&stem)).size(18.0).strong()).truncate());
            ui.add_space(10.0);
            // Step through the library without leaving the viewer.
            let step = |s: &str| egui::Button::new(RichText::new(s).size(14.0)).min_size(Vec2::new(0.0, 28.0)).corner_radius(6);
            if ui.add_enabled(nav.previous.is_some(), step("‹ Previous clip")).on_hover_text("P").clicked() {
                out = nav.previous.clone().map_or(ViewerOutcome::Stay, ViewerOutcome::Open);
            }
            if ui.add_enabled(nav.next.is_some(), step("Next clip ›")).on_hover_text("N").clicked() {
                out = nav.next.clone().map_or(ViewerOutcome::Stay, ViewerOutcome::Open);
            }
            if let Some((i, n)) = nav.position {
                ui.weak(format!("{i} of {n}"));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let edit = egui::Button::new(RichText::new("✂ Edit").size(14.0).color(Color32::WHITE))
                    .fill(ACCENT)
                    .min_size(Vec2::new(76.0, 30.0))
                    .corner_radius(8);
                if ui.add(edit).on_hover_text("Trim it and adjust its audio").clicked() {
                    out = ViewerOutcome::Edit;
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
        // Dev aid: `HESTECLIPS_START_AT=<seconds>` starts there (to check a
        // stretch of a clip without scrubbing to it).
        if let Some(t) = std::env::var("HESTECLIPS_START_AT").ok().and_then(|s| s.parse::<f64>().ok()) {
            player.seek(t);
        }
        // Opened to watch: start right away.
        player.play(l.info.duration);
        let r = Self {
            strip: Filmstrip::build(ctx, clip, &l.info),
            info: l.info,
            player,
            wave: l.wave,
            dragging: None,
            scrolled: None,
            glide: Default::default(),
            last_activity: Instant::now(),
            scroll_to: None,
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
                self.scroll_to = None;
                if was_playing {
                    self.play();
                }
            } else {
                ctx.request_repaint_after(SCROLL_SETTLE);
            }
        }
        let scrubbing = self.dragging.is_some() || self.scrolled.is_some();

        let full = is_fullscreen(&ctx);

        // --- Picture: everything above the timeline and controls; in
        // fullscreen the whole screen, with the controls over it ---
        let timeline_h = 52.0 + 2.0 + 40.0;
        let controls_h = 8.0 + timeline_h + 8.0 + 34.0 + 6.0;
        let preview_h = if full { ui.available_height() } else { (ui.available_height() - controls_h).max(160.0) };
        let (preview, preview_resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), preview_h), Sense::click());
        ui.painter().rect_filled(preview, if full { 0 } else { 8 }, Color32::BLACK);
        if let Some(size) = self.player.update(&ctx, scrubbing) {
            let scale = (preview.width() / size.x).min(preview.height() / size.y);
            // On whole screen pixels: a half-pixel offset alone blurs text.
            let ppp = ctx.pixels_per_point();
            let r = Rect::from_center_size(preview.center(), size * scale);
            let r = Rect::from_min_size(((r.min.to_vec2() * ppp).round() / ppp).to_pos2(), r.size());
            self.player.paint(ui, r);
        }

        let mut timeline_w = preview.width();
        if full {
            // Over the bottom of the picture while the mouse moves (and a few
            // seconds after) or rests on them; hidden otherwise, cursor too,
            // paused or not (a paused frame is often for a screenshot).
            let bar = Rect::from_min_max(egui::pos2(preview.left(), preview.bottom() - controls_h - 24.0), preview.max);
            let shown = self.controls_shown(&ctx, bar);
            if shown > 0.0 {
                // Clear at the top, near-black within 40 points: readable over
                // any picture.
                let shade = |a: f32| Color32::from_black_alpha((a * shown) as u8);
                let mid = bar.top() + 40.0;
                let fade = egui::Mesh {
                    indices: vec![0, 1, 2, 2, 1, 3, 2, 3, 4, 4, 3, 5],
                    vertices: [
                        (bar.left_top(), 0.0),
                        (bar.right_top(), 0.0),
                        (egui::pos2(bar.left(), mid), 190.0),
                        (egui::pos2(bar.right(), mid), 190.0),
                        (bar.left_bottom(), 225.0),
                        (bar.right_bottom(), 225.0),
                    ]
                    .into_iter()
                    .map(|(pos, a)| egui::epaint::Vertex { pos, uv: egui::epaint::WHITE_UV, color: shade(a) })
                    .collect(),
                    texture_id: Default::default(),
                };
                ui.painter().add(fade);
                let area = Rect::from_min_max(egui::pos2(bar.left() + 20.0, bar.top() + 24.0), egui::pos2(bar.right() - 20.0, bar.bottom()));
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(area));
                child.multiply_opacity(shown);
                timeline_w = self.controls(&mut child, preview, timeline_h);
            } else if pointer_over(&ctx, preview) {
                ctx.set_cursor_icon(egui::CursorIcon::None);
            }
        } else {
            timeline_w = self.controls(ui, preview, timeline_h);
        }

        let preview_resp = preview_resp.on_hover_cursor(if full { egui::CursorIcon::Default } else { egui::CursorIcon::PointingHand });
        if preview_resp.double_clicked() {
            // The first click of the two already toggled playing: undo that.
            self.toggle_play();
            set_fullscreen(&ctx, !is_fullscreen(&ctx));
        } else if preview_resp.clicked() {
            self.toggle_play();
        }

        // Scroll over the picture (or the timeline, over or under it) to
        // scrub through the clip.
        let over = pointer_over(&ctx, preview) || ctx.pointer_hover_pos().is_some_and(|p| p.y > preview.bottom());
        let mut input = if over { crate::wheel::read(&ctx).scroll } else { Vec2::ZERO };
        input.y += dev_wheel();
        // Down or left (towards you, or swiping left) goes forward, like reading on.
        let d = self.glide.step(&ctx, input);
        let px = -(d.x + d.y);
        if px.abs() > 0.0 {
            let was = match self.scrolled {
                Some((_, was)) => was,
                None => self.player.is_playing(),
            };
            self.player.pause();
            let dt = px as f64 / timeline_w.max(1.0) as f64 * self.info.duration;
            // Unsnapped, so the glide's last small steps add up.
            let to = (self.scroll_to.unwrap_or_else(|| self.player.time()) + dt).clamp(0.0, self.info.duration);
            self.scroll_to = Some(to);
            self.seek(to);
            self.scrolled = Some((Instant::now(), was));
        }
        if let Some(e) = &self.player.audio_error {
            ui.colored_label(ui.visuals().warn_fg_color, format!("No sound: {e}"));
        }
    }

    /// How visible the fullscreen controls are (0..=1, fading): shown while
    /// the mouse moves, clicks or scrolls and for a few seconds after, while
    /// the pointer rests on them (`bar`), and while dragging on the timeline.
    fn controls_shown(&mut self, ctx: &egui::Context, bar: Rect) -> f32 {
        const LINGER: Duration = Duration::from_millis(2500);
        let active = ctx.input(|i| {
            i.pointer.delta() != Vec2::ZERO || i.pointer.any_down() || i.raw.events.iter().any(|e| matches!(e, egui::Event::MouseWheel { .. }))
        });
        if active {
            self.last_activity = Instant::now();
        }
        let visible = self.last_activity.elapsed() < LINGER || pointer_over(ctx, bar) || self.dragging.is_some();
        if visible {
            ctx.request_repaint_after(LINGER.saturating_sub(self.last_activity.elapsed()) + Duration::from_millis(20));
        }
        ctx.animate_bool_with_time(egui::Id::new("viewer_controls"), visible, 0.2)
    }

    /// The timeline and the play / time / volume row, from the top of `ui`.
    /// Returns the timeline's width (wheel scrubbing scales by it).
    fn controls(&mut self, ui: &mut egui::Ui, preview: Rect, timeline_h: f32) -> f32 {
        ui.add_space(8.0);
        let (outer, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), timeline_h), Sense::click_and_drag());
        self.timeline(ui, outer, &resp, preview);

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.set_min_height(34.0);
            let label = if self.player.is_playing() { "\u{23f8}" } else { "\u{25b6}" };
            let play = egui::Button::new(RichText::new(label).size(18.0)).min_size(Vec2::new(44.0, 32.0)).corner_radius(8);
            if ui.add(play).on_hover_text("Play / pause  (Space)").clicked() {
                self.toggle_play();
            }
            ui.add_space(8.0);
            ui.label(RichText::new(clock(self.player.time())).monospace().size(15.0));
            ui.weak(format!("/ {}", clock(self.info.duration)));

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let full = is_fullscreen(ui.ctx());
                let fs = egui::Button::new(RichText::new("\u{26f6}").size(16.0)).min_size(Vec2::new(32.0, 28.0)).corner_radius(6).selected(full);
                if ui.add(fs).on_hover_text(if full { "Exit fullscreen  (F or Esc)" } else { "Fullscreen  (F, or double-click the picture)" }).clicked() {
                    set_fullscreen(ui.ctx(), !full);
                }
                ui.add_space(8.0);
                let mut changed = false;
                let slider = egui::Slider::new(&mut self.volume, 0.0..=1.0).show_value(false);
                changed |= ui.add_sized(Vec2::new(100.0, 20.0), slider).on_hover_text("Volume").changed();
                let icon = if self.muted || self.volume == 0.0 { "\u{1f507}" } else { "\u{1f50a}" };
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
                ui.weak("Scroll to scrub \u{b7} arrow keys jump 5 s \u{b7} , and . step one frame");
            });
        });
        outer.width()
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
        self.strip.paint(ui, video, 0.0, dur, dur);
        p.rect_filled(audio, 4, v.extreme_bg_color);
        if self.wave.is_empty() {
            p.text(audio.center(), Align2::CENTER_CENTER, "No sound", FontId::proportional(12.0), Color32::from_gray(110));
        } else {
            // Coloured like the meters, so loud moments stand out.
            self.wave.paint(&ui.painter_at(audio), audio, (0.0, dur), |_| 1.0, |_, rms| zone(meter::to_db(rms)));
        }

        // Already-played part of the levels, lightly marked, so where you are reads at a glance.
        let xp = x_of(self.info.snap(self.player.time()));
        p.rect_filled(Rect::from_min_max(audio.min, Pos2::new(xp, audio.bottom())), 4, Color32::from_white_alpha(10));

        // Scrub proxy still building: a thin bar until every frame is scrubbable.
        crate::player::paint_proxy_bar(ui, &self.player, video, 0.0, dur);

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

fn is_fullscreen(ctx: &egui::Context) -> bool {
    ctx.input(|i| i.viewport().fullscreen.unwrap_or(false))
}

fn set_fullscreen(ctx: &egui::Context, on: bool) {
    if is_fullscreen(ctx) != on {
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(on));
    }
}

/// Dev aid: `HESTECLIPS_DEV_WHEEL=3.0:-40,3.3:-40` scrolls by those points at
/// those times (seconds since start), as wheel notches would, to reproduce
/// wheel scrubbing without touching the mouse.
fn dev_wheel() -> f32 {
    static PLAN: std::sync::OnceLock<std::sync::Mutex<Vec<(f64, f32)>>> = std::sync::OnceLock::new();
    let plan = PLAN.get_or_init(|| {
        let spec = std::env::var("HESTECLIPS_DEV_WHEEL").unwrap_or_default();
        let steps = spec.split(',').filter_map(|s| s.split_once(':')).filter_map(|(t, d)| Some((t.trim().parse().ok()?, d.trim().parse().ok()?))).collect();
        std::sync::Mutex::new(steps)
    });
    let now = crate::player::uptime();
    let mut plan = plan.lock().unwrap();
    let due: f32 = plan.iter().filter(|(t, _)| *t <= now).map(|(_, d)| *d).sum();
    plan.retain(|(t, _)| *t > now);
    due
}

fn pointer_over(ctx: &egui::Context, rect: Rect) -> bool {
    ctx.pointer_hover_pos().is_some_and(|p| rect.contains(p))
}

fn volume_id() -> egui::Id {
    egui::Id::new("viewer_volume")
}

/// Meter colour for a loudness (RMS): green, yellow from -18 dB, red from -6 dB.
fn zone(db: f32) -> Color32 {
    if db >= -6.0 {
        meter::RED
    } else if db >= -18.0 {
        meter::YELLOW
    } else {
        meter::GREEN
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
