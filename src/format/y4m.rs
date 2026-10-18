//! YUV4MPEG2 streams (`.y4m`).
//!
//! Written from the yuv4mpeg(5) manual page, which defines the format,
//! and the YUV4MPEG2 description on the MultimediaWiki. A stream is one
//! text header line, `YUV4MPEG2` followed by space separated tags, then
//! for every picture a `FRAME` line (which may carry its own tags) and the
//! raw planes, Y then Cb then Cr, 8 bits per sample.
//!
//! Header tags read here: `W` width, `H` height, `F` frame rate `n:d`, `A`
//! sample aspect ratio `n:d` (`0:0` for unknown), `I` interlacing, `C`
//! colorspace and `X` application comments. Interlaced streams are read
//! as progressive pictures: the samples are stored the same way and only
//! the display differs.

use std::io::{Read, Write};

use crate::codec::rawvideo;
use crate::error::{Error, Result};
use crate::format::{Demuxer, InputFormat, InputOptions, Muxer, OutputFormat, OutputOptions};
use crate::frame::PixelFormat;
use crate::io::Output;
use crate::rational::Rational;
use crate::stream::{CodecId, Packet, StreamInfo, StreamParams, VideoParams};
use crate::util::bytes::ReadBytes;

const MAGIC: &[u8] = b"YUV4MPEG2";

/// Longest stream header accepted. Real headers are well under 100 bytes,
/// but `X` comments are free text, so leave generous room.
const MAX_HEADER_LEN: usize = 64 * 1024;

/// Longest `FRAME` line accepted, parameters included.
const MAX_FRAME_LINE_LEN: usize = 4096;

/// Largest width or height accepted. This keeps the per-picture size of a
/// corrupt header within what fits in memory (16384 x 16384 4:4:4 is
/// 768 MiB) and far from any `usize` overflow.
const MAX_DIMENSION: u32 = 16384;

/// Rate assumed when the header has no `F` tag or says `F0:0` (unknown),
/// the same default as headerless raw video.
const DEFAULT_RATE: Rational = Rational { num: 25, den: 1 };

pub const INPUT: InputFormat = InputFormat {
    name: "yuv4mpegpipe",
    long_name: "YUV4MPEG pipe",
    extensions: &["y4m"],
    probe,
    open,
    raw_codec: None,
};

pub const OUTPUT: OutputFormat = OutputFormat {
    name: "yuv4mpegpipe",
    long_name: "YUV4MPEG pipe",
    extensions: &["y4m"],
    audio_codec: None,
    video_codec: Some(CodecId::RawVideo),
    codecs: &[CodecId::RawVideo],
    pixel_formats: &[
        PixelFormat::Yuv420p,
        PixelFormat::Yuv422p,
        PixelFormat::Yuv444p,
        PixelFormat::Gray,
    ],
    create,
};

fn probe(data: &[u8]) -> u32 {
    if data.starts_with(b"YUV4MPEG2 ") {
        100
    } else {
        0
    }
}

fn open(reader: Box<dyn Read>, _options: &InputOptions) -> Result<Box<dyn Demuxer>> {
    Ok(Box::new(Y4mDemuxer::new(reader)?))
}

fn create(path: &str, _options: &OutputOptions) -> Result<Box<dyn Muxer>> {
    Ok(Box::new(Y4mMuxer::new(Output::create(path)?)))
}

/// Reads bytes up to and excluding the next `\n`. Returns `None` when the
/// stream ends before the first byte, and the partial line with `false`
/// when it ends in the middle of one.
fn read_line(reader: &mut dyn Read, max: usize) -> Result<Option<(Vec<u8>, bool)>> {
    let mut line = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        if reader.read_full(&mut byte)? == 0 {
            return Ok(if line.is_empty() {
                None
            } else {
                Some((line, false))
            });
        }
        if byte[0] == b'\n' {
            return Ok(Some((line, true)));
        }
        if line.len() == max {
            return Err(Error::invalid_data(format!(
                "YUV4MPEG line longer than {max} bytes"
            )));
        }
        line.push(byte[0]);
    }
}

/// Parses `n:d` with both parts plain decimal numbers.
fn parse_ratio(value: &[u8], tag: char) -> Result<(u32, u32)> {
    let bad = || {
        Error::invalid_data(format!(
            "YUV4MPEG tag {tag} expects n:d, got '{}'",
            String::from_utf8_lossy(value)
        ))
    };
    let text = std::str::from_utf8(value).map_err(|_| bad())?;
    let (n, d) = text.split_once(':').ok_or_else(bad)?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(n) || !digits(d) {
        return Err(bad());
    }
    Ok((n.parse().map_err(|_| bad())?, d.parse().map_err(|_| bad())?))
}

fn parse_dimension(value: &[u8], tag: char) -> Result<u32> {
    let text = std::str::from_utf8(value).unwrap_or("");
    let n: u32 = if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
        text.parse().unwrap_or(u32::MAX)
    } else {
        return Err(Error::invalid_data(format!(
            "YUV4MPEG tag {tag} expects a number, got '{}'",
            String::from_utf8_lossy(value)
        )));
    };
    if n == 0 {
        return Err(Error::invalid_data(format!("YUV4MPEG tag {tag} is zero")));
    }
    if n > MAX_DIMENSION {
        return Err(Error::unsupported(format!(
            "YUV4MPEG picture {} of {n} (the limit is {MAX_DIMENSION})",
            if tag == 'W' { "width" } else { "height" }
        )));
    }
    Ok(n)
}

/// Maps a `C` tag to a pixel format. Only the 8-bit layouts exist in the
/// crate; deeper variants (`420p10`, `mono16`...) are recognized so the
/// error says what is missing rather than calling the file broken.
fn parse_colorspace(value: &[u8]) -> Result<PixelFormat> {
    Ok(match value {
        // The 4:2:0 variants differ only in where chroma samples sit
        // relative to luma; the stored layout is the same.
        b"420jpeg" | b"420paldv" | b"420mpeg2" | b"420" => PixelFormat::Yuv420p,
        b"422" => PixelFormat::Yuv422p,
        b"444" => PixelFormat::Yuv444p,
        b"mono" => PixelFormat::Gray,
        _ => {
            let name = String::from_utf8_lossy(value);
            let deep = ["420p", "422p", "444p", "mono"].iter().any(|prefix| {
                name.strip_prefix(prefix).is_some_and(|bits| {
                    !bits.is_empty() && bits.bytes().all(|b| b.is_ascii_digit())
                })
            });
            return Err(Error::unsupported(if deep {
                format!("YUV4MPEG colorspace C{name} (only 8-bit samples are implemented)")
            } else {
                format!("YUV4MPEG colorspace C{name}")
            }));
        }
    })
}

/// Stream parameters decoded from the header line.
#[derive(Debug, PartialEq)]
struct Header {
    width: u32,
    height: u32,
    rate: Rational,
    aspect: Rational,
    format: PixelFormat,
}

fn parse_header(line: &[u8]) -> Result<Header> {
    let rest = line
        .strip_prefix(MAGIC)
        .filter(|r| r.is_empty() || r[0] == b' ')
        .ok_or_else(|| Error::invalid_data("not a YUV4MPEG2 stream"))?;
    let mut width = None;
    let mut height = None;
    let mut rate = DEFAULT_RATE;
    // Rational::new(0, 1) is how the crate spells "unknown aspect".
    let mut aspect = Rational::new(0, 1);
    // The format's own default when no C tag is present.
    let mut format = PixelFormat::Yuv420p;
    for token in rest.split(|&b| b == b' ').filter(|t| !t.is_empty()) {
        let value = &token[1..];
        match token[0] {
            b'W' => width = Some(parse_dimension(value, 'W')?),
            b'H' => height = Some(parse_dimension(value, 'H')?),
            b'F' => {
                rate = match parse_ratio(value, 'F')? {
                    (0, 0) => DEFAULT_RATE,
                    (0, _) | (_, 0) => {
                        return Err(Error::invalid_data(format!(
                            "YUV4MPEG frame rate F{} is not valid",
                            String::from_utf8_lossy(value)
                        )));
                    }
                    (n, d) => Rational::new(n as i64, d as i64),
                }
            }
            b'A' => {
                aspect = match parse_ratio(value, 'A')? {
                    // 0:0 is the documented "unknown"; any other zero is
                    // meaningless and treated the same rather than failing
                    // on a value that only affects display.
                    (0, _) | (_, 0) => Rational::new(0, 1),
                    (n, d) => Rational::new(n as i64, d as i64),
                }
            }
            b'I' => match value {
                // Progressive, top or bottom field first, or mixed. All are
                // stored as whole pictures.
                b"p" | b"t" | b"b" | b"m" | b"?" => {}
                _ => {
                    return Err(Error::invalid_data(format!(
                        "YUV4MPEG interlacing I{} is not valid",
                        String::from_utf8_lossy(value)
                    )));
                }
            },
            b'C' => format = parse_colorspace(value)?,
            // X tags are application comments; unknown letters are skipped
            // too so that newer writers stay readable.
            _ => {}
        }
    }
    let width = width.ok_or_else(|| Error::invalid_data("YUV4MPEG header has no W tag"))?;
    let height = height.ok_or_else(|| Error::invalid_data("YUV4MPEG header has no H tag"))?;
    Ok(Header {
        width,
        height,
        rate,
        aspect,
        format,
    })
}

pub struct Y4mDemuxer {
    reader: Box<dyn Read>,
    streams: Vec<StreamInfo>,
    frame_size: usize,
    next_pts: i64,
    done: bool,
}

impl Y4mDemuxer {
    pub fn new(mut reader: Box<dyn Read>) -> Result<Self> {
        let line = match read_line(&mut reader, MAX_HEADER_LEN)? {
            Some((line, true)) => line,
            _ => return Err(Error::invalid_data("YUV4MPEG header line is incomplete")),
        };
        let h = parse_header(&line)?;
        let params = VideoParams {
            width: h.width,
            height: h.height,
            pixel_format: h.format,
            frame_rate: h.rate,
            sample_aspect_ratio: h.aspect,
        };
        let mut stream = StreamInfo::new(
            0,
            CodecId::RawVideo,
            StreamParams::Video(params),
            h.rate.inv(),
        );
        stream.bit_rate = rawvideo::bit_rate(h.format, h.width, h.height, h.rate);
        Ok(Y4mDemuxer {
            reader,
            streams: vec![stream],
            frame_size: h.format.frame_size(h.width, h.height),
            next_pts: 0,
            done: false,
        })
    }
}

impl Demuxer for Y4mDemuxer {
    fn format_name(&self) -> &'static str {
        "yuv4mpegpipe"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn read_packet(&mut self) -> Result<Option<Packet>> {
        if self.done {
            return Ok(None);
        }
        let line = match read_line(&mut self.reader, MAX_FRAME_LINE_LEN)? {
            Some((line, true)) => line,
            // End of stream, or a stream cut inside a FRAME line: either
            // way no further picture can be read.
            _ => {
                self.done = true;
                return Ok(None);
            }
        };
        let is_frame = line
            .strip_prefix(b"FRAME")
            .is_some_and(|r| r.is_empty() || r[0] == b' ');
        if !is_frame {
            self.done = true;
            return Err(Error::invalid_data(format!(
                "expected a YUV4MPEG FRAME line, found '{}'",
                String::from_utf8_lossy(&line[..line.len().min(32)])
            )));
        }
        // Frame parameters only restate interlacing or add comments,
        // neither of which changes how the samples are read.
        let data = match self.reader.read_vec(self.frame_size) {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // A truncated last picture is dropped, like a partial
                // block at the end of a WAV file.
                self.done = true;
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        let packet = Packet::new(0, data, self.next_pts, 1);
        self.next_pts += 1;
        Ok(Some(packet))
    }
}

pub struct Y4mMuxer {
    out: Output,
    frame_size: usize,
}

impl Y4mMuxer {
    pub fn new(out: Output) -> Self {
        Y4mMuxer { out, frame_size: 0 }
    }

    pub fn into_output(self) -> Output {
        self.out
    }
}

/// Header text for a stream, without the trailing newline.
fn header_line(v: &VideoParams) -> Result<String> {
    // Besides the C tag, two widely used X comments are written:
    // XYSCSS names the chroma layout for readers that only know that
    // spelling, and XCOLORRANGE=FULL marks monochrome pictures as full
    // range (the crate's gray format), since YUV4MPEG otherwise implies
    // limited range luma.
    let colorspace = match v.pixel_format {
        PixelFormat::Yuv420p => "C420jpeg XYSCSS=420JPEG",
        PixelFormat::Yuv422p => "C422 XYSCSS=422",
        PixelFormat::Yuv444p => "C444 XYSCSS=444",
        PixelFormat::Gray => "Cmono XCOLORRANGE=FULL",
        f => {
            return Err(Error::unsupported(format!(
                "YUV4MPEG cannot store {f} pictures (use -pix_fmt yuv420p, yuv422p, yuv444p or gray)"
            )));
        }
    };
    if v.width == 0 || v.height == 0 {
        return Err(Error::invalid_argument("YUV4MPEG picture size is zero"));
    }
    let rate = v.frame_rate;
    if rate.num <= 0 || rate.den <= 0 {
        return Err(Error::invalid_argument(format!(
            "YUV4MPEG needs a positive frame rate, got {rate}"
        )));
    }
    let sar = v.sample_aspect_ratio;
    let (sar_num, sar_den) = if sar.num > 0 && sar.den > 0 {
        (sar.num, sar.den)
    } else {
        (0, 0)
    };
    Ok(format!(
        "YUV4MPEG2 W{} H{} F{}:{} Ip A{sar_num}:{sar_den} {colorspace}",
        v.width, v.height, rate.num, rate.den
    ))
}

impl Muxer for Y4mMuxer {
    fn write_header(&mut self, streams: &[StreamInfo]) -> Result<()> {
        let [stream] = streams else {
            return Err(Error::unsupported(
                "YUV4MPEG holds exactly one video stream",
            ));
        };
        if stream.codec != CodecId::RawVideo {
            return Err(Error::unsupported(format!("{} in YUV4MPEG", stream.codec)));
        }
        let v = stream
            .video()
            .ok_or_else(|| Error::unsupported("YUV4MPEG holds video only"))?;
        let line = header_line(v)?;
        self.frame_size = v.pixel_format.frame_size(v.width, v.height);
        self.out.write_all(line.as_bytes())?;
        self.out.write_all(b"\n")?;
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        // Readers find picture boundaries only from the header size, so a
        // packet of any other size would corrupt every later picture.
        if packet.data.len() != self.frame_size {
            return Err(Error::invalid_data(format!(
                "YUV4MPEG picture of {} bytes, the header announces {}",
                packet.data.len(),
                self.frame_size
            )));
        }
        self.out.write_all(b"FRAME\n")?;
        self.out.write_all(&packet.data)?;
        Ok(())
    }

    fn write_trailer(&mut self, _streams: &[StreamInfo]) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }

    fn bytes_written(&self) -> u64 {
        self.out.bytes_written()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A reference file and its planes as headerless raw video, with the
    /// picture size and layout it declares.
    type Fixture = (&'static [u8], &'static [u8], (u32, u32), PixelFormat);

    const FIXTURES: [Fixture; 4] = [
        (
            include_bytes!("../../tests/fixtures/video-containers/c420_16x8.y4m"),
            include_bytes!("../../tests/fixtures/video-containers/c420_16x8.yuv"),
            (16, 8),
            PixelFormat::Yuv420p,
        ),
        (
            include_bytes!("../../tests/fixtures/video-containers/c422_7x5.y4m"),
            include_bytes!("../../tests/fixtures/video-containers/c422_7x5.yuv"),
            (7, 5),
            PixelFormat::Yuv422p,
        ),
        (
            include_bytes!("../../tests/fixtures/video-containers/c444_5x3.y4m"),
            include_bytes!("../../tests/fixtures/video-containers/c444_5x3.yuv"),
            (5, 3),
            PixelFormat::Yuv444p,
        ),
        (
            include_bytes!("../../tests/fixtures/video-containers/mono_9x7.y4m"),
            include_bytes!("../../tests/fixtures/video-containers/mono_9x7.yuv"),
            (9, 7),
            PixelFormat::Gray,
        ),
    ];

    fn demux(bytes: &[u8]) -> Result<Y4mDemuxer> {
        Y4mDemuxer::new(Box::new(Cursor::new(bytes.to_vec())))
    }

    fn read_all(demux: &mut Y4mDemuxer) -> Vec<Packet> {
        let mut packets = Vec::new();
        while let Some(p) = demux.read_packet().unwrap() {
            packets.push(p);
        }
        packets
    }

    fn stream(width: u32, height: u32, format: PixelFormat, rate: Rational) -> StreamInfo {
        StreamInfo::new(
            0,
            CodecId::RawVideo,
            StreamParams::Video(VideoParams {
                width,
                height,
                pixel_format: format,
                frame_rate: rate,
                sample_aspect_ratio: Rational::new(1, 1),
            }),
            rate.inv(),
        )
    }

    fn mux(info: &StreamInfo, packets: &[Packet]) -> Vec<u8> {
        let mut m = Y4mMuxer::new(Output::memory());
        m.write_header(std::slice::from_ref(info)).unwrap();
        for p in packets {
            m.write_packet(p).unwrap();
        }
        m.write_trailer(std::slice::from_ref(info)).unwrap();
        m.into_output().into_bytes().unwrap()
    }

    #[test]
    fn reads_reference_files() {
        // The fixtures were written by a reference encoder together with
        // their planes as headerless raw video.
        for (y4m, raw, (w, h), format) in FIXTURES {
            let mut d = demux(y4m).unwrap();
            let info = d.streams()[0].clone();
            let v = info.video().unwrap();
            assert_eq!((v.width, v.height, v.pixel_format), (w, h, format));
            assert_eq!(v.frame_rate, Rational::new(25, 1));
            assert_eq!(v.sample_aspect_ratio, Rational::new(1, 1));
            assert_eq!(info.time_base, Rational::new(1, 25));
            let packets = read_all(&mut d);
            assert_eq!(packets.len(), 2);
            for (i, p) in packets.iter().enumerate() {
                assert_eq!(p.pts, i as i64);
                assert_eq!(p.duration, 1);
            }
            let data: Vec<u8> = packets.iter().flat_map(|p| p.data.clone()).collect();
            assert_eq!(data, raw, "{format} {w}x{h}");
        }
    }

    #[test]
    fn rewrites_reference_files_byte_for_byte() {
        for (y4m, ..) in FIXTURES {
            let mut d = demux(y4m).unwrap();
            let info = d.streams()[0].clone();
            let packets = read_all(&mut d);
            assert_eq!(mux(&info, &packets), y4m);
        }
    }

    #[test]
    fn writes_the_documented_header() {
        let info = stream(4, 2, PixelFormat::Yuv420p, Rational::new(30000, 1001));
        let bytes = mux(&info, &[Packet::new(0, (0..12).collect(), 0, 1)]);
        let mut expected =
            b"YUV4MPEG2 W4 H2 F30000:1001 Ip A1:1 C420jpeg XYSCSS=420JPEG\nFRAME\n".to_vec();
        expected.extend(0..12u8);
        assert_eq!(bytes, expected);

        let mut info = stream(3, 1, PixelFormat::Gray, Rational::new(50, 2));
        if let StreamParams::Video(v) = &mut info.params {
            v.sample_aspect_ratio = Rational::new(0, 1);
        }
        let bytes = mux(&info, &[]);
        assert_eq!(
            bytes,
            b"YUV4MPEG2 W3 H1 F25:1 Ip A0:0 Cmono XCOLORRANGE=FULL\n"
        );
    }

    #[test]
    fn round_trips_every_layout() {
        for format in OUTPUT.pixel_formats {
            for (w, h) in [(1, 1), (175, 143), (2, 9)] {
                let info = stream(w, h, *format, Rational::new(24000, 1001));
                let size = format.frame_size(w, h);
                let packets: Vec<Packet> = (0..3)
                    .map(|i| Packet::new(0, vec![i as u8 + 1; size], i, 1))
                    .collect();
                let bytes = mux(&info, &packets);
                let mut d = demux(&bytes).unwrap();
                let back = d.streams()[0].clone();
                assert_eq!(back.video(), info.video());
                assert_eq!(back.time_base, info.time_base);
                assert_eq!(read_all(&mut d), packets, "{format} {w}x{h}");
            }
        }
    }

    #[test]
    fn parses_every_tag() {
        let h = parse_header(b"YUV4MPEG2 C422 W720 H576 F25:1 It A128:117 XCOMMENT=hi Zfuture")
            .unwrap();
        assert_eq!(
            h,
            Header {
                width: 720,
                height: 576,
                rate: Rational::new(25, 1),
                aspect: Rational::new(128, 117),
                format: PixelFormat::Yuv422p,
            }
        );
        // Defaults: 4:2:0, unknown aspect, 25 frames per second.
        let h = parse_header(b"YUV4MPEG2 W2 H2").unwrap();
        assert_eq!(h.format, PixelFormat::Yuv420p);
        assert_eq!(h.aspect, Rational::new(0, 1));
        assert_eq!(h.rate, Rational::new(25, 1));
        assert_eq!(
            parse_header(b"YUV4MPEG2  W2  H2 F0:0 A0:0 ").unwrap().rate,
            Rational::new(25, 1)
        );
        for c in ["420jpeg", "420paldv", "420mpeg2", "420"] {
            let line = format!("YUV4MPEG2 W2 H2 C{c}");
            assert_eq!(
                parse_header(line.as_bytes()).unwrap().format,
                PixelFormat::Yuv420p
            );
        }
        assert_eq!(
            parse_header(b"YUV4MPEG2 W2 H2 C444").unwrap().format,
            PixelFormat::Yuv444p
        );
        assert_eq!(
            parse_header(b"YUV4MPEG2 W2 H2 Cmono").unwrap().format,
            PixelFormat::Gray
        );
        for i in ["p", "t", "b", "m", "?"] {
            let line = format!("YUV4MPEG2 W2 H2 I{i}");
            assert!(parse_header(line.as_bytes()).is_ok(), "I{i}");
        }
    }

    #[test]
    fn rejects_bad_headers() {
        let invalid = |line: &[u8]| {
            assert!(
                matches!(parse_header(line), Err(Error::InvalidData(_))),
                "{}",
                String::from_utf8_lossy(line)
            )
        };
        invalid(b"YUV4MPEG W2 H2");
        invalid(b"YUV4MPEG2X W2 H2");
        invalid(b"YUV4MPEG2 H2");
        invalid(b"YUV4MPEG2 W2");
        invalid(b"YUV4MPEG2 W0 H2");
        invalid(b"YUV4MPEG2 W-2 H2");
        invalid(b"YUV4MPEG2 W2x H2");
        invalid(b"YUV4MPEG2 W H2");
        invalid(b"YUV4MPEG2 W2 H2 F0:1");
        invalid(b"YUV4MPEG2 W2 H2 F25:0");
        invalid(b"YUV4MPEG2 W2 H2 F25");
        invalid(b"YUV4MPEG2 W2 H2 F25:1:1");
        invalid(b"YUV4MPEG2 W2 H2 F99999999999:1");
        invalid(b"YUV4MPEG2 W2 H2 A1");
        invalid(b"YUV4MPEG2 W2 H2 Ix");
        invalid(b"YUV4MPEG2 W\xff H2");
        assert!(matches!(
            parse_header(b"YUV4MPEG2 W99999999999 H2"),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            parse_header(b"YUV4MPEG2 W16385 H2"),
            Err(Error::Unsupported(_))
        ));
        assert_eq!(
            parse_header(b"YUV4MPEG2 W2 H2 A0:5").unwrap().aspect,
            Rational::new(0, 1)
        );
    }

    #[test]
    fn rejects_unsupported_colorspaces() {
        for c in [
            "420p10", "422p12", "444p16", "mono16", "444alpha", "411", "rgb", "",
        ] {
            let line = format!("YUV4MPEG2 W2 H2 C{c}");
            let err = parse_header(line.as_bytes()).unwrap_err();
            assert!(matches!(err, Error::Unsupported(_)), "C{c}: {err}");
        }
        let err = parse_header(b"YUV4MPEG2 W2 H2 C420p10").unwrap_err();
        assert!(err.to_string().contains("8-bit"));
    }

    #[test]
    fn huge_header_does_not_allocate_a_picture() {
        // A legal maximum size header followed by almost nothing: opening
        // must not reserve the announced picture, and reading ends cleanly.
        let mut d = demux(b"YUV4MPEG2 W16384 H16384 C444\nFRAME\nabc").unwrap();
        assert!(d.read_packet().unwrap().is_none());
        assert!(d.read_packet().unwrap().is_none());
    }

    #[test]
    fn rejects_overlong_lines() {
        let mut header = b"YUV4MPEG2 W2 H2 X".to_vec();
        header.resize(MAX_HEADER_LEN + 10, b'a');
        header.push(b'\n');
        assert!(demux(&header).is_err());
        let mut bytes = b"YUV4MPEG2 W2 H2\nFRAME ".to_vec();
        bytes.resize(bytes.len() + MAX_FRAME_LINE_LEN, b'x');
        bytes.push(b'\n');
        assert!(demux(&bytes).unwrap().read_packet().is_err());
    }

    #[test]
    fn handles_frame_lines_and_truncation() {
        // 2x2 4:2:0 pictures are 6 bytes. The second FRAME line carries
        // parameters; the third picture is cut short and dropped.
        let mut bytes = b"YUV4MPEG2 W2 H2 F10:1\nFRAME\n".to_vec();
        bytes.extend_from_slice(&[1; 6]);
        bytes.extend_from_slice(b"FRAME Ib XNOTE=x\n");
        bytes.extend_from_slice(&[2; 6]);
        bytes.extend_from_slice(b"FRAME\n\x03\x03");
        let mut d = demux(&bytes).unwrap();
        assert_eq!(d.streams()[0].time_base, Rational::new(1, 10));
        let packets = read_all(&mut d);
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[1].data, [2; 6]);
        assert_eq!(packets[1].pts, 1);

        // A stream cut inside a FRAME line also just ends.
        let mut d = demux(b"YUV4MPEG2 W2 H2\nFRA").unwrap();
        assert!(d.read_packet().unwrap().is_none());
    }

    #[test]
    fn rejects_garbage_between_frames() {
        let mut bytes = b"YUV4MPEG2 W2 H2\nFRAME\n".to_vec();
        bytes.extend_from_slice(&[0; 6]);
        bytes.extend_from_slice(b"FRAMES\n");
        let mut d = demux(&bytes).unwrap();
        assert!(d.read_packet().unwrap().is_some());
        assert!(d.read_packet().is_err());
        assert!(d.read_packet().unwrap().is_none());
    }

    #[test]
    fn rejects_incomplete_or_foreign_headers() {
        assert!(demux(b"").is_err());
        assert!(demux(b"YUV4MPEG2 W2 H2").is_err());
        assert!(demux(b"RIFF....WAVE\n").is_err());
        assert_eq!(probe(b"YUV4MPEG2 W2"), 100);
        assert_eq!(probe(b"YUV4MPEG W2"), 0);
    }

    #[test]
    fn muxer_rejects_what_it_cannot_store() {
        let mut m = Y4mMuxer::new(Output::memory());
        let rgb = stream(2, 2, PixelFormat::Rgb24, Rational::new(25, 1));
        assert!(matches!(
            m.write_header(std::slice::from_ref(&rgb)),
            Err(Error::Unsupported(_))
        ));
        let gray = stream(2, 2, PixelFormat::Gray, Rational::new(25, 1));
        assert!(m.write_header(&[gray.clone(), gray.clone()]).is_err());
        let mut gif = gray.clone();
        gif.codec = CodecId::Gif;
        assert!(m.write_header(&[gif]).is_err());

        let mut m = Y4mMuxer::new(Output::memory());
        m.write_header(std::slice::from_ref(&gray)).unwrap();
        assert!(m.write_packet(&Packet::new(0, vec![0; 3], 0, 1)).is_err());
        assert!(m.write_packet(&Packet::new(0, vec![0; 5], 0, 1)).is_err());
        m.write_packet(&Packet::new(0, vec![0; 4], 0, 1)).unwrap();
    }
}
