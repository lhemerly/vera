#!/bin/sh
# Usage: VERA=/path/to/vera sh examples/shipping/build.sh OUTPUT_DIRECTORY
set -eu
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
out=${1:?provide a new output directory}
mkdir -p -- "$out"
if test -e "$out/main.rs" || test -e "$out/verified.rs" || test -e "$out/shipping" || test -e "$out/api.json"; then
  echo 'output contains build files; choose a fresh directory' >&2
  exit 2
fi
"${VERA:-vera}" codegen rust "$here/policy.vera" --no-main --out "$out/verified.rs" --api-out "$out/api.json"
cp "$here/main.rs" "$out/main.rs"
rustc --edition 2021 "$out/main.rs" -o "$out/shipping"
printf 'Run: %s/shipping 1000 false 4000\n' "$out"
