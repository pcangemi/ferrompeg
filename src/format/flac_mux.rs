//! Native FLAC stream writer.

use crate::error::{Error, Result};
use crate::format::{Muxer, OutputFormat, OutputOptions};
use crate::stream::CodecId;

pub const OUTPUT: OutputFormat = OutputFormat {
    name: "flac",
    long_name: "raw FLAC",
    extensions: &["flac"],
    audio_codec: Some(CodecId::Flac),
    video_codec: None,
    codecs: &[CodecId::Flac],
    pixel_formats: &[],
    create,
};

fn create(_path: &str, _options: &OutputOptions) -> Result<Box<dyn Muxer>> {
    Err(Error::unsupported("flac muxer is not implemented yet"))
}
