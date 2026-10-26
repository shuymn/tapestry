//! Lint fixtures for the in-play module lints, compiled inside the
//! evaluator module under its real attribute (`cargo xtask check-lints`).
//! Each file violates only the lint it is named after; none carries a lint
//! attribute. Compiled only with `--cfg tapestry_lint_fixtures`.

mod expect_used;
mod indexing_slicing;
mod unwrap_used;
