//! `ferroprobe`: describes a media file.

use crate::cli::log;
use crate::cli::options::InputSpec;
use crate::cli::pipeline::{Input, describe_input};
use crate::error::{Error, Result};
use crate::stream::{StreamInfo, StreamParams, format_rate};

#[derive(Debug, Default)]
struct ProbeOptions {
    show_format: bool,
    show_streams: bool,
    json: bool,
    hide_banner: bool,
    input: InputSpec,
}

const HELP: &str = "usage: ferroprobe [options] INPUT

  -show_format      print container information
  -show_streams     print one section per stream
  -of json          print the sections as JSON (default: key=value)
  -f FORMAT         force the input format (lavfi for generated sources)
  -hide_banner      do not print the version line
  -v LEVEL          quiet, error, warning, info, verbose
";

fn parse(args: &[String]) -> Result<Option<ProbeOptions>> {
    let mut o = ProbeOptions::default();
    let mut path = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |name: &str| {
            it.next().cloned().ok_or_else(|| {
                Error::invalid_argument(format!("missing argument for option '{name}'"))
            })
        };
        match a.as_str() {
            "-h" | "-help" | "--help" => return Ok(None),
            "-show_format" => o.show_format = true,
            "-show_streams" => o.show_streams = true,
            "-hide_banner" => o.hide_banner = true,
            "-of" | "-print_format" | "-output_format" => {
                let v = value("of")?;
                o.json = match v.as_str() {
                    "json" => true,
                    "default" => false,
                    _ => {
                        return Err(Error::invalid_argument(format!(
                            "unknown output format '{v}'"
                        )));
                    }
                };
            }
            "-f" => o.input.format = Some(value("f")?),
            "-v" | "-loglevel" => {
                let v = value("v")?;
                log::set_level(
                    log::LogLevel::parse(&v).ok_or_else(|| {
                        Error::invalid_argument(format!("invalid log level '{v}'"))
                    })?,
                );
            }
            "-i" => path = Some(value("i")?),
            s if s.starts_with('-') && s != "-" => {
                return Err(Error::invalid_argument(format!(
                    "unrecognized option '{s}'"
                )));
            }
            s => {
                if path.replace(s.to_string()).is_some() {
                    return Err(Error::invalid_argument("more than one input file"));
                }
            }
        }
    }
    o.input.path = path.ok_or_else(|| Error::invalid_argument("no input file"))?;
    Ok(Some(o))
}

fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Section fields as key and already formatted value; `quoted` tells
/// whether the JSON writer must quote it.
type Fields = Vec<(String, String, bool)>;

fn stream_fields(s: &StreamInfo) -> Fields {
    let mut f: Fields = vec![
        ("index".into(), s.index.to_string(), false),
        ("codec_name".into(), s.codec.name().into(), true),
        ("codec_long_name".into(), s.codec.long_name().into(), true),
        (
            "codec_type".into(),
            s.media_type().to_string().to_lowercase(),
            true,
        ),
    ];
    match &s.params {
        StreamParams::Audio(a) => {
            f.push(("sample_rate".into(), a.sample_rate.to_string(), true));
            f.push(("channels".into(), a.channels.to_string(), false));
            f.push((
                "bits_per_sample".into(),
                a.bits_per_sample.to_string(),
                false,
            ));
        }
        StreamParams::Video(v) => {
            f.push(("width".into(), v.width.to_string(), false));
            f.push(("height".into(), v.height.to_string(), false));
            f.push(("pix_fmt".into(), v.pixel_format.name().into(), true));
            f.push((
                "r_frame_rate".into(),
                format!("{}/{}", v.frame_rate.num, v.frame_rate.den),
                true,
            ));
            f.push(("avg_frame_rate".into(), format_rate(v.frame_rate), true));
        }
    }
    f.push(("time_base".into(), s.time_base.to_string(), true));
    if let Some(d) = s.duration {
        f.push(("duration_ts".into(), d.to_string(), false));
    }
    if let Some(d) = s.duration_seconds() {
        f.push(("duration".into(), format!("{d:.6}"), true));
    }
    if let Some(b) = s.bit_rate {
        f.push(("bit_rate".into(), b.to_string(), true));
    }
    f
}

fn format_fields(path: &str, input: &Input) -> Fields {
    let mut f: Fields = vec![
        ("filename".into(), path.into(), true),
        ("nb_streams".into(), input.streams.len().to_string(), false),
        ("format_name".into(), input.format_name.clone(), true),
    ];
    if let Some(d) = input.duration() {
        f.push(("duration".into(), format!("{d:.6}"), true));
    }
    if let Ok(meta) = std::fs::metadata(path) {
        f.push(("size".into(), meta.len().to_string(), true));
    }
    f
}

fn print_default(name: &str, fields: &Fields, tags: &[(String, String)]) {
    println!("[{name}]");
    for (k, v, _) in fields {
        println!("{k}={v}");
    }
    for (k, v) in tags {
        println!("TAG:{k}={v}");
    }
    println!("[/{name}]");
}

fn json_object(fields: &Fields, tags: &[(String, String)], indent: &str) -> String {
    let mut parts: Vec<String> = fields
        .iter()
        .map(|(k, v, quoted)| {
            if *quoted {
                format!("{indent}  \"{}\": \"{}\"", escape_json(k), escape_json(v))
            } else {
                format!("{indent}  \"{}\": {v}", escape_json(k))
            }
        })
        .collect();
    if !tags.is_empty() {
        let inner: Vec<String> = tags
            .iter()
            .map(|(k, v)| format!("{indent}    \"{}\": \"{}\"", escape_json(k), escape_json(v)))
            .collect();
        parts.push(format!(
            "{indent}  \"tags\": {{\n{}\n{indent}  }}",
            inner.join(",\n")
        ));
    }
    format!("{indent}{{\n{}\n{indent}}}", parts.join(",\n"))
}

pub fn main(args: Vec<String>) -> i32 {
    match run(&args) {
        Ok(()) => 0,
        Err(e) => {
            log::error(format!("Error: {e}"));
            1
        }
    }
}

fn run(args: &[String]) -> Result<()> {
    let Some(o) = parse(args)? else {
        print!("{HELP}");
        return Ok(());
    };
    if !o.hide_banner {
        log::info(format!("ferroprobe version {}", super::VERSION));
    }
    let input = Input::open(&o.input)?;
    describe_input(0, &o.input.path, &input);

    let streams: Vec<Fields> = input.streams.iter().map(stream_fields).collect();
    let format = format_fields(&o.input.path, &input);
    if o.json {
        let mut sections = Vec::new();
        if o.show_streams {
            let items: Vec<String> = input
                .streams
                .iter()
                .zip(&streams)
                .map(|(s, f)| json_object(f, &s.metadata, "    "))
                .collect();
            sections.push(format!("  \"streams\": [\n{}\n  ]", items.join(",\n")));
        }
        if o.show_format {
            sections.push(format!(
                "  \"format\": {}",
                json_object(&format, &input.metadata, "  ").trim_start()
            ));
        }
        println!("{{\n{}\n}}", sections.join(",\n"));
    } else {
        if o.show_streams {
            for (s, f) in input.streams.iter().zip(&streams) {
                print_default("STREAM", f, &s.metadata);
            }
        }
        if o.show_format {
            print_default("FORMAT", &format, &input.metadata);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_escaping() {
        assert_eq!(escape_json("a\"b\\c\n\u{1}"), "a\\\"b\\\\c\\n\\u0001");
    }

    #[test]
    fn parses_probe_options() {
        let args: Vec<String> = ["-of", "json", "-show_streams", "x.wav"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let o = parse(&args).unwrap().unwrap();
        assert!(o.json && o.show_streams && !o.show_format);
        assert_eq!(o.input.path, "x.wav");
        assert!(parse(&["a".into(), "b".into()]).is_err());
    }
}
