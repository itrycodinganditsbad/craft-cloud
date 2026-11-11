#!/bin/sh
# Checks GitHub for a newer Craft Cloud release or branch commit. The server never contacts GitHub.
set -eu

parse_response() {
    channel=$1
    response=$2
    checked_at=$3
    case $channel in
        release)
            latest=$(printf '%s' "$response" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\(v\{0,1\}[0-9][0-9.]*[-.A-Za-z0-9]*\)".*/\1/p' | head -n 1)
            printf '%s' "$latest" | grep -Eq '^v?[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.]+)?$' || return 1
            printf '{"channel":"release","latest":"%s","checked_at":"%s"}\n' "$latest" "$checked_at"
            ;;
        branch)
            latest=$(printf '%s' "$response" | sed -n 's/.*"sha"[[:space:]]*:[[:space:]]*"\([0-9a-f]\{40\}\)".*/\1/p' | head -n 1)
            printf '%s' "$latest" | grep -Eq '^[0-9a-f]{40}$' || return 1
            printf '{"channel":"branch","latest":"%s","checked_at":"%s"}\n' "$latest" "$checked_at"
            ;;
        *) return 1 ;;
    esac
}

# user_agent_for <version>: the generic Craft-Cloud/<version> User-Agent and nothing else about the installation.
user_agent_for() {
    version=$1
    printf '%s' "$version" | grep -Eq '^[A-Za-z0-9.+-]+$' || version=unknown
    printf 'Craft-Cloud/%s' "$version"
}

if [ "${CC_UPDATE_CHECK_LIBRARY:-0}" = 1 ]; then return 0; fi

if [ "${CC_UPDATE_CHECK:-off}" = test ]; then
    parse_response "$@"
    exit $?
fi

repo=${CC_UPDATE_REPO:-itrycodinganditsbad/craft-cloud}
channel=${CC_UPDATE_CHANNEL:-release}
branch=${CC_UPDATE_BRANCH:-main}
status_dir=${CC_UPDATE_STATUS_DIR:-/status}
user_agent=$(user_agent_for "${CC_VERSION:-}")
printf '%s' "$repo" | grep -Eq '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$' || exit 1
case $channel in release) endpoint="https://api.github.com/repos/$repo/releases/latest" ;; branch) endpoint="https://api.github.com/repos/$repo/commits/$branch" ;; *) exit 1 ;; esac
case $branch in *[!A-Za-z0-9._/-]* | '' | /* | *..*) exit 1 ;; esac

first=1
while :; do
    if [ "${CC_UPDATE_CHECK:-off}" = off ]; then sleep 43200; continue; fi
    if [ "$first" -eq 1 ]; then
        delay=$(od -An -N2 -tu2 /dev/urandom | tr -d ' ')
        sleep $((delay % 1801))
        first=0
    fi
    response=$(wget -q -T 30 -O - --header="User-Agent: $user_agent" --header='Accept: application/vnd.github+json' "$endpoint" 2>/dev/null) || { sleep 43200; continue; }
    checked_at=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
    status=$(parse_response "$channel" "$response" "$checked_at") || { sleep 43200; continue; }
    umask 022  # the status is public data; the server reads it as another user
    printf '%s\n' "$status" > "$status_dir/status.json.tmp"
    mv "$status_dir/status.json.tmp" "$status_dir/status.json"
    sleep 43200
done
