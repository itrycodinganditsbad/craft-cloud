#!/bin/sh
# Back up and upgrade a Docker checkout of Craft Cloud.
#
#   packaging/docker/update.sh [--yes] [--skip-backup]
#
# Git runs at the repository root; every `docker compose` command runs in packaging/docker, where `.env` and the
# COMPOSE_FILE list (the Postgres overlay) live. The order is: fetch, ask, fast-forward to the confirmed commit, build
# the new image while the old server runs, stop the server, dump Postgres (if used), copy the data volume, start.
set -eu
umask 077

root=$(cd "$(dirname "$0")/../.." && pwd)
compose_dir=$root/packaging/docker
cd "$root"

# dc <arguments...>: docker compose in the directory that holds .env and the Compose files.
dc() {
    (cd "$compose_dir" && docker compose "$@")
}

yes=0
skip_backup=0
for arg in "$@"; do
    case $arg in
        --yes) yes=1 ;;
        --skip-backup) skip_backup=1 ;;
        *) echo "update.sh: unknown option $arg" >&2; exit 2 ;;
    esac
done
[ -f "$compose_dir/.env" ] || { echo "update.sh: $compose_dir/.env not found; this is not a set-up Docker installation (run packaging/docker/install.sh first)" >&2; exit 1; }
git symbolic-ref -q HEAD >/dev/null || { echo "update.sh: the checkout is on a detached HEAD; check out the branch you follow first" >&2; exit 1; }
git diff --quiet || { echo "update.sh: working tree has local changes" >&2; exit 1; }
git diff --cached --quiet || { echo "update.sh: working tree has staged changes" >&2; exit 1; }
[ -z "$(git ls-files --others --exclude-standard | grep -v '^backups/' || true)" ] || { echo "update.sh: working tree has untracked files" >&2; exit 1; }
old_sha=$(git rev-parse HEAD)
git fetch --prune
upstream=$(git rev-parse '@{upstream}' 2>/dev/null) || { echo "update.sh: the current branch has no upstream branch to update from" >&2; exit 1; }

# The revision the running server image was built from (the image's org.opencontainers.image.revision label). A
# checkout that is current but whose image is old, for example after an earlier run stopped, is still an update.
running=
server_id=$(dc ps -aq server 2>/dev/null | head -n 1 || true)
if [ -n "$server_id" ]; then
    image_id=$(docker inspect --format '{{.Image}}' "$server_id" 2>/dev/null || true)
    if [ -n "$image_id" ]; then
        running=$(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$image_id" 2>/dev/null || true)
    fi
fi
if [ "$upstream" = "$old_sha" ] && [ "$running" = "$upstream" ]; then
    echo "Craft Cloud is up to date at $old_sha."
    exit 0
fi
if [ "$upstream" = "$old_sha" ]; then
    echo "The source is at $old_sha, but the running server was built from ${running:-an unknown revision}. It will be rebuilt and restarted."
fi
if [ "$yes" -ne 1 ]; then
    printf 'Update Craft Cloud from %s to %s? [y/N] ' "${running:-$old_sha}" "$upstream"
    IFS= read -r answer || answer=
    [ "$answer" = y ] || [ "$answer" = Y ] || exit 1
fi
# Exactly the commit that was confirmed, not whatever the branch is by now.
git merge --ff-only "$upstream"
new_sha=$(git rev-parse HEAD)
branch=$(git symbolic-ref --short HEAD)
back_to_source="git reset --hard $old_sha  (on the branch $branch)"

# set_env <name> <value>: writes the value into .env (mode 0600), replacing the line of that name.
set_env() {
    tmp="$compose_dir/.env.new"
    { grep -v "^$1=" "$compose_dir/.env" || true; echo "$1=$2"; } >"$tmp"
    chmod 600 "$tmp"
    mv "$tmp" "$compose_dir/.env"
}

# Build while the old server keeps running: a failed build leaves the server as it is.
version=$(sh "$compose_dir/workspace-version.sh")
set_env CC_VERSION "$version"
set_env CC_REVISION "$new_sha"
if ! CC_VERSION=$version CC_REVISION=$new_sha dc build server; then
    echo "update.sh: the build failed. The running server is unchanged, but the source is already at $new_sha. To go back to the old source: $back_to_source" >&2
    exit 1
fi

backup=skipped
if [ "$skip_backup" -eq 1 ]; then
    echo "WARNING: backup skipped by explicit request."
else
    stamp=$(date -u '+%Y%m%dT%H%M%SZ')
    backup="$root/backups/$stamp"
    server_id=$(dc ps -aq server | head -n 1)
    [ -n "$server_id" ] || { echo "update.sh: server container not found; refusing to update without a backup (or pass --skip-backup). The source is at $new_sha and the image is rebuilt; the old server was not touched. To go back: $back_to_source" >&2; exit 1; }
    data_volume=$(docker inspect --format '{{range .Mounts}}{{if eq .Destination "/var/lib/craft-cloud"}}{{.Name}}{{end}}{{end}}' "$server_id")
    [ -n "$data_volume" ] || { echo "update.sh: data volume not found; refusing to update without a backup (or pass --skip-backup). The source is at $new_sha and the image is rebuilt; the old server was not touched. To go back: $back_to_source" >&2; exit 1; }
    mkdir -p "$backup"
    # The server stops first, so the database dump and the copy of the volume see the same state. The database
    # service keeps running for the dump.
    dc stop server
    if dc config --services | grep -qx db; then
        dc exec -T db sh -c 'pg_dump -U "$POSTGRES_USER" "$POSTGRES_DB"' > "$backup/database.sql" || true
        if [ ! -s "$backup/database.sql" ]; then
            echo "update.sh: the Postgres backup failed; starting the old server again. The source is at $new_sha and the image is rebuilt; the old server container still runs the old image. To go back to the old source: $back_to_source" >&2
            dc start server
            exit 1
        fi
    fi
    if ! docker run --rm --network none -v "$data_volume:/data:ro" -v "$backup:/backup" alpine:3.22.1@sha256:4bcff63911fcb4448bd4fdacec207030997caf25e9bea4045fa6c8c44de311d1 sh -c 'cp -a /data/. /backup/'; then
        echo "update.sh: copying the data failed; starting the old server again. The source is at $new_sha and the image is rebuilt. To go back to the old source: $back_to_source" >&2
        dc start server
        exit 1
    fi
    echo "Backup: $backup"
fi

if ! dc up -d --wait; then
    echo "update.sh: the new server did not come up. Backup: $backup. The database may already be migrated, so going back needs the backup:" >&2
    echo "  $back_to_source, then in packaging/docker: docker compose build server, restore the backup into the data volume, then docker compose up -d --wait" >&2
    exit 1
fi
echo "Craft Cloud updated to $new_sha. Backup: $backup"
