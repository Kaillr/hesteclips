//! Desktop and app audio on Windows, via WASAPI process loopback (Windows 10
//! 2004+): a stream captures everything one process tree plays (include), or
//! everything except one process tree (exclude).
//!
//! - **App source**: one include stream per top process of the app, so its
//!   helpers (browser tabs, Discord's voice process) come along.
//! - **Desktop**: everything except HasteClip itself (a mic you "Listen" to
//!   would be heard twice); what it plays for you is added back in by
//!   `sources::own_sound`, as is a HasteClip app source's sound.
//! - **Desktop minus app sources** (or minus programs left out of it, e.g. an
//!   audio router like FXSound that plays every app's sound again): WASAPI
//!   can exclude only one tree, so while any of those runs, the desktop is
//!   rebuilt from an include stream per process that has an audio session —
//!   minus them, minus us. Which programs it records is shown on its
//!   channel ([`Channel::programs`]).
//! - **Output device**: one endpoint loopback stream, what that device plays,
//!   mixed (HasteClip's own sounds on it too).
//!
//! Each stream feeds a child of its source's [`SourceFeed`], placed on the QPC
//! clock by its own timestamps. A thread rescans every second so app sources
//! start by themselves when their app opens and follow it across relaunches.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
use windows::Win32::System::Com::{BLOB, CLSCTX_ALL, CoCreateInstance};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{HSTRING, IUnknown, Interface, implement};

use super::system::{ProcessTree, audio_session_pids, com_init, host_now, program_name, render_device_id};
use crate::mixer::{Channel, Program, RATE, SourceFeed, SourceStatus};
use crate::sources::{AudioSource, Retry, SourceKind};

/// What one loopback stream captures.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Target {
    Include(u32),
    Exclude(u32),
    /// What an output device plays, by its endpoint id.
    Endpoint(String),
}

/// One running process-loopback capture.
struct Stream {
    stop: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Stream {
    fn start(target: Target, feed: Arc<SourceFeed>) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let (stop2, alive2) = (stop.clone(), alive.clone());
        let thread = thread::spawn(move || {
            com_init();
            super::system::multimedia_thread("Audio");
            match open(&target) {
                Ok((client, capture)) => {
                    let _ = ready_tx.send(Ok(()));
                    if let Err(e) = pump(&client, &capture, &feed, &stop2) {
                        eprintln!("audio capture {target:?} ended: {e}");
                    }
                    unsafe {
                        let _ = client.Stop();
                    }
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            }
            alive2.store(false, Ordering::Relaxed);
        });
        let ready = ready_rx.recv_timeout(Duration::from_secs(5)).unwrap_or_else(|_| Err(anyhow!("timed out")));
        let stream = Self { stop, alive, thread: Some(thread) };
        ready.map(|()| stream)
    }

    fn alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Activate and start a loopback client: 48 kHz stereo float, which Windows
/// converts to from whatever the apps (or the device) play.
fn open(target: &Target) -> Result<(IAudioClient, IAudioCaptureClient)> {
    let client = match *target {
        Target::Include(pid) => process_client(pid, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE)?,
        Target::Exclude(pid) => process_client(pid, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE)?,
        Target::Endpoint(ref id) => unsafe {
            let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device = enumerator.GetDevice(&HSTRING::from(id.as_str())).context("the output device isn't connected")?;
            device.Activate::<IAudioClient>(CLSCTX_ALL, None)?
        },
    };
    let format = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
        nChannels: 2,
        nSamplesPerSec: RATE,
        nAvgBytesPerSec: RATE * 8,
        nBlockAlign: 8,
        wBitsPerSample: 32,
        cbSize: 0,
    };
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            2_000_000, // 200 ms buffer
            0,
            &format,
            None,
        )?;
        let capture: IAudioCaptureClient = client.GetService()?;
        Ok((client, capture))
    }
}

/// A process-loopback client of `pid`'s tree, activated (not initialized).
fn process_client(pid: u32, mode: PROCESS_LOOPBACK_MODE) -> Result<IAudioClient> {
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS { TargetProcessId: pid, ProcessLoopbackMode: mode },
        },
    };
    // ManuallyDrop: PROPVARIANT's Drop would PropVariantClear the blob, i.e.
    // CoTaskMemFree a pointer to `params` on our stack.
    let mut prop = std::mem::ManuallyDrop::new(PROPVARIANT::default());
    unsafe {
        let inner = &mut *prop.Anonymous.Anonymous;
        inner.vt = VT_BLOB;
        inner.Anonymous.blob = BLOB { cbSize: std::mem::size_of_val(&params) as u32, pBlobData: &params as *const _ as *mut u8 };
    }
    let (tx, rx) = mpsc::channel();
    let handler: IActivateAudioInterfaceCompletionHandler = Activated { tx: Mutex::new(Some(tx)) }.into();
    let _op = unsafe { ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&*prop), &handler) }
        .context("process audio capture isn't available (needs Windows 10 version 2004 or later)")?;
    let client: IAudioClient = rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| anyhow!("audio activation timed out"))?
        .map_err(|e| anyhow!("audio activation failed: {e}"))?
        .0
        .cast()?;
    Ok(client)
}

/// Read packets until stopped, pushing each at its capture time.
fn pump(client: &IAudioClient, capture: &IAudioCaptureClient, feed: &SourceFeed, stop: &AtomicBool) -> Result<()> {
    unsafe {
        let event = CreateEventW(None, false, false, None)?;
        let result = (|| -> Result<()> {
            client.SetEventHandle(event)?;
            client.Start()?;
            let mut silence = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                // Read on a timeout too: a device's loopback sends nothing
                // while it plays nothing, and its event isn't promised.
                let _ = WaitForSingleObject(event, 100) == WAIT_OBJECT_0;
                while capture.GetNextPacketSize()? > 0 {
                    let mut data = std::ptr::null_mut();
                    let mut frames = 0u32;
                    let mut flags = 0u32;
                    let mut qpc = 0u64;
                    capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
                    let n = frames as usize * 2;
                    let samples: &[f32] = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                        silence.resize(n, 0.0);
                        &silence
                    } else {
                        std::slice::from_raw_parts(data as *const f32, n)
                    };
                    // The QPC time of the packet's first frame; estimate it if
                    // Windows didn't give a usable one.
                    let now = host_now();
                    let stamped = qpc as f64 / 1e7;
                    let start = if qpc != 0 && flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0 && (now - stamped).abs() < 1.0 {
                        stamped
                    } else {
                        now - frames as f64 / RATE as f64
                    };
                    feed.push(start, samples, 2);
                    capture.ReleaseBuffer(frames)?;
                }
            }
            Ok(())
        })();
        let _ = CloseHandle(event);
        result
    }
}

/// The activated interface, moved off the completion thread.
struct SendUnknown(IUnknown);
unsafe impl Send for SendUnknown {}

// windows-rs objects are agile already, as this callback must be.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated {
    tx: Mutex<Option<mpsc::Sender<Result<SendUnknown, String>>>>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, op: windows::core::Ref<IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        let result = (|| -> Result<SendUnknown, String> {
            let op = op.ok().map_err(|e| e.message())?;
            let mut hr = windows::core::HRESULT(0);
            let mut unknown = None;
            unsafe { op.GetActivateResult(&mut hr, &mut unknown) }.map_err(|e| e.message())?;
            hr.ok().map_err(|e| e.message())?;
            unknown.map(SendUnknown).ok_or_else(|| "no interface".to_owned())
        })();
        if let Some(tx) = self.tx.lock().unwrap().take() {
            let _ = tx.send(result);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------

/// Captures desktop and app sources, keeping each one's streams in step with
/// what's running.
pub(crate) struct SystemAudio {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// Sources added and taken out while it runs, for its thread.
    changes: mpsc::Sender<TapChange>,
}

/// A source added to a [`SystemAudio`] that runs, or taken out.
enum TapChange {
    Add(AudioSource, Arc<SourceFeed>, Arc<Channel>),
    /// By its id.
    Remove(String),
    /// One running, set up anew (by its id): a desktop leaving out other
    /// programs, an output device's source given another device. Its feed
    /// stays; its streams follow at once.
    Update(AudioSource),
}

/// One desktop/app source and its streams, owned by the `SystemAudio` thread.
struct Tap {
    source: AudioSource,
    feed: Arc<SourceFeed>,
    channel: Arc<Channel>,
    streams: HashMap<Target, (Stream, Arc<SourceFeed>)>,
    /// Targets that failed to start: when to try each again (later and later
    /// while it keeps failing, see [`Retry`]), and the error it last gave.
    /// Not never: the desktop's one target is always wanted, so a desktop
    /// that failed to start once (or to start again after its stream died)
    /// stayed silent until the capture restarted.
    failed: HashMap<Target, (Retry, String)>,
    /// Failed targets being started again, off the rescan's thread: the feed
    /// each fills, and what its start comes back with. An activation that
    /// hangs takes 5 s to give up, and every other source's rescan, and
    /// stopping the capture, waited for it each time it was tried again.
    retrying: HashMap<Target, (Arc<SourceFeed>, mpsc::Receiver<Result<Stream>>)>,
    /// What the log last said about this source, to log only changes.
    logged: String,
    /// What each program a desktop records is called, by executable, so
    /// its file isn't read every second.
    names: HashMap<String, String>,
}

impl Tap {
    fn new(source: AudioSource, feed: Arc<SourceFeed>, channel: Arc<Channel>) -> Self {
        Self { source, feed, channel, streams: HashMap::new(), failed: HashMap::new(), retrying: HashMap::new(), logged: String::new(), names: HashMap::new() }
    }

    /// Stop its streams: it's Off.
    fn stop(&mut self) {
        for (_, (_, child)) in self.streams.drain() {
            self.feed.remove_child(&child);
        }
        // A start under way isn't waited for: a stream it starts is let go
        // of on its thread.
        self.retrying.clear();
        self.channel.set_status(SourceStatus::Off);
        self.channel.set_programs(Vec::new());
    }
}

impl SystemAudio {
    /// How often to look for apps that opened, quit or relaunched.
    const RESCAN: Duration = Duration::from_secs(1);

    pub(crate) fn start(sources: Vec<(AudioSource, Arc<SourceFeed>, Arc<Channel>)>) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (changes, changed) = mpsc::channel();
        let thread = thread::spawn(move || {
            com_init();
            let mut taps: Vec<Tap> = sources.into_iter().map(|(source, feed, channel)| Tap::new(source, feed, channel)).collect();
            let mut first = true;
            let mut ended = false;
            while !ended && !stop2.load(Ordering::Relaxed) {
                sync_taps(&mut taps, Stream::start);
                if std::mem::take(&mut first) {
                    let _ = ready_tx.send(());
                }
                // Until the next rescan, or a source added or taken out:
                // then at once, so one added starts now, and the desktop
                // leaves out an app added on its own, or no longer does.
                let until = Instant::now() + Self::RESCAN;
                while Instant::now() < until && !stop2.load(Ordering::Relaxed) {
                    match changed.recv_timeout(Duration::from_millis(50)) {
                        Ok(change) => {
                            change_taps(&mut taps, change);
                            while let Ok(change) = changed.try_recv() {
                                change_taps(&mut taps, change);
                            }
                            break;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        // Let go of without being stopped.
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            ended = true;
                            break;
                        }
                    }
                }
            }
            for tap in &mut taps {
                tap.stop();
            }
        });
        // Give the first scan a moment, so capture starts with the audio running.
        let _ = ready_rx.recv_timeout(Duration::from_secs(5));
        Ok(Self { stop, thread: Some(thread), changes })
    }

    /// Capture `source` too, into `feed`, while it runs: started by its
    /// thread, at once (not waited for).
    pub(crate) fn add(&self, source: AudioSource, feed: Arc<SourceFeed>, channel: Arc<Channel>) {
        let _ = self.changes.send(TapChange::Add(source, feed, channel));
    }

    /// Stop capturing the source `id`, while it runs: by its thread, at
    /// once (not waited for).
    pub(crate) fn remove(&self, id: &str) {
        let _ = self.changes.send(TapChange::Remove(id.to_owned()));
    }

    /// Capture `source` (one running, by its id) as it's set up now: which
    /// programs a desktop leaves out, which device an output source records.
    /// Into the same feed, so its track goes on; by its thread, at once.
    pub(crate) fn update(&self, source: AudioSource) {
        let _ = self.changes.send(TapChange::Update(source));
    }

    pub(crate) fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Bring every tap's streams in line with the processes running right now,
/// starting a target's stream with `start` (`Stream::start`).
fn sync_taps(taps: &mut [Tap], start: fn(Target, Arc<SourceFeed>) -> Result<Stream>) {
    let tree = ProcessTree::snapshot();
    let own = std::process::id();
    let own_app = crate::sources::own_app_id();
    let app_exes: Vec<String> = taps
        .iter()
        .filter_map(|t| match &t.source.kind {
            SourceKind::App { bundle_id } => Some(bundle_id.clone()),
            _ => None,
        })
        .collect();
    // Session pids: for a desktop's programs, and to rebuild it without some.
    let mut sessions: Option<Vec<u32>> = None;

    for tap in taps.iter_mut() {
        let wanted: Vec<Target> = match &tap.source.kind {
            SourceKind::App { bundle_id } => {
                // Never this process (its sound comes from `own_sound`), but
                // another HasteClip running, yes.
                let mut roots = tree.roots_of(bundle_id);
                roots.retain(|&pid| pid != own);
                let own_source = crate::sources::is_own_app(&tap.source.kind, own_app.as_deref());
                if roots.is_empty() && !own_source {
                    // Nothing to listen to until the app opens.
                    for (_, (_, child)) in tap.streams.drain() {
                        tap.feed.remove_child(&child);
                    }
                    for (_, (child, _)) in tap.retrying.drain() {
                        tap.feed.remove_child(&child);
                    }
                    tap.failed.clear();
                    tap.channel.set_status(SourceStatus::WaitingForApp);
                    // Names like it, in case it runs under another one.
                    let stem = bundle_id.trim_end_matches(".exe").to_lowercase();
                    let mut alike: Vec<&str> = tree.iter().map(|p| p.exe.as_str()).filter(|e| !stem.is_empty() && e.to_lowercase().contains(&stem)).collect();
                    alike.sort_unstable();
                    alike.dedup();
                    log_change(tap, format!("waiting: no process runs {bundle_id} (similar names running: {alike:?})"));
                    continue;
                }
                roots.into_iter().map(Target::Include).collect()
            }
            SourceKind::Desktop { exclude_app_sources, leave_out } => {
                let mut out: Vec<String> = leave_out.clone();
                if *exclude_app_sources {
                    out.extend(app_exes.iter().cloned());
                }
                let sessions = sessions.get_or_insert_with(audio_session_pids);
                let plan = desktop_plan(&tree, sessions, own, &out);
                tap.channel.set_programs(programs(&tree, &plan.recorded, &mut tap.names));
                plan.targets
            }
            SourceKind::Output { device } => match render_device_id(device) {
                Some(id) => vec![Target::Endpoint(id)],
                None => {
                    for (_, (_, child)) in tap.streams.drain() {
                        tap.feed.remove_child(&child);
                    }
                    for (_, (child, _)) in tap.retrying.drain() {
                        tap.feed.remove_child(&child);
                    }
                    tap.failed.clear();
                    tap.channel.set_status(SourceStatus::Unavailable);
                    log_change(tap, format!("unavailable: no output device {device:?}"));
                    continue;
                }
            },
            // Not played by any process: a video's sound comes from its layer.
            SourceKind::Microphone { .. } | SourceKind::Video { .. } => continue,
        };

        // Stop streams no longer wanted (or that died), start new ones.
        let feed = tap.feed.clone();
        tap.streams.retain(|target, (stream, child)| {
            let keep = wanted.contains(target) && stream.alive();
            if !keep {
                feed.remove_child(child);
            }
            keep
        });
        tap.failed.retain(|t, _| wanted.contains(t));
        tap.retrying.retain(|t, (child, _)| {
            let keep = wanted.contains(t);
            if !keep {
                feed.remove_child(child);
            }
            keep
        });
        for target in wanted {
            if tap.streams.contains_key(&target) {
                continue;
            }
            // Started again off this thread: how it went, once it has.
            if let Some((_, outcome)) = tap.retrying.get(&target) {
                let outcome = match outcome.try_recv() {
                    Err(mpsc::TryRecvError::Empty) => continue,
                    Ok(outcome) => outcome,
                    Err(mpsc::TryRecvError::Disconnected) => Err(anyhow!("its start ended without an answer")),
                };
                let (child, _) = tap.retrying.remove(&target).unwrap();
                started(tap, target, child, outcome);
                continue;
            }
            let retry = tap.failed.get(&target).map(|(retry, _)| retry.due(Instant::now()));
            if retry == Some(false) {
                continue;
            }
            let child = tap.feed.add_child(RATE);
            if retry.is_some() {
                let (tx, rx) = mpsc::channel();
                let (fills, starting) = (child.clone(), target.clone());
                thread::spawn(move || {
                    let _ = tx.send(start(starting, fills));
                });
                tap.retrying.insert(target, (child, rx));
            } else {
                // Its first try, here: the first rescan's are waited for, so
                // a capture starts with its sound running.
                let outcome = start(target.clone(), child.clone());
                started(tap, target, child, outcome);
            }
        }
        let live = !tap.streams.is_empty()
            || (matches!(tap.source.kind, SourceKind::Desktop { .. }) || crate::sources::is_own_app(&tap.source.kind, own_app.as_deref())) && tap.failed.is_empty();
        tap.channel.set_status(if live { SourceStatus::Live } else { SourceStatus::Unavailable });
        let mut capturing: Vec<String> = tap.streams.keys().map(|t| format!("{t:?}")).collect();
        capturing.sort_unstable();
        let mut failed: Vec<String> = tap.failed.keys().map(|t| format!("{t:?}")).collect();
        failed.sort_unstable();
        let what = match &tap.source.kind {
            SourceKind::App { bundle_id } => format!("app {bundle_id}"),
            other => format!("{other:?}"),
        };
        log_change(tap, format!("{} ({what}): capturing {capturing:?}, failed {failed:?}", if live { "live" } else { "unavailable" }));
    }
}

/// How starting `target`'s stream, into `child`, went.
fn started(tap: &mut Tap, target: Target, child: Arc<SourceFeed>, outcome: Result<Stream>) {
    match outcome {
        Ok(stream) => {
            tap.failed.remove(&target);
            tap.streams.insert(target, (stream, child));
        }
        Err(e) => {
            tap.feed.remove_child(&child);
            // Each error once, not at every try.
            let error = format!("{e:#}");
            let last = tap.failed.remove(&target);
            if last.as_ref().is_none_or(|(_, said)| *said != error) {
                eprintln!("audio source \"{}\": {error}", tap.source.name);
            }
            tap.failed.insert(target, (Retry::after(last.map(|(retry, _)| retry), Instant::now()), error));
        }
    }
}

/// A source added to the taps, or taken out, while capturing.
fn change_taps(taps: &mut Vec<Tap>, change: TapChange) {
    match change {
        TapChange::Add(source, feed, channel) => {
            // One with its id is never there already, but if it were, this
            // one takes its place.
            if let Some(at) = taps.iter().position(|t| t.source.id == source.id) {
                taps.remove(at).stop();
            }
            let mut tap = Tap::new(source, feed, channel);
            log_change(&mut tap, "added".to_owned());
            taps.push(tap);
        }
        TapChange::Remove(id) => {
            if let Some(at) = taps.iter().position(|t| t.source.id == id) {
                let mut tap = taps.remove(at);
                tap.stop();
                log_change(&mut tap, "taken out".to_owned());
            }
        }
        // Its streams are brought in line by the rescan that follows.
        TapChange::Update(source) => {
            if let Some(tap) = taps.iter_mut().find(|t| t.source.id == source.id) {
                tap.source = source;
            }
        }
    }
}

/// Log what a source is doing when it changes (a line a second otherwise).
fn log_change(tap: &mut Tap, line: String) {
    if tap.logged != line {
        eprintln!("{} audio source \"{}\": {line}", chrono::Local::now().format("%H:%M:%S"), tap.source.name);
        tap.logged = line;
    }
}

/// How a desktop that leaves some programs out is recorded.
#[derive(Debug, PartialEq, Eq)]
struct DesktopPlan {
    /// Its streams.
    targets: Vec<Target>,
    /// The processes with an audio session it records (sorted).
    recorded: Vec<u32>,
}

/// How to record the desktop minus HasteClip (`own`) and the programs `out`
/// (executables: apps that are sources of their own, programs left out),
/// given the processes with an audio session (`sessions`, sorted). While
/// none of `out` runs, one stream of everything but us; else a stream per
/// session process (see [`desktop_without`]). Either way, which of the
/// sessions that adds up to.
fn desktop_plan(tree: &ProcessTree, sessions: &[u32], own: u32, out: &[String]) -> DesktopPlan {
    // HasteClip's whole tree, as an exclude stream leaves out: a web page
    // shown in the video plays from a child of ours.
    let ours = |pid: u32| pid == own || tree.ancestors(pid).any(|a| a.pid == own);
    let any_running = tree.iter().any(|p| out.iter().any(|exe| p.exe.eq_ignore_ascii_case(exe)));
    if !any_running {
        let recorded = sessions.iter().copied().filter(|&pid| !ours(pid)).collect();
        return DesktopPlan { targets: vec![Target::Exclude(own)], recorded };
    }
    let excluded = |pid: u32| ours(pid) || out.iter().any(|exe| tree.belongs_to(pid, exe));
    let roots = desktop_without(tree, sessions, &excluded);
    let recorded = sessions.iter().copied().filter(|&pid| roots.contains(&pid) || tree.ancestors(pid).any(|a| roots.contains(&a.pid))).collect();
    DesktopPlan { targets: roots.into_iter().map(Target::Include).collect(), recorded }
}

/// The programs the processes `recorded` run, once each, by name (each read
/// once, into `names`), in name order.
fn programs(tree: &ProcessTree, recorded: &[u32], names: &mut HashMap<String, String>) -> Vec<Program> {
    let mut out: Vec<Program> = Vec::new();
    for &pid in recorded {
        let Some(p) = tree.get(pid) else { continue };
        if out.iter().any(|o| o.exe.eq_ignore_ascii_case(&p.exe)) {
            continue;
        }
        let name = names.entry(p.exe.to_lowercase()).or_insert_with(|| program_name(pid, &p.exe)).clone();
        out.push(Program { exe: p.exe.clone(), name });
    }
    out.sort_by_key(|p| (p.name.to_lowercase(), p.exe.to_lowercase()));
    out
}

/// The processes to capture one by one so that, together, they're everything
/// with an audio session except the `excluded` ones: each session process
/// that isn't excluded, unless an included ancestor already covers it. A
/// process whose tree contains an excluded one (a launcher that started a
/// game you added separately) is skipped, so nothing is heard twice.
fn desktop_without(tree: &ProcessTree, sessions: &[u32], excluded: &dyn Fn(u32) -> bool) -> Vec<u32> {
    let candidates: HashSet<u32> = sessions.iter().copied().filter(|&p| !excluded(p)).collect();
    // Excluded processes and everything above them.
    let mut tainted = HashSet::new();
    for p in tree.iter().filter(|p| excluded(p.pid)) {
        tainted.extend(tree.ancestors(p.pid).map(|a| a.pid));
    }
    let mut out: Vec<u32> = candidates
        .iter()
        .copied()
        .filter(|&pid| !tainted.contains(&pid))
        .filter(|&pid| !tree.ancestors(pid).any(|a| candidates.contains(&a.pid) && !tainted.contains(&a.pid)))
        .collect();
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mixer::Clock;

    /// A desktop stream that failed to start is started again off the
    /// rescan: a start that hangs (an activation can take 5 s) holds up
    /// neither the other sources' rescans nor stopping. The source is
    /// Unavailable until it has started, Live then. (Nothing is captured:
    /// the starts here fail, or wait and hand back a stream with no thread.)
    #[test]
    fn a_retry_doesnt_hold_up_the_rescan() {
        static TRIES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        fn start(_: Target, _: Arc<SourceFeed>) -> Result<Stream> {
            if TRIES.fetch_add(1, Ordering::SeqCst) == 0 {
                anyhow::bail!("the first try fails at once");
            }
            thread::sleep(Duration::from_millis(600));
            Ok(Stream { stop: Arc::new(AtomicBool::new(false)), alive: Arc::new(AtomicBool::new(true)), thread: None })
        }
        let live = crate::mixer::LiveAudio::new();
        let source = AudioSource {
            id: "desktop".into(),
            name: "Desktop".into(),
            kind: SourceKind::Desktop { exclude_app_sources: false, leave_out: Vec::new() },
            in_mix: true,
            own_track: false,
        };
        let mut taps = vec![Tap::new(source, SourceFeed::new(RATE, Arc::new(Clock::default())), live.channel("desktop"))];
        sync_taps(&mut taps, start);
        assert_eq!(taps[0].channel.status(), SourceStatus::Unavailable);
        // Its retry is due a second later.
        thread::sleep(Duration::from_millis(1050));
        let t = Instant::now();
        sync_taps(&mut taps, start);
        assert!(t.elapsed() < Duration::from_millis(200), "the rescan waited {:?} for the retry", t.elapsed());
        assert_eq!(taps[0].channel.status(), SourceStatus::Unavailable, "still starting");
        thread::sleep(Duration::from_millis(800));
        sync_taps(&mut taps, start);
        assert_eq!(taps[0].channel.status(), SourceStatus::Live);
        assert!(taps[0].failed.is_empty() && taps[0].retrying.is_empty() && taps[0].streams.len() == 1);
        assert_eq!(TRIES.load(Ordering::SeqCst), 2);
    }

    /// Explorer (1) started HasteClip (10, with a child of its own playing
    /// sound, 11), a game (20), FXSound (30), Discord (40, playing from its
    /// child 41) and a launcher (50) playing sound that started another game
    /// (51). Every process but 1, 10 and 40 has an audio session.
    fn computer() -> (ProcessTree, Vec<u32>) {
        let p = |pid: u32, parent: u32, exe: &str| super::super::system::Process { pid, parent, exe: exe.into() };
        let tree = ProcessTree::of(vec![
            p(1, 0, "explorer.exe"),
            p(10, 1, "hasteclip.exe"),
            p(11, 10, "ffplay.exe"),
            p(20, 1, "cs2.exe"),
            p(30, 1, "FXSound.exe"),
            p(40, 1, "Discord.exe"),
            p(41, 40, "Discord.exe"),
            p(50, 1, "launcher.exe"),
            p(51, 50, "game.exe"),
        ]);
        (tree, vec![11, 20, 30, 41, 50, 51])
    }

    fn out(exes: &[&str]) -> Vec<String> {
        exes.iter().map(|e| (*e).to_owned()).collect()
    }

    /// Nothing left out (or what's left out isn't running): one stream of
    /// everything but HasteClip's tree, which records every other program
    /// that has a session.
    #[test]
    fn a_desktop_leaving_nothing_out_records_everything_but_us() {
        let (tree, sessions) = computer();
        let all = DesktopPlan { targets: vec![Target::Exclude(10)], recorded: vec![20, 30, 41, 50, 51] };
        assert_eq!(desktop_plan(&tree, &sessions, 10, &[]), all);
        assert_eq!(desktop_plan(&tree, &sessions, 10, &out(&["Voicemeeter.exe"])), all);
    }

    /// A program left out while it runs (any case): a stream per other
    /// program, a child covered by its parent's, and the program isn't
    /// among what's recorded.
    #[test]
    fn a_program_left_out_isnt_recorded() {
        let (tree, sessions) = computer();
        let plan = desktop_plan(&tree, &sessions, 10, &out(&["fxsound.exe"]));
        assert_eq!(plan.targets, vec![Target::Include(20), Target::Include(41), Target::Include(50)]);
        assert_eq!(plan.recorded, vec![20, 41, 50, 51]);
    }

    /// A program left out that a launcher started: the launcher's tree
    /// would bring it back, so the launcher isn't recorded either, and the
    /// list says so (it's not among what's recorded).
    #[test]
    fn leaving_out_a_launched_game_leaves_out_its_launcher() {
        let (tree, sessions) = computer();
        let plan = desktop_plan(&tree, &sessions, 10, &out(&["game.exe", "FXSound.exe"]));
        assert_eq!(plan.targets, vec![Target::Include(20), Target::Include(41)]);
        assert_eq!(plan.recorded, vec![20, 41]);
    }

    /// The programs recorded: once each however many of its processes
    /// play, by name, in name order.
    #[test]
    fn the_programs_recorded_are_named_once_each() {
        let (tree, _) = computer();
        let mut names: HashMap<String, String> =
            [("discord.exe", "Discord"), ("cs2.exe", "Counter-Strike 2"), ("game.exe", "A Game")].into_iter().map(|(e, n)| (e.to_owned(), n.to_owned())).collect();
        let listed = programs(&tree, &[20, 40, 41, 51], &mut names);
        let listed: Vec<(&str, &str)> = listed.iter().map(|p| (p.exe.as_str(), p.name.as_str())).collect();
        assert_eq!(listed, [("game.exe", "A Game"), ("cs2.exe", "Counter-Strike 2"), ("Discord.exe", "Discord")]);
    }

    fn app(id: &str, exe: &str) -> AudioSource {
        AudioSource { id: id.into(), name: id.into(), kind: SourceKind::App { bundle_id: exe.into() }, in_mix: true, own_track: false }
    }

    /// A source taken out while capturing stops its streams (what they had
    /// sent is still there to mix, nothing after) and is Off.
    #[test]
    fn a_source_taken_out_stops_its_streams() {
        let live = crate::mixer::LiveAudio::new();
        let clock = Arc::new(Clock::default());
        clock.set(1000.0);
        let feed = SourceFeed::new(RATE, clock);
        let mut taps = Vec::new();
        change_taps(&mut taps, TapChange::Add(app("discord", "Discord.exe"), feed.clone(), live.channel("discord")));
        assert_eq!(taps.len(), 1);
        // One of its streams, running (with no thread: nothing's captured).
        let child = feed.add_child(RATE);
        let stream = Stream { stop: Arc::new(AtomicBool::new(false)), alive: Arc::new(AtomicBool::new(true)), thread: None };
        taps[0].streams.insert(Target::Include(1), (stream, child.clone()));
        taps[0].channel.set_status(SourceStatus::Live);
        child.push(1000.0, &[0.5; 960], 2);
        change_taps(&mut taps, TapChange::Remove("discord".into()));
        assert!(taps.is_empty());
        assert_eq!(live.channel("discord").status(), SourceStatus::Off);
        child.push(1000.01, &[0.5; 960], 2);
        assert_eq!(feed.arrived(), Some(480), "what came before, and nothing after");
    }

    /// A source set up anew while capturing (a desktop leaving a program
    /// out) keeps its feed and the streams it has until the rescan that
    /// follows decides which it still wants.
    #[test]
    fn a_source_set_up_anew_keeps_its_feed() {
        let live = crate::mixer::LiveAudio::new();
        let clock = Arc::new(Clock::default());
        clock.set(1000.0);
        let feed = SourceFeed::new(RATE, clock);
        let desktop = |leave_out: Vec<String>| AudioSource { id: "desk".into(), name: "Desktop".into(), kind: SourceKind::Desktop { exclude_app_sources: true, leave_out }, in_mix: true, own_track: false };
        let mut taps = Vec::new();
        change_taps(&mut taps, TapChange::Add(desktop(Vec::new()), feed.clone(), live.channel("desk")));
        let child = feed.add_child(RATE);
        let stream = Stream { stop: Arc::new(AtomicBool::new(false)), alive: Arc::new(AtomicBool::new(true)), thread: None };
        taps[0].streams.insert(Target::Exclude(1), (stream, child));
        change_taps(&mut taps, TapChange::Update(desktop(vec!["FXSound.exe".into()])));
        change_taps(&mut taps, TapChange::Update(AudioSource { id: "other".into(), ..desktop(Vec::new()) }));
        assert_eq!(taps.len(), 1, "one not there isn't added");
        assert_eq!(taps[0].source.kind, SourceKind::Desktop { exclude_app_sources: true, leave_out: vec!["FXSound.exe".into()] });
        assert!(Arc::ptr_eq(&taps[0].feed, &feed));
        assert_eq!(taps[0].streams.len(), 1);
    }

    /// Sources added to and taken out of a running capture of desktop and
    /// app sound are taken up at once, not at its next rescan. (An app that
    /// doesn't run: nothing's captured.)
    #[test]
    fn a_running_capture_takes_sources_added_and_taken_out() {
        let live = crate::mixer::LiveAudio::new();
        let channel = live.channel("app");
        let system = SystemAudio::start(Vec::new()).unwrap();
        let until = |status: SourceStatus| {
            let t = Instant::now();
            while channel.status() != status && t.elapsed() < Duration::from_secs(3) {
                thread::sleep(Duration::from_millis(5));
            }
            t.elapsed()
        };
        system.add(app("app", "hasteclip-test-no-such-app.exe"), SourceFeed::new(RATE, Arc::new(Clock::default())), channel.clone());
        let took = until(SourceStatus::WaitingForApp);
        assert!(took < Duration::from_millis(500), "added: waiting for its app after {took:?}");
        system.remove("app");
        let took = until(SourceStatus::Off);
        assert!(took < Duration::from_millis(500), "taken out: Off after {took:?}");
        system.stop();
    }

    #[test]
    #[ignore = "captures real system audio; run with --ignored"]
    fn loopback_stream_runs() {
        let clock = Arc::new(Clock::default());
        clock.set(host_now());
        let feed = SourceFeed::new(RATE, clock);
        eprintln!("starting");
        let stream = Stream::start(Target::Exclude(std::process::id()), feed.clone()).unwrap();
        eprintln!("started");
        thread::sleep(Duration::from_secs(1));
        assert!(stream.alive());
        drop(stream);
        eprintln!("stopped");
    }

    /// The default output device's loopback opens at the mixer's format and
    /// keeps running. `DEVICE` names another output device.
    #[test]
    #[ignore = "captures real system audio; run with --ignored"]
    fn endpoint_loopback_runs() {
        com_init();
        let name = std::env::var("DEVICE").unwrap_or_else(|_| crate::audio::DEFAULT_DEVICE.to_owned());
        let id = render_device_id(&name).expect("no such output device");
        eprintln!("{name}: {id}");
        let clock = Arc::new(Clock::default());
        clock.set(host_now());
        let feed = SourceFeed::new(RATE, clock);
        let stream = Stream::start(Target::Endpoint(id), feed.clone()).unwrap();
        thread::sleep(Duration::from_secs(2));
        assert!(stream.alive());
    }
}
