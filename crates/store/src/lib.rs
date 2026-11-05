//! Content-addressed blob storage for the Craft Cloud server: chunks and manifests.
//!
//! [`Store`] checks every blob on the way in and on the way out: the BLAKE3 of its bytes must equal its ID,
//! and its size must be within the protocol limits. Backends only move bytes: [`disk`] (atomic writes with
//! fsync), [`s3`] (any S3-compatible service) and [`memory`] (tests).
//!
//! Each stored object is one format byte followed by the blob. Format 0 is plain bytes; other values are
//! reserved for compression or encryption and are refused today.
//!
//! [`Store::list`] returns bounded, ordered pages scoped to one blob namespace. Foreign object paths remain
//! visible in the page. S3-compatible backends may return incomplete or stale listings, so listing is not a
//! proof that the namespace contains every object. Disk listing reports an I/O error for non-UTF-8 names,
//! which the string path API cannot represent. Disk and memory list in byte order of the relative path and
//! resume from the cursor without visiting what lies before it; a disk page reads one directory at a time. On
//! S3 a page reads the listing only up to its limit, and a key that `object_store` cannot represent (an empty
//! path segment, as in `chunks//a`) ends the listing with an error instead of appearing as foreign; a key
//! with a trailing slash is reported without the slash. Cursors longer than any real path are refused.
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
    /// Lists paths below `prefix`, in ascending order, strictly after `after`.
    async fn list_raw(&self, prefix: &str, after: Option<&str>, limit: usize) -> Result<Vec<String>, StoreError>;
}

/// The longest object path a listing cursor can carry. S3 keys are at most 1024 bytes and Linux paths at most
/// 4096, so no object the backends can hold needs more.
const MAX_CURSOR_PATH_BYTES: usize = 4096;
/// The longest valid token: the `1:c:` header plus two hex digits per path byte.
const MAX_CURSOR_TOKEN_BYTES: usize = 4 + 2 * MAX_CURSOR_PATH_BYTES;

/// Opaque continuation token returned by [`Store::list`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListCursor(String);

impl ListCursor {
    /// Reconstructs a cursor token saved from a previous page.
    pub fn from_token(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// Returns this cursor's opaque token for storage or transport.
    pub fn as_token(&self) -> &str {
        &self.0
    }

    fn new(kind: BlobKind, path: &str) -> Result<Self, StoreError> {
        if path.len() > MAX_CURSOR_PATH_BYTES {
            return Err(StoreError::Backend("object path is too long for a listing cursor".into()));
        }
        let kind = match kind {
            BlobKind::Chunk => "c",
            BlobKind::Manifest => "m",
        };
        let encoded = path.as_bytes().iter().map(|byte| format!("{byte:02x}")).collect::<String>();
        Ok(Self(format!("1:{kind}:{encoded}")))
    }

    fn decode(&self, kind: BlobKind) -> Result<String, StoreError> {
        let invalid = || StoreError::Backend("invalid listing cursor".into());
        // Checked before anything is split, allocated or decoded: a token is untrusted input.
        if self.0.len() > MAX_CURSOR_TOKEN_BYTES {
            return Err(invalid());
        }
        let mut parts = self.0.splitn(3, ':');
        let version = parts.next().ok_or_else(invalid)?;
        let cursor_kind = parts.next().ok_or_else(invalid)?;
        let encoded = parts.next().ok_or_else(invalid)?;
        let expected = match kind {
            BlobKind::Chunk => "c",
            BlobKind::Manifest => "m",
        };
        if version != "1" || cursor_kind != expected || encoded.is_empty() || encoded.len() % 2 != 0 {
            return Err(invalid());
        }
        let mut bytes = Vec::with_capacity(encoded.len() / 2);
        let mut index = 0;
        while index < encoded.len() {
            let end = index.checked_add(2).ok_or_else(invalid)?;
            let pair = encoded.as_bytes().get(index..end).ok_or_else(invalid)?;
            let hex = std::str::from_utf8(pair).map_err(|_| invalid())?;
            bytes.push(u8::from_str_radix(hex, 16).map_err(|_| invalid())?);
            index = end;
        }
        let path = String::from_utf8(bytes).map_err(|_| invalid())?;
        if !path.starts_with(&format!("{}/", dir(kind))) {
            return Err(invalid());
        }
        Ok(path)
    }
}

/// One bounded page. Invalid object names appear in `foreign`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListPage {
    pub blobs: Vec<BlobKey>,
    pub foreign: Vec<String>,
    pub next_cursor: Option<ListCursor>,
}

pub const MAX_LIST_PAGE_SIZE: usize = 1000;

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
        let max = max_len(key.kind);
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
        self.get_sized(key).await.map(|(data, _)| data)
    }

    /// Like [`Store::get`], and also says how many bytes the object is stored as (the format byte included), which is
    /// what the metadata records as the blob's stored size.
    pub async fn get_sized(&self, key: BlobKey) -> Result<(Bytes, u64), StoreError> {
        let object = self.backend.get_raw(&key.path()).await?.ok_or(StoreError::NotFound(key))?;
        let Some((&format, _)) = object.split_first() else {
            return Err(StoreError::Corrupt { key, reason: "empty object" });
        };
        if format != FORMAT_PLAIN {
            return Err(StoreError::Corrupt { key, reason: "unknown format byte" });
        }
        let stored = u64::try_from(object.len()).unwrap_or(u64::MAX);
        let data = object.slice(1..);
        if data.len() > max_len(key.kind) {
            return Err(StoreError::Corrupt { key, reason: "larger than the limit" });
        }
        if *blake3::hash(&data).as_bytes() != key.id {
            return Err(StoreError::Corrupt { key, reason: "hash mismatch" });
        }
        Ok((data, stored))
    }

    pub async fn delete(&self, key: BlobKey) -> Result<(), StoreError> {
        self.backend.delete_raw(&key.path()).await
    }

    /// Lists one kind beneath its namespace. S3-compatible listings can be incomplete or stale.
    pub async fn list(&self, kind: BlobKind, cursor: Option<ListCursor>, limit: usize) -> Result<ListPage, StoreError> {
        if limit == 0 {
            return Err(StoreError::Backend("listing limit must be greater than zero".into()));
        }
        let limit = limit.min(MAX_LIST_PAGE_SIZE);
        let after = cursor.as_ref().map(|cursor| cursor.decode(kind)).transpose()?;
        if let Some(path) = &after
            && !path.starts_with(&format!("{}/", dir(kind)))
        {
            return Err(StoreError::Backend("invalid listing cursor".into()));
        }
        let request_limit = limit.checked_add(1).ok_or_else(|| StoreError::Backend("listing limit is too large".into()))?;
        let mut paths = self.backend.list_raw(dir(kind), after.as_deref(), request_limit).await?;
        paths.sort();
        paths.dedup();
        if paths.iter().any(|path| !path.starts_with(&format!("{}/", dir(kind)))) {
            return Err(StoreError::Backend("backend returned object outside listing prefix".into()));
        }
        let has_more = paths.len() > limit;
        paths.truncate(limit);
        let next_cursor = if has_more { paths.last().map(|path| ListCursor::new(kind, path)).transpose()? } else { None };
        let mut blobs = Vec::new();
        let mut foreign = Vec::new();
        for path in paths {
            match parse_key(kind, &path) {
                Some(key) => blobs.push(key),
                None => foreign.push(path),
            }
        }
        Ok(ListPage { blobs, foreign, next_cursor })
    }
}

fn parse_key(kind: BlobKind, path: &str) -> Option<BlobKey> {
    let mut parts = path.split('/');
    if parts.next()? != dir(kind) {
        return None;
    }
    let first = parts.next()?;
    let second = parts.next()?;
    let hex = parts.next()?;
    if parts.next().is_some()
        || first.len() != 2
        || second.len() != 2
        || hex.len() != 64
        || !hex.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        || &hex[..2] != first
        || &hex[2..4] != second
    {
        return None;
    }
    let mut id = [0u8; 32];
    for (i, byte) in id.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(BlobKey { kind, id })
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
