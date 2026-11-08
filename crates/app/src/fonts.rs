//! The fonts installed on the computer, for words in the video (a text
//! source): each family and the weights and italics it comes in, found once
//! in the background (its files' names and weights read, not the whole
//! files), and a family's font loaded when words are drawn in it. The app's
//! own typeface, Hanken Grotesk, comes first; it's what's used when a font
//! isn't installed (any more).

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// The app's own typeface's name, as it's listed.
pub(crate) const OWN: &str = "Hanken Grotesk";

/// One font file (or one font in a collection of them).
#[derive(Debug, Clone)]
pub(crate) struct Face {
    pub path: PathBuf,
    /// Which font in a collection (`.ttc`); 0 in a file of one.
    pub index: u32,
    pub weight: u16,
    pub italic: bool,
    /// A variable font's weights, least to most: any of them can be drawn.
    pub weights: Option<(f32, f32)>,
}

/// A font family: its name, and the files it comes in.
#[derive(Debug, Clone)]
pub(crate) struct Family {
    pub name: String,
    pub faces: Vec<Face>,
}

impl Family {
    /// The weights and italics it can be drawn in, lightest first (a
    /// variable font's every hundred in its range).
    pub(crate) fn styles(&self) -> Vec<(u16, bool)> {
        let mut out: Vec<(u16, bool)> = Vec::new();
        for f in &self.faces {
            match f.weights {
                Some((lo, hi)) => {
                    for w in (100..=900).step_by(100) {
                        if (w as f32) >= lo - 0.5 && (w as f32) <= hi + 0.5 {
                            out.push((w, f.italic));
                        }
                    }
                }
                None => out.push((f.weight, f.italic)),
            }
        }
        out.sort_by_key(|&(w, i)| (i, w));
        out.dedup();
        out
    }

    /// The face that draws `weight` and `italic` best, and the weight to
    /// set on it if it's variable.
    fn best(&self, weight: u16, italic: bool) -> Option<(&Face, Option<f32>)> {
        let score = |f: &Face| {
            let off = match f.weights {
                Some((lo, hi)) => (weight as f32).clamp(lo, hi) - weight as f32,
                None => f.weight as f32 - weight as f32,
            };
            (u32::from(f.italic != italic) * 10_000) + off.abs() as u32
        };
        let f = self.faces.iter().min_by_key(|f| score(f))?;
        Some((f, f.weights.map(|(lo, hi)| (weight as f32).clamp(lo, hi))))
    }
}

/// A style's name: its weight's ("Bold"), then Italic.
pub(crate) fn style_name(weight: u16, italic: bool) -> String {
    let w = match weight {
        0..=149 => "Thin",
        150..=249 => "Extra light",
        250..=349 => "Light",
        350..=449 => "Regular",
        450..=549 => "Medium",
        550..=649 => "Semibold",
        650..=749 => "Bold",
        750..=849 => "Extra bold",
        _ => "Black",
    };
    match (italic, w) {
        (true, "Regular") => "Italic".to_owned(),
        (true, w) => format!("{w} italic"),
        (false, w) => w.to_owned(),
    }
}

/// The families found: none until the first look is done (it starts in
/// the background the first time this is asked).
pub(crate) fn families() -> Option<Arc<Vec<Family>>> {
    static FOUND: OnceLock<Mutex<Option<Arc<Vec<Family>>>>> = OnceLock::new();
    let found = FOUND.get_or_init(|| {
        std::thread::spawn(|| {
            let list = Arc::new(scan());
            if let Some(f) = FOUND.get() {
                *f.lock().unwrap() = Some(list);
            }
        });
        Mutex::new(None)
    });
    found.lock().unwrap().clone()
}

/// A family by its name, ignoring case.
pub(crate) fn family(name: &str) -> Option<Family> {
    families()?.iter().find(|f| f.name.eq_ignore_ascii_case(name)).cloned()
}

/// The app's own typeface: one variable font, every weight.
fn own() -> Family {
    Family { name: OWN.to_owned(), faces: vec![Face { path: PathBuf::new(), index: 0, weight: 400, italic: false, weights: Some((100.0, 900.0)) }] }
}

/// A font to draw words with: its bytes, which font in them, and the weight
/// to set on it when it's variable.
pub(crate) struct Loaded {
    pub bytes: Arc<Vec<u8>>,
    pub index: u32,
    pub variation: Option<f32>,
}

/// Family `name`'s font (the app's own for an empty name, one not installed,
/// or before the fonts are found) for `weight` and `italic`.
pub(crate) fn load(name: &str, weight: u16, italic: bool) -> Loaded {
    let own_font = || Loaded { bytes: own_bytes(), index: 0, variation: Some((weight as f32).clamp(100.0, 900.0)) };
    if name.is_empty() || name.eq_ignore_ascii_case(OWN) {
        return own_font();
    }
    let Some(fam) = family(name) else { return own_font() };
    let Some((face, variation)) = fam.best(weight, italic) else { return own_font() };
    match file_bytes(&face.path) {
        Some(bytes) => Loaded { bytes, index: face.index, variation },
        None => own_font(),
    }
}

fn own_bytes() -> Arc<Vec<u8>> {
    static BYTES: OnceLock<Arc<Vec<u8>>> = OnceLock::new();
    BYTES.get_or_init(|| Arc::new(crate::theme::HANKEN.to_vec())).clone()
}

/// A font file's bytes, read once (and kept while the app runs: few are
/// used, and words are drawn again as they're typed).
fn file_bytes(path: &Path) -> Option<Arc<Vec<u8>>> {
    static READ: OnceLock<Mutex<HashMap<PathBuf, Arc<Vec<u8>>>>> = OnceLock::new();
    let read = READ.get_or_init(Default::default);
    if let Some(b) = read.lock().unwrap().get(path) {
        return Some(b.clone());
    }
    let b = Arc::new(std::fs::read(path).ok()?);
    read.lock().unwrap().insert(path.to_owned(), b.clone());
    Some(b)
}

/// Where fonts are installed here.
fn font_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    #[cfg(target_os = "windows")]
    {
        let windir = std::env::var_os("WINDIR").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        out.push(windir.join("Fonts"));
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            out.push(PathBuf::from(local).join(r"Microsoft\Windows\Fonts"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        out.extend(["/System/Library/Fonts", "/Library/Fonts"].map(PathBuf::from));
        if let Some(home) = dirs::home_dir() {
            out.push(home.join("Library/Fonts"));
        }
    }
    #[cfg(target_os = "linux")]
    {
        out.extend(["/usr/share/fonts", "/usr/local/share/fonts"].map(PathBuf::from));
        if let Some(home) = dirs::home_dir() {
            out.push(home.join(".local/share/fonts"));
            out.push(home.join(".fonts"));
        }
    }
    out
}

/// Every font installed, by family, the app's own first, the rest by name.
fn scan() -> Vec<Family> {
    let mut files = Vec::new();
    for d in font_dirs() {
        walk(&d, 0, &mut files);
    }
    let mut by_name: HashMap<String, Family> = HashMap::new();
    for path in files {
        for (name, face) in faces_of(&path) {
            // Hidden system fonts (".SF NS") aren't for picking.
            if name.starts_with('.') || name.is_empty() || name.eq_ignore_ascii_case(OWN) {
                continue;
            }
            by_name.entry(name.to_lowercase()).or_insert_with(|| Family { name: name.clone(), faces: Vec::new() }).faces.push(face);
        }
    }
    let mut list: Vec<Family> = by_name.into_values().collect();
    list.sort_by_key(|f| f.name.to_lowercase());
    list.insert(0, own());
    list
}

/// Font files under `dir` (and its folders, a few deep).
fn walk(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            if depth < 4 {
                walk(&path, depth + 1, out);
            }
            continue;
        }
        let ext = path.extension().and_then(|x| x.to_str()).unwrap_or_default().to_ascii_lowercase();
        if matches!(ext.as_str(), "ttf" | "otf" | "ttc" | "otc") {
            out.push(path);
        }
    }
}

/// Each font in a file: its family's name and what it is. Only the tables
/// that say so are read.
fn faces_of(path: &Path) -> Vec<(String, Face)> {
    let Ok(mut file) = std::fs::File::open(path) else { return Vec::new() };
    let Some(head) = read_at(&mut file, 0, 12) else { return Vec::new() };
    let offsets: Vec<u32> = if &head[..4] == b"ttcf" {
        let n = be32(&head, 8).min(64);
        let Some(table) = read_at(&mut file, 12, n as usize * 4) else { return Vec::new() };
        (0..n as usize).map(|i| be32(&table, i * 4)).collect()
    } else {
        vec![0]
    };
    offsets.into_iter().enumerate().filter_map(|(index, at)| face_at(&mut file, path, index as u32, at)).collect()
}

fn face_at(file: &mut std::fs::File, path: &Path, index: u32, at: u32) -> Option<(String, Face)> {
    let head = read_at(file, at as u64, 12)?;
    let tables = be16(&head, 4) as usize;
    let dir = read_at(file, at as u64 + 12, tables.min(64) * 16)?;
    let find = |tag: &[u8; 4]| (0..tables.min(64)).map(|i| &dir[i * 16..i * 16 + 16]).find(|t| &t[..4] == tag).map(|t| (be32(t, 8), be32(t, 12)));
    let (name_at, name_len) = find(b"name")?;
    let names = read_at(file, name_at as u64, (name_len as usize).min(1 << 20))?;
    let family = family_name(&names)?;
    let (weight, italic) = find(b"OS/2").and_then(|(o, l)| read_at(file, o as u64, (l as usize).min(100))).filter(|t| t.len() >= 64).map_or((400, false), |t| (be16(&t, 4), be16(&t, 62) & 0x201 != 0));
    let weights = find(b"fvar").and_then(|(o, l)| read_at(file, o as u64, (l as usize).min(4096))).and_then(|t| weight_axis(&t));
    Some((family, Face { path: path.to_owned(), index, weight: weight.clamp(1, 1000), italic, weights }))
}

/// The family's name: its typographic family (name 16) if it has one, else
/// its family (name 1); Windows' English one first.
fn family_name(t: &[u8]) -> Option<String> {
    if t.len() < 6 {
        return None;
    }
    let count = be16(t, 2) as usize;
    let strings = be16(t, 4) as usize;
    let mut best: Option<(u32, String)> = None;
    for i in 0..count {
        let r = 6 + i * 12;
        if r + 12 > t.len() {
            break;
        }
        let (platform, encoding, language, id) = (be16(t, r), be16(t, r + 2), be16(t, r + 4), be16(t, r + 6));
        let (len, off) = (be16(t, r + 8) as usize, be16(t, r + 10) as usize);
        if id != 1 && id != 16 {
            continue;
        }
        let Some(bytes) = t.get(strings + off..strings + off + len) else { continue };
        let text = match (platform, encoding) {
            (3, 1 | 10) | (0, _) => String::from_utf16_lossy(&bytes.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect::<Vec<_>>()),
            (1, 0) => bytes.iter().map(|&b| b as char).collect(),
            _ => continue,
        };
        // Name 16 over 1; Windows over the rest; English over the rest.
        let rank = u32::from(id == 16) * 4 + u32::from(platform == 3) * 2 + u32::from(language == 0x409 || platform != 3);
        if best.as_ref().is_none_or(|(r, _)| rank > *r) {
            best = Some((rank, text.trim().to_owned()));
        }
    }
    best.map(|(_, n)| n).filter(|n| !n.is_empty())
}

/// A variable font's weight axis: least to most.
fn weight_axis(t: &[u8]) -> Option<(f32, f32)> {
    if t.len() < 16 {
        return None;
    }
    let (axes_at, count, size) = (be16(t, 4) as usize, be16(t, 8) as usize, be16(t, 10) as usize);
    let fixed = |o: usize| be32(t, o) as i32 as f32 / 65536.0;
    (0..count).map(|i| axes_at + i * size).find(|&a| t.get(a..a + 4) == Some(b"wght") && a + 16 <= t.len()).map(|a| (fixed(a + 4), fixed(a + 12)))
}

fn read_at(file: &mut std::fs::File, at: u64, len: usize) -> Option<Vec<u8>> {
    file.seek(SeekFrom::Start(at)).ok()?;
    let mut buf = vec![0u8; len];
    file.read_exact(&mut buf).ok()?;
    Some(buf)
}

fn be16(b: &[u8], at: usize) -> u16 {
    b.get(at..at + 2).map_or(0, |s| u16::from_be_bytes([s[0], s[1]]))
}

fn be32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4).map_or(0, |s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The app's own font reads as itself: Hanken Grotesk, variable from
    /// thin to black.
    #[test]
    fn reads_a_fonts_name_and_weights() {
        let dir = std::env::temp_dir().join("hc-fonts-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("hanken.ttf");
        std::fs::write(&path, crate::theme::HANKEN).unwrap();
        let faces = faces_of(&path);
        assert_eq!(faces.len(), 1);
        let (name, face) = &faces[0];
        assert_eq!(name, "Hanken Grotesk");
        assert!(!face.italic);
        let (lo, hi) = face.weights.expect("a variable font");
        assert!(lo <= 100.0 && hi >= 900.0, "{lo}..{hi}");
        let fam = Family { name: name.clone(), faces: vec![face.clone()] };
        assert!(fam.styles().contains(&(700, false)));
    }

    #[test]
    fn style_names() {
        assert_eq!(style_name(400, false), "Regular");
        assert_eq!(style_name(700, true), "Bold italic");
        assert_eq!(style_name(400, true), "Italic");
    }
}
