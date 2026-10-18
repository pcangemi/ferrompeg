#!/usr/bin/env python3
"""Times ferrompeg against ffmpeg on the tasks both can do.

Usage: python3 bench/compare.py [--runs N] [--workdir DIR] [case ...]

Builds the release binary first (cargo build --release), generates the
inputs with ffmpeg, then runs every case once to warm the caches and N more
times, and prints the median wall time as a Markdown table. Times include
process start, reading the input and writing the output.
"""

import argparse
import os
import shutil
import statistics
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FERROMPEG = os.path.join(ROOT, "target", "release", "ferrompeg")
FFMPEG = shutil.which("ffmpeg") or "ffmpeg"

QUIET = ["-hide_banner", "-loglevel", "error", "-nostats", "-y"]

# A two minute, 44.1 kHz stereo signal with tones, envelopes and pink noise,
# and ten seconds of 720p video with grain.
MUSIC = (
    "aevalsrc='0.25*sin(2*PI*220*t)*(0.6+0.4*sin(2*PI*0.5*t))"
    "+0.15*sin(2*PI*277.18*t+2*sin(3*t))+0.1*sin(2*PI*1760*t)*exp(-mod(t,0.5)*8)"
    "|0.25*sin(2*PI*329.63*t)*(0.6+0.4*cos(2*PI*0.3*t))"
    "+0.15*sin(2*PI*440*t)+0.1*sin(2*PI*1320*t)*exp(-mod(t+0.25,0.5)*8)'"
    ":s=44100:d=120"
)
NOISE = "anoisesrc=c=pink:a=0.04:r=44100:d=120:seed=1"
CLIP = "testsrc2=s=1280x720:r=25:d=10,noise=alls=12:allf=t:all_seed=1"

# (name, label, input args, output args, output extension)
CASES = [
    ("flac_enc", "WAV to FLAC", "-i music.wav", "", "flac"),
    ("flac_dec", "FLAC to WAV", "-i music.flac", "", "wav"),
    ("aiff", "WAV to 24-bit AIFF", "-i music.wav", "-c:a pcm_s24be", "aiff"),
    ("filters", "highpass, volume, afade", "-i music.wav",
     "-af highpass=f=80,volume=3dB,afade=t=out:st=110:d=10", "wav"),
    ("copy", "FLAC stream copy", "-i music.flac", "-c copy", "flac"),
    ("resample", "Resample 44.1 to 48 kHz", "-i music.wav", "-ar 48000", "wav"),
    ("atempo", "atempo=1.25", "-i music.wav", "-af atempo=1.25", "wav"),
    ("bilinear", "Scale 720p to 360p, bilinear", "-i clip.y4m",
     "-vf scale=640:360:flags=bilinear", "y4m"),
    ("bicubic", "Scale 720p to 540p, bicubic", "-i clip.y4m",
     "-vf scale=960:540:flags=bicubic", "y4m"),
    ("yuv444p", "yuv420p to yuv444p", "-i clip.y4m", "-pix_fmt yuv444p", "y4m"),
    ("crop", "crop and hflip", "-i clip.y4m", "-vf crop=640:360,hflip", "y4m"),
    ("gif", "720p to 320x180 GIF at 12 fps", "-i clip.y4m", "-vf scale=320:-2,fps=12", "gif"),
    ("testsrc", "testsrc, 10 s of 720p", "-f lavfi -i testsrc=s=1280x720:r=25:d=10",
     "-pix_fmt yuv420p", "y4m"),
]

TOOLS = {
    "ferrompeg": lambda i, o: [FERROMPEG, *QUIET, *i, *o],
    "ferrompeg_1t": lambda i, o: [FERROMPEG, *QUIET, "-threads", "1", *i, *o],
    "ffmpeg": lambda i, o: [FFMPEG, "-nostdin", *QUIET, *i, *o],
    "ffmpeg_1t": lambda i, o: [FFMPEG, "-nostdin", *QUIET, "-filter_threads", "1",
                               "-threads", "1", *i, "-threads", "1", *o],
}


def run(cmd, cwd):
    start = time.perf_counter()
    result = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True)
    elapsed = time.perf_counter() - start
    if result.returncode != 0:
        sys.exit(f"failed: {' '.join(cmd)}\n{result.stderr}")
    return elapsed


def prepare(workdir):
    os.makedirs(os.path.join(workdir, "out"), exist_ok=True)
    steps = [
        ("music.wav", ["-f", "lavfi", "-i", MUSIC, "-f", "lavfi", "-i", NOISE, "-filter_complex",
                       "[1]aformat=channel_layouts=stereo[n];[0][n]amix=inputs=2:normalize=0",
                       "-c:a", "pcm_s16le", "music.wav"]),
        ("music.flac", ["-i", "music.wav", "music.flac"]),
        ("clip.y4m", ["-f", "lavfi", "-i", CLIP, "-pix_fmt", "yuv420p", "clip.y4m"]),
    ]
    for name, args in steps:
        if not os.path.exists(os.path.join(workdir, name)):
            run([FFMPEG, "-nostdin", *QUIET, *args], workdir)


def format_ratio(ours, theirs):
    if ours >= theirs * 1.1:
        return f"{ours / theirs:.1f}x slower"
    if theirs >= ours * 1.1:
        return f"{theirs / ours:.1f}x faster"
    return "same"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--runs", type=int, default=10)
    parser.add_argument("--workdir", default=os.path.join(ROOT, "target", "bench"))
    parser.add_argument("cases", nargs="*")
    args = parser.parse_args()

    subprocess.run(["cargo", "build", "--release", "--quiet"], cwd=ROOT, check=True)
    prepare(args.workdir)

    print("| Task | ferrompeg | ffmpeg | ferrompeg vs ffmpeg | ferrompeg, 1 thread "
          "| ffmpeg, 1 thread |")
    print("| --- | ---: | ---: | --- | ---: | ---: |")
    for name, label, inp, outp, ext in CASES:
        if args.cases and name not in args.cases:
            continue
        medians = {}
        for tool, build in TOOLS.items():
            dst = os.path.join("out", f"{name}.{tool}.{ext}")
            cmd = build(inp.split(), [*outp.split(), dst])
            run(cmd, args.workdir)
            medians[tool] = statistics.median(run(cmd, args.workdir) for _ in range(args.runs))
        ours, theirs = medians["ferrompeg"], medians["ffmpeg"]
        print(f"| {label} | {ours:.3f} s | {theirs:.3f} s | {format_ratio(ours, theirs)} | "
              f"{medians['ferrompeg_1t']:.3f} s | {medians['ffmpeg_1t']:.3f} s |", flush=True)


if __name__ == "__main__":
    main()
