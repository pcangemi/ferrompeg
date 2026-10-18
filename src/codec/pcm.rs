//! Uncompressed PCM in its common byte layouts, plus G.711 A-law and mu-law.

use crate::codec::{Decoder, Encoder, EncoderOptions};
use crate::error::{Error, Result};
use crate::frame::{AudioData, AudioFormat, AudioFrame, Frame, FrameFormat, SampleKind};
use crate::rational::Rational;
use crate::stream::{AudioParams, CodecId, Packet, StreamInfo, StreamParams};

/// Number of bytes one sample takes in the encoded stream.
pub fn bytes_per_sample(codec: CodecId) -> usize {
    codec.pcm_bits().map_or(0, |b| b as usize / 8)
}

/// Sample kind a PCM codec decodes to and expects on encode.
pub fn sample_kind(codec: CodecId) -> SampleKind {
    match codec {
        CodecId::PcmF32Le | CodecId::PcmF32Be | CodecId::PcmF64Le | CodecId::PcmF64Be => {
            SampleKind::Float
        }
        // G.711 expands to 16-bit linear samples.
        CodecId::PcmAlaw | CodecId::PcmMulaw => SampleKind::Int(16),
        c => SampleKind::Int(c.pcm_bits().unwrap_or(16)),
    }
}

/// Picks the PCM codec that stores `kind` without loss, little-endian.
pub fn codec_for_kind(kind: SampleKind) -> CodecId {
    match kind {
        SampleKind::Int(bits) if bits <= 8 => CodecId::PcmU8,
        SampleKind::Int(bits) if bits <= 16 => CodecId::PcmS16Le,
        SampleKind::Int(bits) if bits <= 24 => CodecId::PcmS24Le,
        SampleKind::Int(_) => CodecId::PcmS32Le,
        SampleKind::Float => CodecId::PcmF32Le,
    }
}

/// Decodes raw bytes into interleaved samples. Trailing bytes that do not
/// form a whole sample are ignored.
pub fn decode_samples(codec: CodecId, bytes: &[u8]) -> Result<AudioData> {
    let n = bytes_per_sample(codec);
    if n == 0 {
        return Err(Error::unsupported(format!("{codec} is not a PCM codec")));
    }
    let chunks = bytes.chunks_exact(n);
    let ints = |f: fn(&[u8]) -> i32, bits| AudioData::Int {
        samples: bytes.chunks_exact(n).map(f).collect(),
        bits,
    };
    Ok(match codec {
        CodecId::PcmU8 => ints(|b| b[0] as i32 - 128, 8),
        CodecId::PcmS16Le => ints(|b| i16::from_le_bytes([b[0], b[1]]) as i32, 16),
        CodecId::PcmS16Be => ints(|b| i16::from_be_bytes([b[0], b[1]]) as i32, 16),
        // 24-bit: place the three bytes in the top of an i32, then shift
        // down arithmetically to sign extend.
        CodecId::PcmS24Le => ints(|b| i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8, 24),
        CodecId::PcmS24Be => ints(|b| i32::from_be_bytes([b[0], b[1], b[2], 0]) >> 8, 24),
        CodecId::PcmS32Le => ints(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]), 32),
        CodecId::PcmS32Be => ints(|b| i32::from_be_bytes([b[0], b[1], b[2], b[3]]), 32),
        CodecId::PcmF32Le => AudioData::Float(
            chunks
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect(),
        ),
        CodecId::PcmF32Be => AudioData::Float(
            chunks
                .map(|b| f32::from_be_bytes([b[0], b[1], b[2], b[3]]))
                .collect(),
        ),
        CodecId::PcmF64Le => AudioData::Float(
            chunks
                .map(|b| f64::from_le_bytes(b.try_into().unwrap()) as f32)
                .collect(),
        ),
        CodecId::PcmF64Be => AudioData::Float(
            chunks
                .map(|b| f64::from_be_bytes(b.try_into().unwrap()) as f32)
                .collect(),
        ),
        CodecId::PcmAlaw => ints(|b| alaw_to_linear(b[0]) as i32, 16),
        CodecId::PcmMulaw => ints(|b| mulaw_to_linear(b[0]) as i32, 16),
        _ => unreachable!("pcm_bits returned Some for a non PCM codec"),
    })
}

/// Encodes samples that are already in the codec's [`sample_kind`].
pub fn encode_samples(codec: CodecId, data: &AudioData) -> Result<Vec<u8>> {
    let expected = sample_kind(codec);
    if data.kind() != expected {
        return Err(Error::invalid_argument(format!(
            "{codec} expects {} samples, got {}",
            expected.name(),
            data.kind().name()
        )));
    }
    let mut out = Vec::with_capacity(data.len() * bytes_per_sample(codec));
    match data {
        AudioData::Int { samples, .. } => {
            for &s in samples {
                match codec {
                    CodecId::PcmU8 => out.push((s + 128) as u8),
                    CodecId::PcmS16Le => out.extend_from_slice(&(s as i16).to_le_bytes()),
                    CodecId::PcmS16Be => out.extend_from_slice(&(s as i16).to_be_bytes()),
                    CodecId::PcmS24Le => out.extend_from_slice(&s.to_le_bytes()[..3]),
                    CodecId::PcmS24Be => out.extend_from_slice(&s.to_be_bytes()[1..]),
                    CodecId::PcmS32Le => out.extend_from_slice(&s.to_le_bytes()),
                    CodecId::PcmS32Be => out.extend_from_slice(&s.to_be_bytes()),
                    CodecId::PcmAlaw => out.push(linear_to_alaw(s as i16)),
                    CodecId::PcmMulaw => out.push(linear_to_mulaw(s as i16)),
                    _ => unreachable!(),
                }
            }
        }
        AudioData::Float(samples) => {
            for &s in samples {
                match codec {
                    CodecId::PcmF32Le => out.extend_from_slice(&s.to_le_bytes()),
                    CodecId::PcmF32Be => out.extend_from_slice(&s.to_be_bytes()),
                    CodecId::PcmF64Le => out.extend_from_slice(&(s as f64).to_le_bytes()),
                    CodecId::PcmF64Be => out.extend_from_slice(&(s as f64).to_be_bytes()),
                    _ => unreachable!(),
                }
            }
        }
    }
    Ok(out)
}

// G.711 companding, written from the ITU-T G.711 segment tables.
// A-law inverts every other bit (mask 0x55); mu-law inverts all bits.

fn alaw_to_linear(a: u8) -> i16 {
    let a = a ^ 0x55;
    let exponent = (a >> 4) & 0x07;
    let mantissa = (a & 0x0F) as i16;
    let magnitude = match exponent {
        0 => (mantissa << 4) + 8,
        e => ((mantissa << 4) + 0x108) << (e - 1),
    };
    if a & 0x80 != 0 { magnitude } else { -magnitude }
}

fn linear_to_alaw(sample: i16) -> u8 {
    // Work on the 13-bit magnitude that A-law actually encodes.
    let (sign, mag) = if sample >= 0 {
        (0x80u8, (sample >> 3) as i32)
    } else {
        (0x00u8, (-(sample as i32) - 1) >> 3)
    };
    let mag = mag.min(0x0FFF);
    let code = if mag < 32 {
        (mag >> 1) as u8
    } else {
        let exponent = (31 - (mag as u32).leading_zeros()) as i32 - 4;
        let mantissa = (mag >> exponent) & 0x0F;
        ((exponent as u8) << 4) | mantissa as u8
    };
    (sign | code) ^ 0x55
}

fn mulaw_to_linear(u: u8) -> i16 {
    let u = !u;
    let exponent = (u >> 4) & 0x07;
    let mantissa = (u & 0x0F) as i32;
    let magnitude = (((mantissa << 3) + 0x84) << exponent) - 0x84;
    if u & 0x80 != 0 {
        -magnitude as i16
    } else {
        magnitude as i16
    }
}

fn linear_to_mulaw(sample: i16) -> u8 {
    const BIAS: i32 = 0x84;
    const CLIP: i32 = 32635;
    let (sign, mag) = if sample < 0 {
        (0x80u8, (-(sample as i32)).min(CLIP))
    } else {
        (0x00u8, (sample as i32).min(CLIP))
    };
    let biased = mag + BIAS;
    let exponent = (31 - (biased as u32).leading_zeros()) as i32 - 7;
    let mantissa = (biased >> (exponent + 3)) & 0x0F;
    !(sign | ((exponent as u8) << 4) | mantissa as u8)
}

struct PcmDecoder {
    codec: CodecId,
    sample_rate: u32,
    channels: u16,
}

pub fn new_decoder(stream: &StreamInfo) -> Result<Box<dyn Decoder>> {
    let a = stream
        .audio()
        .ok_or_else(|| Error::invalid_argument("PCM decoder needs an audio stream"))?;
    if a.channels == 0 || a.sample_rate == 0 {
        return Err(Error::invalid_data(
            "PCM stream without channels or sample rate",
        ));
    }
    Ok(Box::new(PcmDecoder {
        codec: stream.codec,
        sample_rate: a.sample_rate,
        channels: a.channels,
    }))
}

impl Decoder for PcmDecoder {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<Frame>> {
        let mut data = decode_samples(self.codec, &packet.data)?;
        // Drop a partial sample frame at the end of a truncated stream.
        let ch = self.channels as usize;
        let whole = data.len() / ch * ch;
        match &mut data {
            AudioData::Int { samples, .. } => samples.truncate(whole),
            AudioData::Float(samples) => samples.truncate(whole),
        }
        if data.is_empty() {
            return Ok(Vec::new());
        }
        Ok(vec![Frame::Audio(AudioFrame::new(
            self.sample_rate,
            self.channels,
            data,
            packet.pts,
        ))])
    }
}

struct PcmEncoder {
    codec: CodecId,
    format: AudioFormat,
    stream_index: usize,
}

pub fn new_encoder(
    codec: CodecId,
    proposed: &FrameFormat,
    options: &EncoderOptions,
) -> Result<Box<dyn Encoder>> {
    let FrameFormat::Audio(proposed) = proposed else {
        return Err(Error::invalid_argument(format!(
            "{codec} encodes audio only"
        )));
    };
    if codec.pcm_bits().is_none() {
        return Err(Error::invalid_argument(format!(
            "{codec} is not a PCM codec"
        )));
    }
    Ok(Box::new(PcmEncoder {
        codec,
        format: AudioFormat {
            kind: sample_kind(codec),
            ..*proposed
        },
        stream_index: options.stream_index,
    }))
}

impl Encoder for PcmEncoder {
    fn input_format(&self) -> FrameFormat {
        FrameFormat::Audio(self.format)
    }

    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        let Frame::Audio(frame) = frame else {
            return Err(Error::invalid_argument("PCM encoder got a video frame"));
        };
        let bytes = encode_samples(self.codec, &frame.data)?;
        Ok(vec![Packet::new(
            self.stream_index,
            bytes,
            frame.pts,
            frame.nb_samples() as i64,
        )])
    }

    fn stream_info(&self) -> StreamInfo {
        let bits = self.codec.pcm_bits().unwrap_or(0);
        let mut info = StreamInfo::new(
            self.stream_index,
            self.codec,
            StreamParams::Audio(AudioParams {
                sample_rate: self.format.sample_rate,
                channels: self.format.channels,
                bits_per_sample: bits,
            }),
            Rational::new(1, self.format.sample_rate as i64),
        );
        info.bit_rate =
            Some(self.format.sample_rate as u64 * self.format.channels as u64 * bits as u64);
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_layouts_round_trip() {
        let cases = [
            (CodecId::PcmU8, vec![-128, -1, 0, 127]),
            (CodecId::PcmS16Le, vec![-32768, -1, 0, 32767]),
            (CodecId::PcmS16Be, vec![-32768, -1, 0, 32767]),
            (CodecId::PcmS24Le, vec![-8388608, -1, 0, 8388607]),
            (CodecId::PcmS24Be, vec![-8388608, -1, 0, 8388607]),
            (CodecId::PcmS32Le, vec![i32::MIN, -1, 0, i32::MAX]),
            (CodecId::PcmS32Be, vec![i32::MIN, -1, 0, i32::MAX]),
        ];
        for (codec, samples) in cases {
            let bits = codec.pcm_bits().unwrap();
            let data = AudioData::Int { samples, bits };
            let bytes = encode_samples(codec, &data).unwrap();
            assert_eq!(bytes.len(), 4 * bytes_per_sample(codec));
            assert_eq!(decode_samples(codec, &bytes).unwrap(), data, "{codec}");
        }
    }

    #[test]
    fn byte_order_is_right() {
        let data = AudioData::Int {
            samples: vec![0x123456],
            bits: 24,
        };
        assert_eq!(
            encode_samples(CodecId::PcmS24Le, &data).unwrap(),
            [0x56, 0x34, 0x12]
        );
        assert_eq!(
            encode_samples(CodecId::PcmS24Be, &data).unwrap(),
            [0x12, 0x34, 0x56]
        );
        assert_eq!(decode_samples(CodecId::PcmU8, &[0x80]).unwrap().len(), 1);
    }

    #[test]
    fn floats_round_trip() {
        let data = AudioData::Float(vec![-1.0, -0.25, 0.0, 0.5, 1.0]);
        for codec in [
            CodecId::PcmF32Le,
            CodecId::PcmF32Be,
            CodecId::PcmF64Le,
            CodecId::PcmF64Be,
        ] {
            let bytes = encode_samples(codec, &data).unwrap();
            assert_eq!(decode_samples(codec, &bytes).unwrap(), data);
        }
    }

    #[test]
    fn g711_is_stable_after_one_pass() {
        // Companding is lossy, but decoding a code and encoding the result
        // must give the same code back for every one of the 256 values.
        for code in 0..=255u8 {
            assert_eq!(linear_to_alaw(alaw_to_linear(code)), code, "alaw {code}");
            let m = linear_to_mulaw(mulaw_to_linear(code));
            // 0x7F and 0xFF both mean zero in mu-law.
            if mulaw_to_linear(code) != 0 {
                assert_eq!(m, code, "mulaw {code}");
            }
        }
    }

    #[test]
    fn g711_reference_points() {
        assert_eq!(alaw_to_linear(0xD5), 8);
        assert_eq!(alaw_to_linear(0x55), -8);
        assert_eq!(alaw_to_linear(0xAA), 32256);
        assert_eq!(mulaw_to_linear(0xFF), 0);
        assert_eq!(mulaw_to_linear(0x80), 32124);
        assert_eq!(mulaw_to_linear(0x00), -32124);
        assert!(linear_to_mulaw(32767) == 0x80);
    }

    #[test]
    fn decoder_drops_partial_frames() {
        let mut info = StreamInfo::new(
            0,
            CodecId::PcmS16Le,
            StreamParams::Audio(AudioParams {
                sample_rate: 8000,
                channels: 2,
                bits_per_sample: 16,
            }),
            Rational::new(1, 8000),
        );
        info.duration = None;
        let mut dec = new_decoder(&info).unwrap();
        let frames = dec.decode(&Packet::new(0, vec![0; 6], 0, 1)).unwrap();
        let Frame::Audio(a) = &frames[0] else {
            panic!()
        };
        assert_eq!(a.nb_samples(), 1);
    }
}
