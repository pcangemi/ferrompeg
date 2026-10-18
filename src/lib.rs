//! ferrompeg: a small multimedia toolkit in pure Rust.
//!
//! The crate follows the classic media pipeline:
//!
//! ```text
//! input -> demuxer -> packets -> decoder -> frames -> filters -> encoder -> packets -> muxer -> output
//! ```
//!
//! * [`format`] holds containers (demuxers and muxers),
//! * [`codec`] holds decoders and encoders,
//! * [`filter`] and [`convert`] transform decoded frames,
//! * [`source`] generates frames (test tones, test patterns),
//! * [`cli`] wires everything together behind an ffmpeg-style command line.

pub mod cli;
pub mod codec;
pub mod convert;
pub mod error;
pub mod filter;
pub mod format;
pub mod frame;
pub mod io;
pub mod rational;
pub mod source;
pub mod stream;
pub mod util;

pub use error::{Error, Result};
