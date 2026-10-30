//! Batch evaluation of a recorded play, through the same merge and the same
//! [`Evaluator`] as live play.

use bms_model::{Chart, TimeUs};
use rules::{GaugeKind, GaugeValue, RulePreset};

use crate::EvalError;
use crate::evaluator::{Evaluator, JudgeEvent, JudgeEventKind, Tally};
use crate::input::{DeviceProfile, InputStart, PhysicalInputEvent, Section, evaluation_time};
use crate::merge::{Staged, merge};

/// A recorded play, as evaluation needs it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EvalInput {
    /// The physical input events in recording order; each source's events
    /// are in its receive order.
    pub events: Vec<PhysicalInputEvent>,
    /// How each source is evaluated.
    pub device: DeviceProfile,
    /// The controls held when recording opened.
    pub start: InputStart,
    /// For an aborted play, the watermark finalized before the abort;
    /// evaluation stops there, and a long note still held there is not
    /// judged or counted. `None` evaluates to the final deadline.
    pub finalized: Option<TimeUs>,
    /// The section to evaluate (design.md "Section Evaluation"); `None`
    /// evaluates the whole chart.
    pub section: Option<Section>,
}

/// One point of a gauge's history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GaugePoint {
    /// When the gauge changed.
    pub time: TimeUs,
    /// The real value after the change.
    pub value: GaugeValue,
    /// The value had the gauge never failed.
    pub reference: GaugeValue,
}

/// One gauge at the end of evaluation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GaugeResult {
    /// The real value.
    pub value: GaugeValue,
    /// The value had the gauge never failed.
    pub reference: GaugeValue,
    /// When a survival gauge reached 0, if it did.
    pub failed_at: Option<TimeUs>,
    /// Whether the gauge is cleared: evaluation completed and, for a gauge
    /// decided at the end, the value is at or above its threshold, or, for a
    /// survival gauge, it never failed.
    pub cleared: bool,
    /// The value after every event that changed gauges.
    pub history: Vec<GaugePoint>,
}

/// The result of evaluating a play.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evaluation {
    /// Every event, as live play emits them.
    pub events: Vec<JudgeEvent>,
    /// Counts, EX score, BP, combo and empty POORs.
    pub tally: Tally,
    /// Every gauge, in the preset's order.
    pub gauges: Vec<GaugeResult>,
    /// Whether evaluation reached the final deadline of the chart, or of
    /// the section.
    pub completed: bool,
}

/// Evaluates a recorded play: merges its sources' events as live play does,
/// pushes them into an [`Evaluator`] with the input's section, if any, and
/// advances to the finalized watermark of an aborted play, or else past
/// every deadline and input.
///
/// # Errors
///
/// What [`Evaluator::new`] refuses, or an input the evaluator cannot queue.
pub fn evaluate(
    chart: &Chart,
    input: &EvalInput,
    rule: &RulePreset,
) -> Result<Evaluation, EvalError> {
    let merged = merge_log(&input.events, &input.device);
    let mut ev = Evaluator::new(
        chart,
        rule,
        &input.device,
        &input.start,
        input.section.as_ref(),
        merged.len(),
    )?;
    let last_input = merged.last().map(|e| evaluation_time(e, &input.device));
    let inputs = merged.len();
    for e in merged {
        ev.push_input(e).map_err(EvalError::Input)?;
    }
    let to = input
        .finalized
        .unwrap_or_else(|| last_input.map_or(ev.final_deadline(), |t| t.max(ev.final_deadline())));
    let mut events = Vec::with_capacity(ev.max_events(inputs));
    // The first advance cannot be backward.
    let _ = ev.advance_to(to, &mut events);
    let completed = ev.is_complete();
    let mut histories = vec![Vec::new(); rule.gauges.len()];
    for e in &events {
        if let JudgeEventKind::Gauge {
            gauge,
            value,
            reference,
        } = e.kind
            && let Some(h) = histories.get_mut(gauge)
        {
            h.push(GaugePoint {
                time: e.time,
                value,
                reference,
            });
        }
    }
    let gauges = rule
        .gauges
        .iter()
        .zip(ev.gauges())
        .zip(histories)
        .map(|((rule, state), history)| {
            let passed = match rule.kind {
                GaugeKind::EndThreshold { clear } => state.value >= clear,
                GaugeKind::Survival => state.failed_at.is_none(),
            };
            GaugeResult {
                value: state.value,
                reference: state.reference,
                failed_at: state.failed_at,
                cleared: completed && passed,
                history,
            }
        })
        .collect();
    Ok(Evaluation {
        tally: *ev.tally(),
        gauges,
        completed,
        events,
    })
}

/// The log's events in merge order: per source in recording order, merged
/// by (evaluation time, source, order within the source).
fn merge_log(events: &[PhysicalInputEvent], device: &DeviceProfile) -> Vec<PhysicalInputEvent> {
    let mut sources: Vec<_> = events.iter().map(|e| e.source).collect();
    sources.sort_unstable();
    sources.dedup();
    let mut staged: Vec<Staged> = sources
        .iter()
        .map(|&s| Staged::with_capacity(s, events.iter().filter(|e| e.source == s).count()))
        .collect();
    for e in events {
        if let Ok(i) = sources.binary_search(&e.source) {
            // Sized for every event of its source, so it cannot be full.
            let _ = staged[i].push(*e);
        }
    }
    let mut out = Vec::with_capacity(events.len());
    merge(&mut staged, device, TimeUs(i64::MAX), &mut out);
    out
}
