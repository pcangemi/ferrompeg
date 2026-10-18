//! AIFF and AIFF-C (big-endian PCM).
//!
//! Written from "Audio Interchange File Format: AIFF, Version 1.3" (1989)
//! and the "AIFF-C" draft (1991), which extends the COMM chunk with a
//! compression type. The file is a big-endian IFF `FORM` holding chunks
//! that may come in any order; this module uses:
//!
//! * `COMM`: channels, sample frames, sample size, and the sample rate as an
//!   80-bit IEEE 754 extended float (plus the compression type in AIFF-C);
//! * `SSND`: an offset and block size, then the interleaved samples;
//! * `NAME`, `AUTH`, `(c) ` and `ANNO`: text, mapped to the title, artist,
//!   copyright and comment tags;
//! * `FVER`: the AIFF-C version stamp, written but not checked.
//!
//! Every chunk with an odd size is followed by one pad byte. Integer
//! samples of any size from 1 to 32 bits are stored left-justified in whole
//! bytes, so they decode as the matching 8, 16, 24 or 32-bit layout.
//!
//! The demuxer reads from a stream that cannot seek, so the `COMM` chunk
//! must come before `SSND` (as every common writer does), and chunks after
//! the sound data are not read. An `SSND` size of 0 or `0xFFFFFFFF` means
//! the samples run to the end of the file: writers that cannot seek back
//! to patch their header leave one or the other. The muxer leaves
//! `0xFFFFFFFF` in the `FORM` and `SSND` sizes on pipes, the variant that
//! streaming readers accept, and a frame count of 0.

use std::io::{Read, Seek, SeekFrom, Write};

use crate::error::{Error, Result};
use crate::format::rawaudio::{PcmPackets, block_align, pcm_stream};
use crate::format::{Demuxer, InputFormat, InputOptions, Muxer, OutputFormat, OutputOptions};
use crate::io::Output;
use crate::stream::{CodecId, Packet, StreamInfo};
use crate::util::bytes::ReadBytes;

pub const INPUT: InputFormat = InputFormat {
    name: "aiff",
    long_name: "Audio IFF",
    extensions: &["aiff", "aif", "aifc"],
    probe,
    open,
    raw_codec: None,
};

fn probe(data: &[u8]) -> u32 {
    if data.len() >= 12
        && &data[0..4] == b"FORM"
        && (&data[8..12] == b"AIFF" || &data[8..12] == b"AIFC")
    {
        100
    } else {
        0
    }
}

fn open(reader: Box<dyn Read>, _options: &InputOptions) -> Result<Box<dyn Demuxer>> {
    Ok(Box::new(AiffDemuxer::new(reader)?))
}

pub const OUTPUT: OutputFormat = OutputFormat {
    name: "aiff",
    long_name: "Audio IFF",
    extensions: &["aiff", "aif"],
    audio_codec: Some(CodecId::PcmS16Be),
    video_codec: None,
    // Integer PCM goes into plain AIFF; the rest needs AIFF-C.
    codecs: &[
        CodecId::PcmS8,
        CodecId::PcmS16Be,
        CodecId::PcmS24Be,
        CodecId::PcmS32Be,
        CodecId::PcmF32Be,
        CodecId::PcmF64Be,
        CodecId::PcmAlaw,
        CodecId::PcmMulaw,
    ],
    pixel_formats: &[],
    create,
};

fn create(path: &str, options: &OutputOptions) -> Result<Box<dyn Muxer>> {
    Ok(Box::new(AiffMuxer::new(
        Output::create(path)?,
        options.metadata.clone(),
    )))
}

/// Value of the `FVER` chunk for the AIFF-C draft this module follows.
const AIFC_VERSION_1: u32 = 0xA280_5140;

/// The largest `COMM` chunk we accept: 18 bytes in AIFF, 22 plus a Pascal
/// string of at most 255 characters in AIFF-C. Anything bigger is corrupt.
const MAX_COMM_SIZE: u32 = 1024;

/// Text chunks above this size are skipped rather than loaded.
const MAX_TEXT_SIZE: u32 = 64 * 1024;

/// Chunk size left in the header when the output cannot seek back to
/// write the real one.
const UNKNOWN_SIZE: u32 = u32::MAX;

// 80-bit IEEE 754 extended precision, as used for the COMM sample rate:
// 1 sign bit, a 15-bit exponent biased by 16383, then a 64-bit significand
// whose top bit is the integer part (explicit, unlike in f32 and f64).

const EXTENDED_BIAS: i32 = 16383;

/// Multiplies `x` by 2 to the power `e` without overflowing an
/// intermediate power of two, whatever the exponent.
fn scale_by_power_of_two(mut x: f64, mut e: i32) -> f64 {
    let pow2 = |e: i32| f64::from_bits(((e + 1023) as u64) << 52);
    while e > 1000 && x.is_finite() {
        x *= pow2(1000);
        e -= 1000;
    }
    while e < -1000 && x != 0.0 {
        x *= pow2(-1000);
        e += 1000;
    }
    x * pow2(e.clamp(-1000, 1000))
}

/// Decodes an 80-bit extended float to the nearest `f64`.
pub fn extended_to_f64(bytes: &[u8; 10]) -> f64 {
    let sign_exp = u16::from_be_bytes([bytes[0], bytes[1]]);
    let mut significand = [0u8; 8];
    significand.copy_from_slice(&bytes[2..]);
    let significand = u64::from_be_bytes(significand);
    let exponent = (sign_exp & 0x7FFF) as i32;
    let magnitude = if exponent == 0x7FFF {
        // The integer bit is not part of the infinity / NaN distinction.
        if significand << 1 == 0 {
            f64::INFINITY
        } else {
            f64::NAN
        }
    } else if significand == 0 {
        0.0
    } else {
        // value = significand * 2^(exponent - bias - 63)
        scale_by_power_of_two(significand as f64, exponent - EXTENDED_BIAS - 63)
    };
    if sign_exp & 0x8000 != 0 {
        -magnitude
    } else {
        magnitude
    }
}

/// Encodes an `f64` as an 80-bit extended float. Every `f64` is exactly
/// representable, so this never rounds.
pub fn f64_to_extended(value: f64) -> [u8; 10] {
    let bits = value.to_bits();
    let sign = ((bits >> 63) as u16) << 15;
    let exponent = ((bits >> 52) & 0x7FF) as i32;
    let fraction = bits & ((1u64 << 52) - 1);
    let (exp_field, significand) = match exponent {
        0x7FF => (0x7FFF, (1u64 << 63) | (fraction << 11)),
        0 if fraction == 0 => (0, 0),
        0 => {
            // Subnormal f64 (fraction * 2^-1074): normalize so the integer
            // bit is set, which the wider exponent always allows.
            let shift = fraction.leading_zeros();
            (EXTENDED_BIAS + 63 - 1074 - shift as i32, fraction << shift)
        }
        e => (e - 1023 + EXTENDED_BIAS, (1u64 << 63) | (fraction << 11)),
    };
    let mut out = [0u8; 10];
    out[..2].copy_from_slice(&(sign | exp_field as u16).to_be_bytes());
    out[2..].copy_from_slice(&significand.to_be_bytes());
    out
}

/// Reads the COMM sample rate, rounding rates such as 22254.5454 to the
/// nearest integer since the rest of the crate counts whole hertz.
fn sample_rate_from_extended(bytes: &[u8; 10]) -> Result<u32> {
    let rate = extended_to_f64(bytes).round();
    if rate.is_finite() && rate >= 1.0 && rate <= u32::MAX as f64 {
        Ok(rate as u32)
    } else {
        Err(Error::invalid_data(format!(
            "AIFF sample rate {} is not usable",
            extended_to_f64(bytes)
        )))
    }
}

/// What the COMM chunk tells us.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Comm {
    codec: CodecId,
    channels: u16,
    frames: u32,
    sample_rate: u32,
}

/// Formats a chunk or compression id for an error message, escaping bytes
/// that are not printable.
fn fourcc(id: &[u8]) -> String {
    id.iter()
        .flat_map(|b| b.escape_ascii())
        .map(char::from)
        .collect()
}

fn parse_comm(body: &[u8], aifc: bool) -> Result<Comm> {
    if body.len() < 18 {
        return Err(Error::invalid_data("AIFF COMM chunk is too short"));
    }
    let channels = i16::from_be_bytes([body[0], body[1]]);
    let frames = u32::from_be_bytes([body[2], body[3], body[4], body[5]]);
    let sample_size = i16::from_be_bytes([body[6], body[7]]);
    let mut rate = [0u8; 10];
    rate.copy_from_slice(&body[8..18]);
    if channels <= 0 {
        return Err(Error::invalid_data(format!(
            "AIFF declares {channels} channels"
        )));
    }
    let sample_rate = sample_rate_from_extended(&rate)?;
    // Plain AIFF has no compression field. Some AIFF-C writers also omit
    // it, which can only mean uncompressed.
    let compression: &[u8] = if aifc && body.len() >= 22 {
        &body[18..22]
    } else {
        b"NONE"
    };
    let int_codec = |little_endian: bool| -> Result<CodecId> {
        let codec = match (sample_size, little_endian) {
            (1..=8, _) => CodecId::PcmS8,
            (9..=16, false) => CodecId::PcmS16Be,
            (17..=24, false) => CodecId::PcmS24Be,
            (25..=32, false) => CodecId::PcmS32Be,
            (9..=16, true) => CodecId::PcmS16Le,
            (17..=24, true) => CodecId::PcmS24Le,
            (25..=32, true) => CodecId::PcmS32Le,
            _ => {
                return Err(Error::invalid_data(format!(
                    "AIFF sample size of {sample_size} bits"
                )));
            }
        };
        Ok(codec)
    };
    let codec = match compression {
        // "twos" is the same big-endian two's complement layout.
        b"NONE" | b"twos" => int_codec(false)?,
        // Byte-swapped integers, mostly 16-bit.
        b"sowt" => int_codec(true)?,
        b"in24" => CodecId::PcmS24Be,
        b"in32" => CodecId::PcmS32Be,
        b"raw " => CodecId::PcmU8,
        b"fl32" | b"FL32" => CodecId::PcmF32Be,
        b"fl64" | b"FL64" => CodecId::PcmF64Be,
        // G.711 always codes one byte per sample; the sample size field
        // varies between writers (8 or 16) and is not needed.
        b"alaw" | b"ALAW" => CodecId::PcmAlaw,
        b"ulaw" | b"ULAW" => CodecId::PcmMulaw,
        other => {
            return Err(Error::unsupported(format!(
                "AIFF-C compression type '{}'",
                fourcc(other)
            )));
        }
    };
    Ok(Comm {
        codec,
        channels: channels as u16,
        frames,
        sample_rate,
    })
}

/// Maps a text chunk id to the tag it carries.
fn text_tag(id: &[u8; 4]) -> Option<&'static str> {
    match id {
        b"NAME" => Some("title"),
        b"AUTH" => Some("artist"),
        b"(c) " => Some("copyright"),
        b"ANNO" => Some("comment"),
        _ => None,
    }
}

pub struct AiffDemuxer {
    streams: Vec<StreamInfo>,
    metadata: Vec<(String, String)>,
    packets: PcmPackets,
}

impl AiffDemuxer {
    pub fn new(mut reader: Box<dyn Read>) -> Result<Self> {
        let form: [u8; 4] = reader.read_fixed()?;
        // The FORM size is not trusted: streaming writers leave 0 or all ones and
        // the SSND size already bounds what we read.
        let _form_size = reader.read_u32_be()?;
        let kind: [u8; 4] = reader.read_fixed()?;
        if &form != b"FORM" {
            return Err(Error::invalid_data("not an IFF FORM file"));
        }
        let aifc = match &kind {
            b"AIFF" => false,
            b"AIFC" => true,
            _ => return Err(Error::invalid_data("IFF FORM is not AIFF or AIFF-C")),
        };

        let mut comm: Option<Comm> = None;
        let mut metadata: Vec<(String, String)> = Vec::new();
        loop {
            let id: [u8; 4] = match reader.read_fixed() {
                Ok(id) => id,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Err(Error::invalid_data("AIFF file has no SSND chunk"));
                }
                Err(e) => return Err(e.into()),
            };
            let size = reader.read_u32_be()?;
            match &id {
                b"COMM" => {
                    if size > MAX_COMM_SIZE {
                        return Err(Error::invalid_data(format!(
                            "AIFF COMM chunk of {size} bytes"
                        )));
                    }
                    let body = reader.read_vec(size as usize)?;
                    comm = Some(parse_comm(&body, aifc)?);
                }
                b"SSND" => {
                    let comm = comm.ok_or_else(|| {
                        Error::unsupported("AIFF sound data before the COMM chunk")
                    })?;
                    return Self::start_sound(reader, comm, size, metadata);
                }
                _ => match text_tag(&id) {
                    Some(key) if size <= MAX_TEXT_SIZE => {
                        let body = reader.read_vec(size as usize)?;
                        add_text(&mut metadata, key, &body);
                    }
                    _ => reader.skip(size as u64)?,
                },
            }
            if size % 2 == 1 {
                reader.skip(1)?;
            }
        }
    }

    /// Reads the SSND header and positions `reader` on the first sample.
    fn start_sound(
        mut reader: Box<dyn Read>,
        comm: Comm,
        size: u32,
        metadata: Vec<(String, String)>,
    ) -> Result<Self> {
        let offset = reader.read_u32_be()? as u64;
        // The block size only matters to writers aligning their data.
        let _block_size = reader.read_u32_be()?;
        let remaining = match size {
            0 | UNKNOWN_SIZE => None,
            size => {
                let size = size as u64;
                if size < 8 + offset {
                    return Err(Error::invalid_data(
                        "AIFF SSND offset points past the end of the chunk",
                    ));
                }
                Some(size - 8 - offset)
            }
        };
        reader.skip(offset)?;

        let align = block_align(comm.codec, comm.channels);
        let mut stream = pcm_stream(comm.codec, comm.sample_rate, comm.channels);
        stream.duration = if comm.frames > 0 {
            Some(comm.frames as i64)
        } else {
            remaining.map(|r| (r / align as u64) as i64)
        };
        Ok(AiffDemuxer {
            streams: vec![stream],
            metadata,
            packets: PcmPackets::new(reader, align, remaining),
        })
    }
}

/// Adds the text of a NAME/AUTH/(c) /ANNO chunk. Several ANNO chunks are
/// allowed and are joined into one comment.
fn add_text(metadata: &mut Vec<(String, String)>, key: &str, body: &[u8]) {
    let text = String::from_utf8_lossy(body)
        .trim_end_matches('\0')
        .to_string();
    if text.is_empty() {
        return;
    }
    match metadata.iter_mut().find(|(k, _)| k == key) {
        Some((_, value)) if key == "comment" => {
            value.push('\n');
            value.push_str(&text);
        }
        Some(_) => {}
        None => metadata.push((key.to_string(), text)),
    }
}

impl Demuxer for AiffDemuxer {
    fn format_name(&self) -> &'static str {
        "aiff"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }

    fn read_packet(&mut self) -> Result<Option<Packet>> {
        self.packets.read_packet()
    }
}

/// How a codec is declared in the COMM chunk.
struct Layout {
    /// `None` for plain AIFF, else the AIFF-C compression type and name.
    compression: Option<(&'static [u8; 4], &'static str)>,
    /// The COMM sample size: significant bits for integers, the
    /// uncompressed size for G.711.
    sample_size: u16,
}

fn layout(codec: CodecId) -> Result<Layout> {
    let plain = |sample_size| Layout {
        compression: None,
        sample_size,
    };
    let aifc = |id, name, sample_size| Layout {
        compression: Some((id, name)),
        sample_size,
    };
    Ok(match codec {
        CodecId::PcmS8 => plain(8),
        CodecId::PcmS16Be => plain(16),
        CodecId::PcmS24Be => plain(24),
        CodecId::PcmS32Be => plain(32),
        CodecId::PcmF32Be => aifc(b"fl32", "32-bit floating point", 32),
        CodecId::PcmF64Be => aifc(b"fl64", "64-bit floating point", 64),
        CodecId::PcmAlaw => aifc(b"alaw", "A-law 2:1", 16),
        CodecId::PcmMulaw => aifc(b"ulaw", "mu-law 2:1", 16),
        c => return Err(Error::unsupported(format!("{c} in AIFF"))),
    })
}

/// Appends an IFF chunk, with its pad byte when the size is odd.
fn put_chunk(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) -> Result<()> {
    let size = u32::try_from(body.len())
        .map_err(|_| Error::invalid_argument("AIFF chunk larger than 4 GiB"))?;
    out.extend_from_slice(id);
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(body);
    if body.len() % 2 == 1 {
        out.push(0);
    }
    Ok(())
}

pub struct AiffMuxer {
    out: Output,
    metadata: Vec<(String, String)>,
    /// Offsets of the fields patched in the trailer.
    frames_offset: u64,
    ssnd_size_offset: u64,
    /// Bytes before the first sample.
    header_len: u64,
    block_align: usize,
    data_bytes: u64,
}

impl AiffMuxer {
    pub fn new(out: Output, metadata: Vec<(String, String)>) -> Self {
        AiffMuxer {
            out,
            metadata,
            frames_offset: 0,
            ssnd_size_offset: 0,
            header_len: 0,
            block_align: 1,
            data_bytes: 0,
        }
    }

    pub fn into_output(self) -> Output {
        self.out
    }

    /// Text chunks from the output tags. NAME, AUTH and (c) may appear
    /// once each; ANNO may repeat.
    fn text_chunks(&self, out: &mut Vec<u8>) -> Result<()> {
        let mut seen: Vec<&[u8; 4]> = Vec::new();
        for (key, value) in &self.metadata {
            let id: &[u8; 4] = match key.to_ascii_lowercase().as_str() {
                "title" => b"NAME",
                "artist" | "author" => b"AUTH",
                "copyright" => b"(c) ",
                "comment" => b"ANNO",
                _ => continue,
            };
            if value.is_empty() || (id != b"ANNO" && seen.contains(&id)) {
                continue;
            }
            seen.push(id);
            put_chunk(out, id, value.as_bytes())?;
        }
        Ok(())
    }
}

impl Muxer for AiffMuxer {
    fn write_header(&mut self, streams: &[StreamInfo]) -> Result<()> {
        let [stream] = streams else {
            return Err(Error::unsupported("AIFF holds exactly one audio stream"));
        };
        let a = stream
            .audio()
            .ok_or_else(|| Error::unsupported("AIFF holds audio only"))?;
        let layout = layout(stream.codec)?;
        let channels = i16::try_from(a.channels)
            .ok()
            .filter(|&c| c > 0)
            .ok_or_else(|| Error::unsupported(format!("{} channels in AIFF", a.channels)))?;
        if a.sample_rate == 0 {
            return Err(Error::invalid_argument("AIFF needs a sample rate"));
        }
        self.block_align = block_align(stream.codec, a.channels);

        let mut header = Vec::with_capacity(128);
        header.extend_from_slice(b"FORM");
        // Patched in the trailer when the output can seek.
        header.extend_from_slice(&UNKNOWN_SIZE.to_be_bytes());
        header.extend_from_slice(if layout.compression.is_some() {
            b"AIFC"
        } else {
            b"AIFF"
        });
        if layout.compression.is_some() {
            put_chunk(&mut header, b"FVER", &AIFC_VERSION_1.to_be_bytes())?;
        }

        let mut comm = Vec::with_capacity(64);
        comm.extend_from_slice(&channels.to_be_bytes());
        comm.extend_from_slice(&0u32.to_be_bytes());
        comm.extend_from_slice(&layout.sample_size.to_be_bytes());
        comm.extend_from_slice(&f64_to_extended(a.sample_rate as f64));
        if let Some((id, name)) = layout.compression {
            comm.extend_from_slice(id);
            // Pascal string: a length byte, the text, and a pad byte when
            // needed to keep the total even.
            comm.push(name.len() as u8);
            comm.extend_from_slice(name.as_bytes());
            if name.len() % 2 == 0 {
                comm.push(0);
            }
        }
        // The frame count sits 2 bytes into the COMM body, after the
        // 8-byte chunk header.
        self.frames_offset = header.len() as u64 + 8 + 2;
        put_chunk(&mut header, b"COMM", &comm)?;

        self.text_chunks(&mut header)?;

        header.extend_from_slice(b"SSND");
        self.ssnd_size_offset = header.len() as u64;
        header.extend_from_slice(&UNKNOWN_SIZE.to_be_bytes());
        // No offset and no block alignment: samples follow immediately.
        header.extend_from_slice(&0u32.to_be_bytes());
        header.extend_from_slice(&0u32.to_be_bytes());
        self.header_len = header.len() as u64;
        self.out.write_all(&header)?;
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        self.out.write_all(&packet.data)?;
        self.data_bytes += packet.data.len() as u64;
        Ok(())
    }

    fn write_trailer(&mut self, _streams: &[StreamInfo]) -> Result<()> {
        let pad = self.data_bytes % 2;
        if pad == 1 {
            self.out.write_all(&[0])?;
        }
        if self.out.is_seekable() {
            let form_size = self.header_len + self.data_bytes + pad - 8;
            let form_size = u32::try_from(form_size)
                .map_err(|_| Error::unsupported("AIFF output larger than 4 GiB"))?;
            // Fits since the FORM size does.
            let ssnd_size = (self.data_bytes + 8) as u32;
            let frames = (self.data_bytes / self.block_align as u64) as u32;
            for (offset, value) in [
                (4, form_size),
                (self.frames_offset, frames),
                (self.ssnd_size_offset, ssnd_size),
            ] {
                self.out.seek(SeekFrom::Start(offset))?;
                self.out.write_all(&value.to_be_bytes())?;
            }
            self.out.seek(SeekFrom::End(0))?;
        }
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
    use crate::rational::Rational;
    use crate::stream::{AudioParams, StreamParams};
    use std::io::Cursor;

    const FIXTURES: &str = "tests/fixtures/audio-containers";

    fn fixture(name: &str) -> Vec<u8> {
        let path = format!("{}/{FIXTURES}/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn stream(codec: CodecId, channels: u16, rate: u32) -> StreamInfo {
        StreamInfo::new(
            0,
            codec,
            StreamParams::Audio(AudioParams {
                sample_rate: rate,
                channels,
                bits_per_sample: codec.pcm_bits().unwrap(),
            }),
            Rational::new(1, rate as i64),
        )
    }

    fn write_aiff(
        codec: CodecId,
        channels: u16,
        rate: u32,
        payload: &[u8],
        metadata: &[(&str, &str)],
    ) -> Vec<u8> {
        let info = stream(codec, channels, rate);
        let tags = metadata
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let mut mux = AiffMuxer::new(Output::memory(), tags);
        mux.write_header(std::slice::from_ref(&info)).unwrap();
        mux.write_packet(&Packet::new(0, payload.to_vec(), 0, 0))
            .unwrap();
        mux.write_trailer(&[info]).unwrap();
        mux.into_output().into_bytes().unwrap()
    }

    fn demux(bytes: Vec<u8>) -> Result<AiffDemuxer> {
        AiffDemuxer::new(Box::new(Cursor::new(bytes)))
    }

    fn read_all(bytes: Vec<u8>) -> (AiffDemuxer, Vec<u8>) {
        let mut d = demux(bytes).unwrap();
        let mut data = Vec::new();
        let mut pts = 0;
        while let Some(p) = d.read_packet().unwrap() {
            assert_eq!(p.pts, pts);
            pts += p.duration;
            data.extend(p.data);
        }
        (d, data)
    }

    /// Builds a file from raw chunks, padding odd ones.
    fn build(kind: &[u8; 4], chunks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut body = kind.to_vec();
        for (id, data) in chunks {
            put_chunk(&mut body, id, data).unwrap();
        }
        let mut file = b"FORM".to_vec();
        file.extend_from_slice(&(body.len() as u32).to_be_bytes());
        file.extend_from_slice(&body);
        file
    }

    fn comm(channels: i16, frames: u32, bits: i16, rate: f64, compression: &[u8]) -> Vec<u8> {
        let mut c = Vec::new();
        c.extend_from_slice(&channels.to_be_bytes());
        c.extend_from_slice(&frames.to_be_bytes());
        c.extend_from_slice(&bits.to_be_bytes());
        c.extend_from_slice(&f64_to_extended(rate));
        c.extend_from_slice(compression);
        c
    }

    fn ssnd(offset: u32, data: &[u8]) -> Vec<u8> {
        let mut s = offset.to_be_bytes().to_vec();
        s.extend_from_slice(&0u32.to_be_bytes());
        s.extend(std::iter::repeat_n(0xEE, offset as usize));
        s.extend_from_slice(data);
        s
    }

    #[test]
    fn extended_matches_known_encodings() {
        let cases: [(f64, [u8; 10]); 6] = [
            (8000.0, [0x40, 0x0B, 0xFA, 0, 0, 0, 0, 0, 0, 0]),
            (44100.0, [0x40, 0x0E, 0xAC, 0x44, 0, 0, 0, 0, 0, 0]),
            (48000.0, [0x40, 0x0E, 0xBB, 0x80, 0, 0, 0, 0, 0, 0]),
            (96000.0, [0x40, 0x0F, 0xBB, 0x80, 0, 0, 0, 0, 0, 0]),
            (1.0, [0x3F, 0xFF, 0x80, 0, 0, 0, 0, 0, 0, 0]),
            (-2.0, [0xC0, 0x00, 0x80, 0, 0, 0, 0, 0, 0, 0]),
        ];
        for (value, bytes) in cases {
            assert_eq!(f64_to_extended(value), bytes, "{value}");
            assert_eq!(extended_to_f64(&bytes), value);
        }
        assert_eq!(f64_to_extended(0.0), [0; 10]);
        assert_eq!(extended_to_f64(&[0; 10]), 0.0);
    }

    #[test]
    fn extended_round_trips_every_kind_of_f64() {
        let values = [
            0.5,
            22254.545454545454,
            11025.0,
            u32::MAX as f64,
            1e300,
            -1e-300,
            f64::MIN_POSITIVE,
            f64::MIN_POSITIVE / 1024.0,
            5e-324,
            f64::MAX,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ];
        for v in values {
            assert_eq!(extended_to_f64(&f64_to_extended(v)).to_bits(), v.to_bits());
        }
        assert!(extended_to_f64(&f64_to_extended(f64::NAN)).is_nan());
        // Unnormalized significand (integer bit clear) still decodes.
        assert_eq!(
            extended_to_f64(&[0x40, 0x0C, 0x7D, 0, 0, 0, 0, 0, 0, 0]),
            8000.0
        );
        // Out of the f64 range: saturates instead of panicking.
        assert_eq!(
            extended_to_f64(&[0x7F, 0xFE, 0x80, 0, 0, 0, 0, 0, 0, 0]),
            f64::INFINITY
        );
        assert_eq!(
            extended_to_f64(&[0x00, 0x01, 0x80, 0, 0, 0, 0, 0, 0, 0]),
            0.0
        );
    }

    #[test]
    fn sample_rate_must_be_usable() {
        assert_eq!(
            sample_rate_from_extended(&f64_to_extended(22254.545)).unwrap(),
            22255
        );
        for bad in [0.0, 0.4, -8000.0, 1e10, f64::INFINITY, f64::NAN] {
            assert!(sample_rate_from_extended(&f64_to_extended(bad)).is_err());
        }
    }

    #[test]
    fn round_trips_every_codec() {
        for codec in OUTPUT.codecs {
            for channels in [1u16, 2, 6] {
                let align = block_align(*codec, channels);
                let payload: Vec<u8> = (0..align * 9).map(|i| (i * 7) as u8).collect();
                let bytes = write_aiff(*codec, channels, 44100, &payload, &[]);
                let aifc = !matches!(
                    codec,
                    CodecId::PcmS8 | CodecId::PcmS16Be | CodecId::PcmS24Be | CodecId::PcmS32Be
                );
                assert_eq!(&bytes[8..12], if aifc { b"AIFC" } else { b"AIFF" });
                let (d, data) = read_all(bytes);
                let info = &d.streams()[0];
                assert_eq!(info.codec, *codec);
                assert_eq!(info.audio().unwrap().channels, channels);
                assert_eq!(info.audio().unwrap().sample_rate, 44100);
                assert_eq!(info.duration, Some(9));
                assert_eq!(data, payload, "{codec} {channels}ch");
            }
        }
    }

    #[test]
    fn header_sizes_are_patched() {
        let bytes = write_aiff(CodecId::PcmS16Be, 2, 8000, &[1, 2, 3, 4, 5, 6, 7, 8], &[]);
        // FORM(12) + COMM(8 + 18) + SSND header (16) + data.
        assert_eq!(bytes.len(), 12 + 26 + 16 + 8);
        assert_eq!(u32::from_be_bytes(bytes[4..8].try_into().unwrap()), 54);
        assert_eq!(&bytes[12..16], b"COMM");
        assert_eq!(u32::from_be_bytes(bytes[22..26].try_into().unwrap()), 2);
        assert_eq!(&bytes[38..42], b"SSND");
        assert_eq!(u32::from_be_bytes(bytes[42..46].try_into().unwrap()), 16);
    }

    #[test]
    fn aifc_header_layout() {
        let bytes = write_aiff(CodecId::PcmF32Be, 1, 48000, &[0; 8], &[]);
        assert_eq!(&bytes[12..16], b"FVER");
        assert_eq!(&bytes[20..24], &AIFC_VERSION_1.to_be_bytes());
        assert_eq!(&bytes[24..28], b"COMM");
        let comm_size = u32::from_be_bytes(bytes[28..32].try_into().unwrap()) as usize;
        // 22 fixed bytes plus "32-bit floating point" (21 chars, odd, so the
        // Pascal string needs no pad).
        assert_eq!(comm_size, 22 + 1 + 21);
        assert_eq!(&bytes[50..54], b"fl32");
        assert_eq!(bytes[54], 21);
        assert_eq!(u32::from_be_bytes(bytes[34..38].try_into().unwrap()), 2);
    }

    #[test]
    fn odd_payload_is_padded() {
        let bytes = write_aiff(CodecId::PcmS8, 1, 8000, &[1, 2, 3], &[]);
        assert_eq!(bytes.len() % 2, 0);
        assert_eq!(*bytes.last().unwrap(), 0);
        let form = u32::from_be_bytes(bytes[4..8].try_into().unwrap()) as usize;
        assert_eq!(form + 8, bytes.len());
        let (_, data) = read_all(bytes);
        assert_eq!(data, [1, 2, 3]);
    }

    #[test]
    fn tags_round_trip() {
        let bytes = write_aiff(
            CodecId::PcmS16Be,
            1,
            8000,
            &[0, 1],
            &[
                ("title", "Odd"),
                ("artist", "Some One"),
                ("copyright", "2026"),
                ("comment", "first"),
                ("comment", "second"),
                ("encoder", "ignored"),
                ("TITLE", "duplicate"),
            ],
        );
        let (d, data) = read_all(bytes);
        assert_eq!(data, [0, 1]);
        let tags: Vec<(&str, &str)> = d
            .metadata()
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            tags,
            [
                ("title", "Odd"),
                ("artist", "Some One"),
                ("copyright", "2026"),
                ("comment", "first\nsecond"),
            ]
        );
    }

    #[test]
    fn reads_chunks_in_any_order_with_offset() {
        let bytes = build(
            b"AIFF",
            &[
                (b"ANNO", b"note\0".to_vec()),
                (b"MARK", vec![0, 0]),
                (b"COMM", comm(1, 3, 12, 11025.0, b"")),
                (b"NAME", b"T".to_vec()),
                (b"SSND", ssnd(5, &[0x10, 0x00, 0x20, 0x00, 0x30, 0x00])),
                (b"APPL", b"trailing".to_vec()),
            ],
        );
        let (d, data) = read_all(bytes);
        let info = &d.streams()[0];
        // 12-bit samples are stored left-justified in 16 bits.
        assert_eq!(info.codec, CodecId::PcmS16Be);
        assert_eq!(info.audio().unwrap().sample_rate, 11025);
        assert_eq!(info.duration, Some(3));
        assert_eq!(data, [0x10, 0x00, 0x20, 0x00, 0x30, 0x00]);
        assert_eq!(
            d.metadata(),
            [
                ("comment".to_string(), "note".to_string()),
                ("title".to_string(), "T".to_string())
            ]
        );
    }

    #[test]
    fn maps_aifc_compression_types() {
        let cases: [(&[u8; 4], i16, CodecId); 14] = [
            (b"NONE", 24, CodecId::PcmS24Be),
            (b"twos", 16, CodecId::PcmS16Be),
            (b"sowt", 16, CodecId::PcmS16Le),
            (b"sowt", 24, CodecId::PcmS24Le),
            (b"sowt", 32, CodecId::PcmS32Le),
            (b"in24", 24, CodecId::PcmS24Be),
            (b"in32", 32, CodecId::PcmS32Be),
            (b"raw ", 8, CodecId::PcmU8),
            (b"fl32", 32, CodecId::PcmF32Be),
            (b"FL32", 32, CodecId::PcmF32Be),
            (b"fl64", 64, CodecId::PcmF64Be),
            (b"FL64", 64, CodecId::PcmF64Be),
            (b"alaw", 8, CodecId::PcmAlaw),
            (b"ULAW", 16, CodecId::PcmMulaw),
        ];
        for (id, bits, codec) in cases {
            let mut compression = id.to_vec();
            compression.extend_from_slice(&[0, 0]);
            let parsed = parse_comm(&comm(2, 0, bits, 8000.0, &compression), true).unwrap();
            assert_eq!(parsed.codec, codec, "{}", fourcc(id));
        }
        // Without the compression field an AIFF-C COMM is uncompressed.
        let parsed = parse_comm(&comm(1, 0, 8, 8000.0, b""), true).unwrap();
        assert_eq!(parsed.codec, CodecId::PcmS8);
        // Plain AIFF ignores trailing bytes, whatever they look like.
        let parsed = parse_comm(&comm(1, 0, 16, 8000.0, b"fl32"), false).unwrap();
        assert_eq!(parsed.codec, CodecId::PcmS16Be);
    }

    #[test]
    fn sowt_file_reads_as_little_endian() {
        let bytes = build(
            b"AIFC",
            &[
                (b"FVER", AIFC_VERSION_1.to_be_bytes().to_vec()),
                (b"COMM", comm(2, 1, 16, 44100.0, b"sowt\0\0")),
                (b"SSND", ssnd(0, &[1, 0, 2, 0])),
            ],
        );
        let (d, data) = read_all(bytes);
        assert_eq!(d.streams()[0].codec, CodecId::PcmS16Le);
        assert_eq!(data, [1, 0, 2, 0]);
    }

    #[test]
    fn unknown_sizes_read_to_end() {
        // What a writer on a pipe leaves: FORM, frame count and SSND size 0.
        let mut bytes = write_aiff(CodecId::PcmS16Be, 1, 8000, &[], &[]);
        assert_eq!(&bytes[38..42], b"SSND");
        for field in [4..8, 22..26, 42..46] {
            bytes[field].fill(0);
        }
        bytes.extend_from_slice(&[0, 1, 0, 2, 0]);
        let (d, data) = read_all(bytes);
        assert_eq!(d.streams()[0].duration, None);
        assert_eq!(data, [0, 1, 0, 2]);
    }

    #[test]
    fn unpatched_output_reads_to_end() {
        // What the muxer leaves on a pipe, where the trailer cannot seek.
        let info = stream(CodecId::PcmS16Be, 2, 8000);
        let mut mux = AiffMuxer::new(Output::memory(), Vec::new());
        mux.write_header(std::slice::from_ref(&info)).unwrap();
        mux.write_packet(&Packet::new(0, vec![1, 2, 3, 4, 5, 6, 7, 8], 0, 2))
            .unwrap();
        let bytes = mux.into_output().into_bytes().unwrap();
        assert_eq!(&bytes[4..8], &[0xFF; 4]);
        assert_eq!(&bytes[22..26], &[0; 4]);
        assert_eq!(&bytes[42..46], &[0xFF; 4]);
        let (d, data) = read_all(bytes);
        assert_eq!(d.streams()[0].duration, None);
        assert_eq!(data, [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn ssnd_size_bounds_the_samples() {
        let bytes = build(
            b"AIFF",
            &[
                (b"COMM", comm(1, 0, 8, 8000.0, b"")),
                (b"SSND", ssnd(0, &[1, 2, 3])),
                (b"ANNO", b"not audio".to_vec()),
            ],
        );
        let (d, data) = read_all(bytes);
        assert_eq!(d.streams()[0].duration, Some(3));
        assert_eq!(data, [1, 2, 3]);
    }

    #[test]
    fn truncated_sound_keeps_whole_frames() {
        let mut bytes = write_aiff(CodecId::PcmS24Be, 2, 8000, &[7; 60], &[]);
        bytes.truncate(bytes.len() - 10);
        let (_, data) = read_all(bytes);
        assert_eq!(data.len(), 48);
    }

    #[test]
    fn rejects_malformed_files() {
        let ok_comm = || comm(1, 0, 16, 8000.0, b"");
        let cases: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"FORM\0\0\0\x04".to_vec(),
            b"RIFF\0\0\0\x04AIFF".to_vec(),
            b"FORM\0\0\0\x04WAVE".to_vec(),
            // No SSND at all.
            build(b"AIFF", &[(b"COMM", ok_comm())]),
            // Sound before COMM.
            build(
                b"AIFF",
                &[(b"SSND", ssnd(0, &[0, 0])), (b"COMM", ok_comm())],
            ),
            // Truncated COMM.
            build(b"AIFF", &[(b"COMM", ok_comm()[..10].to_vec())]),
            // Zero and negative channel counts.
            build(b"AIFF", &[(b"COMM", comm(0, 0, 16, 8000.0, b""))]),
            build(b"AIFF", &[(b"COMM", comm(-2, 0, 16, 8000.0, b""))]),
            // Sample sizes out of range.
            build(b"AIFF", &[(b"COMM", comm(1, 0, 0, 8000.0, b""))]),
            build(b"AIFF", &[(b"COMM", comm(1, 0, 33, 8000.0, b""))]),
            // Unusable rate.
            build(b"AIFF", &[(b"COMM", comm(1, 0, 16, 0.0, b""))]),
            // Unknown compression.
            build(b"AIFC", &[(b"COMM", comm(1, 0, 16, 8000.0, b"ima4\0\0"))]),
            // SSND offset beyond its own size.
            build(
                b"AIFF",
                &[
                    (b"COMM", ok_comm()),
                    (b"SSND", {
                        let mut s = ssnd(0, &[0, 0]);
                        s[3] = 200;
                        s
                    }),
                ],
            ),
        ];
        for (i, bytes) in cases.into_iter().enumerate() {
            assert!(demux(bytes).is_err(), "case {i} should fail");
        }
        let unknown = build(b"AIFC", &[(b"COMM", comm(1, 0, 16, 8000.0, b"ima4\0\0"))]);
        assert!(matches!(demux(unknown), Err(Error::Unsupported(_))));
    }

    #[test]
    fn huge_chunk_sizes_do_not_allocate() {
        // A text chunk and an unknown chunk claiming almost 4 GiB in a tiny
        // file: they must fail on the missing bytes, not on an allocation.
        for id in [b"ANNO", b"junk", b"COMM"] {
            let mut bytes = b"FORM\0\0\0\0AIFF".to_vec();
            bytes.extend_from_slice(id);
            bytes.extend_from_slice(&0xFFFF_FFF0u32.to_be_bytes());
            bytes.extend_from_slice(b"short");
            assert!(demux(bytes).is_err());
        }
    }

    #[test]
    fn muxer_rejects_what_aiff_cannot_hold() {
        let mut mux = AiffMuxer::new(Output::memory(), Vec::new());
        assert!(
            mux.write_header(&[stream(CodecId::PcmS16Le, 1, 8000)])
                .is_err()
        );
        assert!(mux.write_header(&[]).is_err());
        assert!(
            mux.write_header(&[stream(CodecId::PcmS16Be, 40000, 8000)])
                .is_err()
        );
    }

    #[test]
    fn reads_reference_files() {
        // Small files from an independent writer, with the same 80 stereo
        // frames stored raw alongside for comparison.
        let raw_s16 = fixture("sine_s16be.raw");
        let (d, data) = read_all(fixture("sine_s16be.aiff"));
        assert_eq!(d.streams()[0].codec, CodecId::PcmS16Be);
        assert_eq!(d.streams()[0].duration, Some(80));
        assert_eq!(d.streams()[0].audio().unwrap().sample_rate, 8000);
        assert_eq!(data, raw_s16);
        assert!(
            d.metadata()
                .contains(&("title".to_string(), "Fixture".to_string()))
        );

        let (d, data) = read_all(fixture("sine_f32be.aifc"));
        assert_eq!(d.streams()[0].codec, CodecId::PcmF32Be);
        assert_eq!(data, fixture("sine_f32be.raw"));

        let (d, data) = read_all(fixture("sine_alaw.aifc"));
        assert_eq!(d.streams()[0].codec, CodecId::PcmAlaw);
        assert_eq!(data, fixture("sine_alaw.raw"));
    }
}
