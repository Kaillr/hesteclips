//! HesteFiles UI: the account/default-folder settings section, the folder picker
//! and the share dialog.

use crate::ui_kit::Dismissed as _;
use std::path::PathBuf;

use crate::{App, file_name};

pub(crate) enum Dialog {
    /// Folders-only browser for choosing the default clips folder.
    PickDefaultFolder,
    Share(ShareDialog),
    /// An upload's public link, to copy or open.
    PublicLink { clip: PathBuf, link: String, copied: bool },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShareTarget {
    DefaultFolder,
    Custom,
}

pub(crate) struct ShareDialog {
    clip: PathBuf,
    target: ShareTarget,
    /// "Change folder" was clicked: show the browser inline.
    picking: bool,
}

impl App {
    pub(crate) fn open_share_dialog(&mut self, clip: PathBuf) {
        let target = if self.cloud.default_folder.is_some() {
            ShareTarget::DefaultFolder
        } else {
            ShareTarget::Custom
        };
        if target == ShareTarget::Custom {
            self.cloud.open_browser(None);
        }
        let picking = target == ShareTarget::Custom;
        self.dialog = Some(Dialog::Share(ShareDialog { clip, target, picking }));
    }

    pub(crate) fn dialogs(&mut self, ctx: &egui::Context) {
        if self.dialog.is_none() {
            self.dialog = self.public_links.pop_front().map(|(clip, link)| Dialog::PublicLink { clip, link, copied: false });
        }
        let Some(dialog) = self.dialog.take() else { return };
        self.dialog = match dialog {
            Dialog::PickDefaultFolder => self.pick_default_folder_dialog(ctx),
            Dialog::Share(share) => self.share_dialog(ctx, share).map(Dialog::Share),
            Dialog::PublicLink { clip, link, copied } => public_link_dialog(ctx, clip, link, copied),
        };
    }

    /// Returns `Some` while the dialog should stay open.
    fn pick_default_folder_dialog(&mut self, ctx: &egui::Context) -> Option<Dialog> {
        let mut keep_open = true;
        let resp = crate::ui_kit::dialog(ctx, "pick_default_folder", 460.0, |ui| {
            crate::ui_kit::dialog_title(ui, "Default folder");
            crate::ui_kit::dialog_text(ui, "Upload to HesteFiles puts clips here unless you pick another folder.");
            ui.add_space(6.0);
            self.cloud.browser_ui(ui);
            crate::ui_kit::dialog_buttons(ui, |ui| {
                let selection = self.cloud.browser_selection();
                if ui.add(crate::ui_kit::ButtonWidget(crate::ui_kit::Btn::primary("Use this folder").enabled(selection.is_some()))).clicked() {
                    let shown = selection.as_ref().map(|f| f.display().to_string()).unwrap_or_default();
                    self.cloud.set_default_folder(selection);
                    keep_open = false;
                    self.toast_icon(egui_phosphor::regular::CLOUD, format!("Clips upload to HesteFiles / {shown}"));
                }
                if ui.add(crate::ui_kit::button("Cancel", false).ghost()).clicked() {
                    keep_open = false;
                }
            });
        });
        (keep_open && !resp.dismissed()).then_some(Dialog::PickDefaultFolder)
    }

    /// Upload a clip to HesteFiles.
    ///
    /// The common case is one decision: the destination is already your clips
    /// folder, so the dialog reads "Upload <clip> to <folder>" with one button.
    /// Picking somewhere else is a "Change" that opens the folder browser in
    /// place. Upload closes the dialog; the clip's card shows the progress.
    fn share_dialog(&mut self, ctx: &egui::Context, mut share: ShareDialog) -> Option<ShareDialog> {
        use crate::theme::{self, W};
        let mut keep_open = true;
        let clip = self.clips.iter().find(|c| c.path == share.clip).cloned();
        let tex = clip.as_ref().and_then(|c| self.thumbs.get(ctx, c).and_then(|t| t.texture.clone()));
        let title = clip.as_ref().map_or_else(|| file_name(&share.clip), |c| c.title());
        let resp = crate::ui_kit::dialog(ctx, "share_clip", 460.0, |ui| {
            let p = theme::pal(ui);
            crate::ui_kit::dialog_title(ui, "Upload to HesteFiles");
            // The clip: its picture and name.
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                let (r, _) = ui.allocate_exact_size(egui::vec2(80.0, 45.0), egui::Sense::hover());
                match &tex {
                    Some(t) => {
                        let uv = crate::filmstrip::crop_uv(t.size_vec2(), r.size());
                        egui::Image::from_texture((t.id(), r.size())).uv(uv).corner_radius(4).paint_at(ui, r);
                    }
                    None => {
                        ui.painter().rect_filled(r, 4, p.raise2);
                    }
                }
                ui.add(egui::Label::new(theme::text(&title, 13.5, W::Medium).color(p.text)).truncate());
            });
            ui.add_space(10.0);

            if !self.cloud.is_connected() {
                crate::ui_kit::dialog_text(ui, "Connect your HesteFiles account to upload clips.");
                crate::ui_kit::dialog_buttons(ui, |ui| {
                    if ui.add(crate::ui_kit::button("Connect…", true)).clicked() {
                        self.page = crate::Page::Settings;
                        self.connect_open = true;
                        keep_open = false;
                    }
                    if ui.add(crate::ui_kit::button("Cancel", false).ghost()).clicked() {
                        keep_open = false;
                    }
                });
                return;
            }

            // Where it goes: the default folder unless changed for this upload.
            let current = match share.target {
                ShareTarget::DefaultFolder => self.cloud.default_folder.clone(),
                ShareTarget::Custom => self.cloud.browser_selection(),
            };
            ui.horizontal(|ui| {
                ui.set_min_height(30.0);
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.label(theme::text("Uploads to", 13.5, W::Regular).color(p.text3));
                match &current {
                    Some(f) => ui.label(theme::text(format!("HesteFiles / {}", f.display()), 13.5, W::Medium).color(p.text)),
                    None => ui.label(theme::text("Choose a folder below", 13.5, W::Medium).color(p.warn)),
                };
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if share.picking {
                        if self.cloud.default_folder.is_some() && crate::ui_kit::Btn::ghost("Use the default").small().show(ui).clicked() {
                            share.picking = false;
                            share.target = ShareTarget::DefaultFolder;
                        }
                    } else if crate::ui_kit::Btn::ghost("Change").small().show(ui).clicked() {
                        share.picking = true;
                        share.target = ShareTarget::Custom;
                        self.cloud.open_browser(self.cloud.default_folder.clone());
                    }
                });
            });
            // The browser opens in place, its height easing in.
            let open = theme::toggle(ui.ctx(), egui::Id::new("share_browser"), share.picking, theme::NORMAL);
            if open > 0.0 {
                ui.add_space(4.0 * open);
                ui.scope(|ui| {
                    ui.multiply_opacity(open);
                    self.cloud.browser_ui(ui);
                });
            }

            let destination = match share.target {
                ShareTarget::DefaultFolder => self.cloud.default_folder.clone(),
                ShareTarget::Custom => self.cloud.browser_selection(),
            };
            // A clip in one of the library's game folders can go in a folder
            // for its game there too (made if it isn't there yet).
            let game = clip.as_ref().and_then(|c| c.folder.as_deref()).and_then(hestefiles::folder_name);
            let mut subfolder = None;
            ui.add_space(6.0);
            if let Some(game) = &game {
                let mut on = self.cloud.game_folders;
                crate::ui_kit::checkbox(ui, &mut on, &format!("Put it in a folder for {game}")).on_hover_text("Inside the folder above. It's made the first time.");
                self.cloud.set_game_folders(on);
                if on {
                    subfolder = Some(game.clone());
                }
            }
            let mut public = self.cloud.public_links;
            crate::ui_kit::checkbox(ui, &mut public, "Make a public link").on_hover_text("Anyone with the link can watch and download the clip. It's shown when the upload is done.");
            self.cloud.set_public_links(public);
            ui.add_space(4.0);
            crate::ui_kit::hint(ui, "If a file with this name is already there, HesteFiles keeps both.");
            let busy = self.cloud.upload_for(&share.clip).is_some();
            crate::ui_kit::dialog_buttons(ui, |ui| {
                let tip = if busy { "This clip is already uploading" } else { "Choose a folder you can save into" };
                let b = crate::ui_kit::Btn::primary("Upload").min_width(96.0).enabled(destination.is_some() && !busy);
                let b = if destination.is_some() && !busy { b } else { b.tip(tip) };
                if b.show(ui).clicked()
                    && let Some(to) = destination.clone()
                {
                    self.cloud.upload(share.clip.clone(), to, subfolder.clone(), public);
                    keep_open = false;
                }
                if ui.add(crate::ui_kit::button("Cancel", false).ghost()).clicked() {
                    keep_open = false;
                }
            });
        });
        (keep_open && !resp.dismissed()).then_some(share)
    }
}

/// An upload is done and has a public link: show it, with a button to copy it.
fn public_link_dialog(ctx: &egui::Context, clip: PathBuf, link: String, mut copied: bool) -> Option<Dialog> {
    use crate::theme::{self, W};
    let mut keep_open = true;
    let resp = crate::ui_kit::dialog(ctx, "public_link", 460.0, |ui| {
        let p = theme::pal(ui);
        crate::ui_kit::dialog_title(ui, "Public link ready");
        crate::ui_kit::dialog_text(ui, &format!("Anyone with this link can watch and download {}.", file_name(&clip)));
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let w = ui.available_width() - 110.0;
            egui::Frame::new().fill(p.control).corner_radius(crate::ui_kit::RADIUS).inner_margin(egui::Margin::symmetric(10, 7)).show(ui, |ui| {
                ui.set_width(w - 20.0);
                ui.add(egui::Label::new(theme::text(&link, 13.5, W::Regular).color(p.text)).truncate().selectable(true));
            });
            let text = if copied { "Copied" } else { "Copy link" };
            let b = crate::ui_kit::Btn::primary(text).min_width(104.0).id("copy_link");
            let b = if copied { b.with_icon(egui_phosphor::regular::CHECK) } else { b };
            if b.show(ui).clicked() {
                ui.ctx().copy_text(link.clone());
                copied = true;
            }
        });
        crate::ui_kit::dialog_buttons(ui, |ui| {
            if ui.add(crate::ui_kit::button("Done", false)).clicked() {
                keep_open = false;
            }
            if ui.add(crate::ui_kit::button("Open in browser", false).ghost()).clicked() {
                ui.ctx().open_url(egui::OpenUrl::new_tab(&link));
            }
        });
    });
    (keep_open && !resp.dismissed()).then_some(Dialog::PublicLink { clip, link, copied })
}
