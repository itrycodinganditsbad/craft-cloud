//! Server metadata for Craft Cloud: users, devices, spaces, the node tree, versions, which blobs each space
//! may use, and the batch commit that changes it all. SQLite or Postgres, with the same behaviour.
//!
//! Every write goes through one transaction. On SQLite, writes are serialised by a single lock (SQLite has
//! one writer anyway, and this avoids "database is locked" retries); on Postgres, a commit locks its space's
//! row, so commits to one space run one at a time and commits to different spaces run in parallel.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod blobs;
mod codec;
mod commit;
mod schema;
mod tree;
mod users;

use std::str::FromStr;
use std::sync::Arc;

use craft_cloud_proto::{ChunkId, Conflict};
use sqlx::any::{AnyConnectOptions, AnyPoolOptions};
use sqlx::{AnyPool, ConnectOptions};
use tokio::sync::{Mutex, OwnedMutexGuard};

pub use commit::CommitOutcome;
pub use craft_cloud_proto::BlobKind;
pub use tree::SpaceInfo;
pub use users::{DeviceAuth, DeviceRecord, Role, UserId, UserRecord, valid_username};

#[derive(Debug, thiserror::Error)]
pub enum MetaError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("unsupported database URL (use sqlite:… or postgres://…)")]
    UnsupportedUrl,
    #[error("the username is taken")]
    UsernameTaken,
    #[error("no such {0}")]
    NotFound(&'static str),
    #[error("the batch was refused: {} conflicting operation(s)", .0.len())]
    Rejected(Vec<Conflict>),
    #[error("{} chunk(s) of the manifest are not uploaded", .0.len())]
    MissingChunks(Vec<ChunkId>),
    #[error("the change feed no longer reaches that far back; list the whole tree")]
    ResyncRequired,
    /// A blob would take the space over its stored-bytes cap ([`Meta::with_stored_bytes_cap`]); nothing was
    /// recorded. `used` is what the space stores without it.
    #[error("the account's storage on the server is full: {used} of {limit} bytes used")]
    QuotaExceeded { used: u64, limit: u64 },
    #[error("invalid input: {0}")]
    Invalid(&'static str),
    #[error("stored data is damaged: {0}")]
    Corrupt(&'static str),
}

/// How long a SQLite connection waits for a lock another connection or program holds before giving up.
pub const SQLITE_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialect {
    Sqlite,
    Postgres,
}

/// The metadata store. Cheap to clone.
#[derive(Clone)]
pub struct Meta {
    pool: AnyPool,
    dialect: Dialect,
    /// Serialises write transactions on SQLite. Unused on Postgres.
    write_lock: Arc<Mutex<()>>,
    /// The most bytes one space may store ([`Meta::with_stored_bytes_cap`]); `None`: no cap.
    stored_cap: Option<u64>,
}

impl Meta {
    /// Connects and brings the schema up to date. `url` is `sqlite:path/to/meta.db` (created if missing),
    /// `sqlite::memory:`, or `postgres://user:pass@host/db`.
    pub async fn open(url: &str) -> Result<Self, MetaError> {
        sqlx::any::install_default_drivers();
        let dialect = if url.starts_with("sqlite:") {
            Dialect::Sqlite
        } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
            Dialect::Postgres
        } else {
            return Err(MetaError::UnsupportedUrl);
        };
        let url = match dialect {
            Dialect::Sqlite if !url.contains('?') && !url.contains(":memory:") => format!("{url}?mode=rwc"),
            _ => url.to_owned(),
        };
        let options = AnyConnectOptions::from_str(&url)?.disable_statement_logging();
        let pool_options = match dialect {
            // An in-memory SQLite database exists only as long as its one connection. sqlx closes idle and old
            // connections by default, and a closed one takes the database with it: the next query would open
            // a fresh, empty database. So keep exactly one connection, and never retire it.
            Dialect::Sqlite if url.contains(":memory:") => {
                AnyPoolOptions::new().max_connections(1).min_connections(1).idle_timeout(None).max_lifetime(None)
            }
            Dialect::Sqlite => AnyPoolOptions::new().max_connections(8),
            Dialect::Postgres => AnyPoolOptions::new().max_connections(16),
        };
        // SQLite: a connection that finds the database locked (by another program: a backup, the admin CLI) waits
        // up to this long for it rather than failing at once. Said here rather than left to sqlx's default.
        let pool_options = match dialect {
            Dialect::Sqlite => pool_options.after_connect(|conn, _| {
                Box::pin(async move {
                    sqlx::query(sqlx::AssertSqlSafe(format!("PRAGMA busy_timeout = {}", SQLITE_BUSY_TIMEOUT.as_millis()))).execute(conn).await?;
                    Ok(())
                })
            }),
            Dialect::Postgres => pool_options,
        };
        let pool = pool_options.connect_with(options).await?;
        if dialect == Dialect::Sqlite && !url.contains(":memory:") {
            sqlx::query("PRAGMA journal_mode = WAL").execute(&pool).await?;
        }
        schema::migrate(&pool, dialect).await?;
        Ok(Self { pool, dialect, write_lock: Arc::new(Mutex::new(())), stored_cap: None })
    }

    /// The store with a cap on the bytes one space (one user's) stores: the stored size of every chunk and manifest
    /// it has, each counted once however many files use it. Recording a blob that would go over it is refused with
    /// [`MetaError::QuotaExceeded`]. `None` (the default): no cap. The cap belongs to this handle and its clones.
    pub fn with_stored_bytes_cap(mut self, cap: Option<u64>) -> Self {
        self.stored_cap = cap;
        self
    }

    /// Whether a database connection is in use (a query under way, or a connection on its way back to the pool).
    /// A simulation on a simulated clock (`turmoil`) keeps time still while it is, since SQLite answers from a
    /// thread of its own, outside the simulation: otherwise how far simulated time got before an answer would depend
    /// on the real machine's speed, and a seed wouldn't replay exactly.
    #[cfg(feature = "test-support")]
    pub fn busy(&self) -> bool {
        self.pool.num_idle() < self.pool.size() as usize
    }

    /// Starts a write transaction. On SQLite it also holds the write lock until commit or drop.
    async fn begin_write(&self) -> Result<WriteTx, MetaError> {
        let guard = match self.dialect {
            Dialect::Sqlite => Some(self.write_lock.clone().lock_owned().await),
            Dialect::Postgres => None,
        };
        // SQLite: take the write lock up front, so a transaction never starts as a reader and then fails to
        // upgrade ("database is locked") when another connection has written in between.
        let tx = match self.dialect {
            Dialect::Sqlite => self.pool.begin_with("BEGIN IMMEDIATE").await?,
            Dialect::Postgres => self.pool.begin().await?,
        };
        Ok(WriteTx { tx, _guard: guard })
    }
}

/// A write transaction; dropping it without [`WriteTx::commit`] rolls back.
struct WriteTx {
    tx: sqlx::Transaction<'static, sqlx::Any>,
    _guard: Option<OwnedMutexGuard<()>>,
}

impl WriteTx {
    async fn commit(self) -> Result<(), MetaError> {
        self.tx.commit().await?;
        Ok(())
    }
}
