#!/bin/sh
# Prints the version in [workspace.package] of a Cargo.toml (the repository's root one by default), the only place the
# version is kept. Used by install.sh for the image tag and the image's version label.
#
#   workspace-version.sh [path/to/Cargo.toml]
set -eu

manifest=${1:-"$(dirname "$0")/../../Cargo.toml"}
[ -r "$manifest" ] || {
    echo "workspace-version: can't read $manifest" >&2
    exit 1
}

version=$(awk '
    /^\[/ { in_package = ($0 == "[workspace.package]"); next }
    in_package && /^version[ \t]*=/ {
        value = $0
        sub(/^version[ \t]*=[ \t]*"/, "", value)
        sub(/".*$/, "", value)
        print value
        exit
    }
' "$manifest")

case $version in
    "" | *[!A-Za-z0-9.+-]*)
        echo "workspace-version: no usable version in [workspace.package] of $manifest" >&2
        exit 1
        ;;
esac
printf '%s\n' "$version"
