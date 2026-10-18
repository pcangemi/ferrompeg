//! Time values as written on the command line and shown in logs.

use crate::error::{Error, Result};

/// Parses a duration or position in seconds.
///
/// Accepted forms follow the usual command line syntax:
/// `[-][HH:]MM:SS[.m...]` or `[-]S+[.m...][s|ms|us]`.
pub fn parse_duration(input: &str) -> Result<f64> {
    let s = input.trim();
    let bad = || Error::invalid_argument(format!("invalid time value '{input}'"));
    let (negative, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    if body.is_empty() {
        return Err(bad());
    }
    let seconds = if body.contains(':') {
        let parts: Vec<&str> = body.split(':').collect();
        if parts.len() > 3 {
            return Err(bad());
        }
        let mut total = 0.0;
        let last = parts.len() - 1;
        for (i, part) in parts.iter().enumerate() {
            let value: f64 = if i == last {
                parse_decimal(part).ok_or_else(bad)?
            } else {
                part.parse::<u64>().map_err(|_| bad())? as f64
            };
            if i > 0 && value >= 60.0 {
                return Err(bad());
            }
            total = total * 60.0 + value;
        }
        total
    } else if let Some(v) = body.strip_suffix("ms") {
        parse_decimal(v).ok_or_else(bad)? / 1e3
    } else if let Some(v) = body.strip_suffix("us") {
        parse_decimal(v).ok_or_else(bad)? / 1e6
    } else if let Some(v) = body.strip_suffix('s') {
        parse_decimal(v).ok_or_else(bad)?
    } else {
        parse_decimal(body).ok_or_else(bad)?
    };
    Ok(if negative { -seconds } else { seconds })
}

/// Digits with an optional fractional part. Rejects exponents, signs,
/// "inf" and "nan", which `f64::from_str` would accept.
fn parse_decimal(s: &str) -> Option<f64> {
    let valid = !s.is_empty()
        && s.chars().all(|c| c.is_ascii_digit() || c == '.')
        && s.matches('.').count() <= 1
        && s != ".";
    if valid { s.parse().ok() } else { None }
}

/// Formats seconds as `HH:MM:SS.cc`, the form used in progress lines.
pub fn format_duration(seconds: f64) -> String {
    let negative = seconds < 0.0;
    let centis = (seconds.abs() * 100.0).round() as u64;
    let (h, rest) = (centis / 360_000, centis % 360_000);
    let (m, rest) = (rest / 6000, rest % 6000);
    let (s, c) = (rest / 100, rest % 100);
    format!(
        "{}{h:02}:{m:02}:{s:02}.{c:02}",
        if negative { "-" } else { "" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_forms() {
        assert_eq!(parse_duration("5").unwrap(), 5.0);
        assert_eq!(parse_duration("1.5").unwrap(), 1.5);
        assert_eq!(parse_duration("250ms").unwrap(), 0.25);
        assert_eq!(parse_duration("2s").unwrap(), 2.0);
        assert_eq!(parse_duration("1500us").unwrap(), 0.0015);
        assert_eq!(parse_duration("01:02").unwrap(), 62.0);
        assert_eq!(parse_duration("01:00:02.5").unwrap(), 3602.5);
        assert_eq!(parse_duration("-3").unwrap(), -3.0);
    }

    #[test]
    fn rejects_garbage() {
        for s in ["", "-", "abc", "1:60", "1e3", "inf", "1:2:3:4", "."] {
            assert!(parse_duration(s).is_err(), "{s}");
        }
    }

    #[test]
    fn formats_like_progress_lines() {
        assert_eq!(format_duration(5.0), "00:00:05.00");
        assert_eq!(format_duration(3725.456), "01:02:05.46");
        assert_eq!(format_duration(-1.5), "-00:00:01.50");
    }
}
