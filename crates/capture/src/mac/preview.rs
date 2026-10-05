//! The Sources page's preview on macOS: the frames being encoded, handed to
//! the UI as they are (NV12 IOSurfaces, which it draws without a copy) —
//! no conversion, no read back. While nothing records, [`PreviewCapture`]
//! runs the same capture just for it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;

use super::video::{self, Buffer, Frames, Picture};
use crate::preview::PreviewFrame;
use crate::{StillImage, VideoSource};

/// Hands frames to the preview, when it wants one.
pub(crate) struct Producer {
    generation: u64,
}

impl Producer {
    pub(crate) fn new(generation: u64) -> Self {
        Self { generation }
    }

    /// Offer the frame being encoded; taken only if the UI asked.
    pub(crate) fn offer(&self, frame: &Arc<Buffer>, waiting: bool, app: Option<String>) {
        if !crate::preview::wants_frame() {
            return;
        }
        let (width, height) = video::size_of(&frame.0);
        crate::preview::publish_frame(
            self.generation,
            PreviewFrame {
                width: width as u32,
                height: height as u32,
                recorded: (width as u32, height as u32),
                rgba: Vec::new(),
                surface: Some(crate::decode::Surface::from_buffer(frame.0.clone())),
                seq: 0,
                waiting,
                app,
            },
        );
    }
}

impl Drop for Producer {
    fn drop(&mut self) {
        crate::preview::clear(self.generation);
    }
}

/// A capture made only for the preview. Runs on its own thread: starting
/// doesn't wait for the capture to open, and dropping doesn't wait for it to
/// close.
pub(crate) struct PreviewCapture {
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    /// A new app list for the running picture, to change without a restart.
    source: Arc<Mutex<Option<VideoSource>>>,
    /// It follows games and apps (else it shows a display).
    apps: bool,
}

impl PreviewCapture {
    pub(crate) fn start(
        source: VideoSource,
        target_height: Option<u32>,
        fps: u32,
        away: Option<Arc<StillImage>>,
        webcam: Option<crate::webcam::Webcam>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let pending = Arc::new(Mutex::new(None::<VideoSource>));
        let (stop2, error2, pending2) = (stop.clone(), error.clone(), pending.clone());
        let generation = crate::preview::new_producer();
        let apps = matches!(source, VideoSource::Apps { .. });
        thread::spawn(move || {
            let opened = (|| -> Result<(Picture, Frames)> {
                let (content, display, size) = video::plan(&source, target_height)?;
                let away = match &away {
                    Some(img) if matches!(source, VideoSource::Apps { .. }) => Some(Arc::new(video::still_buffer(Some(img), size.0, size.1)?)),
                    _ => None,
                };
                let picture = Picture::start(&source, &content, &display, size, fps, away)?;
                let frames = Frames::new(picture.latest.clone(), webcam.as_ref(), size.0, size.1, generation)?;
                Ok((picture, frames))
            })();
            let (picture, mut frames) = match opened {
                Ok(p) => p,
                Err(e) => {
                    *error2.lock().unwrap() = Some(format!("{e:#}"));
                    return;
                }
            };
            // Paced like the recording would be.
            let every = Duration::from_secs_f64(1.0 / fps.max(1) as f64);
            while !stop2.load(Ordering::Relaxed) {
                if let Some(s) = pending2.lock().unwrap().take() {
                    picture.update(&s);
                }
                if crate::preview::wants_frame() {
                    frames.next();
                }
                thread::sleep(every);
            }
            drop(frames);
            picture.stop();
        });
        Self { stop, error, source: pending, apps }
    }

    /// Follow a different list of games and apps without restarting; false
    /// if that needs a restart.
    pub(crate) fn update(&self, source: &VideoSource) -> bool {
        if !self.apps || !matches!(source, VideoSource::Apps { .. }) {
            return false;
        }
        *self.source.lock().unwrap() = Some(source.clone());
        true
    }

    pub(crate) fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }
}

impl Drop for PreviewCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
