"""One-time model preparation:
  models/mel_weights.npz   the mel graph's weights, for feat.py's batched mel
  kokoro/kokoro-timed.onnx Kokoro with its per-token durations as an output
"""
import os
import onnx
from onnx import numpy_helper
import numpy as np

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
m = onnx.load(os.path.join(HERE, "models", "melspectrogram.onnx"))
w = {i.name: numpy_helper.to_array(i) for i in m.graph.initializer}
np.savez(os.path.join(HERE, "models", "mel_weights.npz"), real=w["0.stft.conv_real.weight"], imag=w["0.stft.conv_imag.weight"], melW=w["1.melW"])
k = onnx.load(os.path.join(HERE, "kokoro", "kokoro-v1.0.onnx"))
k.graph.node.append(onnx.helper.make_node("Identity", ["/encoder/Cast_output_0"], ["duration"]))
k.graph.output.append(onnx.helper.make_tensor_value_info("duration", onnx.TensorProto.INT64, None))
onnx.save(k, os.path.join(HERE, "kokoro", "kokoro-timed.onnx"))
print("done")
