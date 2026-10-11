//! Reading the tree: the full listing and the change feed.
//!
//! Every node records the sequence number of the commit that last changed it (`changed_seq`), and deleted
//! nodes stay as tombstones, so "what changed since N" is one indexed query and reports deletions too.

use craft_cloud_proto::limits::MAX_PAGE_NODES;
use craft_cloud_proto::{ChangeSet, DeviceId, Epoch, FileState, ManifestId, Name, NodeId, NodeKind, NodeState, PageCursor, SpaceId, VersionId};
use sqlx::Row;
use sqlx::any::AnyRow;

use crate::codec::{flag, hash_from, to_i64, to_u64, uuid_bytes, uuid_from};
use crate::{Meta, MetaError};

/// Node columns plus its current version (if a file).
const NODE_SELECT: &str = "SELECT n.id, n.parent, n.name, n.is_dir, n.deleted, n.changed_seq, v.id AS v_id, v.manifest AS v_manifest, v.size AS v_size, \
     v.mtime_ms AS v_mtime, v.exec AS v_exec, v.device AS v_device, v.epoch AS v_epoch \
     FROM nodes n LEFT JOIN versions v ON v.space_id = n.space_id AND v.id = n.version";

/// A space's epoch, current sequence number and root directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpaceInfo {
    pub epoch: Epoch,
    pub seq: u64,
    pub oldest_seq: u64,
    pub root: NodeId,
}

pub(crate) async fn space_info(conn: &mut sqlx::AnyConnection, space: SpaceId, for_update: bool) -> Result<SpaceInfo, MetaError> {
    let sql = if for_update {
        "SELECT epoch, seq, oldest_seq, root FROM spaces WHERE id = $1 FOR UPDATE"
    } else {
        "SELECT epoch, seq, oldest_seq, root FROM spaces WHERE id = $1"
    };
    let row = sqlx::query(sql).bind(uuid_bytes(space.as_uuid())).fetch_optional(&mut *conn).await?.ok_or(MetaError::NotFound("space"))?;
    Ok(SpaceInfo {
        epoch: Epoch::from_uuid(uuid_from(&row.try_get::<Vec<u8>, _>("epoch")?)?),
        seq: to_u64(row.try_get("seq")?)?,
        oldest_seq: to_u64(row.try_get("oldest_seq")?)?,
        root: NodeId::from_uuid(uuid_from(&row.try_get::<Vec<u8>, _>("root")?)?),
    })
}

fn node_state(row: &AnyRow) -> Result<NodeState, MetaError> {
    let parent: Option<Vec<u8>> = row.try_get("parent")?;
    let name: String = row.try_get("name")?;
    let is_dir = flag(row, "is_dir")?;
    let kind = if is_dir {
        NodeKind::Dir
    } else {
        let version: Option<Vec<u8>> = row.try_get("v_id")?;
        let version = version.ok_or(MetaError::Corrupt("a file has no current version"))?;
        NodeKind::File(FileState {
            version: VersionId::from_uuid(uuid_from(&version)?),
            manifest: ManifestId::from_bytes(hash_from(&row.try_get::<Vec<u8>, _>("v_manifest")?)?),
            size: to_u64(row.try_get("v_size")?)?,
            mtime_ms: row.try_get("v_mtime")?,
            exec: flag(row, "v_exec")?,
            device: DeviceId::from_uuid(uuid_from(&row.try_get::<Vec<u8>, _>("v_device")?)?),
            epoch: Epoch::from_uuid(uuid_from(&row.try_get::<Vec<u8>, _>("v_epoch")?)?),
        })
    };
    Ok(NodeState {
        id: NodeId::from_uuid(uuid_from(&row.try_get::<Vec<u8>, _>("id")?)?),
        parent: parent.as_deref().map(uuid_from).transpose()?.map(NodeId::from_uuid),
        name: if parent.is_none() { None } else { Some(Name::new(&name).map_err(|_| MetaError::Corrupt("a stored name is invalid"))?) },
        deleted: flag(row, "deleted")?,
        kind,
    })
}

impl Meta {
    /// Starts a read transaction that sees one consistent snapshot. On Postgres the isolation level has to be
    /// set as the transaction's first statement. SQLite read transactions are already snapshots.
    async fn read_snapshot(&self) -> Result<sqlx::Transaction<'static, sqlx::Any>, MetaError> {
        let mut tx = self.pool.begin().await?;
        if self.dialect == crate::Dialect::Postgres {
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY").execute(&mut *tx).await?;
        }
        Ok(tx)
    }

    pub async fn space_info(&self, space: SpaceId) -> Result<SpaceInfo, MetaError> {
        let mut conn = self.pool.acquire().await?;
        space_info(&mut conn, space, false).await
    }

    /// One page of the live nodes, root included, ordered by `(changed seq, id)`; parents before children is
    /// not guaranteed (sort by depth on the client). `after` is the previous page's `next`. Each call reads one
    /// snapshot, so a page and its `seq` agree; a client resumes the change feed from the `seq` of the FIRST
    /// page of a listing, and then applies the feed on top of what it listed.
    pub async fn tree(&self, space: SpaceId, after: Option<PageCursor>, limit: u32) -> Result<ChangeSet, MetaError> {
        self.page(space, None, after, limit).await
    }

    /// One page of the nodes changed after `since`, deletions included, ordered like [`Meta::tree`].
    /// `ResyncRequired` when `since` is older than the oldest change still known, or newer than the space's
    /// current sequence (the server was restored). A client resumes from the `seq` of the first page.
    pub async fn changes(&self, space: SpaceId, since: u64, after: Option<PageCursor>, limit: u32) -> Result<ChangeSet, MetaError> {
        self.page(space, Some(since), after, limit).await
    }

    async fn page(&self, space: SpaceId, since: Option<u64>, after: Option<PageCursor>, limit: u32) -> Result<ChangeSet, MetaError> {
        if limit == 0 || limit > MAX_PAGE_NODES {
            return Err(MetaError::Invalid("a page has 1 to 5000 nodes"));
        }
        let mut tx = self.read_snapshot().await?;
        let info = space_info(&mut tx, space, false).await?;
        let since = match since {
            Some(since) if since < info.oldest_seq || since > info.seq => return Err(MetaError::ResyncRequired),
            Some(since) => to_i64(since)?,
            None => -1,
        };
        let (cursor_seq, cursor_node) = match after {
            Some(cursor) => (to_i64(cursor.seq)?, uuid_bytes(cursor.node.as_uuid())),
            None => (-1, vec![0u8; 16]),
        };
        // Fetch one row more than asked for, to learn whether another page follows.
        let sql = format!(
            "{NODE_SELECT} WHERE n.space_id = $1 AND n.changed_seq > $2 AND (n.changed_seq > $3 OR (n.changed_seq = $3 AND n.id > $4)){} \
             ORDER BY n.changed_seq, n.id LIMIT $5",
            if since < 0 { " AND n.deleted = 0" } else { "" }
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(uuid_bytes(space.as_uuid()))
            .bind(since)
            .bind(cursor_seq)
            .bind(cursor_node)
            .bind(i64::from(limit) + 1)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        let more = rows.len() > usize::try_from(limit).map_err(|_| MetaError::Invalid("limit too large"))?;
        let kept = if more { rows.len() - 1 } else { rows.len() };
        let nodes = rows.iter().take(kept).map(node_state).collect::<Result<Vec<_>, _>>()?;
        let next = match (more, kept.checked_sub(1).and_then(|i| rows.get(i)), nodes.last()) {
            (true, Some(row), Some(last)) => Some(PageCursor { seq: to_u64(row.try_get("changed_seq")?)?, node: last.id }),
            _ => None,
        };
        Ok(ChangeSet { epoch: info.epoch, seq: info.seq, nodes, next })
    }

    /// Every space on the server, in ID order.
    pub async fn all_spaces(&self) -> Result<Vec<SpaceId>, MetaError> {
        let rows = sqlx::query("SELECT id FROM spaces ORDER BY id").fetch_all(&self.pool).await?;
        rows.iter().map(|row| Ok(SpaceId::from_uuid(uuid_from(&row.try_get::<Vec<u8>, _>("id")?)?))).collect()
    }

    /// Starts a new epoch for the space: run after restoring the server from a backup, so every agent
    /// reconciles instead of trusting its change counter.
    pub async fn reset_epoch(&self, space: SpaceId) -> Result<Epoch, MetaError> {
        let epoch = Epoch::generate();
        let mut w = self.begin_write().await?;
        let done = sqlx::query("UPDATE spaces SET epoch = $1 WHERE id = $2")
            .bind(uuid_bytes(epoch.as_uuid()))
            .bind(uuid_bytes(space.as_uuid()))
            .execute(&mut *w.tx)
            .await?;
        if done.rows_affected() == 0 {
            return Err(MetaError::NotFound("space"));
        }
        w.commit().await?;
        Ok(epoch)
    }
}
