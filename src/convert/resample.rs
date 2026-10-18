//! Sample rate conversion.

use crate::error::{Error, Result};

pub struct Resampler {}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32, _channels: u16) -> Result<Self> {
        Err(Error::unsupported(format!(
            "resampling {in_rate} Hz to {out_rate} Hz is not implemented yet"
        )))
    }

    /// Converts interleaved float samples, returning whatever output is
    /// ready. Input and output lengths differ by the rate ratio and by the
    /// filter delay.
    pub fn process(&mut self, _input: &[f32]) -> Vec<f32> {
        Vec::new()
    }

    /// Returns the samples still held in the filter at the end of input.
    pub fn flush(&mut self) -> Vec<f32> {
        Vec::new()
    }
}
