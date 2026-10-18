//! Audio filters.
//!
//! Every filter accepts any sample kind. Filters that change sample
//! values output floats, so a gain or a fade never loses precision to an
//! intermediate integer rounding; the pipeline converts to the encoder's
//! format afterwards. Timestamps follow the audio convention of the crate:
//! `pts` counts samples at the output rate.

mod biquad;
mod fade;
mod format;
mod reverse;
mod tempo;
mod volume;

use crate::error::{Error, Result};
use crate::filter::FilterInfo;
use crate::frame::{AudioData, AudioFrame, Frame};
use crate::stream::MediaType;

pub static FILTERS: &[FilterInfo] = &[
    FilterInfo {
        name: "volume",
        description: "Change the volume by a factor or in dB (volume=0.5, volume=6dB).",
        media: MediaType::Audio,
        positional: &["volume"],
        create: volume::create,
    },
    FilterInfo {
        name: "aresample",
        description: "Resample to another rate with a band-limited filter.",
        media: MediaType::Audio,
        positional: &["sample_rate"],
        create: format::create_aresample,
    },
    FilterInfo {
        name: "aformat",
        description: "Convert to one of the listed sample formats, rates and channel layouts.",
        media: MediaType::Audio,
        positional: &["sample_fmts", "sample_rates", "channel_layouts"],
        create: format::create_aformat,
    },
    FilterInfo {
        name: "afade",
        description: "Fade in or out (t=in|out, st, d, curve=tri|qsin|exp|log).",
        media: MediaType::Audio,
        positional: &["type"],
        create: fade::create,
    },
    FilterInfo {
        name: "lowpass",
        description: "Second order low-pass filter (f, q or width with width_type).",
        media: MediaType::Audio,
        positional: &["frequency"],
        create: biquad::create_lowpass,
    },
    FilterInfo {
        name: "highpass",
        description: "Second order high-pass filter (f, q or width with width_type).",
        media: MediaType::Audio,
        positional: &["frequency"],
        create: biquad::create_highpass,
    },
    FilterInfo {
        name: "atempo",
        description: "Change the speed from 0.5 to 2 times without changing the pitch.",
        media: MediaType::Audio,
        positional: &["tempo"],
        create: tempo::create,
    },
    FilterInfo {
        name: "areverse",
        description: "Reverse the audio (buffers the whole stream).",
        media: MediaType::Audio,
        positional: &[],
        create: reverse::create,
    },
];

/// Samples per channel in frames that filters build from a buffer.
const CHUNK_SAMPLES: usize = 4096;

/// Unwraps an audio frame; a video frame here means the chain was wired
/// to the wrong stream.
fn expect_audio(frame: Frame, filter: &str) -> Result<AudioFrame> {
    match frame {
        Frame::Audio(a) => Ok(a),
        Frame::Video(_) => Err(Error::invalid_data(format!(
            "{filter} received a video frame"
        ))),
    }
}

/// Cuts interleaved float samples into frames of at most
/// [`CHUNK_SAMPLES`] samples per channel with contiguous timestamps.
fn float_frames(samples: &[f32], sample_rate: u32, channels: u16, pts: i64) -> Vec<Frame> {
    let ch = channels as usize;
    samples
        .chunks(CHUNK_SAMPLES * ch)
        .enumerate()
        .map(|(i, chunk)| {
            Frame::Audio(AudioFrame::new(
                sample_rate,
                channels,
                AudioData::Float(chunk.to_vec()),
                pts + (i * CHUNK_SAMPLES) as i64,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::{flush_chain, parse_chain, run_chain};

    #[test]
    fn every_filter_is_reachable_by_name() {
        for spec in [
            "volume=0.5",
            "aresample=48000",
            "aformat=sample_fmts=s16",
            "afade=t=out:st=1:d=1",
            "lowpass=f=1000",
            "highpass=200",
            "atempo=1.5",
            "areverse",
        ] {
            assert_eq!(
                parse_chain(spec, MediaType::Audio).unwrap().len(),
                1,
                "{spec}"
            );
            assert!(parse_chain(spec, MediaType::Video).is_err());
        }
    }

    #[test]
    fn chain_runs_and_flushes() {
        let mut chain = parse_chain(
            "volume=0.5,aresample=16000,atempo=2,areverse",
            MediaType::Audio,
        )
        .unwrap();
        let mut out = Vec::new();
        for i in 0..8 {
            let frame = AudioFrame::new(8000, 1, AudioData::Float(vec![0.5; 1000]), i * 1000);
            out.extend(run_chain(&mut chain, Frame::Audio(frame)).unwrap());
        }
        out.extend(flush_chain(&mut chain).unwrap());
        let mut pts = 0;
        for f in &out {
            let Frame::Audio(a) = f else { panic!() };
            assert_eq!(a.sample_rate, 16000);
            assert_eq!(a.pts, pts);
            pts += a.nb_samples() as i64;
        }
        // 8000 samples at 8 kHz, resampled to 16000, then played twice as
        // fast.
        assert_eq!(pts, 8000);
    }

    #[test]
    fn video_frames_are_rejected() {
        use crate::frame::{PixelFormat, VideoFrame};
        use crate::rational::Rational;
        let mut chain = parse_chain("volume=2", MediaType::Audio).unwrap();
        let v = VideoFrame::new_black(2, 2, PixelFormat::Gray, 0, Rational::new(1, 25));
        assert!(run_chain(&mut chain, Frame::Video(v)).is_err());
    }

    #[test]
    fn float_frames_split_with_contiguous_pts() {
        let frames = float_frames(&vec![0.0; 2 * 5000], 8000, 2, 10);
        assert_eq!(frames.len(), 2);
        let Frame::Audio(b) = &frames[1] else {
            panic!()
        };
        assert_eq!((b.pts, b.nb_samples()), (10 + 4096, 904));
    }
}
