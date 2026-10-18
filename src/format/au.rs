//! Sun AU / NeXT sound files.
//!
//! Written from the published description of the ".snd" header (the
//! audio file format manual page shipped with the systems that introduced
//! it). The fixed fields are big-endian 32-bit words:
//!
//! | offset | field                                          |
//! |--------|------------------------------------------------|
//! | 0      | magic `.snd`                                   |
//! | 4      | offset of the sample data (at least 24)        |
//! | 8      | sample data size, `0xFFFFFFFF` when unknown    |
//! | 12     | encoding                                       |
//! | 16     | sample rate                                    |
//! | 20     | channels                                       |
//! | 24     | annotation, up to the data offset              |
//!
//! Samples are interleaved and big-endian. The annotation is free text
//! ended by a NUL; by convention it holds `key=value` lines, which become
//! tags, and text without any `=` is kept as a comment.

use std::io::{Read, Seek, SeekFrom, Write};

use crate::error::{Error, Result};
use crate::format::rawaudio::{PcmPackets, block_align, pcm_stream};
use crate::format::{Demuxer, InputFormat, InputOptions, Muxer, OutputFormat, OutputOptions};
use crate::io::Output;
use crate::stream::{CodecId, Packet, StreamInfo};
use crate::util::bytes::ReadBytes;

pub const INPUT: InputFormat = InputFormat {
    name: "au",
    long_name: "Sun AU",
    extensions: &["au", "snd"],
    probe,
    open,
    raw_codec: None,
};

fn probe(data: &[u8]) -> u32 {
    if data.starts_with(MAGIC) { 100 } else { 0 }
}

fn open(reader: Box<dyn Read>, _options: &InputOptions) -> Result<Box<dyn Demuxer>> {
    Ok(Box::new(AuDemuxer::new(reader)?))
}

pub const OUTPUT: OutputFormat = OutputFormat {
    name: "au",
    long_name: "Sun AU",
    extensions: &["au"],
    audio_codec: Some(CodecId::PcmS16Be),
    video_codec: None,
    codecs: &[
        CodecId::PcmS8,
        CodecId::PcmS16Be,
        CodecId::PcmS24Be,
        CodecId::PcmS32Be,
        CodecId::PcmF32Be,
        CodecId::PcmF64Be,
        CodecId::PcmAlaw,
        CodecId::PcmMulaw,
    ],
    pixel_formats: &[],
    create,
};

fn create(path: &str, options: &OutputOptions) -> Result<Box<dyn Muxer>> {
    Ok(Box::new(AuMuxer::new(
        Output::create(path)?,
        options.metadata.clone(),
    )))
}

const MAGIC: &[u8; 4] = b".snd";

/// Size of the fixed part of the header, before the annotation.
const HEADER_LEN: u32 = 24;

/// Data size meaning "until the end of the file".
const UNKNOWN_SIZE: u32 = u32::MAX;

/// Annotation bytes loaded for tags; the rest, if any, is skipped.
const MAX_ANNOTATION: u32 = 64 * 1024;

/// Encoding numbers from the header definition, for the encodings that map
/// onto a PCM codec of the crate.
const ENCODINGS: [(u32, CodecId); 8] = [
    (1, CodecId::PcmMulaw),
    (2, CodecId::PcmS8),
    (3, CodecId::PcmS16Be),
    (4, CodecId::PcmS24Be),
    (5, CodecId::PcmS32Be),
    (6, CodecId::PcmF32Be),
    (7, CodecId::PcmF64Be),
    (27, CodecId::PcmAlaw),
];

pub struct AuDemuxer {
    streams: Vec<StreamInfo>,
    metadata: Vec<(String, String)>,
    packets: PcmPackets,
}

impl AuDemuxer {
    pub fn new(mut reader: Box<dyn Read>) -> Result<Self> {
        let magic: [u8; 4] = reader.read_fixed()?;
        if &magic != MAGIC {
            return Err(Error::invalid_data("not an AU file"));
        }
        let data_offset = reader.read_u32_be()?;
        let data_size = reader.read_u32_be()?;
        let encoding = reader.read_u32_be()?;
        let sample_rate = reader.read_u32_be()?;
        let channels = reader.read_u32_be()?;
        if data_offset < HEADER_LEN {
            return Err(Error::invalid_data(format!(
                "AU data offset {data_offset} is inside the header"
            )));
        }
        let codec = ENCODINGS
            .iter()
            .find(|(e, _)| *e == encoding)
            .map(|(_, c)| *c)
            .ok_or_else(|| Error::unsupported(format!("AU encoding {encoding}")))?;
        if sample_rate == 0 {
            return Err(Error::invalid_data("AU sample rate is 0"));
        }
        let channels = u16::try_from(channels)
            .ok()
            .filter(|&c| c > 0)
            .ok_or_else(|| Error::invalid_data(format!("AU declares {channels} channels")))?;

        let annotation_len = data_offset - HEADER_LEN;
        let kept = annotation_len.min(MAX_ANNOTATION);
        let annotation = reader.read_vec(kept as usize)?;
        reader.skip((annotation_len - kept) as u64)?;

        // Truncated files simply end early: the packet reader stops at the
        // end of the stream whatever the header promised.
        let remaining = match data_size {
            UNKNOWN_SIZE => None,
            size => Some(size as u64),
        };
        let align = block_align(codec, channels);
        let mut stream = pcm_stream(codec, sample_rate, channels);
        stream.duration = remaining.map(|r| (r / align as u64) as i64);
        Ok(AuDemuxer {
            streams: vec![stream],
            metadata: parse_annotation(&annotation),
            packets: PcmPackets::new(reader, align, remaining),
        })
    }
}

/// Turns the annotation into tags: `key=value` lines when there are any,
/// else the whole text as a comment.
fn parse_annotation(bytes: &[u8]) -> Vec<(String, String)> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let text = String::from_utf8_lossy(&bytes[..end]);
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    if !text.contains('=') {
        return vec![("comment".to_string(), text.to_string())];
    }
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            let (key, value) = (key.trim(), value.trim());
            if key.is_empty() || value.is_empty() {
                None
            } else {
                Some((key.to_ascii_lowercase(), value.to_string()))
            }
        })
        .collect()
}

/// Builds the annotation written after the fixed header: one `key=value`
/// line per tag that can be written unambiguously, then NUL bytes so that
/// the text is terminated and the data starts on a multiple of 8.
fn build_annotation(metadata: &[(String, String)]) -> Vec<u8> {
    let lines: Vec<String> = metadata
        .iter()
        .filter(|(k, v)| {
            !k.is_empty()
                && !v.is_empty()
                && !k.contains(['=', '\n', '\0'])
                && !v.contains(['\n', '\0'])
        })
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    let mut annotation = lines.join("\n").into_bytes();
    // At least one NUL, and at least the 4 annotation bytes the header
    // definition reserves.
    let unpadded = HEADER_LEN as usize + (annotation.len() + 1).max(4);
    let total = unpadded.next_multiple_of(8);
    annotation.resize(total - HEADER_LEN as usize, 0);
    annotation
}

impl Demuxer for AuDemuxer {
    fn format_name(&self) -> &'static str {
        "au"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }

    fn read_packet(&mut self) -> Result<Option<Packet>> {
        self.packets.read_packet()
    }
}

pub struct AuMuxer {
    out: Output,
    metadata: Vec<(String, String)>,
    data_bytes: u64,
}

impl AuMuxer {
    pub fn new(out: Output, metadata: Vec<(String, String)>) -> Self {
        AuMuxer {
            out,
            metadata,
            data_bytes: 0,
        }
    }

    pub fn into_output(self) -> Output {
        self.out
    }
}

impl Muxer for AuMuxer {
    fn write_header(&mut self, streams: &[StreamInfo]) -> Result<()> {
        let [stream] = streams else {
            return Err(Error::unsupported("AU holds exactly one audio stream"));
        };
        let a = stream
            .audio()
            .ok_or_else(|| Error::unsupported("AU holds audio only"))?;
        let encoding = ENCODINGS
            .iter()
            .find(|(_, c)| *c == stream.codec)
            .map(|(e, _)| *e)
            .ok_or_else(|| Error::unsupported(format!("{} in AU", stream.codec)))?;
        if a.sample_rate == 0 || a.channels == 0 {
            return Err(Error::invalid_argument(
                "AU needs a sample rate and at least one channel",
            ));
        }
        let annotation = build_annotation(&self.metadata);
        let data_offset = u32::try_from(HEADER_LEN as usize + annotation.len())
            .map_err(|_| Error::invalid_argument("AU annotation is too long"))?;
        let mut header = Vec::with_capacity(data_offset as usize);
        header.extend_from_slice(MAGIC);
        for word in [
            data_offset,
            // Patched in the trailer when the output can seek; on a pipe
            // it stays "unknown" and readers go to the end of the stream.
            UNKNOWN_SIZE,
            encoding,
            a.sample_rate,
            a.channels as u32,
        ] {
            header.extend_from_slice(&word.to_be_bytes());
        }
        header.extend_from_slice(&annotation);
        self.out.write_all(&header)?;
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        self.out.write_all(&packet.data)?;
        self.data_bytes += packet.data.len() as u64;
        Ok(())
    }

    fn write_trailer(&mut self, _streams: &[StreamInfo]) -> Result<()> {
        // Sizes that do not fit (or would read as the "unknown" marker)
        // stay unknown, which is still a valid file.
        if self.out.is_seekable() && self.data_bytes < UNKNOWN_SIZE as u64 {
            self.out.seek(SeekFrom::Start(8))?;
            self.out
                .write_all(&(self.data_bytes as u32).to_be_bytes())?;
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
    use crate::rational::Rational;
    use crate::stream::{AudioParams, StreamParams};
    use std::io::Cursor;

    fn fixture(name: &str) -> Vec<u8> {
        let path = format!(
            "{}/tests/fixtures/audio-containers/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn stream(codec: CodecId, channels: u16, rate: u32) -> StreamInfo {
        StreamInfo::new(
            0,
            codec,
            StreamParams::Audio(AudioParams {
                sample_rate: rate,
                channels,
                bits_per_sample: codec.pcm_bits().unwrap(),
            }),
            Rational::new(1, rate as i64),
        )
    }

    fn write_au(codec: CodecId, channels: u16, payload: &[u8], tags: &[(&str, &str)]) -> Vec<u8> {
        let info = stream(codec, channels, 22050);
        let tags = tags
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let mut mux = AuMuxer::new(Output::memory(), tags);
        mux.write_header(std::slice::from_ref(&info)).unwrap();
        mux.write_packet(&Packet::new(0, payload.to_vec(), 0, 0))
            .unwrap();
        mux.write_trailer(&[info]).unwrap();
        mux.into_output().into_bytes().unwrap()
    }

    fn demux(bytes: Vec<u8>) -> Result<AuDemuxer> {
        AuDemuxer::new(Box::new(Cursor::new(bytes)))
    }

    fn read_all(bytes: Vec<u8>) -> (AuDemuxer, Vec<u8>) {
        let mut d = demux(bytes).unwrap();
        let mut data = Vec::new();
        let mut pts = 0;
        while let Some(p) = d.read_packet().unwrap() {
            assert_eq!(p.pts, pts);
            pts += p.duration;
            data.extend(p.data);
        }
        (d, data)
    }

    fn header(offset: u32, size: u32, encoding: u32, rate: u32, channels: u32) -> Vec<u8> {
        let mut h = MAGIC.to_vec();
        for w in [offset, size, encoding, rate, channels] {
            h.extend_from_slice(&w.to_be_bytes());
        }
        h
    }

    #[test]
    fn round_trips_every_codec() {
        for codec in OUTPUT.codecs {
            for channels in [1u16, 2, 5] {
                let align = block_align(*codec, channels);
                let payload: Vec<u8> = (0..align * 11).map(|i| (i * 3) as u8).collect();
                let (d, data) = read_all(write_au(*codec, channels, &payload, &[]));
                let info = &d.streams()[0];
                assert_eq!(info.codec, *codec);
                assert_eq!(info.audio().unwrap().channels, channels);
                assert_eq!(info.audio().unwrap().sample_rate, 22050);
                assert_eq!(info.duration, Some(11));
                assert_eq!(data, payload, "{codec} {channels}ch");
            }
        }
    }

    #[test]
    fn header_layout() {
        let bytes = write_au(CodecId::PcmMulaw, 1, &[0xFF; 3], &[]);
        assert_eq!(&bytes[..4], b".snd");
        let word = |i: usize| u32::from_be_bytes(bytes[i..i + 4].try_into().unwrap());
        assert_eq!(word(4), 32);
        assert_eq!(word(8), 3);
        assert_eq!(word(12), 1);
        assert_eq!(word(16), 22050);
        assert_eq!(word(20), 1);
        assert_eq!(bytes.len(), 35);
    }

    #[test]
    fn annotation_is_padded_and_terminated() {
        for len in 0..40 {
            let value = "x".repeat(len);
            let tags = vec![("title".to_string(), value)];
            let a = build_annotation(&tags);
            assert_eq!((HEADER_LEN as usize + a.len()) % 8, 0);
            assert!(a.len() >= 4);
            assert_eq!(*a.last().unwrap(), 0);
        }
        assert_eq!(build_annotation(&[]), [0; 8]);
    }

    #[test]
    fn tags_round_trip() {
        let bytes = write_au(
            CodecId::PcmS16Be,
            1,
            &[0, 1],
            &[
                ("title", "A title"),
                ("artist", "Someone"),
                ("bad=key", "dropped"),
                ("comment", "two\nlines"),
                ("genre", "Test"),
            ],
        );
        let (d, data) = read_all(bytes);
        assert_eq!(data, [0, 1]);
        let tags: Vec<(&str, &str)> = d
            .metadata()
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            tags,
            [
                ("title", "A title"),
                ("artist", "Someone"),
                ("genre", "Test")
            ]
        );
    }

    #[test]
    fn annotation_forms() {
        assert_eq!(
            parse_annotation(b"Title=Hi\nAlbum = Al \nno equals\0garbage"),
            [
                ("title".to_string(), "Hi".to_string()),
                ("album".to_string(), "Al".to_string())
            ]
        );
        assert_eq!(
            parse_annotation(b"recorded at home\0\0\0"),
            [("comment".to_string(), "recorded at home".to_string())]
        );
        assert!(parse_annotation(b"\0\0\0\0").is_empty());
        assert!(parse_annotation(&[0xFF, 0xFE, b'=', 0x80]).len() <= 1);
    }

    #[test]
    fn unknown_size_reads_to_end() {
        let mut bytes = header(28, UNKNOWN_SIZE, 3, 8000, 2);
        bytes.extend_from_slice(b"\0\0\0\0");
        bytes.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let (d, data) = read_all(bytes);
        assert_eq!(d.streams()[0].duration, None);
        assert_eq!(data, [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn size_bounds_the_samples_and_minimal_header_works() {
        // A 24-byte header with no annotation at all is accepted.
        let mut bytes = header(24, 2, 2, 8000, 1);
        bytes.extend_from_slice(&[1, 2, 3, 4]);
        let (d, data) = read_all(bytes);
        assert_eq!(d.streams()[0].codec, CodecId::PcmS8);
        assert_eq!(d.streams()[0].duration, Some(2));
        assert_eq!(data, [1, 2]);
    }

    #[test]
    fn truncated_data_ends_cleanly() {
        let mut bytes = header(32, 1000, 4, 8000, 2);
        bytes.extend_from_slice(&[0; 8]);
        bytes.extend_from_slice(&[9; 13]);
        let (d, data) = read_all(bytes);
        assert_eq!(d.streams()[0].duration, Some(166));
        assert_eq!(data, [9; 12]);
    }

    #[test]
    fn rejects_malformed_headers() {
        let cases = [
            Vec::new(),
            b".snd\0\0\0\x18".to_vec(),
            header(20, 0, 3, 8000, 1),
            header(24, 0, 3, 0, 1),
            header(24, 0, 3, 8000, 0),
            header(24, 0, 3, 8000, 70000),
            {
                let mut h = header(24, 0, 3, 8000, 1);
                h[0] = b'x';
                h
            },
            // An annotation that runs past the end of the file.
            header(4096, 0, 3, 8000, 1),
            // A huge offset in a tiny file must fail without allocating.
            header(u32::MAX, 0, 3, 8000, 1),
        ];
        for (i, bytes) in cases.into_iter().enumerate() {
            assert!(demux(bytes).is_err(), "case {i} should fail");
        }
        for encoding in [0, 8, 23, 26, 28] {
            let result = demux(header(24, 0, encoding, 8000, 1));
            assert!(matches!(result, Err(Error::Unsupported(_))), "{encoding}");
        }
    }

    #[test]
    fn muxer_rejects_what_au_cannot_hold() {
        let mut mux = AuMuxer::new(Output::memory(), Vec::new());
        assert!(
            mux.write_header(&[stream(CodecId::PcmS16Le, 1, 8000)])
                .is_err()
        );
        assert!(
            mux.write_header(&[stream(CodecId::PcmU8, 1, 8000)])
                .is_err()
        );
        assert!(mux.write_header(&[]).is_err());
    }

    #[test]
    fn reads_reference_files() {
        // Files from an independent writer, compared with the same samples
        // stored raw.
        let (d, data) = read_all(fixture("sine_s16be.au"));
        assert_eq!(d.streams()[0].codec, CodecId::PcmS16Be);
        assert_eq!(d.streams()[0].duration, Some(80));
        assert_eq!(data, fixture("sine_s16be.raw"));
        assert!(
            d.metadata()
                .contains(&("title".to_string(), "Fixture".to_string()))
        );

        let (d, data) = read_all(fixture("sine_mulaw.au"));
        assert_eq!(d.streams()[0].codec, CodecId::PcmMulaw);
        assert_eq!(d.streams()[0].audio().unwrap().bits_per_sample, 16);
        assert_eq!(data, fixture("sine_mulaw.raw"));
    }
}
