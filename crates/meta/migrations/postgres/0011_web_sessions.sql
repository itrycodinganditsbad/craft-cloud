-- Browser sessions of the web admin. A session is a random token in a cookie, of which only the BLAKE3 hash is kept
-- here (`id_hash`). `auth_gen` is the account's generation when the session was made, so every change that raises
-- `users.auth_gen` (a password change or reset, a disable, an enable, a removal, turning two-factor sign-in on or off)
-- ends the session with it. `last_seen_ms` is moved at most once a minute and carries the idle expiry,
-- `absolute_expires_ms` the hard one, `revoked_ms` is a logout. `mfa_verified_ms` is when a second factor was given
-- at sign-in (`NULL` for an account without one). There is no password, no token and no address in a row.
CREATE TABLE web_sessions (
    id_hash BYTEA NOT NULL PRIMARY KEY,
    user_id BYTEA NOT NULL REFERENCES users(id),
    auth_gen BIGINT NOT NULL,
    created_ms BIGINT NOT NULL,
    last_seen_ms BIGINT NOT NULL,
    absolute_expires_ms BIGINT NOT NULL,
    revoked_ms BIGINT,
    mfa_verified_ms BIGINT
);
CREATE INDEX web_sessions_user ON web_sessions (user_id);
CREATE INDEX web_sessions_expiry ON web_sessions (absolute_expires_ms)
