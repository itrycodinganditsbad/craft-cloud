//! Reading the tree: the full listing and the change feed.
//!
//! Every node records the sequence number of the commit that last changed it (`changed_seq`), and deleted
//! nodes stay as tombstones, so "what changed since N" is one indexed query and reports deletions too.

use craft_cloud_proto::{ChangeSet, DeviceId, Epoch, FileState, ManifestId, Name, NodeId, NodeKind, NodeState, SpaceId, VersionId};
use sqlx::Row;
use sqlx::any::AnyRow;

use crate::codec::{flag, hash_from, to_u64, uuid_bytes, uuid_from};
use crate::{Meta, MetaError};

/// Node columns plus its current version (if a file). The two queries differ only in their filter.
const TREE_SQL: &str = "SELECT n.id, n.parent, n.name, n.is_dir, n.deleted, v.id AS v_id, v.manifest AS v_manifest, v.size AS v_size, \
     v.mtime_ms AS v_mtime, v.exec AS v_exec, v.device AS v_device, v.epoch AS v_epoch \
     FROM nodes n LEFT JOIN versions v ON v.space_id = n.space_id AND v.id = n.version \
     WHERE n.space_id = $1 AND n.deleted = 0 ORDER BY n.changed_seq, n.id";
const CHANGES_SQL: &str = "SELECT n.id, n.parent, n.name, n.is_dir, n.deleted, v.id AS v_id, v.manifest AS v_manifest, v.size AS v_size, \
     v.mtime_ms AS v_mtime, v.exec AS v_exec, v.device AS v_device, v.epoch AS v_epoch \
     FROM nodes n LEFT JOIN versions v ON v.space_id = n.space_id AND v.id = n.version \
     WHERE n.space_id = $1 AND n.changed_seq > $2 ORDER BY n.changed_seq, n.id";

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

    /// Every live node, root included, parents before children is not guaranteed (sort by depth on the
    /// client). Read in one transaction, so the listing and its `seq` agree.
    pub async fn tree(&self, space: SpaceId) -> Result<ChangeSet, MetaError> {
        let mut tx = self.read_snapshot().await?;
        let info = space_info(&mut tx, space, false).await?;
        let rows = sqlx::query(TREE_SQL).bind(uuid_bytes(space.as_uuid())).fetch_all(&mut *tx).await?;
        let nodes = rows.iter().map(node_state).collect::<Result<_, _>>()?;
        tx.commit().await?;
        Ok(ChangeSet { epoch: info.epoch, seq: info.seq, nodes })
    }

    /// Nodes changed after `since`, deletions included. `ResyncRequired` when `since` is older than the
    /// oldest change still known, or newer than the space's current sequence (the server was restored).
    pub async fn changes(&self, space: SpaceId, since: u64) -> Result<ChangeSet, MetaError> {
        let mut tx = self.read_snapshot().await?;
        let info = space_info(&mut tx, space, false).await?;
        if since < info.oldest_seq || since > info.seq {
            return Err(MetaError::ResyncRequired);
        }
        let since = i64::try_from(since).map_err(|_| MetaError::ResyncRequired)?;
        let rows = sqlx::query(CHANGES_SQL).bind(uuid_bytes(space.as_uuid())).bind(since).fetch_all(&mut *tx).await?;
        let nodes = rows.iter().map(node_state).collect::<Result<_, _>>()?;
        tx.commit().await?;
        Ok(ChangeSet { epoch: info.epoch, seq: info.seq, nodes })
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
