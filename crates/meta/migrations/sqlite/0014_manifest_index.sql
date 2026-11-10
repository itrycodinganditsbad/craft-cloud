-- The reference index: which chunks each manifest names, in order. It is derived data: the manifest object in the
-- store is the authority, and a row of the index is written from a manifest that was read and checked against its ID.
-- `manifest_index` holds, per manifest, how many chunks it names, how many edges are written (the ordinals 0 up to
-- that number, never a gap) and whether the whole list was checked against the manifest: state 0 is being built, state
-- 1 is complete. A manifest without a row is not indexed yet. Nothing in this index is used to free anything.
CREATE TABLE manifest_chunks (
    manifest BLOB NOT NULL,
    ordinal BIGINT NOT NULL,
    chunk BLOB NOT NULL,
    PRIMARY KEY (manifest, ordinal)
);
CREATE INDEX manifest_chunks_by_chunk ON manifest_chunks (chunk, manifest);
CREATE TABLE manifest_index (
    manifest BLOB NOT NULL PRIMARY KEY,
    chunk_count BIGINT NOT NULL,
    indexed_ordinals BIGINT NOT NULL,
    state BIGINT NOT NULL
);
