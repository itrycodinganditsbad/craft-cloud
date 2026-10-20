-- A version's save record (when a Craft app saved it from a live document): the JSON of
-- `craft_cloud_proto::SaveRecord` (the document and its version vector then), stored as the agent sent it and never
-- read beyond that. Older versions, and versions made outside Craft, have none.
ALTER TABLE versions ADD COLUMN save TEXT;
