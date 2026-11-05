WITH space_manifests AS (
    SELECT sb.id AS manifest FROM space_blobs sb WHERE sb.space_id = $1 AND sb.kind = 1
), incomplete AS (
    SELECT 1 FROM space_manifests sm
    LEFT JOIN manifest_index i ON i.manifest = sm.manifest
    WHERE i.manifest IS NULL OR i.state <> 1 OR i.indexed_ordinals <> i.chunk_count
       OR i.chunk_count <> (SELECT COUNT(*) FROM manifest_chunks mc WHERE mc.manifest = sm.manifest)
    LIMIT 1
), version_roles AS (
    SELECT v.manifest,
        MAX(CASE WHEN n.deleted = 1 THEN 1 ELSE 0 END) AS has_deleted,
        MAX(CASE WHEN n.deleted = 0 THEN 1 ELSE 0 END) AS has_live,
        MAX(CASE WHEN n.deleted = 0 AND n.version = v.id THEN 1 ELSE 0 END) AS has_current,
        MAX(CASE WHEN n.deleted = 0 AND n.version <> v.id THEN 1 ELSE 0 END) AS has_history,
        MAX(CASE WHEN n.deleted = 0 AND v.save IS NOT NULL THEN 1 ELSE 0 END) AS has_saved
    FROM nodes n JOIN versions v ON v.space_id = n.space_id AND v.node = n.id
    WHERE n.space_id = $1
    GROUP BY v.manifest
), snapshot_manifests AS (
    SELECT snapshot AS manifest FROM live_docs WHERE space_id = $1 AND snapshot IS NOT NULL
    UNION
    SELECT import_snapshot AS manifest FROM live_docs WHERE space_id = $1 AND import_snapshot IS NOT NULL
), classified AS (
    SELECT sm.manifest, b.stored_size,
        CASE WHEN vr.has_deleted = 1 AND vr.has_live = 0 AND snapshots.manifest IS NULL
             THEN 1 ELSE 0 END AS trash_only,
        CASE WHEN vr.has_history = 1 AND vr.has_deleted = 0 AND vr.has_current = 0 AND vr.has_saved = 0 AND snapshots.manifest IS NULL
             THEN 1 ELSE 0 END AS history_only
    FROM space_manifests sm JOIN blobs b ON b.kind = 1 AND b.id = sm.manifest
    LEFT JOIN version_roles vr ON vr.manifest = sm.manifest
    LEFT JOIN snapshot_manifests snapshots ON snapshots.manifest = sm.manifest
), eligible_chunks AS (
    SELECT sb.id, b.stored_size,
        MIN(CASE WHEN c.trash_only = 1 THEN 1 ELSE 0 END) AS trash_only,
        MIN(CASE WHEN c.history_only = 1 THEN 1 ELSE 0 END) AS history_only
    FROM space_blobs sb JOIN blobs b ON b.kind = 0 AND b.id = sb.id
    JOIN manifest_chunks mc ON mc.chunk = sb.id
    JOIN space_manifests sm ON sm.manifest = mc.manifest
    JOIN classified c ON c.manifest = sm.manifest
    WHERE sb.space_id = $1
    GROUP BY sb.id, b.stored_size
)
SELECT s.stored_bytes,
    CASE WHEN EXISTS (SELECT 1 FROM incomplete) THEN NULL ELSE
        CAST(COALESCE((SELECT SUM(stored_size) FROM classified WHERE trash_only = 1), 0) AS BIGINT) +
        CAST(COALESCE((SELECT SUM(stored_size) FROM eligible_chunks WHERE trash_only = 1), 0) AS BIGINT) END AS trash_bytes,
    CASE WHEN EXISTS (SELECT 1 FROM incomplete) THEN NULL ELSE
        CAST(COALESCE((SELECT SUM(stored_size) FROM classified WHERE history_only = 1), 0) AS BIGINT) +
        CAST(COALESCE((SELECT SUM(stored_size) FROM eligible_chunks WHERE history_only = 1), 0) AS BIGINT) END AS history_bytes
FROM spaces s WHERE s.id = $1
