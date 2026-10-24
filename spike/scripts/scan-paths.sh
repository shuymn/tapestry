#!/usr/bin/env bash
# Fails if any file under <dir> contains a developer path of this machine,
# as UTF-8 or UTF-16LE text (case-insensitive): the user profile, Cargo's
# registry and git checkouts, rustup, and this checkout. package.sh runs it
# on the staged bundle.
#
#   bash spike/scripts/scan-paths.sh <dir>
set -euo pipefail

[ $# = 1 ] && [ -d "$1" ] || {
    echo "usage: scan-paths.sh <dir>" >&2
    exit 2
}
dir=$1
spike_dir=$(cd "$(dirname "$0")/.." && pwd)

# A PCRE matching `s` as UTF-16LE, every character as a hex escape.
utf16() {
    local s=$1 out="" i
    for ((i = 0; i < ${#s}; i++)); do
        out+=$(printf '%s%02x%s' '\x' "'${s:i:1}" '\x00')
    done
    printf '%s' "$out"
}

patterns=("$(cygpath -w "$USERPROFILE")" "$(cygpath -m "$USERPROFILE")" "$HOME"
    '.cargo\registry' '.cargo/registry' '.cargo\git' '.cargo/git' '.rustup'
    "$(cygpath -w "$spike_dir")" "$(cygpath -m "$spike_dir")")
hits=0
for p in "${patterns[@]}"; do
    for mode in utf8 utf16; do
        status=0
        if [ $mode = utf8 ]; then
            found=$(grep -r -a -i -l -F -e "$p" "$dir") || status=$?
        else
            found=$(grep -r -a -i -l -P "$(utf16 "$p")" "$dir") || status=$?
        fi
        # grep: 0 match, 1 none; anything else is an error, never a pass.
        if [ $status -gt 1 ]; then
            echo "scan-paths: grep failed for '$p' ($mode)" >&2
            exit 2
        fi
        if [ $status = 0 ]; then
            echo "scan-paths: developer path '$p' ($mode) in:" $found >&2
            hits=1
        fi
    done
done
if [ $hits != 0 ]; then
    exit 1
fi
echo "scan-paths: no developer paths under $dir (${#patterns[@]} patterns, UTF-8 and UTF-16)"
