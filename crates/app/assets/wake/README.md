# "HasteClip that" detector

Used by `src/voice.rs`, embedded byte for byte.

- `melspectrogram.onnx`, `embedding_model.onnx`: openWakeWord's feature
  models (v0.5.1, https://github.com/dscripka/openWakeWord/releases), a
  re-implementation of Google's speech embedding model — Apache-2.0.
- `hasteclip.onnx`: our classifier over 16 of those embeddings (~2 s of
  sound): 1536 → 512 → 512 → 1 with LayerNorm, outputs a logit. Trained
  only on data that allows commercial use: synthetic takes from Piper and
  Kokoro voices (native and accented English), LibriSpeech, Multilingual
  LibriSpeech, MUSAN (CC BY / CC0 / public-domain files only) and OpenSLR
  room impulses. Recipe and licences: `tools/wake/`.

Fires after 4 frames in a row (80 ms each) at ≥ 0.6. On voices and speakers
it never trained on, through simulated mics (noise down to 0 dB, quiet
levels, rooms): 86 % of takes saved (93–99 % on clean audio), 99 of 120 of
Windows' own voices; 0.08 false saves per hour over 25 h of held-out speech
(8 languages); 0 of 71 of the user's real snippets the old detector fired
on. Near misses: "clip that", "HasteClip", "hey, clip that" don't fire;
one-sound rhymes ("paste/taste/case clip that") fire ~30–70 % of the time.
No real recordings of the new phrase yet: collect them with the app's voice
samples and retrain.
