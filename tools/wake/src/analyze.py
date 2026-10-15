"""Which held-out near misses fire, which takes are missed: each test take
run clean (no mic/noise) through the model as the app would, grouped by
what was said and by who. usage: python analyze.py <models/x.pt> [thr] [need]"""
import collections, json, os, re, sys
import numpy as np
import torch

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(HERE, "src"))
import feat
import evaluate as E
from gen import load
from choose import load as load_model

def fires(predict, clip, thr, need):
    x = np.concatenate([np.zeros(32000, np.int16), clip, np.zeros(24000, np.int16)])
    x = x[: len(x) // 1280 * 1280]
    rows = torch.from_numpy(feat.stream_embed(x)).cuda()
    p = E.probs(predict, rows, np.arange(15, len(rows)))
    return bool(E.fire_times(p, thr, need))

def core(segs):
    """The near miss itself, as a pattern: IPA kept, words kept."""
    parts = [s for k, s in segs if k != "text" or True]
    return " | ".join(s for _, s in segs)

def kind_of(segs):
    s = " ".join(x for _, x in segs)
    for name, pat in [("HasteClip + other word", r"hˈ?eɪs[td]?kl.?ɪ?p ?(ðˈɪs|ˈɪt|ðə|bˈæk|hˈæt|ðˈɛm|ðˈɛɹ|ˈæt|kˈæt|ðˈoʊz|ðˈiːz|ˈʌp|nˈaʊ)"),
                      ("HasteClip alone", r"^(hˈ?eɪs[td]?|hˌeɪst|ˈeɪst|xˈeɪst|hˈeːst|hˈɛɪst|hˈeist|hˈejst)kl"),
                      ("other start + clip that", r"^(pˈeɪst|tˈeɪst|wˈeɪst|bˈeɪs|fˈeɪs|kˈeɪs|tʃˈeɪs|ɹˈeɪs|plˈeɪs|hˈeɪ |hˈeɪt|ˈeɪt|sˈeɪ|stˈeɪ|hˈeɪsti|dʒˈʌst|lˈæst|fˈæst|mˈeɪk|tˈeɪk|bˈɛst|nˈɛkst|lˈɛts|hˈɛlθ|hˈoʊst|dʒˈɛst) "),
                      ("haste + other clip-word", r"(flˈɪp|klˈɪk|tʃˈɪp|ʃˈɪp|zˈɪp|klˈæp|kˈʌp|klˈiːn|klˈɪps|lˈɪp|ɡɹˈɪp|kˈɪt)"),
                      ("x + clip that", r"^(hˈeɪ|dʒˈʌst|ˈoʊ|jˈoʊ|plˈiːz|sˈʌmwʌn|pɹˈɑːbli|kˈæn juː) kl"),
                      ("wrong order", r"^kl.*(eɪst|eːst)|^ð.*eɪst")]:
        if re.search(pat, s):
            return name
    if any(k == "text" for k, _ in segs):
        return "text: " + [x for k, x in segs if k == "text"][-1].strip(" ,.!?").lower()[:28]
    return "other"

if __name__ == "__main__":
    path = sys.argv[1]
    thr = float(sys.argv[2]) if len(sys.argv) > 2 else 0.6
    need = int(sys.argv[3]) if len(sys.argv) > 3 else 4
    net, predict = load_model(path)
    for kind in ("neg", "pos"):
        clips, spans = load(os.path.join(HERE, "gen", f"{kind}_test.npz"))
        meta = [json.loads(l) for l in open(os.path.join(HERE, "gen", f"{kind}_test.npz.jsonl"), encoding="utf-8")]
        by = collections.defaultdict(lambda: [0, 0])
        examples = collections.defaultdict(list)
        for c, m in zip(clips, meta):
            f = fires(predict, c, thr, need)
            if kind == "neg":
                k = kind_of(m["segs"])
            else:
                w = m["who"]
                k = ("kokoro " if w.startswith("kokoro") else "piper ") + ("accent" if not (w.startswith("en_") or w.startswith("kokoro:a") or w.startswith("kokoro:b")) else "english")
                k += f", err {'<0.2' if m.get('err', 0) < 0.2 else '0.2-0.35' if m['err'] < 0.35 else '>0.35'}"
            by[k][0] += f
            by[k][1] += 1
            if (kind == "neg") == f and len(examples[k]) < 3:
                examples[k].append(" ".join(s for _, s in m["segs"])[:60] + f"  [{m['who'][:24]}]")
        print(f"\n{kind} test, thr {thr} x{need} ({'fired' if kind == 'neg' else 'caught'}), clean audio:")
        for k, (a, b) in sorted(by.items(), key=lambda kv: -kv[1][0] / kv[1][1]):
            print(f"  {a:4d}/{b:4d} {100 * a / b:5.1f}%  {k}")
            for e in examples[k]:
                print("            e.g.", e)
