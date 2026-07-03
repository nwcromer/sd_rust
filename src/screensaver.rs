//! Idle "Matrix"-style screensaver: green digital rain drawn across the whole
//! deck. Font-free (§7/§8) — the glyphs are hand-coded 5×7 bitmaps, not a real
//! font, so v1 keeps its no-font-dependency promise while still reading as
//! streams of mutating characters rather than plain bars.
//!
//! The rain flows across the key gaps, so a single deck-wide canvas is drawn each
//! frame and sliced into the 15 per-key tiles the runtime uploads.

use std::time::{SystemTime, UNIX_EPOCH};

use image::{imageops, Rgb, RgbImage};

use crate::config::{GRID_COLS, GRID_ROWS, KEY_COUNT};
use crate::render::{Tile, KEY_SIZE};

/// Whole-deck canvas the rain is drawn on before slicing into keys.
const CANVAS_W: u32 = GRID_COLS as u32 * KEY_SIZE;
const CANVAS_H: u32 = GRID_ROWS as u32 * KEY_SIZE;
/// Cell size in pixels; the canvas is a `COLS`×`ROWS` grid of one glyph each.
const CELL: u32 = 15;
const COLS: usize = (CANVAS_W / CELL) as usize;
const ROWS: i32 = (CANVAS_H / CELL) as i32;

/// Hand-drawn 5×7 glyphs (low 5 bits per row, bit 4 = leftmost column). Abstract
/// katakana-ish/symbol shapes — they only need to be varied and glyph-like at a
/// ~10px draw size, not real characters.
#[rustfmt::skip]
const GLYPHS: [[u8; 7]; 16] = [
    [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110], // 0
    [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110], // 1
    [0b00100, 0b11111, 0b00100, 0b11111, 0b00100, 0b01000, 0b10000], // ｷ
    [0b10010, 0b00100, 0b01000, 0b00010, 0b00100, 0b01000, 0b00110], // ﾂ
    [0b10000, 0b01000, 0b00100, 0b11111, 0b00010, 0b00100, 0b01000], // ﾝ
    [0b11111, 0b00010, 0b00100, 0b01111, 0b10100, 0b00100, 0b00100], // ｦ
    [0b00100, 0b00100, 0b11111, 0b00100, 0b00100, 0b00100, 0b00100], // +
    [0b01010, 0b01010, 0b01010, 0b01010, 0b10011, 0b10010, 0b00000], // ﾊ
    [0b11111, 0b00001, 0b00010, 0b00100, 0b00100, 0b01000, 0b01000], // 7
    [0b01000, 0b00010, 0b10000, 0b00010, 0b00001, 0b10010, 0b01100], // ｼ
    [0b00000, 0b00000, 0b11111, 0b00000, 0b11111, 0b00000, 0b00000], // =
    [0b00000, 0b00100, 0b01010, 0b10001, 0b00000, 0b00000, 0b00000], // ﾍ
    [0b11111, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11111], // box
    [0b01110, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b11111], // ﾆ
    [0b00000, 0b01010, 0b00000, 0b00100, 0b00000, 0b01010, 0b00000], // dots
    [0b01001, 0b01001, 0b01001, 0b01001, 0b01001, 0b00011, 0b00110], // ﾘ
];

/// One falling stream, occupying a fixed column of the cell grid.
struct Column {
    /// Cell-row of the leading (brightest) glyph; grows downward each step.
    head: i32,
    /// Trail length in cells (tail extends upward from the head).
    len: i32,
    /// Advance the head one cell every `period` frames (per-column fall speed).
    period: u8,
    phase: u8,
    active: bool,
    /// Glyph index shown at each cell-row; mutates so the stream shimmers.
    glyphs: Vec<u8>,
}

pub struct Matrix {
    columns: Vec<Column>,
    /// xorshift64 state (hand-rolled to avoid a `rand` dependency).
    rng: u64,
    canvas: RgbImage,
}

impl Matrix {
    pub fn new() -> Self {
        // Seed from the wall clock so each run's rain differs; the exact value
        // doesn't matter, only that it's non-zero for xorshift.
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15)
            | 1;
        let columns = (0..COLS)
            .map(|_| Column {
                head: 0,
                len: 0,
                period: 1,
                phase: 0,
                active: false,
                glyphs: vec![0; ROWS as usize],
            })
            .collect();
        Matrix { columns, rng: seed, canvas: RgbImage::new(CANVAS_W, CANVAS_H) }
    }

    fn rand(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    /// Random integer in `[lo, hi)` (`hi > lo`).
    fn rand_range(&mut self, lo: i32, hi: i32) -> i32 {
        lo + (self.rand() % (hi - lo) as u64) as i32
    }

    /// A random glyph index.
    fn rand_glyph(&mut self) -> u8 {
        (self.rand() % GLYPHS.len() as u64) as u8
    }

    /// Advance the animation one frame: fall existing streams, churn their
    /// glyphs, retire the ones fully off the bottom, and spawn new ones.
    pub fn step(&mut self) {
        for c in &mut self.columns {
            if !c.active {
                continue;
            }
            c.phase += 1;
            if c.phase >= c.period {
                c.phase = 0;
                c.head += 1;
            }
            if c.head - c.len > ROWS {
                c.active = false;
            }
        }
        // Glyph churn — index-based so we can call `rand()` (borrows `self`)
        // without holding a `&mut` into `self.columns`.
        for i in 0..COLS {
            if !self.columns[i].active {
                continue;
            }
            // A fresh glyph enters at the head each time it steps down.
            let head = self.columns[i].head;
            if self.columns[i].phase == 0 && (0..ROWS).contains(&head) {
                let g = self.rand_glyph();
                self.columns[i].glyphs[head as usize] = g;
            }
            // And an occasional flicker elsewhere in the stream.
            if self.rand().is_multiple_of(7) {
                let g = self.rand_glyph();
                let r = (self.rand() % ROWS as u64) as usize;
                self.columns[i].glyphs[r] = g;
            }
        }
        // A couple of spawn attempts per frame, each gated so the rain thins and
        // thickens rather than filling every column at once.
        for _ in 0..2 {
            if !self.rand().is_multiple_of(3) {
                continue;
            }
            let i = self.rand() as usize % COLS;
            if self.columns[i].active {
                continue;
            }
            let len = self.rand_range(4, 18);
            let period = self.rand_range(1, 4) as u8;
            let glyphs = (0..ROWS).map(|_| self.rand_glyph()).collect();
            self.columns[i] = Column { head: 0, len, period, phase: 0, active: true, glyphs };
        }
    }

    /// Draw the current frame and slice it into the 15 per-key tiles.
    pub fn render(&mut self) -> Vec<Tile> {
        // Split the borrows so we can read `columns` while writing `canvas`.
        let Matrix { columns, canvas, .. } = self;
        for px in canvas.pixels_mut() {
            *px = Rgb([0, 0, 0]);
        }
        for (i, c) in columns.iter().enumerate() {
            if !c.active {
                continue;
            }
            for d in 0..c.len {
                let row = c.head - d;
                if !(0..ROWS).contains(&row) {
                    continue;
                }
                let glyph = &GLYPHS[c.glyphs[row as usize] as usize];
                draw_glyph(canvas, i as u32 * CELL, row as u32 * CELL, glyph, cell_color(d, c.len));
            }
        }

        let mut tiles = Vec::with_capacity(KEY_COUNT);
        for row in 0..GRID_ROWS as u32 {
            for col in 0..GRID_COLS as u32 {
                tiles.push(
                    imageops::crop_imm(canvas, col * KEY_SIZE, row * KEY_SIZE, KEY_SIZE, KEY_SIZE)
                        .to_image(),
                );
            }
        }
        tiles
    }
}

/// Draw a 5×7 glyph, scaled ×2 and centred, at cell origin `(cx, cy)`.
fn draw_glyph(canvas: &mut RgbImage, cx: u32, cy: u32, glyph: &[u8; 7], color: Rgb<u8>) {
    const S: u32 = 2;
    const GW: u32 = 5;
    const GH: u32 = 7;
    let ox = cx + (CELL - GW * S) / 2;
    let oy = cy + (CELL - GH * S) / 2;
    for (gy, bits) in glyph.iter().enumerate() {
        for gx in 0..GW {
            if bits & (1 << (GW - 1 - gx)) == 0 {
                continue;
            }
            let px0 = ox + gx * S;
            let py0 = oy + gy as u32 * S;
            for py in py0..(py0 + S).min(CANVAS_H) {
                for px in px0..(px0 + S).min(CANVAS_W) {
                    canvas.put_pixel(px, py, color);
                }
            }
        }
    }
}

/// Colour a glyph by its distance `d` behind the head: bright white-green head,
/// fading through green to near-black at the tail.
fn cell_color(d: i32, len: i32) -> Rgb<u8> {
    if d == 0 {
        return Rgb([180, 255, 180]);
    }
    let t = 1.0 - d as f32 / len as f32; // 1 just behind the head → 0 at the tail
    Rgb([0, (60.0 + 195.0 * t) as u8, (30.0 * t) as u8])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stepping and rendering many frames never panics (glyphs stay in bounds),
    /// always yields the 15 correctly-sized tiles, and actually paints green rain
    /// (a green channel with black R/B, never stray colours).
    #[test]
    fn steps_and_renders_in_bounds() {
        let mut m = Matrix::new();
        let mut saw_green = false;
        for _ in 0..1000 {
            m.step();
            let tiles = m.render();
            assert_eq!(tiles.len(), KEY_COUNT);
            for t in &tiles {
                assert!(t.width() == KEY_SIZE && t.height() == KEY_SIZE);
                for p in t.pixels() {
                    // Rain is green: red only appears in the near-white head.
                    assert!(p[1] >= p[0] && p[1] >= p[2], "non-green pixel {p:?}");
                    saw_green |= p[1] > 0;
                }
            }
        }
        assert!(saw_green, "1000 frames produced no rain");
    }
}

