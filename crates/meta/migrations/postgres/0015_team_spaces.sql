-- Team spaces. A space gets a kind (`personal` or `team`), a display name (team spaces only, with its case-folded
-- form for the unique index), a creation time and an archive mark, and its owner becomes optional: set exactly for
-- a personal space. Every existing space becomes a personal one with the same id, owner, epoch and counters, and
-- its creation time is its owner's. Postgres names the column-level UNIQUE on `owner` `spaces_owner_key`.
-- `space_members` says who has which role in a team space. A row with no `accepted_ms` is an invitation and gives no
-- access. `invited_by` has no foreign key, as in `audit_events`. A declined invitation, a member who leaves and a
-- removed member are deleted rows.
ALTER TABLE spaces ADD COLUMN kind TEXT NOT NULL DEFAULT 'personal' CHECK (kind IN ('personal', 'team'));

ALTER TABLE spaces ADD COLUMN name TEXT;

ALTER TABLE spaces ADD COLUMN name_fold TEXT;

ALTER TABLE spaces ADD COLUMN created_ms BIGINT NOT NULL DEFAULT 0;

ALTER TABLE spaces ADD COLUMN archived_ms BIGINT;

UPDATE spaces SET created_ms = COALESCE((SELECT u.created_ms FROM users u WHERE u.id = spaces.owner), 0);

ALTER TABLE spaces ALTER COLUMN owner DROP NOT NULL;

ALTER TABLE spaces DROP CONSTRAINT spaces_owner_key;

ALTER TABLE spaces ADD CONSTRAINT spaces_owner_by_kind CHECK ((kind = 'personal') = (owner IS NOT NULL));

ALTER TABLE spaces ADD CONSTRAINT spaces_name_by_kind CHECK ((kind = 'team') = (name IS NOT NULL AND name_fold IS NOT NULL));

CREATE UNIQUE INDEX spaces_personal_owner ON spaces (owner) WHERE owner IS NOT NULL;

CREATE UNIQUE INDEX spaces_team_name ON spaces (name_fold) WHERE kind = 'team';

CREATE TABLE space_members (
    space_id BYTEA NOT NULL REFERENCES spaces (id),
    user_id BYTEA NOT NULL REFERENCES users (id),
    role TEXT NOT NULL CHECK (role IN ('viewer', 'editor', 'owner')),
    invited_ms BIGINT NOT NULL,
    invited_by BYTEA,
    accepted_ms BIGINT,
    PRIMARY KEY (space_id, user_id)
);

CREATE INDEX space_members_user ON space_members (user_id, space_id)
