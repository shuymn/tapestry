# Development Environment

The environment for developing this repository, and how tool versions are pinned. The design is in [design.md](design.md).

## Requirements

| Item | Details |
| --- | --- |
| OS | Windows 10/11 (x64). Building, running, and measuring the game are done natively on Windows |
| Git | Git for Windows |
| C++ build environment | Visual Studio Build Tools (C++ workload, MSVC toolset, Windows SDK) |
| Version pinning | [mise](https://mise.jdx.dev/). Rust and auxiliary CLI versions are pinned in `mise.toml` and `mise.lock` |
| Linux-side tools (optional) | NixOS on WSL (or a Linux with Nix installed). Used for fuzzing, Linux builds and tests of crates that do not depend on Windows, and checking the WASM build. CI performs the same checks, so development is possible without it locally |

## Repository Location

- Place the working copy at a short path (e.g. `C:\dev\tapestry`). Set `core.longpaths=true` in Git and also enable long paths in the OS. The OS setting alone does not make every tool support long paths:
  - Native programs (Git, mise, cargo) cannot be started from a current directory longer than 260 characters.
  - The MSVC linker (`link.exe`) cannot open paths over 260 characters (`LNK1104`), even with long paths enabled.
  - Keep every path under the working copy, including `target/`, below 260 characters, with margin for new dependencies. With a 94-character root the deepest path was 217 characters and everything built; with a 201-character root it was 333 characters and build scripts failed to link.
- Line endings are determined by `.gitattributes`.
- From WSL, use the same working copy as `/mnt/c/...`. So that worktrees created on Windows can also be read from WSL, set `git config --global worktree.useRelativePaths true` before creating worktrees (both the Windows-side and the WSL-side Git must be versions that support this setting). Git operations that change the working copy (commits, branch switches, etc.) are done only on the Windows side. Do not use file change watching across Windows and WSL.

## Version Pinning

Tool versions are pinned with mise. `mise.toml` specifies major-version ranges, and the exact versions are pinned in `mise.lock`. Configure `mise.toml` so that versions not in `mise.lock` are not used:

```toml
[tool_config]
locked = true

[settings]
lockfile_platforms = ["windows-x64", "linux-x64"]
```

`locked` in `[tool_config]` applies only to tools declared in this repository (it does not apply to global tools).
Updates to `mise.lock` are submitted as PRs by a periodically running workflow ("Lock Updates" below).

### Rust

The Rust version is also pinned with mise. Do not add a language-specific version file (`rust-toolchain.toml`).

```toml
[tools]
rust = { version = "1", profile = "minimal", components = "rustfmt,clippy" }
```

- mise installs Rust using rustup and selects the version by setting `RUSTUP_TOOLCHAIN`. So calling `cargo` without going through mise does not give you the version pinned in this repository. Call it as `mise exec -- cargo ...`, or work in a shell with mise activated.
- On both Windows and Linux (locally and in CI), mise selects the version in `mise.lock` with the `minimal` profile plus `rustfmt` and `clippy`.
- Without mise, install the version written in `mise.lock` with rustup and call `cargo +<version> ...`.
- Additional targets (such as `wasm32-unknown-unknown`) are added in the stage that uses them.

### Hooks

[lefthook](https://lefthook.dev/) runs the git pre-commit hook. lefthook itself is also pinned with mise.

```sh
mise install
mise exec -- lefthook install
```

The pre-commit hook runs the banned-word check and the `rustfmt` check. The hook is for noticing problems early locally; CI is the final gate.

### Lock Updates

`.github/workflows/mise-update.yml` runs daily on `main` (and on manual dispatch):

- `mise lock --bump` re-resolves the ranges in `mise.toml` to the newest versions allowed by `minimum_release_age`. Only tools whose version changes are re-locked, so checksums of unchanged versions stay pinned. It reads release metadata and writes the lockfile; it does not install or run the tools.
- When Rust changes, it also runs `nix flake update rust-overlay` and evaluates the dev shell, which fails if rust-overlay does not know the new version.
- It fails if any file other than `mise.lock` and `flake.lock` changed.
- The result is a signed commit on the `mise-update` branch, with one open PR from it (a From/To table, auto-merge with a merge commit). A branch that already proposes the same lockfiles is kept, and its PR is reused or opened again; otherwise the branch is recreated from `main`, which closes the stale PR. To keep a tool at its version, write the exact version in `mise.toml` with a comment saying why. Major updates are manual: change the range in `mise.toml` and run `mise lock`.

PRs created with `GITHUB_TOKEN` do not run CI unattended, so auto-merge would never get checks. The workflow opens the PR with a GitHub App token instead, which needs:

- A GitHub App installed on this repository only, with the repository permissions Contents (read and write) and Pull requests (read and write).
- The repository variable `APP_CLIENT_ID` (the App's client ID) and the secret `APP_PRIVATE_KEY` (a private key of the App).
- "Allow auto-merge" and the CI jobs required on `main` (auto-merge waits only for required checks). Both are declared in `.github/infra.yaml`.

## Linux-Side Tools (WSL)

- Linux-side tools are pinned in the development shell of a Nix flake (`flake.nix` and `flake.lock`). The Rust version is read from `mise.lock`, so WSL uses the same `rustc` as Windows. The dated nightly used for fuzzing is also used only here.
- On NixOS, enable `nix-command` and `flakes`. The Nix in WSL must be linked to libgit2 1.9.4 or later: `worktree.useRelativePaths` enables the `relativeWorktrees` repository extension, and older libgit2 refuses to open the repository.
- A flake reads only files tracked by Git. Stage (`git add`) a new `flake.nix` input, or a file the flake reads, before using it.
- Build output in WSL is separated into a directory inside WSL via `CARGO_TARGET_DIR` (`~/.cache/tapestry/target/<hash of the working copy root>`). This keeps it from mixing with the Windows-side output and between working copies, and avoids slow writes over `/mnt/c`.
- Work in WSL directly on the `/mnt/c` working copy. With the target directory inside WSL, `cargo test --workspace` (crates that do not depend on Windows) after `cargo clean` took 1.05 times as long as on a copy of the same commit inside WSL (median of three alternating runs, 4.0 s against 3.8 s). Revisit this if the ratio exceeds 1.5; heavy work would then use a copy inside WSL.
- To call WSL commands from the Windows side, use `scripts/wsl.sh <command> [args...]` from Git Bash. It runs `wsl.exe -d NixOS --exec /bin/sh`, which converts the working copy's path with `wslpath` inside WSL, changes to the working copy root (the wrapper always runs there, whatever the current directory), and runs `nix develop --command <command> [args...]`. Arguments are passed verbatim, without being parsed again by a shell, and the exit code is the command's (125 if the working copy cannot be entered). `TAPESTRY_WSL_DISTRO` selects another distro. On Linux the wrapper runs `nix develop --command` directly. `scripts/wsl-selftest.sh` checks the wrapper.
- From PowerShell, call it through Git's `bash.exe` (`& "$env:ProgramFiles\Git\bin\bash.exe" scripts/wsl.sh ...`). Plain `bash` may resolve to WSL's `bash.exe`. Windows PowerShell 5.1 drops empty arguments and strips embedded quotes when calling native programs, so run commands with such arguments from Git Bash.

## CI

| Runner | What it does |
| --- | --- |
| Windows (label with an explicit OS version) | Production build (pinned to a configuration with `dev-tools` disabled), verifying that development features are not included, rendering image comparison with WARP, Windows tests |
| Linux | Builds and tests of crates that do not depend on Windows, WASM build, short fuzzing runs, banned-word check, dependency checks (`cargo-deny`, `cargo-audit`), tests in the Nix development shell through `scripts/wsl.sh` |

- Both runners install tools from the same `mise.lock` (`jdx/mise-action`). Run the install step even after restoring the cache, and verify the Rust version in use.
- GitHub Actions are pinned by commit SHA.
- Windows runner images keep being updated even when the version is specified by label. Record the runner image version, the adapter used, and the available features. When an image update changes image comparison results, review the diff images before updating the expected images.
- On the `windows-2025` runner, a WARP device can be created explicitly (D3D12, `IDXGIFactory4::EnumWarpAdapter`) and a rendered target read back exactly. Verified on image `win25-vs2026 20261021.278.1` (the `windows-2025` label currently maps to the Visual Studio 2026 image): Microsoft Basic Render Driver, driver 10.0.26100.33648, maximum feature level 12_1.
- Repository settings (merge methods, the `main` rulesets with the required CI checks, allowed Actions, security features) are declared in `.github/infra.yaml`. Change them there and run `mise exec -- gh-infra plan .github/infra.yaml`, then `apply`.

## Measurement Conditions

Performance and latency are measured in a state kept separate from everyday development, and the following conditions are recorded along with the results.

- The code commit, whether there are uncommitted changes, and the build configuration
- The measurement scene, the input log used, and the measurement tool versions
- CPU and memory, GPU and driver version, Windows build, power settings
- Output and render resolutions, refresh rate, and VRR
- Audio device and driver, WASAPI shared or exclusive, sample rate, buffer, whether audio enhancements are enabled
- Input device and the USB port it is connected to

Keep separate baselines without OBS and with OBS (encoder, resolution, and fps fixed). Verify absolute audio latency not only with the game's internal times but also with the test input device ([design.md](design.md), "Testing Strategy").

## Other

- Antivirus exclusions are not configured by default. Only if builds are actually confirmed to be slow, exclude the build output directory and nothing else.
- Code signing is prepared before public distribution. It is not needed during development.
