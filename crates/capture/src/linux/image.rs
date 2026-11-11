//! Picture work on the CPU: putting the screen into the recording's frame,
//! drawing the other layers (webcams, pictures, …) under it and over it,
//! and converting to NV12 for the encoder (and to RGBA for the preview).
//!
//! Each step splits the picture into bands across the CPU's cores. At 1080p
//! that's a millisecond or two a frame; a 4K screen recorded at 1080p is
//! averaged down (every source pixel counts, so text stays legible rather
//! than shimmering).

use super::layers::{Pixels, Place, Shown};
use super::screen::{Frame, Order};
use crate::webcam::Placement;

/// A picture at the recording's size: B, G, R, A, rows packed.
pub(crate) struct Canvas {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
    /// A picture fitted inside the canvas, scaled before it's put in its place.
    part: Vec<u8>,
    /// Layers' pictures scaled to their places, kept while they stay (most
    /// don't move or change every frame, as the screen does).
    parts: Vec<(PartKey, Vec<u8>)>,
}

/// What a layer's scaled picture was made from: the layer, its picture,
/// the part of it used, its size scaled, and whether it's blended (and
/// with its colours premultiplied already).
type PartKey = (u64, u64, [u32; 4], (usize, usize), bool, bool);

/// Layers' scaled pictures kept at most.
const PARTS_KEPT: usize = 16;

impl Canvas {
    pub(crate) fn new(width: u32, height: u32) -> Self {
        Self { width, height, bgra: vec![0; width as usize * height as usize * 4], part: Vec::new(), parts: Vec::new() }
    }

    /// Draw a layer other than the screen where it goes: a webcam as
    /// [`Self::draw_camera`] does; anything else into its box as the screen
    /// is (on any pixel), blended by its alpha over what's there when it's
    /// blended, else covering it.
    pub(crate) fn draw_layer(&mut self, l: &Shown) {
        match (&l.place, &l.pixels) {
            (Place::Camera(p), Pixels::Camera(c)) => self.draw_camera(c, p),
            (Place::Camera(_), _) => {}
            (Place::Boxed(place), pixels) => {
                let Some((rows, stride, order)) = pixels.rows() else { return };
                let size = pixels.size();
                let ([sx, sy, sw, sh], [dx, dy, dw, dh]) = place.place_on(size, (self.width, self.height), false);
                if dw == 0 || dh == 0 || sw == 0 || sh == 0 || rows.len() < stride * (size.1 as usize - 1) + size.0 as usize * 4 {
                    return;
                }
                // Scaled its own way round, then put in its place turned and flipped.
                let (pw, ph) = if place.turns % 2 == 1 { (dh as usize, dw as usize) } else { (dw as usize, dh as usize) };
                let premultiplied = pixels.premultiplied();
                let key = (l.id, l.seq, [sx, sy, sw, sh], (pw, ph), l.blended, premultiplied);
                let at = match self.parts.iter().position(|(k, _)| *k == key) {
                    Some(at) => at,
                    None => {
                        let data = &rows[sy as usize * stride + sx as usize * 4..];
                        let blend = match (l.blended, premultiplied) {
                            (false, _) => Blend::Covers,
                            (true, false) => Blend::Alpha,
                            (true, true) => Blend::Premultiplied,
                        };
                        let part = scaled_part(data, (sw as usize, sh as usize), stride, order == Order::Rgb, (pw, ph), blend);
                        self.parts.retain(|(k, _)| k.0 != l.id);
                        if self.parts.len() >= PARTS_KEPT {
                            self.parts.remove(0);
                        }
                        self.parts.push((key, part));
                        self.parts.len() - 1
                    }
                };
                let part = &self.parts[at].1;
                let (dx, dy, dw, dh, w) = (dx as usize, dy as usize, dw as usize, dh as usize, self.width as usize);
                let turned = place.transformed();
                for y in 0..dh {
                    for x in 0..dw {
                        let from = if turned {
                            let (s, t) = place.unturn(((x as f32 + 0.5) / dw as f32, (y as f32 + 0.5) / dh as f32));
                            let (px, py) = (((s * pw as f32) as usize).min(pw - 1), ((t * ph as f32) as usize).min(ph - 1));
                            (py * pw + px) * 4
                        } else {
                            (y * pw + x) * 4
                        };
                        let src = &part[from..from + 4];
                        let to = &mut self.bgra[((dy + y) * w + dx + x) * 4..][..4];
                        if l.blended {
                            // Premultiplied: its colour, and what's under it where it's clear.
                            let a = src[3] as u32;
                            for k in 0..3 {
                                to[k] = (src[k] as u32 + (to[k] as u32 * (255 - a) + 127) / 255).min(255) as u8;
                            }
                        } else {
                            to[..3].copy_from_slice(&src[..3]);
                        }
                        to[3] = 255;
                    }
                }
            }
        }
    }

    /// Fill with a picture, put in as `framing` says (see `crate::placement`):
    /// its middle filling the canvas, all of it with black bars, or all of
    /// it stretched.
    #[cfg(test)]
    pub(crate) fn fill_from(&mut self, src: &Frame, framing: crate::Framing) {
        self.draw_screen(src, &crate::ScreenPlacement::whole(framing), false);
    }

    /// Draw a picture where `place` puts it (its crop, into its box as its
    /// framing says; nothing when hidden). `over` what's drawn already, the
    /// rest is left as it is; else the rest is black.
    pub(crate) fn draw_screen(&mut self, src: &Frame, place: &crate::ScreenPlacement, over: bool) {
        let ([sx, sy, sw, sh], [dx, dy, dw, dh]) = place.place((src.width, src.height), (self.width, self.height));
        if dw == 0 || dh == 0 || sw == 0 || sh == 0 {
            if !over {
                self.clear();
            }
            return;
        }
        let data = &src.data[sy as usize * src.stride + sx as usize * 4..];
        let (size, to) = ((sw as usize, sh as usize), (dw as usize, dh as usize));
        let swap = src.order == Order::Rgb;
        let turned = place.transformed();
        if !over && !turned && (dx, dy, dw, dh) == (0, 0, self.width, self.height) {
            resample(data, size, src.stride, swap, &mut self.bgra, to);
            return;
        }
        // Smaller than the canvas, or turned or flipped: scaled on its own
        // (its own way round), then put in its place, turned and flipped (on
        // black, unless it's over what's there).
        let (pw, ph) = if place.turns % 2 == 1 { (dh as usize, dw as usize) } else { (dw as usize, dh as usize) };
        self.part.resize(pw * ph * 4, 0);
        resample(data, size, src.stride, swap, &mut self.part, (pw, ph));
        if !over {
            self.clear();
        }
        let (dx, dy, dw, dh) = (dx as usize, dy as usize, dw as usize, dh as usize);
        if !turned {
            let row = dw * 4;
            for (y, line) in self.part.chunks_exact(row).enumerate() {
                let at = ((dy + y) * self.width as usize + dx) * 4;
                self.bgra[at..at + row].copy_from_slice(line);
            }
            return;
        }
        // Each pixel from the one it shows of the picture its own way round.
        for y in 0..dh {
            for x in 0..dw {
                let (s, t) = place.unturn(((x as f32 + 0.5) / dw as f32, (y as f32 + 0.5) / dh as f32));
                let (px, py) = (((s * pw as f32) as usize).min(pw - 1), ((t * ph as f32) as usize).min(ph - 1));
                let (from, at) = ((py * pw + px) * 4, ((dy + y) * self.width as usize + dx + x) * 4);
                self.bgra[at..at + 4].copy_from_slice(&self.part[from..from + 4]);
            }
        }
    }

    /// Fill with black.
    pub(crate) fn clear(&mut self) {
        for px in self.bgra.chunks_exact_mut(4) {
            px.copy_from_slice(&[0, 0, 0, 255]);
        }
    }

    /// Draw a camera picture (B, G, R, A rows) into its box, cropped and
    /// flipped as placed, filling the box without stretching.
    pub(crate) fn draw_camera(&mut self, cam: &CameraPicture, place: &Placement) {
        if place.is_hidden() || cam.width == 0 || cam.height == 0 {
            return;
        }
        let (fw, fh) = (self.width as f32, self.height as f32);
        // The picture as turned: its size, and where it is in the camera's.
        let (tw, th) = place.turned((cam.width, cam.height));
        let crop = place.fill_crop((tw, th), (self.width, self.height));
        let (x0, y0) = (place.x * fw, place.y * fh);
        let (bw, bh) = (place.w * fw, place.h * fh);
        // The part of the camera picture shown, in its pixels.
        let (cw, ch) = (tw as f32, th as f32);
        let size = (cam.width as f32, cam.height as f32);
        let (u0, u1) = (crop[0] * cw, (1.0 - crop[2]) * cw);
        let (v0, v1) = (crop[1] * ch, (1.0 - crop[3]) * ch);
        let left = x0.max(0.0).floor() as usize;
        let right = ((x0 + bw).min(fw).ceil() as usize).min(self.width as usize);
        let top = y0.max(0.0).floor() as usize;
        let bottom = ((y0 + bh).min(fh).ceil() as usize).min(self.height as usize);
        if left >= right || top >= bottom {
            return;
        }
        let w = self.width as usize;
        let row_bytes = w * 4;
        let target = &mut self.bgra[top * row_bytes..bottom * row_bytes];
        bands(target, row_bytes, |band_y, rows| {
            for (i, row) in rows.chunks_exact_mut(row_bytes).enumerate() {
                let y = (top + band_y + i) as f32 + 0.5;
                let mut fy = (y - y0) / bh;
                if !(0.0..1.0).contains(&fy) {
                    continue;
                }
                if place.flip_v {
                    fy = 1.0 - fy;
                }
                let v = v0 + fy * (v1 - v0);
                for x in left..right {
                    let mut fx = (x as f32 + 0.5 - x0) / bw;
                    if !(0.0..1.0).contains(&fx) {
                        continue;
                    }
                    if place.flip_h {
                        fx = 1.0 - fx;
                    }
                    let (sx, sy) = place.unturn((u0 + fx * (u1 - u0), v), size);
                    let px = cam.sample(sx - 0.5, sy - 0.5);
                    row[x * 4..x * 4 + 4].copy_from_slice(&px);
                }
            }
        });
    }

    /// The picture as NV12, studio range BT.709 (what the encoder is told it
    /// gets): a full-size Y plane, then a half-size plane of interleaved U, V.
    pub(crate) fn to_nv12(&self, out: &mut Vec<u8>) {
        let (w, h) = (self.width as usize, self.height as usize);
        out.resize(w * h * 3 / 2, 0);
        let (luma, chroma) = out.split_at_mut(w * h);
        // Bands of row pairs: each makes its two luma rows and one chroma row.
        let pairs: Vec<(&mut [u8], &mut [u8])> = luma.chunks_mut(w * 2).zip(chroma.chunks_mut(w)).collect();
        let threads = workers().min(pairs.len().max(1));
        let per = pairs.len().div_ceil(threads);
        let mut pairs = pairs;
        std::thread::scope(|scope| {
            let mut start = 0;
            while !pairs.is_empty() {
                let take = per.min(pairs.len());
                let band: Vec<_> = pairs.drain(..take).collect();
                let first = start;
                start += take;
                scope.spawn(move || {
                    for (k, (y2, uv)) in band.into_iter().enumerate() {
                        let y = (first + k) * 2;
                        let top = &self.bgra[y * w * 4..(y + 1) * w * 4];
                        let bottom = if y + 1 < h { &self.bgra[(y + 1) * w * 4..(y + 2) * w * 4] } else { top };
                        nv12_pair(top, bottom, y2, uv, w);
                    }
                });
            }
        });
    }

    /// The picture as R, G, B, A for the preview, shrunk to fit `max`
    /// (it's shown small: bigger only costs time to convert and upload).
    /// Returns its size.
    pub(crate) fn to_preview(&self, max: (u32, u32), out: &mut Vec<u8>) -> (u32, u32) {
        let scale = (max.0 as f64 / self.width as f64).min(max.1 as f64 / self.height as f64).min(1.0);
        let w = ((self.width as f64 * scale).round() as u32).max(1);
        let h = ((self.height as f64 * scale).round() as u32).max(1);
        out.resize(w as usize * h as usize * 4, 0);
        let size = (self.width as usize, self.height as usize);
        resample(&self.bgra, size, size.0 * 4, true, out, (w as usize, h as usize));
        (w, h)
    }
}

/// How a layer's picture goes over what's under it.
#[derive(Clone, Copy, PartialEq)]
enum Blend {
    /// Covering its box.
    Covers,
    /// Blended by its alpha.
    Alpha,
    /// Blended by its alpha, its colours premultiplied by it already (a web
    /// page's, as Chromium draws them).
    Premultiplied,
}

/// A picture (`size`, rows `stride` bytes apart, B, G, R, A, or R, G, B,
/// A with `swap`) scaled to `to`, each pixel the average of the area it
/// covers: opaque, or when blended its colour premultiplied by its alpha
/// and its alpha kept (each averaged on its own).
fn scaled_part(data: &[u8], size: (usize, usize), stride: usize, swap: bool, to: (usize, usize), blend: Blend) -> Vec<u8> {
    let mut out = vec![0u8; to.0 * to.1 * 4];
    if blend == Blend::Covers {
        resample(data, size, stride, swap, &mut out, to);
        return out;
    }
    let (w, h) = size;
    let mut colour = vec![0u8; w * h * 4];
    let mut alpha = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let p = &data[y * stride + x * 4..][..4];
            let a = p[3] as u32;
            let at = (y * w + x) * 4;
            if blend == Blend::Premultiplied {
                colour[at..at + 3].copy_from_slice(&p[..3]);
            } else {
                for k in 0..3 {
                    colour[at + k] = ((p[k] as u32 * a + 127) / 255) as u8;
                }
            }
            alpha[at..at + 3].fill(p[3]);
        }
    }
    let mut a = vec![0u8; to.0 * to.1 * 4];
    resample(&colour, size, w * 4, swap, &mut out, to);
    resample(&alpha, size, w * 4, false, &mut a, to);
    for (o, a) in out.chunks_exact_mut(4).zip(a.chunks_exact(4)) {
        o[3] = a[0];
    }
    out
}

/// A webcam picture: B, G, R, A rows, packed.
pub(crate) struct CameraPicture {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
    /// Which picture it is, a new number each (pictures' memory is used
    /// again, so where one is says nothing).
    pub seq: u64,
}

impl CameraPicture {
    /// The colour at a point (pixel centres at whole numbers), blended
    /// between the four nearest pixels.
    fn sample(&self, x: f32, y: f32) -> [u8; 4] {
        let (w, h) = (self.width as i32, self.height as i32);
        let (x, y) = (x.clamp(0.0, (w - 1) as f32), y.clamp(0.0, (h - 1) as f32));
        let (x0, y0) = (x as i32, y as i32);
        let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
        let (fx, fy) = (((x - x0 as f32) * 256.0) as u32, ((y - y0 as f32) * 256.0) as u32);
        let at = |x: i32, y: i32| (y as usize * self.width as usize + x as usize) * 4;
        let (a, b, c, d) = (at(x0, y0), at(x1, y0), at(x0, y1), at(x1, y1));
        let mut out = [255u8; 4];
        for (k, o) in out.iter_mut().enumerate().take(3) {
            let top = self.bgra[a + k] as u32 * (256 - fx) + self.bgra[b + k] as u32 * fx;
            let bottom = self.bgra[c + k] as u32 * (256 - fx) + self.bgra[d + k] as u32 * fx;
            *o = ((top * (256 - fy) + bottom * fy + 32768) >> 16) as u8;
        }
        out
    }
}

/// How many threads picture work splits across.
fn workers() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 8))
}

/// Run `f` over bands of whole rows (`row` bytes each) of `buf` in parallel,
/// with each band's first row number.
fn bands(buf: &mut [u8], row: usize, f: impl Fn(usize, &mut [u8]) + Sync) {
    let rows = buf.len() / row.max(1);
    if rows == 0 {
        return;
    }
    let threads = workers().min(rows);
    let per = rows.div_ceil(threads);
    std::thread::scope(|scope| {
        for (i, band) in buf.chunks_mut(per * row).enumerate() {
            let f = &f;
            scope.spawn(move || f(i * per, band));
        }
    });
}

/// Scale a picture of 4-byte pixels (`size`, rows `stride` bytes apart) into
/// `out` (`to`, rows packed), averaging the area each output pixel covers;
/// swapping the first and third bytes (red and blue) if asked. Alpha opaque.
pub(crate) fn resample(src: &[u8], size: (usize, usize), stride: usize, swap: bool, out: &mut [u8], to: (usize, usize)) {
    let ((sw, sh), (dw, dh)) = (size, to);
    if size == to {
        bands(out, dw * 4, |y0, rows| {
            for (i, row) in rows.chunks_exact_mut(dw * 4).enumerate() {
                let at = (y0 + i) * stride;
                copy_row(&src[at..at + dw * 4], row, swap);
            }
        });
        return;
    }
    let xs = Taps::new(sw, dw);
    let ys = Taps::new(sh, dh);
    bands(out, dw * 4, |y0, rows| {
        let n = rows.len() / (dw * 4);
        if n == 0 {
            return;
        }
        // Each source row this band covers, scaled across once (×256, which
        // still fits 16 bits), then the output rows weighted from those.
        let top = ys.first[y0];
        let bottom = ys.first[y0 + n - 1] + ys.n;
        let mut scaled = vec![0u16; (bottom - top) * dw * 3];
        for (k, line) in scaled.chunks_exact_mut(dw * 3).enumerate() {
            let at = (top + k) * stride;
            scale_row(&src[at..at + sw * 4], &xs, line);
        }
        let mut acc = vec![0u32; dw * 3];
        for (i, row) in rows.chunks_exact_mut(dw * 4).enumerate() {
            acc.iter_mut().for_each(|a| *a = 0);
            let (first, weights) = ys.of(y0 + i);
            for (k, &wy) in weights.iter().enumerate().filter(|&(_, &w)| w > 0) {
                let line = &scaled[(first + k - top) * dw * 3..][..dw * 3];
                for (a, &l) in acc.iter_mut().zip(line) {
                    *a += l as u32 * wy;
                }
            }
            // Each pass's weights add up to 256: two passes, 65536.
            for (px, a) in row.chunks_exact_mut(4).zip(acc.chunks_exact(3)) {
                let c = |v: u32| ((v + 32768) >> 16) as u8;
                let (b, g, r) = if swap { (c(a[2]), c(a[1]), c(a[0])) } else { (c(a[0]), c(a[1]), c(a[2])) };
                px.copy_from_slice(&[b, g, r, 255]);
            }
        }
    });
}

/// Copy a row of 4-byte pixels, swapping red and blue if asked; alpha opaque.
fn copy_row(src: &[u8], out: &mut [u8], swap: bool) {
    for (o, s) in out.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
        if swap {
            o.copy_from_slice(&[s[2], s[1], s[0], 255]);
        } else {
            o.copy_from_slice(&[s[0], s[1], s[2], 255]);
        }
    }
}

/// Area-average resampling weights, from `src` samples to `dst`: for each
/// output sample, the first source sample it covers and how much of each
/// (out of 256). Every output sample has the same number of weights (`n`,
/// the unused ones 0), so the loops over them are plain.
struct Taps {
    first: Vec<usize>,
    /// `n` weights for each output sample.
    weights: Vec<u32>,
    n: usize,
}

impl Taps {
    fn new(src: usize, dst: usize) -> Self {
        let scale = src as f64 / dst.max(1) as f64;
        let mut each = Vec::with_capacity(dst);
        for i in 0..dst {
            // The source interval this output sample covers (at least one
            // sample wide, so enlarging picks the nearest).
            let (mut a, mut b) = (i as f64 * scale, (i + 1) as f64 * scale);
            if b - a < 1.0 {
                let c = (a + b) / 2.0;
                a = (c - 0.5).max(0.0);
                b = (a + 1.0).min(src as f64);
                a = b - 1.0;
            }
            let lo = a.floor() as usize;
            let hi = (b.ceil() as usize).min(src).max(lo + 1);
            let raw: Vec<f64> = (lo..hi).map(|k| (b.min((k + 1) as f64) - a.max(k as f64)).max(0.0)).collect();
            let total: f64 = raw.iter().sum::<f64>().max(1e-9);
            let mut w: Vec<u32> = raw.iter().map(|r| (r / total * 256.0).round() as u32).collect();
            // Exactly 256, so flat colours stay exact.
            let sum: u32 = w.iter().sum();
            let biggest = (0..w.len()).max_by_key(|&k| w[k]).unwrap_or(0);
            w[biggest] = (w[biggest] + 256).saturating_sub(sum);
            each.push((lo, w));
        }
        let n = each.iter().map(|(_, w)| w.len()).max().unwrap_or(1).min(src.max(1));
        let mut first = Vec::with_capacity(dst);
        let mut weights = Vec::with_capacity(dst * n);
        for (lo, w) in each {
            // Padded to `n` with zeros, after it, or before it where that
            // would run past the last source sample.
            let start = lo.min(src - n);
            first.push(start);
            weights.extend(std::iter::repeat_n(0, lo - start));
            weights.extend(&w);
            weights.extend(std::iter::repeat_n(0, n - w.len() - (lo - start)));
        }
        Self { first, weights, n }
    }

    fn of(&self, i: usize) -> (usize, &[u32]) {
        (self.first[i], &self.weights[i * self.n..(i + 1) * self.n])
    }
}

/// One source row scaled to the output width: 3 channels a pixel, ×256.
fn scale_row(src: &[u8], xs: &Taps, out: &mut [u16]) {
    // The common tap counts get their own copy of the loop, unrolled.
    match xs.n {
        1 => scale_row_n::<1>(src, xs, out),
        2 => scale_row_n::<2>(src, xs, out),
        3 => scale_row_n::<3>(src, xs, out),
        4 => scale_row_n::<4>(src, xs, out),
        _ => scale_row_any(src, xs, out),
    }
}

fn scale_row_n<const N: usize>(src: &[u8], xs: &Taps, out: &mut [u16]) {
    for ((o, &first), weights) in out.chunks_exact_mut(3).zip(&xs.first).zip(xs.weights.chunks_exact(N)) {
        let px = &src[first * 4..][..N * 4];
        let (mut c0, mut c1, mut c2) = (0u32, 0u32, 0u32);
        for k in 0..N {
            let w = weights[k];
            c0 += px[k * 4] as u32 * w;
            c1 += px[k * 4 + 1] as u32 * w;
            c2 += px[k * 4 + 2] as u32 * w;
        }
        o.copy_from_slice(&[c0 as u16, c1 as u16, c2 as u16]);
    }
}

fn scale_row_any(src: &[u8], xs: &Taps, out: &mut [u16]) {
    let n = xs.n;
    for ((o, &first), weights) in out.chunks_exact_mut(3).zip(&xs.first).zip(xs.weights.chunks_exact(n)) {
        let px = &src[first * 4..(first + n) * 4];
        let (mut c0, mut c1, mut c2) = (0u32, 0u32, 0u32);
        for (p, &w) in px.chunks_exact(4).zip(weights) {
            c0 += p[0] as u32 * w;
            c1 += p[1] as u32 * w;
            c2 += p[2] as u32 * w;
        }
        o.copy_from_slice(&[c0 as u16, c1 as u16, c2 as u16]);
    }
}

/// Two BGRA rows to two Y rows and one interleaved UV row (BT.709, studio range).
fn nv12_pair(top: &[u8], bottom: &[u8], y2: &mut [u8], uv: &mut [u8], w: usize) {
    let (ytop, ybottom) = y2.split_at_mut(w.min(y2.len()));
    let luma = |p: &[u8]| ((47 * p[2] as u32 + 157 * p[1] as u32 + 16 * p[0] as u32 + 128) >> 8) as u8 + 16;
    for x in 0..w {
        ytop[x] = luma(&top[x * 4..]);
        if !ybottom.is_empty() {
            ybottom[x] = luma(&bottom[x * 4..]);
        }
    }
    for cx in 0..w / 2 {
        let (mut r, mut g, mut b) = (0i32, 0i32, 0i32);
        for row in [top, bottom] {
            for dx in 0..2 {
                let p = &row[(cx * 2 + dx) * 4..];
                b += p[0] as i32;
                g += p[1] as i32;
                r += p[2] as i32;
            }
        }
        // Sums of four pixels: the extra 4 (>> 10 instead of >> 8) averages.
        let u = 128 + ((-26 * r - 86 * g + 112 * b + 512) >> 10);
        let v = 128 + ((112 * r - 102 * g - 10 * b + 512) >> 10);
        uv[cx * 2] = u.clamp(16, 240) as u8;
        uv[cx * 2 + 1] = v.clamp(16, 240) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Layers go over one another in order on the CPU too: a half-clear
    /// picture over a webcam lets half of it through; the screen between
    /// the layers under it and over it; a colour over it only where it is;
    /// a picture turned a quarter lands turned.
    #[test]
    fn layers_over_one_another() {
        use std::sync::Arc;
        let (w, h) = (64u32, 36u32);
        let cam = CameraPicture { width: 16, height: 9, bgra: [200u8, 200, 200, 255].repeat(16 * 9), seq: 1 };
        let all = Placement { x: 0.0, y: 0.0, w: 1.0, h: 1.0, crop: [0.0; 4], flip_h: false, flip_v: false, turns: 0, stretch: 1.0 };
        let still = |v: u8, a: u8| Pixels::Still(Arc::new(crate::StillImage { width: 2, height: 2, bgra: [v, v, v, a].repeat(4) }));
        let boxed = |rect| crate::ScreenPlacement { rect, ..crate::ScreenPlacement::whole(crate::Framing::Stretch) };
        let black = Frame { width: 16, height: 9, stride: 64, order: Order::Bgr, data: [0u8, 0, 0, 255].repeat(16 * 9), texture: None };
        let mut canvas = Canvas::new(w, h);
        canvas.clear();
        canvas.draw_layer(&Shown { id: 1, seq: 1, pixels: Pixels::Camera(Arc::new(cam)), place: Place::Camera(all), blended: false });
        canvas.draw_layer(&Shown { id: 2, seq: 1, pixels: still(0, 128), place: Place::Boxed(boxed([0.0, 0.0, 0.25, 1.0])), blended: true });
        canvas.draw_screen(&black, &boxed([0.5, 0.0, 0.5, 1.0]), true);
        canvas.draw_layer(&Shown { id: 3, seq: 1, pixels: still(255, 255), place: Place::Boxed(boxed([0.75, 0.0, 0.25, 0.5])), blended: true });
        let at = |c: &Canvas, x: u32, y: u32| c.bgra[((y * w + x) * 4) as usize];
        let at_now = |x, y| at(&canvas, x, y);
        assert!(at_now(4, 18).abs_diff(100) <= 1, "half through: {}", at_now(4, 18));
        assert_eq!(at_now(24, 18), 200, "the camera alone");
        assert_eq!(at_now(40, 30), 0, "the screen over the camera");
        assert_eq!(at_now(60, 4), 255, "the colour over the screen");
        assert_eq!(at_now(60, 30), 0, "the screen where the colour isn't");
        // A 2×1 picture (left dark, right light) turned a quarter clockwise
        // into a box as tall as it's wide: dark on top, light below.
        let pair = Pixels::Still(Arc::new(crate::StillImage { width: 2, height: 1, bgra: vec![10, 10, 10, 255, 250, 250, 250, 255] }));
        let turned = crate::ScreenPlacement { turns: 1, ..boxed([0.0, 0.0, 0.5, 1.0]) };
        canvas.draw_layer(&Shown { id: 4, seq: 1, pixels: pair, place: Place::Boxed(turned), blended: false });
        let (top, bottom) = (at(&canvas, 16, 2), at(&canvas, 16, 33));
        assert!(top < 60 && bottom > 200, "turned: top {top}, bottom {bottom}");
    }

    fn frame(w: u32, h: u32, px: [u8; 4], order: Order) -> Frame {
        Frame { width: w, height: h, stride: w as usize * 4, order, data: px.repeat((w * h) as usize), texture: None }
    }

    #[test]
    fn flat_colours_survive_scaling() {
        for (sw, sh, dw, dh) in [(64, 36, 64, 36), (64, 36, 32, 18), (96, 54, 64, 36), (40, 30, 64, 48)] {
            let mut c = Canvas::new(dw, dh);
            c.fill_from(&frame(sw, sh, [10, 120, 250, 0], Order::Bgr), crate::Framing::Fit);
            assert!(c.bgra.chunks_exact(4).all(|p| p == [10, 120, 250, 255]), "{sw}x{sh} -> {dw}x{dh}");
            c.fill_from(&frame(sw, sh, [250, 120, 10, 0], Order::Rgb), crate::Framing::Fit);
            assert!(c.bgra.chunks_exact(4).all(|p| p == [10, 120, 250, 255]), "rgb {sw}x{sh} -> {dw}x{dh}");
        }
    }

    /// The screen in a box: on black, or over what's drawn (the webcam,
    /// when the screen's in front), which shows around it.
    #[test]
    fn the_screen_in_its_box() {
        let px = |c: &Canvas, x: u32, y: u32| {
            let at = ((y * c.width + x) * 4) as usize;
            c.bgra[at..at + 3].to_vec()
        };
        let white = frame(16, 8, [255, 255, 255, 0], Order::Bgr);
        let left = crate::ScreenPlacement { rect: [0.0, 0.0, 0.5, 1.0], ..crate::ScreenPlacement::whole(crate::Framing::Fill) };
        let mut c = Canvas::new(16, 8);
        c.draw_screen(&white, &left, false);
        assert_eq!((px(&c, 2, 4), px(&c, 12, 4)), (vec![255, 255, 255], vec![0, 0, 0]));
        let grey = CameraPicture { width: 4, height: 2, bgra: [40, 40, 40, 255].repeat(8), seq: 1 };
        let all = Placement { x: 0.0, y: 0.0, w: 1.0, h: 1.0, crop: [0.0; 4], flip_h: false, flip_v: false, turns: 0, stretch: 1.0 };
        c.clear();
        c.draw_camera(&grey, &all);
        c.draw_screen(&white, &left, true);
        assert_eq!((px(&c, 2, 4), px(&c, 12, 4)), (vec![255, 255, 255], vec![40, 40, 40]));
        // Hidden: nothing of it, over the webcam or on black.
        c.draw_screen(&white, &crate::ScreenPlacement { hidden: true, ..left }, false);
        assert!(c.bgra.chunks_exact(4).all(|p| p == [0, 0, 0, 255]));
    }

    /// Turned a quarter clockwise, the screen's left (white) is the top;
    /// mirrored, it's on the right.
    #[test]
    fn the_screen_turned_and_mirrored() {
        let px = |c: &Canvas, x: u32, y: u32| {
            let at = ((y * c.width + x) * 4) as usize;
            c.bgra[at..at + 3].to_vec()
        };
        let mut f = frame(8, 4, [0, 0, 0, 0], Order::Bgr);
        for y in 0..4 {
            for x in 0..4 {
                f.data[(y * 8 + x) * 4..(y * 8 + x) * 4 + 3].copy_from_slice(&[255, 255, 255]);
            }
        }
        let whole = crate::ScreenPlacement::whole(crate::Framing::Stretch);
        let mut c = Canvas::new(4, 8);
        c.draw_screen(&f, &crate::ScreenPlacement { turns: 1, ..whole }, false);
        assert_eq!((px(&c, 2, 1), px(&c, 2, 6)), (vec![255, 255, 255], vec![0, 0, 0]));
        let mut c = Canvas::new(8, 4);
        c.draw_screen(&f, &crate::ScreenPlacement { flip_h: true, ..whole }, false);
        assert_eq!((px(&c, 1, 2), px(&c, 6, 2)), (vec![0, 0, 0], vec![255, 255, 255]));
    }

    #[test]
    fn halving_averages() {
        // Alternating black and white columns average to grey.
        let mut f = frame(4, 2, [0, 0, 0, 0], Order::Bgr);
        for y in 0..2 {
            for x in [1usize, 3] {
                f.data[(y * 4 + x) * 4..(y * 4 + x) * 4 + 3].copy_from_slice(&[255, 255, 255]);
            }
        }
        let mut c = Canvas::new(2, 1);
        c.fill_from(&f, crate::Framing::Stretch);
        assert!(c.bgra.chunks_exact(4).all(|p| (127..=128).contains(&p[0])), "{:?}", c.bgra);
    }

    /// A wide picture into a square: its middle (Fill), all of it squeezed
    /// (Stretch), or all of it between black bars (Fit).
    #[test]
    fn framings() {
        // Three columns, red, green and blue (B, G, R), each 4 wide.
        let mut f = frame(12, 4, [0, 0, 0, 0], Order::Bgr);
        for y in 0..4 {
            for x in 0..12 {
                let px = [[0, 0, 255], [0, 255, 0], [255, 0, 0]][x / 4];
                f.data[(y * 12 + x) * 4..(y * 12 + x) * 4 + 3].copy_from_slice(&px);
            }
        }
        let px = |c: &Canvas, x: u32, y: u32| {
            let at = ((y * c.width + x) * 4) as usize;
            c.bgra[at..at + 3].to_vec()
        };
        let mut c = Canvas::new(4, 4);
        c.fill_from(&f, crate::Framing::Fill);
        assert!((0..4).all(|x| px(&c, x, 0) == [0, 255, 0]), "the green middle: {:?}", c.bgra);
        c.fill_from(&f, crate::Framing::Stretch);
        assert_eq!((px(&c, 0, 2), px(&c, 3, 2)), (vec![0, 0, 255], vec![255, 0, 0]));
        let mut c = Canvas::new(12, 12);
        c.fill_from(&f, crate::Framing::Fit);
        assert_eq!((px(&c, 0, 0), px(&c, 0, 6), px(&c, 11, 6)), (vec![0, 0, 0], vec![0, 0, 255], vec![255, 0, 0]));
    }

    #[test]
    fn nv12_of_known_colours() {
        let mut c = Canvas::new(4, 2);
        let mut out = Vec::new();
        for (bgr, y, u, v) in [([0, 0, 0], 16, 128, 128), ([255, 255, 255], 235, 128, 128), ([0, 0, 255], 63, 102, 240)] {
            for px in c.bgra.chunks_exact_mut(4) {
                px.copy_from_slice(&[bgr[0], bgr[1], bgr[2], 255]);
            }
            c.to_nv12(&mut out);
            assert_eq!(out.len(), 12);
            assert!(out[..8].iter().all(|&l| (l as i32 - y).abs() <= 1), "{bgr:?}: {out:?}");
            assert!((out[8] as i32 - u).abs() <= 1 && (out[9] as i32 - v).abs() <= 1, "{bgr:?}: {out:?}");
        }
    }

    #[test]
    fn camera_fills_its_box() {
        let mut c = Canvas::new(100, 50);
        c.clear();
        let cam = CameraPicture { width: 16, height: 9, bgra: [200, 100, 50, 255].repeat(16 * 9), seq: 1 };
        let place = Placement { x: 0.5, y: 0.5, w: 0.5, h: 0.5, crop: [0.0; 4], flip_h: true, flip_v: false, turns: 0, stretch: 1.0 };
        c.draw_camera(&cam, &place);
        let at = |x: usize, y: usize| &c.bgra[(y * 100 + x) * 4..(y * 100 + x) * 4 + 4];
        assert_eq!(at(75, 37), [200, 100, 50, 255]);
        assert_eq!(at(10, 10), [0, 0, 0, 255]);
    }
}
