//! Sample rate conversion by band-limited interpolation.
//!
//! Every output sample is a weighted sum of input samples around its exact
//! position on the input time axis, with weights taken from a windowed
//! sinc low-pass filter. The method is the band-limited interpolation
//! described by J. O. Smith and P. Gossett ("A flexible sampling-rate
//! conversion method", ICASSP 1984) and in Smith's "Digital Audio
//! Resampling" notes. The window is a Kaiser window, sized with the design
//! equations from J. F. Kaiser, "Nonrecursive digital filter design using
//! the I0-sinh window function" (1974), as given in Oppenheim and Schafer,
//! "Discrete-Time Signal Processing", section 7.6.
//!
//! Design targets:
//!
//! * passband flat up to 0.91 of the lower of the two Nyquist frequencies,
//!   stopband from that Nyquist frequency on, at least 90 dB down (the
//!   filter is designed for 100 dB to leave room for coefficient and
//!   interpolation error);
//! * zero delay: the filter is centred on each output position, so output
//!   sample 0 lies at input time 0, and after `flush` the output holds
//!   exactly `round(input_len * out_rate / in_rate)` samples;
//! * output that does not depend on how the input is split into chunks:
//!   positions are tracked as exact fractions and an output sample is only
//!   computed once all the input it needs has arrived.
//!
//! When the reduced ratio has few phases the coefficients of every phase
//! are precomputed (polyphase). Otherwise, for ratios such as 44100 to
//! 48001, a finely sampled table of the prototype filter is interpolated
//! linearly for each output sample.

use crate::error::{Error, Result};
use crate::util::dot::dot;

/// Highest rate accepted on either side.
const MAX_RATE: u32 = 768_000;

/// Passband edge as a fraction of the lower Nyquist frequency.
const PASSBAND: f64 = 0.91;

/// Stopband attenuation the Kaiser window is designed for, in dB.
const ATTENUATION_DB: f64 = 100.0;

/// Cap on the half width of the filter in input samples. Downsampling by
/// a factor r stretches the filter by r; past roughly 900:1 the window is
/// shortened instead, which widens the transition band but keeps memory
/// bounded (the history holds two half widths per channel).
const MAX_HALF_SPAN: usize = 1 << 16;

/// Largest polyphase table (phases times taps) built up front.
const POLYPHASE_BUDGET: usize = 1 << 20;

/// Prototype table resolution, in points per sample of the lower rate.
/// Linear interpolation between points this close keeps the error of the
/// weights near 1e-8, far below the stopband.
const TABLE_STEPS: usize = 4096;

/// Streaming resampler for interleaved float samples.
pub struct Resampler {
    channels: usize,
    /// `None` when both rates are equal: samples pass through unchanged.
    state: Option<Box<State>>,
}

struct State {
    /// Reduced ratio: `up` output samples span `down` input samples.
    up: u64,
    down: u64,
    /// The filter covers input frames `pos - half + 1 ..= pos + half`
    /// around the integer part `pos` of each output position.
    half: usize,
    kernel: Kernel,
    /// Input samples of each channel, starting at absolute frame
    /// `hist_start`. The history starts with `half` frames of silence
    /// before time 0, which is what centres the first output samples on
    /// the first input. Channels are kept apart so the filter runs over
    /// contiguous memory.
    history: Vec<Vec<f32>>,
    hist_start: i64,
    received: u64,
    produced: u64,
    /// Position of the next output sample on the input axis:
    /// `pos + rem / up`, kept as exact integers so it never drifts.
    pos: i64,
    rem: u64,
    weights: Vec<f32>,
}

enum Kernel {
    /// `up` rows of `2 * half` weights, each row normalized to sum to 1.
    Polyphase(Vec<f32>),
    /// Prototype sampled every `1 / TABLE_STEPS` of a lower rate sample,
    /// from the centre outwards.
    Table {
        table: Vec<f64>,
        /// Input samples to lower rate samples (1 when upsampling).
        scale: f64,
    },
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32, channels: u16) -> Result<Self> {
        for rate in [in_rate, out_rate] {
            if rate == 0 || rate > MAX_RATE {
                return Err(Error::invalid_argument(format!(
                    "sample rate {rate} Hz is out of range (1 to {MAX_RATE})"
                )));
            }
        }
        if channels == 0 {
            return Err(Error::invalid_argument("cannot resample zero channels"));
        }
        let channels = channels as usize;
        if in_rate == out_rate {
            return Ok(Resampler {
                channels,
                state: None,
            });
        }
        let g = gcd(in_rate as u64, out_rate as u64);
        let (up, down) = (out_rate as u64 / g, in_rate as u64 / g);
        // Lower rate samples per input sample: the filter is designed on
        // the lower rate's time scale and stretched for downsampling.
        let scale = (out_rate as f64 / in_rate as f64).min(1.0);

        let design = Design::new();
        let half = ((design.half_low / scale).ceil() as usize).clamp(1, MAX_HALF_SPAN);
        // Half width actually used, in lower rate samples. Equal to the
        // design value except when MAX_HALF_SPAN shortened the window.
        let half_low = half as f64 * scale;
        let proto = Prototype {
            cutoff: design.cutoff,
            beta: design.beta,
            half_low,
        };
        let taps = 2 * half;

        let kernel = if (up as usize).saturating_mul(taps) <= POLYPHASE_BUDGET {
            let mut rows = Vec::with_capacity(up as usize * taps);
            let mut row = vec![0.0; taps];
            for phase in 0..up {
                let frac = phase as f64 / up as f64;
                for (t, w) in row.iter_mut().enumerate() {
                    *w = proto.eval(tap_offset(t, half, frac).abs() * scale);
                }
                normalize(&mut row);
                rows.extend(row.iter().map(|&w| w as f32));
            }
            Kernel::Polyphase(rows)
        } else {
            let len = (half_low * TABLE_STEPS as f64).ceil() as usize + 2;
            let table = (0..len)
                .map(|i| proto.eval(i as f64 / TABLE_STEPS as f64))
                .collect();
            Kernel::Table { table, scale }
        };

        Ok(Resampler {
            channels,
            state: Some(Box::new(State {
                up,
                down,
                half,
                kernel,
                history: vec![vec![0.0; half]; channels],
                hist_start: -(half as i64),
                received: 0,
                produced: 0,
                pos: 0,
                rem: 0,
                weights: vec![0.0; taps],
            })),
        })
    }

    /// Converts interleaved float samples, returning whatever output is
    /// ready. Input and output lengths differ by the rate ratio and by the
    /// filter delay. Trailing samples that do not form a whole frame are
    /// ignored.
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        let whole = input.len() - input.len() % self.channels;
        let input = &input[..whole];
        let Some(state) = self.state.as_mut() else {
            return input.to_vec();
        };
        for (c, h) in state.history.iter_mut().enumerate() {
            h.extend(input[c..].iter().step_by(self.channels));
        }
        state.received += (whole / self.channels) as u64;
        let target = state.expected_len();
        let mut out = Vec::new();
        // An output sample is ready once the last input frame under the
        // filter has arrived. The length check matters only for extreme
        // downsampling, where the filter can be shorter than one output
        // period and the rounded final length is not yet known.
        while state.produced < target && state.pos + (state.half as i64) < state.received as i64 {
            state.emit(&mut out);
        }
        state.discard_consumed();
        out
    }

    /// Returns the samples still held in the filter at the end of input,
    /// treating everything after the last input sample as silence. The
    /// resampler is then ready for a new stream.
    pub fn flush(&mut self) -> Vec<f32> {
        let Some(state) = self.state.as_mut() else {
            return Vec::new();
        };
        let target = state.expected_len();
        let mut out = Vec::new();
        while state.produced < target {
            let needed = (state.pos + state.half as i64 + 1 - state.hist_start) as usize;
            for h in &mut state.history {
                if needed > h.len() {
                    h.resize(needed, 0.0);
                }
            }
            state.emit(&mut out);
            state.discard_consumed();
        }
        for h in &mut state.history {
            h.clear();
            h.resize(state.half, 0.0);
        }
        state.hist_start = -(state.half as i64);
        state.received = 0;
        state.produced = 0;
        state.pos = 0;
        state.rem = 0;
        out
    }
}

impl State {
    /// Output length for the input received so far:
    /// `round(received * up / down)`, halves rounded up.
    fn expected_len(&self) -> u64 {
        let n = self.received as u128 * self.up as u128;
        let d = self.down as u128;
        ((2 * n + d) / (2 * d)) as u64
    }

    /// Computes one output frame and advances the position. The caller
    /// guarantees the history covers the filter span.
    fn emit(&mut self, out: &mut Vec<f32>) {
        let taps = 2 * self.half;
        let weights: &[f32] = match &self.kernel {
            Kernel::Polyphase(rows) => &rows[self.rem as usize * taps..][..taps],
            Kernel::Table { table, scale } => {
                let frac = self.rem as f64 / self.up as f64;
                let mut row: Vec<f64> = (0..taps)
                    .map(|t| lookup(table, tap_offset(t, self.half, frac).abs() * scale))
                    .collect();
                normalize(&mut row);
                for (w, r) in self.weights.iter_mut().zip(row) {
                    *w = r as f32;
                }
                &self.weights
            }
        };
        let first = (self.pos - self.half as i64 + 1 - self.hist_start) as usize;
        // Fixed summation order: the same output for any chunking.
        for h in &self.history {
            out.push(dot(&h[first..first + taps], weights));
        }
        self.produced += 1;
        self.rem += self.down;
        self.pos += (self.rem / self.up) as i64;
        self.rem %= self.up;
    }

    /// Drops input frames no future output can reach.
    fn discard_consumed(&mut self) {
        let first_needed = self.pos - self.half as i64 + 1;
        let have = self.history[0].len() as i64;
        let drop = (first_needed - self.hist_start).clamp(0, have);
        if drop > 0 {
            for h in &mut self.history {
                h.drain(..drop as usize);
            }
            self.hist_start += drop;
        }
    }
}

/// Kaiser design parameters for the lower rate's time scale.
struct Design {
    /// Cutoff in cycles per lower rate sample, midway through the
    /// transition band.
    cutoff: f64,
    beta: f64,
    /// Half length of the window in lower rate samples.
    half_low: f64,
}

impl Design {
    fn new() -> Self {
        let pass = 0.5 * PASSBAND;
        let stop = 0.5;
        let a = ATTENUATION_DB;
        // Kaiser's empirical formulas: shape parameter for attenuations
        // above 50 dB, and filter order from the transition width in
        // radians per sample.
        let beta = 0.1102 * (a - 8.7);
        let order = (a - 8.0) / (2.285 * std::f64::consts::TAU * (stop - pass));
        Design {
            cutoff: 0.5 * (pass + stop),
            beta,
            half_low: (order / 2.0).ceil(),
        }
    }
}

/// The windowed sinc, as a function of the distance to its centre in
/// lower rate samples. The overall gain does not matter: weights are
/// normalized per output sample so DC passes exactly.
struct Prototype {
    cutoff: f64,
    beta: f64,
    half_low: f64,
}

impl Prototype {
    fn eval(&self, u: f64) -> f64 {
        if u >= self.half_low {
            return 0.0;
        }
        let r = u / self.half_low;
        sinc(2.0 * self.cutoff * u) * bessel_i0(self.beta * (1.0 - r * r).sqrt())
            / bessel_i0(self.beta)
    }
}

/// Distance in input samples from the output position `pos + frac` to tap
/// `t`, which sits at input frame `pos - half + 1 + t`.
fn tap_offset(t: usize, half: usize, frac: f64) -> f64 {
    (half as f64 - 1.0 - t as f64) + frac
}

fn lookup(table: &[f64], u: f64) -> f64 {
    let x = u * TABLE_STEPS as f64;
    let i = x as usize;
    if i + 1 >= table.len() {
        return 0.0;
    }
    let f = x - i as f64;
    table[i] + f * (table[i + 1] - table[i])
}

fn normalize(weights: &mut [f64]) {
    let sum: f64 = weights.iter().sum();
    if sum.abs() > f64::EPSILON {
        weights.iter_mut().for_each(|w| *w /= sum);
    }
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        let px = std::f64::consts::PI * x;
        px.sin() / px
    }
}

/// Zeroth order modified Bessel function of the first kind, from its power
/// series. The terms shrink fast for the arguments a Kaiser window needs.
fn bessel_i0(x: f64) -> f64 {
    let q = x * x / 4.0;
    let mut term = 1.0;
    let mut sum = 1.0;
    let mut k = 1.0;
    while term > sum * 1e-17 {
        term *= q / (k * k);
        sum += term;
        k += 1.0;
    }
    sum
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(r: &mut Resampler, input: &[f32], chunk: usize) -> Vec<f32> {
        let mut out = Vec::new();
        let step = chunk * r.channels;
        for c in input.chunks(step.max(1)) {
            out.extend(r.process(c));
        }
        out.extend(r.flush());
        out
    }

    fn resample(input: &[f32], from: u32, to: u32, channels: u16) -> Vec<f32> {
        let mut r = Resampler::new(from, to, channels).unwrap();
        run(&mut r, input, 1000)
    }

    fn sine(freq: f64, rate: u32, n: usize, amp: f64) -> Vec<f32> {
        (0..n)
            .map(|i| (amp * (std::f64::consts::TAU * freq * i as f64 / rate as f64).sin()) as f32)
            .collect()
    }

    /// Least squares fit of `a sin + b cos` at `freq`, returning the
    /// amplitude and the RMS of what is left, both over `range`.
    fn fit(x: &[f32], rate: u32, freq: f64, range: std::ops::Range<usize>) -> (f64, f64) {
        let w = std::f64::consts::TAU * freq / rate as f64;
        let (mut ss, mut cc, mut sc, mut xs, mut xc) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for i in range.clone() {
            let (s, c) = (w * i as f64).sin_cos();
            let v = x[i] as f64;
            ss += s * s;
            cc += c * c;
            sc += s * c;
            xs += v * s;
            xc += v * c;
        }
        let det = ss * cc - sc * sc;
        let a = (xs * cc - xc * sc) / det;
        let b = (xc * ss - xs * sc) / det;
        let mut residual = 0.0;
        for i in range.clone() {
            let (s, c) = (w * i as f64).sin_cos();
            let e = x[i] as f64 - a * s - b * c;
            residual += e * e;
        }
        (
            (a * a + b * b).sqrt(),
            (residual / range.len() as f64).sqrt(),
        )
    }

    /// Magnitude of the DFT bin at `freq` (Goertzel), scaled to amplitude.
    fn goertzel(x: &[f32], rate: u32, freq: f64) -> f64 {
        let w = std::f64::consts::TAU * freq / rate as f64;
        let coeff = 2.0 * w.cos();
        let (mut s1, mut s2) = (0.0, 0.0);
        for &v in x {
            let s = v as f64 + coeff * s1 - s2;
            s2 = s1;
            s1 = s;
        }
        let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
        2.0 * power.max(0.0).sqrt() / x.len() as f64
    }

    fn db(x: f64) -> f64 {
        20.0 * x.max(1e-20).log10()
    }

    #[test]
    fn output_length_is_exact() {
        let pairs = [
            (44100, 48000),
            (48000, 44100),
            (44100, 8000),
            (8000, 44100),
            (22050, 44100),
            (96000, 48000),
            (44100, 48001),
            (7, 11),
            (1, 3),
            (3, 1),
            (768000, 1),
            (1, 768000),
            (768000, 44100),
            (11025, 768000),
        ];
        for (from, to) in pairs {
            for n in [0usize, 1, 2, 3, 10, 101, 1000] {
                // Keep the extreme upsampling cases short.
                if to as usize * n / from as usize > 1_000_000 {
                    continue;
                }
                let input = vec![0.25f32; n * 2];
                let out = resample(&input, from, to, 2);
                let expected =
                    ((2 * n as u128 * to as u128 + from as u128) / (2 * from as u128)) as usize;
                assert_eq!(out.len(), expected * 2, "{from} -> {to}, {n} frames");
            }
        }
    }

    #[test]
    fn sine_44100_to_48000_keeps_frequency_and_level() {
        let input = sine(1000.0, 44100, 44100, 0.5);
        let out = resample(&input, 44100, 48000, 1);
        assert_eq!(out.len(), 48000);
        // Skip the edges, where the abrupt start and end of the tone ring.
        let (amp, residual) = fit(&out, 48000, 1000.0, 1000..47000);
        let level_error = db(amp / 0.5);
        let floor = db(residual / 0.5);
        assert!(level_error.abs() < 0.001, "level error {level_error} dB");
        // Everything that is not the 1 kHz tone (images, aliases,
        // interpolation error) lies below -90 dB.
        assert!(floor < -90.0, "residual {floor} dB");
        // The peak is at 1 kHz: neighbouring bins hold almost nothing.
        let mid = &out[4000..44000];
        let peak = goertzel(mid, 48000, 1000.0);
        assert!((peak - 0.5).abs() < 1e-3, "peak {peak}");
        assert!(goertzel(mid, 48000, 1012.0) < 1e-3);
    }

    #[test]
    fn sine_44100_to_8000_keeps_frequency_and_level() {
        let input = sine(1000.0, 44100, 44100, 0.5);
        let out = resample(&input, 44100, 8000, 1);
        assert_eq!(out.len(), 8000);
        let (amp, residual) = fit(&out, 8000, 1000.0, 400..7600);
        assert!(db(amp / 0.5).abs() < 0.001);
        assert!(
            db(residual / 0.5) < -90.0,
            "residual {}",
            db(residual / 0.5)
        );
    }

    #[test]
    fn downsampling_removes_content_above_the_new_nyquist() {
        // 6 kHz cannot exist at 8 kHz; it must not alias down to 2 kHz.
        let input = sine(6000.0, 44100, 44100, 0.5);
        let out = resample(&input, 44100, 8000, 1);
        let mid = &out[400..7600];
        let rms = (mid.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / mid.len() as f64).sqrt();
        assert!(db(rms / 0.5) < -90.0, "alias at {} dB", db(rms / 0.5));
    }

    #[test]
    fn upsampling_passband_edge_and_images() {
        // A tone at 0.9 of the input Nyquist is kept at full level, with
        // its image (at 44100 - f) far down.
        let f = 0.9 * 22050.0;
        let input = sine(f, 44100, 44100, 0.5);
        let out = resample(&input, 44100, 96000, 1);
        let (amp, residual) = fit(&out, 96000, f, 2000..94000);
        assert!(
            db(amp / 0.5).abs() < 0.01,
            "passband edge {} dB",
            db(amp / 0.5)
        );
        assert!(
            db(residual / 0.5) < -90.0,
            "image at {} dB",
            db(residual / 0.5)
        );
    }

    #[test]
    fn arbitrary_ratio_uses_the_table_and_keeps_quality() {
        let r = Resampler::new(44100, 48001, 1).unwrap();
        assert!(matches!(
            r.state.as_ref().unwrap().kernel,
            Kernel::Table { .. }
        ));
        let input = sine(1000.0, 44100, 44100, 0.5);
        let out = resample(&input, 44100, 48001, 1);
        assert_eq!(out.len(), 48001);
        let (amp, residual) = fit(&out, 48001, 1000.0, 1000..47000);
        assert!(db(amp / 0.5).abs() < 0.001);
        assert!(
            db(residual / 0.5) < -90.0,
            "residual {}",
            db(residual / 0.5)
        );
    }

    #[test]
    fn dc_stays_dc() {
        for (from, to) in [(44100, 48000), (48000, 8000), (44100, 48001), (8000, 44100)] {
            let input = vec![0.75f32; 4000];
            let out = resample(&input, from, to, 1);
            // Away from the edges the level is exactly the input level.
            let n = out.len();
            for &v in &out[n / 4..3 * n / 4] {
                assert!((v - 0.75).abs() < 1e-6, "{from} -> {to}: {v}");
            }
        }
    }

    #[test]
    fn chunking_does_not_change_the_output() {
        for (from, to) in [(44100, 48000), (48000, 44100), (44100, 48001), (3, 1)] {
            let input: Vec<f32> = (0..6000)
                .map(|i| ((i * 7919 % 1000) as f32 / 500.0) - 1.0)
                .collect();
            let reference = {
                let mut r = Resampler::new(from, to, 2).unwrap();
                let mut out = r.process(&input);
                out.extend(r.flush());
                out
            };
            for chunk in [1, 7, 64, 1024] {
                let mut r = Resampler::new(from, to, 2).unwrap();
                assert_eq!(
                    run(&mut r, &input, chunk),
                    reference,
                    "{from} -> {to}, {chunk}"
                );
            }
        }
    }

    #[test]
    fn impulse_response_is_symmetric() {
        // An impulse at input 147 (44100 Hz) lands exactly on output 160
        // (48000 Hz); the response must mirror around it.
        let mut input = vec![0.0f32; 400];
        input[147] = 1.0;
        let out = resample(&input, 44100, 48000, 1);
        let centre = 160;
        assert!(out[centre] > 0.9);
        for m in 1..150 {
            let (a, b) = (out[centre + m], out[centre - m]);
            assert!((a - b).abs() < 1e-6, "offset {m}: {a} vs {b}");
        }
    }

    #[test]
    fn integer_downsampling_impulse_is_centred() {
        let mut input = vec![0.0f32; 3000];
        input[1500] = 1.0;
        let out = resample(&input, 48000, 16000, 1);
        // Input 1500 is output 500.
        let peak = (0..out.len())
            .max_by(|&a, &b| out[a].total_cmp(&out[b]))
            .unwrap();
        assert_eq!(peak, 500);
        for m in 1..400 {
            assert!((out[500 + m] - out[500 - m]).abs() < 1e-6);
        }
    }

    #[test]
    fn channels_stay_separate() {
        let left = sine(1000.0, 44100, 8820, 0.5);
        let mut input = Vec::new();
        for &l in &left {
            input.push(l);
            input.push(0.0);
        }
        let out = resample(&input, 44100, 48000, 2);
        let mono = resample(&left, 44100, 48000, 1);
        for (i, frame) in out.chunks(2).enumerate() {
            assert_eq!(frame[0], mono[i]);
            assert_eq!(frame[1], 0.0);
        }
    }

    #[test]
    fn equal_rates_pass_through() {
        let mut r = Resampler::new(44100, 44100, 2).unwrap();
        let input = vec![0.1, 0.2, 0.3, 0.4];
        assert_eq!(r.process(&input), input);
        assert!(r.flush().is_empty());
    }

    #[test]
    fn flush_resets_for_a_new_stream() {
        let input = sine(500.0, 8000, 1000, 0.5);
        let mut r = Resampler::new(8000, 11025, 1).unwrap();
        let first = run(&mut r, &input, 100);
        let second = run(&mut r, &input, 33);
        assert_eq!(first, second);
    }

    #[test]
    fn partial_frames_are_ignored() {
        let mut r = Resampler::new(8000, 16000, 2).unwrap();
        r.process(&[0.5, 0.5, 0.5]);
        assert_eq!(r.flush().len(), 4);
    }

    #[test]
    fn rejects_bad_parameters() {
        assert!(Resampler::new(0, 48000, 1).is_err());
        assert!(Resampler::new(44100, 768_001, 1).is_err());
        assert!(Resampler::new(44100, 48000, 0).is_err());
        assert!(Resampler::new(1, 768_000, 8).is_ok());
    }

    #[test]
    fn kaiser_design_matches_the_formulas() {
        let d = Design::new();
        assert!((d.beta - 10.06).abs() < 0.01);
        assert_eq!(d.half_low, 72.0);
        assert!((d.cutoff - 0.4775).abs() < 1e-12);
        assert!((bessel_i0(0.0) - 1.0).abs() < 1e-15);
        // I0(1) = 1.2660658777520082...
        assert!((bessel_i0(1.0) - 1.266_065_877_752_008_4).abs() < 1e-14);
    }
}
