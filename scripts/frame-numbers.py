#!/usr/bin/env python3
"""Clips that carry their own frame numbers, to check exports frame by frame.

  frame-numbers.py make <out.mp4> [--size 2560x1440] [--fps 60] [--seconds 60] [--gop 120]
  frame-numbers.py read <video>

`make` draws each frame's number as 12 squares (white is a 1 bit) on black,
with a 440 Hz tone, encoded by ffmpeg (NVENC where it's there, else x264).
`read` prints which source frames a video shows, as runs ("180..599"), and
how many frames it has: an export of parts 3-10 s, 17.5-25.25 s, 31-40 s of
a 60 fps clip should read "180..599 1050..1514 1860..2399 (1425 frames)".
The squares survive any scaling and a lot of compression. Needs ffmpeg.
"""

import argparse
import subprocess
import sys

# The squares on a 640x360 frame (scaled with the frame): bit k at column
# k % 6, row k // 6, 80 px wide.
W, H, BITS = 640, 360, 12


def square(k):
    return 40 + (k % 6) * 95, 60 + (k // 6) * 130


def make(args):
    w, h = (int(v) for v in args.size.split("x"))
    n = round(args.fps * args.seconds)
    encoder = ["-c:v", "libx264", "-preset", "veryfast", "-crf", "18", "-bf", "0"]
    if subprocess.run(["ffmpeg", "-hide_banner", "-encoders"], capture_output=True, text=True).stdout.find("h264_nvenc") >= 0:
        encoder = ["-c:v", "h264_nvenc", "-preset", "p4", "-bf", "0", "-b:v", "20M"]
    cmd = ["ffmpeg", "-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "yuv420p", "-s", f"{W}x{H}", "-r", str(args.fps), "-i", "-",
           "-f", "lavfi", "-i", f"sine=frequency=440:sample_rate=48000:duration={args.seconds}",
           "-vf", f"scale={w}:{h}:flags=neighbor", *encoder, "-g", str(args.gop), "-pix_fmt", "yuv420p",
           "-c:a", "aac", "-b:a", "96k", "-shortest", args.out]
    ffmpeg = subprocess.Popen(cmd, stdin=subprocess.PIPE)
    chroma = bytes([128]) * (W // 2 * H // 2)
    for f in range(n):
        y = bytearray(b"\x10" * (W * H))
        for k in range(BITS):
            if f >> k & 1:
                x0, y0 = square(k)
                for r in range(y0, y0 + 80):
                    y[r * W + x0:r * W + x0 + 80] = b"\xeb" * 80
        ffmpeg.stdin.write(y)
        ffmpeg.stdin.write(chroma)
        ffmpeg.stdin.write(chroma)
    ffmpeg.stdin.close()
    sys.exit(ffmpeg.wait())


def read(args):
    sw, sh = 64, 36
    raw = subprocess.run(["ffmpeg", "-v", "error", "-i", args.video, "-vf", f"scale={sw}:{sh}:flags=area", "-pix_fmt", "gray", "-f", "rawvideo", "-"],
                         capture_output=True, check=True).stdout
    numbers = []
    for f in range(len(raw) // (sw * sh)):
        frame = raw[f * sw * sh:(f + 1) * sw * sh]
        n = 0
        for k in range(BITS):
            x0, y0 = square(k)
            if frame[(y0 + 40) * sh // H * sw + (x0 + 40) * sw // W] > 128:
                n |= 1 << k
        numbers.append(n)
    runs, start = [], None
    for i, n in enumerate(numbers):
        if start is None:
            start = n
        elif n != numbers[i - 1] + 1:
            runs.append(f"{start}..{numbers[i - 1]}")
            start = n
    if start is not None:
        runs.append(f"{start}..{numbers[-1]}")
    print(f"{' '.join(runs)} ({len(numbers)} frames)")


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="what", required=True)
    m = sub.add_parser("make")
    m.add_argument("out")
    m.add_argument("--size", default="2560x1440")
    m.add_argument("--fps", type=float, default=60)
    m.add_argument("--seconds", type=float, default=60)
    m.add_argument("--gop", type=int, default=120)
    r = sub.add_parser("read")
    r.add_argument("video")
    args = p.parse_args()
    make(args) if args.what == "make" else read(args)


if __name__ == "__main__":
    main()
