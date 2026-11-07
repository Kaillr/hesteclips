//! What a running capture does with its pictures and sound — clipping,
//! recording, streaming, any of them at once — kept the same way by every
//! backend. A backend starts a capture (its session: the screen, the sound,
//! the encoders, and the one writer they all feed) when the first of them
//! starts, and stops it with the last; which of them run is kept here, and
//! each is started and stopped by telling the session's writer
//! (`crate::writer`).
//!
//! What the capture was started with (its size, frame rate, encoder,
//! sources) stays while it runs: what joins it later takes only what's its
//! own from the settings it's started with (the clip length, the stream).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use crate::output::{in_progress, timestamp};
use crate::stream::StreamSettings;
use crate::writer::{self, Command};
use crate::{EncodeSettings, Mode, Running};

/// A running capture, as a backend has it.
pub(crate) trait Capture: Sized {
    /// Its writer.
    fn writer(&self) -> &Sender<Command>;

    /// Start the stream's own pictures (an encoder of its own, at the
    /// stream's size, frame rate and bitrate), or stop them (`None`).
    fn stream_video(&mut self, stream: Option<&StreamSettings>) -> Result<()> {
        match stream {
            Some(_) => bail!("streaming isn't available on this system yet"),
            None => Ok(()),
        }
    }

    /// Stop capturing: every encoder flushes into the writer, which then
    /// finishes. What the writer said (a recording running is finished).
    fn finish(self) -> Result<()>;
}

/// A backend's capture and what it's doing.
pub(crate) struct Outputs<C> {
    pub session: Option<C>,
    running: Running,
    /// The recording's own name (written to `in_progress` of it until it's
    /// done).
    file: Option<PathBuf>,
    replay_seconds: u32,
    container_ext: String,
}

impl<C: Capture> Outputs<C> {
    pub(crate) fn new() -> Self {
        Self { session: None, running: Running::default(), file: None, replay_seconds: 60, container_ext: "mp4".into() }
    }

    pub(crate) fn running(&self) -> Running {
        self.running
    }

    /// Start `mode`. When nothing runs, the capture starts first, with
    /// `start` (given the settings, and whether files are MOV).
    pub(crate) fn start(&mut self, mode: Mode, s: &EncodeSettings, start: impl FnOnce(&EncodeSettings, bool) -> Result<C>) -> Result<()> {
        if self.running.has(mode) {
            return Ok(());
        }
        if self.session.is_none() {
            // Our muxers write MP4 and MOV; MKV isn't one of them.
            self.container_ext = if s.container_ext == "mov" { "mov".into() } else { "mp4".into() };
            std::fs::create_dir_all(&s.output_dir)?;
            self.session = Some(start(s, self.container_ext == "mov")?);
        }
        match self.begin(mode, s) {
            Ok(()) => {
                self.running = self.running.with(mode, true);
                Ok(())
            }
            Err(e) => {
                // Nothing else runs: the capture started for it stops.
                if !self.running.any()
                    && let Some(c) = self.session.take()
                {
                    let _ = c.finish();
                }
                Err(e)
            }
        }
    }

    fn begin(&mut self, mode: Mode, s: &EncodeSettings) -> Result<()> {
        let session = self.session.as_mut().context("the capture isn't running")?;
        let tx = session.writer().clone();
        let send = move |c: Command| tx.send(c).map_err(|_| anyhow!("the capture stopped unexpectedly"));
        match mode {
            Mode::ReplayBuffer => {
                self.replay_seconds = s.replay_seconds;
                send(Command::Replay(Some(s.replay_seconds as f64)))
            }
            Mode::Record => {
                std::fs::create_dir_all(&s.output_dir)?;
                let file = s.output_dir.join(format!("recording_{}.{}", timestamp(), self.container_ext));
                send(Command::Record(in_progress(&file)))?;
                self.file = Some(file);
                Ok(())
            }
            Mode::Stream => {
                let stream = s.stream.as_ref().filter(|st| !st.destinations.is_empty()).context("there's nowhere to stream to")?;
                // The writer streams first, so the encoder's first picture
                // (a keyframe, where every connection starts) goes out.
                send(Command::Stream(Some(stream.clone())))?;
                let started = session.stream_video(Some(stream));
                if started.is_err() {
                    let _ = send(Command::Stream(None));
                }
                started
            }
        }
    }

    /// Stop `mode`; the capture stops with the last. Stopping a recording
    /// finishes its file, moved into `dir` if one's given: the file.
    pub(crate) fn stop(&mut self, mode: Mode, dir: Option<&Path>) -> Result<Option<PathBuf>> {
        if !self.running.has(mode) {
            return Ok(None);
        }
        self.running = self.running.with(mode, false);
        let last = !self.running.any();
        let mut written: Option<Result<()>> = None;
        if let Some(session) = self.session.as_mut() {
            let tx = session.writer().clone();
            match mode {
                Mode::ReplayBuffer => {
                    let _ = tx.send(Command::Replay(None));
                }
                Mode::Stream => {
                    let _ = tx.send(Command::Stream(None));
                    let _ = session.stream_video(None);
                }
                // The last thing running: the capture stops below, and its
                // writer finishes the file with every encoder's last.
                Mode::Record if last => {}
                Mode::Record => {
                    let (done_tx, done_rx) = mpsc::channel();
                    written = Some(match tx.send(Command::EndRecord(done_tx)) {
                        Ok(()) => done_rx.recv_timeout(Duration::from_secs(30)).unwrap_or_else(|_| Err(anyhow!("the recording didn't finish"))),
                        Err(_) => Err(anyhow!("the capture stopped unexpectedly")),
                    });
                }
            }
        }
        if last {
            let finished = self.session.take().map(C::finish);
            match finished {
                Some(r) if mode == Mode::Record => written = Some(r),
                Some(Err(e)) => eprintln!("the capture ended with an error: {e:#}"),
                _ => {}
            }
        }
        if mode != Mode::Record {
            return Ok(None);
        }
        let Some(file) = self.file.take() else { return Ok(None) };
        let partial = in_progress(&file);
        if let Some(Err(e)) = written {
            // A fragmented MP4 is still playable up to the failure: keep what we got.
            if !partial.exists() {
                return Err(e.context("recording failed"));
            }
            eprintln!("recording ended with an error: {e:#}");
        }
        if !partial.exists() {
            bail!("recording failed — nothing was written");
        }
        let file = crate::output::destination(&file, dir);
        crate::output::finish_rename(&partial, &file).context("couldn't finish the recording")?;
        Ok(Some(file))
    }

    /// Stop everything (the app quitting): a recording is finished where
    /// it's written.
    pub(crate) fn stop_all(&mut self) {
        for mode in [Mode::Stream, Mode::ReplayBuffer, Mode::Record] {
            if let Err(e) = self.stop(mode, None) {
                eprintln!("stopping: {e:#}");
            }
        }
        if let Some(c) = self.session.take() {
            let _ = c.finish();
        }
    }

    /// Save a clip of what clipping has kept, in `dir`.
    pub(crate) fn save_clip(&mut self, dir: &Path) -> Result<crate::PendingClip> {
        let session = self.session.as_ref().filter(|_| self.running.replay).context("clipping isn't running")?;
        std::fs::create_dir_all(dir)?;
        writer::request_clip(session.writer(), dir, &self.container_ext, self.replay_seconds as f64)
    }

    /// How far back clipping reaches (and Save clip saves), from now on.
    pub(crate) fn set_replay_seconds(&mut self, seconds: u32) {
        self.replay_seconds = seconds;
        if let Some(session) = self.session.as_ref().filter(|_| self.running.replay) {
            let _ = session.writer().send(Command::SetWindow(seconds as f64));
        }
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
pub(crate) mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use crate::stream::{Destination, Link, Quality, Status, StreamSettings};
    use crate::{BlendShape, EncodeSettings, Mode, Recorder, Running, VideoSource};

    /// As Windows' `everything_at_once` (`crate::win`), for the backend
    /// `rec`: clipping, then going live to two places (RTMP servers here:
    /// ffmpegs listening), each sent a picture of its own, frame blending
    /// turned on and off while it all runs, a recording beside them, a clip
    /// saved: each gets what it should, from the one capture. The capture's
    /// 1080p, at 60 fps where the display shows 120 pictures a second or
    /// more, else at 30, so two looks a frame are as many as it shows. The
    /// places at their own sizes and rates: 720p at 30 (at 60, its frames
    /// blended from the capture's two while blending's on), and as the
    /// capture is; the recording and the clip at the capture's.
    pub(crate) fn everything_at_once(rec: &mut dyn Recorder) {
        /// What's taken of the screen goes, and the servers stop, however
        /// the test ends.
        struct Leave(PathBuf, Vec<std::process::Child>);
        impl Drop for Leave {
            fn drop(&mut self) {
                for c in &mut self.1 {
                    let _ = c.kill();
                    let _ = c.wait();
                }
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let screen = crate::list_screens().into_iter().next().expect("a screen");
        let video = VideoSource::Screen { id: screen.id.clone(), hook: crate::GameHook { auto: false, allowed: Vec::new() }, method: Default::default() };
        let fps = if crate::max_blend_rate(&video).unwrap_or(120) >= 120 { 60 } else { 30 };
        let size = crate::VideoSize { short_side: Some(1080), shape: None, exact: None };
        let (_, height) = crate::frame_size(&video, size).expect("the screen's size");
        let dir = std::env::temp_dir().join(format!("hc-at-once-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut leave = Leave(dir.clone(), Vec::new());
        let mut urls = Vec::new();
        for name in ["small", "full"] {
            let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
            let url = format!("rtmp://127.0.0.1:{port}/live/test_key");
            let got = dir.join(format!("{name}.flv"));
            leave.1.push(crate::stream::ffmpeg().args(["-v", "error", "-y", "-timeout", "30", "-i"]).arg(&url).args(["-c", "copy"]).arg(&got).stdin(std::process::Stdio::null()).spawn().unwrap());
            urls.push(url);
        }
        thread::sleep(Duration::from_millis(700));
        let status = Status::new();
        let mut settings = EncodeSettings {
            output_dir: dir.clone(),
            container_ext: "mp4".into(),
            fps,
            blend_samples: 1,
            blend_shape: BlendShape::Even,
            video_bitrate_kbps: 12000,
            size,
            use_hardware: true,
            replay_seconds: 10,
            video,
            away_screen: None,
            webcam: None,
            layers: crate::layers::main_only(),
            screen: Arc::new(Mutex::new(crate::ScreenPlacement::whole(crate::Framing::Fit))),
            sources: Vec::new(),
            stream: None,
        };
        let place = |name: &str, url: &str, short_side, fps, kbps| Destination { name: name.into(), url: url.into(), secret: "test_key".into(), quality: Quality { short_side: Some(short_side), fps, kbps } };
        settings.stream = Some(StreamSettings { destinations: vec![place("Small", &urls[0], 720, 30, 3000), place("Full", &urls[1], 1080, fps, 6000)], status: status.clone() });
        rec.start(Mode::ReplayBuffer, &settings).unwrap();
        thread::sleep(Duration::from_secs(1));
        rec.start(Mode::Stream, &settings).unwrap();
        assert_eq!(rec.running(), Running { replay: true, record: false, stream: true });
        let deadline = Instant::now() + Duration::from_secs(10);
        while !status.get().iter().all(|d| matches!(d.link, Link::Live { .. })) {
            assert!(Instant::now() < deadline, "never live: {:?}", status.get());
            thread::sleep(Duration::from_millis(50));
        }
        // Frame blending on, and off again, while it runs.
        assert!(rec.set_blend(2, BlendShape::Soft), "blending didn't change");
        thread::sleep(Duration::from_secs(2));
        let blending = crate::health().and_then(|h| h.blend).map(|b| b.looks);
        rec.start(Mode::Record, &settings).unwrap();
        thread::sleep(Duration::from_secs(2));
        assert!(rec.set_blend(1, BlendShape::Even));
        thread::sleep(Duration::from_secs(1));
        let clip = rec.save_clip(&dir).unwrap().finish().unwrap();
        let recording = rec.stop(Mode::Record, None).unwrap().expect("a recording");
        assert_eq!(rec.running(), Running { replay: true, record: false, stream: true });
        rec.stop(Mode::Stream, None).unwrap();
        rec.stop(Mode::ReplayBuffer, None).unwrap();
        assert!(!rec.is_running());
        let deadline = Instant::now() + Duration::from_secs(10);
        while leave.1.iter_mut().any(|c| c.try_wait().unwrap().is_none()) {
            assert!(Instant::now() < deadline, "a server didn't end");
            thread::sleep(Duration::from_millis(50));
        }
        let probe = |f: &Path| {
            let out = std::process::Command::new("ffprobe")
                .args(["-v", "error", "-select_streams", "v:0", "-count_frames", "-show_entries", "stream=width,height,avg_frame_rate,nb_read_frames:format=duration", "-of", "default=nw=1"])
                .arg(f)
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).to_string()
        };
        let (s, full, r, c) = (probe(&dir.join("small.flv")), probe(&dir.join("full.flv")), probe(&recording), probe(&clip));
        eprintln!("blending while it ran: {blending:?} looks\nsmall stream:\n{s}\nfull stream:\n{full}\nrecording:\n{r}\nclip:\n{c}");
        let field = |t: &str, name: &str| t.lines().find_map(|l| l.strip_prefix(name)?.strip_prefix('=').map(str::to_owned)).unwrap_or_default();
        // Frames a second, from "60/1" (or what it averaged, a frame skipped).
        let rate = |t: &str| {
            let r = field(t, "avg_frame_rate");
            let (n, d) = r.split_once('/').unwrap_or((r.as_str(), "1"));
            n.parse::<f64>().unwrap_or(0.0) / d.parse::<f64>().unwrap_or(1.0).max(1e-9)
        };
        let secs = |t: &str| field(t, "duration").parse::<f64>().unwrap_or(0.0);
        assert_eq!(blending, Some(2), "blending changed while running");
        assert_eq!(field(&s, "height"), "720", "{s}");
        assert!((rate(&s) - 30.0).abs() < 0.5, "the small stream at 30 fps: {s}");
        assert_eq!(field(&full, "height"), height.to_string(), "{full}");
        assert!((rate(&full) - fps as f64).abs() < 0.5, "the full stream at {fps} fps: {full}");
        assert_eq!(field(&r, "height"), height.to_string(), "{r}");
        assert!((rate(&r) - fps as f64).abs() < 0.5, "the recording at {fps} fps: {r}");
        assert_eq!(field(&c, "height"), height.to_string(), "{c}");
        assert!(secs(&s) > 4.0, "the stream ran on: {}", secs(&s));
        assert!((2.5..4.5).contains(&secs(&r)), "the recording's 3 s: {}", secs(&r));
    }
}
