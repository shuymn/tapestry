//! Pure evaluator.
//!
//! Contract: `(chart, input log, preset) -> judgment event stream`. The result
//! depends only on those three inputs.
//!
//! - No clock, I/O, audio, or rendering: `cargo xtask check-arch` (R1) fails
//!   when the dependency closure of this crate leaves `bms-model`, `rules`,
//!   and a reviewed allowlist. Use of `std` APIs such as `std::time` or
//!   `std::fs` is not covered by that check; it is a reviewed invariant.
//! - No floating point: times are integer microseconds (`i64`) and gauges are
//!   fixed-point. The float lints below are partial checks: they catch float
//!   arithmetic and comparisons, not every use of a float type.
//!
//! The crate also owns the input contract the evaluator reads: physical input
//! events per source, the device profile, the held-input snapshot, the
//! watermark over sources, and the merge of their events into one stream.
//! The live logic thread and batch evaluation call the same [`merge`] and the
//! same [`Evaluator`], so they cannot diverge.

#![forbid(unsafe_code)]
#![deny(clippy::float_arithmetic, clippy::float_cmp)]

mod batch;
mod convert;
mod error;
mod evaluator;
mod gauge;
mod input;
mod merge;

pub use batch::{EvalInput, Evaluation, GaugePoint, GaugeResult, evaluate};
pub use convert::{ConversionState, Edges, LaneEdge};
pub use error::EvalError;
pub use evaluator::{
    AdvanceError, Evaluator, JudgeEvent, JudgeEventKind, LnOutcome, PushError, Tally,
};
pub use gauge::GaugeState;
pub use input::{
    AXIS_REPORTS_PER_SEC, AxisBinding, AxisMode, BUTTON_EDGES_PER_SEC, Binding, ControlId,
    DeviceProfile, InputKind, InputStart, MAX_AXES, MAX_AXES_PER_SOURCE, MAX_BINDINGS,
    MAX_BINDINGS_PER_SOURCE, PhysicalInputEvent, ScratchSettings, SourceId, SourceProfile,
    evaluation_time, window_inputs,
};
pub use merge::{Published, SourceState, StageError, Staged, merge, watermark};
