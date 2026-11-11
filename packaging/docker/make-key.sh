#!/bin/sh
# make_key <key file> <owner>: makes the two-factor key once (32 random bytes, owner like 65532:65532, mode 0600) and never
# replaces an existing one: with a new key the sealed two-factor secrets in the database could not be opened any more.
#
# Sourced by secrets-init.sh (and by tests/two-factor-key.sh). Runs of it may overlap (a second `docker compose up` while the
# first still runs), so each run writes its own temporary file, created exclusively by mktemp beside the key, and only then
# hard-links it to the key's name. A hard link fails when the name exists and never replaces it, and nothing writes through
# the key's name afterwards: the only name a run ever writes through is one no other run can have. A run removes only its own
# temporary file.
#
# Returns 0 when the key exists afterwards (it prints "made the two-factor key" only for the run that made it) and 1 when it
# could not be made.
make_key() {
    key=$1
    owner=$2
    if [ -e "$key" ] || [ -L "$key" ]; then
        return 0
    fi
    tmp=$(mktemp "$key.new.XXXXXX") || {
        echo "secrets-init: can't make a temporary file for the two-factor key" >&2
        return 1
    }
    if ! head -c 32 /dev/urandom >"$tmp" || [ "$(wc -c <"$tmp")" -ne 32 ]; then
        rm -f -- "$tmp"
        echo "secrets-init: can't read 32 random bytes from /dev/urandom" >&2
        return 1
    fi
    chown "$owner" "$tmp" && chmod 0600 "$tmp" || {
        rm -f -- "$tmp"
        echo "secrets-init: can't set the owner and mode of the two-factor key" >&2
        return 1
    }
    # A hard link fails when a key appeared in the meantime; it never replaces one.
    if ln "$tmp" "$key" 2>/dev/null; then
        rm -f -- "$tmp"
        echo "secrets-init: made the two-factor key"
        return 0
    fi
    rm -f -- "$tmp"
    if [ -e "$key" ]; then
        return 0
    fi
    echo "secrets-init: can't create the two-factor key" >&2
    return 1
}
