#!/usr/bin/env bash
# Realise the flake's dev shells before CI uses them, trying each up to three
# times: cache.nixos.org drops a download now and then, and a later step
# would fail on it. Only the shells' inputs are retried here, never a test.
# Each shell is kept as a profile (a GC root) for the rest of the job.
# Usage: scripts/nix-prepare.sh <shell>...
set -euo pipefail

[ "$#" -ge 1 ] || { echo "usage: scripts/nix-prepare.sh <shell>..." >&2; exit 2; }
cd "$(git -C "$(dirname -- "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)"

# A try normally takes under a minute; at most 3 min with the kill, two
# shells of three tries and the pauses leave the jobs time for their checks.
tries=3
try_seconds=150
profiles=${RUNNER_TEMP:-${TMPDIR:-/tmp}}/nix-shells
mkdir -p "$profiles"
for shell in "$@"; do
  for try in $(seq "$tries"); do
    status=0
    timeout --kill-after=30 "$try_seconds" \
      nix develop ".#$shell" --profile "$profiles/$shell" --command true || status=$?
    [ "$status" -eq 0 ] && break
    if [ "$try" -eq "$tries" ]; then
      echo "the $shell shell failed $tries times, last with status $status" >&2
      exit 1
    fi
    echo "::warning::the $shell shell failed with status $status (try $try of $tries); trying again"
    sleep $((try * 15))
  done
done
