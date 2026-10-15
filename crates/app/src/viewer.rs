//! The clip viewer: clicking a clip plays it here instead of in another app.
//!
//! Built for finding a moment fast. The timeline under the picture is the
//! filmstrip over the clip's audio levels, so a shot, a shout or a quiet patch
//! is visible before you get there. Hovering shows that moment's picture; click
//! or drag to go there; scroll anywhere over the picture or the timeline to
//! scrub. Scrubbing decodes the frame under the playhead live (see
//! `video.rs`), the same as in the editor: keyframes while it moves fast,
//! every frame when it's slow, and the exact frame when you stop.
//!
//! It plays the clip as it's shared: the saved edit if there is one, and the
//! mix (track 1).
//!
//! Share and Rename sit in the header; dragging the picture out of the window
//! drops the clip into any app, as dragging a card does in the library.
//!
//! Previous clip / Next clip (P / N) step through the library in its order
//! without going back to it, and F, double-click or ⛶ go fullscreen, so a
//! session's clips can be reviewed one after another.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use egui::{Align2, Color32, FontId, Key, Pos2, Rect, Sense, Stroke, Vec2};
use media::pcm::Pcm;
use media::{ClipInfo, TrackEdit};

use crate::filmstrip::Filmstrip;
use crate::player::Player;

/// The timeline: ruler, filmstrip and sound, and the space under it.
const TIMELINE_H: f32 = 26.0 + 56.0 + 4.0 + 48.0 + 14.0;
/// Scrolling counts as scrubbing until it's been still this long.
const SCROLL_SETTLE: Duration = Duration::from_millis(200);
/// Arrow keys jump this far.
const JUMP: f64 = 5.0;

pub enum ViewerOutcome {
    Stay,
    Close,
    Edit,
    /// Show this clip instead (the next or previous one).
    Open(PathBuf),
    Share(crate::library::ShareChoice),
    Rename,
    /// Put the clip in this collection, or take it out (`false`).
    InCollection(String, bool),
    /// Make a collection with the clip in it.
    NewCollection,
    /// Delete the clip (to the Recycle Bin, or for good after asking).
    Delete,
    /// The picture was dragged: hand the clip to the OS to drop into an app.
    DragOut,
    /// Play it in the OS default video player instead.
    OpenExternal,
    /// Show the keyboard shortcuts.
    Keys,
}

/// Where this clip sits in the library, for stepping through clips.
#[derive(Default)]
pub struct Nav {
    /// The clips before and after this one in the library.
    pub previous: Option<PathBuf>,
    pub next: Option<PathBuf>,
    /// This clip's place (from 1) and how many there are.
    pub position: Option<(usize, usize)>,
    /// Your collections: (id, name, whether this clip is in it).
    pub collections: Vec<(String, String, bool)>,
    /// What deleting does, for its button: "Move to Recycle Bin" or "Delete…".
    pub delete_label: String,
    /// The clip's edit being saved, 0..=1.
    pub saving: Option<f32>,
}

struct Loaded {
    info: ClipInfo,
    /// The mix, filling in as it's decoded (it plays from the start at once).
    pcm: Arc<Pcm>,
    /// Where this file starts in the recording and how long that was: an
    /// edited clip is part of its original (moments are kept for that).
    span: (f64, f64),
}

enum State {
    /// The clip's edit is being saved: it opens once it's done (its file
    /// is about to be replaced, which an open file would block).
    Saving,
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
    /// The mix's waveform, growing as the sound is decoded.
    wave: Arc<crate::waveform::Live>,
    /// Dragging on the timeline, and whether it was playing when the drag began.
    dragging: Option<bool>,
    /// Last scroll-scrub, and whether it was playing before scrolling began.
    scrolled: Option<(Instant, bool)>,
    /// `,` held down (repeating): stepping back goes as a scrub does, with
    /// the frames on the way kept, sharp once it's let go.
    stepping_back: bool,
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
    /// How fast it plays (1.0 as recorded).
    speed: f32,
    /// The picture was dragged this frame: share the clip by drag and drop.
    drag_out: bool,
    /// Times shown as frames instead of minutes and seconds.
    show_frames: bool,
    /// The big play / pause sign after a click on the picture: which, and when.
    flash_play: Option<(bool, Instant)>,
    /// The clip's kills and your markers (see `moments.rs`), and where this
    /// file sits in the recording they're kept for.
    moments: crate::moments::Moments,
    span: (f64, f64),
    clip: PathBuf,
}

impl Viewer {
    pub fn open(ctx: &egui::Context, clip: &Path) -> Self {
        if crate::player::debug() {
            eprintln!("{:>8.3} viewer: opening {}", crate::player::uptime(), clip.display());
        }
        let (tx, rx) = mpsc::channel();
        let src = clip.to_path_buf();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            let result = (|| -> Result<Loaded, String> {
                let info = media::probe(&src).map_err(|e| e.to_string())?;
                // Track 1 is the mix in our clips; in any other file it's simply
                // the first. Playing starts with what's decoded: no waiting for
                // the whole file to be read.
                let tracks: Vec<usize> = info.audio.first().map(|a| a.index).into_iter().collect();
                if crate::player::debug() {
                    eprintln!("{:>8.3} viewer: clip read", crate::player::uptime());
                }
                let pcm = media::pcm::decode_streaming(&src, &tracks, info.duration, move |_, result| {
                    if let Err(e) = result {
                        eprintln!("decoding the sound: {e:#}");
                    }
                    if crate::player::debug() {
                        eprintln!("{:>8.3} viewer: sound all decoded", crate::player::uptime());
                    }
                });
                let span = recording_span(&src, info.duration);
                Ok(Loaded { info, pcm, span })
            })();
            if let Err(e) = &result {
                eprintln!("couldn't open {} in the player: {e}", src.display());
            }
            let _ = tx.send(result);
            repaint.request_repaint();
        });
        Self { clip: clip.to_path_buf(), state: State::Loading(rx) }
    }

    pub fn clip(&self) -> &Path {
        &self.clip
    }

    /// The clip, while its edit is being saved: the app opens it with
    /// [`Viewer::open`] when that's done.
    pub fn saving(clip: &Path) -> Self {
        Self { clip: clip.to_path_buf(), state: State::Saving }
    }

    pub fn is_saving(&self) -> bool {
        matches!(self.state, State::Saving)
    }

    /// The clip was renamed from `from` to `to`: opened again under its new
    /// name (frames are read by path), at the same moment.
    pub fn renamed(&mut self, ctx: &egui::Context, from: &Path, to: &Path) {
        if self.clip != from {
            return;
        }
        self.clip = to.to_path_buf();
        match &mut self.state {
            // Everything open keeps playing; only reopening by name follows.
            State::Ready(r) => {
                r.player.renamed(ctx, to);
                r.strip.renamed(to);
            }
            // Still loading from the old name: start again.
            _ => *self = Self::open(ctx, to),
        }
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
            State::Saving => {
                let rect = ui.available_rect_before_wrap();
                ui.painter().rect_filled(rect, 0, crate::theme::pal(ui).stage);
                let f = nav.saving.unwrap_or(0.0);
                ui.scope_builder(egui::UiBuilder::new().max_rect(Rect::from_center_size(rect.center(), Vec2::new(280.0, 60.0))), |ui| {
                    ui.vertical_centered(|ui| {
                        ui.label(crate::theme::text(format!("Saving your edit  {:.0}%", f * 100.0), 14.0, crate::theme::W::Medium).color(Color32::WHITE));
                        ui.add_space(10.0);
                        let (bar, _) = ui.allocate_exact_size(Vec2::new(220.0, 3.0), Sense::hover());
                        ui.painter().rect_filled(bar, 2, Color32::from_white_alpha(40));
                        let w = crate::theme::value(ui.ctx(), egui::Id::new("viewer_saving"), f, crate::theme::NORMAL);
                        ui.painter().rect_filled(Rect::from_min_max(bar.min, Pos2::new(bar.left() + bar.width() * w, bar.bottom())), 2, Color32::WHITE);
                    });
                });
                ctx.request_repaint_after(Duration::from_millis(100));
            }
            State::Loading(_) => {
                let rect = ui.available_rect_before_wrap();
                ui.painter().rect_filled(rect, 0, crate::theme::pal(ui).stage);
                ui.put(Rect::from_center_size(rect.center(), Vec2::splat(24.0)), egui::Spinner::new().size(22.0).color(Color32::from_white_alpha(170)));
            }
            State::Failed(e) => {
                let p = crate::theme::pal(ui);
                ui.vertical_centered(|ui| {
                    ui.add_space(80.0);
                    ui.label(crate::theme::text("Couldn't play this clip", 15.0, crate::theme::W::Semibold).color(p.text));
                    ui.add_space(4.0);
                    ui.label(crate::theme::text(e.as_str(), 13.0, crate::theme::W::Regular).color(p.text3));
                });
            }
            State::Ready(r) => r.ui(ui),
        }
        if !ctx.egui_wants_keyboard_input() && !crate::ui_kit::overlay_open(&ctx) {
            let none = egui::Modifiers::NONE;
            let (esc, prev, next, f, f2, delete, keys) = ctx.input_mut(|i| {
                (
                    i.consume_key(none, Key::Escape),
                    i.consume_key(none, Key::Q),
                    i.consume_key(none, Key::E),
                    i.consume_key(none, Key::F),
                    i.consume_key(none, Key::F2),
                    i.consume_key(none, Key::Delete) || i.consume_key(egui::Modifiers::COMMAND, Key::Backspace),
                    i.consume_key(egui::Modifiers::SHIFT, Key::Slash) || i.consume_key(none, Key::Questionmark),
                )
            });
            if keys {
                out = ViewerOutcome::Keys;
            }
            if f2 {
                out = ViewerOutcome::Rename;
            }
            if delete && !matches!(self.state, State::Saving) {
                out = ViewerOutcome::Delete;
            }
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
        if matches!(out, ViewerOutcome::Close | ViewerOutcome::Edit | ViewerOutcome::Open(_)) {
            if let State::Ready(r) = &mut self.state {
                r.player.pause();
            }
        }
        if let State::Ready(r) = &mut self.state {
            if std::mem::take(&mut r.drag_out) {
                out = ViewerOutcome::DragOut;
            }
        }
        if matches!(out, ViewerOutcome::Close | ViewerOutcome::Edit) {
            set_fullscreen(&ctx, false);
        }
        out
    }

    /// Back and the clip's name on the left, previous and next clip in the
    /// middle, Edit, Share and ⋯ on the right.
    fn header(&mut self, ui: &mut egui::Ui, nav: &Nav) -> ViewerOutcome {
        use crate::ui_kit::{Btn, Item, Kind};
        use egui_phosphor::regular as icon;
        let mut out = ViewerOutcome::Stay;
        let stem = self.clip.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let title = crate::clips::title_for_stem(&stem);
        // While its edit saves, the clip's file is about to change.
        let ready = !matches!(self.state, State::Saving);
        let wait = "Wait for the edit to finish saving";
        let mut mark = false;
        let (back, rename) = crate::transport::header(
            ui,
            &title,
            false,
            // Narrow: no "n of m" between the arrows.
            if nav.position.is_some() { if ui.ctx().content_rect().width() < 560.0 { 60.0 } else { 120.0 } } else { 0.0 },
            |ui, part| match part {
            crate::transport::Part::Centre => {
                let p = crate::theme::pal(ui);
                ui.spacing_mut().item_spacing.x = 2.0;
                if Btn::icon(Kind::Ghost, icon::CARET_LEFT).small().enabled(nav.previous.is_some()).tip("Previous clip  (Q)").show(ui).clicked() {
                    out = nav.previous.clone().map_or(ViewerOutcome::Stay, ViewerOutcome::Open);
                }
                if let Some((i, n)) = nav.position.filter(|_| ui.ctx().content_rect().width() >= 560.0) {
                    let text = format!("{i} of {n}");
                    let g = ui.painter().layout_no_wrap(text, crate::theme::font(13.0, crate::theme::W::Regular), p.text3);
                    let (r, _) = ui.allocate_exact_size(egui::vec2(120.0 - 2.0 * 26.0 - 4.0, 26.0), Sense::hover());
                    ui.painter().galley(r.center() - g.size() / 2.0, g, p.text3);
                }
                if Btn::icon(Kind::Ghost, icon::CARET_RIGHT).small().enabled(nav.next.is_some()).tip("Next clip  (E)").show(ui).clicked() {
                    out = nav.next.clone().map_or(ViewerOutcome::Stay, ViewerOutcome::Open);
                }
            }
            crate::transport::Part::Right => {
                // ⋯: collections, rename, delete.
                let more = Btn::icon(Kind::Ghost, icon::DOTS_THREE).tip("More").show(ui);
                crate::ui_kit::menu_popup(&more, false, 250.0, |ui| {
                    crate::ui_kit::submenu(ui, Item::new("Add to collection").icon(icon::FOLDER_PLUS), |ui| {
                        for (id, name, inside) in &nav.collections {
                            let tip = if *inside { "Take out of this collection" } else { "Add to this collection" };
                            if Item::new(name.clone()).check(*inside).tip(tip).show(ui).clicked() {
                                out = ViewerOutcome::InCollection(id.clone(), !inside);
                            }
                        }
                        if !nav.collections.is_empty() {
                            crate::ui_kit::menu_sep(ui);
                        }
                        if Item::new("New collection…").icon(icon::PLUS).show(ui).clicked() {
                            out = ViewerOutcome::NewCollection;
                        }
                    });
                    if Item::new("Rename…").icon(icon::PENCIL_SIMPLE).keys(Some(vec!["F2".into()])).show(ui).clicked() {
                        out = ViewerOutcome::Rename;
                    }
                    if Item::new("Add a marker here").icon(icon::BOOKMARK_SIMPLE).keys(Some(vec!["B".into()])).enabled(matches!(self.state, State::Ready(_))).tip("A pin on the timeline to come back to: [ and ] jump between them").show(ui).clicked() {
                        mark = true;
                    }
                    if Item::new("Open in another player").icon(icon::ARROW_SQUARE_OUT).show(ui).clicked() {
                        out = ViewerOutcome::OpenExternal;
                    }
                    crate::ui_kit::menu_sep(ui);
                    if Item::new("Keyboard shortcuts").icon(icon::KEYBOARD).keys(Some(vec!["?".into()])).show(ui).clicked() {
                        out = ViewerOutcome::Keys;
                    }
                    crate::ui_kit::menu_sep(ui);
                    let del_key = Some(vec![if cfg!(target_os = "macos") { icon::BACKSPACE.to_owned() } else { "Del".to_owned() }]);
                    if Item::new(nav.delete_label.clone()).icon(icon::TRASH).danger().enabled(ready).keys(del_key).show(ui).clicked() {
                        out = ViewerOutcome::Delete;
                    }
                });
                let share_tip = if crate::share::CAN_DRAG_OUT { "Copy it, send it or upload it, or drag the picture into any app" } else { "Copy it, send it or upload it" };
                // Narrow: Share and Edit as their icons.
                let narrow = ui.ctx().content_rect().width() < 700.0;
                let share = if narrow { Btn::icon(Kind::Primary, icon::EXPORT).tip("Share") } else { Btn::primary("Share").with_icon(icon::EXPORT) };
                let share = share.enabled(ready).tip(if ready { share_tip } else { wait }).show(ui);
                if let Some(c) = crate::ui_kit::menu_popup(&share, false, 250.0, |ui| crate::library::share_menu(ui, share.rect.left_bottom())).flatten() {
                    out = ViewerOutcome::Share(c);
                }
                let edit = if narrow { Btn::icon(Kind::Normal, icon::SCISSORS) } else { Btn::normal("Edit").with_icon(icon::SCISSORS) };
                if edit.enabled(ready).tip(if ready { "Trim it, adjust its sound, or save it smaller" } else { wait }).show(ui).clicked() {
                    out = ViewerOutcome::Edit;
                }
            }
            crate::transport::Part::Left => {}
            },
        );
        if back {
            out = ViewerOutcome::Close;
        }
        if rename {
            out = ViewerOutcome::Rename;
        }
        if mark && let State::Ready(r) = &mut self.state {
            r.toggle_marker();
        }
        out
    }
}

impl Ready {
    fn new(ctx: &egui::Context, clip: &Path, l: Loaded) -> Self {
        let volume = volume(ctx).unwrap_or((1.0, false));
        let wave = crate::waveform::follow(l.pcm.clone(), 0, ctx);
        let mut player = Player::new(ctx, clip, l.info.clone(), l.pcm, Vec::new());
        // Dev aid: `HASTECLIP_START_AT=<seconds>` starts there (to check a
        // stretch of a clip without scrubbing to it).
        if let Some(t) = std::env::var("HASTECLIP_START_AT").ok().and_then(|s| s.parse::<f64>().ok()) {
            player.seek(t);
        }
        // Opened to watch: start right away.
        player.play(l.info.duration);
        let r = Self {
            strip: Filmstrip::build(ctx, clip, &l.info),
            info: l.info,
            player,
            wave,
            dragging: None,
            scrolled: None,
            stepping_back: false,
            glide: Default::default(),
            last_activity: Instant::now(),
            scroll_to: None,
            hover: None,
            volume: volume.0,
            muted: volume.1,
            speed: speed(ctx),
            drag_out: false,
            show_frames: false,
            flash_play: None,
            moments: crate::moments::of(clip),
            span: l.span,
            clip: clip.to_path_buf(),
        };
        r.apply_volume(ctx);
        r.player.set_speed(r.speed);
        r
    }

    /// Play at `speed` (one of [`SPEEDS`]); every clip opened after plays
    /// at it too.
    fn set_speed(&mut self, ctx: &egui::Context, speed: f32) {
        self.speed = speed;
        self.player.set_speed(speed);
        ctx.data_mut(|d| d.insert_temp(speed_id(), speed));
    }

    /// The clip's moments as pins on this file's timeline.
    fn pins(&self) -> Vec<crate::transport::Moment> {
        let (start, recording) = self.span;
        let mut pins = self.moments.pins(recording);
        for m in &mut pins {
            m.t -= start;
        }
        pins.retain(|m| (0.0..=self.info.duration).contains(&m.t));
        pins
    }

    /// Add a marker where the playhead is (`B`), or take away the one
    /// that's there.
    fn toggle_marker(&mut self) {
        let t = self.info.snap(self.player.time());
        let at = t + self.span.0;
        if let Some(&m) = self.moments.markers.iter().find(|m| (*m - at).abs() < self.info.frame_duration().max(0.02)) {
            crate::moments::remove_marker(&self.clip, m);
        } else {
            crate::moments::add_marker(&self.clip, at);
        }
        self.moments = crate::moments::of(&self.clip);
    }

    /// One step faster (`up`) or slower through [`SPEEDS`].
    fn step_speed(&mut self, ctx: &egui::Context, up: bool) {
        let i = SPEEDS.iter().position(|s| *s >= self.speed).unwrap_or(SPEEDS.len() - 1);
        let to = if up { SPEEDS.get(i + 1) } else { i.checked_sub(1).and_then(|i| SPEEDS.get(i)) };
        if let Some(&to) = to {
            self.set_speed(ctx, to);
        }
    }

    fn apply_volume(&self, ctx: &egui::Context) {
        let gain = if self.muted { 0.0 } else { self.volume * self.volume }; // a squared slider feels even
        self.player.set_mix(vec![TrackEdit { index: 0, gain, muted: false, points: Vec::new() }]);
        set_volume(ctx, (self.volume, self.muted));
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
        match dev_drag() {
            Some(Some(t)) => {
                if self.dragging.is_none() {
                    self.dragging = Some(self.player.is_playing());
                    self.player.pause();
                }
                self.seek(t);
                ctx.request_repaint();
            }
            Some(None) => {
                if self.dragging.take() == Some(true) {
                    self.play();
                }
            }
            None => {}
        }
        let scrubbing = self.dragging.is_some() || self.scrolled.is_some();

        let full = is_fullscreen(&ctx);

        // --- Picture: everything above the transport and timeline; in
        // fullscreen the whole screen, with them floating over it ---
        let controls_h = crate::transport::BAR_H + TIMELINE_H + 14.0;
        let preview_h = if full { ui.available_height() } else { (ui.available_height() - controls_h).max(160.0) };
        // Click to play or pause; drag out of the window to share the clip.
        let (preview, preview_resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), preview_h), Sense::click_and_drag());
        self.player.set_stepping(self.stepping_back);
        let size = self.player.update(&ctx, scrubbing);
        let player = &self.player;
        crate::transport::stage(ui, preview, size, |p, cover| player.paint_glow(p, cover, egui::Id::new("viewer_stage")), |r| player.paint(ui, r));
        if let Some(f) = self.flash_play.filter(|(_, at)| at.elapsed() < Duration::from_millis(450)) {
            // A big play / pause sign that grows and fades where you clicked.
            let t = f.1.elapsed().as_secs_f32() / 0.45;
            let r = 32.0 * (0.8 + 0.5 * t);
            let c = preview.center();
            ui.painter().circle_filled(c, r, Color32::from_black_alpha((128.0 * (1.0 - t)) as u8));
            let glyph = if f.0 { egui_phosphor::fill::PLAY } else { egui_phosphor::fill::PAUSE };
            ui.painter().text(c + Vec2::new(if f.0 { 2.0 } else { 0.0 }, 0.0), Align2::CENTER_CENTER, glyph, FontId::new(28.0 * (0.8 + 0.5 * t), crate::theme::fill_family()), Color32::from_white_alpha((255.0 * (1.0 - t)) as u8));
            ctx.request_repaint();
        }

        let mut timeline_w = preview.width();
        if full {
            // Over the bottom of the picture while the mouse moves (and a few
            // seconds after) or rests on them; hidden otherwise, cursor too,
            // paused or not (a paused frame is often for a screenshot).
            let bar = Rect::from_min_max(egui::pos2(preview.left(), preview.bottom() - controls_h - 110.0), preview.max);
            let shown = self.controls_shown(&ctx, bar);
            if shown > 0.0 {
                // Clear at the top, near-black at the bottom: readable over
                // any picture.
                let shade = |a: f32| Color32::from_black_alpha((a * shown) as u8);
                let mid = bar.top() + bar.height() * 0.4;
                let fade = egui::Mesh {
                    indices: vec![0, 1, 2, 2, 1, 3, 2, 3, 4, 4, 3, 5],
                    vertices: [
                        (bar.left_top(), 0.0),
                        (bar.right_top(), 0.0),
                        (egui::pos2(bar.left(), mid), 140.0),
                        (egui::pos2(bar.right(), mid), 140.0),
                        (bar.left_bottom(), 210.0),
                        (bar.right_bottom(), 210.0),
                    ]
                    .into_iter()
                    .map(|(pos, a)| egui::epaint::Vertex { pos, uv: egui::epaint::WHITE_UV, color: shade(a) })
                    .collect(),
                    texture_id: Default::default(),
                };
                ui.painter().add(fade);
                let area = Rect::from_min_max(egui::pos2(bar.left() + 10.0, preview.bottom() - controls_h - 4.0 + 8.0 * (1.0 - shown)), egui::pos2(bar.right() - 10.0, bar.bottom()));
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(area));
                child.multiply_opacity(shown);
                // White on the shade, whatever the look.
                child.visuals_mut().dark_mode = true;
                timeline_w = self.controls(&mut child, preview, true);
            } else if pointer_over(&ctx, preview) {
                ctx.set_cursor_icon(egui::CursorIcon::None);
            }
        } else {
            timeline_w = self.controls(ui, preview, false);
        }

        let preview_resp = preview_resp.on_hover_cursor(if full { egui::CursorIcon::Default } else { egui::CursorIcon::PointingHand });
        if crate::share::CAN_DRAG_OUT && preview_resp.drag_started() {
            self.drag_out = true;
        }
        if preview_resp.double_clicked() {
            // The first click of the two already toggled playing: undo that.
            self.toggle_play();
            self.flash_play = None;
            set_fullscreen(&ctx, !is_fullscreen(&ctx));
        } else if preview_resp.clicked() {
            self.toggle_play();
            self.flash_play = Some((self.player.is_playing(), Instant::now()));
        }

        // Scroll over the picture (or the timeline, over or under it) to
        // scrub through the clip.
        let over = pointer_over(&ctx, preview) || (!crate::ui_kit::overlay_open(&ctx) && ctx.pointer_hover_pos().is_some_and(|p| p.y > preview.bottom()));
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

    /// The transport bar and the timeline under it, from the top of `ui`.
    /// Returns the timeline's width (wheel scrubbing scales by it).
    fn controls(&mut self, ui: &mut egui::Ui, preview: Rect, over_picture: bool) -> f32 {
        use crate::ui_kit::{Btn, Kind};
        let ctx = ui.ctx().clone();
        let bar = Rect::from_min_size(ui.cursor().min, Vec2::new(ui.available_width(), crate::transport::BAR_H));
        ui.allocate_rect(bar, Sense::hover());
        let moments = self.pins();
        let t = self.player.time();
        let prev = moments.iter().rev().find(|m| m.t < t - 0.05).cloned();
        let next = moments.iter().find(|m| m.t > t + 0.05).cloned();
        let mut toggle = false;
        let mut jump = None;
        let mut frames = self.show_frames;
        crate::transport::bar(
            ui,
            if over_picture { bar.with_min_y(bar.top() + 1.0) } else { bar },
            150.0,
            |ui, part| match part {
            crate::transport::Part::Left => crate::transport::timecode(ui, t, self.info.duration, self.info.fps, &mut frames),
            crate::transport::Part::Centre => {
                let c = crate::transport::centre(ui, self.player.is_playing(), prev.as_ref(), next.as_ref());
                toggle = c.play;
                if c.prev {
                    jump = prev.as_ref().map(|m| m.t);
                }
                if c.next {
                    jump = next.as_ref().map(|m| m.t);
                }
            }
            crate::transport::Part::Right => {
                let full = is_fullscreen(ui.ctx());
                let fs_icon = if full { egui_phosphor::regular::CORNERS_IN } else { egui_phosphor::regular::CORNERS_OUT };
                if Btn::icon(Kind::Ghost, fs_icon).tip(if full { "Exit fullscreen  (F or Esc)" } else { "Fullscreen  (F, or double-click the picture)" }).show(ui).clicked() {
                    set_fullscreen(ui.ctx(), !full);
                }
                let mut v = if self.muted { 0.0 } else { self.volume };
                let changed = crate::transport::slider(ui, &mut v, 100.0).on_hover_text("Volume").changed();
                if changed {
                    self.volume = v;
                    self.muted = false;
                    self.apply_volume(ui.ctx());
                }
                let speaker = if self.muted || self.volume == 0.0 { egui_phosphor::regular::SPEAKER_X } else { egui_phosphor::regular::SPEAKER_HIGH };
                if Btn::icon(Kind::Ghost, speaker).small().tip(if self.muted { "Unmute  (M)" } else { "Mute  (M)" }).show(ui).clicked() {
                    self.muted = !self.muted;
                    self.apply_volume(ui.ctx());
                }
                // How fast it plays: lit when it isn't as recorded.
                let kind = if self.speed == 1.0 { Kind::Ghost } else { Kind::On };
                let r = Btn::new(kind, speed_label(self.speed)).small().id("viewer_speed").tip("Playback speed  (< >)").show(ui);
                let mut chosen = None;
                crate::ui_kit::menu_popup(&r, false, 130.0, |ui| {
                    for s in SPEEDS {
                        let label = if s == 1.0 { "1×  Normal".to_owned() } else { speed_label(s) };
                        if crate::ui_kit::Item::new(label).check(self.speed == s).show(ui).clicked() {
                            chosen = Some(s);
                        }
                    }
                });
                if let Some(s) = chosen {
                    self.set_speed(ui.ctx(), s);
                }
            }
            },
        );
        self.show_frames = frames;
        if toggle {
            self.toggle_play();
        }
        if let Some(t) = jump {
            self.seek(t);
        }
        let _ = ctx;
        let (outer, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), TIMELINE_H), Sense::click_and_drag());
        let lanes = outer.shrink2(Vec2::new(14.0, 0.0)).with_max_y(outer.bottom() - 14.0);
        self.timeline(ui, lanes, &resp, preview, &moments);
        lanes.width()
    }

    /// The ruler with the clip's moments, the filmstrip, then the sound,
    /// with the playhead through them all. The played part is bright, the
    /// rest dimmed. Hover to see a moment; click or drag to go there.
    fn timeline(&mut self, ui: &mut egui::Ui, outer: Rect, resp: &egui::Response, preview: Rect, moments: &[crate::transport::Moment]) {
        let p = crate::theme::pal(ui);
        let painter = ui.painter_at(outer.expand(8.0));
        let dur = self.info.duration.max(1e-6);
        let ruler = Rect::from_min_size(outer.min, Vec2::new(outer.width(), crate::transport::RULER_H));
        let video = Rect::from_min_size(Pos2::new(outer.left(), ruler.bottom()), Vec2::new(outer.width(), 56.0));
        let audio = Rect::from_min_max(Pos2::new(outer.left(), video.bottom() + 4.0), outer.max);
        let x_of = |t: f64| outer.left() + (t / dur) as f32 * outer.width();
        let t_of = |x: f32| ((x - outer.left()) / outer.width()).clamp(0.0, 1.0) as f64 * dur;

        match crate::transport::ruler(ui, ruler, (0.0, dur), self.info.fps, self.show_frames, moments, true) {
            Some(crate::transport::Pinned::Go(t)) => self.seek(t),
            Some(crate::transport::Pinned::Remove(t)) => {
                crate::moments::remove_marker(&self.clip, t + self.span.0);
                self.moments = crate::moments::of(&self.clip);
            }
            None => {}
        }
        painter.rect_filled(video, 5, p.raise2);
        self.strip.paint(ui, video, 0.0, dur, dur);
        painter.rect_filled(audio, 5, p.raise);
        if self.wave.is_silent_file() {
            painter.text(audio.center(), Align2::CENTER_CENTER, "No sound", crate::theme::font(12.0, crate::theme::W::Regular), p.text3);
        } else {
            // The sound as the app hears it: the loudest peaks as a lighter
            // outline, how loud it sounds as the solid shape inside.
            let wave_hi = p.wave_hi;
            self.wave.paint(&ui.painter_at(audio.shrink2(Vec2::new(0.0, 2.0))), audio.shrink2(Vec2::new(0.0, 2.0)), (0.0, dur), u64::from(p.dark), |_| 1.0, |_, _| wave_hi);
        }
        // What's left to play, dimmed: where you are reads at a glance.
        let xp = x_of(self.info.snap(self.player.time()));
        let dim = Color32::from_rgba_unmultiplied(10, 11, 14, if p.dark { 115 } else { 0 });
        let dim = if p.dark { dim } else { Color32::from_rgba_unmultiplied(248, 248, 249, 128) };
        let rest = Rect::from_min_max(Pos2::new(xp, video.top()), Pos2::new(outer.right(), audio.bottom()));
        painter.rect_filled(rest.intersect(video), egui::CornerRadius { nw: 0, sw: 0, ne: 5, se: 5 }, dim);
        painter.rect_filled(rest.intersect(audio), egui::CornerRadius { nw: 0, sw: 0, ne: 5, se: 5 }, dim);

        // Hover: a line, and that moment's picture and time.
        if let Some(pos) = resp.hover_pos().filter(|_| self.dragging.is_none() && pos_in(video.union(audio), resp.hover_pos())) {
            let t = self.info.snap(t_of(pos.x));
            self.hover_preview(ui, t, pos.x, video, preview);
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        } else {
            self.hover = None;
        }

        // Playhead.
        crate::transport::playhead(&painter, xp, ruler.bottom() - 4.0, audio.bottom(), p.text);

        // Click or drag to go there. Playing pauses while you drag and carries on after.
        if resp.drag_started() {
            self.dragging = Some(self.player.is_playing());
            self.player.pause();
        }
        if let Some(pos) = resp.interact_pointer_pos() {
            if (resp.dragged() || resp.drag_started() || resp.clicked()) && pos.y > ruler.bottom() - 6.0 {
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
        let w = 200.0f32.min(preview.width() * 0.35);
        let size = Vec2::new(w, w / aspect);
        let left = (x - size.x / 2.0).clamp(timeline.left(), timeline.right() - size.x);
        let frame = Rect::from_min_size(Pos2::new(left, timeline.top() - 40.0 - size.y), size);
        let p = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("viewer_hover")));
        let shadow = egui::Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(130) };
        p.add(shadow.as_shape(frame, 5));
        p.rect_filled(frame, 5, Color32::BLACK);
        if let Some(tex) = tex {
            p.add(egui::Shape::Rect(egui::epaint::RectShape::filled(frame, 5, Color32::WHITE).with_texture(tex.id(), Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)))));
        }
        p.rect_stroke(frame, 5, Stroke::new(1.5, Color32::WHITE), egui::StrokeKind::Inside);
        let g = p.layout_no_wrap(if self.show_frames { format!("{}", self.info.frame_index(t)) } else { crate::transport::clock(t) }, crate::theme::font(11.5, crate::theme::W::Semibold), Color32::WHITE);
        let label = Rect::from_center_size(Pos2::new(frame.center().x, frame.bottom() + 14.0), g.size() + Vec2::new(12.0, 4.0));
        p.rect_filled(label, 4, Color32::from_black_alpha(184));
        p.galley(label.min + Vec2::new(6.0, 2.0), g, Color32::WHITE);
    }

    fn keyboard(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() || crate::ui_kit::overlay_open(ctx) {
            return;
        }
        let none = egui::Modifiers::NONE;
        // < and >: slower and faster.
        let (slower, faster) = ctx.input_mut(|i| (i.consume_key(egui::Modifiers::SHIFT, Key::Comma), i.consume_key(egui::Modifiers::SHIFT, Key::Period)));
        if slower || faster {
            self.step_speed(ctx, faster);
        }
        // [ and ]: the previous or next moment. B: a marker here.
        let (back, ahead, mark) = ctx.input_mut(|i| (i.consume_key(none, Key::OpenBracket), i.consume_key(none, Key::CloseBracket), i.consume_key(none, Key::B)));
        if back || ahead {
            let t = self.player.time();
            let pins = self.pins();
            let to = if ahead { pins.iter().find(|m| m.t > t + 0.05) } else { pins.iter().rev().find(|m| m.t < t - 0.05) };
            if let Some(m) = to {
                self.seek(m.t);
            }
        }
        if mark {
            self.toggle_marker();
        }
        let (space, k, left, right, j, l, mut comma, mut period, home, end, m, mut held) = ctx.input_mut(|i| {
            // A held key's repeats: looked at before the presses are taken.
            let repeat = i.events.iter().any(|e| matches!(e, egui::Event::Key { key: Key::Comma, pressed: true, repeat: true, .. }));
            let held = i.key_down(Key::Comma) && (self.stepping_back || repeat);
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
                held,
            )
        });
        let mut steps = u32::from(comma || period);
        if let Some((n, key, repeating)) = dev_hold() {
            steps += n;
            comma |= n > 0 && key == Key::Comma;
            period |= n > 0 && key == Key::Period;
            held |= repeating && key == Key::Comma;
            ctx.request_repaint();
        }
        self.stepping_back = held;
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
            self.seek(self.info.snap(now) + steps as f64 * if period { fd } else { -fd });
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

fn pos_in(rect: Rect, pos: Option<Pos2>) -> bool {
    pos.is_some_and(|p| rect.contains(p))
}

fn is_fullscreen(ctx: &egui::Context) -> bool {
    ctx.input(|i| i.viewport().fullscreen.unwrap_or(false))
}

fn set_fullscreen(ctx: &egui::Context, on: bool) {
    if is_fullscreen(ctx) != on {
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(on));
    }
}

/// Dev aid: `HASTECLIP_DEV_WHEEL=3.0:-40,3.3:-40` scrolls by those points at
/// those times (seconds since start), as wheel notches would, to reproduce
/// wheel scrubbing without touching the mouse.
fn dev_wheel() -> f32 {
    static PLAN: std::sync::OnceLock<std::sync::Mutex<Vec<(f64, f32)>>> = std::sync::OnceLock::new();
    let plan = PLAN.get_or_init(|| {
        let spec = std::env::var("HASTECLIP_DEV_WHEEL").unwrap_or_default();
        let steps = spec.split(',').filter_map(|s| s.split_once(':')).filter_map(|(t, d)| Some((t.trim().parse().ok()?, d.trim().parse().ok()?))).collect();
        std::sync::Mutex::new(steps)
    });
    let now = crate::player::uptime();
    let mut plan = plan.lock().unwrap();
    let due: f32 = plan.iter().filter(|(t, _)| *t <= now).map(|(_, d)| *d).sum();
    plan.retain(|(t, _)| *t > now);
    due
}

/// Dev aid: `HASTECLIP_DEV_DRAG=4-6:10-20,8-9:30-25` drags the playhead
/// from 10 s to 20 s of the clip between 4 and 6 s after start, then from
/// 30 s back to 25 s, as the mouse would, to reproduce drag scrubbing
/// without touching the mouse. `Some(Some(t))`: dragging to `t` now;
/// `Some(None)`: a drag just ended.
fn dev_drag() -> Option<Option<f64>> {
    type Plan = (Vec<[f64; 4]>, bool);
    static PLAN: std::sync::OnceLock<std::sync::Mutex<Plan>> = std::sync::OnceLock::new();
    let plan = PLAN.get_or_init(|| {
        let spec = std::env::var("HASTECLIP_DEV_DRAG").unwrap_or_default();
        let range = |s: &str| -> Option<(f64, f64)> {
            let (a, b) = s.split_once('-')?;
            Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
        };
        let steps = spec
            .split(',')
            .filter_map(|s| s.split_once(':'))
            .filter_map(|(when, to)| {
                let ((t0, t1), (a, b)) = (range(when)?, range(to)?);
                Some([t0, t1, a, b])
            })
            .collect();
        std::sync::Mutex::new((steps, false))
    });
    let mut plan = plan.lock().unwrap();
    if plan.0.is_empty() && !plan.1 {
        return None;
    }
    let now = crate::player::uptime();
    if let Some(&[t0, t1, a, b]) = plan.0.iter().find(|s| s[0] <= now && now < s[1]) {
        plan.1 = true;
        return Some(Some(a + (b - a) * (now - t0) / (t1 - t0).max(1e-3)));
    }
    plan.0.retain(|s| s[1] > now);
    if std::mem::take(&mut plan.1) {
        return Some(None);
    }
    None
}

/// Dev aid: `HASTECLIP_DEV_HOLD=4-8:,` holds `,` (or `.`) down from 4 to 8 s
/// after start, as a held key does on Windows (a press, then repeats 30
/// times a second after half a second), to reproduce frame stepping without
/// touching the keyboard. Presses due now, the key, and whether they repeat.
fn dev_hold() -> Option<(u32, Key, bool)> {
    static PLAN: std::sync::OnceLock<std::sync::Mutex<(Vec<(f64, f64, Key)>, u32)>> = std::sync::OnceLock::new();
    let plan = PLAN.get_or_init(|| {
        let spec = std::env::var("HASTECLIP_DEV_HOLD").unwrap_or_default();
        let holds = spec
            .split(';')
            .filter_map(|s| s.split_once(':'))
            .filter_map(|(when, key)| {
                let (a, b) = when.split_once('-')?;
                let key = if key.trim() == "." { Key::Period } else { Key::Comma };
                Some((a.trim().parse().ok()?, b.trim().parse().ok()?, key))
            })
            .collect();
        std::sync::Mutex::new((holds, 0))
    });
    let mut plan = plan.lock().unwrap();
    let now = crate::player::uptime();
    let Some(&(t0, _, key)) = plan.0.iter().find(|h| h.0 <= now && now < h.1) else {
        plan.1 = 0;
        return None;
    };
    let since = now - t0;
    let due = 1 + if since < 0.5 { 0 } else { ((since - 0.5) * 30.0) as u32 + 1 };
    let new = due - plan.1.min(due);
    plan.1 = due;
    Some((new, key, since >= 0.5))
}

/// The pointer is over `rect`, with no dialog or menu on top of it.
fn pointer_over(ctx: &egui::Context, rect: Rect) -> bool {
    !crate::ui_kit::overlay_open(ctx) && ctx.pointer_hover_pos().is_some_and(|p| rect.contains(p))
}

/// The player's volume and mute, as last set (shared by every clip opened).
pub fn volume(ctx: &egui::Context) -> Option<(f32, bool)> {
    ctx.data(|d| d.get_temp::<(f32, bool)>(volume_id()))
}

pub fn set_volume(ctx: &egui::Context, volume: (f32, bool)) {
    ctx.data_mut(|d| d.insert_temp(volume_id(), volume));
}

fn volume_id() -> egui::Id {
    egui::Id::new("viewer_volume")
}

/// Where the clip at `path` starts in its recording, and how long that
/// recording is: `(0, duration)` unless it's been trimmed.
fn recording_span(path: &Path, duration: f64) -> (f64, f64) {
    let target = crate::store::EditTarget::of(path);
    if target.source == target.clip {
        return (0.0, duration);
    }
    let Some(edit) = crate::store::load_edit(&target) else { return (0.0, duration) };
    let whole = media::probe(&target.source).map_or(edit.end, |i| i.duration);
    (edit.start, whole)
}

/// The speeds offered.
const SPEEDS: [f32; 7] = [0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 2.0];

/// The speed last chosen (shared by every clip opened).
fn speed(ctx: &egui::Context) -> f32 {
    ctx.data(|d| d.get_temp::<f32>(speed_id())).unwrap_or(1.0)
}

fn speed_id() -> egui::Id {
    egui::Id::new("viewer_speed")
}

/// "0.25×", "1×", "1.5×".
fn speed_label(s: f32) -> String {
    format!("{}×", (s * 100.0).round() / 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speeds_read_plainly() {
        let labels: Vec<String> = SPEEDS.iter().map(|s| speed_label(*s)).collect();
        assert_eq!(labels, ["0.25×", "0.5×", "0.75×", "1×", "1.25×", "1.5×", "2×"]);
    }
}
