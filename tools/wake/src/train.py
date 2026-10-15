"""Train the "HasteClip that" classifier on openWakeWord embeddings.

usage: python train.py <name> [steps]
  env: ARCH (mlp|conv), WIDTH, DROPOUT, LR, EVAL_EVERY, MINE_EVERY

Data (feats/): train_pos*.npz / train_neg*.npz scenes (windows labelled
by feats.py), long_*.npz everyday audio (all negative), user_train*.npz
(the user's reviewed "ignore" snippets, all negative).

Each step: windows of the phrase, near misses, half-heard phrases ("cut",
"late"), talk around it, background, everyday audio, and the everyday
windows the model currently likes most (mined every MINE_EVERY steps from
a million random ones): those are what become false saves.
"""
import glob, json, os, sys, time
import numpy as np
import torch
from torch import nn

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FE = os.environ.get("FEATS_DIR", os.path.join(HERE, "feats"))
sys.path.insert(0, os.path.join(HERE, "src"))
import evaluate as E

NAME = sys.argv[1]
STEPS = int(sys.argv[2]) if len(sys.argv) > 2 else 40000
ARCH = os.environ.get("ARCH", "mlp")
WIDTH = int(os.environ.get("WIDTH", "256"))
DROPOUT = float(os.environ.get("DROPOUT", "0.3"))
LR = float(os.environ.get("LR", "1e-3"))
EVAL_EVERY = int(os.environ.get("EVAL_EVERY", "5000"))
MINE_EVERY = int(os.environ.get("MINE_EVERY", "2000"))
# Near misses: loss weight, and how many of the currently surest ones (mined
# like everyday audio) go in each batch: rhymes ("taste clip that") are rare
# in the mix but are what fires.
NEAR_W = float(os.environ.get("NEAR_W", "2.0"))
N_HARD_NEAR = int(os.environ.get("HARD_NEAR", "0"))
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

class Conv(nn.Module):
    """Convolutions over the 16 frames (order and local timing), then dense."""
    def __init__(self, w=WIDTH):
        super().__init__()
        c = w // 2
        self.c = nn.Sequential(
            nn.Conv1d(96, c, 3, padding=1), nn.GroupNorm(1, c), nn.ReLU(),
            nn.Conv1d(c, c, 3, padding=1, dilation=1), nn.GroupNorm(1, c), nn.ReLU(),
            nn.Conv1d(c, c, 3, padding=2, dilation=2), nn.GroupNorm(1, c), nn.ReLU(),
        )
        self.f = nn.Sequential(nn.Flatten(), nn.Dropout(DROPOUT), nn.Linear(16 * c, w), nn.LayerNorm(w), nn.ReLU(), nn.Dropout(DROPOUT), nn.Linear(w, 1))

    def forward(self, x):
        return self.f(self.c(x.transpose(1, 2))).squeeze(-1)

def load_scenes(pattern):
    rows, labels, off = [], [], 0
    for p in sorted(glob.glob(os.path.join(FE, pattern))):
        d = np.load(p)
        rows.append(d["rows"])
        labels.append(d["labels"])
    if not rows:
        return None, None
    return np.concatenate(rows), np.concatenate(labels)

class LongPool:
    """Everyday audio rows (CPU), sampled as windows."""
    def __init__(self, pattern):
        self.rows, ends, off = [], [], 0
        for p in sorted(glob.glob(os.path.join(FE, pattern))):
            d = np.load(p)
            r = d["rows"]
            st = np.append(d["starts"], len(r))
            lead = int(d["lead"])
            for s, e in zip(st[:-1], st[1:]):
                ends.append(np.arange(s + lead, e) + off)
            self.rows.append(r)
            off += len(r)
        self.rows = torch.from_numpy(np.concatenate(self.rows))
        self.ends = np.concatenate(ends)
        self.hours = len(self.ends) * 0.08 / 3600

    def windows(self, idx):
        e = torch.from_numpy(self.ends[idx])
        return self.rows[e[:, None] + torch.arange(-15, 1)[None, :]]

def main():
    from gen import low_priority
    low_priority()
    torch.manual_seed(0)
    np.random.seed(0)
    t0 = time.time()
    pr, pl = load_scenes("train_pos*.npz")
    nr, nl = load_scenes("train_neg*.npz")
    off = len(pr)
    rows = torch.from_numpy(np.concatenate([pr, nr])).to(dev)
    lab = np.concatenate([pl, nl])
    is_neg_scene = np.zeros(len(lab), bool)
    is_neg_scene[off:] = True
    # Window ends by kind.
    pick = {
        "pos": np.flatnonzero(lab == 1),
        "near": np.flatnonzero((lab == 0) & is_neg_scene),
        "talk": np.flatnonzero((lab == 0) & ~is_neg_scene),
        "cut": np.flatnonzero(lab == 2),
        "late": np.flatnonzero(lab == 3),
    }
    long = LongPool("long_*.npz")
    user = LongPool("user_train*.npz") if glob.glob(os.path.join(FE, "user_train*.npz")) else None
    print("windows: " + ", ".join(f"{k} {len(v)}" for k, v in pick.items()) + f"; everyday {long.hours:.0f} h"
          + (f"; user {user.hours * 60:.0f} min" if user else "") + f" (loaded in {time.time() - t0:.0f} s)", flush=True)
    for k, v in pick.items():
        assert len(v) > 0, f"no {k} windows"

    # (kind, per batch, loss weight)
    MIX = [("pos", 512, 1.0), ("near", 320, NEAR_W), ("talk", 96, 1.0), ("cut", 160, 1.5), ("late", 64, 0.5)]
    N_LONG, N_HARD, N_USER = 1024, 256, 64 if user else 0

    net = (MLP() if ARCH == "mlp" else Conv()).to(dev)
    print(f"{ARCH} width {WIDTH}: {sum(p.numel() for p in net.parameters()) / 1e3:.0f}k parameters", flush=True)
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
    best = None
    log = []
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
            if N_HARD_NEAR:
                e = torch.as_tensor(pick["near"], device=dev)
                sc = torch.cat([predict(rows[e[i:i + 65536, None] + torch.arange(-15, 1, device=dev)[None, :]].float()) for i in range(0, len(e), 65536)])
                hard_near = e[torch.topk(sc, 50000).indices]
        xs, ws, ys = [], [], []
        for k, n, w in MIX:
            e = torch.as_tensor(pick[k][np.random.randint(len(pick[k]), size=n)], device=dev)
            xs.append(rows[e[:, None] + torch.arange(-15, 1, device=dev)[None, :]])
            ws.append(torch.full((n,), w, device=dev))
            ys.append(torch.full((n,), 1.0 if k == "pos" else 0.0, device=dev))
        # Everyday audio counts more as training goes on: fewer false saves.
        ramp = 1 + 9 * min(step / (0.6 * STEPS), 1.0)
        xs.append(long.windows(np.random.randint(len(long.ends), size=N_LONG)).to(dev, non_blocking=True))
        ws.append(torch.full((N_LONG,), ramp, device=dev))
        ys.append(torch.zeros(N_LONG, device=dev))
        if hard is not None:
            xs.append(hard[torch.randint(len(hard), (N_HARD,), device=dev)])
            ws.append(torch.full((N_HARD,), ramp, device=dev))
            ys.append(torch.zeros(N_HARD, device=dev))
        if N_HARD_NEAR and hard is not None:
            e = hard_near[torch.randint(len(hard_near), (N_HARD_NEAR,), device=dev)]
            xs.append(rows[e[:, None] + torch.arange(-15, 1, device=dev)[None, :]])
            ws.append(torch.full((N_HARD_NEAR,), NEAR_W, device=dev))
            ys.append(torch.zeros(N_HARD_NEAR, device=dev))
        if user:
            xs.append(user.windows(np.random.randint(len(user.ends), size=N_USER)).to(dev))
            ws.append(torch.full((N_USER,), 3.0, device=dev))
            ys.append(torch.zeros(N_USER, device=dev))
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
            for thr, need in ((0.5, 3), (0.8, 3), (0.8, 4), (0.9, 4)):
                n = rep.numbers(sc, thr, need)
                print("   ", rep.line(n, thr, need), flush=True)
            torch.save(net.state_dict(), os.path.join(HERE, "models", f"{NAME}_{step}.pt"))
    torch.save(net.state_dict(), os.path.join(HERE, "models", f"{NAME}.pt"))
    net.eval().cpu()
    torch.onnx.export(net, torch.zeros(1, 16, 96), os.path.join(HERE, "models", f"{NAME}.onnx"), input_names=["x"], output_names=["logit"],
                      dynamic_axes={"x": {0: "n"}, "logit": {0: "n"}}, opset_version=13, dynamo=False)
    print("saved", NAME, f"({(time.time() - t0) / 60:.1f} min)", flush=True)

if __name__ == "__main__":
    main()
