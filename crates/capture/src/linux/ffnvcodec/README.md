NVIDIA's NVENC, NVDEC and CUDA driver API headers, with a loader that opens
the driver's libraries at run time, as FFmpeg's nv-codec-headers packages
them: tag n11.1.5.3 (commit 43d91706e097565f57b311e567f0219838bcc2f6) of
https://github.com/FFmpeg/nv-codec-headers, unchanged. MIT licensed (see
each file's notice).

API 11.1, which NVIDIA's drivers from 470.57 on take, so GTX 10-series
cards (whose last driver is 580) can use it. Newer headers raise that: 13.1
needs 610.
