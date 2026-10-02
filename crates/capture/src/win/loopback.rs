//! Desktop and app audio on Windows, via WASAPI process loopback (Windows 10
//! 2004+): a stream captures everything one process tree plays (include), or
//! everything except one process tree (exclude).
//!
//! - **App source**: one include stream per top process of the app, so its
//!   helpers (browser tabs, Discord's voice process) come along.
//! - **Desktop**: everything except HesteClips itself (its clip previews).
//! - **Desktop minus app sources**: WASAPI can exclude only one tree, so while
//!   any of those apps runs, the desktop is rebuilt from an include stream per
//!   process that has an audio session — minus the apps, minus us.
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
use windows::Win32::System::Com::BLOB;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{IUnknown, Interface, implement};

use super::system::{ProcessTree, audio_session_pids, com_init, host_now};
use crate::mixer::{Channel, RATE, SourceFeed, SourceStatus};
use crate::sources::{AudioSource, SourceKind};

/// What one loopback stream captures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Target {
    Include(u32),
    Exclude(u32),
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
            match open(target) {
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

/// Activate and start a process-loopback client: 48 kHz stereo float, which
/// Windows converts to from whatever the apps play.
fn open(target: Target) -> Result<(IAudioClient, IAudioCaptureClient)> {
    let (pid, mode) = match target {
        Target::Include(pid) => (pid, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE),
        Target::Exclude(pid) => (pid, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE),
    };
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

/// Read packets until stopped, pushing each at its capture time.
fn pump(client: &IAudioClient, capture: &IAudioCaptureClient, feed: &SourceFeed, stop: &AtomicBool) -> Result<()> {
    unsafe {
        let event = CreateEventW(None, false, false, None)?;
        let result = (|| -> Result<()> {
            client.SetEventHandle(event)?;
            client.Start()?;
            let mut silence = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                if WaitForSingleObject(event, 100) != WAIT_OBJECT_0 {
                    continue;
                }
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
}

/// One desktop/app source and its streams, owned by the `SystemAudio` thread.
struct Tap {
    source: AudioSource,
    feed: Arc<SourceFeed>,
    channel: Arc<Channel>,
    streams: HashMap<Target, (Stream, Arc<SourceFeed>)>,
    /// Targets that failed to start, so they aren't retried every second.
    failed: HashSet<Target>,
}

impl SystemAudio {
    /// How often to look for apps that opened, quit or relaunched.
    const RESCAN: Duration = Duration::from_secs(1);

    pub(crate) fn start(sources: Vec<(AudioSource, Arc<SourceFeed>, Arc<Channel>)>) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            com_init();
            let mut taps: Vec<Tap> = sources
                .into_iter()
                .map(|(source, feed, channel)| Tap { source, feed, channel, streams: HashMap::new(), failed: HashSet::new() })
                .collect();
            let mut first = true;
            while !stop2.load(Ordering::Relaxed) {
                sync_taps(&mut taps);
                if std::mem::take(&mut first) {
                    let _ = ready_tx.send(());
                }
                let until = Instant::now() + Self::RESCAN;
                while Instant::now() < until && !stop2.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(50));
                }
            }
            for tap in &mut taps {
                tap.streams.clear();
                tap.channel.set_status(SourceStatus::Off);
            }
        });
        // Give the first scan a moment, so capture starts with the audio running.
        let _ = ready_rx.recv_timeout(Duration::from_secs(5));
        Ok(Self { stop, thread: Some(thread) })
    }

    pub(crate) fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Bring every tap's streams in line with the processes running right now.
fn sync_taps(taps: &mut [Tap]) {
    let tree = ProcessTree::snapshot();
    let own = std::process::id();
    let app_exes: Vec<String> = taps
        .iter()
        .filter_map(|t| match &t.source.kind {
            SourceKind::App { bundle_id } => Some(bundle_id.clone()),
            _ => None,
        })
        .collect();
    // Session pids are only needed to rebuild a desktop without some apps.
    let mut sessions: Option<Vec<u32>> = None;

    for tap in taps.iter_mut() {
        let wanted: Vec<Target> = match &tap.source.kind {
            SourceKind::App { bundle_id } => {
                let roots = tree.roots_of(bundle_id);
                if roots.is_empty() {
                    // Nothing to listen to until the app opens.
                    for (_, (_, child)) in tap.streams.drain() {
                        tap.feed.remove_child(&child);
                    }
                    tap.failed.clear();
                    tap.channel.set_status(SourceStatus::WaitingForApp);
                    continue;
                }
                roots.into_iter().map(Target::Include).collect()
            }
            SourceKind::Desktop { exclude_app_sources: true } => {
                let excluded = |pid: u32| pid == own || app_exes.iter().any(|exe| tree.belongs_to(pid, exe));
                let any_running = tree.iter().any(|p| app_exes.iter().any(|exe| p.exe.eq_ignore_ascii_case(exe)));
                if !any_running {
                    vec![Target::Exclude(own)]
                } else {
                    let sessions = sessions.get_or_insert_with(audio_session_pids);
                    desktop_without(&tree, sessions, &excluded).into_iter().map(Target::Include).collect()
                }
            }
            SourceKind::Desktop { exclude_app_sources: false } => vec![Target::Exclude(own)],
            SourceKind::Microphone { .. } => continue,
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
        tap.failed.retain(|t| wanted.contains(t));
        for target in wanted {
            if tap.streams.contains_key(&target) || tap.failed.contains(&target) {
                continue;
            }
            let child = tap.feed.add_child(RATE);
            match Stream::start(target, child.clone()) {
                Ok(stream) => {
                    tap.streams.insert(target, (stream, child));
                }
                Err(e) => {
                    tap.feed.remove_child(&child);
                    eprintln!("audio source \"{}\": {e:#}", tap.source.name);
                    tap.failed.insert(target);
                }
            }
        }
        let live = !tap.streams.is_empty() || matches!(tap.source.kind, SourceKind::Desktop { .. }) && tap.failed.is_empty();
        tap.channel.set_status(if live { SourceStatus::Live } else { SourceStatus::Unavailable });
    }
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
}
