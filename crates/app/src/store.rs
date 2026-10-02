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
//! Because the id travels inside the file, renaming or moving a clip in Finder
//! never loses its edit. A clip gets its id the first time it's edited; clips
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
}

impl EditTarget {
    pub fn of(clip: &Path) -> Self {
        let id = read_id(clip);
        let source = id.as_deref().and_then(|id| find_original(library_of(clip), id)).unwrap_or_else(|| clip.to_path_buf());
        Self { clip: clip.to_path_buf(), source, id }
    }

    pub fn library(&self) -> &Path {
        library_of(&self.clip)
    }
}

fn library_of(clip: &Path) -> &Path {
    clip.parent().unwrap_or(Path::new("."))
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

/// Move a clip to the Bin, with its assets unless another copy of the clip
/// (duplicated in Finder) still uses them.
pub fn trash(clip: &Clip, library: &[Clip]) -> Result<(), trash::Error> {
    trash::delete(&clip.path)?;
    if let Some(id) = clip.id.as_deref() {
        let shared = library.iter().any(|c| c.path != clip.path && c.id.as_deref() == Some(id));
        let dir = assets_dir(library_of(&clip.path), id);
        if !shared && dir.exists() {
            trash::delete(dir)?;
        }
    }
    Ok(())
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

    fn edit(end: f64) -> Edit {
        Edit { start: 1.0, end, tracks: Vec::new() }
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
        let lib = clip.parent().unwrap();
        let source = find_original(lib, id).unwrap_or_else(|| clip.to_path_buf());
        EditTarget { clip: clip.to_path_buf(), source, id: Some(id.into()) }
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
    fn rejects_unsafe_ids() {
        assert!(valid_id(&new_id()));
        assert!(!valid_id("../../etc"));
        assert!(!valid_id("short"));
        assert!(!valid_id("has space in it"));
    }
}
