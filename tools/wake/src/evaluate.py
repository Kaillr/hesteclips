"""Score a model as the app uses it (voice.rs): a probability every 80 ms, a
save when the NEEDth frame in a row is >= THR (once per such run), and not
within REFRACTORY s of the last.

  takes caught       held-out voices saying the phrase (test scenes; also
                     Windows' own voices: another engine entirely)
  near misses fired  held-out voices saying things that must not save
  false saves / h    hours of held-out everyday audio (speech in 8
                     languages, LibriSpeech test/dev-other)
  user samples       the user's reviewed snippets ("ignore" ones: fires)

predict(windows (N, 16, 96) float32 tensor on the GPU) -> (N,) probabilities.
"""
import glob, os, sys
import numpy as np
import torch

# Where a run keeps its files (WAKE_HOME, default: this folder).
HERE = os.environ.get("WAKE_HOME") or os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FE = os.environ.get("FEATS_DIR", os.path.join(HERE, "feats"))
REFRACTORY = 1.2
HOP_S = 0.08

def windows(rows, ends):
    """(len(ends), 16, 96) windows ending at the given row indices."""
    idx = torch.as_tensor(ends, device=rows.device)[:, None] + torch.arange(-15, 1, device=rows.device)[None, :]
    return rows[idx].float()

def probs(predict, rows, ends, batch=16384):
    out = []
    for i in range(0, len(ends), batch):
        out.append(predict(windows(rows, ends[i:i + batch])))
    return torch.cat(out).cpu().numpy() if out else np.zeros(0)

def fire_times(p, thr, need):
    """Frame indices where a save happens."""
    above = p >= thr
    run, last, out = 0, -1e9, []
    for i, a in enumerate(above):
        run = run + 1 if a else 0
        if run == need and (i - last) * HOP_S >= REFRACTORY:
            out.append(i)
            last = i
    return out

class Scenes:
    """A test scenes file: per scene, its windows' probabilities in order."""
    def __init__(self, path, dev="cuda"):
        d = np.load(path)
        self.rows = torch.from_numpy(d["rows"]).to(dev)
        self.labels = d["labels"]
        self.starts = np.append(d["starts"], len(d["labels"]))
        self.ends = np.flatnonzero(self.labels != -9)

    def scores(self, predict):
        p = probs(predict, self.rows, self.ends)
        full = np.zeros(len(self.labels), np.float32)
        full[self.ends] = p
        return full

    def caught(self, full, thr, need):
        """Positive scenes: saved while (or right after) the whole phrase is heard."""
        hit = 0
        for s, e in zip(self.starts[:-1], self.starts[1:]):
            lab, p = self.labels[s:e], full[s:e]
            ok = lab != -9
            f = fire_times(p[ok], thr, need)
            l = lab[ok]
            hit += any(l[i] in (1, -1, 2, 3) for i in f)
        return hit, len(self.starts) - 1

    def fired(self, full, thr, need):
        n = 0
        for s, e in zip(self.starts[:-1], self.starts[1:]):
            ok = self.labels[s:e] != -9
            n += bool(fire_times(full[s:e][ok], thr, need))
        return n, len(self.starts) - 1

class Long:
    """Hours of everyday audio: false saves per hour."""
    def __init__(self, paths, dev="cuda"):
        self.parts = []
        for p in paths:
            d = np.load(p)
            rows = torch.from_numpy(d["rows"]).to(dev)
            starts = np.append(d["starts"], len(d["rows"]))
            lead = int(d["lead"])
            self.parts.append((os.path.basename(p), rows, starts, lead))

    def scores(self, predict):
        out = []
        for name, rows, starts, lead in self.parts:
            pieces = []
            for s, e in zip(starts[:-1], starts[1:]):
                ends = np.arange(s + lead, e)
                pieces.append(ends)
            ends = np.concatenate(pieces)
            p = probs(predict, rows, ends)
            # back to pieces
            o, per = 0, []
            for x in pieces:
                per.append(p[o:o + len(x)])
                o += len(x)
            out.append((name, per))
        return out

    @staticmethod
    def per_hour(scored, thr, need):
        res = {}
        for name, per in scored:
            f = sum(len(fire_times(p, thr, need)) for p in per)
            h = sum(len(p) for p in per) * HOP_S / 3600
            res[name] = (f, h)
        return res

class OWWVal:
    """openWakeWord's validation features (whole-file, not streaming; ~11 h):
    the number the old model was tuned on."""
    def __init__(self, dev="cuda"):
        p = os.path.join(HERE, "data", "validation_set_features.npy")
        self.rows = torch.from_numpy(np.load(p).astype(np.float16)).to(dev) if os.path.exists(p) else None

    def scores(self, predict):
        if self.rows is None:
            return None
        return probs(predict, self.rows, np.arange(15, len(self.rows)))

class Report:
    """Everything a model is judged on (files that aren't there are left out):

      caught    test_pos: held-out synthetic voices; win: Windows' voices
      near      test_neg*: held-out near misses that fired
      real      rtest_pos_clean: the users' held-out takes as recorded;
                rtest_pos: the same through simulated mics
      ignore    rtest_neg_clean / rtest_neg: the users' held-out "ignore"
                clips that fired
      haste     haste_test_pos: HasteClip's "HasteClip that" takes that fired
      false/h   eval_*: held-out everyday audio (8 languages)
    """
    def __init__(self, dev="cuda"):
        def scenes(pattern):
            return [Scenes(p, dev) for p in sorted(glob.glob(os.path.join(FE, pattern)))]
        self.pos = scenes("test_pos*.npz")
        self.neg = scenes("test_neg*.npz")
        self.win = scenes("wintts_pos*.npz")
        self.real = {k: scenes(f"rtest_pos{k}.npz") for k in ("_clean", "")}
        self.ign = {k: scenes(f"rtest_neg{k}.npz") for k in ("_clean", "")}
        self.haste = scenes("haste_test_pos*.npz")
        self.long = Long(sorted(glob.glob(os.path.join(FE, "eval_*.npz"))), dev)
        self.oww = OWWVal(dev)

    def score(self, predict):
        both = lambda xs: [(s, s.scores(predict)) for s in xs]
        return {
            "pos": both(self.pos), "neg": both(self.neg), "win": both(self.win),
            "real": {k: both(v) for k, v in self.real.items()},
            "ign": {k: both(v) for k, v in self.ign.items()},
            "haste": both(self.haste),
            "long": self.long.scores(predict),
            "oww": self.oww.scores(predict),
        }

    @staticmethod
    def numbers(sc, thr, need):
        def total(pairs, how):
            c = [getattr(s, how)(p, thr, need) for s, p in pairs]
            return (sum(a for a, _ in c), sum(b for _, b in c))
        n = {"caught": total(sc["pos"], "caught"), "win": total(sc["win"], "caught"), "near": total(sc["neg"], "fired"),
             "real": {k: total(v, "caught") for k, v in sc["real"].items()},
             "ign": {k: total(v, "fired") for k, v in sc["ign"].items()},
             "haste": total(sc["haste"], "fired")}
        per = Long.per_hour(sc["long"], thr, need)
        f = sum(v[0] for v in per.values())
        h = sum(v[1] for v in per.values())
        n["fph"] = f / h if h else float("nan")
        n["hours"] = h
        n["per_source"] = {k: v[0] / v[1] for k, v in per.items()}
        if sc["oww"] is not None:
            n["oww_fph"] = len(fire_times(sc["oww"], thr, need)) / (len(sc["oww"]) * HOP_S / 3600)
        return n

    @staticmethod
    def line(n, thr, need):
        pc = lambda a, b: f"{a}/{b} ({100 * a / max(b, 1):.0f}%)"
        s = f"thr {thr:.2f} x{need}: real {pc(*n['real']['_clean'])}"
        if n["real"][""][1]:
            s += f" mic {pc(*n['real'][''])}"
        s += f"  ignore fired {n['ign']['_clean'][0]}/{n['ign']['_clean'][1]}"
        if n["ign"][""][1]:
            s += f" mic {n['ign'][''][0]}/{n['ign'][''][1]}"
        s += f"  synth {pc(*n['caught'])}"
        if n["win"][1]:
            s += f" win {n['win'][0]}/{n['win'][1]}"
        s += f"  near {pc(*n['near'])}"
        if n["haste"][1]:
            s += f"  haste {pc(*n['haste'])}"
        s += f"  false/h {n['fph']:.2f} ({n['hours']:.0f} h)"
        if "oww_fph" in n:
            s += f" oww {n['oww_fph']:.2f}"
        return s
