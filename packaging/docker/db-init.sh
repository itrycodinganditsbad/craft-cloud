#!/bin/sh
case $- in *x*) set +x ;; esac
# One-shot role setup for the Postgres profile (service `db-init`). It runs as the postgres user of the Postgres
# image, reads the passwords from secret files and never prints them. Safe to run again: every run applies the
# password and the role's attributes, which is also how the password is rotated.
#
# The server migrates its database when it opens it, so its role owns the database and the schema `public` and may
# change them. That is less separation than an owner role plus a runtime role would give. The role is not a
# superuser and has no other privileges, and nobody else may connect to its database or to the other databases.
set -eu
umask 077

# The superuser password goes into a private password file (mode 0600, on the tmpfs at /tmp, removed on exit), so it
# is neither in a process's environment nor on a command line. The password only holds the characters
# A-Za-z0-9._~-, which need no escaping in this file format.
passfile=$(mktemp /tmp/pgpass.XXXXXX)
trap 'rm -f "$passfile"' EXIT INT TERM
printf '*:*:*:postgres:%s\n' "$(cat /run/secrets/db_superuser_password)" >"$passfile"
PGUSER=postgres
PGPASSFILE=$passfile
export PGUSER PGPASSFILE

# POSTGRES_PASSWORD_FILE only applies when the data directory is first created, so a changed superuser password in
# .env cannot log in here.
if ! psql --no-psqlrc --quiet --command 'SELECT 1' >/dev/null 2>&1; then
    cat >&2 <<'MSG'
db-init: cannot log in as the Postgres superuser "postgres" (or the database is unreachable).
If CC_SECRET_DB_SUPERUSER_PASSWORD in .env was changed after the first start, the database still has the old
password: the image applies it only when the data directory is created. Put the old value back in .env.
MSG
    exit 1
fi

# psql reads the password itself (backticks), so it is not on a command line.
psql --no-psqlrc --set ON_ERROR_STOP=1 --quiet <<'SQL'
\set db_password `cat /run/secrets/db_password`

SELECT 'CREATE ROLE craft_cloud LOGIN'
 WHERE NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'craft_cloud')\gexec

ALTER ROLE craft_cloud LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS PASSWORD :'db_password';

ALTER DATABASE craft_cloud OWNER TO craft_cloud;
ALTER SCHEMA public OWNER TO craft_cloud;
REVOKE ALL ON DATABASE craft_cloud FROM PUBLIC;
REVOKE CREATE ON SCHEMA public FROM PUBLIC;
-- The role must not be able to connect to the maintenance databases either.
REVOKE CONNECT ON DATABASE postgres FROM PUBLIC;
REVOKE CONNECT ON DATABASE template1 FROM PUBLIC;
SQL

echo "db-init: the database role craft_cloud is ready"
