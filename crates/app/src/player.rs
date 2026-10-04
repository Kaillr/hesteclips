//! Preview playback for the editor.
//!
//! Audio drives the clock: every source track is decoded up front to stereo f32
//! and mixed live in the cpal callback at the current gains, so moving a slider is
//! heard instantly. The playhead is however many samples the callback has played.
//!
//! Video follows the clock. On Windows it comes from the in-process hardware
//! decoder (`video.rs`), which keeps the next frames decoded so play starts at
//! once; if that can't open a file (or elsewhere), an ffmpeg process streams
//! scaled RGBA frames from the playhead onward, and single exact frames are
//! decoded on demand when paused. Either way the UI shows whichever decoded
//! frame matches the clock. While scrubbing, frames come from the in-memory
//! proxy (`proxy.rs`).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use media::{ClipInfo, PREVIEW_RATE, TrackEdit};

use crate::proxy::Proxy;
#[cfg(windows)]
use capture::win::decode::Picture;
#[cfg(windows)]
use crate::video::Video;
#[cfg(not(windows))]
struct Picture {
    index: u64,
}

/// Elsewhere there's no in-process decoder yet: a type with no values.
#[cfg(not(windows))]
enum Video {}
#[cfg(not(windows))]
impl Video {
    fn failed(&self) -> bool { match *self {} }
    fn show(&self, _: u64) { match *self {} }
    fn play(&self, _: u64) { match *self {} }
    fn pause(&self) { match *self {} }
    fn take_exact(&self, _: u64) -> Option<Picture> { match *self {} }
    fn take_upto(&self, _: u64) -> Option<Picture> { match *self {} }
    fn has(&self, _: u64) -> bool { match *self {} }
    fn open_for(&self) -> Option<std::time::Duration> { match *self {} }
}

/// `HESTECLIPS_DEBUG_VIDEO=1`: log what playback does, with times.
fn debug() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("HESTECLIPS_DEBUG_VIDEO").is_some())
}

macro_rules! trace {
    ($($arg:tt)*) => {
        if debug() {
            eprintln!("{:>8.3} player: {}", crate::player::uptime(), format!($($arg)*));
        }
    };
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
        let color = if on { crate::library::ACCENT } else { egui::Color32::from_white_alpha(25) };
        p.rect_filled(egui::Rect::from_x_y_ranges(x0..=x0 + run as f32 * step, y.clone()), 0, color);
        k += run;
    }
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
}

/// What's on screen, with its size in pixels.
enum Display {
    /// An uploaded image: scrub proxy frames, frames that came back to the CPU.
    Image(egui::TextureId, egui::Vec2),
    /// A decoded frame on the GPU, drawn with the sharp filter.
    #[cfg(windows)]
    Gpu(crate::gpu_frames::Shown, egui::Vec2),
}

impl Display {
    fn size(&self) -> egui::Vec2 {
        match self {
            Display::Image(_, s) => *s,
            #[cfg(windows)]
            Display::Gpu(_, s) => *s,
        }
    }
}

/// Play waits at most this long for its first frame before the sound starts anyway.
const START_WAIT: std::time::Duration = std::time::Duration::from_millis(100);

/// Preview frames are decoded at this width; plenty for an in-app viewer.
pub const PREVIEW_WIDTH: u32 = 1280;

/// Live mix state shared with the audio callback.
pub struct Mix {
    /// Per source track: volume (incl. keyframes) and mute, as in the edit.
    pub tracks: Vec<TrackEdit>,
    /// Peak/RMS of what was just played, per track and for the master, for meters.
    pub track_levels: Vec<(f32, f32)>,
    pub master_level: (f32, f32),
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
}

pub struct Player {
    source: PathBuf,
    info: ClipInfo,
    shared: Arc<Shared>,
    _stream: Option<cpal::Stream>,
    pub audio_error: Option<String>,
    /// Latest frame for display.
    texture: Option<egui::TextureHandle>,
    /// What's on screen.
    display: Option<Display>,
    /// The GPU frame on screen, held so its texture isn't reused meanwhile.
    on_screen: Option<Picture>,
    /// GPU frames just taken off screen, held a few more frames: the GPU may
    /// still be drawing them, and the decoder reuses a released texture.
    retired: std::collections::VecDeque<(u64, Picture)>,
    /// GPU frames opened for drawing (see `gpu_frames.rs`).
    #[cfg(windows)]
    gpu_frames: crate::gpu_frames::Frames,
    shown_frame: Option<u64>,
    decoder: Option<FrameStream>,
    still_tx: Sender<(u64, f64)>,
    still_rx: Receiver<(u64, media::Frame)>,
    /// Frame index the last on-demand still was requested for.
    still_wanted: Option<u64>,
    /// Every frame, small, for instant scrubbing.
    proxy: Proxy,
    /// Showing a proxy frame; swap in the sharp frame once the drag settles.
    showing_proxy: bool,
    /// While scrubbing where the proxy isn't built yet: the nearest frame
    /// that is, on screen in place of the one wanted.
    stand_in: Option<u64>,
    /// The hardware decoder, while it works.
    video: Option<Video>,
    /// Play was pressed; the sound starts with the first frame (or after
    /// [`START_WAIT`]), so picture and sound start together.
    pending_start: Option<std::time::Instant>,
    /// The decoder was told to play (to tell it to stop when playback ends).
    video_playing: bool,
    /// When play was last pressed, until its first frame shows (debug log).
    played_at: Option<std::time::Instant>,
    /// When the full-quality frame was last asked for (debug log).
    asked_at: Option<std::time::Instant>,
    /// Frames shown while playing, per second (debug log).
    shown_count: Option<(std::time::Instant, u32)>,
}

impl Player {
    /// `pcm[i]` is source track `i` as interleaved stereo at [`PREVIEW_RATE`].
    pub fn new(ctx: &egui::Context, source: &Path, info: ClipInfo, pcm: Vec<Vec<f32>>, gains: Vec<TrackEdit>) -> Self {
        let n = pcm.len();
        let shared = Arc::new(Shared {
            pos: AtomicU64::new(0),
            playing: AtomicBool::new(false),
            scrubbing: AtomicBool::new(false),
            end: AtomicU64::new(u64::MAX),
            mix: Mutex::new(Mix { tracks: gains, track_levels: vec![(0.0, 0.0); n], master_level: (0.0, 0.0) }),
        });
        let (stream, audio_error) = match start_audio(shared.clone(), Arc::new(pcm)) {
            Ok(s) => (Some(s), None),
            Err(e) => (None, Some(e)),
        };

        // On-demand exact stills (paused/scrubbing), one worker, newest request wins.
        let (still_tx, job_rx) = mpsc::channel::<(u64, f64)>();
        let (done_tx, still_rx) = mpsc::channel();
        let src = source.to_path_buf();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            while let Ok(mut job) = job_rx.recv() {
                while let Ok(newer) = job_rx.try_recv() {
                    job = newer;
                }
                if let Ok(frame) = media::frame_at(&src, job.1, PREVIEW_WIDTH) {
                    if done_tx.send((job.0, frame)).is_err() {
                        break;
                    }
                    repaint.request_repaint();
                }
            }
        });

        let info_fps = info.fps;
        let total_frames = (info.duration * info_fps).floor() as usize;
        Self {
            source: source.to_path_buf(),
            info,
            shared,
            _stream: stream,
            audio_error,
            texture: None,
            display: None,
            on_screen: None,
            retired: Default::default(),
            #[cfg(windows)]
            gpu_frames: Default::default(),
            shown_frame: None,
            decoder: None,
            still_tx,
            still_rx,
            still_wanted: None,
            proxy: Proxy::build(ctx, source, info_fps, total_frames),
            showing_proxy: false,
            stand_in: None,
            video: open_video(ctx, source, info_fps),
            pending_start: None,
            video_playing: false,
            played_at: None,
            shown_count: None,
            asked_at: None,
        }
    }

    pub fn time(&self) -> f64 {
        self.shared.pos.load(Ordering::Relaxed) as f64 / PREVIEW_RATE as f64
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
        self.shared.pos.store((t * PREVIEW_RATE as f64) as u64, Ordering::Relaxed);
        self.decoder = None; // restarts from the new position if playing
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
        if let Some(v) = self.hw() {
            // Showing the frame under the playhead already: carry on from the
            // next, which was decoded while paused. Otherwise wait for this one.
            let want = self.info.frame_index(self.time());
            let showing = self.shown_frame == Some(want) && !self.showing_proxy;
            v.play(if showing { want + 1 } else { want });
            self.video_playing = true;
            if !showing {
                self.pending_start = Some(std::time::Instant::now());
                return;
            }
        }
        self.shared.playing.store(true, Ordering::Relaxed);
    }

    pub fn pause(&mut self) {
        self.shared.playing.store(false, Ordering::Relaxed);
        self.pending_start = None;
        self.decoder = None;
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
        self.proxy.built(self.info.frame_index(from), self.info.frame_index(to), n)
    }

    /// Fraction of the scrub proxy that's built (0..=1).
    pub fn proxy_progress(&self) -> f32 {
        let total = (self.info.duration * self.info.fps).floor().max(1.0);
        (self.proxy.ready() as f64 / total).min(1.0) as f32
    }

    /// The small scrub frame `idx`, if it's been built yet (for hover previews).
    pub fn proxy_frame(&self, idx: u64) -> Option<egui::ColorImage> {
        self.proxy.frame(idx)
    }

    pub fn set_mix(&self, gains: Vec<TrackEdit>) {
        if let Ok(mut m) = self.shared.mix.lock() {
            m.tracks = gains;
        }
    }

    /// Current meter readings: per track, then master.
    pub fn levels(&self) -> (Vec<(f32, f32)>, (f32, f32)) {
        match self.shared.mix.lock() {
            Ok(m) => (m.track_levels.clone(), m.master_level),
            Err(_) => (Vec::new(), (0.0, 0.0)),
        }
    }

    /// Bring the displayed frame in line with the clock. Call once per UI frame.
    /// `scrubbing`: the user is dragging, so favour instant proxy frames.
    /// Returns the size of what's on screen (in pixels, to fit it); draw it
    /// with [`Self::paint`].
    pub fn update(&mut self, ctx: &egui::Context, scrubbing: bool) -> Option<egui::Vec2> {
        self.proxy.set_focus(self.info.frame_index(self.time()));
        self.shared.scrubbing.store(scrubbing && !self.sound_playing(), Ordering::Relaxed);
        if scrubbing && self.proxy_progress() < 1.0 {
            crate::video::scrubbing();
        }
        if self.hw().is_some() {
            self.update_hw(ctx, scrubbing);
            return self.display.as_ref().map(Display::size);
        }
        let t = self.time();
        let want = self.info.frame_index(t);

        if !self.is_playing() {
            // Stopped by itself at the out point: the stream is now stale.
            self.decoder = None;
        }
        if self.is_playing() {
            if self.decoder.is_none() {
                self.decoder = FrameStream::start(ctx, &self.source, &self.info, t).ok();
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
                    self.show(ctx, idx, frame);
                }
            }
            ctx.request_repaint();
        } else {
            while let Ok((idx, frame)) = self.still_rx.try_recv() {
                if Some(idx) == self.still_wanted && idx == want {
                    self.show(ctx, idx, frame);
                }
            }
            // While dragging, show the proxy frame right away (every mouse move).
            if self.shown_frame != Some(want) || (!scrubbing && self.showing_proxy) {
                if self.shown_frame != Some(want) {
                    if let Some(img) = self.proxy.frame(want) {
                        self.set_texture(ctx, img);
                        self.shown_frame = Some(want);
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
        self.display.as_ref().map(Display::size)
    }

    /// Draw the picture into `rect`: a decoded frame through the sharp
    /// filter (`gpu_frames.rs`), anything else as an image.
    pub fn paint(&self, ui: &egui::Ui, rect: egui::Rect) {
        match &self.display {
            Some(Display::Image(id, _)) => egui::Image::from_texture((*id, rect.size())).paint_at(ui, rect),
            #[cfg(windows)]
            Some(Display::Gpu(shown, _)) => shown.paint(ui, rect),
            None => {}
        }
    }

    /// [`Self::update`] with the hardware decoder.
    /// `scrubbing`: a drag or wheel scrub is under way (proxy frames only).
    fn update_hw(&mut self, ctx: &egui::Context, scrubbing: bool) {
        let Some(v) = self.video.as_ref() else { return };
        let want = self.info.frame_index(self.time());
        // Waiting to start: the sound goes once the first frame is here.
        if let Some(since) = self.pending_start {
            // The wait counts from when the decoder opened (a few hundred ms
            // after a clip opens), so picture and sound start together then
            // too; at most 1.5 s in all.
            let waited = v.open_for().map_or(std::time::Duration::ZERO, |open| open.min(since.elapsed()));
            if v.has(want) || waited >= START_WAIT || since.elapsed() >= std::time::Duration::from_millis(1500) {
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
                    #[cfg(windows)]
                    trace!("playing: want {want}, showing {:?}, ahead {:?}, {passes} redraws/s, last frame {:.1} ms", self.shown_frame, v.ahead_info(), ctx.input(|i| i.unstable_dt) * 1000.0);
                }
            }
            if let Some(p) = v.take_upto(want) {
                self.show_picture(ctx, p);
            }
            ctx.request_repaint();
            return;
        }
        // Stopped by itself at the out point: so does the decoder.
        if self.video_playing {
            v.pause();
            self.video_playing = false;
            self.still_wanted = None;
        }
        if let Some(p) = v.take_exact(want) {
            if let Some(at) = self.asked_at.take() {
                trace!("sharp frame {want} (picture {}) {:.0} ms after asking", p.index, at.elapsed().as_secs_f64() * 1000.0);
            }
            self.show_picture(ctx, p);
            // It's the picture for `want` even when its own number is lower (a
            // dropped frame's slot): don't put the proxy back over it.
            self.shown_frame = Some(want);
        }
        let sharp = self.shown_frame == Some(want) && !self.showing_proxy;
        if !sharp {
            // The small proxy frame right away (every mouse move while scrubbing)…
            if self.shown_frame != Some(want) {
                if let Some(img) = self.proxy.frame(want) {
                    self.set_texture(ctx, img);
                    self.shown_frame = Some(want);
                    self.showing_proxy = true;
                    self.stand_in = None;
                } else if scrubbing {
                    self.show_stand_in(ctx, want);
                }
            }
            // …and the full-quality frame once the scrub has stopped (the mouse
            // let go, the wheel settled). Asked once per frame wanted.
            let v = self.video.as_ref().expect("checked above");
            if !scrubbing && self.still_wanted != Some(want) {
                self.still_wanted = Some(want);
                self.asked_at = Some(std::time::Instant::now());
                v.show(want);
            }
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
        // Counts frames that change the picture, not frames handed over.
        if debug() && self.sound_playing() && self.shown_frame != Some(idx) {
            let (now, ready) = (self.time(), self.proxy.ready());
            let (since, n) = self.shown_count.get_or_insert((std::time::Instant::now(), 0));
            *n += 1;
            if since.elapsed().as_secs_f64() >= 1.0 {
                trace!("playing: {n} new frames in the last second, now {idx} at {now:.2} s (proxy {ready} ready)");
                self.shown_count = None;
            }
        }
        #[cfg(windows)]
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
        #[cfg(not(windows))]
        let _ = (ctx, p);
        self.shown_frame = Some(idx);
        self.showing_proxy = false;
        self.stand_in = None;
    }

    /// Scrubbing where frame `want` isn't built yet: the nearest built frame
    /// within 10 s instead, so the picture keeps following (the frame itself
    /// replaces it as soon as it's built, and the sharp one once you stop).
    fn show_stand_in(&mut self, ctx: &egui::Context, want: u64) {
        let within = (self.info.fps * 10.0).round().max(1.0) as u64;
        let Some(i) = self.proxy.nearest(want, within) else { return };
        if self.stand_in == Some(i) {
            return;
        }
        if let Some(img) = self.proxy.frame(i) {
            self.set_texture(ctx, img);
            self.shown_frame = None;
            self.showing_proxy = true;
            self.stand_in = Some(i);
            trace!("scrubbing: frame {want} not built, showing {i}");
        }
    }

    fn show(&mut self, ctx: &egui::Context, idx: u64, frame: media::Frame) {
        let img = egui::ColorImage::from_rgba_unmultiplied([frame.width as usize, frame.height as usize], &frame.rgba);
        self.set_texture(ctx, img);
        self.shown_frame = Some(idx);
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
        match &mut self.texture {
            Some(tex) => tex.set(img, egui::TextureOptions::LINEAR),
            None => self.texture = Some(ctx.load_texture("preview", img, egui::TextureOptions::LINEAR)),
        }
        self.display = self.texture.as_ref().map(|t| Display::Image(t.id(), t.size_vec2()));
        self.retire(ctx, None);
    }
}

/// The hardware decoder for `source`, on Windows (unless
/// `HESTECLIPS_NO_HW_DECODE` is set, to compare with the ffmpeg path).
fn open_video(ctx: &egui::Context, source: &Path, fps: f64) -> Option<Video> {
    #[cfg(windows)]
    if std::env::var_os("HESTECLIPS_NO_HW_DECODE").is_none() {
        return Some(Video::open(ctx, source, fps));
    }
    let _ = (ctx, source, fps);
    None
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
    fn sample(&self, pcm: &[Vec<f32>], i: usize) -> (f32, f32) {
        let (mut l, mut r) = (0.0, 0.0);
        for (t, track) in pcm.iter().enumerate() {
            if let Some(s) = track.get(2 * i..2 * i + 2) {
                let g = self.gains.get(t).copied().unwrap_or(1.0);
                l += s[0] * g;
                r += s[1] * g;
            }
        }
        (l, r)
    }

    /// The next half slice into `made`, the slice taken near `target`.
    fn make(&mut self, pcm: &[Vec<f32>], tracks: &[TrackEdit], target: usize) {
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
        for (t, track) in pcm.iter().enumerate() {
            let g = self.gains.get(t).copied().unwrap_or(1.0);
            let peak = (start..start + Self::HOP)
                .step_by(8)
                .filter_map(|i| track.get(2 * i..2 * i + 2))
                .map(|s| s[0].abs().max(s[1].abs()) * g)
                .fold(0.0f32, f32::max);
            self.track_peak[t] = peak;
        }
        self.prev = Some(start);
    }

    /// Fill `out` following the playhead at `target` (source frames); `active`
    /// false fades out. Returns the master's (peak, rms) and each track's.
    fn render(&mut self, out: &mut [f32], channels: usize, pcm: &[Vec<f32>], tracks: &[TrackEdit], target: f64, active: bool) -> (f32, f32, Vec<(f32, f32)>) {
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

/// Opens the default output device and mixes the tracks into it.
fn start_audio(shared: Arc<Shared>, pcm: Arc<Vec<Vec<f32>>>) -> Result<cpal::Stream, String> {
    let device = cpal::default_host().default_output_device().ok_or("no audio output device")?;
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    let channels = supported.channels() as usize;
    let out_rate = supported.sample_rate() as f64;
    let config = cpal::StreamConfig { channels: supported.channels(), sample_rate: supported.sample_rate(), buffer_size: cpal::BufferSize::Default };
    // Source frames advanced per output frame (1.0 when the device runs at 48 kHz).
    let step = PREVIEW_RATE as f64 / out_rate;
    let mut frac = 0.0f64;
    let n_tracks = pcm.len();
    let mut was_playing = false;
    let mut scrub = Scrub::new(out_rate);
    trace!("audio output: {} Hz, {} channels, buffer {:?}", out_rate, channels, supported.buffer_size());

    let stream = device
        .build_output_stream::<f32, _, _>(
            config,
            move |out: &mut [f32], info: &cpal::OutputCallbackInfo| {
                out.fill(0.0);
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
                    }
                    return;
                }
                scrub.reset();
                if !playing {
                    // Silence: report zero so meters fall and the UI can stop redrawing.
                    if let Ok(mut m) = shared.mix.try_lock() {
                        m.track_levels.iter_mut().for_each(|l| *l = (0.0, 0.0));
                        m.master_level = (0.0, 0.0);
                    }
                    return;
                }
                let tracks: Vec<TrackEdit> = match shared.mix.lock() {
                    Ok(m) => m.tracks.clone(),
                    Err(_) => return,
                };
                // Volume can ramp (keyframes): evaluate it every 256 samples (~5 ms)
                // and interpolate in between, which is smooth and cheap.
                let mut gains: Vec<f32> = Vec::new();
                let mut next_eval = 0u64;
                let end = shared.end.load(Ordering::Relaxed);
                let mut pos = shared.pos.load(Ordering::Relaxed);
                let mut track_peak = vec![0.0f32; n_tracks];
                let mut track_sq = vec![0.0f64; n_tracks];
                let (mut m_peak, mut m_sq, mut count) = (0.0f32, 0.0f64, 0usize);

                for frame in out.chunks_mut(channels) {
                    if pos >= end {
                        shared.playing.store(false, Ordering::Relaxed);
                        break;
                    }
                    if pos >= next_eval {
                        let t = pos as f64 / PREVIEW_RATE as f64;
                        gains = tracks.iter().map(|tr| tr.gain_at(t)).collect();
                        next_eval = pos + 256;
                    }
                    let i = pos as usize * 2;
                    let (mut l, mut r) = (0.0f32, 0.0f32);
                    for (t, track) in pcm.iter().enumerate() {
                        if i + 1 < track.len() {
                            let g = gains.get(t).copied().unwrap_or(1.0);
                            let (tl, tr) = (track[i] * g, track[i + 1] * g);
                            l += tl;
                            r += tr;
                            let a = tl.abs().max(tr.abs());
                            track_peak[t] = track_peak[t].max(a);
                            track_sq[t] += (a as f64).powi(2);
                        }
                    }
                    m_peak = m_peak.max(l.abs().max(r.abs()));
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

                if count > 0 {
                    if let Ok(mut m) = shared.mix.lock() {
                        let rms = |sq: f64| (sq / count as f64).sqrt() as f32;
                        m.track_levels = (0..n_tracks).map(|t| (track_peak[t], rms(track_sq[t]))).collect();
                        m.master_level = (m_peak, rms(m_sq));
                    }
                }
            },
            |e| eprintln!("audio output error: {e}"),
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok(stream)
}

/// Streams consecutive preview frames from a start time via one ffmpeg process.
struct FrameStream {
    child: Child,
    rx: Receiver<(u64, media::Frame)>,
    peeked: Option<(u64, media::Frame)>,
}

impl FrameStream {
    fn start(ctx: &egui::Context, source: &Path, info: &ClipInfo, t: f64) -> std::io::Result<Self> {
        let w = PREVIEW_WIDTH;
        let h = (((w as f64) * info.height as f64 / info.width.max(1) as f64 / 2.0).round() as u32 * 2).max(2);
        let mut child = media::ffmpeg()
            .args(["-hide_banner", "-loglevel", "error", "-ss", &format!("{t:.4}"), "-i"])
            .arg(source)
            .args(["-an", "-vf", &format!("scale={w}:{h}:flags=bilinear"), "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
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
            let size = (w * h * 4) as usize;
            let mut idx = first;
            loop {
                let mut buf = vec![0u8; size];
                if stdout.read_exact(&mut buf).is_err() {
                    break;
                }
                if tx.send((idx, media::Frame { width: w, height: h, rgba: buf })).is_err() {
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

    fn next(&mut self) -> Option<(u64, media::Frame)> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
        // second even unoptimized.
        let pcm: Vec<Vec<f32>> = (0..6).map(|_| tone(440.0, 20.0)).collect();
        let t = std::time::Instant::now();
        let _ = scrub(&pcm, 1.5, 1.0, 5.0 * PREVIEW_RATE as f64);
        let took = t.elapsed();
        eprintln!("one second of scrub sound, 6 tracks: {took:?}");
        assert!(took.as_secs_f64() < 0.25, "took {took:?}");
    }
}
