//! Screenshots: what's recorded, the moment the shortcut is pressed, saved in
//! the library beside the clips (in the game's folder) and put on the
//! clipboard to paste right away.
//!
//! The picture is the capture's own: every pixel of the screen or the game's
//! window, whatever shape and size recordings are made in (see
//! `capture::Recorder::screenshot`). It's written as PNG or JPG on a thread
//! of its own (a 4K PNG takes a few hundred ms), hidden until it's whole, as
//! clips are.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::settings::ScreenshotFormat;

/// How a screenshot is saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    pub format: ScreenshotFormat,
    /// Also put it on the clipboard.
    pub copy: bool,
}

/// JPG quality: a game's text and HUD stay crisp, at about a quarter of a
/// PNG's size.
const JPG_QUALITY: u8 = 92;

/// Where screenshots are being written: two in the same second each get a
/// name of their own.
static WRITING: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Save `img`, taken at `taken`, in `dir`: `screenshot_<time>.png`; and
/// whether it was copied. Copied first, when asked: what you paste is wanted
/// sooner than the file.
pub fn save(img: &capture::StillImage, dir: &Path, taken: chrono::NaiveDateTime, o: Options) -> Result<(PathBuf, bool), String> {
    let copied = o.copy
        && crate::share::copy_image(img.width, img.height, rgba(img))
            .map_err(|e| eprintln!("couldn't copy the screenshot: {e}"))
            .is_ok();
    std::fs::create_dir_all(dir).map_err(|e| format!("couldn't make its folder: {e}"))?;
    let name = format!("screenshot_{}.{}", taken.format("%Y-%m-%d_%H-%M-%S"), o.format.ext());
    let path = reserve(dir, &name);
    let written = write(img, &path, o.format);
    WRITING.lock().unwrap().retain(|p| *p != path);
    written.map(|()| (path, copied))
}

/// A free name for `name` in `dir`, also among the ones being written.
fn reserve(dir: &Path, name: &str) -> PathBuf {
    let mut writing = WRITING.lock().unwrap();
    let p = Path::new(name);
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = p.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
    let path = std::iter::once(dir.join(name))
        .chain((2..).map(|n| dir.join(format!("{stem} ({n}).{ext}"))))
        .find(|p| !p.exists() && !writing.contains(p))
        .expect("a free name");
    writing.push(path.clone());
    path
}

/// Write `img` to `path`: into a hidden folder first, moved out once it's
/// whole, so the library never shows half of one.
fn write(img: &capture::StillImage, path: &Path, format: ScreenshotFormat) -> Result<(), String> {
    let partial = capture::output::in_progress(path);
    let written = encode(img, &partial, format).and_then(|()| std::fs::rename(&partial, path).map_err(|e| format!("couldn't save it: {e}")));
    if written.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    written
}

fn encode(img: &capture::StillImage, to: &Path, format: ScreenshotFormat) -> Result<(), String> {
    let (w, h) = (img.width, img.height);
    match format {
        ScreenshotFormat::Png => {
            use image::ImageEncoder;
            use image::codecs::png::{CompressionType, FilterType, PngEncoder};
            let file = std::fs::File::create(to).map_err(|e| format!("couldn't save it: {e}"))?;
            let rgb: Vec<u8> = img.bgra.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]).collect();
            PngEncoder::new_with_quality(std::io::BufWriter::new(file), CompressionType::Default, FilterType::Adaptive)
                .write_image(&rgb, w, h, image::ExtendedColorType::Rgb8)
                .map_err(|e| format!("couldn't save it: {e}"))
        }
        ScreenshotFormat::Jpg => {
            let (Ok(jw), Ok(jh)) = (u16::try_from(w), u16::try_from(h)) else {
                return Err("it's too big for a JPG".into());
            };
            let mut enc = jpeg_encoder::Encoder::new_file(to, JPG_QUALITY).map_err(|e| format!("couldn't save it: {e}"))?;
            // Every pixel's colour: halved, as JPGs usually are, coloured
            // text and thin outlines blur.
            enc.set_sampling_factor(jpeg_encoder::SamplingFactor::R_4_4_4);
            enc.encode(&img.bgra, jw, jh, jpeg_encoder::ColorType::Bgra).map_err(|e| format!("couldn't save it: {e}"))
        }
    }
}

fn rgba(img: &capture::StillImage) -> Vec<u8> {
    img.bgra.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], 255]).collect()
}

/// Put the picture in an image file on the clipboard (as a picture, not the
/// file): "Copy image".
pub fn copy_file(path: &Path) -> Result<(), String> {
    let img = image::open(path).map_err(|e| e.to_string())?.to_rgba8();
    let (w, h) = img.dimensions();
    crate::share::copy_image(w, h, img.into_raw())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(w: u32, h: u32) -> capture::StillImage {
        let bgra = (0..w * h).flat_map(|i| [(i % 256) as u8, (i / 7 % 256) as u8, 200, 255]).collect();
        capture::StillImage { width: w, height: h, bgra }
    }

    #[test]
    fn saves_both_ways_and_names_each_its_own() {
        let dir = std::env::temp_dir().join(format!("hc-shot-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let at = chrono::NaiveDate::from_ymd_opt(2026, 10, 19).unwrap().and_hms_opt(18, 40, 2).unwrap();
        let img = picture(64, 36);
        let (png, copied) = save(&img, &dir, at, Options { format: ScreenshotFormat::Png, copy: false }).unwrap();
        assert!(!copied);
        let (again, _) = save(&img, &dir, at, Options { format: ScreenshotFormat::Png, copy: false }).unwrap();
        let (jpg, _) = save(&img, &dir, at, Options { format: ScreenshotFormat::Jpg, copy: false }).unwrap();
        assert_eq!(png.file_name().unwrap(), "screenshot_2026-10-19_18-40-02.png");
        assert_eq!(again.file_name().unwrap(), "screenshot_2026-10-19_18-40-02 (2).png");
        assert_eq!(jpg.file_name().unwrap(), "screenshot_2026-10-19_18-40-02.jpg");
        // PNG keeps every pixel; the JPG is close, and the right size.
        let back = image::open(&png).unwrap().to_rgb8();
        assert_eq!((back.width(), back.height()), (64, 36));
        assert_eq!(back.get_pixel(5, 0).0, [200, 0, 5]);
        let j = image::open(&jpg).unwrap().to_rgb8();
        assert_eq!((j.width(), j.height()), (64, 36));
        assert!(j.get_pixel(5, 0).0[0] > 180);
        // Nothing half-written is left behind.
        let hidden = std::fs::read_dir(&dir).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with('.') && e.file_name() != capture::output::WRITING).count();
        assert_eq!(hidden, 0);
        assert_eq!(std::fs::read_dir(dir.join(capture::output::WRITING)).unwrap().count(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A picture put on the clipboard is there to paste.
    #[test]
    #[ignore = "replaces what's on the clipboard: cargo test -p hesteclips copies_a_picture -- --ignored"]
    fn copies_a_picture() {
        crate::share::copy_image(4, 2, [10, 200, 30, 255].repeat(8)).unwrap();
        #[cfg(windows)]
        {
            let out = std::process::Command::new("powershell")
                .args(["-NoProfile", "-Command", "Add-Type -AssemblyName System.Windows.Forms; $i = [System.Windows.Forms.Clipboard]::GetImage(); \"$($i.Width)x$($i.Height)\""])
                .output()
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "4x2");
        }
    }
}
