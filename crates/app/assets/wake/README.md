# "Hashtag HesteClip that" detector

Used by `src/voice.rs`, embedded byte for byte.

- `melspectrogram.onnx`, `embedding_model.onnx`: openWakeWord's feature
  models (v0.5.1, https://github.com/dscripka/openWakeWord/releases), a
  re-implementation of Google's speech embedding model — Apache-2.0.
- `hesteclip.onnx`: our classifier over 16 of those embeddings (~2 s of
  sound): 1536 → 128 → 128 → 1 with LayerNorm. Trained on synthetic speech
  (Piper voices, English and Norwegian, fast and slurred variants), near
  misses ("hashtag hesteclip", "hashtag clip that", …), cut-off phrases,
  noise, and openWakeWord's ACAV100M negative features
  (CC BY-NC-SA 4.0 data — see tools/wake/README.md). Recipe: `tools/wake/`.

Fires after 4 frames in a row (80 ms each) at ≥ 0.8. On the user's own
recordings, never trained on: 24 of 30 takes in a session, 8 of 8 fast,
quiet live takes, 0 false saves; near misses (123 files, another TTS
engine): 1 fire; everyday audio: ~0.2–0.9 false saves per hour.
