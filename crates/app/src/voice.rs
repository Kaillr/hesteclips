//! "Hashtag HesteClip that": say it and a clip is saved, like pressing the
//! save shortcut.
//!
//! A detector trained for this one phrase (`assets/wake`, 3 MB, built into
//! the app) listens to the microphone on the Sources page, on this computer,
//! only while the replay buffer runs and the setting is on. It knows nothing
//! but the phrase, so talking doesn't save clips.
//!
//! General speech models never heard "HesteClip" (Norwegian "heste" + English
//! "clip") and lost it when said fast: the keyword spotter this replaces
//! caught 15 of 30 of the user's takes. This one learned the whole phrase's
//! sound from thousands of synthetic voices, fast and slurred ones too, and
//! against near misses: 24 of 30, and 8 of 8 fast, quiet ones said live.
//! Only the whole phrase counts: "hashtag", "hashtag hesteclip" and "hesteclip
//! that" don't save clips.
//!
//! How: every 80 ms of sound becomes a frame of openWakeWord's speech features
//! (a mel spectrogram, then Google's speech embedding); the last 16 frames
//! (~2 s) go to our classifier. The phrase is heard when it's sure
//! ([`THRESHOLD`]) for [`NEED`] frames in a row. A few % of one core.

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
        let p = problem.clone();
        std::thread::Builder::new()
            .name("voice".into())
            .spawn(move || detector::run(rx, heard_tx, quick, live, ctx, p))
            .expect("spawn voice thread");
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

mod detector {
    use std::collections::VecDeque;
    use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use capture::mixer::{LiveAudio, RATE};
    use tract_onnx::prelude::*;

    use crate::service::QuickSave;

    /// The models, built into the app (`assets/wake`, see its README).
    const MELSPECTROGRAM: &[u8] = include_bytes!("../assets/wake/melspectrogram.onnx");
    const EMBEDDING: &[u8] = include_bytes!("../assets/wake/embedding_model.onnx");
    const CLASSIFIER: &[u8] = include_bytes!("../assets/wake/hesteclip.onnx");

    /// How sure the classifier must be, frame by frame, and for how many
    /// frames (80 ms each) in a row. One frame alone fired ~16 times an hour
    /// on everyday audio; four in a row at 0.8: under one, with the same
    /// takes caught (picked on half of openWakeWord's validation audio,
    /// checked on the other half).
    pub(super) const THRESHOLD: f32 = 0.8;
    pub(super) const NEED: usize = 4;
    /// After a clip is saved, the phrase isn't heard again for this long: one
    /// take, one clip (the next take can come 1.6 s later).
    const QUIET_AFTER: f64 = 1.2;
    /// The models' rate (48 kHz from the mixer is brought down to it).
    const MODEL_RATE: u32 = 16_000;
    /// Sound per feature frame (80 ms), and what the mel spectrogram is run
    /// over for it: three 10 ms hops more, so its window fits (as openWakeWord).
    const HOP: usize = 1280;
    const MEL_INPUT: usize = HOP + 480;
    const MEL_BINS: usize = 32;
    /// Mel frames per embedding, and embeddings per classification.
    const MEL_FRAMES: usize = 76;
    const EMBEDDINGS: usize = 16;
    const EMBEDDING_SIZE: usize = 96;

    type Model = Arc<TypedRunnableModel>;

    pub(super) fn run(
        rx: Receiver<Option<String>>,
        heard: Sender<bool>,
        quick: QuickSave,
        live: Arc<LiveAudio>,
        ctx: egui::Context,
        problem: Arc<Mutex<Option<String>>>,
    ) {
        let mut models: Option<Models> = None;
        let mut listening: Option<Listening> = None;
        loop {
            let wanted = match (&mut listening, &models) {
                // Listening: take sound until told otherwise.
                (Some(l), Some(m)) => match rx.try_recv() {
                    Ok(w) => Some(w),
                    Err(TryRecvError::Empty) => {
                        if l.step(m) {
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
            if models.is_none() {
                match Models::load() {
                    Ok(m) => {
                        models = Some(m);
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
            listening = Some(Listening::new(mic, sound));
        }
    }

    /// The three models, ready to run.
    pub(super) struct Models {
        mel: Model,
        embedding: Model,
        classifier: Model,
    }

    impl Models {
        pub(super) fn load() -> Result<Self, String> {
            let started = std::time::Instant::now();
            let load = |bytes: &[u8], shape: &[usize]| -> TractResult<Model> {
                tract_onnx::onnx()
                    .model_for_read(&mut std::io::Cursor::new(bytes))?
                    .with_input_fact(0, f32::fact(shape).into())?
                    .into_optimized()?
                    .into_runnable()
            };
            let models = (|| -> TractResult<Self> {
                Ok(Self {
                    mel: load(MELSPECTROGRAM, &[1, MEL_INPUT])?,
                    embedding: load(EMBEDDING, &[1, MEL_FRAMES, MEL_BINS, 1])?,
                    classifier: load(CLASSIFIER, &[1, EMBEDDINGS, EMBEDDING_SIZE])?,
                })
            })()
            .map_err(|e| format!("the voice model didn't load: {e}"))?;
            eprintln!("voice: listening for \"hashtag HesteClip that\" (model loaded in {} ms)", started.elapsed().as_millis());
            Ok(models)
        }

        fn run(model: &Model, input: Tensor) -> TractResult<Vec<f32>> {
            let out = model.run(tvec!(input.into()))?;
            Ok(out[0].to_plain_array_view::<f32>()?.iter().copied().collect())
        }
    }

    /// Listening to one microphone.
    pub(super) struct Listening {
        pub(super) mic: String,
        sound: Receiver<Vec<f32>>,
        /// Seconds of sound heard.
        clock: f64,
        /// Sound at the models' rate, as 16-bit sample values, not yet made
        /// into a frame (plus the 480 samples before, which the next frame
        /// needs too).
        pending: Vec<f32>,
        /// The last input samples, for the 48 → 16 kHz filter.
        tail: Vec<f32>,
        mels: VecDeque<[f32; MEL_BINS]>,
        embeddings: VecDeque<Vec<f32>>,
        /// Frames in a row the classifier has been sure.
        sure: usize,
        /// When the phrase was last heard (a clip saved).
        said_at: Option<f64>,
        /// `HESTECLIPS_DEBUG_VOICE=1`: the loudest sample and the surest
        /// frame since they were last logged, and when that was.
        peak: f32,
        best: f32,
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
        pub(super) fn new(mic: String, sound: Receiver<Vec<f32>>) -> Self {
            let dump = std::env::var_os("HESTECLIPS_VOICE_DUMP").map(|p| (std::path::PathBuf::from(p), Vec::new()));
            Self {
                mic,
                sound,
                clock: 0.0,
                // The first frame's run-up: silence.
                pending: vec![0.0; MEL_INPUT - HOP],
                tail: Vec::new(),
                mels: VecDeque::new(),
                embeddings: VecDeque::new(),
                sure: 0,
                said_at: None,
                peak: 0.0,
                best: 0.0,
                logged_at: 0.0,
                dump,
            }
        }

        /// Take the sound that's come in (waiting a little for some); whether
        /// the whole phrase was just said.
        fn step(&mut self, models: &Models) -> bool {
            let Ok(block) = self.sound.recv_timeout(Duration::from_millis(100)) else { return false };
            let mut samples = block;
            samples.extend(self.sound.try_iter().flatten());
            self.hear(models, &samples, RATE)
        }

        /// Sound at the models' rate (48 kHz is filtered and taken every
        /// third sample), as 16-bit sample values, which the models expect.
        fn prepare(&mut self, samples: &[f32], rate: u32) -> Vec<f32> {
            let out = if rate == MODEL_RATE * 3 {
                // Low-pass below the new rate's limit, then every third sample.
                let taps = decimation_filter();
                let mut all = std::mem::take(&mut self.tail);
                all.extend_from_slice(samples);
                let mut out = Vec::with_capacity(samples.len() / 3 + 1);
                // One output per three inputs, while the filter fits; the rest
                // (the filter's length, less) waits for the next call.
                let mut i = 0;
                while i + taps.len() <= all.len() {
                    out.push(taps.iter().zip(&all[i..]).map(|(t, s)| t * s).sum());
                    i += 3;
                }
                self.tail = all[i..].to_vec();
                out
            } else {
                assert_eq!(rate, MODEL_RATE, "voice: sound must be 16 or 48 kHz");
                samples.to_vec()
            };
            out.into_iter().map(|s| (s.clamp(-1.0, 1.0) * 32767.0).round()).collect()
        }

        /// Listen to `samples` (mono, at `rate`); whether the whole phrase was
        /// just said.
        pub(super) fn hear(&mut self, models: &Models, samples: &[f32], rate: u32) -> bool {
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
            }
            let prepared = self.prepare(samples, rate);
            self.pending.extend(prepared);
            let mut said = false;
            while self.pending.len() >= MEL_INPUT {
                match self.frame(models) {
                    Ok(Some(p)) => said |= self.judge(p),
                    Ok(None) => {}
                    Err(e) => eprintln!("voice: {e}"),
                }
                self.pending.drain(..HOP);
            }
            if debug() && self.clock - self.logged_at >= 2.0 {
                eprintln!("voice: {:.0} s, loudest {:.1} dB, surest {:.2}", self.clock, 20.0 * self.peak.max(1e-9).log10(), self.best);
                (self.peak, self.best, self.logged_at) = (0.0, 0.0, self.clock);
            }
            said
        }

        /// One 80 ms frame (the front of `pending`): its features, and how
        /// sure the classifier is of the phrase once 2 s have been heard.
        fn frame(&mut self, models: &Models) -> TractResult<Option<f32>> {
            let input = tract_ndarray::Array2::from_shape_vec((1, MEL_INPUT), self.pending[..MEL_INPUT].to_vec())?;
            let mel = Models::run(&models.mel, input.into_tensor())?;
            for row in mel.chunks_exact(MEL_BINS) {
                // openWakeWord's scaling of the mel spectrogram.
                self.mels.push_back(std::array::from_fn(|k| row[k] / 10.0 + 2.0));
            }
            while self.mels.len() > MEL_FRAMES {
                self.mels.pop_front();
            }
            if self.mels.len() < MEL_FRAMES {
                return Ok(None);
            }
            let input = tract_ndarray::Array4::from_shape_vec((1, MEL_FRAMES, MEL_BINS, 1), self.mels.iter().flatten().copied().collect())?;
            self.embeddings.push_back(Models::run(&models.embedding, input.into_tensor())?);
            while self.embeddings.len() > EMBEDDINGS {
                self.embeddings.pop_front();
            }
            if self.embeddings.len() < EMBEDDINGS {
                return Ok(None);
            }
            let input = tract_ndarray::Array3::from_shape_vec((1, EMBEDDINGS, EMBEDDING_SIZE), self.embeddings.iter().flatten().copied().collect())?;
            let logit = Models::run(&models.classifier, input.into_tensor())?[0];
            Ok(Some(1.0 / (1.0 + (-logit).exp())))
        }

        /// Whether this frame completes the phrase.
        fn judge(&mut self, p: f32) -> bool {
            self.best = self.best.max(p);
            self.sure = if p >= THRESHOLD { self.sure + 1 } else { 0 };
            if self.sure < NEED || self.said_at.is_some_and(|t| self.clock - t < QUIET_AFTER) {
                return false;
            }
            self.said_at = Some(self.clock);
            eprintln!("voice: heard \"hashtag HesteClip that\"");
            true
        }
    }

    /// A 48 → 16 kHz low-pass: a windowed sinc cutting off at 7 kHz, below
    /// the 8 kHz the new rate can hold.
    fn decimation_filter() -> &'static [f32] {
        static TAPS: std::sync::OnceLock<Vec<f32>> = std::sync::OnceLock::new();
        TAPS.get_or_init(|| {
            let n = 49;
            let cutoff = 7_000.0 / (MODEL_RATE * 3) as f32;
            let mid = (n - 1) as f32 / 2.0;
            let taps: Vec<f32> = (0..n)
                .map(|i| {
                    let x = i as f32 - mid;
                    let sinc = if x == 0.0 { 2.0 * cutoff } else { (2.0 * std::f32::consts::PI * cutoff * x).sin() / (std::f32::consts::PI * x) };
                    let window = 0.54 - 0.46 * (2.0 * std::f32::consts::PI * i as f32 / (n - 1) as f32).cos();
                    sinc * window
                })
                .collect();
            let sum: f32 = taps.iter().sum();
            taps.into_iter().map(|t| t / sum).collect()
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A WAV file as mono samples, and its rate.
        fn read(path: &std::path::Path) -> (Vec<f32>, u32) {
            let mut r = hound::WavReader::open(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let spec = r.spec();
            let ch = spec.channels as usize;
            let samples: Vec<f32> = match spec.sample_format {
                hound::SampleFormat::Int => r.samples::<i32>().map(|s| s.unwrap() as f32 / (1 << (spec.bits_per_sample - 1)) as f32).collect(),
                hound::SampleFormat::Float => r.samples::<f32>().map(Result::unwrap).collect(),
            };
            (samples.chunks(ch).map(|f| f.iter().sum::<f32>() / ch as f32).collect(), spec.sample_rate)
        }

        /// Times (s) the phrase is heard in `samples`, fed in 100 ms pieces
        /// as live sound arrives, with quiet before and after.
        fn heard_in(models: &Models, samples: &[f32], rate: u32) -> Vec<f64> {
            let (_tx, rx) = channel();
            let mut l = Listening::new(String::new(), rx);
            let quiet = vec![0.0; rate as usize * 2];
            let mut at = Vec::new();
            let lead = quiet.len() as f64 / rate as f64;
            for chunk in quiet.iter().chain(samples).chain(&quiet[..rate as usize]).copied().collect::<Vec<_>>().chunks(rate as usize / 10) {
                if l.hear(models, chunk, rate) {
                    at.push(l.clock - lead);
                }
            }
            at
        }

        /// Speech made by Windows' text-to-speech for the test (no recordings
        /// in the repo): the phrase must save a clip, the near misses must
        /// not. The user's own takes were tried too (`hears_real_takes`).
        #[cfg(windows)]
        #[test]
        fn hears_the_phrase_and_nothing_else() {
            let models = Models::load().expect("load the model");
            let dir = std::env::temp_dir().join(format!("hc-voice-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let mut wrong = Vec::new();
            for (text, wanted) in [
                ("hashtag hesteclip that", true),
                // Said in one go. A pause after "hashtag" (this voice makes
                // 0.45 s of a comma) isn't heard: the phrase no longer fits
                // the ~2 s the detector looks at, and it never heard one.
                ("okay that was insane. hashtag heste clip that!", true),
                ("I love this game, let's go again", false),
                ("hashtag blessed, clip it later", false),
                ("has the clip that we made been saved yet", false),
                ("can you clip that for me", false),
                ("that was a nice clip", false),
                ("hashtag gaming", false),
                ("check the hashtag on twitter", false),
                ("stag party this weekend", false),
                ("he has to clip through the wall", false),
                // Parts of the phrase: only the whole of it counts.
                ("hashtag", false),
                ("hashtag hesteclip", false),
                ("hesteclip that", false),
                ("hashtag clip that", false),
            ] {
                let file = dir.join("speech.wav");
                speak(text, &file);
                let (samples, rate) = read(&file);
                if heard_in(&models, &samples, rate).is_empty() == wanted {
                    wrong.push(format!("{text:?} (wanted {wanted})"));
                }
            }
            let _ = std::fs::remove_dir_all(&dir);
            assert!(wrong.is_empty(), "wrong: {}", wrong.join(", "));
        }

        /// How often a long recording (many takes, some talk) triggers it, for
        /// tuning: `HESTECLIPS_VOICE_SESSION=session.wav cargo test -- --ignored
        /// --nocapture counts_a_session` (16 or 48 kHz).
        #[test]
        #[ignore]
        fn counts_a_session() {
            let models = Models::load().expect("load the model");
            let file = std::env::var("HESTECLIPS_VOICE_SESSION").expect("HESTECLIPS_VOICE_SESSION");
            let (samples, rate) = read(std::path::Path::new(&file));
            let t = std::time::Instant::now();
            let at = heard_in(&models, &samples, rate);
            let secs = samples.len() as f64 / rate as f64;
            println!(
                "triggered {} times, at {} s ({:.1}% of one core)",
                at.len(),
                at.iter().map(|t| format!("{t:.1}")).collect::<Vec<_>>().join(", "),
                t.elapsed().as_secs_f64() / secs * 100.0
            );
        }

        /// Say `text` into `file` (16 kHz mono WAV) with Windows' text-to-speech.
        #[cfg(windows)]
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
        /// (WAV, e.g. `ffmpeg -i take.mp3 -ac 1 -ar 16000 take.wav`).
        #[test]
        #[ignore]
        fn hears_real_takes() {
            let models = Models::load().expect("load the model");
            let takes = std::env::var("HESTECLIPS_VOICE_TAKES").expect("HESTECLIPS_VOICE_TAKES");
            for file in takes.split(';') {
                let (samples, rate) = read(std::path::Path::new(file));
                assert!(!heard_in(&models, &samples, rate).is_empty(), "not heard in {file}");
            }
        }
    }
}
