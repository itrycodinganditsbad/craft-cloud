#!/bin/sh
# Renders the server's config.toml from config.toml.in and writes it to standard output.
#
#   render-config.sh <template>
#
# Settings come from the environment:
#   PUBLIC_URL      https://host[:port]          (required)
#   TRUSTED_PROXY   the proxy's IPv4 address     (required)
#   DB_KIND         sqlite (default) or postgres
#   STORAGE_KIND    disk (default) or s3
#   S3_BUCKET S3_PREFIX S3_REGION S3_ENDPOINT S3_ALLOW_HTTP   with STORAGE_KIND=s3 (bucket and region required)
# Secrets are read from files in $SECRETS_DIR (default /run/secrets), never from the environment:
#   db_password                                   with DB_KIND=postgres
#   s3_access_key_id, s3_secret_access_key        with STORAGE_KIND=s3
#
# Every value is checked against a short list of allowed characters before it is written, so none can break the
# TOML or add a setting. A secret is only handled by shell built-ins (never in a command line or a process list) and
# is never printed, not even in an error message.
set -u
case $- in *x*) set +x ;; esac
set -e
umask 077

die() {
    echo "render-config: $*" >&2
    exit 1
}

nl='
'

# check <what> <value> <extended regex>: the value has no line break and matches the regex.
check() {
    case $2 in *"$nl"*) die "$1 must be one line" ;; esac
    printf '%s' "$2" | grep -Eq "$3" || die "$1 has a value that is empty or has characters that are not allowed"
}

template=${1:-}
[ -n "$template" ] || die "usage: render-config.sh <template>"
[ -r "$template" ] || die "can't read the template"
secrets=${SECRETS_DIR:-/run/secrets}

# secret <file name>: prints nothing; sets $secret_value to the file's first line.
secret() {
    [ -r "$secrets/$1" ] || die "the secret file $1 is missing"
    IFS= read -r secret_value <"$secrets/$1" || [ -n "$secret_value" ] || die "the secret file $1 is empty"
}

public_url=${PUBLIC_URL:-}
check "PUBLIC_URL" "$public_url" '^https://[A-Za-z0-9]([A-Za-z0-9.-]*[A-Za-z0-9])?(:[0-9]{1,5})?$'
trusted_proxy=${TRUSTED_PROXY:-}
check "TRUSTED_PROXY" "$trusted_proxy" '^[0-9]{1,3}(\.[0-9]{1,3}){3}$'

db_kind=${DB_KIND:-sqlite}
storage_kind=${STORAGE_KIND:-disk}
active=" $db_kind $storage_kind "
db_password=
s3_bucket=
s3_prefix=
s3_region=
s3_endpoint=
s3_key_id=
s3_secret_key=

case $db_kind in
    sqlite) ;;
    postgres)
        secret db_password
        db_password=$secret_value
        # The password is part of a database URL, and a URL-safe password needs no escaping.
        check "the database password" "$db_password" '^[A-Za-z0-9._~-]{32,}$'
        ;;
    *) die "DB_KIND must be sqlite or postgres" ;;
esac

case $storage_kind in
    disk) ;;
    s3)
        s3_bucket=${S3_BUCKET:-}
        check "S3_BUCKET" "$s3_bucket" '^[A-Za-z0-9][A-Za-z0-9.-]*$'
        s3_prefix=${S3_PREFIX:-}
        if [ -n "$s3_prefix" ]; then check "S3_PREFIX" "$s3_prefix" '^[A-Za-z0-9._/-]+$'; fi
        s3_region=${S3_REGION:-}
        check "S3_REGION" "$s3_region" '^[A-Za-z0-9-]+$'
        s3_endpoint=${S3_ENDPOINT:-}
        if [ -n "$s3_endpoint" ]; then
            check "S3_ENDPOINT" "$s3_endpoint" '^https?://[A-Za-z0-9][A-Za-z0-9.:/_-]*$'
            active="$active s3endpoint "
        fi
        case ${S3_ALLOW_HTTP:-false} in
            true) active="$active s3http " ;;
            false) ;;
            *) die "S3_ALLOW_HTTP must be true or false" ;;
        esac
        secret s3_access_key_id
        s3_key_id=$secret_value
        check "the S3 access key id" "$s3_key_id" '^[A-Za-z0-9+/=._-]+$'
        secret s3_secret_access_key
        s3_secret_key=$secret_value
        check "the S3 secret access key" "$s3_secret_key" '^[A-Za-z0-9+/=._-]+$'
        ;;
    *) die "STORAGE_KIND must be disk or s3" ;;
esac

# replace <name> <value>: puts the value where the line has @name@ (once per line, which is all the template needs).
replace() {
    case $line in *"@$1@"*) line=${line%%"@$1@"*}$2${line#*"@$1@"} ;; esac
}

while IFS= read -r line || [ -n "$line" ]; do
    case $line in
        '#'?*'#'*)
            marker=${line#\#}
            marker=${marker%%\#*}
            case $marker in
                *[!a-z0-9]*) ;; # a comment that happens to hold a second # keeps its line as it is
                *)
                    case $active in
                        *" $marker "*) line=${line#\#"$marker"\#} ;;
                        *) continue ;;
                    esac
                    ;;
            esac
            ;;
    esac
    replace TRUSTED_PROXY "$trusted_proxy"
    replace PUBLIC_URL "$public_url"
    replace DB_PASSWORD "$db_password"
    replace S3_BUCKET "$s3_bucket"
    replace S3_PREFIX "$s3_prefix"
    replace S3_REGION "$s3_region"
    replace S3_ENDPOINT "$s3_endpoint"
    replace S3_ACCESS_KEY_ID "$s3_key_id"
    replace S3_SECRET_ACCESS_KEY "$s3_secret_key"
    case $line in *@[A-Z]*@*) die "the template has a placeholder that was not filled" ;; esac
    printf '%s\n' "$line"
done <"$template"
