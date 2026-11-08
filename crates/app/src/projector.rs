//! Preview on monitor: the video, fullscreen on a display of its own and
//! letterboxed in black, as OBS's projector: to line sources up on, and to
//! watch what's captured. A window of its own (an egui viewport), put on
//! the display and made fullscreen there; Esc, a double-click or its menu
//! closes it. It shows what the video shows: the scene shown.

use std::time::{Duration, Instant};

use egui::{Color32, Rect, Sense, pos2, vec2};

use crate::App;
use crate::ui_kit::{self, Item};

/// How long "Esc closes it" shows as it opens.
const NOTE_FOR: Duration = Duration::from_millis(2500);

/// The preview open on a display.
pub(crate) struct Projector {
    /// The display's id and name.
    display: String,
    name: String,
    opened: Instant,
    /// Fullscreen on its display (put there first, where its place is known).
    placed: bool,
    /// Frames it's been trying to get there.
    tries: u32,
}

/// Frames to try to put the window on its display before it's made
/// fullscreen wherever it is.
const TRIES: u32 = 60;

impl App {
    /// Preview on display `d`, instead of where it was.
    pub(crate) fn open_projector(&mut self, d: &capture::Device) {
        self.projector = Some(Projector { display: d.id.clone(), name: d.name.clone(), opened: Instant::now(), placed: false, tries: 0 });
    }

    /// The displays to preview on, one an item each (the one it's on
    /// checked), and closing it while it's open: in a submenu. The display
    /// picked.
    pub(crate) fn projector_items(&mut self, ui: &mut egui::Ui) -> Option<capture::Device> {
        if self.screens.is_empty() {
            self.screens = capture::list_screens();
        }
        let mut picked = None;
        for d in self.screens.clone() {
            let on = self.projector.as_ref().is_some_and(|p| p.display == d.id);
            if Item::new(d.name.clone()).check(on).show(ui).clicked() {
                picked = Some(d);
            }
        }
        if self.screens.is_empty() {
            ui_kit::menu_head(ui, "No displays found.");
        }
        if self.projector.is_some() {
            ui_kit::menu_sep(ui);
            if Item::new("Close the preview").keys(Some(vec!["Esc".to_owned()])).show(ui).clicked() {
                self.projector = None;
                ui.close();
            }
        }
        picked
    }

    /// The preview's window, while it's open, every frame (its frames keep
    /// coming whatever page is shown).
    pub(crate) fn projector_window(&mut self, ctx: &egui::Context) {
        let Some(p) = self.projector.as_ref() else { return };
        let bounds = capture::display_bounds(&p.display);
        let id = egui::ViewportId::from_hash_of(("hasteclip_projector", &p.display));
        // A window with a frame, hidden till it's fullscreen: winit draws a
        // frameless one a pixel down (its drop shadow's room), a light line
        // along its top, and fullscreen takes the frame off by itself.
        // Opened on its display (as near as points go).
        let mut builder = egui::ViewportBuilder::default().with_title(format!("HasteClip preview: {}", p.name)).with_visible(false).with_inner_size([960.0, 540.0]);
        if let Some(b) = bounds {
            builder = builder.with_position(inside(b, ctx.pixels_per_point()));
        }
        let mut close = false;
        ctx.show_viewport_immediate(id, builder, |ui, _| close = self.projector_frame(ui, bounds));
        if close {
            self.projector = None;
        }
        ctx.request_repaint_after(Duration::from_secs_f64(0.5 / self.settings.fps.max(1) as f64));
    }

    /// One frame of the preview's window: put on its display, then the
    /// video in the middle, as big as fits. Whether it's to close.
    fn projector_frame(&mut self, ui: &mut egui::Ui, bounds: Option<[i32; 4]>) -> bool {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        // On its display (moved there till it is: points don't map onto a
        // display of another scale exactly), then fullscreen there, shown.
        // Where its display isn't known, or it can't be got there, fullscreen
        // where it is.
        if let Some(p) = self.projector.as_mut().filter(|p| !p.placed) {
            p.tries += 1;
            let ppp = ctx.pixels_per_point();
            let there = match bounds {
                Some(b) => ctx.input(|i| i.viewport().outer_rect).is_some_and(|r| on_display(r.center(), ppp, b)),
                None => true,
            };
            if there || p.tries > TRIES {
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                p.placed = true;
            } else if let Some(b) = bounds {
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(inside(b, ppp)));
            }
        }
        let _ = self.pull_preview(ctx);
        let opened = self.projector.as_ref().map_or(Instant::now(), |p| p.opened);
        let mut close = false;
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(Color32::BLACK)).show(ui, |ui| {
            let rect = ui.max_rect();
            let resp = ui.interact(rect, ui.id().with("projector"), Sense::click());
            match self.preview_size() {
                Some((w, h)) => {
                    let f = crate::canvas_ui::fitted(w as f32 / h.max(1) as f32, rect.width() / rect.height().max(1.0));
                    let shown = Rect::from_min_max(pos2(rect.left() + f.min.x * rect.width(), rect.top() + f.min.y * rect.height()), pos2(rect.left() + f.max.x * rect.width(), rect.top() + f.max.y * rect.height()));
                    self.paint_preview(ui.painter(), shown);
                }
                None => {
                    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "Starting preview…", egui::FontId::proportional(15.0), Color32::from_gray(150));
                }
            }
            // How to close it, for a moment as it opens.
            let t = opened.elapsed();
            if t < NOTE_FOR {
                let fade = 1.0 - ((t.as_secs_f32() - (NOTE_FOR.as_secs_f32() - 0.5)) / 0.5).clamp(0.0, 1.0);
                let g = ui.painter().layout_no_wrap("Esc closes the preview".to_owned(), egui::FontId::proportional(14.0), Color32::from_gray(235).gamma_multiply(fade));
                let pill = Rect::from_center_size(pos2(rect.center().x, rect.top() + 40.0), g.size() + vec2(24.0, 12.0));
                ui.painter().rect_filled(pill, pill.height() / 2.0, Color32::from_black_alpha((170.0 * fade) as u8));
                ui.painter().galley(pill.center() - g.size() / 2.0, g, Color32::WHITE);
                ui.ctx().request_repaint();
            }
            if resp.double_clicked() {
                close = true;
            }
            ui_kit::context_menu(&resp, 200.0, |ui| {
                if Item::new("Close the preview").keys(Some(vec!["Esc".to_owned()])).show(ui).clicked() {
                    close = true;
                    ui.close();
                }
            });
        });
        close || ctx.input(|i| i.key_pressed(egui::Key::Escape) || i.viewport().close_requested())
    }
}

/// A point a quarter of the way into display `[x, y, w, h]` (in its
/// pixels), in points at `ppp`: where the window goes to be on it.
fn inside([x, y, w, h]: [i32; 4], ppp: f32) -> egui::Pos2 {
    pos2((x as f32 + w as f32 / 4.0) / ppp, (y as f32 + h as f32 / 4.0) / ppp)
}

/// Whether `p` (in points at `ppp`) is on display `[x, y, w, h]`.
fn on_display(p: egui::Pos2, ppp: f32, [x, y, w, h]: [i32; 4]) -> bool {
    let (px, py) = (p.x * ppp, p.y * ppp);
    px >= x as f32 && px < (x + w) as f32 && py >= y as f32 && py < (y + h) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Put a quarter of the way in, it's on the display, at any scale.
    #[test]
    fn put_on_its_display() {
        let second = [2560, 0, 1920, 1080];
        for ppp in [1.0, 1.25, 1.5, 2.0] {
            assert!(on_display(inside(second, ppp), ppp, second), "{ppp}");
            assert!(!on_display(inside([0, 0, 2560, 1440], ppp), ppp, second), "{ppp}");
        }
    }
}
