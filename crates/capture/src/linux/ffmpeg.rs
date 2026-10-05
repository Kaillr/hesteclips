//! Encoding with ffmpeg, which the app needs anyway (editing, thumbnails):
//! H.264 from NV12 frames, AAC from the mixer's PCM, each in its own ffmpeg
//! process fed through a pipe. ffmpeg gives us the GPU's encoder where there
//! is one — NVENC on NVIDIA, VA-API on AMD and Intel — and x264 or OpenH264
//! otherwise, whichever the installed ffmpeg has.
//!
//! What comes back is a raw stream (H.264 Annex B with an access unit
//! delimiter before each frame, AAC in ADTS frames), cut into packets here and
//! timed by us — frame N of the video is N/fps, as on the other platforms —
//! then muxed by our own MP4 writer like on Windows.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result, anyhow, bail};

use crate::EncodeSettings;
use crate::mixer::RATE;
use crate::mp4file::{EncodedFrame, VideoFrame};
use crate::mp4mux::{AvcConfig, annexb_to_avcc};
use crate::writer::{AacPacket, Command as WriterCommand, Media};

/// The ffmpeg to run: one shipped next to the app wins over the one on `PATH`
/// (as the editor does it). It dies with the thread that starts it — the
/// capture's, or the app if that crashes or is killed — rather than living on
/// and, for a webcam, keeping the camera busy.
pub(crate) fn ffmpeg() -> Command {
    use std::os::unix::process::CommandExt;
    let bundled = std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.join("ffmpeg"))).filter(|p| p.is_file());
    let mut cmd = Command::new(bundled.unwrap_or_else(|| PathBuf::from("ffmpeg")));
    cmd.args(["-hide_banner", "-nostdin", "-loglevel", "error"]);
    // SAFETY: prctl is async-signal-safe, and touches nothing of ours.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
    cmd
}

// ---------------------------------------------------------------------------
// Picking an H.264 encoder
// ---------------------------------------------------------------------------

/// An H.264 encoder ffmpeg has, and how to run it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Nvenc,
    Vaapi,
    X264,
    OpenH264,
}

impl Codec {
    fn ffmpeg_name(self) -> &'static str {
        match self {
            Codec::Nvenc => "h264_nvenc",
            Codec::Vaapi => "h264_vaapi",
            Codec::X264 => "libx264",
            Codec::OpenH264 => "libopenh264",
        }
    }

    /// For the UI: what does the encoding.
    pub fn label(self) -> &'static str {
        match self {
            Codec::Nvenc => "NVIDIA NVENC",
            Codec::Vaapi => "VA-API (GPU)",
            Codec::X264 => "x264 (CPU)",
            Codec::OpenH264 => "OpenH264 (CPU)",
        }
    }

    pub fn is_hardware(self) -> bool {
        matches!(self, Codec::Nvenc | Codec::Vaapi)
    }

    /// ffmpeg's arguments to encode `width`×`height` NV12 from stdin to
    /// H.264 Annex B on stdout.
    fn args(self, width: u32, height: u32, fps: u32, kbps: u32, gop: u32) -> Vec<String> {
        let mut a: Vec<String> = Vec::new();
        let mut push = |s: &[&str]| a.extend(s.iter().map(|s| s.to_string()));
        if self == Codec::Vaapi {
            let device = render_node().unwrap_or_else(|| "/dev/dri/renderD128".into());
            push(&["-init_hw_device", &format!("vaapi=va:{device}"), "-filter_hw_device", "va"]);
        }
        push(&["-f", "rawvideo", "-pix_fmt", "nv12", "-video_size", &format!("{width}x{height}"), "-framerate", &fps.to_string()]);
        push(&["-color_range", "tv", "-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709"]);
        push(&["-i", "pipe:0"]);
        if self == Codec::Vaapi {
            push(&["-vf", "format=nv12,hwupload"]);
        }
        push(&["-c:v", self.ffmpeg_name()]);
        let (rate, max, buf) = (format!("{kbps}k"), format!("{}k", kbps * 3 / 2), format!("{}k", kbps * 2));
        // No B-frames, so frames come out in the order they went in: the Nth
        // packet is the Nth frame. Encoders tuned for low delay, so a clip
        // saved right after a moment has that moment in it.
        match self {
            Codec::Nvenc => push(&[
                "-preset", "p4", "-tune", "ll", "-rc", "vbr", "-b:v", &rate, "-maxrate", &max, "-bufsize", &buf, "-profile:v", "high",
                "-zerolatency", "1",
            ]),
            Codec::Vaapi => push(&["-rc_mode", "VBR", "-b:v", &rate, "-maxrate", &max, "-bufsize", &buf, "-profile:v", "high"]),
            Codec::X264 => push(&[
                "-preset", "veryfast", "-tune", "zerolatency", "-b:v", &rate, "-maxrate", &max, "-bufsize", &buf, "-profile:v", "high",
            ]),
            Codec::OpenH264 => push(&["-rc_mode", "bitrate", "-allow_skip_frames", "0", "-b:v", &rate, "-maxrate", &max]),
        }
        push(&["-g", &gop.to_string(), "-bf", "0", "-fps_mode", "passthrough"]);
        push(&["-color_range", "tv", "-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709"]);
        push(&["-bsf:v", "h264_metadata=aud=insert", "-flush_packets", "1", "-f", "h264", "pipe:1"]);
        a
    }
}

/// The GPU's render node for VA-API.
fn render_node() -> Option<String> {
    let mut nodes: Vec<String> = std::fs::read_dir("/dev/dri")
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("renderD"))
        .collect();
    nodes.sort();
    nodes.first().map(|n| format!("/dev/dri/{n}"))
}

/// The best encoder that actually works here (tried with a few frames: an
/// ffmpeg can list NVENC without an NVIDIA card, or VA-API without a driver
/// that encodes H.264). Found once per run.
pub fn encoder(hardware: bool) -> Result<Codec> {
    static HW: OnceLock<Option<Codec>> = OnceLock::new();
    static SW: OnceLock<Option<Codec>> = OnceLock::new();
    let candidates: &[Codec] =
        if hardware { &[Codec::Nvenc, Codec::Vaapi, Codec::X264, Codec::OpenH264] } else { &[Codec::X264, Codec::OpenH264] };
    let found = (if hardware { &HW } else { &SW }).get_or_init(|| {
        let listed = ffmpeg().arg("-encoders").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
        candidates
            .iter()
            .copied()
            .filter(|c| listed.split_whitespace().any(|w| w == c.ffmpeg_name()))
            .find(|c| works(*c))
    });
    found.ok_or_else(|| {
        anyhow!(
            "ffmpeg can't encode H.264 here: install an ffmpeg with libx264 (on Fedora, RPM Fusion's `ffmpeg`), \
             or the GPU's VA-API or NVENC driver"
        )
    })
}

/// Encode a few frames with `codec`, to see that it really works.
fn works(codec: Codec) -> bool {
    let (w, h) = (320, 180);
    let mut child = match ffmpeg()
        .args(codec.args(w, h, 30, 1000, 30))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    let frame = vec![128u8; (w * h * 3 / 2) as usize];
    let mut stdin = child.stdin.take().unwrap();
    let writer = thread::spawn(move || {
        for _ in 0..5 {
            if stdin.write_all(&frame).is_err() {
                break;
            }
        }
    });
    let mut out = Vec::new();
    let _ = child.stdout.take().unwrap().read_to_end(&mut out);
    let _ = writer.join();
    let ok = child.wait().is_ok_and(|s| s.success());
    ok && split_access_units(&mut out, true).len() == 5
}

// ---------------------------------------------------------------------------
// H.264
// ---------------------------------------------------------------------------

/// Frames waiting for ffmpeg before the pacer starts dropping them.
const QUEUE: usize = 4;

/// A running H.264 encoder.
pub(crate) struct H264 {
    tx: Option<SyncSender<(Vec<u8>, i64)>>,
    /// Frame buffers to reuse.
    spare: Receiver<Vec<u8>>,
    spare_tx: Sender<Vec<u8>>,
    feeder: Option<JoinHandle<()>>,
    reader: Option<JoinHandle<()>>,
    child: Arc<Mutex<Child>>,
}

impl H264 {
    /// Start encoding `width`×`height` frames into `out`.
    pub(crate) fn start(width: u32, height: u32, s: &EncodeSettings, out: Sender<WriterCommand>) -> Result<Self> {
        let codec = encoder(s.use_hardware)?;
        eprintln!("video encoder: {} ({}x{} at {} fps)", codec.label(), width, height, s.fps);
        let gop = s.fps * s.keyframe_interval_secs.max(1);
        let mut child = ffmpeg()
            .args(codec.args(width, height, s.fps, s.video_bitrate_kbps.max(100), gop))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("couldn't run ffmpeg for the video encoder")?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let errors = spawn_stderr(stderr);
        let (tx, rx) = mpsc::sync_channel::<(Vec<u8>, i64)>(QUEUE);
        let (spare_tx, spare) = mpsc::channel();
        // The frame numbers sent, in order: the Nth packet back is the Nth sent.
        let sent = Arc::new(Mutex::new(VecDeque::<i64>::new()));
        let feeder = {
            let (sent, spare_tx) = (sent.clone(), spare_tx.clone());
            thread::Builder::new().name("h264-in".into()).spawn(move || feed(stdin, rx, sent, spare_tx))?
        };
        let child = Arc::new(Mutex::new(child));
        let reader = {
            let fps = s.fps;
            thread::Builder::new().name("h264-out".into()).spawn(move || {
                if let Err(e) = read_video(stdout, fps, &sent, &out) {
                    let detail = errors.join().unwrap_or_default();
                    eprintln!("video encoder ({}) stopped: {e:#}{}", codec.ffmpeg_name(), detail.map(|d| format!("\n{d}")).unwrap_or_default());
                }
            })?
        };
        Ok(Self { tx: Some(tx), spare, spare_tx, feeder: Some(feeder), reader: Some(reader), child })
    }

    /// An empty frame buffer to fill (reused when there is one).
    pub(crate) fn buffer(&self) -> Vec<u8> {
        self.spare.try_recv().unwrap_or_default()
    }

    /// Queue `nv12` as frame `n`. Returns false if the encoder is behind and
    /// the frame was dropped instead of stalling capture.
    pub(crate) fn encode(&self, nv12: Vec<u8>, n: i64) -> bool {
        let Some(tx) = &self.tx else { return false };
        match tx.try_send((nv12, n)) {
            Ok(()) => true,
            Err(TrySendError::Full((buf, _)) | TrySendError::Disconnected((buf, _))) => {
                let _ = self.spare_tx.send(buf);
                false
            }
        }
    }

    /// Encode what's queued and wait for the last packet.
    pub(crate) fn finish(mut self) {
        drop(self.tx.take());
        if let Some(t) = self.feeder.take() {
            let _ = t.join();
        }
        if let Some(t) = self.reader.take() {
            let _ = t.join();
        }
        let _ = self.child.lock().unwrap().wait();
    }
}

impl Drop for H264 {
    fn drop(&mut self) {
        if self.tx.is_some() {
            drop(self.tx.take());
            let _ = self.child.lock().unwrap().kill();
        }
    }
}

/// Write frames to ffmpeg until the sender goes away, then close its input
/// so it finishes.
fn feed(mut stdin: ChildStdin, rx: Receiver<(Vec<u8>, i64)>, sent: Arc<Mutex<VecDeque<i64>>>, spare: Sender<Vec<u8>>) {
    while let Ok((frame, n)) = rx.recv() {
        sent.lock().unwrap().push_back(n);
        let failed = stdin.write_all(&frame).is_err();
        let _ = spare.send(frame);
        if failed {
            break;
        }
    }
}

/// Read ffmpeg's H.264 output, one access unit per frame, and pass each to
/// the writer as frame `sent[k]`.
fn read_video(mut stdout: impl Read, fps: u32, sent: &Mutex<VecDeque<i64>>, out: &Sender<WriterCommand>) -> Result<()> {
    let mut pending = Vec::with_capacity(1 << 20);
    let mut chunk = vec![0u8; 1 << 18];
    let mut config: Option<AvcConfig> = None;
    let mut last = -1i64;
    loop {
        let n = stdout.read(&mut chunk).context("reading from ffmpeg")?;
        let eof = n == 0;
        pending.extend_from_slice(&chunk[..n]);
        for au in split_access_units(&mut pending, eof) {
            let frame = annexb_to_avcc(&au);
            if frame.data.is_empty() {
                continue;
            }
            if let Some(c) = frame.config {
                config = Some(c);
            }
            let n = sent.lock().unwrap().pop_front().unwrap_or(last + 1);
            last = n;
            let key = frame.idr;
            let media = Media::Video {
                frame: VideoFrame(Arc::new(EncodedFrame { data: Arc::from(frame.data), config: if key { config.clone() } else { None } })),
                pts: n as f64 / fps as f64,
                key,
            };
            if out.send(WriterCommand::Media(media)).is_err() {
                bail!("the file writer stopped");
            }
        }
        if eof {
            return Ok(());
        }
    }
}

/// Take the complete access units off the front of an Annex B stream (each
/// starts with an access unit delimiter, NAL type 9). At the end of the stream
/// the last one is complete too.
fn split_access_units(buf: &mut Vec<u8>, eof: bool) -> Vec<Vec<u8>> {
    // Where each delimiter's start code begins.
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 < buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 && buf[i + 3] & 0x1F == 9 {
            starts.push(if i > 0 && buf[i - 1] == 0 { i - 1 } else { i });
            i += 4;
        } else {
            i += 1;
        }
    }
    if eof && !buf.is_empty() {
        starts.push(buf.len());
    }
    let mut units = Vec::new();
    for pair in starts.windows(2) {
        if pair[1] > pair[0] {
            units.push(buf[pair[0]..pair[1]].to_vec());
        }
    }
    let consumed = if eof { buf.len() } else { starts.last().copied().unwrap_or(0) };
    buf.drain(..consumed);
    units
}

/// Collect what ffmpeg says on stderr (it's quiet unless something's wrong),
/// to explain a failure.
fn spawn_stderr(mut stderr: impl Read + Send + 'static) -> JoinHandle<Option<String>> {
    thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        // The last lines say what went wrong.
        let tail: Vec<&str> = text.lines().rev().take(6).collect();
        Some(tail.into_iter().rev().collect::<Vec<_>>().join("\n"))
    })
}

// ---------------------------------------------------------------------------
// AAC
// ---------------------------------------------------------------------------

/// Samples per AAC packet.
const AAC_FRAMES: i64 = 1024;

/// AAC-encode one track's PCM (interleaved stereo at 48 kHz) and pass the
/// packets to the writer as audio track `track`. Ends when `rx` closes.
pub(crate) fn spawn_aac(track: usize, bitrate: u32, rx: Receiver<Vec<f32>>, out: Sender<WriterCommand>) -> Result<JoinHandle<()>> {
    let mut child = ffmpeg()
        .args(["-f", "f32le", "-ar", &RATE.to_string(), "-ac", "2", "-i", "pipe:0"])
        .args(["-c:a", "aac", "-b:a", &bitrate.to_string(), "-flush_packets", "1", "-f", "adts", "pipe:1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("couldn't run ffmpeg for the audio encoder")?;
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let errors = spawn_stderr(child.stderr.take().unwrap());
    let feeder = thread::spawn(move || {
        let mut bytes = Vec::new();
        while let Ok(pcm) = rx.recv() {
            bytes.clear();
            bytes.extend(pcm.iter().flat_map(|s| s.to_le_bytes()));
            if stdin.write_all(&bytes).is_err() {
                break;
            }
        }
    });
    Ok(thread::Builder::new().name("aac".into()).spawn(move || {
        if let Err(e) = read_aac(stdout, track, &out) {
            let detail = errors.join().unwrap_or_default();
            eprintln!("audio encoder stopped: {e:#}{}", detail.map(|d| format!("\n{d}")).unwrap_or_default());
        }
        let _ = feeder.join();
        let _ = child.wait();
    })?)
}

/// Read ADTS frames and pass on their raw AAC. ffmpeg's encoder starts with
/// one packet of priming (its delay), so packets are numbered from -1024:
/// packet k holds the samples from 1024·(k−1) on, and the priming one falls
/// before the file's start.
fn read_aac(mut stdout: impl Read, track: usize, out: &Sender<WriterCommand>) -> Result<()> {
    let mut pending = Vec::new();
    let mut chunk = vec![0u8; 16 * 1024];
    let mut frame = -AAC_FRAMES;
    loop {
        let n = stdout.read(&mut chunk).context("reading from ffmpeg")?;
        if n == 0 {
            return Ok(());
        }
        pending.extend_from_slice(&chunk[..n]);
        let mut at = 0;
        loop {
            let rest = &pending[at..];
            if rest.len() >= 2 && (rest[0] != 0xFF || rest[1] & 0xF0 != 0xF0) {
                at += 1; // lost sync (not expected from a pipe): find the next frame
                continue;
            }
            let Some((header, len)) = adts_frame(rest) else { break };
            let packet = AacPacket { data: pending[at + header..at + len].to_vec(), frame };
            frame += AAC_FRAMES;
            at += len;
            if out.send(WriterCommand::Media(Media::Audio { track, packet })).is_err() {
                bail!("the file writer stopped");
            }
        }
        pending.drain(..at);
    }
}

/// The header length and total length of the ADTS frame at the start of `b`,
/// if it's all there.
fn adts_frame(b: &[u8]) -> Option<(usize, usize)> {
    if b.len() < 7 || b[0] != 0xFF || b[1] & 0xF0 != 0xF0 {
        return None;
    }
    let header = if b[1] & 0x01 == 1 { 7 } else { 9 };
    let len = ((b[3] as usize & 0x03) << 11) | ((b[4] as usize) << 3) | (b[5] as usize >> 5);
    (len >= header && b.len() >= len).then_some((header, len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_units_split_at_delimiters() {
        let au = |t: u8| vec![0, 0, 0, 1, 9, 0xF0, 0, 0, 1, t, 1, 2, 3];
        let mut stream = [au(0x65), au(0x41), au(0x41)].concat();
        let units = split_access_units(&mut stream, false);
        assert_eq!(units, vec![au(0x65), au(0x41)]);
        assert_eq!(stream, au(0x41));
        let units = split_access_units(&mut stream, true);
        assert_eq!(units, vec![au(0x41)]);
        assert!(stream.is_empty());
    }

    #[test]
    fn adts_frames_are_measured() {
        // A 7-byte header (no CRC) announcing 10 bytes in all.
        let len = 10usize;
        let mut f = vec![0xFF, 0xF1, 0x50, 0x80 | ((len >> 11) as u8 & 3), (len >> 3) as u8, ((len & 7) << 5) as u8 | 0x1F, 0xFC];
        f.extend([1, 2, 3]);
        assert_eq!(adts_frame(&f), Some((7, 10)));
        assert_eq!(adts_frame(&f[..9]), None);
    }
}
