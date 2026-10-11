//! The batch commit: apply a list of operations in one transaction, or refuse the whole batch and say
//! which operations conflicted.
//!
//! Operations are applied in order, each seeing the effect of the ones before it. A conflicting operation
//! is recorded and skipped, and evaluation goes on, so the answer lists every conflict (later operations
//! that depended on a refused one usually show up as conflicts of their own, e.g. `NoSuchParent`). If
//! anything conflicted the transaction rolls back; otherwise every changed node gets the space's next
//! sequence number.
//!
//! Rules (from the design): an edit wins over a delete and brings the node back, restoring its deleted
//! parent directories too; names collide by case fold; a directory can't move into itself; only empty
//! directories are deleted; the root can't be moved or deleted.

use craft_cloud_proto::limits::MAX_BATCH_OPS;
use craft_cloud_proto::{Conflict, ConflictKind, DeviceId, Epoch, Name, NewVersion, NodeId, Op, SpaceId, VersionId};
use sqlx::Row;

use crate::blobs::manifest_file_size;
use crate::codec::{flag, to_i64, uuid_bytes, uuid_from};
use crate::tree::space_info;
use crate::{Dialect, Meta, MetaError};

/// Deepest directory nesting walked when checking for cycles; deeper trees are refused as cycles.
const MAX_DEPTH: usize = 4096;

/// A successful commit: the space's new sequence number and its epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitOutcome {
    pub seq: u64,
    pub epoch: Epoch,
}

struct NodeRow {
    parent: Option<NodeId>,
    name: String,
    is_dir: bool,
    version: Option<VersionId>,
    deleted: bool,
}

/// Everything one commit needs while it runs.
struct Ctx<'a> {
    conn: &'a mut sqlx::AnyConnection,
    space: SpaceId,
    seq: i64,
    epoch: Epoch,
    device: DeviceId,
    now_ms: i64,
}

type Outcome = Result<(), ConflictKind>;

impl Ctx<'_> {
    fn space(&self) -> Vec<u8> {
        uuid_bytes(self.space.as_uuid())
    }

    async fn node(&mut self, id: NodeId) -> Result<Option<NodeRow>, MetaError> {
        let row = sqlx::query("SELECT parent, name, is_dir, version, deleted FROM nodes WHERE space_id = $1 AND id = $2")
            .bind(self.space())
            .bind(uuid_bytes(id.as_uuid()))
            .fetch_optional(&mut *self.conn)
            .await?;
        let Some(row) = row else { return Ok(None) };
        let parent: Option<Vec<u8>> = row.try_get("parent")?;
        let version: Option<Vec<u8>> = row.try_get("version")?;
        Ok(Some(NodeRow {
            parent: parent.as_deref().map(uuid_from).transpose()?.map(NodeId::from_uuid),
            name: row.try_get("name")?,
            is_dir: flag(&row, "is_dir")?,
            version: version.as_deref().map(uuid_from).transpose()?.map(VersionId::from_uuid),
            deleted: flag(&row, "deleted")?,
        }))
    }

    /// The live node in `parent` whose name folds to the same key, if any.
    async fn live_child(&mut self, parent: NodeId, name: &Name) -> Result<Option<NodeId>, MetaError> {
        let row = sqlx::query("SELECT id FROM nodes WHERE space_id = $1 AND parent = $2 AND fold_key = $3 AND deleted = 0")
            .bind(self.space())
            .bind(uuid_bytes(parent.as_uuid()))
            .bind(name.fold_key().as_str().to_owned())
            .fetch_optional(&mut *self.conn)
            .await?;
        row.map(|r| Ok(NodeId::from_uuid(uuid_from(&r.try_get::<Vec<u8>, _>("id")?)?))).transpose()
    }

    async fn has_live_children(&mut self, dir: NodeId) -> Result<bool, MetaError> {
        let row = sqlx::query("SELECT 1 FROM nodes WHERE space_id = $1 AND parent = $2 AND deleted = 0 LIMIT 1")
            .bind(self.space())
            .bind(uuid_bytes(dir.as_uuid()))
            .fetch_optional(&mut *self.conn)
            .await?;
        Ok(row.is_some())
    }

    async fn version_exists(&mut self, id: VersionId) -> Result<bool, MetaError> {
        let row = sqlx::query("SELECT 1 FROM versions WHERE space_id = $1 AND id = $2")
            .bind(self.space())
            .bind(uuid_bytes(id.as_uuid()))
            .fetch_optional(&mut *self.conn)
            .await?;
        Ok(row.is_some())
    }

    async fn set_deleted(&mut self, id: NodeId, deleted: bool) -> Result<(), MetaError> {
        sqlx::query("UPDATE nodes SET deleted = $1, changed_seq = $2 WHERE space_id = $3 AND id = $4")
            .bind(i64::from(deleted))
            .bind(self.seq)
            .bind(self.space())
            .bind(uuid_bytes(id.as_uuid()))
            .execute(&mut *self.conn)
            .await?;
        Ok(())
    }

    /// Makes `dir` usable as a parent: it must exist and be a directory; if it (or any ancestor) was
    /// deleted, bring the chain back, as long as no live node has taken one of their names meanwhile.
    async fn ensure_live_dir(&mut self, dir: NodeId) -> Result<Outcome, MetaError> {
        let Some(row) = self.node(dir).await? else { return Ok(Err(ConflictKind::NoSuchParent)) };
        if !row.is_dir {
            return Ok(Err(ConflictKind::NotADirectory));
        }
        self.restore(dir, row).await
    }

    /// Brings a deleted node and its deleted ancestors back (iteratively, root-most first).
    async fn restore(&mut self, id: NodeId, row: NodeRow) -> Result<Outcome, MetaError> {
        let mut chain = Vec::new();
        let (mut current, mut current_row) = (id, row);
        while current_row.deleted {
            if chain.len() >= MAX_DEPTH {
                return Ok(Err(ConflictKind::Cycle));
            }
            let Some(parent) = current_row.parent else { break };
            chain.push((current, parent, current_row.name.clone()));
            let Some(parent_row) = self.node(parent).await? else { return Err(MetaError::Corrupt("a node's parent is missing")) };
            (current, current_row) = (parent, parent_row);
        }
        for (node, parent, name) in chain.into_iter().rev() {
            let name = Name::new(&name).map_err(|_| MetaError::Corrupt("a stored name is invalid"))?;
            if let Some(existing) = self.live_child(parent, &name).await? {
                return Ok(Err(ConflictKind::NameTaken { existing }));
            }
            self.set_deleted(node, false).await?;
        }
        Ok(Ok(()))
    }

    /// Records a new version, refusing a reused version ID or a manifest this space hasn't uploaded.
    async fn insert_version(&mut self, node: NodeId, parent_version: Option<VersionId>, v: &NewVersion) -> Result<Outcome, MetaError> {
        if self.version_exists(v.id).await? {
            return Ok(Err(ConflictKind::VersionExists));
        }
        let space = self.space;
        let Some(size) = manifest_file_size(self.conn, space, &v.manifest).await? else {
            return Ok(Err(ConflictKind::MissingManifest { manifest: v.manifest }));
        };
        sqlx::query(
            "INSERT INTO versions (space_id, id, node, parent_version, epoch, manifest, size, mtime_ms, exec, device, created_ms, seq) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
        )
        .bind(self.space())
        .bind(uuid_bytes(v.id.as_uuid()))
        .bind(uuid_bytes(node.as_uuid()))
        .bind(parent_version.map(|p| uuid_bytes(p.as_uuid())))
        .bind(uuid_bytes(self.epoch.as_uuid()))
        .bind(v.manifest.as_bytes().to_vec())
        .bind(to_i64(size)?)
        .bind(v.mtime_ms)
        .bind(i64::from(v.exec))
        .bind(uuid_bytes(self.device.as_uuid()))
        .bind(self.now_ms)
        .bind(self.seq)
        .execute(&mut *self.conn)
        .await?;
        Ok(Ok(()))
    }

    async fn insert_node(&mut self, id: NodeId, parent: NodeId, name: &Name, is_dir: bool, version: Option<VersionId>) -> Result<(), MetaError> {
        sqlx::query(
            "INSERT INTO nodes (space_id, id, parent, name, fold_key, is_dir, version, deleted, changed_seq) VALUES ($1, $2, $3, $4, $5, $6, $7, 0, $8)",
        )
        .bind(self.space())
        .bind(uuid_bytes(id.as_uuid()))
        .bind(uuid_bytes(parent.as_uuid()))
        .bind(name.as_str().to_owned())
        .bind(name.fold_key().as_str().to_owned())
        .bind(i64::from(is_dir))
        .bind(version.map(|v| uuid_bytes(v.as_uuid())))
        .bind(self.seq)
        .execute(&mut *self.conn)
        .await?;
        Ok(())
    }

    /// Checks a new node can be created as `name` in `parent`.
    async fn check_new(&mut self, node: NodeId, parent: NodeId, name: &Name) -> Result<Outcome, MetaError> {
        if self.node(node).await?.is_some() {
            return Ok(Err(ConflictKind::NodeExists));
        }
        if let Err(kind) = self.ensure_live_dir(parent).await? {
            return Ok(Err(kind));
        }
        if let Some(existing) = self.live_child(parent, name).await? {
            return Ok(Err(ConflictKind::NameTaken { existing }));
        }
        Ok(Ok(()))
    }

    async fn apply(&mut self, op: &Op) -> Result<Outcome, MetaError> {
        match op {
            Op::Mkdir { node, parent, name } => {
                if let Err(kind) = self.check_new(*node, *parent, name).await? {
                    return Ok(Err(kind));
                }
                self.insert_node(*node, *parent, name, true, None).await?;
                Ok(Ok(()))
            }
            Op::Create { node, parent, name, version } => {
                if let Err(kind) = self.check_new(*node, *parent, name).await? {
                    return Ok(Err(kind));
                }
                if let Err(kind) = self.insert_version(*node, None, version).await? {
                    return Ok(Err(kind));
                }
                self.insert_node(*node, *parent, name, false, Some(version.id)).await?;
                Ok(Ok(()))
            }
            Op::Update { node, expected, version } => {
                let Some(row) = self.node(*node).await? else { return Ok(Err(ConflictKind::NoSuchNode)) };
                if row.is_dir {
                    return Ok(Err(ConflictKind::WrongKind));
                }
                if row.version != Some(*expected) {
                    return Ok(Err(ConflictKind::VersionMismatch { current: row.version }));
                }
                // An edit wins over a delete: bring the file (and its folders) back.
                if let Err(kind) = self.restore(*node, row).await? {
                    return Ok(Err(kind));
                }
                if let Err(kind) = self.insert_version(*node, Some(*expected), version).await? {
                    return Ok(Err(kind));
                }
                sqlx::query("UPDATE nodes SET version = $1, changed_seq = $2 WHERE space_id = $3 AND id = $4")
                    .bind(uuid_bytes(version.id.as_uuid()))
                    .bind(self.seq)
                    .bind(self.space())
                    .bind(uuid_bytes(node.as_uuid()))
                    .execute(&mut *self.conn)
                    .await?;
                Ok(Ok(()))
            }
            Op::Move { node, from_parent, from_name, parent, name } => {
                let Some(row) = self.node(*node).await? else { return Ok(Err(ConflictKind::NoSuchNode)) };
                let Some(current_parent) = row.parent else { return Ok(Err(ConflictKind::Root)) };
                if row.deleted {
                    return Ok(Err(ConflictKind::NoSuchNode));
                }
                if current_parent != *from_parent || row.name != from_name.as_str() {
                    let current_name = Name::new(&row.name).map_err(|_| MetaError::Corrupt("a stored name is invalid"))?;
                    return Ok(Err(ConflictKind::Moved { parent: Some(current_parent), name: Some(current_name) }));
                }
                if let Err(kind) = self.ensure_live_dir(*parent).await? {
                    return Ok(Err(kind));
                }
                if row.is_dir && self.is_inside(*parent, *node).await? {
                    return Ok(Err(ConflictKind::Cycle));
                }
                if let Some(existing) = self.live_child(*parent, name).await?
                    && existing != *node
                {
                    return Ok(Err(ConflictKind::NameTaken { existing }));
                }
                sqlx::query("UPDATE nodes SET parent = $1, name = $2, fold_key = $3, changed_seq = $4 WHERE space_id = $5 AND id = $6")
                    .bind(uuid_bytes(parent.as_uuid()))
                    .bind(name.as_str().to_owned())
                    .bind(name.fold_key().as_str().to_owned())
                    .bind(self.seq)
                    .bind(self.space())
                    .bind(uuid_bytes(node.as_uuid()))
                    .execute(&mut *self.conn)
                    .await?;
                Ok(Ok(()))
            }
            Op::Delete { node, expected } => {
                let Some(row) = self.node(*node).await? else { return Ok(Err(ConflictKind::NoSuchNode)) };
                if row.parent.is_none() {
                    return Ok(Err(ConflictKind::Root));
                }
                if row.is_dir != expected.is_none() {
                    return Ok(Err(ConflictKind::WrongKind));
                }
                if !row.is_dir && row.version != *expected {
                    return Ok(Err(ConflictKind::VersionMismatch { current: row.version }));
                }
                if row.deleted {
                    // Both sides deleted it: nothing to do, not a conflict.
                    return Ok(Ok(()));
                }
                if row.is_dir && self.has_live_children(*node).await? {
                    return Ok(Err(ConflictKind::NotEmpty));
                }
                self.set_deleted(*node, true).await?;
                Ok(Ok(()))
            }
        }
    }

    /// Whether `dir` is `ancestor` or lies somewhere below it.
    async fn is_inside(&mut self, dir: NodeId, ancestor: NodeId) -> Result<bool, MetaError> {
        let mut current = Some(dir);
        for _ in 0..MAX_DEPTH {
            let Some(id) = current else { return Ok(false) };
            if id == ancestor {
                return Ok(true);
            }
            current = self.node(id).await?.and_then(|row| row.parent);
        }
        // Absurdly deep: refuse rather than loop.
        Ok(true)
    }
}

impl Meta {
    /// Applies `ops` atomically for `device`, or refuses them all with every conflict listed.
    pub async fn commit(&self, space: SpaceId, device: DeviceId, ops: &[Op], now_ms: i64) -> Result<CommitOutcome, MetaError> {
        if ops.is_empty() || ops.len() > MAX_BATCH_OPS {
            return Err(MetaError::Invalid("a batch has 1 to 1000 operations"));
        }
        let mut w = self.begin_write().await?;
        // On Postgres this row lock makes commits to one space run one at a time.
        let info = space_info(&mut w.tx, space, self.dialect == Dialect::Postgres).await?;
        let seq = info.seq.checked_add(1).ok_or(MetaError::Corrupt("sequence overflow"))?;
        let mut ctx = Ctx { conn: &mut w.tx, space, seq: to_i64(seq)?, epoch: info.epoch, device, now_ms };
        let mut conflicts = Vec::new();
        for (index, op) in ops.iter().enumerate() {
            if let Err(kind) = ctx.apply(op).await? {
                conflicts.push(Conflict { op: index, kind });
            }
        }
        if !conflicts.is_empty() {
            return Err(MetaError::Rejected(conflicts));
        }
        sqlx::query("UPDATE spaces SET seq = $1 WHERE id = $2").bind(to_i64(seq)?).bind(uuid_bytes(space.as_uuid())).execute(&mut *w.tx).await?;
        w.commit().await?;
        Ok(CommitOutcome { seq, epoch: info.epoch })
    }
}
