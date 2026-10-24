#!/usr/bin/env bash
# Downloads the pinned spike tools (tools.toml) into third_party/, verifies
# their SHA-256, checks that the FFmpeg build is LGPL, and writes licenses/.
#
#   third_party/ffmpeg/      FFmpeg LGPL shared build (bin/, include/, lib/)
#   third_party/presentmon/  PresentMon console exe
#   licenses/                FFmpeg license and build record, PresentMon MIT
#
# Fails (non-zero) on a hash mismatch, or unless `ffmpeg -buildconf` lacks
# --enable-gpl and --enable-nonfree and the avcodec and avutil libraries
# report an LGPL license. Idempotent: verified downloads are reused.
# Run from anywhere: bash spike/scripts/fetch-tools.sh
set -euo pipefail

spike_dir=$(cd "$(dirname "$0")/.." && pwd)
tools="$spike_dir/tools.toml"
third="$spike_dir/third_party"
downloads="$third/downloads"
licenses="$spike_dir/licenses"

fail() {
    echo "fetch-tools: $*" >&2
    exit 1
}

# Value of `key = "value"` in `[section]` of tools.toml.
toml() {
    awk -v section="[$1]" -v key="$2" '
        /^\[/ { in_section = ($0 == section) }
        in_section && $1 == key && $2 == "=" {
            sub(/^[^=]*=[ \t]*"/, ""); sub(/".*$/, ""); print; exit
        }' "$tools"
}

# fetch <url> <sha256> <file>: download unless already present and verified.
fetch() {
    local url=$1 sha=$2 out=$3
    if [ -f "$out" ] && echo "$sha  $out" | sha256sum --check --status; then
        echo "fetch-tools: $(basename "$out") already verified"
        return
    fi
    echo "fetch-tools: downloading $url"
    curl --fail --location --silent --show-error --retry 3 --output "$out.part" "$url"
    mv "$out.part" "$out"
    echo "$sha  $out" | sha256sum --check --status ||
        fail "SHA-256 mismatch for $(basename "$out") (expected $sha, got $(sha256sum "$out" | cut -d' ' -f1))"
    echo "fetch-tools: $(basename "$out") SHA-256 ok"
}

mkdir -p "$downloads"

# --- FFmpeg -------------------------------------------------------------------
ff_asset=$(toml ffmpeg asset)
ff_zip="$downloads/$ff_asset"
fetch "$(toml ffmpeg url)" "$(toml ffmpeg sha256)" "$ff_zip"

ff_dir="$third/ffmpeg"
rm -rf "$ff_dir" "$third/ffmpeg.unpack"
mkdir -p "$third/ffmpeg.unpack"
unzip -q "$ff_zip" -d "$third/ffmpeg.unpack"
# The zip holds one top-level directory named after the asset.
mv "$third/ffmpeg.unpack/${ff_asset%.zip}" "$ff_dir"
rmdir "$third/ffmpeg.unpack"

buildconf=$("$ff_dir/bin/ffmpeg.exe" -hide_banner -buildconf)
for flag in --enable-gpl --enable-nonfree; do
    if grep -q -- "$flag" <<<"$buildconf"; then
        fail "FFmpeg build is configured with $flag; an LGPL build is required"
    fi
done
# avcodec_license()/avutil_license() return the FFMPEG_LICENSE string built
# into each library ("LGPL version 2.1 or later" for an LGPL build).
for lib in avcodec avutil; do
    dll=$(find "$ff_dir/bin" -maxdepth 1 -name "$lib-*.dll" | head -n 1)
    [ -n "$dll" ] || fail "$lib DLL not found in $ff_dir/bin"
    license=$(grep -a -o -E '(nonfree and unredistributable|(L)?GPL version [0-9.]+( or later)?)' "$dll" | sort -u)
    if [ "$(wc -l <<<"$license")" -ne 1 ] || ! grep -q -E '^LGPL version ' <<<"$license"; then
        fail "$(basename "$dll") reports license '$license', not LGPL"
    fi
    echo "fetch-tools: $(basename "$dll") license: $license"
done
ff_version=$("$ff_dir/bin/ffmpeg.exe" -hide_banner -version | head -n 1)
configure_line=$(grep -m 1 -o -E 'configuration:.*' <<<"$("$ff_dir/bin/ffmpeg.exe" -hide_banner -version)" || true)

# --- PresentMon ---------------------------------------------------------------
pm_dir="$third/presentmon"
mkdir -p "$pm_dir"
pm_exe="$pm_dir/PresentMon-$(toml presentmon version)-x64.exe"
fetch "$(toml presentmon url)" "$(toml presentmon sha256)" "$pm_exe"

# --- licenses/ ----------------------------------------------------------------
rm -rf "$licenses"
mkdir -p "$licenses/ffmpeg" "$licenses/presentmon"
cp "$ff_dir/LICENSE.txt" "$licenses/ffmpeg/LICENSE.txt"
release=$(toml ffmpeg release)
cat >"$licenses/ffmpeg/BUILD.txt" <<EOF
FFmpeg is used as dynamically linked shared libraries (LGPL).

Version:            $ff_version
Build:              BtbN FFmpeg-Builds $release
Asset:              $ff_asset
SHA-256:            $(toml ffmpeg sha256)
Release:            https://github.com/BtbN/FFmpeg-Builds/releases/tag/$release
Build scripts:      https://github.com/BtbN/FFmpeg-Builds/tree/$(toml ffmpeg build_scripts_commit)
FFmpeg source:      https://github.com/FFmpeg/FFmpeg/commit/$(toml ffmpeg source_commit)
avcodec license:    $(grep -a -o -E 'LGPL version [0-9.]+( or later)?' "$(find "$ff_dir/bin" -maxdepth 1 -name 'avcodec-*.dll' | head -n 1)" | head -n 1)

$configure_line

Full build configuration (ffmpeg -buildconf):
$buildconf
EOF
fetch "$(toml presentmon license_url)" "$(toml presentmon license_sha256)" "$downloads/PresentMon-LICENSE.txt"
cp "$downloads/PresentMon-LICENSE.txt" "$licenses/presentmon/LICENSE.txt"
cat >"$licenses/presentmon/BUILD.txt" <<EOF
PresentMon $(toml presentmon version) (MIT), unmodified release binary.
Source:   https://github.com/GameTechDev/PresentMon/releases/tag/v$(toml presentmon version)
SHA-256:  $(toml presentmon sha256)
EOF

echo "fetch-tools: ok ($ff_version; PresentMon $(toml presentmon version))"
