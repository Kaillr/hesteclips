//! Audio sources: what the user adds (a mic, everything the computer plays, one
//! app), and the capture that turns them into [`SourceFeed`]s for the mixer.
//!
//! The same capture runs in two places: inside a recording, and in the
//! [`LevelMonitor`] that drives the meters while nothing is recording, so levels
//! can be set before the moment you want to clip.

use std::sync::Arc;
#[cfg(not(target_os = "linux"))]
use std::sync::Mutex;
#[cfg(not(target_os = "linux"))]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
#[cfg(not(target_os = "linux"))]
use std::time::{Duration, Instant};

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
    /// A video source's sound: the video layer whose `sound` is `id` (the
    /// source's own) plays its file's sound into it as it plays (see
    /// [`video_sound`]).
    Video { id: String },
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
    /// One per source it started with, in the order given.
    pub feeds: Vec<Arc<SourceFeed>>,
    /// The sources captured now, with their feeds: those it started with,
    /// and those added since ([`Self::set_sources`]), less those taken out.
    captured: Vec<(AudioSource, Arc<SourceFeed>)>,
    /// What every source's sound is placed by.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    clock: Arc<Clock>,
    #[cfg(not(target_os = "linux"))]
    mics: Mics,
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    system: Option<SystemAudio>,
}

/// A microphone source. Its device's sound goes into a feed of its own
/// (at the device's rate) inside the source's, so the device can be switched
/// while capturing, or opened again after it stopped: the source, and its
/// track, stay.
#[cfg(not(target_os = "linux"))]
struct Mic {
    id: String,
    device: String,
    feed: Arc<SourceFeed>,
    channel: Arc<crate::mixer::Channel>,
    /// The device's stream and the feed it fills, while it's open.
    stream: Option<(cpal::Stream, Arc<SourceFeed>)>,
    /// How it's doing, and when to open it next.
    watch: Watch,
    /// Which device it has, of every one any mic has been given (see
    /// [`next_generation`]): an open of one it no longer has (switched, or
    /// the mic taken out, while it was being opened) isn't kept.
    generation: u64,
    /// The last thing the log said about it, so a mic that stays away (or
    /// keeps stopping) is reported once, not at every try.
    said: String,
}

/// What the mics' watch knows of a mic, and decides at each look: its
/// stream aside, so the decisions can be tested without a device.
#[cfg(not(target_os = "linux"))]
#[derive(Debug, Default)]
struct Watch {
    /// Its stream's health, and the host time the stream was open at, while
    /// it's open.
    open: Option<(Arc<MicHealth>, f64)>,
    /// From when it couldn't be opened, or stopped, until its sound has kept
    /// coming again: when to try opening it next.
    retry: Option<Retry>,
    /// Its device is new (the capture just started, or it was switched):
    /// it's opened at once, and Live as soon as it's open.
    new: bool,
}

/// What the watch does about a mic, at a look.
#[cfg(not(target_os = "linux"))]
#[derive(Debug, PartialEq)]
enum Look {
    Wait,
    /// Its stream has stopped, for this reason: close it.
    Close(String),
    /// Opened again, its sound has kept coming: it's Live again.
    Back,
    /// It isn't open, and it's time to open it.
    Open,
}

#[cfg(not(target_os = "linux"))]
impl Watch {
    /// A new device: opened at once, its old retries forgotten.
    fn new_device() -> Self {
        Self { new: true, ..Self::default() }
    }

    /// A look at host time `host` (`now` on the clock retries go by).
    fn look(&mut self, now: Instant, host: f64) -> Look {
        let Some((health, opened)) = &self.open else {
            return if self.new || self.retry.is_some_and(|r| r.due(now)) { Look::Open } else { Look::Wait };
        };
        if let Some(why) = health.stopped(*opened, host) {
            self.open = None;
            // Soon after it worked; later and later while it keeps stopping
            // before its sound has kept coming.
            self.retry = Some(Retry::after(self.retry, now));
            return Look::Close(why);
        }
        // Not at its first packet: a device that gives one and stops was
        // "back" each time, its retries starting over at 1 s: opened and
        // closed every 3.25 s for good, Live and Unavailable in turn.
        if self.retry.is_some() && health.steady(host) {
            self.retry = None;
            return Look::Back;
        }
        Look::Wait
    }

    /// How opening it went (`Some`: its stream's health, and the host time
    /// the stream was open at): the status it has now.
    fn opened(&mut self, open: Option<(Arc<MicHealth>, f64)>, now: Instant) -> SourceStatus {
        let new = std::mem::take(&mut self.new);
        match open {
            Some(open) => {
                self.open = Some(open);
                // A new device is Live at once; one opened again once its
                // sound has kept coming (`Look::Back`).
                if new { SourceStatus::Live } else { SourceStatus::Unavailable }
            }
            None => {
                self.retry = Some(Retry::after(self.retry, now));
                SourceStatus::Unavailable
            }
        }
    }
}

/// A mic's open stream.
#[cfg(not(target_os = "linux"))]
struct OpenMic {
    stream: cpal::Stream,
    /// The feed it fills.
    child: Arc<SourceFeed>,
    health: Arc<MicHealth>,
    /// Host time its stream was open and started at.
    opened: f64,
}

/// A generation no mic has had: a mic's own count, kept across mics, so an
/// open of one taken out can't be taken for one added in its place.
#[cfg(not(target_os = "linux"))]
fn next_generation() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(not(target_os = "linux"))]
impl Mic {
    /// The mic of the source `id`, on `device`, not open yet: a new device,
    /// to open at once.
    fn new(id: &str, device: &str, feed: Arc<SourceFeed>, channel: Arc<crate::mixer::Channel>) -> Self {
        Self {
            id: id.to_owned(),
            device: device.to_owned(),
            feed,
            channel,
            stream: None,
            watch: Watch::new_device(),
            generation: next_generation(),
            said: String::new(),
        }
    }

    /// Give it `device`, to be opened as a new device, closing what it had.
    fn switch(&mut self, device: &str) {
        self.device = device.to_owned();
        self.generation = next_generation();
        self.watch = Watch::new_device();
        self.said.clear();
        self.close();
    }

    /// Close its stream, if it's open. The stream is let go of on a thread
    /// of its own: one stuck in its driver would hold up whoever closed it
    /// (the mics' watch, a mic switch).
    fn close(&mut self) {
        if let Some((stream, child)) = self.stream.take() {
            self.feed.remove_child(&child);
            std::thread::spawn(move || drop(stream));
        }
    }

    /// How opening its device went (see [`open_mic`]); its status follows.
    fn opened(&mut self, open: Result<OpenMic>, now: Instant) {
        self.close();
        let open = match open {
            Ok(open) => {
                self.stream = Some((open.stream, open.child));
                Some((open.health, open.opened))
            }
            Err(e) => {
                self.say(format!("{e:#}"));
                None
            }
        };
        let status = self.watch.opened(open, now);
        self.channel.set_status(status);
    }

    /// Looked at every [`Mics::EVERY`]: a stream that stopped is closed.
    /// True if it isn't open and it's time to open it, which the caller
    /// does (see [`Mics::pass`]).
    fn look(&mut self, now: Instant) -> bool {
        match self.watch.look(now, host_now()) {
            Look::Wait => false,
            Look::Open => true,
            Look::Close(why) => {
                self.say(format!("stopped ({why}); opening it again when it's back"));
                self.close();
                self.channel.set_status(SourceStatus::Unavailable);
                false
            }
            Look::Back => {
                self.channel.set_status(SourceStatus::Live);
                self.say("live again".to_owned());
                false
            }
        }
    }

    /// Log `line` about this mic, unless it's what was said last. Through
    /// [`crate::log_later`]: the mics are locked meanwhile, and a mic switch
    /// or stopping would wait for a line that waits for stderr.
    fn say(&mut self, line: String) {
        if line != self.said {
            crate::eprintln_later!("{} microphone \"{}\": {line}", chrono::Local::now().format("%H:%M:%S"), self.device);
            self.said = line;
        }
    }
}

/// The microphones of a capture, and a thread that keeps them open: a mic
/// that stops (unplugged, a USB hiccup, a driver reset) is opened again by
/// its name until it's back, its source and its track staying as they are.
/// cpal doesn't: it ends the stream on such an error, and a device opened
/// by name gets nothing that follows it.
#[cfg(not(target_os = "linux"))]
struct Mics {
    list: Arc<Mutex<Vec<Mic>>>,
    /// Wakes the watch (a mic was switched or added); dropped to stop it.
    /// None while it isn't watching: it starts with the first mic.
    wake: Option<std::sync::mpsc::Sender<()>>,
}

#[cfg(not(target_os = "linux"))]
impl Mics {
    /// How often the watch looks at the mics.
    const EVERY: Duration = Duration::from_millis(250);

    fn start(list: Vec<Mic>) -> Self {
        let watching = !list.is_empty();
        let mut mics = Self { list: Arc::new(Mutex::new(list)), wake: None };
        if watching {
            mics.watch();
        }
        mics
    }

    /// Start the watch, unless it runs.
    fn watch(&mut self) {
        if self.wake.is_some() {
            return;
        }
        let (wake, woken) = std::sync::mpsc::channel::<()>();
        let mics = self.list.clone();
        // Not waited for when the capture stops (see `stop`).
        let _ = std::thread::Builder::new().name("mic watch".into()).spawn(move || {
            while let Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) = woken.recv_timeout(Self::EVERY) {
                Self::pass(&mics, open_mic);
            }
        });
        self.wake = Some(wake);
    }

    /// Capture `mic` too, while capturing: the watch opens it, woken now.
    /// Not here: an open can take seconds (a Bluetooth headset), and
    /// whoever adds it would wait.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    fn add(&mut self, mic: Mic) {
        locked(&self.list).push(mic);
        self.watch();
        if let Some(wake) = &self.wake {
            let _ = wake.send(());
        }
    }

    /// Stop capturing the mic of the source `id`, while capturing: it's
    /// closed, and Off. False if there's no such mic.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    fn remove(&self, id: &str) -> bool {
        let mut list = locked(&self.list);
        let Some(at) = list.iter().position(|m| m.id == id) else { return false };
        let mut mic = list.remove(at);
        drop(list);
        mic.close();
        mic.channel.set_status(SourceStatus::Off);
        true
    }

    /// One look at every mic, opening those that are due with `open`. They
    /// are looked at with the list locked, and opened with it let go: an
    /// open can take seconds (a Bluetooth headset, a USB device in a bad
    /// state), and stopping the capture or switching a mic (which the app
    /// does by itself when the default mic changes, on the thread that saves
    /// clips) waited for the list meanwhile.
    fn pass(list: &Mutex<Vec<Mic>>, open: impl Fn(&str, &SourceFeed, &crate::mixer::Channel) -> Result<OpenMic>) {
        let now = Instant::now();
        let due: Vec<_> = locked(list)
            .iter_mut()
            .filter_map(|mic| mic.look(now).then(|| (mic.generation, mic.device.clone(), mic.feed.clone(), mic.channel.clone())))
            .collect();
        for (generation, device, feed, channel) in due {
            let opened = open(&device, &feed, &channel);
            let mut mics = locked(list);
            // By its generation, not its place: mics are added and taken
            // out meanwhile.
            match mics.iter_mut().find(|mic| mic.generation == generation) {
                Some(mic) => mic.opened(opened, Instant::now()),
                // Stopped, switched to another device or taken out,
                // meanwhile: what was opened goes.
                None => {
                    drop(mics);
                    if let Ok(open) = opened {
                        feed.remove_child(&open.child);
                        std::thread::spawn(move || drop(open.stream));
                    }
                }
            }
        }
    }

    /// Switch the mic of the source `id` to `device`: the watch opens it,
    /// woken now. False if there's no such mic.
    fn set(&self, id: &str, device: &str) -> bool {
        let mut list = locked(&self.list);
        let Some(mic) = list.iter_mut().find(|m| m.id == id) else { return false };
        if mic.device != device {
            mic.switch(device);
            drop(list);
            if let Some(wake) = &self.wake {
                let _ = wake.send(());
            }
        }
        true
    }

    /// Stop watching, and close every mic. The watch isn't waited for: in
    /// the middle of opening a device, it can take seconds. It ends at once
    /// otherwise; if not, once the open is done, letting go of what it
    /// opened (the list is empty by then).
    fn stop(&mut self) {
        self.wake.take();
        let mics = std::mem::take(&mut *locked(&self.list));
        drop(mics);
    }
}

/// The mics, even after a thread panicked with them locked. Stopping a
/// capture stops its audio before it finishes the file: a poisoned lock
/// made that panic, and again in `Drop` while unwinding, which aborts, and
/// the recording was never finished.
#[cfg(not(target_os = "linux"))]
fn locked(list: &Mutex<Vec<Mic>>) -> std::sync::MutexGuard<'_, Vec<Mic>> {
    list.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(not(target_os = "linux"))]
impl Drop for Mics {
    fn drop(&mut self) {
        self.stop();
    }
}

/// When to try again something that failed (opening a mic, starting a
/// desktop or app sound stream): a second later, then twice as long after
/// each failure in a row, up to every 10 s. Soon enough that a mic plugged
/// back in is heard within seconds, without asking all the time for a
/// device that's gone.
#[cfg(not(target_os = "linux"))]
#[derive(Debug, Clone, Copy)]
pub(crate) struct Retry {
    at: Instant,
    wait: Duration,
}

#[cfg(not(target_os = "linux"))]
impl Retry {
    const FIRST: Duration = Duration::from_secs(1);
    const MOST: Duration = Duration::from_secs(10);

    /// After a failure at `now`; `last` is the retry it failed at, if it
    /// had already failed.
    pub(crate) fn after(last: Option<Self>, now: Instant) -> Self {
        let wait = last.map_or(Self::FIRST, |r| (r.wait * 2).min(Self::MOST));
        Self { at: now + wait, wait }
    }

    pub(crate) fn due(&self, now: Instant) -> bool {
        now >= self.at
    }
}

/// How an open mic's stream is doing, from its callbacks. cpal ends a
/// stream after an error (an unplugged device, a driver that resets) and
/// says so once. Or, when a device just goes away, it can wait for its next
/// sound forever without a word: sound that stops coming counts too.
#[cfg(not(target_os = "linux"))]
#[derive(Debug, Default)]
struct MicHealth {
    /// What its stream said when it ended.
    ended: Mutex<Option<String>>,
    /// Host time its sound first came at (`f64` bits; 0 until it does).
    first: AtomicU64,
    /// Host time its sound last came at (`f64` bits; 0 until it first does).
    heard: AtomicU64,
}

#[cfg(not(target_os = "linux"))]
impl MicHealth {
    /// No sound for this long means it stopped: a packet comes every 10 ms
    /// on Windows (under 100 ms on a Mac), silent or not.
    const STALLED: f64 = 2.0;
    /// None at all this long after opening, too: a Bluetooth headset takes
    /// a moment to switch its mic on.
    const NEVER_CAME: f64 = 5.0;

    /// Its sound came at host time `now` (called from its stream's thread).
    fn heard(&self, now: f64) {
        if self.first.load(Ordering::Relaxed) == 0 {
            self.first.store(now.to_bits(), Ordering::Relaxed);
        }
        self.heard.store(now.to_bits(), Ordering::Relaxed);
    }

    /// Its sound has kept coming for longer than [`Self::STALLED`], at host
    /// time `now` (asked while it hasn't stopped, so with no gap that long).
    fn steady(&self, now: f64) -> bool {
        let first = f64::from_bits(self.first.load(Ordering::Relaxed));
        first > 0.0 && now - first > Self::STALLED
    }

    fn end(&self, why: String) {
        self.ended.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get_or_insert(why);
    }

    /// Why it has stopped, if it has, at host time `now`; it was opened at
    /// `opened`.
    fn stopped(&self, opened: f64, now: f64) -> Option<String> {
        if let Some(why) = self.ended.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone() {
            return Some(why);
        }
        let heard = f64::from_bits(self.heard.load(Ordering::Relaxed));
        if heard == 0.0 {
            return (now - opened > Self::NEVER_CAME).then(|| format!("no sound came in {:.0} s", now - opened));
        }
        (now - heard > Self::STALLED).then(|| format!("no sound for {:.0} s", now - heard))
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
        // Every source's feed is at the mixer's rate: desktop and app sound
        // is captured at it, and on Linux so is every mic (PipeWire converts
        // it). Elsewhere a mic's device fills a feed of its own inside its
        // source's, at the rate it's opened at (see `open_mic`).
        let feeds: Vec<Arc<SourceFeed>> = sources.iter().map(|_| SourceFeed::new(crate::mixer::RATE, clock.clone())).collect();
        #[cfg(not(target_os = "linux"))]
        let mut mics = Vec::new();
        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
        let mut system_sources = Vec::new();
        for (source, feed) in sources.iter().zip(&feeds) {
            let channel = live.channel(&source.id);
            #[cfg(target_os = "linux")]
            if matches!(source.kind, SourceKind::Microphone { .. }) {
                feed.mono_with(channel.mono_flag());
                feed.denoise_with(channel.denoise_mode());
            }
            match &source.kind {
                #[cfg(not(target_os = "linux"))]
                SourceKind::Microphone { device } => {
                    let mut mic = Mic::new(&source.id, device, feed.clone(), channel);
                    let opened = open_mic(&mic.device, &mic.feed, &mic.channel);
                    mic.opened(opened, Instant::now());
                    mics.push(mic);
                }
                // Filled by its video as it plays.
                SourceKind::Video { .. } => {
                    video_sound::listen(&source.id, feed);
                    channel.set_status(crate::mixer::SourceStatus::Live);
                }
                #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
                _ => system_sources.push((source.clone(), feed.clone(), channel)),
                #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
                _ => channel.set_status(SourceStatus::Unavailable),
            }
        }
        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
        let system = if system_sources.is_empty() { None } else { Some(SystemAudio::start(system_sources)?) };
        let capture = Self {
            captured: sources.iter().cloned().zip(feeds.iter().cloned()).collect(),
            feeds,
            clock,
            #[cfg(not(target_os = "linux"))]
            mics: Mics::start(mics),
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            system,
        };
        capture.route_own_sound();
        Ok(capture)
    }

    /// HasteClip' own sound into the sources that take it: its app source
    /// if it has one, else the desktop (unless the desktop leaves out apps
    /// with their own source).
    fn route_own_sound(&self) {
        let own = own_app_id();
        let own_source = self.captured.iter().any(|(s, _)| is_own_app(&s.kind, own.as_deref()));
        for (source, feed) in &self.captured {
            let takes = match source.kind {
                SourceKind::Desktop { exclude_app_sources } => !(own_source && exclude_app_sources),
                ref kind => is_own_app(kind, own.as_deref()),
            };
            if takes {
                own_sound::listen(feed);
            } else {
                own_sound::forget(feed);
            }
        }
    }

    /// Capture `sources` from now on, without stopping anything: one not
    /// captured yet starts (it isn't waited for: a mic can take seconds to
    /// open), one captured that isn't among them stops, its feed taking no
    /// more sound, and a microphone given another device switches to it (as
    /// [`Self::set_mic`]). Anything else about one captured already (where
    /// it goes, which apps it leaves out) stays as it started. The sources
    /// added, with their feeds, and those taken out, for the mixer.
    #[cfg(target_os = "windows")]
    pub(crate) fn set_sources(&mut self, sources: &[AudioSource], live: &LiveAudio) -> (Vec<(AudioSource, Arc<SourceFeed>)>, Vec<AudioSource>) {
        let mut gone = Vec::new();
        self.captured.retain(|(s, feed)| {
            let keep = sources.iter().any(|n| n.id == s.id);
            if !keep {
                gone.push((s.clone(), feed.clone()));
            }
            keep
        });
        for (source, feed) in &gone {
            self.stop_one(source, feed);
        }
        let mut added = Vec::new();
        for source in sources {
            match self.captured.iter().position(|(s, _)| s.id == source.id) {
                None => added.push((source.clone(), self.start_one(source, live))),
                Some(at) => {
                    if let (SourceKind::Microphone { device: was }, SourceKind::Microphone { device }) = (&self.captured[at].0.kind, &source.kind)
                        && was != device
                        && self.set_mic(&source.id, device)
                    {
                        self.captured[at].0.kind = source.kind.clone();
                    }
                }
            }
        }
        if !gone.is_empty() || !added.is_empty() {
            self.route_own_sound();
        }
        (added, gone.into_iter().map(|(s, _)| s).collect())
    }

    /// Start capturing `source` too, while capturing: its feed.
    #[cfg(target_os = "windows")]
    fn start_one(&mut self, source: &AudioSource, live: &LiveAudio) -> Arc<SourceFeed> {
        let feed = SourceFeed::new(crate::mixer::RATE, self.clock.clone());
        let channel = live.channel(&source.id);
        match &source.kind {
            SourceKind::Microphone { device } => self.mics.add(Mic::new(&source.id, device, feed.clone(), channel)),
            SourceKind::Video { .. } => {
                video_sound::listen(&source.id, &feed);
                channel.set_status(SourceStatus::Live);
            }
            // Desktop or app sound, to a capture of it started now if
            // there's none (started empty: nothing's waited for).
            _ => {
                if self.system.is_none() {
                    match SystemAudio::start(Vec::new()) {
                        Ok(system) => self.system = Some(system),
                        Err(e) => eprintln!("audio source \"{}\": {e:#}", source.name),
                    }
                }
                match &self.system {
                    Some(system) => system.add(source.clone(), feed.clone(), channel),
                    None => channel.set_status(SourceStatus::Unavailable),
                }
            }
        }
        self.captured.push((source.clone(), feed.clone()));
        feed
    }

    /// Stop capturing `source`, whose feed is `feed`, while capturing: what
    /// its feed has is still there to mix.
    #[cfg(target_os = "windows")]
    fn stop_one(&mut self, source: &AudioSource, feed: &Arc<SourceFeed>) {
        feed.close();
        own_sound::forget(feed);
        match source.kind {
            SourceKind::Microphone { .. } => {
                self.mics.remove(&source.id);
            }
            SourceKind::Video { .. } => video_sound::forget(feed),
            _ => {
                if let Some(system) = &self.system {
                    system.remove(&source.id);
                }
            }
        }
    }

    /// Switch the microphone of the source `id` to `device`, without stopping
    /// anything. False if that source isn't a mic. On Linux a mic is a
    /// PipeWire stream, made again aimed at the new device.
    pub(crate) fn set_mic(&mut self, id: &str, device: &str) -> bool {
        #[cfg(not(target_os = "linux"))]
        if self.mics.set(id, device) {
            return true;
        }
        #[cfg(target_os = "linux")]
        if let Some(system) = self.system.as_mut() {
            return system.set_mic(id, device);
        }
        let _ = (id, device);
        false
    }

    pub(crate) fn stop(mut self) {
        for (_, feed) in &self.captured {
            own_sound::forget(feed);
            video_sound::forget(feed);
        }
        #[cfg(not(target_os = "linux"))]
        self.mics.stop();
        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
        if let Some(system) = self.system.take() {
            system.stop();
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn find_input(name: &str) -> Option<cpal::Device> {
    cpal::default_host()
        .input_devices()
        .ok()?
        .find(|d| d.description().is_ok_and(|desc| desc.name() == name))
}

/// Open the input device called `name` into a new feed inside `feed` (its
/// mic source's), made at the rate the device is opened at.
#[cfg(not(target_os = "linux"))]
fn open_mic(name: &str, feed: &SourceFeed, channel: &crate::mixer::Channel) -> Result<OpenMic> {
    let device = find_input(name).ok_or_else(|| anyhow::anyhow!("not connected"))?;
    let config = device.default_input_config()?;
    // The feed's rate comes from this same look at the device. Looked up
    // twice, once for the rate and once to open it, a device that was just
    // arriving could be missed the first time and taken as 48 kHz, then
    // found and opened at 44.1 kHz: its sound read 8.8% fast for as long as
    // it played, a packet thrown away every half second or so.
    let child = feed.add_child(config.sample_rate());
    child.mono_with(channel.mono_flag());
    child.denoise_with(channel.denoise_mode());
    let health = Arc::new(MicHealth::default());
    match start_mic(name, &device, config, child.clone(), health.clone()) {
        // Its first sound is given its time from when it's open: an open
        // that takes seconds (a Bluetooth headset) used most of it up.
        Ok(stream) => Ok(OpenMic { stream, child, health, opened: host_now() }),
        Err(e) => {
            feed.remove_child(&child);
            Err(e)
        }
    }
}

/// `device`'s stream, as `config` says, into `feed`.
#[cfg(not(target_os = "linux"))]
fn start_mic(name: &str, device: &cpal::Device, config: cpal::SupportedStreamConfig, feed: Arc<SourceFeed>, health: Arc<MicHealth>) -> Result<cpal::Stream> {
    let channels = config.channels() as usize;
    let mut stream_config = config.config();
    // Room for 200 ms of backlog. By default the device's buffer holds only
    // ~10-20 ms, so the reading thread being scheduled a little late (a game
    // pinning the CPU) overflowed it and lost audio. It still wakes every
    // ~10 ms, and every packet is placed by its timestamp, so this adds no delay.
    let backlog = config.sample_rate() / 5;
    let rate = config.sample_rate() as f64;
    stream_config.buffer_size = match config.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } => cpal::BufferSize::Fixed(backlog.clamp(*min, *max)),
        cpal::SupportedBufferSize::Unknown => cpal::BufferSize::Fixed(backlog),
    };
    let errors = mic_errors(name.to_owned(), health.clone());
    #[cfg(target_os = "windows")]
    let mut scheduled = false;
    let stream = device.build_input_stream::<f32, _, _>(
        stream_config,
        move |data, info| {
            // cpal reads the mic on a thread at normal priority (its
            // `realtime` feature is off), and noise removal runs on it, on
            // every packet: while a game kept every core busy, a USB mic
            // wasn't read in time twice in 6 minutes, and sound was lost. The
            // first time through, Windows' multimedia scheduler takes the
            // thread on as an audio thread, as the desktop and app sound
            // threads already are (see `win::loopback`).
            #[cfg(target_os = "windows")]
            if !std::mem::replace(&mut scheduled, true) {
                crate::win::system::multimedia_thread("Audio");
            }
            let now = host_now();
            health.heard(now);
            let stamped = info.timestamp().capture.as_nanos() as f64 / 1e9;
            feed.push(capture_time(stamped, now, (data.len() / channels) as f64 / rate), data, channels);
        },
        errors,
        None,
    )?;
    stream.play()?;
    Ok(stream)
}

/// When a mic's packet of `length` seconds, stamped `stamped`, was captured,
/// at host time `now`: at its stamp, unless that can't be right, and then
/// as if it had just been. Sound isn't captured in the future (a few ms are
/// let pass for the clocks' rounding), nor handed over a second after, but
/// a driver can give no time (0) or one that jumps: one that jumped ahead
/// put silence in up to it, and then threw real sound away until real time
/// caught up. The same check as desktop and app sound's (`win::loopback`),
/// which lets more of the future pass.
#[cfg(not(target_os = "linux"))]
fn capture_time(stamped: f64, now: f64, length: f64) -> f64 {
    if stamped > now - 1.0 && stamped < now + 0.005 { stamped } else { now - length }
}

/// A recording mic's error handler: an error that ends its stream goes to
/// `health` (the mics' watch then opens it again, see [`Mics`]); the rest
/// to [`quiet_xruns`]. Every error ends the stream but a dropout, the
/// default device changing (the stream follows it) and being refused
/// real-time priority: cpal stops reading after any other.
#[cfg(not(target_os = "linux"))]
fn mic_errors(name: String, health: Arc<MicHealth>) -> impl FnMut(cpal::Error) + Send + 'static {
    let mut others = quiet_xruns(name);
    move |e| match e.kind() {
        cpal::ErrorKind::Xrun | cpal::ErrorKind::DeviceChanged | cpal::ErrorKind::RealtimeDenied => others(e),
        _ => health.end(e.to_string()),
    }
}

/// The mic's error handler. Windows marks the first packets after a stream
/// starts as a gap (an "underrun or overrun"), which isn't lost audio: that's
/// ignored. A real dropout later — the mic's buffer filled before it was read,
/// usually because the PC was too busy — is reported, at most once a minute.
/// It runs on the thread that reads the mic, so its lines go to the log
/// through [`crate::log_later`]: a line written straight to stderr waits for
/// wherever that goes (11-20 ms to a file, as long as text is selected in a
/// terminal), and the mic isn't read meanwhile.
#[cfg(not(target_os = "linux"))]
pub(crate) fn quiet_xruns(name: String) -> impl FnMut(cpal::Error) + Send + 'static {
    let started = std::time::Instant::now();
    let mut dropouts = 0u32;
    let mut reported: Option<std::time::Instant> = None;
    move |e| {
        if !matches!(e.kind(), cpal::ErrorKind::Xrun) {
            crate::eprintln_later!("microphone \"{name}\": {e}");
            return;
        }
        if started.elapsed() < std::time::Duration::from_secs(1) {
            return;
        }
        dropouts += 1;
        if reported.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(60)) {
            crate::eprintln_later!("microphone \"{name}\": {dropouts} short dropout(s) — it wasn't read in time (the PC was busy)");
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

    /// `feed` takes HasteClip' sound (once, however often it's said).
    pub(crate) fn listen(feed: &Arc<SourceFeed>) {
        let mut d = listeners().lock().unwrap();
        d.retain(|w| w.strong_count() > 0);
        if !d.iter().any(|w| std::ptr::eq(w.as_ptr(), Arc::as_ptr(feed))) {
            d.push(Arc::downgrade(feed));
            GENERATION.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// `feed` doesn't take HasteClip' sound (any more).
    pub(crate) fn forget(feed: &Arc<SourceFeed>) {
        let mut d = listeners().lock().unwrap();
        let had = d.len();
        d.retain(|w| w.strong_count() > 0 && !std::ptr::eq(w.as_ptr(), Arc::as_ptr(feed)));
        if d.len() != had {
            GENERATION.fetch_add(1, Ordering::Relaxed);
        }
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

/// Video sources' sound: each video layer with sound plays its file's sound,
/// as it shows its picture, into the feeds of the audio source it names
/// ([`SourceKind::Video`]): the meters' capture's, a recording's, those
/// that start while it plays too.
pub mod video_sound {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, Weak};

    use crate::mixer::SourceFeed;

    /// Each video source's feeds, by its id; bumped generation on change.
    fn listeners() -> &'static Mutex<Vec<(String, Weak<SourceFeed>)>> {
        static LISTENERS: Mutex<Vec<(String, Weak<SourceFeed>)>> = Mutex::new(Vec::new());
        &LISTENERS
    }
    static GENERATION: AtomicU64 = AtomicU64::new(0);

    /// `feed` takes the sound of the video source `id` (once, however
    /// often it's said).
    pub(crate) fn listen(id: &str, feed: &Arc<SourceFeed>) {
        let mut d = listeners().lock().unwrap();
        d.retain(|(_, w)| w.strong_count() > 0);
        if !d.iter().any(|(_, w)| std::ptr::eq(w.as_ptr(), Arc::as_ptr(feed))) {
            d.push((id.to_owned(), Arc::downgrade(feed)));
            GENERATION.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// `feed` takes no video's sound (any more).
    pub(crate) fn forget(feed: &Arc<SourceFeed>) {
        let mut d = listeners().lock().unwrap();
        let had = d.len();
        d.retain(|(_, w)| w.strong_count() > 0 && !std::ptr::eq(w.as_ptr(), Arc::as_ptr(feed)));
        if d.len() != had {
            GENERATION.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// One video's sound being played into the feeds of the source `id`,
    /// at `rate`.
    pub struct Playing {
        id: String,
        rate: u32,
        generation: u64,
        children: Vec<(Weak<SourceFeed>, Arc<SourceFeed>)>,
    }

    impl Playing {
        pub fn new(id: &str, rate: u32) -> Self {
            let mut playing = Self { id: id.to_owned(), rate, generation: u64::MAX, children: Vec::new() };
            playing.follow();
            playing
        }

        /// Into the feeds of its source right now.
        fn follow(&mut self) {
            let generation = GENERATION.load(Ordering::Relaxed);
            if generation == self.generation {
                return;
            }
            self.generation = generation;
            self.leave();
            let children = listeners().lock().unwrap().iter().filter(|(id, _)| *id == self.id).filter_map(|(_, w)| Some((w.clone(), w.upgrade()?.add_child(self.rate)))).collect();
            self.children = children;
        }

        fn leave(&mut self) {
            for (parent, child) in self.children.drain(..) {
                if let Some(p) = parent.upgrade() {
                    p.remove_child(&child);
                }
            }
        }

        /// Interleaved samples, `channels` a frame, the first heard at host
        /// time `at`.
        pub fn push_at(&mut self, at: f64, samples: &[f32], channels: usize) {
            self.follow();
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
            id: s.id.clone(),
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

    /// A thread that panicked with the mics locked doesn't take stopping the
    /// capture down with it (nor dropping them, while unwinding).
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn mics_stop_though_a_thread_panicked_holding_them() {
        let mut mics = Mics::start(Vec::new());
        let list = mics.list.clone();
        let _ = std::thread::spawn(move || {
            let _held = list.lock().unwrap();
            panic!("a panic with the mics locked (this test's own)");
        })
        .join();
        assert!(mics.list.is_poisoned());
        assert!(!mics.set("a mic", "another"));
        mics.stop();
        drop(mics);
    }

    /// The watch opens a mic with the list let go: an open that takes its
    /// time holds up neither a mic switch nor stopping, and the outcome for
    /// a device the mic no longer has isn't kept. (The opener here takes
    /// 600 ms and finds nothing: no device is touched.)
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn a_slow_open_holds_up_neither_a_switch_nor_stopping() {
        let live = LiveAudio::new();
        let mic = Mic {
            id: "mic".into(),
            device: "a slow mic".into(),
            feed: SourceFeed::new(crate::mixer::RATE, Arc::new(Clock::default())),
            channel: live.channel("mic"),
            stream: None,
            watch: Watch::new_device(),
            generation: 0,
            said: String::new(),
        };
        let mut mics = Mics { list: Arc::new(Mutex::new(vec![mic])), wake: None };
        let slow_pass = |list: Arc<Mutex<Vec<Mic>>>, opening: std::sync::mpsc::Sender<String>| {
            std::thread::spawn(move || {
                Mics::pass(&list, |device: &str, _: &SourceFeed, _: &crate::mixer::Channel| {
                    opening.send(device.to_owned()).unwrap();
                    std::thread::sleep(Duration::from_millis(600));
                    Err(anyhow::anyhow!("not connected"))
                })
            })
        };
        let (opening, being_opened) = std::sync::mpsc::channel();
        let pass = slow_pass(mics.list.clone(), opening);
        assert_eq!(being_opened.recv().unwrap(), "a slow mic");
        let t = Instant::now();
        assert!(mics.set("mic", "another mic"));
        assert!(t.elapsed() < Duration::from_millis(200), "the switch waited {:?}", t.elapsed());
        pass.join().unwrap();
        {
            let list = locked(&mics.list);
            assert_eq!(list[0].device, "another mic");
            assert!(list[0].watch.new && list[0].watch.retry.is_none(), "the old device's outcome was kept: {:?}", list[0].watch);
        }
        let (opening, being_opened) = std::sync::mpsc::channel();
        let pass = slow_pass(mics.list.clone(), opening);
        assert_eq!(being_opened.recv().unwrap(), "another mic");
        let t = Instant::now();
        mics.stop();
        assert!(t.elapsed() < Duration::from_millis(200), "stopping waited {:?}", t.elapsed());
        pass.join().unwrap();
        assert!(locked(&mics.list).is_empty());
    }

    /// A mic taken out while the watch opens it, and added again in its
    /// place (its source turned off and on): how that open went isn't taken
    /// for the new one, which is opened afresh. Taken out, it's Off. (The
    /// opener finds nothing: no device is touched.)
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn a_mic_taken_out_while_it_opens_leaves_the_one_added_back_alone() {
        let live = LiveAudio::new();
        let clock = Arc::new(Clock::default());
        let channel = live.channel("mic");
        let mic = Mic::new("mic", "a slow mic", SourceFeed::new(crate::mixer::RATE, clock.clone()), channel.clone());
        let mics = Mics { list: Arc::new(Mutex::new(vec![mic])), wake: None };
        let (opening, being_opened) = std::sync::mpsc::channel();
        let list = mics.list.clone();
        let pass = std::thread::spawn(move || {
            Mics::pass(&list, |device: &str, _: &SourceFeed, _: &crate::mixer::Channel| {
                opening.send(device.to_owned()).unwrap();
                std::thread::sleep(Duration::from_millis(300));
                Err(anyhow::anyhow!("not connected"))
            })
        });
        assert_eq!(being_opened.recv().unwrap(), "a slow mic");
        channel.set_status(SourceStatus::Live);
        assert!(mics.remove("mic"));
        assert_eq!(channel.status(), SourceStatus::Off);
        assert!(!mics.remove("mic"), "taken out already");
        // Added back as `Mics::add` does, but for starting the watch.
        locked(&mics.list).push(Mic::new("mic", "a slow mic", SourceFeed::new(crate::mixer::RATE, clock), channel));
        pass.join().unwrap();
        let list = locked(&mics.list);
        assert_eq!(list.len(), 1);
        assert!(list[0].watch.new && list[0].watch.retry.is_none(), "the one taken out's open was taken for it: {:?}", list[0].watch);
    }

    /// HasteClip' sound goes into a source that takes it once, however often
    /// that's said (the sources are routed again whenever one is added or
    /// taken out while capturing), and none goes in once it's forgotten.
    #[test]
    fn own_sound_goes_into_a_source_once() {
        let clock = Arc::new(Clock::default());
        clock.set(host_now());
        let feed = SourceFeed::new(crate::mixer::RATE, clock);
        own_sound::listen(&feed);
        own_sound::listen(&feed);
        let mut playing = own_sound::Playing::start();
        playing.push(&[0.25; 960]);
        let loudest = feed.peek(0, 60 * crate::mixer::RATE as usize).iter().map(|f| f[0]).fold(0.0, f32::max);
        assert_eq!(loudest, 0.25, "in it once");
        let arrived = feed.arrived();
        own_sound::forget(&feed);
        std::thread::sleep(Duration::from_millis(20));
        playing.push(&[0.25; 960]);
        assert_eq!(feed.arrived(), arrived, "none after it's forgotten");
    }

    /// A mic that's gone is tried again a second later, then less and less
    /// often, down to every 10 s.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn retries_wait_longer_each_time_up_to_ten_seconds() {
        let t = Instant::now();
        let mut retry = Retry::after(None, t);
        assert!(!retry.due(t + Duration::from_millis(999)));
        assert!(retry.due(t + Duration::from_secs(1)));
        let mut waits = Vec::new();
        for _ in 0..6 {
            retry = Retry::after(Some(retry), t);
            waits.push((retry.at - t).as_secs());
        }
        assert_eq!(waits, [2, 4, 8, 10, 10, 10]);
    }

    /// An error that ends the stream (unplugged) stops the mic, and its
    /// message says why; a dropout, or the default device changing, doesn't.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn a_mic_stops_on_an_error_that_ends_its_stream() {
        let health = Arc::new(MicHealth::default());
        let mut errors = mic_errors("test mic".into(), health.clone());
        health.heard(100.0);
        errors(cpal::Error::new(cpal::ErrorKind::Xrun));
        errors(cpal::Error::new(cpal::ErrorKind::DeviceChanged));
        assert_eq!(health.stopped(99.0, 100.5), None);
        errors(cpal::Error::with_message(cpal::ErrorKind::DeviceNotAvailable, "unplugged"));
        errors(cpal::Error::with_message(cpal::ErrorKind::BackendError, "and then this"));
        assert_eq!(health.stopped(99.0, 100.5).as_deref(), Some("unplugged"), "the first error is the reason");
    }

    /// A mic's packet with no time, or one from the future or from long
    /// ago, is taken as just captured. One stamped half a second ahead put
    /// half a second of silence in, then threw real sound away until real
    /// time caught up; now the sound goes on as it came.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn mic_times_that_cant_be_right_are_not_used() {
        assert_eq!(capture_time(99.98, 100.0, 0.01), 99.98);
        for wrong in [0.0, 100.5, 98.5] {
            assert_eq!(capture_time(wrong, 100.0, 0.01), 99.99, "{wrong}");
        }
        let clock = Arc::new(Clock::default());
        clock.set(1000.0);
        let feed = SourceFeed::new(crate::mixer::RATE, clock);
        // A second of a steady level in 10 ms packets, each handed over a
        // couple of ms after its last frame; the 50th stamped 0.5 s ahead.
        for k in 0..100 {
            let captured = 1000.0 + k as f64 * 0.01;
            let stamped = if k == 50 { captured + 0.5 } else { captured };
            feed.push(capture_time(stamped, captured + 0.012, 0.01), &[0.5; 960], 2);
        }
        let out = feed.take(0, crate::mixer::RATE as usize);
        assert!(out.iter().all(|f| f[0] == 0.5), "silence at {:?}", out.iter().position(|f| f[0] != 0.5));
    }

    /// cpal can wait forever for a device that went away, without an error:
    /// no sound for 2 s stops the mic too, and none 5 s after opening.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn a_mic_whose_sound_stops_coming_has_stopped() {
        let health = MicHealth::default();
        // Opened at 100 s, nothing yet: a moment to start.
        assert_eq!(health.stopped(100.0, 104.9), None);
        assert!(health.stopped(100.0, 105.1).is_some());
        // Sound every 10 ms, then none.
        for k in 0..100 {
            health.heard(105.0 + k as f64 * 0.01);
        }
        assert_eq!(health.stopped(100.0, 107.9), None);
        assert_eq!(health.stopped(100.0, 108.1).as_deref(), Some("no sound for 2 s"));
    }

    /// A device that gives one packet and stops, again and again: it's
    /// tried later and later (1, 2, 4, 8, then every 10 s after it stops),
    /// Unavailable meanwhile, and never "back". One whose sound keeps coming
    /// for over 2 s once opened again is back.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn a_mic_is_back_once_its_sound_keeps_coming() {
        let t = Instant::now();
        let at = |s: f64| t + Duration::from_secs_f64(s);
        // Looked at every 250 ms for 100 s: opened at once (a new device:
        // Live), one packet 10 ms after each open, then nothing.
        let mut watch = Watch::new_device();
        let mut status = SourceStatus::Off;
        let mut opens = Vec::new();
        for k in 0..=400 {
            let s = k as f64 * 0.25;
            match watch.look(at(s), s) {
                Look::Open => {
                    opens.push(s);
                    let health = Arc::new(MicHealth::default());
                    status = watch.opened(Some((health.clone(), s)), at(s));
                    health.heard(s + 0.01);
                }
                Look::Close(_) => status = SourceStatus::Unavailable,
                Look::Back => panic!("back at {s}, after a single packet"),
                Look::Wait => {}
            }
            if k == 0 {
                assert_eq!(status, SourceStatus::Live, "a new device, opened");
            } else if s > 2.5 {
                assert_eq!(status, SourceStatus::Unavailable, "at {s} s");
            }
        }
        // Each stops 2.25 s after it opens (no sound for 2 s, seen at the
        // next look), then waits its retry.
        let gaps: Vec<f64> = opens.windows(2).map(|w| w[1] - w[0]).collect();
        assert_eq!(gaps[..6], [3.25, 4.25, 6.25, 10.25, 12.25, 12.25]);

        // Opened again after it stopped, with its sound coming: Unavailable
        // until it has come for over 2 s, then back, its retries forgotten.
        let mut watch = Watch { retry: Some(Retry::after(None, at(0.0))), ..Watch::default() };
        assert_eq!(watch.look(at(1.0), 1.0), Look::Open);
        let health = Arc::new(MicHealth::default());
        assert_eq!(watch.opened(Some((health.clone(), 1.0)), at(1.0)), SourceStatus::Unavailable);
        for k in 1..=200 {
            health.heard(1.0 + k as f64 * 0.01);
        }
        assert_eq!(watch.look(at(3.0), 3.0), Look::Wait, "its sound has come for 1.99 s");
        health.heard(3.1);
        assert_eq!(watch.look(at(3.1), 3.1), Look::Back);
        assert!(watch.retry.is_none());
    }

    /// Desktop sound added to a capture that started without it, then taken
    /// out: live within a moment, its feed filling; Off once taken out.
    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "captures real system audio; run with --ignored"]
    fn desktop_sound_added_while_capturing_goes_live() {
        let live = LiveAudio::new();
        let clock = Arc::new(Clock::default());
        clock.set(host_now());
        let mut capture = AudioCapture::start(&[], &live, clock).unwrap();
        let desktop = src("Desktop", true, false);
        let (added, gone) = capture.set_sources(std::slice::from_ref(&desktop), &live);
        assert_eq!((added.len(), gone.len()), (1, 0));
        let channel = live.channel("Desktop");
        let until = |status: SourceStatus| {
            let t = Instant::now();
            while channel.status() != status && t.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(Duration::from_millis(10));
            }
            t.elapsed()
        };
        let took = until(SourceStatus::Live);
        eprintln!("desktop sound live {took:?} after it was added");
        assert_eq!(channel.status(), SourceStatus::Live);
        std::thread::sleep(Duration::from_millis(300));
        assert!(added[0].1.arrived().is_some(), "its sound is coming");
        let (added, gone) = capture.set_sources(&[], &live);
        assert_eq!((added.len(), gone.len()), (0, 1));
        let took = until(SourceStatus::Off);
        eprintln!("desktop sound off {took:?} after it was taken out");
        assert_eq!(channel.status(), SourceStatus::Off);
        capture.stop();
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

    /// A video's sound reaches the source of its id, at its own rate, and
    /// no other: no device is opened for it (it's filled by its video).
    #[test]
    fn a_videos_sound_reaches_its_source() {
        let live = LiveAudio::new();
        let video = AudioSource { kind: SourceKind::Video { id: "video-7".into() }, ..src("video-7", true, false) };
        let other = AudioSource { kind: SourceKind::Video { id: "video-8".into() }, ..src("video-8", true, false) };
        let _monitor = LevelMonitor::start(&[video, other], live.clone()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let (channel, quiet) = (live.channel("video-7"), live.channel("video-8"));
        assert_eq!(channel.status(), SourceStatus::Live);
        // Half a second of a loud tone at 44.1 kHz, each block when it's heard.
        let mut playing = video_sound::Playing::new("video-7", 44_100);
        let block: Vec<f32> = (0..441).flat_map(|i| [0.8 * (i as f32 * 0.1).sin(); 2]).collect();
        for _ in 0..50 {
            playing.push_at(host_now(), &block, 2);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        let (heard, not) = (channel.input.take().max_peak(), quiet.input.take().max_peak());
        assert!(heard > 0.7, "{heard}");
        assert!(not < 0.01, "{not}");
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
