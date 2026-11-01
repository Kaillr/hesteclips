//! Preview playback for the editor.
//!
//! Audio drives the clock: every source track is decoded up front to stereo f32
//! and mixed live in the cpal callback at the current gains, so moving a slider is
//! heard instantly. The playhead is however many samples the callback has played.
//!
//! Video follows the clock. It comes from the in-process decoder (`video.rs`:
//! the GPU's on Windows and macOS, libavcodec on Linux), which keeps the next
//! frames decoded so play starts at once; if that can't open a file, an ffmpeg process streams
//! frames from the playhead onward (NV12 at the video's own size, uploaded to
//! draw with the same sharp filter), and single exact frames are decoded on
//! demand when paused. Either way the UI shows whichever decoded frame
//! matches the clock. While scrubbing, frames come from the hardware
//! decoder, live (`video.rs`); without one, from the scrub proxy, small
//! frames built ahead in the background (`proxy.rs`).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use media::pcm::Pcm;
use media::{ClipInfo, PREVIEW_RATE, TrackEdit};

use crate::proxy::Proxy;
#[cfg(hw_decode)]
use capture::decode::Picture;
#[cfg(hw_decode)]
use crate::video::Video;
#[cfg(not(hw_decode))]
struct Picture {
    index: u64,
    width: u32,
}
#[cfg(not(hw_decode))]
struct Scrubbed {
    asked: u64,
    picture: Picture,
    exact: bool,
}

/// Elsewhere there's no in-process decoder yet: a type with no values.
#[cfg(not(hw_decode))]
enum Video {}
#[cfg(not(hw_decode))]
impl Video {
    fn failed(&self) -> bool { match *self {} }
    fn show(&self, _: u64) { match *self {} }
    fn scrub(&self, _: u64, _: Option<u64>) { match *self {} }
    fn take_scrub(&self) -> Option<Scrubbed> { match *self {} }
    fn play(&self, _: u64) { match *self {} }
    fn pause(&self) { match *self {} }
    fn take_exact(&self, _: u64) -> Option<Picture> { match *self {} }
    fn take_upto(&self, _: u64) -> Option<Picture> { match *self {} }
    fn has(&self, _: u64) -> bool { match *self {} }
    fn open_for(&self) -> Option<std::time::Duration> { match *self {} }
    fn set_skips(&self, _: Vec<(u64, u64)>) { match *self {} }
}

/// `HASTECLIP_DEBUG_VIDEO=1`: log what playback does, with times.
pub fn debug() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("HASTECLIP_DEBUG_VIDEO").is_some())
}

macro_rules! trace {
    ($($arg:tt)*) => {
        if debug() {
            eprintln!("{:>8.3} player: {}", crate::player::uptime(), format!($($arg)*));
        }
    };
}

/// How evenly new frames reach the screen while playing, for the debug log:
/// the time from one frame to the next, each second. Even playback of a 60
/// fps clip is 16.7 ms apart, give or take a display refresh.
#[derive(Default)]
struct Pacing {
    last: Option<(std::time::Instant, u64)>,
    since: Option<std::time::Instant>,
    /// Milliseconds between frames that follow each other.
    gaps: Vec<f64>,
    /// Frames never shown.
    skipped: u64,
}

impl Pacing {
    /// Frame `idx` just went on screen. Once a second: how it's been.
    fn frame(&mut self, idx: u64, fps: f64) -> Option<String> {
        let now = std::time::Instant::now();
        match self.last {
            Some((at, i)) if idx == i + 1 => self.gaps.push((now - at).as_secs_f64() * 1000.0),
            Some((_, i)) if idx > i + 1 => self.skipped += idx - i - 1,
            _ => {}
        }
        self.last = Some((now, idx));
        let since = *self.since.get_or_insert(now);
        if now - since < std::time::Duration::from_secs(1) || self.gaps.is_empty() {
            return None;
        }
        let n = self.gaps.len() as f64;
        let mean = self.gaps.iter().sum::<f64>() / n;
        let sd = (self.gaps.iter().map(|g| (g - mean).powi(2)).sum::<f64>() / n).sqrt();
        let (min, max) = self.gaps.iter().fold((f64::MAX, 0.0f64), |(a, b), g| (a.min(*g), b.max(*g)));
        let line = format!(
            "pacing {} frames {mean:.1} ± {sd:.1} ms apart ({min:.1}–{max:.1}, even is {:.1}), {} skipped",
            self.gaps.len(),
            1000.0 / fps.max(1.0),
            self.skipped
        );
        *self = Self { last: self.last, ..Self::default() };
        Some(line)
    }
}

/// Seconds since the app started, for debug logs.
pub fn uptime() -> f64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_secs_f64()
}

/// The scrub frames built so far, as a thin bar along the bottom of `lane`
/// (which shows `from..to` seconds): built stretches lit, the rest dark. It
/// fills from wherever the playhead is, not from the start.
pub fn paint_proxy_bar(ui: &egui::Ui, player: &crate::player::Player, lane: egui::Rect, from: f64, to: f64) {
    if player.proxy_progress() >= 1.0 {
        return;
    }
    let n = (lane.width() / 2.0).max(1.0) as usize;
    let built = player.proxy_built(from, to, n);
    let p = ui.painter_at(lane);
    let y = lane.bottom() - 3.0..=lane.bottom();
    let step = lane.width() / n as f32;
    let mut k = 0;
    while k < n {
        let on = built[k];
        let run = built[k..].iter().take_while(|b| **b == on).count();
        let x0 = lane.left() + k as f32 * step;
        let color = if on { egui::Color32::from_white_alpha(150) } else { egui::Color32::from_white_alpha(25) };
        p.rect_filled(egui::Rect::from_x_y_ranges(x0..=x0 + run as f32 * step, y.clone()), 0, color);
        k += run;
    }
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
}

/// What's on screen, with its size in pixels.
enum Display {
    /// An image through egui: scrub proxy frames, frames that came back to
    /// the CPU.
    Image(egui::TextureId, egui::Vec2),
    /// A frame on the GPU, drawn with the sharp filter: the hardware
    /// decoder's, or one ffmpeg decoded, uploaded.
    Gpu(crate::gpu_frames::Shown, egui::Vec2),
}

impl Display {
    fn size(&self) -> egui::Vec2 {
        match self {
            Display::Image(_, s) | Display::Gpu(_, s) => *s,
        }
    }
}

/// A frame from ffmpeg: NV12 at the video's own size, to upload and draw
/// with the sharp filter; RGBA at [`PREVIEW_WIDTH`] where that can't be.
enum Decoded {
    Nv12(Vec<u8>),
    Rgba(media::Frame),
}

/// Play waits at most this long for its first frame before the sound starts anyway.
const START_WAIT: std::time::Duration = std::time::Duration::from_millis(100);
/// The same when the decoder had to go to the playhead first (after a
/// scrub or a paused jump): it decodes from the keyframe before, up to a
/// second of frames.
#[cfg(hw_decode)]
const CATCH_UP_WAIT: std::time::Duration = std::time::Duration::from_millis(500);
/// The same for ffmpeg's first frame, which takes longer: it starts from the
/// keyframe before (~170 ms for 1440p60).
const FFMPEG_START_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// Frames from ffmpeg are decoded at this width when they can't be drawn
/// with the sharp filter.
pub const PREVIEW_WIDTH: u32 = 1280;

/// Live mix state shared with the audio callback.
pub struct Mix {
    /// Per source track: volume (incl. keyframes) and mute, as in the edit.
    pub tracks: Vec<TrackEdit>,
    /// Peak/RMS of what was just played, per track and for the master, for meters.
    pub track_levels: Vec<(f32, f32)>,
    pub master_level: (f32, f32),
    /// The same, per channel (left, right), for a stereo meter.
    pub master_stereo: capture::mixer::Levels,
}

struct Shared {
    /// Stereo frames (sample pairs) played since `start`.
    pos: AtomicU64,
    playing: AtomicBool,
    /// Paused and being scrubbed: the sound follows `pos` (see [`Scrub`]).
    scrubbing: AtomicBool,
    /// Where playback stops (in stereo frames).
    end: AtomicU64,
    mix: Mutex<Mix>,
    pcm: Mutex<Arc<Pcm>>,
    /// When the audio callback last moved `pos` ([`uptime`], µs), and how
    /// much it moves it each time (µs of sound): the clock only moves in
    /// those steps, so this says when the next frame can come up.
    moved_at_us: AtomicU64,
    step_us: AtomicU64,
    /// How fast it plays (1.0 = as recorded), as `f32` bits.
    speed: AtomicU32,
    /// How loud you listen (the volume and mute shared by the player and the
    /// editor), as `f32` bits: applied to what's heard after the meters, so
    /// they show the edit's own levels.
    listen: AtomicU32,
    /// The stretches cut out of the clip, which playing jumps over.
    skips: Mutex<Vec<Skip>>,
}

/// A stretch cut out of the clip, in stereo frames: playing jumps from
/// `from` to `to`, the sound crossfading over `fade` either side of the join
/// as the saved clip's does (`media::join_fades`).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Skip {
    from: u64,
    to: u64,
    fade: u64,
}

impl Skip {
    /// Playing at `pos`, within this join's crossfade: where the other side
    /// of the join is, and how much of it is heard (half, at the join).
    /// Before it, what comes after it fades in; after it, what came before
    /// fades out.
    fn across(&self, pos: u64) -> Option<(u64, f32)> {
        if self.fade == 0 {
            return None;
        }
        if pos < self.from && pos + self.fade >= self.from {
            let left = self.from - pos;
            Some((self.to - left, 0.5 * (1.0 - left as f32 / self.fade as f32)))
        } else if pos >= self.to && pos < self.to + self.fade {
            let past = pos - self.to;
            Some((self.from + past, 0.5 * (1.0 - past as f32 / self.fade as f32)))
        } else {
            None
        }
    }
}

impl Shared {
    fn new(mix: Mix, pcm: Arc<Pcm>) -> Self {
        Self {
            pos: AtomicU64::new(0),
            playing: AtomicBool::new(false),
            scrubbing: AtomicBool::new(false),
            end: AtomicU64::new(u64::MAX),
            mix: Mutex::new(mix),
            pcm: Mutex::new(pcm),
            moved_at_us: AtomicU64::new(0),
            step_us: AtomicU64::new(0),
            speed: AtomicU32::new(1.0f32.to_bits()),
            listen: AtomicU32::new(1.0f32.to_bits()),
            skips: Mutex::new(Vec::new()),
        }
    }

    fn speed(&self) -> f64 {
        f64::from(f32::from_bits(self.speed.load(Ordering::Relaxed)))
    }

    /// The playhead, in seconds. Playing, it moves smoothly: the audio
    /// callback moves `pos` a buffer at a time (480 frames, 10 ms), and the
    /// picture timed by those steps changed 8 to 23 ms apart at 60 fps
    /// instead of every 16.7 (a stutter frame blending shows up). So between
    /// steps it's carried on by the time since the last one, from a step
    /// behind: it gets where the callback put it just as the next one comes,
    /// and never jumps or goes back. A late callback holds it there.
    fn now(&self) -> f64 {
        let pos = self.pos.load(Ordering::Relaxed) as f64;
        let step_us = self.step_us.load(Ordering::Relaxed) as f64;
        if !self.playing.load(Ordering::Relaxed) || step_us <= 0.0 {
            return pos / PREVIEW_RATE as f64;
        }
        let since = (uptime() * 1e6 - self.moved_at_us.load(Ordering::Relaxed) as f64).clamp(0.0, step_us);
        // Source frames per µs.
        let rate = PREVIEW_RATE as f64 * self.speed() / 1e6;
        let behind = (pos - (step_us - since) * rate).max(0.0);
        // A step behind a jump over a cut lands in what's cut out: it's at
        // the frame after the cut by then.
        let cut = self.skips.lock().ok().and_then(|s| s.iter().find(|s| (s.from as f64..s.to as f64).contains(&behind)).copied());
        cut.map_or(behind, |s| s.to as f64) / PREVIEW_RATE as f64
    }

    /// How long until the playhead ([`Self::now`]) shows `t` seconds.
    /// Playing only.
    fn until(&self, t: f64) -> std::time::Duration {
        let ahead = (t - self.now()).max(0.0) / self.speed().max(0.01);
        std::time::Duration::from_secs_f64(ahead.clamp(0.001, 0.1))
    }
}

pub struct Player {
    source: PathBuf,
    info: ClipInfo,
    shared: Arc<Shared>,
    _stream: Option<cpal::Stream>,
    pub audio_error: Option<String>,
    /// Latest frame for display.
    texture: Option<egui::TextureHandle>,
    /// That frame's colours, blurred wide: the stage's light (a frame on the
    /// GPU makes its own there).
    glow: Option<egui::TextureHandle>,
    /// What's on screen.
    display: Option<Display>,
    /// The GPU frame on screen, held so its texture isn't reused meanwhile.
    on_screen: Option<Picture>,
    /// GPU frames just taken off screen, held a few more frames: the GPU may
    /// still be drawing them, and the decoder reuses a released texture.
    retired: std::collections::VecDeque<(u64, Picture)>,
    /// GPU frames opened for drawing (see `gpu_frames.rs`).
    #[cfg(hw_decode)]
    gpu_frames: crate::gpu_frames::Frames,
    /// Frames from ffmpeg, uploaded for drawing.
    uploads: crate::gpu_frames::Uploads,
    shown_frame: Option<u64>,
    decoder: Option<FrameStream>,
    /// Playing up to a cut: frames read from just after it (from the frame
    /// numbered here), ready for when the playhead jumps there.
    after_cut: Option<(u64, FrameStream)>,
    still_tx: Sender<(u64, f64)>,
    still_rx: Receiver<(u64, Decoded)>,
    /// Frame index the last on-demand still was requested for.
    still_wanted: Option<u64>,
    /// Every frame, small, for instant scrubbing, built in the background:
    /// only without the hardware decoder (which scrubs live).
    proxy: Option<Proxy>,
    /// The frame a scrub last asked the hardware decoder for.
    scrub_asked: Option<u64>,
    /// Showing a proxy frame; swap in the sharp frame once the drag settles.
    showing_proxy: bool,
    /// While scrubbing where the proxy isn't built yet: the nearest frame
    /// that is, on screen in place of the one wanted.
    stand_in: Option<u64>,
    /// The hardware decoder, while it works.
    video: Option<Video>,
    /// Live scrubbing (`scrub.rs`): frames decoded ahead of the playhead
    /// by several decoders, read on every redraw.
    #[cfg(hw_decode)]
    scrub: Option<crate::scrub::Engine>,
    /// The engine while it opens, on a thread of its own: it waits for the
    /// graphics card's decoder, which the first clip of a run starts
    /// (~150 ms: the UI was held that long). Until it's there, scrubbing
    /// goes through the playback decoder.
    #[cfg(hw_decode)]
    scrub_opening: Option<Receiver<Option<crate::scrub::Engine>>>,
    /// The playhead's motion while scrubbing.
    #[cfg(hw_decode)]
    motion: Option<Motion>,
    /// How long a redraw takes while scrubbing (s), smoothed.
    #[cfg(hw_decode)]
    redraw_secs: f64,
    /// The engine's frames uploaded ahead of the playhead (`scrub.rs`).
    #[cfg(all(hw_decode, target_os = "linux"))]
    uploader: Option<crate::scrub::Uploader>,
    /// The playhead frame a stand-in (the nearest frame decoded, its own
    /// not being yet) was put on screen for: one per frame the playhead is
    /// at, then only its own. (Each frame decoded on the way to it showed
    /// as it came, nearer and nearer: stopped, the picture played on a
    /// few frames.)
    #[cfg(hw_decode)]
    stood_in_for: Option<u64>,
    /// The playback decoder isn't at the playhead yet: scrubbing left it
    /// elsewhere, or it's still decoding the frame it was asked for. Play
    /// waits for it rather than start the sound over a still picture.
    #[cfg(hw_decode)]
    catching_up: bool,
    /// How long play waits for its first frame before the sound starts
    /// anyway (longer while the decoder is catching up: it decodes from
    /// the keyframe before).
    pending_wait: std::time::Duration,
    /// While the playhead itself is scrubbed (a drag of it, the wheel), how
    /// near the playhead (seconds) a picture held still on stays, and where
    /// letting go lands ([`Self::land`]); `None` for other drags (a trim's
    /// edge: the picture is fetched for where the edge is).
    landing: Option<f64>,
    /// Play was pressed; the sound starts with the first frame (or after
    /// [`START_WAIT`], [`FFMPEG_START_WAIT`] for ffmpeg's), so picture and
    /// sound start together.
    pending_start: Option<std::time::Instant>,
    /// The decoder was told to play (to tell it to stop when playback ends).
    video_playing: bool,
    /// When play was last pressed, until its first frame shows (debug log).
    played_at: Option<std::time::Instant>,
    /// When the full-quality frame was last asked for (debug log).
    asked_at: Option<std::time::Instant>,
    /// Frames shown while playing, per second (debug log).
    /// How evenly new frames reach the screen while playing (debug log).
    pacing: Pacing,
    /// The frame whose picture is on screen, whatever kind (debug log).
    screen_index: Option<u64>,
    /// How the picture keeps up while scrubbing (debug log).
    scrub_stats: ScrubStats,
    /// Stepping back frame by frame with the key held: the picture comes as
    /// it does scrubbing (no sound).
    stepping: bool,
}

/// How the playhead moves while it's scrubbed (with the scrub engine).
#[cfg(hw_decode)]
#[derive(Clone, Copy)]
struct Motion {
    /// Where it was at the last redraw, and when that was.
    at: u64,
    seen: std::time::Instant,
    /// Frames a second (below zero going back), smoothed.
    speed: f64,
    /// When it got where it is, and whether by a jump (a click) rather
    /// than moving there.
    moved: std::time::Instant,
    jumped: bool,
}

/// Scrubbing, the playhead that hasn't moved for this long is held still:
/// the scrub engine decodes around it, and the picture settles. (A pointer
/// held still sends nothing: the UI redraws this long after it last moved
/// to see that.)
#[cfg(hw_decode)]
const HELD: std::time::Duration = std::time::Duration::from_millis(50);

/// Debug log: while scrubbing, once a second, how many new pictures came
/// up, how many redraws showed the frame wanted, and how far behind it the
/// picture was (in frames, over the redraws).
#[derive(Default)]
struct ScrubStats {
    since: Option<std::time::Instant>,
    redraws: u32,
    pictures: u32,
    exact: u32,
    behind: u64,
    worst: u64,
    last: Option<u64>,
    /// Time putting new pictures on screen (s), in all and the longest.
    showing: f64,
    showing_max: f64,
}

impl ScrubStats {
    #[cfg(hw_decode)]
    fn showed(&mut self, secs: f64) {
        self.showing += secs;
        self.showing_max = self.showing_max.max(secs);
    }

    fn note(&mut self, want: u64, on_screen: Option<u64>, exact: bool) {
        let since = *self.since.get_or_insert_with(std::time::Instant::now);
        self.redraws += 1;
        if on_screen != self.last {
            self.pictures += 1;
            self.last = on_screen;
        }
        self.exact += exact as u32;
        let off = on_screen.map_or(want, |i| i.abs_diff(want));
        self.behind += off;
        self.worst = self.worst.max(off);
        if since.elapsed().as_secs_f64() >= 1.0 {
            trace!(
                "scrubbing: {} new pictures/s, {} of {} redraws exact, behind by {:.1} frames on average (at most {}), showing a picture took {:.2} ms (at most {:.2})",
                self.pictures,
                self.exact,
                self.redraws,
                self.behind as f64 / self.redraws as f64,
                self.worst,
                self.showing * 1000.0 / self.pictures.max(1) as f64,
                self.showing_max * 1000.0
            );
            *self = Self { last: self.last, ..Self::default() };
        }
    }
}

impl Player {
    /// `pcm` track `i` is source track `i` as interleaved stereo at
    /// [`PREVIEW_RATE`]; it may still be filling in (it plays as it's decoded).
    pub fn new(ctx: &egui::Context, source: &Path, info: ClipInfo, pcm: Arc<Pcm>, gains: Vec<TrackEdit>) -> Self {
        Self::with_pcm(ctx, source, info, pcm, gains, true)
    }

    /// A player that only plays (a card's preview in the library): no
    /// scrub engine, whose decoders and kept frames would go unused.
    pub fn preview(ctx: &egui::Context, source: &Path, info: ClipInfo, pcm: Arc<Pcm>, gains: Vec<TrackEdit>) -> Self {
        Self::with_pcm(ctx, source, info, pcm, gains, false)
    }

    fn with_pcm(ctx: &egui::Context, source: &Path, info: ClipInfo, pcm: Arc<Pcm>, gains: Vec<TrackEdit>, scrubs: bool) -> Self {
        let (shared, stream, audio_error) = sound(pcm, gains);

        let (still_tx, still_rx) = still_worker(ctx, source, nv12_size(&info));

        let video = open_video(ctx, source, info.fps);
        let proxy = video.is_none().then(|| Proxy::build(ctx, source, info.fps, total_frames(&info)));
        // `HASTECLIP_SCRUB_ENGINE=0`: scrub through the playback decoder
        // instead, to compare (as it does where the engine can't start).
        #[cfg(hw_decode)]
        let scrub_opening = video.as_ref().filter(|_| scrubs && std::env::var_os("HASTECLIP_SCRUB_ENGINE").is_none_or(|v| v != "0")).map(|_| {
            let (tx, rx) = mpsc::channel();
            let (ctx, source, fps, total) = (ctx.clone(), source.to_path_buf(), info.fps, total_frames(&info) as u64);
            let opened = std::thread::Builder::new().name("scrub engine".into()).spawn(move || {
                // Gone already (the clip closed): the engine stops as it drops.
                if tx.send(crate::scrub::Engine::open(&ctx, &source, fps, total)).is_ok() {
                    ctx.request_repaint();
                }
            });
            if let Err(e) = opened {
                eprintln!("scrubbing through the playback decoder: no thread for the scrub engine ({e})");
            }
            rx
        });
        #[cfg(not(hw_decode))]
        let _ = scrubs;
        Self {
            source: source.to_path_buf(),
            info,
            shared,
            _stream: stream,
            audio_error,
            texture: None,
            glow: None,
            display: None,
            on_screen: None,
            retired: Default::default(),
            #[cfg(hw_decode)]
            gpu_frames: Default::default(),
            uploads: Default::default(),
            shown_frame: None,
            decoder: None,
            after_cut: None,
            still_tx,
            still_rx,
            still_wanted: None,
            proxy,
            scrub_asked: None,
            showing_proxy: false,
            stand_in: None,
            video,
            #[cfg(hw_decode)]
            scrub: None,
            #[cfg(hw_decode)]
            scrub_opening,
            #[cfg(hw_decode)]
            motion: None,
            #[cfg(hw_decode)]
            redraw_secs: 1.0 / 240.0,
            #[cfg(all(hw_decode, target_os = "linux"))]
            uploader: None,
            #[cfg(hw_decode)]
            stood_in_for: None,
            #[cfg(hw_decode)]
            catching_up: false,
            pending_wait: START_WAIT,
            landing: None,
            pending_start: None,
            video_playing: false,
            played_at: None,
            pacing: Pacing::default(),
            asked_at: None,
            screen_index: None,
            scrub_stats: ScrubStats::default(),
            stepping: false,
        }
    }

    /// The clip's file was renamed to `to`. What has it open keeps it open
    /// (Windows lets an open file be renamed): the sound, the video decoder,
    /// the scrub preview. Only what opens it again by name follows: nothing
    /// reloads, nothing stops.
    pub fn renamed(&mut self, ctx: &egui::Context, to: &Path) {
        self.source = to.to_path_buf();
        (self.still_tx, self.still_rx) = still_worker(ctx, to, nv12_size(&self.info));
    }

    /// Take over the other screen's player (see `handoff.rs`), if it plays
    /// `source` (a clip never edited: the player and the editor play the
    /// same file): it plays on, with `pcm`'s tracks mixed by `gains`; speed,
    /// scrubbing and cuts start afresh. Its file is as it was: the handoff is
    /// dropped when the clip's files change.
    pub fn adopt(&mut self, source: &Path, pcm: Arc<Pcm>, gains: Vec<TrackEdit>) -> bool {
        if self.source != source {
            return false;
        }
        let n = pcm.len();
        if let Ok(mut playing) = self.shared.pcm.lock() {
            *playing = pcm;
        }
        if let Ok(mut mix) = self.shared.mix.lock() {
            mix.tracks = gains;
            mix.track_levels = vec![(0.0, 0.0); n];
        }
        self.shared.speed.store(1.0f32.to_bits(), Ordering::Relaxed);
        self.shared.scrubbing.store(false, Ordering::Relaxed);
        self.landing = None;
        self.stepping = false;
        self.set_cuts(&[]);
        // Hushed for a curtain and handed straight back (a quick switch
        // back): its sound comes back. The sound is what moves the clock;
        // without it, it would stand still, saying it plays.
        if self._stream.is_none() {
            (self._stream, self.audio_error) = match start_audio(self.shared.clone()) {
                Ok(s) => (Some(s), None),
                Err(e) => (None, Some(e)),
            };
        }
        true
    }

    /// Pause and let go of the sound output: a curtain only shows its
    /// picture, while the player taking its place starts.
    pub fn hush(&mut self) {
        self.pause();
        self._stream = None;
        self.stepping = false;
    }

    /// The player for `source`: `handoff` (the other screen's) taken over
    /// if it can be (see [`Player::adopt`]), else a new one, with the
    /// handoff hushed into its curtain if it has a picture to show. Says
    /// whether it was taken over: then it's still where it was, and playing
    /// if it was.
    pub fn handed_over(
        ctx: &egui::Context,
        source: &Path,
        info: &ClipInfo,
        pcm: Arc<Pcm>,
        gains: Vec<TrackEdit>,
        handoff: Option<Self>,
    ) -> (Self, Option<Self>, bool) {
        let mut handoff = handoff;
        let taken = handoff.as_mut().is_some_and(|p| p.adopt(source, pcm.clone(), gains.clone()));
        if taken {
            return (handoff.expect("just taken over"), None, true);
        }
        let curtain = handoff.filter(|p| p.size().is_some()).map(|mut p| {
            p.hush();
            p
        });
        (Self::new(ctx, source, info.clone(), pcm, gains), curtain, false)
    }

    /// The picture's size, once there's one on screen.
    pub fn size(&self) -> Option<egui::Vec2> {
        self.display.as_ref().map(Display::size)
    }

    pub fn time(&self) -> f64 {
        self.shared.now()
    }

    /// Playing, or about to (waiting for the first frame).
    pub fn is_playing(&self) -> bool {
        self.sound_playing() || self.pending_start.is_some()
    }

    fn sound_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    /// The hardware decoder, unless it's failed (then the ffmpeg path runs).
    fn hw(&self) -> Option<&Video> {
        self.video.as_ref().filter(|v| !v.failed())
    }

    /// Jump to `t` (seconds). Keeps playing if it was.
    pub fn seek(&mut self, t: f64) {
        let t = t.clamp(0.0, self.info.duration);
        self.shared.pos.store(to_samples(t), Ordering::Relaxed);
        self.decoder = None; // restarts from the new position if playing
        self.after_cut = None;
        if self.is_playing() {
            if let Some(v) = self.hw() {
                v.play(self.info.frame_index(t));
            }
        }
    }

    /// Play from the current position until `until` seconds.
    pub fn play(&mut self, until: f64) {
        trace!("play at frame {} (showing {:?}{})", self.info.frame_index(self.time()), self.shown_frame, if self.showing_proxy { ", proxy" } else { "" });
        self.played_at = Some(std::time::Instant::now());
        self.shared.end.store((until * PREVIEW_RATE as f64) as u64, Ordering::Relaxed);
        // The playback decoder still on its way to the playhead (scrubbing
        // left it elsewhere, or it's decoding the frame asked for): play
        // waits for it.
        #[cfg(hw_decode)]
        let behind = std::mem::take(&mut self.catching_up);
        #[cfg(not(hw_decode))]
        let behind = false;
        // Asked for this frame, it decodes the next few after it too.
        let asked = std::mem::take(&mut self.still_wanted);
        self.pending_wait = START_WAIT;
        if let Some(v) = self.hw() {
            // Showing the frame under the playhead already: carry on from the
            // next, which was decoded while paused. Otherwise wait for this one.
            let want = self.info.frame_index(self.time());
            let showing = self.shown_frame == Some(want) && !self.showing_proxy;
            v.play(if showing && (!behind || asked == Some(want)) { want + 1 } else { want });
            self.video_playing = true;
            if !showing || behind {
                #[cfg(hw_decode)]
                if behind {
                    self.pending_wait = CATCH_UP_WAIT;
                }
                self.pending_start = Some(std::time::Instant::now());
                return;
            }
        } else {
            // ffmpeg's frames: the sound waits for the first (`update`
            // starts it). Starting together, the picture stood still while
            // ffmpeg started, then jumped ahead.
            self.pending_start = Some(std::time::Instant::now());
            return;
        }
        self.shared.playing.store(true, Ordering::Relaxed);
    }

    /// Play until `until` seconds: playing already (taken over from the
    /// other screen), it carries on, only stopping there instead.
    pub fn play_on(&mut self, until: f64) {
        if self.is_playing() {
            self.shared.end.store((until * PREVIEW_RATE as f64) as u64, Ordering::Relaxed);
        } else {
            self.play(until);
        }
    }

    /// How fast it plays: 1.0 as recorded, 0.5 half speed.
    pub fn set_speed(&self, speed: f32) {
        self.shared.speed.store(speed.clamp(0.1, 4.0).to_bits(), Ordering::Relaxed);
    }

    pub fn pause(&mut self) {
        // Stopped where the picture is, the smooth playhead (`Shared::now`),
        // not up to a buffer ahead where the callback last put it: the frame
        // on screen stays.
        let at = self.sound_playing().then(|| (self.time() * PREVIEW_RATE as f64) as u64);
        self.shared.playing.store(false, Ordering::Relaxed);
        if let Some(at) = at {
            self.shared.pos.store(at, Ordering::Relaxed);
        }
        self.pending_start = None;
        self.decoder = None;
        self.after_cut = None;
        // Only when it was playing: the decoder acts on its newest command, so
        // a needless pause right after asking for a frame would cancel that.
        if self.video_playing {
            if let Some(v) = self.hw() {
                v.pause();
            }
        }
        self.video_playing = false;
    }

    /// For the `n` steps of `from..to` seconds: which have scrub frames.
    pub fn proxy_built(&self, from: f64, to: f64, n: usize) -> Vec<bool> {
        match &self.proxy {
            Some(p) => p.built(self.info.frame_index(from), self.info.frame_index(to), n),
            None => vec![true; n],
        }
    }

    /// Fraction of the scrub proxy that's built (0..=1); 1 without one.
    pub fn proxy_progress(&self) -> f32 {
        let Some(proxy) = &self.proxy else { return 1.0 };
        (proxy.ready() as f64 / total_frames(&self.info).max(1) as f64).min(1.0) as f32
    }

    /// The small scrub frame `idx`, if there's a scrub proxy and it's been
    /// built yet (for hover previews).
    pub fn proxy_frame(&self, idx: u64) -> Option<egui::ColorImage> {
        self.proxy.as_ref()?.frame(idx)
    }

    /// What's kept of the clip: these parts (in order, apart, as
    /// `media::Edit::kept` has them). Playing jumps over what's cut out
    /// between them, the sound crossfading at each join as the saved clip's
    /// does.
    pub fn set_cuts(&mut self, kept: &[(f64, f64)]) {
        let fades = media::join_fades(kept);
        let skips: Vec<Skip> = kept
            .windows(2)
            .zip(&fades)
            .map(|(w, f)| Skip { from: to_samples(w[0].1), to: to_samples(w[1].0), fade: (f * PREVIEW_RATE as f64) as u64 })
            .collect();
        let Ok(mut now) = self.shared.skips.lock() else { return };
        if *now == skips {
            return;
        }
        // Playing in what's just been cut out: on from its end at once, the
        // picture too (read on to there, it would stand still meanwhile).
        let pos = self.shared.pos.load(Ordering::Relaxed);
        let into = skips.iter().find(|s| (s.from..s.to).contains(&pos)).map(|s| s.to as f64 / PREVIEW_RATE as f64);
        *now = skips;
        drop(now);
        // Frames read ahead from after a cut may be for one that's gone.
        self.after_cut = None;
        if let Some(v) = self.hw() {
            v.set_skips(kept.windows(2).map(|w| (self.info.frame_index(w[0].1), self.info.frame_index(w[1].0))).collect());
        }
        if let Some(to) = into.filter(|_| self.is_playing()) {
            self.seek(to);
        }
    }

    /// The next stretch cut out after `t` seconds, if one is.
    fn next_cut(&self, t: f64) -> Option<Skip> {
        let pos = to_samples(t);
        self.shared.skips.lock().ok()?.iter().find(|s| s.from > pos).copied()
    }

    pub fn set_mix(&self, gains: Vec<TrackEdit>) {
        if let Ok(mut m) = self.shared.mix.lock() {
            m.tracks = gains;
        }
    }

    /// How loud you listen, 0 (muted) to 1: the mix as it is, quieter.
    pub fn set_listening(&self, gain: f32) {
        self.shared.listen.store(gain.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    /// Current meter readings: per track, then master.
    pub fn levels(&self) -> (Vec<(f32, f32)>, (f32, f32)) {
        match self.shared.mix.lock() {
            Ok(m) => (m.track_levels.clone(), m.master_level),
            Err(_) => (Vec::new(), (0.0, 0.0)),
        }
    }

    /// The master's level per channel (left, right), for a stereo meter.
    pub fn master_stereo(&self) -> capture::mixer::Levels {
        self.shared.mix.lock().map(|m| m.master_stereo).unwrap_or_default()
    }

    /// Bring the displayed frame in line with the clock. Call once per UI frame.
    /// `scrubbing`: the user is dragging, so favour instant proxy frames.
    /// Returns the size of what's on screen (in pixels, to fit it); draw it
    /// with [`Self::paint`].
    pub fn update(&mut self, ctx: &egui::Context, scrubbing: bool) -> Option<egui::Vec2> {
        // The hardware decoder broke (or can't read this file): scrub from a
        // proxy after all.
        if self.proxy.is_none() && self.hw().is_none() {
            self.proxy = Some(Proxy::build(ctx, &self.source, self.info.fps, total_frames(&self.info)));
            #[cfg(hw_decode)]
            {
                #[cfg(target_os = "linux")]
                {
                    self.uploader = None;
                }
                self.scrub = None;
                self.scrub_opening = None;
            }
        }
        if let Some(proxy) = &self.proxy {
            proxy.set_focus(self.info.frame_index(self.time()));
        }
        self.shared.scrubbing.store(scrubbing && !self.sound_playing(), Ordering::Relaxed);
        #[cfg(hw_decode)]
        if scrubbing || self.stepping {
            crate::video::scrubbing();
        }
        if self.hw().is_some() {
            // Each step back on its own decodes from the keyframe before it
            // (up to a second of frames): held, it fell behind and froze
            // until the steps came near a keyframe again.
            let scrubbing = scrubbing || self.stepping;
            self.update_hw(ctx, scrubbing);
            self.note_scrub(scrubbing);
            return self.display.as_ref().map(Display::size);
        }
        let t = self.time();
        let want = self.info.frame_index(t);

        if !self.is_playing() {
            // Stopped by itself at the out point: the stream is now stale.
            self.decoder = None;
            self.after_cut = None;
        }
        if self.is_playing() {
            // Over a cut: on with the frames read from after it.
            if self.after_cut.as_ref().is_some_and(|(from, _)| want >= *from) {
                self.decoder = self.after_cut.take().map(|(_, s)| s);
            }
            if self.decoder.is_none() {
                self.decoder = FrameStream::start(ctx, &self.source, &self.info, t).ok();
            }
            // Nearing a cut: start reading from after it, so its first frame
            // is there when the playhead jumps (ffmpeg takes a moment to
            // start, and reading on through what's cut out takes longer).
            if let Some(cut) = self.next_cut(t).filter(|c| c.from as f64 / PREVIEW_RATE as f64 - t < 1.0) {
                let at = cut.to as f64 / PREVIEW_RATE as f64;
                let from = self.info.frame_index(at);
                if self.after_cut.as_ref().is_none_or(|(f, _)| *f != from) {
                    self.after_cut = FrameStream::start(ctx, &self.source, &self.info, at).ok().map(|s| (from, s));
                }
            }
            // Waiting to start: the sound goes once the first frame is here.
            if let Some(since) = self.pending_start {
                let ready = self.decoder.as_mut().is_none_or(|d| d.peek().is_some());
                if ready || since.elapsed() >= FFMPEG_START_WAIT {
                    trace!("sound starts after {:.0} ms (frame ready: {ready})", since.elapsed().as_secs_f64() * 1000.0);
                    self.pending_start = None;
                    self.shared.playing.store(true, Ordering::Relaxed);
                }
            }
            if let Some(dec) = &mut self.decoder {
                // Drop frames we're already past; show the newest one not in the future.
                let mut best = None;
                while let Some((idx, _)) = dec.peek() {
                    if idx > want {
                        break;
                    }
                    best = dec.next();
                }
                if let Some((idx, frame)) = best {
                    if debug() && self.sound_playing() {
                        if let Some(line) = self.pacing.frame(idx, self.info.fps) {
                            trace!("playing: {line}, now {idx} with the clock at {want}");
                        }
                    }
                    self.show(ctx, idx, frame);
                }
            }
            if self.sound_playing() {
                self.repaint_at_next_frame(ctx, want);
            } else {
                ctx.request_repaint();
            }
        } else {
            while let Ok((idx, frame)) = self.still_rx.try_recv() {
                if Some(idx) == self.still_wanted && idx == want {
                    self.show(ctx, idx, frame);
                }
            }
            // While dragging, show the proxy frame right away (every mouse move).
            if self.shown_frame != Some(want) || (!scrubbing && self.showing_proxy) {
                if self.shown_frame != Some(want) {
                    if let Some(img) = self.proxy.as_ref().and_then(|p| p.frame(want)) {
                        self.set_texture(ctx, img);
                        self.shown_frame = Some(want);
                        self.screen_index = Some(want);
                        self.showing_proxy = true;
                        self.stand_in = None;
                        // Coming back to a frame sharpened before asks again.
                        self.still_wanted = None;
                    } else if scrubbing {
                        self.show_stand_in(ctx, want);
                    }
                }
                // Once the drag stops, sharpen to the full-quality exact frame.
                if !scrubbing && self.still_wanted != Some(want) {
                    self.still_wanted = Some(want);
                    let _ = self.still_tx.send((want, t));
                }
            }
        }
        self.note_scrub(scrubbing);
        self.size()
    }

    /// A key held to step back frame by frame (see `stepping`).
    pub fn set_stepping(&mut self, on: bool) {
        self.stepping = on;
    }

    /// Debug log: how the picture keeps up while scrubbing.
    fn note_scrub(&mut self, scrubbing: bool) {
        if !debug() {
            return;
        }
        if !scrubbing || self.sound_playing() {
            self.scrub_stats = ScrubStats::default();
            return;
        }
        let want = self.info.frame_index(self.time());
        let exact = self.shown_frame == Some(want);
        self.scrub_stats.note(want, self.screen_index, exact);
    }

    /// Playing, showing frame `want`: draw again when the next one is due,
    /// not on every display refresh (a 240 Hz screen redrew the whole page
    /// four times per frame of a 60 fps clip). A frame that's late is
    /// drawn when the decoder hands it over (it wakes the UI then).
    fn repaint_at_next_frame(&self, ctx: &egui::Context, want: u64) {
        let next = (want + 1) as f64 / self.info.fps.max(1.0);
        crate::ui_kit::repaint_after(ctx, self.shared.until(next));
    }

    /// Draw the picture into `rect`, cut to `painter`'s clip: a decoded frame
    /// through the sharp filter (`gpu_frames.rs`), anything else as an image.
    pub fn paint(&self, painter: &egui::Painter, rect: egui::Rect) {
        match &self.display {
            Some(Display::Image(id, _)) => {
                use egui::emath::GuiRounding as _;
                let uv = egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0));
                painter.image(*id, rect.round_to_pixels(painter.pixels_per_point()), uv, egui::Color32::WHITE);
            }
            Some(Display::Gpu(shown, _)) => shown.paint(painter, rect),
            None => {}
        }
    }

    /// The picture's colours, blurred very wide, over `cover` (clipped to
    /// `painter`): the light the stage around it takes. `id`: which stage.
    pub fn paint_glow(&self, painter: &egui::Painter, cover: egui::Rect, id: egui::Id) -> bool {
        // Dev aid: `HASTECLIP_DEV_NO_GLOW=1` draws no light, to measure its cost.
        if dev_flag("HASTECLIP_DEV_NO_GLOW") {
            return false;
        }
        match &self.display {
            Some(Display::Image(..)) => match &self.glow {
                Some(tex) => {
                    painter.image(tex.id(), cover, egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
                    true
                }
                None => false,
            },
            Some(Display::Gpu(shown, _)) => {
                shown.paint_glow(painter, cover, id);
                true
            }
            None => false,
        }
    }

    /// [`Self::update`] with the hardware decoder.
    /// `scrubbing`: a drag or wheel scrub is under way (proxy frames only).
    fn update_hw(&mut self, ctx: &egui::Context, scrubbing: bool) {
        let Some(v) = self.video.as_ref() else { return };
        let want = self.info.frame_index(self.time());
        #[cfg(hw_decode)]
        {
            // The engine, once it's open.
            if let Some(rx) = &self.scrub_opening {
                match rx.try_recv() {
                    Ok(engine) => {
                        self.scrub = engine;
                        self.scrub_opening = None;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => self.scrub_opening = None,
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
            // Its decoders gone: its frames and textures go too (scrubbing
            // carries on through the playback decoder).
            if self.scrub.as_ref().is_some_and(|e| e.failed()) {
                #[cfg(target_os = "linux")]
                {
                    self.uploader = None;
                }
                self.scrub = None;
            }
            if let Some(engine) = self.scrub.as_ref().filter(|_| self.is_playing()) {
                engine.rest(want);
            }
            // Nothing to upload ahead unless scrubbing (playing on from a
            // scrub, it kept going).
            #[cfg(target_os = "linux")]
            if let Some(u) = self.uploader.as_ref().filter(|_| self.is_playing() || !scrubbing) {
                u.rest();
            }
        }
        // Waiting to start: the sound goes once the first frame is here.
        if let Some(since) = self.pending_start {
            // The wait counts from when the decoder opened (a few hundred ms
            // after a clip opens), so picture and sound start together then
            // too; at most 1.5 s in all.
            let waited = v.open_for().map_or(std::time::Duration::ZERO, |open| open.min(since.elapsed()));
            if v.has(want) || waited >= self.pending_wait || since.elapsed() >= std::time::Duration::from_millis(1500) {
                trace!("sound starts after {:.0} ms (frame ready: {})", since.elapsed().as_secs_f64() * 1000.0, v.has(want));
                self.pending_start = None;
                self.shared.playing.store(true, Ordering::Relaxed);
            }
            if let Some(p) = v.take_upto(want) {
                self.show_picture(ctx, p);
            }
            ctx.request_repaint();
            return;
        }
        if self.sound_playing() {
            if debug() {
                static LAST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
                let mut last = LAST.lock().unwrap();
                static PASSES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                let passes = PASSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if last.is_none_or(|t| t.elapsed().as_secs_f64() >= 1.0) {
                    *last = Some(std::time::Instant::now());
                    PASSES.store(0, std::sync::atomic::Ordering::Relaxed);
                    #[cfg(not(hw_decode))]
                    let _ = passes;
                    #[cfg(hw_decode)]
                    trace!("playing: want {want}, showing {:?}, ahead {:?}, {passes} redraws/s, last frame {:.1} ms", self.shown_frame, v.ahead_info(), ctx.input(|i| i.unstable_dt) * 1000.0);
                }
            }
            if let Some(p) = v.take_upto(want) {
                // Kept for scrubbing back over it, at no cost.
                #[cfg(hw_decode)]
                if let Some(engine) = &self.scrub {
                    engine.offer(p.clone());
                }
                self.show_picture(ctx, p);
            }
            self.repaint_at_next_frame(ctx, want);
            return;
        }
        // Stopped by itself at the out point: so does the decoder.
        if self.video_playing {
            v.pause();
            self.video_playing = false;
            self.still_wanted = None;
        }
        // Scrubbing with the scrub engine: the picture at the playhead (or
        // the nearest decoded) out of its cache, on this very redraw. Paused,
        // it decodes around the playhead for the next drag.
        #[cfg(hw_decode)]
        if self.scrub.as_ref().is_some_and(|e| !e.failed()) {
            if scrubbing {
                self.scrub_engine(ctx, want);
                return;
            }
            // Not (or no longer) scrubbing: the engine decodes around the
            // playhead for the next drag, and its frame shows if the engine
            // has it.
            let was_scrubbing = self.motion.take().is_some();
            self.stood_in_for = None;
            let found = {
                let engine = self.scrub.as_ref().expect("checked above");
                engine.idle_at(want);
                engine.frame_for(want)
            };
            let showed = match found.filter(|_| self.shown_frame != Some(want)) {
                Some((p, true)) => {
                    self.show_scrubbed(ctx, p, Some(want), 0.0);
                    true
                }
                _ => false,
            };
            // The playback decoder isn't where a scrub, or a jump the engine
            // showed at once, left the playhead: asked once, it decodes this
            // frame (on screen already) and the next few, ready for play.
            // (Not asked, play started the sound over the frame standing
            // still while the decoder went there from the keyframe before.)
            if (was_scrubbing || showed) && self.still_wanted != Some(want) {
                self.ask_exact(want);
            }
        }
        // Scrubbing: what the decoder found for the newest frame asked (the
        // keyframe on the way there, or the frame itself); then the exact
        // frame asked once paused, which is newer.
        let v = self.video.as_ref().expect("checked above");
        let (found, exact) = (v.take_scrub(), v.take_exact(want));
        if let Some(found) = found {
            // Kept small while scrubbing back: sharp again once it stops.
            let small = found.picture.width < self.info.width;
            self.put_picture(ctx, found.picture);
            self.showing_proxy = small;
            // The picture for the frame asked even when its own number is
            // lower (a dropped frame's slot).
            if found.exact {
                self.shown_frame = Some(found.asked);
            }
        }
        if let Some(p) = exact {
            if let Some(at) = self.asked_at.take() {
                trace!("sharp frame {want} (picture {}) {:.0} ms after asking", p.index, at.elapsed().as_secs_f64() * 1000.0);
            }
            #[cfg(hw_decode)]
            {
                self.catching_up = false;
            }
            // On screen already (the scrub engine had it): nothing to redraw.
            if self.screen_index != Some(p.index) || self.showing_proxy {
                self.show_picture(ctx, p);
            }
            // It's the picture for `want` even when its own number is lower (a
            // dropped frame's slot).
            self.shown_frame = Some(want);
        }
        // The frame on screen; small is enough while scrubbing.
        if self.shown_frame == Some(want) && (scrubbing || !self.showing_proxy) {
            return;
        }
        let v = self.video.as_ref().expect("checked above");
        if scrubbing {
            // Every frame the playhead moves to (the decoder takes the newest).
            if self.scrub_asked != Some(want) {
                self.scrub_asked = Some(want);
                // Stopping here again asks for the exact frame again.
                self.still_wanted = None;
                v.scrub(want, self.screen_index);
            }
        } else if self.still_wanted != Some(want) {
            // Paused somewhere new, or a scrub stopped (the mouse let go, the
            // wheel settled) before its frame came: that frame, exactly.
            // Asked once per frame wanted.
            self.ask_exact(want);
        }
    }

    /// Ask the playback decoder for frame `want`, exactly (asked once per
    /// frame wanted): it decodes it with all its threads, then the next few
    /// frames, ready for play.
    fn ask_exact(&mut self, want: u64) {
        self.still_wanted = Some(want);
        self.scrub_asked = None;
        self.asked_at = Some(std::time::Instant::now());
        // On its way there until the frame comes back (`take_exact`).
        #[cfg(hw_decode)]
        {
            self.catching_up = true;
        }
        if let Some(v) = self.video.as_ref() {
            v.show(want);
        }
    }

    /// Scrubbing with the scrub engine, the playhead at frame `want`: its
    /// picture out of the engine's cache, on this very redraw.
    ///
    /// Moving, it's the playhead's own frame if it's decoded, else the
    /// nearest that is: one per frame the playhead is at, then only its own
    /// (each frame decoded on the way to it showed as it came, nearer and
    /// nearer: stopped, the picture played on a few frames).
    ///
    /// Held still, the engine decodes around it. Scrubbing the playhead
    /// itself, a picture near it stays put (letting go lands on it,
    /// [`Self::land`]); otherwise its own frame comes in one step, the
    /// engine's or the playback decoder's (all its threads on the one
    /// frame: soonest).
    #[cfg(hw_decode)]
    fn scrub_engine(&mut self, ctx: &egui::Context, want: u64) {
        let m = self.motion_at(want);
        let held = m.moved.elapsed() >= HELD;
        let speed = if held { 0.0 } else { m.speed };
        let found = {
            let engine = self.scrub.as_ref().expect("the scrub engine");
            engine.scrub_to(want, speed, held || m.jumped);
            engine.frame_for(want)
        };
        #[cfg(target_os = "linux")]
        if let Some(u) = &self.uploader {
            u.steer(want, speed, self.redraw_secs);
        }
        if !held {
            self.catching_up = true;
            // Stopping here asks for its frame again.
            self.still_wanted = None;
            // Drawn again once it's been still a moment, to see that it's
            // stopped (a pointer held still sends nothing).
            ctx.request_repaint_after(HELD.saturating_sub(m.moved.elapsed()) + std::time::Duration::from_millis(2));
        }
        if self.shown_frame == Some(want) {
            return;
        }
        let fps = self.info.fps;
        let keep = held && !m.jumped && self.landing.zip(self.screen_index).is_some_and(|(reach, i)| i.abs_diff(want) as f64 <= reach * fps);
        if keep {
            return;
        }
        match found {
            Some((p, true)) => {
                self.show_scrubbed(ctx, p, Some(want), speed);
                return;
            }
            Some((p, false)) if self.stood_in_for != Some(want) => {
                if self.screen_index != Some(p.index) {
                    self.show_scrubbed(ctx, p, None, speed);
                }
                self.stood_in_for = Some(want);
            }
            _ => {}
        }
        if held {
            match self.video.as_ref().and_then(|v| v.take_exact(want)) {
                Some(p) => {
                    if let Some(at) = self.asked_at.take() {
                        trace!("sharp frame {want} (picture {}) {:.0} ms after asking, held still", p.index, at.elapsed().as_secs_f64() * 1000.0);
                    }
                    self.catching_up = false;
                    self.show_picture(ctx, p);
                    self.shown_frame = Some(want);
                }
                None if self.still_wanted != Some(want) => self.ask_exact(want),
                None => {}
            }
        }
    }

    /// Put picture `p` from the scrub engine on screen. Linux: from the
    /// uploader's textures if it's uploaded already (or one within half a
    /// redraw's travel at `speed` that's nearer than what's on screen), else
    /// uploaded now. Elsewhere it's on the GPU already, drawn from where it
    /// is. `exact_for`: the playhead frame it's the picture of (when it's
    /// that, a dropped frame's slot showing the frame before).
    #[cfg(hw_decode)]
    fn show_scrubbed(&mut self, ctx: &egui::Context, p: Picture, exact_for: Option<u64>, speed: f64) {
        let t = std::time::Instant::now();
        let wanted = p.index;
        #[cfg(target_os = "linux")]
        let got = {
            if self.uploader.is_none() && !dev_flag("HASTECLIP_DEV_NO_UPLOADER") {
                self.uploader = self.scrub.as_ref().and_then(|e| crate::scrub::Uploader::new(e, p.width, p.height));
            }
            let near = (speed.abs() * self.redraw_secs / 2.0).floor() as u64;
            let ready = self.uploader.as_ref().filter(|u| u.size() == (p.width, p.height)).and_then(|u| u.find(p.index, near, self.screen_index));
            match ready {
                Some((shown, idx)) => {
                    self.display = Some(Display::Gpu(shown, egui::vec2(p.width as f32, p.height as f32)));
                    self.retire(ctx, Some(p));
                    self.screen_index = Some(idx);
                    self.showing_proxy = false;
                    self.stand_in = None;
                    idx
                }
                None => {
                    self.put_picture(ctx, p);
                    wanted
                }
            }
        };
        #[cfg(not(target_os = "linux"))]
        let got = {
            let _ = speed;
            self.put_picture(ctx, p);
            wanted
        };
        if debug() {
            self.scrub_stats.showed(t.elapsed().as_secs_f64());
        }
        // One near it instead isn't the frame asked.
        self.shown_frame = exact_for.filter(|_| got == wanted);
    }

    /// The playhead's motion while scrubbing, it being at frame `want`
    /// now: how fast (smoothed over the last few redraws), and since when
    /// it's been where it is. Over ten seconds of video at once, or faster
    /// than 300× real time since it last moved, is a jump (a click), not
    /// motion: the speed starts over from nothing, then from the first
    /// step's own pace. (Judged by distance alone, a fast drag on a slow
    /// display moved over a second a redraw and counted as clicks.)
    #[cfg(hw_decode)]
    fn motion_at(&mut self, want: u64) -> Motion {
        /// Smoothing time (s): about a dozen redraws at 144 Hz.
        const TAU: f64 = 0.08;
        let now = std::time::Instant::now();
        let fps = self.info.fps;
        let m = match self.motion {
            // The first redraw of a scrub: where a press put it.
            None => Motion { at: want, seen: now, speed: 0.0, moved: now, jumped: true },
            Some(m) => {
                let dt = now.duration_since(m.seen).as_secs_f64();
                if dt > 0.0 && dt < 0.1 {
                    self.redraw_secs += (dt - self.redraw_secs) * 0.1;
                }
                let d = want as f64 - m.at as f64;
                let speed = if dt > 0.0 { m.speed + (d / dt - m.speed) * (1.0 - (-dt / TAU).exp()) } else { m.speed };
                let since = now.duration_since(m.moved).as_secs_f64().max(1e-3);
                if want == m.at {
                    Motion { seen: now, speed, ..m }
                } else if d.abs() > 10.0 * fps || d.abs() / since > 300.0 * fps {
                    Motion { at: want, seen: now, speed: 0.0, moved: now, jumped: true }
                } else {
                    // Moving off from where a click put it: at its own pace.
                    let speed = if m.jumped { d / since } else { speed };
                    Motion { at: want, seen: now, speed, moved: now, jumped: false }
                }
            }
        };
        self.motion = Some(m);
        m
    }

    /// While the playhead itself is scrubbed (`Some`), how near the
    /// playhead (seconds) the picture it's held still on may be to stay,
    /// and where letting go lands ([`Self::land`]). Set every frame, before
    /// [`Self::update`].
    pub fn set_landing(&mut self, reach: Option<f64>) {
        self.landing = reach;
    }

    /// A scrub of the playhead (a drag of it, the wheel) ended: when the
    /// picture on screen is a frame near the playhead (within the reach
    /// [`Self::set_landing`] gave) but not the playhead's own, the playhead
    /// goes to it. Letting go lands on the frame you see, rather than the
    /// picture jumping to the one under the pointer once it's decoded. Not
    /// after a jump (a click): that picture is only a stand-in for where it
    /// went. Nor into a stretch cut out of the clip.
    pub fn land(&mut self) {
        #[cfg(hw_decode)]
        {
            let (Some(reach), Some(on_screen)) = (self.landing, self.screen_index) else { return };
            if self.scrub.as_ref().is_none_or(|e| e.failed()) || self.motion.is_none_or(|m| m.jumped) {
                return;
            }
            let want = self.info.frame_index(self.time());
            if self.shown_frame == Some(want) || on_screen == want || on_screen.abs_diff(want) as f64 > reach * self.info.fps {
                return;
            }
            let t = self.info.snap(on_screen as f64 / self.info.fps + 1e-9) + 1e-6;
            let cut = self.shared.skips.lock().is_ok_and(|s| s.iter().any(|k| (k.from..k.to).contains(&to_samples(t))));
            if cut || t > self.info.duration {
                return;
            }
            trace!("scrub lands on the frame shown, {on_screen} (the playhead was at {want})");
            self.seek(t);
            self.shown_frame = Some(on_screen);
        }
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    fn show_picture(&mut self, ctx: &egui::Context, p: Picture) {
        let idx = p.index;
        let clock = self.info.frame_index(self.time());
        if idx + 30 < clock || idx > clock + 30 {
            trace!("WRONG FRAME on screen: {idx}, clock at {clock}");
        }
        if let Some(at) = self.played_at.take() {
            trace!("first frame after play: {idx}, {:.0} ms after pressing", at.elapsed().as_secs_f64() * 1000.0);
        }
        // Frames that change the picture, not frames handed over: how evenly
        // they come, each second.
        if debug() && self.sound_playing() && self.shown_frame != Some(idx) {
            let fps = self.info.fps;
            if let Some(line) = self.pacing.frame(idx, fps) {
                trace!("playing: {line}, now {idx} at {:.2} s", self.time());
            }
        }
        self.put_picture(ctx, p);
    }

    /// Put a decoded picture on screen.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn put_picture(&mut self, ctx: &egui::Context, p: Picture) {
        let idx = p.index;
        // Dev aid: `HASTECLIP_DEV_NO_UPLOAD=1` keeps the picture on screen
        // as it is, to measure what showing a new one costs.
        if dev_flag("HASTECLIP_DEV_NO_UPLOAD") {
            let _ = ctx;
            self.shown_frame = Some(idx);
            self.screen_index = Some(idx);
            self.showing_proxy = false;
            self.stand_in = None;
            return;
        }
        #[cfg(hw_decode)]
        {
            // On the GPU: draw its texture as it is.
            let on_gpu = p.gpu.as_ref().and_then(|s| self.gpu_frames.show(s, p.width, p.height));
            match on_gpu {
                Some(shown) => {
                    self.display = Some(Display::Gpu(shown, egui::vec2(p.width as f32, p.height as f32)));
                    self.retire(ctx, Some(p));
                }
                None if p.rgba.is_empty() => {}
                None => self.set_texture(ctx, crate::video::to_image(p)),
            }
        }
        #[cfg(not(hw_decode))]
        let _ = (ctx, p);
        self.shown_frame = Some(idx);
        self.screen_index = Some(idx);
        self.showing_proxy = false;
        self.stand_in = None;
    }

    /// Scrubbing where frame `want` isn't built yet: the nearest built frame
    /// within 10 s instead, so the picture keeps following (the frame itself
    /// replaces it as soon as it's built, and the sharp one once you stop).
    fn show_stand_in(&mut self, ctx: &egui::Context, want: u64) {
        let within = (self.info.fps * 10.0).round().max(1.0) as u64;
        let Some(i) = self.proxy.as_ref().and_then(|p| p.nearest(want, within)) else { return };
        if self.stand_in == Some(i) {
            return;
        }
        if let Some(img) = self.proxy.as_ref().and_then(|p| p.frame(i)) {
            self.set_texture(ctx, img);
            self.shown_frame = None;
            self.screen_index = Some(i);
            self.showing_proxy = true;
            self.stand_in = Some(i);
            trace!("scrubbing: frame {want} not built, showing {i}");
        }
    }

    fn show(&mut self, ctx: &egui::Context, idx: u64, frame: Decoded) {
        match frame {
            Decoded::Nv12(data) => {
                let (w, h) = (self.info.width, self.info.height);
                let Some(shown) = self.uploads.show(w, h, &data, self.info.bt601) else { return };
                self.display = Some(Display::Gpu(shown, egui::vec2(w as f32, h as f32)));
                self.retire(ctx, None);
            }
            Decoded::Rgba(f) => self.set_texture(ctx, egui::ColorImage::from_rgba_unmultiplied([f.width as usize, f.height as usize], &f.rgba)),
        }
        self.shown_frame = Some(idx);
        self.screen_index = Some(idx);
        self.showing_proxy = false;
        self.stand_in = None;
    }

    /// Put `next` on screen (or nothing), keeping the frame it replaces for a
    /// few more passes before its texture goes back to the decoder.
    fn retire(&mut self, ctx: &egui::Context, next: Option<Picture>) {
        /// Passes to hold a frame after it leaves the screen: more than the
        /// GPU ever runs behind.
        const HOLD: u64 = 4;
        let pass = ctx.cumulative_pass_nr();
        if let Some(old) = std::mem::replace(&mut self.on_screen, next) {
            self.retired.push_back((pass, old));
        }
        while self.retired.front().is_some_and(|(at, _)| pass >= at + HOLD) {
            self.retired.pop_front();
        }
    }

    fn set_texture(&mut self, ctx: &egui::Context, img: egui::ColorImage) {
        // The stage's light, made here for a picture that isn't on the GPU.
        let glow = crate::transport::glow_image(&img);
        match &mut self.glow {
            Some(tex) => tex.set(glow, egui::TextureOptions::LINEAR),
            None => self.glow = Some(ctx.load_texture("stage light", glow, egui::TextureOptions::LINEAR)),
        }
        match &mut self.texture {
            Some(tex) => tex.set(img, egui::TextureOptions::LINEAR),
            None => self.texture = Some(ctx.load_texture("preview", img, egui::TextureOptions::LINEAR)),
        }
        self.display = self.texture.as_ref().map(|t| Display::Image(t.id(), t.size_vec2()));
        self.retire(ctx, None);
    }
}

/// The video's size, when frames from ffmpeg come as NV12 at that size: when
/// they can be drawn with the sharp filter.
fn nv12_size(info: &ClipInfo) -> Option<(u32, u32)> {
    (crate::gpu_frames::can_upload() && info.width > 0 && info.height > 0).then_some((info.width, info.height))
}

/// On-demand exact stills (paused/scrubbing): one worker, the newest
/// request wins. It opens the file by name for each still. NV12 at `nv12`
/// (the video's size), else RGBA.
fn still_worker(ctx: &egui::Context, source: &Path, nv12: Option<(u32, u32)>) -> (Sender<(u64, f64)>, Receiver<(u64, Decoded)>) {
    let (still_tx, job_rx) = mpsc::channel::<(u64, f64)>();
    let (done_tx, still_rx) = mpsc::channel();
    let src = source.to_path_buf();
    let repaint = ctx.clone();
    std::thread::spawn(move || {
        while let Ok(mut job) = job_rx.recv() {
            while let Ok(newer) = job_rx.try_recv() {
                job = newer;
            }
            let frame = match nv12 {
                Some(size) => media::nv12_at(&src, job.1, size).map(Decoded::Nv12),
                None => media::frame_at(&src, job.1, PREVIEW_WIDTH).map(Decoded::Rgba),
            };
            if let Ok(frame) = frame {
                if done_tx.send((job.0, frame)).is_err() {
                    break;
                }
                repaint.request_repaint();
            }
        }
    });
    (still_tx, still_rx)
}

/// The in-process decoder for `source` (unless `HASTECLIP_NO_HW_DECODE` is
/// set, to compare with the ffmpeg path).
fn open_video(ctx: &egui::Context, source: &Path, fps: f64) -> Option<Video> {
    #[cfg(hw_decode)]
    if std::env::var_os("HASTECLIP_NO_HW_DECODE").is_none() {
        return Some(Video::open(ctx, source, fps));
    }
    let _ = (ctx, source, fps);
    None
}

/// Frames in the clip, as the app counts them.
fn total_frames(info: &ClipInfo) -> usize {
    (info.duration * info.fps).floor() as usize
}

/// The sound while scrubbing a paused clip, at its own pitch whatever the
/// speed (WSOLA): every [`Scrub::HOP`] it takes a [`Scrub::GRAIN`]-long slice
/// from where the playhead is right then, shifted by up to [`Scrub::SEARCH`]
/// to line up with how the last slice goes on, and cross-fades it in, the
/// fade's curves chosen by how alike the two are so the loudness holds (a
/// plain fade between unalike slices dips in the middle: at 67 dips a second
/// that sounded soft and buzzy). Moving
/// faster than real time skips through, slower overlaps; a jump just takes
/// the next slice from the new place. With the playhead still for
/// [`Scrub::STILL`] it fades out (else one slice would repeat as a buzz).
/// Runs in the audio callback: its buffers are allocated once.
struct Scrub {
    /// Output frames per second.
    rate: f64,
    /// Where the last slice started (source frames), if sounding.
    prev: Option<usize>,
    /// The second half of the last slice (unfaded), to overlap with the next.
    tail: Vec<(f32, f32)>,
    /// Made, not yet played (source rate).
    made: std::collections::VecDeque<(f32, f32)>,
    /// Two made samples the output is between, and how far (resampling).
    a: (f32, f32),
    b: (f32, f32),
    frac: f64,
    window: Vec<f32>,
    /// Mono copies for lining slices up.
    want: Vec<f32>,
    near: Vec<f32>,
    gains: Vec<f32>,
    track_peak: Vec<f32>,
    /// Loudness, eased (fades in and out).
    level: f64,
    /// The playhead last seen, and for how long (s) it hasn't moved.
    last_target: f64,
    still: f64,
}

impl Scrub {
    // Starting values, to tune by ear.
    /// Slice length (source frames): 40 ms, long enough to hear a sound as
    /// itself, short enough to follow the playhead.
    const GRAIN: usize = 1920;
    /// A new slice every half slice (20 ms), overlapping the last.
    const HOP: usize = Self::GRAIN / 2;
    /// How far a slice may shift (each way) to line up: 7.5 ms.
    const SEARCH: usize = 360;
    /// Samples compared when lining up.
    const MATCH: usize = 360;
    /// Fade out once the playhead has been still this long (s).
    const STILL: f64 = 0.08;
    /// Fade time (s).
    const FADE: f64 = 0.008;

    fn new(rate: f64) -> Self {
        let window = (0..Self::GRAIN).map(|k| (0.5 - 0.5 * (std::f64::consts::TAU * k as f64 / Self::GRAIN as f64).cos()) as f32).collect();
        Self {
            rate,
            prev: None,
            tail: vec![(0.0, 0.0); Self::HOP],
            made: std::collections::VecDeque::with_capacity(Self::GRAIN * 2),
            a: (0.0, 0.0),
            b: (0.0, 0.0),
            frac: 0.0,
            window,
            want: vec![0.0; Self::MATCH],
            near: vec![0.0; 2 * Self::SEARCH + Self::MATCH],
            gains: Vec::new(),
            track_peak: Vec::new(),
            level: 0.0,
            last_target: -1.0,
            still: 0.0,
        }
    }

    /// Whether there's anything to play: scrubbing, or still fading out.
    fn sounding(&self, scrubbing: bool) -> bool {
        scrubbing || self.level > 1e-4
    }

    /// Playing normally again: the next scrub starts afresh.
    fn reset(&mut self) {
        self.silence();
        self.last_target = -1.0;
    }

    /// Faded out: drop the slices (the next sound starts at its own place),
    /// but keep watching the playhead, so standing still stays quiet.
    fn silence(&mut self) {
        self.prev = None;
        self.tail.iter_mut().for_each(|t| *t = (0.0, 0.0));
        self.made.clear();
        self.a = (0.0, 0.0);
        self.b = (0.0, 0.0);
        self.level = 0.0;
    }

    /// Source frame `i` of every track, mixed with this slice's gains.
    fn sample(&self, pcm: &Pcm, i: usize) -> (f32, f32) {
        let (mut l, mut r) = (0.0, 0.0);
        for (t, track) in pcm.tracks().iter().enumerate() {
            if let Some((a, b)) = track.frame(i) {
                let g = self.gains.get(t).copied().unwrap_or(1.0);
                l += a * g;
                r += b * g;
            }
        }
        (l, r)
    }

    /// The next half slice into `made`, the slice taken near `target`.
    fn make(&mut self, pcm: &Pcm, tracks: &[TrackEdit], target: usize) {
        self.gains.clear();
        self.gains.extend(tracks.iter().map(|tr| tr.gain_at(target as f64 / PREVIEW_RATE as f64)));
        let start = match self.prev {
            None => target,
            Some(prev) => {
                // Where the last slice would go on, and the start near the
                // playhead that sounds most like it (cross-correlation).
                let natural = prev + Self::HOP;
                let lo = target.saturating_sub(Self::SEARCH);
                for k in 0..Self::MATCH {
                    let (l, r) = self.sample(pcm, natural + k);
                    self.want[k] = l + r;
                }
                for k in 0..self.near.len() {
                    let (l, r) = self.sample(pcm, lo + k);
                    self.near[k] = l + r;
                }
                let mut best = (f32::MIN, target);
                for off in (0..=2 * Self::SEARCH).step_by(2) {
                    let c: f32 = (0..Self::MATCH).step_by(2).map(|k| self.want[k] * self.near[off + k]).sum();
                    if c > best.0 {
                        best = (c, lo + off);
                    }
                }
                best.1
            }
        };
        let tracks_n = pcm.len();
        self.track_peak.resize(tracks_n, 0.0);
        // How alike the overlapping halves are (correlation, 0..1): alike
        // ones add up in step, unalike ones only in power, so the fade curves
        // are scaled to keep the loudness either way.
        let rho = if self.prev.is_some() {
            let (mut ab, mut aa, mut bb) = (0.0f64, 0.0f64, 0.0f64);
            for k in (0..Self::HOP).step_by(2) {
                let (l, r) = self.sample(pcm, start + k);
                let (x, y) = ((self.tail[k].0 + self.tail[k].1) as f64, (l + r) as f64);
                ab += x * y;
                aa += x * x;
                bb += y * y;
            }
            if aa > 1e-12 && bb > 1e-12 { (ab / (aa * bb).sqrt()).clamp(0.0, 1.0) as f32 } else { 1.0 }
        } else {
            1.0
        };
        for k in 0..Self::HOP {
            let (l, r) = self.sample(pcm, start + k);
            let (fade_in, fade_out) = (self.window[k], self.window[Self::HOP + k]);
            let norm = 1.0 / (fade_in * fade_in + fade_out * fade_out + 2.0 * rho * fade_in * fade_out).sqrt().max(1e-3);
            let t = self.tail[k];
            self.made.push_back(((t.0 * fade_out + l * fade_in) * norm, (t.1 * fade_out + r * fade_in) * norm));
            self.tail[k] = self.sample(pcm, start + Self::HOP + k);
        }
        for (t, track) in pcm.tracks().iter().enumerate() {
            let g = self.gains.get(t).copied().unwrap_or(1.0);
            let peak = (start..start + Self::HOP)
                .step_by(8)
                .filter_map(|i| track.frame(i))
                .map(|(l, r)| l.abs().max(r.abs()) * g)
                .fold(0.0f32, f32::max);
            self.track_peak[t] = peak;
        }
        self.prev = Some(start);
    }

    /// Fill `out` following the playhead at `target` (source frames); `active`
    /// false fades out. Returns the master's (peak, rms) and each track's.
    fn render(&mut self, out: &mut [f32], channels: usize, pcm: &Pcm, tracks: &[TrackEdit], target: f64, active: bool) -> (f32, f32, Vec<(f32, f32)>) {
        let frames = out.len() / channels.max(1);
        if (target - self.last_target).abs() >= 1.0 {
            self.still = 0.0;
        } else {
            self.still += frames as f64 / self.rate;
        }
        self.last_target = target;
        let want_level = if active && self.still < Self::STILL { 1.0 } else { 0.0 };
        if want_level == 0.0 && self.level < 1e-4 {
            // Quiet and staying so: nothing to make.
            self.silence();
            out.fill(0.0);
            return (0.0, 0.0, vec![(0.0, 0.0); pcm.len()]);
        }
        let ease = 1.0 / (Self::FADE * self.rate);
        let step = PREVIEW_RATE as f64 / self.rate;
        let (mut peak, mut sq) = (0.0f32, 0.0f64);
        for frame in out.chunks_mut(channels) {
            self.frac += step;
            while self.frac >= 1.0 {
                self.frac -= 1.0;
                if self.made.is_empty() {
                    self.make(pcm, tracks, target as usize);
                }
                self.a = self.b;
                self.b = self.made.pop_front().unwrap_or((0.0, 0.0));
            }
            self.level += (want_level - self.level) * ease;
            let f = self.frac as f32;
            let g = self.level as f32;
            let l = (self.a.0 + (self.b.0 - self.a.0) * f) * g;
            let r = (self.a.1 + (self.b.1 - self.a.1) * f) * g;
            let m = l.abs().max(r.abs());
            peak = peak.max(m);
            sq += (m as f64).powi(2);
            match channels {
                1 => frame[0] = (l + r) * 0.5,
                _ => {
                    frame[0] = l;
                    frame[1] = r;
                }
            }
        }
        if self.level < 1e-4 && want_level == 0.0 {
            self.silence();
        }
        let rms = if frames == 0 { 0.0 } else { (sq / frames as f64).sqrt() as f32 };
        let g = self.level as f32;
        let levels = self.track_peak.iter().map(|&p| (p * g, p * g * 0.7)).collect();
        (peak, rms, levels)
    }
}

/// What the audio callback and the player share, for `pcm`'s tracks mixed
/// by `gains`, and the sound output playing it (or why there's none).
fn sound(pcm: Arc<Pcm>, gains: Vec<TrackEdit>) -> (Arc<Shared>, Option<cpal::Stream>, Option<String>) {
    let n = pcm.len();
    let shared = Arc::new(Shared::new(Mix { tracks: gains, track_levels: vec![(0.0, 0.0); n], master_level: (0.0, 0.0), master_stereo: Default::default() }, pcm));
    let (stream, audio_error) = match start_audio(shared.clone()) {
        Ok(s) => (Some(s), None),
        Err(e) => (None, Some(e)),
    };
    (shared, stream, audio_error)
}

/// Opens the default output device and mixes the tracks into it.
fn start_audio(shared: Arc<Shared>) -> Result<cpal::Stream, String> {
    let device = cpal::default_host().default_output_device().ok_or("no audio output device")?;
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    let channels = supported.channels() as usize;
    let out_rate = supported.sample_rate() as f64;
    let config = cpal::StreamConfig { channels: supported.channels(), sample_rate: supported.sample_rate(), buffer_size: cpal::BufferSize::Default };
    // Source frames advanced per output frame at normal speed (1.0 when the
    // device runs at 48 kHz).
    let base_step = PREVIEW_RATE as f64 / out_rate;
    let mut frac = 0.0f64;
    let mut was_playing = false;
    let mut scrub = Scrub::new(out_rate);
    let listen = shared.clone();
    trace!("audio output: {} Hz, {} channels, buffer {:?}", out_rate, channels, supported.buffer_size());

    let mut mix = move |out: &mut [f32], info: &cpal::OutputCallbackInfo| {
        out.fill(0.0);
        let pcm = match shared.pcm.lock() {
            Ok(p) => p.clone(),
            Err(_) => return,
        };
        let n_tracks = pcm.len();
        let playing = shared.playing.load(Ordering::Relaxed);
        if playing != was_playing {
            was_playing = playing;
            if playing {
                let ts = info.timestamp();
                let latency = ts.playback.duration_since(ts.callback);
                trace!("audio callback plays ({} frames per callback, heard {:?} later)", out.len() / channels.max(1), latency);
            }
        }
        if !playing && scrub.sounding(shared.scrubbing.load(Ordering::Relaxed)) {
            let tracks: Vec<TrackEdit> = match shared.mix.lock() {
                Ok(m) => m.tracks.clone(),
                Err(_) => return,
            };
            let target = shared.pos.load(Ordering::Relaxed) as f64;
            let active = shared.scrubbing.load(Ordering::Relaxed);
            let (peak, rms, track_levels) = scrub.render(out, channels, &pcm, &tracks, target, active);
            if let Ok(mut m) = shared.mix.lock() {
                m.track_levels = track_levels;
                m.master_level = (peak, rms);
                m.master_stereo = capture::mixer::Levels { peak: [peak; 2], rms: [rms; 2] };
            }
            return;
        }
        scrub.reset();
        if !playing {
            // Silence: report zero so meters fall and the UI can stop redrawing.
            if let Ok(mut m) = shared.mix.try_lock() {
                m.track_levels.iter_mut().for_each(|l| *l = (0.0, 0.0));
                m.master_level = (0.0, 0.0);
                m.master_stereo = Default::default();
            }
            return;
        }
        let tracks: Vec<TrackEdit> = match shared.mix.lock() {
            Ok(m) => m.tracks.clone(),
            Err(_) => return,
        };
        let skips: Vec<Skip> = shared.skips.lock().map(|s| s.clone()).unwrap_or_default();
        // Volume can ramp (keyframes): evaluate it every 256 samples (~5 ms)
        // and interpolate in between, which is smooth and cheap.
        let mut gains: Vec<f32> = Vec::new();
        let mut next_eval = 0u64;
        let end = shared.end.load(Ordering::Relaxed);
        let mut pos = shared.pos.load(Ordering::Relaxed);
        let mut track_peak = vec![0.0f32; n_tracks];
        let mut track_sq = vec![0.0f64; n_tracks];
        let (mut m_peak, mut m_sq, mut count) = (0.0f32, 0.0f64, 0usize);
        let (mut lr_peak, mut lr_sq) = ([0.0f32; 2], [0.0f64; 2]);
        // Faster or slower: through the sound in bigger or smaller steps,
        // between its samples where a step lands between two (the pitch
        // follows, like tape).
        let step = base_step * shared.speed();

        // A track's sound at `pos` (between it and the next sample by `frac`).
        let read = |track: &media::pcm::PcmTrack, pos: u64, frac: f64| -> Option<(f32, f32)> {
            let (a, b) = track.frame(pos as usize)?;
            Some(match track.frame(pos as usize + 1).filter(|_| frac > 0.0) {
                Some((a2, b2)) => (a + (a2 - a) * frac as f32, b + (b2 - b) * frac as f32),
                None => (a, b),
            })
        };

        for frame in out.chunks_mut(channels) {
            // Into a stretch that's cut out: on from its end, as far past it
            // as the step went.
            if let Some(s) = skips.iter().find(|s| (s.from..s.to).contains(&pos)) {
                pos = s.to + (pos - s.from);
                next_eval = 0;
            }
            if pos >= end {
                shared.playing.store(false, Ordering::Relaxed);
                break;
            }
            if pos >= next_eval {
                let t = pos as f64 / PREVIEW_RATE as f64;
                gains = tracks.iter().map(|tr| tr.gain_at(t)).collect();
                next_eval = pos + 256;
            }
            // Either side of a join, the other side of it too, crossfading.
            let across = skips.iter().find_map(|s| s.across(pos));
            let (mut l, mut r) = (0.0f32, 0.0f32);
            for (t, track) in pcm.tracks().iter().enumerate() {
                // Not decoded yet (just opened, jumped far ahead): silence.
                if let Some((a, b)) = read(track, pos, frac) {
                    let g = gains.get(t).copied().unwrap_or(1.0);
                    let (mut tl, mut tr) = (a * g, b * g);
                    if let Some((other, w)) = across {
                        let g = tracks.get(t).map_or(1.0, |e| e.gain_at(other as f64 / PREVIEW_RATE as f64));
                        let (a, b) = read(track, other, frac).unwrap_or_default();
                        (tl, tr) = (tl * (1.0 - w) + a * g * w, tr * (1.0 - w) + b * g * w);
                    }
                    l += tl;
                    r += tr;
                    let a = tl.abs().max(tr.abs());
                    track_peak[t] = track_peak[t].max(a);
                    track_sq[t] += (a as f64).powi(2);
                }
            }
            m_peak = m_peak.max(l.abs().max(r.abs()));
            for (c, v) in [l, r].into_iter().enumerate() {
                lr_peak[c] = lr_peak[c].max(v.abs());
                lr_sq[c] += (v as f64).powi(2);
            }
            m_sq += (l.abs().max(r.abs()) as f64).powi(2);
            count += 1;
            match channels {
                1 => frame[0] = (l + r) * 0.5,
                _ => {
                    frame[0] = l;
                    frame[1] = r;
                }
            }
            frac += step;
            let whole = frac.floor();
            pos += whole as u64;
            frac -= whole;
        }
        shared.pos.store(pos, Ordering::Relaxed);
        shared.moved_at_us.store((uptime() * 1e6) as u64, Ordering::Relaxed);
        shared.step_us.store((out.len() / channels.max(1)) as u64 * 1_000_000 / out_rate.max(1.0) as u64, Ordering::Relaxed);

        if count > 0 {
            if let Ok(mut m) = shared.mix.lock() {
                let rms = |sq: f64| (sq / count as f64).sqrt() as f32;
                m.track_levels = (0..n_tracks).map(|t| (track_peak[t], rms(track_sq[t]))).collect();
                m.master_level = (m_peak, rms(m_sq));
                m.master_stereo = capture::mixer::Levels { peak: lr_peak, rms: [rms(lr_sq[0]), rms(lr_sq[1])] };
            }
        }
    };
    let muted = dev_muted();
    // Recorded like any other app's sound (desktop capture leaves HasteClip
    // out, see `capture::sources::own_sound`).
    let mut recorded = capture::sources::own_sound::Playing::at(out_rate as u32);
    let stream = device
        .build_output_stream::<f32, _, _>(
            config,
            move |out: &mut [f32], info: &cpal::OutputCallbackInfo| {
                mix(out, info);
                let gain = f32::from_bits(listen.listen.load(Ordering::Relaxed));
                if muted || gain == 0.0 {
                    out.fill(0.0);
                } else if gain < 1.0 {
                    out.iter_mut().for_each(|x| *x *= gain);
                }
                // Silence needn't be handed over: a gap is silent.
                if out.iter().any(|&x| x != 0.0) {
                    let ts = info.timestamp();
                    let delay = ts.playback.duration_since(ts.callback).as_secs_f64().min(0.5);
                    recorded.push_frames(out, channels, delay);
                }
            },
            |e| eprintln!("audio output error: {e}"),
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok(stream)
}

/// Whether the dev aid `name` (an environment variable) is set, read once.
fn dev_flag(name: &'static str) -> bool {
    static FLAGS: std::sync::Mutex<Vec<(&'static str, bool)>> = std::sync::Mutex::new(Vec::new());
    let mut flags = FLAGS.lock().unwrap();
    match flags.iter().find(|(n, _)| *n == name) {
        Some((_, on)) => *on,
        None => {
            let on = std::env::var_os(name).is_some();
            flags.push((name, on));
            on
        }
    }
}

/// Dev aid: `HASTECLIP_MUTE=1` plays everything silently, with the same
/// timing (test instances that play and scrub clips).
pub(crate) fn dev_muted() -> bool {
    static MUTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *MUTED.get_or_init(|| std::env::var_os("HASTECLIP_MUTE").is_some())
}

/// Only the sound of a short clip, through the same engine as the editor's
/// player (the live mix, and the pitch-keeping scrub sound), with no picture:
/// for listening to the voice samples.
pub struct SoundPlayer {
    shared: Arc<Shared>,
    _stream: Option<cpal::Stream>,
    pub error: Option<String>,
    duration: f64,
}

impl SoundPlayer {
    /// `pcm`'s one track is stereo at [`PREVIEW_RATE`], `duration` seconds long.
    pub fn new(pcm: Arc<Pcm>, duration: f64) -> Self {
        let tracks = vec![TrackEdit { index: 0, gain: 1.0, muted: false, points: Vec::new() }];
        let shared = Arc::new(Shared::new(Mix { tracks, track_levels: vec![(0.0, 0.0)], master_level: (0.0, 0.0), master_stereo: Default::default() }, pcm));
        let (stream, error) = match start_audio(shared.clone()) {
            Ok(s) => (Some(s), None),
            Err(e) => (None, Some(e)),
        };
        Self { shared, _stream: stream, error, duration }
    }

    pub fn time(&self) -> f64 {
        self.shared.pos.load(Ordering::Relaxed) as f64 / PREVIEW_RATE as f64
    }

    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    pub fn seek(&self, t: f64) {
        self.shared.pos.store(to_samples(t.clamp(0.0, self.duration)), Ordering::Relaxed);
    }

    /// Play from where it is until `until` seconds.
    pub fn play(&self, until: f64) {
        self.shared.end.store(to_samples(until), Ordering::Relaxed);
        self.shared.scrubbing.store(false, Ordering::Relaxed);
        self.shared.playing.store(true, Ordering::Relaxed);
    }

    pub fn pause(&self) {
        self.shared.playing.store(false, Ordering::Relaxed);
    }

    /// The playhead is being dragged: the sound follows it.
    pub fn scrub(&self, on: bool) {
        self.shared.scrubbing.store(on && !self.is_playing(), Ordering::Relaxed);
    }
}

/// Streams consecutive frames from a start time via one ffmpeg process.
struct FrameStream {
    child: Child,
    rx: Receiver<(u64, Decoded)>,
    peeked: Option<(u64, Decoded)>,
}

impl FrameStream {
    fn start(ctx: &egui::Context, source: &Path, info: &ClipInfo, t: f64) -> std::io::Result<Self> {
        let nv12 = nv12_size(info);
        let (w, h) = nv12.unwrap_or_else(|| (PREVIEW_WIDTH, (((PREVIEW_WIDTH as f64) * info.height as f64 / info.width.max(1) as f64 / 2.0).round() as u32 * 2).max(2)));
        let scale = format!("scale={w}:{h}:flags=bilinear");
        let out = match nv12 {
            Some(_) => media::NV12_OUT.to_vec(),
            None => vec!["-vf", &scale, "-f", "rawvideo", "-pix_fmt", "rgba"],
        };
        let mut child = media::ffmpeg()
            .args(["-hide_banner", "-loglevel", "error", "-ss", &format!("{t:.4}"), "-i"])
            .arg(source)
            .arg("-an")
            .args(out)
            .arg("-")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut stdout = child.stdout.take().expect("piped");
        // Small buffer: decoding runs just ahead of playback rather than racing to the end.
        let (tx, rx): (SyncSender<_>, _) = mpsc::sync_channel(6);
        let first = info.frame_index(t);
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            let size = if nv12.is_some() { media::nv12_len(w, h) } else { (w * h * 4) as usize };
            let mut idx = first;
            loop {
                let mut buf = vec![0u8; size];
                if stdout.read_exact(&mut buf).is_err() {
                    break;
                }
                let frame = if nv12.is_some() { Decoded::Nv12(buf) } else { Decoded::Rgba(media::Frame { width: w, height: h, rgba: buf }) };
                if tx.send((idx, frame)).is_err() {
                    break;
                }
                repaint.request_repaint();
                idx += 1;
            }
        });
        Ok(Self { child, rx, peeked: None })
    }

    fn peek(&mut self) -> Option<(u64, ())> {
        if self.peeked.is_none() {
            self.peeked = self.rx.try_recv().ok();
        }
        self.peeked.as_ref().map(|(i, _)| (*i, ()))
    }

    fn next(&mut self) -> Option<(u64, Decoded)> {
        self.peek();
        self.peeked.take()
    }
}

impl Drop for FrameStream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The position, in samples, for a seek to `t` seconds: rounded up, so it's
/// never before `t`. Rounded down, a frame's start at a rate that isn't a
/// whole number (59.9 fps) came back a hair before the frame, in the frame
/// ahead of it, and stepping a frame forward from there stayed put.
fn to_samples(t: f64) -> u64 {
    (t * PREVIEW_RATE as f64).ceil() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A seek to a frame reads back as that frame, at rates that aren't
    /// whole numbers too.
    #[test]
    fn seeking_to_a_frame_lands_on_it() {
        for fps in [60.0, 107760.0 / 1799.0, 43128.0 / 719.0, 30000.0 / 1001.0, 144.0] {
            let info = ClipInfo { duration: 120.0, fps, width: 2, height: 2, video_kbps: None, audio: Vec::new(), id: None, bt601: false };
            for k in 0..(120.0 * fps) as u64 - 1 {
                let t = info.snap(k as f64 / fps + 1e-9) + 1e-6;
                let back = to_samples(t) as f64 / PREVIEW_RATE as f64;
                assert_eq!(info.frame_index(back), k, "frame {k} at {fps} fps");
            }
        }
    }

    /// Either side of a join, what's on the other side of it is heard too:
    /// none of it at the crossfade's ends, half at the join itself, read from
    /// just past where each side leaves off (as the saved clip's crossfade).
    #[test]
    fn joins_crossfade() {
        let s = Skip { from: 1000, to: 5000, fade: 100 };
        assert_eq!(s.across(899), None);
        assert_eq!(s.across(900), Some((4900, 0.0)));
        let (other, w) = s.across(999).unwrap();
        assert!(other == 4999 && (w - 0.495).abs() < 1e-6, "{other} {w}");
        assert_eq!(s.across(5000), Some((1000, 0.5)));
        let (other, w) = s.across(5099).unwrap();
        assert!(other == 1099 && w < 0.01, "{other} {w}");
        assert_eq!(s.across(5100), None);
        assert_eq!(Skip { fade: 0, ..s }.across(999), None);
    }

    /// A stereo tone at `hz`, `secs` long, at the source rate.
    fn tone(hz: f64, secs: f64) -> Vec<f32> {
        let n = (secs * PREVIEW_RATE as f64) as usize;
        (0..n).flat_map(|i| {
            let v = (std::f64::consts::TAU * hz * i as f64 / PREVIEW_RATE as f64).sin() as f32 * 0.5;
            [v, v]
        }).collect()
    }

    /// Scrub with the playhead moving at `speed` × real time for `secs`;
    /// returns the left channel out, at 48 kHz.
    fn scrub(pcm: &[Vec<f32>], speed: f64, secs: f64, start: f64) -> Vec<f32> {
        let pcm = &*Pcm::from_tracks(pcm.to_vec());
        let rate = 48_000.0;
        let mut s = Scrub::new(rate);
        let tracks = [TrackEdit { index: 0, gain: 1.0, muted: false, points: Vec::new() }];
        let mut out = Vec::new();
        let mut target = start;
        let block = 480; // 10 ms callbacks
        for _ in 0..(secs * rate / block as f64) as usize {
            let mut buf = vec![0.0f32; block * 2];
            s.render(&mut buf, 2, pcm, &tracks, target, true);
            out.extend(buf.chunks(2).map(|f| f[0]));
            target += speed * block as f64 * PREVIEW_RATE as f64 / rate;
        }
        out
    }

    /// Frequency from upward zero crossings, over the loud part.
    fn pitch(x: &[f32]) -> f64 {
        let x = &x[x.len() / 4..];
        let ups = x.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        ups as f64 / (x.len() as f64 / 48_000.0)
    }

    #[test]
    fn scrubbing_keeps_the_pitch() {
        let pcm = vec![tone(440.0, 20.0)];
        for speed in [0.5, 1.0, 2.0, 4.0, -1.0] {
            let out = scrub(&pcm, speed, 1.0, 5.0 * PREVIEW_RATE as f64);
            let hz = pitch(&out);
            assert!((hz - 440.0).abs() < 15.0, "at {speed}× it came out at {hz:.0} Hz");
            let loud = out[out.len() / 2..].iter().fold(0.0f32, |a, &b| a.max(b.abs()));
            assert!(loud > 0.3, "at {speed}× it's quiet ({loud})");
        }
    }

    #[test]
    fn standing_still_goes_quiet_and_a_jump_plays_the_new_place() {
        // 440 Hz for the first 10 s, 880 after.
        let mut pcm = tone(440.0, 10.0);
        pcm.extend(tone(880.0, 10.0));
        let pcm = vec![pcm];
        let still = scrub(&pcm, 0.0, 0.5, 2.0 * PREVIEW_RATE as f64);
        let end = still[still.len() - 2400..].iter().fold(0.0f32, |a, &b| a.max(b.abs()));
        assert!(end < 1e-3, "still playing when the playhead stands still ({end})");

        let pcm = Pcm::from_tracks(pcm);
        let rate = 48_000.0;
        let mut s = Scrub::new(rate);
        let tracks = [TrackEdit { index: 0, gain: 1.0, muted: false, points: Vec::new() }];
        let mut out = Vec::new();
        for k in 0..60 {
            // Moving through the 440 Hz part, then a jump into the 880 Hz part.
            let target = if k < 30 { 2.0 + k as f64 * 0.01 } else { 15.0 + k as f64 * 0.01 } * PREVIEW_RATE as f64;
            let mut buf = vec![0.0f32; 960];
            s.render(&mut buf, 2, &pcm, &tracks, target, true);
            if k >= 35 {
                out.extend(buf.chunks(2).map(|f| f[0]));
            }
        }
        let hz = pitch(&out);
        assert!((hz - 880.0).abs() < 30.0, "after the jump: {hz:.0} Hz");
    }

    /// A player hushed into a curtain and handed straight back (switching
    /// back before the new screen's picture came up) is taken over with its
    /// sound back: its clock moves, as a player handed over as it is does.
    #[test]
    #[ignore = "needs a sound output"]
    fn a_curtain_handed_back_plays() {
        let clip = std::env::temp_dir().join(format!("hc-curtain-{}.mp4", std::process::id()));
        let made = media::ffmpeg()
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=320x240:rate=30:duration=3"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .arg(&clip)
            .status()
            .unwrap();
        assert!(made.success());
        let ctx = egui::Context::default();
        let info = media::probe(&clip).unwrap();
        let pcm = Pcm::from_tracks(vec![tone(440.0, 3.0)]);
        // What `update` does once the first frame is there: the sound starts.
        let moves = |p: &Player| {
            p.shared.playing.store(true, Ordering::Relaxed);
            let t0 = p.time();
            std::thread::sleep(std::time::Duration::from_millis(400));
            let moved = p.time() - t0;
            p.shared.playing.store(false, Ordering::Relaxed);
            moved
        };
        let player = Player::new(&ctx, &clip, info.clone(), pcm.clone(), Vec::new());
        let (mut player, _, taken) = Player::handed_over(&ctx, &clip, &info, pcm.clone(), Vec::new(), Some(player));
        assert!(taken, "the same file is taken over");
        let moved = moves(&player);
        assert!(moved > 0.2, "handed over as it is, its clock moved {moved:.3} s in 0.4 s");
        player.hush();
        let (player, _, taken) = Player::handed_over(&ctx, &clip, &info, pcm, Vec::new(), Some(player));
        assert!(taken, "a curtain on the same file is taken over");
        let moved = moves(&player);
        assert!(moved > 0.2, "handed back from a curtain, its clock moved {moved:.3} s in 0.4 s");
        std::fs::remove_file(&clip).unwrap();
    }

    /// Noise (like game sound: no two slices alike), stereo, at the source rate.
    fn noise(secs: f64) -> Vec<f32> {
        let mut x = 12345u32;
        (0..(secs * PREVIEW_RATE as f64) as usize * 2)
            .map(|_| {
                x = x.wrapping_mul(1664525).wrapping_add(1013904223);
                (x >> 8) as f32 / (1u32 << 24) as f32 - 0.5
            })
            .collect()
    }

    /// RMS of `x`, and how much the RMS of 5 ms stretches varies (std / mean).
    fn loudness(x: &[f32]) -> (f64, f64) {
        let rms = |v: &[f32]| (v.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / v.len() as f64).sqrt();
        let blocks: Vec<f64> = x.chunks(240).filter(|c| c.len() == 240).map(rms).collect();
        let mean = blocks.iter().sum::<f64>() / blocks.len() as f64;
        let var = blocks.iter().map(|b| (b - mean).powi(2)).sum::<f64>() / blocks.len() as f64;
        (rms(x), var.sqrt() / mean)
    }

    #[test]
    fn scrubbing_keeps_the_loudness_steady() {
        let src = noise(20.0);
        let left: Vec<f32> = src.chunks(2).map(|f| f[0]).collect();
        let (in_rms, in_flutter) = loudness(&left[..48_000]);
        let pcm = vec![src];
        for speed in [0.5, 1.0, 2.0, 4.0, -1.0] {
            let out = scrub(&pcm, speed, 1.5, 5.0 * PREVIEW_RATE as f64);
            let (rms, flutter) = loudness(&out[24_000..]);
            let db = 20.0 * (rms / in_rms).log10();
            eprintln!("{speed}x: {db:+.2} dB, flutter {flutter:.3} (source {in_flutter:.3})");
            assert!(db.abs() < 1.0, "at {speed}x the loudness is off by {db:.2} dB");
            assert!(flutter < in_flutter * 1.5, "at {speed}x the loudness flutters ({flutter:.3} vs the source's {in_flutter:.3})");
        }
    }

    #[test]
    fn scrubbing_is_cheap_enough_for_the_audio_callback() {
        // Six tracks (an editor clip), one second of output: well under a
        // second even unoptimized (half of it, here). Timed by the work this
        // thread does, not the clock: with the other tests running beside it,
        // the clock said 2.5 times as long. Its work grows too, as they share
        // the cores (measured unoptimized: 140 ms alone, up to 270 ms
        // beside them).
        let pcm: Vec<Vec<f32>> = (0..6).map(|_| tone(440.0, 20.0)).collect();
        let t = thread_cpu();
        let _ = scrub(&pcm, 1.5, 1.0, 5.0 * PREVIEW_RATE as f64);
        let took = thread_cpu() - t;
        eprintln!("one second of scrub sound, 6 tracks: {took:?} of work");
        assert!(took.as_secs_f64() < 0.5, "took {took:?}");
    }

    /// The CPU time this thread has used so far.
    fn thread_cpu() -> std::time::Duration {
        #[cfg(windows)]
        {
            use windows::Win32::Foundation::FILETIME;
            use windows::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
            let (mut created, mut exited, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
            unsafe { GetThreadTimes(GetCurrentThread(), &mut created, &mut exited, &mut kernel, &mut user) }.expect("the thread's times");
            let ticks = |f: FILETIME| (f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64;
            // In 100 ns ticks.
            std::time::Duration::from_nanos((ticks(kernel) + ticks(user)) * 100)
        }
        #[cfg(unix)]
        {
            let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
            unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
            std::time::Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
        }
    }
}
