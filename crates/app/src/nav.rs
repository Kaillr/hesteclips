//! Back and forward, as in a browser: the mouse's side buttons step through the
//! places visited — a library folder, a clip in the player, Sources, Settings.
//!
//! The editor is no place: it may hold unsaved edits, so the buttons do nothing
//! there, and going back never lands in it.

use std::path::PathBuf;

use crate::{App, Page, library};

/// How many places back are kept.
const DEPTH: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Place {
    Clips(library::Filter),
    View(PathBuf),
    Sources,
    Settings,
}

#[derive(Default)]
pub(crate) struct History {
    back: Vec<Place>,
    forward: Vec<Place>,
    current: Option<Place>,
}

impl App {
    fn place(&self) -> Option<Place> {
        Some(match self.page {
            Page::Clips => Place::Clips(self.library_filter.clone()),
            Page::View => Place::View(self.viewer.as_ref()?.clip().to_path_buf()),
            Page::Sources => Place::Sources,
            Page::Settings => Place::Settings,
            Page::Edit => return None,
        })
    }

    /// Follow the mouse's back/forward buttons, then note where we are now.
    /// Once a frame, before the page is drawn.
    pub(crate) fn navigate(&mut self, ctx: &egui::Context) {
        let free = self.page != Page::Edit && self.rename.is_none() && self.dialog.is_none();
        let (back, forward) = ctx.input(|i| (i.pointer.button_pressed(egui::PointerButton::Extra1), i.pointer.button_pressed(egui::PointerButton::Extra2)));
        if free && (back || forward) {
            self.step(back);
        }
        let Some(now) = self.place() else { return };
        if self.nav.current.as_ref() != Some(&now) {
            if let Some(was) = self.nav.current.replace(now) {
                self.nav.back.push(was);
                if self.nav.back.len() > DEPTH {
                    self.nav.back.remove(0);
                }
            }
            self.nav.forward.clear();
        }
    }

    /// One place back (or forward), skipping places that are gone: a trashed
    /// clip, a folder with no clips left.
    fn step(&mut self, back: bool) {
        loop {
            let to = if back { self.nav.back.pop() } else { self.nav.forward.pop() };
            let Some(to) = to else { return };
            if !self.can_go(&to) {
                continue;
            }
            if let Some(was) = self.nav.current.take() {
                if back { self.nav.forward.push(was) } else { self.nav.back.push(was) }
            }
            self.go(to.clone());
            self.nav.current = Some(to);
            return;
        }
    }

    fn can_go(&self, place: &Place) -> bool {
        match place {
            Place::Clips(f) => {
                let mut kept = f.clone();
                kept.retain(&self.clips);
                kept == *f
            }
            Place::View(p) => self.clips.iter().any(|c| &c.path == p),
            Place::Sources | Place::Settings => true,
        }
    }

    fn go(&mut self, place: Place) {
        // Back in the library from the player: bring the clip just watched into view.
        if let Some(v) = &self.viewer
            && !matches!(place, Place::View(_))
        {
            self.reveal_clip = Some(v.clip().to_path_buf());
        }
        match place {
            Place::Clips(f) => {
                if self.library_filter != f {
                    self.library_filter = f;
                    self.selection.clear();
                }
                self.page = Page::Clips;
            }
            Place::View(p) => self.open_viewer(p),
            Place::Sources => self.page = Page::Sources,
            Place::Settings => self.page = Page::Settings,
        }
    }
}
