# Training the "hashtag HesteClip that" detector

How `crates/app/assets/wake/hesteclip.onnx` was made. Nothing here is part of
the app; run it again to retrain (more voices, pauses, other data).

## Setup (Python 3.12)

```sh
python -m venv venv
venv/Scripts/pip install torch --index-url https://download.pytorch.org/whl/cu124
venv/Scripts/pip install "onnxruntime-gpu==1.22.0" numpy scipy soundfile tqdm piper-tts onnx requests scikit-learn
venv/Scripts/pip install --no-deps openwakeword
```

`onnxruntime-gpu` 1.22 matches torch's CUDA 12 (1.30 wants CUDA 13). Download
into this folder:

- `models/`: `melspectrogram.onnx`, `embedding_model.onnx` from openWakeWord
  v0.5.1's GitHub release (Apache-2.0).
- `voices/`: Piper voices (`.onnx` + `.onnx.json`) from
  huggingface.co/rhasspy/piper-voices: en_US-libritts_r-medium,
  en_GB-vctk-medium, en_US-arctic-medium, no_NO-talesyntese-medium,
  sv_SE-nst-medium, da_DK-talesyntese-medium.
- `data/`: from huggingface.co/datasets/davidscripka/openwakeword_features,
  `openwakeword_features_ACAV100M_2000_hrs_16bit.npy` (saved as
  `acav_2000h.npy`, 17 GB) and `validation_set_features.npy`;
  ESC-50 (github.com/karolpiczak/ESC-50, unzipped as `ESC-50-master`);
  MIT room impulse responses (mcdermottlab.mit.edu/Reverb, unzipped as
  `mit_rir`).

**Licence note:** the ACAV100M features are CC BY-NC-SA 4.0. openWakeWord
licenses its own trained models non-commercially because of data like this.
For a model free of that question, train against permissively licensed
negatives (e.g. LibriSpeech, Common Voice) instead.

## Steps

1. `GEN_DEVICE=cuda GEN_JOBS=3 python gen.py pos 10000 gen/pos.npz 1`, then
   `… neg 10000 gen/neg.npz 2`: Piper takes of the phrase (IPA, Norwegian
   "hɛstə", fast and slurred forms) and near misses. One CPU thread per
   worker; more than 3 GPU workers jammed an 8 GB card. ~25 min each.
2. `FEATS_CPU=4 python feats.py`: into 2 s windows with noise, room, mic
   colour and level, then openWakeWord features. **On the CPU**: the GPU gives
   different features (the app computes them on the CPU). ~10 min.
3. `python train.py 30000 wake2`: the classifier, scored on held-out real
   recordings every 5000 steps. ~3 min on a GPU.
4. Copy `wake2.onnx` to `crates/app/assets/wake/hesteclip.onnx`.

`evaluate.py` scores a model: takes caught in your own recordings (folder in
`HC_TAKES`, never trained on), near misses fired on (`test/near_*.wav`, made
with Windows' voices: another engine than training's), and false saves per
hour of openWakeWord's validation audio (half to pick settings, half to
report). `STREAM=1` computes features 80 ms at a time, as the app does.

Results (wake2, 4 frames in a row at ≥ 0.8): session 24/30 (the keyword
spotter it replaced: 15), live 8/8, near misses 1/123, 0.2–0.9 false saves
an hour. Not heard: a pause after "hashtag" (the phrase must fit ~2 s).
