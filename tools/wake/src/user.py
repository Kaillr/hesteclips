"""The user's reviewed voice samples (the app's Settings > Voice > Review),
into features:

  user_train.npz        half of the "ignore" snippets: negatives to train on
  user_ignore.npz       the other half: how many would save a clip (eval)
  user_oldphrase.npz    "trigger" snippets of the OLD phrase ("hashtag
                        HesteClip that"): reported only, never trained on

Split by snippet id, so a retrain with more samples keeps the same halves.
An "ignore" verdict is about the trimmed part: that part plus up to 1 s
around it is used.

usage: python user.py [samples folder]
"""
import glob, hashlib, json, os, sys
import numpy as np
import soundfile as sf

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(HERE, "src"))
import feat
from feats import HOP, LEAD, WIN, WIN_SECS

def main(folder):
    groups = {"train": [], "ignore": [], "oldphrase": []}
    for j in sorted(glob.glob(os.path.join(folder, "*.json"))):
        m = json.load(open(j, encoding="utf-8"))
        label = {"phrase": "trigger", "other": "ignore"}.get(m.get("label"), m.get("label"))
        if label not in ("trigger", "ignore"):
            continue
        a, sr = sf.read(j[:-5] + ".wav", dtype="int16")
        if sr != 16000:
            continue
        if m.get("trim"):
            s, e = m["trim"]
            a = a[max(int((s - 1) * sr), 0):int((e + 1) * sr)]
        if label == "trigger":
            groups["oldphrase"].append(a)
        elif int(hashlib.md5(m["id"].encode()).hexdigest(), 16) % 2 == 0:
            groups["train"].append(a)
        else:
            groups["ignore"].append(a)
    lead = int(np.ceil((LEAD - 0.4 + WIN_SECS) * 16000 / HOP))
    for name, clips in groups.items():
        rows, labels, starts, total = [], [], [], 0
        for a in clips:
            x = np.concatenate([np.zeros(int(LEAD * 16000), np.int16), a, np.zeros(8000, np.int16)])
            x = x[: len(x) // HOP * HOP]
            r = feat.stream_embed(x).astype(np.float16)
            if len(r) <= lead:
                continue
            lab = np.zeros(len(r), np.int8)
            lab[:lead] = -9
            starts.append(total)
            rows.append(r)
            labels.append(lab)
            total += len(r)
        out = os.path.join(HERE, "feats", f"user_{name}.npz")
        np.savez(out, rows=np.concatenate(rows), labels=np.concatenate(labels), starts=np.array(starts), lead=lead)
        print(f"{out}: {len(starts)} snippets, {total * 0.08 / 60:.1f} min", flush=True)

if __name__ == "__main__":
    if len(sys.argv) > 1:
        main(sys.argv[1])
    else:  # the app's folder (before the rename it was "hesteclips")
        app = [os.path.join(os.environ["LOCALAPPDATA"], n, "voice-samples") for n in ("hasteclip", "hesteclips")]
        main(next((d for d in app if os.path.isdir(d)), app[0]))
