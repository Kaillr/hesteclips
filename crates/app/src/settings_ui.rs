//! The Settings page, grouped by what you're trying to do rather than by codec
//! jargon. Changes apply (and save) immediately; there's no Save button.
//!
//! Each setting is a row: what it is (and a line on why you'd change it) on the
//! left, the control on the right. In a narrow window the control moves under
//! its label. Things that reset or remove live in their own section at the end.

use egui::{Color32, RichText};

use crate::settings::{self, Container, Encoder, FPS_CHOICES, OutputResolution, ShortcutAction};
use crate::{App, RecState, reveal_label, shortcuts};

/// Replay lengths offered as one-click choices (seconds).
const REPLAY_CHOICES: [u32; 6] = [15, 30, 60, 120, 180, 300];
/// The page's content column never gets wider than this.
const MAX_WIDTH: f32 = 760.0;
/// Below this content width, controls go under their labels.
const NARROW: f32 = 560.0;
/// Width of the label column in a wide row.
const LABEL_W: f32 = 210.0;

impl App {
    pub(crate) fn settings_page(&mut self, ui: &mut egui::Ui) {
        crate::ui_kit::page(ui, "settings_page", MAX_WIDTH, |ui| self.settings_column(ui));
    }

    fn settings_column(&mut self, ui: &mut egui::Ui) {
        // How the video is encoded and the file it goes in are set when the
        // buffer or a recording starts, so they're locked while one runs
        // rather than looking changed and not applying. The replay length and
        // where clips are saved change right away.
        let idle = self.rec_state == RecState::Idle;
        let stop_first = self.stop_first();
        match self.rec_state {
            RecState::Idle => {}
            RecState::Buffering => {
                ui.add_space(12.0);
                note(ui, "Stop the replay buffer to change video quality or file format. The rest changes right away.");
            }
            RecState::Recording => {
                ui.add_space(12.0);
                note(ui, "Stop recording to change video quality or file format. The rest changes right away.");
            }
        }

        section(ui, "Replay buffer", |ui| {
            row(ui, "Length", Some("How far back Save clip reaches."), |ui| {
                ui.horizontal_wrapped(|ui| {
                    for secs in REPLAY_CHOICES {
                        let label = if secs < 60 { format!("{secs} s") } else { format!("{} min", secs / 60) };
                        ui.selectable_value(&mut self.settings.replay_seconds, secs, label);
                    }
                });
                let size = human_bytes(self.estimated_bytes(self.settings.replay_seconds as f64));
                ui.weak(format!("A full clip is about {size}"))
                    .on_hover_text(self.estimate_explainer());
            });
            divider(ui);
            row(ui, "Start when HesteClips opens", None, |ui| {
                toggle(ui, &mut self.settings.auto_start_buffer);
            });
        });

        section(ui, "Video quality", |ui| {
            ui.weak("What to record is chosen on the Sources page.");
            ui.add_space(4.0);
            ui.add_enabled_ui(idle, |ui| {
                row(ui, "Resolution", None, |ui| {
                    egui::ComboBox::from_id_salt("resolution").selected_text(self.settings.resolution.label()).show_ui(ui, |ui| {
                        for r in OutputResolution::ALL {
                            ui.selectable_value(&mut self.settings.resolution, r, r.label());
                        }
                    });
                });
                divider(ui);
                row(ui, "Frame rate", None, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        for f in FPS_CHOICES {
                            ui.selectable_value(&mut self.settings.fps, f, format!("{f}"));
                        }
                        ui.weak("fps");
                    });
                });
                divider(ui);
                row(ui, "Bitrate", Some("Higher keeps fast motion sharp; files get bigger."), |ui| {
                    ui.add(egui::Slider::new(&mut self.settings.video_bitrate_mbps, 5..=150).suffix(" Mbps"));
                    let per_min = human_bytes(self.estimated_bytes(60.0));
                    ui.weak(format!("About {per_min} per minute"))
                        .on_hover_text(self.estimate_explainer());
                });
                divider(ui);
                egui::CollapsingHeader::new(RichText::new("Advanced").strong())
                    .id_salt("video_advanced")
                    .default_open(false)
                    .show(ui, |ui| {
                        ui.weak("Tuned for clips already. Change these only if you know you need to.");
                        ui.add_space(4.0);
                        self.advanced(ui);
                    });
            })
            .response
            .on_disabled_hover_text(stop_first);
        });

        section(ui, "Saving", |ui| {
            {
                row(ui, "Clips folder", None, |ui| {
                    ui.vertical(|ui| {
                        ui.add(egui::Label::new(RichText::new(self.settings.output_dir.display().to_string()).monospace().size(12.0)).truncate())
                            .on_hover_text(self.settings.output_dir.display().to_string());
                        ui.horizontal(|ui| {
                            if ui.button("Change…").clicked() {
                                if let Some(dir) = rfd::FileDialog::new().set_directory(&self.settings.output_dir).pick_folder() {
                                    self.settings.output_dir = dir;
                                    self.refresh_clips();
                                }
                            }
                            if ui.button(reveal_label()).clicked() {
                                let _ = std::fs::create_dir_all(&self.settings.output_dir);
                                let _ = crate::clips::open_in_default_app(&self.settings.output_dir);
                            }
                        });
                    });
                });
                divider(ui);
                let hint = "Named after the game, like osu!. Clips with no game go in Desktop.";
                row(ui, "A folder per game", Some(hint), |ui| {
                    toggle(ui, &mut self.settings.folder_per_game);
                });
                divider(ui);
                let hint = "Like \"3 kills on Mirage\" or \"412pp · 98.52% FC · +HDDT · FREEDOM DiVE\". Clips where nothing happened keep their time.";
                row(ui, "Name clips after what happened", Some(hint), |ui| {
                    toggle(ui, &mut self.settings.game_details);
                });
                if self.settings.game_details {
                    let t = &mut self.settings.game_titles;
                    let hint = "CS2 and Dota 2 get a small file in their game folder: restart them after turning this on. Dota 2 also needs -gamestateintegration in its launch options.";
                    row(ui, "Games", Some(hint), |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.checkbox(&mut t.cs2, "Counter-Strike 2");
                            ui.checkbox(&mut t.dota2, "Dota 2");
                            ui.checkbox(&mut t.league, "League of Legends");
                            ui.checkbox(&mut t.osu, "osu!");
                        });
                    });
                    if t.osu {
                        let hint = "Scores need tosu running. Without it, osu! stable clips get the map's name only.";
                        row(ui, "In osu! names", Some(hint), |ui| {
                            ui.horizontal_wrapped(|ui| {
                                ui.checkbox(&mut t.osu_accuracy, "Accuracy").on_hover_text("98.52% FC, misses, and the combo mid-play");
                                ui.checkbox(&mut t.osu_pp, "pp");
                                ui.checkbox(&mut t.osu_mods, "Mods");
                                ui.checkbox(&mut t.osu_stars, "Star rating");
                                ui.checkbox(&mut t.osu_artist, "Artist");
                            });
                        });
                    }
                }
                divider(ui);
                let bin = crate::store::bin_name();
                let hint = format!("Instead of moving them to the {bin}. You're asked first, as it can't be undone.");
                row(ui, "Delete clips permanently", Some(&hint), |ui| {
                    toggle(ui, &mut self.settings.delete_permanently);
                });
                divider(ui);
                row(ui, "File format", Some("MP4 plays everywhere."), |ui| {
                    ui.add_enabled_ui(idle, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            for c in [Container::Mp4, Container::Mov] {
                                ui.selectable_value(&mut self.settings.container, c, c.ext().to_uppercase())
                                    .on_hover_text(c.label())
                                    .on_disabled_hover_text(stop_first);
                            }
                        });
                    });
                });
            }
        });

        section(ui, "Sound", |ui| self.sound_settings(ui));

        section(ui, "Shortcuts", |ui| {
            if self.hotkeys.as_ref().is_ok_and(|h| h.via_desktop()) {
                ui.weak(
                    "These work everywhere, even while a game has focus. Your desktop looks after them: \
                     HesteClips asks it for the keys set here (it may ask you to confirm), and they can also \
                     be changed in its keyboard settings.",
                );
            } else {
                ui.weak("These work everywhere, even while a game has focus. Click one, then press the new keys.");
            }
            ui.add_space(4.0);
            if let Err(e) = &self.hotkeys {
                ui.colored_label(ui.visuals().warn_fg_color, format!("Shortcuts are unavailable: {e}"));
            }
            for (i, action) in ShortcutAction::ALL.into_iter().enumerate() {
                if i > 0 {
                    divider(ui);
                }
                row(ui, action.label(), None, |ui| self.shortcut_editor(ui, action));
            }
        });

        section(ui, "Voice", |ui| {
            let hint = "While the replay buffer runs, saying it saves a clip. Listening happens on this computer, for this phrase only.";
            row(ui, "Say \"hashtag HesteClip that\"", Some(hint), |ui| {
                ui.horizontal(|ui| {
                    toggle(ui, &mut self.settings.voice_clip);
                    let problem = self.voice.problem.lock().unwrap().clone();
                    if self.settings.voice_clip {
                        if let Some(p) = problem {
                            ui.add_space(6.0);
                            ui.colored_label(ui.visuals().warn_fg_color, format!("Can't listen: {p}"));
                        } else if self.voice_mic().is_none() {
                            ui.add_space(6.0);
                            ui.weak("Add a microphone on the Sources page to use it.");
                        }
                    }
                });
            });
        });

        section(ui, "HesteFiles", |ui| self.hestefiles_settings(ui));

        section(ui, "Discord", |ui| {
            let hint = "Your profile shows \"Clipping osu!\" while capturing. Needs Discord running.";
            row(ui, "Show as your Discord activity", Some(hint), |ui| {
                ui.horizontal(|ui| {
                    toggle(ui, &mut self.settings.discord_presence);
                    if self.settings.discord_presence && self.rec_state != RecState::Idle {
                        ui.add_space(6.0);
                        if self.presence.connected() {
                            ui.weak("Showing on Discord");
                        } else {
                            ui.weak("Discord isn't running");
                        }
                    }
                });
            });
        });

        section(ui, "Updates", |ui| self.update_settings(ui));


        section(ui, "Reset", |ui| {
            row(ui, "Reset all settings", Some("Your clips, clips folder, sources and HesteFiles account are kept."), |ui| {
                let reset = egui::Button::new(RichText::new("Reset to defaults…").color(ui.visuals().error_fg_color));
                if ui.add_enabled(idle, reset).on_disabled_hover_text("Stop capturing first").clicked() {
                    self.confirm_reset = true;
                }
            });
        });
        ui.add_space(24.0);
        self.reset_dialog(ui.ctx());
    }

    /// Which version this is, what the updater is up to, and whether it runs on its own.
    fn update_settings(&mut self, ui: &mut egui::Ui) {
        use crate::update::Status;
        let status = self.updater.status();
        let line = match &status {
            Status::Unavailable => "Development build: updates only come to the installed app.".to_owned(),
            Status::Idle => String::new(),
            Status::Checking => "Checking for updates…".to_owned(),
            Status::Downloading(p) => format!("Downloading an update… {p}%"),
            Status::UpToDate => "You have the latest version.".to_owned(),
            Status::Ready(v) => format!("Version {v} is ready and installs when you quit HesteClips."),
            Status::Failed(e) => e.clone(),
        };
        row(ui, &format!("HesteClips {}", self.updater.version()), (!line.is_empty()).then_some(line.as_str()), |ui| {
            ui.horizontal(|ui| {
                match status {
                    Status::Ready(_) => {
                        let recording = self.rec_state == RecState::Recording;
                        let restart = ui
                            .add_enabled(!recording, egui::Button::new("Restart now"))
                            .on_disabled_hover_text("Finish your recording first");
                        if restart.clicked() && self.updater.install_on_exit(true) {
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    }
                    Status::Unavailable => {}
                    _ => {
                        let busy = matches!(status, Status::Checking | Status::Downloading(_));
                        if ui.add_enabled(!busy, egui::Button::new("Check for updates")).clicked() {
                            self.updater.check();
                        }
                    }
                }
                if ui.link("What's new").clicked() {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(format!("{}/releases", crate::update::REPO)));
                }
            });
        });
        divider(ui);
        row(ui, "Update automatically", Some("They install when you quit, never mid-game."), |ui| {
            toggle(ui, &mut self.settings.auto_update);
        });
    }

    /// The clip-saved sound: on/off, which one (built-in or your own), volume,
    /// each with a way to hear it.
    fn sound_settings(&mut self, ui: &mut egui::Ui) {
        let cfg = &mut self.settings.save_sound;
        row(ui, "Play a sound when a clip is saved", Some("Never recorded into your clips."), |ui| {
            toggle(ui, &mut cfg.enabled);
        });
        divider(ui);
        ui.add_enabled_ui(cfg.enabled, |ui| {
            row(ui, "Sound", None, |ui| {
                ui.vertical(|ui| {
                    let mut remove = None;
                    let volume = cfg.volume;
                    let mut choice = |ui: &mut egui::Ui, id: String, name: &str| -> bool {
                        let mut gone = false;
                        ui.horizontal(|ui| {
                            if ui.add(egui::Button::new("▶").min_size(egui::vec2(26.0, 0.0))).on_hover_text("Hear it").clicked() {
                                crate::sound::play(&id, volume);
                            }
                            if ui.radio(cfg.sound == id, name).clicked() {
                                cfg.sound = id.clone();
                                crate::sound::preload(&id);
                            }
                            gone = id.starts_with("file:") && ui.small_button("✕").on_hover_text("Remove this sound").clicked();
                        });
                        gone
                    };
                    for (id, name) in crate::sound::BUILTIN {
                        choice(ui, id.to_owned(), name);
                    }
                    for (i, c) in cfg.custom.clone().iter().enumerate() {
                        if choice(ui, crate::sound::id_of(c), &c.name) {
                            remove = Some(i);
                        }
                    }
                    if let Some(i) = remove {
                        let gone = cfg.custom.remove(i);
                        if cfg.sound == crate::sound::id_of(&gone) {
                            cfg.sound = crate::settings::SaveSound::default().sound;
                        }
                        crate::sound::remove_custom(&gone);
                    }
                    if ui.button("Add your own…").on_hover_text("Any audio file: MP3, WAV, OGG, FLAC…").clicked() {
                        let picked = rfd::FileDialog::new()
                            .add_filter("Audio", &["mp3", "wav", "ogg", "oga", "opus", "flac", "m4a", "aac", "wma"])
                            .pick_file();
                        if let Some(path) = picked {
                            match crate::sound::add_custom(&path, &cfg.custom) {
                                Ok(c) => {
                                    cfg.sound = crate::sound::id_of(&c);
                                    crate::sound::play(&cfg.sound, cfg.volume);
                                    cfg.custom.push(c);
                                }
                                Err(e) => self.sound_error = Some(e),
                            }
                        }
                    }
                    if let Some(e) = &self.sound_error {
                        ui.colored_label(ui.visuals().error_fg_color, e);
                    }
                });
            });
            divider(ui);
            row(ui, "Volume", Some("Let go of the slider to hear it."), |ui| {
                let slider = ui.add(egui::Slider::new(&mut cfg.volume, 0.0..=1.0).custom_formatter(|v, _| format!("{:.0}%", v * 100.0)));
                if slider.drag_stopped() || (slider.changed() && !slider.dragged()) {
                    crate::sound::play(&cfg.sound, cfg.volume);
                }
            });
        });
    }

    fn advanced(&mut self, ui: &mut egui::Ui) {
        row(ui, "Encoder", Some("Hardware encoding barely affects your game's frame rate."), |ui| {
            egui::ComboBox::from_id_salt("encoder").selected_text(self.settings.encoder.label()).show_ui(ui, |ui| {
                for e in [Encoder::Auto, Encoder::Hardware, Encoder::Software] {
                    ui.selectable_value(&mut self.settings.encoder, e, e.label());
                }
            });
        });

        divider(ui);
        row(ui, "Keyframe interval", Some("Shorter: edits save faster, files get a little bigger."), |ui| {
            ui.add(egui::Slider::new(&mut self.settings.keyframe_interval_secs, 1..=10).suffix(" s"));
        });
    }

    /// One shortcut: a button showing the keys (click to record new ones), and
    /// buttons to turn it off or restore the default.
    fn shortcut_editor(&mut self, ui: &mut egui::Ui, action: ShortcutAction) {
        let listening = self.recording_shortcut.map(|(a, _)| a) == Some(action);
        let current = self.settings.shortcuts.get(action).to_owned();
        let default = settings::Shortcuts::default().get(action).to_owned();
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                // A field showing the keys as keycaps; click it to record new ones.
                let v = ui.visuals().clone();
                let keys = shortcuts::keys(&current);
                let frame = egui::Frame::new()
                    .fill(if listening { crate::library::ACCENT.gamma_multiply(0.25) } else { v.extreme_bg_color })
                    .stroke(egui::Stroke::new(1.0, if listening { crate::library::ACCENT } else { v.widgets.noninteractive.bg_stroke.color }))
                    .corner_radius(6)
                    .inner_margin(egui::Margin::symmetric(8, 4));
                let field = frame.show(ui, |ui| {
                    ui.set_min_size(egui::vec2(140.0, 24.0));
                    ui.horizontal_centered(|ui| match (&keys, listening) {
                        (_, true) => {
                            ui.label(RichText::new("Press keys…").color(v.strong_text_color()));
                        }
                        (Some(k), false) => {
                            shortcuts::keycaps(ui, k, 12.5);
                        }
                        (None, false) => {
                            ui.weak("Off");
                        }
                    });
                });
                let tip = if listening { "Press the new shortcut. Esc cancels." } else { "Click, then press the new shortcut" };
                let r = ui.interact(field.response.rect, ui.id().with(("shortcut", action.label())), egui::Sense::click());
                if r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip).clicked() {
                    self.recording_shortcut = if listening { None } else { Some((action, None)) };
                }
                if listening {
                    if ui.button("Cancel").clicked() {
                        self.recording_shortcut = None;
                    }
                } else {
                    if !current.is_empty() && ui.small_button("Turn off").clicked() {
                        self.settings.shortcuts.set(action, String::new());
                    }
                    if current != default && ui.small_button("Default").on_hover_text(shortcuts::label(&default)).clicked() {
                        self.settings.shortcuts.set(action, default.clone());
                    }
                }
            });
            if listening {
                self.listen_for_shortcut(ui, action);
            }
            let problem = match (listening, self.recording_shortcut.and_then(|(_, e)| e)) {
                (true, Some(e)) => Some(e.to_owned()),
                (false, _) => self.hotkeys.as_ref().ok().and_then(|h| h.error_for(action)).map(|e| format!("Not working: {e}.")),
                _ => None,
            };
            if let Some(p) = problem {
                ui.colored_label(ui.visuals().warn_fg_color, p);
            } else if let Some(keys) = self.hotkeys.as_ref().ok().and_then(|h| h.assigned(action)) {
                // The desktop has the last word on Wayland: say if it chose differently.
                let asked = shortcuts::label(&current);
                let same = |a: &str, b: &str| a.replace([' ', '+'], "").eq_ignore_ascii_case(&b.replace([' ', '+'], ""));
                if !listening && !same(&asked, &keys) {
                    ui.weak(format!("Your desktop set it to {keys}"));
                }
            }
        });
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

    fn estimate_explainer(&self) -> String {
        let tracks = capture::sources::track_layout(&self.capture_sources()).0.len();
        format!(
            "Video at {} Mbps plus {tracks} audio track{} at {} kbps. A mostly still screen comes out smaller, fast motion a bit bigger.",
            self.settings.video_bitrate_mbps,
            if tracks == 1 { "" } else { "s" },
            capture::AUDIO_BITRATE / 1000
        )
    }

    fn reset_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_reset {
            return;
        }
        let modal = egui::Modal::new(egui::Id::new("reset_settings")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.heading("Reset all settings?");
            ui.add_space(4.0);
            ui.label("Replay buffer, video, saving and shortcuts go back to their defaults.");
            crate::ui_kit::hint(ui, "Your clips, clips folder, sources and HesteFiles account are kept.");
            ui.add_space(12.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(crate::ui_kit::danger_button("Reset")).clicked() {
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
                    self.settings = fresh;
                    self.confirm_reset = false;
                    self.toast("Settings reset to defaults");
                }
                if ui.button("Cancel").clicked() {
                    self.confirm_reset = false;
                }
            });
        });
        if modal.should_close() {
            self.confirm_reset = false;
        }
    }
}

/// A titled block on the settings page.
pub(crate) fn section(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    crate::ui_kit::section(ui, title, add);
}

/// One setting: label (and hint) on the left, control on the right; stacked when
/// the window is narrow.
pub(crate) fn row(ui: &mut egui::Ui, label: &str, hint: Option<&str>, control: impl FnOnce(&mut egui::Ui)) {
    let narrow = ui.available_width() < NARROW;
    let labels = |ui: &mut egui::Ui| {
        ui.label(label);
        if let Some(h) = hint {
            crate::ui_kit::hint(ui, h);
        }
    };
    ui.add_space(6.0);
    if narrow {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            labels(ui);
        });
        ui.add_space(4.0);
        control(ui);
    } else {
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(egui::vec2(LABEL_W, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(LABEL_W);
                ui.add_space(3.0);
                ui.spacing_mut().item_spacing.y = 2.0;
                labels(ui);
            });
            ui.add_space(12.0);
            // Controls are a little taller than a line of text; nudge the label
            // column down so its first line sits level with the control.
            ui.vertical(|ui| control(ui));
        });
    }
    ui.add_space(6.0);
}

fn divider(ui: &mut egui::Ui) {
    let r = ui.available_rect_before_wrap();
    ui.painter().hline(r.x_range(), r.top(), egui::Stroke::new(1.0, crate::ui_kit::line(ui.visuals().dark_mode)));
    ui.add_space(1.0);
}

fn note(ui: &mut egui::Ui, text: &str) {
    egui::Frame::new()
        .fill(ui.visuals().warn_fg_color.gamma_multiply(0.12))
        .corner_radius(crate::ui_kit::RADIUS)
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.colored_label(ui.visuals().warn_fg_color, text);
        });
}

/// An on/off switch, like the system's.
fn toggle(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let size = egui::vec2(36.0, 20.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_responsive(resp.id, *on);
    let v = ui.visuals();
    let fill = if *on { crate::library::ACCENT } else { v.widgets.inactive.bg_fill };
    let r = rect.height() / 2.0;
    ui.painter().rect_filled(rect, r, fill);
    let x = egui::lerp((rect.left() + r)..=(rect.right() - r), t);
    ui.painter().circle_filled(egui::pos2(x, rect.center().y), r - 3.0, Color32::WHITE);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
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
