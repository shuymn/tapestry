//! The practice screen's model (design.md "Mid-Chart Start and Section
//! Repeat", Setup): the section by measure, the speed, the lead-in, the
//! displayed gauge's initial value and repeat, chosen with the menu keys.
//!
//! It is built once when the screen opens, from the chart loaded at 1×,
//! and kept while the screen stays open, so its measures are always 1×
//! times, whatever speed the chart is played at.

use bms_model::{Chart, Position, Speed, TimeUs};
use rules::{GaugeRule, GaugeValue, RulePreset};

use crate::Loaded;
use crate::practice::{LEAD_IN, Practice, chart_end};

/// The shortest lead-in the screen sets.
pub const MIN_LEAD_IN: TimeUs = TimeUs(1_000_000);
/// The longest lead-in the screen sets.
pub const MAX_LEAD_IN: TimeUs = TimeUs(10_000_000);
/// How far one step moves the lead-in.
pub const LEAD_IN_STEP: TimeUs = TimeUs(500_000);

/// A row of the practice screen, top down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PracticeRow {
    /// The section's first measure.
    From,
    /// The section's last measure.
    To,
    /// The speed.
    Speed,
    /// The lead-in.
    LeadIn,
    /// The displayed gauge's initial value.
    Gauge,
    /// Whether rounds repeat.
    Repeat,
    /// Start the rounds.
    Start,
}

impl PracticeRow {
    /// Every row, top down.
    pub const ALL: [Self; 7] = [
        Self::From,
        Self::To,
        Self::Speed,
        Self::LeadIn,
        Self::Gauge,
        Self::Repeat,
        Self::Start,
    ];

    fn index(self) -> usize {
        Self::ALL.iter().position(|&r| r == self).unwrap_or(0)
    }
}

/// A menu input on the practice screen: the arrows, confirm and back, from
/// the keyboard or the controller (design.md "Input", "Menu input").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PracticeInput {
    Up,
    Down,
    Left,
    Right,
    Confirm,
    Back,
}

/// What an input did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PracticeActed {
    /// Nothing changed.
    Nothing,
    /// What the screen shows changed.
    Changed,
    /// Start the rounds.
    Start,
    /// Leave the screen for selection.
    Back,
}

/// The practice screen's state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PracticeScreen {
    /// The 1× start of each measure.
    measures: Vec<TimeUs>,
    /// The section's first measure, by index.
    from: usize,
    /// Just past the section's last measure, by index: the number of
    /// measures for the chart's end.
    to: usize,
    speed: Speed,
    lead_in: TimeUs,
    /// The displayed gauge, by index into the preset's gauges.
    gauge: usize,
    /// Its rule, which bounds the value; `None` for a preset without one.
    rule: Option<GaugeRule>,
    /// Its initial value.
    value: GaugeValue,
    repeat: bool,
    row: PracticeRow,
    editing: bool,
    notice: Option<String>,
}

/// The 1× start of each measure of `chart`: for each bar line, the first
/// microsecond whose position reaches it, or the earliest note at or past
/// it when that comes sooner (a note on the bar line rounds to a time
/// before that microsecond, or sits at the start of a STOP). A chart
/// without bar lines is one measure from 0.
#[must_use]
pub fn measures(chart: &Chart) -> Vec<TimeUs> {
    if chart.bar_lines.is_empty() {
        return vec![TimeUs(0)];
    }
    chart
        .bar_lines
        .iter()
        .map(|&p| {
            let note = chart
                .lanes
                .iter()
                .filter_map(|lane| {
                    let i = lane.partition_point(|n| n.pos < p);
                    lane.get(i).map(|n| n.time)
                })
                .min();
            let reached = reached(chart, p);
            note.map_or(reached, |n| n.min(reached))
        })
        .collect()
}

/// The first microsecond whose position is at or past `p`, from 0.
fn reached(chart: &Chart, p: Position) -> TimeUs {
    let (mut lo, mut hi) = (0_i64, bms_model::DOMAIN);
    if chart.scroll.position_at(TimeUs(hi)) < p {
        return TimeUs(hi);
    }
    // The position never falls with time.
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if chart.scroll.position_at(TimeUs(mid)) >= p {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    TimeUs(lo)
}

impl PracticeScreen {
    /// The screen for `chart` loaded at 1× under `preset`, showing gauge
    /// `displayed` (clamped to the preset's gauges): the whole chart at
    /// 1×, the default lead-in, the gauge's start value, repeat on.
    #[must_use]
    pub fn new(chart: &Chart, preset: &RulePreset, displayed: usize) -> Self {
        let measures = measures(chart);
        let gauge = displayed.min(preset.gauges.len().saturating_sub(1));
        let rule = preset.gauges.get(gauge).cloned();
        let value = rule.as_ref().map_or(GaugeValue::EMPTY, |r| r.start);
        Self {
            to: measures.len(),
            measures,
            from: 0,
            speed: Speed::NORMAL,
            lead_in: LEAD_IN,
            gauge,
            rule,
            value,
            repeat: true,
            row: PracticeRow::From,
            editing: false,
            notice: None,
        }
    }

    /// The 1× start of each measure.
    #[must_use]
    pub fn measures(&self) -> &[TimeUs] {
        &self.measures
    }

    /// The section's first measure, by index.
    #[must_use]
    pub const fn from(&self) -> usize {
        self.from
    }

    /// Just past the section's last measure, by index.
    #[must_use]
    pub const fn to(&self) -> usize {
        self.to
    }

    /// The speed.
    #[must_use]
    pub const fn speed(&self) -> Speed {
        self.speed
    }

    /// The lead-in.
    #[must_use]
    pub const fn lead_in(&self) -> TimeUs {
        self.lead_in
    }

    /// The displayed gauge, by index into the preset's gauges.
    #[must_use]
    pub const fn gauge(&self) -> usize {
        self.gauge
    }

    /// The displayed gauge's name, if the preset has one.
    #[must_use]
    pub fn gauge_name(&self) -> Option<&str> {
        self.rule.as_ref().map(|r| r.id.as_str())
    }

    /// The displayed gauge's initial value.
    #[must_use]
    pub const fn value(&self) -> GaugeValue {
        self.value
    }

    /// Whether rounds repeat.
    #[must_use]
    pub const fn repeat(&self) -> bool {
        self.repeat
    }

    /// The row under the cursor.
    #[must_use]
    pub const fn row(&self) -> PracticeRow {
        self.row
    }

    /// Whether the row under the cursor is being edited.
    #[must_use]
    pub const fn editing(&self) -> bool {
        self.editing
    }

    /// Why the last Start was refused, until the next Start.
    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Shows why Start was refused, or nothing.
    pub fn set_notice(&mut self, notice: Option<String>) {
        self.notice = notice;
    }

    /// Acts on one input (design.md "Input", the practice page's table).
    pub fn input(&mut self, input: PracticeInput) -> PracticeActed {
        use PracticeInput::{Back, Confirm, Down, Left, Right, Up};
        let changed = |c: bool| {
            if c {
                PracticeActed::Changed
            } else {
                PracticeActed::Nothing
            }
        };
        if self.editing {
            return match input {
                Up | Right => changed(self.step(true)),
                Down | Left => changed(self.step(false)),
                Confirm | Back => {
                    self.editing = false;
                    PracticeActed::Changed
                }
            };
        }
        match input {
            Up => changed(self.move_row(-1)),
            Down => changed(self.move_row(1)),
            Left => changed(self.step(false)),
            Right => changed(self.step(true)),
            Confirm if self.row == PracticeRow::Start => {
                self.notice = None;
                PracticeActed::Start
            }
            Confirm => {
                self.editing = true;
                PracticeActed::Changed
            }
            Back => PracticeActed::Back,
        }
    }

    /// Moves the cursor `by` rows, stopping at the first and at Start.
    fn move_row(&mut self, by: isize) -> bool {
        let last = PracticeRow::ALL.len() - 1;
        let to = self.row.index().saturating_add_signed(by).min(last);
        let row = PracticeRow::ALL.get(to).copied().unwrap_or(self.row);
        let moved = row != self.row;
        self.row = row;
        moved
    }

    /// Steps the value of the row under the cursor up or down, within its
    /// bounds; whether it changed. Start has no value.
    fn step(&mut self, up: bool) -> bool {
        match self.row {
            PracticeRow::From => {
                let to = if up {
                    self.from + 1
                } else {
                    self.from.saturating_sub(1)
                };
                let ok = to < self.to && to != self.from;
                if ok {
                    self.from = to;
                }
                ok
            }
            PracticeRow::To => {
                let to = if up { self.to + 1 } else { self.to - 1 };
                let ok = to > self.from && to <= self.measures.len();
                if ok {
                    self.to = to;
                }
                ok
            }
            PracticeRow::Speed => {
                let k = self.speed.twentieths();
                let k = if up { k + 1 } else { k.saturating_sub(1) };
                match Speed::from_twentieths(k) {
                    Some(s) => {
                        self.speed = s;
                        true
                    }
                    None => false,
                }
            }
            PracticeRow::LeadIn => {
                let step = if up { LEAD_IN_STEP.0 } else { -LEAD_IN_STEP.0 };
                let to = TimeUs(self.lead_in.0 + step);
                let ok = (MIN_LEAD_IN..=MAX_LEAD_IN).contains(&to);
                if ok {
                    self.lead_in = to;
                }
                ok
            }
            PracticeRow::Gauge => {
                let step = if up {
                    GaugeValue::PERCENT
                } else {
                    -GaugeValue::PERCENT
                };
                let to = GaugeValue(self.value.0.saturating_add(step));
                let ok = self
                    .rule
                    .as_ref()
                    .is_some_and(|r| eval::accepts_initial(r, to));
                if ok {
                    self.value = to;
                }
                ok
            }
            PracticeRow::Repeat => {
                self.repeat = !self.repeat;
                true
            }
            PracticeRow::Start => false,
        }
    }

    /// The practice set up for `loaded`, read at the chosen speed: the
    /// section from measure `from` to the start of measure `to`, or to the
    /// chart's end ([`chart_end`]) for the last; the displayed gauge at its
    /// value and every other gauge of the current preset at its start.
    #[must_use]
    pub fn practice(&self, loaded: &Loaded) -> Practice {
        let at = |i: usize| self.measures.get(i).copied();
        let start = at(self.from).unwrap_or(TimeUs(0));
        let end = match at(self.to) {
            Some(t) if self.to < self.measures.len() => t,
            _ => chart_end(loaded.chart(), loaded.config().speed),
        };
        let initial_gauges = loaded
            .preset()
            .gauges
            .iter()
            .enumerate()
            .map(|(i, g)| if i == self.gauge { self.value } else { g.start })
            .collect();
        Practice {
            start,
            end,
            lead_in: self.lead_in,
            initial_gauges,
            repeat: self.repeat,
        }
    }
}

#[cfg(test)]
mod tests {
    use bms_model::{Speed, TimeUs};
    use rules::GaugeValue;

    use super::{PracticeActed, PracticeInput, PracticeRow, PracticeScreen, measures};

    fn load(text: &str) -> bms_model::Chart {
        bms_parser::load_bms(
            text.as_bytes(),
            std::path::Path::new("."),
            &Default::default(),
        )
        .unwrap()
        .chart
    }

    /// At 7 BPM a measure lasts 34,285,714.28… µs: the note on measure 1's
    /// bar line rounds down to 34,285,714 µs, a microsecond before the
    /// position reaches the bar line, and still starts measure 1; a note on
    /// the bar line at the start of a STOP too.
    #[test]
    fn measures_include_notes_on_their_bar_line() {
        let chart = load("#BPM 7\n#00111:01\n#00211:01\n#00311:01\n");
        let m = measures(&chart);
        assert_eq!(m[0], TimeUs(0));
        assert_eq!(m[1], TimeUs(34_285_714));
        let note = chart.lanes[1][0].time;
        assert_eq!(note, m[1]);
        // The position reaches the bar line a microsecond later.
        assert!(chart.scroll.position_at(note) < chart.bar_lines[1]);
        // A STOP at measure 1's bar line, with a note on it.
        let chart = load("#BPM 120\n#STOP01 192\n#00211:01\n#00209:01\n");
        let m = measures(&chart);
        let on_bar = chart.lanes[1]
            .iter()
            .find(|n| n.pos == chart.bar_lines[2])
            .unwrap();
        assert_eq!(m[2], on_bar.time);
        // No bar lines: one measure.
        let built = bms_model::ChartBuilder::new(150_000).build().unwrap();
        assert_eq!(measures(&built), [TimeUs(0)]);
    }

    /// Every value stays within its bounds whatever is pressed: the
    /// section keeps `from < to ≤ measures`, the speed the ladder, the
    /// lead-in 1 to 10 s in 0.5 s steps, the gauge values its gauge
    /// accepts; the cursor stops at the first row and at Start.
    #[test]
    fn practice_screen_keeps_values_in_bounds() {
        use PracticeInput::{Back, Confirm, Down, Left, Right, Up};
        let chart = load("#BPM 120\n#00111:01\n#00211:01\n#00311:01\n");
        let preset = rules::tapestry();
        let mut s = PracticeScreen::new(&chart, &preset, 99);
        let n = s.measures().len();
        assert!(n >= 3, "{n}");
        assert_eq!((s.from(), s.to()), (0, n));
        assert_eq!(s.gauge(), preset.gauges.len() - 1);
        assert_eq!(s.value(), preset.gauges[s.gauge()].start);
        assert_eq!(
            (s.speed(), s.lead_in(), s.repeat()),
            (Speed::NORMAL, crate::LEAD_IN, true)
        );
        assert_eq!(s.input(Up), PracticeActed::Nothing);
        // From: up to `to - 1` only.
        for _ in 0..n + 3 {
            s.input(Right);
        }
        assert_eq!(s.from(), n - 1);
        assert_eq!(s.input(Down), PracticeActed::Changed);
        assert_eq!(s.row(), PracticeRow::To);
        assert_eq!(s.input(Left), PracticeActed::Nothing);
        assert_eq!(s.to(), n);
        assert_eq!(s.input(Right), PracticeActed::Nothing);
        s.input(Up);
        for _ in 0..n + 3 {
            s.input(Left);
        }
        assert_eq!(s.from(), 0);
        s.input(Down);
        for _ in 0..n + 3 {
            s.input(Left);
        }
        assert_eq!(s.to(), 1);
        // Speed: the ladder, edited with up and down.
        s.input(Down);
        assert_eq!(s.input(Confirm), PracticeActed::Changed);
        assert!(s.editing());
        for _ in 0..30 {
            s.input(Down);
        }
        assert_eq!(s.speed(), Speed::SLOWEST);
        for _ in 0..30 {
            s.input(Up);
        }
        assert_eq!(s.speed(), Speed::FASTEST);
        assert_eq!(s.input(Back), PracticeActed::Changed);
        assert!(!s.editing());
        // Lead-in: 1 to 10 s.
        s.input(Down);
        for _ in 0..30 {
            s.input(Left);
        }
        assert_eq!(s.lead_in(), TimeUs(1_000_000));
        for _ in 0..30 {
            s.input(Right);
        }
        assert_eq!(s.lead_in(), TimeUs(10_000_000));
        // The gauge: what it accepts, in 1 % steps.
        s.input(Down);
        let rule = preset.gauges[s.gauge()].clone();
        for _ in 0..200 {
            s.input(Left);
        }
        assert!(eval::accepts_initial(&rule, s.value()));
        assert!(!eval::accepts_initial(
            &rule,
            GaugeValue(s.value().0 - GaugeValue::PERCENT)
        ));
        for _ in 0..200 {
            s.input(Right);
        }
        assert!(eval::accepts_initial(&rule, s.value()));
        assert!(s.value() <= rule.max);
        // Repeat toggles; Start has no value and starts.
        s.input(Down);
        s.input(Right);
        assert!(!s.repeat());
        s.input(Down);
        assert_eq!(s.row(), PracticeRow::Start);
        assert_eq!(s.input(Down), PracticeActed::Nothing);
        assert_eq!(s.input(Left), PracticeActed::Nothing);
        s.set_notice(Some("refused".to_owned()));
        assert_eq!(s.input(Confirm), PracticeActed::Start);
        assert_eq!(s.notice(), None);
        assert_eq!(s.input(Back), PracticeActed::Back);
    }
}
