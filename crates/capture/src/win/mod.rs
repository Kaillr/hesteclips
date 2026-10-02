//! Native Windows capture backend: Windows Graphics Capture + Media Foundation +
//! WASAPI, all in-process — no external tools.
//!
//! - **Video**: WGC delivers the monitor as BGRA textures on the GPU (`d3d`). A
//!   pacer thread converts the newest one to NV12 at the output size with the
//!   GPU's video processor and hands it to a Media Foundation H.264 encoder
//!   (`h264`) at exactly `fps`, so the output is constant frame rate even when
//!   the screen is static and WGC sends nothing. Frames never leave the GPU
//!   unless the software encoder is used.
//! - **Desktop and app audio**: WASAPI process loopback (`loopback`). **Mics**
//!   use cpal.
//! - **Mixing** happens in Rust (`crate::mixer`), which also drives the live
//!   meters; each track is AAC-encoded with Media Foundation (`aac`).
//! - **Writing** (`crate::writer`): our own MP4 muxer (`file`), either straight
//!   to a fragmented MP4 (record) or from an in-memory replay ring when a clip
//!   is saved.
//!
//! Everything is timestamped on QueryPerformanceCounter, the clock WGC and
//! WASAPI use: the first video frame is t=0.

mod aac;
mod d3d;
pub(crate) mod file;
mod h264;
mod loopback;
mod system;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

pub(crate) use loopback::SystemAudio;
pub(crate) use system::host_now;
pub use system::{list_apps, list_screens};

use crate::mixer::{self, Clock, LiveAudio};
use crate::mp4mux::Spec;
use crate::output::{in_progress, timestamp};
use crate::sources::{AudioCapture, mix_inputs, track_layout};
use crate::writer::{self, Media, Writer};
use crate::{EncodeSettings, Mode, Recorder};
use aac::AacEncoder;
use file::Layout;

/// AAC bitrate per audio track.
const AUDIO_BITRATE: u32 = 192_000;

pub struct WinRecorder {
    session: Option<Session>,
    mode: Option<Mode>,
    current_file: Option<PathBuf>,
    replay_seconds: u32,
    output_dir: PathBuf,
    container_ext: String,
    live: Arc<LiveAudio>,
}

impl WinRecorder {
    /// `live` receives the meters and supplies each source's volume.
    pub fn new(live: Arc<LiveAudio>) -> Self {
        Self {
            session: None,
            mode: None,
            current_file: None,
            replay_seconds: 60,
            output_dir: PathBuf::from("."),
            container_ext: "mp4".to_string(),
            live,
        }
    }
}

impl Recorder for WinRecorder {
    fn start(&mut self, mode: Mode, settings: &EncodeSettings) -> Result<()> {
        if self.session.is_some() {
            let _ = self.stop();
        }
        self.replay_seconds = settings.replay_seconds;
        self.output_dir = settings.output_dir.clone();
        // Our muxer writes MP4 and MOV; MKV isn't one of them.
        self.container_ext = if settings.container_ext == "mov" { "mov".into() } else { "mp4".into() };
        std::fs::create_dir_all(&settings.output_dir)?;

        let mut s = settings.clone();
        let target = match mode {
            Mode::Record => {
                let file = self.output_dir.join(format!("recording_{}.{}", timestamp(), self.container_ext));
                self.current_file = Some(file.clone());
                Target::Record(in_progress(&file))
            }
            Mode::ReplayBuffer => {
                // 1 s keyframes: a saved clip starts at most 1 s before the window.
                s.keyframe_interval_secs = 1;
                self.current_file = None;
                Target::Replay(settings.replay_seconds as f64)
            }
        };
        self.session = Some(Session::start(&s, target, &self.live, self.container_ext == "mov")?);
        self.mode = Some(mode);
        Ok(())
    }

    fn save_clip(&mut self) -> Result<PathBuf> {
        let session = self.session.as_ref().filter(|_| self.mode == Some(Mode::ReplayBuffer));
        let session = session.context("replay buffer is not running")?;
        let out = self.output_dir.join(format!("clip_{}.{}", timestamp(), self.container_ext));
        let partial = in_progress(&out);
        let (done_tx, done_rx) = mpsc::channel();
        session
            .writer_tx
            .send(writer::Command::SaveClip { out: partial.clone(), seconds: self.replay_seconds as f64, done: done_tx })
            .map_err(|_| anyhow::anyhow!("the capture stopped unexpectedly"))?;
        let result = done_rx.recv_timeout(Duration::from_secs(60)).map_err(|_| anyhow::anyhow!("saving the clip timed out"))?;
        if let Err(e) = result {
            let _ = std::fs::remove_file(&partial);
            return Err(e);
        }
        std::fs::rename(&partial, &out).context("couldn't finish saving the clip")?;
        Ok(out)
    }

    fn stop(&mut self) -> Result<Option<PathBuf>> {
        let finished = self.session.take().map(Session::finish);
        let file = self.current_file.take();
        let was_record = self.mode == Some(Mode::Record);
        self.mode = None;
        if !was_record {
            return Ok(None);
        }
        let Some(file) = file else { return Ok(None) };
        let partial = in_progress(&file);
        if let Some(Err(e)) = finished {
            // A fragmented MP4 is still playable up to the failure: keep what we got.
            if !partial.exists() {
                return Err(e.context("recording failed"));
            }
            eprintln!("recording ended with an error: {e:#}");
        }
        if !partial.exists() {
            bail!("recording failed — nothing was written");
        }
        std::fs::rename(&partial, &file).context("couldn't finish the recording")?;
        Ok(Some(file))
    }

    fn is_running(&self) -> bool {
        self.session.is_some()
    }
}

impl Drop for WinRecorder {
    /// Dropping (app quit) must still finalise the file.
    fn drop(&mut self) {
        if self.session.is_some() {
            let _ = self.stop();
        }
    }
}

/// Where a session's media goes.
enum Target {
    /// Straight to this (hidden, partial) file.
    Record(PathBuf),
    /// An in-memory ring of this many seconds.
    Replay(f64),
}

// ---------------------------------------------------------------------------
// Session: one running capture
// ---------------------------------------------------------------------------

/// How far behind real time the mixer runs, so every source's audio has arrived
/// (WASAPI delivers in ~10 ms packets, sometimes late). Timestamps are exact, so
/// this only delays writing — it never shifts audio against video.
const MIX_LATENCY: f64 = 0.3;

/// Everything a running capture owns. Torn down in `finish` in an order that
/// lets every encoder flush into the writer before it closes the file.
struct Session {
    capture: Option<d3d::Capture>,
    pacer: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
    audio: Option<AudioCapture>,
    mixer: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
    /// AAC encoder threads, one per track; they end when the mixer drops its senders.
    encoders: Vec<JoinHandle<()>>,
    writer_tx: Sender<writer::Command>,
    writer: Option<Writer>,
}

impl Session {
    fn start(s: &EncodeSettings, target: Target, live: &Arc<LiveAudio>, mov: bool) -> Result<Self> {
        system::com_init();
        system::mf_startup()?;
        let gpu = d3d::Gpu::new()?;
        let item = d3d::capture_item(system::find_monitor(&s.screen_id))?;
        let native = item.Size()?;
        let (width, height) = output_size(native.Width.max(2) as u32, native.Height.max(2) as u32, s.target_height);
        let clock = Arc::new(Clock::default());

        let (titles, comment, has_rest) = track_layout(&s.sources);
        let aac: Vec<AacEncoder> = titles.iter().map(|_| AacEncoder::new(AUDIO_BITRATE)).collect::<Result<_>>()?;
        let layout = Layout {
            spec: Spec { width, height, fps: s.fps, audio_titles: titles, audio_bitrate: AUDIO_BITRATE, comment, mov },
        };
        let writer = match target {
            Target::Record(file) => Writer::record(file, layout, s.fps),
            Target::Replay(seconds) => Writer::replay(seconds, layout, s.fps),
        };
        let writer_tx = writer.tx.clone();

        let mut session =
            Self { capture: None, pacer: None, audio: None, mixer: None, encoders: Vec::new(), writer_tx, writer: Some(writer) };
        let started = (|| -> Result<()> {
            // One PCM channel + AAC thread per track, in layout order: the mix,
            // each own track, the rest.
            let mut pcm_senders = Vec::new();
            for (track, enc) in aac.into_iter().enumerate() {
                let (tx, rx) = mpsc::channel::<Vec<f32>>();
                pcm_senders.push(tx);
                session.encoders.push(spawn_aac(track, enc, rx, session.writer_tx.clone()));
            }
            let mut tracks = pcm_senders.into_iter();
            let mix_track = tracks.next();
            let audio = AudioCapture::start(&s.sources, live, clock.clone())?;
            let inputs = mix_inputs(&s.sources, &audio.feeds, live, |_| tracks.next());
            let rest_track = if has_rest { tracks.next() } else { None };
            session.audio = Some(audio);
            let stop = Arc::new(AtomicBool::new(false));
            let mixer = mixer::spawn_mixer(
                inputs,
                mix_track,
                rest_track,
                live.clone(),
                clock.clone(),
                host_now,
                MIX_LATENCY,
                stop.clone(),
            );
            session.mixer = Some((stop, mixer));

            let encoder = h264::Encoder::start(&gpu, width, height, s, session.writer_tx.clone())?;
            let capture = d3d::Capture::start(&gpu, &item)?;
            let converter = d3d::Converter::new(&gpu, &capture.latest, &encoder.pool, width, height, s.fps)?;
            let latest = capture.latest.clone();
            session.capture = Some(capture);

            // Wait for the first frame so a capture that can't see the screen fails
            // here, visibly, instead of producing an empty file.
            let deadline = Instant::now() + Duration::from_secs(3);
            while !latest.has_frame() {
                if Instant::now() >= deadline {
                    bail!("the screen isn't delivering frames");
                }
                thread::sleep(Duration::from_millis(10));
            }
            session.pacer = Some(spawn_pacer(s.fps, latest, converter, encoder, clock));
            Ok(())
        })();
        if let Err(e) = started {
            let _ = session.finish();
            return Err(e);
        }
        Ok(session)
    }

    /// Stop capturing and finish the file (record) or drop the ring (replay).
    fn finish(mut self) -> Result<()> {
        // The pacer flushes the video encoder on its way out.
        if let Some((stop, handle)) = self.pacer.take() {
            stop.store(true, Ordering::Relaxed);
            let _ = handle.join();
        }
        self.capture.take();
        if let Some(audio) = self.audio.take() {
            audio.stop();
        }
        // The mixer writes what's left and drops its senders; the AAC threads then
        // flush and exit.
        if let Some((stop, handle)) = self.mixer.take() {
            stop.store(true, Ordering::Relaxed);
            let _ = handle.join();
        }
        for h in self.encoders.drain(..) {
            let _ = h.join();
        }
        drop(self.writer_tx);
        self.writer.take().map_or(Ok(()), Writer::join)
    }
}

/// AAC-encode one track's PCM and pass the packets to the writer.
fn spawn_aac(track: usize, mut enc: AacEncoder, rx: mpsc::Receiver<Vec<f32>>, out: Sender<writer::Command>) -> JoinHandle<()> {
    thread::spawn(move || {
        system::com_init();
        while let Ok(pcm) = rx.recv() {
            for packet in enc.push(&pcm) {
                if out.send(writer::Command::Media(Media::Audio { track, packet })).is_err() {
                    return;
                }
            }
        }
    })
}

/// Output size: the display's native pixels, downscaled to `target_height` if
/// that's smaller. Even dimensions, as H.264 4:2:0 requires.
fn output_size(mut w: u32, mut h: u32, target_height: Option<u32>) -> (u32, u32) {
    if let Some(t) = target_height.filter(|&t| t < h) {
        w = (w * t + h / 2) / h;
        h = t;
    }
    (w & !1, h & !1)
}

// ---------------------------------------------------------------------------
// Pacer: constant frame rate
// ---------------------------------------------------------------------------

/// Encode the newest screen image every 1/fps on the host clock. WGC only sends
/// frames when the screen changes; repeating the last one keeps the output CFR.
/// Frame N is stamped N/fps seconds after t0, the same clock the audio uses.
fn spawn_pacer(
    fps: u32,
    latest: Arc<d3d::Latest>,
    converter: d3d::Converter,
    encoder: h264::Encoder,
    clock: Arc<Clock>,
) -> (Arc<AtomicBool>, JoinHandle<()>) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let handle = thread::spawn(move || {
        let fps_f = fps as f64;
        let t0 = host_now();
        clock.set(t0);
        let mut next: i64 = 0;
        let mut dropped = 0u64;
        while !stop2.load(Ordering::Relaxed) {
            let now = host_now();
            let due = ((now - t0) * fps_f) as i64;
            // After a stall, catch up gradually rather than in one huge burst.
            let mut burst = 0;
            while next <= due && burst < fps * 2 {
                if latest.has_frame() {
                    match encoder.pool.take() {
                        Some(slot) => match converter.convert(slot) {
                            Ok(()) => encoder.encode(slot, next),
                            Err(e) => {
                                encoder.pool.give(slot);
                                eprintln!("{e:#}");
                            }
                        },
                        // The encoder can't keep up: skip this frame (the file
                        // shows the previous one a little longer).
                        None => {
                            dropped += 1;
                            if dropped.is_power_of_two() {
                                eprintln!("video encoder is behind: {dropped} frames skipped");
                            }
                        }
                    }
                }
                next += 1;
                burst += 1;
            }
            let wake = t0 + next as f64 / fps_f;
            let wait = wake - host_now();
            if wait > 0.0 {
                thread::sleep(Duration::from_secs_f64(wait));
            }
        }
        encoder.finish();
    });
    (stop, handle)
}

#[cfg(test)]
mod tests {
    use super::output_size;

    #[test]
    fn output_size_keeps_aspect_and_even() {
        assert_eq!(output_size(2560, 1440, None), (2560, 1440));
        assert_eq!(output_size(2560, 1440, Some(1080)), (1920, 1080));
        assert_eq!(output_size(1920, 1080, Some(1440)), (1920, 1080));
        assert_eq!(output_size(3440, 1440, Some(721)), (1722, 720));
    }
}
