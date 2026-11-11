//! App icons on Linux, from the desktop's icon theme: a program (`firefox`,
//! `cs2`, `Game.exe`) is matched to its `.desktop` file — through its running
//! window's app id where it has one, else by the program it starts — whose
//! `Icon=` is looked up in the theme (the user's, then Breeze, then hicolor),
//! as PNG or SVG. A Steam game's window says which game it is
//! (`steam_app_730`), and Steam puts its icon in the theme (`steam_icon_730`).

use std::path::{Path, PathBuf};

use super::{focus, kwin_windows};

/// `id`'s icon as `size`×`size` RGBA, straight alpha, if one is found.
pub(crate) fn app_icon(id: &str, size: u32) -> Option<Vec<u8>> {
    let name = icon_name(id)?;
    let file = if Path::new(&name).is_absolute() { PathBuf::from(&name) } else { find(&name, size)? };
    if file.extension().is_some_and(|e| e.eq_ignore_ascii_case("svg")) { render_svg(&file, size) } else { load_png(&file, size) }
}

/// The icon name (or file) for program `id`.
fn icon_name(id: &str) -> Option<String> {
    // Its window's app id, while it runs.
    let app_id = kwin_windows::windows().unwrap_or_default().into_iter().find_map(|w| {
        let program = focus::program_of(w.pid?)?;
        program.file_name()?.to_str()?.eq_ignore_ascii_case(id).then_some(w.app_id).filter(|a| !a.is_empty())
    });
    if let Some(app_id) = &app_id {
        if let Some(n) = app_id.strip_prefix("steam_app_") {
            return Some(format!("steam_icon_{n}"));
        }
        if let Some(icon) = desktop_files().find_map(|f| (stem(&f).eq_ignore_ascii_case(app_id)).then(|| desktop_icon(&f)).flatten()) {
            return Some(icon);
        }
    }
    // A `.desktop` file that starts it, or says its windows are its.
    let by_program = desktop_files().find_map(|f| {
        let text = std::fs::read_to_string(&f).ok()?;
        let entry = entry(&text);
        let starts = field(entry, "Exec").and_then(exec_program).is_some_and(|p| p.eq_ignore_ascii_case(id));
        let class = field(entry, "StartupWMClass").is_some_and(|c| c.eq_ignore_ascii_case(id)) || stem(&f).eq_ignore_ascii_case(id);
        (starts || class).then(|| field(entry, "Icon").map(str::to_owned)).flatten()
    });
    by_program.or_else(|| Some(id.trim_end_matches(".exe").to_lowercase()))
}

/// Where data files are: the user's, then the system's.
fn data_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let user = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).or_else(|| home.as_ref().map(|h| h.join(".local/share")));
    let system = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    user.into_iter().chain(system.split(':').filter(|d| !d.is_empty()).map(PathBuf::from)).collect()
}

/// Every `.desktop` file, the user's first.
fn desktop_files() -> impl Iterator<Item = PathBuf> {
    data_dirs().into_iter().flat_map(|d| std::fs::read_dir(d.join("applications")).into_iter().flatten().flatten().map(|e| e.path())).filter(|p| p.extension().is_some_and(|e| e == "desktop"))
}

fn stem(file: &Path) -> &str {
    file.file_stem().and_then(|s| s.to_str()).unwrap_or("")
}

/// A `.desktop` file's main section.
fn entry(text: &str) -> &str {
    let start = text.find("[Desktop Entry]").map_or(0, |i| i + "[Desktop Entry]".len());
    let rest = &text[start..];
    rest.find("\n[").map_or(rest, |end| &rest[..end])
}

fn field<'a>(entry: &'a str, key: &str) -> Option<&'a str> {
    entry.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix('=')).map(str::trim).filter(|v| !v.is_empty())
}

fn desktop_icon(file: &Path) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    field(entry(&text), "Icon").map(str::to_owned)
}

/// The program an `Exec=` line runs: its file name, past `env VAR=…`.
fn exec_program(exec: &str) -> Option<String> {
    let mut words = exec.split_whitespace().map(|w| w.trim_matches('"'));
    let mut first = words.next()?;
    if first == "env" || first.ends_with("/env") {
        first = words.find(|w| !w.contains('='))?;
    }
    Path::new(first).file_name()?.to_str().map(str::to_owned)
}

/// The icon file for `name` closest to `size`: a PNG at least that big, else
/// an SVG, else the biggest PNG.
fn find(name: &str, size: u32) -> Option<PathBuf> {
    let theme = kde_theme();
    let mut themes: Vec<&str> = theme.iter().map(String::as_str).collect();
    themes.extend(["breeze", "hicolor"]);
    let mut bases: Vec<PathBuf> = data_dirs().into_iter().map(|d| d.join("icons")).collect();
    if let Some(home) = std::env::var_os("HOME") {
        bases.insert(0, PathBuf::from(home).join(".icons"));
    }
    const SIZES: [u32; 10] = [16, 22, 24, 32, 48, 64, 96, 128, 256, 512];
    for theme in themes {
        let mut pngs: Vec<(u32, PathBuf)> = Vec::new();
        let mut svg = None;
        for base in &bases {
            let dir = base.join(theme);
            if !dir.is_dir() {
                continue;
            }
            for s in SIZES {
                let png = dir.join(format!("{s}x{s}/apps/{name}.png"));
                if png.is_file() {
                    pngs.push((s, png));
                }
                // Breeze's layout: apps/48/name.svg, the biggest (the small
                // ones are simplified for their size).
                let sized_svg = dir.join(format!("apps/{s}/{name}.svg"));
                if sized_svg.is_file() {
                    svg = Some(sized_svg);
                }
            }
            let scalable = dir.join(format!("scalable/apps/{name}.svg"));
            if scalable.is_file() {
                svg = Some(scalable);
            }
        }
        pngs.sort_by_key(|(s, _)| *s);
        if let Some((_, p)) = pngs.iter().find(|(s, _)| *s >= size) {
            return Some(p.clone());
        }
        if svg.is_some() {
            return svg;
        }
        if let Some((_, p)) = pngs.pop() {
            return Some(p);
        }
    }
    ["png", "svg"].iter().map(|ext| PathBuf::from(format!("/usr/share/pixmaps/{name}.{ext}"))).find(|p| p.is_file())
}

/// The icon theme KDE uses, if set.
fn kde_theme() -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let text = std::fs::read_to_string(PathBuf::from(home).join(".config/kdeglobals")).ok()?;
    let icons = text.split("\n[").find(|s| s.starts_with("Icons]"))?;
    field(icons, "Theme").map(str::to_owned)
}

fn render_svg(file: &Path, size: u32) -> Option<Vec<u8>> {
    let data = std::fs::read(file).ok()?;
    let tree = resvg::usvg::Tree::from_data(&data, &resvg::usvg::Options::default()).ok()?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size)?;
    let (w, h) = (tree.size().width(), tree.size().height());
    let scale = size as f32 / w.max(h);
    let (dx, dy) = ((size as f32 - w * scale) / 2.0, (size as f32 - h * scale) / 2.0);
    resvg::render(&tree, resvg::tiny_skia::Transform::from_row(scale, 0.0, 0.0, scale, dx, dy), &mut pixmap.as_mut());
    // tiny-skia's pixels are premultiplied.
    Some(pixmap.pixels().iter().flat_map(|p| {
        let c = p.demultiply();
        [c.red(), c.green(), c.blue(), c.alpha()]
    }).collect())
}

fn load_png(file: &Path, size: u32) -> Option<Vec<u8>> {
    let img = image::open(file).ok()?.to_rgba8();
    let img = if img.dimensions() == (size, size) { img } else { image::imageops::resize(&img, size, size, image::imageops::FilterType::Lanczos3) };
    Some(img.into_raw())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Icons of programs installed here, written out with HESTECLIPS_ICONS=<dir>.
    #[test]
    #[ignore = "reads this computer's apps: cargo test -p capture icons_of_installed_apps -- --ignored"]
    fn icons_of_installed_apps() {
        let out = std::env::var_os("HESTECLIPS_ICONS").map(PathBuf::from);
        for id in ["firefox", "alacritty", "konsole", "steam", "dolphin", "not-a-program-xyz"] {
            let name = icon_name(id);
            let file = name.as_deref().and_then(|n| find(n, 64));
            let px = app_icon(id, 64);
            let opaque = px.as_ref().map_or(0, |p| p.chunks_exact(4).filter(|c| c[3] > 0).count());
            eprintln!("{id}: icon {name:?} → {file:?}, {opaque} visible pixels");
            if let (Some(dir), Some(px)) = (&out, px) {
                image::RgbaImage::from_raw(64, 64, px).unwrap().save(dir.join(format!("{id}.png"))).unwrap();
            }
        }
    }

    #[test]
    fn exec_lines() {
        assert_eq!(exec_program("/usr/lib/firefox/firefox %u").as_deref(), Some("firefox"));
        assert_eq!(exec_program("env GDK_BACKEND=x11 \"/opt/app/bin/app\" --x").as_deref(), Some("app"));
        assert_eq!(exec_program("steam steam://rungameid/730"), Some("steam".into()));
    }
}
