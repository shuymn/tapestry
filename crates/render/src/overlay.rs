//! The measurement overlay (design.md "Latency Figures"): lines of text in
//! the built-in bitmap font, appended after any play skin's sprites from
//! a reserve of the batch that only it fills, so a skin that fills its own
//! capacity, sprites or runs, never takes it off the screen.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use crate::SpriteBatch;
use crate::simple::{TEXT, text};

/// Sprites the overlay may draw: one per glyph that is not a space. A
/// frame's overlay past it loses its last glyphs, counted as dropped.
pub const OVERLAY_SPRITES: u32 = 1_024;

/// Runs the overlay takes: its glyphs are all alpha-blended and unclipped,
/// so one run, or none when the skin's last run is the same.
pub const OVERLAY_RUNS: u32 = 1;

/// Left edge and top of the overlay's first line, its glyph scale and the
/// distance between its lines, in the play skin's canvas pixels: right of
/// the simple skin's lanes, glyphs 15×21, up to 50 characters a line.
const LEFT: f32 = 1000.0;
const TOP: f32 = 80.0;
const SCALE: f32 = 3.0;
const LINE: f32 = 28.0;

/// Opens `out`'s reserve and pushes `lines` into it, after everything the
/// skin pushed: one line every 28 canvas pixels, characters the font does
/// not have drawn as spaces.
pub(crate) fn draw(lines: &[&str], out: &mut SpriteBatch) {
    out.open_reserve();
    for (i, line) in lines.iter().enumerate() {
        let y = TOP + i as f32 * LINE;
        text(out, LEFT, y, SCALE, TEXT, line.as_bytes());
    }
}
