#!/usr/bin/env bash
# Build and run the fuzz targets (docs/setup.md "Fuzzing"). Runs inside the
# Nix `fuzz` shell: TAPESTRY_NIX_SHELL=fuzz scripts/wsl.sh scripts/fuzz.sh ...
#
# Usage:
#   scripts/fuzz.sh build [--sanitizer S]
#   scripts/fuzz.sh seeds <dir>
#   scripts/fuzz.sh run <target> <seconds> [--seed N] [--corpus DIR] [--sanitizer S]
#   scripts/fuzz.sh run-all <seconds> [--seed N] [--corpus DIR] [--sanitizer S]
#   scripts/fuzz.sh repro <target> <file> [--sanitizer S]
#   scripts/fuzz.sh canary <panic|hang|malloc|rss> [--corpus DIR] [--sanitizer S]
#
# The corpus root (--corpus, else TAPESTRY_FUZZ_CORPUS) holds <target>/
# corpora, the generated seeds/, and a fresh artifacts/<target>-<run>/
# directory per run. A failing run prints the files of its artifact
# directory and exits with the fuzzer's status.
set -euo pipefail

targets=(bms_load replay_file replay_body raw_input preset_canonical skin_json)
# The per-input limits of every run: seconds, then MiB of RSS and of one
# allocation.
limits=(-timeout=10 -rss_limit_mb=2048 -malloc_limit_mb=2048)
# At most this many failing inputs, each at most this many bytes, are
# printed in full.
dump_files=4
dump_bytes=65536
# The most seconds per target: one target's run must fit in fuzz.yml's
# 45-minute job with the Nix setup and the build.
max_seconds=1800

usage() {
  sed -n '5,11p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' >&2
  exit 2
}

root=$(git -C "$(dirname -- "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)
cd "$root"

seed=
corpus=${TAPESTRY_FUZZ_CORPUS:-}
sanitizer=address
parse_options() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --seed) seed=${2:?--seed needs a value}; shift 2 ;;
      --corpus) corpus=${2:?--corpus needs a value}; shift 2 ;;
      --sanitizer) sanitizer=${2:?--sanitizer needs a value}; shift 2 ;;
      *) echo "unknown option: $1" >&2; usage ;;
    esac
  done
}

# A whole number of seconds from 1 to $max_seconds: libFuzzer reads -1 as no
# deadline and 0.5 as 0.
check_seconds() {
  if ! [[ "$1" =~ ^[1-9][0-9]{0,5}$ ]] || [ "$1" -gt "$max_seconds" ]; then
    echo "seconds must be a whole number from 1 to $max_seconds: got '$1'" >&2
    exit 2
  fi
}

need_corpus() {
  if [ -z "$corpus" ]; then
    echo "no corpus root: pass --corpus or set TAPESTRY_FUZZ_CORPUS" >&2
    exit 2
  fi
  mkdir -p "$corpus"
  corpus=$(cd "$corpus" && pwd)
}

is_target() {
  local t
  for t in "${targets[@]}" canary; do
    [ "$1" = "$t" ] && return 0
  done
  return 1
}

# cargo-fuzz has no --locked: fail on a stale lockfile before it builds.
fetch() {
  cargo fetch --manifest-path fuzz/Cargo.toml --locked
}

# Sets fuzz_args, the cargo-fuzz options of every build and run, once the
# options are parsed.
set_fuzz_args() {
  fuzz_args=(--fuzz-dir fuzz --sanitizer "$sanitizer" --debug-assertions
    --target-dir "${CARGO_TARGET_DIR:?the fuzz shell sets CARGO_TARGET_DIR}")
}

cargo_fuzz() {
  local command=$1
  shift
  set_fuzz_args
  cargo fuzz "$command" "${fuzz_args[@]}" "$@"
}

write_seeds() {
  cargo run --quiet --manifest-path fuzz/Cargo.toml --locked --example write_seeds -- "$1"
}

# Prints every regular file of artifact directory $1: fatal reproducers
# before slow units, the first $dump_files in base64 if at most $dump_bytes
# long, the others by size and SHA-256.
dump_artifacts() {
  local dir=$1 printed=0 file size
  local -a files
  mapfile -t files < <(
    find "$dir" -maxdepth 1 -type f ! -name 'slow-unit-*' | sort
    find "$dir" -maxdepth 1 -type f -name 'slow-unit-*' | sort
  )
  echo "${#files[@]} file(s) in $dir"
  for file in "${files[@]}"; do
    size=$(stat -c %s "$file")
    if [ "$printed" -lt "$dump_files" ] && [ "$size" -le "$dump_bytes" ]; then
      printed=$((printed + 1))
      echo "--- $(basename -- "$file") ($size bytes, base64)"
      base64 -w 76 "$file"
    else
      echo "--- $(basename -- "$file") ($size bytes, not printed) sha256 $(sha256sum "$file" | cut -d' ' -f1)"
    fi
  done
}

run_target() {
  local target=$1 seconds=$2 max_len=65536 artifacts status
  local -a corpora=("$corpus/$target") options
  mkdir -p "$corpus/$target"
  case "$target" in
    bms_load) corpora+=(crates/bms-parser/tests/charts) ;;
    raw_input) max_len=4096; corpora+=("$corpus/seeds/$target") ;;
    *) corpora+=("$corpus/seeds/$target") ;;
  esac
  artifacts="$corpus/artifacts/$target-$(date -u +%Y%m%dT%H%M%SZ)-$$"
  mkdir -p "$artifacts"
  options=(-max_total_time="$seconds" "${limits[@]}" -max_len="$max_len"
    -artifact_prefix="$artifacts/" -print_final_stats=1)
  [ -n "$seed" ] && options+=(-seed="$seed")
  [ "$target" = bms_load ] && options+=(-dict=fuzz/dict/bms.dict)
  [ "$target" = skin_json ] && options+=(-dict=fuzz/dict/skin.dict)
  echo "=== $target: $seconds s"
  status=0
  cargo_fuzz run "$target" "${corpora[@]}" -- "${options[@]}" || status=$?
  if [ "$status" -ne 0 ]; then
    echo "=== $target failed with status $status"
    dump_artifacts "$artifacts"
  fi
  return "$status"
}

# One canary case: its input alone in a fresh corpus directory, the limits
# that case must trip set explicitly, and an outer deadline in case a limit
# is not in force. Prints the artifact names found, then exits with the
# fuzzer's status, which is non-zero when the canary was caught.
run_canary() {
  local case=$1 dir status file
  local -a case_limits
  case "$case" in
    panic) case_limits=(-timeout=10 -rss_limit_mb=2048 -malloc_limit_mb=2048) ;;
    hang) case_limits=(-timeout=2 -rss_limit_mb=2048 -malloc_limit_mb=2048) ;;
    malloc) case_limits=(-timeout=10 -rss_limit_mb=2048 -malloc_limit_mb=64) ;;
    rss) case_limits=(-timeout=10 -rss_limit_mb=256 -malloc_limit_mb=4096) ;;
    *) echo "unknown canary case: $case" >&2; usage ;;
  esac
  dir="$corpus/canary/$case-$$"
  rm -rf "$dir"
  mkdir -p "$dir/corpus" "$dir/artifacts"
  printf %s "$case" > "$dir/corpus/$case"
  # Built outside the deadline, which bounds only the run.
  cargo_fuzz build canary
  set_fuzz_args
  status=0
  # -runs=1: the seed always runs in full; no mutated input runs after it.
  # `timeout` signals the whole process group, the fuzzer included.
  timeout --kill-after=10 120 cargo fuzz run "${fuzz_args[@]}" canary "$dir/corpus" -- \
    -runs=1 "${case_limits[@]}" -artifact_prefix="$dir/artifacts/" || status=$?
  for file in "$dir"/artifacts/*; do
    [ -f "$file" ] && echo "artifact: $(basename -- "$file")"
  done
  rm -rf "$dir"
  echo "canary $case: status $status"
  exit "$status"
}

[ "$#" -ge 1 ] || usage
command=$1
shift
case "$command" in
  build)
    parse_options "$@"
    fetch
    cargo_fuzz build
    git diff --exit-code fuzz/Cargo.lock
    ;;
  seeds)
    [ "$#" -eq 1 ] || usage
    write_seeds "$1"
    ;;
  run)
    [ "$#" -ge 2 ] || usage
    target=$1 seconds=$2
    shift 2
    is_target "$target" || { echo "unknown target: $target" >&2; usage; }
    check_seconds "$seconds"
    parse_options "$@"
    need_corpus
    fetch
    write_seeds "$corpus/seeds"
    run_target "$target" "$seconds"
    ;;
  run-all)
    [ "$#" -ge 1 ] || usage
    seconds=$1
    shift
    check_seconds "$seconds"
    parse_options "$@"
    need_corpus
    fetch
    write_seeds "$corpus/seeds"
    # Every target runs; the exit status is the first failing one's.
    failed=()
    first_status=0
    for target in "${targets[@]}"; do
      status=0
      run_target "$target" "$seconds" || status=$?
      if [ "$status" -ne 0 ]; then
        failed+=("$target")
        [ "$first_status" -ne 0 ] || first_status=$status
      fi
    done
    if [ "${#failed[@]}" -ne 0 ]; then
      echo "failed: ${failed[*]}" >&2
      exit "$first_status"
    fi
    ;;
  repro)
    [ "$#" -ge 2 ] || usage
    target=$1 file=$2
    shift 2
    is_target "$target" || { echo "unknown target: $target" >&2; usage; }
    parse_options "$@"
    fetch
    # A file instead of a corpus directory: libFuzzer runs it once.
    cargo_fuzz run "$target" "$file" -- "${limits[@]}"
    ;;
  canary)
    [ "$#" -ge 1 ] || usage
    case=$1
    shift
    parse_options "$@"
    need_corpus
    fetch
    run_canary "$case"
    ;;
  *)
    usage
    ;;
esac
