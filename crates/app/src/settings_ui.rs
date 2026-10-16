//! The Settings page, grouped by what you're trying to do rather than by codec
//! jargon. Changes apply (and save) immediately; there's no Save button.
//!
//! Each setting is a row: what it is (and a line on why you'd change it) on the
//! left, the control on the right. In a narrow window the control moves under
//! its label. Things that reset or remove live in their own section at the end.

use crate::ui_kit::Dismissed as _;
use egui::Color32;

use crate::settings::{self, Container, Encoder, OutputResolution, ShortcutAction};
use crate::theme::{self, W, icon};
use crate::ui_kit::{self, Btn, Item, Kind, Row, Seg, Typed, ValueField};
use crate::{App, RecState, reveal_label, shortcuts};

/// Replay lengths offered as one-click choices (seconds).
pub(crate) const REPLAY_CHOICES: [u32; 8] = [15, 30, 60, 120, 300, 600, 1200, 1800];
/// The settings column's width.
const COLUMN: f32 = 680.0;
/// The groups, in order, for the section list.
const SECTIONS: [&str; 8] = ["Clipping", "Video", "Clips", "Sharing", "Saved sound", "Shortcuts", "Voice", "App"];

/// A group's rows, filtered by the search: each row says its words, and the
/// group shows only matching rows.
struct Filter {
    q: String,
}

impl Filter {
    fn shows(&self, words: &[&str]) -> bool {
        self.q.is_empty() || words.iter().any(|w| w.to_lowercase().contains(&self.q))
    }
}

impl App {
    pub(crate) fn settings_page(&mut self, ui: &mut egui::Ui) {
        if self.page == crate::Page::Settings && self.naming_open {
            self.naming_page(ui);
            return;
        }
        // ⌘F: search.
        if !ui_kit::overlay_open(ui.ctx()) && ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::F)) {
            ui.memory_mut(|m| m.request_focus(egui::Id::new("settings_search")));
        }
        // The column, and room for the section list beside it when it fits;
        // without the list the column takes the whole page.
        let page_w = ui.available_width();
        let width = if page_w >= COLUMN + 180.0 { COLUMN } else { page_w };
        self.page_head(ui, "Settings", |app, ui| {
            ui.allocate_ui(egui::vec2(ui_kit::SEARCH_W.min(ui.available_width()), ui_kit::CONTROL_H), |ui| {
                crate::library_view::search_field(ui, "settings_search", &mut app.settings_query, "Search");
            });
        });
        ui_kit::arrive(ui, |ui| self.settings_content(ui, width));
        self.reset_dialog(ui.ctx());
    }

    /// Settings under the heading: the groups, and the section list.
    fn settings_content(&mut self, ui: &mut egui::Ui, width: f32) {
        let filter = Filter { q: self.settings_query.trim().to_lowercase() };
        let wide = ui.available_width() >= width + 180.0;
        let page = ui.available_rect_before_wrap();
        let jump_to = self.settings_jump.take();
        let mut tops: Vec<(&'static str, f32)> = Vec::new();
        let mut any = false;
        let scroll = ui_kit::scroll(ui, "settings_page", |ui| {
            let origin = ui.cursor().top();
            // On the page's margin, as every page's content is (the section
            // list sits on the right).
            ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(width);
                for name in SECTIONS {
                    let top = ui.cursor().top();
                    let shown = self.settings_group(ui, name, &filter, jump_to == Some(name));
                    if shown {
                        any = true;
                        tops.push((name, top - origin));
                    }
                }
                if !any {
                    ui.add_space(40.0);
                    ui.vertical_centered(|ui| {
                        let p = theme::pal(ui);
                        ui.label(theme::text(format!("No settings match “{}”", self.settings_query.trim()), 15.0, W::Semibold).color(p.text));
                        ui.add_space(4.0);
                        ui.label(theme::text("Try a word like frame rate, folder or shortcut.", 13.5, W::Regular).color(p.text3));
                    });
                }
                ui.add_space(28.0);
            });
            });
            ui.cursor().top() - origin
        });
        let _ = scroll;
        // The section list: right-aligned under the recorder, its marker on
        // the right; the one you're at follows as you scroll.
        if wide && any {
            let offset = ui_kit::scroll_offset(ui.ctx(), "settings_page");
            let at = tops.iter().rev().find(|(_, y)| *y <= offset + 40.0).map(|(n, _)| *n).or(tops.first().map(|(n, _)| *n));
            let x = page.right();
            let mut y = page.top() + 4.0;
            let p = theme::pal(ui);
            let line_top = y;
            for (name, _) in &tops {
                let g = ui.painter().layout_no_wrap(name.to_string(), theme::font(13.0, if at == Some(*name) { W::Medium } else { W::Regular }), Color32::PLACEHOLDER);
                let r = egui::Rect::from_min_max(egui::pos2(x - 12.0 - g.size().x - 12.0, y), egui::pos2(x, y + 28.0));
                let resp = ui.interact(r, egui::Id::new(("jump", *name)), egui::Sense::click());
                let on = at == Some(*name);
                let hv = theme::toggle(ui.ctx(), resp.id.with("hv"), resp.hovered(), theme::FAST);
                let c = if on { p.text } else { theme::mix(p.text3, p.text, hv) };
                ui.painter().galley(egui::pos2(x - 12.0 - g.size().x, r.center().y - g.size().y / 2.0), g, c);
                if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    self.settings_jump = Some(name);
                }
                y += 28.0;
            }
            ui.painter().vline(x - 0.75, line_top..=y, egui::Stroke::new(1.5, p.line2));
            // The marker slides to the section you're at (which section eases,
            // not where it is: the list moving with the window takes it along).
            if let Some(i) = tops.iter().position(|(n, _)| Some(*n) == at) {
                let my = line_top + theme::value(ui.ctx(), egui::Id::new("settings_marker"), i as f32, theme::NORMAL) * 28.0;
                ui.painter().vline(x - 0.75, my..=my + 28.0, egui::Stroke::new(1.5, p.text));
            }
        }
    }

    /// One group of settings, filtered by the search. Returns whether it shows.
    fn settings_group(&mut self, ui: &mut egui::Ui, name: &'static str, f: &Filter, jump: bool) -> bool {
        // Measure first: a group with no matching rows doesn't show at all.
        let matches = match name {
            "Clipping" => f.shows(&["clip length", "replay buffer duration", "start clipping when hesteclips opens", "auto start"]),
            "Video" => f.shows(&["resolution", "height", "size", "frame rate", "fps", "quality", "bitrate", "mbps", "frame blending", "motion blur", "encoder", "hardware", "software", "file format", "mp4", "mov", "game capture", "anti-cheat", "obs"]),
            "Clips" => f.shows(&["clips folder", "location", "directory", "folder per game", "name clips after what happened", "game events", "osu", "names", "delete", "trash", "permanently"]),
            "Sharing" => f.shows(&["hestefiles", "cloud", "upload", "account", "token", "public link", "default folder", "discord", "activity", "rich presence"]),
            "Saved sound" => f.shows(&["play a sound when a clip is saved", "sound", "volume", "record the sound", "in clip", "add your own"]),
            "Shortcuts" => f.shows(&["shortcuts", "hotkey", "keyboard", "save clip", "start or stop clipping", "start or stop recording"]),
            "Voice" => f.shows(&["save by voice", "hashtag", "speech", "command", "voice samples", "review"]),
            "App" => f.shows(&["appearance", "theme", "dark", "light", "open at login", "autostart", "startup", "closing the window", "tray", "menu bar", "update", "version", "license", "licence", "about", "source code", "copyright", "open source", "reset"]),
            _ => false,
        };
        if !matches {
            return false;
        }
        let head = self.group_head(ui, name);
        if jump {
            head.scroll_to_me(Some(egui::Align::TOP));
        }
        match name {
            "Clipping" => self.clipping_group(ui, f),
            "Video" => self.video_group(ui, f),
            "Clips" => self.clips_group(ui, f),
            "Sharing" => self.sharing_group(ui, f),
            "Saved sound" => self.sound_group(ui, f),
            "Shortcuts" => self.shortcuts_group(ui, f),
            "Voice" => self.voice_group(ui, f),
            "App" => self.app_group(ui, f),
            _ => {}
        }
        true
    }

    /// A group's heading. Video says it's locked while capturing, with the
    /// button to stop.
    fn group_head(&mut self, ui: &mut egui::Ui, name: &str) -> egui::Response {
        let p = theme::pal(ui);
        ui_kit::section_head(ui, name, name == SECTIONS[0], |ui| {
                if name == "Video" && self.rec_state != RecState::Idle {
                    let what = if self.rec_state == RecState::Recording { "recording" } else { "clipping" };
                    ui.label(theme::text(format!("{}  Locked while {what}", icon::LOCK_SIMPLE), 12.5, W::Regular).color(p.text3));
                    let stop = if self.rec_state == RecState::Recording { "Stop recording" } else { "Stop clipping" };
                    if Btn::ghost(stop).small().show(ui).clicked() {
                        self.stop();
                    }
                }
        })
    }

    /// Why a setting fixed at capture start is locked now, if it is.
    fn lock(&self) -> Option<String> {
        match self.rec_state {
            RecState::Idle => None,
            RecState::Buffering => Some("Stop clipping to change this".into()),
            RecState::Recording => Some("Stop recording to change this".into()),
        }
    }

    fn clipping_group(&mut self, ui: &mut egui::Ui, f: &Filter) {
        let bytes = self.estimated_bytes(self.settings.replay_seconds as f64);
        let heavy = capture::total_memory().is_some_and(|t| bytes > t as f64 / 4.0);
        let note = format!("Save clip keeps the last {}. About {} in memory.", length_words(self.settings.replay_seconds), human_bytes(bytes));
        let note = if heavy { format!("{note} That's over a quarter of this computer's memory: games may slow down.") } else { note };
        ui_kit::list(ui, "clipping", |l| {
            if f.shows(&["clip length", "replay buffer duration", "memory"]) {
                l.row(Row::new("Clip length").tip("Clip length", "How far back Save clip reaches. While clipping, that much is kept in memory, so a longer or sharper clip uses more of it.").note(note).warn(heavy), |ui| {
                    self.length_field(ui);
                });
            }
            if f.shows(&["start clipping when hesteclips opens", "auto start"]) {
                l.row(Row::new("Start clipping when HesteClips opens"), |ui| ui_kit::switch(ui, &mut self.settings.auto_start_buffer));
            }
        });
    }

    fn video_group(&mut self, ui: &mut egui::Ui, f: &Filter) {
        let lock = self.lock();
        let mut said = false;
        let size = self.recorded_size();
        let native = capture::frame_size(&self.video_source(), None);
        let res_note = match (size, native) {
            (Some((w, h)), Some((_, nh))) if self.settings.resolution.height().is_some_and(|t| t > nh) => format!("Records at {w}×{h}: your display's size, it isn't made bigger"),
            (Some((w, h)), _) => format!("Records at {w}×{h}"),
            _ => String::new(),
        };
        let max_blend = capture::max_blend_rate(&self.video_source());
        let blends = settings::blend_choices(self.settings.fps, max_blend);
        let samples = self.settings.blend_samples();
        let blend_note = if samples > 1 {
            format!("Captured at {} fps, blended down to {}", self.settings.fps * samples, self.settings.fps)
        } else if blends.is_empty() {
            max_blend.map_or(String::new(), |hz| format!("Your display shows {hz} pictures a second: blending needs {} fps or less", hz / 2))
        } else {
            String::new()
        };
        let fps_note = max_blend.filter(|hz| self.settings.fps > *hz).map_or(String::new(), |hz| format!("More than your display shows ({hz} Hz). Fine for games running faster."));
        let per_min = human_bytes(self.estimated_bytes(60.0));
        let rec = self.recommended_mbps();
        let preset = rec.and_then(|r| [("Smaller", round_mbps(r as f64 * 0.5)), ("Balanced", r), ("High", round_mbps(r as f64 * 2.0))].into_iter().position(|(_, m)| m == self.settings.video_bitrate_mbps));
        let quality_note = format!("{}about {per_min} per minute", if preset.is_some() { "" } else { "Your own bitrate, " });
        let mut locked_click = false;
        ui_kit::list(ui, "video", |l| {
            if f.shows(&["resolution", "height", "size", "native"]) {
                let (_, c) = l.row(Row::new("Resolution").tip("Resolution", "Lower makes smaller files that are quicker to share. Native keeps every pixel of your display.").note(res_note.clone()).locked(lock.clone()), |ui| self.resolution_field(ui));
                locked_click |= c;
            }
            if f.shows(&["frame rate", "fps"]) {
                let (_, c) = l.row(Row::new("Frame rate").note(fps_note.clone()).locked(lock.clone()), |ui| self.fps_field(ui));
                locked_click |= c;
            }
            if f.shows(&["quality", "bitrate", "mbps"]) {
                let (_, c) = l.row(Row::new("Quality").tip("Quality", "How much detail each second gets. Fast motion needs more, and files grow with it. Balanced looks good in fast motion at this size and frame rate.").note(quality_note.clone()).locked(lock.clone()), |ui| self.quality_field(ui, rec, preset));
                locked_click |= c;
            }
            if cfg!(any(windows, target_os = "macos")) && f.shows(&["frame blending", "motion blur"]) {
                let (_, c) = l.row(Row::new("Frame blending").tip("Frame blending", "Captures faster than the frame rate and averages the pictures into each frame, for smooth motion like motion blur. On macOS, up to your display's refresh rate.").note(blend_note.clone()).locked(lock.clone()), |ui| self.blend_field(ui, &blends));
                locked_click |= c;
            }
            if f.shows(&["encoder", "hardware", "software", "gpu"]) {
                let (_, c) = l.row(Row::new("Encoder").tip("Encoder", "Hardware encoding barely affects your game's frame rate. Software is slower and uses the CPU.").locked(lock.clone()), |ui| {
                    let r = ui_kit::select(ui, self.settings.encoder.label(), 150.0);
                    ui_kit::menu_popup(&r, false, 260.0, |ui| {
                        for e in [Encoder::Auto, Encoder::Hardware, Encoder::Software] {
                            if Item::new(e.label()).check(self.settings.encoder == e).show(ui).clicked() {
                                self.settings.encoder = e;
                            }
                        }
                    });
                });
                locked_click |= c;
            }
            if f.shows(&["file format", "mp4", "mov", "container"]) {
                let (_, c) = l.row(Row::new("File format").tip("File format", "MP4 plays everywhere. MOV is for QuickTime and Final Cut.").locked(lock.clone()), |ui| {
                    let chosen = usize::from(self.settings.container == Container::Mov);
                    if let Some(i) = ui_kit::segmented_opts(ui, "container", &[Seg::from("MP4"), Seg::from("MOV")], chosen, None, ui.is_enabled()) {
                        self.settings.container = if i == 0 { Container::Mp4 } else { Container::Mov };
                    }
                });
                locked_click |= c;
            }
            if capture::game_hook_available() && f.shows(&["game capture", "anti-cheat", "anticheat", "obs", "fullscreen"]) {
                let (_, c) = l.row(Row::new("Game capture").tip("Game capture", "For games Windows can't record properly, like exclusive fullscreen games that record frozen, or that lose their cursor. Uses OBS Studio's game capture, only where it's needed.").note("For games that record frozen or without a cursor").locked(lock.clone()), |ui| ui_kit::switch(ui, &mut self.settings.game_capture));
                locked_click |= c;
                let on = self.settings.game_capture;
                l.well("game_capture_well", on, |l| {
                    let (_, c) = l.row(Row::new("Also on these games with anti-cheat").tip("Games with anti-cheat", "Game capture leaves games with anti-cheat alone unless you add them here. Anti-cheat can close the game, or flag your account, when game capture is loaded into it.").kid().locked(lock.clone()), |ui| self.anticheat_list(ui));
                    locked_click |= c;
                });
            }
        });
        if locked_click && !said {
            said = true;
            self.toast_icon(icon::LOCK_SIMPLE, lock.clone().unwrap_or_default());
        }
        let _ = said;
    }

    fn clips_group(&mut self, ui: &mut egui::Ui, f: &Filter) {
        let p = theme::pal(ui);
        let t = &self.settings.game_titles;
        let example = if t.cs2 { "3 kills on Mirage".to_owned() } else if t.osu { crate::osu_plays::example(t) } else if t.league { "Pentakill as Jinx".to_owned() } else { "Rampage as Juggernaut".to_owned() };
        ui_kit::list(ui, "clips", |l| {
            if f.shows(&["clips folder", "location", "directory", "output"]) {
                let path = self.settings.output_dir.display().to_string();
                let home = dirs::home_dir().map(|h| h.display().to_string()).unwrap_or_default();
                let short = if !home.is_empty() && path.starts_with(&home) { format!("~{}", &path[home.len()..]) } else { path.clone() };
                l.row(Row::new("Clips folder"), |ui| {
                    if Btn::icon(Kind::Ghost, icon::FOLDER_OPEN).small().tip(reveal_label()).show(ui).clicked() {
                        let _ = std::fs::create_dir_all(&self.settings.output_dir);
                        let _ = crate::clips::open_in_default_app(&self.settings.output_dir);
                    }
                    if Btn::normal("Change").small().show(ui).clicked()
                        && let Some(dir) = rfd::FileDialog::new().set_directory(&self.settings.output_dir).pick_folder()
                    {
                        self.settings.output_dir = dir;
                        self.refresh_clips();
                    }
                    let room = (ui.available_width() - 8.0).clamp(60.0, 240.0);
                    let job = ui_kit::single_line_job(&short, theme::font(13.0, W::Regular), p.text2, room);
                    ui.label(job).on_hover_text(&path);
                });
            }
            if f.shows(&["folder per game"]) {
                l.row(Row::new("Folder per game").tip("Folder per game", "Clips go in a folder named after the game, like osu!. Clips with no game go in Desktop."), |ui| ui_kit::switch(ui, &mut self.settings.folder_per_game));
            }
            if f.shows(&["name clips after what happened", "game events", "osu", "names", "titles", "tosu"]) {
                let note = if self.settings.game_details { format!("Like “{example}”") } else { "Clips are named by the time they were saved".to_owned() };
                l.row(Row::new("Name clips after what happened").tip("Name clips after what happened", "Clips get names like “3 kills on Mirage” or “412pp · 98.52% FC · FREEDOM DiVE”. Clips where nothing happened keep their time.").note(note), |ui| {
                    ui_kit::switch(ui, &mut self.settings.game_details);
                    let shown = theme::toggle(ui.ctx(), egui::Id::new("names_customize"), self.settings.game_details, theme::NORMAL);
                    if shown > 0.0 {
                        ui.multiply_opacity(shown);
                        if Btn::normal("Customize").trail(icon::CARET_RIGHT).small().show(ui).clicked() && self.settings.game_details {
                            self.naming_open = true;
                        }
                    }
                });
            }
            if f.shows(&["delete", "trash", "recycle", "permanently"]) {
                let bin = crate::store::bin_name();
                l.row(Row::new("When you delete a clip"), |ui| {
                    let opts = [Seg::from(&*format!("Move to {bin}").leak()), Seg::from("Delete for good")];
                    if let Some(i) = ui_kit::segmented_opts(ui, "delete_mode", &opts, usize::from(self.settings.delete_permanently), None, true) {
                        self.settings.delete_permanently = i == 1;
                    }
                });
            }
        });
    }

    fn sharing_group(&mut self, ui: &mut egui::Ui, f: &Filter) {
        let connected = self.cloud.is_connected();
        let picture = self.cloud.profile_picture().and_then(|url| self.web_images.fresh(ui.ctx(), &url));
        let mut disconnect = false;
        let mut pick = false;
        ui_kit::list(ui, "sharing", |l| {
            if f.shows(&["hestefiles", "cloud", "upload", "account", "token", "public link", "default folder", "share"]) {
                let note = match &self.cloud.connection {
                    crate::cloud::Connection::Connected(account) => format!("Connected as {}", account.user_info.username),
                    crate::cloud::Connection::Connecting => "Checking your token…".to_owned(),
                    crate::cloud::Connection::Failed(e) => format!("Couldn't connect: {e}"),
                    crate::cloud::Connection::Disconnected => "Upload clips to your HesteFiles storage and share a link.".to_owned(),
                };
                let failed = matches!(self.cloud.connection, crate::cloud::Connection::Failed(_));
                l.row(Row::new("HesteFiles").note(note).warn(failed), |ui| {
                    if connected {
                        disconnect = Btn::ghost("Disconnect").small().show(ui).clicked();
                        if let Some(tex) = &picture {
                            // Round, as HesteFiles shows it.
                            ui.add(egui::Image::from_texture((tex.id(), egui::vec2(22.0, 22.0))).corner_radius(11));
                        }
                    } else if !matches!(self.cloud.connection, crate::cloud::Connection::Connecting) && Btn::normal("Connect…").small().show(ui).clicked() {
                        self.connect_open = true;
                    }
                });
                l.well("hestefiles_well", connected, |l| {
                    let folder = self.cloud.default_folder.as_ref().map_or("Not set".to_owned(), |f| format!("HesteFiles / {}", f.display()));
                    l.row(Row::new("Upload to").kid().tip("Upload to", "Where Upload to HesteFiles puts clips unless you pick another folder when uploading."), |ui| {
                        pick = Btn::normal("Choose…").small().show(ui).clicked();
                        ui.label(theme::text(&folder, 13.0, W::Regular).color(theme::pal(ui).text2));
                    });
                    l.row(Row::new("Put each clip in a folder for its game").kid(), |ui| {
                        let mut on = self.cloud.game_folders;
                        if ui_kit::switch(ui, &mut on).changed() {
                            self.cloud.set_game_folders(on);
                        }
                    });
                    l.row(Row::new("Make a public link").kid().note("Anyone with the link can watch and download."), |ui| {
                        let mut on = self.cloud.public_links;
                        if ui_kit::switch(ui, &mut on).changed() {
                            self.cloud.set_public_links(on);
                        }
                    });
                });
            }
            if f.shows(&["discord", "activity", "rich presence"]) {
                let note = if self.settings.discord_presence && self.rec_state != RecState::Idle {
                    if self.presence.connected() { "Showing on Discord" } else { "Discord isn't running" }
                } else {
                    ""
                };
                l.row(Row::new("Discord activity").tip("Discord activity", "Your Discord profile shows “Clipping osu!” while HesteClips captures. Needs Discord running.").note(note), |ui| ui_kit::switch(ui, &mut self.settings.discord_presence));
            }
        });
        if disconnect {
            self.cloud.disconnect();
            self.toast_icon(icon::SIGN_OUT, "Disconnected from HesteFiles");
        }
        if pick {
            self.cloud.open_browser(self.cloud.default_folder.clone());
            self.dialog = Some(crate::cloud_ui::Dialog::PickDefaultFolder);
        }
        self.connect_dialog(ui.ctx());
    }

    /// "Connect HesteFiles": paste a token.
    fn connect_dialog(&mut self, ctx: &egui::Context) {
        if !self.connect_open {
            return;
        }
        if self.cloud.is_connected() {
            self.connect_open = false;
            if let crate::cloud::Connection::Connected(a) = &self.cloud.connection {
                let name = a.user_info.username.clone();
                self.toast(format!("Connected as {name}"));
            }
            return;
        }
        let mut close = false;
        let modal = ui_kit::dialog(ctx, "hestefiles_connect", 420.0, |ui| {
            ui_kit::dialog_title(ui, "Connect HesteFiles");
            ui_kit::dialog_text(ui, "Create an account token on the HesteFiles website and paste it here.");
            ui.add_space(4.0);
            let mut out = egui::TextEdit::singleline(&mut self.cloud.token_input).password(true);
            out = out.hint_text("Paste account token").desired_width(f32::INFINITY);
            let p = theme::pal(ui);
            let field = egui::Frame::new().fill(p.control).corner_radius(ui_kit::RADIUS).inner_margin(egui::Margin::symmetric(10, 7)).show(ui, |ui| ui.add(out.frame(egui::Frame::NONE))).inner;
            if !self.connect_focused {
                field.request_focus();
                self.connect_focused = true;
            }
            let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            match &self.cloud.connection {
                crate::cloud::Connection::Failed(e) => {
                    ui.add_space(4.0);
                    ui.label(theme::text(format!("Couldn't connect: {e}"), 12.5, W::Regular).color(p.danger));
                }
                crate::cloud::Connection::Connecting => {
                    ui.add_space(4.0);
                    ui.label(theme::text("Checking…", 12.5, W::Regular).color(p.text3));
                }
                _ => {}
            }
            ui_kit::dialog_buttons(ui, |ui| {
                let can = !self.cloud.token_input.trim().is_empty();
                if (ui.add(ui_kit::button("Connect", true)).clicked() || enter) && can {
                    let token = self.cloud.token_input.clone();
                    self.cloud.connect(&token);
                }
                close = ui.add(ui_kit::button("Cancel", false).ghost()).clicked();
            });
        });
        if close || modal.dismissed() {
            self.connect_open = false;
            self.connect_focused = false;
        }
    }

    fn sound_group(&mut self, ui: &mut egui::Ui, f: &Filter) {
        let mut add = false;
        ui_kit::list(ui, "sound", |l| {
            l.row(Row::new("Play a sound when a clip is saved"), |ui| ui_kit::switch(ui, &mut self.settings.save_sound.enabled));
            let on = self.settings.save_sound.enabled;
            l.well("save_sound_well", on, |l| {
                let cfg = &mut self.settings.save_sound;
                l.row(Row::new("Sound").kid(), |ui| {
                    let name = crate::sound::BUILTIN.iter().find(|(id, _)| *id == cfg.sound).map(|(_, n)| n.to_string()).or_else(|| cfg.custom.iter().find(|c| crate::sound::id_of(c) == cfg.sound).map(|c| c.name.clone())).unwrap_or_else(|| cfg.sound.clone());
                    let r = ui_kit::select(ui, &name, 150.0);
                    let mut remove = None;
                    ui_kit::menu_popup(&r, false, 240.0, |ui| {
                        for (id, n) in crate::sound::BUILTIN {
                            if Item::new(n).check(cfg.sound == id).show(ui).clicked() {
                                cfg.sound = id.to_owned();
                                crate::sound::preload(id);
                                crate::sound::play(id, cfg.volume, cfg.recorded);
                            }
                        }
                        for (i, c) in cfg.custom.iter().enumerate() {
                            let id = crate::sound::id_of(c);
                            ui_kit::submenu(ui, Item::new(c.name.clone()).check(cfg.sound == id), |ui| {
                                if Item::new("Use it").icon(icon::CHECK).show(ui).clicked() {
                                    cfg.sound = id.clone();
                                    crate::sound::play(&id, cfg.volume, cfg.recorded);
                                }
                                if Item::new("Remove").icon(icon::TRASH).danger().show(ui).clicked() {
                                    remove = Some(i);
                                }
                            });
                        }
                        ui_kit::menu_sep(ui);
                        if Item::new("Add your own…").icon(icon::PLUS).tip("Any audio file: MP3, WAV, OGG, FLAC…").show(ui).clicked() {
                            add = true;
                        }
                    });
                    if let Some(i) = remove {
                        let gone = cfg.custom.remove(i);
                        if cfg.sound == crate::sound::id_of(&gone) {
                            cfg.sound = crate::settings::SaveSound::default().sound;
                        }
                        crate::sound::remove_custom(&gone);
                    }
                    if Btn::icon(Kind::Ghost, egui_phosphor::fill::PLAY).small().tip("Hear it").show(ui).clicked() {
                        crate::sound::play(&cfg.sound, cfg.volume, cfg.recorded);
                    }
                });
                l.row(Row::new("Volume").kid(), |ui| {
                    let p = theme::pal(ui);
                    ui.label(theme::text(format!("{:.0}%", cfg.volume * 100.0), 13.0, W::Regular).color(p.text2));
                    let r = crate::transport::slider(ui, &mut cfg.volume, 150.0).on_hover_text("Let go to hear it");
                    if r.drag_stopped() || (r.changed() && !r.dragged()) {
                        crate::sound::play(&cfg.sound, cfg.volume, cfg.recorded);
                    }
                });
                l.row(Row::new("Also put it in the clip").kid().tip("Sound in the clip", "The saved sound is also heard in the clip itself, at the moment you saved it (in clips and recordings made after it)."), |ui| ui_kit::switch(ui, &mut cfg.recorded));
            });
        });
        let _ = f;
        if add {
            let picked = rfd::FileDialog::new().add_filter("Audio", &["mp3", "wav", "ogg", "oga", "opus", "flac", "m4a", "aac", "wma"]).pick_file();
            if let Some(path) = picked {
                let cfg = &mut self.settings.save_sound;
                match crate::sound::add_custom(&path, &cfg.custom) {
                    Ok(c) => {
                        cfg.sound = crate::sound::id_of(&c);
                        crate::sound::play(&cfg.sound, cfg.volume, cfg.recorded);
                        cfg.custom.push(c);
                    }
                    Err(e) => self.toast_error(e),
                }
            }
        }
    }

    fn shortcuts_group(&mut self, ui: &mut egui::Ui, f: &Filter) {
        let p = theme::pal(ui);
        if let Err(e) = &self.hotkeys {
            ui.label(theme::text(format!("Shortcuts are unavailable: {e}"), 12.5, W::Regular).color(p.warn));
            ui.add_space(6.0);
        }
        let desktop = self.hotkeys.as_ref().is_ok_and(|h| h.via_desktop());
        ui_kit::list(ui, "shortcuts", |l| {
            for action in ShortcutAction::ALL {
                if !f.shows(&[action.label(), "shortcut", "hotkey", "keyboard"]) {
                    continue;
                }
                let note = match action {
                    ShortcutAction::SaveClip if desktop => "Works everywhere. Your desktop looks after it, and may ask you to confirm.",
                    ShortcutAction::SaveClip => "Works everywhere, even in a game",
                    _ => "",
                };
                let problem = self.shortcut_problem(action);
                let note = problem.clone().unwrap_or_else(|| note.to_owned());
                l.row(Row::new(action.label()).note(note).warn(problem.is_some()), |ui| self.shortcut_editor(ui, action));
            }
        });
    }

    fn voice_group(&mut self, ui: &mut egui::Ui, f: &Filter) {
        let problem = self.voice.problem.lock().unwrap().clone();
        let note = if self.settings.voice_clip {
            match (problem, self.voice_mic().is_none()) {
                (Some(p), _) => format!("Can't listen: {p}"),
                (None, true) => "Add a microphone on the Sources page to use it".to_owned(),
                _ => "Say “hashtag HesteClip that”".to_owned(),
            }
        } else {
            "Say “hashtag HesteClip that”".to_owned()
        };
        let (to_hear, all) = self.voice_review.summary();
        let mut review = false;
        ui_kit::list(ui, "voice", |l| {
            if f.shows(&["save by voice", "hashtag", "speech", "command", "voice"]) {
                l.row(Row::new("Save by voice").tip("Save by voice", "While clipping, say “hashtag HesteClip that” to save a clip. Listening happens on this computer, for this phrase only.").note(note), |ui| ui_kit::switch(ui, &mut self.settings.voice_clip));
            }
            if f.shows(&["voice samples", "keep", "review", "improve"]) {
                l.row(Row::new("Keep voice samples").tip("Keep voice samples", "Keeps about 6 seconds of your microphone each time the command saves a clip or nearly does. You can listen to them and mark which should have saved a clip, to help improve the detector. They stay on this computer."), |ui| ui_kit::switch(ui, &mut self.settings.voice_samples));
                let open = self.settings.voice_samples || all > 0;
                l.well("samples_well", open, |l| {
                    let label = if to_hear > 0 { format!("{to_hear} samples to listen to") } else if all > 0 { format!("{all} samples") } else { "No samples yet".to_owned() };
                    l.row(Row::new(&label).kid(), |ui| {
                        review = Btn::normal("Review").small().enabled(all > 0).show(ui).clicked();
                    });
                });
            }
        });
        if review {
            self.open_voice_review();
        }
    }

    fn app_group(&mut self, ui: &mut egui::Ui, f: &Filter) {
        use crate::update::Status;
        let status = self.updater.status();
        let line = match &status {
            Status::Unavailable => "Development build: updates only come to the installed app.".to_owned(),
            Status::Idle => "Updates install when you quit, never mid-game.".to_owned(),
            Status::Checking => "Checking for updates…".to_owned(),
            Status::Downloading(p) => format!("Downloading an update… {p}%"),
            Status::UpToDate => "You have the latest version.".to_owned(),
            Status::Ready(v) => format!("Version {v} is ready and installs when you quit HesteClips."),
            Status::Failed(e) => e.clone(),
        };
        let idle = self.rec_state == RecState::Idle;
        ui_kit::list(ui, "app", |l| {
            if f.shows(&["appearance", "theme", "dark", "light", "system"]) {
                l.row(Row::new("Appearance"), |ui| {
                    let chosen = settings::Theme::ALL.iter().position(|t| *t == self.settings.theme).unwrap_or(0);
                    let opts = [Seg { icon: Some(icon::LAPTOP), label: "System", tip: None }, Seg { icon: Some(icon::SUN), label: "Light", tip: None }, Seg { icon: Some(icon::MOON), label: "Dark", tip: None }];
                    if let Some(i) = ui_kit::segmented_opts(ui, "theme", &opts, chosen, None, true) {
                        self.settings.theme = settings::Theme::ALL[i];
                        ui.ctx().set_theme(self.settings.theme.preference());
                    }
                });
            }
            if f.shows(&["open at login", "autostart", "startup", "start with"]) {
                let mut on = crate::autostart::is_on();
                let (r, _) = l.row(Row::new(crate::autostart::label()), |ui| ui_kit::switch(ui, &mut on));
                if r.changed()
                    && let Err(e) = crate::autostart::set(on)
                {
                    self.toast_error(format!("Couldn't change that: {e}"));
                }
            }
            if self.tray.is_some() && f.shows(&["closing the window", "tray", "menu bar", "quit"]) {
                let place = if cfg!(target_os = "macos") { "menu bar" } else { "tray" };
                l.row(Row::new("Closing the window").tip("Closing the window", "In the menu bar or tray, clipping and shortcuts keep working, so you can still save clips."), |ui| {
                    let keep = format!("Keeps it in the {place}");
                    let opts = [Seg::from(&*keep.leak()), Seg::from("Quits")];
                    if let Some(i) = ui_kit::segmented_opts(ui, "close_mode", &opts, usize::from(!self.settings.close_to_tray), None, true) {
                        self.settings.close_to_tray = i == 0;
                        self.settings.close_asked = true;
                    }
                });
            }
            if f.shows(&["update", "version", "what's new"]) {
                let title = format!("HesteClips {}", self.updater.version());
                l.row(Row::new(&title).note(line.clone()), |ui| {
                    if Btn::ghost("What's new").small().show(ui).clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(format!("{}/releases", crate::update::REPO)));
                    }
                    match status {
                        Status::Ready(_) => {
                            let recording = self.rec_state == RecState::Recording;
                            if Btn::normal("Restart now").small().enabled(!recording).tip(if recording { "Finish your recording first" } else { "Restart into the new version" }).show(ui).clicked() && self.updater.install_on_exit(true) {
                                self.quit(ui.ctx());
                            }
                        }
                        Status::Unavailable => {}
                        _ => {
                            let busy = matches!(status, Status::Checking | Status::Downloading(_));
                            if Btn::normal("Check now").small().enabled(!busy).show(ui).clicked() {
                                self.updater.check();
                            }
                        }
                    }
                });
                l.row(Row::new("Update automatically"), |ui| ui_kit::switch(ui, &mut self.settings.auto_update));
            }
            // The copyright notice the GPL asks copies to keep showing.
            if f.shows(&["license", "licence", "about", "source code", "copyright", "open source"]) {
                l.row(Row::new("License").note("© Kaillr. Free software under the GNU GPL, version 3 or later."), |ui| {
                    if Btn::ghost("Source code").small().show(ui).clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(crate::update::REPO));
                    }
                });
            }
            if f.shows(&["reset", "defaults"]) {
                l.row(Row::new("Reset all settings").note("Your clips, folders, sources and HesteFiles account are kept."), |ui| {
                    let b = Btn::new(Kind::DangerText, "Reset…").small().enabled(idle).tip(if idle { "Back to the defaults" } else { "Stop capturing first" });
                    if b.show(ui).clicked() {
                        self.confirm_reset = true;
                    }
                });
            }
        });
    }

    /// The clip length: type any length (digits fill the clock: "230" is 2:30)
    /// or pick one.
    fn length_field(&mut self, ui: &mut egui::Ui) {
        let secs = self.settings.replay_seconds;
        let presets: Vec<(String, String, String)> = REPLAY_CHOICES.iter().map(|&s| (length_words(s), clock_label(s), String::new())).collect();
        let current = REPLAY_CHOICES.iter().position(|&s| s == secs);
        let field = ValueField { id: egui::Id::new("vf_length"), text: clock_label(secs), unit: "", width: 116.0, hint: "Type the digits: 230 is 2:30. ↑ ↓ change the part chosen", presets, current, range: "5 s – 1 h" };
        match field.show_time(ui, f64::from(secs), 5.0, 3600.0) {
            Typed::Preset(i) => self.settings.replay_seconds = REPLAY_CHOICES[i],
            Typed::Secs(v) if (5.0..=3600.0).contains(&v.round()) => self.settings.replay_seconds = v.round() as u32,
            Typed::Secs(v) => self.toast_error(format!("{} isn't a length from 5 seconds to an hour", length_words(v.round() as u32))),
            Typed::Text(t) => self.toast_error(format!("“{}” isn't a length from 5 seconds to an hour", t.trim())),
            Typed::None => {}
        }
    }

    fn resolution_field(&mut self, ui: &mut egui::Ui) {
        let r = self.settings.resolution;
        let native = capture::frame_size(&self.video_source(), None);
        let presets: Vec<(String, String, String)> = OutputResolution::PRESETS
            .iter()
            .map(|p| {
                let note = match (p, native) {
                    (OutputResolution::Native, Some((w, h))) => format!("{w}×{h}"),
                    _ => p.note().to_owned(),
                };
                (p.label(), p.height().map_or("Native".into(), |h| h.to_string()), note)
            })
            .collect();
        let current = OutputResolution::PRESETS.iter().position(|p| *p == r || (p.height() == r.height()));
        let text = r.height().map_or("Native".to_owned(), |h| h.to_string());
        let field = ValueField { id: egui::Id::new("vf_res"), text, unit: if r.height().is_some() { "p" } else { "" }, width: 116.0, hint: "Type any height from 144 to 4320, or Native", presets, current, range: "144–4320" };
        match field.show(ui) {
            Typed::Preset(i) => self.settings.resolution = OutputResolution::PRESETS[i],
            Typed::Text(t) => {
                let t = t.trim().trim_end_matches(['p', 'P']).trim();
                if t.to_lowercase().starts_with('n') {
                    self.settings.resolution = OutputResolution::Native;
                } else {
                    match t.parse::<u32>() {
                        Ok(h) if (144..=4320).contains(&h) => {
                            self.settings.resolution = OutputResolution::PRESETS.iter().copied().find(|p| p.height() == Some(h)).unwrap_or(OutputResolution::Custom(h));
                        }
                        _ => self.toast_error(format!("“{t}” isn't a height from 144 to 4320")),
                    }
                }
            }
            Typed::None | Typed::Secs(_) => {}
        }
    }

    fn fps_field(&mut self, ui: &mut egui::Ui) {
        let fps = self.settings.fps;
        let hz = capture::display_refresh(&self.video_source());
        let choices = settings::fps_choices(hz);
        let presets: Vec<(String, String, String)> = choices
            .iter()
            .map(|&f| (format!("{f} fps"), f.to_string(), hz.filter(|hz| f > *hz).map_or(String::new(), |hz| format!("above {hz} Hz"))))
            .collect();
        let current = choices.iter().position(|&f| f == fps);
        let max = settings::MAX_FPS;
        let hint = format!("Type any frame rate from 10 to {max}");
        let range = format!("10–{max}");
        let field = ValueField { id: egui::Id::new("vf_fps"), text: fps.to_string(), unit: "fps", width: 104.0, hint: &hint, presets, current, range: &range };
        match field.show(ui) {
            Typed::Preset(i) => self.settings.fps = choices[i],
            Typed::Text(t) => match t.trim().trim_end_matches("fps").trim().parse::<u32>() {
                Ok(v) if (10..=max).contains(&v) => self.settings.fps = v,
                _ => self.toast_error(format!("“{}” isn't a frame rate from 10 to {max}", t.trim())),
            },
            Typed::None | Typed::Secs(_) => {}
        }
    }

    /// Smaller / Balanced / High, worked out from the picture's size and
    /// frame rate, and the bitrate itself to type.
    fn quality_field(&mut self, ui: &mut egui::Ui, rec: Option<u32>, preset: Option<usize>) {
        let mbps = self.settings.video_bitrate_mbps;
        let presets: Vec<(&str, u32)> = rec.map_or(Vec::new(), |r| vec![("Smaller", round_mbps(r as f64 * 0.5)), ("Balanced", r), ("High", round_mbps(r as f64 * 2.0))]);
        let field = ValueField {
            id: egui::Id::new("vf_mbps"),
            text: mbps.to_string(),
            unit: "Mbps",
            width: 104.0,
            hint: "Type any bitrate from 1 to 300 Mbps",
            presets: presets.iter().map(|(n, m)| (n.to_string(), m.to_string(), format!("{m} Mbps"))).collect(),
            current: preset,
            range: "1–300",
        };
        match field.show(ui) {
            Typed::Preset(i) => self.settings.video_bitrate_mbps = presets[i].1,
            Typed::Text(t) => match t.trim().trim_end_matches("Mbps").trim().parse::<f64>() {
                Ok(v) if (1.0..=300.0).contains(&v) => self.settings.video_bitrate_mbps = v.round() as u32,
                _ => self.toast_error(format!("“{}” isn't a bitrate from 1 to 300 Mbps", t.trim())),
            },
            Typed::None | Typed::Secs(_) => {}
        }
        if !presets.is_empty() {
            let opts: Vec<Seg> = presets.iter().map(|(n, _)| Seg { icon: None, label: n, tip: None }).collect();
            if let Some(i) = ui_kit::segmented_opts(ui, "quality_preset", &opts, preset.unwrap_or(usize::MAX), None, ui.is_enabled()) {
                self.settings.video_bitrate_mbps = presets[i].1;
            }
        }
    }

    fn blend_field(&mut self, ui: &mut egui::Ui, choices: &[u32]) {
        let fps = self.settings.fps;
        let samples = self.settings.blend_samples();
        // After the frame rate changed: the nearest multiple of it.
        if samples > 1 {
            self.settings.blend_fps = fps * samples;
        }
        let used = choices.iter().copied().filter(|&r| r <= fps * samples).max().filter(|_| samples > 1);
        let mut presets = vec![("Off".to_owned(), "Off".to_owned(), String::new())];
        presets.extend(choices.iter().map(|r| (format!("From {r} fps"), r.to_string(), String::new())));
        let current = match used {
            None => Some(0),
            Some(r) => choices.iter().position(|c| *c == r).map(|i| i + 1),
        };
        let text = used.map_or("Off".to_owned(), |r| r.to_string());
        let unit = if used.is_some() { "fps" } else { "" };
        let field = ValueField { id: egui::Id::new("vf_blend"), text, unit, width: 104.0, hint: "Type a capture rate (a multiple of the frame rate), or Off", presets, current, range: "" };
        match field.show(ui) {
            Typed::Preset(0) => self.settings.blend_fps = 0,
            Typed::Preset(i) => self.settings.blend_fps = choices[i - 1],
            Typed::Text(t) => {
                let t = t.trim().trim_end_matches("fps").trim();
                if t.to_lowercase().starts_with('o') || t == "0" {
                    self.settings.blend_fps = 0;
                } else {
                    match t.parse::<u32>() {
                        Ok(v) if choices.contains(&v) => self.settings.blend_fps = v,
                        Ok(v) => match choices.iter().copied().min_by_key(|c| c.abs_diff(v)) {
                            Some(c) => {
                                self.settings.blend_fps = c;
                                self.toast_icon(icon::INFO, format!("Blending goes in steps of the frame rate: {c} fps"));
                            }
                            None => self.toast_error("Blending needs a lower frame rate on this display"),
                        },
                        Err(_) => self.toast_error(format!("“{t}” isn't a capture rate")),
                    }
                }
            }
            Typed::None | Typed::Secs(_) => {}
        }
    }

    /// The games with anti-cheat game capture may be used on anyway: tags,
    /// and Add.
    fn anticheat_list(&mut self, ui: &mut egui::Ui) {
        let p = theme::pal(ui);
        let add = Btn::ghost("Add").with_icon(icon::PLUS).small().id("anticheat_add").show(ui);
        if add.clicked() {
            self.anticheat_apps = capture::running_anticheat_apps();
        }
        ui_kit::menu_popup(&add, false, 260.0, |ui| {
            ui_kit::menu_head(ui, "Open games with anti-cheat");
            let mut any = false;
            for (app, ac) in self.anticheat_apps.clone() {
                if self.settings.game_capture_allowed.iter().any(|g| g.id.eq_ignore_ascii_case(&app.id)) {
                    continue;
                }
                any = true;
                if Item::new(app.name.clone()).value(ac.clone()).tip(app.id.clone()).show(ui).clicked() {
                    self.settings.game_capture_allowed.push(settings::AllowedGame { id: app.id, name: app.name, anticheat: ac });
                }
            }
            if !any {
                ui_kit::menu_head(ui, "No other game with anti-cheat is open. Start the game, then add it here.");
            }
        });
        let mut remove = None;
        for (i, game) in self.settings.game_capture_allowed.iter().enumerate().rev() {
            // A tag: its name and an ×.
            let g = ui.painter().layout_no_wrap(game.name.clone(), theme::font(12.5, W::Medium), p.text);
            let (r, _) = ui.allocate_exact_size(egui::vec2(g.size().x + 9.0 + 22.0, 24.0), egui::Sense::hover());
            ui.painter().rect_filled(r, 12, p.control);
            ui.painter().galley(egui::pos2(r.left() + 9.0, r.center().y - g.size().y / 2.0), g, p.text);
            let x = egui::Rect::from_center_size(egui::pos2(r.right() - 12.0, r.center().y), egui::vec2(18.0, 18.0));
            let xr = ui.interact(x, egui::Id::new(("allow_rm", i)), egui::Sense::click());
            if xr.hovered() {
                ui.painter().circle_filled(x.center(), 9.0, p.control_h);
            }
            ui.painter().text(x.center(), egui::Align2::CENTER_CENTER, icon::X, egui::FontId::proportional(11.0), if xr.hovered() { p.text } else { p.text3 });
            if xr.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(format!("No game capture on {} again ({})", game.name, game.anticheat)).clicked() {
                remove = Some(i);
            }
        }
        if let Some(i) = remove {
            self.settings.game_capture_allowed.remove(i);
        }
    }

    /// What's wrong with a shortcut, if anything.
    fn shortcut_problem(&self, action: ShortcutAction) -> Option<String> {
        let listening = self.recording_shortcut.map(|(a, _)| a) == Some(action);
        match (listening, self.recording_shortcut.and_then(|(_, e)| e)) {
            (true, Some(e)) => Some(e.to_owned()),
            (false, _) => {
                if let Some(e) = self.hotkeys.as_ref().ok().and_then(|h| h.error_for(action)) {
                    return Some(format!("Not working: {e}."));
                }
                // The desktop has the last word on Wayland: say if it chose differently.
                let current = self.settings.shortcuts.get(action).to_owned();
                let keys = self.hotkeys.as_ref().ok().and_then(|h| h.assigned(action))?;
                let asked = shortcuts::label(&current);
                let same = |a: &str, b: &str| a.replace([' ', '+'], "").eq_ignore_ascii_case(&b.replace([' ', '+'], ""));
                (!same(&asked, &keys)).then(|| format!("Your desktop set it to {keys}"))
            }
            _ => None,
        }
    }

    /// The size recordings come out at with the chosen resolution, when the
    /// display is known.
    fn recorded_size(&self) -> Option<(u32, u32)> {
        capture::frame_size(&self.video_source(), self.settings.resolution.height())
    }

    /// A bitrate that looks good for the recorded size and frame rate, in
    /// Mbps (H.264 at about 0.12 bits a pixel each frame, a guess tuned for
    /// fast game footage), rounded to something readable.
    fn recommended_mbps(&self) -> Option<u32> {
        let (w, h) = self.recorded_size()?;
        Some(round_mbps(w as f64 * h as f64 * self.settings.fps as f64 * 0.12 / 1e6))
    }

    /// One shortcut: its keys as keycaps (click, then press new ones), and
    /// a menu to turn it off or back to the default.
    fn shortcut_editor(&mut self, ui: &mut egui::Ui, action: ShortcutAction) {
        let listening = self.recording_shortcut.map(|(a, _)| a) == Some(action);
        let current = self.settings.shortcuts.get(action).to_owned();
        let default = settings::Shortcuts::default().get(action).to_owned();
        if listening {
            if Btn::ghost("Cancel").small().show(ui).clicked() {
                self.recording_shortcut = None;
            }
        } else {
            let more = Btn::icon(Kind::Ghost, icon::DOTS_THREE).small().tip("Turn off, or back to the default").show(ui);
            ui_kit::menu_popup(&more, false, 240.0, |ui| {
                if Item::new("Turn off").icon(icon::PROHIBIT).enabled(!current.is_empty()).show(ui).clicked() {
                    self.settings.shortcuts.set(action, String::new());
                }
                let def = shortcuts::keys(&default).map(|k| ui_kit::key_text(&k)).unwrap_or_default();
                if Item::new("Back to the default").icon(icon::ARROW_COUNTER_CLOCKWISE).enabled(current != default).value(def).show(ui).clicked() {
                    self.settings.shortcuts.set(action, default.clone());
                }
            });
        }
        let keys = shortcuts::keys(&current).unwrap_or_else(|| vec!["Off".to_owned()]);
        let tip = if listening { "Press the new shortcut. Esc cancels." } else { "Click, then press the new shortcut" };
        let r = ui_kit::keycaps(ui, &keys, listening).on_hover_text(tip);
        if r.clicked() {
            self.recording_shortcut = if listening { None } else { Some((action, None)) };
        }
        if listening {
            self.listen_for_shortcut(ui, action);
        }
    }

    /// While recording a shortcut: take the next key press (with its modifiers).
    fn listen_for_shortcut(&mut self, ui: &mut egui::Ui, action: ShortcutAction) {
        let pressed = ui.input_mut(|i| {
            let mut found = None;
            i.events.retain(|e| match e {
                egui::Event::Key { key, pressed: true, modifiers, .. } if found.is_none() => {
                    found = Some((*key, *modifiers));
                    false
                }
                _ => true,
            });
            found
        });
        let Some((key, mods)) = pressed else { return };
        if key == egui::Key::Escape && mods.is_none() {
            self.recording_shortcut = None;
            return;
        }
        match shortcuts::from_press(key, mods) {
            Ok(text) => {
                // Taking a shortcut another action uses moves it here.
                for other in ShortcutAction::ALL {
                    if other != action && self.settings.shortcuts.get(other) == text {
                        self.settings.shortcuts.set(other, String::new());
                    }
                }
                self.settings.shortcuts.set(action, text);
                self.recording_shortcut = None;
                self.toast_icon(icon::KEYBOARD, "Shortcut changed");
            }
            Err(e) => self.recording_shortcut = Some((action, Some(e))),
        }
    }

    /// Expected size of `secs` of capture: video at the target bitrate plus every
    /// audio track the current sources record. The encoder aims for that bitrate
    /// on average, spending less on a still screen and more on fast motion.
    fn estimated_bytes(&self, secs: f64) -> f64 {
        let tracks = capture::sources::track_layout(&self.capture_sources()).0.len() as f64;
        let bits_per_s = self.settings.video_bitrate_mbps as f64 * 1e6 + tracks * capture::AUDIO_BITRATE as f64;
        // ~1% for the container.
        bits_per_s / 8.0 * secs * 1.01
    }

    fn reset_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_reset {
            return;
        }
        let modal = ui_kit::dialog(ctx, "reset_settings", 400.0, |ui| {
            ui_kit::dialog_title(ui, "Reset all settings?");
            ui_kit::dialog_text(ui, "Clipping, video, saving and shortcuts go back to their defaults. Your clips, folders, sources and HesteFiles account are kept.");
            ui_kit::dialog_buttons(ui, |ui| {
                if ui.add(ui_kit::danger_button("Reset")).clicked() {
                    let mut fresh = settings::RecordSettings::default();
                    fresh.output_dir = self.settings.output_dir.clone();
                    fresh.game_folders = std::mem::take(&mut self.settings.game_folders);
                    fresh.audio_sources = std::mem::take(&mut self.settings.audio_sources);
                    fresh.capture = std::mem::take(&mut self.settings.capture);
                    fresh.idle_apps = self.settings.idle_apps.take();
                    fresh.webcam = self.settings.webcam.take();
                    fresh.display_index = self.settings.display_index;
                    fresh.limiter = self.settings.limiter;
                    fresh.save_sound.custom = std::mem::take(&mut self.settings.save_sound.custom);
                    fresh.library_sort = self.settings.library_sort;
                    self.settings = fresh;
                    self.confirm_reset = false;
                    ui.ctx().set_theme(self.settings.theme.preference());
                    self.toast_icon(icon::ARROW_COUNTER_CLOCKWISE, "Settings reset to defaults");
                }
                if ui.add(ui_kit::button("Cancel", false).ghost()).clicked() {
                    self.confirm_reset = false;
                }
            });
        });
        if modal.dismissed() {
            self.confirm_reset = false;
        }
    }

    /// Clip names, its own page: what your clips will be called, then a card
    /// per game you turn on, with what it needs; osu!'s parts in the order
    /// they're joined, dragged or moved with the arrows, each on or off.
    fn naming_page(&mut self, ui: &mut egui::Ui) {
        // Esc, or the rail, goes back.
        if !ui_kit::overlay_open(ui.ctx()) && !ui.ctx().egui_wants_keyboard_input() && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            self.naming_open = false;
            return;
        }
        let mut back = false;
        self.page_head(
            ui,
            "",
            |_, ui| {
                back = Btn::icon(Kind::Ghost, icon::CARET_LEFT).tip("Back to Settings  (Esc)").show(ui).clicked();
                let p = theme::pal(ui);
                ui.label(theme::text("Clip names", 20.0, W::Semibold).color(p.text));
            },
        );
        if back {
            self.naming_open = false;
            return;
        }
        let width = ui.available_width().min(COLUMN);
        ui_kit::arrive(ui, |ui| ui_kit::scroll(ui, "naming_page", |ui| {
            ui.vertical(|ui| {
                ui.set_width(width);
                let p = theme::pal(ui);
                let t = self.settings.game_titles.clone();
                ui.add(egui::Label::new(theme::text("Clips get named after what happened in the game, for games that tell HesteClips. Clips where nothing happened keep their time.", 13.5, W::Regular).color(p.text2)).wrap());
                ui.add_space(18.0);
                ui.label(theme::text("Your clips will be called", 13.0, W::Semibold).color(p.text2));
                ui.add_space(6.0);
                egui::Frame::new().fill(p.raise).corner_radius(8).inner_margin(egui::Margin::symmetric(0, 4)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let examples = [
                        ("Counter-Strike 2", t.cs2, "3 kills on Mirage".to_owned()),
                        ("Dota 2", t.dota2, "Rampage as Juggernaut".to_owned()),
                        ("League of Legends", t.league, "Pentakill as Jinx".to_owned()),
                        ("osu!", t.osu, crate::osu_plays::example(&t)),
                    ];
                    for (game, on, ex) in examples {
                        ui.horizontal(|ui| {
                            ui.set_min_height(38.0);
                            ui.add_space(16.0);
                            let text = if on && !ex.is_empty() { ex } else { "14.51.07".to_owned() };
                            let c = if on { p.text } else { p.text3 };
                            let room = (ui.available_width() - 150.0).max(60.0);
                            ui.label(ui_kit::single_line_job(&text, theme::font(13.5, if on { W::Medium } else { W::Regular }), c, room));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.add_space(16.0);
                                ui.label(theme::text(game, 12.0, W::Regular).color(p.text3));
                            });
                        });
                    }
                });
                ui.add_space(22.0);
                ui.label(theme::text("Games", 13.0, W::Semibold).color(p.text2));
                ui.add_space(6.0);
                let tt = &mut self.settings.game_titles;
                game_card(ui, "Counter-Strike 2", &mut tt.cs2, "Kills, headshots and aces", Some("A small file goes in the game's cfg folder. Restart the game after turning this on."));
                game_card(ui, "Dota 2", &mut tt.dota2, "Kills and streaks", Some("Restart the game after turning this on, and add -gamestateintegration to its launch options."));
                game_card(ui, "League of Legends", &mut tt.league, "Kills and multikills", None);
                game_card(ui, "osu!", &mut tt.osu, "The play on screen longest in the clip", Some("Scores need tosu running. Without it, osu! stable clips get the map's name only."));
                let shown = theme::toggle(ui.ctx(), egui::Id::new("osu_parts_shown"), tt.osu, theme::NORMAL);
                if shown > 0.0 {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.add_space(4.0);
                        ui.label(theme::text("In osu! names", 12.5, W::Semibold).color(p.text2));
                        ui.label(theme::text("in the order they're joined", 12.5, W::Regular).color(p.text3));
                    });
                    ui.add_space(6.0);
                    ui.scope(|ui| {
                        ui.multiply_opacity(shown);
                        osu_parts_editor(ui, tt);
                    });
                }
                ui.add_space(28.0);
            });
        }));
    }
}

/// A game on the Clip names page: its switch, what it names, what it needs.
fn game_card(ui: &mut egui::Ui, game: &str, on: &mut bool, what: &str, setup: Option<&str>) {
    let p = theme::pal(ui);
    let frame = egui::Frame::new().fill(p.raise).corner_radius(8).inner_margin(egui::Margin { left: 16, right: 16, top: 12, bottom: 12 });
    ui_kit::eased_frame(ui, egui::Id::new(("game_card", game)), frame, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.label(theme::text(game, 14.0, W::Medium).color(if *on { p.text } else { p.text2 }));
                ui.label(theme::text(what, 12.5, W::Regular).color(p.text3));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| ui_kit::switch(ui, on));
        });
        if let (Some(s), true) = (setup, *on) {
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(theme::ic(icon::INFO, 13.0).color(p.text3));
                ui.add(egui::Label::new(theme::text(s, 12.5, W::Regular).color(p.text3)).wrap());
            });
        }
    });
    ui.add_space(8.0);
}

/// The parts of an osu! clip's name, in a recessed list: each on or off,
/// in the order they're joined. Drag a row by its grip (the others slide out
/// of the way), or move it with the arrows that show on hover.
fn osu_parts_editor(ui: &mut egui::Ui, t: &mut crate::settings::GameTitles) {
    use crate::settings::OsuPart;
    const ROW: f32 = 42.0;
    let p = theme::pal(ui);
    let parts = t.osu_parts();
    let drag_id = egui::Id::new("osu_parts_drag");
    // The part being dragged, and how far below its top it was grabbed.
    let mut dragging: Option<(OsuPart, f32)> = ui.data(|d| d.get_temp(drag_id));
    let mut order = parts.clone();
    let mut swap: Option<(usize, usize)> = None;
    egui::Frame::new().fill(p.well).corner_radius(6).inner_margin(egui::Margin::same(3)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        let w = ui.available_width();
        let (area, _) = ui.allocate_exact_size(egui::vec2(w, ROW * parts.len() as f32), egui::Sense::hover());
        let top = area.top();
        let pointer_y = ui.input(|i| i.pointer.latest_pos()).map(|p| p.y);
        for (i, part) in parts.iter().enumerate() {
            // Where it sits: it slides there (the dragged one follows the pointer).
            let target = top + i as f32 * ROW;
            let is_drag = dragging.is_some_and(|(d, _)| d == *part);
            let y = match (dragging, pointer_y) {
                (Some((d, grab)), Some(py)) if d == *part => (py - grab).clamp(top, top + (parts.len() - 1) as f32 * ROW),
                _ => theme::value(ui.ctx(), egui::Id::new(("osu_part_y", *part as u8)), target - top, theme::NORMAL) + top,
            };
            let rect = egui::Rect::from_min_size(egui::pos2(area.left(), y), egui::vec2(w, ROW));
            let hovered = ui.rect_contains_pointer(rect) && dragging.is_none();
            let hv = theme::toggle(ui.ctx(), egui::Id::new(("osu_part_hv", *part as u8)), hovered, theme::FAST);
            let layer = if is_drag { egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("osu_drag")) } else { ui.layer_id() };
            let painter = ui.ctx().layer_painter(layer);
            if is_drag {
                let shadow = egui::Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(90) };
                painter.add(shadow.as_shape(rect, 5));
                painter.rect_filled(rect, 5, p.pop);
            } else if hv > 0.0 {
                painter.rect_filled(rect, 5, theme::fade(p.sel, hv));
            }
            if i > 0 && dragging.is_none() {
                painter.hline(rect.left() + 34.0..=rect.right() - 8.0, rect.top(), egui::Stroke::new(1.0, p.line));
            }
            // The grip.
            let grip = egui::Rect::from_min_size(rect.min + egui::vec2(4.0, 6.0), egui::vec2(22.0, 30.0));
            let gr = ui.interact(grip, egui::Id::new(("osu_grip", *part as u8)), egui::Sense::drag()).on_hover_cursor(egui::CursorIcon::Grab).on_hover_text("Drag to reorder");
            painter.text(grip.center(), egui::Align2::CENTER_CENTER, icon::DOTS_SIX_VERTICAL, egui::FontId::proportional(16.0), if gr.hovered() || is_drag { p.text2 } else { p.text3 });
            if gr.drag_started()
                && let Some(py) = pointer_y
            {
                dragging = Some((*part, py - target));
            }
            let on = t.osu_on(*part);
            // The part as it reads, then its name.
            let sample = match part {
                OsuPart::Pp => "412pp",
                OsuPart::Misses => "FC",
                OsuPart::Accuracy => "98.52%",
                OsuPart::Mods => "+HDDT",
                OsuPart::Stars => "7.83★",
                OsuPart::Map if t.osu_artist => "xi - FREEDOM DiVE",
                OsuPart::Map => "FREEDOM DiVE",
            };
            let sc = if on { p.text } else { p.text3 };
            let sg = painter.layout_no_wrap(sample.to_owned(), theme::font(13.0, if on { W::Semibold } else { W::Medium }), sc);
            let sx = rect.left() + 34.0;
            let sw = sg.size().x;
            painter.galley(egui::pos2(sx, rect.center().y - sg.size().y / 2.0), sg, sc);
            if !on {
                painter.hline(sx..=sx + sw, rect.center().y, egui::Stroke::new(1.0, p.line2));
            }
            painter.text(egui::pos2(sx + 150.0, rect.center().y), egui::Align2::LEFT_CENTER, part.label(), theme::font(12.5, W::Regular), p.text3);
            // Its switch, "With the artist" for the map, and ↑ ↓ on hover.
            let mut cui = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(egui::vec2(8.0, 0.0))).layout(egui::Layout::right_to_left(egui::Align::Center)));
            cui.spacing_mut().item_spacing.x = 4.0;
            let mut v = on;
            if ui_kit::switch(&mut cui, &mut v).changed() {
                *t.osu_on_mut(*part) = v;
            }
            if *part == OsuPart::Map && on {
                cui.add_space(4.0);
                let artist = t.osu_artist;
                if Btn::new(if artist { Kind::On } else { Kind::Ghost }, "With the artist").small().show(&mut cui).clicked() {
                    t.osu_artist = !artist;
                }
            }
            if hv > 0.0 {
                cui.multiply_opacity(hv);
                if Btn::icon(Kind::Ghost, icon::ARROW_DOWN).small().enabled(i + 1 < parts.len()).tip("Move down").show(&mut cui).clicked() {
                    swap = Some((i, i + 1));
                }
                if Btn::icon(Kind::Ghost, icon::ARROW_UP).small().enabled(i > 0).tip("Move up").show(&mut cui).clicked() {
                    swap = Some((i, i - 1));
                }
            }
        }
        // While dragging, the part moves to where the pointer is.
        if let Some((part, grab)) = dragging {
            if ui.input(|i| i.pointer.primary_down()) {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                if let (Some(py), Some(from)) = (pointer_y, order.iter().position(|p| *p == part)) {
                    let to = (((py - grab - top) / ROW).round().max(0.0) as usize).min(order.len() - 1);
                    if to != from {
                        let moved = order.remove(from);
                        order.insert(to, moved);
                        t.osu_order = order.clone();
                    }
                }
            } else {
                dragging = None;
            }
        }
    });
    if let Some((a, b)) = swap {
        order.swap(a, b);
        t.osu_order = order;
    }
    ui.data_mut(|d| match dragging {
        Some(v) => {
            d.insert_temp(drag_id, v);
        }
        None => d.remove::<(OsuPart, f32)>(drag_id),
    });
}

/// "48 MB", "1.2 GB".
fn human_bytes(b: f64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = MB * 1024.0;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= 10.0 * MB {
        format!("{:.0} MB", b / MB)
    } else {
        format!("{:.1} MB", b / MB)
    }
}

/// A length in words, for sentences: "30 seconds", "2 minutes 30 seconds", "1 hour".
pub(crate) fn length_words(secs: u32) -> String {
    let plural = |n: u32, unit: &str| if n == 1 { format!("1 {unit}") } else { format!("{n} {unit}s") };
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    let parts: Vec<String> = [(h, "hour"), (m, "minute"), (s, "second")].into_iter().filter(|(n, _)| *n > 0).map(|(n, u)| plural(n, u)).collect();
    if parts.is_empty() { "0 seconds".into() } else { parts.join(" ") }
}

/// A length as minutes and seconds ("2:30"), for the typed-in value.
fn clock_label(secs: u32) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// A bitrate rounded to something readable: whole Mbps under 20, else to 5.
fn round_mbps(mbps: f64) -> u32 {
    if mbps < 20.0 { (mbps.round() as u32).max(2) } else { ((mbps / 5.0).round() * 5.0) as u32 }
}

#[cfg(test)]
mod length_tests {
    use super::*;

    #[test]
    fn typed_lengths() {
        assert_eq!(clock_label(30), "0:30");
        assert_eq!(clock_label(1200), "20:00");
        assert_eq!(clock_label(75), "1:15");
        assert_eq!(length_words(150), "2 minutes 30 seconds");
        assert_eq!(length_words(3600), "1 hour");
        assert_eq!(length_words(1), "1 second");
    }

    #[test]
    fn readable_bitrates() {
        assert_eq!(round_mbps(12.4), 12);
        assert_eq!(round_mbps(26.5), 25);
        assert_eq!(round_mbps(0.4), 2);
    }
}
