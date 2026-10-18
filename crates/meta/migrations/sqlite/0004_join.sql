-- Join codes, stored only as BLAKE3 hashes. An invite belongs to an account (one at a time, a new one replaces it)
-- and its row is deleted when it is redeemed. A pairing (an add-a-computer code) belongs to the signed-in device that
-- made it (one at a time) and keeps `used_by`, the device that joined with it, so its maker sees who joined. That
-- reference is checked at commit, because a redeem marks the pairing used before it creates the device. Expired rows
-- are deleted on the next write to their table. An account an invite created has `password_hash = ''` until the
-- invite is redeemed with a new password.
CREATE TABLE invites (
    user_id BLOB NOT NULL PRIMARY KEY REFERENCES users (id),
    code_hash BLOB NOT NULL UNIQUE,
    expires_ms BIGINT NOT NULL
);

CREATE TABLE pairings (
    id BLOB NOT NULL PRIMARY KEY,
    user_id BLOB NOT NULL REFERENCES users (id),
    device_id BLOB NOT NULL UNIQUE REFERENCES devices (id),
    code_hash BLOB NOT NULL UNIQUE,
    expires_ms BIGINT NOT NULL,
    used_by BLOB REFERENCES devices (id) DEFERRABLE INITIALLY DEFERRED
);
