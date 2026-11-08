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
    include_str!("../migrations/sqlite/0006_save_records.sql"),
    include_str!("../migrations/sqlite/0007_devices_v2.sql"),
    include_str!("../migrations/sqlite/0008_admin_resets.sql"),
    include_str!("../migrations/sqlite/0009_audit_events.sql"),
    include_str!("../migrations/sqlite/0010_two_factor.sql"),
    include_str!("../migrations/sqlite/0011_web_sessions.sql"),
    include_str!("../migrations/sqlite/0012_versions_node_seq.sql"),
    include_str!("../migrations/sqlite/0013_trash.sql"),
    include_str!("../migrations/sqlite/0014_manifest_index.sql"),
    include_str!("../migrations/sqlite/0015_physical_size.sql"),
    include_str!("../migrations/sqlite/0016_team_spaces.sql"),
];
const POSTGRES: &[&str] = &[
    include_str!("../migrations/postgres/0001_init.sql"),
    include_str!("../migrations/postgres/0002_stored_bytes.sql"),
    include_str!("../migrations/postgres/0003_live.sql"),
    include_str!("../migrations/postgres/0004_join.sql"),
    include_str!("../migrations/postgres/0005_live_documents.sql"),
    include_str!("../migrations/postgres/0006_save_records.sql"),
    include_str!("../migrations/postgres/0007_devices_v2.sql"),
    include_str!("../migrations/postgres/0008_admin_resets.sql"),
    include_str!("../migrations/postgres/0009_audit_events.sql"),
    include_str!("../migrations/postgres/0010_two_factor.sql"),
    include_str!("../migrations/postgres/0011_web_sessions.sql"),
    include_str!("../migrations/postgres/0012_versions_node_seq.sql"),
    include_str!("../migrations/postgres/0013_trash.sql"),
    include_str!("../migrations/postgres/0014_manifest_index.sql"),
    include_str!("../migrations/postgres/0015_physical_size.sql"),
    include_str!("../migrations/postgres/0016_team_spaces.sql"),
];

/// The first line of a SQLite migration that must run with foreign-key enforcement off, because it rebuilds a table
/// other tables reference. The runner sets `PRAGMA foreign_keys = OFF` before the transaction begins (the pragma has no
/// effect inside one), checks `PRAGMA foreign_key_check` before the commit, and turns enforcement on again afterwards,
/// whether the migration worked or not.
const FOREIGN_KEYS_OFF: &str = "-- craft: foreign-keys-off";

fn wants_foreign_keys_off(sql: &str) -> bool {
    sql.lines().next().is_some_and(|line| line.trim_end() == FOREIGN_KEYS_OFF)
}

/// Arbitrary fixed key for the Postgres advisory lock that serialises migrations between servers sharing one
/// database. Every server must use the same key; no other code takes this lock.
const MIGRATION_LOCK_KEY: i64 = 7240318150661;

/// Checks that the database has the current schema, and changes nothing: for a database that must exist already
/// ([`Meta::open_existing`](crate::Meta::open_existing)).
pub(crate) async fn check_current(pool: &AnyPool, dialect: Dialect) -> Result<(), MetaError> {
    let migrations = match dialect {
        Dialect::Sqlite => SQLITE,
        Dialect::Postgres => POSTGRES,
    };
    let known = i64::try_from(migrations.len()).map_err(|_| MetaError::Invalid("too many migrations"))?;
    let table = match dialect {
        Dialect::Sqlite => "SELECT COUNT(*) AS n FROM sqlite_master WHERE type = 'table' AND name = 'schema_version'",
        Dialect::Postgres => {
            "SELECT COUNT(*) AS n FROM information_schema.tables WHERE table_schema = current_schema() AND table_name = 'schema_version'"
        }
    };
    let tables: i64 = sqlx::query(table).fetch_one(pool).await?.try_get("n")?;
    if tables == 0 {
        return Err(MetaError::Missing("the database holds no Craft Cloud data (is the database path right?)"));
    }
    let current: i64 = sqlx::query("SELECT COALESCE(MAX(version), 0) AS v FROM schema_version").fetch_one(pool).await?.try_get("v")?;
    if current == 0 {
        return Err(MetaError::Missing("the database holds no Craft Cloud data (is the database path right?)"));
    }
    if current > known {
        return Err(MetaError::Corrupt("the database was created by a newer Craft Cloud"));
    }
    if current < known {
        return Err(MetaError::Missing("the database schema is older than this program; start the server once (`serve`) to upgrade it"));
    }
    Ok(())
}

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
            if dialect == Dialect::Sqlite && wants_foreign_keys_off(sql) {
                run_with_foreign_keys_off(&mut conn, sql, version).await?;
                continue;
            }
            let mut tx = sqlx::Connection::begin(&mut *conn).await?;
            run_statements(&mut tx, sql, version).await?;
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

/// Runs a migration's statements and records its version, in the transaction `tx`.
async fn run_statements(tx: &mut sqlx::Transaction<'_, sqlx::Any>, sql: &'static str, version: i64) -> Result<(), MetaError> {
    // Statements are split on `;`, so a migration file must not contain `;` inside a string literal or a comment.
    for statement in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        sqlx::query(statement).execute(&mut **tx).await?;
    }
    sqlx::query("INSERT INTO schema_version (version) VALUES ($1)").bind(version).execute(&mut **tx).await?;
    Ok(())
}

/// Runs a SQLite migration that rebuilds a table other tables reference. Foreign-key enforcement is off for the
/// whole transaction and `PRAGMA foreign_key_check` must come back empty before it commits, so a rebuild that left a
/// dangling reference is rolled back and refused. Enforcement is turned on again whatever happened.
async fn run_with_foreign_keys_off(conn: &mut sqlx::pool::PoolConnection<sqlx::Any>, sql: &'static str, version: i64) -> Result<(), MetaError> {
    sqlx::query("PRAGMA foreign_keys = OFF").execute(&mut **conn).await?;
    let migrated = async {
        let mut tx = sqlx::Connection::begin(&mut **conn).await?;
        run_statements(&mut tx, sql, version).await?;
        let dangling = sqlx::query("PRAGMA foreign_key_check").fetch_all(&mut *tx).await?;
        if !dangling.is_empty() {
            return Err(MetaError::Corrupt("a migration left rows that break a foreign key"));
        }
        tx.commit().await?;
        Ok(())
    }
    .await;
    if let Err(e) = sqlx::query("PRAGMA foreign_keys = ON").execute(&mut **conn).await {
        // The connection may still enforce nothing. Close it instead of returning it to the pool.
        conn.close_on_drop();
        migrated?;
        return Err(e.into());
    }
    migrated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Meta;

    async fn enforced(conn: &mut sqlx::AnyConnection) -> i64 {
        sqlx::query("PRAGMA foreign_keys").fetch_one(conn).await.unwrap().try_get::<i64, _>(0).unwrap()
    }

    async fn count(conn: &mut sqlx::AnyConnection, sql: &'static str) -> i64 {
        sqlx::query(sql).fetch_one(conn).await.unwrap().try_get::<i64, _>(0).unwrap()
    }

    #[test]
    fn only_the_first_line_is_the_marker() {
        assert!(wants_foreign_keys_off("-- craft: foreign-keys-off\nCREATE TABLE t (a INT)"));
        assert!(wants_foreign_keys_off("-- craft: foreign-keys-off\r\nCREATE TABLE t (a INT)"));
        assert!(!wants_foreign_keys_off("-- a comment\n-- craft: foreign-keys-off\nCREATE TABLE t (a INT)"));
        assert!(!wants_foreign_keys_off("CREATE TABLE t (a INT)"));
        assert!(!wants_foreign_keys_off("-- craft: foreign-keys-off-ish\n"));
        assert!(!wants_foreign_keys_off(""));
        // The team spaces migration rebuilds `spaces` and says so; the Postgres one needs no marker.
        assert!(wants_foreign_keys_off(SQLITE.last().copied().unwrap_or_default()));
        assert!(!wants_foreign_keys_off(POSTGRES.last().copied().unwrap_or_default()));
    }

    #[tokio::test]
    async fn migrating_leaves_foreign_keys_enforced() {
        let meta = Meta::open("sqlite::memory:").await.unwrap();
        let mut conn = meta.pool.acquire().await.unwrap();
        assert_eq!(enforced(&mut conn).await, 1);
        assert_eq!(count(&mut conn, "SELECT MAX(version) FROM schema_version").await, i64::try_from(SQLITE.len()).unwrap());
    }

    #[tokio::test]
    async fn a_migration_that_breaks_a_foreign_key_is_rolled_back_and_enforcement_comes_back() {
        const DANGLING: &str = "-- craft: foreign-keys-off\n\
             CREATE TABLE probe_parent (id INTEGER PRIMARY KEY);\n\
             CREATE TABLE probe_child (parent INTEGER REFERENCES probe_parent (id));\n\
             INSERT INTO probe_child (parent) VALUES (42)";
        let meta = Meta::open("sqlite::memory:").await.unwrap();
        let mut conn = meta.pool.acquire().await.unwrap();
        let failed = run_with_foreign_keys_off(&mut conn, DANGLING, 900).await;
        assert!(matches!(failed, Err(MetaError::Corrupt(_))), "{failed:?}");
        assert_eq!(enforced(&mut conn).await, 1);
        assert_eq!(count(&mut conn, "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE 'probe_%'").await, 0, "the transaction was not rolled back");
        assert_eq!(count(&mut conn, "SELECT COUNT(*) FROM schema_version WHERE version = 900").await, 0);
    }

    #[tokio::test]
    async fn a_migration_that_fails_midway_changes_nothing_and_enforcement_comes_back() {
        const BROKEN: &str = "-- craft: foreign-keys-off\n\
             CREATE TABLE probe_parent (id INTEGER PRIMARY KEY);\n\
             INSERT INTO probe_missing VALUES (1)";
        let meta = Meta::open("sqlite::memory:").await.unwrap();
        let mut conn = meta.pool.acquire().await.unwrap();
        let failed = run_with_foreign_keys_off(&mut conn, BROKEN, 901).await;
        assert!(matches!(failed, Err(MetaError::Db(_))), "{failed:?}");
        assert_eq!(enforced(&mut conn).await, 1);
        assert_eq!(count(&mut conn, "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE 'probe_%'").await, 0);
    }

    #[tokio::test]
    async fn a_rebuild_with_consistent_rows_commits_and_records_its_version() {
        const REBUILD: &str = "-- craft: foreign-keys-off\n\
             CREATE TABLE probe_parent (id INTEGER PRIMARY KEY);\n\
             CREATE TABLE probe_child (parent INTEGER REFERENCES probe_parent (id));\n\
             INSERT INTO probe_parent (id) VALUES (1);\n\
             INSERT INTO probe_child (parent) VALUES (1);\n\
             DROP TABLE probe_parent;\n\
             CREATE TABLE probe_parent (id INTEGER PRIMARY KEY);\n\
             INSERT INTO probe_parent (id) VALUES (1)";
        let meta = Meta::open("sqlite::memory:").await.unwrap();
        let mut conn = meta.pool.acquire().await.unwrap();
        run_with_foreign_keys_off(&mut conn, REBUILD, 902).await.unwrap();
        assert_eq!(enforced(&mut conn).await, 1);
        assert_eq!(count(&mut conn, "SELECT COUNT(*) FROM probe_child").await, 1);
        assert_eq!(count(&mut conn, "SELECT COUNT(*) FROM schema_version WHERE version = 902").await, 1);
    }
}
