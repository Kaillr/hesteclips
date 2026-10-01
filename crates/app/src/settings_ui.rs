//! The Settings page, grouped by what you're trying to do rather than by codec
//! jargon. Changes apply (and save) immediately; there's no Save button.

use egui::RichText;

use crate::settings::{self, Container, Encoder, FPS_CHOICES, OutputResolution, RateControl};
use crate::{App, RecState, hotkey_label, reveal_label};

/// Replay lengths offered as one-click choices (seconds).
const REPLAY_CHOICES: [u32; 6] = [15, 30, 60, 120, 180, 300];

impl App {
    pub(crate) fn settings_page(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.set_max_width(720.0);

            if self.rec_state != RecState::Idle {
                ui.add_space(8.0);
                ui.weak("Changes apply the next time you start the buffer or a recording.");
            }

            section(ui, "Capture", |ui| {
                form(ui, "capture", |ui| {
                    ui.label("Screen");
                    let names: Vec<String> = self.screens.iter().map(|d| d.name.clone()).collect();
                    let current = names
                        .get(self.settings.display_index)
                        .cloned()
                        .unwrap_or_else(|| "Main display".to_owned());
                    egui::ComboBox::from_id_salt("display").selected_text(current).show_ui(ui, |ui| {
                        for (i, name) in names.iter().enumerate() {
                            ui.selectable_value(&mut self.settings.display_index, i, name);
                        }
                    });
                    ui.end_row();

                    ui.label("Replay length");
                    ui.horizontal(|ui| {
                        for secs in REPLAY_CHOICES {
                            let label = if secs < 60 { format!("{secs}s") } else { format!("{} min", secs / 60) };
                            ui.selectable_value(&mut self.settings.replay_seconds, secs, label);
                        }
                    })
                    .response
                    .on_hover_text("How far back Save clip reaches.");
                    ui.end_row();

                    ui.label("Quality");
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("resolution")
                            .selected_text(self.settings.resolution.label())
                            .show_ui(ui, |ui| {
                                for r in OutputResolution::ALL {
                                    ui.selectable_value(&mut self.settings.resolution, r, r.label());
                                }
                            });
                        egui::ComboBox::from_id_salt("fps")
                            .selected_text(format!("{} fps", self.settings.fps))
                            .show_ui(ui, |ui| {
                                for f in FPS_CHOICES {
                                    ui.selectable_value(&mut self.settings.fps, f, format!("{f} fps"));
                                }
                            });
                    });
                    ui.end_row();

                    ui.label("");
                    ui.checkbox(&mut self.settings.auto_start_buffer, "Start the replay buffer when HesteClips opens");
                    ui.end_row();
                });
            });

            section(ui, "Clips & storage", |ui| {
                form(ui, "storage", |ui| {
                    ui.label("Save clips to");
                    ui.horizontal(|ui| {
                        ui.label(self.settings.output_dir.display().to_string());
                        if ui.button("Change…").clicked() {
                            if let Some(dir) = rfd::FileDialog::new()
                                .set_directory(&self.settings.output_dir)
                                .pick_folder()
                            {
                                self.settings.output_dir = dir;
                                self.refresh_clips();
                            }
                        }
                        if ui.button(reveal_label()).clicked() {
                            let _ = std::fs::create_dir_all(&self.settings.output_dir);
                            let _ = crate::clips::open_in_default_app(&self.settings.output_dir);
                        }
                    });
                    ui.end_row();

                    ui.label("File format");
                    ui.horizontal(|ui| {
                        for c in [Container::Mp4, Container::Mov] {
                            ui.selectable_value(&mut self.settings.container, c, c.label());
                        }
                    });
                    ui.end_row();
                });
            });

            section(ui, "HesteFiles", |ui| self.hestefiles_settings(ui));

            section(ui, "Shortcuts", |ui| {
                ui.weak("Work everywhere, even while a game has focus.");
                ui.add_space(6.0);
                form(ui, "shortcuts", |ui| {
                    for (key, what) in [
                        ("F10", "Save clip"),
                        ("F8", "Start / stop replay buffer"),
                        ("F9", "Start / stop recording"),
                    ] {
                        ui.label(RichText::new(hotkey_label(key)).monospace().strong());
                        ui.label(what);
                        ui.end_row();
                    }
                });
                if let Some(e) = &self.hotkey_error {
                    ui.colored_label(ui.visuals().warn_fg_color, format!("Shortcuts are unavailable: {e}"));
                }
            });

            ui.add_space(12.0);
            egui::CollapsingHeader::new(RichText::new("Advanced encoding").strong())
                .default_open(false)
                .show(ui, |ui| self.advanced(ui));
            ui.add_space(24.0);
        });
    }

    fn advanced(&mut self, ui: &mut egui::Ui) {
        ui.weak("The defaults are tuned for clips. Change these only if you know you need to.");
        ui.add_space(6.0);
        form(ui, "advanced", |ui| {
            ui.label("Bitrate");
            ui.add(egui::Slider::new(&mut self.settings.video_bitrate_mbps, 5..=150).suffix(" Mbps"));
            ui.end_row();

            ui.label("Encoder");
            egui::ComboBox::from_id_salt("encoder")
                .selected_text(self.settings.encoder.label())
                .show_ui(ui, |ui| {
                    for e in [Encoder::Auto, Encoder::Hardware, Encoder::Software] {
                        ui.selectable_value(&mut self.settings.encoder, e, e.label());
                    }
                });
            ui.end_row();

            ui.label("Rate control");
            egui::ComboBox::from_id_salt("rate_control")
                .selected_text(self.settings.rate_control.label())
                .show_ui(ui, |ui| {
                    for rc in [RateControl::Cbr, RateControl::Cqp] {
                        ui.selectable_value(&mut self.settings.rate_control, rc, rc.label());
                    }
                });
            ui.end_row();

            ui.label("Keyframe interval");
            ui.add(egui::Slider::new(&mut self.settings.keyframe_interval_secs, 1..=10).suffix(" s"));
            ui.end_row();
        });
        ui.add_space(6.0);
        if ui.button("Reset all settings to defaults").clicked() {
            // Your folder and your sources (on their own page) stay as they are.
            let mut fresh = settings::RecordSettings::default();
            fresh.output_dir = self.settings.output_dir.clone();
            fresh.audio_sources = std::mem::take(&mut self.settings.audio_sources);
            fresh.limiter = self.settings.limiter;
            self.settings = fresh;
        }
    }
}

/// A titled block on the settings page.
pub(crate) fn section(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(18.0);
    ui.label(RichText::new(title).size(16.0).strong());
    ui.add_space(6.0);
    egui::Frame::group(ui.style()).inner_margin(egui::Margin::same(12)).corner_radius(8).show(ui, |ui| {
        ui.set_width(ui.available_width());
        add(ui);
    });
}

/// Two-column label/control grid.
fn form(ui: &mut egui::Ui, id: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Grid::new(id).num_columns(2).spacing([20.0, 12.0]).min_col_width(110.0).show(ui, add);
}
