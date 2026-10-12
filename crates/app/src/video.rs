//! Playback frames from the in-process hardware decoder (`capture::decode`:
//! Media Foundation on Windows, VideoToolbox on macOS), on a worker thread that keeps the next frames
//! decoded before they're needed:
//!
//! - Paused, it decodes the exact frame asked for, then the few after it, so
//!   pressing play has its frames in hand at once (pre-roll).
//! - Playing, it stays a dozen frames ahead of the clock.
//! - Scrubbing, it decodes the frame under the playhead, live, with nothing
//!   built beforehand. A frame is decoded from the keyframe before it (once
//!   a second in recordings; a keyframe alone takes 1-15 ms), so:
//!   - forward, far from the picture shown, it shows frames on the way
//!     there, a few frames apart, until it's there or the playhead moves on
//!     (near, the next frames are decoded already, or quick to decode on);
//!   - back fast, keyframes, and the frame itself once the playhead stops;
//!   - back slowly, the frames before the playhead are kept from decoding
//!     to it, and the ones before those decoded while it's idle, so each
//!     step back has its frame at hand.
//!
//!   Measured with `HESTECLIPS_DEV_DRAG` (debug build, 1080p60 recording):
//!   a new picture with nearly every redraw at 1-5× forward and 1× back,
//!   65-85 a second at 30-35×.
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
/// Play from `i` keeps the decoded queue when it starts at most this many
/// numbers after `i` (a dropped frame's gap). Any further and it's from later
/// on: a jump back while playing, which it can't show (with 30 here, jumps
/// back of under a second froze the picture until the clock caught up).
/// A guess: gaps in recordings are a frame or two.
const GAP: u64 = 3;

/// A scrub forward this far (seconds of video) from the picture shown shows
/// frames on the way there. A guess: closer, the frame itself comes soon
/// enough, and a picture from its keyframe first would flash back a moment.
const FAR: f64 = 0.5;
/// Nor when the frame is at most this many frames after its keyframe (it's
/// nearly as quick itself). A guess.
const NEAR_KEY: u64 = 6;
/// On the way to a far frame, a picture every this many frames (~1 ms each
/// to decode at 1080p): a newer request is looked at between them. A guess.
const STEP: u64 = 8;
/// Scrubbing faster than this (seconds of video a second) going back shows
/// keyframes; slower, every frame. A guess.
const FAST: f64 = 4.0;
/// Going back fast, the frame itself after its keyframe only if no newer
/// request comes within this long (the playhead stopped). A guess: requests
/// come with each redraw while it moves.
const SETTLE: Duration = Duration::from_millis(40);
/// Frames kept before the playhead while scrubbing back (each a texture
/// from the decoder's pool, ~5 MB at 1440p): one decode from the keyframe
/// covers this many steps back.
const BEHIND: usize = 12;

/// The queue starting with `front` can't play from `i`.
fn elsewhere(front: Option<&Picture>, i: u64) -> bool {
    front.is_none_or(|p| p.index > i + GAP)
}

enum Cmd {
    /// Paused: decode exactly this frame.
    Show(u64),
    /// Scrubbing to this frame: it, or what's quicker to see on the way.
    Scrub(u64),
    /// Play: deliver frames from this one on.
    Play(u64),
    Pause,
}

/// What a scrub found: the frame asked for, or a frame on the way there.
pub struct Scrubbed {
    /// The frame asked for.
    pub asked: u64,
    pub picture: Picture,
    /// The picture showing at `asked` (else one on the way there).
    pub exact: bool,
}

#[derive(Default)]
struct State {
    /// Consecutive frames decoded ahead, oldest first.
    ahead: VecDeque<Picture>,
    /// The answer to the latest `Show`, by the frame asked for (in a gap left
    /// by a dropped frame, the picture is the one before it).
    exact: Option<(u64, Picture)>,
    /// The newest picture a scrub found.
    scrub: Option<Scrubbed>,
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
            // Where a scrub can start decoding from cheaply (see `Cmd::Scrub`).
            let keys = dec.keyframes();
            // The last frame handed over for a paused picture or a scrub
            // (none while playing: the clock decides what's on screen).
            let mut shown: Option<u64> = None;
            // A command that came in while handling one.
            let mut pending: Option<Cmd> = None;
            // Scrubbing back slowly: frames before the playhead, decoded on
            // the way to the last one asked for, consecutive and oldest first,
            // covering up to frame `behind_end`. Each step back is one of
            // them; while idle, the ones before them are decoded too.
            let mut behind: VecDeque<Picture> = VecDeque::new();
            let mut behind_end = 0u64;
            // Frames read back to the CPU are big (a 1440p frame is 15 MB).
            let behind_cap = match dec.pool_size() {
                Some(n) => BEHIND.min(n.saturating_sub(12)).max(4),
                None if cfg!(windows) => 4,
                None => BEHIND,
            };
            // Nothing more could be decoded before this frame (the start).
            let mut no_more_before: Option<u64> = None;
            // The last scrub's frame and when, and how fast the playhead
            // moves (frames a second, smoothed; below zero going back).
            let mut last_scrub: Option<(u64, std::time::Instant)> = None;
            let mut speed = 0.0f64;
            'work: loop {
                let backward = !playing && speed < 0.0 && !behind.is_empty();
                let room = {
                    let s = shared.lock().unwrap();
                    // Going back, frames after the playhead aren't needed.
                    !at_end && !backward && s.ahead.len() < if playing { ahead_playing } else { AHEAD_PAUSED }
                };
                let refill = backward
                    && speed.abs() <= FAST * fps
                    && behind.len() <= behind_cap / 2
                    && behind.front().is_some_and(|p| p.index > 0 && no_more_before != Some(p.index));
                // Busy: just check for news. Full and playing: wait for the clock
                // to take frames. Full and paused: sleep until told something.
                let cmd = if pending.is_some() {
                    pending.take()
                } else if room || refill {
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
                    // Anything but scrubbing on: the frames kept for it go.
                    if !matches!(cmd, Cmd::Scrub(_)) {
                        behind.clear();
                        last_scrub = None;
                        speed = 0.0;
                    }
                    match cmd {
                        Cmd::Show(i) => {
                            playing = false;
                            let have = {
                                let mut s = shared.lock().unwrap();
                                drop_superseded(&mut s.ahead, i);
                                let known = shows(&s.ahead, i);
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
                            shown = pic.as_ref().map(|p| p.index);
                            s.exact = pic.map(|p| (i, p));
                            // A scrub's picture not yet taken is older: not over this.
                            s.scrub = None;
                            ctx.request_repaint();
                        }
                        Cmd::Scrub(i) => {
                            playing = false;
                            let started = std::time::Instant::now();
                            if let Some((j, at)) = last_scrub {
                                let dt = started.duration_since(at).as_secs_f64().max(1e-3);
                                if i.abs_diff(j) as f64 > fps {
                                    // More than a second of video at once: a
                                    // jump (a click), not how fast it moves.
                                    speed = 0.0;
                                } else {
                                    // Smoothed over a few requests (one comes with each redraw).
                                    speed = 0.7 * speed + 0.3 * (i as f64 - j as f64) / dt;
                                }
                            }
                            last_scrub = Some((i, started));
                            let _urgent = Urgent::begin();
                            let from = positioned.then_some(next);
                            // Decoded already: just after the last one (reading
                            // on), or just before it (scrubbing back).
                            let have = {
                                let mut s = shared.lock().unwrap();
                                drop_superseded(&mut s.ahead, i);
                                let known = shows(&s.ahead, i);
                                if known { s.ahead.pop_front() } else {
                                    s.ahead.clear();
                                    None
                                }
                            }
                            .or_else(|| take_behind(&mut behind, &mut behind_end, i));
                            let have_it = have.is_some();
                            let pic = match have {
                                Some(p) => Some(p),
                                None => {
                                    let key = keys.partition_point(|&k| k <= i).checked_sub(1).map(|n| keys[n]);
                                    let fast = speed.abs() > FAST * fps;
                                    if speed < 0.0 {
                                        // Going back fast: the keyframe before it,
                                        // one frame to decode (once: it's the same
                                        // for a second of video); the frame itself
                                        // only if the playhead stops a moment.
                                        let mut settled = !fast;
                                        if let Some(k) = key.filter(|&k| fast && i - k > NEAR_KEY) {
                                            if shown != Some(k) {
                                                match dec.frame(k) {
                                                    Ok(Some(p)) => {
                                                        if crate::player::debug() {
                                                            eprintln!("{:>8.3} video: scrub back to {i}: keyframe {k}, {:.1} ms", crate::player::uptime(), started.elapsed().as_secs_f64() * 1000.0);
                                                        }
                                                        shown = Some(p.index);
                                                        next = p.index + 1;
                                                        positioned = true;
                                                        shared.lock().unwrap().scrub = Some(Scrubbed { asked: i, picture: p, exact: false });
                                                        ctx.request_repaint();
                                                    }
                                                    Ok(None) => {}
                                                    Err(e) => return fail(&format!("{e:#}")),
                                                }
                                            }
                                            match rx.recv_timeout(SETTLE) {
                                                Ok(newer) => {
                                                    pending = Some(newer);
                                                    continue 'work;
                                                }
                                                Err(RecvTimeoutError::Timeout) => settled = true,
                                                Err(RecvTimeoutError::Disconnected) => return,
                                            }
                                        }
                                        if settled {
                                            // Slowly: the frames before it too,
                                            // decoded on the way from the keyframe.
                                            match decode_upto(&mut dec, &keys, i, behind_cap) {
                                                Ok(frames) => behind = frames,
                                                Err(e) => return fail(&format!("{e:#}")),
                                            }
                                            behind_end = i;
                                            no_more_before = None;
                                            // The decoder read past `i` (to know it was the last).
                                            positioned = false;
                                            next = i + 1;
                                            take_behind(&mut behind, &mut behind_end, i)
                                        } else {
                                            match dec.frame(i) {
                                                Ok(p) => {
                                                    positioned = true;
                                                    next = i + 1;
                                                    p
                                                }
                                                Err(e) => return fail(&format!("{e:#}")),
                                            }
                                        }
                                    } else {
                                        // Decoding on from where the decoder is
                                        // gets there without passing a keyframe.
                                        let on_the_way = positioned && next <= i && key.is_none_or(|k| k < next);
                                        let far = shown.is_some_and(|s| s.abs_diff(i) as f64 > FAR * fps);
                                        // Far: on the way there a few frames at a
                                        // time, each shown, from the keyframe before
                                        // it (a jump) or from where the decoder is,
                                        // until it's there or the playhead moves on.
                                        // Near: just the frame (steps from the
                                        // keyframe would flash back a moment).
                                        let mut step = match (far, on_the_way) {
                                            (false, _) => None,
                                            (true, true) => Some(next + STEP),
                                            (true, false) => key.filter(|&k| i - k > NEAR_KEY),
                                        }
                                        .filter(|&t| t < i);
                                        while let Some(to) = step {
                                            match dec.frame(to) {
                                                Ok(Some(p)) => {
                                                    if crate::player::debug() {
                                                        eprintln!("{:>8.3} video: scrub to {i}: frame {to} on the way, {:.1} ms", crate::player::uptime(), started.elapsed().as_secs_f64() * 1000.0);
                                                    }
                                                    shown = Some(p.index);
                                                    next = p.index + 1;
                                                    positioned = true;
                                                    shared.lock().unwrap().scrub = Some(Scrubbed { asked: i, picture: p, exact: false });
                                                    ctx.request_repaint();
                                                }
                                                Ok(None) => break,
                                                Err(e) => return fail(&format!("{e:#}")),
                                            }
                                            // The playhead moved on meanwhile: that first.
                                            if let Ok(newer) = rx.try_recv() {
                                                pending = Some(newer);
                                                continue 'work;
                                            }
                                            step = Some(to + STEP).filter(|&t| t < i);
                                        }
                                        match dec.frame(i) {
                                            Ok(p) => {
                                                positioned = true;
                                                next = i + 1;
                                                p
                                            }
                                            Err(e) => return fail(&format!("{e:#}")),
                                        }
                                    }
                                }
                            };
                            if crate::player::debug() {
                                let how = if have_it { "decoded already" } else { "decoded" };
                                eprintln!(
                                    "{:>8.3} video: scrub to {i} (decoder at {from:?}, {:.0} frames/s): {how}, {:.1} ms",
                                    crate::player::uptime(),
                                    speed,
                                    started.elapsed().as_secs_f64() * 1000.0
                                );
                            }
                            let mut s = shared.lock().unwrap();
                            if let Some(back) = s.ahead.back() {
                                next = back.index + 1;
                            }
                            at_end = pic.is_none() && speed >= 0.0;
                            if let Some(p) = pic {
                                shown = Some(p.index);
                                s.scrub = Some(Scrubbed { asked: i, picture: p, exact: true });
                            }
                            ctx.request_repaint();
                        }
                        Cmd::Play(i) => {
                            playing = true;
                            shown = None;
                            let mut s = shared.lock().unwrap();
                            generation = s.generation;
                            drop_superseded(&mut s.ahead, i);
                            // Carries on from what's decoded, unless that's from
                            // somewhere else (after a gap, the next frame can be
                            // a few numbers on).
                            if elsewhere(s.ahead.front(), i) {
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
                if refill {
                    // Scrubbing back: the frames before those kept, before
                    // the playhead gets there.
                    let front = behind.front().map_or(0, |p| p.index);
                    let started = std::time::Instant::now();
                    let _urgent = Urgent::begin();
                    match decode_upto(&mut dec, &keys, front - 1, behind_cap / 2) {
                        Ok(frames) => {
                            let before: Vec<Picture> = frames.into_iter().filter(|p| p.index < front).collect();
                            if before.is_empty() {
                                no_more_before = Some(front);
                            }
                            if crate::player::debug() {
                                eprintln!("{:>8.3} video: {} frames before {front} decoded ahead of the scrub, {:.1} ms", crate::player::uptime(), before.len(), started.elapsed().as_secs_f64() * 1000.0);
                            }
                            for p in before.into_iter().rev() {
                                behind.push_front(p);
                            }
                        }
                        Err(e) => return fail(&format!("{e:#}")),
                    }
                    positioned = false;
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
                        let ran_out = s.ahead.is_empty();
                        // Decoded for a spot `play` has since moved away from.
                        if s.generation == generation {
                            s.ahead.push_back(p);
                        }
                        drop(s);
                        // The UI draws when each frame is due; it only needs
                        // waking when it ran out (this one may be late).
                        if playing && ran_out {
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

    /// Scrubbing to frame `i`: its picture as soon as it can be had, maybe
    /// frames on the way first (see [`Self::take_scrub`]).
    pub fn scrub(&self, i: u64) {
        let _ = self.tx.send(Cmd::Scrub(i));
    }

    /// The newest picture a scrub found, once.
    pub fn take_scrub(&self) -> Option<Scrubbed> {
        self.state.lock().unwrap().scrub.take()
    }

    /// Start delivering frames from `i` on.
    /// Frames from somewhere else are thrown away here, at once: the caller
    /// takes frames from the queue in the same pass, before the worker has
    /// seen this (it showed a frame from before a scrub for a moment).
    pub fn play(&self, i: u64) {
        {
            let mut s = self.state.lock().unwrap();
            drop_superseded(&mut s.ahead, i);
            if elsewhere(s.ahead.front(), i) {
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

/// Frame `i`'s picture is at the front of `ahead` (after [`drop_superseded`]):
/// numbered `i`, or before it with the next one decoded and after it (a
/// dropped frame's slot shows the frame before the gap). Before it with
/// nothing after, it might not be the last before `i`.
fn shows(ahead: &VecDeque<Picture>, i: u64) -> bool {
    match ahead.front() {
        Some(p) if p.index == i => true,
        Some(p) => p.index < i && ahead.get(1).is_some_and(|n| n.index > i),
        None => false,
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

/// Scrubbing back: the frame showing at `i` from the frames kept before the
/// playhead (covering up to `end`), dropping those after it (passed). `None`
/// if they don't cover it: then they're of no more use, and go.
fn take_behind(behind: &mut VecDeque<Picture>, end: &mut u64, i: u64) -> Option<Picture> {
    if i > *end || behind.front().is_none_or(|p| p.index > i) {
        behind.clear();
        return None;
    }
    while behind.back().is_some_and(|p| p.index > i) {
        behind.pop_back();
    }
    let p = behind.pop_back()?;
    *end = p.index.saturating_sub(1);
    Some(p)
}

/// Up to `n` frames up to frame `to`, oldest first, from no further back
/// than its keyframe (`keys`): decoded on the way from it, as a jump to `to`
/// would be anyway. (From before it, the keyframe before that would have to
/// be decoded from too: a second of video more.)
fn decode_upto(dec: &mut Decoder, keys: &[u64], to: u64, n: usize) -> anyhow::Result<VecDeque<Picture>> {
    let mut got = VecDeque::new();
    let key = keys.partition_point(|&k| k <= to).checked_sub(1).map_or(0, |n| keys[n]);
    let from = to.saturating_sub(n.max(1) as u64 - 1).max(key);
    let Some(first) = dec.frame(from)? else { return Ok(got) };
    let mut last = first.index;
    got.push_back(first);
    while last < to {
        match dec.next()? {
            Some(p) if p.index <= to => {
                last = p.index;
                got.push_back(p);
            }
            // Past it (a dropped frame's slot ends the list), or the end.
            _ => break,
        }
    }
    Ok(got)
}

/// A decoded picture (one that came back to the CPU) as an egui image, without
/// touching each pixel: they're opaque RGBA already, the same bytes egui keeps.
pub fn to_image(p: Picture) -> egui::ColorImage {
    let pixels: Vec<egui::Color32> = bytemuck::cast_slice(&p.rgba).to_vec();
    egui::ColorImage::new([p.width as usize, p.height as usize], pixels)
}
