//! Verifying a saved record against a freshly loaded chart, and
//! re-evaluating its input under its own preset or another one (design.md
//! "Determinism", "Extension Boundaries").
//!
//! A record reaches this module decoded, so the decoding checks (format
//! version, structure, preset hash) have already passed. The rest are
//! checked here in a fixed order, and the first failure is the refusal:
//! the evaluator version, the recorded preset copy, the loader version, the
//! load with the recorded seed, the resolved-chart hash, and the chart
//! transform. [`load_recorded_chart`] runs every check and
//! [`evaluate_record`] the first two again, so each entry point refuses in
//! that order on its own. The chart file's SHA-256 is not compared: an
//! edited file that resolves to the same chart verifies.

use std::path::Path;

use bms_model::{Chart, TimeUs};
use bms_parser::{LOADER_VERSION, LoadError, LoadOptions};
use eval::{EvalError, EvalInput, Evaluation, JudgeEventKind, evaluation_time};
use rules::{CanonicalError, Implementation, RulePreset};

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
/// [`Load`](VerifyError::Load) with the recorded `#RANDOM` seed,
/// [`ResolvedChartMismatch`](VerifyError::ResolvedChartMismatch) and
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
    let opts = LoadOptions {
        seed: recorded.random_seed,
        ..LoadOptions::default()
    };
    let loaded = bms_parser::load_bms(bytes, dir, &opts).map_err(VerifyError::Load)?;
    if loaded.chart.identity.resolved_hash != recorded.identity.resolved_hash {
        return Err(VerifyError::ResolvedChartMismatch);
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
///   up to the recorded watermark, run to the preset's own final deadline.
///   A deadline later than the watermark is
///   [`NoInputAfter`](Coverage::NoInputAfter) the watermark.
/// - **What-if of an aborted play:** to the recorded watermark,
///   [`EndedMidway`](Coverage::EndedMidway), with no gauge cleared even if
///   the preset's final deadline comes before the watermark.
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
    let (events, finalized) = if as_played || aborted {
        (record.events.clone(), Some(watermark))
    } else {
        // What was recorded after the watermark was never evaluated, so it
        // is not input of the play. A source's events are in receive order,
        // so each source keeps a prefix.
        let played = record
            .events
            .iter()
            .filter(|e| evaluation_time(e, &device) <= watermark)
            .copied()
            .collect();
        (played, None)
    };
    let input = EvalInput {
        events,
        device,
        start: record.start.clone(),
        finalized,
        section: None,
    };
    let mut evaluation = eval::evaluate(chart, &input, preset)?;
    let coverage = if aborted {
        if !as_played {
            for g in &mut evaluation.gauges {
                g.cleared = false;
            }
        }
        Coverage::EndedMidway(watermark)
    } else if !as_played && song_end(&evaluation).is_some_and(|t| t > watermark) {
        Coverage::NoInputAfter(watermark)
    } else {
        Coverage::Recorded
    };
    Ok(ReEvaluation {
        evaluation,
        as_played,
        coverage,
    })
}

/// When evaluation reached the final deadline, if it did.
fn song_end(e: &Evaluation) -> Option<TimeUs> {
    e.events
        .iter()
        .rev()
        .find(|e| e.kind == JudgeEventKind::SongEnd)
        .map(|e| e.time)
}

#[cfg(test)]
mod tests;
