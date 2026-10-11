//! Which chunks and manifests exist, and which spaces may use them.
//!
//! A blob is stored once (`blobs`), but each space only "has" the blobs it uploaded (`space_blobs`). Asking
//! which chunks are missing answers for the asking space only, so nobody can learn whether another user
//! stores the same data: they upload it again, and the server just doesn't keep a second copy.

use std::collections::HashSet;

use craft_cloud_proto::{ChunkId, Manifest, ManifestId, SpaceId};
use sqlx::Row;

use crate::codec::{hash_from, to_i64, to_u64, uuid_bytes};
use crate::{Meta, MetaError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobKind {
    Chunk,
    Manifest,
}

impl BlobKind {
    pub(crate) fn code(self) -> i16 {
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
fn placeholders(first: usize, count: usize) -> String {
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

async fn record(
    tx: &mut sqlx::AnyConnection,
    space: SpaceId,
    kind: BlobKind,
    id: &[u8; 32],
    stored_size: i64,
    file_size: Option<i64>,
    now_ms: i64,
) -> Result<(), MetaError> {
    sqlx::query("INSERT INTO blobs (kind, id, stored_size, file_size, created_ms) VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING")
        .bind(kind.code())
        .bind(id.to_vec())
        .bind(stored_size)
        .bind(file_size)
        .bind(now_ms)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO space_blobs (space_id, kind, id) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
        .bind(uuid_bytes(space.as_uuid()))
        .bind(kind.code())
        .bind(id.to_vec())
        .execute(&mut *tx)
        .await?;
    Ok(())
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

    /// Records that `space` uploaded the chunk (the caller has stored and verified the bytes).
    pub async fn add_chunk(&self, space: SpaceId, id: &ChunkId, stored_size: u64, now_ms: i64) -> Result<(), MetaError> {
        let mut w = self.begin_write().await?;
        record(&mut w.tx, space, BlobKind::Chunk, id.as_bytes(), to_i64(stored_size)?, None, now_ms).await?;
        w.commit().await
    }

    /// Records that `space` uploaded all of these chunks (`(id, stored size)`), in one transaction.
    pub async fn add_chunks(&self, space: SpaceId, chunks: &[(ChunkId, u64)], now_ms: i64) -> Result<(), MetaError> {
        let mut w = self.begin_write().await?;
        for part in chunks.chunks(BATCH) {
            let rows = |width: usize| (0..part.len()).map(|i| format!("({})", placeholders(i * width + 1, width))).collect::<Vec<_>>().join(", ");
            let mut blobs = sqlx::query(sqlx::AssertSqlSafe(format!(
                "INSERT INTO blobs (kind, id, stored_size, file_size, created_ms) VALUES {} ON CONFLICT DO NOTHING",
                rows(5)
            )));
            let mut owned =
                sqlx::query(sqlx::AssertSqlSafe(format!("INSERT INTO space_blobs (space_id, kind, id) VALUES {} ON CONFLICT DO NOTHING", rows(3))));
            for (id, stored_size) in part {
                blobs = blobs.bind(BlobKind::Chunk.code()).bind(id.as_bytes().to_vec()).bind(to_i64(*stored_size)?).bind(None::<i64>).bind(now_ms);
                owned = owned.bind(uuid_bytes(space.as_uuid())).bind(BlobKind::Chunk.code()).bind(id.as_bytes().to_vec());
            }
            blobs.execute(&mut *w.tx).await?;
            owned.execute(&mut *w.tx).await?;
        }
        w.commit().await
    }

    /// Records that `space` uploaded the manifest. Every chunk it lists must already be in the space;
    /// otherwise nothing is recorded and the missing chunks are returned.
    pub async fn add_manifest(&self, space: SpaceId, manifest: &Manifest, stored_size: u64, now_ms: i64) -> Result<(), MetaError> {
        let mut w = self.begin_write().await?;
        let raw: Vec<[u8; 32]> = manifest.chunks().iter().map(|c| *c.id.as_bytes()).collect();
        let missing: Vec<ChunkId> = missing_in_space(&mut w.tx, space, BlobKind::Chunk, &raw).await?.into_iter().map(ChunkId::from_bytes).collect();
        if !missing.is_empty() {
            return Err(MetaError::MissingChunks(missing));
        }
        record(&mut w.tx, space, BlobKind::Manifest, manifest.id().as_bytes(), to_i64(stored_size)?, Some(to_i64(manifest.size())?), now_ms).await?;
        w.commit().await
    }

    /// Whether this space has the manifest.
    pub async fn has_manifest(&self, space: SpaceId, id: &ManifestId) -> Result<bool, MetaError> {
        let mut conn = self.pool.acquire().await?;
        space_has(&mut conn, space, BlobKind::Manifest, id.as_bytes()).await
    }
}
