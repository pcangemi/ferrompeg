# ferrompeg

A small multimedia toolkit written from scratch in pure Rust, with a command
line that follows the familiar `ffmpeg` conventions.

![Terminal demo](docs/demo.gif)

```sh
ferrompeg -i input.wav output.flac
ferrompeg -f lavfi -i "mandelbrot=duration=4" -vf "scale=360:-2" zoom.gif
ferroprobe -show_streams -of json output.flac
```

ferrompeg is not a replacement for FFmpeg and is not affiliated with it. It
covers a deliberately small set of formats, and every one of them is
implemented from its published specification.

## Why

- **Memory safe by construction.** No `unsafe` code. Buffer overflows and
  use-after-free bugs, the most common class of vulnerabilities in media
  parsers, cannot happen here, which matters when you process files you did
  not create.
- **Zero dependencies.** Every container, codec and filter is in this
  repository. No C library to build, no supply chain to audit, one binary of
  about 1 MB that builds anywhere Rust does.
- **Permissive license.** MIT or Apache-2.0, so it can be embedded in any
  product.
- **Readable.** The whole pipeline (demuxer, decoder, filters, encoder, muxer)
  fits in about 26,000 lines that can be read in order. A good base to learn
  how audio and video processing works.
- **Familiar.** Same options as the tool everyone knows: `-i`, `-ss`, `-t`,
  `-af`, `-vf`, `-c copy`, `-f lavfi`, plus `ferroprobe` for inspection.

## Gallery

Both animations were generated and encoded by ferrompeg itself:

| `mandelbrot` source | `life` source |
| --- | --- |
| ![Mandelbrot zoom](docs/mandelbrot.gif) | ![Game of Life](docs/life.gif) |

```sh
ferrompeg -f lavfi -i "mandelbrot=s=480x270:r=12:d=4" -vf "scale=360:-2:flags=area" docs/mandelbrot.gif
ferrompeg -f lavfi -i "life=s=120x68:r=8:d=5" -vf "scale=360:-2:flags=neighbor" docs/life.gif
```

## Install

```sh
cargo install --git https://github.com/pcangemi/ferrompeg
```

This installs two binaries, `ferrompeg` and `ferroprobe`. Rust 1.85 or newer
is required.

## What it supports

| | |
| --- | --- |
| **Audio containers** | WAV (including WAVE_FORMAT_EXTENSIBLE), AIFF and AIFF-C, Sun AU, native FLAC, headerless PCM (`s16le`, `f32be` and 12 other layouts) |
| **Video containers** | YUV4MPEG2 (`.y4m`), headerless raw video, animated GIF (output), PPM, PGM and BMP image sequences (output) |
| **Audio codecs** | FLAC (encoder and decoder), PCM from 8 to 64 bit, integer and float, both byte orders, G.711 A-law and mu-law |
| **Video codecs** | raw video (yuv420p, yuv422p, yuv444p, gray, rgb24), GIF (encoder) |
| **Audio filters** | `volume`, `aresample`, `aformat`, `afade`, `lowpass`, `highpass`, `atempo`, `areverse` |
| **Video filters** | `scale`, `crop`, `pad`, `hflip`, `vflip`, `transpose`, `format`, `fps`, `negate`, `eq`, `boxblur` |
| **Sources** (`-f lavfi`) | `sine`, `anullsrc`, `anoisesrc`, `testsrc`, `smptebars`, `color`, `mandelbrot`, `life` |

Run `ferrompeg -formats`, `-codecs` or `-filters` for the full lists.

## Examples

```sh
# Lossless archive and back, bit for bit
ferrompeg -i recording.wav recording.flac
ferrompeg -i recording.flac -c:a pcm_s24le restored.wav

# Speech recognition input: a 45 second cut, 16 kHz mono, filtered and louder
ferrompeg -i interview.flac -ss 00:01:30 -t 45 -af "highpass=f=80,volume=3dB" -ar 16000 -ac 1 clip.wav

# A 3 minute song 25% faster without changing pitch, fading out at the end
ferrompeg -i song.wav -af "atempo=1.25,afade=t=out:st=140:d=4" faster.wav

# Video frames to images, a crop to a looping GIF
ferrompeg -i clip.y4m -vf "scale=iw/2:-2" -frames:v 10 frame%03d.ppm
ferrompeg -i clip.y4m -vf "crop=320:240,fps=12" -loop 0 clip.gif

# Raw PCM in, FLAC out, through pipes
ferrompeg -f s16le -ar 48000 -ac 2 -i - -f flac - < capture.raw > capture.flac

# Test signals for a CI or a measurement bench
ferrompeg -f lavfi -i "anoisesrc=color=pink:duration=10" pink.wav
ferrompeg -f lavfi -i "smptebars=size=1280x720:duration=5" bars.y4m

# Inspect a file, as JSON for scripts
ferroprobe -show_format -show_streams -of json recording.flac
```

Time values accept seconds (`5`, `1.5`, `250ms`) or `[HH:]MM:SS[.xx]`. Options
placed before `-i` apply to the input, options before an output file apply to
that output, and several outputs can be written in one run.

## How it is tested

- More than 400 unit tests, plus end-to-end tests that run the binaries.
- During development every format was checked against the reference tools,
  used as black boxes: files written by ferrompeg are read back by `ffprobe`,
  `ffmpeg` and `flac -t` with identical samples, and files written by those
  tools are decoded by ferrompeg bit for bit. Small fixtures produced that way
  are committed under `tests/fixtures/`, so the test suite itself needs no
  external tool.
- Malformed input returns an error instead of panicking, and allocations
  driven by header fields are capped.

## Performance

Correct first, fast enough second. Short version: FFmpeg is faster at almost
everything that involves real computation, by 4 to 8 times on codecs, scaling
and GIF, and by about 20 times on resampling and `atempo`. ferrompeg is only
level or ahead on tasks that do almost nothing besides reading and writing,
because it starts in under 3 ms where `ffmpeg` needs 28 ms to load its shared
libraries.

### Timing against FFmpeg

Same commands on both tools, for every task both can do. Apple M1 (8 cores),
macOS 26.6, ferrompeg 0.1.0 release build, FFmpeg 9.0.2 from Homebrew. Median
wall time of 10 runs after one warm-up run, including process start, reading
the input and writing the output to the SSD. Inputs: two minutes of 44.1 kHz
16-bit stereo audio (21 MB WAV) and ten seconds of 1280x720 yuv420p video at
25 fps (346 MB `.y4m`).

| Task | ferrompeg | ffmpeg | ffmpeg, 1 thread | ferrompeg vs ffmpeg |
| --- | ---: | ---: | ---: | --- |
| WAV to FLAC | 1.378 s | 0.255 s | 0.252 s | 5.4x slower |
| FLAC to WAV | 0.266 s | 0.063 s | 0.146 s | 4.2x slower |
| WAV to 24-bit AIFF | 0.035 s | 0.076 s | 0.072 s | 2.2x faster |
| highpass, volume, afade | 0.089 s | 0.104 s | 0.086 s | 1.2x faster |
| FLAC stream copy | 0.034 s | 0.035 s | 0.035 s | same |
| Resample 44.1 to 48 kHz | 3.153 s | 0.135 s | 0.129 s | 23.4x slower |
| atempo=1.25 | 3.529 s | 0.177 s | 0.176 s | 19.9x slower |
| Scale 720p to 360p, bilinear | 0.489 s | 0.080 s | 0.109 s | 6.1x slower |
| Scale 720p to 540p, bicubic | 0.922 s | 0.142 s | 0.256 s | 6.5x slower |
| yuv420p to yuv444p | 0.838 s | 0.714 s | 0.673 s | 1.2x slower |
| crop and hflip | 0.075 s | 0.066 s | 0.065 s | 1.1x slower |
| 720p to 320x180 GIF at 12 fps | 1.033 s | 0.135 s | 0.244 s | 7.7x slower |
| testsrc, 10 s of 720p | 1.604 s | 0.394 s | 0.375 s | 4.1x slower |

How to read it:

- ferrompeg runs on a single thread. The "1 thread" column passes
  `-threads 1 -filter_threads 1` to FFmpeg, which still keeps reading,
  decoding, filtering and writing in separate threads, so it is the closest
  comparison available, not an exact one.
- The tasks where ferrompeg is level or faster are dominated by process start
  and disk access. The conversion to yuv444p writes 690 MB and its ratio moved
  between 1.2x and 2.2x from one run of the benchmark to the next.
- The resampler and `atempo` are the slowest parts and the first targets for
  optimization. FFmpeg owes much of its lead elsewhere to years of tuning and
  hand-written SIMD code; ferrompeg has no SIMD yet.

### Are the outputs the same?

Speed only means something if the results match, so every output above was
compared with FFmpeg's:

| Task | Result |
| --- | --- |
| WAV to FLAC | Lossless (decoded samples identical to the source, `flac -t` passes); 8,414,038 bytes against 8,477,065 for FFmpeg, 0.7% smaller |
| FLAC to WAV | Bit-identical samples |
| Bilinear scale, crop and hflip | Bit-identical to FFmpeg |
| Bicubic scale | 56.6 dB PSNR against FFmpeg's output, visually identical |
| Resample to 48 kHz | 55.9 dB SNR against FFmpeg's output |
| highpass, volume, afade | 78.8 dB SNR against FFmpeg's output |
| atempo | Different algorithm; same length within 5 ms, not comparable sample by sample |
| GIF | Different strategies: ferrompeg builds one palette per frame (2.6 MB, 21.0 dB PSNR against the source), FFmpeg by default uses one fixed palette (1.4 MB, 20.7 dB). FFmpeg's best quality mode, `palettegen` with `paletteuse`, gives 4.6 MB and 21.7 dB in 0.55 s |

Run `python3 bench/compare.py` to reproduce the timings on your machine
(needs `ffmpeg` on the `PATH`).

## Limitations

- No MP3, AAC, Opus, H.264 or other lossy compressed codecs, and no MP4,
  Matroska or Ogg containers. Most consumer media therefore needs another tool.
- One input per run, single source graphs for `-f lavfi`, constant frame rate
  video only, YUV in BT.601 limited range.
- WAV output is limited to 4 GiB (no RF64).

## Roadmap

- MP3 decoder
- MP4 (ISO BMFF) demuxer
- WebAssembly build for in-browser conversion
- SIMD paths for the FLAC encoder, the resampler and the scaler

## Architecture

See [docs/architecture.md](docs/architecture.md) for how the pipeline is put
together and where each format lives.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
