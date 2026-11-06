//! What a play has decided about each note so far, for drawing: the note
//! facts the evaluator keeps beside its judgments (design.md "Rendering
//! and Skins", the note snapshot), and each lane's input as conversion
//! left it.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::{Chart, LANE_COUNT, Lane, TimeUs};

/// A lane's visual long-note hold: a press took the long note's head
/// before its end (held, or decided BAD at the press), and neither a
/// release nor its end has ended it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoteHold {
    /// The long note's index in its lane of [`Chart::lanes`].
    pub note: usize,
    /// The evaluation time of the press that started it.
    pub start: TimeUs,
    /// The long note's end.
    pub end: TimeUs,
}

/// Every note's state as the evaluator decided it so far, by the chart's
/// own note identities: one bit per note of each lane of
/// [`Chart::lanes`], set for a tap that is judged, a long note whose head
/// is taken (held or judged) and a mine that was hit; and each lane's
/// visual long-note hold. It depends only on the chart, the input and the
/// preset, as the judgments do.
///
/// Its storage is sized for the chart once: copying it into another of
/// the same chart ([`copy_from`](Self::copy_from)) allocates nothing.
/// Every change advances its [`revision`](Self::revision), which a
/// [`reset`](Self::reset) advances too and never takes back, so a copy
/// whose revision matches holds the same facts.
///
/// The default is the facts of a chart without notes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NoteFacts {
    /// Per lane, its first word in `words`.
    starts: [usize; LANE_COUNT],
    /// Per lane, its notes.
    lens: [usize; LANE_COUNT],
    /// The bits, lane after lane, each lane's in whole words.
    words: Vec<u64>,
    holds: [Option<NoteHold>; LANE_COUNT],
    revision: u64,
}

impl NoteFacts {
    /// No note decided and no hold, sized for `chart`'s lanes.
    #[must_use]
    pub fn new(chart: &Chart) -> Self {
        let mut starts = [0; LANE_COUNT];
        let mut lens = [0; LANE_COUNT];
        let mut at = 0;
        for ((start, len), lane) in starts
            .iter_mut()
            .zip(&mut lens)
            .zip(chart.lanes.iter().map(Vec::len))
        {
            *start = at;
            *len = lane;
            at += lane.div_ceil(64);
        }
        Self {
            starts,
            lens,
            words: vec![0; at],
            holds: [None; LANE_COUNT],
            revision: 0,
        }
    }

    /// The heap bytes facts of `chart` hold: one word per 64 notes of
    /// each lane, rounded up per lane.
    #[must_use]
    pub fn heap_bytes_for(chart: &Chart) -> usize {
        let words: usize = chart.lanes.iter().map(|l| l.len().div_ceil(64)).sum();
        words.saturating_mul(size_of::<u64>())
    }

    /// The heap bytes these facts hold.
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        self.words.capacity().saturating_mul(size_of::<u64>())
    }

    /// Whether note `note` of `lane` is decided: a tap judged, a long
    /// note's head taken, a mine hit. A note past the lane is not.
    #[must_use]
    pub fn is_done(&self, lane: Lane, note: usize) -> bool {
        self.word(lane.index(), note)
            .is_some_and(|w| w & (1 << (note % 64)) != 0)
    }

    /// `lane`'s visual long-note hold.
    #[must_use]
    pub fn hold(&self, lane: Lane) -> Option<NoteHold> {
        self.holds.get(lane.index()).copied().flatten()
    }

    /// The revision of these facts: advanced by every change.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Becomes a copy of `other`, revision included. Allocates nothing
    /// when `other` is of the same chart.
    pub fn copy_from(&mut self, other: &Self) {
        self.starts = other.starts;
        self.lens = other.lens;
        self.words.clear();
        self.words.extend_from_slice(&other.words);
        self.holds = other.holds;
        self.revision = other.revision;
    }

    /// Marks note `note` of `lane` decided; a note past the lane is
    /// ignored.
    pub fn set_done(&mut self, lane: Lane, note: usize) {
        let l = lane.index();
        let Some(i) = self.index(l, note) else {
            return;
        };
        if let Some(w) = self.words.get_mut(i) {
            *w |= 1 << (note % 64);
            self.revision += 1;
        }
    }

    /// Sets `lane`'s visual hold, `None` ending it.
    pub fn set_hold(&mut self, lane: Lane, hold: Option<NoteHold>) {
        if let Some(slot) = self.holds.get_mut(lane.index())
            && *slot != hold
        {
            *slot = hold;
            self.revision += 1;
        }
    }

    /// No note decided and no hold, in place: the revision advances.
    pub fn reset(&mut self) {
        self.words.fill(0);
        self.holds = [None; LANE_COUNT];
        self.revision += 1;
    }

    /// The word of `note` of lane `l`.
    fn word(&self, l: usize, note: usize) -> Option<u64> {
        self.words.get(self.index(l, note)?).copied()
    }

    /// The index in `words` of `note` of lane `l`, if the lane has it.
    fn index(&self, l: usize, note: usize) -> Option<usize> {
        if note >= *self.lens.get(l)? {
            return None;
        }
        self.starts.get(l)?.checked_add(note / 64)
    }
}

/// One lane's input as conversion left it: whether a control holds it
/// now, and the evaluation times of its latest press and release edges
/// (receive time less the source's input offset), with how many press
/// edges came at the latest press's µs. A lane held at the play's opening
/// is held with no press: the opening has no edge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LaneInput {
    /// Whether some control holds the lane.
    pub held: bool,
    /// The latest press edge.
    pub pressed: Option<TimeUs>,
    /// The press edges at `pressed`'s µs so far, from 1, saturating at
    /// `u32::MAX`; 0 with no press. An event of this lane carries the
    /// count at its own µs when it came ([`JudgeEventKind::Judged`]'s
    /// `presses`), so a press after it in the same µs reads greater.
    ///
    /// [`JudgeEventKind::Judged`]: crate::JudgeEventKind::Judged
    pub presses: u32,
    /// The latest release edge.
    pub released: Option<TimeUs>,
}

impl LaneInput {
    /// The press edges at `at` so far: [`presses`](Self::presses) when the
    /// latest press is at `at`, else 0.
    #[must_use]
    pub fn presses_at(&self, at: TimeUs) -> u32 {
        if self.pressed == Some(at) {
            self.presses
        } else {
            0
        }
    }
}

/// Every lane's input ([`LaneInput`]) and how far conversion took it:
/// filled from each edge the conversion returns, judged or not, scratch
/// release deadlines included. A reversal's release and press come at one
/// time, so `held` tells which came last. It depends only on the input
/// and the device profile.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputFacts {
    lanes: [LaneInput; LANE_COUNT],
    /// The evaluation time conversion has processed input through; `None`
    /// before the first advance.
    converted: Option<TimeUs>,
}

impl InputFacts {
    /// No lane held and no edge: the facts of a play without input.
    pub const NONE: Self = Self {
        lanes: [LaneInput {
            held: false,
            pressed: None,
            presses: 0,
            released: None,
        }; LANE_COUNT],
        converted: None,
    };

    /// `lane`'s input.
    #[must_use]
    pub fn lane(&self, lane: Lane) -> LaneInput {
        self.lanes.get(lane.index()).copied().unwrap_or_default()
    }

    /// The evaluation time conversion has processed input through.
    #[must_use]
    pub const fn converted(&self) -> Option<TimeUs> {
        self.converted
    }

    /// The opening's state, in place: each lane held as `held` says, with
    /// no edge and nothing converted.
    pub(crate) fn open(&mut self, held: impl Fn(Lane) -> bool) {
        for (l, slot) in Lane::all().zip(&mut self.lanes) {
            *slot = LaneInput {
                held: held(l),
                pressed: None,
                presses: 0,
                released: None,
            };
        }
        self.converted = None;
    }

    /// A press edge of `lane` at `at`: counted after the earlier ones at
    /// that µs, or the first.
    pub fn press(&mut self, lane: Lane, at: TimeUs) {
        if let Some(slot) = self.lanes.get_mut(lane.index()) {
            slot.held = true;
            slot.presses = if slot.pressed == Some(at) {
                slot.presses.saturating_add(1)
            } else {
                1
            };
            slot.pressed = Some(at);
        }
    }

    /// A release edge of `lane` at `at`.
    pub fn release(&mut self, lane: Lane, at: TimeUs) {
        if let Some(slot) = self.lanes.get_mut(lane.index()) {
            slot.held = false;
            slot.released = Some(at);
        }
    }

    /// Conversion has processed input through `at`.
    pub const fn convert_to(&mut self, at: TimeUs) {
        self.converted = Some(at);
    }
}

#[cfg(test)]
mod tests {
    use bms_model::{ChartBuilder, Lane, TimeUs};

    use super::{NoteFacts, NoteHold};

    fn lane(n: u8) -> Lane {
        Lane::new(n).unwrap()
    }

    /// Bits by the chart's note identities, each lane its own words; the
    /// revision advances by each change and never back; a copy of the
    /// same chart allocates nothing and equals its source.
    #[test]
    fn bits_holds_and_revisions() {
        let mut b = ChartBuilder::new(150_000);
        for i in 0..130 {
            b = b.tap(lane(1), TimeUs(1_000 * i));
        }
        let chart = b
            .long(lane(2), TimeUs(5_000), TimeUs(9_000))
            .build()
            .unwrap();
        let mut f = NoteFacts::new(&chart);
        assert_eq!(f.heap_bytes(), NoteFacts::heap_bytes_for(&chart));
        assert_eq!(NoteFacts::heap_bytes_for(&chart), 4 * 8);
        f.set_done(lane(1), 129);
        f.set_done(lane(1), 130);
        f.set_done(lane(3), 0);
        assert!(f.is_done(lane(1), 129));
        assert!(!f.is_done(lane(1), 128) && !f.is_done(lane(2), 0));
        assert!(!f.is_done(lane(1), 130) && !f.is_done(lane(3), 0));
        assert_eq!(f.revision(), 1);
        let hold = NoteHold {
            note: 0,
            start: TimeUs(5_100),
            end: TimeUs(9_000),
        };
        f.set_hold(lane(2), Some(hold));
        f.set_hold(lane(2), Some(hold));
        assert_eq!((f.hold(lane(2)), f.revision()), (Some(hold), 2));
        let mut copy = NoteFacts::new(&chart);
        let words = copy.words.as_ptr();
        copy.copy_from(&f);
        assert_eq!(copy, f);
        assert_eq!(copy.words.as_ptr(), words);
        f.reset();
        assert_eq!((f.hold(lane(2)), f.revision()), (None, 3));
        assert!(!f.is_done(lane(1), 129));
    }
}
