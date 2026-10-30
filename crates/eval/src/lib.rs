//! Pure evaluator.
//!
//! Contract: `(chart, input log, preset) -> judgment event stream`. The result
//! depends only on those three inputs.
//!
//! - No clock, I/O, audio, or rendering: `cargo xtask check-arch` (R1) fails
//!   when the dependency closure of this crate leaves `bms-model`, `rules`,
//!   and a reviewed allowlist. R1 does not cover `std`; for that,
//!   `clippy.toml` beside this crate's manifest rejects the named `std`
//!   entry points for the clock, the file system, the environment, threads
//!   and printing, and `std`'s `HashMap`, `HashSet` and `RandomState`
//!   (per-process random order), denied below. That check is partial: a
//!   `std` item not on the lists, a trait object, or a callback from the
//!   caller can still reach the outside, so purity beyond the lists stays a
//!   reviewed invariant.
//! - No floating point: times are integer microseconds (`i64`) and gauges are
//!   fixed-point. The float lints below are partial checks: they catch float
//!   arithmetic and comparisons, not every use of a float type.
//! - The cast lints below reject `as` casts that may truncate, wrap, or lose
//!   the sign. They cover the library and its unit tests, not the
//!   integration tests.
//! - The modules the live logic thread runs also deny `unwrap`, `expect` and
//!   indexing outside tests; each place where an invariant makes one safe
//!   says so in an `#[expect]` reason (design.md "Scope of No Allocation").
//!
//! The crate also owns the input contract the evaluator reads: physical input
//! events per source, the device profile, the held-input snapshot, the
//! watermark over sources, and the merge of their events into one stream.
//! The live logic thread and batch evaluation call the same [`merge`] and the
//! same [`Evaluator`], so they cannot diverge.

#![forbid(unsafe_code)]
#![deny(clippy::float_arithmetic, clippy::float_cmp)]
#![deny(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
#![deny(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]

mod batch;
mod convert;
mod error;
mod evaluator;
mod gauge;
mod input;
mod merge;

// Lint fixtures under the crate-root lints and `clippy.toml`: `cargo xtask
// check-lints`.
#[cfg(tapestry_lint_fixtures)]
#[path = "../lint-fixtures/crate/mod.rs"]
mod lint_fixtures;

pub use batch::{EvalInput, Evaluation, GaugePoint, GaugeResult, evaluate};
pub use convert::{ConversionState, Edges, LaneEdge};
pub use error::EvalError;
pub use evaluator::{AdvanceError, Evaluator, JudgeEvent, JudgeEventKind, PushError, Tally};
pub use gauge::{GaugeState, accepts_initial};
pub use input::{
    AXIS_REPORTS_PER_SEC, AxisBinding, AxisMode, BUTTON_EDGES_PER_SEC, Binding, ControlId,
    DeviceProfile, InputKind, InputStart, MAX_AXES, MAX_AXES_PER_SOURCE, MAX_BINDINGS,
    MAX_BINDINGS_PER_SOURCE, PhysicalInputEvent, SOURCE_BUTTON_EDGES_PER_SEC, ScratchSettings,
    Section, SourceId, SourceProfile, evaluation_time, window_inputs,
};
pub use merge::{Published, SourceState, StageError, Staged, merge, watermark};
