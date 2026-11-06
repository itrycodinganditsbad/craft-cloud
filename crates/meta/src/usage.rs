//! Read-only storage estimates for one space.

use craft_cloud_proto::SpaceId;
use sqlx::Row;

use crate::codec::{to_u64, uuid_bytes};
use crate::{Meta, MetaError};

const STORAGE_ESTIMATE_SQL: &str = include_str!("usage_estimate.sql");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageEstimateReason {
    IndexIncomplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageEstimate {
    pub stored_bytes: u64,
    pub trash_bytes: Option<u64>,
    pub history_bytes: Option<u64>,
    pub reason: Option<StorageEstimateReason>,
}

impl Meta {
    /// Reads one consistent snapshot. Estimates are null while any manifest in the space has an incomplete index.
    pub async fn storage_estimate(&self, space: SpaceId) -> Result<StorageEstimate, MetaError> {
        let mut tx = self.read_snapshot().await?;
        let row = sqlx::query(STORAGE_ESTIMATE_SQL)
            .bind(uuid_bytes(space.as_uuid()))
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(MetaError::NotFound("space"))?;
        tx.commit().await?;

        let stored_bytes = to_u64(row.try_get("stored_bytes")?)?;
        let trash_bytes: Option<i64> = row.try_get("trash_bytes")?;
        let history_bytes: Option<i64> = row.try_get("history_bytes")?;
        let (trash_bytes, history_bytes, reason) = match (trash_bytes, history_bytes) {
            (Some(trash), Some(history)) => (Some(to_u64(trash)?), Some(to_u64(history)?), None),
            (None, None) => (None, None, Some(StorageEstimateReason::IndexIncomplete)),
            _ => return Err(MetaError::Corrupt("storage estimates have inconsistent index state")),
        };
        Ok(StorageEstimate { stored_bytes, trash_bytes, history_bytes, reason })
    }
}

#[cfg(test)]
mod tests {
    use craft_cloud_proto::{ChunkId, ChunkRef, ChunkerParamsId, FileHash, Manifest};
    use sqlx::Row;

    use crate::{Meta, Role};

    #[tokio::test]
    async fn any_incomplete_space_manifest_makes_both_estimates_null() {
        let meta = Meta::open("sqlite::memory:").await.unwrap();
        let owner = meta.create_user("ana", "$argon2id$stub", Role::Member, 1).await.unwrap();
        let chunk = ChunkId::from_bytes([1; 32]);
        meta.add_chunk(owner.space, &chunk, 6, 1).await.unwrap();
        let manifest =
            Manifest::new(ChunkerParamsId::new("fastcdc-v1").unwrap(), 5, FileHash::from_bytes([2; 32]), vec![ChunkRef { id: chunk, len: 5 }])
                .unwrap();
        meta.add_manifest(owner.space, &manifest, 100, 1).await.unwrap();
        sqlx::query("UPDATE manifest_index SET state = 0 WHERE manifest = $1")
            .bind(manifest.id().as_bytes().to_vec())
            .execute(&meta.pool)
            .await
            .unwrap();

        let estimate = meta.storage_estimate(owner.space).await.unwrap();
        assert_eq!((estimate.trash_bytes, estimate.history_bytes), (None, None));
        assert_eq!(estimate.reason, Some(super::StorageEstimateReason::IndexIncomplete));
    }

    #[tokio::test]
    async fn storage_estimate_plan_uses_reference_and_version_indexes() {
        let meta = Meta::open("sqlite::memory:").await.unwrap();
        let owner = meta.create_user("ana", "$argon2id$stub", Role::Member, 1).await.unwrap();
        let plan_sql = format!("EXPLAIN QUERY PLAN {}", super::STORAGE_ESTIMATE_SQL);
        let rows =
            sqlx::query(sqlx::AssertSqlSafe(plan_sql)).bind(crate::codec::uuid_bytes(owner.space.as_uuid())).fetch_all(&meta.pool).await.unwrap();
        let plan = rows.iter().map(|row| row.try_get::<String, _>("detail").unwrap()).collect::<Vec<_>>().join(" | ");
        assert!(plan.contains("versions_node_seq"), "missing existing version index in plan: {plan}");
        assert!(plan.contains("manifest_chunks_by_chunk"), "missing existing reverse-reference index in plan: {plan}");
        println!("storage estimate SQLite plan: {plan}");
    }
}
