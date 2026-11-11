#!/bin/sh
# Build and package the native CLI with its static solver.
set -eu
cd "$(dirname "$0")/.."
sh tools/setup_z3.sh
. ./.deps/z3/env.sh
cargo build -p vera-compiler --release --locked
vera_package_dynamic=$(readelf -d target/release/vera)
case "$vera_package_dynamic" in
    *libz3.so*)
        echo 'Refusing to package an executable that depends on a shared Z3 library.' >&2
        exit 1
        ;;
esac
vera_package_version=$(target/release/vera --version | cut -d ' ' -f 2)
vera_package_target=$(rustc -vV | sed -n 's/^host: //p')
vera_package_name="vera-$vera_package_version-$vera_package_target"
vera_package_stage=$(mktemp -d)
trap 'rm -rf "$vera_package_stage"' EXIT HUP INT TERM
mkdir -p "$vera_package_stage/$vera_package_name/docs" dist
cp target/release/vera README.md LICENSE THIRD_PARTY_NOTICES.md "$vera_package_stage/$vera_package_name/"
cp docs/LANGUAGE.md "$vera_package_stage/$vera_package_name/docs/"
tar -czf "dist/$vera_package_name.tar.gz" -C "$vera_package_stage" "$vera_package_name"
(cd dist && sha256sum "$vera_package_name.tar.gz" > "$vera_package_name.tar.gz.sha256")
printf '%s\n' "dist/$vera_package_name.tar.gz" "dist/$vera_package_name.tar.gz.sha256"
