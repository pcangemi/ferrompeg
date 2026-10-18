//! Uncompressed video: each packet is one picture, planes back to back in
//! the layout described by [`crate::frame::PixelFormat::plane_size`].
//!
//! There is no header and no per-picture metadata: the stream description
//! (size, pixel format, frame rate) comes from the container, so a packet
//! of the wrong size can only mean a broken or mislabeled input.

use crate::codec::{Decoder, Encoder, EncoderOptions};
use crate::error::{Error, Result};
use crate::frame::{Frame, FrameFormat, PixelFormat, VideoFormat, VideoFrame};
use crate::rational::Rational;
use crate::stream::{CodecId, Packet, StreamInfo, StreamParams, VideoParams};

/// Splits one tightly packed picture into its planes. `data` must be
/// exactly `format.frame_size(width, height)` bytes.
pub fn split_planes(format: PixelFormat, width: u32, height: u32, data: &[u8]) -> Vec<Vec<u8>> {
    let mut planes = Vec::with_capacity(format.plane_count());
    let mut pos = 0;
    for p in 0..format.plane_count() {
        let (row, rows) = format.plane_size(p, width, height);
        let len = row * rows;
        planes.push(data[pos..pos + len].to_vec());
        pos += len;
    }
    planes
}

/// Bit rate of uncompressed pictures at `rate` frames per second, `None`
/// when the rate is not a positive number.
pub fn bit_rate(format: PixelFormat, width: u32, height: u32, rate: Rational) -> Option<u64> {
    if rate.num <= 0 || rate.den <= 0 {
        return None;
    }
    let bits = format.frame_size(width, height) as u128 * 8 * rate.num as u128;
    u64::try_from(bits / rate.den as u128).ok()
}

struct RawVideoDecoder {
    width: u32,
    height: u32,
    format: PixelFormat,
    frame_size: usize,
    time_base: Rational,
}

pub fn new_decoder(stream: &StreamInfo) -> Result<Box<dyn Decoder>> {
    let v = stream
        .video()
        .ok_or_else(|| Error::invalid_argument("rawvideo decoder needs a video stream"))?;
    if v.width == 0 || v.height == 0 {
        return Err(Error::invalid_data("rawvideo stream with an empty picture"));
    }
    Ok(Box::new(RawVideoDecoder {
        width: v.width,
        height: v.height,
        format: v.pixel_format,
        frame_size: v.pixel_format.frame_size(v.width, v.height),
        time_base: stream.time_base,
    }))
}

impl Decoder for RawVideoDecoder {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<Frame>> {
        if packet.data.len() != self.frame_size {
            return Err(Error::invalid_data(format!(
                "rawvideo packet holds {} bytes, a {}x{} {} picture needs {}",
                packet.data.len(),
                self.width,
                self.height,
                self.format,
                self.frame_size
            )));
        }
        Ok(vec![Frame::Video(VideoFrame {
            width: self.width,
            height: self.height,
            format: self.format,
            planes: split_planes(self.format, self.width, self.height, &packet.data),
            pts: packet.pts,
            time_base: self.time_base,
        })])
    }
}

struct RawVideoEncoder {
    format: VideoFormat,
    stream_index: usize,
}

pub fn new_encoder(
    codec: CodecId,
    proposed: &FrameFormat,
    options: &EncoderOptions,
) -> Result<Box<dyn Encoder>> {
    let FrameFormat::Video(proposed) = proposed else {
        return Err(Error::invalid_argument(format!(
            "{codec} encodes video only"
        )));
    };
    if codec != CodecId::RawVideo {
        return Err(Error::invalid_argument(format!("{codec} is not rawvideo")));
    }
    // The time base is the inverse of the rate, which must therefore be a
    // positive fraction.
    if proposed.frame_rate.num <= 0 || proposed.frame_rate.den <= 0 {
        return Err(Error::invalid_argument(format!(
            "rawvideo needs a positive frame rate, got {}",
            proposed.frame_rate
        )));
    }
    if proposed.width == 0 || proposed.height == 0 {
        return Err(Error::invalid_argument(
            "rawvideo cannot encode an empty picture",
        ));
    }
    Ok(Box::new(RawVideoEncoder {
        format: *proposed,
        stream_index: options.stream_index,
    }))
}

impl Encoder for RawVideoEncoder {
    fn input_format(&self) -> FrameFormat {
        // Every pixel format is stored as is, so nothing needs converting.
        FrameFormat::Video(self.format)
    }

    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        let Frame::Video(frame) = frame else {
            return Err(Error::invalid_argument(
                "rawvideo encoder got an audio frame",
            ));
        };
        if frame.width != self.format.width
            || frame.height != self.format.height
            || frame.format != self.format.pixel_format
        {
            return Err(Error::invalid_argument(format!(
                "rawvideo encoder set up for {}x{} {} got a {}x{} {} frame",
                self.format.width,
                self.format.height,
                self.format.pixel_format,
                frame.width,
                frame.height,
                frame.format
            )));
        }
        frame.validate()?;
        let data = frame.planes.concat();
        Ok(vec![Packet::new(self.stream_index, data, frame.pts, 1)])
    }

    fn stream_info(&self) -> StreamInfo {
        let f = &self.format;
        let mut info = StreamInfo::new(
            self.stream_index,
            CodecId::RawVideo,
            StreamParams::Video(VideoParams {
                width: f.width,
                height: f.height,
                pixel_format: f.pixel_format,
                frame_rate: f.frame_rate,
                sample_aspect_ratio: Rational::new(1, 1),
            }),
            f.frame_rate.inv(),
        );
        info.bit_rate = bit_rate(f.pixel_format, f.width, f.height, f.frame_rate);
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn video_format(width: u32, height: u32, format: PixelFormat) -> FrameFormat {
        FrameFormat::Video(VideoFormat {
            width,
            height,
            pixel_format: format,
            frame_rate: Rational::new(30000, 1001),
        })
    }

    #[test]
    fn round_trips_every_pixel_format_and_odd_sizes() {
        for format in PixelFormat::ALL {
            for (w, h) in [(1, 1), (7, 5), (16, 8), (175, 143)] {
                let size = format.frame_size(w, h);
                let payload: Vec<u8> = (0..size).map(|i| (i * 7) as u8).collect();
                let mut dec = new_decoder(&stream(w, h, format)).unwrap();
                let frames = dec.decode(&Packet::new(0, payload.clone(), 3, 1)).unwrap();
                let [Frame::Video(v)] = frames.as_slice() else {
                    panic!("expected one video frame");
                };
                v.validate().unwrap();
                assert_eq!(v.pts, 3);
                assert_eq!(v.time_base, Rational::new(1, 25));

                let opts = EncoderOptions::default();
                let mut enc =
                    new_encoder(CodecId::RawVideo, &video_format(w, h, format), &opts).unwrap();
                let packets = enc.encode(&Frame::Video(v.clone())).unwrap();
                assert_eq!(packets.len(), 1);
                assert_eq!(packets[0].data, payload, "{format} {w}x{h}");
                assert_eq!(packets[0].pts, 3);
                assert_eq!(packets[0].duration, 1);
            }
        }
    }

    #[test]
    fn planes_are_split_in_order() {
        // 4x2 yuv420p: 8 luma bytes, then 2 Cb, then 2 Cr.
        let data: Vec<u8> = (0..12).collect();
        let planes = split_planes(PixelFormat::Yuv420p, 4, 2, &data);
        assert_eq!(
            planes,
            vec![(0..8).collect::<Vec<u8>>(), vec![8, 9], vec![10, 11]]
        );
    }

    #[test]
    fn decoder_rejects_wrong_sizes() {
        let mut dec = new_decoder(&stream(4, 4, PixelFormat::Yuv420p)).unwrap();
        assert!(dec.decode(&Packet::new(0, vec![0; 23], 0, 1)).is_err());
        assert!(dec.decode(&Packet::new(0, vec![0; 25], 0, 1)).is_err());
        assert!(dec.decode(&Packet::new(0, Vec::new(), 0, 1)).is_err());
        assert!(dec.decode(&Packet::new(0, vec![0; 24], 0, 1)).is_ok());
    }

    #[test]
    fn decoder_rejects_bad_streams() {
        assert!(new_decoder(&stream(0, 4, PixelFormat::Gray)).is_err());
        let audio = StreamInfo::new(
            0,
            CodecId::RawVideo,
            StreamParams::Audio(crate::stream::AudioParams {
                sample_rate: 8000,
                channels: 1,
                bits_per_sample: 16,
            }),
            Rational::new(1, 8000),
        );
        assert!(new_decoder(&audio).is_err());
    }

    #[test]
    fn encoder_keeps_the_proposed_format() {
        let proposed = video_format(9, 3, PixelFormat::Yuv422p);
        let enc = new_encoder(CodecId::RawVideo, &proposed, &EncoderOptions::default()).unwrap();
        assert_eq!(enc.input_format(), proposed);
        let info = enc.stream_info();
        let v = info.video().unwrap();
        assert_eq!((v.width, v.height), (9, 3));
        assert_eq!(v.pixel_format, PixelFormat::Yuv422p);
        assert_eq!(v.frame_rate, Rational::new(30000, 1001));
        assert_eq!(v.sample_aspect_ratio, Rational::new(1, 1));
        assert_eq!(info.time_base, Rational::new(1001, 30000));
        // 9x3 4:2:2 is 27 + 2 * 15 = 57 bytes a picture.
        assert_eq!(info.bit_rate, Some(57 * 8 * 30000 / 1001));
    }

    #[test]
    fn encoder_rejects_bad_input() {
        let opts = EncoderOptions::default();
        let zero_rate = FrameFormat::Video(VideoFormat {
            width: 4,
            height: 4,
            pixel_format: PixelFormat::Gray,
            frame_rate: Rational::new(0, 1),
        });
        assert!(new_encoder(CodecId::RawVideo, &zero_rate, &opts).is_err());
        let audio = FrameFormat::Audio(crate::frame::AudioFormat {
            sample_rate: 8000,
            channels: 1,
            kind: crate::frame::SampleKind::Int(16),
        });
        assert!(new_encoder(CodecId::RawVideo, &audio, &opts).is_err());

        let mut enc = new_encoder(
            CodecId::RawVideo,
            &video_format(4, 4, PixelFormat::Gray),
            &opts,
        )
        .unwrap();
        let other = VideoFrame::new_black(4, 4, PixelFormat::Rgb24, 0, Rational::new(1, 25));
        assert!(enc.encode(&Frame::Video(other)).is_err());
        let mut broken = VideoFrame::new_black(4, 4, PixelFormat::Gray, 0, Rational::new(1, 25));
        broken.planes[0].pop();
        assert!(enc.encode(&Frame::Video(broken)).is_err());
    }
}
