//! Keeping the installed app up to date.
//!
//! Releases are published to GitHub by the release workflow and installed with
//! Velopack (per user, no admin prompt). The installed app checks in the
//! background, downloads a new version quietly and installs it the next time
//! you quit — nothing ever interrupts a game or a recording. Once one is
//! downloaded, an "Update ready" button offers to restart into it now.
//!
//! A copy run from the build folder (`cargo run`) isn't installed, so it has
//! nothing to update: [`Status::Unavailable`].
//!
//! The installer is only started once the app has shut down completely
//! ([`apply_queued`], last thing in `main`): it's meant to wait for the app to
//! exit, but on some PCs Windows won't let it ("Access is denied"), and then
//! it goes ahead at once and kills the app if it's still running — which,
//! mid-way through stopping a capture and finishing its file, looked like a
//! crash.

// Only the installed Windows app updates itself so far.
#![cfg_attr(not(windows), allow(dead_code))]

use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

/// Where releases are published.
pub const REPO: &str = "https://github.com/Kaillr/hesteclips";

/// First check after launch: late enough not to slow start-up.
const FIRST_CHECK_AFTER: Duration = Duration::from_secs(10);
/// Then this often while the app runs (a guess: often enough that an app left
/// open for days still finds updates, rare enough to be no load on GitHub).
const CHECK_EVERY: Duration = Duration::from_secs(4 * 60 * 60);

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    /// Not an installed copy (a development build): nothing to update.
    Unavailable,
    /// Hasn't checked yet.
    Idle,
    Checking,
    /// Downloading a new version, 0–100 %.
    Downloading(u8),
    UpToDate,
    /// This version is downloaded and installs on quit or restart.
    Ready(String),
    Failed(String),
}

pub struct Updater {
    #[cfg(windows)]
    manager: Option<velopack::UpdateManager>,
    shared: Arc<Mutex<Shared>>,
    /// Asks the background thread to check now.
    check_now: Option<mpsc::Sender<()>>,
}

struct Shared {
    status: Status,
    /// Check and download on our own (the setting).
    auto: bool,
    /// The downloaded update.
    #[cfg(windows)]
    ready: Option<velopack::VelopackAsset>,
}

/// The update to install once the app has shut down, and whether to start
/// the new version after.
#[cfg(windows)]
static QUEUED: Mutex<Option<(velopack::UpdateManager, velopack::VelopackAsset, bool)>> = Mutex::new(None);

impl Updater {
    pub fn new(ctx: egui::Context, auto: bool) -> Self {
        let shared = Arc::new(Mutex::new(Shared {
            status: Status::Unavailable,
            auto,
            #[cfg(windows)]
            ready: None,
        }));
        #[cfg(windows)]
        {
            // Fails when not installed through the installer.
            let manager = velopack::UpdateManager::new(velopack::sources::GithubSource::new(REPO, None, false), None, None).ok();
            let check_now = manager.as_ref().map(|m| {
                {
                    let mut s = shared.lock().unwrap();
                    s.status = Status::Idle;
                    // Downloaded last time, but the app didn't quit normally.
                    if let Some(asset) = m.get_update_pending_restart() {
                        s.status = Status::Ready(asset.Version.clone());
                        s.ready = Some(asset);
                    }
                }
                let (tx, rx) = mpsc::channel();
                let (m, shared) = (m.clone(), shared.clone());
                std::thread::Builder::new()
                    .name("updates".into())
                    .spawn(move || run(m, shared, rx, ctx))
                    .expect("spawn update thread");
                tx
            });
            Self { manager, shared, check_now }
        }
        #[cfg(not(windows))]
        {
            let _ = ctx;
            Self { shared, check_now: None }
        }
    }

    pub fn status(&self) -> Status {
        self.shared.lock().unwrap().status.clone()
    }

    /// The version running, as the installer knows it.
    pub fn version(&self) -> String {
        #[cfg(windows)]
        if let Some(m) = &self.manager {
            return m.get_current_version_as_string();
        }
        env!("CARGO_PKG_VERSION").to_owned()
    }

    pub fn set_auto(&self, auto: bool) {
        self.shared.lock().unwrap().auto = auto;
    }

    /// Check (and download) now, whatever the setting.
    pub fn check(&self) {
        if let Some(tx) = &self.check_now {
            let _ = tx.send(());
        }
    }

    /// Install the downloaded update once the app has shut down ([`apply_queued`])
    /// — and start the new version after, if `restart`. The caller closes the
    /// app. Returns false when there's nothing to install.
    pub fn install_on_exit(&self, restart: bool) -> bool {
        #[cfg(windows)]
        {
            let s = self.shared.lock().unwrap();
            if let (Some(m), Some(asset)) = (&self.manager, &s.ready) {
                let mut queued = QUEUED.lock().unwrap();
                // Quitting after "Update ready" was clicked still restarts.
                let restart = restart || queued.as_ref().is_some_and(|(_, _, r)| *r);
                *queued = Some((m.clone(), asset.clone(), restart));
                return true;
            }
        }
        let _ = restart;
        false
    }
}

/// Start the installer for an update queued by [`Updater::install_on_exit`].
/// Called at the very end, once capture has stopped and every file is
/// finished, so nothing is lost if the installer doesn't wait for us.
pub fn apply_queued() {
    #[cfg(windows)]
    if let Some((m, asset, restart)) = QUEUED.lock().ok().and_then(|mut q| q.take()) {
        // Silent when quitting: no window should pop up after the app is gone.
        if let Err(e) = m.wait_exit_then_apply_updates(&asset, !restart, restart, Vec::<String>::new()) {
            eprintln!("couldn't install the update: {e}");
        }
    }
}

/// The background thread: check on a schedule (when automatic) or on request,
/// downloading whatever's new.
#[cfg(windows)]
fn run(m: velopack::UpdateManager, shared: Arc<Mutex<Shared>>, check_now: mpsc::Receiver<()>, ctx: egui::Context) {
    let set = |status: Status| {
        shared.lock().unwrap().status = status;
        ctx.request_repaint();
    };
    let mut wait = FIRST_CHECK_AFTER;
    loop {
        let asked = match check_now.recv_timeout(wait) {
            Ok(()) => true,
            Err(mpsc::RecvTimeoutError::Timeout) => false,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        wait = CHECK_EVERY;
        {
            let s = shared.lock().unwrap();
            // One update at a time: the next check is after it's installed.
            if (!asked && !s.auto) || s.ready.is_some() {
                continue;
            }
        }
        set(Status::Checking);
        let update = match m.check_for_updates() {
            Ok(velopack::UpdateCheck::UpdateAvailable(info)) => info,
            Ok(_) => {
                set(Status::UpToDate);
                continue;
            }
            Err(e) => {
                set(Status::Failed(format!("Couldn't check for updates: {e}")));
                continue;
            }
        };
        set(Status::Downloading(0));
        let (progress_tx, progress_rx) = mpsc::channel::<i16>();
        let progress = {
            let (shared, ctx) = (shared.clone(), ctx.clone());
            std::thread::spawn(move || {
                for p in progress_rx {
                    shared.lock().unwrap().status = Status::Downloading(p.clamp(0, 100) as u8);
                    ctx.request_repaint();
                }
            })
        };
        let result = m.download_updates(&update, Some(progress_tx));
        let _ = progress.join();
        match result {
            Ok(()) => {
                let asset = update.TargetFullRelease;
                let mut s = shared.lock().unwrap();
                s.status = Status::Ready(asset.Version.clone());
                s.ready = Some(asset);
                ctx.request_repaint();
            }
            Err(e) => set(Status::Failed(format!("Couldn't download the update: {e}"))),
        }
    }
}
