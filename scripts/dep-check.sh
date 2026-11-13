#!/usr/bin/env bash
# Run a dependency checker (`cargo deny` or `cargo audit`) on one lockfile,
# trying it up to three times when a run fails only because the registry
# index could not be read: crates.io returns a bad response now and then.
# An advisory, a yanked crate or any other failure fails at once, and the
# third index failure still fails (docs/setup.md "Dependency Checks").
# Usage: scripts/dep-check.sh deny|audit <arguments of cargo deny or cargo audit>...
set -euo pipefail

usage() { echo "usage: scripts/dep-check.sh deny|audit <arguments>..." >&2; exit 2; }
[ "$#" -ge 2 ] || usage
tool=$1
shift
case $tool in deny | audit) ;; *) usage ;; esac

tries=3
log=$(mktemp)
trap 'rm -f "$log"' EXIT

# Each reads the status and the log of the last try.
# cargo-deny with `--deny index-failure` reports each crate it could not look
# up as `error[index-failure]: unable to check for yanked crates` and exits 1
# (only the advisories check failed). Any other error diagnostic, a fatal
# `[ERROR]` log line, or another exit code is a real failure.
deny_index_failure() {
  local lines others
  [ "$status" -eq 1 ] || return 1
  lines=$(sed 's/\x1b\[[0-9;]*m//g' "$log")
  grep -q '^error\[index-failure\]: ' <<<"$lines" || return 1
  others=$(grep -E '^(error|bug)(\[[a-z-]+\])?: |\[ERROR\]' <<<"$lines" |
    grep -v '^error\[index-failure\]: ' || true)
  [ -z "$others" ]
}

# cargo-audit only logs index failures on stderr and still exits 0, so a
# nonzero exit is always a real failure.
audit_index_failure() {
  [ "$status" -eq 0 ] || return 1
  grep -qE "couldn't (update|open) crates.io index|couldn't check if the package is yanked" "$log"
}

for try in $(seq "$tries"); do
  status=0
  # stderr goes to the log (and on to the job's stderr); stdout stays as is.
  { mise exec -- cargo "$tool" "$@" 2>&1 1>&3 | tee "$log" >&2; } 3>&1 || status=$?
  "${tool}_index_failure" || exit "$status"
  if [ "$try" -eq "$tries" ]; then
    echo "cargo $tool could not read the registry index $tries times" >&2
    exit 1
  fi
  echo "::warning::cargo $tool could not read the registry index (try $try of $tries); trying again"
  sleep $((try * 15))
done
