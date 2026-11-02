#!/usr/bin/env bash
# Realise the flake's dev shells before CI uses them, trying each up to three
# times: cache.nixos.org drops a download now and then, and a later step
# would fail on it. Only the shells' inputs are retried here, never a test.
# Usage: scripts/nix-prepare.sh <shell>...
set -euo pipefail

[ "$#" -ge 1 ] || { echo "usage: scripts/nix-prepare.sh <shell>..." >&2; exit 2; }
cd "$(git -C "$(dirname -- "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)"

tries=3
for shell in "$@"; do
  for try in $(seq "$tries"); do
    if timeout 900 nix develop ".#$shell" --command true; then
      break
    fi
    if [ "$try" -eq "$tries" ]; then
      echo "the $shell shell failed $tries times" >&2
      exit 1
    fi
    echo "::warning::the $shell shell failed (try $try of $tries); trying again"
    sleep $((try * 15))
  done
done
