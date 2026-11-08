//! Copy, cut and paste on Sources (see `settings::CopiedSources`): Ctrl+C
//! copies what's selected (a group, or the sources picked), Ctrl+X takes it
//! out of the scene too, Ctrl+V puts it into the scene shown (another
//! scene, or another preset's) placed as it was: the same source where the
//! preset has it, else one of its own. The same in the menus, with Paste as
//! a copy (Ctrl+Shift+V) for sources of their own.

use egui_phosphor::regular as icon;

use crate::App;
use crate::canvas_ui::{Layer, Selected};
use crate::ui_kit::Item;

impl App {
    /// What Copy and Cut take: the group selected, else the sources
    /// selected (the one selected and those picked with it).
    fn copy_set(&self) -> Option<(Vec<Layer>, Option<u64>)> {
        match self.canvas.selected {
            Some(Selected::Group(id)) => self.settings.group(id).map(|g| (g.members.clone(), Some(id))),
            Some(Selected::Layer(_)) => Some((self.chosen(), None)),
            None => None,
        }
    }

    /// What's selected, copied to paste (`cut`: taken out of the scene as
    /// well, Ctrl+Z putting it back). The clipboard gets their names, for
    /// pasting elsewhere, and so Ctrl+V comes here as a paste whatever was
    /// on it (egui only says it was pressed when there are words on it).
    pub(crate) fn copy_selected(&mut self, ctx: &egui::Context, cut: bool) {
        let Some((layers, group)) = self.copy_set() else { return };
        let Some(copied) = self.settings.copy_sources(&layers, group) else { return };
        let names: Vec<String> = copied.sources.iter().map(|s| self.settings.copied_title(s)).collect();
        let what = match (&copied.group, names.as_slice()) {
            (Some(g), _) => g.clone(),
            (None, [one]) => one.clone(),
            (None, many) => format!("{} sources", many.len()),
        };
        ctx.copy_text(names.join("\n"));
        self.canvas.clipboard = Some(copied);
        if !cut {
            self.toast_icon(icon::COPY, format!("Copied {what}"));
            return;
        }
        let was = self.arrangement();
        for l in &layers {
            self.settings.remove_from_scene(*l);
        }
        if let Some(id) = group {
            self.settings.ungroup(id);
        }
        self.forget_gone_selection();
        self.keep_for_undo(was);
        self.toast_action(icon::SCISSORS, format!("Cut {what}"), "Undo", crate::shell::ToastAction::UndoSources);
    }

    /// What was copied, into the scene shown, in front of what's selected,
    /// then selected (`own_copies`: sources of their own, where they can
    /// be). Why any didn't go in, said.
    pub(crate) fn paste_copied(&mut self, own_copies: bool, aspect: f32) {
        let Some(copied) = self.canvas.clipboard.clone() else { return };
        let before = match self.canvas.selected {
            Some(Selected::Layer(l)) => Some(l),
            Some(Selected::Group(id)) => self.settings.group(id).and_then(|g| g.members.first().copied()),
            None => None,
        };
        let was = self.arrangement();
        let pasted = self.settings.paste_sources(&copied, before, own_copies, capture::layers::MANY, aspect);
        if let Some(first) = pasted.layers.first() {
            self.canvas.selected = Some(pasted.group.map_or(Selected::Layer(*first), Selected::Group));
            self.canvas.picked = if pasted.group.is_none() && pasted.layers.len() > 1 { pasted.layers.clone() } else { Vec::new() };
            self.keep_for_undo(was);
        }
        if !pasted.there.is_empty() {
            let verb = if pasted.there.len() == 1 { "is" } else { "are" };
            self.toast_icon(icon::INFO, format!("{} {verb} in this scene already", crate::inspector_ui::and_list(&pasted.there)));
        } else if !pasted.full.is_empty() {
            self.toast_icon(icon::INFO, "This scene has a webcam: a scene shows one here");
        }
    }

    /// Copy, cut and paste from the keyboard, while nothing's being typed:
    /// egui gives them as their own events (see `copy_selected`).
    pub(crate) fn clipboard_keys(&mut self, ctx: &egui::Context, aspect: f32) {
        let has = self.canvas.clipboard.is_some();
        let (copy, cut, paste, shift) = ctx.input_mut(|i| {
            let (mut copy, mut cut, mut paste) = (false, false, false);
            i.events.retain(|e| match e {
                egui::Event::Copy => {
                    copy = true;
                    false
                }
                egui::Event::Cut => {
                    cut = true;
                    false
                }
                egui::Event::Paste(_) if has => {
                    paste = true;
                    false
                }
                _ => true,
            });
            (copy, cut, paste, i.modifiers.shift)
        });
        if copy || cut {
            self.copy_selected(ctx, cut);
        }
        if paste {
            self.paste_copied(shift, aspect);
        }
    }

    /// Cut, Copy and Paste for a menu (Cut and Copy for what's selected,
    /// when `selection`), and Paste as a copy when what's copied has a
    /// source that can be one.
    pub(crate) fn clipboard_items(&mut self, ui: &mut egui::Ui, selection: bool, aspect: f32) {
        let ctrl = crate::shortcuts::ctrl_name();
        let keys = |k: &[&str]| Some(k.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>());
        if selection {
            if Item::new("Cut").icon(icon::SCISSORS).keys(keys(&[ctrl, "X"])).tip("Out of this scene, to paste into another").show(ui).clicked() {
                self.copy_selected(ui.ctx(), true);
                ui.close();
            }
            if Item::new("Copy").icon(icon::COPY).keys(keys(&[ctrl, "C"])).tip("To paste into another scene, placed as here").show(ui).clicked() {
                self.copy_selected(ui.ctx(), false);
                ui.close();
            }
        }
        // Paste, with nothing copied: there only in the preview's own menu,
        // greyed (a source's and a group's are long enough).
        let copied = self.canvas.clipboard.clone();
        let tip = "The same source, placed as it was where it was copied (a copy of it, in a scene that has it)";
        if (copied.is_some() || !selection) && Item::new("Paste").icon(icon::CLIPBOARD_TEXT).enabled(copied.is_some()).keys(keys(&[ctrl, "V"])).tip(tip).show(ui).clicked() {
            self.paste_copied(false, aspect);
            ui.close();
        }
        let copies = copied.is_some_and(|c| c.sources.iter().any(|s| matches!(s, crate::settings::CopiedSource::Visual(_))));
        if copies && Item::new("Paste as a copy").icon(icon::CLIPBOARD).keys(keys(&[ctrl, "Shift", "V"])).tip("A source of its own: what it shows changes here alone").show(ui).clicked() {
            self.paste_copied(true, aspect);
            ui.close();
        }
    }
}
