//! Games' pictures for the library's folders, from Discord: downloaded once,
//! kept in the cache folder, and turned into small textures.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

/// Side of the texture, in pixels: enough for a chip's icon on a 2× display.
const SIZE: u32 = 64;

enum Slot {
    Loading(Arc<OnceLock<Option<egui::ColorImage>>>),
    Ready(Option<egui::TextureHandle>),
}

#[derive(Default)]
pub struct GameIcons {
    by_url: HashMap<String, Slot>,
}

impl GameIcons {
    /// The picture at `url`, once it's loaded; asks for it the first time.
    pub fn get(&mut self, ctx: &egui::Context, url: &str) -> Option<egui::TextureHandle> {
        let slot = self.by_url.entry(url.to_owned()).or_insert_with(|| {
            let cell = Arc::new(OnceLock::new());
            let (done, url, ctx) = (cell.clone(), url.to_owned(), ctx.clone());
            std::thread::spawn(move || {
                let _ = done.set(load(&url));
                ctx.request_repaint();
            });
            Slot::Loading(cell)
        });
        if let Slot::Loading(cell) = slot
            && let Some(image) = cell.get()
        {
            let texture = image.clone().map(|img| ctx.load_texture(format!("game-icon-{url}"), img, egui::TextureOptions::LINEAR));
            *slot = Slot::Ready(texture);
        }
        match slot {
            Slot::Ready(texture) => texture.clone(),
            Slot::Loading(_) => None,
        }
    }
}

/// From the cache, else downloaded (and cached).
fn load(url: &str) -> Option<egui::ColorImage> {
    let cached = cache_path(url);
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
