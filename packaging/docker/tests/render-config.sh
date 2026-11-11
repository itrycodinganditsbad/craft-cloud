#!/bin/sh
# Tests render-config.sh without Docker: the output of each profile, and the refusal of values that could break the
# TOML or add a setting. Run it from anywhere: sh packaging/docker/tests/render-config.sh
set -eu

here=$(cd "$(dirname "$0")" && pwd)
render="$here/../render-config.sh"
template="$here/../config.toml.in"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM
secrets="$work/secrets"
mkdir "$secrets"
failures=0

fail() {
    echo "FAIL: $*" >&2
    failures=$((failures + 1))
}

password=abcdefghijklmnopqrstuvwxyz012345.-_~ABCDEF
printf '%s' "$password" >"$secrets/db_password"
printf '%s' 'AKIAEXAMPLE0123' >"$secrets/s3_access_key_id"
printf '%s' 'abc/def+ghi=0123' >"$secrets/s3_secret_access_key"

# run <env assignments...>: renders with a clean environment; output in $work/out, errors in $work/err.
run() {
    env -i PATH="$PATH" SECRETS_DIR="$secrets" PUBLIC_URL=https://cloud.example.org TRUSTED_PROXY=172.29.215.2 "$@" \
        sh "$render" "$template" >"$work/out" 2>"$work/err"
}

expect_line() {
    grep -qxF -- "$1" "$work/out" || fail "missing line: $1"
}

reject_line() {
    if grep -qF -- "$1" "$work/out"; then fail "unexpected text: $1"; fi
}

# 1. SQLite and disk (the default).
run || fail "default profile failed: $(cat "$work/err")"
expect_line 'listen = "0.0.0.0:8080"'
expect_line 'behind_proxy = true'
expect_line 'trusted_proxies = ["172.29.215.2"]'
expect_line 'public_url = "https://cloud.example.org"'
expect_line '[updates]'
expect_line 'install_dir = "/opt/craft-cloud"'
expect_line 'repo = "itrycodinganditsbad/craft-cloud"'
expect_line 'url = "sqlite:/var/lib/craft-cloud/meta.db"'
expect_line 'kind = "disk"'
expect_line 'path = "/var/lib/craft-cloud/blobs"'
expect_line '[two_factor]'
expect_line 'key_file = "/var/lib/craft-cloud/two-factor.key"'
expect_line '[web_admin]'
expect_line 'enabled = false'
reject_line 'postgres'
reject_line 's3'
reject_line '#sqlite#'
reject_line "$password"

# The update check is opt-in: without CC_UPDATE_CHECK=on there is no status file.
reject_line 'status_file ='
run CC_UPDATE_CHECK=off || fail "updates-off profile failed: $(cat "$work/err")"
reject_line 'status_file ='
run CC_UPDATE_CHECK=on || fail "updates-on profile failed: $(cat "$work/err")"
expect_line 'status_file = "/var/lib/craft-cloud-updates/status.json"'
# CC_UPDATE_REPO sets the repository the banner links to, with or without the check.
run CC_UPDATE_REPO=someone/craft-cloud.fork || fail "update repo profile failed: $(cat "$work/err")"
expect_line 'repo = "someone/craft-cloud.fork"'
reject_line 'itrycodinganditsbad'

# 2. Postgres.
run DB_KIND=postgres || fail "postgres profile failed: $(cat "$work/err")"
expect_line "url = \"postgres://craft_cloud:$password@db:5432/craft_cloud\""
reject_line 'sqlite:'

# 3. S3 with an endpoint and plain HTTP, and without.
run STORAGE_KIND=s3 S3_BUCKET=files S3_REGION=eu-central-1 S3_PREFIX=cc/ S3_ENDPOINT=http://minio:9000 S3_ALLOW_HTTP=true ||
    fail "s3 profile failed: $(cat "$work/err")"
expect_line 'kind = "s3"'
expect_line 'bucket = "files"'
expect_line 'prefix = "cc/"'
expect_line 'region = "eu-central-1"'
expect_line 'endpoint = "http://minio:9000"'
expect_line 'allow_http = true'
expect_line 'access_key_id = "AKIAEXAMPLE0123"'
expect_line 'secret_access_key = "abc/def+ghi=0123"'
reject_line 'kind = "disk"'
run STORAGE_KIND=s3 S3_BUCKET=files S3_REGION=us-east-1 || fail "plain s3 profile failed: $(cat "$work/err")"
reject_line 'endpoint ='
reject_line 'allow_http ='

# 4. Refusals: each must fail, say so on stderr, and never print the offending value or a secret.
refuse() {
    label=$1
    shift
    if run "$@"; then
        fail "accepted: $label"
        return
    fi
    [ -s "$work/err" ] || fail "no message for: $label"
    if grep -qF -e "$password" -e 'abc/def' "$work/err"; then fail "message shows a secret for: $label"; fi
}

refuse 'quote in the URL' 'PUBLIC_URL=https://a.example"x'
refuse 'backslash in the URL' 'PUBLIC_URL=https://a.example\x'
refuse 'new line in the URL' "PUBLIC_URL=https://a.example
evil = true"
refuse 'http URL' PUBLIC_URL=http://cloud.example.org
refuse 'URL with a path' PUBLIC_URL=https://cloud.example.org/x
refuse 'empty URL' PUBLIC_URL=
refuse 'network instead of an address' TRUSTED_PROXY=172.29.215.0/24
refuse 'quote in the proxy' 'TRUSTED_PROXY=1.2.3.4"]'
refuse 'unknown database' DB_KIND=mysql
refuse 'bad update check value' CC_UPDATE_CHECK=yes
refuse 'update repo with a quote' 'CC_UPDATE_REPO=a/b"c'
refuse 'update repo without a slash' CC_UPDATE_REPO=justaname
refuse 'update repo with a path' CC_UPDATE_REPO=a/b/c
refuse 'unknown storage' STORAGE_KIND=ftp
refuse 'quote in the bucket' STORAGE_KIND=s3 'S3_BUCKET=a"b' S3_REGION=us-east-1
refuse 's3 without a region' STORAGE_KIND=s3 S3_BUCKET=files
refuse 'bad allow_http' STORAGE_KIND=s3 S3_BUCKET=files S3_REGION=us-east-1 S3_ALLOW_HTTP=yes
refuse 'endpoint with a quote' STORAGE_KIND=s3 S3_BUCKET=files S3_REGION=us-east-1 'S3_ENDPOINT=http://a"b'

# A secret that would break the TOML or the URL is refused without a trace of it.
printf '%s' 'short' >"$secrets/db_password"
refuse 'short database password' DB_KIND=postgres
printf '%s' 'abcdefghijklmnopqrstuvwxyz0123456@/x"' >"$secrets/db_password"
refuse 'database password with URL characters' DB_KIND=postgres
printf 'abcdefghijklmnopqrstuvwxyz0123456789
secondline
' >"$secrets/db_password"
refuse 'database password with a second line' DB_KIND=postgres
printf 'abcdefghijklmnopqrstuvwxyz0123456789

' >"$secrets/db_password"
refuse 'database password with an empty second line' DB_KIND=postgres
printf '%s
' "$password" >"$secrets/db_password"
run DB_KIND=postgres || fail "a password with one trailing line feed was refused: $(cat "$work/err")"
expect_line "url = \"postgres://craft_cloud:$password@db:5432/craft_cloud\""
rm "$secrets/db_password"
refuse 'missing database password' DB_KIND=postgres
printf '%s' 'abc"def' >"$secrets/s3_secret_access_key"
refuse 'quote in the S3 secret' STORAGE_KIND=s3 S3_BUCKET=files S3_REGION=us-east-1

# A missing template is an error, not an empty file.
if env -i PATH="$PATH" PUBLIC_URL=https://cloud.example.org TRUSTED_PROXY=172.29.215.2 sh "$render" "$work/missing" >/dev/null 2>&1; then
    fail "accepted a missing template"
fi

if [ "$failures" -ne 0 ]; then
    echo "$failures failure(s)" >&2
    exit 1
fi
echo "render-config tests passed"
