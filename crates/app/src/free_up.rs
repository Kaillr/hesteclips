//! Freeing up the space an edited clip's original recording takes: trim it
//! to what the edit uses, or delete it (see [`store::free_up`]).
//!
//! From the library and the player (a clip, or every one selected), a
//! dialog asks: the two ways as choices, each with what it frees, and one
//! button to go. The editor has them as the last step of an edit instead,
//! under Save ▾.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use egui::{Pos2, Sense, Stroke, StrokeKind};

use crate::store::{self, FreeUp};
use crate::theme::{self, W};
use crate::{App, RenderDone, RenderJob, ui_kit};

/// How much there is to free, for the dialog.
struct Sizes {
    /// How many clips have an original to free.
    pub clips: usize,
    /// What deleting frees: everything kept beside them.
    pub delete: u64,
    /// About what trimming frees, once it's worked out.
    pub trim: Option<u64>,
}

/// What the dialog's way out was.
enum Answer {
    Go(FreeUp),
    Cancel,
}

/// The dialog. `choice`: the way chosen (`None` before one can be: trimming
/// frees nothing). `lead`: what it's about, drawn under the title.
fn dialog(ctx: &egui::Context, id: &str, choice: &mut Option<FreeUp>, sizes: &Sizes, permanently: bool, lead: impl FnOnce(&mut egui::Ui)) -> Option<Answer> {
    let many = sizes.clips > 1;
    let trim_none = sizes.trim == Some(0);
    // Trimming is chosen first (it keeps the edit); when it would free
    // nothing, nothing is chosen until Delete is.
    if trim_none && *choice == Some(FreeUp::Trim) {
        *choice = None;
    }
    let mut answer = None;
    let modal = ui_kit::dialog(ctx, id, 460.0, |ui| {
        ui_kit::dialog_title(ui, "Free up space");
        lead(ui);
        ui.add_space(10.0);
        let (trim_text, trim_amount) = match sizes.trim {
            Some(0) if many => ("Each one is already about as short as its edit.".to_owned(), "Nothing".to_owned()),
            Some(0) => ("It's already about as short as the edit.".to_owned(), "Nothing".to_owned()),
            bytes => (
                if many {
                    "Keeps only the part each edit uses. You can still change an edit within it, or revert it.".to_owned()
                } else {
                    "Keeps only the part the edit uses. You can still change the edit within it, or revert it.".to_owned()
                },
                bytes.map_or("…".to_owned(), |b| format!("about {}", crate::export_ui::human_bytes(b))),
            ),
        };
        let trim = choice_row(
            ui,
            egui::Id::new(id).with("trim"),
            *choice == Some(FreeUp::Trim),
            !trim_none,
            false,
            if many { "Trim the originals" } else { "Trim the original" },
            &trim_text,
            &trim_amount,
        );
        if trim.clicked() {
            *choice = Some(FreeUp::Trim);
        }
        ui.add_space(6.0);
        let mut delete_text = if many { "The clips stay as they are, but their edits can't be changed or reverted any more." } else { "The clip stays as it is, but its edit can't be changed or reverted any more." }.to_owned();
        if !permanently {
            delete_text.push_str(&format!(" {} go to the {}: empty it to get the space back.", if many { "They" } else { "It" }, store::bin_name()));
        }
        let delete = choice_row(
            ui,
            egui::Id::new(id).with("delete"),
            *choice == Some(FreeUp::Delete),
            true,
            true,
            if many { "Delete the originals" } else { "Delete the original" },
            &delete_text,
            &crate::export_ui::human_bytes(sizes.delete),
        );
        if delete.clicked() {
            *choice = Some(FreeUp::Delete);
        }
        // ↑ and ↓ pick too.
        let (up, down) = ui.input_mut(|i| (i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp), i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown)));
        if up && !trim_none {
            *choice = Some(FreeUp::Trim);
        }
        if down {
            *choice = Some(FreeUp::Delete);
        }
        ui_kit::dialog_buttons(ui, |ui| {
            let what = if many { "originals" } else { "original" };
            let go = match choice {
                Some(FreeUp::Delete) => ui.add(ui_kit::danger_button(format!("Delete {what}"))),
                _ => ui_kit::Btn::primary(format!("Trim {what}")).enabled(choice.is_some()).show(ui),
            };
            if go.clicked()
                && let Some(how) = *choice
            {
                answer = Some(Answer::Go(how));
            }
            if ui.add(ui_kit::button("Cancel", false).ghost()).clicked() {
                answer = Some(Answer::Cancel);
            }
        });
        // Enter goes, as the default action of a dialog.
        if ui.input(|i| i.key_pressed(egui::Key::Enter))
            && let Some(how) = *choice
        {
            answer = Some(Answer::Go(how));
        }
    });
    if answer.is_none() && ui_kit::Dismissed::dismissed(&modal) {
        answer = Some(Answer::Cancel);
    }
    answer
}

/// One way to free up space, as a choice: a radio, its name, what it does,
/// and on the right what it frees. `danger`: chosen, it's red.
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
            .inner_margin(egui::Margin { left: 40, right: 14, top: 11, bottom: 12 })
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

/// Clips asked about in the library.
pub(crate) struct Ask {
    /// The clips with an original to free, in library order.
    paths: Vec<PathBuf>,
    /// Of those asked about, how many have none.
    skipped: usize,
    /// What deleting frees.
    delete: u64,
    /// About what trimming frees, worked out in the background.
    trim: Arc<OnceLock<u64>>,
    choice: Option<FreeUp>,
}

/// Originals being freed together: a toast for all of them once they're done.
pub(crate) struct Batch {
    how: FreeUp,
    /// The render jobs (one per clip) not done yet.
    jobs: Vec<u64>,
    clips: usize,
    freed: u64,
    /// Trimmed, but already as short as their edits.
    unchanged: usize,
    errors: Vec<String>,
}

impl App {
    /// Ask how to free up the space these clips' originals take. Clips with
    /// none (never edited, or still saving an edit) are left out.
    pub(crate) fn ask_free_up(&mut self, paths: &[PathBuf]) {
        let busy = |p: &PathBuf| self.renders.iter().any(|j| &j.source == p);
        let with: Vec<&crate::clips::Clip> = self.clips.iter().filter(|c| paths.contains(&c.path) && c.kept_bytes > 0 && !c.is_image() && !busy(&c.path)).collect();
        if with.is_empty() {
            self.toast(if paths.len() == 1 { "This clip has no original recording kept" } else { "None of these clips has an original recording kept" });
            return;
        }
        let delete = with.iter().map(|c| c.kept_bytes).sum();
        let paths_with: Vec<PathBuf> = with.iter().map(|c| c.path.clone()).collect();
        let trim = Arc::new(OnceLock::new());
        let (out, todo, ctx) = (trim.clone(), paths_with.clone(), self.ctx());
        // Reading each original's length and edit: in the background, as
        // there may be many.
        std::thread::spawn(move || {
            let bytes = todo.iter().filter_map(|p| store::free_up_sizes(&store::EditTarget::of(p))).map(|(_, trim)| trim).sum();
            let _ = out.set(bytes);
            ctx.request_repaint();
        });
        self.free_up = Some(Ask { skipped: paths.len() - paths_with.len(), paths: paths_with, delete, trim, choice: Some(FreeUp::Trim) });
    }

    /// The library's Free up space dialog, while it's asked.
    pub(crate) fn free_up_dialog(&mut self, ctx: &egui::Context) {
        let Some(ask) = &mut self.free_up else { return };
        let sizes = Sizes { clips: ask.paths.len(), delete: ask.delete, trim: ask.trim.get().copied() };
        let one = match ask.paths.as_slice() {
            [one] => self.clips.iter().find(|c| &c.path == one).cloned(),
            _ => None,
        };
        let tex = one.as_ref().and_then(|c| self.thumbs.get(ctx, c).and_then(|t| t.texture.clone()));
        let skipped = ask.skipped;
        let permanently = self.settings.delete_permanently;
        let lead = |ui: &mut egui::Ui| match &one {
            Some(c) => {
                crate::library::clip_row(ui, c, tex.as_ref(), Some(&format!("{} clip, {} original recording", c.human_size(), crate::export_ui::human_bytes(c.kept_bytes))));
                ui.add_space(8.0);
                ui_kit::dialog_text(ui, "Its original recording is kept so you can change the edit later, or revert it.");
                if skipped > 0 {
                    ui_kit::hint(ui, if skipped == 1 { "The other clip selected has no original kept." } else { "The other clips selected have no original kept." });
                }
            }
            None => {
                ui_kit::dialog_text(ui, &format!("{} edited clips keep their original recordings, so their edits can be changed later: {} in all.", sizes.clips, crate::export_ui::human_bytes(sizes.delete)));
                if skipped > 0 {
                    ui_kit::hint(ui, &format!("{skipped} of the selected clips {} no original kept, and {} left as {}.", if skipped == 1 { "has" } else { "have" }, if skipped == 1 { "is" } else { "are" }, if skipped == 1 { "it is" } else { "they are" }));
                }
            }
        };
        match dialog(ctx, "free_up_clips", &mut ask.choice, &sizes, permanently, lead) {
            Some(Answer::Go(how)) => {
                let paths = std::mem::take(&mut ask.paths);
                self.free_up = None;
                self.selection.clear();
                self.start_free_up(paths.iter().map(|p| store::EditTarget::of(p)).collect(), how);
            }
            Some(Answer::Cancel) => self.free_up = None,
            None => {}
        }
    }

    /// Trim or delete clips' originals, one after another in the background.
    /// Each clip shows as saving meanwhile; one toast says how it went.
    pub(crate) fn start_free_up(&mut self, targets: Vec<store::EditTarget>, how: FreeUp) {
        if targets.is_empty() {
            return;
        }
        let mut jobs = Vec::new();
        for t in &targets {
            let id = self.next_render_id;
            self.next_render_id += 1;
            self.renders.push(RenderJob {
                id,
                source: t.clip.clone(),
                dest: t.clip.clone(),
                as_new: false,
                progress: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            });
            jobs.push(id);
        }
        self.free_up_batches.push(Batch { how, jobs: jobs.clone(), clips: targets.len(), freed: 0, unchanged: 0, errors: Vec::new() });
        let tx = self.render_tx.clone();
        let ctx = self.ctx();
        let permanently = self.settings.delete_permanently;
        std::thread::spawn(move || {
            for (target, id) in targets.into_iter().zip(jobs) {
                let freed = store::free_up(&target, how, permanently);
                let _ = tx.send(RenderDone { id, result: Ok(target.clip.clone()), freed: Some((how, freed)) });
                ctx.request_repaint();
            }
        });
        self.refresh_clips();
    }

    /// A clip's original was freed (render job `id`): counted into its
    /// batch, whose toast shows once all of it is done. `false`: it's not in
    /// one (freed after saving an edit), so the toast is the caller's.
    pub(crate) fn freed_in_batch(&mut self, id: u64, freed: &Result<u64, String>) -> bool {
        let Some(b) = self.free_up_batches.iter().position(|b| b.jobs.contains(&id)) else { return false };
        let batch = &mut self.free_up_batches[b];
        batch.jobs.retain(|&j| j != id);
        match freed {
            Ok(0) if batch.how == FreeUp::Trim => batch.unchanged += 1,
            Ok(bytes) => batch.freed += bytes,
            Err(e) => batch.errors.push(e.clone()),
        }
        if batch.jobs.is_empty() {
            let batch = self.free_up_batches.remove(b);
            self.freed_toast(batch.how, batch.clips, batch.freed, batch.unchanged, &batch.errors);
        }
        true
    }

    /// How freeing up `clips` clips' originals went.
    pub(crate) fn freed_toast(&mut self, how: FreeUp, clips: usize, freed: u64, unchanged: usize, errors: &[String]) {
        let done = clips - errors.len() - unchanged;
        let bytes = crate::export_ui::human_bytes(freed);
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
