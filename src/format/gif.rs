//! Animated GIF files (GIF89a).
//!
//! Written against the "Graphics Interchange Format, Version 89a"
//! specification (CompuServe Incorporated, 1990): header, logical screen
//! descriptor, global color table and trailer (sections 17 to 19 and 27),
//! plus the NETSCAPE2.0 application extension (section 26 defines
//! application extensions; the NETSCAPE2.0 block is the long standing
//! convention viewers read for the loop count).
//!
//! The file layout is:
//!
//! ```text
//! "GIF89a" | logical screen descriptor | global color table
//!          | [NETSCAPE2.0 loop block] | images... | 0x3B
//! ```
//!
//! Every packet is a complete image with its own local color table. The
//! header waits for the first image: its local table becomes the global
//! color table and is removed from the image, and any later image whose
//! local table is identical to it loses its table too, since an image
//! without a local table uses the global one. The GIF encoder reuses the
//! first table whenever it suits a picture, so this saves up to 768 bytes
//! per image.
//!
//! Looping follows [`OutputOptions::loop_count`]: 0 (the default) loops
//! forever, -1 omits the loop block so viewers play the animation once, and
//! a positive `n` stores `n` repeats in the block.

use std::io::Write;
use std::ops::Range;

use crate::error::{Error, Result};
use crate::format::{Muxer, OutputFormat, OutputOptions};
use crate::frame::PixelFormat;
use crate::io::Output;
use crate::stream::{CodecId, Packet, StreamInfo};

pub const OUTPUT: OutputFormat = OutputFormat {
    name: "gif",
    long_name: "CompuServe Graphics Interchange Format (GIF)",
    extensions: &["gif"],
    audio_codec: None,
    video_codec: Some(CodecId::Gif),
    codecs: &[CodecId::Gif],
    pixel_formats: &[PixelFormat::Rgb24],
    create,
};

/// Canvas dimensions are 16-bit fields.
const MAX_DIMENSION: u32 = 65535;

/// Block introducers from the specification.
const EXTENSION: u8 = 0x21;
const IMAGE_SEPARATOR: u8 = 0x2C;
const TRAILER: u8 = 0x3B;

/// Bits of the image descriptor packed field.
const LOCAL_TABLE_FLAG: u8 = 0x80;
const SORT_FLAG: u8 = 0x20;
const TABLE_SIZE_MASK: u8 = 0x07;

fn create(path: &str, options: &OutputOptions) -> Result<Box<dyn Muxer>> {
    // Check the option before creating (and truncating) the file.
    let loop_count = loop_field(options.loop_count)?;
    Ok(Box::new(GifMuxer::with_loop(
        Output::create(path)?,
        loop_count,
    )))
}

/// Turns the user's loop count into the value of the NETSCAPE2.0 field,
/// `None` meaning no loop block at all.
fn loop_field(loop_count: Option<i32>) -> Result<Option<u16>> {
    match loop_count.unwrap_or(0) {
        -1 => Ok(None),
        n if (0..=u16::MAX as i32).contains(&n) => Ok(Some(n as u16)),
        n => Err(Error::invalid_argument(format!(
            "GIF loop count must be -1 (play once), 0 (forever) or up to 65535 repeats, not {n}"
        ))),
    }
}

pub struct GifMuxer {
    out: Output,
    loop_count: Option<u16>,
    /// Canvas size, known once `write_header` accepted the stream.
    canvas: Option<(u16, u16)>,
    /// Whether the file header went out (it waits for the first image).
    header_written: bool,
    /// Global color table bytes, empty when the file has none.
    global: Vec<u8>,
}

impl GifMuxer {
    /// A muxer that loops forever.
    pub fn new(out: Output) -> Self {
        GifMuxer::with_loop(out, Some(0))
    }

    /// `loop_count` is the NETSCAPE2.0 field (0 forever, `n` repeats), or
    /// `None` to play once.
    pub fn with_loop(out: Output, loop_count: Option<u16>) -> Self {
        GifMuxer {
            out,
            loop_count,
            canvas: None,
            header_written: false,
            global: Vec::new(),
        }
    }

    pub fn into_output(self) -> Output {
        self.out
    }

    /// Writes the header, with `global` (a whole color table, its length
    /// three times a power of two from 2 to 256) as the global color table
    /// when not empty.
    fn write_file_header(&mut self, global: &[u8]) -> Result<()> {
        let (width, height) = self
            .canvas
            .ok_or_else(|| Error::invalid_argument("GIF data written before the header"))?;
        let mut head = Vec::with_capacity(32 + global.len());
        head.extend_from_slice(b"GIF89a");
        // Logical screen descriptor: color resolution of 8 bits per primary
        // (field value 7), global table flag and size, background index 0,
        // no pixel aspect ratio information.
        head.extend_from_slice(&width.to_le_bytes());
        head.extend_from_slice(&height.to_le_bytes());
        let packed = match table_size_bits(global.len()) {
            Some(bits) => 0x80 | 0x70 | bits,
            None => 0x70,
        };
        head.extend_from_slice(&[packed, 0x00, 0x00]);
        head.extend_from_slice(global);
        if let Some(count) = self.loop_count {
            // Application extension: identifier "NETSCAPE", authentication
            // code "2.0", then one sub-block holding sub-block id 1 and the
            // 16-bit loop count.
            head.extend_from_slice(&[EXTENSION, 0xFF, 0x0B]);
            head.extend_from_slice(b"NETSCAPE2.0");
            head.extend_from_slice(&[0x03, 0x01]);
            head.extend_from_slice(&count.to_le_bytes());
            head.push(0x00);
        }
        self.out.write_all(&head)?;
        self.global = global.to_vec();
        self.header_written = true;
        Ok(())
    }
}

/// Size field of a color table of `bytes` bytes, `None` for no table.
fn table_size_bits(bytes: usize) -> Option<u8> {
    (0..8u8).find(|&n| 3 * (2usize << n) == bytes)
}

/// Where the image of a packet keeps its descriptor and local color table.
struct ImageLayout {
    /// Offset of the image descriptor's packed field.
    packed: usize,
    /// Bytes of the local color table, empty when there is none.
    table: Range<usize>,
}

/// Walks the blocks of a packet up to the first image descriptor. Returns
/// `None` for packets holding only extensions.
fn image_layout(data: &[u8]) -> Result<Option<ImageLayout>> {
    let truncated = || Error::invalid_data("truncated GIF packet");
    let mut pos = 0;
    while pos < data.len() {
        match data[pos] {
            EXTENSION => {
                // Introducer, label, then data sub-blocks up to an empty one.
                pos += 2;
                loop {
                    let len = *data.get(pos).ok_or_else(truncated)? as usize;
                    pos += 1 + len;
                    if len == 0 {
                        break;
                    }
                }
            }
            IMAGE_SEPARATOR => {
                let packed = pos + 9;
                let flags = *data.get(packed).ok_or_else(truncated)?;
                let start = packed + 1;
                let len = if flags & LOCAL_TABLE_FLAG != 0 {
                    3 * (2usize << (flags & TABLE_SIZE_MASK))
                } else {
                    0
                };
                if start + len > data.len() {
                    return Err(truncated());
                }
                return Ok(Some(ImageLayout {
                    packed,
                    table: start..start + len,
                }));
            }
            b => {
                return Err(Error::invalid_data(format!(
                    "unexpected block 0x{b:02X} in a GIF packet"
                )));
            }
        }
    }
    Ok(None)
}

impl Muxer for GifMuxer {
    fn write_header(&mut self, streams: &[StreamInfo]) -> Result<()> {
        let [stream] = streams else {
            return Err(Error::unsupported("GIF holds exactly one video stream"));
        };
        if stream.codec != CodecId::Gif {
            return Err(Error::unsupported(format!(
                "GIF holds gif pictures, not {}",
                stream.codec
            )));
        }
        let v = stream
            .video()
            .ok_or_else(|| Error::unsupported("GIF holds video only"))?;
        if !(1..=MAX_DIMENSION).contains(&v.width) || !(1..=MAX_DIMENSION).contains(&v.height) {
            return Err(Error::unsupported(format!(
                "GIF canvas must be 1 to {MAX_DIMENSION} pixels wide and high, not {}x{}",
                v.width, v.height
            )));
        }
        self.canvas = Some((v.width as u16, v.height as u16));
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        if self.canvas.is_none() {
            return Err(Error::invalid_argument(
                "GIF packet written before the header",
            ));
        }
        if packet.stream_index != 0 {
            return Err(Error::invalid_argument(format!(
                "GIF has one stream, got a packet for stream {}",
                packet.stream_index
            )));
        }
        let data = &packet.data;
        if data.is_empty() {
            return Err(Error::invalid_data("empty GIF packet"));
        }
        let layout = image_layout(data)?;
        if !self.header_written {
            // The first image's local table becomes the global one.
            let table = layout.as_ref().map_or(&[][..], |l| &data[l.table.clone()]);
            self.write_file_header(table)?;
        }
        match layout {
            Some(l) if !l.table.is_empty() && data[l.table.clone()] == self.global[..] => {
                // Same colors as the global table: point the image at it.
                self.out.write_all(&data[..l.packed])?;
                let flags = data[l.packed] & !(LOCAL_TABLE_FLAG | SORT_FLAG | TABLE_SIZE_MASK);
                self.out.write_all(&[flags])?;
                self.out.write_all(&data[l.table.end..])?;
            }
            _ => self.out.write_all(data)?,
        }
        Ok(())
    }

    fn write_trailer(&mut self, _streams: &[StreamInfo]) -> Result<()> {
        if !self.header_written {
            // No image at all: still a valid, empty file.
            self.write_file_header(&[])?;
        }
        self.out.write_all(&[TRAILER])?;
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
    use crate::codec::gif::testing::{composite, read_file};
    use crate::codec::{self, EncoderOptions};
    use crate::frame::{Frame, FrameFormat, VideoFormat, VideoFrame};
    use crate::rational::Rational;
    use crate::stream::{AudioParams, StreamParams, VideoParams};

    fn video_stream(codec: CodecId, width: u32, height: u32) -> StreamInfo {
        StreamInfo::new(
            0,
            codec,
            StreamParams::Video(VideoParams {
                width,
                height,
                pixel_format: PixelFormat::Rgb24,
                frame_rate: Rational::new(10, 1),
                sample_aspect_ratio: Rational::new(1, 1),
            }),
            Rational::new(1, 10),
        )
    }

    fn header_bytes(loop_count: Option<u16>) -> Vec<u8> {
        let mut m = GifMuxer::with_loop(Output::memory(), loop_count);
        let streams = [video_stream(CodecId::Gif, 320, 240)];
        m.write_header(&streams).unwrap();
        m.write_trailer(&streams).unwrap();
        m.into_output().into_bytes().unwrap()
    }

    #[test]
    fn header_layout() {
        let bytes = header_bytes(Some(0));
        let mut expected = b"GIF89a".to_vec();
        expected.extend_from_slice(&[0x40, 0x01, 0xF0, 0x00, 0x70, 0, 0]);
        expected.extend_from_slice(b"\x21\xFF\x0BNETSCAPE2.0\x03\x01\x00\x00\x00");
        expected.push(0x3B);
        assert_eq!(bytes, expected);
    }

    #[test]
    fn loop_count_option() {
        assert_eq!(loop_field(None).unwrap(), Some(0));
        assert_eq!(loop_field(Some(0)).unwrap(), Some(0));
        assert_eq!(loop_field(Some(-1)).unwrap(), None);
        assert_eq!(loop_field(Some(3)).unwrap(), Some(3));
        assert_eq!(loop_field(Some(65535)).unwrap(), Some(65535));
        assert!(loop_field(Some(-2)).is_err());
        assert!(loop_field(Some(65536)).is_err());

        let once = header_bytes(None);
        assert_eq!(once.len(), 6 + 7 + 1);
        assert!(!once.windows(8).any(|w| w == b"NETSCAPE"));
        let three = header_bytes(Some(3));
        assert_eq!(read_file(&three).loops, Some(3));
    }

    #[test]
    fn rejects_streams_that_are_not_one_gif_video() {
        let mut m = GifMuxer::new(Output::memory());
        assert!(m.write_header(&[]).is_err());
        let two = [
            video_stream(CodecId::Gif, 8, 8),
            video_stream(CodecId::Gif, 8, 8),
        ];
        assert!(m.write_header(&two).is_err());
        assert!(
            m.write_header(&[video_stream(CodecId::RawVideo, 8, 8)])
                .is_err()
        );
        assert!(m.write_header(&[video_stream(CodecId::Gif, 0, 8)]).is_err());
        assert!(
            m.write_header(&[video_stream(CodecId::Gif, 8, 70000)])
                .is_err()
        );
        let audio = StreamInfo::new(
            0,
            CodecId::Gif,
            StreamParams::Audio(AudioParams {
                sample_rate: 8000,
                channels: 1,
                bits_per_sample: 16,
            }),
            Rational::new(1, 8000),
        );
        assert!(m.write_header(&[audio]).is_err());
        // Nothing was written by the failed attempts.
        assert_eq!(m.bytes_written(), 0);
        assert!(m.write_packet(&Packet::new(0, vec![0x21], 0, 1)).is_err());
        assert!(m.write_trailer(&[]).is_err());
    }

    #[test]
    fn rejects_foreign_packets() {
        let mut m = GifMuxer::new(Output::memory());
        m.write_header(&[video_stream(CodecId::Gif, 8, 8)]).unwrap();
        assert!(m.write_packet(&Packet::new(0, vec![], 0, 1)).is_err());
        assert!(m.write_packet(&Packet::new(0, vec![0x3B], 0, 1)).is_err());
        assert!(m.write_packet(&Packet::new(1, vec![0x21], 0, 1)).is_err());
    }

    /// A minimal image packet: graphic control extension, a 1x1 image
    /// descriptor, a two entry local table and the LZW data of index 0.
    fn tiny_image(color: [u8; 3]) -> Vec<u8> {
        let mut p = vec![0x21, 0xF9, 0x04, 0x04, 0x0A, 0x00, 0x00, 0x00];
        p.extend_from_slice(&[0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0x80]);
        p.extend_from_slice(&color);
        p.extend_from_slice(&[0, 0, 0]);
        p.extend_from_slice(&[2, 2, 0x44, 0x01, 0]);
        p
    }

    #[test]
    fn local_tables_move_to_the_global_table() {
        let mut m = GifMuxer::with_loop(Output::memory(), None);
        let streams = [video_stream(CodecId::Gif, 1, 1)];
        m.write_header(&streams).unwrap();
        // Nothing is written until the first image brings its colors.
        assert_eq!(m.bytes_written(), 0);
        let red = tiny_image([255, 0, 0]);
        let blue = tiny_image([0, 0, 255]);
        for p in [&red, &blue, &red] {
            m.write_packet(&Packet::new(0, p.clone(), 0, 1)).unwrap();
        }
        // A packet holding only an extension passes through untouched.
        let comment = vec![0x21, 0xFE, 0x02, b'h', b'i', 0x00];
        m.write_packet(&Packet::new(0, comment.clone(), 0, 1))
            .unwrap();
        m.write_trailer(&streams).unwrap();
        let bytes = m.into_output().into_bytes().unwrap();

        let mut expected = b"GIF89a\x01\x00\x01\x00\xF0\x00\x00".to_vec();
        expected.extend_from_slice(&[255, 0, 0, 0, 0, 0]);
        let stripped = |p: &[u8]| {
            let mut v = p[..17].to_vec();
            v.push(0x00);
            v.extend_from_slice(&p[24..]);
            v
        };
        expected.extend(stripped(&red));
        expected.extend_from_slice(&blue);
        expected.extend(stripped(&red));
        expected.extend_from_slice(&comment);
        expected.push(0x3B);
        assert_eq!(bytes, expected);
    }

    #[test]
    fn rejects_malformed_packets() {
        let mut m = GifMuxer::new(Output::memory());
        m.write_header(&[video_stream(CodecId::Gif, 1, 1)]).unwrap();
        let image = tiny_image([1, 2, 3]);
        // Cut inside the descriptor, inside the table, and an extension
        // whose sub-blocks never end.
        for bad in [&image[..12], &image[..20], &[0x21, 0xF9, 0x04, 0, 0][..]] {
            assert!(m.write_packet(&Packet::new(0, bad.to_vec(), 0, 1)).is_err());
        }
        assert_eq!(m.bytes_written(), 0);
    }

    #[test]
    fn create_checks_the_loop_count_first() {
        let options = OutputOptions {
            loop_count: Some(-7),
            ..Default::default()
        };
        // The path is never touched because the option is rejected first.
        assert!(create("/nonexistent-dir/never.gif", &options).is_err());
    }

    #[test]
    fn encoder_and_muxer_make_a_playable_animation() {
        let (w, h) = (24u32, 16u32);
        let rate = Rational::new(30, 1);
        let proposed = FrameFormat::Video(VideoFormat {
            width: w,
            height: h,
            pixel_format: PixelFormat::Rgb24,
            frame_rate: rate,
        });
        let mut enc =
            codec::create_encoder(CodecId::Gif, &proposed, &EncoderOptions::default()).unwrap();
        let mut muxer = GifMuxer::with_loop(Output::memory(), Some(2));
        muxer.write_header(&[enc.stream_info()]).unwrap();

        // A square moving over a two color background: few colors, so the
        // composited result must match the source exactly.
        let pictures: Vec<Vec<u8>> = (0..8)
            .map(|i| {
                let mut p = Vec::with_capacity((w * h * 3) as usize);
                for y in 0..h {
                    for x in 0..w {
                        let inside = (i..i + 5).contains(&x) && (4..9).contains(&y);
                        p.extend_from_slice(match (inside, (x + y) % 2) {
                            (true, _) => &[250, 20, 20],
                            (false, 0) => &[0, 0, 80],
                            _ => &[0, 80, 0],
                        });
                    }
                }
                p
            })
            .collect();
        for (i, p) in pictures.iter().enumerate() {
            let frame = Frame::Video(VideoFrame {
                width: w,
                height: h,
                format: PixelFormat::Rgb24,
                planes: vec![p.clone()],
                pts: i as i64,
                time_base: rate.inv(),
            });
            for packet in enc.encode(&frame).unwrap() {
                muxer.write_packet(&packet).unwrap();
            }
        }
        for packet in enc.flush().unwrap() {
            muxer.write_packet(&packet).unwrap();
        }
        muxer.write_trailer(&[enc.stream_info()]).unwrap();
        let bytes = muxer.into_output().into_bytes().unwrap();

        let file = read_file(&bytes);
        assert_eq!((file.width, file.height), (24, 16));
        assert_eq!(file.loops, Some(2));
        assert_eq!(file.images.len(), pictures.len());
        let delays: Vec<u16> = file.images.iter().map(|i| i.delay).collect();
        assert_eq!(delays, vec![3, 4, 3, 3, 4, 3, 3, 4]);
        let mut canvas = vec![0u8; (w * h * 3) as usize];
        for (image, source) in file.images.iter().zip(&pictures) {
            composite(&mut canvas, w as usize, image);
            assert_eq!(&canvas, source);
        }
        // Moving a square one pixel only touches a narrow band.
        for image in &file.images[1..] {
            assert_eq!((image.width, image.height), (6, 5));
        }
        // Three colors: the first table (four entries) became the global
        // one, and every image reuses it, the free fourth entry serving as
        // the transparent index.
        assert_eq!(file.global.as_ref().map(Vec::len), Some(4));
        assert!(file.images.iter().all(|i| !i.local));
        assert!(file.images[1..].iter().all(|i| i.transparent == Some(3)));
    }
}
