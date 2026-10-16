//! Moving clips between libraries: a `.hesteclips` file holds clips with
//! everything that goes with them, so they arrive in another library (another
//! computer, a friend's) just as they were: their game folder, their edit
//! (the original and what was cut), their markers and kills, and the
//! collections they're in.
//!
//! The file is a zip with nothing compressed (video doesn't shrink, and
//! stored files are written and read at disk speed):
//!
//! ```text
//! manifest.json          what's in it (see `Manifest`)
//! clips/<n>/<name>       each clip as it looks now (the library's file)
//! clips/<n>/assets/…     its edit folder (`.hesteclips/<id>/`), if it's edited
//! ```
//!
//! `manifest.json` has a format number: a newer app reads every older one,
//! and an older app says plainly that the file is too new.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::clips::Clip;
use crate::collections::Collections;
use crate::moments::Moments;

/// The file's extension.
pub const EXT: &str = "hesteclips";
/// The newest manifest this app writes and reads.
const FORMAT: u32 = 1;

#[derive(Serialize, Deserialize)]
struct Manifest {
    /// See [`FORMAT`].
    format: u32,
    /// The app that wrote it (for reading old files, should that matter).
    app: String,
    clips: Vec<Entry>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Entry {
    /// Its folder in the zip (`clips/<n>`) and file name there.
    dir: String,
    name: String,
    /// Where it was in its library: its game's folder, or none.
    #[serde(default)]
    folder: Option<String>,
    /// Its date in the library (file modified time, seconds since 1970): an
    /// imported clip keeps its day.
    modified: i64,
    size: u64,
    /// Its id (names its edit folder), if it's been edited.
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    moments: Moments,
    /// Names of the collections it's in.
    #[serde(default)]
    collections: Vec<String>,
}

/// How far an export or import has come: bytes done, of how many.
pub type Progress = std::sync::Arc<(std::sync::atomic::AtomicU64, std::sync::atomic::AtomicU64)>;

fn progress_add(p: &Progress, n: u64) {
    p.0.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
}

/// Write `clips` (of library `lib`) to `out`, with their edits and moments;
/// `collections`: the names of the collections each clip is in (in order).
/// How many were written.
pub fn export(lib: &Path, clips: &[Clip], collections: &[Vec<String>], out: &Path, progress: &Progress) -> Result<usize, String> {
    use zip::write::SimpleFileOptions;
    // Video doesn't compress: stored, and big files (over 4 GB) allowed.
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored).large_file(true);
    let total: u64 = clips.iter().map(|c| c.size_bytes + edit_size(lib, c)).sum();
    progress.1.store(total, std::sync::atomic::Ordering::Relaxed);

    // Written aside, then moved in: never half a file under the real name.
    let partial = out.with_extension(format!("{EXT}.part"));
    let file = std::fs::File::create(&partial).map_err(|e| format!("Couldn't create {}: {e}", partial.display()))?;
    let mut zip = zip::ZipWriter::new(std::io::BufWriter::with_capacity(1 << 20, file));
    let mut entries = Vec::new();
    let result = (|| -> Result<(), String> {
        for (n, clip) in clips.iter().enumerate() {
            let dir = format!("clips/{n}");
            let name = clip.path.file_name().map(|s| s.to_string_lossy().into_owned()).ok_or("A clip has no name")?;
            add_file(&mut zip, &format!("{dir}/{name}"), &clip.path, options, progress)?;
            // Its edit folder: the original and what was done to it.
            if let Some(id) = &clip.id {
                let assets = crate::store::assets_dir(lib, id);
                for f in files_in(&assets) {
                    let rel = f.strip_prefix(&assets).map_err(|e| e.to_string())?;
                    // A render half-written when the app closed isn't part of it.
                    if rel.to_string_lossy().starts_with("rendering.") {
                        continue;
                    }
                    add_file(&mut zip, &format!("{dir}/assets/{}", slashed(rel)), &f, options, progress)?;
                }
            }
            let in_collections = collections.get(n).cloned().unwrap_or_default();
            entries.push(Entry {
                dir,
                name,
                folder: clip.folder.clone(),
                modified: seconds(clip.modified),
                size: clip.size_bytes,
                id: clip.id.clone(),
                moments: crate::moments::of(&clip.path),
                collections: in_collections,
            });
        }
        let manifest = Manifest { format: FORMAT, app: format!("HesteClips {}", env!("CARGO_PKG_VERSION")), clips: entries.clone() };
        zip.start_file("manifest.json", SimpleFileOptions::default()).map_err(|e| e.to_string())?;
        zip.write_all(&serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        zip.finish().map_err(|e| e.to_string())?.flush().map_err(|e| e.to_string())?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&partial);
        return Err(e);
    }
    std::fs::rename(&partial, out).map_err(|e| e.to_string())?;
    Ok(entries.len())
}

/// What an import did.
#[derive(Debug, Default, PartialEq)]
pub struct Imported {
    /// The clips added, where they are now, and the names of the collections
    /// each was in (for the app to put it in: see [`Imported::collect`]).
    pub added: Vec<(PathBuf, Vec<String>)>,
    /// Clips the library already had (left as they were).
    pub skipped: usize,
}

impl Imported {
    /// Put the new clips in their collections, by name, making the ones the
    /// library hasn't got. (On the app's own thread: it owns the collections.)
    pub fn collect(&self, collections: &mut Collections) {
        for (clip, names) in &self.added {
            for name in names.iter().map(|n| n.trim()).filter(|n| !n.is_empty()) {
                let id = match collections.list().iter().find(|c| c.name.eq_ignore_ascii_case(name)) {
                    Some(c) => c.id.clone(),
                    None => collections.create(name),
                };
                collections.add(&id, std::slice::from_ref(clip));
            }
        }
    }
}

/// Put the clips in `file` into library `lib`: each in its game's folder,
/// with its edit and moments (their collections are for the app to add, see
/// [`Imported::collect`]). A clip the library already has (same id, or same
/// name and size in the same folder) is left alone. Names that are taken get
/// " (2)".
pub fn import(file: &Path, lib: &Path, progress: &Progress) -> Result<Imported, String> {
    let f = std::fs::File::open(file).map_err(|e| format!("Couldn't open {}: {e}", file.display()))?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(f)).map_err(|_| "This isn't a HesteClips clips file.".to_owned())?;
    let manifest: Manifest = {
        let mut m = zip.by_name("manifest.json").map_err(|_| "This isn't a HesteClips clips file (it has no list of clips).".to_owned())?;
        let mut s = String::new();
        m.read_to_string(&mut s).map_err(|e| e.to_string())?;
        serde_json::from_str(&s).map_err(|e| format!("The list of clips in it can't be read: {e}"))?
    };
    if manifest.format > FORMAT {
        return Err(format!("These clips were exported by a newer HesteClips ({}). Update HesteClips to import them.", manifest.app));
    }
    // Everything but the list itself is copied out (or skipped and counted).
    let total: u64 = (0..zip.len()).filter_map(|i| zip.by_index_raw(i).ok().filter(|f| f.name().is_ok_and(|n| n != "manifest.json")).map(|f| f.size())).sum();
    progress.1.store(total, std::sync::atomic::Ordering::Relaxed);

    let have_ids: HashSet<String> = files_in(&lib.join(crate::store::DIR)).iter().filter_map(|p| p.parent()?.file_name()?.to_str().map(str::to_owned)).collect();
    let mut done = Imported::default();
    for e in &manifest.clips {
        let folder = e.folder.as_deref().map(safe_name).filter(|f| !f.is_empty());
        let dir = folder.as_ref().map_or_else(|| lib.to_path_buf(), |f| lib.join(f));
        let name = safe_name(&e.name);
        if name.is_empty() {
            continue;
        }
        let id = e.id.clone().filter(|id| crate::store::valid_id(id));
        // Already here: the same edited clip, or the same file.
        let same_file = std::fs::metadata(dir.join(&name)).is_ok_and(|m| m.len() == e.size);
        if same_file || id.as_ref().is_some_and(|id| have_ids.contains(id)) {
            done.skipped += 1;
            let prefix = format!("{}/", e.dir);
            let skipped: u64 = (0..zip.len()).filter_map(|i| zip.by_index_raw(i).ok().filter(|f| f.name().is_ok_and(|n| n.starts_with(&prefix))).map(|f| f.size())).sum();
            progress_add(progress, skipped);
            continue;
        }
        std::fs::create_dir_all(&dir).map_err(|err| format!("Couldn't make {}: {err}", dir.display()))?;
        let to = free_name(&dir, &name);
        extract(&mut zip, &format!("{}/{}", e.dir, e.name), &to, progress)?;
        set_modified(&to, e.modified);
        // Its edit folder.
        if let Some(id) = &id {
            let prefix = format!("{}/assets/", e.dir);
            let names: Vec<String> = zip.file_names().filter_map(Result::ok).filter(|n| n.starts_with(&prefix) && !n.ends_with('/')).map(|n| n.into_owned()).collect();
            if !names.is_empty() {
                let assets = crate::store::create_assets_dir(lib, id).map_err(|err| err.to_string())?;
                for n in names {
                    let rel = &n[prefix.len()..];
                    // Only plain names inside the folder: nothing can land outside it.
                    let Some(dest) = inside(&assets, rel) else { continue };
                    extract(&mut zip, &n, &dest, progress)?;
                }
            }
        }
        crate::moments::put(&to, e.moments.clone());
        done.added.push((to, e.collections.clone()));
    }
    Ok(done)
}

fn add_file<W: Write + std::io::Seek>(zip: &mut zip::ZipWriter<W>, name: &str, from: &Path, options: zip::write::SimpleFileOptions, progress: &Progress) -> Result<(), String> {
    zip.start_file(name, options).map_err(|e| e.to_string())?;
    let mut f = std::fs::File::open(from).map_err(|e| format!("Couldn't read {}: {e}", from.display()))?;
    copy(&mut f, zip, progress).map_err(|e| format!("Couldn't write {}: {e}", from.display()))
}

fn extract<R: Read + std::io::Seek>(zip: &mut zip::ZipArchive<R>, name: &str, to: &Path, progress: &Progress) -> Result<(), String> {
    let mut entry = zip.by_name(name).map_err(|_| format!("{name} is missing from the file"))?;
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    // Written aside, then moved in: the library never sees half a clip.
    let partial = to.with_file_name(format!(".{}.importing", to.file_name().unwrap_or_default().to_string_lossy()));
    let result = (|| {
        let mut out = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&partial)?);
        copy(&mut entry, &mut out, progress)?;
        out.flush()?;
        std::fs::rename(&partial, to)
    })();
    result.map_err(|e| {
        let _ = std::fs::remove_file(&partial);
        format!("Couldn't write {}: {e}", to.display())
    })
}

/// Copy, counting the bytes into `progress`.
fn copy(from: &mut impl Read, to: &mut impl Write, progress: &Progress) -> std::io::Result<()> {
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = from.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        to.write_all(&buf[..n])?;
        progress_add(progress, n as u64);
    }
}

fn edit_size(lib: &Path, clip: &Clip) -> u64 {
    clip.id.as_ref().map_or(0, |id| files_in(&crate::store::assets_dir(lib, id)).iter().filter_map(|f| std::fs::metadata(f).ok()).map(|m| m.len()).sum())
}

/// Every file under `dir`.
fn files_in(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(files_in(&p));
        } else {
            out.push(p);
        }
    }
    out
}

fn slashed(p: &Path) -> String {
    p.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
}

/// `rel` (slashes) under `dir`, if it's only plain names: no `..`, no root.
fn inside(dir: &Path, rel: &str) -> Option<PathBuf> {
    let mut out = dir.to_path_buf();
    for part in rel.split('/') {
        let part = safe_name(part);
        if part.is_empty() {
            return None;
        }
        out.push(part);
    }
    Some(out)
}

/// A file or folder name from the file, made safe: no slashes, no `..`, none
/// of the characters Windows can't have.
fn safe_name(s: &str) -> String {
    let s = crate::clips::sanitize_name(s);
    if s == ".." || s == "." { String::new() } else { s }
}

/// `name` in `dir`, or "name (2).mp4" and so on if it's taken.
fn free_name(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    if !path.exists() {
        return path;
    }
    let p = Path::new(name);
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (2..).map(|n| dir.join(format!("{stem} ({n}){ext}"))).find(|p| !p.exists()).expect("a free name")
}

fn seconds(t: std::time::SystemTime) -> i64 {
    t.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

fn set_modified(path: &Path, secs: i64) {
    if secs <= 0 {
        return;
    }
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64);
    if let Ok(f) = std::fs::File::options().write(true).open(path) {
        let _ = f.set_modified(t);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progress() -> Progress {
        Default::default()
    }


    /// A clip file with a size of its own, in `dir`, as the library lists it.
    fn clip(lib: &Path, folder: Option<&str>, name: &str, bytes: usize) -> Clip {
        let dir = folder.map_or_else(|| lib.to_path_buf(), |f| lib.join(f));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, vec![7u8; bytes]).unwrap();
        let modified = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_760_000_000);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(modified).unwrap();
        Clip { path, name: name.into(), modified, created: None, size_bytes: bytes as u64, id: None, original: None, folder: folder.map(str::to_owned) }
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hesteclips-archive-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Clips go over with their folder, date, edit folder, markers and
    /// collections; importing again adds nothing.
    #[test]
    fn round_trip() {
        let _turn = crate::store::LIBRARY_TURN.lock().unwrap_or_else(|e| e.into_inner());
        let (a, b) = (temp("a"), temp("b"));
        crate::store::set_library(&a);
        let mut c1 = clip(&a, Some("Counter-Strike 2"), "clip_2026-10-14_10-47-46 - Ace on Ancient.mp4", 1000);
        let c2 = clip(&a, None, "clip_2026-10-15_12-18-05.mp4", 500);
        // c1 is edited: its id and edit folder.
        let id = "0123456789abcdef0123456789abcdef".to_owned();
        c1.id = Some(id.clone());
        let assets = crate::store::assets_dir(&a, &id);
        std::fs::create_dir_all(&assets).unwrap();
        std::fs::write(assets.join("original.mp4"), vec![1u8; 3000]).unwrap();
        std::fs::write(assets.join("edit.json"), b"{}").unwrap();
        crate::moments::add_marker(&c1.path, 21.5);
        let out = a.join("export.hesteclips");
        let names = vec![vec!["Best of the week".to_owned()], vec![]];
        assert_eq!(export(&a, &[c1.clone(), c2.clone()], &names, &out, &progress()).unwrap(), 2);

        crate::store::set_library(&b);
        let mut into = Collections::load(&b);
        let p = progress();
        let done = import(&out, &b, &p).unwrap();
        done.collect(&mut into);
        assert_eq!(done.added.len(), 2);
        assert_eq!(done.skipped, 0);
        let got = b.join("Counter-Strike 2").join("clip_2026-10-14_10-47-46 - Ace on Ancient.mp4");
        assert_eq!(std::fs::read(&got).unwrap().len(), 1000);
        assert_eq!(std::fs::metadata(&got).unwrap().modified().unwrap(), c1.modified, "keeps its day");
        assert!(b.join("clip_2026-10-15_12-18-05.mp4").exists(), "loose clips stay loose");
        assert_eq!(std::fs::read(crate::store::assets_dir(&b, &id).join("original.mp4")).unwrap().len(), 3000, "its original came along");
        assert_eq!(crate::moments::of(&got).markers, vec![21.5]);
        let made = into.list().iter().find(|c| c.name == "Best of the week").expect("its collection was made");
        assert!(into.contains(&made.id, &got));
        assert_eq!(p.0.load(std::sync::atomic::Ordering::Relaxed), p.1.load(std::sync::atomic::Ordering::Relaxed), "progress reaches the end");

        // Again: the library has them all.
        let again = import(&out, &b, &progress()).unwrap();
        assert_eq!(again, Imported { added: vec![], skipped: 2 });
        let _ = (std::fs::remove_dir_all(&a), std::fs::remove_dir_all(&b));
    }

    /// Real clips, if `HESTECLIPS_TEST_LIBRARY` names a library: everything
    /// in it goes out and comes back byte for byte (a check on real files,
    /// edited ones too; it only reads the library). Run with
    /// `cargo test --release archive::tests::real_library -- --ignored`.
    #[test]
    #[ignore = "needs a real library"]
    fn real_library() {
        let _turn = crate::store::LIBRARY_TURN.lock().unwrap_or_else(|e| e.into_inner());
        let lib = PathBuf::from(std::env::var("HESTECLIPS_TEST_LIBRARY").expect("HESTECLIPS_TEST_LIBRARY"));
        crate::store::set_library(&lib);
        let mut clips = Vec::new();
        for entry in std::fs::read_dir(&lib).unwrap().flatten().chain(std::fs::read_dir(&lib).unwrap().flatten().filter(|e| e.path().is_dir() && !e.file_name().to_string_lossy().starts_with('.')).flat_map(|d| std::fs::read_dir(d.path()).unwrap().flatten())) {
            let p = entry.path();
            if !p.is_file() || p.extension().is_none_or(|e| e != "mp4") {
                continue;
            }
            let m = std::fs::metadata(&p).unwrap();
            let id = crate::store::read_id(&p);
            // Edited clips always (their edit folders are the interesting
            // part); others only when small, to keep the check quick.
            if id.is_none() && m.len() > 200 << 20 {
                continue;
            }
            let folder = p.parent().filter(|d| *d != lib).and_then(|d| d.file_name()).map(|n| n.to_string_lossy().into_owned());
            clips.push(Clip { id, path: p, name: String::new(), modified: m.modified().unwrap(), created: None, size_bytes: m.len(), original: None, folder });
        }
        assert!(!clips.is_empty(), "no clips found");
        let out = temp("real").join("all.hesteclips");
        let names: Vec<Vec<String>> = clips.iter().map(|_| vec!["Check".to_owned()]).collect();
        let p = progress();
        let n = export(&lib, &clips, &names, &out, &p).unwrap();
        assert_eq!(n, clips.len());
        let into = temp("real-into");
        crate::store::set_library(&into);
        let done = import(&out, &into, &progress()).unwrap();
        assert_eq!(done.added.len(), clips.len());
        for c in &clips {
            let rel = c.path.strip_prefix(&lib).unwrap();
            let got = into.join(rel);
            assert_eq!(std::fs::read(&c.path).unwrap(), std::fs::read(&got).unwrap(), "{} differs", rel.display());
            if let Some(id) = &c.id {
                let (a, b) = (files_in(&crate::store::assets_dir(&lib, id)).len(), files_in(&crate::store::assets_dir(&into, id)).len());
                assert_eq!(a, b, "{id}: the edit folder differs");
            }
        }
        eprintln!("{} clips ({} edited), {} bytes in the file", clips.len(), clips.iter().filter(|c| c.id.is_some()).count(), std::fs::metadata(&out).unwrap().len());
        let _ = std::fs::remove_dir_all(out.parent().unwrap());
        let _ = std::fs::remove_dir_all(&into);
    }

    /// A different clip with a name that's taken comes in as "name (2)".
    #[test]
    fn taken_names_get_a_number() {
        let _turn = crate::store::LIBRARY_TURN.lock().unwrap_or_else(|e| e.into_inner());
        let (a, b) = (temp("c"), temp("d"));
        crate::store::set_library(&a);
        let c = clip(&a, None, "clip.mp4", 100);
        let out = a.join("x.hesteclips");
        export(&a, &[c], &[], &out, &progress()).unwrap();
        std::fs::write(b.join("clip.mp4"), vec![0u8; 42]).unwrap();
        crate::store::set_library(&b);
        let done = import(&out, &b, &progress()).unwrap();
        assert_eq!(done.added, vec![(b.join("clip (2).mp4"), vec![])]);
        let _ = (std::fs::remove_dir_all(&a), std::fs::remove_dir_all(&b));
    }

    /// Names in the file can't put anything outside the library.
    #[test]
    fn names_stay_inside() {
        let dir = Path::new("/lib/.hesteclips/abc");
        assert_eq!(inside(dir, "original.mp4"), Some(dir.join("original.mp4")));
        assert_eq!(inside(dir, "../../etc/passwd"), None);
        assert_eq!(inside(dir, ""), None);
        assert_eq!(safe_name(".."), "");
        assert!(!safe_name("a/b").contains('/'));
    }

    /// A file from a newer app says so instead of half-importing.
    #[test]
    fn newer_files_say_so() {
        let a = temp("e");
        let out = a.join("new.hesteclips");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&out).unwrap());
        zip.start_file("manifest.json", zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(br#"{"format": 99, "app": "HesteClips 9.0", "clips": []}"#).unwrap();
        zip.finish().unwrap();
        let err = import(&out, &a, &progress()).unwrap_err();
        assert!(err.contains("newer HesteClips"), "{err}");
        let _ = std::fs::remove_dir_all(&a);
    }
}
