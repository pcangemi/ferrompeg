//! A muxer that discards everything, for benchmarking a decode or a filter
//! chain without writing a file (`-f null -`).

use crate::error::Result;
use crate::format::{Muxer, OutputFormat, OutputOptions};
use crate::frame::PixelFormat;
use crate::stream::{CodecId, Packet, StreamInfo};

pub const OUTPUT: OutputFormat = OutputFormat {
    name: "null",
    long_name: "discard all output",
    extensions: &[],
    // Cheapest encoders, so the measurement is about decoding and filtering.
    audio_codec: Some(CodecId::PcmS16Le),
    video_codec: Some(CodecId::RawVideo),
    codecs: &CodecId::ALL,
    pixel_formats: &PixelFormat::ALL,
    create,
};

fn create(_path: &str, _options: &OutputOptions) -> Result<Box<dyn Muxer>> {
    Ok(Box::new(NullMuxer { bytes: 0 }))
}

struct NullMuxer {
    bytes: u64,
}

impl Muxer for NullMuxer {
    fn write_header(&mut self, _streams: &[StreamInfo]) -> Result<()> {
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        self.bytes += packet.data.len() as u64;
        Ok(())
    }

    fn write_trailer(&mut self, _streams: &[StreamInfo]) -> Result<()> {
        Ok(())
    }

    fn bytes_written(&self) -> u64 {
        self.bytes
    }
}
