//! Static test patterns: `testsrc`, `smptebars` and `color`.

use super::draw::{Canvas, GLYPH_HEIGHT, Rgb, text_width};
use super::{Painter, VideoGenerator, VideoOptions};
use crate::error::{Error, Result};
use crate::filter::Args;
use crate::source::Source;

/// Paints the same picture on every frame, rendered once up front.
struct Still {
    picture: Vec<u8>,
}

impl Painter for Still {
    fn paint(&mut self, _index: u64, pixels: &mut [u8]) {
        pixels.copy_from_slice(&self.picture);
    }
}

/// Allocates a picture of the given size and lets `draw` fill it.
fn render(options: &VideoOptions, draw: impl FnOnce(&mut Canvas<'_>)) -> Vec<u8> {
    let (w, h) = (options.width as usize, options.height as usize);
    let mut picture = vec![0u8; w * h * 3];
    draw(&mut Canvas::new(w, h, &mut picture));
    picture
}

pub(crate) fn create_color(args: &Args) -> Result<Box<dyn Source>> {
    let options = VideoOptions::parse(args, "color", &["color", "c"])?;
    let color = match args.get(&["color", "c"]) {
        Some(spec) => parse_color(spec)?,
        None => [0, 0, 0],
    };
    let picture = render(&options, |c| {
        c.fill_rect(0, 0, c.width() as i64, c.height() as i64, color)
    });
    Ok(Box::new(VideoGenerator::new(options, Still { picture })))
}

/// Named colors from CSS Color Level 4: all sixteen basic names plus the
/// most common extended ones. Values are sRGB, the same space as rgb24.
const NAMED_COLORS: &[(&str, Rgb)] = &[
    ("black", [0, 0, 0]),
    ("silver", [192, 192, 192]),
    ("gray", [128, 128, 128]),
    ("grey", [128, 128, 128]),
    ("white", [255, 255, 255]),
    ("maroon", [128, 0, 0]),
    ("red", [255, 0, 0]),
    ("purple", [128, 0, 128]),
    ("fuchsia", [255, 0, 255]),
    ("magenta", [255, 0, 255]),
    // CSS green is the half intensity one; full green is "lime".
    ("green", [0, 128, 0]),
    ("lime", [0, 255, 0]),
    ("olive", [128, 128, 0]),
    ("yellow", [255, 255, 0]),
    ("navy", [0, 0, 128]),
    ("blue", [0, 0, 255]),
    ("teal", [0, 128, 128]),
    ("aqua", [0, 255, 255]),
    ("cyan", [0, 255, 255]),
    ("orange", [255, 165, 0]),
    ("darkorange", [255, 140, 0]),
    ("pink", [255, 192, 203]),
    ("hotpink", [255, 105, 180]),
    ("deeppink", [255, 20, 147]),
    ("brown", [165, 42, 42]),
    ("gold", [255, 215, 0]),
    ("goldenrod", [218, 165, 32]),
    ("violet", [238, 130, 238]),
    ("indigo", [75, 0, 130]),
    ("orchid", [218, 112, 214]),
    ("plum", [221, 160, 221]),
    ("lavender", [230, 230, 250]),
    ("darkgray", [169, 169, 169]),
    ("darkgrey", [169, 169, 169]),
    ("lightgray", [211, 211, 211]),
    ("lightgrey", [211, 211, 211]),
    ("dimgray", [105, 105, 105]),
    ("dimgrey", [105, 105, 105]),
    ("slategray", [112, 128, 144]),
    ("slategrey", [112, 128, 144]),
    ("darkred", [139, 0, 0]),
    ("crimson", [220, 20, 60]),
    ("tomato", [255, 99, 71]),
    ("coral", [255, 127, 80]),
    ("salmon", [250, 128, 114]),
    ("chocolate", [210, 105, 30]),
    ("sienna", [160, 82, 45]),
    ("tan", [210, 180, 140]),
    ("wheat", [245, 222, 179]),
    ("beige", [245, 245, 220]),
    ("ivory", [255, 255, 240]),
    ("khaki", [240, 230, 140]),
    ("darkgreen", [0, 100, 0]),
    ("forestgreen", [34, 139, 34]),
    ("seagreen", [46, 139, 87]),
    ("limegreen", [50, 205, 50]),
    ("chartreuse", [127, 255, 0]),
    ("turquoise", [64, 224, 208]),
    ("skyblue", [135, 206, 235]),
    ("steelblue", [70, 130, 180]),
    ("royalblue", [65, 105, 225]),
    ("darkblue", [0, 0, 139]),
    ("midnightblue", [25, 25, 112]),
];

/// Parses a CSS color name (any case), `#RRGGBB`, `#RGB` or `0xRRGGBB`.
pub(crate) fn parse_color(spec: &str) -> Result<Rgb> {
    let s = spec.trim();
    let bad = || {
        Error::invalid_argument(format!(
            "unknown color '{spec}' (use a name such as red, #RRGGBB or 0xRRGGBB)"
        ))
    };
    let hex = s
        .strip_prefix('#')
        .or_else(|| s.strip_prefix("0x"))
        .or_else(|| s.strip_prefix("0X"));
    if let Some(hex) = hex {
        if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(bad());
        }
        let digit = |i: usize| u8::from_str_radix(&hex[i..i + 1], 16).unwrap_or(0);
        return match hex.len() {
            6 => Ok([0, 2, 4].map(|i| digit(i) * 16 + digit(i + 1))),
            // Short form: each digit is doubled, so #f80 is #ff8800. Only
            // the CSS spelling has it; 0x always takes six digits.
            3 if s.starts_with('#') => Ok([0, 1, 2].map(|i| digit(i) * 17)),
            _ => Err(bad()),
        };
    }
    NAMED_COLORS
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(s))
        .map(|&(_, rgb)| rgb)
        .ok_or_else(bad)
}

// Levels of the SMPTE bars. Rgb24 is full range, so 0% video is code 0
// and 100% is 255; the bars are 75% amplitude.
const B75: u8 = 191;
const GRAY: Rgb = [B75, B75, B75];
const YELLOW: Rgb = [B75, B75, 0];
const CYAN: Rgb = [0, B75, B75];
const GREEN: Rgb = [0, B75, 0];
const MAGENTA: Rgb = [B75, 0, B75];
const RED: Rgb = [B75, 0, 0];
const BLUE: Rgb = [0, 0, B75];
const BLACK: Rgb = [0, 0, 0];
const WHITE: Rgb = [255, 255, 255];
/// The -I and +Q bars carry 40 IRE peak to peak of chroma (amplitude 0.2)
/// on black. A pure chroma signal with no luminance lies outside the RGB
/// cube, so these are the darkest colors on each axis that reach that
/// amplitude: through the NTSC YIQ to RGB matrix, -I with I = -0.2 needs
/// Y = 0.956 * 0.2 for red to reach zero, giving (0, 0.246, 0.412); +Q with
/// Q = 0.2 needs Y = 0.647 * 0.2 for green to reach zero, giving
/// (0.254, 0, 0.470).
const MINUS_I: Rgb = [0, 63, 105];
const PLUS_Q: Rgb = [65, 0, 120];
/// PLUGE levels: 4% below and above black. Full range rgb24 has no code
/// below black, so the darker bar clips to black and only the brighter
/// one stays distinguishable; the layout keeps all three for alignment.
const PLUGE_LOW: Rgb = [0, 0, 0];
const PLUGE_HIGH: Rgb = [10, 10, 10];

const TOP_BARS: [Rgb; 7] = [GRAY, YELLOW, CYAN, GREEN, MAGENTA, RED, BLUE];
/// The "castellations": the blue carrying bars in reverse order with black
/// between them, so a monitor in blue only mode shows matching halves.
const MIDDLE_BARS: [Rgb; 7] = [BLUE, BLACK, MAGENTA, BLACK, CYAN, BLACK, GRAY];

/// Color of the SMPTE bars at a pixel.
///
/// The width is divided into 84 units: twelve per top bar, so the bottom
/// row's four wide patches of five fourths of a bar (15 units each) and
/// the three PLUGE strips of a third of a bar (4 units each) fall on unit
/// boundaries. Heights follow EG 1: two thirds bars, one twelfth
/// castellations, one fourth bottom row.
fn smpte_color(x: usize, y: usize, width: usize, height: usize) -> Rgb {
    let unit = x * 84 / width;
    let band = y * 12 / height;
    if band < 8 {
        TOP_BARS[unit / 12]
    } else if band < 9 {
        MIDDLE_BARS[unit / 12]
    } else {
        match unit {
            0..15 => MINUS_I,
            15..30 => WHITE,
            30..45 => PLUS_Q,
            45..60 => BLACK,
            60..64 => PLUGE_LOW,
            64..68 => BLACK,
            68..72 => PLUGE_HIGH,
            _ => BLACK,
        }
    }
}

pub(crate) fn create_smptebars(args: &Args) -> Result<Box<dyn Source>> {
    let options = VideoOptions::parse(args, "smptebars", &[])?;
    let picture = render(&options, |c| {
        let (w, h) = (c.width(), c.height());
        for y in 0..h {
            for (x, px) in c.row_mut(y).chunks_exact_mut(3).enumerate() {
                px.copy_from_slice(&smpte_color(x, y, w, h));
            }
        }
    });
    Ok(Box::new(VideoGenerator::new(options, Still { picture })))
}

/// Full intensity bars of the test pattern, in decreasing luminance.
const TESTSRC_BARS: [Rgb; 8] = [
    [255, 255, 255],
    [255, 255, 0],
    [0, 255, 255],
    [0, 255, 0],
    [255, 0, 255],
    [255, 0, 0],
    [0, 0, 255],
    [0, 0, 0],
];

/// `testsrc`: eight vertical bars over the top three quarters, a gray ramp
/// below, a disc that bounces around the picture so motion (and dropped or
/// repeated frames) is visible, and the frame number in the top left
/// corner.
struct TestPattern {
    width: usize,
    height: usize,
    background: Vec<u8>,
}

impl TestPattern {
    fn new(options: &VideoOptions) -> Self {
        let background = render(options, |c| {
            let (w, h) = (c.width(), c.height());
            for y in 0..h {
                let bars = y * 4 < h * 3;
                for (x, px) in c.row_mut(y).chunks_exact_mut(3).enumerate() {
                    let color = if bars {
                        TESTSRC_BARS[x * 8 / w]
                    } else {
                        let v = (x * 255).checked_div(w - 1).unwrap_or(0) as u8;
                        [v, v, v]
                    };
                    px.copy_from_slice(&color);
                }
            }
        });
        TestPattern {
            width: options.width as usize,
            height: options.height as usize,
            background,
        }
    }

    fn disc_radius(&self) -> usize {
        (self.width.min(self.height) / 10).max(1)
    }

    /// Center of the disc in frame `index`. It moves a fixed number of
    /// pixels per frame along each axis and reflects off the edges; the
    /// different speeds make it sweep the whole picture over time.
    fn disc_center(&self, index: u64) -> (usize, usize) {
        let r = self.disc_radius();
        let span_x = self.width.saturating_sub(2 * r + 1);
        let span_y = self.height.saturating_sub(2 * r + 1);
        let step_x = (self.width / 64).max(1) as u64;
        let step_y = (self.height / 48).max(1) as u64;
        // Start in the middle, away from the counter.
        let x = bounce(index.wrapping_mul(step_x), span_x, span_x / 2);
        let y = bounce(index.wrapping_mul(step_y), span_y, span_y / 2);
        (r + x, r + y)
    }

    /// Size of the counter digits: three pixels per font pixel at 240
    /// lines, scaling with the picture height.
    fn counter_scale(&self) -> usize {
        (self.height / 80).max(1)
    }
}

/// Position after travelling `distance` from `start` along a segment of
/// length `span`, reflecting at both ends.
fn bounce(distance: u64, span: usize, start: usize) -> usize {
    if span == 0 {
        return 0;
    }
    let period = 2 * span as u64;
    let p = (distance % period + start as u64) % period;
    if p <= span as u64 {
        p as usize
    } else {
        (period - p) as usize
    }
}

impl Painter for TestPattern {
    fn paint(&mut self, index: u64, pixels: &mut [u8]) {
        pixels.copy_from_slice(&self.background);
        let r = self.disc_radius() as i64;
        let (cx, cy) = self.disc_center(index);
        let scale = self.counter_scale();
        let mut c = Canvas::new(self.width, self.height, pixels);
        c.invert_disc(cx as i64, cy as i64, r);
        // At least four digits, so the box only grows after frame 9999.
        let text = format!("{index:04}");
        let pad = scale as i64;
        let (x, y) = (2 * pad, 2 * pad);
        c.fill_rect(
            x - pad,
            y - pad,
            text_width(&text, scale) as i64 + 2 * pad,
            (GLYPH_HEIGHT * scale) as i64 + 2 * pad,
            BLACK,
        );
        c.draw_text(x, y, &text, scale as i64, WHITE);
    }
}

pub(crate) fn create_testsrc(args: &Args) -> Result<Box<dyn Source>> {
    let options = VideoOptions::parse(args, "testsrc", &[])?;
    let painter = TestPattern::new(&options);
    Ok(Box::new(VideoGenerator::new(options, painter)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{Frame, VideoFrame};
    use crate::rational::Rational;
    use crate::source::open;

    fn first_frame(spec: &str) -> VideoFrame {
        match open(spec).unwrap().next_frame().unwrap() {
            Some(Frame::Video(v)) => v,
            _ => panic!("no video frame"),
        }
    }

    fn pixel(f: &VideoFrame, x: usize, y: usize) -> Rgb {
        let i = (y * f.width as usize + x) * 3;
        let p = &f.planes[0];
        [p[i], p[i + 1], p[i + 2]]
    }

    #[test]
    fn color_names_and_hex() {
        assert_eq!(parse_color("red").unwrap(), [255, 0, 0]);
        assert_eq!(parse_color("Green").unwrap(), [0, 128, 0]);
        assert_eq!(parse_color("lime").unwrap(), [0, 255, 0]);
        assert_eq!(parse_color("gray").unwrap(), parse_color("grey").unwrap());
        assert_eq!(parse_color("orange").unwrap(), [255, 165, 0]);
        assert_eq!(parse_color("purple").unwrap(), [128, 0, 128]);
        assert_eq!(parse_color("pink").unwrap(), [255, 192, 203]);
        assert_eq!(parse_color("#1a2B3c").unwrap(), [0x1a, 0x2b, 0x3c]);
        assert_eq!(parse_color("0xFF8000").unwrap(), [255, 128, 0]);
        assert_eq!(parse_color("#f80").unwrap(), [255, 136, 0]);
        for bad in [
            "",
            "#",
            "#12345",
            "#1234567",
            "0xfff",
            "#gg0000",
            "#ff00€0",
            "chartreux",
        ] {
            assert!(
                matches!(parse_color(bad), Err(Error::InvalidArgument(_))),
                "{bad}"
            );
        }
        // Every table entry is reachable and lowercase.
        for (name, rgb) in NAMED_COLORS {
            assert_eq!(&parse_color(name).unwrap(), rgb);
            assert_eq!(name.to_ascii_lowercase(), *name);
        }
    }

    #[test]
    fn color_source_is_uniform() {
        let f = first_frame("color=c=0x336699:s=17x5");
        assert_eq!((f.width, f.height), (17, 5));
        assert!(f.planes[0].chunks_exact(3).all(|p| p == [0x33, 0x66, 0x99]));
        // Positional color and size; the default is black.
        let f = first_frame("color=yellow:8x2");
        assert!(f.planes[0].chunks_exact(3).all(|p| p == [255, 255, 0]));
        let f = first_frame("color");
        assert!(f.planes[0].iter().all(|&b| b == 0));
        assert!(open("color=c=nosuchcolor").is_err());
    }

    #[test]
    fn smpte_bars_have_exact_levels() {
        // 840 is a multiple of 84 and 480 of 12, so units are 10 pixels
        // wide and bands 40 lines tall.
        let f = first_frame("smptebars=s=840x480");
        let top = [GRAY, YELLOW, CYAN, GREEN, MAGENTA, RED, BLUE];
        for (i, want) in top.iter().enumerate() {
            assert_eq!(pixel(&f, i * 120, 0), *want, "top bar {i}");
            assert_eq!(pixel(&f, i * 120 + 119, 319), *want, "top bar {i}");
        }
        assert_eq!(top[0], [191, 191, 191]);
        assert_eq!(top[1], [191, 191, 0]);
        let middle = [BLUE, BLACK, MAGENTA, BLACK, CYAN, BLACK, GRAY];
        for (i, want) in middle.iter().enumerate() {
            assert_eq!(pixel(&f, i * 120 + 60, 320), *want, "castellation {i}");
            assert_eq!(pixel(&f, i * 120 + 60, 359), *want, "castellation {i}");
        }
        let bottom = [
            (0, [0, 63, 105]),
            (149, [0, 63, 105]),
            (150, [255, 255, 255]),
            (299, [255, 255, 255]),
            (300, [65, 0, 120]),
            (449, [65, 0, 120]),
            (450, [0, 0, 0]),
            (599, [0, 0, 0]),
            (600, PLUGE_LOW),
            (640, BLACK),
            (680, [10, 10, 10]),
            (719, [10, 10, 10]),
            (720, BLACK),
            (839, BLACK),
        ];
        for (x, want) in bottom {
            assert_eq!(pixel(&f, x, 360), want, "bottom row at x = {x}");
            assert_eq!(pixel(&f, x, 479), want, "bottom row at x = {x}");
        }
    }

    #[test]
    fn smpte_bars_at_odd_sizes() {
        for (w, h) in [(1, 1), (7, 3), (83, 13), (321, 241)] {
            let f = first_frame(&format!("smptebars=s={w}x{h}"));
            f.validate().unwrap();
            assert_eq!(pixel(&f, 0, 0), GRAY);
            assert_eq!(pixel(&f, w - 1, 0), if w >= 7 { BLUE } else { GRAY });
        }
    }

    #[test]
    fn testsrc_bars_ramp_and_motion() {
        let p = TestPattern::new(&VideoOptions::new(320, 240, Rational::new(25, 1), None).unwrap());
        let mut s = VideoGenerator::new(
            VideoOptions::new(320, 240, Rational::new(25, 1), None).unwrap(),
            p,
        );
        let Some(Frame::Video(f0)) = s.next_frame().unwrap() else {
            panic!()
        };
        // Disc starts in the middle: (r + span / 2) on both axes.
        assert_eq!(s.painter.disc_center(0), (24 + 271 / 2, 24 + 191 / 2));
        // Bars along a row the disc and counter do not reach in frame 0.
        for (i, want) in TESTSRC_BARS.iter().enumerate() {
            assert_eq!(pixel(&f0, i * 40 + 20, 170), *want, "bar {i}");
        }
        // Ramp: black at the left edge, white at the right one.
        assert_eq!(pixel(&f0, 0, 239), [0, 0, 0]);
        assert_eq!(pixel(&f0, 319, 239), [255, 255, 255]);
        assert_eq!(pixel(&f0, 160, 200), [127, 127, 127]);
        // The disc inverts the bars under it.
        let (cx, cy) = s.painter.disc_center(0);
        let under = TESTSRC_BARS[cx * 8 / 320];
        assert_eq!(pixel(&f0, cx, cy), under.map(|b| 255 - b));

        let Some(Frame::Video(f1)) = s.next_frame().unwrap() else {
            panic!()
        };
        assert_ne!(f0.planes, f1.planes);
        let (x1, y1) = s.painter.disc_center(1);
        assert_eq!((x1, y1), (cx + 5, cy + 5));
    }

    #[test]
    fn disc_bounces_inside_the_picture() {
        let p = TestPattern::new(&VideoOptions::new(100, 60, Rational::new(25, 1), None).unwrap());
        let r = p.disc_radius();
        for i in 0..1000 {
            let (x, y) = p.disc_center(i);
            assert!(x >= r && x + r < 100, "frame {i}: x {x}");
            assert!(y >= r && y + r < 60, "frame {i}: y {y}");
        }
        assert_eq!(bounce(0, 10, 3), 3);
        assert_eq!(bounce(7, 10, 3), 10);
        assert_eq!(bounce(9, 10, 3), 8);
        assert_eq!(bounce(20, 10, 3), 3);
        assert_eq!(bounce(u64::MAX, 10, 3), bounce(u64::MAX % 20, 10, 3));
        assert_eq!(bounce(5, 0, 0), 0);
    }

    #[test]
    fn testsrc_draws_the_frame_number() {
        let options = VideoOptions::new(160, 120, Rational::new(25, 1), None).unwrap();
        let mut p = TestPattern::new(&options);
        let scale = p.counter_scale();
        assert_eq!(scale, 1);
        let mut pixels = vec![0u8; 160 * 120 * 3];
        p.paint(1234, &mut pixels);
        // The counter box starts at (1, 1) and the text at (2, 2); read the
        // glyph area back as ASCII art.
        let art: Vec<String> = (0..7)
            .map(|y| {
                (0..23)
                    .map(|x| {
                        let i = ((y + 2) * 160 + x + 2) * 3;
                        match pixels[i..i + 3] {
                            [255, 255, 255] => '#',
                            [0, 0, 0] => '.',
                            _ => '?',
                        }
                    })
                    .collect()
            })
            .collect();
        assert_eq!(
            art,
            [
                "..#....###..#####....#.",
                ".##...#...#....#....##.",
                "..#.......#...#....#.#.",
                "..#......#.....#..#..#.",
                "..#.....#.......#.#####",
                "..#....#....#...#....#.",
                ".###..#####..###.....#.",
            ]
        );
        // Frame numbers are zero padded to four digits, then grow.
        let mut a = vec![0u8; 160 * 120 * 3];
        let mut b = vec![0u8; 160 * 120 * 3];
        p.paint(7, &mut a);
        p.paint(12345, &mut b);
        // Four digits take 23 columns from x = 2, so the box ends at x = 26
        // and the bars show through there; a fifth digit starts at x = 26.
        let at = |p: &[u8], x: usize, y: usize| -> Rgb {
            let i = (y * 160 + x) * 3;
            [p[i], p[i + 1], p[i + 2]]
        };
        assert_eq!(at(&a, 25, 2), BLACK);
        assert_eq!(at(&a, 26, 2), [255, 255, 0]);
        assert_eq!(at(&b, 26, 2), WHITE, "top bar of the 5");
        assert_ne!(a, b);
    }

    #[test]
    fn testsrc_is_deterministic_at_any_size() {
        for size in ["1x1", "2x3", "17x9", "320x240", "641x479"] {
            let spec = format!("testsrc=s={size}:d=0.2");
            let mut a = open(&spec).unwrap();
            let mut b = open(&spec).unwrap();
            while let Some(fa) = a.next_frame().unwrap() {
                let fb = b.next_frame().unwrap().unwrap();
                assert_eq!(fa, fb);
            }
        }
    }
}
