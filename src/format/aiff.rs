//! AIFF and AIFF-C (big-endian PCM).

use std::io::Read;

use crate::error::{Error, Result};
use crate::format::{Demuxer, InputFormat, InputOptions, Muxer, OutputFormat, OutputOptions};
use crate::stream::CodecId;

pub const INPUT: InputFormat = InputFormat {
    name: "aiff",
    long_name: "Audio IFF",
    extensions: &["aiff", "aif", "aifc"],
    probe,
    open,
    raw_codec: None,
};

fn probe(data: &[u8]) -> u32 {
    if data.len() >= 12
        && &data[0..4] == b"FORM"
        && (&data[8..12] == b"AIFF" || &data[8..12] == b"AIFC")
    {
        100
    } else {
        0
    }
}

fn open(_reader: Box<dyn Read>, _options: &InputOptions) -> Result<Box<dyn Demuxer>> {
    Err(Error::unsupported("aiff demuxer is not implemented yet"))
}

pub const OUTPUT: OutputFormat = OutputFormat {
    name: "aiff",
    long_name: "Audio IFF",
    extensions: &["aiff", "aif"],
    audio_codec: Some(CodecId::PcmS16Be),
    video_codec: None,
    codecs: &[
        CodecId::PcmU8,
        CodecId::PcmS16Be,
        CodecId::PcmS24Be,
        CodecId::PcmS32Be,
    ],
    pixel_formats: &[],
    create,
};

fn create(_path: &str, _options: &OutputOptions) -> Result<Box<dyn Muxer>> {
    Err(Error::unsupported("aiff muxer is not implemented yet"))
}
