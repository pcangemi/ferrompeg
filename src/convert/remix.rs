//! Channel count conversion (downmix and upmix).

use crate::error::{Error, Result};

/// Converts interleaved float samples from `from` channels to `to`.
pub fn remix(samples: &[f32], from: u16, to: u16) -> Result<Vec<f32>> {
    if from == to {
        return Ok(samples.to_vec());
    }
    Err(Error::unsupported(format!(
        "remixing {from} channels to {to} is not implemented yet"
    )))
}
