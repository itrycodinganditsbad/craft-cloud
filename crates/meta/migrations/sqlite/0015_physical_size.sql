-- What a blob really takes on the backend, next to stored_size (its logical size, the plain bytes plus the format
-- byte, which quotas count). physical_size is the length of the stored object, and NULL for blobs written before
-- it was tracked: unknown until the blob is written again, recompressed or measured. compress_level is the zstd
-- level the object was last compressed or tried at (0: not tried), NULL when unknown, so a recompress pass can skip
-- what it has already done.
ALTER TABLE blobs ADD COLUMN physical_size BIGINT;

ALTER TABLE blobs ADD COLUMN compress_level BIGINT;

-- Small server-owned values: the admin's compression level, the last estimate of what higher levels would save.
CREATE TABLE server_state (
    key TEXT NOT NULL PRIMARY KEY,
    value TEXT NOT NULL,
    updated_ms BIGINT NOT NULL
);
