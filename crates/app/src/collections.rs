//! Collections: your own groups of clips, like "Awesome ace clutches".
//!
//! A clip lives in one place on disk (its game's folder), but can be in any
//! number of collections — they're lists, not folders, so adding a clip to one
//! never moves it. They're kept in the library's `.hesteclips/collections.json`,
//! by the clip's path inside the library. Renaming or moving a clip in the app
//! takes its memberships along; one moved to another folder outside the app is
//! found again by its file name.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::clips::Clip;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Collection {
    pub id: String,
    pub name: String,
    /// Clips by path inside the library, with `/` between folders.
    pub clips: Vec<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    collections: Vec<Collection>,
}

/// The collections of one library.
#[derive(Default)]
pub struct Collections {
    lib: PathBuf,
    list: Vec<Collection>,
}

impl Collections {
    pub fn load(lib: &Path) -> Self {
        let list = std::fs::read(file_path(lib))
            .ok()
            .and_then(|b| serde_json::from_slice::<File>(&b).ok())
            .map(|f| f.collections)
            .unwrap_or_default();
        Self { lib: lib.to_path_buf(), list }
    }

    pub fn library(&self) -> &Path {
        &self.lib
    }

    fn save(&self) {
        let path = file_path(&self.lib);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let file = File { collections: self.list.clone() };
        if let Ok(json) = serde_json::to_vec_pretty(&file) {
            // Written aside, then swapped in: never half a file.
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, &path).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }

    pub fn list(&self) -> &[Collection] {
        &self.list
    }

    pub fn get(&self, id: &str) -> Option<&Collection> {
        self.list.iter().find(|c| c.id == id)
    }

    /// A clip's path as collections keep it.
    fn key(&self, clip: &Path) -> Option<String> {
        let rel = clip.strip_prefix(&self.lib).ok()?;
        Some(rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/"))
    }

    pub fn contains(&self, id: &str, clip: &Path) -> bool {
        let Some(key) = self.key(clip) else { return false };
        self.get(id).is_some_and(|c| c.clips.contains(&key))
    }

    /// A new, empty collection; its id. The name is checked by [`check_name`].
    pub fn create(&mut self, name: &str) -> String {
        let id = uuid::Uuid::new_v4().simple().to_string();
        self.list.push(Collection { id: id.clone(), name: name.trim().to_owned(), clips: Vec::new() });
        self.save();
        id
    }

    pub fn rename(&mut self, id: &str, name: &str) {
        if let Some(c) = self.list.iter_mut().find(|c| c.id == id) {
            c.name = name.trim().to_owned();
            self.save();
        }
    }

    /// Delete a collection. Its clips stay in the library.
    pub fn delete(&mut self, id: &str) {
        self.list.retain(|c| c.id != id);
        self.save();
    }

    /// Add clips (those already in it stay once). How many were added.
    pub fn add(&mut self, id: &str, clips: &[PathBuf]) -> usize {
        let keys: Vec<String> = clips.iter().filter_map(|p| self.key(p)).collect();
        let Some(c) = self.list.iter_mut().find(|c| c.id == id) else { return 0 };
        let mut added = 0;
        for key in keys {
            if !c.clips.contains(&key) {
                c.clips.push(key);
                added += 1;
            }
        }
        if added > 0 {
            self.save();
        }
        added
    }

    /// Take clips out of a collection (not out of the library).
    pub fn remove(&mut self, id: &str, clips: &[PathBuf]) {
        let keys: Vec<String> = clips.iter().filter_map(|p| self.key(p)).collect();
        if let Some(c) = self.list.iter_mut().find(|c| c.id == id) {
            c.clips.retain(|k| !keys.contains(k));
            self.save();
        }
    }

    /// A clip was renamed or moved: its memberships go with it.
    pub fn follow(&mut self, from: &Path, to: &Path) {
        let (Some(from), Some(to)) = (self.key(from), self.key(to)) else { return };
        let mut changed = false;
        for c in &mut self.list {
            for k in c.clips.iter_mut().filter(|k| **k == from) {
                *k = to.clone();
                changed = true;
            }
        }
        if changed {
            self.save();
        }
    }

    /// Clips that were deleted leave every collection.
    pub fn forget(&mut self, clips: &[PathBuf]) {
        let keys: Vec<String> = clips.iter().filter_map(|p| self.key(p)).collect();
        let mut changed = false;
        for c in &mut self.list {
            let before = c.clips.len();
            c.clips.retain(|k| !keys.contains(k));
            changed |= c.clips.len() != before;
        }
        if changed {
            self.save();
        }
    }

    /// Find clips that moved without the app (in Explorer, to another folder of
    /// the library) again by their file name, when only one clip has it.
    /// Clips that are just missing are kept: the drive may be away.
    pub fn reconnect(&mut self, library: &[Clip]) {
        let present: std::collections::HashSet<String> = library.iter().filter_map(|c| self.key(&c.path)).collect();
        let mut changed = false;
        for c in &mut self.list {
            for k in c.clips.iter_mut() {
                if present.contains(k) {
                    continue;
                }
                let name = k.rsplit('/').next().unwrap_or(k).to_owned();
                let mut same = library.iter().filter(|clip| clip.name == name);
                if let (Some(found), None) = (same.next(), same.next())
                    && let Ok(rel) = found.path.strip_prefix(&self.lib)
                {
                    *k = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
                    changed = true;
                }
            }
        }
        if changed {
            self.save();
        }
    }
}

fn file_path(lib: &Path) -> PathBuf {
    lib.join(".hesteclips").join("collections.json")
}

/// Check a name typed for a collection (`current`: the one being renamed).
pub fn check_name(cols: &Collections, name: &str, current: Option<&str>) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Give the collection a name.".into());
    }
    if name.chars().count() > 80 {
        return Err("That name is too long.".into());
    }
    if cols.list.iter().any(|c| Some(c.id.as_str()) != current && c.name.eq_ignore_ascii_case(name)) {
        return Err("You already have a collection with that name.".into());
    }
    Ok(name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(lib: &Path, rel: &str) -> Clip {
        let path = lib.join(rel);
        Clip {
            name: path.file_name().unwrap().to_string_lossy().into_owned(),
            path,
            modified: std::time::SystemTime::UNIX_EPOCH, created: None,
            size_bytes: 0,
            id: None,
            original: None,
            folder: None,
        }
    }

    #[test]
    fn add_follow_forget_and_reload() {
        let lib = std::env::temp_dir().join(format!("hc-collections-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&lib);
        let mut cols = Collections::load(&lib);
        let id = cols.create("  Awesome ace clutches ");
        let a = lib.join("Counter-Strike 2").join("a.mp4");
        let b = lib.join("osu!").join("b.mp4");
        assert_eq!(cols.add(&id, &[a.clone(), b.clone()]), 2);
        assert_eq!(cols.add(&id, &[a.clone()]), 0, "a clip is in a collection once");
        assert!(cols.contains(&id, &a));

        // Renamed in the app: it stays in.
        let a2 = lib.join("Counter-Strike 2").join("Ace.mp4");
        cols.follow(&a, &a2);
        assert!(cols.contains(&id, &a2) && !cols.contains(&id, &a));

        // Saved and read back.
        let mut cols = Collections::load(&lib);
        assert_eq!(cols.get(&id).unwrap().name, "Awesome ace clutches");
        assert!(cols.contains(&id, &b));

        // Moved in Explorer to another folder: found by its name.
        cols.reconnect(&[clip(&lib, "Desktop/b.mp4"), clip(&lib, "Counter-Strike 2/Ace.mp4")]);
        assert!(cols.contains(&id, &lib.join("Desktop").join("b.mp4")));

        cols.forget(&[a2.clone()]);
        assert!(!cols.contains(&id, &a2));
        cols.delete(&id);
        assert!(Collections::load(&lib).list().is_empty());
        std::fs::remove_dir_all(&lib).unwrap();
    }

    #[test]
    fn names() {
        let mut cols = Collections::default();
        let id = cols.list.len().to_string();
        cols.list.push(Collection { id: id.clone(), name: "Clutches".into(), clips: Vec::new() });
        assert!(check_name(&cols, "  ", None).is_err());
        assert!(check_name(&cols, "clutches", None).is_err());
        assert_eq!(check_name(&cols, "clutches", Some(&id)).as_deref(), Ok("clutches"));
        assert_eq!(check_name(&cols, " Funny ", None).as_deref(), Ok("Funny"));
    }
}
