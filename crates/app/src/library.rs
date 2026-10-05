//! The clip library: a thumbnail grid grouped by day, newest first.
//!
//! Click a card to play it; hover for a Share button; right-click for everything
//! else. While a capture is running a placeholder card stands in for the clip, so
//! a half-written file never shows up as if it were finished.
//!
//! Clips in the library's folders (one per game) can be shown one game at a
//! time, picked from a row of chips on top, and moved between folders.
//!
//! Selecting works like Photos and Finder: the check circle on a card, ⌘-click
//! or Shift-click start a selection; while one is active a plain click toggles a
//! card instead of playing it, and a bar on top acts on all of them.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};

use crate::{App, RecState, clips, share, store, thumbs};

const MIN_CARD_WIDTH: f32 = 220.0;
const GAP: f32 = 14.0;
const CAPTION_HEIGHT: f32 = 42.0;
const RADIUS: u8 = 8;
/// How long a freshly saved clip stays highlighted.
const NEW_HIGHLIGHT: Duration = Duration::from_secs(8);

pub(crate) const REC_RED: Color32 = Color32::from_rgb(235, 72, 72);
pub(crate) const ACCENT: Color32 = Color32::from_rgb(90, 150, 255);

/// The "Rename clip" dialog, or "Rename folder" for one of the library's folders.
pub(crate) struct Rename {
    path: PathBuf,
    name: String,
    error: Option<String>,
    focused: bool,
    folder: bool,
}

/// Which clips the library shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum Filter {
    #[default]
    All,
    /// The clips in this folder of the library (a game's).
    Folder(String),
    /// The clips in the library itself, in no folder.
    Loose,
}

impl Filter {
    fn matches(&self, clip: &clips::Clip) -> bool {
        match self {
            Filter::All => true,
            Filter::Folder(name) => clip.folder.as_ref() == Some(name),
            Filter::Loose => clip.folder.is_none(),
        }
    }

    /// Back to all clips once none is left to show (the last one moved or deleted).
    pub fn retain(&mut self, clips: &[clips::Clip]) {
        if !clips.iter().any(|c| self.matches(c)) {
            *self = Filter::All;
        }
    }

    /// Make sure the clip at `path` is shown: all clips, if it's another game's.
    pub fn reveal(&mut self, clips: &[clips::Clip], path: &Path) {
        if clips.iter().any(|c| c.path == path && !self.matches(c)) {
            *self = Filter::All;
        }
    }
}

/// A chip on top of the library: a folder (game) to show the clips of.
struct Category {
    filter: Filter,
    label: String,
    count: usize,
}

/// The chips: All, each folder with clips (the game played last first), and
/// Other for clips in no folder. None when there are no folders to pick between.
fn categories(clips: &[clips::Clip]) -> Vec<Category> {
    let mut cats: Vec<Category> = Vec::new();
    let mut loose = 0;
    // Newest first, so a folder's place is its newest clip's.
    for clip in clips {
        match &clip.folder {
            Some(f) => match cats.iter_mut().find(|c| &c.label == f) {
                Some(c) => c.count += 1,
                None => cats.push(Category { filter: Filter::Folder(f.clone()), label: f.clone(), count: 1 }),
            },
            None => loose += 1,
        }
    }
    if cats.is_empty() {
        return cats;
    }
    if loose > 0 {
        cats.push(Category { filter: Filter::Loose, label: "Other".into(), count: loose });
    }
    cats.insert(0, Category { filter: Filter::All, label: "All".into(), count: clips.len() });
    cats
}

/// Clips picked for a bulk action.
#[derive(Default)]
pub(crate) struct Selection {
    paths: HashSet<PathBuf>,
    /// Last card clicked, where a Shift-click range starts.
    anchor: Option<PathBuf>,
}

impl Selection {
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    pub fn clear(&mut self) {
        self.paths.clear();
        self.anchor = None;
    }

    /// Forget clips that are gone (trashed, renamed, deleted in Finder).
    pub fn retain(&mut self, clips: &[clips::Clip]) {
        self.paths.retain(|p| clips.iter().any(|c| &c.path == p));
        if self.anchor.as_ref().is_some_and(|a| !self.paths.contains(a)) {
            self.anchor = None;
        }
    }

    fn toggle(&mut self, path: &PathBuf) {
        if !self.paths.remove(path) {
            self.paths.insert(path.clone());
        }
        self.anchor = Some(path.clone());
    }

    /// Add every clip from the anchor to `path`, in library order.
    fn extend_to(&mut self, path: &PathBuf, clips: &[clips::Clip]) {
        let at = |p: &PathBuf| clips.iter().position(|c| &c.path == p);
        match (self.anchor.as_ref().and_then(at), at(path)) {
            (Some(a), Some(b)) => {
                for c in &clips[a.min(b)..=a.max(b)] {
                    self.paths.insert(c.path.clone());
                }
            }
            _ => {
                self.paths.insert(path.clone());
                self.anchor = Some(path.clone());
            }
        }
    }
}

enum Action {
    /// Drag the file out of the window (to another app / the desktop).
    DragOut(PathBuf, Option<PathBuf>),
    Share(PathBuf, ShareChoice),
    Rename(PathBuf),
    Open(PathBuf),
    /// Play it in the OS default video player instead.
    OpenExternal(PathBuf),
    Edit(PathBuf),
    Trash(PathBuf),
    /// Move clips to this folder of the library (`None`: out of any folder).
    MoveTo(Vec<PathBuf>, Option<String>),
    MoveSelectedTo(Option<String>),
    Filter(Filter),
    RenameFolder(String),
    RevealFolder(String),
    /// Toggle a card's selection, or (`range`) select up to it from the anchor.
    Select { path: PathBuf, range: bool },
    SelectAll,
    Deselect,
    TrashSelected,
    CancelUpload(PathBuf),
}

enum Card<'a> {
    /// Recording or saving a replay clip.
    Placeholder,
    /// A "Save as new clip" render that has no file yet.
    NewRender(usize),
    Clip(&'a clips::Clip),
}

impl App {
    pub(crate) fn library(&mut self, ui: &mut egui::Ui, frame: &eframe::Frame) {
        self.permission_banner(ui);

        // One card for a recording in progress, and one per clip still being written.
        let placeholders = usize::from(self.rec_state == RecState::Recording) + self.saving;
        let placeholder = placeholders > 0;
        let new_renders: Vec<usize> = (0..self.renders.len()).filter(|&i| self.renders[i].as_new).collect();
        if self.clips.is_empty() && !placeholder && new_renders.is_empty() {
            self.empty_state(ui);
            return;
        }

        // The clips of the game picked on top. Cards borrow a snapshot so
        // drawing them can still use `&mut self` (thumbnail cache).
        let cats = categories(&self.clips);
        let mut folders: Vec<String> = cats.iter().filter_map(|c| if let Filter::Folder(f) = &c.filter { Some(f.clone()) } else { None }).collect();
        folders.sort_by_key(|f| f.to_lowercase());
        let clips: Vec<clips::Clip> = self.clips.iter().filter(|c| self.library_filter.matches(c)).cloned().collect();

        let mut action = self.selection_keys(ui);
        if !self.selection.is_empty() {
            if let Some(a) = self.selection_bar(ui, clips.len(), &folders) {
                action = Some(a);
            }
        }
        if let Some(a) = self.category_chips(ui, &cats) {
            action = Some(a);
        }

        // Group into days. The placeholder always belongs to today.
        let mut groups: Vec<(chrono::NaiveDate, Vec<Card>)> = Vec::new();
        let mut live: Vec<Card> = new_renders.into_iter().map(Card::NewRender).collect();
        for _ in 0..placeholders {
            live.insert(0, Card::Placeholder);
        }
        if !live.is_empty() {
            groups.push((chrono::Local::now().date_naive(), live));
        }
        for clip in &clips {
            let day = clip.day();
            match groups.last_mut() {
                Some((d, cards)) if *d == day => cards.push(Card::Clip(clip)),
                _ => groups.push((day, vec![Card::Clip(clip)])),
            }
        }

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let avail = ui.available_width();
            let cols = (((avail + GAP) / (MIN_CARD_WIDTH + GAP)).floor() as usize).max(1);
            let card_w = (avail - GAP * (cols - 1) as f32) / cols as f32;

            for (day, cards) in &groups {
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(clips::day_label(*day)).strong().size(15.0));
                    let n = cards.iter().filter(|c| matches!(c, Card::Clip(_))).count();
                    if n > 0 {
                        ui.weak(if n == 1 { "1 clip".to_owned() } else { format!("{n} clips") });
                    }
                });
                ui.add_space(6.0);
                for row in cards.chunks(cols) {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = GAP;
                        for card in row {
                            match card {
                                Card::Placeholder => self.placeholder_card(ui, card_w),
                                Card::NewRender(i) => self.new_render_card(ui, *i, card_w),
                                Card::Clip(clip) => {
                                    if let Some(a) = self.clip_card(ui, clip, card_w, &folders) {
                                        action = Some(a);
                                    }
                                }
                            }
                        }
                    });
                    ui.add_space(GAP);
                }
            }
        });

        match action {
            Some(Action::Open(p)) => self.open_viewer(p),
            Some(Action::OpenExternal(p)) => {
                if let Err(e) = clips::open_in_default_app(&p) {
                    self.toast_error(format!("Couldn't open the clip: {e}"));
                }
            }
            Some(Action::Share(p, choice)) => self.share(frame, p, choice),
            Some(Action::DragOut(p, preview)) => self.drag_out(ui.ctx(), frame, &p, preview),
            Some(Action::Edit(p)) => self.open_editor(p),
            Some(Action::Rename(p)) => self.rename_clip(p),
            Some(Action::Trash(p)) => self.trash_clips(&[p]),
            Some(Action::MoveTo(paths, folder)) => self.move_clips(ui.ctx(), &paths, folder),
            Some(Action::MoveSelectedTo(folder)) => {
                let paths: Vec<PathBuf> = clips.iter().filter(|c| self.selection.paths.contains(&c.path)).map(|c| c.path.clone()).collect();
                self.move_clips(ui.ctx(), &paths, folder);
            }
            Some(Action::Filter(f)) => {
                self.library_filter = f;
                self.selection.clear();
            }
            Some(Action::RenameFolder(name)) => self.rename_folder(&name),
            Some(Action::RevealFolder(name)) => {
                if let Err(e) = clips::open_in_default_app(&self.settings.output_dir.join(name)) {
                    self.toast_error(format!("Couldn't open the folder: {e}"));
                }
            }
            Some(Action::Select { path, range }) => {
                if range {
                    self.selection.extend_to(&path, &clips);
                } else {
                    self.selection.toggle(&path);
                }
            }
            Some(Action::SelectAll) => {
                self.selection.paths = clips.iter().map(|c| c.path.clone()).collect();
            }
            Some(Action::Deselect) => self.selection.clear(),
            Some(Action::CancelUpload(p)) => {
                if let Some(up) = self.cloud.upload_for(&p) {
                    up.cancel();
                }
            }
            Some(Action::TrashSelected) => {
                // In library order, so the toast and any failure read naturally.
                let paths: Vec<PathBuf> = self.clips.iter().filter(|c| self.selection.paths.contains(&c.path)).map(|c| c.path.clone()).collect();
                self.trash_clips(&paths);
            }
            None => {}
        }
    }

    /// ⌘A selects every clip, ⌘⌫ / Delete trashes the selection, Esc clears it.
    fn selection_keys(&mut self, ui: &egui::Ui) -> Option<Action> {
        if self.rename.is_some() || self.dialog.is_some() || ui.ctx().egui_wants_keyboard_input() {
            return None;
        }
        let cmd = egui::Modifiers::COMMAND;
        ui.ctx().input_mut(|i| {
            if i.consume_key(cmd, egui::Key::A) {
                Some(Action::SelectAll)
            } else if self.selection.is_empty() {
                None
            } else if i.consume_key(cmd, egui::Key::Backspace) || i.consume_key(egui::Modifiers::NONE, egui::Key::Delete) {
                Some(Action::TrashSelected)
            } else if i.consume_key(egui::Modifiers::NONE, egui::Key::Escape) {
                Some(Action::Deselect)
            } else {
                None
            }
        })
    }

    /// The bar on top while clips are selected: how many, and what to do with
    /// them. `shown`: how many clips the library shows.
    fn selection_bar(&self, ui: &mut egui::Ui, shown: usize, folders: &[String]) -> Option<Action> {
        let mut action = None;
        let n = self.selection.paths.len();
        ui.add_space(8.0);
        egui::Frame::new()
            .fill(ACCENT.gamma_multiply(0.16))
            .stroke(Stroke::new(1.0, ACCENT.gamma_multiply(0.6)))
            .corner_radius(CornerRadius::same(RADIUS))
            .inner_margin(egui::Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(if n == 1 { "1 clip selected".to_owned() } else { format!("{n} clips selected") }).strong().size(15.0));
                    ui.add_space(8.0);
                    if n < shown && ui.button("Select all").on_hover_text(crate::hotkey_label_cmd("A")).clicked() {
                        action = Some(Action::SelectAll);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let trash = egui::Button::new(egui::RichText::new("🗑  Move to Trash").color(Color32::WHITE)).fill(ui.visuals().error_fg_color);
                        let key = if cfg!(target_os = "macos") { crate::hotkey_label_cmd("Delete") } else { "Delete".to_owned() };
                        if ui.add(trash).on_hover_text(key).clicked() {
                            action = Some(Action::TrashSelected);
                        }
                        if !folders.is_empty() {
                            ui.menu_button("📁  Move to", |ui| {
                                if let Some(f) = move_menu(ui, folders, None, true) {
                                    action = Some(Action::MoveSelectedTo(f));
                                }
                            });
                        }
                        if ui.button("Cancel").on_hover_text("Esc").clicked() {
                            action = Some(Action::Deselect);
                        }
                    });
                });
            });
        action
    }

    /// Move clips (and their edits) to the Bin. Clips with a save in progress are
    /// left alone: their file is about to be replaced.
    fn trash_clips(&mut self, paths: &[PathBuf]) {
        let mut library = self.clips.clone();
        let (mut moved, mut busy) = (Vec::new(), 0);
        let mut error = None;
        for path in paths {
            let Some(i) = library.iter().position(|c| &c.path == path) else { continue };
            if self.renders.iter().any(|j| &j.source == path) {
                busy += 1;
                continue;
            }
            // Trash against what's left, so duplicates sharing assets let go of them
            // with the last copy.
            let clip = library.remove(i);
            match store::trash(&clip, &library) {
                Ok(()) => moved.push(clip.path),
                Err(e) => {
                    library.insert(i, clip);
                    error = Some(e.to_string());
                }
            }
        }
        if let Some(e) = error {
            self.toast_error(format!("Couldn't move to the Trash: {e}"));
        } else if busy > 0 {
            self.toast_error("Clips that are still saving an edit were kept.");
        } else if let [one] = moved.as_slice() {
            self.toast(format!("Moved {} to the Trash", crate::file_name(one)));
        } else if !moved.is_empty() {
            self.toast(format!("Moved {} clips to the Trash", moved.len()));
        }
        self.selection.clear();
        self.refresh_clips();
    }

    /// The row of folders (games) to show the clips of.
    fn category_chips(&mut self, ui: &mut egui::Ui, cats: &[Category]) -> Option<Action> {
        if cats.is_empty() {
            return None;
        }
        let mut action = None;
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
            for cat in cats {
                let selected = self.library_filter == cat.filter;
                let v = ui.visuals();
                let (text, weak) = if selected { (Color32::WHITE, Color32::from_white_alpha(190)) } else { (v.text_color(), v.weak_text_color()) };
                let mut job = egui::text::LayoutJob::default();
                let format = |size: f32, color| egui::TextFormat { font_id: FontId::proportional(size), color, valign: egui::Align::Center, ..Default::default() };
                job.append(&cat.label, 0.0, format(14.0, text));
                job.append(&cat.count.to_string(), 7.0, format(12.0, weak));
                let fill = if selected { ACCENT } else { v.widgets.inactive.weak_bg_fill };
                let icon = match &cat.filter {
                    Filter::Folder(name) => self.folder_icon(ui.ctx(), name),
                    _ => None,
                };
                let button = match icon {
                    Some(tex) => egui::Button::image_and_text(egui::Image::from_texture((tex.id(), Vec2::splat(18.0))).corner_radius(4), job),
                    None => egui::Button::new(job),
                };
                let r = ui.add(button.fill(fill).corner_radius(CornerRadius::same(14)).min_size(Vec2::new(0.0, 28.0)));
                if r.clicked() && !selected {
                    action = Some(Action::Filter(cat.filter.clone()));
                }
                if let Filter::Folder(name) = &cat.filter {
                    r.context_menu(|ui| {
                        if ui.button(format!("📂  {}", crate::reveal_label())).clicked() {
                            action = Some(Action::RevealFolder(name.clone()));
                        }
                        if ui.button("✏  Rename folder…").clicked() {
                            action = Some(Action::RenameFolder(name.clone()));
                        }
                    });
                }
            }
        });
        ui.add_space(2.0);
        action
    }

    /// The game's picture for one of the library's folders, when Discord knows
    /// the game (also after the folder's been renamed).
    fn folder_icon(&mut self, ctx: &egui::Context, folder: &str) -> Option<egui::TextureHandle> {
        let game = self.settings.game_folders.iter().find(|(_, f)| *f == folder).map_or(folder, |(game, _)| game.as_str());
        let url = crate::discord::game_for_folder(game)?.icon?;
        self.game_icons.get(ctx, &url)
    }

    /// Move clips to another folder of the library (`None`: the library
    /// itself). Their edits follow by id. Clips busy saving an edit or
    /// uploading stay where they are.
    fn move_clips(&mut self, ctx: &egui::Context, paths: &[PathBuf], folder: Option<String>) {
        let lib = self.settings.output_dir.clone();
        let dir = folder.as_ref().map_or(lib.clone(), |f| lib.join(f));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.toast_error(format!("Couldn't move: {e}"));
            return;
        }
        let (mut moved, mut busy, mut error) = (0, 0, None);
        for path in paths {
            if path.parent() == Some(dir.as_path()) {
                continue;
            }
            if self.renders.iter().any(|j| &j.source == path) || self.cloud.upload_for(path).is_some() {
                busy += 1;
                continue;
            }
            let Some(name) = path.file_name() else { continue };
            let to = clips::free_path(&dir, &name.to_string_lossy());
            match clips::rename(path, &to) {
                Ok(()) => {
                    self.follow_rename(ctx, path, &to);
                    moved += 1;
                }
                Err(e) => error = Some(e.to_string()),
            }
        }
        let place = folder.as_deref().unwrap_or("Other");
        if let Some(e) = error {
            self.toast_error(format!("Couldn't move: {e}"));
        } else if busy > 0 {
            self.toast_error("Clips that are still saving an edit or uploading were kept where they are.");
        } else if moved == 1 {
            self.toast(format!("Moved to {place}"));
        } else if moved > 1 {
            self.toast(format!("Moved {moved} clips to {place}"));
        }
        self.selection.clear();
        self.refresh_clips();
    }

    /// A clip's file moved (renamed, or to another folder): the player, the
    /// editor and the "new" highlight follow it.
    fn follow_rename(&mut self, ctx: &egui::Context, from: &Path, to: &Path) {
        if let Some((p, _)) = &mut self.last_saved
            && p == from
        {
            *p = to.to_path_buf();
        }
        if let Some(v) = &mut self.viewer {
            v.renamed(ctx, from, to);
        }
        if let Some(e) = &mut self.editor {
            e.renamed(ctx, from, to);
        }
    }

    fn empty_state(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.28);
            ui.label(egui::RichText::new("🎬").size(44.0));
            ui.add_space(8.0);
            ui.label(egui::RichText::new("No clips yet").size(20.0).strong());
            ui.add_space(4.0);
            let hint = match self.rec_state {
                RecState::Buffering => format!(
                    "The replay buffer is running. Press {} or Save clip to keep the last {} seconds.",
                    self.shortcut_label(crate::settings::ShortcutAction::SaveClip),
                    self.settings.replay_seconds
                ),
                RecState::Recording => "Recording… stop it to see your clip here.".to_owned(),
                RecState::Idle => format!(
                    "Start the replay buffer and press {} whenever something worth keeping happens.",
                    self.shortcut_label(crate::settings::ShortcutAction::SaveClip)
                ),
            };
            ui.weak(hint);
        });
    }

    /// Placeholder for a clip that doesn't exist yet: a pulsing dot, a label and,
    /// when known, a progress bar. Shared by recording, saving and rendering.
    fn progress_card(ui: &mut egui::Ui, w: f32, title: &str, caption: &str, color: Color32, progress: Option<f32>) {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(w, w * 9.0 / 16.0 + CAPTION_HEIGHT), Sense::hover());
        let thumb = Rect::from_min_size(rect.min, Vec2::new(w, w * 9.0 / 16.0));
        let p = ui.painter();
        let v = ui.visuals();
        p.rect_filled(thumb, RADIUS, v.extreme_bg_color);
        p.rect_stroke(thumb, RADIUS, Stroke::new(1.5, color), StrokeKind::Inside);
        pulse(p, thumb.center() - Vec2::new(0.0, 14.0), color, ui.input(|i| i.time));
        p.text(thumb.center() + Vec2::new(0.0, 14.0), Align2::CENTER_CENTER, title, FontId::proportional(15.0), v.text_color());
        if let Some(f) = progress {
            progress_bar(p, thumb, f, color);
        }
        p.text(Pos2::new(rect.left() + 2.0, thumb.bottom() + 8.0), Align2::LEFT_TOP, caption, FontId::proportional(14.0), v.weak_text_color());
        ui.ctx().request_repaint();
    }

    fn new_render_card(&mut self, ui: &mut egui::Ui, i: usize, w: f32) {
        let job = &self.renders[i];
        let f = job.progress();
        // Show the source clip's preview under the progress, so it's clear what's being made.
        let source = self.clips.iter().find(|c| c.path == job.source).cloned();
        if let Some(src) = source {
            let name = job.dest.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let (rect, _) = ui.allocate_exact_size(Vec2::new(w, w * 9.0 / 16.0 + CAPTION_HEIGHT), Sense::hover());
            let thumb_rect = Rect::from_min_size(rect.min, Vec2::new(w, w * 9.0 / 16.0));
            let ctx = ui.ctx().clone();
            if let Some(tex) = self.thumbs.get(&ctx, &src).and_then(|t| t.texture.clone()) {
                let uv = crate::filmstrip::crop_uv(tex.size_vec2(), thumb_rect.size());
                egui::Image::from_texture((tex.id(), thumb_rect.size())).uv(uv).corner_radius(RADIUS).paint_at(ui, thumb_rect);
            }
            let p = ui.painter();
            p.rect_filled(thumb_rect, RADIUS, Color32::from_black_alpha(160));
            p.rect_stroke(thumb_rect, RADIUS, Stroke::new(1.5, ACCENT), StrokeKind::Inside);
            pulse(p, thumb_rect.center() - Vec2::new(0.0, 12.0), ACCENT, ui.input(|i| i.time));
            p.text(thumb_rect.center() + Vec2::new(0.0, 14.0), Align2::CENTER_CENTER, format!("Saving new clip  {:.0}%", f * 100.0), FontId::proportional(14.0), Color32::WHITE);
            progress_bar(p, thumb_rect, f, ACCENT);
            let v = ui.visuals();
            p.text(Pos2::new(rect.left() + 2.0, thumb_rect.bottom() + 6.0), Align2::LEFT_TOP, &name, FontId::proportional(14.0), v.strong_text_color());
            ui.ctx().request_repaint();
            return;
        }
        let name = job.dest.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        Self::progress_card(ui, w, &format!("Saving  {:.0}%", f * 100.0), &name, ACCENT, Some(f));
    }

    fn placeholder_card(&mut self, ui: &mut egui::Ui, w: f32) {
        if self.rec_state == RecState::Recording {
            let secs = self.rec_started.map_or(0, |t| t.elapsed().as_secs());
            let title = format!("Recording  {}", thumbs::format_duration(Duration::from_secs(secs)));
            Self::progress_card(ui, w, &title, "In progress", REC_RED, None);
        } else {
            Self::progress_card(ui, w, "Saving clip…", "In progress", ACCENT, None);
        }
    }

    fn clip_card(&mut self, ui: &mut egui::Ui, clip: &clips::Clip, w: f32, folders: &[String]) -> Option<Action> {
        let thumb_h = w * 9.0 / 16.0;
        // click_and_drag: a click plays, a drag pulls the file out of the window.
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, thumb_h + CAPTION_HEIGHT), Sense::click_and_drag());
        let thumb_rect = Rect::from_min_size(rect.min, Vec2::new(w, thumb_h));
        if self.reveal_clip.as_ref() == Some(&clip.path) {
            ui.scroll_to_rect(rect, Some(egui::Align::Center));
            self.reveal_clip = None;
        }
        let hovered = ui.rect_contains_pointer(rect);
        let is_new = self
            .last_saved
            .as_ref()
            .is_some_and(|(p, at)| *p == clip.path && at.elapsed() < NEW_HIGHLIGHT);
        let selecting = !self.selection.is_empty();
        let selected = self.selection.paths.contains(&clip.path);

        let ctx = ui.ctx().clone();
        let thumb = self.thumbs.get(&ctx, clip);
        let v = ui.visuals().clone();
        let p = ui.painter();

        // --- Thumbnail (cropped to 16:9 so every card lines up) ---
        match thumb.and_then(|t| t.texture.as_ref()) {
            Some(tex) => {
                let uv = crate::filmstrip::crop_uv(tex.size_vec2(), thumb_rect.size());
                egui::Image::from_texture((tex.id(), thumb_rect.size()))
                    .uv(uv)
                    .corner_radius(RADIUS)
                    .paint_at(ui, thumb_rect);
            }
            None => {
                p.rect_filled(thumb_rect, RADIUS, v.extreme_bg_color);
                let icon = if thumb.is_some() { "🎬" } else { "…" };
                p.text(thumb_rect.center(), Align2::CENTER_CENTER, icon, FontId::proportional(26.0), v.weak_text_color());
            }
        }

        if let Some(d) = thumb.and_then(|t| t.duration) {
            badge(p, thumb_rect.right_bottom() - Vec2::new(6.0, 6.0), Align2::RIGHT_BOTTOM, &thumbs::format_duration(d), Color32::from_black_alpha(190));
        }
        // An edit of this clip is rendering: dim it, pulse, and show progress.
        let render = self.renders.iter().find(|j| !j.as_new && j.source == clip.path).map(|j| j.progress());
        if let Some(f) = render {
            p.rect_filled(thumb_rect, RADIUS, Color32::from_black_alpha(150));
            pulse(p, thumb_rect.center() - Vec2::new(0.0, 12.0), ACCENT, ui.input(|i| i.time));
            p.text(thumb_rect.center() + Vec2::new(0.0, 14.0), Align2::CENTER_CENTER, format!("Saving edit  {:.0}%", f * 100.0), FontId::proportional(14.0), Color32::WHITE);
            progress_bar(p, thumb_rect, f, ACCENT);
            ui.ctx().request_repaint();
        } else if let Some(up) = self.cloud.upload_for(&clip.path) {
            // Uploading: a slim bar along the bottom and a label, without hiding the clip.
            let f = up.progress();
            let label = if up.merging() { "Finishing upload…".to_owned() } else { format!("☁ Uploading  {:.0}%", f * 100.0) };
            badge(p, thumb_rect.left_top() + Vec2::new(6.0, 6.0), Align2::LEFT_TOP, &label, Color32::from_black_alpha(190));
            progress_bar(p, thumb_rect, f, ACCENT);
            ui.ctx().request_repaint();
        } else if is_new && !selecting && !hovered {
            badge(p, thumb_rect.left_top() + Vec2::new(6.0, 6.0), Align2::LEFT_TOP, "NEW", ACCENT);
        }
        if selected {
            p.rect_filled(thumb_rect, RADIUS, ACCENT.gamma_multiply(0.22));
        }

        if hovered && render.is_none() && !selecting {
            p.rect_filled(thumb_rect, RADIUS, Color32::from_black_alpha(70));
            let c = thumb_rect.center();
            p.circle_filled(c, 22.0, Color32::from_black_alpha(150));
            p.add(egui::Shape::convex_polygon(
                vec![c + Vec2::new(-6.0, -10.0), c + Vec2::new(11.0, 0.0), c + Vec2::new(-6.0, 10.0)],
                Color32::WHITE,
                Stroke::NONE,
            ));
        }
        let border = if selected {
            Stroke::new(3.0, ACCENT)
        } else if is_new {
            Stroke::new(2.0, ACCENT)
        } else if hovered {
            Stroke::new(1.0, v.widgets.hovered.bg_stroke.color)
        } else {
            Stroke::new(1.0, v.widgets.noninteractive.bg_stroke.color)
        };
        p.rect_stroke(thumb_rect, RADIUS, border, StrokeKind::Inside);

        // --- Caption ---
        // Edited clips say so in words, in the accent colour, right where you read
        // the clip's details — not a dark chip lost on top of a busy thumbnail.
        let edited = clip.original.is_some() && render.is_none();
        let mut text_x = rect.left() + 2.0;
        let title_y = thumb_rect.bottom() + 6.0;
        if edited {
            text_x += edited_pill(p, Pos2::new(text_x, title_y + 1.0)) + 6.0;
        }
        let text_w = rect.right() - text_x - 2.0;
        let title = p.layout_job(single_line(&clip.title(), FontId::proportional(14.0), v.strong_text_color(), text_w));
        p.galley(Pos2::new(text_x, title_y), title, v.text_color());
        let ext = clip.path.extension().map(|e| e.to_string_lossy().to_uppercase()).unwrap_or_default();
        // "trimmed from 0:52" only when the length actually changed (an audio-only
        // edit keeps it). Probing the original is cached like any thumbnail.
        let original = clip.original.as_ref().filter(|_| edited).and_then(|p| clips::Clip::at(p));
        let trimmed_from = if let Some(original) = &original {
            let _ = self.thumbs.get(&ctx, original);
            match (self.thumbs.duration_of(original), self.thumbs.duration_of(clip)) {
                (Some(orig), Some(now)) if orig - now > 0.5 => Some(thumbs::format_duration(Duration::from_secs_f64(orig))),
                _ => None,
            }
        } else {
            None
        };
        let mut detail = match trimmed_from {
            Some(orig) => format!("{}  ·  {ext}  ·  trimmed from {orig}", clip.human_size()),
            None => format!("{}  ·  {ext}", clip.human_size()),
        };
        // Every game's clips together: say whose this is.
        if let (Filter::All, Some(folder)) = (&self.library_filter, &clip.folder) {
            detail = format!("{folder}  ·  {detail}");
        }
        let meta = p.layout_job(single_line(&detail, FontId::proportional(12.0), v.weak_text_color(), w - 4.0));
        p.galley(Pos2::new(rect.left() + 2.0, thumb_rect.bottom() + 24.0), meta, v.weak_text_color());

        let mut action = None;
        let modifiers = ui.input(|i| i.modifiers);

        // Check circle: always there while selecting, on hover otherwise.
        if (hovered || selecting) && render.is_none() {
            let check = Rect::from_min_size(thumb_rect.left_top() + Vec2::new(8.0, 8.0), Vec2::splat(24.0));
            let r = ui.interact(check.expand(4.0), ui.id().with(("select", &clip.path)), Sense::click());
            check_circle(ui.painter(), check.center(), selected, r.hovered());
            let tip = if selected { "Deselect" } else { "Select" };
            if r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip).clicked() {
                action = Some(Action::Select { path: clip.path.clone(), range: modifiers.shift });
            }
        }

        // Hover shortcuts for the most common follow-ups.
        let share_rect = Rect::from_min_size(thumb_rect.right_top() + Vec2::new(-84.0, 6.0), Vec2::new(78.0, 26.0));
        let edit_rect = Rect::from_min_size(share_rect.left_top() - Vec2::new(70.0, 0.0), Vec2::new(64.0, 26.0));
        let share_id = ui.id().with(("share_menu", &clip.path));
        let menu_open = egui::Popup::is_id_open(ui.ctx(), share_id);
        if (hovered || menu_open) && render.is_none() && !selecting {
            let overlay_button = |ui: &mut egui::Ui, rect: Rect, text: &str, fill: Color32| {
                // A detached child Ui: `ui.put` would count as another item in the row
                // and push the next card one gap to the right while hovered.
                ui.new_child(egui::UiBuilder::new().max_rect(rect)).add_sized(
                    rect.size(),
                    egui::Button::new(egui::RichText::new(text).color(Color32::WHITE))
                        .fill(fill)
                        .corner_radius(CornerRadius::same(6)),
                )
            };
            if overlay_button(ui, edit_rect, "✂ Edit", Color32::from_black_alpha(170)).clicked() {
                action = Some(Action::Edit(clip.path.clone()));
            }
            let share_btn = overlay_button(ui, share_rect, "📤 Share", ACCENT);
            egui::Popup::menu(&share_btn).id(share_id).show(|ui| {
                if let Some(c) = share_menu(ui, share_btn.rect.left_bottom()) {
                    action = Some(Action::Share(clip.path.clone(), c));
                }
            });
        }

        let hint = match (selecting, share::CAN_DRAG_OUT) {
            (true, _) => "Click to select",
            (false, true) => "Click to play · drag into any app to share",
            (false, false) => "Click to play",
        };
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(format!("{}\n{hint}", clip.name));
        // Once the pointer has moved a little with the button held, it's a drag:
        // hand it to the OS so the clip can be dropped into Discord, Finder, a
        // browser… (egui alone can't drag outside its own window).
        if share::CAN_DRAG_OUT && resp.drag_started() && action.is_none() && !selecting {
            let preview = thumbs::cached_jpeg(clip);
            action = Some(Action::DragOut(clip.path.clone(), preview));
        }
        if resp.clicked() && action.is_none() && render.is_none() {
            action = Some(if modifiers.shift && (selecting || modifiers.command) {
                Action::Select { path: clip.path.clone(), range: true }
            } else if selecting || modifiers.command {
                Action::Select { path: clip.path.clone(), range: false }
            } else {
                Action::Open(clip.path.clone())
            });
        }
        let n = self.selection.paths.len();
        let uploading = self.cloud.upload_for(&clip.path).is_some();
        resp.context_menu(|ui| {
            ui.set_min_width(190.0);
            // Right-clicking one of several selected clips acts on all of them.
            if selected && n > 1 {
                if !folders.is_empty() {
                    ui.menu_button(format!("📁  Move {n} clips to"), |ui| {
                        if let Some(f) = move_menu(ui, folders, None, true) {
                            action = Some(Action::MoveSelectedTo(f));
                        }
                    });
                }
                if ui.button(egui::RichText::new(format!("🗑  Move {n} clips to Trash")).color(v.error_fg_color)).clicked() {
                    action = Some(Action::TrashSelected);
                }
                if ui.button("Deselect all").clicked() {
                    action = Some(Action::Deselect);
                }
                return;
            }
            if ui.button("▶  Play").clicked() {
                action = Some(Action::Open(clip.path.clone()));
            }
            if ui.button("↗  Open in default player").clicked() {
                action = Some(Action::OpenExternal(clip.path.clone()));
            }
            if ui.button("✂  Edit…").clicked() {
                action = Some(Action::Edit(clip.path.clone()));
            }
            if ui.button("✏  Rename…").clicked() {
                action = Some(Action::Rename(clip.path.clone()));
            }
            if !folders.is_empty() {
                ui.menu_button("📁  Move to", |ui| {
                    if let Some(f) = move_menu(ui, folders, clip.folder.as_deref(), clip.folder.is_some()) {
                        action = Some(Action::MoveTo(vec![clip.path.clone()], f));
                    }
                });
            }
            if !selected && ui.button("☑  Select").clicked() {
                action = Some(Action::Select { path: clip.path.clone(), range: false });
            }
            ui.separator();
            if uploading {
                if ui.button("✕  Cancel upload").clicked() {
                    action = Some(Action::CancelUpload(clip.path.clone()));
                }
            } else if let Some(c) = share_menu(ui, ui.ctx().pointer_latest_pos().unwrap_or_default()) {
                action = Some(Action::Share(clip.path.clone(), c));
            }
            ui.separator();
            if ui.button(egui::RichText::new("🗑  Move to Trash").color(v.error_fg_color)).clicked() {
                action = Some(Action::Trash(clip.path.clone()));
            }
        });
        action
    }
}

impl App {
    pub(crate) fn rename_clip(&mut self, path: PathBuf) {
        let name = self.clips.iter().find(|c| c.path == path).map(|c| c.editable_name()).unwrap_or_default();
        self.rename = Some(Rename { path, name, error: None, focused: false, folder: false });
    }

    fn rename_folder(&mut self, name: &str) {
        let path = self.settings.output_dir.join(name);
        self.rename = Some(Rename { path, name: name.to_owned(), error: None, focused: false, folder: true });
    }

    /// Rename one of the library's folders. Its game's clips keep going in it.
    fn commit_folder_rename(&mut self, ctx: &egui::Context, from: &Path, name: &str) -> Result<(), String> {
        let to = clips::path_for_folder(&self.settings.output_dir, name, from)?;
        if to == from {
            return Ok(());
        }
        if self.renders.iter().any(|j| j.source.starts_with(from)) {
            return Err("Wait for the edits being saved in it to finish.".into());
        }
        std::fs::rename(from, &to).map_err(|e| format!("Couldn't rename: {e}"))?;
        let inside: Vec<PathBuf> = self.clips.iter().filter(|c| c.path.parent() == Some(from)).map(|c| c.path.clone()).collect();
        for path in inside {
            if let Some(file) = path.file_name() {
                self.follow_rename(ctx, &path, &to.join(file));
            }
        }
        let (old, new) = (file_name_of(from), file_name_of(&to));
        // New clips of its game go in it from now on.
        let folders = &mut self.settings.game_folders;
        let mut followed = false;
        for f in folders.values_mut().filter(|f| **f == old) {
            *f = new.clone();
            followed = true;
        }
        if !followed {
            folders.insert(old.clone(), new.clone());
        }
        folders.retain(|game, f| game != f);
        if self.library_filter == Filter::Folder(old) {
            self.library_filter = Filter::Folder(new);
        }
        self.refresh_clips();
        Ok(())
    }

    /// The "Rename clip" dialog, if open.
    pub(crate) fn rename_dialog(&mut self, ctx: &egui::Context) {
        let Some(r) = &mut self.rename else { return };
        let mut close = false;
        let mut submit = false;
        let modal = egui::Modal::new(egui::Id::new("rename_clip")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.heading(if r.folder { "Rename folder" } else { "Rename clip" });
            ui.add_space(6.0);
            let out = egui::TextEdit::singleline(&mut r.name).desired_width(f32::INFINITY).show(ui);
            if !r.focused {
                // Open with the whole name selected, ready to type over.
                out.response.request_focus();
                let mut state = out.state.clone();
                state.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                    egui::text::CCursor::new(0),
                    egui::text::CCursor::new(r.name.chars().count()),
                )));
                state.store(ui.ctx(), out.response.id);
                r.focused = true;
            }
            submit = out.response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if let Some(e) = &r.error {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.add(egui::Button::new(egui::RichText::new("Rename").color(Color32::WHITE)).fill(ACCENT)).clicked() {
                    submit = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if submit && r.folder {
            let (from, name) = (r.path.clone(), r.name.clone());
            match self.commit_folder_rename(ctx, &from, &name) {
                Ok(()) => close = true,
                Err(e) => {
                    if let Some(r) = &mut self.rename {
                        r.error = Some(e);
                    }
                }
            }
        } else if submit {
            let ext = r.path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or("mp4".into());
            let dir = r.path.parent().unwrap_or(std::path::Path::new(".")).to_path_buf();
            match clips::path_for_name(&dir, &r.name, &ext, Some(&r.path)) {
                Ok(to) if to == r.path => close = true,
                Ok(to) => match clips::rename(&r.path, &to) {
                    Ok(()) => {
                        // Open in the player or editor: it follows the file.
                        let from = r.path.clone();
                        self.follow_rename(ctx, &from, &to);
                        close = true;
                        self.refresh_clips();
                    }
                    Err(e) => r.error = Some(format!("Couldn't rename: {e}")),
                },
                Err(e) => r.error = Some(e),
            }
        }
        if close || (!submit && modal.should_close()) {
            self.rename = None;
        }
    }
}

/// A way to get a clip out, picked from [`share_menu`].
pub(crate) enum ShareChoice {
    Copy,
    /// The system share sheet, at this window position.
    Sheet(Pos2),
    Upload,
    Reveal,
}

/// The ways to get a clip out, shared by the library's Share button and
/// right-click menu and the player's Share button. Ordered by how often
/// people reach for them.
pub(crate) fn share_menu(ui: &mut egui::Ui, anchor: Pos2) -> Option<ShareChoice> {
    let mut choice = None;
    ui.set_min_width(230.0);
    let paste = crate::hotkey_label_cmd("V");
    if ui.button("📋  Copy clip").on_hover_text(format!("Then paste it into Discord, a chat or a folder ({paste})")).clicked() {
        choice = Some(ShareChoice::Copy);
    }
    if share::HAS_SHARE_SHEET && ui.button(share::SHARE_SHEET_LABEL).clicked() {
        choice = Some(ShareChoice::Sheet(anchor));
    }
    if ui.button("☁  Upload to HesteFiles…").clicked() {
        choice = Some(ShareChoice::Upload);
    }
    if ui.button(format!("📂  {}", crate::reveal_label())).clicked() {
        choice = Some(ShareChoice::Reveal);
    }
    if share::CAN_DRAG_OUT {
        ui.separator();
        ui.weak("Tip: drag the clip into any app");
    }
    if choice.is_some() {
        ui.close();
    }
    choice
}

impl App {
    /// Do what was picked in [`share_menu`] for `file`.
    pub(crate) fn share(&mut self, frame: &eframe::Frame, file: PathBuf, choice: ShareChoice) {
        match choice {
            ShareChoice::Copy => match share::copy_file(&file) {
                Ok(()) => self.toast(format!("Copied — paste it into any app ({})", crate::hotkey_label_cmd("V"))),
                Err(e) => self.toast_error(format!("Couldn't copy the clip: {e}")),
            },
            ShareChoice::Sheet(at) => {
                if let Err(e) = share::share_sheet(frame, &file, at) {
                    self.toast_error(format!("Couldn't open sharing: {e}"));
                }
            }
            ShareChoice::Upload => self.open_share_dialog(file),
            ShareChoice::Reveal => {
                if let Err(e) = clips::reveal_in_file_manager(&file) {
                    self.toast_error(format!("Couldn't show the file: {e}"));
                }
            }
        }
    }

    /// Hand a drag of `file` to the OS, so it can be dropped into any app.
    pub(crate) fn drag_out(&mut self, ctx: &egui::Context, frame: &eframe::Frame, file: &Path, preview: Option<PathBuf>) {
        // egui must forget its own drag, or what was dragged would stay
        // "grabbed" after the drop.
        ctx.stop_dragging();
        if let Err(e) = share::start_drag(frame, file, preview) {
            self.toast_error(format!("Couldn't start the drag: {e}"));
        }
    }
}

/// The "Edited" marker in a card's caption: a small accent pill. Returns its width.
fn edited_pill(p: &egui::Painter, at: Pos2) -> f32 {
    let galley = p.layout_no_wrap("✂ Edited".to_owned(), FontId::proportional(11.5), Color32::WHITE);
    let rect = Rect::from_min_size(at, galley.size() + Vec2::new(12.0, 3.0));
    p.rect_filled(rect, 8, ACCENT);
    p.galley(rect.min + Vec2::new(6.0, 1.5), galley, Color32::WHITE);
    rect.width()
}

/// A card's selection circle: hollow when not selected, a filled check when it is.
fn check_circle(p: &egui::Painter, c: Pos2, selected: bool, hovered: bool) {
    if selected {
        p.circle_filled(c, 12.0, ACCENT);
        p.circle_stroke(c, 12.0, Stroke::new(1.5, Color32::WHITE));
        let tick = vec![c + Vec2::new(-5.5, 0.5), c + Vec2::new(-1.5, 4.5), c + Vec2::new(6.0, -4.0)];
        p.add(egui::Shape::line(tick, Stroke::new(2.2, Color32::WHITE)));
    } else {
        p.circle_filled(c, 12.0, Color32::from_black_alpha(if hovered { 140 } else { 90 }));
        p.circle_stroke(c, 11.0, Stroke::new(1.8, Color32::WHITE));
    }
}

/// Soft pulsing dot (the same "live" cue as the recording indicator).
fn pulse(p: &egui::Painter, c: Pos2, color: Color32, time: f64) {
    let phase = (time * 2.5).sin() as f32 * 0.5 + 0.5;
    p.circle_filled(c, 9.0 + 5.0 * phase, color.gamma_multiply(0.18 + 0.12 * (1.0 - phase)));
    p.circle_filled(c, 7.0, color);
}

/// Thin progress bar along the bottom of a thumbnail.
fn progress_bar(p: &egui::Painter, thumb: Rect, f: f32, color: Color32) {
    let track = Rect::from_min_max(Pos2::new(thumb.left() + 14.0, thumb.bottom() - 16.0), Pos2::new(thumb.right() - 14.0, thumb.bottom() - 11.0));
    p.rect_filled(track, 3, Color32::from_white_alpha(30));
    let fill = Rect::from_min_max(track.min, Pos2::new(track.left() + track.width() * f.clamp(0.0, 1.0), track.bottom()));
    p.rect_filled(fill, 3, color);
}

fn badge(p: &egui::Painter, pos: Pos2, anchor: Align2, text: &str, fill: Color32) {
    let galley = p.layout_no_wrap(text.to_owned(), FontId::proportional(11.5), Color32::WHITE);
    let rect = anchor.anchor_size(pos, galley.size() + Vec2::new(10.0, 4.0));
    p.rect_filled(rect, 4, fill);
    p.galley(rect.min + Vec2::new(5.0, 2.0), galley, Color32::WHITE);
}

/// The folders to move clips to: each of the library's but `current`, and
/// Other (out of any folder) when `offer_other`. The one picked, if any.
fn move_menu(ui: &mut egui::Ui, folders: &[String], current: Option<&str>, offer_other: bool) -> Option<Option<String>> {
    let mut picked = None;
    for f in folders.iter().filter(|f| Some(f.as_str()) != current) {
        if ui.button(f).clicked() {
            picked = Some(Some(f.clone()));
        }
    }
    if offer_other {
        ui.separator();
        if ui.button("Other").on_hover_text("Out of any game's folder").clicked() {
            picked = Some(None);
        }
    }
    picked
}

fn file_name_of(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn single_line(text: &str, font: FontId, color: Color32, max_width: f32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_width);
    job
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(name: &str) -> clips::Clip {
        clips::Clip {
            path: PathBuf::from(name),
            name: name.into(),
            modified: std::time::SystemTime::UNIX_EPOCH,
            size_bytes: 0,
            id: None,
            original: None,
            folder: None,
        }
    }

    #[test]
    fn select_toggle_range_and_prune() {
        let lib: Vec<_> = ["a", "b", "c", "d", "e"].map(clip).into();
        let p = |n: &str| PathBuf::from(n);
        let mut s = Selection::default();
        s.toggle(&p("b"));
        s.extend_to(&p("d"), &lib);
        assert_eq!(s.paths, ["b", "c", "d"].map(p).into());
        // Ranges work backwards too, and keep what's already selected.
        s.toggle(&p("e"));
        s.extend_to(&p("a"), &lib);
        assert_eq!(s.paths.len(), 5);
        s.toggle(&p("c"));
        assert!(!s.paths.contains(&p("c")));
        // Clips that vanish drop out of the selection.
        s.retain(&lib[..2]);
        assert_eq!(s.paths, ["a", "b"].map(p).into());
        s.clear();
        assert!(s.is_empty());
    }
}
