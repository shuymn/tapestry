//! The scroll arithmetic against the simple skin's original: `reference`
//! holds the original's constants and `Scale` verbatim (crates/render/src/
//! simple.rs, `layout`), a property test checks that the model's `Scale`,
//! given the original's line, pixels per position and playhead position,
//! places and windows positions to the same bits, and a drift guard fails
//! once the original changes without the reference. Both skins take that
//! position from the exact playhead (`ExactPlayhead`); only the arithmetic
//! from a position to pixels is tied here, the model's pixels per position
//! coming from the gain (`Gain::px_per_pos`). Render is Windows-only, so the
//! original is read as text, not linked. The guard goes when the original
//! is deleted.

use bms_model::{Position, ScrollMap, TimeUs};
use proptest::prelude::*;
use skin_model::{Gain, Scale};

#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
mod reference {
    use bms_model::{Position, TimeUs};

    // BEGIN render copy
    /// Top of the lanes, where a note appears `visible_time` before it
    /// reaches the judgment line.
    pub const LANE_TOP: f32 = 80.0;
    /// The judgment line.
    pub const JUDGE_Y: f32 = 880.0;
    // END render copy

    // BEGIN render copy
    /// Where the scroll stands for one frame: the playhead at the display
    /// time and the pixels per unit of position.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct Scale {
        /// The display position at the judgment line.
        pub at_line: Position,
        /// Pixels per unit of position (2^-80 beat).
        pub px_per_pos: f64,
    }

    impl Scale {
        /// The scale with the playhead at `at_line` and hi-speed
        /// `visible_time`. A visible time below 1 µs counts as 1 µs.
        ///
        /// The lane from [`LANE_TOP`] to [`JUDGE_Y`] holds the scroll of
        /// `visible_time` at the chart's initial rate, `initial_rate` units
        /// per µs. The pixels per position never change during a play,
        /// whatever the BPM at the playhead.
        #[must_use]
        pub fn new(at_line: Position, initial_rate: i128, visible_time: TimeUs) -> Self {
            let visible = visible_time.0.max(1) as f64;
            let span = visible * initial_rate as f64;
            Self {
                at_line,
                px_per_pos: f64::from(JUDGE_Y - LANE_TOP) / span,
            }
        }

        /// The vertical center of something at display position `p`.
        #[must_use]
        pub fn y(&self, p: Position) -> f32 {
            let ahead = p.minus(self.at_line) as f64;
            (f64::from(JUDGE_Y) - ahead * self.px_per_pos) as f32
        }

        /// The positions whose vertical center lies `below` pixels under
        /// the judgment line through `above` pixels over it, widened to
        /// whole units.
        #[must_use]
        pub fn positions(&self, below: f32, above: f32) -> (Position, Position) {
            let units = |px: f32| (f64::from(px) / self.px_per_pos).ceil() as i128;
            (
                Position(self.at_line.0.saturating_sub(units(below))),
                Position(self.at_line.0.saturating_add(units(above))),
            )
        }
    }
    // END render copy
}

/// The simple skin's span as the lanes widget computes it with no cover
/// and no lift: the raised line (`line`) less the scroll top (`top`).
fn span() -> f64 {
    let (top, line) = (80_i32, 880_i32);
    f64::from(line) - f64::from(top)
}

/// The text between each pair of markers in `src`, whitespace collapsed.
fn blocks(src: &str) -> Vec<String> {
    let begin = ["// BEGIN", " render copy"].concat();
    let end = ["// END", " render copy"].concat();
    let mut out = Vec::new();
    let mut rest = src;
    while let Some(i) = rest.find(&begin) {
        let after = &rest[i + begin.len()..];
        let j = after.find(&end).expect("an end marker");
        out.push(normalized(&after[..j]));
        rest = &after[j + end.len()..];
    }
    out
}

fn normalized(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn the_reference_is_still_the_originals_text() {
    let original = normalized(include_str!("../../render/src/simple.rs"));
    let copies = blocks(include_str!("scroll.rs"));
    assert_eq!(copies.len(), 2);
    for block in copies {
        assert!(
            original.contains(&block),
            "render's simple.rs no longer holds:\n{block}"
        );
    }
}

#[test]
fn the_simple_span_is_the_originals_f64() {
    assert_eq!(
        span().to_bits(),
        f64::from(reference::JUDGE_Y - reference::LANE_TOP).to_bits()
    );
    assert_eq!(f64::from(reference::JUDGE_Y), 880.0);
}

/// The initial rates scroll maps give: from the slowest initial tempo a
/// map accepts (1 millionth of a BPM) to the fastest (`i64::MAX`
/// millionths), both included, with tempos up to 10^6 BPM between.
fn initial_rates() -> impl Strategy<Value = i128> {
    prop_oneof![Just(1_i64), Just(i64::MAX), 1_i64..=1_000_000_000_000]
        .prop_filter_map("a valid map", |bpm| {
            ScrollMap::new(bpm, &[]).ok().map(|m| m.initial_rate())
        })
}

fn visible_times() -> impl Strategy<Value = i64> {
    prop_oneof![
        Just(i64::MIN),
        Just(-1),
        Just(0),
        Just(1),
        any::<i64>(),
        1..=100_000_000_i64,
    ]
}

fn positions() -> impl Strategy<Value = i128> {
    prop_oneof![
        Just(i128::MIN),
        Just(i128::MAX),
        any::<i128>(),
        -(1_i128 << 120)..(1_i128 << 120),
    ]
}

proptest! {
    #[test]
    fn the_copy_draws_the_originals_bits(
        at_line in positions(),
        rate in initial_rates(),
        visible in visible_times(),
        ps in prop::collection::vec(positions(), 1..8),
        below in 0.0_f32..2_000.0,
        above in 0.0_f32..2_000.0,
    ) {
        let at_line = Position(at_line);
        let original = reference::Scale::new(at_line, rate, TimeUs(visible));
        let copy = Scale {
            at_line,
            px_per_pos: original.px_per_pos,
            line_y: f64::from(reference::JUDGE_Y),
        };
        // Near the line as well as anywhere.
        let near = ps.iter().map(|p| Position(copy.at_line.0.saturating_add(p >> 64)));
        for p in ps.iter().map(|&p| Position(p)).chain(near) {
            prop_assert_eq!(copy.y(p).to_bits(), original.y(p).to_bits());
        }
        prop_assert_eq!(copy.positions(below, above), original.positions(below, above));
    }
}

/// A gain needs both terms positive and lies within 1/100 to 20.
#[test]
fn gain_refuses_zero_and_out_of_range() {
    for (num, den) in [(0, 1), (1, 0), (0, 0), (1, 101), (2_001, 100), (21, 1)] {
        assert_eq!(Gain::new(num, den), None, "{num}/{den}");
    }
}

#[test]
fn gain_accepts_both_ends() {
    for (num, den) in [(1, 100), (20, 1), (2_000, 100), (u64::MAX, u64::MAX)] {
        let g = Gain::new(num, den).unwrap();
        assert_eq!((g.num(), g.den()), (num, den));
    }
}

/// Terms near `u64::MAX` compare exactly and still give a finite,
/// positive scale.
#[test]
fn gain_with_large_terms_is_finite() {
    let g = Gain::new(u64::MAX - 1, (u64::MAX - 1) / 3).unwrap();
    let beat = (1_u128 << 80) as f64;
    for len in [1.0, 800.0, 1e6] {
        let p = g.px_per_pos(len);
        assert!(p.is_finite() && p > 0.0, "{len}: {p}");
        // H 3 over `len` px: a beat is `3 · len / 4` px.
        assert!((p * beat / (0.75 * len) - 1.0).abs() < 1e-12, "{len}");
    }
    // One unit past either end at the largest terms, and the end itself.
    let (k, h) = (u64::MAX / 20, u64::MAX / 100);
    assert_eq!(Gain::new(20 * k + 1, k), None);
    assert!(Gain::new(20 * k, k).is_some());
    assert_eq!(Gain::new(h - 1, 100 * h), None);
    assert!(Gain::new(h, 100 * h).is_some());
}
