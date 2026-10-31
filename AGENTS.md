# tapestry

A Windows-only BMS player (a 7-key + scratch music game). The authoritative design is [docs/design.md](docs/design.md); the environment and tool versions are in [docs/setup.md](docs/setup.md). When changing a design decision, update design.md first.

## Rules

- **Do not write commercial game names:** Do not put the title, series name, maker name, or online service name of the commercial game this is based on into identifiers, comments, strings, logs, test names, file names, or documents. Use generic terms such as "7-key", "scratch", and "BMS". The only exception is the banned-word list file.
- **The evaluator is pure:** `eval` determines results solely from the chart, the input log, and the preset. Do not make it depend on clock, I/O, audio, or rendering crates. Times are integer µs (`i64`) and gauges are fixed-point; do not use floating point.
- **Confine Windows-dependent code to `input`, `audio`, `render`, and `player`:** Keep every other crate building and passing tests on Linux.
- **Development features live only in the `dev-tools` feature:** Do not include the code itself in production builds. Do not hide it behind runtime flags.
- **Do not allocate memory in in-play processing:** This covers input, logic, the audio callback, and per-frame rendering work (design.md "Key Libraries and Algorithms"). That code also uses no `unwrap`, `expect` or indexing unless an `#[expect]` states the invariant that makes it safe; a new in-play module or function goes into `IN_PLAY` in `xtask/src/lints.rs`.
- **The license follows the layer:** L0–L2 crates, `eval-cli`, and `xtask` are `MIT OR Apache-2.0`; L3 and above, `player`, and `fuzz` (it links `input`) are `GPL-3.0-or-later`. Each crate states its license in its own `Cargo.toml`, and a permissive crate never depends on a GPL crate (design.md "Licenses").
- **No compatibility code before the first release:** Until the first release, every format (records, presets, settings, canonical encodings) may change incompatibly. Bump its version and refuse older versions with a clear error; never convert them, and keep no code that reads or evaluates them.
- **Do not put copyrighted charts or audio in the repository:** Build test charts in test code, or write them as small self-made BMS text.

## Commands

Rust and tool versions are pinned with mise. Invoke `cargo` through `mise exec --` (otherwise you do not get the pinned version).

```sh
mise install
mise exec -- lefthook install
mise exec -- cargo fmt --check
mise exec -- cargo clippy --workspace --all-targets -- -D warnings
mise exec -- cargo test --workspace
mise exec -- cargo clippy -p player -p game --all-targets --features player/dev-tools,game/dev-tools -- -D warnings
mise exec -- cargo test -p player -p game --features player/dev-tools,game/dev-tools
mise exec -- cargo xtask portable test
mise exec -- cargo xtask check-arch
mise exec -- cargo xtask check-lints
```

The two `dev-tools` commands build and test the development features, which the workspace commands leave out. `cargo xtask portable <command>` runs a cargo command on every crate except the Windows-only ones (what Linux CI builds and tests). `cargo xtask check-arch` checks the rules above that can be checked mechanically. `cargo xtask check-lints` checks that the lints enforcing them are configured (workspace lints, `eval`'s `clippy.toml`, the crate-root cast lines, the in-play attributes) and still fire on one fixture per lint.

Linux-side tools (fuzzing, Linux builds) run inside `nix develop` via the WSL wrapper script (docs/setup.md "Linux-Side Tools (WSL)"). The fuzz crate `fuzz/` is outside the workspace: check it with `--manifest-path fuzz/Cargo.toml` (fmt, clippy, test); running the fuzzer is in docs/setup.md "Fuzzing".
