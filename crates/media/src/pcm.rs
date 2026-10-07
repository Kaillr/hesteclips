//! A clip's sound, decoded into memory while it's already being played.
//!
//! Decoding a whole track takes the time to read the whole file (sound and
//! picture are interleaved in it): ~0.6 s for 5 minutes from the disk cache,
//! seconds from a hard drive. Waiting for it held up opening the player. Here
//! every track fills in from the start, and what's decoded is readable at once:
//! playback starts with the first chunk.
//!
//! No locks, so the audio callback never waits on the decoder: samples are
//! stored as `AtomicU32` (an f32's bits; a relaxed load is a plain load), and
//! each track publishes how far it's filled after each chunk.

use std::io::Read;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use anyhow::{Context, Result};

use crate::PREVIEW_RATE;

/// Sound being decoded right now, by any clip.
static DECODING: AtomicUsize = AtomicUsize::new(0);

/// Some clip's sound is being decoded. Work that reads whole clips in the
/// background (scrub previews) waits for it: sharing a hard drive, they made
/// each other several times slower (12 s for a 2-minute clip's sound); one
/// after the other, the second reads the file from memory.
pub fn busy() -> bool {
    DECODING.load(Ordering::Relaxed) > 0
}

/// One track: interleaved stereo at [`PREVIEW_RATE`].
pub struct PcmTrack {
    samples: Box<[AtomicU32]>,
    /// How many samples are decoded (always whole stereo frames).
    len: AtomicUsize,
}

impl PcmTrack {
    fn with_capacity(n: usize) -> Self {
        Self { samples: (0..n).map(|_| AtomicU32::new(0)).collect(), len: AtomicUsize::new(0) }
    }

    /// Samples decoded so far (2 per stereo frame).
    pub fn len(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Stereo frame `i`, if it's decoded.
    #[inline]
    pub fn frame(&self, i: usize) -> Option<(f32, f32)> {
        if 2 * i + 1 >= self.len() {
            return None;
        }
        let l = f32::from_bits(self.samples[2 * i].load(Ordering::Relaxed));
        let r = f32::from_bits(self.samples[2 * i + 1].load(Ordering::Relaxed));
        Some((l, r))
    }

    /// Samples `from..to` (clamped to what's decoded), as plain samples.
    pub fn copy(&self, from: usize, to: usize) -> Vec<f32> {
        let to = to.min(self.len());
        self.samples[from.min(to)..to].iter().map(|s| f32::from_bits(s.load(Ordering::Relaxed))).collect()
    }

    /// What's decoded, as plain samples (for the waveform).
    pub fn to_vec(&self) -> Vec<f32> {
        self.samples[..self.len()].iter().map(|s| f32::from_bits(s.load(Ordering::Relaxed))).collect()
    }
}

/// Every track of a clip, filling in.
pub struct Pcm {
    tracks: Vec<PcmTrack>,
    done: AtomicBool,
}

impl Pcm {
    /// Already decoded tracks (tests, or sound from elsewhere).
    pub fn from_tracks(tracks: Vec<Vec<f32>>) -> Arc<Self> {
        let tracks = tracks
            .into_iter()
            .map(|t| {
                let n = t.len() / 2 * 2;
                PcmTrack { samples: t[..n].iter().map(|s| AtomicU32::new(s.to_bits())).collect(), len: AtomicUsize::new(n) }
            })
            .collect();
        Arc::new(Self { tracks, done: AtomicBool::new(true) })
    }

    pub fn tracks(&self) -> &[PcmTrack] {
        &self.tracks
    }

    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    /// Every track is decoded to its end.
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }
}

/// Start decoding these audio streams (`indices`, as `0:a:<index>`) of a clip
/// `duration` seconds long. Returns at once; `finished` is called (on a
/// decoding thread) when every track is done, or one failed.
///
/// One ffmpeg per track, side by side: Windows reads the file once for all of
/// them (they go through it together), and each decodes on its own core —
/// faster to the end than one ffmpeg decoding them all (1.9 s vs 2.6 s for a
/// 2-minute clip on a hard drive), and the start is there as soon.
pub fn decode_streaming(source: &Path, indices: &[usize], duration: f64, finished: impl FnOnce(&Pcm, Result<()>) + Send + 'static) -> Arc<Pcm> {
    // Room for the whole clip and a little more (the duration is the
    // container's, rounded); anything past it is dropped.
    let frames = ((duration.max(0.0) + 2.0) * PREVIEW_RATE as f64) as usize;
    let pcm = Arc::new(Pcm { tracks: indices.iter().map(|_| PcmTrack::with_capacity(frames * 2)).collect(), done: AtomicBool::new(indices.is_empty()) });
    if indices.is_empty() {
        finished(&pcm, Ok(()));
        return pcm;
    }
    let (source, indices, out) = (source.to_path_buf(), indices.to_vec(), pcm.clone());
    DECODING.fetch_add(1, Ordering::Relaxed);
    std::thread::Builder::new()
        .name("decode audio".into())
        .spawn(move || {
            let jobs: Vec<_> = indices
                .iter()
                .enumerate()
                .map(|(k, &i)| {
                    let (source, out) = (source.clone(), out.clone());
                    std::thread::spawn(move || decode_track(&source, i, &out.tracks[k]))
                })
                .collect();
            let mut result = Ok(());
            for job in jobs {
                let r = job.join().unwrap_or_else(|_| Err(anyhow::anyhow!("decoding the sound crashed")));
                if result.is_ok() {
                    result = r;
                }
            }
            out.done.store(true, Ordering::Release);
            DECODING.fetch_sub(1, Ordering::Relaxed);
            finished(&out, result);
        })
        .expect("spawn audio decoder");
    pcm
}

/// Decode one track into `out` as it comes (the same ffmpeg command as
/// [`crate::decode_audio`]).
fn decode_track(source: &Path, index: usize, out: &PcmTrack) -> Result<()> {
    let mut child = crate::ffmpeg()
        .args(["-hide_banner", "-loglevel", "error", "-vn", "-i"])
        .arg(source)
        .args(["-map", &format!("0:a:{index}"), "-ac", "2", "-ar", &PREVIEW_RATE.to_string()])
        .args(["-f", "f32le", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to run ffmpeg")?;
    let mut stdout = child.stdout.take().context("no ffmpeg output")?;
    // ~20 ms of sound at a time; whole stereo frames are published.
    let mut buf = vec![0u8; 8 * 1024];
    let (mut have, mut written) = (0usize, 0usize);
    let capacity = out.samples.len();
    loop {
        let read = stdout.read(&mut buf[have..])?;
        if read == 0 {
            break;
        }
        have += read;
        let whole = have / 8 * 8;
        for chunk in buf[..whole].chunks_exact(4) {
            if written < capacity {
                out.samples[written].store(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]), Ordering::Relaxed);
                written += 1;
            }
        }
        out.len.store(written, Ordering::Release);
        buf.copy_within(whole..have, 0);
        have -= whole;
    }
    let status = child.wait()?;
    if !status.success() && written == 0 {
        anyhow::bail!("couldn't decode the sound");
    }
    Ok(())
}
