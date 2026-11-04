//! macOS: what HasteClip needs macOS to allow, shown once at launch so
//! nobody has to find it in Settings after something didn't work.
//!
//! - **Screen Recording**: without it nothing can be clipped.
//! - **Input Monitoring**: without it, shortcuts don't reach HasteClip while
//!   a game in true fullscreen holds the screen (see `key_tap`).
//!
//! Both are granted in System Settings, which macOS opens from here; neither
//! takes effect until it is, so each row shows what's allowed now and is
//! checked again as the window comes back.

use crate::theme::{self, W, icon};

/// The permissions screen shows at launch until it's been closed in this
/// round. Raised when a release needs everyone to allow them again.
/// 1: the first, with the switch from ad hoc signing (macOS forgot what
/// each version was allowed) to HasteClip's own certificate (it doesn't).
pub(crate) const SETUP_ROUND: u32 = 1;
use crate::ui_kit::{self, Btn, Dismissed as _};
use crate::App;

impl App {
    /// Shown once, until it's closed.
    pub(crate) fn setup_dialog(&mut self, ctx: &egui::Context) {
        if !self.setup_open {
            return;
        }
        let screen = self.permission != capture::Permission::Denied;
        // Asked as it's drawn: cheap, and what's allowed changes while
        // System Settings is open.
        let keys = crate::key_tap::allowed();
        if keys {
            self.start_key_tap();
        }
        let (mut ask_screen, mut ask_keys, mut done) = (false, false, false);
        let modal = ui_kit::dialog(ctx, "setup", 460.0, |ui| {
            let p = theme::pal(ui);
            ui_kit::dialog_title(ui, "Before you start");
            ui_kit::dialog_text(ui, "HasteClip needs macOS to allow two things. You can change them later in System Settings, under Privacy & Security.");
            ui.add_space(10.0);
            ask_screen = permission_row(ui, icon::MONITOR, "Screen Recording", "Needed to clip anything.", screen);
            ui.add_space(10.0);
            ask_keys = permission_row(
                ui,
                icon::KEYBOARD,
                "Input Monitoring",
                "Some games take over the whole screen when they run in fullscreen, and macOS doesn't let shortcuts through to other apps while they do. Allow this so your shortcuts keep working in those games.",
                keys,
            );
            if (ask_screen || ask_keys) || ui.ctx().input(|i| i.viewport().focused) == Some(false) {
                // Back from System Settings: what's allowed is asked again.
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
            }
            if !screen || !keys {
                ui.add_space(10.0);
                ui.add(egui::Label::new(theme::text("macOS may ask you to quit and reopen HasteClip after you allow something.", 12.5, W::Regular).color(p.text3)).wrap());
            }
            ui_kit::dialog_buttons(ui, |ui| {
                // Without Screen Recording there's nothing to start: the
                // library's banner keeps asking.
                let label = match (screen, keys) {
                    (true, true) => "Start clipping",
                    (true, false) => "Start without it",
                    (false, _) => "Not now",
                };
                done = ui.add(ui_kit::button(label, screen && keys)).clicked();
            });
        });
        if ask_screen {
            capture::request_screen_permission();
            crate::open_screen_settings();
        }
        if ask_keys && crate::key_tap::request() {
            self.start_key_tap();
        }
        if done || modal.dismissed() {
            self.setup_open = false;
            self.settings.setup_shown = SETUP_ROUND;
            // Clipping starts as it would have at launch, now it can.
            if self.settings.auto_start_buffer && !self.rec_state.clipping && capture::screen_permission() != capture::Permission::Denied {
                self.start_replay_buffer();
            }
        }
    }

    /// Start the key tap, if allowed and the shortcuts are up.
    pub(crate) fn start_key_tap(&mut self) -> bool {
        self.hotkeys.as_mut().is_ok_and(|h| h.start_key_tap())
    }
}

/// What HasteClip may need macOS to allow, for Settings' Permissions.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Need {
    Screen,
    Keys,
    Microphone,
    Camera,
}

impl Need {
    pub(crate) const ALL: [Need; 4] = [Need::Screen, Need::Keys, Need::Microphone, Need::Camera];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Need::Screen => "Screen Recording",
            Need::Keys => "Input Monitoring",
            Need::Microphone => "Microphone",
            Need::Camera => "Camera",
        }
    }

    /// What it's for, as the row's note.
    pub(crate) fn why(self) -> &'static str {
        match self {
            Need::Screen => "Needed to clip anything.",
            Need::Keys => "Keeps your shortcuts working in games that take over the whole screen in fullscreen.",
            Need::Microphone => "For a microphone in your clips, and saving by voice.",
            Need::Camera => "For your webcam in your clips.",
        }
    }

    /// The words Settings' search finds it by.
    pub(crate) fn words(self) -> &'static [&'static str] {
        match self {
            Need::Screen => &["screen recording", "capture"],
            Need::Keys => &["input monitoring", "shortcuts", "fullscreen games", "keyboard"],
            Need::Microphone => &["microphone", "mic", "voice"],
            Need::Camera => &["camera", "webcam"],
        }
    }

    /// Allowed now. Cheap enough to ask as Settings is drawn.
    pub(crate) fn allowed(self) -> bool {
        match self {
            Need::Screen => capture::screen_permission() != capture::Permission::Denied,
            Need::Keys => crate::key_tap::allowed(),
            Need::Microphone => capture::macos::media_access(false) == capture::macos::Access::Allowed,
            Need::Camera => capture::macos::media_access(true) == capture::macos::Access::Allowed,
        }
    }

    /// Ask for it: macOS's prompt if it's never been asked, else System
    /// Settings at its list, where it's turned on.
    pub(crate) fn ask(self) {
        use capture::macos::{Access, media_access, request_media_access};
        match self {
            Need::Screen => {
                capture::request_screen_permission();
                crate::open_screen_settings();
            }
            Need::Keys => {
                crate::key_tap::request();
            }
            Need::Microphone | Need::Camera => {
                let camera = self == Need::Camera;
                if media_access(camera) == Access::NotAsked {
                    request_media_access(camera);
                } else {
                    let pane = if camera { "Privacy_Camera" } else { "Privacy_Microphone" };
                    let _ = std::process::Command::new("open").arg(format!("x-apple.systempreferences:com.apple.preference.security?{pane}")).spawn();
                }
            }
        }
    }
}

/// One permission: what it's for, and whether it's allowed (or a button to
/// allow it). Whether the button was clicked.
fn permission_row(ui: &mut egui::Ui, glyph: &str, title: &str, why: &str, allowed: bool) -> bool {
    let p = theme::pal(ui);
    let mut clicked = false;
    egui::Frame::new().fill(p.raise).corner_radius(ui_kit::CARD_RADIUS).inner_margin(egui::Margin::same(14)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_top(|ui| {
            ui.label(theme::text(glyph, 20.0, W::Regular).color(p.text2));
            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(theme::text(title, 14.0, W::Semibold).color(p.text));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if allowed {
                            ui.label(theme::text(format!("{}  Allowed", icon::CHECK), 13.0, W::Medium).color(p.text2));
                        } else {
                            clicked = Btn::normal("Allow").small().show(ui).clicked();
                        }
                    });
                });
                ui.add_space(2.0);
                ui.add(egui::Label::new(theme::text(why, 13.0, W::Regular).color(p.text2)).wrap());
            });
        });
    });
    clicked
}
