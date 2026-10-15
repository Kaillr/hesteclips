//! Playback frames from the in-process hardware decoder (`capture::decode`:
//! Media Foundation on Windows, VideoToolbox on macOS), on a worker thread that keeps the next frames
//! decoded before they're needed:
//!
//! - Paused, it decodes the exact frame asked for, then the few after it, so
//!   pressing play has its frames in hand at once (pre-roll).
//! - Playing, it stays a dozen frames ahead of the clock.
//! - Scrubbing, it decodes the frame under the playhead, live, with nothing
//!   built beforehand. A frame is decoded from the keyframe before it (once
//!   a second in recordings; a keyframe alone takes 1-15 ms), so every frame
//!   decoded on the way to the one asked for is kept (small, see
//!   [`SMALL_WIDTH`]: a keyframe gap and a half of them), and while idle the
//!   ones on the way are decoded too (going back, the gap before those kept;
//!   forward, on after them), 8 frames at a time, on from where it stopped:
//!   a frame kept shows at once either way, no frame is decoded twice, and a
//!   newer request never waits long. Fast, or forward far from the picture
//!   shown, the frames on the way show as they come (the keyframe first).
//!   The full-size frame comes when the scrub stops (on a 240 fps recording
//!   with 480 frames between keyframes, 30-480 ms: decoded from the keyframe).
//!
//!   Measured with `HASTECLIP_DEV_DRAG` (debug build; a release build is
//!   the same: the decoder sets the pace), frames behind the playhead once
//!   steady: 1080p60 recording, 1-10× back 0.2-7, 1× back and forth 0.2,
//!   30× forward 40; a 1440p clip with B-frames, 1-3× back 0.2-0.8, 30×
//!   forward 67; a 240 fps recording with 480 frames between keyframes, 1×
//!   either way 1, 5× sweeps 26-32, 10× sweeps ~70 (the first sweep over a
//!   stretch ~100-270, decoding it).
//!
//! Commands are coalesced: only the newest request matters, so a burst of
//! seeks costs one decode.

use std::collections::{BTreeMap, VecDeque};
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
/// Scrubbing back faster than this (seconds of video a second) to a frame not
/// kept shows the first frame decoded on the way (its keyframe) at once;
/// slower, only the frame itself (a frame from up to a keyframe gap before
/// would flash). A guess.
const FAST: f64 = 4.0;
/// Scrubbing back, frames are kept small: a keyframe gap and a half of them
/// (every frame from a keyframe is decoded on the way: kept, none is decoded
/// twice), at most this wide, smaller if that doesn't fit in
/// [`SMALL_BUDGET`] of the graphics card's memory: 60 fps recordings with a
/// keyframe a second, 1280 px (1.4 MB a frame); 240 fps with one every 2 s,
/// 640. Only while scrubbing; sharp again once it stops.
const SMALL_WIDTH: u32 = 1280;
const SMALL_BUDGET: f64 = (256 << 20) as f64;
/// Without small frames (macOS; frames read back to the CPU), full-size ones
/// are kept instead, at most this many (playback's textures).
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
            // Scrubbing: frames kept to show again at once, either way (see
            // `Kept`), small; the decoder's small frames, once asked for (how
            // many); the stretch decoded into them last (the decoder's next
            // frame follows `.0`, on up to `.1`: done once `.0` gets there);
            // and where filling in gave nothing more (the way, and the start
            // or end of the frames kept).
            let mut kept = Kept::default();
            let mut small: Option<usize> = None;
            let mut asked_small = false;
            let mut filling: Option<(u64, u64)> = None;
            let mut stuck: Option<(i8, u64)> = None;
            // The longest gap between keyframes, in frames.
            let gop = keys.windows(2).map(|w| w[1] - w[0]).max().unwrap_or((fps * 2.0) as u64).max(1);
            let full_cap = match dec.pool_size() {
                Some(n) => BEHIND.min(n.saturating_sub(12)).max(4),
                // Read back to the CPU, a 1440p frame is 15 MB.
                None if cfg!(windows) => 4,
                None => BEHIND,
            };
            // Leave room for what's on screen and just off it.
            let kept_cap = |small: Option<usize>| small.map_or(full_cap, |n| n.saturating_sub(8).max(4));
            // The last scrub's frame and when, and how fast the playhead
            // moves (frames a second, smoothed; below zero going back).
            let mut last_scrub: Option<(u64, std::time::Instant)> = None;
            let mut speed = 0.0f64;
            'work: loop {
                // Scrubbing with frames kept, which way: back (-1), forward (1,
                // with small frames: without, playback's read-ahead serves).
                let dir: i8 = match (playing, last_scrub) {
                    (false, Some(_)) if speed < 0.0 => -1,
                    (false, Some(_)) if speed > 0.0 && small.is_some() => 1,
                    _ => 0,
                };
                let room = {
                    let s = shared.lock().unwrap();
                    // Scrubbing, the frames kept are the read-ahead.
                    !at_end && dir == 0 && s.ahead.len() < if playing { ahead_playing } else { AHEAD_PAUSED }
                };
                // Filling in the frames on the way: on with the stretch under
                // way if it's still wanted, else the next one.
                let prefetch = dir != 0
                    && last_scrub.is_some_and(|(at, _)| {
                        filling.is_some_and(|f| wanted(f, at, dir)) || fill_target(&kept, &keys, at, kept_cap(small), stuck, dir).is_some()
                    });
                // Busy: just check for news. Full and playing: wait for the clock
                // to take frames. Full and paused: sleep until told something.
                let cmd = if pending.is_some() {
                    pending.take()
                } else if room || prefetch {
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
                        kept.clear();
                        filling = None;
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
                            .or_else(|| kept.get(i));
                            let have_it = have.is_some();
                            let pic = match have {
                                Some(p) => Some(p),
                                None => {
                                    let key = keys.partition_point(|&k| k <= i).checked_sub(1).map(|n| keys[n]);
                                    let fast = speed.abs() > FAST * fps;
                                    if !asked_small {
                                        asked_small = true;
                                        let (w, bytes) = small_size(dec.source_size(), gop);
                                        small = dec
                                            .small_frames(w, bytes)
                                            .inspect_err(|e| eprintln!("scrubbing keeps full-size frames: {e:#}"))
                                            .ok();
                                        if crate::player::debug() {
                                            eprintln!(
                                                "{:>8.3} video: {small:?} small frames at {w} px kept scrubbing ({gop} frames between keyframes), made in {:.0} ms",
                                                crate::player::uptime(),
                                                started.elapsed().as_secs_f64() * 1000.0
                                            );
                                        }
                                    }
                                    if speed < 0.0 || small.is_some() {
                                        let cap = kept_cap(small);
                                        let use_small = small.is_some();
                                        let back = speed < 0.0;
                                        let k = key.unwrap_or(0);
                                        // Forward far from the picture shown: the
                                        // frames on the way show as they come.
                                        let far = !back && shown.is_some_and(|s| s.abs_diff(i) as f64 > FAR * fps);
                                        // Decoding toward it already (on from its
                                        // keyframe): on to it. Else from its keyframe,
                                        // or where the frames kept from it end.
                                        let heading = filling.is_some_and(|(last, _)| last < i && key_before(&keys, last) == k);
                                        if heading {
                                            if let Some(f) = &mut filling {
                                                f.1 = f.1.max(i);
                                            }
                                        } else {
                                            let from = resume_from(&kept, k, i);
                                            kept.trim(i, cap.saturating_sub(STEP as usize + 2), back);
                                            match start_fill(&mut dec, use_small, &mut kept, from, i) {
                                                Ok(f) => filling = f,
                                                Err(e) => return fail(&format!("{e:#}")),
                                            }
                                            positioned = false;
                                            // Fast, or far: the first frame on the
                                            // way (its keyframe) shows at once.
                                            if (fast || far) && kept.get(i).is_none() {
                                                if let Some(p) = kept.get(from).filter(|p| shown != Some(p.index)) {
                                                    if crate::player::debug() {
                                                        eprintln!("{:>8.3} video: scrub back to {i}: frame {from} first, {:.1} ms", crate::player::uptime(), started.elapsed().as_secs_f64() * 1000.0);
                                                    }
                                                    shown = Some(p.index);
                                                    shared.lock().unwrap().scrub = Some(Scrubbed { asked: i, picture: p, exact: false });
                                                    ctx.request_repaint();
                                                }
                                            }
                                        }
                                        // On to it, a few frames at a time, all kept;
                                        // a newer request first (this one goes on
                                        // while idle if it's still wanted).
                                        loop {
                                            if let Some(p) = kept.get(i) {
                                                break Some(p);
                                            }
                                            let Some(mut f) = filling.take() else { break None };
                                            kept.trim(i, cap.saturating_sub(STEP as usize + 2), back);
                                            let before = f.0;
                                            let done = match step_fill(&mut dec, use_small, &mut kept, &mut f) {
                                                Ok(done) => done,
                                                Err(e) => return fail(&format!("{e:#}")),
                                            };
                                            filling = Some(f);
                                            if done && f.0 == before {
                                                // Nothing more to decode (the end).
                                                break None;
                                            }
                                            if far {
                                                if let Some(p) = kept.get(f.0).filter(|p| shown != Some(p.index)) {
                                                    shown = Some(p.index);
                                                    shared.lock().unwrap().scrub = Some(Scrubbed { asked: i, picture: p, exact: false });
                                                    ctx.request_repaint();
                                                }
                                            }
                                            if let Ok(newer) = rx.try_recv() {
                                                pending = Some(newer);
                                                continue 'work;
                                            }
                                        }
                                    } else {
                                        // The decoder goes elsewhere: a fill under way stops.
                                        filling = None;
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
                if prefetch {
                    // Scrubbing: the frames on the way (before the playhead going
                    // back, after it going forward), a few at a time (a newer
                    // request is looked at between), before it gets there.
                    let at = last_scrub.map_or(0, |(i, _)| i);
                    let cap = kept_cap(small);
                    let use_small = small.is_some();
                    let back = dir < 0;
                    let _urgent = Urgent::begin();
                    let started = std::time::Instant::now();
                    positioned = false;
                    // Room first: each frame decoded takes a texture.
                    kept.trim(at, cap.saturating_sub(STEP as usize + 2), back);
                    let mut f = match filling.filter(|&f| wanted(f, at, dir)) {
                        // On with the stretch under way (going back, no further
                        // than the playhead).
                        Some((last, to)) => (last, if back { to.min(at) } else { to }),
                        None => {
                            let Some((from, to)) = fill_target(&kept, &keys, at, cap, stuck, dir) else { continue };
                            let start = resume_from(&kept, key_before(&keys, from), from);
                            match filling {
                                // The decoder is just before it: on from there.
                                Some((last, _)) if last + 1 == from => (last, to),
                                _ => match start_fill(&mut dec, use_small, &mut kept, start, to) {
                                    Ok(Some(f)) => f,
                                    Ok(None) => {
                                        stuck = Some((dir, from));
                                        continue;
                                    }
                                    Err(e) => return fail(&format!("{e:#}")),
                                },
                            }
                        }
                    };
                    let done = match step_fill(&mut dec, use_small, &mut kept, &mut f) {
                        Ok(done) => done,
                        Err(e) => return fail(&format!("{e:#}")),
                    };
                    filling = Some(f);
                    if done {
                        // Nothing joined up (a gap the file can't fill, or its
                        // end): stop that way.
                        if let Some((from, _)) = fill_target(&kept, &keys, at, cap, stuck, dir) {
                            if filling.is_none_or(|(last, _)| last + 1 != from) {
                                stuck = Some((dir, from));
                            }
                        }
                        if crate::player::debug() {
                            eprintln!(
                                "{:>8.3} video: kept frames {:?}..={:?} while idle (playhead at {at}), {} kept",
                                crate::player::uptime(),
                                kept.start_of(at),
                                kept.end_of(at),
                                kept.frames.len()
                            );
                        }
                    }
                    if crate::player::debug() && started.elapsed().as_millis() > 20 {
                        eprintln!("{:>8.3} video: filling in took {:.1} ms", crate::player::uptime(), started.elapsed().as_secs_f64() * 1000.0);
                    }
                    continue;
                }
                if !room {
                    continue;
                }
                filling = None;
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
        if std::env::var_os("HASTECLIP_DEBUG_VIDEO").is_some() {
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

/// Frames kept while scrubbing, to show again at once either way: by number,
/// each with the last frame slot it's known to show at (up to the slot
/// before the next frame decoded after it: a dropped frame's slot shows the
/// one before the gap).
#[derive(Default)]
struct Kept {
    frames: BTreeMap<u64, (Picture, u64)>,
}

impl Kept {
    /// The picture showing at frame `i`, if that's known.
    fn get(&self, i: u64) -> Option<Picture> {
        let (_, (p, to)) = self.frames.range(..=i).next_back()?;
        (*to >= i).then(|| p.clone())
    }

    /// The last frame slot the picture kept for `i` shows at, if one is.
    fn covered_to(&self, i: u64) -> Option<u64> {
        let (_, (_, to)) = self.frames.range(..=i).next_back()?;
        (*to >= i).then_some(*to)
    }

    /// The first frame of the unbroken stretch kept up to `i`.
    fn start_of(&self, i: u64) -> Option<u64> {
        let (&f, (_, to)) = self.frames.range(..=i).next_back()?;
        if *to < i {
            return None;
        }
        let mut start = f;
        for (&g, (_, to)) in self.frames.range(..f).rev() {
            if to.saturating_add(1) < start {
                break;
            }
            start = g;
        }
        Some(start)
    }

    /// Keep `p`, showing at frames up to `to`. Decoded right after `prev`,
    /// which so shows up to the frame before it.
    fn push(&mut self, prev: Option<u64>, p: Picture, to: u64) {
        let i = p.index;
        if let Some(q) = prev.and_then(|q| self.frames.get_mut(&q)) {
            q.1 = q.1.max(i.saturating_sub(1));
        }
        self.frames.insert(i, (p, to.max(i)));
    }

    /// `last` is the video's last frame: it shows to the end.
    fn to_end(&mut self, last: u64) {
        if let Some(f) = self.frames.get_mut(&last) {
            f.1 = u64::MAX;
        }
    }

    /// The last frame slot of the unbroken stretch kept from `i` on.
    fn end_of(&self, i: u64) -> Option<u64> {
        let mut end = self.covered_to(i)?;
        for (&g, (_, to)) in self.frames.range(i.saturating_add(1)..) {
            if g > end.saturating_add(1) {
                break;
            }
            end = end.max(*to);
        }
        Some(end)
    }

    /// At most `cap` frames: drop those farthest from `at`, those passed
    /// first (after it going `back`, before it going forward).
    fn trim(&mut self, at: u64, cap: usize, back: bool) {
        while self.frames.len() > cap {
            let (Some((&lo, _)), Some((&hi, _))) = (self.frames.first_key_value(), self.frames.last_key_value()) else { return };
            let drop = if back {
                if hi > at || at.abs_diff(hi) >= at.abs_diff(lo) { hi } else { lo }
            } else if lo < at || at.abs_diff(lo) >= at.abs_diff(hi) {
                lo
            } else {
                hi
            };
            self.frames.remove(&drop);
        }
    }

    fn clear(&mut self) {
        self.frames.clear();
    }
}

/// Start decoding frames `from..` into `kept` (small ones if `small`): the
/// one showing at `from` now (decoded from its keyframe; `from` is one, or
/// where the frames kept from it end). Then [`step_fill`] goes on to `to`:
/// the stretch, done once its first frame gets there. `None`: no frame.
fn start_fill(dec: &mut Decoder, small: bool, kept: &mut Kept, from: u64, to: u64) -> anyhow::Result<Option<(u64, u64)>> {
    let first = if small { dec.frame_small(from)? } else { dec.frame(from)? };
    dec.flush();
    let Some(p) = first else { return Ok(None) };
    let last = p.index;
    kept.push(None, p, from);
    Ok(Some((last.max(from), to)))
}

/// The next few frames of the stretch `f` (the decoder's next frame follows
/// `f.0`, up to `f.1`) into `kept`. Whether it's done.
fn step_fill(dec: &mut Decoder, small: bool, kept: &mut Kept, f: &mut (u64, u64)) -> anyhow::Result<bool> {
    let done = step_fill_unflushed(dec, small, kept, f);
    // Made a few at a time, sent to the GPU together (before any is shown).
    dec.flush();
    done
}

fn step_fill_unflushed(dec: &mut Decoder, small: bool, kept: &mut Kept, f: &mut (u64, u64)) -> anyhow::Result<bool> {
    for _ in 0..STEP {
        if f.0 >= f.1 {
            return Ok(true);
        }
        match if small { dec.next_small()? } else { dec.next()? } {
            Some(p) => {
                let i = p.index;
                kept.push(Some(f.0), p, i);
                f.0 = i;
            }
            None => {
                kept.to_end(f.0);
                return Ok(true);
            }
        }
    }
    Ok(f.0 >= f.1)
}

/// The keyframe at or before frame `i` (0 if none is known).
fn key_before(keys: &[u64], i: u64) -> u64 {
    keys.partition_point(|&k| k <= i).checked_sub(1).map_or(0, |n| keys[n])
}

/// The first frame from keyframe `key` on not kept (up to `i`).
fn first_missing(kept: &Kept, key: u64, i: u64) -> u64 {
    let mut at = key;
    while at <= i {
        match kept.covered_to(at) {
            Some(to) => at = to.saturating_add(1),
            None => break,
        }
    }
    at.min(i)
}

/// Where to decode from to get frame `i` from keyframe `key`: the first frame
/// after those kept from `key` on (they're decoded already), if that's a few
/// frames on. Further on, from the keyframe again, every frame shown and
/// kept: getting there is decoding all those frames anyway, and this way a
/// newer request is looked at every few (getting to a frame 2 s into a
/// keyframe gap at 240 fps took 245 ms without a look).
fn resume_from(kept: &Kept, key: u64, i: u64) -> u64 {
    let at = first_missing(kept, key, i);
    if at - key > STEP { key } else { at }
}

/// Scrubbing with the playhead at `at`, going `dir` (-1 back, 1 forward):
/// the frames to decode next (`from..=to`, `from` the first missing), unless
/// a third of `cap` are kept that way already. Back: the keyframe gap just
/// before the stretch kept up to the playhead. Forward: on after the
/// stretch kept from it.
fn fill_target(kept: &Kept, keys: &[u64], at: u64, cap: usize, stuck: Option<(i8, u64)>, dir: i8) -> Option<(u64, u64)> {
    let third = cap as u64 / 3;
    let (from, to) = if dir < 0 {
        let start = kept.start_of(at)?;
        if start == 0 || at - start >= third {
            return None;
        }
        let to = start - 1;
        (first_missing(kept, key_before(keys, to), to), to)
    } else {
        let end = kept.end_of(at)?;
        if end == u64::MAX || end - at >= third {
            return None;
        }
        (end + 1, at + third)
    };
    (stuck != Some((dir, from))).then_some((from, to))
}

/// Whether the stretch `f` being decoded is still wanted with the playhead
/// at `at` going `dir`: not done, and still before the playhead going back,
/// at or after it going forward.
fn wanted(f: (u64, u64), at: u64, dir: i8) -> bool {
    f.0 < f.1 && if dir < 0 { f.0 < at } else { f.0 + 1 >= at }
}

/// The small frames for scrubbing back (see [`SMALL_WIDTH`]): their width and
/// the memory for them, for a video of `source` size with keyframes `gop`
/// frames apart.
fn small_size((sw, sh): (u32, u32), gop: u64) -> (u32, u64) {
    let n = (gop * 3 / 2 + 16) as f64;
    let bytes = |w: u32| (w as f64) * (w as f64 * sh as f64 / sw.max(1) as f64) * 1.5 * n;
    let mut w = SMALL_WIDTH.min(sw);
    while w > 480 && bytes(w) > SMALL_BUDGET {
        w -= 16;
    }
    (w, bytes(w).min(SMALL_BUDGET) as u64)
}

/// A decoded picture (one that came back to the CPU) as an egui image, without
/// touching each pixel: they're opaque RGBA already, the same bytes egui keeps.
pub fn to_image(p: Picture) -> egui::ColorImage {
    let pixels: Vec<egui::Color32> = bytemuck::cast_slice(&p.rgba).to_vec();
    egui::ColorImage::new([p.width as usize, p.height as usize], pixels)
}
