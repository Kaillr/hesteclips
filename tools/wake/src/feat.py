"""openWakeWord features exactly as the app computes them live (voice.rs):
every 80 ms (1280 samples) the mel spectrogram of the last 1280 + 480
samples is appended to a buffer, then one embedding is made from the
buffer's last 76 mel frames. Done here for a whole recording at once, in
batches, on the GPU (TF32 off: then it matches the CPU to ~1e-4).

stream_embed(x) -> (chunks, 96) float32: row k is what the app has after
hearing samples [0, 1280 (k + 1)).
"""
import os
import numpy as np

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
HOP, CTX, MEL_FRAMES = 1280, 480, 76

_s = {}
def sessions(device=None):
    device = device or os.environ.get("FEAT_DEVICE", "gpu")
    if device not in _s:
        if device == "gpu":
            import torch  # noqa: F401  its CUDA libraries, for onnxruntime-gpu
        import onnxruntime as ort
        ort.set_default_logger_severity(3)
        so = ort.SessionOptions()
        so.intra_op_num_threads = int(os.environ.get("FEAT_THREADS", "1"))
        so.inter_op_num_threads = 1
        prov = [("CUDAExecutionProvider", {"use_tf32": 0}), "CPUExecutionProvider"] if device == "gpu" else ["CPUExecutionProvider"]
        m = os.path.join(HERE, "models")
        _s[device] = (ort.InferenceSession(os.path.join(m, "melspectrogram.onnx"), so, providers=prov),
                      ort.InferenceSession(os.path.join(m, "embedding_model.onnx"), so, providers=prov))
    return _s[device]

_torch_mel = None
def mel_chunks(chunks):
    """melspectrogram.onnx run on each (1760-sample) chunk by itself, for a
    batch of chunks at once. The ONNX model floors the log mel at 80 dB below
    the loudest value of its whole input, so a batched run would mix chunks;
    this is the same graph with the floor per chunk. (n, 1760) -> (n, 8, 32)."""
    global _torch_mel
    import torch
    if _torch_mel is None:
        torch.backends.cudnn.allow_tf32 = False
        torch.backends.cuda.matmul.allow_tf32 = False
        w = np.load(os.path.join(HERE, "models", "mel_weights.npz"))
        dev = "cuda" if torch.cuda.is_available() else "cpu"
        _torch_mel = {k: torch.from_numpy(w[k]).to(dev) for k in ("real", "imag", "melW")}, dev
    w, dev = _torch_mel
    with torch.no_grad():
        x = torch.from_numpy(np.ascontiguousarray(chunks, dtype=np.float32)).to(dev)[:, None]
        re = torch.nn.functional.conv1d(x, w["real"], stride=160)
        im = torch.nn.functional.conv1d(x, w["imag"], stride=160)
        mel = (re ** 2 + im ** 2).transpose(1, 2) @ w["melW"]  # (n, F, 32)
        db = torch.log(mel.clamp(min=1e-10)) * 10 / 2.3025851
        db = torch.maximum(db, db.amax(dim=(1, 2), keepdim=True) - 80)
        return db.cpu().numpy()

def stream_embed(x, device=None, batch=4096):
    """x: int16 (or float in int16 scale) mono 16 kHz."""
    _, emb_s = sessions(device)
    x = np.asarray(x, dtype=np.float32)
    k = len(x) // HOP
    if k == 0:
        return np.zeros((0, 96), np.float32)
    # Chunk k hears samples [1280 k - 480, 1280 (k + 1)); before the start: silence.
    xp = np.concatenate([np.zeros(CTX, np.float32), x[: k * HOP]])
    idx = np.arange(k)[:, None] * HOP + np.arange(HOP + CTX)[None, :]
    mel = np.concatenate([mel_chunks(xp[idx[i:i + batch]]) for i in range(0, k, batch)]) / 10 + 2  # (k, F, 32)
    f = mel.shape[1]
    buf = mel.reshape(-1, 32)
    # Embedding after chunk j: the buffer's last 76 frames, ending at (j + 1) f.
    first = -(-MEL_FRAMES // f) - 1  # first chunk with 76 frames of buffer
    ends = (np.arange(k) + 1) * f
    ok = ends >= MEL_FRAMES
    win = np.lib.stride_tricks.sliding_window_view(buf, (MEL_FRAMES, 32))[:, 0]  # (n - 75, 76, 32)
    sel = ends[ok] - MEL_FRAMES
    out = np.zeros((k, 96), np.float32)
    rows = []
    for i in range(0, len(sel), batch):
        w = np.ascontiguousarray(win[sel[i:i + batch]])[..., None]
        rows.append(emb_s.run(None, {"input_1": w})[0].reshape(-1, 96))
    out[ok] = np.concatenate(rows)
    out[~ok] = out[ok][0]
    return out

# The window the classifier sees: the last 16 rows. Row k ends at sample
# 1280 (k + 1); its embedding hears the last 76 mel frames (~0.775 s).
WINDOW = 16

def row_end(k):
    """Time (s) of the last sample heard when row k was made."""
    return HOP * (k + 1) / 16000
