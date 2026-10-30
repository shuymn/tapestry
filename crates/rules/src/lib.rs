//! Judgment and gauge rule presets.
//!
//! A [`RulePreset`] holds every number and fixed choice the evaluator reads,
//! plus the name and version of the evaluation implementation. Its
//! [`canonical_bytes`](RulePreset::canonical_bytes) identify it in records.
//! This crate takes no external dependency, so `eval`'s dependency closure
//! stays `bms-model` and `rules`; reading preset files lives in `rules-toml`.
//!
//! Times are integer µs and gauges are fixed-point; this crate does not use
//! floating point. The float lints below are partial checks: they catch float
//! arithmetic and comparisons, not every use of a float type. The cast lints
//! below reject `as` casts that may truncate, wrap, or lose the sign; they
//! cover the library and its unit tests, not integration tests.

#![forbid(unsafe_code)]
#![deny(clippy::float_arithmetic, clippy::float_cmp)]
#![deny(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

mod beatoraja_style;
mod canonical;
mod custom;
mod error;
mod headers;
mod preset;
mod validate;
mod windows;

pub use beatoraja_style::beatoraja_style;
pub use custom::custom;
pub use error::{CanonicalError, RuleError, Side};
pub use headers::{HeaderPolicy, JudgeRank, ReadHeaders};
pub use preset::{
    Assignment, Denominator, EmptyPoorRule, GaugeDelta, GaugeKind, GaugeRule, GaugeValue,
    Implementation, JudgeWindows, Judgment, LateHeadRule, LnEndWindows, LnMode, LnModeRule,
    LowDamage, MAX_WINDOW_EDGE, MineRule, MissRule, PerJudgment, RankScale, RulePreset,
    SCALE_FULL_MILLI, SameTimeOrder, ScoreWeights, TotalMode, Window, scale_edge,
};
pub use windows::{
    EmptyPoorRange, LaneClass, LaneTable, PerLaneClass, PreparedTable, Reach, ScaledRank,
    ScanEnvelope, WindowPolicy,
};
