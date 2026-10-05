//! Global shortcuts on Wayland, through the GlobalShortcuts desktop portal.
//!
//! Wayland doesn't let an app watch keys pressed in other apps, so the
//! desktop does it for us: we name each shortcut and the keys we'd like for
//! it (the ones set in Settings), the desktop may ask the user to confirm
//! them (GNOME does, once), and it tells us when one is pressed — in a game
//! too. The keys it actually assigns are the desktop's to decide (the user
//! can change them in its settings), so they're reported back for Settings
//! to show.
//!
//! Each set of shortcuts is one portal session on its own thread; changing
//! the shortcuts closes it and binds the new set in a new one. Binding is
//! what makes the desktop ask the user, so it's only done when the desktop
//! doesn't already have these shortcuts as last asked for (remembered in
//! `$XDG_STATE_HOME/hesteclips/shortcuts`): not at every launch.

use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
use futures_util::StreamExt;
use futures_util::future::{Either, select};
use global_hotkey::hotkey::{Code, HotKey, Modifiers};

use crate::settings::ShortcutAction;

/// The app's id with the desktop: the portal keeps our shortcuts under it,
/// and it names the `.desktop` file that says what we're called.
pub const APP_ID: &str = "io.github.kaillr.HesteClips";

/// Tell the desktop portal who we are, before anything else talks to it (it
/// can't tell for an app that isn't sandboxed). Best effort: older portals
/// don't know this call, and then work without it.
pub fn register_app() {
    let Ok(id) = ashpd::AppID::try_from(APP_ID) else { return };
    if let Err(e) = pollster::block_on(ashpd::register_host_app(id)) {
        eprintln!("desktop portal: couldn't register the app ({e}); global shortcuts may not be remembered");
    }
}

/// Whether to use the portal: in a Wayland session, where it's the only way.
pub fn wanted() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland")
}

/// Whether the desktop has the GlobalShortcuts portal.
pub fn available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| pollster::block_on(GlobalShortcuts::new()).is_ok())
}

/// How binding went.
#[derive(Debug, Clone)]
pub enum State {
    Binding,
    /// What the desktop assigned each shortcut, as it describes the keys
    /// ("Alt+F10"); empty when it left one without keys.
    Bound(Vec<(ShortcutAction, String)>),
    Failed(String),
}

/// One bound set of shortcuts. Dropping it unbinds them.
pub struct Portal {
    events: Receiver<ShortcutAction>,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<State>>,
}

impl Portal {
    /// Bind `wanted` (each action with its keys, as `global_hotkey` text) in
    /// the background. `ctx` is woken when one is pressed.
    pub fn bind(wanted: Vec<(ShortcutAction, String)>, ctx: egui::Context) -> Self {
        let (tx, events) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(State::Binding));
        let (stop2, state2) = (stop.clone(), state.clone());
        std::thread::spawn(move || {
            let run = async {
                let proxy = GlobalShortcuts::new().await?;
                let session = proxy.create_session(Default::default()).await?;
                let asked = request_key(&wanted);
                // The desktop may already have them, as we last asked: then
                // there's nothing to ask the user.
                let known = proxy.list_shortcuts(&session, Default::default()).await?.response()?;
                let has_all = wanted.iter().all(|(a, _)| known.shortcuts().iter().any(|s| s.id() == id(*a)));
                let shortcuts = if has_all && last_request().as_deref() == Some(asked.as_str()) {
                    known.shortcuts().to_vec()
                } else {
                    let new: Vec<NewShortcut> = wanted
                        .iter()
                        .map(|(action, keys)| NewShortcut::new(id(*action), action.label()).preferred_trigger(trigger(keys).as_deref()))
                        .collect();
                    let bound = proxy.bind_shortcuts(&session, &new, None, Default::default()).await?.response()?;
                    save_request(&asked);
                    bound.shortcuts().to_vec()
                };
                let assigned = shortcuts.iter().filter_map(|s| Some((action(s.id())?, s.trigger_description().to_owned()))).collect();
                *state2.lock().unwrap() = State::Bound(assigned);
                ctx.request_repaint();
                let mut activated = pin!(proxy.receive_activated().await?);
                loop {
                    // Woken by a press, or now and then to see if we're done.
                    let tick = pin!(async_io::Timer::after(Duration::from_millis(250)));
                    match select(activated.next(), tick).await {
                        Either::Left((Some(press), _)) => {
                            if let Some(a) = action(press.shortcut_id()) {
                                let _ = tx.send(a);
                                ctx.request_repaint();
                            }
                        }
                        Either::Left((None, _)) => break,
                        Either::Right(_) if stop2.load(Ordering::Relaxed) => break,
                        Either::Right(_) => {}
                    }
                }
                let _ = session.close().await;
                Ok::<(), ashpd::Error>(())
            };
            if let Err(e) = pollster::block_on(run) {
                let why = match e {
                    ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled) => "not allowed in the desktop's dialog".to_owned(),
                    e => e.to_string(),
                };
                *state2.lock().unwrap() = State::Failed(why);
                ctx.request_repaint();
            }
        });
        Self { events, stop, state }
    }

    /// Shortcuts pressed since last asked.
    pub fn pressed(&self) -> Vec<ShortcutAction> {
        self.events.try_iter().collect()
    }

    pub fn state(&self) -> State {
        self.state.lock().unwrap().clone()
    }
}

impl Drop for Portal {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// What a bind asks for, as one line: each shortcut's id and keys.
fn request_key(wanted: &[(ShortcutAction, String)]) -> String {
    wanted.iter().map(|(a, keys)| format!("{}={}", id(*a), trigger(keys).unwrap_or_default())).collect::<Vec<_>>().join(";")
}

/// `$XDG_STATE_HOME/hesteclips/shortcuts` (`~/.local/state/…`).
fn state_file() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state")))?;
    Some(base.join("hesteclips").join("shortcuts"))
}

/// What the desktop was last asked for and granted.
fn last_request() -> Option<String> {
    std::fs::read_to_string(state_file()?).ok().map(|s| s.trim().to_owned())
}

fn save_request(key: &str) {
    let Some(file) = state_file() else { return };
    let _ = file.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(file, key));
}

fn id(action: ShortcutAction) -> &'static str {
    match action {
        ShortcutAction::SaveClip => "save-clip",
        ShortcutAction::ToggleBuffer => "toggle-replay-buffer",
        ShortcutAction::ToggleRecord => "toggle-recording",
    }
}

fn action(id: &str) -> Option<ShortcutAction> {
    ShortcutAction::ALL.into_iter().find(|a| self::id(*a) == id)
}

/// A `global_hotkey` shortcut ("alt+F10") as the XDG shortcuts spec writes
/// it ("ALT+F10"): modifiers, then the key's XKB name.
fn trigger(text: &str) -> Option<String> {
    let hk: HotKey = text.parse().ok()?;
    let mut parts: Vec<String> = [(Modifiers::CONTROL, "CTRL"), (Modifiers::ALT, "ALT"), (Modifiers::SHIFT, "SHIFT"), (Modifiers::SUPER, "LOGO")]
        .iter()
        .filter(|(m, _)| hk.mods.contains(*m))
        .map(|(_, n)| (*n).to_owned())
        .collect();
    parts.push(keysym(hk.key)?);
    Some(parts.join("+"))
}

fn keysym(code: Code) -> Option<String> {
    let raw = code.to_string();
    if let Some(letter) = raw.strip_prefix("Key") {
        return Some(letter.to_ascii_lowercase());
    }
    if let Some(digit) = raw.strip_prefix("Digit") {
        return Some(digit.to_owned());
    }
    if raw.len() > 1 && raw.starts_with('F') && raw[1..].chars().all(|c| c.is_ascii_digit()) {
        return Some(raw);
    }
    Some(
        match code {
            Code::Space => "space",
            Code::Enter => "Return",
            Code::Tab => "Tab",
            Code::Backspace => "BackSpace",
            Code::Delete => "Delete",
            Code::Insert => "Insert",
            Code::Home => "Home",
            Code::End => "End",
            Code::PageUp => "Page_Up",
            Code::PageDown => "Page_Down",
            Code::ArrowUp => "Up",
            Code::ArrowDown => "Down",
            Code::ArrowLeft => "Left",
            Code::ArrowRight => "Right",
            Code::Minus => "minus",
            Code::Equal => "equal",
            Code::Comma => "comma",
            Code::Period => "period",
            Code::Slash => "slash",
            Code::Backslash => "backslash",
            Code::Semicolon => "semicolon",
            Code::Quote => "apostrophe",
            Code::Backquote => "grave",
            Code::BracketLeft => "bracketleft",
            Code::BracketRight => "bracketright",
            _ => return None,
        }
        .to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers_follow_the_xdg_spec() {
        assert_eq!(trigger("alt+F10").as_deref(), Some("ALT+F10"));
        assert_eq!(trigger("control+shift+KeyS").as_deref(), Some("CTRL+SHIFT+s"));
        assert_eq!(trigger("super+Digit1").as_deref(), Some("LOGO+1"));
        assert_eq!(trigger("F9").as_deref(), Some("F9"));
        assert_eq!(trigger("alt+PageUp").as_deref(), Some("ALT+Page_Up"));
        assert_eq!(trigger(""), None);
    }

    #[test]
    fn ids_round_trip() {
        for a in ShortcutAction::ALL {
            assert_eq!(action(id(a)), Some(a));
        }
    }
}
