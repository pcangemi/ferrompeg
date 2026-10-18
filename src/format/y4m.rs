//! YUV4MPEG2 streams (`.y4m`).

use std::io::Read;

use crate::error::{Error, Result};
use crate::format::{Demuxer, InputFormat, InputOptions, Muxer, OutputFormat, OutputOptions};
use crate::frame::PixelFormat;
use crate::stream::CodecId;

pub const INPUT: InputFormat = InputFormat {
    name: "yuv4mpegpipe",
    long_name: "YUV4MPEG pipe",
    extensions: &["y4m"],
    probe,
    open,
    raw_codec: None,
};

fn probe(data: &[u8]) -> u32 {
    if data.starts_with(b"YUV4MPEG2 ") {
        100
    } else {
        0
    }
}

fn open(_reader: Box<dyn Read>, _options: &InputOptions) -> Result<Box<dyn Demuxer>> {
    Err(Error::unsupported(
        "yuv4mpegpipe demuxer is not implemented yet",
    ))
}

pub const OUTPUT: OutputFormat = OutputFormat {
    name: "yuv4mpegpipe",
    long_name: "YUV4MPEG pipe",
    extensions: &["y4m"],
    audio_codec: None,
    video_codec: Some(CodecId::RawVideo),
    codecs: &[CodecId::RawVideo],
    pixel_formats: &[
        PixelFormat::Yuv420p,
        PixelFormat::Yuv422p,
        PixelFormat::Yuv444p,
        PixelFormat::Gray,
    ],
    create,
};

fn create(_path: &str, _options: &OutputOptions) -> Result<Box<dyn Muxer>> {
    Err(Error::unsupported(
        "yuv4mpegpipe muxer is not implemented yet",
    ))
}
