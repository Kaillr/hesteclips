//! Live audio: per-source volume/mute controls and level meters shared with the
//! UI ([`LiveAudio`]), and the mixer that turns timestamped source audio into the
//! recorded tracks.
//!
//! Every source is aligned on one host clock: [`SourceFeed`] places each block of
//! samples by its capture timestamp (gaps become silence, overlaps are dropped)
//! and converts it to 48 kHz stereo. The mixer then pulls exactly the same span
//! from every feed a short moment behind real time, applies volume, meters it, and
//! writes the mix and the separate tracks — so tracks can never drift apart.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

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
}

impl Channel {
    fn new() -> Self {
        Self {
            gain: AtomicF32::new(1.0),
            muted: AtomicBool::new(false),
            status: AtomicU8::new(0),
            meter: Meter::default(),
            input: Meter::default(),
        }
    }

    /// Volume as linear gain (1.0 = unchanged). Takes effect immediately, even
    /// mid-recording.
    pub fn set_volume(&self, gain: f32, muted: bool) {
        self.gain.store(gain.max(0.0));
        self.muted.store(muted, Ordering::Relaxed);
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
    /// Output frames still to discard: resampler delay, or audio that arrived
    /// after the mixer had already moved past it.
    skip_out: i64,
}

impl SourceFeed {
    /// Correct timing once it's off by more than this; smaller jitter is ignored.
    const TOLERANCE: f64 = 0.04;

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
                skip_out: 0,
            }),
            children: Mutex::new(Vec::new()),
        })
    }

    /// A new feed (at `rate`) whose audio is added to this one's.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn add_child(&self, rate: u32) -> Arc<SourceFeed> {
        let child = SourceFeed::new(rate, self.clock.clone());
        self.children.lock().unwrap().push(child.clone());
        child
    }

    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn remove_child(&self, child: &Arc<SourceFeed>) {
        self.children.lock().unwrap().retain(|c| !Arc::ptr_eq(c, child));
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

        if st.next_in.is_none() {
            st.next_in = Some(expected);
            st.out.clear();
            st.out_base = (expected as f64 * RATE as f64 / rate).round() as i64;
            st.skip_out = st.resampler.as_ref().map_or(0, |r| r.output_delay() as i64);
        }
        let drift = expected - st.next_in.unwrap();
        let tolerance = (Self::TOLERANCE * rate) as i64;
        let mut skip = 0usize;
        if drift > tolerance {
            st.feed_silence(drift as usize);
        } else if drift < -tolerance {
            skip = ((-drift) as usize).min(frames);
        }
        let (l, r): (Vec<f32>, Vec<f32>) = (skip..frames)
            .map(|i| {
                let f = &samples[i * channels..];
                (f[0], if channels > 1 { f[1] } else { f[0] })
            })
            .unzip();
        st.feed(&l, &r);
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
        for child in self.children.lock().unwrap().iter() {
            for (a, c) in buf.iter_mut().zip(child.take(from, n)) {
                a[0] += c[0];
                a[1] += c[1];
            }
        }
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
        loop {
            let resampler = self.resampler.as_mut().unwrap();
            let need = resampler.input_frames_next();
            if self.pending[0].len() < need {
                break;
            }
            let input: Vec<Vec<f32>> = self.pending.iter_mut().map(|p| p.drain(..need).collect()).collect();
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
    }

    fn append(&mut self, frames: impl Iterator<Item = [f32; 2]>) {
        for f in frames {
            if self.skip_out > 0 {
                self.skip_out -= 1;
            } else {
                self.out.push_back(f);
            }
        }
    }
}

/// A source as the mixer sees it.
pub(crate) struct MixInput {
    pub feed: Arc<SourceFeed>,
    pub channel: Arc<Channel>,
    pub in_mix: bool,
    /// Its own track, if it has one.
    pub track: Option<Sender<Vec<f32>>>,
}

/// How far behind real time the meters run: just enough for the sources'
/// newest audio to have arrived.
const METER_LAG: f64 = 0.04;

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
    thread::spawn(move || {
        let mut gains: Vec<f32> = inputs.iter().map(|i| i.channel.effective_gain()).collect();
        let mut limiter = Limiter::new();
        let mut mixed: Option<i64> = None;
        // The meters' own position and limiter, ahead of the mix.
        let mut metered: Option<i64> = None;
        let mut meter_limiter = Limiter::new();
        loop {
            let stopping = stop.load(Ordering::Relaxed);
            if let Some(t0) = clock.get() {
                let target = ((now() - t0 - METER_LAG) * RATE as f64).floor() as i64;
                let from = metered.get_or_insert(target.max(0));
                // Never meter what's already been written (and so is gone).
                *from = (*from).max(mixed.unwrap_or(0));
                while *from < target {
                    let n = ((target - *from) as usize).min(RATE as usize / 50);
                    let mut mix = vec![[0f32; 2]; n];
                    for input in &inputs {
                        let mut block = input.feed.peek(*from, n);
                        input.channel.input.record(&block);
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
                    if live.limiter.load(Ordering::Relaxed) {
                        let reduction = meter_limiter.process(&mut mix);
                        live.reduction_db.fetch_max(reduction);
                    }
                    live.master.record(&mix);
                    *from += n as i64;
                }
                let behind = if stopping { 0.0 } else { latency };
                let target = ((now() - t0 - behind) * RATE as f64).floor() as i64;
                let from = mixed.get_or_insert(0);
                while *from < target {
                    let n = ((target - *from) as usize).min(RATE as usize / 50);
                    let mut mix = vec![[0f32; 2]; n];
                    let mut rest = vec![[0f32; 2]; if rest_track.is_some() { n } else { 0 }];
                    for (input, gain) in inputs.iter().zip(gains.iter_mut()) {
                        let mut block = input.feed.take(*from, n);
                        // Ramp to the new volume across the block so fader moves don't click.
                        let target_gain = input.channel.effective_gain();
                        for (k, f) in block.iter_mut().enumerate() {
                            let g = *gain + (target_gain - *gain) * (k + 1) as f32 / n as f32;
                            f[0] *= g;
                            f[1] *= g;
                        }
                        *gain = target_gain;
                        if input.in_mix {
                            add(&mut mix, &block);
                            if input.track.is_none() {
                                add(&mut rest, &block);
                            }
                        }
                        if let Some(tx) = &input.track {
                            let _ = tx.send(interleave(&block));
                        }
                    }
                    if live.limiter.load(Ordering::Relaxed) {
                        limiter.process(&mut mix);
                    }
                    if let Some(tx) = &mix_track {
                        let _ = tx.send(interleave(&mix));
                    }
                    if let Some(tx) = &rest_track {
                        let _ = tx.send(interleave(&rest));
                    }
                    *from += n as i64;
                }
            }
            if stopping {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        live.all_off();
    })
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

    #[test]
    fn peek_reads_ahead_without_consuming() {
        let feed = SourceFeed::new(RATE, clock_at_zero());
        feed.push(1000.0, &vec![0.5; 960], 2); // 10 ms
        assert!(feed.peek(0, 480).iter().all(|f| f[0] == 0.5));
        assert!(feed.peek(240, 480)[..240].iter().all(|f| f[0] == 0.5), "still there");
        assert!(feed.peek(240, 480)[240..].iter().all(|f| f[0] == 0.0), "silence past what arrived");
        assert!(feed.take(0, 480).iter().all(|f| f[0] == 0.5), "peeking took nothing");
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
