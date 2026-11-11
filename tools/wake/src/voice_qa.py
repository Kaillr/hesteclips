"""Score every Piper voice (and Kokoro voice) on a plain "HasteClip that":
mean phoneme error of 8 takes. Writes voices/quality.json."""
import json, os, random, sys
import numpy as np
from scipy.signal import resample_poly
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import torch  # noqa: F401
import gen, phon
from math import gcd
FORMS = ["hˈeɪstklˌɪp ðˈæt", "hˈeɪst klˈɪp ðˈæt", "hˈeɪstklˌɪp ðˈæt!"]
def to16(a, rate):
    g = gcd(16000, rate)
    a = resample_poly(np.asarray(a, np.float32), 16000 // g, rate // g)
    return (a / (np.abs(a).max() + 1e-6) * 0.9 * 32767).astype(np.int16)
random.seed(0)
out = {}
for name in [v for v in gen.EN + gen.ACCENT if v not in gen.SKIP]:
    try:
        v = gen.Piper(name, True)
    except Exception as e:
        print(name, "FAILED", e, flush=True)
        continue
    takes = []
    for i in range(8):
        sp = random.randrange(v.speakers) if v.speakers > 1 else None
        a, rate, _ = v.say([("phrase", FORMS[i % 3])], sp, 1.0)
        takes.append(to16(a, rate))
    seqs = phon.phonemes(takes)
    errs = [phon.phrase_error(s) for s in seqs]
    out[name] = float(np.mean(errs))
    print(f"{np.mean(errs):.2f} {name:34s} {''.join(seqs[0])} | {''.join(seqs[1])}", flush=True)
k = gen.Kokoro(True)
for name in k.names:
    takes = []
    for i in range(6):
        a, rate, _ = k.say([("phrase", FORMS[i % 3])], k.k.voices[name], 1.0)
        takes.append(to16(a, rate))
    seqs = phon.phonemes(takes)
    errs = [phon.phrase_error(s) for s in seqs]
    out["kokoro:" + name] = float(np.mean(errs))
    print(f"{np.mean(errs):.2f} kokoro:{name:27s} {''.join(seqs[0])} | {''.join(seqs[1])}", flush=True)
json.dump(out, open(os.path.join(gen.HERE, "voices", "quality.json"), "w"), indent=1)
