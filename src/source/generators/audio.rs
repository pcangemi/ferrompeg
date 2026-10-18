//! Audio generators: `anullsrc` (silence) and `anoisesrc` (noise).

use std::f64::consts::TAU;

use super::rng::Rng;
use super::{AudioGenerator, SampleSource};
use crate::error::{Error, Result};
use crate::filter::Args;
use crate::source::Source;

/// Most channels `anullsrc` will produce; enough for any speaker layout.
const MAX_CHANNELS: u16 = 64;

pub(crate) fn create_anullsrc(args: &Args) -> Result<Box<dyn Source>> {
    args.check_known(
        "anullsrc",
        &["sample_rate", "r", "channel_layout", "cl", "duration", "d"],
    )?;
    let sample_rate = args.u32(&["sample_rate", "r"])?.unwrap_or(44100);
    let channels = match args.get(&["channel_layout", "cl"]) {
        Some(layout) => parse_layout(layout)?,
        None => 2,
    };
    let duration = args.duration(&["duration", "d"])?;
    Ok(Box::new(AudioGenerator::new(
        sample_rate,
        channels,
        duration,
        Silence,
    )?))
}

/// Channel count of a layout name (`mono`, `stereo`, `5.1`) or of an
/// explicit count (`3`, `3c`).
fn parse_layout(layout: &str) -> Result<u16> {
    let layout = layout.trim();
    let named = match layout {
        "mono" => Some(1),
        "stereo" => Some(2),
        "2.1" | "3.0" => Some(3),
        "quad" | "4.0" => Some(4),
        "5.0" => Some(5),
        "5.1" | "6.0" => Some(6),
        "6.1" | "7.0" => Some(7),
        "7.1" => Some(8),
        _ => None,
    };
    if let Some(n) = named {
        return Ok(n);
    }
    let digits = layout.strip_suffix('c').unwrap_or(layout);
    match digits.parse::<u16>() {
        Ok(n) if (1..=MAX_CHANNELS).contains(&n) => Ok(n),
        _ => Err(Error::invalid_argument(format!(
            "unknown channel layout '{layout}' (use mono, stereo, 2.1, quad, 5.0, 5.1, 7.1 \
             or a count from 1 to {MAX_CHANNELS})"
        ))),
    }
}

struct Silence;

impl SampleSource for Silence {
    fn fill(&mut self, out: &mut [f32]) {
        out.fill(0.0);
    }
}

pub(crate) fn create_anoisesrc(args: &Args) -> Result<Box<dyn Source>> {
    args.check_known(
        "anoisesrc",
        &[
            "sample_rate",
            "r",
            "amplitude",
            "a",
            "color",
            "c",
            "seed",
            "duration",
            "d",
        ],
    )?;
    let sample_rate = args.u32(&["sample_rate", "r"])?.unwrap_or(48000);
    let amplitude = args.f64(&["amplitude", "a"])?.unwrap_or(1.0);
    let color = match args.get(&["color", "c"]) {
        Some(name) => NoiseColor::from_name(name)?,
        None => NoiseColor::White,
    };
    let seed = super::parse_seed(args)?;
    let duration = args.duration(&["duration", "d"])?;
    let noise = Noise::new(color, amplitude, seed, sample_rate)?;
    Ok(Box::new(AudioGenerator::new(
        sample_rate,
        1,
        duration,
        noise,
    )?))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NoiseColor {
    /// Flat spectrum.
    White,
    /// Power falling 3 dB per octave.
    Pink,
    /// Power falling 6 dB per octave (the integral of white noise).
    Brown,
}

impl NoiseColor {
    fn from_name(name: &str) -> Result<Self> {
        match name.trim() {
            "white" => Ok(NoiseColor::White),
            "pink" => Ok(NoiseColor::Pink),
            "brown" | "brownian" => Ok(NoiseColor::Brown),
            other => Err(Error::invalid_argument(format!(
                "unknown noise color '{other}' (use white, pink or brown)"
            ))),
        }
    }
}

/// Number of octave generators in the pink noise sum. Sixteen rows give a
/// 1/f slope over sixteen octaves, from Nyquist down to below 1 Hz at any
/// common sample rate.
const PINK_ROWS: usize = 16;

/// Standard deviation targeted by pink and brown noise at amplitude 1.
/// Both are sums of many random values, so their distribution is close to
/// normal: at this level a sample exceeds full scale (four standard
/// deviations) only about once every 16000 samples, where it is clipped.
const COLORED_RMS: f64 = 0.25;

/// Corner frequency of the brown noise integrator. A pure integrator would
/// wander off without bound; leaking below this frequency keeps the signal
/// centered while leaving the audible band with the 1/f^2 slope.
const BROWN_CORNER_HZ: f64 = 10.0;

struct Noise {
    color: NoiseColor,
    amplitude: f64,
    rng: Rng,
    /// Pink: the octave rows, their running sum, and the sample counter
    /// whose trailing zero count picks which row to refresh.
    rows: [f64; PINK_ROWS],
    row_sum: f64,
    counter: u64,
    pink_gain: f64,
    /// Brown: integrator state, leak factor and input scale.
    level: f64,
    leak: f64,
    step: f64,
}

impl Noise {
    fn new(color: NoiseColor, amplitude: f64, seed: u64, sample_rate: u32) -> Result<Self> {
        if !(0.0..=1.0).contains(&amplitude) {
            return Err(Error::invalid_argument(
                "noise amplitude must be between 0 and 1",
            ));
        }
        let mut rng = Rng::new(seed);
        // Start every row with a value so the first samples already have
        // the full spectrum instead of fading in.
        let rows: [f64; PINK_ROWS] = std::array::from_fn(|_| rng.next_signed());
        let row_sum = rows.iter().sum();
        // Rows plus one fresh white value, each uniform with variance 1/3.
        let terms = (PINK_ROWS + 1) as f64;
        let pink_gain = COLORED_RMS / (terms / 3.0).sqrt();
        // One pole leaky integrator y += x - (1 - leak) y. Its output
        // variance for white input of variance 1/3 is step^2 / 3 / (1 -
        // leak^2), which fixes `step` for the target level. Very low sample
        // rates put the corner above Nyquist; the leak then bottoms out and
        // the output is simply white.
        let leak = (1.0 - TAU * BROWN_CORNER_HZ / sample_rate.max(1) as f64).max(0.0);
        let step = COLORED_RMS * (3.0 * (1.0 - leak * leak)).sqrt();
        Ok(Noise {
            color,
            amplitude,
            rng,
            rows,
            row_sum,
            counter: 0,
            pink_gain,
            level: 0.0,
            leak,
            step,
        })
    }

    fn next(&mut self) -> f64 {
        let white = self.rng.next_signed();
        let v = match self.color {
            NoiseColor::White => white,
            NoiseColor::Pink => {
                // Voss's scheme: row k changes every 2^k samples, so each
                // row contributes white noise low passed one octave below
                // the previous one and the sum approximates 1/f. Picking
                // the row from the trailing zeros of a counter refreshes
                // exactly one row per sample, keeping the cost constant.
                self.counter = self.counter.wrapping_add(1);
                let k = self.counter.trailing_zeros() as usize;
                if k < PINK_ROWS {
                    let fresh = self.rng.next_signed();
                    self.row_sum += fresh - self.rows[k];
                    self.rows[k] = fresh;
                }
                (self.row_sum + white) * self.pink_gain
            }
            NoiseColor::Brown => {
                self.level = self.level * self.leak + white * self.step;
                self.level
            }
        };
        v.clamp(-1.0, 1.0) * self.amplitude
    }
}

impl SampleSource for Noise {
    fn fill(&mut self, out: &mut [f32]) {
        for s in out {
            *s = self.next() as f32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Frame;
    use crate::source::open;

    fn collect(spec: &str) -> Vec<f32> {
        let mut s = open(spec).unwrap();
        let mut out = Vec::new();
        while let Some(Frame::Audio(f)) = s.next_frame().unwrap() {
            out.extend(f.to_f32());
        }
        out
    }

    fn noise(color: NoiseColor, seed: u64, n: usize) -> Vec<f64> {
        let mut g = Noise::new(color, 1.0, seed, 48000).unwrap();
        (0..n).map(|_| g.next()).collect()
    }

    /// Ratio of the power of the first difference to the power of the
    /// signal: 2 for white noise, smaller the more the spectrum leans to
    /// low frequencies.
    fn difference_ratio(v: &[f64]) -> f64 {
        let mean = v.iter().sum::<f64>() / v.len() as f64;
        let power: f64 = v.iter().map(|x| (x - mean).powi(2)).sum();
        let diff: f64 = v.windows(2).map(|w| (w[1] - w[0]).powi(2)).sum();
        diff / power
    }

    #[test]
    fn layouts() {
        assert_eq!(parse_layout("mono").unwrap(), 1);
        assert_eq!(parse_layout("stereo").unwrap(), 2);
        assert_eq!(parse_layout("5.1").unwrap(), 6);
        assert_eq!(parse_layout("7.1").unwrap(), 8);
        assert_eq!(parse_layout("3").unwrap(), 3);
        assert_eq!(parse_layout("12c").unwrap(), 12);
        assert!(parse_layout("0").is_err());
        assert!(parse_layout("65").is_err());
        assert!(parse_layout("surround").is_err());
    }

    #[test]
    fn silence_is_silent() {
        let mut s = open("anullsrc=cl=5.1:r=8000:d=0.2").unwrap();
        let a = *s.stream().audio().unwrap();
        assert_eq!(
            (a.sample_rate, a.channels, a.bits_per_sample),
            (8000, 6, 32)
        );
        let mut total = 0;
        while let Some(Frame::Audio(f)) = s.next_frame().unwrap() {
            assert_eq!(f.channels, 6);
            assert!(f.to_f32().iter().all(|&v| v == 0.0));
            total += f.nb_samples();
        }
        assert_eq!(total, 1600);
        // Positional order: layout, then rate.
        let s = open("anullsrc=mono:22050").unwrap();
        let a = *s.stream().audio().unwrap();
        assert_eq!((a.sample_rate, a.channels), (22050, 1));
        assert_eq!(s.stream().duration, None);
    }

    #[test]
    fn noise_is_deterministic_and_seeded() {
        let a = collect("anoisesrc=d=0.1:c=pink:seed=5");
        let b = collect("anoisesrc=d=0.1:c=pink:seed=5");
        let c = collect("anoisesrc=d=0.1:c=pink:seed=6");
        assert_eq!(a.len(), 4800);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(
            collect("anoisesrc=d=0.01:seed=-1"),
            collect("anoisesrc=d=0.01:seed=18446744073709551615")
        );
    }

    #[test]
    fn noise_respects_amplitude() {
        for color in ["white", "pink", "brown"] {
            let v = collect(&format!("anoisesrc=d=1:c={color}:a=0.3"));
            let peak = v.iter().fold(0.0f32, |m, x| m.max(x.abs()));
            assert!(peak <= 0.3, "{color}: peak {peak}");
            assert!(peak > 0.15, "{color}: peak {peak}");
            // Pink and brown noise carry most of their power at low
            // frequencies, so their mean over one second wanders more.
            let mean = v.iter().map(|&x| x as f64).sum::<f64>() / v.len() as f64;
            assert!(mean.abs() < 0.06, "{color}: mean {mean}");
        }
        assert!(collect("anoisesrc=d=0.1:a=0").iter().all(|&v| v == 0.0));
    }

    #[test]
    fn noise_colors_have_the_right_slope() {
        let n = 1 << 17;
        let white = difference_ratio(&noise(NoiseColor::White, 1, n));
        let pink = difference_ratio(&noise(NoiseColor::Pink, 1, n));
        let brown = difference_ratio(&noise(NoiseColor::Brown, 1, n));
        assert!((white - 2.0).abs() < 0.05, "white {white}");
        assert!(pink < 0.5 * white, "pink {pink}");
        assert!(brown < 0.05, "brown {brown}");
        assert!(brown < pink);
    }

    #[test]
    fn colored_noise_level_is_near_target() {
        for color in [NoiseColor::Pink, NoiseColor::Brown] {
            let v = noise(color, 3, 1 << 18);
            let rms = (v.iter().map(|x| x * x).sum::<f64>() / v.len() as f64).sqrt();
            assert!((rms - COLORED_RMS).abs() < 0.05, "{color:?}: rms {rms}");
        }
    }

    #[test]
    fn rejects_bad_options() {
        assert!(open("anoisesrc=a=1.5").is_err());
        assert!(open("anoisesrc=c=purple").is_err());
        assert!(open("anoisesrc=r=0").is_err());
        assert!(open("anullsrc=cl=bogus").is_err());
        assert!(open("anullsrc=r=1000000").is_err());
        assert!(open("anoisesrc=48000:0.5:c=brown").is_ok());
        // Absurdly low rates still work: the integrator degrades to white.
        assert!(open("anoisesrc=r=1:c=brown:d=5").is_ok());
    }
}
