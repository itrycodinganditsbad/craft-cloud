#!/bin/sh
# Tests update.sh shell syntax and dirty-tree refusal without Docker or network access.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
script="$here/../update.sh"
sh -n "$script"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM
mkdir -p "$work/packaging/docker"
cp "$script" "$work/packaging/docker/update.sh"
printf 'version = "0.1.0"\n' >"$work/Cargo.toml"
git -C "$work" init -q
git -C "$work" -c user.name=test -c user.email=test@example.invalid add Cargo.toml
git -C "$work" -c user.name=test -c user.email=test@example.invalid commit -qm initial
printf 'changed\n' >>"$work/Cargo.toml"
if (cd "$work" && sh packaging/docker/update.sh --yes) >"$work/out" 2>&1; then
    echo 'update.sh accepted a dirty tree' >&2
    exit 1
fi
grep -q 'working tree has local changes' "$work/out"
echo 'update dirty-tree test passed'
