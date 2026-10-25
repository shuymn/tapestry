//! The incremental evaluator: inputs and chart deadlines processed in one
//! fixed order, finalized only up to the watermark.

use std::collections::VecDeque;

use bms_model::{Chart, LANE_COUNT, Lane, NoteKind, SoundId, TimeUs};
use rules::{Assignment, JudgeWindows, Judgment, MissRule, PerJudgment, RulePreset, SameTimeOrder};

use crate::EvalError;
use crate::convert::{ConversionState, LaneEdge};
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
    /// A press judged no note but had one close ahead (an empty POOR).
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

/// Kinds of chart deadline; at equal times they are processed in this
/// order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum DeadlineKind {
    /// The late edge of an unjudged note's miss window.
    Miss,
}

/// A chart deadline, prepared at construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Deadline {
    time: TimeUs,
    kind: DeadlineKind,
    /// Index into the lane's targets.
    target: usize,
}

/// One lane's notes to judge and its deadlines.
#[derive(Clone, Debug, Default)]
struct LaneState {
    /// Indices into the chart lane of the judgeable notes (taps and LN
    /// heads), in time order.
    targets: Vec<usize>,
    /// Whether each target is judged.
    judged: Vec<bool>,
    /// The first unjudged target.
    cursor: usize,
    /// Deadlines sorted by `(time, kind, target)`.
    deadlines: Vec<Deadline>,
    /// The first deadline not yet processed or skipped.
    next: usize,
}

impl LaneState {
    fn mark_judged(&mut self, target: usize) {
        self.judged[target] = true;
        while self.judged.get(self.cursor) == Some(&true) {
            self.cursor += 1;
        }
    }

    /// The first deadline still to process, skipping those whose own
    /// condition says so: a miss deadline of a judged note.
    fn head(&mut self) -> Option<Deadline> {
        while let Some(&d) = self.deadlines.get(self.next) {
            let skip = match d.kind {
                DeadlineKind::Miss => self.judged[d.target],
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
/// while the input ring and the output have room.
#[derive(Debug)]
pub struct Evaluator<'a> {
    chart: &'a Chart,
    rule: &'a RulePreset,
    device: &'a DeviceProfile,
    windows: JudgeWindows,
    lanes: [LaneState; LANE_COUNT],
    conversion: ConversionState,
    inputs: VecDeque<(TimeUs, PhysicalInputEvent)>,
    seq: u64,
    tally: Tally,
}

impl<'a> Evaluator<'a> {
    /// Prepares evaluation: scales the windows by the chart's rank, builds
    /// each lane's deadlines, and sets up input conversion from `start`.
    /// `input_capacity` sizes the ring of inputs pushed but not yet
    /// processed.
    ///
    /// # Errors
    ///
    /// An invalid preset, a rank whose scale is unusable, a deadline outside
    /// the `i64` range, or a source binding one control twice.
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
            MissRule::PoorAfterLateBad => windows.bad.late,
        };
        let mut lanes: [LaneState; LANE_COUNT] = Default::default();
        for (state, notes) in lanes.iter_mut().zip(&chart.lanes) {
            state.targets = notes
                .iter()
                .enumerate()
                .filter(|(_, n)| matches!(n.kind, NoteKind::Tap | NoteKind::Long { .. }))
                .map(|(i, _)| i)
                .collect();
            state.judged = vec![false; state.targets.len()];
            state.deadlines = state
                .targets
                .iter()
                .enumerate()
                .map(|(target, &i)| {
                    let time = notes[i]
                        .time
                        .checked_add(miss_after)
                        .ok_or(EvalError::TimeOverflow)?;
                    Ok(Deadline {
                        time,
                        kind: DeadlineKind::Miss,
                        target,
                    })
                })
                .collect::<Result<_, EvalError>>()?;
            // Note times being sorted does not make deadlines sorted once
            // other kinds join, so sort explicitly.
            state.deadlines.sort_unstable();
        }
        Ok(Self {
            chart,
            rule,
            device,
            windows,
            lanes,
            conversion: ConversionState::init(device, start)?,
            inputs: VecDeque::with_capacity(input_capacity),
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

    /// Appends an input, in the order [`merge`](crate::merge) produced.
    /// Inputs must come in nondecreasing evaluation time and not below a
    /// watermark already advanced to. Beyond the capacity given at
    /// construction the ring grows (allocating) rather than dropping input.
    pub fn push_input(&mut self, ev: PhysicalInputEvent) {
        let t = evaluation_time(&ev, self.device);
        self.inputs.push_back((t, ev));
    }

    /// Processes every input and deadline at or before `watermark`, in the
    /// order (time, inputs before deadlines, deadline kind, lane, sequence),
    /// and appends the resulting events to `out`.
    pub fn advance_to(&mut self, watermark: TimeUs, out: &mut Vec<JudgeEvent>) {
        loop {
            let input = self
                .inputs
                .front()
                .map(|&(t, _)| t)
                .filter(|&t| t <= watermark);
            let deadline = self.next_deadline().filter(|(d, _)| d.time <= watermark);
            match (input, deadline) {
                (None, None) => return,
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

    /// The earliest pending deadline over all lanes, by (time, kind, lane).
    fn next_deadline(&mut self) -> Option<(Deadline, usize)> {
        let mut best: Option<(Deadline, usize)> = None;
        for (lane, state) in self.lanes.iter_mut().enumerate() {
            if let Some(d) = state.head()
                && best.is_none_or(|(b, bl)| (d.time, d.kind, lane) < (b.time, b.kind, bl))
            {
                best = Some((d, lane));
            }
        }
        best
    }

    fn process_deadline(&mut self, d: Deadline, lane: usize, out: &mut Vec<JudgeEvent>) {
        self.lanes[lane].next += 1;
        match d.kind {
            DeadlineKind::Miss => {
                self.judge(lane, d.target, Judgment::Poor, None, d.time, None, out);
            }
        }
    }

    fn process_input(&mut self, out: &mut Vec<JudgeEvent>) {
        let Some((t, ev)) = self.inputs.pop_front() else {
            return;
        };
        let edges = self.conversion.apply(&ev);
        for &edge in edges.as_slice() {
            match edge {
                LaneEdge::Press(lane) => self.press(lane.index(), t, ev.received, out),
                // Releases judge nothing for taps.
                LaneEdge::Release(_) => {}
            }
        }
    }

    /// A press edge on `lane` at evaluation time `t`.
    fn press(&mut self, lane: usize, t: TimeUs, origin: TimeUs, out: &mut Vec<JudgeEvent>) {
        let bad = self.windows.bad;
        let notes = &self.chart.lanes[lane];
        let state = &self.lanes[lane];
        let at = i128::from(t.0);
        // Candidates: unjudged targets with −early ≤ t − note ≤ late (BAD).
        let mut best: Option<(i128, usize)> = None;
        for k in state.cursor..state.targets.len() {
            let deviation = at - i128::from(notes[state.targets[k]].time.0);
            if deviation < -i128::from(bad.early.0) {
                break;
            }
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
        if let Some((deviation, k)) = best {
            let note = notes[state.targets[k]];
            // Within the BAD window, so it fits in i64.
            let deviation = TimeUs(i64::try_from(deviation).unwrap_or(i64::MAX));
            if let Some(sound) = note.sound {
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
            let judgment = judgment_of(&self.windows, deviation);
            self.judge(lane, k, judgment, Some(deviation), t, Some(origin), out);
            return;
        }
        // No note inside BAD: the first unjudged target is the next ahead.
        let ahead = (state.cursor..state.targets.len())
            .find(|&k| !state.judged[k])
            .map(|k| notes[state.targets[k]]);
        let Some(next) = ahead else {
            return;
        };
        if let Some(sound) = next.sound {
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
        let gap = i128::from(next.time.0) - at;
        let rule = self.rule.empty_poor;
        if gap > 0 && gap <= i128::from(rule.ahead.0) {
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
        }
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
        let state = &mut self.lanes[lane];
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
