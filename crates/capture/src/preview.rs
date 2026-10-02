//! A live picture of what the recording shows, for the Sources page: at the
//! recording's resolution and frame rate.
//!
//! While the replay buffer or a recording runs, the picture comes from the
//! running capture itself, so it's exactly what's being recorded. While nothing
//! runs, a [`VideoPreview`] captures just for the preview, with the same
//! settings. Frames are only made while someone [`request`]s them — the Sources
//! page, every UI frame — and only once the last one has been taken, so the
//! preview costs nothing the rest of the time and never makes frames nobody
//! sees.
//!
//! Producers overlap for a moment when one takes over from another (a stopping
//! recording still finishing as the preview capture starts), so each has a
//! generation and only the newest may publish: the picture never flips back to
//! what the old one shows.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::VideoSource;

/// Whether this platform can show a preview.
pub const AVAILABLE: bool = cfg!(target_os = "windows");

/// One preview picture, RGBA.
#[derive(Debug)]
pub struct PreviewFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// Increases with every new frame, to notice changes cheaply.
    pub seq: u64,
    /// Nothing to show yet: none of the recorded apps is open, so the picture
    /// (and the recording) is black.
    pub waiting: bool,
    /// When recording games and apps: the one being recorded (its executable).
    pub app: Option<String>,
}

static LATEST: Mutex<Option<Arc<PreviewFrame>>> = Mutex::new(None);
static SEQ: AtomicU64 = AtomicU64::new(0);
static REQUESTED: Mutex<Option<Instant>> = Mutex::new(None);
/// The newest frame has been taken by the UI: time for another.
static TAKEN: AtomicBool = AtomicBool::new(true);
/// The newest producer; older ones' frames are dropped.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Ask for preview frames. Call every UI frame while the preview is on screen;
/// producers stop shortly after the calls stop.
pub fn request() {
    *REQUESTED.lock().unwrap() = Some(Instant::now());
}

/// The newest preview frame, if there is one. Taking it lets the next one be
/// made.
pub fn latest() -> Option<Arc<PreviewFrame>> {
    TAKEN.store(true, Ordering::Release);
    LATEST.lock().unwrap().clone()
}

/// Whether to make a frame now: someone looked at the preview recently, and
/// has taken the last frame.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn wants_frame() -> bool {
    TAKEN.load(Ordering::Acquire) && REQUESTED.lock().unwrap().is_some_and(|t| t.elapsed() < Duration::from_secs(1))
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
/// Register a new producer, which from now on is the only one whose frames are
/// shown. Returns its generation.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn new_producer() -> u64 {
    let generation = GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    *LATEST.lock().unwrap() = None; // the old picture isn't what's captured anymore
    TAKEN.store(true, Ordering::Release);
    generation
}

/// Make `rgba` the newest frame, if `generation` is still the newest producer.
/// Returns it either way, so the producer can reuse its buffer once nobody
/// else holds it.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn publish(generation: u64, width: u32, height: u32, rgba: Vec<u8>, waiting: bool, app: Option<String>) -> Arc<PreviewFrame> {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    let frame = Arc::new(PreviewFrame { width, height, rgba, seq, waiting, app });
    let mut latest = LATEST.lock().unwrap();
    if GENERATION.load(Ordering::Acquire) == generation {
        TAKEN.store(false, Ordering::Release);
        *latest = Some(frame.clone());
    }
    frame
}

/// A producer stopped: forget its last frame, unless a newer one took over.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn clear(generation: u64) {
    let mut latest = LATEST.lock().unwrap();
    if GENERATION.load(Ordering::Acquire) == generation {
        *latest = None;
    }
}

/// A capture made only for the preview, for while nothing records: what a
/// recording with these settings would show, at its size and frame rate.
///
/// Starting and stopping happen in the background — opening a capture takes a
/// moment — so neither ever holds up the UI. Stops when dropped.
pub struct VideoPreview {
    #[cfg(target_os = "windows")]
    inner: crate::win::PreviewCapture,
}

impl VideoPreview {
    /// `target_height` and `fps` as in [`crate::EncodeSettings`].
    pub fn start(source: &VideoSource, target_height: Option<u32>, fps: u32) -> Self {
        #[cfg(target_os = "windows")]
        {
            Self { inner: crate::win::PreviewCapture::start(source.clone(), target_height, fps) }
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (source, target_height, fps);
            Self {}
        }
    }

    /// Change what the preview shows without restarting it, if the change
    /// allows (a different list of games and apps). Returns whether it did.
    pub fn update(&self, source: &VideoSource) -> bool {
        #[cfg(target_os = "windows")]
        {
            self.inner.update(source)
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = source;
            false
        }
    }

    /// Why the preview couldn't start, if it couldn't.
    pub fn error(&self) -> Option<String> {
        #[cfg(target_os = "windows")]
        {
            self.inner.error()
        }
        #[cfg(not(target_os = "windows"))]
        {
            Some("the preview isn't available on this platform yet".into())
        }
    }
}
