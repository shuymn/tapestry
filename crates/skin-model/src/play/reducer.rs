//! The reducer: edge timers sourced from the drained effects, in emission
//! order (stage3.md R3).
//!
//! It reads only each event's kind and chart time, never the frame, so its
//! state after a sequence of effects does not depend on how drains split
//! the sequence. It keeps animations only: each note's state and the
//! visual holds come from the play view's note facts, so a dropped or late
//! effect never changes them. Feedback follows the play screen's rules: a
//! press, an empty POOR included, gives a bomb when its grade is within
//! the skin's bomb threshold, and a long note's completion, recognized
//! from its own judgment and the chart, gives its judgment text but never
//! a bomb, even when its head's `LnHeld` was dropped.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
#![deny(clippy::float_arithmetic, clippy::float_cmp)]

use bms_model::{Chart, LANE_COUNT, NoteKind, TimeUs};
use eval::{JudgeEvent, JudgeEventKind, Section};
use rules::{GaugeValue, Judgment, LnMode};

use super::schema::{Grade, READOUT_MAX};

/// A long note's head and end times.
#[derive(Clone, Copy, Debug)]
struct Long {
    head: TimeUs,
    end: TimeUs,
}

/// A note's time, and its end for a long note.
#[derive(Clone, Copy, Debug)]
struct Times {
    time: TimeUs,
    end: Option<TimeUs>,
}

/// The recent timing (stage3.md R3): the latest judgment the source's
/// timing readout takes, its timing in the source's sign (note time
/// minus press, so early is positive) in whole ms truncated toward 0
/// (`play/JudgeManager.java:457`, `:690`), and whether it is worse than
/// PERFECT. 0 and PERFECT before any (`:130-133`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Recent {
    pub(crate) ms: i32,
    pub(crate) imperfect: bool,
}

impl Recent {
    /// The readout of a judgment whose timing, in the source's sign, is
    /// `source_us` µs, worse than PERFECT when `imperfect`: truncated
    /// toward 0, then bounded to what a number shows.
    fn of(source_us: i64, imperfect: bool) -> Self {
        let ms = (source_us / 1_000).clamp(-i64::from(READOUT_MAX), i64::from(READOUT_MAX));
        Self {
            ms: i32::try_from(ms).unwrap_or(0),
            imperfect,
        }
    }
}

/// A lane's visual state from the effects (stage3.md R3), the source's
/// per-lane judge value (`play/JudgeManager.java:805-811`): its code, the
/// time of the event that set it, `None` before any, and whether that
/// event was a press. A later press clears it to 0 unless an effect at
/// that press sets it again ([`LaneJudge::read`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LaneJudge {
    code: u8,
    at: Option<TimeUs>,
    by_press: bool,
}

impl LaneJudge {
    /// A normal long note held (`:266`, `:438`).
    const HELD: u8 = 8;

    /// The code of a judgment other than a miss whose timing, in the
    /// source's sign (note time minus press), is `source_us`:
    /// `j == 0 ? 1 : 2j + (late ? 1 : 0)` for the source's index `j`, late
    /// meaning a timing of 0 or less, so on time is late (`:671-672`). An
    /// empty POOR is index 5. `None` for a miss, index 4, which leaves the
    /// code (`:671`).
    fn code(grade: Grade, source_us: i64) -> Option<u8> {
        let j = match grade {
            Grade::Perfect => return Some(1),
            Grade::Great => 1,
            Grade::Good => 2,
            Grade::Bad => 3,
            Grade::Poor => return None,
            Grade::EmptyPoor => 5,
        };
        Some(2 * j + u8::from(source_us <= 0))
    }

    /// What lane input `input` reads with this state: 0 when a press
    /// came after the event that set it, or with it when that event was
    /// not a press (a reversal's release, then its press), since the
    /// source clears a lane on a press matching no note (`:460-462`) and
    /// a press that matches one is set by its own effect at its time.
    pub(crate) fn read(self, input: eval::LaneInput) -> i32 {
        match (self.at, input.pressed) {
            (None, _) => 0,
            (Some(at), Some(p)) if p > at || (p == at && !self.by_press) => 0,
            (Some(_), _) => i32::from(self.code),
        }
    }
}

/// The effect-sourced state of one attempt.
#[derive(Clone, Debug)]
pub(crate) struct Reducer {
    /// Per lane and note: its time, and its end for a long note.
    notes: [Box<[Times]>; LANE_COUNT],
    /// The long-note judgment mode the notes are drawn by: LN, the only
    /// mode judged (design.md "LN Judgment Modes").
    pub(crate) ln_mode: LnMode,
    pub(crate) bomb: [Option<TimeUs>; LANE_COUNT],
    pub(crate) mine: [Option<TimeUs>; LANE_COUNT],
    pub(crate) judge: Option<(Grade, TimeUs)>,
    pub(crate) recent: Recent,
    pub(crate) lanes: [LaneJudge; LANE_COUNT],
    pub(crate) combo_up: Option<TimeUs>,
    targets: u32,
    pub(crate) full_combo: Option<TimeUs>,
    /// Per gauge: when it reached its maximum, if it is there by the
    /// effects delivered so far.
    pub(crate) gauge_max: Box<[Option<TimeUs>]>,
    /// Per gauge: `gauge_max` as the attempt starts, for a rearm.
    gauge_start: Box<[Option<TimeUs>]>,
    /// Per gauge: its maximum.
    pub(crate) gauge_top: Box<[GaugeValue]>,
    empty_poor_breaks_combo: bool,
    /// The last grade that gives a bomb, `None` for none.
    bomb_up_to: Option<Grade>,
}

/// Whether a long note's body is drawn active under `mode`, given whether
/// it is held: in LN mode exactly while held.
pub(crate) const fn body_active(mode: LnMode, held: bool) -> bool {
    match mode {
        LnMode::Ln => held,
    }
}

/// Whether a note from `time` to `end` is a target of the section from
/// `start` to `end` (every note without one), as the evaluator decides it.
pub(crate) fn is_target(section: Option<(TimeUs, TimeUs)>, time: TimeUs, end: TimeUs) -> bool {
    section.is_none_or(|(start, until)| start <= time && time < until && end < until)
}

impl Reducer {
    /// The bytes its buffers hold: every lane's note times, and each
    /// gauge's maximum and when it reached it.
    pub(crate) fn heap_bytes(&self) -> usize {
        let notes: usize = self.notes.iter().map(|l| size_of_val(&**l)).sum();
        notes
            + size_of_val(&*self.gauge_max)
            + size_of_val(&*self.gauge_start)
            + size_of_val(&*self.gauge_top)
    }

    /// A clean reducer for one attempt: `initial` gives each gauge's
    /// initial value and maximum, and a gauge whose initial value is at its
    /// maximum reads on from `play_at`, the attempt's start after
    /// READY.
    pub(crate) fn new(
        chart: &Chart,
        section: Option<&Section>,
        initial: impl Iterator<Item = (GaugeValue, GaugeValue)>,
        play_at: TimeUs,
        empty_poor_breaks_combo: bool,
        bomb_up_to: Option<Grade>,
    ) -> Self {
        let mut targets: u32 = 0;
        for note in chart.lanes.iter().flatten() {
            if matches!(note.kind, NoteKind::Tap | NoteKind::Long { .. })
                && is_target(section.map(|s| (s.start, s.end)), note.time, note.end())
            {
                targets = targets.saturating_add(1);
            }
        }
        let notes = std::array::from_fn(|l| {
            chart.lanes.get(l).map_or_else(Box::default, |notes| {
                notes
                    .iter()
                    .map(|n| Times {
                        time: n.time,
                        end: match n.kind {
                            NoteKind::Long { end, .. } => Some(end),
                            NoteKind::Tap | NoteKind::Mine { .. } => None,
                        },
                    })
                    .collect()
            })
        });
        let (gauge_max, gauge_top): (Vec<_>, Vec<_>) = initial
            .map(|(value, top)| ((value >= top).then_some(play_at), top))
            .unzip();
        Self {
            notes,
            ln_mode: LnMode::Ln,
            bomb: [None; LANE_COUNT],
            mine: [None; LANE_COUNT],
            judge: None,
            recent: Recent::default(),
            lanes: [LaneJudge::default(); LANE_COUNT],
            combo_up: None,
            targets,
            full_combo: None,
            gauge_start: gauge_max.clone().into_boxed_slice(),
            gauge_max: gauge_max.into_boxed_slice(),
            gauge_top: gauge_top.into_boxed_slice(),
            empty_poor_breaks_combo,
            bomb_up_to,
        }
    }

    /// Back to the clean reducer [`new`](Self::new) made, in place, for a
    /// rearmed READY candidate (design.md "Play Lifecycle", the start
    /// gate): no lane bombed or hit by a mine, nothing judged, the recent
    /// timing and every lane's visual state 0, and each gauge's maximum as
    /// the attempt started. Allocates nothing.
    pub(crate) fn rearm(&mut self) {
        self.bomb = [None; LANE_COUNT];
        self.mine = [None; LANE_COUNT];
        self.judge = None;
        self.recent = Recent::default();
        self.lanes = [LaneJudge::default(); LANE_COUNT];
        self.combo_up = None;
        self.full_combo = None;
        self.gauge_max.copy_from_slice(&self.gauge_start);
    }

    /// The attempt's target count.
    pub(crate) fn targets(&self) -> u32 {
        self.targets
    }

    /// Applies one effect. Lanes come from [`bms_model::Lane`], so every
    /// lane index is below [`LANE_COUNT`]; notes and gauges out of range
    /// are ignored.
    pub(crate) fn apply(&mut self, ev: &JudgeEvent) {
        let t = ev.time;
        match ev.kind {
            JudgeEventKind::Judged {
                lane,
                note,
                judgment,
                deviation,
                release,
                combo,
            } => {
                let l = lane.index();
                self.judge = Some((Grade::of(judgment), t));
                if let Some(us) = self.source_timing(l, note, judgment, deviation, release, t) {
                    self.recent = Recent::of(us, judgment != Judgment::Perfect);
                    // A release or an end sets it at its own time; a
                    // judgment decided at a press, at the press's.
                    let by_press = ev.origin.is_some() && release.is_none();
                    self.set_lane(l, LaneJudge::code(Grade::of(judgment), us), t, by_press);
                }
                self.combo_up = (combo > 0).then_some(t);
                if self.targets > 0 && combo == self.targets && self.full_combo.is_none() {
                    self.full_combo = Some(t);
                }
                let long = self.long(l, note);
                let Some(Long { head, end }) = long else {
                    self.feedback(l, Grade::of(judgment), t);
                    return;
                };
                // A completion: finished by a release, or at its end after
                // a head press before the end. Its press already gave the
                // feedback, even if that press's `LnHeld` was dropped.
                let completion = release.is_some()
                    || (t == end
                        && deviation.is_some_and(|d| {
                            i128::from(head.0) + i128::from(d.0) < i128::from(end.0)
                        }));
                if completion {
                    return;
                }
                self.feedback(l, Grade::of(judgment), t);
            }
            JudgeEventKind::LnHeld { lane, judgment, .. } => {
                // The source's held head leaves the recent timing alone
                // (`play/JudgeManager.java:428-438`); its long note's
                // judgment sets it. Its lane shows the normal long note in
                // progress (`:438`).
                let l = lane.index();
                self.set_lane(l, Some(LaneJudge::HELD), t, true);
                self.judge = Some((Grade::of(judgment), t));
                self.feedback(l, Grade::of(judgment), t);
            }
            JudgeEventKind::MineHit { lane, .. } => {
                if let Some(mine) = self.mine.get_mut(lane.index()) {
                    *mine = Some(t);
                }
            }
            JudgeEventKind::EmptyPoor { lane, deviation } => {
                self.judge = Some((Grade::EmptyPoor, t));
                // The source's empty POOR, its judgment 5
                // (`play/JudgeManager.java:400-404`), takes the timing of
                // the note it is against, as a judged press does (`:423`
                // and `:440` for a long note's head, `:457-458` for a
                // tap).
                let us = deviation.0.saturating_neg();
                self.recent = Recent::of(us, true);
                self.set_lane(lane.index(), LaneJudge::code(Grade::EmptyPoor, us), t, true);
                if self.empty_poor_breaks_combo {
                    self.combo_up = None;
                }
                self.feedback(lane.index(), Grade::EmptyPoor, t);
            }
            JudgeEventKind::Gauge { gauge, value, .. } => {
                if let (Some(at), Some(&top)) =
                    (self.gauge_max.get_mut(gauge), self.gauge_top.get(gauge))
                {
                    if value < top {
                        *at = None;
                    } else if at.is_none() {
                        *at = Some(t);
                    }
                }
            }
            JudgeEventKind::LnHoldEnded { .. }
            | JudgeEventKind::SongEnd
            | JudgeEventKind::PlaySound { .. } => {}
        }
    }

    /// Note `note` on lane `l` if it is a long note.
    fn long(&self, l: usize, note: usize) -> Option<Long> {
        let n = self.notes.get(l)?.get(note)?;
        n.end.map(|end| Long { head: n.time, end })
    }

    /// The timing, in µs and the source's sign (note time minus press),
    /// the source gives the judgment of note `note` on lane `l` decided at
    /// `t`; `None` for a note out of range.
    ///
    /// - A press: its deviation, negated (`play/JudgeManager.java:457`).
    /// - A miss: the note's time minus `t`, where the source takes the
    ///   frame that passed its late BAD edge (`:594-598`); here the
    ///   evaluator's deadline, or the press that passed the note.
    /// - A long note finished by a release: of its head's and its
    ///   release's, the one farther from its time, the release on a tie
    ///   (`:533-537`). An early one that leaves the judgment BAD or worse
    ///   is taken when the release margin has passed, from the release
    ///   (`:538-540`, `:565-567`): the source from the frame that read
    ///   the release (`:539`), here the release's own time.
    /// - A long note held to its end, or decided at its head's press: its
    ///   head's (`:572-573`). A BAD head is decided at its press here,
    ///   where the source's long note waits for its release (`:428`), the
    ///   preset's known difference.
    fn source_timing(
        &self,
        l: usize,
        note: usize,
        judgment: Judgment,
        deviation: Option<TimeUs>,
        release: Option<TimeUs>,
        t: TimeUs,
    ) -> Option<i64> {
        let neg = |d: TimeUs| d.0.saturating_neg();
        Some(match (deviation, release) {
            (None, _) => self.notes.get(l)?.get(note)?.time.0.saturating_sub(t.0),
            (Some(head), None) => neg(head),
            (Some(head), Some(r)) => {
                let (head, r) = (neg(head), neg(r));
                let chosen = if head.unsigned_abs() > r.unsigned_abs() {
                    head
                } else {
                    r
                };
                if judgment >= Judgment::Bad && chosen > 0 {
                    r
                } else {
                    chosen
                }
            }
        })
    }

    /// Lane `l`'s visual state set to `code` by an event at `at`, a press
    /// when `by_press`; a miss's `None` leaves it.
    fn set_lane(&mut self, l: usize, code: Option<u8>, at: TimeUs, by_press: bool) {
        if let (Some(code), Some(lane)) = (code, self.lanes.get_mut(l)) {
            *lane = LaneJudge {
                code,
                at: Some(at),
                by_press,
            };
        }
    }

    /// A press's bomb on lane `l` when `grade` is within the bomb
    /// threshold: grades are ordered PERFECT first and empty POOR last
    /// (`play/JudgeManager.java:673`).
    fn feedback(&mut self, l: usize, grade: Grade, at: TimeUs) {
        if self.bomb_up_to.is_some_and(|up| grade as u8 <= up as u8)
            && let Some(bomb) = self.bomb.get_mut(l)
        {
            *bomb = Some(at);
        }
    }
}
