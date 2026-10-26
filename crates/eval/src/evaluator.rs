//! The incremental evaluator: inputs and chart deadlines processed in one
//! fixed order, finalized only up to the watermark.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::collections::VecDeque;

use bms_model::{Chart, LANE_COUNT, Lane, LnMode as ChartLnMode, NoteKind, SoundId, TimeUs};
use rules::{
    Assignment, Denominator, GaugeValue, JudgeWindows, Judgment, LateHeadRule, LnMode, MissRule,
    PerJudgment, RulePreset, SameTimeOrder, TotalMode,
};

use crate::EvalError;
use crate::convert::{ConversionState, LaneEdge};
use crate::gauge::{Cause, GaugeState, Gauges, increment};
use crate::input::{DeviceProfile, InputStart, PhysicalInputEvent, evaluation_time};

/// One output of the evaluator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JudgeEvent {
    /// Position in the evaluator's output, from 0 without gaps.
    pub seq: u64,
    /// The evaluation time it happened at.
    pub time: TimeUs,
    /// The raw receive time of the input that caused it (not offset), or
    /// `None` for an event caused by a deadline. Several events of one batch
    /// share it; it is for measuring latency, never an identifier.
    pub origin: Option<TimeUs>,
    /// What happened.
    pub kind: JudgeEventKind,
}

/// What a [`JudgeEvent`] reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JudgeEventKind {
    /// A note was judged.
    Judged {
        /// The note's lane.
        lane: Lane,
        /// The note's index in its lane of [`Chart::lanes`].
        note: usize,
        /// The judgment.
        judgment: Judgment,
        /// Press time minus note time, or `None` for a miss.
        deviation: Option<TimeUs>,
        /// The combo after this judgment.
        combo: u32,
    },
    /// A press judged no note but had one close by (an empty POOR).
    EmptyPoor {
        /// The lane pressed.
        lane: Lane,
    },
    /// A press on `lane` plays `sound`.
    PlaySound {
        /// The lane pressed.
        lane: Lane,
        /// The sound.
        sound: SoundId,
    },
    /// A long note whose head was judged finished.
    LnEnd {
        /// The note's lane.
        lane: Lane,
        /// The note's index in its lane of [`Chart::lanes`].
        note: usize,
        /// How it finished.
        outcome: LnOutcome,
    },
    /// A mine passed while its lane was held.
    MineHit {
        /// The mine's lane.
        lane: Lane,
        /// The mine's index in its lane of [`Chart::lanes`].
        note: usize,
    },
    /// A gauge's value after the event just before it. After each event
    /// that changes gauges, every gauge is reported in the preset's order.
    Gauge {
        /// The gauge's index in the preset's gauges.
        gauge: usize,
        /// The real value.
        value: GaugeValue,
        /// The value had the gauge never failed.
        reference: GaugeValue,
    },
    /// Evaluation is complete: the watermark reached the chart's final
    /// deadline. Emitted once, at that deadline's time.
    SongEnd,
}

/// How a long note finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LnOutcome {
    /// Held to its end, released within the margin, or its head was pressed
    /// at or after its end.
    Completed,
    /// Released too early: an LN break.
    Broken,
}

/// Running totals of a play.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tally {
    /// Judgments per kind; empty POORs are counted only if the preset says
    /// so.
    pub counts: PerJudgment<u32>,
    /// EX score.
    pub ex_score: u32,
    /// BP (miss count).
    pub bp: u32,
    /// Current combo.
    pub combo: u32,
    /// Highest combo.
    pub max_combo: u32,
    /// Number of empty POORs.
    pub empty_poor: u32,
    /// Number of LN breaks.
    pub ln_breaks: u32,
}

impl Tally {
    const fn new() -> Self {
        Self {
            counts: PerJudgment {
                perfect: 0,
                great: 0,
                good: 0,
                bad: 0,
                poor: 0,
            },
            ex_score: 0,
            bp: 0,
            combo: 0,
            max_combo: 0,
            empty_poor: 0,
            ln_breaks: 0,
        }
    }

    const fn count_mut(&mut self, j: Judgment) -> &mut u32 {
        match j {
            Judgment::Perfect => &mut self.counts.perfect,
            Judgment::Great => &mut self.counts.great,
            Judgment::Good => &mut self.counts.good,
            Judgment::Bad => &mut self.counts.bad,
            Judgment::Poor => &mut self.counts.poor,
        }
    }

    fn apply(&mut self, breaks_combo: bool, counts_bp: bool) {
        if breaks_combo {
            self.combo = 0;
        } else {
            self.combo = self.combo.saturating_add(1);
            self.max_combo = self.max_combo.max(self.combo);
        }
        if counts_bp {
            self.bp = self.bp.saturating_add(1);
        }
    }
}

/// Kinds of deadline; at equal times they are processed in this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum DeadlineKind {
    /// A pending scratch release (input-dependent, not stored per lane).
    /// It stands for the release the player made by stopping, so it comes
    /// first among deadlines, as a release input would.
    ScratchRelease,
    /// The late edge of an unjudged note's miss window.
    Miss,
    /// The end of a long note not yet finished.
    LnEnd,
    /// A mine passing.
    Mine,
    /// The chart's final deadline, after every lane deadline (not stored per
    /// lane).
    SongEnd,
}

/// A chart deadline, prepared at construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Deadline {
    time: TimeUs,
    kind: DeadlineKind,
    /// Index into the lane's targets for `Miss` and `LnEnd`; into the chart
    /// lane for `Mine`; the axis for `ScratchRelease`.
    index: usize,
}

/// One lane's notes to judge and its deadlines.
#[derive(Clone, Debug, Default)]
struct LaneState {
    /// Indices into the chart lane of the judgeable notes (taps and LN
    /// heads), in time order.
    targets: Vec<usize>,
    /// Whether each target is judged.
    judged: Vec<bool>,
    /// The long note whose head was judged and which is held now. A long
    /// note is finished (completed, broken, or its head missed) once its
    /// head is judged and it is not this one.
    holding: Option<usize>,
    /// The first unjudged target.
    cursor: usize,
    /// Deadlines sorted by `(time, kind, index)`.
    deadlines: Vec<Deadline>,
    /// The first deadline not yet processed or skipped.
    next: usize,
}

impl LaneState {
    fn mark_judged(&mut self, target: usize) {
        #[expect(
            clippy::indexing_slicing,
            reason = "callers pass a position in `targets`, and `judged` has one entry per target"
        )]
        {
            self.judged[target] = true;
        }
        while self.judged.get(self.cursor) == Some(&true) {
            self.cursor += 1;
        }
    }

    /// The first deadline still to process, skipping those whose own
    /// condition says so: a miss deadline of a judged note, and an LN end of
    /// a finished long note (a judged head does not finish it). A mine is
    /// never skipped.
    fn head(&mut self) -> Option<Deadline> {
        while let Some(&d) = self.deadlines.get(self.next) {
            #[expect(
                clippy::indexing_slicing,
                reason = "miss and LN end deadlines hold a position in `targets`, which `judged` matches in length"
            )]
            let skip = match d.kind {
                DeadlineKind::Miss => self.judged[d.index],
                DeadlineKind::LnEnd => self.judged[d.index] && self.holding != Some(d.index),
                DeadlineKind::Mine | DeadlineKind::ScratchRelease | DeadlineKind::SongEnd => false,
            };
            if !skip {
                return Some(d);
            }
            self.next += 1;
        }
        None
    }
}

/// The incremental evaluator. It references the chart, preset and device
/// profile, and after construction allocates nothing in
/// [`push_input`](Self::push_input) and [`advance_to`](Self::advance_to)
/// while the output has room; a full input ring refuses input instead.
#[derive(Debug)]
pub struct Evaluator<'a> {
    chart: &'a Chart,
    rule: &'a RulePreset,
    device: &'a DeviceProfile,
    windows: JudgeWindows,
    lanes: [LaneState; LANE_COUNT],
    conversion: ConversionState,
    inputs: VecDeque<(TimeUs, PhysicalInputEvent)>,
    /// The ring's fixed capacity.
    input_capacity: usize,
    /// The watermark last advanced to: everything at or before it is final.
    finalized: Option<TimeUs>,
    /// The latest deadline of the chart; evaluation is complete once the
    /// watermark reaches it.
    final_deadline: TimeUs,
    /// Whether [`JudgeEventKind::SongEnd`] was emitted.
    ended: bool,
    gauges: Gauges,
    seq: u64,
    tally: Tally,
}

/// Why [`Evaluator::push_input`] refused an input. The evaluator is left
/// unchanged; the caller marks the input log incomplete or aborts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushError {
    /// The input ring is at the capacity given at construction.
    Full,
    /// The input's evaluation time is at or before the finalized watermark,
    /// so it can no longer be evaluated in order.
    Finalized {
        /// The input's evaluation time.
        time: TimeUs,
        /// The finalized watermark.
        finalized: TimeUs,
    },
    /// The input's evaluation time is earlier than the previous input's.
    OutOfOrder {
        /// The input's evaluation time.
        time: TimeUs,
        /// The previous input's evaluation time.
        last: TimeUs,
    },
}

/// Why [`Evaluator::advance_to`] refused to advance. Nothing was processed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdvanceError {
    /// The watermark is earlier than one already advanced to.
    Backward {
        /// The requested watermark.
        watermark: TimeUs,
        /// The finalized watermark.
        finalized: TimeUs,
    },
}

impl<'a> Evaluator<'a> {
    /// Prepares evaluation: scales the windows by the chart's rank, builds
    /// each lane's deadlines, computes the gauge increment, and sets up input
    /// conversion from `start`. `input_capacity` sizes the ring of inputs
    /// pushed but not yet processed.
    ///
    /// # Errors
    ///
    /// An invalid preset, a rank whose scale is unusable, a long-note mode
    /// the preset cannot judge, a chart without judgeable notes, a deadline
    /// outside the `i64` range, or a source binding one control twice.
    pub fn new(
        chart: &'a Chart,
        rule: &'a RulePreset,
        device: &'a DeviceProfile,
        start: &InputStart,
        input_capacity: usize,
    ) -> Result<Self, EvalError> {
        rule.validate().map_err(EvalError::InvalidPreset)?;
        let windows = rule
            .rank_scale
            .scale_milli(chart.meta.rank)
            .and_then(|scale| rule.windows.scaled(scale))
            .ok_or(EvalError::InvalidRank)?;
        let miss_after = match rule.miss {
            MissRule::PoorAfterLateBadOrOvertaken => windows.bad.late,
        };
        match (rule.ln_mode.default, chart.meta.ln_mode) {
            (LnMode::Ln, None | Some(ChartLnMode::Ln)) => {}
            (LnMode::Ln, Some(mode)) => return Err(EvalError::UnsupportedLnMode(mode)),
        }
        let mut lanes: [LaneState; LANE_COUNT] = Default::default();
        for (state, notes) in lanes.iter_mut().zip(&chart.lanes) {
            // Reserved at its exact size, which the recording policy counts.
            let judgeable =
                |n: &bms_model::Note| matches!(n.kind, NoteKind::Tap | NoteKind::Long { .. });
            state.targets = Vec::with_capacity(notes.iter().filter(|n| judgeable(n)).count());
            state.targets.extend(
                notes
                    .iter()
                    .enumerate()
                    .filter(|(_, n)| judgeable(n))
                    .map(|(i, _)| i),
            );
            state.judged = vec![false; state.targets.len()];
            let mut deadlines = Vec::with_capacity(notes.len() * 2);
            for (target, &i) in state.targets.iter().enumerate() {
                #[expect(
                    clippy::indexing_slicing,
                    reason = "construction: `targets` holds indices into this lane's notes"
                )]
                let note = notes[i];
                let miss = note
                    .time
                    .checked_add(miss_after)
                    .ok_or(EvalError::TimeOverflow)?;
                deadlines.push(Deadline {
                    time: miss,
                    kind: DeadlineKind::Miss,
                    index: target,
                });
                if let NoteKind::Long { end, .. } = note.kind {
                    deadlines.push(Deadline {
                        time: end,
                        kind: DeadlineKind::LnEnd,
                        index: target,
                    });
                }
            }
            for (i, note) in notes.iter().enumerate() {
                if matches!(note.kind, NoteKind::Mine { .. }) {
                    deadlines.push(Deadline {
                        time: note.time,
                        kind: DeadlineKind::Mine,
                        index: i,
                    });
                }
            }
            // Note times being sorted does not make deadlines sorted (a tap
            // at 1000 ms misses at 1180 ms, after a mine at 1100 ms).
            deadlines.sort_unstable();
            state.deadlines = deadlines;
        }
        let notes = match rule.denominator {
            Denominator::TapsAndLnHeads => lanes.iter().map(|l| l.targets.len()).sum::<usize>(),
        };
        if notes == 0 {
            return Err(EvalError::NoJudgeableNotes);
        }
        let final_deadline = lanes
            .iter()
            .filter_map(|l| l.deadlines.last().map(|d| d.time))
            .max()
            .unwrap_or(TimeUs(0));
        // A #TOTAL that is not positive counts as absent.
        let total_milli = chart
            .meta
            .total_milli
            .filter(|&t| t > 0)
            .unwrap_or(rule.default_total_milli);
        let increment = match rule.total_mode {
            TotalMode::ScalesIncrease => increment(total_milli, notes),
        };
        Ok(Self {
            chart,
            rule,
            device,
            windows,
            lanes,
            conversion: ConversionState::init(device, start)?,
            inputs: VecDeque::with_capacity(input_capacity),
            input_capacity,
            finalized: None,
            final_deadline,
            ended: false,
            gauges: Gauges::new(&rule.gauges, increment),
            seq: 0,
            tally: Tally::new(),
        })
    }

    /// The judgment windows after the rank scale.
    #[must_use]
    pub const fn windows(&self) -> &JudgeWindows {
        &self.windows
    }

    /// The running totals.
    #[must_use]
    pub const fn tally(&self) -> &Tally {
        &self.tally
    }

    /// Every gauge's state, in the preset's order.
    #[must_use]
    pub fn gauges(&self) -> &[GaugeState] {
        self.gauges.states()
    }

    /// The chart's latest deadline: tap and LN head misses, LN ends and
    /// mines.
    #[must_use]
    pub const fn final_deadline(&self) -> TimeUs {
        self.final_deadline
    }

    /// Whether evaluation is complete ([`JudgeEventKind::SongEnd`] was
    /// emitted).
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.ended
    }

    /// The most events a whole play with at most `inputs` inputs can emit.
    /// An output reserved to this capacity (and cleared between steps, never
    /// shrunk) never reallocates in [`advance_to`](Self::advance_to).
    ///
    /// Each judgeable note is judged once (1 event plus one per gauge),
    /// finishes as a long note once, and breaks at most once (one per
    /// gauge). Each input causes at most one press (an axis reversing
    /// releases first, which only finishes long notes), which plays a sound
    /// and may be an empty POOR (1 plus one per gauge). A scratch release
    /// deadline only finishes long notes. Each mine hits at most once (1
    /// plus one per gauge). The song ends once.
    #[must_use]
    pub fn max_events(&self, inputs: usize) -> usize {
        let g = self.gauges.states().len();
        let notes: usize = self.lanes.iter().map(|l| l.targets.len()).sum();
        let mines = self
            .lanes
            .iter()
            .flat_map(|l| &l.deadlines)
            .filter(|d| d.kind == DeadlineKind::Mine)
            .count();
        notes
            .saturating_mul(2 + 2 * g)
            .saturating_add(inputs.saturating_mul(2 + g))
            .saturating_add(mines.saturating_mul(1 + g))
            .saturating_add(1)
    }

    /// Appends an input, in the order [`merge`](crate::merge) produced.
    /// Never allocates: the ring keeps the capacity given at construction.
    ///
    /// # Errors
    ///
    /// The input is refused, and nothing changes, when the ring is full,
    /// when its evaluation time is at or before the finalized watermark, or
    /// when it is earlier than the previous input's.
    pub fn push_input(&mut self, ev: PhysicalInputEvent) -> Result<(), PushError> {
        let time = evaluation_time(&ev, self.device);
        if let Some(finalized) = self.finalized
            && time <= finalized
        {
            return Err(PushError::Finalized { time, finalized });
        }
        // Inputs already processed are at or before `finalized`, so only the
        // queued ones need checking.
        if let Some(&(last, _)) = self.inputs.back()
            && time < last
        {
            return Err(PushError::OutOfOrder { time, last });
        }
        if self.inputs.len() >= self.input_capacity {
            return Err(PushError::Full);
        }
        self.inputs.push_back((time, ev));
        Ok(())
    }

    /// Processes every input and deadline at or before `watermark`, in the
    /// order (time, inputs before deadlines, deadline kind, lane, sequence),
    /// and appends the resulting events to `out`. Everything at or before
    /// `watermark` is then final. `out` grows only past its capacity; see
    /// [`max_events`](Self::max_events) for a capacity that is never passed.
    ///
    /// # Errors
    ///
    /// [`AdvanceError::Backward`], with nothing processed, when `watermark`
    /// is earlier than one already advanced to.
    pub fn advance_to(
        &mut self,
        watermark: TimeUs,
        out: &mut Vec<JudgeEvent>,
    ) -> Result<(), AdvanceError> {
        if let Some(finalized) = self.finalized
            && watermark < finalized
        {
            return Err(AdvanceError::Backward {
                watermark,
                finalized,
            });
        }
        self.finalized = Some(watermark);
        loop {
            let input = self
                .inputs
                .front()
                .map(|&(t, _)| t)
                .filter(|&t| t <= watermark);
            let deadline = self.next_deadline().filter(|(d, _)| d.time <= watermark);
            match (input, deadline) {
                (None, None) => return Ok(()),
                (Some(t), Some((d, lane))) => {
                    let deadline_first = match self.rule.same_time_order {
                        SameTimeOrder::InputsFirst => d.time < t,
                    };
                    if deadline_first {
                        self.process_deadline(d, lane, out);
                    } else {
                        self.process_input(out);
                    }
                }
                (Some(_), None) => self.process_input(out),
                (None, Some((d, lane))) => self.process_deadline(d, lane, out),
            }
        }
    }

    /// The earliest pending deadline by (time, kind, lane): the lanes'
    /// chart deadlines, the pending scratch releases, and the song end,
    /// which comes after every chart deadline (none is later than it).
    fn next_deadline(&mut self) -> Option<(Deadline, usize)> {
        let mut best: Option<(Deadline, usize)> = None;
        let mut offer = |d: Deadline, lane: usize| {
            if best.is_none_or(|(b, bl)| (d.time, d.kind, lane) < (b.time, b.kind, bl)) {
                best = Some((d, lane));
            }
        };
        for (lane, state) in self.lanes.iter_mut().enumerate() {
            if let Some(d) = state.head() {
                offer(d, lane);
            }
        }
        if let Some((time, lane, axis)) = self.conversion.next_release() {
            let d = Deadline {
                time,
                kind: DeadlineKind::ScratchRelease,
                index: axis,
            };
            offer(d, lane.index());
        }
        if !self.ended {
            let end = Deadline {
                time: self.final_deadline,
                kind: DeadlineKind::SongEnd,
                index: 0,
            };
            offer(end, 0);
        }
        best
    }

    fn process_deadline(&mut self, d: Deadline, lane: usize, out: &mut Vec<JudgeEvent>) {
        match d.kind {
            DeadlineKind::SongEnd => {
                self.ended = true;
                self.emit(d.time, None, JudgeEventKind::SongEnd, out);
                return;
            }
            DeadlineKind::ScratchRelease => {
                if let Some(LaneEdge::Release(lane)) = self.conversion.release_due(d.index) {
                    self.release(lane.index(), d.time, None, out);
                }
                return;
            }
            DeadlineKind::Miss | DeadlineKind::LnEnd | DeadlineKind::Mine => {}
        }
        #[expect(
            clippy::indexing_slicing,
            reason = "`lane` comes from `Lane::index`, below LANE_COUNT"
        )]
        let state = &mut self.lanes[lane];
        state.next += 1;
        #[expect(
            clippy::indexing_slicing,
            reason = "`lane` is below LANE_COUNT; a miss or LN end deadline holds a position in `targets`, a mine one an index into the chart lane"
        )]
        match d.kind {
            DeadlineKind::Miss => {
                // A missed head is judged, which finishes its long note too.
                self.judge(lane, d.index, Judgment::Poor, None, d.time, None, out);
            }
            DeadlineKind::LnEnd => {
                // An unjudged head still waits for its press or its miss. A
                // judged one is held here, since a release would have
                // finished it.
                if state.judged[d.index] {
                    state.holding = None;
                    self.ln_end(lane, d.index, LnOutcome::Completed, d.time, None, out);
                }
            }
            DeadlineKind::Mine => {
                if self.conversion.is_held(lane_of(lane)) {
                    let damage = match self.chart.lanes[lane][d.index].kind {
                        NoteKind::Mine { damage } => damage,
                        NoteKind::Tap | NoteKind::Long { .. } => 0,
                    };
                    let kind = JudgeEventKind::MineHit {
                        lane: lane_of(lane),
                        note: d.index,
                    };
                    self.emit(d.time, None, kind, out);
                    self.change_gauges(Cause::Mine(damage), d.time, None, out);
                }
            }
            DeadlineKind::ScratchRelease | DeadlineKind::SongEnd => {}
        }
    }

    fn process_input(&mut self, out: &mut Vec<JudgeEvent>) {
        let Some((t, ev)) = self.inputs.pop_front() else {
            return;
        };
        let edges = self.conversion.apply(&ev, t);
        for &edge in edges.as_slice() {
            match edge {
                LaneEdge::Press(lane) => self.press(lane.index(), t, ev.received, out),
                LaneEdge::Release(lane) => self.release(lane.index(), t, Some(ev.received), out),
            }
        }
    }

    /// A press edge on `lane` at evaluation time `t`.
    fn press(&mut self, lane: usize, t: TimeUs, origin: TimeUs, out: &mut Vec<JudgeEvent>) {
        let bad = self.windows.bad;
        #[expect(
            clippy::indexing_slicing,
            reason = "`lane` comes from `Lane::index`, below LANE_COUNT"
        )]
        let notes = &self.chart.lanes[lane];
        #[expect(
            clippy::indexing_slicing,
            reason = "`lane` comes from `Lane::index`, below LANE_COUNT"
        )]
        let state = &self.lanes[lane];
        let at = i128::from(t.0);
        #[expect(
            clippy::indexing_slicing,
            reason = "callers pass a position below `targets.len()`, and `targets` holds indices into the lane's notes"
        )]
        let time_of = |k: usize| i128::from(notes[state.targets[k]].time.0);
        let first = (state.cursor < state.targets.len()).then_some(state.cursor);
        // Candidates: unjudged targets with −early ≤ t − note ≤ late (BAD).
        let mut best: Option<(i128, usize)> = None;
        for k in state.cursor..state.targets.len() {
            let deviation = at - time_of(k);
            if deviation < -i128::from(bad.early.0) {
                break;
            }
            #[expect(
                clippy::indexing_slicing,
                reason = "the loop keeps `k` below `targets.len()`, which `judged` matches"
            )]
            if state.judged[k] || deviation > i128::from(bad.late.0) {
                continue;
            }
            let better = match self.rule.assignment {
                // Strictly nearer only, so a tie keeps the earlier note.
                Assignment::NearestTiesEarlier => {
                    best.is_none_or(|(d, _)| deviation.abs() < d.abs())
                }
            };
            if better {
                best = Some((deviation, k));
            }
        }
        // The judged note, else the next unjudged one (the cursor: no
        // unjudged note lies inside BAD, and earlier ones are missed).
        let target = best.map(|(_, k)| k).or(first);
        let rule = self.rule.empty_poor;
        // The first note at or after `t − behind`, judged or not, lies at or
        // before `t + ahead`.
        let empty_poor = best.is_none() && {
            let from = at - i128::from(rule.behind.0);
            #[expect(
                clippy::indexing_slicing,
                reason = "`targets` holds indices into the lane's notes"
            )]
            let k = state
                .targets
                .partition_point(|&i| i128::from(notes[i].time.0) < from);
            k < state.targets.len() && time_of(k) <= at + i128::from(rule.ahead.0)
        };
        #[expect(
            clippy::indexing_slicing,
            reason = "`target` is a candidate or the cursor, both below `targets.len()`"
        )]
        if let Some(sound) = target.and_then(|k| notes[state.targets[k]].sound) {
            self.emit(
                t,
                Some(origin),
                JudgeEventKind::PlaySound {
                    lane: lane_of(lane),
                    sound,
                },
                out,
            );
        }
        if let Some((deviation, k)) = best {
            match self.rule.miss {
                // The earlier notes are passed: each is missed now, which
                // also finishes a long note whose head it is, and its miss
                // deadline is then skipped. All of them are unjudged, since
                // under this rule a judgment passes every earlier note and
                // misses come in note order.
                MissRule::PoorAfterLateBadOrOvertaken =>
                {
                    #[expect(
                        clippy::indexing_slicing,
                        reason = "`lane` comes from `Lane::index`, below LANE_COUNT"
                    )]
                    for j in self.lanes[lane].cursor..k {
                        self.judge(lane, j, Judgment::Poor, None, t, Some(origin), out);
                    }
                }
            }
            #[expect(
                clippy::indexing_slicing,
                reason = "`lane` is below LANE_COUNT, `k` is a candidate below `targets.len()`, and `targets` indexes the lane's notes"
            )]
            let note = self.chart.lanes[lane][self.lanes[lane].targets[k]];
            // Within the BAD window, so it fits in i64.
            let deviation = TimeUs(i64::try_from(deviation).unwrap_or(i64::MAX));
            let judgment = judgment_of(&self.windows, deviation);
            self.judge(lane, k, judgment, Some(deviation), t, Some(origin), out);
            if let NoteKind::Long { end, .. } = note.kind {
                let completed = match self.rule.ln_mode.late_head {
                    // A head pressed at or after its end completes the LN at
                    // the press; the following release is ignored.
                    LateHeadRule::CompletesAtPress => t >= end,
                };
                if completed {
                    self.ln_end(lane, k, LnOutcome::Completed, t, Some(origin), out);
                } else {
                    #[expect(
                        clippy::indexing_slicing,
                        reason = "`lane` comes from `Lane::index`, below LANE_COUNT"
                    )]
                    {
                        self.lanes[lane].holding = Some(k);
                    }
                }
            }
            return;
        }
        if empty_poor {
            self.tally.empty_poor = self.tally.empty_poor.saturating_add(1);
            if rule.counted {
                let c = self.tally.count_mut(Judgment::Poor);
                *c = c.saturating_add(1);
            }
            if rule.breaks_combo {
                self.tally.combo = 0;
            }
            if rule.counts_bp {
                self.tally.bp = self.tally.bp.saturating_add(1);
            }
            self.emit(
                t,
                Some(origin),
                JudgeEventKind::EmptyPoor {
                    lane: lane_of(lane),
                },
                out,
            );
            self.change_gauges(Cause::EmptyPoor, t, Some(origin), out);
        }
    }

    /// A release edge on `lane` at evaluation time `t`: finishes the held
    /// long note, as a break if strictly earlier than its end minus the
    /// margin. `origin` is `None` for a scratch release deadline.
    fn release(
        &mut self,
        lane: usize,
        t: TimeUs,
        origin: Option<TimeUs>,
        out: &mut Vec<JudgeEvent>,
    ) {
        #[expect(
            clippy::indexing_slicing,
            reason = "`lane` comes from `Lane::index`, below LANE_COUNT"
        )]
        let state = &mut self.lanes[lane];
        let Some(k) = state.holding.take() else {
            return;
        };
        #[expect(
            clippy::indexing_slicing,
            reason = "`lane` is below LANE_COUNT, `holding` is a position in `targets`, and `targets` indexes the lane's notes"
        )]
        let end = self.chart.lanes[lane][state.targets[k]].end();
        let rule = self.rule.ln_break;
        let broken = i128::from(t.0) < i128::from(end.0) - i128::from(rule.release_margin.0);
        if !broken {
            self.ln_end(lane, k, LnOutcome::Completed, t, origin, out);
            return;
        }
        // The head's judgment, counts and EX score stay.
        self.tally.ln_breaks = self.tally.ln_breaks.saturating_add(1);
        if rule.breaks_combo {
            self.tally.combo = 0;
        }
        if rule.counts_bp {
            self.tally.bp = self.tally.bp.saturating_add(1);
        }
        self.ln_end(lane, k, LnOutcome::Broken, t, origin, out);
        self.change_gauges(Cause::LnBreak, t, origin, out);
    }

    fn ln_end(
        &mut self,
        lane: usize,
        target: usize,
        outcome: LnOutcome,
        time: TimeUs,
        origin: Option<TimeUs>,
        out: &mut Vec<JudgeEvent>,
    ) {
        #[expect(
            clippy::indexing_slicing,
            reason = "`lane` is below LANE_COUNT and callers pass a position in `targets`"
        )]
        let kind = JudgeEventKind::LnEnd {
            lane: lane_of(lane),
            note: self.lanes[lane].targets[target],
            outcome,
        };
        self.emit(time, origin, kind, out);
    }

    #[allow(clippy::too_many_arguments)]
    fn judge(
        &mut self,
        lane: usize,
        target: usize,
        judgment: Judgment,
        deviation: Option<TimeUs>,
        time: TimeUs,
        origin: Option<TimeUs>,
        out: &mut Vec<JudgeEvent>,
    ) {
        #[expect(
            clippy::indexing_slicing,
            reason = "`lane` comes from `Lane::index`, below LANE_COUNT"
        )]
        let state = &mut self.lanes[lane];
        #[expect(
            clippy::indexing_slicing,
            reason = "callers pass a position in `targets`"
        )]
        let note = state.targets[target];
        state.mark_judged(target);
        let score = &self.rule.score;
        let c = self.tally.count_mut(judgment);
        *c = c.saturating_add(1);
        self.tally.ex_score = self.tally.ex_score.saturating_add(*score.ex.get(judgment));
        self.tally.apply(
            *score.breaks_combo.get(judgment),
            *score.counts_bp.get(judgment),
        );
        let kind = JudgeEventKind::Judged {
            lane: lane_of(lane),
            note,
            judgment,
            deviation,
            combo: self.tally.combo,
        };
        self.emit(time, origin, kind, out);
        self.change_gauges(Cause::Judged(judgment), time, origin, out);
    }

    /// Applies `cause` to every gauge and reports each gauge's value.
    fn change_gauges(
        &mut self,
        cause: Cause,
        time: TimeUs,
        origin: Option<TimeUs>,
        out: &mut Vec<JudgeEvent>,
    ) {
        self.gauges
            .apply(&self.rule.gauges, &self.rule.mine, cause, time);
        for gauge in 0..self.gauges.states().len() {
            #[expect(
                clippy::indexing_slicing,
                reason = "the loop keeps `gauge` below the number of states"
            )]
            let s = self.gauges.states()[gauge];
            let kind = JudgeEventKind::Gauge {
                gauge,
                value: s.value,
                reference: s.reference,
            };
            self.emit(time, origin, kind, out);
        }
    }

    fn emit(
        &mut self,
        time: TimeUs,
        origin: Option<TimeUs>,
        kind: JudgeEventKind,
        out: &mut Vec<JudgeEvent>,
    ) {
        out.push(JudgeEvent {
            seq: self.seq,
            time,
            origin,
            kind,
        });
        self.seq += 1;
    }
}

/// The best judgment whose window contains `deviation`; POOR if none does.
fn judgment_of(windows: &JudgeWindows, deviation: TimeUs) -> Judgment {
    windows
        .table()
        .into_iter()
        .find(|(_, w)| -w.early.0 <= deviation.0 && deviation.0 <= w.late.0)
        .map_or(Judgment::Poor, |(j, _)| j)
}

fn lane_of(index: usize) -> Lane {
    u8::try_from(index)
        .ok()
        .and_then(Lane::new)
        .unwrap_or(Lane::SCRATCH)
}

// Lint fixtures under this module's attribute: `cargo xtask check-lints`.
#[cfg(tapestry_lint_fixtures)]
#[path = "../lint-fixtures/in_play/mod.rs"]
mod lint_fixtures;
