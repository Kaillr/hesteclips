//! Live frame blending on Linux, on the graphics card (`super::gpu_frame`):
//! each frame is the average light of a fixed number of looks at the screen,
//! evenly spaced over the frame's time, or blended softly into the frames
//! either side (`crate::shutter`). As on macOS (`crate::mac::blend`), whose
//! timing this follows.
//!
//! The compositor only hands over what the display shows, so the blend rate
//! is at most the display's refresh rate. Each picture is queued with when
//! it arrived (`Latest::queue_arrivals`); a look takes the picture that was
//! up at its time. Looks right next to the pictures' times would get one
//! picture twice and the next not at all by chance, so they're kept halfway
//! between the pictures, locked to their pace as on Windows and macOS
//! (`crate::shutter::follow_picture`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::screen::Frame;
use super::{Picture, Previewed, host_now, submit_preview};
use crate::mixer::Clock;
use crate::shutter::Shutter;

/// How long after a frame's time it's made: a picture reaches us a moment
/// after the screen showed it, and must still get into its frame. A guess,
/// with room for a busy system; as on macOS.
const BLEND_LATE: f64 = 0.006;

struct BlendPacer {
    t0: f64,
    /// Seconds between looks, and looks per frame.
    step: f64,
    looks: i64,
    /// Look `m` is at `t0 + phase + (m + 0.5) * step`.
    phase: f64,
    /// A picture the capture timed has come up (it set `phase`).
    timed: bool,
    shutter: Shutter,
    /// The next look to take, and the last frame done.
    next: i64,
    closed: i64,
    /// The picture up (its number), and the one the last look took.
    held: Option<u64>,
    last_taken: Option<u64>,
    /// Different pictures in the frame's own time so far.
    different: u32,
    picture: Picture,
    encoder: super::VideoEncoder,
    skipped: u64,
    stats: BlendStats,
}

impl BlendPacer {
    fn look_time(&self, m: i64) -> f64 {
        self.t0 + self.phase + (m as f64 + 0.5) * self.step
    }

    /// When the next frame to make is done: its last look.
    fn frame_end(&self) -> f64 {
        self.look_time(self.shutter.last_look(self.closed + 1))
    }

    /// Take every look before `t`: each sees the picture that's up.
    fn look_until(&mut self, t: f64) {
        let mut burst = 0;
        while self.look_time(self.next) < t && burst < 10_000 {
            self.look();
            burst += 1;
        }
    }

    fn look(&mut self) {
        let (first, weights) = self.shutter.split(self.next);
        if let Some(id) = self.held {
            for (k, weight) in [(first, weights[0]), (first + 1, weights[1])] {
                if k > self.closed && weight > 0.0 {
                    if let Some(g) = &mut self.picture.gpu {
                        g.add(k, weight);
                    }
                }
            }
            if self.last_taken != Some(id) {
                self.different += 1;
                self.last_taken = Some(id);
            }
        }
        self.next += 1;
        if self.next % self.looks == 0 {
            let different = std::mem::take(&mut self.different);
            if self.held.is_some() {
                self.stats.frame(different, self.looks as u32);
            }
        }
        // The frames no later look counts toward are done.
        let (first, _) = self.shutter.split(self.next);
        while self.closed < first - 1 {
            self.close(self.closed + 1);
        }
    }

    /// Frame `k` is done: made (with the webcam over it) and encoded.
    fn close(&mut self, k: i64) {
        self.closed = k;
        self.picture.layers();
        let Some(g) = &mut self.picture.gpu else { return };
        if !g.finish(k) {
            return;
        }
        self.picture.version += 1;
        let Some(g) = &self.picture.gpu else { return };
        if self.encoder.encode_made(g, self.picture.version, k) {
            super::encoded();
        } else {
            super::skipped(&mut self.skipped, 1);
        }
    }

    /// A new picture came up at `at`: the looks before it saw the one
    /// before. Then the looks move toward halfway between pictures.
    /// `timed`: it came from the feed's queue with its time (else it was put
    /// up some other way, and `at` is when it was noticed).
    fn arrive(&mut self, at: f64, frame: &Frame, id: u64, timed: bool) {
        crate::count_picture();
        if at < self.look_time(self.next - 1) {
            self.stats.late += 1;
            crate::count_missed(1);
        }
        self.look_until(at);
        // Pictures belong on whole steps of the looks (halfway between two);
        // the first the capture timed puts them there.
        let first = timed && !self.timed;
        self.timed |= timed;
        self.phase = crate::shutter::follow_picture(self.phase, self.step, at - self.t0, first);
        if let Some(g) = &mut self.picture.gpu {
            g.hold(frame, id);
        }
        self.held = Some(id);
        self.stats.pictures += 1;
    }
}

/// What the blending saw, printed every few seconds (to the log): how many
/// pictures the screen showed, how many different ones each frame got (all
/// its looks, when the screen changes at least at the blend rate), and how
/// many came too late for their frame. As on macOS.
struct BlendStats {
    since: Instant,
    pictures: u32,
    frames: u32,
    full: u32,
    fewest: u32,
    most: u32,
    late: u32,
}

impl BlendStats {
    fn new() -> Self {
        Self { since: Instant::now(), pictures: 0, frames: 0, full: 0, fewest: u32::MAX, most: 0, late: 0 }
    }

    fn frame(&mut self, different: u32, looks: u32) {
        self.frames += 1;
        self.full += (different == looks) as u32;
        self.fewest = self.fewest.min(different);
        self.most = self.most.max(different);
        if self.since.elapsed() >= Duration::from_secs(5) {
            eprintln!(
                "frame blending: the screen showed {:.0} pictures/s; {looks} looks per frame, {}-{} different pictures in each, {:.0}% of frames with {looks}; {} pictures too late, over {} frames",
                self.pictures as f64 / self.since.elapsed().as_secs_f64(),
                self.fewest,
                self.most,
                self.full as f64 * 100.0 / self.frames as f64,
                self.late,
                self.frames
            );
            *self = Self::new();
        }
    }
}

/// Like `super::spawn_pacer`, for frame blending: `looks` looks at the
/// screen per frame, their light averaged on the graphics card (which
/// `picture` must be made on).
pub(super) fn spawn_blend_pacer(fps: u32, looks: u32, shutter: Shutter, mut picture: Picture, encoder: super::VideoEncoder, clock: Arc<Clock>) -> (Arc<AtomicBool>, JoinHandle<()>) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    if let Some(g) = &mut picture.gpu {
        g.start_blending(shutter.frames_open());
    }
    picture.feed.queue_arrivals();
    let handle = thread::Builder::new()
        .name("blend pacer".into())
        .spawn(move || {
            let mut sent = Previewed::new(crate::preview::new_producer());
            crate::blending(fps, looks);
            let t0 = host_now();
            clock.set(t0);
            let mut p = BlendPacer {
                t0,
                step: 1.0 / (fps.max(1) * looks.max(1)) as f64,
                looks: looks.max(1) as i64,
                phase: 0.0,
                timed: false,
                shutter,
                next: 0,
                closed: -1,
                held: None,
                last_taken: None,
                different: 0,
                picture,
                encoder,
                skipped: 0,
                stats: BlendStats::new(),
            };
            let mut seen = 0;
            if let Some((f, s)) = p.picture.feed.get() {
                p.arrive(t0, &f, s, false);
                seen = s;
            }
            let mut ended = false;
            while !stop2.load(Ordering::Relaxed) {
                p.picture.feed.want();
                let wait = p.frame_end() + BLEND_LATE - host_now();
                if wait > 0.0 {
                    p.picture.feed.wait(seen, Duration::from_secs_f64(wait.min(0.05)));
                }
                for a in p.picture.feed.drain() {
                    if a.seq > seen {
                        seen = a.seq;
                        p.arrive(a.at, &a.frame, a.seq, true);
                    }
                }
                // Put up some other way (the away screen, a new window): taken from the feed.
                if let Some((f, now)) = p.picture.feed.get()
                    && now > seen
                {
                    seen = now;
                    p.arrive(host_now(), &f, now, false);
                }
                // Looks whose time is past (and late pictures' too) see what's up.
                p.look_until(host_now() - BLEND_LATE);
                if !ended && let Some(why) = p.picture.feed.ended() {
                    eprintln!("{why}");
                    ended = true;
                }
                submit_preview(&mut p.picture, &mut sent);
            }
            p.encoder.finish();
            crate::preview::clear(sent.generation);
        })
        .expect("spawning the blend pacer thread");
    (stop, handle)
}
