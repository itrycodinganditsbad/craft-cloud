#!/bin/sh
# Tests the capabilities of the Caddy service without Docker. `install.sh` creates the certificate directory for the `custom`
# TLS mode with mode 0700, owned by whoever ran it; Caddy's root has no DAC_OVERRIDE (cap_drop: [ALL]), so without
# DAC_READ_SEARCH it cannot open /certs ("permission denied") and exits at start. It may read, and nothing more.
# Run it from anywhere: sh packaging/docker/tests/caddy-capabilities.sh
set -eu

here=$(cd "$(dirname "$0")" && pwd)
compose="$here/../compose.yaml"
failures=0

fail() {
    echo "FAIL: $*" >&2
    failures=$((failures + 1))
}

# The cap_add line of the caddy service (the block ends at the next service).
caps=$(grep -v '^[[:space:]]*#' "$compose" | awk '/^  caddy:/ { in_caddy = 1; next } in_caddy && /^  [A-Za-z]/ { in_caddy = 0 } in_caddy && /cap_add:/ { print }')
[ -n "$caps" ] || fail 'the caddy service has no cap_add'
case $caps in *NET_BIND_SERVICE*) ;; *) fail "caddy cannot bind ports 80 and 443: $caps" ;; esac
case $caps in *DAC_READ_SEARCH*) ;; *) fail "caddy cannot read a certificate that belongs to another user: $caps" ;; esac
case $caps in
    *DAC_OVERRIDE* | *SYS_ADMIN* | *CHOWN* | *FOWNER*) fail "caddy has more than reading and binding: $caps" ;;
esac

if [ "$failures" -ne 0 ]; then
    echo "$failures failure(s)" >&2
    exit 1
fi
echo "caddy-capabilities.sh: ok"
