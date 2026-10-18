//! Filters that change sample values: negate, eq and boxblur.

use super::video_input;
use crate::convert::video::{self as vconv, ScaleAlgorithm};
use crate::error::{Error, Result};
use crate::filter::{Args, Filter};
use crate::frame::{Frame, PixelFormat, VideoFrame};

fn apply_lut(data: &mut [u8], lut: &[u8; 256]) {
    for v in data {
        *v = lut[*v as usize];
    }
}

/// Inverts every sample, `255 - v`, on every plane. In YUV this maps
/// black to white and swaps chroma around its neutral point.
struct Negate;

pub fn create_negate(args: &Args) -> Result<Box<dyn Filter>> {
    args.check_known("negate", &[])?;
    Ok(Box::new(Negate))
}

impl Filter for Negate {
    fn filter(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        let mut v = video_input(frame, "negate")?;
        for plane in &mut v.planes {
            for s in plane.iter_mut() {
                *s = 255 - *s;
            }
        }
        Ok(vec![Frame::Video(v)])
    }
}

/// Brightness, contrast and gamma act on luma normalized to 0..1 (the
/// 8-bit value over 255), in that order:
/// `v = ((v - 0.5) * contrast + 0.5 + brightness) ^ (1 / gamma)`.
/// Saturation scales chroma around its neutral value. RGB pictures are
/// adjusted in YUV 4:4:4 and converted back, so the controls mean the same
/// thing for every format; gray pictures have only the luma curve.
struct Eq {
    luma: [u8; 256],
    chroma: [u8; 256],
    identity: bool,
}

pub fn create_eq(args: &Args) -> Result<Box<dyn Filter>> {
    args.check_known("eq", &["contrast", "brightness", "saturation", "gamma"])?;
    let get = |key: &str, default: f64, lo: f64, hi: f64| -> Result<f64> {
        let v = args.f64(&[key])?.unwrap_or(default);
        if !(lo..=hi).contains(&v) {
            return Err(Error::invalid_argument(format!(
                "eq: {key} must be between {lo} and {hi}"
            )));
        }
        Ok(v)
    };
    let contrast = get("contrast", 1.0, 0.0, 4.0)?;
    let brightness = get("brightness", 0.0, -1.0, 1.0)?;
    let saturation = get("saturation", 1.0, 0.0, 3.0)?;
    let gamma = get("gamma", 1.0, 0.1, 10.0)?;
    Ok(Box::new(Eq::new(contrast, brightness, saturation, gamma)))
}

impl Eq {
    fn new(contrast: f64, brightness: f64, saturation: f64, gamma: f64) -> Self {
        let mut luma = [0u8; 256];
        let mut chroma = [0u8; 256];
        for (i, (l, c)) in luma.iter_mut().zip(chroma.iter_mut()).enumerate() {
            let v = i as f64 / 255.0;
            let v = ((v - 0.5) * contrast + 0.5 + brightness).clamp(0.0, 1.0);
            let v = v.powf(1.0 / gamma);
            *l = (v * 255.0).round() as u8;
            *c = (128.0 + (i as f64 - 128.0) * saturation)
                .round()
                .clamp(0.0, 255.0) as u8;
        }
        let identity = luma
            .iter()
            .zip(&chroma)
            .enumerate()
            .all(|(i, (&l, &c))| l as usize == i && c as usize == i);
        Eq {
            luma,
            chroma,
            identity,
        }
    }

    fn adjust_yuv(&self, v: &mut VideoFrame) {
        for (p, plane) in v.planes.iter_mut().enumerate() {
            apply_lut(plane, if p == 0 { &self.luma } else { &self.chroma });
        }
    }
}

impl Filter for Eq {
    fn filter(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        let mut v = video_input(frame, "eq")?;
        if self.identity {
            return Ok(vec![Frame::Video(v)]);
        }
        match v.format {
            PixelFormat::Rgb24 => {
                let (w, h) = (v.width, v.height);
                let alg = ScaleAlgorithm::default();
                let mut yuv = vconv::convert(&v, w, h, PixelFormat::Yuv444p, alg)?;
                self.adjust_yuv(&mut yuv);
                v = vconv::convert(&yuv, w, h, PixelFormat::Rgb24, alg)?;
            }
            PixelFormat::Gray => apply_lut(&mut v.planes[0], &self.luma),
            _ => self.adjust_yuv(&mut v),
        }
        Ok(vec![Frame::Video(v)])
    }
}

/// Averages each sample with its neighbors within `radius` along rows,
/// then along columns, `power` times. Edges repeat the border sample.
struct BoxBlur {
    luma: (u32, u32),
    chroma: (u32, u32),
}

/// Largest radius and repeat count accepted; beyond that the picture is a
/// flat average anyway and the work would only grow.
const MAX_RADIUS: u32 = 16384;
const MAX_POWER: u32 = 64;

pub fn create_boxblur(args: &Args) -> Result<Box<dyn Filter>> {
    args.check_known(
        "boxblur",
        &[
            "luma_radius",
            "lr",
            "luma_power",
            "lp",
            "chroma_radius",
            "cr",
            "chroma_power",
            "cp",
        ],
    )?;
    let get = |keys: &[&str], max: u32| -> Result<Option<u32>> {
        match args.u32(keys)? {
            Some(v) if v > max => Err(Error::invalid_argument(format!(
                "boxblur: {} must be at most {max}",
                keys[0]
            ))),
            v => Ok(v),
        }
    };
    let lr = get(&["luma_radius", "lr"], MAX_RADIUS)?.unwrap_or(2);
    let lp = get(&["luma_power", "lp"], MAX_POWER)?.unwrap_or(2);
    // Chroma follows the luma settings unless given.
    let cr = get(&["chroma_radius", "cr"], MAX_RADIUS)?.unwrap_or(lr);
    let cp = get(&["chroma_power", "cp"], MAX_POWER)?.unwrap_or(lp);
    Ok(Box::new(BoxBlur {
        luma: (lr, lp),
        chroma: (cr, cp),
    }))
}

impl Filter for BoxBlur {
    fn filter(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        let mut v = video_input(frame, "boxblur")?;
        let format = v.format;
        let (width, height) = (v.width, v.height);
        if format == PixelFormat::Rgb24 {
            // Packed: blur each channel with the luma settings.
            let (w, h) = (width as usize, height as usize);
            let mut channels: Vec<Vec<u8>> = (0..3).map(|_| Vec::with_capacity(w * h)).collect();
            for px in v.planes[0].chunks_exact(3) {
                for (c, &s) in channels.iter_mut().zip(px) {
                    c.push(s);
                }
            }
            for c in &mut channels {
                blur_plane(c, w, h, self.luma.0 as usize, self.luma.1);
            }
            for (i, px) in v.planes[0].chunks_exact_mut(3).enumerate() {
                for (s, c) in px.iter_mut().zip(&channels) {
                    *s = c[i];
                }
            }
        } else {
            for (p, plane) in v.planes.iter_mut().enumerate() {
                let (w, h) = format.plane_size(p, width, height);
                let (radius, power) = if p == 0 { self.luma } else { self.chroma };
                blur_plane(plane, w, h, radius as usize, power);
            }
        }
        Ok(vec![Frame::Video(v)])
    }
}

fn blur_plane(data: &mut [u8], w: usize, h: usize, radius: usize, power: u32) {
    if radius == 0 || power == 0 {
        return;
    }
    let mut line = Vec::new();
    let mut out = Vec::new();
    for _ in 0..power {
        for row in data.chunks_exact_mut(w) {
            line.clear();
            line.extend_from_slice(row);
            blur_line(&line, radius, &mut out);
            row.copy_from_slice(&out);
        }
        for x in 0..w {
            line.clear();
            line.extend(data[x..].iter().step_by(w).take(h));
            blur_line(&line, radius, &mut out);
            for (s, &o) in data[x..].iter_mut().step_by(w).zip(&out) {
                *s = o;
            }
        }
    }
}

/// Running box average over `2 * radius + 1` samples with clamped edges,
/// rounded to nearest. Each step adds the sample entering the window and
/// removes the one leaving it, so the cost does not depend on the radius.
fn blur_line(src: &[u8], radius: usize, out: &mut Vec<u8>) {
    let n = src.len();
    let at = |i: isize| src[i.clamp(0, n as isize - 1) as usize] as u32;
    let r = radius as isize;
    let size = 2 * radius as u32 + 1;
    let mut sum: u32 = (-r..=r).map(at).sum();
    out.clear();
    for i in 0..n as isize {
        out.push(((sum + size / 2) / size) as u8);
        sum = sum + at(i + r + 1) - at(i - r);
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
    fn negate_is_an_involution() {
        let f = gray(3, 1, &[0, 100, 255]);
        assert_eq!(run("negate", f).unwrap().planes[0], vec![255, 155, 0]);
        for format in PixelFormat::ALL {
            let f = pattern(5, 3, format);
            assert_eq!(run("negate,negate", f.clone()).unwrap(), f);
        }
        assert!(parse_chain("negate=1", MediaType::Video).is_err());
    }

    #[test]
    fn eq_curves() {
        let ramp: Vec<u8> = (0..=255).collect();
        let f = gray(256, 1, &ramp);
        assert_eq!(run("eq", f.clone()).unwrap(), f);
        let b = run("eq=brightness=0.2", f.clone()).unwrap();
        assert_eq!(b.planes[0][0], 51);
        assert_eq!(b.planes[0][255], 255);
        let c = run("eq=2", f.clone()).unwrap();
        assert_eq!(c.planes[0][64], 0);
        assert_eq!(c.planes[0][200], 255);
        let g = run("eq=gamma=2", f.clone()).unwrap();
        assert_eq!(g.planes[0][64], 128);
        assert_eq!(g.planes[0][0], 0);
        assert_eq!(g.planes[0][255], 255);
        let flat = run("eq=contrast=0", f.clone()).unwrap();
        assert!(flat.planes[0].iter().all(|&v| v == 128));
    }

    #[test]
    fn eq_saturation() {
        let mut f = VideoFrame::new_black(2, 2, PixelFormat::Yuv420p, 0, Rational::new(1, 1));
        f.planes[1] = vec![100];
        f.planes[2] = vec![60];
        let s = run("eq=saturation=2", f.clone()).unwrap();
        assert_eq!((s.planes[1][0], s.planes[2][0]), (72, 0));
        let s = run("eq=saturation=0", f.clone()).unwrap();
        assert_eq!((s.planes[1][0], s.planes[2][0]), (128, 128));
        // RGB: zero saturation leaves a gray.
        let mut rgb = VideoFrame::new_black(1, 1, PixelFormat::Rgb24, 0, Rational::new(1, 1));
        rgb.planes[0] = vec![200, 40, 90];
        let out = run("eq=saturation=0", rgb.clone()).unwrap();
        let p = &out.planes[0];
        assert!(p[0] == p[1] && p[1] == p[2]);
        // Neutral settings pass through untouched.
        assert_eq!(run("eq=1:0:1:1", rgb.clone()).unwrap(), rgb);
        for format in PixelFormat::ALL {
            let out = run("eq=1.2:0.1:1.5:1.3", pattern(5, 3, format)).unwrap();
            out.validate().unwrap();
            assert_eq!(out.format, format);
        }
    }

    #[test]
    fn eq_rejects_out_of_range() {
        for spec in [
            "eq=brightness=2",
            "eq=contrast=-1",
            "eq=contrast=5",
            "eq=saturation=4",
            "eq=gamma=0",
            "eq=gamma=abc",
            "eq=hue=1",
        ] {
            assert!(parse_chain(spec, MediaType::Video).is_err(), "{spec}");
        }
    }

    #[test]
    fn box_average() {
        let mut out = Vec::new();
        blur_line(&[0, 0, 90, 0, 0], 1, &mut out);
        assert_eq!(out, vec![0, 30, 30, 30, 0]);
        // Edges repeat the border sample.
        blur_line(&[90, 0, 0], 1, &mut out);
        assert_eq!(out, vec![60, 30, 0]);
        // A radius larger than the line averages with clamping.
        blur_line(&[10, 20], 5, &mut out);
        assert_eq!(out.len(), 2);
        let f = gray(3, 3, &[0, 0, 0, 0, 90, 0, 0, 0, 0]);
        let out = run("boxblur=1:1", f).unwrap();
        assert_eq!(out.planes[0], vec![10; 9]);
    }

    #[test]
    fn boxblur_keeps_flat_areas_and_formats() {
        for format in PixelFormat::ALL {
            let mut f = VideoFrame::new_black(9, 7, format, 0, Rational::new(1, 1));
            for plane in &mut f.planes {
                plane.fill(77);
            }
            assert_eq!(run("boxblur=3:2", f.clone()).unwrap(), f);
            let out = run("boxblur=lr=2:lp=1:cr=1:cp=3", pattern(9, 7, format)).unwrap();
            out.validate().unwrap();
            assert_eq!(out.format, format);
        }
        let f = pattern(9, 7, PixelFormat::Gray);
        assert_eq!(run("boxblur=0", f.clone()).unwrap(), f);
        assert_eq!(run("boxblur=luma_power=0", f.clone()).unwrap(), f);
        assert!(parse_chain("boxblur=-1", MediaType::Video).is_err());
        assert!(parse_chain("boxblur=99999", MediaType::Video).is_err());
        assert!(parse_chain("boxblur=2:1000", MediaType::Video).is_err());
    }
}
