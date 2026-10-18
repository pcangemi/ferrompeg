//! Headerless raw video (`-f rawvideo -s WxH -pix_fmt fmt -r rate`).
//!
//! The file is nothing but pictures back to back, each laid out as the
//! rawvideo codec stores it, so the size, pixel format and rate must come
//! from the command line. Pixel format and rate have the usual defaults
//! (yuv420p, 25 frames per second); the size has none.

use std::io::{Read, Write};

use crate::codec::rawvideo;
use crate::error::{Error, Result};
use crate::format::{
    Demuxer, InputFormat, InputOptions, Muxer, OutputFormat, OutputOptions, probe_never,
};
use crate::frame::PixelFormat;
use crate::io::Output;
use crate::rational::Rational;
use crate::stream::{CodecId, Packet, StreamInfo, StreamParams, VideoParams};

/// Largest width or height accepted from `-s`, which bounds the picture
/// size and keeps the size arithmetic far from overflow.
const MAX_DIMENSION: u32 = 32768;

pub const INPUT: InputFormat = InputFormat {
    name: "rawvideo",
    long_name: "raw video",
    extensions: &["yuv", "rgb"],
    probe: probe_never,
    open,
    raw_codec: None,
};

pub const OUTPUT: OutputFormat = OutputFormat {
    name: "rawvideo",
    long_name: "raw video",
    extensions: &["yuv", "rgb"],
    audio_codec: None,
    video_codec: Some(CodecId::RawVideo),
    codecs: &[CodecId::RawVideo],
    pixel_formats: &[],
    create,
};

fn open(reader: Box<dyn Read>, options: &InputOptions) -> Result<Box<dyn Demuxer>> {
    Ok(Box::new(RawVideoDemuxer::new(reader, options)?))
}

fn create(path: &str, _options: &OutputOptions) -> Result<Box<dyn Muxer>> {
    Ok(Box::new(RawVideoMuxer::new(Output::create(path)?)))
}

pub struct RawVideoDemuxer {
    reader: Box<dyn Read>,
    streams: Vec<StreamInfo>,
    frame_size: usize,
    next_pts: i64,
    done: bool,
}

impl RawVideoDemuxer {
    pub fn new(reader: Box<dyn Read>, options: &InputOptions) -> Result<Self> {
        let (width, height) = options.video_size.ok_or_else(|| {
            Error::invalid_argument("raw video input needs its picture size (use -s WxH)")
        })?;
        if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
            return Err(Error::invalid_argument(format!(
                "raw video size {width}x{height} is out of range (1 to {MAX_DIMENSION} per side)"
            )));
        }
        let pixel_format = options.pixel_format.unwrap_or(PixelFormat::Yuv420p);
        let rate = options.frame_rate.unwrap_or(Rational::new(25, 1));
        if rate.num <= 0 || rate.den <= 0 {
            return Err(Error::invalid_argument(format!(
                "raw video frame rate must be positive, got {rate}"
            )));
        }
        let params = VideoParams {
            width,
            height,
            pixel_format,
            frame_rate: rate,
            // Nothing in the file says otherwise.
            sample_aspect_ratio: Rational::new(1, 1),
        };
        let mut stream = StreamInfo::new(
            0,
            CodecId::RawVideo,
            StreamParams::Video(params),
            rate.inv(),
        );
        stream.bit_rate = rawvideo::bit_rate(pixel_format, width, height, rate);
        Ok(RawVideoDemuxer {
            reader,
            streams: vec![stream],
            frame_size: pixel_format.frame_size(width, height),
            next_pts: 0,
            done: false,
        })
    }
}

impl Demuxer for RawVideoDemuxer {
    fn format_name(&self) -> &'static str {
        "rawvideo"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn read_packet(&mut self) -> Result<Option<Packet>> {
        if self.done {
            return Ok(None);
        }
        // `take` + `read_to_end` grows the buffer with the data actually
        // read instead of reserving a whole picture up front.
        let mut data = Vec::new();
        (&mut self.reader)
            .take(self.frame_size as u64)
            .read_to_end(&mut data)?;
        if data.len() < self.frame_size {
            // End of input; a partial picture at the end is dropped, like
            // a partial block at the end of a WAV file.
            self.done = true;
            return Ok(None);
        }
        let packet = Packet::new(0, data, self.next_pts, 1);
        self.next_pts += 1;
        Ok(Some(packet))
    }
}

pub struct RawVideoMuxer {
    out: Output,
}

impl RawVideoMuxer {
    pub fn new(out: Output) -> Self {
        RawVideoMuxer { out }
    }

    pub fn into_output(self) -> Output {
        self.out
    }
}

impl Muxer for RawVideoMuxer {
    fn write_header(&mut self, streams: &[StreamInfo]) -> Result<()> {
        let [stream] = streams else {
            return Err(Error::unsupported(
                "raw video output holds exactly one stream",
            ));
        };
        if stream.codec != CodecId::RawVideo {
            return Err(Error::unsupported(format!(
                "{} in a raw video file",
                stream.codec
            )));
        }
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
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

    fn options(size: Option<(u32, u32)>) -> InputOptions {
        InputOptions {
            video_size: size,
            ..InputOptions::default()
        }
    }

    fn demux(bytes: Vec<u8>, options: &InputOptions) -> Result<RawVideoDemuxer> {
        RawVideoDemuxer::new(Box::new(Cursor::new(bytes)), options)
    }

    #[test]
    fn defaults_to_yuv420p_at_25() {
        let d = demux(Vec::new(), &options(Some((4, 2)))).unwrap();
        let info = &d.streams()[0];
        let v = info.video().unwrap();
        assert_eq!(v.pixel_format, PixelFormat::Yuv420p);
        assert_eq!(v.frame_rate, Rational::new(25, 1));
        assert_eq!(info.time_base, Rational::new(1, 25));
        assert_eq!(info.codec, CodecId::RawVideo);
        assert_eq!(info.duration, None);
        // 4x2 yuv420p is 12 bytes a picture.
        assert_eq!(info.bit_rate, Some(12 * 8 * 25));
    }

    #[test]
    fn splits_pictures_and_drops_a_partial_one() {
        let opts = InputOptions {
            video_size: Some((3, 1)),
            pixel_format: Some(PixelFormat::Rgb24),
            frame_rate: Some(Rational::new(30000, 1001)),
            ..InputOptions::default()
        };
        let bytes: Vec<u8> = (0..9 * 3 + 4).collect();
        let mut d = demux(bytes.clone(), &opts).unwrap();
        assert_eq!(d.streams()[0].time_base, Rational::new(1001, 30000));
        let mut packets = Vec::new();
        while let Some(p) = d.read_packet().unwrap() {
            packets.push(p);
        }
        assert_eq!(packets.len(), 3);
        for (i, p) in packets.iter().enumerate() {
            assert_eq!(p.pts, i as i64);
            assert_eq!(p.duration, 1);
            assert_eq!(p.data, &bytes[i * 9..(i + 1) * 9]);
        }
        assert!(d.read_packet().unwrap().is_none());
    }

    #[test]
    fn requires_a_sane_size_and_rate() {
        let err = demux(Vec::new(), &options(None)).err().unwrap();
        assert!(matches!(err, Error::InvalidArgument(_)));
        assert!(err.to_string().contains("-s"));
        assert!(demux(Vec::new(), &options(Some((0, 4)))).is_err());
        assert!(demux(Vec::new(), &options(Some((4, MAX_DIMENSION + 1)))).is_err());
        let zero_rate = InputOptions {
            video_size: Some((2, 2)),
            frame_rate: Some(Rational::new(0, 1)),
            ..InputOptions::default()
        };
        assert!(demux(Vec::new(), &zero_rate).is_err());
    }

    #[test]
    fn huge_size_does_not_allocate_without_data() {
        let size = Some((MAX_DIMENSION, MAX_DIMENSION));
        let mut d = demux(vec![1, 2, 3], &options(size)).unwrap();
        assert!(d.read_packet().unwrap().is_none());
    }

    #[test]
    fn muxer_writes_packets_verbatim() {
        let opts = options(Some((2, 2)));
        let d = demux(Vec::new(), &opts).unwrap();
        let info = d.streams()[0].clone();
        let mut m = RawVideoMuxer::new(Output::memory());
        m.write_header(std::slice::from_ref(&info)).unwrap();
        m.write_packet(&Packet::new(0, vec![1, 2, 3, 4, 5, 6], 0, 1))
            .unwrap();
        m.write_packet(&Packet::new(0, vec![7; 6], 1, 1)).unwrap();
        m.write_trailer(std::slice::from_ref(&info)).unwrap();
        assert_eq!(m.bytes_written(), 12);
        let bytes = m.into_output().into_bytes().unwrap();
        assert_eq!(bytes, [1, 2, 3, 4, 5, 6, 7, 7, 7, 7, 7, 7]);
    }

    #[test]
    fn muxer_rejects_other_streams() {
        let d = demux(Vec::new(), &options(Some((2, 2)))).unwrap();
        let info = d.streams()[0].clone();
        let mut m = RawVideoMuxer::new(Output::memory());
        assert!(m.write_header(&[info.clone(), info.clone()]).is_err());
        let mut gif = info;
        gif.codec = CodecId::Gif;
        assert!(m.write_header(&[gif]).is_err());
    }
}
