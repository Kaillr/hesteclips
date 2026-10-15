//! Audio sources: what the user adds (a mic, everything the computer plays, one
//! app), and the capture that turns them into [`SourceFeed`]s for the mixer.
//!
//! The same capture runs in two places: inside a recording, and in the
//! [`LevelMonitor`] that drives the meters while nothing is recording, so levels
//! can be set before the moment you want to clip.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use anyhow::Result;
#[cfg(not(target_os = "linux"))]
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::mixer::{Clock, LiveAudio, MixInput, SourceFeed, spawn_mixer};
#[cfg(not(target_os = "linux"))]
use crate::mixer::SourceStatus;

/// Where a source's sound comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceKind {
    /// An input device, by name (already resolved from "default").
    Microphone { device: String },
    /// Everything the computer plays. With `exclude_app_sources`, apps that are
    /// also added as their own source are left out, so they aren't heard twice.
    Desktop { exclude_app_sources: bool },
    /// One application (and its helper processes), by bundle id on macOS,
    /// executable name (`Discord.exe`) on Windows, or program name
    /// (`Discord`, or `Game.exe` for one running in Wine/Proton) on Linux.
    App { bundle_id: String },
}

/// One source to capture, and where its audio goes in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSource {
    /// Stable id; keys its volume/meter in [`LiveAudio`].
    pub id: String,
    /// Shown to the user and used as its track's name.
    pub name: String,
    pub kind: SourceKind,
    /// Mixed into track 1, what the clip sounds like everywhere.
    pub in_mix: bool,
    /// Also recorded alone on its own track, for rebalancing in the editor.
    pub own_track: bool,
}

/// Tag stored on recordings listing which audio streams are part of the mix
/// (comma-separated stream indices), so the editor can rebuild track 1 exactly.
pub const MIX_TAG_PREFIX: &str = "hasteclip:mix=";

/// Name of the track holding sources that are in the clip without a track of
/// their own.
pub const REST_TRACK: &str = "Other clip audio";

/// The track layout for a set of sources: track 1 is always the mix (silent if
/// no source is in it), then one track per source with `own_track`, in order,
/// then — if some sources are only in the mix — one track with their sum, so the
/// separate tracks always add up to the mix. Returns the track titles, the
/// `MIX_TAG_PREFIX` comment, and whether there's a rest track.
pub fn track_layout(sources: &[AudioSource]) -> (Vec<String>, String, bool) {
    if sources.is_empty() {
        return (Vec::new(), String::new(), false);
    }
    let mut titles = vec!["Mix".to_owned()];
    let mut in_mix = Vec::new();
    for s in sources.iter().filter(|s| s.own_track) {
        if s.in_mix {
            in_mix.push(titles.len().to_string());
        }
        titles.push(s.name.clone());
    }
    // Without any own track, the mix alone already is "the" track.
    let rest = titles.len() > 1 && sources.iter().any(|s| s.in_mix && !s.own_track);
    if rest {
        in_mix.push(titles.len().to_string());
        titles.push(REST_TRACK.to_owned());
    }
    (titles, format!("{MIX_TAG_PREFIX}{}", in_mix.join(",")), rest)
}

/// Live capture of a set of sources, each feeding its own [`SourceFeed`].
pub(crate) struct AudioCapture {
    /// One per source, in the order given.
    pub feeds: Vec<Arc<SourceFeed>>,
    #[cfg(not(target_os = "linux"))]
    mics: Vec<Mic>,
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    system: Option<SystemAudio>,
}

/// A microphone source. Its device's sound goes into a feed of its own
/// (at the device's rate) inside the source's, so the device can be switched
/// while capturing: the source, and its track, stay.
#[cfg(not(target_os = "linux"))]
struct Mic {
    id: String,
    device: String,
    feed: Arc<SourceFeed>,
    channel: Arc<crate::mixer::Channel>,
    /// The device's stream and the feed it fills, while it's open.
    open: Option<(cpal::Stream, Arc<SourceFeed>)>,
}

#[cfg(not(target_os = "linux"))]
impl Mic {
    fn open(&mut self) {
        if let Some((stream, child)) = self.open.take() {
            drop(stream);
            self.feed.remove_child(&child);
        }
        let child = self.feed.add_child(native_rate(&SourceKind::Microphone { device: self.device.clone() }));
        child.denoise_with(self.channel.denoise_flag());
        match start_mic(&self.device, child.clone()) {
            Ok(stream) => {
                self.channel.set_status(SourceStatus::Live);
                self.open = Some((stream, child));
            }
            Err(e) => {
                eprintln!("microphone \"{}\": {e}", self.device);
                self.feed.remove_child(&child);
                self.channel.set_status(SourceStatus::Unavailable);
            }
        }
    }
}

#[cfg(target_os = "macos")]
use crate::sck::SystemAudio;
#[cfg(target_os = "windows")]
use crate::win::SystemAudio;
/// On Linux every kind of source, mics included, is a PipeWire stream.
#[cfg(target_os = "linux")]
use crate::linux::audio::SystemAudio;

impl AudioCapture {
    /// Start every source. A source that can't start (unplugged mic, app not
    /// running) doesn't fail the capture: its status says why and its track is
    /// silent until it can.
    pub(crate) fn start(sources: &[AudioSource], live: &LiveAudio, clock: Arc<Clock>) -> Result<Self> {
        // A mic's own feed is inside its source's (see `Mic`): that one is at
        // the mixer's rate.
        let feeds: Vec<Arc<SourceFeed>> = sources
            .iter()
            .map(|s| match s.kind {
                #[cfg(not(target_os = "linux"))]
                SourceKind::Microphone { .. } => SourceFeed::new(crate::mixer::RATE, clock.clone()),
                _ => SourceFeed::new(native_rate(&s.kind), clock.clone()),
            })
            .collect();
        #[cfg(not(target_os = "linux"))]
        let mut mics = Vec::new();
        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
        let mut system_sources = Vec::new();
        for (source, feed) in sources.iter().zip(&feeds) {
            let channel = live.channel(&source.id);
            #[cfg(target_os = "linux")]
            if matches!(source.kind, SourceKind::Microphone { .. }) {
                feed.denoise_with(channel.denoise_flag());
            }
            match &source.kind {
                #[cfg(not(target_os = "linux"))]
                SourceKind::Microphone { device } => {
                    let mut mic = Mic { id: source.id.clone(), device: device.clone(), feed: feed.clone(), channel, open: None };
                    mic.open();
                    mics.push(mic);
                }
                #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
                _ => system_sources.push((source.clone(), feed.clone(), channel)),
                #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
                _ => channel.set_status(SourceStatus::Unavailable),
            }
        }
        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
        let system = if system_sources.is_empty() { None } else { Some(SystemAudio::start(system_sources)?) };
        // HasteClip' own sound: into its app source if it has one, else into
        // the desktop (unless the desktop leaves out apps with their own source).
        let own = own_app_id();
        let own_source = sources.iter().any(|s| is_own_app(&s.kind, own.as_deref()));
        for (source, feed) in sources.iter().zip(&feeds) {
            let takes = match source.kind {
                SourceKind::Desktop { exclude_app_sources } => !(own_source && exclude_app_sources),
                ref kind => is_own_app(kind, own.as_deref()),
            };
            if takes {
                own_sound::listen(feed);
            }
        }
        Ok(Self {
            feeds,
            #[cfg(not(target_os = "linux"))]
            mics,
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            system,
        })
    }

    /// Switch the microphone of the source `id` to `device`, without stopping
    /// anything. False if that source isn't a mic here (on Linux mics are
    /// PipeWire streams, set at the start).
    pub(crate) fn set_mic(&mut self, id: &str, device: &str) -> bool {
        #[cfg(not(target_os = "linux"))]
        if let Some(mic) = self.mics.iter_mut().find(|m| m.id == id) {
            if mic.device != device {
                mic.device = device.to_owned();
                mic.open();
            }
            return true;
        }
        let _ = (id, device);
        false
    }

    pub(crate) fn stop(mut self) {
        for feed in &self.feeds {
            own_sound::forget(feed);
        }
        #[cfg(not(target_os = "linux"))]
        self.mics.clear();
        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
        if let Some(system) = self.system.take() {
            system.stop();
        }
    }
}

/// Every source is converted to 48 kHz; desktop/app audio is captured at it,
/// and on Linux so is every mic (PipeWire converts it).
fn native_rate(kind: &SourceKind) -> u32 {
    match kind {
        #[cfg(not(target_os = "linux"))]
        SourceKind::Microphone { device } => {
            find_input(device).and_then(|d| d.default_input_config().ok()).map_or(crate::mixer::RATE, |c| c.sample_rate())
        }
        _ => crate::mixer::RATE,
    }
}

#[cfg(not(target_os = "linux"))]
fn find_input(name: &str) -> Option<cpal::Device> {
    cpal::default_host()
        .input_devices()
        .ok()?
        .find(|d| d.description().is_ok_and(|desc| desc.name() == name))
}

#[cfg(not(target_os = "linux"))]
fn start_mic(name: &str, feed: Arc<SourceFeed>) -> Result<cpal::Stream> {
    let device = find_input(name).ok_or_else(|| anyhow::anyhow!("not connected"))?;
    let config = device.default_input_config()?;
    let channels = config.channels() as usize;
    let mut stream_config = config.config();
    // Room for 200 ms of backlog. By default the device's buffer holds only
    // ~10-20 ms, so the reading thread being scheduled a little late (a game
    // pinning the CPU) overflowed it and lost audio. It still wakes every
    // ~10 ms, and every packet is placed by its timestamp, so this adds no delay.
    let backlog = config.sample_rate() / 5;
    stream_config.buffer_size = match config.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } => cpal::BufferSize::Fixed(backlog.clamp(*min, *max)),
        cpal::SupportedBufferSize::Unknown => cpal::BufferSize::Fixed(backlog),
    };
    let stream = device.build_input_stream::<f32, _, _>(
        stream_config,
        move |data, info| {
            let start = info.timestamp().capture.as_nanos() as f64 / 1e9;
            feed.push(start, data, channels);
        },
        quiet_xruns(name.to_owned()),
        None,
    )?;
    stream.play()?;
    Ok(stream)
}

/// The mic's error handler. Windows marks the first packets after a stream
/// starts as a gap (an "underrun or overrun"), which isn't lost audio: that's
/// ignored. A real dropout later — the mic's buffer filled before it was read,
/// usually because the PC was too busy — is reported, at most once a minute.
#[cfg(not(target_os = "linux"))]
pub(crate) fn quiet_xruns(name: String) -> impl FnMut(cpal::Error) + Send + 'static {
    let started = std::time::Instant::now();
    let mut dropouts = 0u32;
    let mut reported: Option<std::time::Instant> = None;
    move |e| {
        if !matches!(e.kind(), cpal::ErrorKind::Xrun) {
            eprintln!("microphone \"{name}\": {e}");
            return;
        }
        if started.elapsed() < std::time::Duration::from_secs(1) {
            return;
        }
        dropouts += 1;
        if reported.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(60)) {
            eprintln!("microphone \"{name}\": {dropouts} short dropout(s) — it wasn't read in time (the PC was busy)");
            reported = Some(std::time::Instant::now());
            dropouts = 0;
        }
    }
}

/// HasteClip' own sounds (the clip-saved cue) in desktop audio.
///
/// Desktop capture leaves out everything HasteClip plays, so a mic you
/// "Listen" to never ends up in a clip twice. What it plays for you (clips,
/// scrubbing, the clip sound) is added here instead, as it plays, so it's in
/// the recording like any other app's: in the desktop source, or in a
/// HasteClip app source when there's one (and the desktop leaves apps with
/// their own source out).
pub mod own_sound {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, Weak};

    use crate::mixer::{RATE, SourceFeed};

    /// The sources HasteClip' sound goes into; bumped generation on change.
    fn listeners() -> &'static Mutex<Vec<Weak<SourceFeed>>> {
        static LISTENERS: Mutex<Vec<Weak<SourceFeed>>> = Mutex::new(Vec::new());
        &LISTENERS
    }
    static GENERATION: AtomicU64 = AtomicU64::new(0);

    pub(crate) fn listen(feed: &Arc<SourceFeed>) {
        let mut d = listeners().lock().unwrap();
        d.retain(|w| w.strong_count() > 0);
        d.push(Arc::downgrade(feed));
        GENERATION.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn forget(feed: &Arc<SourceFeed>) {
        listeners().lock().unwrap().retain(|w| w.strong_count() > 0 && !std::ptr::eq(w.as_ptr(), Arc::as_ptr(feed)));
        GENERATION.fetch_add(1, Ordering::Relaxed);
    }

    /// One sound being played: what it pushes goes into every source taking
    /// HasteClip' sound, including ones that start while it plays (the
    /// player stays open across recordings).
    pub struct Playing {
        rate: u32,
        generation: u64,
        children: Vec<(Weak<SourceFeed>, Arc<SourceFeed>)>,
    }

    impl Playing {
        /// Stereo at 48 kHz.
        pub fn start() -> Self {
            Self::at(RATE)
        }

        /// At `rate` (an output device's).
        pub fn at(rate: u32) -> Self {
            let mut playing = Self { rate, generation: u64::MAX, children: Vec::new() };
            playing.follow();
            playing
        }

        /// Into the sources taking HasteClip' sound right now.
        fn follow(&mut self) {
            let generation = GENERATION.load(Ordering::Relaxed);
            if generation == self.generation {
                return;
            }
            self.generation = generation;
            self.leave();
            self.children = listeners().lock().unwrap().iter().filter_map(|w| Some((w.clone(), w.upgrade()?.add_child(self.rate)))).collect();
        }

        fn leave(&mut self) {
            for (parent, child) in self.children.drain(..) {
                if let Some(p) = parent.upgrade() {
                    p.remove_child(&child);
                }
            }
        }

        /// Stereo samples, played now.
        pub fn push(&mut self, samples: &[f32]) {
            self.push_frames(samples, 2, 0.0);
        }

        /// Interleaved samples, `channels` per frame (as the output device
        /// takes them), heard `delay` seconds from now.
        pub fn push_frames(&mut self, samples: &[f32], channels: usize, delay: f64) {
            self.follow();
            if self.children.is_empty() || samples.is_empty() {
                return;
            }
            let at = super::host_now() + delay;
            for (_, child) in &self.children {
                child.push(at, samples, channels);
            }
        }
    }

    impl Drop for Playing {
        fn drop(&mut self) {
            self.leave();
        }
    }
}

/// HasteClip' own id as an app source ([`SourceKind::App`]): its executable
/// on Windows, its bundle id on macOS. None on Linux, where it isn't listed.
pub fn own_app_id() -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        std::env::current_exe().ok()?.file_name().map(|n| n.to_string_lossy().into_owned())
    }
    #[cfg(target_os = "macos")]
    {
        objc2_app_kit::NSRunningApplication::currentApplication().bundleIdentifier().map(|id| id.to_string())
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        None
    }
}

/// `kind` is HasteClip itself, added as an app.
pub(crate) fn is_own_app(kind: &SourceKind, own: Option<&str>) -> bool {
    matches!((kind, own), (SourceKind::App { bundle_id }, Some(own)) if bundle_id.eq_ignore_ascii_case(own))
}

/// Captures sources only to drive the meters (nothing is written anywhere).
pub struct LevelMonitor {
    capture: Option<AudioCapture>,
    stop: Arc<AtomicBool>,
    mixer: Option<JoinHandle<()>>,
}

impl LevelMonitor {
    pub fn start(sources: &[AudioSource], live: Arc<LiveAudio>) -> Result<Self> {
        let clock = Arc::new(Clock::default());
        clock.set(host_now());
        let capture = AudioCapture::start(sources, &live, clock.clone())?;
        let inputs = mix_inputs(sources, &capture.feeds, &live, |_| None);
        let stop = Arc::new(AtomicBool::new(false));
        // The meters look ahead at fresh audio on their own (`METER_LAG`);
        // the mix here only clears what they've seen, so it stays well behind
        // them, as a recording's does. Close behind, it took audio away before
        // the meters got to it (a source sends its sound in blocks), and they
        // stood still.
        let latency = 0.3;
        let mixer = spawn_mixer(inputs, None, None, live, clock, host_now, latency, stop.clone());
        Ok(Self { capture: Some(capture), stop, mixer: Some(mixer) })
    }
}

impl Drop for LevelMonitor {
    fn drop(&mut self) {
        if let Some(capture) = self.capture.take() {
            capture.stop();
        }
        self.stop.store(true, Ordering::Relaxed);
        if let Some(m) = self.mixer.take() {
            let _ = m.join();
        }
    }
}

pub(crate) fn mix_inputs(
    sources: &[AudioSource],
    feeds: &[Arc<SourceFeed>],
    live: &LiveAudio,
    mut track_for: impl FnMut(&AudioSource) -> Option<std::sync::mpsc::Sender<Vec<f32>>>,
) -> Vec<MixInput> {
    sources
        .iter()
        .zip(feeds)
        .map(|(s, feed)| MixInput {
            feed: feed.clone(),
            channel: live.channel(&s.id),
            in_mix: s.in_mix,
            track: if s.own_track { track_for(s) } else { None },
        })
        .collect()
}

/// Host time in seconds — the clock capture timestamps use (CoreAudio and SCK
/// on macOS; QueryPerformanceCounter for WGC and WASAPI on Windows;
/// CLOCK_MONOTONIC, PipeWire's, on Linux).
pub(crate) fn host_now() -> f64 {
    #[cfg(target_os = "macos")]
    {
        crate::sck::host_now()
    }
    #[cfg(target_os = "windows")]
    {
        crate::win::host_now()
    }
    #[cfg(target_os = "linux")]
    {
        crate::linux::host_now()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        use std::sync::OnceLock;
        static START: OnceLock<std::time::Instant> = OnceLock::new();
        START.get_or_init(std::time::Instant::now).elapsed().as_secs_f64()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(name: &str, in_mix: bool, own_track: bool) -> AudioSource {
        AudioSource {
            id: name.into(),
            name: name.into(),
            kind: SourceKind::Desktop { exclude_app_sources: false },
            in_mix,
            own_track,
        }
    }

    #[test]
    fn layout_puts_mix_first_and_tags_members() {
        let (titles, tag, rest) = track_layout(&[src("Mic", true, true), src("Discord", false, true)]);
        assert_eq!(titles, ["Mix", "Mic", "Discord"]);
        assert_eq!(tag, "hasteclip:mix=1");
        assert!(!rest);
    }

    #[test]
    fn mix_only_sources_get_a_shared_track() {
        let (titles, tag, rest) = track_layout(&[src("Mic", true, true), src("Music", true, false), src("Game", true, false)]);
        assert_eq!(titles, ["Mix", "Mic", REST_TRACK]);
        assert_eq!(tag, "hasteclip:mix=1,2");
        assert!(rest);
        // No own tracks at all: the mix is the only track.
        assert_eq!(track_layout(&[src("Music", true, false)]).0, ["Mix"]);
    }

    #[test]
    fn no_sources_no_tracks() {
        assert_eq!(track_layout(&[]).0.len(), 0);
    }

    #[test]
    #[ignore = "captures real system audio; run with --ignored"]
    fn own_sound_reaches_the_desktop_source() {
        let live = LiveAudio::new();
        let desktop = src("Desktop", true, false);
        let _monitor = LevelMonitor::start(std::slice::from_ref(&desktop), live.clone()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        let channel = live.channel("Desktop");
        let before = channel.input.take().max_peak();
        // Half a second of a loud tone, pushed as it would play.
        let mut playing = own_sound::Playing::start();
        let block: Vec<f32> = (0..480).flat_map(|i| [0.8 * (i as f32 * 0.1).sin(); 2]).collect();
        for _ in 0..50 {
            playing.push(&block);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        let during = channel.input.take().max_peak();
        eprintln!("desktop peak before {before:.3}, with our sound {during:.3}");
        assert!(during > 0.7, "{during}");
    }

    #[test]
    #[ignore = "captures real system audio; run with --ignored"]
    fn own_sound_goes_to_its_app_source_from_a_player_opened_before() {
        // A player open before the capture starts (at 44.1 kHz, 4 channels).
        let mut playing = own_sound::Playing::at(44_100);
        let live = LiveAudio::new();
        let own = own_app_id().expect("HasteClip' own app id");
        let desktop = AudioSource { kind: SourceKind::Desktop { exclude_app_sources: true }, ..src("Desktop", true, true) };
        let app = AudioSource { kind: SourceKind::App { bundle_id: own }, ..src("HasteClip", true, true) };
        let _monitor = LevelMonitor::start(&[desktop, app], live.clone()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        let (desktop, app) = (live.channel("Desktop"), live.channel("HasteClip"));
        let _ = (desktop.input.take(), app.input.take());
        let block: Vec<f32> = (0..441).flat_map(|i| [0.8 * (i as f32 * 0.1).sin(); 4]).collect();
        for _ in 0..50 {
            playing.push_frames(&block, 4, 0.02);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        let (in_desktop, in_app) = (desktop.input.take().max_peak(), app.input.take().max_peak());
        eprintln!("our sound: desktop peak {in_desktop:.3}, app source peak {in_app:.3}, app source {:?}", app.status());
        assert!(in_app > 0.7, "{in_app}");
        assert!(in_desktop < 0.1, "heard twice: {in_desktop}");
    }
}
