//! Decoded media: audio and video frames and their formats.
//!
//! Two conventions hold everywhere in the crate and keep timestamp math
//! trivial:
//!
//! * audio frames use a time base of `1 / sample_rate`, so `pts` counts
//!   samples per channel;
//! * video is constant frame rate: the time base is exactly one frame
//!   duration, so `pts` counts frames and the frame rate is `1 / time_base`.

use std::fmt;

use crate::error::{Error, Result};
use crate::rational::Rational;

/// How audio samples are stored in memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SampleKind {
    /// Signed integers holding `bits` significant bits (8 to 32), sign
    /// extended into an `i32`. Unsigned 8-bit PCM is recentered on decode.
    Int(u32),
    /// 32-bit floats with a nominal range of -1.0 to 1.0.
    Float,
}

impl SampleKind {
    /// Short name in the style of the usual sample format names.
    pub fn name(self) -> String {
        match self {
            SampleKind::Int(8) => "s8".into(),
            SampleKind::Int(16) => "s16".into(),
            SampleKind::Int(32) => "s32".into(),
            SampleKind::Int(bits) => format!("s32 ({bits} bit)"),
            SampleKind::Float => "flt".into(),
        }
    }
}

/// Audio sample storage. Samples are interleaved: frame by frame, then
/// channel by channel within a frame.
#[derive(Clone, Debug, PartialEq)]
pub enum AudioData {
    Int { samples: Vec<i32>, bits: u32 },
    Float(Vec<f32>),
}

impl AudioData {
    pub fn len(&self) -> usize {
        match self {
            AudioData::Int { samples, .. } => samples.len(),
            AudioData::Float(samples) => samples.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn kind(&self) -> SampleKind {
        match self {
            AudioData::Int { bits, .. } => SampleKind::Int(*bits),
            AudioData::Float(_) => SampleKind::Float,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub kind: SampleKind,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioFrame {
    pub sample_rate: u32,
    pub channels: u16,
    pub data: AudioData,
    /// Position of the first sample, counted in samples per channel.
    pub pts: i64,
}

impl AudioFrame {
    pub fn new(sample_rate: u32, channels: u16, data: AudioData, pts: i64) -> Self {
        debug_assert!(channels > 0);
        debug_assert_eq!(data.len() % channels as usize, 0);
        AudioFrame {
            sample_rate,
            channels,
            data,
            pts,
        }
    }

    /// Number of samples per channel.
    pub fn nb_samples(&self) -> usize {
        self.data.len() / self.channels.max(1) as usize
    }

    pub fn time_base(&self) -> Rational {
        Rational::new(1, self.sample_rate as i64)
    }

    pub fn format(&self) -> AudioFormat {
        AudioFormat {
            sample_rate: self.sample_rate,
            channels: self.channels,
            kind: self.data.kind(),
        }
    }

    /// Returns the samples as floats in -1.0..1.0, converting if needed.
    pub fn to_f32(&self) -> Vec<f32> {
        match &self.data {
            AudioData::Float(s) => s.clone(),
            AudioData::Int { samples, bits } => {
                let scale = 1.0 / (1u64 << (bits - 1)) as f64;
                samples.iter().map(|&s| (s as f64 * scale) as f32).collect()
            }
        }
    }
}

/// Pixel layouts. All formats are 8 bits per component.
///
/// YUV formats are planar, BT.601, limited range (16 to 235 for luma), which
/// is what YUV4MPEG streams and most tools assume when nothing says otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// Chroma halved in both directions.
    Yuv420p,
    /// Chroma halved horizontally.
    Yuv422p,
    /// Full resolution chroma.
    Yuv444p,
    /// Single full range luma plane.
    Gray,
    /// Packed R, G, B bytes, full range.
    Rgb24,
}

impl PixelFormat {
    pub const ALL: [PixelFormat; 5] = [
        PixelFormat::Yuv420p,
        PixelFormat::Yuv422p,
        PixelFormat::Yuv444p,
        PixelFormat::Gray,
        PixelFormat::Rgb24,
    ];

    pub fn name(self) -> &'static str {
        match self {
            PixelFormat::Yuv420p => "yuv420p",
            PixelFormat::Yuv422p => "yuv422p",
            PixelFormat::Yuv444p => "yuv444p",
            PixelFormat::Gray => "gray",
            PixelFormat::Rgb24 => "rgb24",
        }
    }

    pub fn from_name(name: &str) -> Result<Self> {
        PixelFormat::ALL
            .into_iter()
            .find(|f| f.name() == name)
            .ok_or_else(|| Error::invalid_argument(format!("unknown pixel format '{name}'")))
    }

    pub fn is_yuv(self) -> bool {
        matches!(
            self,
            PixelFormat::Yuv420p | PixelFormat::Yuv422p | PixelFormat::Yuv444p
        )
    }

    pub fn plane_count(self) -> usize {
        match self {
            PixelFormat::Gray | PixelFormat::Rgb24 => 1,
            _ => 3,
        }
    }

    /// Horizontal and vertical chroma subsampling as shifts (0 or 1).
    pub fn chroma_shift(self) -> (u32, u32) {
        match self {
            PixelFormat::Yuv420p => (1, 1),
            PixelFormat::Yuv422p => (1, 0),
            _ => (0, 0),
        }
    }

    /// Bytes per row and number of rows of `plane` for a picture of the
    /// given size. Planes are tightly packed: the stride is the row width.
    /// Odd sizes round chroma up so no picture column is left without one.
    pub fn plane_size(self, plane: usize, width: u32, height: u32) -> (usize, usize) {
        let (w, h) = (width as usize, height as usize);
        match (self, plane) {
            (PixelFormat::Rgb24, 0) => (w * 3, h),
            (_, 0) => (w, h),
            _ => {
                let (sx, sy) = self.chroma_shift();
                (w.div_ceil(1 << sx), h.div_ceil(1 << sy))
            }
        }
    }

    /// Total size in bytes of a picture in this format.
    pub fn frame_size(self, width: u32, height: u32) -> usize {
        (0..self.plane_count())
            .map(|p| {
                let (row, rows) = self.plane_size(p, width, height);
                row * rows
            })
            .sum()
    }
}

impl fmt::Display for PixelFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VideoFormat {
    pub width: u32,
    pub height: u32,
    pub pixel_format: PixelFormat,
    pub frame_rate: Rational,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    /// One buffer per plane, each `plane_size` bytes, rows tightly packed.
    pub planes: Vec<Vec<u8>>,
    /// Frame index: the time base is one frame duration.
    pub pts: i64,
    pub time_base: Rational,
}

impl VideoFrame {
    /// Allocates a black picture.
    pub fn new_black(
        width: u32,
        height: u32,
        format: PixelFormat,
        pts: i64,
        time_base: Rational,
    ) -> Self {
        let planes = (0..format.plane_count())
            .map(|p| {
                let (row, rows) = format.plane_size(p, width, height);
                let fill = match (format.is_yuv(), p) {
                    (true, 0) => 16,
                    (true, _) => 128,
                    _ => 0,
                };
                vec![fill; row * rows]
            })
            .collect();
        VideoFrame {
            width,
            height,
            format,
            planes,
            pts,
            time_base,
        }
    }

    pub fn video_format(&self) -> VideoFormat {
        VideoFormat {
            width: self.width,
            height: self.height,
            pixel_format: self.format,
            frame_rate: self.time_base.inv(),
        }
    }

    /// Checks that the planes match the declared size and format.
    pub fn validate(&self) -> Result<()> {
        if self.planes.len() != self.format.plane_count() {
            return Err(Error::invalid_data(
                "wrong number of planes for the pixel format",
            ));
        }
        for (p, plane) in self.planes.iter().enumerate() {
            let (row, rows) = self.format.plane_size(p, self.width, self.height);
            if plane.len() != row * rows {
                return Err(Error::invalid_data(format!(
                    "plane {p} holds {} bytes, expected {}",
                    plane.len(),
                    row * rows
                )));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Frame {
    Audio(AudioFrame),
    Video(VideoFrame),
}

impl Frame {
    pub fn format(&self) -> FrameFormat {
        match self {
            Frame::Audio(a) => FrameFormat::Audio(a.format()),
            Frame::Video(v) => FrameFormat::Video(v.video_format()),
        }
    }

    /// Presentation time of the frame in seconds.
    pub fn time_seconds(&self) -> f64 {
        match self {
            Frame::Audio(a) => a.pts as f64 / a.sample_rate as f64,
            Frame::Video(v) => v.pts as f64 * v.time_base.to_f64(),
        }
    }

    /// Time just after the last sample or at the end of the picture's display.
    pub fn end_seconds(&self) -> f64 {
        match self {
            Frame::Audio(a) => (a.pts + a.nb_samples() as i64) as f64 / a.sample_rate as f64,
            Frame::Video(v) => (v.pts + 1) as f64 * v.time_base.to_f64(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FrameFormat {
    Audio(AudioFormat),
    Video(VideoFormat),
}

/// Conventional name of a channel count, as players display it.
pub fn channel_layout_name(channels: u16) -> String {
    match channels {
        1 => "mono".into(),
        2 => "stereo".into(),
        3 => "2.1".into(),
        4 => "quad".into(),
        5 => "5.0".into(),
        6 => "5.1".into(),
        8 => "7.1".into(),
        n => format!("{n} channels"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plane_sizes_round_chroma_up() {
        let f = PixelFormat::Yuv420p;
        assert_eq!(f.plane_size(0, 5, 3), (5, 3));
        assert_eq!(f.plane_size(1, 5, 3), (3, 2));
        assert_eq!(f.frame_size(4, 4), 16 + 4 + 4);
        assert_eq!(PixelFormat::Yuv422p.plane_size(2, 5, 3), (3, 3));
        assert_eq!(PixelFormat::Rgb24.frame_size(2, 2), 12);
    }

    #[test]
    fn int_samples_convert_to_float_range() {
        let frame = AudioFrame::new(
            8000,
            1,
            AudioData::Int {
                samples: vec![-32768, 0, 16384],
                bits: 16,
            },
            0,
        );
        assert_eq!(frame.to_f32(), vec![-1.0, 0.0, 0.5]);
    }

    #[test]
    fn black_frame_is_valid() {
        for f in PixelFormat::ALL {
            let frame = VideoFrame::new_black(7, 5, f, 0, Rational::new(1, 25));
            frame.validate().unwrap();
        }
    }
}
