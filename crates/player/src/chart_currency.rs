//! What makes a published chart-image table, and its acknowledgment,
//! current (design.md "Rendering and Skins", Chart images): one test,
//! [`PublicationKey::current`], which every path that publishes, keeps,
//! re-keys or takes the answer of a table asks. The render thread asks it
//! per frame, so this module is in-play code (`xtask/src/lints.rs`
//! `IN_PLAY`): no allocation, no logging.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use skin_model::chart_image::ChartKey;

/// Where a publication went, as the render thread answers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishedTo {
    /// The staged array of the attempt whose staging has this request.
    Staged(u64),
    /// The committed play installation's chart layer, by its live key
    /// (`None` with no installation or on a lost device).
    Committed(Option<ChartKey>),
}

/// What a published table and its acknowledgment are for: the images'
/// generation, the attempt, and the target the table went into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublicationKey {
    /// The generation of the images' resolution
    /// ([`crate::chart_images::ChartImageWorker::renew`]): a new one at
    /// each choice, practice Start, retry's re-resolution and chart left.
    pub generation: u64,
    /// The attempt: the request of the staging the table is for, `None`
    /// for the committed installation's table, which is the current
    /// generation's whatever attempt draws it.
    pub request: Option<u64>,
    /// Where the table went.
    pub target: PublishedTo,
}

/// What a key is checked against: the current generation and the live
/// targets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChartNow {
    /// The generation the game thread set last.
    pub generation: u64,
    /// The request of the staging the render thread holds, if any.
    pub staged: Option<u64>,
    /// The committed play installation's live chart key.
    pub committed: Option<ChartKey>,
}

impl PublicationKey {
    /// Whether this publication is current at `now` for the attempt
    /// `request` the caller expects: the same generation, that attempt,
    /// and its target still the live one. The one test of currency, asked
    /// by:
    /// - the game thread's [`crate::chart_images::ChartImageWorker::publish`],
    ///   before taking an acknowledgment (the barrier before readiness and
    ///   the loading frames' publication) and before keeping one it asked
    ///   for;
    /// - the render thread's `Run::publish_chart`, before admitting an
    ///   incoming publication into its one target;
    /// - the render thread's `Run::commit`, before re-keying the staged
    ///   table into the committed installation;
    /// - the render thread's `Run::sync_chart`, before every frame and
    ///   publication, for the committed table (expecting `None`) and the
    ///   staged one (expecting the live staging's request).
    #[must_use]
    pub fn current(&self, now: &ChartNow, request: Option<u64>) -> bool {
        self.generation == now.generation
            && self.request == request
            && match self.target {
                PublishedTo::Staged(r) => request == Some(r) && now.staged == Some(r),
                PublishedTo::Committed(k) => request.is_none() && now.committed == k,
            }
    }
}

/// The cursor row's chart images as a selection update carries them
/// (design.md "Rendering and Skins", chart images): resident in a slot of
/// a pool generation, absent (the row has no image to draw), or pending
/// (no entry yet). A frame draws a resident value only while its
/// generation is the live pool's ([`slot_in`](Self::slot_in)); any other
/// value draws the no-image branch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorChart {
    /// The row's images are in slot `slot` of pool generation
    /// `generation`.
    Resident { generation: u64, slot: u16 },
    /// The row has no image to draw: a row that is no chart, a chart
    /// naming none, or one whose images were read absent.
    Absent,
    /// The row has no entry yet.
    #[default]
    Pending,
}

impl CursorChart {
    /// The slot a frame draws when `live` is the live pool generation (0
    /// for none): a resident value's own while its generation is `live`;
    /// `None` otherwise, which draws as pending. Allocates nothing.
    #[must_use]
    pub const fn slot_in(self, live: u64) -> Option<u16> {
        match self {
            Self::Resident { generation, slot } if generation == live && live != 0 => Some(slot),
            Self::Resident { .. } | Self::Absent | Self::Pending => None,
        }
    }

    /// Slot `slot` of pool generation `generation` packed in one `u64`, as
    /// the render thread publishes the slot it displays: the generation
    /// above 16 bits and the slot plus 1 below, 0 for none. Generations
    /// come from a process-wide counter that never reaches 2^48, so no bit
    /// of one is dropped. Allocates nothing.
    #[must_use]
    pub const fn pack(generation: u64, slot: u16) -> u64 {
        (generation << 16) | (slot as u64 + 1)
    }

    /// What [`pack`](Self::pack) packed: the generation and the slot, or
    /// `None` for 0.
    #[must_use]
    pub fn unpack(packed: u64) -> Option<(u64, u16)> {
        let slot = u16::try_from((packed & 0xFFFF).checked_sub(1)?).ok()?;
        Some((packed >> 16, slot))
    }
}

#[cfg(test)]
mod tests {
    use skin_model::chart_image::ChartKey;

    use super::{ChartNow, PublicationKey, PublishedTo};

    const K: ChartKey = ChartKey {
        renderer: 1,
        installation: 2,
        atlas: 3,
        generation: 4,
    };

    /// A key is current only for its generation, the attempt expected and
    /// its live target; a staged key is never the committed table's and a
    /// committed key never a staging's.
    #[test]
    fn a_key_is_current_for_its_generation_attempt_and_target() {
        let now = ChartNow {
            generation: 5,
            staged: Some(9),
            committed: Some(K),
        };
        let committed = PublicationKey {
            generation: 5,
            request: None,
            target: PublishedTo::Committed(Some(K)),
        };
        let staged = PublicationKey {
            generation: 5,
            request: Some(9),
            target: PublishedTo::Staged(9),
        };
        assert!(committed.current(&now, None));
        assert!(staged.current(&now, Some(9)));
        assert!(
            !committed.current(&now, Some(9)),
            "the committed table for a staging"
        );
        assert!(
            !staged.current(&now, None),
            "a staged table as the committed one"
        );
        assert!(!committed.current(
            &ChartNow {
                generation: 6,
                ..now
            },
            None
        ));
        assert!(!staged.current(
            &ChartNow {
                staged: Some(8),
                ..now
            },
            Some(9)
        ));
        let other = ChartKey {
            installation: 7,
            ..K
        };
        assert!(!committed.current(
            &ChartNow {
                committed: Some(other),
                ..now
            },
            None
        ));
        // A key whose request and staged target disagree is never current.
        let torn = PublicationKey {
            request: Some(9),
            target: PublishedTo::Staged(8),
            ..staged
        };
        assert!(!torn.current(
            &ChartNow {
                staged: Some(8),
                ..now
            },
            Some(9)
        ));
    }

    /// A resident value draws its slot only in its own live generation,
    /// never with no pool; a packed slot unpacks to itself, and 0 to none.
    #[test]
    fn a_resident_value_draws_only_in_its_generation() {
        use super::CursorChart;
        let r = CursorChart::Resident {
            generation: 7,
            slot: 3,
        };
        assert_eq!(r.slot_in(7), Some(3));
        assert_eq!(r.slot_in(8), None);
        assert_eq!(r.slot_in(0), None);
        assert_eq!(CursorChart::Absent.slot_in(7), None);
        assert_eq!(CursorChart::Pending.slot_in(7), None);
        assert_eq!(CursorChart::unpack(CursorChart::pack(7, 3)), Some((7, 3)));
        assert_eq!(CursorChart::unpack(CursorChart::pack(1, 47)), Some((1, 47)));
        assert_eq!(CursorChart::unpack(0), None);
    }
}
