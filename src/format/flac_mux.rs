//! Native FLAC stream writer.
//!
//! Layout from RFC 9639, sections 6 and 8: the `fLaC` marker, a STREAMINFO
//! block, a VORBIS_COMMENT block with the tags, a PADDING block that leaves
//! room for tag editors to grow the metadata in place, then the frames
//! exactly as the encoder produced them.
//!
//! The encoder only knows the stream length, the frame sizes and the MD5
//! signature at the end, so on seekable outputs the trailer rewrites the
//! STREAMINFO body with the final values. On pipes it keeps the zeros,
//! which mean "unknown" and still make a valid stream.

use std::io::{Seek, SeekFrom, Write};

use crate::codec::flac::{STREAMINFO_SIZE, StreamInfoBlock};
use crate::error::{Error, Result};
use crate::format::{Muxer, OutputFormat, OutputOptions};
use crate::io::Output;
use crate::stream::{CodecId, Packet, StreamInfo};

pub const OUTPUT: OutputFormat = OutputFormat {
    name: "flac",
    long_name: "raw FLAC",
    extensions: &["flac"],
    audio_codec: Some(CodecId::Flac),
    video_codec: None,
    codecs: &[CodecId::Flac],
    pixel_formats: &[],
    create,
};

/// Metadata block types (RFC 9639, section 8.1).
const BLOCK_STREAMINFO: u8 = 0;
const BLOCK_PADDING: u8 = 1;
const BLOCK_VORBIS_COMMENT: u8 = 4;

/// Size of the PADDING block body.
const PADDING_SIZE: usize = 8192;

/// Name stored as the Vorbis comment vendor string.
const VENDOR: &str = "ferrompeg";

/// Offset of the STREAMINFO body: the 4-byte marker and the 4-byte block
/// header come first.
const STREAMINFO_OFFSET: u64 = 8;

/// Largest metadata block body, limited by its 24-bit length field.
const MAX_BLOCK_SIZE: usize = (1 << 24) - 1;

fn create(path: &str, options: &OutputOptions) -> Result<Box<dyn Muxer>> {
    Ok(Box::new(FlacMuxer::new(
        Output::create(path)?,
        options.metadata.clone(),
    )))
}

pub struct FlacMuxer {
    out: Output,
    tags: Vec<(String, String)>,
    header_written: bool,
}

impl FlacMuxer {
    pub fn new(out: Output, tags: Vec<(String, String)>) -> Self {
        FlacMuxer {
            out,
            tags,
            header_written: false,
        }
    }

    pub fn into_output(self) -> Output {
        self.out
    }
}

/// Checks that `streams` is a single FLAC audio stream and returns its
/// STREAMINFO body.
fn streaminfo_body(streams: &[StreamInfo]) -> Result<[u8; STREAMINFO_SIZE]> {
    let [stream] = streams else {
        return Err(Error::unsupported("FLAC holds exactly one audio stream"));
    };
    if stream.codec != CodecId::Flac || stream.audio().is_none() {
        return Err(Error::unsupported(format!(
            "{} in a FLAC file (only FLAC audio fits)",
            stream.codec
        )));
    }
    if stream.extradata.len() != STREAMINFO_SIZE {
        return Err(Error::invalid_data(format!(
            "FLAC stream setup is {} bytes, expected a {STREAMINFO_SIZE}-byte STREAMINFO",
            stream.extradata.len()
        )));
    }
    // Parsing rejects bodies a decoder would refuse (zero sample rate, too
    // few bits), so a broken setup fails here rather than in a player.
    StreamInfoBlock::parse(&stream.extradata)?;
    let mut body = [0u8; STREAMINFO_SIZE];
    body.copy_from_slice(&stream.extradata);
    Ok(body)
}

/// Metadata block header: last block flag, 7-bit type, 24-bit length.
fn block_header(last: bool, kind: u8, len: usize) -> Result<[u8; 4]> {
    if len > MAX_BLOCK_SIZE {
        return Err(Error::unsupported("FLAC metadata block larger than 16 MiB"));
    }
    let len = (len as u32).to_be_bytes();
    Ok([(last as u8) << 7 | kind, len[1], len[2], len[3]])
}

/// Builds a VORBIS_COMMENT body (RFC 9639, section 8.6). Unlike the rest
/// of FLAC its lengths are little-endian. Field names are case
/// insensitive, conventionally upper case, and limited to printable ASCII
/// without `=`; an empty value means "no such tag" on the command line, so
/// those entries are dropped.
fn vorbis_comment(tags: &[(String, String)]) -> Result<Vec<u8>> {
    let mut fields = Vec::new();
    for (key, value) in tags {
        if value.is_empty() {
            continue;
        }
        if key.is_empty() || !key.bytes().all(|b| (0x20..=0x7D).contains(&b) && b != b'=') {
            return Err(Error::invalid_argument(format!(
                "'{key}' is not a valid FLAC tag name (printable ASCII without '=')"
            )));
        }
        fields.push(format!("{}={value}", key.to_ascii_uppercase()));
    }
    let mut body = Vec::new();
    body.extend_from_slice(&(VENDOR.len() as u32).to_le_bytes());
    body.extend_from_slice(VENDOR.as_bytes());
    body.extend_from_slice(&(fields.len() as u32).to_le_bytes());
    for field in &fields {
        let len = u32::try_from(field.len())
            .map_err(|_| Error::invalid_argument("FLAC tag value too long"))?;
        body.extend_from_slice(&len.to_le_bytes());
        body.extend_from_slice(field.as_bytes());
    }
    Ok(body)
}

impl Muxer for FlacMuxer {
    fn write_header(&mut self, streams: &[StreamInfo]) -> Result<()> {
        let streaminfo = streaminfo_body(streams)?;
        let comment = vorbis_comment(&self.tags)?;
        let mut head = Vec::with_capacity(4 + 4 + STREAMINFO_SIZE + 4 + comment.len() + 4);
        head.extend_from_slice(b"fLaC");
        head.extend_from_slice(&block_header(false, BLOCK_STREAMINFO, STREAMINFO_SIZE)?);
        head.extend_from_slice(&streaminfo);
        head.extend_from_slice(&block_header(false, BLOCK_VORBIS_COMMENT, comment.len())?);
        head.extend_from_slice(&comment);
        head.extend_from_slice(&block_header(true, BLOCK_PADDING, PADDING_SIZE)?);
        self.out.write_all(&head)?;
        self.out.write_all(&[0u8; PADDING_SIZE])?;
        self.header_written = true;
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        if !self.header_written {
            return Err(Error::invalid_argument(
                "FLAC packet written before the header",
            ));
        }
        if packet.stream_index != 0 {
            return Err(Error::invalid_argument(format!(
                "FLAC file has one stream, got a packet for stream {}",
                packet.stream_index
            )));
        }
        self.out.write_all(&packet.data)?;
        Ok(())
    }

    fn write_trailer(&mut self, streams: &[StreamInfo]) -> Result<()> {
        let streaminfo = streaminfo_body(streams)?;
        if self.header_written && self.out.is_seekable() {
            self.out.seek(SeekFrom::Start(STREAMINFO_OFFSET))?;
            self.out.write_all(&streaminfo)?;
            self.out.seek(SeekFrom::End(0))?;
        }
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
    use crate::codec::{EncoderOptions, create_encoder};
    use crate::frame::{AudioData, AudioFormat, AudioFrame, Frame, FrameFormat, SampleKind};
    use crate::rational::Rational;
    use crate::stream::{AudioParams, StreamParams, VideoParams};

    fn flac_stream(info: &StreamInfoBlock) -> StreamInfo {
        let mut s = StreamInfo::new(
            0,
            CodecId::Flac,
            StreamParams::Audio(AudioParams {
                sample_rate: info.sample_rate,
                channels: info.channels as u16,
                bits_per_sample: info.bits_per_sample as u32,
            }),
            Rational::new(1, info.sample_rate as i64),
        );
        s.extradata = info.serialize().to_vec();
        s
    }

    fn placeholder() -> StreamInfoBlock {
        StreamInfoBlock {
            min_block_size: 4096,
            max_block_size: 4096,
            sample_rate: 44100,
            channels: 2,
            bits_per_sample: 16,
            ..StreamInfoBlock::default()
        }
    }

    fn tags(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// Splits the metadata section into (last flag, type, body) triples
    /// and returns them with the offset of the first frame byte.
    fn blocks(file: &[u8]) -> (Vec<(bool, u8, Vec<u8>)>, usize) {
        assert_eq!(&file[..4], b"fLaC");
        let mut pos = 4;
        let mut out = Vec::new();
        loop {
            let last = file[pos] & 0x80 != 0;
            let kind = file[pos] & 0x7F;
            let len = u32::from_be_bytes([0, file[pos + 1], file[pos + 2], file[pos + 3]]) as usize;
            out.push((last, kind, file[pos + 4..pos + 4 + len].to_vec()));
            pos += 4 + len;
            if last {
                return (out, pos);
            }
        }
    }

    fn parse_comment(body: &[u8]) -> (String, Vec<String>) {
        let u32_at = |i: usize| u32::from_le_bytes(body[i..i + 4].try_into().unwrap()) as usize;
        let vendor_len = u32_at(0);
        let vendor = String::from_utf8(body[4..4 + vendor_len].to_vec()).unwrap();
        let mut pos = 4 + vendor_len;
        let count = u32_at(pos);
        pos += 4;
        let mut fields = Vec::new();
        for _ in 0..count {
            let len = u32_at(pos);
            fields.push(String::from_utf8(body[pos + 4..pos + 4 + len].to_vec()).unwrap());
            pos += 4 + len;
        }
        assert_eq!(pos, body.len());
        (vendor, fields)
    }

    #[test]
    fn writes_the_metadata_layout() {
        let stream = flac_stream(&placeholder());
        let mut mux = FlacMuxer::new(
            Output::memory(),
            tags(&[("title", "Song"), ("Artist", "Somebody"), ("comment", "")]),
        );
        mux.write_header(std::slice::from_ref(&stream)).unwrap();
        mux.write_packet(&Packet::new(0, vec![0xFF, 0xF8, 1, 2], 0, 4096))
            .unwrap();
        mux.write_packet(&Packet::new(0, vec![0xFF, 0xF8, 3], 4096, 10))
            .unwrap();
        mux.write_trailer(std::slice::from_ref(&stream)).unwrap();
        let file = mux.into_output().into_bytes().unwrap();

        let (blocks, frames_at) = blocks(&file);
        assert_eq!(blocks.len(), 3);
        assert_eq!(&file[4..8], [0x00, 0x00, 0x00, 34]);
        assert_eq!((blocks[0].0, blocks[0].1), (false, BLOCK_STREAMINFO));
        assert_eq!(blocks[0].2, stream.extradata);
        assert_eq!((blocks[1].0, blocks[1].1), (false, BLOCK_VORBIS_COMMENT));
        let (vendor, fields) = parse_comment(&blocks[1].2);
        assert_eq!(vendor, "ferrompeg");
        assert_eq!(fields, ["TITLE=Song", "ARTIST=Somebody"]);
        assert_eq!((blocks[2].0, blocks[2].1), (true, BLOCK_PADDING));
        assert_eq!(blocks[2].2, vec![0u8; PADDING_SIZE]);
        assert_eq!(&file[frames_at..], [0xFF, 0xF8, 1, 2, 0xFF, 0xF8, 3]);
    }

    #[test]
    fn empty_tag_list_still_has_a_vendor() {
        let body = vorbis_comment(&[]).unwrap();
        let (vendor, fields) = parse_comment(&body);
        assert_eq!(vendor, VENDOR);
        assert!(fields.is_empty());
        // Values keep their bytes as given, including '=' and UTF-8, and
        // the whole printable range up to '}' is a valid name.
        let body = vorbis_comment(&tags(&[("x", "a=b é")])).unwrap();
        assert_eq!(parse_comment(&body).1, ["X=a=b é"]);
        let body = vorbis_comment(&tags(&[("{a b}", "v")])).unwrap();
        assert_eq!(parse_comment(&body).1, ["{A B}=v"]);
    }

    #[test]
    fn rejects_invalid_tag_names() {
        for key in ["", "a=b", "caf\u{e9}", "tab\there", "tilde~"] {
            let mut mux = FlacMuxer::new(Output::memory(), tags(&[(key, "v")]));
            assert!(
                mux.write_header(&[flac_stream(&placeholder())]).is_err(),
                "{key:?}"
            );
        }
    }

    #[test]
    fn trailer_rewrites_streaminfo() {
        let start = placeholder();
        let mut mux = FlacMuxer::new(Output::memory(), Vec::new());
        mux.write_header(&[flac_stream(&start)]).unwrap();
        mux.write_packet(&Packet::new(0, vec![9; 100], 0, 4096))
            .unwrap();
        let fin = StreamInfoBlock {
            min_frame_size: 100,
            max_frame_size: 100,
            total_samples: 4096,
            md5: [0xAB; 16],
            ..start
        };
        mux.write_trailer(&[flac_stream(&fin)]).unwrap();
        let file = mux.into_output().into_bytes().unwrap();
        let (blocks, frames_at) = blocks(&file);
        assert_eq!(StreamInfoBlock::parse(&blocks[0].2).unwrap(), fin);
        // The rewrite happens in place: frames stay at the end.
        assert_eq!(file.len(), frames_at + 100);
        assert!(file[frames_at..].iter().all(|&b| b == 9));
    }

    #[test]
    fn rejects_unsuitable_streams() {
        let good = flac_stream(&placeholder());
        let header = |streams: &[StreamInfo]| {
            FlacMuxer::new(Output::memory(), Vec::new()).write_header(streams)
        };
        assert!(header(&[]).is_err());
        assert!(header(&[good.clone(), good.clone()]).is_err());
        let mut pcm = good.clone();
        pcm.codec = CodecId::PcmS16Le;
        assert!(header(&[pcm]).is_err());
        let mut short = good.clone();
        short.extradata.truncate(20);
        assert!(header(&[short]).is_err());
        let mut zero_rate = good.clone();
        zero_rate.extradata[10..13].fill(0);
        assert!(header(&[zero_rate]).is_err());
        let mut video = good.clone();
        video.params = StreamParams::Video(VideoParams {
            width: 2,
            height: 2,
            pixel_format: crate::frame::PixelFormat::Gray,
            frame_rate: Rational::new(25, 1),
            sample_aspect_ratio: Rational::new(1, 1),
        });
        assert!(header(&[video]).is_err());

        let mut mux = FlacMuxer::new(Output::memory(), Vec::new());
        assert!(mux.write_packet(&Packet::new(0, vec![1], 0, 1)).is_err());
        mux.write_header(std::slice::from_ref(&good)).unwrap();
        assert!(mux.write_packet(&Packet::new(1, vec![1], 0, 1)).is_err());
        let mut bad = good.clone();
        bad.extradata.clear();
        assert!(mux.write_trailer(&[bad]).is_err());
    }

    #[test]
    fn encoder_output_makes_a_complete_file() {
        let format = FrameFormat::Audio(AudioFormat {
            sample_rate: 48000,
            channels: 2,
            kind: SampleKind::Int(16),
        });
        let mut enc = create_encoder(CodecId::Flac, &format, &EncoderOptions::default()).unwrap();
        let mut mux = FlacMuxer::new(Output::memory(), tags(&[("encoder", "test")]));
        mux.write_header(&[enc.stream_info()]).unwrap();
        let samples: Vec<i32> = (0..2 * 10_000).map(|i| ((i * 37) % 2001) - 1000).collect();
        let frame = AudioFrame::new(48000, 2, AudioData::Int { samples, bits: 16 }, 0);
        let mut frame_bytes = 0;
        let mut packets = enc.encode(&Frame::Audio(frame)).unwrap();
        packets.extend(enc.flush().unwrap());
        for p in &packets {
            frame_bytes += p.data.len();
            mux.write_packet(p).unwrap();
        }
        let info = enc.stream_info();
        mux.write_trailer(std::slice::from_ref(&info)).unwrap();
        let written = mux.bytes_written();
        let file = mux.into_output().into_bytes().unwrap();
        let (blocks, frames_at) = blocks(&file);
        let si = StreamInfoBlock::parse(&blocks[0].2).unwrap();
        assert_eq!(si.total_samples, 10_000);
        assert_ne!(si.md5, [0; 16]);
        assert_eq!(blocks[0].2, info.extradata);
        assert_eq!(file.len(), frames_at + frame_bytes);
        // The rewrite is counted in the progress figure as well.
        assert_eq!(written, (file.len() + STREAMINFO_SIZE) as u64);
        assert_eq!(&file[frames_at..frames_at + 2], [0xFF, 0xF8]);
    }
}
