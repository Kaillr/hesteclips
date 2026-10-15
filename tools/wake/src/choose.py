"""Choose the app's THRESHOLD and NEED for a trained model: the settings
that catch the most held-out takes while false saves per hour of everyday
audio stay under the target. usage: python choose.py <models/x.pt> [target/h]"""
import os, sys
import numpy as np
import torch

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(HERE, "src"))
os.environ.setdefault("ARCH", "mlp")
import evaluate as E

def load(path):
    sys.argv = ["train.py", "x"]
    import train
    net = (train.MLP() if os.environ["ARCH"] == "mlp" else train.Conv()).cuda()
    net.load_state_dict(torch.load(path))
    net.eval()
    def predict(x):
        with torch.no_grad():
            return torch.sigmoid(net(x))
    return net, predict

if __name__ == "__main__":
    path = sys.argv[1]
    target = float(sys.argv[2]) if len(sys.argv) > 2 else 0.3
    net, predict = load(path)
    rep = E.Report()
    sc = rep.score(predict)
    rows = []
    for thr in (0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.85, 0.9, 0.93, 0.95, 0.97):
        for need in range(2, 9):
            n = rep.numbers(sc, thr, need)
            rows.append((n["caught"][0] / n["caught"][1], thr, need, n))
    ok = [r for r in rows if r[3]["fph"] <= target]
    ok.sort(key=lambda r: (-r[0], -r[1]))
    print(f"settings with <= {target} false saves/h, most caught first:")
    for frac, thr, need, n in ok[:12]:
        print("  ", E.Report.line(n, thr, need))
        print("      per source/h:", " ".join(f"{k.replace('.npz', '')} {v:.2f}" for k, v in n["per_source"].items()))
    if not ok:
        print("  none; lowest:", min(rows, key=lambda r: r[3]["fph"])[3]["fph"])
