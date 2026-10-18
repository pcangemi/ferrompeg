use std::fmt;

use crate::error::{Error, Result};

/// An exact fraction, used for time bases and frame rates.
///
/// Timestamps are integers counted in a time base (for example 1/44100 for
/// audio), so durations never accumulate floating point drift.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rational {
    pub num: i64,
    pub den: i64,
}

impl Rational {
    /// Builds a reduced fraction with a positive denominator.
    ///
    /// # Panics
    ///
    /// Panics if `den` is zero.
    pub fn new(num: i64, den: i64) -> Self {
        assert!(den != 0, "rational with a zero denominator");
        let g = gcd(num.unsigned_abs(), den.unsigned_abs()).max(1) as i64;
        let sign = if den < 0 { -1 } else { 1 };
        Rational {
            num: sign * num / g,
            den: sign * den / g,
        }
    }

    pub fn to_f64(self) -> f64 {
        self.num as f64 / self.den as f64
    }

    /// Returns the inverse fraction, so a frame rate becomes a frame duration.
    ///
    /// # Panics
    ///
    /// Panics if the fraction is zero.
    pub fn inv(self) -> Self {
        Rational::new(self.den, self.num)
    }

    /// Converts `value` counted in `from` units into `to` units, rounding to
    /// the nearest integer (halves away from zero).
    pub fn rescale(value: i64, from: Rational, to: Rational) -> i64 {
        // value * from / to = value * from.num * to.den / (from.den * to.num).
        // i128 keeps the intermediate product exact for any realistic input.
        let num = value as i128 * from.num as i128 * to.den as i128;
        let den = from.den as i128 * to.num as i128;
        div_round(num, den) as i64
    }

    /// Approximates a float with a fraction whose denominator fits common
    /// frame rates, so "29.97" becomes 2997/100 and "23.976" 2997/125.
    pub fn from_f64(value: f64) -> Option<Self> {
        if !value.is_finite() {
            return None;
        }
        let den = 1_000_000i64;
        Some(Rational::new((value * den as f64).round() as i64, den))
    }

    /// Parses "25", "30000/1001", "30000:1001" or "29.97".
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        let bad = || Error::invalid_argument(format!("cannot parse '{s}' as a rational number"));
        if let Some((n, d)) = s.split_once(['/', ':']) {
            let num: i64 = n.trim().parse().map_err(|_| bad())?;
            let den: i64 = d.trim().parse().map_err(|_| bad())?;
            if den == 0 {
                return Err(bad());
            }
            return Ok(Rational::new(num, den));
        }
        if let Ok(n) = s.parse::<i64>() {
            return Ok(Rational::new(n, 1));
        }
        let f: f64 = s.parse().map_err(|_| bad())?;
        Rational::from_f64(f).ok_or_else(bad)
    }
}

impl fmt::Display for Rational {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.num, self.den)
    }
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

fn div_round(num: i128, den: i128) -> i128 {
    let (num, den) = if den < 0 { (-num, -den) } else { (num, den) };
    if num >= 0 {
        (num + den / 2) / den
    } else {
        (num - den / 2) / den
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduces_and_normalizes_sign() {
        assert_eq!(Rational::new(50, 100), Rational::new(1, 2));
        assert_eq!(Rational::new(1, -2), Rational { num: -1, den: 2 });
        assert_eq!(Rational::new(0, 7), Rational { num: 0, den: 1 });
    }

    #[test]
    fn rescales_with_rounding() {
        let audio = Rational::new(1, 44100);
        let ms = Rational::new(1, 1000);
        assert_eq!(Rational::rescale(44100, audio, ms), 1000);
        assert_eq!(Rational::rescale(22, audio, ms), 0);
        assert_eq!(Rational::rescale(23, audio, ms), 1);
        assert_eq!(Rational::rescale(-23, audio, ms), -1);
    }

    #[test]
    fn parses_common_spellings() {
        assert_eq!(Rational::parse("25").unwrap(), Rational::new(25, 1));
        assert_eq!(
            Rational::parse("30000/1001").unwrap(),
            Rational::new(30000, 1001)
        );
        assert_eq!(Rational::parse("24:1").unwrap(), Rational::new(24, 1));
        assert_eq!(Rational::parse("29.97").unwrap(), Rational::new(2997, 100));
        assert!(Rational::parse("1/0").is_err());
        assert!(Rational::parse("abc").is_err());
    }
}
