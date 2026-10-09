//! The tray icon (Windows, Linux) / menu bar icon (macOS): HesteClips keeps
//! running there when its window is closed, so the replay buffer and
//! shortcuts keep working. Its menu: open the window, save a clip, start or
//! stop the replay buffer, quit. Clicking the icon opens the window.
//!
//! On Linux it's a StatusNotifierItem on the session bus, which KDE, XFCE,
//! Cinnamon, Hyprland's and Sway's bars and most others show (GNOME with its
//! AppIndicator extension, which Ubuntu has). Where nothing shows it, there's
//! no tray, and closing the window quits as before.

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

/// Whether there's a tray here: always on Windows and macOS; on Linux when
/// the desktop shows tray icons (asked once).
pub fn available() -> bool {
    #[cfg(target_os = "linux")]
    {
        linux::host_present()
    }
    #[cfg(not(target_os = "linux"))]
    {
        cfg!(any(windows, target_os = "macos"))
    }
}

pub struct Tray {
    rx: Receiver<Cmd>,
    #[cfg(any(windows, target_os = "macos"))]
    inner: Inner,
    #[cfg(target_os = "linux")]
    inner: ksni::blocking::Handle<linux::Item>,
    /// What the menu shows now, so it's only changed when it changes.
    #[cfg(target_os = "linux")]
    buffering: std::cell::Cell<bool>,
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
        #[cfg(target_os = "linux")]
        {
            let inner = linux::spawn(ctx, icon, tx)?;
            Some(Self { rx, inner, buffering: Default::default() })
        }
        #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
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
        // Only when it changed: each update tells the desktop to redraw the menu.
        #[cfg(target_os = "linux")]
        if self.buffering.replace(buffering) != buffering {
            self.inner.update(|item| item.buffering = buffering);
        }
        #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
        let _ = buffering;
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::sync::mpsc;

    use ksni::blocking::TrayMethods;

    use super::Cmd;

    /// Whether something on the desktop shows tray icons: the
    /// StatusNotifierWatcher is on the session bus. Asked once.
    pub fn host_present() -> bool {
        static PRESENT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *PRESENT.get_or_init(|| {
            pollster::block_on(async {
                let bus = ashpd::zbus::Connection::session().await.ok()?;
                let dbus = ashpd::zbus::fdo::DBusProxy::new(&bus).await.ok()?;
                let name = ashpd::zbus::names::BusName::try_from("org.kde.StatusNotifierWatcher").ok()?;
                dbus.name_has_owner(name).await.ok()
            })
            .unwrap_or(false)
        })
    }

    /// The tray item: what it shows, and where its clicks go.
    pub struct Item {
        icon: Vec<ksni::Icon>,
        tx: mpsc::Sender<Cmd>,
        ctx: egui::Context,
        pub buffering: bool,
    }

    impl Item {
        fn send(&self, cmd: Cmd) {
            let _ = self.tx.send(cmd);
            self.ctx.request_repaint();
        }
    }

    impl ksni::Tray for Item {
        fn id(&self) -> String {
            crate::portal_shortcuts::APP_ID.into()
        }
        fn title(&self) -> String {
            "HesteClips".into()
        }
        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            self.icon.clone()
        }
        fn tool_tip(&self) -> ksni::ToolTip {
            ksni::ToolTip { title: "HesteClips".into(), ..Default::default() }
        }
        /// A left click opens the window.
        fn activate(&mut self, _x: i32, _y: i32) {
            self.send(Cmd::Open);
        }
        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            use ksni::menu::StandardItem;
            let item = |label: &str, enabled: bool, cmd: Cmd| {
                StandardItem { label: label.into(), enabled, activate: Box::new(move |this: &mut Self| this.send(cmd)), ..Default::default() }.into()
            };
            vec![
                item("Open HesteClips", true, Cmd::Open),
                ksni::MenuItem::Separator,
                item("Save clip", self.buffering, Cmd::SaveClip),
                item(if self.buffering { "Stop replay buffer" } else { "Start replay buffer" }, true, Cmd::ToggleBuffer),
                ksni::MenuItem::Separator,
                item("Quit HesteClips", true, Cmd::Quit),
            ]
        }
        /// The desktop stopped showing tray icons (its panel restarted, say):
        /// keep the item, it comes back when the panel does.
        fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
            true
        }
    }

    /// Put the item in the tray. `None` where nothing shows tray icons.
    pub fn spawn(ctx: &egui::Context, icon: &egui::IconData, tx: mpsc::Sender<Cmd>) -> Option<ksni::blocking::Handle<Item>> {
        if !host_present() {
            eprintln!("no tray icon: this desktop doesn't show tray icons (on GNOME, the AppIndicator extension adds them); closing the window quits");
            return None;
        }
        // Trays show 16–48 px: a few sizes (sent again with every change, so
        // not the 1024 px original), ARGB32 in network byte order as
        // StatusNotifierItem wants it.
        let full = image::RgbaImage::from_raw(icon.width, icon.height, icon.rgba.clone())?;
        let sizes = [22, 32, 48, 64].map(|size| {
            let small = image::imageops::resize(&full, size, size, image::imageops::FilterType::Lanczos3);
            let data = small.pixels().flat_map(|p| [p[3], p[0], p[1], p[2]]).collect();
            ksni::Icon { width: size as i32, height: size as i32, data }
        });
        let item = Item { icon: sizes.to_vec(), tx, ctx: ctx.clone(), buffering: false };
        match item.spawn() {
            Ok(handle) => Some(handle),
            Err(e) => {
                eprintln!("no tray icon: {e}");
                None
            }
        }
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
