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
pub const MIX_TAG_PREFIX: &str = "hesteclips:mix=";

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
    mics: Vec<cpal::Stream>,
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    system: Option<SystemAudio>,
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
        let feeds: Vec<Arc<SourceFeed>> = sources
            .iter()
            .map(|s| SourceFeed::new(native_rate(&s.kind), clock.clone()))
            .collect();
        #[cfg(not(target_os = "linux"))]
        let mut mics = Vec::new();
        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
        let mut system_sources = Vec::new();
        for (source, feed) in sources.iter().zip(&feeds) {
            let channel = live.channel(&source.id);
            match &source.kind {
                #[cfg(not(target_os = "linux"))]
                SourceKind::Microphone { device } => match start_mic(device, feed.clone()) {
                    Ok(stream) => {
                        channel.set_status(SourceStatus::Live);
                        mics.push(stream);
                    }
                    Err(e) => {
                        eprintln!("microphone \"{device}\": {e}");
                        channel.set_status(SourceStatus::Unavailable);
                    }
                },
                #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
                _ => system_sources.push((source.clone(), feed.clone(), channel)),
                #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
                _ => channel.set_status(SourceStatus::Unavailable),
            }
        }
        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
        let system = if system_sources.is_empty() { None } else { Some(SystemAudio::start(system_sources)?) };
        Ok(Self {
            feeds,
            #[cfg(not(target_os = "linux"))]
            mics,
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            system,
        })
    }

    pub(crate) fn stop(mut self) {
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
fn quiet_xruns(name: String) -> impl FnMut(cpal::Error) + Send + 'static {
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
        // Short latency: meters should feel live; a late block only costs a blip.
        // Longer on Linux, where PipeWire's timestamps include the device's
        // buffering, which runs to a fifth of a second for Bluetooth mics and
        // in virtual machines: their audio would always arrive too late.
        let latency = if cfg!(target_os = "linux") { 0.25 } else { 0.08 };
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
        assert_eq!(tag, "hesteclips:mix=1");
        assert!(!rest);
    }

    #[test]
    fn mix_only_sources_get_a_shared_track() {
        let (titles, tag, rest) = track_layout(&[src("Mic", true, true), src("Music", true, false), src("Game", true, false)]);
        assert_eq!(titles, ["Mix", "Mic", REST_TRACK]);
        assert_eq!(tag, "hesteclips:mix=1,2");
        assert!(rest);
        // No own tracks at all: the mix is the only track.
        assert_eq!(track_layout(&[src("Music", true, false)]).0, ["Mix"]);
    }

    #[test]
    fn no_sources_no_tracks() {
        assert_eq!(track_layout(&[]).0.len(), 0);
    }
}
