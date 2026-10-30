//! The results extension point and its built-in extension, re-evaluation
//! of the saved record.

use bms_model::{Chart, TimeUs};
use eval::{EvalError, Evaluation, JudgeEvent, JudgeEventKind};
use replay::PlayRecord;
use replay::verify::ReEvaluation;
use rules::{Implementation, RulePreset};

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
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReEvaluationError {
    /// The record was made by an evaluator version this build does not
    /// run, so its rules are not this build's.
    #[error("the record was made by evaluator version {0}, which this build does not run")]
    UnsupportedEvaluator(u32),
    /// The record names a preset other than the one the play ran under.
    #[error("the record names another preset")]
    PresetMismatch,
    /// The evaluator refused the record.
    #[error("the record cannot be evaluated")]
    Eval(#[source] EvalError),
}

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

/// Re-evaluates the saved record of the chart just played: at the end of
/// a play under its own preset, so the end screen's numbers come from the
/// log, and under any other preset with [`under`](Self::under). The chart
/// is the one played, so the record is not verified against a reloaded
/// chart (design.md "Determinism").
#[derive(Clone, Copy, Debug, Default)]
pub struct ReEvaluate;

impl ReEvaluate {
    /// Re-evaluates the record of `ctx` on its chart under `preset`
    /// (design.md "Extension Boundaries"): as played when `preset` is the
    /// record's own, through the recorded watermark; otherwise a what-if,
    /// to the preset's own final deadline after a completed play, or to the
    /// recorded watermark, without a clear, after an aborted one.
    ///
    /// # Errors
    ///
    /// [`UnsupportedEvaluator`](ReEvaluationError::UnsupportedEvaluator),
    /// checked first, or what the evaluator refuses.
    pub fn under(
        ctx: &ResultContext<'_>,
        preset: &RulePreset,
    ) -> Result<ReEvaluation, ReEvaluationError> {
        check_evaluator(ctx.record)?;
        replay::verify::evaluate_under(ctx.record, ctx.chart, preset)
            .map_err(ReEvaluationError::Eval)
    }
}

/// Refuses a record of an evaluator version this build does not run.
fn check_evaluator(rec: &PlayRecord) -> Result<(), ReEvaluationError> {
    if rec.versions.evaluator == Implementation::STANDARD_VERSION {
        Ok(())
    } else {
        Err(ReEvaluationError::UnsupportedEvaluator(
            rec.versions.evaluator,
        ))
    }
}

impl ResultExtension for ReEvaluate {
    fn on_play_end(&mut self, ctx: &ResultContext<'_>) -> ResultSummary {
        let rec = ctx.record;
        // The preset the play ran under is the record's, so another one is
        // a mismatch, not a what-if. The evaluator version still comes
        // first: `under` checks it, and a record of another version is
        // refused whatever preset it names.
        let evaluation = if replay::preset_hash(ctx.preset) == rec.preset.sha256 {
            Self::under(ctx, ctx.preset).map(|r| r.evaluation)
        } else {
            check_evaluator(rec).and(Err(ReEvaluationError::PresetMismatch))
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

#[cfg(test)]
mod tests {
    use bms_model::{Lane, TimeUs};
    use eval::{JudgeEvent, JudgeEventKind};
    use rules::Judgment;

    use super::{MIN_OFFSET_SAMPLES, RecommendedOffset, recommend};

    fn judged(deviation: Option<i64>) -> JudgeEvent {
        JudgeEvent {
            seq: 0,
            time: TimeUs(0),
            origin: None,
            kind: JudgeEventKind::Judged {
                lane: Lane::SCRATCH,
                note: 0,
                judgment: Judgment::Great,
                deviation: deviation.map(TimeUs),
                release: None,
                combo: 0,
            },
        }
    }

    #[test]
    fn recommendation_needs_enough_presses_and_takes_median_and_quartiles() {
        // Deviations -15 to +16 ms, out of order, plus a miss and an event
        // that is not a judgment, which do not count.
        let mut events: Vec<JudgeEvent> = (0..32)
            .map(|i| judged(Some((((i * 13) % 32) - 15) * 1_000)))
            .collect();
        events.push(judged(None));
        events.push(JudgeEvent {
            kind: JudgeEventKind::SongEnd,
            ..judged(None)
        });
        assert_eq!(MIN_OFFSET_SAMPLES, 32);
        // Sorted, value i is (i − 15) ms: the median is value 16, the
        // quartiles values 8 and 24.
        assert_eq!(
            recommend(&events),
            Some(RecommendedOffset {
                median: TimeUs(1_000),
                spread: TimeUs(9_000 - -7_000),
                samples: 32,
            })
        );
        events.remove(0);
        assert_eq!(recommend(&events), None);
    }
}
