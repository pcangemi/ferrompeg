//! Image sequences: one PPM, PGM or BMP file per frame (`out%03d.ppm`).
//!
//! The file type follows the extension. Formats, from their published
//! descriptions:
//!
//! * PPM and PGM, binary variants (`P6` and `P5`) as defined by the
//!   Netpbm ppm(5) and pgm(5) manual pages: a short text header giving
//!   the size and a maximum value of 255, then the samples row by row;
//! * BMP with a BITMAPFILEHEADER and a BITMAPINFOHEADER as documented in
//!   the Windows GDI reference: 24 bits per pixel, `BI_RGB` (no
//!   compression), pixels stored as blue, green, red, rows bottom-up and
//!   each padded to a multiple of 4 bytes.
//!
//! The stream reaching the muxer is rgb24 or gray. When the extension
//! needs the other one the muxer converts: gray to RGB repeats the value,
//! RGB to gray uses the BT.601 luma weights (Rec. ITU-R BT.601).
//!
//! A path with a printf style number (`%d`, `%05d`) gets one file per
//! frame, numbered from 1. A plain path takes exactly one frame.

use std::borrow::Cow;
use std::io::Write;
use std::path::Path;

use crate::error::{Error, Result};
use crate::format::{Muxer, OutputFormat, OutputOptions};
use crate::frame::PixelFormat;
use crate::io::Output;
use crate::stream::{CodecId, Packet, StreamInfo};

/// Widest zero padding accepted in a pattern. Anything wider is surely a
/// typo, and refusing it keeps a `%999999999d` from building huge names.
const MAX_PAD: usize = 32;

/// Number given to the first file of a sequence.
const FIRST_NUMBER: u64 = 1;

pub const OUTPUT: OutputFormat = OutputFormat {
    name: "image2",
    long_name: "image file sequence",
    extensions: &["ppm", "pgm", "bmp"],
    audio_codec: None,
    video_codec: Some(CodecId::RawVideo),
    codecs: &[CodecId::RawVideo],
    pixel_formats: &[PixelFormat::Rgb24, PixelFormat::Gray],
    create,
};

fn create(path: &str, _options: &OutputOptions) -> Result<Box<dyn Muxer>> {
    Ok(Box::new(Image2Muxer::new(path, Destination::Files)?))
}

/// True if `path` contains a printf style frame number such as `%d` or
/// `%03d`.
pub fn is_pattern(path: &str) -> bool {
    let bytes = path.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'd' {
                return true;
            }
            // "%%" is a literal percent sign.
            i = j.max(i + 2);
            continue;
        }
        i += 1;
    }
    false
}

/// Expands every `%d` style conversion of `pattern` with `number`, with
/// printf semantics: `%05d` pads with zeros, `%5d` with spaces, and `%%`
/// is a literal percent sign. Other `%` sequences are kept as written.
fn expand_pattern(pattern: &str, number: u64) -> Result<String> {
    let mut out = String::with_capacity(pattern.len() + 8);
    let mut chars = pattern.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        if chars.peek().map(|&(_, c)| c) == Some('%') {
            chars.next();
            out.push('%');
            continue;
        }
        let spec_start = i + 1;
        let mut spec_end = spec_start;
        while let Some(&(j, d)) = chars.peek() {
            if !d.is_ascii_digit() {
                break;
            }
            spec_end = j + 1;
            chars.next();
        }
        if chars.peek().map(|&(_, c)| c) != Some('d') {
            // Not a number conversion: keep the text unchanged.
            out.push_str(&pattern[i..spec_end]);
            continue;
        }
        chars.next();
        let spec = &pattern[spec_start..spec_end];
        let zero = spec.starts_with('0');
        let width = if spec.is_empty() {
            0
        } else {
            spec.parse::<usize>().unwrap_or(usize::MAX)
        };
        if width > MAX_PAD {
            return Err(Error::invalid_argument(format!(
                "'{pattern}': frame number padding %{spec}d is wider than {MAX_PAD}"
            )));
        }
        if zero {
            out.push_str(&format!("{number:0width$}"));
        } else {
            out.push_str(&format!("{number:width$}"));
        }
    }
    Ok(out)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImageKind {
    Ppm,
    Pgm,
    Bmp,
}

impl ImageKind {
    fn from_path(path: &str) -> Result<Self> {
        let ext = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        match ext.as_deref() {
            Some("ppm") => Ok(ImageKind::Ppm),
            Some("pgm") => Ok(ImageKind::Pgm),
            Some("bmp") => Ok(ImageKind::Bmp),
            _ => Err(Error::unsupported(format!(
                "'{path}': image sequences are written as .ppm, .pgm or .bmp files"
            ))),
        }
    }

    /// Encodes one picture. `data` is a tightly packed rgb24 or gray
    /// picture of `width` x `height`.
    fn encode(self, format: PixelFormat, width: u32, height: u32, data: &[u8]) -> Result<Vec<u8>> {
        match self {
            ImageKind::Ppm => Ok(netpbm(b"P6", width, height, &to_rgb(format, data))),
            ImageKind::Pgm => Ok(netpbm(b"P5", width, height, &to_gray(format, data))),
            ImageKind::Bmp => bmp(width, height, &to_rgb(format, data)),
        }
    }
}

fn to_rgb(format: PixelFormat, data: &[u8]) -> Cow<'_, [u8]> {
    match format {
        PixelFormat::Gray => Cow::Owned(data.iter().flat_map(|&y| [y, y, y]).collect()),
        _ => Cow::Borrowed(data),
    }
}

fn to_gray(format: PixelFormat, data: &[u8]) -> Cow<'_, [u8]> {
    match format {
        PixelFormat::Rgb24 => Cow::Owned(data.chunks_exact(3).map(luma).collect()),
        _ => Cow::Borrowed(data),
    }
}

/// BT.601 luma of a full range RGB pixel, rounded to nearest. The weights
/// add up to 1000, so white stays 255.
fn luma(rgb: &[u8]) -> u8 {
    let (r, g, b) = (rgb[0] as u32, rgb[1] as u32, rgb[2] as u32);
    ((299 * r + 587 * g + 114 * b + 500) / 1000) as u8
}

/// A binary Netpbm file: magic, size, maximum value, then the samples.
/// One whitespace byte ends the header, as the format requires.
fn netpbm(magic: &[u8], width: u32, height: u32, samples: &[u8]) -> Vec<u8> {
    let header = format!("{width} {height}\n255\n");
    let mut out = Vec::with_capacity(3 + header.len() + samples.len());
    out.extend_from_slice(magic);
    out.push(b'\n');
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(samples);
    out
}

/// Byte length of a BMP row of 24-bit pixels: rows start on 4-byte
/// boundaries.
fn bmp_stride(width: u32) -> u64 {
    (width as u64 * 3).div_ceil(4) * 4
}

/// A 24-bit uncompressed BMP from packed RGB. A positive height in the
/// info header means the rows are stored bottom-up.
fn bmp(width: u32, height: u32, rgb: &[u8]) -> Result<Vec<u8>> {
    const HEADERS: u32 = 14 + 40;
    let stride = bmp_stride(width);
    let image_size = stride * height as u64;
    let file_size = u32::try_from(image_size + HEADERS as u64)
        .map_err(|_| Error::unsupported("BMP file larger than 4 GiB"))?;
    let stride = stride as usize;
    let mut out = Vec::with_capacity(file_size as usize);
    // BITMAPFILEHEADER.
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&file_size.to_le_bytes());
    out.extend_from_slice(&[0; 4]); // two reserved words
    out.extend_from_slice(&HEADERS.to_le_bytes()); // offset of the pixels
    // BITMAPINFOHEADER.
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(width as i32).to_le_bytes());
    out.extend_from_slice(&(height as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&24u16.to_le_bytes()); // bits per pixel
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&(image_size as u32).to_le_bytes());
    // Resolution is unknown; zero lets viewers pick their default.
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // palette entries
    out.extend_from_slice(&0u32.to_le_bytes()); // important colors
    let row_bytes = width as usize * 3;
    for row in rgb.chunks_exact(row_bytes).rev() {
        for px in row.chunks_exact(3) {
            out.extend_from_slice(&[px[2], px[1], px[0]]);
        }
        out.resize(out.len() + stride - row_bytes, 0);
    }
    Ok(out)
}

/// Where finished images go: files on disk, or memory for tests.
enum Destination {
    Files,
    #[cfg(test)]
    Memory(Vec<(String, Vec<u8>)>),
}

pub struct Image2Muxer {
    path: String,
    pattern: bool,
    kind: ImageKind,
    destination: Destination,
    /// Picture layout, known once the header is written.
    picture: Option<(u32, u32, PixelFormat)>,
    frames: u64,
    bytes: u64,
}

impl Image2Muxer {
    fn new(path: &str, destination: Destination) -> Result<Self> {
        let kind = ImageKind::from_path(path)?;
        let pattern = is_pattern(path);
        if pattern {
            // Surface a bad pattern now rather than after decoding starts.
            expand_pattern(path, FIRST_NUMBER)?;
        }
        Ok(Image2Muxer {
            path: path.to_string(),
            pattern,
            kind,
            destination,
            picture: None,
            frames: 0,
            bytes: 0,
        })
    }

    fn file_name(&self) -> Result<String> {
        if self.pattern {
            expand_pattern(&self.path, FIRST_NUMBER + self.frames)
        } else {
            Ok(self.path.clone())
        }
    }
}

impl Muxer for Image2Muxer {
    fn write_header(&mut self, streams: &[StreamInfo]) -> Result<()> {
        let [stream] = streams else {
            return Err(Error::unsupported(
                "an image sequence holds exactly one video stream",
            ));
        };
        if stream.codec != CodecId::RawVideo {
            return Err(Error::unsupported(format!(
                "{} in an image sequence",
                stream.codec
            )));
        }
        let v = stream
            .video()
            .ok_or_else(|| Error::unsupported("an image sequence holds video only"))?;
        if !matches!(v.pixel_format, PixelFormat::Rgb24 | PixelFormat::Gray) {
            return Err(Error::unsupported(format!(
                "image files are written from rgb24 or gray pictures, not {}",
                v.pixel_format
            )));
        }
        if v.width == 0 || v.height == 0 {
            return Err(Error::invalid_argument("image size is zero"));
        }
        if self.kind == ImageKind::Bmp
            && (v.width > i32::MAX as u32
                || bmp_stride(v.width) * v.height as u64 + 54 > u32::MAX as u64)
        {
            return Err(Error::unsupported(format!(
                "a {}x{} picture does not fit in a BMP file",
                v.width, v.height
            )));
        }
        self.picture = Some((v.width, v.height, v.pixel_format));
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        let (width, height, format) = self
            .picture
            .ok_or_else(|| Error::invalid_argument("image packet before the header"))?;
        if !self.pattern && self.frames >= 1 {
            return Err(Error::invalid_argument(format!(
                "'{}' names a single image but more frames follow; use a numbered \
                 pattern such as 'frame%03d.{}' for a sequence, or -frames:v 1 to \
                 keep only the first frame",
                self.path,
                match self.kind {
                    ImageKind::Ppm => "ppm",
                    ImageKind::Pgm => "pgm",
                    ImageKind::Bmp => "bmp",
                }
            )));
        }
        let expected = format.frame_size(width, height);
        if packet.data.len() != expected {
            return Err(Error::invalid_data(format!(
                "image packet of {} bytes, a {width}x{height} {format} picture needs {expected}",
                packet.data.len()
            )));
        }
        let image = self.kind.encode(format, width, height, &packet.data)?;
        let name = self.file_name()?;
        match &mut self.destination {
            Destination::Files => {
                let mut out = Output::create(&name)?;
                out.write_all(&image)?;
                out.flush()?;
            }
            #[cfg(test)]
            Destination::Memory(files) => files.push((name, image.clone())),
        }
        self.bytes += image.len() as u64;
        self.frames += 1;
        Ok(())
    }

    fn write_trailer(&mut self, _streams: &[StreamInfo]) -> Result<()> {
        // Every file is complete as soon as its frame is written.
        Ok(())
    }

    fn bytes_written(&self) -> u64 {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rational::Rational;
    use crate::stream::{StreamParams, VideoParams};

    fn stream(width: u32, height: u32, format: PixelFormat) -> StreamInfo {
        StreamInfo::new(
            0,
            CodecId::RawVideo,
            StreamParams::Video(VideoParams {
                width,
                height,
                pixel_format: format,
                frame_rate: Rational::new(25, 1),
                sample_aspect_ratio: Rational::new(1, 1),
            }),
            Rational::new(1, 25),
        )
    }

    /// Runs the muxer in memory and returns the files it would write.
    fn write(path: &str, info: &StreamInfo, frames: &[Vec<u8>]) -> Result<Vec<(String, Vec<u8>)>> {
        let mut m = Image2Muxer::new(path, Destination::Memory(Vec::new()))?;
        m.write_header(std::slice::from_ref(info))?;
        for (i, f) in frames.iter().enumerate() {
            m.write_packet(&Packet::new(0, f.clone(), i as i64, 1))?;
        }
        m.write_trailer(std::slice::from_ref(info))?;
        let total: usize = match &m.destination {
            Destination::Memory(files) => files.iter().map(|(_, b)| b.len()).sum(),
            Destination::Files => unreachable!(),
        };
        assert_eq!(m.bytes_written(), total as u64);
        match m.destination {
            Destination::Memory(files) => Ok(files),
            Destination::Files => unreachable!(),
        }
    }

    #[test]
    fn recognizes_patterns() {
        assert!(is_pattern("out%d.ppm"));
        assert!(is_pattern("dir/out%03d.bmp"));
        assert!(is_pattern("a%%b%4d.pgm"));
        assert!(!is_pattern("out.ppm"));
        assert!(!is_pattern("100%%d.ppm"));
        assert!(!is_pattern("out%s.ppm"));
        assert!(!is_pattern("out%"));
    }

    #[test]
    fn expands_patterns_like_printf() {
        assert_eq!(expand_pattern("f%d.ppm", 7).unwrap(), "f7.ppm");
        assert_eq!(expand_pattern("f%03d.ppm", 7).unwrap(), "f007.ppm");
        assert_eq!(expand_pattern("f%03d.ppm", 12345).unwrap(), "f12345.ppm");
        assert_eq!(expand_pattern("f%3d.ppm", 7).unwrap(), "f  7.ppm");
        assert_eq!(expand_pattern("100%%_%02d.ppm", 1).unwrap(), "100%_01.ppm");
        assert_eq!(expand_pattern("a%sb%d", 2).unwrap(), "a%sb2");
        assert_eq!(expand_pattern("x%5", 2).unwrap(), "x%5");
        assert_eq!(expand_pattern("été%d.ppm", 2).unwrap(), "été2.ppm");
        assert!(expand_pattern("f%033d.ppm", 1).is_err());
        assert!(expand_pattern("f%99999999999999999999d.ppm", 1).is_err());
    }

    #[test]
    fn numbers_files_from_one() {
        let info = stream(1, 1, PixelFormat::Gray);
        let files = write("out%02d.pgm", &info, &[vec![1], vec![2], vec![3]]).unwrap();
        let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["out01.pgm", "out02.pgm", "out03.pgm"]);
        assert_eq!(files[2].1, b"P5\n1 1\n255\n\x03");
    }

    #[test]
    fn writes_ppm_and_pgm() {
        let rgb = vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];
        let files = write(
            "a.ppm",
            &stream(2, 2, PixelFormat::Rgb24),
            std::slice::from_ref(&rgb),
        )
        .unwrap();
        let mut expected = b"P6\n2 2\n255\n".to_vec();
        expected.extend_from_slice(&rgb);
        assert_eq!(files, [("a.ppm".to_string(), expected)]);

        let files = write("a.PGM", &stream(2, 2, PixelFormat::Rgb24), &[rgb]).unwrap();
        // BT.601: red 76, green 150, blue 29, white 255.
        assert_eq!(files[0].1, b"P5\n2 2\n255\n\x4c\x96\x1d\xff");

        let gray = vec![0, 128, 255];
        let files = write("g.ppm", &stream(3, 1, PixelFormat::Gray), &[gray]).unwrap();
        assert_eq!(files[0].1, b"P6\n3 1\n255\n\0\0\0\x80\x80\x80\xff\xff\xff");
    }

    #[test]
    fn writes_bmp_bottom_up_with_padding() {
        // 2x2: top row red, green; bottom row blue, white. Rows of 6 bytes
        // are padded to 8.
        let rgb = vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];
        let files = write("a.bmp", &stream(2, 2, PixelFormat::Rgb24), &[rgb]).unwrap();
        let b = &files[0].1;
        assert_eq!(b.len(), 54 + 16);
        assert_eq!(&b[0..2], b"BM");
        let u32_at = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        let u16_at = |i: usize| u16::from_le_bytes(b[i..i + 2].try_into().unwrap());
        assert_eq!(u32_at(2), 70);
        assert_eq!(u32_at(10), 54);
        assert_eq!(u32_at(14), 40);
        assert_eq!((u32_at(18), u32_at(22)), (2, 2));
        assert_eq!((u16_at(26), u16_at(28)), (1, 24));
        assert_eq!(u32_at(30), 0);
        assert_eq!(u32_at(34), 16);
        assert_eq!(
            &b[54..],
            [255, 0, 0, 255, 255, 255, 0, 0, 0, 0, 255, 0, 255, 0, 0, 0]
        );

        // Gray 3x1: 9 bytes padded to 12, each value repeated.
        let files = write("g.bmp", &stream(3, 1, PixelFormat::Gray), &[vec![1, 2, 3]]).unwrap();
        assert_eq!(&files[0].1[54..], [1, 1, 1, 2, 2, 2, 3, 3, 3, 0, 0, 0]);
        // Width 4 needs no padding.
        assert_eq!(bmp_stride(4), 12);
        assert_eq!(bmp_stride(1), 4);
    }

    #[test]
    fn single_file_takes_one_frame() {
        let info = stream(1, 1, PixelFormat::Gray);
        assert_eq!(write("one.pgm", &info, &[vec![9]]).unwrap().len(), 1);
        let err = write("one.pgm", &info, &[vec![9], vec![8]]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("%03d") && msg.contains("-frames:v 1"), "{msg}");
        // Without a conversion, the percent signs stay as written.
        let files = write("100%%.pgm", &info, &[vec![9]]).unwrap();
        assert_eq!(files[0].0, "100%%.pgm");
    }

    #[test]
    fn rejects_unknown_extensions_and_formats() {
        assert!(Image2Muxer::new("out%d.png", Destination::Files).is_err());
        assert!(Image2Muxer::new("out", Destination::Files).is_err());
        assert!(Image2Muxer::new("-", Destination::Files).is_err());
        assert!(Image2Muxer::new("out%040d.ppm", Destination::Files).is_err());
        let yuv = stream(2, 2, PixelFormat::Yuv420p);
        assert!(matches!(
            write("a%d.ppm", &yuv, &[]),
            Err(Error::Unsupported(_))
        ));
        let gray = stream(2, 2, PixelFormat::Gray);
        let mut m = Image2Muxer::new("a%d.ppm", Destination::Memory(Vec::new())).unwrap();
        assert!(m.write_header(&[gray.clone(), gray.clone()]).is_err());
        assert!(m.write_packet(&Packet::new(0, vec![0; 4], 0, 1)).is_err());
        m.write_header(std::slice::from_ref(&gray)).unwrap();
        assert!(m.write_packet(&Packet::new(0, vec![0; 3], 0, 1)).is_err());
    }

    #[test]
    fn rejects_bmp_too_large() {
        let huge = stream(100_000, 100_000, PixelFormat::Rgb24);
        let mut m = Image2Muxer::new("a%d.bmp", Destination::Memory(Vec::new())).unwrap();
        assert!(m.write_header(&[huge]).is_err());
    }

    #[test]
    fn luma_rounds_to_nearest() {
        assert_eq!(luma(&[0, 0, 0]), 0);
        assert_eq!(luma(&[255, 255, 255]), 255);
        assert_eq!(luma(&[100, 100, 100]), 100);
        assert_eq!(luma(&[255, 0, 0]), 76);
        assert_eq!(luma(&[0, 255, 0]), 150);
        assert_eq!(luma(&[0, 0, 255]), 29);
    }
}
