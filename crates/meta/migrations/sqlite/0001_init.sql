CREATE TABLE users (
    id BLOB PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('admin', 'member')),
    created_ms BIGINT NOT NULL
);

CREATE TABLE spaces (
    id BLOB PRIMARY KEY,
    owner BLOB NOT NULL UNIQUE REFERENCES users (id),
    epoch BLOB NOT NULL,
    seq BIGINT NOT NULL,
    oldest_seq BIGINT NOT NULL,
    root BLOB NOT NULL
);

CREATE TABLE devices (
    id BLOB PRIMARY KEY,
    user_id BLOB NOT NULL REFERENCES users (id),
    name TEXT NOT NULL,
    token_hash BLOB NOT NULL UNIQUE,
    created_ms BIGINT NOT NULL,
    last_used_ms BIGINT NOT NULL,
    revoked INTEGER NOT NULL
);

CREATE TABLE nodes (
    space_id BLOB NOT NULL REFERENCES spaces (id),
    id BLOB NOT NULL,
    parent BLOB,
    name TEXT NOT NULL,
    fold_key TEXT NOT NULL,
    is_dir INTEGER NOT NULL,
    version BLOB,
    deleted INTEGER NOT NULL,
    changed_seq BIGINT NOT NULL,
    PRIMARY KEY (space_id, id)
);

CREATE UNIQUE INDEX nodes_live_names ON nodes (space_id, parent, fold_key) WHERE deleted = 0;
CREATE INDEX nodes_changed ON nodes (space_id, changed_seq);

CREATE TABLE versions (
    space_id BLOB NOT NULL REFERENCES spaces (id),
    id BLOB NOT NULL,
    node BLOB NOT NULL,
    parent_version BLOB,
    epoch BLOB NOT NULL,
    manifest BLOB NOT NULL,
    size BIGINT NOT NULL,
    mtime_ms BIGINT NOT NULL,
    exec INTEGER NOT NULL,
    device BLOB NOT NULL,
    created_ms BIGINT NOT NULL,
    seq BIGINT NOT NULL,
    PRIMARY KEY (space_id, id)
);

CREATE TABLE blobs (
    kind SMALLINT NOT NULL,
    id BLOB NOT NULL,
    stored_size BIGINT NOT NULL,
    file_size BIGINT,
    created_ms BIGINT NOT NULL,
    PRIMARY KEY (kind, id)
);

CREATE TABLE space_blobs (
    space_id BLOB NOT NULL REFERENCES spaces (id),
    kind SMALLINT NOT NULL,
    id BLOB NOT NULL,
    PRIMARY KEY (space_id, kind, id)
);
