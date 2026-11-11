#!/bin/bash
# Step 4 of README.md: train each setting on the held-out split (real.py
# without FINAL) and score it (choose.py), one at a time on the GPU.
# usage: run_sweep.sh "name:ENV=x ENV=y:steps" ...   (WAKE_HOME, PY as before)
set -u
SRC="$(cd "$(dirname "$0")" && pwd)/src"
cd "$WAKE_HOME"
export PYTHONUTF8=1 HF_HUB_OFFLINE=1
mkdir -p logs models
stamp() { echo "[$(date +%H:%M:%S)] $*"; }
for job in "$@"; do
  IFS=: read -r name envs steps <<< "$job"
  if [ ! -f "models/$name.pt" ]; then
    stamp "train $name ($envs, $steps steps)"
    env $envs "$PY" -u -X utf8 "$SRC/train.py" "$name" "$steps" > "logs/train_$name.log" 2>&1 || { stamp "FAILED train $name"; continue; }
  fi
  stamp "choose $name"
  env $envs "$PY" -X utf8 "$SRC/choose.py" "models/$name.pt" --target 0.1 > "logs/choose_$name.log" 2>&1 || stamp "FAILED choose $name"
  head -4 "logs/choose_$name.log"
done
stamp "sweep done"
