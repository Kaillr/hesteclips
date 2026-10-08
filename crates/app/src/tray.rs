//! The tray icon (Windows) / menu bar icon (macOS): HesteClips keeps running
//! there when its window is closed, so the replay buffer and shortcuts keep
//! working. Its menu: open the window, save a clip, start or stop the replay
//! buffer, quit. Clicking the icon opens the window.
//!
//! Linux has none yet (the tray library needs GTK there): closing quits.

use std::sync::mpsc::{self, Receiver};

/// What the tray asks for. Handled in the app's `logic`, which runs while the
/// window is hidden too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    Open,
    SaveClip,
    ToggleBuffer,
    Quit,
}

/// Whether this platform has a tray here.
pub const AVAILABLE: bool = cfg!(any(windows, target_os = "macos"));

pub struct Tray {
    rx: Receiver<Cmd>,
    #[cfg(any(windows, target_os = "macos"))]
    inner: Inner,
}

#[cfg(any(windows, target_os = "macos"))]
struct Inner {
    _icon: tray_icon::TrayIcon,
    save: tray_icon::menu::MenuItem,
    buffer: tray_icon::menu::MenuItem,
    /// What the buffer item says now, so it's only changed when it changes.
    buffering: std::cell::Cell<Option<bool>>,
}

impl Tray {
    /// The tray icon, or `None` where there's none. `tx`: commands from
    /// elsewhere too (another launch asking to be shown).
    pub fn new(ctx: &egui::Context, icon: &egui::IconData, tx: mpsc::Sender<Cmd>, rx: Receiver<Cmd>) -> Option<Self> {
        #[cfg(any(windows, target_os = "macos"))]
        {
            use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
            use tray_icon::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

            let open = MenuItem::new("Open HesteClips", true, None);
            let save = MenuItem::new("Save clip", false, None);
            let buffer = MenuItem::new("Start replay buffer", true, None);
            let quit = MenuItem::new("Quit HesteClips", true, None);
            let menu = Menu::new();
            menu.append_items(&[&open, &PredefinedMenuItem::separator(), &save, &buffer, &PredefinedMenuItem::separator(), &quit]).ok()?;
            let image = tray_icon::Icon::from_rgba(icon.rgba.clone(), icon.width, icon.height).ok()?;
            let built = TrayIconBuilder::new().with_tooltip("HesteClips").with_icon(image).with_menu(Box::new(menu)).with_menu_on_left_click(false).build();
            let tray = match built {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("no tray icon: {e}");
                    return None;
                }
            };
            let ids = [(open.id().clone(), Cmd::Open), (save.id().clone(), Cmd::SaveClip), (buffer.id().clone(), Cmd::ToggleBuffer), (quit.id().clone(), Cmd::Quit)];
            let (menu_tx, menu_ctx) = (tx.clone(), ctx.clone());
            MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
                if let Some((_, cmd)) = ids.iter().find(|(id, _)| *id == e.id) {
                    let _ = menu_tx.send(*cmd);
                    menu_ctx.request_repaint();
                }
            }));
            let click_ctx = ctx.clone();
            TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
                if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                    let _ = tx.send(Cmd::Open);
                    click_ctx.request_repaint();
                }
            }));
            Some(Self { rx, inner: Inner { _icon: tray, save, buffer, buffering: Default::default() } })
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            let _ = (ctx, icon, tx, rx);
            None
        }
    }

    /// What was asked for since the last call.
    pub fn take(&self) -> Vec<Cmd> {
        self.rx.try_iter().collect()
    }

    /// Keep the menu in step with capture.
    pub fn update(&self, buffering: bool) {
        #[cfg(any(windows, target_os = "macos"))]
        {
            let i = &self.inner;
            if i.buffering.get() != Some(buffering) {
                i.buffering.set(Some(buffering));
                i.save.set_enabled(buffering);
                i.buffer.set_text(if buffering { "Stop replay buffer" } else { "Start replay buffer" });
            }
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        let _ = buffering;
    }
}

/// One HesteClips at a time: opening it again (a shortcut, a second click)
/// shows the one already running, in the tray maybe. The first listens on a
/// local port; a later one tells it to show itself and exits.
///
/// Called first thing: `Some(listener)` to run (hand it to [`serve`]),
/// `None` when another one was asked to show itself and this one should exit.
pub fn claim() -> Option<Option<std::net::TcpListener>> {
    use std::io::Write;
    // Fixed, so a second launch knows where to knock (nothing special).
    const PORT: u16 = 52131;
    match std::net::TcpListener::bind(("127.0.0.1", PORT)) {
        Ok(listener) => Some(Some(listener)),
        // Taken: one is running. Ask it to show itself.
        Err(_) => match std::net::TcpStream::connect(("127.0.0.1", PORT)) {
            Ok(mut s) => {
                let _ = s.write_all(b"show");
                None
            }
            // Something else has the port: run anyway, alone.
            Err(_) => Some(None),
        },
    }
}

/// Answer later launches: each asks this one to show itself.
pub fn serve(listener: std::net::TcpListener, ctx: &egui::Context, tx: mpsc::Sender<Cmd>) {
    use std::io::Read;
    let ctx = ctx.clone();
    let _ = std::thread::Builder::new().name("single instance".into()).spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut word = [0u8; 4];
            if stream.read_exact(&mut word).is_ok() && &word == b"show" {
                let _ = tx.send(Cmd::Open);
                ctx.request_repaint();
            }
        }
    });
}
