//! Constant frame rate conversion.
//!
//! The output time line is a grid of slots, one per output frame. Each
//! input frame starts at the slot nearest to its timestamp and fills every
//! slot up to the next frame's: frames are duplicated when the output rate
//! is higher and dropped when several land on the same slot (the latest
//! wins). The last frame fills the slots up to the end of its own display
//! time, so the output never extends past the input.

use super::video_input;
use crate::error::{Error, Result};
use crate::filter::{Args, Filter};
use crate::frame::{Frame, VideoFrame};
use crate::rational::Rational;

/// Highest output rate accepted.
const MAX_RATE: f64 = 1_000_000.0;
/// Most bytes of duplicated pictures one call may return; a timestamp jump
/// that needs more is reported instead of exhausting memory.
const MAX_OUTPUT_BYTES: usize = 1 << 30;
/// Most frames one call may return, whatever their size, since every frame
/// carries its own allocations.
const MAX_OUTPUT_FRAMES: usize = 1 << 16;

pub fn create(args: &Args) -> Result<Box<dyn Filter>> {
    args.check_known("fps", &["fps"])?;
    let rate = args.rational(&["fps"])?.unwrap_or(Rational::new(25, 1));
    if rate.num <= 0 || rate.to_f64() > MAX_RATE {
        return Err(Error::invalid_argument(format!(
            "fps: rate {rate} is outside 0 to {MAX_RATE}"
        )));
    }
    Ok(Box::new(Fps::new(rate)))
}

struct Fps {
    /// Output time base, one output frame.
    time_base: Rational,
    /// The latest frame, not yet emitted for its slots.
    pending: Option<VideoFrame>,
    /// First slot not emitted yet, set by the first frame.
    next: Option<i64>,
    /// Slot where the pending frame's display time ends.
    end: i64,
}

impl Fps {
    fn new(rate: Rational) -> Self {
        Fps {
            time_base: rate.inv(),
            pending: None,
            next: None,
            end: 0,
        }
    }

    /// Emits the pending frame for every slot before `until`.
    fn emit_until(&mut self, until: i64) -> Result<Vec<Frame>> {
        let (Some(frame), Some(next)) = (self.pending.as_ref(), self.next) else {
            return Ok(Vec::new());
        };
        if until <= next {
            return Ok(Vec::new());
        }
        // i128: both ends come from input timestamps and may be far apart.
        let count = until as i128 - next as i128;
        let bytes: usize = frame.planes.iter().map(Vec::len).sum();
        let limit = (MAX_OUTPUT_BYTES / bytes.max(1)).clamp(1, MAX_OUTPUT_FRAMES) as i128;
        if count > limit {
            return Err(Error::invalid_data(format!(
                "fps: a timestamp gap would repeat one picture {count} times"
            )));
        }
        let out = (next..until)
            .map(|slot| {
                let mut f = frame.clone();
                f.pts = slot;
                f.time_base = self.time_base;
                Frame::Video(f)
            })
            .collect();
        self.next = Some(until);
        Ok(out)
    }
}

impl Filter for Fps {
    fn filter(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        let v = video_input(frame, "fps")?;
        let tb = v.time_base;
        if tb.num <= 0 || tb.den <= 0 {
            return Err(Error::invalid_data(format!(
                "fps: invalid input time base {tb}"
            )));
        }
        // Rescaling rounds to nearest, which is the slot choice.
        let slot = Rational::rescale(v.pts, tb, self.time_base);
        let end = Rational::rescale(v.pts.saturating_add(1), tb, self.time_base);
        let out = self.emit_until(slot)?;
        if self.next.is_none() {
            self.next = Some(slot);
        }
        self.pending = Some(v);
        self.end = end;
        Ok(out)
    }

    fn flush(&mut self) -> Result<Vec<Frame>> {
        let out = self.emit_until(self.end)?;
        self.pending = None;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_util::{pattern, video};
    use super::*;
    use crate::filter::{flush_chain, parse_chain, run_chain};
    use crate::frame::PixelFormat;
    use crate::stream::MediaType;

    /// Runs `n` frames at `rate` (pts from `start`) through `spec` and
    /// returns the output pts and the input index each output came from.
    fn convert(spec: &str, rate: Rational, start: i64, n: i64) -> Vec<(i64, u8)> {
        let mut chain = parse_chain(spec, MediaType::Video).unwrap();
        let mut out = Vec::new();
        for i in 0..n {
            let mut f = pattern(4, 2, PixelFormat::Gray);
            f.pts = start + i;
            f.time_base = rate.inv();
            f.planes[0][0] = i as u8;
            out.extend(run_chain(&mut chain, Frame::Video(f)).unwrap());
        }
        out.extend(flush_chain(&mut chain).unwrap());
        video(out).iter().map(|f| (f.pts, f.planes[0][0])).collect()
    }

    #[test]
    fn same_rate_passes_every_frame() {
        let out = convert("fps=25", Rational::new(25, 1), 0, 5);
        assert_eq!(out, vec![(0, 0), (1, 1), (2, 2), (3, 3), (4, 4)]);
    }

    #[test]
    fn halving_drops_every_other_frame() {
        // 50 to 25: input i is at slot round(i / 2), the later one wins.
        let out = convert("fps=25", Rational::new(50, 1), 0, 6);
        assert_eq!(out, vec![(0, 0), (1, 2), (2, 4)]);
    }

    #[test]
    fn doubling_duplicates() {
        let out = convert("fps=50", Rational::new(25, 1), 0, 3);
        assert_eq!(out, vec![(0, 0), (1, 0), (2, 1), (3, 1), (4, 2), (5, 2)]);
    }

    #[test]
    fn one_second_to_one_fps_gives_one_frame() {
        let out = convert("fps=1", Rational::new(25, 1), 0, 25);
        assert_eq!(out, vec![(0, 12)]);
        let out = convert("fps=1", Rational::new(25, 1), 0, 50);
        assert_eq!(out, vec![(0, 12), (1, 37)]);
    }

    #[test]
    fn ntsc_rates() {
        // 25 to 30000/1001 for 2 seconds: 60 slots minus rounding.
        let out = convert("fps=ntsc", Rational::new(25, 1), 0, 50);
        assert_eq!(out.len(), 60);
        assert!(out.windows(2).all(|w| w[1].0 == w[0].0 + 1));
        let out = convert("fps=30000/1001", Rational::new(25, 1), 0, 50);
        assert_eq!(out.len(), 60);
        // The pipeline spells -r as num/den.
        let out = convert("fps=24000/1001", Rational::new(24000, 1001), 0, 10);
        assert_eq!(out.len(), 10);
    }

    #[test]
    fn output_starts_at_the_first_frame() {
        let out = convert("fps=10", Rational::new(10, 1), 100, 2);
        assert_eq!(out, vec![(100, 0), (101, 1)]);
        let out = convert("fps=20", Rational::new(10, 1), 5, 1);
        assert_eq!(out, vec![(10, 0), (11, 0)]);
    }

    #[test]
    fn output_time_base_is_the_rate() {
        let mut chain = parse_chain("fps=30000/1001", MediaType::Video).unwrap();
        let f = pattern(4, 2, PixelFormat::Yuv420p);
        run_chain(&mut chain, Frame::Video(f)).unwrap();
        let out = video(flush_chain(&mut chain).unwrap());
        assert!(
            out.iter()
                .all(|f| f.time_base == Rational::new(1001, 30000))
        );
    }

    #[test]
    fn gaps_and_bad_input() {
        let mut fps = Fps::new(Rational::new(25, 1));
        let mut f = pattern(4, 2, PixelFormat::Gray);
        f.pts = 0;
        fps.filter(Frame::Video(f.clone())).unwrap();
        // A jump of several seconds duplicates the held frame.
        f.pts = 100;
        assert_eq!(fps.filter(Frame::Video(f.clone())).unwrap().len(), 100);
        // A jump of a billion frames is refused.
        f.pts = 1_000_000_000_000;
        assert!(fps.filter(Frame::Video(f.clone())).is_err());
        // Tiny pictures are capped by count, not only by bytes.
        let mut fps = Fps::new(Rational::new(25, 1));
        let mut tiny = VideoFrame::new_black(1, 1, PixelFormat::Gray, 0, Rational::new(1, 25));
        fps.filter(Frame::Video(tiny.clone())).unwrap();
        tiny.pts = 5_000_000;
        assert!(fps.filter(Frame::Video(tiny.clone())).is_err());
        // Extreme timestamps do not overflow.
        let mut fps = Fps::new(Rational::new(25, 1));
        tiny.pts = i64::MIN / 2;
        fps.filter(Frame::Video(tiny.clone())).unwrap();
        tiny.pts = i64::MAX / 2;
        assert!(fps.filter(Frame::Video(tiny.clone())).is_err());
        // Going backwards only replaces the held frame.
        let mut fps = Fps::new(Rational::new(25, 1));
        f.pts = 10;
        fps.filter(Frame::Video(f.clone())).unwrap();
        f.pts = 5;
        assert!(fps.filter(Frame::Video(f.clone())).unwrap().is_empty());
        let mut bad = f.clone();
        bad.time_base = Rational { num: 0, den: 1 };
        assert!(fps.filter(Frame::Video(bad)).is_err());
        assert!(parse_chain("fps=0", MediaType::Video).is_err());
        assert!(parse_chain("fps=-5", MediaType::Video).is_err());
        assert!(parse_chain("fps=abc", MediaType::Video).is_err());
        assert!(parse_chain("fps=10000000", MediaType::Video).is_err());
        // Nothing in, nothing out.
        assert!(Fps::new(Rational::new(25, 1)).flush().unwrap().is_empty());
    }
}
