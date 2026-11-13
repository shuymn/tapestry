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
    /// The selection installation's chart layer, by its live key (`None`
    /// with no selection installation or on a lost device). Its key's
    /// generation is the cursor generation, not the play one.
    Selection(Option<ChartKey>),
}

/// What a published table and its acknowledgment are for: the images'
/// generation, the attempt, and the target the table went into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublicationKey {
    /// The generation of the images' resolution
    /// ([`crate::chart_images::ChartImageWorker::renew`]): a new one at
    /// each choice, practice Start, retry's re-resolution and chart left.
    /// For a [`PublishedTo::Selection`] key, the cursor generation.
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
    /// The cursor generation the game thread set last: a new one at each
    /// change of the selection's image request.
    pub cursor_generation: u64,
    /// The selection installation's live chart key.
    pub selection: Option<ChartKey>,
}

impl PublicationKey {
    /// Whether this publication is current at `now` for the attempt
    /// `request` the caller expects: the same generation, that attempt,
    /// and its target still the live one. A selection key is compared with
    /// the cursor generation instead of the play one, and is never an
    /// attempt's. The one test of currency, asked by:
    /// - the game thread's [`crate::chart_images::ChartImageWorker::publish`],
    ///   before taking an acknowledgment (the barrier before readiness and
    ///   the loading frames' publication) and before keeping one it asked
    ///   for, and its `Core::publish_selection_now`, before keeping a
    ///   selection publication it asked for;
    /// - the render thread's `Run::publish_chart` and
    ///   `Run::publish_selection_chart`, before admitting an incoming
    ///   publication into its one target;
    /// - the render thread's `Run::commit`, before re-keying the staged
    ///   table into the committed installation;
    /// - the render thread's `Run::sync_chart`, before every frame and
    ///   publication, for the committed table (expecting `None`), the
    ///   staged one (expecting the live staging's request) and the
    ///   selection's (expecting `None`).
    #[must_use]
    pub fn current(&self, now: &ChartNow, request: Option<u64>) -> bool {
        let generation = match self.target {
            PublishedTo::Selection(_) => now.cursor_generation,
            PublishedTo::Staged(_) | PublishedTo::Committed(_) => now.generation,
        };
        self.generation == generation
            && self.request == request
            && match self.target {
                PublishedTo::Staged(r) => request == Some(r) && now.staged == Some(r),
                PublishedTo::Committed(k) => request.is_none() && now.committed == k,
                PublishedTo::Selection(k) => request.is_none() && now.selection == k,
            }
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
            ..ChartNow::default()
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

    /// A selection key is current for its cursor generation and the live
    /// selection key, whatever the play generation; never for an attempt,
    /// and a missing key is not a missing installation's.
    #[test]
    fn a_selection_key_is_current_for_its_cursor_generation_and_key() {
        let now = ChartNow {
            generation: 5,
            staged: None,
            committed: None,
            cursor_generation: 11,
            selection: Some(K),
        };
        let key = PublicationKey {
            generation: 11,
            request: None,
            target: PublishedTo::Selection(Some(K)),
        };
        assert!(key.current(&now, None));
        assert!(
            key.current(
                &ChartNow {
                    generation: 6,
                    ..now
                },
                None
            ),
            "the play generation is not compared"
        );
        assert!(
            !key.current(
                &ChartNow {
                    cursor_generation: 12,
                    ..now
                },
                None
            ),
            "another cursor generation"
        );
        let other = ChartKey {
            installation: 7,
            ..K
        };
        assert!(
            !key.current(
                &ChartNow {
                    selection: Some(other),
                    ..now
                },
                None
            ),
            "another selection key"
        );
        assert!(
            !key.current(
                &ChartNow {
                    selection: None,
                    ..now
                },
                None
            ),
            "the installation gone"
        );
        let unkeyed = PublicationKey {
            target: PublishedTo::Selection(None),
            ..key
        };
        assert!(
            unkeyed.current(
                &ChartNow {
                    selection: None,
                    ..now
                },
                None
            ),
            "a None key is current only while there is no key"
        );
        assert!(
            !unkeyed.current(&now, None),
            "a None key against a live one"
        );
        assert!(!key.current(&now, Some(11)), "a request given");
        let attempt = PublicationKey {
            request: Some(3),
            ..key
        };
        assert!(
            !attempt.current(&now, Some(3)),
            "a selection key with an attempt"
        );
        // A committed key with the cursor generation is not a selection one.
        let committed = PublicationKey {
            target: PublishedTo::Committed(Some(K)),
            ..key
        };
        assert!(!committed.current(
            &ChartNow {
                committed: Some(K),
                ..now
            },
            None
        ));
    }
}
