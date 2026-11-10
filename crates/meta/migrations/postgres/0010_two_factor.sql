CREATE TABLE user_two_factor (
    user_id BYTEA NOT NULL PRIMARY KEY REFERENCES users(id),
    secret_ciphertext BYTEA,
    enabled_ms BIGINT,
    pending_ciphertext BYTEA,
    pending_device BYTEA,
    pending_expires_ms BIGINT,
    pending_attempts BIGINT NOT NULL DEFAULT 0,
    last_accepted_step BIGINT NOT NULL DEFAULT 0,
    failed_attempts BIGINT NOT NULL DEFAULT 0,
    locked_until_ms BIGINT
);
CREATE TABLE user_recovery_codes (
    user_id BYTEA NOT NULL REFERENCES users(id),
    code_hash BYTEA NOT NULL,
    used_ms BIGINT,
    PRIMARY KEY (user_id, code_hash)
);
CREATE TABLE login_challenges (
    id_hash BYTEA NOT NULL PRIMARY KEY,
    user_id BYTEA NOT NULL REFERENCES users(id),
    kind TEXT NOT NULL,
    client_key TEXT NOT NULL,
    device_name TEXT NOT NULL,
    client_kind TEXT NOT NULL DEFAULT '',
    platform TEXT NOT NULL DEFAULT '',
    app_version TEXT NOT NULL DEFAULT '',
    attempts BIGINT NOT NULL DEFAULT 0,
    expires_ms BIGINT NOT NULL
);
CREATE INDEX login_challenges_expiry ON login_challenges (expires_ms);
ALTER TABLE invites ADD COLUMN reset_two_factor BIGINT NOT NULL DEFAULT 0;