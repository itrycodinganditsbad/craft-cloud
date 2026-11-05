-- The history of a file lists its versions newest first by (seq, id): this index lets the database walk them in that
-- order and stop at the page size, instead of reading and sorting every version of the space.
CREATE INDEX versions_node_seq ON versions (space_id, node, seq DESC, id DESC);
