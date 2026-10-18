//! Containers: demuxers split a file into packets, muxers write packets
//! into a file.

pub mod aiff;
pub mod au;
pub mod flac_demux;
pub mod flac_mux;
pub mod gif;
pub mod image2;
pub mod null;
pub mod rawaudio;
pub mod rawvideo;
pub mod wav;
pub mod y4m;

use std::io::Read;
use std::path::Path;

use crate::error::{Error, Result};
use crate::frame::PixelFormat;
use crate::rational::Rational;
use crate::stream::{CodecId, Packet, StreamInfo};

pub trait Demuxer {
    fn format_name(&self) -> &'static str;

    fn streams(&self) -> &[StreamInfo];

    /// Returns the next packet, or `None` at the end of the input.
    fn read_packet(&mut self) -> Result<Option<Packet>>;

    /// Container level tags (title, artist...).
    fn metadata(&self) -> &[(String, String)] {
        &[]
    }
}

pub trait Muxer {
    fn write_header(&mut self, streams: &[StreamInfo]) -> Result<()>;

    fn write_packet(&mut self, packet: &Packet) -> Result<()>;

    /// Finishes the file. `streams` carries the encoders' final stream
    /// info, so muxers can patch sizes, sample counts or checksums that
    /// were unknown when the header was written.
    fn write_trailer(&mut self, streams: &[StreamInfo]) -> Result<()>;

    /// Bytes written so far, for progress reporting.
    fn bytes_written(&self) -> u64;
}

/// Parameters that raw inputs cannot carry themselves and that the user
/// supplies on the command line (`-ar`, `-ac`, `-s`, `-pix_fmt`, `-r`).
#[derive(Clone, Debug, Default)]
pub struct InputOptions {
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    pub video_size: Option<(u32, u32)>,
    pub pixel_format: Option<PixelFormat>,
    pub frame_rate: Option<Rational>,
    /// Set by [`open_input`] for raw audio formats such as `s16le`, which
    /// are all handled by one demuxer.
    pub raw_codec: Option<CodecId>,
}

#[derive(Clone, Debug, Default)]
pub struct OutputOptions {
    /// GIF loop count: 0 loops forever, -1 plays once, n repeats n times.
    pub loop_count: Option<i32>,
    /// Tags to write where the container supports them.
    pub metadata: Vec<(String, String)>,
}

/// Builds a demuxer over a reader.
pub type OpenFn = fn(Box<dyn Read>, &InputOptions) -> Result<Box<dyn Demuxer>>;

pub struct InputFormat {
    pub name: &'static str,
    pub long_name: &'static str,
    pub extensions: &'static [&'static str],
    /// Scores how likely the first bytes of a file belong to this format,
    /// from 0 (no) to 100 (certain). Raw formats always answer 0 and must
    /// be selected with `-f`.
    pub probe: fn(&[u8]) -> u32,
    pub open: OpenFn,
    /// Codec of a raw audio format, passed to the demuxer in
    /// [`InputOptions::raw_codec`].
    pub raw_codec: Option<CodecId>,
}

pub struct OutputFormat {
    pub name: &'static str,
    pub long_name: &'static str,
    pub extensions: &'static [&'static str],
    /// Codec used when the user does not pick one.
    pub audio_codec: Option<CodecId>,
    pub video_codec: Option<CodecId>,
    /// Every codec this container can hold.
    pub codecs: &'static [CodecId],
    /// Pixel formats the container can store, in order of preference.
    /// Empty means any.
    pub pixel_formats: &'static [PixelFormat],
    /// Opens the destination (a path, or "-" for standard output) and
    /// returns the muxer.
    pub create: fn(&str, &OutputOptions) -> Result<Box<dyn Muxer>>,
}

/// Every demuxer, in probing order.
pub fn input_formats() -> Vec<&'static InputFormat> {
    let mut all: Vec<&'static InputFormat> = vec![
        &wav::INPUT,
        &aiff::INPUT,
        &au::INPUT,
        &flac_demux::INPUT,
        &y4m::INPUT,
        &rawvideo::INPUT,
    ];
    all.extend(rawaudio::INPUTS.iter());
    all
}

/// Every muxer.
pub fn output_formats() -> Vec<&'static OutputFormat> {
    let mut all: Vec<&'static OutputFormat> = vec![
        &wav::OUTPUT,
        &aiff::OUTPUT,
        &au::OUTPUT,
        &flac_mux::OUTPUT,
        &y4m::OUTPUT,
        &gif::OUTPUT,
        &image2::OUTPUT,
        &rawvideo::OUTPUT,
        &null::OUTPUT,
    ];
    all.extend(rawaudio::OUTPUTS.iter());
    all
}

fn extension(path: &str) -> Option<String> {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// Opens `path` ("-" reads standard input) and picks a demuxer: the one
/// named by `forced` (`-f`), else the best content probe, else the file
/// extension.
pub fn open_input(
    path: &str,
    forced: Option<&str>,
    options: &InputOptions,
) -> Result<Box<dyn Demuxer>> {
    let reader = crate::io::open_input(path)?;
    let format = match forced {
        Some(name) => input_formats()
            .into_iter()
            .find(|f| f.name == name)
            .ok_or_else(|| Error::invalid_argument(format!("unknown input format '{name}'")))?,
        None => {
            let (prefix, reader) = crate::io::peek_prefix(reader, 4096)?;
            let format = detect_input_format(&prefix, path)?;
            return open_with(format, reader, options);
        }
    };
    open_with(format, reader, options)
}

fn open_with(
    format: &'static InputFormat,
    reader: Box<dyn Read>,
    options: &InputOptions,
) -> Result<Box<dyn Demuxer>> {
    let mut options = options.clone();
    if format.raw_codec.is_some() {
        options.raw_codec = format.raw_codec;
    }
    (format.open)(reader, &options)
}

/// Picks a demuxer from the first bytes of a file, falling back on the
/// extension of `path`.
pub fn detect_input_format(prefix: &[u8], path: &str) -> Result<&'static InputFormat> {
    let best = input_formats()
        .into_iter()
        .map(|f| ((f.probe)(prefix), f))
        .filter(|(score, _)| *score > 0)
        .max_by_key(|(score, _)| *score);
    if let Some((_, format)) = best {
        return Ok(format);
    }
    if let Some(ext) = extension(path) {
        if let Some(f) = input_formats()
            .into_iter()
            .find(|f| f.extensions.contains(&ext.as_str()))
        {
            return Ok(f);
        }
    }
    Err(Error::invalid_data(format!(
        "{path}: cannot recognize the input format (use -f to force one)"
    )))
}

/// Picks a muxer: the one named by `forced`, else from the file extension.
/// Paths containing a `%d` style pattern go to the image sequence muxer.
pub fn guess_output_format(path: &str, forced: Option<&str>) -> Result<&'static OutputFormat> {
    if let Some(name) = forced {
        return output_formats()
            .into_iter()
            .find(|f| f.name == name)
            .ok_or_else(|| Error::invalid_argument(format!("unknown output format '{name}'")));
    }
    if image2::is_pattern(path) {
        return Ok(&image2::OUTPUT);
    }
    let ext = extension(path).unwrap_or_default();
    output_formats()
        .into_iter()
        .find(|f| f.extensions.contains(&ext.as_str()))
        .ok_or_else(|| {
            Error::invalid_argument(format!(
                "unable to choose an output format for '{path}'; use a standard extension or -f"
            ))
        })
}

/// A probe for formats that can only be selected explicitly.
pub fn probe_never(_data: &[u8]) -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_names_are_unique() {
        let mut names: Vec<_> = input_formats().iter().map(|f| f.name).collect();
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate input format name");
        let mut names: Vec<_> = output_formats().iter().map(|f| f.name).collect();
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate output format name");
    }

    #[test]
    fn output_defaults_are_listed_codecs() {
        for f in output_formats() {
            for c in [f.audio_codec, f.video_codec].into_iter().flatten() {
                assert!(
                    f.codecs.contains(&c),
                    "{} default {c} not in its list",
                    f.name
                );
            }
        }
    }

    #[test]
    fn guesses_by_extension() {
        assert_eq!(guess_output_format("a.WAV", None).unwrap().name, "wav");
        assert_eq!(guess_output_format("x", Some("null")).unwrap().name, "null");
        assert!(guess_output_format("a.unknown", None).is_err());
    }
}
