//! Gain change: `volume=0.5`, `volume=2`, `volume=6dB`, `volume=-3dB`.
//!
//! A level in decibels is an amplitude ratio of `10^(dB / 20)`
//! (IEC 60027-3), so 6 dB roughly doubles the amplitude.

use crate::error::{Error, Result};
use crate::filter::{Args, Filter};
use crate::frame::{AudioData, AudioFrame, Frame};

use super::expect_audio;

pub struct Volume {
    gain: f32,
}

pub fn create(args: &Args) -> Result<Box<dyn Filter>> {
    args.check_known("volume", &["volume"])?;
    let gain = match args.get(&["volume"]) {
        Some(v) => parse_gain(v)?,
        None => 1.0,
    };
    Ok(Box::new(Volume { gain }))
}

/// Parses a linear factor or a level with a `dB` suffix (any case).
fn parse_gain(value: &str) -> Result<f32> {
    let bad = || Error::invalid_argument(format!("invalid volume '{value}'"));
    let v = value.trim();
    let lower = v.to_ascii_lowercase();
    let gain = if let Some(db) = lower.strip_suffix("db") {
        let db: f64 = db.trim().parse().map_err(|_| bad())?;
        10f64.powf(db / 20.0)
    } else {
        let g: f64 = v.parse().map_err(|_| bad())?;
        if g < 0.0 {
            return Err(Error::invalid_argument(format!(
                "volume must not be negative: '{value}'"
            )));
        }
        g
    };
    // Anything past f32 range would turn every sample into infinity.
    if !gain.is_finite() || gain > f32::MAX as f64 {
        return Err(bad());
    }
    Ok(gain as f32)
}

impl Filter for Volume {
    fn filter(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        let a = expect_audio(frame, "volume")?;
        if self.gain == 1.0 {
            return Ok(vec![Frame::Audio(a)]);
        }
        let samples = a.to_f32().into_iter().map(|s| s * self.gain).collect();
        Ok(vec![Frame::Audio(AudioFrame::new(
            a.sample_rate,
            a.channels,
            AudioData::Float(samples),
            a.pts,
        ))])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(spec: &str, frame: AudioFrame) -> AudioFrame {
        let mut f = create(&Args::parse(spec, &["volume"]).unwrap()).unwrap();
        let mut out = f.filter(Frame::Audio(frame)).unwrap();
        let Some(Frame::Audio(a)) = out.pop() else {
            panic!()
        };
        a
    }

    #[test]
    fn parses_factors_and_decibels() {
        assert_eq!(parse_gain("0.5").unwrap(), 0.5);
        assert_eq!(parse_gain("2").unwrap(), 2.0);
        assert!((parse_gain("6dB").unwrap() - 1.995_262_3).abs() < 1e-6);
        assert!((parse_gain("-3dB").unwrap() - 0.707_945_8).abs() < 1e-6);
        assert!((parse_gain(" -20 db ").unwrap() - 0.1).abs() < 1e-7);
        assert_eq!(parse_gain("0").unwrap(), 0.0);
        for bad in ["", "x", "-1", "dB", "1e400", "5000dB", "nan"] {
            assert!(parse_gain(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn scales_any_sample_kind() {
        let int = AudioFrame::new(
            8000,
            2,
            AudioData::Int {
                samples: vec![16384, -16384],
                bits: 16,
            },
            42,
        );
        let out = apply("volume=0.5", int);
        assert_eq!(out.data, AudioData::Float(vec![0.25, -0.25]));
        assert_eq!((out.pts, out.channels, out.sample_rate), (42, 2, 8000));

        let float = AudioFrame::new(8000, 1, AudioData::Float(vec![0.25]), 0);
        assert_eq!(apply("2", float).data, AudioData::Float(vec![0.5]));
    }

    #[test]
    fn unity_passes_through() {
        let int = AudioFrame::new(
            8000,
            1,
            AudioData::Int {
                samples: vec![7],
                bits: 16,
            },
            0,
        );
        assert_eq!(apply("volume=1", int.clone()), int);
        assert_eq!(apply("0dB", int.clone()), int);
    }

    #[test]
    fn rejects_unknown_options() {
        assert!(create(&Args::parse("gain=2", &["volume"]).unwrap()).is_err());
    }
}
