//! GIF encoder: palette quantization and LZW compression (GIF89a).

use crate::codec::{Encoder, EncoderOptions};
use crate::error::{Error, Result};
use crate::frame::FrameFormat;
use crate::stream::CodecId;

pub fn new_encoder(
    _codec: CodecId,
    _proposed: &FrameFormat,
    _options: &EncoderOptions,
) -> Result<Box<dyn Encoder>> {
    Err(Error::unsupported("gif encoder is not implemented yet"))
}
