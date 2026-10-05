//! Small pictures from the web — games' icons (from Discord) for the library's
//! folders, your HesteFiles profile picture — downloaded on a thread and
//! turned into textures. Game icons are kept in the cache folder; they never
//! change at their address.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

/// Side of the texture, in pixels: enough for a 32 px picture on a 2× display.
const SIZE: u32 = 64;

enum Slot {
    Loading(Arc<OnceLock<Option<egui::ColorImage>>>),
    Ready(Option<egui::TextureHandle>),
}

#[derive(Default)]
pub struct WebImages {
    by_url: HashMap<String, Slot>,
}

impl WebImages {
    /// A game's icon at `url`, once it's loaded (from the cache folder after the
    /// first time); asks for it the first time.
    pub fn icon(&mut self, ctx: &egui::Context, url: &str) -> Option<egui::TextureHandle> {
        self.get(ctx, url, true)
    }

    /// The picture at `url`, downloaded once per run (it may change at the
    /// same address, as a profile picture does).
    pub fn fresh(&mut self, ctx: &egui::Context, url: &str) -> Option<egui::TextureHandle> {
        self.get(ctx, url, false)
    }

    fn get(&mut self, ctx: &egui::Context, url: &str, keep: bool) -> Option<egui::TextureHandle> {
        let slot = self.by_url.entry(url.to_owned()).or_insert_with(|| {
            let cell = Arc::new(OnceLock::new());
            let (done, url, ctx) = (cell.clone(), url.to_owned(), ctx.clone());
            std::thread::spawn(move || {
                let _ = done.set(load(&url, keep));
                ctx.request_repaint();
            });
            Slot::Loading(cell)
        });
        if let Slot::Loading(cell) = slot
            && let Some(image) = cell.get()
        {
            let texture = image.clone().map(|img| ctx.load_texture(format!("web-image-{url}"), img, egui::TextureOptions::LINEAR));
            *slot = Slot::Ready(texture);
        }
        match slot {
            Slot::Ready(texture) => texture.clone(),
            Slot::Loading(_) => None,
        }
    }
}

/// From the cache (when `keep`), else downloaded (and cached, when `keep`).
fn load(url: &str, keep: bool) -> Option<egui::ColorImage> {
    let cached = keep.then(|| cache_path(url)).flatten();
    let bytes = match cached.as_ref().and_then(|p| std::fs::read(p).ok()) {
        Some(bytes) => bytes,
        None => {
            let bytes = ureq::get(url).call().ok()?.body_mut().with_config().limit(4 << 20).read_to_vec().ok()?;
            if let Some(p) = &cached {
                if let Some(dir) = p.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = std::fs::write(p, &bytes);
            }
            bytes
        }
    };
    let img = image::load_from_memory(&bytes).ok()?.resize(SIZE, SIZE, image::imageops::FilterType::Triangle).to_rgba8();
    Some(egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw()))
}

/// `…/app-icons/<app id>/<icon hash>.png?size=512` → `<cache>/game-icons/<app id>-<icon hash>.png`.
fn cache_path(url: &str) -> Option<PathBuf> {
    let path = url.split('?').next()?;
    let mut parts = path.rsplit('/');
    let file = parts.next()?;
    let app = parts.next()?;
    let name: String = format!("{app}-{file}").chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.')).collect();
    Some(dirs::cache_dir()?.join("hesteclips").join("game-icons").join(name))
}
