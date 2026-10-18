//! FLAC decoder (RFC 9639).

use crate::codec::Decoder;
use crate::error::{Error, Result};
use crate::stream::StreamInfo;

pub fn new_decoder(_stream: &StreamInfo) -> Result<Box<dyn Decoder>> {
    Err(Error::unsupported("flac decoder is not implemented yet"))
}
