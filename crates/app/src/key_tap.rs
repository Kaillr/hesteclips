//! macOS: shortcuts that keep working while a game holds the screen.
//!
//! A game in true fullscreen captures the display (`CGCaptureAllDisplays`),
//! and while it does, macOS doesn't deliver system-wide shortcuts
//! (`RegisterEventHotKey`, what `global_hotkey` uses) to any other app. A
//! listen-only event tap at the HID level sees keys before that happens, so
//! it still does (tested: a press while the display was captured reached the
//! tap and not the hotkey). It needs Input Monitoring, which the user grants
//! in System Settings, so the regular shortcuts stay as they are for everyone
//! and the tap is added when it's allowed.
//!
//! While the tap runs it does the firing, and the regular shortcut only keeps
//! the keys from reaching other apps: both seeing one press would save two
//! clips. The exception is Secure Input (a password field focused), which
//! hides keys from the tap but not from the regular shortcut.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use objc2_core_foundation::{CFMachPort, CFRetained, CFRunLoop, kCFRunLoopCommonModes};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventMask, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy, CGEventType,
    CGPreflightListenEventAccess, CGRequestListenEventAccess,
};

/// Whether Input Monitoring is granted. Cheap.
pub fn allowed() -> bool {
    CGPreflightListenEventAccess()
}

/// Ask for Input Monitoring. The first time, macOS shows its prompt and adds
/// HesteClips to the list in System Settings; after that it only opens the
/// list's pane if it's still off. Whether it's granted now.
pub fn request() -> bool {
    CGRequestListenEventAccess() || {
        open_settings();
        false
    }
}

/// Open System Settings at Privacy & Security, Input Monitoring.
pub fn open_settings() {
    let _ = std::process::Command::new("open").arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent").spawn();
}

/// What a press is handed to: the shortcut's id (as `HotKey::id`).
pub type OnPress = Box<dyn Fn(u32) + Send + Sync>;

/// The shortcuts the tap watches for, as (key code, modifiers, id).
type Watched = Vec<(u16, CGEventFlags, u32)>;

struct Shared {
    watched: Mutex<Watched>,
    on_press: OnPress,
    /// The tap, once made, to turn it back on when macOS turns it off.
    port: Mutex<Option<CFRetained<CFMachPort>>>,
}

// SAFETY: CFMachPort is thread-safe to retain, release and pass to
// CGEventTapEnable from any thread.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

/// The running tap, once started. It lives for the rest of the app: it
/// costs nothing idle, and one that couldn't be made is tried again.
static TAP: Mutex<Option<Arc<Shared>>> = Mutex::new(None);
/// Started and taking keys.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Whether the tap is running, so presses come from it.
pub fn running() -> bool {
    RUNNING.load(Ordering::Relaxed)
}

/// Start the tap on a thread of its own, if Input Monitoring is granted and
/// it isn't running yet. Whether it runs now.
pub fn start(on_press: OnPress) -> bool {
    let mut tap = TAP.lock().unwrap();
    if running() {
        return true;
    }
    if !allowed() {
        return false;
    }
    let shared = Arc::new(Shared { watched: Mutex::new(Vec::new()), on_press, port: Mutex::new(None) });
    let (made_tx, made_rx) = std::sync::mpsc::channel();
    let thread_shared = shared.clone();
    let spawned = std::thread::Builder::new().name("key tap".into()).spawn(move || {
        let shared = thread_shared;
        let info = Arc::as_ptr(&shared) as *mut c_void;
        let mask: CGEventMask = 1 << CGEventType::KeyDown.0;
        // SAFETY: `callback` matches CGEventTapCallBack; `info` points into
        // the Arc this thread holds for as long as its run loop runs.
        let port = unsafe { CGEvent::tap_create(CGEventTapLocation::HIDEventTap, CGEventTapPlacement::HeadInsertEventTap, CGEventTapOptions::ListenOnly, mask, Some(callback), info) };
        let Some(port) = port else {
            let _ = made_tx.send(false);
            return;
        };
        let Some(source) = CFMachPort::new_run_loop_source(None, Some(&port), 0) else {
            let _ = made_tx.send(false);
            return;
        };
        let Some(run_loop) = CFRunLoop::current() else {
            let _ = made_tx.send(false);
            return;
        };
        // SAFETY: a static CoreFoundation constant.
        run_loop.add_source(Some(&source), unsafe { kCFRunLoopCommonModes });
        CGEvent::tap_enable(&port, true);
        *shared.port.lock().unwrap() = Some(port);
        RUNNING.store(true, Ordering::Relaxed);
        let _ = made_tx.send(true);
        CFRunLoop::run();
        RUNNING.store(false, Ordering::Relaxed);
    });
    let made = spawned.is_ok() && made_rx.recv().unwrap_or(false);
    if made {
        *tap = Some(shared);
    }
    made
}

/// The shortcuts to watch for. Ones the tap can't see (no key code for the
/// key) are left to the regular shortcut.
pub fn watch(hotkeys: &[HotKey]) {
    let Some(shared) = TAP.lock().unwrap().clone() else { return };
    *shared.watched.lock().unwrap() = hotkeys.iter().filter_map(|hk| Some((key_code(hk.key)?, flags(hk.mods), hk.id()))).collect();
}

/// Whether the tap watches for this shortcut (so the regular one shouldn't fire too).
pub fn watches(id: u32) -> bool {
    running() && TAP.lock().unwrap().as_ref().is_some_and(|s| s.watched.lock().unwrap().iter().any(|(_, _, i)| *i == id))
}

/// Secure Input is on (a password field has the keyboard): the tap sees no
/// keys, so the regular shortcut has to fire.
pub fn keys_hidden() -> bool {
    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn IsSecureEventInputEnabled() -> u8;
    }
    // SAFETY: no arguments, reads a flag.
    unsafe { IsSecureEventInputEnabled() != 0 }
}

/// The modifiers that tell shortcuts apart.
const MODS: CGEventFlags = CGEventFlags::MaskShift.union(CGEventFlags::MaskControl).union(CGEventFlags::MaskAlternate).union(CGEventFlags::MaskCommand);

unsafe extern "C-unwind" fn callback(_proxy: CGEventTapProxy, kind: CGEventType, event: NonNull<CGEvent>, info: *mut c_void) -> *mut CGEvent {
    // SAFETY: `info` is the Arc's pointer given to tap_create, alive for good.
    let shared = unsafe { &*(info as *const Shared) };
    // macOS turns a tap off when it's slow or the user does something odd
    // with input; on again.
    if kind == CGEventType::TapDisabledByTimeout || kind == CGEventType::TapDisabledByUserInput {
        if let Some(port) = shared.port.lock().unwrap().as_ref() {
            CGEvent::tap_enable(port, true);
        }
        return event.as_ptr();
    }
    if kind != CGEventType::KeyDown {
        return event.as_ptr();
    }
    // SAFETY: macOS hands a valid event for the duration of the call.
    let ev = unsafe { event.as_ref() };
    // Held down, a key repeats: one press, one clip.
    if CGEvent::integer_value_field(Some(ev), CGEventField::KeyboardEventAutorepeat) != 0 {
        return event.as_ptr();
    }
    let code = CGEvent::integer_value_field(Some(ev), CGEventField::KeyboardEventKeycode) as u16;
    let mods = CGEvent::flags(Some(ev)) & MODS;
    let hit = shared.watched.lock().unwrap().iter().find(|(c, m, _)| *c == code && *m == mods).map(|(_, _, id)| *id);
    if let Some(id) = hit {
        (shared.on_press)(id);
    }
    event.as_ptr()
}

fn flags(mods: Modifiers) -> CGEventFlags {
    let mut f = CGEventFlags::empty();
    if mods.contains(Modifiers::SHIFT) {
        f |= CGEventFlags::MaskShift;
    }
    if mods.contains(Modifiers::CONTROL) {
        f |= CGEventFlags::MaskControl;
    }
    if mods.contains(Modifiers::ALT) {
        f |= CGEventFlags::MaskAlternate;
    }
    if mods.intersects(Modifiers::SUPER | Modifiers::META) {
        f |= CGEventFlags::MaskCommand;
    }
    f
}

/// The Mac key code for `code`: the same table `global_hotkey` registers with
/// (Carbon's `kVK_*`), so a shortcut means the same key either way.
fn key_code(code: Code) -> Option<u16> {
    Some(match code {
        Code::KeyA => 0x00,
        Code::KeyS => 0x01,
        Code::KeyD => 0x02,
        Code::KeyF => 0x03,
        Code::KeyH => 0x04,
        Code::KeyG => 0x05,
        Code::KeyZ => 0x06,
        Code::KeyX => 0x07,
        Code::KeyC => 0x08,
        Code::KeyV => 0x09,
        Code::KeyB => 0x0b,
        Code::KeyQ => 0x0c,
        Code::KeyW => 0x0d,
        Code::KeyE => 0x0e,
        Code::KeyR => 0x0f,
        Code::KeyY => 0x10,
        Code::KeyT => 0x11,
        Code::Digit1 => 0x12,
        Code::Digit2 => 0x13,
        Code::Digit3 => 0x14,
        Code::Digit4 => 0x15,
        Code::Digit6 => 0x16,
        Code::Digit5 => 0x17,
        Code::Equal => 0x18,
        Code::Digit9 => 0x19,
        Code::Digit7 => 0x1a,
        Code::Minus => 0x1b,
        Code::Digit8 => 0x1c,
        Code::Digit0 => 0x1d,
        Code::BracketRight => 0x1e,
        Code::KeyO => 0x1f,
        Code::KeyU => 0x20,
        Code::BracketLeft => 0x21,
        Code::KeyI => 0x22,
        Code::KeyP => 0x23,
        Code::Enter => 0x24,
        Code::KeyL => 0x25,
        Code::KeyJ => 0x26,
        Code::Quote => 0x27,
        Code::KeyK => 0x28,
        Code::Semicolon => 0x29,
        Code::Backslash => 0x2a,
        Code::Comma => 0x2b,
        Code::Slash => 0x2c,
        Code::KeyN => 0x2d,
        Code::KeyM => 0x2e,
        Code::Period => 0x2f,
        Code::Tab => 0x30,
        Code::Space => 0x31,
        Code::Backquote => 0x32,
        Code::Backspace => 0x33,
        Code::Escape => 0x35,
        Code::F17 => 0x40,
        Code::F18 => 0x4f,
        Code::F19 => 0x50,
        Code::F20 => 0x5a,
        Code::F5 => 0x60,
        Code::F6 => 0x61,
        Code::F7 => 0x62,
        Code::F3 => 0x63,
        Code::F8 => 0x64,
        Code::F9 => 0x65,
        Code::F11 => 0x67,
        Code::F13 => 0x69,
        Code::F16 => 0x6a,
        Code::F14 => 0x6b,
        Code::F10 => 0x6d,
        Code::F12 => 0x6f,
        Code::F15 => 0x71,
        Code::Insert => 0x72,
        Code::Home => 0x73,
        Code::PageUp => 0x74,
        Code::Delete => 0x75,
        Code::F4 => 0x76,
        Code::End => 0x77,
        Code::F2 => 0x78,
        Code::PageDown => 0x79,
        Code::F1 => 0x7a,
        Code::ArrowLeft => 0x7b,
        Code::ArrowRight => 0x7c,
        Code::ArrowDown => 0x7d,
        Code::ArrowUp => 0x7e,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    /// The table agrees with global_hotkey's for every key a shortcut can be
    /// set to (`shortcuts::from_press`).
    #[test]
    fn key_codes_match_the_regular_shortcuts() {
        let keys = [
            "KeyA", "KeyH", "KeyZ", "Digit0", "Digit5", "F1", "F5", "F10", "F12", "F20", "Space", "Enter", "Tab", "Backspace", "Delete", "Insert", "Home", "End", "PageUp", "PageDown",
            "ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "Minus", "Equal", "Comma", "Period", "Slash", "Backslash", "Semicolon", "Quote", "Backquote", "BracketLeft", "BracketRight",
        ];
        for k in keys {
            let hk = HotKey::from_str(&format!("alt+{k}")).unwrap();
            assert!(key_code(hk.key).is_some(), "{k}");
        }
        assert_eq!(key_code(Code::F10), Some(0x6d));
        assert_eq!(key_code(Code::KeyH), Some(0x04));
        assert_eq!(flags(HotKey::from_str("alt+F10").unwrap().mods), CGEventFlags::MaskAlternate);
        assert_eq!(flags(HotKey::from_str("shift+super+KeyS").unwrap().mods), CGEventFlags::MaskShift | CGEventFlags::MaskCommand);
    }
}
