//! Picture work on the CPU: fitting the screen into the recording's frame,
//! drawing the webcam over it, and converting to NV12 for the encoder (and to
//! RGBA for the preview).
//!
//! Each step splits the picture into bands across the CPU's cores. At 1080p
//! that's a millisecond or two a frame; a 4K screen recorded at 1080p is
//! averaged down (every source pixel counts, so text stays legible rather
//! than shimmering).

use super::screen::{Frame, Order};
use crate::webcam::Placement;

/// A picture at the recording's size: B, G, R, A, rows packed.
pub(crate) struct Canvas {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

impl Canvas {
    pub(crate) fn new(width: u32, height: u32) -> Self {
        Self { width, height, bgra: vec![0; width as usize * height as usize * 4] }
    }

    /// Fill with a still picture (the same layout), scaled to fit.
    pub(crate) fn fill_from(&mut self, src: &Frame) {
        let size = (src.width as usize, src.height as usize);
        let to = (self.width as usize, self.height as usize);
        resample(&src.data, size, src.stride, src.order == Order::Rgb, &mut self.bgra, to);
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
        let crop = place.fill_crop((cam.width, cam.height), (self.width, self.height));
        let (x0, y0) = (place.x * fw, place.y * fh);
        let (bw, bh) = (place.w * fw, place.h * fh);
        // The part of the camera picture shown, in its pixels.
        let (cw, ch) = (cam.width as f32, cam.height as f32);
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
                let sy = v0 + fy * (v1 - v0) - 0.5;
                for x in left..right {
                    let mut fx = (x as f32 + 0.5 - x0) / bw;
                    if !(0.0..1.0).contains(&fx) {
                        continue;
                    }
                    if place.flip_h {
                        fx = 1.0 - fx;
                    }
                    let sx = u0 + fx * (u1 - u0) - 0.5;
                    let px = cam.sample(sx, sy);
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

/// A webcam picture: B, G, R, A rows, packed.
pub(crate) struct CameraPicture {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
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
fn resample(src: &[u8], size: (usize, usize), stride: usize, swap: bool, out: &mut [u8], to: (usize, usize)) {
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
        // One horizontally scaled source row at a time, weighted into the
        // output row: each source row is scaled once per output row it
        // touches (once or twice when shrinking).
        let mut acc = vec![0u32; dw * 3];
        let mut line = vec![0u32; dw * 3];
        for (i, row) in rows.chunks_exact_mut(dw * 4).enumerate() {
            acc.iter_mut().for_each(|a| *a = 0);
            let (first, weights) = ys.of(y0 + i);
            for (k, &wy) in weights.iter().enumerate() {
                let at = (first + k) * stride;
                scale_row(&src[at..at + sw * 4], &xs, &mut line);
                for (a, l) in acc.iter_mut().zip(&line) {
                    *a += l * wy;
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
/// (out of 256).
struct Taps {
    first: Vec<usize>,
    /// Concatenated weights; `span[i]` is where output `i`'s start.
    weights: Vec<u32>,
    span: Vec<(usize, usize)>,
}

impl Taps {
    fn new(src: usize, dst: usize) -> Self {
        let scale = src as f64 / dst.max(1) as f64;
        let mut first = Vec::with_capacity(dst);
        let mut weights = Vec::new();
        let mut span = Vec::with_capacity(dst);
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
            first.push(lo);
            span.push((weights.len(), w.len()));
            weights.extend(w);
        }
        Self { first, weights, span }
    }

    fn of(&self, i: usize) -> (usize, &[u32]) {
        let (at, n) = self.span[i];
        (self.first[i], &self.weights[at..at + n])
    }
}

/// One source row scaled to the output width: 3 channels a pixel, ×256.
fn scale_row(src: &[u8], xs: &Taps, out: &mut [u32]) {
    for (i, o) in out.chunks_exact_mut(3).enumerate() {
        let (first, weights) = xs.of(i);
        let (mut c0, mut c1, mut c2) = (0u32, 0u32, 0u32);
        for (k, &w) in weights.iter().enumerate() {
            let p = &src[(first + k) * 4..];
            c0 += p[0] as u32 * w;
            c1 += p[1] as u32 * w;
            c2 += p[2] as u32 * w;
        }
        o.copy_from_slice(&[c0, c1, c2]);
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

    fn frame(w: u32, h: u32, px: [u8; 4], order: Order) -> Frame {
        Frame { width: w, height: h, stride: w as usize * 4, order, data: px.repeat((w * h) as usize) }
    }

    #[test]
    fn flat_colours_survive_scaling() {
        for (sw, sh, dw, dh) in [(64, 36, 64, 36), (64, 36, 32, 18), (96, 54, 64, 36), (40, 30, 64, 48)] {
            let mut c = Canvas::new(dw, dh);
            c.fill_from(&frame(sw, sh, [10, 120, 250, 0], Order::Bgr));
            assert!(c.bgra.chunks_exact(4).all(|p| p == [10, 120, 250, 255]), "{sw}x{sh} -> {dw}x{dh}");
            c.fill_from(&frame(sw, sh, [250, 120, 10, 0], Order::Rgb));
            assert!(c.bgra.chunks_exact(4).all(|p| p == [10, 120, 250, 255]), "rgb {sw}x{sh} -> {dw}x{dh}");
        }
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
        c.fill_from(&f);
        assert!(c.bgra.chunks_exact(4).all(|p| (127..=128).contains(&p[0])), "{:?}", c.bgra);
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
        let cam = CameraPicture { width: 16, height: 9, bgra: [200, 100, 50, 255].repeat(16 * 9) };
        let place = Placement { x: 0.5, y: 0.5, w: 0.5, h: 0.5, crop: [0.0; 4], flip_h: true, flip_v: false };
        c.draw_camera(&cam, &place);
        let at = |x: usize, y: usize| &c.bgra[(y * 100 + x) * 4..(y * 100 + x) * 4 + 4];
        assert_eq!(at(75, 37), [200, 100, 50, 255]);
        assert_eq!(at(10, 10), [0, 0, 0, 255]);
    }
}
