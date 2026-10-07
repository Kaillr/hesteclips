//! HesteFiles UI: the account/default-folder settings section, the folder picker
//! and the share dialog.

use std::path::PathBuf;

use crate::{App, cloud, file_name};

pub(crate) enum Dialog {
    /// Folders-only browser for choosing the default clips folder.
    PickDefaultFolder,
    Share(ShareDialog),
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
    pub(crate) fn hestefiles_settings(&mut self, ui: &mut egui::Ui) {
        ui.weak("Share clips to your HesteFiles cloud storage. Create an account token on the HesteFiles website.");
        ui.add_space(6.0);

        egui::Grid::new("hestefiles_settings").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
            ui.label("Account");
            match &self.cloud.connection {
                cloud::Connection::Connected(account) => {
                    let (user, token) = (&account.user_info, &account.token_info);
                    let expires = if token.expires == "never" {
                        "never expires".to_owned()
                    } else {
                        format!("expires {}", token.expires)
                    };
                    let mut disconnect = false;
                    let picture = self.cloud.profile_picture().and_then(|url| self.web_images.fresh(ui.ctx(), &url));
                    ui.horizontal(|ui| {
                        match &picture {
                            // Round, as HesteFiles shows it.
                            Some(tex) => {
                                ui.add(egui::Image::from_texture((tex.id(), egui::vec2(24.0, 24.0))).corner_radius(12));
                            }
                            None => {
                                ui.colored_label(egui::Color32::from_rgb(90, 190, 110), "●");
                            }
                        }
                        ui.label(format!("Connected as {}", user.username));
                        ui.weak(format!("· token “{}”, {expires}", token.name));
                        disconnect = ui.small_button("Disconnect").clicked();
                    });
                    if disconnect {
                        self.cloud.disconnect();
                    }
                }
                cloud::Connection::Connecting => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Checking token…");
                    });
                }
                cloud::Connection::Disconnected | cloud::Connection::Failed(_) => {
                    let error = match &self.cloud.connection {
                        cloud::Connection::Failed(e) => Some(e.clone()),
                        _ => None,
                    };
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            let field = ui.add(
                                egui::TextEdit::singleline(&mut self.cloud.token_input)
                                    .password(true)
                                    .hint_text("Paste account token")
                                    .desired_width(260.0),
                            );
                            let enter = field.lost_focus()
                                && ui.input(|i| i.key_pressed(egui::Key::Enter));
                            let can = !self.cloud.token_input.trim().is_empty();
                            if (ui.add_enabled(can, egui::Button::new("Connect")).clicked() || enter)
                                && can
                            {
                                let token = self.cloud.token_input.clone();
                                self.cloud.connect(&token);
                            }
                        });
                        if let Some(e) = error {
                            ui.colored_label(ui.visuals().error_fg_color, format!("Couldn't connect: {e}"));
                        }
                    });
                }
            }
            ui.end_row();

            ui.label("Default clips folder");
            ui.horizontal(|ui| {
                match &self.cloud.default_folder {
                    Some(f) => ui.label(format!("📁 {}", f.display())),
                    None => ui.weak("Not set"),
                };
                let connected = self.cloud.is_connected();
                if ui
                    .add_enabled(connected, egui::Button::new("Choose…"))
                    .on_disabled_hover_text("Connect your account first.")
                    .clicked()
                {
                    self.cloud.open_browser(self.cloud.default_folder.clone());
                    self.dialog = Some(Dialog::PickDefaultFolder);
                }
                if self.cloud.default_folder.is_some() && ui.small_button("Clear").clicked() {
                    self.cloud.set_default_folder(None);
                }
            });
            ui.end_row();
        });
    }

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
        let Some(dialog) = self.dialog.take() else { return };
        self.dialog = match dialog {
            Dialog::PickDefaultFolder => self.pick_default_folder_dialog(ctx),
            Dialog::Share(share) => self.share_dialog(ctx, share).map(Dialog::Share),
        };
    }

    /// Returns `Some` while the dialog should stay open.
    fn pick_default_folder_dialog(&mut self, ctx: &egui::Context) -> Option<Dialog> {
        let mut keep_open = true;
        let resp = egui::Modal::new(egui::Id::new("pick_default_folder")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.heading("Default clips folder");
            ui.weak("Shared clips go here unless you pick another folder.");
            ui.add_space(6.0);
            self.cloud.browser_ui(ui);
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let selection = self.cloud.browser_selection();
                if ui
                    .add_enabled(selection.is_some(), egui::Button::new("Use this folder"))
                    .clicked()
                {
                    self.cloud.set_default_folder(selection);
                    keep_open = false;
                }
                if ui.button("Cancel").clicked() {
                    keep_open = false;
                }
            });
        });
        (keep_open && !resp.should_close()).then_some(Dialog::PickDefaultFolder)
    }

    /// Upload a clip to HesteFiles.
    ///
    /// The common case is one decision: the destination is already your clips
    /// folder, so the dialog reads "Upload <clip> to <folder>" with one button.
    /// Picking somewhere else is a small "Change" link that expands the folder
    /// browser in place — no radio buttons to puzzle over. Upload closes the
    /// dialog; the clip's card shows the progress.
    fn share_dialog(&mut self, ctx: &egui::Context, mut share: ShareDialog) -> Option<ShareDialog> {
        let mut keep_open = true;
        let resp = egui::Modal::new(egui::Id::new("share_clip")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.heading("Upload to HesteFiles");
            ui.add_space(4.0);
            ui.label(egui::RichText::new(format!("🎬  {}", file_name(&share.clip))).strong());
            ui.add_space(10.0);

            if !self.cloud.is_connected() {
                ui.label("Connect your HesteFiles account to upload clips.");
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    let go = egui::Button::new(egui::RichText::new("Open Settings").color(egui::Color32::WHITE)).fill(crate::library::ACCENT);
                    if ui.add(go).clicked() {
                        self.page = crate::Page::Settings;
                        keep_open = false;
                    }
                    if ui.button("Cancel").clicked() {
                        keep_open = false;
                    }
                });
                return;
            }

            // Where it goes: the default clips folder unless changed for this upload.
            ui.horizontal(|ui| {
                ui.weak("Uploads to");
                let current = match share.target {
                    ShareTarget::DefaultFolder => self.cloud.default_folder.clone(),
                    ShareTarget::Custom => self.cloud.browser_selection(),
                };
                match &current {
                    Some(f) => ui.label(egui::RichText::new(format!("📁 {}", f.display())).strong()),
                    None => ui.label(egui::RichText::new("Choose a folder below").color(ui.visuals().warn_fg_color)),
                };
                if !share.picking && ui.link("Change").clicked() {
                    share.picking = true;
                    share.target = ShareTarget::Custom;
                    self.cloud.open_browser(self.cloud.default_folder.clone());
                }
            });

            if share.picking {
                ui.add_space(6.0);
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    self.cloud.browser_ui(ui);
                });
                if self.cloud.default_folder.is_some() && ui.link("Use my clips folder").clicked() {
                    share.picking = false;
                    share.target = ShareTarget::DefaultFolder;
                }
            }

            let destination = match share.target {
                ShareTarget::DefaultFolder => self.cloud.default_folder.clone(),
                ShareTarget::Custom => self.cloud.browser_selection(),
            };
            // A clip in one of the library's game folders can go in a folder
            // for its game there too (made if it isn't there yet).
            let game = self.clips.iter().find(|c| c.path == share.clip).and_then(|c| c.folder.as_deref()).and_then(hestefiles::folder_name);
            let mut subfolder = None;
            if let Some(game) = &game {
                ui.add_space(8.0);
                let mut on = self.cloud.game_folders;
                ui.checkbox(&mut on, format!("Put it in a folder for {game}"))
                    .on_hover_text("Inside the folder above. It's made the first time.");
                self.cloud.set_game_folders(on);
                if on {
                    subfolder = Some(game.clone());
                }
            }
            ui.add_space(if game.is_some() { 2.0 } else { 8.0 });
            let mut public = self.cloud.public_links;
            ui.checkbox(&mut public, "Make a public link")
                .on_hover_text("Anyone with the link can watch and download the clip. It's copied for you when the upload is done.");
            self.cloud.set_public_links(public);
            ui.add_space(12.0);
            let busy = self.cloud.upload_for(&share.clip).is_some();
            ui.horizontal(|ui| {
                let upload = egui::Button::new(egui::RichText::new("☁  Upload").color(egui::Color32::WHITE))
                    .fill(crate::library::ACCENT)
                    .min_size(egui::vec2(96.0, 28.0));
                let r = ui
                    .add_enabled(destination.is_some() && !busy, upload)
                    .on_disabled_hover_text(if busy { "This clip is already uploading." } else { "Choose a folder you can save into." });
                if r.clicked() {
                    if let Some(to) = destination.clone() {
                        self.cloud.upload(share.clip.clone(), to, subfolder.clone(), public);
                        keep_open = false;
                    }
                }
                if ui.button("Cancel").clicked() {
                    keep_open = false;
                }
            });
            ui.add_space(4.0);
            ui.weak("If a file with this name is already there, HesteFiles keeps both.");
        });
        (keep_open && !resp.should_close()).then_some(share)
    }
}
