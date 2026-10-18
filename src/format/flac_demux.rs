//! Native FLAC streams: the `fLaC` marker, metadata blocks and frames.
//!
//! Written from RFC 9639 ("Free Lossless Audio Codec"): section 6 for the
//! stream layout, 8 for metadata blocks (8.2 STREAMINFO, 8.6 Vorbis
//! comment) and 9.1 for the frame header. An ID3v2 tag in front of the
//! marker, which some taggers add, is skipped using the tag size from the
//! ID3v2.4 structure document (section 3.1).
//!
//! Frames carry no length field, so a frame ends where the next one begins.
//! A candidate boundary is accepted only when a valid frame header starts
//! there (sync code, CRC-8, parameters matching STREAMINFO) and the bytes
//! before it close the current frame with a matching CRC-16. That way
//! sync-like byte patterns inside frame data are not mistaken for
//! boundaries: a false one would need a sync code, a matching CRC-8 and a
//! matching CRC-16 at once.
//!
//! The earliest such boundary is always taken. CRC-16 is linear with a
//! zero initial value, so a run of several whole frames also leaves a zero
//! remainder; any later candidate would merge frames. Frame numbers are
//! not required to follow on, so streams with a missing frame still split
//! correctly around the gap.

use std::io::Read;

use crate::codec::flac::{STREAMINFO_SIZE, StreamInfoBlock};
use crate::error::{Error, Result};
use crate::format::{Demuxer, InputFormat, InputOptions};
use crate::rational::Rational;
use crate::stream::{AudioParams, CodecId, Packet, StreamInfo, StreamParams};
use crate::util::bytes::ReadBytes;
use crate::util::crc;

pub const INPUT: InputFormat = InputFormat {
    name: "flac",
    long_name: "raw FLAC",
    extensions: &["flac"],
    probe,
    open,
    raw_codec: None,
};

/// Bytes requested from the reader at a time.
const CHUNK: usize = 64 * 1024;

/// Longest possible frame header: 4 fixed bytes, a 7-byte coded number,
/// 2 bytes of block size, 2 of sample rate and the CRC-8.
const MAX_HEADER: usize = 16;

const BLOCK_STREAMINFO: u8 = 0;
const BLOCK_VORBIS_COMMENT: u8 = 4;
const BLOCK_INVALID: u8 = 127;

fn probe(data: &[u8]) -> u32 {
    if data.starts_with(b"fLaC") {
        return 100;
    }
    match id3v2_size(data) {
        Some(size) => match data.get(size..size + 4) {
            Some(marker) if marker == b"fLaC" => 100,
            Some(_) => 0,
            // The tag runs past the probe window (embedded cover art):
            // a weak claim, so the extension or a stronger probe wins.
            None => 10,
        },
        None => 0,
    }
}

/// Total size of an ID3v2 tag at the start of `data`, header and optional
/// footer included, or `None` when there is no well formed tag header.
fn id3v2_size(data: &[u8]) -> Option<usize> {
    if data.len() < 10 || &data[..3] != b"ID3" {
        return None;
    }
    let flags = data[5];
    let size_bytes = &data[6..10];
    // The size is "synchsafe": four 7-bit groups, high bit always clear.
    if size_bytes.iter().any(|b| b & 0x80 != 0) {
        return None;
    }
    let size = size_bytes
        .iter()
        .fold(0usize, |acc, &b| acc << 7 | b as usize);
    let footer = if flags & 0x10 != 0 { 10 } else { 0 };
    Some(10 + size + footer)
}

fn open(reader: Box<dyn Read>, _options: &InputOptions) -> Result<Box<dyn Demuxer>> {
    Ok(Box::new(FlacDemuxer::new(reader)?))
}

/// The frame header fields the demuxer needs to find and stamp frames.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FrameHeader {
    variable: bool,
    block_size: u32,
    sample_rate: Option<u32>,
    channels: u8,
    bits_per_sample: Option<u32>,
    /// Frame number (fixed blocking) or first sample number (variable).
    number: u64,
    /// Header length in bytes, CRC-8 included.
    len: usize,
}

/// Parses a frame header at the start of `b`. Returns `None` when the bytes
/// are not a valid header, including when they stop short of one.
///
/// This mirrors the decoder's header parser on purpose: it runs at every
/// candidate sync position, so it works on bytes and fails without
/// building error values.
fn parse_header(b: &[u8]) -> Option<FrameHeader> {
    let byte = |i: usize| b.get(i).copied();
    if byte(0)? != 0xFF || byte(1)? & 0xFE != 0xF8 {
        return None;
    }
    let variable = b[1] & 1 == 1;
    let codes = byte(2)?;
    let (block_code, rate_code) = (codes >> 4, codes & 0x0F);
    let format = byte(3)?;
    let channels = match format >> 4 {
        n @ 0..=7 => n + 1,
        8..=10 => 2,
        _ => return None,
    };
    let bits_per_sample = match (format >> 1) & 0x07 {
        0 => None,
        1 => Some(8),
        2 => Some(12),
        3 => return None,
        4 => Some(16),
        5 => Some(20),
        6 => Some(24),
        _ => Some(32),
    };
    if format & 1 != 0 || block_code == 0 || rate_code == 15 {
        return None;
    }

    // Coded number: an extended UTF-8 sequence of 1 to 7 bytes.
    let first = byte(4)?;
    let (extra, mut number) = match first.leading_ones() {
        0 => (0, first as u64),
        1 | 8 => return None,
        7 => (6, 0),
        n => (n - 1, (first & (0x7F >> n)) as u64),
    };
    let mut pos = 5;
    for _ in 0..extra {
        let c = byte(pos)?;
        if c & 0xC0 != 0x80 {
            return None;
        }
        number = number << 6 | (c & 0x3F) as u64;
        pos += 1;
    }
    if !variable && number >= 1 << 31 {
        return None;
    }

    let mut field = |len: usize| -> Option<u32> {
        let v = b
            .get(pos..pos + len)?
            .iter()
            .fold(0u32, |a, &x| a << 8 | x as u32);
        pos += len;
        Some(v)
    };
    let block_size = match block_code {
        1 => 192,
        2..=5 => 576 << (block_code - 2),
        6 => field(1)? + 1,
        7 => field(2)? + 1,
        _ => 256 << (block_code - 8),
    };
    let sample_rate = match rate_code {
        0 => None,
        1 => Some(88_200),
        2 => Some(176_400),
        3 => Some(192_000),
        4 => Some(8_000),
        5 => Some(16_000),
        6 => Some(22_050),
        7 => Some(24_000),
        8 => Some(32_000),
        9 => Some(44_100),
        10 => Some(48_000),
        11 => Some(96_000),
        12 => Some(field(1)? * 1000),
        13 => Some(field(2)?),
        _ => Some(field(2)? * 10),
    };
    if sample_rate == Some(0) || crc::crc8(&b[..pos]) != byte(pos)? {
        return None;
    }
    Some(FrameHeader {
        variable,
        block_size,
        sample_rate,
        channels,
        bits_per_sample,
        number,
        len: pos + 1,
    })
}

pub struct FlacDemuxer {
    reader: Box<dyn Read>,
    streams: Vec<StreamInfo>,
    metadata: Vec<(String, String)>,
    info: StreamInfoBlock,
    /// Frame bytes read ahead; `buf[pos..]` is not yet returned.
    buf: Vec<u8>,
    pos: usize,
    eof: bool,
    /// Block size of a fixed blocking stream, which turns frame numbers
    /// into sample positions.
    fixed_block: u64,
}

impl FlacDemuxer {
    pub fn new(mut reader: Box<dyn Read>) -> Result<Self> {
        let mut marker: [u8; 4] = reader.read_fixed().map_err(eof_as_invalid)?;
        if &marker[..3] == b"ID3" {
            let mut head = [0u8; 10];
            head[..4].copy_from_slice(&marker);
            reader.read_exact(&mut head[4..]).map_err(eof_as_invalid)?;
            let size = id3v2_size(&head)
                .ok_or_else(|| Error::invalid_data("malformed ID3v2 tag before FLAC stream"))?;
            reader.skip((size - 10) as u64).map_err(eof_as_invalid)?;
            marker = reader.read_fixed().map_err(eof_as_invalid)?;
        }
        if &marker != b"fLaC" {
            return Err(Error::invalid_data("not a FLAC stream (no fLaC marker)"));
        }

        let mut info: Option<(StreamInfoBlock, Vec<u8>)> = None;
        let mut metadata = Vec::new();
        loop {
            let header = reader.read_u32_be().map_err(eof_as_invalid)?;
            let last = header >> 31 == 1;
            let kind = (header >> 24) as u8 & 0x7F;
            let len = header & 0x00FF_FFFF;
            match (kind, &info) {
                (BLOCK_STREAMINFO, None) => {
                    if len as usize != STREAMINFO_SIZE {
                        return Err(Error::invalid_data(
                            "FLAC STREAMINFO block has the wrong size",
                        ));
                    }
                    let body = reader.read_vec(STREAMINFO_SIZE).map_err(eof_as_invalid)?;
                    info = Some((StreamInfoBlock::parse(&body)?, body));
                }
                (_, None) => {
                    return Err(Error::invalid_data(
                        "FLAC stream does not start with a STREAMINFO block",
                    ));
                }
                (BLOCK_INVALID, _) => {
                    return Err(Error::invalid_data(
                        "FLAC metadata block type 127 is invalid",
                    ));
                }
                (BLOCK_VORBIS_COMMENT, _) => {
                    let body = reader.read_vec(len as usize).map_err(eof_as_invalid)?;
                    metadata.extend(parse_vorbis_comment(&body));
                }
                // Padding, application data, seek table, cue sheet, picture
                // and reserved types carry nothing the pipeline uses.
                _ => reader.skip(len as u64).map_err(eof_as_invalid)?,
            }
            if last {
                break;
            }
        }
        // Unreachable in practice (a first block other than STREAMINFO is
        // rejected above), but an error costs nothing over a panic.
        let (info, body) =
            info.ok_or_else(|| Error::invalid_data("FLAC stream has no STREAMINFO block"))?;

        let params = AudioParams {
            sample_rate: info.sample_rate,
            channels: info.channels as u16,
            bits_per_sample: info.bits_per_sample as u32,
        };
        let mut stream = StreamInfo::new(
            0,
            CodecId::Flac,
            StreamParams::Audio(params),
            Rational::new(1, info.sample_rate as i64),
        );
        if info.total_samples > 0 {
            stream.duration = Some(info.total_samples as i64);
        }
        stream.extradata = body;
        Ok(FlacDemuxer {
            reader,
            streams: vec![stream],
            metadata,
            fixed_block: info.max_block_size as u64,
            info,
            buf: Vec::new(),
            pos: 0,
            eof: false,
        })
    }

    /// Appends up to one chunk from the reader. Returns false at the end.
    fn fill(&mut self) -> Result<bool> {
        if self.eof {
            return Ok(false);
        }
        let old = self.buf.len();
        self.buf.resize(old + CHUNK, 0);
        let n = self.reader.read_full(&mut self.buf[old..])?;
        self.buf.truncate(old + n);
        // `read_full` only comes back short at the end of the stream.
        if n < CHUNK {
            self.eof = true;
        }
        Ok(n > 0)
    }

    /// Reads until `buf` holds `end` bytes or the input ends.
    fn ensure(&mut self, end: usize) -> Result<()> {
        while self.buf.len() < end && self.fill()? {}
        Ok(())
    }

    /// Drops bytes already returned, once they are the bulk of the buffer.
    fn compact(&mut self) {
        if self.pos > 0 && self.pos * 2 >= self.buf.len() {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
    }

    /// Whether a header agrees with STREAMINFO. Frames that change the
    /// stream parameters cannot be decoded here, and checking them filters
    /// out most false sync codes.
    fn matches_stream(&self, h: &FrameHeader) -> bool {
        h.channels == self.info.channels
            && h.sample_rate.is_none_or(|r| r == self.info.sample_rate)
            && h.bits_per_sample
                .is_none_or(|b| b == self.info.bits_per_sample as u32)
    }

    /// Advances `pos` to the next valid frame header and returns it, or
    /// `None` at the end of the input.
    fn sync(&mut self) -> Result<Option<FrameHeader>> {
        loop {
            match self.buf[self.pos..].iter().position(|&b| b == 0xFF) {
                Some(offset) => self.pos += offset,
                None => {
                    self.pos = self.buf.len();
                    self.compact();
                    if !self.fill()? {
                        return Ok(None);
                    }
                    continue;
                }
            }
            self.ensure(self.pos + MAX_HEADER)?;
            if let Some(h) = parse_header(&self.buf[self.pos..]) {
                if self.matches_stream(&h) {
                    return Ok(Some(h));
                }
            }
            self.pos += 1;
            self.compact();
        }
    }

    /// Upper bound on the size of the frame `h` starts. Verbatim coding is
    /// the worst an encoder should ever need; the bound doubles it to stay
    /// clear of unusual but valid frames, and honors STREAMINFO when it
    /// declares more.
    fn frame_size_limit(&self, h: &FrameHeader) -> usize {
        let channels = self.info.channels as usize;
        let bits = self.info.bits_per_sample as usize + 1;
        let verbatim = (h.block_size as usize * bits).div_ceil(8) * channels + channels * 8;
        (2 * verbatim).max(self.info.max_frame_size as usize) + MAX_HEADER + CHUNK
    }

    /// Finds where the frame starting at `pos` ends, or `None` if no
    /// boundary checks out before the size limit or the end of the input.
    fn find_end(&mut self, h: &FrameHeader) -> Result<Option<usize>> {
        let start = self.pos;
        let min_end = start + h.len + 3;
        let limit = start + self.frame_size_limit(h);
        // CRC-16 of buf[start..crc_pos], advanced lazily to each candidate.
        let mut crc = 0u16;
        let mut crc_pos = start;
        let mut scan = min_end;
        loop {
            // Only look at bytes that have a successor, as a sync code
            // spans two.
            let searchable = self.buf.len().saturating_sub(1);
            let found = self
                .buf
                .get(scan..searchable)
                .and_then(|s| s.iter().position(|&b| b == 0xFF));
            let Some(offset) = found else {
                if self.buf.len() >= limit {
                    return Ok(None);
                }
                scan = scan.max(searchable);
                if !self.fill()? {
                    return Ok(self.end_at_eof(start, min_end));
                }
                continue;
            };
            let i = scan + offset;
            if i >= limit {
                return Ok(None);
            }
            scan = i + 1;
            if self.buf[i + 1] & 0xFE != 0xF8 {
                continue;
            }
            crc = crc::crc16_update(crc, &self.buf[crc_pos..i]);
            crc_pos = i;
            if crc != 0 {
                continue;
            }
            self.ensure(i + MAX_HEADER)?;
            if let Some(next) = parse_header(&self.buf[i..]) {
                if next.variable == h.variable && self.matches_stream(&next) {
                    return Ok(Some(i));
                }
            }
        }
    }

    /// End of the last frame, which runs to the end of the input. A tag
    /// appended after the audio (ID3v1, APE) is left out when the frame
    /// checks out without it.
    ///
    /// Trailing zero bytes are deliberately not recognized: a CRC-16 that
    /// is zero stays zero over any number of zero bytes, so the frame end
    /// would be ambiguous whenever the stored CRC itself ends in zeros.
    fn end_at_eof(&self, start: usize, min_end: usize) -> Option<usize> {
        let len = self.buf.len();
        if len < min_end {
            return None;
        }
        let mut crc = crc::crc16_update(0, &self.buf[start..min_end]);
        for j in min_end..=len {
            if crc == 0 && is_trailer(&self.buf[j..]) {
                return Some(j);
            }
            if j < len {
                crc = crc::crc16_update(crc, &self.buf[j..j + 1]);
            }
        }
        None
    }
}

/// Recognizes what may follow the last complete frame: nothing, a tag, or
/// the start of a frame cut off before its header was complete.
fn is_trailer(rest: &[u8]) -> bool {
    let cut_frame = rest.len() < MAX_HEADER
        && rest.first() == Some(&0xFF)
        && rest.get(1).is_none_or(|&b| b & 0xFE == 0xF8);
    rest.is_empty()
        || cut_frame
        || rest.starts_with(b"TAG")
        || rest.starts_with(b"APETAGEX")
        || rest.starts_with(b"ID3")
}

/// A truncated header means a malformed file, not an I/O failure.
fn eof_as_invalid(e: std::io::Error) -> Error {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        Error::invalid_data("FLAC stream ends inside its metadata")
    } else {
        e.into()
    }
}

/// Reads the `KEY=value` fields of a Vorbis comment block (RFC 9639
/// section 8.6). Lengths are little-endian, unlike the rest of FLAC. Keys
/// are case insensitive, so they are lowercased; malformed trailing data
/// ends the list rather than failing the whole file.
fn parse_vorbis_comment(body: &[u8]) -> Vec<(String, String)> {
    let mut tags = Vec::new();
    let mut pos = 0usize;
    let mut take = |len: usize| -> Option<&[u8]> {
        let end = pos.checked_add(len).filter(|&e| e <= body.len())?;
        let s = &body[pos..end];
        pos = end;
        Some(s)
    };
    let Some(vendor_len) = take(4).map(le_u32) else {
        return tags;
    };
    if take(vendor_len as usize).is_none() {
        return tags;
    }
    let Some(count) = take(4).map(le_u32) else {
        return tags;
    };
    for _ in 0..count {
        let Some(len) = take(4).map(le_u32) else {
            break;
        };
        let Some(field) = take(len as usize) else {
            break;
        };
        let Some(eq) = field.iter().position(|&b| b == b'=') else {
            continue;
        };
        let key = String::from_utf8_lossy(&field[..eq]).to_ascii_lowercase();
        let value = String::from_utf8_lossy(&field[eq + 1..]).into_owned();
        if !key.is_empty() {
            tags.push((key, value));
        }
    }
    tags
}

fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

impl Demuxer for FlacDemuxer {
    fn format_name(&self) -> &'static str {
        "flac"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }

    fn read_packet(&mut self) -> Result<Option<Packet>> {
        loop {
            self.compact();
            let Some(h) = self.sync()? else {
                return Ok(None);
            };
            let Some(end) = self.find_end(&h)? else {
                // Corrupt or truncated frame: step past its sync code and
                // resume at the next valid header.
                self.pos += 1;
                continue;
            };
            let data = self.buf[self.pos..end].to_vec();
            self.pos = end;
            if self.fixed_block == 0 {
                // STREAMINFO did not say; the first frame's size is the
                // fixed size, as only the last frame may be shorter.
                self.fixed_block = h.block_size as u64;
            }
            let pts = if h.variable {
                h.number
            } else {
                h.number * self.fixed_block
            };
            return Ok(Some(Packet::new(0, data, pts as i64, h.block_size as i64)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::flac::new_decoder;
    use crate::frame::{AudioData, Frame};
    use crate::util::bits::BitWriter;
    use crate::util::md5::{Md5, to_hex};

    macro_rules! fixture {
        ($name:literal) => {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/flac-decoder/",
                $name
            ))
        };
    }

    /// Small encoded clips with the MD5 of their samples in the STREAMINFO
    /// layout, as computed by an independent decoder. Sample counts and
    /// packet timestamps come from the same reference.
    struct Fixture {
        bytes: &'static [u8],
        md5: &'static str,
        rate: u32,
        channels: u16,
        bits: u32,
        pts: &'static [i64],
        total: i64,
    }

    const FIXTURES: [Fixture; 6] = [
        Fixture {
            bytes: fixture!("stereo16_sweep.flac"),
            md5: "516ce86c69b80e9a95ebf0e9b62b9ee6",
            rate: 8000,
            channels: 2,
            bits: 16,
            pts: &[0, 576, 1152, 1728, 2304],
            total: 2400,
        },
        Fixture {
            bytes: fixture!("stereo16_variable.flac"),
            md5: "fb2835e90ac5fa8b7f3f71303314483f",
            rate: 8000,
            channels: 2,
            bits: 16,
            pts: &[0, 576, 768, 960, 1060, 1360],
            total: 1600,
        },
        Fixture {
            bytes: fixture!("mono24_noise_alt.flac"),
            md5: "393406ed27c2431078775b07ad29f593",
            rate: 8000,
            channels: 1,
            bits: 24,
            pts: &[0, 128, 256, 384],
            total: 500,
        },
        Fixture {
            bytes: fixture!("mono24_wasted.flac"),
            md5: "f8d499fafff388690a34942b261fd973",
            rate: 8000,
            channels: 1,
            bits: 24,
            pts: &[0, 192, 384],
            total: 400,
        },
        Fixture {
            bytes: fixture!("stereo32_noise.flac"),
            md5: "a690a6ac37f6717242fe399a54ff1cd9",
            rate: 8000,
            channels: 2,
            bits: 32,
            pts: &[0, 96, 192, 288],
            total: 320,
        },
        Fixture {
            bytes: fixture!("surround8.flac"),
            md5: "81eb328607792946810f3dccadfcd83a",
            rate: 8000,
            channels: 6,
            bits: 8,
            pts: &[0, 192, 384],
            total: 400,
        },
    ];

    fn demux(bytes: &[u8]) -> Result<FlacDemuxer> {
        FlacDemuxer::new(Box::new(std::io::Cursor::new(bytes.to_vec())))
    }

    fn packets(d: &mut FlacDemuxer) -> Vec<Packet> {
        let mut out = Vec::new();
        while let Some(p) = d.read_packet().unwrap() {
            out.push(p);
        }
        out
    }

    /// Decodes every packet and returns the MD5 of the samples in the
    /// STREAMINFO layout, plus the samples per channel decoded. The
    /// decoder's own signature check runs in `flush`.
    fn decode_md5(stream: &StreamInfo, packets: &[Packet]) -> Result<(String, i64)> {
        let mut decoder = new_decoder(stream)?;
        let bits = stream.audio().unwrap().bits_per_sample;
        let width = bits.div_ceil(8) as usize;
        let mut md5 = Md5::new();
        let mut count = 0;
        for p in packets {
            for f in decoder.decode(p)? {
                let Frame::Audio(a) = f else {
                    panic!("expected audio");
                };
                assert_eq!(a.pts, p.pts);
                count += a.nb_samples() as i64;
                let AudioData::Int { samples, bits: b } = &a.data else {
                    panic!("expected integer samples");
                };
                assert_eq!(*b, bits);
                for s in samples {
                    md5.update(&s.to_le_bytes()[..width]);
                }
            }
        }
        decoder.flush()?;
        Ok((to_hex(&md5.finalize()), count))
    }

    #[test]
    fn fixtures_decode_bit_exact() {
        for fx in &FIXTURES {
            let mut d = demux(fx.bytes).unwrap();
            let stream = d.streams()[0].clone();
            let a = *stream.audio().unwrap();
            assert_eq!(
                (a.sample_rate, a.channels, a.bits_per_sample),
                (fx.rate, fx.channels, fx.bits)
            );
            assert_eq!(stream.codec, CodecId::Flac);
            assert_eq!(stream.time_base, Rational::new(1, fx.rate as i64));
            assert_eq!(stream.duration, Some(fx.total));
            assert_eq!(stream.extradata.len(), STREAMINFO_SIZE);
            let info = StreamInfoBlock::parse(&stream.extradata).unwrap();
            assert_eq!(to_hex(&info.md5), fx.md5);

            let pkts = packets(&mut d);
            let pts: Vec<i64> = pkts.iter().map(|p| p.pts).collect();
            assert_eq!(pts, fx.pts);
            // Durations chain the timestamps and add up to the total.
            for w in pkts.windows(2) {
                assert_eq!(w[0].pts + w[0].duration, w[1].pts);
            }
            let last = pkts.last().unwrap();
            assert_eq!(last.pts + last.duration, fx.total);

            let (md5, count) = decode_md5(&stream, &pkts).unwrap();
            assert_eq!(md5, fx.md5);
            assert_eq!(count, fx.total);
        }
    }

    #[test]
    fn fixtures_contain_false_sync_codes() {
        // Noise-like frame data holds byte pairs that look like a sync
        // code; the splitter must have stepped over them.
        let inner_syncs: usize = FIXTURES
            .iter()
            .map(|fx| {
                let mut d = demux(fx.bytes).unwrap();
                packets(&mut d)
                    .iter()
                    .map(|p| {
                        p.data[1..]
                            .windows(2)
                            .filter(|w| w[0] == 0xFF && w[1] & 0xFE == 0xF8)
                            .count()
                    })
                    .sum::<usize>()
            })
            .sum();
        assert!(inner_syncs > 0);
    }

    #[test]
    fn vorbis_comments_become_lowercase_tags() {
        let d = demux(FIXTURES[0].bytes).unwrap();
        let tags = d.metadata();
        assert!(tags.contains(&("title".into(), "Sweep".into())));
        assert!(tags.contains(&("artist".into(), "Nobody".into())));
    }

    #[test]
    fn vorbis_comment_parsing_is_bounded() {
        let mut body = Vec::new();
        body.extend_from_slice(&3u32.to_le_bytes());
        body.extend_from_slice(b"abc");
        body.extend_from_slice(&1000u32.to_le_bytes());
        for field in [&b"ALBUM=X=Y"[..], b"novalue", b"=empty key", b"Genre="] {
            body.extend_from_slice(&(field.len() as u32).to_le_bytes());
            body.extend_from_slice(field);
        }
        // A field length running past the block ends the list.
        body.extend_from_slice(&u32::MAX.to_le_bytes());
        body.extend_from_slice(b"TRACK=1");
        assert_eq!(
            parse_vorbis_comment(&body),
            vec![
                ("album".to_string(), "X=Y".to_string()),
                ("genre".to_string(), String::new()),
            ]
        );
        assert!(parse_vorbis_comment(&[]).is_empty());
        assert!(parse_vorbis_comment(&[0xFF, 0xFF, 0xFF, 0xFF, 0]).is_empty());
    }

    fn id3_tag(payload: usize, footer: bool) -> Vec<u8> {
        let mut tag = b"ID3\x04\x00".to_vec();
        tag.push(if footer { 0x10 } else { 0 });
        for shift in [21, 14, 7, 0] {
            tag.push(((payload >> shift) & 0x7F) as u8);
        }
        tag.resize(10 + payload, 0);
        if footer {
            tag.extend_from_slice(b"3DI\x04\x00\x10\x00\x00\x00\x00");
        }
        tag
    }

    #[test]
    fn id3v2_before_the_stream_is_skipped() {
        let fx = &FIXTURES[0];
        for (payload, footer) in [(0, false), (300, false), (200, true), (100_000, false)] {
            let mut bytes = id3_tag(payload, footer);
            bytes.extend_from_slice(fx.bytes);
            let mut d = demux(&bytes).unwrap();
            let stream = d.streams()[0].clone();
            let (md5, _) = decode_md5(&stream, &packets(&mut d)).unwrap();
            assert_eq!(md5, fx.md5);
            let expected = if bytes.len() > 4096 && payload > 4000 {
                10
            } else {
                100
            };
            assert_eq!(probe(&bytes[..bytes.len().min(4096)]), expected);
        }
    }

    #[test]
    fn probe_scores() {
        assert_eq!(probe(b"fLaC\0\0\0\x22"), 100);
        assert_eq!(probe(b"RIFF....WAVE"), 0);
        assert_eq!(probe(b""), 0);
        let mut other = id3_tag(16, false);
        other.extend_from_slice(b"\xFF\xFB\x90\x00");
        assert_eq!(probe(&other), 0);
        // A malformed size byte is not an ID3v2 header.
        assert_eq!(probe(b"ID3\x04\x00\x00\x80\x00\x00\x00fLaC"), 0);
    }

    #[test]
    fn tags_after_the_last_frame_are_ignored() {
        let fx = &FIXTURES[1];
        let mut id3v1 = b"TAG".to_vec();
        id3v1.resize(128, b'x');
        let mut ape = b"APETAGEX".to_vec();
        ape.resize(64, 0xFF);
        for trailer in [id3v1, ape] {
            let mut bytes = fx.bytes.to_vec();
            bytes.extend_from_slice(&trailer);
            let mut d = demux(&bytes).unwrap();
            let stream = d.streams()[0].clone();
            let pkts = packets(&mut d);
            assert_eq!(pkts.len(), fx.pts.len());
            assert_eq!(decode_md5(&stream, &pkts).unwrap().0, fx.md5);
        }
    }

    #[test]
    fn truncated_streams_lose_only_the_broken_frame() {
        let fx = &FIXTURES[0];
        let full = {
            let mut d = demux(fx.bytes).unwrap();
            packets(&mut d)
        };
        let last_start = fx.bytes.len() - full.last().unwrap().data.len();
        for cut in [fx.bytes.len() - 1, last_start + 20, last_start + 1] {
            let mut d = demux(&fx.bytes[..cut]).unwrap();
            let pkts = packets(&mut d);
            assert_eq!(pkts.len(), full.len() - 1, "cut at {cut}");
            assert_eq!(pkts[..], full[..full.len() - 1]);
        }
        // Inside the metadata, the file is unusable.
        for cut in [0, 3, 10, 40] {
            assert!(demux(&fx.bytes[..cut]).is_err(), "cut at {cut}");
        }
    }

    /// Byte offsets of each frame in a fixture.
    fn frame_offsets(bytes: &[u8]) -> Vec<usize> {
        let mut d = demux(bytes).unwrap();
        let pkts = packets(&mut d);
        let total: usize = pkts.iter().map(|p| p.data.len()).sum();
        let mut pos = bytes.len() - total;
        pkts.iter()
            .map(|p| {
                let start = pos;
                pos += p.data.len();
                start
            })
            .collect()
    }

    #[test]
    fn corrupt_frame_is_skipped_and_the_rest_kept() {
        let fx = &FIXTURES[0];
        let offsets = frame_offsets(fx.bytes);
        let mut bytes = fx.bytes.to_vec();
        bytes[offsets[2] + 40] ^= 0x5A;
        let mut d = demux(&bytes).unwrap();
        let pkts = packets(&mut d);
        let pts: Vec<i64> = pkts.iter().map(|p| p.pts).collect();
        assert_eq!(pts, vec![0, 576, 1728, 2304]);
        // Every packet handed out still decodes; the MD5 check is skipped
        // because the stream is incomplete.
        let stream = d.streams()[0].clone();
        assert_eq!(decode_md5(&stream, &pkts).unwrap().1, 2400 - 576);
    }

    #[test]
    fn garbage_between_frames_is_skipped() {
        let fx = &FIXTURES[0];
        let offsets = frame_offsets(fx.bytes);
        let mut bytes = fx.bytes[..offsets[3]].to_vec();
        // Junk that includes sync-like pairs.
        for i in 0..500u32 {
            bytes.push(if i % 7 == 0 {
                0xFF
            } else {
                (i * 37 % 251) as u8 | 0x08
            });
        }
        bytes.extend_from_slice(&fx.bytes[offsets[3]..]);
        let mut d = demux(&bytes).unwrap();
        let pkts = packets(&mut d);
        let pts: Vec<i64> = pkts.iter().map(|p| p.pts).collect();
        // The frame before the junk cannot be closed by a CRC-16 that
        // matches, so it is lost with the junk.
        assert_eq!(pts, vec![0, 576, 1728, 2304]);
    }

    #[test]
    fn missing_frame_does_not_merge_neighbors() {
        let fx = &FIXTURES[0];
        let offsets = frame_offsets(fx.bytes);
        // Frame 2 removed entirely: frame 1 is followed by frame 3, whose
        // number does not follow on, but its CRCs check out.
        let mut bytes = fx.bytes[..offsets[2]].to_vec();
        bytes.extend_from_slice(&fx.bytes[offsets[3]..]);
        let mut d = demux(&bytes).unwrap();
        let pts: Vec<i64> = packets(&mut d).iter().map(|p| p.pts).collect();
        assert_eq!(pts, vec![0, 576, 1728, 2304]);
    }

    /// A reader that hands out data in tiny pieces, to exercise refills at
    /// every possible boundary.
    struct Trickle(Vec<u8>, usize);

    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf.len().min(3).min(self.0.len() - self.1);
            buf[..n].copy_from_slice(&self.0[self.1..self.1 + n]);
            self.1 += n;
            Ok(n)
        }
    }

    #[test]
    fn short_reads_give_the_same_packets() {
        for fx in &FIXTURES {
            let mut a = demux(fx.bytes).unwrap();
            let mut b = FlacDemuxer::new(Box::new(Trickle(fx.bytes.to_vec(), 0))).unwrap();
            assert_eq!(packets(&mut a), packets(&mut b));
        }
    }

    // Synthetic streams: verbatim mono 16-bit frames built here, for cases
    // the fixtures cannot reach (frames bigger than the read chunk, fake
    // headers planted inside frame data, malformed metadata).

    fn synth_info(block: u16, total: u64) -> StreamInfoBlock {
        StreamInfoBlock {
            min_block_size: block,
            max_block_size: block,
            sample_rate: 8000,
            channels: 1,
            bits_per_sample: 16,
            total_samples: total,
            ..StreamInfoBlock::default()
        }
    }

    /// Frame header bytes for a mono 16-bit 8000 Hz frame with an explicit
    /// 16-bit block size and a one byte frame number.
    fn synth_header(number: u8, block: u16) -> Vec<u8> {
        let mut h = vec![0xFF, 0xF8, 0x74, 0x08, number];
        h.extend_from_slice(&(block - 1).to_be_bytes());
        h.push(crc::crc8(&h));
        h
    }

    fn synth_frame(number: u8, samples: &[i16]) -> Vec<u8> {
        let mut w = BitWriter::new();
        for &b in &synth_header(number, samples.len() as u16) {
            w.write_bits(b as u32, 8);
        }
        w.write_bits(0b0000_0010, 8);
        for &s in samples {
            w.write_bits(s as u16 as u32, 16);
        }
        let mut bytes = w.into_bytes();
        let crc = crc::crc16(&bytes);
        bytes.extend_from_slice(&crc.to_be_bytes());
        bytes
    }

    fn synth_stream(
        info: &StreamInfoBlock,
        extra_blocks: &[(u8, Vec<u8>)],
        frames: &[Vec<u8>],
    ) -> Vec<u8> {
        let mut out = b"fLaC".to_vec();
        let mut blocks = vec![(0u8, info.serialize().to_vec())];
        blocks.extend_from_slice(extra_blocks);
        let n = blocks.len();
        for (i, (kind, body)) in blocks.iter().enumerate() {
            let last = if i + 1 == n { 0x80 } else { 0 };
            out.push(last | kind);
            out.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
            out.extend_from_slice(body);
        }
        for f in frames {
            out.extend_from_slice(f);
        }
        out
    }

    #[test]
    fn frames_larger_than_a_read_chunk() {
        let block = 40_000u16;
        let frames: Vec<Vec<u8>> = (0..3u8)
            .map(|n| {
                let samples: Vec<i16> = (0..block as i32)
                    .map(|i| ((i * 7919 + n as i32 * 31) % 65536 - 32768) as i16)
                    .collect();
                synth_frame(n, &samples)
            })
            .collect();
        assert!(frames[0].len() > CHUNK);
        let bytes = synth_stream(&synth_info(block, 3 * block as u64), &[], &frames);
        let mut d = demux(&bytes).unwrap();
        let pkts = packets(&mut d);
        assert_eq!(pkts.len(), 3);
        for (p, f) in pkts.iter().zip(&frames) {
            assert_eq!(&p.data, f);
        }
        assert_eq!(pkts[2].pts, 2 * block as i64);
        let stream = d.streams()[0].clone();
        assert_eq!(decode_md5(&stream, &pkts).unwrap().1, 3 * block as i64);
    }

    #[test]
    fn planted_headers_inside_frames_are_not_boundaries() {
        // Samples whose bytes spell a valid header of the next frame, with
        // a correct CRC-8, several times over.
        let fake = synth_header(1, 64);
        assert_eq!(fake.len(), 8);
        // Four samples spell the header, then a zero sample separates the
        // copies.
        let samples: Vec<i16> = fake
            .chunks(2)
            .map(|p| i16::from_be_bytes([p[0], p[1]]))
            .chain([0])
            .cycle()
            .take(64)
            .collect();
        let frames = vec![
            synth_frame(0, &samples),
            synth_frame(1, &samples),
            synth_frame(2, &samples),
        ];
        // The planted copies really are valid headers.
        let planted = (1..frames[0].len())
            .filter(|&i| parse_header(&frames[0][i..]).is_some())
            .count();
        assert!(planted >= 10);
        let bytes = synth_stream(&synth_info(64, 192), &[], &frames);
        let mut d = demux(&bytes).unwrap();
        let pkts = packets(&mut d);
        assert_eq!(pkts.len(), 3);
        for (p, f) in pkts.iter().zip(&frames) {
            assert_eq!(&p.data, f);
        }
    }

    #[test]
    fn metadata_blocks_are_skipped_or_parsed() {
        let mut comment = Vec::new();
        comment.extend_from_slice(&0u32.to_le_bytes());
        comment.extend_from_slice(&1u32.to_le_bytes());
        comment.extend_from_slice(&9u32.to_le_bytes());
        comment.extend_from_slice(b"Album=Big");
        let blocks = vec![
            (1u8, vec![0; 100]),
            (2, b"TEST1234".to_vec()),
            (3, vec![0xFF; 18]),
            (4, comment),
            (5, vec![0; 50]),
            (6, vec![0; 40]),
            (99, vec![1, 2, 3]),
        ];
        let frames = vec![synth_frame(0, &[1, 2, 3, 4])];
        let bytes = synth_stream(&synth_info(4, 4), &blocks, &frames);
        let mut d = demux(&bytes).unwrap();
        assert_eq!(d.metadata(), &[("album".to_string(), "Big".to_string())]);
        assert_eq!(packets(&mut d).len(), 1);
    }

    #[test]
    fn malformed_headers_are_errors() {
        let info = synth_info(4, 4);
        let frames = vec![synth_frame(0, &[1, 2, 3, 4])];
        let good = synth_stream(&info, &[], &frames);
        assert!(demux(&good).is_ok());

        // Not a FLAC stream.
        assert!(demux(b"OggS\0\0\0\0").is_err());
        assert!(demux(b"").is_err());
        // First block is not STREAMINFO.
        let mut bad = good.clone();
        bad[4] = 0x81;
        assert!(demux(&bad).is_err());
        // STREAMINFO of the wrong size.
        let mut bad = good.clone();
        bad[7] = 33;
        assert!(demux(&bad).is_err());
        // Invalid block type after STREAMINFO.
        let bad = synth_stream(&info, &[(127, vec![])], &frames);
        assert!(demux(&bad).is_err());
        // A block longer than the file.
        let bad = synth_stream(&info, &[(1, vec![0; 10])], &[]);
        assert!(demux(&bad[..bad.len() - 5]).is_err());
        // Zero sample rate inside STREAMINFO.
        let mut zero = info.clone();
        zero.sample_rate = 0;
        let mut bad = good.clone();
        bad[8..42].copy_from_slice(&zero.serialize());
        assert!(demux(&bad).is_err());
        // Malformed ID3v2 size.
        let mut bad = b"ID3\x04\x00\x00\x80\x00\x00\x00".to_vec();
        bad.extend_from_slice(&good);
        assert!(demux(&bad).is_err());
    }

    #[test]
    fn frames_that_disagree_with_streaminfo_are_skipped() {
        // A stereo STREAMINFO with mono frames: nothing matches.
        let mut info = synth_info(4, 4);
        info.channels = 2;
        let bytes = synth_stream(&info, &[], &[synth_frame(0, &[1, 2, 3, 4])]);
        let mut d = demux(&bytes).unwrap();
        assert!(d.read_packet().unwrap().is_none());
    }

    #[test]
    fn stream_without_frames() {
        let bytes = synth_stream(&synth_info(4096, 0), &[], &[]);
        let mut d = demux(&bytes).unwrap();
        assert_eq!(d.streams()[0].duration, None);
        assert!(d.read_packet().unwrap().is_none());
        assert!(d.read_packet().unwrap().is_none());
    }

    #[test]
    fn mutated_frames_with_fixed_crcs_never_panic() {
        // Random damage with the CRCs recomputed, so the subframe parser
        // itself sees the garbage. Every outcome is fine except a panic
        // (including integer overflow in debug builds).
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        // Decoded, rejected by a CRC, rejected by the parser.
        let mut outcomes = [0usize; 3];
        for fx in &FIXTURES {
            let mut d = demux(fx.bytes).unwrap();
            let stream = d.streams()[0].clone();
            let pkts = packets(&mut d);
            for round in 0..300 {
                let mut p = pkts[round % pkts.len()].clone();
                let len = p.data.len();
                for _ in 0..1 + next() % 4 {
                    let i = (next() as usize) % (len - 2);
                    p.data[i] ^= 1 << (next() % 8);
                }
                if round % 3 == 0 {
                    // Sometimes cut the frame short as well.
                    let keep = 8 + (next() as usize) % (len - 8);
                    p.data.truncate(keep);
                }
                let n = p.data.len();
                if parse_header(&p.data).is_none() && n > 8 {
                    // Repair the header CRC-8 when the header length can
                    // be guessed from an undamaged copy.
                    let hlen = parse_header(&pkts[round % pkts.len()].data).unwrap().len;
                    if hlen < n {
                        p.data[hlen - 1] = crc::crc8(&p.data[..hlen - 1]);
                    }
                }
                if n >= 2 {
                    let crc = crc::crc16(&p.data[..n - 2]);
                    p.data[n - 2..].copy_from_slice(&crc.to_be_bytes());
                }
                let mut decoder = new_decoder(&stream).unwrap();
                match decoder.decode(&p) {
                    Ok(_) => outcomes[0] += 1,
                    Err(Error::InvalidData(m)) if m.contains("CRC") => outcomes[1] += 1,
                    Err(_) => outcomes[2] += 1,
                }
                let _ = decoder.flush();
            }
        }
        // Most cases must get past both CRCs into the subframe parser.
        assert!(outcomes[1] < outcomes[0] + outcomes[2], "{outcomes:?}");
    }

    #[test]
    fn header_parser_agrees_on_fields() {
        let h = parse_header(&synth_header(7, 300)).unwrap();
        assert_eq!(
            h,
            FrameHeader {
                variable: false,
                block_size: 300,
                sample_rate: Some(8000),
                channels: 1,
                bits_per_sample: Some(16),
                number: 7,
                len: 8,
            }
        );
        let mut bad = synth_header(7, 300);
        bad[7] ^= 1;
        assert!(parse_header(&bad).is_none());
        for cut in 0..8 {
            assert!(parse_header(&synth_header(7, 300)[..cut]).is_none());
        }
    }
}
