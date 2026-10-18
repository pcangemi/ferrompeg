//! Image sequences: one PPM, PGM or BMP file per frame (`out%03d.ppm`).

use crate::error::{Error, Result};
use crate::format::{Muxer, OutputFormat, OutputOptions};
use crate::frame::PixelFormat;
use crate::stream::CodecId;

pub const OUTPUT: OutputFormat = OutputFormat {
    name: "image2",
    long_name: "image file sequence",
    extensions: &["ppm", "pgm", "bmp"],
    audio_codec: None,
    video_codec: Some(CodecId::RawVideo),
    codecs: &[CodecId::RawVideo],
    pixel_formats: &[PixelFormat::Rgb24, PixelFormat::Gray],
    create,
};

fn create(_path: &str, _options: &OutputOptions) -> Result<Box<dyn Muxer>> {
    Err(Error::unsupported("image2 muxer is not implemented yet"))
}

/// True if `path` contains a printf style frame number such as `%d` or
/// `%03d`.
pub fn is_pattern(path: &str) -> bool {
    let bytes = path.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'd' {
                return true;
            }
            // "%%" is a literal percent sign.
            i = j.max(i + 2);
            continue;
        }
        i += 1;
    }
    false
}
