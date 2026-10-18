//! Generated inputs, selected with `-f lavfi -i "name=options"`.
//!
//! The format name matches the usual spelling so existing command lines
//! keep working, but only single source graphs are supported.

pub mod generators;
pub mod sine;

use crate::error::{Error, Result};
use crate::filter::Args;
use crate::frame::Frame;
use crate::stream::{MediaType, StreamInfo};

pub trait Source {
    fn stream(&self) -> &StreamInfo;

    /// Returns the next frame, or `None` once the requested duration is
    /// reached. Sources without a duration never end; the caller stops
    /// them with `-t` or a frame count.
    fn next_frame(&mut self) -> Result<Option<Frame>>;
}

pub struct SourceInfo {
    pub name: &'static str,
    pub description: &'static str,
    pub media: MediaType,
    pub positional: &'static [&'static str],
    pub create: fn(&Args) -> Result<Box<dyn Source>>,
}

pub fn sources() -> Vec<&'static SourceInfo> {
    std::iter::once(&sine::INFO)
        .chain(generators::SOURCES)
        .collect()
}

/// Builds a source from `name[=options]`.
pub fn open(spec: &str) -> Result<Box<dyn Source>> {
    let spec = spec.trim();
    if spec.contains([',', ';', '[']) {
        return Err(Error::unsupported(
            "filter graphs in -f lavfi inputs; give a single source and use -af/-vf for filters",
        ));
    }
    let (name, options) = spec.split_once('=').unwrap_or((spec, ""));
    let info = sources()
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| Error::invalid_argument(format!("no such source: '{name}'")))?;
    let args = Args::parse(options, info.positional)?;
    (info.create)(&args)
}

/// Number of whole units (samples or frames) in `duration` seconds at
/// `rate` units per second, or `None` for an endless source.
pub fn units_for(duration: Option<f64>, rate: f64) -> Option<u64> {
    duration
        .filter(|d| *d > 0.0)
        .map(|d| (d * rate).round() as u64)
}
