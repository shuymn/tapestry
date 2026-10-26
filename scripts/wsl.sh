#!/usr/bin/env bash
# Run a command in the Nix dev shell of this working copy, at its root.
# On Windows (Git Bash) the command runs in a WSL distro; on Linux, directly.
# TAPESTRY_NIX_SHELL names the flake's dev shell: `default` (the default) or
# `fuzz`.
# Usage: scripts/wsl.sh <command> [args...]
set -euo pipefail

if [ "$#" -eq 0 ]; then
  echo "usage: scripts/wsl.sh <command> [args...]" >&2
  exit 2
fi

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
root=$(git -C "$script_dir" rev-parse --show-toplevel)
shell=${TAPESTRY_NIX_SHELL:-default}

case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*)
    winroot=$(cygpath -w "$root")
    # Stop MSYS from rewriting /bin/sh and other POSIX-looking arguments.
    export MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*'
    # The shell name and the arguments reach sh as positional parameters, so
    # they are not re-parsed.
    # Exit code 125: the working copy is not reachable from WSL.
    # shellcheck disable=SC2016
    exec wsl.exe -d "${TAPESTRY_WSL_DISTRO:-NixOS}" --exec /bin/sh -lc \
      'cd "$(wslpath -u "$1")" || exit 125; shell=$2; shift 2; exec nix develop ".#$shell" --command "$@"' \
      tapestry-wsl "$winroot" "$shell" "$@"
    ;;
  *)
    cd "$root"
    exec nix develop ".#$shell" --command "$@"
    ;;
esac
