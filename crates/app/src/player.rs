//! Preview playback for the editor.
//!
//! Audio drives the clock: every source track is decoded up front to stereo f32
//! and mixed live in the cpal callback at the current gains, so moving a slider is
//! heard instantly. The playhead is however many samples the callback has played.
//!
//! Video follows the clock: an ffmpeg process streams scaled RGBA frames from the
//! playhead onward, and the UI shows whichever decoded frame matches the clock.
//! When paused or scrubbing, single exact frames are decoded on demand instead.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use media::{ClipInfo, PREVIEW_RATE, TrackEdit};

use crate::proxy::Proxy;

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
        Self {
            source: source.to_path_buf(),
            info,
            shared,
            _stream: stream,
            audio_error,
            texture: None,
            shown_frame: None,
            decoder: None,
            still_tx,
            still_rx,
            still_wanted: None,
            proxy: Proxy::build(ctx, source, info_fps),
            showing_proxy: false,
        }
    }

    pub fn time(&self) -> f64 {
        self.shared.pos.load(Ordering::Relaxed) as f64 / PREVIEW_RATE as f64
    }

    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    /// Jump to `t` (seconds). Keeps playing if it was.
    pub fn seek(&mut self, t: f64) {
        let t = t.clamp(0.0, self.info.duration);
        self.shared.pos.store((t * PREVIEW_RATE as f64) as u64, Ordering::Relaxed);
        self.decoder = None; // restarts from the new position if playing
    }

    /// Play from the current position until `until` seconds.
    pub fn play(&mut self, until: f64) {
        self.shared.end.store((until * PREVIEW_RATE as f64) as u64, Ordering::Relaxed);
        self.shared.playing.store(true, Ordering::Relaxed);
    }

    pub fn pause(&mut self) {
        self.shared.playing.store(false, Ordering::Relaxed);
        self.decoder = None;
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
    pub fn update(&mut self, ctx: &egui::Context, scrubbing: bool) -> Option<&egui::TextureHandle> {
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
                    }
                }
                // Once the drag stops, sharpen to the full-quality exact frame.
                if !scrubbing && self.still_wanted != Some(want) {
                    self.still_wanted = Some(want);
                    let _ = self.still_tx.send((want, t));
                }
            }
        }
        self.texture.as_ref()
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

    let stream = device
        .build_output_stream::<f32, _, _>(
            config,
            move |out: &mut [f32], _| {
                out.fill(0.0);
                if !shared.playing.load(Ordering::Relaxed) {
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
