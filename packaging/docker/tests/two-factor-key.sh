#!/bin/sh
# Tests make-key.sh without Docker: the key is made once, with 32 bytes and mode 0600; and runs that overlap, many times over,
# never change a key that was made (one run makes it, the others leave it be, and no temporary file is left behind).
# Run it from anywhere: sh packaging/docker/tests/two-factor-key.sh [rounds] [runs per round]
set -eu

here=$(cd "$(dirname "$0")" && pwd)
rounds=${1:-60}
runs=${2:-8}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM
owner="$(id -u):$(id -g)"
failures=0

fail() {
    echo "FAIL: $*" >&2
    failures=$((failures + 1))
}

# shellcheck disable=SC1091
. "$here/../make-key.sh"

# One run, as secrets-init.sh makes it; its words go to $1.
run() {
    ( make_key "$2" "$owner" ) >"$1" 2>&1
}

# 1. A key is made, once: 32 bytes, mode 0600, and no temporary file stays.
data="$work/one"
mkdir "$data"
key="$data/two-factor.key"
run "$work/said" "$key" || fail "making the key failed: $(cat "$work/said")"
[ "$(wc -c <"$key")" -eq 32 ] || fail "the key is not 32 bytes"
case $(uname -s) in
    MINGW* | MSYS* | CYGWIN*) ;; # these have no file modes
    *) [ "$(ls -ld "$key" | cut -c1-10)" = "-rw-------" ] || fail "the key is not mode 0600: $(ls -ld "$key")" ;;
esac
grep -q "made the two-factor key" "$work/said" || fail "the first run did not say it made the key"
first=$(cksum <"$key")
run "$work/said" "$key" || fail "a second run failed"
[ "$(cksum <"$key")" = "$first" ] || fail "a second run changed the key"
if grep -q "made" "$work/said"; then fail "a second run said it made the key"; fi
[ "$(ls -A "$data" | wc -l)" -eq 1 ] || fail "a temporary file was left: $(ls -A "$data")"

# 2. A key that is a link is left alone as well (where links can be made).
data="$work/link"
mkdir "$data"
if ln -s "$work/nowhere" "$data/two-factor.key" 2>/dev/null; then
    run "$work/said" "$data/two-factor.key" || fail "a dangling link made the run fail"
    [ -L "$data/two-factor.key" ] && [ ! -e "$work/nowhere" ] || fail "a dangling link was replaced"
fi

# 3. Runs that overlap: the key never changes after it first exists, and exactly one run made it.
round=0
while [ "$round" -lt "$rounds" ]; do
    round=$((round + 1))
    data="$work/race.$round"
    mkdir "$data"
    key="$data/two-factor.key"
    n=0
    while [ "$n" -lt "$runs" ]; do
        n=$((n + 1))
        run "$data.said.$n" "$key" &
    done
    # A watcher that reads the key while the runs go on: once it exists it must not change.
    seen=
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        if [ -e "$key" ]; then
            now=$(cksum <"$key")
            [ -z "$seen" ] || [ "$seen" = "$now" ] || fail "round $round: the key changed while the runs went on"
            seen=$now
        fi
    done
    wait
    [ -e "$key" ] || { fail "round $round: no key"; continue; }
    [ "$(wc -c <"$key")" -eq 32 ] || fail "round $round: the key is not 32 bytes"
    [ -z "$seen" ] || [ "$seen" = "$(cksum <"$key")" ] || fail "round $round: the key at the end is not the one seen first"
    made=$(cat "$data".said.* | grep -c "made the two-factor key" || true)
    [ "$made" -eq 1 ] || fail "round $round: $made runs said they made the key"
    [ "$(ls -A "$data" | wc -l)" -eq 1 ] || fail "round $round: a temporary file was left: $(ls -A "$data")"
done

if [ "$failures" -ne 0 ]; then
    echo "$failures failure(s)" >&2
    exit 1
fi
echo "two-factor-key.sh: ok"
