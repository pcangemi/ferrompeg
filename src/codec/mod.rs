//! Decoders turn packets into frames, encoders turn frames into packets.

pub mod flac;
pub mod gif;
pub mod pcm;
pub mod rawvideo;

use crate::error::{Error, Result};
use crate::frame::{Frame, FrameFormat};
use crate::stream::{CodecId, Packet, StreamInfo};

pub trait Decoder {
    /// Decodes one packet. A packet may yield zero, one or several frames.
    fn decode(&mut self, packet: &Packet) -> Result<Vec<Frame>>;

    /// Returns frames still buffered once the input is exhausted.
    fn flush(&mut self) -> Result<Vec<Frame>> {
        Ok(Vec::new())
    }
}

pub trait Encoder {
    /// The exact frame format this encoder accepts. The pipeline converts
    /// every frame to it (sample rate, channels, sample kind, size, pixel
    /// format) before calling [`Encoder::encode`].
    fn input_format(&self) -> FrameFormat;

    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>>;

    /// Returns packets still buffered once the input is exhausted.
    fn flush(&mut self) -> Result<Vec<Packet>> {
        Ok(Vec::new())
    }

    /// Stream description for the muxer. Called before the header is
    /// written and again after [`Encoder::flush`], so encoders that only
    /// learn some values at the end (FLAC's sample count and MD5) can hand
    /// them to the muxer's trailer.
    fn stream_info(&self) -> StreamInfo;
}

#[derive(Clone, Debug, Default)]
pub struct EncoderOptions {
    /// Index of the output stream the encoder feeds, copied into its packets
    /// and stream info.
    pub stream_index: usize,
    /// Codec specific effort setting (FLAC: 0 to 8).
    pub compression_level: Option<u32>,
}

pub fn create_decoder(stream: &StreamInfo) -> Result<Box<dyn Decoder>> {
    match stream.codec {
        c if c.pcm_bits().is_some() => pcm::new_decoder(stream),
        CodecId::RawVideo => rawvideo::new_decoder(stream),
        CodecId::Flac => flac::new_decoder(stream),
        c => Err(Error::unsupported(format!("no decoder for {c}"))),
    }
}

/// Creates an encoder for `codec`. `proposed` is the format of the frames
/// the pipeline has (after user overrides such as `-ar`); the encoder keeps
/// what it can and reports its final choice through `input_format`.
pub fn create_encoder(
    codec: CodecId,
    proposed: &FrameFormat,
    options: &EncoderOptions,
) -> Result<Box<dyn Encoder>> {
    match (codec, proposed) {
        (c, FrameFormat::Audio(_)) if c.pcm_bits().is_some() => {
            pcm::new_encoder(codec, proposed, options)
        }
        (CodecId::Flac, FrameFormat::Audio(_)) => flac::new_encoder(codec, proposed, options),
        (CodecId::RawVideo, FrameFormat::Video(_)) => {
            rawvideo::new_encoder(codec, proposed, options)
        }
        (CodecId::Gif, FrameFormat::Video(_)) => gif::new_encoder(codec, proposed, options),
        (c, _) => Err(Error::unsupported(format!(
            "no encoder for {c} with this kind of input"
        ))),
    }
}

pub fn can_decode(codec: CodecId) -> bool {
    codec != CodecId::Gif
}

pub fn can_encode(_codec: CodecId) -> bool {
    true
}
