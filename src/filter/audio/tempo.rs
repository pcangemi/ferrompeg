//! Tempo change without pitch change: `atempo=1.5`.
//!
//! Implements WSOLA, waveform similarity overlap-add (W. Verhelst and
//! M. Roelands, "An overlap-add technique based on waveform similarity
//! (WSOLA) for high quality time-scale modification of speech", ICASSP
//! 1993). The output is built from windowed segments of the input placed
//! every `hop` samples, with 50% overlap so the Hann windows add up to
//! exactly one. Segment `m` is taken near input position `m * hop * tempo`;
//! within a small search range the start that best matches the natural
//! continuation of the previous segment (normalized cross-correlation on a
//! mono mix) is chosen, so overlapping segments add in phase and the pitch
//! is kept.
//!
//! The output length is `round(input_len / tempo)` and output timestamps
//! are input timestamps divided by the tempo.

use crate::error::{Error, Result};
use crate::filter::{Args, Filter};
use crate::frame::{AudioFrame, Frame};

use super::{expect_audio, float_frames};

/// Segment length: long enough to hold a few periods of a low voice,
/// short enough that transients are not smeared.
const WINDOW_SECONDS: f64 = 0.03;

/// Search radius around the nominal position, which must cover at least
/// half a period of the lowest pitch to be matched.
const SEARCH_SECONDS: f64 = 0.01;

pub struct Atempo {
    tempo: f64,
    state: Option<State>,
}

pub fn create(args: &Args) -> Result<Box<dyn Filter>> {
    args.check_known("atempo", &["tempo"])?;
    let tempo = args.f64(&["tempo"])?.unwrap_or(1.0);
    if !(0.5..=2.0).contains(&tempo) {
        return Err(Error::invalid_argument(format!(
            "atempo tempo must be between 0.5 and 2 (chain several for more), not {tempo}"
        )));
    }
    Ok(Box::new(Atempo { tempo, state: None }))
}

struct State {
    tempo: f64,
    rate: u32,
    channels: usize,
    window: usize,
    hop: usize,
    search: usize,
    hann: Vec<f32>,
    /// Interleaved input and its mono mix, from absolute frame `in_start`.
    input: Vec<f32>,
    mono: Vec<f32>,
    in_start: i64,
    received: i64,
    /// Index of the next segment and input start of the previous one.
    segment: u64,
    prev_pos: i64,
    /// Overlap-add accumulator, starting at output frame `emitted`.
    out: Vec<f32>,
    emitted: i64,
    pts: i64,
}

impl State {
    fn new(tempo: f64, first: &AudioFrame) -> Self {
        let rate = first.sample_rate;
        let hop = ((rate as f64 * WINDOW_SECONDS / 2.0).round() as usize).max(1);
        let window = 2 * hop;
        let search = ((rate as f64 * SEARCH_SECONDS).round() as usize).clamp(1, hop);
        // Periodic Hann: w[n] + w[n + hop] = 1, so overlapping segments
        // keep the level constant.
        let hann = (0..window)
            .map(|n| {
                let x = std::f64::consts::TAU * n as f64 / window as f64;
                (0.5 - 0.5 * x.cos()) as f32
            })
            .collect();
        State {
            tempo,
            rate,
            channels: first.channels as usize,
            window,
            hop,
            search,
            hann,
            input: Vec::new(),
            mono: Vec::new(),
            in_start: 0,
            received: 0,
            segment: 0,
            prev_pos: 0,
            out: Vec::new(),
            emitted: 0,
            pts: (first.pts as f64 / tempo).round() as i64,
        }
    }

    fn push(&mut self, samples: &[f32]) {
        let ch = self.channels;
        self.input.extend_from_slice(samples);
        self.mono.extend(
            samples
                .chunks_exact(ch)
                .map(|f| f.iter().sum::<f32>() / ch as f32),
        );
        self.received += (samples.len() / ch) as i64;
    }

    fn nominal(&self, segment: u64) -> i64 {
        (segment as f64 * self.hop as f64 * self.tempo).round() as i64
    }

    fn available(&self) -> i64 {
        self.in_start + self.mono.len() as i64
    }

    /// Output length for the input received so far.
    fn expected_len(&self) -> i64 {
        (self.received as f64 / self.tempo).round() as i64
    }

    /// Places the next segment if its input is available (or, when
    /// flushing, after padding the input with silence). Returns the output
    /// position up to which samples are final.
    fn place_segment(&mut self, flushing: bool) -> Option<i64> {
        let m = self.segment;
        let nominal = self.nominal(m);
        let (lo, hi) = if m == 0 {
            (0, 0)
        } else {
            let s = self.search as i64;
            ((nominal - s).max(0), nominal + s)
        };
        let w = self.window as i64;
        let need = (hi + w).max(self.prev_pos + w);
        if need > self.received && !flushing {
            return None;
        }
        if need > self.available() {
            let extra = (need - self.available()) as usize;
            self.mono.resize(self.mono.len() + extra, 0.0);
            self.input
                .resize(self.input.len() + extra * self.channels, 0.0);
        }

        let pos = if m == 0 {
            0
        } else {
            self.best_match(lo, hi, nominal)
        };

        // Overlap-add the windowed segment at output m * hop. The first
        // segment keeps its leading half at full level since nothing
        // overlaps it.
        let ch = self.channels;
        let at = (m as i64 * self.hop as i64 - self.emitted) as usize;
        let end = (at + self.window) * ch;
        if self.out.len() < end {
            self.out.resize(end, 0.0);
        }
        let src = (pos - self.in_start) as usize * ch;
        let segment = &self.input[src..src + self.window * ch];
        for (n, (o, x)) in self.out[at * ch..end]
            .chunks_exact_mut(ch)
            .zip(segment.chunks_exact(ch))
            .enumerate()
        {
            let g = if m == 0 && n < self.hop {
                1.0
            } else {
                self.hann[n]
            };
            for (o, &x) in o.iter_mut().zip(x) {
                *o += g * x;
            }
        }
        self.prev_pos = pos;
        self.segment += 1;

        // Input before both the next continuation target and the next
        // search range is no longer needed.
        let keep_from = (pos + self.hop as i64)
            .min(self.nominal(m + 1) - self.search as i64)
            .clamp(self.in_start, self.available());
        let drop = (keep_from - self.in_start) as usize;
        self.mono.drain(..drop);
        self.input.drain(..drop * ch);
        self.in_start = keep_from;

        Some((m as i64 + 1) * self.hop as i64)
    }

    /// Start in `lo..=hi` whose first half best continues the previous
    /// segment. Ties and silence keep the nominal position.
    fn best_match(&self, lo: i64, hi: i64, nominal: i64) -> i64 {
        let hop = self.hop;
        let base = self.in_start;
        let t0 = (self.prev_pos + hop as i64 - base) as usize;
        let target = &self.mono[t0..t0 + hop];
        let candidate = |c: i64| {
            let start = (c - base) as usize;
            &self.mono[start..start + hop]
        };
        let score = |c: i64| {
            let seg = candidate(c);
            let mut dot = 0.0f64;
            let mut energy = 0.0f64;
            for (&t, &s) in target.iter().zip(seg) {
                dot += t as f64 * s as f64;
                energy += s as f64 * s as f64;
            }
            if energy > 0.0 {
                dot / energy.sqrt()
            } else {
                0.0
            }
        };
        let mut best = nominal.clamp(lo, hi);
        let mut best_score = score(best);
        for c in lo..=hi {
            let s = score(c);
            if s > best_score {
                best = c;
                best_score = s;
            }
        }
        best
    }

    /// Takes the finished output up to `end`, as frames.
    fn take(&mut self, end: i64) -> Vec<Frame> {
        let n = (end - self.emitted).max(0) as usize;
        if n == 0 {
            return Vec::new();
        }
        let ch = self.channels;
        // Placing a segment always leaves the accumulator past the
        // finished point; the clamp only guards against an inconsistent
        // state turning into a panic.
        let have = (self.out.len() / ch).min(n);
        let mut samples: Vec<f32> = self.out.drain(..have * ch).collect();
        samples.resize(n * ch, 0.0);
        let frames = float_frames(&samples, self.rate, ch as u16, self.pts + self.emitted);
        self.emitted = end;
        frames
    }
}

impl Filter for Atempo {
    fn filter(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        let a = expect_audio(frame, "atempo")?;
        if self.tempo == 1.0 {
            return Ok(vec![Frame::Audio(a)]);
        }
        let tempo = self.tempo;
        let state = self.state.get_or_insert_with(|| State::new(tempo, &a));
        if a.sample_rate != state.rate || a.channels as usize != state.channels {
            return Err(Error::unsupported(
                "atempo input changing sample rate or channel count mid-stream",
            ));
        }
        state.push(&a.to_f32());
        let mut out = Vec::new();
        while let Some(done) = state.place_segment(false) {
            let end = done.min(state.expected_len());
            out.extend(state.take(end));
        }
        Ok(out)
    }

    fn flush(&mut self) -> Result<Vec<Frame>> {
        let Some(mut state) = self.state.take() else {
            return Ok(Vec::new());
        };
        let total = state.expected_len();
        let mut out = Vec::new();
        while state.emitted < total {
            let done = state.place_segment(true).unwrap_or(total);
            out.extend(state.take(done.min(total)));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::AudioData;

    fn run(tempo: f64, input: &[f32], rate: u32, channels: u16, chunk: usize) -> Vec<AudioFrame> {
        let mut f = create(&Args::parse(&format!("{tempo}"), &["tempo"]).unwrap()).unwrap();
        let mut out = Vec::new();
        let ch = channels as usize;
        for (i, c) in input.chunks(chunk * ch).enumerate() {
            let frame = AudioFrame::new(
                rate,
                channels,
                AudioData::Float(c.to_vec()),
                (i * chunk) as i64,
            );
            out.extend(f.filter(Frame::Audio(frame)).unwrap());
        }
        out.extend(f.flush().unwrap());
        out.into_iter()
            .map(|f| match f {
                Frame::Audio(a) => a,
                Frame::Video(_) => panic!(),
            })
            .collect()
    }

    fn samples(frames: &[AudioFrame]) -> Vec<f32> {
        frames.iter().flat_map(AudioFrame::to_f32).collect()
    }

    fn sine(freq: f64, rate: u32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (0.5 * (std::f64::consts::TAU * freq * i as f64 / rate as f64).sin()) as f32)
            .collect()
    }

    /// Frequency of the strongest DFT bin between 50 Hz and 2 kHz, in
    /// 5 Hz steps.
    fn peak_frequency(x: &[f32], rate: u32) -> f64 {
        let power = |f: f64| {
            let w = std::f64::consts::TAU * f / rate as f64;
            let (mut re, mut im) = (0.0, 0.0);
            for (i, &v) in x.iter().enumerate() {
                re += v as f64 * (w * i as f64).cos();
                im += v as f64 * (w * i as f64).sin();
            }
            re * re + im * im
        };
        (10..=400)
            .map(|k| k as f64 * 5.0)
            .max_by(|&a, &b| power(a).total_cmp(&power(b)))
            .unwrap()
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    #[test]
    fn length_follows_the_tempo() {
        let input = sine(440.0, 8000, 8000);
        for tempo in [0.5, 0.75, 0.9, 1.25, 1.5, 2.0] {
            let out = run(tempo, &input, 8000, 1, 1000);
            let n: usize = out.iter().map(AudioFrame::nb_samples).sum();
            assert_eq!(n, (8000.0 / tempo).round() as usize, "tempo {tempo}");
            let mut pts = 0;
            for f in &out {
                assert_eq!(f.pts, pts);
                pts += f.nb_samples() as i64;
            }
        }
    }

    #[test]
    fn pitch_and_level_are_kept() {
        let rate = 8000;
        let input = sine(440.0, rate, 16000);
        for tempo in [0.5, 0.8, 1.5, 2.0] {
            let out = samples(&run(tempo, &input, rate, 1, 512));
            let mid = &out[out.len() / 4..3 * out.len() / 4];
            assert_eq!(peak_frequency(mid, rate), 440.0, "tempo {tempo}");
            let level = rms(mid) / (0.5 / 2f64.sqrt());
            assert!((level - 1.0).abs() < 0.05, "tempo {tempo}: level {level}");
        }
    }

    #[test]
    fn stereo_channels_stay_aligned() {
        let left = sine(300.0, 8000, 8000);
        let input: Vec<f32> = left.iter().flat_map(|&v| [v, -v]).collect();
        let out = samples(&run(1.5, &input, 8000, 2, 700));
        for f in out.chunks(2) {
            assert_eq!(f[0], -f[1]);
        }
    }

    #[test]
    fn chunking_does_not_change_the_output() {
        let input = sine(523.0, 8000, 5000);
        let reference = samples(&run(1.3, &input, 8000, 1, 5000));
        for chunk in [1, 100, 999] {
            assert_eq!(samples(&run(1.3, &input, 8000, 1, chunk)), reference);
        }
    }

    #[test]
    fn timestamps_scale_with_the_tempo() {
        let mut f = create(&Args::parse("2", &["tempo"]).unwrap()).unwrap();
        let frame = AudioFrame::new(8000, 1, AudioData::Float(vec![0.1; 4000]), 8000);
        let mut out = f.filter(Frame::Audio(frame)).unwrap();
        out.extend(f.flush().unwrap());
        let Frame::Audio(first) = &out[0] else {
            panic!()
        };
        assert_eq!(first.pts, 4000);
    }

    #[test]
    fn short_and_empty_inputs() {
        assert!(run(1.5, &[], 8000, 1, 10).is_empty());
        let out = run(0.5, &[0.25; 3], 8000, 1, 10);
        assert_eq!(out.iter().map(AudioFrame::nb_samples).sum::<usize>(), 6);
        // A tiny rate still works.
        let out = run(2.0, &[0.25; 9], 10, 1, 4);
        assert_eq!(out.iter().map(AudioFrame::nb_samples).sum::<usize>(), 5);
    }

    #[test]
    fn unity_tempo_passes_through_and_bad_values_fail() {
        let frame = AudioFrame::new(8000, 1, AudioData::Float(vec![0.5; 4]), 3);
        let mut f = create(&Args::parse("1", &["tempo"]).unwrap()).unwrap();
        assert_eq!(
            f.filter(Frame::Audio(frame.clone())).unwrap(),
            vec![Frame::Audio(frame)]
        );
        for bad in ["0.4", "2.5", "0", "-1", "x", "speed=2"] {
            assert!(
                create(&Args::parse(bad, &["tempo"]).unwrap()).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn format_change_is_rejected() {
        let mut f = create(&Args::parse("1.5", &["tempo"]).unwrap()).unwrap();
        let a = AudioFrame::new(8000, 1, AudioData::Float(vec![0.0; 10]), 0);
        let b = AudioFrame::new(16000, 1, AudioData::Float(vec![0.0; 10]), 10);
        f.filter(Frame::Audio(a)).unwrap();
        assert!(f.filter(Frame::Audio(b)).is_err());
    }
}
