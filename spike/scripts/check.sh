#!/usr/bin/env bash
# CI for the stage 0 spike workspace, run locally and by spike.yml:
# fmt, clippy, tests, the app smoke, and the repository's banned-word check.
# Run from anywhere: bash spike/scripts/check.sh
#
# The app links FFmpeg through ffmpeg-sys-next, whose build needs libclang
# and scripts/fetch-tools.sh's FFmpeg. SPIKE_SKIP_APP=1 checks only the
# portable `core` crate (for a machine without libclang).
set -euo pipefail

spike_dir=$(cd "$(dirname "$0")/.." && pwd)
repo_root=$(cd "$spike_dir/.." && pwd)

if [ "${SPIKE_SKIP_APP:-0}" = 1 ]; then
    echo "check.sh: SPIKE_SKIP_APP=1, checking core only (app build and smoke skipped)" >&2
    packages=(-p spike-core)
else
    packages=(--workspace)
fi

cd "$spike_dir"
mise exec -- cargo fmt --all --check
mise exec -- cargo clippy "${packages[@]}" --all-targets --locked -- -D warnings
mise exec -- cargo test "${packages[@]}" --locked

# App smoke. P2 adds the WARP offscreen run here
# (`run --offscreen 1920x1080 --adapter warp --audio null --frames 300`).
if [ "${SPIKE_SKIP_APP:-0}" != 1 ]; then
    mise exec -- cargo build -p tapestry-spike --locked
    version=$(target/debug/tapestry-spike.exe --version)
    echo "$version"
    grep -q '^ffmpeg license: LGPL$' <<<"$version"
fi

cd "$repo_root"
mise exec -- cargo xtask banned-words --all
