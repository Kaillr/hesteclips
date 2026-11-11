"""Train the "hashtag HesteClip that" classifier on openWakeWord embeddings.

usage: python train.py <name> [steps]
  env: WIDTH, DROPOUT, LR, SEED, EVAL_EVERY, MINE_EVERY, REAL_W

Data (feats/): train_pos*.npz / train_neg*.npz synthetic scenes (windows
labelled by feats.py), real_pos_train*.npz / real_neg_train*.npz the users'
own takes and "ignore" clips (real.py, then feats.py scenes), haste_*.npz
HasteClip's "HasteClip that" scenes (all negative here: no "hashtag"),
long_*.npz everyday audio (all negative).

Each step: windows of the phrase (synthetic and the users' own), near
misses, half-heard phrases ("cut", "late"), talk around it, background,
"HasteClip that", the users' "ignore" clips, everyday audio, and the
everyday windows the model currently likes most (mined every MINE_EVERY
steps from a million random ones): those are what become false saves.
"""
import glob, os, sys, time
import numpy as np
import torch
from torch import nn

# Where a run keeps its files (WAKE_HOME, default: this folder).
HERE = os.environ.get("WAKE_HOME") or os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FE = os.environ.get("FEATS_DIR", os.path.join(HERE, "feats"))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import evaluate as E

NAME = sys.argv[1] if len(sys.argv) > 1 else "x"
STEPS = int(sys.argv[2]) if len(sys.argv) > 2 else 60000
WIDTH = int(os.environ.get("WIDTH", "512"))
DROPOUT = float(os.environ.get("DROPOUT", "0.4"))
LR = float(os.environ.get("LR", "1e-3"))
SEED = int(os.environ.get("SEED", "0"))
EVAL_EVERY = int(os.environ.get("EVAL_EVERY", "10000"))
MINE_EVERY = int(os.environ.get("MINE_EVERY", "2000"))
NEAR_W = float(os.environ.get("NEAR_W", "2.0"))
# The users' own takes and clips: few, but what the model is for.
REAL_W = float(os.environ.get("REAL_W", "2.0"))
dev = "cuda"

class MLP(nn.Module):
    def __init__(self, w=WIDTH):
        super().__init__()
        self.f = nn.Sequential(
            nn.Flatten(),
            nn.Linear(16 * 96, w), nn.LayerNorm(w), nn.ReLU(), nn.Dropout(DROPOUT),
            nn.Linear(w, w), nn.LayerNorm(w), nn.ReLU(), nn.Dropout(DROPOUT),
            nn.Linear(w, 1),
        )

    def forward(self, x):
        return self.f(x).squeeze(-1)

def load_scenes(pattern):
    rows, labels = [], []
    for p in sorted(glob.glob(os.path.join(FE, pattern))):
        d = np.load(p)
        rows.append(d["rows"])
        labels.append(d["labels"])
    if not rows:
        return None, None
    return np.concatenate(rows), np.concatenate(labels)

class Pool:
    """Rows on the CPU, sampled as 16-row windows ending at `ends`."""
    def __init__(self, rows, ends):
        self.rows = torch.from_numpy(rows)
        self.ends = ends
        self.hours = len(ends) * 0.08 / 3600

    def windows(self, idx):
        e = torch.from_numpy(self.ends[idx])
        return self.rows[e[:, None] + torch.arange(-15, 1)[None, :]]

def long_pool(pattern):
    """Everyday audio: every window after each piece's lead-in."""
    rows, ends, off = [], [], 0
    for p in sorted(glob.glob(os.path.join(FE, pattern))):
        d = np.load(p)
        r = d["rows"]
        st = np.append(d["starts"], len(r))
        lead = int(d["lead"])
        for s, e in zip(st[:-1], st[1:]):
            ends.append(np.arange(s + lead, e) + off)
        rows.append(r)
        off += len(r)
    return Pool(np.concatenate(rows), np.concatenate(ends))

def scene_pool(pattern):
    """Scenes whose every window is negative here (HasteClip's phrase)."""
    r, l = load_scenes(pattern)
    return Pool(r, np.flatnonzero(l != -9)) if r is not None else None

def main():
    torch.manual_seed(SEED)
    np.random.seed(SEED)
    t0 = time.time()
    pr, pl = load_scenes("train_pos*.npz")
    nr, nl = load_scenes("train_neg*.npz")
    rpr, rpl = load_scenes("real_pos_train*.npz")
    rnr, rnl = load_scenes("real_neg_train*.npz")
    parts = [pr, nr] + ([rpr] if rpr is not None else []) + ([rnr] if rnr is not None else [])
    rows = torch.from_numpy(np.concatenate(parts)).to(dev)
    o1 = len(pr)
    o2 = o1 + len(nr)
    o3 = o2 + (len(rpr) if rpr is not None else 0)
    pick = {
        "pos": np.flatnonzero(pl == 1),
        "near": np.flatnonzero(nl == 0) + o1,
        "talk": np.flatnonzero(pl == 0),
        "cut": np.flatnonzero(pl == 2),
        "late": np.flatnonzero(pl == 3),
    }
    if rpr is not None:
        pick["rpos"] = np.flatnonzero(rpl == 1) + o2
        # The users' half-heard takes count with the synthetic ones.
        pick["cut"] = np.concatenate([pick["cut"], np.flatnonzero(rpl == 2) + o2])
        pick["late"] = np.concatenate([pick["late"], np.flatnonzero(rpl == 3) + o2])
    if rnr is not None:
        pick["rneg"] = np.flatnonzero((rnl == 0) | (rnl == 4)) + o3
    long = long_pool("long_*.npz")
    haste = scene_pool("haste_train_*.npz")
    print("windows: " + ", ".join(f"{k} {len(v)}" for k, v in pick.items()) + f"; everyday {long.hours:.0f} h"
          + (f"; HasteClip phrase {len(haste.ends)}" if haste else "") + f" (loaded in {time.time() - t0:.0f} s)", flush=True)
    for k, v in pick.items():
        assert len(v) > 0, f"no {k} windows"

    # (kind, per batch, loss weight)
    MIX = [("pos", 448, 1.0), ("near", 320, NEAR_W), ("talk", 96, 1.0), ("cut", 160, 1.5), ("late", 64, 0.5)]
    if "rpos" in pick:
        MIX.append(("rpos", 160, REAL_W))
    if "rneg" in pick:
        MIX.append(("rneg", 160, 1.5 * REAL_W))
    N_LONG, N_HARD, N_HASTE = 1024, 256, 192 if haste else 0

    net = MLP().to(dev)
    print(f"mlp width {WIDTH}, dropout {DROPOUT}, seed {SEED}: {sum(p.numel() for p in net.parameters()) / 1e3:.0f}k parameters", flush=True)
    opt = torch.optim.AdamW(net.parameters(), lr=LR, weight_decay=0.01)
    sched = torch.optim.lr_scheduler.OneCycleLR(opt, max_lr=LR, total_steps=STEPS, pct_start=0.05)
    bce = nn.BCEWithLogitsLoss(reduction="none")
    rep = E.Report()

    def predict(x):
        net.eval()
        with torch.no_grad():
            p = torch.sigmoid(net(x))
        net.train()
        return p

    hard = None
    ar = torch.arange(-15, 1, device=dev)[None, :]
    for step in range(1, STEPS + 1):
        if step % MINE_EVERY == 0 or hard is None and step > MINE_EVERY // 2:
            # The everyday windows the model now finds most phrase-like.
            idx = np.random.randint(len(long.ends), size=1_000_000)
            sc = []
            for i in range(0, len(idx), 50000):
                sc.append(predict(long.windows(idx[i:i + 50000]).to(dev, non_blocking=True).float()))
            sc = torch.cat(sc).cpu().numpy()
            top = np.argsort(-sc)[:20000]
            hard = long.windows(idx[top]).to(dev)
        xs, ws, ys = [], [], []
        for k, n, w in MIX:
            e = torch.as_tensor(pick[k][np.random.randint(len(pick[k]), size=n)], device=dev)
            xs.append(rows[e[:, None] + ar])
            ws.append(torch.full((n,), w, device=dev))
            ys.append(torch.full((n,), 1.0 if k in ("pos", "rpos") else 0.0, device=dev))
        # Everyday audio counts more as training goes on: fewer false saves.
        ramp = 1 + 9 * min(step / (0.6 * STEPS), 1.0)
        xs.append(long.windows(np.random.randint(len(long.ends), size=N_LONG)).to(dev, non_blocking=True))
        ws.append(torch.full((N_LONG,), ramp, device=dev))
        ys.append(torch.zeros(N_LONG, device=dev))
        if hard is not None:
            xs.append(hard[torch.randint(len(hard), (N_HARD,), device=dev)])
            ws.append(torch.full((N_HARD,), ramp, device=dev))
            ys.append(torch.zeros(N_HARD, device=dev))
        if haste:
            xs.append(haste.windows(np.random.randint(len(haste.ends), size=N_HASTE)).to(dev, non_blocking=True))
            ws.append(torch.full((N_HASTE,), 1.5, device=dev))
            ys.append(torch.zeros(N_HASTE, device=dev))
        x = torch.cat(xs).float()
        w, y = torch.cat(ws), torch.cat(ys)
        x = x + 0.05 * x.std() * torch.randn_like(x)
        loss = (bce(net(x), y) * w).sum() / w.sum()
        if not torch.isfinite(loss):
            sys.exit(f"step {step}: loss is {loss.item()}, stopping")
        opt.zero_grad()
        loss.backward()
        opt.step()
        sched.step()
        if step % 500 == 0:
            print(f"step {step}/{STEPS} loss {loss.item():.4f} ({time.time() - t0:.0f} s)", flush=True)
        if step % EVAL_EVERY == 0 or step == STEPS:
            sc = rep.score(predict)
            for thr, need in ((0.5, 3), (0.7, 4), (0.8, 4), (0.9, 4)):
                print("   ", rep.line(rep.numbers(sc, thr, need), thr, need), flush=True)
            torch.save(net.state_dict(), os.path.join(HERE, "models", f"{NAME}_{step}.pt"))
    torch.save(net.state_dict(), os.path.join(HERE, "models", f"{NAME}.pt"))
    export(net, os.path.join(HERE, "models", f"{NAME}.onnx"))
    print("saved", NAME, f"({(time.time() - t0) / 60:.1f} min)", flush=True)

def export(net, path):
    net.eval().cpu()
    torch.onnx.export(net, torch.zeros(1, 16, 96), path, input_names=["x"], output_names=["logit"],
                      dynamic_axes={"x": {0: "n"}, "logit": {0: "n"}}, opset_version=13, dynamo=False)

if __name__ == "__main__":
    main()
