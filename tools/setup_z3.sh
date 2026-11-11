#!/bin/sh
# Install the pinned static solver used for Linux release builds.
set -eu

vera_z3_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
vera_z3_version=5.4.1
vera_z3_archive=z3-5.4.1-x64-glibc-2.39.zip
vera_z3_checksum=b548bac48ef2452b4e09ea2945cf4b011d5f43fcf39e4799df268ee7e0769d85
vera_z3_prefix="$vera_z3_root/.deps/z3/$vera_z3_version"

case "$(uname -s):$(uname -m)" in
    Linux:x86_64) ;;
    *)
        echo "Install Z3 >= 4.13.3 for your platform and build with Cargo." >&2
        echo "This static-solver installer supports Linux x86-64 (glibc 2.39+)." >&2
        exit 2
        ;;
esac

if ! test -f "$vera_z3_prefix/lib/libz3.a"; then
    vera_z3_temp=$(mktemp -d)
    trap 'rm -rf "$vera_z3_temp"' EXIT HUP INT TERM
    curl --fail --location --retry 3 --proto '=https' --tlsv1.2 \
        "https://github.com/Z3Prover/z3/releases/download/z3-$vera_z3_version/$vera_z3_archive" \
        --output "$vera_z3_temp/$vera_z3_archive"
    (cd "$vera_z3_temp" && printf '%s  %s\n' "$vera_z3_checksum" "$vera_z3_archive" | sha256sum -c -)
    unzip -q "$vera_z3_temp/$vera_z3_archive" -d "$vera_z3_temp/unpacked"
    vera_z3_source="$vera_z3_temp/unpacked/${vera_z3_archive%.zip}"
    mkdir -p "$vera_z3_prefix/lib" "$vera_z3_prefix/include"
    cp "$vera_z3_source/LICENSE.txt" "$vera_z3_prefix/LICENSE.txt"
    cp "$vera_z3_source/include/"*.h "$vera_z3_prefix/include/"
    # Copy the static library last. Shared libraries are intentionally excluded.
    cp "$vera_z3_source/bin/libz3.a" "$vera_z3_prefix/lib/libz3.a"
fi

mkdir -p "$vera_z3_root/.deps/z3"
vera_z3_library=$(printf '%s' "$vera_z3_prefix/lib" | sed "s/'/'\\\\''/g")
{
    printf "export Z3_LIBRARY_PATH_OVERRIDE='%s'\n" "$vera_z3_library"
    printf "export Z3_SYS_Z3_VERSION='%s'\n" "$vera_z3_version"
    printf "export Z3_NO_PKG_CONFIG='1'\n"
} > "$vera_z3_root/.deps/z3/env.sh"
printf '%s\n' "Solver installed. Run: . ./.deps/z3/env.sh"
