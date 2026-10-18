//! Command line parsing with the usual positional semantics: options apply
//! to the next `-i` input or the next output file that follows them.

use crate::cli::log::LogLevel;
use crate::convert::video::ScaleAlgorithm;
use crate::error::{Error, Result};
use crate::filter::args::{parse_frame_rate, parse_size};
use crate::format::InputOptions;
use crate::frame::PixelFormat;
use crate::rational::Rational;
use crate::stream::CodecId;
use crate::util::time::parse_duration;

#[derive(Clone, Debug, Default)]
pub struct GlobalOptions {
    /// `Some(true)` for `-y`, `Some(false)` for `-n`, `None` to ask.
    pub overwrite: Option<bool>,
    pub hide_banner: bool,
    pub log_level: Option<LogLevel>,
    pub stats: bool,
}

#[derive(Clone, Debug, Default)]
pub struct InputSpec {
    pub path: String,
    pub format: Option<String>,
    pub start: Option<f64>,
    pub duration: Option<f64>,
    pub to: Option<f64>,
    pub options: InputOptions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecChoice {
    Encode(CodecId),
    /// Pass packets through without decoding (`-c copy`).
    Copy,
}

#[derive(Clone, Debug, Default)]
pub struct OutputSpec {
    pub path: String,
    pub format: Option<String>,
    pub audio_codec: Option<CodecChoice>,
    pub video_codec: Option<CodecChoice>,
    pub audio_filters: Option<String>,
    pub video_filters: Option<String>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    pub size: Option<(u32, u32)>,
    pub pixel_format: Option<PixelFormat>,
    pub frame_rate: Option<Rational>,
    pub start: Option<f64>,
    pub duration: Option<f64>,
    pub to: Option<f64>,
    pub max_audio_frames: Option<u64>,
    pub max_video_frames: Option<u64>,
    pub no_audio: bool,
    pub no_video: bool,
    pub compression_level: Option<u32>,
    pub scale_algorithm: Option<ScaleAlgorithm>,
    pub loop_count: Option<i32>,
    pub metadata: Vec<(String, String)>,
}

#[derive(Debug)]
pub enum Action {
    Help,
    Version,
    ListFormats,
    ListCodecs,
    ListFilters,
    Transcode {
        inputs: Vec<InputSpec>,
        outputs: Vec<OutputSpec>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Global,
    Output,
    Both,
}

/// Canonical name, scope and whether the option takes a value.
fn lookup(name: &str) -> Option<(&'static str, Scope, bool)> {
    use Scope::*;
    Some(match name {
        "y" => ("y", Global, false),
        "n" => ("n", Global, false),
        "hide_banner" => ("hide_banner", Global, false),
        "loglevel" | "v" => ("loglevel", Global, true),
        "stats" => ("stats", Global, false),
        "nostats" => ("nostats", Global, false),
        "f" => ("f", Both, true),
        "ss" => ("ss", Both, true),
        "t" => ("t", Both, true),
        "to" => ("to", Both, true),
        "ar" => ("ar", Both, true),
        "ac" => ("ac", Both, true),
        "s" => ("s", Both, true),
        "pix_fmt" => ("pix_fmt", Both, true),
        "r" => ("r", Both, true),
        "c" | "codec" => ("c", Output, true),
        "c:a" | "codec:a" | "acodec" => ("c:a", Output, true),
        "c:v" | "codec:v" | "vcodec" => ("c:v", Output, true),
        "af" | "filter:a" => ("af", Output, true),
        "vf" | "filter:v" => ("vf", Output, true),
        "an" => ("an", Output, false),
        "vn" => ("vn", Output, false),
        "frames:v" | "vframes" => ("frames:v", Output, true),
        "frames:a" | "aframes" => ("frames:a", Output, true),
        "frames" => ("frames", Output, true),
        "compression_level" => ("compression_level", Output, true),
        "loop" => ("loop", Output, true),
        "metadata" => ("metadata", Output, true),
        "sws_flags" => ("sws_flags", Output, true),
        _ => return None,
    })
}

/// Parses the arguments that follow the program name.
pub fn parse(args: &[String]) -> Result<(GlobalOptions, Action)> {
    let mut global = GlobalOptions {
        stats: true,
        ..Default::default()
    };
    let mut pending: Vec<(&'static str, String)> = Vec::new();
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        let Some(name) = arg.strip_prefix('-').filter(|n| !n.is_empty()) else {
            // A bare word (or "-" for standard output) is an output file.
            outputs.push(build_output(arg, std::mem::take(&mut pending))?);
            continue;
        };
        match name {
            "h" | "help" | "-help" | "?" => return Ok((global, Action::Help)),
            "version" | "-version" => return Ok((global, Action::Version)),
            "formats" => return Ok((global, Action::ListFormats)),
            "codecs" | "encoders" | "decoders" => return Ok((global, Action::ListCodecs)),
            "filters" => return Ok((global, Action::ListFilters)),
            "i" => {
                let path = args
                    .get(i)
                    .ok_or_else(|| Error::invalid_argument("missing argument for option 'i'"))?;
                i += 1;
                inputs.push(build_input(path, std::mem::take(&mut pending))?);
                continue;
            }
            _ => {}
        }
        let (canonical, scope, takes_value) = lookup(name)
            .ok_or_else(|| Error::invalid_argument(format!("unrecognized option '{name}'")))?;
        let value = if takes_value {
            let v = args.get(i).ok_or_else(|| {
                Error::invalid_argument(format!("missing argument for option '{name}'"))
            })?;
            i += 1;
            v.clone()
        } else {
            String::new()
        };
        if scope == Scope::Global {
            apply_global(&mut global, canonical, &value)?;
        } else {
            pending.push((canonical, value));
        }
    }
    if !pending.is_empty() {
        let names: Vec<_> = pending.iter().map(|(n, _)| format!("-{n}")).collect();
        return Err(Error::invalid_argument(format!(
            "trailing options without an output file: {}",
            names.join(" ")
        )));
    }
    if inputs.is_empty() && outputs.is_empty() {
        return Ok((global, Action::Help));
    }
    Ok((global, Action::Transcode { inputs, outputs }))
}

fn apply_global(global: &mut GlobalOptions, name: &str, value: &str) -> Result<()> {
    match name {
        "y" => global.overwrite = Some(true),
        "n" => global.overwrite = Some(false),
        "hide_banner" => global.hide_banner = true,
        "stats" => global.stats = true,
        "nostats" => global.stats = false,
        "loglevel" => {
            // Allow "repeat+level" style prefixes by keeping the last part.
            let level = value.rsplit('+').next().unwrap_or(value);
            global.log_level =
                Some(LogLevel::parse(level).ok_or_else(|| {
                    Error::invalid_argument(format!("invalid log level '{value}'"))
                })?);
        }
        _ => unreachable!("not a global option: {name}"),
    }
    Ok(())
}

fn parse_u32(name: &str, v: &str) -> Result<u32> {
    v.trim()
        .parse()
        .map_err(|_| Error::invalid_argument(format!("invalid value '{v}' for -{name}")))
}

fn parse_u64(name: &str, v: &str) -> Result<u64> {
    v.trim()
        .parse()
        .map_err(|_| Error::invalid_argument(format!("invalid value '{v}' for -{name}")))
}

fn parse_channels(v: &str) -> Result<u16> {
    match v.trim().parse::<u16>() {
        Ok(n) if (1..=8).contains(&n) => Ok(n),
        _ => Err(Error::invalid_argument(format!(
            "invalid channel count '{v}' (1 to 8)"
        ))),
    }
}

fn parse_rate(v: &str) -> Result<u32> {
    match v.trim().parse::<u32>() {
        Ok(n) if (1..=768_000).contains(&n) => Ok(n),
        _ => Err(Error::invalid_argument(format!(
            "invalid sample rate '{v}'"
        ))),
    }
}

fn build_input(path: &str, options: Vec<(&'static str, String)>) -> Result<InputSpec> {
    let mut spec = InputSpec {
        path: path.to_string(),
        ..Default::default()
    };
    for (name, v) in options {
        match name {
            "f" => spec.format = Some(v),
            "ss" => spec.start = Some(parse_duration(&v)?),
            "t" => spec.duration = Some(parse_duration(&v)?),
            "to" => spec.to = Some(parse_duration(&v)?),
            "ar" => spec.options.sample_rate = Some(parse_rate(&v)?),
            "ac" => spec.options.channels = Some(parse_channels(&v)?),
            "s" => spec.options.video_size = Some(parse_size(&v)?),
            "pix_fmt" => spec.options.pixel_format = Some(PixelFormat::from_name(&v)?),
            "r" => spec.options.frame_rate = Some(parse_frame_rate(&v)?),
            other => {
                return Err(Error::invalid_argument(format!(
                    "option -{other} cannot be applied to an input"
                )));
            }
        }
    }
    Ok(spec)
}

fn parse_codec(v: &str) -> Result<CodecChoice> {
    if v == "copy" {
        Ok(CodecChoice::Copy)
    } else {
        CodecId::from_name(v).map(CodecChoice::Encode)
    }
}

fn build_output(path: &str, options: Vec<(&'static str, String)>) -> Result<OutputSpec> {
    let mut spec = OutputSpec {
        path: path.to_string(),
        ..Default::default()
    };
    for (name, v) in options {
        match name {
            "f" => spec.format = Some(v),
            "ss" => spec.start = Some(parse_duration(&v)?),
            "t" => spec.duration = Some(parse_duration(&v)?),
            "to" => spec.to = Some(parse_duration(&v)?),
            "ar" => spec.sample_rate = Some(parse_rate(&v)?),
            "ac" => spec.channels = Some(parse_channels(&v)?),
            "s" => spec.size = Some(parse_size(&v)?),
            "pix_fmt" => spec.pixel_format = Some(PixelFormat::from_name(&v)?),
            "r" => spec.frame_rate = Some(parse_frame_rate(&v)?),
            "c" => {
                let c = parse_codec(&v)?;
                spec.audio_codec = Some(c);
                spec.video_codec = Some(c);
            }
            "c:a" => spec.audio_codec = Some(parse_codec(&v)?),
            "c:v" => spec.video_codec = Some(parse_codec(&v)?),
            "af" => spec.audio_filters = Some(v),
            "vf" => spec.video_filters = Some(v),
            "an" => spec.no_audio = true,
            "vn" => spec.no_video = true,
            "frames:v" => spec.max_video_frames = Some(parse_u64(name, &v)?),
            "frames:a" => spec.max_audio_frames = Some(parse_u64(name, &v)?),
            "frames" => {
                let n = parse_u64(name, &v)?;
                spec.max_video_frames = Some(n);
                spec.max_audio_frames = Some(n);
            }
            "compression_level" => spec.compression_level = Some(parse_u32(name, &v)?),
            "loop" => {
                spec.loop_count = Some(v.trim().parse().map_err(|_| {
                    Error::invalid_argument(format!("invalid value '{v}' for -loop"))
                })?)
            }
            "metadata" => {
                let (k, val) = v.split_once('=').ok_or_else(|| {
                    Error::invalid_argument(format!("-metadata expects key=value, got '{v}'"))
                })?;
                spec.metadata.push((k.to_string(), val.to_string()));
            }
            "sws_flags" => spec.scale_algorithm = Some(ScaleAlgorithm::from_name(&v)?),
            other => {
                return Err(Error::invalid_argument(format!(
                    "option -{other} cannot be applied to an output"
                )));
            }
        }
    }
    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn options_bind_to_the_next_file() {
        let (global, action) = parse(&args(
            "-y -f s16le -ar 8000 -ac 1 -i in.raw -ar 44100 -c:a pcm_s24le out.wav",
        ))
        .unwrap();
        assert_eq!(global.overwrite, Some(true));
        let Action::Transcode { inputs, outputs } = action else {
            panic!()
        };
        assert_eq!(inputs[0].format.as_deref(), Some("s16le"));
        assert_eq!(inputs[0].options.sample_rate, Some(8000));
        assert_eq!(inputs[0].options.channels, Some(1));
        assert_eq!(outputs[0].path, "out.wav");
        assert_eq!(outputs[0].sample_rate, Some(44100));
        assert_eq!(
            outputs[0].audio_codec,
            Some(CodecChoice::Encode(CodecId::PcmS24Le))
        );
    }

    #[test]
    fn several_outputs_and_stdout() {
        let (_, action) = parse(&args("-i a.wav -t 1 one.wav -f wav -")).unwrap();
        let Action::Transcode { outputs, .. } = action else {
            panic!()
        };
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].duration, Some(1.0));
        assert_eq!(outputs[1].path, "-");
        assert_eq!(outputs[1].duration, None);
    }

    #[test]
    fn errors_are_reported() {
        assert!(parse(&args("-i a.wav -bogus 1 b.wav")).is_err());
        assert!(parse(&args("-i a.wav -ar")).is_err());
        assert!(parse(&args("-i a.wav out.wav -t 3")).is_err());
        assert!(parse(&args("-af volume=2 -i a.wav out.wav")).is_err());
        assert!(parse(&args("-i a.wav -ac 0 out.wav")).is_err());
    }

    #[test]
    fn info_actions() {
        assert!(matches!(
            parse(&args("-formats")).unwrap().1,
            Action::ListFormats
        ));
        assert!(matches!(parse(&[]).unwrap().1, Action::Help));
        let (g, _) = parse(&args("-v error -hide_banner -nostats -formats")).unwrap();
        assert_eq!(g.log_level, Some(LogLevel::Error));
        assert!(g.hide_banner && !g.stats);
    }
}
