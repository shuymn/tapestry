//! The texture atlas, built in code at startup: an ASCII 5×7 bitmap font
//! and one solid white block for untextured sprites (D9).
//!
//! The atlas is `ATLAS_SIZE` square, RGBA8. Glyphs for the codes 0x20 to
//! 0x5F sit in 8×8 cells, 16 to a row from the top left; each glyph's
//! pixels fill the top left 5×7 of its cell. Lowercase letters draw as
//! uppercase, and every other code draws as a space. The solid block is the
//! bottom right 8×8 cell.

/// Width and height of the atlas texture, in texels.
pub const ATLAS_SIZE: u32 = 128;
/// Width of a glyph, in texels.
pub const GLYPH_W: u32 = 5;
/// Height of a glyph, in texels.
pub const GLYPH_H: u32 = 7;
/// Side of a glyph cell.
const CELL: u32 = 8;
/// Glyph cells per atlas row.
const CELLS_PER_ROW: u32 = 16;
/// The first code with a cell.
const FIRST: u8 = 0x20;
/// The last code with a cell.
const LAST: u8 = 0x5F;
/// Top left texel of the solid block.
const SOLID: u32 = ATLAS_SIZE - CELL;

/// The rows of `c`'s glyph, top first. Bit 4 is the leftmost column.
#[must_use]
pub const fn glyph_rows(c: u8) -> [u8; 7] {
    match c.to_ascii_uppercase() {
        b'0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        b'1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        b'2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        b'3' => [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        b'4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        b'5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        b'6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        b'7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        b'8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        b'9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        b'A' => [0x0E, 0x11, 0x11, 0x11, 0x1F, 0x11, 0x11],
        b'B' => [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
        b'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        b'D' => [0x1C, 0x12, 0x11, 0x11, 0x11, 0x12, 0x1C],
        b'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        b'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        b'G' => [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F],
        b'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        b'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        b'J' => [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0C],
        b'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        b'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        b'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        b'N' => [0x11, 0x11, 0x19, 0x15, 0x13, 0x11, 0x11],
        b'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        b'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        b'Q' => [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D],
        b'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        b'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        b'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        b'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        b'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
        b'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x15, 0x0A],
        b'X' => [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11],
        b'Y' => [0x11, 0x11, 0x11, 0x0A, 0x04, 0x04, 0x04],
        b'Z' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F],
        b'.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C],
        b',' => [0x00, 0x00, 0x00, 0x00, 0x0C, 0x04, 0x08],
        b':' => [0x00, 0x0C, 0x0C, 0x00, 0x0C, 0x0C, 0x00],
        b'-' => [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00],
        b'+' => [0x00, 0x04, 0x04, 0x1F, 0x04, 0x04, 0x00],
        b'=' => [0x00, 0x00, 0x1F, 0x00, 0x1F, 0x00, 0x00],
        b'/' => [0x00, 0x01, 0x02, 0x04, 0x08, 0x10, 0x00],
        b'%' => [0x18, 0x19, 0x02, 0x04, 0x08, 0x13, 0x03],
        b'(' => [0x02, 0x04, 0x08, 0x08, 0x08, 0x04, 0x02],
        b')' => [0x08, 0x04, 0x02, 0x02, 0x02, 0x04, 0x08],
        b'<' => [0x02, 0x04, 0x08, 0x10, 0x08, 0x04, 0x02],
        b'>' => [0x08, 0x04, 0x02, 0x01, 0x02, 0x04, 0x08],
        b'_' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1F],
        b'?' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x00, 0x04],
        b'!' => [0x04, 0x04, 0x04, 0x04, 0x04, 0x00, 0x04],
        b'\'' => [0x0C, 0x04, 0x08, 0x00, 0x00, 0x00, 0x00],
        _ => [0; 7],
    }
}

/// The cell `c` draws from: its own, its uppercase letter's, or the
/// space's.
const fn cell_of(c: u8) -> u32 {
    let c = c.to_ascii_uppercase();
    if c < FIRST || c > LAST {
        0
    } else {
        (c - FIRST) as u32
    }
}

/// The texture coordinates of `c`'s glyph: left, top, right, bottom.
#[must_use]
pub(crate) fn glyph_uv(c: u8) -> [f32; 4] {
    let cell = cell_of(c);
    let x = (cell % CELLS_PER_ROW) * CELL;
    let y = (cell / CELLS_PER_ROW) * CELL;
    texels(x, y, x + GLYPH_W, y + GLYPH_H)
}

/// The texture coordinates of the middle of the solid block, as a
/// zero-size rectangle, so every sample is white.
#[must_use]
pub(crate) fn solid_uv() -> [f32; 4] {
    let c = (SOLID as f32 + CELL as f32 / 2.0) / ATLAS_SIZE as f32;
    [c, c, c, c]
}

fn texels(x0: u32, y0: u32, x1: u32, y1: u32) -> [f32; 4] {
    let s = ATLAS_SIZE as f32;
    [x0 as f32 / s, y0 as f32 / s, x1 as f32 / s, y1 as f32 / s]
}

/// The atlas texels, RGBA8 row by row: white where a glyph or the solid
/// block is set, transparent elsewhere.
#[must_use]
pub(crate) fn build_atlas() -> Vec<u8> {
    let mut texels = vec![0u8; (ATLAS_SIZE * ATLAS_SIZE * 4) as usize];
    let mut set = |x: u32, y: u32| {
        let i = ((y * ATLAS_SIZE + x) * 4) as usize;
        texels[i..i + 4].copy_from_slice(&[255; 4]);
    };
    for c in FIRST..=LAST {
        let cell = cell_of(c);
        let (cx, cy) = ((cell % CELLS_PER_ROW) * CELL, (cell / CELLS_PER_ROW) * CELL);
        for (row, bits) in (0..).zip(glyph_rows(c)) {
            for col in 0..GLYPH_W {
                if bits & (0x10 >> col) != 0 {
                    set(cx + col, cy + row);
                }
            }
        }
    }
    for y in SOLID..ATLAS_SIZE {
        for x in SOLID..ATLAS_SIZE {
            set(x, y);
        }
    }
    texels
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(atlas: &[u8], x: u32, y: u32) -> bool {
        atlas[((y * ATLAS_SIZE + x) * 4) as usize + 3] == 255
    }

    #[test]
    fn glyph_cells_hold_their_bits() {
        let atlas = build_atlas();
        // 'A' is code 0x41, cell 0x21: column 1, row 2, at texel (8, 16).
        // Its top row is .###. and its fifth row #####.
        let top: Vec<bool> = (0..5).map(|x| lit(&atlas, 8 + x, 16)).collect();
        assert_eq!(top, [false, true, true, true, false]);
        let fifth: Vec<bool> = (0..5).map(|x| lit(&atlas, 8 + x, 20)).collect();
        assert_eq!(fifth, [true; 5]);
        // The cell's padding column and row stay empty.
        assert!((0..8).all(|y| !lit(&atlas, 8 + 5, 16 + y)));
        assert!((0..8).all(|x| !lit(&atlas, 8 + x, 16 + 7)));
        // '7' is code 0x37, cell 0x17: column 7, row 1, at texel (56, 8).
        let top7: Vec<bool> = (0..5).map(|x| lit(&atlas, 56 + x, 8)).collect();
        assert_eq!(top7, [true; 5]);
        assert_eq!(
            glyph_uv(b'A'),
            [8.0 / 128.0, 16.0 / 128.0, 13.0 / 128.0, 23.0 / 128.0]
        );
    }

    #[test]
    fn lowercase_draws_uppercase_and_unknown_draws_blank() {
        assert_eq!(glyph_uv(b'p'), glyph_uv(b'P'));
        assert_eq!(glyph_rows(b'p'), glyph_rows(b'P'));
        for c in [b'~', b'{', 0x7F, 0x00, 0xC0] {
            assert_eq!(glyph_uv(c), glyph_uv(b' '), "code {c:#x}");
            assert_eq!(glyph_rows(c), [0; 7], "code {c:#x}");
        }
        assert_ne!(glyph_uv(b'_'), glyph_uv(b' '));
    }

    #[test]
    fn solid_block_is_white() {
        let atlas = build_atlas();
        for (x, y) in [(120, 120), (127, 127), (124, 124)] {
            assert!(lit(&atlas, x, y));
        }
        assert!(!lit(&atlas, 119, 124));
        assert_eq!(solid_uv(), [124.0 / 128.0; 4]);
    }
}
