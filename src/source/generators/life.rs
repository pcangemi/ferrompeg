//! `life`: Conway's Game of Life, one cell per pixel, on a grid that wraps
//! around at the edges (a torus), so patterns leaving one side come back
//! on the other and the population never hits a wall.

use super::draw::Rgb;
use super::rng::Rng;
use super::{Painter, VideoGenerator, VideoOptions};
use crate::error::{Error, Result};
use crate::filter::Args;
use crate::source::Source;

/// Empty cells: a dark blue that keeps the live cells bright by contrast.
const BACKGROUND: Rgb = [12, 16, 36];
/// Cells alive in this generation and the previous one.
const ALIVE: Rgb = [255, 208, 96];
/// Cells that came to life in this generation.
const BORN: Rgb = [96, 255, 208];
/// Cells that died in this generation. Later generations fade them back
/// to the background, leaving a short trail behind moving patterns.
const DYING: Rgb = [224, 48, 96];
/// Generations a trail takes to fade out completely.
const FADE: u8 = 8;

/// Default share of live cells in the initial soup. Densities around a
/// third produce long lived chaotic activity; much denser soups die of
/// overcrowding within a few generations.
const DEFAULT_RATIO: f64 = 0.3;

pub(crate) fn create(args: &Args) -> Result<Box<dyn Source>> {
    let options = VideoOptions::parse(args, "life", &["seed", "ratio"])?;
    let seed = super::parse_seed(args)?;
    let ratio = args.f64(&["ratio"])?.unwrap_or(DEFAULT_RATIO);
    if !(0.0..=1.0).contains(&ratio) {
        return Err(Error::invalid_argument(
            "life ratio must be between 0 and 1",
        ));
    }
    let life = Life::random(options.width as usize, options.height as usize, seed, ratio);
    Ok(Box::new(VideoGenerator::new(options, life)))
}

struct Life {
    width: usize,
    height: usize,
    /// 1 for a live cell, 0 for an empty one, row by row.
    cells: Vec<u8>,
    /// The previous generation, to tell births and deaths from survivors.
    previous: Vec<u8>,
    /// Generations since each empty cell died, saturating at `FADE`.
    since_death: Vec<u8>,
    /// Generation currently held in `cells`.
    generation: u64,
}

impl Life {
    /// Fills the grid with independent cells, each alive with probability
    /// `ratio`, from a seeded generator so the soup is reproducible.
    fn random(width: usize, height: usize, seed: u64, ratio: f64) -> Self {
        let mut rng = Rng::new(seed);
        let cells = (0..width * height)
            .map(|_| u8::from(rng.next_f64() < ratio))
            .collect();
        Life::from_cells(width, height, cells)
    }

    fn from_cells(width: usize, height: usize, cells: Vec<u8>) -> Self {
        debug_assert_eq!(cells.len(), width * height);
        Life {
            width,
            height,
            previous: cells.clone(),
            since_death: vec![FADE; cells.len()],
            cells,
            generation: 0,
        }
    }

    /// Advances one generation: a live cell with two or three live
    /// neighbors survives, an empty cell with exactly three is born, every
    /// other cell is empty in the next generation.
    fn step(&mut self) {
        let (w, h) = (self.width, self.height);
        std::mem::swap(&mut self.previous, &mut self.cells);
        let old = &self.previous;
        for y in 0..h {
            // Wrapping row and column neighbors. Adding the size before
            // subtracting keeps the arithmetic unsigned.
            let up = (y + h - 1) % h * w;
            let row = y * w;
            let down = (y + 1) % h * w;
            for x in 0..w {
                let left = (x + w - 1) % w;
                let right = (x + 1) % w;
                let n = old[up + left]
                    + old[up + x]
                    + old[up + right]
                    + old[row + left]
                    + old[row + right]
                    + old[down + left]
                    + old[down + x]
                    + old[down + right];
                let alive = old[row + x] == 1;
                self.cells[row + x] = u8::from(n == 3 || (alive && n == 2));
            }
        }
        for ((age, &now), &before) in self
            .since_death
            .iter_mut()
            .zip(&self.cells)
            .zip(&self.previous)
        {
            *age = match (before, now) {
                (1, 0) => 1,
                (_, 0) => age.saturating_add(1).min(FADE),
                _ => 0,
            };
        }
        self.generation += 1;
    }

    fn color(&self, i: usize) -> Rgb {
        match (self.previous[i], self.cells[i]) {
            (1, 1) => ALIVE,
            (_, 1) => BORN,
            _ => trail(self.since_death[i]),
        }
    }
}

/// Color of an empty cell `age` generations after it died: the dying color
/// at age 1, then a linear fade to the background.
fn trail(age: u8) -> Rgb {
    if age == 0 || age >= FADE {
        return BACKGROUND;
    }
    let k = (age - 1) as u32;
    let span = (FADE - 1) as u32;
    let mut out = [0u8; 3];
    for c in 0..3 {
        let (from, to) = (DYING[c] as u32, BACKGROUND[c] as u32);
        out[c] = ((from * (span - k) + to * k) / span) as u8;
    }
    out
}

impl Painter for Life {
    fn paint(&mut self, index: u64, pixels: &mut [u8]) {
        while self.generation < index {
            self.step();
        }
        for (i, px) in pixels.chunks_exact_mut(3).enumerate() {
            px.copy_from_slice(&self.color(i));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Frame;
    use crate::source::open;

    /// Builds a grid from rows of `#` (alive) and `.` (empty).
    fn grid(rows: &[&str]) -> Life {
        let cells = rows
            .iter()
            .flat_map(|r| r.bytes().map(|b| u8::from(b == b'#')))
            .collect();
        Life::from_cells(rows[0].len(), rows.len(), cells)
    }

    fn picture(life: &Life) -> Vec<String> {
        (0..life.height)
            .map(|y| {
                (0..life.width)
                    .map(|x| {
                        if life.cells[y * life.width + x] == 1 {
                            '#'
                        } else {
                            '.'
                        }
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn blinker_oscillates() {
        let mut life = grid(&[".....", ".....", ".###.", ".....", "....."]);
        life.step();
        assert_eq!(
            picture(&life),
            [".....", "..#..", "..#..", "..#..", "....."]
        );
        life.step();
        assert_eq!(
            picture(&life),
            [".....", ".....", ".###.", ".....", "....."]
        );
    }

    #[test]
    fn glider_moves_diagonally_and_wraps() {
        let start = [
            ".#......", "..#.....", "###.....", "........", "........", "........", "........",
            "........",
        ];
        let mut life = grid(&start);
        for _ in 0..4 {
            life.step();
        }
        assert_eq!(
            picture(&life),
            [
                "........", "..#.....", "...#....", ".###....", "........", "........", "........",
                "........",
            ]
        );
        // Every four generations it moves one cell down and right, so 32
        // generations bring it around the 8x8 torus to where it started.
        for _ in 4..32 {
            life.step();
        }
        assert_eq!(picture(&life), start);
        assert_eq!(life.cells.iter().filter(|&&c| c == 1).count(), 5);
    }

    #[test]
    fn block_is_still_and_lonely_cells_die() {
        let mut life = grid(&["......", ".##...", ".##...", "......", "....#.", "......"]);
        life.step();
        assert_eq!(
            picture(&life),
            ["......", ".##...", ".##...", "......", "......", "......"]
        );
    }

    #[test]
    fn colors_tell_births_survivors_and_deaths() {
        let mut life = grid(&[".....", ".....", ".###.", ".....", "....."]);
        let mut pixels = vec![0u8; 5 * 5 * 3];
        let at = |p: &[u8], x: usize, y: usize| -> Rgb {
            let i = (y * 5 + x) * 3;
            [p[i], p[i + 1], p[i + 2]]
        };
        life.paint(0, &mut pixels);
        assert_eq!(at(&pixels, 1, 2), ALIVE);
        assert_eq!(at(&pixels, 0, 0), BACKGROUND);
        life.paint(1, &mut pixels);
        assert_eq!(at(&pixels, 2, 2), ALIVE, "center survives");
        assert_eq!(at(&pixels, 2, 1), BORN);
        assert_eq!(at(&pixels, 2, 3), BORN);
        assert_eq!(at(&pixels, 1, 2), DYING);
        assert_eq!(at(&pixels, 3, 2), DYING);
        assert_eq!(at(&pixels, 0, 0), BACKGROUND);
        life.paint(2, &mut pixels);
        // The ends that died last generation are born again; the ends of
        // the vertical phase just died.
        assert_eq!(at(&pixels, 1, 2), BORN);
        assert_eq!(at(&pixels, 2, 1), DYING);
    }

    #[test]
    fn trails_fade_to_the_background() {
        assert_eq!(trail(1), DYING);
        assert_eq!(trail(FADE), BACKGROUND);
        assert_eq!(trail(0), BACKGROUND);
        let mut last = 0u32;
        for age in 1..FADE {
            let c = trail(age);
            let distance: u32 = (0..3)
                .map(|i| (c[i] as i32 - BACKGROUND[i] as i32).unsigned_abs())
                .sum();
            assert!(age == 1 || distance < last, "age {age}");
            last = distance;
        }
        // A lone cell dies, then fades out over the following generations.
        let mut life = grid(&["...", ".#.", "..."]);
        life.step();
        assert_eq!(life.color(4), DYING);
        for _ in 0..FADE {
            life.step();
        }
        assert_eq!(life.color(4), BACKGROUND);
    }

    #[test]
    fn seeded_soup_is_reproducible() {
        let a = Life::random(64, 48, 9, 0.3);
        let b = Life::random(64, 48, 9, 0.3);
        let c = Life::random(64, 48, 10, 0.3);
        assert_eq!(a.cells, b.cells);
        assert_ne!(a.cells, c.cells);
        let density = a.cells.iter().filter(|&&v| v == 1).count() as f64 / (64.0 * 48.0);
        assert!((density - 0.3).abs() < 0.05, "density {density}");
        assert!(Life::random(8, 8, 1, 0.0).cells.iter().all(|&v| v == 0));
        assert!(Life::random(8, 8, 1, 1.0).cells.iter().all(|&v| v == 1));
    }

    #[test]
    fn source_frames_follow_generations() {
        let mut s = open("life=s=40x30:seed=3:ratio=0.4:d=0.4:r=10").unwrap();
        let mut life = Life::random(40, 30, 3, 0.4);
        let mut expected = vec![0u8; 40 * 30 * 3];
        let mut index = 0;
        while let Some(Frame::Video(f)) = s.next_frame().unwrap() {
            life.paint(index, &mut expected);
            assert_eq!(f.planes[0], expected, "frame {index}");
            index += 1;
        }
        assert_eq!(index, 4);
    }

    #[test]
    fn tiny_grids_and_bad_options() {
        // On a 1x1 torus a cell is its own eight neighbors; on 2x1 they
        // overlap. Neither may panic.
        let mut one = grid(&["#"]);
        one.step();
        assert_eq!(picture(&one), ["."]);
        let mut two = grid(&["##"]);
        two.step();
        two.step();
        assert!(open("life=ratio=1.5").is_err());
        assert!(open("life=seed=abc").is_err());
        assert!(open("life=s=3x2:seed=-5:ratio=0.5").is_ok());
    }
}
