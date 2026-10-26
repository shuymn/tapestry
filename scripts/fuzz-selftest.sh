#!/usr/bin/env bash
# Check that a fuzz run fails, as seen from outside scripts/wsl.sh, on a
# panic, a hang, one allocation over the malloc limit, and RSS over the RSS
# limit: each `canary` case must exit non-zero, print its libFuzzer
# diagnostic and no other error class, and leave its reproducer.
# Usage: scripts/fuzz-selftest.sh [--sanitizer S]
set -uo pipefail

wsl="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/wsl.sh"
failures=0

# case|marker|error class|artifact prefix
cases=(
  "panic|panicked|deadly signal|crash-"
  "hang|ALARM: working on the last Unit|timeout after|timeout-"
  "malloc|out-of-memory (malloc(|out-of-memory (malloc(|oom-"
  "rss|limit: 256Mb|out-of-memory (used:|oom-"
)

for entry in "${cases[@]}"; do
  IFS='|' read -r name marker class prefix <<<"$entry"
  output=$(TAPESTRY_NIX_SHELL=fuzz "$wsl" scripts/fuzz.sh canary "$name" "$@" 2>&1)
  code=$?
  printf '%s\n' "$output"
  problems=()
  [ "$code" -ne 0 ] || problems+=("exit code 0")
  grep -qF -- "$marker" <<<"$output" || problems+=("no '$marker'")
  errors=$(grep -E 'ERROR: (libFuzzer|AddressSanitizer|LeakSanitizer)' <<<"$output")
  if [ -z "$errors" ]; then
    problems+=("no libFuzzer error")
  elif grep -vF -- "$class" <<<"$errors" >/dev/null; then
    problems+=("another error class: $(grep -vF -- "$class" <<<"$errors" | head -1)")
  fi
  grep -q "^artifact: $prefix" <<<"$output" || problems+=("no $prefix artifact")
  if [ "${#problems[@]}" -eq 0 ]; then
    echo "ok: canary $name (exit code $code)"
  else
    echo "FAIL: canary $name: ${problems[*]}" >&2
    failures=$((failures + 1))
  fi
done

if [ "$failures" -ne 0 ]; then
  echo "$failures case(s) failed" >&2
  exit 1
fi
echo "all cases passed"
