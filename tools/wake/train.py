"""Train the "hashtag HesteClip that" detector on openWakeWord features.

usage: python train.py [steps] [name]
"""
import os, sys, time
import numpy as np
import torch
from torch import nn

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from gen import low_priority
import evaluate

STEPS = int(sys.argv[1]) if len(sys.argv) > 1 else 30000
NAME = sys.argv[2] if len(sys.argv) > 2 else "wake"
dev = "cuda"
DROPOUT = float(os.environ.get("DROPOUT", "0.3"))
# Every this many steps a third of the everyday-audio pool is swapped for
# fresh windows, so training sees most of the 2000 hours instead of
# memorizing one sample of it.
REFRESH = int(os.environ.get("REFRESH", "2000"))

class Net(nn.Module):
    def __init__(self, width=128):
        super().__init__()
        self.f = nn.Sequential(
            nn.Flatten(),
            nn.Linear(16 * 96, width), nn.LayerNorm(width), nn.ReLU(), nn.Dropout(DROPOUT),
            nn.Linear(width, width), nn.LayerNorm(width), nn.ReLU(), nn.Dropout(DROPOUT),
            nn.Linear(width, 1),
        )

    def forward(self, x):
        return self.f(x).squeeze(-1)

def load(name):
    return torch.from_numpy(np.load(os.path.join(HERE, os.environ.get("FEATS_DIR", "feats"), f"{name}.npy")).astype(np.float16)).to(dev)

ACAV = None
def acav_sample(n_blocks=12, block=100_000, seed=0):
    global ACAV
    if ACAV is None:
        ACAV = np.load(os.path.join(HERE, "data", "acav_2000h.npy"), mmap_mode="r")
    rng = np.random.default_rng(seed)
    starts = rng.choice(len(ACAV) - block, n_blocks, replace=False)
    return torch.from_numpy(np.concatenate([np.asarray(ACAV[s:s + block]) for s in sorted(starts)])).to(dev)

def main():
    low_priority()
    torch.manual_seed(0)
    t0 = time.time()
    pos, neg, cut, noise = load("pos"), load("neg"), load("cut"), load("noise")
    acav = acav_sample(n_blocks=int(os.environ.get("ACAV_BLOCKS", "12")))
    print(f"data: pos {len(pos)}, neg {len(neg)}, cut {len(cut)}, noise {len(noise)}, acav {len(acav)} ({time.time() - t0:.0f} s)", flush=True)
    net = Net().to(dev)
    opt = torch.optim.AdamW(net.parameters(), lr=1e-3, weight_decay=float(os.environ.get("WD", "0.01")))
    sched = torch.optim.lr_scheduler.OneCycleLR(opt, max_lr=1e-3, total_steps=STEPS, pct_start=0.1)
    bce = nn.BCEWithLogitsLoss(reduction="none")

    def pick(t, n):
        return t[torch.randint(len(t), (n,), device=dev)].float()

    def predict(x):
        net.eval()
        out = []
        with torch.no_grad():
            for i in range(0, len(x), 8192):
                out.append(torch.sigmoid(net(torch.from_numpy(x[i:i + 8192]).to(dev))).cpu().numpy())
        net.train()
        return np.concatenate(out) if out else np.zeros(0)

    blocks = int(os.environ.get("ACAV_BLOCKS", "12"))
    for step in range(1, STEPS + 1):
        if step % REFRESH == 0 and blocks >= 3:
            third = len(acav) // 3
            k = (step // REFRESH) % 3
            acav[k * third:(k + 1) * third] = acav_sample(blocks // 3, seed=step)[:third]
        x = torch.cat([pick(pos, 512), pick(neg, 256), pick(cut, 192), pick(noise, 64), pick(acav, 1024)])
        y = torch.cat([torch.ones(512, device=dev), torch.zeros(512 + 1024, device=dev)])
        # Everyday audio counts more as training goes on: fewer false saves.
        ramp = 1 + 19 * min(step / (0.7 * STEPS), 1.0)
        w = torch.cat([torch.ones(512, device=dev), torch.full((512,), 3.0, device=dev), torch.full((1024,), ramp, device=dev)])
        # A little jitter on the features themselves.
        x = x + 0.05 * x.std() * torch.randn_like(x)
        loss = (bce(net(x), y) * w).sum() / w.sum()
        opt.zero_grad()
        loss.backward()
        opt.step()
        sched.step()
        if step % int(os.environ.get("REPORT_EVERY", "5000")) == 0 or step == STEPS:
            print(f"step {step} loss {loss.item():.4f} ({time.time() - t0:.0f} s)", flush=True)
            print("   ", evaluate.report(predict, 0.5), flush=True)

    torch.save(net.state_dict(), os.path.join(HERE, f"{NAME}.pt"))
    for thr in (0.3, 0.5, 0.7, 0.8, 0.9, 0.95):
        print("   ", evaluate.report(predict, thr), flush=True)
    net.eval().cpu()
    torch.onnx.export(net, torch.zeros(1, 16, 96), os.path.join(HERE, f"{NAME}.onnx"), input_names=["x"], output_names=["logit"], dynamic_axes={"x": {0: "n"}, "logit": {0: "n"}}, opset_version=13)
    print("saved", NAME, flush=True)

if __name__ == "__main__":
    main()
