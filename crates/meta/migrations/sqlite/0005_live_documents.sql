-- Live documents linked to files, and snapshots. A document of a file has `node` set (its kind is `document:<app>`).
-- `base_seq` is the last update its snapshot holds: the rows at or below it are compacted (kept, but no longer counted
-- in `bytes` nor sent to an agent that can read the snapshot). `snapshot` names the snapshot's blobs in the chunk
-- store (the 32-byte IDs of its pieces, one after another) and `snapshot_bytes` is its size. A document made by import
-- has a snapshot from the start, with `base_seq` 0. `bytes` counts only the rows above `base_seq` from now on.
ALTER TABLE live_docs ADD COLUMN base_seq BIGINT NOT NULL DEFAULT 0;

ALTER TABLE live_docs ADD COLUMN snapshot BLOB;

ALTER TABLE live_docs ADD COLUMN snapshot_bytes BIGINT NOT NULL DEFAULT 0
