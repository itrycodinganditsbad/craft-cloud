//! Which chunks and manifests exist, and which spaces may use them.
//!
//! A blob is stored once (`blobs`), but each space only "has" the blobs it uploaded (`space_blobs`). Asking
//! which chunks are missing answers for the asking space only, so nobody can learn whether another user
//! stores the same data: they upload it again, and the server just doesn't keep a second copy.

use craft_cloud_proto::{ChunkId, Manifest, ManifestId, SpaceId};

use crate::codec::{to_i64, uuid_bytes};
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

impl Meta {
    /// Of `ids`, the chunks this space doesn't have yet, in the order given (duplicates once).
    pub async fn missing_chunks(&self, space: SpaceId, ids: &[ChunkId]) -> Result<Vec<ChunkId>, MetaError> {
        let mut conn = self.pool.acquire().await?;
        let mut missing = Vec::new();
        for id in ids {
            if !missing.contains(id) && !space_has(&mut conn, space, BlobKind::Chunk, id.as_bytes()).await? {
                missing.push(*id);
            }
        }
        Ok(missing)
    }

    /// Records that `space` uploaded the chunk (the caller has stored and verified the bytes).
    pub async fn add_chunk(&self, space: SpaceId, id: &ChunkId, stored_size: u64, now_ms: i64) -> Result<(), MetaError> {
        let mut w = self.begin_write().await?;
        record(&mut w.tx, space, BlobKind::Chunk, id.as_bytes(), to_i64(stored_size)?, None, now_ms).await?;
        w.commit().await
    }

    /// Records that `space` uploaded the manifest. Every chunk it lists must already be in the space;
    /// otherwise nothing is recorded and the missing chunks are returned.
    pub async fn add_manifest(&self, space: SpaceId, manifest: &Manifest, stored_size: u64, now_ms: i64) -> Result<(), MetaError> {
        let mut w = self.begin_write().await?;
        let mut missing: Vec<ChunkId> = Vec::new();
        for chunk in manifest.chunks() {
            if !missing.contains(&chunk.id) && !space_has(&mut w.tx, space, BlobKind::Chunk, chunk.id.as_bytes()).await? {
                missing.push(chunk.id);
            }
        }
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
