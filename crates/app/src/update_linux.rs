//! Updating the Linux app: the release tarball, unpacked in place.
//!
//! The app runs from the folder its tarball was unpacked into (the binary,
//! ffmpeg, lib/, the desktop entry). An update is the next release's tarball:
//! found on GitHub (the newest release that has one: not every release is
//! built for Linux), downloaded and checked against its `.sha256`, and
//! unpacked beside the folder. When the app quits, the folder's contents are
//! swapped for the new ones (the running binary can be replaced: Linux keeps
//! the old file for as long as it runs), and the new version is started if
//! asked to. Nothing else is needed: no installer, no admin rights — as long
//! as the folder is the user's to write to.
//!
//! A development build (run from `target/`) has nothing to update.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

const TARBALL: &str = "HesteClips-linux-x64.tar.gz";
/// GitHub's API for the repo's releases, newest first.
const RELEASES: &str = "https://api.github.com/repos/Kaillr/hesteclips/releases?per_page=10";

/// The installed app: the folder the binary is in, if it's one we can update
/// (unpacked from a release, ours to write to).
pub fn install_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.to_path_buf();
    // A build, not a release: `target/debug`, `target/release`.
    if dir.components().any(|c| c.as_os_str() == "target") {
        return None;
    }
    // A release folder has the desktop entry beside the binary.
    if !dir.join("io.github.kaillr.HesteClips.desktop").is_file() {
        return None;
    }
    Some(dir)
}

/// Whether the install folder (and the folder it's in, where the new
/// version is unpacked) can be written: not when it's somewhere like /opt.
pub fn writable(dir: &Path) -> bool {
    let probe = |d: &Path| {
        let f = d.join(format!(".hesteclips-write-test-{}", std::process::id()));
        let ok = std::fs::write(&f, b"").is_ok();
        let _ = std::fs::remove_file(&f);
        ok
    };
    probe(dir) && dir.parent().is_some_and(probe)
}

/// A release with a Linux tarball.
pub struct Release {
    pub version: String,
    tarball: String,
    checksum: String,
}

/// The newest release that has a Linux tarball, if it's newer than `current`.
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
    // `HESTECLIPS_RELEASES=<url>`: another list in GitHub's form, to try an update.
    let url = std::env::var("HESTECLIPS_RELEASES").unwrap_or_else(|_| RELEASES.to_owned());
    let releases: Vec<GhRelease> = ureq::get(&url)
        .header("User-Agent", "HesteClips-updater")
        .header("Accept", "application/vnd.github+json")
        .call()
        .context("GitHub didn't answer")?
        .body_mut()
        .read_json()
        .context("GitHub's answer didn't make sense")?;
    let newest = releases.into_iter().filter(|r| !r.draft && !r.prerelease).find_map(|r| {
        let url = |name: &str| r.assets.iter().find(|a| a.name == name).map(|a| a.browser_download_url.clone());
        Some(Release {
            version: r.tag_name.trim_start_matches('v').to_owned(),
            tarball: url(TARBALL)?,
            checksum: url(&format!("{TARBALL}.sha256"))?,
        })
    });
    Ok(newest.filter(|r| newer(&r.version, current)))
}

/// Whether version `a` is newer than `b` ("1.10.0" > "1.9.2").
fn newer(a: &str, b: &str) -> bool {
    let parts = |v: &str| v.split(['.', '-']).map(|p| p.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    parts(a) > parts(b)
}

/// Download `release` beside the install folder and unpack it there, checked
/// against its checksum. `progress` gets 0–100. Returns the unpacked folder.
pub fn download(release: &Release, dir: &Path, mut progress: impl FnMut(u8)) -> Result<PathBuf> {
    let parent = dir.parent().context("the app's folder has no parent")?;
    let staging = parent.join(".hesteclips-update");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).context("can't write next to the app's folder")?;

    let expected = ureq::get(&release.checksum).header("User-Agent", "HesteClips-updater").call()?.body_mut().read_to_string()?;
    let expected = expected.split_whitespace().next().unwrap_or_default().to_ascii_lowercase();
    if expected.len() != 64 {
        bail!("the release's checksum file is malformed");
    }

    let mut response = ureq::get(&release.tarball).header("User-Agent", "HesteClips-updater").call()?;
    let total = response.headers().get("content-length").and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
    let mut body = response.body_mut().with_config().limit(1 << 30).reader();
    let file_path = staging.join(TARBALL);
    let mut file = std::fs::File::create(&file_path)?;
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

    // `tar` is part of every Linux system, like `ls`. Unpacked aside, then
    // renamed into place: an update half unpacked (the app quit meanwhile) is
    // never taken for a ready one ([`pending`]).
    let partial = staging.join("unpacking");
    std::fs::create_dir_all(&partial)?;
    let status = Command::new("tar").arg("-xzf").arg(&file_path).arg("-C").arg(&partial).status().context("couldn't run tar")?;
    if !status.success() {
        bail!("couldn't unpack the update");
    }
    let _ = std::fs::remove_file(&file_path);
    if !partial.join("HesteClips-linux-x64").join("hesteclips").is_file() {
        bail!("the update doesn't contain the app");
    }
    let unpacked = staging.join("HesteClips-linux-x64");
    std::fs::rename(partial.join("HesteClips-linux-x64"), &unpacked)?;
    let _ = std::fs::remove_dir(&partial);
    Ok(unpacked)
}

/// An update downloaded earlier and left unpacked (the app didn't quit
/// normally since), with its version from the folder's README.
pub fn pending(dir: &Path) -> Option<(PathBuf, String)> {
    let unpacked = dir.parent()?.join(".hesteclips-update").join("HesteClips-linux-x64");
    if !unpacked.join("hesteclips").is_file() {
        return None;
    }
    let readme = std::fs::read_to_string(unpacked.join("README.txt")).ok()?;
    let version = readme.lines().next()?.split_whitespace().nth(1)?.to_owned();
    newer(&version, env!("CARGO_PKG_VERSION")).then_some((unpacked, version))
}

/// At launch, before anything else: an update downloaded last time but never
/// put in place (the app was killed, or the computer shut down) goes in now,
/// and the new version starts in our place — so a launch is never one
/// version behind. Returns true when it did (the caller exits).
pub fn apply_pending_at_launch() -> bool {
    let Some(dir) = install_dir() else { return false };
    let Some((unpacked, version)) = pending(&dir) else { return false };
    eprintln!("installing version {version}, downloaded last time");
    match apply(&unpacked, &dir, true) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("couldn't install the update: {e:#}");
            false
        }
    }
}

/// Put the unpacked update in place of the install folder's contents, and
/// start it if `restart`. Run once the app has shut down. Each file is moved
/// over its old self (an atomic rename per file); what the new version no
/// longer has is left alone.
pub fn apply(unpacked: &Path, dir: &Path, restart: bool) -> Result<()> {
    move_into(unpacked, dir)?;
    if let Some(staging) = unpacked.parent() {
        let _ = std::fs::remove_dir_all(staging);
    }
    if restart {
        // With the arguments we were started with (`--background` from autostart).
        Command::new(dir.join("hesteclips")).args(std::env::args_os().skip(1)).spawn().context("couldn't start the new version")?;
    }
    Ok(())
}

/// Move everything in `from` into `to`, replacing files of the same name.
fn move_into(from: &Path, to: &Path) -> Result<()> {
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            std::fs::create_dir_all(&target)?;
            move_into(&entry.path(), &target)?;
        } else {
            std::fs::rename(entry.path(), &target).with_context(|| format!("couldn't replace {}", target.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number() {
        assert!(newer("1.10.0", "1.9.2"));
        assert!(newer("2.0.0", "1.99.99"));
        assert!(!newer("1.7.0", "1.7.0"));
        assert!(!newer("1.6.0", "1.7.0"));
        assert!(newer("1.7.1", "1.7.0-local"));
    }

    #[test]
    fn update_replaces_files_in_place() {
        let root = std::env::temp_dir().join(format!("hc-update-test-{}", std::process::id()));
        let (old, new) = (root.join("app"), root.join(".hesteclips-update").join("HesteClips-linux-x64"));
        std::fs::create_dir_all(old.join("lib")).unwrap();
        std::fs::create_dir_all(new.join("lib")).unwrap();
        std::fs::write(old.join("hesteclips"), "old").unwrap();
        std::fs::write(old.join("lib/a.so"), "old").unwrap();
        std::fs::write(old.join("mine.txt"), "kept").unwrap();
        std::fs::write(new.join("hesteclips"), "new").unwrap();
        std::fs::write(new.join("lib/a.so"), "new").unwrap();
        std::fs::write(new.join("lib/b.so"), "new").unwrap();
        apply(&new, &old, false).unwrap();
        assert_eq!(std::fs::read_to_string(old.join("hesteclips")).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(old.join("lib/a.so")).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(old.join("lib/b.so")).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(old.join("mine.txt")).unwrap(), "kept");
        assert!(!root.join(".hesteclips-update").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
