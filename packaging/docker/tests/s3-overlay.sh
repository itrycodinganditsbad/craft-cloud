#!/bin/sh
# Tests that the S3 overlay gives the server its own network for outbound access and does not attach it to `edge`.
# On `edge`, Docker's DNS would answer Caddy's lookup of `server` with the address on `edge`, Caddy would connect from an
# address that is not CC_CADDY_IP (so trusted_proxies would not match and X-Forwarded-For would be ignored), and
# update-check, which is on `edge`, could reach the server without Caddy. No Docker needed.
# Run it from anywhere: sh packaging/docker/tests/s3-overlay.sh
set -eu

here=$(cd "$(dirname "$0")" && pwd)
overlay="$here/../compose.s3.yaml"
failures=0

fail() {
    echo "FAIL: $*" >&2
    failures=$((failures + 1))
}

code() {
    grep -v '^[[:space:]]*#' "$1"
}

# The networks line of the server service in the overlay (the service block ends at the next top-level key or service).
server_networks=$(code "$overlay" | awk '/^  server:/ { in_server = 1; next } in_server && /^  [A-Za-z]/ { in_server = 0 } in_server && /networks:/ { print }')
[ -n "$server_networks" ] || fail 'the overlay does not set the networks of the server'
case $server_networks in
    *edge*) fail "the overlay attaches the server to edge: $server_networks" ;;
esac
case $server_networks in
    *egress*) ;;
    *) fail "the server is not on the egress network: $server_networks" ;;
esac
code "$overlay" | grep -Eq '^  egress:' || fail 'the overlay does not define the egress network'
# Only the server may use egress, and it must not be internal (it is the way out).
if code "$overlay" | grep -A3 -E '^  egress:' | grep -q 'internal: true'; then fail 'the egress network is internal, so the server has no way out'; fi
if code "$here/../compose.yaml" | grep -Eq '^[[:space:]]+egress'; then fail 'compose.yaml attaches a service to egress'; fi

if [ "$failures" -ne 0 ]; then
    echo "$failures failure(s)" >&2
    exit 1
fi
echo "s3-overlay.sh: ok"
