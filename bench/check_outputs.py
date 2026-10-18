#!/usr/bin/env python3
"""Compares the files written by bench/compare.py with FFmpeg's.

Usage: python3 bench/check_outputs.py [--workdir DIR]

Run bench/compare.py first. Prints a Markdown table: whether the FLAC files
are lossless, PSNR between the pictures of both tools, SNR between their
audio, and sizes and quality of the GIFs against the source.
"""

import argparse
import array
import hashlib
import math
import os
import re
import shutil
import statistics
import subprocess
import time
import wave

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FFMPEG = shutil.which("ffmpeg") or "ffmpeg"
QUIET = ["-nostdin", "-hide_banner", "-loglevel", "error", "-y"]


def ffmpeg(*args, cwd):
    return subprocess.run([FFMPEG, *QUIET, *args], cwd=cwd, capture_output=True, check=True)


def pcm_md5(path, cwd):
    out = ffmpeg("-i", path, "-f", "s16le", "-", cwd=cwd).stdout
    return hashlib.md5(out).hexdigest()


def psnr(a, b, cwd):
    """Average PSNR between two videos, 'inf' when identical."""
    result = subprocess.run(
        [FFMPEG, "-nostdin", "-hide_banner", "-i", a, "-i", b,
         "-lavfi", "[0]format=rgb24[x];[1]format=rgb24[y];[x][y]psnr", "-f", "null", "-"],
        cwd=cwd, capture_output=True, text=True, check=True)
    return re.search(r"average:(\S+)", result.stderr).group(1)


def samples(path):
    with wave.open(path) as w:
        return array.array("h", w.readframes(w.getnframes()))


def snr(a, b):
    """Signal to difference ratio of `a` against reference `b`, in dB."""
    n = min(len(a), len(b))
    signal = sum(v * v for v in b[:n])
    error = sum((a[i] - b[i]) ** 2 for i in range(n))
    return math.inf if error == 0 else 10 * math.log10(signal / error)


def describe_psnr(value):
    return "bit-identical" if value == "inf" else f"{float(value):.1f} dB PSNR"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--workdir", default=os.path.join(ROOT, "target", "bench"))
    args = parser.parse_args()
    wd = args.workdir
    out = lambda name, tool, ext: os.path.join("out", f"{name}.{tool}.{ext}")
    size = lambda path: os.path.getsize(os.path.join(wd, path))
    rows = []

    ours, theirs = out("flac_enc", "ferrompeg", "flac"), out("flac_enc", "ffmpeg", "flac")
    lossless = pcm_md5(ours, wd) == pcm_md5("music.wav", wd)
    if shutil.which("flac"):
        lossless &= subprocess.run(["flac", "-t", "-s", ours], cwd=wd).returncode == 0
    rows.append(("WAV to FLAC",
                 f"{'Lossless' if lossless else 'NOT LOSSLESS'}; {size(ours):,} bytes against "
                 f"{size(theirs):,} for FFmpeg ({(size(ours) / size(theirs) - 1) * 100:+.1f}%)"))

    same = pcm_md5(out("flac_dec", "ferrompeg", "wav"), wd) == pcm_md5(out("flac_dec", "ffmpeg", "wav"), wd)
    rows.append(("FLAC to WAV", "bit-identical samples" if same else "DIFFERENT samples"))

    for name, label in [("bilinear", "Bilinear scale"), ("bicubic", "Bicubic scale"),
                        ("crop", "crop and hflip"), ("yuv444p", "yuv420p to yuv444p")]:
        value = psnr(out(name, "ferrompeg", "y4m"), out(name, "ffmpeg", "y4m"), wd)
        rows.append((label, f"{describe_psnr(value)} against FFmpeg's output"))

    for name, label in [("resample", "Resample to 48 kHz"), ("filters", "highpass, volume, afade")]:
        a = samples(os.path.join(wd, out(name, "ferrompeg", "wav")))
        b = samples(os.path.join(wd, out(name, "ffmpeg", "wav")))
        rows.append((label, f"{snr(a, b):.1f} dB SNR against FFmpeg's output"))

    a = samples(os.path.join(wd, out("atempo", "ferrompeg", "wav")))
    b = samples(os.path.join(wd, out("atempo", "ffmpeg", "wav")))
    gap_ms = abs(len(a) - len(b)) / 2 / 44100 * 1000
    rows.append(("atempo", f"Different algorithm; lengths differ by {gap_ms:.1f} ms"))

    # GIFs against the source scaled the same way, and FFmpeg's two pass
    # palette mode for reference.
    ffmpeg("-i", "clip.y4m", "-vf", "scale=320:-2,fps=12", "-c:v", "rawvideo", "-pix_fmt", "rgb24",
           "out/gif_ref.nut", cwd=wd)
    palette = ["-i", "clip.y4m", "-filter_complex",
               "scale=320:-2,fps=12,split[a][b];[a]palettegen[p];[b][p]paletteuse", "out/gif.palette.gif"]
    ffmpeg(*palette, cwd=wd)
    times = []
    for _ in range(5):
        start = time.perf_counter()
        ffmpeg(*palette, cwd=wd)
        times.append(time.perf_counter() - start)
    parts = []
    for tool, label in [("ferrompeg", "ferrompeg"), ("ffmpeg", "FFmpeg default"),
                        ("palette", "FFmpeg palettegen")]:
        path = out("gif", tool, "gif")
        parts.append(f"{label} {size(path) / 1e6:.2f} MB, "
                     f"{float(psnr(path, 'out/gif_ref.nut', wd)):.1f} dB")
    rows.append(("GIF (size, PSNR against the source)",
                 "; ".join(parts) + f"; palettegen takes {statistics.median(times):.2f} s"))

    print("| Task | Result |")
    print("| --- | --- |")
    for task, result in rows:
        print(f"| {task} | {result} |")


if __name__ == "__main__":
    main()
