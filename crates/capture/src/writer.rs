//! Writing captured media, shared by the capture backends.
//!
//! The recorder sends [`Media`] (encoded H.264 frames, AAC packets per audio
//! track, and the stream's own pictures) to one writer thread with exact
//! timestamps, which hands them to whatever runs, any of these at once:
//!
//! - **Clipping** (the replay buffer): encoded media stays in a ring in
//!   memory, a little longer than the replay window. "Save clip" takes
//!   everything from the keyframe nearest (now − replay window) and writes it
//!   to a file on another thread, so capture never pauses. Nothing touches
//!   the disk until you save.
//! - **Recording**: everything goes straight into the platform's file
//!   writer, which writes a fragmented MP4 — if the app dies mid-recording,
//!   everything up to the last fragment (≤ 1 s) still plays. Started while
//!   clipping, it begins at the newest keyframe the ring has (so at once);
//!   else at the next one. Ended while the capture goes on, it ends at the
//!   newest picture once the sound has caught up with it.
//! - **Streaming** (`crate::stream`): the stream's own pictures and the mix
//!   track's sound go to each destination.
//!
//! The file itself is written by `FileWriter`: AVAssetWriter on macOS
//! (`avwriter`), our own MP4 muxer on Windows and Linux (`mp4file`).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

#[cfg(target_os = "macos")]
pub(crate) use crate::avwriter::{FileWriter, Layout, VideoFrame};
use crate::mixer::RATE;
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(crate) use crate::mp4file::{FileWriter, Layout, VideoFrame};
use crate::stream::{Sink, StreamFrame, StreamSettings};

/// One encoded AAC packet and the frame (48 kHz sample index) it starts at.
pub struct AacPacket {
    pub data: Vec<u8>,
    pub frame: i64,
}

/// How long an AAC packet's sound lasts (1024 samples).
const PACKET_SECS: f64 = 1024.0 / RATE as f64;

/// Something to write: a video frame, one AAC packet for audio track
/// `track`, or one of the stream's own pictures (from its encoder).
pub(crate) enum Media {
    Video { frame: VideoFrame, pts: f64, key: bool },
    Audio { track: usize, packet: AacPacket },
    // Only Windows has a stream encoder so far (`crate::STREAMING`).
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    Stream(StreamFrame),
}

pub(crate) enum Command {
    Media(Media),
    /// Clipping: write the last `seconds` to `out`, report the result.
    SaveClip { out: PathBuf, seconds: f64, done: Sender<Result<PathBuf>> },
    /// Clipping: keep this many seconds from now on. Longer fills up as it
    /// goes; shorter lets the oldest go at the next keyframe.
    SetWindow(f64),
    /// Start clipping, keeping this many seconds (`Some`), or stop and let
    /// go of what's kept (`None`).
    Replay(Option<f64>),
    /// Start recording into this file (hidden, partial: the caller gives it
    /// its name once it's done).
    Record(PathBuf),
    /// End the recording, while the capture goes on: at the newest picture,
    /// once every track's sound has reached it (a second at the most). Then
    /// the file's finished, and `done` says how that went.
    EndRecord(Sender<Result<()>>),
    /// Start streaming to these destinations (`Some`), or end the stream.
    Stream(Option<StreamSettings>),
}

/// Ask the replay writer behind `tx` for a clip of the last `seconds` in `dir`.
/// The clip ends at the newest picture the writer has when it gets this, so
/// it's of now (its sound, which comes a little later, is waited for: see
/// [`Asked`]); the returned clip finishes once the file is written.
pub(crate) fn request_clip(tx: &Sender<Command>, dir: &Path, ext: &str, seconds: f64) -> Result<crate::PendingClip> {
    let out = crate::output::new_clip_path(dir, ext);
    let partial = crate::output::in_progress(&out);
    let (done_tx, done_rx) = mpsc::channel();
    tx.send(Command::SaveClip { out: partial.clone(), seconds, done: done_tx })
        .map_err(|_| anyhow!("the capture stopped unexpectedly"))?;
    Ok(crate::PendingClip::new(move || {
        let result = done_rx
            .recv_timeout(std::time::Duration::from_secs(300))
            .map_err(|_| anyhow!("saving the clip timed out"))?;
        if let Err(e) = result {
            let _ = std::fs::remove_file(&partial);
            return Err(e);
        }
        crate::output::finish_rename(&partial, &out).context("couldn't finish saving the clip")?;
        Ok(out)
    }))
}

/// The writer thread. Dropping the sender ends it; `join` finishes the file.
pub(crate) struct Writer {
    pub tx: Sender<Command>,
    thread: Option<JoinHandle<Result<()>>>,
}

impl Writer {
    /// A writer with nothing running yet: clipping, recording and streaming
    /// are each started (and stopped) by their own [`Command`]. When every
    /// sender's gone, a recording still running is finished, and what that
    /// gave is what [`Self::join`] says.
    pub(crate) fn new(layout: Layout, fps: u32) -> Self {
        Self::spawn(move |rx| writer_loop(rx, layout, fps))
    }

    /// Record straight to `file` (hidden partial path; the caller renames it).
    pub(crate) fn record(file: PathBuf, layout: Layout, fps: u32) -> Self {
        let w = Self::new(layout, fps);
        let _ = w.tx.send(Command::Record(file));
        w
    }

    fn spawn(f: impl FnOnce(Receiver<Command>) -> Result<()> + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        Self { tx, thread: Some(thread::spawn(move || f(rx))) }
    }

    /// Wait for the writer to finish (after every sender is dropped).
    pub(crate) fn join(mut self) -> Result<()> {
        drop(self.tx);
        match self.thread.take().map(JoinHandle::join) {
            Some(Ok(r)) => r,
            Some(Err(_)) => bail!("the file writer crashed"),
            None => Ok(()),
        }
    }
}

fn writer_loop(rx: Receiver<Command>, layout: Layout, fps: u32) -> Result<()> {
    let tracks = layout.audio_tracks();
    let mut ring: Option<Ring> = None;
    let mut asked = Asked::default();
    let mut recording: Option<Recording> = None;
    let mut stream: Option<Sink> = None;
    let mut saves: Vec<JoinHandle<()>> = Vec::new();
    let mut save = |ask: Ask, items: Vec<(f64, Media)>| {
        let layout = layout.clone();
        // Writing happens off this thread so the ring keeps filling.
        saves.push(thread::spawn(move || {
            let _ = ask.done.send(write_clip(&ask.out, &items, &layout, fps));
        }));
        saves.retain(|h| !h.is_finished());
    };
    loop {
        // While a clip waits for its sound, or a recording that's ending
        // does, wake when it stops waiting even if nothing comes.
        let wake = [asked.until(), recording.as_ref().and_then(Recording::until)].into_iter().flatten().min();
        let cmd = match wake {
            None => rx.recv().map(Some).map_err(|_| ()),
            Some(until) => match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
                Ok(cmd) => Ok(Some(cmd)),
                Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
                Err(mpsc::RecvTimeoutError::Disconnected) => Err(()),
            },
        };
        let Ok(cmd) = cmd else { break };
        match cmd {
            Some(Command::Media(m)) => {
                if let Some(s) = &mut stream {
                    match &m {
                        Media::Stream(f) => s.video(f),
                        Media::Audio { track: 0, packet } => s.audio(&packet.data, packet.frame),
                        _ => {}
                    }
                }
                if !matches!(m, Media::Stream(_)) {
                    if let Some(r) = &mut recording {
                        r.push(&m, &layout, fps);
                    }
                    if let Some(ring) = &mut ring {
                        ring.push(m);
                    }
                }
            }
            Some(Command::SetWindow(window)) => {
                if let Some(ring) = &mut ring {
                    ring.keep = window + 2.0;
                }
            }
            Some(Command::Replay(Some(window))) => match &mut ring {
                Some(ring) => ring.keep = window + 2.0,
                None => ring = Some(Ring::new(window + 2.0)),
            },
            Some(Command::Replay(None)) => {
                // Clips still waiting for their sound get what there is.
                if let Some(ring) = ring.take() {
                    for (ask, items) in asked.ready(&ring, tracks, Instant::now() + Asked::WAIT) {
                        save(ask, items);
                    }
                }
            }
            Some(Command::SaveClip { out, seconds, done }) => match ring.as_ref() {
                None => {
                    let _ = done.send(Err(anyhow!("clipping isn't running")));
                }
                Some(ring) => match ring.newest_video() {
                    Some(end) => asked.0.push(Ask { out, seconds, done, end, until: Instant::now() + Asked::WAIT }),
                    // No picture yet: nothing to wait for (`write_clip` says so).
                    None => save(Ask { out, seconds, done, end: 0.0, until: Instant::now() }, Vec::new()),
                },
            },
            Some(Command::Record(file)) => {
                if let Some(old) = recording.take() {
                    let _ = old.finish();
                }
                let mut r = Recording::new(file, tracks);
                // Clipping: from the newest keyframe it has, so it starts now.
                if let Some(ring) = &ring {
                    for m in ring.since_newest_key() {
                        r.push(m, &layout, fps);
                    }
                }
                recording = Some(r);
            }
            Some(Command::EndRecord(done)) => match &mut recording {
                Some(r) => r.end(done),
                None => {
                    let _ = done.send(Err(anyhow!("nothing was recorded")));
                }
            },
            Some(Command::Stream(Some(s))) => {
                if let Some(old) = stream.take() {
                    old.stop();
                }
                stream = Some(Sink::start(&s, tracks > 0));
            }
            // Each destination's ffmpeg ends its stream properly, which can
            // take a moment: waited for off this thread, so the ring and the
            // recording keep up meanwhile.
            Some(Command::Stream(None)) => {
                if let Some(s) = stream.take() {
                    let _ = thread::Builder::new().name("stream-end".into()).spawn(move || s.stop());
                }
            }
            None => {}
        }
        if let Some(ring) = &mut ring {
            for (ask, items) in asked.ready(ring, tracks, Instant::now()) {
                save(ask, items);
            }
            ring.hold = asked.hold();
        }
        if recording.as_ref().is_some_and(|r| r.ended(Instant::now()))
            && let Some(r) = recording.take()
        {
            r.finish_ending();
        }
    }
    // The capture stopped (its sound is all in): clips still waiting get
    // what there is.
    if let Some(ring) = &ring {
        for (ask, items) in asked.ready(ring, tracks, Instant::now() + Asked::WAIT) {
            save(ask, items);
        }
    }
    if let Some(s) = stream.take() {
        s.stop();
    }
    // Let pending saves finish before the app exits.
    for h in saves {
        let _ = h.join();
    }
    match recording.take() {
        Some(r) if r.ending.is_some() => {
            r.finish_ending();
            Ok(())
        }
        Some(r) => r.finish(),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Recording
// ---------------------------------------------------------------------------

/// A recording being written, straight to its file.
struct Recording {
    file: PathBuf,
    /// Opened at the first keyframe, so the file opens on a clean picture.
    writer: Option<FileWriter>,
    /// The first thing that went wrong: what's written up to it is kept.
    error: Option<anyhow::Error>,
    /// The newest picture written, and how far each track's sound has got
    /// (the start of its newest packet).
    newest_video: Option<f64>,
    sound: Vec<Option<f64>>,
    ending: Option<Ending>,
}

/// A recording asked to end while the capture goes on.
struct Ending {
    /// The newest picture when it was asked: where it ends.
    end: f64,
    /// When to stop waiting for its sound.
    until: Instant,
    done: Sender<Result<()>>,
}

impl Recording {
    fn new(file: PathBuf, tracks: usize) -> Self {
        Self { file, writer: None, error: None, newest_video: None, sound: vec![None; tracks], ending: None }
    }

    fn push(&mut self, media: &Media, layout: &Layout, fps: u32) {
        if self.error.is_some() {
            return; // keep draining so capture never blocks on us
        }
        let t = media_time(media);
        // Ending: nothing after its end.
        if self.ending.as_ref().is_some_and(|e| t > e.end + 1e-9) {
            return;
        }
        if self.writer.is_none() {
            let Media::Video { pts, key: true, .. } = media else { return };
            match FileWriter::create(&self.file, layout, *pts, fps, true) {
                Ok(w) => self.writer = Some(w),
                Err(e) => {
                    self.error = Some(e);
                    return;
                }
            }
        }
        if let Err(e) = self.writer.as_mut().map_or(Ok(()), |w| w.append(media)) {
            self.error = Some(e);
            return;
        }
        match media {
            Media::Video { pts, .. } => self.newest_video = Some(self.newest_video.map_or(*pts, |v| v.max(*pts))),
            Media::Audio { track, .. } => {
                if let Some(s) = self.sound.get_mut(*track) {
                    *s = Some(s.map_or(t, |s| s.max(t)));
                }
            }
            Media::Stream(_) => {}
        }
    }

    /// End at the newest picture, once the sound has reached it.
    fn end(&mut self, done: Sender<Result<()>>) {
        let end = self.newest_video.unwrap_or(f64::NEG_INFINITY);
        self.ending = Some(Ending { end, until: Instant::now() + Asked::WAIT, done });
    }

    /// While ending: when to stop waiting for its sound.
    fn until(&self) -> Option<Instant> {
        self.ending.as_ref().map(|e| e.until)
    }

    /// Ending, and done: every track's sound has reached its end, or it's
    /// waited as long as it does.
    fn ended(&self, now: Instant) -> bool {
        let Some(e) = &self.ending else { return false };
        now >= e.until || self.writer.is_none() || self.sound.iter().all(|s| s.is_some_and(|s| s + PACKET_SECS > e.end))
    }

    /// Finish the file: what went wrong first, if anything did.
    fn finish(self) -> Result<()> {
        let Some(w) = self.writer else {
            return Err(self.error.unwrap_or_else(|| anyhow!("nothing was recorded")));
        };
        w.finish()?;
        self.error.map_or(Ok(()), Err)
    }

    /// Finish an ending recording, and say how it went.
    fn finish_ending(mut self) {
        let done = self.ending.take().map(|e| e.done);
        let result = self.finish();
        if let Some(done) = done {
            let _ = done.send(result);
        }
    }
}

// ---------------------------------------------------------------------------
// Replay buffer
// ---------------------------------------------------------------------------

/// Media kept in memory, oldest first.
struct Ring {
    items: VecDeque<(f64, Media)>,
    /// How much to keep: the replay window plus room to start on a keyframe.
    keep: f64,
    /// Everything from this time on is kept too, whatever `keep` says: what
    /// clips waiting for their sound need (see [`Asked::hold`]). A replay
    /// window shortened while one waited trimmed its start away.
    hold: Option<f64>,
}

impl Ring {
    fn new(keep: f64) -> Self {
        Self { items: VecDeque::new(), keep, hold: None }
    }

    fn push(&mut self, media: Media) {
        let t = media_time(&media);
        let key = matches!(media, Media::Video { key: true, .. });
        self.items.push_back((t, media));
        if !key {
            return;
        }
        // Drop everything older than the newest keyframe that still has `keep`
        // after it, so the ring always reaches back to a keyframe. By time, not
        // position: items are in arrival order, which isn't time order — audio
        // runs late, and when video stalls, audio newer than the next keyframe
        // arrives before it. (Trimming by position then could never get past
        // such a packet, and looped forever.) Once per keyframe keeps it cheap.
        let keep = self.hold.map_or(self.keep, |hold| self.keep.max(t - hold));
        let cut = self
            .items
            .iter()
            .filter(|(k, m)| matches!(m, Media::Video { key: true, .. }) && t - *k >= keep)
            .map(|(k, _)| *k)
            .fold(None, |a: Option<f64>, k| Some(a.map_or(k, |a| a.max(k))));
        if let Some(cut) = cut {
            self.items.retain(|(k, _)| *k >= cut - 1e-9);
        }
    }

    /// The newest picture's time.
    fn newest_video(&self) -> Option<f64> {
        self.items.iter().rev().find_map(|(t, m)| matches!(m, Media::Video { .. }).then_some(*t))
    }

    /// The newest keyframe, then everything from its time on (in the order
    /// it came): where a recording started now begins.
    fn since_newest_key(&self) -> Vec<&Media> {
        let Some(i) = self.items.iter().rposition(|(_, m)| matches!(m, Media::Video { key: true, .. })) else { return Vec::new() };
        let start = self.items[i].0;
        let rest = self.items.iter().enumerate().filter(|(j, (t, _))| *j != i && *t >= start - 1e-9).map(|(_, (_, m))| m);
        std::iter::once(&self.items[i].1).chain(rest).collect()
    }

    /// The newest moment every track has reached, up to `until`. The sound
    /// runs a little behind the picture, so a clip cut at the newest picture
    /// would end with a fraction of a second of silence.
    fn reached(&self, audio_tracks: usize, until: f64) -> Option<f64> {
        let newest_video = self.newest_video()?;
        let newest_audio = (0..audio_tracks)
            .map(|k| {
                self.items.iter().rev().find_map(|(t, m)| match m {
                    Media::Audio { track, .. } if *track == k => Some(*t),
                    _ => None,
                })
            })
            .collect::<Option<Vec<f64>>>()
            .and_then(|v| v.into_iter().reduce(f64::min));
        match (audio_tracks, newest_audio) {
            (0, _) => Some(newest_video.min(until)),
            (_, Some(a)) => Some(newest_video.min(a).min(until)),
            (_, None) => None,
        }
    }

    /// Everything from the keyframe nearest `end - seconds` up to `end`,
    /// where `end` is the newest moment every track has reached. Shares the
    /// frames.
    #[cfg(all(test, any(target_os = "windows", target_os = "linux")))]
    fn snapshot(&self, seconds: f64, audio_tracks: usize) -> Vec<(f64, Media)> {
        self.reached(audio_tracks, f64::INFINITY).map_or_else(Vec::new, |end| self.snapshot_to(seconds, end))
    }

    /// Everything from the keyframe nearest `newest - seconds` up to
    /// `newest`. Shares the frames.
    fn snapshot_to(&self, seconds: f64, newest: f64) -> Vec<(f64, Media)> {
        // The keyframe nearest `want`, before or after: the clip is then within
        // half a keyframe interval of `seconds` long. (The last one before it
        // made clips up to a whole interval longer: 30.6 s for 30, shown as 0:31.)
        let want = newest - seconds;
        let start = self
            .items
            .iter()
            .filter(|(t, m)| matches!(m, Media::Video { key: true, .. }) && *t <= newest)
            .map(|(t, _)| *t)
            .min_by(|a, b| (a - want).abs().total_cmp(&(b - want).abs()));
        let Some(start) = start else { return Vec::new() };
        self.items
            .iter()
            .filter(|(t, _)| *t >= start - 1e-6 && *t <= newest + 1e-6)
            .map(|(t, m)| (*t, m.share()))
            .collect()
    }
}

/// One clip asked for.
struct Ask {
    out: PathBuf,
    seconds: f64,
    done: Sender<Result<PathBuf>>,
    /// The newest picture when it was asked for: where it ends.
    end: f64,
    /// When to stop waiting for its sound.
    until: Instant,
}

/// Clips asked for, each waiting for its sound to reach its end. A clip was
/// cut at the newest moment every track had reached when it was asked for,
/// and the sound runs behind the picture: the mix 0.3 s behind real time
/// (so every source's sound is in), and the AAC encoder holds two packets
/// (43 ms) back. So every clip ended ~0.4 s before the moment it was asked
/// at. Now it ends at the newest picture then, once every track's sound has
/// reached it: a wait of about that, while the ring keeps filling. If the
/// sound doesn't come within [`Self::WAIT`], it's cut as it was.
#[derive(Default)]
struct Asked(Vec<Ask>);

impl Asked {
    /// The longest a clip waits for its sound.
    const WAIT: Duration = Duration::from_secs(1);

    /// When the next clip stops waiting, if one is.
    fn until(&self) -> Option<Instant> {
        self.0.iter().map(|a| a.until).min()
    }

    /// The time from which the ring keeps everything for the clips waiting
    /// ([`Ring::hold`]): the earliest one's start, with the room to start on
    /// a keyframe that the window's own keep has.
    fn hold(&self) -> Option<f64> {
        self.0.iter().map(|a| a.end - a.seconds - 2.0).reduce(f64::min)
    }

    /// The clips that are ready at `now`, each with what's in it.
    fn ready(&mut self, ring: &Ring, audio_tracks: usize, now: Instant) -> Vec<(Ask, Vec<(f64, Media)>)> {
        if self.0.is_empty() {
            return Vec::new();
        }
        let reached = ring.reached(audio_tracks, f64::INFINITY);
        let (ready, waiting) = std::mem::take(&mut self.0).into_iter().partition(|a| reached.is_some_and(|r| r >= a.end) || now >= a.until);
        self.0 = waiting;
        ready
            .into_iter()
            .map(|a: Ask| {
                let items = ring.reached(audio_tracks, a.end).map_or_else(Vec::new, |end| ring.snapshot_to(a.seconds, end));
                (a, items)
            })
            .collect()
    }
}

fn write_clip(out: &Path, items: &[(f64, Media)], layout: &Layout, fps: u32) -> Result<PathBuf> {
    let Some((start, _)) = items.iter().find(|(_, m)| matches!(m, Media::Video { key: true, .. })) else {
        bail!("no buffered footage yet — give it a second");
    };
    let mut w = FileWriter::create(out, layout, *start, fps, false)?;
    for (_, m) in items {
        w.append(m)?;
    }
    w.finish()?;
    Ok(out.to_path_buf())
}

fn media_time(m: &Media) -> f64 {
    match m {
        Media::Video { pts, .. } => *pts,
        Media::Audio { packet, .. } => packet.frame as f64 / RATE as f64,
        Media::Stream(f) => f.pts,
    }
}

impl Media {
    /// Another handle to the same media (frames are shared, AAC bytes copied).
    fn share(&self) -> Media {
        match self {
            Media::Video { frame, pts, key } => Media::Video { frame: frame.clone(), pts: *pts, key: *key },
            Media::Audio { track, packet } => {
                Media::Audio { track: *track, packet: AacPacket { data: packet.data.clone(), frame: packet.frame } }
            }
            Media::Stream(f) => Media::Stream(f.clone()),
        }
    }
}

#[cfg(all(test, any(target_os = "windows", target_os = "linux")))]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn video(pts: f64, key: bool) -> Media {
        let frame = crate::mp4file::VideoFrame(Arc::new(crate::mp4file::EncodedFrame { data: Arc::from(vec![0u8; 4]), config: None }));
        Media::Video { frame, pts, key }
    }

    fn audio(t: f64) -> Media {
        Media::Audio { track: 0, packet: AacPacket { data: vec![0; 4], frame: (t * RATE as f64) as i64 } }
    }

    /// Video stalls for a moment while audio keeps coming: an audio packet newer
    /// than the next keyframe arrives before that keyframe. Trimming must still
    /// finish (it used to loop forever, freezing saves and stopping).
    #[test]
    fn trimming_survives_audio_ahead_of_video() {
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done2 = done.clone();
        let t = std::thread::spawn(move || {
            let mut ring = Ring::new(2.0);
            ring.push(video(0.0, true));
            ring.push(audio(3.5)); // ahead of the stalled video
            for k in 1..=10 {
                ring.push(video(k as f64, true));
            }
            done2.store(true, std::sync::atomic::Ordering::SeqCst);
            ring
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !done.load(std::sync::atomic::Ordering::SeqCst) {
            assert!(std::time::Instant::now() < deadline, "trimming never finished");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let ring = t.join().unwrap();
        // Only the last few seconds are kept, starting at a keyframe.
        assert!(ring.items.len() <= 4, "kept {} items", ring.items.len());
        assert!(ring.items.iter().all(|(t, _)| *t >= 8.0 - 1e-9));
    }

    /// A clip starts at the keyframe nearest the length asked for, so it's
    /// about that long rather than up to a keyframe interval longer.
    #[test]
    fn clip_starts_at_the_nearest_keyframe() {
        let mut ring = Ring::new(32.0);
        // 60 fps, a keyframe every second; ends at 40.65 s.
        for k in 0..=2439 {
            let t = k as f64 / 60.0;
            ring.push(video(t, k % 60 == 0));
            ring.push(audio(t));
        }
        let start = |s: &[(f64, Media)]| s.first().unwrap().0;
        // 30 s back from 40.65 is 10.65: 11 is nearer than 10.
        assert_eq!(start(&ring.snapshot(30.0, 1)), 11.0);
        // From 40.65, 10.0 s back is 30.65: 31 is nearer than 30.
        assert_eq!(start(&ring.snapshot(10.0, 1)), 31.0);
        // 10.2 s back is 30.45: 30 is nearer.
        assert_eq!(start(&ring.snapshot(10.2, 1)), 30.0);
    }

    /// Pictures at 60 fps (a keyframe every second) up to `until`, and two
    /// tracks' sound in AAC packets 0.4 s behind them, in the order they'd
    /// come; `pushed` counts what's been pushed so far.
    fn play(ring: &mut Ring, pushed: &mut (usize, usize), until: f64, sound: bool) {
        let packet = 1024.0 / RATE as f64;
        while pushed.0 as f64 / 60.0 <= until + 1e-9 {
            let t = pushed.0 as f64 / 60.0;
            ring.push(video(t, pushed.0.is_multiple_of(60)));
            pushed.0 += 1;
            while sound && pushed.1 as f64 * packet <= t - 0.4 {
                for track in 0..2 {
                    ring.push(Media::Audio { track, packet: AacPacket { data: vec![0; 4], frame: pushed.1 as i64 * 1024 } });
                }
                pushed.1 += 1;
            }
        }
    }

    fn ends(items: &[(f64, Media)]) -> (f64, [f64; 2]) {
        let mut video = f64::MIN;
        let mut sound = [f64::MIN; 2];
        for (t, m) in items {
            match m {
                Media::Video { .. } => video = video.max(*t),
                Media::Audio { track, .. } => sound[*track] = sound[*track].max(*t),
                Media::Stream(_) => {}
            }
        }
        (video, sound)
    }

    /// The sound runs ~0.4 s behind the picture. A clip asked for waits for
    /// it, and ends at the picture that was newest when it was asked for,
    /// its sound up to there: not 0.4 s before, and nothing after.
    #[test]
    fn clip_waits_for_its_sound() {
        let mut ring = Ring::new(12.0);
        let mut pushed = (0, 0);
        play(&mut ring, &mut pushed, 20.0, true);
        let end = ring.newest_video().unwrap();
        assert!(ring.reached(2, f64::INFINITY).unwrap() < end - 0.35, "cut when asked, it would end 0.4 s short");
        let now = Instant::now();
        let mut asked = Asked::default();
        asked.0.push(Ask { out: PathBuf::new(), seconds: 10.0, done: mpsc::channel().0, end, until: now + Asked::WAIT });
        assert!(asked.ready(&ring, 2, now).is_empty(), "its sound isn't in yet");
        play(&mut ring, &mut pushed, 20.3, true);
        assert!(asked.ready(&ring, 2, now).is_empty(), "its sound isn't in yet");
        play(&mut ring, &mut pushed, 20.45, true);
        let ready = asked.ready(&ring, 2, now);
        assert_eq!(ready.len(), 1);
        let items = &ready[0].1;
        assert_eq!(items[0].0, 10.0, "from the keyframe 10 s back");
        let (video, sound) = ends(items);
        assert_eq!(video, end, "to the picture that was newest");
        let packet = 1024.0 / RATE as f64;
        for s in sound {
            assert!(s <= end && s + packet > end, "sound to {s}, for a clip to {end}");
        }
        assert!(asked.0.is_empty());
    }

    /// When the sound doesn't come (the mix held up), a clip waits a second
    /// at the most, then ends where the sound got to, as it did.
    #[test]
    fn clip_without_its_sound_is_cut_where_it_got_to() {
        let mut ring = Ring::new(12.0);
        let mut pushed = (0, 0);
        play(&mut ring, &mut pushed, 20.0, true);
        let (end, got_to) = (ring.newest_video().unwrap(), ring.reached(2, f64::INFINITY).unwrap());
        let now = Instant::now();
        let mut asked = Asked::default();
        asked.0.push(Ask { out: PathBuf::new(), seconds: 10.0, done: mpsc::channel().0, end, until: now + Asked::WAIT });
        play(&mut ring, &mut pushed, 21.0, false);
        assert!(asked.ready(&ring, 2, now + Duration::from_millis(999)).is_empty());
        let ready = asked.ready(&ring, 2, now + Asked::WAIT);
        assert_eq!(ready.len(), 1);
        let (video, sound) = ends(&ready[0].1);
        assert!(video <= got_to && video > got_to - 1.0 / 60.0, "pictures to {video}, sound got to {got_to}");
        assert_eq!(sound, [got_to; 2]);
    }

    /// A writer like a capture's: 60 fps, one track of sound.
    fn capture_writer() -> Writer {
        let spec = crate::mp4mux::Spec { width: 1920, height: 1080, fps: 60, audio_titles: vec!["Mix".into()], audio_bitrate: 192_000, comment: String::new(), mov: false, reorder: 0 };
        Writer::new(Layout { spec }, 60)
    }

    /// Pictures at 60 fps (a keyframe every second, with its set-up) from
    /// frame `from` to `to`, and the sound 0.4 s behind them, into `w`.
    fn feed(w: &Writer, frames: std::ops::RangeInclusive<i64>) {
        // x264's 1080p SPS, and a PPS.
        let sps = vec![0x67, 0x64, 0x00, 0x2a, 0xac, 0xd9, 0x40, 0x78, 0x02, 0x27, 0xe5, 0xc0, 0x44, 0x00, 0x00, 0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x01, 0xe0, 0x3c, 0x60, 0xc6, 0x58];
        let config = crate::mp4mux::AvcConfig { sps, pps: vec![0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0] };
        for n in frames {
            let key = n % 60 == 0;
            let frame = crate::mp4file::VideoFrame(Arc::new(crate::mp4file::EncodedFrame { data: Arc::from(vec![0u8, 0, 0, 1, 0x65]), config: key.then(|| config.clone()) }));
            let t = n as f64 / 60.0;
            w.tx.send(Command::Media(Media::Video { frame, pts: t, key })).unwrap();
            // The sound's packets up to 0.4 s ago.
            let (from, to) = ((((n - 1) as f64 / 60.0 - 0.4) / PACKET_SECS).floor() as i64, ((t - 0.4) / PACKET_SECS).floor() as i64);
            for p in (from + 1).max(0)..=to {
                w.tx.send(Command::Media(Media::Audio { track: 0, packet: AacPacket { data: vec![0x21, 0, 0, 0], frame: p * 1024 } })).unwrap();
            }
        }
    }

    /// Recording started while clipping begins at once, at the newest
    /// keyframe clipping has; ended while clipping goes on, it ends at the
    /// newest picture then, once its sound has caught up, and clipping goes
    /// on.
    #[test]
    fn recording_while_clipping() {
        let dir = std::env::temp_dir().join(format!("hc-writer-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("recording.mp4");
        let w = capture_writer();
        w.tx.send(Command::Replay(Some(30.0))).unwrap();
        feed(&w, 0..=210);
        w.tx.send(Command::Record(file.clone())).unwrap();
        feed(&w, 211..=330);
        let (done_tx, done_rx) = mpsc::channel();
        w.tx.send(Command::EndRecord(done_tx)).unwrap();
        // Not done until its sound reaches 5.5 s (0.4 s behind the pictures).
        assert!(done_rx.recv_timeout(Duration::from_millis(100)).is_err(), "ended before its sound caught up");
        feed(&w, 331..=370);
        done_rx.recv_timeout(Duration::from_secs(5)).expect("it ended").expect("written");
        let index = crate::mp4read::Index::read(&file).unwrap();
        // From the keyframe at 3 s (180) to the newest picture (330).
        assert_eq!(index.samples.len(), 151);
        assert!(index.samples[0].key && index.samples[0].pts == 0);
        assert_eq!(index.keyframes().len(), 3, "at 3, 4 and 5 s");
        // Clipping went on throughout.
        let clip = dir.join("clip.mp4");
        let (clip_tx, clip_rx) = mpsc::channel();
        w.tx.send(Command::SaveClip { out: clip.clone(), seconds: 2.0, done: clip_tx }).unwrap();
        feed(&w, 371..=420);
        assert_eq!(clip_rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap(), clip);
        // Stopped: what's kept is let go, and a clip can't be saved.
        w.tx.send(Command::Replay(None)).unwrap();
        let (none_tx, none_rx) = mpsc::channel();
        w.tx.send(Command::SaveClip { out: dir.join("none.mp4"), seconds: 2.0, done: none_tx }).unwrap();
        assert!(none_rx.recv_timeout(Duration::from_secs(5)).unwrap().is_err());
        w.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A recording that's the last thing running is finished when the
    /// writer is (the capture stopping): what that gave is the writer's.
    #[test]
    fn a_recording_finishes_with_the_writer() {
        let dir = std::env::temp_dir().join(format!("hc-writer-test2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("recording.mp4");
        let w = Writer::record(file.clone(), capture_writer_layout(), 60);
        feed(&w, 0..=119);
        w.join().unwrap();
        assert_eq!(crate::mp4read::Index::read(&file).unwrap().samples.len(), 120);
        // Nothing came: it says so.
        let empty = Writer::record(dir.join("empty.mp4"), capture_writer_layout(), 60);
        assert!(empty.join().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn capture_writer_layout() -> Layout {
        let spec = crate::mp4mux::Spec { width: 1920, height: 1080, fps: 60, audio_titles: vec!["Mix".into()], audio_bitrate: 192_000, comment: String::new(), mov: false, reorder: 0 };
        Layout { spec }
    }

    /// The replay window shortened (30 s to 5) while a 30 s clip waits for
    /// its sound: the ring still has all of the clip when it's taken, and
    /// keeps only the new window after.
    #[test]
    fn a_waiting_clip_keeps_its_start_when_the_window_shrinks() {
        let mut ring = Ring::new(32.0);
        let mut pushed = (0, 0);
        play(&mut ring, &mut pushed, 40.0, true);
        let end = ring.newest_video().unwrap();
        let now = Instant::now();
        let mut asked = Asked::default();
        asked.0.push(Ask { out: PathBuf::new(), seconds: 30.0, done: mpsc::channel().0, end, until: now + Asked::WAIT });
        ring.hold = asked.hold();
        ring.keep = 5.0 + 2.0;
        // Its sound comes in while pictures (and a keyframe, at 41 s) do.
        play(&mut ring, &mut pushed, 41.2, true);
        let ready = asked.ready(&ring, 2, now);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].1[0].0, 10.0, "from the keyframe 30 s back");
        assert_eq!(ends(&ready[0].1).0, end);
        ring.hold = asked.hold();
        play(&mut ring, &mut pushed, 42.2, true);
        assert!(ring.items.front().unwrap().0 >= 42.0 - 7.0 - 1.0, "kept from {}", ring.items.front().unwrap().0);
    }
}
