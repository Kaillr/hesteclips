//! Playback frames from the in-process hardware decoder (Windows; see
//! `capture::win::decode`), on a worker thread that keeps the next frames
//! decoded before they're needed:
//!
//! - Paused, it decodes the exact frame asked for, then the few after it, so
//!   pressing play has its frames in hand at once (pre-roll).
//! - Playing, it stays a dozen frames ahead of the clock.
//!
//! Commands are coalesced: only the newest request matters, so a burst of
//! seeks costs one decode.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use capture::win::decode::{Decoder, Picture};

/// Frames kept decoded after the one shown, while paused.
const AHEAD_PAUSED: usize = 4;
/// Frames kept decoded ahead of the clock, while playing.
const AHEAD_PLAYING: usize = 12;

enum Cmd {
    /// Paused: decode exactly this frame.
    Show(u64),
    /// Play: deliver frames from this one on.
    Play(u64),
    Pause,
}

#[derive(Default)]
struct State {
    /// Consecutive frames decoded ahead, oldest first.
    ahead: VecDeque<Picture>,
    /// The answer to the latest `Show`.
    exact: Option<Picture>,
    /// Opening failed or the decoder broke: use another way.
    failed: bool,
}

pub struct Video {
    tx: Sender<Cmd>,
    state: Arc<Mutex<State>>,
}

impl Video {
    /// Start opening `path` in the background, decoding `width` wide.
    pub fn open(ctx: &egui::Context, path: &Path, width: u32) -> Self {
        let (tx, rx) = mpsc::channel::<Cmd>();
        let state = Arc::new(Mutex::new(State::default()));
        let (path, shared, ctx) = (path.to_path_buf(), state.clone(), ctx.clone());
        std::thread::spawn(move || {
            // Frames straight to the screen when the renderer can take them;
            // copied back as RGBA otherwise.
            let on_gpu = crate::gpu_frames::share_luid().and_then(|luid| {
                Decoder::open(&path, width, Some(luid)).inspect_err(|e| eprintln!("sharing decoded frames isn't possible, copying them: {e:#}")).ok()
            });
            if std::env::var_os("HESTECLIPS_DEBUG_VIDEO").is_some() {
                eprintln!("video: decoded frames {}", if on_gpu.is_some() { "drawn from the GPU" } else { "copied to the CPU" });
            }
            let opened = match on_gpu {
                Some(d) => Ok(d),
                None => Decoder::open(&path, width, None),
            };
            let mut dec = match opened {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("hardware decoder unavailable, using ffmpeg: {e:#}");
                    shared.lock().unwrap().failed = true;
                    ctx.request_repaint();
                    return;
                }
            };
            let fail = |e: &dyn std::fmt::Display| {
                eprintln!("hardware decoder: {e}");
                shared.lock().unwrap().failed = true;
                ctx.request_repaint();
            };
            let mut playing = false;
            // The frame the ahead queue continues with.
            let mut next: u64 = 0;
            let mut at_end = false;
            loop {
                let room = {
                    let s = shared.lock().unwrap();
                    !at_end && s.ahead.len() < if playing { AHEAD_PLAYING } else { AHEAD_PAUSED }
                };
                // Busy: just check for news. Full and playing: wait for the clock
                // to take frames. Full and paused: sleep until told something.
                let cmd = if room {
                    rx.try_recv().ok()
                } else if playing {
                    match rx.recv_timeout(Duration::from_millis(4)) {
                        Ok(c) => Some(c),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                } else {
                    match rx.recv() {
                        Ok(c) => Some(c),
                        Err(_) => return,
                    }
                };
                if let Some(mut cmd) = cmd {
                    while let Ok(newer) = rx.try_recv() {
                        cmd = newer;
                    }
                    match cmd {
                        Cmd::Show(i) => {
                            playing = false;
                            let have = {
                                let mut s = shared.lock().unwrap();
                                while s.ahead.front().is_some_and(|p| p.index < i) {
                                    s.ahead.pop_front();
                                }
                                if s.ahead.front().is_some_and(|p| p.index == i) { s.ahead.pop_front() } else {
                                    s.ahead.clear();
                                    None
                                }
                            };
                            let pic = match have {
                                Some(p) => Some(p),
                                None => match dec.frame(i) {
                                    Ok(p) => p,
                                    Err(e) => return fail(&format!("{e:#}")),
                                },
                            };
                            let mut s = shared.lock().unwrap();
                            next = s.ahead.back().map_or(i + 1, |p| p.index + 1);
                            at_end = pic.is_none();
                            s.exact = pic;
                            ctx.request_repaint();
                        }
                        Cmd::Play(i) => {
                            playing = true;
                            let mut s = shared.lock().unwrap();
                            while s.ahead.front().is_some_and(|p| p.index < i) {
                                s.ahead.pop_front();
                            }
                            if s.ahead.front().is_none_or(|p| p.index != i) {
                                s.ahead.clear();
                                next = i;
                                at_end = false;
                            }
                        }
                        Cmd::Pause => playing = false,
                    }
                    continue;
                }
                if !room {
                    continue;
                }
                match dec.frame(next) {
                    Ok(Some(p)) => {
                        next = p.index + 1;
                        shared.lock().unwrap().ahead.push_back(p);
                        if playing {
                            ctx.request_repaint();
                        }
                    }
                    Ok(None) => at_end = true,
                    Err(e) => return fail(&format!("{e:#}")),
                }
            }
        });
        Self { tx, state }
    }

    /// Opening failed or decoding broke.
    pub fn failed(&self) -> bool {
        self.state.lock().unwrap().failed
    }

    /// Paused on frame `i`: decode it (and the next few, for play).
    pub fn show(&self, i: u64) {
        let _ = self.tx.send(Cmd::Show(i));
    }

    /// Start delivering frames from `i` on.
    pub fn play(&self, i: u64) {
        let _ = self.tx.send(Cmd::Play(i));
    }

    pub fn pause(&self) {
        let _ = self.tx.send(Cmd::Pause);
    }

    /// The exact frame `i`, once decoded after `show(i)`.
    pub fn take_exact(&self, i: u64) -> Option<Picture> {
        let mut s = self.state.lock().unwrap();
        if s.exact.as_ref().is_some_and(|p| p.index == i) { s.exact.take() } else { None }
    }

    /// Playing: the newest decoded frame at or before `i`, dropping the ones
    /// it replaces.
    pub fn take_upto(&self, i: u64) -> Option<Picture> {
        let mut s = self.state.lock().unwrap();
        let mut best = None;
        while s.ahead.front().is_some_and(|p| p.index <= i) {
            best = s.ahead.pop_front();
        }
        best
    }

    /// Frame `i` is decoded and waiting.
    pub fn has(&self, i: u64) -> bool {
        let s = self.state.lock().unwrap();
        s.ahead.iter().any(|p| p.index == i) || s.exact.as_ref().is_some_and(|p| p.index == i)
    }
}

/// A decoded picture (one that came back to the CPU) as an egui image, without
/// touching each pixel: they're opaque RGBA already, the same bytes egui keeps.
pub fn to_image(p: Picture) -> egui::ColorImage {
    let pixels: Vec<egui::Color32> = bytemuck::cast_slice(&p.rgba).to_vec();
    egui::ColorImage::new([p.width as usize, p.height as usize], pixels)
}
