#!/usr/bin/env bash
# Local CI for the stage 0 spike workspace: fmt, clippy, tests.
# Run from anywhere: bash spike/scripts/check.sh
set -euo pipefail

spike_dir=$(cd "$(dirname "$0")/.." && pwd)
repo_root=$(cd "$spike_dir/.." && pwd)

cd "$spike_dir"
mise exec -- cargo fmt --all --check
mise exec -- cargo clippy --workspace --all-targets --locked -- -D warnings
mise exec -- cargo test --workspace --locked

# The banned-word check (PR #9) runs once it is on this branch's base; until
# then the subcommand does not exist and this step is skipped with a note.
cd "$repo_root"
if mise exec -- cargo xtask --help 2>/dev/null | grep -q 'banned-words'; then
    mise exec -- cargo xtask banned-words --all
else
    echo "check.sh: 'cargo xtask banned-words' not available on this branch; skipped" >&2
fi
