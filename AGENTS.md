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
mise exec -- cargo clippy -p player -p game -p skin-beatoraja --all-targets --features player/dev-tools,game/dev-tools,skin-beatoraja/dev-tools -- -D warnings
mise exec -- cargo test -p player -p game -p skin-beatoraja --features player/dev-tools,game/dev-tools,skin-beatoraja/dev-tools
mise exec -- cargo xtask portable test
mise exec -- cargo xtask check-arch
mise exec -- cargo xtask check-lints
mise exec -- cargo xtask check-alloc-tests
```

The two `dev-tools` commands build and test the development features, which the workspace commands leave out. `cargo xtask portable <command>` runs a cargo command on every crate except the Windows-only ones (what Linux CI builds and tests). `cargo xtask check-arch` checks the rules above that can be checked mechanically. `cargo xtask check-lints` checks that the lints enforcing them are configured (workspace lints, `eval`'s `clippy.toml`, the crate-root cast lines, the in-play attributes) and still fire on one fixture per lint. `cargo xtask check-alloc-tests` checks that every allocation test (an integration test named `alloc` or `*_alloc`; other allocation measurements take other names, such as `alloc_order`) runs again in CI's release allocation step, with its crate in that step's `-p` list. `player`'s counting allocator exists only with `dev-tools`, so its allocation tests are unit tests: every `#[test]` in `crates/player/src` that reads `alloc_count` must be named `*_alloc`, and each must be selected by CI's "Player allocation guards (release)" step, a command of exactly the form `cargo test --release --locked -p player --features dev-tools --bin tapestry -- <filters>` (only `--exact` and `--skip` besides the filters).

Linux-side tools (fuzzing, Linux builds) run inside `nix develop` via the WSL wrapper script (docs/setup.md "Linux-Side Tools (WSL)"). The fuzz crate `fuzz/` is outside the workspace: check it with `--manifest-path fuzz/Cargo.toml` (fmt, clippy, test); running the fuzzer is in docs/setup.md "Fuzzing".

On Linux, the Windows-only crates can be checked (not tested or run) by cross-compiling with [cargo-xwin](https://github.com/rust-cross/cargo-xwin), which mise installs on Linux only. It needs the `x86_64-pc-windows-msvc` target (`mise exec -- rustup target add x86_64-pc-windows-msvc`) and, on `PATH` outside mise and the Nix shells, `clang` (run as `clang-cl` for the bundled C code), `llvm-lib` and `lld-link` (Debian and Ubuntu packages `clang`, `llvm` and `lld`). On first use it downloads the MSVC CRT and Windows SDK under Microsoft's license (set `XWIN_ACCEPT_LICENSE=1`, or pre-cache them with `XWIN_ACCEPT_LICENSE=1 mise exec -- cargo xwin cache xwin`):

```sh
mise exec -- cargo xwin clippy --workspace --all-targets --target x86_64-pc-windows-msvc -- -D warnings
mise exec -- cargo xwin clippy -p player -p game -p skin-beatoraja --all-targets --features player/dev-tools,game/dev-tools,skin-beatoraja/dev-tools --target x86_64-pc-windows-msvc -- -D warnings
```
