#!/bin/sh
# Back up and upgrade a Docker checkout of Craft Cloud.
set -eu

cd "$(dirname "$0")/../.."
yes=0
skip_backup=0
for arg in "$@"; do
    case $arg in
        --yes) yes=1 ;;
        --skip-backup) skip_backup=1 ;;
        *) echo "update.sh: unknown option $arg" >&2; exit 2 ;;
    esac
done
git diff --quiet || { echo "update.sh: working tree has local changes" >&2; exit 1; }
git diff --cached --quiet || { echo "update.sh: working tree has staged changes" >&2; exit 1; }
[ -z "$(git ls-files --others --exclude-standard | grep -v '^backups/' || true)" ] || { echo "update.sh: working tree has untracked files" >&2; exit 1; }
old_sha=$(git rev-parse HEAD)
git fetch --prune
upstream=$(git rev-parse '@{upstream}')
if [ "$upstream" = "$old_sha" ]; then
    echo "Craft Cloud is up to date at $old_sha."
    exit 0
fi
if [ "$yes" -ne 1 ]; then
    printf 'Update Craft Cloud from %s to %s? [y/N] ' "$old_sha" "$upstream"
    IFS= read -r answer || answer=
    [ "$answer" = y ] || [ "$answer" = Y ] || exit 1
fi
git pull --ff-only
new_sha=$(git rev-parse HEAD)

# Build while the old server keeps running: a failed build changes nothing.
version=$(./packaging/docker/workspace-version.sh)
if ! CC_VERSION=$version CC_REVISION=$new_sha docker compose build server; then
    echo "update.sh: the build failed; the running server is unchanged. To go back to the old source: git checkout $old_sha" >&2
    exit 1
fi

backup=skipped
if [ "$skip_backup" -eq 1 ]; then
    echo "WARNING: backup skipped by explicit request."
else
    stamp=$(date -u '+%Y%m%dT%H%M%SZ')
    backup="$(pwd)/backups/$stamp"
    server_id=$(docker compose ps -aq server | head -n 1)
    [ -n "$server_id" ] || { echo "update.sh: server container not found; refusing to update without a backup (or pass --skip-backup)" >&2; exit 1; }
    data_volume=$(docker inspect --format '{{range .Mounts}}{{if eq .Destination "/var/lib/craft-cloud"}}{{.Name}}{{end}}{{end}}' "$server_id")
    [ -n "$data_volume" ] || { echo "update.sh: data volume not found; refusing to update without a backup (or pass --skip-backup)" >&2; exit 1; }
    mkdir -p "$backup"
    if docker compose config --services | grep -qx db; then
        docker compose exec -T db sh -c 'pg_dump -U "$POSTGRES_USER" "$POSTGRES_DB"' > "$backup/database.sql"
        [ -s "$backup/database.sql" ] || { echo "update.sh: the Postgres backup failed; nothing was changed" >&2; exit 1; }
    fi
    # The server stops for the copy, so the SQLite files and blobs are consistent.
    docker compose stop server
    if ! docker run --rm --network none -v "$data_volume:/data:ro" -v "$backup:/backup" alpine:3.22.1@sha256:4bcff63911fcb4448bd4fdacec207030997caf25e9bea4045fa6c8c44de311d1 sh -c 'cp -a /data/. /backup/'; then
        echo "update.sh: copying the data failed; starting the old server again" >&2
        docker compose start server
        exit 1
    fi
    echo "Backup: $backup"
fi

if ! docker compose up -d --wait; then
    echo "update.sh: the new server did not come up. Backup: $backup. The database may already be migrated, so going back needs the backup:" >&2
    echo "  git checkout $old_sha && docker compose build server, restore the backup into the data volume, then docker compose up -d --wait" >&2
    exit 1
fi
echo "Craft Cloud updated to $new_sha. Backup: $backup"
