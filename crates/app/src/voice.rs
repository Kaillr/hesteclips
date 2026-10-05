//! "Hashtag HesteClip that": say it and a clip is saved, like pressing the
//! save shortcut.
//!
//! A small keyword spotter (sherpa-onnx, with a 5 MB English model built into
//! the app) listens to the microphone on the Sources page, on this PC, only
//! while the replay buffer runs and the setting is on. It can only hear the
//! words it's given, so talking doesn't save clips.
//!
//! "HesteClip" isn't English, and said quickly no one spelling of the phrase
//! comes through whole, so it's heard in three parts — "hashtag", "heste",
//! "clip that" — and any two of them within [`WITHIN`] of each other count.
//! One part alone ("hashtag blessed", "has the clip that we made") does
//! nothing. Tuned on 30 takes of the user saying it (fast, slow, in
//! sentences): the two-part version caught 5 of them (and saved some twice),
//! this one 13, once each, with no near miss triggering it. The small model
//! still misses some fast takes.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};

use capture::mixer::LiveAudio;

use crate::service::QuickSave;

pub struct Voice {
    /// The microphone source to listen to (its id), or none.
    listen: Sender<Option<String>>,
    /// Clips saved by voice (the flag: whether it was).
    heard: Receiver<bool>,
    wanted: Option<String>,
    /// Why it can't listen, if it can't.
    pub problem: Arc<Mutex<Option<String>>>,
}

impl Voice {
    pub fn new(quick: QuickSave, live: Arc<LiveAudio>, ctx: egui::Context) -> Self {
        let (listen, rx) = channel();
        let (heard_tx, heard) = channel();
        let problem: Arc<Mutex<Option<String>>> = Default::default();
        #[cfg(windows)]
        {
            let problem = problem.clone();
            std::thread::Builder::new()
                .name("voice".into())
                .spawn(move || spotting::run(rx, heard_tx, quick, live, ctx, problem))
                .expect("spawn voice thread");
        }
        #[cfg(not(windows))]
        {
            let _ = (rx, heard_tx, quick, live, ctx);
            *problem.lock().unwrap() = Some("only on Windows for now".into());
        }
        Self { listen, heard, wanted: None, problem }
    }

    /// Listen to this microphone source, or not (cheap to call every frame).
    pub fn listen(&mut self, mic: Option<&str>) {
        if mic != self.wanted.as_deref() {
            self.wanted = mic.map(str::to_owned);
            let _ = self.listen.send(self.wanted.clone());
        }
    }

    /// Clips saved by voice since the last call.
    pub fn heard(&self) -> usize {
        self.heard.try_iter().filter(|saved| *saved).count()
    }
}

#[cfg(windows)]
mod spotting {
    use std::path::PathBuf;
    use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use capture::mixer::{LiveAudio, RATE};
    use sherpa_onnx::{KeywordSpotter, KeywordSpotterConfig, OnlineStream};

    use crate::service::QuickSave;

    /// The model, built into the app (`assets/kws`, see its README).
    const MODEL: [(&str, &[u8]); 4] = [
        ("encoder.int8.onnx", include_bytes!("../assets/kws/encoder.int8.onnx")),
        ("decoder.int8.onnx", include_bytes!("../assets/kws/decoder.int8.onnx")),
        ("joiner.int8.onnx", include_bytes!("../assets/kws/joiner.int8.onnx")),
        ("tokens.txt", include_bytes!("../assets/kws/tokens.txt")),
    ];

    /// The phrase's parts, as the model's word pieces, each spelled the ways
    /// it comes out. Found on the user's takes; more spellings for "hashtag"
    /// made it worse (short ones like "hash" fired on part of the word and
    /// the whole one was lost), and ones that fired on near misses ("clip at"
    /// in "clip it later") are left out.
    const PARTS: [&str; 3] = [
        // "Hashtag"
        "▁HAS H TA G @hashtag\n▁HAS H ▁TA G @hashtag\n▁HE SH TA G @hashtag\n▁HAS ▁TA G @hashtag\n▁HAS TA G @hashtag",
        // "Heste"
        "▁HE S TE @heste\n▁HE S TA @heste\n▁HAS TE @heste\n▁HE S TER @heste\n▁HE S TY @heste\n▁HE S T @heste\n▁HE S TI @heste\n▁HE S SE @heste\n▁HE S T EN @heste\n▁E S TE @heste",
        // "Clip that" ("clips": how the model heard it said quickly)
        "▁C LI P ▁THAT @clip_that\n▁K LI PP ▁THAT @clip_that\n▁C LI P ▁DA T @clip_that\n▁K LI PP ▁DA T @clip_that\n▁C LI PP ▁THAT @clip_that\n▁C LI P ▁THE T @clip_that\n▁C LI P S @clip_that",
    ];
    /// Two parts count as the phrase this close together, in either order
    /// (the model sometimes reports a part late; in the user's takes the
    /// parts came 0.3–1.2 s apart: a guess with room for saying it slowly).
    const WITHIN: f64 = 2.5;
    /// After a clip is saved, parts heard this long after are ignored: the
    /// take's late third part could pair with something and save it twice
    /// (0.7 and 1.0 s after, in the user's session), while the next take can
    /// come 1.6 s later (2.0 swallowed real takes).
    const QUIET_AFTER: f64 = 1.2;
    /// How readily a part is taken (sherpa-onnx's boosting score and trigger
    /// threshold): the best of a sweep over 30 takes of the user (1.5/0.2
    /// caught 10, 2.0/0.15 15, 2.5/0.1 14), with no near miss firing two parts.
    const SCORE: f32 = 2.0;
    const THRESHOLD: f32 = 0.15;

    pub(super) fn run(
        rx: Receiver<Option<String>>,
        heard: Sender<bool>,
        quick: QuickSave,
        live: Arc<LiveAudio>,
        ctx: egui::Context,
        problem: Arc<Mutex<Option<String>>>,
    ) {
        let mut spotters: Option<Spotters> = None;
        let mut listening: Option<Listening> = None;
        loop {
            let wanted = match (&mut listening, &spotters) {
                // Listening: take sound until told otherwise.
                (Some(l), Some(s)) => match rx.try_recv() {
                    Ok(w) => Some(w),
                    Err(TryRecvError::Empty) => {
                        if l.step(s) {
                            let _ = heard.send(quick.save());
                            ctx.request_repaint();
                        }
                        None
                    }
                    Err(TryRecvError::Disconnected) => return,
                },
                _ => match rx.recv_timeout(Duration::from_secs(60)) {
                    Ok(w) => Some(w),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                },
            };
            let Some(wanted) = wanted else { continue };
            if let Some(l) = listening.take() {
                live.channel(&l.mic).set_tap(None);
            }
            let Some(mic) = wanted else { continue };
            if spotters.is_none() {
                match Spotters::load() {
                    Ok(s) => {
                        spotters = Some(s);
                        *problem.lock().unwrap() = None;
                    }
                    Err(e) => {
                        eprintln!("voice: can't listen: {e}");
                        *problem.lock().unwrap() = Some(e);
                        continue;
                    }
                }
            }
            let (tx, sound) = channel();
            live.channel(&mic).set_tap(Some(tx));
            listening = Some(Listening::new(spotters.as_ref().expect("just made"), mic, sound));
        }
    }

    /// One spotter per part of the phrase. A stream's own keywords are added
    /// to its spotter's list rather than replacing it (found by test: the
    /// "clip that" stream heard "hashtag"), so each part gets a spotter of
    /// its own.
    struct Spotters {
        parts: Vec<KeywordSpotter>,
    }

    impl Spotters {
        /// The model, unpacked into the cache folder (the engine reads files),
        /// and the spotters made from it.
        fn load() -> Result<Self, String> {
            let dir = dirs::cache_dir().ok_or("no cache folder")?.join("hesteclips").join("kws-1");
            std::fs::create_dir_all(&dir).map_err(|e| format!("can't unpack the voice model: {e}"))?;
            let path = |name: &str| -> PathBuf { dir.join(name) };
            for (name, bytes) in MODEL {
                let p = path(name);
                if std::fs::metadata(&p).map(|m| m.len()).ok() != Some(bytes.len() as u64) {
                    std::fs::write(&p, bytes).map_err(|e| format!("can't unpack the voice model: {e}"))?;
                }
            }
            let started = std::time::Instant::now();
            let spotter = |keywords: &str| {
                let text = |name: &str| Some(path(name).to_string_lossy().into_owned());
                let mut config = KeywordSpotterConfig::default();
                config.model_config.transducer.encoder = text("encoder.int8.onnx");
                config.model_config.transducer.decoder = text("decoder.int8.onnx");
                config.model_config.transducer.joiner = text("joiner.int8.onnx");
                config.model_config.tokens = text("tokens.txt");
                // One thread: it keeps up ~30× faster than speech (measured),
                // and a game wants the rest.
                config.model_config.num_threads = 1;
                config.keywords_score = SCORE;
                config.keywords_threshold = THRESHOLD;
                config.keywords_buf = Some(keywords.to_owned());
                KeywordSpotter::create(&config).ok_or_else(|| "the voice model didn't load".to_owned())
            };
            let spotters = Self { parts: PARTS.iter().map(|k| spotter(k)).collect::<Result<_, _>>()? };
            eprintln!("voice: listening for \"hashtag HesteClip that\" (model loaded in {} ms)", started.elapsed().as_millis());
            Ok(spotters)
        }
    }

    /// Listening to one microphone: a stream per part of the phrase, over the
    /// same sound.
    struct Listening {
        mic: String,
        sound: Receiver<Vec<f32>>,
        /// A stream per part, on its spotter.
        streams: Vec<OnlineStream>,
        /// Seconds of sound heard, and when each part was last heard.
        clock: f64,
        heard_at: [Option<f64>; 3],
        /// When the phrase was last heard (a clip saved).
        said_at: Option<f64>,
        /// `HESTECLIPS_DEBUG_VOICE=1`: the loudest sample since the level was
        /// last logged, and when that was.
        peak: f32,
        logged_at: f64,
        /// `HESTECLIPS_VOICE_DUMP=<file.wav>`: everything heard, kept to
        /// replay through the tests (written when listening stops).
        dump: Option<(std::path::PathBuf, Vec<f32>)>,
    }

    fn debug() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("HESTECLIPS_DEBUG_VOICE").is_some())
    }

    impl Drop for Listening {
        fn drop(&mut self) {
            if let Some((path, samples)) = self.dump.take() {
                match write_wav(&path, &samples, RATE) {
                    Ok(()) => eprintln!("voice: {:.1} s of what was heard written to {}", samples.len() as f64 / RATE as f64, path.display()),
                    Err(e) => eprintln!("voice: couldn't write {}: {e}", path.display()),
                }
            }
        }
    }

    /// 16-bit mono WAV.
    fn write_wav(path: &std::path::Path, samples: &[f32], rate: u32) -> std::io::Result<()> {
        let data: Vec<u8> = samples.iter().flat_map(|s| ((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes()).collect();
        let mut out = Vec::with_capacity(44 + data.len());
        out.extend(b"RIFF");
        out.extend((36 + data.len() as u32).to_le_bytes());
        out.extend(b"WAVEfmt ");
        out.extend(16u32.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend(rate.to_le_bytes());
        out.extend((rate * 2).to_le_bytes());
        out.extend(2u16.to_le_bytes());
        out.extend(16u16.to_le_bytes());
        out.extend(b"data");
        out.extend((data.len() as u32).to_le_bytes());
        out.extend(data);
        std::fs::write(path, out)
    }

    impl Listening {
        fn new(spotters: &Spotters, mic: String, sound: Receiver<Vec<f32>>) -> Self {
            let streams = spotters.parts.iter().map(KeywordSpotter::create_stream).collect();
            let dump = std::env::var_os("HESTECLIPS_VOICE_DUMP").map(|p| (std::path::PathBuf::from(p), Vec::new()));
            Self { mic, sound, streams, clock: 0.0, heard_at: [None; 3], said_at: None, peak: 0.0, logged_at: 0.0, dump }
        }

        /// Take the sound that's come in (waiting a little for some); whether
        /// the whole phrase was just said.
        fn step(&mut self, spotters: &Spotters) -> bool {
            let Ok(block) = self.sound.recv_timeout(Duration::from_millis(100)) else { return false };
            let mut samples = block;
            samples.extend(self.sound.try_iter().flatten());
            self.hear(spotters, &samples, RATE)
        }

        /// Listen to `samples` (mono, at `rate`); whether the whole phrase was
        /// just said.
        fn hear(&mut self, spotters: &Spotters, samples: &[f32], rate: u32) -> bool {
            self.clock += samples.len() as f64 / rate as f64;
            if let Some((path, all)) = &mut self.dump {
                all.extend_from_slice(samples);
                // Every 10 s too: quitting the app doesn't stop this thread
                // in an orderly way.
                if (all.len() as f64 / rate as f64) % 10.0 < samples.len() as f64 / rate as f64 {
                    let _ = write_wav(path, all, rate);
                }
            }
            if debug() {
                self.peak = samples.iter().fold(self.peak, |m, s| m.max(s.abs()));
                if self.clock - self.logged_at >= 2.0 {
                    eprintln!("voice: {:.0} s, loudest {:.1} dB", self.clock, 20.0 * self.peak.max(1e-9).log10());
                    (self.peak, self.logged_at) = (0.0, self.clock);
                }
            }
            let mut said = false;
            for (part, (spotter, stream)) in spotters.parts.iter().zip(&self.streams).enumerate() {
                stream.accept_waveform(rate as i32, samples);
                while spotter.is_ready(stream) {
                    spotter.decode(stream);
                    let Some(hit) = spotter.get_result(stream) else { continue };
                    if hit.keyword.is_empty() {
                        continue;
                    }
                    spotter.reset(stream);
                    if debug() {
                        eprintln!("voice: {:.1} s, heard \"{}\"", self.clock, hit.keyword);
                    }
                    let now = self.clock;
                    if self.said_at.is_some_and(|t| now - t < QUIET_AFTER) {
                        continue;
                    }
                    self.heard_at[part] = Some(now);
                    // Any two parts close enough together.
                    let near = self.heard_at.iter().flatten().filter(|t| now - **t <= WITHIN).count();
                    if near >= 2 {
                        self.said_at = Some(now);
                        eprintln!("voice: heard \"hashtag HesteClip that\"");
                        self.heard_at = [None; 3];
                        said = true;
                    }
                }
            }
            said
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Speech made by Windows' text-to-speech for the test (no recordings
        /// in the repo): the phrase must save a clip, the near misses must
        /// not. The user's own takes were tried too (`hears_real_takes`).
        #[test]
        fn hears_the_phrase_and_nothing_else() {
            let spotters = Spotters::load().expect("load the model");
            let dir = std::env::temp_dir().join(format!("hc-voice-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            for (text, wanted) in [
                ("hashtag hesteclip that", true),
                ("okay that was insane. hashtag, heste clip that!", true),
                ("I love this game, let's go again", false),
                ("hashtag blessed, clip it later", false),
                ("has the clip that we made been saved yet", false),
            ] {
                let file = dir.join("speech.wav");
                speak(text, &file);
                let wave = sherpa_onnx::Wave::read(&file.to_string_lossy()).expect(text);
                let rate = wave.sample_rate() as u32;
                let (_tx, rx) = channel();
                let mut l = Listening::new(&spotters, String::new(), rx);
                // Some quiet before and after, as live sound has; fed in
                // 100 ms pieces, as it arrives live.
                let quiet = vec![0.0; rate as usize / 2];
                let mut said = l.hear(&spotters, &quiet, rate);
                for chunk in wave.samples().chunks(rate as usize / 10) {
                    said |= l.hear(&spotters, chunk, rate);
                }
                said |= l.hear(&spotters, &[quiet.clone(), quiet].concat(), rate);
                assert_eq!(said, wanted, "{text}");
            }
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// How often a long recording (many takes, some talk) triggers it, for
        /// tuning: `HESTECLIPS_VOICE_SESSION=session.wav cargo test -- --ignored
        /// --nocapture counts_a_session`.
        #[test]
        #[ignore]
        fn counts_a_session() {
            let spotters = Spotters::load().expect("load the model");
            let file = std::env::var("HESTECLIPS_VOICE_SESSION").expect("HESTECLIPS_VOICE_SESSION");
            let wave = sherpa_onnx::Wave::read(&file).expect("read the session");
            let rate = wave.sample_rate() as u32;
            let (_tx, rx) = channel();
            let mut l = Listening::new(&spotters, String::new(), rx);
            let mut at = Vec::new();
            for (i, chunk) in wave.samples().chunks(rate as usize / 10).enumerate() {
                if l.hear(&spotters, chunk, rate) {
                    at.push(format!("{:.1}", (i + 1) as f64 / 10.0));
                }
            }
            println!("triggered {} times, at {} s", at.len(), at.join(", "));
        }

        /// Say `text` into `file` (16 kHz mono WAV) with Windows' text-to-speech.
        fn speak(text: &str, file: &std::path::Path) {
            let script = format!(
                "Add-Type -AssemblyName System.Speech; \
                 $f = New-Object System.Speech.AudioFormat.SpeechAudioFormatInfo(16000, [System.Speech.AudioFormat.AudioBitsPerSample]::Sixteen, [System.Speech.AudioFormat.AudioChannel]::Mono); \
                 $s = New-Object System.Speech.Synthesis.SpeechSynthesizer; $s.SetOutputToWaveFile('{}', $f); $s.Speak('{}'); $s.Dispose()",
                file.display(),
                text.replace('\'', "''")
            );
            let ok = std::process::Command::new("powershell").args(["-NoProfile", "-Command", &script]).status().is_ok_and(|s| s.success());
            assert!(ok, "text-to-speech for {text:?}");
        }

        /// Recordings of real people saying the phrase, kept out of the repo:
        /// `HESTECLIPS_VOICE_TAKES=a.wav;b.wav cargo test -- --ignored voice`
        /// (16-bit WAV, e.g. `ffmpeg -i take.mp3 -ac 1 -ar 16000 take.wav`).
        #[test]
        #[ignore]
        fn hears_real_takes() {
            let spotters = Spotters::load().expect("load the model");
            let takes = std::env::var("HESTECLIPS_VOICE_TAKES").expect("HESTECLIPS_VOICE_TAKES");
            for file in takes.split(';') {
                let wave = sherpa_onnx::Wave::read(file).expect(file);
                let rate = wave.sample_rate() as u32;
                let (_tx, rx) = channel();
                let mut l = Listening::new(&spotters, String::new(), rx);
                let quiet = vec![0.0; rate as usize / 2];
                let mut said = l.hear(&spotters, &quiet, rate);
                for chunk in wave.samples().chunks(rate as usize / 10) {
                    said |= l.hear(&spotters, chunk, rate);
                }
                said |= l.hear(&spotters, &[quiet.clone(), quiet].concat(), rate);
                assert!(said, "not heard in {file}");
            }
        }
    }
}
