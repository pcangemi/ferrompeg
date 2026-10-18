//! FLAC frame decoder.
//!
//! Written from RFC 9639 ("Free Lossless Audio Codec"): section 9.1 for the
//! frame header, 9.2 for subframes (constant, verbatim, fixed and linear
//! prediction), 9.2.7 for the partitioned Rice residual, 9.3 for the frame
//! footer, and 8.2 for the STREAMINFO fields and its MD5 signature.
//!
//! Each packet holds exactly one frame, as split by the demuxer. Samples are
//! reconstructed in `i64`: a 32-bit stream has a 33-bit side channel, and
//! prediction sums of such samples do not fit in 32 bits.

use crate::codec::Decoder;
use crate::codec::flac::{STREAMINFO_SIZE, StreamInfoBlock};
use crate::error::{Error, Result};
use crate::frame::{AudioData, AudioFrame, Frame};
use crate::stream::{CodecId, Packet, StreamInfo};
use crate::util::bits::BitReader;
use crate::util::crc;
use crate::util::md5::Md5;

/// Highest LPC order (6-bit subframe type with order minus one in 5 bits).
const MAX_LPC_ORDER: usize = 32;

pub fn new_decoder(stream: &StreamInfo) -> Result<Box<dyn Decoder>> {
    if stream.codec != CodecId::Flac {
        return Err(Error::invalid_argument(format!(
            "flac decoder cannot decode {}",
            stream.codec
        )));
    }
    let info = if stream.extradata.len() >= STREAMINFO_SIZE {
        StreamInfoBlock::parse(&stream.extradata)?
    } else {
        // Without STREAMINFO the stream parameters stand in for it: frames
        // that defer to STREAMINFO still decode, and the MD5 is unknown.
        let a = stream
            .audio()
            .ok_or_else(|| Error::invalid_data("flac stream has no audio parameters"))?;
        if a.channels == 0 || a.channels > 8 || !(4..=32).contains(&a.bits_per_sample) {
            return Err(Error::invalid_data(
                "flac stream needs 1 to 8 channels of 4 to 32 bits",
            ));
        }
        StreamInfoBlock {
            sample_rate: a.sample_rate,
            channels: a.channels as u8,
            bits_per_sample: a.bits_per_sample as u8,
            ..StreamInfoBlock::default()
        }
    };
    if info.sample_rate == 0 {
        return Err(Error::invalid_data("flac stream has a zero sample rate"));
    }
    Ok(Box::new(FlacDecoder::new(info)))
}

/// How the channels of a frame are coded (RFC 9639 section 9.1.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelAssignment {
    /// 1 to 8 channels, each coded on its own.
    Independent(u8),
    /// Left, then left minus right.
    LeftSide,
    /// Left minus right, then right.
    SideRight,
    /// Mid (sum shifted right), then side.
    MidSide,
}

impl ChannelAssignment {
    pub fn channels(self) -> usize {
        match self {
            ChannelAssignment::Independent(n) => n as usize,
            _ => 2,
        }
    }

    /// Whether subframe `index` is a side channel, which carries one extra
    /// bit because a difference of two n-bit values needs n + 1 bits.
    fn is_side(self, index: usize) -> bool {
        matches!(
            (self, index),
            (ChannelAssignment::LeftSide, 1)
                | (ChannelAssignment::MidSide, 1)
                | (ChannelAssignment::SideRight, 0)
        )
    }
}

/// A parsed frame header. Fields the header defers to STREAMINFO are `None`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    /// Variable blocking strategy: `number` is a sample number rather than
    /// a frame number.
    pub variable: bool,
    pub block_size: u32,
    pub sample_rate: Option<u32>,
    pub assignment: ChannelAssignment,
    pub bits_per_sample: Option<u32>,
    pub number: u64,
    /// Header length in bytes, CRC-8 included.
    pub len: usize,
}

/// Parses and checks a frame header at the start of `data`.
pub fn parse_frame_header(data: &[u8]) -> Result<FrameHeader> {
    let mut r = BitReader::new(data);
    // 15-bit sync code 0b111111111111100, then the blocking strategy bit.
    if r.read_bits(15)? != 0x7FFC {
        return Err(Error::invalid_data(
            "flac frame does not start with a sync code",
        ));
    }
    let variable = r.read_bit()?;
    let block_code = r.read_bits(4)?;
    let rate_code = r.read_bits(4)?;
    let assignment = match r.read_bits(4)? {
        n @ 0..=7 => ChannelAssignment::Independent(n as u8 + 1),
        8 => ChannelAssignment::LeftSide,
        9 => ChannelAssignment::SideRight,
        10 => ChannelAssignment::MidSide,
        _ => {
            return Err(Error::invalid_data(
                "flac frame uses a reserved channel assignment",
            ));
        }
    };
    let bits_per_sample = match r.read_bits(3)? {
        0 => None,
        1 => Some(8),
        2 => Some(12),
        4 => Some(16),
        5 => Some(20),
        6 => Some(24),
        7 => Some(32),
        _ => {
            return Err(Error::invalid_data(
                "flac frame uses a reserved sample size",
            ));
        }
    };
    if r.read_bit()? {
        return Err(Error::invalid_data("flac frame header reserved bit is set"));
    }
    let number = read_coded_number(&mut r)?;
    // A frame number has at most 31 bits; only sample numbers use 36.
    if !variable && number >= 1 << 31 {
        return Err(Error::invalid_data("flac frame number is out of range"));
    }
    let block_size = match block_code {
        0 => return Err(Error::invalid_data("flac frame uses a reserved block size")),
        1 => 192,
        2..=5 => 576 << (block_code - 2),
        6 => r.read_bits(8)? + 1,
        7 => r.read_bits(16)? + 1,
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
        12 => Some(r.read_bits(8)? * 1000),
        13 => Some(r.read_bits(16)?),
        14 => Some(r.read_bits(16)? * 10),
        _ => {
            return Err(Error::invalid_data(
                "flac frame uses an invalid sample rate code",
            ));
        }
    };
    if sample_rate == Some(0) {
        return Err(Error::invalid_data(
            "flac frame declares a zero sample rate",
        ));
    }
    let len = r.byte_position();
    let stored = r.read_bits(8)? as u8;
    if crc::crc8(&data[..len]) != stored {
        return Err(Error::invalid_data("flac frame header CRC-8 mismatch"));
    }
    Ok(FrameHeader {
        variable,
        block_size,
        sample_rate,
        assignment,
        bits_per_sample,
        number,
        len: len + 1,
    })
}

/// Reads the frame or sample number, coded like an extended UTF-8
/// character of 1 to 7 bytes (RFC 9639 section 9.1.5).
fn read_coded_number(r: &mut BitReader) -> Result<u64> {
    let first = r.read_bits(8)?;
    let leading = (first as u8).leading_ones();
    let (extra, mut value) = match leading {
        0 => return Ok(first as u64),
        // A lone continuation byte, or eight leading ones, is not a valid
        // start byte.
        1 | 8 => {
            return Err(Error::invalid_data(
                "flac frame has a malformed coded number",
            ));
        }
        7 => (6, 0u64),
        n => (n - 1, (first & (0x7F >> n)) as u64),
    };
    for _ in 0..extra {
        let byte = r.read_bits(8)?;
        if byte & 0xC0 != 0x80 {
            return Err(Error::invalid_data(
                "flac frame has a malformed coded number",
            ));
        }
        value = value << 6 | (byte & 0x3F) as u64;
    }
    Ok(value)
}

struct FlacDecoder {
    info: StreamInfoBlock,
    channels: usize,
    bits: u32,
    /// One reconstruction buffer per channel, reused across frames.
    buffers: Vec<Vec<i64>>,
    md5: Md5,
    md5_scratch: Vec<u8>,
    /// Samples per channel decoded so far.
    decoded: u64,
    flushed: bool,
}

impl FlacDecoder {
    fn new(info: StreamInfoBlock) -> Self {
        FlacDecoder {
            channels: info.channels as usize,
            bits: info.bits_per_sample as u32,
            buffers: vec![Vec::new(); info.channels as usize],
            info,
            md5: Md5::new(),
            md5_scratch: Vec::new(),
            decoded: 0,
            flushed: false,
        }
    }

    fn decode_frame(&mut self, data: &[u8], packet_pts: i64) -> Result<AudioFrame> {
        let header = parse_frame_header(data)?;
        if data.len() < header.len + 2 {
            return Err(Error::invalid_data("flac frame is truncated"));
        }
        let body_end = data.len() - 2;
        let stored = u16::from_be_bytes([data[body_end], data[body_end + 1]]);
        if crc::crc16(&data[..body_end]) != stored {
            return Err(Error::invalid_data("flac frame CRC-16 mismatch"));
        }
        // The pipeline cannot follow parameter changes inside a stream, so
        // every frame must agree with STREAMINFO.
        if header.assignment.channels() != self.channels {
            return Err(Error::unsupported(format!(
                "flac frame has {} channels, the stream {}",
                header.assignment.channels(),
                self.channels
            )));
        }
        if header
            .sample_rate
            .is_some_and(|r| r != self.info.sample_rate)
        {
            return Err(Error::unsupported(
                "flac sample rate changes inside the stream",
            ));
        }
        if header.bits_per_sample.is_some_and(|b| b != self.bits) {
            return Err(Error::unsupported(
                "flac bit depth changes inside the stream",
            ));
        }

        let block_size = header.block_size as usize;
        let body = &data[header.len..body_end];
        let mut r = BitReader::new(body);
        for (ch, buf) in self.buffers.iter_mut().enumerate() {
            let bps = self.bits + header.assignment.is_side(ch) as u32;
            buf.clear();
            buf.resize(block_size, 0);
            decode_subframe(&mut r, body, bps, buf)?;
        }
        // Subframes end on a byte boundary padded with zero bits, right
        // before the CRC-16.
        let pad = (8 - r.bit_position() % 8) % 8;
        if r.read_bits(pad as u32)? != 0 || r.bits_left() != 0 {
            return Err(Error::invalid_data(
                "flac frame has data after its subframes",
            ));
        }

        let samples = self.interleave(header.assignment, block_size)?;
        self.update_md5(&samples);
        self.decoded += block_size as u64;

        let pts = if header.variable {
            header.number as i64
        } else if self.info.max_block_size > 0 {
            header.number as i64 * self.info.max_block_size as i64
        } else {
            packet_pts
        };
        Ok(AudioFrame::new(
            self.info.sample_rate,
            self.channels as u16,
            AudioData::Int {
                samples,
                bits: self.bits,
            },
            pts,
        ))
    }

    /// Undoes inter-channel decorrelation and interleaves the channels,
    /// checking that every sample fits the stream bit depth.
    fn interleave(&self, assignment: ChannelAssignment, n: usize) -> Result<Vec<i32>> {
        let (lo, hi) = sample_range(self.bits);
        let narrow = |v: i64| -> Result<i32> {
            if v < lo || v > hi {
                return Err(Error::invalid_data(
                    "flac sample exceeds the stream bit depth",
                ));
            }
            Ok(v as i32)
        };
        let mut out = Vec::with_capacity(n * self.channels);
        match assignment {
            ChannelAssignment::Independent(_) => {
                for i in 0..n {
                    for buf in &self.buffers {
                        // Already range checked against the subframe depth,
                        // which equals the stream depth here.
                        out.push(buf[i] as i32);
                    }
                }
            }
            ChannelAssignment::LeftSide => {
                for (&left, &side) in self.buffers[0].iter().zip(&self.buffers[1]) {
                    out.push(narrow(left)?);
                    out.push(narrow(left - side)?);
                }
            }
            ChannelAssignment::SideRight => {
                for (&side, &right) in self.buffers[0].iter().zip(&self.buffers[1]) {
                    out.push(narrow(side + right)?);
                    out.push(narrow(right)?);
                }
            }
            ChannelAssignment::MidSide => {
                for (&mid, &side) in self.buffers[0].iter().zip(&self.buffers[1]) {
                    // The encoder dropped the low bit of left + right; it
                    // equals the low bit of left - right.
                    let sum = (mid << 1) | (side & 1);
                    out.push(narrow((sum + side) >> 1)?);
                    out.push(narrow((sum - side) >> 1)?);
                }
            }
        }
        Ok(out)
    }

    /// Feeds samples to the MD5 in the STREAMINFO layout: interleaved,
    /// little-endian, each in the fewest whole bytes that hold the depth.
    fn update_md5(&mut self, samples: &[i32]) {
        let width = self.bits.div_ceil(8) as usize;
        self.md5_scratch.clear();
        self.md5_scratch.reserve(samples.len() * width);
        for &s in samples {
            self.md5_scratch
                .extend_from_slice(&s.to_le_bytes()[..width]);
        }
        self.md5.update(&self.md5_scratch);
    }
}

impl Decoder for FlacDecoder {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<Frame>> {
        let frame = self.decode_frame(&packet.data, packet.pts)?;
        Ok(vec![Frame::Audio(frame)])
    }

    fn flush(&mut self) -> Result<Vec<Frame>> {
        if self.flushed {
            return Ok(Vec::new());
        }
        self.flushed = true;
        let digest = std::mem::take(&mut self.md5).finalize();
        // The signature covers the whole stream, so it can only be checked
        // when every declared sample went through this decoder.
        let declared = self.info.md5 != [0; 16];
        if declared
            && self.info.total_samples > 0
            && self.decoded == self.info.total_samples
            && digest != self.info.md5
        {
            return Err(Error::invalid_data(
                "flac decoded audio does not match the STREAMINFO MD5 signature",
            ));
        }
        Ok(Vec::new())
    }
}

/// Smallest and largest value of a signed `bits`-bit integer (`bits` up to
/// 33, the depth of a 32-bit side channel).
fn sample_range(bits: u32) -> (i64, i64) {
    let half = 1i64 << (bits - 1);
    (-half, half - 1)
}

/// Reads an `n`-bit two's complement value, `n` up to 33.
fn read_signed(r: &mut BitReader, n: u32) -> Result<i64> {
    if n == 0 {
        return Ok(0);
    }
    let v = r.read_bits_u64(n)?;
    let shift = 64 - n;
    Ok(((v << shift) as i64) >> shift)
}

/// Decodes one subframe of `out.len()` samples at depth `bps` into `out`.
/// `data` is the buffer `r` reads, for the fast residual path.
fn decode_subframe(r: &mut BitReader, data: &[u8], bps: u32, out: &mut [i64]) -> Result<()> {
    if r.read_bit()? {
        return Err(Error::invalid_data("flac subframe padding bit is set"));
    }
    let kind = r.read_bits(6)?;
    // Wasted bits: low bits that are zero in every sample of the subframe
    // are left out and the samples shifted back up at the end.
    let wasted = if r.read_bit()? {
        r.read_unary()?.saturating_add(1)
    } else {
        0
    };
    if wasted >= bps {
        return Err(Error::invalid_data("flac subframe wastes all of its bits"));
    }
    let bps = bps - wasted;
    match kind {
        0 => {
            let v = read_signed(r, bps)?;
            out.fill(v);
        }
        1 => {
            for s in out.iter_mut() {
                *s = read_signed(r, bps)?;
            }
        }
        8..=12 => {
            let order = (kind - 8) as usize;
            read_warmup(r, bps, order, out)?;
            decode_residual(r, data, order, out)?;
            restore_fixed(order, bps, out)?;
        }
        32..=63 => {
            let order = (kind - 31) as usize;
            read_warmup(r, bps, order, out)?;
            let precision = r.read_bits(4)? + 1;
            if precision == 16 {
                return Err(Error::invalid_data(
                    "flac LPC subframe uses the invalid precision code",
                ));
            }
            // The shift is a signed field, but RFC 9639 gives a negative
            // shift no meaning, so it is rejected rather than guessed at.
            let shift = r.read_bits_signed(5)?;
            if shift < 0 {
                return Err(Error::invalid_data(
                    "flac LPC subframe has a negative shift",
                ));
            }
            let mut coefs = [0i64; MAX_LPC_ORDER];
            for c in coefs[..order].iter_mut() {
                *c = r.read_bits_signed(precision)? as i64;
            }
            decode_residual(r, data, order, out)?;
            restore_lpc(&coefs[..order], shift as u32, bps, out)?;
        }
        _ => {
            return Err(Error::invalid_data("flac subframe uses a reserved type"));
        }
    }
    if wasted > 0 {
        for s in out.iter_mut() {
            *s <<= wasted;
        }
    }
    Ok(())
}

fn read_warmup(r: &mut BitReader, bps: u32, order: usize, out: &mut [i64]) -> Result<()> {
    if order > out.len() {
        return Err(Error::invalid_data(
            "flac predictor order exceeds the block size",
        ));
    }
    for s in out[..order].iter_mut() {
        *s = read_signed(r, bps)?;
    }
    Ok(())
}

/// Reads the partitioned Rice residual into `out[order..]`.
fn decode_residual(r: &mut BitReader, data: &[u8], order: usize, out: &mut [i64]) -> Result<()> {
    let (param_bits, escape) = match r.read_bits(2)? {
        0 => (4, 15),
        1 => (5, 31),
        _ => {
            return Err(Error::invalid_data(
                "flac residual uses a reserved coding method",
            ));
        }
    };
    let partition_order = r.read_bits(4)?;
    let n = out.len();
    let partitions = 1usize << partition_order;
    if n % partitions != 0 || n / partitions < order {
        return Err(Error::invalid_data(
            "flac residual partition order does not fit the block",
        ));
    }
    let per_partition = n / partitions;
    let mut pos = order;
    for p in 0..partitions {
        let end = (p + 1) * per_partition;
        let param = r.read_bits(param_bits)?;
        let part = &mut out[pos..end];
        if param == escape {
            // Escaped partition: plain signed values of a given width, or
            // all zeros when the width is 0.
            let width = r.read_bits(5)?;
            for s in part.iter_mut() {
                *s = r.read_bits_signed(width)? as i64;
            }
        } else {
            let consumed = read_rice_run(data, r.bit_position(), param, part)?;
            r.skip_bits(consumed)?;
        }
        pos = end;
    }
    Ok(())
}

/// Decodes `out.len()` Rice coded values with parameter `k` (at most 30)
/// from bit `start` of `data` and returns the number of bits consumed.
///
/// Same result as `BitReader::read_rice_signed` per value. Residuals are
/// most of a frame, and that reader takes unary codes a bit at a time;
/// here a 64-bit window and a leading zero count take a whole quotient at
/// once.
fn read_rice_run(data: &[u8], start: usize, k: u32, out: &mut [i64]) -> Result<usize> {
    let total = data.len() * 8;
    let eof = || Error::invalid_data("flac residual runs past the end of the frame");
    let mut pos = start;
    for s in out.iter_mut() {
        let mut q = 0u64;
        loop {
            if pos >= total {
                return Err(eof());
            }
            // At least 57 bits of the window are real data or zero padding
            // past the end; a one bit found in it is always real data.
            let z = window(data, pos).leading_zeros();
            if z < 57 {
                q += z as u64;
                pos += z as usize + 1;
                break;
            }
            q += 57;
            pos += 57;
        }
        if pos + k as usize > total {
            return Err(eof());
        }
        let low = if k == 0 {
            0
        } else {
            window(data, pos) >> (64 - k)
        };
        pos += k as usize;
        // `q` is bounded by the frame size in bits, so the shift cannot
        // overflow; values beyond 32 bits are invalid residuals.
        let u = q << k | low;
        if u > u32::MAX as u64 {
            return Err(Error::invalid_data("flac residual value out of range"));
        }
        *s = (u >> 1) as i64 ^ -((u & 1) as i64);
    }
    Ok(pos - start)
}

/// The 64 bits of `data` starting at bit `bit`, zero padded past the end.
/// Only the top 57 are guaranteed to come from the stream (or padding).
fn window(data: &[u8], bit: usize) -> u64 {
    let byte = bit / 8;
    let word = match data.get(byte..byte + 8) {
        Some(b) => u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
        None => {
            let mut buf = [0u8; 8];
            let tail = data.get(byte..).unwrap_or(&[]);
            buf[..tail.len()].copy_from_slice(tail);
            u64::from_be_bytes(buf)
        }
    };
    word << (bit % 8)
}

fn out_of_range() -> Error {
    Error::invalid_data("flac predicted sample exceeds the subframe bit depth")
}

/// Adds the fixed polynomial prediction of RFC 9639 section 9.2.5 to the
/// residuals in `out[order..]`.
///
/// Each result is checked against the subframe depth before it feeds the
/// next prediction, which keeps every intermediate value small enough that
/// the `i64` arithmetic cannot overflow on hostile input.
fn restore_fixed(order: usize, bps: u32, out: &mut [i64]) -> Result<()> {
    // One monomorphized loop per order keeps the predictor inlined.
    match order {
        0 => predict_with(0, bps, out, |_| 0),
        1 => predict_with(1, bps, out, |w| w[0]),
        2 => predict_with(2, bps, out, |w| 2 * w[1] - w[0]),
        3 => predict_with(3, bps, out, |w| 3 * w[2] - 3 * w[1] + w[0]),
        _ => predict_with(4, bps, out, |w| 4 * w[3] - 6 * w[2] + 4 * w[1] - w[0]),
    }
}

/// Adds `predict(previous order samples, oldest first)` to each residual
/// from index `order` on. Warmup samples were read at the subframe depth,
/// so only the predicted ones need a range check.
fn predict_with(
    order: usize,
    bps: u32,
    out: &mut [i64],
    predict: impl Fn(&[i64]) -> i64,
) -> Result<()> {
    let (lo, hi) = sample_range(bps);
    for i in order..out.len() {
        let v = out[i] + predict(&out[i - order..i]);
        if v < lo || v > hi {
            return Err(out_of_range());
        }
        out[i] = v;
    }
    Ok(())
}

/// Adds the linear prediction of RFC 9639 section 9.2.6: the first
/// coefficient applies to the most recent sample.
fn restore_lpc(coefs: &[i64], shift: u32, bps: u32, out: &mut [i64]) -> Result<()> {
    // Coefficients in history order (oldest first) so both slices are
    // walked forward, which the compiler turns into tighter code.
    let mut ordered = [0i64; MAX_LPC_ORDER];
    let ordered = &mut ordered[..coefs.len()];
    for (o, &c) in ordered.iter_mut().zip(coefs.iter().rev()) {
        *o = c;
    }
    // Bounds: samples have at most 33 bits, coefficients 15, and there are
    // at most 32 terms, so the sum stays below 2^53.
    predict_with(coefs.len(), bps, out, |history| {
        let sum: i64 = ordered.iter().zip(history).map(|(&c, &s)| c * s).sum();
        sum >> shift
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rational::Rational;
    use crate::stream::{AudioParams, StreamParams};
    use crate::util::bits::BitWriter;

    /// A small deterministic generator so tests need no external crates.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 11
        }

        /// A value uniformly spread over the signed `bits`-bit range.
        fn sample(&mut self, bits: u32) -> i64 {
            let (lo, hi) = sample_range(bits);
            lo + (self.next() % (hi - lo + 1) as u64) as i64
        }
    }

    fn write_signed(w: &mut BitWriter, v: i64, bits: u32) {
        let masked = (v as u64) & (u64::MAX >> (64 - bits));
        w.write_bits_u64(masked, bits);
    }

    /// Codes a frame or sample number the way the header expects.
    fn write_coded_number(w: &mut BitWriter, v: u64) {
        if v < 0x80 {
            w.write_bits(v as u32, 8);
            return;
        }
        // Payload bits available for 2 to 7 bytes: 11, 16, 21, 26, 31, 36.
        let bytes = (2..=7u32).find(|&n| v < 1u64 << (5 * n + 1)).unwrap();
        let first_bits = if bytes == 7 { 0 } else { 7 - bytes };
        let lead = if bytes == 7 {
            0xFE
        } else {
            (0xFF00u32 >> bytes) & 0xFF
        };
        let first = lead | (v >> (6 * (bytes - 1))) as u32 & ((1 << first_bits) - 1);
        w.write_bits(first, 8);
        for i in (0..bytes - 1).rev() {
            w.write_bits(0x80 | ((v >> (6 * i)) & 0x3F) as u32, 8);
        }
    }

    struct HeaderSpec {
        variable: bool,
        number: u64,
        block_size: u32,
        sample_rate: u32,
        assignment: u32,
        bits: u32,
    }

    /// Writes a frame header, choosing explicit codes where they exist.
    fn write_header(w: &mut BitWriter, h: &HeaderSpec) {
        w.write_bits(0x7FFC, 15);
        w.write_bit(h.variable);
        let (block_code, block_extra) = match h.block_size {
            192 => (1, None),
            576 => (2, None),
            4096 => (12, None),
            n if n <= 256 => (6, Some((n - 1, 8))),
            n => (7, Some((n - 1, 16))),
        };
        w.write_bits(block_code, 4);
        let (rate_code, rate_extra) = match h.sample_rate {
            44_100 => (9, None),
            8_000 => (4, None),
            r if r % 1000 == 0 && r / 1000 < 256 => (12, Some((r / 1000, 8))),
            r if r < 65536 => (13, Some((r, 16))),
            r => (14, Some((r / 10, 16))),
        };
        w.write_bits(rate_code, 4);
        w.write_bits(h.assignment, 4);
        let size_code = match h.bits {
            8 => 1,
            12 => 2,
            16 => 4,
            20 => 5,
            24 => 6,
            32 => 7,
            _ => 0,
        };
        w.write_bits(size_code, 3);
        w.write_bit(false);
        write_coded_number(w, h.number);
        if let Some((v, n)) = block_extra {
            w.write_bits(v, n);
        }
        if let Some((v, n)) = rate_extra {
            w.write_bits(v, n);
        }
        let crc = crc::crc8(w.bytes());
        w.write_bits(crc as u32, 8);
    }

    fn finish_frame(mut w: BitWriter) -> Vec<u8> {
        w.align();
        let mut bytes = w.into_bytes();
        let crc = crc::crc16(&bytes);
        bytes.extend_from_slice(&crc.to_be_bytes());
        bytes
    }

    /// Builds a frame whose subframes are all verbatim, from already
    /// decorrelated channel data.
    fn verbatim_frame(h: &HeaderSpec, channels: &[Vec<i64>]) -> Vec<u8> {
        let mut w = BitWriter::new();
        write_header(&mut w, h);
        let assignment = decode_assignment(h.assignment);
        for (ch, data) in channels.iter().enumerate() {
            let bps = h.bits + assignment.is_side(ch) as u32;
            w.write_bits(0b0000_0010, 8);
            for &s in data {
                write_signed(&mut w, s, bps);
            }
        }
        finish_frame(w)
    }

    fn decode_assignment(code: u32) -> ChannelAssignment {
        match code {
            0..=7 => ChannelAssignment::Independent(code as u8 + 1),
            8 => ChannelAssignment::LeftSide,
            9 => ChannelAssignment::SideRight,
            _ => ChannelAssignment::MidSide,
        }
    }

    fn info(channels: u8, bits: u8, rate: u32, block: u16) -> StreamInfoBlock {
        StreamInfoBlock {
            min_block_size: block,
            max_block_size: block,
            sample_rate: rate,
            channels,
            bits_per_sample: bits,
            ..StreamInfoBlock::default()
        }
    }

    fn stream_for(info: &StreamInfoBlock) -> StreamInfo {
        let params = AudioParams {
            sample_rate: info.sample_rate,
            channels: info.channels as u16,
            bits_per_sample: info.bits_per_sample as u32,
        };
        let mut s = StreamInfo::new(
            0,
            CodecId::Flac,
            StreamParams::Audio(params),
            Rational::new(1, info.sample_rate as i64),
        );
        s.extradata = info.serialize().to_vec();
        s
    }

    fn decode_one(info: &StreamInfoBlock, frame: &[u8]) -> Result<AudioFrame> {
        let mut d = new_decoder(&stream_for(info))?;
        let frames = d.decode(&Packet::new(0, frame.to_vec(), 0, 0))?;
        match frames.into_iter().next() {
            Some(Frame::Audio(a)) => Ok(a),
            _ => panic!("expected one audio frame"),
        }
    }

    fn int_samples(frame: &AudioFrame) -> &[i32] {
        match &frame.data {
            AudioData::Int { samples, .. } => samples,
            AudioData::Float(_) => panic!("expected integer samples"),
        }
    }

    #[test]
    fn coded_numbers_round_trip() {
        for v in [
            0u64,
            0x7F,
            0x80,
            0x7FF,
            0x800,
            0xFFFF,
            0x1_0000,
            0x1F_FFFF,
            0x3FF_FFFF,
            0x7FFF_FFFF,
            0x8000_0000,
            0xF_FFFF_FFFF,
        ] {
            let mut w = BitWriter::new();
            write_coded_number(&mut w, v);
            let bytes = w.into_bytes();
            let mut r = BitReader::new(&bytes);
            assert_eq!(read_coded_number(&mut r).unwrap(), v, "value {v:#x}");
            assert_eq!(r.bits_left(), 0);
        }
    }

    #[test]
    fn malformed_coded_numbers_are_rejected() {
        for bytes in [&[0x80u8][..], &[0xFF], &[0xC2, 0x41], &[0xE0, 0x80]] {
            let mut r = BitReader::new(bytes);
            assert!(read_coded_number(&mut r).is_err(), "{bytes:?}");
        }
    }

    #[test]
    fn verbatim_round_trip_every_assignment_and_depth() {
        let mut rng = Lcg(1);
        for bits in [8u32, 12, 16, 20, 24, 32] {
            for assignment in 0..=10u32 {
                let dec = decode_assignment(assignment);
                let channels = dec.channels();
                let n = 37;
                // Random interleaved output samples at the stream depth.
                let pcm: Vec<Vec<i64>> = (0..channels)
                    .map(|_| (0..n).map(|_| rng.sample(bits)).collect())
                    .collect();
                let coded: Vec<Vec<i64>> = match dec {
                    ChannelAssignment::Independent(_) => pcm.clone(),
                    ChannelAssignment::LeftSide => vec![
                        pcm[0].clone(),
                        pcm[0].iter().zip(&pcm[1]).map(|(l, r)| l - r).collect(),
                    ],
                    ChannelAssignment::SideRight => vec![
                        pcm[0].iter().zip(&pcm[1]).map(|(l, r)| l - r).collect(),
                        pcm[1].clone(),
                    ],
                    ChannelAssignment::MidSide => vec![
                        pcm[0]
                            .iter()
                            .zip(&pcm[1])
                            .map(|(l, r)| (l + r) >> 1)
                            .collect(),
                        pcm[0].iter().zip(&pcm[1]).map(|(l, r)| l - r).collect(),
                    ],
                };
                let h = HeaderSpec {
                    variable: false,
                    number: 3,
                    block_size: n as u32,
                    sample_rate: 44_100,
                    assignment,
                    bits,
                };
                let frame = verbatim_frame(&h, &coded);
                let si = info(channels as u8, bits as u8, 44_100, 4096);
                let out = decode_one(&si, &frame).unwrap();
                assert_eq!(out.pts, 3 * 4096);
                assert_eq!(out.channels as usize, channels);
                assert_eq!(out.data.kind(), crate::frame::SampleKind::Int(bits));
                let expected: Vec<i32> = (0..n)
                    .flat_map(|i| pcm.iter().map(move |c| c[i] as i32))
                    .collect();
                assert_eq!(
                    int_samples(&out),
                    &expected[..],
                    "bits {bits} assignment {assignment}"
                );
            }
        }
    }

    #[test]
    fn extreme_values_of_32_bit_side_channels() {
        // Left and right at opposite extremes make the side channel need
        // all 33 bits.
        let left = vec![i32::MIN as i64, i32::MAX as i64, 0, -1];
        let right = vec![i32::MAX as i64, i32::MIN as i64, -1, i32::MIN as i64];
        let side: Vec<i64> = left.iter().zip(&right).map(|(l, r)| l - r).collect();
        let mid: Vec<i64> = left.iter().zip(&right).map(|(l, r)| (l + r) >> 1).collect();
        let si = info(2, 32, 44_100, 4096);
        for (assignment, coded) in [
            (8, vec![left.clone(), side.clone()]),
            (9, vec![side.clone(), right.clone()]),
            (10, vec![mid, side]),
        ] {
            let h = HeaderSpec {
                variable: false,
                number: 0,
                block_size: 4,
                sample_rate: 44_100,
                assignment,
                bits: 32,
            };
            let out = decode_one(&si, &verbatim_frame(&h, &coded)).unwrap();
            let expected: Vec<i32> = (0..4)
                .flat_map(|i| [left[i] as i32, right[i] as i32])
                .collect();
            assert_eq!(int_samples(&out), &expected[..]);
        }
    }

    /// Starts a one channel frame and returns the writer positioned at the
    /// first subframe.
    fn mono_writer(bits: u32, block_size: u32) -> BitWriter {
        let mut w = BitWriter::new();
        write_header(
            &mut w,
            &HeaderSpec {
                variable: false,
                number: 0,
                block_size,
                sample_rate: 8_000,
                assignment: 0,
                bits,
            },
        );
        w
    }

    fn decode_mono(bits: u32, frame: &[u8]) -> Result<Vec<i32>> {
        let si = info(1, bits as u8, 8_000, 4096);
        decode_one(&si, frame).map(|f| int_samples(&f).to_vec())
    }

    #[test]
    fn constant_subframe_with_wasted_bits() {
        let mut w = mono_writer(16, 192);
        // Constant type, wasted flag, 3 wasted bits (unary 2 then a one).
        w.write_bits(0b0000_0001, 8);
        w.write_bits(0b001, 3);
        write_signed(&mut w, -1000, 13);
        let out = decode_mono(16, &finish_frame(w)).unwrap();
        assert_eq!(out, vec![-8000; 192]);
    }

    #[derive(Clone, Copy)]
    enum Part {
        Rice(u32),
        Escape(u32),
    }

    /// Writes a Rice residual with one partition per entry of `params`
    /// (a Rice parameter or an escape with a sample width).
    fn write_residual(
        w: &mut BitWriter,
        method: u32,
        partition_order: u32,
        params: &[Part],
        residual: &[i64],
        order: usize,
    ) {
        w.write_bits(method, 2);
        w.write_bits(partition_order, 4);
        let per = (residual.len() + order) >> partition_order;
        let mut pos = 0;
        for (p, param) in params.iter().enumerate() {
            let count = if p == 0 { per - order } else { per };
            let part = &residual[pos..pos + count];
            match *param {
                Part::Rice(k) => {
                    w.write_bits(k, 4 + method);
                    for &s in part {
                        w.write_rice_signed(s as i32, k);
                    }
                }
                Part::Escape(width) => {
                    w.write_bits(if method == 0 { 15 } else { 31 }, 4 + method);
                    w.write_bits(width, 5);
                    for &s in part {
                        if width > 0 {
                            write_signed(w, s, width);
                        }
                    }
                }
            }
            pos += count;
        }
    }

    /// Fixed and LPC predictions computed independently of the decoder.
    fn fixed_predict(order: usize, s: &[i64], i: usize) -> i64 {
        match order {
            0 => 0,
            1 => s[i - 1],
            2 => 2 * s[i - 1] - s[i - 2],
            3 => 3 * s[i - 1] - 3 * s[i - 2] + s[i - 3],
            _ => 4 * s[i - 1] - 6 * s[i - 2] + 4 * s[i - 3] - s[i - 4],
        }
    }

    fn smooth_signal(n: usize, amplitude: f64) -> Vec<i64> {
        (0..n)
            .map(|i| ((i as f64 * 0.05).sin() * amplitude).round() as i64)
            .collect()
    }

    #[test]
    fn fixed_subframes_every_order() {
        let n = 64;
        let signal = smooth_signal(n, 20000.0);
        for order in 0..=4usize {
            let residual: Vec<i64> = (order..n)
                .map(|i| signal[i] - fixed_predict(order, &signal, i))
                .collect();
            let mut w = mono_writer(16, n as u32);
            w.write_bits((8 + order as u32) << 1, 8);
            for &s in &signal[..order] {
                write_signed(&mut w, s, 16);
            }
            // Two partitions with 4-bit parameters, the second escaped.
            write_residual(
                &mut w,
                0,
                1,
                &[Part::Rice(10), Part::Escape(17)],
                &residual,
                order,
            );
            let out = decode_mono(16, &finish_frame(w)).unwrap();
            let expected: Vec<i32> = signal.iter().map(|&s| s as i32).collect();
            assert_eq!(out, expected, "order {order}");
        }
    }

    #[test]
    fn lpc_subframes_with_5_bit_parameters() {
        let n = 128;
        let signal = smooth_signal(n, 3_000_000.0);
        // A second order predictor in 14-bit precision with shift 12:
        // 2 cos(0.05) and -1.
        let coefs = [8182i64, -4096];
        let shift = 12;
        for order in [1usize, 2, 32] {
            let mut c = vec![0i64; order];
            c[..order.min(2)].copy_from_slice(&coefs[..order.min(2)]);
            let residual: Vec<i64> = (order..n)
                .map(|i| {
                    let sum: i64 = (0..order).map(|j| c[j] * signal[i - 1 - j]).sum();
                    signal[i] - (sum >> shift)
                })
                .collect();
            let mut w = mono_writer(24, n as u32);
            w.write_bits((32 + order as u32 - 1) << 1, 8);
            for &s in &signal[..order] {
                write_signed(&mut w, s, 24);
            }
            w.write_bits(14 - 1, 4);
            write_signed(&mut w, shift, 5);
            for &k in &c {
                write_signed(&mut w, k, 14);
            }
            // Partition order 2: four partitions, one escaped with width 0
            // only when its residual is all zeros, which it is not here.
            write_residual(
                &mut w,
                1,
                2,
                &[
                    Part::Rice(20),
                    Part::Rice(17),
                    Part::Escape(24),
                    Part::Rice(30),
                ],
                &residual,
                order,
            );
            let out = decode_mono(24, &finish_frame(w)).unwrap();
            let expected: Vec<i32> = signal.iter().map(|&s| s as i32).collect();
            assert_eq!(out, expected, "order {order}");
        }
    }

    #[test]
    fn escaped_partition_of_width_zero() {
        let n = 16;
        let mut w = mono_writer(8, n);
        // Fixed order 1, warmup 5, residual all zeros through an escape of
        // width 0: the whole block holds 5.
        w.write_bits(9 << 1, 8);
        write_signed(&mut w, 5, 8);
        write_residual(&mut w, 0, 0, &[Part::Escape(0)], &[0; 15], 1);
        assert_eq!(decode_mono(8, &finish_frame(w)).unwrap(), vec![5; 16]);
    }

    #[test]
    fn variable_blocking_uses_sample_numbers() {
        let si = info(1, 16, 44_100, 4096);
        let h = HeaderSpec {
            variable: true,
            number: 0x9_0000_0001,
            block_size: 300,
            sample_rate: 44_100,
            assignment: 0,
            bits: 16,
        };
        let frame = verbatim_frame(&h, &[vec![7; 300]]);
        let out = decode_one(&si, &frame).unwrap();
        assert_eq!(out.pts, 0x9_0000_0001);
        assert_eq!(out.nb_samples(), 300);
    }

    #[test]
    fn header_rate_and_size_codes() {
        for (rate, block) in [
            (96_000u32, 1u32),
            (22_050, 65536),
            (8_000, 192),
            (11_025, 576),
        ] {
            let mut w = BitWriter::new();
            write_header(
                &mut w,
                &HeaderSpec {
                    variable: false,
                    number: 1234,
                    block_size: block,
                    sample_rate: rate,
                    assignment: 1,
                    bits: 24,
                },
            );
            let bytes = w.into_bytes();
            let h = parse_frame_header(&bytes).unwrap();
            assert_eq!(h.sample_rate, Some(rate));
            assert_eq!(h.block_size, block);
            assert_eq!(h.number, 1234);
            assert_eq!(h.bits_per_sample, Some(24));
            assert_eq!(h.len, bytes.len());
        }
        // Sample size and rate deferred to STREAMINFO.
        let mut w = BitWriter::new();
        write_header(
            &mut w,
            &HeaderSpec {
                variable: false,
                number: 0,
                block_size: 4096,
                sample_rate: 44_100,
                assignment: 0,
                bits: 7,
            },
        );
        let mut bytes = w.into_bytes();
        bytes[2] &= 0xF0;
        let len = bytes.len();
        bytes[len - 1] = crc::crc8(&bytes[..len - 1]);
        let h = parse_frame_header(&bytes).unwrap();
        assert_eq!((h.sample_rate, h.bits_per_sample), (None, None));
    }

    #[test]
    fn stream_info_defaults_decode_odd_depths() {
        // A 7-bit stream can only declare its depth in STREAMINFO.
        let si = info(1, 7, 44_100, 4096);
        let h = HeaderSpec {
            variable: false,
            number: 0,
            block_size: 4,
            sample_rate: 44_100,
            assignment: 0,
            bits: 7,
        };
        let out = decode_one(&si, &verbatim_frame(&h, &[vec![-64, 63, 0, 1]])).unwrap();
        assert_eq!(int_samples(&out), &[-64, 63, 0, 1]);
        assert_eq!(out.data.kind(), crate::frame::SampleKind::Int(7));
    }

    fn simple_frame() -> Vec<u8> {
        let h = HeaderSpec {
            variable: false,
            number: 0,
            block_size: 8,
            sample_rate: 8_000,
            assignment: 0,
            bits: 16,
        };
        verbatim_frame(&h, &[vec![1, -2, 3, -4, 5, -6, 7, -8]])
    }

    #[test]
    fn corrupt_frames_are_errors() {
        let si = info(1, 16, 8_000, 4096);
        let good = simple_frame();
        assert!(decode_one(&si, &good).is_ok());

        let mut bad_header = good.clone();
        bad_header[4] ^= 0x01;
        assert!(decode_one(&si, &bad_header).is_err());

        let mut bad_body = good.clone();
        bad_body[8] ^= 0x10;
        assert!(decode_one(&si, &bad_body).is_err());

        for cut in 0..good.len() {
            assert!(decode_one(&si, &good[..cut]).is_err(), "cut at {cut}");
        }

        // Stream parameters that disagree with the frame.
        assert!(decode_one(&info(2, 16, 8_000, 4096), &good).is_err());
        assert!(decode_one(&info(1, 24, 8_000, 4096), &good).is_err());
        assert!(decode_one(&info(1, 16, 44_100, 4096), &good).is_err());
    }

    /// Rebuilds a mono frame from a header and a raw subframe bit writer.
    fn mono_frame_with(bits: u32, n: u32, body: impl FnOnce(&mut BitWriter)) -> Vec<u8> {
        let mut w = mono_writer(bits, n);
        body(&mut w);
        finish_frame(w)
    }

    #[test]
    fn invalid_subframes_are_errors() {
        let cases: Vec<(&str, Vec<u8>)> = vec![
            (
                "reserved type",
                mono_frame_with(16, 8, |w| w.write_bits(0b0000_0100, 8)),
            ),
            (
                "fixed order 5",
                mono_frame_with(16, 8, |w| w.write_bits(13 << 1, 8)),
            ),
            (
                "padding bit",
                mono_frame_with(16, 8, |w| w.write_bits(0x80, 8)),
            ),
            (
                "wasted all bits",
                mono_frame_with(16, 8, |w| {
                    w.write_bits(0b0000_0001, 8);
                    w.write_unary(15);
                }),
            ),
            (
                "precision code 15",
                mono_frame_with(16, 8, |w| {
                    w.write_bits(32 << 1, 8);
                    w.write_bits(0, 16);
                    w.write_bits(15, 4);
                }),
            ),
            (
                "negative shift",
                mono_frame_with(16, 8, |w| {
                    w.write_bits(32 << 1, 8);
                    w.write_bits(0, 16);
                    w.write_bits(3, 4);
                    w.write_bits(0b11111, 5);
                    w.write_bits(1, 4);
                    write_residual(w, 0, 0, &[Part::Rice(0)], &[0; 7], 1);
                }),
            ),
            (
                "order above block size",
                mono_frame_with(16, 2, |w| {
                    w.write_bits(11 << 1, 8);
                    w.write_bits(0, 32);
                    w.write_bits(0, 16);
                }),
            ),
            (
                "partition order too large",
                mono_frame_with(16, 8, |w| {
                    w.write_bits(10 << 1, 8);
                    w.write_bits(0, 32);
                    w.write_bits(0, 2);
                    w.write_bits(3, 4);
                }),
            ),
            (
                "reserved residual method",
                mono_frame_with(16, 8, |w| {
                    w.write_bits(8 << 1, 8);
                    w.write_bits(2, 2);
                }),
            ),
            (
                "trailing data",
                mono_frame_with(16, 1, |w| {
                    w.write_bits(0, 8);
                    w.write_bits(0, 16);
                    w.write_bits(0xFF, 8);
                }),
            ),
            (
                "prediction overflow",
                mono_frame_with(8, 8, |w| {
                    // Fixed order 1 from 127 with residuals of +100 leaves the
                    // 8-bit range at once.
                    w.write_bits(9 << 1, 8);
                    write_signed(w, 127, 8);
                    write_residual(w, 0, 0, &[Part::Rice(7)], &[100; 7], 1);
                }),
            ),
        ];
        for (name, frame) in cases {
            let bits = if name == "prediction overflow" { 8 } else { 16 };
            assert!(decode_mono(bits, &frame).is_err(), "{name}");
        }
    }

    #[test]
    fn header_errors() {
        let good = simple_frame();
        let h = parse_frame_header(&good).unwrap();
        // Reserved channel assignment, block size code 0, reserved sample
        // size and reserved bit, each with a recomputed CRC-8.
        for (byte, mask, value) in [
            (3usize, 0xF0u8, 0xB0u8),
            (2, 0xF0, 0x00),
            (3, 0x0E, 0x06),
            (3, 0x01, 0x01),
            (2, 0x0F, 0x0F),
        ] {
            let mut hdr = good[..h.len].to_vec();
            hdr[byte] = (hdr[byte] & !mask) | value;
            let last = hdr.len() - 1;
            hdr[last] = crc::crc8(&hdr[..last]);
            assert!(
                parse_frame_header(&hdr).is_err(),
                "byte {byte} value {value:#x}"
            );
        }
        // A 36-bit number in a fixed blocking frame.
        let mut w = BitWriter::new();
        write_header(
            &mut w,
            &HeaderSpec {
                variable: false,
                number: 1 << 31,
                block_size: 192,
                sample_rate: 44_100,
                assignment: 0,
                bits: 16,
            },
        );
        assert!(parse_frame_header(w.bytes()).is_err());
        assert!(parse_frame_header(&[0xFF, 0xF8]).is_err());
        assert!(parse_frame_header(&[0xFF, 0xFA, 0x00, 0x00]).is_err());
    }

    #[test]
    fn md5_is_checked_when_the_stream_is_complete() {
        let frame = simple_frame();
        let mut md5 = Md5::new();
        for s in [1i16, -2, 3, -4, 5, -6, 7, -8] {
            md5.update(&s.to_le_bytes());
        }
        let good = md5.finalize();

        let mut si = info(1, 16, 8_000, 4096);
        si.total_samples = 8;
        si.md5 = good;
        let mut d = new_decoder(&stream_for(&si)).unwrap();
        d.decode(&Packet::new(0, frame.clone(), 0, 8)).unwrap();
        assert!(d.flush().is_ok());
        assert!(d.flush().is_ok());

        si.md5[0] ^= 1;
        let mut d = new_decoder(&stream_for(&si)).unwrap();
        d.decode(&Packet::new(0, frame.clone(), 0, 8)).unwrap();
        assert!(d.flush().is_err());

        // A partial decode cannot be checked, so it is not an error.
        si.total_samples = 16;
        let mut d = new_decoder(&stream_for(&si)).unwrap();
        d.decode(&Packet::new(0, frame, 0, 8)).unwrap();
        assert!(d.flush().is_ok());
    }

    #[test]
    fn decoder_without_extradata_uses_stream_params() {
        let si = info(1, 16, 8_000, 0);
        let mut stream = stream_for(&si);
        stream.extradata.clear();
        let mut d = new_decoder(&stream).unwrap();
        let frames = d.decode(&Packet::new(0, simple_frame(), 77, 8)).unwrap();
        let Frame::Audio(a) = &frames[0] else {
            panic!("expected audio");
        };
        // No block size is known, so the packet timestamp is kept.
        assert_eq!(a.pts, 77);

        stream.params = StreamParams::Audio(AudioParams {
            sample_rate: 8_000,
            channels: 9,
            bits_per_sample: 16,
        });
        assert!(new_decoder(&stream).is_err());
    }
}
