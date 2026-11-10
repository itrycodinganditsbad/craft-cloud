//! Read-only storage estimates for one space.

use craft_cloud_proto::SpaceId;
use sqlx::Row;

use crate::blobs::effective_quota;
use crate::codec::{to_u64, uuid_bytes, uuid_from};
use crate::{Meta, MetaError, UserId};

const STORAGE_ESTIMATE_SQL: &str = include_str!("usage_estimate.sql");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageEstimateReason {
    IndexIncomplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageEstimate {
    pub stored_bytes: u64,
    pub quota_bytes: Option<u64>,
    pub trash_bytes: Option<u64>,
    pub history_bytes: Option<u64>,
    pub reason: Option<StorageEstimateReason>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageAccount {
    pub id: UserId,
    pub username: String,
    pub stored_bytes: u64,
    pub quota_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageOverview {
    pub accounts: u64,
    pub stored_bytes: u64,
    pub default_quota_bytes: Option<u64>,
    pub accounts_with_override: u64,
    pub accounts_near_quota: u64,
    pub accounts_over_quota: u64,
    pub top_accounts: Vec<StorageAccount>,
    pub manifest_index: StorageIndexCounts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageIndexCounts {
    pub complete: u64,
    pub building: u64,
    pub not_indexed: u64,
}

const STORAGE_OVERVIEW_SQL: &str = "WITH account_rows AS (\
    SELECT u.id, u.username, s.stored_bytes, s.quota_bytes, COALESCE(s.quota_bytes, $1) AS effective_quota \
    FROM users u JOIN spaces s ON s.owner = u.id WHERE u.removed_ms IS NULL\
), account_totals AS (\
    SELECT COUNT(*) AS accounts, CAST(COALESCE(SUM(stored_bytes), 0) AS BIGINT) AS stored_bytes, \
        COALESCE(SUM(CASE WHEN quota_bytes IS NOT NULL THEN 1 ELSE 0 END), 0) AS accounts_with_override, \
        COALESCE(SUM(CASE WHEN effective_quota IS NOT NULL AND stored_bytes >= effective_quota - (effective_quota / 10) THEN 1 ELSE 0 END), 0) AS accounts_near_quota, \
        COALESCE(SUM(CASE WHEN effective_quota IS NOT NULL AND stored_bytes >= effective_quota THEN 1 ELSE 0 END), 0) AS accounts_over_quota \
    FROM account_rows\
), index_counts AS (\
    SELECT COUNT(*) AS manifests, \
        COALESCE(SUM(CASE WHEN i.state = 1 THEN 1 ELSE 0 END), 0) AS complete, \
        COALESCE(SUM(CASE WHEN i.manifest IS NOT NULL AND i.state <> 1 THEN 1 ELSE 0 END), 0) AS building \
    FROM blobs b LEFT JOIN manifest_index i ON i.manifest = b.id WHERE b.kind = 1\
) SELECT account_totals.*, index_counts.manifests, index_counts.complete AS index_complete, index_counts.building AS index_building \
FROM account_totals CROSS JOIN index_counts";

impl Meta {
    /// Reads one consistent snapshot. Estimates are null while any manifest in the space has an incomplete index.
    pub async fn storage_estimate(&self, space: SpaceId) -> Result<StorageEstimate, MetaError> {
        let mut tx = self.read_snapshot().await?;
        let row = sqlx::query(STORAGE_ESTIMATE_SQL)
            .bind(uuid_bytes(space.as_uuid()))
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(MetaError::NotFound("space"))?;
        let estimate = self.storage_estimate_from_row(&row)?;
        tx.commit().await?;
        Ok(estimate)
    }

    /// Reads an account's estimate in one snapshot. Removed and unknown accounts are indistinguishable.
    pub async fn admin_storage_estimate(&self, user: UserId) -> Result<StorageEstimate, MetaError> {
        let mut tx = self.read_snapshot().await?;
        let row = sqlx::query("SELECT s.id FROM users u JOIN spaces s ON s.owner = u.id WHERE u.id = $1 AND u.removed_ms IS NULL")
            .bind(uuid_bytes(&user.0))
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(MetaError::NotFound("user"))?;
        let space = uuid_from(&row.try_get::<Vec<u8>, _>("id")?)?;
        let row = sqlx::query(STORAGE_ESTIMATE_SQL).bind(uuid_bytes(&space)).fetch_optional(&mut *tx).await?.ok_or(MetaError::NotFound("user"))?;
        let estimate = self.storage_estimate_from_row(&row)?;
        tx.commit().await?;
        Ok(estimate)
    }

    /// Aggregate counts and the ten largest current accounts from one consistent snapshot.
    pub async fn admin_storage_overview(&self) -> Result<StorageOverview, MetaError> {
        let mut tx = self.read_snapshot().await?;
        let cap = self.stored_bytes_cap().map(i64::try_from).transpose().map_err(|_| MetaError::Invalid("number too large"))?;
        let row = sqlx::query(STORAGE_OVERVIEW_SQL).bind(cap).fetch_one(&mut *tx).await?;
        let top_rows = sqlx::query(
            "SELECT u.id, u.username, s.stored_bytes, COALESCE(s.quota_bytes, $1) AS effective_quota \
             FROM users u JOIN spaces s ON s.owner = u.id WHERE u.removed_ms IS NULL \
             ORDER BY s.stored_bytes DESC, u.username ASC, u.id ASC LIMIT 10",
        )
        .bind(cap)
        .fetch_all(&mut *tx)
        .await?;
        let top_accounts = top_rows
            .iter()
            .map(|row| {
                let quota: Option<i64> = row.try_get("effective_quota")?;
                Ok(StorageAccount {
                    id: UserId(uuid_from(&row.try_get::<Vec<u8>, _>("id")?)?),
                    username: row.try_get("username")?,
                    stored_bytes: to_u64(row.try_get("stored_bytes")?)?,
                    quota_bytes: quota.map(to_u64).transpose()?,
                })
            })
            .collect::<Result<Vec<_>, MetaError>>()?;
        let manifests: u64 = to_u64(row.try_get("manifests")?)?;
        let complete: u64 = to_u64(row.try_get("index_complete")?)?;
        let building: u64 = to_u64(row.try_get("index_building")?)?;
        let overview = StorageOverview {
            accounts: to_u64(row.try_get("accounts")?)?,
            stored_bytes: to_u64(row.try_get("stored_bytes")?)?,
            default_quota_bytes: self.stored_bytes_cap(),
            accounts_with_override: to_u64(row.try_get("accounts_with_override")?)?,
            accounts_near_quota: to_u64(row.try_get("accounts_near_quota")?)?,
            accounts_over_quota: to_u64(row.try_get("accounts_over_quota")?)?,
            top_accounts,
            manifest_index: StorageIndexCounts { complete, building, not_indexed: manifests.saturating_sub(complete).saturating_sub(building) },
        };
        tx.commit().await?;
        Ok(overview)
    }

    fn storage_estimate_from_row(&self, row: &sqlx::any::AnyRow) -> Result<StorageEstimate, MetaError> {
        let stored_bytes = to_u64(row.try_get("stored_bytes")?)?;
        let stored_quota: Option<i64> = row.try_get("quota_bytes")?;
        let quota_bytes = effective_quota(stored_quota.map(to_u64).transpose()?, self.stored_bytes_cap());
        let trash_bytes: Option<i64> = row.try_get("trash_bytes")?;
        let history_bytes: Option<i64> = row.try_get("history_bytes")?;
        let (trash_bytes, history_bytes, reason) = match (trash_bytes, history_bytes) {
            (Some(trash), Some(history)) => (Some(to_u64(trash)?), Some(to_u64(history)?), None),
            (None, None) => (None, None, Some(StorageEstimateReason::IndexIncomplete)),
            _ => return Err(MetaError::Corrupt("storage estimates have inconsistent index state")),
        };
        Ok(StorageEstimate { stored_bytes, quota_bytes, trash_bytes, history_bytes, reason })
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

    #[tokio::test]
    async fn storage_overview_plan_uses_bounded_aggregate_scans() {
        let meta = Meta::open("sqlite::memory:").await.unwrap();
        let plan_sql = format!("EXPLAIN QUERY PLAN {}", super::STORAGE_OVERVIEW_SQL);
        let rows = sqlx::query(sqlx::AssertSqlSafe(plan_sql)).bind(Some(1_000_i64)).fetch_all(&meta.pool).await.unwrap();
        let plan = rows.iter().map(|row| row.try_get::<String, _>("detail").unwrap()).collect::<Vec<_>>().join(" | ");
        assert!(plan.contains("SCAN u") || plan.contains("SEARCH u"), "missing account aggregate scan in plan: {plan}");
        assert!(plan.contains("b USING") && plan.contains("i USING"), "missing bounded manifest/index scans in plan: {plan}");
        println!("storage overview SQLite plan: {plan}");
    }
}
