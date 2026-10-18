//! FLAC encoder (RFC 9639).

use crate::codec::{Encoder, EncoderOptions};
use crate::error::{Error, Result};
use crate::frame::FrameFormat;
use crate::stream::CodecId;

pub fn new_encoder(
    _codec: CodecId,
    _proposed: &FrameFormat,
    _options: &EncoderOptions,
) -> Result<Box<dyn Encoder>> {
    Err(Error::unsupported("flac encoder is not implemented yet"))
}
