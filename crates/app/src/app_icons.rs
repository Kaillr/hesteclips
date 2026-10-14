//! Apps' own icons (Spotify's, Discord's), for app sources and the games and
//! apps a capture follows, in place of a generic window. Read once per app on
//! a thread of their own (from its bundle or executable), kept while the app
//! runs; one that has none (Linux, a program without a bundle, an app on
//! Windows that isn't running) is asked again now and then, as it may start.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Icons are read at this size (pixels): sharp at the 22–30 point boxes
/// they're shown in on a 2× display.
const SIZE: u32 = 64;

/// How long before asking again for an app that had no icon.
const RETRY: Duration = Duration::from_secs(10);

enum Slot {
    Loading,
    Ready(egui::TextureHandle),
    /// None to be had, as of then.
    Missing(Instant),
}

#[derive(Default)]
pub struct AppIcons {
    slots: HashMap<String, Slot>,
    /// Pixels read on a thread, waiting to become textures.
    done: Arc<Mutex<Vec<(String, Option<Vec<u8>>)>>>,
}

impl AppIcons {
    /// The icon of app `id` (an app source's id), if it's been read; starts
    /// reading it if not.
    pub fn get(&mut self, ctx: &egui::Context, id: &str) -> Option<egui::TextureHandle> {
        if !capture::APP_CAPTURE {
            return None;
        }
        let finished: Vec<_> = std::mem::take(&mut *self.done.lock().unwrap());
        for (app, px) in finished {
            let slot = match px {
                Some(px) => Slot::Ready(ctx.load_texture(format!("app icon {app}"), egui::ColorImage::from_rgba_unmultiplied([SIZE as usize, SIZE as usize], &px), egui::TextureOptions::LINEAR)),
                None => Slot::Missing(Instant::now()),
            };
            self.slots.insert(app, slot);
        }
        match self.slots.get(id) {
            Some(Slot::Ready(tex)) => return Some(tex.clone()),
            Some(Slot::Loading) => return None,
            Some(Slot::Missing(at)) if at.elapsed() < RETRY => return None,
            _ => {}
        }
        self.slots.insert(id.to_owned(), Slot::Loading);
        let (done, app, ctx) = (self.done.clone(), id.to_owned(), ctx.clone());
        std::thread::spawn(move || {
            let px = capture::app_icon(&app, SIZE);
            done.lock().unwrap().push((app, px));
            ctx.request_repaint();
        });
        None
    }
}

/// Draw app `id`'s icon into `rect` if there is one; false if not (draw the
/// generic one then).
pub fn paint(ui: &egui::Ui, icons: &mut AppIcons, id: &str, rect: egui::Rect) -> bool {
    let Some(tex) = icons.get(ui.ctx(), id) else { return false };
    egui::Image::from_texture((tex.id(), rect.size())).paint_at(ui, rect);
    true
}
