//! Global shortcuts: registering them with the OS, showing them, and turning a
//! key press in the Settings recorder into a shortcut.
//!
//! Keys are shown by name, as keycaps ([Option] [F10]) or as "Option + F10" in
//! plain text: easier to read than ⌥F10 for anyone who doesn't know the Mac
//! symbols, and ⌥ and ⌃ are easy to mix up.
//!
//! Shortcuts are stored as `global_hotkey` strings ("alt+F10"), which round-trip
//! through `HotKey`'s `FromStr`/`Display`.
//!
//! They're registered with `global_hotkey` on macOS, Windows and X11. On
//! Wayland, where an app can't see keys pressed elsewhere, the desktop portal
//! does it (`portal_shortcuts`): the keys set here are what we ask it for.

use std::str::FromStr;

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

use crate::settings::{ShortcutAction, Shortcuts};

/// The registered shortcuts. The manager must stay alive for them to work.
pub struct Registered {
    /// `None` when the desktop portal has them instead.
    mgr: Option<GlobalHotKeyManager>,
    #[cfg(target_os = "linux")]
    portal: Option<crate::portal_shortcuts::Portal>,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    ctx: egui::Context,
    /// What's registered now, with the action each id triggers.
    active: Vec<(HotKey, ShortcutAction)>,
    /// What `active` was built from, to re-register only on change.
    from: Option<Shortcuts>,
    /// Shortcuts that couldn't be registered (taken by another app, invalid).
    pub errors: Vec<(ShortcutAction, String)>,
}

impl Registered {
    /// `ctx` is woken when a shortcut is pressed (where the backend can).
    pub fn new(ctx: egui::Context) -> Result<Self, String> {
        #[cfg(target_os = "linux")]
        if crate::portal_shortcuts::wanted() {
            if crate::portal_shortcuts::available() {
                return Ok(Self { mgr: None, portal: None, ctx, active: Vec::new(), from: None, errors: Vec::new() });
            }
            eprintln!("the desktop has no GlobalShortcuts portal: shortcuts only work while an X11 app is focused");
        }
        Ok(Self {
            mgr: Some(GlobalHotKeyManager::new().map_err(|e| e.to_string())?),
            #[cfg(target_os = "linux")]
            portal: None,
            ctx,
            active: Vec::new(),
            from: None,
            errors: Vec::new(),
        })
    }

    /// Make the OS shortcuts match `wanted`. Cheap when nothing changed.
    pub fn sync(&mut self, wanted: &Shortcuts) {
        if self.from.as_ref() == Some(wanted) {
            return;
        }
        if let Some(mgr) = &self.mgr {
            for (hk, _) in self.active.drain(..) {
                let _ = mgr.unregister(hk);
            }
        }
        self.errors.clear();
        let mut valid = Vec::new();
        for action in ShortcutAction::ALL {
            let text = wanted.get(action);
            if text.is_empty() {
                continue;
            }
            match HotKey::from_str(text) {
                Ok(hk) if self.active.iter().chain(&valid).any(|(h, _)| h.id() == hk.id()) => {
                    self.errors.push((action, "used twice".into()));
                }
                Ok(hk) => match &self.mgr {
                    Some(mgr) => match mgr.register(hk) {
                        Ok(()) => self.active.push((hk, action)),
                        Err(_) => self.errors.push((action, "taken by another app".into())),
                    },
                    None => valid.push((hk, action)),
                },
                Err(_) => self.errors.push((action, "not a valid shortcut".into())),
            }
        }
        #[cfg(target_os = "linux")]
        if self.mgr.is_none() {
            // The old set goes (dropping it unbinds them), the new one is bound.
            self.portal = None;
            let list = valid.iter().map(|(_, a)| (*a, wanted.get(*a).to_owned())).collect();
            self.portal = Some(crate::portal_shortcuts::Portal::bind(list, self.ctx.clone()));
        }
        let _ = valid;
        self.from = Some(wanted.clone());
    }

    /// Shortcuts pressed since the last call.
    pub fn pressed(&mut self) -> Vec<ShortcutAction> {
        let mut out = Vec::new();
        while let Ok(ev) = GlobalHotKeyEvent::receiver().try_recv() {
            if ev.state == HotKeyState::Pressed {
                out.extend(self.active.iter().find(|(h, _)| h.id() == ev.id).map(|(_, a)| *a));
            }
        }
        #[cfg(target_os = "linux")]
        if let Some(p) = &self.portal {
            out.extend(p.pressed());
        }
        out
    }

    pub fn error_for(&self, action: ShortcutAction) -> Option<String> {
        if let Some((_, e)) = self.errors.iter().find(|(a, _)| *a == action) {
            return Some(e.clone());
        }
        #[cfg(target_os = "linux")]
        if let Some(p) = &self.portal {
            use crate::portal_shortcuts::State;
            return match p.state() {
                State::Failed(e) => Some(format!("the desktop didn't take it ({e})")),
                State::Bound(list) if list.iter().any(|(a, keys)| *a == action && keys.trim().is_empty()) => {
                    Some("the desktop gave it no keys — set them in its keyboard settings".into())
                }
                _ => None,
            };
        }
        None
    }

    /// Whether the desktop owns the shortcuts (Wayland), so the keys set here
    /// are what's asked for and the desktop's settings have the last word.
    pub fn via_desktop(&self) -> bool {
        self.mgr.is_none()
    }

    /// The keys the desktop assigned to `action`, as it writes them, when
    /// the desktop owns the shortcuts.
    pub fn assigned(&self, action: ShortcutAction) -> Option<String> {
        #[cfg(target_os = "linux")]
        if let Some(p) = &self.portal {
            if let crate::portal_shortcuts::State::Bound(list) = p.state() {
                return list.into_iter().find(|(a, _)| *a == action).map(|(_, k)| k).filter(|k| !k.trim().is_empty());
            }
        }
        let _ = action;
        None
    }
}

/// The keys of a shortcut by name, modifiers first in the platform's order:
/// `["Option", "F10"]` on a Mac, `["Alt", "F10"]` elsewhere. `None` when off.
pub fn keys(text: &str) -> Option<Vec<String>> {
    let hk = HotKey::from_str(text).ok()?;
    let names: [(Modifiers, &str); 4] = if cfg!(target_os = "macos") {
        [(Modifiers::CONTROL, "Control"), (Modifiers::ALT, "Option"), (Modifiers::SHIFT, "Shift"), (Modifiers::SUPER, "Cmd")]
    } else {
        [(Modifiers::CONTROL, "Ctrl"), (Modifiers::ALT, "Alt"), (Modifiers::SHIFT, "Shift"), (Modifiers::SUPER, "Win")]
    };
    let mut out: Vec<String> = names.iter().filter(|(m, _)| hk.mods.contains(*m)).map(|(_, n)| (*n).to_owned()).collect();
    out.push(key_name(hk.key));
    Some(out)
}

/// A shortcut as plain text, "Option + F10", or "Off".
pub fn label(text: &str) -> String {
    keys(text).map_or_else(|| "Off".into(), |k| k.join(" + "))
}

/// The platform's command key plus `key`, for in-app shortcuts: "Cmd + A" on a
/// Mac, "Ctrl + A" elsewhere.
pub fn command(key: &str) -> Vec<String> {
    vec![if cfg!(target_os = "macos") { "Cmd" } else { "Ctrl" }.to_owned(), key.to_owned()]
}

/// Draw keys as keycaps: a small rounded box per key, side by side.
pub fn keycaps(ui: &mut egui::Ui, keys: &[String], size: f32) -> egui::Response {
    let v = ui.visuals().clone();
    let font = egui::FontId::proportional(size);
    let pad = egui::vec2((size * 0.45).round(), (size * 0.18).round());
    let gap = (size * 0.3).round();
    let galleys: Vec<_> = keys.iter().map(|k| ui.painter().layout_no_wrap(k.clone(), font.clone(), v.strong_text_color())).collect();
    let h = size + pad.y * 2.0 + 2.0;
    let w: f32 = galleys.iter().map(|g| g.size().x.max(size * 0.7) + pad.x * 2.0).sum::<f32>() + gap * galleys.len().saturating_sub(1) as f32;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::hover());
    let mut x = rect.left();
    for g in galleys {
        let cap = egui::Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(g.size().x.max(size * 0.7) + pad.x * 2.0, h - 2.0));
        // A slightly darker lip under the cap, like a real key.
        ui.painter().rect_filled(cap.translate(egui::vec2(0.0, 2.0)), 5, v.widgets.noninteractive.bg_stroke.color);
        ui.painter().rect_filled(cap, 5, v.widgets.inactive.weak_bg_fill);
        ui.painter().rect_stroke(cap, 5, egui::Stroke::new(1.0, v.widgets.noninteractive.bg_stroke.color), egui::StrokeKind::Inside);
        ui.painter().galley(cap.center() - g.size() / 2.0, g, v.strong_text_color());
        x = cap.right() + gap;
    }
    resp
}

fn key_name(code: Code) -> String {
    let raw = code.to_string();
    let name = raw.strip_prefix("Key").or_else(|| raw.strip_prefix("Digit")).unwrap_or(&raw);
    match name {
        "Space" => "Space".into(),
        "ArrowUp" => "Up".into(),
        "ArrowDown" => "Down".into(),
        "ArrowLeft" => "Left".into(),
        "ArrowRight" => "Right".into(),
        "PageUp" => "Page Up".into(),
        "PageDown" => "Page Down".into(),
        "Backquote" => "`".into(),
        "Minus" => "-".into(),
        "Equal" => "=".into(),
        "BracketLeft" => "[".into(),
        "BracketRight" => "]".into(),
        "Backslash" => "\\".into(),
        "Semicolon" => ";".into(),
        "Quote" => "'".into(),
        "Comma" => ",".into(),
        "Period" => ".".into(),
        "Slash" => "/".into(),
        other => other.into(),
    }
}

/// Turn a key pressed in the recorder into a shortcut string, or explain why it
/// can't be one. Function keys work alone (games rarely use them); anything else
/// needs a modifier so typing never triggers it.
pub fn from_press(key: egui::Key, m: egui::Modifiers) -> Result<String, &'static str> {
    let code = egui_to_code(key).ok_or("That key can't be used for a shortcut.")?;
    let is_fn = key.name().starts_with('F') && key.name().len() > 1 && key.name()[1..].chars().all(|c| c.is_ascii_digit());
    let mut mods = Modifiers::empty();
    if m.ctrl {
        mods |= Modifiers::CONTROL;
    }
    if m.alt {
        mods |= Modifiers::ALT;
    }
    if m.shift {
        mods |= Modifiers::SHIFT;
    }
    if m.mac_cmd || (!cfg!(target_os = "macos") && m.command && !m.ctrl) {
        mods |= Modifiers::SUPER;
    }
    let only_shift = mods == Modifiers::SHIFT;
    if !is_fn && (mods.is_empty() || only_shift) {
        return Err(if cfg!(target_os = "macos") {
            "Add Option, Control or Cmd so typing doesn't trigger it (function keys work alone)."
        } else {
            "Add Alt, Ctrl or Win so typing doesn't trigger it (function keys work alone)."
        });
    }
    Ok(HotKey::new(Some(mods), code).into_string())
}

fn egui_to_code(key: egui::Key) -> Option<Code> {
    use egui::Key as K;
    Some(match key {
        K::A => Code::KeyA,
        K::B => Code::KeyB,
        K::C => Code::KeyC,
        K::D => Code::KeyD,
        K::E => Code::KeyE,
        K::F => Code::KeyF,
        K::G => Code::KeyG,
        K::H => Code::KeyH,
        K::I => Code::KeyI,
        K::J => Code::KeyJ,
        K::K => Code::KeyK,
        K::L => Code::KeyL,
        K::M => Code::KeyM,
        K::N => Code::KeyN,
        K::O => Code::KeyO,
        K::P => Code::KeyP,
        K::Q => Code::KeyQ,
        K::R => Code::KeyR,
        K::S => Code::KeyS,
        K::T => Code::KeyT,
        K::U => Code::KeyU,
        K::V => Code::KeyV,
        K::W => Code::KeyW,
        K::X => Code::KeyX,
        K::Y => Code::KeyY,
        K::Z => Code::KeyZ,
        K::Num0 => Code::Digit0,
        K::Num1 => Code::Digit1,
        K::Num2 => Code::Digit2,
        K::Num3 => Code::Digit3,
        K::Num4 => Code::Digit4,
        K::Num5 => Code::Digit5,
        K::Num6 => Code::Digit6,
        K::Num7 => Code::Digit7,
        K::Num8 => Code::Digit8,
        K::Num9 => Code::Digit9,
        K::F1 => Code::F1,
        K::F2 => Code::F2,
        K::F3 => Code::F3,
        K::F4 => Code::F4,
        K::F5 => Code::F5,
        K::F6 => Code::F6,
        K::F7 => Code::F7,
        K::F8 => Code::F8,
        K::F9 => Code::F9,
        K::F10 => Code::F10,
        K::F11 => Code::F11,
        K::F12 => Code::F12,
        K::F13 => Code::F13,
        K::F14 => Code::F14,
        K::F15 => Code::F15,
        K::F16 => Code::F16,
        K::F17 => Code::F17,
        K::F18 => Code::F18,
        K::F19 => Code::F19,
        K::F20 => Code::F20,
        K::Space => Code::Space,
        K::Enter => Code::Enter,
        K::Tab => Code::Tab,
        K::Backspace => Code::Backspace,
        K::Delete => Code::Delete,
        K::Insert => Code::Insert,
        K::Home => Code::Home,
        K::End => Code::End,
        K::PageUp => Code::PageUp,
        K::PageDown => Code::PageDown,
        K::ArrowUp => Code::ArrowUp,
        K::ArrowDown => Code::ArrowDown,
        K::ArrowLeft => Code::ArrowLeft,
        K::ArrowRight => Code::ArrowRight,
        K::Minus => Code::Minus,
        K::Equals => Code::Equal,
        K::Comma => Code::Comma,
        K::Period => Code::Period,
        K::Slash => Code::Slash,
        K::Backslash => Code::Backslash,
        K::Semicolon => Code::Semicolon,
        K::Quote => Code::Quote,
        K::Backtick => Code::Backquote,
        K::OpenBracket => Code::BracketLeft,
        K::CloseBracket => Code::BracketRight,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse_and_display() {
        for a in ShortcutAction::ALL {
            let text = Shortcuts::default().get(a).to_owned();
            assert!(HotKey::from_str(&text).is_ok(), "{text}");
            assert!(keys(&text).is_some());
        }
        let alt = if cfg!(target_os = "macos") { "Option" } else { "Alt" };
        assert_eq!(keys("alt+F10").unwrap(), vec![alt.to_owned(), "F10".to_owned()]);
        assert_eq!(label("alt+F10"), format!("{alt} + F10"));
        assert_eq!(label("shift+alt+KeyS"), format!("{alt} + Shift + S"));
        assert_eq!(label(""), "Off");
    }

    #[test]
    fn presses_become_shortcuts() {
        let alt = egui::Modifiers { alt: true, ..Default::default() };
        let text = from_press(egui::Key::S, alt).unwrap();
        assert_eq!(HotKey::from_str(&text).unwrap(), HotKey::new(Some(Modifiers::ALT), Code::KeyS));
        // Function keys alone are fine; letters alone aren't.
        assert!(from_press(egui::Key::F7, egui::Modifiers::NONE).is_ok());
        assert!(from_press(egui::Key::S, egui::Modifiers::NONE).is_err());
        assert!(from_press(egui::Key::S, egui::Modifiers::SHIFT).is_err());
        // Round-trips through storage.
        let cmd_shift = egui::Modifiers { mac_cmd: true, command: true, shift: true, ..Default::default() };
        let text = from_press(egui::Key::Num4, cmd_shift).unwrap();
        assert_eq!(HotKey::from_str(&text).unwrap().into_string(), text);
    }
}
