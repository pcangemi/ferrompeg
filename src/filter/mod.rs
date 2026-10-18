//! Filters transform decoded frames: `-af volume=0.5`, `-vf scale=320:240`.

pub mod args;
pub mod audio;
pub mod video;

use crate::error::{Error, Result};
use crate::frame::Frame;
use crate::stream::MediaType;

pub use args::Args;

/// A frame transformer. Filters adapt to whatever format reaches them, so
/// a chain needs no negotiation up front; a filter may still reject an
/// input it cannot handle.
pub trait Filter {
    /// Processes one frame. Filters that buffer (resampling, frame rate
    /// changes) may return zero or several frames.
    fn filter(&mut self, frame: Frame) -> Result<Vec<Frame>>;

    /// Returns buffered frames at the end of the stream.
    fn flush(&mut self) -> Result<Vec<Frame>> {
        Ok(Vec::new())
    }
}

pub struct FilterInfo {
    pub name: &'static str,
    pub description: &'static str,
    pub media: MediaType,
    /// Names given to unnamed values, in order (`scale=320:240` sets `w`
    /// then `h`).
    pub positional: &'static [&'static str],
    pub create: fn(&Args) -> Result<Box<dyn Filter>>,
}

/// Passes frames through unchanged.
struct Null;

impl Filter for Null {
    fn filter(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        Ok(vec![frame])
    }
}

fn create_null(args: &Args) -> Result<Box<dyn Filter>> {
    args.check_known("null", &[])?;
    Ok(Box::new(Null))
}

static BUILTIN: &[FilterInfo] = &[
    FilterInfo {
        name: "null",
        description: "Pass the video through unchanged.",
        media: MediaType::Video,
        positional: &[],
        create: create_null,
    },
    FilterInfo {
        name: "anull",
        description: "Pass the audio through unchanged.",
        media: MediaType::Audio,
        positional: &[],
        create: create_null,
    },
];

/// Every filter, audio first.
pub fn filters() -> Vec<&'static FilterInfo> {
    BUILTIN
        .iter()
        .chain(audio::FILTERS)
        .chain(video::FILTERS)
        .collect()
}

/// Builds a filter chain from `name[=options],name[=options]...`.
pub fn parse_chain(spec: &str, media: MediaType) -> Result<Vec<Box<dyn Filter>>> {
    let mut chain = Vec::new();
    for item in args::split_raw(spec, ',')? {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (name, options) = item.split_once('=').unwrap_or((item, ""));
        let info = filters()
            .into_iter()
            .find(|f| f.name == name)
            .ok_or_else(|| Error::invalid_argument(format!("no such filter: '{name}'")))?;
        if info.media != media {
            return Err(Error::invalid_argument(format!(
                "'{name}' is a {} filter and cannot be used on {}",
                info.media.to_string().to_lowercase(),
                media.to_string().to_lowercase()
            )));
        }
        let args = Args::parse(options, info.positional)?;
        chain.push((info.create)(&args)?);
    }
    Ok(chain)
}

/// Runs `frame` through `chain`, feeding each filter's outputs to the next.
pub fn run_chain(chain: &mut [Box<dyn Filter>], frame: Frame) -> Result<Vec<Frame>> {
    let mut frames = vec![frame];
    for filter in chain.iter_mut() {
        let mut next = Vec::new();
        for f in frames {
            next.extend(filter.filter(f)?);
        }
        frames = next;
    }
    Ok(frames)
}

/// Flushes `chain` front to back: frames released by one filter's flush
/// still go through the filters after it.
pub fn flush_chain(chain: &mut [Box<dyn Filter>]) -> Result<Vec<Frame>> {
    let mut pending = Vec::new();
    for filter in chain.iter_mut() {
        let mut next = Vec::new();
        for f in pending {
            next.extend(filter.filter(f)?);
        }
        next.extend(filter.flush()?);
        pending = next;
    }
    Ok(pending)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_names_are_unique() {
        let mut names: Vec<_> = filters().iter().map(|f| f.name).collect();
        names.sort();
        let n = names.len();
        names.dedup();
        assert_eq!(n, names.len());
    }

    #[test]
    fn rejects_unknown_and_mismatched_filters() {
        assert!(parse_chain("nope", MediaType::Audio).is_err());
        assert!(parse_chain("null", MediaType::Audio).is_err());
        assert_eq!(
            parse_chain("anull,anull", MediaType::Audio).unwrap().len(),
            2
        );
        assert!(parse_chain("", MediaType::Video).unwrap().is_empty());
    }
}
