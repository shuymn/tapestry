#!/usr/bin/env bash
# Check that scripts/wsl.sh reaches the dev shell, passes arguments verbatim,
# and returns the command's exit code.
# Usage: scripts/wsl-selftest.sh
set -uo pipefail

wsl="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/wsl.sh"
failures=0

pass() { echo "ok: $1"; }
fail() {
  echo "FAIL: $1" >&2
  failures=$((failures + 1))
}

# Preflight: Nix must be able to open this repository. Worktrees with relative
# paths need Nix linked to libgit2 >= 1.9.4. Every later check depends on this.
if "$wsl" nix flake metadata >/dev/null; then
  pass "nix flake metadata"
else
  echo "FAIL: nix flake metadata (is Nix linked to libgit2 >= 1.9.4?)" >&2
  exit 1
fi

"$wsl" false
code=$?
if [ "$code" -eq 1 ]; then pass "exit code 1"; else fail "exit code 1: got $code"; fi

"$wsl" sh -c 'exit 42'
code=$?
if [ "$code" -eq 42 ]; then pass "exit code 42"; else fail "exit code 42: got $code"; fi

# shellcheck disable=SC2016
args=('a b' '' '"q"' '$HOME' '*' '/usr/bin' 'C:\x y')
expected=$(printf '<%s>\n' "${args[@]}")
actual=$("$wsl" printf '<%s>\n' "${args[@]}")
if [ "$actual" = "$expected" ]; then
  pass "arguments passed verbatim"
else
  fail "arguments passed verbatim"
  printf 'expected:\n%s\nactual:\n%s\n' "$expected" "$actual" >&2
fi

# shellcheck disable=SC2016
cache=$("$wsl" sh -c 'echo "${XDG_CACHE_HOME:-$HOME/.cache}"')
# shellcheck disable=SC2016
target=$("$wsl" sh -c 'echo "$CARGO_TARGET_DIR"')
case "$target" in
  "$cache"/tapestry/target/?*) pass "CARGO_TARGET_DIR is $target" ;;
  *) fail "CARGO_TARGET_DIR under $cache/tapestry/target/: got '$target'" ;;
esac

if [ "$failures" -ne 0 ]; then
  echo "$failures check(s) failed" >&2
  exit 1
fi
echo "all checks passed"
