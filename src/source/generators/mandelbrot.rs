//! `mandelbrot`: a continuous zoom into the Mandelbrot set, colored by a
//! smooth escape count so the picture has no visible iteration bands.
//!
//! The set is the c for which z(n+1) = z(n)^2 + c, starting at z(0) = 0,
//! stays bounded. Points that escape are colored from how fast they do;
//! points that never escape within `maxiter` iterations are black.

use std::f64::consts::LN_2;

use super::draw::Rgb;
use super::{Painter, VideoGenerator, VideoOptions};
use crate::error::{Error, Result};
use crate::filter::Args;
use crate::source::Source;

/// Escape radius. Far larger than the minimal 2 so that the smooth count
/// below is accurate: its error shrinks as the radius grows.
const BAILOUT: f64 = 256.0;

/// Smallest distance between pixel centers the zoom goes down to. Doubles
/// carry about 16 significant digits; near |c| = 2 neighboring values are
/// 4e-16 apart, so 1e-13 leaves a few hundred representable values per
/// pixel and the picture never turns blocky. At that depth the zoom turns
/// around and heads back out.
const MIN_PIXEL_STEP: f64 = 1e-13;

/// Most iterations accepted, which bounds the work per pixel.
const MAX_ITERATIONS: u32 = 1 << 20;

/// Pictures smaller than this are drawn on the calling thread: starting
/// workers would cost more than it saves.
const PARALLEL_MIN_PIXELS: usize = 1 << 14;

/// The center of a spiral in the "seahorse valley" between the
/// main cardioid and the period 2 bulb. Around it the boundary keeps
/// unfolding new spirals at every depth without large interior regions,
/// which would be black and the slowest to compute.
const DEFAULT_CENTER: (f64, f64) = (-0.775_683_77, 0.136_467_37);

pub(crate) fn create(args: &Args) -> Result<Box<dyn Source>> {
    let options = VideoOptions::parse(
        args,
        "mandelbrot",
        &["maxiter", "start_x", "start_y", "start_scale", "zoom"],
    )?;
    let center = (
        args.f64(&["start_x"])?.unwrap_or(DEFAULT_CENTER.0),
        args.f64(&["start_y"])?.unwrap_or(DEFAULT_CENTER.1),
    );
    let view = View {
        max_iterations: args.u32(&["maxiter"])?.unwrap_or(256),
        center,
        start_scale: args.f64(&["start_scale"])?.unwrap_or(3.0),
        zoom: args.f64(&["zoom"])?.unwrap_or(1.5),
    };
    let painter = Mandelbrot::new(&options, view)?;
    Ok(Box::new(VideoGenerator::new(options, painter)))
}

#[derive(Clone, Copy, Debug)]
struct View {
    /// Iteration limit of the first frame. Points near the boundary take
    /// longer to escape the closer one looks, so a fixed limit would turn
    /// deep views black: the limit grows by this amount again for every
    /// decade of magnification.
    max_iterations: u32,
    /// Point the zoom closes in on, at the center of every frame.
    center: (f64, f64),
    /// Width of the first frame in the complex plane.
    start_scale: f64,
    /// Magnification per second.
    zoom: f64,
}

struct Mandelbrot {
    width: usize,
    height: usize,
    view: View,
    /// Seconds per frame, to make the zoom speed independent of the rate.
    frame_seconds: f64,
    /// Natural log of the deepest magnification before turning around.
    max_depth: f64,
    threads: usize,
}

impl Mandelbrot {
    fn new(options: &VideoOptions, view: View) -> Result<Self> {
        if !(1..=MAX_ITERATIONS).contains(&view.max_iterations) {
            return Err(Error::invalid_argument(format!(
                "mandelbrot maxiter must be 1 to {MAX_ITERATIONS}"
            )));
        }
        if view.center.0.abs() > 4.0 || view.center.1.abs() > 4.0 {
            return Err(Error::invalid_argument(
                "mandelbrot start_x and start_y must be between -4 and 4",
            ));
        }
        if !(1e-6..=100.0).contains(&view.start_scale) {
            return Err(Error::invalid_argument(
                "mandelbrot start_scale must be between 1e-6 and 100",
            ));
        }
        if !(1.0..=100.0).contains(&view.zoom) {
            return Err(Error::invalid_argument(
                "mandelbrot zoom must be between 1 (still) and 100 (per second)",
            ));
        }
        let width = options.width as usize;
        let max_depth = (view.start_scale / (MIN_PIXEL_STEP * width as f64))
            .ln()
            .max(0.0);
        let threads = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(16);
        Ok(Mandelbrot {
            width,
            height: options.height as usize,
            view,
            frame_seconds: options.seconds(1),
            max_depth,
            threads,
        })
    }

    /// Width of frame `index` in the complex plane. The log of the
    /// magnification grows linearly with time, which looks like a steady
    /// dive, and bounces between zero and `max_depth` so an endless source
    /// never runs out of precision.
    fn scale(&self, index: u64) -> f64 {
        let depth = index as f64 * self.frame_seconds * self.view.zoom.ln();
        let depth = if self.max_depth > 0.0 {
            let p = depth % (2.0 * self.max_depth);
            if p > self.max_depth {
                2.0 * self.max_depth - p
            } else {
                p
            }
        } else {
            0.0
        };
        self.view.start_scale * (-depth).exp()
    }

    /// Iteration limit for a frame `scale` wide: the base limit times one
    /// plus the number of decades zoomed in.
    fn iterations(&self, scale: f64) -> u32 {
        let decades = (self.view.start_scale / scale).log10().max(0.0);
        let limit = self.view.max_iterations as f64 * (1.0 + decades);
        limit.min(MAX_ITERATIONS as f64) as u32
    }

    /// Draws the given rows, each paired with its line number, with pixel
    /// centers `step` apart in the complex plane.
    fn draw_rows<'a>(
        &self,
        step: f64,
        iterations: u32,
        rows: impl Iterator<Item = (usize, &'a mut [u8])>,
    ) {
        let (cx, cy) = self.view.center;
        let half_w = self.width as f64 / 2.0;
        let half_h = self.height as f64 / 2.0;
        // Cycle detection threshold: well below a pixel so it never
        // swallows an escaping point, and never below what doubles near
        // the set can resolve.
        let tolerance = (step * 1e-3).max(1e-15);
        for (y, row) in rows {
            let ci = cy - (y as f64 + 0.5 - half_h) * step;
            for (x, px) in row.chunks_exact_mut(3).enumerate() {
                let cr = cx + (x as f64 + 0.5 - half_w) * step;
                let color = match escape(cr, ci, iterations, tolerance) {
                    Some(nu) => palette(nu),
                    None => [0, 0, 0],
                };
                px.copy_from_slice(&color);
            }
        }
    }
}

impl Painter for Mandelbrot {
    fn paint(&mut self, index: u64, pixels: &mut [u8]) {
        let scale = self.scale(index);
        let step = scale / self.width as f64;
        let iterations = self.iterations(scale);
        let stride = self.width * 3;
        let rows = pixels.chunks_exact_mut(stride).enumerate();
        if self.threads <= 1 || self.width * self.height < PARALLEL_MIN_PIXELS {
            self.draw_rows(step, iterations, rows);
            return;
        }
        // Rows are dealt out round robin rather than in bands: the cost of
        // a row depends on how much of the set it crosses, and interleaving
        // spreads the expensive rows over every worker. Each pixel depends
        // only on its coordinates, so the result is the same whatever the
        // thread count.
        let mut shares: Vec<Vec<(usize, &mut [u8])>> =
            (0..self.threads).map(|_| Vec::new()).collect();
        for (y, row) in rows {
            shares[y % self.threads].push((y, row));
        }
        let this = &*self;
        std::thread::scope(|scope| {
            for share in shares {
                scope.spawn(move || this.draw_rows(step, iterations, share.into_iter()));
            }
        });
    }
}

/// Smooth escape count of `c = cr + i ci`, or `None` if it is taken to be
/// in the set.
///
/// For an escaping point the count n + 1 - log2(ln|z(n)| / ln R), with
/// z(n) the first value beyond the radius R, varies continuously with c:
/// it follows from the Douady and Hubbard potential, which grows like
/// ln|z(n)| / 2^n.
fn escape(cr: f64, ci: f64, max_iterations: u32, tolerance: f64) -> Option<f64> {
    // Points inside the main cardioid or the period 2 disc never escape,
    // and they would otherwise cost the full iteration count.
    let xq = cr - 0.25;
    let q = xq * xq + ci * ci;
    if q * (q + xq) <= 0.25 * ci * ci {
        return None;
    }
    if (cr + 1.0) * (cr + 1.0) + ci * ci <= 1.0 / 16.0 {
        return None;
    }
    let (mut zr, mut zi) = (0.0f64, 0.0f64);
    // Brent's cycle detection: compare against a saved value replaced at
    // doubling intervals. An orbit that falls into a cycle comes back to
    // the saved value within one interval longer than its period, which
    // settles most interior points long before the iteration limit.
    let (mut saved_r, mut saved_i) = (0.0f64, 0.0f64);
    let mut interval = 8u32;
    let mut since_saved = 0u32;
    for n in 0..max_iterations {
        let zr2 = zr * zr;
        let zi2 = zi * zi;
        let modulus2 = zr2 + zi2;
        if modulus2 > BAILOUT * BAILOUT {
            let log_modulus = 0.5 * modulus2.ln();
            return Some(n as f64 + 1.0 - (log_modulus / BAILOUT.ln()).ln() / LN_2);
        }
        zi = 2.0 * zr * zi + ci;
        zr = zr2 - zi2 + cr;
        if (zr - saved_r).abs() < tolerance && (zi - saved_i).abs() < tolerance {
            return None;
        }
        since_saved += 1;
        if since_saved == interval {
            saved_r = zr;
            saved_i = zi;
            since_saved = 0;
            interval = interval.saturating_mul(2);
        }
    }
    None
}

/// Gradient stops, evenly spaced around a cycle: deep blue, azure, a
/// near white highlight, amber and a dark umber that leads back to blue.
const STOPS: [[f64; 3]; 5] = [
    [8.0, 20.0, 80.0],
    [32.0, 120.0, 216.0],
    [236.0, 246.0, 255.0],
    [255.0, 172.0, 32.0],
    [72.0, 20.0, 12.0],
];

/// Color for a smooth escape count. The count is compressed with a square
/// root so that the gradient spreads over both the fast escaping outside
/// and the slow points hugging the boundary, whose counts climb as the
/// zoom deepens.
fn palette(nu: f64) -> Rgb {
    let t = (nu.max(0.0).sqrt() * 0.35).fract() * STOPS.len() as f64;
    let i = (t as usize).min(STOPS.len() - 1);
    let f = t - i as f64;
    let (a, b) = (STOPS[i], STOPS[(i + 1) % STOPS.len()]);
    [0, 1, 2].map(|c| (a[c] + (b[c] - a[c]) * f).round().clamp(0.0, 255.0) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Frame;
    use crate::rational::Rational;
    use crate::source::open;

    fn view() -> View {
        View {
            max_iterations: 256,
            center: (-0.5, 0.0),
            start_scale: 3.0,
            zoom: 2.0,
        }
    }

    #[test]
    fn known_points() {
        // Interior: the origin, the period 2 bulb and a period 3 bulb (the
        // last one is only caught by the cycle detection).
        assert_eq!(escape(0.0, 0.0, 1000, 1e-12), None);
        assert_eq!(escape(-1.0, 0.0, 1000, 1e-12), None);
        assert_eq!(escape(-0.1225, 0.7449, 1000, 1e-12), None);
        assert_eq!(escape(-1.754_877_666, 0.0, 1000, 1e-12), None);
        // Exterior: c = 1 escapes after a handful of iterations.
        let nu = escape(1.0, 0.0, 1000, 1e-12).unwrap();
        assert!((3.0..6.0).contains(&nu), "nu {nu}");
        assert!(escape(-2.1, 0.0, 1000, 1e-12).is_some());
        assert!(escape(0.3, 0.6, 1000, 1e-12).is_some());
    }

    #[test]
    fn smooth_count_is_continuous() {
        // Walk towards the set along the real axis: the count rises with
        // no jumps of a whole iteration between neighboring samples.
        let mut last = None;
        for i in 0..2000 {
            let cr = 0.5 + 1.5 * (1.0 - i as f64 / 2000.0);
            let nu = escape(cr, 0.1, 10_000, 1e-15).unwrap();
            if let Some(prev) = last {
                assert!(nu >= prev - 1e-9, "count fell at {cr}");
                assert!(nu - prev < 0.5, "count jumped at {cr}: {prev} to {nu}");
            }
            last = Some(nu);
        }
    }

    #[test]
    fn zoom_bounces_within_precision() {
        let options = VideoOptions::new(320, 240, Rational::new(25, 1), None).unwrap();
        let m = Mandelbrot::new(&options, view()).unwrap();
        assert_eq!(m.scale(0), 3.0);
        // Two times per second at 25 fps: one second halves the width.
        assert!((m.scale(25) - 1.5).abs() < 1e-12);
        let mut deepest = f64::MAX;
        for i in (0..100_000).step_by(7) {
            let s = m.scale(i);
            assert!(s <= 3.0 + 1e-9 && s.is_finite());
            deepest = deepest.min(s);
        }
        assert!(deepest / 320.0 >= MIN_PIXEL_STEP * 0.999);
        assert!(deepest / 320.0 < 1e-11, "the zoom reaches deep views");
        let still = View {
            zoom: 1.0,
            ..view()
        };
        let m = Mandelbrot::new(&options, still).unwrap();
        assert_eq!(m.scale(1000), 3.0);
        assert_eq!(m.iterations(3.0), 256);
        assert_eq!(m.iterations(3e-3), 1024);
        let heavy = View {
            max_iterations: MAX_ITERATIONS / 2,
            ..view()
        };
        let m = Mandelbrot::new(&options, heavy).unwrap();
        assert_eq!(m.iterations(3e-6), MAX_ITERATIONS, "capped");
    }

    #[test]
    fn frames_are_deterministic_and_threading_does_not_matter() {
        let options = VideoOptions::new(160, 120, Rational::new(25, 1), None).unwrap();
        let mut a = Mandelbrot::new(&options, view()).unwrap();
        let mut b = Mandelbrot::new(&options, view()).unwrap();
        a.threads = 1;
        b.threads = 7;
        for index in [0, 10, 200] {
            let mut pa = vec![0u8; 160 * 120 * 3];
            let mut pb = vec![1u8; 160 * 120 * 3];
            a.paint(index, &mut pa);
            b.paint(index, &mut pb);
            assert_eq!(pa, pb, "frame {index}");
        }
    }

    #[test]
    fn first_frame_shows_the_set() {
        let options = VideoOptions::new(90, 60, Rational::new(25, 1), None).unwrap();
        let mut m = Mandelbrot::new(&options, view()).unwrap();
        let mut p = vec![0u8; 90 * 60 * 3];
        m.paint(0, &mut p);
        let at = |x: usize, y: usize| -> Rgb {
            let i = (y * 90 + x) * 3;
            [p[i], p[i + 1], p[i + 2]]
        };
        // Centered on -0.5 with a width of 3: the middle is in the main
        // cardioid, the corners far outside.
        assert_eq!(at(45, 30), [0, 0, 0]);
        assert_ne!(at(0, 0), [0, 0, 0]);
        assert_ne!(at(89, 59), [0, 0, 0]);
        // The picture is symmetric about the real axis.
        for y in 0..30 {
            for x in 0..90 {
                assert_eq!(at(x, y), at(x, 59 - y), "({x}, {y})");
            }
        }
    }

    #[test]
    fn palette_is_continuous() {
        // Counts start near 1: no point escapes before its first iteration.
        let mut last = palette(1.0);
        for i in 101..20_000 {
            let c = palette(i as f64 * 0.01);
            let jump = (0..3)
                .map(|k| (c[k] as i32 - last[k] as i32).abs())
                .max()
                .unwrap();
            assert!(jump <= 16, "jump of {jump} at {}", i as f64 * 0.01);
            last = c;
        }
    }

    #[test]
    fn options() {
        let mut s =
            open("mandelbrot=s=64x48:maxiter=50:start_x=-1:start_y=0.2:zoom=3:d=0.2").unwrap();
        let Some(Frame::Video(f)) = s.next_frame().unwrap() else {
            panic!()
        };
        assert_eq!((f.width, f.height), (64, 48));
        for bad in [
            "maxiter=0",
            "maxiter=99999999",
            "zoom=0.5",
            "start_x=7",
            "start_scale=0",
            "start_y=nan",
        ] {
            assert!(open(&format!("mandelbrot={bad}")).is_err(), "{bad}");
        }
    }

    /// Frame time at the default size over the first 60 seconds of the
    /// zoom, with all cores and with one, printed for benchmarking:
    /// `cargo test --release frame_time -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn frame_time() {
        let options = VideoOptions::new(320, 240, Rational::new(25, 1), Some(60.0)).unwrap();
        let defaults = View {
            max_iterations: 256,
            center: DEFAULT_CENTER,
            start_scale: 3.0,
            zoom: 1.5,
        };
        for threads in [0, 1] {
            let mut m = Mandelbrot::new(&options, defaults).unwrap();
            if threads > 0 {
                m.threads = threads;
            }
            let mut pixels = vec![0u8; 320 * 240 * 3];
            let start = std::time::Instant::now();
            let mut slowest = (std::time::Duration::ZERO, 0);
            let frames = options.total.unwrap();
            for index in 0..frames {
                let t = std::time::Instant::now();
                m.paint(index, &mut pixels);
                slowest = slowest.max((t.elapsed(), index));
            }
            println!(
                "{} threads: {frames} frames, {:.2} ms per frame on average, slowest {:.2} ms \
                 (frame {})",
                m.threads,
                start.elapsed().as_secs_f64() * 1e3 / frames as f64,
                slowest.0.as_secs_f64() * 1e3,
                slowest.1
            );
        }
    }
}
