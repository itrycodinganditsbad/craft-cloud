-- Live documents linked to files, and snapshots. A document of a file has `node` set (its kind is `document:<app>`).
-- `base_seq` is the last update its snapshot holds: the rows at or below it are compacted (kept, but no longer counted
-- against the document's cap nor sent to an agent that can read the snapshot), and `compacted_bytes` is their size
-- (`bytes` still counts every row). `snapshot` is the 32-byte ID of the manifest that names the snapshot (uploaded
-- through the chunk API as the space's) and `snapshot_bytes` is its size. A document made by import has a snapshot
-- from the start, with `base_seq` 0, and keeps that first one as `import_snapshot` (with `import_bytes`): the log
-- doesn't hold the import, so an agent that can't read a later snapshot reads the import and then the whole log.
-- `spaces.live_bytes` is the bytes every live document of the space holds (every row, compacted ones too), which its
-- cap on live bytes is checked against.
ALTER TABLE live_docs ADD COLUMN base_seq BIGINT NOT NULL DEFAULT 0;

ALTER TABLE live_docs ADD COLUMN import_snapshot BYTEA;

ALTER TABLE live_docs ADD COLUMN import_bytes BIGINT NOT NULL DEFAULT 0;

ALTER TABLE live_docs ADD COLUMN compacted_bytes BIGINT NOT NULL DEFAULT 0;

ALTER TABLE live_docs ADD COLUMN snapshot BYTEA;

ALTER TABLE live_docs ADD COLUMN snapshot_bytes BIGINT NOT NULL DEFAULT 0;

ALTER TABLE spaces ADD COLUMN live_bytes BIGINT NOT NULL DEFAULT 0;

UPDATE spaces SET live_bytes = (SELECT CAST(COALESCE(SUM(d.bytes), 0) AS BIGINT) FROM live_docs d WHERE d.space_id = spaces.id)
