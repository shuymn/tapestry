//! Practice rounds (design.md "Mid-Chart Start and Section Repeat"): a
//! section of the chart played from its lead-in, evaluated as a section
//! (design.md "Section Evaluation"), and ended by a short ring-out.
//!
//! Each round runs reset → restore → lead-in → section evaluation through
//! the play lifecycle every driver shares ([`crate::live`]), and is one
//! attempt and one saved record. Its recording opens [`RESTORE_AHEAD`]
//! before the lead-in's start, where the countdown begins, so the play's
//! origin is fixed before its audio is restored at the lead-in's start
//! ([`crate::restore`]): the schedule is the BGM with the key sounds of
//! every note that is not a target, played as autoplay ([`schedule`]).
//! Completion comes at the
//! section's final deadline; the round then rings out for [`RING_OUT`],
//! fading over its last [`FADE`], instead of the full play's audio tail,
//! and the frozen record is saved once the ring-out has ended.

use bms_model::{BgmEvent, Chart, NoteKind, Speed, TimeUs};
use eval::{DeviceProfile, EvalError, Evaluator, InputStart, Section};
use rules::{GaugeValue, RulePreset};

/// The lead-in a practice starts with (tuned by playing).
pub const LEAD_IN: TimeUs = TimeUs(2_000_000);
/// How long before its lead-in a round's recording opens (tuned by
/// playing): the opening fixes the play's origin, so the restore is planned
/// on it for the lead-in's start, this far in the future, and lands on time
/// although the output is written ahead.
pub const RESTORE_AHEAD: TimeUs = TimeUs(200_000);
/// How much later than the lead-in's start the fallback restore is
/// planned for, should the first be late: a game thread slower than
/// [`RESTORE_AHEAD`] minus the queued output.
pub const RESTORE_RETRY: TimeUs = TimeUs(500_000);

/// How long a round's ring-out lasts from completion (tuned by playing).
pub const RING_OUT: TimeUs = TimeUs(1_500_000);
/// The ring-out's last part, over which the output fades to silence
/// (tuned by playing).
pub const FADE: TimeUs = TimeUs(500_000);

/// What the practice screen sets up: a section of the chart, its lead-in,
/// the gauges' values at its start, and whether rounds repeat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Practice {
    /// The section's first time, in the chart's 1× time.
    pub start: TimeUs,
    /// The time just past the section, in the chart's 1× time.
    pub end: TimeUs,
    /// How long the lead-in before the section lasts, in real time: the
    /// round's countdown.
    pub lead_in: TimeUs,
    /// Each gauge's value at the section's start, in the preset's order.
    pub initial_gauges: Vec<GaugeValue>,
    /// Whether rounds repeat (section repeat) or the round is played once
    /// (mid-chart start).
    pub repeat: bool,
}

/// Why a practice cannot be set up.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PracticeError {
    /// An autoplay session plays whole charts only.
    #[error("an autoplay session cannot practice a section")]
    Autoplay,
    /// The section cannot be evaluated: its range, lead-in or initial
    /// gauges are unusable for the chart and the preset.
    #[error("the section cannot be evaluated")]
    Section(#[source] EvalError),
    /// No tap or long note of the chart is a target of the section: there
    /// is nothing to play.
    #[error("the section has no notes to play")]
    NoTargets,
    /// The play preset no longer has a gauge whose initial value was set.
    #[error("the preset no longer has the gauge {0}")]
    GaugeGone(String),
}

/// `t` of the chart's 1× time in the real time of a chart loaded at
/// `speed`: `floor(t × 20/k)`, as the loader maps chart times (design.md
/// "Practice Speed Changes").
#[must_use]
pub(crate) fn mapped(t: TimeUs, speed: Speed) -> TimeUs {
    let real = (i128::from(t.0) * 20).div_euclid(i128::from(speed.twentieths()));
    TimeUs(i64::try_from(real).unwrap_or(if real < 0 { i64::MIN } else { i64::MAX }))
}

impl Practice {
    /// The section the evaluator takes for a chart loaded at `speed`: its
    /// edges mapped to real time ([`mapped`]), the lead-in and the initial
    /// gauges as they are.
    #[must_use]
    pub fn section(&self, speed: Speed) -> Section {
        Section {
            start: mapped(self.start, speed),
            end: mapped(self.end, speed),
            lead_in: self.lead_in,
            initial_gauges: self.initial_gauges.clone(),
        }
    }
}

/// Where a practice to the chart's end ends, in 1× time, for `chart` as
/// loaded at `speed`: `ceil((r + 1) × k/20)`, where `r` is the latest note
/// end of the loaded chart, so that `floor(end × 20/k) ≥ r + 1` and the
/// last note stays inside the section at every speed.
#[must_use]
pub fn chart_end(chart: &Chart, speed: Speed) -> TimeUs {
    let last = chart
        .lanes
        .iter()
        .flatten()
        .map(|n| n.end().0)
        .max()
        .unwrap_or(0);
    let k = i128::from(speed.twentieths());
    let end = (i128::from(last) + 1) * k;
    let end = end.div_euclid(20) + i128::from(end.rem_euclid(20) != 0);
    TimeUs(i64::try_from(end).unwrap_or(i64::MAX))
}

/// A mid-chart start from `start` (the chart's 1× time) to the chart's end
/// ([`chart_end`]) of `chart` as loaded at `speed`: one round with the
/// default lead-in ([`LEAD_IN`]) and every gauge of `preset` at its start.
/// [`Loaded::set_practice`](crate::Loaded::set_practice) checks it.
#[must_use]
pub fn mid_chart_start(
    chart: &Chart,
    speed: Speed,
    preset: &RulePreset,
    start: TimeUs,
) -> Practice {
    Practice {
        start,
        end: chart_end(chart, speed),
        lead_in: LEAD_IN,
        initial_gauges: preset.gauges.iter().map(|g| g.start).collect(),
        repeat: false,
    }
}

/// Checks that `section` can be evaluated on `chart` under `preset`, as a
/// round's opening would, and that it has a tap or long note to play.
///
/// # Errors
///
/// [`PracticeError::NoTargets`] for a section without a tap or long note
/// among its targets, then what [`Evaluator::new`] refuses.
pub(crate) fn check(
    chart: &Chart,
    preset: &RulePreset,
    section: &Section,
) -> Result<(), PracticeError> {
    let playable = |n: &bms_model::Note| matches!(n.kind, NoteKind::Tap | NoteKind::Long { .. });
    if !chart
        .lanes
        .iter()
        .flatten()
        .any(|n| playable(n) && section.is_target(n))
    {
        return Err(PracticeError::NoTargets);
    }
    let device = DeviceProfile {
        sources: Vec::new(),
    };
    Evaluator::new(
        chart,
        preset,
        &device,
        &InputStart::default(),
        Some(section),
        0,
    )
    .map(|_| ())
    .map_err(PracticeError::Section)
}

/// The audio schedule of a round of `section`: the chart's BGM merged with
/// the key sound of every note that is not a target, at the note's time,
/// as autoplay plays it (before the section, after its end, and long notes
/// crossing either edge).
#[must_use]
pub(crate) fn schedule(chart: &Chart, section: &Section) -> Vec<BgmEvent> {
    crate::autoplay::schedule_where(chart, |n| !section.is_target(n))
}
