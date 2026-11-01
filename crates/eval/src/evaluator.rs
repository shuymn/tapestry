//! The incremental evaluator: inputs and chart deadlines processed in one
//! fixed order, finalized only up to the watermark.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::collections::VecDeque;

use bms_model::{
    Chart, DOMAIN, LANE_COUNT, Lane, LnMode as ChartLnMode, NoteKind, SoundId, TimeUs,
};
use rules::{
    Assignment, Denominator, GaugeValue, JudgeWindows, Judgment, LaneTable, LateHeadRule, LnMode,
    MissRule, PerJudgment, PerLaneClass, RulePreset, SameTimeOrder, TotalMode,
};

use crate::EvalError;
use crate::batch::{GaugePoint, GaugeResult};
use crate::convert::{ConversionState, LaneEdge};
use crate::gauge::{Cause, GaugeState, Gauges};
use crate::input::{
    ControlId, DeviceProfile, InputStart, PhysicalInputEvent, Section, SourceId, evaluation_time,
};

/// The bytes of one event of the evaluator's output.
pub const EVENT_BYTES: usize = size_of::<JudgeEvent>();
/// The bytes of one recorded input.
pub const INPUT_BYTES: usize = size_of::<PhysicalInputEvent>();
/// The bytes of one control of a held-input snapshot.
pub const HELD_BYTES: usize = size_of::<(SourceId, ControlId, i32)>();
/// The bytes of one slot of the evaluator's input ring.
pub const RING_SLOT_BYTES: usize = size_of::<(TimeUs, PhysicalInputEvent)>();
/// The bytes the evaluator's own tables may take per chart object (its
/// judgeable notes, their flags and its deadlines): an allowance, counted
/// once for every object of the chart.
pub const TABLE_BYTES_PER_OBJECT: usize = 64;
/// The allowance per gauge for its state, its `#TOTAL` correction and the
/// preset's validation, beside its history and its result.
pub const GAUGE_BYTES: usize = 256;
/// The allowance for a batch evaluation's small fixed parts: the
/// conversion state, the lane tables and each source's staging.
pub const BATCH_SMALL_BYTES: usize = 64 << 10;
/// The least capacity a growing vector of gauge points takes.
const MIN_HISTORY_CAPACITY: usize = 4;

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
    /// A note was judged: the only event that scores a note, emitted once
    /// per judgeable note. A long note is judged once, at the press that
    /// decides it, at the release that finishes it, or at its end.
    Judged {
        /// The note's lane.
        lane: Lane,
        /// The note's index in its lane of [`Chart::lanes`].
        note: usize,
        /// The judgment.
        judgment: Judgment,
        /// Press time minus note time (a long note's head press), or `None`
        /// for a miss.
        deviation: Option<TimeUs>,
        /// For a long note finished by a release, release time minus its
        /// end (not positive; `Some(0)` for a release exactly at the end);
        /// otherwise `None`.
        release: Option<TimeUs>,
        /// The combo after this judgment.
        combo: u32,
    },
    /// A long note's head was pressed before its end and graded GOOD or
    /// better, and the long note is held now. Feedback only: it changes no
    /// count, combo, EX score, BP or gauge; the long note's [`Judged`]
    /// follows at its release or end.
    ///
    /// [`Judged`]: JudgeEventKind::Judged
    LnHeld {
        /// The note's lane.
        lane: Lane,
        /// The note's index in its lane of [`Chart::lanes`].
        note: usize,
        /// The head's judgment.
        judgment: Judgment,
        /// Press time minus note time.
        deviation: TimeUs,
    },
    /// The visual hold of a long note whose head press started or decided
    /// it before its end ended: its lane was released (at or before the
    /// end) or its end passed while held. Emitted once per such long note,
    /// before the long note's judgment when both come together. Feedback
    /// only: it changes nothing scored.
    LnHoldEnded {
        /// The note's lane.
        lane: Lane,
        /// The note's index in its lane of [`Chart::lanes`].
        note: usize,
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

/// Kinds of deadline; at equal times they are processed in this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum DeadlineKind {
    /// A pending scratch release (input-dependent, not stored per lane).
    /// It stands for the release the player made by stopping, so it comes
    /// first among deadlines, as a release input would.
    ScratchRelease,
    /// The end of a long note not yet finished. Before misses, so the notes
    /// of a lane are judged in note order even when a scaled late window
    /// puts a later note's miss at this µs.
    LnEnd,
    /// The late edge of an unjudged note's miss window.
    Miss,
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

/// A long note whose head was pressed before its end and graded GOOD or
/// better, held now and not yet judged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Held {
    /// Its position in the lane's targets.
    target: usize,
    /// The head's judgment.
    judgment: Judgment,
    /// The head's press time minus note time.
    deviation: TimeUs,
}

/// What a press's candidate scan found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Scan {
    /// The note the press takes, as its deviation (press − note) and its
    /// position in the lane's targets.
    primary: Option<(i128, usize)>,
    /// The target an empty POOR is against: the nearest one, judged or
    /// not, inside the empty POOR range, ties to the earlier one. Sought
    /// only while no primary is found.
    against: Option<usize>,
    /// The lane's latest target strictly before the press, judged or not.
    before: Option<usize>,
}

/// One lane's notes to judge and its deadlines.
#[derive(Clone, Debug, Default)]
struct LaneState {
    /// Indices into the chart lane of the judgeable notes (taps and long
    /// notes), in time order.
    targets: Vec<usize>,
    /// Whether each target's head is taken: judged, or held as [`Held`].
    /// It drives assignment and the cursor; a long note is scored once it
    /// is taken and it is not `holding`.
    judged: Vec<bool>,
    /// The long note held and not yet judged.
    holding: Option<Held>,
    /// The long note whose head press started or decided it before its
    /// end, while its lane stays held and its end has not passed: the
    /// visual hold, reported by [`JudgeEventKind::LnHoldEnded`] when a
    /// release or the end ends it. It scores nothing.
    visual: Option<usize>,
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
    /// condition says so: a miss deadline of a taken note, and an LN end of
    /// a taken long note neither held nor visually held. A mine is never
    /// skipped.
    fn head(&mut self) -> Option<Deadline> {
        while let Some(&d) = self.deadlines.get(self.next) {
            #[expect(
                clippy::indexing_slicing,
                reason = "miss and LN end deadlines hold a position in `targets`, which `judged` matches in length"
            )]
            let skip = match d.kind {
                DeadlineKind::Miss => self.judged[d.index],
                DeadlineKind::LnEnd => {
                    self.judged[d.index]
                        && self.holding.map(|h| h.target) != Some(d.index)
                        && self.visual != Some(d.index)
                }
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

/// Where a section's input is judged (design.md "Section Evaluation").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Bounds {
    /// The lead-in start: earlier input feeds input conversion only.
    lead_in: TimeUs,
    /// The section start: earlier presses are judged only when they judge a
    /// target.
    start: TimeUs,
}

/// How an input at one time is taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Acceptance {
    /// Judged as in a whole-chart play.
    Judged,
    /// In a section's lead-in: a press is judged only when it judges a
    /// target, and is otherwise ignored.
    LeadIn,
    /// Before a section's lead-in or after its final deadline: input
    /// conversion only.
    ConversionOnly,
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
    /// Each lane class's table after the window policy.
    tables: PerLaneClass<LaneTable>,
    lanes: [LaneState; LANE_COUNT],
    conversion: ConversionState,
    inputs: VecDeque<(TimeUs, PhysicalInputEvent)>,
    /// The ring's fixed capacity.
    input_capacity: usize,
    /// The watermark last advanced to: everything at or before it is final.
    finalized: Option<TimeUs>,
    /// The latest deadline of the chart, or of the section's targets;
    /// evaluation is complete once the watermark reaches it.
    final_deadline: TimeUs,
    /// The section's bounds; `None` evaluates the whole chart.
    section: Option<Bounds>,
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
    /// Prepares evaluation: derives each lane class's windows, LN-end edges
    /// and empty POOR range from the chart's rank by the preset's window
    /// policy, builds each lane's deadlines from its own
    /// class's table and the final deadline from them, computes the gauge
    /// increment, the default `#TOTAL` and each gauge's correction, and sets
    /// up input conversion from `start`.
    /// `section`, if given, restricts the targets, the input judged and the
    /// final deadline to it and sets the initial gauges ([`Section`]); the
    /// gauge increment, the default `#TOTAL` and the corrections still count
    /// the whole chart's judgeable notes.
    /// `input_capacity` sizes the ring of inputs pushed but not yet
    /// processed.
    ///
    /// # Errors
    ///
    /// An invalid preset (including an implementation version this build
    /// does not run), a rank whose scale is unusable, a long-note mode the
    /// preset cannot judge, a note time or long-note end outside `±2^32` µs,
    /// a chart without judgeable notes, an unusable section, or a source
    /// binding one control twice.
    pub fn new(
        chart: &'a Chart,
        rule: &'a RulePreset,
        device: &'a DeviceProfile,
        start: &InputStart,
        section: Option<&Section>,
        input_capacity: usize,
    ) -> Result<Self, EvalError> {
        rule.validate().map_err(EvalError::InvalidPreset)?;
        let bounds = match section {
            None => None,
            Some(s) => {
                let invalid = |field| EvalError::InvalidSection { field };
                if s.end < s.start {
                    return Err(invalid("range"));
                }
                if s.lead_in.0 < 0 {
                    return Err(invalid("lead_in"));
                }
                if !Gauges::accepts(&rule.gauges, &s.initial_gauges) {
                    return Err(invalid("initial_gauges"));
                }
                let lead_in = s.start.checked_sub(s.lead_in).ok_or(invalid("lead_in"))?;
                Some(Bounds {
                    lead_in,
                    start: s.start,
                })
            }
        };
        // Without a section, every note is a target.
        let in_section = |n: &bms_model::Note| section.is_none_or(|s| s.is_target(n));
        // The preset's header policy reads the chart's declarations into
        // one rank and one #TOTAL (design.md "Evaluation Stages").
        let headers = rule.headers.read(&chart.meta.judgment_headers);
        // The window policy derives every lane class's table from that rank
        // (design.md "Evaluation Stages"); derived edges may tie.
        let tables = rule
            .window_policy
            .prepare(&rule.tables, headers.rank)
            .ok_or(EvalError::InvalidRank)?;
        // With every edge within 2^32 µs (`MAX_WINDOW_EDGE`), times within
        // ±2^32 µs keep a press deviation and a release's distance from its
        // long note's end inside `i64`.
        let in_domain = |t: TimeUs| (-DOMAIN..=DOMAIN).contains(&t.0);
        if chart
            .lanes
            .iter()
            .flatten()
            .any(|n| !in_domain(n.time) || !in_domain(n.end()))
        {
            return Err(EvalError::TimeOverflow);
        }
        match (rule.ln_mode.default, chart.meta.ln_mode) {
            (LnMode::Ln, None | Some(ChartLnMode::Ln)) => {}
            (LnMode::Ln, Some(mode)) => return Err(EvalError::UnsupportedLnMode(mode)),
        }
        let judgeable =
            |n: &bms_model::Note| matches!(n.kind, NoteKind::Tap | NoteKind::Long { .. });
        let mut lanes: [LaneState; LANE_COUNT] = Default::default();
        for (lane, (state, notes)) in lanes.iter_mut().zip(&chart.lanes).enumerate() {
            let judgeable = |n: &bms_model::Note| judgeable(n) && in_section(n);
            // Each lane's misses by its own class's late BAD edge.
            let miss_after = match rule.miss {
                MissRule::PoorAfterLateBadOrOvertaken => {
                    tables.of_lane(lane_of(lane)).windows.bad.late
                }
            };
            // Reserved at its exact size, which the recording policy counts.
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
                if matches!(note.kind, NoteKind::Mine { .. }) && in_section(note) {
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
        // The whole chart's notes, also for a section, so a note moves the
        // gauges as far as in a full play.
        let notes = match rule.denominator {
            Denominator::TapsAndLnHeads => chart
                .lanes
                .iter()
                .flatten()
                .filter(|n| judgeable(n))
                .count(),
        };
        if notes == 0 {
            return Err(EvalError::NoJudgeableNotes);
        }
        // A whole chart has a judgeable note, so a deadline; a section
        // without targets or mines is complete at its start.
        let final_deadline = lanes
            .iter()
            .filter_map(|l| l.deadlines.last().map(|d| d.time))
            .max()
            .or(section.map(|s| s.start))
            .unwrap_or(TimeUs(0));
        // The #TOTAL read, else the preset's default for the whole chart's
        // notes; it sets the increment and each gauge's correction.
        let total_milli = match rule.total_mode {
            TotalMode::ScalesIncrease => headers
                .total_milli
                .unwrap_or_else(|| rule.default_total.milli(notes)),
        };
        Ok(Self {
            chart,
            rule,
            device,
            tables,
            lanes,
            conversion: ConversionState::init(device, start)?,
            inputs: VecDeque::with_capacity(input_capacity),
            input_capacity,
            finalized: None,
            final_deadline,
            section: bounds,
            ended: false,
            gauges: Gauges::new(
                &rule.gauges,
                total_milli,
                notes,
                section.map(|s| s.initial_gauges.as_slice()),
            ),
            seq: 0,
            tally: Tally::new(),
        })
    }

    /// The table `lane` is judged by after the window policy: its class's
    /// windows, LN-end edges and empty POOR range.
    #[must_use]
    pub fn table(&self, lane: Lane) -> &LaneTable {
        self.tables.of_lane(lane)
    }

    /// The table of the lane at `index`, below [`LANE_COUNT`].
    fn lane_table(&self, index: usize) -> &LaneTable {
        self.tables.of_lane(lane_of(index))
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

    /// The latest deadline of the chart, or of the section's targets: tap
    /// and LN head misses (each by its lane class's late BAD edge), LN ends
    /// and mines; for a section with none, its start. Completion, a
    /// what-if's run and a section's recording window all read this one
    /// value (design.md "Evaluation Stages"); none recomputes it.
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
    /// Each judgeable note is judged once (1 event plus one per gauge); a
    /// long note is held at most once (1) and its visual hold, set only by
    /// the press that takes its head, ends at most once (1), at a release
    /// or at its end. Each input causes at most one press (an axis
    /// reversing releases first, which only judges long notes and ends
    /// visual holds), which plays a sound and may be an empty POOR (1 plus
    /// one per gauge). A scratch release deadline only judges long notes
    /// and ends visual holds. Each mine hits at most once (1 plus one per
    /// gauge). The song ends once.
    #[must_use]
    pub fn max_events(&self, inputs: usize) -> usize {
        let g = self.gauges.states().len();
        let (notes, mines) = self.targets_and_mines();
        notes
            .saturating_mul(3 + g)
            .saturating_add(inputs.saturating_mul(2 + g))
            .saturating_add(mines.saturating_mul(1 + g))
            .saturating_add(1)
    }

    /// The judgeable notes and the mines evaluated.
    fn targets_and_mines(&self) -> (usize, usize) {
        let notes = self.lanes.iter().map(|l| l.targets.len()).sum();
        let mines = self
            .lanes
            .iter()
            .flat_map(|l| &l.deadlines)
            .filter(|d| d.kind == DeadlineKind::Mine)
            .count();
        (notes, mines)
    }

    /// The allowance for the evaluator's own tables:
    /// [`TABLE_BYTES_PER_OBJECT`] per object of every lane of the chart, a
    /// section's evaluation included.
    #[must_use]
    pub fn table_bytes(&self) -> usize {
        let objects: usize = self.chart.lanes.iter().map(Vec::len).sum();
        objects.saturating_mul(TABLE_BYTES_PER_OBJECT)
    }

    /// The most bytes [`evaluate`](crate::evaluate) holds at its peak for a
    /// record of `inputs` inputs and `held` controls held at its start, under
    /// this evaluator's chart and preset,
    /// the caller's record included (design.md "Recording Capacity"): the
    /// input four times (the record, the copy evaluated, the staging by
    /// source and the merge) and the input ring; the event output at
    /// [`max_events`](Self::max_events); each gauge's history at twice the
    /// most points it can hold (one per judgment, empty POOR and mine hit),
    /// never under a vector's least capacity, with its vector, its result
    /// and [`GAUGE_BYTES`], and one history more for the copy a growth
    /// makes; the [tables](Self::table_bytes); [`BATCH_SMALL_BYTES`]; and
    /// the held-input snapshot twice (the record's and the copy evaluated).
    /// Plain arithmetic, saturating.
    #[must_use]
    pub fn batch_bytes(&self, inputs: usize, held: usize) -> usize {
        let g = self.gauges.states().len();
        let (notes, mines) = self.targets_and_mines();
        let points = notes.saturating_add(inputs).saturating_add(mines);
        let history = points
            .saturating_mul(2)
            .max(MIN_HISTORY_CAPACITY)
            .saturating_mul(size_of::<GaugePoint>());
        let per_gauge = history
            .saturating_add(size_of::<Vec<GaugePoint>>())
            .saturating_add(size_of::<GaugeResult>())
            .saturating_add(GAUGE_BYTES);
        let snapshot = held.saturating_mul(2 * HELD_BYTES);
        inputs
            .saturating_mul(4 * INPUT_BYTES + RING_SLOT_BYTES)
            .saturating_add(self.max_events(inputs).saturating_mul(EVENT_BYTES))
            .saturating_add(g.saturating_mul(per_gauge))
            .saturating_add(history)
            .saturating_add(self.table_bytes())
            .saturating_add(BATCH_SMALL_BYTES)
            .saturating_add(snapshot)
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
                // A missed head is the long note's one judgment.
                self.judge(lane, d.index, Judgment::Poor, None, d.time, None, out);
            }
            DeadlineKind::LnEnd => {
                // An unjudged head still waits for its press or its miss. A
                // taken one is still held here, since a release would have
                // ended both holds: the visual hold ends, and a long note
                // held through its end keeps its head's judgment.
                let visual = state.visual == Some(d.index);
                let held = state.holding.filter(|h| h.target == d.index);
                let note = state.targets[d.index];
                if visual {
                    state.visual = None;
                }
                if held.is_some() {
                    state.holding = None;
                }
                if visual {
                    let kind = JudgeEventKind::LnHoldEnded {
                        lane: lane_of(lane),
                        note,
                    };
                    self.emit(d.time, None, kind, out);
                }
                if let Some(held) = held {
                    self.score(
                        lane,
                        d.index,
                        held.judgment,
                        Some(held.deviation),
                        None,
                        d.time,
                        None,
                        out,
                    );
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
        let acceptance = self.acceptance(t);
        if acceptance == Acceptance::ConversionOnly {
            return;
        }
        let lead_in = acceptance == Acceptance::LeadIn;
        for &edge in edges.as_slice() {
            match edge {
                LaneEdge::Press(lane) => self.press(lane.index(), t, ev.received, lead_in, out),
                LaneEdge::Release(lane) => self.release(lane.index(), t, Some(ev.received), out),
            }
        }
    }

    /// How an input at evaluation time `t` is taken (design.md "Section
    /// Evaluation").
    fn acceptance(&self, t: TimeUs) -> Acceptance {
        match self.section {
            None => Acceptance::Judged,
            Some(b) if t < b.lead_in || t > self.final_deadline => Acceptance::ConversionOnly,
            Some(b) if t < b.start => Acceptance::LeadIn,
            Some(_) => Acceptance::Judged,
        }
    }

    /// The candidate scan of a press on `lane` at `t` (design.md "Evaluation
    /// Stages", press resolution): one pass over the lane's targets, judged
    /// and unjudged, located by time inside the envelope of the class's BAD
    /// window and empty POOR range, each with its own bounds, not started at
    /// the assignment cursor. It picks the primary among the unjudged
    /// targets inside BAD by the assignment policy, and finds whether a
    /// target lies inside the empty POOR range and names the nearest. It
    /// also names the lane's latest target before the press: every target
    /// before the press at or after the located start is visited, and the
    /// one just before the start lies before the press, since the envelope
    /// edges are magnitudes.
    fn scan(&self, lane: usize, t: TimeUs, table: &LaneTable) -> Scan {
        let (bad, good) = (table.windows.bad, table.windows.good);
        let range = table.empty_poor;
        #[expect(
            clippy::indexing_slicing,
            reason = "`lane` comes from `Lane::index`, below LANE_COUNT"
        )]
        let (notes, state) = (&self.chart.lanes[lane], &self.lanes[lane]);
        let at = i128::from(t.0);
        let late = i128::from(bad.late.0.max(range.behind.0));
        #[expect(
            clippy::indexing_slicing,
            reason = "`targets` holds indices into the lane's notes"
        )]
        let start = state
            .targets
            .partition_point(|&i| i128::from(notes[i].time.0) < at - late);
        let mut primary: Option<(i128, usize)> = None;
        let mut against: Option<(i128, usize)> = None;
        let mut before = start.checked_sub(1);
        // Each slice iterator skips to `start` directly, so the press costs
        // no walk over the lane's earlier targets; `k` stays the position
        // in `targets`.
        let targets = state.targets.iter().skip(start);
        let judged = state.judged.iter().skip(start);
        for (k, (&i, &judged)) in (start..).zip(targets.zip(judged)) {
            #[expect(
                clippy::indexing_slicing,
                reason = "`targets` holds indices into the lane's notes"
            )]
            let d = at - i128::from(notes[i].time.0);
            if d > 0 {
                before = Some(k);
            }
            let past_bad = d < -i128::from(bad.early.0);
            // Past the BAD window nothing can become the primary, and the
            // empty POOR range matters only without one.
            if past_bad && primary.is_some() {
                break;
            }
            let near = -i128::from(range.ahead.0) <= d && d <= i128::from(range.behind.0);
            // Strictly nearer only, so a tie keeps the earlier note.
            if near && against.is_none_or(|(c, _)| d.abs() < c.abs()) {
                against = Some((d, k));
            }
            // Later notes lie farther ahead, so the first one past BAD
            // also ends the search for the range's nearest.
            if past_bad && (against.is_some() || d < -i128::from(range.ahead.0)) {
                break;
            }
            // Every unjudged note inside BAD is eligible, taps and LN heads
            // alike.
            if judged || past_bad || d > i128::from(bad.late.0) {
                continue;
            }
            let replaces = match (self.rule.assignment, primary) {
                (_, None) => true,
                // Strictly nearer only, so a tie keeps the earlier note.
                (Assignment::NearestTiesEarlier, Some((c, _))) => d.abs() < c.abs(),
                // A later note takes over only once the press is past the
                // taken note's late GOOD edge, and when it is not before
                // the later note's early GOOD edge.
                (Assignment::ComboPriority, Some((c, _))) => {
                    c > i128::from(good.late.0) && d >= -i128::from(good.early.0)
                }
            };
            if replaces {
                primary = Some((d, k));
            }
        }
        Scan {
            primary,
            against: against.map(|(_, k)| k),
            before,
        }
    }

    /// A press edge on `lane` at evaluation time `t`. In a section's
    /// lead-in (`lead_in`), a press that judges no target is ignored.
    fn press(
        &mut self,
        lane: usize,
        t: TimeUs,
        origin: TimeUs,
        lead_in: bool,
        out: &mut Vec<JudgeEvent>,
    ) {
        let table = *self.lane_table(lane);
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
        let Scan {
            primary: best,
            against,
            before,
        } = self.scan(lane, t, &table);
        // A lead-in press judges only inside its lane's first unjudged
        // target's early window: no sound, no empty POOR otherwise.
        if lead_in && best.is_none() {
            return;
        }
        // The sound (design.md "The Only Output Is the Judgment Event
        // Stream"): the note taken, else the one the empty POOR is against,
        // else the lane's latest note before the press, else its first.
        let empty_poor = best.is_none() && against.is_some();
        let target = best
            .map(|(_, k)| k)
            .or(against)
            .or(before)
            .or((!state.targets.is_empty()).then_some(0));
        let rule = self.rule.empty_poor;
        #[expect(
            clippy::indexing_slicing,
            reason = "`target` is a position the scan found, or 0 of non-empty `targets`, all below `targets.len()`"
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
        // Emission order, all at the press's time (design.md "Evaluation
        // Stages"): the sound above, the passed notes' POORs in note order,
        // then the primary or the empty POOR.
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
            let (index, note) = {
                let i = self.lanes[lane].targets[k];
                (i, self.chart.lanes[lane][i])
            };
            // Within the BAD window, at most 2^32 µs, so it fits in i64.
            let deviation = TimeUs(i64::try_from(deviation).unwrap_or(i64::MAX));
            let judgment = judgment_of(&table.windows, deviation);
            let before_end = match note.kind {
                NoteKind::Long { end, .. } => match self.rule.ln_mode.late_head {
                    // A head pressed at or after its end decides the long
                    // note at the press; the following release is ignored.
                    LateHeadRule::CompletesAtPress => t < end,
                },
                NoteKind::Tap | NoteKind::Mine { .. } => false,
            };
            if !before_end {
                self.judge(lane, k, judgment, Some(deviation), t, Some(origin), out);
                return;
            }
            #[expect(
                clippy::indexing_slicing,
                reason = "`lane` comes from `Lane::index`, below LANE_COUNT"
            )]
            let state = &mut self.lanes[lane];
            // A press edge needs the lane released, and every release ends
            // both holds, so neither is set here.
            state.visual = Some(k);
            if judgment == Judgment::Bad {
                // No release can better a BAD head: decided at the press.
                self.judge(lane, k, judgment, Some(deviation), t, Some(origin), out);
                return;
            }
            state.mark_judged(k);
            state.holding = Some(Held {
                target: k,
                judgment,
                deviation,
            });
            let kind = JudgeEventKind::LnHeld {
                lane: lane_of(lane),
                note: index,
                judgment,
                deviation,
            };
            self.emit(t, Some(origin), kind, out);
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

    /// A release edge on `lane` at evaluation time `t`: ends the visual
    /// hold, reporting it, and judges the
    /// held long note, the worse of its head's judgment and the release's
    /// by the LN-end edges when before its end. `origin` is `None` for a
    /// scratch release deadline.
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
        let visual = state.visual.take();
        let held = state.holding.take();
        #[expect(
            clippy::indexing_slicing,
            reason = "the visual hold is a position in `targets`"
        )]
        let visual = visual.map(|k| state.targets[k]);
        if let Some(note) = visual {
            let kind = JudgeEventKind::LnHoldEnded {
                lane: lane_of(lane),
                note,
            };
            self.emit(t, origin, kind, out);
        }
        let Some(held) = held else {
            return;
        };
        #[expect(
            clippy::indexing_slicing,
            reason = "`lane` is below LANE_COUNT, `held.target` is a position in `targets`, and `targets` indexes the lane's notes"
        )]
        let end = self.chart.lanes[lane][self.lanes[lane].targets[held.target]].end();
        // A held long note is judged by its end deadline, which comes after
        // every input of its µs, so `t` is at most `end`. It is at least
        // the head minus the BAD window, so with note times and LN ends in
        // ±2^32 µs and edges at most 2^32 µs, −3·2^32 ≤ t − end ≤ 0.
        let before = TimeUs(t.0 - end.0);
        let judgment = held
            .judgment
            .max(self.lane_table(lane).ln_end.judgment_of(before));
        self.score(
            lane,
            held.target,
            judgment,
            Some(held.deviation),
            Some(before),
            t,
            origin,
            out,
        );
    }

    /// Takes and scores a target whose judgment is decided now: a tap, a
    /// missed head, or a long note decided at its head's press.
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
        self.lanes[lane].mark_judged(target);
        self.score(lane, target, judgment, deviation, None, time, origin, out);
    }

    /// Scores a taken target: counts, EX score, combo, BP, its
    /// [`JudgeEventKind::Judged`] and the gauges.
    #[allow(clippy::too_many_arguments)]
    fn score(
        &mut self,
        lane: usize,
        target: usize,
        judgment: Judgment,
        deviation: Option<TimeUs>,
        release: Option<TimeUs>,
        time: TimeUs,
        origin: Option<TimeUs>,
        out: &mut Vec<JudgeEvent>,
    ) {
        #[expect(
            clippy::indexing_slicing,
            reason = "`lane` comes from `Lane::index`, below LANE_COUNT, and callers pass a position in `targets`"
        )]
        let note = self.lanes[lane].targets[target];
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
            release,
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
