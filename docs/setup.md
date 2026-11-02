# Development Environment

The environment for developing this repository, and how tool versions are pinned. The design is in [design.md](design.md).

## Requirements

| Item | Details |
| --- | --- |
| OS | Windows 10/11 (x64). Building, running, and measuring the game are done natively on Windows |
| Git | Git for Windows |
| C++ build environment | Visual Studio Build Tools (C++ workload, MSVC toolset, Windows SDK) |
| Version pinning | [mise](https://mise.jdx.dev/). Rust and auxiliary CLI versions are pinned in `mise.toml` and `mise.lock` |
| Linux-side tools (optional) | NixOS on WSL (or a Linux with Nix installed). Used for fuzzing, Linux builds and tests of crates that do not depend on Windows, and, from the stage that adds it, checking the WASM build. CI performs the same checks, so development is possible without it locally |

## Repository Location

- Place the working copy at a short path (e.g. `C:\dev\tapestry`). Set `core.longpaths=true` in Git and also enable long paths in the OS. The OS setting alone does not make every tool support long paths:
  - Native programs (Git, mise, cargo) cannot be started from a current directory longer than 260 characters.
  - The MSVC linker (`link.exe`) cannot open paths over 260 characters (`LNK1104`), even with long paths enabled.
  - Keep every path under the working copy, including `target/`, below 260 characters, with margin for new dependencies. With a 94-character root the deepest path was 217 characters and everything built; with a 201-character root it was 333 characters and build scripts failed to link.
- Line endings are determined by `.gitattributes`.
- From WSL, use the same working copy as `/mnt/c/...`. So that worktrees created on Windows can also be read from WSL, set `git config --global worktree.useRelativePaths true` before creating worktrees (both the Windows-side and the WSL-side Git must be versions that support this setting). Git operations that change the working copy (commits, branch switches, etc.) are done only on the Windows side. Do not use file change watching across Windows and WSL.

## Version Pinning

Use mise 2026.10.3 or newer. CI runs the minimum supported mise version.

Tool versions are pinned with mise. `mise.toml` specifies major-version ranges, and the exact versions are pinned in `mise.lock`. Configure `mise.toml` so that versions not in `mise.lock` are not used:

```toml
[tool_config]
locked = true

[settings]
lockfile_platforms = ["windows-x64", "linux-x64", "macos-arm64"]
```

`locked` in `[tool_config]` applies only to tools declared in this repository (it does not apply to global tools).
Updates to `mise.lock` are submitted as PRs by a periodically running workflow ("Lock Updates" below).

### Rust

The Rust version is also pinned with mise. Do not add a language-specific version file (`rust-toolchain.toml`).

```toml
[tools]
rust = { version = "1", profile = "minimal", components = "rustfmt,clippy", targets = "wasm32-unknown-unknown" }
```

- mise installs Rust using rustup and selects the version by setting `RUSTUP_TOOLCHAIN`. So calling `cargo` without going through mise does not give you the version pinned in this repository. Call it as `mise exec -- cargo ...`, or work in a shell with mise activated.
- On both Windows and Linux (locally and in CI), mise selects the version in `mise.lock` with the `minimal` profile plus `rustfmt` and `clippy` and the `wasm32-unknown-unknown` target.
- Without mise, install the version written in `mise.lock` with rustup and call `cargo +<version> ...`.
- `targets` adds `wasm32-unknown-unknown` for the crates the IR server runs as WASM (`eval`, `bms-parser`, `replay`; design.md "Server Architecture (Cloudflare)"). `zstd-sys` (through `replay`) compiles C for that target with `clang`: Apple clang works on macOS, and Linux needs a `clang` on `PATH` (the Ubuntu runner image has one). Other targets are added in the stage that uses them.
- Clippy reads the nearest `clippy.toml`: the workspace root's sets the test allowances, and `crates/eval/clippy.toml` repeats them and adds the evaluator's disallowed `std` APIs. `cargo xtask check-lints` checks that the two agree.

### Hooks

[lefthook](https://lefthook.dev/) runs the git pre-commit hook. lefthook itself is also pinned with mise.

```sh
mise install
mise exec -- lefthook install
```

The pre-commit hook runs the banned-word check and the `rustfmt` check (of the workspace and of the fuzz crate). The hook is for noticing problems early locally; CI is the final gate.

### Lock Updates

`.github/workflows/mise-update.yml` runs daily on `main` (and on manual dispatch):

- `mise lock --bump` re-resolves the ranges in `mise.toml` to the newest versions allowed by `minimum_release_age`. Only tools whose version changes are re-locked, so checksums of unchanged versions stay pinned. It reads release metadata and writes the lockfile; it does not install or run the tools.
- When Rust changes, it also runs `nix flake update rust-overlay` and evaluates both dev shells, which fails if rust-overlay does not know the new version or no longer has the fuzz shell's dated nightly.
- It fails if any file other than `mise.lock` and `flake.lock` changed.
- The result is a signed commit on the `mise-update` branch, with one open PR from it (a From/To table, auto-merge with a merge commit). A branch that already proposes the same lockfiles is kept, and its PR is reused or opened again; otherwise the branch is recreated from `main`, which closes the stale PR. To keep a tool at its version, write the exact version in `mise.toml` with a comment saying why. Major updates are manual: change the range in `mise.toml` and run `mise lock`.

PRs created with `GITHUB_TOKEN` do not run CI unattended, so auto-merge would never get checks. The workflow opens the PR with a GitHub App token instead, which needs:

- A GitHub App installed on this repository only, with the repository permissions Contents (read and write) and Pull requests (read and write).
- The repository variable `APP_CLIENT_ID` (the App's client ID) and the secret `APP_PRIVATE_KEY` (a private key of the App).
- "Allow auto-merge" and the CI jobs required on `main` (auto-merge waits only for required checks). Both are declared in `.github/infra.yaml`.

## Linux-Side Tools (WSL)

- Linux-side tools are pinned in the development shells of a Nix flake (`flake.nix` and `flake.lock`). In the `default` shell the Rust version is read from `mise.lock`, so WSL uses the same `rustc` as Windows. The `fuzz` shell instead has a dated nightly (`fuzzNightly` in `flake.nix`, from the locked rust-overlay, bumped by hand, for example when stable passes it) and `cargo-fuzz` from the locked nixpkgs ("Fuzzing" below). The nightly is used only there.
- On NixOS, enable `nix-command` and `flakes`. The Nix in WSL must be linked to libgit2 1.9.4 or later: `worktree.useRelativePaths` enables the `relativeWorktrees` repository extension, and older libgit2 refuses to open the repository.
- A flake reads only files tracked by Git. Stage (`git add`) a new `flake.nix` input, or a file the flake reads, before using it.
- Build output in WSL is separated into a directory inside WSL via `CARGO_TARGET_DIR` (`~/.cache/tapestry/target/<hash of the working copy root>`; `fuzz-target/` in the `fuzz` shell, which also sets `TAPESTRY_FUZZ_CORPUS` to `fuzz-corpus/<hash>`). This keeps it from mixing with the Windows-side output and between working copies, and avoids slow writes over `/mnt/c`.
- Work in WSL directly on the `/mnt/c` working copy. With the target directory inside WSL, `cargo test --workspace` (crates that do not depend on Windows) after `cargo clean` took 1.05 times as long as on a copy of the same commit inside WSL (median of three alternating runs, 4.0 s against 3.8 s). Revisit this if the ratio exceeds 1.5; heavy work would then use a copy inside WSL.
- To call WSL commands from the Windows side, use `scripts/wsl.sh <command> [args...]` from Git Bash. It runs `wsl.exe -d NixOS --exec /bin/sh`, which converts the working copy's path with `wslpath` inside WSL, changes to the working copy root (the wrapper always runs there, whatever the current directory), and runs `nix develop ".#<shell>" --command <command> [args...]`. Arguments are passed verbatim, without being parsed again by a shell, and the exit code is the command's (125 if the working copy cannot be entered). `<shell>` is `TAPESTRY_NIX_SHELL` (`default` unless set, or `fuzz`), and `TAPESTRY_WSL_DISTRO` selects another distro. On Linux the wrapper runs the same `nix develop` directly. `scripts/wsl-selftest.sh` checks the wrapper with both shells.
- From PowerShell, call it through Git's `bash.exe` (`& "$env:ProgramFiles\Git\bin\bash.exe" scripts/wsl.sh ...`). Plain `bash` may resolve to WSL's `bash.exe`. Windows PowerShell 5.1 drops empty arguments and strips embedded quotes when calling native programs, so run commands with such arguments from Git Bash.

### Fuzzing

The fuzz targets are in `fuzz/`, a crate outside the workspace with its own `fuzz/Cargo.lock`, built with `cargo-fuzz` in the `fuzz` shell. Each target is one function of `fuzz/src/lib.rs`:

| Target | Input |
| --- | --- |
| `bms_load` | A chart file's bytes, loaded with `bms_parser::load_bms` against an empty folder |
| `replay_file` | An input log file's bytes, decoded with `replay::decode` |
| `replay_body` | An uncompressed body, decoded from a valid file built around it, so every input reaches the sections, counts and varints. A body that decodes must be the canonical body of its record |
| `raw_input` | A block count byte, then a Raw Input buffer: the block walk and the keyboard and HID decoders of `input::raw` |
| `preset_canonical` | A preset's canonical bytes, decoded with `RulePreset::from_canonical_bytes`. Bytes that decode must be the canonical bytes of their preset |
| `skin_json` | Up to four JSON skin definitions separated by NUL bytes, written to a temporary skin folder as the entry file and three it may include: `skin_beatoraja::read_header`, then `resolve` with every option item enabled and again with none, which must give the first header back. Every header must be within its caps. A configured pass that resolves is imported with the header's defaults (`skin_beatoraja::import`), whose counters must be within the model's caps, and an imported skin is drawn at a few times within its quad capacity. The skin folder also holds a bitmap font, `font.fnt` with its page `font_0.png` |
| `skin_fnt` | A bitmap `.fnt` file's bytes, parsed with `skin_model::FntFile::parse` at one file's caps. A parsed font must be within its caps, with its glyph and kerning tables sorted and unique, its glyphs on its pages, and every value a finite number of ems |

- Seeds are generated from the public types when a run starts (`fuzz/src/seeds.rs`), so they follow format changes and no seed bytes are committed; `bms_load` also reads the self-made charts in `crates/bms-parser/tests/charts/` and the dictionary `fuzz/dict/bms.dict`; `skin_json` imports what resolves and draws it, with the dictionary `fuzz/dict/skin.dict`; `skin_fnt` uses `fuzz/dict/fnt.dict`. `cargo test --manifest-path fuzz/Cargo.toml` checks that the seeds reach what each target is for: successful decodes, and each malformed seed failing with the error it was built for.
- Every run has the same per-input limits: 10 s (`-timeout`), 2048 MiB of RSS and of one allocation, inputs up to 64 KiB (4 KiB for `raw_input`). The targets are built with AddressSanitizer and debug assertions, so overflow checks are on.
- `scripts/fuzz.sh` holds the flags and refuses a duration that is not a whole number of seconds from 1 to 1800 (a run must fit in `fuzz.yml`'s job). Run it in the `fuzz` shell, for example `TAPESTRY_NIX_SHELL=fuzz scripts/wsl.sh scripts/fuzz.sh run replay_body 60`. `run-all <seconds>` runs every target, one after another, or with `--jobs N` up to N at once, each run's output printed whole when it ends (CI's `nix-fuzz` passes the runner's core count, since each libFuzzer run uses one core); `repro <target> <file>` runs one input once, and `build` builds the targets and fails if `fuzz/Cargo.lock` is stale. Corpora collect under `TAPESTRY_FUZZ_CORPUS` (or `--corpus`); minimize an input with `cargo fuzz tmin --fuzz-dir fuzz <target> <file>` in the shell.
- A failing run writes its inputs to a fresh `artifacts/<target>-<time>-<pid>/` under the corpus root and prints them in base64 (at most four, each at most 64 KiB; the others by size and SHA-256), so a CI log is enough to reproduce a failure.
- The `canary` target proves the limits are in force: `scripts/fuzz-selftest.sh` runs a panic, a hang, one 128 MiB allocation and 512 MiB of held memory through `scripts/wsl.sh`, each with only the limit it must trip lowered, and requires a non-zero exit, libFuzzer's diagnostic and no other error class, and the reproducer.

## Property Tests

The property tests (`crates/eval/tests/props.rs`, `crates/replay/tests/round_trip.rs`, `crates/rules/tests/canonical.rs`) are ordinary tests: `cargo test` runs them on every runner with proptest's 256 cases per property and a random seed.

- For a longer local run, set `PROPTEST_CASES`, for example `PROPTEST_CASES=4096 mise exec -- cargo test --test props`.
- A failure prints the shrunk input. proptest saves its seed beside the test (`*.proptest-regressions`, ignored by git) and replays it first on the next local run. Turn the case into a named hand-written test in the regular test files; regression files are not committed, since a generator change makes their seeds mean other cases.
- `generators_reach_the_fragile_spots`, `generator_reaches_ten_byte_varints_and_empty_lists` and `generator_reaches_every_variant_and_length_class` sample the generators with a fixed seed and fail when a generator change stops reaching a case a property depends on.

## CI

| Runner | What it does |
| --- | --- |
| Windows (label with an explicit OS version) | Production build (pinned to a configuration with `dev-tools` disabled), verifying that development features are not included, the render benchmark run once, the render staging allocation test in the release profile, rendering image comparison with WARP, Windows tests, including the scripted plays' check of the render thread's app region allocations (`dev-tools`) |
| Linux | Builds and tests of crates that do not depend on Windows, the `eval`, `bms-parser` and `library` benchmarks run once, the allocation tests of `eval`, `input`, `game` and `audio` again in the release profile, the fuzz crate's format, clippy and seed smoke tests, the release build of `eval`, `bms-parser` and `replay` for `wasm32-unknown-unknown` (the `wasm` job), banned-word check, lint fixtures (`cargo xtask check-lints`), dependency checks of both lockfiles (`cargo-deny`, `cargo-audit`, `cargo-machete`; "Dependency Checks" below), tests in the Nix development shell through `scripts/wsl.sh`, and the fuzz self-test and short fuzzing runs (60 s per target, a fixed libFuzzer seed, in the `fuzz` shell) |

- A pull request or push to `main` that changes only documentation (files under `docs/` and Markdown files, a rename counting as a deletion and an addition) skips the Windows, `linux`, `wasm` and Nix jobs; the `changes` job decides, from the merge commit's first parent for a pull request and from the previous `main` for a push. A skipped job passes as a required check. The `lint` job runs on every change and checks banned words, so documentation is still checked. A manual run, or a range the job cannot read, runs everything.
- The Windows checks run as three jobs in parallel, each building on its own runner: `windows-test` (format, clippy, tests, architecture rules), `windows-release` (the render benchmark run once, the production build and its checks, the render staging allocation test in the release profile) and `windows-dev-tools` (clippy, the positive control and the tests with `dev-tools`). The required check is `windows`, a job that passes when all three passed, or when `changes` classified the change as documentation only and all three were skipped.
- The `nix` checks run the same way as two jobs: `nix-test` (the tests in the development shell and the wrapper self-test) and `nix-fuzz` (the fuzz build, self-test and short runs), with `nix` the required check over both.
- Caches are saved only by runs on `main`; a pull request restores `main`'s and saves none, so caches are not duplicated per pull request within the repository's 10 GB. `jdx/mise-action` caches the tools. `Swatinem/rust-cache` caches the dependencies' build output of the Windows jobs and of `linux` (the workspace and the fuzz crate), each job's cache apart from the others', keyed by the toolchain and the lockfiles, so a toolchain or lockfile update starts a new entry. It keeps the dependencies' build output, not the workspace crates' own. The Windows debug jobs build with line tables only (`CARGO_PROFILE_DEV_DEBUG`/`CARGO_PROFILE_TEST_DEBUG`), which keeps backtraces' files and lines and their cache small. `bench.yml` restores, without saving, the caches of `windows-release` and `linux` (whose "Benchmarks run once" builds the same bench-profile dependencies) into the head's `target` and a copy for the merge base's directory. The other jobs build without a Rust cache.
- Both runners install tools from the same `mise.lock` (`jdx/mise-action`). Run the install step even after restoring the cache, and verify the Rust version in use.
- `.github/workflows/fuzz.yml` fuzzes each target for 900 s daily (and on manual dispatch with another duration), with the same per-input limits, starting from the generated seeds; pull requests that change the workflow, `scripts/fuzz.sh` or `flake.nix` run it for 60 s. It is not a required check. Its scheduled runs notify and are disabled after 60 idle days as `audit.yml`'s are ("Dependency Checks" below).
- GitHub Actions are pinned by commit SHA.
- Windows runner images keep being updated even when the version is specified by label. Record the runner image version, the adapter used, and the available features. When an image update changes image comparison results, review the diff images before updating the expected images. A failed comparison writes the drawn image and a diff (differences in magenta) to `target/tmp/expected/`; after review, running the test with `TAPESTRY_UPDATE_EXPECTED=1` writes its expected image to `crates/render/tests/expected/`. The menu text test (`crates/render/tests/text.rs`) also pins its font file, `YuGothM.ttc`, by SHA-256: when the runner's file differs it fails naming both hashes, and the pin and the expected image are updated together after review.
- On the `windows-2025` runner, a WARP device can be created explicitly (D3D12, `IDXGIFactory4::EnumWarpAdapter`) and a rendered target read back exactly. Verified on image `win25-vs2026 20261021.278.1` (the `windows-2025` label currently maps to the Visual Studio 2026 image): Microsoft Basic Render Driver, driver 10.0.26100.33648, maximum feature level 12_1.
- Repository settings (merge methods, the `main` rulesets with the required CI checks, allowed Actions, security features) are declared in `.github/infra.yaml`. Change them there and run `mise exec -- gh-infra plan .github/infra.yaml`, then `apply`.

### Benchmarks

CPU-side time ([design.md](design.md), "Measurement") is measured with `criterion` (default features off, `cargo_bench_support` on; a dev-dependency pinned in the root `Cargo.toml`). The benchmarks build their charts and input in code; no chart file is in the repository.

| Crate | Benchmark file | What it measures |
| --- | --- | --- |
| `eval` | `benches/evaluate.rs` | `evaluate` over generated 2,000- and 10,000-note charts with dense input, and the same plays advanced live in 1 ms steps |
| `bms-parser` | `benches/load.rs` | `load_bms` on generated text with thousands of BPM changes |
| `audio` | `benches/loudness.rs` | The loudness analysis of a dense minute (64 voices sounding throughout) at 48 kHz, mixed and metered (`analyze_one_minute_dense`) |
| `library` | `benches/scan.rs` | `read_header` on a dense generated chart, a full scan of 300 generated chart files into a new database, and searches and sorts over an index of 50,000 generated header-only charts (one scanned, the rest written straight into the database, since writing and scanning 50,000 files takes minutes) |
| `skin-model` | `benches/build.rs` | `Skin::build` of a skin of just the lanes on the render benchmark's dense chart and frame, so the two compare (`skin_model_build_dense_frame`), and of a skin of widgets only: numbers, a flicker gauge and a judgment display with its combo (`skin_model_build_widgets`) |
| `render` (Windows only) | `benches/skin.rs` | `SimpleSkin::build` on a dense frame, without a GPU |

- Run them with `mise exec -- cargo bench -p eval -p bms-parser -p library -p skin-model -p audio` and `mise exec -- cargo bench -p render`. Add `-- --test` to run each once without measuring, as CI does so they do not rot. To compare two commits locally, run `cargo bench -p eval -p bms-parser -p library -p skin-model -p audio --bench evaluate --bench load --bench scan --bench build --bench loudness -- --save-baseline <name>` on one and `-- --baseline <name>` on the other (naming the benches keeps the unit-test harnesses, which do not take criterion's options, out of the run).
- `.github/workflows/bench.yml` runs the benchmarks on the merge base and then on the pull request's head, `eval`, `bms-parser`, `library`, `skin-model` and `audio` in a Linux job and `render` in a Windows job (each pair on one runner, the merge base built into a target directory of its own so the head is really rebuilt), and prints criterion's change estimates (and puts them in the job summary). It runs on pull requests that touch `crates/`, the manifests or the workflow, and on manual dispatch. It is not a required check and does not fail the run, even when a benchmark fails. A benchmark the merge base lacks runs without a comparison (`--baseline-lenient`), and results captured before a failure are still printed. If the merge base has no benchmarks yet, it only says so.
- A pull request that changes a benchmark's workload compares different work, so its change estimates are not meaningful.
- Measurements on shared runners fluctuate, so a change estimate flags a large regression for a reviewer to look at; absolute values are tracked on the development PC ("Measurement Conditions").

### Dependency Checks

- `cargo deny check` checks the graph configured in `deny.toml` (`[graph] targets`, all features). Its `[advisories]` section makes vulnerabilities, unmaintained and unsound advisories against any crate in that graph, and yanked versions, errors (the pinned cargo-deny defaults check unsound advisories only for direct dependencies of workspace crates and only warn on yanked versions).
- The fuzz crate has its own graph and lockfile, so CI and `audit.yml` run each tool on it too: `cargo deny --manifest-path fuzz/Cargo.toml --config deny.toml check` and `cargo audit --file fuzz/Cargo.lock`. `NCSA` is allowed for `libfuzzer-sys` (LLVM libFuzzer), which only the fuzz crate uses; the workspace crates it links (GPL `input` included) are skipped as private crates, as in the workspace.
- `cargo audit --deny warnings` checks every package in `Cargo.lock` and fails on vulnerabilities and on unmaintained, unsound and yanked warnings. Informational `notice` advisories are printed but do not fail it. Both tools stay because their scopes and defaults differ. cargo-audit does not read `deny.toml`, so `.cargo/audit.toml` repeats each advisory exception of its `[advisories] ignore`, with the same reason and removal condition.
- When either tool cannot read the registry index, it skips the yanked check for the affected packages and only warns. The CI and `audit.yml` steps turn that into a failure (`--deny index-failure` for cargo-deny, a scan of stderr for cargo-audit).
- `cargo machete --with-metadata` (Linux CI) fails when a crate declares a dependency whose library name its source files never mention, in every dependency table (normal, dev, build, and target-specific). Findings are reviewed, not removed with `--fix`: remove a dependency that is unused, or list a false positive (a dependency used only through a macro or a feature) in the crate's `[package.metadata.cargo-machete] ignored` with a comment saying why. The pinned cargo-machete does not scan a package's root `build.rs`, so a build-dependency used only there is reported too and goes in `ignored` the same way (there is none today). It runs `cargo metadata` without `--locked`, and it exits 0 when it cannot read a manifest, so the CI step also fails on its error lines and on a changed `Cargo.lock` or `fuzz/Cargo.lock` (it also finds `fuzz/Cargo.toml` by itself).
- `.github/workflows/audit.yml` runs `cargo deny check advisories` and `cargo audit --deny warnings` daily, on manual dispatch, and on pull requests that change the workflow, `deny.toml` or `.cargo/audit.toml`, so advisories published while the repository is idle are reported. A failed scheduled run notifies the user who last changed the workflow's cron or last re-enabled the workflow, and only if that user has Actions notifications enabled (the default). GitHub disables scheduled workflows in a public repository after 60 days without repository activity and emails the owner before it does; the daily audit covers idle periods up to that limit, and the owner re-enables the workflow from the Actions tab, which makes the owner the recipient. CI's cargo-deny and cargo-audit steps still run on pushes to `main` and pull requests targeting `main`, except documentation-only ones ("CI" above).

## Measurement Conditions

Performance and latency are measured in a state kept separate from everyday development, and the following conditions are recorded along with the results.

- The code commit, whether there are uncommitted changes, and the build configuration
- The measurement scene, the input log used, and the measurement tool versions
- CPU and memory, GPU and driver version, Windows build, power settings
- Output and render resolutions, refresh rate, and VRR
- Audio device and driver, WASAPI shared or exclusive, sample rate, buffer, whether audio enhancements are enabled
- Input device and the USB port it is connected to

Keep separate baselines without OBS and with OBS (encoder, resolution, and fps fixed). Absolute press-to-light and press-to-sound latency can additionally be checked with the measurement output and a high-speed camera, or with the optional test input device ([design.md](design.md), "Test Input Device"); neither is required.

The game's own figures ([design.md](design.md), "Latency Figures") show in an overlay that F9 toggles during play and are written after each saved play as `<record>.measure.toml` beside the record, numbers only, with a figure that lacks evidence left out (N/A, never 0). The development command `tapestry measure play <chart>` (built with `dev-tools`) also captures the play's presents with PresentMon and adds the lost refreshes, off-grid and never-displayed counts of the play span to that file. `tapestry measure play --perfect <chart>` does the same with generated input that hits every note on time and key sounds sent from judgments (design.md "Autoplay"), to measure charts too heavy to play by hand. PresentMon is installed by hand and never downloaded: set `TAPESTRY_PRESENTMON` to the path of its executable, or put `PresentMon.exe` on `PATH`. It runs unelevated, so the user must be in the Performance Log Users group; its capture goes to `measure\` under the data root. The game opens on the system's primary monitor; to measure on another one without changing Windows settings, set `monitor` in `device.toml` to its display device name (for example `monitor = '\\.\DISPLAY1'`). The game prints the monitor and refresh rate it opened on, and the names it knows when the setting matches none.

## Other

- Your own charts never enter the repository. To check the loader and autoplay against them, run the local chart sweep on the development PC (Git Bash): `TAPESTRY_CHARTS_DIR=<dir> mise exec -- cargo test -p game --test local_charts -- --ignored --nocapture`. The sweep prints one line per chart with counts and warnings, never titles or paths (the game's own log does record the paths of charts it plays).
- To re-evaluate saved records without the game, run `mise exec -- cargo run -p eval-cli -- --chart <chart> [--preset <file.toml>]... <record.tprl>...`. It verifies each record against the chart and prints one line per record and preset (without `--preset`, the record's own preset): as played or what-if with what the input covers, the judgment counts, EX, BP, max combo and each gauge. A refused or incomplete record prints `refused: <reason>` on each of its lines and the exit code is 1; a chart or preset file that cannot be read or loaded exits 1 before any line, and bad arguments exit 2.
- The game's log is `%LOCALAPPDATA%\tapestry\logs\tapestry.log`, with the two runs before it in `tapestry.1.log` and `tapestry.2.log` (design.md "Log File").
- Antivirus exclusions are not configured by default. Only if builds are actually confirmed to be slow, exclude the build output directory and nothing else.
- Code signing is prepared before public distribution. It is not needed during development.
