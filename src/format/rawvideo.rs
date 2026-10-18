//! Headerless raw video (`-f rawvideo -s WxH -pix_fmt fmt -r rate`).

use std::io::Read;

use crate::error::{Error, Result};
use crate::format::{
    Demuxer, InputFormat, InputOptions, Muxer, OutputFormat, OutputOptions, probe_never,
};
use crate::stream::CodecId;

pub const INPUT: InputFormat = InputFormat {
    name: "rawvideo",
    long_name: "raw video",
    extensions: &["yuv", "rgb"],
    probe: probe_never,
    open,
    raw_codec: None,
};

fn open(_reader: Box<dyn Read>, _options: &InputOptions) -> Result<Box<dyn Demuxer>> {
    Err(Error::unsupported(
        "rawvideo demuxer is not implemented yet",
    ))
}

pub const OUTPUT: OutputFormat = OutputFormat {
    name: "rawvideo",
    long_name: "raw video",
    extensions: &["yuv", "rgb"],
    audio_codec: None,
    video_codec: Some(CodecId::RawVideo),
    codecs: &[CodecId::RawVideo],
    pixel_formats: &[],
    create,
};

fn create(_path: &str, _options: &OutputOptions) -> Result<Box<dyn Muxer>> {
    Err(Error::unsupported("rawvideo muxer is not implemented yet"))
}
