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

enum Cmd {
    Start(Mode, EncodeSettings),
    /// Save the replay buffer as a clip in this folder, its name ending in what
    /// happened in the game, if known.
    SaveClip(PathBuf, Option<String>),
    /// Change what the running capture records, if it can without a restart.
    UpdateVideo(capture::VideoSource),
    /// How far back the running replay buffer reaches.
    ReplaySeconds(u32),
    /// Stop; a recording goes into this folder, named after what happened.
    Stop(Option<PathBuf>, Option<String>),
    /// Stop, then signal once the file is finished (app being killed).
    StopAndAck(Sender<()>),
    /// The app is closing: end the thread (dropping the recorder finishes any
    /// recording). Asked for outright, as the shortcut handler keeps a sender
    /// of its own for as long as the process runs.
    Quit,
}

/// Results from the capture thread, drained by the UI.
pub enum Evt {
    /// Authoritative capture state: `None` = idle, `Some(mode)` = capturing.
    /// The UI syncs its own state to this so the two can never drift apart.
    State(Option<Mode>),
    Saved(PathBuf),
    Error(String),
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

    /// Save a clip now, if the replay buffer runs. Whether it was asked for.
    pub fn save(&self) -> bool {
        let Some(a) = self.armed.lock().ok().and_then(|a| a.clone()) else { return false };
        let dir = crate::games::folder_for(&a.library, a.folder_per_game, &a.game_folders, self.games.clip_game().as_deref());
        if self.tx.send(Cmd::SaveClip(dir, self.games.clip_details())).is_err() {
            return false;
        }
        crate::sound::play_saved(&a.sound);
        true
    }
}

pub struct CaptureService {
    cmd_tx: Option<Sender<Cmd>>,
    evt_rx: Receiver<Evt>,
    thread: Option<thread::JoinHandle<()>>,
}

impl CaptureService {
    /// `live` carries per-source volume in and meters out while capturing.
    pub fn new(live: std::sync::Arc<capture::mixer::LiveAudio>) -> Self {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Cmd>();
        let (evt_tx, evt_rx) = std::sync::mpsc::channel::<Evt>();

        let _ = STOP_HOOK.get_or_init(|| Mutex::new(None)).lock().map(|mut h| *h = Some(cmd_tx.clone()));
        let thread = thread::spawn(move || {
            // The backend lives entirely on this thread.
            let mut recorder = capture::default_recorder(live);
            while let Ok(cmd) = cmd_rx.recv() {
                match cmd {
                    Cmd::Start(mode, settings) => match recorder.start(mode, &settings) {
                        Ok(()) => send(&evt_tx, Evt::State(Some(mode))),
                        Err(e) => {
                            send(&evt_tx, Evt::Error(describe(&e)));
                            send(&evt_tx, Evt::State(None));
                        }
                    },
                    // Save doesn't change capture state — still buffering afterwards.
                    // The clip's moment is taken now; it's written on its own
                    // thread, so this one is free for the next save at once.
                    Cmd::SaveClip(dir, details) => match recorder.save_clip(&dir) {
                        Ok(pending) => {
                            let evt_tx = evt_tx.clone();
                            thread::spawn(move || match pending.finish() {
                                Ok(path) => send(&evt_tx, Evt::Saved(named(path, details))),
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
                    Cmd::UpdateVideo(video) => {
                        recorder.update_video(&video);
                    }
                    Cmd::ReplaySeconds(seconds) => recorder.set_replay_seconds(seconds),
                    Cmd::Stop(dir, details) => {
                        match recorder.stop(dir.as_deref()) {
                            Ok(Some(path)) => send(&evt_tx, Evt::Saved(named(path, details))),
                            Ok(None) => {}
                            Err(e) => send(&evt_tx, Evt::Error(describe(&e))),
                        }
                        send(&evt_tx, Evt::State(None));
                    }
                    Cmd::StopAndAck(ack) => {
                        let _ = recorder.stop(None);
                        let _ = ack.send(());
                    }
                    Cmd::Quit => break,
                }
            }
        });

        Self { cmd_tx: Some(cmd_tx), evt_rx, thread: Some(thread) }
    }

    pub fn start(&self, mode: Mode, settings: EncodeSettings) {
        self.send(Cmd::Start(mode, settings));
    }
    /// A way to save a clip from another thread (a shortcut's).
    pub fn quick_save(&self, games: crate::games::Tracker) -> QuickSave {
        let tx = self.cmd_tx.clone().expect("the capture thread runs while the service lives");
        QuickSave { tx, games, armed: Default::default() }
    }

    pub fn save_clip(&self, dir: PathBuf, details: Option<String>) {
        self.send(Cmd::SaveClip(dir, details));
    }
    pub fn stop(&self, dir: Option<PathBuf>, details: Option<String>) {
        self.send(Cmd::Stop(dir, details));
    }
    pub fn update_video(&self, video: capture::VideoSource) {
        self.send(Cmd::UpdateVideo(video));
    }
    pub fn set_replay_seconds(&self, seconds: u32) {
        self.send(Cmd::ReplaySeconds(seconds));
    }

    fn send(&self, cmd: Cmd) {
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.send(cmd);
        }
    }

    /// Drain the next event, if any (non-blocking).
    pub fn poll(&self) -> Option<Evt> {
        match self.evt_rx.try_recv() {
            Ok(evt) => Some(evt),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }
}

impl Drop for CaptureService {
    /// Quitting the app must stop capture: the thread exits its loop and drops
    /// the recorder (which finishes any recording), and we wait for that so the
    /// process doesn't exit mid-write.
    fn drop(&mut self) {
        if let Some(tx) = self.cmd_tx.take() {
            let _ = tx.send(Cmd::Quit);
        }
        if let Some(hook) = STOP_HOOK.get() {
            let _ = hook.lock().map(|mut h| *h = None);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn send(tx: &Sender<Evt>, evt: Evt) {
    let _ = tx.send(evt);
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
}
