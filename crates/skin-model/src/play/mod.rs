//! The play screen's state: the per-attempt [`Play`] a skin fills each
//! frame from a [`SkinFrame`], the play schema and the reducer.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
#![deny(clippy::float_arithmetic, clippy::float_cmp)]

mod reducer;
pub(crate) mod schema;
pub(crate) mod timers;

use bms_model::{Chart, Lane, ScrollMap, TimeUs};
use eval::{GaugeState, JudgeEvent, Section, Tally};
use rules::{GaugeRule, GaugeValue};

use self::reducer::Reducer;
use self::schema::{field, timer};
use crate::expr::{State, TIMER_OFF};

/// What the skin knows of one of the preset's gauges, in the preset's
/// order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GaugeDesc {
    /// The value a whole-chart attempt starts at.
    pub start: GaugeValue,
    /// The value `gauge_max` turns on at.
    pub max: GaugeValue,
}

impl GaugeDesc {
    /// The descriptor of a preset's gauge rule.
    #[must_use]
    pub fn from_rule(rule: &GaugeRule) -> Self {
        Self {
            start: rule.start,
            max: rule.max,
        }
    }
}

/// What one attempt plays.
#[derive(Clone, Copy, Debug)]
pub enum Attempt<'a> {
    /// The whole chart: `ready` starts `lead_in` before chart time 0 and
    /// `play` at 0, every note is a target and the gauges start at their
    /// descriptors' `start`.
    Full { lead_in: TimeUs },
    /// A section: a practice round, or a start mid-chart (a section whose
    /// end is past the chart). `ready` starts `lead_in` before the
    /// section's start and `play` at it, the targets are the section's and
    /// the gauges start at its `initial_gauges`.
    Section(&'a Section),
}

/// What is fixed for one attempt.
#[derive(Clone, Copy, Debug)]
pub struct PlayContext<'a> {
    /// The preset's gauges, in its order.
    pub gauges: &'a [GaugeDesc],
    /// The attempt.
    pub attempt: Attempt<'a>,
    /// Whether an empty POOR breaks the combo (the preset's
    /// `empty_poor.breaks_combo`), which turns `combo.up` off.
    pub empty_poor_breaks_combo: bool,
}

/// What one frame gives the skin.
#[derive(Clone, Copy, Debug)]
pub struct SkinFrame<'a> {
    /// The frame's chart time: timers age on it.
    pub now: TimeUs,
    /// Added to `now` for the display clock, which `beat` follows.
    pub display_offset: TimeUs,
    /// The play view's running totals.
    pub tally: &'a Tally,
    /// The play view's gauges, in the preset's order.
    pub gauges: &'a [GaugeState],
    /// The shown gauge's index into `gauges`; it may change between
    /// frames.
    pub gauge: usize,
    /// The effects drained since the last frame, in emission order.
    pub effects: &'a [JudgeEvent],
}

/// Why [`Skin::prepare`](crate::Skin::prepare) refused an attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PrepareError {
    #[error("the attempt's lead-in is negative")]
    NegativeLeadIn,
    #[error("the attempt gives {got} initial gauges for {expected} gauges")]
    InitialGauges { expected: usize, got: usize },
    #[error("the attempt's countdown starts outside the time range")]
    Overflow,
}

/// One attempt's skin state: the reducer, every timer's start and reading,
/// and the fields, as of the last build. [`Skin::prepare`](crate::Skin::prepare)
/// makes a fresh one per attempt, so a retry starts clean.
#[derive(Clone, Debug)]
pub struct Play {
    reducer: Reducer,
    /// Per timer slot (built-in, then `when`): its start, `None` while off.
    starts: Box<[Option<TimeUs>]>,
    /// Per timer slot: elapsed ms, or [`TIMER_OFF`].
    ms: Box<[i32]>,
    /// Per `when` timer: its condition at the last build.
    when_on: Box<[bool]>,
    fields: [i32; field::COUNT],
    scroll: ScrollMap,
    /// The display beat last seen and the µs it was crossed at.
    beat: Option<(i128, i64)>,
    ready_at: TimeUs,
    play_at: TimeUs,
}

impl Play {
    /// A fresh attempt with `whens` `when` timers.
    pub(crate) fn new(
        chart: &Chart,
        ctx: &PlayContext<'_>,
        whens: usize,
    ) -> Result<Self, PrepareError> {
        let (play_at, lead_in, section) = match ctx.attempt {
            Attempt::Full { lead_in } => (TimeUs(0), lead_in, None),
            Attempt::Section(s) => (s.start, s.lead_in, Some(s)),
        };
        if lead_in.0 < 0 {
            return Err(PrepareError::NegativeLeadIn);
        }
        let ready_at = play_at.checked_sub(lead_in).ok_or(PrepareError::Overflow)?;
        if let Some(s) = section
            && s.initial_gauges.len() != ctx.gauges.len()
        {
            return Err(PrepareError::InitialGauges {
                expected: ctx.gauges.len(),
                got: s.initial_gauges.len(),
            });
        }
        let initial = ctx.gauges.iter().enumerate().map(|(i, g)| {
            let value = section
                .and_then(|s| s.initial_gauges.get(i).copied())
                .unwrap_or(g.start);
            (value, g.max)
        });
        let reducer = Reducer::new(
            chart,
            section,
            initial,
            ready_at,
            ctx.empty_poor_breaks_combo,
        );
        let slots = usize::from(timer::BUILTIN).saturating_add(whens);
        Ok(Self {
            reducer,
            starts: vec![None; slots].into_boxed_slice(),
            ms: vec![TIMER_OFF; slots].into_boxed_slice(),
            when_on: vec![false; whens].into_boxed_slice(),
            fields: [0; field::COUNT],
            scroll: chart.scroll.clone(),
            beat: None,
            ready_at,
            play_at,
        })
    }

    /// The number of timer slots, built-in and `when`.
    pub(crate) fn slots(&self) -> usize {
        self.ms.len()
    }

    /// Timer `slot`'s start as of the last build, `None` while off.
    #[must_use]
    pub fn timer(&self, slot: u16) -> Option<TimeUs> {
        self.timer_at(usize::from(slot))
    }

    pub(crate) fn timer_at(&self, slot: usize) -> Option<TimeUs> {
        self.starts.get(slot).copied().flatten()
    }

    /// The fields and timer readings as of the last build.
    #[must_use]
    pub fn state(&self) -> State<'_> {
        State {
            fields: &self.fields,
            timers: &self.ms,
        }
    }

    /// Whether the long note `note` on `lane` was held (its `LnHeld` was
    /// delivered).
    #[must_use]
    pub fn held(&self, lane: Lane, note: usize) -> bool {
        self.reducer.held(lane.index(), note)
    }

    /// The long note whose visual hold `lane` shows, as of the last build.
    #[must_use]
    pub fn hold_note(&self, lane: Lane) -> Option<usize> {
        self.reducer
            .hold
            .get(lane.index())
            .copied()
            .flatten()
            .map(|h| h.note)
    }
}
