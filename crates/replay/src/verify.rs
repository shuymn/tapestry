//! Verifying a saved record against a freshly loaded chart, and
//! re-evaluating its input under its own preset or another one (design.md
//! "Determinism", "Extension Boundaries").
//!
//! A record reaches this module decoded, so the decoding checks (format
//! version, structure, preset hash) have already passed. The rest are
//! checked here in a fixed order, and the first failure is the refusal:
//! the evaluator version, the recorded preset copy, the loader version, the
//! load with the recorded seed and speed, the resolved-chart hash, the
//! played-chart hash, and the chart transform. [`load_recorded_chart`]
//! runs every check and [`evaluate_record`] the first two again, so each
//! entry point refuses in that order on its own. The chart file's SHA-256
//! is not compared: an edited file that resolves to the same chart, at
//! the record's speed too, verifies.

use std::path::Path;

use bms_model::{Chart, TimeUs};
use bms_parser::{LOADER_VERSION, LoadError, LoadOptions};
use eval::{EvalError, EvalInput, Evaluation, Evaluator, Section, evaluation_time};
use rules::{CanonicalError, GaugeKind, GaugeRule, GaugeValue, Implementation, RulePreset};

use crate::{EndStatus, PASSTHROUGH, PlayRecord, preset_hash};

/// Why a record was refused, in the order the checks run.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    /// The record was made by an evaluator version this build does not run.
    #[error("the record was made by evaluator version {0}, which this build does not run")]
    UnsupportedEvaluator(u32),
    /// The record's copy of its preset does not decode, for example because
    /// it uses an older canonical encoding.
    #[error("the record's preset copy cannot be decoded")]
    PresetCopy(#[source] CanonicalError),
    /// The record was made by a loader version this build does not run.
    #[error("the record was made by loader version {0}, which this build does not run")]
    UnsupportedLoader(u32),
    /// The chart file does not load.
    #[error("the chart cannot be loaded")]
    Load(#[source] LoadError),
    /// The chart file resolves to another chart than the record's.
    #[error("the chart resolves to another chart than the record's")]
    ResolvedChartMismatch,
    /// The chart file resolves to the record's chart at 1×, but its times
    /// at the record's speed are not the ones the play was judged on.
    #[error("the chart plays at the record's speed otherwise than the record's")]
    PlayedChartMismatch,
    /// The record names a chart transform this build does not know.
    #[error("the record's chart transform {id:?} version {version} is unknown")]
    UnknownTransform {
        /// The transform's id.
        id: String,
        /// The version of its rules.
        version: u32,
    },
    /// The evaluator refused the record.
    #[error("the record cannot be evaluated")]
    Eval(#[source] EvalError),
}

/// What part of a re-evaluation the recorded input covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coverage {
    /// The recorded input covers all of it.
    Recorded,
    /// A what-if of a completed play ran past the recording's end: there is
    /// no input after this time, the recorded finalized watermark.
    NoInputAfter(TimeUs),
    /// The play was aborted: evaluation ended midway, at this recorded
    /// finalized watermark.
    EndedMidway(TimeUs),
}

/// A record's input evaluated under a preset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReEvaluation {
    /// Totals, gauges and the event stream.
    pub evaluation: Evaluation,
    /// Whether the preset is the record's own (equal hash), so the result
    /// is the play as it was judged.
    pub as_played: bool,
    /// What the recorded input covers.
    pub coverage: Coverage,
    /// For a practice record, one per gauge of the preset, in its order:
    /// whether the gauge started at the preset's start value because the
    /// record holds no value for its ID that the gauge accepts (design.md
    /// "Practice Records"). Empty for a full play, whose gauges all start
    /// at the preset's start values.
    pub started_at_default: Vec<bool>,
}

/// The section a practice record's input is evaluated in under `preset`,
/// and which of the preset's gauges start at its start value: each gauge
/// starts at the value the record holds for its ID, unless the record has
/// none or the gauge does not accept it (outside its bounds, or 0 or less
/// on a survival gauge), as a preset's own start must be; then at its start
/// value. `None` for a full play.
fn practice_section(record: &PlayRecord, preset: &RulePreset) -> Option<(Section, Vec<bool>)> {
    let practice = record.play_type.practice()?;
    let (initial_gauges, started_at_default) = preset
        .gauges
        .iter()
        .map(|rule| {
            practice
                .initial_gauges
                .iter()
                .find(|g| g.id == rule.id)
                .map(|g| g.value)
                .filter(|&v| accepts(rule, v))
                .map_or((rule.start, true), |v| (v, false))
        })
        .unzip();
    let section = Section {
        start: practice.start,
        end: practice.end,
        lead_in: practice.lead_in,
        initial_gauges,
    };
    Some((section, started_at_default))
}

/// Whether `rule`'s gauge can start at `v`, as the evaluator takes a
/// section's initial gauges (design.md "Section Evaluation"): within its
/// bounds, and for a survival gauge above 0 and not below the value it
/// fails under.
fn accepts(rule: &GaugeRule, v: GaugeValue) -> bool {
    (rule.min..=rule.max).contains(&v)
        && match rule.kind {
            GaugeKind::EndThreshold { .. } => true,
            GaugeKind::Survival { fails_below } => v > GaugeValue::EMPTY && v >= fails_below,
        }
}

/// Checks the record's evaluator version, then decodes its preset copy.
fn recorded_preset(record: &PlayRecord) -> Result<RulePreset, VerifyError> {
    let evaluator = record.versions.evaluator;
    if evaluator != Implementation::STANDARD_VERSION {
        return Err(VerifyError::UnsupportedEvaluator(evaluator));
    }
    RulePreset::from_canonical_bytes(&record.preset.canonical_bytes)
        .map_err(VerifyError::PresetCopy)
}

/// Loads the record's chart from its file `bytes`, sounds resolving inside
/// `dir`, and returns it as the play evaluated it.
///
/// # Errors
///
/// The first check that fails, in this order:
/// [`UnsupportedEvaluator`](VerifyError::UnsupportedEvaluator),
/// [`PresetCopy`](VerifyError::PresetCopy) (both before the bytes are
/// read), [`UnsupportedLoader`](VerifyError::UnsupportedLoader),
/// [`Load`](VerifyError::Load) with the recorded `#RANDOM` seed and speed,
/// [`ResolvedChartMismatch`](VerifyError::ResolvedChartMismatch),
/// [`PlayedChartMismatch`](VerifyError::PlayedChartMismatch) and
/// [`UnknownTransform`](VerifyError::UnknownTransform).
pub fn load_recorded_chart(
    record: &PlayRecord,
    bytes: &[u8],
    dir: &Path,
) -> Result<Chart, VerifyError> {
    recorded_preset(record)?;
    let recorded = &record.chart;
    let loader = recorded.identity.loader_version;
    if loader != LOADER_VERSION {
        return Err(VerifyError::UnsupportedLoader(loader));
    }
    // At the recorded speed: the recorded input is in its real time. The
    // resolved hash is the 1× chart's at every speed.
    let opts = LoadOptions {
        seed: recorded.random_seed,
        speed: record.speed,
        ..LoadOptions::default()
    };
    let loaded = bms_parser::load_bms(bytes, dir, &opts).map_err(VerifyError::Load)?;
    if loaded.chart.identity.resolved_hash != recorded.identity.resolved_hash {
        return Err(VerifyError::ResolvedChartMismatch);
    }
    // At a speed, times floored from the exact time can differ between
    // files that resolve alike at 1×; the played hash pins the play's.
    if loaded.chart.identity.played_hash != recorded.identity.played_hash {
        return Err(VerifyError::PlayedChartMismatch);
    }
    // The pass-through returns the chart unchanged.
    if !PASSTHROUGH.is(&recorded.transform) {
        return Err(VerifyError::UnknownTransform {
            id: recorded.transform.id.clone(),
            version: recorded.transform.version,
        });
    }
    Ok(loaded.chart)
}

/// Evaluates the record's input on `chart` (from [`load_recorded_chart`])
/// under `preset_override`, or else under the record's own preset copy, as
/// [`evaluate_under`] does.
///
/// # Errors
///
/// [`UnsupportedEvaluator`](VerifyError::UnsupportedEvaluator), then
/// [`PresetCopy`](VerifyError::PresetCopy): the copy is decoded with an
/// override too. Then what the evaluator refuses
/// ([`Eval`](VerifyError::Eval)).
pub fn evaluate_record(
    record: &PlayRecord,
    chart: &Chart,
    preset_override: Option<&RulePreset>,
) -> Result<ReEvaluation, VerifyError> {
    let recorded = recorded_preset(record)?;
    evaluate_under(record, chart, preset_override.unwrap_or(&recorded)).map_err(VerifyError::Eval)
}

/// Evaluates the record's input on `chart` under `preset`, in one of three
/// ways (design.md "Extension Boundaries"):
/// - **As played** (the preset's hash is the record's): to the recorded
///   finalized watermark, which reproduces the live stream.
/// - **What-if of a completed play:** the inputs the play evaluated, those
///   up to the recorded watermark, run to the preset's own final deadline
///   and stop there, so an input between that deadline and the watermark
///   is not judged. A deadline later than the watermark is
///   [`NoInputAfter`](Coverage::NoInputAfter) the watermark.
/// - **What-if of an aborted play:** to the recorded watermark,
///   [`EndedMidway`](Coverage::EndedMidway), with no gauge cleared even if
///   the preset's final deadline comes before the watermark.
///
/// A practice record is evaluated in its section, in each of these ways,
/// with its initial gauges transferred to the preset's gauges by ID
/// ([`ReEvaluation::started_at_default`]); the final deadline is then the
/// section's.
///
/// The evaluator version is not checked here; the caller checks it first.
///
/// # Errors
///
/// What [`eval::evaluate`] refuses.
pub fn evaluate_under(
    record: &PlayRecord,
    chart: &Chart,
    preset: &RulePreset,
) -> Result<ReEvaluation, EvalError> {
    let watermark = record.status.finalized;
    let as_played = preset_hash(preset) == record.preset.sha256;
    let aborted = record.status.end == EndStatus::EndedMidway;
    let device = record.devices.profile();
    let (section, started_at_default) = practice_section(record, preset).unzip();
    let (events, finalized, no_input_after) = if as_played || aborted {
        (record.events.clone(), watermark, None)
    } else {
        // The live evaluator judged every input at or before the watermark
        // (`merge` and `advance_to` include it), and nothing after it, so
        // only those are input of the play. Each source's events are in
        // evaluation-time order, so each keeps a prefix.
        let played = record
            .events
            .iter()
            .filter(|e| evaluation_time(e, &device) <= watermark)
            .copied()
            .collect();
        let deadline = Evaluator::new(chart, preset, &device, &record.start, section.as_ref(), 0)?
            .final_deadline();
        (played, deadline, Some(watermark).filter(|&w| deadline > w))
    };
    let input = EvalInput {
        events,
        device,
        start: record.start.clone(),
        finalized: Some(finalized),
        section,
    };
    let mut evaluation = eval::evaluate(chart, &input, preset)?;
    let coverage = if aborted {
        if !as_played {
            for g in &mut evaluation.gauges {
                g.cleared = false;
            }
        }
        Coverage::EndedMidway(watermark)
    } else if let Some(t) = no_input_after {
        Coverage::NoInputAfter(t)
    } else {
        Coverage::Recorded
    };
    Ok(ReEvaluation {
        evaluation,
        as_played,
        coverage,
        started_at_default: started_at_default.unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests;
