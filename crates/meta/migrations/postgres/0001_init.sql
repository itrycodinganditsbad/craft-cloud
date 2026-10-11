CREATE TABLE users (
    id BYTEA PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('admin', 'member')),
    created_ms BIGINT NOT NULL
);

CREATE TABLE spaces (
    id BYTEA PRIMARY KEY,
    owner BYTEA NOT NULL UNIQUE REFERENCES users (id),
    epoch BYTEA NOT NULL,
    seq BIGINT NOT NULL,
    oldest_seq BIGINT NOT NULL,
    root BYTEA NOT NULL
);

CREATE TABLE devices (
    id BYTEA PRIMARY KEY,
    user_id BYTEA NOT NULL REFERENCES users (id),
    name TEXT NOT NULL,
    token_hash BYTEA NOT NULL UNIQUE,
    created_ms BIGINT NOT NULL,
    last_used_ms BIGINT NOT NULL,
    revoked BIGINT NOT NULL
);

CREATE TABLE nodes (
    space_id BYTEA NOT NULL REFERENCES spaces (id),
    id BYTEA NOT NULL,
    parent BYTEA,
    name TEXT NOT NULL,
    fold_key TEXT NOT NULL,
    is_dir BIGINT NOT NULL,
    version BYTEA,
    deleted BIGINT NOT NULL,
    changed_seq BIGINT NOT NULL,
    PRIMARY KEY (space_id, id)
);

CREATE UNIQUE INDEX nodes_live_names ON nodes (space_id, parent, fold_key) WHERE deleted = 0;
CREATE INDEX nodes_changed ON nodes (space_id, changed_seq);

CREATE TABLE versions (
    space_id BYTEA NOT NULL REFERENCES spaces (id),
    id BYTEA NOT NULL,
    node BYTEA NOT NULL,
    parent_version BYTEA,
    epoch BYTEA NOT NULL,
    manifest BYTEA NOT NULL,
    size BIGINT NOT NULL,
    mtime_ms BIGINT NOT NULL,
    exec BIGINT NOT NULL,
    device BYTEA NOT NULL,
    created_ms BIGINT NOT NULL,
    seq BIGINT NOT NULL,
    PRIMARY KEY (space_id, id)
);

CREATE TABLE blobs (
    kind SMALLINT NOT NULL,
    id BYTEA NOT NULL,
    stored_size BIGINT NOT NULL,
    file_size BIGINT,
    created_ms BIGINT NOT NULL,
    PRIMARY KEY (kind, id)
);

CREATE TABLE space_blobs (
    space_id BYTEA NOT NULL REFERENCES spaces (id),
    kind SMALLINT NOT NULL,
    id BYTEA NOT NULL,
    PRIMARY KEY (space_id, kind, id)
);
