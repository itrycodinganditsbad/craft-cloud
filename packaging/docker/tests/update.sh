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
    run)
        case $* in
            *'rm -rf'*)
                # Removing an old backup: the last argument is its name.
                for last; do :; done
                [ "${STUB_FAIL:-}" != remove ] || exit 1
                rm -rf "$STUB_BACKUPS/$last"
                ;;
            *) [ "${STUB_FAIL:-}" != copy ] || exit 1 ;;
        esac
        ;;
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
STUB_BACKUPS="$install/backups"
PATH="$work/bin:$PATH"
export STUB_LOG STUB_BACKUPS PATH

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

# 9. Old backups: the newest N are kept (this run's included), older ones are removed through a container, other names and
# links are left alone, and nothing is removed after a failure, with --skip-backup or with --keep 0.
old_backups() {
    rm -rf "$install/backups"
    mkdir -p "$install/backups"
    for stamp in 20200101T000000Z 20200102T000000Z 20200103T000000Z 20200104T000000Z; do
        mkdir "$install/backups/$stamp"
        echo data >"$install/backups/$stamp/database.sql"
    done
    mkdir "$install/backups/notes"
    mkdir "$install/backups/20200105T000000Z.keep"
    mkdir -p "$work/elsewhere"
    # A link where the system has them, otherwise a plain file: neither is a backup directory.
    ln -s "$work/elsewhere" "$install/backups/20200106T000000Z" 2>/dev/null || true
    if [ ! -L "$install/backups/20200106T000000Z" ]; then
        rm -rf "$install/backups/20200106T000000Z"
        : >"$install/backups/20200106T000000Z"
    fi
}
backups_left() { ls -1 "$install/backups" | grep -E '^2020' | tr '
' ' '; }
reset_to_first() { git_ -C "$install" reset -q --hard "$first"; }

reset_to_first
old_backups
: >"$work/log"
STUB_REVISION=$first run_update --yes || fail "the update with old backups failed"
current=$(ls -1 "$install/backups" | grep -E '^20[0-9]{2}(0[1-9]|1[0-2])' | grep -v '^2020' | head -n 1)
[ -n "$current" ] || fail "this run's backup is gone"
[ -d "$install/backups/20200104T000000Z" ] && [ -d "$install/backups/20200103T000000Z" ] || fail "the newest old backups were removed"
[ ! -d "$install/backups/20200102T000000Z" ] && [ ! -d "$install/backups/20200101T000000Z" ] || fail "old backups beyond the default of 3 were kept"
[ -d "$install/backups/notes" ] && [ -d "$install/backups/20200105T000000Z.keep" ] || fail "a directory that is no backup was removed"
[ -e "$install/backups/20200106T000000Z" ] || [ -L "$install/backups/20200106T000000Z" ] || fail "a link or file in backups/ was removed"
[ -d "$work/elsewhere" ] || fail "a link in backups/ was followed"
grep -q 'Removed old backup: .*20200102T000000Z' "$work/out" || fail "the removal of a backup is not printed"
grep -q 'Removed old backup: .*20200101T000000Z' "$work/out" || fail "the removal of the oldest backup is not printed"
grep -q ' | run --rm --network none -v .*/backups:/backups' "$work/log" || fail "old backups are not removed through a container"

reset_to_first
old_backups
STUB_REVISION=$first run_update --yes --keep 1 || fail "the update with --keep 1 failed"
[ -d "$install/backups/$current" ] || [ "$(ls -1 "$install/backups" | grep -Ec '^20[0-9]{2}(0[1-9]|1[0-2])')" -ge 1 ] || fail "--keep 1 removed this run's backup"
[ -z "$(backups_left | grep -E '2020010[1-4]')" ] || fail "--keep 1 kept an old backup: $(backups_left)"

reset_to_first
old_backups
CC_BACKUP_KEEP=0 STUB_REVISION=$first run_update --yes || fail "the update with CC_BACKUP_KEEP=0 failed"
[ "$(backups_left | grep -Eo '2020010[1-4]' | wc -l)" -eq 4 ] || fail "CC_BACKUP_KEEP=0 removed backups: $(backups_left)"

reset_to_first
old_backups
STUB_REVISION=$first run_update --yes --skip-backup || fail "the update with --skip-backup failed"
[ "$(backups_left | grep -Eo '2020010[1-4]' | wc -l)" -eq 4 ] || fail "--skip-backup removed backups: $(backups_left)"

reset_to_first
old_backups
if STUB_REVISION=$first STUB_FAIL=up run_update --yes; then fail "ignored a failed start"; fi
[ "$(backups_left | grep -Eo '2020010[1-4]' | wc -l)" -eq 4 ] || fail "a failed update removed backups: $(backups_left)"

# A backup that cannot be removed is a warning, not a failed update.
reset_to_first
old_backups
STUB_REVISION=$first STUB_FAIL=remove run_update --yes || fail "a failed removal failed the update"
grep -q 'could not remove the old backup' "$work/out" || fail "no warning about a backup that could not be removed"

# Not a number, and no value.
reset_to_first
if run_update --yes --keep many; then fail "accepted --keep many"; fi
grep -q 'whole number' "$work/out" || fail "no message about a bad --keep"
if run_update --keep; then fail "accepted --keep without a value"; fi
if CC_BACKUP_KEEP=-1 run_update --yes; then fail "accepted CC_BACKUP_KEEP=-1"; fi

# 10. The script starts with a restrictive umask.
grep -q '^umask 077$' "$script" || fail "no umask 077"
echo 'update tests passed'
