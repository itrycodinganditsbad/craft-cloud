//! Content-addressed blob storage for the Craft Cloud server: chunks and manifests.
//!
//! [`Store`] checks every blob on the way in and on the way out: the BLAKE3 of its bytes must equal its ID,
//! and its size must be within the protocol limits. Backends only move bytes: [`disk`] (atomic writes with
//! fsync), [`s3`] (any S3-compatible service) and [`memory`] (tests).
//!
//! Each stored object is one format byte followed by the blob. Format 0 is plain bytes; other values are
//! reserved for compression or encryption and are refused today.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod disk;
pub mod memory;
pub mod s3;

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use craft_cloud_proto::limits::{MAX_CHUNK_BYTES, MAX_MANIFEST_BYTES};
use craft_cloud_proto::{ChunkId, ManifestId};

/// Format byte for plain, uncompressed, unencrypted blobs.
const FORMAT_PLAIN: u8 = 0;

/// What a blob is. Chunks and manifests live in separate namespaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlobKind {
    Chunk,
    Manifest,
}

impl BlobKind {
    fn dir(self) -> &'static str {
        match self {
            BlobKind::Chunk => "chunks",
            BlobKind::Manifest => "manifests",
        }
    }

    fn max_len(self) -> usize {
        match self {
            BlobKind::Chunk => usize::try_from(MAX_CHUNK_BYTES).unwrap_or(usize::MAX),
            BlobKind::Manifest => MAX_MANIFEST_BYTES,
        }
    }
}

/// Names one blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlobKey {
    pub kind: BlobKind,
    pub id: [u8; 32],
}

impl BlobKey {
    pub fn chunk(id: &ChunkId) -> Self {
        Self { kind: BlobKind::Chunk, id: *id.as_bytes() }
    }

    pub fn manifest(id: &ManifestId) -> Self {
        Self { kind: BlobKind::Manifest, id: *id.as_bytes() }
    }

    /// Relative object path, sharded by the first two bytes: `chunks/ab/cd/abcd…`.
    pub fn path(&self) -> String {
        let hex: String = self.id.iter().map(|b| format!("{b:02x}")).collect();
        let (a, rest) = hex.split_at(2);
        let (b, _) = rest.split_at(2);
        format!("{}/{a}/{b}/{hex}", self.kind.dir())
    }
}

impl std::fmt::Display for BlobKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.path())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{0} not found")]
    NotFound(BlobKey),
    #[error("the bytes don't match the ID {0}")]
    HashMismatch(BlobKey),
    #[error("{key} is {len} bytes; the limit is {max}")]
    TooLarge { key: BlobKey, len: usize, max: usize },
    #[error("{key} is damaged: {reason}")]
    Corrupt { key: BlobKey, reason: &'static str },
    #[error("storage I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("storage backend failed: {0}")]
    Backend(String),
}

/// Moves raw bytes. Implementations don't check anything; [`Store`] does.
#[async_trait]
pub trait Backend: Send + Sync + 'static {
    /// Writes the object atomically: a reader sees the old object, the new one, or none, never a mix.
    async fn put_raw(&self, path: &str, data: Bytes) -> Result<(), StoreError>;
    /// `None` when there is no such object.
    async fn get_raw(&self, path: &str) -> Result<Option<Bytes>, StoreError>;
    /// Removing a missing object succeeds.
    async fn delete_raw(&self, path: &str) -> Result<(), StoreError>;
}

/// The blob store the server uses. Cheap to clone.
#[derive(Clone)]
pub struct Store {
    backend: Arc<dyn Backend>,
}

impl Store {
    pub fn new(backend: impl Backend) -> Self {
        Self { backend: Arc::new(backend) }
    }

    /// Stores `data` under `key` after checking its size and hash. Storing a blob that is already there
    /// succeeds and changes nothing.
    pub async fn put(&self, key: BlobKey, data: &[u8]) -> Result<(), StoreError> {
        let max = key.kind.max_len();
        if data.len() > max {
            return Err(StoreError::TooLarge { key, len: data.len(), max });
        }
        if *blake3::hash(data).as_bytes() != key.id {
            return Err(StoreError::HashMismatch(key));
        }
        let mut object = Vec::with_capacity(data.len().saturating_add(1));
        object.push(FORMAT_PLAIN);
        object.extend_from_slice(data);
        self.backend.put_raw(&key.path(), Bytes::from(object)).await
    }

    /// Reads a blob and checks it. A blob whose bytes no longer match its ID is an error, never served.
    pub async fn get(&self, key: BlobKey) -> Result<Bytes, StoreError> {
        let object = self.backend.get_raw(&key.path()).await?.ok_or(StoreError::NotFound(key))?;
        let Some((&format, _)) = object.split_first() else {
            return Err(StoreError::Corrupt { key, reason: "empty object" });
        };
        if format != FORMAT_PLAIN {
            return Err(StoreError::Corrupt { key, reason: "unknown format byte" });
        }
        let data = object.slice(1..);
        if data.len() > key.kind.max_len() {
            return Err(StoreError::Corrupt { key, reason: "larger than the limit" });
        }
        if *blake3::hash(&data).as_bytes() != key.id {
            return Err(StoreError::Corrupt { key, reason: "hash mismatch" });
        }
        Ok(data)
    }

    pub async fn delete(&self, key: BlobKey) -> Result<(), StoreError> {
        self.backend.delete_raw(&key.path()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_sharded_by_the_first_two_bytes() {
        let mut id = [0u8; 32];
        id[0] = 0xab;
        id[1] = 0xcd;
        let key = BlobKey { kind: BlobKind::Chunk, id };
        assert_eq!(key.path(), format!("chunks/ab/cd/abcd{}", "00".repeat(30)));
        assert!(BlobKey { kind: BlobKind::Manifest, id }.path().starts_with("manifests/ab/cd/"));
    }
}
