//! Uncompressed video: each packet is one picture, planes back to back in
//! the layout described by [`crate::frame::PixelFormat::plane_size`].

use crate::codec::{Decoder, Encoder, EncoderOptions};
use crate::error::{Error, Result};
use crate::frame::FrameFormat;
use crate::stream::{CodecId, StreamInfo};

pub fn new_decoder(_stream: &StreamInfo) -> Result<Box<dyn Decoder>> {
    Err(Error::unsupported(
        "rawvideo decoder is not implemented yet",
    ))
}

pub fn new_encoder(
    _codec: CodecId,
    _proposed: &FrameFormat,
    _options: &EncoderOptions,
) -> Result<Box<dyn Encoder>> {
    Err(Error::unsupported(
        "rawvideo encoder is not implemented yet",
    ))
}
