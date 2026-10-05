//! The ScreenCast desktop portal: how any app gets to see the screen on a
//! Wayland desktop (and on X11 under GNOME and KDE). The first time, the
//! desktop asks which screen to share; the answer is remembered with a restore
//! token, so later recordings start without asking until the token is revoked
//! (or [`forget_choice`] is called).
//!
//! What it hands back is a PipeWire remote (a file descriptor) with one video
//! node on it, which `super::screen` reads frames from.

use std::os::fd::OwnedFd;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{PersistMode, Session};

/// A running screen cast: hold it for as long as the picture is wanted.
pub(crate) struct Cast {
    proxy: Screencast,
    session: Session<Screencast>,
    /// The PipeWire node with the picture, on the remote behind `fd`.
    pub node: u32,
}

impl Cast {
    /// Ask for the screen: at once if a choice was remembered, otherwise
    /// after the user picks one in the desktop's dialog (this waits for that).
    /// Returns the cast and the PipeWire remote to read it from.
    pub(crate) fn open() -> Result<(Self, OwnedFd)> {
        pollster::block_on(Self::open_async()).map_err(|e| match e {
            ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled) => anyhow!("no screen was picked to record"),
            ashpd::Error::PortalNotFound(_) => anyhow!(
                "the desktop has no screen-capture portal: install xdg-desktop-portal and the one for your desktop \
                 (xdg-desktop-portal-gnome, -kde, -wlr or -hyprland)"
            ),
            e => anyhow!("screen capture through the desktop portal failed: {e}"),
        })
    }

    async fn open_async() -> ashpd::Result<(Self, OwnedFd)> {
        let proxy = Screencast::new().await?;
        let session = proxy.create_session(Default::default()).await?;
        // The pointer drawn into the picture, as on macOS and Windows;
        // where the desktop can't, none rather than a separate one.
        let cursors = proxy.available_cursor_modes().await.unwrap_or_default();
        let cursor = if cursors.contains(CursorMode::Embedded) { CursorMode::Embedded } else { CursorMode::Hidden };
        let token = load_state().token;
        proxy
            .select_sources(
                &session,
                SelectSourcesOptions::default()
                    .set_cursor_mode(cursor)
                    .set_sources(ashpd::enumflags2::BitFlags::from(SourceType::Monitor))
                    .set_multiple(false)
                    .set_restore_token(token.as_deref())
                    .set_persist_mode(PersistMode::ExplicitlyRevoked),
            )
            .await?
            .response()?;
        let streams = proxy.start(&session, None, Default::default()).await?.response()?;
        let stream = streams.streams().first().ok_or(ashpd::Error::NoResponse)?;
        let node = stream.pipe_wire_node_id();
        let size = stream.size().filter(|(w, h)| *w > 0 && *h > 0).map(|(w, h)| (w as u32, h as u32));
        // A restore token is good once: keep the new one for next time.
        save_state(&State { token: streams.restore_token().map(str::to_owned), size: size.or(load_state().size) });
        let fd = proxy.open_pipe_wire_remote(&session, Default::default()).await?;
        Ok((Self { proxy, session, node }, fd))
    }

    /// Stop sharing the screen (the desktop's "sharing" indicator goes away).
    pub(crate) fn close(self) {
        let _ = pollster::block_on(self.session.close());
        drop(self.proxy);
    }
}

/// Forget which screen was picked, so the next recording asks again.
pub fn forget_choice() {
    let state = load_state();
    save_state(&State { token: None, size: state.size });
}

/// The size of the screen last recorded, if any was.
pub(crate) fn last_size() -> Option<(u32, u32)> {
    load_state().size
}

/// Remember the screen's size once its frames say what it is (the portal
/// doesn't always).
pub(crate) fn remember_size(size: (u32, u32)) {
    let state = load_state();
    if state.size != Some(size) {
        save_state(&State { token: state.token, size: Some(size) });
    }
}

/// What's remembered between runs.
#[derive(Default)]
struct State {
    token: Option<String>,
    size: Option<(u32, u32)>,
}

/// `$XDG_STATE_HOME/hesteclips/screencast` (`~/.local/state/…`).
fn state_file() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("hesteclips").join("screencast"))
}

/// Two lines: the restore token (may be empty), and `WIDTHxHEIGHT`.
fn load_state() -> State {
    let Some(text) = state_file().and_then(|f| std::fs::read_to_string(f).ok()) else { return State::default() };
    let mut lines = text.lines();
    let token = lines.next().map(str::trim).filter(|t| !t.is_empty()).map(str::to_owned);
    let size = lines.next().and_then(|l| {
        let (w, h) = l.trim().split_once('x')?;
        Some((w.parse().ok()?, h.parse().ok()?))
    });
    State { token, size }
}

fn save_state(state: &State) {
    let Some(file) = state_file() else { return };
    let text = format!(
        "{}\n{}\n",
        state.token.as_deref().unwrap_or(""),
        state.size.map(|(w, h)| format!("{w}x{h}")).unwrap_or_default()
    );
    let written = file.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&file, text));
    if let Err(e) = written.with_context(|| format!("couldn't save {}", file.display())) {
        eprintln!("{e:#}");
    }
}
