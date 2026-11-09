//! Updating the macOS app: the next release's `.app`, swapped in for this one.
//!
//! An update is the release's zipped app (`HasteClip-mac-arm64.zip`, beside
//! the `.dmg` people install from): found on GitHub, downloaded and checked
//! against its `.sha256`, unzipped into HasteClip's cache folder, and
//! accepted only if it's signed with HasteClip's own certificate. That
//! certificate is what keeps macOS's permissions (Screen Recording, Input
//! Monitoring) with the app from one version to the next, and what says the
//! download is ours: the checksum comes from the same place as the download,
//! the signature can't. When the app quits, the new one and the old one
//! trade places in a single step, so there's always a whole app, old or
//! new, and the new one starts if asked to. A file the app downloads itself
//! isn't quarantined, so macOS doesn't ask about it the way it did the first
//! time.
//!
//! The whole app is swapped when its folder can be written. A standard (not
//! administrator) account can't write to Applications, but it owns an app
//! it copied there while it had administrator rights (as the Privileges app
//! gives them for a while). Then only what's inside the app is swapped: its
//! `Contents` folder, which holds all of it, signature included. Firefox's
//! updater swaps `Contents` the same way.
//!
//! Only an installed app updates: not one run from a build folder, or
//! straight from the disk image.

use std::ffi::CString;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

const ZIP: &str = "HasteClip-mac-arm64.zip";
/// GitHub's API for the repo's releases, newest first.
const RELEASES: &str = "https://api.github.com/repos/hasteclip/hasteclip/releases?per_page=10";
/// What the new app's signature must say: our identifier, our certificate
/// (`codesign -dr -` on a release). Anything else isn't put in place.
const REQUIREMENT: &str = r#"=identifier "io.github.kaillr.HasteClip" and certificate root = H"57f24a68cb31a35998890f8b6f9b94b74918c2ea""#;

/// The installed app bundle, if it's one we can update: signed as a release
/// is, and somewhere that isn't a disk image or a build folder.
pub fn install_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // …/HasteClip.app/Contents/MacOS/hasteclip
    let app = exe.parent()?.parent()?.parent()?.to_path_buf();
    if app.extension().is_none_or(|e| e != "app") {
        return None;
    }
    // Opened straight from the disk image (read-only, and gone on eject).
    if app.starts_with("/Volumes") {
        return None;
    }
    signed_by_us(&app).then_some(app)
}

/// What keeps the app from updating where it is, if anything: the app or
/// folder that isn't ours to write to. The app always has to be, as it's
/// moved or what's in it is. Its folder only has to be when nothing in the
/// app can be (see [`apply`]), or when the update has to wait beside it
/// ([`staging`]).
pub fn blocker(app: &Path) -> Option<PathBuf> {
    let Some(dir) = app.parent() else { return Some(app.to_path_buf()) };
    if !can_write(app) {
        return Some(app.to_path_buf());
    }
    if can_write(dir) {
        return None;
    }
    if !can_write(&app.join("Contents")) {
        return Some(app.to_path_buf());
    }
    staging(app).is_none_or(|s| s.starts_with(dir)).then(|| dir.to_path_buf())
}

/// Whether this account can write to `path`: make, rename and remove what's
/// in it. A folder moved to another folder needs it too.
fn can_write(path: &Path) -> bool {
    // SAFETY: a valid C string, which `access` only reads.
    CString::new(path.as_os_str().as_bytes()).is_ok_and(|p| unsafe { libc::access(p.as_ptr(), libc::W_OK) } == 0)
}

/// Whether `app` is signed with HasteClip's certificate and sealed whole.
fn signed_by_us(app: &Path) -> bool {
    Command::new("codesign")
        .args(["--verify", "--deep", "--strict", "-R", REQUIREMENT])
        .arg(app)
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// A release with a macOS app.
pub struct Release {
    pub version: String,
    zip: String,
    checksum: String,
}

/// The newest release that has a macOS app, if it's newer than `current`.
pub fn check(current: &str) -> Result<Option<Release>> {
    #[derive(serde::Deserialize)]
    struct Asset {
        name: String,
        browser_download_url: String,
    }
    #[derive(serde::Deserialize)]
    struct GhRelease {
        tag_name: String,
        draft: bool,
        prerelease: bool,
        assets: Vec<Asset>,
    }
    // `HASTECLIP_RELEASES=<url>`: another list in GitHub's form, to try an update.
    let url = std::env::var("HASTECLIP_RELEASES").unwrap_or_else(|_| RELEASES.to_owned());
    let releases: Vec<GhRelease> = ureq::get(&url)
        .header("User-Agent", "HasteClip-updater")
        .header("Accept", "application/vnd.github+json")
        .call()
        .context("GitHub didn't answer")?
        .body_mut()
        .read_json()
        .context("GitHub's answer didn't make sense")?;
    let newest = releases.into_iter().filter(|r| !r.draft && !r.prerelease).find_map(|r| {
        let url = |name: &str| r.assets.iter().find(|a| a.name == name).map(|a| a.browser_download_url.clone());
        Some(Release { version: r.tag_name.trim_start_matches('v').to_owned(), zip: url(ZIP)?, checksum: url(&format!("{ZIP}.sha256"))? })
    });
    Ok(newest.filter(|r| newer(&r.version, current)))
}

/// Whether version `a` is newer than `b` ("1.10.0" > "1.9.2").
fn newer(a: &str, b: &str) -> bool {
    let parts = |v: &str| v.split(['.', '-']).map(|p| p.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    parts(a) > parts(b)
}

/// Where a downloaded update waits: in HasteClip's cache folder when that's
/// on the app's disk, as the update is moved into place and a move can't
/// cross disks. Otherwise beside the app, hidden.
fn staging(app: &Path) -> Option<PathBuf> {
    let disk = |p: &Path| std::fs::metadata(p).ok().map(|m| m.dev());
    if let Some(cache) = dirs::cache_dir()
        && disk(&cache).is_some_and(|d| disk(app) == Some(d))
    {
        return Some(cache.join("hasteclip").join("update"));
    }
    Some(app.parent()?.join(".hasteclip-update"))
}

/// Download `release` for `app`, check it, unzip it and check its
/// signature. `progress` gets 0–100. Returns the new app.
pub fn download(release: &Release, app: &Path, mut progress: impl FnMut(u8)) -> Result<PathBuf> {
    let staging = staging(app).context("the app has no folder")?;
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).with_context(|| format!("can't write to {}", staging.display()))?;

    let expected = ureq::get(&release.checksum).header("User-Agent", "HasteClip-updater").call()?.body_mut().read_to_string()?;
    let expected = expected.split_whitespace().next().unwrap_or_default().to_ascii_lowercase();
    if expected.len() != 64 {
        bail!("the release's checksum file is malformed");
    }

    let mut response = ureq::get(&release.zip).header("User-Agent", "HasteClip-updater").call()?;
    let total = response.headers().get("content-length").and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
    let mut body = response.body_mut().with_config().limit(1 << 30).reader();
    let zip = staging.join(ZIP);
    let mut file = std::fs::File::create(&zip)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut got = 0u64;
    loop {
        let n = body.read(&mut buf)?;
        if n == 0 {
            break;
        }
        std::io::Write::write_all(&mut file, &buf[..n])?;
        hasher.update(&buf[..n]);
        got += n as u64;
        if let Some(total) = total.filter(|t| *t > 0) {
            progress((got * 100 / total).min(100) as u8);
        }
    }
    drop(file);
    let actual: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if actual != expected {
        let _ = std::fs::remove_dir_all(&staging);
        bail!("the download was damaged (its checksum doesn't match)");
    }

    // `ditto` keeps the app's signature and permissions as it unzips (a
    // plain unzip can lose them). Unzipped aside, then renamed: an update
    // half unzipped is never taken for a ready one ([`pending`]).
    let partial = staging.join("unzipping");
    std::fs::create_dir_all(&partial)?;
    let status = Command::new("ditto").arg("-x").arg("-k").arg(&zip).arg(&partial).status().context("couldn't run ditto")?;
    let _ = std::fs::remove_file(&zip);
    if !status.success() {
        bail!("couldn't unzip the update");
    }
    let unzipped = partial.join("HasteClip.app");
    if !signed_by_us(&unzipped) {
        let _ = std::fs::remove_dir_all(&staging);
        bail!("the update isn't signed by HasteClip, so it wasn't installed");
    }
    let ready = staging.join("HasteClip.app");
    std::fs::rename(&unzipped, &ready)?;
    let _ = std::fs::remove_dir(&partial);
    Ok(ready)
}

/// An update downloaded earlier and still waiting (the app didn't quit
/// normally since), with its version.
pub fn pending(app: &Path) -> Option<(PathBuf, String)> {
    waiting_in(&staging(app)?)
}

/// The update waiting in `staging`, if it's newer than this version and ours.
fn waiting_in(staging: &Path) -> Option<(PathBuf, String)> {
    let ready = staging.join("HasteClip.app");
    let version = bundle_version(&ready)?;
    (newer(&version, env!("CARGO_PKG_VERSION")) && signed_by_us(&ready)).then_some((ready, version))
}

/// The app's version, from its Info.plist.
fn bundle_version(app: &Path) -> Option<String> {
    let out = Command::new("defaults").arg("read").arg(app.join("Contents/Info")).arg("CFBundleShortVersionString").output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned()).filter(|v| !v.is_empty())
}

/// At launch, before anything else: an update downloaded last time but
/// never put in place goes in now, and the new version starts in our place.
/// Returns true when it did (the caller exits).
pub fn apply_pending_at_launch() -> bool {
    let Some(app) = install_dir() else { return false };
    let Some((ready, version)) = pending(&app) else { return false };
    eprintln!("installing version {version}, downloaded last time");
    match apply(&ready, &app, true) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("couldn't install the update: {e:#}");
            false
        }
    }
}

/// Put the new app in place of `app`, and start it if `restart`. Run once
/// the app has shut down. The whole app is swapped when its folder can be
/// written, as Apple advises (nothing inside an app is touched). When it
/// can't, only what's inside: `Contents`, all an app holds.
pub fn apply(ready: &Path, app: &Path, restart: bool) -> Result<()> {
    // Checked again: it waited on disk since it was downloaded.
    if !signed_by_us(ready) {
        bail!("the update isn't signed by HasteClip");
    }
    let staging = ready.parent().context("the update has no folder")?;
    if app.parent().is_some_and(can_write) {
        swap(ready, app).context("couldn't put the new version in place")?;
    } else {
        swap(&ready.join("Contents"), &app.join("Contents")).context("couldn't put the new version in place")?;
        // The app's own folder still carries the quarantine flag from the
        // download it was first installed from, but what's in it now came
        // from us, checked. Without the flag, macOS sees the app as it sees
        // a whole one swapped in: not quarantined, nothing to ask about.
        unquarantine(app);
    }
    let _ = std::fs::remove_dir_all(staging);
    if restart {
        // `open` starts it as an app (its own Dock icon, its permissions),
        // with the arguments we were started with (`--background` from login).
        let args: Vec<_> = std::env::args_os().skip(1).collect();
        let mut open = Command::new("open");
        open.arg("-n").arg(app);
        if !args.is_empty() {
            open.arg("--args").args(args);
        }
        open.spawn().context("couldn't start the new version")?;
    }
    Ok(())
}

/// Swap `new` and `installed` in a single step (`renamex_np` with
/// `RENAME_SWAP`): the new one takes the installed one's place and the old
/// one goes where the new one was, with no moment in between. A disk that
/// can't (exFAT says so; FAT ignores it and won't rename over a folder)
/// gets two renames instead, the old one put back if the second fails.
fn swap(new: &Path, installed: &Path) -> std::io::Result<()> {
    let c_path = |p: &Path| CString::new(p.as_os_str().as_bytes()).map_err(std::io::Error::other);
    let (from, to) = (c_path(new)?, c_path(installed)?);
    // SAFETY: two valid C strings, which `renamex_np` only reads.
    if unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_SWAP) } == 0 {
        return Ok(());
    }
    let mut aside = new.as_os_str().to_owned();
    aside.push("-old");
    let _ = std::fs::remove_dir_all(&aside);
    std::fs::rename(installed, &aside)?;
    if let Err(e) = std::fs::rename(new, installed) {
        let _ = std::fs::rename(&aside, installed);
        return Err(e);
    }
    Ok(())
}

/// Take the quarantine flag off `path` itself (not what's in it).
fn unquarantine(path: &Path) {
    if let Ok(p) = CString::new(path.as_os_str().as_bytes()) {
        // SAFETY: two valid C strings, which `removexattr` only reads.
        unsafe { libc::removexattr(p.as_ptr(), c"com.apple.quarantine".as_ptr(), libc::XATTR_NOFOLLOW) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number() {
        assert!(newer("1.10.0", "1.9.2"));
        assert!(!newer("1.7.0", "1.7.0"));
        assert!(newer("1.7.1", "1.7.0-local"));
    }

    /// An app that isn't signed with our certificate is never taken: not
    /// a build from this machine, and not anything else.
    #[test]
    fn only_our_signature_is_taken() {
        let dir = std::env::temp_dir().join(format!("hc-mac-update-{}", std::process::id()));
        let app = dir.join("HasteClip.app");
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        std::fs::copy("/bin/echo", app.join("Contents/MacOS/hasteclip")).unwrap();
        std::fs::write(app.join("Contents/Info.plist"), r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>io.github.kaillr.HasteClip</string><key>CFBundleExecutable</key><string>hasteclip</string><key>CFBundleShortVersionString</key><string>99.0.0</string></dict></plist>"#).unwrap();
        // Ad hoc, with our identifier: still not ours.
        assert!(Command::new("codesign").args(["--force", "--sign", "-"]).arg(&app).status().unwrap().success());
        assert!(!signed_by_us(&app));
        // Waiting as an update, it isn't taken for one.
        let staged = dir.join("Apps").join(".hasteclip-update").join("HasteClip.app");
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::rename(&app, &staged).unwrap();
        assert_eq!(bundle_version(&staged).as_deref(), Some("99.0.0"));
        assert!(waiting_in(staged.parent().unwrap()).is_none());
        let installed = dir.join("Apps").join("HasteClip.app");
        std::fs::create_dir_all(&installed).unwrap();
        assert!(apply(&staged, &installed, false).is_err());
        assert!(installed.exists(), "the installed app stays");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// A standard account can't write to Applications, but owns the app it
    /// copied there with administrator rights: the whole app can't be
    /// swapped, what's inside it can, and nothing is left beside it.
    #[test]
    fn swaps_what_is_inside_when_the_folder_is_read_only() {
        let dir = std::env::temp_dir().join(format!("hc-mac-swap-{}", std::process::id()));
        let apps = dir.join("Apps");
        let app = apps.join("HasteClip.app");
        let ready = dir.join("update").join("HasteClip.app");
        for (bundle, version) in [(&app, "old"), (&ready, "new")] {
            std::fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
            std::fs::write(bundle.join("Contents/MacOS/hasteclip"), version).unwrap();
        }
        set_mode(&apps, 0o555);
        assert_eq!(blocker(&app), None);
        assert!(swap(&ready, &app).is_err(), "the whole app can't leave a folder that can't be written");
        swap(&ready.join("Contents"), &app.join("Contents")).unwrap();
        let exe = |bundle: &Path| std::fs::read_to_string(bundle.join("Contents/MacOS/hasteclip")).unwrap();
        assert_eq!((exe(&app).as_str(), exe(&ready).as_str()), ("new", "old"));
        let beside: Vec<_> = std::fs::read_dir(&apps).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(beside, ["HasteClip.app"]);
        set_mode(&apps, 0o755);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An app this account can't change doesn't update, and says what's in
    /// the way.
    #[test]
    fn says_what_isnt_ours_to_write() {
        let dir = std::env::temp_dir().join(format!("hc-mac-blocker-{}", std::process::id()));
        let apps = dir.join("Apps");
        let app = apps.join("HasteClip.app");
        std::fs::create_dir_all(app.join("Contents")).unwrap();
        assert_eq!(blocker(&app), None);
        set_mode(&app, 0o555);
        assert_eq!(blocker(&app), Some(app.clone()));
        set_mode(&app, 0o755);
        // Neither the folder nor what's inside the app.
        set_mode(&apps, 0o555);
        set_mode(&app.join("Contents"), 0o555);
        assert_eq!(blocker(&app), Some(app.clone()));
        set_mode(&app.join("Contents"), 0o755);
        set_mode(&apps, 0o755);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
