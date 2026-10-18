//! Low-pass and high-pass filters: `lowpass=f=1000`, `highpass=f=200:q=1`.
//!
//! The two pole filters use the formulas of R. Bristow-Johnson's "Cookbook
//! formulae for audio EQ biquad filter coefficients" (the Audio EQ
//! Cookbook): with `w0 = 2 pi f / fs` and `alpha` derived from the width,
//!
//! ```text
//! low-pass:  b = [(1 - cos w0) / 2,  1 - cos w0,    (1 - cos w0) / 2]
//! high-pass: b = [(1 + cos w0) / 2, -(1 + cos w0),  (1 + cos w0) / 2]
//! both:      a = [1 + alpha, -2 cos w0, 1 - alpha]
//! ```
//!
//! The width is a Q factor by default (0.707, the maximally flat
//! Butterworth response), or a bandwidth in Hz (`width_type=h`, converted
//! to `Q = f / bandwidth`) or in octaves (`width_type=o`, with the
//! cookbook's `alpha = sin w0 sinh(ln 2 / 2 * BW * w0 / sin w0)`).
//!
//! `poles=1` selects a first order filter instead, the bilinear transform
//! of an RC section with its cutoff prewarped so it lands exactly on `f`.
//!
//! The filters run in direct form I with double precision state, which
//! stays accurate for low cutoffs where the poles sit close to the unit
//! circle.

use crate::error::{Error, Result};
use crate::filter::{Args, Filter};
use crate::frame::{AudioData, AudioFrame, Frame};

use super::expect_audio;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Lowpass,
    Highpass,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Width {
    Q(f64),
    Hertz(f64),
    Octaves(f64),
}

pub struct Biquad {
    kind: Kind,
    frequency: f64,
    width: Width,
    poles: u32,
    /// Coefficients normalized by a0, for the rate in `configured`.
    b: [f64; 3],
    a: [f64; 2],
    configured: Option<(u32, u16)>,
    /// Per channel: last two inputs, then last two outputs.
    state: Vec<[f64; 4]>,
}

pub fn create_lowpass(args: &Args) -> Result<Box<dyn Filter>> {
    create(args, Kind::Lowpass, "lowpass", 500.0)
}

pub fn create_highpass(args: &Args) -> Result<Box<dyn Filter>> {
    create(args, Kind::Highpass, "highpass", 3000.0)
}

fn create(args: &Args, kind: Kind, name: &str, default_frequency: f64) -> Result<Box<dyn Filter>> {
    args.check_known(
        name,
        &[
            "frequency",
            "f",
            "width_type",
            "t",
            "width",
            "w",
            "q",
            "poles",
            "p",
        ],
    )?;
    let frequency = args.f64(&["frequency", "f"])?.unwrap_or(default_frequency);
    if frequency <= 0.0 {
        return Err(Error::invalid_argument(format!(
            "{name} frequency must be positive"
        )));
    }
    let width = match args.f64(&["q"])? {
        Some(q) => Width::Q(q),
        None => {
            let w = args.f64(&["width", "w"])?;
            match args.get(&["width_type", "t"]).map(str::trim) {
                None | Some("q") => Width::Q(w.unwrap_or(std::f64::consts::FRAC_1_SQRT_2)),
                Some("h") => Width::Hertz(w.unwrap_or(frequency)),
                Some("o") => Width::Octaves(w.unwrap_or(1.0)),
                Some(other) => {
                    return Err(Error::invalid_argument(format!(
                        "{name} width_type must be q, h or o, not '{other}'"
                    )));
                }
            }
        }
    };
    let w = match width {
        Width::Q(v) | Width::Hertz(v) | Width::Octaves(v) => v,
    };
    if w <= 0.0 {
        return Err(Error::invalid_argument(format!(
            "{name} width must be positive"
        )));
    }
    let poles = args.u32(&["poles", "p"])?.unwrap_or(2);
    if !(1..=2).contains(&poles) {
        return Err(Error::invalid_argument(format!(
            "{name} poles must be 1 or 2"
        )));
    }
    Ok(Box::new(Biquad {
        kind,
        frequency,
        width,
        poles,
        b: [1.0, 0.0, 0.0],
        a: [0.0, 0.0],
        configured: None,
        state: Vec::new(),
    }))
}

impl Biquad {
    fn configure(&mut self, rate: u32, channels: u16) -> Result<()> {
        let nyquist = rate as f64 / 2.0;
        if self.frequency >= nyquist {
            return Err(Error::invalid_argument(format!(
                "cutoff {} Hz is not below the Nyquist frequency of a {rate} Hz stream",
                self.frequency
            )));
        }
        let w0 = std::f64::consts::TAU * self.frequency / rate as f64;
        let (sin, cos) = w0.sin_cos();
        if self.poles == 1 {
            let k = (w0 / 2.0).tan();
            let a1 = (k - 1.0) / (k + 1.0);
            self.b = match self.kind {
                Kind::Lowpass => [k / (1.0 + k), k / (1.0 + k), 0.0],
                Kind::Highpass => [1.0 / (1.0 + k), -1.0 / (1.0 + k), 0.0],
            };
            self.a = [a1, 0.0];
        } else {
            let alpha = match self.width {
                Width::Q(q) => sin / (2.0 * q),
                Width::Hertz(bw) => sin / (2.0 * (self.frequency / bw)),
                Width::Octaves(bw) => sin * ((2f64.ln() / 2.0) * bw * w0 / sin).sinh(),
            };
            let b = match self.kind {
                Kind::Lowpass => [(1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0],
                Kind::Highpass => [(1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0],
            };
            let a0 = 1.0 + alpha;
            self.b = [b[0] / a0, b[1] / a0, b[2] / a0];
            self.a = [-2.0 * cos / a0, (1.0 - alpha) / a0];
        }
        self.state = vec![[0.0; 4]; channels as usize];
        self.configured = Some((rate, channels));
        Ok(())
    }
}

impl Filter for Biquad {
    fn filter(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        let a = expect_audio(frame, "biquad")?;
        // A new rate or channel count needs new coefficients; the history
        // of the old stream does not apply to it.
        if self.configured != Some((a.sample_rate, a.channels)) {
            self.configure(a.sample_rate, a.channels)?;
        }
        let ch = a.channels as usize;
        let mut samples = a.to_f32();
        let ([b0, b1, b2], [a1, a2]) = (self.b, self.a);
        for frame in samples.chunks_exact_mut(ch) {
            for (s, st) in frame.iter_mut().zip(self.state.iter_mut()) {
                let x = *s as f64;
                let y = b0 * x + b1 * st[0] + b2 * st[1] - a1 * st[2] - a2 * st[3];
                *st = [x, st[0], y, st[2]];
                *s = y as f32;
            }
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

    fn make(name: &str, spec: &str) -> Box<dyn Filter> {
        let args = Args::parse(spec, &["frequency"]).unwrap();
        match name {
            "lowpass" => create_lowpass(&args).unwrap(),
            _ => create_highpass(&args).unwrap(),
        }
    }

    /// Steady state amplitude of a sine at `freq` through the filter.
    fn response(name: &str, spec: &str, freq: f64) -> f64 {
        let rate = 48000;
        let mut f = make(name, spec);
        let n = 48000;
        let input: Vec<f32> = (0..n)
            .map(|i| (std::f64::consts::TAU * freq * i as f64 / rate as f64).sin() as f32)
            .collect();
        let frame = AudioFrame::new(rate, 1, AudioData::Float(input), 0);
        let Some(Frame::Audio(a)) = f.filter(Frame::Audio(frame)).unwrap().pop() else {
            panic!()
        };
        let out = a.to_f32();
        // RMS over the second half, after the transient, as an amplitude.
        let tail = &out[n / 2..];
        let power = tail.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / tail.len() as f64;
        (2.0 * power).sqrt()
    }

    fn db(x: f64) -> f64 {
        20.0 * x.log10()
    }

    #[test]
    fn butterworth_lowpass_is_3db_down_at_cutoff() {
        assert!((db(response("lowpass", "f=1000", 1000.0)) + 3.01).abs() < 0.05);
        assert!(db(response("lowpass", "f=1000", 100.0)).abs() < 0.01);
        // Second order: about 12 dB per octave above the cutoff (a bit
        // more near Nyquist, where the bilinear transform compresses the
        // frequency axis).
        let r4 = db(response("lowpass", "f=1000", 4000.0));
        let r8 = db(response("lowpass", "f=1000", 8000.0));
        assert!(r4 < -23.0 && (r4 - r8 - 12.0).abs() < 1.5, "{r4} {r8}");
    }

    #[test]
    fn highpass_mirrors_lowpass() {
        assert!((db(response("highpass", "f=1000", 1000.0)) + 3.01).abs() < 0.05);
        assert!(db(response("highpass", "f=1000", 10000.0)).abs() < 0.05);
        assert!(db(response("highpass", "f=1000", 125.0)) < -35.0);
    }

    #[test]
    fn q_shapes_the_peak() {
        // At the cutoff the gain of these filters equals Q.
        assert!((response("lowpass", "f=1000:q=2", 1000.0) - 2.0).abs() < 0.01);
        assert!((response("lowpass", "f=1000:w=2", 1000.0) - 2.0).abs() < 0.01);
        // 1000 Hz wide at 1000 Hz is Q = 1.
        assert!((response("lowpass", "f=1000:t=h:w=1000", 1000.0) - 1.0).abs() < 0.01);
        let octave = response("lowpass", "f=1000:t=o:w=1", 1000.0);
        assert!((octave - 1.414).abs() < 0.02, "{octave}");
    }

    #[test]
    fn first_order_is_3db_down_and_6db_per_octave() {
        assert!((db(response("lowpass", "f=1000:p=1", 1000.0)) + 3.01).abs() < 0.05);
        let r4 = db(response("lowpass", "f=1000:p=1", 4000.0));
        let r8 = db(response("lowpass", "f=1000:p=1", 8000.0));
        assert!((r4 - r8 - 6.0).abs() < 1.0, "{r4} {r8}");
        assert!((db(response("highpass", "f=1000:p=1", 1000.0)) + 3.01).abs() < 0.05);
    }

    #[test]
    fn dc_passes_lowpass_and_not_highpass() {
        let frame = || AudioFrame::new(8000, 2, AudioData::Float(vec![0.5; 2 * 8000]), 0);
        for (name, expected) in [("lowpass", 0.5), ("highpass", 0.0)] {
            let mut f = make(name, "f=100");
            let Some(Frame::Audio(a)) = f.filter(Frame::Audio(frame())).unwrap().pop() else {
                panic!()
            };
            let v = a.to_f32();
            assert!((v[v.len() - 1] - expected).abs() < 1e-4, "{name}");
            assert!((v[v.len() - 2] - expected).abs() < 1e-4, "{name}");
        }
    }

    #[test]
    fn state_carries_across_frames() {
        let input: Vec<f32> = (0..1000).map(|i| ((i * 37) % 100) as f32 / 100.0).collect();
        let whole = {
            let mut f = make("lowpass", "f=500");
            let frame = AudioFrame::new(8000, 1, AudioData::Float(input.clone()), 0);
            let Some(Frame::Audio(a)) = f.filter(Frame::Audio(frame)).unwrap().pop() else {
                panic!()
            };
            a.to_f32()
        };
        let mut f = make("lowpass", "f=500");
        let mut split = Vec::new();
        for (i, c) in input.chunks(77).enumerate() {
            let frame = AudioFrame::new(8000, 1, AudioData::Float(c.to_vec()), i as i64 * 77);
            let Some(Frame::Audio(a)) = f.filter(Frame::Audio(frame)).unwrap().pop() else {
                panic!()
            };
            split.extend(a.to_f32());
        }
        assert_eq!(whole, split);
    }

    #[test]
    fn rejects_bad_options() {
        for spec in ["f=0", "f=-5", "w=0", "q=-1", "t=s", "p=3", "gain=2"] {
            assert!(
                create_lowpass(&Args::parse(spec, &["frequency"]).unwrap()).is_err(),
                "{spec}"
            );
        }
        // A cutoff above Nyquist is only known to be wrong once the rate is.
        let mut f = make("lowpass", "f=5000");
        let frame = AudioFrame::new(8000, 1, AudioData::Float(vec![0.0; 4]), 0);
        assert!(f.filter(Frame::Audio(frame)).is_err());
    }
}
