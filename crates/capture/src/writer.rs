//! Writing captured media to files, shared by the capture backends.
//!
//! The recorder sends [`Media`] (encoded H.264 frames, AAC packets per audio
//! track) to one writer thread with exact timestamps:
//!
//! - **Record**: everything goes straight into the platform's file writer, which
//!   writes a fragmented MP4 — if the app dies mid-recording, everything up to
//!   the last fragment (≤ 1 s) still plays.
//! - **Replay buffer**: encoded media stays in a ring in memory, a little longer
//!   than the replay window. "Save clip" takes everything from the last keyframe
//!   at or before (now − replay window) and writes it to a file on another
//!   thread, so capture never pauses. Nothing touches the disk until you save.
//!
//! The file itself is written by `FileWriter`: AVAssetWriter on macOS
//! (`avwriter`), our own MP4 muxer on Windows and Linux (`mp4file`).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result, anyhow, bail};

#[cfg(target_os = "macos")]
pub(crate) use crate::avwriter::{FileWriter, Layout, VideoFrame};
use crate::mixer::RATE;
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(crate) use crate::mp4file::{FileWriter, Layout, VideoFrame};

/// One encoded AAC packet and the frame (48 kHz sample index) it starts at.
pub struct AacPacket {
    pub data: Vec<u8>,
    pub frame: i64,
}

/// Something to write: a video frame, or one AAC packet for audio track `track`.
pub(crate) enum Media {
    Video { frame: VideoFrame, pts: f64, key: bool },
    Audio { track: usize, packet: AacPacket },
}

pub(crate) enum Command {
    Media(Media),
    /// Replay buffer: write the last `seconds` to `out`, report the result.
    SaveClip { out: PathBuf, seconds: f64, done: Sender<Result<PathBuf>> },
}

/// Ask the replay writer behind `tx` for a clip of the last `seconds` in `dir`.
/// The writer takes its snapshot as soon as it gets this, so the clip is of
/// now; the returned clip finishes once the file is written.
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
    /// Record straight to `file` (hidden partial path; the caller renames it).
    pub(crate) fn record(file: PathBuf, layout: Layout, fps: u32) -> Self {
        Self::spawn(move |rx| record_loop(rx, &file, &layout, fps))
    }

    /// Keep the last `window` seconds (plus slack) in memory until asked to save.
    pub(crate) fn replay(window: f64, layout: Layout, fps: u32) -> Self {
        Self::spawn(move |rx| replay_loop(rx, window, layout, fps))
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

// ---------------------------------------------------------------------------
// Record
// ---------------------------------------------------------------------------

fn record_loop(rx: Receiver<Command>, file: &Path, layout: &Layout, fps: u32) -> Result<()> {
    let mut file_writer: Option<FileWriter> = None;
    let mut first_error: Option<anyhow::Error> = None;
    while let Ok(cmd) = rx.recv() {
        let Command::Media(media) = cmd else { continue };
        if first_error.is_some() {
            continue; // keep draining so capture never blocks on us
        }
        // Start the file at the first keyframe, so it opens on a clean picture.
        if file_writer.is_none() {
            let Media::Video { pts, key: true, .. } = &media else { continue };
            match FileWriter::create(file, layout, *pts, fps, true) {
                Ok(w) => file_writer = Some(w),
                Err(e) => {
                    first_error = Some(e);
                    continue;
                }
            }
        }
        if let Err(e) = file_writer.as_mut().unwrap().append(&media) {
            first_error = Some(e);
        }
    }
    let Some(w) = file_writer else {
        return Err(first_error.unwrap_or_else(|| anyhow!("nothing was recorded")));
    };
    w.finish()?;
    first_error.map_or(Ok(()), Err)
}

// ---------------------------------------------------------------------------
// Replay buffer
// ---------------------------------------------------------------------------

/// Media kept in memory, oldest first.
struct Ring {
    items: VecDeque<(f64, Media)>,
    /// How much to keep: the replay window plus room to start on a keyframe.
    keep: f64,
}

impl Ring {
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
        let cut = self
            .items
            .iter()
            .filter(|(k, m)| matches!(m, Media::Video { key: true, .. }) && t - *k >= self.keep)
            .map(|(k, _)| *k)
            .fold(None, |a: Option<f64>, k| Some(a.map_or(k, |a| a.max(k))));
        if let Some(cut) = cut {
            self.items.retain(|(k, _)| *k >= cut - 1e-9);
        }
    }

    /// Everything from the keyframe nearest `end - seconds` up to `end`,
    /// where `end` is the newest moment every track has reached (audio runs a
    /// little behind video, so cutting at the newest frame would leave the clip's
    /// last fraction of a second silent). Shares the frames.
    fn snapshot(&self, seconds: f64, audio_tracks: usize) -> Vec<(f64, Media)> {
        let newest_video = self.items.iter().rev().find_map(|(t, m)| matches!(m, Media::Video { .. }).then_some(*t));
        let Some(newest_video) = newest_video else { return Vec::new() };
        let newest_audio = (0..audio_tracks)
            .map(|k| {
                self.items.iter().rev().find_map(|(t, m)| match m {
                    Media::Audio { track, .. } if *track == k => Some(*t),
                    _ => None,
                })
            })
            .collect::<Option<Vec<f64>>>()
            .and_then(|v| v.into_iter().reduce(f64::min));
        let newest = match (audio_tracks, newest_audio) {
            (0, _) => newest_video,
            (_, Some(a)) => newest_video.min(a),
            (_, None) => return Vec::new(),
        };
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

fn replay_loop(rx: Receiver<Command>, window: f64, layout: Layout, fps: u32) -> Result<()> {
    let mut ring = Ring { items: VecDeque::new(), keep: window + 2.0 };
    let mut saves: Vec<JoinHandle<()>> = Vec::new();
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Command::Media(m) => ring.push(m),
            Command::SaveClip { out, seconds, done } => {
                let items = ring.snapshot(seconds, layout.audio_tracks());
                let layout = layout.clone();
                // Writing happens off this thread so the ring keeps filling.
                saves.push(thread::spawn(move || {
                    let _ = done.send(write_clip(&out, &items, &layout, fps));
                }));
                saves.retain(|h| !h.is_finished());
            }
        }
    }
    // Let pending saves finish before the app exits.
    for h in saves {
        let _ = h.join();
    }
    Ok(())
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
            let mut ring = Ring { items: VecDeque::new(), keep: 2.0 };
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
        let mut ring = Ring { items: VecDeque::new(), keep: 32.0 };
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
}
