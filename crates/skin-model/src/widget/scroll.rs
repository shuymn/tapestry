//! Where the scroll stands for one frame: the playhead's position at the
//! judgment line and the pixels per unit of position, from the exact
//! original-lane gain (design.md "Play readouts"). The arithmetic from a
//! position to pixels is the simple skin's `layout::Scale`
//! (crates/render/src/simple.rs); tests/scroll.rs ties the two together
//! until the original is deleted.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::{BEAT, Position};

/// The lanes' exact original-lane gain: the active multiplier `H` as
/// `num / den`, one measure of four beats taking `1 / H` of the original
/// lane. Within 1/100 to 20, both terms positive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gain {
    num: u64,
    den: u64,
}

impl Gain {
    /// `H` 1, for the crate's tests.
    #[cfg(test)]
    pub(crate) const ONE: Self = Self { num: 1, den: 1 };

    /// `num / den`, if both are positive and `1/100 ≤ num/den ≤ 20`
    /// (compared exactly); `None` otherwise.
    #[must_use]
    pub const fn new(num: u64, den: u64) -> Option<Self> {
        // Widened, so neither product overflows.
        let (n, d) = (num as u128, den as u128);
        if n > 0 && d > 0 && 100 * n >= d && n <= 20 * d {
            Some(Self { num, den })
        } else {
            None
        }
    }

    /// The numerator.
    #[must_use]
    pub const fn num(self) -> u64 {
        self.num
    }

    /// The denominator, never 0.
    #[must_use]
    pub const fn den(self) -> u64 {
        self.den
    }

    /// Pixels per unit of position (2^-80 beat) over an original lane of
    /// `lane_len_px` pixels: `num · lane_len_px / (den · 4 · BEAT)`. The
    /// only conversion of a gain to pixels; finite and positive for a
    /// lane length of at least 1.
    #[must_use]
    pub fn px_per_pos(self, lane_len_px: f64) -> f64 {
        let measure = 4.0 * BEAT as f64;
        self.num as f64 * lane_len_px / (self.den as f64 * measure)
    }
}

/// The playhead at the display time and the pixels per unit of position.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scale {
    /// The display position at the judgment line.
    pub at_line: Position,
    /// Pixels per unit of position (2^-80 beat).
    pub px_per_pos: f64,
    /// The judgment line's y.
    pub line_y: f64,
}

impl Scale {
    /// The scale with the playhead at `at_line` and the gain `gain` over
    /// an original lane of `lane_len_px` pixels, the judgment line at
    /// `line_y`. For a fixed gain the pixels per position are independent
    /// of the BPM at the playhead.
    #[must_use]
    pub fn from_gain(at_line: Position, gain: Gain, lane_len_px: f64, line_y: f64) -> Self {
        Self {
            at_line,
            px_per_pos: gain.px_per_pos(lane_len_px),
            line_y,
        }
    }

    /// The vertical center of something at display position `p`.
    #[must_use]
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the original's casts, kept for equal bits"
    )]
    pub fn y(&self, p: Position) -> f32 {
        let ahead = p.minus(self.at_line) as f64;
        (self.line_y - ahead * self.px_per_pos) as f32
    }

    /// The positions whose vertical center lies `below` pixels under the
    /// judgment line through `above` pixels over it, widened to whole
    /// units.
    #[must_use]
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the original's casts, kept for equal bits; `as` saturates"
    )]
    pub fn positions(&self, below: f32, above: f32) -> (Position, Position) {
        let units = |px: f32| (f64::from(px) / self.px_per_pos).ceil() as i128;
        (
            Position(self.at_line.0.saturating_sub(units(below))),
            Position(self.at_line.0.saturating_add(units(above))),
        )
    }
}
