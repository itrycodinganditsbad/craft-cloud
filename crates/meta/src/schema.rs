//! Schema migrations: numbered SQL files per backend, applied in order, each in its own transaction, and
//! recorded in `schema_version`.

use sqlx::{AnyPool, Row};

use crate::{Dialect, MetaError};

const SQLITE: &[&str] = &[include_str!("../migrations/sqlite/0001_init.sql")];
const POSTGRES: &[&str] = &[include_str!("../migrations/postgres/0001_init.sql")];

pub(crate) async fn migrate(pool: &AnyPool, dialect: Dialect) -> Result<(), MetaError> {
    sqlx::query("CREATE TABLE IF NOT EXISTS schema_version (version BIGINT NOT NULL)").execute(pool).await?;
    let migrations = match dialect {
        Dialect::Sqlite => SQLITE,
        Dialect::Postgres => POSTGRES,
    };
    let current: i64 = sqlx::query("SELECT COALESCE(MAX(version), 0) AS v FROM schema_version").fetch_one(pool).await?.try_get("v")?;
    let known = i64::try_from(migrations.len()).map_err(|_| MetaError::Invalid("too many migrations"))?;
    if current > known {
        return Err(MetaError::Corrupt("the database was created by a newer Craft Cloud"));
    }
    for (index, sql) in migrations.iter().enumerate().skip(usize::try_from(current).unwrap_or(usize::MAX)) {
        let version = i64::try_from(index).map_err(|_| MetaError::Invalid("too many migrations"))? + 1;
        let mut tx = pool.begin().await?;
        for statement in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            sqlx::query(statement).execute(&mut *tx).await?;
        }
        sqlx::query("INSERT INTO schema_version (version) VALUES ($1)").bind(version).execute(&mut *tx).await?;
        tx.commit().await?;
    }
    Ok(())
}
