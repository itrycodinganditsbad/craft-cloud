//! The trash: the deleted nodes of a space, and the request to remove one of them for good.
//!
//! Deleting stays what it always was, a tombstone in the change feed. The trash is that same set of tombstones seen
//! with the time of the deletion. Bringing a node back is [`Meta::restore_from_trash`] in `commit.rs`, because it
//! shares the commit path's rules for names. Removing a node from the trash here only marks it: nothing is freed,
//! so versions, chunks and the tombstone stay as they were.
//!
//! A node is in the trash when it is deleted and its removal was not asked for. What is below a folder in the trash
//! is listed only through the folder (the folder's restore and removal take it along), whenever it was deleted.

use craft_cloud_proto::api::{TRASH_MAX_LIMIT, TrashCursor};
use craft_cloud_proto::limits::MAX_TREE_DEPTH;
use craft_cloud_proto::{NodeId, SpaceId};
use sqlx::Row;

use crate::codec::{flag, to_u64, uuid_bytes, uuid_from};
use crate::tree::space_info;
use crate::{Dialect, Meta, MetaError};

/// Why a folder with too many deleted items below it is refused (`invalid`), for a restore and for a removal.
pub(crate) const TRASH_TREE_TOO_LARGE: &str =
    "this folder holds too many deleted items to restore or remove at once; do it for the folders inside it first";

/// A deleted node in the trash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashEntry {
    pub id: NodeId,
    pub parent: NodeId,
    pub name: String,
    pub is_dir: bool,
    /// The size of a file's last version.
    pub size: Option<u64>,
    pub deleted_ms: i64,
}

/// One page of the trash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashPage {
    pub entries: Vec<TrashEntry>,
    /// The cursor of the next page, if there is one.
    pub next: Option<TrashCursor>,
    /// How many deleted nodes without a deletion time were found and stamped with `now` (a damaged database: the
    /// migration and the commit path always set it). They are never treated as expired.
    pub repaired: u64,
}

/// The deleted nodes below the node `$2` of space `$1` that are in the trash, found through nodes that are in the trash
/// themselves (a folder that was removed from the trash hides what is below it); `depth` is 1 for a child. `$3` bounds
/// the depth. A statement continues after it with `$4` and up.
pub(crate) const TRASH_BELOW: &str = "WITH RECURSIVE sub(id, depth) AS (\
         SELECT id, 1 FROM nodes WHERE space_id = $1 AND parent = $2 AND deleted = 1 AND purge_requested_ms IS NULL \
         UNION ALL \
         SELECT n.id, sub.depth + 1 FROM nodes n JOIN sub ON n.parent = sub.id \
         WHERE n.space_id = $1 AND n.deleted = 1 AND n.purge_requested_ms IS NULL AND sub.depth < $3) ";

impl Meta {
    /// One page of the trash of `space`: its deleted nodes that were not removed from the trash, newest deletion
    /// first by `(deleted_ms, id)`, after `after` (exclusive). `Invalid` for a `limit` outside 1 to
    /// [`TRASH_MAX_LIMIT`]. A deleted node without a deletion time gets `now_ms` first, so the trash never lists a
    /// node as older than it can be shown to be.
    pub async fn trash(&self, space: SpaceId, after: Option<TrashCursor>, limit: u32, now_ms: i64) -> Result<TrashPage, MetaError> {
        if limit == 0 || limit > TRASH_MAX_LIMIT {
            return Err(MetaError::Invalid("a trash page has 1 to 200 items"));
        }
        let repaired = self.stamp_unstamped(space, now_ms).await?;
        let mut tx = self.read_snapshot().await?;
        let (cursor_ms, cursor_node) = match after {
            Some(cursor) => (cursor.deleted_ms, uuid_bytes(cursor.node.as_uuid())),
            // Above every time and every ID.
            None => (i64::MAX, vec![0xff; 16]),
        };
        // One row more than asked for tells whether another page follows. The row-value comparison and the order
        // match the index `nodes_trash`.
        let rows = sqlx::query(
            "SELECT n.id, n.parent, n.name, n.is_dir, n.deleted_ms, v.size AS v_size \
             FROM nodes n LEFT JOIN versions v ON v.space_id = n.space_id AND v.id = n.version \
             WHERE n.space_id = $1 AND n.deleted = 1 AND n.purge_requested_ms IS NULL AND (n.deleted_ms, n.id) < ($2, $3) \
             AND NOT EXISTS (SELECT 1 FROM nodes p WHERE p.space_id = n.space_id AND p.id = n.parent AND p.deleted = 1 AND p.purge_requested_ms IS NULL) \
             ORDER BY n.deleted_ms DESC, n.id DESC LIMIT $4",
        )
        .bind(uuid_bytes(space.as_uuid()))
        .bind(cursor_ms)
        .bind(cursor_node)
        .bind(i64::from(limit) + 1)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let limit = usize::try_from(limit).map_err(|_| MetaError::Invalid("limit too large"))?;
        let more = rows.len() > limit;
        let mut entries = Vec::with_capacity(rows.len().min(limit));
        for row in rows.iter().take(limit) {
            let parent: Option<Vec<u8>> = row.try_get("parent")?;
            let size: Option<i64> = row.try_get("v_size")?;
            entries.push(TrashEntry {
                id: NodeId::from_uuid(uuid_from(&row.try_get::<Vec<u8>, _>("id")?)?),
                parent: NodeId::from_uuid(uuid_from(&parent.ok_or(MetaError::Corrupt("a deleted node has no parent"))?)?),
                name: row.try_get("name")?,
                is_dir: flag(row, "is_dir")?,
                size: size.map(to_u64).transpose()?,
                deleted_ms: row.try_get("deleted_ms")?,
            });
        }
        let next = if more { entries.last().map(|e| TrashCursor { deleted_ms: e.deleted_ms, node: e.id }) } else { None };
        Ok(TrashPage { entries, next, repaired })
    }

    /// Gives every deleted node of `space` that has no deletion time `now_ms`; returns how many. Looks first without
    /// the write lock, since there are none in a sound database.
    async fn stamp_unstamped(&self, space: SpaceId, now_ms: i64) -> Result<u64, MetaError> {
        let space_key = uuid_bytes(space.as_uuid());
        let any = sqlx::query("SELECT 1 FROM nodes WHERE space_id = $1 AND deleted = 1 AND deleted_ms IS NULL LIMIT 1")
            .bind(space_key.clone())
            .fetch_optional(&self.pool)
            .await?;
        if any.is_none() {
            return Ok(0);
        }
        let mut w = self.begin_write().await?;
        space_info(&mut w.tx, space, self.dialect == Dialect::Postgres).await?;
        let done = sqlx::query("UPDATE nodes SET deleted_ms = $1 WHERE space_id = $2 AND deleted = 1 AND deleted_ms IS NULL")
            .bind(now_ms)
            .bind(space_key)
            .execute(&mut *w.tx)
            .await?;
        w.commit().await?;
        Ok(done.rows_affected())
    }

    /// Takes the deleted node `node` out of the trash: it is no longer listed and can't be restored from the trash.
    /// Only marks it (`purge_requested_ms`): no version, chunk or tombstone is touched, so the change feed and the
    /// sequence number stay as they are. Asking again changes nothing. `NotFound` when `node` is not a deleted node of
    /// `space` in the trash. Removing a folder marks the nodes in the trash below it too (all, whenever they were
    /// deleted); `Invalid` and nothing marked when there are more than [`Meta::with_trash_tree_cap`] of them.
    pub async fn request_purge(&self, space: SpaceId, node: NodeId, now_ms: i64) -> Result<(), MetaError> {
        let mut w = self.begin_write().await?;
        // On Postgres this row lock orders the mark with commits and restores of the space.
        space_info(&mut w.tx, space, self.dialect == Dialect::Postgres).await?;
        let row = sqlx::query("SELECT deleted, purge_requested_ms, is_dir FROM nodes WHERE space_id = $1 AND id = $2")
            .bind(uuid_bytes(space.as_uuid()))
            .bind(uuid_bytes(node.as_uuid()))
            .fetch_optional(&mut *w.tx)
            .await?
            .ok_or(MetaError::NotFound("node in the trash"))?;
        if !flag(&row, "deleted")? {
            return Err(MetaError::NotFound("node in the trash"));
        }
        if row.try_get::<Option<i64>, _>("purge_requested_ms")?.is_some() {
            return Ok(());
        }
        if flag(&row, "is_dir")? {
            // What is in the trash below the folder goes with it, or it would stay hidden and unmarked. A folder with
            // more than the cap is refused whole.
            let bound = i64::try_from(MAX_TREE_DEPTH + 2).map_err(|_| MetaError::Corrupt("depth limit out of range"))?;
            let cap = i64::try_from(self.trash_tree_cap).map_err(|_| MetaError::Corrupt("trash cap out of range"))?;
            let counted = sqlx::query(sqlx::AssertSqlSafe(format!("{TRASH_BELOW} SELECT COUNT(*) AS c FROM (SELECT id FROM sub LIMIT $4) AS below")))
                .bind(uuid_bytes(space.as_uuid()))
                .bind(uuid_bytes(node.as_uuid()))
                .bind(bound)
                .bind(cap.saturating_add(1))
                .fetch_one(&mut *w.tx)
                .await?;
            if counted.try_get::<i64, _>("c")? > cap {
                return Err(MetaError::Invalid(TRASH_TREE_TOO_LARGE));
            }
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "{TRASH_BELOW} UPDATE nodes SET purge_requested_ms = $4 WHERE space_id = $1 AND id IN (SELECT id FROM sub)"
            )))
            .bind(uuid_bytes(space.as_uuid()))
            .bind(uuid_bytes(node.as_uuid()))
            .bind(bound)
            .bind(now_ms)
            .execute(&mut *w.tx)
            .await?;
        }
        sqlx::query("UPDATE nodes SET purge_requested_ms = $1 WHERE space_id = $2 AND id = $3")
            .bind(now_ms)
            .bind(uuid_bytes(space.as_uuid()))
            .bind(uuid_bytes(node.as_uuid()))
            .execute(&mut *w.tx)
            .await?;
        w.commit().await?;
        Ok(())
    }
}
