//! Picture size and pixel format conversion.

use crate::error::{Error, Result};
use crate::frame::{PixelFormat, VideoFrame};

/// Interpolation used when the size changes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScaleAlgorithm {
    Nearest,
    #[default]
    Bilinear,
    Bicubic,
    /// Box filter averaging every covered source pixel; best for large
    /// reductions.
    Area,
}

impl ScaleAlgorithm {
    pub fn from_name(name: &str) -> Result<Self> {
        Ok(match name {
            "neighbor" | "nearest" | "point" => ScaleAlgorithm::Nearest,
            "bilinear" | "fast_bilinear" => ScaleAlgorithm::Bilinear,
            "bicubic" => ScaleAlgorithm::Bicubic,
            "area" => ScaleAlgorithm::Area,
            _ => {
                return Err(Error::invalid_argument(format!(
                    "unknown scaling algorithm '{name}'"
                )));
            }
        })
    }
}

/// Returns `frame` resized to `width` x `height` and converted to `format`,
/// keeping its timestamp.
pub fn convert(
    frame: &VideoFrame,
    width: u32,
    height: u32,
    format: PixelFormat,
    _algorithm: ScaleAlgorithm,
) -> Result<VideoFrame> {
    if frame.width == width && frame.height == height && frame.format == format {
        return Ok(frame.clone());
    }
    Err(Error::unsupported(
        "video conversion is not implemented yet",
    ))
}
