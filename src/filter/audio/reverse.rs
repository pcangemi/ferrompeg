//! Time reversal: `areverse`.
//!
//! The last sample has to come out first, so the whole stream is held in
//! memory until the end and released by `flush`. Samples keep their kind:
//! reversing does not change values, so integer input stays bit exact.
//! Frames are reversed as a whole, keeping the channels of each sample
//! instant together.

use crate::convert::sample::convert_kind;
use crate::error::{Error, Result};
use crate::filter::{Args, Filter};
use crate::frame::{AudioData, AudioFrame, Frame};

use super::{CHUNK_SAMPLES, expect_audio};

pub struct Areverse {
    buffer: Option<Buffer>,
}

struct Buffer {
    sample_rate: u32,
    channels: u16,
    pts: i64,
    data: AudioData,
}

pub fn create(args: &Args) -> Result<Box<dyn Filter>> {
    args.check_known("areverse", &[])?;
    Ok(Box::new(Areverse { buffer: None }))
}

impl Filter for Areverse {
    fn filter(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        let a = expect_audio(frame, "areverse")?;
        let Some(buf) = self.buffer.as_mut() else {
            self.buffer = Some(Buffer {
                sample_rate: a.sample_rate,
                channels: a.channels,
                pts: a.pts,
                data: a.data,
            });
            return Ok(Vec::new());
        };
        if a.sample_rate != buf.sample_rate || a.channels != buf.channels {
            return Err(Error::unsupported(
                "areverse input changing sample rate or channel count mid-stream",
            ));
        }
        // A later frame in another kind joins the buffer converted to the
        // kind of the first one.
        let data = if a.data.kind() == buf.data.kind() {
            a.data
        } else {
            convert_kind(&a.data, buf.data.kind())
        };
        match (&mut buf.data, data) {
            (AudioData::Int { samples, .. }, AudioData::Int { samples: more, .. }) => {
                samples.extend(more)
            }
            (AudioData::Float(samples), AudioData::Float(more)) => samples.extend(more),
            _ => return Err(Error::invalid_data("areverse sample kind mismatch")),
        }
        Ok(Vec::new())
    }

    fn flush(&mut self) -> Result<Vec<Frame>> {
        let Some(buf) = self.buffer.take() else {
            return Ok(Vec::new());
        };
        let ch = buf.channels as usize;
        let chunk = CHUNK_SAMPLES * ch;
        let frame_at = |i: usize, data: AudioData| {
            Frame::Audio(AudioFrame::new(
                buf.sample_rate,
                buf.channels,
                data,
                buf.pts + (i * CHUNK_SAMPLES) as i64,
            ))
        };
        Ok(match buf.data {
            AudioData::Int { samples, bits } => reversed(&samples, ch)
                .chunks(chunk)
                .enumerate()
                .map(|(i, c)| {
                    frame_at(
                        i,
                        AudioData::Int {
                            samples: c.to_vec(),
                            bits,
                        },
                    )
                })
                .collect(),
            AudioData::Float(samples) => reversed(&samples, ch)
                .chunks(chunk)
                .enumerate()
                .map(|(i, c)| frame_at(i, AudioData::Float(c.to_vec())))
                .collect(),
        })
    }
}

/// Reverses the order of frames of `ch` interleaved samples.
fn reversed<T: Copy>(samples: &[T], ch: usize) -> Vec<T> {
    samples.chunks_exact(ch).rev().flatten().copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(frames: Vec<AudioFrame>) -> Vec<AudioFrame> {
        let mut f = create(&Args::default()).unwrap();
        for frame in frames {
            assert!(f.filter(Frame::Audio(frame)).unwrap().is_empty());
        }
        f.flush()
            .unwrap()
            .into_iter()
            .map(|f| match f {
                Frame::Audio(a) => a,
                Frame::Video(_) => panic!(),
            })
            .collect()
    }

    #[test]
    fn reverses_frames_keeping_channels_together() {
        let a = AudioFrame::new(8000, 2, AudioData::Float(vec![1.0, -1.0, 2.0, -2.0]), 7);
        let b = AudioFrame::new(8000, 2, AudioData::Float(vec![3.0, -3.0]), 9);
        let out = feed(vec![a, b]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pts, 7);
        assert_eq!(
            out[0].data,
            AudioData::Float(vec![3.0, -3.0, 2.0, -2.0, 1.0, -1.0])
        );
    }

    #[test]
    fn integer_samples_stay_exact_and_mixed_kinds_convert() {
        let a = AudioFrame::new(
            8000,
            1,
            AudioData::Int {
                samples: vec![1, 2],
                bits: 16,
            },
            0,
        );
        let b = AudioFrame::new(8000, 1, AudioData::Float(vec![0.5]), 2);
        let out = feed(vec![a, b]);
        assert_eq!(
            out[0].data,
            AudioData::Int {
                samples: vec![16384, 2, 1],
                bits: 16
            }
        );
    }

    #[test]
    fn long_streams_come_out_in_chunks_with_contiguous_pts() {
        let n = CHUNK_SAMPLES * 2 + 10;
        let samples: Vec<f32> = (0..n).map(|i| i as f32).collect();
        let out = feed(vec![AudioFrame::new(100, 1, AudioData::Float(samples), 50)]);
        assert_eq!(out.len(), 3);
        let mut pts = 50;
        let mut all = Vec::new();
        for f in &out {
            assert_eq!(f.pts, pts);
            pts += f.nb_samples() as i64;
            all.extend(f.to_f32());
        }
        assert_eq!(all.len(), n);
        assert_eq!(all[0], (n - 1) as f32);
        assert_eq!(all[n - 1], 0.0);
    }

    #[test]
    fn empty_stream_and_format_changes() {
        assert!(feed(Vec::new()).is_empty());
        let mut f = create(&Args::default()).unwrap();
        let a = AudioFrame::new(8000, 1, AudioData::Float(vec![0.0]), 0);
        let b = AudioFrame::new(8000, 2, AudioData::Float(vec![0.0, 0.0]), 1);
        f.filter(Frame::Audio(a)).unwrap();
        assert!(f.filter(Frame::Audio(b)).is_err());
        assert!(create(&Args::parse("x=1", &[]).unwrap()).is_err());
    }
}
