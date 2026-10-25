//! Judgment and gauge rule presets.
//!
//! A [`RulePreset`] holds every number and fixed choice the evaluator reads,
//! plus the name and version of the evaluation implementation. Its
//! [`canonical_bytes`](RulePreset::canonical_bytes) identify it in records.
//! This crate takes no external dependency, so `eval`'s dependency closure
//! stays `bms-model` and `rules`; reading preset files lives in `game`.
//!
//! Times are integer µs and gauges are fixed-point; this crate does not use
//! floating point. The float lints below are partial checks: they catch float
//! arithmetic and comparisons, not every use of a float type.

#![forbid(unsafe_code)]
#![deny(clippy::float_arithmetic, clippy::float_cmp)]

mod canonical;
mod custom;
mod error;
mod preset;
mod validate;

pub use custom::custom;
pub use error::{RuleError, Side};
pub use preset::{
    Assignment, Denominator, EmptyPoorRule, GaugeDelta, GaugeKind, GaugeRule, GaugeValue,
    Implementation, JudgeWindows, Judgment, LateHeadRule, LnBreakRule, LnMode, LnModeRule,
    LowDamage, MineRule, MissRule, PerJudgment, RankScale, RulePreset, SCALE_FULL_MILLI,
    SameTimeOrder, ScoreWeights, TotalMode, Window,
};
