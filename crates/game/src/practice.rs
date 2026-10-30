//! Practice rounds (design.md "Mid-Chart Start and Section Repeat"): a
//! section of the chart played from its lead-in, evaluated as a section
//! (design.md "Section Evaluation"), and ended by a short ring-out.
//!
//! Each round runs reset → restore → lead-in → section evaluation through
//! the play lifecycle every driver shares ([`crate::live`]), and is one
//! attempt and one saved record. Its recording opens at the lead-in's
//! start, where the countdown begins, and its audio is restored there:
//! the schedule is the BGM with the key sounds of every note that is not a
//! target, played as autoplay ([`schedule`]). Completion comes at the
//! section's final deadline; the round then rings out for [`RING_OUT`],
//! fading over its last [`FADE`], instead of the full play's audio tail,
//! and the frozen record is saved once the ring-out has ended.

use bms_model::{BgmEvent, Chart, Note, Speed, TimeUs};
use eval::{DeviceProfile, EvalError, Evaluator, InputStart, Section};
use rules::{GaugeValue, RulePreset};

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

/// Checks that `section` can be evaluated on `chart` under `preset`, as a
/// round's opening would.
///
/// # Errors
///
/// What [`Evaluator::new`] refuses.
pub(crate) fn check(
    chart: &Chart,
    preset: &RulePreset,
    section: &Section,
) -> Result<(), PracticeError> {
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

/// Whether `note` is a target of `section` (design.md "Section
/// Evaluation"): a tap or mine with `start ≤ t < end`, a long note with
/// `start ≤ head` and its end before `end`.
fn is_target(section: &Section, note: &Note) -> bool {
    section.start <= note.time && note.time < section.end && note.end() < section.end
}

/// The audio schedule of a round of `section`: the chart's BGM merged with
/// the key sound of every note that is not a target, at the note's time,
/// as autoplay plays it (before the section, after its end, and long notes
/// crossing either edge).
#[must_use]
pub(crate) fn schedule(chart: &Chart, section: &Section) -> Vec<BgmEvent> {
    crate::autoplay::schedule_where(chart, |n| !is_target(section, n))
}
