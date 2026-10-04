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
    /// What's on screen and its size: `texture`, or a frame on the GPU.
    display: Option<(egui::TextureId, egui::Vec2)>,
    /// The GPU frame on screen, held so its texture isn't reused meanwhile.
    on_screen: Option<Picture>,
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
            #[cfg(windows)]
            gpu_frames: Default::default(),
            shown_frame: None,
            decoder: None,
            still_tx,
            still_rx,
            still_wanted: None,
            proxy: Proxy::build(ctx, source, info_fps, total_frames),
            showing_proxy: false,
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
    /// Returns what to draw: an egui texture and its size.
    pub fn update(&mut self, ctx: &egui::Context, scrubbing: bool) -> Option<(egui::TextureId, egui::Vec2)> {
        if self.hw().is_some() {
            self.update_hw(ctx);
            return self.display;
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
                        // Coming back to a frame sharpened before asks again.
                        self.still_wanted = None;
                    }
                }
                // Once the drag stops, sharpen to the full-quality exact frame.
                if !scrubbing && self.still_wanted != Some(want) {
                    self.still_wanted = Some(want);
                    let _ = self.still_tx.send((want, t));
                }
            }
        }
        self.display
    }

    /// [`Self::update`] with the hardware decoder.
    fn update_hw(&mut self, ctx: &egui::Context) {
        let Some(v) = self.video.as_ref() else { return };
        let want = self.info.frame_index(self.time());
        // Waiting to start: the sound goes once the first frame is here.
        if let Some(since) = self.pending_start {
            if v.has(want) || since.elapsed() >= START_WAIT {
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
                }
            }
            // …and the full-quality frame as soon as the decoder has it, also
            // mid-scrub: it keeps up with slow scrubs, and in fast ones only the
            // newest request is decoded. Asked once per frame wanted.
            let v = self.video.as_ref().expect("checked above");
            if self.still_wanted != Some(want) {
                self.still_wanted = Some(want);
                self.asked_at = Some(std::time::Instant::now());
                v.show(want);
            }
        }
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    fn show_picture(&mut self, ctx: &egui::Context, p: Picture) {
        let idx = p.index;
        if let Some(at) = self.played_at.take() {
            trace!("first frame after play: {idx}, {:.0} ms after pressing", at.elapsed().as_secs_f64() * 1000.0);
        }
        if debug() && self.sound_playing() {
            let (since, n) = self.shown_count.get_or_insert((std::time::Instant::now(), 0));
            *n += 1;
            if since.elapsed().as_secs_f64() >= 1.0 {
                trace!("playing: {n} frames shown in the last second (proxy {} ready)", self.proxy.ready());
                self.shown_count = None;
            }
        }
        #[cfg(windows)]
        {
            // On the GPU: draw its texture as it is.
            let on_gpu = p.gpu.as_ref().and_then(|s| self.gpu_frames.texture(s, p.width, p.height));
            match on_gpu {
                Some(id) => {
                    self.display = Some((id, egui::vec2(p.width as f32, p.height as f32)));
                    self.on_screen = Some(p);
                }
                None if p.rgba.is_empty() => {}
                None => self.set_texture(ctx, crate::video::to_image(p)),
            }
        }
        #[cfg(not(windows))]
        let _ = (ctx, p);
        self.shown_frame = Some(idx);
        self.showing_proxy = false;
    }

    fn show(&mut self, ctx: &egui::Context, idx: u64, frame: media::Frame) {
        let img = egui::ColorImage::from_rgba_unmultiplied([frame.width as usize, frame.height as usize], &frame.rgba);
        self.set_texture(ctx, img);
        self.shown_frame = Some(idx);
        self.showing_proxy = false;
    }

    fn set_texture(&mut self, ctx: &egui::Context, img: egui::ColorImage) {
        match &mut self.texture {
            Some(tex) => tex.set(img, egui::TextureOptions::LINEAR),
            None => self.texture = Some(ctx.load_texture("preview", img, egui::TextureOptions::LINEAR)),
        }
        self.display = self.texture.as_ref().map(|t| (t.id(), t.size_vec2()));
        self.on_screen = None;
    }
}

/// The hardware decoder for `source`, on Windows (unless
/// `HESTECLIPS_NO_HW_DECODE` is set, to compare with the ffmpeg path).
fn open_video(ctx: &egui::Context, source: &Path, fps: f64) -> Option<Video> {
    #[cfg(windows)]
    if std::env::var_os("HESTECLIPS_NO_HW_DECODE").is_none() {
        return Some(Video::open(ctx, source, PREVIEW_WIDTH, fps));
    }
    let _ = (ctx, source, fps);
    None
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
