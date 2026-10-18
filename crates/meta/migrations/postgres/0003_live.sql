-- Live documents: each space's documents and the log of their updates. The server stores updates as opaque bytes
-- and never parses them. `head_seq` is a document's last sequence number, so an append takes the next one in the
-- same statement that locks the document. `client_seq` is the sending device's name for an update, so an update
-- sent again after a lost ack is found and acked with its first sequence number. `bytes` is the size of the
-- document's log, kept under its cap in the same statement.
CREATE TABLE live_docs (
    space_id BYTEA NOT NULL REFERENCES spaces (id),
    id BYTEA NOT NULL,
    kind TEXT NOT NULL,
    node BYTEA,
    created_ms BIGINT NOT NULL,
    head_seq BIGINT NOT NULL,
    bytes BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (space_id, id)
);

CREATE TABLE live_updates (
    space_id BYTEA NOT NULL,
    doc BYTEA NOT NULL,
    seq BIGINT NOT NULL,
    bytes BYTEA NOT NULL,
    device BYTEA NOT NULL,
    client_seq BIGINT NOT NULL,
    created_ms BIGINT NOT NULL,
    PRIMARY KEY (space_id, doc, seq),
    FOREIGN KEY (space_id, doc) REFERENCES live_docs (space_id, id)
);

CREATE INDEX live_updates_sent ON live_updates (space_id, doc, device, client_seq);

-- The live service's own generation per space: the admin's `live reset-doc` raises it, and the live `welcome` names
-- an epoch made of the space's epoch and this, so live agents start over while the file sync's epoch is untouched.
CREATE TABLE live_spaces (
    space_id BYTEA NOT NULL PRIMARY KEY REFERENCES spaces (id),
    generation BIGINT NOT NULL
);
