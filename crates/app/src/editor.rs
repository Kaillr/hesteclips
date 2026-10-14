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
use crate::player::Player;
use crate::store::{self, EditTarget};

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
                // Every track at once, playable as it's decoded; the
                // waveforms follow once it's all there.
                let tracks: Vec<usize> = info.source_tracks().iter().map(|t| t.index).collect();
                if crate::player::debug() {
                    eprintln!("{:>8.3} editor: clip read, decoding {} tracks", crate::player::uptime(), tracks.len());
                }
                let pcm = media::pcm::decode_streaming(&src, &tracks, info.duration, |_, result| {
                    if let Err(e) = result {
                        eprintln!("decoding the sound: {e:#}");
                    }
                    if crate::player::debug() {
                        eprintln!("{:>8.3} editor: sound all decoded", crate::player::uptime());
                    }
                });
                Ok(Loaded { info, pcm })
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
            // Everything open keeps going (sound, picture, waveforms, the
            // edit); only reopening by name follows.
            State::Ready(r) => {
                r.player.renamed(ctx, to);
                r.strip.renamed(to);
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
        let waves = (0..n).map(|i| crate::waveform::follow(l.pcm.clone(), i, ctx)).collect();
        let mut player = Player::new(ctx, source, l.info.clone(), l.pcm, gains);
        player.seek(edit.start);

        let full = l.info.duration.max(1e-6);
        let strip = Filmstrip::build(ctx, source, &l.info);
        Self {
            info: l.info,
            player,
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
            snap: true,
            looping: false,
            solo: None,
            show_frames: false,
            view_target: None,
            was_playing: false,
            confirm_revert: false,
            save_as_focused: false,
            moments: crate::moments::of(&target.clip).pins(full),
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, target: &EditTarget, preview_share: &mut f32) -> EditorOutcome {
        // The clip as named in the library; decoding uses `target.source`.
        let source = target.clip.as_path();
        let ctx = ui.ctx().clone();
        self.keyboard(&ctx);
        // What's heard: the edit, with only the soloed track if there is one.
        let mut mix = self.edit.tracks.clone();
        if let Some(solo) = self.solo {
            for (i, t) in mix.iter_mut().enumerate() {
                t.muted |= i != solo;
            }
        }
        self.player.set_mix(mix);
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
                Btn::ghost(summary).with_icon(icon::SLIDERS_HORIZONTAL).trail(icon::CARET_DOWN).id("export_btn").tip("Export settings: resolution, frame rate, quality, a size to fit under, sound tracks").show(ui)
            };
            if export.clicked() {
                self.export_open = !self.export_open;
            }
            // Dev aid: `HESTECLIPS_OPEN_EXPORT=1` opens it at launch.
            if std::env::var_os("HESTECLIPS_OPEN_EXPORT").is_some() && !self.export_shown {
                self.export_shown = true;
                self.export_open = true;
            }
            // Its dropdowns open popups of their own, and egui remembers only
            // one open popup: left to egui, opening a dropdown closed this. So
            // it's kept open here, and a click closes it only outside it, and
            // not when it was for an open dropdown.
            let dropdown_open = egui::Popup::is_any_open(ui.ctx());
            let before = self.edit.output;
            let shown = egui::Popup::from_response(&export)
                .open_bool(&mut self.export_open)
                .close_behavior(egui::PopupCloseBehavior::IgnoreClicks)
                .align(egui::RectAlign::BOTTOM_END)
                .gap(6.0)
                .frame(crate::ui_kit::pop_frame(ui).inner_margin(egui::Margin::same(14)))
                .show(|ui| crate::export_ui::ui(ui, &self.info, &mut self.edit));
            if shown.is_some_and(|r| r.response.clicked_elsewhere()) && !dropdown_open && !export.clicked() {
                self.export_open = false;
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
        let size = self.player.update(&ctx, self.dragging.is_some());
        let player = &self.player;
        crate::transport::stage(ui, preview_rect, size, |p, cover| player.paint_glow(p, cover, egui::Id::new("editor_stage")), |r| player.paint(ui, r));
        if preview_resp.clicked() {
            self.toggle_play();
        }
        divider(ui, preview_h, total, preview_share);

        // --- Transport ---
        let bar = Rect::from_min_size(ui.cursor().min, Vec2::new(ui.available_width(), crate::transport::BAR_H));
        ui.allocate_rect(bar, Sense::hover());
        self.transport(ui, bar);

        // --- Timeline: ruler, video and sound lanes on one time axis ---
        self.timeline(ui);
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
        let (mut undo, mut redo, mut set_in, mut set_out, mut play, mut jump) = (false, false, false, false, false, None);
        let (mut zoom_by, mut zoom_to) = (None, None);
        let mut frames = self.show_frames;
        let can_undo = !self.undo.is_empty();
        let can_redo = !self.redo.is_empty();
        let full = self.info.duration.max(1e-6);
        let zoom_frac = (1.0 - ((self.view.1 / full).ln() / (self.min_span() / full).ln()).clamp(0.0, 1.0)) as f32;
        let zoom_frac = 1.0 - zoom_frac;
        // Narrower windows: Start and End become their brackets, the zoom
        // slider goes (its buttons stay), then the time moves out of the
        // middle to make room.
        let medium = bar.width() < 860.0;
        let narrow = bar.width() < 660.0;
        let centre_w = if narrow { 44.0 } else { 250.0 };
        crate::transport::bar(ui, bar, centre_w, |ui, part| match part {
            crate::transport::Part::Left => {
                undo = Btn::icon(Kind::Ghost, icon::ARROW_U_UP_LEFT).enabled(can_undo).tip("Undo  (Cmd Z)").show(ui).clicked();
                redo = Btn::icon(Kind::Ghost, icon::ARROW_U_UP_RIGHT).enabled(can_redo).tip("Redo  (Shift Cmd Z)").show(ui).clicked();
                crate::transport::vrule(ui);
                if medium {
                    set_in = Btn::ghost("[").small().tip("Start the clip here  (I)").show(ui).clicked();
                    set_out = Btn::ghost("]").small().tip("End the clip here  (O)").show(ui).clicked();
                    // No room in the middle: the time here, without the length.
                    if narrow {
                        crate::transport::vrule(ui);
                        crate::transport::timecode(ui, t, 0.0, self.info.fps, &mut frames);
                    }
                } else {
                    set_in = Btn::ghost("Start").with_icon("[").small().tip("Start the clip here  (I)").show(ui).clicked();
                    set_out = Btn::ghost("End").trail("]").small().tip("End the clip here  (O)").show(ui).clicked();
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
                if Btn::icon(kind(self.looping), icon::REPEAT).tip("Loop the kept part").show(ui).clicked() {
                    self.looping = !self.looping;
                }
                if Btn::icon(kind(self.snap), icon::MAGNET).tip("Snapping  (N): edges snap to the playhead, the clip's ends and its moments").show(ui).clicked() {
                    self.snap = !self.snap;
                }
            }
        });
        self.show_frames = frames;
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

    /// The kept length under the timeline: shown, and on a click typed over
    /// ("12", "0:12.5", or frames when times show as frames). Enter makes the
    /// kept part that long around its middle (as long as the clip allows),
    /// Esc or clicking away leaves it.
    fn length_field(&mut self, ui: &mut egui::Ui, rect: Rect, shown: &str) {
        let pal = crate::theme::pal(ui);
        let id = ui.id().with("length_field");
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
                        let secs = if self.show_frames { typed.trim().parse::<f64>().ok().map(|f| f / self.info.fps) } else { crate::settings_ui::parse_length(&typed) };
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

    /// Snap `t` to the playhead, the trim edges or the clip's ends when it's
    /// within a few points of one (and snapping is on). `except`: what's being
    /// moved, which mustn't snap to itself.
    fn snapped(&self, t: f64, px_per_s: f64, except: Option<Drag>) -> f64 {
        if !self.snap {
            return t;
        }
        let reach = 8.0 / px_per_s.max(1e-6);
        let mut targets = vec![0.0, self.info.duration];
        targets.extend(self.moments.iter().map(|m| m.t));
        // Not while a trim edge moves: the playhead follows the edge, and
        // snapping to it made the edge chase its own shadow (back and forth
        // while held still).
        if !matches!(except, Some(Drag::Playhead | Drag::In | Drag::Out)) {
            targets.push(self.info.snap(self.player.time()));
        }
        if except != Some(Drag::In) {
            targets.push(self.edit.start);
        }
        if except != Some(Drag::Out) {
            targets.push(self.edit.end);
        }
        targets.into_iter().filter(|x| (x - t).abs() < reach).min_by(|a, b| (a - t).abs().total_cmp(&(b - t).abs())).unwrap_or(t)
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
        let (redo, undo, snap) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, Key::Z),
                i.consume_key(egui::Modifiers::COMMAND, Key::Z),
                i.consume_key(egui::Modifiers::NONE, Key::N),
            )
        });
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
        if t < self.edit.end - self.info.frame_duration() && t != self.edit.start {
            self.push_undo(self.edit.clone());
            self.edit.start = t;
        }
    }

    fn set_out(&mut self) {
        // Out is exclusive: include the frame under the playhead.
        let t = (self.info.snap(self.player.time()) + self.info.frame_duration()).min(self.info.duration);
        if t > self.edit.start + self.info.frame_duration() && t != self.edit.end {
            self.push_undo(self.edit.clone());
            self.edit.end = t;
        }
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
    fn timeline(&mut self, ui: &mut egui::Ui) {
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
        let body_bottom = outer.top() + total_h;
        let audio_view = Rect::from_x_y_ranges(outer.x_range(), outer.top() + RULER_H + VIDEO_H..=body_bottom);
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
        // The mix's meter stands at the right, as tall as the timeline.
        let meter_col = Rect::from_min_max(Pos2::new(outer.right() - MIX_W, outer.top()), Pos2::new(outer.right(), body_bottom));
        // Room between for the end handle and the lanes' scrollbar.
        let lanes_x = outer.left() + HEADER_W..=meter_col.left() - 24.0;
        let lanes = Rect::from_x_y_ranges(lanes_x.clone(), outer.top()..=body_bottom);
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
        let (v0, span) = self.view;
        let px_per_s = lanes.width() as f64 / span.max(1e-9);
        let x_of = |t: f64| lanes.left() + ((t - v0) / span) as f32 * lanes.width();
        let t_of = |x: f32| (v0 + ((x - lanes.left()) / lanes.width()).clamp(0.0, 1.0) as f64 * span).clamp(0.0, dur);
        // Everything on the time axis is clipped to the lanes, so a zoomed-in
        // timeline never draws into the headers.
        let lp = ui.painter_at(lanes);

        // --- Ruler ---
        let ruler = Rect::from_min_size(Pos2::new(lanes.left(), outer.top()), Vec2::new(lanes.width(), RULER_H));
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
        p.vline(meter_col.left() - 0.5, meter_col.y_range(), Stroke::new(1.0, pal.line));
        let mix = Rect::from_min_max(meter_col.min + Vec2::new(6.0, 4.0), meter_col.max);
        if crate::meter::stereo_vertical(ui, mix, ui.id().with("mix_meter"), &self.mix_meter) {
            self.mix_meter.reset_clip();
        }
        ui.interact(mix.with_max_y(mix.top() + 16.0), ui.id().with("mix_tip"), Sense::hover()).on_hover_text("The clip's mix: every track together. The number is its loudest peak in the last few seconds.");
        // --- Header column: each lane's name ---

        // --- Video lane ---
        let video = Rect::from_min_size(Pos2::new(lanes.left(), ruler.bottom()), Vec2::new(lanes.width(), VIDEO_H));
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
        let mut lane_rects = Vec::new();
        let labels: Vec<String> = self.info.source_tracks().iter().map(|a| a.label()).collect();
        let mut y = audio_view.top() - self.lane_scroll + 4.0;
        for i in 0..n_audio {
            let lane = Rect::from_min_size(Pos2::new(lanes.left(), y), Vec2::new(lanes.width(), AUDIO_H));
            lane_rects.push(lane);
            y += lane_step;
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

        // --- Trim: the cut parts dimmed, the kept part outlined, handles ---
        let body = Rect::from_x_y_ranges(lanes_x, video.top()..=(y - 4.0).min(audio_view.bottom()));
        let (xi, xo) = (x_of(self.edit.start), x_of(self.edit.end));
        lp.rect_filled(Rect::from_min_max(body.min, Pos2::new(xi, body.bottom())), 0, pal.dim);
        lp.rect_filled(Rect::from_min_max(Pos2::new(xo, body.top()), body.max), 0, pal.dim);
        // The outline shows once something's cut: before that the whole clip
        // is kept and a white frame round everything only adds noise.
        let trimmed = self.edit.start > fd * 0.5 || self.edit.end < dur - fd * 0.5;
        let outline = theme_fade(&ctx, "ed_trimmed", trimmed || self.dragging.is_some_and(|d| matches!(d, Drag::In | Drag::Out)));
        let keep = Rect::from_min_max(Pos2::new(xi, body.top()), Pos2::new(xo, body.bottom()));
        let edge = crate::theme::mix(pal.text3, pal.text, outline);
        if outline > 0.0 {
            lp.hline(keep.x_range(), keep.top() + 1.0, Stroke::new(2.0, crate::theme::fade(pal.text, outline)));
            lp.hline(keep.x_range(), keep.bottom() - 1.0, Stroke::new(2.0, crate::theme::fade(pal.text, outline)));
        }
        let handle_w = 9.0;
        let in_handle = Rect::from_min_max(Pos2::new(xi - handle_w, body.top()), Pos2::new(xi, body.bottom()));
        let out_handle = Rect::from_min_max(Pos2::new(xo, body.top()), Pos2::new(xo + handle_w, body.bottom()));
        let hover_pos = ui.input(|i| i.pointer.hover_pos());
        for (h, left, drag) in [(in_handle, true, Drag::In), (out_handle, false, Drag::Out)] {
            let active = self.dragging == Some(drag) || (self.dragging.is_none() && hover_pos.is_some_and(|p| h.expand(5.0).contains(p)));
            let grow = theme_fade(&ctx, if left { "ed_hin" } else { "ed_hout" }, active);
            let h = if left { h.with_min_x(h.min.x - 2.0 * grow) } else { h.with_max_x(h.max.x + 2.0 * grow) };
            let corners = if left { egui::CornerRadius { nw: 4, sw: 4, ne: 0, se: 0 } } else { egui::CornerRadius { nw: 0, sw: 0, ne: 4, se: 4 } };
            p.rect_filled(h, corners, crate::theme::mix(edge, pal.text, grow));
            p.vline(h.center().x, h.center().y - 8.0..=h.center().y + 8.0, Stroke::new(2.0, crate::theme::fade(pal.base, 0.7)));
        }
        // The volume lines over the handles: their points stay where you can
        // see and grab them.
        for (i, lane) in lane_rects.iter().enumerate() {
            if lane.intersects(audio_view) {
                draw_envelope(&alp, pal, *lane, &self.edit.tracks[i], dur, x_of);
            }
        }
        // The kept part's length, as a dimension line under the lanes; drag it
        // to slide the kept part along the clip, its length kept.
        let dim_y = body_bottom + LENGTH_H / 2.0;
        let range = Rect::from_x_y_ranges(xi.max(lanes.left())..=xo.min(lanes.right()), dim_y - 9.0..=dim_y + 9.0);
        let len = self.edit.duration();
        let len_text = if self.show_frames { format!("{} frames", (len * self.info.fps).round() as u64) } else { crate::transport::clock(len) };
        let range_hv = self.dragging == Some(Drag::Range) || (self.dragging.is_none() && hover_pos.is_some_and(|p| range.contains(p)));
        let rc = if range_hv { pal.text2 } else { pal.line2 };
        p.hline(range.x_range(), dim_y, Stroke::new(1.0, rc));
        p.vline(range.left(), dim_y - 5.0..=dim_y + 5.0, Stroke::new(1.0, if range_hv { pal.text2 } else { pal.text3 }));
        p.vline(range.right(), dim_y - 5.0..=dim_y + 5.0, Stroke::new(1.0, if range_hv { pal.text2 } else { pal.text3 }));
        // The length itself: click it to type another (drawn last, after
        // the timeline takes its clicks, so it gets its own).
        let len_label = {
            let g = p.layout_no_wrap(len_text.clone(), crate::theme::font(12.0, crate::theme::W::Semibold), pal.text);
            let w = if self.typing_length.is_some() { 90.0 } else { g.size().x + 16.0 };
            (w < range.width().max(110.0)).then(|| Rect::from_center_size(Pos2::new(range.center().x, dim_y), Vec2::new(w, 20.0)))
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
            let name_g = ap.layout_job(crate::ui_kit::single_line_job(&name, crate::theme::font(13.0, crate::theme::W::Medium), tc, head.width() - 40.0 - 10.0 - 56.0 * shown_ms(&ctx, i)));
            ap.galley(Pos2::new(head.left() + 40.0, head.center().y - 16.0), name_g, tc);
            ap.text(Pos2::new(head.left() + 40.0, head.center().y + 2.0), Align2::LEFT_TOP, sub, crate::theme::font(11.5, crate::theme::W::Regular), pal.text3);
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
                let s_rect = Rect::from_min_size(Pos2::new(head.right() - 12.0 - 22.0, head.center().y - 10.0), Vec2::new(22.0, 20.0));
                let m_rect = s_rect.translate(Vec2::new(-25.0, 0.0));
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
            // Right-click: mute, solo, reset the volume.
            let track = &self.edit.tracks[i];
            let (muted, flat) = (track.muted, track.points.is_empty() && (track.gain - 1.0).abs() < 0.005);
            let has_points = !track.points.is_empty();
            let mut pick = None;
            crate::ui_kit::context_menu(&hresp, 220.0, |ui| {
                use crate::ui_kit::Item;
                use egui_phosphor::regular as icon;
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

        // --- Interaction ---
        let hit_area = Rect::from_x_y_ranges(lanes.left() - handle_w - 4.0..=lanes.right() + handle_w + 4.0, ruler.top()..=body_bottom + LENGTH_H);
        let resp = ui.interact(hit_area, ui.id().with("timeline"), Sense::click_and_drag());
        let pointer = resp.hover_pos();
        if let Some(label) = len_label {
            self.length_field(ui, label, &len_text);
        }
        // (A click on the length is its own; a drag that starts on it is
        // the timeline's, sliding the kept part: egui gives each its widget.)

        // What's under the pointer, in priority order.
        let hit = |pos: Pos2, this: &Self| -> Option<Drag> {
            // A volume point over a handle is the point's.
            for (i, lane) in lane_rects.iter().enumerate() {
                if lane.contains(pos) && audio_view.contains(pos) {
                    let track = &this.edit.tracks[i];
                    if let Some(k) = track.points.iter().position(|pt| Pos2::new(x_of(pt.t), y_of_db(*lane, pt.db)).distance(pos) < 7.0) {
                        return Some(Drag::Point(i, k));
                    }
                }
            }
            if in_handle.expand(5.0).contains(pos) {
                return Some(Drag::In);
            }
            if out_handle.expand(5.0).contains(pos) {
                return Some(Drag::Out);
            }
            if range.expand2(Vec2::new(0.0, 4.0)).contains(pos) {
                return Some(Drag::Range);
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
                Some(Drag::Range) => ui.ctx().set_cursor_icon(if self.dragging.is_some() { egui::CursorIcon::Grabbing } else { egui::CursorIcon::Grab }),
                Some(Drag::Point(..)) => ui.ctx().set_cursor_icon(egui::CursorIcon::Grab),
                Some(Drag::Line(_)) => ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical),
                _ => {}
            }
            // Hover: a line through the lanes, and the volume on a lane.
            if self.dragging.is_none() && lanes.contains(pos) && pos.y > ruler.bottom() {
                if let Some(i) = lane_rects.iter().position(|l| l.contains(pos) && audio_view.contains(pos)) {
                    let t = self.info.snap(t_of(pos.x));
                    let text = db_label(self.edit.tracks[i].db_at(t));
                    let g = p.layout_no_wrap(text, crate::theme::font(11.0, crate::theme::W::Semibold), Color32::WHITE);
                    let r = Rect::from_min_size(Pos2::new(pos.x + 8.0, pos.y - 22.0), g.size() + Vec2::new(10.0, 4.0));
                    p.rect_filled(r, 4, Color32::from_black_alpha(184));
                    p.galley(r.min + Vec2::new(5.0, 2.0), g, Color32::WHITE);
                }
            }
        }

        if resp.drag_started() {
            // Hit-test where the button went down, not where the pointer is now: a
            // drag only starts after a few pixels of movement, by which time a
            // vertical drag has already left the thin volume line or point.
            if let Some(pos) = ui.input(|i| i.pointer.press_origin()).or(resp.interact_pointer_pos()) {
                let d = hit(pos, self).unwrap_or(Drag::Playhead);
                self.dragging = Some(d);
                if d != Drag::Playhead {
                    self.before_drag = Some(self.edit.clone());
                }
                // Where it was grabbed (in the selection, or beside an edge),
                // so it moves with the pointer instead of jumping to it.
                let at = t_of(pos.x);
                self.grab_offset = match d {
                    Drag::In => at - self.edit.start,
                    Drag::Out => at - self.edit.end,
                    _ => self.info.snap(at) - self.edit.start,
                };
                // Moving the playhead or a trim pauses while you drag and carries on
                // after; shaping the volume never interrupts playback.
                if matches!(d, Drag::In | Drag::Out | Drag::Range | Drag::Playhead) {
                    self.resume = self.player.is_playing();
                    self.player.pause();
                }
            }
        }
        if let (Some(pos), Some(d)) = (resp.interact_pointer_pos(), self.dragging) {
            // Dragging the playhead or a trim past either end scrolls a zoomed timeline.
            if matches!(d, Drag::In | Drag::Out | Drag::Range | Drag::Playhead) && resp.dragged() {
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
                        let t = self.info.snap(self.snapped(t_of(pos.x) - self.grab_offset, px_per_s, Some(Drag::In)));
                        self.edit.start = t.min(self.edit.end - fd);
                        self.player.seek(self.edit.start);
                    }
                    Drag::Out => {
                        let t = self.snapped(t_of(pos.x) - self.grab_offset, px_per_s, Some(Drag::Out));
                        self.edit.end = self.info.snap(t).clamp(self.edit.start + fd, self.info.duration);
                        self.player.seek(self.edit.end - fd);
                    }
                    Drag::Range => {
                        // Same length, slid along the clip, stopping at its ends.
                        let len = self.edit.end - self.edit.start;
                        let start = self.info.snap((t - self.grab_offset).clamp(0.0, (self.info.duration - len).max(0.0)));
                        self.edit.start = start;
                        self.edit.end = (start + len).min(self.info.duration);
                        self.player.seek(self.edit.start);
                    }
                    Drag::Playhead => {
                        let t = self.snapped(t, px_per_s, Some(Drag::Playhead));
                        self.player.seek(self.info.snap(t) + 1e-6);
                    }
                    Drag::Line(i) => {
                        // Move the whole curve up or down by the drag distance.
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
                        // Keep points in time order: clamp between neighbours.
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
            if let Some(was) = self.before_drag.take()
                && !same_edit(&was, &self.edit)
            {
                self.push_undo(was);
            }
            self.dragging = None;
            if std::mem::take(&mut self.resume) && self.player.time() < self.edit.end - fd {
                self.player.play(self.edit.end);
            }
        }

        if resp.double_clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                if let Some(i) = lane_rects.iter().position(|l| l.contains(pos) && audio_view.contains(pos)) {
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
            // Jump there; playing carries on from the new spot.
            if let Some(pos) = resp.interact_pointer_pos() {
                if pos.y < body_bottom {
                    let t = self.snapped(t_of(pos.x), px_per_s, Some(Drag::Playhead));
                    self.player.seek(self.info.snap(t) + 1e-6);
                }
            }
        }
        if resp.secondary_clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                if let Some(Drag::Point(i, k)) = hit(pos, self) {
                    self.push_undo(self.edit.clone());
                    let track = &mut self.edit.tracks[i];
                    let removed = track.points.remove(k);
                    // Removing the last point leaves its level as the flat level.
                    if track.points.is_empty() {
                        track.gain = media::from_db(removed.db);
                    }
                }
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
    In,
    Out,
    /// The whole selection, its length kept.
    Range,
    Playhead,
    /// Whole volume line of track i.
    Line(usize),
    /// Keyframe k of track i.
    Point(usize, usize),
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

fn db_of_y(lane: Rect, y: f32) -> f32 {
    // Detents a few pixels wide at 0 dB (unchanged) and at the bottom (silent).
    if (y - y_of_db(lane, 0.0)).abs() <= 3.0 {
        return 0.0;
    }
    let f = (lane.bottom() - 4.0 - y) / (lane.height() - 8.0);
    if f <= 0.02 { media::SILENT_DB } else { media::fader_db(f) }
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

/// The volume line, striped (so it reads as a control over the waveform, not
/// part of it): faint where it's unchanged, bright with its points as
/// diamonds where it's been changed.
fn draw_envelope(p: &egui::Painter, pal: &crate::theme::Pal, lane: Rect, track: &media::TrackEdit, dur: f64, x_of: impl Fn(f64) -> f32) {
    let flat = track.points.is_empty() && (track.gain - 1.0).abs() < 0.005;
    let color = if track.muted { pal.text3 } else { pal.text };
    if flat {
        // Unchanged: just a faint line at 0 dB to grab.
        let y = y_of_db(lane, 0.0);
        p.extend(egui::Shape::dashed_line(&[Pos2::new(lane.left(), y), Pos2::new(lane.right(), y)], Stroke::new(1.0, crate::theme::fade(pal.text, 0.22)), 5.0, 4.0));
        return;
    }
    let mut pts = vec![Pos2::new(x_of(0.0), y_of_db(lane, track.db_at(0.0)))];
    for pt in &track.points {
        pts.push(Pos2::new(x_of(pt.t), y_of_db(lane, pt.db)));
    }
    pts.push(Pos2::new(x_of(dur), y_of_db(lane, track.db_at(dur))));
    p.extend(egui::Shape::dashed_line(&pts, Stroke::new(1.5, crate::theme::fade(color, 0.85)), 6.0, 4.0));
    for pt in &track.points {
        let c = Pos2::new(x_of(pt.t), y_of_db(lane, pt.db));
        let d = 5.5;
        let diamond = vec![c + Vec2::new(0.0, -d), c + Vec2::new(d, 0.0), c + Vec2::new(0.0, d), c + Vec2::new(-d, 0.0)];
        p.add(egui::Shape::convex_polygon(diamond, color, Stroke::new(1.5, pal.base)));
    }
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
        media::ClipInfo { duration: 60.0, fps: 60.0, width: 1920, height: 1080, video_kbps: None, audio: Vec::new(), id: None }
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
