CREATE TABLE user_two_factor (
    user_id BLOB NOT NULL PRIMARY KEY REFERENCES users(id),
    secret_ciphertext BLOB,
    enabled_ms BIGINT,
    pending_ciphertext BLOB,
    pending_device BLOB,
    pending_expires_ms BIGINT,
    pending_attempts INTEGER NOT NULL DEFAULT 0,
    last_accepted_step BIGINT NOT NULL DEFAULT 0,
    failed_attempts INTEGER NOT NULL DEFAULT 0,
    locked_until_ms BIGINT
);
CREATE TABLE user_recovery_codes (
    user_id BLOB NOT NULL REFERENCES users(id),
    code_hash BLOB NOT NULL,
    used_ms BIGINT,
    PRIMARY KEY (user_id, code_hash)
);
CREATE TABLE login_challenges (
    id_hash BLOB NOT NULL PRIMARY KEY,
    user_id BLOB NOT NULL REFERENCES users(id),
    kind TEXT NOT NULL,
    client_key TEXT NOT NULL,
    device_name TEXT NOT NULL,
    client_kind TEXT NOT NULL DEFAULT '',
    platform TEXT NOT NULL DEFAULT '',
    app_version TEXT NOT NULL DEFAULT '',
    attempts INTEGER NOT NULL DEFAULT 0,
    expires_ms BIGINT NOT NULL
);
CREATE INDEX login_challenges_expiry ON login_challenges (expires_ms);
ALTER TABLE invites ADD COLUMN reset_two_factor INTEGER NOT NULL DEFAULT 0;