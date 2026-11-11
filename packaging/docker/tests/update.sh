#!/bin/sh
# Tests update.sh without Docker, network access or a real installation: a stub `docker` on PATH records the working
# directory and arguments of every call, and a local bare repository plays the upstream.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
script="$here/../update.sh"
version_script="$here/../workspace-version.sh"
sh -n "$script"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM
GIT_CONFIG_COUNT=1
GIT_CONFIG_KEY_0=core.autocrlf
GIT_CONFIG_VALUE_0=false
export GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0
git_() { git -c user.name=test -c user.email=test@example.invalid "$@"; }

# --- a stub docker --------------------------------------------------------------------------------------------------
mkdir "$work/bin"
cat >"$work/bin/docker" <<'STUB'
#!/bin/sh
# Records "<working directory> | <arguments>"; answers the few questions update.sh asks.
echo "$(pwd) | $*" >>"$STUB_LOG"
case $1 in
    compose)
        shift
        case $1 in
            ps) echo cid123 ;;
            config) printf 'server\ndb\n' ;;
            exec) echo 'dump contents' ;;
            build) [ "${STUB_FAIL:-}" != build ] || exit 1 ;;
            up) [ "${STUB_FAIL:-}" != up ] || exit 1 ;;
        esac
        ;;
    inspect)
        case $* in
            *Mounts*) echo volume1 ;;
            *) echo sha256:image1 ;;
        esac
        ;;
    image) echo "${STUB_REVISION:-}" ;;
    run) [ "${STUB_FAIL:-}" != copy ] || exit 1 ;;
esac
exit 0
STUB
chmod +x "$work/bin/docker"

# --- a repository with an upstream ----------------------------------------------------------------------------------
git init -q --bare -b main "$work/upstream.git"
git init -q -b main "$work/install"
git -C "$work/install" remote add origin "$work/upstream.git"
mkdir -p "$work/install/packaging/docker"
cp "$script" "$work/install/packaging/docker/update.sh"
cp "$version_script" "$work/install/packaging/docker/workspace-version.sh"
printf '[workspace.package]\nversion = "0.2.0"\n' >"$work/install/Cargo.toml"
printf 'backups/\npackaging/docker/.env\n' >"$work/install/.gitignore"
git_ -C "$work/install" add -A
git_ -C "$work/install" commit -qm first
git_ -C "$work/install" push -q -u origin main
first=$(git -C "$work/install" rev-parse HEAD)
git_ clone -q "$work/upstream.git" "$work/other" 2>/dev/null
printf 'more\n' >"$work/other/news.txt"
git_ -C "$work/other" add -A
git_ -C "$work/other" commit -qm second
git_ -C "$work/other" push -q origin main
second=$(git -C "$work/other" rev-parse HEAD)
install="$work/install"
STUB_LOG="$work/log"
PATH="$work/bin:$PATH"
export STUB_LOG PATH

run_update() { (cd "$install" && sh packaging/docker/update.sh "$@") >"$work/out" 2>&1; }
fail() {
    echo "FAIL: $*" >&2
    cat "$work/out" >&2
    echo '--- docker calls:' >&2
    cat "$work/log" >&2
    exit 1
}
line_of() { grep -n -- "$1" "$work/log" | head -n 1 | cut -d: -f1; }

# 1. Without .env nothing happens and the message says why.
: >"$work/log"
if run_update --yes; then fail "ran without a .env"; fi
grep -q '\.env not found' "$work/out" || fail "no message about the missing .env"
[ ! -s "$work/log" ] || fail "docker was called without a .env"

# 2. A dirty tree is refused.
printf 'COMPOSE_FILE=compose.yaml\n' >"$install/packaging/docker/.env"
printf 'changed\n' >>"$install/Cargo.toml"
if run_update --yes; then fail "accepted a dirty tree"; fi
grep -q 'working tree has local changes' "$work/out" || fail "no message about the dirty tree"
git -C "$install" checkout -q Cargo.toml

# 3. A full update: every compose call runs in packaging/docker, in the order build, stop, dump, copy, up.
: >"$work/log"
STUB_REVISION=$first run_update --yes || fail "the update failed"
[ "$(git -C "$install" rev-parse HEAD)" = "$second" ] || fail "the checkout is not at the confirmed commit"
compose_dir=$(cd "$install/packaging/docker" && pwd)
compose_lines=$(grep -c ' | compose ' "$work/log")
in_dir=$(grep -c "^$compose_dir | compose " "$work/log" || true)
[ "$compose_lines" -gt 0 ] && [ "$compose_lines" = "$in_dir" ] || fail "a compose call ran outside packaging/docker"
build=$(line_of ' | compose build server')
stop=$(line_of ' | compose stop server')
dump=$(line_of ' | compose exec -T db')
copy=$(line_of ' | run --rm --network none')
up=$(line_of ' | compose up -d --wait')
[ -n "$build" ] && [ -n "$stop" ] && [ -n "$dump" ] && [ -n "$copy" ] && [ -n "$up" ] || fail "a step is missing"
[ "$build" -lt "$stop" ] && [ "$stop" -lt "$dump" ] && [ "$dump" -lt "$copy" ] && [ "$copy" -lt "$up" ] ||
    fail "the steps are not in the order build, stop, dump, copy, up"
ls "$install"/backups/*/database.sql >/dev/null 2>&1 || fail "no database dump in the backup"
grep -q "^CC_REVISION=$second\$" "$install/packaging/docker/.env" || fail ".env does not name the new revision"
grep -q '^CC_VERSION=0.2.0$' "$install/packaging/docker/.env" || fail ".env does not name the version"
grep -q '^COMPOSE_FILE=compose.yaml$' "$install/packaging/docker/.env" || fail ".env lost its other lines"

# 4. Current checkout and current image: up to date, no compose build.
: >"$work/log"
STUB_REVISION=$second run_update --yes || fail "the up-to-date run failed"
grep -q 'up to date' "$work/out" || fail "no up to date message"
if grep -q ' | compose build' "$work/log"; then fail "built although up to date"; fi

# 5. Current checkout but an old image: the update goes on (an earlier run stopped after the merge).
: >"$work/log"
STUB_REVISION=$first run_update --yes || fail "the rebuild run failed"
[ -n "$(line_of ' | compose build server')" ] || fail "did not rebuild for an old image"
[ -n "$(line_of ' | compose up -d --wait')" ] || fail "did not restart for an old image"

# 6. A failed build keeps the branch: the hint is a reset on the branch, not a detached checkout.
git_ -C "$install" reset -q --hard "$first"
: >"$work/log"
if STUB_REVISION=$first STUB_FAIL=build run_update --yes; then fail "ignored a failed build"; fi
grep -q "git reset --hard $first" "$work/out" || fail "no reset hint after a failed build"
if grep -q 'git checkout' "$work/out"; then fail "the hint detaches HEAD"; fi
[ "$(git -C "$install" symbolic-ref --short HEAD)" = main ] || fail "left the branch"
[ -z "$(line_of ' | compose stop server')" ] || fail "stopped the server before the build succeeded"

# 7. A failed copy starts the old server again and says what changed.
git_ -C "$install" reset -q --hard "$first"
: >"$work/log"
if STUB_REVISION=$first STUB_FAIL=copy run_update --yes; then fail "ignored a failed copy"; fi
[ -n "$(line_of ' | compose start server')" ] || fail "did not start the old server after a failed copy"
grep -q 'The source is at' "$work/out" || fail "the message does not say that the source was updated"

# 8. A detached HEAD is refused clearly.
git_ -C "$install" checkout -q --detach
if run_update --yes; then fail "accepted a detached HEAD"; fi
grep -q 'detached HEAD' "$work/out" || fail "no message about the detached HEAD"
git_ -C "$install" checkout -q main

# 9. The script starts with a restrictive umask.
grep -q '^umask 077$' "$script" || fail "no umask 077"
echo 'update tests passed'
