//! Headerless PCM (`-f s16le -ar 44100 -ac 2`). One demuxer and one muxer
//! serve every layout; the format name selects the codec.
//!
//! The packet reader used here, `PcmPackets`, is shared with the AIFF and
//! AU demuxers: once their header is parsed, their payload is the same
//! stream of interleaved sample frames.

use std::io::{Read, Write};

use crate::error::{Error, Result};
use crate::format::{
    Demuxer, InputFormat, InputOptions, Muxer, OutputFormat, OutputOptions, probe_never,
};
use crate::io::Output;
use crate::rational::Rational;
use crate::stream::{AudioParams, CodecId, Packet, StreamInfo, StreamParams};
use crate::util::bytes::ReadBytes;

/// Sample frames (one sample per channel) in each packet.
const PACKET_FRAMES: usize = 4096;

/// Upper bound on the bytes of one packet. Channel counts come from file
/// headers, so without this a hostile header could make every packet
/// allocation enormous; the bound only shortens packets of streams with
/// very many channels.
const MAX_PACKET_BYTES: usize = 1 << 20;

const DEFAULT_SAMPLE_RATE: u32 = 44100;
const DEFAULT_CHANNELS: u16 = 1;

const fn input(name: &'static str, long_name: &'static str, codec: CodecId) -> InputFormat {
    InputFormat {
        name,
        long_name,
        extensions: &[],
        probe: probe_never,
        open,
        raw_codec: Some(codec),
    }
}

const fn output(name: &'static str, long_name: &'static str, codec: CodecId) -> OutputFormat {
    // A single codec per layout: the format name already says how the bytes
    // are laid out, so there is nothing to choose.
    let codecs: &'static [CodecId] = match codec {
        CodecId::PcmU8 => &[CodecId::PcmU8],
        CodecId::PcmS8 => &[CodecId::PcmS8],
        CodecId::PcmS16Le => &[CodecId::PcmS16Le],
        CodecId::PcmS16Be => &[CodecId::PcmS16Be],
        CodecId::PcmS24Le => &[CodecId::PcmS24Le],
        CodecId::PcmS24Be => &[CodecId::PcmS24Be],
        CodecId::PcmS32Le => &[CodecId::PcmS32Le],
        CodecId::PcmS32Be => &[CodecId::PcmS32Be],
        CodecId::PcmF32Le => &[CodecId::PcmF32Le],
        CodecId::PcmF32Be => &[CodecId::PcmF32Be],
        CodecId::PcmF64Le => &[CodecId::PcmF64Le],
        CodecId::PcmF64Be => &[CodecId::PcmF64Be],
        CodecId::PcmAlaw => &[CodecId::PcmAlaw],
        CodecId::PcmMulaw => &[CodecId::PcmMulaw],
        _ => &[],
    };
    OutputFormat {
        name,
        long_name,
        extensions: &[],
        audio_codec: Some(codec),
        video_codec: None,
        codecs,
        pixel_formats: &[],
        create,
    }
}

pub static INPUTS: &[InputFormat] = &[
    input("u8", "PCM unsigned 8-bit", CodecId::PcmU8),
    input("s8", "PCM signed 8-bit", CodecId::PcmS8),
    input(
        "s16le",
        "PCM signed 16-bit little-endian",
        CodecId::PcmS16Le,
    ),
    input("s16be", "PCM signed 16-bit big-endian", CodecId::PcmS16Be),
    input(
        "s24le",
        "PCM signed 24-bit little-endian",
        CodecId::PcmS24Le,
    ),
    input("s24be", "PCM signed 24-bit big-endian", CodecId::PcmS24Be),
    input(
        "s32le",
        "PCM signed 32-bit little-endian",
        CodecId::PcmS32Le,
    ),
    input("s32be", "PCM signed 32-bit big-endian", CodecId::PcmS32Be),
    input(
        "f32le",
        "PCM 32-bit floating point little-endian",
        CodecId::PcmF32Le,
    ),
    input(
        "f32be",
        "PCM 32-bit floating point big-endian",
        CodecId::PcmF32Be,
    ),
    input(
        "f64le",
        "PCM 64-bit floating point little-endian",
        CodecId::PcmF64Le,
    ),
    input(
        "f64be",
        "PCM 64-bit floating point big-endian",
        CodecId::PcmF64Be,
    ),
    input("alaw", "PCM A-law", CodecId::PcmAlaw),
    input("mulaw", "PCM mu-law", CodecId::PcmMulaw),
];

pub static OUTPUTS: &[OutputFormat] = &[
    output("u8", "PCM unsigned 8-bit", CodecId::PcmU8),
    output("s8", "PCM signed 8-bit", CodecId::PcmS8),
    output(
        "s16le",
        "PCM signed 16-bit little-endian",
        CodecId::PcmS16Le,
    ),
    output("s16be", "PCM signed 16-bit big-endian", CodecId::PcmS16Be),
    output(
        "s24le",
        "PCM signed 24-bit little-endian",
        CodecId::PcmS24Le,
    ),
    output("s24be", "PCM signed 24-bit big-endian", CodecId::PcmS24Be),
    output(
        "s32le",
        "PCM signed 32-bit little-endian",
        CodecId::PcmS32Le,
    ),
    output("s32be", "PCM signed 32-bit big-endian", CodecId::PcmS32Be),
    output(
        "f32le",
        "PCM 32-bit floating point little-endian",
        CodecId::PcmF32Le,
    ),
    output(
        "f32be",
        "PCM 32-bit floating point big-endian",
        CodecId::PcmF32Be,
    ),
    output(
        "f64le",
        "PCM 64-bit floating point little-endian",
        CodecId::PcmF64Le,
    ),
    output(
        "f64be",
        "PCM 64-bit floating point big-endian",
        CodecId::PcmF64Be,
    ),
    output("alaw", "PCM A-law", CodecId::PcmAlaw),
    output("mulaw", "PCM mu-law", CodecId::PcmMulaw),
];

/// Builds the description of a PCM audio stream as every PCM demuxer
/// reports it. G.711 streams declare the 16-bit linear size they expand to,
/// like WAV does, since that is what a decoder produces.
pub(crate) fn pcm_stream(codec: CodecId, sample_rate: u32, channels: u16) -> StreamInfo {
    let coded_bits = codec.pcm_bits().unwrap_or(0);
    let bits_per_sample = match codec {
        CodecId::PcmAlaw | CodecId::PcmMulaw => 16,
        _ => coded_bits,
    };
    let mut stream = StreamInfo::new(
        0,
        codec,
        StreamParams::Audio(AudioParams {
            sample_rate,
            channels,
            bits_per_sample,
        }),
        Rational::new(1, sample_rate as i64),
    );
    stream.bit_rate = Some(sample_rate as u64 * channels as u64 * coded_bits as u64);
    stream
}

/// Bytes of one sample frame (one sample for every channel) of a PCM codec.
pub(crate) fn block_align(codec: CodecId, channels: u16) -> usize {
    codec.pcm_bits().map_or(0, |b| b as usize / 8) * channels as usize
}

/// Cuts interleaved PCM read from `reader` into packets of whole sample
/// frames, with timestamps counted in samples.
pub(crate) struct PcmPackets {
    reader: Box<dyn Read>,
    block_align: usize,
    frames_per_packet: usize,
    /// Payload bytes left, `None` to read until the end of the stream.
    remaining: Option<u64>,
    next_pts: i64,
}

impl PcmPackets {
    pub(crate) fn new(reader: Box<dyn Read>, block_align: usize, remaining: Option<u64>) -> Self {
        let block_align = block_align.max(1);
        PcmPackets {
            reader,
            block_align,
            frames_per_packet: (MAX_PACKET_BYTES / block_align).clamp(1, PACKET_FRAMES),
            remaining,
            next_pts: 0,
        }
    }

    pub(crate) fn read_packet(&mut self) -> Result<Option<Packet>> {
        let mut want = self.frames_per_packet * self.block_align;
        if let Some(remaining) = self.remaining {
            want = want.min(usize::try_from(remaining).unwrap_or(usize::MAX));
        }
        if want == 0 {
            return Ok(None);
        }
        let mut data = vec![0u8; want];
        let got = self.reader.read_full(&mut data)?;
        if let Some(remaining) = &mut self.remaining {
            // A short read means the file is truncated: stop there even if
            // the header promised more.
            *remaining = if got < want {
                0
            } else {
                *remaining - got as u64
            };
        }
        // A truncated stream ends on a partial sample frame; keep whole
        // frames only so every packet decodes to complete samples.
        let whole = got / self.block_align * self.block_align;
        if whole == 0 {
            self.remaining = Some(0);
            return Ok(None);
        }
        data.truncate(whole);
        let frames = (whole / self.block_align) as i64;
        let packet = Packet::new(0, data, self.next_pts, frames);
        self.next_pts += frames;
        Ok(Some(packet))
    }
}

fn open(reader: Box<dyn Read>, options: &InputOptions) -> Result<Box<dyn Demuxer>> {
    let codec = options
        .raw_codec
        .ok_or_else(|| Error::invalid_argument("raw PCM input needs a format such as -f s16le"))?;
    let name = INPUTS
        .iter()
        .find(|f| f.raw_codec == Some(codec))
        .map(|f| f.name)
        .ok_or_else(|| Error::invalid_argument(format!("{codec} is not a raw PCM format")))?;
    let sample_rate = options.sample_rate.unwrap_or(DEFAULT_SAMPLE_RATE);
    let channels = options.channels.unwrap_or(DEFAULT_CHANNELS);
    if sample_rate == 0 || channels == 0 {
        return Err(Error::invalid_argument(
            "raw PCM needs a positive sample rate and channel count",
        ));
    }
    Ok(Box::new(RawDemuxer {
        name,
        streams: vec![pcm_stream(codec, sample_rate, channels)],
        packets: PcmPackets::new(reader, block_align(codec, channels), None),
    }))
}

struct RawDemuxer {
    name: &'static str,
    streams: Vec<StreamInfo>,
    packets: PcmPackets,
}

impl Demuxer for RawDemuxer {
    fn format_name(&self) -> &'static str {
        self.name
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn read_packet(&mut self) -> Result<Option<Packet>> {
        self.packets.read_packet()
    }
}

fn create(path: &str, _options: &OutputOptions) -> Result<Box<dyn Muxer>> {
    Ok(Box::new(RawMuxer {
        out: Output::create(path)?,
    }))
}

/// Writes packet payloads back to back. The pipeline has already checked
/// that the stream codec is the one the format name stands for.
struct RawMuxer {
    out: Output,
}

impl Muxer for RawMuxer {
    fn write_header(&mut self, streams: &[StreamInfo]) -> Result<()> {
        match streams {
            [stream] if stream.audio().is_some() => Ok(()),
            _ => Err(Error::unsupported(
                "raw PCM output holds exactly one audio stream",
            )),
        }
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        self.out.write_all(&packet.data)?;
        Ok(())
    }

    fn write_trailer(&mut self, _streams: &[StreamInfo]) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }

    fn bytes_written(&self) -> u64 {
        self.out.bytes_written()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn options(codec: CodecId, rate: Option<u32>, channels: Option<u16>) -> InputOptions {
        InputOptions {
            sample_rate: rate,
            channels,
            raw_codec: Some(codec),
            ..InputOptions::default()
        }
    }

    fn read_all(demux: &mut dyn Demuxer) -> Vec<Packet> {
        let mut packets = Vec::new();
        while let Some(p) = demux.read_packet().unwrap() {
            packets.push(p);
        }
        packets
    }

    #[test]
    fn every_layout_has_matching_input_and_output() {
        assert_eq!(INPUTS.len(), 14);
        assert_eq!(OUTPUTS.len(), 14);
        for (i, o) in INPUTS.iter().zip(OUTPUTS) {
            assert_eq!(i.name, o.name);
            let codec = i.raw_codec.unwrap();
            assert_eq!(o.audio_codec, Some(codec));
            assert_eq!(o.codecs, &[codec]);
            assert_eq!((i.probe)(b"anything at all"), 0);
            assert!(codec.pcm_bits().is_some());
        }
        // Names follow the codec names without their "pcm_" prefix.
        for o in OUTPUTS {
            let codec = o.audio_codec.unwrap();
            assert_eq!(codec.name(), format!("pcm_{}", o.name));
        }
    }

    #[test]
    fn defaults_to_mono_44100() {
        let demux = open(
            Box::new(Cursor::new(Vec::new())),
            &options(CodecId::PcmS16Le, None, None),
        )
        .unwrap();
        let a = *demux.streams()[0].audio().unwrap();
        assert_eq!(a.sample_rate, 44100);
        assert_eq!(a.channels, 1);
        assert_eq!(a.bits_per_sample, 16);
        assert_eq!(demux.format_name(), "s16le");
        assert_eq!(demux.streams()[0].duration, None);
    }

    #[test]
    fn packets_hold_whole_frames_with_sample_pts() {
        // 3 channels of s24 = 9 bytes per frame; 5000 frames plus 4 stray bytes.
        let total = 5000 * 9 + 4;
        let bytes: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();
        let mut demux = open(
            Box::new(Cursor::new(bytes.clone())),
            &options(CodecId::PcmS24Be, Some(8000), Some(3)),
        )
        .unwrap();
        assert_eq!(demux.format_name(), "s24be");
        let packets = read_all(demux.as_mut());
        assert_eq!(packets.len(), 2);
        assert_eq!((packets[0].pts, packets[0].duration), (0, 4096));
        assert_eq!((packets[1].pts, packets[1].duration), (4096, 904));
        let joined: Vec<u8> = packets.into_iter().flat_map(|p| p.data).collect();
        assert_eq!(joined, &bytes[..5000 * 9]);
    }

    #[test]
    fn g711_reports_linear_bits() {
        let demux = open(
            Box::new(Cursor::new(vec![0xD5; 10])),
            &options(CodecId::PcmMulaw, Some(8000), None),
        )
        .unwrap();
        let s = &demux.streams()[0];
        assert_eq!(s.audio().unwrap().bits_per_sample, 16);
        assert_eq!(s.bit_rate, Some(64000));
    }

    #[test]
    fn rejects_missing_codec_and_zero_parameters() {
        let empty = || -> Box<dyn Read> { Box::new(Cursor::new(Vec::new())) };
        assert!(matches!(
            open(empty(), &InputOptions::default()),
            Err(Error::InvalidArgument(_))
        ));
        assert!(open(empty(), &options(CodecId::PcmU8, Some(0), None)).is_err());
        assert!(open(empty(), &options(CodecId::PcmU8, None, Some(0))).is_err());
        assert!(open(empty(), &options(CodecId::Flac, None, None)).is_err());
    }

    #[test]
    fn packet_size_is_capped_for_many_channels() {
        // 65535 channels of f64: one frame is already over half a megabyte.
        let align = block_align(CodecId::PcmF64Be, u16::MAX);
        let bytes = vec![0u8; align * 3];
        let mut packets = PcmPackets::new(Box::new(Cursor::new(bytes)), align, None);
        let mut frames = 0;
        while let Some(p) = packets.read_packet().unwrap() {
            assert!(p.data.len() <= MAX_PACKET_BYTES.max(align));
            assert_eq!(p.pts, frames);
            frames += p.duration;
        }
        assert_eq!(frames, 3);
    }

    #[test]
    fn byte_limit_stops_before_trailing_data() {
        let bytes: Vec<u8> = (0..100).collect();
        let mut packets = PcmPackets::new(Box::new(Cursor::new(bytes)), 4, Some(10));
        let p = packets.read_packet().unwrap().unwrap();
        // 10 bytes allowed, but only two whole 4-byte frames fit.
        assert_eq!(p.data, (0..8).collect::<Vec<u8>>());
        assert!(packets.read_packet().unwrap().is_none());
    }

    #[test]
    fn short_file_ends_cleanly() {
        let mut packets = PcmPackets::new(Box::new(Cursor::new(vec![1, 2, 3])), 4, Some(1000));
        assert!(packets.read_packet().unwrap().is_none());
        assert!(packets.read_packet().unwrap().is_none());
    }

    #[test]
    fn muxer_writes_payload_verbatim() {
        let stream = pcm_stream(CodecId::PcmS16Le, 8000, 2);
        let mut mux = RawMuxer {
            out: Output::memory(),
        };
        mux.write_header(std::slice::from_ref(&stream)).unwrap();
        mux.write_packet(&Packet::new(0, vec![1, 2, 3, 4], 0, 1))
            .unwrap();
        mux.write_packet(&Packet::new(0, vec![5, 6, 7, 8], 1, 1))
            .unwrap();
        mux.write_trailer(&[stream]).unwrap();
        assert_eq!(mux.bytes_written(), 8);
        assert_eq!(mux.out.into_bytes().unwrap(), [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn muxer_rejects_several_streams() {
        let stream = pcm_stream(CodecId::PcmS16Le, 8000, 2);
        let mut mux = RawMuxer {
            out: Output::memory(),
        };
        assert!(mux.write_header(&[]).is_err());
        assert!(mux.write_header(&[stream.clone(), stream]).is_err());
    }
}
