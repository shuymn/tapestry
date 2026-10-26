//! Lint fixtures for the workspace lints (`cargo xtask check-lints`). Each
//! file violates only the lint it is named after, under the real
//! configuration in the workspace `Cargo.toml`; none carries a lint
//! attribute. Compiled only with `--cfg tapestry_lint_fixtures`.

mod dbg_macro;
mod todo;
mod undocumented_unsafe_blocks;
mod unimplemented;
