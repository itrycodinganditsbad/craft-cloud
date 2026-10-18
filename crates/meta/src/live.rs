//! Live documents: each space's documents and the log of their updates (see `craft-cloud-proto::live`).
//!
//! An update is opaque bytes, stored as it came; its sequence number is the document's next one, taken in the same
//! write transaction that stores it, so a document's updates are numbered 1, 2, 3… without gaps. An append is
//! durable when it returns (the transaction committed), which is what lets the server ack it. Everything is keyed by
//! `(space, doc)`, so a document of another space is simply not found.

use craft_cloud_proto::{DeviceId, DocId, NodeId, SpaceId};
use sqlx::Row;

use crate::codec::{to_i64, to_u64, uuid_bytes, uuid_from};
use crate::{Meta, MetaError};

/// A live document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveDoc {
    pub id: DocId,
    /// Its kind as text (`recents:<app>`).
    pub kind: String,
    /// The file node it belongs to, if any (recents have none).
    pub node: Option<NodeId>,
    pub created_ms: i64,
    /// Its last sequence number (0: no update yet).
    pub head: u64,
    /// The bytes of updates its log holds (at or over [`Meta::live_doc_cap`]: full).
    pub bytes: u64,
}

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
        let mut w = self.begin_write().await?;
        let found = sqlx::query("SELECT kind, node, created_ms, head_seq, bytes FROM live_docs WHERE space_id = $1 AND id = $2")
            .bind(uuid_bytes(space.as_uuid()))
            .bind(uuid_bytes(id.as_uuid()))
            .fetch_optional(&mut *w.tx)
            .await?;
        if let Some(row) = found {
            let doc = doc_from(&row, id)?;
            return if doc.kind == kind { Ok(doc) } else { Err(MetaError::Invalid("the document exists with another kind")) };
        }
        sqlx::query("INSERT INTO live_docs (space_id, id, kind, node, created_ms, head_seq) VALUES ($1, $2, $3, $4, $5, 0)")
            .bind(uuid_bytes(space.as_uuid()))
            .bind(uuid_bytes(id.as_uuid()))
            .bind(kind)
            .bind(node.map(|n| uuid_bytes(n.as_uuid())))
            .bind(now_ms)
            .execute(&mut *w.tx)
            .await?;
        w.commit().await?;
        Ok(LiveDoc { id, kind: kind.to_owned(), node, created_ms: now_ms, head: 0, bytes: 0 })
    }

    /// The most bytes of updates a live document's log may hold ([`Meta::with_live_doc_cap`]).
    pub fn live_doc_cap(&self) -> u64 {
        self.live_doc_cap
    }

    /// Empties the document's log (the admin's `live reset-doc`, for a document that is full), and raises the space's
    /// live generation in the same transaction, so its live epoch ([`Meta::live_epoch`]) changes: a reset no agent can
    /// miss, however far the log grows back before it looks (its `welcome`, and every connection's ping, compare the
    /// live epoch). Every agent then catches up from the start and sends up what the log lacks of its document; nothing
    /// any machine holds is lost. The space's own epoch, which the file sync reads, doesn't change. `NotFound` if the
    /// space has no such document.
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
        let done = sqlx::query("UPDATE live_docs SET head_seq = 0, bytes = 0 WHERE space_id = $1 AND id = $2")
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
        let row = sqlx::query("SELECT kind, node, created_ms, head_seq, bytes FROM live_docs WHERE space_id = $1 AND id = $2")
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
        let client_seq = to_i64(client_seq)?;
        let (space_key, doc_key, device_key) = (uuid_bytes(space.as_uuid()), uuid_bytes(doc.as_uuid()), uuid_bytes(device.as_uuid()));
        let mut w = self.begin_write().await?;
        // Taking the next number locks the document's row (on Postgres), so appends to one document run one at a
        // time and the check for a repeat below can't race another append of the same update.
        let size = i64::try_from(bytes.len()).map_err(|_| MetaError::Invalid("an update too large"))?;
        let head =
            sqlx::query("UPDATE live_docs SET head_seq = head_seq + 1, bytes = bytes + $3 WHERE space_id = $1 AND id = $2 RETURNING head_seq, bytes")
                .bind(space_key.clone())
                .bind(doc_key.clone())
                .bind(size)
                .fetch_optional(&mut *w.tx)
                .await?
                .ok_or(MetaError::NotFound("document"))?;
        let seq: i64 = head.try_get("head_seq")?;
        let total = to_u64(head.try_get("bytes")?)?;
        let earlier = sqlx::query("SELECT seq, bytes FROM live_updates WHERE space_id = $1 AND doc = $2 AND device = $3 AND client_seq = $4")
            .bind(space_key.clone())
            .bind(doc_key.clone())
            .bind(device_key.clone())
            .bind(client_seq)
            .fetch_all(&mut *w.tx)
            .await?;
        for row in earlier {
            if row.try_get::<Vec<u8>, _>("bytes")? == bytes {
                let first = to_u64(row.try_get("seq")?)?;
                // The number taken above goes back.
                w.tx.rollback().await?;
                return Ok(first);
            }
        }
        // Full is one way: once the log holds the cap, every append is refused (the one that crosses it is stored), so
        // no later, smaller update can be stored past a refused one it depends on. A repeat above is still acked, so
        // a full document never stalls a device whose ack was lost. Only `reset_live_doc` empties it again.
        let used = total.saturating_sub(to_u64(size)?);
        if used >= self.live_doc_cap {
            w.tx.rollback().await?;
            return Err(MetaError::DocFull { used, limit: self.live_doc_cap });
        }
        sqlx::query("INSERT INTO live_updates (space_id, doc, seq, bytes, device, client_seq, created_ms) VALUES ($1, $2, $3, $4, $5, $6, $7)")
            .bind(space_key)
            .bind(doc_key)
            .bind(seq)
            .bind(bytes.to_vec())
            .bind(device_key)
            .bind(client_seq)
            .bind(now_ms)
            .execute(&mut *w.tx)
            .await?;
        w.commit().await?;
        to_u64(seq)
    }

    /// Every live document of `space` with its last sequence number: what a backup of the database holds. A test seam
    /// (the `test-support` feature), for the recents simulation's restores.
    #[cfg(feature = "test-support")]
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
            "UPDATE live_docs SET head_seq = $3, bytes = (SELECT COALESCE(SUM(LENGTH(u.bytes)), 0) FROM live_updates u \
             WHERE u.space_id = $1 AND u.doc = $2) WHERE space_id = $1 AND id = $2"
        } else {
            "DELETE FROM live_docs WHERE space_id = $1 AND id = $2 AND $3 = 0"
        };
        sqlx::query(sql).bind(space_key).bind(doc_key).bind(keep).execute(&mut *w.tx).await?;
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
