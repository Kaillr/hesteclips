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
    /// An animated picture: the frame showing (by the time since it
    /// started) uploaded as it changes. `None` showing: too big to upload
    /// as it is, its first frame stays (shrunk).
    Animated { of: crate::layers::Animation, latest: Arc<Latest>, start: std::time::Instant, showing: Option<usize> },
    /// A video file playing (its sound into the source `sound`, if any).
    Video { path: std::path::PathBuf, size: (u32, u32), sound: Option<String>, video: VideoFeed },
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
            Feed::Still { latest, .. } | Feed::Animated { latest, .. } => latest,
            Feed::Video { video, .. } => &video.latest,
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
            (Feed::Animated { of, .. }, LayerSource::Animation { animation, .. }) => of == animation,
            (Feed::Video { path, size, sound, .. }, LayerSource::Video { path: p, width, height, sound: s, .. }) => path == p && *size == (*width, *height) && sound == s,
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
                LayerSource::Animation { animation, .. } => {
                    let Some((first, _)) = animation.0.first() else { continue };
                    // One too big to upload as it is: its first frame, still.
                    let made = if first.width <= MAX_PICTURE && first.height <= MAX_PICTURE {
                        Latest::new(&self.gpu, first.width.max(1), first.height.max(1)).map(|latest| {
                            latest.upload_bgra(first.bgra.as_ptr(), first.width * 4, first.width, first.height);
                            Feed::Animated { of: animation.clone(), latest, start: std::time::Instant::now(), showing: Some(0) }
                        })
                    } else {
                        still_texture(&self.gpu, first).map(|latest| Feed::Animated { of: animation.clone(), latest, start: std::time::Instant::now(), showing: None })
                    };
                    match made {
                        Ok(feed) => feed,
                        Err(e) => {
                            eprintln!("animated picture layer: {e:#}");
                            continue;
                        }
                    }
                }
                LayerSource::Video { path, width, height, fps, looped, sound, placement, .. } => match VideoFeed::start(&self.gpu, path, (*width, *height), *fps, *looped, sound.clone(), !placement.hidden) {
                    Ok(video) => Feed::Video { path: path.clone(), size: (*width, *height), sound: sound.clone(), video },
                    Err(e) => {
                        eprintln!("video layer {}: {e:#}", path.display());
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
            // An animated picture's frame showing now, uploaded as it
            // changes; a video told whether it shows (and how it plays).
            match feed {
                Feed::Animated { of, latest, start, showing: Some(showing) } => {
                    let at = frame_at(&of.0, start.elapsed());
                    if at != *showing
                        && let Some((frame, _)) = of.0.get(at)
                    {
                        latest.upload_bgra(frame.bgra.as_ptr(), frame.width * 4, frame.width, frame.height);
                        *showing = at;
                    }
                }
                Feed::Video { video, .. } => {
                    if let Some(LayerSource::Video { looped, restart, placement, .. }) = layers.iter().find(|l| l.id == *id).map(|l| &l.source) {
                        video.set(!placement.hidden, *looped, *restart);
                    }
                }
                _ => {}
            }
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

/// Which of an animation's frames shows `t` after it started: each for its
/// time, over and over.
fn frame_at(frames: &[(crate::StillImage, Duration)], t: Duration) -> usize {
    let total: Duration = frames.iter().map(|(_, d)| *d).sum();
    if total.is_zero() || frames.len() < 2 {
        return 0;
    }
    let mut left = Duration::from_nanos((t.as_nanos() % total.as_nanos()) as u64);
    for (i, (_, d)) in frames.iter().enumerate() {
        if left < *d {
            return i;
        }
        left -= *d;
    }
    frames.len() - 1
}

/// A video file playing in a layer: decoded on a thread of its own (the
/// playback decoder, `crate::decode`), each frame put up in the layer's
/// texture when its time comes. At its end it starts over (`looped`), else
/// its last picture stays; hidden it waits where it is, and shown again
/// goes on, or starts over (`restart`).
pub(crate) struct VideoFeed {
    latest: Arc<Latest>,
    state: Arc<VideoState>,
}

#[derive(Default)]
struct VideoState {
    stop: AtomicBool,
    shown: AtomicBool,
    looped: AtomicBool,
    /// From the start: it's been shown again, with restart on.
    again: AtomicBool,
}

/// The biggest a video layer's decoded at: a bigger one is shrunk.
const MAX_VIDEO: (u32, u32) = (3840, 2160);

impl VideoFeed {
    fn start(gpu: &Gpu, path: &std::path::Path, size: (u32, u32), fps: f64, looped: bool, sound: Option<String>, shown: bool) -> anyhow::Result<Self> {
        let (w, h) = super::quad::fit_within(size.0.max(2), size.1.max(2), MAX_VIDEO.0, MAX_VIDEO.1);
        let latest = Latest::new(gpu, w, h)?;
        let state = Arc::new(VideoState::default());
        state.shown.store(shown, Ordering::Relaxed);
        state.looped.store(looped, Ordering::Relaxed);
        let (latest2, state2, path) = (latest.clone(), state.clone(), path.to_owned());
        thread::spawn(move || {
            system::com_init();
            if let Err(e) = play_video(&path, w, fps, sound, &latest2, &state2) {
                eprintln!("video layer {}: {e:#}", path.display());
            }
        });
        Ok(Self { latest, state })
    }

    /// The layer as it is now: shown or hidden, looping or not, starting
    /// over when it's shown again or not.
    fn set(&self, shown: bool, looped: bool, restart: bool) {
        let was = self.state.shown.swap(shown, Ordering::AcqRel);
        if shown && !was && restart {
            self.state.again.store(true, Ordering::Release);
        }
        self.state.looped.store(looped, Ordering::Relaxed);
    }
}

impl Drop for VideoFeed {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Relaxed);
    }
}

/// Play `path` into `latest` at `width` wide, at `fps` (none: as the
/// decoder says), till it's stopped (see [`VideoFeed`]), each frame when
/// its time comes; its sound into the audio source `sound`, if it has one.
fn play_video(path: &std::path::Path, width: u32, fps: f64, sound: Option<String>, latest: &Latest, state: &VideoState) -> anyhow::Result<()> {
    let mut decoder = crate::decode::Decoder::open_rgba(path, width)?;
    // Its first sound goes with its first picture.
    let mut sound = sound.and_then(|id| match Sound::open(path, &id) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("video layer {}: no sound ({e:#})", path.display());
            None
        }
    });
    // Frames are numbered by their time at this rate.
    decoder.set_fps(fps);
    let fps = if decoder.fps.is_finite() && decoder.fps > 1.0 { decoder.fps } else { 30.0 };
    let at = |index: u64| Duration::from_secs_f64(index as f64 / fps);
    let mut bgra = Vec::new();
    // When frame 0 was (or would have been) up: moved on by the time it's
    // been hidden, so it goes on where it was.
    let mut origin = std::time::Instant::now();
    let mut hidden_since: Option<std::time::Instant> = None;
    let mut ended = false;
    // When the last frame went up: one numbered as it was (two close
    // together in a video of an uneven rate) goes a frame after it.
    let mut last_due = None::<std::time::Instant>;
    while !state.stop.load(Ordering::Relaxed) {
        if !state.shown.load(Ordering::Acquire) {
            hidden_since.get_or_insert_with(std::time::Instant::now);
            thread::sleep(Duration::from_millis(20));
            continue;
        }
        if let Some(t) = hidden_since.take() {
            origin += t.elapsed();
        }
        if state.again.swap(false, Ordering::AcqRel) || (ended && state.looped.load(Ordering::Relaxed)) {
            ended = false;
            origin = std::time::Instant::now();
            last_due = Some(origin);
            if let Some(s) = sound.as_mut() {
                s.rewind();
            }
            if let Some(f) = decoder.frame(0)? {
                put_rgba(latest, &f, &mut bgra);
            }
            continue;
        }
        if let Some(s) = sound.as_mut() {
            s.feed(origin);
        }
        if ended {
            thread::sleep(Duration::from_millis(20));
            continue;
        }
        let Some(frame) = decoder.next()? else {
            ended = true;
            continue;
        };
        let due = match last_due {
            Some(last) if origin + at(frame.index) <= last => last + at(1),
            _ => origin + at(frame.index),
        };
        last_due = Some(due);
        loop {
            let now = std::time::Instant::now();
            if now >= due || state.stop.load(Ordering::Relaxed) || state.again.load(Ordering::Relaxed) || !state.shown.load(Ordering::Relaxed) {
                break;
            }
            if let Some(s) = sound.as_mut() {
                s.feed(origin);
            }
            thread::sleep((due - now).min(Duration::from_millis(10)));
        }
        // Far behind (the computer busy): its time is from now on.
        let now = std::time::Instant::now();
        if now > due + Duration::from_millis(500) {
            origin = now.checked_sub(at(frame.index)).unwrap_or(now);
        }
        put_rgba(latest, &frame, &mut bgra);
    }
    Ok(())
}

/// A video layer's sound: read from its file as it plays, each block handed
/// to the mix a moment before it's heard.
struct Sound {
    reader: super::sound_file::SoundReader,
    playing: crate::sources::video_sound::Playing,
    /// A block read whose time hasn't come yet.
    held: Option<(f64, Vec<f32>)>,
    /// All of it's been handed over (or it can't be read further).
    ended: bool,
}

/// How far ahead of being heard a video's sound goes to the mix (which runs
/// behind real time): little enough that what's gone when it starts over
/// is done in a moment.
const SOUND_AHEAD: f64 = 0.05;

impl Sound {
    fn open(path: &std::path::Path, id: &str) -> anyhow::Result<Self> {
        let reader = super::sound_file::SoundReader::open(path)?;
        let playing = crate::sources::video_sound::Playing::new(id, reader.rate);
        Ok(Self { reader, playing, held: None, ended: false })
    }

    /// Back to its start, with the picture.
    fn rewind(&mut self) {
        self.held = None;
        self.ended = false;
        if let Err(e) = self.reader.rewind() {
            eprintln!("video sound: {e:#}");
            self.ended = true;
        }
    }

    /// What's heard up to a moment from now handed to the mix, each block
    /// at its time after `origin` (when the video's start was up).
    fn feed(&mut self, origin: std::time::Instant) {
        if self.ended {
            return;
        }
        let played = origin.elapsed().as_secs_f64();
        let host_origin = crate::sources::host_now() - played;
        loop {
            let block = match self.held.take() {
                Some(b) => b,
                None => match self.reader.next() {
                    Ok(Some(b)) => b,
                    Ok(None) => {
                        self.ended = true;
                        return;
                    }
                    Err(e) => {
                        eprintln!("video sound: {e:#}");
                        self.ended = true;
                        return;
                    }
                },
            };
            if block.0 > played + SOUND_AHEAD {
                self.held = Some(block);
                return;
            }
            self.playing.push_at(host_origin + block.0, &block.1, self.reader.channels as usize);
        }
    }
}

/// A decoded frame (RGBA) into a layer's texture (BGRA).
fn put_rgba(latest: &Latest, frame: &crate::decode::Picture, bgra: &mut Vec<u8>) {
    let n = (frame.width * frame.height * 4) as usize;
    if frame.rgba.len() < n {
        return;
    }
    bgra.clear();
    bgra.extend(frame.rgba[..n].chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], p[3]]));
    latest.upload_bgra(bgra.as_ptr(), frame.width * 4, frame.width, frame.height);
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
                        .and_then(|monitor| d3d::monitor_item(monitor).and_then(|item| d3d::Capture::start(&gpu, &item, &latest2, d3d::Of::Display(monitor), fps)))
                        .map(|c| (c, None)),
                    Target::Window(app) => system::find_app_window(app)
                        .ok_or_else(|| anyhow::anyhow!("{app} isn't open"))
                        .and_then(|hwnd| d3d::window_item(hwnd).and_then(|item| d3d::Capture::start(&gpu, &item, &latest2, d3d::Of::Window(hwnd), fps)).map(|c| (c, Some(hwnd)))),
                    Target::Browser(host) => host
                        .window()
                        .ok_or_else(|| anyhow::anyhow!("the page isn't up yet"))
                        .and_then(|hwnd| d3d::window_item(hwnd).and_then(|item| d3d::Capture::start(&gpu, &item, &latest2, d3d::Of::Page(hwnd), fps)).map(|c| (c, Some(hwnd)))),
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    /// Each frame shows for its time, then the next; after the last, the
    /// first again.
    #[test]
    fn an_animation_shows_each_frame_for_its_time() {
        let still = || crate::StillImage { width: 1, height: 1, bgra: vec![0; 4] };
        let frames = [(still(), Duration::from_millis(100)), (still(), Duration::from_millis(50)), (still(), Duration::from_millis(200))];
        let at = |ms: u64| super::frame_at(&frames, Duration::from_millis(ms));
        assert_eq!([at(0), at(99), at(100), at(149), at(150), at(349), at(350), at(460)], [0, 0, 1, 1, 2, 2, 0, 1]);
        assert_eq!(super::frame_at(&frames[..1], Duration::from_secs(9)), 0);
    }

    /// A video layer's decoding: to its end in order, then from frame 0
    /// again and on (its loop). `HASTECLIP_TEST_VIDEO=<a video file>`, and
    /// `HASTECLIP_TEST_VIDEO_FPS=<its rate>` where the decoder's is off (it
    /// said 20 for a 60 fps recording, each number three frames).
    #[test]
    #[ignore]
    fn a_video_plays_to_its_end_and_from_the_start_again() {
        let Ok(path) = std::env::var("HASTECLIP_TEST_VIDEO") else { return };
        let path = std::path::Path::new(&path);
        assert!(crate::layers::video_size(path).is_some_and(|(w, h)| w > 0 && h > 0));
        let mut d = crate::decode::Decoder::open_rgba(path, 640).unwrap();
        if let Ok(fps) = std::env::var("HASTECLIP_TEST_VIDEO_FPS") {
            d.set_fps(fps.parse().unwrap());
        }
        let (mut last, mut n) = (None::<u64>, 0);
        while let Some(f) = d.next().unwrap() {
            assert!(last.is_none_or(|l| f.index > l), "in order: {} after {last:?} (frame {n})", f.index);
            assert!(f.rgba.len() >= (f.width * f.height * 4) as usize);
            last = Some(f.index);
            n += 1;
        }
        assert!(n > 10, "{n} frames");
        assert_eq!(d.frame(0).unwrap().map(|f| f.index), Some(0));
        let on = d.next().unwrap().map(|f| f.index);
        assert!(on.is_some_and(|i| (1..5).contains(&i)), "{on:?}");
        // Its sound: in order to its end, as long as its picture, then from
        // its start again.
        let mut sound = super::super::sound_file::SoundReader::open(path).unwrap();
        assert!(sound.rate >= 8000 && (1..=8).contains(&sound.channels), "{} Hz, {} channels", sound.rate, sound.channels);
        let (mut at, mut frames) = (-1.0, 0usize);
        while let Some((t, samples)) = sound.next().unwrap() {
            assert!(t > at, "in order: {t} after {at}");
            at = t;
            frames += samples.len() / sound.channels as usize;
        }
        let secs = frames as f64 / f64::from(sound.rate);
        let fps: f64 = std::env::var("HASTECLIP_TEST_VIDEO_FPS").ok().and_then(|f| f.parse().ok()).unwrap_or(d.fps);
        let picture = n as f64 / fps;
        assert!((secs - picture).abs() < 0.5, "sound {secs:.2} s, picture {picture:.2} s");
        sound.rewind().unwrap();
        assert_eq!(sound.next().unwrap().map(|(t, _)| t), Some(0.0));
    }
}
