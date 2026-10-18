//! FLAC encoder.
//!
//! Written from RFC 9639 (Free Lossless Audio Codec): the frame layout of
//! section 9, the STREAMINFO block of section 8.2, and the coding tools of
//! section 4 (interchannel decorrelation, prediction, residual coding).
//!
//! Every frame uses a fixed block size (1152 samples at levels 0 to 2,
//! 4096 above), except the last one which may be shorter. Each channel is
//! coded with the cheapest of the CONSTANT, VERBATIM, FIXED and LPC
//! subframes, after removing wasted low bits. Stereo frames also try the
//! three decorrelated channel assignments.
//!
//! LPC coefficients come from the autocorrelation of a windowed block and
//! the Levinson-Durbin recursion, as described in J. Makhoul, "Linear
//! prediction: a tutorial review", Proc. IEEE 63(4), 1975. The windows are
//! the tapered cosine (Tukey) and Welch windows from F. J. Harris, "On the
//! use of windows for harmonic analysis with the discrete Fourier
//! transform", Proc. IEEE 66(1), 1978.

use crate::codec::flac::StreamInfoBlock;
use crate::codec::{Encoder, EncoderOptions};
use crate::error::{Error, Result};
use crate::frame::{AudioData, AudioFormat, Frame, FrameFormat, SampleKind};
use crate::rational::Rational;
use crate::stream::{AudioParams, CodecId, Packet, StreamInfo, StreamParams};
use crate::util::bits::BitWriter;
use crate::util::crc;
use crate::util::md5::Md5;

/// Default compression level, a good balance of speed and size.
const DEFAULT_LEVEL: u32 = 5;

/// Highest sample rate the 20-bit STREAMINFO field can hold.
const MAX_SAMPLE_RATE: u32 = (1 << 20) - 1;

/// Largest total sample count the 36-bit STREAMINFO field can hold; longer
/// streams store 0, which means unknown.
const MAX_TOTAL_SAMPLES: u64 = (1 << 36) - 1;

/// The coded frame number of a fixed block size stream is at most 31 bits.
const MAX_FRAME_NUMBER: u64 = (1 << 31) - 1;

/// Largest Rice parameter of each coding method. The all ones value is the
/// escape code, which this encoder never emits: some decoders handle
/// escaped partitions poorly, and a 5-bit parameter up to 30 already codes
/// any residual that fits in 32 bits.
const MAX_RICE_4BIT: u32 = 14;
const MAX_RICE_5BIT: u32 = 30;

/// Residuals must fit in a signed 32-bit integer (RFC 9639, section 9.2.7);
/// the most negative value is excluded so its magnitude also fits.
const RESIDUAL_LIMIT: i64 = i32::MAX as i64;

/// Blocks this short gain nothing from LPC: the coefficients would cost
/// more than the prediction saves, and the window degenerates.
const MIN_LPC_BLOCK: usize = 32;

/// Analysis windows applied before the autocorrelation.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Window {
    /// Tapered cosine: flat in the middle, cosine tapers covering the given
    /// fraction of the block (1.0 is the Hann window).
    Tukey(f64),
    /// Parabolic window.
    Welch,
}

impl Window {
    fn coefficients(self, n: usize) -> Vec<f64> {
        if n < 2 {
            return vec![1.0; n];
        }
        let last = (n - 1) as f64;
        match self {
            Window::Tukey(alpha) => {
                let alpha = alpha.clamp(0.0, 1.0);
                let taper = alpha * last / 2.0;
                (0..n)
                    .map(|i| {
                        let x = i as f64;
                        // Distance from the nearest edge, so one formula
                        // covers both tapers.
                        let d = x.min(last - x);
                        if taper <= 0.0 || d >= taper {
                            1.0
                        } else {
                            0.5 * (1.0 - (std::f64::consts::PI * d / taper).cos())
                        }
                    })
                    .collect()
            }
            Window::Welch => {
                let half = last / 2.0;
                (0..n)
                    .map(|i| {
                        let t = (i as f64 - half) / half;
                        1.0 - t * t
                    })
                    .collect()
            }
        }
    }
}

/// How hard a compression level searches.
#[derive(Clone, Debug)]
struct Settings {
    block_size: usize,
    /// 0 disables LPC (FIXED predictors only).
    max_lpc_order: usize,
    /// Try every LPC order instead of the one the recursion's error
    /// estimate prefers.
    exhaustive_order: bool,
    windows: &'static [Window],
    /// Try neighbouring coefficient precisions for the best predictor.
    precision_search: bool,
    max_partition_order: u32,
    /// Fully code all four stereo channel candidates instead of guessing
    /// from a cheap estimate.
    exhaustive_stereo: bool,
}

const TUKEY: &[Window] = &[Window::Tukey(0.5)];
const TUKEY_WELCH: &[Window] = &[Window::Tukey(0.5), Window::Welch];
const MANY_WINDOWS: &[Window] = &[
    Window::Tukey(0.5),
    Window::Tukey(0.25),
    Window::Tukey(1.0),
    Window::Welch,
];

impl Settings {
    fn for_level(level: u32) -> Settings {
        let fixed = |max_partition_order, exhaustive_stereo| Settings {
            block_size: 1152,
            max_lpc_order: 0,
            exhaustive_order: false,
            windows: TUKEY,
            precision_search: false,
            max_partition_order,
            exhaustive_stereo,
        };
        let lpc = |max_lpc_order, exhaustive_order, windows, max_partition_order| Settings {
            block_size: 4096,
            max_lpc_order,
            exhaustive_order,
            windows,
            precision_search: false,
            max_partition_order,
            exhaustive_stereo: true,
        };
        match level {
            0 => fixed(3, false),
            1 => fixed(4, true),
            2 => fixed(5, true),
            3 => Settings {
                exhaustive_stereo: false,
                ..lpc(6, false, TUKEY, 4)
            },
            4 => lpc(8, false, TUKEY, 5),
            5 => lpc(8, true, TUKEY, 6),
            6 => lpc(8, true, TUKEY_WELCH, 6),
            7 => lpc(12, true, TUKEY_WELCH, 6),
            _ => Settings {
                precision_search: true,
                ..lpc(12, true, MANY_WINDOWS, 8)
            },
        }
    }
}

pub fn new_encoder(
    codec: CodecId,
    proposed: &FrameFormat,
    options: &EncoderOptions,
) -> Result<Box<dyn Encoder>> {
    if codec != CodecId::Flac {
        return Err(Error::invalid_argument(format!("{codec} is not FLAC")));
    }
    let FrameFormat::Audio(proposed) = proposed else {
        return Err(Error::invalid_argument("flac encodes audio only"));
    };
    Ok(Box::new(FlacEncoder::new(proposed, options)?))
}

struct FlacEncoder {
    stream_index: usize,
    format: AudioFormat,
    bits: u32,
    settings: Settings,
    /// Window coefficients for the nominal block size, one set per window.
    windows: Vec<Vec<f64>>,
    /// Samples not coded yet, one buffer per channel.
    pending: Vec<Vec<i32>>,
    /// Timestamp of the first input sample, the base of packet timestamps.
    first_pts: Option<i64>,
    samples_in: u64,
    samples_coded: u64,
    frame_number: u64,
    min_frame_size: u32,
    max_frame_size: u32,
    bytes_out: u64,
    md5: Option<Md5>,
    digest: Option<[u8; 16]>,
    md5_scratch: Vec<u8>,
}

impl FlacEncoder {
    fn new(proposed: &AudioFormat, options: &EncoderOptions) -> Result<Self> {
        if proposed.channels == 0 || proposed.channels > 8 {
            return Err(Error::unsupported(format!(
                "flac codes 1 to 8 channels, not {}",
                proposed.channels
            )));
        }
        if proposed.sample_rate == 0 || proposed.sample_rate > MAX_SAMPLE_RATE {
            return Err(Error::unsupported(format!(
                "flac cannot code a sample rate of {} Hz (1 to {MAX_SAMPLE_RATE})",
                proposed.sample_rate
            )));
        }
        // Float input has no exact integer equivalent; 24 bits keeps more
        // precision than a 32-bit float mantissa carries in its usual range.
        let bits = match proposed.kind {
            SampleKind::Float => 24,
            SampleKind::Int(b) if (4..=24).contains(&b) || b == 32 => b,
            SampleKind::Int(b) => {
                return Err(Error::unsupported(format!(
                    "flac encodes 4 to 24 or 32 bits per sample, not {b}"
                )));
            }
        };
        let level = options.compression_level.unwrap_or(DEFAULT_LEVEL);
        if level > 8 {
            return Err(Error::invalid_argument(format!(
                "flac compression level must be 0 to 8, not {level}"
            )));
        }
        let settings = Settings::for_level(level);
        let windows = settings
            .windows
            .iter()
            .map(|w| w.coefficients(settings.block_size))
            .collect();
        Ok(FlacEncoder {
            stream_index: options.stream_index,
            format: AudioFormat {
                sample_rate: proposed.sample_rate,
                channels: proposed.channels,
                kind: SampleKind::Int(bits),
            },
            bits,
            pending: vec![Vec::new(); proposed.channels as usize],
            settings,
            windows,
            first_pts: None,
            samples_in: 0,
            samples_coded: 0,
            frame_number: 0,
            min_frame_size: 0,
            max_frame_size: 0,
            bytes_out: 0,
            md5: Some(Md5::new()),
            digest: None,
            md5_scratch: Vec::new(),
        })
    }

    fn streaminfo(&self) -> StreamInfoBlock {
        let block = self.settings.block_size as u16;
        let mut info = StreamInfoBlock {
            min_block_size: block,
            max_block_size: block,
            sample_rate: self.format.sample_rate,
            channels: self.format.channels as u8,
            bits_per_sample: self.bits as u8,
            ..StreamInfoBlock::default()
        };
        // Before the end of the stream the sizes, the length and the
        // checksum are unknown, which STREAMINFO expresses with zeros.
        if let Some(digest) = self.digest {
            info.min_frame_size = self.min_frame_size;
            info.max_frame_size = self.max_frame_size;
            if self.samples_in <= MAX_TOTAL_SAMPLES {
                info.total_samples = self.samples_in;
            }
            info.md5 = digest;
        }
        info
    }

    /// Feeds the MD5 with the input exactly as a decoder will output it:
    /// interleaved, little-endian, in the fewest whole bytes per sample.
    fn hash(&mut self, samples: &[i32]) {
        let bytes = self.bits.div_ceil(8) as usize;
        self.md5_scratch.clear();
        for &s in samples {
            self.md5_scratch
                .extend_from_slice(&s.to_le_bytes()[..bytes]);
        }
        if let Some(md5) = self.md5.as_mut() {
            md5.update(&self.md5_scratch);
        }
    }

    fn emit(&mut self, len: usize) -> Result<Packet> {
        if self.frame_number > MAX_FRAME_NUMBER {
            return Err(Error::unsupported(
                "flac stream too long for fixed block size frame numbers",
            ));
        }
        let block: Vec<Vec<i64>> = self
            .pending
            .iter_mut()
            .map(|ch| ch.drain(..len).map(i64::from).collect())
            .collect();
        let windows = if len == self.settings.block_size {
            None
        } else {
            Some(
                self.settings
                    .windows
                    .iter()
                    .map(|w| w.coefficients(len))
                    .collect::<Vec<_>>(),
            )
        };
        let ctx = FrameContext {
            sample_rate: self.format.sample_rate,
            bits: self.bits,
            settings: &self.settings,
            windows: windows.as_deref().unwrap_or(&self.windows),
        };
        let data = encode_frame(&ctx, &block, self.frame_number);
        let size = data.len() as u32;
        self.min_frame_size = if self.frame_number == 0 {
            size
        } else {
            self.min_frame_size.min(size)
        };
        self.max_frame_size = self.max_frame_size.max(size);
        self.bytes_out += data.len() as u64;
        let pts = self.first_pts.unwrap_or(0) + self.samples_coded as i64;
        self.frame_number += 1;
        self.samples_coded += len as u64;
        Ok(Packet::new(self.stream_index, data, pts, len as i64))
    }
}

impl Encoder for FlacEncoder {
    fn input_format(&self) -> FrameFormat {
        FrameFormat::Audio(self.format)
    }

    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        let Frame::Audio(frame) = frame else {
            return Err(Error::invalid_argument("flac encoder got a video frame"));
        };
        if self.digest.is_some() {
            return Err(Error::invalid_argument(
                "flac encoder got a frame after flush",
            ));
        }
        let AudioData::Int { samples, bits } = &frame.data else {
            return Err(Error::invalid_argument(
                "flac encoder expects integer samples",
            ));
        };
        let channels = self.format.channels as usize;
        if *bits != self.bits
            || frame.channels as usize != channels
            || frame.sample_rate != self.format.sample_rate
        {
            return Err(Error::invalid_argument(format!(
                "flac encoder set up for {} channels of {} bits at {} Hz, got {} of {bits} at {} Hz",
                channels, self.bits, self.format.sample_rate, frame.channels, frame.sample_rate
            )));
        }
        if samples.len() % channels != 0 {
            return Err(Error::invalid_data("audio frame ends on a partial sample"));
        }
        // A sample outside the declared depth would not survive coding, so
        // refuse it rather than write a stream that decodes differently.
        let max = (1i64 << (self.bits - 1)) - 1;
        if samples
            .iter()
            .any(|&s| (s as i64) > max || (s as i64) < -max - 1)
        {
            return Err(Error::invalid_data(format!(
                "sample out of range for {} bits",
                self.bits
            )));
        }
        self.first_pts.get_or_insert(frame.pts);
        self.hash(samples);
        self.samples_in += (samples.len() / channels) as u64;
        for (i, &s) in samples.iter().enumerate() {
            self.pending[i % channels].push(s);
        }
        let mut packets = Vec::new();
        while self.pending[0].len() >= self.settings.block_size {
            packets.push(self.emit(self.settings.block_size)?);
        }
        Ok(packets)
    }

    fn flush(&mut self) -> Result<Vec<Packet>> {
        if self.digest.is_some() {
            return Ok(Vec::new());
        }
        let mut packets = Vec::new();
        let left = self.pending[0].len();
        if left > 0 {
            packets.push(self.emit(left)?);
        }
        self.digest = self.md5.take().map(Md5::finalize);
        Ok(packets)
    }

    fn stream_info(&self) -> StreamInfo {
        let rate = self.format.sample_rate;
        let mut info = StreamInfo::new(
            self.stream_index,
            CodecId::Flac,
            StreamParams::Audio(AudioParams {
                sample_rate: rate,
                channels: self.format.channels,
                bits_per_sample: self.bits,
            }),
            Rational::new(1, rate as i64),
        );
        info.extradata = self.streaminfo().serialize().to_vec();
        if self.digest.is_some() {
            info.duration = Some(self.samples_in as i64);
            if self.samples_in > 0 {
                info.bit_rate = Some(
                    (self.bytes_out as u128 * 8 * rate as u128 / self.samples_in as u128) as u64,
                );
            }
        }
        info
    }
}

/// What a frame needs to know about the stream and the search effort.
struct FrameContext<'a> {
    sample_rate: u32,
    bits: u32,
    settings: &'a Settings,
    /// Window coefficients matching the length of the block being coded.
    windows: &'a [Vec<f64>],
}

/// Channel assignment codes of the frame header (RFC 9639, section 9.1.3).
const LEFT_SIDE: u32 = 8;
const SIDE_RIGHT: u32 = 9;
const MID_SIDE: u32 = 10;

/// Codes one block (one buffer per channel, all the same length) as a
/// complete frame, header and CRCs included.
fn encode_frame(ctx: &FrameContext, block: &[Vec<i64>], frame_number: u64) -> Vec<u8> {
    let len = block[0].len();
    let (assignment, subframes) = if block.len() == 2 {
        choose_stereo(ctx, &block[0], &block[1])
    } else {
        let subs = block
            .iter()
            .map(|ch| best_subframe(ctx, ch, ctx.bits))
            .collect();
        (block.len() as u32 - 1, subs)
    };

    let mut header = write_frame_header(ctx, len, assignment, frame_number);
    header.push(crc::crc8(&header));

    let mut w = BitWriter::with_capacity(len * block.len() * ctx.bits as usize / 8 + 16);
    for sub in &subframes {
        write_subframe(&mut w, sub);
    }
    let mut frame = header;
    frame.extend_from_slice(&w.into_bytes());
    let crc16 = crc::crc16(&frame);
    frame.extend_from_slice(&crc16.to_be_bytes());
    frame
}

/// Picks the cheapest of the four stereo channel assignments.
fn choose_stereo(ctx: &FrameContext, left: &[i64], right: &[i64]) -> (u32, Vec<Subframe>) {
    let side: Vec<i64> = left.iter().zip(right).map(|(l, r)| l - r).collect();
    // The arithmetic shift drops the low bit of the sum; the decoder gets
    // it back from the side channel, whose low bit is the same.
    let mid: Vec<i64> = left.iter().zip(right).map(|(l, r)| (l + r) >> 1).collect();
    let bits = ctx.bits;

    if ctx.settings.exhaustive_stereo {
        let l = best_subframe(ctx, left, bits);
        let r = best_subframe(ctx, right, bits);
        let s = best_subframe(ctx, &side, bits + 1);
        let m = best_subframe(ctx, &mid, bits);
        let costs = [
            l.bits + r.bits,
            l.bits + s.bits,
            s.bits + r.bits,
            m.bits + s.bits,
        ];
        let best = (0..4).min_by_key(|&i| costs[i]).unwrap_or(0);
        return match best {
            0 => (1, vec![l, r]),
            1 => (LEFT_SIDE, vec![l, s]),
            2 => (SIDE_RIGHT, vec![s, r]),
            _ => (MID_SIDE, vec![m, s]),
        };
    }

    // Cheap guess: the second order difference roughly tracks how well
    // each signal predicts, and coded size grows with its logarithm.
    let estimate = |x: &[i64]| -> f64 {
        let sum: u64 = x
            .windows(3)
            .map(|w| (w[2] - 2 * w[1] + w[0]).unsigned_abs())
            .sum();
        ((sum + 1) as f64).log2()
    };
    let (el, er, es, em) = (
        estimate(left),
        estimate(right),
        estimate(&side),
        estimate(&mid),
    );
    let costs = [el + er, el + es, es + er, em + es];
    let mut best = 0;
    for i in 1..4 {
        if costs[i] < costs[best] {
            best = i;
        }
    }
    match best {
        0 => (
            1,
            vec![
                best_subframe(ctx, left, bits),
                best_subframe(ctx, right, bits),
            ],
        ),
        1 => (
            LEFT_SIDE,
            vec![
                best_subframe(ctx, left, bits),
                best_subframe(ctx, &side, bits + 1),
            ],
        ),
        2 => (
            SIDE_RIGHT,
            vec![
                best_subframe(ctx, &side, bits + 1),
                best_subframe(ctx, right, bits),
            ],
        ),
        _ => (
            MID_SIDE,
            vec![
                best_subframe(ctx, &mid, bits),
                best_subframe(ctx, &side, bits + 1),
            ],
        ),
    }
}

/// Builds the frame header up to, but excluding, its CRC-8.
fn write_frame_header(ctx: &FrameContext, len: usize, assignment: u32, number: u64) -> Vec<u8> {
    let mut w = BitWriter::with_capacity(16);
    // 15-bit sync code (0b111111111111100), then 0 for the fixed block
    // size strategy.
    w.write_bits(0x7FFC, 15);
    w.write_bit(false);
    let (size_code, size_extra) = block_size_code(len);
    let (rate_code, rate_extra) = sample_rate_code(ctx.sample_rate);
    w.write_bits(size_code, 4);
    w.write_bits(rate_code, 4);
    w.write_bits(assignment, 4);
    w.write_bits(sample_size_code(ctx.bits), 3);
    w.write_bit(false);
    for b in coded_number(number) {
        w.write_bits(b as u32, 8);
    }
    if let Some((value, bits)) = size_extra {
        w.write_bits(value, bits);
    }
    if let Some((value, bits)) = rate_extra {
        w.write_bits(value, bits);
    }
    w.into_bytes()
}

/// Block size code and the optional field that follows the coded number
/// (RFC 9639, section 9.1.1).
fn block_size_code(len: usize) -> (u32, Option<(u32, u32)>) {
    match len {
        192 => (1, None),
        576 => (2, None),
        1152 => (3, None),
        2304 => (4, None),
        4608 => (5, None),
        256 => (8, None),
        512 => (9, None),
        1024 => (10, None),
        2048 => (11, None),
        4096 => (12, None),
        8192 => (13, None),
        16384 => (14, None),
        32768 => (15, None),
        n if n <= 256 => (6, Some((n as u32 - 1, 8))),
        n => (7, Some((n as u32 - 1, 16))),
    }
}

/// Sample rate code and its optional trailing field (section 9.1.2). Rates
/// without a code of their own fall back to the STREAMINFO value.
fn sample_rate_code(rate: u32) -> (u32, Option<(u32, u32)>) {
    match rate {
        88200 => (1, None),
        176400 => (2, None),
        192000 => (3, None),
        8000 => (4, None),
        16000 => (5, None),
        22050 => (6, None),
        24000 => (7, None),
        32000 => (8, None),
        44100 => (9, None),
        48000 => (10, None),
        96000 => (11, None),
        r if r % 1000 == 0 && r / 1000 <= 255 => (12, Some((r / 1000, 8))),
        r if r <= 0xFFFF => (13, Some((r, 16))),
        r if r % 10 == 0 && r / 10 <= 0xFFFF => (14, Some((r / 10, 16))),
        _ => (0, None),
    }
}

/// Bit depth code (section 9.1.4); depths without one point to STREAMINFO.
fn sample_size_code(bits: u32) -> u32 {
    match bits {
        8 => 1,
        12 => 2,
        16 => 4,
        20 => 5,
        24 => 6,
        32 => 7,
        _ => 0,
    }
}

/// The frame number in the variable length code of section 9.1.5, which
/// extends the UTF-8 byte layout to 36 bits.
fn coded_number(n: u64) -> Vec<u8> {
    if n < 0x80 {
        return vec![n as u8];
    }
    // Number of continuation bytes, each carrying 6 bits.
    let extra = match n {
        0..0x800 => 1,
        0x800..0x1_0000 => 2,
        0x1_0000..0x20_0000 => 3,
        0x20_0000..0x400_0000 => 4,
        0x400_0000..0x8000_0000 => 5,
        _ => 6,
    };
    let lead_marker = !(0xFFu8 >> (extra + 1));
    let mut out = vec![lead_marker | (n >> (6 * extra)) as u8];
    for i in (0..extra).rev() {
        out.push(0x80 | ((n >> (6 * i)) & 0x3F) as u8);
    }
    out
}

/// A residual's Rice coding: the partition order and one parameter per
/// partition.
#[derive(Clone, Debug, PartialEq)]
struct RicePlan {
    partition_order: u32,
    params: Vec<u32>,
    /// Coding method 1 (5-bit parameters) instead of 0 (4-bit).
    wide: bool,
    /// Exact size of the coded residual section in bits.
    bits: u64,
}

#[derive(Clone, Debug, PartialEq)]
enum Kind {
    Constant,
    Verbatim,
    Fixed {
        order: usize,
        rice: RicePlan,
    },
    Lpc {
        coefs: Vec<i32>,
        precision: u32,
        shift: u32,
        rice: RicePlan,
    },
}

/// One coded channel, ready to be written.
#[derive(Clone, Debug)]
struct Subframe {
    kind: Kind,
    wasted: u32,
    /// Bits per sample after removing the wasted bits.
    bits_per_sample: u32,
    /// Samples with the wasted bits shifted out.
    samples: Vec<i64>,
    residual: Vec<i64>,
    /// Size of the whole subframe in bits.
    bits: u64,
}

impl Subframe {
    fn header_bits(&self) -> u64 {
        8 + self.wasted as u64
    }
}

/// Codes one channel with the cheapest subframe type.
fn best_subframe(ctx: &FrameContext, signal: &[i64], bits: u32) -> Subframe {
    let n = signal.len();
    if signal.iter().all(|&s| s == signal[0]) {
        return Subframe {
            kind: Kind::Constant,
            wasted: 0,
            bits_per_sample: bits,
            samples: vec![signal[0]],
            residual: Vec::new(),
            bits: 8 + bits as u64,
        };
    }

    // Low bits that are zero in every sample need not be coded. The
    // signal is not constant here, so at least one bit is not wasted.
    let wasted = signal
        .iter()
        .fold(0i64, |acc, &s| acc | s)
        .trailing_zeros()
        .min(bits - 1);
    let samples: Vec<i64> = signal.iter().map(|&s| s >> wasted).collect();
    let sbits = bits - wasted;

    let mut best = Subframe {
        kind: Kind::Verbatim,
        wasted,
        bits_per_sample: sbits,
        samples,
        residual: Vec::new(),
        bits: 0,
    };
    best.bits = best.header_bits() + n as u64 * sbits as u64;
    let header = best.header_bits();
    let max_po = ctx.settings.max_partition_order;
    let mut residual = Vec::with_capacity(n);

    for order in 0..=4.min(n - 1) {
        if !fixed_residual(&best.samples, order, &mut residual) {
            continue;
        }
        let rice = plan_rice(&residual, n, order, max_po);
        let cost = header + (order as u64) * sbits as u64 + rice.bits;
        if cost < best.bits {
            best.bits = cost;
            best.kind = Kind::Fixed { order, rice };
            std::mem::swap(&mut best.residual, &mut residual);
        }
    }

    let max_order = ctx.settings.max_lpc_order.min(n - 1);
    if max_order > 0 && n >= MIN_LPC_BLOCK {
        search_lpc(ctx, &mut best, max_order, header);
    }
    best
}

/// Default coefficient precision: shorter blocks cannot amortize long
/// coefficients, and low depths do not need them.
fn base_precision(n: usize, bits: u32) -> u32 {
    let p = if n <= 1152 { 13 } else { 15 };
    p.min(bits + 6).max(5)
}

/// Searches LPC predictors and replaces `best` when one is cheaper.
fn search_lpc(ctx: &FrameContext, best: &mut Subframe, max_order: usize, header: u64) {
    let n = best.samples.len();
    let sbits = best.bits_per_sample;
    let max_po = ctx.settings.max_partition_order;
    let precision = base_precision(n, sbits);
    let mut residual = Vec::with_capacity(n);
    // Best LPC candidate as (cost, unquantized coefficients).
    let mut best_lpc: Option<(u64, Vec<f64>)> = None;

    let try_candidate =
        |best: &mut Subframe, coefs: &[f64], precision: u32, residual: &mut Vec<i64>| {
            let (q, shift) = quantize(coefs, precision)?;
            if !lpc_residual(&best.samples, &q, shift, residual) {
                return None;
            }
            let order = q.len() as u64;
            let rice = plan_rice(residual, n, q.len(), max_po);
            let cost = header + order * sbits as u64 + 4 + 5 + order * precision as u64 + rice.bits;
            if cost < best.bits {
                best.bits = cost;
                best.kind = Kind::Lpc {
                    coefs: q,
                    precision,
                    shift,
                    rice,
                };
                std::mem::swap(&mut best.residual, residual);
            }
            Some(cost)
        };

    for window in ctx.windows {
        let Some(models) = lpc_models(&best.samples, window, max_order) else {
            continue;
        };
        let orders: Vec<usize> = if ctx.settings.exhaustive_order {
            (1..=models.len()).collect()
        } else {
            vec![estimate_order(&models, n, sbits, precision)]
        };
        for order in orders {
            let coefs = &models[order - 1].0;
            if let Some(cost) = try_candidate(best, coefs, precision, &mut residual) {
                if best_lpc.as_ref().is_none_or(|(c, _)| cost < *c) {
                    best_lpc = Some((cost, coefs.clone()));
                }
            }
        }
    }

    // Coefficient precision trades header bits for prediction accuracy;
    // refine it only around the best predictor found.
    if ctx.settings.precision_search {
        if let Some((_, coefs)) = best_lpc {
            for p in [precision - 3, precision - 2, precision - 1, precision + 1] {
                if (5..=15).contains(&p) {
                    try_candidate(best, &coefs, p, &mut residual);
                }
            }
        }
    }
}

/// Predictor coefficients for every order from 1 up to `max_order`, each
/// with the prediction error energy the recursion reports. Returns `None`
/// when the windowed block is silent.
fn lpc_models(samples: &[i64], window: &[f64], max_order: usize) -> Option<Vec<(Vec<f64>, f64)>> {
    let n = samples.len();
    if window.len() != n {
        return None;
    }
    let x: Vec<f64> = samples
        .iter()
        .zip(window)
        .map(|(&s, &w)| s as f64 * w)
        .collect();
    let mut r = vec![0.0f64; max_order + 1];
    for (lag, slot) in r.iter_mut().enumerate() {
        *slot = x[lag..].iter().zip(&x).map(|(a, b)| a * b).sum();
    }
    if r[0] <= 0.0 {
        return None;
    }
    Some(levinson_durbin(&r))
}

/// Solves the normal equations for each order in turn (Levinson-Durbin).
/// `r` holds autocorrelation lags 0 to p. Result `i` holds the coefficients
/// of order `i + 1`, where coefficient `j` multiplies the sample `j + 1`
/// steps back. Stops early when the error stops being positive, which
/// happens when the signal is perfectly predictable or numerically
/// degenerate.
fn levinson_durbin(r: &[f64]) -> Vec<(Vec<f64>, f64)> {
    let mut models = Vec::with_capacity(r.len().saturating_sub(1));
    let mut a: Vec<f64> = Vec::with_capacity(r.len());
    let mut err = r[0];
    for i in 1..r.len() {
        let acc = r[i]
            - a.iter()
                .enumerate()
                .map(|(j, c)| c * r[i - 1 - j])
                .sum::<f64>();
        let k = acc / err;
        if !k.is_finite() {
            break;
        }
        let prev = a.clone();
        for j in 0..prev.len() {
            a[j] = prev[j] - k * prev[prev.len() - 1 - j];
        }
        a.push(k);
        err *= 1.0 - k * k;
        if err <= 0.0 || !err.is_finite() {
            // Still usable: an exact predictor of this order exists.
            models.push((a.clone(), f64::MIN_POSITIVE));
            break;
        }
        models.push((a.clone(), err));
    }
    models
}

/// Picks the order whose estimated size is smallest: each residual sample
/// costs about half the log2 of the error energy, each order adds a warm
/// up sample and a coefficient.
fn estimate_order(models: &[(Vec<f64>, f64)], n: usize, bits: u32, precision: u32) -> usize {
    let mut best = (f64::INFINITY, 1);
    for (i, (_, err)) in models.iter().enumerate() {
        let order = i + 1;
        let per_sample = 0.5 * (err / n as f64).max(1e-9).log2().max(0.0);
        let cost = per_sample * (n - order) as f64 + (order as u32 * (bits + precision)) as f64;
        if cost < best.0 {
            best = (cost, order);
        }
    }
    best.1
}

/// Quantizes coefficients to `precision` signed bits with a common right
/// shift. The shift is the largest that keeps the biggest coefficient in
/// range, capped at 15 since the header field cannot carry more and must
/// not be negative. Rounding errors are carried into the next coefficient,
/// which keeps the quantized filter closer to the real one.
fn quantize(coefs: &[f64], precision: u32) -> Option<(Vec<i32>, u32)> {
    let max = coefs.iter().fold(0.0f64, |m, c| m.max(c.abs()));
    if max <= 0.0 || !max.is_finite() {
        return None;
    }
    let limit = (1i32 << (precision - 1)) - 1;
    // Exponent such that max < 2^log2, then fit the scaled max under
    // 2^(precision - 1).
    let log2 = max.log2().floor() as i32 + 1;
    let shift = (precision as i32 - 1 - log2).min(15);
    if shift < 0 {
        return None;
    }
    let scale = (1u32 << shift) as f64;
    let mut carry = 0.0;
    let q = coefs
        .iter()
        .map(|&c| {
            let v = c * scale + carry;
            let r = v.round();
            carry = v - r;
            (r as i64).clamp(-(limit as i64) - 1, limit as i64) as i32
        })
        .collect::<Vec<_>>();
    if q.iter().all(|&c| c == 0) {
        return None;
    }
    Some((q, shift as u32))
}

/// Residual of the FIXED predictor of `order` (RFC 9639, section 9.2.5).
/// Returns false when a residual does not fit in 32 bits.
fn fixed_residual(x: &[i64], order: usize, out: &mut Vec<i64>) -> bool {
    out.clear();
    for i in order..x.len() {
        let p = match order {
            0 => 0,
            1 => x[i - 1],
            2 => 2 * x[i - 1] - x[i - 2],
            3 => 3 * x[i - 1] - 3 * x[i - 2] + x[i - 3],
            _ => 4 * x[i - 1] - 6 * x[i - 2] + 4 * x[i - 3] - x[i - 4],
        };
        let r = x[i] - p;
        if r.abs() > RESIDUAL_LIMIT {
            return false;
        }
        out.push(r);
    }
    true
}

/// Residual of an LPC predictor (section 9.2.6). Sums run in 64 bits: with
/// 33-bit samples, 15-bit coefficients and at most 32 taps they need 53.
fn lpc_residual(x: &[i64], coefs: &[i32], shift: u32, out: &mut Vec<i64>) -> bool {
    out.clear();
    let order = coefs.len();
    for i in order..x.len() {
        let history = &x[i - order..i];
        // Coefficient j applies to the sample j + 1 steps back.
        let sum: i64 = coefs
            .iter()
            .zip(history.iter().rev())
            .map(|(&c, &s)| c as i64 * s)
            .sum();
        let r = x[i] - (sum >> shift);
        if r.abs() > RESIDUAL_LIMIT {
            return false;
        }
        out.push(r);
    }
    true
}

/// Maps signed residuals to unsigned values (0, -1, 1, -2... to 0, 1, 2,
/// 3...) as Rice coding expects.
fn zigzag(r: i64) -> u64 {
    ((r << 1) ^ (r >> 63)) as u64
}

/// Estimated bits for `count` values summing to `sum` with parameter `k`.
/// The low bits cost exactly `k` each and the stop bit one; the unary part
/// is about the sum shifted down, minus half a bit per value lost to the
/// truncation of each quotient.
fn rice_estimate(count: u64, sum: u64, k: u32) -> u64 {
    let unary = if k == 0 {
        sum
    } else {
        (sum >> k).saturating_sub(count / 2)
    };
    count * (k as u64 + 1) + unary
}

/// Best parameter for one partition, by estimate.
fn rice_param(count: u64, sum: u64) -> (u32, u64) {
    if count == 0 || sum == 0 {
        return (0, count);
    }
    let mean = sum / count;
    let guess = if mean == 0 {
        0
    } else {
        63 - mean.leading_zeros()
    };
    let mut best = (0, u64::MAX);
    for k in guess.saturating_sub(1)..=(guess + 1).min(MAX_RICE_5BIT) {
        let bits = rice_estimate(count, sum, k);
        if bits < best.1 {
            best = (k, bits);
        }
    }
    best
}

/// Chooses the partition order and parameters for a residual. `residual`
/// excludes the `order` warm up samples of a block of `n` samples.
fn plan_rice(residual: &[i64], n: usize, order: usize, max_po: u32) -> RicePlan {
    // Partitions must split the block evenly and the first one must hold
    // at least one residual after the warm up samples.
    let mut top = max_po.min(n.trailing_zeros()).min(15);
    while top > 0 && (n >> top) <= order {
        top -= 1;
    }
    let mut sums = Vec::with_capacity(1 << top);
    let mut counts = Vec::with_capacity(1 << top);
    let mut start = 0;
    for p in 0..1usize << top {
        let len = (n >> top) - if p == 0 { order } else { 0 };
        sums.push(
            residual[start..start + len]
                .iter()
                .map(|&r| zigzag(r))
                .sum::<u64>(),
        );
        counts.push(len as u64);
        start += len;
    }

    let mut best: Option<(u64, u32, Vec<u32>)> = None;
    let mut po = top;
    loop {
        let mut params = Vec::with_capacity(sums.len());
        let mut bits = 0u64;
        for (&s, &c) in sums.iter().zip(&counts) {
            let (k, b) = rice_param(c, s);
            params.push(k);
            bits += b;
        }
        let wide = params.iter().any(|&k| k > MAX_RICE_4BIT);
        bits += params.len() as u64 * if wide { 5 } else { 4 };
        if best.as_ref().is_none_or(|(b, _, _)| bits < *b) {
            best = Some((bits, po, params));
        }
        if po == 0 {
            break;
        }
        po -= 1;
        sums = sums.chunks(2).map(|c| c[0] + c[1]).collect();
        counts = counts.chunks(2).map(|c| c[0] + c[1]).collect();
    }
    let (_, partition_order, params) = best.unwrap_or((0, 0, vec![0]));
    finish_plan(residual, n, order, partition_order, params)
}

/// Exact size of `values` Rice coded with parameter `k`.
fn rice_exact(values: &[i64], k: u32) -> u64 {
    values
        .iter()
        .map(|&r| (zigzag(r) >> k) + 1 + k as u64)
        .sum()
}

/// Settles each parameter of the chosen partition order on exact sizes,
/// testing the neighbours of the estimate, and returns the plan with its
/// exact size, so subframe types are compared on real sizes.
fn finish_plan(
    residual: &[i64],
    n: usize,
    order: usize,
    partition_order: u32,
    mut params: Vec<u32>,
) -> RicePlan {
    let mut data_bits = 0;
    let mut start = 0;
    for (p, k) in params.iter_mut().enumerate() {
        let len = (n >> partition_order) - if p == 0 { order } else { 0 };
        let part = &residual[start..start + len];
        let mut best = (*k, rice_exact(part, *k));
        for candidate in [k.saturating_sub(1), *k + 1] {
            if candidate != *k && candidate <= MAX_RICE_5BIT {
                let bits = rice_exact(part, candidate);
                if bits < best.1 {
                    best = (candidate, bits);
                }
            }
        }
        *k = best.0;
        data_bits += best.1;
        start += len;
    }
    let wide = params.iter().any(|&k| k > MAX_RICE_4BIT);
    let bits = 2 + 4 + params.len() as u64 * if wide { 5 } else { 4 } + data_bits;
    RicePlan {
        partition_order,
        params,
        wide,
        bits,
    }
}

/// Writes `value` as a two's complement field of `bits` bits (up to 33 for
/// a side channel of 32-bit audio).
fn write_signed(w: &mut BitWriter, value: i64, bits: u32) {
    w.write_bits_u64(value as u64, bits);
}

fn write_subframe(w: &mut BitWriter, sub: &Subframe) {
    // Subframe header: a zero padding bit, the 6-bit type, and the wasted
    // bits flag followed by the count minus one in unary (section 9.2.1).
    let kind = match &sub.kind {
        Kind::Constant => 0,
        Kind::Verbatim => 1,
        Kind::Fixed { order, .. } => 8 | *order as u32,
        Kind::Lpc { coefs, .. } => 32 | (coefs.len() as u32 - 1),
    };
    w.write_bits(kind, 7);
    if sub.wasted > 0 {
        w.write_bit(true);
        w.write_unary(sub.wasted - 1);
    } else {
        w.write_bit(false);
    }
    let bits = sub.bits_per_sample;
    match &sub.kind {
        Kind::Constant => write_signed(w, sub.samples[0], bits),
        Kind::Verbatim => {
            for &s in &sub.samples {
                write_signed(w, s, bits);
            }
        }
        Kind::Fixed { order, rice } => {
            for &s in &sub.samples[..*order] {
                write_signed(w, s, bits);
            }
            write_residual(w, &sub.residual, sub.samples.len(), *order, rice);
        }
        Kind::Lpc {
            coefs,
            precision,
            shift,
            rice,
        } => {
            for &s in &sub.samples[..coefs.len()] {
                write_signed(w, s, bits);
            }
            w.write_bits(precision - 1, 4);
            w.write_bits(*shift, 5);
            for &c in coefs {
                w.write_bits_signed(c, *precision);
            }
            write_residual(w, &sub.residual, sub.samples.len(), coefs.len(), rice);
        }
    }
}

/// Writes the coded residual section (section 9.2.7).
fn write_residual(w: &mut BitWriter, residual: &[i64], n: usize, order: usize, rice: &RicePlan) {
    w.write_bits(rice.wide as u32, 2);
    w.write_bits(rice.partition_order, 4);
    let param_bits = if rice.wide { 5 } else { 4 };
    let mut start = 0;
    for (p, &k) in rice.params.iter().enumerate() {
        w.write_bits(k, param_bits);
        let len = (n >> rice.partition_order) - if p == 0 { order } else { 0 };
        for &r in &residual[start..start + len] {
            // Residuals fit in 32 bits, so the zigzag value fits in u32.
            let u = zigzag(r) as u32;
            w.write_unary(u >> k);
            if k > 0 {
                w.write_bits(u, k);
            }
        }
        start += len;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::flac::STREAMINFO_SIZE;
    use crate::frame::AudioFrame;
    use crate::util::bits::BitReader;

    /// Deterministic pseudo random numbers (a 64-bit LCG), so the test
    /// signals are identical on every platform.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 32) as u32
        }

        /// Uniform value in -2^(bits-1) .. 2^(bits-1) - 1.
        fn sample(&mut self, bits: u32) -> i32 {
            (self.next() as i32) >> (32 - bits)
        }
    }

    /// A tone made of integer triangle waves plus a little noise, with a
    /// different mix per channel so stereo decorrelation has work to do.
    fn test_signal(len: usize, channels: usize, bits: u32, seed: u64) -> Vec<i32> {
        let mut rng = Lcg(seed);
        let amp = (1i64 << (bits - 1)) / 3;
        let max = (1i64 << (bits - 1)) - 1;
        let mut out = Vec::with_capacity(len * channels);
        for i in 0..len as i64 {
            for c in 0..channels as i64 {
                let period = 97 + 13 * c;
                let phase = (i + 7 * c) % period;
                let tri = (4 * amp * phase / period - 2 * amp).abs() - amp;
                let noise = if bits > 6 {
                    rng.sample(bits - 6) as i64
                } else {
                    0
                };
                out.push((tri + noise).clamp(-max - 1, max) as i32);
            }
        }
        out
    }

    fn format(channels: u16, bits: u32, rate: u32) -> FrameFormat {
        FrameFormat::Audio(AudioFormat {
            sample_rate: rate,
            channels,
            kind: SampleKind::Int(bits),
        })
    }

    fn options(level: u32) -> EncoderOptions {
        EncoderOptions {
            stream_index: 0,
            compression_level: Some(level),
        }
    }

    /// Encodes interleaved samples fed in chunks of `chunk` samples per
    /// channel and returns the packets and the final stream info.
    fn encode_all(
        samples: &[i32],
        channels: u16,
        bits: u32,
        rate: u32,
        level: u32,
        chunk: usize,
    ) -> (Vec<Packet>, StreamInfo) {
        let mut enc = new_encoder(
            CodecId::Flac,
            &format(channels, bits, rate),
            &options(level),
        )
        .unwrap();
        let mut packets = Vec::new();
        let mut pts = 0;
        for part in samples.chunks(chunk * channels as usize) {
            let data = AudioData::Int {
                samples: part.to_vec(),
                bits,
            };
            let frame = AudioFrame::new(rate, channels, data, pts);
            pts += frame.nb_samples() as i64;
            packets.extend(enc.encode(&Frame::Audio(frame)).unwrap());
        }
        packets.extend(enc.flush().unwrap());
        (packets, enc.stream_info())
    }

    /// What the test decoder saw in one subframe.
    #[derive(Debug)]
    struct SubInfo {
        kind: u32,
        wasted: u32,
    }

    struct DecodedFrame {
        number: u64,
        assignment: u32,
        samples: Vec<Vec<i64>>,
        subframes: Vec<SubInfo>,
    }

    fn read_signed(r: &mut BitReader, bits: u32) -> i64 {
        let v = r.read_bits_u64(bits).unwrap();
        ((v << (64 - bits)) as i64) >> (64 - bits)
    }

    fn read_coded_number(r: &mut BitReader) -> u64 {
        let first = r.read_bits(8).unwrap() as u8;
        if first < 0x80 {
            return first as u64;
        }
        // The count of leading ones is the total byte count.
        let ones = first.leading_ones();
        assert!((2..=7).contains(&ones), "bad leading byte");
        let mut value = (first & (0x7F >> ones)) as u64;
        for _ in 1..ones {
            let b = r.read_bits(8).unwrap();
            assert_eq!(b & 0xC0, 0x80, "bad continuation byte");
            value = value << 6 | (b & 0x3F) as u64;
        }
        value
    }

    /// Decodes one frame that must fill `data` exactly.
    fn decode_frame(data: &[u8], bits: u32) -> DecodedFrame {
        let (frame, len) = decode_frame_prefix(data, bits);
        assert_eq!(len, data.len(), "frame length");
        frame
    }

    /// Decodes the frame at the start of `data`, checking both CRCs, and
    /// returns it with its length. Written against RFC 9639 independently
    /// of the encoder's helpers.
    fn decode_frame_prefix(data: &[u8], bits: u32) -> (DecodedFrame, usize) {
        let mut r = BitReader::new(data);
        assert_eq!(r.read_bits(15).unwrap(), 0x7FFC, "sync code");
        assert!(!r.read_bit().unwrap(), "fixed block size strategy");
        let size_code = r.read_bits(4).unwrap();
        let rate_code = r.read_bits(4).unwrap();
        let assignment = r.read_bits(4).unwrap();
        let depth_code = r.read_bits(3).unwrap();
        assert!(!r.read_bit().unwrap(), "reserved bit");
        let number = read_coded_number(&mut r);
        let block_size = match size_code {
            1 => 192,
            2..=5 => 576 << (size_code - 2),
            6 => r.read_bits(8).unwrap() as usize + 1,
            7 => r.read_bits(16).unwrap() as usize + 1,
            8..=15 => 256 << (size_code - 8),
            _ => panic!("reserved block size code"),
        };
        match rate_code {
            12..=14 => {
                r.read_bits(if rate_code == 12 { 8 } else { 16 }).unwrap();
            }
            15 => panic!("invalid sample rate code"),
            _ => {}
        }
        let depth = match depth_code {
            0 => bits,
            1 => 8,
            2 => 12,
            4 => 16,
            5 => 20,
            6 => 24,
            7 => 32,
            _ => panic!("reserved bit depth code"),
        };
        assert_eq!(depth, bits);
        let header_len = r.byte_position();
        assert_eq!(
            r.read_bits(8).unwrap() as u8,
            crc::crc8(&data[..header_len])
        );

        let channels = match assignment {
            0..=7 => assignment as usize + 1,
            8..=10 => 2,
            _ => panic!("reserved channel assignment"),
        };
        let mut samples = Vec::new();
        let mut subframes = Vec::new();
        for ch in 0..channels {
            let side = matches!((assignment, ch), (8, 1) | (9, 0) | (10, 1));
            let (s, info) = decode_subframe(&mut r, block_size, bits + side as u32);
            samples.push(s);
            subframes.push(info);
        }
        r.align();
        let end = r.byte_position();
        let crc = r.read_bits(16).unwrap() as u16;
        assert_eq!(crc, crc::crc16(&data[..end]), "frame CRC-16");

        if channels == 2 {
            let (a, b) = (samples[0].clone(), samples[1].clone());
            let (l, r): (Vec<i64>, Vec<i64>) = match assignment {
                8 => a.iter().zip(&b).map(|(&l, &s)| (l, l - s)).unzip(),
                9 => a.iter().zip(&b).map(|(&s, &r)| (s + r, r)).unzip(),
                10 => a
                    .iter()
                    .zip(&b)
                    .map(|(&m, &s)| {
                        let sum = (m << 1) | (s & 1);
                        ((sum + s) >> 1, (sum - s) >> 1)
                    })
                    .unzip(),
                _ => (a, b),
            };
            samples = vec![l, r];
        }
        let frame = DecodedFrame {
            number,
            assignment,
            samples,
            subframes,
        };
        (frame, end + 2)
    }

    fn decode_subframe(r: &mut BitReader, n: usize, bits: u32) -> (Vec<i64>, SubInfo) {
        assert!(!r.read_bit().unwrap(), "subframe padding bit");
        let kind = r.read_bits(6).unwrap();
        let wasted = if r.read_bit().unwrap() {
            r.read_unary().unwrap() + 1
        } else {
            0
        };
        let sbits = bits - wasted;
        let mut x: Vec<i64> = match kind {
            0 => vec![read_signed(r, sbits); n],
            1 => (0..n).map(|_| read_signed(r, sbits)).collect(),
            8..=12 => {
                let order = (kind - 8) as usize;
                let mut x: Vec<i64> = (0..order).map(|_| read_signed(r, sbits)).collect();
                let res = decode_residual(r, n, order);
                // Fixed predictor coefficients from RFC 9639, section 9.2.5.
                let taps: &[i64] = match order {
                    0 => &[],
                    1 => &[1],
                    2 => &[2, -1],
                    3 => &[3, -3, 1],
                    _ => &[4, -6, 4, -1],
                };
                for e in res {
                    let i = x.len();
                    let p: i64 = taps.iter().enumerate().map(|(j, t)| t * x[i - 1 - j]).sum();
                    x.push(p + e);
                }
                x
            }
            32..=63 => {
                let order = (kind - 31) as usize;
                let mut x: Vec<i64> = (0..order).map(|_| read_signed(r, sbits)).collect();
                let precision = r.read_bits(4).unwrap() + 1;
                assert!(precision <= 15);
                let shift = read_signed(r, 5);
                assert!((0..=15).contains(&shift), "negative or large shift");
                let coefs: Vec<i64> = (0..order).map(|_| read_signed(r, precision)).collect();
                let res = decode_residual(r, n, order);
                for e in res {
                    let i = x.len();
                    let p: i64 = coefs
                        .iter()
                        .enumerate()
                        .map(|(j, c)| c * x[i - 1 - j])
                        .sum();
                    x.push((p >> shift) + e);
                }
                x
            }
            _ => panic!("reserved subframe type {kind}"),
        };
        for s in &mut x {
            *s <<= wasted;
        }
        (x, SubInfo { kind, wasted })
    }

    fn decode_residual(r: &mut BitReader, n: usize, order: usize) -> Vec<i64> {
        let method = r.read_bits(2).unwrap();
        assert!(method <= 1, "reserved residual coding method");
        let param_bits = 4 + method;
        let po = r.read_bits(4).unwrap();
        assert_eq!(n % (1 << po), 0, "partitions must split the block evenly");
        let mut out = Vec::new();
        for p in 0..1usize << po {
            let k = r.read_bits(param_bits).unwrap();
            assert_ne!(k, (1 << param_bits) - 1, "escape codes are never written");
            let len = (n >> po) - if p == 0 { order } else { 0 };
            for _ in 0..len {
                let q = r.read_unary().unwrap() as u64;
                let u = q << k | r.read_bits(k).unwrap() as u64;
                assert!(u <= u32::MAX as u64);
                out.push((u >> 1) as i64 ^ -((u & 1) as i64));
            }
        }
        out
    }

    /// Decodes all packets and checks numbering, timing and the samples.
    fn check_round_trip(samples: &[i32], channels: usize, bits: u32, packets: &[Packet]) {
        let mut decoded = vec![Vec::new(); channels];
        let mut next_pts = 0;
        for (i, p) in packets.iter().enumerate() {
            let f = decode_frame(&p.data, bits);
            assert_eq!(f.number, i as u64);
            assert_eq!(p.pts, next_pts);
            assert_eq!(p.duration as usize, f.samples[0].len());
            next_pts += p.duration;
            for (c, s) in f.samples.into_iter().enumerate() {
                decoded[c].extend(s);
            }
        }
        let interleaved: Vec<i32> = (0..decoded[0].len())
            .flat_map(|i| decoded.iter().map(move |ch| ch[i] as i32))
            .collect();
        assert_eq!(interleaved.len(), samples.len());
        assert!(interleaved == samples, "decoded samples differ");
    }

    #[test]
    fn every_level_round_trips() {
        let samples = test_signal(10_001, 2, 16, 1);
        for level in 0..=8 {
            let (packets, info) = encode_all(&samples, 2, 16, 44100, level, 1000);
            check_round_trip(&samples, 2, 16, &packets);
            let block = if level <= 2 { 1152 } else { 4096 };
            assert_eq!(packets[0].duration, block, "level {level}");
            assert_eq!(info.duration, Some(10_001));
        }
    }

    #[test]
    fn higher_levels_compress_better() {
        let samples = test_signal(20_000, 2, 16, 2);
        let size = |level| {
            let (packets, _) = encode_all(&samples, 2, 16, 44100, level, 4096);
            packets.iter().map(|p| p.data.len()).sum::<usize>()
        };
        let (fast, default, best) = (size(0), size(5), size(8));
        assert!(default < fast, "{default} >= {fast}");
        assert!(best <= default, "{best} > {default}");
        // The signal carries 10 bits of noise per sample, so the coded size
        // ends up a little above 10/16 of the 16-bit input.
        assert!(default < 20_000 * 2 * 2 * 7 / 10, "{default}");
    }

    #[test]
    fn every_bit_depth_round_trips() {
        for bits in [4, 5, 8, 12, 16, 20, 23, 24, 32] {
            let mut samples = test_signal(5000, 1, bits, bits as u64);
            // Include both extremes of the range.
            samples[10] = -(1i64 << (bits - 1)) as i32;
            samples[11] = ((1i64 << (bits - 1)) - 1) as i32;
            for level in [0, 5] {
                let (packets, _) = encode_all(&samples, 1, bits, 48000, level, 4096);
                check_round_trip(&samples, 1, bits, &packets);
            }
        }
    }

    #[test]
    fn full_scale_noise_at_32_bits_survives_side_overflow() {
        // Opposite extremes make the side channel need 33 bits and push
        // residuals beyond 32 bits, which must fall back to safe subframes.
        let mut rng = Lcg(9);
        let mut samples = Vec::new();
        for i in 0..3000 {
            let (l, r) = if i % 2 == 0 {
                (i32::MAX, i32::MIN)
            } else {
                (rng.sample(32), rng.sample(32))
            };
            samples.extend([l, r]);
        }
        for level in [0, 2, 5, 8] {
            let (packets, _) = encode_all(&samples, 2, 32, 96000, level, 4096);
            check_round_trip(&samples, 2, 32, &packets);
        }
    }

    #[test]
    fn every_channel_count_round_trips() {
        for channels in 1..=8 {
            let samples = test_signal(4500, channels, 16, channels as u64);
            let (packets, _) = encode_all(&samples, channels as u16, 16, 48000, 5, 777);
            check_round_trip(&samples, channels, 16, &packets);
        }
    }

    #[test]
    fn tiny_inputs_round_trip() {
        for len in 1..=40 {
            for channels in [1, 2] {
                let samples = test_signal(len, channels, 16, len as u64);
                for level in [0, 5, 8] {
                    let (packets, info) =
                        encode_all(&samples, channels as u16, 16, 44100, level, 3);
                    assert_eq!(packets.len(), 1);
                    check_round_trip(&samples, channels, 16, &packets);
                    let si = StreamInfoBlock::parse(&info.extradata).unwrap();
                    assert_eq!(si.total_samples, len as u64);
                }
            }
        }
    }

    #[test]
    fn empty_input_gives_no_frames() {
        let (packets, info) = encode_all(&[], 2, 16, 44100, 5, 100);
        assert!(packets.is_empty());
        let si = StreamInfoBlock::parse(&info.extradata).unwrap();
        assert_eq!(si.total_samples, 0);
        assert_eq!((si.min_frame_size, si.max_frame_size), (0, 0));
    }

    #[test]
    fn silence_uses_constant_subframes() {
        let samples = vec![0i32; 2 * 9000];
        let (packets, _) = encode_all(&samples, 2, 16, 44100, 5, 4096);
        for p in &packets {
            let f = decode_frame(&p.data, 16);
            assert!(f.subframes.iter().all(|s| s.kind == 0));
            // Header (at most 10 bytes here), two 3-byte constant
            // subframes and the CRC-16.
            assert!(p.data.len() <= 18, "{}", p.data.len());
        }
        check_round_trip(&samples, 2, 16, &packets);
    }

    #[test]
    fn wasted_bits_are_detected() {
        // 16-bit samples that only use the top 12 bits.
        let samples: Vec<i32> = test_signal(5000, 1, 12, 3).iter().map(|s| s << 4).collect();
        let (packets, _) = encode_all(&samples, 1, 16, 44100, 5, 4096);
        for p in &packets {
            let f = decode_frame(&p.data, 16);
            assert_eq!(f.subframes[0].wasted, 4);
        }
        check_round_trip(&samples, 1, 16, &packets);
    }

    #[test]
    fn identical_channels_are_decorrelated() {
        let mono = test_signal(8192, 1, 16, 4);
        let samples: Vec<i32> = mono.iter().flat_map(|&s| [s, s]).collect();
        for level in [0, 5] {
            let (packets, _) = encode_all(&samples, 2, 16, 44100, level, 4096);
            for p in &packets {
                let f = decode_frame(&p.data, 16);
                assert!(f.assignment >= LEFT_SIDE, "level {level}");
                // The side channel is all zeros.
                let side = if f.assignment == SIDE_RIGHT { 0 } else { 1 };
                assert_eq!(f.subframes[side].kind, 0);
            }
            check_round_trip(&samples, 2, 16, &packets);
        }
    }

    #[test]
    fn subframe_size_accounting_is_exact() {
        // The search compares candidates by their computed size; it must
        // match what is actually written, or the choice is meaningless.
        let ctx_settings = Settings::for_level(8);
        for (len, bits, seed) in [(4096, 16, 1), (1000, 24, 2), (37, 8, 3), (4096, 33, 4)] {
            let windows: Vec<Vec<f64>> = ctx_settings
                .windows
                .iter()
                .map(|w| w.coefficients(len))
                .collect();
            let ctx = FrameContext {
                sample_rate: 44100,
                bits: 16,
                settings: &ctx_settings,
                windows: &windows,
            };
            let signal: Vec<i64> = test_signal(len, 1, bits.min(32), seed)
                .iter()
                .map(|&s| s as i64)
                .collect();
            let sub = best_subframe(&ctx, &signal, bits);
            let mut w = BitWriter::new();
            write_subframe(&mut w, &sub);
            assert_eq!(w.bit_len() as u64, sub.bits, "len {len} bits {bits}");
        }
    }

    #[test]
    fn streaminfo_placeholders_then_final_values() {
        let samples = test_signal(10_000, 2, 16, 5);
        let mut enc = new_encoder(CodecId::Flac, &format(2, 16, 44100), &options(5)).unwrap();
        let before = enc.stream_info();
        assert_eq!(before.extradata.len(), STREAMINFO_SIZE);
        let si = StreamInfoBlock::parse(&before.extradata).unwrap();
        assert_eq!((si.min_block_size, si.max_block_size), (4096, 4096));
        assert_eq!((si.min_frame_size, si.max_frame_size), (0, 0));
        assert_eq!(si.total_samples, 0);
        assert_eq!(si.md5, [0; 16]);
        assert_eq!(
            (si.sample_rate, si.channels, si.bits_per_sample),
            (44100, 2, 16)
        );
        assert_eq!(before.duration, None);

        // Timestamps follow the input, starting wherever it starts.
        let frame = AudioFrame::new(
            44100,
            2,
            AudioData::Int {
                samples: samples.clone(),
                bits: 16,
            },
            1000,
        );
        let mut packets = enc.encode(&Frame::Audio(frame)).unwrap();
        assert_eq!(packets.len(), 2);
        assert_eq!(
            StreamInfoBlock::parse(&enc.stream_info().extradata).unwrap(),
            si
        );
        packets.extend(enc.flush().unwrap());
        assert_eq!(
            packets
                .iter()
                .map(|p| (p.pts, p.duration))
                .collect::<Vec<_>>(),
            [(1000, 4096), (5096, 4096), (9192, 1808)]
        );
        assert!(enc.flush().unwrap().is_empty());

        let after = enc.stream_info();
        let si = StreamInfoBlock::parse(&after.extradata).unwrap();
        let sizes: Vec<u32> = packets.iter().map(|p| p.data.len() as u32).collect();
        assert_eq!(si.min_frame_size, *sizes.iter().min().unwrap());
        assert_eq!(si.max_frame_size, *sizes.iter().max().unwrap());
        assert_eq!(si.total_samples, 10_000);
        assert_eq!((si.min_block_size, si.max_block_size), (4096, 4096));
        let mut md5 = Md5::new();
        for s in &samples {
            md5.update(&(*s as i16).to_le_bytes());
        }
        assert_eq!(si.md5, md5.finalize());
        assert_eq!(after.duration, Some(10_000));
        assert_eq!(after.codec, CodecId::Flac);
        assert!(after.bit_rate.is_some());
    }

    #[test]
    fn md5_uses_whole_bytes_per_sample() {
        // 12-bit samples hash as 2 bytes, 20-bit ones as 3.
        for (bits, bytes) in [(4, 1), (12, 2), (20, 3), (24, 3), (32, 4)] {
            let samples = test_signal(100, 1, bits, 6);
            let (_, info) = encode_all(&samples, 1, bits, 8000, 0, 100);
            let mut md5 = Md5::new();
            for s in &samples {
                md5.update(&s.to_le_bytes()[..bytes]);
            }
            let si = StreamInfoBlock::parse(&info.extradata).unwrap();
            assert_eq!(si.md5, md5.finalize(), "{bits} bits");
        }
    }

    #[test]
    fn input_format_negotiation() {
        let audio = |kind, channels, rate| {
            FrameFormat::Audio(AudioFormat {
                sample_rate: rate,
                channels,
                kind,
            })
        };
        let make = |f: FrameFormat, level| new_encoder(CodecId::Flac, &f, &options(level));
        let chosen = |f| match make(f, 5).unwrap().input_format() {
            FrameFormat::Audio(a) => a,
            FrameFormat::Video(_) => panic!(),
        };
        assert_eq!(
            chosen(audio(SampleKind::Float, 2, 48000)).kind,
            SampleKind::Int(24)
        );
        let a = chosen(audio(SampleKind::Int(16), 6, 96000));
        assert_eq!(
            (a.kind, a.channels, a.sample_rate),
            (SampleKind::Int(16), 6, 96000)
        );
        assert_eq!(
            chosen(audio(SampleKind::Int(32), 1, 8000)).kind,
            SampleKind::Int(32)
        );
        assert_eq!(chosen(audio(SampleKind::Int(8), 1, 1)).sample_rate, 1);
        assert!(make(audio(SampleKind::Int(16), 1, MAX_SAMPLE_RATE), 5).is_ok());

        assert!(make(audio(SampleKind::Int(16), 0, 44100), 5).is_err());
        assert!(make(audio(SampleKind::Int(16), 9, 44100), 5).is_err());
        assert!(make(audio(SampleKind::Int(16), 2, 0), 5).is_err());
        assert!(make(audio(SampleKind::Int(16), 2, MAX_SAMPLE_RATE + 1), 5).is_err());
        assert!(make(audio(SampleKind::Int(3), 2, 44100), 5).is_err());
        assert!(make(audio(SampleKind::Int(28), 2, 44100), 5).is_err());
        assert!(make(audio(SampleKind::Int(16), 2, 44100), 9).is_err());
        assert!(new_encoder(CodecId::PcmS16Le, &format(2, 16, 44100), &options(5)).is_err());
        let video = FrameFormat::Video(crate::frame::VideoFormat {
            width: 2,
            height: 2,
            pixel_format: crate::frame::PixelFormat::Gray,
            frame_rate: Rational::new(25, 1),
        });
        assert!(new_encoder(CodecId::Flac, &video, &options(5)).is_err());
        // No level means the default.
        let enc = new_encoder(
            CodecId::Flac,
            &format(1, 16, 44100),
            &EncoderOptions::default(),
        );
        assert!(enc.is_ok());
    }

    #[test]
    fn rejects_mismatched_frames() {
        let mut enc = new_encoder(CodecId::Flac, &format(2, 16, 44100), &options(5)).unwrap();
        let frame = |samples: Vec<i32>, bits, channels| {
            Frame::Audio(AudioFrame {
                sample_rate: 44100,
                channels,
                data: AudioData::Int { samples, bits },
                pts: 0,
            })
        };
        assert!(enc.encode(&frame(vec![0; 4], 24, 2)).is_err());
        assert!(enc.encode(&frame(vec![0; 4], 16, 1)).is_err());
        let other_rate = Frame::Audio(AudioFrame::new(
            48000,
            2,
            AudioData::Int {
                samples: vec![0; 2],
                bits: 16,
            },
            0,
        ));
        assert!(enc.encode(&other_rate).is_err());
        assert!(enc.encode(&frame(vec![0; 3], 16, 2)).is_err());
        assert!(enc.encode(&frame(vec![0, 40000], 16, 2)).is_err());
        assert!(enc.encode(&frame(vec![0, -32769], 16, 2)).is_err());
        let float = Frame::Audio(AudioFrame::new(44100, 2, AudioData::Float(vec![0.0; 2]), 0));
        assert!(enc.encode(&float).is_err());
        assert!(enc.encode(&frame(vec![1, -1], 16, 2)).unwrap().is_empty());
        assert_eq!(enc.flush().unwrap().len(), 1);
        assert!(enc.encode(&frame(vec![1, -1], 16, 2)).is_err());
    }

    #[test]
    fn frame_header_fields() {
        assert_eq!(coded_number(0), [0]);
        assert_eq!(coded_number(0x7F), [0x7F]);
        assert_eq!(coded_number(0x80), [0xC2, 0x80]);
        assert_eq!(coded_number(0x7FF), [0xDF, 0xBF]);
        assert_eq!(coded_number(0x800), [0xE0, 0xA0, 0x80]);
        assert_eq!(coded_number(0xFFFF), [0xEF, 0xBF, 0xBF]);
        assert_eq!(coded_number(0x10000), [0xF0, 0x90, 0x80, 0x80]);
        assert_eq!(
            coded_number(0x7FFF_FFFF),
            [0xFD, 0xBF, 0xBF, 0xBF, 0xBF, 0xBF]
        );
        assert_eq!(
            coded_number(0xF_FFFF_FFFF),
            [0xFE, 0xBF, 0xBF, 0xBF, 0xBF, 0xBF, 0xBF]
        );
        for n in [0, 1, 0x7F, 0x80, 0x1234, 0x10_0000, 0x7FFF_FFFF] {
            let bytes = coded_number(n);
            assert_eq!(read_coded_number(&mut BitReader::new(&bytes)), n);
        }

        assert_eq!(block_size_code(1152), (3, None));
        assert_eq!(block_size_code(4096), (12, None));
        assert_eq!(block_size_code(192), (1, None));
        assert_eq!(block_size_code(1), (6, Some((0, 8))));
        assert_eq!(block_size_code(256), (8, None));
        assert_eq!(block_size_code(255), (6, Some((254, 8))));
        assert_eq!(block_size_code(257), (7, Some((256, 16))));
        assert_eq!(block_size_code(4095), (7, Some((4094, 16))));

        assert_eq!(sample_rate_code(44100), (9, None));
        assert_eq!(sample_rate_code(48000), (10, None));
        assert_eq!(sample_rate_code(192000), (3, None));
        assert_eq!(sample_rate_code(11000), (12, Some((11, 8))));
        assert_eq!(sample_rate_code(11025), (13, Some((11025, 16))));
        assert_eq!(sample_rate_code(352800), (14, Some((35280, 16))));
        assert_eq!(sample_rate_code(352801), (0, None));
        assert_eq!(sample_rate_code(MAX_SAMPLE_RATE), (0, None));

        assert_eq!(sample_size_code(16), 4);
        assert_eq!(sample_size_code(24), 6);
        assert_eq!(sample_size_code(32), 7);
        assert_eq!(sample_size_code(4), 0);
        assert_eq!(sample_size_code(20), 5);
    }

    #[test]
    fn uncommon_rates_and_sizes_round_trip() {
        for rate in [1, 11025, 37800, 352800, 705601, MAX_SAMPLE_RATE] {
            let samples = test_signal(5000, 2, 16, rate as u64);
            let (packets, info) = encode_all(&samples, 2, 16, rate, 3, 1000);
            check_round_trip(&samples, 2, 16, &packets);
            let si = StreamInfoBlock::parse(&info.extradata).unwrap();
            assert_eq!(si.sample_rate, rate);
        }
    }

    #[test]
    fn rice_plan_matches_written_size() {
        let mut rng = Lcg(7);
        for (n, order, spread) in [(4096, 0, 10), (4096, 8, 200), (1152, 2, 3), (10, 3, 1000)] {
            let residual: Vec<i64> = (0..n - order)
                .map(|i| {
                    let scale = if i % 512 < 64 { spread * 50 } else { spread };
                    rng.next() as i64 % scale - scale / 2
                })
                .collect();
            let plan = plan_rice(&residual, n, order, 8);
            assert!(n % (1 << plan.partition_order) == 0);
            assert!(plan.partition_order == 0 || (n >> plan.partition_order) > order);
            assert_eq!(plan.params.len(), 1 << plan.partition_order);
            let mut w = BitWriter::new();
            write_residual(&mut w, &residual, n, order, &plan);
            assert_eq!(w.bit_len() as u64, plan.bits);
            let bytes = w.into_bytes();
            let mut r = BitReader::new(&bytes);
            assert_eq!(decode_residual(&mut r, n, order), residual);
        }
    }

    #[test]
    fn rice_switches_to_wide_parameters_for_large_residuals() {
        let residual: Vec<i64> = (0..256)
            .map(|i| {
                if i % 2 == 0 {
                    RESIDUAL_LIMIT
                } else {
                    -RESIDUAL_LIMIT
                }
            })
            .collect();
        let plan = plan_rice(&residual, 256, 0, 4);
        assert!(plan.wide);
        assert!(plan.params.iter().all(|&k| k <= MAX_RICE_5BIT));
        let mut w = BitWriter::new();
        write_residual(&mut w, &residual, 256, 0, &plan);
        let bytes = w.into_bytes();
        assert_eq!(
            decode_residual(&mut BitReader::new(&bytes), 256, 0),
            residual
        );

        let small = vec![0i64, 1, -1, 2];
        assert!(!plan_rice(&small, 4, 0, 4).wide);
    }

    #[test]
    fn partition_order_respects_block_and_warm_up() {
        let residual = vec![0i64; 1152 - 4];
        let plan = plan_rice(&residual, 1152, 4, 15);
        assert!(plan.partition_order <= 7, "1152 is 9 * 2^7");
        let residual = vec![5i64; 4096 - 12];
        let plan = plan_rice(&residual, 4096, 12, 15);
        assert!(4096 >> plan.partition_order > 12);
        let plan = plan_rice(&[3], 1, 0, 8);
        assert_eq!(plan.partition_order, 0);
    }

    #[test]
    fn fixed_residual_reconstructs() {
        let x: Vec<i64> = test_signal(300, 1, 16, 8)
            .iter()
            .map(|&s| s as i64)
            .collect();
        let taps: [&[i64]; 5] = [&[], &[1], &[2, -1], &[3, -3, 1], &[4, -6, 4, -1]];
        for (order, t) in taps.iter().enumerate() {
            let mut res = Vec::new();
            assert!(fixed_residual(&x, order, &mut res));
            assert_eq!(res.len(), x.len() - order);
            let mut y = x[..order].to_vec();
            for e in res {
                let i = y.len();
                let p: i64 = t.iter().enumerate().map(|(j, c)| c * y[i - 1 - j]).sum();
                y.push(p + e);
            }
            assert_eq!(y, x, "order {order}");
        }
        // A residual beyond 32 bits is refused.
        let big = [i32::MIN as i64 * 2, i32::MAX as i64 * 2];
        assert!(!fixed_residual(&big, 1, &mut Vec::new()));
    }

    #[test]
    fn lpc_residual_reconstructs() {
        let x: Vec<i64> = test_signal(500, 1, 20, 9)
            .iter()
            .map(|&s| s as i64)
            .collect();
        let coefs = [3000, -1500, 700, -20];
        let shift = 11;
        let mut res = Vec::new();
        assert!(lpc_residual(&x, &coefs, shift, &mut res));
        let mut y = x[..4].to_vec();
        for e in res {
            let i = y.len();
            let p: i64 = coefs
                .iter()
                .enumerate()
                .map(|(j, &c)| c as i64 * y[i - 1 - j])
                .sum();
            y.push((p >> shift) + e);
        }
        assert_eq!(y, x);
    }

    #[test]
    fn quantization_stays_in_range() {
        let mut rng = Lcg(10);
        for _ in 0..500 {
            let order = 1 + rng.next() as usize % 12;
            let magnitude = [0.001, 0.5, 1.0, 3.9, 100.0][rng.next() as usize % 5];
            let coefs: Vec<f64> = (0..order)
                .map(|_| (rng.next() as f64 / u32::MAX as f64 - 0.5) * 2.0 * magnitude)
                .collect();
            for precision in 5..=15 {
                let Some((q, shift)) = quantize(&coefs, precision) else {
                    continue;
                };
                assert!(shift <= 15);
                let limit = 1i32 << (precision - 1);
                assert!(q.iter().all(|&c| (-limit..limit).contains(&c)));
                // Quantized values approximate the originals.
                for (&c, &qc) in coefs.iter().zip(&q) {
                    let back = qc as f64 / (1u32 << shift) as f64;
                    assert!((back - c).abs() <= 2.0 / (1u32 << shift) as f64 + 1e-9);
                }
            }
        }
        assert_eq!(quantize(&[0.0, 0.0], 12), None);
        assert_eq!(quantize(&[f64::NAN], 12), None);
        // Too large to fit any shift of 15 bits.
        assert_eq!(quantize(&[1e6], 15), None);
        let (q, shift) = quantize(&[1.5, -0.75], 12).unwrap();
        assert_eq!((q, shift), (vec![1536, -768], 10));
    }

    #[test]
    fn levinson_durbin_finds_an_ar1_model() {
        // Autocorrelation of a first order process x[n] = 0.9 x[n-1] + e.
        let r: Vec<f64> = (0..5).map(|k| 0.9f64.powi(k)).collect();
        let models = levinson_durbin(&r);
        assert_eq!(models.len(), 4);
        assert!((models[0].0[0] - 0.9).abs() < 1e-12);
        for (coefs, err) in &models[1..] {
            assert!((coefs[0] - 0.9).abs() < 1e-9);
            assert!(coefs[1..].iter().all(|c| c.abs() < 1e-9));
            assert!((err - (1.0 - 0.81)).abs() < 1e-9);
        }
        // A perfectly predictable signal stops the recursion early.
        let models = levinson_durbin(&[1.0, 1.0, 1.0, 1.0]);
        assert_eq!(models.len(), 1);
    }

    #[test]
    fn lpc_beats_fixed_on_a_resonance() {
        // A decaying two pole resonance is predicted exactly by a second
        // order filter, which FIXED predictors cannot match.
        let mut x = vec![0i64; 4096];
        x[0] = 10_000;
        x[1] = 9_000;
        let (a1, a2) = (1.8, -0.97);
        let mut y = [9000.0f64, 10_000.0];
        for s in x.iter_mut().skip(2) {
            let v = a1 * y[0] + a2 * y[1] + 30.0 * ((y[0] as i64 % 7) as f64);
            y = [v, y[0]];
            *s = v.round() as i64;
        }
        let settings = Settings::for_level(5);
        let windows: Vec<Vec<f64>> = settings
            .windows
            .iter()
            .map(|w| w.coefficients(4096))
            .collect();
        let ctx = FrameContext {
            sample_rate: 44100,
            bits: 16,
            settings: &settings,
            windows: &windows,
        };
        let sub = best_subframe(&ctx, &x, 17);
        assert!(matches!(sub.kind, Kind::Lpc { .. }), "{:?}", sub.kind);
    }

    #[test]
    fn windows_have_the_expected_shape() {
        let w = Window::Tukey(0.5).coefficients(101);
        assert_eq!(w[0], 0.0);
        assert_eq!(w[100], 0.0);
        assert!(w[25..=75].iter().all(|&v| v == 1.0));
        for i in 0..101 {
            assert!((w[i] - w[100 - i]).abs() < 1e-12);
        }
        let hann = Window::Tukey(1.0).coefficients(5);
        assert!((hann[1] - 0.5).abs() < 1e-12 && (hann[2] - 1.0).abs() < 1e-12);
        let welch = Window::Welch.coefficients(5);
        assert_eq!(welch, vec![0.0, 0.75, 1.0, 0.75, 0.0]);
        assert_eq!(Window::Welch.coefficients(1), vec![1.0]);
        assert!(
            Window::Tukey(0.0)
                .coefficients(10)
                .iter()
                .all(|&v| v == 1.0)
        );
    }

    /// Builds the golden file: 3000 stereo 16-bit samples of the test
    /// signal at 44.1 kHz, level 2 (three frames of up to 1152 samples),
    /// tagged, through the real muxer.
    fn golden_file() -> (Vec<i32>, Vec<u8>) {
        use crate::format::Muxer;
        use crate::format::flac_mux::FlacMuxer;
        let samples = test_signal(3000, 2, 16, 42);
        let (packets, info) = encode_all(&samples, 2, 16, 44100, 2, 1024);
        let mut mux = FlacMuxer::new(
            crate::io::Output::memory(),
            vec![("title".to_string(), "golden".to_string())],
        );
        mux.write_header(std::slice::from_ref(&info)).unwrap();
        for p in &packets {
            mux.write_packet(p).unwrap();
        }
        mux.write_trailer(&[info]).unwrap();
        (samples, mux.into_output().into_bytes().unwrap())
    }

    /// The fixture was produced by this encoder and accepted by an
    /// independent decoder (CRCs and MD5 checked, samples compared), so
    /// reproducing it byte for byte ties the unit tests to that check.
    /// Any deliberate change to the encoder's choices must regenerate it
    /// (run this test with FLAC_FIXTURE_DIR pointing at the fixture
    /// directory) and validate it again with an independent decoder.
    #[test]
    fn reproduces_the_validated_fixture() {
        let fixture = include_bytes!("../../../tests/fixtures/flac-encoder/stereo16_l2.flac");
        let (samples, file) = golden_file();
        if let Ok(dir) = std::env::var("FLAC_FIXTURE_DIR") {
            std::fs::write(format!("{dir}/stereo16_l2.flac"), &file).unwrap();
            // The bytes above were embedded at compile time; the next
            // normal run rebuilds against the new file and checks it.
            return;
        }
        assert!(file == fixture.as_slice(), "encoder output changed");

        // Frames start after the marker and the three metadata blocks.
        let mut pos = 4;
        loop {
            let last = fixture[pos] & 0x80 != 0;
            let len = u32::from_be_bytes([0, fixture[pos + 1], fixture[pos + 2], fixture[pos + 3]]);
            pos += 4 + len as usize;
            if last {
                break;
            }
        }
        let mut decoded = Vec::new();
        let mut frames = 0;
        while pos < fixture.len() {
            let (f, len) = decode_frame_prefix(&fixture[pos..], 16);
            assert_eq!(f.number, frames);
            for i in 0..f.samples[0].len() {
                decoded.push(f.samples[0][i] as i32);
                decoded.push(f.samples[1][i] as i32);
            }
            frames += 1;
            pos += len;
        }
        assert_eq!(frames, 3);
        assert!(decoded == samples);
    }
}
