//! The tray icon (Windows, Linux) / menu bar icon (macOS): HasteClip keeps
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

/// The tray / menu bar icon: the logo's glyph alone, as the bar's other icons
/// are (the app icon, with its dark plate, looked small and boxed in there).
/// Made from its SVG by `scripts/icons.sh`.
#[cfg(any(windows, target_os = "macos"))]
fn tray_image() -> Option<tray_icon::Icon> {
    // macOS: 18 points high on a 2x display; macOS sizes it to the bar.
    #[cfg(target_os = "macos")]
    let png: &[u8] = include_bytes!("../assets/tray-mac.png");
    // Windows: drawn at exactly the size the tray shows it at, which depends
    // on the display's scale (16 px at 100%, 24 at 150%, ...). Windows scaled
    // a bigger one down coarsely: it looked pixelated.
    #[cfg(windows)]
    let png: &[u8] = {
        use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSMICON};
        // SAFETY: no arguments to get wrong.
        let want = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16);
        const SIZES: [(i32, &[u8]); 8] = [
            (16, include_bytes!("../assets/tray/glyph-16.png")),
            (20, include_bytes!("../assets/tray/glyph-20.png")),
            (24, include_bytes!("../assets/tray/glyph-24.png")),
            (28, include_bytes!("../assets/tray/glyph-28.png")),
            (32, include_bytes!("../assets/tray/glyph-32.png")),
            (40, include_bytes!("../assets/tray/glyph-40.png")),
            (48, include_bytes!("../assets/tray/glyph-48.png")),
            (64, include_bytes!("../assets/tray/glyph-64.png")),
        ];
        // The size asked for, else the next bigger one (scaled down a little,
        // which stays smooth), else the biggest.
        SIZES.iter().find(|(s, _)| *s >= want).unwrap_or(&SIZES[SIZES.len() - 1]).1
    };
    let img = image::load_from_memory_with_format(png, image::ImageFormat::Png).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    tray_icon::Icon::from_rgba(img.into_raw(), w, h).ok()
}

impl Tray {
    /// The tray icon, or `None` where there's none. `tx`: commands from
    /// elsewhere too (another launch asking to be shown).
    /// `quick`: saves a clip from the tray's own thread (Linux: the UI's
    /// doesn't run while the window's hidden).
    pub fn new(ctx: &egui::Context, tx: mpsc::Sender<Cmd>, rx: Receiver<Cmd>, quick: crate::service::QuickSave) -> Option<Self> {
        #[cfg(any(windows, target_os = "macos"))]
        {
            // Their UI runs while the window's hidden: the tray's commands wait for it.
            let _ = quick;
            use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
            use tray_icon::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

            let open = MenuItem::new("Open HasteClip", true, None);
            let save = MenuItem::new("Save clip", false, None);
            let buffer = MenuItem::new("Start clipping", true, None);
            let quit = MenuItem::new("Quit HasteClip", true, None);
            let menu = Menu::new();
            menu.append_items(&[&open, &PredefinedMenuItem::separator(), &save, &buffer, &PredefinedMenuItem::separator(), &quit]).ok()?;
            let image = tray_image()?;
            // macOS: a template image, tinted by macOS to suit the menu bar
            // (white on a dark one, black on a light one), as its own icons.
            let built = TrayIconBuilder::new()
                .with_tooltip("HasteClip")
                .with_icon(image)
                .with_icon_as_template(cfg!(target_os = "macos"))
                .with_menu(Box::new(menu))
                .with_menu_on_left_click(false)
                .build();
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
            let inner = linux::spawn(ctx, tx, quick)?;
            Some(Self { rx, inner, buffering: Default::default() })
        }
        #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
        {
            let _ = (ctx, tx, rx, quick);
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
                i.buffer.set_text(if buffering { "Stop clipping" } else { "Start clipping" });
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
        quick: crate::service::QuickSave,
        pub buffering: bool,
    }

    impl Item {
        /// Do `cmd`. While the window's hidden (KDE under Wayland), the app's
        /// UI doesn't run: opening it, saving a clip and quitting are done
        /// from here; starting or stopping clipping brings the window back to
        /// do it.
        fn send(&self, cmd: Cmd) {
            if crate::kwin_window::hidden() {
                match cmd {
                    Cmd::Open | Cmd::ToggleBuffer => {
                        crate::kwin_window::bring_back();
                    }
                    Cmd::SaveClip => {
                        self.quick.save();
                        return;
                    }
                    Cmd::Quit => crate::quit_now(),
                }
            }
            let _ = self.tx.send(cmd);
            self.ctx.request_repaint();
        }
    }

    impl ksni::Tray for Item {
        fn id(&self) -> String {
            crate::portal_shortcuts::APP_ID.into()
        }
        fn title(&self) -> String {
            "HasteClip".into()
        }
        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            self.icon.clone()
        }
        fn tool_tip(&self) -> ksni::ToolTip {
            ksni::ToolTip { title: "HasteClip".into(), ..Default::default() }
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
                item("Open HasteClip", true, Cmd::Open),
                ksni::MenuItem::Separator,
                item("Save clip", self.buffering, Cmd::SaveClip),
                item(if self.buffering { "Stop clipping" } else { "Start clipping" }, true, Cmd::ToggleBuffer),
                ksni::MenuItem::Separator,
                item("Quit HasteClip", true, Cmd::Quit),
            ]
        }
        /// The desktop stopped showing tray icons (its panel restarted, say):
        /// keep the item, it comes back when the panel does.
        fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
            true
        }
    }

    /// Put the item in the tray. `None` where nothing shows tray icons.
    pub fn spawn(ctx: &egui::Context, tx: mpsc::Sender<Cmd>, quick: crate::service::QuickSave) -> Option<ksni::blocking::Handle<Item>> {
        if !host_present() {
            eprintln!("no tray icon: this desktop doesn't show tray icons (on GNOME, the AppIndicator extension adds them); closing the window quits");
            return None;
        }
        // The glyph alone, as the tray's other icons are (and as on Windows),
        // drawn at the sizes trays show (16–64 px), for the tray to pick
        // from; ARGB32 in network byte order as StatusNotifierItem wants it.
        const SIZES: [&[u8]; 8] = [
            include_bytes!("../assets/tray/glyph-16.png"),
            include_bytes!("../assets/tray/glyph-20.png"),
            include_bytes!("../assets/tray/glyph-24.png"),
            include_bytes!("../assets/tray/glyph-28.png"),
            include_bytes!("../assets/tray/glyph-32.png"),
            include_bytes!("../assets/tray/glyph-40.png"),
            include_bytes!("../assets/tray/glyph-48.png"),
            include_bytes!("../assets/tray/glyph-64.png"),
        ];
        let sizes: Vec<ksni::Icon> = SIZES
            .iter()
            .filter_map(|png| image::load_from_memory_with_format(png, image::ImageFormat::Png).ok())
            .map(|img| {
                let img = img.to_rgba8();
                let data = img.pixels().flat_map(|p| [p[3], p[0], p[1], p[2]]).collect();
                ksni::Icon { width: img.width() as i32, height: img.height() as i32, data }
            })
            .collect();
        let item = Item { icon: sizes, tx, ctx: ctx.clone(), quick, buffering: false };
        match item.spawn() {
            Ok(handle) => Some(handle),
            Err(e) => {
                eprintln!("no tray icon: {e}");
                None
            }
        }
    }
}

/// One HasteClip at a time: opening it again (a shortcut, a second click)
/// shows the one already running, in the tray maybe. The first listens on a
/// local port; a later one tells it to show itself and exits. A build of
/// your own takes over instead (see [`take_over`]).
///
/// Called first thing: `Some(listener)` to run (hand it to [`serve`]),
/// `None` when another one runs and this one should exit.
pub fn claim() -> Option<Option<std::net::TcpListener>> {
    claim_on(PORT)
}

/// Where the first one listens. Fixed, so a later one knows where to knock
/// (nothing special).
const PORT: u16 = 52131;

fn claim_on(port: u16) -> Option<Option<std::net::TcpListener>> {
    use std::io::Write;
    match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => Some(Some(listener)),
        // Taken: one is running.
        Err(_) => match std::net::TcpStream::connect(("127.0.0.1", port)) {
            Ok(running) if from_build_folder() => take_over(running, port),
            Ok(mut running) => {
                let _ = running.write_all(b"show");
                eprintln!("HasteClip is already running: showed that one instead");
                None
            }
            // Something else has the port: run anyway, alone.
            Err(_) => Some(None),
        },
    }
}

/// Run from Cargo's build folder (`cargo run`, with `--release` or not): a
/// build of your own, not an installed HasteClip.
fn from_build_folder() -> bool {
    std::env::current_exe().is_ok_and(|exe| exe.ancestors().skip(1).take(3).any(|dir| dir.join("CACHEDIR.TAG").is_file()))
}

/// A build of your own, started while another HasteClip runs (the installed
/// one, in the tray): that one is asked to quit (finishing a recording
/// first, as when it's quit from the tray), and this one runs once it has.
/// It used to be shown instead, as for a second launch of the same app,
/// which looked like the build starting while the installed one went on
/// recording. One that doesn't answer (0.4.0 and before don't know how) is
/// left running, and this one says why it can't.
fn take_over(mut running: std::net::TcpStream, port: u16) -> Option<Option<std::net::TcpListener>> {
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};
    let _ = running.write_all(b"quit");
    let _ = running.set_read_timeout(Some(Duration::from_secs(2)));
    let mut answer = [0u8; 4];
    if running.read_exact(&mut answer).is_ok() && &answer == b"okay" {
        // Stopping a capture may take 10 s, and the app a little more.
        let until = Instant::now() + Duration::from_secs(20);
        while Instant::now() < until {
            std::thread::sleep(Duration::from_millis(100));
            if let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", port)) {
                crate::logfile::tell("HasteClip was already running: it quit, so this one runs");
                return Some(Some(listener));
            }
        }
    }
    crate::logfile::tell("HasteClip is already running, and this build of your own can't start beside it. Quit HasteClip from the tray, then run this again.");
    None
}

/// Answer later launches: each asks this one to show itself, or (a build of
/// your own, see [`take_over`]) to quit.
pub fn serve(listener: std::net::TcpListener, ctx: &egui::Context, tx: mpsc::Sender<Cmd>) {
    use std::io::{Read, Write};
    let ctx = ctx.clone();
    let _ = std::thread::Builder::new().name("single instance".into()).spawn(move || {
        for mut stream in listener.incoming().flatten() {
            // One that connects and says nothing doesn't hold up the next.
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
            let mut word = [0u8; 4];
            if stream.read_exact(&mut word).is_err() {
                continue;
            }
            match &word {
                b"show" => {
                    // Hidden (KDE under Wayland): the UI doesn't run to hear it.
                    #[cfg(target_os = "linux")]
                    crate::kwin_window::bring_back();
                    let _ = tx.send(Cmd::Open);
                }
                b"quit" => {
                    let _ = stream.write_all(b"okay");
                    // Hidden (KDE under Wayland): quit from here.
                    #[cfg(target_os = "linux")]
                    {
                        if crate::kwin_window::hidden() {
                            crate::quit_now();
                        }
                    }
                    let _ = tx.send(Cmd::Quit);
                }
                _ => continue,
            }
            ctx.request_repaint();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::time::{Duration, Instant};

    /// A port of the test's own, for a HasteClip "already running" on it.
    fn spare_port() -> u16 {
        TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port()
    }

    /// One already running answers "show" by showing itself, and "quit" by
    /// saying it will and quitting.
    #[test]
    fn answers_show_and_quit() {
        let port = spare_port();
        let (tx, rx) = mpsc::channel();
        serve(TcpListener::bind(("127.0.0.1", port)).unwrap(), &egui::Context::default(), tx);
        let mut show = TcpStream::connect(("127.0.0.1", port)).unwrap();
        show.write_all(b"show").unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(Cmd::Open));
        let mut quit = TcpStream::connect(("127.0.0.1", port)).unwrap();
        quit.write_all(b"quit").unwrap();
        let mut answer = [0u8; 4];
        quit.read_exact(&mut answer).unwrap();
        assert_eq!(&answer, b"okay");
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(Cmd::Quit));
    }

    /// A build of your own (tests run from the build folder) takes over from
    /// one that answers: it's asked to quit, and gets the port once that one
    /// lets go of it (here half a second later).
    #[test]
    fn a_build_of_your_own_takes_over() {
        assert!(from_build_folder());
        let port = spare_port();
        let running = TcpListener::bind(("127.0.0.1", port)).unwrap();
        let quitting = std::thread::spawn(move || {
            let (mut stream, _) = running.accept().unwrap();
            let mut word = [0u8; 4];
            stream.read_exact(&mut word).unwrap();
            stream.write_all(b"okay").unwrap();
            std::thread::sleep(Duration::from_millis(500));
            word
        });
        let claimed = claim_on(port);
        assert_eq!(&quitting.join().unwrap(), b"quit");
        assert!(claimed.is_some_and(|l| l.is_some()), "didn't run once the other one quit");
    }

    /// One that doesn't know how to hand over (0.4.0 reads the word, and only
    /// knows "show") is left running, and the build of your own exits, at once.
    #[test]
    fn an_older_one_is_left_running() {
        let port = spare_port();
        let running = TcpListener::bind(("127.0.0.1", port)).unwrap();
        std::thread::spawn(move || {
            for mut stream in running.incoming().flatten() {
                let mut word = [0u8; 4];
                let _ = stream.read_exact(&mut word);
            }
        });
        let t = Instant::now();
        assert!(claim_on(port).is_none());
        assert!(t.elapsed() < Duration::from_secs(1), "took {:?}", t.elapsed());
    }
}
