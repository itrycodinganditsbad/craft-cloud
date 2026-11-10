-- craft: foreign-keys-off
-- Team spaces. A space gets a kind (`personal` or `team`), a display name (team spaces only, with its case-folded
-- form for the unique index), a creation time and an archive mark, and its owner becomes optional: set exactly for
-- a personal space. SQLite cannot drop the column-level UNIQUE on `owner` or add the checks in place, so `spaces` is
-- rebuilt as its documentation describes: create the new table, copy every row, drop the old one, rename the new
-- one. The marker on the first line makes the runner turn foreign-key enforcement off for this migration (other
-- tables reference `spaces`) and run `PRAGMA foreign_key_check` before it commits. Every existing space becomes a
-- personal one with the same id, owner, epoch and counters.
-- `space_members` says who has which role in a team space. A row with no `accepted_ms` is an invitation and gives no
-- access. `invited_by` has no foreign key, as in `audit_events`. A declined invitation, a member who leaves and a
-- removed member are deleted rows.
CREATE TABLE spaces_new (
    id BLOB PRIMARY KEY,
    owner BLOB REFERENCES users (id),
    epoch BLOB NOT NULL,
    seq BIGINT NOT NULL,
    oldest_seq BIGINT NOT NULL,
    root BLOB NOT NULL,
    stored_bytes BIGINT NOT NULL DEFAULT 0,
    live_bytes BIGINT NOT NULL DEFAULT 0,
    quota_bytes BIGINT,
    kind TEXT NOT NULL DEFAULT 'personal' CHECK (kind IN ('personal', 'team')),
    name TEXT,
    name_fold TEXT,
    created_ms BIGINT NOT NULL DEFAULT 0,
    archived_ms BIGINT,
    CHECK ((kind = 'personal') = (owner IS NOT NULL)),
    CHECK ((kind = 'team') = (name IS NOT NULL AND name_fold IS NOT NULL))
);

INSERT INTO spaces_new (id, owner, epoch, seq, oldest_seq, root, stored_bytes, live_bytes, quota_bytes, kind, created_ms)
SELECT s.id, s.owner, s.epoch, s.seq, s.oldest_seq, s.root, s.stored_bytes, s.live_bytes, s.quota_bytes, 'personal',
       COALESCE((SELECT u.created_ms FROM users u WHERE u.id = s.owner), 0)
FROM spaces s;

DROP TABLE spaces;

ALTER TABLE spaces_new RENAME TO spaces;

CREATE UNIQUE INDEX spaces_personal_owner ON spaces (owner) WHERE owner IS NOT NULL;

CREATE UNIQUE INDEX spaces_team_name ON spaces (name_fold) WHERE kind = 'team';

CREATE TABLE space_members (
    space_id BLOB NOT NULL REFERENCES spaces (id),
    user_id BLOB NOT NULL REFERENCES users (id),
    role TEXT NOT NULL CHECK (role IN ('viewer', 'editor', 'owner')),
    invited_ms BIGINT NOT NULL,
    invited_by BLOB,
    accepted_ms BIGINT,
    PRIMARY KEY (space_id, user_id)
);

CREATE INDEX space_members_user ON space_members (user_id, space_id)
