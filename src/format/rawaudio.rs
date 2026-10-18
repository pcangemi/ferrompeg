//! Headerless PCM (`-f s16le -ar 44100 -ac 2`). One demuxer and one muxer
//! serve every layout; the format name selects the codec.

use crate::format::{InputFormat, OutputFormat};

pub static INPUTS: &[InputFormat] = &[];

pub static OUTPUTS: &[OutputFormat] = &[];
