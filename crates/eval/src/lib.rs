//! Pure evaluator.
//!
//! Contract: `(chart, input log, preset) -> judgment event stream`. The result
//! depends only on those three inputs. The API arrives with stage 1.
//!
//! - No clock, I/O, audio, or rendering: `cargo xtask check-arch` (R1) fails
//!   when the dependency closure of this crate leaves `bms-model`, `rules`,
//!   and a reviewed allowlist. Use of `std` APIs such as `std::time` or
//!   `std::fs` is not covered by that check; it is a reviewed invariant.
//! - No floating point: times are integer microseconds (`i64`) and gauges are
//!   fixed-point. The float lints below are partial checks: they catch float
//!   arithmetic and comparisons, not every use of a float type.

#![forbid(unsafe_code)]
#![deny(clippy::float_arithmetic, clippy::float_cmp)]
