//! The library panel's glass: behind its card is the clip grid itself, at its
//! real place, blurred very wide, under a light tint. The card catches the
//! light spilling over from the clips beside it, like a macOS sidebar.
//!
//! Made on the CPU, once, whenever the clips on screen or the layout change:
//! the clips' tiny pictures are painted at 1/8 scale on transparent, blurred
//! (three box passes per axis, close to a Gaussian), and only the light is
//! saturated and laid over the card's own colour (saturating the card's
//! slightly blue grey turned it visibly blue). The result is rounded to 8 bits
//! once, with an ordered threshold, so the gradient has no bands. Scrolling
//! only moves it: it's drawn offset inside the card, never made again per
//! frame.

use std::path::PathBuf;

use egui::{Color32, Pos2, Rect, vec2};

use crate::App;
use crate::theme;

/// The picture is made at this fraction of the grid's size.
const SCALE: f32 = 1.0 / 8.0;
/// Blur radius in points (the user asked for 150 after 100).
const RADIUS: f32 = 150.0;
/// The light is this much more saturated.
const SATURATE: f32 = 2.6;
/// Clips this far into the grid (from its left edge) light the card.
pub const REACH: f32 = RADIUS * 1.5;

#[derive(Default)]
pub struct Glass {
    texture: Option<egui::TextureHandle>,
    /// What it was made from: the clips and their places, the card, the look.
    signature: u64,
    /// Where its top-left is in grid coordinates (content, before scroll),
    /// and its size in points.
    origin: Pos2,
    size: egui::Vec2,
    /// The first clip's place when it was made (to stretch it with the
    /// clips while the window resizes), and the window's size and when it
    /// last changed.
    made_for: Option<(PathBuf, Rect)>,
    window: egui::Vec2,
    resized_at: f64,
}

/// A clip on the grid, for the glass: its picture's place in the grid's
/// content (scrolling not applied, where it's going if it's moving) and its
/// tiny picture.
pub struct Tile {
    pub rect: Rect,
    pub path: PathBuf,
    pub small: Option<std::sync::Arc<(usize, usize, Vec<u8>)>>,
}

/// Paint the card with its glass. `origin`: where the grid's content
/// top-left is on screen now. Call where the card is drawn, after the grid.
pub fn paint(ui: &mut egui::Ui, card: Rect, app: &mut App) {
    let p = theme::pal(ui);
    let painter = ui.painter_at(card);
    painter.rect_filled(card, 8, p.raise);
    let origin = app.glass_origin;
    let tiles = std::mem::take(&mut app.glass_tiles);
    // Only clips within a blur radius of the card light it: the grid's first
    // column or two.
    let near: Vec<&Tile> = tiles.iter().filter(|t| t.rect.left() < REACH).collect();
    use std::hash::{Hash, Hasher};
    let mut sig = std::collections::hash_map::DefaultHasher::new();
    p.dark.hash(&mut sig);
    ((card.width() as i32), (card.height() as i32), ((card.left() - origin.x) as i32)).hash(&mut sig);
    for t in &near {
        t.path.hash(&mut sig);
        (t.rect.min.x as i32, t.rect.min.y as i32, t.rect.width() as i32).hash(&mut sig);
        t.small.is_some().hash(&mut sig);
    }
    let sig = sig.finish();
    // While the window is being resized (its size changed in the last few
    // frames), the picture isn't made again every frame: it would cost a
    // frame's time each time (the resize stuttered). The last one stays,
    // stretched to where the clips are now; it's made again once the size
    // settles. Blurred this much, the stretch can't be seen.
    let now = ui.input(|i| i.time);
    let size = ui.ctx().content_rect().size();
    if app.glass.window != size {
        app.glass.window = size;
        app.glass.resized_at = now;
    }
    let resizing = now - app.glass.resized_at < 0.15;
    if resizing {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(160));
    }
    if sig != app.glass.signature && !(resizing && app.glass.texture.is_some()) {
        app.glass.signature = sig;
        if near.is_empty() {
            app.glass.texture = None;
        } else {
            make(ui.ctx(), &mut app.glass, &near, card.left() - origin.x, p);
            app.glass.made_for = near.first().map(|t| (t.path.clone(), t.rect));
        }
    }
    // Stretched with the clips while it's waiting to be made again: one
    // clip's place now against when it was made (the same clip, found by
    // its file).
    let stretch = app.glass.made_for.as_ref().and_then(|(path, was)| {
        let is = tiles.iter().find(|t| &t.path == path)?.rect;
        (is != *was && was.width() > 0.0).then_some((*was, is))
    });
    app.glass_tiles = tiles;
    if let Some(tex) = &app.glass.texture {
        let mut r = Rect::from_min_size(app.glass.origin, app.glass.size);
        if let Some((was, is)) = stretch {
            // Scaled about the first clip: its column widths and rows grow
            // with the window, and so does the picture.
            let k = is.width() / was.width();
            r = Rect::from_min_size(is.min + (r.min - was.min) * k, r.size() * k);
        }
        let r = r.translate(origin.to_vec2());
        painter.add(egui::Shape::mesh(rounded_mesh(tex.id(), card, 8.0, r)));
    }
    let tint = if p.dark { Color32::from_rgba_unmultiplied(24, 25, 29, 56) } else { Color32::from_rgba_unmultiplied(250, 250, 251, 92) };
    painter.rect_filled(card, 8, tint);
    painter.rect_stroke(card, 8, egui::Stroke::new(1.0, if p.dark { Color32::from_white_alpha(13) } else { Color32::from_black_alpha(15) }), egui::StrokeKind::Inside);
}

/// The picture laid over `r` (screen points), showing only where it's
/// inside `card` with its corners rounded by `radius`: a fan of triangles
/// over that outline (egui clips to rectangles only, so the picture's own
/// corners would show past the card's rounded ones).
fn rounded_mesh(texture: egui::TextureId, card: Rect, radius: f32, r: Rect) -> egui::Mesh {
    let radius = radius.min(card.width() / 2.0).min(card.height() / 2.0);
    let mut outline = Vec::new();
    // Each corner's arc, clockwise from the top left.
    let corners = [
        (Pos2::new(card.left() + radius, card.top() + radius), 180.0f32),
        (Pos2::new(card.right() - radius, card.top() + radius), 270.0),
        (Pos2::new(card.right() - radius, card.bottom() - radius), 0.0),
        (Pos2::new(card.left() + radius, card.bottom() - radius), 90.0),
    ];
    const STEPS: usize = 6;
    for (c, start) in corners {
        for k in 0..=STEPS {
            let a = (start + 90.0 * k as f32 / STEPS as f32).to_radians();
            outline.push(c + egui::vec2(a.cos(), a.sin()) * radius);
        }
    }
    // Only where the picture is (it spans the grid, which may be shorter).
    let outline = clip_to(outline, r);
    let mut mesh = egui::Mesh::with_texture(texture);
    if outline.len() < 3 {
        return mesh;
    }
    let uv = |p: Pos2| Pos2::new((p.x - r.left()) / r.width().max(1.0), (p.y - r.top()) / r.height().max(1.0));
    let centre = outline.iter().fold(Pos2::ZERO, |a, p| a + p.to_vec2()) / outline.len() as f32;
    mesh.vertices.push(egui::epaint::Vertex { pos: centre, uv: uv(centre), color: Color32::WHITE });
    for p in &outline {
        mesh.vertices.push(egui::epaint::Vertex { pos: *p, uv: uv(*p), color: Color32::WHITE });
    }
    let n = outline.len() as u32;
    for i in 0..n {
        mesh.add_triangle(0, 1 + i, 1 + (i + 1) % n);
    }
    mesh
}

/// A convex outline cut to the part inside `r` (Sutherland–Hodgman).
fn clip_to(mut pts: Vec<Pos2>, r: Rect) -> Vec<Pos2> {
    // Each edge of `r`: is a point inside it, and where a line crosses it.
    type Inside = fn(Pos2, Rect) -> bool;
    let edges: [(Inside, usize, fn(Rect) -> f32); 4] = [
        (|p, r| p.x >= r.left(), 0, |r| r.left()),
        (|p, r| p.x <= r.right(), 0, |r| r.right()),
        (|p, r| p.y >= r.top(), 1, |r| r.top()),
        (|p, r| p.y <= r.bottom(), 1, |r| r.bottom()),
    ];
    for (inside, axis, at) in edges {
        if pts.is_empty() {
            break;
        }
        let v = at(r);
        let cross = |a: Pos2, b: Pos2| {
            let (pa, pb) = if axis == 0 { (a.x, b.x) } else { (a.y, b.y) };
            let t = (v - pa) / (pb - pa);
            a + (b - a) * t
        };
        let mut out = Vec::with_capacity(pts.len() + 2);
        for i in 0..pts.len() {
            let (a, b) = (pts[i], pts[(i + 1) % pts.len()]);
            match (inside(a, r), inside(b, r)) {
                (true, true) => out.push(b),
                (true, false) => out.push(cross(a, b)),
                (false, true) => {
                    out.push(cross(a, b));
                    out.push(b);
                }
                (false, false) => {}
            }
        }
        pts = out;
    }
    pts
}

/// Make the glass picture: the grid's clips near the card, blurred, one
/// picture the height of the grid (scrolling only moves it). `left`: the
/// card's left edge in grid content coordinates.
fn make(ctx: &egui::Context, glass: &mut Glass, tiles: &[&Tile], left: f32, p: &theme::Pal) {
    let top = tiles.iter().map(|t| t.rect.top()).fold(f32::INFINITY, f32::min).min(0.0) - RADIUS;
    let bottom = tiles.iter().map(|t| t.rect.bottom()).fold(0.0, f32::max) + RADIUS;
    // Very long libraries: no taller than a GPU texture allows.
    let bottom = bottom.min(top + 8000.0 / SCALE);
    let right = REACH;
    let area = Rect::from_min_max(Pos2::new(left, top), Pos2::new(right.max(left + 1.0), bottom));
    let w = ((area.width() * SCALE).ceil() as usize).max(2);
    let h = ((area.height() * SCALE).ceil() as usize).max(2);
    // Premultiplied light, linear, f32.
    let mut buf = vec![[0f32; 4]; w * h];
    let lin = |v: u8| (v as f32 / 255.0).powf(2.2);
    for t in tiles {
        let Some(small) = &t.small else { continue };
        let (sw, sh, px) = (small.0, small.1, &small.2);
        let x0 = ((t.rect.left() - area.left()) * SCALE).floor() as i32;
        let y0 = ((t.rect.top() - area.top()) * SCALE).floor() as i32;
        let tw = ((t.rect.width() * SCALE).round() as i32).max(1);
        let th = ((t.rect.height() * SCALE).round() as i32).max(1);
        for y in 0..th {
            let yy = y0 + y;
            if yy < 0 || yy >= h as i32 {
                continue;
            }
            let sy = (y as usize * sh / th as usize).min(sh - 1);
            for x in 0..tw {
                let xx = x0 + x;
                if xx < 0 || xx >= w as i32 {
                    continue;
                }
                let sx = (x as usize * sw / tw as usize).min(sw - 1);
                let o = (sy * sw + sx) * 4;
                buf[yy as usize * w + xx as usize] = [lin(px[o]), lin(px[o + 1]), lin(px[o + 2]), 1.0];
            }
        }
    }
    // Blur: three box passes per axis (σ = radius / 2).
    let r = ((RADIUS * SCALE) / 2.0).round().max(1.0) as usize;
    for _ in 0..3 {
        box_blur(&mut buf, w, h, r, true);
        box_blur(&mut buf, w, h, r, false);
    }
    // Composite: only the light saturated, over the card's own colour.
    let base = p.raise;
    let card_lin = [lin(base.r()), lin(base.g()), lin(base.b())];
    let strength = if p.dark { 0.9 } else { 0.55 };
    let mut img = egui::ColorImage::new([w, h], vec![Color32::TRANSPARENT; w * h]);
    for y in 0..h {
        for x in 0..w {
            let [r, g, b, a] = buf[y * w + x];
            let (cr, cg, cb) = if a > 1e-4 { (r / a, g / a, b / a) } else { (0.0, 0.0, 0.0) };
            let l = 0.2126 * cr + 0.7152 * cg + 0.0722 * cb;
            let sat = |c: f32| (l + (c - l) * SATURATE).max(0.0);
            let k = a * strength;
            let out = [card_lin[0] * (1.0 - k) + sat(cr) * k, card_lin[1] * (1.0 - k) + sat(cg) * k, card_lin[2] * (1.0 - k) + sat(cb) * k];
            // Rounded to 8 bits once, with an ordered threshold: no bands.
            let n = (52.982_918 * ((0.067_110_56 * x as f32 + 0.005_837_15 * y as f32).fract())).fract() - 0.5;
            let enc = |v: f32| ((v.clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0) + n).round().clamp(0.0, 255.0) as u8;
            img.pixels[y * w + x] = Color32::from_rgb(enc(out[0]), enc(out[1]), enc(out[2]));
        }
    }
    glass.texture = Some(ctx.load_texture("library_glass", img, egui::TextureOptions::LINEAR));
    glass.origin = area.min;
    glass.size = vec2(w as f32 / SCALE, h as f32 / SCALE);
}
/// One box blur pass of radius `r`, along rows (`horizontal`) or columns,
/// edges clamped.
pub(crate) fn box_blur(buf: &mut [[f32; 4]], w: usize, h: usize, r: usize, horizontal: bool) {
    let (n, lines) = if horizontal { (w, h) } else { (h, w) };
    let idx = |line: usize, i: usize| if horizontal { line * w + i } else { i * w + line };
    let mut tmp = vec![[0f32; 4]; n];
    let win = (2 * r + 1) as f32;
    for line in 0..lines {
        let mut acc = [0f32; 4];
        let at = |i: isize| -> usize { i.clamp(0, n as isize - 1) as usize };
        for i in -(r as isize)..=(r as isize) {
            let c = buf[idx(line, at(i))];
            for k in 0..4 {
                acc[k] += c[k];
            }
        }
        for i in 0..n {
            for k in 0..4 {
                tmp[i][k] = acc[k] / win;
            }
            let add = buf[idx(line, at(i as isize + r as isize + 1))];
            let sub = buf[idx(line, at(i as isize - r as isize))];
            for k in 0..4 {
                acc[k] += add[k] - sub[k];
            }
        }
        for i in 0..n {
            buf[idx(line, i)] = tmp[i];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How long making the glass takes (it's made again whenever the clips
    /// near the card move: every frame while the window is resized):
    /// `cargo test -p hesteclips glass_cost -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn glass_cost() {
        let ctx = egui::Context::default();
        let small = std::sync::Arc::new((40usize, 23usize, vec![128u8; 40 * 23 * 4]));
        // Two columns of clips near the card, down a long grid.
        let tiles: Vec<Tile> = (0..60).map(|i| Tile { rect: Rect::from_min_size(Pos2::new((i % 2) as f32 * 216.0, (i / 2) as f32 * 180.0), vec2(200.0, 112.0)), path: PathBuf::from(format!("/c{i}")), small: Some(small.clone()) }).collect();
        let near: Vec<&Tile> = tiles.iter().collect();
        let mut glass = Glass::default();
        let t = std::time::Instant::now();
        for _ in 0..10 {
            make(&ctx, &mut glass, &near, -260.0, &theme::DARK);
        }
        println!("glass, made again: {:.1} ms", t.elapsed().as_secs_f64() * 100.0);
    }
}
