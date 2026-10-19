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

use std::collections::HashMap;

use craft_cloud_proto::limits::{MAX_BATCH_OPS, MAX_TREE_DEPTH};
use craft_cloud_proto::{Conflict, ConflictKind, DeviceId, Epoch, Name, NewVersion, NodeId, Op, SaveRecord, SpaceId, VersionId};
use sqlx::Row;

use crate::blobs::manifest_file_size;
use crate::codec::{flag, to_i64, uuid_bytes, uuid_from};
use crate::tree::space_info;
use crate::{Dialect, Meta, MetaError};

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
    /// Deleted nodes whose restore was refused in this commit, and why. Cleared whenever an operation could
    /// free a name or change the tree under them.
    failed_restores: HashMap<NodeId, ConflictKind>,
}

/// One row of an ancestor walk.
struct Ancestor {
    id: NodeId,
    parent: Option<NodeId>,
    name: String,
    deleted: bool,
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
        if deleted {
            self.failed_restores.clear();
        }
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

    /// Brings a deleted node and its deleted ancestors back (root-most first), using one query for the whole
    /// chain. A chain that cannot come back is remembered for the rest of the commit, so later operations that
    /// need the same node answer at once instead of walking again.
    async fn restore(&mut self, id: NodeId, row: NodeRow) -> Result<Outcome, MetaError> {
        if !row.deleted {
            return Ok(Ok(()));
        }
        if let Some(kind) = self.failed_restores.get(&id) {
            return Ok(Err(kind.clone()));
        }
        let up = self.ancestors(id).await?;
        let prefix: Vec<&Ancestor> = up.iter().take_while(|a| a.deleted).collect();
        if prefix.len() == up.len() && up.last().is_some_and(|a| a.parent.is_some()) {
            // The walk hit its bound before reaching a live node.
            return Ok(Err(self.remember_failure(&prefix, ConflictKind::TooDeep)));
        }
        for (index, a) in prefix.iter().enumerate().rev() {
            let parent = a.parent.ok_or(MetaError::Corrupt("a deleted node has no parent"))?;
            let name = Name::new(&a.name).map_err(|_| MetaError::Corrupt("a stored name is invalid"))?;
            if let Some(existing) = self.live_child(parent, &name).await? {
                // This node and everything below it in the chain stay deleted.
                return Ok(Err(self.remember_failure(&prefix[..=index], ConflictKind::NameTaken { existing })));
            }
            self.set_deleted(a.id, false).await?;
        }
        Ok(Ok(()))
    }

    fn remember_failure(&mut self, nodes: &[&Ancestor], kind: ConflictKind) -> ConflictKind {
        for a in nodes {
            self.failed_restores.insert(a.id, kind.clone());
        }
        kind
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
        // The save record as the agent sent it (the server never reads what it means), within its peer limit.
        let save = match &v.save {
            Some(record) if record.version.len() <= craft_cloud_proto::ops::MAX_SAVE_PEERS => serde_json::to_string(record).ok(),
            _ => None,
        };
        sqlx::query(
            "INSERT INTO versions (space_id, id, node, parent_version, epoch, manifest, size, mtime_ms, exec, device, created_ms, seq, save) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
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
        .bind(save)
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
    async fn check_new(&mut self, node: NodeId, parent: NodeId, name: &Name, is_dir: bool) -> Result<Outcome, MetaError> {
        if self.node(node).await?.is_some() {
            return Ok(Err(ConflictKind::NodeExists));
        }
        if let Err(kind) = self.ensure_live_dir(parent).await? {
            return Ok(Err(kind));
        }
        // Only folders count towards the nesting limit; files may sit in the deepest folder.
        if is_dir && let Err(kind) = self.check_placement(parent, None).await? {
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
                if let Err(kind) = self.check_new(*node, *parent, name, true).await? {
                    return Ok(Err(kind));
                }
                self.insert_node(*node, *parent, name, true, None).await?;
                Ok(Ok(()))
            }
            Op::Create { node, parent, name, version } => {
                if let Err(kind) = self.check_new(*node, *parent, name, false).await? {
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
                // A move can free a name or change an ancestor chain.
                self.failed_restores.clear();
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
                if row.is_dir
                    && let Err(kind) = self.check_placement(*parent, Some(*node)).await?
                {
                    return Ok(Err(kind));
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
    /// `node` and its ancestors, nearest first; stops after `MAX_TREE_DEPTH + 2` rows, so a walk that did not
    /// reach the root (last parent is `Some`) is too deep.
    async fn ancestors(&mut self, node: NodeId) -> Result<Vec<Ancestor>, MetaError> {
        let bound = i64::try_from(MAX_TREE_DEPTH + 1).map_err(|_| MetaError::Corrupt("depth limit out of range"))?;
        let rows = sqlx::query(
            "WITH RECURSIVE up(id, parent, name, deleted, depth) AS (\
                 SELECT id, parent, name, deleted, 0 FROM nodes WHERE space_id = $1 AND id = $2 \
                 UNION ALL \
                 SELECT n.id, n.parent, n.name, n.deleted, up.depth + 1 FROM nodes n JOIN up ON n.space_id = $1 AND n.id = up.parent WHERE up.depth < $3 \
             ) SELECT id, parent, name, deleted FROM up ORDER BY depth",
        )
        .bind(self.space())
        .bind(uuid_bytes(node.as_uuid()))
        .bind(bound)
        .fetch_all(&mut *self.conn)
        .await?;
        rows.iter()
            .map(|row| {
                let id = NodeId::from_uuid(uuid_from(&row.try_get::<Vec<u8>, _>("id")?)?);
                let parent: Option<Vec<u8>> = row.try_get("parent")?;
                Ok(Ancestor {
                    id,
                    parent: parent.as_deref().map(uuid_from).transpose()?.map(NodeId::from_uuid),
                    name: row.try_get("name")?,
                    deleted: flag(row, "deleted")?,
                })
            })
            .collect()
    }

    /// Checks that something may sit directly inside `parent`: a moved directory (`moving`) must not be
    /// `parent` or one of its ancestors, and the folder must not nest deeper than `MAX_TREE_DEPTH`.
    async fn check_placement(&mut self, parent: NodeId, moving: Option<NodeId>) -> Result<Outcome, MetaError> {
        let chain = self.ancestors(parent).await?;
        if let Some(dir) = moving
            && chain.iter().any(|a| a.id == dir)
        {
            return Ok(Err(ConflictKind::Cycle));
        }
        let reached_root = chain.last().is_some_and(|a| a.parent.is_none());
        // `parent` sits at depth `chain.len() - 1`; its child one deeper.
        if !reached_root || chain.len() > MAX_TREE_DEPTH {
            return Ok(Err(ConflictKind::TooDeep));
        }
        Ok(Ok(()))
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
        let mut ctx = Ctx { conn: &mut w.tx, space, seq: to_i64(seq)?, epoch: info.epoch, device, now_ms, failed_restores: HashMap::new() };
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

    /// Adds a save record to `node`'s version `version`, which went up without one (the app's record reached the
    /// agent only after the upload: it was saved while the agent was away). Appended only: a version that has a
    /// record keeps it, and the same call again changes nothing. When it is added, the node is in the change feed
    /// again, so the other machines learn it. `NotFound` when `node` has no such version.
    pub async fn attach_save(&self, space: SpaceId, node: NodeId, version: VersionId, save: &SaveRecord) -> Result<CommitOutcome, MetaError> {
        if save.version.len() > craft_cloud_proto::ops::MAX_SAVE_PEERS {
            return Err(MetaError::Invalid("a save record has at most 256 peers"));
        }
        let json = serde_json::to_string(save).map_err(|_| MetaError::Invalid("a save record that can't be written"))?;
        let mut w = self.begin_write().await?;
        let info = space_info(&mut w.tx, space, self.dialect == Dialect::Postgres).await?;
        let found = sqlx::query("SELECT save FROM versions WHERE space_id = $1 AND id = $2 AND node = $3")
            .bind(uuid_bytes(space.as_uuid()))
            .bind(uuid_bytes(version.as_uuid()))
            .bind(uuid_bytes(node.as_uuid()))
            .fetch_optional(&mut *w.tx)
            .await?
            .ok_or(MetaError::NotFound("version"))?;
        if found.try_get::<Option<String>, _>("save")?.is_some() {
            return Ok(CommitOutcome { seq: info.seq, epoch: info.epoch });
        }
        let seq = info.seq.checked_add(1).ok_or(MetaError::Corrupt("sequence overflow"))?;
        sqlx::query("UPDATE versions SET save = $1 WHERE space_id = $2 AND id = $3")
            .bind(json)
            .bind(uuid_bytes(space.as_uuid()))
            .bind(uuid_bytes(version.as_uuid()))
            .execute(&mut *w.tx)
            .await?;
        sqlx::query("UPDATE nodes SET changed_seq = $1 WHERE space_id = $2 AND id = $3")
            .bind(to_i64(seq)?)
            .bind(uuid_bytes(space.as_uuid()))
            .bind(uuid_bytes(node.as_uuid()))
            .execute(&mut *w.tx)
            .await?;
        sqlx::query("UPDATE spaces SET seq = $1 WHERE id = $2").bind(to_i64(seq)?).bind(uuid_bytes(space.as_uuid())).execute(&mut *w.tx).await?;
        w.commit().await?;
        Ok(CommitOutcome { seq, epoch: info.epoch })
    }
}
