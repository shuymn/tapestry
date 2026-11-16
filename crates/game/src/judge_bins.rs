//! The play's judgment bins (design.md "Rendering and Skins", the note
//! snapshot's judgment bins): each judged note counted once, as the logic
//! step hands its event on, in its head row's bin of the play's 1×
//! profile ([`ChartProfile::row_bins`]) and in the channel the result's
//! timelines use ([`channel`]), so the play's last frame and the result
//! agree in their per-channel totals, each over its own axis.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::{Chart, ChartProfile, LANE_COUNT, Lane, NO_BIN, NOTE_BINS, RowIdentity, TimeUs};
use eval::{JudgeEvent, JudgeEventKind, side_deviation};
use rules::Judgment;

/// The channels of a bin, as the result's timelines have them: PERFECT;
/// then GREAT, GOOD, BAD and POOR early; then the same four late.
pub const JUDGE_CHANNELS: usize = 9;

/// The copies of [`JudgeBins`] a play holds at its peak: the logic step's
/// own, the initial play view, each of the view's three buffers made from
/// it, and the view the play's end takes.
pub const JUDGE_BIN_COPIES: usize = 6;

/// The play's judgments by bin and channel, with the revision a view
/// buffer compares to know it is stale and whether the bins are of this
/// attempt's notes at all. 18 448 bytes, held boxed by the logic step and
/// by every [`PlayView`](crate::PlayView), never allocated in play.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JudgeBins {
    /// Each bin's judgments by channel, the bins of the play's profile
    /// ([`ChartProfile::bins`]) first; the rest stay 0.
    pub bins: [[u32; JUDGE_CHANNELS]; NOTE_BINS],
    /// Advances with every change: a step that counts, the mapping's
    /// installation and a rearm's clearing. Never goes back.
    pub revision: u64,
    /// Whether the attempt's notes were mapped to the profile's bins
    /// ([`crate::Logic::bin_judgments`]): without it nothing counts and
    /// a judgment graph draws nothing, which an empty view, all grey,
    /// is not.
    pub available: bool,
}

impl JudgeBins {
    /// Nothing counted, no mapping.
    pub const EMPTY: Self = Self {
        bins: [[0; JUDGE_CHANNELS]; NOTE_BINS],
        revision: 0,
        available: false,
    };

    /// [`EMPTY`](Self::EMPTY), boxed: a play view's or the logic step's
    /// copy.
    #[must_use]
    pub fn boxed() -> Box<Self> {
        Box::new(Self::EMPTY)
    }

    /// Takes `other`'s counts, revision and availability, in place.
    pub fn copy_from(&mut self, other: &Self) {
        self.bins = other.bins;
        self.revision = other.revision;
        self.available = other.available;
    }
}

/// The channel of a judgment: PERFECT 0; GREAT to POOR 1 to 4 early and 5
/// to 8 late. The side is [`eval::Tally`]'s: a long note's by whichever of
/// its head's press and its release lies farther from its time, the
/// release on a tie; early at a deviation of 0 or less; a miss late.
#[must_use]
pub fn channel(judgment: Judgment, deviation: Option<TimeUs>, release: Option<TimeUs>) -> usize {
    let grade = match judgment {
        Judgment::Perfect => return 0,
        Judgment::Great => 0,
        Judgment::Good => 1,
        Judgment::Bad => 2,
        Judgment::Poor => 3,
    };
    if side_deviation(deviation, release).is_some_and(|d| d.0 <= 0) {
        1 + grade
    } else {
        5 + grade
    }
}

/// Each note's bin, by the chart's note identities: built before READY,
/// read by every judgment in play.
#[derive(Debug, Default)]
pub(crate) struct NoteBins {
    /// Where each lane's notes start in `bins`.
    starts: [usize; LANE_COUNT],
    /// Each chart object's bin, lane after lane, mines included;
    /// [`NO_BIN`] for a note in none.
    bins: Box<[u16]>,
}

impl NoteBins {
    /// The bins of `chart`'s notes by their head rows'
    /// [`ChartProfile::row_bins`], when `chart`'s timeline has the rows
    /// `profile` is of; `None` otherwise, a preview's profile included.
    pub(crate) fn of(chart: &Chart, profile: &ChartProfile) -> Option<Self> {
        let rows = profile.row_identity()?;
        if RowIdentity::of(&chart.timeline) != rows {
            return None;
        }
        let row_bins = profile.row_bins();
        let mut starts = [0; LANE_COUNT];
        let mut at = 0_usize;
        for (s, lane) in starts.iter_mut().zip(&chart.lanes) {
            *s = at;
            at = at.saturating_add(lane.len());
        }
        let bins = chart
            .lanes
            .iter()
            .flatten()
            .map(|n| {
                row_bins
                    .get(n.anchors.head.index())
                    .copied()
                    .unwrap_or(NO_BIN)
            })
            .collect();
        Some(Self { starts, bins })
    }

    /// The heap bytes of the bins of a chart of `objects` notes, mines
    /// included.
    #[must_use]
    pub(crate) const fn bytes_for(objects: usize) -> usize {
        objects.saturating_mul(size_of::<u16>())
    }

    /// Note `note` of `lane`'s bin, if it has one.
    fn bin(&self, lane: Lane, note: usize) -> Option<usize> {
        let start = *self.starts.get(lane.index())?;
        let b = *self.bins.get(start.checked_add(note)?)?;
        (b != NO_BIN).then_some(usize::from(b))
    }

    /// Counts `ev` into `bins` when it judged a note with a bin: `true`
    /// when it did.
    pub(crate) fn count(&self, ev: &JudgeEvent, bins: &mut JudgeBins) -> bool {
        let JudgeEventKind::Judged {
            lane,
            note,
            judgment,
            deviation,
            release,
            ..
        } = ev.kind
        else {
            return false;
        };
        let Some(slot) = self
            .bin(lane, note)
            .and_then(|b| bins.bins.get_mut(b))
            .and_then(|b| b.get_mut(channel(judgment, deviation, release)))
        else {
            return false;
        };
        *slot = slot.saturating_add(1);
        true
    }
}
