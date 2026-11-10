#!/bin/sh
# Tests workspace-version.sh: it takes the version of [workspace.package] and nothing else.
# Run it from anywhere: sh packaging/docker/tests/workspace-version.sh
set -eu

here=$(cd "$(dirname "$0")" && pwd)
script="$here/../workspace-version.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM
failures=0

fail() {
    echo "FAIL: $*" >&2
    failures=$((failures + 1))
}

# expect <label> <expected version> <manifest text>
expect() {
    printf '%s\n' "$3" >"$work/Cargo.toml"
    got=$(sh "$script" "$work/Cargo.toml" 2>/dev/null) || got=
    [ "$got" = "$2" ] || fail "$1: expected '$2', got '$got'"
}

# expect_refused <label> <manifest text>
expect_refused() {
    printf '%s\n' "$2" >"$work/Cargo.toml"
    if sh "$script" "$work/Cargo.toml" >/dev/null 2>&1; then fail "$1: accepted"; fi
}

expect 'plain' '0.1.0' '[workspace]
members = ["a"]

[workspace.package]
version = "0.1.0"
edition = "2024"'

expect 'a pre-release' '1.2.3-rc.1' '[workspace.package]
edition = "2024"
version = "1.2.3-rc.1"'

expect 'another version line comes first' '0.4.0' '[package]
version = "9.9.9"

[workspace.package]
version = "0.4.0"

[dependencies]
version = "7.7.7"'

expect_refused 'no version in the section' '[workspace.package]
edition = "2024"

[package]
version = "9.9.9"'
expect_refused 'no section' '[package]
version = "1.0.0"'
expect_refused 'a version with a quote or a space' '[workspace.package]
version = "1.0 0"'
if sh "$script" "$work/missing.toml" >/dev/null 2>&1; then fail "a missing file was accepted"; fi

# The repository's own manifest gives a version of the shape install.sh expects.
real=$(sh "$script") || fail "the repository's Cargo.toml gives no version"
case $real in
    [0-9]*.[0-9]*.[0-9]*) ;;
    *) fail "unexpected version '$real' in the repository's Cargo.toml" ;;
esac

if [ "$failures" -ne 0 ]; then
    echo "$failures failure(s)" >&2
    exit 1
fi
echo "workspace-version tests passed"
