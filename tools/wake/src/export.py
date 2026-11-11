"""Several trained models as the one classifier the app runs: their logits
averaged, in one ONNX graph (input "x" (n, 16, 96), output "logit" (n)).

usage: python export.py <out.onnx> <models/a.pt> [<models/b.pt> ...]  (WIDTH as trained)
"""
import os, sys
import numpy as np
import torch
from torch import nn

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

class Mean(nn.Module):
    def __init__(self, nets):
        super().__init__()
        self.nets = nn.ModuleList(nets)

    def forward(self, x):
        return torch.stack([n(x) for n in self.nets]).mean(0)

if __name__ == "__main__":
    out, paths = sys.argv[1], sys.argv[2:]
    sys.argv = [sys.argv[0]]
    import train
    nets = []
    for p in paths:
        n = train.MLP()
        n.load_state_dict(torch.load(p, map_location="cpu"))
        nets.append(n.eval())
    model = Mean(nets).eval()
    torch.onnx.export(model, torch.zeros(1, 16, 96), out, input_names=["x"], output_names=["logit"],
                      dynamic_axes={"x": {0: "n"}, "logit": {0: "n"}}, opset_version=13, dynamo=False)
    # The ONNX graph gives what torch gives.
    import onnxruntime as ort
    x = np.random.default_rng(0).standard_normal((64, 16, 96)).astype(np.float32)
    a = ort.InferenceSession(out, providers=["CPUExecutionProvider"]).run(None, {"x": x})[0]
    with torch.no_grad():
        b = model(torch.from_numpy(x)).numpy()
    print(f"{out}: {len(nets)} models, {os.path.getsize(out) / 1e6:.1f} MB, max difference {np.abs(a - b).max():.2e}")
