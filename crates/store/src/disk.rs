//! Local-disk backend: one file per object under a root directory.
//!
//! Writes go to a temporary file in the target directory, are flushed to disk, then renamed over the
//! target, and (on Unix) the directory is flushed too. Directories created for a write are flushed into
//! their parents as well, so a crash can't lose the entry that points at a new shard directory.
//! A crash leaves the old object or the new one.

use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;

use crate::{Backend, StoreError};

pub struct DiskBackend {
    root: PathBuf,
    /// How many directories listings have opened, for tests that check a page doesn't walk finished directories again.
    dirs_read: Arc<AtomicUsize>,
}

impl DiskBackend {
    /// Uses `root` (created if missing) as the top directory.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root, dirs_read: Arc::new(AtomicUsize::new(0)) })
    }

    fn full_path(&self, path: &str) -> PathBuf {
        self.root.join(path)
    }

    /// The number of directories listings have read so far. Only meant for tests of the listing's cost.
    #[doc(hidden)]
    pub fn directories_read(&self) -> usize {
        self.dirs_read.load(Ordering::Relaxed)
    }
}

/// The name of a write's temporary file: `.tmp-` and a hyphenated lowercase UUIDv7.
fn temp_name(id: uuid::Uuid) -> String {
    format!("{TEMP_PREFIX}{}", id.hyphenated())
}

const TEMP_PREFIX: &str = ".tmp-";

/// Whether `name` is exactly what [`temp_name`] writes. Any other `.tmp-` name is somebody else's file and
/// is listed as foreign.
fn is_temp_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(TEMP_PREFIX) else {
        return false;
    };
    uuid::Uuid::try_parse(rest).is_ok_and(|id| id.get_version() == Some(uuid::Version::SortRand) && id.hyphenated().to_string() == rest)
}

fn write_atomic(target: &Path, data: &[u8]) -> io::Result<()> {
    let dir = target.parent().ok_or_else(|| io::Error::other("object path has no directory"))?;
    // Directories this write will create, deepest first. Their entries in the parents must be flushed too.
    let created: Vec<&Path> = dir.ancestors().take_while(|p| !p.as_os_str().is_empty() && !p.exists()).collect();
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(temp_name(uuid::Uuid::now_v7()));
    let result = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, target)?;
        sync_dir(dir)?;
        for created_dir in &created {
            if let Some(parent) = created_dir.parent().filter(|p| !p.as_os_str().is_empty()) {
                sync_dir(parent)?;
            }
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    // Windows can't open a directory as a file; NTFS journals the rename itself.
    Ok(())
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> io::Result<T> + Send + 'static) -> Result<T, StoreError> {
    tokio::task::spawn_blocking(f).await.map_err(|e| StoreError::Backend(format!("disk task failed: {e}")))?.map_err(StoreError::Io)
}

#[async_trait]
impl Backend for DiskBackend {
    async fn put_raw(&self, path: &str, data: Bytes) -> Result<(), StoreError> {
        let target = self.full_path(path);
        blocking(move || write_atomic(&target, &data)).await
    }

    async fn get_raw(&self, path: &str) -> Result<Option<Bytes>, StoreError> {
        self.get_raw_limited(path, usize::MAX).await
    }

    async fn get_raw_limited(&self, path: &str, max_bytes: usize) -> Result<Option<Bytes>, StoreError> {
        let target = self.full_path(path);
        blocking(move || match std::fs::File::open(&target) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(u64::try_from(max_bytes.saturating_add(1)).unwrap_or(u64::MAX)).read_to_end(&mut bytes)?;
                Ok(Some(Bytes::from(bytes)))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        })
        .await
    }

    async fn size_raw(&self, path: &str) -> Result<Option<u64>, StoreError> {
        let target = self.full_path(path);
        blocking(move || match std::fs::symlink_metadata(&target) {
            Ok(meta) if meta.file_type().is_file() => Ok(Some(meta.len())),
            Ok(_) => Ok(None),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        })
        .await
    }

    async fn delete_raw(&self, path: &str) -> Result<(), StoreError> {
        let target = self.full_path(path);
        blocking(move || match std::fs::remove_file(&target) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        })
        .await
    }

    async fn list_raw(&self, prefix: &str, after: Option<&str>, limit: usize) -> Result<Vec<String>, StoreError> {
        if Path::new(prefix).is_absolute() || Path::new(prefix).components().any(|part| !matches!(part, Component::Normal(_))) {
            return Err(StoreError::Backend("listing prefix must be a relative path".into()));
        }
        let root = self.root.clone();
        let prefix = prefix.to_owned();
        let after = after.map(str::to_owned);
        let dirs_read = Arc::clone(&self.dirs_read);
        blocking(move || {
            let Some(start) = list_prefix_dir(&root, &prefix)? else {
                return Ok(Vec::new());
            };
            let mut out = Vec::new();
            walk_sorted(&start, &prefix, after.as_deref(), limit, &dirs_read, &mut out)?;
            Ok(out)
        })
        .await
    }
}

fn list_prefix_dir(root: &Path, prefix: &str) -> io::Result<Option<PathBuf>> {
    let mut current = root.to_path_buf();
    for component in Path::new(prefix).components() {
        let Component::Normal(name) = component else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "listing prefix must be a relative path"));
        };
        current.push(name);
        let metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Ok(None);
        }
    }
    Ok(Some(current))
}

/// Collects up to `limit` file paths below `dir` that sort strictly after `after`, in byte order of the
/// relative path (`/` separators), the order memory and S3 use. `relative` is `dir`'s path relative to the root.
/// Returns `true` once `out` is full.
///
/// One directory's entries are read and sorted at a time, so memory is bounded by the largest single
/// directory listing (a shard directory holds at most 256 entries) plus `limit` paths. A subtree whose paths
/// all sort at or before `after` is skipped without being opened, and the walk stops as soon as `limit` paths
/// are found, so a page costs the directories on the way to `after` and to the end of the page, not the
/// store. A directory sorts as `name/`, so `a.b` comes before everything in `a/`.
fn walk_sorted(dir: &Path, relative: &str, after: Option<&str>, limit: usize, dirs_read: &AtomicUsize, out: &mut Vec<String>) -> io::Result<bool> {
    if out.len() >= limit {
        return Ok(true);
    }
    let metadata = match std::fs::symlink_metadata(dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(false);
    }
    let reader = match std::fs::read_dir(dir) {
        Ok(reader) => reader,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    dirs_read.fetch_add(1, Ordering::Relaxed);
    // (sort key, name, is a directory)
    let mut entries: Vec<(String, String, bool)> = Vec::new();
    for entry in reader {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "store path is not valid UTF-8"))?.to_owned();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            entries.push((format!("{name}/"), name, true));
        } else if file_type.is_file() && !is_temp_name(&name) {
            entries.push((name.clone(), name, false));
        }
    }
    entries.sort_unstable();
    for (_, name, is_dir) in entries {
        let path = if relative.is_empty() { name.clone() } else { format!("{relative}/{name}") };
        if is_dir {
            let subtree = format!("{path}/");
            // Everything below sorts after `subtree` and starts with it; it is all at or before `after`
            // exactly when `after` is greater and is not itself below it.
            if after.is_some_and(|cursor| cursor > subtree.as_str() && !cursor.starts_with(&subtree)) {
                continue;
            }
            if walk_sorted(&dir.join(&name), &path, after, limit, dirs_read, out)? {
                return Ok(true);
            }
        } else if after.is_none_or(|cursor| path.as_str() > cursor) {
            out.push(path);
            if out.len() >= limit {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
