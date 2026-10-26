//! The results extension point and its built-in extension, re-evaluation
//! of the saved record.

use std::fmt;

use bms_model::{Chart, TimeUs};
use eval::{EvalError, EvalInput, Evaluation, JudgeEvent, JudgeEventKind};
use replay::PlayRecord;
use rules::RulePreset;

/// What a result extension reads at the end of a play.
#[derive(Clone, Copy, Debug)]
pub struct ResultContext<'a> {
    /// The play's record, as saved.
    pub record: &'a PlayRecord,
    /// The chart evaluated, after the transform.
    pub chart: &'a Chart,
    /// The preset the play ran under.
    pub preset: &'a RulePreset,
    /// The live judgment event stream.
    pub live: &'a [JudgeEvent],
}

/// A suggested input offset from the play's timing (design.md "Automatic
/// Offset Adjustment"): a recommendation based on timing bias, so it
/// includes the player's habits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecommendedOffset {
    /// The median deviation of the judged presses (press − note).
    pub median: TimeUs,
    /// The spread: the third quartile minus the first.
    pub spread: TimeUs,
    /// How many deviations it is computed from.
    pub samples: usize,
}

/// The fewest judged presses a recommended offset is computed from.
pub const MIN_OFFSET_SAMPLES: usize = 32;

/// Why a record could not be re-evaluated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReEvaluationError {
    /// The record names a preset other than the one given.
    PresetMismatch,
    /// The evaluator refused the record.
    Eval(EvalError),
}

impl fmt::Display for ReEvaluationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PresetMismatch => f.write_str("the record names another preset"),
            Self::Eval(e) => write!(f, "the record cannot be evaluated: {e}"),
        }
    }
}

impl std::error::Error for ReEvaluationError {}

/// What the end screen shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResultSummary {
    /// The re-evaluation: totals, gauges and its event stream.
    pub evaluation: Result<Evaluation, ReEvaluationError>,
    /// Whether the re-evaluated stream equals the live one; a mismatch is
    /// flagged, never hidden.
    pub matches_live: bool,
    /// The suggested input offset, once enough presses were judged.
    pub recommended_offset: Option<RecommendedOffset>,
}

/// The results extension point: reads the record and the live stream.
pub trait ResultExtension {
    /// Summarizes a play that ended.
    fn on_play_end(&mut self, ctx: &ResultContext<'_>) -> ResultSummary;
}

/// Re-evaluates the saved record under its own preset, through the
/// finalized watermark, so the end screen's numbers come from the log.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReEvaluate;

impl ResultExtension for ReEvaluate {
    fn on_play_end(&mut self, ctx: &ResultContext<'_>) -> ResultSummary {
        let rec = ctx.record;
        let evaluation = if replay::preset_hash(ctx.preset) == rec.preset.sha256 {
            let input = EvalInput {
                events: rec.events.clone(),
                device: rec.devices.profile(),
                start: rec.start.clone(),
                finalized: Some(rec.status.finalized),
            };
            eval::evaluate(ctx.chart, &input, ctx.preset).map_err(ReEvaluationError::Eval)
        } else {
            Err(ReEvaluationError::PresetMismatch)
        };
        ResultSummary {
            matches_live: evaluation.as_ref().is_ok_and(|e| e.events == ctx.live),
            recommended_offset: evaluation.as_ref().ok().and_then(|e| recommend(&e.events)),
            evaluation,
        }
    }
}

/// The median and interquartile spread of the judged presses' deviations,
/// once there are [`MIN_OFFSET_SAMPLES`] of them. The quartiles are the
/// sorted values at `n / 4`, `n / 2` and `3n / 4`.
fn recommend(events: &[JudgeEvent]) -> Option<RecommendedOffset> {
    let mut d: Vec<i64> = events
        .iter()
        .filter_map(|e| match e.kind {
            JudgeEventKind::Judged {
                deviation: Some(d), ..
            } => Some(d.0),
            _ => None,
        })
        .collect();
    let n = d.len();
    if n < MIN_OFFSET_SAMPLES {
        return None;
    }
    d.sort_unstable();
    Some(RecommendedOffset {
        median: TimeUs(d[n / 2]),
        spread: TimeUs(d[3 * n / 4] - d[n / 4]),
        samples: n,
    })
}
