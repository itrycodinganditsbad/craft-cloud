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
# checked_at is the time of the check, never a date from the commit.
commit=$(parse_response branch '{"sha":"0123456789012345678901234567890123456789","commit":{"author":{"date":"2020-01-01T00:00:00Z"},"committer":{"date":"2020-01-02T00:00:00Z"}}}' 2026-11-10T00:00:00Z)
[ "$commit" = '{"channel":"branch","latest":"0123456789012345678901234567890123456789","checked_at":"2026-11-10T00:00:00Z"}' ]
[ "$(parse_response branch '{"sha":"0123456789012345678901234567890123456789"}' 2026-11-10T00:00:00Z)" = "$commit" ]
if parse_response branch '{"sha":"abc"}' 2026-11-10T00:00:00Z >/dev/null 2>&1; then
    echo 'accepted a short commit id' >&2
    exit 1
fi
# The User-Agent is generic: the version and nothing else; a missing or odd version becomes unknown.
[ "$(user_agent_for 0.1.0)" = 'Craft-Cloud/0.1.0' ]
[ "$(user_agent_for '')" = 'Craft-Cloud/unknown' ]
[ "$(user_agent_for 'a b;c')" = 'Craft-Cloud/unknown' ]
if parse_response release '<html>not json</html>' today >/dev/null 2>&1; then
    echo 'accepted garbage release response' >&2
    exit 1
fi
# The check is opt-in: off in the checker, in compose and in the example .env.
grep -q 'CC_UPDATE_CHECK:-off' "$script"
grep -q 'CC_UPDATE_CHECK: \${CC_UPDATE_CHECK:-off}' "$here/../compose.yaml"
grep -q '^CC_UPDATE_CHECK=off' "$here/../.env.example"
echo 'update-check parsing tests passed'
