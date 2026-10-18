//! Seeded pseudorandom numbers for the generators.
//!
//! The state is expanded from the user's 64-bit seed with SplitMix64
//! (Steele, Lea and Flood, 2014) and advanced with xoshiro256** (Blackman
//! and Vigna, 2018). Both are tiny, fast, pass the usual statistical test
//! batteries, and are fully specified by their papers, so a given seed
//! yields the same sequence on every platform and every release.

/// SplitMix64: a 64-bit counter run through a mixing function. Used only
/// to turn one seed into a well spread xoshiro state, since xoshiro must
/// not start from an all-zero state and nearby seeds must not give
/// correlated streams.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

/// xoshiro256**: 256 bits of state, period 2^256 - 1.
#[derive(Clone, Debug)]
pub(crate) struct Rng {
    s: [u64; 4],
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        let mut sm = SplitMix64(seed);
        // SplitMix64 is a bijection of its counter, so four consecutive
        // outputs are never all zero.
        let s = [sm.next(), sm.next(), sm.next(), sm.next()];
        Rng { s }
    }

    pub fn next_u64(&mut self) -> u64 {
        let s = &mut self.s;
        let result = s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        result
    }

    /// Uniform in [0, 1), from the top 53 bits so every value is an exact
    /// multiple of 2^-53.
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// Uniform in [-1, 1).
    pub fn next_signed(&mut self) -> f64 {
        self.next_f64() * 2.0 - 1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitmix_matches_the_published_sequence() {
        // First outputs for seed 0, as listed with the reference description.
        let mut sm = SplitMix64(0);
        assert_eq!(sm.next(), 0xe220_a839_7b1d_cdaf);
        assert_eq!(sm.next(), 0x6e78_9e6a_a1b9_65f4);
        assert_eq!(sm.next(), 0x06c4_5d18_8009_454f);
    }

    #[test]
    fn same_seed_same_sequence() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        let mut c = Rng::new(43);
        let va: Vec<u64> = (0..100).map(|_| a.next_u64()).collect();
        let vb: Vec<u64> = (0..100).map(|_| b.next_u64()).collect();
        let vc: Vec<u64> = (0..100).map(|_| c.next_u64()).collect();
        assert_eq!(va, vb);
        assert_ne!(va, vc);
    }

    #[test]
    fn floats_are_in_range_and_spread() {
        let mut r = Rng::new(7);
        let mut sum = 0.0;
        let n = 100_000;
        for _ in 0..n {
            let v = r.next_f64();
            assert!((0.0..1.0).contains(&v));
            let s = r.next_signed();
            assert!((-1.0..1.0).contains(&s));
            sum += v;
        }
        let mean = sum / n as f64;
        assert!((mean - 0.5).abs() < 0.01, "mean {mean}");
    }
}
