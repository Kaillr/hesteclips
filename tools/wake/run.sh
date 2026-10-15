#!/bin/bash
# The whole run, one GPU stage at a time (see README: more at once stalls an
# 8 GB card). Logs in logs/; re-running skips finished outputs.
cd "$(dirname "$0")"
PY=venv/Scripts/python
export PYTHONUTF8=1 HF_HUB_DISABLE_SYMLINKS_WARNING=1
mkdir -p logs gen feats models
stamp() { echo "[$(date +%H:%M:%S)] $*"; }
die() { stamp "FAILED: $*"; exit 1; }

[ -f voices/quality.json ] || { stamp "voice check"; $PY src/voice_qa.py > logs/voice_qa.log 2>&1 || die "voice check"; }
for job in "pos 40000 train 21" "neg 36000 train 22" "pos 3000 test 23" "neg 3000 test 24"; do
  set -- $job
  [ -f gen/$1_$3_raw.npz ] || { stamp "gen $1 $3"; $PY src/gen.py $1 $2 $3 gen/$1_$3_raw.npz $4 > logs/gen_$1_$3.log 2>&1 || die "gen $1 $3"; }
done
for job in "pos train" "neg train" "pos test" "neg test"; do
  set -- $job
  [ -f gen/$1_$2.npz ] || { stamp "check $1 $2"; $PY src/check.py gen/$1_$2_raw.npz $1 gen/$1_$2.npz > logs/check_$1_$2.log 2>&1 || die "check $1 $2"; }
done
powershell -ExecutionPolicy Bypass -File src/wintts.ps1 && $PY src/wintts.py
export FEATS_CPU=8
for job in "gen/pos_train.npz pos 3 train_pos" "gen/neg_train.npz neg 2 train_neg" "gen/pos_test.npz pos 1 test_pos" \
           "gen/neg_test.npz neg 1 test_neg" "gen/wintts_pos.npz pos 3 wintts_pos" "gen/wintts_neg.npz neg 3 test_neg_win"; do
  set -- $job
  [ -f feats/$4.npz ] || { stamp "scenes $4"; $PY src/feats.py scenes $1 $2 $3 feats/$4.npz 41 >> logs/scenes.log 2>&1 || die "scenes $4"; }
done
for job in "ls100 long_ls100 100" "ls500 long_ls500 100" "musan_speech long_musan_speech 70" "musan_music long_musan_music 30" \
           "musan_noise long_musan_noise 10" "mls_german long_mls_german 15" "mls_french long_mls_french 15" "mls_spanish long_mls_spanish 15" \
           "mls_dutch long_mls_dutch 15" "mls_italian long_mls_italian 15" "mls_portuguese long_mls_portuguese 15" "mls_polish long_mls_polish 15" \
           "lseval eval_ls 12" "mls_german_test eval_mls_german 2" "mls_french_test eval_mls_french 2" "mls_spanish_test eval_mls_spanish 2" \
           "mls_dutch_test eval_mls_dutch 2" "mls_italian_test eval_mls_italian 2" "mls_portuguese_test eval_mls_portuguese 2" "mls_polish_test eval_mls_polish 2"; do
  set -- $job
  [ -f feats/$2.npz ] || { stamp "long $1"; $PY src/feats.py long $1 feats/$2.npz $3 31 >> logs/long.log 2>&1 || die "long $1"; }
done
[ -f feats/user_train.npz ] || $PY src/user.py
stamp "train"
WIDTH=512 DROPOUT=0.4 $PY -u src/train.py mlp512 60000 > logs/train_mlp512.log 2>&1 || die "train"
WIDTH=512 $PY src/choose.py models/mlp512.pt 0.3 | tee logs/choose.log
stamp "done: models/mlp512.onnx"
