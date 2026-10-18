//! Sample kind conversion: integer bit depths and float.

use crate::frame::{AudioData, SampleKind};

/// Converts samples to `kind`.
///
/// Integer to integer conversion shifts, which is exact when widening.
/// Float to integer rounds to nearest and clips, so out of range floats
/// saturate instead of wrapping around into loud clicks.
pub fn convert_kind(data: &AudioData, kind: SampleKind) -> AudioData {
    match (data, kind) {
        (d, k) if d.kind() == k => d.clone(),
        (AudioData::Int { samples, bits }, SampleKind::Int(to)) => {
            let samples = if to >= *bits {
                let shift = to - bits;
                samples.iter().map(|&s| s << shift).collect()
            } else {
                // Narrowing: round to nearest by adding half of the dropped
                // range before shifting, then clamp the one value that can
                // overflow (the largest positive input).
                let shift = bits - to;
                let half = 1i64 << (shift - 1);
                let max = (1i64 << (to - 1)) - 1;
                samples
                    .iter()
                    .map(|&s| ((s as i64 + half) >> shift).min(max) as i32)
                    .collect()
            };
            AudioData::Int { samples, bits: to }
        }
        (AudioData::Int { samples, bits }, SampleKind::Float) => {
            let scale = 1.0 / (1u64 << (bits - 1)) as f64;
            AudioData::Float(samples.iter().map(|&s| (s as f64 * scale) as f32).collect())
        }
        (AudioData::Float(samples), SampleKind::Int(to)) => {
            let full = (1u64 << (to - 1)) as f64;
            let (min, max) = (-full, full - 1.0);
            AudioData::Int {
                samples: samples
                    .iter()
                    .map(|&s| (s as f64 * full).round().clamp(min, max) as i32)
                    .collect(),
                bits: to,
            }
        }
        (AudioData::Float(_), SampleKind::Float) => unreachable!("handled by the first arm"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widening_is_exact_and_reversible() {
        let d = AudioData::Int {
            samples: vec![-32768, -1, 0, 1, 32767],
            bits: 16,
        };
        let wide = convert_kind(&d, SampleKind::Int(24));
        assert_eq!(
            wide,
            AudioData::Int {
                samples: vec![-8388608, -256, 0, 256, 8388352],
                bits: 24
            }
        );
        assert_eq!(convert_kind(&wide, SampleKind::Int(16)), d);
    }

    #[test]
    fn narrowing_rounds_and_saturates() {
        let d = AudioData::Int {
            samples: vec![8388607, 128, 127, -129],
            bits: 24,
        };
        let AudioData::Int { samples, .. } = convert_kind(&d, SampleKind::Int(16)) else {
            panic!()
        };
        assert_eq!(samples, vec![32767, 1, 0, -1]);
    }

    #[test]
    fn float_clips() {
        let d = AudioData::Float(vec![-2.0, -1.0, 0.0, 0.5, 1.0, 3.0]);
        let AudioData::Int { samples, .. } = convert_kind(&d, SampleKind::Int(16)) else {
            panic!()
        };
        assert_eq!(samples, vec![-32768, -32768, 0, 16384, 32767, 32767]);
    }

    #[test]
    fn int_to_float_and_back() {
        let d = AudioData::Int {
            samples: vec![-8388608, -5, 0, 77, 8388607],
            bits: 24,
        };
        let f = convert_kind(&d, SampleKind::Float);
        assert_eq!(convert_kind(&f, SampleKind::Int(24)), d);
    }
}
