//! Sources in a box of their own beside the screen (`settings::Visual`):
//! another display, one app's window, an image, a colour, words, a web
//! page. Each is placed, turned, flipped and cropped on the preview as the
//! screen is (`canvas_ui`). An image's picture and words drawn are made
//! here, again when they change, and handed to the capture as they are
//! (`capture::LayerSource::Picture`), which blends them by their transparency.

use std::sync::Arc;

use ab_glyph::{Font, FontRef, PxScale, ScaleFont, VariableFont, point};
use egui::{Pos2, Rect, vec2};

use crate::App;
use crate::canvas_ui::Layer;
use crate::settings::{BoxCfg, TextAlign, TextStyle, Visual, VisualCfg};

impl App {
    /// Each image's and words' picture, made again when what it shows
    /// changes (its file, its words, their look; words again once the
    /// computer's fonts are found). Every frame: unchanged, it's kept.
    pub(crate) fn refresh_visual_pictures(&mut self) {
        let wanted: Vec<(u64, Visual)> = self.settings.visuals.iter().filter(|v| matches!(v.visual, Visual::Image { .. } | Visual::Text(_))).map(|v| (v.key, v.visual.clone())).collect();
        self.canvas.pictures.retain(|k, _| wanted.iter().any(|(w, _)| w == k));
        // Words in a font that wasn't found yet are drawn again once it is.
        let fonts = crate::fonts::families().is_some();
        if fonts != self.canvas.fonts_found {
            self.canvas.fonts_found = fonts;
            self.canvas.pictures.retain(|_, (v, _)| !matches!(v, Visual::Text(s) if !s.font.is_empty()));
        }
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
                Visual::Text(style) => Some(Arc::new(render_text(style))),
                _ => None,
            };
            let before = self.visual_picture(key).map(|p| (p.width, p.height));
            self.canvas.pictures.insert(key, (visual.clone(), picture.clone()));
            if let (Visual::Text(_), Some(p)) = (&visual, picture) {
                self.fit_words_box(key, before, (p.width, p.height));
            }
        }
        // Words not placed yet (the video's shape wasn't known): now.
        let unplaced: Vec<u64> = self.settings.visuals.iter().filter(|v| v.placed.is_none() && matches!(v.visual, Visual::Text(_))).map(|v| v.key).collect();
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

    /// A boxed source's picture's own size (all of it, not turned), when
    /// it's known: a display's or window's asked for now and then. A colour
    /// has none.
    pub(crate) fn visual_own_size(&mut self, key: u64) -> Option<(u32, u32)> {
        let v = self.settings.visual(key)?.clone();
        let asked = |this: &mut Self, ask: &dyn Fn() -> Option<(u32, u32)>| {
            let fresh = this.canvas.sizes.get(&key).is_some_and(|(_, at)| at.elapsed() < std::time::Duration::from_secs(2));
            if !fresh {
                this.canvas.sizes.insert(key, (ask(), std::time::Instant::now()));
            }
            this.canvas.sizes.get(&key).and_then(|(s, _)| *s)
        };
        match &v.visual {
            Visual::Display { id, .. } => {
                let source = capture::VideoSource::Screen { id: id.clone(), hook: Default::default(), method: Default::default() };
                asked(self, &|| capture::frame_size(&source, Default::default()))
            }
            Visual::Window { app, .. } => asked(self, &|| capture::layers::app_window_size(app)),
            Visual::Image { .. } | Visual::Text(_) => self.visual_picture(key).map(|p| (p.width, p.height)),
            Visual::Browser { width, height, .. } => Some((*width, *height)),
            Visual::Colour { .. } => None,
        }
    }

    /// A boxed source's picture's size as it shows: cropped, then turned.
    pub(crate) fn visual_size(&mut self, key: u64) -> Option<(u32, u32)> {
        let own = self.visual_own_size(key)?;
        let v = self.settings.visual(key)?;
        let crop = v.crop.filter(|c| !c.is_none() && v.visual.croppable()).map(Into::into);
        let (w, h) = capture::cropped_size(crop, own);
        Some(if v.turns % 2 == 1 { (h, w) } else { (w, h) })
    }

    /// A boxed source's crop as it is (left, top, width, height in its own
    /// pixels; all of it when it isn't cropped), and its picture's size:
    /// `None` for what can't be cropped, and until the size is known.
    pub(crate) fn visual_crop_rect(&mut self, key: u64) -> Option<([u32; 4], (u32, u32))> {
        let crop = self.settings.visual(key).filter(|v| v.visual.croppable())?.crop;
        let (w, h) = self.visual_own_size(key)?;
        let rect = crop.filter(|c| !c.is_none()).map_or([0, 0, w & !1, h & !1], |c| capture::Crop::from(c).rect(w, h));
        Some((rect, (w, h)))
    }

    /// [`visual_crop_rect`](Self::visual_crop_rect) as the picture shows,
    /// turned and flipped: the part kept, and the picture's size.
    pub(crate) fn visual_crop_shown(&mut self, key: u64) -> Option<(Rect, (u32, u32))> {
        let ([x, y, w, h], own) = self.visual_crop_rect(key)?;
        let v = self.settings.visual(key)?;
        let turned = crate::canvas_ui::Turned::new(v.turns, v.flip_h, v.flip_v);
        let part = Rect::from_min_size(Pos2::new(x as f32, y as f32), vec2(w as f32, h as f32));
        Some((turned.part(part, own, false), turned.size(own)))
    }

    /// A boxed source as it's taken hold of to crop it: its box (the part in
    /// the video), the part of its picture showing there, and the picture's
    /// size as it shows (see `screen_crop_hold`).
    pub(crate) fn visual_crop_hold(&mut self, key: u64, aspect: f32) -> Option<(Rect, Rect, (u32, u32))> {
        let (part, size) = self.visual_crop_shown(key)?;
        let (shown, part) = crate::canvas_ui::in_video(self.visual_rect(key, aspect), part);
        Some((shown, part, size))
    }

    /// All of a boxed source's picture again: its box takes the whole
    /// picture's shape, its width and place kept.
    pub(crate) fn visual_uncrop(&mut self, key: u64, aspect: f32) {
        if let Some(v) = self.settings.visual_mut(key) {
            v.crop = None;
        }
        let picture = self.visual_size(key).map(|(w, h)| w as f32 / h.max(1) as f32);
        if let Some(b) = self.settings.visual_mut(key).and_then(|v| v.placed.as_mut()) {
            b.refit(aspect, picture);
        }
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
        let text = self.settings.visual(key).is_some_and(|v| matches!(v.visual, Visual::Text(_)));
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
            Visual::Text(s) => s.text.lines().next().unwrap_or_default().to_owned(),
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

}

/// A colour's swatch, its picker under it (with transparency): the new
/// colour, once one's picked.
pub(crate) fn colour_button(ui: &mut egui::Ui, rgba: [u8; 4]) -> Option<[u8; 4]> {
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

/// Words drawn as `style` says, on nothing (transparent): a line for each
/// line, lined up as it says, its letters in its font and colour, an outline
/// round them and a shadow under them if it has them. Their picture, just
/// big enough for all of it.
pub(crate) fn render_text(style: &TextStyle) -> capture::StillImage {
    let loaded = crate::fonts::load(&style.font, style.weight, style.italic);
    let font = FontRef::try_from_slice_and_index(&loaded.bytes, loaded.index)
        .or_else(|_| FontRef::try_from_slice(crate::theme::HANKEN))
        .expect("the bundled font is valid");
    let mut font = font;
    if let Some(w) = loaded.variation {
        font.set_variation(b"wght", w);
    }
    let px = style.size.clamp(4, 600) as f32;
    let scaled = font.as_scaled(PxScale::from(px));
    let line_h = (scaled.height() + scaled.line_gap()) * style.line_spacing.clamp(50, 300) as f32 / 100.0;
    let lines: Vec<&str> = if style.text.is_empty() { vec![" "] } else { style.text.lines().collect() };
    // Each line's glyphs, laid out from its left, and how wide it is.
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
    // Room round the letters for the outline, and the shadow's offset and blur.
    let outline = if style.outline { style.outline_width.clamp(1, 60) as f32 } else { 0.0 };
    let (sx, sy) = if style.shadow { (style.shadow_x.clamp(-200, 200) as f32, style.shadow_y.clamp(-200, 200) as f32) } else { (0.0, 0.0) };
    let blur = if style.shadow { style.shadow_blur.min(100) as usize } else { 0 };
    let pad = (px * 0.12).ceil() + outline + blur as f32;
    let (left_room, top_room) = (pad + (-sx).max(0.0), pad + (-sy).max(0.0));
    let width = (left_room + widest + pad + sx.max(0.0)).ceil() as u32;
    let height = (top_room + line_h * (lines.len() - 1) as f32 + scaled.height() + pad + sy.max(0.0)).ceil() as u32;
    let (w, h) = (width.max(1) as usize, height.max(1) as usize);
    // How much of each pixel the letters cover (`ink`), drawn again round
    // them for the outline (`edge`).
    let mut ink = vec![0f32; w * h];
    let mut edge = vec![0f32; w * h];
    let rings: Vec<(f32, f32)> = if outline > 0.0 {
        // Each ring of the outline's circle, round enough at its size.
        let mut r = Vec::new();
        let mut radius = outline;
        while radius > 0.5 {
            let n = ((radius * std::f32::consts::TAU / 1.5).ceil() as usize).clamp(8, 96);
            r.extend((0..n).map(|k| {
                let a = k as f32 / n as f32 * std::f32::consts::TAU;
                (radius * a.cos(), radius * a.sin())
            }));
            radius -= 1.5;
        }
        r
    } else {
        Vec::new()
    };
    for (glyphs, line_w) in &laid {
        let left = left_room
            + match style.align {
                TextAlign::Left => 0.0,
                TextAlign::Centre => (widest - line_w) / 2.0,
                TextAlign::Right => widest - line_w,
            };
        for glyph in glyphs {
            let Some(o) = font.outline_glyph(glyph.clone()) else { continue };
            let b = o.px_bounds();
            let (ox, oy) = (left + b.min.x, top_room + b.min.y);
            let mut cover = Vec::new();
            o.draw(|gx, gy, c| cover.push((gx as i64, gy as i64, c.clamp(0.0, 1.0))));
            let put = |buf: &mut Vec<f32>, dx: f32, dy: f32, max: bool| {
                for &(gx, gy, c) in &cover {
                    let x = (ox + dx).round() as i64 + gx;
                    let y = (oy + dy).round() as i64 + gy;
                    if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
                        let at = y as usize * w + x as usize;
                        buf[at] = if max { buf[at].max(c) } else { (buf[at] + c).min(1.0) };
                    }
                }
            };
            put(&mut ink, 0.0, 0.0, false);
            for &(dx, dy) in &rings {
                put(&mut edge, dx, dy, true);
            }
        }
    }
    // The outline is round the letters and under them; the shadow is the
    // shape of both, moved and softened.
    for i in 0..w * h {
        edge[i] = edge[i].max(ink[i]);
    }
    let mut shade = Vec::new();
    if style.shadow {
        shade = vec![0f32; w * h];
        let shape = if outline > 0.0 { &edge } else { &ink };
        let (dx, dy) = (sx.round() as i64, sy.round() as i64);
        for y in 0..h as i64 {
            for x in 0..w as i64 {
                let (fx, fy) = (x - dx, y - dy);
                if fx >= 0 && fy >= 0 && (fx as usize) < w && (fy as usize) < h {
                    shade[y as usize * w + x as usize] = shape[fy as usize * w + fx as usize];
                }
            }
        }
        if blur > 0 {
            // Three box blurs of a third each: close to a Gaussian.
            let r = (blur / 3).max(1);
            for _ in 0..3 {
                box_blur(&mut shade, w, h, r);
            }
        }
    }
    // Shadow, then outline, then letters, each over the last (straight alpha).
    let layer = |rgba: [u8; 4]| ([rgba[0] as f32, rgba[1] as f32, rgba[2] as f32], rgba[3] as f32 / 255.0);
    let (fill, fill_a) = layer(style.rgba);
    let (line, line_a) = layer(style.outline_rgba);
    let (drop, drop_a) = layer(style.shadow_rgba);
    let mut bgra = vec![0u8; w * h * 4];
    for i in 0..w * h {
        let mut c = [0f32; 3];
        let mut a = 0f32;
        let mut over = |col: [f32; 3], alpha: f32| {
            if alpha <= 0.0 {
                return;
            }
            let out = alpha + a * (1.0 - alpha);
            for k in 0..3 {
                c[k] = (col[k] * alpha + c[k] * a * (1.0 - alpha)) / out;
            }
            a = out;
        };
        if let Some(s) = shade.get(i) {
            over(drop, s * drop_a);
        }
        if outline > 0.0 {
            over(line, edge[i] * line_a);
        }
        over(fill, ink[i] * fill_a);
        if a > 0.0 {
            bgra[i * 4..i * 4 + 4].copy_from_slice(&[c[2].round() as u8, c[1].round() as u8, c[0].round() as u8, (a * 255.0).round() as u8]);
        }
    }
    capture::StillImage { width, height, bgra }
}
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
    use crate::settings::{TextAlign, TextStyle};

    fn words(text: &str) -> TextStyle {
        TextStyle { text: text.into(), size: 48, rgba: [255, 0, 0, 255], shadow: false, ..TextStyle::default() }
    }

    /// Words come out just big enough, the letters in the colour asked
    /// for, around them nothing.
    #[test]
    fn words_drawn() {
        let p = super::render_text(&words("Hi"));
        assert!(p.width > 20 && p.height > 40, "{}×{}", p.width, p.height);
        assert_eq!(p.bgra.len(), (p.width * p.height * 4) as usize);
        assert_eq!(&p.bgra[..4], &[0, 0, 0, 0]);
        assert!(p.bgra.chunks_exact(4).any(|px| px == [0, 0, 255, 255]));
        // Two lines: taller.
        let two = super::render_text(&words("Hi\nthere"));
        assert!(two.height > p.height * 3 / 2);
    }

    /// A shadow is under the letters, its own colour, softened; an outline
    /// is round them, and makes them take more room.
    #[test]
    fn shadow_and_outline() {
        let plain = super::render_text(&words("Hi"));
        let shadowed = super::render_text(&TextStyle { shadow: true, shadow_rgba: [0, 0, 255, 255], shadow_blur: 6, ..words("Hi") });
        // Blue shadow pixels (B, G, R, A), some half-covered by its blur.
        assert!(shadowed.bgra.chunks_exact(4).any(|px| px[0] > 200 && px[2] < 50 && px[3] > 0 && px[3] < 255), "a soft blue shadow shows");
        let outlined = super::render_text(&TextStyle { outline: true, outline_width: 6, outline_rgba: [0, 255, 0, 255], ..words("Hi") });
        assert!(outlined.width > plain.width && outlined.height > plain.height);
        assert!(outlined.bgra.chunks_exact(4).any(|px| px == [0, 255, 0, 255]), "a green outline shows");
    }

    /// Lines line up as asked: a short line under a long one starts at the
    /// left, sits in the middle, or ends at the right.
    #[test]
    fn lines_line_up() {
        // The first column of pixels with ink in the second line's rows.
        let first_ink = |align: TextAlign| {
            let p = super::render_text(&TextStyle { align, ..words("Wwwwwwwwww\ni") });
            let (w, h) = (p.width as usize, p.height as usize);
            (0..w).find(|&x| (h * 2 / 3..h).any(|y| p.bgra[(y * w + x) * 4 + 3] > 128)).unwrap_or(w)
        };
        let (l, c, r) = (first_ink(TextAlign::Left), first_ink(TextAlign::Centre), first_ink(TextAlign::Right));
        assert!(l < c && c < r, "{l} {c} {r}");
    }
}
