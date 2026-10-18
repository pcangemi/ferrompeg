//! The transcoding loop: read, decode, trim, filter, convert, encode, mux.

use std::time::Instant;

use crate::cli::log::{self, LogLevel};
use crate::cli::options::{CodecChoice, GlobalOptions, InputSpec, OutputSpec};
use crate::codec::{self, Decoder, Encoder, EncoderOptions};
use crate::convert::audio::AudioConverter;
use crate::convert::video::{self as vconv, ScaleAlgorithm};
use crate::error::{Error, Result};
use crate::filter::{self, Filter};
use crate::format::{self, Demuxer, Muxer, OutputFormat, OutputOptions};
use crate::frame::{
    AudioData, AudioFormat, Frame, FrameFormat, PixelFormat, SampleKind, VideoFormat,
};
use crate::rational::Rational;
use crate::source::{self, Source};
use crate::stream::{CodecId, MediaType, Packet, StreamInfo, StreamParams};
use crate::util::time::format_duration;

/// An opened input: either a container with its decoders or a generator.
pub struct Input {
    pub format_name: String,
    pub streams: Vec<StreamInfo>,
    pub metadata: Vec<(String, String)>,
    kind: InputKind,
}

enum InputKind {
    Demux {
        demuxer: Box<dyn Demuxer>,
        decoders: Vec<Option<Box<dyn Decoder>>>,
    },
    Source(Box<dyn Source>),
}

enum Item {
    Packet(Packet),
    Frame(usize, Frame),
}

impl Input {
    pub fn open(spec: &InputSpec) -> Result<Self> {
        if spec.format.as_deref() == Some("lavfi") {
            let source = source::open(&spec.path)?;
            return Ok(Input {
                format_name: "lavfi".into(),
                streams: vec![source.stream().clone()],
                metadata: Vec::new(),
                kind: InputKind::Source(source),
            });
        }
        let demuxer = format::open_input(&spec.path, spec.format.as_deref(), &spec.options)?;
        let streams = demuxer.streams().to_vec();
        if streams.is_empty() {
            return Err(Error::invalid_data(format!("{}: no streams", spec.path)));
        }
        let mut decoders = Vec::new();
        decoders.resize_with(streams.len(), || None);
        Ok(Input {
            format_name: demuxer.format_name().into(),
            metadata: demuxer.metadata().to_vec(),
            streams,
            kind: InputKind::Demux { demuxer, decoders },
        })
    }

    /// Total duration in seconds, the longest stream when several.
    pub fn duration(&self) -> Option<f64> {
        self.streams
            .iter()
            .filter_map(|s| s.duration_seconds())
            .reduce(f64::max)
    }

    fn read(&mut self) -> Result<Option<Item>> {
        match &mut self.kind {
            InputKind::Demux { demuxer, .. } => Ok(demuxer.read_packet()?.map(Item::Packet)),
            InputKind::Source(source) => Ok(source.next_frame()?.map(|f| Item::Frame(0, f))),
        }
    }

    fn decode(&mut self, packet: &Packet) -> Result<Vec<Frame>> {
        let InputKind::Demux { decoders, .. } = &mut self.kind else {
            return Ok(Vec::new());
        };
        let slot = &mut decoders[packet.stream_index];
        if slot.is_none() {
            *slot = Some(codec::create_decoder(&self.streams[packet.stream_index])?);
        }
        slot.as_mut().unwrap().decode(packet)
    }

    fn flush_decoders(&mut self) -> Result<Vec<(usize, Frame)>> {
        let mut out = Vec::new();
        if let InputKind::Demux { decoders, .. } = &mut self.kind {
            for (i, d) in decoders.iter_mut().enumerate() {
                if let Some(d) = d {
                    out.extend(d.flush()?.into_iter().map(|f| (i, f)));
                }
            }
        }
        Ok(out)
    }

    fn is_endless(&self) -> bool {
        matches!(&self.kind, InputKind::Source(s) if s.stream().duration.is_none())
    }
}

/// A time window in seconds. Frames are cut to it and their timestamps
/// shifted so the window starts at zero.
#[derive(Clone, Copy, Debug)]
struct Trim {
    start: f64,
    end: Option<f64>,
}

enum Trimmed {
    Before,
    Keep(Frame),
    After,
}

impl Trim {
    fn new(start: Option<f64>, duration: Option<f64>, to: Option<f64>) -> Option<Self> {
        let start = start.unwrap_or(0.0).max(0.0);
        let end = match (duration, to) {
            (Some(d), Some(t)) => Some((start + d).min(t)),
            (Some(d), None) => Some(start + d),
            (None, Some(t)) => Some(t),
            (None, None) => None,
        };
        if start == 0.0 && end.is_none() {
            None
        } else {
            Some(Trim { start, end })
        }
    }

    fn apply(&self, frame: Frame) -> Trimmed {
        match frame {
            Frame::Audio(mut a) => {
                let rate = a.sample_rate as f64;
                let n = a.nb_samples() as i64;
                let start = (self.start * rate).round() as i64;
                let end = self.end.map(|e| (e * rate).round() as i64);
                if end.is_some_and(|e| a.pts >= e) {
                    return Trimmed::After;
                }
                if a.pts + n <= start {
                    return Trimmed::Before;
                }
                let front = (start - a.pts).clamp(0, n) as usize;
                let back = end.map_or(0, |e| (a.pts + n - e).clamp(0, n)) as usize;
                if front > 0 || back > 0 {
                    let ch = a.channels as usize;
                    let range = front * ch..(n as usize - back) * ch;
                    a.data = match a.data {
                        AudioData::Int { samples, bits } => AudioData::Int {
                            samples: samples[range].to_vec(),
                            bits,
                        },
                        AudioData::Float(samples) => AudioData::Float(samples[range].to_vec()),
                    };
                }
                a.pts = a.pts + front as i64 - start;
                Trimmed::Keep(Frame::Audio(a))
            }
            Frame::Video(mut v) => {
                let tb = v.time_base.to_f64();
                let t = v.pts as f64 * tb;
                // Half a frame of tolerance absorbs rounding in the
                // seconds to frames conversion.
                let eps = tb / 2.0;
                if self.end.is_some_and(|e| t >= e - eps) {
                    return Trimmed::After;
                }
                if t < self.start - eps {
                    return Trimmed::Before;
                }
                v.pts -= (self.start / tb).round() as i64;
                Trimmed::Keep(Frame::Video(v))
            }
        }
    }

    /// Packet level trimming for stream copy, where samples cannot be cut.
    fn apply_packet(&self, mut packet: Packet, time_base: Rational) -> Trimmed2 {
        let t = packet.pts as f64 * time_base.to_f64();
        let end_t = (packet.pts + packet.duration) as f64 * time_base.to_f64();
        if self.end.is_some_and(|e| t >= e) {
            return Trimmed2::After;
        }
        if end_t <= self.start && packet.duration > 0 {
            return Trimmed2::Before;
        }
        packet.pts -= (self.start / time_base.to_f64()).round() as i64;
        Trimmed2::Keep(packet)
    }
}

enum Trimmed2 {
    Before,
    Keep(Packet),
    After,
}

/// Collects stream descriptions until every stream of an output is ready,
/// then writes the header and any packets that arrived before it.
struct Sink {
    muxer: Box<dyn Muxer>,
    infos: Vec<Option<StreamInfo>>,
    pending: Vec<Packet>,
    header_written: bool,
    output_index: usize,
    path: String,
    format: &'static OutputFormat,
}

impl Sink {
    fn stream_ready(&mut self, info: StreamInfo) -> Result<()> {
        let idx = info.index;
        self.infos[idx] = Some(info);
        if !self.header_written && self.infos.iter().all(Option::is_some) {
            let infos: Vec<StreamInfo> = self.infos.iter().flatten().cloned().collect();
            self.muxer.write_header(&infos)?;
            self.header_written = true;
            log::info(format!(
                "Output #{}, {}, to '{}':",
                self.output_index, self.format.name, self.path
            ));
            for s in &infos {
                log::info(format!(
                    "  Stream #{}:{}: {}",
                    self.output_index,
                    s.index,
                    s.describe()
                ));
            }
            for p in std::mem::take(&mut self.pending) {
                self.muxer.write_packet(&p)?;
            }
        }
        Ok(())
    }

    fn write(&mut self, packet: Packet) -> Result<()> {
        if self.header_written {
            self.muxer.write_packet(&packet)
        } else {
            self.pending.push(packet);
            Ok(())
        }
    }
}

struct Transcode {
    codec: CodecId,
    filters: Vec<Box<dyn Filter>>,
    encoder: Option<Box<dyn Encoder>>,
    audio_conv: Option<AudioConverter>,
    video_target: Option<VideoFormat>,
    encoder_options: EncoderOptions,
    sample_rate: Option<u32>,
    channels: Option<u16>,
    size: Option<(u32, u32)>,
    pixel_format: Option<PixelFormat>,
    allowed_pixel_formats: &'static [PixelFormat],
    scale_algorithm: ScaleAlgorithm,
}

enum Mode {
    Copy,
    Transcode(Box<Transcode>),
}

struct OutStream {
    index: usize,
    input_index: usize,
    input_info: StreamInfo,
    trim: Option<Trim>,
    frame_limit: Option<u64>,
    frames_out: u64,
    /// Set once the output time window has ended: no more input is taken,
    /// but frames still buffered in filters are flushed and encoded.
    input_done: bool,
    /// Set once the frame limit is reached: nothing more is encoded.
    output_done: bool,
    /// End time of the last frame or packet written, for progress.
    position: f64,
    mode: Mode,
}

struct OutputCtx {
    streams: Vec<OutStream>,
    sink: Sink,
}

impl Transcode {
    fn proposed_format(&self, frame: FrameFormat) -> FrameFormat {
        match frame {
            FrameFormat::Audio(a) => FrameFormat::Audio(AudioFormat {
                sample_rate: self.sample_rate.unwrap_or(a.sample_rate),
                channels: self.channels.unwrap_or(a.channels),
                kind: a.kind,
            }),
            FrameFormat::Video(v) => {
                let (width, height) = self.size.unwrap_or((v.width, v.height));
                let mut pixel_format = self.pixel_format.unwrap_or(v.pixel_format);
                let allowed = self.allowed_pixel_formats;
                if !allowed.is_empty() && !allowed.contains(&pixel_format) {
                    pixel_format = allowed[0];
                }
                FrameFormat::Video(VideoFormat {
                    width,
                    height,
                    pixel_format,
                    frame_rate: v.frame_rate,
                })
            }
        }
    }

    fn init_encoder(&mut self, frame: FrameFormat, sink: &mut Sink) -> Result<()> {
        let proposed = self.proposed_format(frame);
        let encoder = codec::create_encoder(self.codec, &proposed, &self.encoder_options)?;
        match encoder.input_format() {
            FrameFormat::Audio(target) => self.audio_conv = Some(AudioConverter::new(target)),
            FrameFormat::Video(target) => {
                if !self.allowed_pixel_formats.is_empty()
                    && !self.allowed_pixel_formats.contains(&target.pixel_format)
                {
                    return Err(Error::unsupported(format!(
                        "{} cannot store {} pictures",
                        sink.format.name, target.pixel_format
                    )));
                }
                self.video_target = Some(target);
            }
        }
        let mut info = encoder.stream_info();
        info.index = self.encoder_options.stream_index;
        self.encoder = Some(encoder);
        sink.stream_ready(info)
    }
}

/// Proposed frame format for an input stream, used when no frame ever
/// reaches the encoder and it still has to be created.
fn format_of_stream(info: &StreamInfo) -> FrameFormat {
    match &info.params {
        StreamParams::Audio(a) => FrameFormat::Audio(AudioFormat {
            sample_rate: a.sample_rate,
            channels: a.channels,
            kind: if info.codec.pcm_bits().is_some() {
                crate::codec::pcm::sample_kind(info.codec)
            } else if (8..=32).contains(&a.bits_per_sample) {
                SampleKind::Int(a.bits_per_sample)
            } else {
                SampleKind::Float
            },
        }),
        StreamParams::Video(v) => FrameFormat::Video(VideoFormat {
            width: v.width,
            height: v.height,
            pixel_format: v.pixel_format,
            frame_rate: v.frame_rate,
        }),
    }
}

impl OutStream {
    /// Sends one decoded frame through trimming, filters, conversion and
    /// the encoder.
    fn push_frame(&mut self, frame: Frame, sink: &mut Sink) -> Result<()> {
        if self.input_done || self.output_done {
            return Ok(());
        }
        let frame = match self.trim.map(|t| t.apply(frame.clone())) {
            None => frame,
            Some(Trimmed::Keep(f)) => f,
            Some(Trimmed::Before) => return Ok(()),
            Some(Trimmed::After) => {
                self.input_done = true;
                return Ok(());
            }
        };
        let Mode::Transcode(t) = &mut self.mode else {
            return Ok(());
        };
        let filtered = filter::run_chain(&mut t.filters, frame)?;
        for f in filtered {
            self.encode(f, sink)?;
        }
        Ok(())
    }

    fn encode(&mut self, frame: Frame, sink: &mut Sink) -> Result<()> {
        let Mode::Transcode(t) = &mut self.mode else {
            return Ok(());
        };
        if self.output_done {
            return Ok(());
        }
        if t.encoder.is_none() {
            t.init_encoder(frame.format(), sink)?;
        }
        let converted: Vec<Frame> = match frame {
            Frame::Audio(a) => t
                .audio_conv
                .as_mut()
                .ok_or_else(|| Error::invalid_data("audio frame on a video stream"))?
                .convert(a)?
                .into_iter()
                .map(Frame::Audio)
                .collect(),
            Frame::Video(v) => {
                let target = t
                    .video_target
                    .ok_or_else(|| Error::invalid_data("video frame on an audio stream"))?;
                vec![Frame::Video(vconv::convert(
                    &v,
                    target.width,
                    target.height,
                    target.pixel_format,
                    t.scale_algorithm,
                )?)]
            }
        };
        for f in converted {
            if self
                .frame_limit
                .is_some_and(|limit| self.frames_out >= limit)
            {
                self.output_done = true;
                break;
            }
            self.position = self.position.max(f.end_seconds());
            let encoder = t.encoder.as_mut().unwrap();
            for p in encoder.encode(&f)? {
                sink.write(p)?;
            }
            self.frames_out += 1;
            if self
                .frame_limit
                .is_some_and(|limit| self.frames_out >= limit)
            {
                self.output_done = true;
            }
        }
        Ok(())
    }

    fn push_packet(&mut self, packet: &Packet, sink: &mut Sink) -> Result<()> {
        if self.input_done || self.output_done || !matches!(self.mode, Mode::Copy) {
            return Ok(());
        }
        let tb = self.input_info.time_base;
        let packet = match self.trim.map(|t| t.apply_packet(packet.clone(), tb)) {
            None => packet.clone(),
            Some(Trimmed2::Keep(p)) => p,
            Some(Trimmed2::Before) => return Ok(()),
            Some(Trimmed2::After) => {
                self.input_done = true;
                return Ok(());
            }
        };
        if self
            .frame_limit
            .is_some_and(|limit| self.frames_out >= limit)
        {
            self.output_done = true;
            return Ok(());
        }
        self.position = self
            .position
            .max((packet.pts + packet.duration) as f64 * tb.to_f64());
        self.frames_out += 1;
        sink.write(Packet {
            stream_index: self.index,
            ..packet
        })
    }

    /// Drains filters, converter and encoder, and returns the final stream
    /// description for the trailer.
    fn finish(&mut self, sink: &mut Sink) -> Result<StreamInfo> {
        if let Mode::Transcode(t) = &mut self.mode {
            let rest = filter::flush_chain(&mut t.filters)?;
            for f in rest {
                self.encode(f, sink)?;
            }
        }
        let Mode::Transcode(t) = &mut self.mode else {
            let mut info = self.input_info.clone();
            info.index = self.index;
            return Ok(info);
        };
        if t.encoder.is_none() {
            t.init_encoder(format_of_stream(&self.input_info), sink)?;
        }
        if let Some(conv) = t.audio_conv.as_mut() {
            let encoder = t.encoder.as_mut().unwrap();
            for f in conv.flush()? {
                for p in encoder.encode(&Frame::Audio(f))? {
                    sink.write(p)?;
                }
            }
        }
        let encoder = t.encoder.as_mut().unwrap();
        for p in encoder.flush()? {
            sink.write(p)?;
        }
        let mut info = encoder.stream_info();
        info.index = self.index;
        Ok(info)
    }
}

fn confirm_overwrite(path: &str, global: &GlobalOptions) -> Result<()> {
    if crate::io::is_pipe(path) || !std::path::Path::new(path).exists() {
        return Ok(());
    }
    match global.overwrite {
        Some(true) => Ok(()),
        Some(false) => Err(Error::invalid_argument(format!(
            "file '{path}' already exists, exiting (-n)"
        ))),
        None => {
            use std::io::{BufRead, IsTerminal, Write};
            if !std::io::stdin().is_terminal() {
                return Err(Error::invalid_argument(format!(
                    "file '{path}' already exists; use -y to overwrite"
                )));
            }
            eprint!("File '{path}' already exists. Overwrite? [y/N] ");
            std::io::stderr().flush()?;
            let mut answer = String::new();
            std::io::stdin().lock().read_line(&mut answer)?;
            if matches!(answer.trim(), "y" | "Y" | "yes") {
                Ok(())
            } else {
                Err(Error::invalid_argument("not overwriting, exiting"))
            }
        }
    }
}

fn pick_stream(input: &Input, media: MediaType) -> Option<usize> {
    input.streams.iter().position(|s| s.media_type() == media)
}

/// Prints the input block of the banner (format, duration, streams).
pub fn describe_input(index: usize, path: &str, input: &Input) {
    log::info(format!(
        "Input #{index}, {}, from '{path}':",
        input.format_name
    ));
    if !input.metadata.is_empty() {
        log::info("  Metadata:");
        for (k, v) in &input.metadata {
            log::info(format!("    {k:<16}: {v}"));
        }
    }
    let duration = input.duration().map_or("N/A".to_string(), format_duration);
    // Compressed streams rarely declare a bit rate; the file size over the
    // duration gives the overall rate, as players show it.
    let file_rate = || {
        let size = std::fs::metadata(path).ok()?.len();
        let secs = input.duration().filter(|d| *d > 0.0)?;
        Some((size as f64 * 8.0 / secs) as u64)
    };
    let bitrate = input
        .streams
        .iter()
        .map(|s| s.bit_rate)
        .sum::<Option<u64>>()
        .or_else(file_rate);
    let bitrate = bitrate.map_or("N/A".to_string(), |b| format!("{} kb/s", (b + 500) / 1000));
    log::info(format!("  Duration: {duration}, bitrate: {bitrate}"));
    for s in &input.streams {
        log::info(format!("  Stream #{index}:{}: {}", s.index, s.describe()));
    }
}

fn setup_output(
    output_index: usize,
    spec: &OutputSpec,
    input: &Input,
    global: &GlobalOptions,
    input_path: &str,
) -> Result<OutputCtx> {
    if !crate::io::is_pipe(&spec.path) && spec.path == input_path {
        return Err(Error::invalid_argument(format!(
            "output '{}' is the same file as the input",
            spec.path
        )));
    }
    let format = format::guess_output_format(&spec.path, spec.format.as_deref())?;

    let mut mapped: Vec<(usize, MediaType)> = Vec::new();
    if !spec.no_video && (format.video_codec.is_some() || spec.video_codec.is_some()) {
        if let Some(i) = pick_stream(input, MediaType::Video) {
            mapped.push((i, MediaType::Video));
        }
    }
    if !spec.no_audio && (format.audio_codec.is_some() || spec.audio_codec.is_some()) {
        if let Some(i) = pick_stream(input, MediaType::Audio) {
            mapped.push((i, MediaType::Audio));
        }
    }
    if mapped.is_empty() {
        return Err(Error::invalid_argument(format!(
            "output '{}' ({}) cannot hold any stream of the input",
            spec.path, format.name
        )));
    }

    let mut streams = Vec::new();
    let mut copy_infos = Vec::new();
    for (out_index, (input_index, media)) in mapped.into_iter().enumerate() {
        let input_info = input.streams[input_index].clone();
        let (choice, filters_spec, frame_limit) = match media {
            MediaType::Audio => (spec.audio_codec, &spec.audio_filters, spec.max_audio_frames),
            MediaType::Video => (spec.video_codec, &spec.video_filters, spec.max_video_frames),
        };
        let default_codec = match media {
            MediaType::Audio => format.audio_codec,
            MediaType::Video => format.video_codec,
        };
        let mode = match choice {
            Some(CodecChoice::Copy) => {
                if filters_spec.is_some() {
                    return Err(Error::invalid_argument(
                        "filtering and stream copy cannot be used together",
                    ));
                }
                if !format.codecs.contains(&input_info.codec) {
                    return Err(Error::unsupported(format!(
                        "{} cannot hold {} (stream copy)",
                        format.name, input_info.codec
                    )));
                }
                let mut info = input_info.clone();
                info.index = out_index;
                copy_infos.push(info);
                Mode::Copy
            }
            choice => {
                let codec = match choice {
                    Some(CodecChoice::Encode(c)) => c,
                    _ => default_codec.ok_or_else(|| {
                        Error::invalid_argument(format!(
                            "{} has no default {} codec; pick one",
                            format.name,
                            media.to_string().to_lowercase()
                        ))
                    })?,
                };
                if codec.media_type() != media {
                    return Err(Error::invalid_argument(format!(
                        "{codec} is not a {} codec",
                        media.to_string().to_lowercase()
                    )));
                }
                if !format.codecs.contains(&codec) {
                    return Err(Error::unsupported(format!(
                        "{} cannot hold {codec}",
                        format.name
                    )));
                }
                let mut chain = String::new();
                if let Some(f) = filters_spec {
                    chain.push_str(f);
                }
                if media == MediaType::Video {
                    if let Some(rate) = spec.frame_rate {
                        if !chain.is_empty() {
                            chain.push(',');
                        }
                        chain.push_str(&format!("fps={}/{}", rate.num, rate.den));
                    }
                }
                Mode::Transcode(Box::new(Transcode {
                    codec,
                    filters: filter::parse_chain(&chain, media)?,
                    encoder: None,
                    audio_conv: None,
                    video_target: None,
                    encoder_options: EncoderOptions {
                        stream_index: out_index,
                        compression_level: spec.compression_level,
                    },
                    sample_rate: spec.sample_rate,
                    channels: spec.channels,
                    size: spec.size,
                    pixel_format: spec.pixel_format,
                    allowed_pixel_formats: format.pixel_formats,
                    scale_algorithm: spec.scale_algorithm.unwrap_or_default(),
                }))
            }
        };
        streams.push(OutStream {
            index: out_index,
            input_index,
            input_info,
            trim: Trim::new(spec.start, spec.duration, spec.to),
            frame_limit,
            frames_out: 0,
            input_done: false,
            output_done: false,
            position: 0.0,
            mode,
        });
    }

    confirm_overwrite(&spec.path, global)?;
    let options = OutputOptions {
        loop_count: spec.loop_count,
        metadata: spec.metadata.clone(),
    };
    let muxer = (format.create)(&spec.path, &options)?;
    let mut sink = Sink {
        muxer,
        infos: vec![None; streams.len()],
        pending: Vec::new(),
        header_written: false,
        output_index,
        path: spec.path.clone(),
        format,
    };
    for info in copy_infos {
        sink.stream_ready(info)?;
    }
    Ok(OutputCtx { streams, sink })
}

struct Progress {
    started: Instant,
    last: Option<Instant>,
    enabled: bool,
}

impl Progress {
    fn line(&self, outputs: &[OutputCtx]) -> String {
        let bytes: u64 = outputs.iter().map(|o| o.sink.muxer.bytes_written()).sum();
        let time = outputs
            .iter()
            .flat_map(|o| o.streams.iter().map(|s| s.position))
            .fold(0.0, f64::max);
        let frames: Option<u64> = outputs
            .iter()
            .flat_map(|o| o.streams.iter())
            .find(|s| s.input_info.media_type() == MediaType::Video)
            .map(|s| s.frames_out);
        let elapsed = self.started.elapsed().as_secs_f64().max(1e-6);
        let mut line = String::new();
        if let Some(n) = frames {
            line += &format!("frame={n:>5} fps={:>5.1} ", n as f64 / elapsed);
        }
        line += &format!(
            "size={:>8}KiB time={} ",
            bytes / 1024,
            format_duration(time)
        );
        if time > 0.0 {
            line += &format!(
                "bitrate={:>7.1}kbits/s ",
                bytes as f64 * 8.0 / time / 1000.0
            );
        } else {
            line += "bitrate=N/A ";
        }
        line += &format!("speed={:>5.3}x", time / elapsed);
        line
    }

    fn tick(&mut self, outputs: &[OutputCtx]) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        if self
            .last
            .is_some_and(|l| now.duration_since(l).as_millis() < 500)
        {
            return;
        }
        self.last = Some(now);
        eprint!("{}\r", self.line(outputs));
    }

    fn done(&self, outputs: &[OutputCtx]) {
        if self.enabled {
            eprintln!("{}", self.line(outputs));
        }
    }
}

/// Runs a full transcode.
pub fn run(global: &GlobalOptions, inputs: &[InputSpec], outputs: &[OutputSpec]) -> Result<()> {
    let input_spec = match inputs {
        [one] => one,
        [] => return Err(Error::invalid_argument("no input file (use -i)")),
        _ => return Err(Error::unsupported("more than one input")),
    };
    if outputs.is_empty() {
        return Err(Error::invalid_argument(
            "at least one output file must be specified",
        ));
    }
    let mut input = Input::open(input_spec)?;
    describe_input(0, &input_spec.path, &input);

    let input_trim = Trim::new(input_spec.start, input_spec.duration, input_spec.to);
    let mut outs = Vec::new();
    for (i, spec) in outputs.iter().enumerate() {
        outs.push(setup_output(i, spec, &input, global, &input_spec.path)?);
    }

    let bounded = input_trim.is_some_and(|t| t.end.is_some())
        || outs.iter().all(|o| {
            o.streams
                .iter()
                .all(|s| s.frame_limit.is_some() || s.trim.is_some_and(|t| t.end.is_some()))
        });
    if input.is_endless() && !bounded {
        return Err(Error::invalid_argument(
            "the source never ends; limit it with -t, -to or -frames",
        ));
    }

    log::info("Stream mapping:");
    for (oi, o) in outs.iter().enumerate() {
        for s in &o.streams {
            let target = match &s.mode {
                Mode::Copy => "copy".to_string(),
                Mode::Transcode(t) => format!("{} -> {}", s.input_info.codec, t.codec),
            };
            log::info(format!(
                "  Stream #0:{} -> #{oi}:{} ({target})",
                s.input_index, s.index
            ));
        }
    }

    let decode_needed: Vec<bool> = (0..input.streams.len())
        .map(|i| {
            outs.iter().any(|o| {
                o.streams
                    .iter()
                    .any(|s| s.input_index == i && matches!(s.mode, Mode::Transcode(_)))
            })
        })
        .collect();

    let mut progress = Progress {
        started: Instant::now(),
        // Wait half a second before the first line so short jobs print
        // only the final summary, after the stream descriptions.
        last: Some(Instant::now()),
        enabled: global.stats && log::enabled(LogLevel::Info),
    };

    let mut input_done = false;
    while !input_done {
        if outs
            .iter()
            .all(|o| o.streams.iter().all(|s| s.input_done || s.output_done))
        {
            break;
        }
        let frames: Vec<(usize, Frame)> = match input.read()? {
            None => break,
            Some(Item::Packet(packet)) => {
                // Stream copy sees the packet before any input trim; the
                // window is applied per output instead.
                for o in outs.iter_mut() {
                    for s in o.streams.iter_mut() {
                        if s.input_index == packet.stream_index {
                            s.push_packet(&packet, &mut o.sink)?;
                        }
                    }
                }
                if decode_needed[packet.stream_index] {
                    input
                        .decode(&packet)?
                        .into_iter()
                        .map(|f| (packet.stream_index, f))
                        .collect()
                } else {
                    Vec::new()
                }
            }
            Some(Item::Frame(i, frame)) => vec![(i, frame)],
        };
        for (i, frame) in frames {
            if dispatch(i, frame, input_trim, &mut outs)? {
                input_done = true;
            }
        }
        progress.tick(&outs);
    }
    for (i, frame) in input.flush_decoders()? {
        dispatch(i, frame, input_trim, &mut outs)?;
    }

    for o in outs.iter_mut() {
        let mut infos = Vec::new();
        for s in o.streams.iter_mut() {
            infos.push(s.finish(&mut o.sink)?);
        }
        if !o.sink.header_written {
            o.sink.muxer.write_header(&infos)?;
            o.sink.header_written = true;
        }
        o.sink.muxer.write_trailer(&infos)?;
    }
    progress.done(&outs);
    Ok(())
}

/// Applies the input window and hands the frame to every output using its
/// stream. Returns true once the input window has ended.
fn dispatch(
    stream_index: usize,
    frame: Frame,
    input_trim: Option<Trim>,
    outs: &mut [OutputCtx],
) -> Result<bool> {
    let frame = match input_trim.map(|t| t.apply(frame.clone())) {
        None => frame,
        Some(Trimmed::Keep(f)) => f,
        Some(Trimmed::Before) => return Ok(false),
        Some(Trimmed::After) => return Ok(true),
    };
    for o in outs.iter_mut() {
        for s in o.streams.iter_mut() {
            if s.input_index == stream_index {
                s.push_frame(frame.clone(), &mut o.sink)?;
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::AudioFrame;

    fn audio(pts: i64, n: usize) -> Frame {
        Frame::Audio(AudioFrame::new(
            1000,
            1,
            AudioData::Float((0..n).map(|i| (pts as usize + i) as f32).collect()),
            pts,
        ))
    }

    #[test]
    fn audio_trim_is_sample_exact() {
        let trim = Trim::new(Some(0.25), Some(0.5), None).unwrap();
        assert!(matches!(trim.apply(audio(0, 100)), Trimmed::Before));
        let Trimmed::Keep(Frame::Audio(a)) = trim.apply(audio(200, 100)) else {
            panic!()
        };
        assert_eq!(a.pts, 0);
        assert_eq!(a.to_f32()[0], 250.0);
        assert_eq!(a.nb_samples(), 50);
        let Trimmed::Keep(Frame::Audio(a)) = trim.apply(audio(700, 100)) else {
            panic!()
        };
        assert_eq!(a.pts, 450);
        assert_eq!(a.nb_samples(), 50);
        assert!(matches!(trim.apply(audio(750, 100)), Trimmed::After));
    }

    #[test]
    fn no_window_means_no_trim() {
        assert!(Trim::new(None, None, None).is_none());
        let t = Trim::new(None, Some(2.0), Some(1.0)).unwrap();
        assert_eq!(t.end, Some(1.0));
    }
}
