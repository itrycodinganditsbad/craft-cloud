#!/bin/sh
case $- in *x*) set +x ;; esac
# One-shot setup of the files the other services read (service `secrets-init`).
#
# Compose can hand a value from .env to a service only as an environment variable or as a file owned by root, but
# the server runs as uid 65532 and Postgres as its own user. So this container, which has no network and runs for a
# second, is the only one that sees the secrets from .env. It writes
#
#   server/config.toml                     65532  the rendered config (render-config.sh), mode 0400
#   db/db_superuser_password               root   read by the Postgres entrypoint          (Postgres profile)
#   db-init/db_superuser_password          70     read by db-init.sh                       (Postgres profile)
#   db-init/db_password                    70     the password of the server's database role (Postgres profile)
#
# into the `secrets` volume (directories 0500). Each service mounts only its own directory, read-only. Uid 70 is the
# postgres user of the postgres:*-alpine images.
#
# Nothing here prints a secret. Every run rewrites all files, so changing a value in .env and running
# `docker compose run --rm secrets-init` is how a password or a setting is changed; restart the server afterwards.
#
# The directories are mounted into running containers (volume subpaths), so they are never deleted or recreated:
# each file is written next to its place and moved over the old one, and only files that no longer belong are removed.
set -eu
umask 077

OUT=/out
SRC=/run/secrets
DB_KIND=${DB_KIND:-sqlite}

# put <uid> <directory> <file>: reads the content from standard input.
put() {
    dir="$OUT/$2"
    install -d -m 0500 -o "$1" -g "$1" "$dir"
    tmp="$dir/.new.$3"
    cat >"$tmp"
    chown "$1:$1" "$tmp"
    chmod 0400 "$tmp"
    mv -f "$tmp" "$dir/$3"
}

# prune <directory> <file>...: removes every file in the directory that is not listed (a directory that does not
# exist is left alone).
prune() {
    dir="$OUT/$1"
    shift
    [ -d "$dir" ] || return 0
    for path in "$dir"/* "$dir"/.[!.]*; do
        [ -e "$path" ] || continue
        keep=0
        for name in "$@"; do
            [ "$path" = "$dir/$name" ] && keep=1
        done
        [ "$keep" = 1 ] || rm -f -- "$path"
    done
}

# The config: rendered into a variable first, so a refused value leaves the old file untouched.
config=$(sh /render-config.sh /config.toml.in)
printf '%s\n' "$config" | put 65532 server config.toml
unset config
prune server config.toml

case $DB_KIND in
    postgres)
        superuser_password=$(cat "$SRC/db_superuser_password")
        case $superuser_password in
            "" | *[!A-Za-z0-9._~-]*)
                echo "secrets-init: CC_SECRET_DB_SUPERUSER_PASSWORD is empty or has characters outside A-Z a-z 0-9 . _ ~ -" >&2
                exit 1
                ;;
        esac
        if [ "${#superuser_password}" -lt 32 ]; then
            echo "secrets-init: CC_SECRET_DB_SUPERUSER_PASSWORD is shorter than 32 characters" >&2
            exit 1
        fi
        unset superuser_password
        # The database password was checked by render-config.sh.
        put 0 db db_superuser_password <"$SRC/db_superuser_password"
        put 70 db-init db_superuser_password <"$SRC/db_superuser_password"
        put 70 db-init db_password <"$SRC/db_password"
        prune db db_superuser_password
        prune db-init db_superuser_password db_password
        ;;
    *)
        # Left over from an earlier Postgres profile: the passwords no longer belong anywhere.
        prune db
        prune db-init
        ;;
esac

echo "secrets-init: done"
