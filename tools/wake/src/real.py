"""The users' own reviewed voice samples (the app's Settings > Voice >
Review > Export), as takes like gen.py makes:

  real/trig_<split>.npz   "trigger" takes: the phrase, trimmed to it by the
                          person (span = all of it)
  real/ign_<split>.npz    "ignore" clips: must not save a clip (what the
                          detector fired on or nearly did, and clips saved
                          by hand that weren't the phrase)
  data/pool_user_*.npy    the train ignore clips as background sound for
                          aug.py (real game audio, real rooms)

Split by day: train on the earliest days, test on the later ones, as the
model will be used. FINAL=1 puts every sample in train (for the model that
ships, once the settings are chosen with the split), into real_all/.

usage: python real.py <export folder> [<export folder> ...]
"""
import json, os, sys
import numpy as np
import soundfile as sf

HERE = os.environ.get("WAKE_HOME") or os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TEST_DAYS = {"20261012", "20261014", "20261015", "20261016", "20261017"}
FINAL = os.environ.get("FINAL") == "1"

def main(folders):
    sets = {(k, s): [] for k in ("trig", "ign") for s in ("train", "test")}
    for folder in folders:
        j = json.load(open(os.path.join(folder, "samples.json"), encoding="utf-8"))
        for it in j["samples"]:
            a, sr = sf.read(os.path.join(folder, it["file"]), dtype="int16")
            if sr != 16000 or a.ndim != 1:
                continue
            day = os.path.basename(it["file"])[:8]
            split = "test" if day in TEST_DAYS and not FINAL else "train"
            kind = "trig" if it["label"] == "trigger" else "ign"
            sets[(kind, split)].append((a, it, folder))
    out_dir = os.path.join(HERE, "real_all" if FINAL else "real")
    os.makedirs(out_dir, exist_ok=True)
    for (kind, split), items in sets.items():
        if not items:
            continue
        clips = [a for a, *_ in items]
        if kind == "trig":
            spans = [[(0, len(a)), (-1, -1)] for a in clips]
        else:
            spans = [[(-1, -1), (-1, -1)] for _ in clips]
        out = os.path.join(out_dir, f"{kind}_{split}.npz")
        np.savez(out, audio=np.concatenate(clips), lens=np.array([len(a) for a in clips]), spans=np.array(spans, np.int64))
        with open(out + ".jsonl", "w", encoding="utf-8") as f:
            for a, it, folder in items:
                f.write(json.dumps({"who": "user:" + os.path.basename(folder.rstrip("/\\")), "file": it["file"], "kind": it["kind"],
                                    "score": it.get("score"), "secs": len(a) / 16000}) + "\n")
        print(f"{out}: {len(clips)} ({sum(len(a) for a in clips) / 16000 / 60:.1f} min)", flush=True)
    # The train ignore clips as a background pool (int16, peak-normalized).
    bg = [a.astype(np.float32) for a, *_ in sets[("ign", "train")]]
    bg = [(x / (np.abs(x).max() + 1e-6) * 0.9 * 32767).astype(np.int16) for x in bg if np.abs(x).max() > 50]
    os.makedirs(os.path.join(HERE, "data"), exist_ok=True)
    np.save(os.path.join(HERE, "data", "pool_user_audio.npy"), np.concatenate(bg))
    np.save(os.path.join(HERE, "data", "pool_user_lens.npy"), np.array([len(x) for x in bg]))
    print(f"background pool: {len(bg)} clips", flush=True)

if __name__ == "__main__":
    main(sys.argv[1:])
