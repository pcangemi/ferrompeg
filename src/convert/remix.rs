//! Channel count conversion (downmix and upmix).
//!
//! Channels are in WAV order, the order of the speaker position bits of
//! WAVE_FORMAT_EXTENSIBLE (Microsoft, "Multiple Channel Audio Data and WAVE
//! Files"). Each count maps to one layout, the same ones
//! [`channel_layout_name`](crate::frame::channel_layout_name) reports:
//!
//! | count | layout | channels                      |
//! |-------|--------|-------------------------------|
//! | 1     | mono   | FC                            |
//! | 2     | stereo | FL FR                         |
//! | 3     | 2.1    | FL FR LFE                     |
//! | 4     | quad   | FL FR BL BR                   |
//! | 5     | 5.0    | FL FR FC BL BR                |
//! | 6     | 5.1    | FL FR FC LFE BL BR            |
//! | 7     | 6.1    | FL FR FC LFE BC SL SR         |
//! | 8     | 7.1    | FL FR FC LFE BL BR SL SR      |
//!
//! The matrix routes every input channel to the output channel at the same
//! position. A channel the output lacks is folded into its neighbours, in
//! the spirit of the ITU-R BS.775 downmix equations:
//!
//! * the centre goes to FL and FR at -3 dB;
//! * a back channel goes to the back centre at -3 dB, else to the side
//!   channel of its side, else to the front channel of its side at -3 dB;
//! * a side channel goes to the back channel of its side, else to the
//!   front channel of its side at -3 dB;
//! * the back centre is split over the back pair, else the side pair,
//!   else the front pair, at -3 dB each;
//! * the LFE is dropped: BS.775 treats it as optional and leaves it out
//!   of its downmix equations;
//! * a mono output sums all the other channels, with the surrounds at
//!   -3 dB, and then averages them (see below).
//!
//! Each output row is then scaled so its coefficients add up to at most 1
//! in absolute value: a full scale input can never clip. For 5.1 to stereo
//! that gives `L = 0.414 FL + 0.293 FC + 0.293 BL`, the BS.775 weights
//! (1, 0.707, 0.707) scaled by 1 / 2.414; stereo to mono becomes the
//! average of the two sides.
//!
//! Mono input is the one exception to position matching: it is copied at
//! full level into FL and FR, not into FC. Two speakers playing the same
//! signal produce a centred phantom image, which is how a mono source is
//! usually expected to sound on a stereo or surround system, and a mono to
//! stereo to mono round trip gives back the original samples exactly.
//! Every other upmix leaves the new channels silent.

use crate::error::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pos {
    FL,
    FR,
    FC,
    Lfe,
    BL,
    BR,
    BC,
    SL,
    SR,
}

use Pos::*;

/// -3 dB, the usual gain for folding one channel into two.
const MINUS_3DB: f32 = std::f32::consts::FRAC_1_SQRT_2;

fn layout(channels: u16) -> Option<&'static [Pos]> {
    Some(match channels {
        1 => &[FC],
        2 => &[FL, FR],
        3 => &[FL, FR, Lfe],
        4 => &[FL, FR, BL, BR],
        5 => &[FL, FR, FC, BL, BR],
        6 => &[FL, FR, FC, Lfe, BL, BR],
        7 => &[FL, FR, FC, Lfe, BC, SL, SR],
        8 => &[FL, FR, FC, Lfe, BL, BR, SL, SR],
        _ => return None,
    })
}

/// Converts interleaved float samples from `from` channels to `to`.
pub fn remix(samples: &[f32], from: u16, to: u16) -> Result<Vec<f32>> {
    let m = matrix(from, to)?;
    let (from, to) = (from as usize, to as usize);
    if samples.len() % from != 0 {
        return Err(Error::invalid_data(format!(
            "{} samples do not divide into frames of {from} channels",
            samples.len()
        )));
    }
    if from == to {
        return Ok(samples.to_vec());
    }
    let mut out = Vec::with_capacity(samples.len() / from * to);
    for frame in samples.chunks_exact(from) {
        for row in m.chunks_exact(from) {
            out.push(row.iter().zip(frame).map(|(c, x)| c * x).sum());
        }
    }
    Ok(out)
}

/// Mixing matrix, `to` rows of `from` coefficients.
fn matrix(from: u16, to: u16) -> Result<Vec<f32>> {
    let unsupported = |n: u16| {
        Error::unsupported(format!(
            "remixing {n} channels (only 1 to 8 channel layouts are known)"
        ))
    };
    let src = layout(from).ok_or_else(|| unsupported(from))?;
    let dst = layout(to).ok_or_else(|| unsupported(to))?;
    let (nf, nt) = (src.len(), dst.len());
    let mut m = vec![0.0f32; nf * nt];
    let index = |p: Pos| dst.iter().position(|&d| d == p);
    let mut route = |input: usize, targets: &[(Pos, f32)]| {
        for &(p, gain) in targets {
            if let Some(o) = index(p) {
                m[o * nf + input] += gain;
            }
        }
    };

    for (i, &p) in src.iter().enumerate() {
        if nf == 1 {
            if nt == 1 {
                route(i, &[(FC, 1.0)]);
            } else {
                route(i, &[(FL, 1.0), (FR, 1.0)]);
            }
            continue;
        }
        if index(p).is_some() {
            route(i, &[(p, 1.0)]);
            continue;
        }
        // Fallbacks in order of preference; the first one whose speakers
        // all exist in the output is used.
        let options: &[&[(Pos, f32)]] = match p {
            // Only a mono output lacks the front pair.
            FL | FR => &[&[(FC, 1.0)]],
            FC => &[&[(FL, MINUS_3DB), (FR, MINUS_3DB)]],
            Lfe => &[],
            // A back channel stays at the back if it can.
            BL => &[
                &[(BC, MINUS_3DB)],
                &[(SL, 1.0)],
                &[(FL, MINUS_3DB)],
                &[(FC, MINUS_3DB)],
            ],
            BR => &[
                &[(BC, MINUS_3DB)],
                &[(SR, 1.0)],
                &[(FR, MINUS_3DB)],
                &[(FC, MINUS_3DB)],
            ],
            SL => &[&[(BL, 1.0)], &[(FL, MINUS_3DB)], &[(FC, MINUS_3DB)]],
            SR => &[&[(BR, 1.0)], &[(FR, MINUS_3DB)], &[(FC, MINUS_3DB)]],
            BC => &[
                &[(BL, MINUS_3DB), (BR, MINUS_3DB)],
                &[(SL, MINUS_3DB), (SR, MINUS_3DB)],
                &[(FL, MINUS_3DB), (FR, MINUS_3DB)],
                &[(FC, MINUS_3DB)],
            ],
        };
        let targets = options
            .iter()
            .find(|option| option.iter().all(|&(q, _)| index(q).is_some()))
            .copied()
            .unwrap_or(&[]);
        route(i, targets);
    }

    // Scale rows whose gains add up past 1 so the sum of full scale
    // inputs stays within full scale.
    for row in m.chunks_exact_mut(nf) {
        let sum: f32 = row.iter().map(|c| c.abs()).sum();
        if sum > 1.0 {
            row.iter_mut().for_each(|c| *c /= sum);
        }
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: &[f32], b: &[f32]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-6)
    }

    /// Output of a single frame where only input channel `ch` is 1.
    fn column(from: u16, to: u16, ch: usize) -> Vec<f32> {
        let mut frame = vec![0.0; from as usize];
        frame[ch] = 1.0;
        remix(&frame, from, to).unwrap()
    }

    #[test]
    fn identity_keeps_samples() {
        for n in 1..=8 {
            let s: Vec<f32> = (0..n * 3).map(|i| i as f32).collect();
            assert_eq!(remix(&s, n, n).unwrap(), s);
        }
    }

    #[test]
    fn stereo_to_mono_averages() {
        assert!(close(
            &remix(&[1.0, 0.0, 0.5, 0.5, -1.0, 1.0], 2, 1).unwrap(),
            &[0.5, 0.5, 0.0]
        ));
    }

    #[test]
    fn mono_upmix_goes_to_the_front_pair() {
        assert_eq!(
            remix(&[0.5, -0.25], 1, 2).unwrap(),
            vec![0.5, 0.5, -0.25, -0.25]
        );
        // 5.1: FL and FR carry the signal, FC, LFE and surrounds stay silent.
        assert_eq!(column(1, 6, 0), vec![1.0, 1.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(column(1, 3, 0), vec![1.0, 1.0, 0.0]);
        assert_eq!(column(1, 7, 0), vec![1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn mono_stereo_round_trip_is_exact() {
        let mono = [0.5, -0.25, 1.0, -1.0, 0.123];
        let stereo = remix(&mono, 1, 2).unwrap();
        assert_eq!(remix(&stereo, 2, 1).unwrap(), mono);
    }

    #[test]
    fn surround_51_to_stereo_follows_bs775() {
        let total = 1.0 + 2.0 * MINUS_3DB;
        let (direct, folded) = (1.0 / total, MINUS_3DB / total);
        assert!(close(&column(6, 2, 0), &[direct, 0.0]));
        assert!(close(&column(6, 2, 1), &[0.0, direct]));
        assert!(close(&column(6, 2, 2), &[folded, folded]));
        assert!(close(&column(6, 2, 3), &[0.0, 0.0]));
        assert!(close(&column(6, 2, 4), &[folded, 0.0]));
        assert!(close(&column(6, 2, 5), &[0.0, folded]));
        assert!((direct - 0.414_213_57).abs() < 1e-6);
        assert!((folded - 0.292_893_2).abs() < 1e-6);
    }

    #[test]
    fn surround_71_to_stereo() {
        // FL, FC at -3 dB, BL at -3 dB, SL at -3 dB on the left side.
        let total = 1.0 + 3.0 * MINUS_3DB;
        let (direct, folded) = (1.0 / total, MINUS_3DB / total);
        assert!(close(&column(8, 2, 0), &[direct, 0.0]));
        assert!(close(&column(8, 2, 2), &[folded, folded]));
        assert!(close(&column(8, 2, 3), &[0.0, 0.0]));
        assert!(close(&column(8, 2, 4), &[folded, 0.0]));
        assert!(close(&column(8, 2, 7), &[0.0, folded]));
    }

    #[test]
    fn surround_71_to_51_merges_side_and_back() {
        // BL row = (BL + SL) / 2, front channels untouched.
        assert!(close(&column(8, 6, 0), &[1.0, 0.0, 0.0, 0.0, 0.0, 0.0]));
        assert!(close(&column(8, 6, 3), &[0.0, 0.0, 0.0, 1.0, 0.0, 0.0]));
        assert!(close(&column(8, 6, 4), &[0.0, 0.0, 0.0, 0.0, 0.5, 0.0]));
        assert!(close(&column(8, 6, 6), &[0.0, 0.0, 0.0, 0.0, 0.5, 0.0]));
        assert!(close(&column(8, 6, 7), &[0.0, 0.0, 0.0, 0.0, 0.0, 0.5]));
    }

    #[test]
    fn surround_51_to_mono_drops_lfe() {
        let total = 3.0 + 2.0 * MINUS_3DB;
        assert!(close(&column(6, 1, 0), &[1.0 / total]));
        assert!(close(&column(6, 1, 2), &[1.0 / total]));
        assert!(close(&column(6, 1, 3), &[0.0]));
        assert!(close(&column(6, 1, 4), &[MINUS_3DB / total]));
    }

    #[test]
    fn quad_and_50_to_stereo() {
        let total = 1.0 + MINUS_3DB;
        assert!(close(&column(4, 2, 0), &[1.0 / total, 0.0]));
        assert!(close(&column(4, 2, 3), &[0.0, MINUS_3DB / total]));
        let total = 1.0 + 2.0 * MINUS_3DB;
        assert!(close(
            &column(5, 2, 2),
            &[MINUS_3DB / total, MINUS_3DB / total]
        ));
    }

    #[test]
    fn surround_61_to_51_splits_back_centre() {
        // BC goes to BL and BR at -3 dB, SL and SR take the back pair.
        let back = column(7, 6, 4);
        let total = 1.0 + MINUS_3DB;
        assert!(close(
            &back,
            &[0.0, 0.0, 0.0, 0.0, MINUS_3DB / total, MINUS_3DB / total]
        ));
        assert!(close(
            &column(7, 6, 5),
            &[0.0, 0.0, 0.0, 0.0, 1.0 / total, 0.0]
        ));
        // 7.1 to 6.1: the back pair folds into BC, sides match.
        assert!(close(
            &column(8, 7, 4),
            &[0.0, 0.0, 0.0, 0.0, 0.5, 0.0, 0.0]
        ));
        assert!(close(
            &column(8, 7, 6),
            &[0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0]
        ));
    }

    #[test]
    fn upmixes_route_matching_channels_and_zero_the_rest() {
        assert_eq!(column(2, 6, 0), vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(column(2, 6, 1), vec![0.0, 1.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(column(3, 6, 2), vec![0.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
        assert_eq!(column(4, 6, 2), vec![0.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
        assert_eq!(
            column(6, 8, 5),
            vec![0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0]
        );
        assert_eq!(column(5, 6, 2), vec![0.0, 0.0, 1.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn full_scale_never_clips() {
        for from in 1..=8u16 {
            for to in 1..=8u16 {
                for sign in [1.0f32, -1.0] {
                    let frame = vec![sign; from as usize];
                    for v in remix(&frame, from, to).unwrap() {
                        assert!(v.abs() <= 1.0 + 1e-6, "{from} -> {to}: {v}");
                    }
                }
            }
        }
    }

    #[test]
    fn rejects_bad_input() {
        assert!(remix(&[0.0; 3], 2, 1).is_err());
        assert!(remix(&[0.0; 9], 9, 2).is_err());
        assert!(remix(&[0.0; 2], 2, 0).is_err());
        assert!(remix(&[], 0, 2).is_err());
    }
}
