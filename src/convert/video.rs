//! Picture size and pixel format conversion.
//!
//! References:
//!
//! * ITU-R BT.601-7, "Studio encoding parameters of digital television for
//!   standard 4:3 and wide-screen 16:9 aspect ratios": luma weights
//!   (Kr = 0.299, Kb = 0.114), colour difference scaling (1.772 and 1.402)
//!   and the limited quantization range (Y 16 to 235, Cb and Cr 16 to 240).
//! * R. G. Keys, "Cubic convolution interpolation for digital image
//!   processing", IEEE Transactions on Acoustics, Speech, and Signal
//!   Processing 29(6), 1981: the cubic kernel with a = -0.5.
//! * Chroma siting: a subsampled chroma sample sits at the center of the
//!   luma samples it covers, the convention of the JPEG File Interchange
//!   Format 1.02 and of ISO/IEC 11172-2.
//!
//! Everything works on planes of 8-bit samples: packed RGB is split into
//! three planes first and packed again at the end. A plane sample `k` whose
//! samples each span `f` picture pixels is centered at picture coordinate
//! `(k + 0.5) * f`, so one resampling routine handles resizing, chroma
//! subsampling and chroma upsampling alike, each plane at its own size.
//!
//! Resampling is separable with precomputed per-output weights in 14-bit
//! fixed point that always sum to exactly one, so a solid area stays solid
//! whatever the kernel. The horizontal pass keeps 6 fractional bits for the
//! vertical pass, so the two passes round only once.

use crate::error::{Error, Result};
use crate::frame::{PixelFormat, VideoFrame};

/// Interpolation used when the size changes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScaleAlgorithm {
    Nearest,
    #[default]
    Bilinear,
    Bicubic,
    /// Box filter averaging every covered source pixel; best for large
    /// reductions.
    Area,
}

impl ScaleAlgorithm {
    pub fn from_name(name: &str) -> Result<Self> {
        Ok(match name {
            "neighbor" | "nearest" | "point" => ScaleAlgorithm::Nearest,
            "bilinear" | "fast_bilinear" => ScaleAlgorithm::Bilinear,
            "bicubic" => ScaleAlgorithm::Bicubic,
            "area" => ScaleAlgorithm::Area,
            _ => {
                return Err(Error::invalid_argument(format!(
                    "unknown scaling algorithm '{name}'"
                )));
            }
        })
    }
}

/// Largest width or height accepted, which bounds every allocation.
pub const MAX_DIMENSION: u32 = 16384;

const WEIGHT_BITS: u32 = 14;
const WEIGHT_ONE: i32 = 1 << WEIGHT_BITS;
/// Fractional bits kept between the horizontal and the vertical pass.
const MID_BITS: u32 = 6;

/// Returns `frame` resized to `width` x `height` and converted to `format`,
/// keeping its timestamp.
pub fn convert(
    frame: &VideoFrame,
    width: u32,
    height: u32,
    format: PixelFormat,
    algorithm: ScaleAlgorithm,
) -> Result<VideoFrame> {
    frame.validate()?;
    check_size(frame.width, frame.height)?;
    check_size(width, height)?;
    if frame.width == width && frame.height == height && frame.format == format {
        return Ok(frame.clone());
    }
    let src = Picture::from_frame(frame);
    let (w, h) = (width as usize, height as usize);
    let planes = match (family(frame.format), family(format)) {
        (Family::Yuv, Family::Yuv) => {
            let mut out = vec![src.scale_plane(0, w, h, 1, 1, algorithm)];
            let (sx, sy) = format.chroma_shift();
            for p in 1..3 {
                out.push(src.scale_plane(p, w, h, 1 << sx, 1 << sy, algorithm));
            }
            out
        }
        (Family::Yuv, Family::Gray) => {
            let mut y = src.scale_plane(0, w, h, 1, 1, algorithm);
            apply_lut(&mut y.data, &luma_to_gray_lut());
            vec![y]
        }
        (Family::Yuv, Family::Rgb) => {
            let y = src.scale_plane(0, w, h, 1, 1, algorithm);
            let u = src.scale_plane(1, w, h, 1, 1, algorithm);
            let v = src.scale_plane(2, w, h, 1, 1, algorithm);
            vec![yuv_planes_to_rgb(&y, &u, &v)]
        }
        (Family::Gray, Family::Yuv) => {
            let mut y = src.scale_plane(0, w, h, 1, 1, algorithm);
            apply_lut(&mut y.data, &gray_to_luma_lut());
            let (cw, ch) = format.plane_size(1, width, height);
            let neutral = Plane {
                width: cw,
                height: ch,
                data: vec![128; cw * ch],
            };
            vec![y, neutral.clone(), neutral]
        }
        (Family::Gray, Family::Gray) => vec![src.scale_plane(0, w, h, 1, 1, algorithm)],
        (Family::Gray, Family::Rgb) => {
            let g = src.scale_plane(0, w, h, 1, 1, algorithm);
            vec![pack_rgb(&g, &g, &g)]
        }
        (Family::Rgb, Family::Yuv) => {
            // Convert at the source size, then resample each plane once:
            // chroma goes straight from full resolution to the target
            // size and subsampling.
            let full = src.map_rgb(rgb_to_yuv);
            let (sx, sy) = format.chroma_shift();
            vec![
                full.scale_plane(0, w, h, 1, 1, algorithm),
                full.scale_plane(1, w, h, 1 << sx, 1 << sy, algorithm),
                full.scale_plane(2, w, h, 1 << sx, 1 << sy, algorithm),
            ]
        }
        (Family::Rgb, Family::Gray) => {
            let gray = src.map_rgb(|c| [rgb_to_gray(c)]);
            vec![gray.scale_plane(0, w, h, 1, 1, algorithm)]
        }
        (Family::Rgb, Family::Rgb) => {
            let r = src.scale_plane(0, w, h, 1, 1, algorithm);
            let g = src.scale_plane(1, w, h, 1, 1, algorithm);
            let b = src.scale_plane(2, w, h, 1, 1, algorithm);
            vec![pack_rgb(&r, &g, &b)]
        }
    };
    let out = VideoFrame {
        width,
        height,
        format,
        planes: planes.into_iter().map(|p| p.data).collect(),
        pts: frame.pts,
        time_base: frame.time_base,
    };
    debug_assert!(out.validate().is_ok());
    Ok(out)
}

fn check_size(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(Error::invalid_argument(format!(
            "picture size {width}x{height} is outside 1x1 to {MAX_DIMENSION}x{MAX_DIMENSION}"
        )));
    }
    Ok(())
}

/// BT.601 limited range Y, Cb, Cr of a full range RGB color.
pub fn rgb_to_yuv([r, g, b]: [u8; 3]) -> [u8; 3] {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    // Coefficients are 219/255 (luma) or 224/255 (chroma) times the
    // BT.601 matrix, in 16-bit fixed point. They were rounded so that each
    // chroma row sums to exactly zero (any gray keeps Cb = Cr = 128) and
    // the luma row maps white to exactly 235.
    let y = (16829 * r + 33039 * g + 6416 * b + (16 << 16) + (1 << 15)) >> 16;
    let u = (-9714 * r - 19070 * g + 28784 * b + (128 << 16) + (1 << 15)) >> 16;
    let v = (28784 * r - 24103 * g - 4681 * b + (128 << 16) + (1 << 15)) >> 16;
    [clamp_u8(y), clamp_u8(u), clamp_u8(v)]
}

/// Full range RGB of a BT.601 limited range color. Values outside the
/// nominal range are accepted and the result clamped.
pub fn yuv_to_rgb([y, u, v]: [u8; 3]) -> [u8; 3] {
    // 255/219, and 255/224 times 1.402, 1.772 and the green terms
    // 1.772 * 0.114 / 0.587 and 1.402 * 0.299 / 0.587, in 16-bit fixed point.
    let c = (y as i32 - 16) * 76309 + (1 << 15);
    let d = u as i32 - 128;
    let e = v as i32 - 128;
    [
        clamp_u8((c + 104597 * e) >> 16),
        clamp_u8((c - 25675 * d - 53279 * e) >> 16),
        clamp_u8((c + 132201 * d) >> 16),
    ]
}

/// Full range luma (the BT.601 weighted sum) of a full range RGB color.
pub fn rgb_to_gray([r, g, b]: [u8; 3]) -> u8 {
    // 0.299, 0.587 and 0.114 in 16-bit fixed point, summing to 65536.
    clamp_u8((19595 * r as i32 + 38470 * g as i32 + 7471 * b as i32 + (1 << 15)) >> 16)
}

fn clamp_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// Limited range luma to full range gray, rounding to nearest.
fn luma_to_gray_lut() -> [u8; 256] {
    let mut lut = [0u8; 256];
    for (y, out) in lut.iter_mut().enumerate() {
        let v = (y as i32 - 16).max(0);
        *out = clamp_u8((v * 255 + 109) / 219);
    }
    lut
}

/// Full range gray to limited range luma, rounding to nearest.
fn gray_to_luma_lut() -> [u8; 256] {
    let mut lut = [0u8; 256];
    for (g, out) in lut.iter_mut().enumerate() {
        *out = (16 + (g as i32 * 219 + 127) / 255) as u8;
    }
    lut
}

fn apply_lut(data: &mut [u8], lut: &[u8; 256]) {
    for v in data {
        *v = lut[*v as usize];
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    Yuv,
    Gray,
    Rgb,
}

fn family(format: PixelFormat) -> Family {
    match format {
        PixelFormat::Gray => Family::Gray,
        PixelFormat::Rgb24 => Family::Rgb,
        _ => Family::Yuv,
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Plane {
    width: usize,
    height: usize,
    data: Vec<u8>,
}

/// A source picture split into planes, with the subsampling of each.
struct Picture {
    width: usize,
    height: usize,
    planes: Vec<Plane>,
    /// Picture pixels per plane sample, horizontally and vertically.
    factors: Vec<(usize, usize)>,
}

impl Picture {
    fn from_frame(frame: &VideoFrame) -> Self {
        let (width, height) = (frame.width as usize, frame.height as usize);
        let (planes, factors) = if frame.format == PixelFormat::Rgb24 {
            let mut rgb: Vec<Vec<u8>> =
                (0..3).map(|_| Vec::with_capacity(width * height)).collect();
            for px in frame.planes[0].chunks_exact(3) {
                for (plane, &v) in rgb.iter_mut().zip(px) {
                    plane.push(v);
                }
            }
            let planes = rgb
                .into_iter()
                .map(|data| Plane {
                    width,
                    height,
                    data,
                })
                .collect();
            (planes, vec![(1, 1); 3])
        } else {
            let (sx, sy) = frame.format.chroma_shift();
            let mut planes = Vec::new();
            let mut factors = Vec::new();
            for (p, data) in frame.planes.iter().enumerate() {
                let (w, h) = frame.format.plane_size(p, frame.width, frame.height);
                planes.push(Plane {
                    width: w,
                    height: h,
                    data: data.clone(),
                });
                factors.push(if p == 0 { (1, 1) } else { (1 << sx, 1 << sy) });
            }
            (planes, factors)
        };
        Picture {
            width,
            height,
            planes,
            factors,
        }
    }

    /// Maps the three planes of an RGB picture through `f`, pixel by
    /// pixel, into `N` full resolution planes.
    fn map_rgb<const N: usize>(&self, f: impl Fn([u8; 3]) -> [u8; N]) -> Picture {
        let n = self.width * self.height;
        let mut planes: Vec<Plane> = (0..N)
            .map(|_| Plane {
                width: self.width,
                height: self.height,
                data: Vec::with_capacity(n),
            })
            .collect();
        let [r, g, b] = [0, 1, 2].map(|p| &self.planes[p].data);
        for ((&r, &g), &b) in r.iter().zip(g).zip(b) {
            for (plane, v) in planes.iter_mut().zip(f([r, g, b])) {
                plane.data.push(v);
            }
        }
        Picture {
            width: self.width,
            height: self.height,
            planes,
            factors: vec![(1, 1); N],
        }
    }

    /// Resamples plane `p` for an output picture of `width` x `height`
    /// whose samples each span `fx` x `fy` pixels.
    fn scale_plane(
        &self,
        p: usize,
        width: usize,
        height: usize,
        fx: usize,
        fy: usize,
        algorithm: ScaleAlgorithm,
    ) -> Plane {
        let src = &self.planes[p];
        let (sfx, sfy) = self.factors[p];
        let x = AxisMap {
            src_picture: self.width,
            src_factor: sfx,
            dst_picture: width,
            dst_factor: fx,
        };
        let y = AxisMap {
            src_picture: self.height,
            src_factor: sfy,
            dst_picture: height,
            dst_factor: fy,
        };
        resample(src, x, y, algorithm)
    }
}

/// How one axis of a source plane maps onto one axis of the output plane.
#[derive(Clone, Copy, Debug)]
struct AxisMap {
    src_picture: usize,
    src_factor: usize,
    dst_picture: usize,
    dst_factor: usize,
}

impl AxisMap {
    fn src_len(self) -> usize {
        self.src_picture.div_ceil(self.src_factor)
    }

    fn dst_len(self) -> usize {
        self.dst_picture.div_ceil(self.dst_factor)
    }

    /// Builds the filter, or `None` when the axis is unchanged.
    fn filter(self, algorithm: ScaleAlgorithm) -> Option<AxisFilter> {
        let num = self.dst_factor * self.src_picture;
        let den = self.dst_picture * self.src_factor;
        if num == den && self.src_len() == self.dst_len() {
            return None;
        }
        // Source samples per output sample.
        let step = num as f64 / den as f64;
        // When only the chroma resolution changes, the picture itself is
        // not being resized: reduce by averaging and enlarge by
        // interpolation whatever the requested algorithm.
        let kernel = if self.src_picture == self.dst_picture {
            if step > 1.0 {
                Kernel::Box
            } else {
                Kernel::Linear
            }
        } else {
            match algorithm {
                ScaleAlgorithm::Nearest => Kernel::Nearest,
                ScaleAlgorithm::Bilinear => Kernel::Linear,
                ScaleAlgorithm::Bicubic => Kernel::Cubic,
                ScaleAlgorithm::Area => Kernel::Box,
            }
        };
        Some(AxisFilter::new(
            self.src_len(),
            self.dst_len(),
            step,
            kernel,
        ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kernel {
    Nearest,
    Linear,
    Cubic,
    Box,
}

/// Keys cubic convolution kernel with a = -0.5.
fn cubic(x: f64) -> f64 {
    const A: f64 = -0.5;
    let x = x.abs();
    if x < 1.0 {
        ((A + 2.0) * x - (A + 3.0)) * x * x + 1.0
    } else if x < 2.0 {
        ((A * x - 5.0 * A) * x + 8.0 * A) * x - 4.0 * A
    } else {
        0.0
    }
}

/// Fixed point weights for one axis. Every output sample reads `taps`
/// consecutive source samples starting at its `starts` entry; taps that
/// would fall outside the source were folded onto the edge samples
/// (clamped edges), so no bounds logic remains in the inner loops.
#[derive(Debug)]
struct AxisFilter {
    taps: usize,
    starts: Vec<usize>,
    weights: Vec<i32>,
}

impl AxisFilter {
    fn new(src_len: usize, dst_len: usize, step: f64, kernel: Kernel) -> Self {
        let last = src_len as isize - 1;
        let mut rows: Vec<(usize, Vec<i32>)> = Vec::with_capacity(dst_len);
        let mut raw: Vec<(isize, f64)> = Vec::new();
        for j in 0..dst_len {
            // Center of output sample j in source sample coordinates.
            let c = (j as f64 + 0.5) * step - 0.5;
            raw.clear();
            match kernel {
                Kernel::Nearest => raw.push(((c + 0.5).floor() as isize, 1.0)),
                Kernel::Box => {
                    // Overlap of the output footprint with each source
                    // sample, both as intervals on the source axis.
                    let half = step / 2.0;
                    let lo = (c - half + 0.5).floor() as isize;
                    let hi = (c + half - 0.5).ceil() as isize;
                    for i in lo..=hi {
                        let x = i as f64;
                        let w = (c + half).min(x + 0.5) - (c - half).max(x - 0.5);
                        raw.push((i, w.max(0.0)));
                    }
                }
                Kernel::Linear | Kernel::Cubic => {
                    // Reductions widen the kernel so it covers every source
                    // sample under the output footprint (no aliasing).
                    let scale = step.max(1.0);
                    let radius = if kernel == Kernel::Linear { 1.0 } else { 2.0 } * scale;
                    let lo = (c - radius).ceil() as isize;
                    let hi = (c + radius).floor() as isize;
                    for i in lo..=hi {
                        let x = (i as f64 - c) / scale;
                        let w = if kernel == Kernel::Linear {
                            (1.0 - x.abs()).max(0.0)
                        } else {
                            cubic(x)
                        };
                        raw.push((i, w));
                    }
                }
            }
            rows.push(quantize(&raw, last, c));
        }
        let taps = rows.iter().map(|(_, w)| w.len()).max().unwrap_or(1).max(1);
        let mut starts = Vec::with_capacity(dst_len);
        let mut weights = vec![0; dst_len * taps];
        for ((first, w), out) in rows.iter().zip(weights.chunks_exact_mut(taps)) {
            // Shift the window left near the end so it stays inside the
            // source; the weights move right by the same amount.
            let start = (*first).min(src_len - taps);
            let offset = first - start;
            out[offset..offset + w.len()].copy_from_slice(w);
            starts.push(start);
        }
        AxisFilter {
            taps,
            starts,
            weights,
        }
    }
}

/// Folds out of range taps onto the edges, normalizes and converts to
/// fixed point with an exact sum. Returns the first source index and the
/// weights from there on, without zero weights at either end.
fn quantize(raw: &[(isize, f64)], last: isize, center: f64) -> (usize, Vec<i32>) {
    let clamp = |i: isize| i.clamp(0, last) as usize;
    let lo = raw.iter().map(|&(i, _)| clamp(i)).min().unwrap_or(0);
    let hi = raw.iter().map(|&(i, _)| clamp(i)).max().unwrap_or(0);
    let mut folded = vec![0.0f64; hi - lo + 1];
    for &(i, w) in raw {
        folded[clamp(i) - lo] += w;
    }
    let sum: f64 = folded.iter().sum();
    if !(sum.is_finite() && sum > 1e-9) {
        // Cannot happen with the kernels above; fall back to the nearest
        // sample rather than dividing by zero.
        return (clamp((center + 0.5).floor() as isize), vec![WEIGHT_ONE]);
    }
    let mut q: Vec<i32> = folded
        .iter()
        .map(|w| (w / sum * WEIGHT_ONE as f64).round() as i32)
        .collect();
    // Rounding may leave the total a little off one: give the difference
    // to the largest weight, where it matters least.
    let diff = WEIGHT_ONE - q.iter().sum::<i32>();
    if let Some((imax, _)) = q.iter().enumerate().max_by_key(|&(_, w)| *w) {
        q[imax] += diff;
    }
    let first = q.iter().position(|&w| w != 0).unwrap_or(0);
    let end = q.iter().rposition(|&w| w != 0).map_or(1, |p| p + 1);
    (lo + first, q[first..end].to_vec())
}

fn resample(src: &Plane, x: AxisMap, y: AxisMap, algorithm: ScaleAlgorithm) -> Plane {
    let (dw, dh) = (x.dst_len(), y.dst_len());
    let data = match (x.filter(algorithm), y.filter(algorithm)) {
        (None, None) => src.data.clone(),
        (Some(fx), None) => horizontal_u8(&src.data, src.width, &fx, dw),
        (None, Some(fy)) => vertical(&src.data, src.width, 0, &fy, dh),
        (Some(fx), Some(fy)) => {
            let mid = horizontal_mid(&src.data, src.width, &fx, dw);
            vertical(&mid, dw, MID_BITS, &fy, dh)
        }
    };
    Plane {
        width: dw,
        height: dh,
        data,
    }
}

/// Horizontal pass producing samples with `MID_BITS` fractional bits.
fn horizontal_mid(src: &[u8], width: usize, f: &AxisFilter, dw: usize) -> Vec<i16> {
    let shift = WEIGHT_BITS - MID_BITS;
    let round = 1 << (shift - 1);
    let mut out = vec![0i16; dw * (src.len() / width)];
    for (row, out_row) in src.chunks_exact(width).zip(out.chunks_exact_mut(dw)) {
        for ((o, &start), w) in out_row
            .iter_mut()
            .zip(&f.starts)
            .zip(f.weights.chunks_exact(f.taps))
        {
            let acc = dot(&row[start..start + f.taps], w);
            // Cubic overshoot stays far inside the i16 range.
            *o = ((acc + round) >> shift).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        }
    }
    out
}

/// Horizontal pass producing final 8-bit samples.
fn horizontal_u8(src: &[u8], width: usize, f: &AxisFilter, dw: usize) -> Vec<u8> {
    let round = 1 << (WEIGHT_BITS - 1);
    let mut out = vec![0u8; dw * (src.len() / width)];
    for (row, out_row) in src.chunks_exact(width).zip(out.chunks_exact_mut(dw)) {
        for ((o, &start), w) in out_row
            .iter_mut()
            .zip(&f.starts)
            .zip(f.weights.chunks_exact(f.taps))
        {
            *o = clamp_u8((dot(&row[start..start + f.taps], w) + round) >> WEIGHT_BITS);
        }
    }
    out
}

fn dot(samples: &[u8], weights: &[i32]) -> i32 {
    samples
        .iter()
        .zip(weights)
        .map(|(&s, &w)| s as i32 * w)
        .sum()
}

/// Vertical pass over samples carrying `frac_bits` fractional bits,
/// producing final 8-bit samples. Rows are accumulated whole so the inner
/// loop runs along contiguous memory.
fn vertical<T: Copy + Into<i32>>(
    src: &[T],
    width: usize,
    frac_bits: u32,
    f: &AxisFilter,
    dh: usize,
) -> Vec<u8> {
    let shift = WEIGHT_BITS + frac_bits;
    let mut out = vec![0u8; width * dh];
    let mut acc = vec![0i32; width];
    for ((out_row, &start), w) in out
        .chunks_exact_mut(width)
        .zip(&f.starts)
        .zip(f.weights.chunks_exact(f.taps))
    {
        acc.fill(1 << (shift - 1));
        for (k, &wk) in w.iter().enumerate() {
            if wk == 0 {
                continue;
            }
            let row = &src[(start + k) * width..(start + k + 1) * width];
            for (a, &s) in acc.iter_mut().zip(row) {
                *a += wk * s.into();
            }
        }
        for (o, &a) in out_row.iter_mut().zip(&acc) {
            *o = clamp_u8(a >> shift);
        }
    }
    out
}

fn pack_rgb(r: &Plane, g: &Plane, b: &Plane) -> Plane {
    let mut data = Vec::with_capacity(r.data.len() * 3);
    for ((&r, &g), &b) in r.data.iter().zip(&g.data).zip(&b.data) {
        data.extend_from_slice(&[r, g, b]);
    }
    Plane {
        width: r.width * 3,
        height: r.height,
        data,
    }
}

/// Full resolution Y, Cb, Cr planes to packed RGB.
fn yuv_planes_to_rgb(y: &Plane, u: &Plane, v: &Plane) -> Plane {
    let mut data = vec![0u8; y.data.len() * 3];
    for (((out, &y), &u), &v) in data
        .chunks_exact_mut(3)
        .zip(&y.data)
        .zip(&u.data)
        .zip(&v.data)
    {
        out.copy_from_slice(&yuv_to_rgb([y, u, v]));
    }
    Plane {
        width: y.width * 3,
        height: y.height,
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rational::Rational;

    const ALGORITHMS: [ScaleAlgorithm; 4] = [
        ScaleAlgorithm::Nearest,
        ScaleAlgorithm::Bilinear,
        ScaleAlgorithm::Bicubic,
        ScaleAlgorithm::Area,
    ];

    /// A deterministic picture with detail in every plane.
    fn pattern(width: u32, height: u32, format: PixelFormat) -> VideoFrame {
        let mut f = VideoFrame::new_black(width, height, format, 7, Rational::new(1, 25));
        let mut seed = 0x1234_5678u32;
        for (p, plane) in f.planes.iter_mut().enumerate() {
            for v in plane.iter_mut() {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                let r = (seed >> 16) as u8;
                *v = if format.is_yuv() {
                    if p == 0 { 16 + r % 220 } else { 16 + r % 225 }
                } else {
                    r
                };
            }
        }
        f
    }

    fn solid(width: u32, height: u32, format: PixelFormat, values: [u8; 3]) -> VideoFrame {
        let mut f = VideoFrame::new_black(width, height, format, 0, Rational::new(1, 25));
        if format == PixelFormat::Rgb24 {
            for px in f.planes[0].chunks_exact_mut(3) {
                px.copy_from_slice(&values);
            }
        } else {
            for (plane, v) in f.planes.iter_mut().zip(values) {
                plane.fill(v);
            }
        }
        f
    }

    fn rgb_pixel(f: &VideoFrame, x: usize, y: usize) -> [u8; 3] {
        let i = (y * f.width as usize + x) * 3;
        [f.planes[0][i], f.planes[0][i + 1], f.planes[0][i + 2]]
    }

    #[test]
    fn identity_is_exact_for_every_format() {
        for format in PixelFormat::ALL {
            let f = pattern(9, 7, format);
            for alg in ALGORITHMS {
                assert_eq!(convert(&f, 9, 7, format, alg).unwrap(), f);
            }
        }
    }

    #[test]
    fn identity_filters_are_skipped() {
        let same = AxisMap {
            src_picture: 9,
            src_factor: 2,
            dst_picture: 9,
            dst_factor: 2,
        };
        assert!(same.filter(ScaleAlgorithm::Bicubic).is_none());
        // Same scale but a different length still needs a filter.
        let other = AxisMap {
            dst_picture: 10,
            ..same
        };
        assert!(other.filter(ScaleAlgorithm::Bicubic).is_some());
    }

    #[test]
    fn coefficients_match_the_bt601_formulas() {
        let s = 65536.0;
        let (kr, kb) = (0.299, 0.114);
        let kg = 1.0 - kr - kb;
        let close = |q: i32, x: f64| (q as f64 - x * s).abs() <= 1.0;
        assert!(close(16829, 219.0 / 255.0 * kr));
        assert!(close(33039, 219.0 / 255.0 * kg));
        assert!(close(6416, 219.0 / 255.0 * kb));
        assert!(close(-9714, -224.0 / 255.0 * kr / 1.772));
        assert!(close(28784, 224.0 / 255.0 * (1.0 - kb) / 1.772));
        assert!(close(-24103, -224.0 / 255.0 * kg / 1.402));
        assert!(close(-4681, -224.0 / 255.0 * kb / 1.402));
        assert!(close(76309, 255.0 / 219.0));
        assert!(close(104597, 255.0 / 224.0 * 1.402));
        assert!(close(132201, 255.0 / 224.0 * 1.772));
        assert!(close(25675, 255.0 / 224.0 * 1.772 * kb / kg));
        assert!(close(53279, 255.0 / 224.0 * 1.402 * kr / kg));
    }

    #[test]
    fn reference_colors() {
        let cases = [
            ([0, 0, 0], [16, 128, 128]),
            ([255, 255, 255], [235, 128, 128]),
            ([255, 0, 0], [81, 90, 240]),
            ([0, 255, 0], [145, 54, 34]),
            ([0, 0, 255], [41, 240, 110]),
            ([128, 128, 128], [126, 128, 128]),
        ];
        for (rgb, yuv) in cases {
            assert_eq!(rgb_to_yuv(rgb), yuv, "{rgb:?}");
            let back = yuv_to_rgb(yuv);
            for (a, b) in back.iter().zip(rgb) {
                assert!((*a as i32 - b as i32).abs() <= 1, "{rgb:?} -> {back:?}");
            }
            // Through whole frames, every YUV layout.
            for format in [
                PixelFormat::Yuv420p,
                PixelFormat::Yuv422p,
                PixelFormat::Yuv444p,
            ] {
                let f = convert(
                    &solid(4, 4, PixelFormat::Rgb24, rgb),
                    4,
                    4,
                    format,
                    Default::default(),
                )
                .unwrap();
                assert!(f.planes[0].iter().all(|&v| v == yuv[0]));
                assert!(f.planes[1].iter().all(|&v| v == yuv[1]));
                assert!(f.planes[2].iter().all(|&v| v == yuv[2]));
            }
        }
        assert_eq!(yuv_to_rgb([16, 128, 128]), [0, 0, 0]);
        assert_eq!(yuv_to_rgb([235, 128, 128]), [255, 255, 255]);
        // Out of range input clamps instead of wrapping.
        assert_eq!(yuv_to_rgb([0, 128, 128]), [0, 0, 0]);
        assert_eq!(yuv_to_rgb([255, 128, 128]), [255, 255, 255]);
        assert_eq!(rgb_to_gray([255, 255, 255]), 255);
        assert_eq!(rgb_to_gray([0, 0, 0]), 0);
        assert_eq!(rgb_to_gray([255, 0, 0]), 76);
    }

    #[test]
    fn rgb_yuv444_round_trip_within_two() {
        let mut worst = 0;
        for r in (0..=255).step_by(5) {
            for g in (0..=255).step_by(3) {
                for b in (0..=255).step_by(7) {
                    let back = yuv_to_rgb(rgb_to_yuv([r, g, b]));
                    for (x, y) in back.iter().zip([r, g, b]) {
                        worst = worst.max((*x as i32 - y as i32).abs());
                    }
                }
            }
        }
        assert!(worst <= 2, "worst error {worst}");
        let f = pattern(13, 11, PixelFormat::Rgb24);
        let yuv = convert(&f, 13, 11, PixelFormat::Yuv444p, ScaleAlgorithm::Nearest).unwrap();
        let back = convert(&yuv, 13, 11, PixelFormat::Rgb24, ScaleAlgorithm::Nearest).unwrap();
        for (a, b) in back.planes[0].iter().zip(&f.planes[0]) {
            assert!((*a as i32 - *b as i32).abs() <= 2);
        }
    }

    #[test]
    fn gray_and_luma_ranges() {
        let gray = solid(3, 3, PixelFormat::Gray, [255, 0, 0]);
        let yuv = convert(&gray, 3, 3, PixelFormat::Yuv420p, Default::default()).unwrap();
        assert!(yuv.planes[0].iter().all(|&v| v == 235));
        assert!(yuv.planes[1].iter().all(|&v| v == 128));
        let back = convert(&yuv, 3, 3, PixelFormat::Gray, Default::default()).unwrap();
        assert_eq!(back, gray);
        // Every gray level survives the trip through limited range luma
        // within the precision the narrower range allows.
        let lut_in = gray_to_luma_lut();
        let lut_out = luma_to_gray_lut();
        for g in 0..=255u8 {
            let r = lut_out[lut_in[g as usize] as usize];
            assert!((r as i32 - g as i32).abs() <= 1);
        }
        assert_eq!(lut_out[0], 0);
        assert_eq!(lut_out[255], 255);
        let rgb = convert(&gray, 3, 3, PixelFormat::Rgb24, Default::default()).unwrap();
        assert!(rgb.planes[0].iter().all(|&v| v == 255));
        let g2 = convert(&rgb, 3, 3, PixelFormat::Gray, Default::default()).unwrap();
        assert_eq!(g2, gray);
    }

    #[test]
    fn solid_color_stays_solid_after_any_scaling() {
        let sizes = [(1, 1), (3, 5), (17, 9), (64, 48), (100, 7)];
        for format in PixelFormat::ALL {
            let values = match format {
                PixelFormat::Rgb24 => [200, 30, 99],
                PixelFormat::Gray => [77, 0, 0],
                _ => [180, 60, 200],
            };
            let f = solid(31, 23, format, values);
            for target in PixelFormat::ALL {
                let expected = convert(
                    &solid(1, 1, format, values),
                    1,
                    1,
                    target,
                    Default::default(),
                )
                .unwrap();
                for alg in ALGORITHMS {
                    for &(w, h) in &sizes {
                        let out = convert(&f, w, h, target, alg).unwrap();
                        out.validate().unwrap();
                        for (plane, e) in out.planes.iter().zip(&expected.planes) {
                            let unit = if target == PixelFormat::Rgb24 { 3 } else { 1 };
                            for px in plane.chunks_exact(unit) {
                                assert_eq!(px, &e[..unit], "{format} -> {target} {w}x{h} {alg:?}");
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn every_pair_works_with_odd_sizes() {
        for from in PixelFormat::ALL {
            let f = pattern(7, 5, from);
            for to in PixelFormat::ALL {
                for alg in ALGORITHMS {
                    for (w, h) in [(7, 5), (3, 3), (11, 9), (1, 1), (2, 13)] {
                        let out = convert(&f, w, h, to, alg).unwrap();
                        out.validate().unwrap();
                        assert_eq!((out.width, out.height, out.format), (w, h, to));
                        assert_eq!(out.pts, 7);
                        assert_eq!(out.time_base, Rational::new(1, 25));
                    }
                }
            }
        }
    }

    #[test]
    fn area_halving_averages_exactly() {
        let mut f = VideoFrame::new_black(4, 2, PixelFormat::Gray, 0, Rational::new(1, 1));
        f.planes[0] = vec![10, 20, 0, 255, 30, 41, 1, 0];
        let out = convert(&f, 2, 1, PixelFormat::Gray, ScaleAlgorithm::Area).unwrap();
        // (10 + 20 + 30 + 41) / 4 = 25.25 and (0 + 255 + 1 + 0) / 4 = 64.
        assert_eq!(out.planes[0], vec![25, 64]);
        let f = pattern(64, 48, PixelFormat::Gray);
        let out = convert(&f, 32, 24, PixelFormat::Gray, ScaleAlgorithm::Area).unwrap();
        for y in 0..24 {
            for x in 0..32 {
                let p = |dx: usize, dy: usize| f.planes[0][(2 * y + dy) * 64 + 2 * x + dx] as u32;
                let sum = p(0, 0) + p(1, 0) + p(0, 1) + p(1, 1);
                assert_eq!(out.planes[0][y * 32 + x] as u32, (sum + 2) / 4);
            }
        }
    }

    #[test]
    fn chroma_subsampling_averages_and_upsampling_interpolates() {
        // A yuv444p picture with two chroma columns of 100 and 200: the
        // 4:2:0 chroma sample covering both is their average.
        let mut f = VideoFrame::new_black(2, 2, PixelFormat::Yuv444p, 0, Rational::new(1, 1));
        f.planes[1] = vec![100, 200, 100, 200];
        let out = convert(&f, 2, 2, PixelFormat::Yuv420p, ScaleAlgorithm::Nearest).unwrap();
        assert_eq!(out.planes[1], vec![150]);
        // Upsampling a 4:2:0 row of 100, 200 to full resolution: luma
        // columns sit a quarter chroma sample from the chroma centers.
        let mut f = VideoFrame::new_black(4, 2, PixelFormat::Yuv420p, 0, Rational::new(1, 1));
        f.planes[1] = vec![100, 200];
        let out = convert(&f, 4, 2, PixelFormat::Yuv444p, ScaleAlgorithm::Nearest).unwrap();
        assert_eq!(&out.planes[1][..4], &[100, 125, 175, 200]);
        assert_eq!(&out.planes[1][4..], &[100, 125, 175, 200]);
    }

    #[test]
    fn scaling_kernels_behave() {
        let mut f = VideoFrame::new_black(2, 1, PixelFormat::Gray, 0, Rational::new(1, 1));
        f.planes[0] = vec![0, 200];
        let near = convert(&f, 4, 1, PixelFormat::Gray, ScaleAlgorithm::Nearest).unwrap();
        assert_eq!(near.planes[0], vec![0, 0, 200, 200]);
        let lin = convert(&f, 4, 1, PixelFormat::Gray, ScaleAlgorithm::Bilinear).unwrap();
        assert_eq!(lin.planes[0], vec![0, 50, 150, 200]);
        let area = convert(&f, 4, 1, PixelFormat::Gray, ScaleAlgorithm::Area).unwrap();
        assert_eq!(area.planes[0], vec![0, 0, 200, 200]);
        let cubic_out = convert(&f, 4, 1, PixelFormat::Gray, ScaleAlgorithm::Bicubic).unwrap();
        // The negative lobes overshoot past the edge values (clamped).
        assert_eq!(cubic_out.planes[0][0], 0);
        assert!(cubic_out.planes[0][3] > 200);
        assert!(cubic_out.planes[0][1] < cubic_out.planes[0][2]);
        // Keys kernel: interpolating, and the a = -0.5 lobes.
        assert_eq!(cubic(0.0), 1.0);
        assert_eq!(cubic(1.0), 0.0);
        assert_eq!(cubic(2.0), 0.0);
        assert!((cubic(0.5) - 0.5625).abs() < 1e-12);
        assert!((cubic(1.5) + 0.0625).abs() < 1e-12);
        // Reduction widens the kernel: a 1 pixel line on a 4 to 1
        // bilinear reduction still contributes.
        let mut f = VideoFrame::new_black(8, 1, PixelFormat::Gray, 0, Rational::new(1, 1));
        f.planes[0][1] = 255;
        let out = convert(&f, 2, 1, PixelFormat::Gray, ScaleAlgorithm::Bilinear).unwrap();
        assert!(out.planes[0][0] > 0);
    }

    #[test]
    fn rgb_scaling_keeps_channels_apart() {
        let mut f = solid(4, 4, PixelFormat::Rgb24, [255, 0, 0]);
        for y in 0..4 {
            for x in 2..4 {
                let i = (y * 4 + x) * 3;
                f.planes[0][i..i + 3].copy_from_slice(&[0, 0, 255]);
            }
        }
        let out = convert(&f, 2, 2, PixelFormat::Rgb24, ScaleAlgorithm::Area).unwrap();
        assert_eq!(rgb_pixel(&out, 0, 0), [255, 0, 0]);
        assert_eq!(rgb_pixel(&out, 1, 1), [0, 0, 255]);
    }

    #[test]
    fn enlargement_and_reduction_preserve_content() {
        let f = pattern(40, 30, PixelFormat::Yuv420p);
        for alg in ALGORITHMS {
            let up = convert(&f, 80, 60, PixelFormat::Yuv420p, alg).unwrap();
            let down = convert(&up, 40, 30, PixelFormat::Yuv420p, ScaleAlgorithm::Area).unwrap();
            // Average luma is preserved through both directions.
            let mean = |p: &[u8]| p.iter().map(|&v| v as u64).sum::<u64>() / p.len() as u64;
            let (a, b) = (mean(&f.planes[0]), mean(&down.planes[0]));
            assert!(a.abs_diff(b) <= 1, "{alg:?}: {a} vs {b}");
        }
    }

    #[test]
    fn rejects_bad_sizes_and_malformed_frames() {
        let f = pattern(4, 4, PixelFormat::Yuv420p);
        assert!(convert(&f, 0, 4, PixelFormat::Yuv420p, Default::default()).is_err());
        assert!(
            convert(
                &f,
                4,
                MAX_DIMENSION + 1,
                PixelFormat::Rgb24,
                Default::default()
            )
            .is_err()
        );
        let mut bad = f.clone();
        bad.planes[1].pop();
        assert!(convert(&bad, 2, 2, PixelFormat::Rgb24, Default::default()).is_err());
        let mut bad = f.clone();
        bad.planes.pop();
        assert!(convert(&bad, 2, 2, PixelFormat::Gray, Default::default()).is_err());
        let mut bad = f;
        bad.width = 0;
        bad.planes = vec![Vec::new(), Vec::new(), Vec::new()];
        assert!(convert(&bad, 2, 2, PixelFormat::Gray, Default::default()).is_err());
    }

    #[test]
    fn huge_reduction_and_enlargement() {
        let f = pattern(1000, 3, PixelFormat::Gray);
        let out = convert(&f, 1, 1, PixelFormat::Gray, ScaleAlgorithm::Bicubic).unwrap();
        assert_eq!(out.planes[0].len(), 1);
        let f = pattern(1, 1, PixelFormat::Yuv420p);
        let out = convert(&f, 300, 2, PixelFormat::Rgb24, ScaleAlgorithm::Bicubic).unwrap();
        out.validate().unwrap();
        // A single source pixel enlarges to a solid picture.
        assert!(
            out.planes[0]
                .chunks_exact(3)
                .all(|px| px == &out.planes[0][..3])
        );
    }

    /// Builds a frame from planes stored back to back.
    fn from_bytes(width: u32, height: u32, format: PixelFormat, bytes: &[u8]) -> VideoFrame {
        let mut f = VideoFrame::new_black(width, height, format, 0, Rational::new(1, 25));
        let mut rest = bytes;
        for plane in &mut f.planes {
            let (head, tail) = rest.split_at(plane.len());
            plane.copy_from_slice(head);
            rest = tail;
        }
        assert!(rest.is_empty());
        f
    }

    fn psnr(a: &[u8], b: &[u8]) -> f64 {
        let se: f64 = a
            .iter()
            .zip(b)
            .map(|(&x, &y)| (x as f64 - y as f64).powi(2))
            .sum();
        let mse = se / a.len() as f64;
        if mse == 0.0 {
            f64::INFINITY
        } else {
            10.0 * (255.0f64 * 255.0 / mse).log10()
        }
    }

    /// Outputs of an independent implementation of the same definitions
    /// (center sited chroma, BT.601 limited range, clamped edges) on a
    /// 64x48 test picture, stored as raw planes.
    #[test]
    fn matches_reference_outputs() {
        const DIR: &str = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/video-processing/"
        );
        let read = |name: &str| std::fs::read(format!("{DIR}{name}")).unwrap();
        let src = from_bytes(64, 48, PixelFormat::Yuv420p, &read("src_64x48.yuv"));
        // Exact 2:1 box averages.
        let area = convert(&src, 32, 24, PixelFormat::Yuv420p, ScaleAlgorithm::Area).unwrap();
        assert_eq!(area.planes.concat(), read("area_32x24.yuv"));
        // Bilinear reduction by 1.6: rounding may differ by one step.
        let lin = convert(&src, 40, 30, PixelFormat::Yuv420p, ScaleAlgorithm::Bilinear).unwrap();
        let expected = read("bilinear_40x30.yuv");
        let got = lin.planes.concat();
        assert!(got.iter().zip(&expected).all(|(a, b)| a.abs_diff(*b) <= 1));
        // Chroma upsampling and the YUV to RGB matrix.
        let rgb = convert(&src, 64, 48, PixelFormat::Rgb24, ScaleAlgorithm::Bilinear).unwrap();
        let p = psnr(&rgb.planes[0], &read("rgb24_64x48.rgb"));
        assert!(p > 50.0, "PSNR {p}");
    }
}
