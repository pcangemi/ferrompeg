//! GIF encoder: palette quantization and LZW compression (GIF89a).
//!
//! Written against the "Graphics Interchange Format, Version 89a"
//! specification (CompuServe Incorporated, 1990), sections 18 to 23 (image
//! descriptor, local color table, table based image data, graphic control
//! extension) and appendix F (variable length code LZW).
//!
//! Every packet holds one complete image, ready to be appended to a GIF
//! data stream after the logical screen descriptor written by the muxer:
//!
//! ```text
//! graphic control extension | image descriptor | local color table | LZW data
//! ```
//!
//! # Pictures
//!
//! Each picture gets its own local color table of at most 256 colors. When
//! the picture (or the part of it that is encoded) uses no more colors than
//! the table holds, the table is exactly those colors and the encoding is
//! lossless. Otherwise the palette comes from median cut over the color
//! histogram and pixels are mapped with serpentine Floyd-Steinberg error
//! diffusion, through a nearest color cache indexed by color reduced to 6
//! bits per channel. The LZW minimum code size is the index width of the
//! color table (at least 2, as the specification requires), so full 256
//! entry tables use 8 and small tables get shorter codes.
//!
//! The first picture's table is kept as a reference: the muxer makes it the
//! global color table and strips identical local tables. A later picture
//! reuses it when it renders that picture's colors with at most a quarter
//! more squared error than a fresh palette would (plus a small margin), and
//! never when a fresh table would be exact and the reference is not. A
//! transparent pixel then takes an entry of the reference table that no
//! encoded pixel uses.
//!
//! # Size optimization between pictures
//!
//! The first picture covers the whole canvas. Each later one only encodes
//! the bounding rectangle of the pixels whose source color differs from the
//! previous picture written, and every image uses disposal method 1 (leave
//! in place), so the viewer keeps everything outside the rectangle. Inside
//! the rectangle, unchanged pixels become a transparent index when they make
//! up at least one sixteenth of it: this costs one palette slot, but gives
//! the LZW coder long runs and keeps those pixels exactly as already shown.
//! Comparing source pixels rather than quantized output matters: dithering
//! noise would otherwise mark the whole picture as changed.
//!
//! A picture identical to the previous one is not written; the previous
//! image simply stays on screen longer.
//!
//! # Timing
//!
//! GIF stores per image delays in hundredths of a second. Delays come from
//! the frame timestamps rounded on an absolute centisecond grid, so the
//! rounding never accumulates: at 30 fps the delays run 3, 4, 3, 3, 4, 3 and
//! every second of video is exactly 100 centiseconds. Encoding waits for the
//! next frame (or the end of the stream) to learn how long a picture lasts.
//! A picture whose display time rounds to zero (above 100 fps) is dropped,
//! since the next one replaces it within the same centisecond. Delays above
//! 65535 centiseconds are clamped to the 16-bit field.
//!
//! Most viewers, browsers included, do not honour delays below 2
//! centiseconds: a delay of 0 or 1 is commonly replaced by about 10
//! centiseconds, so animations above 50 fps play slower than intended. The
//! encoder still writes the exact delay and leaves the frame rate to the
//! user (an `fps` filter of 50 or less avoids the issue).

use crate::codec::{Encoder, EncoderOptions};
use crate::error::{Error, Result};
use crate::frame::{Frame, FrameFormat, PixelFormat, VideoFormat};
use crate::rational::Rational;
use crate::stream::{CodecId, Packet, StreamInfo, StreamParams, VideoParams};

/// LZW codes are at most 12 bits wide, so the code table holds 4096 entries.
const MAX_CODES: usize = 4096;
const MAX_CODE_SIZE: u32 = 12;

/// Image dimensions and positions are 16-bit fields.
const MAX_DIMENSION: u32 = 65535;

/// Graphic control extension delays are 16-bit too.
const MAX_DELAY: i64 = 65535;

/// Image data sub-blocks carry at most 255 bytes after their length byte.
const SUB_BLOCK: usize = 255;

/// Bits per channel of the nearest color cache key.
const CACHE_BITS: u32 = 6;
const CACHE_SIZE: usize = 1 << (3 * CACHE_BITS);
const CACHE_EMPTY: u16 = u16::MAX;

/// Histograms with more distinct colors than this are reduced to 6 bits per
/// channel before median cut, which bounds its sorting work on large
/// pictures. Each bin keeps exact sums, so palette colors stay accurate.
const COARSE_LIMIT: usize = 32768;

/// How much more squared error than a fresh palette the global color
/// table may cause and still be reused, as a factor and as a margin per
/// pixel.
const GLOBAL_TOLERANCE: f64 = 1.25;
const GLOBAL_MARGIN: f64 = 2.0;

/// Disposal method 1 from the graphic control extension: leave the image
/// in place, so later images only need to cover what changed.
const DISPOSAL_KEEP: u8 = 1;

pub fn new_encoder(
    codec: CodecId,
    proposed: &FrameFormat,
    options: &EncoderOptions,
) -> Result<Box<dyn Encoder>> {
    if codec != CodecId::Gif {
        return Err(Error::invalid_argument(format!(
            "gif encoder cannot produce {codec}"
        )));
    }
    let FrameFormat::Video(v) = proposed else {
        return Err(Error::invalid_argument("gif encoder needs video input"));
    };
    Ok(Box::new(GifEncoder::new(
        v.width,
        v.height,
        v.frame_rate,
        options.stream_index,
    )?))
}

/// A picture waiting for the next frame, which decides its delay.
struct Pending {
    pixels: Vec<u8>,
    pts: i64,
    /// Timestamp of the last identical frame folded into this picture.
    last_pts: i64,
}

/// Part of the canvas an image covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

pub struct GifEncoder {
    format: VideoFormat,
    time_base: Rational,
    stream_index: usize,
    /// Source pixels of the last picture written: what the viewer shows,
    /// before quantization.
    previous: Option<Vec<u8>>,
    pending: Option<Pending>,
    /// Color table of the first picture, which the muxer turns into the
    /// global color table; later pictures reuse it when it suits them.
    global: Option<Vec<[u8; 3]>>,
    /// Nearest palette index per reduced color, kept to reuse the buffer.
    cache: Vec<u16>,
}

impl GifEncoder {
    pub fn new(width: u32, height: u32, frame_rate: Rational, stream_index: usize) -> Result<Self> {
        if !(1..=MAX_DIMENSION).contains(&width) || !(1..=MAX_DIMENSION).contains(&height) {
            return Err(Error::unsupported(format!(
                "GIF pictures are 1 to {MAX_DIMENSION} pixels wide and high, not {width}x{height}"
            )));
        }
        if frame_rate.num <= 0 || frame_rate.den <= 0 {
            return Err(Error::invalid_argument(
                "gif encoder needs a positive frame rate",
            ));
        }
        Ok(GifEncoder {
            format: VideoFormat {
                width,
                height,
                pixel_format: PixelFormat::Rgb24,
                frame_rate,
            },
            time_base: frame_rate.inv(),
            stream_index,
            previous: None,
            pending: None,
            global: None,
            cache: Vec::new(),
        })
    }

    /// Position of frame `pts` on the absolute centisecond grid.
    fn centiseconds(&self, pts: i64) -> i64 {
        Rational::rescale(pts, self.time_base, Rational::new(1, 100))
    }

    /// Encodes the pending picture, shown from `pending.pts` to `end_pts`.
    fn emit(&mut self, pending: Pending, end_pts: i64, delay: i64) -> Packet {
        let delay = delay.clamp(1, MAX_DELAY) as u16;
        let keyframe = self.previous.is_none();
        let data = self.encode_picture(&pending.pixels, delay);
        self.previous = Some(pending.pixels);
        let mut packet = Packet::new(
            self.stream_index,
            data,
            pending.pts,
            (end_pts - pending.pts).max(1),
        );
        // Later images only patch the canvas left by the earlier ones.
        packet.keyframe = keyframe;
        packet
    }

    fn encode_picture(&mut self, pixels: &[u8], delay: u16) -> Vec<u8> {
        let width = self.format.width as usize;
        let height = self.format.height as usize;
        let full = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        // `opaque` marks the pixels of the rectangle that must be encoded;
        // `None` means all of them.
        let (rect, opaque) = match &self.previous {
            None => (full, None),
            Some(prev) => match changed_rect(prev, pixels, width, height) {
                None => {
                    // Nothing changed but time still has to pass: a single
                    // transparent pixel carries the delay.
                    let rect = Rect {
                        x: 0,
                        y: 0,
                        width: 1,
                        height: 1,
                    };
                    (rect, Some(vec![false]))
                }
                Some(rect) => {
                    let mask = changed_mask(prev, pixels, width, rect);
                    let unchanged = mask.iter().filter(|&&m| !m).count();
                    if unchanged > 0 && unchanged * 16 >= mask.len() {
                        (rect, Some(mask))
                    } else {
                        (rect, None)
                    }
                }
            },
        };

        let mapped = self.map_picture(pixels, width, rect, opaque.as_deref());
        if self.global.is_none() {
            self.global = Some(mapped.table.clone());
        }
        let size_bits = table_size_bits(mapped.table.len());
        let min_code_size = (size_bits as u8 + 1).max(2);

        let mut out = Vec::with_capacity(32 + 3 * mapped.table.len() + mapped.indices.len() / 2);
        // Graphic control extension.
        out.extend_from_slice(&[0x21, 0xF9, 0x04]);
        out.push((DISPOSAL_KEEP << 2) | mapped.transparent.is_some() as u8);
        out.extend_from_slice(&delay.to_le_bytes());
        out.push(mapped.transparent.unwrap_or(0));
        out.push(0x00);
        // Image descriptor: local color table present, not interlaced.
        out.push(0x2C);
        for v in [rect.x, rect.y, rect.width, rect.height] {
            out.extend_from_slice(&(v as u16).to_le_bytes());
        }
        out.push(0x80 | size_bits as u8);
        for c in &mapped.table {
            out.extend_from_slice(c);
        }
        let indices = mapped.indices;
        // Indices are below the table size by construction, so this
        // cannot fail.
        if let Err(e) = lzw_compress(&indices, min_code_size, &mut out) {
            debug_assert!(false, "{e}");
        }
        out
    }
}

impl GifEncoder {
    /// Chooses a color table for the pixels of `rect` (those in `opaque`
    /// only, when given) and maps them to it.
    fn map_picture(
        &mut self,
        pixels: &[u8],
        width: usize,
        rect: Rect,
        opaque: Option<&[bool]>,
    ) -> Mapped {
        let keys = rect_keys(pixels, width, rect, opaque);
        let bins = histogram(&keys);
        let slots = if opaque.is_some() { 255 } else { 256 };
        let (palette, exact) = build_palette(&bins, slots);

        // Reusing the first picture's table lets the muxer drop the local
        // table altogether (it becomes the global one), saving up to 768
        // bytes per picture. Only do it when the table renders these
        // colors about as well as a fresh palette, and never at the cost
        // of an exact rendering.
        if let Some(global) = self.global.take() {
            let reused = if global_is_good_enough(&global, &bins, &palette, exact) {
                self.map_to_global(&global, exact, pixels, width, rect, opaque)
            } else {
                None
            };
            self.global = Some(global);
            if let Some(m) = reused {
                return m;
            }
        }

        let mut indices = self.map_indices(&palette, exact, pixels, width, rect, opaque);
        // A fresh palette keeps the slot after its colors for transparency.
        let transparent = opaque.map(|mask| {
            let index = palette.len() as u8;
            set_transparent(&mut indices, mask, index);
            index
        });
        let mut table = palette;
        let used = table.len() + transparent.is_some() as usize;
        table.resize(2usize << table_size_bits(used), [0, 0, 0]);
        Mapped {
            table,
            indices,
            transparent,
        }
    }

    /// Maps the picture to the global table. When some pixels must be
    /// transparent, the transparent index is an entry no encoded pixel
    /// uses; returns `None` if every entry is taken.
    fn map_to_global(
        &mut self,
        global: &[[u8; 3]],
        exact: bool,
        pixels: &[u8],
        width: usize,
        rect: Rect,
        opaque: Option<&[bool]>,
    ) -> Option<Mapped> {
        let mut indices = self.map_indices(global, exact, pixels, width, rect, opaque);
        let transparent = match opaque {
            None => None,
            Some(mask) => {
                let mut used = [false; 256];
                for (&i, &keep) in indices.iter().zip(mask) {
                    if keep {
                        used[i as usize] = true;
                    }
                }
                let free = (0..global.len()).find(|&i| !used[i])? as u8;
                set_transparent(&mut indices, mask, free);
                Some(free)
            }
        };
        Some(Mapped {
            table: global.to_vec(),
            indices,
            transparent,
        })
    }

    /// Maps pixels to `palette`: by lookup when `exact` says every color
    /// is in it, with error diffusion otherwise. Pixels outside `opaque`
    /// get index 0 until the caller picks a transparent index.
    fn map_indices(
        &mut self,
        palette: &[[u8; 3]],
        exact: bool,
        pixels: &[u8],
        width: usize,
        rect: Rect,
        opaque: Option<&[bool]>,
    ) -> Vec<u8> {
        if exact {
            let mut lookup: Vec<(u32, u8)> = palette
                .iter()
                .enumerate()
                .map(|(i, c)| (color_key(c), i as u8))
                .collect();
            // Stable sort and dedup keep the first index of a repeated
            // color (padding entries are black).
            lookup.sort_by_key(|&(k, _)| k);
            lookup.dedup_by_key(|(k, _)| *k);
            return map_exact(pixels, width, rect, opaque, &lookup);
        }
        if self.cache.len() != CACHE_SIZE {
            self.cache = vec![CACHE_EMPTY; CACHE_SIZE];
        } else {
            self.cache.fill(CACHE_EMPTY);
        }
        dither(pixels, width, rect, opaque, palette, &mut self.cache)
    }
}

/// A picture mapped to a color table.
struct Mapped {
    /// The color table as written, its length a power of two.
    table: Vec<[u8; 3]>,
    indices: Vec<u8>,
    transparent: Option<u8>,
}

/// Gives every pixel outside `opaque` the transparent `index`.
fn set_transparent(indices: &mut [u8], opaque: &[bool], index: u8) {
    for (i, &keep) in indices.iter_mut().zip(opaque) {
        if !keep {
            *i = index;
        }
    }
}

/// Sum over the histogram of squared distances to the nearest palette
/// entry.
fn mapping_error(palette: &[[u8; 3]], bins: &[Bin]) -> u64 {
    bins.iter()
        .map(|b| {
            let c = b.mean();
            let p = palette[nearest(palette, c)];
            let d: i32 = (0..3).map(|i| (p[i] as i32 - c[i]).pow(2)).sum();
            d as u64 * b.count
        })
        .sum()
}

/// Whether `global` renders the colors of `bins` nearly as well as the
/// fresh `palette` built for them (exact when `exact`).
fn global_is_good_enough(
    global: &[[u8; 3]],
    bins: &[Bin],
    palette: &[[u8; 3]],
    exact: bool,
) -> bool {
    if global.is_empty() {
        return false;
    }
    let bins = if bins.len() > COARSE_LIMIT {
        coarsen(bins)
    } else {
        bins.to_vec()
    };
    let with_global = mapping_error(global, &bins);
    if exact || palette.is_empty() {
        return with_global == 0;
    }
    let with_fresh = mapping_error(palette, &bins);
    let count: u64 = bins.iter().map(|b| b.count).sum();
    // Allow a quarter more error, plus a small absolute margin so nearly
    // perfect fresh palettes do not always win.
    with_global as f64 <= with_fresh as f64 * GLOBAL_TOLERANCE + count as f64 * GLOBAL_MARGIN
}

impl Encoder for GifEncoder {
    fn input_format(&self) -> FrameFormat {
        FrameFormat::Video(self.format)
    }

    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        let Frame::Video(frame) = frame else {
            return Err(Error::invalid_argument("gif encoder got an audio frame"));
        };
        if frame.format != PixelFormat::Rgb24
            || frame.width != self.format.width
            || frame.height != self.format.height
        {
            return Err(Error::invalid_data(format!(
                "gif encoder expects {}x{} rgb24 pictures, got {}x{} {}",
                self.format.width, self.format.height, frame.width, frame.height, frame.format
            )));
        }
        frame.validate()?;
        let pixels = &frame.planes[0];
        let pts = frame.pts;

        let Some(mut pending) = self.pending.take() else {
            self.pending = Some(Pending {
                pixels: pixels.clone(),
                pts,
                last_pts: pts,
            });
            return Ok(Vec::new());
        };
        if pending.pixels == *pixels {
            // Same picture: keep the pending one on screen longer.
            pending.last_pts = pending.last_pts.max(pts);
            self.pending = Some(pending);
            return Ok(Vec::new());
        }
        let delay = self.centiseconds(pts) - self.centiseconds(pending.pts);
        let next = Pending {
            pixels: pixels.clone(),
            pts,
            last_pts: pts,
        };
        if delay <= 0 {
            // The pending picture would be shown for no time at all.
            self.pending = Some(next);
            return Ok(Vec::new());
        }
        let packet = self.emit(pending, pts, delay);
        self.pending = Some(next);
        Ok(vec![packet])
    }

    fn flush(&mut self) -> Result<Vec<Packet>> {
        let Some(pending) = self.pending.take() else {
            return Ok(Vec::new());
        };
        let end = pending.last_pts + 1;
        let delay = self.centiseconds(end) - self.centiseconds(pending.pts);
        Ok(vec![self.emit(pending, end, delay)])
    }

    fn stream_info(&self) -> StreamInfo {
        StreamInfo::new(
            self.stream_index,
            CodecId::Gif,
            StreamParams::Video(VideoParams {
                width: self.format.width,
                height: self.format.height,
                pixel_format: PixelFormat::Rgb24,
                frame_rate: self.format.frame_rate,
                sample_aspect_ratio: Rational::new(1, 1),
            }),
            self.time_base,
        )
    }
}

/// Packs a color into one integer, red in the high byte, so sorting the
/// keys sorts colors and equal colors compare equal.
fn color_key(c: &[u8]) -> u32 {
    ((c[0] as u32) << 16) | ((c[1] as u32) << 8) | c[2] as u32
}

fn key_color(k: u32) -> [u8; 3] {
    [(k >> 16) as u8, (k >> 8) as u8, k as u8]
}

/// Smallest rectangle holding every pixel that differs between `a` and `b`.
fn changed_rect(a: &[u8], b: &[u8], width: usize, height: usize) -> Option<Rect> {
    let row = width * 3;
    let row_differs = |y: usize| a[y * row..(y + 1) * row] != b[y * row..(y + 1) * row];
    let top = (0..height).find(|&y| row_differs(y))?;
    let bottom = (top..height).rev().find(|&y| row_differs(y))?;
    let mut left = width;
    let mut right = 0;
    for y in top..=bottom {
        let ra = &a[y * row..(y + 1) * row];
        let rb = &b[y * row..(y + 1) * row];
        let differs = |x: &usize| ra[x * 3..x * 3 + 3] != rb[x * 3..x * 3 + 3];
        if let Some(l) = (0..left).find(differs) {
            left = l;
        }
        if let Some(r) = (right..width).rev().find(differs) {
            right = r;
        }
    }
    Some(Rect {
        x: left,
        y: top,
        width: right + 1 - left,
        height: bottom + 1 - top,
    })
}

/// For each pixel of `rect`, whether it differs between `a` and `b`.
fn changed_mask(a: &[u8], b: &[u8], width: usize, rect: Rect) -> Vec<bool> {
    let mut mask = Vec::with_capacity(rect.width * rect.height);
    for y in rect.y..rect.y + rect.height {
        let start = (y * width + rect.x) * 3;
        let end = start + rect.width * 3;
        mask.extend(
            a[start..end]
                .chunks_exact(3)
                .zip(b[start..end].chunks_exact(3))
                .map(|(p, q)| p != q),
        );
    }
    mask
}

/// Color keys of the pixels of `rect` that will be encoded.
fn rect_keys(pixels: &[u8], width: usize, rect: Rect, opaque: Option<&[bool]>) -> Vec<u32> {
    let mut keys = Vec::with_capacity(rect.width * rect.height);
    for (row, y) in (rect.y..rect.y + rect.height).enumerate() {
        let start = (y * width + rect.x) * 3;
        for (col, p) in pixels[start..start + rect.width * 3]
            .chunks_exact(3)
            .enumerate()
        {
            if opaque.is_none_or(|m| m[row * rect.width + col]) {
                keys.push(color_key(p));
            }
        }
    }
    keys
}

/// Number of bits `n` in the color table size field, the table holding
/// `2^(n+1)` entries: the smallest table with room for `colors`.
fn table_size_bits(colors: usize) -> u32 {
    (0..7).find(|&n| 2usize << n >= colors).unwrap_or(7)
}

/// One histogram entry: a color (or a group of close colors) and the
/// statistics median cut needs.
#[derive(Clone, Copy, Debug)]
struct Bin {
    count: u64,
    sum: [u64; 3],
    sum_sq: [u64; 3],
    /// Representative key, used to break ties so sorting is deterministic.
    key: u32,
}

impl Bin {
    fn new(key: u32, count: u64) -> Self {
        let c = key_color(key);
        Bin {
            count,
            sum: c.map(|v| v as u64 * count),
            sum_sq: c.map(|v| (v as u64) * (v as u64) * count),
            key,
        }
    }

    fn merge(&mut self, other: &Bin) {
        self.count += other.count;
        for c in 0..3 {
            self.sum[c] += other.sum[c];
            self.sum_sq[c] += other.sum_sq[c];
        }
    }

    fn mean(&self) -> [i32; 3] {
        let n = self.count.max(1);
        self.sum.map(|s| ((s + n / 2) / n) as i32)
    }

    /// Mean of one channel, scaled by 256 to keep sub-unit precision when
    /// sorting.
    fn mean_fixed(&self, channel: usize) -> u64 {
        (self.sum[channel] << 8) / self.count.max(1)
    }
}

/// Statistics of a run of bins: total count, per channel sums and squared
/// error around the mean.
struct BoxStats {
    count: u64,
    sum: [u64; 3],
    error: [f64; 3],
}

fn box_stats(bins: &[Bin]) -> BoxStats {
    let mut count = 0u64;
    let mut sum = [0u64; 3];
    let mut sum_sq = [0u64; 3];
    for b in bins {
        count += b.count;
        for c in 0..3 {
            sum[c] += b.sum[c];
            sum_sq[c] += b.sum_sq[c];
        }
    }
    let n = count.max(1) as f64;
    let error = [0, 1, 2].map(|c| (sum_sq[c] as f64 - (sum[c] as f64).powi(2) / n).max(0.0));
    BoxStats { count, sum, error }
}

/// Distinct colors of `keys` with their counts, sorted by key.
fn histogram(keys: &[u32]) -> Vec<Bin> {
    let mut sorted = keys.to_vec();
    sorted.sort_unstable();
    let mut bins: Vec<Bin> = Vec::new();
    let mut i = 0;
    while i < sorted.len() {
        let key = sorted[i];
        let run = sorted[i..].iter().take_while(|&&k| k == key).count();
        bins.push(Bin::new(key, run as u64));
        i += run;
    }
    bins
}

/// Builds a palette of at most `max_colors` entries for a histogram.
/// Returns the palette and whether it holds every color exactly.
fn build_palette(bins: &[Bin], max_colors: usize) -> (Vec<[u8; 3]>, bool) {
    if bins.len() <= max_colors {
        return (bins.iter().map(|b| key_color(b.key)).collect(), true);
    }
    let mut work = if bins.len() > COARSE_LIMIT {
        coarsen(bins)
    } else {
        bins.to_vec()
    };
    (median_cut(&mut work, max_colors), false)
}

/// Groups bins by color reduced to 6 bits per channel.
fn coarsen(bins: &[Bin]) -> Vec<Bin> {
    let reduce = |k: u32| {
        let [r, g, b] = key_color(k);
        ((r as u32 >> 2) << 12) | ((g as u32 >> 2) << 6) | (b as u32 >> 2)
    };
    let mut keyed: Vec<(u32, Bin)> = bins.iter().map(|b| (reduce(b.key), *b)).collect();
    keyed.sort_unstable_by_key(|(q, b)| (*q, b.key));
    let mut out: Vec<Bin> = Vec::new();
    let mut last = None;
    for (q, b) in keyed {
        if last == Some(q) {
            if let Some(o) = out.last_mut() {
                o.merge(&b);
            }
        } else {
            out.push(b);
            last = Some(q);
        }
    }
    out
}

/// Median cut: repeatedly splits the box with the largest squared error
/// along its widest channel at the weighted median, then takes the mean
/// color of each box.
fn median_cut(bins: &mut [Bin], max_colors: usize) -> Vec<[u8; 3]> {
    struct ColorBox {
        start: usize,
        end: usize,
        stats: BoxStats,
    }
    let mut boxes = vec![ColorBox {
        start: 0,
        end: bins.len(),
        stats: box_stats(bins),
    }];
    while boxes.len() < max_colors {
        // The first box wins ties, which keeps the result deterministic.
        let mut best: Option<(usize, f64)> = None;
        for (i, b) in boxes.iter().enumerate() {
            let err: f64 = b.stats.error.iter().sum();
            if b.end - b.start >= 2 && err > 0.0 && best.is_none_or(|(_, e)| err > e) {
                best = Some((i, err));
            }
        }
        let Some((index, _)) = best else {
            break;
        };
        let ColorBox { start, end, stats } = boxes.swap_remove(index);
        let channel = (0..3)
            .max_by(|&a, &b| stats.error[a].total_cmp(&stats.error[b]).then(b.cmp(&a)))
            .unwrap_or(0);
        let slice = &mut bins[start..end];
        slice.sort_unstable_by_key(|b| (b.mean_fixed(channel), b.key));
        let half = stats.count.div_ceil(2);
        let mut acc = 0;
        let mut split = slice.len() - 1;
        for (i, b) in slice.iter().enumerate() {
            acc += b.count;
            if acc >= half {
                split = i + 1;
                break;
            }
        }
        let split = start + split.clamp(1, slice.len() - 1);
        boxes.push(ColorBox {
            start,
            end: split,
            stats: box_stats(&bins[start..split]),
        });
        boxes.push(ColorBox {
            start: split,
            end,
            stats: box_stats(&bins[split..end]),
        });
    }
    // Order the palette by box position for a stable, readable layout.
    boxes.sort_unstable_by_key(|b| b.start);
    boxes
        .iter()
        .map(|b| {
            let n = b.stats.count.max(1);
            b.stats.sum.map(|s| ((s + n / 2) / n).min(255) as u8)
        })
        .collect()
}

/// Maps pixels whose colors all appear in `lookup`, a list of color keys
/// and palette indices sorted by key.
fn map_exact(
    pixels: &[u8],
    width: usize,
    rect: Rect,
    opaque: Option<&[bool]>,
    lookup: &[(u32, u8)],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(rect.width * rect.height);
    for (row, y) in (rect.y..rect.y + rect.height).enumerate() {
        let start = (y * width + rect.x) * 3;
        for (col, p) in pixels[start..start + rect.width * 3]
            .chunks_exact(3)
            .enumerate()
        {
            let index = if opaque.is_none_or(|m| m[row * rect.width + col]) {
                let key = color_key(p);
                lookup
                    .binary_search_by_key(&key, |&(k, _)| k)
                    .map_or(0, |i| lookup[i].1)
            } else {
                0
            };
            out.push(index);
        }
    }
    out
}

/// Index of the palette entry closest to `c` in RGB distance.
fn nearest(palette: &[[u8; 3]], c: [i32; 3]) -> usize {
    let mut best = 0;
    let mut best_dist = i32::MAX;
    for (i, p) in palette.iter().enumerate() {
        let dr = p[0] as i32 - c[0];
        let dg = p[1] as i32 - c[1];
        let db = p[2] as i32 - c[2];
        let d = dr * dr + dg * dg + db * db;
        if d < best_dist {
            best_dist = d;
            best = i;
        }
    }
    best
}

/// Maps the pixels of `rect` to `palette` with Floyd-Steinberg error
/// diffusion, scanning rows alternately left to right and right to left
/// so the error does not drift in one direction. Pixels outside `opaque`
/// neither receive nor spread error: the viewer keeps showing what it
/// had there.
fn dither(
    pixels: &[u8],
    width: usize,
    rect: Rect,
    opaque: Option<&[bool]>,
    palette: &[[u8; 3]],
    cache: &mut [u16],
) -> Vec<u8> {
    let rw = rect.width;
    let mut out = vec![0u8; rw * rect.height];
    // Error rows in sixteenths, with one pad pixel on each side so the
    // kernel never needs bounds checks.
    let mut current = vec![0i32; (rw + 2) * 3];
    let mut next = vec![0i32; (rw + 2) * 3];
    let shift = 8 - CACHE_BITS;
    let half_cell = 1i32 << (shift - 1);
    for row in 0..rect.height {
        let src_start = ((rect.y + row) * width + rect.x) * 3;
        let src = &pixels[src_start..src_start + rw * 3];
        let forward = row % 2 == 0;
        for step in 0..rw {
            let col = if forward { step } else { rw - 1 - step };
            if opaque.is_some_and(|m| !m[row * rw + col]) {
                continue;
            }
            let e = (col + 1) * 3;
            let mut target = [0i32; 3];
            for c in 0..3 {
                target[c] = (src[col * 3 + c] as i32 + current[e + c] / 16).clamp(0, 255);
            }
            let key = ((target[0] as usize >> shift) << (2 * CACHE_BITS))
                | ((target[1] as usize >> shift) << CACHE_BITS)
                | (target[2] as usize >> shift);
            let index = match cache[key] {
                CACHE_EMPTY => {
                    // Resolve the center of the reduced cell, so the cache
                    // does not depend on which pixel filled it first.
                    let center = target.map(|v| ((v >> shift) << shift) + half_cell);
                    let i = nearest(palette, center);
                    cache[key] = i as u16;
                    i
                }
                i => i as usize,
            };
            out[row * rw + col] = index as u8;
            let chosen = palette[index];
            // Neighbor offsets mirror on right to left rows.
            let (ahead, behind) = if forward {
                (e + 3, e - 3)
            } else {
                (e - 3, e + 3)
            };
            for c in 0..3 {
                let err = target[c] - chosen[c] as i32;
                current[ahead + c] += err * 7;
                next[behind + c] += err * 3;
                next[e + c] += err * 5;
                next[ahead + c] += err;
            }
        }
        std::mem::swap(&mut current, &mut next);
        next.fill(0);
    }
    out
}

/// Accumulates variable width codes, least significant bit first, as the
/// GIF specification packs them.
struct BitWriter {
    bytes: Vec<u8>,
    acc: u32,
    bits: u32,
}

impl BitWriter {
    fn put(&mut self, code: u16, width: u32) {
        self.acc |= (code as u32) << self.bits;
        self.bits += width;
        while self.bits >= 8 {
            self.bytes.push(self.acc as u8);
            self.acc >>= 8;
            self.bits -= 8;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.bits > 0 {
            self.bytes.push(self.acc as u8);
        }
        self.bytes
    }
}

/// Writes table based image data: the LZW minimum code size, the
/// compressed indices in data sub-blocks of at most 255 bytes, and the
/// block terminator.
///
/// The code width starts at `min_code_size + 1` bits and grows by one each
/// time a new table entry needs it, up to 12 bits. When the table is full
/// a clear code resets it. Every index must be below `2^min_code_size`.
pub(crate) fn lzw_compress(indices: &[u8], min_code_size: u8, out: &mut Vec<u8>) -> Result<()> {
    if !(2..=8).contains(&min_code_size) {
        return Err(Error::invalid_argument(
            "LZW minimum code size must be 2 to 8",
        ));
    }
    let alphabet = 1usize << min_code_size;
    if indices.iter().any(|&i| i as usize >= alphabet) {
        return Err(Error::invalid_argument(
            "color index outside the LZW alphabet",
        ));
    }
    let clear = alphabet as u16;
    let end = clear + 1;
    let first_free = clear + 2;
    let initial_width = min_code_size as u32 + 1;

    // Code table as a trie: child[prefix * alphabet + index] is the code of
    // the string `prefix` followed by `index`, 0 when absent (0 is a root
    // code and can never be a child).
    let mut child = vec![0u16; MAX_CODES * alphabet];
    let mut filled: Vec<usize> = Vec::with_capacity(MAX_CODES);
    let mut writer = BitWriter {
        bytes: Vec::with_capacity(indices.len() / 2 + 16),
        acc: 0,
        bits: 0,
    };
    let mut width = initial_width;
    let mut next = first_free;

    writer.put(clear, width);
    if let Some((&first, rest)) = indices.split_first() {
        let mut prefix = first as u16;
        for &index in rest {
            let slot = prefix as usize * alphabet + index as usize;
            if child[slot] != 0 {
                prefix = child[slot];
                continue;
            }
            writer.put(prefix, width);
            // The decoder adds its entry for this code only after reading
            // the next one, so the width grows once the entry about to be
            // added no longer fits.
            if next as u32 >= 1 << width && width < MAX_CODE_SIZE {
                width += 1;
            }
            if (next as usize) < MAX_CODES {
                child[slot] = next;
                filled.push(slot);
                next += 1;
            } else {
                writer.put(clear, width);
                for s in filled.drain(..) {
                    child[s] = 0;
                }
                width = initial_width;
                next = first_free;
            }
            prefix = index as u16;
        }
        writer.put(prefix, width);
        if next as u32 >= 1 << width && width < MAX_CODE_SIZE {
            width += 1;
        }
    }
    writer.put(end, width);

    out.push(min_code_size);
    for block in writer.finish().chunks(SUB_BLOCK) {
        out.push(block.len() as u8);
        out.extend_from_slice(block);
    }
    out.push(0x00);
    Ok(())
}

/// Test helpers shared with the muxer tests: an LZW decoder and a GIF
/// reader written from the specification, independent of the encoder.
#[cfg(test)]
pub(crate) mod testing {
    /// Decodes table based image data starting at `data[*pos]` (the LZW
    /// minimum code size byte) and advances `pos` past the terminator.
    pub fn lzw_decode(data: &[u8], pos: &mut usize) -> Result<Vec<u8>, String> {
        let byte = |p: usize| data.get(p).copied().ok_or("truncated image data");
        let min = byte(*pos)? as u32;
        *pos += 1;
        if !(2..=8).contains(&min) {
            return Err(format!("bad minimum code size {min}"));
        }
        let mut raw = Vec::new();
        loop {
            let len = byte(*pos)? as usize;
            *pos += 1;
            if len == 0 {
                break;
            }
            let block = data.get(*pos..*pos + len).ok_or("truncated sub-block")?;
            raw.extend_from_slice(block);
            *pos += len;
        }

        let clear = 1usize << min;
        let end = clear + 1;
        let roots = || {
            let mut t: Vec<Vec<u8>> = (0..clear).map(|i| vec![i as u8]).collect();
            t.push(Vec::new());
            t.push(Vec::new());
            t
        };
        let mut table = roots();
        let mut width = min + 1;
        let mut prev: Option<usize> = None;
        let mut out = Vec::new();
        let mut bit = 0usize;
        loop {
            if bit + width as usize > raw.len() * 8 {
                return Err("image data ends before the end code".into());
            }
            let mut code = 0usize;
            for i in 0..width as usize {
                let b = bit + i;
                if raw[b / 8] >> (b % 8) & 1 == 1 {
                    code |= 1 << i;
                }
            }
            bit += width as usize;
            if code == clear {
                table = roots();
                width = min + 1;
                prev = None;
                continue;
            }
            if code == end {
                break;
            }
            let entry = if code < table.len() {
                table[code].clone()
            } else if let (true, Some(p)) = (code == table.len(), prev) {
                let mut s = table[p].clone();
                s.push(table[p][0]);
                s
            } else {
                return Err(format!("code {code} not in table of {}", table.len()));
            };
            if entry.is_empty() {
                return Err("empty entry".into());
            }
            out.extend_from_slice(&entry);
            if let Some(p) = prev {
                if table.len() < 4096 {
                    let mut s = table[p].clone();
                    s.push(entry[0]);
                    table.push(s);
                    if table.len() == 1 << width && width < 12 {
                        width += 1;
                    }
                }
            }
            prev = Some(code);
        }
        // The end code must sit in the last data byte: no stray bytes.
        if bit.div_ceil(8) != raw.len() {
            return Err(format!(
                "{} bytes of data after the end code",
                raw.len() - bit.div_ceil(8)
            ));
        }
        Ok(out)
    }

    /// One image with its graphic control extension.
    #[derive(Debug)]
    pub struct Image {
        pub left: usize,
        pub top: usize,
        pub width: usize,
        pub height: usize,
        pub delay: u16,
        pub disposal: u8,
        pub transparent: Option<u8>,
        pub palette: Vec<[u8; 3]>,
        /// Whether the image carries a local color table.
        pub local: bool,
        pub indices: Vec<u8>,
    }

    /// Reads one encoder packet (or the same bytes inside a file) starting
    /// at `data[*pos]`.
    pub fn read_image(data: &[u8], pos: &mut usize) -> Image {
        read_image_with(data, pos, None)
    }

    /// Same, for an image that may use the file's global color table.
    pub fn read_image_with(data: &[u8], pos: &mut usize, global: Option<&[[u8; 3]]>) -> Image {
        let gce = &data[*pos..*pos + 8];
        assert_eq!(&gce[..3], &[0x21, 0xF9, 0x04], "graphic control extension");
        assert_eq!(gce[7], 0, "block terminator");
        let disposal = (gce[3] >> 2) & 7;
        let transparent = (gce[3] & 1 == 1).then_some(gce[6]);
        let delay = u16::from_le_bytes([gce[4], gce[5]]);
        *pos += 8;
        let d = &data[*pos..*pos + 10];
        assert_eq!(d[0], 0x2C, "image separator");
        let field = |i: usize| u16::from_le_bytes([d[i], d[i + 1]]) as usize;
        let (left, top, width, height) = (field(1), field(3), field(5), field(7));
        assert_eq!(d[9] & 0x40, 0, "not interlaced");
        let packed = d[9];
        *pos += 10;
        let palette = if packed & 0x80 != 0 {
            read_table(data, pos, packed)
        } else {
            assert_eq!(packed & 0x27, 0, "no table size without a table");
            global.expect("image without any color table").to_vec()
        };
        let indices = lzw_decode(data, pos).unwrap();
        assert_eq!(indices.len(), width * height, "pixel count");
        Image {
            left,
            top,
            width,
            height,
            delay,
            disposal,
            transparent,
            palette,
            local: packed & 0x80 != 0,
            indices,
        }
    }

    /// Reads a color table whose size is in the low bits of `packed`.
    pub fn read_table(data: &[u8], pos: &mut usize, packed: u8) -> Vec<[u8; 3]> {
        let len = 2usize << (packed & 7);
        let table = data[*pos..*pos + 3 * len]
            .chunks_exact(3)
            .map(|c| [c[0], c[1], c[2]])
            .collect();
        *pos += 3 * len;
        table
    }

    /// Paints `image` onto an RGB canvas `canvas_width` pixels wide,
    /// skipping transparent pixels.
    pub fn composite(canvas: &mut [u8], canvas_width: usize, image: &Image) {
        for row in 0..image.height {
            for col in 0..image.width {
                let index = image.indices[row * image.width + col];
                if image.transparent == Some(index) {
                    continue;
                }
                let at = ((image.top + row) * canvas_width + image.left + col) * 3;
                canvas[at..at + 3].copy_from_slice(&image.palette[index as usize]);
            }
        }
    }

    /// A whole file: canvas size, NETSCAPE loop count and images.
    pub struct File {
        pub global: Option<Vec<[u8; 3]>>,
        pub width: usize,
        pub height: usize,
        pub loops: Option<u16>,
        pub images: Vec<Image>,
    }

    pub fn read_file(data: &[u8]) -> File {
        assert_eq!(&data[..6], b"GIF89a");
        let width = u16::from_le_bytes([data[6], data[7]]) as usize;
        let height = u16::from_le_bytes([data[8], data[9]]) as usize;
        let mut pos = 13;
        let global = (data[10] & 0x80 != 0).then(|| read_table(data, &mut pos, data[10]));
        let mut loops = None;
        let mut images = Vec::new();
        loop {
            match (data[pos], data.get(pos + 1)) {
                (0x3B, _) => {
                    assert_eq!(pos + 1, data.len(), "bytes after the trailer");
                    break;
                }
                (0x21, Some(0xFF)) => {
                    assert_eq!(&data[pos + 2..pos + 14], b"\x0BNETSCAPE2.0");
                    assert_eq!(&data[pos + 14..pos + 16], &[3, 1]);
                    loops = Some(u16::from_le_bytes([data[pos + 16], data[pos + 17]]));
                    assert_eq!(data[pos + 18], 0);
                    pos += 19;
                }
                (0x21, Some(0xF9)) => {
                    images.push(read_image_with(data, &mut pos, global.as_deref()))
                }
                (b, _) => panic!("unexpected block 0x{b:02X} at {pos}"),
            }
        }
        File {
            global,
            width,
            height,
            loops,
            images,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use crate::frame::VideoFrame;

    /// Small deterministic generator for test data.
    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    fn compress(indices: &[u8], min: u8) -> Vec<u8> {
        let mut out = Vec::new();
        lzw_compress(indices, min, &mut out).unwrap();
        out
    }

    fn round_trip(indices: &[u8], min: u8) {
        let data = compress(indices, min);
        let mut pos = 0;
        let decoded = lzw_decode(&data, &mut pos).unwrap();
        assert_eq!(pos, data.len());
        assert_eq!(
            decoded,
            indices,
            "min code size {min}, {} indices",
            indices.len()
        );
        // Sub-blocks never exceed 255 bytes.
        let mut p = 1;
        while data[p] != 0 {
            p += data[p] as usize + 1;
        }
        assert_eq!(p + 1, data.len());
    }

    #[test]
    fn lzw_known_answer_for_one_pixel() {
        // Codes 256 (clear), 0, 257 (end), 9 bits each, least significant
        // bit first: 0x000 | 0x100 -> 00 01, then 0 and 257 shifted by 18.
        assert_eq!(compress(&[0], 8), vec![8, 4, 0x00, 0x01, 0x04, 0x04, 0]);
        // Same with the smallest code size: codes 4, 0, 5 at 3 bits.
        // 100 000 101 -> bits 0..9 = 0b101_000_100 = 0x144.
        assert_eq!(compress(&[0], 2), vec![2, 2, 0x44, 0x01, 0]);
    }

    #[test]
    fn lzw_round_trips_random_data() {
        let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
        for min in 2..=8u8 {
            let mask = (1u16 << min) - 1;
            for len in [0usize, 1, 2, 3, 17, 255, 1000, 20_000, 100_000] {
                let data: Vec<u8> = (0..len).map(|_| (rng.next() as u16 & mask) as u8).collect();
                round_trip(&data, min);
            }
        }
    }

    #[test]
    fn lzw_round_trips_across_every_width_change() {
        // Random 8-bit data adds one table entry per code, so every length
        // in this range ends right around a width change or the reset.
        let mut rng = XorShift(12345);
        let data: Vec<u8> = (0..5000).map(|_| rng.next() as u8).collect();
        for len in (240..280)
            .chain(500..530)
            .chain(1010..1040)
            .chain(2030..2060)
        {
            round_trip(&data[..len], 8);
        }
        for len in 3820..3900 {
            round_trip(&data[..len], 8);
        }
        // Small alphabets cross 4, 5... 12 bits at short lengths.
        let small: Vec<u8> = (0..3000).map(|_| (rng.next() & 3) as u8).collect();
        for len in 0..600 {
            round_trip(&small[..len], 2);
        }
    }

    #[test]
    fn lzw_resets_when_the_table_is_full() {
        let mut rng = XorShift(777);
        let data: Vec<u8> = (0..50_000).map(|_| rng.next() as u8).collect();
        let packed = compress(&data, 8);
        // The table holds 4096 - 258 entries, so this input needs several
        // clear codes; the round trip proves they are placed correctly.
        assert!(packed.len() < data.len() * 2);
        round_trip(&data, 8);
    }

    #[test]
    fn lzw_round_trips_single_color_runs() {
        for min in [2u8, 8] {
            for len in [1usize, 2, 3, 4, 5, 6, 100, 4096, 200_000] {
                round_trip(&vec![1u8; len], min);
            }
        }
        // Long runs compress to a handful of bytes.
        assert!(compress(&vec![0u8; 100_000], 8).len() < 1200);
    }

    #[test]
    fn lzw_rejects_bad_parameters() {
        let mut out = Vec::new();
        assert!(lzw_compress(&[4], 2, &mut out).is_err());
        assert!(lzw_compress(&[0], 1, &mut out).is_err());
        assert!(lzw_compress(&[0], 9, &mut out).is_err());
        assert!(out.is_empty());
    }

    #[test]
    fn test_decoder_rejects_garbage() {
        let mut pos = 0;
        assert!(lzw_decode(&[8, 1, 0xFF, 0], &mut pos).is_err());
        let mut pos = 0;
        assert!(lzw_decode(&[8, 5, 1, 2], &mut pos).is_err());
    }

    fn frame(width: u32, height: u32, pixels: Vec<u8>, pts: i64, rate: Rational) -> Frame {
        Frame::Video(VideoFrame {
            width,
            height,
            format: PixelFormat::Rgb24,
            planes: vec![pixels],
            pts,
            time_base: rate.inv(),
        })
    }

    /// Encodes `pictures` (one per frame, pts 0, 1, 2...) and returns the
    /// packets.
    fn encode_all(width: u32, height: u32, rate: Rational, pictures: &[Vec<u8>]) -> Vec<Packet> {
        let mut enc = GifEncoder::new(width, height, rate, 0).unwrap();
        let mut packets = Vec::new();
        for (i, p) in pictures.iter().enumerate() {
            packets.extend(
                enc.encode(&frame(width, height, p.clone(), i as i64, rate))
                    .unwrap(),
            );
        }
        packets.extend(enc.flush().unwrap());
        packets
    }

    fn decode_packets(width: usize, height: usize, packets: &[Packet]) -> Vec<(Vec<u8>, Image)> {
        let mut canvas = vec![0u8; width * height * 3];
        packets
            .iter()
            .map(|p| {
                let mut pos = 0;
                let image = read_image(&p.data, &mut pos);
                assert_eq!(pos, p.data.len(), "packet holds exactly one image");
                assert!(image.left + image.width <= width);
                assert!(image.top + image.height <= height);
                composite(&mut canvas, width, &image);
                (canvas.clone(), image)
            })
            .collect()
    }

    /// A picture using `colors` distinct colors in a scattered pattern.
    fn few_colors(width: usize, height: usize, colors: usize, seed: u64) -> Vec<u8> {
        let mut rng = XorShift(seed);
        let palette: Vec<[u8; 3]> = (0..colors)
            .map(|i| {
                [
                    (i * 37) as u8,
                    (i * 91 + 7) as u8,
                    (i * 13 + (i >> 8)) as u8,
                ]
            })
            .collect();
        let mut out = Vec::with_capacity(width * height * 3);
        for i in 0..width * height {
            // Every color appears at least once.
            let c = if i < colors {
                i
            } else {
                (rng.next() % colors as u64) as usize
            };
            out.extend_from_slice(&palette[c]);
        }
        out
    }

    fn psnr(a: &[u8], b: &[u8]) -> f64 {
        let mse: f64 = a
            .iter()
            .zip(b)
            .map(|(&x, &y)| (x as f64 - y as f64).powi(2))
            .sum::<f64>()
            / a.len() as f64;
        10.0 * (255.0f64 * 255.0 / mse.max(1e-12)).log10()
    }

    #[test]
    fn pictures_with_few_colors_are_exact() {
        let rate = Rational::new(10, 1);
        for colors in [1usize, 2, 3, 5, 16, 17, 200, 255, 256] {
            let (w, h) = (40, 30);
            let picture = few_colors(w, h, colors, colors as u64 + 1);
            let packets = encode_all(w as u32, h as u32, rate, std::slice::from_ref(&picture));
            let decoded = decode_packets(w, h, &packets);
            assert_eq!(decoded.len(), 1);
            let (canvas, image) = &decoded[0];
            assert_eq!(canvas, &picture, "{colors} colors");
            assert_eq!(image.transparent, None);
            assert!(image.palette.len() >= colors && image.palette.len() < 2 * colors.max(2));
        }
    }

    #[test]
    fn rich_pictures_are_dithered_closely() {
        // A smooth gradient with far more than 256 colors.
        let (w, h) = (128usize, 96usize);
        let mut picture = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            for x in 0..w {
                picture.extend_from_slice(&[
                    (x * 2) as u8,
                    (y * 255 / h) as u8,
                    ((x + y) % 256) as u8,
                ]);
            }
        }
        let packets = encode_all(
            w as u32,
            h as u32,
            Rational::new(25, 1),
            std::slice::from_ref(&picture),
        );
        let decoded = decode_packets(w, h, &packets);
        let (canvas, image) = &decoded[0];
        assert_eq!(image.palette.len(), 256);
        let quality = psnr(canvas, &picture);
        assert!(quality > 30.0, "PSNR {quality:.1} dB");
    }

    #[test]
    fn median_cut_is_deterministic_and_bounded() {
        let mut rng = XorShift(99);
        let keys: Vec<u32> = (0..50_000).map(|_| rng.next() as u32 & 0xFF_FFFF).collect();
        let (a, exact) = build_palette(&histogram(&keys), 255);
        assert!(!exact);
        assert_eq!(a.len(), 255);
        let (b, _) = build_palette(&histogram(&keys), 255);
        assert_eq!(a, b);
        // Two colors far apart in a sea of noise both get their own entry.
        let mut keys = vec![0x000000u32; 1000];
        keys.extend(vec![0xFFFFFFu32; 1000]);
        keys.extend((0..300).map(|i| 0x808000 + i));
        let (p, _) = build_palette(&histogram(&keys), 16);
        assert!(p.contains(&[0, 0, 0]) && p.contains(&[255, 255, 255]));
    }

    #[test]
    fn encoding_is_deterministic() {
        let rate = Rational::new(15, 1);
        let pictures: Vec<Vec<u8>> = (0..4).map(|i| few_colors(33, 21, 300, i)).collect();
        let a = encode_all(33, 21, rate, &pictures);
        let b = encode_all(33, 21, rate, &pictures);
        assert_eq!(a, b);
    }

    #[test]
    fn later_pictures_cover_only_the_changes() {
        let (w, h) = (20usize, 10usize);
        let base = few_colors(w, h, 50, 3);
        let mut second = base.clone();
        // Change a 3x2 block with a hole in the middle row left unchanged.
        for (x, y) in [(5, 4), (6, 4), (7, 4), (5, 5), (7, 5)] {
            let at = (y * w + x) * 3;
            second[at..at + 3].copy_from_slice(&[1, 2, 3]);
        }
        let mut third = second.clone();
        third[0..3].copy_from_slice(&[9, 9, 9]);
        let last = (w * h - 1) * 3;
        third[last..last + 3].copy_from_slice(&[9, 9, 9]);
        let pictures = vec![base, second, third];
        let packets = encode_all(w as u32, h as u32, Rational::new(10, 1), &pictures);
        assert!(packets[0].keyframe && !packets[1].keyframe);
        let decoded = decode_packets(w, h, &packets);
        assert_eq!(decoded.len(), 3);
        for ((canvas, image), source) in decoded.iter().zip(&pictures) {
            assert_eq!(canvas, source);
            assert_eq!(image.disposal, 1);
        }
        let second = &decoded[1].1;
        assert_eq!(
            (second.left, second.top, second.width, second.height),
            (5, 4, 3, 2)
        );
        // The unchanged pixel inside the rectangle is transparent.
        assert_eq!(second.transparent, Some(second.indices[4]));
        let third = &decoded[2].1;
        assert_eq!(
            (third.left, third.top, third.width, third.height),
            (0, 0, w, h)
        );
        assert!(third.transparent.is_some());
    }

    #[test]
    fn later_pictures_reuse_the_first_table_when_it_fits() {
        let (w, h) = (16usize, 8usize);
        let first = few_colors(w, h, 6, 1);
        // Swap two pixels: same colors, so the first table fits exactly.
        let mut second = first.clone();
        let (a, b) = (0..w * h)
            .flat_map(|i| (0..w * h).map(move |j| (i, j)))
            .find(|&(i, j)| first[i * 3..i * 3 + 3] != first[j * 3..j * 3 + 3])
            .unwrap();
        second[a * 3..a * 3 + 3].copy_from_slice(&first[b * 3..b * 3 + 3]);
        second[b * 3..b * 3 + 3].copy_from_slice(&first[a * 3..a * 3 + 3]);
        // Brand new colors: the first table cannot render them exactly.
        let third: Vec<u8> = second.iter().map(|&v| v ^ 0x55).collect();
        let pictures = vec![first, second, third];
        let packets = encode_all(w as u32, h as u32, Rational::new(10, 1), &pictures);
        let decoded = decode_packets(w, h, &packets);
        for ((canvas, _), source) in decoded.iter().zip(&pictures) {
            assert_eq!(canvas, source);
        }
        let table = |i: usize| &decoded[i].1.palette;
        assert_eq!(table(1), table(0));
        assert_ne!(table(2), table(0));
        // The reused table has room for a transparent entry because the
        // six colors leave two of its eight entries free.
        assert_eq!(table(0).len(), 8);
    }

    #[test]
    fn rich_pictures_reuse_a_close_table() {
        // A gradient that shifts by one level: the first palette still
        // renders it well, so its table comes back.
        let (w, h) = (64usize, 64usize);
        let picture = |shift: usize| -> Vec<u8> {
            let mut p = Vec::with_capacity(w * h * 3);
            for y in 0..h {
                for x in 0..w {
                    p.extend_from_slice(&[
                        (x * 4 + shift) as u8,
                        (y * 4) as u8,
                        ((x + y) * 2) as u8,
                    ]);
                }
            }
            p
        };
        let pictures = vec![picture(0), picture(1)];
        let packets = encode_all(w as u32, h as u32, Rational::new(10, 1), &pictures);
        let decoded = decode_packets(w, h, &packets);
        assert_eq!(decoded[1].1.palette, decoded[0].1.palette);
        assert!(psnr(&decoded[1].0, &pictures[1]) > 30.0);
    }

    #[test]
    fn identical_pictures_are_merged() {
        let rate = Rational::new(10, 1);
        let a = few_colors(8, 8, 4, 1);
        let b = few_colors(8, 8, 4, 2);
        let packets = encode_all(8, 8, rate, &[a.clone(), a.clone(), a, b.clone(), b]);
        let decoded = decode_packets(8, 8, &packets);
        let delays: Vec<u16> = decoded.iter().map(|(_, i)| i.delay).collect();
        assert_eq!(delays, vec![30, 20]);
        assert_eq!(packets[0].duration, 3);
        assert_eq!((packets[1].pts, packets[1].duration), (3, 2));
    }

    #[test]
    fn unchanged_pictures_after_a_drop_keep_their_time() {
        // At 200 fps every other picture lasts zero centiseconds. Frame 1
        // is dropped and frame 2 equals frame 0: a one pixel transparent
        // image must still carry the time.
        let rate = Rational::new(200, 1);
        let a = few_colors(4, 4, 3, 1);
        let b = few_colors(4, 4, 3, 2);
        let packets = encode_all(4, 4, rate, &[a.clone(), b, a.clone()]);
        let decoded = decode_packets(4, 4, &packets);
        assert_eq!(decoded.len(), 2);
        let (canvas, image) = &decoded[1];
        assert_eq!(canvas, &a);
        assert_eq!((image.width, image.height), (1, 1));
        assert_eq!(image.transparent, Some(image.indices[0]));
        let total: u32 = decoded.iter().map(|(_, i)| i.delay as u32).sum();
        assert_eq!(total, 2);
    }

    /// Delays written for `n` distinct pictures at `rate`.
    fn delays(rate: Rational, n: usize) -> Vec<u16> {
        let pictures: Vec<Vec<u8>> = (0..n).map(|i| vec![i as u8; 3]).collect();
        let packets = encode_all(1, 1, rate, &pictures);
        decode_packets(1, 1, &packets)
            .into_iter()
            .map(|(_, i)| i.delay)
            .collect()
    }

    #[test]
    fn delays_follow_the_centisecond_grid() {
        assert_eq!(delays(Rational::new(30, 1), 6), vec![3, 4, 3, 3, 4, 3]);
        let d = delays(Rational::new(30, 1), 30);
        assert_eq!(d.iter().map(|&v| v as u32).sum::<u32>(), 100);
        assert_eq!(delays(Rational::new(15, 1), 6), vec![7, 6, 7, 7, 6, 7]);
        assert_eq!(delays(Rational::new(25, 1), 4), vec![4; 4]);
        assert_eq!(delays(Rational::new(10, 1), 3), vec![10; 3]);
        assert_eq!(delays(Rational::new(1, 2), 2), vec![200; 2]);
        // NTSC film: 1001/24000 s per frame, 4.17 cs.
        let d = delays(Rational::new(24000, 1001), 24);
        assert!(d.iter().all(|&v| v == 4 || v == 5));
        assert_eq!(d.iter().map(|&v| v as u32).sum::<u32>(), 100); // 24 * 4.1708 = 100.1
        // Very slow rates clamp to the 16-bit field.
        assert_eq!(delays(Rational::new(1, 1000), 1), vec![65535]);
    }

    #[test]
    fn pictures_shorter_than_a_centisecond_are_dropped() {
        // 120 fps: 0.83 cs per frame, so one frame in six rounds away.
        let d = delays(Rational::new(120, 1), 12);
        assert_eq!(d.len(), 10);
        assert!(d.iter().all(|&v| v == 1));
        // The total still matches the 12 frames: 12 / 120 s = 10 cs.
        assert_eq!(d.iter().map(|&v| v as u32).sum::<u32>(), 10);
    }

    #[test]
    fn timestamps_with_gaps_lengthen_the_delay() {
        let rate = Rational::new(10, 1);
        let mut enc = GifEncoder::new(1, 1, rate, 3).unwrap();
        let mut packets = Vec::new();
        for (pts, v) in [(0, 1u8), (5, 2), (6, 3)] {
            packets.extend(enc.encode(&frame(1, 1, vec![v; 3], pts, rate)).unwrap());
        }
        packets.extend(enc.flush().unwrap());
        let durations: Vec<(i64, i64)> = packets.iter().map(|p| (p.pts, p.duration)).collect();
        assert_eq!(durations, vec![(0, 5), (5, 1), (6, 1)]);
        assert!(packets.iter().all(|p| p.stream_index == 3));
        let d: Vec<u16> = decode_packets(1, 1, &packets)
            .iter()
            .map(|(_, i)| i.delay)
            .collect();
        assert_eq!(d, vec![50, 10, 10]);
    }

    #[test]
    fn rejects_bad_setup_and_frames() {
        let video = |w, h, r| {
            FrameFormat::Video(VideoFormat {
                width: w,
                height: h,
                pixel_format: PixelFormat::Yuv420p,
                frame_rate: r,
            })
        };
        let opts = EncoderOptions::default();
        let rate = Rational::new(25, 1);
        assert!(new_encoder(CodecId::Gif, &video(0, 10, rate), &opts).is_err());
        assert!(new_encoder(CodecId::Gif, &video(65536, 10, rate), &opts).is_err());
        assert!(new_encoder(CodecId::Gif, &video(10, 10, Rational::new(0, 1)), &opts).is_err());
        assert!(new_encoder(CodecId::Gif, &video(10, 10, Rational::new(-5, 1)), &opts).is_err());
        assert!(new_encoder(CodecId::RawVideo, &video(10, 10, rate), &opts).is_err());
        let audio = FrameFormat::Audio(crate::frame::AudioFormat {
            sample_rate: 8000,
            channels: 1,
            kind: crate::frame::SampleKind::Int(16),
        });
        assert!(new_encoder(CodecId::Gif, &audio, &opts).is_err());

        // Any proposed pixel format becomes rgb24.
        let mut enc = new_encoder(CodecId::Gif, &video(4, 2, rate), &opts).unwrap();
        let FrameFormat::Video(f) = enc.input_format() else {
            panic!("video expected");
        };
        assert_eq!(
            (f.width, f.height, f.pixel_format),
            (4, 2, PixelFormat::Rgb24)
        );
        assert_eq!(f.frame_rate, rate);
        let info = enc.stream_info();
        assert_eq!(info.codec, CodecId::Gif);
        assert_eq!(info.time_base, Rational::new(1, 25));
        assert_eq!(info.video().unwrap().width, 4);

        assert!(enc.encode(&frame(4, 2, vec![0; 23], 0, rate)).is_err());
        assert!(enc.encode(&frame(2, 4, vec![0; 24], 0, rate)).is_err());
        let mut yuv = VideoFrame::new_black(4, 2, PixelFormat::Yuv444p, 0, rate.inv());
        assert!(enc.encode(&Frame::Video(yuv.clone())).is_err());
        yuv.format = PixelFormat::Rgb24;
        assert!(enc.encode(&Frame::Video(yuv)).is_err());
        let sound = crate::frame::AudioFrame::new(
            8000,
            1,
            crate::frame::AudioData::Int {
                samples: vec![0],
                bits: 16,
            },
            0,
        );
        assert!(enc.encode(&Frame::Audio(sound)).is_err());
        // Nothing valid went in, so nothing comes out.
        assert!(enc.flush().unwrap().is_empty());
    }

    #[test]
    fn color_table_sizes() {
        assert_eq!(table_size_bits(1), 0);
        assert_eq!(table_size_bits(2), 0);
        assert_eq!(table_size_bits(3), 1);
        assert_eq!(table_size_bits(4), 1);
        assert_eq!(table_size_bits(5), 2);
        assert_eq!(table_size_bits(129), 7);
        assert_eq!(table_size_bits(256), 7);
    }

    #[test]
    fn bounding_rectangles() {
        let a = vec![0u8; 5 * 4 * 3];
        assert_eq!(changed_rect(&a, &a, 5, 4), None);
        let mut b = a.clone();
        b[(2 * 5 + 3) * 3 + 1] = 1;
        b[(5 + 1) * 3 + 2] = 1;
        assert_eq!(
            changed_rect(&a, &b, 5, 4),
            Some(Rect {
                x: 1,
                y: 1,
                width: 3,
                height: 2
            })
        );
    }
}
