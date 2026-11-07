//! Sources in a box of their own beside the screen (`settings::Visual`):
//! another display, one app's window, an image, a colour, words. Each is
//! placed, turned and flipped on the preview as the screen is (`canvas_ui`),
//! its card under the preview saying what it shows. An image's picture and
//! words drawn are made here, again when they change, and handed to the
//! capture as they are (`capture::LayerSource::Picture`), which blends them
//! by their transparency.

use std::sync::Arc;

use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};
use egui::{Pos2, Rect, vec2};

use crate::App;
use crate::canvas_ui::Layer;
use crate::settings::{BoxCfg, Visual, VisualCfg};

/// Words' size at first, in the video's pixels at 1080p.
const TEXT_SIZE: u32 = 72;
/// The sizes offered for words.
const TEXT_SIZES: [u32; 7] = [24, 36, 48, 72, 96, 144, 200];
/// Image files offered.
const IMAGE_EXTS: [&str; 6] = ["png", "jpg", "jpeg", "webp", "bmp", "gif"];

impl App {
    /// Each image's and words' picture, made again when what it shows
    /// changes (its file, its words, their size or colour). Every frame:
    /// unchanged, it's kept.
    pub(crate) fn refresh_visual_pictures(&mut self) {
        let wanted: Vec<(u64, Visual)> = self.settings.visuals.iter().filter(|v| matches!(v.visual, Visual::Image { .. } | Visual::Text { .. })).map(|v| (v.key, v.visual.clone())).collect();
        self.canvas.pictures.retain(|k, _| wanted.iter().any(|(w, _)| w == k));
        for (key, visual) in wanted {
            if self.canvas.pictures.get(&key).is_some_and(|(was, _)| *was == visual) {
                continue;
            }
            let picture = match &visual {
                Visual::Image { path } => match load_image(path) {
                    Ok(p) => Some(Arc::new(p)),
                    Err(e) => {
                        eprintln!("image source {}: {e:#}", path.display());
                        None
                    }
                },
                Visual::Text { text, size, rgba, shadow } => Some(Arc::new(render_text(text, *size, *rgba, *shadow))),
                _ => None,
            };
            let before = self.visual_picture(key).map(|p| (p.width, p.height));
            self.canvas.pictures.insert(key, (visual.clone(), picture.clone()));
            if let (Visual::Text { .. }, Some(p)) = (&visual, picture) {
                self.fit_words_box(key, before, (p.width, p.height));
            }
        }
        // Words not placed yet (the video's shape wasn't known): now.
        let unplaced: Vec<u64> = self.settings.visuals.iter().filter(|v| v.placed.is_none() && matches!(v.visual, Visual::Text { .. })).map(|v| v.key).collect();
        for key in unplaced {
            if let Some(p) = self.visual_picture(key) {
                self.fit_words_box(key, None, (p.width, p.height));
            }
        }
    }

    /// Words' box, for their new picture (`now`, its size; `before`, the
    /// last one's): always placed (where it goes at first: their own size,
    /// in the middle), and as their words or size change, the new picture's
    /// shape at the same scale, its top-left corner kept, so the letters
    /// stay the size they were (not squashed into the old box).
    fn fit_words_box(&mut self, key: u64, before: Option<(u32, u32)>, now: (u32, u32)) {
        let Some(aspect) = self.frame_aspect() else { return };
        let Some(d) = self.settings.visual(key) else { return };
        let turned = d.turns % 2 == 1;
        let (w, h) = if turned { (now.1, now.0) } else { now };
        let natural = vec2(w as f32 / (1080.0 * aspect), h as f32 / 1080.0);
        let r = match (d.placed, before) {
            (Some(b), Some(was)) => {
                let was_w = if turned { was.1 } else { was.0 } as f32 / (1080.0 * aspect);
                let scale = b.w / was_w.max(1e-6);
                Rect::from_min_size(Pos2::new(b.x, b.y), natural * scale)
            }
            (Some(b), None) => Rect::from_min_size(Pos2::new(b.x, b.y), vec2(b.w, b.h)),
            (None, _) => {
                let fit = (1.0 / natural.x).min(1.0 / natural.y).min(1.0);
                Rect::from_center_size(Pos2::new(0.5, 0.5), natural * fit)
            }
        };
        let picture = Some(w as f32 / h.max(1) as f32);
        if let Some(d) = self.settings.visual_mut(key) {
            d.placed = Some(BoxCfg::new([r.min.x, r.min.y, r.width(), r.height()], aspect, picture));
        }
    }

    /// An image's or words' picture, once made (see `refresh_visual_pictures`).
    pub(crate) fn visual_picture(&self, key: u64) -> Option<Arc<capture::StillImage>> {
        self.canvas.pictures.get(&key)?.1.clone()
    }

    /// A boxed source's picture's size, as it shows (turned), when it's
    /// known: a display's or window's asked for now and then. A colour has none.
    pub(crate) fn visual_size(&mut self, key: u64) -> Option<(u32, u32)> {
        let v = self.settings.visual(key)?.clone();
        let asked = |this: &mut Self, ask: &dyn Fn() -> Option<(u32, u32)>| {
            let fresh = this.canvas.sizes.get(&key).is_some_and(|(_, at)| at.elapsed() < std::time::Duration::from_secs(2));
            if !fresh {
                this.canvas.sizes.insert(key, (ask(), std::time::Instant::now()));
            }
            this.canvas.sizes.get(&key).and_then(|(s, _)| *s)
        };
        let (w, h) = match &v.visual {
            Visual::Display { id, .. } => {
                let source = capture::VideoSource::Screen { id: id.clone(), hook: Default::default(), method: Default::default() };
                asked(self, &|| capture::frame_size(&source, Default::default()))?
            }
            Visual::Window { app, .. } => asked(self, &|| capture::layers::app_window_size(app))?,
            Visual::Image { .. } | Visual::Text { .. } => self.visual_picture(key).map(|p| (p.width, p.height))?,
            Visual::Browser { width, height, .. } => (*width, *height),
            Visual::Colour { .. } => return None,
        };
        Some(if v.turns % 2 == 1 { (h, w) } else { (w, h) })
    }

    /// Where a boxed source is in the video (fractions of it): its box when
    /// it's been placed by hand, else where it goes at first.
    pub(crate) fn visual_rect(&mut self, key: u64, frame_aspect: f32) -> Rect {
        if let Some(b) = self.settings.visual(key).and_then(|d| d.placed) {
            return Rect::from_min_size(Pos2::new(b.x, b.y), vec2(b.w, b.h));
        }
        self.visual_first_rect(key, frame_aspect)
    }

    /// Where a boxed source goes before it's placed: a colour all of the
    /// video; words their own size (as at 1080p), in the middle; the rest
    /// as big as fits.
    pub(crate) fn visual_first_rect(&mut self, key: u64, frame_aspect: f32) -> Rect {
        let whole = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
        let text = self.settings.visual(key).is_some_and(|v| matches!(v.visual, Visual::Text { .. }));
        match self.visual_size(key) {
            Some((w, h)) if text => {
                let size = vec2(w as f32 / (1080.0 * frame_aspect), h as f32 / 1080.0);
                let fit = (1.0 / size.x).min(1.0 / size.y).min(1.0);
                Rect::from_center_size(Pos2::new(0.5, 0.5), size * fit)
            }
            Some((w, h)) => crate::canvas_ui::fitted(w as f32 / h.max(1) as f32, frame_aspect),
            None => whole,
        }
    }

    /// A boxed source in the box `r` (fractions of a video of shape `aspect`).
    pub(crate) fn place_visual(&mut self, key: u64, r: Rect, aspect: f32) {
        let shapeless = self.settings.visual(key).is_some_and(|v| v.visual.shapeless());
        let picture = if shapeless { None } else { self.settings.visual(key).and_then(|d| d.placed).and_then(|b| b.picture_aspect).or_else(|| self.visual_size(key).map(|(w, h)| w as f32 / h.max(1) as f32)) };
        let first = self.visual_first_rect(key, aspect);
        let same = (r.min - first.min).length() < 1e-3 && (r.max - first.max).length() < 1e-3;
        if let Some(d) = self.settings.visual_mut(key) {
            d.placed = (!same).then(|| BoxCfg::new([r.min.x, r.min.y, r.width(), r.height()], aspect, picture));
        }
    }

    /// What's wrong with a boxed source, for its card's line, if anything.
    pub(crate) fn visual_state(&self, key: u64) -> Option<&'static str> {
        let v = self.settings.visual(key)?;
        match &v.visual {
            Visual::Display { id, .. } if !self.screens.is_empty() && !self.screens.iter().any(|s| s.id == *id) => Some("Not connected"),
            Visual::Window { .. } if self.canvas.sizes.get(&key).is_some_and(|(s, _)| s.is_none()) => Some("Not open"),
            Visual::Image { .. } if self.canvas.pictures.get(&key).is_some_and(|(_, p)| p.is_none()) => Some("Can't be read"),
            _ => None,
        }
    }

    /// What a boxed source shows, for its card's line.
    pub(crate) fn visual_line(&self, key: u64) -> String {
        let Some(v) = self.settings.visual(key) else { return String::new() };
        match &v.visual {
            Visual::Display { name, .. } | Visual::Window { name, .. } => name.clone(),
            Visual::Image { path } => path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned()),
            Visual::Colour { rgba } => format!("#{:02X}{:02X}{:02X}", rgba[0], rgba[1], rgba[2]),
            Visual::Text { text, .. } => text.lines().next().unwrap_or_default().to_owned(),
            Visual::Browser { url, .. } => url.clone(),
        }
    }

    /// Put `visual` in the video, in front of the rest. Its key.
    pub(crate) fn add_visual(&mut self, visual: Visual) -> u64 {
        let key = self.settings.new_key();
        self.settings.visuals.push(VisualCfg::new(key, visual));
        self.settings.layer_order.insert(0, Layer::Visual(key));
        key
    }

    /// Adding a window, an image, a colour or words (where every source is
    /// drawn: `capture::layers::MANY`), in the row of Add buttons. The one
    /// added, to open its card.
    pub(crate) fn add_visual_buttons(&mut self, ui: &mut egui::Ui) -> Option<Layer> {
        use crate::ui_kit::{Btn, Item};
        use egui_phosphor::regular as icon;
        let mut added = None;
        if capture::layers::WINDOWS {
            let b = Btn::normal("Window").with_icon(icon::PLUS).trail(icon::CARET_DOWN).small().id("add_window").tip("Put one app's window in your clips, in a box of its own: chat, a browser, a game").show(ui);
            if b.clicked() {
                self.windowed_apps = capture::list_windowed_apps();
            }
            crate::ui_kit::menu_popup(&b, true, 260.0, |ui| {
                crate::ui_kit::menu_head(ui, "Put a window in your clips");
                egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                    for app in self.windowed_apps.clone() {
                        let tex = self.app_icons.get(ui.ctx(), &app.id).map(|t| t.id());
                        if Item::new(app.name.clone()).image(tex).show(ui).clicked() {
                            added = Some(self.add_visual(Visual::Window { app: app.id, name: app.name }));
                        }
                    }
                });
                if self.windowed_apps.is_empty() {
                    crate::ui_kit::menu_head(ui, "No windows found.");
                }
            });
        }
        if Btn::normal("Image").with_icon(icon::PLUS).small().tip("Put a picture in your clips: a logo, a frame (its transparency's kept)").show(ui).clicked()
            && let Some(path) = rfd::FileDialog::new().add_filter("Images", &IMAGE_EXTS).pick_file()
        {
            added = Some(self.add_visual(Visual::Image { path }));
        }
        if Btn::normal("Colour").with_icon(icon::PLUS).small().tip("All of the video one colour (or a box of it): a background behind your webcam").show(ui).clicked() {
            let key = self.add_visual(Visual::Colour { rgba: [24, 24, 28, 255] });
            // A background: at the back.
            let mut order = self.layer_order();
            order.retain(|l| *l != Layer::Visual(key));
            order.push(Layer::Visual(key));
            self.set_layer_order(&order);
            added = Some(key);
        }
        if Btn::normal("Text").with_icon(icon::PLUS).small().tip("Put words in your clips: your name, a title").show(ui).clicked() {
            added = Some(self.add_visual(Visual::Text { text: "Your text".into(), size: TEXT_SIZE, rgba: [255, 255, 255, 255], shadow: true }));
        }
        if capture::layers::BROWSER && Btn::normal("Browser").with_icon(icon::PLUS).small().tip("Put a web page in your clips: alerts, a chat overlay, a widget (its transparency's kept)").show(ui).clicked() {
            added = Some(self.add_visual(Visual::Browser { url: "https://".into(), width: 800, height: 600 }));
        }
        added.map(Layer::Visual)
    }

    /// A boxed source's settings, in its card: what it shows (which display
    /// or window, the image, the colour, the words), then where it sits.
    pub(crate) fn visual_rows(&mut self, ui: &mut egui::Ui, key: u64) {
        use crate::theme::{self, W};
        use crate::ui_kit::{Btn, Item};
        let pal = theme::pal(ui);
        let Some(v) = self.settings.visual(key).cloned() else { return };
        let row = |ui: &mut egui::Ui, label: &str, add: &mut dyn FnMut(&mut egui::Ui)| {
            ui.horizontal(|ui| {
                ui.set_min_height(crate::sources_ui::GROUP_ROW_H);
                ui.label(theme::text(label, 13.5, W::Regular).color(pal.text));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| add(ui));
            });
        };
        let mut set: Option<Visual> = None;
        match &v.visual {
            Visual::Display { id, name } => row(ui, "Display", &mut |ui| {
                let r = crate::ui_kit::select(ui, name, 180.0);
                if r.clicked() {
                    self.sources_view.add_displays = capture::list_screens();
                }
                crate::ui_kit::menu_popup(&r, false, 240.0, |ui| {
                    for s in self.sources_view.add_displays.clone() {
                        if Item::new(s.name.clone()).check(s.id == *id).show(ui).clicked() && s.id != *id {
                            set = Some(Visual::Display { id: s.id, name: s.name });
                        }
                    }
                    if self.sources_view.add_displays.is_empty() {
                        crate::ui_kit::menu_head(ui, "No displays found.");
                    }
                });
            }),
            Visual::Window { app, name } => row(ui, "Window", &mut |ui| {
                let r = crate::ui_kit::select(ui, name, 180.0);
                if r.clicked() {
                    self.windowed_apps = capture::list_windowed_apps();
                }
                crate::ui_kit::menu_popup(&r, false, 260.0, |ui| {
                    egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                        for a in self.windowed_apps.clone() {
                            let tex = self.app_icons.get(ui.ctx(), &a.id).map(|t| t.id());
                            if Item::new(a.name.clone()).image(tex).check(a.id == *app).show(ui).clicked() && a.id != *app {
                                set = Some(Visual::Window { app: a.id, name: a.name });
                            }
                        }
                    });
                    if self.windowed_apps.is_empty() {
                        crate::ui_kit::menu_head(ui, "No windows found.");
                    }
                });
            }),
            Visual::Image { path } => row(ui, "Image", &mut |ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                if Btn::normal("Choose…").small().tip("Another picture").show(ui).clicked()
                    && let Some(p) = rfd::FileDialog::new().add_filter("Images", &IMAGE_EXTS).pick_file()
                {
                    set = Some(Visual::Image { path: p });
                }
                let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                ui.label(theme::text(name, 12.5, W::Regular).color(pal.text3));
            }),
            Visual::Colour { rgba } => row(ui, "Colour", &mut |ui| {
                if let Some(c) = colour_button(ui, *rgba) {
                    set = Some(Visual::Colour { rgba: c });
                }
            }),
            Visual::Browser { url, width, height } => {
                // Typed in full, then taken when it's done (Enter, or clicking
                // away): each key would load the page again.
                let draft_id = egui::Id::new(("browser_url", key));
                let mut draft: String = ui.data(|d| d.get_temp(draft_id)).unwrap_or_else(|| url.clone());
                let mut done = false;
                row(ui, "Address", &mut |ui| {
                    ui.set_max_width(ui.available_width().min(300.0));
                    // Free text: a web address.
                    let out = crate::ui_kit::text_field(ui, &mut draft, "https://");
                    done = out.response.lost_focus();
                });
                if done {
                    ui.data_mut(|d| d.remove::<String>(draft_id));
                    let typed = draft.trim().to_owned();
                    if typed != *url && !typed.is_empty() {
                        let typed = if typed.contains("://") { typed } else { format!("https://{typed}") };
                        set = Some(Visual::Browser { url: typed, width: *width, height: *height });
                    }
                } else {
                    ui.data_mut(|d| d.insert_temp(draft_id, draft));
                }
                crate::ui_kit::divider(ui, 0.0);
                row(ui, "Page size", &mut |ui| {
                    if let Some((w, h)) = crate::ui_kit::size_field(ui, &format!("browser_size_{key}"), (*width, *height), 16..=3840, 16..=2160) {
                        set = Some(Visual::Browser { url: url.clone(), width: w, height: h });
                    }
                });
            }
            Visual::Text { text, size, rgba, shadow } => {
                let mut words = text.clone();
                row(ui, "Text", &mut |ui| {
                    ui.set_max_width(ui.available_width().min(260.0));
                    // Free text: the words to show.
                    crate::ui_kit::text_field(ui, &mut words, "Your text");
                });
                if words != *text {
                    set = Some(Visual::Text { text: words, size: *size, rgba: *rgba, shadow: *shadow });
                }
                crate::ui_kit::divider(ui, 0.0);
                row(ui, "Size", &mut |ui| {
                    let field = crate::ui_kit::ValueField {
                        id: egui::Id::new(("visual_text_size", key)),
                        value: f64::from(*size),
                        decimals: 0,
                        range: 8.0..=600.0,
                        unit: "px",
                        width: 96.0,
                        hint: "How tall its letters are, in pixels of a 1080p video",
                        presets: TEXT_SIZES.iter().map(|&s| crate::ui_kit::Preset::new(format!("{s} px"), f64::from(s))).collect(),
                    };
                    if let Some(n) = field.show(ui) {
                        set = Some(Visual::Text { text: text.clone(), size: n.round() as u32, rgba: *rgba, shadow: *shadow });
                    }
                });
                crate::ui_kit::divider(ui, 0.0);
                row(ui, "Colour", &mut |ui| {
                    if let Some(c) = colour_button(ui, *rgba) {
                        set = Some(Visual::Text { text: text.clone(), size: *size, rgba: c, shadow: *shadow });
                    }
                });
                crate::ui_kit::divider(ui, 0.0);
                row(ui, "Shadow", &mut |ui| {
                    let mut on = *shadow;
                    if crate::ui_kit::switch(ui, &mut on).on_hover_text("A soft shadow under the letters, so they read on a bright picture").changed() {
                        set = Some(Visual::Text { text: text.clone(), size: *size, rgba: *rgba, shadow: on });
                    }
                });
            }
        }
        if let Some(new) = set {
            // Another picture's another shape: it goes where it goes at first.
            let reshaped = !matches!(new, Visual::Colour { .. } | Visual::Text { .. } | Visual::Browser { .. });
            if let Some(d) = self.settings.visual_mut(key) {
                d.visual = new;
                if reshaped {
                    d.placed = None;
                }
            }
            self.canvas.sizes.remove(&key);
        }
        crate::ui_kit::divider(ui, 0.0);
        self.placement_row(ui, Layer::Visual(key));
    }
}

/// A colour's swatch, its picker under it (with transparency): the new
/// colour, once one's picked.
fn colour_button(ui: &mut egui::Ui, rgba: [u8; 4]) -> Option<[u8; 4]> {
    let mut c = egui::Color32::from_rgba_unmultiplied(rgba[0], rgba[1], rgba[2], rgba[3]);
    let changed = egui::widgets::color_picker::color_edit_button_srgba(ui, &mut c, egui::widgets::color_picker::Alpha::OnlyBlend).changed();
    changed.then(|| c.to_srgba_unmultiplied())
}

/// An image file as a picture, its transparency kept.
fn load_image(path: &std::path::Path) -> anyhow::Result<capture::StillImage> {
    let img = image::open(path)?.to_rgba8();
    let (width, height) = img.dimensions();
    let bgra = img.pixels().flat_map(|p| [p[2], p[1], p[0], p[3]]).collect();
    Ok(capture::StillImage { width, height, bgra })
}

/// Words drawn in `rgba`, `size` pixels tall, a line for each line, on
/// nothing (transparent), with a soft shadow under them: their picture,
/// just big enough.
pub(crate) fn render_text(text: &str, size: u32, rgba: [u8; 4], shadow: bool) -> capture::StillImage {
    let font = FontRef::try_from_slice(crate::theme::HANKEN).expect("bundled font is valid");
    let px = size.max(4) as f32;
    let scaled = font.as_scaled(PxScale::from(px));
    let line_h = scaled.height() + scaled.line_gap();
    let lines: Vec<&str> = if text.is_empty() { vec![" "] } else { text.lines().collect() };
    // Each line's glyphs, laid out from its left.
    let mut laid = Vec::new();
    let mut widest: f32 = 1.0;
    for (i, line) in lines.iter().enumerate() {
        let mut x = 0.0;
        let mut prev = None;
        let mut glyphs = Vec::new();
        for c in line.chars() {
            let id = font.glyph_id(c);
            if let Some(p) = prev {
                x += scaled.kern(p, id);
            }
            glyphs.push(id.with_scale_and_position(px, point(x, scaled.ascent() + i as f32 * line_h)));
            x += scaled.h_advance(id);
            prev = Some(id);
        }
        widest = widest.max(x);
        laid.push((glyphs, x));
    }
    let pad = (px * 0.15).ceil();
    let offset = if shadow { (px / 18.0).max(1.0) } else { 0.0 };
    let blur = if shadow { (px / 24.0).round().max(1.0) as usize } else { 0 };
    let width = (widest + pad * 2.0 + offset + blur as f32 * 2.0).ceil() as u32;
    let height = (line_h * lines.len() as f32 + pad * 2.0 + offset + blur as f32 * 2.0).ceil() as u32;
    let (w, h) = (width as usize, height as usize);
    // Coverage of the letters, and (offset) of their shadow.
    let mut ink = vec![0f32; w * h];
    let mut shade = vec![0f32; w * h];
    let origin = pad + blur as f32;
    for (glyphs, line_w) in &laid {
        // Each line in the middle of the widest.
        let left = origin + (widest - line_w) / 2.0;
        for glyph in glyphs {
            let Some(outline) = font.outline_glyph(glyph.clone()) else { continue };
            let b = outline.px_bounds();
            outline.draw(|gx, gy, coverage| {
                let x = (left + b.min.x) as i64 + gx as i64;
                let y = (origin + b.min.y) as i64 + gy as i64;
                let put = |buf: &mut Vec<f32>, x: i64, y: i64| {
                    if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
                        let at = y as usize * w + x as usize;
                        buf[at] = (buf[at] + coverage).min(1.0);
                    }
                };
                put(&mut ink, x, y);
                if shadow {
                    put(&mut shade, x + offset as i64, y + offset as i64);
                }
            });
        }
    }
    if blur > 0 {
        box_blur(&mut shade, w, h, blur);
        box_blur(&mut shade, w, h, blur);
    }
    let [r, g, b, a] = rgba;
    let alpha = a as f32 / 255.0;
    let mut bgra = vec![0u8; w * h * 4];
    for i in 0..w * h {
        // The letters over their shadow (black, 60%), as straight alpha.
        let (sa, ta) = (shade[i] * 0.6 * alpha, ink[i] * alpha);
        let out = ta + sa * (1.0 - ta);
        if out <= 0.0 {
            continue;
        }
        let mix = |c: u8| ((c as f32 * ta) / out).round().clamp(0.0, 255.0) as u8;
        bgra[i * 4..i * 4 + 4].copy_from_slice(&[mix(b), mix(g), mix(r), (out * 255.0).round() as u8]);
    }
    capture::StillImage { width, height, bgra }
}

/// A box blur of `radius` across and then down.
fn box_blur(buf: &mut [f32], w: usize, h: usize, radius: usize) {
    let mut tmp = vec![0f32; buf.len()];
    let n = (radius * 2 + 1) as f32;
    for y in 0..h {
        for x in 0..w {
            let (a, b) = (x.saturating_sub(radius), (x + radius).min(w - 1));
            tmp[y * w + x] = buf[y * w + a..=y * w + b].iter().sum::<f32>() / n;
        }
    }
    for y in 0..h {
        for x in 0..w {
            let (a, b) = (y.saturating_sub(radius), (y + radius).min(h - 1));
            buf[y * w + x] = (a..=b).map(|yy| tmp[yy * w + x]).sum::<f32>() / n;
        }
    }
}

#[cfg(test)]
mod tests {
    /// Words come out just big enough, the letters in the colour asked
    /// for, around them nothing; a shadow is under them.
    #[test]
    fn words_drawn() {
        let p = super::render_text("Hi", 48, [255, 0, 0, 255], false);
        assert!(p.width > 20 && p.height > 40, "{}×{}", p.width, p.height);
        assert_eq!(p.bgra.len(), (p.width * p.height * 4) as usize);
        assert_eq!(&p.bgra[..4], &[0, 0, 0, 0]);
        assert!(p.bgra.chunks_exact(4).any(|px| px == [0, 0, 255, 255]));
        let s = super::render_text("Hi", 48, [255, 255, 255, 255], true);
        assert!(s.bgra.chunks_exact(4).any(|px| px[3] > 0 && px[3] < 200 && px[0] < 50), "a shadow shows");
        // Two lines: taller.
        let two = super::render_text("Hi\nthere", 48, [255, 255, 255, 255], false);
        assert!(two.height > p.height * 3 / 2);
    }
}
