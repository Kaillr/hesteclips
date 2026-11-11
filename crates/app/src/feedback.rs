//! Feedback: the rail's last item, to report a bug or suggest an idea. Both
//! are GitHub issue forms (`.github/ISSUE_TEMPLATE`), opened in the browser:
//! a bug report with the version and the system filled in, the fields people
//! skip or get wrong (a build of your own says so, and its commit: its version
//! number is only a placeholder), and the log file shown in the file manager,
//! ready to drag in as the form asks. The ideas others asked for are a click
//! away too, to add a thumbs up to rather than ask again. Nothing here pops up
//! by itself.
//!
//! Only the forms' text fields are filled in: GitHub fills those from the
//! link, and a link it won't take (a label someone may not set) is a 404.

use std::sync::OnceLock;

use egui::{Rect, Sense};
use egui_phosphor::regular as icon;

use crate::App;
use crate::ui_kit::{self, Item};

/// What the menu was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Choice {
    Bug,
    Idea,
    Ideas,
}

/// The rail's feedback item in `r`, and its menu: what was chosen in it.
pub(crate) fn item(ui: &mut egui::Ui, r: Rect) -> Option<Choice> {
    let resp = ui.interact(r, egui::Id::new(("rail", "Feedback")), Sense::click());
    let open = egui::Popup::is_id_open(ui.ctx(), resp.id.with("menu"));
    crate::shell::rail_cell(ui, r, resp.id, egui_phosphor::fill::CHAT_CIRCLE_DOTS, "Feedback", resp.hovered(), open);
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let resp = if open { resp } else { resp.on_hover_text("Report a bug or suggest an idea") };
    ui_kit::menu_popup_beside(&resp, 240.0, |ui| {
        ui_kit::menu_head(ui, "On GitHub, with a free account");
        // macOS keeps no log file (see `logfile`).
        let tip = if cfg!(target_os = "macos") {
            "Your version and system are filled in"
        } else {
            "Your version and system are filled in, and your log file shows in a folder, to drag into the report"
        };
        let bug = Item::new("Report a bug").icon(icon::BUG).tip(tip);
        if bug.show(ui).clicked() {
            return Some(Choice::Bug);
        }
        if Item::new("Suggest an idea").icon(icon::LIGHTBULB).show(ui).clicked() {
            return Some(Choice::Idea);
        }
        ui_kit::menu_sep(ui);
        if Item::new("Ideas others asked for").icon(icon::THUMBS_UP).tip("The most wanted first: give the ones you'd like a thumbs up").show(ui).clicked() {
            return Some(Choice::Ideas);
        }
        None
    })
    .flatten()
}

impl App {
    /// Do what the feedback menu was asked for.
    pub(crate) fn feedback(&mut self, ctx: &egui::Context, choice: Choice) {
        let url = match choice {
            Choice::Bug => {
                // Shown first, so the browser opens in front of it.
                if let Some(log) = crate::logfile::current()
                    && crate::clips::reveal_in_file_manager(log).is_ok()
                {
                    self.toast_icon(icon::FILE_TEXT, "Drag hesteclips.log from the folder that opened into the report");
                }
                bug_report_url(&version(&self.updater.version()), &setup(self.capture_summary()))
            }
            Choice::Idea => feature_request_url(),
            Choice::Ideas => ideas_url(),
        };
        ctx.open_url(egui::OpenUrl::new_tab(url));
    }

    /// How the picture is recorded, for a bug report (Windows: what most
    /// recording bugs turn on): the screen and how, or games and apps, and
    /// whether games may be recorded through the game capture hook.
    fn capture_summary(&self) -> Option<String> {
        if !cfg!(windows) {
            return None;
        }
        let what = match self.video_source() {
            capture::VideoSource::Apps { .. } => "records games and apps".to_owned(),
            capture::VideoSource::Screen { .. } => {
                use crate::settings::ScreenCapture::*;
                let how = match (capture::screen_capture_in_use(), self.settings.screen_capture) {
                    (Some(u), _) if u.duplication => "Desktop Duplication",
                    (Some(u), _) if u.fell_back => "Windows Graphics Capture, standing in for Desktop Duplication",
                    (Some(_), _) | (None, Wgc) => "Windows Graphics Capture",
                    (None, Duplication) => "Desktop Duplication",
                    (None, Auto) if capture::screen_capture_auto_duplicates() => "Desktop Duplication",
                    (None, Auto) => "Windows Graphics Capture",
                };
                format!("records the screen with {how}")
            }
        };
        let hook = match (capture::game_hook_available(), self.settings.game_capture) {
            (false, _) => "without the game capture hook's files",
            (true, true) => "game capture on",
            (true, false) => "game capture off",
        };
        Some(format!("{what}, {hook}"))
    }
}

/// The graphics card the window draws with, noted at launch.
static GPU: OnceLock<String> = OnceLock::new();

pub(crate) fn note_gpu(name: String) {
    if !name.is_empty() {
        let _ = GPU.set(name);
    }
}

/// The bug report form, with the version and the system filled in.
fn bug_report_url(version: &str, setup: &str) -> String {
    format!("{}/issues/new?template=bug_report.yml&version={}&setup={}", crate::update::REPO, query(version), query(setup))
}

fn feature_request_url() -> String {
    format!("{}/issues/new?template=feature_request.yml", crate::update::REPO)
}

/// The open ideas, the most wanted first.
fn ideas_url() -> String {
    format!("{}/issues?q={}", crate::update::REPO, query("is:issue is:open label:enhancement sort:reactions-+1-desc"))
}

/// `s` as a value in a link: anything but plain characters percent-encoded.
fn query(s: &str) -> String {
    let mut out = String::new();
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// This copy's version as a bug report gives it: a release's, or for a build
/// of your own, where it was built from (asked of git, in the folder the
/// build folder is in).
fn version(release: &str) -> String {
    let Some(build) = crate::tray::build_folder() else { return release.to_owned() };
    let repo = build.parent().unwrap_or(&build);
    let git = |args: &[&str]| {
        let mut cmd = std::process::Command::new("git");
        cmd.arg("-C").arg(repo).args(args);
        // A GUI app: each run would flash a console window.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let out = cmd.output().ok().filter(|o| o.status.success())?;
        Some(String::from_utf8_lossy(&out.stdout).trim().to_owned()).filter(|s| !s.is_empty())
    };
    built_from(git(&["rev-parse", "--abbrev-ref", "HEAD"]), git(&["describe", "--tags", "--always", "--dirty"]))
}

/// "built from source: main at v0.4.0-3-gfceb397", as much of it as is known.
fn built_from(branch: Option<String>, at: Option<String>) -> String {
    match (branch.filter(|b| b != "HEAD"), at) {
        (Some(branch), Some(at)) => format!("built from source: {branch} at {at}"),
        (None, Some(at)) => format!("built from source at {at}"),
        _ => "built from source".to_owned(),
    }
}

/// The report's system line: the system and its version, the graphics card,
/// and how the picture is recorded (see [`App::capture_summary`]).
fn setup(capture: Option<String>) -> String {
    let mut parts = vec![crate::logfile::system()];
    parts.extend(GPU.get().cloned());
    parts.extend(capture);
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links() {
        assert_eq!(
            bug_report_url("0.4.0", "Windows 11 Pro 24H2 (build 26100.4061), NVIDIA GeForce RTX 4060 Ti"),
            "https://github.com/Kaillr/hesteclips/issues/new?template=bug_report.yml&version=0.4.0&setup=Windows%2011%20Pro%2024H2%20%28build%2026100.4061%29%2C%20NVIDIA%20GeForce%20RTX%204060%20Ti"
        );
        assert_eq!(feature_request_url(), "https://github.com/Kaillr/hesteclips/issues/new?template=feature_request.yml");
        assert_eq!(ideas_url(), "https://github.com/Kaillr/hesteclips/issues?q=is%3Aissue%20is%3Aopen%20label%3Aenhancement%20sort%3Areactions-%2B1-desc");
        // Letters beyond ASCII as their UTF-8 bytes.
        assert_eq!(query("Kraków & ü"), "Krak%C3%B3w%20%26%20%C3%BC");
    }

    #[test]
    fn a_build_of_your_own_says_where_its_from() {
        assert_eq!(built_from(Some("main".into()), Some("v0.4.0-3-gfceb397-dirty".into())), "built from source: main at v0.4.0-3-gfceb397-dirty");
        // A detached checkout has no branch to name.
        assert_eq!(built_from(Some("HEAD".into()), Some("fceb397".into())), "built from source at fceb397");
        assert_eq!(built_from(None, None), "built from source");
        // Tests run from the build folder: git answers there.
        assert!(version("0.4.0").starts_with("built from source"), "{}", version("0.4.0"));
    }

    /// The item opens its menu, and choosing in it says what was chosen.
    #[test]
    fn menu() {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let item_rect = Rect::from_min_size(egui::pos2(8.0, 500.0), egui::vec2(64.0, 58.0));
        let screen = Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 600.0));
        let frame = |events: Vec<egui::Event>| {
            let raw = egui::RawInput { screen_rect: Some(screen), max_texture_side: Some(8192), events, ..Default::default() };
            let mut chosen = None;
            let mut out = ctx.run_ui(raw, |ui| chosen = item(ui, item_rect));
            out.textures_delta.clear();
            chosen
        };
        let click = |at: egui::Pos2| {
            vec![
                egui::Event::PointerMoved(at),
                egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: true, modifiers: Default::default() },
                egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: false, modifiers: Default::default() },
            ]
        };
        frame(vec![]);
        assert_eq!(frame(click(item_rect.center())), None);
        frame(vec![]);
        let id = egui::Id::new(("rail", "Feedback")).with("menu");
        assert!(egui::Popup::is_id_open(&ctx, id), "the menu didn't open");
        // Its rows, beside the item: the first under the menu's heading.
        let menu = ctx.memory(|m| m.area_rect(id)).expect("the menu's area");
        assert!(menu.left() >= item_rect.right(), "not beside the item: {menu:?}");
        let mut got = None;
        for y in (menu.top() as i32..menu.bottom() as i32).step_by(4) {
            let at = egui::pos2(menu.center().x, y as f32);
            if let Some(c) = frame(click(at)) {
                got = Some(c);
                break;
            }
            if !egui::Popup::is_id_open(&ctx, id) {
                frame(click(item_rect.center()));
                frame(vec![]);
            }
        }
        assert_eq!(got, Some(Choice::Bug), "the first choice in the menu");
    }
}
