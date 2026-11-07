//! Content-addressed blob storage for the Craft Cloud server: chunks and manifests.
//!
//! [`Store`] checks every blob on the way in and on the way out: the BLAKE3 of its bytes must equal its ID,
//! and its size must be within the protocol limits. Backends only move bytes: [`disk`] (atomic writes with
//! fsync), [`s3`] (any S3-compatible service) and [`memory`] (tests).
//!
//! Each stored object is one format byte followed by the blob. Format 0 is plain bytes; format 1 is one zstd
//! stream of the blob, written when [`Compression::Zstd`] makes it at least 5% smaller. Other values are reserved
//! (encryption) and are refused. A blob's ID is always the BLAKE3 of its plain bytes, so how it is stored never
//! touches deduplication, and both formats are read whatever [`Compression`] says.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod disk;
pub mod memory;
pub mod s3;

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
pub use craft_cloud_proto::BlobKind;
use craft_cloud_proto::limits::{MAX_CHUNK_BYTES, MAX_MANIFEST_BYTES};
use craft_cloud_proto::{ChunkId, ManifestId};

/// Format byte for plain, uncompressed, unencrypted blobs.
const FORMAT_PLAIN: u8 = 0;

/// Format byte for blobs stored as one zstd stream of their plain bytes.
const FORMAT_ZSTD: u8 = 1;

/// zstd's own default level: fast enough not to slow uploads down, and most of what higher levels gain.
const ZSTD_LEVEL: i32 = 3;

/// Blobs shorter than this are stored plain without trying: the few bytes compression could save aren't worth it.
const MIN_COMPRESS_LEN: usize = 4 * 1024;

/// Compressing a blob from this size up runs on a blocking thread, off the async runtime.
const BLOCKING_LEN: usize = 64 * 1024;

/// The largest zstd window a stored stream may ask for when it is read (2^23 bytes = 8 MiB). Level 3 never uses more
/// than 2 MiB, and blobs are at most 4 MiB, so this only bounds what a damaged or planted object can make the
/// decoder allocate.
const ZSTD_WINDOW_LOG_MAX: u32 = 23;

/// How [`Store::put`] writes blobs. Reading understands every format either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Compression {
    /// zstd (level 3) for blobs of 4 KiB and up, kept only when it saves at least 5%; plain otherwise.
    #[default]
    Zstd,
    /// Always plain: what a server before compression can read.
    None,
}

/// The folder (or key prefix) a kind of blob lives under: chunks and manifests live in separate namespaces.
fn dir(kind: BlobKind) -> &'static str {
    match kind {
        BlobKind::Chunk => "chunks",
        BlobKind::Manifest => "manifests",
    }
}

/// The largest blob of a kind.
fn max_len(kind: BlobKind) -> usize {
    match kind {
        BlobKind::Chunk => usize::try_from(MAX_CHUNK_BYTES).unwrap_or(usize::MAX),
        BlobKind::Manifest => MAX_MANIFEST_BYTES,
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
        format!("{}/{a}/{b}/{hex}", dir(self.kind))
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
    compression: Compression,
}

impl Store {
    /// A store that compresses with zstd ([`Compression::Zstd`]).
    pub fn new(backend: impl Backend) -> Self {
        Self { backend: Arc::new(backend), compression: Compression::default() }
    }

    /// The same store, writing blobs as `compression` says.
    pub fn with_compression(mut self, compression: Compression) -> Self {
        self.compression = compression;
        self
    }

    /// Stores `data` under `key` after checking its size and hash. Storing a blob that is already there
    /// succeeds and changes nothing.
    pub async fn put(&self, key: BlobKey, data: &[u8]) -> Result<(), StoreError> {
        let max = max_len(key.kind);
        if data.len() > max {
            return Err(StoreError::TooLarge { key, len: data.len(), max });
        }
        if *blake3::hash(data).as_bytes() != key.id {
            return Err(StoreError::HashMismatch(key));
        }
        let object = match self.compression {
            Compression::Zstd if data.len() >= BLOCKING_LEN => {
                let owned = data.to_vec();
                tokio::task::spawn_blocking(move || encode_zstd(&owned).unwrap_or_else(|| plain(&owned)))
                    .await
                    .map_err(|e| StoreError::Backend(format!("compression task failed: {e}")))?
            }
            Compression::Zstd if data.len() >= MIN_COMPRESS_LEN => encode_zstd(data).unwrap_or_else(|| plain(data)),
            Compression::Zstd | Compression::None => plain(data),
        };
        self.backend.put_raw(&key.path(), Bytes::from(object)).await
    }

    /// Reads a blob and checks it. A blob whose bytes no longer match its ID is an error, never served.
    pub async fn get(&self, key: BlobKey) -> Result<Bytes, StoreError> {
        let object = self.backend.get_raw(&key.path()).await?.ok_or(StoreError::NotFound(key))?;
        let Some((&format, _)) = object.split_first() else {
            return Err(StoreError::Corrupt { key, reason: "empty object" });
        };
        let data = match format {
            FORMAT_PLAIN => object.slice(1..),
            // Always on a blocking thread: a small stream can stand for a blob of up to 4 MiB.
            FORMAT_ZSTD => tokio::task::spawn_blocking(move || decode_zstd(key, object.get(1..).unwrap_or_default()))
                .await
                .map_err(|e| StoreError::Backend(format!("decompression task failed: {e}")))??,
            _ => return Err(StoreError::Corrupt { key, reason: "unknown format byte" }),
        };
        if data.len() > max_len(key.kind) {
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

/// The object for `data` stored plain.
fn plain(data: &[u8]) -> Vec<u8> {
    let mut object = Vec::with_capacity(data.len().saturating_add(1));
    object.push(FORMAT_PLAIN);
    object.extend_from_slice(data);
    object
}

/// The object for `data` compressed with zstd, or `None` when that doesn't save at least 5% (or zstd fails, which
/// leaves the plain object as a safe answer).
fn encode_zstd(data: &[u8]) -> Option<Vec<u8>> {
    let mut object = Vec::with_capacity(data.len() / 2 + 64);
    object.push(FORMAT_ZSTD);
    zstd::stream::copy_encode(data, &mut object, ZSTD_LEVEL).ok()?;
    let compressed = object.len().saturating_sub(1) as u64;
    // At most 95% of the original: compressed * 20 <= len * 19, in u64 so it can't overflow.
    (compressed.saturating_mul(20) <= (data.len() as u64).saturating_mul(19)).then_some(object)
}

/// A zstd stream's plain bytes, refusing to produce more than the largest blob of the key's kind, so a damaged or
/// planted object can't fill memory.
fn decode_zstd(key: BlobKey, compressed: &[u8]) -> Result<Bytes, StoreError> {
    use std::io::Read;
    let max = max_len(key.kind);
    let damaged = |_| StoreError::Corrupt { key, reason: "undecodable compressed data" };
    let mut decoder = zstd::stream::read::Decoder::with_buffer(compressed).map_err(damaged)?;
    decoder.window_log_max(ZSTD_WINDOW_LOG_MAX).map_err(damaged)?;
    let mut data = Vec::new();
    decoder.take(max as u64 + 1).read_to_end(&mut data).map_err(damaged)?;
    if data.len() > max {
        return Err(StoreError::Corrupt { key, reason: "larger than the limit" });
    }
    // `read_to_end` grows the buffer by doubling; don't hold the spare half for as long as the blob is served.
    data.shrink_to_fit();
    Ok(Bytes::from(data))
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
