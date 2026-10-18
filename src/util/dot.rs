//! Dot product of float slices, written so the compiler turns it into
//! vector instructions.

/// Independent partial sums. Eight lanes fill two 128-bit registers and
/// hide the latency of the additions.
const LANES: usize = 8;

/// Sum of `a[i] * b[i]` over the shorter of the two slices. The summation
/// order is fixed, so the result is the same on every run.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let (a, b) = (&a[..n], &b[..n]);
    let mut acc = [0.0f32; LANES];
    let mut ca = a.chunks_exact(LANES);
    let mut cb = b.chunks_exact(LANES);
    for (x, y) in (&mut ca).zip(&mut cb) {
        for i in 0..LANES {
            acc[i] += x[i] * y[i];
        }
    }
    let mut tail = 0.0;
    for (x, y) in ca.remainder().iter().zip(cb.remainder()) {
        tail += x * y;
    }
    let pairs = [
        acc[0] + acc[4],
        acc[1] + acc[5],
        acc[2] + acc[6],
        acc[3] + acc[7],
    ];
    (pairs[0] + pairs[2]) + (pairs[1] + pairs[3]) + tail
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_a_plain_loop() {
        for n in [0, 1, 7, 8, 9, 31, 144, 661] {
            let a: Vec<f32> = (0..n)
                .map(|i| ((i * 37 % 101) as f32 - 50.0) / 50.0)
                .collect();
            let b: Vec<f32> = (0..n)
                .map(|i| ((i * 53 % 89) as f32 - 44.0) / 44.0)
                .collect();
            let plain: f64 = a.iter().zip(&b).map(|(&x, &y)| x as f64 * y as f64).sum();
            assert!((dot(&a, &b) as f64 - plain).abs() < 1e-4, "n = {n}");
        }
    }

    #[test]
    fn uses_the_shorter_slice() {
        assert_eq!(dot(&[1.0, 2.0, 3.0], &[1.0, 1.0]), 3.0);
    }
}
