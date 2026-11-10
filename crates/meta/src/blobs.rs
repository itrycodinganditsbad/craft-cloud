//! Which chunks and manifests exist, and which spaces may use them.
//!
//! A blob is stored once (`blobs`), but each space only "has" the blobs it uploaded (`space_blobs`). Asking
//! which chunks are missing answers for the asking space only, so nobody can learn whether another user
//! stores the same data: they upload it again, and the server just doesn't keep a second copy.

use std::collections::HashSet;

use craft_cloud_proto::api::MAX_MISSING_IDS;
use craft_cloud_proto::{BlobKind, ChunkId, Manifest, ManifestId, SpaceId};
use sqlx::Row;

use crate::codec::{hash_from, to_i64, to_u64, uuid_bytes};
use crate::{Dialect, Meta, MetaError, UserId};

/// How a blob's kind is stored in the `kind` columns.
pub(crate) trait KindCode {
    fn code(self) -> i16;
}

impl KindCode for BlobKind {
    fn code(self) -> i16 {
        match self {
            BlobKind::Chunk => 0,
            BlobKind::Manifest => 1,
        }
    }
}

async fn space_has(tx: &mut sqlx::AnyConnection, space: SpaceId, kind: BlobKind, id: &[u8; 32]) -> Result<bool, MetaError> {
    let row = sqlx::query("SELECT 1 FROM space_blobs WHERE space_id = $1 AND kind = $2 AND id = $3")
        .bind(uuid_bytes(space.as_uuid()))
        .bind(kind.code())
        .bind(id.to_vec())
        .fetch_optional(&mut *tx)
        .await?;
    Ok(row.is_some())
}

/// How many IDs one ownership query (or multi-row insert) covers.
const BATCH: usize = 500;

/// `$first, $first+1, …` for `count` placeholders.
pub(crate) fn placeholders(first: usize, count: usize) -> String {
    (first..first + count).map(|n| format!("${n}")).collect::<Vec<_>>().join(", ")
}

/// Of `ids`, those the space doesn't have, in first-seen order, each once. Asks the database 500 IDs at a time.
async fn missing_in_space(conn: &mut sqlx::AnyConnection, space: SpaceId, kind: BlobKind, ids: &[[u8; 32]]) -> Result<Vec<[u8; 32]>, MetaError> {
    let mut seen = HashSet::with_capacity(ids.len());
    let unique: Vec<&[u8; 32]> = ids.iter().filter(|id| seen.insert(**id)).collect();
    let mut present: HashSet<[u8; 32]> = HashSet::new();
    for part in unique.chunks(BATCH) {
        // Only placeholder text is generated; every value is bound.
        let sql = format!("SELECT id FROM space_blobs WHERE space_id = $1 AND kind = $2 AND id IN ({})", placeholders(3, part.len()));
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(uuid_bytes(space.as_uuid())).bind(kind.code());
        for id in part {
            query = query.bind(id.to_vec());
        }
        for row in query.fetch_all(&mut *conn).await? {
            present.insert(hash_from(&row.try_get::<Vec<u8>, _>("id")?)?);
        }
    }
    Ok(unique.into_iter().filter(|id| !present.contains(*id)).copied().collect())
}

/// The result of [`Meta::claim_chunks`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    /// What the target space has of the asked IDs afterwards.
    pub claimed: Vec<ChunkId>,
    /// What neither space has.
    pub missing: Vec<ChunkId>,
}

/// One blob to record: what it is, how many bytes it is stored as, and (for a manifest) the size of its file.
pub(crate) struct Blob<'a> {
    pub(crate) kind: BlobKind,
    pub(crate) id: &'a [u8; 32],
    pub(crate) stored_size: i64,
    pub(crate) file_size: Option<i64>,
}

/// Records that `space` has the blob. The first time, its stored size is added to the space's stored bytes, and if
/// that takes them over `cap`, nothing is recorded (the caller's transaction is dropped, which rolls it back).
pub(crate) async fn record(tx: &mut sqlx::AnyConnection, space: SpaceId, blob: Blob<'_>, now_ms: i64, cap: Option<u64>) -> Result<(), MetaError> {
    let Blob { kind, id, stored_size, file_size } = blob;
    sqlx::query("INSERT INTO blobs (kind, id, stored_size, file_size, created_ms) VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING")
        .bind(kind.code())
        .bind(id.to_vec())
        .bind(stored_size)
        .bind(file_size)
        .bind(now_ms)
        .execute(&mut *tx)
        .await?;
    let added = sqlx::query("INSERT INTO space_blobs (space_id, kind, id) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
        .bind(uuid_bytes(space.as_uuid()))
        .bind(kind.code())
        .bind(id.to_vec())
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if added == 0 {
        return Ok(());
    }
    // The size the blob is stored at (another space may have stored it first; the bytes are the same).
    let size: i64 = sqlx::query("SELECT stored_size FROM blobs WHERE kind = $1 AND id = $2")
        .bind(kind.code())
        .bind(id.to_vec())
        .fetch_one(&mut *tx)
        .await?
        .try_get("stored_size")?;
    charge(tx, space, size, cap).await
}

/// Adds `size` to the space's stored bytes, refusing (`QuotaExceeded`) when that goes over the space's limit: its own
/// quota (set by an admin), else `cap`. On Postgres the update locks the space's row until the transaction ends, so two
/// uploads to one space can't both slip under.
async fn charge(tx: &mut sqlx::AnyConnection, space: SpaceId, size: i64, cap: Option<u64>) -> Result<(), MetaError> {
    let row = sqlx::query("UPDATE spaces SET stored_bytes = stored_bytes + $1 WHERE id = $2 RETURNING stored_bytes, quota_bytes")
        .bind(size)
        .bind(uuid_bytes(space.as_uuid()))
        .fetch_one(&mut *tx)
        .await?;
    let total = to_u64(row.try_get("stored_bytes")?)?;
    let quota: Option<i64> = row.try_get("quota_bytes")?;
    match effective_quota(quota.map(to_u64).transpose()?, cap) {
        Some(limit) if total > limit => Err(MetaError::QuotaExceeded { used: total.saturating_sub(to_u64(size)?), limit }),
        _ => Ok(()),
    }
}

/// The account's quota override, else the server-wide cap.
pub(crate) fn effective_quota(quota: Option<u64>, cap: Option<u64>) -> Option<u64> {
    quota.or(cap)
}

/// The bytes `space` stores.
async fn stored_bytes(conn: &mut sqlx::AnyConnection, space: SpaceId) -> Result<u64, MetaError> {
    Ok(stored_and_quota(conn, space).await?.0)
}

/// The bytes `space` stores, and the quota an admin set for it (`None`: the server-wide cap, if any, applies).
async fn stored_and_quota(conn: &mut sqlx::AnyConnection, space: SpaceId) -> Result<(u64, Option<u64>), MetaError> {
    let row = sqlx::query("SELECT stored_bytes, quota_bytes FROM spaces WHERE id = $1")
        .bind(uuid_bytes(space.as_uuid()))
        .fetch_optional(&mut *conn)
        .await?;
    let row = row.ok_or(MetaError::NotFound("space"))?;
    let quota: Option<i64> = row.try_get("quota_bytes")?;
    Ok((to_u64(row.try_get("stored_bytes")?)?, quota.map(to_u64).transpose()?))
}

/// The file size recorded for a manifest this space has, if it has it.
pub(crate) async fn manifest_file_size(tx: &mut sqlx::AnyConnection, space: SpaceId, id: &ManifestId) -> Result<Option<u64>, MetaError> {
    let row = sqlx::query("SELECT b.file_size FROM space_blobs sb JOIN blobs b ON b.kind = sb.kind AND b.id = sb.id WHERE sb.space_id = $1 AND sb.kind = $2 AND sb.id = $3")
        .bind(uuid_bytes(space.as_uuid()))
        .bind(BlobKind::Manifest.code())
        .bind(id.as_bytes().to_vec())
        .fetch_optional(&mut *tx)
        .await?;
    match row {
        None => Ok(None),
        Some(row) => {
            let size: Option<i64> = row.try_get("file_size")?;
            Ok(Some(to_u64(size.ok_or(MetaError::Corrupt("a manifest has no file size"))?)?))
        }
    }
}
impl Meta {
    /// Of `ids`, the chunks this space doesn't have yet, in the order given (duplicates once).
    pub async fn missing_chunks(&self, space: SpaceId, ids: &[ChunkId]) -> Result<Vec<ChunkId>, MetaError> {
        let mut conn = self.pool.acquire().await?;
        let raw: Vec<[u8; 32]> = ids.iter().map(|id| *id.as_bytes()).collect();
        Ok(missing_in_space(&mut conn, space, BlobKind::Chunk, &raw).await?.into_iter().map(ChunkId::from_bytes).collect())
    }

    /// The bytes `space` stores: the stored size of every chunk and manifest it has, each counted once.
    pub async fn stored_bytes(&self, space: SpaceId) -> Result<u64, MetaError> {
        let mut conn = self.pool.acquire().await?;
        stored_bytes(&mut conn, space).await
    }

    /// Whether `space` has room for a blob of `stored_size` bytes under its limit: the quota an admin set for the
    /// space, else the cap ([`Meta::with_stored_bytes_cap`]), else none (so a quota holds without a cap):
    /// `QuotaExceeded` if it doesn't have the blob yet and taking it would go over. A cheap look before the bytes are
    /// stored, so a space over its cap can't fill the disk with blobs that are then refused; recording the blob
    /// checks again, under the write lock. Two uploads under the cap can both pass this look and one then be refused
    /// after its bytes are stored: at most a few unrecorded blobs per account (bounded by its body slots), which GC
    /// (M4) sweeps.
    pub async fn check_room(&self, space: SpaceId, kind: BlobKind, id: &[u8; 32], stored_size: u64) -> Result<(), MetaError> {
        let mut conn = self.pool.acquire().await?;
        let (used, quota) = stored_and_quota(&mut conn, space).await?;
        let Some(limit) = effective_quota(quota, self.stored_cap) else { return Ok(()) };
        if space_has(&mut conn, space, kind, id).await? {
            return Ok(());
        }
        if used.saturating_add(stored_size) > limit { Err(MetaError::QuotaExceeded { used, limit }) } else { Ok(()) }
    }

    /// Records that `space` uploaded the chunk (the caller has stored and verified the bytes). Refused with
    /// `QuotaExceeded`, recording nothing, if it would take the space over the cap.
    pub async fn add_chunk(&self, space: SpaceId, id: &ChunkId, stored_size: u64, now_ms: i64) -> Result<(), MetaError> {
        let mut w = self.begin_write().await?;
        self.check_guard(&mut w.tx, space).await?;
        let blob = Blob { kind: BlobKind::Chunk, id: id.as_bytes(), stored_size: to_i64(stored_size)?, file_size: None };
        record(&mut w.tx, space, blob, now_ms, self.stored_cap).await?;
        w.commit().await
    }

    /// Records that `space` uploaded all of these chunks (`(id, stored size)`), in one transaction, under the cap
    /// like [`Meta::add_chunk`]. Only the rows this call really inserted are charged (the insert returns them), so
    /// two calls racing with an overlapping chunk on Postgres charge it once. Chunks go in ID order, so two such
    /// calls wait on each other's rows in one order and can't deadlock.
    pub async fn add_chunks(&self, space: SpaceId, chunks: &[(ChunkId, u64)], now_ms: i64) -> Result<(), MetaError> {
        let mut sorted: Vec<(ChunkId, u64)> = chunks.to_vec();
        sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        sorted.dedup_by(|a, b| a.0 == b.0);
        let mut w = self.begin_write().await?;
        self.check_guard(&mut w.tx, space).await?;
        for part in sorted.chunks(BATCH) {
            let rows = |width: usize| (0..part.len()).map(|i| format!("({})", placeholders(i * width + 1, width))).collect::<Vec<_>>().join(", ");
            let mut blobs = sqlx::query(sqlx::AssertSqlSafe(format!(
                "INSERT INTO blobs (kind, id, stored_size, file_size, created_ms) VALUES {} ON CONFLICT DO NOTHING",
                rows(5)
            )));
            let mut owned = sqlx::query(sqlx::AssertSqlSafe(format!(
                "INSERT INTO space_blobs (space_id, kind, id) VALUES {} ON CONFLICT DO NOTHING RETURNING id",
                rows(3)
            )));
            for (id, stored_size) in part {
                blobs = blobs.bind(BlobKind::Chunk.code()).bind(id.as_bytes().to_vec()).bind(to_i64(*stored_size)?).bind(None::<i64>).bind(now_ms);
                owned = owned.bind(uuid_bytes(space.as_uuid())).bind(BlobKind::Chunk.code()).bind(id.as_bytes().to_vec());
            }
            blobs.execute(&mut *w.tx).await?;
            let mut new: HashSet<[u8; 32]> = HashSet::new();
            for row in owned.fetch_all(&mut *w.tx).await? {
                new.insert(hash_from(&row.try_get::<Vec<u8>, _>("id")?)?);
            }
            if !new.is_empty() {
                // Postgres sums BIGINT as NUMERIC, which the `Any` driver can't read: cast back.
                let sql = format!(
                    "SELECT CAST(COALESCE(SUM(stored_size), 0) AS BIGINT) AS total FROM blobs WHERE kind = $1 AND id IN ({})",
                    placeholders(2, new.len())
                );
                let mut sum = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(BlobKind::Chunk.code());
                for id in &new {
                    sum = sum.bind(id.to_vec());
                }
                let size: i64 = sum.fetch_one(&mut *w.tx).await?.try_get("total")?;
                charge(&mut w.tx, space, size, self.stored_cap).await?;
            }
        }
        w.commit().await
    }

    /// Adds to `target` the chunks of `ids` that `from` has and `target` has not yet, charging their logical stored size to
    /// `target` like an upload (`QuotaExceeded` refuses the whole claim and records nothing). `user` must have at least read access
    /// to `from`, else the answer is `NotFound("space")`, the same as for a space that does not exist. The caller's role in
    /// `target` is checked by the [`Meta::guarded`] handle, as for an upload. No bytes move: a blob is stored once.
    ///
    /// Returns what `target` has afterwards of `ids` (`claimed`: added now or held before, which the caller could download from
    /// `target` anyway) and what neither space has (`missing`), each once, in the order given. A claim again changes nothing.
    /// Chunks are inserted in ID order, so two overlapping claims wait on each other's rows in one order.
    pub async fn claim_chunks(&self, user: UserId, target: SpaceId, from: SpaceId, ids: &[ChunkId]) -> Result<Claim, MetaError> {
        let mut seen = HashSet::with_capacity(ids.len());
        let unique: Vec<[u8; 32]> = ids.iter().map(|id| *id.as_bytes()).filter(|id| seen.insert(*id)).collect();
        let mut w = self.begin_write().await?;
        self.check_guard(&mut w.tx, target).await?;
        if crate::spaces::access_on(&mut w.tx, user, from).await?.is_none() {
            return Err(MetaError::NotFound("space"));
        }
        let absent = missing_in_space(&mut w.tx, target, BlobKind::Chunk, &unique).await?;
        let missing_there: HashSet<[u8; 32]> = missing_in_space(&mut w.tx, from, BlobKind::Chunk, &absent).await?.into_iter().collect();
        let in_source: HashSet<[u8; 32]> = absent.iter().filter(|id| !missing_there.contains(*id)).copied().collect();

        let mut take: Vec<[u8; 32]> = in_source.iter().copied().collect();
        take.sort();
        for part in take.chunks(BATCH) {
            let rows = (0..part.len()).map(|i| format!("(${}, ${}, ${})", i * 3 + 1, i * 3 + 2, i * 3 + 3)).collect::<Vec<_>>().join(", ");
            let mut owned = sqlx::query(sqlx::AssertSqlSafe(format!(
                "INSERT INTO space_blobs (space_id, kind, id) VALUES {rows} ON CONFLICT DO NOTHING RETURNING id"
            )));
            for id in part {
                owned = owned.bind(uuid_bytes(target.as_uuid())).bind(BlobKind::Chunk.code()).bind(id.to_vec());
            }
            let mut new: Vec<[u8; 32]> = Vec::new();
            for row in owned.fetch_all(&mut *w.tx).await? {
                new.push(hash_from(&row.try_get::<Vec<u8>, _>("id")?)?);
            }
            if !new.is_empty() {
                // Postgres sums BIGINT as NUMERIC, which the `Any` driver can't read: cast back.
                let sql = format!(
                    "SELECT CAST(COALESCE(SUM(stored_size), 0) AS BIGINT) AS total FROM blobs WHERE kind = $1 AND id IN ({})",
                    placeholders(2, new.len())
                );
                let mut sum = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(BlobKind::Chunk.code());
                for id in &new {
                    sum = sum.bind(id.to_vec());
                }
                let size: i64 = sum.fetch_one(&mut *w.tx).await?.try_get("total")?;
                charge(&mut w.tx, target, size, self.stored_cap).await?;
            }
        }
        w.commit().await?;
        let absent: HashSet<[u8; 32]> = absent.into_iter().collect();
        let (mut claimed, mut missing) = (Vec::new(), Vec::new());
        for id in unique {
            if !absent.contains(&id) || in_source.contains(&id) {
                claimed.push(ChunkId::from_bytes(id));
            } else {
                missing.push(ChunkId::from_bytes(id));
            }
        }
        Ok(Claim { claimed, missing })
    }

    /// Records that `space` uploaded the manifest. Every chunk it lists must already be in the space;
    /// otherwise nothing is recorded and the missing chunks are returned (the first `MAX_MISSING_IDS` of them).
    /// Refused with `QuotaExceeded`, recording nothing, if it would take the space over the cap.
    ///
    /// The chunk check runs before the write transaction, not in it: for a manifest of a million chunks it is
    /// thousands of queries, and on SQLite every other write would wait behind them. A space never loses a chunk
    /// before garbage collection exists, and GC's generation rule covers the gap once it does (an upload touches what
    /// it references).
    pub async fn add_manifest(&self, space: SpaceId, manifest: &Manifest, stored_size: u64, now_ms: i64) -> Result<(), MetaError> {
        let raw: Vec<[u8; 32]> = manifest.chunks().iter().map(|c| *c.id.as_bytes()).collect();
        let mut missing: Vec<ChunkId> = {
            let mut conn = self.pool.acquire().await?;
            missing_in_space(&mut conn, space, BlobKind::Chunk, &raw).await?.into_iter().map(ChunkId::from_bytes).collect()
        };
        if !missing.is_empty() {
            // As the server's own check does: at most `MAX_MISSING_IDS` are listed; the client asks again for the rest.
            missing.truncate(MAX_MISSING_IDS);
            return Err(MetaError::MissingChunks(missing));
        }
        let mut w = self.begin_write().await?;
        self.check_guard(&mut w.tx, space).await?;
        let id = manifest.id();
        let blob = Blob { kind: BlobKind::Manifest, id: id.as_bytes(), stored_size: to_i64(stored_size)?, file_size: Some(to_i64(manifest.size())?) };
        record(&mut w.tx, space, blob, now_ms, self.stored_cap).await?;
        // The index of the manifest's chunks is kept in the same transaction (see `index`): an accepted manifest is never
        // without an index row.
        crate::index::register(&mut w.tx, self.dialect == Dialect::Postgres, manifest, self.index_rows).await?;
        w.commit().await
    }

    /// Whether this space has the manifest.
    pub async fn has_manifest(&self, space: SpaceId, id: &ManifestId) -> Result<bool, MetaError> {
        let mut conn = self.pool.acquire().await?;
        space_has(&mut conn, space, BlobKind::Manifest, id.as_bytes()).await
    }
}
