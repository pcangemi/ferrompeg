//! Filters that move pixels without changing their values: crop, pad,
//! hflip, vflip and transpose.
//!
//! On subsampled formats, offsets are rounded down to the chroma grid so
//! every chroma sample still covers the same luma samples. Flipping a
//! subsampled plane of odd size mirrors the chroma grid as a whole, a
//! shift of at most half a chroma sample.
//!
//! Named pad colors are the basic and extended color keywords of the W3C
//! CSS Color Module Level 3 (a subset).

use super::expr::{self, Expr};
use super::{output_size, parse_expr, plane_shift, sample_bytes, video_input};
use crate::convert::video::{self as vconv, ScaleAlgorithm, rgb_to_gray, rgb_to_yuv};
use crate::error::{Error, Result};
use crate::filter::{Args, Filter};
use crate::frame::{Frame, PixelFormat, VideoFrame};

/// Rounds `v` down to a multiple of `1 << shift`.
fn align_down(v: u32, shift: u32) -> u32 {
    v >> shift << shift
}

struct Crop {
    w: Expr,
    h: Expr,
    x: Expr,
    y: Expr,
}

pub fn create_crop(args: &Args) -> Result<Box<dyn Filter>> {
    args.check_known("crop", &["w", "out_w", "h", "out_h", "x", "y"])?;
    Ok(Box::new(Crop {
        w: parse_expr(args, &["w", "out_w"], "iw")?,
        h: parse_expr(args, &["h", "out_h"], "ih")?,
        x: parse_expr(args, &["x"], "(in_w-out_w)/2")?,
        y: parse_expr(args, &["y"], "(in_h-out_h)/2")?,
    }))
}

impl Filter for Crop {
    fn filter(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        let v = video_input(frame, "crop")?;
        let (iw, ih) = (v.width, v.height);
        // Crop rounds computed sizes and offsets to the nearest integer
        // while scale and pad truncate: the established convention of
        // each option.
        let (w, h) = expr::solve_size(&self.w, &self.h, iw, ih)?;
        let (w, h) = output_size("crop", w.round(), h.round())?;
        if w > iw || h > ih {
            return Err(Error::invalid_argument(format!(
                "crop: area {w}x{h} is larger than the {iw}x{ih} input"
            )));
        }
        let vars = expr::sizes(iw, ih, w, h);
        // Offsets that would push the area outside the picture are pulled
        // back inside rather than rejected.
        let x = self.x.eval(&vars)?.round().clamp(0.0, (iw - w) as f64) as u32;
        let y = self.y.eval(&vars)?.round().clamp(0.0, (ih - h) as f64) as u32;
        let (sx, sy) = v.format.chroma_shift();
        let out = crop(&v, align_down(x, sx), align_down(y, sy), w, h);
        Ok(vec![Frame::Video(out)])
    }
}

/// Copies the `w` x `h` area at `x`, `y`, which must lie inside `v` and be
/// aligned to its chroma grid.
fn crop(v: &VideoFrame, x: u32, y: u32, w: u32, h: u32) -> VideoFrame {
    let planes = v
        .planes
        .iter()
        .enumerate()
        .map(|(p, src)| {
            let (sx, sy) = plane_shift(v.format, p);
            let unit = sample_bytes(v.format, p);
            let (src_row, _) = v.format.plane_size(p, v.width, v.height);
            let (row, rows) = v.format.plane_size(p, w, h);
            let left = (x >> sx) as usize * unit;
            let top = (y >> sy) as usize;
            let mut out = Vec::with_capacity(row * rows);
            for r in 0..rows {
                let start = (top + r) * src_row + left;
                out.extend_from_slice(&src[start..start + row]);
            }
            out
        })
        .collect();
    VideoFrame {
        width: w,
        height: h,
        format: v.format,
        planes,
        pts: v.pts,
        time_base: v.time_base,
    }
}

struct Pad {
    w: Expr,
    h: Expr,
    x: Expr,
    y: Expr,
    color: [u8; 3],
}

pub fn create_pad(args: &Args) -> Result<Box<dyn Filter>> {
    args.check_known("pad", &["w", "width", "h", "height", "x", "y", "color"])?;
    Ok(Box::new(Pad {
        w: parse_expr(args, &["w", "width"], "iw")?,
        h: parse_expr(args, &["h", "height"], "ih")?,
        x: parse_expr(args, &["x"], "0")?,
        y: parse_expr(args, &["y"], "0")?,
        color: parse_color(args.get(&["color"]).unwrap_or("black"))?,
    }))
}

impl Filter for Pad {
    fn filter(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        let v = video_input(frame, "pad")?;
        let (iw, ih) = (v.width, v.height);
        let (w, h) = expr::solve_size(&self.w, &self.h, iw, ih)?;
        // Zero keeps the input size.
        let w = if w.trunc() == 0.0 {
            iw as f64
        } else {
            w.trunc()
        };
        let h = if h.trunc() == 0.0 {
            ih as f64
        } else {
            h.trunc()
        };
        let (w, h) = output_size("pad", w, h)?;
        if w < iw || h < ih {
            return Err(Error::invalid_argument(format!(
                "pad: canvas {w}x{h} is smaller than the {iw}x{ih} input"
            )));
        }
        let vars = expr::sizes(iw, ih, w, h);
        // A negative offset centers the picture on that axis.
        let place = |e: &Expr, room: u32| -> Result<u32> {
            let v = e.eval(&vars)?.trunc();
            Ok(if v < 0.0 {
                room / 2
            } else {
                v.min(room as f64) as u32
            })
        };
        let x = place(&self.x, w - iw)?;
        let y = place(&self.y, h - ih)?;
        let (sx, sy) = v.format.chroma_shift();
        let out = pad(&v, align_down(x, sx), align_down(y, sy), w, h, self.color);
        Ok(vec![Frame::Video(out)])
    }
}

/// Places `v` at `x`, `y` (aligned to the chroma grid, with room to fit)
/// on a `w` x `h` canvas of `color`.
fn pad(v: &VideoFrame, x: u32, y: u32, w: u32, h: u32, color: [u8; 3]) -> VideoFrame {
    let mut out = solid_frame(w, h, v.format, color, v);
    for (p, (dst, src)) in out.planes.iter_mut().zip(&v.planes).enumerate() {
        let (sx, sy) = plane_shift(v.format, p);
        let unit = sample_bytes(v.format, p);
        let (dst_row, _) = v.format.plane_size(p, w, h);
        let (src_row, src_rows) = v.format.plane_size(p, v.width, v.height);
        let left = (x >> sx) as usize * unit;
        let top = (y >> sy) as usize;
        for (r, line) in src.chunks_exact(src_row).take(src_rows).enumerate() {
            let start = (top + r) * dst_row + left;
            dst[start..start + src_row].copy_from_slice(line);
        }
    }
    out
}

/// A `w` x `h` picture filled with an RGB color, timed like `like`.
fn solid_frame(
    w: u32,
    h: u32,
    format: PixelFormat,
    color: [u8; 3],
    like: &VideoFrame,
) -> VideoFrame {
    let mut f = VideoFrame::new_black(w, h, format, like.pts, like.time_base);
    match format {
        PixelFormat::Rgb24 => {
            for px in f.planes[0].chunks_exact_mut(3) {
                px.copy_from_slice(&color);
            }
        }
        PixelFormat::Gray => f.planes[0].fill(rgb_to_gray(color)),
        _ => {
            for (plane, value) in f.planes.iter_mut().zip(rgb_to_yuv(color)) {
                plane.fill(value);
            }
        }
    }
    f
}

/// Parses a color keyword, `#RRGGBB` or `0xRRGGBB`.
fn parse_color(s: &str) -> Result<[u8; 3]> {
    let s = s.trim();
    let hex = s
        .strip_prefix('#')
        .or_else(|| s.strip_prefix("0x"))
        .or_else(|| s.strip_prefix("0X"));
    let value = match hex {
        Some(digits) if digits.len() == 6 && digits.chars().all(|c| c.is_ascii_hexdigit()) => {
            u32::from_str_radix(digits, 16).ok()
        }
        Some(_) => None,
        None => named_color(&s.to_ascii_lowercase()),
    };
    let value =
        value.ok_or_else(|| Error::invalid_argument(format!("pad: unknown color '{s}'")))?;
    Ok([(value >> 16) as u8, (value >> 8) as u8, value as u8])
}

fn named_color(name: &str) -> Option<u32> {
    Some(match name {
        "black" => 0x000000,
        "white" => 0xffffff,
        "red" => 0xff0000,
        "green" => 0x008000,
        "lime" => 0x00ff00,
        "blue" => 0x0000ff,
        "yellow" => 0xffff00,
        "cyan" | "aqua" => 0x00ffff,
        "magenta" | "fuchsia" => 0xff00ff,
        "gray" | "grey" => 0x808080,
        "silver" => 0xc0c0c0,
        "maroon" => 0x800000,
        "olive" => 0x808000,
        "navy" => 0x000080,
        "purple" => 0x800080,
        "teal" => 0x008080,
        "orange" => 0xffa500,
        "pink" => 0xffc0cb,
        "brown" => 0xa52a2a,
        _ => return None,
    })
}

struct Flip {
    horizontal: bool,
}

pub fn create_hflip(args: &Args) -> Result<Box<dyn Filter>> {
    args.check_known("hflip", &[])?;
    Ok(Box::new(Flip { horizontal: true }))
}

pub fn create_vflip(args: &Args) -> Result<Box<dyn Filter>> {
    args.check_known("vflip", &[])?;
    Ok(Box::new(Flip { horizontal: false }))
}

impl Filter for Flip {
    fn filter(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        let name = if self.horizontal { "hflip" } else { "vflip" };
        let mut v = video_input(frame, name)?;
        let format = v.format;
        let (width, height) = (v.width, v.height);
        for (p, plane) in v.planes.iter_mut().enumerate() {
            let (row, _) = format.plane_size(p, width, height);
            if self.horizontal {
                let unit = sample_bytes(format, p);
                for line in plane.chunks_exact_mut(row) {
                    line.reverse();
                    // Reversing the row reversed the bytes inside each
                    // packed pixel too; put them back in order.
                    if unit > 1 {
                        for px in line.chunks_exact_mut(unit) {
                            px.reverse();
                        }
                    }
                }
            } else {
                let flipped: Vec<u8> = plane.chunks_exact(row).rev().flatten().copied().collect();
                *plane = flipped;
            }
        }
        Ok(vec![Frame::Video(v)])
    }
}

/// Rotation directions, numbered as on the command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dir {
    /// Rotate counterclockwise and flip vertically: a transposition
    /// across the main diagonal.
    CclockFlip,
    Clock,
    Cclock,
    /// Rotate clockwise and flip vertically: the anti-diagonal.
    ClockFlip,
}

struct Transpose {
    dir: Dir,
}

pub fn create_transpose(args: &Args) -> Result<Box<dyn Filter>> {
    args.check_known("transpose", &["dir"])?;
    let dir = match args.get(&["dir"]).map(str::trim).unwrap_or("cclock_flip") {
        "0" | "cclock_flip" => Dir::CclockFlip,
        "1" | "clock" => Dir::Clock,
        "2" | "cclock" => Dir::Cclock,
        "3" | "clock_flip" => Dir::ClockFlip,
        other => {
            return Err(Error::invalid_argument(format!(
                "transpose: unknown direction '{other}'"
            )));
        }
    };
    Ok(Box::new(Transpose { dir }))
}

impl Filter for Transpose {
    fn filter(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        let v = video_input(frame, "transpose")?;
        // 4:2:2 halves only the horizontal chroma resolution, which would
        // become the vertical one: go through 4:4:4 and back.
        if v.format == PixelFormat::Yuv422p {
            let full = vconv::convert(
                &v,
                v.width,
                v.height,
                PixelFormat::Yuv444p,
                ScaleAlgorithm::default(),
            )?;
            let turned = transpose(&full, self.dir);
            let out = vconv::convert(
                &turned,
                turned.width,
                turned.height,
                PixelFormat::Yuv422p,
                ScaleAlgorithm::default(),
            )?;
            return Ok(vec![Frame::Video(out)]);
        }
        Ok(vec![Frame::Video(transpose(&v, self.dir))])
    }
}

/// Rotates every plane; valid for formats whose chroma is subsampled the
/// same way in both directions.
fn transpose(v: &VideoFrame, dir: Dir) -> VideoFrame {
    let planes = v
        .planes
        .iter()
        .enumerate()
        .map(|(p, src)| {
            let unit = sample_bytes(v.format, p);
            let (row, h) = v.format.plane_size(p, v.width, v.height);
            let w = row / unit;
            let mut out = vec![0u8; src.len()];
            // The output has `w` rows of `h` samples. Output sample (ox, oy)
            // reads input sample (ix, iy).
            for (oy, line) in out.chunks_exact_mut(h * unit).enumerate() {
                for (ox, px) in line.chunks_exact_mut(unit).enumerate() {
                    let (ix, iy) = match dir {
                        Dir::CclockFlip => (oy, ox),
                        Dir::Clock => (oy, h - 1 - ox),
                        Dir::Cclock => (w - 1 - oy, ox),
                        Dir::ClockFlip => (w - 1 - oy, h - 1 - ox),
                    };
                    let i = iy * row + ix * unit;
                    px.copy_from_slice(&src[i..i + unit]);
                }
            }
            out
        })
        .collect();
    VideoFrame {
        width: v.height,
        height: v.width,
        format: v.format,
        planes,
        pts: v.pts,
        time_base: v.time_base,
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_util::{pattern, video};
    use super::*;
    use crate::filter::{parse_chain, run_chain};
    use crate::rational::Rational;
    use crate::stream::MediaType;

    fn run(spec: &str, frame: VideoFrame) -> Result<VideoFrame> {
        let mut chain = parse_chain(spec, MediaType::Video)?;
        Ok(video(run_chain(&mut chain, Frame::Video(frame))?).remove(0))
    }

    fn gray(width: u32, height: u32, data: &[u8]) -> VideoFrame {
        let mut f = VideoFrame::new_black(width, height, PixelFormat::Gray, 0, Rational::new(1, 1));
        f.planes[0] = data.to_vec();
        f
    }

    #[test]
    fn crop_defaults_center() {
        let f = gray(4, 4, &(0..16).collect::<Vec<u8>>());
        let out = run("crop=2:2", f.clone()).unwrap();
        assert_eq!(out.planes[0], vec![5, 6, 9, 10]);
        let out = run("crop=2:1:0:3", f.clone()).unwrap();
        assert_eq!(out.planes[0], vec![12, 13]);
        let out = run(
            "crop=w=iw/2:h=ih/2:x=iw-ow:y=0",
            gray(4, 2, &[0, 1, 2, 3, 4, 5, 6, 7]),
        )
        .unwrap();
        assert_eq!(out.planes[0], vec![2, 3]);
        // Offsets past the edge are pulled back in.
        let out = run("crop=2:2:100:100", f.clone()).unwrap();
        assert_eq!(out.planes[0], vec![10, 11, 14, 15]);
        assert_eq!(run("crop", f.clone()).unwrap(), f);
        assert!(run("crop=5:2", f.clone()).is_err());
        assert!(run("crop=0:2", f.clone()).is_err());
        assert!(parse_chain("crop=w=q", MediaType::Video).is_err());
    }

    #[test]
    fn crop_every_format_and_odd_sizes() {
        for format in PixelFormat::ALL {
            let f = pattern(9, 7, format);
            let out = run("crop=5:3:1:1", f.clone()).unwrap();
            out.validate().unwrap();
            assert_eq!((out.width, out.height, out.pts), (5, 3, f.pts));
            // Luma or RGB samples come from the requested position, or the
            // aligned one for subsampled formats.
            let (sx, sy) = format.chroma_shift();
            let (x, y) = (1 >> sx << sx, 1 >> sy << sy);
            let unit = if format == PixelFormat::Rgb24 { 3 } else { 1 };
            let row = 9 * unit;
            assert_eq!(out.planes[0][0], f.planes[0][y * row + x * unit]);
        }
        let f = pattern(8, 8, PixelFormat::Yuv420p);
        let out = run("crop=4:4:2:2", f.clone()).unwrap();
        // Chroma of the 4x4 area at (2, 2) starts at chroma (1, 1).
        assert_eq!(out.planes[1][0], f.planes[1][4 + 1]);
    }

    #[test]
    fn pad_places_and_fills() {
        let f = gray(2, 1, &[7, 9]);
        let out = run("pad=4:3:1:1:white", f.clone()).unwrap();
        assert_eq!(
            out.planes[0],
            vec![255, 255, 255, 255, 255, 7, 9, 255, 255, 255, 255, 255]
        );
        let out = run("pad=w=iw+2:h=ih:x=-1:y=0", f.clone()).unwrap();
        assert_eq!(out.planes[0], vec![0, 7, 9, 0]);
        let out = run("pad=0:2", f.clone()).unwrap();
        assert_eq!(out.planes[0], vec![7, 9, 0, 0]);
        assert!(run("pad=1:1", f.clone()).is_err());
        assert!(run("pad=100000:2", f.clone()).is_err());
        assert!(parse_chain("pad=4:4:color=nope", MediaType::Video).is_err());
        assert!(parse_chain("pad=4:4:color=#12345", MediaType::Video).is_err());
        let rgb = VideoFrame::new_black(1, 1, PixelFormat::Rgb24, 0, Rational::new(1, 1));
        let out = run("pad=2:1:color=#ff8000", rgb).unwrap();
        assert_eq!(out.planes[0], vec![0, 0, 0, 255, 128, 0]);
    }

    #[test]
    fn pad_every_format_and_color() {
        assert_eq!(parse_color("red").unwrap(), [255, 0, 0]);
        assert_eq!(parse_color("0x00FF7f").unwrap(), [0, 255, 127]);
        assert_eq!(parse_color("Navy").unwrap(), [0, 0, 128]);
        for format in PixelFormat::ALL {
            let f = pattern(5, 3, format);
            let out = run("pad=iw+4:ih+3:3:1:blue", f).unwrap();
            out.validate().unwrap();
            assert_eq!((out.width, out.height), (9, 6));
            match format {
                PixelFormat::Rgb24 => assert_eq!(&out.planes[0][..3], &[0, 0, 255]),
                PixelFormat::Gray => assert_eq!(out.planes[0][0], rgb_to_gray([0, 0, 255])),
                _ => {
                    assert_eq!(out.planes[0][0], 41);
                    assert_eq!(out.planes[1][0], 240);
                    assert_eq!(out.planes[2][0], 110);
                }
            }
        }
    }

    #[test]
    fn crop_inverts_pad() {
        for format in PixelFormat::ALL {
            let f = pattern(7, 5, format);
            let padded = run("pad=12:10:2:4", f.clone()).unwrap();
            let back = run("crop=7:5:2:4", padded).unwrap();
            assert_eq!(back, f, "{format}");
        }
    }

    #[test]
    fn flips() {
        let f = gray(3, 2, &[1, 2, 3, 4, 5, 6]);
        assert_eq!(
            run("hflip", f.clone()).unwrap().planes[0],
            vec![3, 2, 1, 6, 5, 4]
        );
        assert_eq!(
            run("vflip", f.clone()).unwrap().planes[0],
            vec![4, 5, 6, 1, 2, 3]
        );
        let mut rgb = VideoFrame::new_black(2, 1, PixelFormat::Rgb24, 0, Rational::new(1, 1));
        rgb.planes[0] = vec![1, 2, 3, 4, 5, 6];
        assert_eq!(run("hflip", rgb).unwrap().planes[0], vec![4, 5, 6, 1, 2, 3]);
        assert!(parse_chain("hflip=1", MediaType::Video).is_err());
        for format in PixelFormat::ALL {
            let f = pattern(7, 5, format);
            for spec in ["hflip", "vflip"] {
                let once = run(spec, f.clone()).unwrap();
                assert_ne!(once, f);
                assert_eq!(run(spec, once).unwrap(), f, "{spec} {format}");
            }
        }
    }

    #[test]
    fn transpose_directions() {
        // 1 2 3
        // 4 5 6
        let f = gray(3, 2, &[1, 2, 3, 4, 5, 6]);
        let t = |dir: &str| run(&format!("transpose={dir}"), f.clone()).unwrap();
        assert_eq!(t("cclock_flip").planes[0], vec![1, 4, 2, 5, 3, 6]);
        assert_eq!(t("clock").planes[0], vec![4, 1, 5, 2, 6, 3]);
        assert_eq!(t("cclock").planes[0], vec![3, 6, 2, 5, 1, 4]);
        assert_eq!(t("clock_flip").planes[0], vec![6, 3, 5, 2, 4, 1]);
        assert_eq!(t("1"), t("clock"));
        assert_eq!((t("2").width, t("2").height), (2, 3));
        assert_eq!(run("transpose", f.clone()).unwrap(), t("0"));
        assert!(parse_chain("transpose=4", MediaType::Video).is_err());
        assert!(parse_chain("transpose=up", MediaType::Video).is_err());
    }

    #[test]
    fn transpose_every_format() {
        for format in PixelFormat::ALL {
            let f = pattern(7, 5, format);
            let clock = run("transpose=clock", f.clone()).unwrap();
            clock.validate().unwrap();
            assert_eq!((clock.width, clock.height, clock.format), (5, 7, format));
            if format != PixelFormat::Yuv422p {
                // Four clockwise turns, or a transposition twice, restore
                // the picture exactly.
                let mut g = clock;
                for _ in 0..3 {
                    g = run("transpose=clock", g).unwrap();
                }
                assert_eq!(g, f, "{format}");
                let twice = run("transpose,transpose", f.clone()).unwrap();
                assert_eq!(twice, f);
            }
        }
        // 4:2:2 keeps its luma exactly through the 4:4:4 detour.
        let f = pattern(6, 4, PixelFormat::Yuv422p);
        let out = run("transpose=clock,transpose=cclock", f.clone()).unwrap();
        assert_eq!(out.planes[0], f.planes[0]);
    }
}
