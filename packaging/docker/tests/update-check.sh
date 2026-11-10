#!/bin/sh
# Tests update-check.sh parsing without network access or Docker.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
script="$here/../update-check.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

sh -n "$script"
CC_UPDATE_CHECK_LIBRARY=1 . "$script"
release=$(parse_response release '{"tag_name":"v1.2.3"}' '2026-11-10T00:00:00Z')
[ "$release" = '{"channel":"release","latest":"v1.2.3","checked_at":"2026-11-10T00:00:00Z"}' ]
commit=$(parse_response branch '{"sha":"0123456789012345678901234567890123456789","commit":{"committer":{"date":"2026-11-10T00:00:00Z"}}}' ignored)
[ "$commit" = '{"channel":"branch","latest":"0123456789012345678901234567890123456789","checked_at":"2026-11-10T00:00:00Z"}' ]
if parse_response release '<html>not json</html>' today >/dev/null 2>&1; then
    echo 'accepted garbage release response' >&2
    exit 1
fi
echo 'update-check parsing tests passed'
