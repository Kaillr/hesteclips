//! Short snippets of the microphone, kept (when the user opts in) to make the
//! "hashtag HesteClip that" detector better: what it fired on, what it almost
//! fired on, and what was heard around a clip saved by hand.
//!
//! Nothing here is sent anywhere. The snippets sit in a folder on this
//! computer until the user has listened to them, said which ones should have
//! saved a clip, and (if they want to help) exported the ones they're happy to
//! share as one zip file to send over themselves.
//!
//! A snippet is `<id>.wav` (16 kHz mono) with `<id>.json` beside it: what
//! caused it, the user's verdict, and where they trimmed it.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Sound rate of the snippets: what the detector hears.
pub const RATE: u32 = 16_000;
/// Seconds kept before the moment that caused a snippet, and after it.
pub const BEFORE: f64 = 5.0;
pub const AFTER: f64 = 1.5;
/// A snippet made by saving a clip by hand holds this much leading up to it.
pub const BY_HAND: f64 = 8.0;
/// Snippets nobody has listened to are deleted after this many days, and
/// only this many are kept (the newest). Guesses.
const KEEP_DAYS: i64 = 14;
const KEEP_UNREVIEWED: usize = 300;

/// What made the detector keep a snippet.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// It fired: a clip was saved.
    Fired,
    /// It came close to firing.
    Almost,
    /// A clip was saved with the shortcut or the button: the user may have
    /// said the phrase first and not been heard.
    ByHand,
}

impl Kind {
    pub fn describe(self) -> &'static str {
        match self {
            Kind::Fired => "Saved a clip by voice",
            Kind::Almost => "Nearly saved a clip by voice",
            Kind::ByHand => "You saved a clip by hand",
        }
    }
}

/// The user's verdict on what's in the snippet.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub enum Label {
    #[default]
    Unreviewed,
    /// The trimmed part should save a clip: "hashtag HesteClip that", said
    /// well enough that the user wants it to count. ("phrase" in files made
    /// before it was asked as "should it save a clip".)
    #[serde(alias = "phrase")]
    Trigger,
    /// It wasn't (inside the trimmed part).
    #[serde(alias = "other")]
    Ignore,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Meta {
    pub id: String,
    pub kind: Kind,
    /// Unix seconds.
    pub at: i64,
    /// How sure the detector was, at its surest.
    pub score: f32,
    /// Where in the snippet the moment was (s), if it has one.
    #[serde(default)]
    pub moment: Option<f32>,
    #[serde(default)]
    pub label: Label,
    /// The part (seconds from the start) the verdict and any sharing are
    /// about; the whole snippet when not set.
    #[serde(default)]
    pub trim: Option<(f32, f32)>,
    #[serde(default)]
    pub version: String,
}

#[derive(Clone, Debug)]
pub struct Sample {
    pub meta: Meta,
    pub secs: f32,
}

impl Sample {
    /// The part that counts: trimmed, or all of it.
    pub fn range(&self) -> (f32, f32) {
        let (a, b) = self.meta.trim.unwrap_or((0.0, self.secs));
        (a.clamp(0.0, self.secs), b.clamp(0.0, self.secs).max(a))
    }

    /// Shared if exported: judged.
    pub fn shareable(&self) -> bool {
        self.meta.label != Label::Unreviewed
    }
}

/// Where the tests keep snippets.
#[cfg(test)]
pub static TEST_DIR: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

pub fn dir() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(d) = TEST_DIR.lock().unwrap().clone() {
        return Some(d);
    }
    dirs::data_local_dir().map(|d| d.join("hasteclip").join("voice-samples"))
}

fn paths(id: &str) -> Option<(PathBuf, PathBuf)> {
    let d = dir()?;
    Some((d.join(format!("{id}.wav")), d.join(format!("{id}.json"))))
}

/// Keep `samples` (mono, [`RATE`]) as a new snippet.
pub fn save_new(kind: Kind, score: f32, moment: Option<f32>, samples: &[f32]) {
    let result = (|| -> Result<(), String> {
        let d = dir().ok_or("no data folder")?;
        std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
        let now = chrono::Local::now();
        let id = format!("{}-{}", now.format("%Y%m%d-%H%M%S"), &uuid::Uuid::new_v4().simple().to_string()[..4]);
        let meta = Meta {
            id: id.clone(),
            kind,
            at: now.timestamp(),
            score,
            moment,
            label: Label::Unreviewed,
            trim: None,
            version: env!("CARGO_PKG_VERSION").to_owned(),
        };
        let (wav, json) = paths(&id).ok_or("no data folder")?;
        std::fs::write(&wav, wav_bytes(samples)).map_err(|e| e.to_string())?;
        std::fs::write(&json, serde_json::to_vec_pretty(&meta).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            eprintln!("voice: kept a {kind:?} snippet (score {score:.2}) to review");
            prune();
        }
        Err(e) => eprintln!("voice: couldn't keep a snippet: {e}"),
    }
}

/// Everything kept, newest first.
pub fn list() -> Vec<Sample> {
    let Some(d) = dir() else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(&d) else { return Vec::new() };
    let mut out: Vec<Sample> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| {
            let meta: Meta = serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok()?;
            let bytes = std::fs::metadata(e.path().with_extension("wav")).ok()?.len();
            Some(Sample { secs: bytes.saturating_sub(44) as f32 / 2.0 / RATE as f32, meta })
        })
        .collect();
    out.sort_by(|a, b| b.meta.at.cmp(&a.meta.at).then_with(|| b.meta.id.cmp(&a.meta.id)));
    out
}

pub fn load_audio(id: &str) -> Option<Vec<f32>> {
    let (wav, _) = paths(id)?;
    let bytes = std::fs::read(wav).ok()?;
    Some(bytes.get(44..)?.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0).collect())
}

pub fn save_meta(meta: &Meta) {
    if let Some((_, json)) = paths(&meta.id)
        && let Ok(bytes) = serde_json::to_vec_pretty(meta)
        && let Err(e) = std::fs::write(json, bytes)
    {
        eprintln!("voice: couldn't save a verdict: {e}");
    }
}

pub fn delete(id: &str) {
    if let Some((wav, json)) = paths(id) {
        let _ = std::fs::remove_file(wav);
        let _ = std::fs::remove_file(json);
    }
}

pub fn delete_all() {
    for s in list() {
        delete(&s.meta.id);
    }
}

/// Old unreviewed snippets go, and so do the oldest past the limit.
fn prune() {
    let cutoff = chrono::Utc::now().timestamp() - KEEP_DAYS * 86_400;
    let mut kept = 0;
    for s in list().into_iter().filter(|s| s.meta.label == Label::Unreviewed) {
        kept += 1;
        if s.meta.at < cutoff || kept > KEEP_UNREVIEWED {
            delete(&s.meta.id);
        }
    }
}

/// A random code for this installation, so samples from one person can be
/// told from another's (and kept apart when testing) without a name.
fn contributor() -> String {
    let file = dir().map(|d| d.join("contributor.txt"));
    if let Some(f) = &file
        && let Ok(s) = std::fs::read_to_string(f)
        && !s.trim().is_empty()
    {
        return s.trim().to_owned();
    }
    let code = uuid::Uuid::new_v4().simple().to_string()[..8].to_owned();
    if let Some(f) = &file {
        let _ = std::fs::write(f, &code);
    }
    code
}

#[derive(Serialize)]
struct ExportedSample<'a> {
    file: String,
    /// "trigger" (should save a clip) or "ignore".
    label: Label,
    kind: Kind,
    score: f32,
    secs: f32,
    version: &'a str,
}

#[derive(Serialize)]
struct ExportedIndex<'a> {
    contributor: String,
    exported: String,
    samples: Vec<ExportedSample<'a>>,
}

/// Write the shareable snippets (just their trimmed parts) and what they are
/// to a zip file. How many were included.
pub fn export(to: &Path, samples: &[Sample]) -> Result<usize, String> {
    use zip::write::SimpleFileOptions;
    let shareable: Vec<&Sample> = samples.iter().filter(|s| s.shareable()).collect();
    if shareable.is_empty() {
        return Err("There is nothing to share yet: say for some samples whether they should save a clip.".into());
    }
    let file = std::fs::File::create(to).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let mut index = ExportedIndex { contributor: contributor(), exported: chrono::Local::now().format("%Y-%m-%d").to_string(), samples: Vec::new() };
    for s in &shareable {
        let audio = load_audio(&s.meta.id).ok_or_else(|| format!("Couldn't read sample {}", s.meta.id))?;
        let (a, b) = s.range();
        let (from, to) = ((a * RATE as f32) as usize, ((b * RATE as f32) as usize).min(audio.len()));
        if to <= from {
            continue;
        }
        let name = format!("samples/{}.wav", s.meta.id);
        zip.start_file(&name, options).map_err(|e| e.to_string())?;
        zip.write_all(&wav_bytes(&audio[from..to])).map_err(|e| e.to_string())?;
        index.samples.push(ExportedSample { file: name, label: s.meta.label, kind: s.meta.kind, score: s.meta.score, secs: (to - from) as f32 / RATE as f32, version: &s.meta.version });
    }
    zip.start_file("samples.json", options).map_err(|e| e.to_string())?;
    zip.write_all(&serde_json::to_vec_pretty(&index).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    zip.start_file("README.txt", options).map_err(|e| e.to_string())?;
    zip.write_all(README.as_bytes()).map_err(|e| e.to_string())?;
    zip.finish().map_err(|e| e.to_string())?;
    Ok(index.samples.len())
}

const README: &str = "Voice samples from HasteClip, for improving the \"hashtag HesteClip that\" detector.\n\
\n\
samples/*.wav   16 kHz mono sound, only the part the person trimmed it to\n\
samples.json    for each: label (trigger = it should save a clip, ignore = it shouldn't),\n\
                what made it get kept (fired, almost, by_hand), the detector's score\n\
\n\
Nothing else is included: no name, no clips, no other audio.\n";

/// 16-bit mono WAV at [`RATE`].
fn wav_bytes(samples: &[f32]) -> Vec<u8> {
    let data: Vec<u8> = samples.iter().flat_map(|s| ((s.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes()).collect();
    let mut out = Vec::with_capacity(44 + data.len());
    out.extend(b"RIFF");
    out.extend((36 + data.len() as u32).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(16u32.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(RATE.to_le_bytes());
    out.extend((RATE * 2).to_le_bytes());
    out.extend(2u16.to_le_bytes());
    out.extend(16u16.to_le_bytes());
    out.extend(b"data");
    out.extend((data.len() as u32).to_le_bytes());
    out.extend(data);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_round_trips() {
        let samples: Vec<f32> = (0..1000).map(|i| (i as f32 / 1000.0 * 6.28).sin() * 0.5).collect();
        let bytes = wav_bytes(&samples);
        let back: Vec<f32> = bytes[44..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0).collect();
        assert_eq!(back.len(), samples.len());
        assert!(samples.iter().zip(&back).all(|(a, b)| (a - b).abs() < 1e-3));
    }

    #[test]
    fn trim_is_kept_inside_the_sample() {
        let meta = Meta { id: "x".into(), kind: Kind::Almost, at: 0, score: 0.5, moment: None, label: Label::Trigger, trim: Some((-1.0, 99.0)), version: String::new() };
        let s = Sample { meta, secs: 6.0 };
        assert_eq!(s.range(), (0.0, 6.0));
        assert!(s.shareable());
    }
}
