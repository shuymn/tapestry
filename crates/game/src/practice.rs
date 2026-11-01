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

/// A value the player set that fitting a practice to an edited play preset
/// reset ([`Loaded::refit_practice`](crate::Loaded::refit_practice)), as
/// the practice page states it: a round never plays a reset value the
/// player set, so the page states the reset and the player starts again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reset {
    /// The gauge's rule no longer accepts the value set: it starts at its
    /// start.
    Refused {
        /// The gauge's ID.
        gauge: String,
        /// The value set.
        set: GaugeValue,
        /// Its start, which the round plays.
        start: GaugeValue,
    },
    /// The preset lost the gauge set, and its value with it; the practice
    /// page shows the gauge at its place instead, at its start.
    Lost {
        /// The gauge's ID.
        gauge: String,
        /// The value set.
        set: GaugeValue,
        /// The gauge shown in its place and its start; `None` for a preset
        /// without gauges.
        shown: Option<(String, GaugeValue)>,
    },
}

impl std::fmt::Display for Reset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused { gauge, set, start } => write!(
                f,
                "the gauge {gauge} no longer accepts {}; it starts at {}",
                Percent(*set),
                Percent(*start)
            ),
            Self::Lost { gauge, set, shown } => {
                write!(f, "the gauge {gauge} ({}) left the preset", Percent(*set))?;
                match shown {
                    Some((id, start)) => write!(f, "; showing {id} at {}", Percent(*start)),
                    None => Ok(()),
                }
            }
        }
    }
}

/// Why a practice fitted to an edited play preset opens no round
/// ([`Loaded::refit_practice`](crate::Loaded::refit_practice),
/// [`Loaded::start_practice`](crate::Loaded::start_practice)): what the fit
/// reset of what the player set, and what the section's check refused,
/// each told when it happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unfit {
    /// What the fit reset of what the player set.
    pub resets: Vec<Reset>,
    /// What the section's check refused under the edited preset.
    pub refused: Option<PracticeError>,
}

impl std::fmt::Display for Unfit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut sep = "";
        if let Some(e) = &self.refused {
            write!(f, "{}", crate::Chain(e))?;
            sep = "; ";
        }
        for reset in &self.resets {
            write!(f, "{sep}{reset}")?;
            sep = "; ";
        }
        Ok(())
    }
}

/// The resets of one fit, as one notice: `None` when nothing was reset.
#[must_use]
pub fn reset_notice(resets: &[Reset]) -> Option<String> {
    let notice = resets
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    (!notice.is_empty()).then_some(notice)
}

/// A gauge value as a percentage, with as many decimals as it has:
/// `20%`, `20.5%`.
struct Percent(GaugeValue);

impl std::fmt::Display for Percent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let v = self.0.0;
        let sign = if v < 0 { "-" } else { "" };
        let (whole, frac) = (
            v.unsigned_abs() / GaugeValue::PERCENT.unsigned_abs(),
            v.unsigned_abs() % GaugeValue::PERCENT.unsigned_abs(),
        );
        if frac == 0 {
            return write!(f, "{sign}{whole}%");
        }
        let digits = format!("{frac:05}");
        write!(f, "{sign}{whole}.{}%", digits.trim_end_matches('0'))
    }
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
}

/// `t` of the chart's 1× time in the real time of a chart loaded at
/// `speed`: `floor(t × 20/k)`, as the loader maps chart times (design.md
/// "Practice Speed Changes").
#[must_use]
pub(crate) fn mapped(t: TimeUs, speed: Speed) -> TimeUs {
    mapped_wide(i128::from(t.0), speed)
}

/// [`mapped`] for a 1× time wider than a [`TimeUs`], such as one past its
/// range by an added pad: only the real time saturates.
#[must_use]
pub(crate) fn mapped_wide(t: i128, speed: Speed) -> TimeUs {
    let real = (t * 20).div_euclid(i128::from(speed.twentieths()));
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

#[cfg(test)]
mod tests {
    use rules::GaugeValue;

    use super::{Reset, reset_notice};

    /// A reset names the gauge and the values, each with the decimals it
    /// has; several join into one notice, and none gives none.
    #[test]
    fn a_reset_says_its_values() {
        let refused = Reset::Refused {
            gauge: "normal".to_owned(),
            set: GaugeValue(2_050_000),
            start: GaugeValue(2_000_000),
        };
        let lost = Reset::Lost {
            gauge: "hard".to_owned(),
            set: GaugeValue(1),
            shown: None,
        };
        assert_eq!(
            reset_notice(&[refused, lost]).as_deref(),
            Some(
                "the gauge normal no longer accepts 20.5%; it starts at 20%; \
                 the gauge hard (0.00001%) left the preset"
            )
        );
        assert_eq!(reset_notice(&[]), None);
    }
}
