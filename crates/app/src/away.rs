//! The away screen: what clips show while you're tabbed out of your games and
//! apps (or none is open) — the logo, with "Tabbed out" under it. Drawn once
//! here and handed to the capture backend, which scales it to the recording.

use std::sync::Arc;

use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};
use image::{Rgba, RgbaImage, imageops};

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const BACKGROUND: Rgba<u8> = Rgba([20, 20, 22, 255]);
const LOGO: u32 = 300;
const TEXT: &str = "Tabbed out";
const TEXT_PX: f32 = 72.0;
const TEXT_COLOR: [u8; 3] = [235, 235, 238];

/// The away screen at 1920×1080.
pub fn screen() -> Arc<capture::StillImage> {
    let mut img = RgbaImage::from_pixel(WIDTH, HEIGHT, BACKGROUND);

    // Logo and text centred together, a little above the middle.
    let font = FontRef::try_from_slice(epaint_default_fonts::UBUNTU_LIGHT).expect("bundled font is valid");
    let scaled = font.as_scaled(PxScale::from(TEXT_PX));
    let text_h = (scaled.ascent() - scaled.descent()).ceil() as u32;
    let gap = 36;
    let top = (HEIGHT - (LOGO + gap + text_h)) / 2 - 20;

    let logo = image::load_from_memory_with_format(include_bytes!("../assets/icon-1024.png"), image::ImageFormat::Png)
        .expect("bundled icon is a valid PNG")
        .to_rgba8();
    let logo = imageops::resize(&logo, LOGO, LOGO, imageops::FilterType::Lanczos3);
    imageops::overlay(&mut img, &logo, ((WIDTH - LOGO) / 2) as i64, top as i64);

    draw_text(&mut img, &font, TEXT, WIDTH / 2, top + LOGO + gap);

    let bgra = img.pixels().flat_map(|p| [p[2], p[1], p[0], 255]).collect();
    Arc::new(capture::StillImage { width: WIDTH, height: HEIGHT, bgra })
}

/// One line of text, horizontally centred on `center_x`, its top at `top`.
fn draw_text(img: &mut RgbaImage, font: &FontRef, text: &str, center_x: u32, top: u32) {
    let scaled = font.as_scaled(PxScale::from(TEXT_PX));
    let mut glyphs = Vec::new();
    let mut x = 0.0;
    let mut prev = None;
    for c in text.chars() {
        let id = font.glyph_id(c);
        if let Some(p) = prev {
            x += scaled.kern(p, id);
        }
        glyphs.push(id.with_scale_and_position(TEXT_PX, point(x, scaled.ascent())));
        x += scaled.h_advance(id);
        prev = Some(id);
    }
    let left = center_x as f32 - x / 2.0;
    for glyph in glyphs {
        let Some(outline) = font.outline_glyph(glyph) else { continue };
        let bounds = outline.px_bounds();
        outline.draw(|gx, gy, coverage| {
            let px = (left + bounds.min.x) as i64 + gx as i64;
            let py = top as i64 + bounds.min.y as i64 + gy as i64;
            if px < 0 || py < 0 || px >= img.width() as i64 || py >= img.height() as i64 {
                return;
            }
            let dst = img.get_pixel_mut(px as u32, py as u32);
            for c in 0..3 {
                dst[c] = (dst[c] as f32 + (TEXT_COLOR[c] as f32 - dst[c] as f32) * coverage.clamp(0.0, 1.0)).round() as u8;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn renders_full_frame() {
        let img = super::screen();
        assert_eq!((img.width, img.height), (super::WIDTH, super::HEIGHT));
        assert_eq!(img.bgra.len(), (img.width * img.height * 4) as usize);
        // Something besides the background was drawn (logo and text).
        assert!(img.bgra.chunks_exact(4).any(|p| p[0] > 200));
        if let Some(path) = std::env::var_os("HC_AWAY_PNG") {
            let rgba: Vec<u8> = img.bgra.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], 255]).collect();
            image::RgbaImage::from_raw(img.width, img.height, rgba).unwrap().save(path).unwrap();
        }
    }
}
