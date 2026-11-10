//! Live audio: per-source volume/mute controls and level meters shared with the
//! UI ([`LiveAudio`]), and the mixer that turns timestamped source audio into the
//! recorded tracks.
//!
//! Every source is aligned on one host clock: [`SourceFeed`] places each block of
//! samples by its capture timestamp (gaps become silence, overlaps are dropped,
//! a clock that runs a little fast or slow is steered back a frame at a time)
//! and converts it to 48 kHz stereo. The mixer then pulls exactly the same span
//! from every feed a short moment behind real time, applies volume, meters it, and
//! writes the mix and the separate tracks — so tracks can never drift apart.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{Fft, FixedSync, Resampler};

/// Sample rate of every recorded track.
pub const RATE: u32 = 48_000;

/// Limiter ceiling: -1 dBFS, leaving headroom for the AAC encoder's overshoot.
const CEILING: f32 = 0.891;

/// An `f32` that can be shared between threads without a lock.
#[derive(Debug, Default)]
struct AtomicF32(AtomicU32);

impl AtomicF32 {
    fn new(v: f32) -> Self {
        Self(AtomicU32::new(v.to_bits()))
    }
    fn load(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
    fn store(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed);
    }
    fn swap(&self, v: f32) -> f32 {
        f32::from_bits(self.0.swap(v.to_bits(), Ordering::Relaxed))
    }
    /// For non-negative values the bit patterns sort like the floats do.
    fn fetch_max(&self, v: f32) {
        self.0.fetch_max(v.max(0.0).to_bits(), Ordering::Relaxed);
    }
}

/// Left/right levels since the meter was last read, linear (1.0 = full scale).
#[derive(Debug, Default, Clone, Copy)]
pub struct Levels {
    pub peak: [f32; 2],
    pub rms: [f32; 2],
}

impl Levels {
    pub fn max_peak(&self) -> f32 {
        self.peak[0].max(self.peak[1])
    }
}

/// A level meter, written by the audio thread and read by the UI.
#[derive(Debug, Default)]
pub struct Meter {
    peak: [AtomicF32; 2],
    /// Sum of squares per channel and frame count since the last read, plus the
    /// RMS last handed out (for reads that land between two blocks).
    energy: Mutex<Energy>,
}

#[derive(Debug, Default)]
struct Energy {
    sum: [f64; 2],
    frames: usize,
    last_rms: [f32; 2],
}

impl Meter {
    /// Levels since the last call. Taking resets them, so no transient is missed
    /// between UI frames and the RMS covers everything in between.
    pub fn take(&self) -> Levels {
        let peak = [self.peak[0].swap(0.0), self.peak[1].swap(0.0)];
        let mut e = self.energy.lock().unwrap();
        if e.frames > 0 {
            let n = e.frames as f64;
            e.last_rms = [(e.sum[0] / n).sqrt() as f32, (e.sum[1] / n).sqrt() as f32];
            e.sum = [0.0; 2];
            e.frames = 0;
        }
        Levels { peak, rms: e.last_rms }
    }

    fn record(&self, block: &[[f32; 2]]) {
        let mut peak = [0f32; 2];
        let mut sum = [0f64; 2];
        for f in block {
            for c in 0..2 {
                peak[c] = peak[c].max(f[c].abs());
                sum[c] += (f[c] * f[c]) as f64;
            }
        }
        for c in 0..2 {
            self.peak[c].fetch_max(peak[c]);
        }
        let mut e = self.energy.lock().unwrap();
        e.sum[0] += sum[0];
        e.sum[1] += sum[1];
        e.frames += block.len();
    }

    fn clear(&self) {
        for p in &self.peak {
            p.store(0.0);
        }
        *self.energy.lock().unwrap() = Energy::default();
    }
}

/// What a source is doing right now, for the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceStatus {
    /// Not being captured.
    Off,
    Live,
    /// An app source whose app isn't running; it starts by itself when it opens.
    WaitingForApp,
    /// The device isn't connected (or couldn't be opened).
    Unavailable,
}

/// Live controls and meter for one source.
#[derive(Debug)]
pub struct Channel {
    gain: AtomicF32,
    muted: AtomicBool,
    status: AtomicU8,
    /// After volume and mute: what goes into the clip.
    pub meter: Meter,
    /// Before volume and mute: what the source delivers (shows a muted mic is live).
    pub input: Meter,
    /// Gets the source's sound as it arrives (before volume and mute), mono at
    /// [`RATE`], when set: for listening to the microphone ("HasteClip
    /// that").
    tap: Mutex<Option<std::sync::mpsc::Sender<Vec<f32>>>>,
    /// Noise removal on its sound (a microphone's), a [`NoiseReduction`]:
    /// shared with its feed, which does it as the sound arrives.
    denoise: Arc<AtomicU8>,
    /// Its two sides made one (a microphone's, see [`Mono`]): shared with
    /// its feed, like `denoise`.
    mono: Arc<AtomicBool>,
    /// The programs a desktop source records right now (see [`Program`]).
    programs: Mutex<Vec<Program>>,
}

/// A program a desktop source records: one with an audio session, so one
/// that plays (or has played) sound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    /// Its executable, `FXSound.exe`: what a desktop source's `leave_out` takes.
    pub exe: String,
    /// What to call it: its file description ("FxSound"), else its executable.
    pub name: String,
}

/// How much of a microphone's background noise is taken out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NoiseReduction {
    #[default]
    Off,
    /// The noise that stays the same (fans, hum, hiss): what comes and goes
    /// stays, a keyboard as well as a voice.
    Light,
    /// Everything but a voice (RNNoise), a keyboard too.
    Strong,
}

impl NoiseReduction {
    fn from_u8(n: u8) -> Self {
        match n {
            1 => Self::Light,
            2 => Self::Strong,
            _ => Self::Off,
        }
    }

    /// How much of the sound as it came, Light's and Strong's make up the sound.
    fn weights(self) -> [f32; 3] {
        match self {
            Self::Off => [1.0, 0.0, 0.0],
            Self::Light => [0.0, 1.0, 0.0],
            Self::Strong => [0.0, 0.0, 1.0],
        }
    }
}

impl Channel {
    fn new() -> Self {
        Self {
            gain: AtomicF32::new(1.0),
            muted: AtomicBool::new(false),
            status: AtomicU8::new(0),
            meter: Meter::default(),
            input: Meter::default(),
            tap: Mutex::new(None),
            denoise: Arc::new(AtomicU8::new(NoiseReduction::Off as u8)),
            mono: Arc::new(AtomicBool::new(false)),
            programs: Mutex::new(Vec::new()),
        }
    }

    /// The programs a desktop source records right now, by name (where
    /// [`crate::DESKTOP_PROGRAMS`]; empty while it isn't recording).
    pub fn programs(&self) -> Vec<Program> {
        self.programs.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    pub(crate) fn set_programs(&self, programs: Vec<Program>) {
        *self.programs.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = programs;
    }

    /// Take background noise out of its sound: steady noise (fans, hum), or
    /// everything but a voice. Takes effect immediately, even mid-recording.
    pub fn set_denoise(&self, mode: NoiseReduction) {
        self.denoise.store(mode as u8, Ordering::Relaxed);
    }

    pub(crate) fn denoise_mode(&self) -> Arc<AtomicU8> {
        self.denoise.clone()
    }

    /// Play its sound in the middle, both sides made one (for a mic heard on
    /// one side only). Takes effect immediately, even mid-recording.
    pub fn set_mono(&self, on: bool) {
        self.mono.store(on, Ordering::Relaxed);
    }

    pub(crate) fn mono_flag(&self) -> Arc<AtomicBool> {
        self.mono.clone()
    }

    /// Hand the source's sound, mono at [`RATE`], to `tap` (`None`: stop).
    pub fn set_tap(&self, tap: Option<std::sync::mpsc::Sender<Vec<f32>>>) {
        *self.tap.lock().unwrap() = tap;
    }

    fn send_tap(&self, block: &[[f32; 2]]) {
        let mut tap = self.tap.lock().unwrap();
        if let Some(tx) = tap.as_ref()
            && tx.send(block.iter().map(|f| (f[0] + f[1]) * 0.5).collect()).is_err()
        {
            *tap = None;
        }
    }

    /// Volume as linear gain (1.0 = unchanged). Takes effect immediately, even
    /// mid-recording.
    pub fn set_volume(&self, gain: f32, muted: bool) {
        self.gain.store(gain.max(0.0));
        self.muted.store(muted, Ordering::Relaxed);
    }

    /// The gain applied now: 0 when muted.
    pub fn gain(&self) -> f32 {
        self.effective_gain()
    }

    fn effective_gain(&self) -> f32 {
        if self.muted.load(Ordering::Relaxed) { 0.0 } else { self.gain.load() }
    }

    pub fn status(&self) -> SourceStatus {
        match self.status.load(Ordering::Relaxed) {
            1 => SourceStatus::Live,
            2 => SourceStatus::WaitingForApp,
            3 => SourceStatus::Unavailable,
            _ => SourceStatus::Off,
        }
    }

    pub(crate) fn set_status(&self, s: SourceStatus) {
        let v = match s {
            SourceStatus::Off => 0,
            SourceStatus::Live => 1,
            SourceStatus::WaitingForApp => 2,
            SourceStatus::Unavailable => 3,
        };
        self.status.store(v, Ordering::Relaxed);
        if s != SourceStatus::Live {
            self.meter.clear();
            self.input.clear();
        }
    }
}

/// Volume controls and meters for every source plus the clip's mix, shared by the
/// UI, the level monitor and the recorder. Keyed by the source's stable id.
#[derive(Debug, Default)]
pub struct LiveAudio {
    channels: Mutex<HashMap<String, Arc<Channel>>>,
    /// The clip's mix (track 1), after the limiter.
    pub master: Meter,
    limiter: AtomicBool,
    /// Largest limiter gain reduction (dB) since the UI last looked.
    reduction_db: AtomicF32,
}

impl LiveAudio {
    pub fn new() -> Arc<Self> {
        let live = Self::default();
        live.limiter.store(true, Ordering::Relaxed);
        Arc::new(live)
    }

    /// The channel for a source id (created at unity gain on first use).
    pub fn channel(&self, id: &str) -> Arc<Channel> {
        self.channels.lock().unwrap().entry(id.to_owned()).or_insert_with(|| Arc::new(Channel::new())).clone()
    }

    pub fn set_limiter(&self, on: bool) {
        self.limiter.store(on, Ordering::Relaxed);
    }

    /// How hard the limiter worked since the last call, in dB (0 = untouched).
    pub fn take_reduction_db(&self) -> f32 {
        self.reduction_db.swap(0.0)
    }

    /// Mark every source as not captured (when capture stops).
    pub(crate) fn all_off(&self) {
        for ch in self.channels.lock().unwrap().values() {
            ch.set_status(SourceStatus::Off);
        }
        self.master.clear();
    }
}

/// Host time of the first video frame (or monitor start): t=0 for every track.
#[derive(Debug, Default)]
pub(crate) struct Clock {
    t0_bits: AtomicU64,
}

impl Clock {
    pub(crate) fn get(&self) -> Option<f64> {
        let bits = self.t0_bits.load(Ordering::Acquire);
        (bits != 0).then(|| f64::from_bits(bits))
    }
    pub(crate) fn set(&self, t0: f64) {
        self.t0_bits.store(t0.to_bits(), Ordering::Release);
    }
}

/// One source's audio, placed on the shared clock and converted to 48 kHz stereo,
/// waiting for the mixer.
pub(crate) struct SourceFeed {
    rate: u32,
    clock: Arc<Clock>,
    st: Mutex<FeedState>,
    /// Feeds summed into this one: a source made of several streams (on
    /// Windows, one per captured process tree), each placed on the clock by
    /// itself.
    children: Mutex<Vec<Arc<SourceFeed>>>,
}

struct FeedState {
    /// Next expected input frame (at the source's own rate), once started.
    next_in: Option<i64>,
    resampler: Option<Fft<f32>>,
    /// Input waiting for a full resampler chunk, per channel.
    pending: [Vec<f32>; 2],
    /// Converted frames; `out_base` is the output frame index of the first one.
    out: VecDeque<[f32; 2]>,
    out_base: i64,
    /// The output frame just past the newest audio pushed. Not `out_base +
    /// out.len()`: the mixer moves `out_base` on through a source that's gone
    /// quiet, which made one that sent nothing look up to date, and the
    /// meters waited for it forever.
    heard: i64,
    /// Output frames still to discard: resampler delay, or audio that arrived
    /// after the mixer had already moved past it.
    skip_out: i64,
    /// Input frames still to throw away: sound that came twice (an overlap
    /// put right at once), from the packets that bring it.
    skip_in: i64,
    /// Keeps its sound on its timestamps while its clock runs a little fast
    /// or slow.
    steer: Steer,
    /// Both sides made one, when this feed is a microphone's: before noise
    /// removal, which then cleans what's heard.
    mono: Option<Mono>,
    /// Noise removal, when this feed is a microphone's.
    denoise: Option<Denoise>,
    /// Its stream has stopped (see [`SourceFeed::remove_child`]): it takes
    /// no more sound, and goes once the mixer has taken what it has.
    closed: bool,
}

impl SourceFeed {
    /// Timing off by more than this, even for a packet, is put right at
    /// once: a gap (a dropout) filled with silence, or what came twice thrown
    /// away. Smaller sudden offsets are too, once they're kept up, and a
    /// clock that runs a little fast or slow is steered back (see [`Steer`]).
    const TOLERANCE: f64 = 0.04;
    /// A gap longer than this starts the feed over instead of being filled
    /// with silence (a guess: longer than any dropout worth keeping in step).
    const RESTART_AFTER: f64 = 1.0;

    pub(crate) fn new(rate: u32, clock: Arc<Clock>) -> Arc<Self> {
        let resampler = (rate != RATE)
            .then(|| Fft::<f32>::new(rate as usize, RATE as usize, 1024, 2, FixedSync::Input).ok())
            .flatten();
        Arc::new(Self {
            rate,
            clock,
            st: Mutex::new(FeedState {
                next_in: None,
                resampler,
                pending: [Vec::new(), Vec::new()],
                out: VecDeque::new(),
                out_base: 0,
                heard: 0,
                skip_out: 0,
                skip_in: 0,
                steer: Steer::default(),
                mono: None,
                denoise: None,
                closed: false,
            }),
            children: Mutex::new(Vec::new()),
        })
    }

    /// Take noise out of this feed's sound as `mode` says (a
    /// [`NoiseReduction`]; it can change at any moment). Delays it by one
    /// RNNoise frame, 10 ms, on or off, so switching doesn't shift it in time.
    pub(crate) fn denoise_with(&self, mode: Arc<AtomicU8>) {
        let mut st = self.st.lock().unwrap();
        st.denoise = Some(Denoise::new(mode));
    }

    /// Make this feed's two sides one while `on` is set (it can change at any
    /// moment). See [`Mono`].
    pub(crate) fn mono_with(&self, on: Arc<AtomicBool>) {
        self.st.lock().unwrap().mono = Some(Mono::new(on));
    }

    /// A new feed (at `rate`) whose audio is added to this one's. Closed
    /// from the start if this one is (see [`Self::close`]).
    pub(crate) fn add_child(&self, rate: u32) -> Arc<SourceFeed> {
        let child = SourceFeed::new(rate, self.clock.clone());
        // Its state held throughout, so it can't be closed in between.
        let st = self.st.lock().unwrap();
        if st.closed {
            child.st.lock().unwrap().closed = true;
        } else {
            self.children.lock().unwrap().push(child.clone());
        }
        drop(st);
        child
    }

    /// Its source has stopped being captured (taken out while recording):
    /// it takes no more sound, nor do the feeds it sums, now or added
    /// later. What they've sent is still taken: the mixer runs behind, and
    /// mixes it to its end.
    #[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
    pub(crate) fn close(&self) {
        // Held while its feeds are closed: none is added meanwhile.
        let mut st = self.st.lock().unwrap();
        st.closed = true;
        for child in self.children.lock().unwrap().iter() {
            child.close();
        }
        drop(st);
    }

    /// Stop summing `child`, whose stream has stopped (a mic switched or
    /// opened again, an app that quit, the player closed), once the mixer
    /// has taken the sound it had sent: up to the mix's 0.3 s behind real
    /// time, which went with it. It takes no more sound from now on.
    pub(crate) fn remove_child(&self, child: &Arc<SourceFeed>) {
        child.st.lock().unwrap().closed = true;
    }

    /// Interleaved samples (`channels` per frame) whose first frame was captured
    /// at host time `start`. Mono is played on both sides; extra channels beyond
    /// two are dropped.
    pub(crate) fn push(&self, start: f64, samples: &[f32], channels: usize) {
        let Some(t0) = self.clock.get() else { return };
        let channels = channels.max(1);
        let frames = samples.len() / channels;
        let rate = self.rate as f64;
        let expected = ((start - t0) * rate).round() as i64;
        let mut st = self.st.lock().unwrap();
        if st.closed {
            return;
        }

        // Back after a long gap (playback paused, a device that stopped):
        // start over at its time, as on its first sound, rather than fill the
        // gap with silence: the mixer reads silence where nothing arrived
        // anyway, and filling it at the source's rate went through the
        // resampler (384 kHz output: a long pause took tens of seconds, in an
        // audio callback the player's closing waited on).
        if st.next_in.is_some_and(|next| expected - next > (Self::RESTART_AFTER * rate) as i64) {
            st.next_in = None;
            st.pending.iter_mut().for_each(Vec::clear);
            if let Some(r) = st.resampler.as_mut() {
                r.reset();
            }
        }
        if st.next_in.is_none() {
            st.next_in = Some(expected);
            st.out.clear();
            st.out_base = (expected as f64 * RATE as f64 / rate).round() as i64;
            st.skip_out = st.resampler.as_ref().map_or(0, |r| r.output_delay() as i64)
                + if st.denoise.is_some() { Denoise::DELAY as i64 } else { 0 };
            st.skip_in = 0;
            st.steer = Steer::default();
        }
        let drift = expected - st.next_in.unwrap();
        let tolerance = (Self::TOLERANCE * rate) as i64;
        let mut slips = 0;
        // A big jump at once; a smaller sudden one once it's kept up; a
        // clock's drift a frame at a time. Not while an overlap is still
        // being thrown away.
        let jump = if st.skip_in > 0 {
            None
        } else if drift.abs() > tolerance {
            Some(drift)
        } else {
            match st.steer.step(drift, frames, rate) {
                Steering::Slip(n) => {
                    slips = n;
                    None
                }
                Steering::Jump(off) => Some(off),
            }
        };
        if let Some(off) = jump {
            if off > 0 {
                st.feed_silence(off as usize);
            } else {
                st.skip_in = -off;
            }
            st.steer = Steer::default();
        }
        let skip = (st.skip_in as usize).min(frames);
        st.skip_in -= skip as i64;
        let (mut l, mut r): (Vec<f32>, Vec<f32>) = (skip..frames)
            .map(|i| {
                let f = &samples[i * channels..];
                (f[0], if channels > 1 { f[1] } else { f[0] })
            })
            .unzip();
        if slips != 0 {
            let over = (Steer::SLIP * rate).round() as usize;
            slip(&mut l, slips, over);
            slip(&mut r, slips, over);
        }
        st.feed(&l, &r);
        st.heard = st.out_base + st.out.len() as i64;
    }

    /// The output frame just past the newest audio that has arrived (in this
    /// feed or any it sums), if any has.
    pub(crate) fn arrived(&self) -> Option<i64> {
        let own = {
            let st = self.st.lock().unwrap();
            st.next_in.map(|_| st.heard)
        };
        self.children.lock().unwrap().iter().filter_map(|c| c.arrived()).chain(own).max()
    }

    /// `n` frames starting at output frame `from` (silence where nothing has
    /// arrived), without consuming them — for the meters, which look at audio
    /// well ahead of what's being written.
    pub(crate) fn peek(&self, from: i64, n: usize) -> Vec<[f32; 2]> {
        let mut buf = vec![[0f32; 2]; n];
        {
            let st = self.st.lock().unwrap();
            let end = from + n as i64;
            let lo = from.max(st.out_base);
            let hi = end.min(st.out_base + st.out.len() as i64);
            for idx in lo..hi {
                buf[(idx - from) as usize] = st.out[(idx - st.out_base) as usize];
            }
        }
        for child in self.children.lock().unwrap().iter() {
            add(&mut buf, &child.peek(from, n));
        }
        buf
    }

    /// Exactly `n` frames starting at output frame `from` (silence where nothing
    /// arrived), and forget everything before `from + n`.
    pub(crate) fn take(&self, from: i64, n: usize) -> Vec<[f32; 2]> {
        let mut st = self.st.lock().unwrap();
        let end = from + n as i64;
        let write = st.out_base + st.out.len() as i64;
        let mut buf = vec![[0f32; 2]; n];
        let lo = from.max(st.out_base);
        let hi = end.min(write);
        for idx in lo..hi {
            buf[(idx - from) as usize] = st.out[(idx - st.out_base) as usize];
        }
        let consumed = (end - st.out_base).clamp(0, st.out.len() as i64);
        st.out.drain(..consumed as usize);
        st.out_base += consumed;
        if st.out_base < end {
            // The mixer got ahead of this source: whatever arrives for that span
            // now is too late.
            st.skip_out += end - st.out_base;
            st.out_base = end;
        }
        drop(st);
        let mut children = self.children.lock().unwrap();
        for child in children.iter() {
            for (a, c) in buf.iter_mut().zip(child.take(from, n)) {
                a[0] += c[0];
                a[1] += c[1];
            }
        }
        // A stopped stream's feed goes once all it had is taken.
        children.retain(|c| {
            let st = c.st.lock().unwrap();
            !(st.closed && st.out.is_empty())
        });
        buf
    }
}

impl FeedState {
    fn feed_silence(&mut self, frames: usize) {
        let zeros = vec![0f32; frames];
        self.feed(&zeros, &zeros);
    }

    fn feed(&mut self, l: &[f32], r: &[f32]) {
        if let Some(next) = self.next_in.as_mut() {
            *next += l.len() as i64;
        }
        if self.resampler.is_none() {
            self.append(l.iter().zip(r).map(|(&a, &b)| [a, b]));
            return;
        }
        self.pending[0].extend_from_slice(l);
        self.pending[1].extend_from_slice(r);
        // Chunks are read from `used` on and the start cut off once at the
        // end: cutting each chunk off the front moved all the rest every
        // time, quadratic in a long run of input.
        let mut used = 0;
        loop {
            let resampler = self.resampler.as_mut().unwrap();
            let need = resampler.input_frames_next();
            if self.pending[0].len() - used < need {
                break;
            }
            let input: Vec<Vec<f32>> = self.pending.iter().map(|p| p[used..used + need].to_vec()).collect();
            used += need;
            let out_frames = resampler.output_frames_next();
            let mut output = vec![vec![0f32; out_frames]; 2];
            let done = SequentialSliceOfVecs::new(&input, 2, need)
                .ok()
                .zip(SequentialSliceOfVecs::new_mut(&mut output, 2, out_frames).ok())
                .and_then(|(i, mut o)| resampler.process_into_buffer(&i, &mut o, None).ok());
            let Some((_, written)) = done else { break };
            let frames: Vec<[f32; 2]> = (0..written).map(|i| [output[0][i], output[1][i]]).collect();
            self.append(frames.into_iter());
        }
        for p in &mut self.pending {
            p.drain(..used);
        }
    }

    fn append(&mut self, frames: impl Iterator<Item = [f32; 2]>) {
        let mut frames: Vec<[f32; 2]> = frames.collect();
        if let Some(m) = self.mono.as_mut() {
            m.process(&mut frames);
        }
        let frames = match self.denoise.as_mut() {
            Some(d) => d.process(frames.into_iter()),
            None => frames,
        };
        for f in frames {
            if self.skip_out > 0 {
                self.skip_out -= 1;
            } else {
                self.out.push_back(f);
            }
        }
    }
}

/// Keeps a source on its timestamps while its clock runs a little fast or
/// slow against the host's: a USB mic, or an HDMI or Bluetooth output, is
/// often 10-100 ppm off. Sound is put where counting its frames says, so
/// that error grew for as long as it played, and was put right only at
/// [`SourceFeed::TOLERANCE`]: 40 ms of silence at once (a slow clock) or a
/// packet thrown away (fast). At 50 ppm that's a gap every 13 minutes,
/// with the sound up to 40 ms early or late in between.
///
/// Now, from 2 ms off on, single frames are put in (slow) or taken out
/// (fast) as it comes: as often as the drift needs, never more than one in
/// 5000 frames (up to 200 ppm is followed: ~10 a second at 48 kHz), each
/// spread over a millisecond (see [`slip`]). At 100 ppm, a frame every
/// 10,000 and the sound ~3 ms off its timestamps for good.
///
/// Each frame slid moves the sound by a frame within a millisecond; many a
/// second could make a long high note sound rough. So they're kept for
/// gradual drift, and an offset that comes all at once (a dropout under
/// [`SourceFeed::TOLERANCE`], timestamps that jump) is put right at once,
/// silence in or the overlap thrown away, once it has been over
/// [`Self::SUDDEN`] for [`Self::SURE`]: steered, 20 ms took 20 s of 48
/// slips a second.
#[derive(Debug, Default)]
struct Steer {
    /// How far its sound is put from where its timestamps say, in input
    /// frames (+: earlier, as a slow clock's is), averaged over about a
    /// second: timestamps jitter by a millisecond or so.
    drift: f64,
    /// Frames owed: one is put in (+) or taken out (-) each time it gets to 1.
    owed: f64,
    /// The packets in a row that came over [`Self::SUDDEN`] off, one way.
    run: Option<Run>,
}

/// Packets in a row that came a sudden offset off, one way.
#[derive(Debug, Clone, Copy)]
struct Run {
    frames: usize,
    packets: u32,
    /// The least any of them was off (input frames, signed).
    least: i64,
}

/// What [`Steer::step`] does with a packet.
#[derive(Debug, PartialEq)]
enum Steering {
    /// Put this many frames into it (+) or take them out (-), one by one.
    Slip(i64),
    /// It's this far off (input frames; +: early), and has been for a
    /// while: put right at once.
    Jump(i64),
}

impl Steer {
    /// Off by less than this (seconds), it's left alone: that's the jitter.
    const FROM: f64 = 0.002;
    /// From this far off on, it's steered as hard as it gets.
    const FULL: f64 = 0.004;
    /// At most one frame in this many put in or taken out.
    const EVERY: f64 = 5000.0;
    /// How long its drift is averaged over (seconds).
    const SMOOTH: f64 = 1.0;
    /// How long a frame put in or taken out is spread over (seconds).
    const SLIP: f64 = 0.001;
    /// An offset over this (seconds) that came all at once is put right at
    /// once, not steered off...
    const SUDDEN: f64 = 0.01;
    /// ...once it has held for this long (seconds), and three packets: a
    /// packet whose timestamp is off alone isn't one.
    const SURE: f64 = 0.05;

    /// A packet of `frames` frames (at `rate`) that came `drift` frames off
    /// where counting puts it.
    fn step(&mut self, drift: i64, frames: usize, rate: f64) -> Steering {
        if drift.abs() as f64 > Self::SUDDEN * rate {
            let run = self.run.filter(|r| r.least.signum() == drift.signum()).unwrap_or(Run { frames: 0, packets: 0, least: drift });
            let run = Run {
                frames: run.frames + frames,
                packets: run.packets + 1,
                least: if drift.abs() < run.least.abs() { drift } else { run.least },
            };
            if run.packets >= 3 && run.frames as f64 >= Self::SURE * rate {
                *self = Self::default();
                return Steering::Jump(run.least);
            }
            // Not steered meanwhile: it's about to be put right.
            self.run = Some(run);
            return Steering::Slip(0);
        }
        self.run = None;
        let weight = 1.0 - (-(frames as f64 / rate) / Self::SMOOTH).exp();
        self.drift += (drift as f64 - self.drift) * weight;
        let pull = ((self.drift.abs() / rate - Self::FROM) / (Self::FULL - Self::FROM)).clamp(0.0, 1.0);
        let towards = self.drift.signum() * pull;
        if towards == 0.0 || towards * self.owed < 0.0 {
            self.owed = 0.0;
        }
        self.owed += towards * frames as f64 / Self::EVERY;
        let now = self.owed.trunc();
        self.owed -= now;
        Steering::Slip(now as i64)
    }
}

/// Put `n` frames into `x` (or take `-n` out), spread evenly through it.
/// A frame repeated or dropped outright jumps (a click, on a high note):
/// instead the `over` frames around it are read a little slower (or
/// faster), each taken between two neighbours, so the sound slides by one
/// frame over them.
fn slip(x: &mut Vec<f32>, n: i64, over: usize) {
    let len = x.len();
    if len < 3 {
        return;
    }
    let w = over.clamp(1, len - 2);
    let count = n.unsigned_abs() as usize;
    let step = 1.0 / (w + 1) as f32;
    // From the last to the first, so each leaves the places of those before it.
    for k in (0..count).rev() {
        let centre = (2 * k + 1) * len / (2 * count);
        let at = centre.saturating_sub(w / 2).clamp(1, len - w - 1);
        if n > 0 {
            // Read back towards the frame before, one more frame out.
            let slid: Vec<f32> = (0..w).map(|i| x[at + i] + (x[at + i - 1] - x[at + i]) * (i + 1) as f32 * step).collect();
            x.splice(at..at + w - 1, slid);
        } else {
            // Read on towards the frame after, one frame less.
            let slid: Vec<f32> = (0..w).map(|i| x[at + i] + (x[at + i + 1] - x[at + i]) * (i + 1) as f32 * step).collect();
            x.splice(at..at + w + 1, slid);
        }
    }
}

/// A feed's two sides made one and played on both, while `on` is set: for a
/// microphone heard on one side only, as one in an audio interface's first
/// input is (its left). It comes out as loud as the louder side: the sides'
/// sum, halved when they're alike (a mic that gives both the same stays as
/// it was), whole when one side is all there is. How alike they are is taken
/// from the last couple of seconds, the loud parts counting most, so it's
/// right from the first word. Kept up while off too, so it's right the
/// moment it's turned on; a switch fades over 10 ms.
pub(crate) struct Mono {
    on: Arc<AtomicBool>,
    /// Energy of the left, the right and their sum, fading over [`Self::MEMORY`].
    energy: [f32; 3],
    /// What the sum is multiplied by: ½ (alike) to 1 (one side).
    scale: f32,
    /// How far it's made one: 0 (as it came) to 1, moving across a switch.
    amount: f32,
}

impl Mono {
    /// Seconds of sound the sides' likeness is taken from.
    const MEMORY: f32 = 2.0;
    /// Frames a switch fades over: 10 ms.
    const FADE: f32 = (RATE / 100) as f32;

    pub(crate) fn new(on: Arc<AtomicBool>) -> Self {
        Self { on, energy: [0.0; 3], scale: 0.5, amount: 0.0 }
    }

    pub(crate) fn process(&mut self, frames: &mut [[f32; 2]]) {
        if frames.is_empty() {
            return;
        }
        let fade = (-(frames.len() as f32) / (RATE as f32 * Self::MEMORY)).exp();
        let mut block = [0f32; 3];
        for f in frames.iter() {
            let sum = f[0] + f[1];
            block[0] += f[0] * f[0];
            block[1] += f[1] * f[1];
            block[2] += sum * sum;
        }
        for (e, b) in self.energy.iter_mut().zip(block) {
            *e = *e * fade + b;
        }
        // Alike, the sum is twice either side (½); one side alone, it's that
        // side (1). Never more: sides that cancel would make a quiet sum loud.
        let [left, right, sum] = self.energy;
        let scale = if sum > 1e-9 { (left.max(right) / sum).sqrt().clamp(0.5, 1.0) } else { self.scale };
        let on = self.on.load(Ordering::Relaxed);
        let from = std::mem::replace(&mut self.scale, scale);
        if !on && self.amount == 0.0 {
            return;
        }
        let step = if on { 1.0 / Self::FADE } else { -1.0 / Self::FADE };
        let n = frames.len() as f32;
        for (i, f) in frames.iter_mut().enumerate() {
            self.amount = (self.amount + step).clamp(0.0, 1.0);
            let one = (f[0] + f[1]) * (from + (scale - from) * (i + 1) as f32 / n);
            *f = [f[0] + (one - f[0]) * self.amount, f[1] + (one - f[1]) * self.amount];
        }
    }
}

/// Noise removal on a feed's 48 kHz sound, in 10 ms frames: both sides are
/// made one (a microphone is mono) and cleaned together, by RNNoise (Strong)
/// and by taking out steady noise (Light, [`crate::steady`]). Both run all
/// the time, so each knows the noise the moment it's chosen, and both hand
/// each frame back one frame later: the sound always comes out a frame late,
/// whatever's chosen, and a switch never shifts it. Across a switch it fades
/// from one to the other over a frame.
pub(crate) struct Denoise {
    mode: Arc<AtomicU8>,
    state: Box<nnnoiseless::DenoiseState<'static>>,
    steady: crate::steady::Steady,
    /// Waiting for a full frame.
    pending: Vec<[f32; 2]>,
    /// The previous frame as it came: what the cleaned ones line up with.
    dry: Vec<[f32; 2]>,
    /// What the last frame was.
    was: NoiseReduction,
}

impl Denoise {
    pub(crate) const FRAME: usize = nnnoiseless::DenoiseState::FRAME_SIZE;
    /// How late the sound comes out, in frames of sound.
    const DELAY: usize = Self::FRAME;

    pub(crate) fn new(mode: Arc<AtomicU8>) -> Self {
        Self {
            mode,
            state: nnnoiseless::DenoiseState::new(),
            steady: crate::steady::Steady::new(),
            pending: Vec::with_capacity(Self::FRAME * 2),
            dry: vec![[0.0; 2]; Self::FRAME],
            was: NoiseReduction::Off,
        }
    }

    /// Mono sound in, mono out (a frame late).
    pub(crate) fn process_mono(&mut self, samples: &[f32]) -> Vec<f32> {
        self.process(samples.iter().map(|&s| [s, s])).into_iter().map(|f| f[0]).collect()
    }

    fn process(&mut self, frames: impl Iterator<Item = [f32; 2]>) -> Vec<[f32; 2]> {
        self.pending.extend(frames);
        let whole = self.pending.len() / Self::FRAME * Self::FRAME;
        let mut out = Vec::with_capacity(whole);
        let mut mono = [0f32; Self::FRAME];
        let mut input = [0f32; Self::FRAME];
        let mut strong = [0f32; Self::FRAME];
        let mut light = [0f32; Self::FRAME];
        for chunk in self.pending[..whole].chunks_exact(Self::FRAME) {
            for ((m, i), f) in mono.iter_mut().zip(&mut input).zip(chunk) {
                *m = (f[0] + f[1]) * 0.5;
                *i = *m * 32768.0;
            }
            self.state.process_frame(&mut strong, &input);
            self.steady.process(&mono, &mut light);
            let mode = NoiseReduction::from_u8(self.mode.load(Ordering::Relaxed));
            if mode == NoiseReduction::Off && self.was == NoiseReduction::Off {
                // Off and staying off: the sound exactly as it came.
                out.extend_from_slice(&self.dry);
            } else {
                let (from, to) = (self.was.weights(), mode.weights());
                for (k, f) in self.dry.iter().enumerate() {
                    let t = (k + 1) as f32 / Self::FRAME as f32;
                    let w: [f32; 3] = std::array::from_fn(|i| from[i] + (to[i] - from[i]) * t);
                    let cleaned = light[k] * w[1] + (strong[k] / 32768.0).clamp(-1.0, 1.0) * w[2];
                    out.push([f[0] * w[0] + cleaned, f[1] * w[0] + cleaned]);
                }
            }
            self.dry.copy_from_slice(chunk);
            self.was = mode;
        }
        self.pending.drain(..whole);
        out
    }
}

/// A source as the mixer sees it.
pub(crate) struct MixInput {
    /// Its source's id: what a [`MixChange`] names it by.
    pub id: String,
    pub feed: Arc<SourceFeed>,
    pub channel: Arc<Channel>,
    pub in_mix: bool,
    /// Its own track, if it has one.
    pub track: Option<Sender<Vec<f32>>>,
}

/// A change to the sources of a mixer that's running (see
/// [`spawn_changing_mixer`]). The tracks stay as they are: a file's are set
/// once it's started.
#[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
pub(crate) enum MixChange {
    /// Mix this source in from now on. One taken out before (by its id)
    /// gets its own track back, if it had one. Any other has none: in the
    /// mix, it goes into the rest track, with the others that have none.
    Add(MixInput),
    /// Take out the source with this id: the sound it has sent is mixed,
    /// then it goes, and its own track, if it has one, goes on silent.
    Remove(String),
}

/// How far behind real time the meters run: just enough for the sources'
/// newest audio to have arrived. A source whose audio comes later (a device
/// that buffers a lot: Bluetooth, or any in a virtual machine on Linux,
/// where timestamps include the device's buffering) holds them back until it
/// has, so it doesn't look silent.
const METER_LAG: f64 = 0.04;

/// A source whose newest audio is older than this isn't delivering (an app
/// that went quiet, a mic unplugged): the meters don't wait for it.
const STALLED: f64 = 0.5;

/// Mix sources into tracks until `stop` is set: every 10 ms, take the span that's
/// `latency` seconds behind now from every feed, apply volume, sum the mix
/// (through the limiter) and send each track as interleaved 48 kHz stereo. On
/// stop it flushes up to the current time, then drops the senders.
///
/// The meters don't wait for that: `latency` is there so late audio still
/// makes it into the file (up to 300 ms while recording), which made meters
/// lag just as far. Each tick they look ahead at audio only [`METER_LAG`] old,
/// which the feeds still hold, and run the same volume, mix and limiter on it.
///
/// `rest_track` gets the sum of sources that are in the mix without a track of
/// their own, so the separate tracks always add up to the mix (the editor
/// rebuilds it from them).
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_mixer(
    inputs: Vec<MixInput>,
    mix_track: Option<Sender<Vec<f32>>>,
    rest_track: Option<Sender<Vec<f32>>>,
    live: Arc<LiveAudio>,
    clock: Arc<Clock>,
    now: fn() -> f64,
    latency: f64,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    // Nothing sends it changes: its sources stay as they are.
    let (_, changes) = std::sync::mpsc::channel();
    spawn_changing_mixer(inputs, changes, mix_track, rest_track, live, clock, now, latency, stop)
}

/// [`spawn_mixer`], with its sources changed while it runs by what `changes`
/// brings: one added, one taken out (see [`MixChange`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_changing_mixer(
    inputs: Vec<MixInput>,
    changes: Receiver<MixChange>,
    mix_track: Option<Sender<Vec<f32>>>,
    rest_track: Option<Sender<Vec<f32>>>,
    live: Arc<LiveAudio>,
    clock: Arc<Clock>,
    now: fn() -> f64,
    latency: f64,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut mixer = Mixing::new(inputs, mix_track, rest_track, live.clone(), latency);
        loop {
            let tick = Instant::now();
            let stopping = stop.load(Ordering::Relaxed);
            while let Ok(change) = changes.try_recv() {
                mixer.change(change);
            }
            if let Some(t0) = clock.get() {
                mixer.meter(now() - t0, stopping);
                mixer.mix(now() - t0, stopping);
            }
            if stopping {
                break;
            }
            if tick.elapsed() > Duration::from_millis(500) {
                eprintln!("mixer: one pass took {:.2} s (a source's audio held it up)", tick.elapsed().as_secs_f32());
            }
            thread::sleep(Duration::from_millis(10));
        }
        live.all_off();
    })
}

/// A running mixer: its sources and tracks, and how far it has got, in
/// mixing them and in metering them.
struct Mixing {
    inputs: Vec<Input>,
    /// The own tracks of sources taken out, by the source's id. They go on,
    /// silent (every track runs to the file's end), and go back to the
    /// source if it's added again.
    parked: Vec<(String, Sender<Vec<f32>>)>,
    mix_track: Option<Sender<Vec<f32>>>,
    rest_track: Option<Sender<Vec<f32>>>,
    live: Arc<LiveAudio>,
    latency: f64,
    limiter: Limiter,
    /// The next frame to mix, once mixing has begun.
    mixed: Option<i64>,
    /// The meters' own position and limiter, ahead of the mix.
    metered: Option<i64>,
    meter_limiter: Limiter,
    /// When the meters last moved, and whether that's been said: they
    /// stood still now and then, until clipping was restarted; this says
    /// where they were stuck, in the log.
    moved: Instant,
    said_stuck: bool,
}

/// One of a mixer's sources.
struct Input {
    mix: MixInput,
    /// The gain its last block ended at, which the next one ramps from.
    gain: f32,
    /// Taken out: mixed up to where its sound ends, then let go of.
    leaving: bool,
}

impl Input {
    fn new(mix: MixInput) -> Self {
        Self { gain: mix.channel.effective_gain(), mix, leaving: false }
    }
}

impl Mixing {
    fn new(inputs: Vec<MixInput>, mix_track: Option<Sender<Vec<f32>>>, rest_track: Option<Sender<Vec<f32>>>, live: Arc<LiveAudio>, latency: f64) -> Self {
        Self {
            inputs: inputs.into_iter().map(Input::new).collect(),
            parked: Vec::new(),
            mix_track,
            rest_track,
            live,
            latency,
            limiter: Limiter::new(),
            mixed: None,
            metered: None,
            meter_limiter: Limiter::new(),
            moved: Instant::now(),
            said_stuck: false,
        }
    }

    fn change(&mut self, change: MixChange) {
        match change {
            MixChange::Add(mut input) => {
                // Its own track, if it was here before: from it still
                // leaving (which goes now, the last of its sound with it),
                // or from it gone.
                let mut tracks = Vec::new();
                if let Some(at) = self.inputs.iter().position(|i| i.mix.id == input.id) {
                    tracks.extend(self.inputs.remove(at).mix.track);
                }
                if let Some(at) = self.parked.iter().position(|(id, _)| *id == input.id) {
                    tracks.push(self.parked.remove(at).1);
                }
                for tx in tracks {
                    if input.track.is_none() {
                        input.track = Some(tx);
                    } else {
                        // A source has one track of its own at most, but a
                        // track is never let go of before the file ends.
                        self.parked.push((input.id.clone(), tx));
                    }
                }
                self.inputs.push(Input::new(input));
            }
            MixChange::Remove(id) => {
                for input in self.inputs.iter_mut().filter(|i| i.mix.id == id) {
                    input.leaving = true;
                }
            }
        }
    }

    /// Meter up to `at` (seconds on the clock) less [`METER_LAG`], or as far
    /// as the sources' sound has come, if that's less. Sources taken out
    /// aren't: their sound is only mixed, to its end.
    fn meter(&mut self, at: f64, stopping: bool) {
        let mut target = ((at - METER_LAG) * RATE as f64).floor() as i64;
        let stalled = target - (STALLED * RATE as f64) as i64;
        if let Some(slowest) = self.inputs.iter().filter(|i| !i.leaving).filter_map(|i| i.mix.feed.arrived()).filter(|&a| a > stalled).min() {
            target = target.min(slowest);
        }
        let from = self.metered.get_or_insert(target.max(0));
        // Never meter what's already been written (and so is gone).
        *from = (*from).max(self.mixed.unwrap_or(0));
        while *from < target {
            let n = ((target - *from) as usize).min(RATE as usize / 50);
            let mut mix = vec![[0f32; 2]; n];
            for input in self.inputs.iter().filter(|i| !i.leaving).map(|i| &i.mix) {
                let mut block = input.feed.peek(*from, n);
                input.channel.input.record(&block);
                input.channel.send_tap(&block);
                let g = input.channel.effective_gain();
                for f in block.iter_mut() {
                    f[0] *= g;
                    f[1] *= g;
                }
                input.channel.meter.record(&block);
                if input.in_mix {
                    add(&mut mix, &block);
                }
            }
            if self.live.limiter.load(Ordering::Relaxed) {
                let reduction = self.meter_limiter.process(&mut mix);
                self.live.reduction_db.fetch_max(reduction);
            }
            self.live.master.record(&mix);
            *from += n as i64;
            self.moved = Instant::now();
        }
        if !stopping && self.moved.elapsed() > Duration::from_secs(2) {
            if !self.said_stuck {
                self.said_stuck = true;
                let arrived: Vec<Option<i64>> = self.inputs.iter().map(|i| i.mix.feed.arrived()).collect();
                eprintln!(
                    "meters: stuck for {:.1} s at frame {}, aiming at {target} ({at:.3} s since the start), mixed to {:?}; newest audio per source: {arrived:?}",
                    self.moved.elapsed().as_secs_f32(),
                    self.metered.unwrap_or(0),
                    self.mixed,
                );
            }
        } else if self.said_stuck && self.moved.elapsed() < Duration::from_secs(1) {
            self.said_stuck = false;
            eprintln!("meters: moving again");
        }
    }

    /// Mix up to `latency` behind `at` (seconds on the clock; all of it,
    /// `stopping`), and send every track its part.
    fn mix(&mut self, at: f64, stopping: bool) {
        let behind = if stopping { 0.0 } else { self.latency };
        let target = ((at - behind) * RATE as f64).floor() as i64;
        let from = self.mixed.get_or_insert(0);
        while *from < target {
            let n = ((target - *from) as usize).min(RATE as usize / 50);
            let mut mix = vec![[0f32; 2]; n];
            let mut rest = vec![[0f32; 2]; if self.rest_track.is_some() { n } else { 0 }];
            for input in &mut self.inputs {
                let mut block = input.mix.feed.take(*from, n);
                // Ramp to the new volume across the block so fader moves don't click.
                let target_gain = input.mix.channel.effective_gain();
                for (k, f) in block.iter_mut().enumerate() {
                    let g = input.gain + (target_gain - input.gain) * (k + 1) as f32 / n as f32;
                    f[0] *= g;
                    f[1] *= g;
                }
                input.gain = target_gain;
                if input.mix.in_mix {
                    add(&mut mix, &block);
                    if input.mix.track.is_none() {
                        add(&mut rest, &block);
                    }
                }
                if let Some(tx) = &input.mix.track {
                    let _ = tx.send(interleave(&block));
                }
            }
            for (_, tx) in &self.parked {
                let _ = tx.send(vec![0.0; n * 2]);
            }
            if self.live.limiter.load(Ordering::Relaxed) {
                self.limiter.process(&mut mix);
            }
            if let Some(tx) = &self.mix_track {
                let _ = tx.send(interleave(&mix));
            }
            if let Some(tx) = &self.rest_track {
                let _ = tx.send(interleave(&rest));
            }
            *from += n as i64;
        }
        // Those taken out go once all their sound is mixed: their own
        // tracks go on, silent.
        let mixed = *from;
        let parked = &mut self.parked;
        self.inputs.retain_mut(|input| {
            let gone = input.leaving && input.mix.feed.arrived().is_none_or(|a| a <= mixed);
            if gone && let Some(tx) = input.mix.track.take() {
                parked.push((input.mix.id.clone(), tx));
            }
            !gone
        });
    }
}

fn add(acc: &mut [[f32; 2]], block: &[[f32; 2]]) {
    for (a, f) in acc.iter_mut().zip(block) {
        a[0] += f[0];
        a[1] += f[1];
    }
}

fn interleave(block: &[[f32; 2]]) -> Vec<f32> {
    block.iter().flat_map(|f| *f).collect()
}

/// Peak limiter: instant attack (nothing ever exceeds the ceiling), smooth release.
struct Limiter {
    gain: f32,
    release: f32,
}

impl Limiter {
    fn new() -> Self {
        // ~150 ms back to unity after a peak.
        Self { gain: 1.0, release: 1.0 - (-1.0 / (0.15 * RATE as f32)).exp() }
    }

    /// Limit in place; returns the largest gain reduction applied, in dB.
    fn process(&mut self, block: &mut [[f32; 2]]) -> f32 {
        let mut min_gain = 1f32;
        for f in block.iter_mut() {
            let peak = f[0].abs().max(f[1].abs());
            let allowed = if peak > CEILING { CEILING / peak } else { 1.0 };
            self.gain = (self.gain + (1.0 - self.gain) * self.release).min(allowed);
            f[0] *= self.gain;
            f[1] *= self.gain;
            min_gain = min_gain.min(self.gain);
        }
        -20.0 * min_gain.log10()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock_at_zero() -> Arc<Clock> {
        let c = Arc::new(Clock::default());
        c.set(1000.0);
        c
    }

    /// Hiss under a voice that comes and goes: the hiss between words much
    /// quieter, Light or Strong, and the sound a frame late, on or off.
    #[test]
    fn denoise_removes_noise_and_keeps_timing() {
        let mut seed = 1u32;
        let mut noise = || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
        };
        // 4 s: a voice-like buzz (150 Hz and its harmonics) for half of each
        // second, over hiss throughout.
        let input: Vec<[f32; 2]> = (0..192_000)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                let voiced = t.fract() < 0.5;
                let buzz: f32 = if voiced { (1..15).map(|h| (std::f32::consts::TAU * 150.0 * h as f32 * t).sin() / h as f32).sum::<f32>() * 0.1 } else { 0.0 };
                let v = buzz + noise() * 0.02;
                [v, v]
            })
            .collect();
        let rms = |s: &[[f32; 2]]| (s.iter().map(|f| f[0] * f[0]).sum::<f32>() / s.len() as f32).sqrt();
        // A gap between words (well inside it), and a word, in the last second.
        let gap = 3 * 48_000 + 30_000..3 * 48_000 + 46_000;
        let word = 3 * 48_000 + 4_000..3 * 48_000 + 20_000;
        // Light only turns the hiss down (to `steady::FLOOR`), Strong takes it out.
        for (mode, hiss_left) in [(NoiseReduction::Light, 0.3), (NoiseReduction::Strong, 0.2)] {
            let mut d = Denoise::new(Arc::new(AtomicU8::new(mode as u8)));
            let out = d.process(input.iter().copied());
            assert_eq!(out.len(), input.len());
            let (before, after) = (rms(&input[gap.clone()]), rms(&out[gap.clone()]));
            assert!(after < before * hiss_left, "{mode:?}: hiss between words {before} -> {after}");
            let (before, after) = (rms(&input[word.clone()]), rms(&out[word.clone()]));
            assert!(after > before * 0.5, "{mode:?}: voice {before} -> {after}");
        }

        // Off: the sound as it came, a frame late.
        let mut d = Denoise::new(Arc::new(AtomicU8::new(NoiseReduction::Off as u8)));
        let out = d.process(input.iter().copied());
        assert_eq!(out[Denoise::DELAY..], input[..input.len() - Denoise::DELAY]);
    }

    /// A recording through each noise reduction, to listen to: with
    /// `DENOISE_IN=<raw f32 stereo, 48 kHz>`, `cargo test --release -p capture
    /// -- --ignored denoise_file` writes `<it>.light` and `<it>.strong`, the same.
    #[test]
    #[ignore]
    fn denoise_file() {
        let path = std::env::var("DENOISE_IN").expect("DENOISE_IN");
        let input: Vec<[f32; 2]> = std::fs::read(&path)
            .unwrap()
            .chunks_exact(8)
            .map(|b| [f32::from_le_bytes(b[..4].try_into().unwrap()), f32::from_le_bytes(b[4..].try_into().unwrap())])
            .collect();
        for (mode, name) in [(NoiseReduction::Light, "light"), (NoiseReduction::Strong, "strong")] {
            let out = Denoise::new(Arc::new(AtomicU8::new(mode as u8))).process(input.iter().copied());
            std::fs::write(format!("{path}.{name}"), out.iter().flatten().flat_map(|s| s.to_le_bytes()).collect::<Vec<u8>>()).unwrap();
        }
    }

    #[test]
    fn denoised_feed_stays_on_time() {
        let feed = SourceFeed::new(RATE, clock_at_zero());
        feed.denoise_with(Arc::new(AtomicU8::new(NoiseReduction::Off as u8)));
        // 30 ms of ones, captured 20 ms after t0: still at 20 ms, though it
        // comes out of the denoiser a frame late.
        feed.push(1000.02, &vec![1.0; 1440 * 2], 2);
        feed.push(1000.05, &vec![0.0; 960 * 2], 2);
        let out = feed.take(0, 3840);
        assert!(out[..960].iter().all(|f| f[0] == 0.0));
        assert!(out[960..2400].iter().all(|f| f[0] == 1.0), "{:?}", &out[950..970]);
        assert!(out[2400..2880].iter().all(|f| f[0] == 0.0));
    }

    /// A voice in 10 ms blocks, as a mic delivers it: `side` says how much of
    /// it each side gets.
    fn voice_blocks(side: [f32; 2], seconds: f32) -> Vec<Vec<[f32; 2]>> {
        let frames: Vec<[f32; 2]> = (0..(RATE as f32 * seconds) as usize)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                let v = (std::f32::consts::TAU * 180.0 * t).sin() * 0.3 + (std::f32::consts::TAU * 410.0 * t).sin() * 0.1;
                [v * side[0], v * side[1]]
            })
            .collect();
        frames.chunks(480).map(<[_]>::to_vec).collect()
    }

    /// A mic heard on the left only: on both sides, as loud as the left was,
    /// from the first word.
    #[test]
    fn mono_puts_one_side_on_both_as_loud() {
        let mut m = Mono::new(Arc::new(AtomicBool::new(true)));
        let rms = |s: &[[f32; 2]], c: usize| (s.iter().map(|f| f[c] * f[c]).sum::<f32>() / s.len() as f32).sqrt();
        for (i, mut block) in voice_blocks([1.0, 0.0], 1.0).into_iter().enumerate() {
            let was = rms(&block, 0);
            m.process(&mut block);
            // After the fade in (10 ms) and the first block's ramp.
            if i >= 2 {
                assert!(block.iter().all(|f| (f[0] - f[1]).abs() < 1e-6));
                assert!((rms(&block, 0) / was - 1.0).abs() < 0.01, "block {i}: {was} -> {}", rms(&block, 0));
            }
        }
    }

    /// A mic that gives both sides the same stays as it was.
    #[test]
    fn mono_leaves_alike_sides_alone() {
        let mut m = Mono::new(Arc::new(AtomicBool::new(true)));
        for block in voice_blocks([1.0, 1.0], 1.0) {
            let mut out = block.clone();
            m.process(&mut out);
            for (a, b) in out.iter().zip(&block) {
                assert!((a[0] - b[0]).abs() < 1e-5 && (a[1] - b[1]).abs() < 1e-5);
            }
        }
    }

    /// Off, the sound exactly as it came; turned on, the right side comes in
    /// over 10 ms, not with a click.
    #[test]
    fn mono_off_is_untouched_and_switching_fades() {
        let on = Arc::new(AtomicBool::new(false));
        let mut m = Mono::new(on.clone());
        let blocks = voice_blocks([1.0, 0.0], 0.5);
        for block in &blocks[..20] {
            let mut out = block.clone();
            m.process(&mut out);
            assert_eq!(&out, block);
        }
        on.store(true, Ordering::Relaxed);
        let mut out = blocks[20].clone();
        m.process(&mut out);
        let steps: Vec<f32> = out.windows(2).map(|w| (w[1][1] - w[0][1]).abs()).collect();
        // The voice moves at most ~0.03 a frame; a jump to it would be ~0.3.
        assert!(steps.iter().all(|&d| d < 0.05), "{:?}", steps.iter().cloned().fold(0.0, f32::max));
        assert!((out[479][1] - out[479][0]).abs() < 1e-4);
    }

    #[test]
    fn feed_places_audio_by_timestamp() {
        let feed = SourceFeed::new(RATE, clock_at_zero());
        // 10 ms of ones, captured 20 ms after t0.
        feed.push(1000.02, &vec![1.0; 480 * 2], 2);
        let out = feed.take(0, 1920);
        assert!(out[..960].iter().all(|f| f[0] == 0.0));
        assert!(out[960..1440].iter().all(|f| f[0] == 1.0));
        assert!(out[1440..].iter().all(|f| f[0] == 0.0));
    }

    #[test]
    fn late_audio_is_dropped_not_shifted() {
        let feed = SourceFeed::new(RATE, clock_at_zero());
        let _ = feed.take(0, 480); // mixer already past the first 10 ms
        feed.push(1000.0, &vec![1.0; 960], 1); // 20 ms starting at 0
        let out = feed.take(480, 480);
        assert!(out.iter().all(|f| f[0] == 1.0 && f[1] == 1.0));
        assert!(feed.take(960, 10).iter().all(|f| f[0] == 0.0));
    }

    #[test]
    fn resampled_feed_keeps_its_place() {
        let feed = SourceFeed::new(44_100, clock_at_zero());
        // One second of a constant, starting at t0.
        feed.push(1000.0, &vec![0.5; 44_100], 1);
        let out = feed.take(0, 40_000);
        let mid = &out[2000..38_000];
        assert!(mid.iter().all(|f| (f[0] - 0.5).abs() < 0.01), "resampled level holds");
    }

    /// A 384 kHz output (the player's, on a card that runs that fast) that
    /// stops for a minute and starts again: placed at its new time, fast (the
    /// gap was filled with silence through the resampler: tens of seconds).
    #[test]
    fn long_gap_starts_the_feed_over() {
        let feed = SourceFeed::new(384_000, clock_at_zero());
        feed.push(1000.0, &vec![0.5; 38_400 * 2], 2); // 100 ms
        let _ = feed.take(0, 4_800);
        let t = std::time::Instant::now();
        feed.push(1060.0, &vec![0.5; 384_000 * 2], 2); // 1 s, a minute later
        assert!(t.elapsed() < std::time::Duration::from_millis(500), "took {:?}", t.elapsed());
        let at = 60 * RATE as i64;
        // Silence in the gap, up to its time; then the sound.
        assert!(feed.take(4_800, 4_800).iter().all(|f| f[0] == 0.0));
        assert!(feed.take(at - 4_800, 4_800).iter().all(|f| f[0] == 0.0), "nothing before its time");
        let out = feed.take(at, RATE as usize);
        assert!(out[4_000..44_000].iter().all(|f| (f[0] - 0.5).abs() < 0.01), "its level, in place");
    }

    /// A source whose clock runs 100 ppm fast or slow (a USB mic, an HDMI
    /// output) for 10 minutes, its timestamps jittering by up to a
    /// millisecond: it stays within a few ms of where they put it, without a
    /// gap filled or a packet thrown away, steered one frame at a time, all
    /// one way, at most one in 5000 frames. At its right rate, the jitter
    /// alone steers nothing.
    #[test]
    fn drifting_clock_is_steered_a_frame_at_a_time() {
        for (rate, ppm) in [(RATE, 100.0), (RATE, -100.0), (RATE, 0.0), (44_100, 100.0), (44_100, -100.0)] {
            let feed = SourceFeed::new(rate, clock_at_zero());
            let packet = rate as usize / 100;
            let true_rate = rate as f64 * (1.0 + ppm * 1e-6);
            let mut seed = 7u32;
            let mut jitter = move || {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                ((seed >> 8) as f64 / (1u32 << 24) as f64 * 2.0 - 1.0) * 0.001
            };
            // A click every 100 ms of its own time, on a steady level.
            let every = packet * 10;
            let level = |frame: usize| if frame % every == every / 2 { 1.0f32 } else { 0.25 };
            let packets = 10 * 60 * 100;
            let mut worst = 0f64;
            let mut slips: Vec<(usize, i64)> = Vec::new();
            let (mut taken, mut silent) = (0i64, 0usize);
            let mut clicks: Vec<(i64, f32)> = Vec::new();
            for k in 0..packets {
                let first = k * packet;
                let captured = 1000.05 + first as f64 / true_rate;
                let samples: Vec<f32> = (first..first + packet).map(level).collect();
                let before = feed.st.lock().unwrap().next_in;
                if let Some(next) = before {
                    // Where it's put, against where it was captured.
                    worst = worst.max(((captured - 1000.0) * rate as f64 - next as f64).abs() / rate as f64);
                }
                feed.push(captured + jitter(), &samples, 1);
                let after = feed.st.lock().unwrap().next_in.unwrap();
                if let Some(before) = before
                    && after - before != packet as i64
                {
                    slips.push((k, after - before - packet as i64));
                }
                // The mixer, 100 ms behind. At 48 kHz, where each click lands.
                let upto = ((captured - 1000.1) * RATE as f64) as i64;
                if upto > taken {
                    for (i, f) in feed.take(taken, (upto - taken) as usize).into_iter().enumerate() {
                        let at = taken + i as i64;
                        if rate != RATE {
                            continue;
                        }
                        if at > (0.06 * RATE as f64) as i64 && f[0] < 0.2 {
                            silent += 1;
                        }
                        // A click slid over is split between two frames.
                        if f[0] > 0.6 {
                            match clicks.last_mut() {
                                Some(c) if at - c.0 <= 2 => {
                                    if f[0] > c.1 {
                                        *c = (at, f[0]);
                                    }
                                }
                                _ => clicks.push((at, f[0])),
                            }
                        }
                    }
                    taken = upto;
                }
            }
            let what = format!("{rate} Hz at {ppm:+} ppm");
            assert!(worst < 0.004, "{what}: up to {:.2} ms off", worst * 1e3);
            let total: i64 = slips.iter().map(|s| s.1).sum();
            eprintln!("{what}: {} frames steered, up to {:.2} ms off", total, worst * 1e3);
            if ppm == 0.0 {
                assert!(slips.is_empty(), "{what}: steered by jitter: {slips:?}");
                continue;
            }
            // Fast: frames taken out. All of the 10 minutes' drift (60 ms)
            // but the couple of ms it's let be off.
            let drift = -ppm * 1e-6 * 600.0 * rate as f64;
            assert!(slips.iter().all(|s| s.1.signum() == drift.signum() as i64), "{what}: steered both ways");
            assert!(total as f64 / drift > 0.9 && total as f64 / drift < 1.0, "{what}: steered {total} of {drift:.0}");
            assert!(slips.iter().all(|s| s.1.abs() == 1), "{what}: more than a frame at once");
            let closest = slips.windows(2).map(|w| (w[1].0 - w[0].0) * packet).min().unwrap();
            assert!(closest >= 4_800, "{what}: two frames {closest} apart");
            if rate == RATE {
                assert_eq!(silent, 0, "{what}: silence put in");
                // Every click there, within 4 ms of where it was captured.
                let expected = (taken as f64 / RATE as f64 - 0.05) * true_rate / every as f64;
                assert!((clicks.len() as f64 - expected).abs() < 2.0, "{what}: {} clicks of {expected:.0}", clicks.len());
                for (m, (at, _)) in clicks.iter().enumerate() {
                    let captured = 0.05 + (m * every + every / 2) as f64 / true_rate;
                    let off = (*at as f64 / RATE as f64 - captured).abs();
                    assert!(off < 0.004, "{what}: click {m} {:.2} ms off", off * 1e3);
                }
            }
        }
    }

    /// A dropout of 20 ms (under the 40 ms put right at once even for one
    /// packet), or timestamps that jump 20 ms back: put right at once, 20 ms
    /// of silence put in or 20 ms thrown away, not steered off a frame at a
    /// time (that took 20 s). A single packet stamped 15 ms off isn't one.
    #[test]
    fn sudden_offset_is_put_right_at_once() {
        for (jump, outlier) in [(0.02, false), (-0.02, false), (0.0, true)] {
            let feed = SourceFeed::new(RATE, clock_at_zero());
            let mut seed = 11u32;
            let mut jitter = move || {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                ((seed >> 8) as f64 / (1u32 << 24) as f64 * 2.0 - 1.0) * 0.0005
            };
            // 20 s in 10 ms packets of a steady level; at 10 s, the jump.
            let (mut slips, mut silent, mut taken) = (0i64, 0usize, 0i64);
            let mut late = 0f64;
            for k in 0..2_000 {
                let mut captured = 1000.05 + k as f64 * 0.01;
                if k >= 1_000 {
                    captured += jump;
                }
                let stamped = captured + jitter() + if outlier && k == 1_000 { 0.015 } else { 0.0 };
                let before = feed.st.lock().unwrap().next_in;
                feed.push(stamped, &[0.25; 480], 1);
                let after = feed.st.lock().unwrap().next_in.unwrap();
                if let Some(before) = before {
                    // A frame or two: slid (not the jump, which is hundreds).
                    let moved = after - before - 480;
                    if moved.abs() <= 2 {
                        slips += moved.abs();
                    }
                    // From 0.1 s after the jump, within a couple of ms.
                    if k >= 1_010 {
                        late = late.max(((captured - 1000.0) * RATE as f64 - after as f64 + 480.0).abs() / RATE as f64);
                    }
                }
                let upto = ((captured - 1000.1) * RATE as f64) as i64;
                if upto > taken {
                    silent += feed.take(taken, (upto - taken) as usize).iter().filter(|f| f[0] == 0.0).count();
                    taken = upto;
                }
            }
            let what = if outlier { "one packet 15 ms off".to_owned() } else { format!("a {:+} ms jump", jump * 1e3) };
            eprintln!("{what}: {silent} frames of silence, {slips} frames slid, then up to {:.2} ms off", late * 1e3);
            assert!(late < 0.002, "{what}: {:.2} ms off after it", late * 1e3);
            assert_eq!(slips, 0, "{what}: steered off");
            let expected = if jump > 0.0 { (jump * RATE as f64) as usize } else { 0 };
            // The first 50 ms, before the source began, are silent too.
            assert!(silent.abs_diff(2_400 + expected) < 50, "{what}: {silent} frames of silence");
        }
    }

    /// A frame put in or taken out slides the sound by one frame over a
    /// millisecond, read a little slower or faster there: no jump, and the
    /// rest as it was.
    #[test]
    fn a_slip_slides_without_a_jump() {
        let ramp: Vec<f32> = (0..480).map(|i| i as f32).collect();
        for n in [1i64, -1] {
            let mut x = ramp.clone();
            slip(&mut x, n, 48);
            assert_eq!(x.len() as i64, 480 + n);
            let steps: Vec<f32> = x.windows(2).map(|w| w[1] - w[0]).collect();
            // A ramp read at 48/49 or 50/49 of its speed there, else as it was.
            assert!(steps.iter().all(|&s| (s - 1.0).abs() <= 1.0 / 49.0 + 1e-4), "{n}: {steps:?}");
            assert_eq!(x[..200], ramp[..200]);
            assert_eq!(x[x.len() - 200..], ramp[ramp.len() - 200..]);
        }
        // An 8 kHz note (at half scale) moves up to 0.433 from one frame to
        // the next, and barely more slid; a frame dropped outright can jump
        // 0.866.
        let note: Vec<f32> = (0..480).map(|i| 0.5 * (std::f32::consts::TAU * 8000.0 * i as f32 / RATE as f32).sin()).collect();
        let most = |x: &[f32]| x.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
        for n in [1i64, -1] {
            let mut x = note.clone();
            slip(&mut x, n, 48);
            eprintln!("8 kHz note, {n:+} frame: moves up to {:.3} a frame ({:.3} as it came)", most(&x), most(&note));
            assert!(most(&x) < most(&note) * 1.05, "{n}: {}", most(&x));
        }
    }

    /// A short gap is still filled, so what follows stays in step.
    #[test]
    fn short_gap_is_filled() {
        let feed = SourceFeed::new(RATE, clock_at_zero());
        feed.push(1000.0, &vec![1.0; 480 * 2], 2); // 10 ms at 0
        feed.push(1000.5, &vec![1.0; 480 * 2], 2); // 10 ms at 500 ms
        let out = feed.take(0, 25_000);
        assert!(out[..480].iter().all(|f| f[0] == 1.0));
        assert!(out[480..24_000].iter().all(|f| f[0] == 0.0));
        assert!(out[24_000..24_480].iter().all(|f| f[0] == 1.0));
    }

    #[test]
    fn children_are_summed() {
        let feed = SourceFeed::new(RATE, clock_at_zero());
        let a = feed.add_child(RATE);
        let b = feed.add_child(RATE);
        a.push(1000.0, &vec![0.25; 960], 2);
        b.push(1000.0, &vec![0.5; 960], 2);
        assert!(feed.take(0, 480).iter().all(|f| (f[0] - 0.75).abs() < 1e-6));
        feed.remove_child(&b);
        b.push(1000.01, &vec![0.5; 960], 2);
        a.push(1000.01, &vec![0.25; 960], 2);
        assert!(feed.take(480, 480).iter().all(|f| (f[0] - 0.25).abs() < 1e-6));
    }

    /// A mic switched while its last 300 ms aren't mixed yet (the mix runs
    /// that far behind): they're in the mix, then the new mic. The old
    /// feed takes nothing more, and goes once all it had is taken.
    #[test]
    fn removed_child_is_mixed_to_its_end() {
        let feed = SourceFeed::new(RATE, clock_at_zero());
        let old = feed.add_child(RATE);
        old.push(1000.0, &vec![0.5; 14_400 * 2], 2);
        feed.remove_child(&old);
        old.push(1000.3, &vec![0.5; 480 * 2], 2);
        let new = feed.add_child(RATE);
        new.push(1000.3, &vec![0.25; 480 * 2], 2);
        assert!(feed.take(0, 4_800).iter().all(|f| f[0] == 0.5));
        assert_eq!(feed.children.lock().unwrap().len(), 2, "not all taken yet");
        let out = feed.take(4_800, 14_400 - 4_800 + 480);
        assert!(out[..9_600].iter().all(|f| f[0] == 0.5), "the old mic's last 300 ms");
        assert!(out[9_600..].iter().all(|f| f[0] == 0.25), "the new mic, alone");
        assert_eq!(feed.children.lock().unwrap().len(), 1, "the old one's gone");
    }

    #[test]
    fn peek_reads_ahead_without_consuming() {
        let feed = SourceFeed::new(RATE, clock_at_zero());
        feed.push(1000.0, &vec![0.5; 960], 2); // 10 ms
        assert!(feed.peek(0, 480).iter().all(|f| f[0] == 0.5));
        assert!(feed.peek(240, 480)[..240].iter().all(|f| f[0] == 0.5), "still there");
        assert!(feed.peek(240, 480)[240..].iter().all(|f| f[0] == 0.0), "silence past what arrived");
        assert!(feed.take(0, 480).iter().all(|f| f[0] == 0.5), "peeking took nothing");
    }

    /// A source gone quiet stays where its sound ended while the mixer moves
    /// on through it, so the meters stop waiting for it.
    #[test]
    fn quiet_source_stays_where_it_ended() {
        let feed = SourceFeed::new(RATE, clock_at_zero());
        feed.push(1000.0, &vec![0.5; 960], 2); // 10 ms
        assert_eq!(feed.arrived(), Some(480));
        feed.take(0, 4800);
        assert_eq!(feed.arrived(), Some(480), "the mixer reading on isn't sound arriving");
    }

    /// Everything a track got, as frames.
    fn frames(track: &Receiver<Vec<f32>>) -> Vec<[f32; 2]> {
        track.try_iter().flatten().collect::<Vec<f32>>().chunks_exact(2).map(|f| [f[0], f[1]]).collect()
    }

    /// A source of the clip's sound (unity gain), in or out of the mix.
    fn source(live: &LiveAudio, id: &str, feed: &Arc<SourceFeed>, track: Option<Sender<Vec<f32>>>) -> MixInput {
        MixInput { id: id.into(), feed: feed.clone(), channel: live.channel(id), in_mix: true, track }
    }

    /// Frame `at` seconds in.
    fn frame(at: f64) -> usize {
        (at * RATE as f64) as usize
    }

    /// `seconds` of a steady level, as interleaved stereo.
    fn steady(level: f32, seconds: f64) -> Vec<f32> {
        vec![level; frame(seconds) * 2]
    }

    /// A source added while mixing (one added while recording, which has no
    /// track of its own in the file) goes into the mix and, so the separate
    /// tracks still add up to it, into the rest track. The others' tracks
    /// go on as they were.
    #[test]
    fn a_source_added_while_mixing_goes_into_the_mix_and_the_rest_track() {
        let clock = clock_at_zero();
        let live = LiveAudio::new();
        live.set_limiter(false);
        let (mix_tx, mix) = std::sync::mpsc::channel();
        let (rest_tx, rest) = std::sync::mpsc::channel();
        let (game_tx, game_track) = std::sync::mpsc::channel();
        let game = SourceFeed::new(RATE, clock.clone());
        let mut mixer = Mixing::new(vec![source(&live, "game", &game, Some(game_tx))], Some(mix_tx), Some(rest_tx), live.clone(), 0.25);
        game.push(1000.0, &steady(0.25, 2.0), 2);
        mixer.mix(1.25, false);
        // Added when the mix is at 1 s; its sound from 1.1 s on.
        let music = SourceFeed::new(RATE, clock.clone());
        mixer.change(MixChange::Add(source(&live, "music", &music, None)));
        music.push(1001.1, &steady(0.5, 0.9), 2);
        mixer.mix(2.25, false);
        let (mix, rest, game_track) = (frames(&mix), frames(&rest), frames(&game_track));
        assert_eq!((mix.len(), rest.len(), game_track.len()), (frame(2.0), frame(2.0), frame(2.0)), "every track as long");
        assert!(mix[..frame(1.1)].iter().all(|f| f[0] == 0.25));
        assert!(mix[frame(1.1)..].iter().all(|f| f[0] == 0.75), "the added source mixed in");
        assert!(rest[..frame(1.1)].iter().all(|f| f[0] == 0.0));
        assert!(rest[frame(1.1)..].iter().all(|f| f[0] == 0.5), "and in the rest track");
        assert!(game_track.iter().all(|f| f[0] == 0.25), "the game's own track, the game alone");
    }

    /// A source taken out while mixing (its capture stopped when the mix is
    /// 250 ms behind) is mixed to where its sound ended, in the mix and in
    /// its own track; then it's gone, and its track goes on silent, as long
    /// as the others. Added again, it has its own track back.
    #[test]
    fn a_source_taken_out_is_mixed_to_its_end_then_its_track_goes_on_silent() {
        let clock = clock_at_zero();
        let live = LiveAudio::new();
        live.set_limiter(false);
        let (mix_tx, mix) = std::sync::mpsc::channel();
        let (rest_tx, rest) = std::sync::mpsc::channel();
        let (game_tx, game_track) = std::sync::mpsc::channel();
        let (mic_tx, mic_track) = std::sync::mpsc::channel();
        let (game, mic) = (SourceFeed::new(RATE, clock.clone()), SourceFeed::new(RATE, clock.clone()));
        let inputs = vec![source(&live, "game", &game, Some(game_tx)), source(&live, "mic", &mic, Some(mic_tx))];
        let mut mixer = Mixing::new(inputs, Some(mix_tx), Some(rest_tx), live.clone(), 0.25);
        game.push(1000.0, &steady(0.25, 3.0), 2);
        mic.push(1000.0, &steady(0.5, 1.0), 2);
        mixer.mix(1.0, false);
        // Taken out at 1 s: what it sends after isn't taken.
        mic.close();
        mixer.change(MixChange::Remove("mic".into()));
        mic.push(1001.0, &steady(0.5, 0.5), 2);
        mixer.mix(2.0, false);
        assert!(mixer.inputs.iter().all(|i| i.mix.id != "mic"), "gone once its sound is mixed");
        assert_eq!(mixer.parked.len(), 1);
        mixer.mix(2.25, false);
        let mic_was = frames(&mic_track);
        assert_eq!((mic_was.len(), frames(&game_track).len()), (frame(2.0), frame(2.0)), "its track goes on, as long as the game's");
        assert!(mic_was[..frame(1.0)].iter().all(|f| f[0] == 0.5), "its last 250 ms too");
        assert!(mic_was[frame(1.0)..].iter().all(|f| f[0] == 0.0), "silent once it's taken out");
        let mixed = frames(&mix);
        assert!(mixed[..frame(1.0)].iter().all(|f| f[0] == 0.75));
        assert!(mixed[frame(1.0)..].iter().all(|f| f[0] == 0.25));

        // Added again when the mix is at 2 s, its sound from 2.5 s: on its
        // own track.
        let mic = SourceFeed::new(RATE, clock.clone());
        mixer.change(MixChange::Add(source(&live, "mic", &mic, None)));
        mic.push(1002.5, &steady(0.5, 0.5), 2);
        mixer.mix(3.25, false);
        let mic_now = frames(&mic_track);
        assert_eq!(mic_now.len(), frame(1.0));
        assert!(mic_now[..frame(0.5)].iter().all(|f| f[0] == 0.0));
        assert!(mic_now[frame(0.5)..].iter().all(|f| f[0] == 0.5), "its own track back");
        assert!(mixer.parked.is_empty());
        assert!(frames(&rest).iter().all(|f| f[0] == 0.0), "not in the rest track: it has a track of its own");
    }

    /// A feed whose source has stopped being captured takes no more sound,
    /// in itself or the feeds it sums, nor in one added to it after; what
    /// came before is still there to mix.
    #[test]
    fn a_closed_feed_takes_no_more_sound() {
        let feed = SourceFeed::new(RATE, clock_at_zero());
        let child = feed.add_child(RATE);
        child.push(1000.0, &steady(0.5, 0.01), 2);
        feed.close();
        child.push(1000.01, &steady(0.5, 0.01), 2);
        feed.push(1000.01, &steady(0.5, 0.01), 2);
        feed.add_child(RATE).push(1000.01, &steady(0.5, 0.01), 2);
        assert_eq!(feed.arrived(), Some(480), "only what came before");
        let out = feed.take(0, 960);
        assert!(out[..480].iter().all(|f| f[0] == 0.5));
        assert!(out[480..].iter().all(|f| f[0] == 0.0));
    }

    /// A change sent to a mixer that's running is taken up: here a source
    /// added before its first pass, which (stopping) mixes all there is.
    #[test]
    fn a_running_mixer_takes_changes() {
        static NOW: AtomicU64 = AtomicU64::new(0);
        fn now() -> f64 {
            f64::from_bits(NOW.load(Ordering::Relaxed))
        }
        NOW.store(1001.0f64.to_bits(), Ordering::Relaxed);
        let clock = clock_at_zero();
        let live = LiveAudio::new();
        live.set_limiter(false);
        let (mix_tx, mix) = std::sync::mpsc::channel();
        let (changes, changed) = std::sync::mpsc::channel();
        let music = SourceFeed::new(RATE, clock.clone());
        music.push(1000.0, &steady(0.5, 1.0), 2);
        changes.send(MixChange::Add(source(&live, "music", &music, None))).unwrap();
        let stop = Arc::new(AtomicBool::new(true));
        spawn_changing_mixer(Vec::new(), changed, Some(mix_tx), None, live, clock, now, 0.25, stop).join().unwrap();
        let mix = frames(&mix);
        assert_eq!(mix.len(), frame(1.0));
        assert!(mix.iter().all(|f| f[0] == 0.5));
    }

    #[test]
    fn limiter_holds_the_ceiling() {
        let mut lim = Limiter::new();
        let mut block = vec![[1.8f32, -1.5]; 1000];
        let reduction = lim.process(&mut block);
        assert!(block.iter().all(|f| f[0].abs() <= CEILING + 1e-6 && f[1].abs() <= CEILING + 1e-6));
        assert!(reduction > 5.0);
    }
}
