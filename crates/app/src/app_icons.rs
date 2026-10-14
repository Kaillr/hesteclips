//! Apps' own icons (Spotify's, Discord's), for app sources and the games and
//! apps a capture follows, in place of a generic window. Read once per app on
//! a thread of their own (from its bundle or executable), kept while the app
//! runs; one that has none (Linux, a program without a bundle, an app on
//! Windows that isn't running) is asked again now and then, as it may start.
//! Each icon read is also saved, so an app that isn't running (Windows can
//! only find its executable then) still shows its icon from the last time;
//! it's read again once the app is running, in case it changed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Icons are read at this size (pixels): sharp at the 22–30 point boxes
/// they're shown in on a 2× display.
const SIZE: u32 = 64;

/// Smooth however small it's drawn: plain linear filtering only blends the
/// four nearest pixels, so shrinking 64 to 26 made edges jagged.
const MIPMAPPED: egui::TextureOptions = egui::TextureOptions::LINEAR.with_mipmap_mode(Some(egui::TextureFilter::Linear));

/// How long before asking again for an app that had no icon.
const RETRY: Duration = Duration::from_secs(10);

enum Slot {
    /// Being read; the saved icon shows meanwhile, if there was one.
    Loading(Option<egui::TextureHandle>),
    /// Read from the app itself: final.
    Ready(egui::TextureHandle),
    /// The one saved last time, as of then: the app wasn't running to read.
    Saved(egui::TextureHandle, Instant),
    /// None to be had, as of then.
    Missing(Instant),
}

/// What a read found.
enum Found {
    Live(Vec<u8>),
    Saved(Vec<u8>),
    None,
}

#[derive(Default)]
pub struct AppIcons {
    slots: HashMap<String, Slot>,
    /// Pixels read on a thread, waiting to become textures.
    done: Arc<Mutex<Vec<(String, Found)>>>,
}

impl AppIcons {
    /// The icon of app `id` (an app source's id), if it's been read; starts
    /// reading it if not.
    pub fn get(&mut self, ctx: &egui::Context, id: &str) -> Option<egui::TextureHandle> {
        if !capture::APP_CAPTURE {
            return None;
        }
        let finished: Vec<_> = std::mem::take(&mut *self.done.lock().unwrap());
        for (app, found) in finished {
            let texture = |px: &[u8]| ctx.load_texture(format!("app icon {app}"), egui::ColorImage::from_rgba_unmultiplied([SIZE as usize, SIZE as usize], px), MIPMAPPED);
            let slot = match found {
                Found::Live(px) => Slot::Ready(texture(&px)),
                Found::Saved(px) => Slot::Saved(texture(&px), Instant::now()),
                // Not running again: the saved one it had stays.
                Found::None => match self.slots.remove(&app) {
                    Some(Slot::Loading(Some(tex))) => Slot::Saved(tex, Instant::now()),
                    _ => Slot::Missing(Instant::now()),
                },
            };
            self.slots.insert(app, slot);
        }
        let shown = match self.slots.get(id) {
            Some(Slot::Ready(tex)) => return Some(tex.clone()),
            Some(Slot::Loading(tex)) => return tex.clone(),
            Some(Slot::Saved(tex, at)) if at.elapsed() < RETRY => return Some(tex.clone()),
            Some(Slot::Missing(at)) if at.elapsed() < RETRY => return None,
            Some(Slot::Saved(tex, _)) => Some(tex.clone()),
            _ => None,
        };
        // Saved icons are looked for only the first time.
        let first = !self.slots.contains_key(id);
        self.slots.insert(id.to_owned(), Slot::Loading(shown.clone()));
        let (done, app, ctx) = (self.done.clone(), id.to_owned(), ctx.clone());
        std::thread::spawn(move || {
            let found = match capture::app_icon(&app, SIZE) {
                Some(px) => {
                    save(&app, &px);
                    Found::Live(px)
                }
                None => match first.then(|| load(&app)).flatten() {
                    Some(px) => Found::Saved(px),
                    None => Found::None,
                },
            };
            done.lock().unwrap().push((app, found));
            ctx.request_repaint();
        });
        shown
    }
}

/// Where app `id`'s icon is saved.
fn saved_path(id: &str) -> Option<std::path::PathBuf> {
    // Executables' names are the same in any case on Windows.
    let name: String = id.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' }).collect();
    // (`app-icons-2`: macOS icons cropped to their plate since; the ones
    // saved before have the margin.)
    dirs::cache_dir().map(|d| d.join("hesteclips").join("app-icons-2").join(format!("{name}.png")))
}

fn save(id: &str, px: &[u8]) {
    let Some(path) = saved_path(id) else { return };
    let Some(img) = image::RgbaImage::from_raw(SIZE, SIZE, px.to_vec()) else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = img.save(&path);
}

fn load(id: &str) -> Option<Vec<u8>> {
    let img = image::open(saved_path(id)?).ok()?.into_rgba8();
    (img.dimensions() == (SIZE, SIZE)).then(|| img.into_raw())
}

/// Draw app `id`'s icon into `rect` if there is one; false if not (draw the
/// generic one then).
pub fn paint(ui: &egui::Ui, icons: &mut AppIcons, id: &str, rect: egui::Rect) -> bool {
    let Some(tex) = icons.get(ui.ctx(), id) else { return false };
    egui::Image::from_texture((tex.id(), rect.size())).paint_at(ui, rect);
    true
}
