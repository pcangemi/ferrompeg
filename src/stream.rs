//! Stream descriptions and compressed packets.

use std::fmt;

use crate::error::{Error, Result};
use crate::frame::{PixelFormat, channel_layout_name};
use crate::rational::Rational;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MediaType {
    Audio,
    Video,
}

impl fmt::Display for MediaType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            MediaType::Audio => "Audio",
            MediaType::Video => "Video",
        })
    }
}

/// Every codec the crate knows about, whether or not it can encode it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CodecId {
    PcmU8,
    PcmS8,
    PcmS16Le,
    PcmS16Be,
    PcmS24Le,
    PcmS24Be,
    PcmS32Le,
    PcmS32Be,
    PcmF32Le,
    PcmF32Be,
    PcmF64Le,
    PcmF64Be,
    PcmAlaw,
    PcmMulaw,
    RawVideo,
    Flac,
    Gif,
}

impl CodecId {
    pub const ALL: [CodecId; 17] = [
        CodecId::PcmU8,
        CodecId::PcmS8,
        CodecId::PcmS16Le,
        CodecId::PcmS16Be,
        CodecId::PcmS24Le,
        CodecId::PcmS24Be,
        CodecId::PcmS32Le,
        CodecId::PcmS32Be,
        CodecId::PcmF32Le,
        CodecId::PcmF32Be,
        CodecId::PcmF64Le,
        CodecId::PcmF64Be,
        CodecId::PcmAlaw,
        CodecId::PcmMulaw,
        CodecId::RawVideo,
        CodecId::Flac,
        CodecId::Gif,
    ];

    pub fn name(self) -> &'static str {
        match self {
            CodecId::PcmU8 => "pcm_u8",
            CodecId::PcmS8 => "pcm_s8",
            CodecId::PcmS16Le => "pcm_s16le",
            CodecId::PcmS16Be => "pcm_s16be",
            CodecId::PcmS24Le => "pcm_s24le",
            CodecId::PcmS24Be => "pcm_s24be",
            CodecId::PcmS32Le => "pcm_s32le",
            CodecId::PcmS32Be => "pcm_s32be",
            CodecId::PcmF32Le => "pcm_f32le",
            CodecId::PcmF32Be => "pcm_f32be",
            CodecId::PcmF64Le => "pcm_f64le",
            CodecId::PcmF64Be => "pcm_f64be",
            CodecId::PcmAlaw => "pcm_alaw",
            CodecId::PcmMulaw => "pcm_mulaw",
            CodecId::RawVideo => "rawvideo",
            CodecId::Flac => "flac",
            CodecId::Gif => "gif",
        }
    }

    pub fn long_name(self) -> &'static str {
        match self {
            CodecId::PcmU8 => "PCM unsigned 8-bit",
            CodecId::PcmS8 => "PCM signed 8-bit",
            CodecId::PcmS16Le => "PCM signed 16-bit little-endian",
            CodecId::PcmS16Be => "PCM signed 16-bit big-endian",
            CodecId::PcmS24Le => "PCM signed 24-bit little-endian",
            CodecId::PcmS24Be => "PCM signed 24-bit big-endian",
            CodecId::PcmS32Le => "PCM signed 32-bit little-endian",
            CodecId::PcmS32Be => "PCM signed 32-bit big-endian",
            CodecId::PcmF32Le => "PCM 32-bit floating point little-endian",
            CodecId::PcmF32Be => "PCM 32-bit floating point big-endian",
            CodecId::PcmF64Le => "PCM 64-bit floating point little-endian",
            CodecId::PcmF64Be => "PCM 64-bit floating point big-endian",
            CodecId::PcmAlaw => "PCM A-law (G.711)",
            CodecId::PcmMulaw => "PCM mu-law (G.711)",
            CodecId::RawVideo => "raw video",
            CodecId::Flac => "FLAC (Free Lossless Audio Codec)",
            CodecId::Gif => "GIF (Graphics Interchange Format)",
        }
    }

    pub fn from_name(name: &str) -> Result<Self> {
        CodecId::ALL
            .into_iter()
            .find(|c| c.name() == name)
            .ok_or_else(|| Error::invalid_argument(format!("unknown codec '{name}'")))
    }

    pub fn media_type(self) -> MediaType {
        match self {
            CodecId::RawVideo | CodecId::Gif => MediaType::Video,
            _ => MediaType::Audio,
        }
    }

    /// Bits per sample of a PCM codec, `None` for anything else.
    pub fn pcm_bits(self) -> Option<u32> {
        Some(match self {
            CodecId::PcmU8 | CodecId::PcmS8 | CodecId::PcmAlaw | CodecId::PcmMulaw => 8,
            CodecId::PcmS16Le | CodecId::PcmS16Be => 16,
            CodecId::PcmS24Le | CodecId::PcmS24Be => 24,
            CodecId::PcmS32Le | CodecId::PcmS32Be | CodecId::PcmF32Le | CodecId::PcmF32Be => 32,
            CodecId::PcmF64Le | CodecId::PcmF64Be => 64,
            _ => return None,
        })
    }

    /// Whether a codec is lossless for its input, which decides whether the
    /// pipeline may skip dithering when reducing bit depth.
    pub fn is_lossless(self) -> bool {
        !matches!(self, CodecId::PcmAlaw | CodecId::PcmMulaw | CodecId::Gif)
    }
}

impl fmt::Display for CodecId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioParams {
    pub sample_rate: u32,
    pub channels: u16,
    /// Bits per decoded sample (for FLAC this is the stream bit depth).
    pub bits_per_sample: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoParams {
    pub width: u32,
    pub height: u32,
    pub pixel_format: PixelFormat,
    pub frame_rate: Rational,
    pub sample_aspect_ratio: Rational,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamParams {
    Audio(AudioParams),
    Video(VideoParams),
}

#[derive(Clone, Debug, PartialEq)]
pub struct StreamInfo {
    pub index: usize,
    pub codec: CodecId,
    pub params: StreamParams,
    /// Unit of packet timestamps: `1 / sample_rate` for audio, one frame
    /// duration for video.
    pub time_base: Rational,
    /// Total length in `time_base` units, when the container says.
    pub duration: Option<i64>,
    /// Codec specific setup bytes. For FLAC this is the 34-byte STREAMINFO
    /// block; encoders refresh it after `flush` with the final values.
    pub extradata: Vec<u8>,
    /// Average bit rate in bits per second, when known.
    pub bit_rate: Option<u64>,
    pub metadata: Vec<(String, String)>,
}

impl StreamInfo {
    pub fn new(index: usize, codec: CodecId, params: StreamParams, time_base: Rational) -> Self {
        StreamInfo {
            index,
            codec,
            params,
            time_base,
            duration: None,
            extradata: Vec::new(),
            bit_rate: None,
            metadata: Vec::new(),
        }
    }

    pub fn media_type(&self) -> MediaType {
        match self.params {
            StreamParams::Audio(_) => MediaType::Audio,
            StreamParams::Video(_) => MediaType::Video,
        }
    }

    pub fn audio(&self) -> Option<&AudioParams> {
        match &self.params {
            StreamParams::Audio(a) => Some(a),
            StreamParams::Video(_) => None,
        }
    }

    pub fn video(&self) -> Option<&VideoParams> {
        match &self.params {
            StreamParams::Video(v) => Some(v),
            StreamParams::Audio(_) => None,
        }
    }

    pub fn duration_seconds(&self) -> Option<f64> {
        self.duration.map(|d| d as f64 * self.time_base.to_f64())
    }

    /// One line description in the style of the usual stream listings, for
    /// example "Audio: pcm_s16le, 44100 Hz, stereo, s16, 1411 kb/s".
    pub fn describe(&self) -> String {
        let mut s = format!("{}: {}", self.media_type(), self.codec.name());
        match &self.params {
            StreamParams::Audio(a) => {
                s += &format!(
                    ", {} Hz, {}",
                    a.sample_rate,
                    channel_layout_name(a.channels)
                );
                if a.bits_per_sample > 0 {
                    s += &format!(", {} bit", a.bits_per_sample);
                }
            }
            StreamParams::Video(v) => {
                s += &format!(", {}, {}x{}", v.pixel_format, v.width, v.height);
                let sar = v.sample_aspect_ratio;
                if sar.num > 0 && sar != Rational::new(1, 1) {
                    s += &format!(" [SAR {}:{}]", sar.num, sar.den);
                }
                s += &format!(", {} fps", format_rate(v.frame_rate));
            }
        }
        if let Some(rate) = self.bit_rate {
            s += &format!(", {} kb/s", (rate + 500) / 1000);
        }
        s
    }
}

/// Formats a frame rate the way people write it: "25", "29.97".
pub fn format_rate(rate: Rational) -> String {
    let v = rate.to_f64();
    if rate.den == 1 {
        format!("{}", rate.num)
    } else {
        let s = format!("{v:.2}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// A unit of compressed data for one stream.
#[derive(Clone, Debug, PartialEq)]
pub struct Packet {
    pub stream_index: usize,
    pub data: Vec<u8>,
    /// Presentation time of the first sample or of the picture, in the
    /// stream time base.
    pub pts: i64,
    /// Duration in the stream time base (samples for audio, frames for video).
    pub duration: i64,
    pub keyframe: bool,
}

impl Packet {
    pub fn new(stream_index: usize, data: Vec<u8>, pts: i64, duration: i64) -> Self {
        Packet {
            stream_index,
            data,
            pts,
            duration,
            keyframe: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_names_round_trip() {
        for c in CodecId::ALL {
            assert_eq!(CodecId::from_name(c.name()).unwrap(), c);
        }
    }

    #[test]
    fn rate_formatting() {
        assert_eq!(format_rate(Rational::new(25, 1)), "25");
        assert_eq!(format_rate(Rational::new(30000, 1001)), "29.97");
        assert_eq!(format_rate(Rational::new(24000, 1001)), "23.98");
        assert_eq!(format_rate(Rational::new(25, 2)), "12.5");
    }
}
