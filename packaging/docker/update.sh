#!/bin/sh
# Back up and upgrade a Docker checkout of Craft Cloud.
#
#   packaging/docker/update.sh [--yes] [--skip-backup] [--keep N]
#
# Git runs at the repository root; every `docker compose` command runs in packaging/docker, where `.env` and the
# COMPOSE_FILE list (the Postgres overlay) live. The order is: fetch, ask, fast-forward to the confirmed commit, build
# the new image while the old server runs, stop the server, dump Postgres (if used), copy the data volume, start.
#
# Every backup is a full copy of the data volume under backups/<UTC time>/. After a successful update the newest N
# backups are kept (N is 3 unless --keep N or CC_BACKUP_KEEP says otherwise; 0 keeps all) and older ones are removed,
# never the one made by this run. A run that fails or skips the backup removes nothing.
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
keep=${CC_BACKUP_KEEP:-3}
while [ $# -gt 0 ]; do
    case $1 in
        --yes) yes=1 ;;
        --skip-backup) skip_backup=1 ;;
        --keep)
            [ $# -ge 2 ] || { echo "update.sh: --keep needs a number" >&2; exit 2; }
            keep=$2
            shift
            ;;
        *) echo "update.sh: unknown option $1" >&2; exit 2 ;;
    esac
    shift
done
case $keep in
    '' | *[!0-9]*) echo "update.sh: the number of backups to keep (--keep, CC_BACKUP_KEEP) must be a whole number, 0 for all" >&2; exit 2 ;;
esac
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
stamp=
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

# prune_backups: after a successful update, keeps the newest $keep backups (this run's included) and removes the older
# ones. Only directories named like a backup of this script are touched. The files in a backup belong to the user of the
# server (uid 65532) and are not readable by you, so a container that runs as root removes them, as it made them.
prune_backups() {
    [ -n "$stamp" ] || return 0
    [ "$keep" -gt 0 ] || { echo "Old backups are kept (--keep 0)."; return 0; }
    [ -d "$root/backups" ] || return 0
    kept=0
    ls -1 "$root/backups" | grep -E '^[0-9]{8}T[0-9]{6}Z$' | sort -r | while IFS= read -r name; do
        # A link or a file with such a name is none of ours: neither counted nor removed.
        if [ ! -d "$root/backups/$name" ] || [ -L "$root/backups/$name" ]; then
            continue
        fi
        if [ "$name" = "$stamp" ] || [ "$kept" -lt "$keep" ]; then
            kept=$((kept + 1))
            continue
        fi
        if docker run --rm --network none -v "$root/backups:/backups" alpine:3.22.1@sha256:4bcff63911fcb4448bd4fdacec207030997caf25e9bea4045fa6c8c44de311d1 sh -c 'rm -rf -- "/backups/$1"' sh "$name"; then
            echo "Removed old backup: $root/backups/$name"
        else
            echo "update.sh: warning: could not remove the old backup $root/backups/$name; remove it by hand" >&2
        fi
    done
}

if ! dc up -d --wait; then
    echo "update.sh: the new server did not come up. Backup: $backup. The database may already be migrated, so going back needs the backup:" >&2
    echo "  $back_to_source, then in packaging/docker: docker compose build server, restore the backup into the data volume, then docker compose up -d --wait" >&2
    exit 1
fi
prune_backups
echo "Craft Cloud updated to $new_sha. Backup: $backup"
