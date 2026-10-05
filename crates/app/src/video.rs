//! Playback frames from the in-process hardware decoder (`capture::decode`:
//! Media Foundation on Windows, VideoToolbox on macOS), on a worker thread that keeps the next frames
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

use capture::decode::{Decoder, Picture};

/// Decodes someone is waiting to see (a paused frame, filmstrip pictures).
/// While any runs, background decoding (the scrub proxy) waits: they share
/// the GPU's video decoder, and a full-speed proxy left the filmstrip
/// loading for seconds.
static URGENT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Marks an urgent decode for as long as it lives.
pub struct Urgent(());

impl Urgent {
    pub fn begin() -> Self {
        URGENT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self(())
    }
}

impl Drop for Urgent {
    fn drop(&mut self) {
        URGENT.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Background decoding: wait while anything urgent decodes.
pub fn yield_to_urgent() {
    while URGENT.load(std::sync::atomic::Ordering::Relaxed) > 0 {
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Until when a scrub through unbuilt scrub frames is under way (ms since
/// `player::uptime`'s start): the filmstrip's decoder waits meanwhile. Two
/// decoders at once made each scrub proxy seek 3-4× slower (130-170 ms, not
/// 30-45), so a fast scrub found almost nothing to show.
static SCRUB_UNTIL_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A scrub needs the scrub proxy's decoder: keep other decoding off it for
/// the next moment (renewed every UI frame while scrubbing).
pub fn scrubbing() {
    let until = (crate::player::uptime() * 1000.0) as u64 + 250;
    SCRUB_UNTIL_MS.fetch_max(until, std::sync::atomic::Ordering::Relaxed);
}

/// Thumbnail decoding: wait while a scrub is under way.
pub fn yield_to_scrub() {
    while ((crate::player::uptime() * 1000.0) as u64) < SCRUB_UNTIL_MS.load(std::sync::atomic::Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// This thread yields to the game, the app and playback (background decoding
/// and compressing: scrub frames, the filmstrip).
pub fn lower_priority() {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
    // Utility: below the UI and playback, still on the performance cores
    // when there's room (background would pin it to the efficiency cores).
    #[cfg(target_os = "macos")]
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0);
    }
}

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
    /// The answer to the latest `Show`, by the frame asked for (in a gap left
    /// by a dropped frame, the picture is the one before it).
    exact: Option<(u64, Picture)>,
    /// Opening failed or the decoder broke: use another way.
    failed: bool,
    /// When the decoder opened (play waits for its first frame until then).
    opened: Option<std::time::Instant>,
    /// Bumped each time `play` throws the queue away (playing from somewhere
    /// else): a frame the worker was already decoding for the old spot is
    /// dropped instead of joining the queue.
    generation: u64,
}

pub struct Video {
    tx: Sender<Cmd>,
    state: Arc<Mutex<State>>,
}

impl Video {
    /// Start opening `path` in the background.
    /// Frames are decoded at the video's own resolution and numbered at
    /// `fps` (the clip's, as the app counts them).
    pub fn open(ctx: &egui::Context, path: &Path, fps: f64) -> Self {
        let (tx, rx) = mpsc::channel::<Cmd>();
        let state = Arc::new(Mutex::new(State::default()));
        let (path, shared, ctx) = (path.to_path_buf(), state.clone(), ctx.clone());
        std::thread::spawn(move || {
            let opened = open_decoder(&path);
            let mut dec = match opened {
                Ok(mut d) => {
                    d.set_fps(fps);
                    shared.lock().unwrap().opened = Some(std::time::Instant::now());
                    d
                }
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
            // The decoder's next frame continues the ahead queue: read on in
            // order. Otherwise start from `next` with a lookup. (Looking up
            // each number in turn froze playback at a dropped frame: the
            // frame showing at the gap is the one before it, so the same
            // number came back every time.)
            let mut positioned = false;
            // Leave pool textures for the frame on screen, the ones just taken
            // off it, and the paused answer.
            let ahead_playing = dec.pool_size().map_or(AHEAD_PLAYING, |n| AHEAD_PLAYING.min(n.saturating_sub(8)).max(AHEAD_PAUSED));
            // The queue generation this worker is filling (see `State::generation`).
            let mut generation = 0u64;
            let mut at_end = false;
            loop {
                let room = {
                    let s = shared.lock().unwrap();
                    !at_end && s.ahead.len() < if playing { ahead_playing } else { AHEAD_PAUSED }
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
                                drop_superseded(&mut s.ahead, i);
                                // The frame showing at `i`, if the one after it
                                // is decoded too (else it might not be the last).
                                let known = s.ahead.front().is_some_and(|p| p.index <= i) && s.ahead.get(1).is_some_and(|p| p.index > i);
                                if known { s.ahead.pop_front() } else {
                                    s.ahead.clear();
                                    None
                                }
                            };
                            let pic = match have {
                                Some(p) => Some(p),
                                None => {
                                    let _urgent = Urgent::begin();
                                    match dec.frame(i) {
                                        Ok(p) => p,
                                        Err(e) => return fail(&format!("{e:#}")),
                                    }
                                }
                            };
                            let mut s = shared.lock().unwrap();
                            next = s.ahead.back().map_or(i + 1, |p| p.index + 1);
                            // Either way the decoder now continues right after
                            // what's decoded (the answer, or the queue's end).
                            positioned = true;
                            at_end = pic.is_none();
                            s.exact = pic.map(|p| (i, p));
                            ctx.request_repaint();
                        }
                        Cmd::Play(i) => {
                            playing = true;
                            let mut s = shared.lock().unwrap();
                            generation = s.generation;
                            drop_superseded(&mut s.ahead, i);
                            // Carries on from what's decoded, unless that's from
                            // somewhere else (after a gap, the next frame can be
                            // a few numbers on).
                            if s.ahead.front().is_none_or(|p| p.index > i + 30) {
                                s.ahead.clear();
                                next = i;
                                positioned = false;
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
                let got = if positioned { dec.next() } else { dec.frame(next) };
                positioned = true;
                match got {
                    Ok(Some(p)) => {
                        next = p.index + 1;
                        let mut s = shared.lock().unwrap();
                        // Decoded for a spot `play` has since moved away from.
                        if s.generation == generation {
                            s.ahead.push_back(p);
                        }
                        drop(s);
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
    /// Frames from somewhere else are thrown away here, at once: the caller
    /// takes frames from the queue in the same pass, before the worker has
    /// seen this (it showed a frame from before a scrub for a moment).
    pub fn play(&self, i: u64) {
        {
            let mut s = self.state.lock().unwrap();
            drop_superseded(&mut s.ahead, i);
            if s.ahead.front().is_none_or(|p| p.index > i + 30) {
                s.ahead.clear();
                s.generation += 1;
            }
        }
        let _ = self.tx.send(Cmd::Play(i));
    }

    /// How long ago the decoder opened, once it has.
    pub fn open_for(&self) -> Option<std::time::Duration> {
        self.state.lock().unwrap().opened.map(|t| t.elapsed())
    }

    pub fn pause(&self) {
        let _ = self.tx.send(Cmd::Pause);
    }

    /// The exact frame `i`, once decoded after `show(i)`.
    pub fn take_exact(&self, i: u64) -> Option<Picture> {
        let mut s = self.state.lock().unwrap();
        if s.exact.as_ref().is_some_and(|(asked, _)| *asked == i) { s.exact.take().map(|(_, p)| p) } else { None }
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

    /// Debug: how many frames are decoded ahead, and their numbers' range.
    pub fn ahead_info(&self) -> (usize, Option<u64>, Option<u64>) {
        let s = self.state.lock().unwrap();
        (s.ahead.len(), s.ahead.front().map(|p| p.index), s.ahead.back().map(|p| p.index))
    }

    /// Frame `i` is decoded and waiting.
    pub fn has(&self, i: u64) -> bool {
        let s = self.state.lock().unwrap();
        s.ahead.front().is_some_and(|p| p.index <= i) || s.exact.as_ref().is_some_and(|(asked, _)| *asked == i)
    }
}

/// The playback decoder: frames straight to the screen when the renderer
/// can take them; copied back as RGBA otherwise (Windows only: on macOS the
/// renderer is always Metal, which opens every frame).
fn open_decoder(path: &Path) -> anyhow::Result<Decoder> {
    #[cfg(windows)]
    {
        let on_gpu = crate::gpu_frames::share_luid().and_then(|luid| {
            Decoder::open(path, u32::MAX, Some(luid)).inspect_err(|e| eprintln!("sharing decoded frames isn't possible, copying them: {e:#}")).ok()
        });
        if std::env::var_os("HESTECLIPS_DEBUG_VIDEO").is_some() {
            eprintln!("video: decoded frames {}", if on_gpu.is_some() { "drawn from the GPU" } else { "copied to the CPU" });
        }
        match on_gpu {
            Some(d) => Ok(d),
            None => Decoder::open(path, u32::MAX, None),
        }
    }
    #[cfg(target_os = "macos")]
    {
        anyhow::ensure!(crate::gpu_frames::available(), "the renderer can't draw decoded frames");
        Decoder::open(path, capture::decode::Output::Screen)
    }
}

/// Drop decoded frames that frame `i` is past: each one whose successor is
/// at or before `i` (the last one at or before it is the one showing).
fn drop_superseded(ahead: &mut VecDeque<Picture>, i: u64) {
    while ahead.get(1).is_some_and(|next| next.index <= i) {
        ahead.pop_front();
    }
    if ahead.front().is_some_and(|p| p.index < i) && ahead.len() == 1 {
        // Alone and behind: can't tell it's still the one showing at `i`.
        ahead.clear();
    }
}

/// A decoded picture (one that came back to the CPU) as an egui image, without
/// touching each pixel: they're opaque RGBA already, the same bytes egui keeps.
pub fn to_image(p: Picture) -> egui::ColorImage {
    let pixels: Vec<egui::Color32> = bytemuck::cast_slice(&p.rgba).to_vec();
    egui::ColorImage::new([p.width as usize, p.height as usize], pixels)
}
