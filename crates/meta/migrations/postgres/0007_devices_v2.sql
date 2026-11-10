-- Device sessions v2. A device now says what kind of client it is (`agent`, `desktop` or `cli`, empty when the
-- client didn't say), on what platform and in what version: shown in the device list and never a basis for any
-- permission. A revoked device keeps when and why it was revoked (`user`, `others`, `password`, `admin`,
-- `disabled`, `two_factor`, `idle` or `reset`), set together with `revoked` and only by the first revoke. The
-- `revoked` flag stays the one the server checks. A user keeps when the password last changed through the
-- server (`NULL` when never) and, for a later release, when the account was disabled (`NULL` while it is not).
-- Existing rows stay valid: the new text columns read as empty and the new times as `NULL`.
ALTER TABLE devices ADD COLUMN client_kind TEXT NOT NULL DEFAULT '';
ALTER TABLE devices ADD COLUMN platform TEXT NOT NULL DEFAULT '';
ALTER TABLE devices ADD COLUMN app_version TEXT NOT NULL DEFAULT '';
ALTER TABLE devices ADD COLUMN revoked_ms BIGINT;
ALTER TABLE devices ADD COLUMN revoke_reason TEXT;
ALTER TABLE users ADD COLUMN password_changed_ms BIGINT;
ALTER TABLE users ADD COLUMN disabled_ms BIGINT;
