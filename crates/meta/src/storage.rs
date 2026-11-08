//! What the blob store really holds: each blob's physical size next to its logical one, a way to walk the blobs
//! (for recompressing and measuring), and a few small server-owned values.
//!
//! `blobs.stored_size` is the logical size (the plain bytes plus the format byte) that quotas count. `physical_size`
//! is the length of the object on the backend, which is smaller when it is compressed. It is `NULL` for blobs written
//! before it was tracked, and every total here says how much of the data it knows.

use craft_cloud_proto::BlobKind;
use sqlx::Row;

use crate::blobs::KindCode;
use crate::codec::{hash_from, to_i64, to_u64};
use crate::{Meta, MetaError};

/// One row of `blobs`, as the storage tools see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobRow {
    pub kind: BlobKind,
    pub id: [u8; 32],
    /// The logical size: the plain bytes plus the format byte.
    pub stored_size: u64,
    /// The size of the stored object, when known.
    pub physical_size: Option<u64>,
    /// The zstd level the object was last compressed or tried at (`0`: not tried), when known.
    pub level: Option<u8>,
}

/// Sums over all blobs. Only blobs with a known physical size count towards `known_*`, so the saving they give is
/// never mixed with blobs nobody has measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StorageTotals {
    pub blobs: u64,
    /// The logical size of every blob.
    pub logical_bytes: u64,
    pub known_blobs: u64,
    /// The logical size of the blobs whose physical size is known.
    pub known_logical_bytes: u64,
    /// The physical size of those same blobs.
    pub known_physical_bytes: u64,
}

fn kind_from(code: i64) -> Result<BlobKind, MetaError> {
    match code {
        0 => Ok(BlobKind::Chunk),
        1 => Ok(BlobKind::Manifest),
        _ => Err(MetaError::Corrupt("a stored blob kind is unknown")),
    }
}

fn blob_row(row: &sqlx::any::AnyRow) -> Result<BlobRow, MetaError> {
    let physical: Option<i64> = row.try_get("physical_size")?;
    let level: Option<i64> = row.try_get("compress_level")?;
    Ok(BlobRow {
        kind: kind_from(row.try_get::<i64, _>("kind")?)?,
        id: hash_from(&row.try_get::<Vec<u8>, _>("id")?)?,
        stored_size: to_u64(row.try_get("stored_size")?)?,
        physical_size: physical.map(to_u64).transpose()?,
        level: level.map(|l| u8::try_from(l).map_err(|_| MetaError::Corrupt("a stored compression level is out of range"))).transpose()?,
    })
}

// `kind` is a SMALLINT; the `Any` driver reads integers as BIGINT.
const BLOB_COLUMNS: &str = "CAST(kind AS BIGINT) AS kind, id, stored_size, physical_size, compress_level";

impl Meta {
    /// Records how big the object of a blob is on the backend and the level it was written at. A blob without a
    /// row is not recorded (an upload refused by a quota): nothing changes. Never fails an upload's data: callers
    /// treat an error here as a missing statistic.
    pub async fn set_physical(&self, kind: BlobKind, id: &[u8; 32], physical_size: u64, level: u8) -> Result<(), MetaError> {
        let mut w = self.begin_write().await?;
        sqlx::query("UPDATE blobs SET physical_size = $1, compress_level = $2 WHERE kind = $3 AND id = $4")
            .bind(to_i64(physical_size)?)
            .bind(i64::from(level))
            .bind(kind.code())
            .bind(id.to_vec())
            .execute(&mut *w.tx)
            .await?;
        w.commit().await
    }

    /// Records only the physical size of a blob (a measurement), leaving the level it was written at as it is.
    pub async fn set_measured_size(&self, kind: BlobKind, id: &[u8; 32], physical_size: u64) -> Result<(), MetaError> {
        let mut w = self.begin_write().await?;
        sqlx::query("UPDATE blobs SET physical_size = $1 WHERE kind = $2 AND id = $3")
            .bind(to_i64(physical_size)?)
            .bind(kind.code())
            .bind(id.to_vec())
            .execute(&mut *w.tx)
            .await?;
        w.commit().await
    }

    /// Up to `limit` blobs in `(kind, id)` order after `after` (the start when `None`), for walking all of them in
    /// pages. The next page starts after the last row's `(kind, id)`.
    pub async fn blob_page(&self, after: Option<(BlobKind, [u8; 32])>, limit: usize) -> Result<Vec<BlobRow>, MetaError> {
        let mut conn = self.pool.acquire().await?;
        let limit = i64::try_from(limit.min(10_000)).map_err(|_| MetaError::Invalid("page too large"))?;
        let rows = match after {
            None => {
                sqlx::query(sqlx::AssertSqlSafe(format!("SELECT {BLOB_COLUMNS} FROM blobs ORDER BY kind, id LIMIT $1")))
                    .bind(limit)
                    .fetch_all(&mut *conn)
                    .await?
            }
            Some((kind, id)) => {
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "SELECT {BLOB_COLUMNS} FROM blobs WHERE kind > $1 OR (kind = $1 AND id > $2) ORDER BY kind, id LIMIT $3"
                )))
                .bind(kind.code())
                .bind(id.to_vec())
                .bind(limit)
                .fetch_all(&mut *conn)
                .await?
            }
        };
        rows.iter().map(blob_row).collect()
    }

    /// The first blob of `kind` whose ID is `from` or after it, wrapping to the first of the kind when there is none:
    /// with a random `from`, a cheap pick of a blob (IDs are hashes, so they are spread evenly). `None` when there
    /// are no blobs of the kind.
    pub async fn blob_from(&self, kind: BlobKind, from: &[u8; 32]) -> Result<Option<BlobRow>, MetaError> {
        let mut conn = self.pool.acquire().await?;
        let found = sqlx::query(sqlx::AssertSqlSafe(format!("SELECT {BLOB_COLUMNS} FROM blobs WHERE kind = $1 AND id >= $2 ORDER BY id LIMIT 1")))
            .bind(kind.code())
            .bind(from.to_vec())
            .fetch_optional(&mut *conn)
            .await?;
        let row = match found {
            Some(row) => Some(row),
            None => {
                sqlx::query(sqlx::AssertSqlSafe(format!("SELECT {BLOB_COLUMNS} FROM blobs WHERE kind = $1 ORDER BY id LIMIT 1")))
                    .bind(kind.code())
                    .fetch_optional(&mut *conn)
                    .await?
            }
        };
        row.as_ref().map(blob_row).transpose()
    }

    /// Sums over all blobs: one scan of the `blobs` table. Callers that serve it to many requests cache it.
    pub async fn storage_totals(&self) -> Result<StorageTotals, MetaError> {
        let mut conn = self.pool.acquire().await?;
        // Postgres sums BIGINT as NUMERIC, which the `Any` driver can't read: cast back.
        let row = sqlx::query(
            "SELECT COUNT(*) AS blobs, \
             CAST(COALESCE(SUM(stored_size), 0) AS BIGINT) AS logical, \
             COUNT(physical_size) AS known, \
             CAST(COALESCE(SUM(CASE WHEN physical_size IS NOT NULL THEN stored_size ELSE 0 END), 0) AS BIGINT) AS known_logical, \
             CAST(COALESCE(SUM(physical_size), 0) AS BIGINT) AS known_physical \
             FROM blobs",
        )
        .fetch_one(&mut *conn)
        .await?;
        Ok(StorageTotals {
            blobs: to_u64(row.try_get("blobs")?)?,
            logical_bytes: to_u64(row.try_get("logical")?)?,
            known_blobs: to_u64(row.try_get("known")?)?,
            known_logical_bytes: to_u64(row.try_get("known_logical")?)?,
            known_physical_bytes: to_u64(row.try_get("known_physical")?)?,
        })
    }

    /// A small server-owned value and when it was set (ms), or `None`.
    pub async fn server_value(&self, key: &str) -> Result<Option<(String, i64)>, MetaError> {
        let mut conn = self.pool.acquire().await?;
        let row = sqlx::query("SELECT value, updated_ms FROM server_state WHERE key = $1").bind(key).fetch_optional(&mut *conn).await?;
        row.map(|row| Ok((row.try_get("value")?, row.try_get("updated_ms")?))).transpose()
    }

    /// Sets a server-owned value.
    pub async fn set_server_value(&self, key: &str, value: &str, now_ms: i64) -> Result<(), MetaError> {
        let mut w = self.begin_write().await?;
        set_server_value_in(&mut w.tx, key, value, now_ms).await?;
        w.commit().await
    }
}

/// `set_server_value` inside a transaction the caller owns.
pub(crate) async fn set_server_value_in(tx: &mut sqlx::AnyConnection, key: &str, value: &str, now_ms: i64) -> Result<(), MetaError> {
    sqlx::query(
        "INSERT INTO server_state (key, value, updated_ms) VALUES ($1, $2, $3) \
         ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_ms = excluded.updated_ms",
    )
    .bind(key)
    .bind(value)
    .bind(now_ms)
    .execute(&mut *tx)
    .await?;
    Ok(())
}
