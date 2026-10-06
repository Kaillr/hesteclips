//! The sound played when a clip is saved, so you know it worked without
//! leaving the game.
//!
//! Sounds are decoded once (ffmpeg, so any audio file works) and kept in
//! memory; playing one opens the default output for as long as it lasts.
//! Desktop capture leaves out what HesteClips plays, so as it plays the cue is
//! also handed to the desktop sound source (`capture::sources::own_sound`):
//! like any other app's sound, it's in the clips saved after it.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::settings::{CustomSound, SaveSound};

/// Built-in sounds: id and name.
pub const BUILTIN: [(&str, &str); 3] = [("horse", "Horse"), ("silverfish", "Silverfish"), ("chime", "Chime")];

const HORSE: &[u8] = include_bytes!("../assets/sounds/horse.ogg");
const SILVERFISH: &[u8] = include_bytes!("../assets/sounds/silverfish.ogg");
/// Decoded sounds are stereo f32 at this rate.
const RATE: u32 = 48_000;
/// Longer sounds are cut off: it's a cue, not a song.
const MAX_SECONDS: f64 = 5.0;

type Pcm = Arc<Vec<f32>>;

fn cache() -> &'static Mutex<HashMap<String, Pcm>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Pcm>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// The clip-saved sound, if it's on.
pub fn play_saved(cfg: &SaveSound) {
    if cfg.enabled {
        play(&cfg.sound, cfg.volume);
    }
}

/// Play `sound` (a built-in id or `file:<path>`) at `volume` (0..=1), in the
/// background.
pub fn play(sound: &str, volume: f32) {
    let sound = sound.to_owned();
    std::thread::spawn(move || {
        if let Ok(pcm) = pcm(&sound) {
            if let Err(e) = output(&pcm, gain(volume)) {
                eprintln!("clip sound: {e}");
            }
        }
    });
}

/// Decode `sound` ahead of time so the first save plays without a delay.
pub fn preload(sound: &str) {
    let sound = sound.to_owned();
    std::thread::spawn(move || {
        let _ = pcm(&sound);
    });
}

/// Slider position to gain: squared, so the slider feels even to the ear.
pub fn gain(volume: f32) -> f32 {
    let v = volume.clamp(0.0, 1.0);
    v * v
}

/// Add a sound file: check it plays, then copy it into HesteClips' sounds
/// folder so it keeps working if the original is moved.
pub fn add_custom(from: &Path, existing: &[CustomSound]) -> Result<CustomSound, String> {
    decode(Source::File(from)).map_err(|e| format!("Couldn't read that sound: {e}"))?;
    let dir = sounds_dir().ok_or("No settings folder")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let stem = from.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Sound".into());
    let ext = from.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
    let mut dest = dir.join(format!("{stem}.{ext}"));
    let mut n = 2;
    while dest.exists() {
        dest = dir.join(format!("{stem} ({n}).{ext}"));
        n += 1;
    }
    std::fs::copy(from, &dest).map_err(|e| format!("Couldn't copy the sound: {e}"))?;
    let mut name = stem.clone();
    let mut n = 2;
    while existing.iter().any(|c| c.name == name) {
        name = format!("{stem} ({n})");
        n += 1;
    }
    Ok(CustomSound { name, file: dest })
}

/// Forget an added sound and delete its copy.
pub fn remove_custom(sound: &CustomSound) {
    if sounds_dir().is_some_and(|d| sound.file.starts_with(d)) {
        let _ = std::fs::remove_file(&sound.file);
    }
    if let Ok(mut c) = cache().lock() {
        c.remove(&id_of(sound));
    }
}

pub fn id_of(sound: &CustomSound) -> String {
    format!("file:{}", sound.file.display())
}

fn sounds_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("hesteclips").join("sounds"))
}

enum Source<'a> {
    Bytes(&'a [u8]),
    File(&'a Path),
}

fn pcm(sound: &str) -> Result<Pcm, String> {
    if let Some(p) = cache().lock().ok().and_then(|c| c.get(sound).cloned()) {
        return Ok(p);
    }
    let pcm = Arc::new(match sound {
        "chime" => chime(),
        "horse" => decode(Source::Bytes(HORSE))?,
        "silverfish" => decode(Source::Bytes(SILVERFISH))?,
        s => match s.strip_prefix("file:") {
            Some(path) => decode(Source::File(Path::new(path)))?,
            None => decode(Source::Bytes(HORSE))?, // an id from a newer version: the default
        },
    });
    if let Ok(mut c) = cache().lock() {
        c.insert(sound.to_owned(), pcm.clone());
    }
    Ok(pcm)
}

/// Any audio ffmpeg reads, as stereo f32 at [`RATE`], at most [`MAX_SECONDS`].
fn decode(src: Source) -> Result<Vec<f32>, String> {
    let mut cmd = media::ffmpeg();
    cmd.args(["-hide_banner", "-loglevel", "error", "-i"]);
    match src {
        Source::Bytes(_) => cmd.arg("pipe:0"),
        Source::File(p) => cmd.arg(p),
    };
    let mut child = cmd
        .args(["-vn", "-t", &MAX_SECONDS.to_string(), "-ac", "2", "-ar", &RATE.to_string(), "-f", "f32le", "-"])
        .stdin(if matches!(src, Source::Bytes(_)) { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("ffmpeg: {e}"))?;
    let writer = match src {
        Source::Bytes(b) => {
            let mut stdin = child.stdin.take().ok_or("no ffmpeg input")?;
            let bytes = b.to_vec();
            Some(std::thread::spawn(move || {
                let _ = stdin.write_all(&bytes);
            }))
        }
        Source::File(_) => None,
    };
    let mut out = Vec::new();
    child.stdout.take().ok_or("no ffmpeg output")?.read_to_end(&mut out).map_err(|e| e.to_string())?;
    let _ = child.wait();
    if let Some(w) = writer {
        let _ = w.join();
    }
    if out.len() < 8 {
        return Err("no audio in it".into());
    }
    Ok(out.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect())
}

/// A short two-note chime.
fn chime() -> Vec<f32> {
    let rate = RATE as f32;
    let note = |f: f32, start: f32, len: f32, t: f32| -> f32 {
        let t = t - start;
        if !(0.0..len).contains(&t) {
            return 0.0;
        }
        let attack = (t / 0.005).min(1.0);
        let env = attack * (-t * 7.0).exp();
        env * ((std::f32::consts::TAU * f * t).sin() + 0.25 * (std::f32::consts::TAU * 2.0 * f * t).sin())
    };
    let n = (rate * 0.55) as usize;
    (0..n)
        .flat_map(|i| {
            let t = i as f32 / rate;
            let s = 0.3 * (note(880.0, 0.0, 0.45, t) + note(1318.5, 0.09, 0.46, t));
            [s, s]
        })
        .collect()
}

/// Play `pcm` once on the default output and wait for it to finish.
fn output(pcm: &Pcm, gain: f32) -> Result<(), String> {
    let device = cpal::default_host().default_output_device().ok_or("no audio output device")?;
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    let channels = supported.channels() as usize;
    let out_rate = supported.sample_rate() as f64;
    let config = cpal::StreamConfig { channels: supported.channels(), sample_rate: supported.sample_rate(), buffer_size: cpal::BufferSize::Default };
    let step = RATE as f64 / out_rate;
    let frames = pcm.len() / 2;
    let data = pcm.clone();
    let mut pos = 0.0f64;
    let recorded = capture::sources::own_sound::Playing::start();
    let stream = device
        .build_output_stream::<f32, _, _>(
            config,
            move |out: &mut [f32], _| {
                let from = (pos as usize).min(frames);
                for frame in out.chunks_mut(channels) {
                    let i = pos as usize;
                    let (l, r) = if i < frames { (data[i * 2] * gain, data[i * 2 + 1] * gain) } else { (0.0, 0.0) };
                    match channels {
                        1 => frame[0] = (l + r) * 0.5,
                        _ => {
                            frame[0] = l;
                            frame[1] = r;
                            frame[2..].fill(0.0);
                        }
                    }
                    pos += step;
                }
                let to = (pos as usize).min(frames);
                if to > from {
                    let played: Vec<f32> = data[from * 2..to * 2].iter().map(|x| x * gain).collect();
                    recorded.push(&played);
                }
            },
            |e| eprintln!("clip sound output: {e}"),
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    std::thread::sleep(Duration::from_secs_f64(frames as f64 / RATE as f64 + 0.25));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_decode() {
        let s = decode(Source::Bytes(SILVERFISH)).unwrap();
        let secs = s.len() as f64 / 2.0 / RATE as f64;
        assert!((0.3..0.5).contains(&secs), "{secs}");
        assert!(s.iter().any(|x| x.abs() > 0.05), "not silent");
        let h = decode(Source::Bytes(HORSE)).unwrap();
        let secs = h.len() as f64 / 2.0 / RATE as f64;
        assert!((1.4..1.7).contains(&secs), "{secs}");
        assert!(h.iter().any(|x| x.abs() > 0.05), "not silent");
        assert!(chime().iter().all(|x| x.abs() <= 1.0));
    }
}
