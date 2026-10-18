//! Animated GIF files (GIF89a).

use crate::error::{Error, Result};
use crate::format::{Muxer, OutputFormat, OutputOptions};
use crate::frame::PixelFormat;
use crate::stream::CodecId;

pub const OUTPUT: OutputFormat = OutputFormat {
    name: "gif",
    long_name: "CompuServe Graphics Interchange Format (GIF)",
    extensions: &["gif"],
    audio_codec: None,
    video_codec: Some(CodecId::Gif),
    codecs: &[CodecId::Gif],
    pixel_formats: &[PixelFormat::Rgb24],
    create,
};

fn create(_path: &str, _options: &OutputOptions) -> Result<Box<dyn Muxer>> {
    Err(Error::unsupported("gif muxer is not implemented yet"))
}
