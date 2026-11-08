//! The Sources page's frame, as Clips and Settings have theirs: the panel
//! with the preset in use and its scenes; the page beside it shows the
//! scene picked (`sources_ui`: its layers and preview, and the preset's
//! sound under them). Presets and scenes are made, renamed, put in order
//! and deleted here. A scene picked is shown, live while capturing; a
//! preset picked is used (see `App::apply_switch`).

use egui::{Align2, Pos2, Rect, Sense, vec2};
use egui_phosphor::regular as icon;

use crate::App;
use crate::theme::{self, W};
use crate::ui_kit::{self, Btn, Dismissed, Item, Kind, Seg};

/// One of the Sources page's dialogs, open.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Dialog {
    /// A new preset: its name, and whether it starts as a copy of the one
    /// in use (else the screen with a mic and your computer's sound).
    NewPreset { name: String, copy: bool },
    RenamePreset { id: u64, name: String },
    DeletePreset(u64),
    DeleteScene(u64),
    /// Another preset while clipping: clipping starts again with it.
    SwitchPreset(u64),
}

impl App {
    /// A new scene, after the one shown (a copy of it, or just the screen),
    /// shown at once.
    pub(crate) fn new_scene(&mut self, copy: bool) {
        let id = self.settings.add_scene(copy);
        self.show_scene(id);
    }

    /// Use preset `id` when it can be: now while nothing runs; while only
    /// clipping, once a dialog says clipping starts again; never while
    /// recording or streaming (a capture keeps what it started with).
    pub(crate) fn pick_preset(&mut self, id: u64) {
        if id == self.settings.active_preset {
            return;
        }
        let s = self.rec_state;
        if s.idle() {
            self.use_preset(id, false);
        } else if self.presets_locked().is_none() {
            self.sources_view.dialog = Some(Dialog::SwitchPreset(id));
        }
    }

    /// Why another preset can't be used now: recording or streaming (only
    /// clipping, it starts again with it).
    pub(crate) fn presets_locked(&self) -> Option<String> {
        let s = self.rec_state;
        (s.recording || s.streaming).then(|| format!("Stop {} to switch presets", crate::RecState { clipping: false, ..s }.words()))
    }

    /// The panel, where Clips and Settings have theirs: the title, the
    /// preset in use, then the card of its scenes.
    pub(crate) fn sources_panel(&mut self, ui: &mut egui::Ui) {
        let p = theme::pal(ui);
        let rect = ui.max_rect();
        ui.painter().rect_filled(rect, 0, p.base);
        // As Settings' panel: its rows line up with the page title, the same
        // gap below the card as beside it.
        let side = ui_kit::PAGE_MARGIN as f32 - 10.0;
        let inner = Rect::from_min_max(rect.min + vec2(side, 0.0), rect.max - vec2(0.0, side));
        let head = Rect::from_min_size(inner.min, vec2(inner.width(), crate::shell::HEAD_H));
        crate::shell::drag_window_area(ui, Rect::from_min_size(head.min, vec2(70.0, head.height())), "sources_panel_head");
        let title_x = rect.left() + ui_kit::PAGE_MARGIN as f32;
        let mut hui = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_min_max(Pos2::new(title_x, head.top()), head.max)).layout(egui::Layout::left_to_right(egui::Align::Center)));
        hui.label(theme::text("Sources", 20.0, W::Semibold).color(p.text));
        let picker = Rect::from_min_size(Pos2::new(inner.left(), head.bottom()), vec2(inner.width(), ui_kit::CONTROL_H));
        let mut pui = ui.new_child(egui::UiBuilder::new().max_rect(picker).layout(egui::Layout::left_to_right(egui::Align::Center)));
        self.preset_picker(&mut pui, picker.width());
        let card = Rect::from_min_max(Pos2::new(inner.left(), picker.bottom() + 10.0), inner.max);
        ui.painter().rect_filled(card, ui_kit::CARD_RADIUS, p.raise);
        let mut cui = ui.new_child(egui::UiBuilder::new().max_rect(card.shrink(6.0)).layout(egui::Layout::top_down(egui::Align::Min)));
        ui_kit::scroll(&mut cui, "sources_panel_card", |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            self.panel_rows(ui);
        });
    }

    /// The card's rows: the scenes (+ adds one).
    fn panel_rows(&mut self, ui: &mut egui::Ui) {
        let p = theme::pal(ui);
        ui.horizontal(|ui| {
            ui.set_min_height(28.0);
            ui.add_space(10.0);
            ui.label(theme::text("Scenes", 12.0, W::Medium).color(p.text3));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let add = Btn::icon(Kind::Ghost, icon::PLUS).small().tip("Add a scene").show(ui);
                ui_kit::menu_popup(&add, false, 230.0, |ui| self.add_scene_items(ui));
            });
        });
        // Sliding to their places as one's added, deleted or moved.
        let ids: Vec<(u64, f32)> = self.settings.scenes.iter().map(|s| (s.id, 34.0)).collect();
        ui_kit::sliding_rows(ui, egui::Id::new("scene_rows"), &ids, 2.0, None, |ui, i| self.scene_row(ui, ids[i].0));
    }

    /// New scene, and a copy of the one shown: under + and in menus.
    fn add_scene_items(&mut self, ui: &mut egui::Ui) {
        let shown = self.settings.scene().map(|s| s.name.clone()).unwrap_or_default();
        if Item::new("New scene").icon(icon::PLUS).tip("The screen, filling the video").show(ui).clicked() {
            self.new_scene(false);
        }
        if Item::new(format!("Duplicate “{shown}”")).icon(icon::COPY).show(ui).clicked() {
            self.new_scene(true);
        }
    }

    /// A scene's row: its name (double-click to rename), a dot in the
    /// recorder's colour while it's what's captured, its menu on hover and
    /// on a right-click. A click shows it.
    fn scene_row(&mut self, ui: &mut egui::Ui, id: u64) {
        let p = theme::pal(ui);
        let Some(name) = self.settings.scenes.iter().find(|s| s.id == id).map(|s| s.name.clone()) else { return };
        let shown = id == self.settings.active_scene;
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::hover());
        let rename_id = egui::Id::new(("rename_scene", id));
        let draft_id = rename_id.with("draft");
        if let Some(mut draft) = ui.data(|d| d.get_temp::<String>(draft_id)) {
            let mut fui = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(vec2(8.0, 3.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
            if crate::sources_ui::name_field(&mut fui, rename_id, &mut draft, theme::font(13.5, W::Medium), rect.width() - 16.0) {
                self.settings.rename_scene(id, &draft);
                ui.data_mut(|d| d.remove::<String>(draft_id));
            } else {
                ui.data_mut(|d| d.insert_temp(draft_id, draft));
            }
            return;
        }
        crate::sources_ui::forget_name_field(ui, rename_id);
        let resp = ui.interact(rect, egui::Id::new(("scene_row", id)), Sense::click());
        // Hovered as a whole, its menu button too (that takes the hover).
        let over = ui.rect_contains_pointer(rect);
        let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), over, theme::FAST);
        let on = theme::toggle(ui.ctx(), resp.id.with("on"), shown, theme::NORMAL);
        if hv > 0.0 && !shown {
            ui.painter().rect_filled(rect, ui_kit::RADIUS, theme::fade(p.sb_hover, hv));
        }
        if on > 0.0 {
            ui.painter().rect_filled(rect, ui_kit::RADIUS, theme::fade(p.sb_sel, on));
        }
        if shown && !self.rec_state.idle() {
            let color = if self.rec_state.streaming {
                p.stream
            } else if self.rec_state.recording {
                p.rec
            } else {
                p.live
            };
            ui.painter().circle_filled(Pos2::new(rect.left() + 13.0, rect.center().y), 3.5, color);
        }
        let text = theme::mix(theme::mix(p.text2, p.text, hv), p.text, on);
        let job = ui_kit::single_line_job(&name, theme::font(13.5, if shown { W::Medium } else { W::Regular }), text, rect.width() - 26.0 - 34.0);
        let g = ui.painter().layout_job(job);
        ui.painter().galley(Pos2::new(rect.left() + 26.0, rect.center().y - g.size().y / 2.0), g, text);
        // Its menu's button: on hover (and the one shown), and while its menu
        // is open (the pointer's in the menu then, off the row).
        let open_key = egui::Id::new(("scene_menu_open", id));
        let menu_open = ui.data(|d| d.get_temp::<bool>(open_key)).unwrap_or(false);
        if over || shown || menu_open {
            let more = Rect::from_center_size(Pos2::new(rect.right() - 16.0, rect.center().y), vec2(24.0, 24.0));
            let mut mui = ui.new_child(egui::UiBuilder::new().id_salt(("scene_more", id)).max_rect(more).layout(egui::Layout::left_to_right(egui::Align::Center)));
            let b = Btn::icon(Kind::Ghost, icon::DOTS_THREE).small().tip("Rename, duplicate, move, delete").show(&mut mui);
            ui_kit::menu_popup(&b, false, 220.0, |ui| self.scene_menu(ui, id, rename_id));
            let open = egui::Popup::is_id_open(ui.ctx(), b.id.with("menu"));
            ui.data_mut(|d| d.insert_temp(open_key, open));
        }
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        ui_kit::context_menu(&resp, 220.0, |ui| self.scene_menu(ui, id, rename_id));
        if resp.double_clicked() {
            ui.data_mut(|d| d.insert_temp(draft_id, name));
        } else if resp.clicked() {
            self.show_scene(id);
        }
    }

    /// A scene's menu: rename (in the name field `rename_at`, where the menu
    /// was opened), duplicate, move, preview on a monitor (it shown), delete.
    pub(crate) fn scene_menu(&mut self, ui: &mut egui::Ui, id: u64, rename_at: egui::Id) {
        let Some(at) = self.settings.scenes.iter().position(|s| s.id == id) else { return };
        let count = self.settings.scenes.len();
        let name = self.settings.scenes[at].name.clone();
        if Item::new("Rename").icon(icon::PENCIL_SIMPLE).show(ui).clicked() {
            ui.data_mut(|d| d.insert_temp(rename_at.with("draft"), name));
        }
        if Item::new("Duplicate").icon(icon::COPY).show(ui).clicked()
            && let Some(new) = self.settings.duplicate_scene(id)
        {
            self.show_scene(new);
        }
        ui_kit::menu_sep(ui);
        if Item::new("Move up").icon(icon::ARROW_UP).enabled(at > 0).show(ui).clicked() {
            self.settings.move_scene(id, true);
        }
        if Item::new("Move down").icon(icon::ARROW_DOWN).enabled(at + 1 < count).show(ui).clicked() {
            self.settings.move_scene(id, false);
        }
        ui_kit::menu_sep(ui);
        // Fullscreen on a display, letterboxed: what the video shows, this
        // scene shown.
        let mut on = None;
        ui_kit::submenu(ui, Item::new("Preview on monitor").icon(icon::PROJECTOR_SCREEN), |ui| on = self.projector_items(ui));
        if let Some(d) = on {
            self.show_scene(id);
            self.open_projector(&d);
            ui.close();
        }
        ui_kit::menu_sep(ui);
        let delete = Item::new("Delete…").icon(icon::TRASH).danger().enabled(count > 1);
        let delete = if count > 1 { delete } else { delete.tip("A preset has one scene at least") };
        if delete.show(ui).clicked() {
            self.sources_view.dialog = Some(Dialog::DeleteScene(id));
        }
    }

    /// The preset in use, as wide as `width`: its menu switches to another,
    /// and makes, duplicates, renames and deletes presets.
    pub(crate) fn preset_picker(&mut self, ui: &mut egui::Ui, width: f32) {
        let p = theme::pal(ui);
        let name = self.settings.preset().map_or_else(|| "Preset".to_owned(), |x| x.name.clone());
        let (rect, resp) = ui.allocate_exact_size(vec2(width, ui_kit::CONTROL_H), Sense::click());
        let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered(), theme::FAST);
        ui.painter().rect_filled(rect, ui_kit::RADIUS, theme::mix(p.control, p.control_h, hv));
        let lead = Pos2::new(rect.left() + 16.0, rect.center().y);
        ui.painter().text(lead, Align2::CENTER_CENTER, egui_phosphor::fill::STACK, egui::FontId::new(15.0, theme::fill_family()), p.text2);
        let job = ui_kit::single_line_job(&name, theme::font(13.5, W::Medium), p.text, rect.width() - 30.0 - 26.0);
        let g = ui.painter().layout_job(job);
        ui.painter().galley(Pos2::new(rect.left() + 30.0, rect.center().y - g.size().y / 2.0), g, p.text);
        ui_kit::chevron(ui.painter(), Pos2::new(rect.right() - 14.0, rect.center().y), 3.5, 0.0, p.text2);
        let resp = resp.on_hover_text("The preset in use: a setup of its own, with its scenes, sources and sound").on_hover_cursor(egui::CursorIcon::PointingHand);
        ui_kit::menu_popup(&resp, true, width.max(230.0), |ui| self.preset_menu(ui));
    }

    /// The presets (a click uses one), then new, duplicate, rename, delete.
    fn preset_menu(&mut self, ui: &mut egui::Ui) {
        let locked = self.presets_locked();
        let using = self.settings.active_preset;
        let presets: Vec<(u64, String)> = self.settings.presets.iter().map(|x| (x.id, x.name.clone())).collect();
        for (id, name) in presets {
            let item = Item::new(name).check(id == using);
            let item = match &locked {
                Some(why) if id != using => item.enabled(false).tip(why.clone()),
                _ => item,
            };
            if item.show(ui).clicked() {
                self.pick_preset(id);
            }
        }
        ui_kit::menu_sep(ui);
        let current = self.settings.preset().map(|x| x.name.clone()).unwrap_or_default();
        if Item::new("New preset…").icon(icon::PLUS).show(ui).clicked() {
            self.sources_view.dialog = Some(Dialog::NewPreset { name: String::new(), copy: true });
        }
        if Item::new(format!("Duplicate “{current}”")).icon(icon::COPY).show(ui).clicked() {
            self.settings.add_preset(&format!("{current} copy"), true);
        }
        if Item::new("Rename…").icon(icon::PENCIL_SIMPLE).show(ui).clicked() {
            self.sources_view.dialog = Some(Dialog::RenamePreset { id: using, name: current });
        }
        let idle = self.rec_state.idle();
        let delete = Item::new("Delete…").icon(icon::TRASH).danger().enabled(self.settings.presets.len() > 1 && idle);
        let delete = if !idle {
            delete.tip(format!("Stop {} to delete the preset in use", self.rec_state.words()))
        } else if self.settings.presets.len() < 2 {
            delete.tip("There's one preset at least")
        } else {
            delete
        };
        if delete.show(ui).clicked() {
            self.sources_view.dialog = Some(Dialog::DeletePreset(using));
        }
    }

    /// The heading's left side: the scene's name and the video's size (and
    /// while cropping on the preview, Done); with the panel folded away (a
    /// narrow window), the preset and a menu of the scenes instead.
    pub(crate) fn sources_head(&mut self, ui: &mut egui::Ui, wide: bool) {
        self.page_head_folding(ui, "", |app, ui, tight| {
            let p = theme::pal(ui);
            let title = app.settings.scene().map(|s| s.name.clone()).unwrap_or_default();
            if wide {
                ui.label(theme::text(&title, 15.0, W::Semibold).color(p.text));
                // The video's size, then while cropping on the preview what's
                // cropped and Done: the page's own, here rather than in a row
                // over the preview of their own.
                if !tight {
                    ui.add_space(6.0);
                    let size = app.video_size_text();
                    app.size_button(ui, &size);
                }
                if let Some(l) = app.canvas.crop_mode {
                    ui.add_space(10.0);
                    ui.label(theme::text(format!("Cropping {}", app.settings.layer_title(l)), 12.5, W::Regular).color(p.text2));
                    ui.add_space(4.0);
                    if Btn::new(Kind::Primary, "Done").with_icon(icon::CHECK).small().tip("Stop cropping (Enter)").show(ui).clicked() {
                        app.canvas.crop_mode = None;
                    }
                }
            } else {
                app.preset_picker(ui, 150.0_f32.min(ui.available_width() * 0.45));
                ui.add_space(4.0);
                app.scenes_menu(ui, &title);
            }
        });
    }

    /// The scenes in a menu on the heading, when the window's too narrow
    /// for the panel.
    fn scenes_menu(&mut self, ui: &mut egui::Ui, title: &str) {
        let r = Btn::ghost(title).trail(icon::CARET_DOWN).id("sources_scenes_menu").show(ui);
        ui_kit::menu_popup(&r, true, 230.0, |ui| {
            let scenes: Vec<(u64, String)> = self.settings.scenes.iter().map(|s| (s.id, s.name.clone())).collect();
            for (id, name) in scenes {
                if Item::new(name).check(id == self.settings.active_scene).show(ui).clicked() {
                    self.show_scene(id);
                }
            }
            ui_kit::menu_sep(ui);
            self.add_scene_items(ui);
        });
    }

    /// The Sources page's dialog, if one's open.
    pub(crate) fn sources_dialogs(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.sources_view.dialog.take() else { return };
        self.sources_view.dialog = match dialog {
            Dialog::NewPreset { name, copy } => self.new_preset_dialog(ctx, name, copy),
            Dialog::RenamePreset { id, name } => self.rename_preset_dialog(ctx, id, name),
            Dialog::DeletePreset(id) => self.delete_preset_dialog(ctx, id),
            Dialog::DeleteScene(id) => self.delete_scene_dialog(ctx, id),
            Dialog::SwitchPreset(id) => self.switch_preset_dialog(ctx, id),
        };
    }

    /// A new preset's name, and what it starts with. The dialog as it is
    /// now, while it's open.
    fn new_preset_dialog(&mut self, ctx: &egui::Context, mut name: String, mut copy: bool) -> Option<Dialog> {
        let current = self.settings.preset().map(|x| x.name.clone()).unwrap_or_default();
        let copy_label = format!("A copy of “{current}”");
        let modal = ui_kit::dialog(ctx, "new_preset", 400.0, |ui| {
            ui_kit::dialog_title(ui, "New preset");
            ui_kit::dialog_text(ui, "A setup of its own, with its scenes, sources and sound: one for streaming, one for plain clipping.");
            ui.add_space(4.0);
            let out = ui_kit::text_field(ui, &mut name, "Its name");
            focus_once(ui, "new_preset", &out.response);
            ui.add_space(10.0);
            let opts = [Seg { icon: None, label: copy_label.as_str(), tip: None }, Seg { icon: None, label: "Just the screen and sound", tip: Some("The screen, your mic and your computer's sound, in one scene") }];
            if let Some(i) = ui_kit::segmented_opts(ui, "new_preset_from", &opts, usize::from(!copy), None, true) {
                copy = i == 0;
            }
            let enter = out.response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let (mut create, mut cancel) = (enter, false);
            ui_kit::dialog_buttons(ui, |ui| {
                create |= ui.add(ui_kit::button("Create", true)).clicked();
                cancel = ui.add(ui_kit::button("Cancel", false).ghost()).clicked();
            });
            (create, cancel)
        });
        let (create, cancel) = modal.inner;
        if create {
            forget_focus(ctx, "new_preset");
            let name = if name.trim().is_empty() { "New preset".to_owned() } else { name };
            let id = self.settings.add_preset(&name, copy);
            match self.presets_locked() {
                Some(_) => {
                    let what = crate::RecState { clipping: false, ..self.rec_state }.words();
                    self.toast_icon(icon::STACK, format!("Made the preset. Switch to it once you stop {what}"));
                }
                None => self.pick_preset(id),
            }
            return self.sources_view.dialog.take();
        }
        if cancel || modal.dismissed() {
            forget_focus(ctx, "new_preset");
            return None;
        }
        Some(Dialog::NewPreset { name, copy })
    }

    fn rename_preset_dialog(&mut self, ctx: &egui::Context, id: u64, mut name: String) -> Option<Dialog> {
        let modal = ui_kit::dialog(ctx, "rename_preset", 380.0, |ui| {
            ui_kit::dialog_title(ui, "Rename the preset");
            let out = ui_kit::text_field(ui, &mut name, "Its name");
            focus_once(ui, "rename_preset", &out.response);
            let enter = out.response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let (mut save, mut cancel) = (enter, false);
            ui_kit::dialog_buttons(ui, |ui| {
                save |= ui.add(ui_kit::button("Save", true)).clicked();
                cancel = ui.add(ui_kit::button("Cancel", false).ghost()).clicked();
            });
            (save, cancel)
        });
        let (save, cancel) = modal.inner;
        if save {
            self.settings.rename_preset(id, &name);
        }
        if save || cancel || modal.dismissed() {
            forget_focus(ctx, "rename_preset");
            return None;
        }
        Some(Dialog::RenamePreset { id, name })
    }

    fn delete_preset_dialog(&mut self, ctx: &egui::Context, id: u64) -> Option<Dialog> {
        let name = self.settings.presets.iter().find(|x| x.id == id).map(|x| x.name.clone())?;
        let modal = ui_kit::dialog(ctx, "delete_preset", 380.0, |ui| {
            ui_kit::dialog_title(ui, &format!("Delete “{name}”?"));
            ui_kit::dialog_text(ui, "Its scenes, sources and sound go with it.");
            let (mut delete, mut cancel) = (false, false);
            ui_kit::dialog_buttons(ui, |ui| {
                delete = ui.add(ui_kit::danger_button("Delete")).clicked();
                cancel = ui.add(ui_kit::button("Cancel", false).ghost()).clicked();
            });
            (delete, cancel)
        });
        let (delete, cancel) = modal.inner;
        if delete && self.rec_state.idle() {
            let using = id == self.settings.active_preset;
            if self.settings.delete_preset(id) && using {
                self.after_preset_switch();
            }
        }
        (!(delete || cancel || modal.dismissed())).then_some(Dialog::DeletePreset(id))
    }

    fn delete_scene_dialog(&mut self, ctx: &egui::Context, id: u64) -> Option<Dialog> {
        let name = self.settings.scenes.iter().find(|s| s.id == id).map(|s| s.name.clone())?;
        let modal = ui_kit::dialog(ctx, "delete_scene", 380.0, |ui| {
            ui_kit::dialog_title(ui, &format!("Delete the scene “{name}”?"));
            ui_kit::dialog_text(ui, "Sources only it has go with it; the rest stay in your other scenes.");
            let (mut delete, mut cancel) = (false, false);
            ui_kit::dialog_buttons(ui, |ui| {
                delete = ui.add(ui_kit::danger_button("Delete")).clicked();
                cancel = ui.add(ui_kit::button("Cancel", false).ghost()).clicked();
            });
            (delete, cancel)
        });
        let (delete, cancel) = modal.inner;
        if delete {
            let shown = id == self.settings.active_scene;
            if self.settings.delete_scene(id) {
                // Its undo steps would bring back what it had.
                self.forget_canvas_steps();
                if shown {
                    self.canvas.selected = None;
                }
            }
        }
        (!(delete || cancel || modal.dismissed())).then_some(Dialog::DeleteScene(id))
    }

    fn switch_preset_dialog(&mut self, ctx: &egui::Context, id: u64) -> Option<Dialog> {
        let name = self.settings.presets.iter().find(|x| x.id == id).map(|x| x.name.clone())?;
        let modal = ui_kit::dialog(ctx, "switch_preset", 400.0, |ui| {
            ui_kit::dialog_title(ui, &format!("Switch to “{name}”?"));
            ui_kit::dialog_text(ui, "Clipping starts again with it, so what it's kept so far is lost.");
            let (mut switch, mut cancel) = (false, false);
            ui_kit::dialog_buttons(ui, |ui| {
                switch = ui.add(ui_kit::button("Switch", true)).clicked();
                cancel = ui.add(ui_kit::button("Cancel", false).ghost()).clicked();
            });
            (switch, cancel)
        });
        let (switch, cancel) = modal.inner;
        if switch && self.presets_locked().is_none() {
            self.use_preset(id, true);
        }
        (!(switch || cancel || modal.dismissed())).then_some(Dialog::SwitchPreset(id))
    }
}

/// Put the typing in a dialog's name field once, as it opens.
fn focus_once(ui: &mut egui::Ui, dialog: &str, field: &egui::Response) {
    let id = egui::Id::new((dialog, "focused"));
    if ui.data(|d| d.get_temp::<bool>(id)).is_none() {
        field.request_focus();
        ui.data_mut(|d| d.insert_temp(id, true));
    }
}

/// The dialog's closed: its field takes the typing again next time.
fn forget_focus(ctx: &egui::Context, dialog: &str) {
    ctx.data_mut(|d| d.remove::<bool>(egui::Id::new((dialog, "focused"))));
}
