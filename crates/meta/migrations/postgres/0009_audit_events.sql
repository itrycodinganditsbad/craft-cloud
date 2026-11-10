-- The security audit log. One row per event: when, who did it (`actor_user`, `actor_device`, `NULL` for the command
-- line or a guess at a name nobody has), what (`action`, a name from a closed list in the code), what it was done to
-- (`target_kind`, `target_id`), whether it worked, and a small JSON text of typed values (`details`) that never holds
-- an address, a name of a file or device, a secret or anything a client typed. There are no foreign keys, so an
-- event outlives the account it is about. A failed sign-in is one row per account and minute: `agg_key` names
-- that row and `agg_count` counts the tries in it (rows of other events have a `NULL` key, which a unique index
-- allows many times). Nothing updates or deletes a row except the pruning of old events and that counter.
CREATE TABLE audit_events (
    id BYTEA NOT NULL PRIMARY KEY,
    at_ms BIGINT NOT NULL,
    actor_user BYTEA,
    actor_device BYTEA,
    action TEXT NOT NULL,
    target_kind TEXT NOT NULL DEFAULT '',
    target_id BYTEA,
    outcome TEXT NOT NULL,
    details TEXT NOT NULL DEFAULT '{}',
    agg_key TEXT,
    agg_count BIGINT NOT NULL DEFAULT 1
);

CREATE INDEX audit_events_at ON audit_events (at_ms);

CREATE INDEX audit_events_actor ON audit_events (actor_user, at_ms);

CREATE UNIQUE INDEX audit_events_agg ON audit_events (agg_key);
