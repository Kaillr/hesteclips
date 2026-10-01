//! Runs the capture backend on its own thread so blocking work (finishing a
//! file, writing a saved clip) never freezes the egui UI.
//!
//! The UI sends `Cmd`s and drains `Evt`s each frame; it never calls the recorder
//! directly.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use capture::{EncodeSettings, Mode};

enum Cmd {
    Start(Mode, EncodeSettings),
    SaveClip,
    Stop,
    /// Stop, then signal once the file is finished (app being killed).
    StopAndAck(Sender<()>),
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
                            send(&evt_tx, Evt::Error(e.to_string()));
                            send(&evt_tx, Evt::State(None));
                        }
                    },
                    // Save doesn't change capture state — still buffering afterwards.
                    Cmd::SaveClip => match recorder.save_clip() {
                        Ok(path) => send(&evt_tx, Evt::Saved(path)),
                        Err(e) => send(&evt_tx, Evt::Error(e.to_string())),
                    },
                    Cmd::Stop => {
                        match recorder.stop() {
                            Ok(Some(path)) => send(&evt_tx, Evt::Saved(path)),
                            Ok(None) => {}
                            Err(e) => send(&evt_tx, Evt::Error(e.to_string())),
                        }
                        send(&evt_tx, Evt::State(None));
                    }
                    Cmd::StopAndAck(ack) => {
                        let _ = recorder.stop();
                        let _ = ack.send(());
                    }
                }
            }
        });

        Self { cmd_tx: Some(cmd_tx), evt_rx, thread: Some(thread) }
    }

    pub fn start(&self, mode: Mode, settings: EncodeSettings) {
        self.send(Cmd::Start(mode, settings));
    }
    pub fn save_clip(&self) {
        self.send(Cmd::SaveClip);
    }
    pub fn stop(&self) {
        self.send(Cmd::Stop);
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
    /// Quitting the app must stop capture: close the command channels so the
    /// thread exits its loop and drops the recorder (which finishes any recording),
    /// and wait for that so the process doesn't exit mid-write.
    fn drop(&mut self) {
        self.cmd_tx = None;
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
