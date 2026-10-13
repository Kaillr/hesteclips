//! The clip library: a thumbnail grid grouped by day, newest first.
//!
//! Click a card to play it; hover for a Share button; right-click for everything
//! else. While a capture is running a placeholder card stands in for the clip, so
//! a half-written file never shows up as if it were finished.
//!
//! A sidebar picks what's shown: every clip, one game's (the library's
//! folders), or one of your collections — your own groups of clips, like
//! "Awesome ace clutches" (`collections`). A narrow window puts the same list
//! in a menu on the page's title.
//!
//! Dragging clips works like a file manager: inside the window they can be
//! dropped on a game (moved there) or a collection (added to it); taken out of
//! the window, they're handed to the OS to drop into any app.
//!
//! Selecting works like Photos and Finder: the check circle on a card, ⌘-click
//! or Shift-click start a selection; while one is active a plain click toggles a
//! card instead of playing it, and a bar on top acts on all of them.

use crate::ui_kit::Dismissed as _;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use egui::{Color32, Pos2, Sense};

use crate::collections::Collections;
use crate::theme;
use crate::ui_kit;
use crate::{App, clips, share, store};

pub(crate) const REC_RED: Color32 = crate::ui_kit::DANGER;
pub(crate) const ACCENT: Color32 = crate::ui_kit::ACCENT;

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
    /// One of your collections, by id.
    Collection(String),
}

impl Filter {
    pub(crate) fn matches(&self, clip: &clips::Clip, cols: &Collections) -> bool {
        match self {
            Filter::All => true,
            Filter::Folder(name) => clip.folder.as_ref() == Some(name),
            Filter::Loose => clip.folder.is_none(),
            Filter::Collection(id) => cols.contains(id, &clip.path),
        }
    }

    /// Back to all clips once there's nothing left to show: the last clip of a
    /// game moved or deleted, or the collection deleted. (An emptied collection
    /// stays: you may be about to fill it.)
    pub fn retain(&mut self, clips: &[clips::Clip], cols: &Collections) {
        let gone = match self {
            Filter::Collection(id) => cols.get(id).is_none(),
            _ => !clips.iter().any(|c| self.matches(c, cols)),
        };
        if gone {
            *self = Filter::All;
        }
    }

    /// Make sure the clip at `path` is shown: all clips, if it isn't here.
    pub fn reveal(&mut self, clips: &[clips::Clip], cols: &Collections, path: &Path) {
        if clips.iter().any(|c| c.path == path && !self.matches(c, cols)) {
            *self = Filter::All;
        }
    }
}

/// Something to show in the library, as the sidebar lists it.
#[derive(Clone)]
pub(crate) struct View {
    pub filter: Filter,
    pub label: String,
    pub count: usize,
}

/// What the sidebar lists: the games (the library's folders, by name, with
/// Desktop and then Other — clips in no folder — last, as they aren't games)
/// and the collections (by name). No games when the library has no folders.
pub(crate) fn views(clips: &[clips::Clip], cols: &Collections) -> (Vec<View>, Vec<View>) {
    let mut games: Vec<View> = Vec::new();
    let mut loose = 0;
    // Newest first, so a folder's place is its newest clip's.
    for clip in clips {
        match &clip.folder {
            Some(f) => match games.iter_mut().find(|c| &c.label == f) {
                Some(c) => c.count += 1,
                None => games.push(View { filter: Filter::Folder(f.clone()), label: f.clone(), count: 1 }),
            },
            None => loose += 1,
        }
    }
    // A fixed order, so nothing moves when a clip is saved.
    games.sort_by_key(|v| (v.label == clips::DESKTOP, v.label.to_lowercase()));
    if !games.is_empty() && loose > 0 {
        games.push(View { filter: Filter::Loose, label: "Other".into(), count: loose });
    }
    let mut collections: Vec<View> = cols
        .list()
        .iter()
        .map(|c| {
            let filter = Filter::Collection(c.id.clone());
            let count = clips.iter().filter(|clip| filter.matches(clip, cols)).count();
            View { filter, label: c.name.clone(), count }
        })
        .collect();
    collections.sort_by_key(|v| v.label.to_lowercase());
    (games, collections)
}

/// Clips being dragged inside the window.
pub(crate) struct CardDrag {
    pub files: Vec<PathBuf>,
    /// The clip the drag started on, for the picture under the pointer.
    pub clip: PathBuf,
    /// What dropping now would do, as the panel found it this frame.
    pub target: Option<(DropOn, String)>,
}

/// Where dragged clips can go.
#[derive(Clone)]
pub(crate) enum DropOn {
    View(Filter),
    NewCollection,
}

/// The "New collection", "Rename collection" or "Delete collection" dialog.
pub(crate) struct CollectionDialog {
    kind: CollectionDialogKind,
    name: String,
    error: Option<String>,
    focused: bool,
}

enum CollectionDialogKind {
    /// Make one, with these clips in it.
    New(Vec<PathBuf>),
    Rename(String),
    Delete(String),
}

/// Clips picked for a bulk action.
#[derive(Default)]
pub(crate) struct Selection {
    pub paths: HashSet<PathBuf>,
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

    pub fn toggle(&mut self, path: &PathBuf) {
        if !self.paths.remove(path) {
            self.paths.insert(path.clone());
        }
        self.anchor = Some(path.clone());
    }

    /// Add every clip from the anchor to `path`, in library order.
    pub fn extend_to(&mut self, path: &PathBuf, clips: &[clips::Clip]) {
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

pub(crate) enum Action {
    /// Drag the file out of the window (to another app / the desktop).
    DragOut(Vec<PathBuf>, Option<PathBuf>),
    /// Start dragging clips inside the window (from this clip's card).
    StartDrag(Vec<PathBuf>, PathBuf),
    /// Dragged clips were dropped here.
    Drop(Vec<PathBuf>, DropOn),
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
    /// Ask for a name, then make a collection with these clips in it.
    NewCollection(Vec<PathBuf>),
    /// Put these clips in the collection, or take them out (`false`).
    InCollection(Vec<PathBuf>, String, bool),
    RenameCollection(String),
    DeleteCollection(String),
    /// Toggle a card's selection, or (`range`) select up to it from the anchor.
    Select { path: PathBuf, range: bool },
    SelectAll,
    Deselect,
    /// Delete the selected clips; `true`: ask first (a key was pressed, which
    /// doesn't say what it does).
    TrashSelected(bool),
    CancelUpload(PathBuf),
}

pub(crate) enum Card<'a> {
    /// Recording or saving a replay clip.
    Placeholder,
    /// A "Save as new clip" render that has no file yet.
    NewRender(usize),
    Clip(&'a clips::Clip),
}

impl App {
    /// Move clips (and their edits) to the Bin. Clips with a save in progress are
    /// left alone: their file is about to be replaced.
    /// Delete clips the way the setting says: to the Recycle Bin, or for good.
    /// Asks first when deleting for good, or when `ask` (a key or a plain
    /// "Delete" button, which don't say where the clip goes); a menu item
    /// that says "Move to Recycle Bin" just does it.
    pub(crate) fn delete_clips(&mut self, paths: &[PathBuf], ask: bool) {
        if paths.is_empty() {
            return;
        }
        if self.settings.delete_permanently || ask {
            self.confirm_delete = Some(paths.to_vec());
        } else {
            self.delete_now(paths);
        }
    }

    /// "Move … to the Recycle Bin?" or "Delete … for good?", while clips wait
    /// for it, with the clip's picture and name when it's one. Enter confirms.
    pub(crate) fn delete_dialog(&mut self, ctx: &egui::Context) {
        let Some(paths) = self.confirm_delete.clone() else { return };
        let permanently = self.settings.delete_permanently;
        let bin = store::bin_name();
        let one = match paths.as_slice() {
            [one] => self.clips.iter().find(|c| &c.path == one).cloned(),
            _ => None,
        };
        let what = if paths.len() == 1 { "it" } else { "them" };
        let title = match (permanently, paths.len()) {
            (true, 1) => "Delete for good?".to_owned(),
            (true, n) => format!("Delete {n} clips for good?"),
            (false, 1) => format!("Move to the {bin}?"),
            (false, n) => format!("Move {n} clips to the {bin}?"),
        };
        let tex = one.as_ref().and_then(|c| self.thumbs.get(ctx, c).and_then(|t| t.texture.clone()));
        let (mut yes, mut no) = (false, false);
        let modal = ui_kit::dialog(ctx, "confirm_delete", 380.0, |ui| {
            ui_kit::dialog_title(ui, &title);
            if let Some(c) = &one {
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(egui::vec2(80.0, 45.0), Sense::hover());
                    match &tex {
                        Some(t) => {
                            let uv = crate::filmstrip::crop_uv(t.size_vec2(), r.size());
                            egui::Image::from_texture((t.id(), r.size())).uv(uv).corner_radius(4).paint_at(ui, r);
                        }
                        None => {
                            ui.painter().rect_filled(r, 4, theme::pal(ui).raise2);
                        }
                    }
                    ui.add_space(4.0);
                    ui.add(egui::Label::new(theme::text(c.title(), 13.5, theme::W::Medium).color(theme::pal(ui).text)).truncate());
                });
                ui.add_space(8.0);
            }
            ui_kit::dialog_text(ui, &if permanently { format!("You can't get {what} back.") } else { format!("You can get {what} back from the {bin}.") });
            ui_kit::dialog_buttons(ui, |ui| {
                let label = if permanently { "Delete".to_owned() } else { format!("Move to {bin}") };
                yes = ui.add(ui_kit::danger_button(label)).clicked();
                no = ui.add(ui_kit::button("Cancel", false).ghost()).clicked();
            });
            // Enter deletes, as the default action of a confirmation.
            yes |= ui.input(|i| i.key_pressed(egui::Key::Enter));
        });
        if yes {
            self.confirm_delete = None;
            self.delete_now(&paths);
        } else if no || modal.dismissed() {
            self.confirm_delete = None;
        }
    }

    /// Delete clips now. The player moves on to the next clip if it was
    /// showing one of them.
    fn delete_now(&mut self, paths: &[PathBuf]) {
        let permanently = self.settings.delete_permanently;
        // Where the player goes if its clip goes: the next clip in what's
        // shown, else the one before.
        let after = self.viewer.as_ref().map(|v| v.clip().to_path_buf()).filter(|c| paths.contains(c)).map(|current| {
            let shown: Vec<&clips::Clip> = self.clips.iter().filter(|c| self.library_filter.matches(c, &self.collections)).collect();
            let at = shown.iter().position(|c| c.path == current);
            let left = |c: &&&clips::Clip| !paths.contains(&c.path);
            at.and_then(|i| shown[i + 1..].iter().find(left).or_else(|| shown[..i].iter().rev().find(left))).map(|c| c.path.clone())
        });
        // Let go of the clips first: Windows won't delete a file that's open,
        // and while a clip loads its sound decoders, scrub preview and the
        // player all have it open.
        if after.is_some() {
            self.viewer = None;
        }
        for p in paths {
            media::pcm::release(p);
            crate::proxy::release(p);
        }
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
            match delete_when_free(&clip, &library, permanently) {
                Ok(()) => moved.push(clip.path),
                Err(e) => {
                    library.insert(i, clip);
                    error = Some(e);
                }
            }
        }
        self.collections.forget(&moved);
        let bin = store::bin_name();
        let what = match moved.as_slice() {
            [one] => crate::clips::title_for_stem(&one.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()),
            _ => format!("{} clips", moved.len()),
        };
        if let Some(e) = error {
            self.toast_error(format!("Couldn't delete: {e}"));
        } else if busy > 0 {
            self.toast_error("Clips that are still saving an edit were kept.");
        } else if !moved.is_empty() && permanently {
            self.toast_icon(egui_phosphor::regular::TRASH, format!("Deleted {what}"));
        } else if !moved.is_empty() {
            self.toast_icon(egui_phosphor::regular::TRASH, format!("Moved {what} to the {bin}"));
        }
        self.selection.clear();
        self.refresh_clips();
        if let Some(next) = after {
            match next {
                Some(clip) => self.open_viewer(clip),
                None => {
                    self.viewer = None;
                    self.page = crate::Page::Clips;
                }
            }
        }
    }

    /// "Move to Recycle Bin" or "Delete…" (it asks), for `n` clips if given.
    pub(crate) fn delete_label(&self, n: Option<usize>) -> String {
        match (self.settings.delete_permanently, n) {
            (true, Some(n)) => format!("Delete {n} clips…"),
            (true, None) => "Delete…".to_owned(),
            (false, Some(n)) => format!("Move {n} clips to {}", store::bin_name()),
            (false, None) => format!("Move to {}", store::bin_name()),
        }
    }


    /// Ask for a name for a new collection with these clips in it.
    pub(crate) fn new_collection_with(&mut self, paths: Vec<PathBuf>) {
        self.collection_dialog = Some(CollectionDialog { kind: CollectionDialogKind::New(paths), name: String::new(), error: None, focused: false });
    }

    /// Put clips in a collection, or take them out.
    pub(crate) fn put_in_collection(&mut self, paths: &[PathBuf], id: &str, add: bool) {
        let Some(name) = self.collections.get(id).map(|c| c.name.clone()) else { return };
        if add {
            let n = self.collections.add(id, paths);
            match (paths.len(), n) {
                (_, 0) => self.toast_icon(egui_phosphor::regular::FOLDER_PLUS, format!("Already in {name}")),
                (1, _) => self.toast_icon(egui_phosphor::regular::FOLDER_PLUS, format!("Added to {name}")),
                _ => self.toast_icon(egui_phosphor::regular::FOLDER_PLUS, format!("Added {n} clips to {name}")),
            }
        } else {
            self.collections.remove(id, paths);
            self.toast_icon(egui_phosphor::regular::MINUS_CIRCLE, if paths.len() == 1 { format!("Removed from {name}") } else { format!("Removed {} clips from {name}", paths.len()) });
        }
        self.selection.clear();
    }

    pub(crate) fn rename_collection(&mut self, id: String) {
        let name = self.collections.get(&id).map(|c| c.name.clone()).unwrap_or_default();
        self.collection_dialog = Some(CollectionDialog { kind: CollectionDialogKind::Rename(id), name, error: None, focused: false });
    }

    pub(crate) fn delete_collection(&mut self, id: String) {
        self.collection_dialog = Some(CollectionDialog { kind: CollectionDialogKind::Delete(id), name: String::new(), error: None, focused: false });
    }

    /// The "New collection", "Rename collection" and "Delete collection" dialogs.
    pub(crate) fn collection_dialog(&mut self, ctx: &egui::Context) {
        let Some(d) = &mut self.collection_dialog else { return };
        let (mut close, mut submit) = (false, false);
        let modal = ui_kit::dialog(ctx, "collection_dialog", 380.0, |ui| {
            if let CollectionDialogKind::Delete(id) = &d.kind {
                let c = self.collections.get(id);
                ui_kit::dialog_title(ui, &format!("Delete “{}”?", c.map_or("", |c| c.name.as_str())));
                ui_kit::dialog_text(ui, "The collection goes away. Its clips stay in your library.");
                ui_kit::dialog_buttons(ui, |ui| {
                    submit = ui.add(ui_kit::danger_button("Delete collection")).clicked();
                    close = ui.add(ui_kit::button("Cancel", false).ghost()).clicked();
                });
                submit |= ui.input(|i| i.key_pressed(egui::Key::Enter));
                return;
            }
            let (heading, button) = match &d.kind {
                CollectionDialogKind::New(clips) if clips.len() == 1 => ("New collection".to_owned(), "Create and add the clip".to_owned()),
                CollectionDialogKind::New(clips) if !clips.is_empty() => ("New collection".to_owned(), format!("Create and add {} clips", clips.len())),
                CollectionDialogKind::New(_) => ("New collection".to_owned(), "Create".to_owned()),
                _ => ("Rename collection".to_owned(), "Rename".to_owned()),
            };
            ui_kit::dialog_title(ui, &heading);
            let out = ui_kit::text_field(ui, &mut d.name, "Like “Best of the week”");
            if !d.focused {
                out.response.request_focus();
                let mut state = out.state.clone();
                state.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(d.name.chars().count()))));
                state.store(ui.ctx(), out.response.id);
                d.focused = true;
            }
            submit = out.response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if let Some(e) = &d.error {
                ui.add_space(4.0);
                ui.label(theme::text(e, 12.5, theme::W::Regular).color(theme::pal(ui).danger));
            }
            ui_kit::dialog_buttons(ui, |ui| {
                if ui.add(ui_kit::button(button, true)).clicked() {
                    submit = true;
                }
                if ui.add(ui_kit::button("Cancel", false).ghost()).clicked() {
                    close = true;
                }
            });
        });
        if submit {
            let d = self.collection_dialog.as_mut().unwrap();
            match &d.kind {
                CollectionDialogKind::Delete(id) => {
                    let id = id.clone();
                    self.collections.delete(&id);
                    self.library_filter.retain(&self.clips, &self.collections);
                    close = true;
                }
                CollectionDialogKind::Rename(id) => match crate::collections::check_name(&self.collections, &d.name, Some(id)) {
                    Ok(name) => {
                        let id = id.clone();
                        self.collections.rename(&id, &name);
                        close = true;
                    }
                    Err(e) => d.error = Some(e),
                },
                CollectionDialogKind::New(paths) => match crate::collections::check_name(&self.collections, &d.name, None) {
                    Ok(name) => {
                        let paths = paths.clone();
                        let id = self.collections.create(&name);
                        if paths.is_empty() {
                            // Made to fill: show it, with how to.
                            self.library_filter = Filter::Collection(id);
                            self.selection.clear();
                        } else {
                            self.put_in_collection(&paths, &id, true);
                        }
                        close = true;
                    }
                    Err(e) => d.error = Some(e),
                },
            }
        }
        if close || (!submit && modal.dismissed()) {
            self.collection_dialog = None;
        }
    }

    /// The game's picture for one of the library's folders, when Discord knows
    /// the game (also after the folder's been renamed).
    pub(crate) fn folder_icon(&mut self, ctx: &egui::Context, folder: &str) -> Option<egui::TextureHandle> {
        let game = self.settings.game_folders.iter().find(|(_, f)| *f == folder).map_or(folder, |(game, _)| game.as_str());
        let url = crate::discord::game_for_folder(game)?.icon?;
        self.web_images.icon(ctx, &url)
    }

    /// Move clips to another folder of the library (`None`: the library
    /// itself). Their edits follow by id. Clips busy saving an edit or
    /// uploading stay where they are.
    pub(crate) fn move_clips(&mut self, ctx: &egui::Context, paths: &[PathBuf], folder: Option<String>) {
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
            self.toast_icon(egui_phosphor::regular::FOLDERS, format!("Moved to {place}"));
        } else if moved > 1 {
            self.toast_icon(egui_phosphor::regular::FOLDERS, format!("Moved {moved} clips to {place}"));
        }
        self.selection.clear();
        self.refresh_clips();
    }

    /// A clip's file moved (renamed, or to another folder): the player, the
    /// editor and the "new" highlight follow it.
    pub(crate) fn follow_rename(&mut self, ctx: &egui::Context, from: &Path, to: &Path) {
        self.collections.follow(from, to);
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
}

impl App {
    pub(crate) fn rename_clip(&mut self, path: PathBuf) {
        let name = self.clips.iter().find(|c| c.path == path).map(|c| c.editable_name()).unwrap_or_default();
        self.rename = Some(Rename { path, name, error: None, focused: false, folder: false });
    }

    pub(crate) fn rename_folder(&mut self, name: &str) {
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
        let modal = ui_kit::dialog(ctx, "rename_clip", 380.0, |ui| {
            ui_kit::dialog_title(ui, if r.folder { "Rename folder" } else { "Rename clip" });
            let out = ui_kit::text_field(ui, &mut r.name, "");
            if !r.focused {
                // Open with the whole name selected, ready to type over.
                out.response.request_focus();
                let mut state = out.state.clone();
                state.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(r.name.chars().count()))));
                state.store(ui.ctx(), out.response.id);
                r.focused = true;
            }
            submit = out.response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if let Some(e) = &r.error {
                ui.add_space(4.0);
                ui.label(theme::text(e, 12.5, theme::W::Regular).color(theme::pal(ui).danger));
            }
            ui_kit::dialog_buttons(ui, |ui| {
                if ui.add(ui_kit::button("Rename", true)).clicked() {
                    submit = true;
                }
                if ui.add(ui_kit::button("Cancel", false).ghost()).clicked() {
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
        if close || (!submit && modal.dismissed()) {
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
    Mp3,
}

/// The ways to get a clip out, shared by the library's Share button and
/// right-click menu and the player's Share button.
pub(crate) fn share_menu(ui: &mut egui::Ui, anchor: Pos2) -> Option<ShareChoice> {
    crate::library_view::share_items(ui, anchor, false)
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
            ShareChoice::Mp3 => self.save_mp3(file),
        }
    }

    /// Ask where, then save the clip's sound there as an MP3 in the background.
    fn save_mp3(&mut self, clip: PathBuf) {
        let name = clip.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Clip".into());
        let picked = rfd::FileDialog::new()
            .set_title("Save audio as MP3")
            .set_file_name(format!("{name}.mp3"))
            .add_filter("MP3 audio", &["mp3"])
            .save_file();
        let Some(mut out) = picked else { return };
        if !out.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp3")) {
            let mut s = out.into_os_string();
            s.push(".mp3");
            out = s.into();
        }
        let (tx, ctx) = (self.mp3_tx.clone(), self.ctx());
        std::thread::spawn(move || {
            let result = share::save_mp3(&clip, &out).map(|()| out);
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }

    /// Finished MP3s: say so.
    pub(crate) fn pump_mp3s(&mut self) {
        while let Ok(result) = self.mp3_rx.try_recv() {
            match result {
                Ok(out) => self.toast(format!("Saved “{}”", out.file_name().unwrap_or_default().to_string_lossy())),
                Err(e) => self.toast_error(format!("Couldn't save the MP3: {e}")),
            }
        }
    }

    /// Hand a drag of `files` to the OS, so they can be dropped into any app.
    pub(crate) fn drag_out(&mut self, ctx: &egui::Context, frame: &eframe::Frame, files: Vec<PathBuf>, preview: Option<PathBuf>) {
        // egui must forget its own drag, or what was dragged would stay
        // "grabbed" after the drop.
        ctx.stop_dragging();
        if let Err(e) = share::start_drag(frame, files, preview) {
            self.toast_error(format!("Couldn't start the drag: {e}"));
        }
    }
}

/// Delete a clip, waiting a moment if it's still open (decoders closing
/// after being told to stop: a few ms, rarely more).
fn delete_when_free(clip: &clips::Clip, library: &[clips::Clip], permanently: bool) -> Result<(), String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        match store::delete(clip, library, permanently) {
            // "Used by another process" (os error 32; 0x80070020 through the Recycle Bin).
            Err(e) if (e.contains("os error 32") || e.contains("used by another process") || e.contains("-2147024864")) && std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            r => return r,
        }
    }
}

fn file_name_of(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(name: &str) -> clips::Clip {
        clips::Clip {
            path: PathBuf::from(name),
            name: name.into(),
            modified: std::time::SystemTime::UNIX_EPOCH, created: None,
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

/// The days folded away in the library, kept in its `.hesteclips` folder so
/// they stay folded next time. A new day starts open.
pub(crate) struct FoldedDays {
    lib: PathBuf,
    days: HashSet<chrono::NaiveDate>,
}

impl FoldedDays {
    pub(crate) fn load(lib: &Path) -> Self {
        let days = std::fs::read(Self::file(lib))
            .ok()
            .and_then(|b| serde_json::from_slice::<Vec<String>>(&b).ok())
            .unwrap_or_default()
            .iter()
            .filter_map(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
            .collect();
        Self { lib: lib.to_path_buf(), days }
    }

    fn file(lib: &Path) -> PathBuf {
        lib.join(store::DIR).join("folded-days.json")
    }

    pub(crate) fn library(&self) -> &Path {
        &self.lib
    }

    pub(crate) fn contains(&self, day: &chrono::NaiveDate) -> bool {
        self.days.contains(day)
    }

    /// Fold a day away (`true`) or open it.
    pub(crate) fn set(&mut self, day: chrono::NaiveDate, folded: bool) {
        self.set_all(&[day], folded);
    }

    pub(crate) fn set_all(&mut self, days: &[chrono::NaiveDate], folded: bool) {
        let before = self.days.len();
        for d in days {
            if folded {
                self.days.insert(*d);
            } else {
                self.days.remove(d);
            }
        }
        if self.days.len() != before {
            self.save();
        }
    }

    fn save(&self) {
        let path = Self::file(&self.lib);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut days: Vec<String> = self.days.iter().map(|d| d.format("%Y-%m-%d").to_string()).collect();
        days.sort();
        if let Ok(json) = serde_json::to_vec_pretty(&days) {
            // Written aside, then swapped in: never half a file.
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, &path).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
}

#[cfg(test)]
mod folded_tests {
    use super::*;

    #[test]
    fn folded_days_stay_folded() {
        let lib = std::env::temp_dir().join(format!("hc-folded-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&lib);
        let (a, b) = (chrono::NaiveDate::from_ymd_opt(2026, 10, 5).unwrap(), chrono::NaiveDate::from_ymd_opt(2026, 10, 6).unwrap());
        let mut f = FoldedDays::load(&lib);
        f.set_all(&[a, b], true);
        f.set(b, false);
        let again = FoldedDays::load(&lib);
        assert!(again.contains(&a) && !again.contains(&b));
        std::fs::remove_dir_all(&lib).unwrap();
    }
}
