# Training the "hashtag HesteClip that" detector

How `crates/app/assets/wake/hesteclip.onnx` was made. Nothing here is part
of the app. Run it again to retrain, for example with more voice samples
exported from the app (Settings > Voice > Review > Export).

The recipe is HasteClip's (its `tools/wake`), for this phrase: synthetic
takes from many voices, a phoneme check that drops bad ones, simulated mics
and rooms, hours of everyday audio with the windows the model likes most
mined as negatives, thresholds chosen on held-out data. On top of it, the
users' own takes and "ignore" clips, and HasteClip's "HasteClip that" takes
as negatives (they lack "hashtag").

## How people say it

The users' exported takes (110 "trigger", 368 "ignore", two people) set
the forms `src/text.py` makes. They are fast (0.64 to 1.75 s, median
0.84 s) and reduced: the "h" of both words usually gone, "hashtag" and
"heste" run together, "that" cut to "ta"/"da". A phoneme recognizer hears
them as e.g. `aʃaɡɛsklipta`, `aʃtaɡɛstɪklɪpta`, `haʒdaɡɛskliptad`. Their
"ignore" clips are mostly game sound (rhythmic "ta-ta-ta"), plus Norwegian
near misses ("bare hest klipp da").

## Setup (Python 3.12, Windows)

As HasteClip's README: a venv with CUDA torch, onnxruntime-gpu, piper-tts,
kokoro-onnx, transformers, pyworld; its downloads (`dl/`: LibriSpeech,
MUSAN, RIRS_NOISES, Multilingual LibriSpeech), Piper voices (`voices/`),
Kokoro (`kokoro/`), feature models (`models/`). Everything a run makes
goes into `WAKE_HOME` (set it to a folder outside the repo). A HasteClip
run's folder can be shared: its `dl`, `voices`, `kokoro` (as links), its
background pools (`data/pool_*.npy`), and its everyday-audio features
(`feats/long_*.npz`, `feats/eval_*.npz`), which don't depend on the
phrase. Its `feats/train_pos.npz`, `train_neg.npz`, `test_pos.npz` go in as
`haste_train_pos.npz`, `haste_train_neg.npz`, `haste_test_pos.npz`.

## Steps

1. `run_gen.sh`: synthetic takes (`src/gen.py`, `src/text.py`) and the
   phoneme check (`src/check.py`). Voices: English 40 %, Nordic 35 %
   (Norwegian most, half of their takes in Norwegian spelling read with
   Norwegian phonemes: `hæsjtæg hesteklipp dæt`), other accents 25 %;
   Kokoro for a third. 60 % of takes fast. Held-out voices
   (`TEST_VOICES`, every 10th speaker, Danish) are only in the test split.
2. `src/real.py <export folders>`: the users' samples, split by day (train
   on the earliest days, test on the later ones); `FINAL=1` puts all in
   train (`real_all/`). Its train "ignore" clips also become a background
   pool (`aug.py`): real game sound behind synthetic takes.
3. `run_scenes.sh`, and the real samples' scenes:
   `feats.py scenes real/trig_train.npz rpos 40 feats/real_pos_train.npz`,
   `… real/ign_train.npz rneg 8 feats/real_neg_train.npz`, and for the
   test split `CLEAN_COPY=1 … rpos 0 feats/rtest_pos_clean.npz` (as
   recorded), `… rpos 3 feats/rtest_pos.npz` (through simulated mics), the
   same for `ign_test` as `rtest_neg*`. `src/wintts.ps1` (PowerShell 7:
   Windows' own voices, the Norwegian "Microsoft Jon" among them, a third
   engine never trained on) then `src/wintts.py` and their scenes
   (`wintts_pos`, `test_neg_win`).
4. `run_sweep.sh`: train settings on the held-out split (`src/train.py`)
   and score each (`src/choose.py`).
5. The model that ships, in a folder of its own (`FEATS_DIR=feats_final`):
   `FINAL=1 src/real.py …` and its scenes as `real_pos_train.npz` /
   `real_neg_train.npz` (every sample, so the last days too); the Danish
   voice, held out until now, as `train_pos_da.npz` / `train_neg_da.npz`
   (`GEN_VOICES=da_DK-talesyntese-medium GEN_KOKORO=0 GEN_KOKORO_SHARE=0
   gen.py pos 2400 train …`, then `check.py` and `feats.py scenes` as
   above); everything else linked from `feats/`. Then `train.py` with the
   chosen settings for several seeds, `src/export.py` averaging them into
   one ONNX graph, and `src/choose.py` on it for `THRESHOLD`/`NEED` in
   `voice.rs`.
6. Before building the app: `src/apptest.py <thr> <need> <model…>
   --voices` runs the app's own voice test (`hears_the_phrase_and_nothing_else`)
   in Python, with each of Windows' voices; `src/groups.py` shows the
   held-out takes caught per voice and what fools the model;
   `src/session.py` writes a session of real takes and clips with the
   probabilities Python gives it, to compare with the app's
   (`HESTECLIPS_VOICE_TRACE=1`, `counts_a_session`): they agree to 0.01.

Features are computed exactly as the app computes them live (`src/feat.py`,
see HasteClip's README).

## Results

The sweep (60k steps each, ~9 min on an RTX 4060 Ti at ~120 steps/s),
compared at its best setting with ≤ 0.1 false saves an hour: width 512
against 1024, the users' samples weighted ×1, ×2 or ×4, near misses ×1, ×2
or ×3, and a second seed. All within a seed's noise of each other (±1 of
the users' takes, ±3 % of held-out synthetic takes, ±6 of Windows'
voices); 120k steps wasn't tried, as every run levelled off by ~40k.
Averaging models helped a little (+2 % synthetic, +5 Windows' voices at
the same false saves), so the app's classifier is three: seeds 10 and 11
with near misses ×2, seed 12 with ×1, users' samples ×2, width 512
(12.6 MB).

The same three trained without the users' last five days (`real.py`'s
`TEST_DAYS`), at the app's setting (0.75, 3 frames), against the previous
detector (0.8, 4):

|                                        | new          | previous     |
|----------------------------------------|--------------|--------------|
| users' takes, as recorded              | 53 / 53      | 47 / 53      |
| users' takes, through simulated mics   | 156 / 159    | 122 / 159    |
| users' "ignore" clips fired (as rec.)  | 3 / 196      | 6 / 196      |
| "ignore" clips, simulated mics         | 3 / 392      | 7 / 392      |
| held-out synthetic takes, as recorded  | 89 %         | 71 %         |
| … Norwegian speakers / fast takes      | 98 % / 89 %  | 74 % / 60 %  |
| … through simulated mics               | 82 %         | 56 %         |
| Windows' voices (another engine)       | 181 / 240    | 169 / 240    |
| near misses fired                      | 10 %         | 10 %         |
| "HasteClip that" fired                 | 0.3 %        | 1 %          |
| false saves / h (25 h, 8 languages)    | 0.04         | 0.04         |

The model that ships (also trained on those days and the Danish voice):
0.04 false saves an hour on the same 25 h, 86 % of held-out synthetic takes
through simulated mics (93 % as recorded), 186 of 240 of Windows' voices,
and the app's own voice test passes in Python with each of Windows'
Desktop voices (Mark, a OneCore voice the test can't use, misses its
"okay that was insane. hashtag heste clip that!"). In the app, a session
of 20 takes and 10 "ignore" clips saved 20 clips, one per take.

## Pitfalls met

- Run `src/wintts.ps1` with PowerShell 7: Windows PowerShell 5.1 sees only
  the two Desktop voices (Mark and Jon are missing), and reads the script as
  ANSI without its BOM.
- `python -I` leaves the script's folder off `sys.path` (and ignores
  `PYTHONUTF8`): run the scripts plainly, with `-X utf8`.
- The users' takes are trimmed to the phrase: padded with digital silence,
  the old model caught 37 of 53 held-out takes; padded with their own noise
  floor, as a live mic hears them, 47. The scenes pad real takes so.
- A quick take keeps the new model sure for as long as the whole phrase is
  in the ~2 s it hears, up to ~1.5 s: longer than the 1.2 s the app waited
  between saves, so one take saved two clips (2 of 20 in a test session).
  The app now saves on the `NEED`th sure frame in a row only, and
  `evaluate.py` counts saves the same way.
- The previous model was trained on the Danish voice, so on it alone the
  old model looks better (69 % against 54 %, a voice the new one never
  heard). Compare unseen voices on the ones neither model heard.
