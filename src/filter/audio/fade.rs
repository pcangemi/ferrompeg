//! Fade in and fade out: `afade=t=in:st=0:d=2:curve=qsin`.
//!
//! The gain of each sample comes from its absolute position (`pts` plus
//! its index in the frame), never from a running counter, so a fade lands
//! at the requested time whatever trimming or buffering happened before.
//! With `x` the progress through the fade from 0 to 1, the curves are:
//!
//! * `tri`: `x`, a straight line in amplitude;
//! * `qsin`: `sin(x * pi / 2)`, a quarter sine period, which rises fast
//!   and settles smoothly;
//! * `exp`: `(10^(3x) - 1) / 999`, linear in decibels over the last 60 dB,
//!   so it starts slowly and sounds even to the ear;
//! * `log`: `log10(1 + 999x) / 3`, the mirror image of `exp`, which rises
//!   quickly at first.
//!
//! A fade out plays the same curve backwards in time: its gain is the
//! curve at `1 - x`. Before a fade in the output is silent and after it
//! the input passes unchanged; the opposite holds for a fade out.

use crate::error::{Error, Result};
use crate::filter::{Args, Filter};
use crate::frame::{AudioData, AudioFrame, Frame};

use super::expect_audio;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Direction {
    In,
    Out,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Curve {
    Tri,
    Qsin,
    Exp,
    Log,
}

impl Curve {
    fn gain(self, x: f64) -> f64 {
        match self {
            Curve::Tri => x,
            Curve::Qsin => (x * std::f64::consts::FRAC_PI_2).sin(),
            Curve::Exp => (10f64.powf(3.0 * x) - 1.0) / 999.0,
            Curve::Log => (1.0 + 999.0 * x).log10() / 3.0,
        }
    }
}

/// Where the fade starts and how long it lasts: either in seconds, which
/// depends on the sample rate of the frames, or directly in samples.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Span {
    Seconds(f64),
    Samples(i64),
}

impl Span {
    fn samples(self, rate: u32) -> i64 {
        match self {
            Span::Seconds(s) => (s * rate as f64).round() as i64,
            Span::Samples(n) => n,
        }
    }
}

pub struct Fade {
    direction: Direction,
    curve: Curve,
    start: Span,
    length: Span,
}

pub fn create(args: &Args) -> Result<Box<dyn Filter>> {
    args.check_known(
        "afade",
        &[
            "type",
            "t",
            "start_time",
            "st",
            "duration",
            "d",
            "start_sample",
            "ss",
            "nb_samples",
            "ns",
            "curve",
            "c",
        ],
    )?;
    let direction = match args.get(&["type", "t"]).map(str::trim) {
        None | Some("in") | Some("0") => Direction::In,
        Some("out") | Some("1") => Direction::Out,
        Some(other) => {
            return Err(Error::invalid_argument(format!(
                "afade type must be in or out, not '{other}'"
            )));
        }
    };
    let curve = match args.get(&["curve", "c"]).map(str::trim) {
        None | Some("tri") => Curve::Tri,
        Some("qsin") => Curve::Qsin,
        Some("exp") => Curve::Exp,
        Some("log") => Curve::Log,
        Some(other) => {
            return Err(Error::invalid_argument(format!(
                "unknown afade curve '{other}' (known: tri, qsin, exp, log)"
            )));
        }
    };
    // A time given in seconds wins over a sample count, as it does not
    // depend on the rate of the stream.
    let start = match (
        args.duration(&["start_time", "st"])?,
        args.i64(&["start_sample", "ss"])?,
    ) {
        (Some(s), _) if s >= 0.0 => Span::Seconds(s),
        (None, Some(n)) if n >= 0 => Span::Samples(n),
        (None, None) => Span::Samples(0),
        _ => return Err(Error::invalid_argument("afade start must not be negative")),
    };
    let length = match (
        args.duration(&["duration", "d"])?,
        args.i64(&["nb_samples", "ns"])?,
    ) {
        (Some(d), _) if d > 0.0 => Span::Seconds(d),
        (None, Some(n)) if n > 0 => Span::Samples(n),
        // One second by default: long enough to be heard as a fade.
        (None, None) => Span::Seconds(1.0),
        _ => return Err(Error::invalid_argument("afade duration must be positive")),
    };
    Ok(Box::new(Fade {
        direction,
        curve,
        start,
        length,
    }))
}

impl Fade {
    /// Gain at absolute sample `i` for a fade over `start..start + len`.
    fn gain(&self, i: i64, start: i64, len: i64) -> f32 {
        let x = ((i - start) as f64 / len as f64).clamp(0.0, 1.0);
        let x = match self.direction {
            Direction::In => x,
            Direction::Out => 1.0 - x,
        };
        self.curve.gain(x) as f32
    }
}

impl Filter for Fade {
    fn filter(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        let a = expect_audio(frame, "afade")?;
        let start = self.start.samples(a.sample_rate);
        let len = self.length.samples(a.sample_rate).max(1);
        let n = a.nb_samples() as i64;
        let (first, last) = (a.pts, a.pts + n);
        // Frames entirely on the unity side of the fade pass untouched,
        // keeping integer samples exact.
        let unity = match self.direction {
            Direction::In => first >= start + len,
            Direction::Out => last <= start,
        };
        if unity {
            return Ok(vec![Frame::Audio(a)]);
        }
        let ch = a.channels as usize;
        let mut samples = a.to_f32();
        for (k, frame) in samples.chunks_exact_mut(ch).enumerate() {
            let g = self.gain(first + k as i64, start, len);
            frame.iter_mut().for_each(|s| *s *= g);
        }
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

    fn fade(spec: &str, frame: AudioFrame) -> Vec<f32> {
        let mut f = create(&Args::parse(spec, &["type"]).unwrap()).unwrap();
        let Some(Frame::Audio(a)) = f.filter(Frame::Audio(frame)).unwrap().pop() else {
            panic!()
        };
        a.to_f32()
    }

    fn ones(n: usize, pts: i64) -> AudioFrame {
        AudioFrame::new(10, 1, AudioData::Float(vec![1.0; n]), pts)
    }

    #[test]
    fn linear_fade_in_and_out() {
        // 10 Hz, fade over one second: ten steps of 0.1.
        let v = fade("t=in:d=1", ones(12, 0));
        let expected = [0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0, 1.0];
        for (a, b) in v.iter().zip(expected) {
            assert!((a - b).abs() < 1e-6);
        }
        let v = fade("t=out:st=0.5:d=1", ones(20, 0));
        assert_eq!(&v[..6], &[1.0; 6]);
        assert!((v[10] - 0.5).abs() < 1e-6);
        assert_eq!(&v[15..], &[0.0; 5]);
    }

    #[test]
    fn position_comes_from_pts() {
        // Second half of the same fade in, delivered as a later frame.
        let v = fade("t=in:d=1", ones(4, 5));
        assert!((v[0] - 0.5).abs() < 1e-6 && (v[3] - 0.8).abs() < 1e-6);
        // Samples given directly.
        let v = fade("t=in:ss=2:ns=4", ones(8, 0));
        assert_eq!(v[..3], [0.0, 0.0, 0.0]);
        assert!((v[4] - 0.5).abs() < 1e-6);
        assert_eq!(v[7], 1.0);
    }

    #[test]
    fn curves_have_the_documented_shape() {
        for curve in [Curve::Tri, Curve::Qsin, Curve::Exp, Curve::Log] {
            assert!(curve.gain(0.0).abs() < 1e-12, "{curve:?}");
            assert!((curve.gain(1.0) - 1.0).abs() < 1e-12, "{curve:?}");
            let mut prev = 0.0;
            for i in 1..=100 {
                let g = curve.gain(i as f64 / 100.0);
                assert!(g > prev, "{curve:?} is not increasing");
                prev = g;
            }
        }
        assert!(Curve::Exp.gain(0.5) < 0.05);
        assert!(Curve::Log.gain(0.5) > 0.85);
        assert!((Curve::Qsin.gain(0.5) - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-12);
        let v = fade("t=out:d=1:curve=exp", ones(11, 0));
        assert!((v[5] as f64 - Curve::Exp.gain(0.5)).abs() < 1e-6);
    }

    #[test]
    fn frames_outside_the_fade_are_untouched() {
        let int = AudioFrame::new(
            10,
            2,
            AudioData::Int {
                samples: vec![5; 8],
                bits: 16,
            },
            100,
        );
        let mut f = create(&Args::parse("t=in:d=1", &["type"]).unwrap()).unwrap();
        assert_eq!(
            f.filter(Frame::Audio(int.clone())).unwrap(),
            vec![Frame::Audio(int)]
        );
    }

    #[test]
    fn rejects_bad_options() {
        for spec in [
            "t=sideways",
            "curve=cubic",
            "d=0",
            "ns=0",
            "st=-1",
            "ss=-5",
            "bogus=1",
            "d=abc",
        ] {
            assert!(
                create(&Args::parse(spec, &["type"]).unwrap()).is_err(),
                "{spec}"
            );
        }
    }
}
