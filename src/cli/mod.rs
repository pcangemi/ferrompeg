//! The `ferrompeg` command line.

pub mod log;
pub mod options;
pub mod pipeline;
pub mod probe;

use crate::codec;
use crate::format;
use crate::source;
use crate::stream::{CodecId, MediaType};
use options::Action;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const HELP: &str =
    "usage: ferrompeg [global options] [input options] -i INPUT [output options] OUTPUT...

Global options:
  -y                overwrite output files without asking
  -n                never overwrite output files
  -hide_banner      do not print the version line
  -v LEVEL          quiet, error, warning, info, verbose
  -nostats          do not print the progress line

Input options (before -i):
  -f FORMAT         force the format (raw PCM such as s16le, rawvideo, lavfi)
  -ss TIME          skip to TIME
  -t DURATION       read at most DURATION
  -to TIME          stop reading at TIME
  -ar RATE -ac N    sample rate and channel count of raw PCM
  -s WxH -pix_fmt FMT -r RATE
                    picture size, pixel format and frame rate of raw video

Output options (before each output file):
  -f FORMAT         force the format
  -c:a CODEC        audio codec (or copy), -c:v CODEC for video, -c for both
  -af FILTERS       audio filter chain, e.g. volume=0.5,aresample=48000
  -vf FILTERS       video filter chain, e.g. scale=320:240,hflip
  -ar RATE -ac N    resample and remix the audio
  -s WxH -pix_fmt FMT -r RATE
                    scale, convert and change the frame rate of the video
  -ss TIME -t DURATION -to TIME
                    cut the output
  -frames:v N       stop after N video frames (-frames:a for audio)
  -an -vn           drop audio or video
  -compression_level N   encoder effort (FLAC: 0 to 8)
  -loop N           GIF loop count (0 forever, -1 once)
  -metadata KEY=VALUE
  -sws_flags ALGO   scaling: neighbor, bilinear, bicubic, area

Information:
  -formats -codecs -filters -version -h

TIME is seconds (5, 1.5, 250ms) or [HH:]MM:SS[.xx].
Generated inputs: -f lavfi -i \"sine=frequency=440:duration=5\" (see -filters).
";

fn print_formats() {
    println!("File formats:");
    println!(" D. = demuxing supported");
    println!(" .E = muxing supported");
    println!(" --");
    let mut names: Vec<(&str, &str, bool, bool)> = Vec::new();
    for f in format::input_formats() {
        names.push((f.name, f.long_name, true, false));
    }
    for f in format::output_formats() {
        match names.iter_mut().find(|n| n.0 == f.name) {
            Some(n) => n.3 = true,
            None => names.push((f.name, f.long_name, false, true)),
        }
    }
    names.push(("lavfi", "generated sources (sine, testsrc...)", true, false));
    names.sort_by_key(|n| n.0);
    for (name, long, d, e) in names {
        println!(
            " {}{} {name:<14} {long}",
            if d { 'D' } else { ' ' },
            if e { 'E' } else { ' ' }
        );
    }
}

fn print_codecs() {
    println!("Codecs:");
    println!(" D.. = decoding supported");
    println!(" .E. = encoding supported");
    println!(" ..A = audio, ..V = video");
    println!(" ---");
    for c in CodecId::ALL {
        println!(
            " {}{}{} {:<12} {}",
            if codec::can_decode(c) { 'D' } else { '.' },
            if codec::can_encode(c) { 'E' } else { '.' },
            if c.media_type() == MediaType::Audio {
                'A'
            } else {
                'V'
            },
            c.name(),
            c.long_name()
        );
    }
}

fn print_filters() {
    let tag = |m: MediaType| if m == MediaType::Audio { 'A' } else { 'V' };
    println!("Filters:");
    println!("  A = audio, V = video, | = source (no input)");
    println!("  --");
    for f in crate::filter::filters() {
        let t = tag(f.media);
        println!("  {t}->{t} {:<14} {}", f.name, f.description);
    }
    for s in source::sources() {
        println!("  |->{} {:<14} {}", tag(s.media), s.name, s.description);
    }
}

/// Runs the command line and returns the process exit code.
pub fn main(args: Vec<String>) -> i32 {
    let (global, action) = match options::parse(&args) {
        Ok(parsed) => parsed,
        Err(e) => {
            eprintln!("Error: {e}");
            eprintln!("Use -h to get full help.");
            return 1;
        }
    };
    if let Some(level) = global.log_level {
        log::set_level(level);
    }
    if !global.hide_banner && matches!(action, Action::Transcode { .. }) {
        log::info(format!("ferrompeg version {VERSION}"));
    }
    match action {
        Action::Help => print!("{HELP}"),
        Action::Version => println!("ferrompeg version {VERSION}"),
        Action::ListFormats => print_formats(),
        Action::ListCodecs => print_codecs(),
        Action::ListFilters => print_filters(),
        Action::Transcode { inputs, outputs } => {
            if let Err(e) = pipeline::run(&global, &inputs, &outputs) {
                log::error(format!("Error: {e}"));
                return 1;
            }
        }
    }
    0
}
