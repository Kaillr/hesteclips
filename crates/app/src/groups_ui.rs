//! Groups of a scene's sources (see `settings::GroupCfg`): a group's row in
//! the layers, over its sources' (folded away or not), its menu and its
//! keys. On the preview (`canvas_ui`) a group is selected by a click on one
//! of its sources, and moved and resized as one.

use egui::{Align2, FontId, Sense, pos2, vec2};
use egui_phosphor::regular as icon;

use crate::App;
use crate::canvas_ui::{Layer, Selected};
use crate::theme::{self, W};
use crate::ui_kit::{self, Item};

impl App {
    /// Whether any of group `id`'s sources is in the video (its eye).
    pub(crate) fn group_enabled(&self, id: u64) -> bool {
        self.settings.group(id).is_some_and(|g| g.members.iter().any(|m| self.layer_enabled(*m)))
    }

    /// All of group `id`'s sources shown, or all hidden.
    pub(crate) fn set_group_enabled(&mut self, id: u64, on: bool) {
        for m in self.settings.group(id).map(|g| g.members.clone()).unwrap_or_default() {
            self.set_layer_enabled(m, on);
        }
    }

    /// Whether all of group `id`'s sources are locked.
    pub(crate) fn group_locked(&self, id: u64) -> bool {
        self.settings.group(id).is_some_and(|g| g.members.iter().all(|m| self.settings.is_locked(*m)))
    }

    /// All of group `id`'s sources locked, or all unlocked.
    pub(crate) fn set_group_locked(&mut self, id: u64, on: bool) {
        for m in self.settings.group(id).map(|g| g.members.clone()).unwrap_or_default() {
            self.settings.set_locked(m, on);
        }
    }

    /// Group `id`'s sources taken out of the scene shown (the screen stays,
    /// out of the group), and so the group. Ctrl+Z puts them back.
    pub(crate) fn remove_group(&mut self, id: u64) {
        let Some(g) = self.settings.group(id).cloned() else { return };
        for m in &g.members {
            if *m == Layer::Screen {
                self.settings.set_group(*m, None);
            } else {
                self.settings.remove_from_scene(*m);
            }
        }
        self.settings.ungroup(id);
        self.forget_gone_selection();
        let ctrl = crate::shortcuts::ctrl_name();
        self.toast_icon(icon::TRASH, format!("Took {} out of this scene · {ctrl}+Z puts it back", g.name));
    }

    /// A row dragged in the layers onto `target`'s place: a source to it
    /// (it stays in its group, if it's in one), a group as one, past what
    /// it was dragged over.
    pub(crate) fn move_row(&mut self, held: &Selected, target: &Selected) {
        let order = self.layer_order();
        let first_of = |s: &Selected| -> Option<Layer> {
            match s {
                Selected::Layer(l) => Some(*l),
                Selected::Group(id) => self.settings.group(*id).and_then(|g| g.members.first().copied()),
            }
        };
        match held {
            Selected::Layer(l) => {
                if let Some(to) = first_of(target).and_then(|t| order.iter().position(|x| *x == t)) {
                    self.move_layer(*l, to);
                }
            }
            Selected::Group(id) => {
                let Some(members) = self.settings.group(*id).map(|g| g.members.clone()) else { return };
                let over: Vec<Layer> = match target {
                    Selected::Layer(t) => vec![*t],
                    Selected::Group(t) => self.settings.group(*t).map(|g| g.members.clone()).unwrap_or_default(),
                };
                if over.iter().any(|t| members.contains(t)) {
                    return;
                }
                let rest: Vec<Layer> = order.iter().filter(|l| !members.contains(l)).copied().collect();
                let now = order.iter().take_while(|l| !members.contains(l)).count();
                let (Some(first), Some(last)) = (over.first().and_then(|t| rest.iter().position(|x| x == t)), over.last().and_then(|t| rest.iter().position(|x| x == t))) else { return };
                self.settings.move_group(*id, if first >= now { last + 1 } else { first });
            }
        }
    }

    /// Group `id`'s row in the layers, over its sources': its grip (drag it
    /// up or down, its sources with it), a fold, its name (double-click to
    /// rename), how many it has, its lock and eye for all of them. A click
    /// selects it; a right-click opens its menu. Where it was drawn.
    pub(crate) fn group_row(&mut self, ui: &mut egui::Ui, id: u64) -> egui::Rect {
        let pal = theme::pal(ui);
        let Some(g) = self.settings.group(id).cloned() else { return egui::Rect::NOTHING };
        let selected = self.selected_group() == Some(id);
        let held = self.sources_view.row_drag.as_ref().is_some_and(|(s, _)| *s == Selected::Group(id));
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::click());
        let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered() || held, theme::FAST);
        ui.painter().rect_filled(rect, 6, if selected { pal.sel } else { theme::fade(pal.control, hv) });
        let painter = ui.painter().clone();
        let cy = rect.center().y;
        let over_row = ui.rect_contains_pointer(rect);
        // Its grip.
        let grip_rect = egui::Rect::from_min_max(rect.min, pos2(rect.left() + 20.0, rect.bottom()));
        let grip = ui.interact(grip_rect, resp.id.with("grip"), Sense::drag());
        if over_row || held {
            painter.text(pos2(rect.left() + 10.0, cy), Align2::CENTER_CENTER, icon::DOTS_SIX_VERTICAL, FontId::proportional(14.0), if grip.hovered() || held { pal.text } else { pal.text3 });
        }
        if grip.on_hover_text("Drag up or down, its sources with it").on_hover_cursor(egui::CursorIcon::Grab).drag_started() {
            self.sources_view.row_drag = Some((Selected::Group(id), self.arrangement()));
        }
        // Its fold.
        let fold_rect = egui::Rect::from_center_size(pos2(rect.left() + 32.0, cy), vec2(20.0, 24.0));
        let fold = ui.interact(fold_rect, resp.id.with("fold"), Sense::click());
        // Pointing right while folded, down while open.
        let (c, s) = (fold_rect.center(), 3.5);
        let pts = if g.collapsed { vec![c + vec2(-s * 0.5, -s), c + vec2(s * 0.5, 0.0), c + vec2(-s * 0.5, s)] } else { vec![c + vec2(-s, -s * 0.5), c + vec2(0.0, s * 0.5), c + vec2(s, -s * 0.5)] };
        painter.add(egui::Shape::line(pts, egui::Stroke::new(1.6, if fold.hovered() { pal.text } else { pal.text2 })));
        if fold.on_hover_text(if g.collapsed { "Show its sources" } else { "Fold its sources away" }).clicked()
            && let Some(g) = self.settings.group_mut(id)
        {
            g.collapsed = !g.collapsed;
        }
        // Its eye, and its lock: for all of its sources.
        let on = self.group_enabled(id);
        let eye_rect = egui::Rect::from_center_size(pos2(rect.right() - 18.0, cy), vec2(26.0, 26.0));
        let eye = ui.interact(eye_rect, resp.id.with("eye"), Sense::click());
        if eye.hovered() {
            painter.rect_filled(eye_rect, 5, pal.control_h);
        }
        painter.text(eye_rect.center(), Align2::CENTER_CENTER, if on { icon::EYE } else { icon::EYE_SLASH }, FontId::proportional(15.0), if on { pal.text2 } else { pal.text3 });
        if eye.on_hover_text(if on { "Hide its sources  (H)" } else { "Show its sources  (H)" }).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            let was = self.arrangement();
            self.set_group_enabled(id, !on);
            self.keep_for_undo(was);
        }
        let locked = self.group_locked(id);
        let lock_rect = egui::Rect::from_center_size(pos2(eye_rect.left() - 16.0, cy), vec2(26.0, 26.0));
        if locked || over_row {
            let lock = ui.interact(lock_rect, resp.id.with("lock"), Sense::click());
            if lock.hovered() {
                painter.rect_filled(lock_rect, 5, pal.control_h);
            }
            painter.text(lock_rect.center(), Align2::CENTER_CENTER, if locked { icon::LOCK_SIMPLE } else { icon::LOCK_SIMPLE_OPEN }, FontId::proportional(14.0), if locked { pal.text2 } else { pal.text3 });
            let ctrl = crate::shortcuts::ctrl_name();
            let tip = if locked { format!("Unlock its sources  ({ctrl}+Shift+L)") } else { format!("Lock its sources: they can't be moved on the preview  ({ctrl}+Shift+L)") };
            if lock.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                self.set_group_locked(id, !locked);
            }
        }
        // Its symbol, its name (or the box renaming it), how many it has.
        let x = rect.left() + 46.0;
        painter.text(pos2(x + 8.0, cy), Align2::CENTER_CENTER, if g.collapsed { icon::FOLDER_SIMPLE } else { icon::FOLDER_OPEN }, FontId::proportional(16.0), pal.text2);
        let right = lock_rect.left() - 6.0;
        let rename_id = egui::Id::new(("rename_group", id));
        let draft_id = rename_id.with("draft");
        if let Some(mut draft) = ui.data(|d| d.get_temp::<String>(draft_id)) {
            let field = egui::Rect::from_min_max(pos2(x + 24.0, rect.top() + 3.0), pos2(right, rect.bottom() - 3.0));
            let mut fui = ui.new_child(egui::UiBuilder::new().max_rect(field).layout(egui::Layout::left_to_right(egui::Align::Center)));
            if crate::sources_ui::name_field(&mut fui, rename_id, &mut draft, theme::font(13.5, W::Medium), field.width()) {
                if let Some(g) = self.settings.group_mut(id).filter(|_| !draft.trim().is_empty()) {
                    g.name = draft.trim().to_owned();
                }
                ui.data_mut(|d| d.remove::<String>(draft_id));
            } else {
                ui.data_mut(|d| d.insert_temp(draft_id, draft));
            }
        } else {
            crate::sources_ui::forget_name_field(ui, rename_id);
            let name = painter.layout_job(ui_kit::single_line_job(&g.name, theme::font(13.5, W::Medium), pal.text, (right - x - 24.0).max(10.0) * 0.7));
            let name_w = name.size().x;
            painter.galley(pos2(x + 24.0, cy - name.size().y / 2.0), name, pal.text);
            let n = g.members.len();
            let count = format!("{n} source{}", if n == 1 { "" } else { "s" });
            let line = painter.layout_job(ui_kit::single_line_job(&count, theme::font(12.5, W::Regular), pal.text3, (right - x - 32.0 - name_w).max(10.0)));
            painter.galley(pos2(x + 32.0 + name_w, cy - line.size().y / 2.0), line, pal.text3);
        }
        if resp.double_clicked() {
            ui.data_mut(|d| d.insert_temp(draft_id, g.name.clone()));
        } else if resp.clicked() || resp.secondary_clicked() {
            self.canvas.selected = Some(Selected::Group(id));
            self.canvas.picked.clear();
        }
        let was = self.arrangement();
        ui_kit::context_menu(&resp, 230.0, |ui| self.group_menu(ui, id));
        if self.arrangement() != was {
            self.keep_for_undo(was);
        }
        rect
    }

    /// A group's menu (right-click its row, or one of its sources on the
    /// preview while it's selected): rename, ungroup, copy to another scene,
    /// lock, hide, and take its sources out of the scene.
    pub(crate) fn group_menu(&mut self, ui: &mut egui::Ui, id: u64) {
        let Some(g) = self.settings.group(id).cloned() else { return };
        let ctrl = crate::shortcuts::ctrl_name();
        let keys = |k: &[&str]| Some(k.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>());
        if Item::new("Rename").icon(icon::PENCIL_SIMPLE).show(ui).clicked() {
            ui.data_mut(|d| d.insert_temp(egui::Id::new(("rename_group", id)).with("draft"), g.name.clone()));
            ui.close();
        }
        if Item::new("Ungroup").icon(icon::FOLDER_SIMPLE_MINUS).keys(keys(&[ctrl, "Shift", "G"])).tip("Its sources on their own again, where they are").show(ui).clicked() {
            self.settings.ungroup(id);
            self.canvas.selected = None;
            ui.close();
        }
        let others: Vec<(u64, String)> = self.settings.scenes.iter().filter(|s| s.id != self.settings.active_scene).map(|s| (s.id, s.name.clone())).collect();
        if !others.is_empty() {
            let mut to: Vec<(u64, String)> = Vec::new();
            ui_kit::submenu(ui, Item::new("Copy to scene").icon(icon::COPY_SIMPLE), |ui| {
                for (sid, name) in &others {
                    if Item::new(name.clone()).tip("Its sources, placed as here, in this group there").show(ui).clicked() {
                        to = vec![(*sid, name.clone())];
                    }
                }
                if others.len() > 1 {
                    ui_kit::menu_sep(ui);
                    if Item::new("Every scene").show(ui).clicked() {
                        to.clone_from(&others);
                    }
                }
            });
            if !to.is_empty() {
                for (sid, _) in &to {
                    self.settings.copy_group_to_scene(id, *sid);
                }
                let where_ = if to.len() == 1 { to[0].1.clone() } else { "every scene".to_owned() };
                self.toast_icon(icon::COPY_SIMPLE, format!("Copied {} to {where_}", g.name));
                ui.close();
            }
        }
        ui_kit::menu_sep(ui);
        let locked = self.group_locked(id);
        if Item::new(if locked { "Unlock" } else { "Lock" }).icon(if locked { icon::LOCK_SIMPLE_OPEN } else { icon::LOCK_SIMPLE }).keys(keys(&[ctrl, "Shift", "L"])).show(ui).clicked() {
            self.set_group_locked(id, !locked);
            ui.close();
        }
        let on = self.group_enabled(id);
        if Item::new(if on { "Hide" } else { "Show" }).icon(if on { icon::EYE_SLASH } else { icon::EYE }).keys(keys(&["H"])).show(ui).clicked() {
            self.set_group_enabled(id, !on);
            ui.close();
        }
        if Item::new("Remove").icon(icon::TRASH).danger().keys(keys(&["Del"])).tip("Take its sources out of this scene").show(ui).clicked() {
            self.remove_group(id);
            ui.close();
        }
    }

    /// The keys for the group selected (see `source_keys`): Delete takes
    /// its sources out, H hides or shows them, Ctrl+Shift+L locks them,
    /// Ctrl+Shift+G ungroups, the arrows nudge them (Shift: ten pixels),
    /// Esc lets go of it.
    pub(crate) fn group_keys(&mut self, ctx: &egui::Context, id: u64, aspect: f32) {
        use egui::{Key, Modifiers};
        let was = self.arrangement();
        let (esc, remove, hide, lock, ungroup) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::Escape),
                i.consume_key(Modifiers::NONE, Key::Delete) || i.consume_key(Modifiers::NONE, Key::Backspace),
                i.consume_key(Modifiers::NONE, Key::H),
                i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::L),
                i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::G),
            )
        });
        if esc {
            self.canvas.selected = None;
            return;
        }
        if remove {
            self.remove_group(id);
        }
        if hide {
            let on = self.group_enabled(id);
            self.set_group_enabled(id, !on);
        }
        if lock {
            let locked = self.group_locked(id);
            self.set_group_locked(id, !locked);
        }
        if ungroup {
            self.settings.ungroup(id);
            self.canvas.selected = None;
        }
        let nudge = ctx.input(|i| {
            let step = if i.modifiers.shift { 10.0 } else { 1.0 };
            let mut d = vec2(0.0, 0.0);
            if !i.modifiers.command && !i.modifiers.alt {
                if i.key_pressed(Key::ArrowLeft) {
                    d.x -= step;
                }
                if i.key_pressed(Key::ArrowRight) {
                    d.x += step;
                }
                if i.key_pressed(Key::ArrowUp) {
                    d.y -= step;
                }
                if i.key_pressed(Key::ArrowDown) {
                    d.y += step;
                }
            }
            d
        });
        if nudge != vec2(0.0, 0.0) {
            for m in self.settings.group(id).map(|g| g.members.clone()).unwrap_or_default() {
                if !self.settings.is_locked(m) {
                    self.nudge_layer(m, nudge, aspect);
                }
            }
        }
        if self.arrangement() != was {
            if nudge != vec2(0.0, 0.0) && !remove && !hide {
                self.keep_for_undo_as(was, format!("nudge group {id}"));
            } else {
                self.keep_for_undo(was);
            }
        }
    }
}
