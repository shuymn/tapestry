# tapestry

A Windows-only BMS player (a 7-key + scratch music game). The authoritative design is [docs/design.md](docs/design.md); the environment and tool versions are in [docs/setup.md](docs/setup.md). When changing a design decision, update design.md first.

## Rules

- **Do not write commercial game names:** Do not put the title, series name, or maker name of the commercial game this is based on into identifiers, comments, strings, logs, test names, file names, or documents. Use generic terms such as "7-key", "scratch", and "BMS". The only exception is the banned-word list file.
- **The evaluator is pure:** `eval` determines results solely from the chart, the input log, and the preset. Do not make it depend on clock, I/O, audio, or rendering crates. Times are integer µs (`i64`) and gauges are fixed-point; do not use floating point.
- **Confine Windows-dependent code to `input`, `audio`, `render`, and `player`:** Keep every other crate building and passing tests on Linux.
- **Development features live only in the `dev-tools` feature:** Do not include the code itself in production builds. Do not hide it behind runtime flags.
- **Do not allocate memory in in-play processing:** This covers input, logic, the audio callback, and per-frame rendering work (design.md "Key Libraries and Algorithms").
- **Do not put copyrighted charts or audio in the repository:** Build test charts in test code, or write them as small self-made BMS text.

## Commands

Rust and tool versions are pinned with mise. Invoke `cargo` through `mise exec --` (otherwise you do not get the pinned version).

```sh
mise install
mise exec -- lefthook install
mise exec -- cargo fmt --check
mise exec -- cargo clippy --workspace --all-targets -- -D warnings
mise exec -- cargo test --workspace
mise exec -- cargo xtask portable test
mise exec -- cargo xtask check-arch
```

`cargo xtask portable <command>` runs a cargo command on every crate except the Windows-only ones (what Linux CI builds and tests). `cargo xtask check-arch` checks the rules above that can be checked mechanically.

Linux-side tools (fuzzing, Linux builds) run inside `nix develop` via the WSL wrapper script (docs/setup.md "Linux-Side Tools (WSL)").
