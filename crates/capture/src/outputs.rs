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
