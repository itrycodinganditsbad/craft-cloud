-- What each space stores: the stored size of every blob it has, counted once per space.
ALTER TABLE spaces ADD COLUMN stored_bytes BIGINT NOT NULL DEFAULT 0;

UPDATE spaces SET stored_bytes = (
    SELECT COALESCE(SUM(b.stored_size), 0) FROM space_blobs sb JOIN blobs b ON b.kind = sb.kind AND b.id = sb.id
    WHERE sb.space_id = spaces.id
);
