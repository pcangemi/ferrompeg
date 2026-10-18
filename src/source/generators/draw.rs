//! Minimal drawing on rgb24 pictures: rectangles, discs and text in a
//! built-in 5x7 bitmap font.

pub(crate) type Rgb = [u8; 3];

/// A mutable view of a tightly packed rgb24 picture. Every drawing call
/// clips to the picture, so shapes may start or extend outside it.
pub(crate) struct Canvas<'a> {
    width: usize,
    height: usize,
    pixels: &'a mut [u8],
}

impl<'a> Canvas<'a> {
    pub fn new(width: usize, height: usize, pixels: &'a mut [u8]) -> Self {
        debug_assert_eq!(pixels.len(), width * height * 3);
        Canvas {
            width,
            height,
            pixels,
        }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    #[cfg(test)]
    pub fn get(&self, x: usize, y: usize) -> Rgb {
        let i = (y * self.width + x) * 3;
        [self.pixels[i], self.pixels[i + 1], self.pixels[i + 2]]
    }

    /// One row of pixels, three bytes each.
    pub fn row_mut(&mut self, y: usize) -> &mut [u8] {
        let stride = self.width * 3;
        &mut self.pixels[y * stride..(y + 1) * stride]
    }

    /// Clips a rectangle given in signed coordinates to the picture and
    /// returns its column and row ranges.
    fn clip(
        &self,
        x: i64,
        y: i64,
        w: i64,
        h: i64,
    ) -> (std::ops::Range<usize>, std::ops::Range<usize>) {
        let x0 = x.clamp(0, self.width as i64) as usize;
        let x1 = x.saturating_add(w).clamp(0, self.width as i64) as usize;
        let y0 = y.clamp(0, self.height as i64) as usize;
        let y1 = y.saturating_add(h).clamp(0, self.height as i64) as usize;
        (x0..x1.max(x0), y0..y1.max(y0))
    }

    pub fn fill_rect(&mut self, x: i64, y: i64, w: i64, h: i64, color: Rgb) {
        let (cols, rows) = self.clip(x, y, w, h);
        for y in rows {
            let row = self.row_mut(y);
            for px in row[cols.start * 3..cols.end * 3].chunks_exact_mut(3) {
                px.copy_from_slice(&color);
            }
        }
    }

    /// Inverts every pixel inside the disc of radius `r` centered on
    /// (`cx`, `cy`). Inversion stays visible on any background except
    /// mid gray, which is why the moving marker of the test pattern uses it.
    pub fn invert_disc(&mut self, cx: i64, cy: i64, r: i64) {
        // Radii are a fraction of the picture size, far from overflowing.
        let r = r.clamp(0, 1 << 20);
        let (cols, rows) = self.clip(
            cx.saturating_sub(r),
            cy.saturating_sub(r),
            2 * r + 1,
            2 * r + 1,
        );
        let r2 = r * r;
        for y in rows {
            let dy = y as i64 - cy;
            let row = self.row_mut(y);
            for x in cols.clone() {
                let dx = x as i64 - cx;
                if dx * dx + dy * dy <= r2 {
                    for b in &mut row[x * 3..x * 3 + 3] {
                        *b = 255 - *b;
                    }
                }
            }
        }
    }

    /// Draws `text` with its top left corner at (`x`, `y`), each font pixel
    /// a `scale` x `scale` square. Characters missing from the font are
    /// left blank but still advance.
    pub fn draw_text(&mut self, x: i64, y: i64, text: &str, scale: i64, color: Rgb) {
        let mut pen = x;
        for c in text.chars() {
            if let Some(rows) = glyph(c) {
                for (gy, bits) in rows.iter().enumerate() {
                    for gx in 0..GLYPH_WIDTH {
                        if bits & (1 << (GLYPH_WIDTH - 1 - gx)) != 0 {
                            // Saturating: text placed absurdly far away
                            // clips instead of overflowing.
                            self.fill_rect(
                                pen.saturating_add((gx as i64).saturating_mul(scale)),
                                y.saturating_add((gy as i64).saturating_mul(scale)),
                                scale,
                                scale,
                                color,
                            );
                        }
                    }
                }
            }
            pen = pen.saturating_add((ADVANCE as i64).saturating_mul(scale));
        }
    }
}

/// Glyph cell size in font pixels.
pub(crate) const GLYPH_WIDTH: usize = 5;
pub(crate) const GLYPH_HEIGHT: usize = 7;

/// Horizontal distance between the left edges of consecutive characters:
/// the glyph plus one column of spacing.
pub(crate) const ADVANCE: usize = GLYPH_WIDTH + 1;

/// Width in pixels of `text` drawn at `scale`, without trailing spacing.
pub(crate) fn text_width(text: &str, scale: usize) -> usize {
    let n = text.chars().count();
    if n == 0 { 0 } else { (n * ADVANCE - 1) * scale }
}

/// Rows of a glyph, top first; bit 4 is the leftmost column.
///
/// The digits are drawn with a flat top on 3 and 5, a diagonal 4 and a
/// slashed 0 so the counter stays readable when scaled down or blurred by
/// a lossy encoder.
fn glyph(c: char) -> Option<[u8; GLYPH_HEIGHT]> {
    Some(match c {
        '0' => [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        '1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        '2' => [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
        ],
        '3' => [
            0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110,
        ],
        '4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        '5' => [
            0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
        ],
        '6' => [
            0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        '7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        '8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        '9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
        ],
        ':' => [0, 0b01100, 0b01100, 0, 0b01100, 0b01100, 0],
        '.' => [0, 0, 0, 0, 0, 0b01100, 0b01100],
        '-' => [0, 0, 0, 0b11111, 0, 0, 0],
        ' ' => [0; GLYPH_HEIGHT],
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const INK: Rgb = [255, 255, 255];

    /// Renders `text` at scale 1 on a black canvas just large enough and
    /// returns it as lines of `#` and `.`.
    fn render(text: &str) -> Vec<String> {
        let w = text_width(text, 1);
        let mut buf = vec![0u8; w * GLYPH_HEIGHT * 3];
        let mut c = Canvas::new(w, GLYPH_HEIGHT, &mut buf);
        c.draw_text(0, 0, text, 1, INK);
        (0..GLYPH_HEIGHT)
            .map(|y| {
                (0..w)
                    .map(|x| if c.get(x, y) == INK { '#' } else { '.' })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn digits_render_as_designed() {
        let expected = [
            ".###....#....###..#####....#..#####...##..#####..###...###.",
            "#...#..##...#...#....#....##..#......#........#.#...#.#...#",
            "#..##...#.......#...#....#.#..####..#........#..#...#.#...#",
            "#.#.#...#......#.....#..#..#......#.####....#....###...####",
            "##..#...#.....#.......#.#####.....#.#...#..#....#...#.....#",
            "#...#...#....#....#...#....#..#...#.#...#..#....#...#....#.",
            ".###...###..#####..###.....#...###...###...#.....###...##..",
        ];
        assert_eq!(render("0123456789"), expected);
    }

    #[test]
    fn punctuation_and_unknown_characters() {
        assert_eq!(
            render("1:2"),
            [
                "..#..........###.",
                ".##....##...#...#",
                "..#....##.......#",
                "..#............#.",
                "..#....##.....#..",
                "..#....##....#...",
                ".###........#####",
            ]
        );
        // Unknown characters advance without drawing.
        assert!(render("?").iter().all(|r| r == "....."));
    }

    #[test]
    fn scaling_and_clipping() {
        let mut buf = vec![0u8; 12 * 16 * 3];
        let mut c = Canvas::new(12, 16, &mut buf);
        c.draw_text(0, 0, "1", 2, INK);
        // The stem of the 1 is font column 2, so pixels 4 and 5 at scale 2.
        for y in 0..14 {
            assert_eq!(c.get(4, y), INK, "row {y}");
            assert_eq!(c.get(5, y), INK, "row {y}");
        }
        assert_eq!(c.get(3, 0), [0, 0, 0]);
        assert_eq!(c.get(2, 2), INK, "the flag of the 1");
        // Text far outside or straddling the edges must not panic.
        c.draw_text(-7, -9, "88", 3, INK);
        c.draw_text(10, 14, "0123", 4, INK);
        c.draw_text(i64::MAX - 10, i64::MIN + 10, "8", 1000, INK);
        c.fill_rect(i64::MIN, i64::MIN, i64::MAX, i64::MAX, INK);
        c.invert_disc(-100, -100, 5);
        c.invert_disc(6, 8, 0);
        assert_eq!(c.get(6, 8), [0, 0, 0]);
    }

    #[test]
    fn disc_inverts_a_round_area() {
        let mut buf = vec![10u8; 9 * 9 * 3];
        let mut c = Canvas::new(9, 9, &mut buf);
        c.invert_disc(4, 4, 3);
        assert_eq!(c.get(4, 4), [245, 245, 245]);
        assert_eq!(c.get(4, 1), [245, 245, 245]);
        assert_eq!(c.get(7, 4), [245, 245, 245]);
        // Corners of the bounding square lie outside the circle.
        assert_eq!(c.get(1, 1), [10, 10, 10]);
        assert_eq!(c.get(0, 4), [10, 10, 10]);
    }

    #[test]
    fn text_widths() {
        assert_eq!(text_width("", 3), 0);
        assert_eq!(text_width("0", 1), 5);
        assert_eq!(text_width("0000", 2), 46);
    }
}
