#!/bin/sh
# Tests the Caddyfile and the compose file without Docker: Caddy has no management API, nothing asks the port it would
# have used, and the error log drops the fields that name the client. Run it from anywhere: sh packaging/docker/tests/caddyfile.sh
set -eu

here=$(cd "$(dirname "$0")" && pwd)
caddyfile="$here/../caddy/Caddyfile"
compose="$here/../compose.yaml"
failures=0

fail() {
    echo "FAIL: $*" >&2
    failures=$((failures + 1))
}

# Comments may mention the old port; only lines that are not comments count.
code() {
    grep -v '^[[:space:]]*#' "$1"
}

code "$caddyfile" | grep -Eq '^[[:space:]]*admin[[:space:]]+off[[:space:]]*$' || fail 'the Caddyfile does not switch the admin API off'
if code "$caddyfile" | grep -Eq '^[[:space:]]*admin[[:space:]]+[^o]'; then fail 'the Caddyfile gives the admin API an address'; fi
if code "$compose" | grep -q '2019'; then fail 'compose.yaml still uses the admin API port (2019)'; fi
code "$compose" | grep -q '127.0.0.1:8081/healthz' || fail 'the Caddy health check does not ask the loopback health site'
code "$caddyfile" | grep -q 'respond /healthz 204' || fail 'the Caddyfile has no health site'
for field in request\>uri request\>headers request\>remote_ip request\>remote_port request\>client_ip; do
    code "$caddyfile" | grep -Eq "^[[:space:]]*$field[[:space:]]+delete" || fail "the error log does not delete $field"
done

if [ "$failures" -ne 0 ]; then
    echo "$failures failure(s)" >&2
    exit 1
fi
echo "caddyfile.sh: ok"
