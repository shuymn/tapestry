//! Lint fixtures for the crate-root lints of `eval` and its `clippy.toml`
//! (`cargo xtask check-lints`). Each file violates only the lint it is
//! named after, under the real configuration; none carries a lint
//! attribute. Compiled only with `--cfg tapestry_lint_fixtures`.

mod cast_possible_truncation;
mod cast_possible_wrap;
mod cast_sign_loss;
mod disallowed_macros;
mod disallowed_methods;
mod disallowed_types;
