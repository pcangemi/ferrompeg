//! FLAC, the Free Lossless Audio Codec, implemented from RFC 9639.
//!
//! The container side (the `fLaC` marker and metadata blocks) lives in
//! `format::flac_demux` and `format::flac_mux`. Packets exchanged with them
//! are whole FLAC frames, and the stream extradata is the 34-byte
//! STREAMINFO block body.

mod decoder;
mod encoder;

pub use decoder::new_decoder;
pub use encoder::new_encoder;

use crate::error::{Error, Result};
use crate::util::bits::{BitReader, BitWriter};

/// Size of a STREAMINFO block body in bytes.
pub const STREAMINFO_SIZE: usize = 34;

/// The mandatory first metadata block of every FLAC stream.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StreamInfoBlock {
    pub min_block_size: u16,
    pub max_block_size: u16,
    /// Smallest and largest frame in bytes; 0 means unknown.
    pub min_frame_size: u32,
    pub max_frame_size: u32,
    pub sample_rate: u32,
    pub channels: u8,
    pub bits_per_sample: u8,
    /// Samples per channel in the whole stream; 0 means unknown.
    pub total_samples: u64,
    /// MD5 of the decoded samples (interleaved, little-endian, each sample
    /// in the smallest whole number of bytes); all zeros means unknown.
    pub md5: [u8; 16],
}

impl StreamInfoBlock {
    pub fn parse(body: &[u8]) -> Result<Self> {
        if body.len() < STREAMINFO_SIZE {
            return Err(Error::invalid_data("STREAMINFO block is too short"));
        }
        let mut r = BitReader::new(body);
        let info = StreamInfoBlock {
            min_block_size: r.read_bits(16)? as u16,
            max_block_size: r.read_bits(16)? as u16,
            min_frame_size: r.read_bits(24)?,
            max_frame_size: r.read_bits(24)?,
            sample_rate: r.read_bits(20)?,
            channels: r.read_bits(3)? as u8 + 1,
            bits_per_sample: r.read_bits(5)? as u8 + 1,
            total_samples: r.read_bits_u64(36)?,
            md5: body[18..34].try_into().unwrap(),
        };
        if info.sample_rate == 0 {
            return Err(Error::invalid_data(
                "STREAMINFO declares a zero sample rate",
            ));
        }
        if info.bits_per_sample < 4 {
            return Err(Error::invalid_data(
                "STREAMINFO declares fewer than 4 bits per sample",
            ));
        }
        Ok(info)
    }

    pub fn serialize(&self) -> [u8; STREAMINFO_SIZE] {
        let mut w = BitWriter::with_capacity(STREAMINFO_SIZE);
        w.write_bits(self.min_block_size as u32, 16);
        w.write_bits(self.max_block_size as u32, 16);
        w.write_bits(self.min_frame_size, 24);
        w.write_bits(self.max_frame_size, 24);
        w.write_bits(self.sample_rate, 20);
        w.write_bits(self.channels as u32 - 1, 3);
        w.write_bits(self.bits_per_sample as u32 - 1, 5);
        w.write_bits_u64(self.total_samples, 36);
        let mut out = [0u8; STREAMINFO_SIZE];
        out[..18].copy_from_slice(&w.into_bytes());
        out[18..].copy_from_slice(&self.md5);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaminfo_round_trip() {
        let info = StreamInfoBlock {
            min_block_size: 4096,
            max_block_size: 4096,
            min_frame_size: 14,
            max_frame_size: 12345,
            sample_rate: 44100,
            channels: 2,
            bits_per_sample: 16,
            total_samples: 0x9_8765_4321,
            md5: [7; 16],
        };
        let bytes = info.serialize();
        assert_eq!(StreamInfoBlock::parse(&bytes).unwrap(), info);
    }
}
