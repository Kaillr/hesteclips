"""Who the model catches and what fools it: the held-out synthetic takes
(gen/pos_test.npz) caught per voice, Norwegian spelling or not, fast or
not, and the held-out negatives (gen/neg_test.npz) that save a clip, most
often first. Each take as recorded, with its own faint noise around it, fed
as the app hears it (THRESHOLD for NEED frames in a row).

usage: python groups.py <thr> <need> <model> [<model> ...]
"""
import collections, json, os, sys
import numpy as np
import torch

HERE = os.environ.get("WAKE_HOME") or os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import feat
from choose import predictor
from gen import load

def fired(predict, clips, thr, need):
    """Whether each clip saves a clip: one stream of them, 2 s of their own
    noise floor between, each judged on the frames while it plays (and
    1 s after)."""
    rng = np.random.default_rng(0)
    parts, spans, t = [np.zeros(32000, np.int16)], [], 32000
    for x in clips:
        fr = x[: max(len(x) // 400, 1) * 400].astype(np.float32)
        floor = np.percentile(np.sqrt((fr.reshape(-1, 400) ** 2).mean(1)), 10) if len(fr) >= 400 else 1.0
        gap = (rng.standard_normal(32000) * max(floor, 1.0)).astype(np.int16)
        parts += [x, gap]
        spans.append((t, t + len(x) + 16000))
        t += len(x) + 32000
    audio = np.concatenate(parts)
    rows = feat.stream_embed(audio[: len(audio) // 1280 * 1280])
    w = torch.from_numpy(np.lib.stride_tricks.sliding_window_view(rows, (16, 96))[:, 0].astype(np.float32)).cuda()
    p = torch.cat([predict(w[i:i + 65536]) for i in range(0, len(w), 65536)]).cpu().numpy()
    run = np.zeros(len(p), np.int32)
    c = 0
    for i, v in enumerate(p >= thr):
        c = c + 1 if v else 0
        run[i] = c
    # Window i ends with row i + 15, which ends at sample 1280 (i + 16).
    ends = (np.arange(len(p)) + 16) * 1280
    out = []
    for a, b in spans:
        m = (ends > a) & (ends <= b)
        out.append(bool((run[m] >= need).any()))
    return np.array(out)

def main(thr, need, models):
    predict = predictor(models)
    a, _ = load(os.path.join(HERE, "gen", "pos_test.npz"))
    meta = [json.loads(l) for l in open(os.path.join(HERE, "gen", "pos_test.npz.jsonl"), encoding="utf-8")]
    hit = fired(predict, a, thr, need)
    groups = collections.defaultdict(list)
    for h, m in zip(hit, meta):
        who = m["who"].split("#")[0]
        lang = who.split(":")[-1][:5] if not who.startswith("kokoro") else "kokoro"
        groups["voice " + who].append(h)
        groups["lang " + lang].append(h)
        groups["spelling " + ("norwegian" if any(s[0] == "phrase_no" for s in m["segs"]) else "english")].append(h)
        groups["fast" if m.get("fast") else "not fast"].append(h)
    print(f"held-out takes caught at {thr} x{need}: {hit.sum()}/{len(hit)} ({hit.mean():.0%})")
    for k in sorted(groups, key=lambda k: (k.split()[0], -len(groups[k]))):
        v = np.array(groups[k])
        print(f"  {k:40s} {v.sum():5d}/{len(v):<5d} {v.mean():5.0%}")
    n, _ = load(os.path.join(HERE, "gen", "neg_test.npz"))
    nmeta = [json.loads(l) for l in open(os.path.join(HERE, "gen", "neg_test.npz.jsonl"), encoding="utf-8")]
    bad = fired(predict, n, thr, need)
    kinds = collections.defaultdict(list)
    for b, m in zip(bad, nmeta):
        kinds["+".join(s[0] for s in m["segs"])].append(b)
    print(f"held-out negatives that save a clip: {bad.sum()}/{len(bad)} ({bad.mean():.1%})")
    for k in sorted(kinds, key=lambda k: -len(kinds[k])):
        v = np.array(kinds[k])
        print(f"  {k:40s} {v.sum():5d}/{len(v):<5d} {v.mean():5.1%}")
    said = collections.Counter(" | ".join(s[1] for s in m["segs"]) for b, m in zip(bad, nmeta) if b)
    print("  what fooled it:")
    for s, c in said.most_common(25):
        print(f"    {c}x {s}")

if __name__ == "__main__":
    a = sys.argv[1:]
    main(float(a[0]), int(a[1]), a[2:])
