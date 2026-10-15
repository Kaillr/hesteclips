# Training the "HasteClip that" detector

How `crates/app/assets/wake/hasteclip.onnx` was made. Nothing here is part
of the app. Run it again to retrain (for example with the voice samples
people export from the app). A full run takes ~4 h on an RTX 4060 Ti (8 GB) +
12-thread CPU + 32 GB RAM, and ~200 GB of disk (downloads included).

## Setup (Python 3.12, Windows)

```sh
python -m venv venv
venv/Scripts/pip install torch --index-url https://download.pytorch.org/whl/cu130
venv/Scripts/pip install numpy scipy soundfile pyarrow piper-tts kokoro-onnx onnx pyworld transformers huggingface_hub
venv/Scripts/pip install --no-deps openwakeword "onnxruntime-gpu==1.31.0"
venv/Scripts/pip uninstall -y onnxruntime   # pulled in by kokoro-onnx: clashes with -gpu
venv/Scripts/pip install --no-deps --force-reinstall "onnxruntime-gpu==1.31.0"
```

onnxruntime-gpu 1.31 wants CUDA 13: it finds it in torch's cu130 libraries,
so `import torch` before creating a CUDA session (the scripts do).

Downloads, into this folder:

- `models/`: `melspectrogram.onnx`, `embedding_model.onnx` (copy from
  `crates/app/assets/wake`).
- `voices/`: the Piper voices listed in `voices/pick.py` (`.onnx` +
  `.onnx.json`, huggingface.co/rhasspy/piper-voices, plus `voices.json`).
  Only voices whose model card allows commercial use (CC0, public domain,
  CC BY, Apache, MIT, Unlicense, CMU Arctic). `python src/voice_qa.py`
  scores each on a plain "HasteClip that" with a phoneme recognizer and
  writes `voices/quality.json`; voices above 0.45 phoneme error are skipped
  (the MLS voices babble when given English phonemes).
- `kokoro/`: `kokoro-v1.0.onnx`, `voices-v1.0.bin`
  (github.com/thewh1teagle/kokoro-onnx releases, Apache-2.0).

Then `python src/setup_models.py`: the mel graph's weights for `feat.py`, and
Kokoro with its per-token durations exposed (`kokoro-timed.onnx`), so takes
inside sentences have exact phrase timings.
- `dl/`: LibriSpeech train-clean-100, train-other-500, test-other,
  dev-other (openslr.org/12, CC BY 4.0); MUSAN (openslr.org/17;
  `src/musan_ok.py` keeps only CC BY / CC0 / public-domain files); RIRS_NOISES
  (openslr.org/28, Apache-2.0); Multilingual LibriSpeech, 3 train shards +
  the test shard of 7 languages (huggingface.co/datasets/facebook/multilingual_librispeech, CC BY 4.0).
- `data/validation_set_features.npy` (optional): openWakeWord's validation
  features, only to compare false saves per hour with the old model.

## Steps (`run.sh`)

1. `src/gen.py`: synthetic takes. Positives: the phrase in many forms
   (`src/text.py`: one word/three words/run together, dropped t, accent
   forms, a pause before "that"), alone or inside gaming talk, said once or
   twice; native English voices, accented voices (other languages' voices
   reading English phonemes, with sounds they lack swapped for the nearest),
   Kokoro voices and blends of them. Near misses: parts of the phrase, wrong
   order, rhymes, "clip that" after other words, everyday gaming phrases.
   Test voices/speakers (`TEST_VOICES`, every 10th speaker) are never used
   for train. Piper runs on CPU workers (one thread each), Kokoro on 3 GPU
   workers.
2. `src/check.py`: a phoneme recognizer (wav2vec2 espeak) drops positives
   that don't say the phrase (> 0.5 phoneme error, ~14 %) and near misses
   that do (≤ 0.1, ~4 %).
3. `src/feats.py scenes`: each take through 1–3 random mics (room, mic
   colour, noise/music/other people at 0–30 dB, levels down to -34 dB,
   pitch/vocal-tract changes favouring deeper voices) with background
   before and after; every 80 ms window labelled by where the phrase is in
   it (whole phrase in the window = positive, at any alignment).
   `src/feats.py long`: everyday audio, all negative.
   `src/user.py`: the user's reviewed voice samples ("ignore" ones: half to
   train on, half to test).
4. `src/train.py mlp512 60000` (`WIDTH=512 DROPOUT=0.4`): ~10 min. Each step
   mixes positives, near misses, half-heard phrases, everyday audio and the
   everyday windows the model currently likes most (mined every 2000 steps).
5. `src/choose.py models/mlp512.pt 0.3`: thresholds and frames-in-a-row that
   catch the most held-out takes at ≤ 0.3 false saves an hour;
   `src/analyze.py` lists which near misses fire and which takes are missed.
6. Copy `models/mlp512.onnx` to `crates/app/assets/wake/hasteclip.onnx`,
   set `THRESHOLD`/`NEED` in `voice.rs`, and check the app agrees:
   `HASTECLIP_VOICE_TRACE=1 HASTECLIP_VOICE_SESSION=x.wav cargo test --release counts_a_session -- --ignored --nocapture`
   prints its per-frame probabilities (they matched Python to 0.005).

Features are computed exactly as the app computes them live (`src/feat.py`):
the ONNX mel model floors at 80 dB below the loudest value of its whole
input, so it can't be batched as is; `feat.py` runs the same graph in torch
with the floor per 80 ms chunk, on the GPU with TF32 off (matches the CPU to
1e-4).

## Results (2026-10-16)

Held-out voices, harsh simulated mics, at 0.6 × 4: 86 % of 2667 takes saved
(93–99 % on clean audio; weakest: heavily accented Piper voices, 77 %),
99/120 of Windows' voices; 0.08 false saves an hour over 25 h of held-out
LibriSpeech and MLS (8 languages); 0 of 71 of the user's real false-trigger
snippets; 4 of 77 old "hashtag HesteClip that" takes. Near misses fired
14 %, almost all one-sound rhymes ("taste/case/his/paste clip that", "haste
click that", "hey Steve clip that"); everything else ~0 %.

Tried and not used: convolutions over the 16 frames (more takes at a given
threshold, but 4 false saves/h on openWakeWord's set); mining the near
misses the model likes most (rhymes down to ~1–3 %, takes saved down to
~50–80 %: the nearest near misses are almost the phrase).

Not measured: real people saying the new phrase. Collect takes with the
app's voice samples (Settings > Voice > Review, export) and add them.

## Pitfalls met

- More than ~4 CUDA processes on an 8 GB card (Kokoro workers + another GPU
  job) fill it and everything stalls; the phoneme check next to Kokoro
  jammed twice. Keep GPU stages one at a time.
- Windows' page file: 8 scene workers × the take file + pools came close to
  the commit limit; pools are memory-mapped `.npy` for that reason.
- `np.load(...)["x"]` reads the array again on every access.
