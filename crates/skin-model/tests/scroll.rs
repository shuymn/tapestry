//! The scroll copy against the simple skin's original: `reference` holds
//! the original's constants and `Scale` verbatim (crates/render/src/
//! simple.rs, `layout`), a property test checks that the copy, given the
//! simple skin's span and line, produces the same bits, and a drift guard
//! fails once the original changes without the reference. Render is
//! Windows-only, so the original is read as text, not linked. The guard
//! goes when the original is deleted.

use bms_model::{DOMAIN, Position, ScrollChange, ScrollMap, TimeUs};
use proptest::prelude::*;
use skin_model::Scale;

#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
mod reference {
    use bms_model::{Position, ScrollMap, TimeUs};

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
        /// The scale at display time `at` with hi-speed `visible_time`. A
        /// visible time below 1 µs counts as 1 µs.
        ///
        /// The lane from [`LANE_TOP`] to [`JUDGE_Y`] holds the scroll of
        /// `visible_time` at the chart's initial rate. The pixels per
        /// position never change during a play, whatever the BPM at `at`.
        #[must_use]
        pub fn new(scroll: &ScrollMap, at: TimeUs, visible_time: TimeUs) -> Self {
            let visible = visible_time.0.max(1) as f64;
            let span = visible * scroll.initial_rate() as f64;
            Self {
                at_line: scroll.position_at(at),
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

/// Scroll maps of up to 6 BPM changes and STOPs anywhere in the domain.
fn scroll_maps() -> impl Strategy<Value = ScrollMap> {
    let change = (any::<bool>(), 0..DOMAIN, 1_i64..=1_000_000_000_000).prop_map(|(stop, at, v)| {
        if stop {
            ScrollChange::Stop {
                at: TimeUs(at),
                len: TimeUs(v % 10_000_000_000 + 1),
            }
        } else {
            ScrollChange::Bpm {
                at: TimeUs(at),
                bpm_micro: v,
            }
        }
    });
    (
        1_i64..=1_000_000_000_000,
        prop::collection::vec(change, 0..6),
    )
        .prop_filter_map("a valid map", |(bpm, changes)| {
            ScrollMap::new(bpm, &changes).ok()
        })
}

fn times() -> impl Strategy<Value = i64> {
    prop_oneof![
        Just(i64::MIN),
        Just(i64::MAX),
        Just(-DOMAIN),
        Just(DOMAIN),
        Just(0),
        any::<i64>(),
        -DOMAIN..=DOMAIN,
    ]
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
        map in scroll_maps(),
        at in times(),
        visible in visible_times(),
        ps in prop::collection::vec(positions(), 1..8),
        below in 0.0_f32..2_000.0,
        above in 0.0_f32..2_000.0,
    ) {
        let original = reference::Scale::new(&map, TimeUs(at), TimeUs(visible));
        let copy = Scale::new(&map, TimeUs(at), TimeUs(visible), span(), 880.0);
        prop_assert_eq!(copy.at_line, original.at_line);
        prop_assert_eq!(copy.px_per_pos.to_bits(), original.px_per_pos.to_bits());
        // Near the line as well as anywhere.
        let near = ps.iter().map(|p| Position(copy.at_line.0.saturating_add(p >> 64)));
        for p in ps.iter().map(|&p| Position(p)).chain(near) {
            prop_assert_eq!(copy.y(p).to_bits(), original.y(p).to_bits());
        }
        prop_assert_eq!(copy.positions(below, above), original.positions(below, above));
    }
}
