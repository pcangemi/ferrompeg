//! RIFF WAVE files.
//!
//! Reads PCM, IEEE float, A-law and mu-law, including the
//! WAVE_FORMAT_EXTENSIBLE header and streams whose sizes were never
//! patched (written to a pipe). Writes PCM and float, switching to the
//! extensible header when plain WAVEFORMATEX is ambiguous.

use std::io::{Read, Seek, SeekFrom, Write};

use crate::error::{Error, Result};
use crate::format::{Demuxer, InputFormat, InputOptions, Muxer, OutputFormat, OutputOptions};
use crate::io::Output;
use crate::rational::Rational;
use crate::stream::{AudioParams, CodecId, Packet, StreamInfo, StreamParams};
use crate::util::bytes::ReadBytes;

const FORMAT_PCM: u16 = 0x0001;
const FORMAT_IEEE_FLOAT: u16 = 0x0003;
const FORMAT_ALAW: u16 = 0x0006;
const FORMAT_MULAW: u16 = 0x0007;
const FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// Tail shared by every KSDATAFORMAT_SUBTYPE GUID; the first two bytes
/// carry the plain format tag.
const GUID_TAIL: [u8; 14] = [
    0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];

/// Samples per channel in each packet read from a WAV file.
const PACKET_SAMPLES: usize = 4096;

pub const INPUT: InputFormat = InputFormat {
    name: "wav",
    long_name: "WAV / WAVE (Waveform Audio)",
    extensions: &["wav", "wave"],
    probe,
    open,
    raw_codec: None,
};

pub const OUTPUT: OutputFormat = OutputFormat {
    name: "wav",
    long_name: "WAV / WAVE (Waveform Audio)",
    extensions: &["wav", "wave"],
    audio_codec: Some(CodecId::PcmS16Le),
    video_codec: None,
    codecs: &[
        CodecId::PcmU8,
        CodecId::PcmS16Le,
        CodecId::PcmS24Le,
        CodecId::PcmS32Le,
        CodecId::PcmF32Le,
        CodecId::PcmF64Le,
        CodecId::PcmAlaw,
        CodecId::PcmMulaw,
    ],
    pixel_formats: &[],
    create,
};

fn probe(data: &[u8]) -> u32 {
    if data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WAVE" {
        100
    } else {
        0
    }
}

fn open(reader: Box<dyn Read>, _options: &InputOptions) -> Result<Box<dyn Demuxer>> {
    Ok(Box::new(WavDemuxer::new(reader)?))
}

fn create(path: &str, _options: &OutputOptions) -> Result<Box<dyn Muxer>> {
    Ok(Box::new(WavMuxer::new(Output::create(path)?)))
}

pub struct WavDemuxer {
    reader: Box<dyn Read>,
    streams: Vec<StreamInfo>,
    metadata: Vec<(String, String)>,
    block_align: usize,
    /// Bytes of sample data left, `None` when the size is unknown.
    remaining: Option<u64>,
    next_pts: i64,
}

impl WavDemuxer {
    pub fn new(mut reader: Box<dyn Read>) -> Result<Self> {
        let riff: [u8; 4] = reader.read_fixed()?;
        let _riff_size = reader.read_u32_le()?;
        let wave: [u8; 4] = reader.read_fixed()?;
        if &riff != b"RIFF" || &wave != b"WAVE" {
            return Err(Error::invalid_data("not a RIFF WAVE file"));
        }

        let mut fmt: Option<(CodecId, AudioParams, usize)> = None;
        let mut metadata = Vec::new();
        loop {
            let id: [u8; 4] = match reader.read_fixed() {
                Ok(id) => id,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Err(Error::invalid_data("WAV file has no data chunk"));
                }
                Err(e) => return Err(e.into()),
            };
            let size = reader.read_u32_le()?;
            match &id {
                b"fmt " => {
                    let body = reader.read_vec(size as usize)?;
                    fmt = Some(parse_fmt(&body)?);
                }
                b"data" => {
                    let (codec, params, block_align) =
                        fmt.ok_or_else(|| Error::invalid_data("WAV data chunk before fmt chunk"))?;
                    // 0xFFFFFFFF (and 0, which some streaming writers use)
                    // mean the writer could not seek back to fill the size.
                    let remaining = match size {
                        0 | u32::MAX => None,
                        s => Some(s as u64),
                    };
                    let time_base = Rational::new(1, params.sample_rate as i64);
                    let mut stream =
                        StreamInfo::new(0, codec, StreamParams::Audio(params), time_base);
                    stream.duration = remaining.map(|r| (r / block_align as u64) as i64);
                    stream.bit_rate = Some(params.sample_rate as u64 * block_align as u64 * 8);
                    return Ok(WavDemuxer {
                        reader,
                        streams: vec![stream],
                        metadata,
                        block_align,
                        remaining,
                        next_pts: 0,
                    });
                }
                b"LIST" => {
                    let body = reader.read_vec(size as usize)?;
                    metadata.extend(parse_info_list(&body));
                }
                _ => reader.skip(size as u64)?,
            }
            // Chunks are padded to an even size.
            if size % 2 == 1 {
                reader.skip(1)?;
            }
        }
    }
}

fn parse_fmt(body: &[u8]) -> Result<(CodecId, AudioParams, usize)> {
    if body.len() < 16 {
        return Err(Error::invalid_data("WAV fmt chunk is too short"));
    }
    let u16_at = |i: usize| u16::from_le_bytes([body[i], body[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes(body[i..i + 4].try_into().unwrap());
    let mut tag = u16_at(0);
    let channels = u16_at(2);
    let sample_rate = u32_at(4);
    let block_align = u16_at(12) as usize;
    let mut bits = u16_at(14) as u32;
    if tag == FORMAT_EXTENSIBLE {
        if body.len() < 40 {
            return Err(Error::invalid_data("WAV extensible fmt chunk is too short"));
        }
        let valid_bits = u16_at(18) as u32;
        if body[26..40] != GUID_TAIL {
            return Err(Error::unsupported("WAV extensible subformat GUID"));
        }
        tag = u16_at(24);
        // The container size decides the byte layout; valid bits only say
        // how many of them carry signal, so 20-in-24 still decodes as 24.
        if valid_bits > bits {
            return Err(Error::invalid_data("WAV valid bits exceed container size"));
        }
    }
    if channels == 0 || sample_rate == 0 {
        return Err(Error::invalid_data(
            "WAV declares no channels or a zero sample rate",
        ));
    }
    let codec = match (tag, bits) {
        (FORMAT_PCM, 8) => CodecId::PcmU8,
        (FORMAT_PCM, 16) => CodecId::PcmS16Le,
        (FORMAT_PCM, 24) => CodecId::PcmS24Le,
        (FORMAT_PCM, 32) => CodecId::PcmS32Le,
        (FORMAT_IEEE_FLOAT, 32) => CodecId::PcmF32Le,
        (FORMAT_IEEE_FLOAT, 64) => CodecId::PcmF64Le,
        (FORMAT_ALAW, 8) => CodecId::PcmAlaw,
        (FORMAT_MULAW, 8) => CodecId::PcmMulaw,
        (tag, b) => {
            return Err(Error::unsupported(format!(
                "WAV format tag 0x{tag:04x} with {b} bits per sample"
            )));
        }
    };
    let expected_align = channels as usize * (bits as usize / 8);
    if block_align != expected_align {
        return Err(Error::invalid_data(format!(
            "WAV block align {block_align} does not match {channels} channels of {bits} bits"
        )));
    }
    if matches!(codec, CodecId::PcmAlaw | CodecId::PcmMulaw) {
        bits = 16;
    }
    Ok((
        codec,
        AudioParams {
            sample_rate,
            channels,
            bits_per_sample: bits,
        },
        block_align,
    ))
}

/// Reads INFO tags from a LIST chunk body (INAM is the title, IART the
/// artist...). Other LIST types are ignored.
fn parse_info_list(body: &[u8]) -> Vec<(String, String)> {
    let mut tags = Vec::new();
    if body.len() < 4 || &body[..4] != b"INFO" {
        return tags;
    }
    let mut pos = 4;
    while pos + 8 <= body.len() {
        let id = &body[pos..pos + 4];
        let size = u32::from_le_bytes(body[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let start = pos + 8;
        let Some(end) = start.checked_add(size).filter(|&e| e <= body.len()) else {
            break;
        };
        let value = String::from_utf8_lossy(&body[start..end])
            .trim_end_matches('\0')
            .to_string();
        let key = match id {
            b"INAM" => "title",
            b"IART" => "artist",
            b"IPRD" => "album",
            b"ICMT" => "comment",
            b"ICRD" => "date",
            b"IGNR" => "genre",
            b"ISFT" => "encoder",
            b"ICOP" => "copyright",
            _ => "",
        };
        if !key.is_empty() && !value.is_empty() {
            tags.push((key.to_string(), value));
        }
        pos = end + size % 2;
    }
    tags
}

impl Demuxer for WavDemuxer {
    fn format_name(&self) -> &'static str {
        "wav"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }

    fn read_packet(&mut self) -> Result<Option<Packet>> {
        let mut want = PACKET_SAMPLES * self.block_align;
        if let Some(remaining) = self.remaining {
            want = want.min(remaining as usize);
        }
        if want == 0 {
            return Ok(None);
        }
        let mut data = vec![0u8; want];
        let got = self.reader.read_full(&mut data)?;
        // A truncated file ends on a partial block; keep whole blocks only.
        let whole = got / self.block_align * self.block_align;
        data.truncate(whole);
        if let Some(remaining) = &mut self.remaining {
            *remaining = if got < want {
                0
            } else {
                *remaining - got as u64
            };
        }
        if whole == 0 {
            return Ok(None);
        }
        let samples = (whole / self.block_align) as i64;
        let packet = Packet::new(0, data, self.next_pts, samples);
        self.next_pts += samples;
        Ok(Some(packet))
    }
}

pub struct WavMuxer {
    out: Output,
    /// Offset of the data chunk size field, to patch it in the trailer.
    data_size_offset: u64,
    data_bytes: u64,
}

impl WavMuxer {
    pub fn new(out: Output) -> Self {
        WavMuxer {
            out,
            data_size_offset: 0,
            data_bytes: 0,
        }
    }

    pub fn into_output(self) -> Output {
        self.out
    }
}

fn channel_mask(channels: u16) -> u32 {
    // Speaker positions from the WAVE_FORMAT_EXTENSIBLE definition:
    // FL=0x1 FR=0x2 FC=0x4 LFE=0x8 BL=0x10 BR=0x20 SL=0x200 SR=0x400.
    match channels {
        1 => 0x4,
        2 => 0x3,
        3 => 0x7,
        4 => 0x33,
        5 => 0x37,
        6 => 0x3F,
        7 => 0x13F,
        8 => 0x63F,
        _ => 0,
    }
}

impl Muxer for WavMuxer {
    fn write_header(&mut self, streams: &[StreamInfo]) -> Result<()> {
        let [stream] = streams else {
            return Err(Error::unsupported("WAV holds exactly one audio stream"));
        };
        let a = stream
            .audio()
            .ok_or_else(|| Error::unsupported("WAV holds audio only"))?;
        let (tag, bits) = match stream.codec {
            CodecId::PcmF32Le | CodecId::PcmF64Le => {
                (FORMAT_IEEE_FLOAT, stream.codec.pcm_bits().unwrap())
            }
            CodecId::PcmAlaw => (FORMAT_ALAW, 8),
            CodecId::PcmMulaw => (FORMAT_MULAW, 8),
            CodecId::PcmU8 | CodecId::PcmS16Le | CodecId::PcmS24Le | CodecId::PcmS32Le => {
                (FORMAT_PCM, stream.codec.pcm_bits().unwrap())
            }
            c => return Err(Error::unsupported(format!("{c} in WAV"))),
        };
        let block_align = a.channels as u32 * bits / 8;
        // Plain WAVEFORMATEX cannot say which speakers more than two
        // channels map to, and is ambiguous for integer PCM above 16 bits.
        let extensible = a.channels > 2 || (tag == FORMAT_PCM && bits > 16);

        let mut fmt = Vec::with_capacity(40);
        fmt.extend_from_slice(&(if extensible { FORMAT_EXTENSIBLE } else { tag }).to_le_bytes());
        fmt.extend_from_slice(&a.channels.to_le_bytes());
        fmt.extend_from_slice(&a.sample_rate.to_le_bytes());
        fmt.extend_from_slice(&(a.sample_rate * block_align).to_le_bytes());
        fmt.extend_from_slice(&(block_align as u16).to_le_bytes());
        fmt.extend_from_slice(&(bits as u16).to_le_bytes());
        if extensible {
            fmt.extend_from_slice(&22u16.to_le_bytes());
            fmt.extend_from_slice(&(bits as u16).to_le_bytes());
            fmt.extend_from_slice(&channel_mask(a.channels).to_le_bytes());
            fmt.extend_from_slice(&tag.to_le_bytes());
            fmt.extend_from_slice(&GUID_TAIL);
        } else if tag != FORMAT_PCM {
            // Non-PCM WAVEFORMATEX carries a cbSize field.
            fmt.extend_from_slice(&0u16.to_le_bytes());
        }

        self.out.write_all(b"RIFF")?;
        self.out.write_all(&u32::MAX.to_le_bytes())?;
        self.out.write_all(b"WAVE")?;
        self.out.write_all(b"fmt ")?;
        self.out.write_all(&(fmt.len() as u32).to_le_bytes())?;
        self.out.write_all(&fmt)?;
        self.out.write_all(b"data")?;
        self.data_size_offset = 12 + 8 + fmt.len() as u64 + 4;
        // Left at 0xFFFFFFFF on pipes, which readers treat as "until EOF".
        self.out.write_all(&u32::MAX.to_le_bytes())?;
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        self.out.write_all(&packet.data)?;
        self.data_bytes += packet.data.len() as u64;
        Ok(())
    }

    fn write_trailer(&mut self, _streams: &[StreamInfo]) -> Result<()> {
        if self.data_bytes % 2 == 1 {
            self.out.write_all(&[0])?;
        }
        if self.out.is_seekable() {
            let riff_size = self.data_size_offset + 4 + self.data_bytes + self.data_bytes % 2 - 8;
            if riff_size > u32::MAX as u64 {
                return Err(Error::unsupported(
                    "WAV output larger than 4 GiB (RF64 is not implemented)",
                ));
            }
            self.out.seek(SeekFrom::Start(4))?;
            self.out.write_all(&(riff_size as u32).to_le_bytes())?;
            self.out.seek(SeekFrom::Start(self.data_size_offset))?;
            self.out
                .write_all(&(self.data_bytes as u32).to_le_bytes())?;
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
    use std::io::Cursor;

    fn stream(codec: CodecId, channels: u16, rate: u32) -> StreamInfo {
        let bits = match codec {
            CodecId::PcmAlaw | CodecId::PcmMulaw => 16,
            c => c.pcm_bits().unwrap(),
        };
        StreamInfo::new(
            0,
            codec,
            StreamParams::Audio(AudioParams {
                sample_rate: rate,
                channels,
                bits_per_sample: bits,
            }),
            Rational::new(1, rate as i64),
        )
    }

    fn write_wav(codec: CodecId, channels: u16, payload: &[u8]) -> Vec<u8> {
        let info = stream(codec, channels, 48000);
        let mut mux = WavMuxer::new(Output::memory());
        mux.write_header(std::slice::from_ref(&info)).unwrap();
        mux.write_packet(&Packet::new(0, payload.to_vec(), 0, 0))
            .unwrap();
        mux.write_trailer(&[info]).unwrap();
        mux.into_output().into_bytes().unwrap()
    }

    fn read_all(bytes: Vec<u8>) -> (StreamInfo, Vec<u8>) {
        let mut demux = WavDemuxer::new(Box::new(Cursor::new(bytes))).unwrap();
        let info = demux.streams()[0].clone();
        let mut data = Vec::new();
        while let Some(p) = demux.read_packet().unwrap() {
            data.extend(p.data);
        }
        (info, data)
    }

    #[test]
    fn round_trips_every_codec() {
        for codec in OUTPUT.codecs {
            for channels in [1u16, 2, 6] {
                let block = channels as usize * codec.pcm_bits().unwrap() as usize / 8;
                let payload: Vec<u8> = (0..block * 10).map(|i| i as u8).collect();
                let bytes = write_wav(*codec, channels, &payload);
                let (info, data) = read_all(bytes);
                assert_eq!(info.codec, *codec);
                assert_eq!(info.audio().unwrap().channels, channels);
                assert_eq!(info.duration, Some(10));
                assert_eq!(data, payload, "{codec} {channels}ch");
            }
        }
    }

    #[test]
    fn header_layout_is_canonical() {
        let bytes = write_wav(CodecId::PcmS16Le, 2, &[1, 2, 3, 4]);
        assert_eq!(bytes.len(), 44 + 4);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 40);
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 4);
    }

    #[test]
    fn odd_data_is_padded() {
        let bytes = write_wav(CodecId::PcmU8, 1, &[1, 2, 3]);
        assert_eq!(bytes.len(), 44 + 4);
        let (_, data) = read_all(bytes);
        assert_eq!(data, [1, 2, 3]);
    }

    #[test]
    fn unknown_size_reads_to_end() {
        let mut bytes = write_wav(CodecId::PcmS16Le, 1, &[1, 0, 2, 0]);
        bytes[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&[3, 0, 9]);
        let (info, data) = read_all(bytes);
        assert_eq!(info.duration, None);
        assert_eq!(data, [1, 0, 2, 0, 3, 0]);
    }

    #[test]
    fn skips_unknown_chunks_and_reads_tags() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF\0\0\0\0WAVE");
        bytes.extend_from_slice(b"junk\x03\0\0\0abc\0");
        let mut info = b"INFO".to_vec();
        info.extend_from_slice(b"INAM\x05\0\0\0Hello\0");
        bytes.extend_from_slice(b"LIST");
        bytes.extend_from_slice(&(info.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&info);
        bytes.extend_from_slice(b"fmt \x10\0\0\0");
        bytes.extend_from_slice(&[1, 0, 1, 0, 0x40, 0x1F, 0, 0, 0x80, 0x3E, 0, 0, 2, 0, 16, 0]);
        bytes.extend_from_slice(b"data\x02\0\0\0\x05\x00");
        let demux = WavDemuxer::new(Box::new(Cursor::new(bytes))).unwrap();
        assert_eq!(demux.metadata(), [("title".into(), "Hello".into())]);
        assert_eq!(demux.streams()[0].audio().unwrap().sample_rate, 8000);
    }

    #[test]
    fn rejects_garbage() {
        assert!(WavDemuxer::new(Box::new(Cursor::new(b"RIFX0000WAVE".to_vec()))).is_err());
        assert!(WavDemuxer::new(Box::new(Cursor::new(b"RIFF0000WAVE".to_vec()))).is_err());
    }
}
