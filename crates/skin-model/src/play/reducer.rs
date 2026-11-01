//! The reducer: edge timers sourced from the drained effects, in emission
//! order (stage3.md R3).
//!
//! It reads only each event's kind and chart time, never the frame, so its
//! state after a sequence of effects does not depend on how drains split
//! the sequence. Feedback follows the play screen's rules: a press, an
//! empty POOR included, gives a bomb when its grade is within the skin's
//! bomb threshold, a long note's completion gives its judgment text but
//! never a bomb or a hold, and the visual hold starts at a head press
//! before the long note's end, a BAD head included.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
#![deny(clippy::float_arithmetic, clippy::float_cmp)]

use bms_model::{Chart, LANE_COUNT, NoteKind, TimeUs};
use eval::{JudgeEvent, JudgeEventKind, Section};
use rules::{GaugeValue, LnMode};

use super::schema::Grade;

/// One bit per note of a lane.
#[derive(Clone, Debug, Default)]
struct Bits(Box<[u64]>);

impl Bits {
    fn sized(n: usize) -> Self {
        Self(vec![0; n.div_ceil(64)].into_boxed_slice())
    }

    /// Sets bit `i`; an index past the lane is ignored.
    fn set(&mut self, i: usize) {
        if let Some(w) = self.0.get_mut(i / 64) {
            *w |= 1 << (i % 64);
        }
    }

    fn get(&self, i: usize) -> bool {
        self.0.get(i / 64).is_some_and(|w| w & (1 << (i % 64)) != 0)
    }
}

/// A long note's head and end times.
#[derive(Clone, Copy, Debug)]
struct Long {
    head: TimeUs,
    end: TimeUs,
}

/// A lane's visual long-note hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Hold {
    pub(crate) note: usize,
    pub(crate) start: TimeUs,
    /// The long note's end: the hold reads off from here even if its
    /// `LnHoldEnded` was dropped.
    pub(crate) end: TimeUs,
}

/// The effect-sourced state of one attempt.
#[derive(Clone, Debug)]
pub(crate) struct Reducer {
    /// Per lane and note: the long note's times, `None` for a tap or mine.
    longs: [Box<[Option<Long>]>; LANE_COUNT],
    /// Per lane: the notes no longer drawn, judged taps and hit mines.
    done: [Bits; LANE_COUNT],
    /// The long-note judgment mode the notes are drawn by: LN, the only
    /// mode judged (design.md "LN Judgment Modes").
    pub(crate) ln_mode: LnMode,
    pub(crate) hold: [Option<Hold>; LANE_COUNT],
    pub(crate) bomb: [Option<TimeUs>; LANE_COUNT],
    pub(crate) mine: [Option<TimeUs>; LANE_COUNT],
    pub(crate) judge: Option<(Grade, TimeUs)>,
    pub(crate) combo_up: Option<TimeUs>,
    targets: u32,
    pub(crate) full_combo: Option<TimeUs>,
    /// Per gauge: when it reached its maximum, if it is there by the
    /// effects delivered so far.
    pub(crate) gauge_max: Box<[Option<TimeUs>]>,
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
    /// The bytes its buffers hold: every lane's long notes and done bits.
    pub(crate) fn heap_bytes(&self) -> usize {
        let longs: usize = self.longs.iter().map(|l| size_of_val(&**l)).sum();
        let done: usize = self.done.iter().map(|d| size_of_val(&*d.0)).sum();
        longs + done
    }

    /// A clean reducer for one attempt: `initial` gives each gauge's
    /// initial value and maximum, and a gauge whose initial value is at its
    /// maximum reads on from `play_at`, the attempt's start after the
    /// countdown.
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
        let done = std::array::from_fn(|l| Bits::sized(chart.lanes.get(l).map_or(0, Vec::len)));
        let (gauge_max, gauge_top): (Vec<_>, Vec<_>) = initial
            .map(|(value, top)| ((value >= top).then_some(play_at), top))
            .unzip();
        Self {
            longs,
            done,
            ln_mode: LnMode::Ln,
            hold: [None; LANE_COUNT],
            bomb: [None; LANE_COUNT],
            mine: [None; LANE_COUNT],
            judge: None,
            combo_up: None,
            targets,
            full_combo: None,
            gauge_max: gauge_max.into_boxed_slice(),
            gauge_top: gauge_top.into_boxed_slice(),
            empty_poor_breaks_combo,
            bomb_up_to,
        }
    }

    /// The attempt's target count.
    pub(crate) fn targets(&self) -> u32 {
        self.targets
    }

    /// Whether `note` on lane `l` is no longer drawn: a judged tap or a
    /// hit mine. A long note is never done; it is drawn until its end
    /// passes the line.
    pub(crate) fn done(&self, l: usize, note: usize) -> bool {
        self.done.get(l).is_some_and(|b| b.get(note))
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
                self.end_hold(l, note);
                let long = self.long(l, note);
                let Some(Long { head, end }) = long else {
                    if let Some(bits) = self.done.get_mut(l) {
                        bits.set(note);
                    }
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
                // A head judged at its press: a press before the end (a
                // BAD head) starts the visual hold.
                if deviation.is_some()
                    && t < end
                    && let Some(hold) = self.hold.get_mut(l)
                {
                    *hold = Some(Hold {
                        note,
                        start: t,
                        end,
                    });
                }
                self.feedback(l, Grade::of(judgment), t);
            }
            JudgeEventKind::LnHeld {
                lane,
                note,
                judgment,
                ..
            } => {
                let l = lane.index();
                let long = self.long(l, note);
                if let (Some(Long { end, .. }), Some(hold)) = (long, self.hold.get_mut(l)) {
                    *hold = Some(Hold {
                        note,
                        start: t,
                        end,
                    });
                }
                self.judge = Some((Grade::of(judgment), t));
                self.feedback(l, Grade::of(judgment), t);
            }
            JudgeEventKind::LnHoldEnded { lane, note } => self.end_hold(lane.index(), note),
            JudgeEventKind::MineHit { lane, note } => {
                let l = lane.index();
                if let Some(mine) = self.mine.get_mut(l) {
                    *mine = Some(t);
                }
                if let Some(bits) = self.done.get_mut(l) {
                    bits.set(note);
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
            JudgeEventKind::SongEnd | JudgeEventKind::PlaySound { .. } => {}
        }
    }

    /// Note `note` on lane `l` if it is a long note.
    fn long(&self, l: usize, note: usize) -> Option<Long> {
        self.longs.get(l)?.get(note).copied().flatten()
    }

    /// Ends lane `l`'s hold if it holds `note`.
    fn end_hold(&mut self, l: usize, note: usize) {
        if let Some(hold) = self.hold.get_mut(l)
            && hold.is_some_and(|h| h.note == note)
        {
            *hold = None;
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

    /// Lane `l`'s visual hold at `now`: off once the long note's end has
    /// passed, even if its `LnHoldEnded` was dropped, and then cleared.
    pub(crate) fn hold_at(&mut self, l: usize, now: TimeUs) -> Option<Hold> {
        let slot = self.hold.get_mut(l)?;
        if slot.is_some_and(|h| h.end <= now) {
            *slot = None;
        }
        *slot
    }
}
