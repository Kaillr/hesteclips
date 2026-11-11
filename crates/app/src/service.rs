//! Runs the capture backend on its own thread so blocking work (finishing a
//! file, writing a saved clip) never freezes the egui UI.
//!
//! The UI sends `Cmd`s and drains `Evt`s each frame; it never calls the recorder
//! directly.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use capture::{EncodeSettings, Mode};

use crate::games::Details;

pub(crate) enum Cmd {
    Start(Mode, Box<EncodeSettings>),
    /// Save the replay buffer as a clip in this folder, its name ending in what
    /// happened in the game, if known.
    SaveClip(PathBuf, Details),
    /// Take a screenshot of what's recorded (with a capture of its own if
    /// nothing runs), into this folder.
    Screenshot(PathBuf, capture::VideoSource, crate::screenshot::Options),
    /// Change what the running capture records, if it can without a restart.
    UpdateVideo(capture::VideoSource),
    /// How far back the running replay buffer reaches.
    ReplaySeconds(u32),
    /// Switch a microphone source (by id) to another device.
    SetMic(String, String),
    /// Capture these audio sources from now on: added and taken out while
    /// capturing, where that can be (`capture::SOURCES_WHILE_CAPTURING`).
    SetSources(Vec<capture::sources::AudioSource>),
    /// Frame blending changed while capturing: looks a frame, and how
    /// they're weighed.
    SetBlend(u32, capture::BlendShape),
    /// Stop clipping, recording or streaming; a recording goes into this
    /// folder, named after what happened.
    Stop(Mode, Option<PathBuf>, Details),
    /// Stop everything, then signal once the file is finished (app being
    /// killed).
    StopAndAck(Sender<()>),
    /// The app is closing: end the thread (dropping the recorder finishes any
    /// recording). Asked for outright, as the shortcut handler keeps a sender
    /// of its own for as long as the process runs.
    Quit,
}

/// Results from the capture thread, drained by the UI.
pub enum Evt {
    /// Authoritative capture state: what runs now (clipping, recording,
    /// streaming, any of them). The UI syncs its own state to this so the two
    /// can never drift apart.
    State(capture::Running),
    Saved(PathBuf),
    /// A screenshot is saved here, and was copied (or not).
    Shot(PathBuf, bool),
    Error(String),
    /// Something to tell that isn't a failed start or save.
    Notice(String),
}

/// For the signal handler: a way to stop the running capture and wait for its
/// file to be finished (destructors don't run on SIGTERM).
static STOP_HOOK: OnceLock<Mutex<Option<Sender<Cmd>>>> = OnceLock::new();

/// Stop capture and wait (up to `timeout`) until the recording is written.
pub fn stop_all_and_wait(timeout: Duration) {
    let Some(tx) = STOP_HOOK.get().and_then(|h| h.lock().ok()?.clone()) else { return };
    let (ack_tx, ack_rx) = std::sync::mpsc::channel();
    if tx.send(Cmd::StopAndAck(ack_tx)).is_ok() {
        let _ = ack_rx.recv_timeout(timeout);
    }
}

/// Saves a replay clip right from a shortcut's own event, without waiting for
/// the app's next frame: while a game covers the window, Windows can hold
/// those back for seconds, and the clip is of the moment it's asked for.
#[derive(Clone)]
pub struct QuickSave {
    tx: Sender<Cmd>,
    /// What's in focus, for the folder the clip goes in.
    games: crate::games::Tracker,
    /// While the replay buffer runs: how to save.
    armed: Arc<Mutex<Option<Armed>>>,
    /// While recording: the sound that says a moment's marked.
    marking: Arc<Mutex<Option<crate::settings::SaveSound>>>,
    /// Where screenshots can be taken: how.
    shot: Arc<Mutex<Option<ShotArmed>>>,
}

/// How a screenshot is taken: of what, into which library, as what.
#[derive(Clone, PartialEq)]
pub struct ShotArmed {
    pub video: capture::VideoSource,
    pub library: PathBuf,
    pub folder_per_game: bool,
    pub game_folders: std::collections::BTreeMap<String, String>,
    pub options: crate::screenshot::Options,
    pub sound: crate::settings::SaveSound,
}

/// How a clip is saved: in which library, sorted by game or not, and the
/// sound that says it's saved.
#[derive(Clone, PartialEq)]
pub struct Armed {
    pub library: PathBuf,
    pub folder_per_game: bool,
    pub game_folders: std::collections::BTreeMap<String, String>,
    pub sound: crate::settings::SaveSound,
}

impl QuickSave {
    /// Kept up to date by the app, every frame.
    pub fn arm(&self, to: Option<Armed>) {
        if let Ok(mut armed) = self.armed.lock()
            && *armed != to
        {
            *armed = to;
        }
    }

    /// Kept up to date by the app, every frame.
    pub fn arm_marking(&self, to: Option<crate::settings::SaveSound>) {
        if let Ok(mut armed) = self.marking.lock()
            && *armed != to
        {
            *armed = to;
        }
    }

    /// Kept up to date by the app, every frame.
    pub fn arm_screenshot(&self, to: Option<ShotArmed>) {
        if let Ok(mut armed) = self.shot.lock()
            && *armed != to
        {
            *armed = to;
        }
    }

    /// Take a screenshot now, into the folder of the game in focus. Whether
    /// it was asked for.
    pub fn screenshot(&self) -> bool {
        let Some(a) = self.shot.lock().ok().and_then(|a| a.clone()) else { return false };
        let game = self.games.focused().map(|(_, g)| g.name);
        let dir = crate::games::folder_for(&a.library, a.folder_per_game, &a.game_folders, game.as_deref());
        if self.tx.send(Cmd::Screenshot(dir, a.video, a.options)).is_err() {
            return false;
        }
        crate::sound::play_saved(&a.sound);
        true
    }

    /// Save a clip now, if clipping runs. Whether it was asked for. While
    /// recording, the moment's marked in the recording too (without
    /// clipping, only that: no clip, false).
    pub fn save(&self) -> bool {
        let Some(a) = self.armed.lock().ok().and_then(|a| a.clone()) else {
            self.mark(true);
            return false;
        };
        let dir = crate::games::folder_for(&a.library, a.folder_per_game, &a.game_folders, self.games.clip_game().as_deref());
        if self.tx.send(Cmd::SaveClip(dir, self.games.clip_details())).is_err() {
            return false;
        }
        crate::sound::play_saved(&a.sound);
        // The clip's sound says it for the mark too.
        self.mark(false);
        true
    }

    /// Mark this moment in the recording, if one runs (`sound`: saying so).
    /// Whether it was marked.
    fn mark(&self, sound: bool) -> bool {
        let Some(said) = self.marking.lock().ok().and_then(|s| s.clone()) else { return false };
        let marked = self.games.mark();
        if marked && sound {
            crate::sound::play_saved(&said);
        }
        marked
    }

    /// Mark this moment in the recording, saying so (a Stream Deck's Mark
    /// moment). Whether a recording runs to mark.
    pub fn mark_moment(&self) -> bool {
        self.mark(true)
    }
}

pub struct CaptureService {
    cmd_tx: Option<Sender<Cmd>>,
    evt_rx: Receiver<Evt>,
    thread: Option<thread::JoinHandle<()>>,
    in_flight: InFlight,
}

/// Starts and stops not answered yet. Each gets one `Evt::State`, in the
/// order they were sent, so only the answer to the last one says what runs
/// now: one to a start or stop that another has followed is out of date.
/// (Clipping that started at launch just as a session stopped it for a
/// recording showed clipping again, then nothing, then recording.)
#[derive(Default)]
struct InFlight(std::sync::atomic::AtomicU32);

impl InFlight {
    fn sent(&self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// An answer came: whether it's to the last one sent.
    fn replied(&self) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        let was = self.0.fetch_update(Relaxed, Relaxed, |n| Some(n.saturating_sub(1))).unwrap_or(0);
        was <= 1
    }
}

impl CaptureService {
    /// `live` carries per-source volume in and meters out while capturing.
    pub fn new(live: std::sync::Arc<capture::mixer::LiveAudio>) -> Self {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Cmd>();
        let (evt_tx, evt_rx) = std::sync::mpsc::channel::<Evt>();

        let _ = STOP_HOOK.get_or_init(|| Mutex::new(None)).lock().map(|mut h| *h = Some(cmd_tx.clone()));
        // A demo recording clips nothing: its stand-in hands over prepared clips.
        if crate::demo::on() {
            let thread = crate::demo::capture_thread(cmd_rx, evt_tx);
            return Self { cmd_tx: Some(cmd_tx), evt_rx, thread: Some(thread), in_flight: InFlight::default() };
        }
        let thread = thread::spawn(move || {
            // The backend lives entirely on this thread.
            let mut recorder = capture::default_recorder(live);
            while let Ok(cmd) = cmd_rx.recv() {
                match cmd {
                    Cmd::Start(mode, settings) => {
                        if let Err(e) = recorder.start(mode, &settings) {
                            eprintln!("starting {mode:?} failed: {e:#}");
                            let why = describe(&e);
                            send(&evt_tx, Evt::Error(if mode == Mode::Stream { format!("Couldn't go live: {why}") } else { why }));
                        }
                        send(&evt_tx, Evt::State(recorder.running()));
                    }
                    // Save doesn't change capture state — still buffering afterwards.
                    // The clip's moment is taken now; it's written on its own
                    // thread, so this one is free for the next save at once.
                    Cmd::SaveClip(dir, details) => match recorder.save_clip(&dir) {
                        Ok(pending) => {
                            let evt_tx = evt_tx.clone();
                            thread::spawn(move || match pending.finish() {
                                Ok(path) => send(&evt_tx, Evt::Saved(landed(path, details))),
                                Err(e) => {
                                    eprintln!("saving a clip failed: {e:#}");
                                    send(&evt_tx, Evt::Error(describe(&e)));
                                }
                            });
                        }
                        Err(e) => {
                            eprintln!("saving a clip failed: {e:#}");
                            send(&evt_tx, Evt::Error(describe(&e)));
                        }
                    },
                    // The picture's taken now; it's saved on a thread of its own.
                    Cmd::Screenshot(dir, video, options) => {
                        let taken = chrono::Local::now().naive_local();
                        match recorder.screenshot(&video) {
                            Ok(pending) => {
                                let evt_tx = evt_tx.clone();
                                thread::spawn(move || match pending.finish().map_err(|e| e.to_string()).and_then(|img| crate::screenshot::save(&img, &dir, taken, options)) {
                                    Ok((path, copied)) => send(&evt_tx, Evt::Shot(path, copied)),
                                    Err(e) => {
                                        eprintln!("taking a screenshot failed: {e}");
                                        send(&evt_tx, Evt::Notice(format!("Couldn't take the screenshot: {e}")));
                                    }
                                });
                            }
                            Err(e) => {
                                eprintln!("taking a screenshot failed: {e:#}");
                                send(&evt_tx, Evt::Notice(format!("Couldn't take the screenshot: {}", describe(&e))));
                            }
                        }
                    }
                    Cmd::UpdateVideo(video) => {
                        if !recorder.update_video(&video) {
                            send(&evt_tx, Evt::Notice("Couldn't switch what's recorded while capturing. Stop and start again to record it.".into()));
                        }
                    }
                    Cmd::ReplaySeconds(seconds) => recorder.set_replay_seconds(seconds),
                    Cmd::SetMic(id, device) => {
                        if !recorder.set_mic(&id, &device) {
                            eprintln!("couldn't switch the microphone of {id} while capturing");
                        }
                    }
                    Cmd::SetSources(sources) => {
                        if !recorder.set_sources(&sources) {
                            eprintln!("couldn't add or take out audio sources while capturing");
                        }
                    }
                    Cmd::SetBlend(looks, shape) => {
                        if !recorder.set_blend(looks, shape) {
                            eprintln!("couldn't change frame blending while capturing");
                        }
                    }
                    Cmd::Stop(mode, dir, details) => {
                        match recorder.stop(mode, dir.as_deref()) {
                            Ok(Some(path)) => send(&evt_tx, Evt::Saved(landed(path, details))),
                            Ok(None) => {}
                            Err(e) => send(&evt_tx, Evt::Error(describe(&e))),
                        }
                        send(&evt_tx, Evt::State(recorder.running()));
                    }
                    Cmd::StopAndAck(ack) => {
                        recorder.stop_all();
                        let _ = ack.send(());
                    }
                    Cmd::Quit => break,
                }
            }
        });

        Self { cmd_tx: Some(cmd_tx), evt_rx, thread: Some(thread), in_flight: InFlight::default() }
    }

    pub fn start(&self, mode: Mode, settings: EncodeSettings) {
        self.in_flight.sent();
        self.send(Cmd::Start(mode, Box::new(settings)));
    }
    /// A way to save a clip from another thread (a shortcut's).
    pub fn quick_save(&self, games: crate::games::Tracker) -> QuickSave {
        let tx = self.cmd_tx.clone().expect("the capture thread runs while the service lives");
        QuickSave { tx, games, armed: Default::default(), marking: Default::default(), shot: Default::default() }
    }

    pub fn save_clip(&self, dir: PathBuf, details: Details) {
        self.send(Cmd::SaveClip(dir, details));
    }
    pub fn screenshot(&self, dir: PathBuf, video: capture::VideoSource, options: crate::screenshot::Options) {
        self.send(Cmd::Screenshot(dir, video, options));
    }
    /// Stop `mode` (a recording into `dir`, named after `details`).
    pub fn stop(&self, mode: Mode, dir: Option<PathBuf>, details: Details) {
        self.in_flight.sent();
        self.send(Cmd::Stop(mode, dir, details));
    }
    pub fn set_blend(&self, looks: u32, shape: capture::BlendShape) {
        self.send(Cmd::SetBlend(looks, shape));
    }
    pub fn update_video(&self, video: capture::VideoSource) {
        self.send(Cmd::UpdateVideo(video));
    }
    pub fn set_mic(&self, id: String, device: String) {
        self.send(Cmd::SetMic(id, device));
    }
    pub fn set_sources(&self, sources: Vec<capture::sources::AudioSource>) {
        self.send(Cmd::SetSources(sources));
    }
    pub fn set_replay_seconds(&self, seconds: u32) {
        self.send(Cmd::ReplaySeconds(seconds));
    }

    fn send(&self, cmd: Cmd) {
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.send(cmd);
        }
    }

    /// Drain the next event, if any (non-blocking). A state that's out of
    /// date (see `InFlight`) is passed over.
    pub fn poll(&self) -> Option<Evt> {
        loop {
            match self.evt_rx.try_recv() {
                Ok(Evt::State(_)) if !self.in_flight.replied() => continue,
                Ok(evt) => return Some(evt),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return None,
            }
        }
    }
}

impl Drop for CaptureService {
    /// Quitting the app must stop capture: the thread exits its loop and drops
    /// the recorder (which finishes any recording), and we wait for that so the
    /// process doesn't exit mid-write.
    fn drop(&mut self) {
        let t = std::time::Instant::now();
        eprintln!("quitting: stopping capture");
        if let Some(tx) = self.cmd_tx.take() {
            let _ = tx.send(Cmd::Quit);
        }
        if let Some(hook) = STOP_HOOK.get() {
            let _ = hook.lock().map(|mut h| *h = None);
        }
        // Wait for the file to be finished, but never forever: a capture
        // thread that won't stop froze the app on quit (and kept an update
        // from installing). A recording is crash-safe up to its last second
        // anyway, and a replay buffer is only in memory.
        if let Some(thread) = self.thread.take() {
            let limit = std::time::Duration::from_secs(10);
            while !thread.is_finished() && t.elapsed() < limit {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            if thread.is_finished() {
                let _ = thread.join();
                eprintln!("quitting: capture stopped ({} ms)", t.elapsed().as_millis());
            } else {
                eprintln!("quitting: capture didn't stop within {} s; quitting anyway", limit.as_secs());
            }
        }
    }
}

fn send(tx: &Sender<Evt>, evt: Evt) {
    let _ = tx.send(evt);
}

/// A just-saved clip: named after what happened in the game, its moments
/// kept with it.
pub(crate) fn landed(path: PathBuf, details: Details) -> PathBuf {
    let path = named(path, details.summary);
    // Marks are known from the end: where they are takes the file's length.
    let markers = if details.marks.is_empty() {
        Vec::new()
    } else {
        match media::probe(&path) {
            Ok(info) => crate::moments::marks_at(info.duration, &details.marks),
            Err(e) => {
                eprintln!("couldn't place the marks in {}: {e:#}", path.display());
                Vec::new()
            }
        }
    };
    if !details.moments.is_empty() || !markers.is_empty() {
        crate::moments::keep(&path, &details.moments, &markers);
    }
    path
}

/// A just-saved clip, renamed to end in what happened in the game:
/// `clip_<time> - 3 kills on Mirage.mp4` (shown as "3 kills on Mirage ·
/// 18:40:02"). Keeps its name if that can't be done.
fn named(path: PathBuf, details: Option<String>) -> PathBuf {
    let Some(details) = details.map(|d| crate::clips::sanitize_name(&d)).filter(|d| !d.is_empty()) else { return path };
    let (Some(stem), Some(ext)) = (path.file_stem(), path.extension()) else { return path };
    let to = path.with_file_name(format!("{} - {details}.{}", stem.to_string_lossy(), ext.to_string_lossy()));
    if to.exists() {
        return path;
    }
    // An antivirus scan can hold a new file open for a moment.
    for _ in 0..20 {
        match std::fs::rename(&path, &to) {
            Ok(()) => return to,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => thread::sleep(Duration::from_millis(100)),
            Err(e) => {
                eprintln!("couldn't name {} after the game: {e}", path.display());
                return path;
            }
        }
    }
    path
}

/// A capture error as the user should read it. Running out of disk space
/// otherwise shows as just "couldn't finish <hidden file>": the cause is deep
/// in the chain, so look for it there and say it plainly.
fn describe(e: &anyhow::Error) -> String {
    let full = e.chain().filter_map(|c| c.downcast_ref::<std::io::Error>()).any(|io| {
        matches!(io.kind(), std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded)
    });
    if full { "Your disk is full. Free up some space and try again.".to_owned() } else { e.to_string() }
}

#[cfg(test)]
mod tests {
    use anyhow::Context;

    /// A full disk deep in the chain is what the user is told, not the outer context.
    #[test]
    fn full_disk_is_said_plainly() {
        let io = std::io::Error::from(std::io::ErrorKind::StorageFull);
        let e = Err::<(), _>(io).context(r"couldn't finish E:\.clip.mp4").unwrap_err();
        assert_eq!(super::describe(&e), "Your disk is full. Free up some space and try again.");
        assert_eq!(super::describe(&anyhow::anyhow!("nothing was recorded")), "nothing was recorded");
    }

    /// Clipping started at launch, then stopped for a recording right away:
    /// only the recording's reply says what runs.
    #[test]
    fn only_the_last_reply_counts() {
        let f = super::InFlight::default();
        f.sent(); // start clipping
        f.sent(); // stop it
        f.sent(); // start recording
        assert!(!f.replied(), "clipping started: out of date");
        assert!(!f.replied(), "clipping stopped: out of date");
        assert!(f.replied(), "recording started");
        f.sent();
        assert!(f.replied());
        // A reply nothing was sent for (never expected) still counts.
        assert!(f.replied());
    }
}
