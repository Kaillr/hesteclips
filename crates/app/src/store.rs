//! Where a clip's extra files live.
//!
//! The library folder holds one plain video per clip, under the clip's name: the
//! clip as it looks now, so Finder, drag-and-drop and sharing all hand over the
//! right file. Everything else about a clip lives in a hidden folder named after
//! its permanent id, which is written into the video's own metadata:
//!
//! ```text
//! Clutch ace.mp4              ← comment "hesteclips:mix=1,2 id=3f9c…"
//! .hesteclips/3f9c…/
//!     original.mp4            ← the untouched recording
//!     edit.json               ← trim + volume, to reopen and change the edit
//! ```
//!
//! Clips in the library's folders (one per game) keep theirs in the same
//! `.hesteclips` at the library's top. Because the id travels inside the file,
//! renaming a clip or moving it to another folder never loses its edit. A clip gets its id the first time it's edited; clips
//! that were never edited are just their file.

use std::path::{Path, PathBuf};

use media::Edit;

use crate::clips::Clip;

/// The hidden folder in the library that holds every clip's assets.
pub const DIR: &str = ".hesteclips";

/// A clip as the editor sees it: the file in the library, the file to decode
/// (its original once it's been edited), and its id if it has one.
#[derive(Debug, Clone)]
pub struct EditTarget {
    pub clip: PathBuf,
    pub source: PathBuf,
    pub id: Option<String>,
    library: PathBuf,
}

impl EditTarget {
    pub fn of(clip: &Path) -> Self {
        let id = read_id(clip);
        let library = library_of(clip);
        let source = id.as_deref().and_then(|id| find_original(&library, id)).unwrap_or_else(|| clip.to_path_buf());
        Self { clip: clip.to_path_buf(), source, id, library }
    }

    pub fn library(&self) -> &Path {
        &self.library
    }
}

/// The library folder, as the app has it (Settings → Clips folder).
static LIBRARY: std::sync::RwLock<Option<PathBuf>> = std::sync::RwLock::new(None);

/// Tell the store where the library is, so clips in its folders find their
/// assets at its top.
pub fn set_library(lib: &Path) {
    if let Ok(mut l) = LIBRARY.write() {
        *l = Some(lib.to_path_buf());
    }
}

/// The library a clip belongs to: the folder above its own when that's the
/// library (a game's folder), else its own folder.
fn library_of(clip: &Path) -> PathBuf {
    let dir = clip.parent().unwrap_or(Path::new("."));
    let lib = LIBRARY.read().ok().and_then(|l| l.clone());
    match lib {
        Some(lib) if dir.parent() == Some(lib.as_path()) => lib,
        _ => dir.to_path_buf(),
    }
}

pub fn assets_dir(lib: &Path, id: &str) -> PathBuf {
    lib.join(DIR).join(id)
}

/// Create a clip's asset folder. On Windows the leading dot doesn't hide
/// `.hesteclips` the way it does on macOS and Linux, so it's marked hidden.
pub fn create_assets_dir(lib: &Path, id: &str) -> std::io::Result<PathBuf> {
    let dir = assets_dir(lib, id);
    std::fs::create_dir_all(&dir)?;
    #[cfg(windows)]
    {
        use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_HIDDEN, GetFileAttributesW, INVALID_FILE_ATTRIBUTES, SetFileAttributesW};
        use windows::core::HSTRING;
        let root = HSTRING::from(lib.join(DIR).as_os_str());
        // SAFETY: a valid path string for both calls.
        unsafe {
            let current = GetFileAttributesW(&root);
            if current != INVALID_FILE_ATTRIBUTES && current & FILE_ATTRIBUTE_HIDDEN.0 == 0 {
                let _ = SetFileAttributesW(&root, windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(current | FILE_ATTRIBUTE_HIDDEN.0));
            }
        }
    }
    Ok(dir)
}

fn edit_path(lib: &Path, id: &str) -> PathBuf {
    assets_dir(lib, id).join("edit.json")
}

/// Where an in-place render is written before it replaces the clip.
pub fn staging_path(lib: &Path, id: &str, ext: &str) -> PathBuf {
    assets_dir(lib, id).join(format!("rendering.{ext}"))
}

/// The edited clip's untouched original (`original.<ext>`), if it has one.
pub fn find_original(lib: &Path, id: &str) -> Option<PathBuf> {
    std::fs::read_dir(assets_dir(lib, id))
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.file_stem().is_some_and(|s| s == "original"))
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// The clip's id from its file comment. Ids name folders we delete, so anything
/// that isn't a plain token is ignored.
pub fn read_id(clip: &Path) -> Option<String> {
    let comment = match capture::mp4meta::read_comment(clip) {
        Some(c) => Some(c),
        // Not MP4/MOV (e.g. older MKV clips): ask ffprobe.
        None if !is_mp4_family(clip) => media::probe(clip).ok().and_then(|i| i.id).map(|id| format!("hesteclips: id={id}")),
        None => None,
    }?;
    media::parse_clip_tag(&comment).id.filter(|id| valid_id(id))
}

fn is_mp4_family(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "mp4" | "mov" | "m4v"))
}

fn valid_id(id: &str) -> bool {
    (8..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

pub fn load_edit(target: &EditTarget) -> Option<Edit> {
    let id = target.id.as_deref()?;
    serde_json::from_slice(&std::fs::read(edit_path(target.library(), id)).ok()?).ok()
}

/// Swap a finished render in as the clip. The first time, the clip itself
/// becomes the original; after that the original stays and only the visible file
/// and the edit change. Each step is a rename, so the clip is never missing.
pub fn commit_render(target: &EditTarget, staged: &Path, id: &str, edit: &Edit) -> std::io::Result<()> {
    let lib = target.library();
    let dir = create_assets_dir(lib, id)?;
    let mut moved_original = None;
    if find_original(lib, id).is_none() {
        let ext = target.clip.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or("mp4".into());
        let original = dir.join(format!("original.{ext}"));
        std::fs::rename(&target.clip, &original)?;
        moved_original = Some(original);
    }
    if let Err(e) = std::fs::rename(staged, &target.clip) {
        if let Some(original) = moved_original {
            let _ = std::fs::rename(original, &target.clip);
        }
        return Err(e);
    }
    std::fs::write(edit_path(lib, id), serde_json::to_vec_pretty(edit).map_err(std::io::Error::other)?)
}

/// Clean up after a render that failed: the clip is left exactly as it was.
pub fn abandon_render(lib: &Path, id: &str, staged: &Path) {
    let _ = std::fs::remove_file(staged);
    // A first edit made the folder just for this render.
    if find_original(lib, id).is_none() {
        let _ = std::fs::remove_dir_all(assets_dir(lib, id));
    }
}

/// Undo the edit: the original goes back in the clip's place.
pub fn revert(target: &EditTarget) -> std::io::Result<()> {
    let Some(id) = target.id.as_deref() else { return Ok(()) };
    let lib = target.library();
    if let Some(original) = find_original(lib, id) {
        let ext = original.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or("mp4".into());
        let back = target.clip.with_extension(ext);
        std::fs::rename(&original, &back)?;
        if back != target.clip {
            let _ = std::fs::remove_file(&target.clip);
        }
    }
    std::fs::remove_dir_all(assets_dir(lib, id))
}

/// Delete a clip, with its assets unless another copy of the clip
/// (duplicated in Finder) still uses them: to the Bin, or for good.
pub fn delete(clip: &Clip, library: &[Clip], permanently: bool) -> Result<(), String> {
    let remove = |path: &Path| -> Result<(), String> {
        if !permanently {
            return trash::delete(path).map_err(|e| e.to_string());
        }
        let r = if path.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) };
        r.map_err(|e| e.to_string())
    };
    remove(&clip.path)?;
    if let Some(id) = clip.id.as_deref() {
        let shared = library.iter().any(|c| c.path != clip.path && c.id.as_deref() == Some(id));
        let dir = assets_dir(&library_of(&clip.path), id);
        if !shared && dir.exists() {
            remove(&dir)?;
        }
    }
    Ok(())
}

/// Edited clips saved before edits kept their clip's date show as made the
/// day they were edited. Their untouched original still has the real date:
/// give it back. Returns how many were fixed.
pub fn restore_edit_dates(clips: &[Clip]) -> usize {
    let mut fixed = 0;
    for clip in clips {
        let Some(original) = &clip.original else { continue };
        let Ok(made) = std::fs::metadata(original).and_then(|m| m.modified()) else { continue };
        let off = clip.modified.duration_since(made).or_else(|e| Ok::<_, ()>(e.duration())).unwrap_or_default();
        if off < std::time::Duration::from_secs(60) {
            continue;
        }
        if std::fs::File::options().write(true).open(&clip.path).and_then(|f| f.set_modified(made)).is_ok() {
            fixed += 1;
        }
    }
    fixed
}

/// What the OS calls the place deleted files go.
pub fn bin_name() -> &'static str {
    if cfg!(windows) { "Recycle Bin" } else { "Trash" }
}

/// Assets whose clip is gone (deleted in Finder) go to the Bin, so they can still
/// be restored but don't use disk space forever. Returns how many.
pub fn sweep_orphans(lib: &Path, clips: &[Clip]) -> usize {
    let Ok(entries) = std::fs::read_dir(lib.join(DIR)) else { return 0 };
    let mut swept = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !entry.path().is_dir() || !valid_id(&name) || clips.iter().any(|c| c.id.as_deref() == Some(name.as_str())) {
            continue;
        }
        if trash::delete(entry.path()).is_ok() {
            swept += 1;
        }
    }
    swept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lib(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hc-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Deleting for good removes the clip and its edit, unless a copy of the
    /// clip still uses the edit.
    #[test]
    fn delete_permanently() {
        let dir = lib("delete");
        let clip = |name: &str, id: &str| {
            let path = dir.join(name);
            std::fs::write(&path, b"x").unwrap();
            Clip { path, name: name.into(), modified: std::time::SystemTime::UNIX_EPOCH, created: None, size_bytes: 1, id: Some(id.into()), original: None, folder: None }
        };
        let (a, b, c) = (clip("a.mp4", "1111"), clip("b.mp4", "1111"), clip("c.mp4", "2222"));
        for id in ["1111", "2222"] {
            std::fs::create_dir_all(assets_dir(&dir, id)).unwrap();
        }
        let library = vec![a.clone(), b.clone(), c.clone()];
        // a's edit is shared with its copy b: kept.
        delete(&a, &library, true).unwrap();
        assert!(!a.path.exists() && assets_dir(&dir, "1111").exists());
        delete(&c, &library, true).unwrap();
        assert!(!c.path.exists() && !assets_dir(&dir, "2222").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// An edited clip dated the day it was edited gets its original's date.
    #[test]
    fn edit_dates_come_back() {
        let dir = lib("dates");
        let (clip_path, original) = (dir.join("a.mp4"), dir.join("original.mp4"));
        std::fs::write(&clip_path, b"edit").unwrap();
        std::fs::write(&original, b"orig").unwrap();
        let made = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_790_000_000);
        std::fs::File::options().write(true).open(&original).unwrap().set_modified(made).unwrap();
        let modified = std::fs::metadata(&clip_path).unwrap().modified().unwrap();
        let clip = Clip { path: clip_path.clone(), name: "a.mp4".into(), modified, created: None, size_bytes: 4, id: None, original: Some(original), folder: None };
        assert_eq!(restore_edit_dates(std::slice::from_ref(&clip)), 1);
        assert_eq!(std::fs::metadata(&clip_path).unwrap().modified().unwrap(), made);
        // Already right: left alone.
        let clip = Clip { modified: made, ..clip };
        assert_eq!(restore_edit_dates(&[clip]), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn edit(end: f64) -> Edit {
        Edit { start: 1.0, end, tracks: Vec::new(), output: Default::default() }
    }

    /// Stand-in for a render: write the staged file the way the app would.
    fn render(target: &EditTarget, id: &str, bytes: &[u8]) -> PathBuf {
        let staged = staging_path(target.library(), id, "mp4");
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::write(&staged, bytes).unwrap();
        staged
    }

    /// The store reads ids from the file, which our stub files don't carry.
    fn with_id(clip: &Path, id: &str) -> EditTarget {
        let library = library_of(clip);
        let source = find_original(&library, id).unwrap_or_else(|| clip.to_path_buf());
        EditTarget { clip: clip.to_path_buf(), source, id: Some(id.into()), library }
    }

    #[test]
    fn edit_reedit_revert() {
        let dir = lib("edit");
        let clip = dir.join("Ace.mp4");
        std::fs::write(&clip, b"original").unwrap();
        let id = "aaaaaaaa1111";

        // First edit: the clip becomes the original, the render takes its place.
        let t = with_id(&clip, id);
        assert_eq!(t.source, clip);
        let staged = render(&t, id, b"render 1");
        commit_render(&t, &staged, id, &edit(5.0)).unwrap();
        assert_eq!(std::fs::read(&clip).unwrap(), b"render 1");
        let original = find_original(&dir, id).unwrap();
        assert_eq!(std::fs::read(&original).unwrap(), b"original");
        assert_eq!(load_edit(&with_id(&clip, id)).unwrap().end, 5.0);

        // Re-edit decodes the original and keeps it.
        let t = with_id(&clip, id);
        assert_eq!(t.source, original);
        let staged = render(&t, id, b"render 2");
        commit_render(&t, &staged, id, &edit(4.0)).unwrap();
        assert_eq!(std::fs::read(&clip).unwrap(), b"render 2");
        assert_eq!(std::fs::read(&original).unwrap(), b"original");
        assert_eq!(load_edit(&t).unwrap().end, 4.0);

        // A failed render leaves everything as it was.
        let staged = render(&t, id, b"half");
        abandon_render(&dir, id, &staged);
        assert_eq!(std::fs::read(&clip).unwrap(), b"render 2");
        assert!(!staged.exists() && original.exists());

        // Renamed in Finder: the id still finds the edit.
        let renamed = dir.join("Renamed.mp4");
        std::fs::rename(&clip, &renamed).unwrap();
        let t = with_id(&renamed, id);
        assert_eq!(t.source, original);
        assert!(load_edit(&t).is_some());

        revert(&t).unwrap();
        assert_eq!(std::fs::read(&renamed).unwrap(), b"original");
        assert!(!assets_dir(&dir, id).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn failed_first_render_leaves_no_folder() {
        let dir = lib("fail");
        let clip = dir.join("Ace.mp4");
        std::fs::write(&clip, b"original").unwrap();
        let id = "bbbbbbbb2222";
        let staged = render(&with_id(&clip, id), id, b"half");
        abandon_render(&dir, id, &staged);
        assert_eq!(std::fs::read(&clip).unwrap(), b"original");
        assert!(!assets_dir(&dir, id).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn game_folders_share_the_library_assets() {
        let dir = lib("folders");
        std::fs::create_dir_all(dir.join("osu!")).unwrap();
        let clip = dir.join("osu!").join("Ace.mp4");
        std::fs::write(&clip, b"original").unwrap();
        let id = "cccccccc3333";
        set_library(&dir);
        let t = with_id(&clip, id);
        assert_eq!(t.library(), dir);
        let staged = render(&t, id, b"render");
        commit_render(&t, &staged, id, &edit(5.0)).unwrap();
        assert!(assets_dir(&dir, id).join("original.mp4").exists());

        // Moved to another game's folder: the edit is still found.
        std::fs::create_dir_all(dir.join("Desktop")).unwrap();
        let moved = dir.join("Desktop").join("Ace.mp4");
        std::fs::rename(&clip, &moved).unwrap();
        let t = with_id(&moved, id);
        assert_eq!(t.source, assets_dir(&dir, id).join("original.mp4"));
        assert!(load_edit(&t).is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_unsafe_ids() {
        assert!(valid_id(&new_id()));
        assert!(!valid_id("../../etc"));
        assert!(!valid_id("short"));
        assert!(!valid_id("has space in it"));
    }
}
