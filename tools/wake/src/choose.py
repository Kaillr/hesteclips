"""Choose the app's THRESHOLD and NEED for a model: the settings that catch
the most of the users' held-out takes (then held-out synthetic voices)
while false saves per hour of everyday audio stay under the target and the
users' held-out "ignore" clips stay quiet.

usage: python choose.py <model> [<model> ...] [--target 0.3]
  model: models/x.pt (WIDTH as trained) or any .onnx classifier (the app's
  own hesteclip.onnx too). Several are averaged (their logits), as one
  model made of them would.
"""
import os, sys
import numpy as np
import torch

HERE = os.environ.get("WAKE_HOME") or os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import evaluate as E

def logit_fn(path):
    if path.endswith(".onnx"):
        import onnxruntime as ort
        ort.set_default_logger_severity(3)
        s = ort.InferenceSession(path, providers=["CUDAExecutionProvider", "CPUExecutionProvider"])
        name, shape = s.get_inputs()[0].name, s.get_inputs()[0].shape
        fixed = isinstance(shape[0], int)
        def f(x):
            a = x.float().cpu().numpy()
            if fixed:  # a model made for one window at a time
                out = np.concatenate([s.run(None, {name: a[i:i + 1]})[0].reshape(-1) for i in range(len(a))])
            else:
                out = np.concatenate([s.run(None, {name: a[i:i + 8192]})[0].reshape(-1) for i in range(0, len(a), 8192)])
            return torch.from_numpy(out).to(x.device)
        return f
    sys.argv = [sys.argv[0]]
    import train
    net = train.MLP().cuda()
    net.load_state_dict(torch.load(path))
    net.eval()
    def f(x):
        with torch.no_grad():
            return net(x)
    return f

def predictor(paths):
    fns = [logit_fn(p) for p in paths]
    def predict(x):
        return torch.sigmoid(sum(f(x) for f in fns) / len(fns))
    return predict

THRESHOLDS = (0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.85, 0.9, 0.93, 0.95, 0.97, 0.98, 0.99)

def table(predict, target, show=12):
    rep = E.Report()
    sc = rep.score(predict)
    rows = []
    for thr in THRESHOLDS:
        for need in range(2, 9):
            n = rep.numbers(sc, thr, need)
            real = n["real"]["_clean"]
            rows.append((real[0] / max(real[1], 1), n["real"][""][0] / max(n["real"][""][1], 1), n["caught"][0] / max(n["caught"][1], 1), thr, need, n))
    ok = [r for r in rows if r[5]["fph"] <= target]
    ok.sort(key=lambda r: (-r[0], -r[1], -r[2], r[5]["ign"]["_clean"][0], -r[3]))
    print(f"settings with <= {target} false saves/h, most of the users' takes caught first:")
    for *_, thr, need, n in ok[:show]:
        print("  ", E.Report.line(n, thr, need))
    if not ok:
        print("  none; lowest:", min(rows, key=lambda r: r[5]["fph"])[5]["fph"])
    for thr, need in ((0.8, 4), (0.9, 4), (0.95, 4)):
        print("   at", E.Report.line(rep.numbers(sc, thr, need), thr, need))
    return rows

if __name__ == "__main__":
    args = sys.argv[1:]
    target = 0.3
    if "--target" in args:
        i = args.index("--target")
        target = float(args[i + 1])
        del args[i:i + 2]
    table(predictor(args), target)
