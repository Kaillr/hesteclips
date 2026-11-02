//! Free up space: make clips take less room, from the library and the
//! player (a clip, or every one selected). Two ways, in one dialog:
//!
//! - **Their original recordings**: an edited clip keeps the recording it
//!   was cut from, so the edit can be changed. Keep it, trim it to what the
//!   edit uses, or delete it (see [`store::free_up`]).
//! - **Make them smaller**: encode the clips again at a lower resolution,
//!   frame rate or quality. An edited clip is made again from its original
//!   (the best it can look); one never edited is replaced by its smaller
//!   version.
//!
//! The dialog says what each way frees, and what it all comes to, worked
//! out for each clip as the choices change. The editor has the originals'
//! ways as the last step of an edit instead, under Save ▾, and its own
//! export settings for the quality.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use egui::{Pos2, Sense, Stroke, StrokeKind};
use media::{ClipInfo, Edit};

use crate::export_ui::{self, LEVELS};
use crate::store::{self, FreeUp};
use crate::theme::{self, W};
use crate::ui_kit::{self, Choice, ChoiceField, Chosen, Custom};
use crate::{App, RenderDone, RenderJob};

/// A clip re-encoded smaller has to come out at most this share of what it
/// is, else it's left as it is (encoding again for little loses quality
/// for nothing).
const WORTH: f64 = 0.85;

/// How "Make them smaller" makes them: at most this height and frame rate
/// (`None`: each clip's own), at this quality.
#[derive(Clone, Copy, PartialEq)]
struct Smaller {
    height: Option<u32>,
    fps: Option<u32>,
    quality: Quality,
}

impl Default for Smaller {
    /// 1080p at Smaller: worth it for clips recorded at Settings' defaults.
    fn default() -> Self {
        Self { height: Some(1080), fps: None, quality: Quality::Level(2) }
    }
}

/// A quality for clips of all sizes: a level (of [`LEVELS`]), worked out
/// for each clip's picture, or a bitrate typed (kbps).
#[derive(Clone, Copy, PartialEq)]
enum Quality {
    Level(usize),
    Kbps(u32),
}

/// What's known of a clip asked about, read in the background.
struct Item {
    target: store::EditTarget,
    /// The recording the clip is made from: its original once edited.
    info: ClipInfo,
    /// Its edit (as recorded, if it was never edited). `None`: edited, but
    /// its edit is missing, so it can't be made again.
    edit: Option<Edit>,
    edited: bool,
    clip_bytes: u64,
    /// All that's kept beside it: its original and edit.
    kept_bytes: u64,
    /// About what trimming its original frees.
    trim_bytes: u64,
}

/// What's done to one clip, and about what it frees.
struct Plan {
    /// Made again with this edit (its output smaller).
    render: Option<Edit>,
    /// Then its original trimmed or deleted.
    then: Option<FreeUp>,
    frees: u64,
}

impl Item {
    /// The clip's edit, its picture made no bigger than `s` allows (never
    /// bigger than it's set to already; a size to fit under gives way to the
    /// quality chosen here).
    fn capped(&self, s: &Smaller) -> Option<Edit> {
        let mut e = self.edit.clone()?;
        let o = &mut e.output;
        o.max_mb = None;
        if let Some(h) = s.height {
            o.height = Some(o.height.map_or(h, |x| x.min(h)));
        }
        if let Some(f) = s.fps {
            o.fps = Some(o.fps.map_or(f, |x| x.min(f)));
        }
        Some(e)
    }

    /// What each quality level comes to for this clip made `s`, in kbps.
    fn level_bitrates(&self, s: &Smaller) -> Option<[u32; 3]> {
        let o = self.capped(s)?.output;
        let (w, h) = o.size(&self.info);
        let fps = o.frame_rate(&self.info);
        Some(LEVELS.map(|(_, share, _)| export_ui::level_kbps(w, h, fps, share)))
    }

    /// The clip's edit, made to come out `s`, if that's worth it: and about
    /// how big it comes out.
    fn smaller(&self, s: &Smaller) -> Option<(Edit, u64)> {
        let mut e = self.capped(s)?;
        let kbps = match s.quality {
            Quality::Level(i) => self.level_bitrates(s)?[i],
            Quality::Kbps(k) => k,
        };
        let o = &mut e.output;
        o.video_kbps = Some(o.video_kbps.map_or(kbps, |x| x.min(kbps)));
        let bytes = e.output.estimate_bytes(&self.info, &e)?;
        ((bytes as f64) < self.clip_bytes as f64 * WORTH).then_some((e, bytes))
    }

    fn plan(&self, originals: Option<FreeUp>, smaller: Option<&Smaller>) -> Plan {
        let render = smaller.and_then(|s| self.smaller(s));
        // Never edited, made smaller: the clip as it was goes (it would be
        // kept as its original otherwise, and free nothing).
        let then = if self.edited { originals } else { render.as_ref().map(|_| FreeUp::Delete) };
        let mut frees = render.as_ref().map_or(0, |(_, b)| self.clip_bytes.saturating_sub(*b));
        if self.edited {
            frees += match then {
                Some(FreeUp::Trim) => self.trim_bytes,
                Some(FreeUp::Delete) => self.kept_bytes,
                None => 0,
            };
        }
        Plan { render: render.map(|(e, _)| e), then, frees }
    }
}

/// Clips asked about in the library or the player.
pub(crate) struct Ask {
    /// The video clips asked about, in library order.
    paths: Vec<PathBuf>,
    /// Of those asked about, how many are left out (screenshots, clips
    /// still saving an edit).
    skipped: usize,
    /// How many keep an original, and what all of them take.
    with_original: usize,
    kept: u64,
    /// What each clip is, read in the background.
    items: Arc<OnceLock<Vec<Item>>>,
    /// What's done to the originals: `None` keeps them.
    originals: Option<FreeUp>,
    /// Make them smaller, and how.
    smaller_on: bool,
    smaller: Smaller,
}

/// Clips being freed up together: one toast for all of them once they're done.
pub(crate) struct Batch {
    /// The render jobs (one per clip) not done yet.
    jobs: Vec<u64>,
    clips: usize,
    /// What's done: made smaller (some of them), and to the originals.
    smaller: bool,
    how: Option<FreeUp>,
    /// Something went to the Bin (its space comes back once it's emptied).
    binned: bool,
    freed: u64,
    /// Nothing freed (an original already as short as its edit).
    unchanged: usize,
    errors: Vec<String>,
}

impl App {
    /// Ask how to free up the space these clips take. Screenshots and clips
    /// still saving an edit are left out.
    pub(crate) fn ask_free_up(&mut self, paths: &[PathBuf]) {
        let busy = |p: &PathBuf| self.renders.iter().any(|j| &j.source == p);
        let with: Vec<&crate::clips::Clip> = self.clips.iter().filter(|c| paths.contains(&c.path) && !c.is_image() && !busy(&c.path)).collect();
        if with.is_empty() {
            self.toast(if paths.iter().all(|p| crate::clips::is_image(p)) { "Screenshots have no space to free up" } else { "These clips are still saving" });
            return;
        }
        let with_original = with.iter().filter(|c| c.kept_bytes > 0).count();
        let kept = with.iter().map(|c| c.kept_bytes).sum();
        let paths_with: Vec<PathBuf> = with.iter().map(|c| c.path.clone()).collect();
        let items = Arc::new(OnceLock::new());
        let (out, todo, ctx) = (items.clone(), paths_with.clone(), self.ctx());
        // Reading each clip (its recording, edit and sizes): in the
        // background, as there may be many.
        std::thread::spawn(move || {
            let _ = out.set(todo.iter().filter_map(|p| read_item(p)).collect());
            ctx.request_repaint();
        });
        // Their originals are the space freed without losing anything, so
        // trimming them is offered first; clips with none can only be made
        // smaller.
        let (originals, smaller_on) = if with_original > 0 { (Some(FreeUp::Trim), false) } else { (None, true) };
        let smaller = ctx_smaller(&self.ctx());
        self.free_up = Some(Ask { skipped: paths.len() - paths_with.len(), paths: paths_with, with_original, kept, items, originals, smaller_on, smaller });
    }

    /// The Free up space dialog, while it's asked.
    pub(crate) fn free_up_dialog(&mut self, ctx: &egui::Context) {
        let Some(ask) = &mut self.free_up else { return };
        let permanently = self.settings.delete_permanently;
        let one = match ask.paths.as_slice() {
            [one] => self.clips.iter().find(|c| &c.path == one).cloned(),
            _ => None,
        };
        let tex = one.as_ref().and_then(|c| self.thumbs.get(ctx, c).and_then(|t| t.texture.clone()));
        let many = ask.paths.len() > 1;
        let items = ask.items.get();
        let all = |f: &dyn Fn(&Item) -> u64| items.map(|i| i.iter().map(f).sum::<u64>());
        let trim = all(&|i| if i.edited { i.trim_bytes } else { 0 });
        let total = all(&|i| i.clip_bytes + i.kept_bytes);
        // Trimming that frees nothing isn't offered.
        if trim == Some(0) && ask.originals == Some(FreeUp::Trim) {
            ask.originals = None;
        }
        let smaller = ask.smaller_on.then_some(ask.smaller);
        let plans: Option<Vec<Plan>> = items.map(|i| i.iter().map(|i| i.plan(ask.originals, smaller.as_ref())).collect());
        let frees = plans.as_ref().map(|p| p.iter().map(|p| p.frees).sum::<u64>());
        let made_smaller = plans.as_ref().map_or(0, |p| p.iter().filter(|p| p.render.is_some()).count());
        let not_worth = items.zip(smaller.as_ref()).map_or(0, |(i, _)| i.iter().filter(|i| i.edit.is_some()).count() - made_smaller);
        let replaced = items.zip(plans.as_ref()).map_or(0, |(i, p)| i.iter().zip(p).filter(|(i, p)| !i.edited && p.render.is_some()).count());
        // The tallest picture and fastest rate among them: what's offered below.
        let top_h = items.and_then(|i| i.iter().map(|i| i.info.height).max()).unwrap_or(4320);
        let top_fps = items.and_then(|i| i.iter().map(|i| i.info.fps.round() as u32).max()).unwrap_or(240);
        let (them, they) = if many { ("them", "they") } else { ("it", "it") };

        let mut go = false;
        let mut cancel = false;
        // A value being typed when the frame began: Enter and Esc are its
        // (applying or putting it back), not the dialog's.
        let typing = ctx.memory(|m| m.focused().is_some());
        let modal = ui_kit::dialog(ctx, "free_up_clips", 480.0, |ui| {
            let p = theme::pal(ui);
            ui_kit::dialog_title(ui, "Free up space");
            match &one {
                Some(c) => {
                    let under = if c.kept_bytes > 0 { format!("{}, and {} for its original recording", c.human_size(), crate::clips::human_bytes(c.kept_bytes)) } else { c.human_size() };
                    crate::library::clip_row(ui, c, tex.as_ref(), Some(&under));
                }
                None => ui_kit::dialog_text(ui, &format!("{} clips, {}", ask.paths.len(), total.map_or("…".to_owned(), export_ui::human_bytes))),
            }
            if ask.skipped > 0 {
                ui_kit::hint(ui, &format!("{} more selected {} left out: screenshots, and clips still saving.", ask.skipped, if ask.skipped == 1 { "is" } else { "are" }));
            }

            // --- Their original recordings ---
            if ask.with_original > 0 {
                section(ui, "Original recording", if many { "s" } else { "" });
                ui_kit::hint(ui, &if many {
                    format!("{} of them keep the recording they were cut from ({}), so their edits can be changed later.", ask.with_original, export_ui::human_bytes(ask.kept))
                } else {
                    "Kept so the edit can be changed later, or reverted.".to_owned()
                });
                ui.add_space(8.0);
                let id = egui::Id::new("free_up_originals");
                if choice_row(ui, id.with("keep"), ask.originals.is_none(), true, false, "Keep", if many { "Their edits can still be changed." } else { "The edit can still be changed." }, "").clicked() {
                    ask.originals = None;
                }
                ui.add_space(6.0);
                let (trim_text, trim_amount) = match trim {
                    Some(0) => (if many { "Each one is already about as short as its edit." } else { "It's already about as short as the edit." }, "Nothing".to_owned()),
                    t => (
                        if many { "Keeps only the part each edit uses: they can still be changed within it." } else { "Keeps only the part the edit uses: it can still be changed within it." },
                        t.map_or("…".to_owned(), |b| format!("about {}", export_ui::human_bytes(b))),
                    ),
                };
                if choice_row(ui, id.with("trim"), ask.originals == Some(FreeUp::Trim), trim != Some(0), false, "Trim", trim_text, &trim_amount).clicked() {
                    ask.originals = Some(FreeUp::Trim);
                }
                ui.add_space(6.0);
                let mut delete_text = if many { "The clips stay as they are, but their edits can't be changed or reverted any more." } else { "The clip stays as it is, but its edit can't be changed or reverted any more." }.to_owned();
                if !permanently {
                    delete_text.push_str(&format!(" {} go to the {}.", if many { "They" } else { "It" }, store::bin_name()));
                }
                if choice_row(ui, id.with("delete"), ask.originals == Some(FreeUp::Delete), true, true, "Delete", &delete_text, &export_ui::human_bytes(ask.kept)).clicked() {
                    ask.originals = Some(FreeUp::Delete);
                }
            }

            // --- Make them smaller ---
            ui.add_space(if ask.with_original > 0 { 14.0 } else { 10.0 });
            ui.horizontal(|ui| {
                ui.label(theme::text(format!("Make {them} smaller"), 13.5, W::Semibold).color(p.text));
                // Clips with no original: this is all there is to do.
                if ask.with_original > 0 {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| ui_kit::switch(ui, &mut ask.smaller_on));
                }
            });
            ui_kit::hint(ui, &format!("Encodes {them} again at a lower resolution, frame rate or quality. Lost detail can't be brought back."));
            if ask.smaller_on {
                ui.add_space(6.0);
                // For one clip, what each quality level comes to for it.
                let bitrates = items.filter(|i| !many && i.len() == 1).and_then(|i| i[0].level_bitrates(&ask.smaller));
                let s = &mut ask.smaller;
                let row = |ui: &mut egui::Ui, label: &str, add: &mut dyn FnMut(&mut egui::Ui)| {
                    ui.horizontal(|ui| {
                        ui.set_min_height(36.0);
                        ui.label(theme::text(label, 13.5, W::Regular).color(p.text));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| add(ui));
                    });
                };
                // Original: each clip as it is, for several; for one, what it is.
                let original = |what: String| if many { Choice::new("Original").note("each clip's own") } else { Choice::new("Original").shown(format!("Original ({what})")).note(what) };
                row(ui, "Resolution, at most", &mut |ui| {
                    if let Some(h) = export_ui::height_choice(ui, "free_up_height", s.height, top_h, s.height.unwrap_or(top_h), (None, original(format!("{top_h}p"))), "any height") {
                        s.height = h;
                    }
                });
                row(ui, "Frame rate, at most", &mut |ui| {
                    if let Some(f) = export_ui::fps_choice(ui, "free_up_fps", s.fps, top_fps, s.fps.unwrap_or(top_fps), (None, original(format!("{top_fps} fps"))), "any frame rate") {
                        s.fps = f;
                    }
                });
                row(ui, "Quality", &mut |ui| {
                    if let Some(q) = quality_choice(ui, "free_up_quality", s.quality, bitrates) {
                        s.quality = q;
                    }
                });
                if items.is_some() {
                    if not_worth > 0 {
                        ui_kit::hint(ui, &if !many {
                            "It's already about this small, so it's left as it is.".to_owned()
                        } else if made_smaller == 0 {
                            "They're all already about this small, so they're left as they are.".to_owned()
                        } else {
                            format!("{not_worth} of them {} already about this small, and {} left as {}.", if not_worth == 1 { "is" } else { "are" }, if not_worth == 1 { "is" } else { "are" }, if not_worth == 1 { "it is" } else { "they are" })
                        });
                    }
                    if replaced > 0 {
                        let bin = if permanently { String::new() } else { format!(" (to the {})", store::bin_name()) };
                        ui_kit::hint(ui, &if many {
                            format!("{replaced} that were never edited are replaced by their smaller version{bin}.")
                        } else {
                            format!("It was never edited, so it's replaced by its smaller version{bin}.")
                        });
                    }
                }
            }

            // --- What it comes to ---
            ui.add_space(12.0);
            let (r, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), Sense::hover());
            ui.painter().hline(r.x_range(), r.center().y, Stroke::new(1.0, p.line));
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let words = match frees {
                    None => "Working out what it frees…".to_owned(),
                    Some(0) => "Frees nothing as it's set".to_owned(),
                    Some(b) => format!("Frees about {}", export_ui::human_bytes(b)),
                };
                ui.label(theme::text(words, 13.5, W::Semibold).color(if frees == Some(0) { p.text3 } else { p.text }));
                if let (Some(t), Some(f)) = (total, frees.filter(|&f| f > 0)) {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(theme::text(format!("{} → {}", export_ui::human_bytes(t), export_ui::human_bytes(t.saturating_sub(f))), 13.0, W::Regular).color(p.text3));
                    });
                }
            });
            if made_smaller > 0 {
                ui_kit::hint(ui, &format!("Encoding {} again takes a while: {they} show as saving meanwhile.", if many { "them" } else { "it" }));
            }
            ui_kit::dialog_buttons(ui, |ui| {
                let ready = frees.is_some_and(|f| f > 0);
                let label = frees.filter(|&f| f > 0).map_or("Free up space".to_owned(), |f| format!("Free up {}", export_ui::human_bytes(f)));
                let b = if ask.originals == Some(FreeUp::Delete) && ask.with_original > 0 { ui_kit::Btn::new(ui_kit::Kind::Stop, label) } else { ui_kit::Btn::primary(label) };
                go |= b.enabled(ready).show(ui).clicked();
                cancel = ui.add(ui_kit::button("Cancel", false).ghost()).clicked();
                // Enter goes, when nothing's being typed.
                go |= ready && !typing && ui.input(|i| i.key_pressed(egui::Key::Enter));
            });
        });
        if go && let (Some(items), Some(plans)) = (items, plans) {
            ctx_keep_smaller(ctx, ask.smaller);
            let work: Vec<(store::EditTarget, ClipInfo, Plan)> = items.iter().zip(plans).filter(|(_, p)| p.render.is_some() || p.then.is_some()).map(|(i, p)| (i.target.clone(), i.info.clone(), p)).collect();
            let how = ask.originals.filter(|_| ask.with_original > 0);
            self.free_up = None;
            self.selection.clear();
            self.start_free_up(work, how);
        } else if cancel || (ui_kit::Dismissed::dismissed(&modal) && !typing) {
            self.free_up = None;
        }
    }

    /// Trim or delete an edited clip's original, in the background, as
    /// the editor's Save ▾ does when the edit's already saved.
    pub(crate) fn free_up_original(&mut self, target: store::EditTarget, how: FreeUp) {
        let info = media::probe(&target.source).ok();
        let Some(info) = info else {
            self.toast_error("Couldn't free up the space: the original can't be read");
            return;
        };
        self.start_free_up(vec![(target, info, Plan { render: None, then: Some(how), frees: 0 })], Some(how));
    }

    /// Free up clips' space as planned, one after another in the
    /// background (encoding several at once would only slow each down).
    /// Each clip shows as saving meanwhile; one toast says how it went.
    fn start_free_up(&mut self, work: Vec<(store::EditTarget, ClipInfo, Plan)>, how: Option<FreeUp>) {
        if work.is_empty() {
            return;
        }
        let permanently = self.settings.delete_permanently;
        let smaller = work.iter().any(|(_, _, p)| p.render.is_some());
        let binned = !permanently && work.iter().any(|(_, _, p)| p.then == Some(FreeUp::Delete));
        let mut jobs = Vec::new();
        let mut progress = Vec::new();
        for (t, _, plan) in &work {
            let id = self.next_render_id;
            self.next_render_id += 1;
            let p = Arc::new(std::sync::atomic::AtomicU32::new(0));
            self.renders.push(RenderJob { id, source: t.clip.clone(), dest: t.clip.clone(), as_new: false, progress: p.clone() });
            jobs.push(id);
            progress.push(p);
            if plan.render.is_some() {
                // Its file is replaced: nothing may hold it open (Windows
                // won't replace a file that's open).
                media::pcm::release(&t.clip);
                crate::proxy::release(&t.clip);
            }
        }
        // A clip playing in its card, or in the player, lets go of its file
        // too: the player shows it saving until it's done.
        self.hover_play = None;
        if let Some(v) = &self.viewer
            && let Some((t, _, _)) = work.iter().find(|(t, _, p)| p.render.is_some() && t.clip == v.clip())
        {
            let resume = v.resume();
            self.viewer = Some(crate::viewer::Viewer::saving(&t.clip, resume));
        }
        self.free_up_batches.push(Batch { jobs: jobs.clone(), clips: work.len(), smaller, how, binned, freed: 0, unchanged: 0, errors: Vec::new() });
        let tx = self.render_tx.clone();
        let ctx = self.ctx();
        std::thread::spawn(move || {
            for (((target, info, plan), id), progress) in work.into_iter().zip(jobs).zip(progress) {
                let before = footprint(&target.clip);
                let report = |f: f32| {
                    progress.store(f.to_bits(), std::sync::atomic::Ordering::Relaxed);
                    ctx.request_repaint();
                };
                let made = match &plan.render {
                    Some(edit) => crate::render_in_place(&target, &info, edit, report).map(|_| ()),
                    None => Ok(()),
                };
                let freed = made.and_then(|()| match plan.then {
                    // Read again: a clip never edited has an original now.
                    Some(how) => store::free_up(&store::EditTarget::of(&target.clip), how, permanently).map(|_| ()),
                    None => Ok(()),
                });
                let freed = freed.map(|()| before.saturating_sub(footprint(&target.clip)));
                // How it went is the batch's to say: the clip is there either way.
                let _ = tx.send(RenderDone { id, result: Ok(target.clip.clone()), freed: Some((plan.then.unwrap_or(FreeUp::Trim), freed)) });
                ctx.request_repaint();
            }
        });
        self.refresh_clips();
    }

    /// A clip was freed up (render job `id`): counted into its batch, whose
    /// toast shows once all of it is done. `false`: it's not in one (freed
    /// after saving an edit), so the toast is the caller's.
    pub(crate) fn freed_in_batch(&mut self, id: u64, freed: &Result<u64, String>) -> bool {
        let Some(b) = self.free_up_batches.iter().position(|b| b.jobs.contains(&id)) else { return false };
        let batch = &mut self.free_up_batches[b];
        batch.jobs.retain(|&j| j != id);
        match freed {
            Ok(0) => batch.unchanged += 1,
            Ok(bytes) => batch.freed += bytes,
            Err(e) => batch.errors.push(e.clone()),
        }
        if batch.jobs.is_empty() {
            let batch = self.free_up_batches.remove(b);
            if batch.smaller {
                self.smaller_toast(&batch);
            } else {
                self.freed_toast(batch.how.unwrap_or(FreeUp::Trim), batch.clips, batch.freed, batch.unchanged, &batch.errors);
            }
        }
        true
    }

    /// How making clips smaller (and maybe their originals) went.
    fn smaller_toast(&mut self, b: &Batch) {
        let done = b.clips - b.errors.len();
        let bytes = export_ui::human_bytes(b.freed);
        let bin = if b.binned { format!(" (empty the {} to get all of it back)", store::bin_name()) } else { String::new() };
        let what = if done == 1 { "Made the clip smaller".to_owned() } else { format!("Made {done} clips smaller") };
        if let Some(e) = b.errors.first() {
            let also = if done > 0 { format!(" ({what}: freed {bytes})") } else { String::new() };
            let failed = if b.clips == 1 { String::new() } else { format!(" of {} clip{}", b.errors.len(), if b.errors.len() == 1 { "" } else { "s" }) };
            self.toast_error(format!("Couldn't free up the space{failed}: {e}{also}"));
        } else {
            self.toast(format!("{what}: freed {bytes}{bin}"));
        }
    }

    /// How freeing up `clips` clips' originals went.
    pub(crate) fn freed_toast(&mut self, how: FreeUp, clips: usize, freed: u64, unchanged: usize, errors: &[String]) {
        let done = clips - errors.len() - unchanged;
        let bytes = export_ui::human_bytes(freed);
        let bin = if how == FreeUp::Delete && !self.settings.delete_permanently { format!(" (empty the {} to get the space back)", store::bin_name()) } else { String::new() };
        let what = match (how, done) {
            (FreeUp::Trim, 1) => "Trimmed the original".to_owned(),
            (FreeUp::Trim, n) => format!("Trimmed {n} originals"),
            (FreeUp::Delete, 1) => "Deleted the original".to_owned(),
            (FreeUp::Delete, n) => format!("Deleted {n} originals"),
        };
        if let Some(e) = errors.first() {
            let failed = if errors.len() == 1 && clips == 1 { String::new() } else { format!(" of {} clip{}", errors.len(), if errors.len() == 1 { "" } else { "s" }) };
            let also = if done > 0 { format!(" ({what}: freed {bytes})") } else { String::new() };
            self.toast_error(format!("Couldn't free up the space{failed}: {e}{also}"));
        } else if done == 0 {
            self.toast(if clips == 1 { "The original is already as short as this edit" } else { "The originals are already as short as their edits" });
        } else {
            self.toast(format!("{what}: freed {bytes}{bin}"));
        }
    }
}

/// Read what's needed of the clip at `path` (`None` if it can't be read).
fn read_item(path: &Path) -> Option<Item> {
    let target = store::EditTarget::of(path);
    let edited = target.source != target.clip;
    let info = media::probe(&target.source).ok()?;
    let edit = if edited { store::load_edit(&target) } else { Some(Edit::new(&info)) };
    let (kept_bytes, trim_bytes) = if edited { store::free_up_sizes(&target).unwrap_or((0, 0)) } else { (0, 0) };
    let clip_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    Some(Item { target, info, edit, edited, clip_bytes, kept_bytes, trim_bytes })
}

/// All a clip takes: its file, and what's kept beside it.
fn footprint(clip: &Path) -> u64 {
    let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let target = store::EditTarget::of(clip);
    let kept = target.id.as_deref().map_or(0, |id| std::fs::read_dir(store::assets_dir(target.library(), id)).map(|d| d.flatten().map(|e| size(&e.path())).sum()).unwrap_or(0));
    size(clip) + kept
}

/// How clips were last made smaller, for the next time it's asked (while
/// the app runs).
fn ctx_smaller(ctx: &egui::Context) -> Smaller {
    ctx.data(|d| d.get_temp::<Smaller>(egui::Id::new("free_up_smaller"))).unwrap_or_default()
}

fn ctx_keep_smaller(ctx: &egui::Context, s: Smaller) {
    ctx.data_mut(|d| d.insert_temp(egui::Id::new("free_up_smaller"), s));
}

/// A section's heading: `name`, plural with `s`.
fn section(ui: &mut egui::Ui, name: &str, s: &str) {
    ui.add_space(12.0);
    ui.label(theme::text(format!("{name}{s}"), 13.5, W::Semibold).color(theme::pal(ui).text));
}

/// The quality: a level, worked out for each clip's picture, or a bitrate
/// of your own (whole Mbps, as Settings takes it) for all of them.
/// `bitrates`: for one clip, what each level comes to for it, to show. The
/// new one when it changes.
fn quality_choice(ui: &mut egui::Ui, id: &str, now: Quality, bitrates: Option<[u32; 3]>) -> Option<Quality> {
    let choices = LEVELS
        .iter()
        .enumerate()
        .map(|(i, &(name, _, like))| match bitrates {
            Some(b) => Choice::new(name).shown(format!("{name} ({})", export_ui::mbps(b[i]))).note(export_ui::mbps(b[i])),
            None => Choice::new(name).note(like),
        })
        .collect();
    let (at, number) = match now {
        Quality::Level(i) => (Some(i), bitrates.map_or(8, |b| (b[i] / 1000).max(1))),
        Quality::Kbps(k) => (None, (k / 1000).max(1)),
    };
    let note = if bitrates.is_some() { "any bitrate" } else { "the same for every clip" };
    let field = ChoiceField { id, choices, custom: Some(Custom { unit: "Mbps", range: 1..=300, note }), width: export_ui::FIELD_W };
    match field.show(ui, at, number)? {
        Chosen::Choice(i) => Some(Quality::Level(i)),
        Chosen::Number(m) => Some(Quality::Kbps(m * 1000)),
    }
}

/// One way, as a choice: a radio, its name, what it does, and on the right
/// what it frees. `danger`: chosen, it's red.
#[allow(clippy::too_many_arguments)]
fn choice_row(ui: &mut egui::Ui, id: egui::Id, chosen: bool, enabled: bool, danger: bool, title: &str, text: &str, amount: &str) -> egui::Response {
    let p = theme::pal(ui);
    ui.scope(|ui| {
        if !enabled {
            ui.multiply_opacity(0.45);
        }
        // The card goes under what's in it, once its size is known.
        let under = ui.painter().add(egui::Shape::Noop);
        let accent = if danger { p.danger } else { p.text };
        let amount_g = ui.painter().layout_no_wrap(amount.to_owned(), theme::font(13.5, W::Semibold), if chosen { accent } else { p.text2 });
        let inner = egui::Frame::new()
            .inner_margin(egui::Margin { left: 40, right: 14, top: 10, bottom: 11 })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let w = ui.available_width() - amount_g.size().x - 16.0;
                ui.vertical(|ui| {
                    ui.set_width(w);
                    ui.spacing_mut().item_spacing.y = 3.0;
                    let t = ui.label(theme::text(title, 13.5, W::Semibold).color(p.text));
                    ui_kit::hint(ui, text);
                    t.rect
                })
                .inner
            });
        let rect = inner.response.rect;
        let title_rect = inner.inner;
        let resp = ui.interact(rect, id, if enabled { Sense::click() } else { Sense::hover() });
        let hv = theme::toggle(ui.ctx(), id.with("hv"), resp.hovered() && enabled, theme::FAST);
        let on = theme::toggle(ui.ctx(), id.with("on"), chosen, theme::FAST);
        let fill = theme::mix(p.raise2, p.sel, hv * (1.0 - on * 0.5));
        let stroke = Stroke::new(1.0 + 0.5 * on, theme::mix(p.line, accent, on));
        ui.painter().set(under, egui::Shape::Vec(vec![egui::Shape::rect_filled(rect, 8, fill), egui::Shape::rect_stroke(rect, 8, stroke, StrokeKind::Inside)]));
        // The radio, beside the name.
        let c = Pos2::new(rect.left() + 20.0, title_rect.center().y);
        ui.painter().circle_stroke(c, 7.0, Stroke::new(1.5, theme::mix(p.text3, accent, on)));
        if on > 0.0 {
            ui.painter().circle_filled(c, 3.5 * on, accent);
        }
        let at = Pos2::new(rect.right() - 14.0 - amount_g.size().x, title_rect.center().y - amount_g.size().y / 2.0);
        ui.painter().galley(at, amount_g, p.text2);
        if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
    })
    .inner
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1080p60 clip of a minute at `mbps`, never edited.
    fn item(mbps: u32) -> Item {
        let info = ClipInfo {
            duration: 60.0,
            width: 1920,
            height: 1080,
            fps: 60.0,
            video_kbps: Some(mbps * 1000),
            audio: Vec::new(),
            id: None,
            bt601: false,
        };
        let clip_bytes = mbps as u64 * 1_000_000 / 8 * 60;
        Item { target: store::EditTarget::of(Path::new("clip.mp4")), edit: Some(Edit::new(&info)), info, edited: false, clip_bytes, kept_bytes: 0, trim_bytes: 0 }
    }

    #[test]
    fn smaller_only_when_worth_it() {
        let s = Smaller { height: None, fps: None, quality: Quality::Level(1) };
        // Recorded at Balanced (15 Mbps): Balanced again frees nothing.
        let plan = item(15).plan(None, Some(&s));
        assert!(plan.render.is_none() && plan.then.is_none() && plan.frees == 0);
        // Recorded at 50 Mbps: made again at 15, the recording as it was goes.
        let plan = item(50).plan(None, Some(&s));
        assert_eq!(plan.then, Some(FreeUp::Delete));
        assert_eq!(plan.render.as_ref().and_then(|e| e.output.video_kbps), Some(15_000));
        assert!(plan.frees > 250_000_000, "frees {}", plan.frees);
    }

    #[test]
    fn a_lower_resolution_lowers_the_level() {
        let s = Smaller { height: Some(720), fps: Some(30), quality: Quality::Level(1) };
        let e = item(15).plan(None, Some(&s)).render.unwrap();
        assert_eq!((e.output.height, e.output.fps), (Some(720), Some(30)));
        // Balanced at 720p30 is about 3 Mbps.
        assert_eq!(e.output.video_kbps, Some(3_000));
    }

    #[test]
    fn originals_without_making_smaller() {
        let mut i = item(15);
        (i.edited, i.kept_bytes, i.trim_bytes) = (true, 500, 300);
        assert_eq!(i.plan(Some(FreeUp::Trim), None).frees, 300);
        assert_eq!(i.plan(Some(FreeUp::Delete), None).frees, 500);
        let keep = i.plan(None, None);
        assert!(keep.render.is_none() && keep.then.is_none());
    }
}
