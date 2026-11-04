//! Going live: what's captured, sent as it happens to each place it's
//! streamed to (Twitch, YouTube, Kick, a server of your own), all at once.
//!
//! The picture comes from an encoder of its own (the backend's: at the
//! stream's size, frame rate and bitrate, constant bitrate and a keyframe
//! every two seconds, as the services ask), so clips and recordings keep
//! theirs; the sound is the clip's mix track as it's encoded for the file
//! (AAC). Each destination has a thread and an ffmpeg of its own: the thread
//! writes FLV into the ffmpeg's input, and ffmpeg sends it on over RTMP (or
//! RTMPS) without encoding anything again. A destination that drops (its
//! connection lost, its server gone away) is connected again, from the next
//! keyframe, while the others go on; one whose connection can't keep up has
//! frames left out up to the next keyframe, never the others'.
//!
//! The stream key is part of the address ffmpeg is given: it's taken out of
//! whatever is said about a destination ([`Destination::secret`]), and the
//! address is never logged.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// The sound's AudioSpecificConfig: AAC-LC, 48 kHz, stereo (as the files
/// have it: `mp4mux`).
const AAC_CONFIG: [u8; 2] = [0x11, 0x90];
/// The sound's rate (`mixer::RATE`).
const AUDIO_RATE: f64 = 48_000.0;

/// Where a stream goes, and how each place gets it.
#[derive(Debug, Clone)]
pub struct StreamSettings {
    pub destinations: Vec<Destination>,
    /// How each destination is doing, kept up to date for the app.
    pub status: Arc<Status>,
}

/// What a destination is sent: a picture of its own size, frame rate and
/// bitrate (Twitch takes less than YouTube).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Quality {
    /// The picture's short side brought down to this (never up): 1080 is
    /// 1080p. None keeps the size the capture records at.
    pub short_side: Option<u32>,
    /// Frames a second, at most: the capture's own rate, or a whole fraction
    /// of it ([`stream_rate`]).
    pub fps: u32,
    pub kbps: u32,
}

/// Where a stream goes.
#[derive(Clone, PartialEq, Eq)]
pub struct Destination {
    /// What it's called when it's said how it's doing: "Twitch".
    pub name: String,
    /// The RTMP or RTMPS address, the stream key on it.
    pub url: String,
    /// The secret part of `url` (its stream key): never said or logged.
    pub secret: String,
    pub quality: Quality,
}

/// Its address, the key on it, never printed.
impl std::fmt::Debug for Destination {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("Destination").field("name", &self.name).finish_non_exhaustive()
    }
}

impl Destination {
    /// `text` (something ffmpeg said) with the stream key and the address
    /// it's in taken out.
    pub fn hide(&self, text: &str) -> String {
        let mut out = text.replace(&self.url, &self.name);
        if self.secret.len() >= 4 {
            out = out.replace(&self.secret, "••••");
        }
        out
    }
}

/// A server and a stream key as one address: `rtmp://host/app` and `key`
/// make `rtmp://host/app/key`. A server that already ends in its key (or
/// no key) is kept as it is.
pub fn join(server: &str, key: &str) -> String {
    let server = server.trim();
    let key = key.trim().trim_start_matches('/');
    if key.is_empty() || server.ends_with(key) {
        return server.to_owned();
    }
    format!("{}/{key}", server.trim_end_matches('/'))
}

/// The frame rate a stream is sent at, for a capture at `fps` and a stream
/// of at most `most`: the capture's own rate when it's no more, else the
/// largest whole fraction of it that is (each stream frame one of the
/// capture's: 144 sends 48, 120 sends 60). Never more than 60 (what the
/// services take).
pub fn stream_rate(fps: u32, most: u32) -> u32 {
    let most = most.clamp(1, 60);
    let fps = fps.max(1);
    (1..=fps).find(|n| fps.is_multiple_of(*n) && fps / n <= most).map_or(1, |n| fps / n)
}

/// The frame rates a stream can be sent at from a capture at `fps`, fastest
/// first: its whole fractions from 60 down to 20 (each stream frame one of
/// its frames, so motion stays even), at most four. A capture slower than
/// 20 has only its own.
pub fn stream_rates(fps: u32) -> Vec<u32> {
    let fps = fps.max(1);
    let rates: Vec<u32> = (1..=fps).filter(|n| fps.is_multiple_of(*n)).map(|n| fps / n).filter(|r| (20..=60).contains(r)).take(4).collect();
    if rates.is_empty() { vec![stream_rate(fps, 60)] } else { rates }
}

/// One picture a stream sends, made by an encoder of its own: as many as
/// there are different ones among the destinations (two of the same share).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rendition {
    pub short_side: Option<u32>,
    /// Frames a second, as sent (see [`stream_rate`]).
    pub fps: u32,
    pub kbps: u32,
}

/// The pictures a stream needs from a capture at `fps`, and which of them
/// each destination is sent (by its place in the list).
pub(crate) fn renditions(s: &StreamSettings, fps: u32) -> (Vec<Rendition>, Vec<usize>) {
    let mut list: Vec<Rendition> = Vec::new();
    let mut of = Vec::with_capacity(s.destinations.len());
    for d in &s.destinations {
        let r = Rendition { short_side: d.quality.short_side, fps: stream_rate(fps, d.quality.fps), kbps: d.quality.kbps.max(100) };
        let i = match list.iter().position(|x| *x == r) {
            Some(i) => i,
            None => {
                list.push(r);
                list.len() - 1
            }
        };
        of.push(i);
    }
    (list, of)
}

/// How a destination is doing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Link {
    /// Connecting: the first time (`attempt` 0), or again.
    Connecting { attempt: u32 },
    /// Sending, since then.
    Live { since: Instant },
    /// Lost, or couldn't connect: trying again then.
    Waiting { attempt: u32, at: Instant },
    /// Stopped.
    Ended,
}

/// One destination's state, for the app.
#[derive(Debug, Clone)]
pub struct DestinationStatus {
    pub name: String,
    pub link: Link,
    /// What went wrong last, as ffmpeg said it (the key taken out).
    pub problem: Option<String>,
    /// Frames left out because its connection couldn't keep up.
    pub dropped: u64,
}

/// How each destination of a stream is doing, shared between the stream
/// and the app (as `mixer::LiveAudio` is).
#[derive(Debug, Default)]
pub struct Status(Mutex<Vec<DestinationStatus>>);

impl Status {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Each destination, in the order they were given.
    pub fn get(&self) -> Vec<DestinationStatus> {
        self.0.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// The stream starts: these destinations, connecting.
    fn begin(&self, names: impl Iterator<Item = String>) {
        if let Ok(mut s) = self.0.lock() {
            *s = names.map(|name| DestinationStatus { name, link: Link::Connecting { attempt: 0 }, problem: None, dropped: 0 }).collect();
        }
    }

    fn set(&self, i: usize, f: impl FnOnce(&mut DestinationStatus)) {
        if let Ok(mut s) = self.0.lock()
            && let Some(d) = s.get_mut(i)
        {
            f(d);
        }
    }
}

/// A picture of the stream's own (from its encoder): H.264, NAL units with
/// 4-byte lengths.
#[derive(Clone)]
pub(crate) struct StreamFrame {
    /// Which of the stream's pictures it is ([`renditions`]).
    pub rendition: usize,
    pub data: Arc<[u8]>,
    /// The decoder's set-up for it (an `avcC` record): on every keyframe.
    pub avcc: Option<Arc<[u8]>>,
    pub pts: f64,
    pub key: bool,
}

/// What goes to a destination's thread.
#[derive(Clone)]
enum Packet {
    Video(StreamFrame),
    /// An AAC packet and its time.
    Audio(Arc<[u8]>, f64),
}

/// The ffmpeg shipped next to the app, else the one on `PATH`; on Windows,
/// without a console window (the app is a GUI program).
pub(crate) fn ffmpeg() -> Command {
    let bundled = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join("ffmpeg").with_extension(std::env::consts::EXE_EXTENSION)))
        .filter(|p| p.is_file());
    #[allow(unused_mut)]
    let mut cmd = Command::new(bundled.as_deref().unwrap_or(Path::new("ffmpeg")));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Whether ffmpeg here can send over RTMPS (it's built with TLS). Asked
/// once; call it off the UI thread first (it runs ffmpeg).
pub fn rtmps() -> bool {
    static CAN: OnceLock<bool> = OnceLock::new();
    *CAN.get_or_init(|| {
        ffmpeg()
            .args(["-hide_banner", "-protocols"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).lines().any(|l| l.trim() == "rtmps"))
            .unwrap_or(false)
    })
}

// ---------------------------------------------------------------------------
// The stream, in the writer: one feed per destination
// ---------------------------------------------------------------------------

/// A running stream: what the writer hands its pictures and sound to.
pub(crate) struct Sink {
    feeds: Vec<Feed>,
    /// The sound goes too: there's a mix track.
    audio: bool,
    status: Arc<Status>,
}

struct Feed {
    tx: Option<SyncSender<Packet>>,
    thread: Option<JoinHandle<()>>,
    /// Left out until the next keyframe: its queue was full (its connection
    /// can't keep up).
    skipping: bool,
    index: usize,
    /// Which of the stream's pictures it's sent.
    rendition: usize,
}

impl Sink {
    /// Start sending to each destination (each connects on the first
    /// keyframe of its picture), from a capture at `fps`. `audio`: whether
    /// there's a mix track to send.
    pub(crate) fn start(s: &StreamSettings, audio: bool, fps: u32) -> Self {
        s.status.begin(s.destinations.iter().map(|d| d.name.clone()));
        let (list, of) = renditions(s, fps);
        let feeds = s
            .destinations
            .iter()
            .zip(of)
            .enumerate()
            .map(|(index, (dest, rendition))| {
                let r = list[rendition];
                // A few seconds of pictures and sound wait for a connection
                // that's slow (or still connecting) before frames are left out.
                let queue = ((r.fps.max(1) + 50) * 4) as usize;
                let meta = Meta { width: 0, height: 0, fps: r.fps, kbps: r.kbps, audio };
                let (tx, rx) = mpsc::sync_channel(queue);
                let (dest, status) = (dest.clone(), s.status.clone());
                let thread = thread::Builder::new().name("stream".into()).spawn(move || send_loop(dest, rx, status, index, meta)).ok();
                Feed { tx: Some(tx), thread, skipping: false, index, rendition }
            })
            .collect();
        Self { feeds, audio, status: s.status.clone() }
    }

    /// One of the stream's pictures: to the destinations sent that one.
    pub(crate) fn video(&mut self, frame: &StreamFrame) {
        for f in self.feeds.iter_mut().filter(|f| f.rendition == frame.rendition) {
            if f.skipping && !frame.key {
                self.status.set(f.index, |d| d.dropped += 1);
                continue;
            }
            f.skipping = false;
            if !f.send(Packet::Video(frame.clone())) {
                f.skipping = true;
                self.status.set(f.index, |d| d.dropped += 1);
            }
        }
    }

    /// A packet of the mix track's sound, `frame` its first sample.
    pub(crate) fn audio(&mut self, data: &[u8], frame: i64) {
        if !self.audio {
            return;
        }
        let packet = Packet::Audio(Arc::from(data), frame as f64 / AUDIO_RATE);
        for f in &mut self.feeds {
            // Sound between pictures left out goes too, if it fits: it's
            // only the pictures that can't start anywhere.
            if !f.skipping && !f.send(packet.clone()) {
                f.skipping = true;
            }
        }
    }

    /// End the stream: each destination is told it's over (its ffmpeg
    /// ends the stream properly), waited for a little, then let go.
    pub(crate) fn stop(mut self) {
        for f in &mut self.feeds {
            f.tx = None;
        }
        for f in &mut self.feeds {
            if let Some(t) = f.thread.take() {
                let _ = t.join();
            }
        }
    }
}

impl Feed {
    /// Queue a packet for the destination's thread; false when its queue
    /// is full (or it's gone).
    fn send(&self, p: Packet) -> bool {
        match self.tx.as_ref().map(|tx| tx.try_send(p)) {
            Some(Ok(())) => true,
            Some(Err(TrySendError::Full(_))) => false,
            // Its thread ended (it can't: only if it panicked).
            Some(Err(TrySendError::Disconnected(_))) | None => true,
        }
    }
}

/// What a connection's FLV says about the stream, up front.
#[derive(Clone, Copy)]
struct Meta {
    width: u32,
    height: u32,
    fps: u32,
    kbps: u32,
    audio: bool,
}

/// How long a destination waits before trying again after its `attempt`th
/// failure in a row: 2, 4, 8 and 15 seconds, then every 30.
fn retry_after(attempt: u32) -> Duration {
    Duration::from_secs(match attempt {
        0 | 1 => 2,
        2 => 4,
        3 => 8,
        4 => 15,
        _ => 30,
    })
}

/// A connection live this long has made it: the next failure is the first
/// in a row again.
const STEADY: Duration = Duration::from_secs(10);

/// A destination's thread: connects at a keyframe, sends what comes, and
/// connects again after a failure, until the stream ends.
fn send_loop(dest: Destination, rx: Receiver<Packet>, status: Arc<Status>, i: usize, mut meta: Meta) {
    let mut conn: Option<Conn> = None;
    let mut attempt = 0u32;
    let mut retry_at: Option<Instant> = None;
    let mut avcc: Option<Arc<[u8]>> = None;
    let fail = |conn: &mut Option<Conn>, attempt: &mut u32, retry_at: &mut Option<Instant>, problem: Option<String>| {
        if let Some(c) = conn.take() {
            if c.opened.elapsed() >= STEADY && c.live() {
                *attempt = 0;
            }
            c.kill();
        }
        *attempt += 1;
        let at = Instant::now() + retry_after(*attempt);
        *retry_at = Some(at);
        let problem = problem.map(|p| dest.hide(&p));
        eprintln!("stream to {}: lost, trying again in {} s{}", dest.name, retry_after(*attempt).as_secs(), problem.as_ref().map(|p| format!(" ({p})")).unwrap_or_default());
        let attempt = *attempt;
        status.set(i, |d| {
            d.link = Link::Waiting { attempt, at };
            if problem.is_some() {
                d.problem = problem;
            }
        });
    };
    loop {
        let packet = match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(p) => Some(p),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        // A connection that ended by itself (ffmpeg said why).
        if let Some(c) = &mut conn {
            if let Some(problem) = c.ended() {
                fail(&mut conn, &mut attempt, &mut retry_at, Some(problem));
            } else if c.became_live() {
                let since = Instant::now();
                status.set(i, |d| {
                    d.link = Link::Live { since };
                    d.problem = None;
                });
                eprintln!("stream to {}: live", dest.name);
            }
        }
        let Some(packet) = packet else { continue };
        if let Packet::Video(f) = &packet
            && let Some(c) = &f.avcc
        {
            avcc = Some(c.clone());
        }
        if let Some(c) = &mut conn {
            if let Err(e) = c.send(&packet) {
                // Writing failed: ffmpeg went away. What it said, if it said.
                let said = c.problem().unwrap_or_else(|| format!("{e:#}"));
                fail(&mut conn, &mut attempt, &mut retry_at, Some(said));
            }
            continue;
        }
        // Not connected: once it's time, at a keyframe.
        if retry_at.is_some_and(|at| Instant::now() < at) {
            continue;
        }
        let (Packet::Video(frame), Some(config)) = (&packet, &avcc) else { continue };
        if !frame.key {
            continue;
        }
        if let Some((w, h)) = avc_size(config) {
            (meta.width, meta.height) = (w, h);
        }
        status.set(i, |d| d.link = Link::Connecting { attempt });
        match Conn::open(&dest, meta, config, frame.pts) {
            Ok(mut c) => match c.send(&packet) {
                Ok(()) => conn = Some(c),
                Err(e) => {
                    let said = c.problem().unwrap_or_else(|| format!("{e:#}"));
                    conn = Some(c);
                    fail(&mut conn, &mut attempt, &mut retry_at, Some(said));
                }
            },
            Err(e) => fail(&mut conn, &mut attempt, &mut retry_at, Some(format!("{e:#}"))),
        }
    }
    if let Some(c) = conn {
        c.finish();
    }
    status.set(i, |d| d.link = Link::Ended);
    eprintln!("stream to {}: ended", dest.name);
}

/// One connection: an ffmpeg sending what's written into it on to the
/// destination.
struct Conn {
    child: Child,
    stdin: Option<ChildStdin>,
    /// The stream's time at the connection's first keyframe: its 0.
    t0: f64,
    /// The decoder set-up last sent.
    avcc: Arc<[u8]>,
    log: Arc<Mutex<Said>>,
    reader: Option<JoinHandle<()>>,
    opened: Instant,
    /// Said live already.
    told_live: bool,
}

/// What a connection's ffmpeg has said.
#[derive(Default)]
struct Said {
    /// It's sending (it opened its output: connected, and publishing).
    live: bool,
    /// It ended (its output stopped).
    ended: bool,
    /// Its last few error lines, the key taken out.
    lines: Vec<String>,
}

impl Conn {
    fn open(dest: &Destination, meta: Meta, avcc: &Arc<[u8]>, t0: f64) -> Result<Self> {
        let mut child = ffmpeg()
            .args(["-hide_banner", "-nostats", "-loglevel", "info"])
            // Its streams are known from the FLV's start (its set-up
            // tags): no need to read seconds of it first, which would
            // hold the stream back that long.
            .args(["-probesize", "65536", "-analyzeduration", "500000", "-f", "flv", "-i", "pipe:0"])
            .args(["-map", "0", "-c", "copy", "-f", "flv", "-flvflags", "no_duration_filesize"])
            .arg(&dest.url)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("couldn't start ffmpeg")?;
        let stdin = child.stdin.take();
        let stderr = child.stderr.take();
        let log = Arc::new(Mutex::new(Said::default()));
        let (log2, dest2) = (log.clone(), dest.clone());
        let reader = stderr.and_then(|e| thread::Builder::new().name("stream-log".into()).spawn(move || read_log(e, &dest2, &log2)).ok());
        let mut conn = Self { child, stdin, t0, avcc: avcc.clone(), log, reader, opened: Instant::now(), told_live: false };
        let mut head = flv_header(meta.audio);
        head.extend(script_tag(&meta));
        head.extend(video_tag(0, true, true, avcc));
        if meta.audio {
            head.extend(audio_tag(0, true, &AAC_CONFIG));
        }
        conn.write(&head)?;
        Ok(conn)
    }

    /// Write a picture or sound into the connection.
    fn send(&mut self, p: &Packet) -> std::io::Result<()> {
        match p {
            Packet::Video(f) => {
                let ms = ms(f.pts - self.t0);
                let mut out = Vec::with_capacity(f.data.len() + 64);
                // Its set-up changed (the encoder said so): sent again first.
                if let Some(c) = f.avcc.as_ref().filter(|c| c[..] != self.avcc[..]) {
                    out.extend(video_tag(ms, true, true, c));
                    self.avcc = c.clone();
                }
                out.extend(video_tag(ms, f.key, false, &f.data));
                self.write(&out)
            }
            Packet::Audio(data, at) => {
                // Sound from before the connection's first picture is left out.
                if *at < self.t0 {
                    return Ok(());
                }
                self.write(&audio_tag(ms(at - self.t0), false, data))
            }
        }
    }

    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self.stdin.as_mut() {
            Some(s) => s.write_all(bytes),
            None => Err(std::io::ErrorKind::BrokenPipe.into()),
        }
    }

    /// Whether ffmpeg has said it's sending.
    fn live(&self) -> bool {
        self.log.lock().is_ok_and(|l| l.live)
    }

    /// It's sending, newly: true once.
    fn became_live(&mut self) -> bool {
        if self.told_live || !self.live() {
            return false;
        }
        self.told_live = true;
        true
    }

    /// Why it ended, once ffmpeg has stopped.
    fn ended(&mut self) -> Option<String> {
        let gone = self.log.lock().is_ok_and(|l| l.ended) || matches!(self.child.try_wait(), Ok(Some(_)));
        gone.then(|| self.problem().unwrap_or_else(|| "the connection closed".into()))
    }

    /// What ffmpeg said went wrong, if it said.
    fn problem(&self) -> Option<String> {
        let log = self.log.lock().ok()?;
        problem_of(&log.lines)
    }

    /// The stream is over: its input closed, ffmpeg ends it properly (up to
    /// a few seconds), else it's stopped.
    fn finish(mut self) {
        drop(self.stdin.take());
        let since = Instant::now();
        while since.elapsed() < Duration::from_secs(3) {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        self.kill();
    }

    fn kill(mut self) {
        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
    }
}

/// Read what a connection's ffmpeg says: whether it's sending, and its
/// errors, the key taken out. Nothing it says is logged as it is (the
/// address has the key).
fn read_log(stderr: std::process::ChildStderr, dest: &Destination, log: &Mutex<Said>) {
    for line in BufReader::new(stderr).lines() {
        let Ok(line) = line else { break };
        let Ok(mut l) = log.lock() else { break };
        // "Output #0, flv, to 'rtmp://…'": its output is open (connected and
        // publishing).
        if line.starts_with("Output #0") {
            l.live = true;
            continue;
        }
        if is_problem(&line) {
            l.lines.push(dest.hide(line.trim()));
            if l.lines.len() > 6 {
                l.lines.remove(0);
            }
        }
    }
    if let Ok(mut l) = log.lock() {
        l.ended = true;
    }
}

/// Whether a line ffmpeg wrote says something went wrong.
fn is_problem(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    ["error", "failed", "refused", "unable", "invalid", "broken pipe", "timed out", "not found", "cannot", "could not", "denied", "reset"].iter().any(|w| l.contains(w))
}

/// The most telling of what ffmpeg said went wrong, in a few words: an
/// RTMP server's own words first, then the last thing said.
fn problem_of(lines: &[String]) -> Option<String> {
    let pick = lines.iter().rev().find(|l| l.contains("Server error")).or_else(|| lines.last())?;
    // "[rtmp @ 0000021f…] Server error: …" → "Server error: …".
    let text = match pick.strip_prefix('[') {
        Some(rest) => rest.split_once("] ").map_or(pick.as_str(), |(_, t)| t),
        None => pick.as_str(),
    };
    Some(text.trim().to_owned())
}

/// Milliseconds from the connection's start, as FLV counts them.
fn ms(secs: f64) -> u32 {
    (secs.max(0.0) * 1000.0).round() as u32
}

/// The picture's width and height, from its decoder set-up (the SPS in
/// an `avcC` record).
fn avc_size(avcc: &[u8]) -> Option<(u32, u32)> {
    let len = u16::from_be_bytes([*avcc.get(6)?, *avcc.get(7)?]) as usize;
    let sps = avcc.get(8..8 + len)?;
    crate::h264::sps_size(sps)
}

// ---------------------------------------------------------------------------
// FLV
// ---------------------------------------------------------------------------

/// FLV's own header, and the size of the (no) tag before the first.
fn flv_header(audio: bool) -> Vec<u8> {
    let flags = 0x01 | if audio { 0x04 } else { 0 };
    vec![b'F', b'L', b'V', 1, flags, 0, 0, 0, 9, 0, 0, 0, 0]
}

/// A tag of `kind` (8 sound, 9 picture, 18 script) at `ms`, then its size.
fn tag(kind: u8, ms: u32, body: &[u8]) -> Vec<u8> {
    let len = body.len() as u32;
    let mut t = Vec::with_capacity(body.len() + 15);
    t.push(kind);
    t.extend_from_slice(&len.to_be_bytes()[1..]);
    t.extend_from_slice(&ms.to_be_bytes()[1..]);
    t.push((ms >> 24) as u8);
    t.extend_from_slice(&[0, 0, 0]);
    t.extend_from_slice(body);
    t.extend_from_slice(&(len + 11).to_be_bytes());
    t
}

/// A picture: its NAL units (`set_up`: the decoder's set-up, an `avcC`
/// record, instead).
fn video_tag(ms: u32, key: bool, set_up: bool, data: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(data.len() + 5);
    body.push(if key { 0x17 } else { 0x27 });
    body.push(if set_up { 0 } else { 1 });
    // Shown as decoded: no B-frames.
    body.extend_from_slice(&[0, 0, 0]);
    body.extend_from_slice(data);
    tag(9, ms, &body)
}

/// A packet of sound (`set_up`: its AudioSpecificConfig instead).
fn audio_tag(ms: u32, set_up: bool, data: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(data.len() + 2);
    // AAC (10), "44 kHz" (always, for AAC), 16-bit, stereo.
    body.push(0xAF);
    body.push(if set_up { 0 } else { 1 });
    body.extend_from_slice(data);
    tag(8, ms, &body)
}

/// The stream's description (`onMetaData`): its size, rate and codecs, so
/// what reads it needn't work them out from the first seconds.
fn script_tag(m: &Meta) -> Vec<u8> {
    let mut b = Vec::new();
    amf_string(&mut b, "onMetaData");
    let mut fields: Vec<(&str, Amf)> = vec![
        ("duration", Amf::Number(0.0)),
        ("width", Amf::Number(m.width as f64)),
        ("height", Amf::Number(m.height as f64)),
        ("framerate", Amf::Number(m.fps as f64)),
        ("videocodecid", Amf::Number(7.0)),
        ("videodatarate", Amf::Number(m.kbps as f64)),
    ];
    if m.audio {
        fields.extend([
            ("audiocodecid", Amf::Number(10.0)),
            ("audiosamplerate", Amf::Number(AUDIO_RATE)),
            ("audiosamplesize", Amf::Number(16.0)),
            ("stereo", Amf::Bool(true)),
        ]);
    }
    if m.width == 0 {
        fields.retain(|(k, _)| *k != "width" && *k != "height");
    }
    b.push(0x08);
    b.extend_from_slice(&(fields.len() as u32).to_be_bytes());
    for (k, v) in fields {
        b.extend_from_slice(&(k.len() as u16).to_be_bytes());
        b.extend_from_slice(k.as_bytes());
        match v {
            Amf::Number(n) => {
                b.push(0x00);
                b.extend_from_slice(&n.to_be_bytes());
            }
            Amf::Bool(on) => b.extend_from_slice(&[0x01, on as u8]),
        }
    }
    b.extend_from_slice(&[0, 0, 9]);
    tag(18, 0, &b)
}

enum Amf {
    Number(f64),
    Bool(bool),
}

fn amf_string(b: &mut Vec<u8>, s: &str) {
    b.push(0x02);
    b.extend_from_slice(&(s.len() as u16).to_be_bytes());
    b.extend_from_slice(s.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(join("rtmp://live.twitch.tv/app", "live_123_abc"), "rtmp://live.twitch.tv/app/live_123_abc");
        assert_eq!(join("rtmp://live.twitch.tv/app/", " live_123_abc "), "rtmp://live.twitch.tv/app/live_123_abc");
        assert_eq!(join("rtmps://x.example/app/sk_1", "sk_1"), "rtmps://x.example/app/sk_1", "the key's on it already");
        assert_eq!(join("rtmp://my.server/live/stream", ""), "rtmp://my.server/live/stream");
    }

    /// The key never shows in what's said about a destination, nor the
    /// address it's in.
    #[test]
    fn the_key_is_hidden() {
        let d = Destination { name: "Twitch".into(), url: "rtmp://live.twitch.tv/app/live_123_abc".into(), secret: "live_123_abc".into(), quality: Quality { short_side: None, fps: 60, kbps: 6000 } };
        assert_eq!(d.hide("[rtmp @ 0x1] rtmp://live.twitch.tv/app/live_123_abc: I/O error"), "[rtmp @ 0x1] Twitch: I/O error");
        assert_eq!(d.hide("Server error: bad key live_123_abc"), "Server error: bad key ••••");
        assert_eq!(problem_of(&["[rtmp @ 0000021f] Server error: Invalid stream key".into(), "Error opening output Twitch: I/O error".into()]).unwrap(), "Server error: Invalid stream key");
        assert_eq!(problem_of(&["[out#0/flv @ 1] Error opening output Twitch: I/O error".into()]).unwrap(), "Error opening output Twitch: I/O error");
    }

    /// Each stream frame is one of the capture's: a whole fraction of its
    /// rate, at most what's asked (and 60).
    #[test]
    fn each_stream_frame_is_one_of_the_captures() {
        assert_eq!(stream_rate(60, 60), 60);
        assert_eq!(stream_rate(120, 60), 60);
        assert_eq!(stream_rate(240, 60), 60);
        assert_eq!(stream_rate(144, 60), 48);
        assert_eq!(stream_rate(165, 60), 55);
        assert_eq!(stream_rate(60, 30), 30);
        assert_eq!(stream_rate(50, 60), 50);
        assert_eq!(stream_rate(30, 60), 30);
        assert_eq!(stream_rate(240, 144), 60, "never over 60");
        assert_eq!(stream_rate(75, 60), 25);
    }

    /// The rates a stream can have are the capture's whole fractions from
    /// 60 down to 20: 60 and 30 from 60, none of them from 144.
    #[test]
    fn rates_offered() {
        assert_eq!(stream_rates(60), [60, 30, 20]);
        assert_eq!(stream_rates(144), [48, 36, 24]);
        assert_eq!(stream_rates(120), [60, 40, 30, 24]);
        assert_eq!(stream_rates(240), [60, 48, 40, 30]);
        assert_eq!(stream_rates(165), [55, 33]);
        assert_eq!(stream_rates(15), [15]);
    }

    /// One encoder per different picture: two places sent the same share
    /// it, whatever rate each asked for that comes out the same.
    #[test]
    fn places_alike_share_a_picture() {
        let d = |name: &str, short_side, fps, kbps| Destination { name: name.into(), url: String::new(), secret: String::new(), quality: Quality { short_side, fps, kbps } };
        let s = StreamSettings {
            destinations: vec![d("Twitch", Some(1080), 60, 6000), d("YouTube", Some(1440), 60, 12000), d("Kick", Some(1080), 50, 6000)],
            status: Status::new(),
        };
        let (list, of) = renditions(&s, 144);
        assert_eq!(list, [Rendition { short_side: Some(1080), fps: 48, kbps: 6000 }, Rendition { short_side: Some(1440), fps: 48, kbps: 12000 }]);
        assert_eq!(of, [0, 1, 0], "50 at most from 144 is 48 too");
    }

    #[test]
    fn retries_slow_down() {
        let secs: Vec<u64> = (1..=7).map(|n| retry_after(n).as_secs()).collect();
        assert_eq!(secs, [2, 4, 8, 15, 30, 30, 30]);
    }

    /// Tags as FLV has them: type, size, time (its top byte last), stream
    /// 0, the body, then the tag's whole size.
    #[test]
    fn flv_tags() {
        assert_eq!(flv_header(true), [b'F', b'L', b'V', 1, 5, 0, 0, 0, 9, 0, 0, 0, 0]);
        assert_eq!(flv_header(false)[4], 1);
        let t = video_tag(0x0102_0304, true, false, &[0, 0, 0, 1, 0x65]);
        assert_eq!(t[0], 9);
        assert_eq!(&t[1..4], &[0, 0, 10], "5 bytes of header and 5 of picture");
        assert_eq!(&t[4..8], &[0x02, 0x03, 0x04, 0x01], "the time's top byte comes last");
        assert_eq!(&t[8..11], &[0, 0, 0]);
        assert_eq!(&t[11..16], &[0x17, 1, 0, 0, 0]);
        assert_eq!(&t[16..21], &[0, 0, 0, 1, 0x65]);
        assert_eq!(&t[21..], &(21u32).to_be_bytes());
        assert_eq!(video_tag(40, false, false, &[1])[11], 0x27);
        assert_eq!(&video_tag(0, true, true, &[1])[11..13], &[0x17, 0]);
        let a = audio_tag(23, true, &AAC_CONFIG);
        assert_eq!((a[0], &a[11..15]), (8, &[0xAF, 0, 0x11, 0x90][..]));
        let s = script_tag(&Meta { width: 1920, height: 1080, fps: 60, kbps: 6000, audio: true });
        assert_eq!(s[0], 18);
        assert_eq!(&s[11..24], b"\x02\x00\x0aonMetaData");
    }

    /// Pictures and sound as the encoders make them, from ffmpeg: 640×360
    /// at 30 fps, a keyframe a second, and a tone, `secs` long. The
    /// pictures as access units (NAL units with 4-byte lengths, keyframes
    /// with their set-up); the sound as AAC packets.
    fn encoded(dir: &Path, secs: u32) -> (Vec<StreamFrame>, Vec<Vec<u8>>) {
        let (v, a) = (dir.join("v.h264"), dir.join("a.aac"));
        let made = ffmpeg()
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=640x360:rate=30", "-t"])
            .arg(secs.to_string())
            .args(["-c:v", "libx264", "-g", "30", "-bf", "0", "-x264-params", "aud=1", "-f", "h264"])
            .arg(&v)
            .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000", "-t"])
            .arg(secs.to_string())
            .args(["-map", "1:a", "-ac", "2", "-c:a", "aac", "-f", "adts"])
            .arg(&a)
            .status()
            .unwrap();
        assert!(made.success());
        let es = std::fs::read(&v).unwrap();
        // Access units start at their delimiters (00 00 00 01 09).
        let mut starts: Vec<usize> = es.windows(5).enumerate().filter(|(_, w)| *w == [0, 0, 0, 1, 9]).map(|(i, _)| i).collect();
        starts.push(es.len());
        let frames = starts
            .windows(2)
            .enumerate()
            .map(|(n, w)| {
                let au = crate::mp4mux::annexb_to_avcc(&es[w[0]..w[1]]);
                StreamFrame { rendition: 0, data: Arc::from(au.data), avcc: au.config.map(|c| Arc::from(c.record())), pts: n as f64 / 30.0, key: au.idr }
            })
            .collect();
        // ADTS: a 7-byte header (9 with its checksum) before each packet.
        let adts = std::fs::read(&a).unwrap();
        let mut packets = Vec::new();
        let mut at = 0;
        while at + 7 <= adts.len() {
            let h = &adts[at..];
            let len = ((h[3] as usize & 3) << 11) | ((h[4] as usize) << 3) | (h[5] as usize >> 5);
            let head = if h[1] & 1 == 1 { 7 } else { 9 };
            packets.push(adts[at + head..at + len].to_vec());
            at += len;
        }
        (frames, packets)
    }

    /// Our FLV, through an ffmpeg of ours, to an RTMP server (an ffmpeg
    /// listening for it): it says it's live, and what the server got plays,
    /// pictures and sound, as long as what was sent.
    #[test]
    fn streams_to_a_server() {
        if ffmpeg().arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_err() {
            eprintln!("no ffmpeg here: skipped");
            return;
        }
        let dir = std::env::temp_dir().join(format!("hc-stream-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (frames, packets) = encoded(&dir, 4);
        assert_eq!(frames.len(), 120);
        assert!(frames[0].key && frames[0].avcc.is_some() && !frames[1].key);
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let url = format!("rtmp://127.0.0.1:{port}/live/sk_test_key");
        let got = dir.join("got.flv");
        let mut server = ffmpeg().args(["-v", "error", "-y", "-timeout", "20", "-i"]).arg(&url).args(["-c", "copy"]).arg(&got).stdin(Stdio::null()).spawn().unwrap();
        thread::sleep(Duration::from_millis(700));
        let status = Status::new();
        let settings = StreamSettings {
            destinations: vec![Destination { name: "Test".into(), url: url.clone(), secret: "sk_test_key".into(), quality: Quality { short_side: None, fps: 30, kbps: 1000 } }],
            status: status.clone(),
        };
        let mut sink = Sink::start(&settings, true, 30);
        let mut sound = packets.iter().enumerate().peekable();
        for f in &frames {
            sink.video(f);
            // The sound up to this picture, as it comes (it's late, here not).
            while let Some((i, p)) = sound.next_if(|(i, _)| *i as f64 * 1024.0 / AUDIO_RATE <= f.pts) {
                sink.audio(p, i as i64 * 1024);
            }
            thread::sleep(Duration::from_millis(10));
        }
        for (i, p) in sound {
            sink.audio(p, i as i64 * 1024);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while !matches!(status.get()[0].link, Link::Live { .. }) {
            assert!(Instant::now() < deadline, "never live: {:?}", status.get());
            thread::sleep(Duration::from_millis(50));
        }
        sink.stop();
        assert_eq!(status.get()[0].link, Link::Ended);
        let deadline = Instant::now() + Duration::from_secs(10);
        while server.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "the server didn't end");
            thread::sleep(Duration::from_millis(50));
        }
        let probe = Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "stream=codec_name,width,height:format=duration", "-of", "default=nw=1"])
            .arg(&got)
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&probe.stdout).to_string();
        assert!(said.contains("codec_name=h264") && said.contains("width=640") && said.contains("height=360"), "{said}");
        assert!(said.contains("codec_name=aac"), "{said}");
        let duration: f64 = said.lines().find_map(|l| l.strip_prefix("duration=")).and_then(|d| d.parse().ok()).unwrap_or(0.0);
        assert!((3.8..4.3).contains(&duration), "{duration} s: {said}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The picture's size is read from its set-up.
    #[test]
    fn size_from_the_set_up() {
        // 1920×1080 High profile SPS (as NVENC makes it).
        let sps = [0x67, 0x64, 0x00, 0x2A, 0xAC, 0x2B, 0x40, 0x3C, 0x01, 0x13, 0xF2, 0xE0, 0x22, 0x00, 0x00, 0x03, 0x00, 0x02, 0x00, 0x00, 0x03, 0x00, 0x79, 0x08];
        let config = crate::mp4mux::AvcConfig { sps: sps.to_vec(), pps: vec![0x68, 0xEE, 0x3C, 0xB0] };
        assert_eq!(avc_size(&config.record()), Some((1920, 1080)));
    }
}
