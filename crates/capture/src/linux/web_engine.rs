//! The engine that draws web pages on Linux (Chromium, through CEF: see
//! `super::web_host`) isn't part of the app's download: it's fetched the
//! first time a page is shown. It comes from the app's releases on GitHub
//! (the build of CEF this app speaks, [`CEF_VERSION`], stripped and packed
//! by `scripts/release-linux.sh`), is checked against its checksum, and is
//! unpacked with the app's data (`~/.local/share/hasteclip/web-engine/<version>`),
//! where every version of the app on the same CEF uses it. Engines of other
//! versions are taken out once this one's in.
//!
//! `HASTECLIP_WEB_ENGINE_DIR=<folder>` (`super::web_host::ENGINE_ENV`)
//! uses an engine unpacked there instead.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use super::web_host::ENGINE_ENV;

/// The CEF this app speaks (its C API: `super::cef`), as its download and
/// folder are named.
pub(crate) const CEF_VERSION: &str = "156.0.26";

/// GitHub's API for the repo's releases, newest first.
const RELEASES: &str = "https://api.github.com/repos/hasteclip/hasteclip/releases?per_page=30";

/// The engine's download, as a release has it.
pub(crate) fn asset() -> String {
    format!("HasteClip-web-engine-linux-x64-{CEF_VERSION}.tar.xz")
}

/// Where the engine stands.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Engine {
    /// Not here, and not asked for yet.
    Missing,
    /// Downloading: bytes so far, of how many.
    Fetching { got: u64, total: Option<u64> },
    Unpacking,
    /// Here, in this folder.
    Ready(PathBuf),
    /// It couldn't be fetched, or doesn't start: why.
    Failed(String),
}

static STATE: Mutex<Option<Engine>> = Mutex::new(None);

/// `$XDG_DATA_HOME`, or `~/.local/share`.
pub(crate) fn data_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).filter(|p| p.is_absolute()).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
}

/// The folder engines are unpacked in.
fn root() -> Option<PathBuf> {
    Some(data_dir()?.join("hasteclip").join("web-engine"))
}

/// The engine's folder, if it's here.
fn installed() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(ENGINE_ENV).map(PathBuf::from) {
        return dir.join("libcef.so").is_file().then_some(dir);
    }
    let dir = root()?.join(CEF_VERSION);
    dir.join("libcef.so").is_file().then_some(dir)
}

fn with_state<R>(f: impl FnOnce(&mut Engine) -> R) -> R {
    let mut s = STATE.lock().unwrap();
    f(s.get_or_insert_with(|| installed().map_or(Engine::Missing, Engine::Ready)))
}

/// Where it stands now.
pub(crate) fn status() -> Engine {
    with_state(|s| s.clone())
}

/// The engine's folder, if it's here; if it isn't, it starts being fetched
/// (once: after a failure, only [`retry`] fetches it again).
pub(crate) fn ensure() -> Option<PathBuf> {
    let start = with_state(|s| match s {
        Engine::Ready(dir) => Err(Some(dir.clone())),
        Engine::Missing => {
            *s = Engine::Fetching { got: 0, total: None };
            Ok(())
        }
        _ => Err(None),
    });
    match start {
        Err(dir) => dir,
        Ok(()) => {
            let _ = std::thread::Builder::new().name("web engine fetch".into()).spawn(fetch);
            None
        }
    }
}

/// Try again after a failure: the engine looked for again, fetched if it
/// isn't here.
pub(crate) fn retry() {
    with_state(|s| {
        if matches!(s, Engine::Failed(_)) {
            *s = installed().map_or(Engine::Missing, Engine::Ready);
        }
    });
    ensure();
}

/// The engine's here but doesn't start: why (until [`retry`]).
pub(crate) fn broken(why: &str) {
    with_state(|s| *s = Engine::Failed(format!("Chromium doesn't start: {why}")));
}

/// The engine started (after a failure, perhaps): it's ready.
pub(crate) fn started() {
    with_state(|s| {
        if matches!(s, Engine::Failed(_))
            && let Some(dir) = installed()
        {
            *s = Engine::Ready(dir);
        }
    });
}

/// Fetch, check and unpack it; the state says how it went.
fn fetch() {
    let result = fetch_into();
    if let Err(e) = &result {
        eprintln!("web engine: {e:#}");
    }
    with_state(|s| *s = result.map_or_else(|e| Engine::Failed(format!("Chromium couldn't be downloaded: {e:#}")), Engine::Ready));
}

const AGENT: &str = "HasteClip";

fn fetch_into() -> Result<PathBuf> {
    let root = root().context("there's no home folder to keep it in")?;
    std::fs::create_dir_all(&root).with_context(|| format!("can't write {}", root.display()))?;
    let (archive_url, sum_url) = find_download()?;
    let expected = ureq::get(&sum_url).header("User-Agent", AGENT).call().context("GitHub didn't answer")?.body_mut().read_to_string()?;
    let expected = expected.split_whitespace().next().unwrap_or_default().to_ascii_lowercase();
    if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("its checksum file is malformed");
    }
    let part = root.join(format!(".{}.part", asset()));
    let result = download(&archive_url, &part, &expected).and_then(|()| unpack(&part, &root));
    let _ = std::fs::remove_file(&part);
    let dir = result?;
    // Other versions' engines (and anything half done) go.
    if let Ok(entries) = std::fs::read_dir(&root) {
        for e in entries.flatten() {
            if e.file_name() != CEF_VERSION {
                let p = e.path();
                let _ = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
            }
        }
    }
    Ok(dir)
}

/// The engine's download and its checksum, from the newest release that
/// has this version of it.
fn find_download() -> Result<(String, String)> {
    // `HASTECLIP_RELEASES=<url>`: another list in GitHub's form (as the
    // updater takes), to try it.
    let url = std::env::var("HASTECLIP_RELEASES").unwrap_or_else(|_| RELEASES.to_owned());
    let text = ureq::get(&url)
        .header("User-Agent", AGENT)
        .header("Accept", "application/vnd.github+json")
        .call()
        .context("GitHub didn't answer")?
        .body_mut()
        .read_to_string()
        .context("GitHub's answer didn't come")?;
    let releases: serde_json::Value = serde_json::from_str(&text).context("GitHub's answer didn't make sense")?;
    let name = asset();
    pick(&releases, &name).with_context(|| format!("no release has it yet ({name})"))
}

/// In a list of releases (GitHub's form, newest first), the newest one's
/// download named `name` and its checksum (both needed), drafts left out.
fn pick(releases: &serde_json::Value, name: &str) -> Option<(String, String)> {
    let sum = format!("{name}.sha256");
    releases.as_array()?.iter().filter(|r| r["draft"].as_bool() != Some(true)).find_map(|r| {
        let url = |want: &str| r["assets"].as_array()?.iter().find(|a| a["name"].as_str() == Some(want))?["browser_download_url"].as_str().map(str::to_owned);
        Some((url(name)?, url(&sum)?))
    })
}

/// Download `url` into `to`, the state saying how far it's got; checked
/// against `expected` (SHA-256, hex).
fn download(url: &str, to: &Path, expected: &str) -> Result<()> {
    let mut response = ureq::get(url).header("User-Agent", AGENT).call().context("its download didn't start")?;
    let total = response.headers().get("content-length").and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
    let mut body = response.body_mut().with_config().limit(1 << 31).reader();
    let mut file = std::fs::File::create(to).with_context(|| format!("can't write {}", to.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut got = 0u64;
    loop {
        let n = body.read(&mut buf).context("the download stopped")?;
        if n == 0 {
            break;
        }
        std::io::Write::write_all(&mut file, &buf[..n]).context("the disk is full")?;
        hasher.update(&buf[..n]);
        got += n as u64;
        with_state(|s| *s = Engine::Fetching { got, total });
    }
    let actual: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if actual != expected {
        bail!("the download was damaged (its checksum doesn't match)");
    }
    Ok(())
}

/// Unpack the archive `file` into `root`/[`CEF_VERSION`]: aside first, then
/// renamed into place, so a half-unpacked engine is never taken for one.
fn unpack(file: &Path, root: &Path) -> Result<PathBuf> {
    with_state(|s| *s = Engine::Unpacking);
    let aside = root.join(format!(".unpacking-{CEF_VERSION}"));
    let _ = std::fs::remove_dir_all(&aside);
    std::fs::create_dir_all(&aside)?;
    // `tar` and `xz` are part of every Linux system.
    let status = Command::new("tar").arg("-xJf").arg(file).arg("-C").arg(&aside).status().context("couldn't run tar")?;
    if !status.success() || !aside.join("libcef.so").is_file() {
        let _ = std::fs::remove_dir_all(&aside);
        bail!("couldn't unpack it (is the disk full?)");
    }
    let dir = root.join(CEF_VERSION);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::rename(&aside, &dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    /// The newest release with both the engine and its checksum: a draft,
    /// or one with only half of them, is passed over.
    #[test]
    fn the_newest_release_with_the_engine_is_picked() {
        let asset = |release: u32, name: &str| serde_json::json!({ "name": name, "browser_download_url": format!("https://x/{release}/{name}") });
        let name = super::asset();
        let sum = format!("{name}.sha256");
        let releases = serde_json::json!([
            { "draft": true, "assets": [asset(1, &name), asset(1, &sum)] },
            { "draft": false, "assets": [asset(2, &name)] },
            { "draft": false, "assets": [asset(3, "HasteClip-linux-x64.tar.gz"), asset(3, &name), asset(3, &sum)] },
            { "draft": false, "assets": [asset(4, &name), asset(4, &sum)] },
        ]);
        assert_eq!(super::pick(&releases, &name), Some((format!("https://x/3/{name}"), format!("https://x/3/{sum}"))));
        assert_eq!(super::pick(&serde_json::json!([{ "assets": [] }]), &name), None);
        assert_eq!(super::pick(&serde_json::json!({ "message": "rate limited" }), &name), None);
    }
}
