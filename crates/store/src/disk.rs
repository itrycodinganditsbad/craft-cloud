//! Local-disk backend: one file per object under a root directory.
//!
//! Writes go to a temporary file in the target directory, are flushed to disk, then renamed over the
//! target, and (on Unix) the directory is flushed too. Directories created for a write are flushed into
//! their parents as well, so a crash can't lose the entry that points at a new shard directory.
//! A crash leaves the old object or the new one.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use bytes::Bytes;

use crate::{Backend, StoreError};

pub struct DiskBackend {
    root: PathBuf,
}

impl DiskBackend {
    /// Uses `root` (created if missing) as the top directory.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn full_path(&self, path: &str) -> PathBuf {
        self.root.join(path)
    }
}

fn write_atomic(target: &Path, data: &[u8]) -> io::Result<()> {
    let dir = target.parent().ok_or_else(|| io::Error::other("object path has no directory"))?;
    // Directories this write will create, deepest first. Their entries in the parents must be flushed too.
    let created: Vec<&Path> = dir.ancestors().take_while(|p| !p.as_os_str().is_empty() && !p.exists()).collect();
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".tmp-{}", uuid::Uuid::now_v7()));
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
        let target = self.full_path(path);
        blocking(move || match std::fs::read(&target) {
            Ok(bytes) => Ok(Some(Bytes::from(bytes))),
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
}
