//! Live documents: each space's documents and the log of their updates (see `craft-cloud-proto::live`).
//!
//! An update is opaque bytes, stored as it came; its sequence number is the document's next one, taken in the same
//! write transaction that stores it, so a document's updates are numbered 1, 2, 3… without gaps. An append is
//! durable when it returns (the transaction committed), which is what lets the server ack it. Everything is keyed by
//! `(space, doc)`, so a document of another space is simply not found.
//!
//! **Snapshots.** A document of a file is made by import, with a snapshot ([`Meta::create_live_document`]); any
//! document's log is compacted into a new snapshot ([`Meta::compact_live_doc`]). A snapshot is stored as blobs in the
//! chunk store (pieces of at most [`SNAPSHOT_PIECE_BYTES`], each a chunk named by its BLAKE3, charged to the space
//! like any chunk); the database holds which, and the last update it holds (`base`). The rows at or below `base` are
//! kept (compacted: no longer counted against the document's cap, and not sent to an agent that reads the snapshot)
//! until the retention rules come.

use craft_cloud_proto::{DeviceId, DocId, ManifestId, NodeId, SpaceId};
use sqlx::Row;

use crate::blobs::manifest_file_size;
use crate::codec::{flag, to_i64, to_u64, uuid_bytes, uuid_from};
use crate::{Meta, MetaError};

/// A live document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveDoc {
    pub id: DocId,
    /// Its kind as text (`recents:<app>`, `document:<app>`).
    pub kind: String,
    /// The file node it belongs to, if any (recents have none).
    pub node: Option<NodeId>,
    pub created_ms: i64,
    /// Its last sequence number (0: no update yet).
    pub head: u64,
    /// The bytes of updates its log holds, every row (compacted ones too).
    pub bytes: u64,
    /// The bytes of the rows at or below `base`: the rest ([`LiveDoc::above_base`]) counts against the cap (at or over
    /// [`Meta::live_doc_cap`]: full).
    pub compacted_bytes: u64,
    /// The last update its snapshot holds (0: none, or a snapshot of an import only).
    pub base: u64,
    /// Whether it has a snapshot ([`Meta::live_snapshot`]), and its size.
    pub has_snapshot: bool,
    pub snapshot_bytes: u64,
}

impl LiveDoc {
    /// The bytes of the log above its snapshot: what counts against its cap.
    pub fn above_base(&self) -> u64 {
        self.bytes.saturating_sub(self.compacted_bytes)
    }
}

/// A snapshot: the manifest that names it (a file's content, uploaded through the chunk API and charged to the space
/// like any), and its size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotRef {
    pub manifest: ManifestId,
    pub bytes: u64,
}

impl SnapshotRef {
    fn decode(id: &[u8], bytes: u64) -> Result<Self, MetaError> {
        let id = <[u8; 32]>::try_from(id).map_err(|_| MetaError::Corrupt("a snapshot's manifest ID"))?;
        Ok(Self { manifest: ManifestId::from_bytes(id), bytes })
    }
}

/// The snapshot `manifest` names, if `space` has that manifest and it is a snapshot's size; read in the caller's
/// transaction, so it is the space's when recorded. Nothing a client sends is recorded unless it was uploaded (and
/// charged) as the space's.
async fn snapshot_of(tx: &mut sqlx::AnyConnection, space: SpaceId, manifest: &ManifestId) -> Result<SnapshotRef, MetaError> {
    let bytes = manifest_file_size(tx, space, manifest).await?.ok_or(MetaError::NotFound("snapshot manifest"))?;
    if bytes == 0 || bytes > craft_cloud_proto::live::MAX_SNAPSHOT_BYTES as u64 {
        return Err(MetaError::Invalid("a snapshot is 1 byte up to the snapshot limit"));
    }
    Ok(SnapshotRef { manifest: *manifest, bytes })
}

/// The most documents of files (rows) one space may have. A document of a file needs a file node of the space, so
/// they are bounded by the tree already; this is a loose cap on top, counted apart from recents.
pub const MAX_LIVE_DOCUMENTS_PER_SPACE: u64 = 65_536;

/// The most documents of files holding updates one space may have. The first update to one more is refused with
/// [`MetaError::TooManyDocs`] (`quota_exceeded`, as for a full document); one that holds updates already never is. On
/// Postgres the count is read without a lock, so concurrent first updates can overshoot it by a few; that soft cap is
/// accepted.
pub const MAX_LIVE_DOCUMENTS_HOLDING_UPDATES: u64 = 10_000;

/// The default for the most bytes of updates all of one space's live documents may hold together, compacted rows too
/// (they are kept until retention): 1 GiB. Over it, every append is refused with [`MetaError::QuotaExceeded`]
/// (`quota_exceeded`). The server's `live_bytes_per_user` sets it.
pub const DEFAULT_LIVE_BYTES_PER_SPACE: u64 = 1024 * 1024 * 1024;

/// What an append did ([`Meta::append_live_update_counted`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Appended {
    /// The update's sequence number (its first one, for a repeat).
    pub seq: u64,
    /// The document's snapshot holds the updates up to here.
    pub base: u64,
    /// The bytes of the log above `base`, with this update.
    pub above_bytes: u64,
    /// The update was stored before (a repeat after a lost ack).
    pub repeat: bool,
}

const DOC_COLUMNS: &str = "kind, node, created_ms, head_seq, bytes, compacted_bytes, base_seq, snapshot_bytes, CAST(CASE WHEN snapshot IS NULL THEN 0 ELSE 1 END AS BIGINT) AS has_snapshot";

/// One stored update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveUpdate {
    pub seq: u64,
    pub bytes: Vec<u8>,
    /// The device that sent it.
    pub device: DeviceId,
}

/// The most bytes of updates one live document's log holds: 16 MiB. A recents document's state is about 30 KB (100
/// entries) and an add (an entry, with its prunes) 400 to 600 bytes, so this is some 30 000 adds, the same file
/// opened 30 times a day for three years; a whole-document push after a restore is one state, about 30 KB. Once the
/// log holds it, every update is refused as `quota_exceeded` (one way: the update that crosses it is stored, nothing
/// after it) and stays on the machine, until the admin empties the log (`live reset-doc`). Log compaction (later) is the
/// real fix.
pub const MAX_LIVE_DOC_BYTES: u64 = 16 * 1024 * 1024;

/// The most live documents holding updates one space may have. The app name is client-chosen, so without a cap an
/// account could fill documents without end, each up to [`MAX_LIVE_DOC_BYTES`]. The first update to one more is refused
/// with [`MetaError::TooManyDocs`]; a document that holds updates already is never refused by it. On Postgres the count
/// is read without a lock, so concurrent first updates to different documents can overshoot it by a few; that soft
/// cap is accepted.
pub const MAX_LIVE_DOCS_PER_SPACE: u64 = 64;

/// The most live documents (rows, holding updates or not) one space may have. Following a list makes a document
/// whether or not anything is ever written to it, and nothing deletes one, so creates are capped loosely. A create over
/// it is refused with [`MetaError::TooManyDocRows`]; re-creating an existing document is never refused. On Postgres
/// the count is read without a lock, so concurrent creates can overshoot it by a few; that soft cap is accepted.
pub const MAX_LIVE_DOC_ROWS_PER_SPACE: u64 = 1024;

/// Most updates [`Meta::live_updates`] returns at once.
pub const MAX_LIVE_PAGE: u32 = 1000;

fn doc_from(row: &sqlx::any::AnyRow, id: DocId) -> Result<LiveDoc, MetaError> {
    let node: Option<Vec<u8>> = row.try_get("node")?;
    Ok(LiveDoc {
        id,
        kind: row.try_get("kind")?,
        node: node.map(|n| uuid_from(&n).map(NodeId::from_uuid)).transpose()?,
        created_ms: row.try_get("created_ms")?,
        head: to_u64(row.try_get("head_seq")?)?,
        bytes: to_u64(row.try_get("bytes")?)?,
        compacted_bytes: to_u64(row.try_get("compacted_bytes")?)?,
        base: to_u64(row.try_get("base_seq")?)?,
        has_snapshot: flag(row, "has_snapshot")?,
        snapshot_bytes: to_u64(row.try_get("snapshot_bytes")?)?,
    })
}

impl Meta {
    /// Makes the document `id` of `kind` in `space`, unless it is there already. A document that exists with the same
    /// kind is returned as it is (two machines may create the same recents document); one of another kind is
    /// `Invalid`.
    pub async fn create_live_doc(&self, space: SpaceId, id: DocId, kind: &str, node: Option<NodeId>, now_ms: i64) -> Result<LiveDoc, MetaError> {
        if kind.is_empty() || kind.len() > 64 {
            return Err(MetaError::Invalid("a document kind is 1-64 bytes"));
        }
        let (space_key, id_key) = (uuid_bytes(space.as_uuid()), uuid_bytes(id.as_uuid()));
        // Insert first: on Postgres, begin_write takes no lock, so a select-then-insert would race another device's
        // create of the same document into a primary-key violation. A conflict does nothing, and the row is read back.
        let mut w = self.begin_write().await?;
        let inserted = sqlx::query(
            "INSERT INTO live_docs (space_id, id, kind, node, created_ms, head_seq) VALUES ($1, $2, $3, $4, $5, 0) \
             ON CONFLICT (space_id, id) DO NOTHING",
        )
        .bind(space_key.clone())
        .bind(id_key.clone())
        .bind(kind)
        .bind(node.map(|n| uuid_bytes(n.as_uuid())))
        .bind(now_ms)
        .execute(&mut *w.tx)
        .await?
        .rows_affected();
        if inserted == 0 {
            let row = sqlx::query(sqlx::AssertSqlSafe(format!("SELECT {DOC_COLUMNS} FROM live_docs WHERE space_id = $1 AND id = $2")))
                .bind(space_key)
                .bind(id_key)
                .fetch_one(&mut *w.tx)
                .await?;
            let doc = doc_from(&row, id)?;
            return if doc.kind == kind { Ok(doc) } else { Err(MetaError::Invalid("the document exists with another kind")) };
        }
        // A new document: the space may not go over its cap on rows (of recents: documents of files have their own).
        // Counted with this one; over the cap, the transaction (and the insert) is dropped uncommitted.
        let count = sqlx::query("SELECT COUNT(*) AS n FROM live_docs WHERE space_id = $1 AND kind NOT LIKE 'document:%'")
            .bind(space_key)
            .fetch_one(&mut *w.tx)
            .await?
            .try_get::<i64, _>("n")?;
        if to_u64(count)? > self.live_doc_rows_cap {
            return Err(MetaError::TooManyDocRows { limit: self.live_doc_rows_cap });
        }
        w.commit().await?;
        Ok(LiveDoc {
            id,
            kind: kind.to_owned(),
            node,
            created_ms: now_ms,
            head: 0,
            bytes: 0,
            compacted_bytes: 0,
            base: 0,
            has_snapshot: false,
            snapshot_bytes: 0,
        })
    }

    /// Whether `node` is a file of `space` (not a folder, not deleted): what a document of a file needs.
    pub async fn is_live_file(&self, space: SpaceId, node: NodeId) -> Result<bool, MetaError> {
        let row = sqlx::query("SELECT is_dir, deleted FROM nodes WHERE space_id = $1 AND id = $2")
            .bind(uuid_bytes(space.as_uuid()))
            .bind(uuid_bytes(node.as_uuid()))
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(row) => Ok(!flag(&row, "is_dir")? && !flag(&row, "deleted")?),
            None => Ok(false),
        }
    }

    /// Makes the document `id` of the file node `node` of `space` by import: `kind` (`document:<app>`), its first
    /// snapshot (the one `manifest` names, uploaded as the space's already), unless the space has the document already.
    /// Returns the document and whether this snapshot was kept: the first one is, every later one (another machine
    /// imported the same file at the same moment) is not, and the document stays as it is. `NotFound` if the space has
    /// no such file (a missing, deleted or folder node) or no such manifest, `Invalid` for a manifest of a size no
    /// snapshot has, `TooManyDocRows` over the space's cap on documents of files.
    pub async fn create_live_document(
        &self,
        space: SpaceId,
        id: DocId,
        kind: &str,
        node: NodeId,
        manifest: &ManifestId,
        now_ms: i64,
    ) -> Result<(LiveDoc, bool), MetaError> {
        if !kind.starts_with(craft_cloud_proto::live::DOCUMENT_KIND_PREFIX) || kind.len() > 64 {
            return Err(MetaError::Invalid("a document of a file is of kind document:<app>"));
        }
        let (space_key, id_key, node_key) = (uuid_bytes(space.as_uuid()), uuid_bytes(id.as_uuid()), uuid_bytes(node.as_uuid()));
        let mut w = self.begin_write().await?;
        let file = sqlx::query("SELECT is_dir, deleted FROM nodes WHERE space_id = $1 AND id = $2")
            .bind(space_key.clone())
            .bind(node_key.clone())
            .fetch_optional(&mut *w.tx)
            .await?;
        match file {
            Some(row) if !flag(&row, "is_dir")? && !flag(&row, "deleted")? => {}
            _ => return Err(MetaError::NotFound("file")),
        }
        let snapshot = snapshot_of(&mut w.tx, space, manifest).await?;
        let inserted = sqlx::query(
            "INSERT INTO live_docs (space_id, id, kind, node, created_ms, head_seq, base_seq, snapshot, snapshot_bytes, import_snapshot, import_bytes) \
             VALUES ($1, $2, $3, $4, $5, 0, 0, $6, $7, $6, $7) ON CONFLICT (space_id, id) DO NOTHING",
        )
        .bind(space_key.clone())
        .bind(id_key.clone())
        .bind(kind)
        .bind(node_key)
        .bind(now_ms)
        .bind(snapshot.manifest.as_bytes().to_vec())
        .bind(to_i64(snapshot.bytes)?)
        .execute(&mut *w.tx)
        .await?
        .rows_affected();
        if inserted == 0 {
            let row = sqlx::query(sqlx::AssertSqlSafe(format!("SELECT {DOC_COLUMNS} FROM live_docs WHERE space_id = $1 AND id = $2")))
                .bind(space_key)
                .bind(id_key)
                .fetch_one(&mut *w.tx)
                .await?;
            let doc = doc_from(&row, id)?;
            // Another machine's import (of any app) was first: this one is not kept, and nothing changes.
            return if doc.kind.starts_with(craft_cloud_proto::live::DOCUMENT_KIND_PREFIX) && doc.node == Some(node) {
                Ok((doc, false))
            } else {
                Err(MetaError::Invalid("the document exists with another kind"))
            };
        }
        let count = sqlx::query("SELECT COUNT(*) AS n FROM live_docs WHERE space_id = $1 AND kind LIKE 'document:%'")
            .bind(space_key)
            .fetch_one(&mut *w.tx)
            .await?
            .try_get::<i64, _>("n")?;
        if to_u64(count)? > self.live_documents_cap {
            return Err(MetaError::TooManyDocRows { limit: self.live_documents_cap });
        }
        w.commit().await?;
        let doc = LiveDoc {
            id,
            kind: kind.to_owned(),
            node: Some(node),
            created_ms: now_ms,
            head: 0,
            bytes: 0,
            compacted_bytes: 0,
            base: 0,
            has_snapshot: true,
            snapshot_bytes: snapshot.bytes,
        };
        Ok((doc, true))
    }

    /// The document's snapshot, if it has one: the last update it holds, and where it is.
    pub async fn live_snapshot(&self, space: SpaceId, doc: DocId) -> Result<Option<(u64, SnapshotRef)>, MetaError> {
        let row = sqlx::query("SELECT base_seq, snapshot, snapshot_bytes FROM live_docs WHERE space_id = $1 AND id = $2")
            .bind(uuid_bytes(space.as_uuid()))
            .bind(uuid_bytes(doc.as_uuid()))
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else { return Ok(None) };
        let Some(ids) = row.try_get::<Option<Vec<u8>>, _>("snapshot")? else { return Ok(None) };
        let snapshot = SnapshotRef::decode(&ids, to_u64(row.try_get("snapshot_bytes")?)?)?;
        Ok(Some((to_u64(row.try_get("base_seq")?)?, snapshot)))
    }

    /// The snapshot a document of a file was made by import with (kept when its log is compacted: the log doesn't hold
    /// it), if it has one.
    pub async fn live_import_snapshot(&self, space: SpaceId, doc: DocId) -> Result<Option<SnapshotRef>, MetaError> {
        let row = sqlx::query("SELECT import_snapshot, import_bytes FROM live_docs WHERE space_id = $1 AND id = $2")
            .bind(uuid_bytes(space.as_uuid()))
            .bind(uuid_bytes(doc.as_uuid()))
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else { return Ok(None) };
        let Some(ids) = row.try_get::<Option<Vec<u8>>, _>("import_snapshot")? else { return Ok(None) };
        Ok(Some(SnapshotRef::decode(&ids, to_u64(row.try_get("import_bytes")?)?)?))
    }

    /// Compacts the document's log: the snapshot `manifest` names (uploaded as the space's already, durably) holds the
    /// updates 1..=`upto` (and the snapshot before it). Recorded with `base = upto` in one transaction, and the rows up
    /// to `upto` stop counting against the document's cap; they are kept. Only a snapshot past the current base, and
    /// not past the head, is taken: one for an older or equal `upto` (a second device answering late, or two at once)
    /// is ignored, `Ok(false)`. `NotFound` if the space has no such manifest, `Invalid` for one of a size no snapshot
    /// has (nothing changes).
    pub async fn compact_live_doc(&self, space: SpaceId, doc: DocId, upto: u64, manifest: &ManifestId) -> Result<bool, MetaError> {
        let (space_key, doc_key) = (uuid_bytes(space.as_uuid()), uuid_bytes(doc.as_uuid()));
        let mut w = self.begin_write().await?;
        let snapshot = snapshot_of(&mut w.tx, space, manifest).await?;
        let row = sqlx::query("SELECT base_seq, head_seq FROM live_docs WHERE space_id = $1 AND id = $2")
            .bind(space_key.clone())
            .bind(doc_key.clone())
            .fetch_optional(&mut *w.tx)
            .await?
            .ok_or(MetaError::NotFound("document"))?;
        let (base, head) = (to_u64(row.try_get("base_seq")?)?, to_u64(row.try_get("head_seq")?)?);
        if upto <= base || upto > head {
            return Ok(false);
        }
        // Postgres sums BIGINT as NUMERIC, which the `Any` driver can't read: cast back.
        let compacted: i64 = sqlx::query(
            "SELECT CAST(COALESCE(SUM(LENGTH(bytes)), 0) AS BIGINT) AS n FROM live_updates WHERE space_id = $1 AND doc = $2 AND seq > $3 AND seq <= $4",
        )
        .bind(space_key.clone())
        .bind(doc_key.clone())
        .bind(to_i64(base)?)
        .bind(to_i64(upto)?)
        .fetch_one(&mut *w.tx)
        .await?
        .try_get("n")?;
        // Only from the base read above: a compaction that raced this one (Postgres) leaves nothing to change here.
        let done = sqlx::query(
            "UPDATE live_docs SET base_seq = $3, snapshot = $4, snapshot_bytes = $5, compacted_bytes = compacted_bytes + $6 \
             WHERE space_id = $1 AND id = $2 AND base_seq = $7",
        )
        .bind(space_key)
        .bind(doc_key)
        .bind(to_i64(upto)?)
        .bind(snapshot.manifest.as_bytes().to_vec())
        .bind(to_i64(snapshot.bytes)?)
        .bind(compacted)
        .bind(to_i64(base)?)
        .execute(&mut *w.tx)
        .await?
        .rows_affected();
        if done == 0 {
            return Ok(false);
        }
        w.commit().await?;
        Ok(true)
    }

    /// The most bytes of updates a live document's log may hold ([`Meta::with_live_doc_cap`]).
    pub fn live_doc_cap(&self) -> u64 {
        self.live_doc_cap
    }

    /// Empties the document's log (the admin's `live reset-doc`, for a document that is full), and raises the space's
    /// live generation in the same transaction, so its live epoch ([`Meta::live_epoch`]) changes: a reset no agent can
    /// miss, however far the log grows back before it looks (its `welcome`, and every connection's ping, compare the
    /// live epoch). Every agent then catches up from the start and sends up what the log lacks of its document; nothing
    /// any machine holds is lost. The space's own epoch, which the file sync reads, doesn't change. The snapshot goes
    /// too (`base` back to 0; its blobs stay in the chunk store): machines that hold the document send it up again as
    /// updates. `NotFound` if the space has no such document.
    pub async fn reset_live_doc(&self, space: SpaceId, doc: DocId) -> Result<(), MetaError> {
        let (space_key, doc_key) = (uuid_bytes(space.as_uuid()), uuid_bytes(doc.as_uuid()));
        let mut w = self.begin_write().await?;
        sqlx::query(
            "INSERT INTO live_spaces (space_id, generation) VALUES ($1, 1) \
             ON CONFLICT (space_id) DO UPDATE SET generation = live_spaces.generation + 1",
        )
        .bind(space_key.clone())
        .execute(&mut *w.tx)
        .await?;
        // Its rows no longer count towards the space's live bytes.
        sqlx::query("UPDATE spaces SET live_bytes = live_bytes - (SELECT bytes FROM live_docs WHERE space_id = $1 AND id = $2) WHERE id = $1 AND EXISTS (SELECT 1 FROM live_docs WHERE space_id = $1 AND id = $2)")
            .bind(space_key.clone())
            .bind(doc_key.clone())
            .execute(&mut *w.tx)
            .await?;
        let done = sqlx::query(
            "UPDATE live_docs SET head_seq = 0, bytes = 0, compacted_bytes = 0, base_seq = 0, snapshot = NULL, snapshot_bytes = 0, \
             import_snapshot = NULL, import_bytes = 0 WHERE space_id = $1 AND id = $2",
        )
        .bind(space_key.clone())
        .bind(doc_key.clone())
        .execute(&mut *w.tx)
        .await?;
        if done.rows_affected() == 0 {
            return Err(MetaError::NotFound("document"));
        }
        sqlx::query("DELETE FROM live_updates WHERE space_id = $1 AND doc = $2").bind(space_key).bind(doc_key).execute(&mut *w.tx).await?;
        w.commit().await
    }

    /// The epoch the live service names in `welcome`: the space's epoch while no live document was reset, else a hash
    /// of it and the live generation. A restore (a new space epoch) and a `live reset-doc` (a new generation) both
    /// change it; the file sync only ever sees the space's epoch.
    pub async fn live_epoch(&self, space: SpaceId) -> Result<craft_cloud_proto::Epoch, MetaError> {
        let epoch = self.space_info(space).await?.epoch;
        let row = sqlx::query("SELECT generation FROM live_spaces WHERE space_id = $1")
            .bind(uuid_bytes(space.as_uuid()))
            .fetch_optional(&self.pool)
            .await?;
        let generation = row.map(|row| row.try_get::<i64, _>("generation")).transpose()?.unwrap_or(0);
        if generation == 0 {
            return Ok(epoch);
        }
        let mut input = b"craft-cloud live epoch".to_vec();
        input.extend_from_slice(epoch.as_uuid().as_bytes());
        input.extend_from_slice(&generation.to_le_bytes());
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&craft_cloud_proto::live::update_hash(&input)[..16]);
        Ok(craft_cloud_proto::Epoch::from_uuid(uuid::Builder::from_custom_bytes(bytes).into_uuid()))
    }

    /// The document `id` of `space`, if it has one.
    pub async fn live_doc(&self, space: SpaceId, id: DocId) -> Result<Option<LiveDoc>, MetaError> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!("SELECT {DOC_COLUMNS} FROM live_docs WHERE space_id = $1 AND id = $2")))
            .bind(uuid_bytes(space.as_uuid()))
            .bind(uuid_bytes(id.as_uuid()))
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| doc_from(&row, id)).transpose()
    }

    /// Appends an update `device` sent as `client_seq` to the document and returns its sequence number, once it is
    /// durable. The same update sent again (the same device, `client_seq` and bytes: its ack was lost) is not stored
    /// twice: its first sequence number comes back. `NotFound` if the space has no such document.
    pub async fn append_live_update(
        &self,
        space: SpaceId,
        doc: DocId,
        device: DeviceId,
        client_seq: u64,
        bytes: &[u8],
        now_ms: i64,
    ) -> Result<u64, MetaError> {
        Ok(self.append_live_update_counted(space, doc, device, client_seq, bytes, now_ms).await?.seq)
    }

    /// [`Meta::append_live_update`], also saying how long the document's log is above its snapshot (whether to compact
    /// it).
    pub async fn append_live_update_counted(
        &self,
        space: SpaceId,
        doc: DocId,
        device: DeviceId,
        client_seq: u64,
        bytes: &[u8],
        now_ms: i64,
    ) -> Result<Appended, MetaError> {
        let mut appended = self.append_live_updates_counted(space, doc, device, &[(client_seq, bytes)], now_ms).await?;
        appended.pop().ok_or(MetaError::Invalid("no update"))
    }

    /// [`Meta::append_live_update_counted`] for several updates of one device, stored together or not at all, with
    /// consecutive sequence numbers (a change sent in fragments: it is in the log whole or not at all, and counts
    /// against the document's cap only then). Each update sent again (as it was stored) gets its first number back;
    /// the rest are stored. The answers are in the order of `updates`.
    pub async fn append_live_updates_counted(
        &self,
        space: SpaceId,
        doc: DocId,
        device: DeviceId,
        updates: &[(u64, &[u8])],
        now_ms: i64,
    ) -> Result<Vec<Appended>, MetaError> {
        let (space_key, doc_key, device_key) = (uuid_bytes(space.as_uuid()), uuid_bytes(doc.as_uuid()), uuid_bytes(device.as_uuid()));
        let mut w = self.begin_write().await?;
        // Locking the document's row first (on Postgres), so appends to one document run one at a time and the check
        // for a repeat below can't race another append of the same update.
        let head = sqlx::query(
            "UPDATE live_docs SET head_seq = head_seq WHERE space_id = $1 AND id = $2 RETURNING head_seq, bytes, compacted_bytes, base_seq, kind",
        )
        .bind(space_key.clone())
        .bind(doc_key.clone())
        .fetch_optional(&mut *w.tx)
        .await?
        .ok_or(MetaError::NotFound("document"))?;
        let head_seq: i64 = head.try_get("head_seq")?;
        // What counts against the cap: the log above its snapshot.
        let total = to_u64(head.try_get("bytes")?)?.saturating_sub(to_u64(head.try_get("compacted_bytes")?)?);
        let base = to_u64(head.try_get("base_seq")?)?;
        let recents = !head.try_get::<String, _>("kind")?.starts_with(craft_cloud_proto::live::DOCUMENT_KIND_PREFIX);
        // Which were stored before (their acks lost): their first numbers.
        let mut repeats: Vec<Option<u64>> = Vec::with_capacity(updates.len());
        for (client_seq, bytes) in updates {
            let earlier = sqlx::query("SELECT seq, bytes FROM live_updates WHERE space_id = $1 AND doc = $2 AND device = $3 AND client_seq = $4")
                .bind(space_key.clone())
                .bind(doc_key.clone())
                .bind(device_key.clone())
                .bind(to_i64(*client_seq)?)
                .fetch_all(&mut *w.tx)
                .await?;
            let mut first = None;
            for row in earlier {
                if row.try_get::<Vec<u8>, _>("bytes")? == *bytes {
                    first = Some(to_u64(row.try_get("seq")?)?);
                    break;
                }
            }
            repeats.push(first);
        }
        let fresh: Vec<&(u64, &[u8])> = updates.iter().zip(&repeats).filter(|(_, r)| r.is_none()).map(|(u, _)| u).collect();
        if fresh.is_empty() {
            w.tx.rollback().await?;
            return Ok(repeats.into_iter().map(|r| Appended { seq: r.unwrap_or_default(), base, above_bytes: total, repeat: true }).collect());
        }
        let size = fresh.iter().map(|(_, b)| b.len()).sum::<usize>();
        let size_i = i64::try_from(size).map_err(|_| MetaError::Invalid("updates too large"))?;
        // Full is one way: once the log holds the cap, every append is refused (the one that crosses it is stored, a
        // set of fragments whole), so no later, smaller update can be stored past a refused one it depends on. A
        // repeat above is still acked, so a full document never stalls a device whose ack was lost. Only
        // `reset_live_doc` empties it again.
        let used = total;
        if used >= self.live_doc_cap {
            w.tx.rollback().await?;
            return Err(MetaError::DocFull { used, limit: self.live_doc_cap });
        }
        // The first update to a recents document: the space may have only so many that hold updates. Counted after the
        // row lock above, in the same transaction; over the cap, nothing is stored, as for a full document. A repeat
        // was acked above, and a document that holds updates already is never refused here (by its numbers, not its
        // bytes: a compacted log holds updates, though none count against its cap). Documents of files aren't counted:
        // each needs a file of the space.
        if head_seq == 0 {
            // Documents of files are counted apart: each needs a file of the space, and their cap is far higher.
            let (sql, cap) = if recents {
                (
                    "SELECT COUNT(*) AS n FROM live_docs WHERE space_id = $1 AND id <> $2 AND head_seq > 0 AND kind NOT LIKE 'document:%'",
                    self.live_docs_cap,
                )
            } else {
                (
                    "SELECT COUNT(*) AS n FROM live_docs WHERE space_id = $1 AND id <> $2 AND head_seq > 0 AND kind LIKE 'document:%'",
                    self.live_held_documents_cap,
                )
            };
            let holding = sqlx::query(sql).bind(space_key.clone()).bind(doc_key.clone()).fetch_one(&mut *w.tx).await?.try_get::<i64, _>("n")?;
            if to_u64(holding)? >= cap {
                w.tx.rollback().await?;
                return Err(MetaError::TooManyDocs { limit: cap });
            }
        }
        // Every row of every live document of the space, compacted ones too, counts towards its live bytes (kept until
        // retention): over the cap, nothing is stored. A repeat was acked above.
        let live: i64 = sqlx::query("UPDATE spaces SET live_bytes = live_bytes + $1 WHERE id = $2 RETURNING live_bytes")
            .bind(size_i)
            .bind(space_key.clone())
            .fetch_one(&mut *w.tx)
            .await?
            .try_get("live_bytes")?;
        let live = to_u64(live)?;
        if live > self.live_bytes_cap {
            w.tx.rollback().await?;
            return Err(MetaError::LiveFull { used: live.saturating_sub(to_u64(size_i)?), limit: self.live_bytes_cap });
        }
        let n = i64::try_from(fresh.len()).map_err(|_| MetaError::Invalid("too many updates"))?;
        sqlx::query("UPDATE live_docs SET head_seq = head_seq + $3, bytes = bytes + $4 WHERE space_id = $1 AND id = $2")
            .bind(space_key.clone())
            .bind(doc_key.clone())
            .bind(n)
            .bind(size_i)
            .execute(&mut *w.tx)
            .await?;
        let above_bytes = total.saturating_add(to_u64(size_i)?);
        let mut next = head_seq;
        let mut out = Vec::with_capacity(updates.len());
        for ((client_seq, bytes), repeat) in updates.iter().zip(repeats) {
            if let Some(first) = repeat {
                out.push(Appended { seq: first, base, above_bytes, repeat: true });
                continue;
            }
            next += 1;
            sqlx::query("INSERT INTO live_updates (space_id, doc, seq, bytes, device, client_seq, created_ms) VALUES ($1, $2, $3, $4, $5, $6, $7)")
                .bind(space_key.clone())
                .bind(doc_key.clone())
                .bind(next)
                .bind(bytes.to_vec())
                .bind(device_key.clone())
                .bind(to_i64(*client_seq)?)
                .bind(now_ms)
                .execute(&mut *w.tx)
                .await?;
            out.push(Appended { seq: to_u64(next)?, base, above_bytes, repeat: false });
        }
        w.commit().await?;
        Ok(out)
    }

    /// Every live document of `space` with its last sequence number (for the admin's checks, and the recents
    /// simulation's restores).
    pub async fn live_heads(&self, space: SpaceId) -> Result<Vec<(DocId, u64)>, MetaError> {
        let rows = sqlx::query("SELECT id, head_seq FROM live_docs WHERE space_id = $1 ORDER BY id")
            .bind(uuid_bytes(space.as_uuid()))
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(|row| Ok((DocId::from_uuid(uuid_from(&row.try_get::<Vec<u8>, _>("id")?)?), to_u64(row.try_get("head_seq")?)?))).collect()
    }

    /// Cuts a document's log back to `head` (and drops it if `head` is `None`): the database as a backup taken back
    /// then has it. A test seam (the `test-support` feature), for the recents simulation's restores.
    #[cfg(feature = "test-support")]
    pub async fn restore_live_log(&self, space: SpaceId, doc: DocId, head: Option<u64>) -> Result<(), MetaError> {
        let (space_key, doc_key) = (uuid_bytes(space.as_uuid()), uuid_bytes(doc.as_uuid()));
        let mut w = self.begin_write().await?;
        let keep = to_i64(head.unwrap_or(0))?;
        sqlx::query("DELETE FROM live_updates WHERE space_id = $1 AND doc = $2 AND seq > $3")
            .bind(space_key.clone())
            .bind(doc_key.clone())
            .bind(keep)
            .execute(&mut *w.tx)
            .await?;
        let sql = if head.is_some() {
            // (A snapshot past the head the backup has goes back to the head: what a backup taken back then holds is
            // [`Meta::restore_live_snapshot`]'s to set.)
            "UPDATE live_docs SET head_seq = $3, base_seq = CASE WHEN base_seq > $3 THEN $3 ELSE base_seq END, \
             bytes = (SELECT COALESCE(SUM(LENGTH(u.bytes)), 0) FROM live_updates u WHERE u.space_id = $1 AND u.doc = $2), \
             compacted_bytes = (SELECT COALESCE(SUM(LENGTH(u.bytes)), 0) FROM live_updates u \
             WHERE u.space_id = $1 AND u.doc = $2 AND u.seq <= live_docs.base_seq AND u.seq <= $3) WHERE space_id = $1 AND id = $2"
        } else {
            "DELETE FROM live_docs WHERE space_id = $1 AND id = $2 AND $3 = 0"
        };
        sqlx::query(sql).bind(space_key.clone()).bind(doc_key).bind(keep).execute(&mut *w.tx).await?;
        sqlx::query("UPDATE spaces SET live_bytes = (SELECT CAST(COALESCE(SUM(d.bytes), 0) AS BIGINT) FROM live_docs d WHERE d.space_id = spaces.id) WHERE id = $1")
            .bind(space_key)
            .execute(&mut *w.tx)
            .await?;
        w.commit().await
    }

    /// The document's snapshot and base as a backup holds them (`None`: no snapshot, base 0). A test seam (the
    /// `test-support` feature), for the simulations' restores; the manifest must be the space's.
    #[cfg(feature = "test-support")]
    pub async fn restore_live_snapshot(&self, space: SpaceId, doc: DocId, snapshot: Option<(u64, SnapshotRef)>) -> Result<(), MetaError> {
        let (space_key, doc_key) = (uuid_bytes(space.as_uuid()), uuid_bytes(doc.as_uuid()));
        let mut w = self.begin_write().await?;
        let (base, ids, size) = match &snapshot {
            Some((base, s)) => (to_i64(*base)?, Some(s.manifest.as_bytes().to_vec()), to_i64(s.bytes)?),
            None => (0, None, 0),
        };
        sqlx::query(
            "UPDATE live_docs SET base_seq = $3, snapshot = $4, snapshot_bytes = $5, compacted_bytes = (SELECT COALESCE(SUM(LENGTH(u.bytes)), 0) \
             FROM live_updates u WHERE u.space_id = $1 AND u.doc = $2 AND u.seq <= $3) WHERE space_id = $1 AND id = $2",
        )
        .bind(space_key)
        .bind(doc_key)
        .bind(base)
        .bind(ids)
        .bind(size)
        .execute(&mut *w.tx)
        .await?;
        w.commit().await
    }

    /// The document's updates above `after`, in order: as many as fit in `max_bytes` (always at least one, if there is
    /// one), and at most `max_rows` (and [`MAX_LIVE_PAGE`]); and whether more may follow. The sizes are read first, so a
    /// page never holds more than one update past the limit in memory.
    pub async fn live_page(
        &self,
        space: SpaceId,
        doc: DocId,
        after: u64,
        max_bytes: u64,
        max_rows: u32,
    ) -> Result<(Vec<LiveUpdate>, bool), MetaError> {
        let limit = i64::from(max_rows.clamp(1, MAX_LIVE_PAGE));
        let sizes = sqlx::query(
            "SELECT seq, CAST(LENGTH(bytes) AS BIGINT) AS size FROM live_updates WHERE space_id = $1 AND doc = $2 AND seq > $3 ORDER BY seq LIMIT $4",
        )
        .bind(uuid_bytes(space.as_uuid()))
        .bind(uuid_bytes(doc.as_uuid()))
        .bind(to_i64(after)?)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let mut last = None;
        let mut total = 0u64;
        let mut taken = 0usize;
        for row in &sizes {
            let size = to_u64(row.try_get::<i64, _>("size")?)?;
            if last.is_some() && total.saturating_add(size) > max_bytes {
                break;
            }
            total = total.saturating_add(size);
            taken += 1;
            last = Some(to_u64(row.try_get("seq")?)?);
        }
        let more = taken < sizes.len() || i64::try_from(sizes.len()).unwrap_or(i64::MAX) >= limit;
        let Some(last) = last else { return Ok((Vec::new(), false)) };
        let rows =
            sqlx::query("SELECT seq, bytes, device FROM live_updates WHERE space_id = $1 AND doc = $2 AND seq > $3 AND seq <= $4 ORDER BY seq")
                .bind(uuid_bytes(space.as_uuid()))
                .bind(uuid_bytes(doc.as_uuid()))
                .bind(to_i64(after)?)
                .bind(to_i64(last)?)
                .fetch_all(&self.pool)
                .await?;
        let page = rows
            .iter()
            .map(|row| {
                Ok(LiveUpdate {
                    seq: to_u64(row.try_get("seq")?)?,
                    bytes: row.try_get("bytes")?,
                    device: DeviceId::from_uuid(uuid_from(&row.try_get::<Vec<u8>, _>("device")?)?),
                })
            })
            .collect::<Result<Vec<_>, MetaError>>()?;
        Ok((page, more))
    }

    /// The updates `device` sent as `client_seq` to the document that the log holds (their bytes): normally none or
    /// one. (A change in fragments partly stored before: the rest of it is joined with what is here.)
    pub async fn live_updates_sent(&self, space: SpaceId, doc: DocId, device: DeviceId, client_seq: u64) -> Result<Vec<Vec<u8>>, MetaError> {
        let rows = sqlx::query("SELECT bytes FROM live_updates WHERE space_id = $1 AND doc = $2 AND device = $3 AND client_seq = $4 ORDER BY seq")
            .bind(uuid_bytes(space.as_uuid()))
            .bind(uuid_bytes(doc.as_uuid()))
            .bind(uuid_bytes(device.as_uuid()))
            .bind(to_i64(client_seq)?)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(|row| Ok(row.try_get("bytes")?)).collect()
    }

    /// The document's updates with a sequence number above `after`, in order, at most `limit` (and at most
    /// [`MAX_LIVE_PAGE`]) of them.
    pub async fn live_updates(&self, space: SpaceId, doc: DocId, after: u64, limit: u32) -> Result<Vec<LiveUpdate>, MetaError> {
        let limit = i64::from(limit.clamp(1, MAX_LIVE_PAGE));
        let rows = sqlx::query("SELECT seq, bytes, device FROM live_updates WHERE space_id = $1 AND doc = $2 AND seq > $3 ORDER BY seq LIMIT $4")
            .bind(uuid_bytes(space.as_uuid()))
            .bind(uuid_bytes(doc.as_uuid()))
            .bind(to_i64(after)?)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| {
                Ok(LiveUpdate {
                    seq: to_u64(row.try_get("seq")?)?,
                    bytes: row.try_get("bytes")?,
                    device: DeviceId::from_uuid(uuid_from(&row.try_get::<Vec<u8>, _>("device")?)?),
                })
            })
            .collect()
    }
}
