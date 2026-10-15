"""wintts/*.wav (Windows' voices) -> gen/wintts_{pos,neg}.npz like gen.py makes."""
import glob, json, os
import numpy as np
import soundfile as sf
HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
for kind in ("pos", "neg"):
    clips, spans, meta = [], [], []
    for f in sorted(glob.glob(os.path.join(HERE, "wintts", f"{kind}_*.wav"))):
        a, sr = sf.read(f, dtype="float32")
        loud = np.flatnonzero(np.abs(a) > 0.02 * np.abs(a).max())
        a = a[max(loud[0] - 1280, 0):loud[-1] + 1280]
        a = a / np.abs(a).max() * 0.9
        clips.append((a * 32767).astype(np.int16))
        spans.append([(1280 if loud[0] >= 1280 else loud[0], len(a) - 1280), (-1, -1)])
        meta.append(json.dumps({"who": "windows", "file": os.path.basename(f)}))
    out = os.path.join(HERE, "gen", f"wintts_{kind}.npz")
    np.savez(out, audio=np.concatenate(clips), lens=np.array([len(c) for c in clips]), spans=np.array(spans, np.int64))
    open(out + ".jsonl", "w").write("\n".join(meta) + "\n")
    print(out, len(clips))
