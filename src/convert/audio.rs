//! Converts a stream of audio frames to a fixed target format.

use crate::convert::remix::remix;
use crate::convert::resample::Resampler;
use crate::convert::sample::convert_kind;
use crate::error::Result;
use crate::frame::{AudioData, AudioFormat, AudioFrame, SampleKind};
use crate::rational::Rational;

/// Stateful converter: resampling keeps a history of past samples, so one
/// converter must see the whole stream in order.
pub struct AudioConverter {
    target: AudioFormat,
    resampler: Option<Resampler>,
    /// Output position of the next sample, set from the first frame.
    next_pts: Option<i64>,
}

impl AudioConverter {
    pub fn new(target: AudioFormat) -> Self {
        AudioConverter {
            target,
            resampler: None,
            next_pts: None,
        }
    }

    pub fn target(&self) -> AudioFormat {
        self.target
    }

    pub fn convert(&mut self, frame: AudioFrame) -> Result<Vec<AudioFrame>> {
        let source = frame.format();
        if source == self.target {
            return Ok(vec![frame]);
        }
        let start = *self.next_pts.get_or_insert_with(|| {
            Rational::rescale(
                frame.pts,
                Rational::new(1, source.sample_rate as i64),
                Rational::new(1, self.target.sample_rate as i64),
            )
        });

        // Only the sample kind differs: convert directly, which keeps
        // integer to integer conversions exact.
        if source.sample_rate == self.target.sample_rate && source.channels == self.target.channels
        {
            let data = convert_kind(&frame.data, self.target.kind);
            let out = AudioFrame::new(self.target.sample_rate, self.target.channels, data, start);
            self.next_pts = Some(start + out.nb_samples() as i64);
            return Ok(vec![out]);
        }

        let mut samples = frame.to_f32();
        if source.channels != self.target.channels {
            samples = remix(&samples, source.channels, self.target.channels)?;
        }
        if source.sample_rate != self.target.sample_rate {
            if self.resampler.is_none() {
                self.resampler = Some(Resampler::new(
                    source.sample_rate,
                    self.target.sample_rate,
                    self.target.channels,
                )?);
            }
            samples = self.resampler.as_mut().unwrap().process(&samples);
        }
        Ok(self.emit(samples, start))
    }

    /// Drains the resampler's delay line at the end of the stream.
    pub fn flush(&mut self) -> Result<Vec<AudioFrame>> {
        let Some(resampler) = self.resampler.as_mut() else {
            return Ok(Vec::new());
        };
        let samples = resampler.flush();
        let start = self.next_pts.unwrap_or(0);
        Ok(self.emit(samples, start))
    }

    fn emit(&mut self, samples: Vec<f32>, start: i64) -> Vec<AudioFrame> {
        if samples.is_empty() {
            return Vec::new();
        }
        let data = match self.target.kind {
            SampleKind::Float => AudioData::Float(samples),
            kind => convert_kind(&AudioData::Float(samples), kind),
        };
        let out = AudioFrame::new(self.target.sample_rate, self.target.channels, data, start);
        self.next_pts = Some(start + out.nb_samples() as i64);
        vec![out]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_only_conversion_keeps_timing() {
        let target = AudioFormat {
            sample_rate: 8000,
            channels: 1,
            kind: SampleKind::Int(16),
        };
        let mut conv = AudioConverter::new(target);
        let frame = AudioFrame::new(8000, 1, AudioData::Float(vec![0.5; 10]), 100);
        let out = conv.convert(frame).unwrap();
        assert_eq!(out[0].pts, 100);
        assert_eq!(out[0].format(), target);
        let frame = AudioFrame::new(8000, 1, AudioData::Float(vec![0.5; 10]), 110);
        assert_eq!(conv.convert(frame).unwrap()[0].pts, 110);
    }

    #[test]
    fn matching_format_passes_through() {
        let frame = AudioFrame::new(8000, 2, AudioData::Float(vec![0.1; 8]), 0);
        let mut conv = AudioConverter::new(frame.format());
        assert_eq!(conv.convert(frame.clone()).unwrap(), vec![frame]);
    }
}
