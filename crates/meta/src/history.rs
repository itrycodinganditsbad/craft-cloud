//! A file's version history: the versions of one file, newest first, a page at a time.
//!
//! Versions are never removed, so every row of `versions` for the node is reported. The `pruned` flag of the answer is
//! part of the contract for the day versions can be thinned; until then it is always false.

use craft_cloud_proto::api::{HISTORY_MAX_LIMIT, HistoryCursor, HistoryVersion};
use craft_cloud_proto::{DeviceId, Epoch, NodeId, SpaceId, VersionId};
use sqlx::Row;

use crate::codec::{flag, to_i64, to_u64, uuid_bytes, uuid_from};
use crate::{Meta, MetaError};

/// One page of a file's history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryPage {
    pub versions: Vec<HistoryVersion>,
    /// The cursor of the next page, if there is one.
    pub next: Option<HistoryCursor>,
}

impl Meta {
    /// One page of the versions of the file `node` in `space`, ordered by `(seq, id)` descending. `after` is the
    /// previous page's `next` (exclusive). `NotFound` when the space has no such node; `Invalid` for a directory or
    /// a `limit` outside 1 to [`HISTORY_MAX_LIMIT`]. A deleted file still has a history. Each call reads one snapshot.
    pub async fn history(&self, space: SpaceId, node: NodeId, after: Option<HistoryCursor>, limit: u32) -> Result<HistoryPage, MetaError> {
        if limit == 0 || limit > HISTORY_MAX_LIMIT {
            return Err(MetaError::Invalid("a history page has 1 to 200 versions"));
        }
        let mut tx = self.read_snapshot().await?;
        let found = sqlx::query("SELECT is_dir FROM nodes WHERE space_id = $1 AND id = $2")
            .bind(uuid_bytes(space.as_uuid()))
            .bind(uuid_bytes(node.as_uuid()))
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(MetaError::NotFound("node"))?;
        if flag(&found, "is_dir")? {
            return Err(MetaError::Invalid("only files have a history"));
        }
        let (cursor_seq, cursor_version) = match after {
            Some(cursor) => (to_i64(cursor.seq)?, uuid_bytes(cursor.version.as_uuid())),
            // Above every sequence number and every ID.
            None => (i64::MAX, vec![0xff; 16]),
        };
        // One row more than asked for tells whether another page follows. The cursor is exclusive.
        let rows = sqlx::query(
            "SELECT id, parent_version, epoch, seq, size, mtime_ms, device, created_ms, \
                    CAST(CASE WHEN save IS NULL THEN 0 ELSE 1 END AS BIGINT) AS has_save \
             FROM versions WHERE space_id = $1 AND node = $2 AND (seq < $3 OR (seq = $3 AND id < $4)) \
             ORDER BY seq DESC, id DESC LIMIT $5",
        )
        .bind(uuid_bytes(space.as_uuid()))
        .bind(uuid_bytes(node.as_uuid()))
        .bind(cursor_seq)
        .bind(cursor_version)
        .bind(i64::from(limit) + 1)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let limit = usize::try_from(limit).map_err(|_| MetaError::Invalid("limit too large"))?;
        let more = rows.len() > limit;
        let mut versions = Vec::with_capacity(rows.len().min(limit));
        for row in rows.iter().take(limit) {
            let parent: Option<Vec<u8>> = row.try_get("parent_version")?;
            versions.push(HistoryVersion {
                id: VersionId::from_uuid(uuid_from(&row.try_get::<Vec<u8>, _>("id")?)?),
                parent_version: parent.as_deref().map(uuid_from).transpose()?.map(VersionId::from_uuid),
                epoch: Epoch::from_uuid(uuid_from(&row.try_get::<Vec<u8>, _>("epoch")?)?),
                seq: to_u64(row.try_get("seq")?)?,
                size: to_u64(row.try_get("size")?)?,
                mtime_ms: row.try_get("mtime_ms")?,
                device: DeviceId::from_uuid(uuid_from(&row.try_get::<Vec<u8>, _>("device")?)?),
                created_ms: row.try_get("created_ms")?,
                save: flag(row, "has_save")?,
                pruned: false,
            });
        }
        let next = if more { versions.last().map(|v| HistoryCursor { seq: v.seq, version: v.id }) } else { None };
        Ok(HistoryPage { versions, next })
    }
}
