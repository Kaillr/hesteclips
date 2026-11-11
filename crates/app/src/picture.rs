//! A screenshot in the viewer, on the same stage as a clip: fitted to the
//! window, its colours lighting the stage around it.
//!
//! Scroll to zoom in towards the pointer (a pinch, or Ctrl and scroll, too),
//! double-click for every pixel (100%) and back, drag to look around a
//! zoomed picture. Fitted, dragging it out of the window drops the file into
//! any app, as a clip's picture does. Under it: its size, and the zoom.

use std::path::Path;
use std::sync::mpsc::{self, Receiver};

use egui::{Align2, Color32, Pos2, Rect, Sense, Vec2, vec2};

use crate::theme::{self, W};

/// The bar under the picture.
const BAR_H: f32 = 48.0;
/// Zooming in goes this far past every pixel (100%), and at least this far
/// past fitted.
const PAST_100: f32 = 4.0;
const MIN_MAX_ZOOM: f32 = 4.0;

/// A screenshot read from its file, ready to show.
pub struct Loaded {
    image: egui::ColorImage,
    glow: egui::ColorImage,
    /// The file's size in pixels (the image may have been made smaller).
    pixels: [u32; 2],
    bytes: u64,
}

/// Read the picture at `path` on a thread of its own (a 4K PNG takes a
/// moment); `ctx` is woken when it's done. One bigger than the graphics card
/// takes (three 4K screens side by side, on some) is shown smaller.
pub fn load(ctx: &egui::Context, path: &Path) -> Receiver<Result<Loaded, String>> {
    let (tx, rx) = mpsc::channel();
    let max = ctx.input(|i| i.max_texture_side) as u32;
    let (path, ctx) = (path.to_path_buf(), ctx.clone());
    std::thread::spawn(move || {
        let result = (|| -> Result<Loaded, String> {
            let img = image::open(&path).map_err(|e| e.to_string())?;
            let pixels = [img.width(), img.height()];
            let img = if img.width() > max || img.height() > max { img.resize(max, max, image::imageops::FilterType::Triangle) } else { img };
            let rgba = img.to_rgba8();
            let image = egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw());
            let glow = crate::transport::glow_image(&image);
            let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            Ok(Loaded { image, glow, pixels, bytes })
        })();
        if let Err(e) = &result {
            eprintln!("couldn't open {} in the viewer: {e}", path.display());
        }
        let _ = tx.send(result);
        ctx.request_repaint();
    });
    rx
}

pub struct Picture {
    texture: egui::TextureHandle,
    glow: egui::TextureHandle,
    pixels: [u32; 2],
    bytes: u64,
    /// What it's shown as: "PNG", "JPG".
    format: String,
    /// How much bigger than fitted (1: fitted), and how far its middle is
    /// from the stage's, in points: as shown now, and where they're easing to.
    zoom: f32,
    pan: Vec2,
    zoom_to: f32,
    pan_to: Vec2,
    /// The picture was dragged out of the window this frame: share the file
    /// by drag and drop.
    pub drag_out: bool,
}

impl Picture {
    pub fn new(ctx: &egui::Context, path: &Path, l: Loaded) -> Self {
        // Mipmapped: fitted, a 4K picture is drawn at a third of its size.
        let options = egui::TextureOptions::LINEAR.with_mipmap_mode(Some(egui::TextureFilter::Linear));
        let format = path.extension().map(|e| e.to_string_lossy().to_uppercase()).unwrap_or_default();
        Self {
            texture: ctx.load_texture("picture", l.image, options),
            glow: ctx.load_texture("picture light", l.glow, egui::TextureOptions::LINEAR),
            pixels: l.pixels,
            bytes: l.bytes,
            format: if format == "JPEG" { "JPG".into() } else { format },
            zoom: 1.0,
            pan: Vec2::ZERO,
            zoom_to: 1.0,
            pan_to: Vec2::ZERO,
            drag_out: false,
        }
    }

    /// The picture, and (unless `full`, fullscreen) the bar under it.
    /// Returns whether the bar's fullscreen button was clicked.
    pub fn ui(&mut self, ui: &mut egui::Ui, full: bool) -> bool {
        let ctx = ui.ctx().clone();
        let bar_h = if full { 0.0 } else { BAR_H };
        let (stage, resp) = ui.allocate_exact_size(vec2(ui.available_width(), (ui.available_height() - bar_h).max(160.0)), Sense::click_and_drag());
        let glow = self.glow.id();
        let uv = Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0));
        let Some(fit) = crate::transport::stage(ui, stage, Some(self.texture.size_vec2()), |p, cover| {
            p.image(glow, cover, uv, Color32::WHITE);
            true
        }, |_| {}) else {
            return false;
        };
        let ppp = ctx.pixels_per_point();
        // Every pixel of the picture on one of the screen's.
        let hundred = self.texture.size_vec2().x / ppp / fit.width().max(1.0);
        let max_zoom = (hundred * PAST_100).max(MIN_MAX_ZOOM);
        let over = resp.hovered() || resp.dragged();
        let pointer = ctx.pointer_hover_pos().filter(|p| stage.contains(*p)).unwrap_or(stage.center());

        // Zoom towards the pointer: the spot under it stays there.
        if over {
            let wheel = crate::wheel::read(&ctx);
            let f = 1.0035f32.powf(wheel.scroll.y + wheel.zoom) * wheel.pinch;
            if (f - 1.0).abs() > f32::EPSILON {
                self.zoom_around(pointer, stage, (self.zoom_to * f).clamp(1.0, max_zoom));
            }
        }
        if resp.double_clicked() {
            let to = if self.zoom_to > 1.001 { 1.0 } else if hundred > 1.05 { hundred } else { 2.0 };
            self.zoom_around(resp.interact_pointer_pos().unwrap_or(pointer), stage, to.min(max_zoom));
        }
        // Zoomed: dragging looks around (at once, no easing). Fitted: it
        // drags the file out.
        let zoomed = self.zoom_to > 1.001;
        if zoomed && resp.dragged() {
            self.pan_to += resp.drag_delta();
            self.pan = self.pan_to;
        } else if !zoomed && crate::share::can_drag_out() && resp.drag_started() {
            self.drag_out = true;
        }
        self.clamp(stage, fit);

        // Eased to where it's going.
        let k = (ctx.input(|i| i.stable_dt) * 18.0).min(1.0);
        self.zoom += (self.zoom_to - self.zoom) * k;
        self.pan += (self.pan_to - self.pan) * k;
        if (self.zoom - self.zoom_to).abs() < 0.001 && (self.pan - self.pan_to).length() < 0.1 {
            (self.zoom, self.pan) = (self.zoom_to, self.pan_to);
        } else {
            ctx.request_repaint();
        }
        let shown = Rect::from_center_size(stage.center() + self.pan, fit.size() * self.zoom);
        let shown = Rect::from_min_size(((shown.min.to_vec2() * ppp).round() / ppp).to_pos2(), shown.size());
        ui.painter_at(stage).image(self.texture.id(), shown, uv, Color32::WHITE);

        let cursor = if zoomed && resp.dragged() {
            egui::CursorIcon::Grabbing
        } else if zoomed {
            egui::CursorIcon::Grab
        } else {
            egui::CursorIcon::Default
        };
        resp.on_hover_cursor(cursor);
        if full {
            return false;
        }
        self.bar(ui, fit, hundred, max_zoom, stage)
    }

    /// Zoom to `to`, keeping the spot under `at` where it is.
    fn zoom_around(&mut self, at: Pos2, stage: Rect, to: f32) {
        let centre = stage.center() + self.pan_to;
        let f = to / self.zoom_to;
        self.pan_to = (at - (at - centre) * f) - stage.center();
        self.zoom_to = to;
        if to <= 1.001 {
            self.pan_to = Vec2::ZERO;
        }
    }

    /// No looking past the picture's edges: where it's bigger than the
    /// stage it covers it; where it's smaller it's in the middle.
    fn clamp(&mut self, stage: Rect, fit: Rect) {
        let room = |zoom: f32| ((fit.size() * zoom - stage.size()) / 2.0).max(Vec2::ZERO);
        let r = room(self.zoom_to);
        self.pan_to = self.pan_to.clamp(-r, r);
        let r = room(self.zoom);
        self.pan = self.pan.clamp(-r, r);
    }

    /// Its size and format on the left; the zoom (click: fitted or 100%) and
    /// fullscreen on the right. Returns whether fullscreen was clicked.
    fn bar(&mut self, ui: &mut egui::Ui, fit: Rect, hundred: f32, max_zoom: f32, stage: Rect) -> bool {
        use crate::ui_kit::{Btn, Kind};
        use egui_phosphor::regular as icon;
        let p = theme::pal(ui);
        let rect = Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), BAR_H));
        ui.allocate_rect(rect, Sense::hover());
        let [w, h] = self.pixels;
        let info = format!("{w} × {h}  ·  {}  ·  {}", self.format, crate::export_ui::human_bytes(self.bytes));
        ui.painter().text(egui::pos2(rect.left() + 16.0, rect.center().y), Align2::LEFT_CENTER, info, theme::font(13.0, W::Regular), p.text3);
        let mut full = false;
        let mut bui = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(vec2(10.0, 8.0))).layout(egui::Layout::right_to_left(egui::Align::Center)));
        bui.spacing_mut().item_spacing.x = 4.0;
        if Btn::icon(Kind::Ghost, icon::CORNERS_OUT).small().tip("Fullscreen  (F)").show(&mut bui).clicked() {
            full = true;
        }
        // How big each of its pixels is now, as a share of one of the screen's.
        let fitted = self.zoom_to <= 1.001;
        let pct = self.zoom_to / hundred.max(f32::EPSILON) * 100.0;
        let label = if fitted { format!("Fit  {pct:.0}%") } else { format!("{pct:.0}%") };
        let tip = if fitted { "Every pixel (100%). Or scroll on the picture to zoom, double-click for 100%" } else { "Fit it to the window. Or scroll on the picture to zoom, double-click to fit" };
        if Btn::ghost(label).with_icon(icon::MAGNIFYING_GLASS).small().tip(tip).show(&mut bui).clicked() {
            let to = if fitted { (if hundred > 1.05 { hundred } else { 2.0 }).min(max_zoom) } else { 1.0 };
            self.zoom_around(stage.center(), stage, to);
            self.clamp(stage, fit);
        }
        full
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2560×1440 picture on a 1000×700 window, with what the mouse did
    /// this frame; then a few frames more for the easing to settle.
    struct Window {
        ctx: egui::Context,
        pic: Picture,
        t: f64,
    }

    impl Window {
        fn new() -> Self {
            let ctx = egui::Context::default();
            crate::theme::install_fonts(&ctx);
            // As a graphics card would say.
            ctx.run_ui(egui::RawInput { max_texture_side: Some(8192), ..Default::default() }, |_| {}).textures_delta.clear();
            let image = egui::ColorImage::filled([2560, 1440], Color32::from_rgb(20, 40, 200));
            let glow = crate::transport::glow_image(&image);
            let pic = Picture::new(&ctx, Path::new("screenshot.png"), Loaded { image, glow, pixels: [2560, 1440], bytes: 1 });
            let mut w = Self { ctx, pic, t: 0.0 };
            w.frame(Vec::new());
            w
        }

        fn frame(&mut self, events: Vec<egui::Event>) {
            self.t += 1.0 / 60.0;
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 700.0))), time: Some(self.t), max_texture_side: Some(8192), events, ..Default::default() };
            let pic = &mut self.pic;
            // Nothing's drawn: what would go to the graphics card is let go.
            self.ctx
                .run_ui(input, |ui| {
                    pic.ui(ui, false);
                })
                .textures_delta
                .clear();
        }

        fn settle(&mut self) {
            for _ in 0..60 {
                self.frame(Vec::new());
            }
        }

        fn at(&mut self, p: Pos2) {
            self.frame(vec![egui::Event::PointerMoved(p)]);
        }

        fn button(&mut self, p: Pos2, pressed: bool) {
            self.frame(vec![egui::Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() }]);
        }
    }

    #[test]
    fn zoom_look_around_and_back() {
        let mut w = Window::new();
        let spot = egui::pos2(300.0, 200.0);
        w.at(spot);
        // Scrolling up zooms in, keeping what's under the pointer there.
        let wheel = egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: vec2(0.0, 200.0), modifiers: Default::default(), phase: egui::TouchPhase::Move };
        w.frame(vec![wheel]);
        w.settle();
        assert!(w.pic.zoom_to > 1.5, "zoomed {}", w.pic.zoom_to);
        assert!((w.pic.zoom - w.pic.zoom_to).abs() < 0.001, "eased there");
        let zoom = w.pic.zoom_to;
        // Dragging moves the picture by as much, and keeps the zoom.
        let pan = w.pic.pan_to;
        w.button(spot, true);
        for i in 1..=10 {
            w.at(spot + vec2(12.0 * i as f32, 6.0 * i as f32));
        }
        w.button(spot + vec2(120.0, 60.0), false);
        w.settle();
        assert_eq!(w.pic.zoom_to, zoom, "a drag keeps the zoom");
        assert!((w.pic.pan_to - pan - vec2(120.0, 60.0)).length() < 1.0, "moved by the drag: {:?} → {:?}", pan, w.pic.pan_to);
        assert!(!w.pic.drag_out, "zoomed, a drag looks around");
        // A double-click goes back to fitted, in the middle.
        let p = egui::pos2(500.0, 300.0);
        w.at(p);
        for _ in 0..2 {
            w.button(p, true);
            w.button(p, false);
        }
        w.settle();
        assert_eq!((w.pic.zoom_to, w.pic.pan_to), (1.0, Vec2::ZERO));
        // And another to every pixel: 2560 wide on a 1000-wide stage.
        for _ in 0..2 {
            w.button(p, true);
            w.button(p, false);
        }
        w.settle();
        assert!((w.pic.zoom_to - 2.56).abs() < 0.01, "100%: {}", w.pic.zoom_to);
    }

    /// Never past the edges: however far it's dragged, the picture still
    /// covers the stage.
    #[test]
    fn stays_on_the_stage() {
        let mut w = Window::new();
        let p = egui::pos2(500.0, 300.0);
        w.at(p);
        for _ in 0..2 {
            w.button(p, true);
            w.button(p, false);
        }
        w.settle();
        w.button(p, true);
        for i in 1..=10 {
            w.at(p + vec2(400.0 * i as f32, 0.0));
        }
        w.button(p + vec2(4000.0, 0.0), false);
        w.settle();
        // 2.56 × 1000 wide on 1000: 780 to spare on each side.
        assert!((w.pic.pan_to.x - 780.0).abs() < 2.0, "{:?}", w.pic.pan_to);
    }
}
