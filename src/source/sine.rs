//! Pure tone generator.

use crate::error::{Error, Result};
use crate::filter::Args;
use crate::frame::{AudioData, AudioFrame, Frame};
use crate::rational::Rational;
use crate::source::{Source, SourceInfo, units_for};
use crate::stream::{AudioParams, CodecId, MediaType, StreamInfo, StreamParams};

pub const INFO: SourceInfo = SourceInfo {
    name: "sine",
    description: "Generate a sine wave (options: frequency, sample_rate, duration, amplitude).",
    media: MediaType::Audio,
    positional: &["frequency"],
    create,
};

const FRAME_SAMPLES: u64 = 1024;

pub struct Sine {
    stream: StreamInfo,
    frequency: f64,
    amplitude: f32,
    sample_rate: u32,
    total: Option<u64>,
    produced: u64,
}

fn create(args: &Args) -> Result<Box<dyn Source>> {
    args.check_known(
        "sine",
        &[
            "frequency",
            "f",
            "sample_rate",
            "r",
            "duration",
            "d",
            "amplitude",
            "a",
        ],
    )?;
    let frequency = args.f64(&["frequency", "f"])?.unwrap_or(440.0);
    let sample_rate = args.u32(&["sample_rate", "r"])?.unwrap_or(44100);
    // Default level matches the common generator convention of one eighth
    // of full scale, loud enough to hear without being harsh.
    let amplitude = args.f64(&["amplitude", "a"])?.unwrap_or(0.125);
    let duration = args.duration(&["duration", "d"])?;
    Ok(Box::new(Sine::new(
        frequency,
        sample_rate,
        amplitude as f32,
        duration,
    )?))
}

impl Sine {
    pub fn new(
        frequency: f64,
        sample_rate: u32,
        amplitude: f32,
        duration: Option<f64>,
    ) -> Result<Self> {
        if sample_rate == 0 || sample_rate > 768_000 {
            return Err(Error::invalid_argument(
                "sine sample rate must be 1 to 768000",
            ));
        }
        if !(0.0..=sample_rate as f64 / 2.0).contains(&frequency) {
            return Err(Error::invalid_argument(
                "sine frequency must be between 0 and half the sample rate",
            ));
        }
        if !(0.0..=1.0).contains(&amplitude) {
            return Err(Error::invalid_argument(
                "sine amplitude must be between 0 and 1",
            ));
        }
        let total = units_for(duration, sample_rate as f64);
        let time_base = Rational::new(1, sample_rate as i64);
        let mut stream = StreamInfo::new(
            0,
            CodecId::PcmF32Le,
            StreamParams::Audio(AudioParams {
                sample_rate,
                channels: 1,
                bits_per_sample: 32,
            }),
            time_base,
        );
        stream.duration = total.map(|t| t as i64);
        Ok(Sine {
            stream,
            frequency,
            amplitude,
            sample_rate,
            total,
            produced: 0,
        })
    }
}

impl Source for Sine {
    fn stream(&self) -> &StreamInfo {
        &self.stream
    }

    fn next_frame(&mut self) -> Result<Option<Frame>> {
        let n = match self.total {
            Some(total) => FRAME_SAMPLES.min(total - self.produced),
            None => FRAME_SAMPLES,
        };
        if n == 0 {
            return Ok(None);
        }
        // Phase from the absolute sample index, not an accumulator, so a
        // long tone never drifts.
        let step = std::f64::consts::TAU * self.frequency / self.sample_rate as f64;
        let samples = (self.produced..self.produced + n)
            .map(|i| ((i as f64 * step).sin() as f32) * self.amplitude)
            .collect();
        let frame = AudioFrame::new(
            self.sample_rate,
            1,
            AudioData::Float(samples),
            self.produced as i64,
        );
        self.produced += n;
        Ok(Some(Frame::Audio(frame)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produces_the_requested_length() {
        let mut s = Sine::new(1000.0, 8000, 0.5, Some(0.3)).unwrap();
        let mut total = 0;
        let mut expected_pts = 0;
        while let Some(Frame::Audio(f)) = s.next_frame().unwrap() {
            assert_eq!(f.pts, expected_pts);
            expected_pts += f.nb_samples() as i64;
            total += f.nb_samples();
        }
        assert_eq!(total, 2400);
    }

    #[test]
    fn waveform_is_right() {
        // 2 kHz at 8 kHz: four samples per period, 0, 1, 0, -1.
        let mut s = Sine::new(2000.0, 8000, 1.0, Some(0.001)).unwrap();
        let Some(Frame::Audio(f)) = s.next_frame().unwrap() else {
            panic!()
        };
        let v = f.to_f32();
        assert!(v[0].abs() < 1e-6 && (v[1] - 1.0).abs() < 1e-6 && (v[3] + 1.0).abs() < 1e-6);
    }

    #[test]
    fn rejects_bad_options() {
        assert!(Sine::new(5000.0, 8000, 0.5, None).is_err());
        assert!(crate::source::open("sine=f=440:bogus=1").is_err());
        assert!(crate::source::open("sine=440:d=1").is_ok());
    }
}
