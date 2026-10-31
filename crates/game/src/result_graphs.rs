//! What the result screen draws under a view's summary (design.md "Play
//! Lifecycle", "Gauges and Session Policy"): the listed gauges' histories
//! condensed to [`GRAPH_COLUMNS`] columns over the play's time, and the
//! distribution of press deviations. Fixed in size whatever the play, so
//! a cached summary stays small; built from a batch evaluation, never
//! during play.

use bms_model::TimeUs;
use eval::{Evaluation, GaugeResult, JudgeEvent};
use replay::verify::Coverage;
use rules::{GaugeKind, GaugeValue, Judgment, PerJudgment};

use crate::result_screen::SHOWN_GAUGES;
use crate::results::press_deviations;

/// The columns a gauge's history is condensed to.
pub const GRAPH_COLUMNS: usize = 288;

/// The width of a deviation bin: 2 ms.
pub const TIMING_BIN_US: i64 = 2_000;

/// The deviation bins, from −64 ms up to +64 ms.
pub const TIMING_BINS: usize = 64;

/// The deviation the first bin starts at.
const TIMING_LOW_US: i64 = -(TIMING_BIN_US * TIMING_BINS as i64 / 2);

/// The time axis of one result screen: every view is condensed on it, so
/// a time lands in the same column under every preset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Axis {
    /// The play's start: chart time 0, or a practice round's section start.
    pub origin: TimeUs,
    /// The as-played final deadline.
    pub end: TimeUs,
}

impl Axis {
    /// The column `t` falls in: before the origin the first, at or after
    /// the end the last.
    #[must_use]
    pub fn column(self, t: TimeUs) -> u16 {
        let span = i128::from(self.end.0.saturating_sub(self.origin.0).max(0));
        let at = i128::from(t.0).saturating_sub(i128::from(self.origin.0));
        let n = GRAPH_COLUMNS as i128;
        let c = (at.clamp(0, span) * n / span.max(1)).min(n - 1);
        u16::try_from(c).unwrap_or(u16::MAX)
    }

    /// The columns the evaluation covers, from the first: all of them, or,
    /// for a play that ended midway at `w`, up to `w`'s column.
    #[must_use]
    pub fn reached(self, coverage: Coverage) -> u16 {
        match coverage {
            Coverage::EndedMidway(w) if w < self.origin => 0,
            Coverage::EndedMidway(w) => self.column(w) + 1,
            Coverage::Recorded | Coverage::NoInputAfter(_) => GRAPH_COLUMNS as u16,
        }
    }
}

/// One column of a gauge's graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GaugeColumn {
    /// The lowest value in the column: of the value carried into it and
    /// of every change in it.
    pub low: GaugeValue,
    /// The highest such value.
    pub high: GaugeValue,
    /// The value at the column's end, carried into the next.
    pub last: GaugeValue,
}

impl GaugeColumn {
    const AT_ZERO: Self = Self {
        low: GaugeValue::EMPTY,
        high: GaugeValue::EMPTY,
        last: GaugeValue::EMPTY,
    };
}

/// The line a gauge's graph marks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GaugeMark {
    /// Cleared at or above this value at the end.
    EndThreshold {
        /// The clear threshold.
        clear: GaugeValue,
    },
    /// Fails below this value.
    Survival {
        /// The failure threshold.
        fails_below: GaugeValue,
    },
}

/// One listed gauge's graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GaugeTrace {
    /// The gauge's index in the viewed preset.
    pub gauge: u16,
    /// Its history over the axis.
    pub columns: [GaugeColumn; GRAPH_COLUMNS],
    /// Its clear or failure line.
    pub mark: GaugeMark,
    /// The column a survival gauge failed in, if it did.
    pub failed: Option<u16>,
}

impl GaugeTrace {
    const EMPTY: Self = Self {
        gauge: 0,
        columns: [GaugeColumn::AT_ZERO; GRAPH_COLUMNS],
        mark: GaugeMark::EndThreshold {
            clear: GaugeValue::EMPTY,
        },
        failed: None,
    };
}

/// Press deviations by judgment and in 2 ms bins (design.md "Play
/// Lifecycle"): the presses the recommended offset reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    /// Presses before their note, per judgment.
    pub early: PerJudgment<u32>,
    /// Presses after their note, per judgment.
    pub late: PerJudgment<u32>,
    /// Presses exactly on their note.
    pub exact: u32,
    /// Every press by deviation: bin `i` holds
    /// `[-64 ms + 2i ms, -64 ms + 2(i + 1) ms)`.
    pub bins: [u32; TIMING_BINS],
    /// Presses earlier than the first bin.
    pub earlier: u32,
    /// Presses at or later than the last bin's end.
    pub later: u32,
}

const ZERO: PerJudgment<u32> = PerJudgment {
    perfect: 0,
    great: 0,
    good: 0,
    bad: 0,
    poor: 0,
};

impl Timing {
    /// No press.
    pub const NONE: Self = Self {
        early: ZERO,
        late: ZERO,
        exact: 0,
        bins: [0; TIMING_BINS],
        earlier: 0,
        later: 0,
    };

    /// The presses of `events`.
    #[must_use]
    pub fn of(events: &[JudgeEvent]) -> Self {
        let mut t = Self::NONE;
        for (j, d) in press_deviations(events) {
            let side = match d.0.signum() {
                -1 => Some(&mut t.early),
                1 => Some(&mut t.late),
                _ => None,
            };
            match side {
                Some(side) => {
                    let n = match j {
                        Judgment::Perfect => &mut side.perfect,
                        Judgment::Great => &mut side.great,
                        Judgment::Good => &mut side.good,
                        Judgment::Bad => &mut side.bad,
                        Judgment::Poor => &mut side.poor,
                    };
                    *n = n.saturating_add(1);
                }
                None => t.exact = t.exact.saturating_add(1),
            }
            let bin = d.0.saturating_sub(TIMING_LOW_US).div_euclid(TIMING_BIN_US);
            let slot = match usize::try_from(bin) {
                Err(_) => &mut t.earlier,
                Ok(i) => t.bins.get_mut(i).unwrap_or(&mut t.later),
            };
            *slot = slot.saturating_add(1);
        }
        t
    }

    /// How many presses it counts.
    #[must_use]
    pub fn samples(&self) -> u64 {
        self.bins
            .iter()
            .chain([&self.earlier, &self.later])
            .map(|&n| u64::from(n))
            .sum()
    }
}

/// What a view draws under its text: the listed gauges' graphs and the
/// deviations. A fixed size whatever the play.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResultGraphs {
    /// The listed gauges' graphs, in the list's order; as many as listed.
    pub traces: [Option<GaugeTrace>; SHOWN_GAUGES],
    /// The slot of the emphasized gauge, drawn last.
    pub emphasized: Option<u8>,
    /// The top of the value scale: the largest maximum among the listed
    /// gauges, at least 100 %.
    pub top: GaugeValue,
    /// The columns the evaluation covers; none from here on is drawn.
    pub reached: u16,
    /// The column a what-if's recorded input ends in.
    pub no_input_from: Option<u16>,
    /// The press deviations.
    pub timing: Timing,
}

impl ResultGraphs {
    /// Nothing listed, no press.
    pub const EMPTY: Self = Self {
        traces: [None; SHOWN_GAUGES],
        emphasized: None,
        top: GaugeValue::FULL,
        reached: GRAPH_COLUMNS as u16,
        no_input_from: None,
        timing: Timing::NONE,
    };

    /// The graphs of `e` on `axis`, for the gauges at `listed` (indices in
    /// `rules`, the preset `e` was evaluated under), `emphasized` among
    /// them.
    pub(crate) fn of(
        e: &Evaluation,
        rules: &[rules::GaugeRule],
        listed: &[usize],
        emphasized: Option<usize>,
        axis: Axis,
        coverage: Coverage,
    ) -> Self {
        let mut g = Self::EMPTY;
        g.reached = axis.reached(coverage);
        g.no_input_from = match coverage {
            Coverage::NoInputAfter(t) => Some(axis.column(t)),
            Coverage::Recorded | Coverage::EndedMidway(_) => None,
        };
        g.timing = Timing::of(&e.events);
        let mut top = GaugeValue::FULL;
        for (slot, (trace, &i)) in g.traces.iter_mut().zip(listed).enumerate() {
            let (Some(rule), Some(result)) = (rules.get(i), e.gauges.get(i)) else {
                continue;
            };
            top = top.max(rule.max);
            let mut t = GaugeTrace::EMPTY;
            t.gauge = u16::try_from(i).unwrap_or(u16::MAX);
            condense(result, axis, &mut t.columns);
            t.mark = match rule.kind {
                GaugeKind::EndThreshold { clear } => GaugeMark::EndThreshold { clear },
                GaugeKind::Survival { fails_below } => GaugeMark::Survival { fails_below },
            };
            t.failed = result.failed_at.map(|f| axis.column(f));
            *trace = Some(t);
            if emphasized == Some(i) {
                g.emphasized = u8::try_from(slot).ok();
            }
        }
        g.top = top;
        g
    }
}

/// Condenses `g`'s history into `columns` on `axis`: each column holds the
/// value carried into it, the lowest and highest of it and every change in
/// it, and the value it leaves; the first is carried in from the start.
pub fn condense(g: &GaugeResult, axis: Axis, columns: &mut [GaugeColumn; GRAPH_COLUMNS]) {
    let mut points = g.history.iter().peekable();
    let mut carried = g.start;
    for (c, col) in (0_u16..).zip(columns.iter_mut()) {
        let (mut low, mut high) = (carried, carried);
        while let Some(p) = points.next_if(|p| axis.column(p.time) <= c) {
            low = low.min(p.value);
            high = high.max(p.value);
            carried = p.value;
        }
        *col = GaugeColumn {
            low,
            high,
            last: carried,
        };
    }
}

/// The gauges a view lists, by index in the preset of `count` gauges: all
/// of them up to [`SHOWN_GAUGES`]; with more, the first five, and the
/// emphasized one in the fifth slot when it is not among them. The rest
/// are counted as "more".
#[must_use]
pub fn listed(count: usize, emphasized: Option<usize>) -> ([usize; SHOWN_GAUGES], usize) {
    let mut list: [usize; SHOWN_GAUGES] = std::array::from_fn(|i| i);
    if count <= SHOWN_GAUGES {
        return (list, count);
    }
    let n = SHOWN_GAUGES - 1;
    if let Some(e) = emphasized.filter(|&e| e >= n)
        && let Some(last) = list.get_mut(n - 1)
    {
        *last = e;
    }
    (list, n)
}
