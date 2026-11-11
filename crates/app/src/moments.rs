//! A clip's moments: the kills in it (as the game reported them while it was
//! recorded) and your own markers, shown as pins on its timeline.
//!
//! Kept in the library's `.hesteclips/moments.json`, by the clip's path inside
//! the library, like collections: renaming or moving a clip in the app takes
//! its moments along. Kills are kept as how long before the clip's end they
//! were (that's what's known when it's saved); markers as where they are.
//! An edited clip's moments are its original's: trimming moves them with it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Moments {
    /// Kills: seconds before the end of the recording, and what it was.
    #[serde(default)]
    pub kills: Vec<(f64, String)>,
    /// Your markers: seconds into the recording.
    #[serde(default)]
    pub markers: Vec<f64>,
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    clips: HashMap<String, Moments>,
}

/// One write at a time: the capture thread keeps a saved clip's kills while
/// the app may be adding a marker.
static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn file_path(lib: &Path) -> PathBuf {
    lib.join(crate::store::DIR).join("moments.json")
}

fn load(lib: &Path) -> File {
    std::fs::read(file_path(lib)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save(lib: &Path, file: &File) {
    let path = file_path(lib);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_vec(file) {
        // Written aside, then swapped in: never half a file.
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// The clip's library and its key there.
fn place(clip: &Path) -> Option<(PathBuf, String)> {
    let lib = crate::store::library_of(clip);
    let rel = clip.strip_prefix(&lib).ok()?;
    Some((lib, rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")))
}

fn change(clip: &Path, f: impl FnOnce(&mut HashMap<String, Moments>, &str)) {
    let Some((lib, key)) = place(clip) else { return };
    let _lock = LOCK.lock();
    let mut file = load(&lib);
    f(&mut file.clips, &key);
    save(&lib, &file);
}

/// A clip just saved: the kills in it, each how long before its end, and
/// the markers put in while it was recorded (seconds into it).
pub fn keep(clip: &Path, kills: &[(Duration, String)], markers: &[f64]) {
    let kills: Vec<(f64, String)> = kills.iter().map(|(ago, what)| (ago.as_secs_f64(), what.clone())).collect();
    change(clip, |clips, key| {
        let m = clips.entry(key.to_owned()).or_default();
        m.kills = kills;
        m.markers = markers.to_vec();
    });
}

/// Moments marked while recording, each how long before the end, as markers
/// on a recording `length` seconds long.
pub fn marks_at(length: f64, marks: &[Duration]) -> Vec<f64> {
    let mut at: Vec<f64> = marks.iter().map(|ago| length - ago.as_secs_f64()).filter(|t| (0.0..=length).contains(t)).collect();
    at.sort_by(f64::total_cmp);
    at
}

/// The clip's moments (none kept: empty).
pub fn of(clip: &Path) -> Moments {
    let Some((lib, key)) = place(clip) else { return Moments::default() };
    load(&lib).clips.remove(&key).unwrap_or_default()
}

/// Add a marker at `t` seconds into the recording.
pub fn add_marker(clip: &Path, t: f64) {
    change(clip, |clips, key| {
        let m = clips.entry(key.to_owned()).or_default();
        m.markers.push(t);
        m.markers.sort_by(f64::total_cmp);
    });
}

/// Remove the marker nearest `t` (within a frame or so).
pub fn remove_marker(clip: &Path, t: f64) {
    change(clip, |clips, key| {
        if let Some(m) = clips.get_mut(key) {
            m.markers.retain(|x| (x - t).abs() > 0.02);
            if m.kills.is_empty() && m.markers.is_empty() {
                clips.remove(key);
            }
        }
    });
}

/// The clip's recording is now these `parts` of what it was (in order, as
/// `media::Edit::kept` has them): its original trimmed, or the edit became
/// the recording, or a new clip saved from part of it, at `to`. `was`: how
/// long it was. Its moments move with it; those cut away are left out.
pub fn rebase(from: &Path, to: &Path, was: f64, parts: &[(f64, f64)]) {
    let m = of(from);
    if m.kills.is_empty() && m.markers.is_empty() {
        return;
    }
    let length: f64 = parts.iter().map(|(a, b)| b - a).sum();
    // Kills are kept from the end: where they are, then from the new end.
    let kills: Vec<(f64, String)> = m.kills.iter().filter_map(|(ago, what)| Some((length - media::output_time(parts, was - ago)?, what.clone()))).collect();
    let markers: Vec<f64> = m.markers.iter().filter_map(|&t| media::output_time(parts, t)).collect();
    change(to, |clips, key| {
        clips.insert(key.to_owned(), Moments { kills, markers });
    });
}

/// A clip was renamed or moved: its moments go with it.
pub fn follow(from: &Path, to: &Path) {
    let (Some((lib, from)), Some((lib_to, to))) = (place(from), place(to)) else { return };
    if lib != lib_to {
        return;
    }
    let _lock = LOCK.lock();
    let mut file = load(&lib);
    if let Some(m) = file.clips.remove(&from) {
        file.clips.insert(to, m);
        save(&lib, &file);
    }
}

/// Put a clip's moments back (Undo after deleting it).
pub fn put(clip: &Path, m: Moments) {
    if m.kills.is_empty() && m.markers.is_empty() {
        return;
    }
    change(clip, |clips, key| {
        clips.insert(key.to_owned(), m);
    });
}

/// Clips that were deleted take their moments with them.
pub fn forget(clips: &[PathBuf]) {
    for clip in clips {
        let Some((lib, key)) = place(clip) else { continue };
        let _lock = LOCK.lock();
        let mut file = load(&lib);
        if file.clips.remove(&key).is_some() {
            save(&lib, &file);
        }
    }
}

impl Moments {
    /// As pins on a recording `duration` seconds long, in order.
    pub fn pins(&self, duration: f64) -> Vec<crate::transport::Moment> {
        let mut pins: Vec<crate::transport::Moment> = self
            .kills
            .iter()
            .map(|(ago, what)| (duration - ago, what))
            .filter(|(t, _)| (0.0..=duration).contains(t))
            .map(|(t, what)| crate::transport::Moment { t, label: format!("{what} at {}", crate::transport::clock(t)), marker: false })
            .chain(self.markers.iter().filter(|t| (0.0..=duration).contains(*t)).map(|&t| crate::transport::Moment {
                t,
                label: format!("Your marker at {}", crate::transport::clock(t)),
                marker: true,
            }))
            .collect();
        pins.sort_by(|a, b| a.t.total_cmp(&b.t));
        pins
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_land_before_the_end() {
        let s = Duration::from_secs_f64;
        // Pressed 10 s and 70.5 s before the end of a 600 s recording; one
        // from before it started is left out.
        assert_eq!(marks_at(600.0, &[s(10.0), s(70.5), s(601.0)]), vec![529.5, 590.0]);
    }

    #[test]
    fn kills_land_before_the_end() {
        let m = Moments { kills: vec![(5.0, "Headshot".into()), (90.0, "Kill".into())], markers: vec![2.0] };
        let pins = m.pins(60.0);
        // The kill from before the clip started isn't in it.
        assert_eq!(pins.len(), 2);
        assert_eq!(pins[0].t, 2.0);
        assert!(pins[0].marker);
        assert_eq!(pins[1].t, 55.0);
        assert!(pins[1].label.starts_with("Headshot at"));
    }

    #[test]
    fn rebase_keeps_what_is_left() {
        let lib = std::env::temp_dir().join(format!("hc-moments-rebase-{}", std::process::id()));
        std::fs::create_dir_all(&lib).unwrap();
        let (a, b) = (lib.join("a.mp4"), lib.join("b.mp4"));
        // A 60 s recording: kills at 10 s and 50 s, a marker at 30 s.
        keep(&a, &[(Duration::from_secs(50), "Kill".into()), (Duration::from_secs(10), "Ace".into())], &[30.0]);
        // Saved as a new clip of 20..45 s.
        rebase(&a, &b, 60.0, &[(20.0, 45.0)]);
        let pins = of(&b).pins(25.0);
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0].t, 10.0);
        assert!(pins[0].marker);
        // Of 5..35 and 40..60 s: the kill at 10 s and the marker in the
        // first part, the kill at 50 s in the second.
        rebase(&a, &b, 60.0, &[(5.0, 35.0), (40.0, 60.0)]);
        let ts: Vec<f64> = of(&b).pins(50.0).iter().map(|m| m.t).collect();
        assert_eq!(ts, [5.0, 25.0, 40.0]);
        // A moment in what's cut out is left out.
        rebase(&a, &b, 60.0, &[(0.0, 20.0), (40.0, 60.0)]);
        let ts: Vec<f64> = of(&b).pins(40.0).iter().map(|m| m.t).collect();
        assert_eq!(ts, [10.0, 30.0]);
        // The first 5 s cut off the original itself.
        rebase(&a, &a, 60.0, &[(5.0, 60.0)]);
        let ts: Vec<f64> = of(&a).pins(55.0).iter().map(|m| m.t).collect();
        assert_eq!(ts, [5.0, 25.0, 45.0]);
        let _ = std::fs::remove_dir_all(&lib);
    }

    #[test]
    fn follow_renames() {
        let lib = std::env::temp_dir().join(format!("hc-moments-{}", std::process::id()));
        std::fs::create_dir_all(&lib).unwrap();
        let (a, b) = (lib.join("a.mp4"), lib.join("b.mp4"));
        keep(&a, &[(Duration::from_secs(3), "Kill".into())], &[]);
        add_marker(&a, 1.5);
        follow(&a, &b);
        assert_eq!(of(&a), Moments::default());
        assert_eq!(of(&b).kills.len(), 1);
        remove_marker(&b, 1.5);
        assert!(of(&b).markers.is_empty());
        forget(&[b.clone()]);
        assert_eq!(of(&b), Moments::default());
        let _ = std::fs::remove_dir_all(&lib);
    }
}
