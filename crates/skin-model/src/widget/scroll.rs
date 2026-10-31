//! Where the scroll stands for one frame: a copy of the simple skin's
//! `layout::Scale` (crates/render/src/simple.rs), parameterized only by the
//! uncovered lane length and the judgment line's y. With no cover and no
//! lift its every output bit equals the original's; tests/scroll.rs ties
//! the two together until the original is deleted.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::{Position, ScrollMap, TimeUs};

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
    /// The scale at display time `at` with hi-speed `visible_time`, over
    /// `span_px` pixels of lane ending at the line `line_y`. A visible
    /// time below 1 µs counts as 1 µs.
    ///
    /// The span holds the scroll of `visible_time` at the chart's initial
    /// rate. The pixels per position never change during a play, whatever
    /// the BPM at `at`.
    #[must_use]
    pub fn new(
        scroll: &ScrollMap,
        at: TimeUs,
        visible_time: TimeUs,
        span_px: f64,
        line_y: f64,
    ) -> Self {
        let visible = visible_time.0.max(1) as f64;
        let span = visible * scroll.initial_rate() as f64;
        Self {
            at_line: scroll.position_at(at),
            px_per_pos: span_px / span,
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
