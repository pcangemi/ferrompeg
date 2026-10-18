//! Sun AU / NeXT sound files.

use std::io::Read;

use crate::error::{Error, Result};
use crate::format::{Demuxer, InputFormat, InputOptions, Muxer, OutputFormat, OutputOptions};
use crate::stream::CodecId;

pub const INPUT: InputFormat = InputFormat {
    name: "au",
    long_name: "Sun AU",
    extensions: &["au", "snd"],
    probe,
    open,
    raw_codec: None,
};

fn probe(data: &[u8]) -> u32 {
    if data.starts_with(b".snd") { 100 } else { 0 }
}

fn open(_reader: Box<dyn Read>, _options: &InputOptions) -> Result<Box<dyn Demuxer>> {
    Err(Error::unsupported("au demuxer is not implemented yet"))
}

pub const OUTPUT: OutputFormat = OutputFormat {
    name: "au",
    long_name: "Sun AU",
    extensions: &["au"],
    audio_codec: Some(CodecId::PcmS16Be),
    video_codec: None,
    codecs: &[
        CodecId::PcmS16Be,
        CodecId::PcmS24Be,
        CodecId::PcmS32Be,
        CodecId::PcmF32Be,
        CodecId::PcmF64Be,
        CodecId::PcmAlaw,
        CodecId::PcmMulaw,
    ],
    pixel_formats: &[],
    create,
};

fn create(_path: &str, _options: &OutputOptions) -> Result<Box<dyn Muxer>> {
    Err(Error::unsupported("au muxer is not implemented yet"))
}
