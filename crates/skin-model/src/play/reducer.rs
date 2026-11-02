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
use rules::{GaugeValue, LnMode};

use super::schema::Grade;

/// A long note's head and end times.
#[derive(Clone, Copy, Debug)]
struct Long {
    head: TimeUs,
    end: TimeUs,
}

/// The effect-sourced state of one attempt.
#[derive(Clone, Debug)]
pub(crate) struct Reducer {
    /// Per lane and note: the long note's times, `None` for a tap or mine.
    longs: [Box<[Option<Long>]>; LANE_COUNT],
    /// The long-note judgment mode the notes are drawn by: LN, the only
    /// mode judged (design.md "LN Judgment Modes").
    pub(crate) ln_mode: LnMode,
    pub(crate) bomb: [Option<TimeUs>; LANE_COUNT],
    pub(crate) mine: [Option<TimeUs>; LANE_COUNT],
    pub(crate) judge: Option<(Grade, TimeUs)>,
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
    /// The bytes its buffers hold: every lane's long notes, and each
    /// gauge's maximum and when it reached it.
    pub(crate) fn heap_bytes(&self) -> usize {
        let longs: usize = self.longs.iter().map(|l| size_of_val(&**l)).sum();
        longs
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
        let longs = std::array::from_fn(|l| {
            chart.lanes.get(l).map_or_else(Box::default, |notes| {
                notes
                    .iter()
                    .map(|n| match n.kind {
                        NoteKind::Long { end, .. } => Some(Long { head: n.time, end }),
                        NoteKind::Tap | NoteKind::Mine { .. } => None,
                    })
                    .collect()
            })
        });
        let (gauge_max, gauge_top): (Vec<_>, Vec<_>) = initial
            .map(|(value, top)| ((value >= top).then_some(play_at), top))
            .unzip();
        Self {
            longs,
            ln_mode: LnMode::Ln,
            bomb: [None; LANE_COUNT],
            mine: [None; LANE_COUNT],
            judge: None,
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
    /// gate): no lane bombed or hit by a mine, nothing judged, and each
    /// gauge's maximum as the attempt started. Allocates nothing.
    pub(crate) fn rearm(&mut self) {
        self.bomb = [None; LANE_COUNT];
        self.mine = [None; LANE_COUNT];
        self.judge = None;
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
                let l = lane.index();
                self.judge = Some((Grade::of(judgment), t));
                self.feedback(l, Grade::of(judgment), t);
            }
            JudgeEventKind::MineHit { lane, .. } => {
                if let Some(mine) = self.mine.get_mut(lane.index()) {
                    *mine = Some(t);
                }
            }
            JudgeEventKind::EmptyPoor { lane } => {
                self.judge = Some((Grade::EmptyPoor, t));
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
        self.longs.get(l)?.get(note).copied().flatten()
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
