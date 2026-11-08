#!/usr/bin/env sh
# Regenerates the differential table of the presentation program's
# numerics from the source's interpreter.
# usage: xtask/luaj/luaj_table.sh <path to luaj-jse-3.0.2-custom.jar>
set -eu
here=$(dirname "$0")
java -cp "$1" "$here/Probe.java" > "$here/../../crates/skin-beatoraja/tests/luaj_numerics.txt"
