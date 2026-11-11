#!/bin/sh
# Sets up the Docker deployment of Craft Cloud in this directory: checks the requirements, writes `.env` (mode 0600)
# with random database passwords, builds the image, starts the stack and, if asked, creates the first admin.
#
#   ./install.sh [--domain cloud.example.org] [--tls acme|internal|custom] [--email you@example.org]
#                [--db sqlite|postgres] [--http-port 80] [--https-port 443] [--admin name | --no-admin] [--no-start]
#                [--update-check]
#
# Missing values are asked for when a terminal is attached; without one, --domain is required. The admin's password
# is read from standard input without echo (or from a pipe: printf '%s\n' "$PASSWORD" | ./install.sh ... --admin you)
# and goes to the server over a pipe: never into an argument, the environment or a log.
#
# The update check (a service that asks api.github.com every 12 hours whether a newer Craft Cloud exists) is off unless
# you answer y when asked, or pass --update-check. Without a terminal it is off unless --update-check is given.
#
# It never overwrites an existing `.env`. It runs from a clone of the repository or an unpacked release archive; it
# downloads nothing itself (Docker pulls the pinned base images).
set -eu
umask 077

cd "$(dirname "$0")"

die() {
    echo "install.sh: $*" >&2
    exit 1
}

domain=
tls_mode=
acme_email=
db_kind=
http_port=
https_port=
admin=
no_admin=0
no_start=0
update_check=off
interactive=0
[ -t 0 ] && interactive=1

while [ $# -gt 0 ]; do
    case $1 in
        --domain | --tls | --email | --db | --http-port | --https-port | --admin)
            [ $# -ge 2 ] || die "$1 needs a value"
            case $1 in
                --domain) domain=$2 ;;
                --tls) tls_mode=$2 ;;
                --email) acme_email=$2 ;;
                --db) db_kind=$2 ;;
                --http-port) http_port=$2 ;;
                --https-port) https_port=$2 ;;
                --admin) admin=$2 ;;
            esac
            shift 2
            ;;
        --no-admin)
            no_admin=1
            shift
            ;;
        --no-start)
            no_start=1
            shift
            ;;
        --update-check)
            update_check=on
            shift
            ;;
        -h | --help)
            sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) die "unknown option $1 (see --help)" ;;
    esac
done

# --- requirements ---------------------------------------------------------------------------------------------------
command -v docker >/dev/null 2>&1 || die "docker is not installed"
compose_version=$(docker compose version --short 2>/dev/null) || die "docker compose (version 2) is not available"
# Volume subpaths, which the secret distribution relies on, need Compose 2.32.
major=${compose_version%%.*}
rest=${compose_version#*.}
minor=${rest%%.*}
case "$major$minor" in
    *[!0-9]*) die "can't read the Docker Compose version '$compose_version'" ;;
esac
if [ "$major" -lt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -lt 32 ]; }; then
    die "Docker Compose 2.32 or newer is needed (found $compose_version)"
fi
[ ! -e .env ] || die ".env exists already; edit it, or move it away to set up again"

free_kib=$(df -Pk . | awk 'NR == 2 { print $4 }')
case $free_kib in
    "" | *[!0-9]*) ;;
    *) [ "$free_kib" -ge 4194304 ] || die "less than 4 GiB of free disk space here; the image build needs room" ;;
esac

# --- settings -------------------------------------------------------------------------------------------------------
ask() {
    # ask <variable> <question> <default>
    if [ "$interactive" -eq 1 ]; then
        printf '%s [%s]: ' "$2" "$3"
        IFS= read -r answer || answer=
        eval "$1=\${answer:-\$3}"
    else
        eval "$1=\$3"
    fi
}

if [ -z "$domain" ]; then
    [ "$interactive" -eq 1 ] || die "--domain is required without a terminal"
    printf 'Domain the server is reached at (for example cloud.example.org): '
    IFS= read -r domain || domain=
fi
[ -n "$tls_mode" ] || ask tls_mode "TLS (acme, internal, custom)" acme
[ -n "$db_kind" ] || ask db_kind "Database (sqlite, postgres)" sqlite
[ -n "$http_port" ] || ask http_port "HTTP port" 80
[ -n "$https_port" ] || ask https_port "HTTPS port" 443
if [ "$tls_mode" = acme ] && [ -z "$acme_email" ] && [ "$interactive" -eq 1 ]; then
    printf 'Email for Let'\''s Encrypt notices (optional): '
    IFS= read -r acme_email || acme_email=
fi

if [ "$update_check" = off ] && [ "$interactive" -eq 1 ]; then
    printf 'Check GitHub for a newer Craft Cloud every 12 hours? [y/N] '
    IFS= read -r answer || answer=
    case $answer in y | Y) update_check=on ;; esac
fi

printf '%s' "$domain" | grep -Eq '^[A-Za-z0-9]([A-Za-z0-9.-]*[A-Za-z0-9])?$' || die "the domain has characters that are not allowed"
case $tls_mode in acme | internal | custom) ;; *) die "--tls must be acme, internal or custom" ;; esac
case $db_kind in sqlite | postgres) ;; *) die "--db must be sqlite or postgres" ;; esac
for port in "$http_port" "$https_port"; do
    case $port in "" | *[!0-9]* | 0*) die "ports are numbers from 1 to 65535" ;; esac
    [ "$port" -le 65535 ] || die "ports are numbers from 1 to 65535"
done
if [ -n "$acme_email" ]; then
    printf '%s' "$acme_email" | grep -Eq '^[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+$' || die "the email address has characters that are not allowed"
fi
if [ -n "$admin" ]; then
    printf '%s' "$admin" | grep -Eq '^[A-Za-z0-9._-]{1,64}$' || die "usernames are 1-64 letters, digits, '.', '_' or '-'"
fi

if command -v ss >/dev/null 2>&1; then
    for port in "$http_port" "$https_port"; do
        if ss -ltn 2>/dev/null | awk '{ print $4 }' | grep -Eq "[:.]$port\$"; then
            die "port $port is already in use; pick another with --http-port / --https-port"
        fi
    done
fi

public_url="https://$domain"
[ "$https_port" = 443 ] || public_url="$public_url:$https_port"

version=$(sh ./workspace-version.sh)
revision=$(git rev-parse HEAD 2>/dev/null || true)
[ -n "$revision" ] || revision=unknown
install_dir=$(cd ../.. && pwd)
# The checkout path is only shown in the update banner, but render-config.sh refuses other characters, which would stop
# the stack at its first start. Fall back to a placeholder the administrator reads as "your checkout".
if ! printf '%s' "$install_dir" | grep -Eq '^[A-Za-z0-9._/ -]+$'; then
    echo "install.sh: warning: the checkout path has characters that the update banner can't show; it will say /path/to/craft-cloud instead." >&2
    install_dir=/path/to/craft-cloud
fi

# --- .env ---------------------------------------------------------------------------------------------------------
# A random password of 40 URL-safe characters (the database password becomes part of a database URL).
random_password() {
    value=$(LC_ALL=C tr -dc 'A-Za-z0-9' </dev/urandom 2>/dev/null | head -c 40 || true)
    [ "${#value}" -eq 40 ] || die "can't read random bytes from /dev/urandom"
    printf '%s' "$value"
}

compose_file=compose.yaml
[ "$db_kind" != postgres ] || compose_file=compose.yaml:compose.postgres.yaml

{
    echo "# Written by install.sh. Holds the database passwords: keep it private (mode 0600) and out of version control."
    echo "COMPOSE_PATH_SEPARATOR=:"
    echo "COMPOSE_FILE=$compose_file"
    echo "CC_DOMAIN=$domain"
    echo "CC_PUBLIC_URL=$public_url"
    echo "CC_TLS_MODE=$tls_mode"
    echo "CC_ACME_EMAIL=$acme_email"
    echo "CC_HTTP_PORT=$http_port"
    echo "CC_HTTPS_PORT=$https_port"
    echo "CC_IMAGE=craft-cloud:$version"
    echo "CC_VERSION=$version"
    echo "CC_REVISION=$revision"
    echo "CC_INSTALL_DIR=$install_dir"
    echo "CC_UPDATE_CHECK=$update_check"
    if [ "$db_kind" = postgres ]; then
        echo "CC_SECRET_DB_SUPERUSER_PASSWORD=$(random_password)"
        echo "CC_SECRET_DB_PASSWORD=$(random_password)"
    fi
} >.env.new
chmod 600 .env.new
mv .env.new .env
echo "Wrote .env (mode 0600)."

if [ "$tls_mode" = custom ]; then
    mkdir -p tls
    chmod 700 tls
    echo "Put fullchain.pem and private.key in $(pwd)/tls before the stack starts, then run: docker compose up -d --wait"
    no_start=1
fi

[ "$no_start" -eq 0 ] || {
    echo "Not started. Next: docker compose build && docker compose up -d --wait"
    exit 0
}

# --- build and start ----------------------------------------------------------------------------------------------
echo "Building the image craft-cloud:$version (this takes a while the first time)..."
docker compose build server
echo "Starting the stack..."
docker compose up -d --wait

# --- first admin ----------------------------------------------------------------------------------------------------
if [ "$no_admin" -eq 0 ] && [ -z "$admin" ] && [ "$interactive" -eq 1 ]; then
    printf 'Name of the first admin (empty to skip): '
    IFS= read -r admin || admin=
    printf '%s' "$admin" | grep -Eq '^([A-Za-z0-9._-]{1,64})?$' || die "usernames are 1-64 letters, digits, '.', '_' or '-'"
fi

if [ "$no_admin" -eq 0 ] && [ -n "$admin" ]; then
    password=
    if [ "$interactive" -eq 1 ]; then
        trap 'stty echo 2>/dev/null || true' EXIT INT TERM
        stty -echo
        printf 'Password for %s: ' "$admin"
        IFS= read -r password || password=
        printf '\nRepeat the password: '
        IFS= read -r again || again=
        stty echo
        printf '\n'
        [ "$password" = "$again" ] || die "the passwords differ; create the admin later (see docs/server.md)"
        unset again
    else
        IFS= read -r password || password=
    fi
    [ -n "$password" ] || die "no password given; create the admin later (see docs/server.md)"
    # The password travels through a pipe into the container's standard input. A refusal (a password the server's rules
    # do not accept) is shown by the server without the password.
    if printf '%s\n' "$password" | docker compose run --rm --no-deps -T server user --config /etc/craft-cloud/config.toml add --admin "$admin"; then
        unset password
    else
        unset password
        die "the admin was not created; see the message above, then run it by hand (see docs/server.md)"
    fi
fi

echo
echo "Craft Cloud runs at $public_url"
[ "$tls_mode" != internal ] || echo "TLS mode 'internal': the certificate is from Caddy's own CA; clients must be told to trust it (curl -k for a test)."
echo "Two-factor sign-in is on. Its key is in the data volume (two-factor.key): back it up apart from the database."
echo "Next: docker compose ps, and docker compose logs -f server"
