#!/bin/bash
# Step 3 of README.md: the synthetic takes heard through simulated mics, as
# openWakeWord features (the users' own takes: real.py, then the scenes
# lines in README.md). WAKE_HOME: the run's folder; PY: its Python.
set -u
SRC="$(cd "$(dirname "$0")" && pwd)/src"
cd "$WAKE_HOME"
export PYTHONUTF8=1 HF_HUB_OFFLINE=1 FEATS_CPU=${FEATS_CPU:-8}
mkdir -p logs feats
stamp() { echo "[$(date +%H:%M:%S)] $*"; }
for job in "gen/pos_train.npz pos 3 train_pos" "gen/neg_train.npz neg 2 train_neg" "gen/pos_test.npz pos 1 test_pos" "gen/neg_test.npz neg 1 test_neg"; do
  set -- $job
  [ -f feats/$4.npz ] || { stamp "scenes $4"; "$PY" -X utf8 "$SRC/feats.py" scenes $1 $2 $3 feats/$4.npz 41 >> logs/scenes.log 2>&1 || { stamp "FAILED scenes $4"; exit 1; }; }
done
stamp "scenes done"
