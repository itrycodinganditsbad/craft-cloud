-- Admin API. A row of `invites` now says what it is for: `invite` (a new computer for an account, as before) or
-- `reset` (a password reset code the admin made, good for an hour and redeemed through the same join route). A
-- space may carry a quota of its own that replaces the server-wide cap for that account (`NULL`: the cap applies),
-- and a user may be marked for removal (`removed_ms`, `NULL` while it is not): the account is disabled and left
-- out of the lists, and its data stays until a later release frees it. Existing rows stay valid: every old
-- invite reads as `invite`, and no space has a quota or user a removal mark. `users.auth_gen` counts the changes
-- that end what a sign-in checked before them (a disable, an enable, a removal, a password change or reset): a
-- sign-in that read it before verifying the password creates its device only if it is still the same afterwards.
ALTER TABLE invites ADD COLUMN purpose TEXT NOT NULL DEFAULT 'invite';
ALTER TABLE spaces ADD COLUMN quota_bytes BIGINT;
ALTER TABLE users ADD COLUMN removed_ms BIGINT;
ALTER TABLE users ADD COLUMN auth_gen BIGINT NOT NULL DEFAULT 0;
