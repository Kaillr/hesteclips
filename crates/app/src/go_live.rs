//! Going live, asked first: where the stream goes, and what everyone
//! watching will see and hear. Nothing starts a stream without it (no
//! shortcut, nor the tray), so going live is never an accident.

use egui::vec2;

use crate::App;
use crate::settings_ui::Pane;
use crate::theme::{self, W};
use crate::ui_kit::{self, Btn, Kind};
use crate::ui_kit::Dismissed as _;

impl App {
    /// "Go live on Twitch and YouTube?", while it's asked for (Go live… in
    /// the recorder's menu).
    pub(crate) fn go_live_dialog(&mut self, ctx: &egui::Context) {
        if !self.go_live_open {
            return;
        }
        if self.rec_state.streaming || !capture::STREAMING {
            self.go_live_open = false;
            return;
        }
        let places: Vec<String> = self.settings.stream.ready().map(|d| d.name()).collect();
        let lines = self.go_live_lines();
        let (mut go, mut close, mut set_up) = (false, false, false);
        let modal = ui_kit::dialog(ctx, "go_live", 440.0, |ui| {
            let p = theme::pal(ui);
            if places.is_empty() {
                ui_kit::dialog_title(ui, "Where to stream to?");
                ui_kit::dialog_text(ui, "Add Twitch, YouTube, Kick or a server of your own in Settings, with its stream key. Go live then streams to all of them at once.");
                ui_kit::dialog_buttons(ui, |ui| {
                    set_up = ui.add(ui_kit::button("Open Streaming settings", true)).clicked();
                    close = ui.add(ui_kit::button("Cancel", false).ghost()).clicked();
                });
                return;
            }
            ui_kit::dialog_title(ui, &format!("Go live on {}?", crate::words_and(&places)));
            ui_kit::dialog_text(ui, "Everyone watching sees what's recorded and hears your clips' sound, as it happens.");
            ui.add_space(6.0);
            egui::Grid::new("go_live_grid").num_columns(2).spacing(vec2(18.0, 6.0)).show(ui, |ui| {
                for (what, value) in &lines {
                    ui.label(theme::text(*what, 13.0, W::Medium).color(p.text3));
                    ui.add(egui::Label::new(theme::text(value, 13.0, W::Regular).color(p.text)).wrap());
                    ui.end_row();
                }
            });
            ui_kit::dialog_buttons(ui, |ui| {
                go = Btn::new(Kind::Live, "Go live").with_icon(egui_phosphor::fill::BROADCAST).filled().show(ui).clicked();
                close = ui.add(ui_kit::button("Cancel", false).ghost()).clicked();
            });
        });
        if go {
            self.go_live_open = false;
            self.go_live();
        } else if set_up {
            self.go_live_open = false;
            self.open_settings(Pane::Streaming);
        } else if close || modal.dismissed() {
            self.go_live_open = false;
        }
    }

    /// What goes out, line by line: what's in it, the picture, and what
    /// else goes on beside it.
    fn go_live_lines(&self) -> Vec<(&'static str, String)> {
        let s = &self.settings;
        let mut lines = vec![("What", self.sources_summary())];
        let fps = capture::stream::stream_rate(s.fps, s.stream.fps);
        let size = self.recorded_size().map(|(w, h)| capture::output_size(w, h, s.stream.short_side()));
        let size = size.map_or_else(|| format!("{}p", s.stream.height), |(w, h)| format!("{w}×{h}"));
        lines.push(("Picture", format!("{size} at {fps} fps, {} kbps", thousands(s.stream.kbps))));
        let looks = s.blend_samples();
        if looks > 1 {
            lines.push(("Frame blending", format!("Blended from {} fps", s.fps * looks)));
        }
        let also: Vec<&str> = [(self.rec_state.clipping, "Clipping"), (self.rec_state.recording, "Recording")].into_iter().filter(|(on, _)| *on).map(|(_, w)| w).collect();
        if !also.is_empty() {
            lines.push(("Goes on", format!("{}, as it is", also.join(" and "))));
        }
        lines
    }
}

/// A number with its thousands apart: 6000 is "6,000".
fn thousands(n: u32) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn thousands_apart() {
        assert_eq!(super::thousands(6000), "6,000");
        assert_eq!(super::thousands(500), "500");
        assert_eq!(super::thousands(51000), "51,000");
        assert_eq!(super::thousands(1234567), "1,234,567");
    }
}
