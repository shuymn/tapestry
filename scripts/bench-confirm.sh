#!/usr/bin/env bash
# usage: BENCHES="<cargo bench selection>" bench-confirm.sh <log> <noise-threshold> <base-checkout> <base-target> <out.md>
#
# Confirms the regressions of one criterion comparison (docs/setup.md
# "Benchmarks"). <log> is the head run's output, compared against the merge
# base with `--noise-threshold <noise-threshold>` (a fraction, such as 0.03).
# Each benchmark it reports as regressed is measured once more, on the merge
# base (built in <base-checkout> into <base-target>) and then on the head (the
# current directory), both builds already done, and a regression counts as
# confirmed only when this second comparison reports it too. BENCHES holds the
# `-p` and `--bench` arguments that select the merge base's benchmarks.
#
# Writes a Markdown section to <out.md> and a warning annotation for each
# confirmed regression. At most MAX_FLAGGED benchmarks are measured again,
# which bounds the job's time. Always exits 0 unless its arguments are wrong:
# the comparison never fails the workflow.
set -u

MAX_FLAGGED=8

if [ "$#" -ne 5 ] || [ -z "${BENCHES:-}" ]; then
  echo "usage: BENCHES=\"<cargo bench selection>\" $0 <log> <noise-threshold> <base-checkout> <base-target> <out.md>" >&2
  exit 2
fi
log=$1 threshold=$2 base=$3 base_target=$4 final=$5
# Written aside and moved into place at the end, so a run cut short leaves
# no <out.md> and the summary says the confirmation did not finish.
out=$final.part
finish() {
  mv "$out" "$final"
  exit 0
}
percent=$(awk -v t="$threshold" 'BEGIN { printf "%g", t * 100 }')
# shellcheck disable=SC2206 # split into arguments on purpose
benches=($BENCHES)

# Prints "<id><TAB><change>" for each benchmark of a criterion log: those it
# reports as regressed (`parse <log> 0`) or every compared one (`parse <log>
# 1`). The id is criterion's stdout line before `time:` (on its own line, or
# in front of `time:` when it is short); the stderr "Benchmarking <id>: ..."
# lines are skipped, since they interleave out of order. Ids with spaces are
# not read (none of the repository's have one).
parse() {
  awk -v all="$2" '
    { sub(/\r$/, "") }
    /^Benchmarking / { next }
    /^[A-Za-z0-9_][A-Za-z0-9_.\/-]*$/ { id = $0; next }
    /^[A-Za-z0-9_][A-Za-z0-9_.\/-]* +time:/ { id = $1; next }
    /^ +change:/ {
      change = $0; sub(/^ +change: +/, "", change)
      if (all && id != "") print id "\t" change
      next
    }
    !all && /^ +Performance has regressed\.$/ && id != "" { print id "\t" change }
    # A verdict ends a benchmark: its id is not carried to the next one.
    /^ +(Performance has (regressed|improved)|No change in performance detected|Change within noise threshold)\.$/ { id = "" }
  ' "$1"
}
regressed() { parse "$1" 0; }

# Prints "<id><TAB><change>" lines as a Markdown list.
list() {
  printf '%s\n' "$1" | awk -F'\t' 'NF { print "- `" $1 "`: " $2 }'
}

# Prints "<id><TAB><first change><TAB><second change>" lines as a Markdown list.
list_both() {
  printf '%s\n' "$1" | awk -F'\t' 'NF { print "- `" $1 "`: first " $2 "; second " $3 }'
}

: > "$out"
flagged=$(regressed "$log" | sort -u)
count=$(printf '%s' "$flagged" | grep -c . || true)
lines=$(grep -c '^ *Performance has regressed\.' "$log" || true)
if [ "$count" -ne "$lines" ]; then
  message="The log reports $lines regressions, but only $count could be named; criterion's output format may have changed."
  echo "::warning::$message"
  printf '**%s**\n\n' "$message" >> "$out"
fi

if [ "$count" -eq 0 ]; then
  echo "No benchmark regressed beyond the ${percent} % noise threshold; nothing to confirm." >> "$out"
  finish
fi
# Past the cap, the MAX_FLAGGED with the highest lower bound are measured
# again (a change that slows many benchmarks still gets confirmed); the rest
# are listed as not measured again.
rest=
if [ "$count" -gt "$MAX_FLAGGED" ]; then
  ranked=$(printf '%s\n' "$flagged" | awk -F'\t' '{ lo = $2; sub(/^\[\+?/, "", lo); sub(/%.*/, "", lo); print lo "\t" $0 }' | sort -t "$(printf '\t')" -k1,1gr | cut -f2-)
  flagged=$(printf '%s\n' "$ranked" | head -n "$MAX_FLAGGED")
  rest=$(printf '%s\n' "$ranked" | tail -n +"$((MAX_FLAGGED + 1))")
  echo "::notice::$count benchmarks regressed beyond the ${percent} % noise threshold; the $MAX_FLAGGED with the highest lower bound are measured again."
fi

ids=$(printf '%s\n' "$flagged" | cut -f1)
# One anchored alternation of the ids, regex characters escaped.
filter="^($(printf '%s\n' "$ids" | sed 's/[][\\.*^$+?(){}|]/\\&/g' | paste -sd'|'))\$"
echo "Confirming: $filter"

status=0
CARGO_TARGET_DIR="$base_target" mise exec -- cargo bench --no-fail-fast --manifest-path "$base/Cargo.toml" "${benches[@]}" -- "$filter" --save-baseline confirm || status=$?
confirm_log=$(mktemp)
if [ "$status" -eq 0 ]; then
  set -o pipefail
  mise exec -- cargo bench --no-fail-fast "${benches[@]}" -- "$filter" --baseline-lenient confirm --noise-threshold "$threshold" 2>&1 | tee "$confirm_log" || status=$?
  set +o pipefail
fi
again=$(regressed "$confirm_log" | cut -f1)
second=$(parse "$confirm_log" 1)
rm -f "$confirm_log"
# Each measured benchmark with both changes. Every one is listed, confirmed or
# not, so the summaries of pull requests that leave a benchmark's code
# unchanged show how often the two passes in one job agree.
paired=$(awk -F'\t' 'NR == FNR { s[$1] = $2; next } NF { print $1 "\t" $2 "\t" (($1 in s) ? s[$1] : "not measured") }' <(printf '%s\n' "$second") <(printf '%s\n' "$flagged"))
confirmed=$(awk -F'\t' 'NR == FNR { c[$1] = 1; next } $1 in c' <(printf '%s\n' "$again") <(printf '%s\n' "$paired"))
unconfirmed=$(awk -F'\t' 'NR == FNR { c[$1] = 1; next } NF && !($1 in c)' <(printf '%s\n' "$again") <(printf '%s\n' "$paired"))

{
  if [ "$status" -ne 0 ]; then
    echo "**The confirmation run failed (exit $status); the regressions below are unconfirmed.**"
    echo
    list "$flagged"
  else
    if [ -n "$confirmed" ]; then
      echo "**Confirmed regressions** (beyond the ${percent} % noise threshold in both measurements):"
      echo
      list_both "$confirmed"
    else
      echo "No confirmed regression."
    fi
    if [ -n "$unconfirmed" ]; then
      echo
      echo "Not repeated in the second measurement:"
      echo
      list_both "$unconfirmed"
    fi
  fi
  if [ -n "$rest" ]; then
    echo
    echo "Over the cap of $MAX_FLAGGED, not measured again (change of the first):"
    echo
    list "$rest"
  fi
} >> "$out"

if [ "$status" -ne 0 ]; then
  echo "::warning::The confirmation run failed (exit $status); the flagged regressions are unconfirmed."
else
  printf '%s\n' "$confirmed" | awk -F'\t' 'NF { print "::warning::Confirmed regression: " $1 ": first " $2 "; second " $3 }'
fi
finish
