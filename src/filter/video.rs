//! Video filters.
//!
//! Every filter accepts the five pixel formats and keeps the input format,
//! except `format`, whose job is to change it. Geometry options take small
//! arithmetic expressions over the input and output sizes (see [`expr`]).
//! Filters that need a different layout internally (for example `transpose`
//! on 4:2:2 chroma) convert there and back.

mod adjust;
mod expr;
mod fps;
mod geometry;

use crate::convert::video::{self as vconv, MAX_DIMENSION, ScaleAlgorithm};
use crate::error::{Error, Result};
use crate::filter::{Args, Filter, FilterInfo};
use crate::frame::{Frame, PixelFormat, VideoFrame};
use crate::stream::MediaType;

use expr::{Expr, Var};

pub static FILTERS: &[FilterInfo] = &[
    FilterInfo {
        name: "scale",
        description: "Resize the video to w x h (expressions over iw and ih; -1 or -2 keep the aspect ratio).",
        media: MediaType::Video,
        positional: &["w", "h"],
        create: create_scale,
    },
    FilterInfo {
        name: "crop",
        description: "Keep a w x h area at x, y (centered by default).",
        media: MediaType::Video,
        positional: &["w", "h", "x", "y"],
        create: geometry::create_crop,
    },
    FilterInfo {
        name: "pad",
        description: "Place the video at x, y on a w x h canvas of the given color.",
        media: MediaType::Video,
        positional: &["w", "h", "x", "y", "color"],
        create: geometry::create_pad,
    },
    FilterInfo {
        name: "hflip",
        description: "Mirror the video horizontally.",
        media: MediaType::Video,
        positional: &[],
        create: geometry::create_hflip,
    },
    FilterInfo {
        name: "vflip",
        description: "Flip the video upside down.",
        media: MediaType::Video,
        positional: &[],
        create: geometry::create_vflip,
    },
    FilterInfo {
        name: "transpose",
        description: "Rotate the video by 90 degrees (dir: cclock_flip, clock, cclock, clock_flip or 0 to 3).",
        media: MediaType::Video,
        positional: &["dir"],
        create: geometry::create_transpose,
    },
    FilterInfo {
        name: "format",
        description: "Convert to the first pixel format of a |-separated list.",
        media: MediaType::Video,
        positional: &["pix_fmts"],
        create: create_format,
    },
    FilterInfo {
        name: "fps",
        description: "Convert to a constant frame rate by dropping or duplicating frames.",
        media: MediaType::Video,
        positional: &["fps"],
        create: fps::create,
    },
    FilterInfo {
        name: "negate",
        description: "Invert every component of the picture.",
        media: MediaType::Video,
        positional: &[],
        create: adjust::create_negate,
    },
    FilterInfo {
        name: "eq",
        description: "Adjust contrast, brightness, saturation and gamma.",
        media: MediaType::Video,
        positional: &["contrast", "brightness", "saturation", "gamma"],
        create: adjust::create_eq,
    },
    FilterInfo {
        name: "boxblur",
        description: "Blur with a box of the given luma radius, applied luma_power times.",
        media: MediaType::Video,
        positional: &["luma_radius", "luma_power", "chroma_radius", "chroma_power"],
        create: adjust::create_boxblur,
    },
];

/// Every variable a geometry expression may use.
const SIZE_VARS: &[Var] = &[Var::InW, Var::InH, Var::OutW, Var::OutH, Var::Aspect];

/// Unwraps a video frame and checks its planes, so the filters below can
/// index them without further checks.
fn video_input(frame: Frame, filter: &str) -> Result<VideoFrame> {
    match frame {
        Frame::Video(v) => {
            v.validate()?;
            if v.width == 0 || v.height == 0 || v.width > MAX_DIMENSION || v.height > MAX_DIMENSION
            {
                return Err(Error::invalid_data(format!(
                    "{filter}: unsupported picture size {}x{}",
                    v.width, v.height
                )));
            }
            Ok(v)
        }
        Frame::Audio(_) => Err(Error::invalid_data(format!(
            "{filter} received an audio frame"
        ))),
    }
}

/// Checks a computed output size, already truncated to integers.
fn output_size(filter: &str, w: f64, h: f64) -> Result<(u32, u32)> {
    let max = MAX_DIMENSION as f64;
    if !(1.0..=max).contains(&w) || !(1.0..=max).contains(&h) {
        return Err(Error::invalid_argument(format!(
            "{filter}: output size {w}x{h} is outside 1x1 to {MAX_DIMENSION}x{MAX_DIMENSION}"
        )));
    }
    Ok((w as u32, h as u32))
}

/// Bytes per sample of `plane`: packed RGB stores three per pixel.
fn sample_bytes(format: PixelFormat, plane: usize) -> usize {
    if format == PixelFormat::Rgb24 && plane == 0 {
        3
    } else {
        1
    }
}

/// Subsampling shifts of `plane`: zero for luma and packed planes.
fn plane_shift(format: PixelFormat, plane: usize) -> (u32, u32) {
    if plane == 0 {
        (0, 0)
    } else {
        format.chroma_shift()
    }
}

fn parse_expr(args: &Args, keys: &[&str], default: &str) -> Result<Expr> {
    Expr::parse(args.get(keys).unwrap_or(default).trim(), SIZE_VARS)
}

struct Scale {
    w: Expr,
    h: Expr,
    algorithm: ScaleAlgorithm,
    /// Last input size and the output size computed for it.
    cached: Option<((u32, u32), (u32, u32))>,
}

fn create_scale(args: &Args) -> Result<Box<dyn Filter>> {
    args.check_known(
        "scale",
        &["w", "width", "h", "height", "flags", "s", "size"],
    )?;
    let (w, h) = match args.size(&["s", "size"])? {
        Some((w, h)) => (
            Expr::parse(&w.to_string(), SIZE_VARS)?,
            Expr::parse(&h.to_string(), SIZE_VARS)?,
        ),
        None => (
            parse_expr(args, &["w", "width"], "iw")?,
            parse_expr(args, &["h", "height"], "ih")?,
        ),
    };
    let algorithm = parse_flags(args.get(&["flags"]).unwrap_or(""))?;
    Ok(Box::new(Scale {
        w,
        h,
        algorithm,
        cached: None,
    }))
}

/// Picks the scaling algorithm out of a `+`-separated flag list. A few
/// accuracy flags that common command lines add are accepted and have no
/// effect, since the conversion is always computed at full precision.
fn parse_flags(flags: &str) -> Result<ScaleAlgorithm> {
    let mut algorithm = ScaleAlgorithm::default();
    for flag in flags.split('+').map(str::trim).filter(|f| !f.is_empty()) {
        match flag {
            "accurate_rnd" | "full_chroma_int" | "full_chroma_inp" | "bitexact" => {}
            name => algorithm = ScaleAlgorithm::from_name(name)?,
        }
    }
    Ok(algorithm)
}

impl Scale {
    fn output_size(&self, iw: u32, ih: u32) -> Result<(u32, u32)> {
        let (w, h) = expr::solve_size(&self.w, &self.h, iw, ih)?;
        let (w, h) = (w.trunc(), h.trunc());
        // Zero means the input size; a negative value -n derives the size
        // from the other one, keeping the aspect ratio, rounded to the
        // nearest multiple of n.
        let or_input = |v: f64, input: u32| if v == 0.0 { input as f64 } else { v };
        let keep = |other: f64, num: u32, den: u32, n: f64| {
            ((other * num as f64 / den as f64 / n).round() * n).max(n)
        };
        let (w, h) = match (w < 0.0, h < 0.0) {
            (true, true) => (iw as f64, ih as f64),
            (true, false) => {
                let h = or_input(h, ih);
                (keep(h, iw, ih, -w), h)
            }
            (false, true) => {
                let w = or_input(w, iw);
                (w, keep(w, ih, iw, -h))
            }
            (false, false) => (or_input(w, iw), or_input(h, ih)),
        };
        output_size("scale", w, h)
    }
}

impl Filter for Scale {
    fn filter(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        let v = video_input(frame, "scale")?;
        let input = (v.width, v.height);
        let (w, h) = match self.cached {
            Some((size, out)) if size == input => out,
            _ => {
                let out = self.output_size(v.width, v.height)?;
                self.cached = Some((input, out));
                out
            }
        };
        let out = vconv::convert(&v, w, h, v.format, self.algorithm)?;
        Ok(vec![Frame::Video(out)])
    }
}

struct Format {
    target: PixelFormat,
}

fn create_format(args: &Args) -> Result<Box<dyn Filter>> {
    args.check_known("format", &["pix_fmts"])?;
    let list = args
        .get(&["pix_fmts"])
        .ok_or_else(|| Error::invalid_argument("format needs a pixel format"))?;
    let mut formats = Vec::new();
    for name in list.split('|').map(str::trim).filter(|n| !n.is_empty()) {
        formats.push(PixelFormat::from_name(name)?);
    }
    let target = *formats
        .first()
        .ok_or_else(|| Error::invalid_argument("format needs a pixel format"))?;
    Ok(Box::new(Format { target }))
}

impl Filter for Format {
    fn filter(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        let v = video_input(frame, "format")?;
        let out = vconv::convert(
            &v,
            v.width,
            v.height,
            self.target,
            ScaleAlgorithm::default(),
        )?;
        Ok(vec![Frame::Video(out)])
    }
}

#[cfg(test)]
mod test_util {
    use crate::frame::{Frame, PixelFormat, VideoFrame};
    use crate::rational::Rational;

    /// A picture where every sample differs from its neighbors.
    pub fn pattern(width: u32, height: u32, format: PixelFormat) -> VideoFrame {
        let mut f = VideoFrame::new_black(width, height, format, 3, Rational::new(1, 25));
        for (p, plane) in f.planes.iter_mut().enumerate() {
            for (i, v) in plane.iter_mut().enumerate() {
                let base = if format.is_yuv() { 16 } else { 0 };
                *v = base + ((i * 7 + p * 50) % 200) as u8;
            }
        }
        f
    }

    pub fn video(frames: Vec<Frame>) -> Vec<VideoFrame> {
        frames
            .into_iter()
            .map(|f| match f {
                Frame::Video(v) => v,
                Frame::Audio(_) => panic!("audio frame"),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::{pattern, video};
    use super::*;
    use crate::filter::parse_chain;
    use crate::frame::{AudioData, AudioFrame};

    fn run(spec: &str, frame: VideoFrame) -> Result<VideoFrame> {
        let mut chain = parse_chain(spec, MediaType::Video)?;
        let mut out = crate::filter::run_chain(&mut chain, Frame::Video(frame))?;
        out.extend(crate::filter::flush_chain(&mut chain)?);
        assert_eq!(out.len(), 1);
        Ok(video(out).remove(0))
    }

    fn size_of(spec: &str, w: u32, h: u32) -> Result<(u32, u32)> {
        let out = run(spec, pattern(w, h, PixelFormat::Gray))?;
        Ok((out.width, out.height))
    }

    #[test]
    fn registry_entries_are_complete() {
        for name in [
            "scale",
            "crop",
            "pad",
            "hflip",
            "vflip",
            "transpose",
            "format",
            "fps",
            "negate",
            "eq",
            "boxblur",
        ] {
            let info = FILTERS.iter().find(|f| f.name == name).unwrap();
            assert_eq!(info.media, MediaType::Video);
            assert!(!info.description.is_empty());
        }
        // Video filters refuse audio chains and audio frames.
        assert!(parse_chain("hflip", MediaType::Audio).is_err());
        let mut chain = parse_chain("scale=2:2", MediaType::Video).unwrap();
        let audio = Frame::Audio(AudioFrame::new(8000, 1, AudioData::Float(vec![0.0]), 0));
        assert!(crate::filter::run_chain(&mut chain, audio).is_err());
    }

    #[test]
    fn scale_sizes() {
        assert_eq!(size_of("scale=160:120", 320, 240).unwrap(), (160, 120));
        assert_eq!(size_of("scale=iw/2:ih/2", 320, 240).unwrap(), (160, 120));
        assert_eq!(
            size_of("scale=w=iw*0.33:h=ih/7", 320, 240).unwrap(),
            (105, 34)
        );
        assert_eq!(size_of("scale=-1:101", 320, 240).unwrap(), (135, 101));
        assert_eq!(size_of("scale=-2:101", 320, 240).unwrap(), (134, 101));
        assert_eq!(size_of("scale=-2:103", 320, 240).unwrap(), (138, 103));
        assert_eq!(size_of("scale=-4:101", 320, 240).unwrap(), (136, 101));
        assert_eq!(size_of("scale=iw/3:-1", 320, 240).unwrap(), (106, 80));
        assert_eq!(size_of("scale=0:50", 320, 240).unwrap(), (320, 50));
        assert_eq!(size_of("scale=-1:-1", 320, 240).unwrap(), (320, 240));
        assert_eq!(size_of("scale", 320, 240).unwrap(), (320, 240));
        assert_eq!(size_of("scale=s=64x32", 320, 240).unwrap(), (64, 32));
        assert_eq!(size_of("scale=oh*2:100", 320, 240).unwrap(), (200, 100));
        assert_eq!(size_of("scale='(iw+10)/2':ih", 30, 20).unwrap(), (20, 20));
    }

    #[test]
    fn scale_rejects_bad_options() {
        assert!(parse_chain("scale=foo:2", MediaType::Video).is_err());
        assert!(parse_chain("scale=2:2:flags=sinc", MediaType::Video).is_err());
        assert!(parse_chain("scale=2:2:bogus=1", MediaType::Video).is_err());
        assert!(parse_chain("scale=2:2:flags=bicubic+accurate_rnd", MediaType::Video).is_ok());
        assert!(size_of("scale=iw*1000:10", 320, 240).is_err());
        assert!(size_of("scale=-100000:10", 320, 240).is_err());
        assert!(size_of("scale=1/(iw-320):10", 320, 240).is_err());
        assert!(size_of("scale=ow:oh", 320, 240).is_err());
    }

    #[test]
    fn scale_keeps_format_and_timing() {
        for format in PixelFormat::ALL {
            let out = run("scale=33:17:flags=bicubic", pattern(64, 48, format)).unwrap();
            assert_eq!((out.width, out.height, out.format), (33, 17, format));
            assert_eq!(out.pts, 3);
            out.validate().unwrap();
        }
        // Size changes between frames are followed.
        let mut chain = parse_chain("scale=iw/2:-2", MediaType::Video).unwrap();
        for (w, h, ew, eh) in [(64, 48, 32, 24), (100, 50, 50, 26), (64, 48, 32, 24)] {
            let frame = Frame::Video(pattern(w, h, PixelFormat::Yuv420p));
            let out = video(crate::filter::run_chain(&mut chain, frame).unwrap());
            assert_eq!((out[0].width, out[0].height), (ew, eh));
        }
    }

    #[test]
    fn format_picks_the_first() {
        let out = run("format=rgb24", pattern(8, 6, PixelFormat::Yuv420p)).unwrap();
        assert_eq!(out.format, PixelFormat::Rgb24);
        let out = run(
            "format=pix_fmts=gray|yuv444p",
            pattern(8, 6, PixelFormat::Rgb24),
        )
        .unwrap();
        assert_eq!(out.format, PixelFormat::Gray);
        let f = pattern(8, 6, PixelFormat::Yuv422p);
        assert_eq!(run("format=yuv422p", f.clone()).unwrap(), f);
        assert!(parse_chain("format", MediaType::Video).is_err());
        assert!(parse_chain("format=nv12", MediaType::Video).is_err());
        assert!(parse_chain("format=|", MediaType::Video).is_err());
    }

    /// A geometry chain checked against an independent implementation of
    /// the same filters on a 64x48 test picture.
    #[test]
    fn geometry_chain_matches_reference() {
        const DIR: &str = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/video-processing/"
        );
        let read = |name: &str| std::fs::read(format!("{DIR}{name}")).unwrap();
        let bytes = read("src_64x48.yuv");
        let mut f = VideoFrame::new_black(
            64,
            48,
            PixelFormat::Yuv420p,
            0,
            crate::rational::Rational::new(1, 25),
        );
        let (y, chroma) = bytes.split_at(64 * 48);
        let (u, v) = chroma.split_at(32 * 24);
        f.planes = vec![y.to_vec(), u.to_vec(), v.to_vec()];
        let out = run("transpose=clock,pad=60:70:5:3:red,crop=40:50:2:1,hflip", f).unwrap();
        assert_eq!((out.width, out.height), (40, 50));
        assert_eq!(out.planes.concat(), read("geometry_40x50.yuv"));
    }

    #[test]
    fn malformed_frames_are_rejected() {
        let mut f = pattern(8, 6, PixelFormat::Yuv420p);
        f.planes[2].truncate(3);
        for spec in [
            "scale=4:4",
            "crop=2:2",
            "pad=20:20",
            "hflip",
            "vflip",
            "transpose",
            "format=gray",
            "fps=10",
            "negate",
            "eq=brightness=0.1",
            "boxblur",
        ] {
            let mut chain = parse_chain(spec, MediaType::Video).unwrap();
            assert!(
                crate::filter::run_chain(&mut chain, Frame::Video(f.clone())).is_err(),
                "{spec}"
            );
        }
    }
}
