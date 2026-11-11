#!/bin/bash
# Synthetic takes and their phoneme check (steps 1-2 of README.md), one
# stage at a time. WAKE_HOME: the run's folder; PY: its Python.
set -u
SRC="$(cd "$(dirname "$0")" && pwd)/src"
cd "$WAKE_HOME"
export PYTHONUTF8=1 HF_HUB_OFFLINE=1
mkdir -p logs gen
stamp() { echo "[$(date +%H:%M:%S)] $*"; }
for job in "pos 32000 train 21" "neg 26000 train 22" "pos 3000 test 23" "neg 3000 test 24"; do
  set -- $job
  [ -f gen/$1_$3_raw.npz ] || { stamp "gen $1 $3"; "$PY" -X utf8 "$SRC/gen.py" $1 $2 $3 gen/$1_$3_raw.npz $4 > logs/gen_$1_$3.log 2>&1 || { stamp "FAILED gen $1 $3"; exit 1; }; }
done
for job in "pos train" "neg train" "pos test" "neg test"; do
  set -- $job
  [ -f gen/$1_$2.npz ] || { stamp "check $1 $2"; "$PY" -X utf8 "$SRC/check.py" gen/$1_$2_raw.npz $1 gen/$1_$2.npz > logs/check_$1_$2.log 2>&1 || { stamp "FAILED check $1 $2"; exit 1; }; }
done
stamp "gen done"
