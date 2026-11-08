//! The pictures of a capture's layers other than its own (see
//! `crate::layers`): each webcam's newest picture, and each other display's
//! capture, in GPU textures the compositor (`d3d::Converter`) draws from.
//!
//! The layer list changes while it runs (the app keeps it up to date): a
//! layer added gets its picture, one taken out lets go of it, one changed
//! to another camera or display gets the new one's. Only [`Feeds::update`]
//! changes what's there, on the pacer's thread, once per frame; converters
//! take up the change at their next frame (see [`Feeds::generation`]).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::camera::CameraLayer;
use super::d3d::{self, Gpu, Latest};
use super::system;
use crate::layers::{Layer, LayerSource, SharedLayers};

/// The layers' pictures, shared by a capture's converters (the recording's,
/// the preview's, the stream's).
pub(crate) struct Feeds {
    gpu: Gpu,
    /// What the app wants drawn, bottom first.
    pub layers: SharedLayers,
    /// Each layer's own picture source, by layer id (none for the main
    /// picture): changed by `update` only.
    feeds: Mutex<Vec<(u64, Feed)>>,
    /// Goes up each time a layer gets another picture, or loses one.
    generation: AtomicU64,
    /// The rate a display's captured at.
    fps: u32,
}

/// One layer's picture.
enum Feed {
    Camera { device: String, format: Option<crate::webcam::Format>, layer: CameraLayer },
    Display { id: String, display: CaptureFeed },
    Window { app: String, window: CaptureFeed },
    Browser { url: String, size: (u32, u32), css: String, page: CaptureFeed },
    /// A picture or a colour, uploaded once.
    Still { of: Still, latest: Arc<Latest> },
}

/// What a still layer's picture was made from.
#[derive(PartialEq)]
enum Still {
    Picture(crate::layers::Picture),
    Colour([u8; 4]),
}

impl Feed {
    fn latest(&self) -> &Arc<Latest> {
        match self {
            Feed::Camera { layer, .. } => &layer.latest,
            Feed::Display { display: c, .. } | Feed::Window { window: c, .. } | Feed::Browser { page: c, .. } => &c.latest,
            Feed::Still { latest, .. } => latest,
        }
    }

    /// Whether it's still the picture `source` wants.
    fn is(&self, source: &LayerSource) -> bool {
        match (self, source) {
            (Feed::Camera { device, format, .. }, LayerSource::Camera { device: d, format: f, .. }) => device == d && format == f,
            (Feed::Display { id, .. }, LayerSource::Display { id: i, .. }) => id == i,
            (Feed::Window { app, .. }, LayerSource::Window { app: a, .. }) => app == a,
            (Feed::Browser { url, size, css, .. }, LayerSource::Browser { url: u, width, height, css: c, .. }) => url == u && *size == (*width, *height) && css == c,
            (Feed::Still { of: Still::Picture(p), .. }, LayerSource::Picture { picture, .. }) => p == picture,
            (Feed::Still { of: Still::Colour(c), .. }, LayerSource::Colour { rgba, .. }) => c == rgba,
            _ => false,
        }
    }
}

/// The biggest side a picture layer's uploaded at: bigger is shrunk.
const MAX_PICTURE: u32 = 4096;

/// A texture holding `image` (shrunk to fit [`MAX_PICTURE`]), its alpha kept.
fn still_texture(gpu: &Gpu, image: &crate::StillImage) -> anyhow::Result<Arc<Latest>> {
    let (w, h) = (image.width.max(1), image.height.max(1));
    anyhow::ensure!(image.bgra.len() >= (w * h * 4) as usize, "the picture's pixels are missing");
    let (tw, th) = super::quad::fit_within(w, h, MAX_PICTURE, MAX_PICTURE);
    let latest = Latest::new(gpu, tw, th)?;
    if (tw, th) == (w, h) {
        latest.upload_bgra(image.bgra.as_ptr(), w * 4, w, h);
    } else {
        // Each pixel the one under it: a picture this big is rare.
        let mut small = vec![0u8; (tw * th * 4) as usize];
        for y in 0..th {
            let sy = (y as u64 * h as u64 / th as u64) as u32;
            for x in 0..tw {
                let sx = (x as u64 * w as u64 / tw as u64) as u32;
                let from = ((sy * w + sx) * 4) as usize;
                let to = ((y * tw + x) * 4) as usize;
                small[to..to + 4].copy_from_slice(&image.bgra[from..from + 4]);
            }
        }
        latest.upload_bgra(small.as_ptr(), tw * 4, tw, th);
    }
    Ok(latest)
}

impl Feeds {
    pub(crate) fn new(gpu: &Gpu, layers: SharedLayers, fps: u32) -> Arc<Self> {
        let feeds = Arc::new(Self { gpu: gpu.clone(), layers, feeds: Mutex::new(Vec::new()), generation: AtomicU64::new(0), fps });
        feeds.update();
        feeds
    }

    /// The layers now, bottom first.
    pub(crate) fn layers(&self) -> Vec<Layer> {
        self.layers.lock().unwrap().clone()
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Layer `id`'s picture, if it has one.
    pub(crate) fn picture(&self, id: u64) -> Option<Arc<Latest>> {
        self.feeds.lock().unwrap().iter().find(|(i, _)| *i == id).map(|(_, f)| f.latest().clone())
    }

    /// Follow the layer list: a picture for each layer added (or changed
    /// to another camera or display), none for those gone. Then each
    /// webcam's newest picture, turned as it's placed, into its texture.
    /// Once per frame, on the pacer's thread.
    pub(crate) fn update(&self) {
        let layers = self.layers();
        let mut feeds = self.feeds.lock().unwrap();
        let before = feeds.len();
        feeds.retain(|(id, feed)| layers.iter().any(|l| l.id == *id && feed.is(&l.source)));
        let mut changed = feeds.len() != before;
        for l in &layers {
            if feeds.iter().any(|(id, _)| *id == l.id) {
                continue;
            }
            let feed = match &l.source {
                LayerSource::Main => continue,
                LayerSource::Camera { device, format, placement } if !device.is_empty() => {
                    match CameraLayer::new(&self.gpu, device, *format, placement.is_hidden()) {
                        Ok(layer) => Feed::Camera { device: device.clone(), format: *format, layer },
                        Err(e) => {
                            eprintln!("webcam layer: {e:#}");
                            continue;
                        }
                    }
                }
                LayerSource::Camera { .. } => continue,
                LayerSource::Display { id, .. } => match CaptureFeed::start(&self.gpu, Target::Display(id.clone()), self.fps) {
                    Ok(display) => Feed::Display { id: id.clone(), display },
                    Err(e) => {
                        eprintln!("display layer: {e:#}");
                        continue;
                    }
                },
                LayerSource::Window { app, .. } => match CaptureFeed::start(&self.gpu, Target::Window(app.clone()), self.fps) {
                    Ok(window) => Feed::Window { app: app.clone(), window },
                    Err(e) => {
                        eprintln!("window layer: {e:#}");
                        continue;
                    }
                },
                LayerSource::Browser { url, width, height, css, .. } => {
                    let host = super::browser::BrowserHost::start(url.clone(), *width, *height, css.clone());
                    match CaptureFeed::start(&self.gpu, Target::Browser(host), self.fps) {
                        Ok(page) => Feed::Browser { url: url.clone(), size: (*width, *height), css: css.clone(), page },
                        Err(e) => {
                            eprintln!("browser layer: {e:#}");
                            continue;
                        }
                    }
                }
                LayerSource::Picture { picture, .. } => match still_texture(&self.gpu, &picture.0) {
                    Ok(latest) => Feed::Still { of: Still::Picture(picture.clone()), latest },
                    Err(e) => {
                        eprintln!("picture layer: {e:#}");
                        continue;
                    }
                },
                LayerSource::Colour { rgba, .. } => {
                    let [r, g, b, a] = *rgba;
                    let image = crate::StillImage { width: 2, height: 2, bgra: [b, g, r, a].repeat(4) };
                    match still_texture(&self.gpu, &image) {
                        Ok(latest) => Feed::Still { of: Still::Colour(*rgba), latest },
                        Err(e) => {
                            eprintln!("colour layer: {e:#}");
                            continue;
                        }
                    }
                }
            };
            feeds.push((l.id, feed));
            changed = true;
        }
        for (id, feed) in feeds.iter_mut() {
            if let Feed::Camera { layer, .. } = feed {
                let placement = layers.iter().find(|l| l.id == *id).and_then(|l| match &l.source {
                    LayerSource::Camera { placement, .. } => Some(placement),
                    _ => None,
                });
                // Hidden (off, or in another scene: kept open, so switching to
                // it shows it at once), its frames aren't uploaded till it
                // shows; the newest is taken then.
                if placement.is_some_and(|p| p.is_hidden()) {
                    continue;
                }
                layer.pull(placement.map_or(0, |p| p.turns));
            }
        }
        if changed {
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
    }
}

/// What a [`CaptureFeed`] captures: a display by id, an app's main window
/// by executable.
enum Target {
    Display(String),
    Window(String),
    /// A web page's window, once it's up.
    Browser(super::browser::BrowserHost),
}

/// Another display's, or an app window's, capture: Windows' own, on a thread
/// of its own that starts it (and again, every few seconds, while it isn't
/// there or its capture stopped: a window closed, opened again) and keeps
/// it until it's dropped.
struct CaptureFeed {
    latest: Arc<Latest>,
    stop: Arc<AtomicBool>,
}

impl CaptureFeed {
    fn start(gpu: &Gpu, target: Target, fps: u32) -> anyhow::Result<Self> {
        // Room for any display (a window fits in the biggest; one bigger
        // is shrunk to fit): it can be another by the time it's found.
        let (w, h) = system::largest_display_box();
        let latest = Latest::new(gpu, w, h)?;
        let stop = Arc::new(AtomicBool::new(false));
        let (gpu, latest2, stop2) = (gpu.clone(), latest.clone(), stop.clone());
        thread::spawn(move || {
            system::com_init();
            let mut said = None::<String>;
            let name = match &target {
                Target::Display(id) => format!("display layer {id}"),
                Target::Window(app) => format!("window layer {app}"),
                Target::Browser(_) => "browser layer".to_owned(),
            };
            while !stop2.load(Ordering::Relaxed) {
                let started = match &target {
                    Target::Display(id) => system::connected_monitor(id)
                        .ok_or_else(|| anyhow::anyhow!("the display isn't connected"))
                        .and_then(d3d::monitor_item)
                        .and_then(|item| d3d::Capture::start(&gpu, &item, &latest2, None, fps))
                        .map(|c| (c, None)),
                    Target::Window(app) => system::find_app_window(app)
                        .ok_or_else(|| anyhow::anyhow!("{app} isn't open"))
                        .and_then(|hwnd| d3d::window_item(hwnd).and_then(|item| d3d::Capture::start(&gpu, &item, &latest2, Some(hwnd), fps)).map(|c| (c, Some(hwnd)))),
                    Target::Browser(host) => host
                        .window()
                        .ok_or_else(|| anyhow::anyhow!("the page isn't up yet"))
                        .and_then(|hwnd| d3d::window_item(hwnd).and_then(|item| d3d::Capture::start(&gpu, &item, &latest2, Some(hwnd), fps)).map(|c| (c, Some(hwnd)))),
                };
                match started {
                    Ok((capture, window)) => {
                        said = None;
                        // A window closed ends its capture; one hidden away
                        // (not minimized) is looked for again, it may be another.
                        let gone = |w: Option<windows::Win32::Foundation::HWND>| w.is_some_and(|h| !system::window_alive(h));
                        while !stop2.load(Ordering::Relaxed) && !capture.is_closed() && !gone(window) {
                            thread::sleep(Duration::from_millis(50));
                        }
                        latest2.forget();
                    }
                    Err(e) => {
                        // Once each time it's another reason.
                        let why = format!("{e:#}");
                        if said.as_ref() != Some(&why) {
                            eprintln!("{name}: {why}");
                            said = Some(why);
                        }
                        // Left out meanwhile, not its last picture.
                        latest2.forget();
                        let until = std::time::Instant::now() + Duration::from_secs(3);
                        while std::time::Instant::now() < until && !stop2.load(Ordering::Relaxed) {
                            thread::sleep(Duration::from_millis(50));
                        }
                    }
                }
            }
        });
        Ok(Self { latest, stop })
    }
}

impl Drop for CaptureFeed {
    /// Its thread stops the capture by itself.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
