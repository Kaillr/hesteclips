# "Hashtag HesteClip that" detector

Used by `src/voice.rs`, embedded byte for byte.

- `melspectrogram.onnx`, `embedding_model.onnx`: openWakeWord's feature
  models (v0.5.1, https://github.com/dscripka/openWakeWord/releases), a
  re-implementation of Google's speech embedding model — Apache-2.0.
- `hesteclip.onnx`: our classifier over 16 of those embeddings (~2 s of
  sound): three networks of 1536 → 512 → 512 → 1 with LayerNorm, their
  logits averaged; outputs a logit. Trained on synthetic takes from Piper
  and Kokoro voices whose model cards allow commercial use (English,
  Norwegian, also in Norwegian spelling, Swedish, Danish and other
  accents, mostly fast and slurred), the HesteClips users' own exported
  takes and "ignore" clips, near misses ("hashtag hesteclip", "hashtag clip
  that", "bare heste klipp da", …), HasteClip's "HasteClip that" takes,
  LibriSpeech, Multilingual LibriSpeech, MUSAN (CC BY / CC0 /
  public-domain files only) and OpenSLR room impulses. Recipe:
  `tools/wake/`.

Saves on the 3rd frame in a row (80 ms each) at ≥ 0.75, once per take.
Trained the same way without the users' last five days of samples, and
checked on those days: 53 of 53 takes heard (the previous detector: 47),
156 of 159 through simulated mics (122), 3 of 196 "ignore" clips fired (6).
Held-out synthetic voices: 89 % of takes (71 %), 98 % of the Norwegian
speakers' (74 %), 89 % of fast takes (60 %); through simulated mics 82 %
(56 %). Windows' own voices, another engine: 181 of 240 (169).
Everyday audio, 25 h of speech in 8 languages: 0.04 false saves per hour
(the same). Near misses still fire on ~10 % of tries (the same), HasteClip's
"HasteClip that" on 0.3 % (1 %).
