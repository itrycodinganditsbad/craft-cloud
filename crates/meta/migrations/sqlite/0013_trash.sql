-- The trash: when a node was deleted, and when its permanent removal was asked for. `deleted_ms` is set when a node
-- goes from live to deleted and cleared when it comes back. `purge_requested_ms` is set by an explicit removal from the
-- trash and only marks the node: nothing is freed yet. Nodes deleted before this migration start their time in the
-- trash at the migration, so no old deletion looks expired the moment a retention period is introduced.
ALTER TABLE nodes ADD COLUMN deleted_ms BIGINT;
ALTER TABLE nodes ADD COLUMN purge_requested_ms BIGINT;
UPDATE nodes SET deleted_ms = CAST(strftime('%s', 'now') AS BIGINT) * 1000 WHERE deleted = 1;
CREATE INDEX nodes_trash ON nodes (space_id, deleted_ms, id) WHERE deleted = 1;
