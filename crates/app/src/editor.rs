//! The clip editor: frame-exact trim and a per-track audio mix with live meters.
//!
//! Layout, top to bottom: the preview; transport (play, frame step, frame
//! counter); a Premiere-style timeline where a frame ruler, the video lane and
//! one lane per audio track share a time axis, with volume edited directly on
//! each audio lane; the master meter for the mix that becomes track 1.
//!
//! Edits are non-destructive (see the `media` crate): Done saves the sidecar and
//! renders the edited file in the background; the original is never touched.

use crate::ui_kit::Dismissed as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};

use egui::{Align2, Color32, FontId, Key, Pos2, Rect, Sense, Stroke, Vec2};
use media::{ClipInfo, Edit};

use crate::filmstrip::Filmstrip;
use crate::handoff::{Handoff, Kept};
use crate::player::Player;
use crate::store::{self, EditTarget};
use crate::viewer::Resume;

/// Timeline geometry, in points.
const RULER_H: f32 = crate::transport::RULER_H;
const VIDEO_H: f32 = 56.0;
const AUDIO_H: f32 = 52.0;
/// The line under the lanes that says how long the kept part is.
const LENGTH_H: f32 = 28.0;
const DIVIDER_H: f32 = 9.0;
/// The lane headers' width.
/// The mix meter's column, on the right of the timeline.
const MIX_W: f32 = 56.0;
const HEADER_W: f32 = 150.0;
const MIN_PREVIEW_H: f32 = 120.0;
/// Deepest timeline zoom: this many frames across.
const MIN_VIEW_FRAMES: f64 = 24.0;
/// Gain slider range in dB.
/// Meter scale floor in dB.
const METER_FLOOR: f32 = -60.0;
/// Meter colours: healthy / hot / clipping.


/// What the editor asks the app to do after a frame.
pub enum EditorOutcome {
    Stay,
    Close,
    /// Saved: render `edit` of the clip in the background — as this clip's edit,
    /// or (`new_name`) as a separate new clip, leaving this clip as it was.
    /// `then`: free up the original's space once it's saved.
    Saved { target: EditTarget, info: ClipInfo, edit: Edit, new_name: Option<String>, then: Option<store::FreeUp> },
    /// Free up the space the (saved) edit's original takes.
    FreeUp(EditTarget, store::FreeUp),
    Reverted(EditTarget),
    /// Rename the clip (the edit stays open).
    Rename,
    /// Show the keyboard shortcuts.
    Keys,
}

struct Loaded {
    info: ClipInfo,
    /// Every source track, filling in as it's decoded.
    pcm: Arc<media::pcm::Pcm>,
}

/// Read the clip and start decoding every source track, but for what's
/// `kept`. Every track at once, playable as it's decoded; the waveforms
/// follow once it's all there.
fn load(source: &Path, kept: &mut Kept) -> Result<Loaded, String> {
    let info = kept.probe(source).map_err(|e| e.to_string())?;
    let tracks: Vec<usize> = info.source_tracks().iter().map(|t| t.index).collect();
    if crate::player::debug() {
        eprintln!("{:>8.3} editor: clip read, decoding {} tracks", crate::player::uptime(), tracks.len());
    }
    let pcm = kept.sound(source, &tracks, || {
        media::pcm::decode_streaming(source, &tracks, info.duration, |_, result| {
            if let Err(e) = result {
                eprintln!("decoding the sound: {e:#}");
            }
            if crate::player::debug() {
                eprintln!("{:>8.3} editor: sound all decoded", crate::player::uptime());
            }
        })
    });
    Ok(Loaded { info, pcm })
}

enum State {
    /// Being read (what's kept comes back with it).
    Loading(Receiver<(Result<Loaded, String>, Kept)>),
    Failed(String),
    Ready(Box<Ready>),
}

struct Ready {
    info: ClipInfo,
    player: Player,
    /// The last screen's player, hushed: its picture shows until this one
    /// has one (see `handoff.rs`).
    curtain: Option<Player>,
    edit: Edit,
    /// What was last saved, to know whether there are unsaved changes.
    saved: Edit,
    /// Per source track, growing as the sound is decoded.
    waves: Vec<Arc<crate::waveform::Live>>,
    strip: Filmstrip,
    /// Smoothed meter values (peak, rms) per track, then master; and peak holds.
    meters: Vec<Meter>,
    master: Meter,
    /// How far short the sound lanes came last frame, for the split between
    /// picture and timeline (whatever the guess of the other parts' height
    /// missed).
    lanes_short: f32,
    /// The kept length being typed (click the length under the timeline).
    typing_length: Option<String>,
    /// The mix's meter on the right of the timeline (as on Sources).
    mix_meter: crate::meter::MeterState,
    dragging: Option<Drag>,
    /// Was playing when a playhead or trim drag began: carry on after it.
    resume: bool,
    /// While the selection is dragged: how far into it it was grabbed.
    grab_offset: f64,
    /// The timeline's pixels per second, as last drawn.
    px_per_s: f64,
    /// Where the timeline was laid out last frame: what the pointer does on
    /// it is read there, first thing in the next (see [`Ready::timeline_input`]).
    timeline_at: Option<Rect>,
    /// The dragged playhead sits on a snap target (an edge, a marker, an
    /// end): it stays there, its own frame fetched.
    playhead_snapped: bool,
    /// How far the audio tracks are scrolled up when they don't all fit.
    lane_scroll: f32,
    /// Wheel scrolling and Ctrl+wheel zoom, eased out over a few frames.
    glide: crate::wheel::Glide,
    zoom_glide: crate::wheel::Glide,
    /// The part of the clip the timeline shows: start and length, in seconds.
    /// The whole clip until you zoom in.
    view: (f64, f64),
    confirm_discard: bool,
    /// The "Free up space" dialog: the original's size in bytes.
    free_up: Option<u64>,
    /// "Save as new clip" dialog: the name being typed, and any problem with it.
    save_as: Option<(String, Option<String>)>,
    /// F2 was pressed.
    rename_requested: bool,
    /// The export settings were opened by the dev hook.
    export_shown: bool,
    /// The export settings popup is open.
    export_open: bool,
    /// Earlier edits to step back to (Cmd Z), and ones undone (Shift Cmd Z).
    undo: Vec<Edit>,
    redo: Vec<Edit>,
    /// The edit when a drag began, to put on the undo stack once it ends.
    before_drag: Option<Edit>,
    /// A volume drag: how far it's gone (Shift's fine steps counted small),
    /// where along the clip its line was taken hold of (seconds), and the
    /// level it's snapped to (lane, dB), which a guide marks.
    vol_moved: Vec2,
    vol_grab: f64,
    vol_guide: Option<(usize, f32)>,
    /// Trim edges and the playhead snap to each other and the clip's ends.
    snap: bool,
    /// Playing loops the kept part.
    looping: bool,
    /// The one track heard (for listening, not saved).
    solo: Option<usize>,
    /// Times shown as frames.
    show_frames: bool,
    /// The view the timeline is easing to, after a zoom by button or slider.
    view_target: Option<(f64, f64)>,
    was_playing: bool,
    /// "Revert to the original?" is asked.
    confirm_revert: bool,
    /// The Save as new name was selected for typing over.
    save_as_focused: bool,
    /// The clip's kills and your markers, as pins (the editor shows the
    /// whole recording, which they're kept for).
    moments: Vec<crate::transport::Moment>,
    /// Cropping the picture (C): the whole of it shows, to choose the part kept.
    crop_tool: Option<crate::crop_ui::CropTool>,
    /// What the timeline's right-click menu is for, and the time clicked,
    /// while it's open (a part right-clicked is lit meanwhile).
    menu_at: Option<(Spot, f64)>,
    /// A stretch just cut out (or put back): its dimming fades in (or out).
    fading: Option<Fading>,
}

/// A place on the timeline, for its right-click menu.
#[derive(Clone, Copy, PartialEq)]
enum Spot {
    /// Part `k` of what's kept.
    Part(usize),
    /// What's cut out between part `k` and the next.
    Gap(usize),
    /// The split between part `k` and the next (nothing cut out).
    Split(usize),
    /// What's trimmed off the start, or the end.
    Before,
    After,
    /// Track `i`'s volume line, and its point `k`.
    Volume(usize),
    Point(usize, usize),
}

/// A stretch whose dimming changes: `from` to `to` seconds, cut out (`cut`)
/// or put back, at `since` (the UI's clock).
#[derive(Clone, Copy)]
struct Fading {
    from: f64,
    to: f64,
    cut: bool,
    since: f64,
}

/// How long a stretch's dimming takes to fade in or out (seconds).
const FADE_S: f64 = 0.18;

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
    /// Snapping, as the app keeps it (settings), until the clip has loaded.
    snap: bool,
    /// Where the player was when the editor was opened from it (the clip
    /// preview), to start the timeline there.
    from: Option<Resume>,
    /// What the player handed over, until the clip has loaded; then what's
    /// kept, to hand back.
    handoff: Handoff,
}

impl Editor {
    /// Edit the clip at `clip`. An edited clip is decoded from its original, so
    /// every earlier decision can still be changed. Opened from the player:
    /// `from` where it was, with what it handed over.
    pub fn open(ctx: &egui::Context, clip: &Path, preview_share: f32, snap: bool, from: Option<Resume>, mut handoff: Handoff) -> Self {
        let target = EditTarget::of(clip);
        let state = if handoff.kept.info(&target.source).is_some() {
            match load(&target.source, &mut handoff.kept) {
                Ok(l) => State::Ready(Box::new(Ready::new(ctx, &target, l, snap, from, &mut handoff))),
                Err(e) => {
                    handoff.let_go();
                    State::Failed(e)
                }
            }
        } else {
            let (tx, rx) = mpsc::channel();
            let src = target.source.clone();
            let repaint = ctx.clone();
            let mut kept = std::mem::take(&mut handoff.kept);
            std::thread::spawn(move || {
                let result = load(&src, &mut kept);
                let _ = tx.send((result, kept));
                repaint.request_repaint();
            });
            State::Loading(rx)
        };
        Self { target, state, preview_share, snap, from, handoff }
    }

    /// Going back to the player: what it takes over (see `handoff.rs`).
    pub fn into_handoff(self) -> Handoff {
        let mut handoff = self.handoff;
        let State::Ready(r) = self.state else {
            handoff.let_go();
            return handoff;
        };
        // The saved clip is cropped, and this plays its original: as the
        // player's curtain, the whole picture would show for a moment.
        let cropped = r.saved_crop().is_some();
        let player = (!cropped).then_some(r.player);
        Handoff::from_screen(player, r.curtain, r.strip, handoff.kept)
    }

    /// The clip was renamed from `from` to `to`: saved under the new name.
    /// A clip never edited is its own original, so it's read from there too
    /// (the edit in progress is kept).
    pub fn renamed(&mut self, ctx: &egui::Context, from: &Path, to: &Path) {
        if self.target.clip != from {
            return;
        }
        self.target.clip = to.to_path_buf();
        self.handoff.kept.renamed(from, to);
        if self.target.source != from {
            return;
        }
        self.target.source = to.to_path_buf();
        match &mut self.state {
            // Everything open keeps going (sound, picture, waveforms, the
            // edit); only reopening by name follows.
            State::Ready(r) => {
                r.player.renamed(ctx, to);
                r.strip.renamed(to);
            }
            _ => *self = Self::open(ctx, to, self.preview_share, self.snap(), self.from, Handoff::default()),
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

    /// Snapping, as it's set now (the magnet, N), to keep for next time.
    pub fn snap(&self) -> bool {
        match &self.state {
            State::Ready(r) => r.snap,
            _ => self.snap,
        }
    }

    /// Where the playhead is, in what's kept, and whether it's playing: for
    /// the player to carry on there, back from the editor.
    pub fn resume(&self) -> Option<Resume> {
        let State::Ready(r) = &self.state else { return None };
        Some(Resume {
            at: settle(&r.edit.kept(), r.player.time(), r.info.frame_duration()),
            playing: r.player.is_playing(),
        })
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) -> EditorOutcome {
        if let State::Loading(rx) = &self.state {
            if let Ok((result, kept)) = rx.try_recv() {
                self.handoff.kept = kept;
                self.state = match result {
                    Ok(l) => State::Ready(Box::new(Ready::new(ui.ctx(), &self.target, l, self.snap, self.from, &mut self.handoff))),
                    Err(e) => {
                        self.handoff.let_go();
                        State::Failed(e)
                    }
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
    fn new(ctx: &egui::Context, target: &EditTarget, l: Loaded, snap: bool, from: Option<Resume>, handoff: &mut Handoff) -> Self {
        let source = &target.source;
        let mut edit = store::load_edit(target).unwrap_or_else(|| Edit::new(&l.info));
        // Sidecar from an older layout or a hand-edited file: make it fit.
        if edit.tracks.len() != l.pcm.len() {
            edit.tracks = Edit::new(&l.info).tracks;
        }
        edit.end = edit.end.min(l.info.duration);
        let gains = edit.tracks.clone();
        let n = l.pcm.len();
        let waves = (0..n).map(|i| crate::waveform::follow(l.pcm.clone(), i, ctx)).collect();
        let playhead = from.map_or(edit.start, |r| settle(&edit.kept(), r.at, l.info.frame_duration()));
        let (mut player, curtain, taken) = Player::handed_over(ctx, source, &l.info, l.pcm.clone(), gains, handoff.player.take());
        if !taken {
            player.seek(playhead);
        }
        if from.is_some_and(|r| r.playing) {
            player.set_cuts(&edit.kept());
            player.set_listening(crate::transport::listening_gain(ctx));
            player.play_on(edit.end);
        }

        let full = l.info.duration.max(1e-6);
        let strip = handoff.strip(source).unwrap_or_else(|| Filmstrip::build(ctx, source, &l.info));
        Self {
            info: l.info,
            player,
            curtain,
            saved: edit.clone(),
            edit,
            waves,
            strip,
            meters: vec![Meter::default(); n],
            master: Meter::default(),
            mix_meter: Default::default(),
            typing_length: None,
            lanes_short: 0.0,
            dragging: None,
            resume: false,
            grab_offset: 0.0,
            px_per_s: 0.0,
            timeline_at: None,
            playhead_snapped: false,
            lane_scroll: 0.0,
            glide: Default::default(),
            zoom_glide: Default::default(),
            view: (0.0, full),
            confirm_discard: false,
            free_up: None,
            save_as: None,
            rename_requested: false,
            export_shown: false,
            export_open: false,
            undo: Vec::new(),
            redo: Vec::new(),
            before_drag: None,
            vol_moved: Vec2::ZERO,
            vol_grab: 0.0,
            vol_guide: None,
            snap,
            looping: false,
            solo: None,
            show_frames: false,
            view_target: None,
            was_playing: false,
            confirm_revert: false,
            save_as_focused: false,
            moments: crate::moments::of(&target.clip).pins(full),
            crop_tool: None,
            menu_at: None,
            fading: None,
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, target: &EditTarget, preview_share: &mut f32) -> EditorOutcome {
        // The clip as named in the library; decoding uses `target.source`.
        let source = target.clip.as_path();
        let ctx = ui.ctx().clone();
        self.keyboard(&ctx);
        // A click or drag on the timeline before anything that shows the
        // playhead or the edit: the picture, the time, the playhead and the
        // parts' edges follow the pointer on the frame it moves, as in the
        // player. (Read after them, every frame of a drag showed where the
        // move before had put them.) The timeline is where it was laid out
        // last frame, as egui finds what's under the pointer.
        let timeline = self.timeline_at.take().map(|outer| {
            let l = self.layout(outer);
            let resp = ui.interact(l.hit_area, ui.id().with("timeline"), Sense::click_and_drag());
            self.timeline_input(ui, &l, &resp);
            resp
        });
        // What's heard: the edit, with only the soloed track if there is one.
        let mut mix = self.edit.tracks.clone();
        if let Some(solo) = self.solo {
            for (i, t) in mix.iter_mut().enumerate() {
                t.muted |= i != solo;
            }
        }
        self.player.set_mix(mix);
        // What's cut out, skipped while playing.
        self.player.set_cuts(&self.edit.kept());
        // As loud as you listen, in the player too (they share it): the
        // edit's own levels stay as they are.
        self.player.set_listening(crate::transport::listening_gain(&ctx));
        // Looping: back to the start at the end.
        if self.looping && !self.player.is_playing() && self.was_playing && self.player.time() >= self.edit.end - self.info.frame_duration() * 1.5 {
            self.player.seek(self.edit.start);
            self.player.play(self.edit.end);
        }
        self.was_playing = self.player.is_playing();
        self.ease_view(&ctx);

        let dt = ctx.input(|i| i.stable_dt).min(0.1);
        let (track_levels, master_level) = self.player.levels();
        let playing = self.player.is_playing();
        for (m, l) in self.meters.iter_mut().zip(track_levels.iter().chain(std::iter::repeat(&(0.0, 0.0)))) {
            m.feed(if playing { *l } else { (0.0, 0.0) }, dt);
        }
        self.master.feed(if playing { master_level } else { (0.0, 0.0) }, dt);
        let stereo = if playing { self.player.master_stereo() } else { Default::default() };
        self.mix_meter.update(stereo, std::time::Instant::now(), dt);
        // Keep animating until the meters have fallen back to rest: about 60
        // times a second is smooth (every refresh of a fast display redrew
        // the whole editor for nothing).
        if self.meters.iter().chain([&self.master]).any(|m| m.peak > 1e-3 || m.hold > 1e-3) || playing {
            crate::ui_kit::repaint_after(&ctx, std::time::Duration::from_millis(16));
        }

        let mut outcome = EditorOutcome::Stay;
        if std::mem::take(&mut self.rename_requested) {
            outcome = EditorOutcome::Rename;
        }

        // --- Header: back and the name; export, ⋯, Cancel, Save ---
        let stem = source.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let title = crate::clips::title_for_stem(&stem);
        let dirty = self.dirty();
        let changed = !self.edit.is_identity(&self.info);
        let has_saved_edit = target.source != target.clip;
        let mut cancel = false;
        let mut save = false;
        let mut save_new = false;
        let mut revert = false;
        let mut free_up = false;
        let mut keys = false;
        let (back, rename) = crate::transport::header(ui, &title, dirty, 0.0, |ui, part| {
            use crate::ui_kit::{Btn, Item, Kind};
            use egui_phosphor::regular as icon;
            if part != crate::transport::Part::Right {
                return;
            }
            // Narrower windows: the export button shows just its icon, and
            // Cancel goes (back and Esc cancel too).
            let narrow = ui.ctx().content_rect().width() < 820.0;
            // Save, with Save as new in its menu.
            let corners_l = egui::CornerRadius { nw: 6, sw: 6, ne: 0, se: 0 };
            let corners_r = egui::CornerRadius { nw: 0, sw: 0, ne: 6, se: 6 };
            ui.spacing_mut().item_spacing.x = 1.0;
            let more = Btn::icon(Kind::Primary, icon::CARET_DOWN).corners(corners_r).tip("More ways to save").show(ui);
            crate::ui_kit::menu_popup(&more, false, 240.0, |ui| {
                if Item::new("Save").icon(icon::CHECK).keys(Some(crate::shortcuts::command("S"))).show(ui).clicked() {
                    save = true;
                }
                let item = Item::new("Save as new clip…").icon(icon::COPY).enabled(changed);
                let item = if changed { item.tip("Keep this clip as it is and save the edit as a separate clip") } else { item.tip("Trim, change the sound or the export settings first") };
                if item.show(ui).clicked() {
                    save_new = true;
                }
            });
            if Btn::primary("Save").corners(corners_l).min_width(64.0).tip("Save the edit to this clip. The original recording is kept, so you can change it later.").show(ui).clicked() {
                save = true;
            }
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.add_space(4.0);
            if !narrow && Btn::ghost("Cancel").show(ui).clicked() {
                cancel = true;
            }
            crate::transport::vrule(ui);
            let more = Btn::icon(Kind::Ghost, icon::DOTS_THREE).tip("More").show(ui);
            crate::ui_kit::menu_popup(&more, false, 240.0, |ui| {
                let item = Item::new("Revert to the original").icon(icon::ARROW_COUNTER_CLOCKWISE).enabled(has_saved_edit || changed).tip("Undo every edit and go back to the recording");
                if item.show(ui).clicked() {
                    revert = true;
                }
                if Item::new("Free up space…").icon(icon::HARD_DRIVE).enabled(changed).tip("Trim or delete the original recording this edit keeps").show(ui).clicked() {
                    free_up = true;
                }
                crate::ui_kit::menu_sep(ui);
                if Item::new("Keyboard shortcuts").icon(icon::KEYBOARD).keys(Some(vec!["?".into()])).show(ui).clicked() {
                    keys = true;
                }
            });
            // How the saved file is made: applies to Save and Save as new.
            let summary = crate::export_ui::summary(&self.info, &self.edit);
            let export = if narrow {
                Btn::icon(Kind::Ghost, icon::SLIDERS_HORIZONTAL).id("export_btn").tip(format!("Export settings: {summary}")).show(ui)
            } else {
                Btn::ghost(summary).with_icon(icon::SLIDERS_HORIZONTAL).trail(icon::CARET_DOWN).id("export_btn").tip("Export settings: a crop, resolution, frame rate, quality, a size to fit under, sound tracks").show(ui)
            };
            if export.clicked() {
                self.export_open = !self.export_open;
            }
            // Dev aid: `HASTECLIP_OPEN_EXPORT=1` opens it at launch.
            if std::env::var_os("HASTECLIP_OPEN_EXPORT").is_some() && !self.export_shown {
                self.export_shown = true;
                self.export_open = true;
            }
            // Its dropdowns open popups of their own, and egui remembers only
            // one open popup: left to egui, opening a dropdown closed this. So
            // it's kept open here, and a click closes it only outside it, and
            // not when it was for an open dropdown.
            let dropdown_open = egui::Popup::is_any_open(ui.ctx());
            let before = self.edit.output;
            let mut crop_on_picture = false;
            let shown = egui::Popup::from_response(&export)
                .open_bool(&mut self.export_open)
                .close_behavior(egui::PopupCloseBehavior::IgnoreClicks)
                .align(egui::RectAlign::BOTTOM_END)
                .gap(6.0)
                .frame(crate::ui_kit::pop_frame(ui).inner_margin(egui::Margin::same(14)))
                .show(|ui| crate::export_ui::ui(ui, &self.info, &mut self.edit, &mut crop_on_picture));
            if shown.is_some_and(|r| r.response.clicked_elsewhere()) && !dropdown_open && !export.clicked() {
                self.export_open = false;
            }
            if crop_on_picture {
                self.export_open = false;
                self.crop_tool = Some(crate::crop_ui::CropTool::new(self.crop()));
            }
            if self.edit.output != before {
                let mut was = self.edit.clone();
                was.output = before;
                self.push_undo(was);
            }
        });
        if keys || ctx.input_mut(|i| i.consume_key(egui::Modifiers::SHIFT, Key::Slash)) && !crate::ui_kit::overlay_open(&ctx) {
            outcome = EditorOutcome::Keys;
        }
        if rename {
            outcome = EditorOutcome::Rename;
        }
        if back || cancel {
            if dirty {
                self.confirm_discard = true;
            } else {
                outcome = EditorOutcome::Close;
            }
        }
        if save || (ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, Key::S)) && !crate::ui_kit::overlay_open(&ctx)) {
            self.player.pause();
            outcome = self.save(target, None);
        }
        if save_new {
            self.player.pause();
            let base = source.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let suggested = crate::clips::sanitize_name(&format!("{} (edit)", crate::clips::title_for_stem(&base)));
            self.save_as = Some((suggested, None));
        }
        if revert {
            self.player.pause();
            self.confirm_revert = true;
        }
        if free_up {
            self.player.pause();
            let original = if has_saved_edit { &target.source } else { &target.clip };
            self.free_up = Some(std::fs::metadata(original).map(|m| m.len()).unwrap_or(0));
        }

        // --- Picture: what's left over after the transport and timeline ---
        // The divider under it sets the split; the timeline always keeps room
        // for the ruler, the video lane and one sound lane (the rest scroll).
        let total = ui.available_height();
        let below_min = DIVIDER_H + crate::transport::BAR_H + RULER_H + VIDEO_H + 4.0 + AUDIO_H + LENGTH_H + 10.0;
        // Never more timeline than its lanes need: the picture gets the rest.
        // (The same sum the timeline makes: its ruler, video lane, the gap
        // above the sound lanes, each lane and the gap after it, the length.)
        let below_max = DIVIDER_H + crate::transport::BAR_H + RULER_H + VIDEO_H + 4.0 + 4.0 + self.edit.tracks.len() as f32 * (AUDIO_H + 4.0) + LENGTH_H + 10.0 + self.lanes_short;
        // The divider's share, except that every sound lane shows (no
        // scrolling) whenever the picture can still have a third of the room.
        let preview_h = (total * *preview_share).min((total - below_max).max(total / 3.0)).max(total - below_max).min(total - below_min).max(MIN_PREVIEW_H);
        let (preview_rect, preview_resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), preview_h), Sense::click());
        // Dragging the playhead: held still, the picture stays, and letting
        // go lands on it, within a dozen pixels of the timeline. (A trim's
        // edge instead gets its frame: the edge is where it's put.)
        let reach = (12.0 / self.px_per_s.max(1e-9)).min(0.5);
        let free = matches!(self.dragging, Some(Drag::Playhead)) && !self.playhead_snapped;
        self.player.set_landing(free.then_some(reach));
        let size = self.player.update(&ctx, self.dragging.is_some());
        if size.is_some() {
            self.curtain = None;
        }
        let curtain = self.curtain.as_ref().filter(|_| size.is_none());
        let size = curtain.and_then(Player::size).or(size);
        let player = &self.player;
        let frame = (self.info.width, self.info.height);
        if let Some(tool) = &mut self.crop_tool {
            let before = self.edit.clone();
            let did = tool.ui(ui, preview_rect, size, frame, &mut self.edit.output.crop, |painter, r| match curtain {
                Some(c) => c.paint(painter, r),
                None => player.paint(painter, r),
            });
            if did.took_hold {
                self.before_drag = Some(before.clone());
            }
            if did.changed {
                self.push_undo(before);
            }
            if did.let_go
                && let Some(was) = self.before_drag.take()
                && !same_edit(&was, &self.edit)
            {
                self.push_undo(was);
            }
            if did.done {
                self.crop_tool = None;
            }
        } else {
            // Just the part kept, as the saved clip will be.
            let crop = self.crop().filter(|_| curtain.is_none());
            let shown = size.map(|s| crop.map_or(s, |c| crate::crop_ui::cropped_size(s, &c, frame)));
            let stage = egui::Id::new("editor_stage");
            let glow = |p: &egui::Painter, cover: Rect| match curtain {
                Some(c) => c.paint_glow(p, cover, stage),
                None => player.paint_glow(p, cover, stage),
            };
            let picture = crate::transport::stage(ui, preview_rect, shown, glow, |r| match (curtain, crop) {
                (Some(c), _) => c.paint(ui.painter(), r),
                (None, Some(c)) => crate::crop_ui::paint_cropped(ui, r, &c, frame, |painter, whole| player.paint(painter, whole)),
                (None, None) => player.paint(ui.painter(), r),
            });
            if let Some(picture) = picture.filter(|_| self.player.is_playing()) {
                crate::transport::muted_note(ui, picture);
            }
            if preview_resp.clicked() {
                self.toggle_play();
            }
        }
        divider(ui, preview_h, total, preview_share);

        // --- Transport ---
        let bar = Rect::from_min_size(ui.cursor().min, Vec2::new(ui.available_width(), crate::transport::BAR_H));
        ui.allocate_rect(bar, Sense::hover());
        self.transport(ui, bar);

        // --- Timeline: ruler, video and sound lanes on one time axis ---
        self.timeline(ui, timeline);
        if let Some(e) = &self.player.audio_error {
            ui.colored_label(crate::theme::pal(ui).warn, format!("No sound preview: {e}"));
        }

        if self.confirm_revert {
            let mut close = false;
            let modal = crate::ui_kit::dialog(&ctx, "revert_edit", 380.0, |ui| {
                crate::ui_kit::dialog_title(ui, "Revert to the original?");
                crate::ui_kit::dialog_text(ui, "Every edit to this clip is undone. The original recording comes back as it was.");
                crate::ui_kit::dialog_buttons(ui, |ui| {
                    if ui.add(crate::ui_kit::danger_button("Revert")).clicked() {
                        outcome = EditorOutcome::Reverted(target.clone());
                        close = true;
                    }
                    if ui.add(crate::ui_kit::button("Cancel", false).ghost()).clicked() {
                        close = true;
                    }
                });
            });
            if close || modal.dismissed() {
                self.confirm_revert = false;
            }
        }

        if let Some((name, err)) = &mut self.save_as {
            let mut close = false;
            let mut submit = false;
            let modal = crate::ui_kit::dialog(&ctx, "save_as_new", 400.0, |ui| {
                crate::ui_kit::dialog_title(ui, "Save as new clip");
                crate::ui_kit::dialog_text(ui, "This clip stays as it is. Your edit is saved as a separate clip:");
                ui.add_space(4.0);
                let out = crate::ui_kit::text_field(ui, name, "");
                // Open with the name selected and ready to type over.
                if !out.response.has_focus() && err.is_none() && ui.input(|i| !i.pointer.any_down()) && !self.save_as_focused {
                    out.response.request_focus();
                    let mut state = out.state.clone();
                    state.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(name.chars().count()))));
                    state.store(ui.ctx(), out.response.id);
                    self.save_as_focused = true;
                }
                submit = out.response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                if let Some(e) = err.as_ref() {
                    ui.add_space(4.0);
                    ui.label(crate::theme::text(e.as_str(), 12.5, crate::theme::W::Regular).color(crate::theme::pal(ui).danger));
                }
                crate::ui_kit::dialog_buttons(ui, |ui| {
                    if ui.add(crate::ui_kit::button("Save new clip", true)).clicked() {
                        submit = true;
                    }
                    if ui.add(crate::ui_kit::button("Cancel", false).ghost()).clicked() {
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
                        self.save_as_focused = false;
                        outcome = self.save(target, Some(name));
                    }
                    Err(e) => *err = Some(e),
                }
            } else if close || modal.dismissed() {
                self.save_as = None;
                self.save_as_focused = false;
            }
        }

        if let Some(bytes) = self.free_up {
            let needs_save = self.dirty() || target.source == target.clip;
            // Roughly what a trim keeps: the edit's share of the length.
            let kept = ((self.edit.end + 1.0).min(self.info.duration) - self.edit.start) / self.info.duration.max(1e-6);
            let trim_frees = (bytes as f64 * (1.0 - kept.clamp(0.0, 1.0))) as u64;
            let mut choice = None;
            let mut cancel = false;
            let modal = crate::ui_kit::dialog(&ctx, "free_up", 440.0, |ui| {
                let pal = crate::theme::pal(ui);
                crate::ui_kit::dialog_title(ui, "Free up space");
                crate::ui_kit::dialog_text(ui, &format!("The original recording ({}) is kept so you can change this edit later.", crate::export_ui::human_bytes(bytes)));
                if needs_save {
                    crate::ui_kit::hint(ui, "Your changes are saved first.");
                }
                ui.add_space(8.0);
                let option = |ui: &mut egui::Ui, title: &str, text: &str, button: crate::ui_kit::ButtonWidget| {
                    egui::Frame::new()
                        .fill(pal.raise2)
                        .corner_radius(8)
                        .inner_margin(egui::Margin::symmetric(12, 10))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                let w = ui.available_width() - 100.0;
                                ui.vertical(|ui| {
                                    ui.set_width(w);
                                    ui.label(crate::theme::text(title, 13.5, crate::theme::W::Semibold).color(pal.text));
                                    crate::ui_kit::hint(ui, text);
                                });
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| ui.add(button).clicked()).inner
                            })
                            .inner
                        })
                        .inner
                };
                if option(
                    ui,
                    "Trim the original",
                    &format!("Frees about {}. Keeps only the part this edit uses: you can still change the edit within it.", crate::export_ui::human_bytes(trim_frees)),
                    crate::ui_kit::button("Trim", false),
                ) {
                    choice = Some(store::FreeUp::Trim);
                }
                ui.add_space(8.0);
                if option(
                    ui,
                    "Delete the original",
                    &format!("Frees {}. The edit can't be changed or reverted afterwards.", crate::export_ui::human_bytes(bytes)),
                    crate::ui_kit::ButtonWidget(crate::ui_kit::Btn::new(crate::ui_kit::Kind::DangerText, "Delete")),
                ) {
                    choice = Some(store::FreeUp::Delete);
                }
                crate::ui_kit::dialog_buttons(ui, |ui| {
                    cancel = ui.add(crate::ui_kit::button("Cancel", false).ghost()).clicked();
                });
            });
            if modal.dismissed() || cancel {
                self.free_up = None;
            }
            if let Some(how) = choice {
                self.free_up = None;
                outcome = if needs_save {
                    match self.save(target, None) {
                        EditorOutcome::Saved { target, info, edit, new_name, .. } => EditorOutcome::Saved { target, info, edit, new_name, then: Some(how) },
                        other => other,
                    }
                } else {
                    EditorOutcome::FreeUp(target.clone(), how)
                };
            }
        }

        if self.confirm_discard {
            let modal = crate::ui_kit::dialog(&ctx, "discard_edit", 380.0, |ui| {
                crate::ui_kit::dialog_title(ui, "Discard changes?");
                crate::ui_kit::dialog_text(ui, "Your changes to this clip haven't been saved.");
                crate::ui_kit::dialog_buttons(ui, |ui| {
                    if ui.add(crate::ui_kit::button("Save", true)).clicked() {
                        self.player.pause();
                        outcome = self.save(target, None);
                        self.confirm_discard = false;
                    }
                    if ui.add(crate::ui_kit::button("Keep editing", false)).clicked() {
                        self.confirm_discard = false;
                    }
                    if ui.add(crate::ui_kit::button("Discard", false).ghost()).clicked() {
                        outcome = EditorOutcome::Close;
                        self.confirm_discard = false;
                    }
                });
            });
            if modal.dismissed() {
                self.confirm_discard = false;
            }
        }

        outcome
    }

    /// Under the picture: undo and redo, Start and End on the left; the
    /// previous moment, play, the next moment and the time in the middle;
    /// snapping, loop, zoom and fullscreen on the right.
    fn transport(&mut self, ui: &mut egui::Ui, bar: Rect) {
        use crate::ui_kit::{Btn, Kind};
        use egui_phosphor::regular as icon;
        let t = self.player.time();
        let prev = self.moments.iter().rev().find(|m| m.t < t - 0.05).cloned();
        let next = self.moments.iter().find(|m| m.t > t + 0.05).cloned();
        let (mut undo, mut redo, mut set_in, mut set_out, mut play, mut jump, mut crop) = (false, false, false, false, false, None, false);
        let mut split = false;
        let can_split = self.split_at(t).is_some();
        // With the clip in parts, Start and End are the part's at the playhead.
        let whose = if self.edit.parts().len() > 1 { "this part" } else { "the clip" };
        let cropping = self.crop_tool.is_some();
        let (mut zoom_by, mut zoom_to) = (None, None);
        let mut frames = self.show_frames;
        let can_undo = !self.undo.is_empty();
        let can_redo = !self.redo.is_empty();
        let full = self.info.duration.max(1e-6);
        let zoom_frac = (1.0 - ((self.view.1 / full).ln() / (self.min_span() / full).ln()).clamp(0.0, 1.0)) as f32;
        let zoom_frac = 1.0 - zoom_frac;
        // Narrower windows: Start, End and Split become their brackets and
        // icons and the sliders go (the zoom's buttons stay), then the time
        // moves out of the middle to make room. (Where each side's buttons
        // still fit beside the middle's.)
        let medium = bar.width() < 1040.0;
        let narrow = bar.width() < 740.0;
        let centre_w = if narrow { 44.0 } else { 250.0 };
        crate::transport::bar(ui, bar, true, centre_w, |ui, part| match part {
            crate::transport::Part::Left => {
                // Narrow: the time joins these (there's no room in the
                // middle), so they're smaller, with no rules between them,
                // and narrower still, Redo goes (Shift Cmd Z stays).
                let icon_btn = |kind, glyph| if narrow { Btn::icon(kind, glyph).small() } else { Btn::icon(kind, glyph) };
                let rule = |ui: &mut egui::Ui| if !narrow { crate::transport::vrule(ui) };
                undo = icon_btn(Kind::Ghost, icon::ARROW_U_UP_LEFT).enabled(can_undo).tip(format!("Undo  ({} Z)", crate::shortcuts::cmd_name())).show(ui).clicked();
                if bar.width() >= 600.0 {
                    redo = icon_btn(Kind::Ghost, icon::ARROW_U_UP_RIGHT).enabled(can_redo).tip(format!("Redo  (Shift {} Z)", crate::shortcuts::cmd_name())).show(ui).clicked();
                }
                rule(ui);
                let (start_tip, end_tip) = (format!("Start {whose} here  (I)"), format!("End {whose} here  (O)"));
                let split_tip = if can_split { "Split at the playhead, to cut a part out  (S)" } else { "Move the playhead to where you want to split  (S)" };
                if medium {
                    set_in = Btn::ghost("[").small().tip(start_tip).show(ui).clicked();
                    set_out = Btn::ghost("]").small().tip(end_tip).show(ui).clicked();
                    split = icon_btn(Kind::Ghost, icon::SCISSORS).enabled(can_split).tip(split_tip).show(ui).clicked();
                } else {
                    set_in = Btn::ghost("Start").with_icon("[").small().tip(start_tip).show(ui).clicked();
                    set_out = Btn::ghost("End").trail("]").small().tip(end_tip).show(ui).clicked();
                    split = Btn::ghost("Split").with_icon(icon::SCISSORS).small().enabled(can_split).tip(split_tip).show(ui).clicked();
                }
                rule(ui);
                let kind = if cropping { Kind::On } else { Kind::Ghost };
                let btn = if medium { icon_btn(kind, icon::CROP) } else { Btn::new(kind, "Crop").with_icon(icon::CROP).small() };
                crop = btn.tip("Crop the picture: keep a part of it, like 16:9 out of an ultrawide screen  (C)").show(ui).clicked();
                // No room in the middle: the time here, without the length.
                if narrow {
                    ui.add_space(4.0);
                    crate::transport::timecode(ui, t, 0.0, self.info.fps, &mut frames);
                }
            }
            crate::transport::Part::Centre => {
                // Narrow: play alone (the pins and [ ] still go to moments).
                let c = if narrow { crate::transport::play_button(ui, self.player.is_playing()) } else { crate::transport::centre(ui, self.player.is_playing(), prev.as_ref(), next.as_ref()) };
                play = c.play;
                if c.prev {
                    jump = prev.as_ref().map(|m| m.t);
                }
                if c.next {
                    jump = next.as_ref().map(|m| m.t);
                }
                if !narrow {
                    crate::transport::timecode(ui, t, self.info.duration, self.info.fps, &mut frames);
                }
            }
            crate::transport::Part::Right => {
                // Zoom: out, the slider, in.
                if Btn::icon(Kind::Ghost, icon::MAGNIFYING_GLASS_PLUS).small().tip("Zoom in  (+)").show(ui).clicked() {
                    zoom_by = Some(2.0);
                }
                let mut z = zoom_frac;
                if !medium && crate::transport::slider(ui, &mut z, 96.0).on_hover_text("Zoom the timeline").changed() {
                    zoom_to = Some(z);
                }
                if Btn::icon(Kind::Ghost, icon::MAGNIFYING_GLASS_MINUS).small().tip("Zoom out  (−)").show(ui).clicked() {
                    zoom_by = Some(0.5);
                }
                crate::transport::vrule(ui);
                let kind = |on: bool| if on { Kind::On } else { Kind::Ghost };
                if Btn::icon(kind(self.looping), icon::REPEAT).tip("Loop what's kept").show(ui).clicked() {
                    self.looping = !self.looping;
                }
                if Btn::icon(kind(self.snap), icon::MAGNET).tip("Snapping  (N): edges snap to the playhead, each other, the clip's ends and its moments").show(ui).clicked() {
                    self.snap = !self.snap;
                }
                // How loud you listen (not the edit's levels, which are on
                // the tracks): the player's, shared.
                crate::transport::vrule(ui);
                crate::transport::volume(ui, (!medium).then_some(80.0));
            }
        });
        self.show_frames = frames;
        if crop {
            self.toggle_crop();
        }
        if undo {
            self.undo_step();
        }
        if redo {
            self.redo_step();
        }
        if set_in {
            self.set_in();
        }
        if set_out {
            self.set_out();
        }
        if split {
            self.split(t);
        }
        if play {
            self.toggle_play();
        }
        if let Some(t) = jump {
            self.player.seek(t);
        }
        if let Some(f) = zoom_by {
            let (v0, span) = self.view;
            let frac = ((self.player.time() - v0) / span).clamp(0.0, 1.0);
            let new = (span / f).clamp(self.min_span(), full);
            let at = v0 + frac * span;
            let start = (at - frac * new).clamp(0.0, (full - new).max(0.0));
            self.view_target = Some((start, new));
        }
        if let Some(z) = zoom_to {
            // The slider's left end is the whole clip; its right, the closest zoom.
            let new = full * (self.min_span() / full).powf(z as f64);
            let (v0, span) = self.view;
            let frac = ((self.player.time() - v0) / span).clamp(0.0, 1.0);
            let at = v0 + frac * span;
            self.view = ((at - frac * new).clamp(0.0, (full - new).max(0.0)), new);
            self.view_target = None;
        }
    }

    /// Ease the view towards where a zoom button sent it.
    fn ease_view(&mut self, ctx: &egui::Context) {
        let Some((t0, ts)) = self.view_target else { return };
        let k = 1.0 - (-ctx.input(|i| i.stable_dt).min(0.05) as f64 * 18.0).exp();
        let (v0, span) = self.view;
        // Zoom in log space, so in and out feel the same.
        let span = (span.ln() + (ts.ln() - span.ln()) * k).exp();
        let v0 = v0 + (t0 - v0) * k;
        self.view = (v0, span);
        if (span / ts - 1.0).abs() < 0.002 && (v0 - t0).abs() < ts * 0.002 {
            self.view = (t0, ts);
            self.view_target = None;
        } else {
            ctx.request_repaint();
        }
    }

    /// The part of the picture kept, if not all of it.
    fn crop(&self) -> Option<media::Crop> {
        self.edit.output.crop.and_then(|c| c.within(self.info.width, self.info.height))
    }

    /// The part of the picture the saved clip keeps, if not all of it.
    fn saved_crop(&self) -> Option<media::Crop> {
        self.saved.output.crop.and_then(|c| c.within(self.info.width, self.info.height))
    }

    /// Open the crop tool, or close it.
    fn toggle_crop(&mut self) {
        self.crop_tool = match self.crop_tool {
            Some(_) => None,
            None => Some(crate::crop_ui::CropTool::new(self.crop())),
        };
    }

    /// Note the edit as it was before a change, for Cmd Z.
    fn push_undo(&mut self, was: Edit) {
        if self.undo.last().is_some_and(|e| same_edit(e, &was)) {
            return;
        }
        self.undo.push(was);
        if self.undo.len() > 200 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn undo_step(&mut self) {
        if let Some(e) = self.undo.pop() {
            self.redo.push(std::mem::replace(&mut self.edit, e));
        }
    }

    fn redo_step(&mut self) {
        if let Some(e) = self.redo.pop() {
            self.undo.push(std::mem::replace(&mut self.edit, e));
        }
    }

    /// Track `i`'s volume typed in, as it is at `t`: a flat one is set to
    /// it; a curve goes up or down as a whole until it's there (as dragging
    /// its line does).
    fn set_level_at(&mut self, i: usize, t: f64, db: f32) {
        if i >= self.edit.tracks.len() {
            return;
        }
        let db = (db.clamp(media::SILENT_DB, media::FADER_MAX_DB) * 10.0).round() / 10.0;
        let was = self.edit.clone();
        let track = &mut self.edit.tracks[i];
        if track.points.is_empty() {
            track.gain = media::from_db(db);
        } else {
            let d = media::fader_pos(db) - media::fader_pos(track.db_at(t));
            for pt in &mut track.points {
                pt.db = media::fader_db(media::fader_pos(pt.db) + d);
            }
        }
        if !same_edit(&self.edit, &was) {
            self.push_undo(was);
        }
    }

    /// The kept length under the timeline: shown, and on a click typed over
    /// (digits fill the clock, "1250" is 12:50 and "12.5" 12.5 seconds; or
    /// frames when times show as frames). Enter, or ↑ ↓, makes the kept part
    /// that long around its middle (as long as the clip allows).
    fn length_field(&mut self, ui: &mut egui::Ui, rect: Rect, shown: &str) {
        let pal = crate::theme::pal(ui);
        let id = ui.id().with("length_field");
        if !self.show_frames {
            let tid = id.with("time");
            let focused = ui.memory(|m| m.has_focus(tid));
            let hv = crate::theme::toggle(ui.ctx(), id.with("hv"), ui.rect_contains_pointer(rect), crate::theme::FAST);
            ui.painter().rect_filled(rect, 5, if focused { pal.control } else { crate::theme::mix(pal.base, pal.control, hv) });
            let field = crate::time_field::TimeField {
                id: tid,
                secs: self.edit.duration(),
                min: 1.0 / self.info.fps.max(1.0),
                max: self.info.duration,
                decimals: 2,
                font: crate::theme::font(12.0, crate::theme::W::Semibold),
                align: egui::Align::Center,
                hint: "The kept part's length: click to type another (500 is 5:00)",
            };
            if let crate::time_field::Timed::Stepped(secs) | crate::time_field::Timed::Typed(secs) = field.show(ui, rect)
                && secs > 0.0
            {
                let was = self.edit.clone();
                (self.edit.start, self.edit.end) = trim_to_length(&self.info, self.edit.start, self.edit.end, secs);
                if !same_edit(&self.edit, &was) {
                    self.push_undo(was);
                }
            }
            if focused {
                ui.painter().rect_stroke(rect, 5, Stroke::new(1.5, pal.text2), egui::StrokeKind::Inside);
            }
            return;
        }
        match self.typing_length.as_mut() {
            None => {
                let resp = ui.interact(rect, id, Sense::click());
                let hv = crate::theme::toggle(ui.ctx(), id.with("hv"), resp.hovered(), crate::theme::FAST);
                ui.painter().rect_filled(rect, 5, crate::theme::mix(pal.base, pal.control, hv));
                ui.painter().text(rect.center(), Align2::CENTER_CENTER, shown, crate::theme::font(12.0, crate::theme::W::Semibold), pal.text);
                if resp.on_hover_cursor(egui::CursorIcon::Text).on_hover_text("The kept part's length: click to type another").clicked() {
                    self.typing_length = Some(shown.trim_end_matches(" frames").to_owned());
                    ui.memory_mut(|m| m.request_focus(id.with("edit")));
                }
            }
            Some(text) => {
                ui.painter().rect_filled(rect, 5, pal.control);
                let out = egui::TextEdit::singleline(text)
                    .id(id.with("edit"))
                    .frame(egui::Frame::NONE)
                    .font(crate::theme::font(12.0, crate::theme::W::Semibold))
                    .horizontal_align(egui::Align::Center)
                    .desired_width(rect.width() - 8.0)
                    .show(&mut ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(Vec2::new(4.0, 2.0)))));
                ui.painter().rect_stroke(rect, 5, Stroke::new(1.5, pal.text2), egui::StrokeKind::Inside);
                let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                let esc = ui.input(|i| i.key_pressed(egui::Key::Escape));
                if out.response.lost_focus() || esc {
                    let typed = std::mem::take(text);
                    self.typing_length = None;
                    if enter && !esc {
                        let secs = typed.trim().parse::<f64>().ok().map(|f| f / self.info.fps);
                        match secs.filter(|s| *s > 0.0) {
                            Some(secs) => {
                                let was = self.edit.clone();
                                (self.edit.start, self.edit.end) = trim_to_length(&self.info, self.edit.start, self.edit.end, secs);
                                if !same_edit(&self.edit, &was) {
                                    self.push_undo(was);
                                }
                            }
                            None => {}
                        }
                    }
                }
            }
        }
    }

    /// Snap `t` to the playhead, the parts' edges or the clip's ends when
    /// it's within a few points of one (and snapping is on). `except`: what's
    /// being moved, which mustn't snap to itself.
    fn snapped(&self, t: f64, px_per_s: f64, except: Option<Drag>) -> f64 {
        self.snap_target(t, px_per_s, except).unwrap_or(t)
    }

    /// What [`Self::snapped`] snaps `t` to, if anything.
    fn snap_target(&self, t: f64, px_per_s: f64, except: Option<Drag>) -> Option<f64> {
        if !self.snap {
            return None;
        }
        let reach = 8.0 / px_per_s.max(1e-6);
        let mut targets = vec![0.0, self.info.duration];
        targets.extend(self.moments.iter().map(|m| m.t));
        // Not while a trim edge moves: the playhead follows the edge, and
        // snapping to it made the edge chase its own shadow (back and forth
        // while held still).
        if !matches!(except, Some(Drag::Playhead | Drag::Start(_) | Drag::End(_))) {
            targets.push(self.info.snap(self.player.time()));
        }
        for (k, (a, b)) in self.edit.parts().into_iter().enumerate() {
            if except != Some(Drag::Start(k)) {
                targets.push(a);
            }
            if except != Some(Drag::End(k)) {
                targets.push(b);
            }
        }
        targets.into_iter().filter(|x| (x - t).abs() < reach).min_by(|a, b| (a - t).abs().total_cmp(&(b - t).abs()))
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
        EditorOutcome::Saved { target: target.clone(), info: self.info.clone(), edit: self.edit.clone(), new_name, then: None }
    }

    fn keyboard(&mut self, ctx: &egui::Context) {
        // A dialog (its own or the app's) or a menu has the keys.
        if ctx.egui_wants_keyboard_input() || crate::ui_kit::overlay_open(ctx) || self.save_as.is_some() || self.confirm_discard || self.free_up.is_some() {
            return;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::F2)) {
            self.rename_requested = true;
        }
        let (crop, done) = ctx.input_mut(|i| {
            let crop = i.consume_key(egui::Modifiers::NONE, Key::C);
            let done = self.crop_tool.is_some() && (i.consume_key(egui::Modifiers::NONE, Key::Enter) || i.consume_key(egui::Modifiers::NONE, Key::Escape));
            (crop, done)
        });
        if crop {
            self.toggle_crop();
        }
        if done {
            self.crop_tool = None;
        }
        let (redo, undo, snap, mute) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, Key::Z),
                i.consume_key(egui::Modifiers::COMMAND, Key::Z),
                i.consume_key(egui::Modifiers::NONE, Key::N),
                i.consume_key(egui::Modifiers::NONE, Key::M),
            )
        });
        if mute {
            crate::transport::toggle_mute(ctx);
        }
        if undo {
            self.undo_step();
        }
        if redo {
            self.redo_step();
        }
        if snap {
            self.snap = !self.snap;
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
        if self.confirm_revert {
            return;
        }
        if fit {
            self.view_target = Some((0.0, self.info.duration.max(1e-6)));
        }
        // Splitting and cutting out, at the playhead: S (or Cmd B, as other
        // editors have it) splits there, Delete cuts out the part it's in.
        let at_playhead = (self.edit.parts().len() > 1).then(|| self.part_at(self.player.time())).flatten();
        let (split, cut, up, down) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, Key::S) || i.consume_key(egui::Modifiers::COMMAND, Key::B),
                at_playhead.is_some() && (i.consume_key(egui::Modifiers::NONE, Key::Delete) || i.consume_key(egui::Modifiers::NONE, Key::Backspace)),
                i.consume_key(egui::Modifiers::NONE, Key::ArrowUp),
                i.consume_key(egui::Modifiers::NONE, Key::ArrowDown),
            )
        });
        if split {
            self.split(self.player.time());
        }
        if let (true, Some(k)) = (cut, at_playhead) {
            self.cut_out(k, ctx.input(|i| i.time));
        }
        // ↑ ↓: the previous or next cut (a part's first or last frame).
        if up || down {
            let t = self.info.snap(self.player.time());
            let points = self.cut_points();
            let to = if up { points.iter().rev().find(|&&p| p < t - 1e-6) } else { points.iter().find(|&&p| p > t + 1e-6) };
            if let Some(&to) = to {
                self.player.pause();
                self.player.seek(to + 1e-6);
            }
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
        // Play within the trim; starting from outside it (or the end) restarts
        // at in. The last kept frame counts as the end: dragging the out
        // handle leaves the playhead on it, and playing from there would show
        // that one frame and stop.
        let t = self.player.time();
        if t < self.edit.start || t >= self.edit.end - self.info.frame_duration() * 1.5 {
            self.player.seek(self.edit.start);
        } else if self.part_at(t).is_none()
            && let Some(&(a, _)) = self.edit.parts().iter().find(|&&(a, _)| a > t)
        {
            // In what's cut out: on from the part after it.
            self.player.seek(a);
        }
        self.player.play(self.edit.end);
    }

    fn step(&mut self, frames: i64) {
        self.player.pause();
        let f = self.info.frame_index(self.player.time()) as i64 + frames;
        let max = (self.info.duration * self.info.fps).floor() as i64 - 1;
        self.player.seek(f.clamp(0, max.max(0)) as f64 / self.info.fps + 1e-6);
    }

    /// Start here (I): see [`start_at`].
    fn set_in(&mut self) {
        let t = self.info.snap(self.player.time());
        if let Some(parts) = start_at(&self.edit.parts(), t, self.info.frame_duration()) {
            self.push_undo(self.edit.clone());
            self.edit.set_parts(&parts);
        }
    }

    /// End here (O): see [`end_at`].
    fn set_out(&mut self) {
        let fd = self.info.frame_duration();
        // Out is exclusive: include the frame under the playhead.
        let t = (self.info.snap(self.player.time()) + fd).min(self.info.duration);
        if let Some(parts) = end_at(&self.edit.parts(), t, fd) {
            self.push_undo(self.edit.clone());
            self.edit.set_parts(&parts);
        }
    }

    /// The part `t` is in, if it's kept.
    fn part_at(&self, t: f64) -> Option<usize> {
        self.edit.parts().iter().position(|&(a, b)| t >= a - 1e-9 && t < b - 1e-9)
    }

    /// The part a split at `t` would split, if one would be.
    fn split_at(&self, t: f64) -> Option<usize> {
        splits(&self.edit.parts(), self.info.snap(t), self.info.frame_duration())
    }

    /// Split the part at `t` in two there: the frame at `t` starts the second.
    fn split(&mut self, t: f64) {
        let t = self.info.snap(t);
        let mut parts = self.edit.parts();
        let Some(k) = splits(&parts, t, self.info.frame_duration()) else { return };
        self.push_undo(self.edit.clone());
        let (a, b) = parts[k];
        parts.splice(k..=k, [(a, t), (t, b)]);
        self.edit.set_parts(&parts);
    }

    /// A volume point on track `i` at `t`, at the level the line has there
    /// (the first one takes the track's flat level).
    fn add_point(&mut self, i: usize, t: f64) {
        let fd = self.info.frame_duration();
        let Some(track) = self.edit.tracks.get(i) else { return };
        if track.points.iter().any(|pt| (pt.t - t).abs() < fd / 2.0) {
            return;
        }
        self.push_undo(self.edit.clone());
        let track = &mut self.edit.tracks[i];
        let db = track.db_at(t);
        if track.points.is_empty() {
            track.gain = 1.0;
        }
        let at = track.points.partition_point(|pt| pt.t < t);
        track.points.insert(at, media::VolumePoint { t, db });
    }

    /// Take point `k` of track `i` away; the last one leaves its level as the
    /// flat level.
    fn remove_point(&mut self, i: usize, k: usize) {
        if self.edit.tracks.get(i).is_none_or(|t| k >= t.points.len()) {
            return;
        }
        self.push_undo(self.edit.clone());
        let track = &mut self.edit.tracks[i];
        let removed = track.points.remove(k);
        if track.points.is_empty() {
            track.gain = media::from_db(removed.db);
        }
    }

    /// Cut part `k` out (it stays on the timeline, dimmed, to put back). The
    /// clip keeps one part at least. A playhead in it moves on to the next.
    fn cut_out(&mut self, k: usize, now: f64) {
        let mut parts = self.edit.parts();
        if parts.len() < 2 || k >= parts.len() {
            return;
        }
        self.push_undo(self.edit.clone());
        let (a, b) = parts.remove(k);
        self.edit.set_parts(&parts);
        self.fading = Some(Fading { from: a, to: b, cut: true, since: now });
        let t = self.player.time();
        if t >= a && t < b {
            let fd = self.info.frame_duration();
            let to = parts.get(k).map_or_else(|| parts[k - 1].1 - fd, |p| p.0);
            self.player.seek(to + 1e-6);
        }
    }

    /// Keep what's cut out or trimmed off at `spot` again: the parts either
    /// side of it are one again.
    fn put_back(&mut self, spot: Spot, now: f64) {
        let Some((parts, back)) = put_back(&self.edit.parts(), spot, self.info.duration) else { return };
        self.push_undo(self.edit.clone());
        self.edit.set_parts(&parts);
        if back.1 - back.0 > 1e-6 {
            self.fading = Some(Fading { from: back.0, to: back.1, cut: false, since: now });
        }
    }

    /// Where the playhead goes with ↑ and ↓: the first and last frame of
    /// each part, in order.
    fn cut_points(&self) -> Vec<f64> {
        let fd = self.info.frame_duration();
        self.edit.parts().iter().flat_map(|&(a, b)| [a, (b - fd).max(a)]).collect()
    }

    /// What the pointer does on the timeline (`resp`, laid out as `l`, as
    /// it was drawn last frame): a drag takes hold of what's under it (a
    /// part's edge, a split, a part, a volume line or point, else the
    /// playhead) and moves it; a press goes there; a double-click adds a
    /// volume point or takes one away; a right-click picks what the menu is
    /// for. Read first thing in the frame, so the picture, the time, the
    /// playhead and the parts' edges all follow the pointer on the frame it
    /// moves, as in the player.
    fn timeline_input(&mut self, ui: &egui::Ui, l: &Layout, resp: &egui::Response) {
        let (lanes, lane_rects, audio_view, splits) = (l.lanes, &l.lane_rects, l.audio_view, &l.splits);
        let parts = &l.parts;
        let n = parts.len();
        let (dur, fd, px_per_s) = (l.dur, self.info.frame_duration(), l.px_per_s());
        let t_of = |x: f32| l.t_of(x);
        let near_grip = |pos: Pos2| l.near_grip(pos);
        let vol_at = |pos: Pos2, this: &Self| l.vol_at(pos, &this.edit.tracks);
        let hit = |pos: Pos2, this: &Self| l.hit(pos, &this.edit.tracks);

        // The playhead goes where the button goes down, at once, and playing
        // pauses until it's let go, a click as a drag (a drag on a part or
        // its edge then moves it on). Not on the volume, where a drag only
        // shapes it and never interrupts playback.
        if resp.is_pointer_button_down_on()
            && let Some(pos) = ui.input(|i| i.pointer.primary_pressed().then(|| i.pointer.press_origin()).flatten())
            && !matches!(hit(pos, self), Some(Drag::Line(_) | Drag::Point(..)))
        {
            self.resume = self.player.is_playing();
            self.player.pause();
            let t = self.snapped(t_of(pos.x), px_per_s, Some(Drag::Playhead));
            self.player.seek(self.info.snap(t) + 1e-6);
        }
        if resp.drag_started() {
            // Hit-test where the button went down, not where the pointer is now: a
            // drag only starts after a few pixels of movement, by which time a
            // vertical drag has already left the thin volume line or point.
            if let Some(pos) = ui.input(|i| i.pointer.press_origin()).or(resp.interact_pointer_pos()) {
                let d = match hit(pos, self) {
                    // A split opens the way it's pulled: the part after it
                    // starts later, or the one before ends sooner.
                    Some(Drag::Split(k)) => {
                        if resp.interact_pointer_pos().unwrap_or(pos).x >= pos.x {
                            Drag::Start(k + 1)
                        } else {
                            Drag::End(k)
                        }
                    }
                    d => d.unwrap_or(Drag::Playhead),
                };
                self.dragging = Some(d);
                if d != Drag::Playhead {
                    self.before_drag = Some(self.edit.clone());
                }
                (self.vol_moved, self.vol_grab, self.vol_guide) = (Vec2::ZERO, t_of(pos.x), None);
                // Where it was grabbed (in a part, or beside an edge), so it
                // moves with the pointer instead of jumping to it.
                let at = t_of(pos.x);
                self.grab_offset = match d {
                    Drag::Start(k) => at - parts[k].0,
                    Drag::End(k) => at - parts[k].1,
                    Drag::Range(k) => self.info.snap(at) - parts[k].0,
                    _ => self.info.snap(at) - self.edit.start,
                };
                // Moving the playhead or a trim pauses while you drag and carries on
                // after; shaping the volume never interrupts playback.
                if matches!(d, Drag::Start(_) | Drag::End(_) | Drag::Range(_) | Drag::Playhead) {
                    // (Already paused by the press, which noted if it played.)
                    self.resume |= self.player.is_playing();
                    self.player.pause();
                }
            }
        }
        if let (Some(pos), Some(d)) = (resp.interact_pointer_pos(), self.dragging) {
            // Dragging the playhead or a trim past either end scrolls a zoomed timeline.
            if matches!(d, Drag::Start(_) | Drag::End(_) | Drag::Range(_) | Drag::Playhead) && resp.dragged() {
                let over = if pos.x > lanes.right() { pos.x - lanes.right() } else if pos.x < lanes.left() { pos.x - lanes.left() } else { 0.0 };
                if over != 0.0 {
                    self.view.0 = (self.view.0 + (over / lanes.width()) as f64 * self.view.1 * 0.1).clamp(0.0, (dur - self.view.1).max(0.0));
                    ui.ctx().request_repaint();
                }
            }
            if resp.dragged() || resp.drag_started() {
                // A volume drag goes a fifth as far with Shift held, for fine
                // steps (the first frame takes in the movement before the drag
                // registered); Ctrl keeps it from snapping.
                let (fine, no_snap) = ui.input(|i| (if i.modifiers.shift { 0.2 } else { 1.0 }, i.modifiers.ctrl));
                if matches!(d, Drag::Line(_) | Drag::Point(..)) {
                    let moved = match ui.input(|i| i.pointer.press_origin()) {
                        Some(origin) if resp.drag_started() => pos - origin,
                        _ => resp.drag_delta(),
                    };
                    self.vol_moved += moved * fine;
                }
                let t = self.info.snap(t_of(pos.x));
                let mut parts = self.edit.parts();
                // A part's edges go no further than the parts beside it (or
                // the clip's ends), and keep it a frame long.
                let lo = |k: usize, parts: &[(f64, f64)]| if k > 0 { parts[k - 1].1 } else { 0.0 };
                let hi = |k: usize, parts: &[(f64, f64)]| parts.get(k + 1).map_or(self.info.duration, |p| p.0);
                match d {
                    Drag::Start(k) if k < parts.len() => {
                        let t = self.info.snap(self.snapped(t_of(pos.x) - self.grab_offset, px_per_s, Some(d)));
                        parts[k].0 = t.min(parts[k].1 - fd).max(lo(k, &parts));
                        self.edit.set_parts(&parts);
                        self.player.seek(parts[k].0);
                    }
                    Drag::End(k) if k < parts.len() => {
                        let t = self.snapped(t_of(pos.x) - self.grab_offset, px_per_s, Some(d));
                        parts[k].1 = self.info.snap(t).clamp(parts[k].0 + fd, hi(k, &parts).max(parts[k].0 + fd));
                        self.edit.set_parts(&parts);
                        self.player.seek(parts[k].1 - fd);
                    }
                    Drag::Range(k) if k < parts.len() => {
                        // Same length, slid along the clip, stopping at the
                        // parts beside it (or the clip's ends).
                        let len = parts[k].1 - parts[k].0;
                        let (lo, hi) = (lo(k, &parts), hi(k, &parts));
                        let start = self.info.snap((t - self.grab_offset).clamp(lo, (hi - len).max(lo)));
                        parts[k] = (start, (start + len).min(hi));
                        self.edit.set_parts(&parts);
                        self.player.seek(start);
                    }
                    Drag::Start(_) | Drag::End(_) | Drag::Range(_) | Drag::Split(_) => {}
                    Drag::Playhead => {
                        let target = self.snap_target(t, px_per_s, Some(Drag::Playhead));
                        // On an edge, a marker or an end: that frame exactly,
                        // not a picture near it (letting go doesn't land).
                        self.playhead_snapped = target.is_some();
                        self.player.seek(self.info.snap(target.unwrap_or(t)) + 1e-6);
                    }
                    Drag::Line(i) => {
                        // The whole curve, up or down by how far it's been
                        // dragged (along the fader, so it follows the pointer).
                        // Level, it snaps to 0 dB as it passes (Ctrl: it doesn't).
                        let lane = lane_rects[i];
                        let travel = lane.height() - 8.0;
                        let was = self.before_drag.as_ref().and_then(|e| e.tracks.get(i)).cloned();
                        let mut guide = None;
                        if let Some(was) = was {
                            let d = -self.vol_moved.y / travel;
                            let track = &mut self.edit.tracks[i];
                            if was.points.is_empty() {
                                let mut db = media::fader_db(media::fader_pos(media::to_db(was.gain)) + d);
                                // Unchanged, or silent at the bottom.
                                if let Some(to) = [0.0, media::SILENT_DB].into_iter().find(|&to| !no_snap && (y_of_db(lane, db) - y_of_db(lane, to)).abs() < SNAP_PX) {
                                    db = to;
                                    guide = Some((i, to));
                                }
                                track.gain = media::from_db(db);
                            } else {
                                for (pt, from) in track.points.iter_mut().zip(&was.points) {
                                    pt.db = media::fader_db(media::fader_pos(from.db) + d);
                                }
                            }
                        }
                        self.vol_guide = guide;
                    }
                    Drag::Point(i, k) => {
                        // From where it was by how far it's been dragged (taken
                        // hold of off its middle, it doesn't jump to the
                        // pointer), between its neighbours. Its level snaps to
                        // 0 dB and to theirs as it passes (Ctrl: it doesn't).
                        let lane = lane_rects[i];
                        let from = self.before_drag.as_ref().and_then(|e| e.tracks.get(i)).and_then(|t| t.points.get(k)).map(|p| (p.t, p.db));
                        let mut guide = None;
                        if let Some((t0, db0)) = from.filter(|_| k < self.edit.tracks[i].points.len()) {
                            let track = &mut self.edit.tracks[i];
                            // Keep points in time order: clamp between neighbours.
                            let lo = if k > 0 { track.points[k - 1].t + fd } else { 0.0 };
                            let hi = track.points.get(k + 1).map_or(self.info.duration, |n| n.t - fd);
                            let t = self.info.snap(t0 + self.vol_moved.x as f64 / px_per_s).clamp(lo, hi.max(lo));
                            let mut db = media::fader_db(media::fader_pos(db0) - self.vol_moved.y / (lane.height() - 8.0));
                            if !no_snap {
                                let levels = [Some(0.0), Some(media::SILENT_DB), k.checked_sub(1).map(|j| track.points[j].db), track.points.get(k + 1).map(|n| n.db)];
                                let nearest = levels
                                    .into_iter()
                                    .flatten()
                                    .map(|to| (to, (y_of_db(lane, db) - y_of_db(lane, to)).abs()))
                                    .filter(|(_, off)| *off < SNAP_PX)
                                    .min_by(|a, b| a.1.total_cmp(&b.1));
                                if let Some((to, _)) = nearest {
                                    db = to;
                                    guide = Some((i, to));
                                }
                            }
                            track.points[k].t = t;
                            track.points[k].db = db;
                            if !self.player.is_playing() {
                                self.player.seek(t + 1e-6);
                            }
                        }
                        self.vol_guide = guide;
                    }
                }
            }
        }
        if resp.drag_stopped() {
            self.vol_guide = None;
            if let Some(was) = self.before_drag.take()
                && !same_edit(&was, &self.edit)
            {
                self.push_undo(was);
            }
            // The playhead on the frame you see, not one decoded after.
            if self.dragging == Some(Drag::Playhead) {
                self.player.land();
            }
            self.dragging = None;
            self.playhead_snapped = false;
            if std::mem::take(&mut self.resume) && self.player.time() < self.edit.end - fd {
                self.player.play(self.edit.end);
            }
        }

        // Let go of a press that didn't become a drag: playing carries on
        // from where it went.
        if self.dragging.is_none() && self.resume && !ui.input(|i| i.pointer.primary_down()) {
            self.resume = false;
            self.player.land();
            if self.player.time() < self.edit.end - fd {
                self.player.play(self.edit.end);
            }
        }
        if resp.double_clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                if let Some(Drag::Point(i, k)) = vol_at(pos, self) {
                    // On a point: it goes.
                    self.remove_point(i, k);
                } else if let Some(i) = lane_rects.iter().position(|l| l.contains(pos) && audio_view.contains(pos)) {
                    // Add a point on the curve at this time (keeps the sound unchanged
                    // until it's dragged). The first one adopts the track's flat level.
                    let t = self.info.snap(t_of(pos.x));
                    let was = self.edit.clone();
                    let track = &mut self.edit.tracks[i];
                    let db = track.db_at(t);
                    if track.points.is_empty() {
                        track.gain = 1.0;
                    }
                    if !track.points.iter().any(|pt| (pt.t - t).abs() < fd / 2.0) {
                        let at = track.points.partition_point(|pt| pt.t < t);
                        track.points.insert(at, media::VolumePoint { t, db });
                        self.push_undo(was);
                    }
                }
            }
        } else if resp.clicked() {
            // On the volume, jump there (elsewhere, the press already did).
            if let Some(pos) = resp.interact_pointer_pos().filter(|&pos| matches!(hit(pos, self), Some(Drag::Line(_) | Drag::Point(..)))) {
                let t = self.snapped(t_of(pos.x), px_per_s, Some(Drag::Playhead));
                self.player.seek(self.info.snap(t) + 1e-6);
            }
        }
        // Right-click: a menu for what's there (a part's handle is the part's).
        if resp.secondary_clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                {
                    let t = t_of(pos.x);
                    let spot = if let Some(Drag::Point(i, k)) = vol_at(pos, self) {
                        Spot::Point(i, k)
                    } else if let Some(Drag::Start(k) | Drag::End(k)) = near_grip(pos) {
                        Spot::Part(k)
                    } else if let Some(Drag::Line(i)) = vol_at(pos, self) {
                        Spot::Volume(i)
                    } else if let Some(&(_, Drag::Split(k))) = splits.iter().find(|(r, _)| r.contains(pos)) {
                        Spot::Split(k)
                    } else if let Some(k) = parts.iter().position(|&(a, b)| t >= a && t < b) {
                        Spot::Part(k)
                    } else if t < parts[0].0 {
                        Spot::Before
                    } else if t >= parts[n - 1].1 {
                        Spot::After
                    } else {
                        Spot::Gap(parts.iter().rposition(|&(_, b)| b <= t).unwrap_or(0))
                    };
                    self.menu_at = Some((spot, t));
                }
            }
        }
    }

    /// The timeline laid out in `outer`, as the view, the sound lanes'
    /// scroll and the edit are now (see [`Layout`]).
    fn layout(&self, outer: Rect) -> Layout {
        let n_audio = self.edit.tracks.len();
        let lane_step = AUDIO_H + 4.0;
        // Under the lanes, the line saying how long the parts are.
        let body_bottom = outer.bottom() - LENGTH_H;
        let audio_view = Rect::from_x_y_ranges(outer.x_range(), outer.top() + RULER_H + VIDEO_H..=body_bottom);
        // The mix's meter stands at the right, as tall as the timeline, as
        // far in from the edge as the transport's buttons are.
        let right = outer.right() - crate::transport::BAR_PAD;
        let meter_col = Rect::from_min_max(Pos2::new(right - MIX_W, outer.top()), Pos2::new(right, body_bottom));
        // Room between for the end handle and the lanes' scrollbar.
        let lanes_x = outer.left() + HEADER_W..=meter_col.left() - 24.0;
        let lanes = Rect::from_x_y_ranges(lanes_x.clone(), outer.top()..=body_bottom);
        let ruler = Rect::from_min_size(Pos2::new(lanes.left(), outer.top()), Vec2::new(lanes.width(), RULER_H));
        let video = Rect::from_min_size(Pos2::new(lanes.left(), ruler.bottom()), Vec2::new(lanes.width(), VIDEO_H));
        // The sound lanes, as far as they're scrolled.
        let top = audio_view.top() - self.lane_scroll + 4.0;
        let lane_rects = (0..n_audio).map(|i| Rect::from_min_size(Pos2::new(lanes.left(), top + i as f32 * lane_step), Vec2::new(lanes.width(), AUDIO_H))).collect();
        let body = Rect::from_x_y_ranges(lanes_x, video.top()..=(top + n_audio as f32 * lane_step - 4.0).min(audio_view.bottom()));
        let hit_area = Rect::from_x_y_ranges(lanes.left() - HANDLE_W - 4.0..=lanes.right() + HANDLE_W + 4.0, ruler.top()..=body_bottom + LENGTH_H);
        let dur = self.info.duration.max(1e-6);
        let mut l = Layout { lanes, ruler, video, audio_view, meter_col, body, lane_rects, dur, view: self.view, parts: self.edit.parts(), xs: Vec::new(), splits: Vec::new(), grips: Vec::new(), ranges: Vec::new(), dim_y: body_bottom + LENGTH_H / 2.0, hit_area };
        l.xs = l.parts.iter().map(|&(a, b)| (l.x_of(a), l.x_of(b))).collect();
        let n = l.parts.len();
        // A split: the film cut through, with a grip on the picture's lane to
        // pull it open with.
        l.splits = (0..n).filter(|&k| l.split(k)).map(|k| (Rect::from_center_size(Pos2::new(l.xs[k].1, body.center().y), Vec2::new(12.0, body.height())), Drag::Split(k))).collect();
        // Handles at each part's ends, outside it; between parts, as wide as
        // half of what's cut out lets them be.
        for k in 0..n {
            let (xa, xb) = l.xs[k];
            if k == 0 || !l.split(k - 1) {
                let w = if k == 0 { HANDLE_W } else { ((xa - l.xs[k - 1].1) / 2.0).clamp(3.0, HANDLE_W) };
                l.grips.push((Rect::from_x_y_ranges(xa - w..=xa, body.y_range()), Drag::Start(k)));
            }
            if !l.split(k) {
                let w = if k + 1 == n { HANDLE_W } else { ((l.xs[k + 1].0 - xb) / 2.0).clamp(3.0, HANDLE_W) };
                l.grips.push((Rect::from_x_y_ranges(xb..=xb + w, body.y_range()), Drag::End(k)));
            }
        }
        l.ranges = (0..n)
            .map(|k| {
                let (xa, xb) = l.shown_x(k);
                Rect::from_x_y_ranges(xa.max(lanes.left())..=xb.min(lanes.right()), l.dim_y - 9.0..=l.dim_y + 9.0)
            })
            .collect();
        l
    }

    /// One shared time axis for everything, like Premiere / Resolve: the
    /// ruler, the video lane (filmstrip) and one lane per sound track, all
    /// lined up so a sound sits right under the frames it belongs to. A
    /// header column names each lane (mute, solo, its level). The kept part
    /// is outlined across every lane, the rest dimmed, with its length under
    /// the lanes; the handles at its ends run the full height.
    ///
    /// Volume is edited on the lane itself: drag the line up or down to change
    /// the whole track, double-click it to add a point, drag a point to shape
    /// a ramp, right-click a point to remove it.
    ///
    /// `resp`: what the pointer does on it, read at the start of the frame
    /// (see [`Ready::timeline_input`]).
    fn timeline(&mut self, ui: &mut egui::Ui, resp: Option<egui::Response>) {
        let pal = crate::theme::pal(ui);
        let n_audio = self.edit.tracks.len();
        let lane_step = AUDIO_H + 4.0;
        let lanes_h = n_audio as f32 * lane_step;
        // As tall as it needs, up to what's left; the sound lanes scroll in
        // whatever height that leaves them.
        let room = (ui.available_height() - LENGTH_H - 8.0).max(RULER_H + VIDEO_H + lane_step);
        // Ruler, video lane, the gap above the lanes, the lanes.
        let total_h = (RULER_H + VIDEO_H + 4.0 + lanes_h).min(room);
        let (outer, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), total_h + LENGTH_H), Sense::hover());
        // What the pointer does on it is read here, first thing next frame.
        self.timeline_at = Some(outer);
        // Laid out as it was, for the wheel (where it zooms about): laid out
        // again below, as the wheel leaves it.
        let before = self.layout(outer);
        let (lanes, audio_view, meter_col) = (before.lanes, before.audio_view, before.meter_col);
        // The lanes and the gap above them; a few points over is no reason
        // to scroll.
        let over = 4.0 + lanes_h - audio_view.height();
        let max_scroll = if over > 2.0 { over } else { 0.0 };
        // Short with the picture already as small as the divider lets it be
        // (it gives the lanes their room first): next frame asks for this much more.
        if over > 0.5 {
            self.lanes_short = (self.lanes_short + over).min(200.0);
        } else if over < -0.5 {
            self.lanes_short = (self.lanes_short + over).max(0.0);
        }
        let p = ui.painter_at(outer);

        let dur = self.info.duration.max(1e-6);
        let fd = self.info.frame_duration();
        // The wheel over the sound lanes scrolls them when they don't all fit;
        // sideways (Shift+wheel, trackpad) still pans the timeline.
        // Wheel input glides (see `wheel.rs`): read raw, eased here.
        let ctx = ui.ctx().clone();
        let pointer = ctx.pointer_hover_pos();
        // Not under a dialog or menu: scrolling it mustn't move the timeline.
        let input = if pointer.is_some_and(|p| outer.contains(p)) && !crate::ui_kit::overlay_open(&ctx) { crate::wheel::read(&ctx) } else { crate::wheel::Input { pinch: 1.0, ..Default::default() } };
        let scroll = self.glide.step(&ctx, input.scroll);
        let zoom_pts = self.zoom_glide.step1(&ctx, input.zoom);
        let zoom = (ctx.options(|o| o.input_options.scroll_zoom_speed) * zoom_pts).exp() * input.pinch;
        if zoom != 1.0 || scroll != Vec2::ZERO {
            self.view_target = None;
        }
        let over_audio = pointer.is_some_and(|p| audio_view.contains(p));
        let lanes_take_vertical = over_audio && max_scroll > 0.0;
        if lanes_take_vertical {
            self.lane_scroll -= scroll.y;
        }
        self.lane_scroll = self.lane_scroll.clamp(0.0, max_scroll);
        let pan = -(scroll.x + if lanes_take_vertical { 0.0 } else { scroll.y });
        self.zoom_and_pan(lanes, dur, pointer, pan, zoom);
        // Laid out as the wheel leaves the view and the lanes' scroll (and as
        // the pointer left the edit, first thing this frame).
        let l = self.layout(outer);
        let (v0, span) = l.view;
        let px_per_s = l.px_per_s();
        self.px_per_s = px_per_s;
        let x_of = |t: f64| l.x_of(t);
        let t_of = |x: f32| l.t_of(x);
        // Everything on the time axis is clipped to the lanes, so a zoomed-in
        // timeline never draws into the headers.
        let lp = ui.painter_at(lanes);

        // --- Ruler ---
        let ruler = l.ruler;
        // Pins: click one to go there (markers are added and taken away
        // while watching).
        let moments = std::mem::take(&mut self.moments);
        let pinned = crate::transport::ruler(ui, ruler, (v0, span), self.info.fps, self.show_frames, &moments, false);
        self.moments = moments;
        if let Some(crate::transport::Pinned::Go(t)) = pinned {
            self.player.seek(t);
        }
        // Zoomed in: which part of the clip is showing, along the ruler's top.
        let zoomed = theme_fade(&ctx, "ed_zoomed", span < dur - 1e-9);
        if zoomed > 0.0 {
            let track = Rect::from_min_size(ruler.min + Vec2::new(0.0, 1.0), Vec2::new(ruler.width(), 3.0));
            lp.rect_filled(track, 2, crate::theme::fade(pal.line2, zoomed));
            let a = track.left() + (v0 / dur) as f32 * track.width();
            let b = track.left() + ((v0 + span) / dur) as f32 * track.width();
            lp.rect_filled(Rect::from_x_y_ranges(a..=b.max(a + 4.0), track.y_range()), 2, crate::theme::fade(pal.text3, zoomed));
        }

        // --- The mix's meter, on the right ---
        let mix = Rect::from_min_max(meter_col.min + Vec2::new(6.0, 4.0), meter_col.max);
        crate::meter::stereo_vertical(ui, mix, ui.id().with("mix_meter"), &self.mix_meter);
        ui.interact(mix.with_max_y(mix.top() + 16.0), ui.id().with("mix_tip"), Sense::hover()).on_hover_text("The clip's mix: every track together. The number is its loudest peak in the last few seconds.");
        // --- Header column: each lane's name ---

        // --- Video lane ---
        let video = l.video;
        let vhead = Rect::from_min_max(Pos2::new(outer.left(), video.top()), Pos2::new(lanes.left() - 8.0, video.bottom()));
        p.text(Pos2::new(vhead.left() + 16.0 + 8.0, vhead.center().y), Align2::CENTER_CENTER, egui_phosphor::regular::FILM_STRIP, FontId::proportional(15.0), pal.text3);
        p.text(Pos2::new(vhead.left() + 40.0, vhead.center().y), Align2::LEFT_CENTER, "Video", crate::theme::font(13.0, crate::theme::W::Medium), pal.text);
        lp.rect_filled(video, 5, pal.raise2);
        self.strip.paint(ui, video, v0, v0 + span, dur);
        // Scrub-proxy progress: a thin bar until every frame is scrubbable.
        crate::player::paint_proxy_bar(ui, &self.player, video, v0, v0 + span);

        // --- Sound lanes ---
        // Scrolled lanes are clipped to the space under the video lane.
        let ap = ui.painter_at(audio_view);
        let alp = ui.painter_at(lanes.intersect(audio_view));
        let lane_rects = &l.lane_rects;
        let labels: Vec<String> = self.info.source_tracks().iter().map(|a| a.label()).collect();
        for (i, &lane) in lane_rects.iter().enumerate() {
            if !lane.intersects(audio_view) {
                continue;
            }
            let track = &self.edit.tracks[i];
            let heard = !track.muted && self.solo.is_none_or(|s| s == i);
            alp.rect_filled(lane, 5, pal.raise);
            // Drawn with the volume curve applied: what you'll hear.
            if let Some(w) = self.waves.get(i) {
                let base = if heard { pal.wave_hi } else { crate::theme::fade(pal.text3, 0.5) };
                // The volume curve, mute and solo decide what's drawn: drawn anew when they change.
                let style = {
                    use std::hash::{Hash, Hasher};
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    serde_json::to_string(track).unwrap_or_default().hash(&mut h);
                    heard.hash(&mut h);
                    pal.dark.hash(&mut h);
                    h.finish()
                };
                w.paint(&alp, lane.shrink2(Vec2::new(0.0, 2.0)), (v0, span), style, |t| track.gain_at(t), |_, _| base);
            }
        }
        // Scrollbar, at the lanes' right edge, when they don't all fit.
        if max_scroll > 0.0 {
            // Clear of the end handle, which stands out past the lanes.
            let track = Rect::from_x_y_ranges(lanes.right() + 14.0..=lanes.right() + 18.0, audio_view.y_range());
            let view_h = audio_view.height();
            let thumb_h = (view_h / lanes_h * view_h).max(24.0);
            let thumb_y = track.top() + self.lane_scroll / max_scroll * (view_h - thumb_h);
            let bar = ui.interact(track.expand2(Vec2::new(3.0, 0.0)), ui.id().with("lane_scrollbar"), Sense::drag());
            if bar.dragged() {
                self.lane_scroll = (self.lane_scroll + bar.drag_delta().y / (view_h - thumb_h).max(1.0) * max_scroll).clamp(0.0, max_scroll);
            }
            let hv = bar.hovered() || bar.dragged();
            p.rect_filled(Rect::from_x_y_ranges(track.x_range(), thumb_y..=thumb_y + thumb_h), 2, if hv { pal.text3 } else { pal.control_h });
        }

        // --- What's kept: each part outlined, with handles at its ends; what's
        // cut away (trimmed off the ends, cut out between parts) dimmed ---
        let body = l.body;
        let parts = &l.parts;
        let n = parts.len();
        let xs = &l.xs;
        // A stretch just cut out fades to dim; one put back, from it.
        let now = ctx.input(|i| i.time);
        let fading = self.fading.filter(|f| now - f.since < FADE_S);
        let faded = fading.map_or(1.0, |f| crate::theme::ease_out(((now - f.since) / FADE_S) as f32));
        if fading.is_some() {
            ctx.request_repaint();
        }
        let gaps = std::iter::once((0.0, parts[0].0)).chain(parts.windows(2).map(|w| (w[0].1, w[1].0))).chain(std::iter::once((parts[n - 1].1, dur)));
        let dim = |a: f64, b: f64, k: f32| {
            let (xa, xb) = (x_of(a), x_of(b));
            if xb > xa {
                lp.rect_filled(Rect::from_x_y_ranges(xa..=xb, body.y_range()), 0, crate::theme::fade(pal.dim, k));
            }
        };
        for (a, b) in gaps {
            // (What's just been cut out of it, fading in: it may have joined
            // what was cut out either side.)
            match fading.filter(|f| f.cut && f.from >= a - 1e-6 && f.to <= b + 1e-6) {
                Some(f) => {
                    dim(a, f.from, 1.0);
                    dim(f.from, f.to, faded);
                    dim(f.to, b, 1.0);
                }
                None => dim(a, b, 1.0),
            }
        }
        if let Some(f) = fading.filter(|f| !f.cut) {
            lp.rect_filled(Rect::from_x_y_ranges(x_of(f.from)..=x_of(f.to), body.y_range()), 0, crate::theme::fade(pal.dim, 1.0 - faded));
        }
        // The outline shows once something's cut: before that the whole clip
        // is kept and a white frame round everything only adds noise.
        let trimmed = self.edit.start > fd * 0.5 || self.edit.end < dur - fd * 0.5 || n > 1;
        let outline = theme_fade(&ctx, "ed_trimmed", trimmed || self.dragging.is_some_and(|d| matches!(d, Drag::Start(_) | Drag::End(_))));
        let edge = crate::theme::mix(pal.text3, pal.text, outline);
        let hover_pos = ui.input(|i| i.pointer.hover_pos());
        // At a split the two parts' outlines stop short of each other.
        let shown_x = |k: usize| l.shown_x(k);
        for k in 0..n {
            let (xa, xb) = shown_x(k);
            let part = Rect::from_x_y_ranges(xa..=xb, body.y_range());
            crate::demo::mark(&ctx, || format!("part:{}", k + 1), part);
            // The part right-clicked: lit and framed while its menu is open,
            // so it's clear what the menu acts on.
            let chosen = theme_fade(&ctx, &format!("ed_sel{k}"), self.menu_at.is_some_and(|(s, _)| s == Spot::Part(k)));
            if chosen > 0.0 {
                // (In the light look, more would read as greyed out.)
                lp.rect_filled(part, 0, crate::theme::fade(pal.text, if pal.dark { 0.14 } else { 0.06 } * chosen));
                lp.rect_stroke(part.shrink(1.0), 3, Stroke::new(2.0, crate::theme::fade(pal.text, chosen)), egui::StrokeKind::Middle);
            }
            if outline > 0.0 {
                let line = Stroke::new(2.0, crate::theme::fade(pal.text, outline));
                lp.hline(part.x_range(), part.top() + 1.0, line);
                lp.hline(part.x_range(), part.bottom() - 1.0, line);
            }
        }
        // A split: the film cut through, a hair of the page between its
        // parts, and a grip on the picture's lane to pull it open with.
        // Handles and grips stand out past the lanes a little, never over
        // the lanes' names (zoomed in, an edge just out of view).
        let hp = ui.painter_at(lanes.expand2(Vec2::new(HANDLE_W + 3.0, 0.0)).intersect(outer));
        for &(r, d) in &l.splits {
            let Drag::Split(k) = d else { continue };
            let x = xs[k].1;
            let hv = theme_fade(&ctx, &format!("ed_split{k}"), self.dragging.is_none() && hover_pos.is_some_and(|p| r.contains(p)));
            lp.rect_filled(Rect::from_x_y_ranges(x - SPLIT_GAP..=x + SPLIT_GAP, body.y_range()), 0, pal.base);
            let grip = Rect::from_center_size(Pos2::new(x, video.center().y), Vec2::new(6.0 + 2.0 * hv, 26.0 + 6.0 * hv));
            hp.rect_filled(grip.expand(1.5), 4, pal.base);
            hp.rect_filled(grip, 3, crate::theme::mix(edge, pal.text, hv));
            crate::demo::mark(&ctx, || format!("split:{}", k + 1), r);
        }
        // Handles at each part's ends (see `Ready::layout`).
        let grips = &l.grips;
        if let Some((r, _)) = grips.first() {
            crate::demo::mark(&ctx, || "trim:in".into(), *r);
        }
        if let Some((r, _)) = grips.last() {
            crate::demo::mark(&ctx, || "trim:out".into(), *r);
        }
        let near_grip = |pos: Pos2| l.near_grip(pos);
        let hovered_grip = hover_pos.filter(|_| self.dragging.is_none()).and_then(near_grip);
        let vol_at = |pos: Pos2, this: &Self| l.vol_at(pos, &this.edit.tracks);
        // The volume line or point pointed at, or taken hold of.
        let vol_hot = match self.dragging {
            Some(d @ (Drag::Line(_) | Drag::Point(..))) => Some(d),
            Some(_) => None,
            None => hover_pos.and_then(|p| vol_at(p, self)),
        };
        // The volume lines, under the handles (a line is grabbed beside one).
        for (i, lane) in lane_rects.iter().enumerate() {
            if lane.intersects(audio_view) {
                let look = Look {
                    line: theme_fade(&ctx, &format!("ed_vline{i}"), vol_hot == Some(Drag::Line(i))),
                    point: match vol_hot {
                        Some(Drag::Point(j, k)) if j == i => Some(k),
                        _ => None,
                    },
                    held: matches!(self.dragging, Some(Drag::Point(j, _)) if j == i),
                    guide: self.vol_guide.filter(|(j, _)| *j == i).map(|(_, db)| db),
                };
                draw_envelope(&alp, pal, *lane, &self.edit.tracks[i], dur, x_of, look);
            }
        }
        for (h, drag) in grips {
            let (left, k) = match *drag {
                Drag::Start(k) => (true, k),
                Drag::End(k) => (false, k),
                _ => continue,
            };
            let active = self.dragging == Some(*drag) || hovered_grip == Some(*drag);
            let grow = theme_fade(&ctx, &format!("ed_h{k}{}", if left { "in" } else { "out" }), active);
            // Slim, so it never covers much; a pixel wider each side (its
            // notch stays in the middle) and brighter while it's pointed at
            // or pulled.
            let h = h.expand2(Vec2::new(grow, 0.0));
            let corners = if left { egui::CornerRadius { nw: 3, sw: 3, ne: 0, se: 0 } } else { egui::CornerRadius { nw: 0, sw: 0, ne: 3, se: 3 } };
            hp.rect_filled(h, corners, crate::theme::mix(edge, pal.text, grow));
            if h.width() >= 5.0 {
                hp.vline(h.center().x, h.center().y - 6.0..=h.center().y + 6.0, Stroke::new(1.5, crate::theme::fade(pal.base, 0.75)));
            }
        }
        // Each part's length, as a dimension line under the lanes; drag one
        // to slide its part along the clip, its length kept.
        let dim_y = l.dim_y;
        // What the timeline says where the pointer rests: on the picture's
        // lane, what a stretch cut out, or the part chosen, is for.
        let mut tip: Option<String> = None;
        match vol_hot.filter(|_| self.dragging.is_none()) {
            Some(Drag::Line(_)) => tip = Some("Drag to set the volume (Shift: finer). Double-click to add a point".into()),
            Some(Drag::Point(..)) => tip = Some("Drag to move it (Shift: finer). Double-click to take it away".into()),
            _ => {}
        }
        if let Some(pos) = hover_pos.filter(|p| n > 1 && self.dragging.is_none() && video.contains(*p) && lanes.contains(*p)) {
            let t = t_of(pos.x);
            if parts.windows(2).any(|w| t >= w[0].1 && t < w[1].0) {
                tip = Some("Cut out: playing skips it. Drag its edges, or right-click to put it back".into());
            }
        }
        let ranges = &l.ranges;
        for (k, range) in ranges.iter().enumerate().filter(|(_, r)| r.width() > 0.0) {
            let hovered = self.dragging.is_none() && hover_pos.is_some_and(|p| range.contains(p));
            let lit = self.dragging == Some(Drag::Range(k)) || self.menu_at.is_some_and(|(s, _)| s == Spot::Part(k)) || hovered;
            // A slim rounded bar under the part: brighter, a little thicker,
            // when pointed at or pulled.
            let hv = theme_fade(&ctx, &format!("ed_range{k}"), lit);
            let h = 3.0 + 2.0 * hv;
            let bar = Rect::from_x_y_ranges(range.left() + 1.0..=(range.right() - 1.0).max(range.left() + 1.0), dim_y - h / 2.0..=dim_y + h / 2.0);
            p.rect_filled(bar, h / 2.0, crate::theme::mix(pal.line2, pal.text2, hv));
            // In parts, each says how long it is.
            if hovered && n > 1 {
                let (a, b) = parts[k];
                let long = if self.show_frames { format!("{} frames", ((b - a) * self.info.fps).round() as u64) } else { crate::transport::clock(b - a) };
                tip = Some(format!("This part: {long}. Drag to slide it along the clip"));
            }
        }
        // How long it all is: the parts together. With one part, click it
        // to type another length (drawn last, after the timeline takes its
        // clicks, so it gets its own).
        let len = self.edit.duration();
        let len_text = if self.show_frames { format!("{} frames", (len * self.info.fps).round() as u64) } else { crate::transport::clock(len) };
        let span = Rect::from_x_y_ranges(ranges[0].left()..=ranges[n - 1].right().max(ranges[0].left()), dim_y - 9.0..=dim_y + 9.0);
        let len_label = {
            let g = p.layout_no_wrap(len_text.clone(), crate::theme::font(12.0, crate::theme::W::Semibold), pal.text);
            let more = if n > 1 { p.layout_no_wrap(format!("in {n} parts"), crate::theme::font(12.0, crate::theme::W::Regular), pal.text3).size().x + 5.0 } else { 0.0 };
            // (A little wider than the time: room for the caret as it's typed.)
            let w = if self.typing_length.is_some() { 90.0 } else { g.size().x + more + 22.0 };
            (w < span.width().max(110.0)).then(|| Rect::from_center_size(Pos2::new(span.center().x, dim_y), Vec2::new(w, 20.0)))
        };

        // The playhead, on the start of the frame showing, with that frame's
        // width marked once zoomed in far enough to see it.
        let frame_start = self.info.snap(self.player.time());
        let xp = x_of(frame_start);
        let frame_w = x_of(frame_start + fd) - xp;
        if frame_w >= 3.0 {
            lp.rect_filled(Rect::from_x_y_ranges(xp..=xp + frame_w, body.y_range()), 0, crate::theme::fade(pal.text, 0.07));
        }
        if xp >= lanes.left() - 1.0 && xp <= lanes.right() + 1.0 {
            crate::transport::playhead(&p, xp, ruler.bottom() - 4.0, body.bottom(), pal.text);
        }

        // --- Lane headers (drawn after the shading so they stay readable) ---
        let solo_before = self.solo;
        for (i, lane) in lane_rects.iter().enumerate() {
            if !lane.intersects(audio_view) {
                continue;
            }
            let head = Rect::from_min_max(Pos2::new(outer.left(), lane.top()), Pos2::new(lanes.left() - 8.0, lane.bottom()));
            let hresp = ui.interact(head, ui.id().with(("lane_head", i)), Sense::click());
            let name = labels.get(i).cloned().unwrap_or_default();
            let heard = !self.edit.tracks[i].muted && solo_before.is_none_or(|s| s == i);
            let track = &self.edit.tracks[i];
            let sub = if track.muted {
                "Muted".to_owned()
            } else if solo_before.is_some_and(|s| s != i) {
                "Not soloed".to_owned()
            } else if track.points.is_empty() {
                db_label(media::to_db(track.gain))
            } else if track.points.len() == 1 {
                "1 point".to_owned()
            } else {
                format!("{} points", track.points.len())
            };
            let glyph = lane_icon(&name);
            let tc = if heard { pal.text } else { pal.text3 };
            ap.text(Pos2::new(head.left() + 24.0, head.center().y), Align2::CENTER_CENTER, glyph, FontId::proportional(15.0), pal.text3);
            let name_g = ap.layout_job(crate::ui_kit::single_line_job(&name, crate::theme::font(13.0, crate::theme::W::Medium), tc, head.width() - 40.0 - 10.0));
            ap.galley(Pos2::new(head.left() + 40.0, head.center().y - 16.0), name_g, tc);
            // M and S take the second row's place while they show (the
            // name keeps all its room).
            ap.text(Pos2::new(head.left() + 40.0, head.center().y + 2.0), Align2::LEFT_TOP, sub, crate::theme::font(11.5, crate::theme::W::Regular), crate::theme::fade(pal.text3, 1.0 - shown_ms(&ctx, i)));
            // Its level: a thin bar on the header's right edge.
            let m = self.meters.get(i).copied().unwrap_or_default();
            let bar = Rect::from_min_max(Pos2::new(head.right() - 3.0, head.top() + 8.0), Pos2::new(head.right(), head.bottom() - 8.0));
            ap.rect_filled(bar, 2, pal.meter_bg);
            if heard && m.rms > 0.0 {
                let f = ((media::to_db(m.rms).max(METER_FLOOR) - METER_FLOOR) / -METER_FLOOR).clamp(0.0, 1.0);
                ap.rect_filled(Rect::from_min_max(Pos2::new(bar.left(), bar.bottom() - bar.height() * f), bar.max), 2, zone(pal, m.rms));
            }
            // M and S, on hover (kept while on).
            let shown = theme_fade(&ctx, &format!("ed_ms{i}"), hresp.hovered() || track.muted || solo_before == Some(i) || hover_pos.is_some_and(|p| head.contains(p)));
            ctx.data_mut(|d| d.insert_temp(egui::Id::new(("ed_ms_shown", i)), shown));
            if shown > 0.0 {
                let s_rect = Rect::from_min_size(Pos2::new(head.left() + 40.0 + 25.0, head.center().y + 1.0), Vec2::new(22.0, 18.0));
                let m_rect = Rect::from_min_size(Pos2::new(head.left() + 40.0, head.center().y + 1.0), Vec2::new(22.0, 18.0));
                let put = |rect: Rect, label: &str, on: bool, solo: bool, tip: &str| -> bool {
                    let r = ui.interact(rect, ui.id().with((label, i)), Sense::click());
                    if !rect.intersects(audio_view) {
                        return false;
                    }
                    let (fill, fg) = match (on, solo) {
                        (true, false) => (pal.warn, Color32::from_rgb(20, 21, 24)),
                        (true, true) => (pal.text, pal.base),
                        _ => (if r.hovered() { pal.control_h } else { pal.control }, pal.text2),
                    };
                    ap.rect_filled(rect, 4, crate::theme::fade(fill, shown));
                    ap.text(rect.center(), Align2::CENTER_CENTER, label, crate::theme::font(10.5, crate::theme::W::Bold), crate::theme::fade(fg, shown));
                    r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip).clicked()
                };
                let muted = track.muted;
                if put(m_rect, "M", muted, false, if muted { "Unmute" } else { "Mute" }) {
                    self.push_undo(self.edit.clone());
                    self.edit.tracks[i].muted = !muted;
                }
                if put(s_rect, "S", solo_before == Some(i), true, "Solo: hear only this track (not saved)") {
                    self.solo = if solo_before == Some(i) { None } else { Some(i) };
                }
            }
            // Right-click: the volume (where the playhead is) to type, mute,
            // solo, reset the volume.
            let track = &self.edit.tracks[i];
            let (muted, flat) = (track.muted, track.points.is_empty() && (track.gain - 1.0).abs() < 0.005);
            let has_points = !track.points.is_empty();
            let now_t = self.player.time();
            let shown = crate::ui_kit::db_edit_text(track.db_at(now_t), media::SILENT_DB);
            let mut pick = None;
            let mut typed = None;
            crate::ui_kit::context_menu_typed(&hresp, 220.0, |ui| {
                use crate::ui_kit::Item;
                use egui_phosphor::regular as icon;
                typed = crate::ui_kit::menu_db_field(ui, "Volume", &shown);
                crate::ui_kit::menu_sep(ui);
                if Item::new(if muted { "Unmute" } else { "Mute" }).icon(icon::SPEAKER_X).show(ui).clicked() {
                    pick = Some(0);
                }
                if Item::new(if solo_before == Some(i) { "Stop soloing" } else { "Solo" }).icon(icon::HEADPHONES).show(ui).clicked() {
                    pick = Some(1);
                }
                crate::ui_kit::menu_sep(ui);
                if Item::new("Reset volume to 0 dB").icon(icon::ARROW_COUNTER_CLOCKWISE).enabled(!flat).show(ui).clicked() {
                    pick = Some(2);
                }
                if has_points && Item::new("Remove volume points").icon(icon::X_CIRCLE).show(ui).clicked() {
                    pick = Some(3);
                }
            });
            if let Some(db) = typed {
                self.set_level_at(i, now_t, db);
            }
            match pick {
                Some(0) => {
                    self.push_undo(self.edit.clone());
                    self.edit.tracks[i].muted = !muted;
                }
                Some(1) => self.solo = if solo_before == Some(i) { None } else { Some(i) },
                Some(2) => {
                    self.push_undo(self.edit.clone());
                    self.edit.tracks[i].points.clear();
                    self.edit.tracks[i].gain = 1.0;
                }
                Some(3) => {
                    self.push_undo(self.edit.clone());
                    let db = self.edit.tracks[i].db_at(self.player.time());
                    self.edit.tracks[i].points.clear();
                    self.edit.tracks[i].gain = media::from_db(db);
                }
                _ => {}
            }
        }

        // --- The pointer: what's under it, what it holds (what it does to
        // them was read first thing, see `Ready::timeline_input`) ---
        let pointer = resp.as_ref().and_then(egui::Response::hover_pos);
        if let Some(label) = len_label {
            if n == 1 {
                self.length_field(ui, label, &len_text);
            } else {
                // In parts, it's what they come to together.
                let g = p.layout_no_wrap(len_text.clone(), crate::theme::font(12.0, crate::theme::W::Semibold), pal.text);
                let more = p.layout_no_wrap(format!("in {n} parts"), crate::theme::font(12.0, crate::theme::W::Regular), pal.text3);
                p.rect_filled(label, 5, pal.base);
                let x = label.center().x - (g.size().x + 5.0 + more.size().x) / 2.0;
                p.galley(Pos2::new(x, label.center().y - g.size().y / 2.0), g.clone(), pal.text);
                p.galley(Pos2::new(x + g.size().x + 5.0, label.center().y - more.size().y / 2.0), more, pal.text3);
                if pointer.is_some_and(|p| label.contains(p)) {
                    tip = Some("How long the saved clip is: the parts kept, together".into());
                }
            }
        }
        let resp = resp.map(|resp| match tip.filter(|_| self.dragging.is_none()) {
            Some(tip) => resp.on_hover_text(tip),
            None => resp,
        });
        // (A click on the length is its own; a drag that starts on it is
        // the timeline's, sliding the kept part: egui gives each its widget.)

        // The cursor says what a drag takes hold of; while dragging, what's
        // held (wherever the pointer has gone).
        let held = self.dragging.filter(|_| resp.as_ref().is_some_and(egui::Response::dragged));
        if let Some(d) = held.or_else(|| pointer.and_then(|pos| l.hit(pos, &self.edit.tracks))) {
            let icon = match d {
                Drag::Start(_) | Drag::End(_) | Drag::Split(_) => Some(egui::CursorIcon::ResizeHorizontal),
                Drag::Range(_) | Drag::Point(..) => Some(if held.is_some() { egui::CursorIcon::Grabbing } else { egui::CursorIcon::Grab }),
                Drag::Line(_) => Some(egui::CursorIcon::ResizeVertical),
                _ => None,
            };
            if let Some(icon) = icon {
                ui.ctx().set_cursor_icon(icon);
            }
        }
        if let Some(pos) = pointer {
            // Hover: a line through the lanes, and the volume on a lane.
            if self.dragging.is_none() && self.menu_at.is_none() && lanes.contains(pos) && pos.y > ruler.bottom() {
                if let Some(i) = lane_rects.iter().position(|l| l.contains(pos) && audio_view.contains(pos)) {
                    let t = self.info.snap(t_of(pos.x));
                    readout(&p, pos, db_label(self.edit.tracks[i].db_at(t)));
                }
            }
        }
        // Dragging the volume: the level as it goes (a point's time too).
        if let Some(pos) = resp.as_ref().and_then(egui::Response::interact_pointer_pos) {
            let text = match self.dragging {
                Some(Drag::Line(i)) => self.edit.tracks.get(i).map(|t| db_label(t.db_at(self.vol_grab))),
                Some(Drag::Point(i, k)) => self.edit.tracks.get(i).and_then(|t| t.points.get(k)).map(|pt| format!("{}  ·  {}", db_label(pt.db), crate::transport::clock(pt.t))),
                _ => None,
            };
            if let Some(text) = text {
                readout(&p, pos, text);
            }
        }

        if let (Some((spot, t)), Some(resp)) = (self.menu_at, resp.as_ref()) {
            enum Act {
                Split,
                CutOut(usize),
                PutBack,
                AddPoint(usize),
                Level(usize, f32),
                Flatten(usize),
                Mute(usize),
                RemovePoint(usize, usize),
                PointLevel(usize, usize, f32),
                Typed(usize, f32),
            }
            let can_split = self.split_at(t).is_some();
            let at_playhead = self.part_at(self.player.time());
            let vol_t = self.info.snap(t);
            let mut act = None;
            // A volume menu has a field to type it into, which a click
            // into mustn't close.
            let typing = matches!(spot, Spot::Volume(_) | Spot::Point(..));
            let body = |ui: &mut egui::Ui| {
                use crate::ui_kit::Item;
                use egui_phosphor::regular as icon;
                match spot {
                    Spot::Part(k) => {
                        if Item::new("Split here").icon(icon::SCISSORS).enabled(can_split).show(ui).clicked() {
                            act = Some(Act::Split);
                        }
                        // Delete does it too, to the part the playhead's in.
                        let item = Item::new("Cut out this part").icon(icon::TRASH).keys((at_playhead == Some(k)).then(|| vec!["Delete".into()])).enabled(n > 1);
                        let item = if n > 1 { item } else { item.tip("Split the clip first, then cut out the part you don't want") };
                        if item.show(ui).clicked() {
                            act = Some(Act::CutOut(k));
                        }
                    }
                    Spot::Split(_) => {
                        if Item::new("Join the parts").icon(icon::ARROWS_IN_LINE_HORIZONTAL).tip("Take the split away").show(ui).clicked() {
                            act = Some(Act::PutBack);
                        }
                    }
                    Spot::Gap(_) | Spot::Before | Spot::After => {
                        if Item::new("Put back").icon(icon::ARROW_COUNTER_CLOCKWISE).tip("Keep this part of the clip again").show(ui).clicked() {
                            act = Some(Act::PutBack);
                        }
                    }
                    Spot::Volume(i) => {
                        let Some(track) = self.edit.tracks.get(i) else { return };
                        let flat = track.points.is_empty() && (track.gain - 1.0).abs() < 0.005;
                        // Here, on a curve: it all goes up or down to it.
                        let shown = crate::ui_kit::db_edit_text(track.db_at(vol_t), media::SILENT_DB);
                        if let Some(db) = crate::ui_kit::menu_db_field(ui, if track.points.is_empty() { "Volume" } else { "Volume here" }, &shown) {
                            act = Some(Act::Typed(i, db));
                        }
                        crate::ui_kit::menu_sep(ui);
                        if Item::new("Add a point here").icon(icon::PLUS).keys(Some(vec!["Double-click".into()])).show(ui).clicked() {
                            act = Some(Act::AddPoint(i));
                        }
                        if Item::new("Reset to 0 dB").icon(icon::ARROW_COUNTER_CLOCKWISE).enabled(!flat).show(ui).clicked() {
                            act = Some(Act::Level(i, 0.0));
                        }
                        if !track.points.is_empty() && Item::new("Remove all points").icon(icon::X_CIRCLE).tip("Keeps the level the playhead is at").show(ui).clicked() {
                            act = Some(Act::Flatten(i));
                        }
                        crate::ui_kit::menu_sep(ui);
                        if Item::new(if track.muted { "Unmute" } else { "Mute" }).icon(icon::SPEAKER_X).show(ui).clicked() {
                            act = Some(Act::Mute(i));
                        }
                    }
                    Spot::Point(i, k) => {
                        if let Some(pt) = self.edit.tracks.get(i).and_then(|t| t.points.get(k)) {
                            let shown = crate::ui_kit::db_edit_text(pt.db, media::SILENT_DB);
                            if let Some(db) = crate::ui_kit::menu_db_field(ui, "Volume", &shown) {
                                act = Some(Act::PointLevel(i, k, (db.clamp(media::SILENT_DB, media::FADER_MAX_DB) * 10.0).round() / 10.0));
                            }
                            crate::ui_kit::menu_sep(ui);
                        }
                        if Item::new("Set to 0 dB").icon(icon::ARROW_COUNTER_CLOCKWISE).show(ui).clicked() {
                            act = Some(Act::PointLevel(i, k, 0.0));
                        }
                        if Item::new("Remove this point").icon(icon::TRASH).keys(Some(vec!["Double-click".into()])).show(ui).clicked() {
                            act = Some(Act::RemovePoint(i, k));
                        }
                    }
                }
            };
            let open = if typing { crate::ui_kit::context_menu_typed(resp, 220.0, body) } else { crate::ui_kit::context_menu(resp, 220.0, body) };
            // Closed (picked from, or clicked away): its part isn't lit anymore.
            if open.is_none() {
                self.menu_at = None;
            }
            let now = ctx.input(|i| i.time);
            match act {
                Some(Act::Split) => self.split(t),
                Some(Act::CutOut(k)) => self.cut_out(k, now),
                Some(Act::PutBack) => self.put_back(spot, now),
                Some(Act::AddPoint(i)) => self.add_point(i, self.info.snap(t)),
                Some(Act::Level(i, db)) => {
                    self.push_undo(self.edit.clone());
                    let track = &mut self.edit.tracks[i];
                    track.points.clear();
                    track.gain = media::from_db(db);
                }
                Some(Act::Flatten(i)) => {
                    self.push_undo(self.edit.clone());
                    let db = self.edit.tracks[i].db_at(self.player.time());
                    let track = &mut self.edit.tracks[i];
                    track.points.clear();
                    track.gain = media::from_db(db);
                }
                Some(Act::Mute(i)) => {
                    self.push_undo(self.edit.clone());
                    self.edit.tracks[i].muted = !self.edit.tracks[i].muted;
                }
                Some(Act::RemovePoint(i, k)) => self.remove_point(i, k),
                Some(Act::Typed(i, db)) => self.set_level_at(i, vol_t, db),
                Some(Act::PointLevel(i, k, db)) => {
                    if let Some(pt) = self.edit.tracks.get(i).and_then(|t| t.points.get(k)).copied() {
                        self.push_undo(self.edit.clone());
                        self.edit.tracks[i].points[k] = media::VolumePoint { t: pt.t, db };
                    }
                }
                None => {}
            }
        }
    }
}

/// How shown lane `i`'s M and S are (they take room from its name).
fn shown_ms(ctx: &egui::Context, i: usize) -> f32 {
    ctx.data(|d| d.get_temp::<f32>(egui::Id::new(("ed_ms_shown", i)))).unwrap_or(0.0)
}

/// 0 to 1 as `on`, eased: a stable id per use.
fn theme_fade(ctx: &egui::Context, id: &str, on: bool) -> f32 {
    crate::theme::toggle(ctx, egui::Id::new(id), on, crate::theme::FAST)
}

/// A lane's icon, from its name.
fn lane_icon(name: &str) -> &'static str {
    let n = name.to_lowercase();
    use egui_phosphor::regular as icon;
    if n.contains("mic") {
        icon::MICROPHONE
    } else if n.contains("discord") {
        icon::DISCORD_LOGO
    } else if n.contains("mix") {
        icon::WAVEFORM
    } else if n.contains("desktop") || n.contains("game") || n.contains("system") {
        icon::SPEAKER_HIFI
    } else {
        icon::WAVEFORM
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Drag {
    /// The start or the end of part k.
    Start(usize),
    End(usize),
    /// The split after part k, grabbed: pulled open which ever way it's
    /// dragged (moving the start of the part after it, or the end of k).
    Split(usize),
    /// Part k, slid along, its length kept.
    Range(usize),
    Playhead,
    /// Whole volume line of track i.
    Line(usize),
    /// Keyframe k of track i.
    Point(usize, usize),
}

/// The handles at the parts' ends: this wide, outside them.
const HANDLE_W: f32 = 6.0;
/// At a split the two parts' outlines stop this short of each other: the
/// film is cut there.
const SPLIT_GAP: f32 = 2.0;

/// Where everything on the timeline is, for a frame: laid out from the
/// space it has, the view, the sound lanes' scroll and the edit as they are
/// then. What the pointer does is read against where things were drawn last
/// frame, first thing (see [`Ready::timeline_input`]); then they're laid out
/// again where they're drawn, with what the pointer moved.
struct Layout {
    /// On the time axis, the ruler's top to the last sound lane's bottom.
    lanes: Rect,
    ruler: Rect,
    video: Rect,
    /// Where the sound lanes show (they scroll in it).
    audio_view: Rect,
    /// The mix's meter, on the right.
    meter_col: Rect,
    /// From the video lane down through the sound lanes: what the parts span.
    body: Rect,
    lane_rects: Vec<Rect>,
    /// The clip's length, and the part of it shown (start, length).
    dur: f64,
    view: (f64, f64),
    parts: Vec<(f64, f64)>,
    /// Where each part starts and ends on screen.
    xs: Vec<(f32, f32)>,
    splits: Vec<(Rect, Drag)>,
    grips: Vec<(Rect, Drag)>,
    /// Each part's length, as a bar on the line under the lanes (`dim_y`).
    ranges: Vec<Rect>,
    dim_y: f32,
    /// Where the pointer takes hold of the timeline.
    hit_area: Rect,
}

impl Layout {
    fn x_of(&self, t: f64) -> f32 {
        let (v0, span) = self.view;
        self.lanes.left() + ((t - v0) / span) as f32 * self.lanes.width()
    }

    fn t_of(&self, x: f32) -> f64 {
        let (v0, span) = self.view;
        (v0 + ((x - self.lanes.left()) / self.lanes.width()).clamp(0.0, 1.0) as f64 * span).clamp(0.0, self.dur)
    }

    fn px_per_s(&self) -> f64 {
        self.lanes.width() as f64 / self.view.1.max(1e-9)
    }

    /// Part `k` and the next touch: split, nothing cut out between them.
    fn split(&self, k: usize) -> bool {
        k + 1 < self.parts.len() && self.parts[k + 1].0 - self.parts[k].1 < 1e-6
    }

    /// Where part `k`'s outline starts and ends: short of a split.
    fn shown_x(&self, k: usize) -> (f32, f32) {
        let (xa, xb) = self.xs[k];
        (if k > 0 && self.split(k - 1) { xa + SPLIT_GAP } else { xa }, if self.split(k) { xb - SPLIT_GAP } else { xb })
    }

    /// The handle nearest `pos`, if it's on one (or almost).
    fn near_grip(&self, pos: Pos2) -> Option<Drag> {
        self.grips.iter().filter(|(r, _)| r.expand(5.0).contains(pos)).min_by(|a, b| (a.0.center().x - pos.x).abs().total_cmp(&(b.0.center().x - pos.x).abs())).map(|g| g.1)
    }

    /// A volume point under the pointer, else its lane's line (unless a
    /// handle is nearer): what a drag there takes hold of.
    fn vol_at(&self, pos: Pos2, tracks: &[media::TrackEdit]) -> Option<Drag> {
        let (i, lane) = self.lane_rects.iter().enumerate().find(|(_, l)| l.contains(pos) && self.audio_view.contains(pos))?;
        let track = &tracks[i];
        if let Some(k) = track.points.iter().position(|pt| Pos2::new(self.x_of(pt.t), y_of_db(*lane, pt.db)).distance(pos) < 8.0) {
            return Some(Drag::Point(i, k));
        }
        let on_line = (pos.y - y_of_db(*lane, track.db_at(self.t_of(pos.x)))).abs() < 6.0;
        (on_line && self.near_grip(pos).is_none()).then_some(Drag::Line(i))
    }

    /// What's under the pointer, in priority order.
    fn hit(&self, pos: Pos2, tracks: &[media::TrackEdit]) -> Option<Drag> {
        // A volume point over a handle is the point's.
        let vol = self.vol_at(pos, tracks);
        if let Some(d @ Drag::Point(..)) = vol {
            return Some(d);
        }
        // The nearest handle, then a split, then a part's length, then a
        // volume line.
        if let Some(d) = self.near_grip(pos) {
            return Some(d);
        }
        if let Some(&(_, d)) = self.splits.iter().find(|(r, _)| r.contains(pos)) {
            return Some(d);
        }
        if let Some(k) = self.ranges.iter().position(|r| r.width() > 0.0 && r.expand2(Vec2::new(0.0, 4.0)).contains(pos)) {
            return Some(Drag::Range(k));
        }
        vol
    }
}

/// The bar between the picture and the timeline: drag it to give either more
/// room. `share` is the picture's fraction of `total`.
fn divider(ui: &mut egui::Ui, preview_h: f32, total: f32, share: &mut f32) {
    let pal = crate::theme::pal(ui);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), DIVIDER_H), Sense::drag());
    let resp = resp.on_hover_cursor(egui::CursorIcon::ResizeVertical).on_hover_text("Drag to resize the picture and the timeline");
    if resp.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
        *share = ((preview_h + resp.drag_delta().y) / total.max(1.0)).clamp(0.1, 0.95);
    }
    let active = theme_fade(ui.ctx(), "ed_divider", resp.hovered() || resp.dragged());
    let grip = Rect::from_center_size(rect.center(), Vec2::new(36.0 + 8.0 * active, 3.0));
    ui.painter().rect_filled(grip, 2, crate::theme::mix(pal.line2, pal.text3, active));
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

fn same_edit(a: &Edit, b: &Edit) -> bool {
    a.output == b.output
        && (a.start - b.start).abs() < 1e-6
        && (a.end - b.end).abs() < 1e-6
        && a.cuts.len() == b.cuts.len()
        && a.cuts.iter().zip(&b.cuts).all(|(x, y)| (x.from - y.from).abs() < 1e-6 && (x.to - y.to).abs() < 1e-6)
        && a.tracks.len() == b.tracks.len()
        && a.tracks.iter().zip(&b.tracks).all(|(x, y)| {
            x.muted == y.muted && (x.gain - y.gain).abs() < 1e-4 && x.points == y.points
        })
}

/// The kept part (`start`..`end`) made `secs` long around its middle; where
/// that runs past an end of the clip, it shifts back in (as long as the clip
/// allows). On frames, at least one long.
fn trim_to_length(info: &media::ClipInfo, start: f64, end: f64, secs: f64) -> (f64, f64) {
    let fd = info.frame_duration();
    let secs = secs.clamp(fd, info.duration);
    let mid = (start + end) / 2.0;
    let mut a = mid - secs / 2.0;
    if a < 0.0 {
        a = 0.0;
    }
    if a + secs > info.duration {
        a = info.duration - secs;
    }
    let a = info.snap(a.max(0.0));
    (a, (info.snap(a + secs)).min(info.duration).max(a + fd))
}

/// The playhead at `t` (in the recording), moved into what's kept: where it
/// is inside a part, else the next part's start (or the last part's last
/// frame, past them all).
fn settle(parts: &[(f64, f64)], t: f64, fd: f64) -> f64 {
    for &(a, b) in parts {
        if t < b - fd * 0.5 {
            return t.max(a);
        }
    }
    parts.last().map_or(t, |&(a, b)| (b - fd).max(a))
}

/// The part of `parts` a split at `t` (a frame's start) splits: one with a
/// frame or more either side of it.
fn splits(parts: &[(f64, f64)], t: f64, fd: f64) -> Option<usize> {
    parts.iter().position(|&(a, b)| t >= a + fd - 1e-9 && t <= b - fd + 1e-9)
}

/// Start here (I) at `t` (a frame's start): the part `t` is in starts there;
/// in what's cut out or trimmed off before a part, that part starts there
/// (taking some of it back). A part keeps a frame at least. `None`: nothing
/// changes.
fn start_at(parts: &[(f64, f64)], t: f64, fd: f64) -> Option<Vec<(f64, f64)>> {
    let k = parts.iter().position(|&(_, b)| t < b)?;
    let mut parts = parts.to_vec();
    (t < parts[k].1 - fd + 1e-9 && (t - parts[k].0).abs() > 1e-9).then(|| {
        parts[k].0 = t;
        parts
    })
}

/// End here (O) at `t` (the end of the frame at the playhead): the part
/// that frame is in ends there; in what's cut out or trimmed off after a
/// part, that part ends there. A part keeps a frame at least.
fn end_at(parts: &[(f64, f64)], t: f64, fd: f64) -> Option<Vec<(f64, f64)>> {
    let k = parts.iter().rposition(|&(a, _)| t > a)?;
    let mut parts = parts.to_vec();
    (t > parts[k].0 + fd - 1e-9 && (t - parts[k].1).abs() > 1e-9).then(|| {
        parts[k].1 = t;
        parts
    })
}

/// `parts` with what's at `spot` kept again, and the stretch put back: what
/// was cut out between two parts (or the split between them, nothing) makes
/// them one; what was trimmed off the start or the end of a clip
/// `duration` long comes back.
fn put_back(parts: &[(f64, f64)], spot: Spot, duration: f64) -> Option<(Vec<(f64, f64)>, (f64, f64))> {
    let mut parts = parts.to_vec();
    let n = parts.len();
    let back = match spot {
        Spot::Gap(k) | Spot::Split(k) if k + 1 < n => {
            let gap = (parts[k].1, parts[k + 1].0);
            parts[k].1 = parts[k + 1].1;
            parts.remove(k + 1);
            gap
        }
        Spot::Before if parts[0].0 > 0.0 => {
            let gap = (0.0, parts[0].0);
            parts[0].0 = 0.0;
            gap
        }
        Spot::After if parts[n - 1].1 < duration => {
            let gap = (parts[n - 1].1, duration);
            parts[n - 1].1 = duration;
            gap
        }
        _ => return None,
    };
    Some((parts, back))
}

/// The volume line, striped (so it reads as a control over the waveform, not
/// part of it): faint where it's unchanged, bright with its points as
/// diamonds where it's been changed.
/// How a volume drag that comes this close (points) to a level it snaps to
/// lands on it.
const SNAP_PX: f32 = 5.0;

/// How a lane's volume line looks this frame: how much it's pointed at (0 to
/// 1), the point pointed at or held, whether that point is held, and a level
/// it's snapped to (a guide marks it).
#[derive(Clone, Copy, Default)]
struct Look {
    line: f32,
    point: Option<usize>,
    held: bool,
    guide: Option<f32>,
}

fn draw_envelope(p: &egui::Painter, pal: &crate::theme::Pal, lane: Rect, track: &media::TrackEdit, dur: f64, x_of: impl Fn(f64) -> f32, look: Look) {
    let flat = track.points.is_empty() && (track.gain - 1.0).abs() < 0.005;
    // Amber: apart from the waveform's blue-grey and the edit's white.
    let amber = if pal.dark { Color32::from_rgb(255, 196, 72) } else { Color32::from_rgb(200, 118, 0) };
    let color = if track.muted { pal.text3 } else { amber };
    if let Some(db) = look.guide {
        p.hline(lane.x_range(), y_of_db(lane, db), Stroke::new(1.0, crate::theme::fade(color, 0.45)));
    }
    let mut pts = vec![Pos2::new(x_of(0.0), y_of_db(lane, track.db_at(0.0)))];
    pts.extend(track.points.iter().map(|pt| Pos2::new(x_of(pt.t), y_of_db(lane, pt.db))));
    pts.push(Pos2::new(x_of(dur), y_of_db(lane, track.db_at(dur))));
    // Unchanged, it's faint: something to take hold of, not to read. Set,
    // it's clear, on a dark edge that keeps it readable over the waveform.
    // Pointed at, it's brighter, a little thicker.
    let strength = if flat { 0.3 + 0.6 * look.line } else { 0.9 + 0.1 * look.line };
    let width = 1.5 + 0.5 * look.line;
    let edge = if flat { look.line } else { 1.0 };
    if edge > 0.0 {
        p.add(egui::Shape::line(pts.clone(), Stroke::new(width + 3.0, crate::theme::fade(pal.base, 0.7 * edge))));
    }
    p.add(egui::Shape::line(pts, Stroke::new(width, crate::theme::fade(color, strength))));
    for (k, pt) in track.points.iter().enumerate() {
        let c = Pos2::new(x_of(pt.t), y_of_db(lane, pt.db));
        let hot = look.point == Some(k);
        let d = if hot { 6.0 } else { 4.5 };
        let diamond = vec![c + Vec2::new(0.0, -d), c + Vec2::new(d, 0.0), c + Vec2::new(0.0, d), c + Vec2::new(-d, 0.0)];
        p.add(egui::Shape::convex_polygon(diamond, color, Stroke::new(1.5, pal.base)));
        if hot && look.held {
            p.circle_stroke(c, d + 4.0, Stroke::new(1.5, crate::theme::fade(color, 0.45)));
        }
    }
}

/// A small dark label just above and right of `at`: a level, a time.
fn readout(p: &egui::Painter, at: Pos2, text: String) {
    let g = p.layout_no_wrap(text, crate::theme::font(11.0, crate::theme::W::Semibold), Color32::WHITE);
    let r = Rect::from_min_size(Pos2::new(at.x + 8.0, at.y - 22.0), g.size() + Vec2::new(10.0, 4.0));
    p.rect_filled(r, 4, Color32::from_black_alpha(184));
    p.galley(r.min + Vec2::new(5.0, 2.0), g, Color32::WHITE);
}

/// Meter colour zones: green to −12 dB, yellow to −3 dB, red above.
fn zone(pal: &crate::theme::Pal, lin: f32) -> Color32 {
    let db = media::to_db(lin);
    if db >= -3.0 {
        pal.meter_r
    } else if db >= -12.0 {
        pal.meter_y
    } else {
        pal.meter_g
    }
}

#[cfg(test)]
mod length_tests {
    use super::*;

    fn info() -> media::ClipInfo {
        media::ClipInfo { duration: 60.0, fps: 60.0, width: 1920, height: 1080, video_kbps: None, audio: Vec::new(), id: None, bt601: false }
    }

    #[test]
    fn typed_length_grows_from_the_middle() {
        // 20..30 (middle 25) made 20 long: 15..35.
        assert_eq!(trim_to_length(&info(), 20.0, 30.0, 20.0), (15.0, 35.0));
        // Made shorter: 23..27.
        assert_eq!(trim_to_length(&info(), 20.0, 30.0, 4.0), (23.0, 27.0));
        // Past the clip's end: shifted back in.
        assert_eq!(trim_to_length(&info(), 50.0, 58.0, 20.0), (40.0, 60.0));
        // Past its start: shifted forward.
        assert_eq!(trim_to_length(&info(), 0.0, 4.0, 10.0), (0.0, 10.0));
        // Longer than the whole clip: all of it.
        assert_eq!(trim_to_length(&info(), 5.0, 10.0, 90.0), (0.0, 60.0));
    }
}

#[cfg(test)]
mod part_tests {
    use super::*;

    /// A frame at 10 fps, so times read plainly.
    const FD: f64 = 0.1;
    /// Kept: 1–4 s and 6–9 s of a 10 s clip.
    const PARTS: [(f64, f64); 2] = [(1.0, 4.0), (6.0, 9.0)];

    #[test]
    fn splitting_keeps_a_frame_either_side() {
        assert_eq!(splits(&PARTS, 2.5, FD), Some(0));
        assert_eq!(splits(&PARTS, 7.0, FD), Some(1));
        // At a part's first frame there's nothing before it to split off; a
        // frame later there is. Its last frame can start the second part.
        assert_eq!(splits(&PARTS, 6.0, FD), None);
        assert_eq!(splits(&PARTS, 6.1, FD), Some(1));
        assert_eq!(splits(&PARTS, 8.9, FD), Some(1));
        // Not in what's cut out or trimmed off.
        assert_eq!(splits(&PARTS, 5.0, FD), None);
        assert_eq!(splits(&PARTS, 0.5, FD), None);
    }

    /// Opening the editor from the preview: the playhead lands in what's kept,
    /// wherever the preview was.
    #[test]
    fn the_playhead_settles_in_what_is_kept() {
        // In a kept part: where it was.
        assert_eq!(settle(&PARTS, 2.5, FD), 2.5);
        assert_eq!(settle(&PARTS, 3.9, FD), 3.9);
        // In what's cut out: the next part starts there.
        assert_eq!(settle(&PARTS, 5.0, FD), 6.0);
        // Before the first part or after the last: its first frame, its last.
        assert_eq!(settle(&PARTS, 0.2, FD), 1.0);
        assert!((settle(&PARTS, 9.5, FD) - 8.9).abs() < 1e-9);
        assert!((settle(&PARTS, 8.9, FD) - 8.9).abs() < 1e-9);
    }

    #[test]
    fn start_and_end_here_act_on_the_part_at_the_playhead() {
        // In a part: that part starts (ends) there.
        assert_eq!(start_at(&PARTS, 7.0, FD), Some(vec![(1.0, 4.0), (7.0, 9.0)]));
        assert_eq!(end_at(&PARTS, 2.0, FD), Some(vec![(1.0, 2.0), (6.0, 9.0)]));
        // In what's cut out: the part after it starts sooner, the one before
        // ends later, taking some of it back.
        assert_eq!(start_at(&PARTS, 5.0, FD), Some(vec![(1.0, 4.0), (5.0, 9.0)]));
        assert_eq!(end_at(&PARTS, 5.0, FD), Some(vec![(1.0, 5.0), (6.0, 9.0)]));
        // Trimmed off: the clip starts sooner or ends later, as ever.
        assert_eq!(start_at(&PARTS, 0.5, FD), Some(vec![(0.5, 4.0), (6.0, 9.0)]));
        assert_eq!(end_at(&PARTS, 9.5, FD), Some(vec![(1.0, 4.0), (6.0, 9.5)]));
        // Nothing to start after the end, or to end before the start.
        assert_eq!(start_at(&PARTS, 9.5, FD), None);
        assert_eq!(end_at(&PARTS, 0.5, FD), None);
        // A part keeps a frame: its last frame can start it, no later.
        assert_eq!(start_at(&PARTS, 3.9, FD), Some(vec![(3.9, 4.0), (6.0, 9.0)]));
        // Where it already starts or ends: nothing to do.
        assert_eq!(start_at(&PARTS, 6.0, FD), None);
        assert_eq!(end_at(&PARTS, 4.0, FD), None);
    }

    #[test]
    fn putting_back() {
        assert_eq!(put_back(&PARTS, Spot::Gap(0), 10.0), Some((vec![(1.0, 9.0)], (4.0, 6.0))));
        assert_eq!(put_back(&PARTS, Spot::Before, 10.0), Some((vec![(0.0, 4.0), (6.0, 9.0)], (0.0, 1.0))));
        assert_eq!(put_back(&PARTS, Spot::After, 10.0), Some((vec![(1.0, 4.0), (6.0, 10.0)], (9.0, 10.0))));
        // A split taken away joins its parts.
        assert_eq!(put_back(&[(0.0, 5.0), (5.0, 10.0)], Spot::Split(0), 10.0), Some((vec![(0.0, 10.0)], (5.0, 5.0))));
        // Nothing trimmed off: nothing to put back.
        assert_eq!(put_back(&[(0.0, 10.0)], Spot::Before, 10.0), None);
    }
}
