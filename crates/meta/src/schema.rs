//! Schema migrations: numbered SQL files per backend, applied in order, each in its own transaction, and
//! recorded in `schema_version`.

use sqlx::{AnyPool, Row};

use crate::{Dialect, MetaError};

const SQLITE: &[&str] = &[
    include_str!("../migrations/sqlite/0001_init.sql"),
    include_str!("../migrations/sqlite/0002_stored_bytes.sql"),
    include_str!("../migrations/sqlite/0003_live.sql"),
    include_str!("../migrations/sqlite/0004_join.sql"),
    include_str!("../migrations/sqlite/0005_live_documents.sql"),
];
const POSTGRES: &[&str] = &[
    include_str!("../migrations/postgres/0001_init.sql"),
    include_str!("../migrations/postgres/0002_stored_bytes.sql"),
    include_str!("../migrations/postgres/0003_live.sql"),
    include_str!("../migrations/postgres/0004_join.sql"),
    include_str!("../migrations/postgres/0005_live_documents.sql"),
];

/// Arbitrary fixed key for the Postgres advisory lock that serialises migrations between servers sharing one
/// database. Every server must use the same key; no other code takes this lock.
const MIGRATION_LOCK_KEY: i64 = 7240318150661;

pub(crate) async fn migrate(pool: &AnyPool, dialect: Dialect) -> Result<(), MetaError> {
    // One connection for the whole run. The Postgres advisory lock belongs to this session, so the lock,
    // the work and the unlock must all use it.
    let mut conn = pool.acquire().await?;
    if dialect == Dialect::Postgres {
        // Taken before anything else, so two servers starting on one database run their migrations one at a
        // time, and the second sees the first's finished schema.
        sqlx::query("SELECT pg_advisory_lock($1)").bind(MIGRATION_LOCK_KEY).execute(&mut *conn).await?;
    }
    // SQLite has no lock here: one process per SQLite file is the supported setup.
    let work = async {
        sqlx::query("CREATE TABLE IF NOT EXISTS schema_version (version BIGINT NOT NULL)").execute(&mut *conn).await?;
        let migrations = match dialect {
            Dialect::Sqlite => SQLITE,
            Dialect::Postgres => POSTGRES,
        };
        let current: i64 = sqlx::query("SELECT COALESCE(MAX(version), 0) AS v FROM schema_version").fetch_one(&mut *conn).await?.try_get("v")?;
        let known = i64::try_from(migrations.len()).map_err(|_| MetaError::Invalid("too many migrations"))?;
        if current > known {
            return Err(MetaError::Corrupt("the database was created by a newer Craft Cloud"));
        }
        for (index, sql) in migrations.iter().enumerate().skip(usize::try_from(current).unwrap_or(usize::MAX)) {
            let version = i64::try_from(index).map_err(|_| MetaError::Invalid("too many migrations"))? + 1;
            let mut tx = sqlx::Connection::begin(&mut *conn).await?;
            // Statements are split on `;`, so a migration file must not contain `;` inside a string literal
            // or a comment.
            for statement in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                sqlx::query(statement).execute(&mut *tx).await?;
            }
            sqlx::query("INSERT INTO schema_version (version) VALUES ($1)").bind(version).execute(&mut *tx).await?;
            tx.commit().await?;
        }
        Ok(())
    }
    .await;
    if dialect == Dialect::Postgres {
        let unlocked = sqlx::query("SELECT pg_advisory_unlock($1)").bind(MIGRATION_LOCK_KEY).execute(&mut *conn).await;
        if unlocked.is_err() {
            // The session may still hold the lock. Closing the session releases it, so detach and drop the
            // connection rather than returning it to the pool.
            drop(conn.detach());
        }
        unlocked?;
    }
    work
}
