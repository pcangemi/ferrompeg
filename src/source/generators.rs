//! Test signal generators beyond the sine tone: picture test patterns,
//! a solid color, a Mandelbrot zoom, the Game of Life, silence and noise.
//!
//! Every generator is deterministic: the same options always produce the
//! same bytes, which makes them usable as reference inputs in tests.
//!
//! References:
//!
//! * SMPTE EG 1-1990, "Alignment Color Bar Test Signal for Television
//!   Picture Monitors", for the layout and levels of `smptebars`;
//! * W3C CSS Color Module Level 4, section 6.1 "Named Colors", for the
//!   color names accepted by `color`;
//! * B. Mandelbrot, "Fractal aspects of the iteration of z -> lambda z (1-z)
//!   for complex lambda and z" (1980), and A. Douady and J. H. Hubbard,
//!   "Iteration des polynomes quadratiques complexes" (1982), whose
//!   potential function gives the smooth escape count of `mandelbrot`;
//!   R. P. Brent, "An improved Monte Carlo factorization algorithm" (1980),
//!   for the cycle detection that skips interior points early;
//! * M. Gardner, "Mathematical Games", Scientific American, October 1970,
//!   for the rules of Conway's Game of Life, and April 1978 for R. F. Voss's
//!   1/f noise algorithm used by the pink noise of `anoisesrc`;
//! * G. L. Steele, D. Lea and C. H. Flood, "Fast splittable pseudorandom
//!   number generators" (2014), and D. Blackman and S. Vigna, "Scrambled
//!   linear pseudorandom number generators" (2018), for the generators
//!   behind every seeded source.
//!
//! Video sources produce rgb24 frames at a constant rate (time base of one
//! frame, pts counting frames); audio sources produce float frames of 1024
//! samples with pts counting samples, as described in [`crate::frame`].

mod audio;
mod draw;
mod life;
mod mandelbrot;
mod pattern;
mod rng;

use crate::error::{Error, Result};
use crate::filter::Args;
use crate::frame::{AudioData, AudioFrame, Frame, PixelFormat, VideoFrame};
use crate::rational::Rational;
use crate::source::{Source, SourceInfo, units_for};
use crate::stream::{AudioParams, CodecId, MediaType, StreamInfo, StreamParams, VideoParams};

pub static SOURCES: &[SourceInfo] = &[
    SourceInfo {
        name: "testsrc",
        description: "Generate a test pattern with color bars, a moving disc and a frame counter \
                      (options: size, rate, duration).",
        media: MediaType::Video,
        positional: &["size"],
        create: pattern::create_testsrc,
    },
    SourceInfo {
        name: "smptebars",
        description: "Generate SMPTE color bars (options: size, rate, duration).",
        media: MediaType::Video,
        positional: &["size"],
        create: pattern::create_smptebars,
    },
    SourceInfo {
        name: "color",
        description: "Generate a solid color (options: color, size, rate, duration).",
        media: MediaType::Video,
        positional: &["color", "size"],
        create: pattern::create_color,
    },
    SourceInfo {
        name: "mandelbrot",
        description: "Generate a zoom into the Mandelbrot set (options: size, rate, duration, \
                      maxiter, start_x, start_y, start_scale, zoom).",
        media: MediaType::Video,
        positional: &["size"],
        create: mandelbrot::create,
    },
    SourceInfo {
        name: "life",
        description: "Generate Conway's Game of Life (options: size, rate, duration, seed, ratio).",
        media: MediaType::Video,
        positional: &["size"],
        create: life::create,
    },
    SourceInfo {
        name: "anullsrc",
        description: "Generate silence (options: sample_rate, channel_layout, duration).",
        media: MediaType::Audio,
        positional: &["channel_layout", "sample_rate"],
        create: audio::create_anullsrc,
    },
    SourceInfo {
        name: "anoisesrc",
        description: "Generate white, pink or brown noise (options: sample_rate, amplitude, \
                      color, seed, duration).",
        media: MediaType::Audio,
        positional: &["sample_rate", "amplitude"],
        create: audio::create_anoisesrc,
    },
];

/// Largest picture a generator accepts, a little above 8K UHD. One rgb24
/// frame of this size is about 100 MB, which bounds the allocation a
/// command line can trigger.
const MAX_PIXELS: u64 = 1 << 25;

/// Highest frame rate accepted; far above any display, low enough that a
/// typo such as `r=25000` is caught.
const MAX_FRAME_RATE: f64 = 1000.0;

/// Option names shared by every video generator.
const VIDEO_KEYS: &[&str] = &["size", "s", "rate", "r", "duration", "d"];

/// Picture size, frame rate and length common to every video generator.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct VideoOptions {
    pub width: u32,
    pub height: u32,
    pub rate: Rational,
    /// Number of frames, `None` for an endless source.
    pub total: Option<u64>,
}

impl VideoOptions {
    pub fn new(width: u32, height: u32, rate: Rational, duration: Option<f64>) -> Result<Self> {
        if width == 0 || height == 0 || width as u64 * height as u64 > MAX_PIXELS {
            return Err(Error::invalid_argument(format!(
                "picture size {width}x{height} is out of range (at most {MAX_PIXELS} pixels)"
            )));
        }
        if rate.num <= 0 || rate.den <= 0 || rate.to_f64() > MAX_FRAME_RATE {
            return Err(Error::invalid_argument(format!(
                "frame rate {rate} is out of range (0 to {MAX_FRAME_RATE})"
            )));
        }
        Ok(VideoOptions {
            width,
            height,
            rate,
            total: units_for(duration, rate.to_f64()),
        })
    }

    /// Reads the shared options, checking that every option given is one
    /// of them or one of `extra`.
    pub fn parse(args: &Args, name: &str, extra: &[&str]) -> Result<Self> {
        let known: Vec<&str> = VIDEO_KEYS.iter().chain(extra).copied().collect();
        args.check_known(name, &known)?;
        let (width, height) = args.size(&["size", "s"])?.unwrap_or((320, 240));
        let rate = args
            .rational(&["rate", "r"])?
            .unwrap_or(Rational::new(25, 1));
        let duration = args.duration(&["duration", "d"])?;
        VideoOptions::new(width, height, rate, duration)
    }

    /// Time of frame `index` in seconds.
    pub fn seconds(&self, index: u64) -> f64 {
        index as f64 * self.rate.den as f64 / self.rate.num as f64
    }
}

/// Reads the `seed` option. Any 64-bit value is accepted, written either
/// unsigned or signed (a negative seed is reinterpreted as its two's
/// complement), so a seed printed by any tool can be given back.
pub(crate) fn parse_seed(args: &Args) -> Result<u64> {
    match args.get(&["seed"]) {
        None => Ok(0),
        Some(v) => {
            let v = v.trim();
            v.parse::<u64>()
                .or_else(|_| v.parse::<i64>().map(|s| s as u64))
                .map_err(|_| Error::invalid_argument(format!("invalid value '{v}' for 'seed'")))
        }
    }
}

/// Draws one picture of a video generator.
pub(crate) trait Painter {
    /// Fills `pixels`, a tightly packed rgb24 picture of the size given at
    /// construction, with frame number `index`. Frames are requested in
    /// order, starting at zero, so painters may keep state between calls.
    fn paint(&mut self, index: u64, pixels: &mut [u8]);
}

/// Turns a [`Painter`] into a [`Source`] of rgb24 frames.
pub(crate) struct VideoGenerator<P> {
    stream: StreamInfo,
    options: VideoOptions,
    produced: u64,
    painter: P,
}

impl<P: Painter> VideoGenerator<P> {
    pub fn new(options: VideoOptions, painter: P) -> Self {
        let time_base = options.rate.inv();
        let mut stream = StreamInfo::new(
            0,
            CodecId::RawVideo,
            StreamParams::Video(VideoParams {
                width: options.width,
                height: options.height,
                pixel_format: PixelFormat::Rgb24,
                frame_rate: options.rate,
                sample_aspect_ratio: Rational::new(1, 1),
            }),
            time_base,
        );
        stream.duration = options.total.map(|t| t as i64);
        VideoGenerator {
            stream,
            options,
            produced: 0,
            painter,
        }
    }
}

impl<P: Painter> Source for VideoGenerator<P> {
    fn stream(&self) -> &StreamInfo {
        &self.stream
    }

    fn next_frame(&mut self) -> Result<Option<Frame>> {
        if self.options.total.is_some_and(|t| self.produced >= t) {
            return Ok(None);
        }
        let mut frame = VideoFrame::new_black(
            self.options.width,
            self.options.height,
            PixelFormat::Rgb24,
            self.produced as i64,
            self.stream.time_base,
        );
        self.painter.paint(self.produced, &mut frame.planes[0]);
        self.produced += 1;
        Ok(Some(Frame::Video(frame)))
    }
}

/// Samples per audio frame, the same block size as the sine source.
const FRAME_SAMPLES: u64 = 1024;

/// Highest sample rate accepted, matching the sine source.
const MAX_SAMPLE_RATE: u32 = 768_000;

/// Produces the samples of an audio generator.
pub(crate) trait SampleSource {
    /// Fills `out` with interleaved samples, continuing where the previous
    /// call stopped.
    fn fill(&mut self, out: &mut [f32]);
}

/// Turns a [`SampleSource`] into a [`Source`] of float frames.
pub(crate) struct AudioGenerator<S> {
    stream: StreamInfo,
    sample_rate: u32,
    channels: u16,
    total: Option<u64>,
    produced: u64,
    samples: S,
}

impl<S: SampleSource> AudioGenerator<S> {
    pub fn new(sample_rate: u32, channels: u16, duration: Option<f64>, samples: S) -> Result<Self> {
        if sample_rate == 0 || sample_rate > MAX_SAMPLE_RATE {
            return Err(Error::invalid_argument(format!(
                "sample rate must be 1 to {MAX_SAMPLE_RATE}"
            )));
        }
        if channels == 0 {
            return Err(Error::invalid_argument(
                "a source needs at least one channel",
            ));
        }
        let total = units_for(duration, sample_rate as f64);
        let mut stream = StreamInfo::new(
            0,
            CodecId::PcmF32Le,
            StreamParams::Audio(AudioParams {
                sample_rate,
                channels,
                bits_per_sample: 32,
            }),
            Rational::new(1, sample_rate as i64),
        );
        stream.duration = total.map(|t| t as i64);
        Ok(AudioGenerator {
            stream,
            sample_rate,
            channels,
            total,
            produced: 0,
            samples,
        })
    }
}

impl<S: SampleSource> Source for AudioGenerator<S> {
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
        let mut data = vec![0.0f32; n as usize * self.channels as usize];
        self.samples.fill(&mut data);
        let frame = AudioFrame::new(
            self.sample_rate,
            self.channels,
            AudioData::Float(data),
            self.produced as i64,
        );
        self.produced += n;
        Ok(Some(Frame::Audio(frame)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::open;

    struct Gray;

    impl Painter for Gray {
        fn paint(&mut self, index: u64, pixels: &mut [u8]) {
            pixels.fill(index as u8);
        }
    }

    #[test]
    fn video_generator_counts_frames() {
        let options = VideoOptions::new(4, 3, Rational::new(10, 1), Some(0.75)).unwrap();
        assert_eq!(options.total, Some(8));
        let mut g = VideoGenerator::new(options, Gray);
        assert_eq!(g.stream().duration, Some(8));
        assert_eq!(g.stream().time_base, Rational::new(1, 10));
        let mut count = 0;
        while let Some(Frame::Video(f)) = g.next_frame().unwrap() {
            f.validate().unwrap();
            assert_eq!(f.format, PixelFormat::Rgb24);
            assert_eq!(f.pts, count);
            assert_eq!(f.time_base, Rational::new(1, 10));
            assert!(f.planes[0].iter().all(|&b| b == count as u8));
            count += 1;
        }
        assert_eq!(count, 8);
    }

    #[test]
    fn video_options_defaults_and_limits() {
        let args = Args::parse("", &[]).unwrap();
        let o = VideoOptions::parse(&args, "x", &[]).unwrap();
        assert_eq!(
            (o.width, o.height, o.rate, o.total),
            (320, 240, Rational::new(25, 1), None)
        );
        let args = Args::parse("s=64x48:r=30000/1001:d=2", &[]).unwrap();
        let o = VideoOptions::parse(&args, "x", &[]).unwrap();
        assert_eq!((o.width, o.height), (64, 48));
        assert_eq!(o.total, Some(60));
        assert!((o.seconds(30) - 1.001).abs() < 1e-9);
        assert!(VideoOptions::new(16384, 16384, Rational::new(25, 1), None).is_err());
        assert!(VideoOptions::new(0, 10, Rational::new(25, 1), None).is_err());
        assert!(VideoOptions::new(10, 10, Rational::new(5000, 1), None).is_err());
        let args = Args::parse("size=10x10:bogus=1", &[]).unwrap();
        assert!(VideoOptions::parse(&args, "x", &[]).is_err());
    }

    #[test]
    fn every_source_opens_with_defaults() {
        for info in SOURCES {
            let mut s = open(&format!("{}=d=0.1", info.name)).unwrap();
            assert_eq!(s.stream().media_type(), info.media, "{}", info.name);
            let frame = s.next_frame().unwrap().unwrap();
            match (&frame, info.media) {
                (Frame::Video(v), MediaType::Video) => {
                    v.validate().unwrap();
                    assert_eq!((v.width, v.height), (320, 240));
                    assert_eq!(s.stream().codec, CodecId::RawVideo);
                }
                (Frame::Audio(a), MediaType::Audio) => {
                    assert_eq!(a.data.kind(), crate::frame::SampleKind::Float);
                    assert_eq!(s.stream().codec, CodecId::PcmF32Le);
                }
                _ => panic!("{} produced the wrong media", info.name),
            }
        }
    }

    #[test]
    fn video_sources_honor_duration() {
        for name in ["testsrc", "smptebars", "color", "mandelbrot", "life"] {
            let mut s = open(&format!("{name}=s=32x24:r=10:d=0.5")).unwrap();
            assert_eq!(s.stream().duration, Some(5), "{name}");
            let mut n = 0;
            while let Some(f) = s.next_frame().unwrap() {
                assert!((f.time_seconds() - n as f64 / 10.0).abs() < 1e-12);
                n += 1;
            }
            assert_eq!(n, 5, "{name}");
        }
    }

    #[test]
    fn audio_sources_honor_duration() {
        for name in ["anullsrc", "anoisesrc"] {
            let mut s = open(&format!("{name}=r=8000:d=0.3")).unwrap();
            assert_eq!(s.stream().duration, Some(2400));
            let mut total = 0;
            let mut sizes = Vec::new();
            while let Some(Frame::Audio(f)) = s.next_frame().unwrap() {
                assert_eq!(f.pts, total as i64);
                total += f.nb_samples();
                sizes.push(f.nb_samples());
            }
            assert_eq!(total, 2400, "{name}");
            assert_eq!(sizes, vec![1024, 1024, 352]);
        }
    }

    #[test]
    fn unknown_options_are_rejected() {
        for info in SOURCES {
            assert!(
                open(&format!("{}=bogus=1", info.name)).is_err(),
                "{}",
                info.name
            );
        }
    }
}
