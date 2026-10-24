#!/usr/bin/env bash
# Builds the owner bundle (design §4): the release exe with the pinned FFmpeg
# DLLs, PresentMon, MediaMTX with its loopback-only config, the BGA asset,
# obs/, licenses/, the .cmd wrappers, owner-notes.toml, README.txt, and
# bundle-manifest.json listing every other file with its size and SHA-256.
#
#   LIBCLANG_PATH=<llvm bin> bash spike/scripts/package.sh
#       -> spike/target/bundle/tapestry-spike-bundle.zip
#
# The exe is built here, in its own target directory, with every local build
# path (Cargo's registry, the user profile, this checkout) remapped, so panic
# locations and debug strings carry no developer paths; the staged files are
# then scanned for them (scan-paths.sh) and the package fails on any hit. Needs libclang
# (LIBCLANG_PATH), scripts/fetch-tools.sh's third_party/ and licenses/, and
# assets/bga.mp4 (gen-assets).
set -euo pipefail

spike_dir=$(cd "$(dirname "$0")/.." && pwd)
cd "$spike_dir"

fail() {
    echo "package: $*" >&2
    exit 1
}

# The pinned FFmpeg payload's DLLs (tools.toml [ffmpeg]); only these ship.
ffmpeg_dlls=(avcodec-63.dll avdevice-63.dll avfilter-12.dll avformat-63.dll avutil-61.dll
    swresample-7.dll swscale-10.dll)

[ -f assets/bga.mp4 ] && [ -f assets/bga.mp4.sha256 ] || fail "assets/bga.mp4 missing (gen-assets)"
[ -d licenses/ffmpeg ] && [ -d licenses/presentmon ] && [ -d licenses/mediamtx ] ||
    fail "licenses/ incomplete (fetch-tools.sh)"
(cd assets && sha256sum --check --status bga.mp4.sha256) || fail "assets/bga.mp4 does not match its SHA-256"
[ -n "${LIBCLANG_PATH:-}" ] || fail "set LIBCLANG_PATH for this command (FFmpeg bindings)"
actual=$(cd third_party/ffmpeg/bin && ls -1 ./*.dll | sed 's|^\./||' | LC_ALL=C sort | tr '\n' ' ')
expected=$(printf '%s\n' "${ffmpeg_dlls[@]}" | LC_ALL=C sort | tr '\n' ' ')
[ "$actual" = "$expected" ] || fail "third_party/ffmpeg/bin DLLs ($actual) differ from the pinned set ($expected)"

# Release build with local paths remapped. rustc applies the last matching
# mapping first, so the specific ones follow the general one.
user_dir=$(cygpath -w "$USERPROFILE")
cargo_home=$(cygpath -w "${CARGO_HOME:-$USERPROFILE/.cargo}")
spike_win=$(cygpath -w "$spike_dir")
export RUSTFLAGS="--remap-path-prefix=$user_dir=/home --remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$spike_win=/spike"
export CARGO_TARGET_DIR="$spike_dir/target/bundle-build"
mise exec -- cargo build -p tapestry-spike --release --locked
unset RUSTFLAGS
exe=$CARGO_TARGET_DIR/release/tapestry-spike.exe
"$exe" --version | grep -q '^ffmpeg license: LGPL$' || fail "the exe does not report LGPL FFmpeg"

out=target/bundle
root=$out/tapestry-spike
if [ -e "$root" ]; then
    trash "$root"
fi
mkdir -p "$root/presentmon" "$root/mediamtx" "$root/obs" "$root/assets" "$root/runs"

cp "$exe" "$root/"
for d in "${ffmpeg_dlls[@]}"; do
    cp "third_party/ffmpeg/bin/$d" "$root/"
done
cp third_party/presentmon/PresentMon-*-x64.exe "$root/presentmon/"
cp third_party/mediamtx/mediamtx.exe "$root/mediamtx/"
cp obs/mediamtx.yml obs/overlay.html "$root/obs/"
cp assets/bga.mp4 assets/bga.mp4.sha256 "$root/assets/"
cp -r licenses "$root/licenses"
cp bundle/*.cmd bundle/owner-notes.toml bundle/README.txt "$root/"
# cmd.exe needs CRLF line endings.
for f in "$root"/*.cmd; do
    sed -i 's/\r$//; s/$/\r/' "$f"
done

# No developer path in any staged file.
bash scripts/scan-paths.sh "$root" || fail "staged files contain developer paths"

# Manifest: every file except itself, sorted, with size and SHA-256.
commit=$("$exe" --version | sed -n 's/^commit: //p')
{
    printf '{\n  "commit": "%s",\n  "files": [\n' "$commit"
    first=1
    (cd "$root" && find . -type f ! -name bundle-manifest.json | sed 's|^\./||' | LC_ALL=C sort) |
        while read -r f; do
            sha=$(sha256sum "$root/$f" | cut -d' ' -f1)
            size=$(stat -c %s "$root/$f")
            [ $first = 1 ] || printf ',\n'
            first=0
            printf '    {"path": "%s", "bytes": %s, "sha256": "%s"}' "$f" "$size" "$sha"
        done
    printf '\n  ]\n}\n'
} >"$root/bundle-manifest.json"

zip_path=$out/tapestry-spike-bundle.zip
if [ -e "$zip_path" ]; then
    trash "$zip_path"
fi
# Windows bsdtar writes zip archives with -a.
(cd "$out" && /c/Windows/System32/tar.exe -a -c -f tapestry-spike-bundle.zip tapestry-spike)
count=$(grep -c '"sha256"' "$root/bundle-manifest.json")
echo "package: $zip_path ($count files in the manifest; commit $commit)"
