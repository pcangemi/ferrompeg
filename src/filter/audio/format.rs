//! Format changing filters: `aresample=48000` and
//! `aformat=sample_fmts=s16|flt:sample_rates=44100|48000:channel_layouts=stereo`.
//!
//! Both delegate to [`AudioConverter`], so they share the band-limited
//! resampler, the channel matrices and the timestamp handling of the
//! conversions the pipeline inserts before encoders.

use crate::convert::audio::AudioConverter;
use crate::error::{Error, Result};
use crate::filter::{Args, Filter};
use crate::frame::{AudioFormat, Frame, SampleKind};

use super::expect_audio;

/// Highest sample rate accepted, matching the resampler.
const MAX_RATE: u32 = 768_000;

/// Runs frames through an [`AudioConverter`] whose target is chosen from
/// the first frame. If the input format changes mid-stream the converter
/// is drained and replaced, keeping the same target.
struct Converting {
    name: &'static str,
    choose: Box<dyn Fn(AudioFormat) -> AudioFormat>,
    source: Option<AudioFormat>,
    target: Option<AudioFormat>,
    conv: Option<AudioConverter>,
}

impl Converting {
    fn new(name: &'static str, choose: impl Fn(AudioFormat) -> AudioFormat + 'static) -> Self {
        Converting {
            name,
            choose: Box::new(choose),
            source: None,
            target: None,
            conv: None,
        }
    }
}

impl Filter for Converting {
    fn filter(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        let a = expect_audio(frame, self.name)?;
        let format = a.format();
        let mut out = Vec::new();
        if self.source != Some(format) {
            if let Some(conv) = self.conv.as_mut() {
                out.extend(conv.flush()?.into_iter().map(Frame::Audio));
            }
            let target = *self.target.get_or_insert_with(|| (self.choose)(format));
            self.conv = Some(AudioConverter::new(target));
            self.source = Some(format);
        }
        let conv = self.conv.as_mut().expect("created above");
        out.extend(conv.convert(a)?.into_iter().map(Frame::Audio));
        Ok(out)
    }

    fn flush(&mut self) -> Result<Vec<Frame>> {
        match self.conv.as_mut() {
            Some(conv) => Ok(conv.flush()?.into_iter().map(Frame::Audio).collect()),
            None => Ok(Vec::new()),
        }
    }
}

fn parse_rate(value: &str) -> Result<u32> {
    match value.trim().parse::<u32>() {
        Ok(rate) if (1..=MAX_RATE).contains(&rate) => Ok(rate),
        _ => Err(Error::invalid_argument(format!(
            "invalid sample rate '{value}' (expected 1 to {MAX_RATE})"
        ))),
    }
}

pub fn create_aresample(args: &Args) -> Result<Box<dyn Filter>> {
    args.check_known("aresample", &["sample_rate", "osr", "out_sample_rate"])?;
    let rate = args
        .get(&["sample_rate", "osr", "out_sample_rate"])
        .map(parse_rate)
        .transpose()?;
    Ok(Box::new(Converting::new("aresample", move |input| {
        match rate {
            // Resampling produces floats anyway; keeping them avoids a
            // rounding step before the encoder converts once more.
            Some(r) if r != input.sample_rate => AudioFormat {
                sample_rate: r,
                channels: input.channels,
                kind: SampleKind::Float,
            },
            _ => input,
        }
    })))
}

/// Sample format names as written on the command line. Planar names are
/// accepted as aliases: frames are always interleaved here.
fn parse_kind(name: &str) -> Result<SampleKind> {
    Ok(match name.trim() {
        "u8" | "u8p" | "s8" => SampleKind::Int(8),
        "s16" | "s16p" => SampleKind::Int(16),
        "s32" | "s32p" => SampleKind::Int(32),
        "flt" | "fltp" => SampleKind::Float,
        other => {
            return Err(Error::invalid_argument(format!(
                "unknown or unsupported sample format '{other}' (known: u8, s16, s32, flt)"
            )));
        }
    })
}

/// Channel layout names, mapped to the channel counts whose layouts
/// [`crate::convert::remix`] uses.
fn parse_layout(name: &str) -> Result<u16> {
    let name = name.trim();
    let count = match name {
        "mono" => Some(1),
        "stereo" => Some(2),
        "2.1" => Some(3),
        "quad" => Some(4),
        "5.0" => Some(5),
        "5.1" => Some(6),
        "6.1" => Some(7),
        "7.1" => Some(8),
        // "6c" style: a plain channel count with its default layout.
        _ => name
            .strip_suffix('c')
            .and_then(|n| n.parse::<u16>().ok())
            .filter(|n| (1..=8).contains(n)),
    };
    count.ok_or_else(|| {
        Error::invalid_argument(format!(
            "unknown channel layout '{name}' (known: mono, stereo, 2.1, quad, 5.0, 5.1, 6.1, 7.1, 1c to 8c)"
        ))
    })
}

/// Splits a `|` separated list, rejecting empty entries.
fn parse_list<T>(value: Option<&str>, parse: impl Fn(&str) -> Result<T>) -> Result<Vec<T>> {
    match value {
        None => Ok(Vec::new()),
        Some(v) => v.split('|').map(parse).collect(),
    }
}

/// Picks the listed sample kind that loses the least: the input kind if
/// listed, else the narrowest integer kind at least as wide, else float,
/// else the widest integer kind.
fn pick_kind(input: SampleKind, allowed: &[SampleKind]) -> SampleKind {
    if allowed.is_empty() || allowed.contains(&input) {
        return input;
    }
    let ints = || {
        allowed.iter().filter_map(|k| match k {
            SampleKind::Int(b) => Some(*b),
            SampleKind::Float => None,
        })
    };
    let has_float = allowed.contains(&SampleKind::Float);
    let wider = match input {
        SampleKind::Int(bits) => ints().filter(|&b| b >= bits).min(),
        SampleKind::Float => None,
    };
    match (wider, has_float) {
        (Some(b), _) => SampleKind::Int(b),
        (None, true) => SampleKind::Float,
        (None, false) => SampleKind::Int(ints().max().unwrap_or(16)),
    }
}

/// Picks the listed rate closest to the input rate, the higher one on a
/// tie so no bandwidth is lost.
fn pick_rate(input: u32, allowed: &[u32]) -> u32 {
    allowed
        .iter()
        .copied()
        .min_by_key(|&r| (r.abs_diff(input), std::cmp::Reverse(r)))
        .unwrap_or(input)
}

/// Picks the input channel count if listed, else the smallest listed count
/// above it (nothing is mixed away), else the largest one.
fn pick_channels(input: u16, allowed: &[u16]) -> u16 {
    if allowed.is_empty() || allowed.contains(&input) {
        return input;
    }
    allowed
        .iter()
        .copied()
        .filter(|&c| c > input)
        .min()
        .or_else(|| allowed.iter().copied().max())
        .unwrap_or(input)
}

pub fn create_aformat(args: &Args) -> Result<Box<dyn Filter>> {
    args.check_known(
        "aformat",
        &[
            "sample_fmts",
            "f",
            "sample_rates",
            "r",
            "channel_layouts",
            "cl",
        ],
    )?;
    let kinds = parse_list(args.get(&["sample_fmts", "f"]), parse_kind)?;
    let rates = parse_list(args.get(&["sample_rates", "r"]), parse_rate)?;
    let layouts = parse_list(args.get(&["channel_layouts", "cl"]), parse_layout)?;
    Ok(Box::new(Converting::new("aformat", move |input| {
        AudioFormat {
            sample_rate: pick_rate(input.sample_rate, &rates),
            channels: pick_channels(input.channels, &layouts),
            kind: pick_kind(input.kind, &kinds),
        }
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::parse_chain;
    use crate::frame::{AudioData, AudioFrame};
    use crate::stream::MediaType;

    fn run(spec: &str, frames: Vec<AudioFrame>) -> Vec<AudioFrame> {
        let mut chain = parse_chain(spec, MediaType::Audio).unwrap();
        let mut out = Vec::new();
        for f in frames {
            out.extend(crate::filter::run_chain(&mut chain, Frame::Audio(f)).unwrap());
        }
        out.extend(crate::filter::flush_chain(&mut chain).unwrap());
        out.into_iter()
            .map(|f| match f {
                Frame::Audio(a) => a,
                Frame::Video(_) => panic!(),
            })
            .collect()
    }

    fn tone(rate: u32, channels: u16, n: usize, pts: i64) -> AudioFrame {
        let samples = (0..n * channels as usize)
            .map(|i| ((i / channels as usize) as f32 * 0.01).sin() * 0.5)
            .collect();
        AudioFrame::new(rate, channels, AudioData::Float(samples), pts)
    }

    fn total(frames: &[AudioFrame]) -> usize {
        frames.iter().map(AudioFrame::nb_samples).sum()
    }

    fn assert_contiguous(frames: &[AudioFrame], start: i64) {
        let mut pts = start;
        for f in frames {
            assert_eq!(f.pts, pts);
            pts += f.nb_samples() as i64;
        }
    }

    #[test]
    fn aresample_changes_rate_with_exact_length() {
        let frames = (0..10).map(|i| tone(44100, 2, 4410, i * 4410)).collect();
        let out = run("aresample=48000", frames);
        assert_eq!(total(&out), 48000);
        assert!(
            out.iter()
                .all(|f| f.sample_rate == 48000 && f.channels == 2)
        );
        assert_contiguous(&out, 0);
    }

    #[test]
    fn aresample_rescales_the_start_time() {
        // Starting one second in: pts 44100 at 44.1 kHz is 8000 at 8 kHz.
        let out = run("aresample=8000", vec![tone(44100, 1, 44100, 44100)]);
        assert_eq!(out[0].pts, 8000);
        assert_eq!(total(&out), 8000);
    }

    #[test]
    fn aresample_to_the_same_rate_passes_through() {
        let frame = AudioFrame::new(
            8000,
            1,
            AudioData::Int {
                samples: vec![1, 2, 3],
                bits: 16,
            },
            5,
        );
        assert_eq!(run("aresample=8000", vec![frame.clone()]), vec![frame]);
    }

    #[test]
    fn aformat_converts_kind_rate_and_layout() {
        let out = run(
            "aformat=sample_fmts=s16:sample_rates=8000:channel_layouts=mono",
            vec![tone(16000, 2, 1600, 0)],
        );
        assert_eq!(total(&out), 800);
        for f in &out {
            assert_eq!(
                f.format(),
                AudioFormat {
                    sample_rate: 8000,
                    channels: 1,
                    kind: SampleKind::Int(16)
                }
            );
        }
        assert_contiguous(&out, 0);
    }

    #[test]
    fn aformat_keeps_what_is_allowed() {
        let frame = tone(48000, 2, 100, 0);
        let out = run(
            "aformat=flt|s16:48000|44100:stereo|5.1",
            vec![frame.clone()],
        );
        assert_eq!(out, vec![frame]);
    }

    #[test]
    fn aformat_choices() {
        use SampleKind::*;
        assert_eq!(pick_kind(Int(16), &[Float, Int(32)]), Int(32));
        assert_eq!(pick_kind(Int(24), &[Int(16), Float]), Float);
        assert_eq!(pick_kind(Float, &[Int(16), Int(32)]), Int(32));
        assert_eq!(pick_kind(Int(16), &[]), Int(16));
        assert_eq!(pick_rate(44100, &[48000, 32000]), 48000);
        assert_eq!(pick_rate(44100, &[]), 44100);
        assert_eq!(pick_rate(30000, &[20000, 40000]), 40000);
        assert_eq!(pick_channels(2, &[1, 6, 8]), 6);
        assert_eq!(pick_channels(8, &[1, 2]), 2);
        assert_eq!(pick_channels(2, &[]), 2);
    }

    #[test]
    fn parses_names() {
        assert_eq!(parse_layout("5.1").unwrap(), 6);
        assert_eq!(parse_layout("7.1").unwrap(), 8);
        assert_eq!(parse_layout("3c").unwrap(), 3);
        assert_eq!(parse_kind("fltp").unwrap(), SampleKind::Float);
        for bad in ["", "9c", "0c", "hexaphonic", "c"] {
            assert!(parse_layout(bad).is_err(), "{bad}");
        }
        assert!(parse_kind("dbl").is_err());
        assert!(parse_rate("0").is_err());
        assert!(parse_rate("768001").is_err());
        assert!(parse_rate("abc").is_err());
    }

    #[test]
    fn rejects_bad_options() {
        for spec in [
            "aformat=sample_fmts=s16|",
            "aformat=sample_rates=0",
            "aformat=channel_layouts=bogus",
            "aformat=mode=x",
            "aresample=0",
            "aresample=fast",
            "aresample=48000:async=1",
        ] {
            assert!(parse_chain(spec, MediaType::Audio).is_err(), "{spec}");
        }
    }

    #[test]
    fn input_format_change_drains_and_restarts() {
        let out = run(
            "aresample=16000",
            vec![tone(8000, 1, 800, 0), tone(32000, 1, 3200, 3200)],
        );
        // 0.1 s of each input becomes 1600 samples of each at 16 kHz.
        assert_eq!(total(&out), 3200);
        assert!(out.iter().all(|f| f.sample_rate == 16000));
    }
}
