//! Format conversions the pipeline inserts between filters and encoders:
//! sample kind, sample rate and channel count for audio, size and pixel
//! format for video.

pub mod audio;
pub mod remix;
pub mod resample;
pub mod sample;
pub mod video;
