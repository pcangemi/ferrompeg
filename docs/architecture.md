# Architecture

ferrompeg follows the classic media pipeline. Each stage is a trait, and
every format, codec and filter is one implementation of it.

```text
            packets             frames                 frames            packets
 Demuxer ------------> Decoder --------> Filter chain --------> Encoder ---------> Muxer
 (format/)             (codec/)          (filter/)     convert/ (codec/)            (format/)

 Source (source/) ----------------------> frames
```

## Data model

- `frame.rs`: decoded media. `AudioFrame` holds interleaved samples as either
  sign-extended integers with their bit depth, or floats in -1.0 to 1.0.
  `VideoFrame` holds tightly packed planes in one of five pixel formats.
- `stream.rs`: `StreamInfo` (codec, parameters, time base, duration,
  extradata) and `Packet` (compressed bytes with a timestamp).
- `rational.rs`: exact fractions for time bases and frame rates.

Two conventions keep timestamp math trivial everywhere:

- audio uses a time base of `1 / sample_rate`, so `pts` counts samples;
- video is constant frame rate, the time base is one frame, so `pts` counts
  frames.

## Stages

| Trait | Module | Role |
| --- | --- | --- |
| `Demuxer` | `format/` | Splits an input into packets and describes its streams |
| `Decoder` | `codec/` | Packets to frames |
| `Filter` | `filter/` | Frames to frames; filters adapt to whatever format reaches them |
| `Encoder` | `codec/` | Frames to packets; declares the exact input format it needs |
| `Muxer` | `format/` | Writes packets, then patches sizes or checksums in the trailer |
| `Source` | `source/` | Generates frames directly (`-f lavfi`) |

Between the last filter and the encoder, the pipeline inserts the
conversions the encoder asks for: `convert/audio.rs` (sample kind, channel
remix, resampling) and `convert/video.rs` (pixel format and scaling).

Encoders are created lazily, from the format of the first frame that comes
out of the filter chain, so `-vf scale=...` or `-af aresample=...` decide
the output parameters without any negotiation step. A muxer writes its
header once every encoder of its output is ready.

## Parallelism

`util/parallel.rs` spreads independent work over the cores with scoped
threads from the standard library, no thread pool and no `unsafe`. Work is
cut into more chunks than there are threads and each thread takes the next
free chunk, which keeps fast cores busy on processors that mix fast and
efficient ones. Each chunk writes only its own part of the output, so
results never depend on the thread count (`-threads N`, one per core by
default); `tests/cli.rs` checks this end to end.

| Where | Unit of work |
| --- | --- |
| `convert/video.rs` | Bands of output rows, all planes of a picture in one batch |
| `codec/flac/encoder.rs` | Batches of 32 blocks, coded in parallel and written in order |
| `codec/flac/decoder.rs` | Batches of 32 frames; the MD5 signature is still checked in order |
| `codec/gif.rs` | Batches of pictures: each depends only on the source pixels of the previous one and on the first picture's color table |

Starting a set of threads costs about 70 microseconds, so batches are sized
to keep that small next to the work they carry.

## Registries

- `format/mod.rs` lists every `InputFormat` (with a content probe) and
  `OutputFormat` (with its default codecs and allowed pixel formats).
- `codec/mod.rs` maps a `CodecId` to its decoder and encoder.
- `filter/mod.rs` and `source/mod.rs` list filters and generators by name.

Adding a format means writing the module and adding one line to the
matching registry.

## Command line

- `cli/options.rs` parses arguments with the usual positional rule: options
  apply to the next `-i` input or the next output file.
- `cli/pipeline.rs` runs the loop: read, decode, trim, filter, convert,
  encode, mux, with stream copy (`-c copy`), several outputs per run and a
  progress line.
- `cli/probe.rs` is `ferroprobe`.

## Utilities

`util/` holds the shared low level pieces: an MSB-first bit reader and
writer, CRC-8 and CRC-16 tables built at compile time, MD5 (for the FLAC
signature), byte readers for both endiannesses, and time parsing.

## Specifications

Each module names the specification it implements in its doc comment, for
example RFC 9639 for FLAC, GIF89a for GIF, the AIFF 1.3 and AIFF-C documents,
ITU-T G.711 and ITU-R BT.601.
