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
//! the shortcuts closes it and binds the new set in a new one. They're bound
//! at every start: binding is what makes them work (KDE Plasma 6.7.4 and
//! later only lists the ones bound before, it doesn't turn them on), and the
//! desktop only asks the user about ones it hasn't seen. Keys the user has set
//! are the desktop's to keep, so a change is made in its own shortcut editor,
//! which the portal opens (`Portal::configure`); what it assigns comes back
//! for Settings to show.
//!
//! KDE Plasma asks about new shortcuts once; closed or turned down, they're
//! kept with no keys and it never asks again. There the keys are set in its
//! shortcut service directly (`kde`), as its own settings do: the keys set in
//! Settings are the ones that work, and keys changed in Plasma's settings
//! come back to ours.

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

/// Whether the desktop's portal can open its own editor for our shortcuts
/// (version 2 of GlobalShortcuts). Asked once.
pub fn can_configure() -> bool {
    static CAN: OnceLock<bool> = OnceLock::new();
    *CAN.get_or_init(|| pollster::block_on(async { GlobalShortcuts::new().await.is_ok_and(|p| p.version() >= 2) }))
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
    /// Presses, and whether it was done already (`QuickSave`).
    events: Receiver<(ShortcutAction, bool)>,
    stop: Arc<AtomicBool>,
    /// Open the desktop's shortcut editor for ours, on the next tick.
    configure: Arc<AtomicBool>,
    state: Arc<Mutex<State>>,
    /// The keys are set here (KDE Plasma), not in the desktop's settings.
    managed: Arc<AtomicBool>,
    /// Keys changed in the desktop's settings, for ours (`global_hotkey`
    /// text, empty for none).
    adopted: Arc<Mutex<Vec<(ShortcutAction, String)>>>,
}

impl Portal {
    /// Bind `wanted` (each action with its keys, as `global_hotkey` text) in
    /// the background. `ctx` is woken when one is pressed; Save clip and
    /// Take a screenshot are done through `quick` as they're pressed, as
    /// the UI may not run then (a hidden window gets no frames). `ours`: they were
    /// just changed in Settings, so where the keys are set here (KDE), ours
    /// win; else (starting up) keys changed in the desktop's settings meanwhile
    /// are taken, and only missing ones set.
    pub fn bind(wanted: Vec<(ShortcutAction, String)>, ours: bool, ctx: egui::Context, quick: crate::service::QuickSave) -> Self {
        let (tx, events) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(State::Binding));
        let configure = Arc::new(AtomicBool::new(false));
        let managed = Arc::new(AtomicBool::new(false));
        let adopted: Arc<Mutex<Vec<(ShortcutAction, String)>>> = Default::default();
        let (stop2, state2, configure2, managed2, adopted2) = (stop.clone(), state.clone(), configure.clone(), managed.clone(), adopted.clone());
        std::thread::spawn(move || {
            let run = || async {
                let proxy = GlobalShortcuts::new().await?;
                let session = proxy.create_session(Default::default()).await?;
                // Bound every time: that's what turns them on. The desktop
                // asks the user only about ones it hasn't seen, and keeps the
                // keys of the ones it has.
                let new: Vec<NewShortcut> = wanted
                    .iter()
                    .map(|(action, keys)| NewShortcut::new(id(*action), action.label()).preferred_trigger(trigger(keys).as_deref()))
                    .collect();
                let bound = proxy.bind_shortcuts(&session, &new, None, Default::default()).await?.response()?;
                let assigned = |list: &[ashpd::desktop::global_shortcuts::Shortcut]| list.iter().filter_map(|s| Some((action(s.id())?, s.trigger_description().to_owned()))).collect();
                *state2.lock().unwrap() = State::Bound(assigned(bound.shortcuts()));
                // KDE: the keys, as they are there (`None`: none).
                let mut kde_keys: Option<(zbus::Connection, Vec<(ShortcutAction, Option<i32>)>)> = None;
                if let Some(conn) = kde::connect().await {
                    let mut now = Vec::new();
                    for (action, keys) in &wanted {
                        let Ok(theirs) = kde::key(&conn, *action).await else { continue };
                        let mine = kde::to_qt(keys);
                        let mut kept = theirs;
                        if theirs != mine {
                            match theirs.map(kde::from_qt) {
                                Some(Some(text)) if !ours => adopted2.lock().unwrap().push((*action, text)),
                                // Keys we can't write in Settings: left as they are.
                                Some(None) if !ours => {}
                                _ => match kde::set(&conn, *action, mine).await {
                                    Ok(()) => kept = mine,
                                    Err(e) => eprintln!("KDE shortcuts: couldn't set the keys for {}: {e}", action.label()),
                                },
                            }
                        }
                        now.push((*action, kept));
                    }
                    managed2.store(now.len() == wanted.len(), Ordering::Relaxed);
                    kde_keys = Some((conn, now));
                }
                ctx.request_repaint();
                let mut activated = pin!(proxy.receive_activated().await?);
                let mut changed = pin!(proxy.receive_shortcuts_changed().await?);
                loop {
                    // Woken by a press, new keys from the desktop's editor, or
                    // now and then to see if we're done or asked to open it.
                    let tick = pin!(async_io::Timer::after(Duration::from_millis(250)));
                    match select(activated.next(), select(changed.next(), tick)).await {
                        Either::Left((Some(press), _)) => {
                            if let Some(a) = action(press.shortcut_id()) {
                                let done = match a {
                                    ShortcutAction::SaveClip => quick.save(),
                                    ShortcutAction::Screenshot => quick.screenshot(),
                                    _ => false,
                                };
                                let _ = tx.send((a, done));
                                ctx.request_repaint();
                            }
                        }
                        Either::Left((None, _)) => break,
                        Either::Right((Either::Left((Some(now), _)), _)) => {
                            *state2.lock().unwrap() = State::Bound(assigned(now.shortcuts()));
                            // Changed in KDE's settings: ours follow.
                            if let Some((conn, known)) = &mut kde_keys {
                                for (action, was) in known.iter_mut() {
                                    if let Ok(k) = kde::key(conn, *action).await
                                        && k != *was
                                    {
                                        *was = k;
                                        let text = k.map_or(Some(String::new()), kde::from_qt);
                                        if let Some(text) = text {
                                            adopted2.lock().unwrap().push((*action, text));
                                        }
                                    }
                                }
                            }
                            ctx.request_repaint();
                        }
                        Either::Right((Either::Left((None, _)), _)) => break,
                        Either::Right(_) if stop2.load(Ordering::Relaxed) => break,
                        Either::Right(_) => {
                            if configure2.swap(false, Ordering::Relaxed)
                                && let Err(e) = proxy.configure_shortcuts(&session, None, Default::default()).await
                            {
                                eprintln!("desktop portal: couldn't open the shortcut settings: {e}");
                            }
                        }
                    }
                }
                let _ = session.close().await;
                Ok::<(), ashpd::Error>(())
            };
            let mut result = pollster::block_on(run());
            // Turned down in KDE's dialog: it keeps them anyway, without keys,
            // and binds them without asking again; their keys are set here.
            if matches!(result, Err(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled))) && pollster::block_on(kde::connect()).is_some() {
                result = pollster::block_on(run());
            }
            if let Err(e) = result {
                let why = match e {
                    ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled) => "not allowed in the desktop's dialog".to_owned(),
                    e => e.to_string(),
                };
                *state2.lock().unwrap() = State::Failed(why);
                ctx.request_repaint();
            }
        });
        Self { events, stop, configure, state, managed, adopted }
    }

    /// Whether the keys are set from here (KDE Plasma), so they're changed in
    /// Settings like anywhere else.
    pub fn managed(&self) -> bool {
        self.managed.load(Ordering::Relaxed)
    }

    /// Keys changed in the desktop's settings since last asked, for ours.
    pub fn adopted(&self) -> Vec<(ShortcutAction, String)> {
        std::mem::take(&mut *self.adopted.lock().unwrap())
    }

    /// Open the desktop's own editor for our shortcuts (where their keys are
    /// changed: the desktop keeps the ones the user set).
    pub fn configure(&self) {
        self.configure.store(true, Ordering::Relaxed);
    }

    /// Shortcuts pressed since last asked, and whether each was done already.
    pub fn pressed(&self) -> Vec<(ShortcutAction, bool)> {
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

fn id(action: ShortcutAction) -> &'static str {
    match action {
        ShortcutAction::SaveClip => "save-clip",
        ShortcutAction::ToggleBuffer => "toggle-replay-buffer",
        ShortcutAction::ToggleRecord => "toggle-recording",
        ShortcutAction::Screenshot => "take-screenshot",
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

/// KDE Plasma's shortcut service (kglobalaccel), where its portal keeps our
/// shortcuts, under our app id. Keys there are Qt key codes: the key with
/// its modifiers' bits.
mod kde {
    use super::{APP_ID, ShortcutAction, id};
    use global_hotkey::hotkey::{Code, HotKey, Modifiers};

    const SHIFT: i32 = 0x0200_0000;
    const CTRL: i32 = 0x0400_0000;
    const ALT: i32 = 0x0800_0000;
    const META: i32 = 0x1000_0000;
    const MODS: [(Modifiers, i32); 4] = [(Modifiers::SHIFT, SHIFT), (Modifiers::CONTROL, CTRL), (Modifiers::ALT, ALT), (Modifiers::SUPER, META)];

    /// The session bus, on a Plasma desktop.
    pub async fn connect() -> Option<zbus::Connection> {
        let plasma = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.split(':').any(|d| d.eq_ignore_ascii_case("KDE")));
        if !plasma {
            return None;
        }
        zbus::Connection::session().await.inspect_err(|e| eprintln!("KDE shortcuts: {e}")).ok()
    }

    fn action_id(action: ShortcutAction) -> Vec<&'static str> {
        vec![APP_ID, id(action), "HesteClips", action.label()]
    }

    /// The first keys a shortcut has there, if any.
    pub async fn key(conn: &zbus::Connection, action: ShortcutAction) -> zbus::Result<Option<i32>> {
        let reply = conn.call_method(Some("org.kde.kglobalaccel"), "/kglobalaccel", Some("org.kde.KGlobalAccel"), "shortcutKeys", &(action_id(action),)).await?;
        let keys: Vec<(Vec<i32>,)> = reply.body().deserialize()?;
        Ok(keys.first().and_then(|(k,)| k.first().copied()).filter(|k| *k != 0))
    }

    /// Give a shortcut these keys there (or none).
    pub async fn set(conn: &zbus::Connection, action: ShortcutAction, key: Option<i32>) -> zbus::Result<()> {
        let keys: Vec<(Vec<i32>,)> = key.map(|k| (vec![k, 0, 0, 0],)).into_iter().collect();
        conn.call_method(Some("org.kde.kglobalaccel"), "/kglobalaccel", Some("org.kde.KGlobalAccel"), "setForeignShortcutKeys", &(action_id(action), keys)).await?;
        Ok(())
    }

    /// `global_hotkey` text as a Qt key code.
    pub fn to_qt(text: &str) -> Option<i32> {
        let hk: HotKey = text.parse().ok()?;
        let key = KEYS.iter().find(|(c, _)| *c == hk.key)?.1;
        Some(MODS.iter().filter(|(m, _)| hk.mods.contains(*m)).fold(key, |k, (_, bit)| k | bit))
    }

    /// A Qt key code as `global_hotkey` text, for keys we know.
    pub fn from_qt(qt: i32) -> Option<String> {
        let key = qt & 0x01ff_ffff;
        let code = KEYS.iter().find(|(_, k)| *k == key)?.0;
        let mods = MODS.iter().filter(|(_, bit)| qt & bit != 0).fold(Modifiers::empty(), |m, (flag, _)| m | *flag);
        Some(HotKey::new(Some(mods), code).into_string())
    }

    /// Keys and their Qt codes (`Qt::Key`).
    const KEYS: &[(Code, i32)] = &[
        (Code::KeyA, 0x41), (Code::KeyB, 0x42), (Code::KeyC, 0x43), (Code::KeyD, 0x44), (Code::KeyE, 0x45), (Code::KeyF, 0x46), (Code::KeyG, 0x47),
        (Code::KeyH, 0x48), (Code::KeyI, 0x49), (Code::KeyJ, 0x4a), (Code::KeyK, 0x4b), (Code::KeyL, 0x4c), (Code::KeyM, 0x4d), (Code::KeyN, 0x4e),
        (Code::KeyO, 0x4f), (Code::KeyP, 0x50), (Code::KeyQ, 0x51), (Code::KeyR, 0x52), (Code::KeyS, 0x53), (Code::KeyT, 0x54), (Code::KeyU, 0x55),
        (Code::KeyV, 0x56), (Code::KeyW, 0x57), (Code::KeyX, 0x58), (Code::KeyY, 0x59), (Code::KeyZ, 0x5a),
        (Code::Digit0, 0x30), (Code::Digit1, 0x31), (Code::Digit2, 0x32), (Code::Digit3, 0x33), (Code::Digit4, 0x34),
        (Code::Digit5, 0x35), (Code::Digit6, 0x36), (Code::Digit7, 0x37), (Code::Digit8, 0x38), (Code::Digit9, 0x39),
        (Code::F1, 0x0100_0030), (Code::F2, 0x0100_0031), (Code::F3, 0x0100_0032), (Code::F4, 0x0100_0033), (Code::F5, 0x0100_0034), (Code::F6, 0x0100_0035),
        (Code::F7, 0x0100_0036), (Code::F8, 0x0100_0037), (Code::F9, 0x0100_0038), (Code::F10, 0x0100_0039), (Code::F11, 0x0100_003a), (Code::F12, 0x0100_003b),
        (Code::F13, 0x0100_003c), (Code::F14, 0x0100_003d), (Code::F15, 0x0100_003e), (Code::F16, 0x0100_003f), (Code::F17, 0x0100_0040), (Code::F18, 0x0100_0041),
        (Code::F19, 0x0100_0042), (Code::F20, 0x0100_0043), (Code::F21, 0x0100_0044), (Code::F22, 0x0100_0045), (Code::F23, 0x0100_0046), (Code::F24, 0x0100_0047),
        (Code::Space, 0x20), (Code::Enter, 0x0100_0004), (Code::Tab, 0x0100_0001), (Code::Backspace, 0x0100_0003), (Code::Delete, 0x0100_0007),
        (Code::Insert, 0x0100_0006), (Code::Home, 0x0100_0010), (Code::End, 0x0100_0011), (Code::PageUp, 0x0100_0016), (Code::PageDown, 0x0100_0017),
        (Code::ArrowLeft, 0x0100_0012), (Code::ArrowUp, 0x0100_0013), (Code::ArrowRight, 0x0100_0014), (Code::ArrowDown, 0x0100_0015),
        (Code::Pause, 0x0100_0008), (Code::PrintScreen, 0x0100_0009), (Code::ScrollLock, 0x0100_0026),
        (Code::Minus, 0x2d), (Code::Equal, 0x3d), (Code::Comma, 0x2c), (Code::Period, 0x2e), (Code::Slash, 0x2f), (Code::Backslash, 0x5c),
        (Code::Semicolon, 0x3b), (Code::Quote, 0x27), (Code::Backquote, 0x60), (Code::BracketLeft, 0x5b), (Code::BracketRight, 0x5d),
    ];
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
    fn qt_keys_round_trip() {
        // As KDE keeps Alt+F10.
        assert_eq!(kde::to_qt("alt+F10"), Some(150_995_001));
        assert_eq!(kde::from_qt(150_995_001).as_deref(), Some("alt+F10"));
        assert_eq!(kde::from_qt(kde::to_qt("control+shift+KeyS").unwrap()).as_deref(), Some("shift+control+KeyS"));
    }

    #[test]
    fn ids_round_trip() {
        for a in ShortcutAction::ALL {
            assert_eq!(action(id(a)), Some(a));
        }
    }
}
