//! Native FLAC streams: the `fLaC` marker, metadata blocks and frames.

use std::io::Read;

use crate::error::{Error, Result};
use crate::format::{Demuxer, InputFormat, InputOptions};

pub const INPUT: InputFormat = InputFormat {
    name: "flac",
    long_name: "raw FLAC",
    extensions: &["flac"],
    probe,
    open,
    raw_codec: None,
};

fn probe(data: &[u8]) -> u32 {
    if data.starts_with(b"fLaC") { 100 } else { 0 }
}

fn open(_reader: Box<dyn Read>, _options: &InputOptions) -> Result<Box<dyn Demuxer>> {
    Err(Error::unsupported("flac demuxer is not implemented yet"))
}
