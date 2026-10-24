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

# App smoke: `--version` reports LGPL FFmpeg, then the WARP offscreen run
# (300 frames on a virtual 60 Hz clock over a generated 3 s BGA clip) must
# use the software adapter, show every due BGA frame (the run fails on a
# decoder error or stall), and read back non-blank frames.
if [ "${SPIKE_SKIP_APP:-0}" != 1 ]; then
    # The test profile's optimized dependencies are already built by the
    # test step above; reuse them for the smoke instead of a second build.
    mise exec -- cargo build -p tapestry-spike --profile test --locked
    exe=target/debug/tapestry-spike.exe
    version=$("$exe" --version)
    echo "$version"
    grep -q '^ffmpeg license: LGPL$' <<<"$version"
    smoke_dir=target/smoke
    "$exe" gen-assets --ffmpeg third_party/ffmpeg/bin/ffmpeg.exe --out "$smoke_dir/assets" --seconds 3
    smoke=$("$exe" run --offscreen 1920x1080 --adapter warp --audio null --frames 300         --bga "$smoke_dir/assets/bga.mp4" --out "$smoke_dir/run")
    echo "$smoke"
    grep -q '^adapter: Microsoft Basic Render Driver ' <<<"$smoke"
    # Every frame showed its due BGA frame across the 3 s clip's loop
    # boundary, the BGA alone is non-blank, and so is the full frame.
    grep -q -E '^bga: 1920x1080 decoded [1-9][0-9]* due 300 displayed 300 .* advanced Some\(true\)$' <<<"$smoke"
    grep -q '^bga-only readback: non-blank ' <<<"$smoke"
    grep -q '^readback: non-blank ' <<<"$smoke"
fi

cd "$repo_root"
mise exec -- cargo xtask banned-words --all
