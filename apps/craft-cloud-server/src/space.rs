//! How much room the server has, for the admin's storage overview (`GET /admin/storage`).
//!
//! The numbers are cheap: a filesystem query for the store's directory and for the directory of the SQLite file, and a
//! `stat` of the database files. Nothing scans the stored data, so the overview costs the same on an empty server and
//! a full one. A number the server can't get (a directory that vanished, a filesystem that doesn't say) is left out
//! rather than failing the overview.

use std::path::{Path, PathBuf};

use crate::config::{Config, StorageConfig};

/// Where to measure, and what the admin said the store may hold. Built from the config at start; `Default` knows
/// nothing (an in-memory test server), so every number is `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StorageInfo {
    capacity: Option<u64>,
    /// The disk store's directory. `None` for S3 and memory.
    store_dir: Option<PathBuf>,
    /// The SQLite file. `None` for Postgres and `sqlite::memory:`.
    database_file: Option<PathBuf>,
}

/// What [`StorageInfo::measure`] found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Measured {
    pub store_fs_total: Option<u64>,
    pub store_fs_free: Option<u64>,
    pub server_fs_total: Option<u64>,
    pub server_fs_free: Option<u64>,
    pub database_bytes: Option<u64>,
}

impl StorageInfo {
    pub fn new(capacity: Option<u64>, store_dir: Option<PathBuf>, database_file: Option<PathBuf>) -> Self {
        Self { capacity, store_dir, database_file }
    }

    pub fn from_config(config: &Config) -> Self {
        let store_dir = match &config.storage {
            StorageConfig::Disk { path, .. } => Some(path.clone()),
            StorageConfig::S3 { .. } => None,
        };
        Self::new(config.storage.capacity(), store_dir, sqlite_file(&config.database.url))
    }

    /// What the admin set as `capacity`, in bytes.
    pub fn capacity(&self) -> Option<u64> {
        self.capacity
    }

    /// The disk store's directory; `None` for S3 and memory.
    pub fn store_dir(&self) -> Option<&Path> {
        self.store_dir.as_deref()
    }

    /// Total and free bytes of the filesystem that holds the disk store; `(None, None)` for S3 and memory, or when it
    /// can't be read. Blocking.
    pub fn store_filesystem(&self) -> (Option<u64>, Option<u64>) {
        self.store_dir.as_deref().map_or((None, None), filesystem)
    }

    /// Reads the filesystem numbers and the database's size. Blocking: call it off the async threads.
    pub fn measure(&self) -> Measured {
        let mut measured = Measured::default();
        if let Some(dir) = &self.store_dir {
            (measured.store_fs_total, measured.store_fs_free) = filesystem(dir);
        }
        if let Some(file) = &self.database_file {
            let dir = match file.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent,
                _ => Path::new("."),
            };
            (measured.server_fs_total, measured.server_fs_free) = filesystem(dir);
            measured.database_bytes = sqlite_bytes(file);
        }
        measured
    }
}

/// The SQLite file a database URL names (`sqlite:path`, `sqlite://path`, an optional `?query`); `None` for Postgres
/// and for the in-memory database.
pub fn sqlite_file(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("sqlite:")?;
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    // `mode=memory` makes any name an in-memory database.
    if query.split('&').any(|pair| pair.split_once('=').is_some_and(|(key, value)| key == "mode" && value == "memory")) {
        return None;
    }
    if path.is_empty() || path.starts_with(":memory:") {
        return None;
    }
    // As sqlx reads it: the path is percent-decoded, so a `?` or `#` can be in a file name.
    let decoded = percent_encoding::percent_decode_str(path).decode_utf8().ok()?;
    Some(PathBuf::from(decoded.as_ref()))
}

/// Total and available bytes of the filesystem holding `dir`.
fn filesystem(dir: &Path) -> (Option<u64>, Option<u64>) {
    // Windows answers for a directory that is not there with the numbers of the disk it would be on; a missing store
    // is not a measurement.
    if !dir.is_dir() {
        tracing::debug!(dir = %dir.display(), "the directory to measure is not there");
        return (None, None);
    }
    match fs4::statvfs(dir) {
        Ok(stats) => (Some(stats.total_space()), Some(stats.available_space())),
        Err(e) => {
            tracing::debug!(dir = %dir.display(), error = %e, "can't read the size of a filesystem");
            (None, None)
        }
    }
}

/// The database file's size plus its write-ahead log and shared-memory file when they exist. `None` when the main
/// file can't be read.
fn sqlite_bytes(file: &Path) -> Option<u64> {
    let mut total = std::fs::metadata(file).ok()?.len();
    for suffix in ["-wal", "-shm"] {
        let mut name = file.as_os_str().to_owned();
        name.push(suffix);
        if let Ok(meta) = std::fs::metadata(PathBuf::from(name)) {
            total = total.saturating_add(meta.len());
        }
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sqlite_file_comes_out_of_the_url() {
        assert_eq!(sqlite_file("sqlite:/var/lib/cc/meta.db"), Some(PathBuf::from("/var/lib/cc/meta.db")));
        assert_eq!(sqlite_file("sqlite:meta.db"), Some(PathBuf::from("meta.db")));
        assert_eq!(sqlite_file("sqlite:///var/lib/meta.db?mode=rwc"), Some(PathBuf::from("/var/lib/meta.db")));
        assert_eq!(sqlite_file("sqlite:C:/cc/meta.db"), Some(PathBuf::from("C:/cc/meta.db")));
        assert_eq!(sqlite_file("sqlite:/srv/a%20b/meta.db"), Some(PathBuf::from("/srv/a b/meta.db")));
        assert_eq!(sqlite_file("sqlite:/srv/a%3Fb.db?mode=rwc"), Some(PathBuf::from("/srv/a?b.db")));
        for none in [
            "sqlite:meta.db?mode=memory",
            "sqlite:meta.db?cache=shared&mode=memory",
            "sqlite::memory:",
            "sqlite:",
            "postgres://u:p@host/db",
            "mysql://x",
        ] {
            assert_eq!(sqlite_file(none), None, "{none}");
        }
    }

    #[test]
    fn the_database_size_counts_the_wal_and_shm_files() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("meta.db");
        assert_eq!(sqlite_bytes(&db), None, "no file");
        std::fs::write(&db, [0u8; 100]).unwrap();
        assert_eq!(sqlite_bytes(&db), Some(100));
        std::fs::write(dir.path().join("meta.db-wal"), [0u8; 20]).unwrap();
        std::fs::write(dir.path().join("meta.db-shm"), [0u8; 3]).unwrap();
        assert_eq!(sqlite_bytes(&db), Some(123));
    }

    #[test]
    fn a_disk_store_and_a_sqlite_file_have_filesystem_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("meta.db");
        std::fs::write(&db, [0u8; 10]).unwrap();
        let measured = StorageInfo::new(Some(7), Some(dir.path().to_owned()), Some(db)).measure();
        let (total, free) = (measured.store_fs_total.unwrap(), measured.store_fs_free.unwrap());
        assert!(total > 0 && free <= total, "{total} {free}");
        assert_eq!((measured.server_fs_total, measured.database_bytes), (Some(total), Some(10)), "same directory, same filesystem");
        assert!(measured.server_fs_free.is_some());
    }

    #[test]
    fn what_is_not_there_is_left_out() {
        assert_eq!(StorageInfo::default().measure(), Measured::default());
        let gone = tempfile::tempdir().unwrap().path().join("not-here");
        let measured = StorageInfo::new(None, Some(gone.clone()), Some(gone.join("meta.db"))).measure();
        assert_eq!(measured, Measured::default());
    }
}
