//! Option strings of filters and sources: `320:240`, `w=320:h=240`,
//! `frequency=440:duration=5`.
//!
//! Values are separated by `:`. A value may be quoted with single quotes
//! or use a backslash to include a literal `:` or `,`.

use crate::error::{Error, Result};
use crate::rational::Rational;
use crate::util::time::parse_duration;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Args {
    /// Key and raw value of every option, positional ones already named.
    pairs: Vec<(String, String)>,
}

impl Args {
    /// Parses `spec`, giving unnamed values the names in `positional`, in
    /// order.
    pub fn parse(spec: &str, positional: &[&str]) -> Result<Self> {
        let mut pairs = Vec::new();
        let mut next_positional = 0;
        for token in split_unescaped(spec, ':')? {
            if token.is_empty() {
                continue;
            }
            match token.split_once('=') {
                Some((k, v)) => pairs.push((k.trim().to_string(), v.to_string())),
                None => {
                    let name = positional.get(next_positional).ok_or_else(|| {
                        Error::invalid_argument(format!("too many unnamed values in '{spec}'"))
                    })?;
                    next_positional += 1;
                    pairs.push((name.to_string(), token));
                }
            }
        }
        Ok(Args { pairs })
    }

    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    pub fn pairs(&self) -> &[(String, String)] {
        &self.pairs
    }

    /// Fails on any option whose key is not listed, so typos are reported
    /// instead of silently ignored.
    pub fn check_known(&self, what: &str, known: &[&str]) -> Result<()> {
        match self
            .pairs
            .iter()
            .find(|(k, _)| !known.contains(&k.as_str()))
        {
            Some((k, _)) => Err(Error::invalid_argument(format!(
                "{what} has no option '{k}' (known: {})",
                known.join(", ")
            ))),
            None => Ok(()),
        }
    }

    /// Raw value of the last option named by any of `keys` (aliases such
    /// as `frequency` and `f`).
    pub fn get(&self, keys: &[&str]) -> Option<&str> {
        self.pairs
            .iter()
            .rev()
            .find(|(k, _)| keys.contains(&k.as_str()))
            .map(|(_, v)| v.as_str())
    }

    fn typed<T>(&self, keys: &[&str], parse: impl Fn(&str) -> Option<T>) -> Result<Option<T>> {
        match self.get(keys) {
            None => Ok(None),
            Some(v) => parse(v).map(Some).ok_or_else(|| {
                Error::invalid_argument(format!("invalid value '{v}' for '{}'", keys[0]))
            }),
        }
    }

    pub fn f64(&self, keys: &[&str]) -> Result<Option<f64>> {
        self.typed(keys, |v| {
            v.trim().parse::<f64>().ok().filter(|f| f.is_finite())
        })
    }

    pub fn i64(&self, keys: &[&str]) -> Result<Option<i64>> {
        self.typed(keys, |v| v.trim().parse().ok())
    }

    pub fn u32(&self, keys: &[&str]) -> Result<Option<u32>> {
        self.typed(keys, |v| v.trim().parse().ok())
    }

    pub fn bool(&self, keys: &[&str]) -> Result<Option<bool>> {
        self.typed(keys, |v| match v.trim() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        })
    }

    /// A duration in seconds, in any form [`parse_duration`] accepts.
    pub fn duration(&self, keys: &[&str]) -> Result<Option<f64>> {
        self.typed(keys, |v| parse_duration(v).ok())
    }

    /// A picture size such as `320x240` or `hd720`.
    pub fn size(&self, keys: &[&str]) -> Result<Option<(u32, u32)>> {
        self.typed(keys, |v| parse_size(v).ok())
    }

    pub fn rational(&self, keys: &[&str]) -> Result<Option<Rational>> {
        self.typed(keys, |v| parse_frame_rate(v).ok())
    }
}

/// Splits on `sep`, honoring single quotes and backslash escapes, and
/// removes the quoting characters.
pub fn split_unescaped(s: &str, sep: char) -> Result<Vec<String>> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(next) => current.push(next),
                None => {
                    return Err(Error::invalid_argument(format!(
                        "trailing backslash in '{s}'"
                    )));
                }
            },
            '\'' => quoted = !quoted,
            c if c == sep && !quoted => parts.push(std::mem::take(&mut current)),
            c => current.push(c),
        }
    }
    if quoted {
        return Err(Error::invalid_argument(format!(
            "unterminated quote in '{s}'"
        )));
    }
    parts.push(current);
    Ok(parts)
}

/// Splits on `sep` outside quotes, keeping escapes and quotes intact so the
/// pieces can be parsed again (used to split a chain into filters before
/// each filter parses its own options).
pub fn split_raw(s: &str, sep: char) -> Result<Vec<String>> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                current.push(c);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            '\'' => {
                quoted = !quoted;
                current.push(c);
            }
            c if c == sep && !quoted => parts.push(std::mem::take(&mut current)),
            c => current.push(c),
        }
    }
    if quoted {
        return Err(Error::invalid_argument(format!(
            "unterminated quote in '{s}'"
        )));
    }
    parts.push(current);
    Ok(parts)
}

/// Parses `WxH` or one of the usual size abbreviations.
pub fn parse_size(s: &str) -> Result<(u32, u32)> {
    let named = match s {
        "sqcif" => Some((128, 96)),
        "qcif" => Some((176, 144)),
        "cif" => Some((352, 288)),
        "qvga" => Some((320, 240)),
        "vga" => Some((640, 480)),
        "svga" => Some((800, 600)),
        "xga" => Some((1024, 768)),
        "hd480" => Some((852, 480)),
        "hd720" => Some((1280, 720)),
        "hd1080" => Some((1920, 1080)),
        "uhd2160" | "4k" => Some((3840, 2160)),
        _ => None,
    };
    if let Some(size) = named {
        return Ok(size);
    }
    let bad = || Error::invalid_argument(format!("invalid size '{s}' (expected WxH)"));
    let (w, h) = s.split_once(['x', 'X']).ok_or_else(bad)?;
    let w: u32 = w.trim().parse().map_err(|_| bad())?;
    let h: u32 = h.trim().parse().map_err(|_| bad())?;
    if w == 0 || h == 0 || w > 16384 || h > 16384 {
        return Err(bad());
    }
    Ok((w, h))
}

/// Parses a frame rate: `25`, `30000/1001`, `29.97`, or `ntsc`, `pal`,
/// `film`.
pub fn parse_frame_rate(s: &str) -> Result<Rational> {
    let rate = match s {
        "ntsc" => Rational::new(30000, 1001),
        "pal" => Rational::new(25, 1),
        "film" => Rational::new(24, 1),
        "ntsc-film" => Rational::new(24000, 1001),
        _ => Rational::parse(s)?,
    };
    if rate.num <= 0 {
        return Err(Error::invalid_argument(format!(
            "frame rate must be positive: '{s}'"
        )));
    }
    Ok(rate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positional_and_named() {
        let a = Args::parse("320:h=240:flags=bicubic", &["w", "h"]).unwrap();
        assert_eq!(a.u32(&["w", "width"]).unwrap(), Some(320));
        assert_eq!(a.u32(&["h"]).unwrap(), Some(240));
        assert_eq!(a.get(&["flags"]), Some("bicubic"));
        assert!(a.check_known("scale", &["w", "h"]).is_err());
        assert!(Args::parse("1:2:3", &["a"]).is_err());
    }

    #[test]
    fn quoting_and_escapes() {
        let a = Args::parse(r"text='a:b':other=c\:d", &[]).unwrap();
        assert_eq!(a.get(&["text"]), Some("a:b"));
        assert_eq!(a.get(&["other"]), Some("c:d"));
        assert!(Args::parse("x='open", &[]).is_err());
        assert_eq!(
            split_raw(r"scale=1:2,drawtext=text='a,b',hflip", ',').unwrap(),
            vec!["scale=1:2", "drawtext=text='a,b'", "hflip"]
        );
    }

    #[test]
    fn typed_values() {
        let a = Args::parse("d=1.5:s=hd720:r=ntsc:on=yes", &[]).unwrap();
        assert_eq!(a.duration(&["d"]).unwrap(), Some(1.5));
        assert_eq!(a.size(&["s"]).unwrap(), Some((1280, 720)));
        assert_eq!(
            a.rational(&["r"]).unwrap(),
            Some(Rational::new(30000, 1001))
        );
        assert_eq!(a.bool(&["on"]).unwrap(), Some(true));
        assert_eq!(a.f64(&["missing"]).unwrap(), None);
        assert!(Args::parse("d=abc", &[]).unwrap().duration(&["d"]).is_err());
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("640x480").unwrap(), (640, 480));
        assert!(parse_size("0x480").is_err());
        assert!(parse_size("640").is_err());
    }
}
