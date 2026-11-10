//! Content-addressed blob storage for the Craft Cloud server: chunks and manifests.
//!
//! [`Store`] checks every blob on the way in and on the way out: the BLAKE3 of its bytes must equal its ID,
//! and its size must be within the protocol limits. Backends only move bytes: [`disk`] (atomic writes with
//! fsync), [`s3`] (any S3-compatible service) and [`memory`] (tests).
//!
//! Each stored object is one format byte followed by the blob. Format 0 is plain bytes; format 1 is one zstd
//! stream of the blob, written when [`Compression::Zstd`] makes it at least 5% smaller (the level and the 5% are
//! settable). Other values are reserved (encryption) and are refused. A blob's ID is always the BLAKE3 of its plain
//! bytes, so how it is stored never touches deduplication, and both formats are read whatever [`Compression`] says.
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
use std::sync::atomic::{AtomicU8, Ordering};

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
pub const DEFAULT_ZSTD_LEVEL: u8 = 3;

/// The lowest level a store writes.
pub const MIN_ZSTD_LEVEL: u8 = 1;

/// The highest level a store writes. 19 is the highest zstd level whose window (8 MiB) the reader's cap
/// ([`ZSTD_WINDOW_LOG_MAX`]) allows; the "ultra" levels 20 to 22 ask for windows of 32 to 128 MiB, which a stored
/// object may not use. A longer window or long-distance matching would gain nothing either: blobs are at most 4 MiB
/// and every blob is compressed on its own, so level 19's window already spans a whole blob.
pub const MAX_ZSTD_LEVEL: u8 = 19;

/// By default a compressed blob is kept only when it is at least this many percent smaller than the plain one.
pub const DEFAULT_MIN_SAVING_PERCENT: u8 = 5;

/// [`Store::recompress`] replaces an object only when the new one is at least this many percent smaller than the
/// stored one: a rewrite that gains a fraction of a percent costs a full write for nothing.
pub const RECOMPRESS_MIN_GAIN_PERCENT: u64 = 1;

/// Blobs shorter than this are stored plain without trying: the few bytes compression could save aren't worth it.
pub const MIN_COMPRESS_LEN: usize = 4 * 1024;

/// Compressing a blob from this size up runs on a blocking thread, off the async runtime.
const BLOCKING_LEN: usize = 64 * 1024;

/// The largest zstd window a stored stream may ask for when it is read (2^23 bytes = 8 MiB). Level 3 never uses more
/// than 2 MiB and level 19 (the highest a store writes) 8 MiB, and blobs are at most 4 MiB, so this only bounds what
/// a damaged or planted object can make the decoder allocate.
const ZSTD_WINDOW_LOG_MAX: u32 = 23;

/// How [`Store::put`] writes blobs. Reading understands every format either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Compression {
    /// zstd (level 3 unless set) for blobs of 4 KiB and up, kept only when it saves at least 5% (unless set); plain
    /// otherwise.
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
    /// What kind of storage this is, for the admin's server overview (`disk`, `s3` or `memory`). A backend that
    /// doesn't say is `other`. Never a path, an address or a setting.
    fn kind(&self) -> &'static str {
        "other"
    }
    /// Writes the object atomically: a reader sees the old object, the new one, or none, never a mix.
    async fn put_raw(&self, path: &str, data: Bytes) -> Result<(), StoreError>;
    /// `None` when there is no such object.
    async fn get_raw(&self, path: &str) -> Result<Option<Bytes>, StoreError>;
    /// Reads at most `max_bytes` plus one byte. Backends should override this for bounded I/O.
    async fn get_raw_limited(&self, path: &str, max_bytes: usize) -> Result<Option<Bytes>, StoreError> {
        Ok(self.get_raw(path).await?.map(|bytes| bytes.slice(..bytes.len().min(max_bytes.saturating_add(1)))))
    }
    /// Stored object size without reading object contents.
    async fn size_raw(&self, _path: &str) -> Result<Option<u64>, StoreError> {
        Err(StoreError::Backend("backend does not support metadata-only object sizes".into()))
    }
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

/// What [`Store::put`] wrote: how big the object is on the backend (format byte included), and the zstd level it
/// was compressed or tried at (`0`: not tried, because compression is off or the blob is under 4 KiB). A blob that
/// was tried but kept plain still names the level, so a later pass at that level or lower doesn't try again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Written {
    pub physical_len: u64,
    pub level: u8,
}

/// What [`Store::recompress`] found: the object's size before and after (equal when nothing changes), and whether
/// the object was replaced. With `write` off, `rewritten` is `false` and `after` is what a rewrite would have made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recompressed {
    pub before: u64,
    pub after: u64,
    pub rewritten: bool,
}

/// One blob compressed at one level, for a measurement: the plain size, the size it would be stored at (the plain
/// size plus the format byte when compression doesn't pay), and how long compressing took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LevelSample {
    pub level: u8,
    pub plain_len: u64,
    pub stored_len: u64,
    pub nanos: u64,
}

/// The blob store the server uses. Cheap to clone; clones share the level, so changing it takes effect everywhere.
#[derive(Clone)]
pub struct Store {
    backend: Arc<dyn Backend>,
    compression: Compression,
    level: Arc<AtomicU8>,
    min_saving: u8,
}

impl Store {
    /// A store that compresses with zstd ([`Compression::Zstd`]) at [`DEFAULT_ZSTD_LEVEL`].
    pub fn new(backend: impl Backend) -> Self {
        Self {
            backend: Arc::new(backend),
            compression: Compression::default(),
            level: Arc::new(AtomicU8::new(DEFAULT_ZSTD_LEVEL)),
            min_saving: DEFAULT_MIN_SAVING_PERCENT,
        }
    }

    /// The same store, writing blobs as `compression` says.
    pub fn with_compression(mut self, compression: Compression) -> Self {
        self.compression = compression;
        self
    }

    /// The same store, compressing new writes at `level` (clamped to [`MIN_ZSTD_LEVEL`] to [`MAX_ZSTD_LEVEL`]).
    pub fn with_level(self, level: u8) -> Self {
        self.set_level(level);
        self
    }

    /// The same store, keeping a compressed blob only when it is at least `percent` smaller (clamped to 1 to 90).
    pub fn with_min_saving(mut self, percent: u8) -> Self {
        self.min_saving = percent.clamp(1, 90);
        self
    }

    /// Changes the level of new writes, for this store and every clone of it. Reading is unaffected.
    pub fn set_level(&self, level: u8) {
        self.level.store(level.clamp(MIN_ZSTD_LEVEL, MAX_ZSTD_LEVEL), Ordering::Relaxed);
    }

    /// The level new writes are compressed at (when [`Store::compression`] is zstd).
    pub fn level(&self) -> u8 {
        self.level.load(Ordering::Relaxed)
    }

    pub fn compression(&self) -> Compression {
        self.compression
    }

    /// How much smaller (in percent) a compressed blob must be to be kept.
    pub fn min_saving(&self) -> u8 {
        self.min_saving
    }

    /// The backend's kind (`disk`, `s3`, `memory` or `other`).
    pub fn kind(&self) -> &'static str {
        self.backend.kind()
    }

    /// Stores `data` under `key` after checking its size and hash. Storing a blob that is already there
    /// succeeds and writes the object again, in whatever format the store now writes. Returns what was written.
    pub async fn put(&self, key: BlobKey, data: &[u8]) -> Result<Written, StoreError> {
        let max = max_len(key.kind);
        if data.len() > max {
            return Err(StoreError::TooLarge { key, len: data.len(), max });
        }
        if *blake3::hash(data).as_bytes() != key.id {
            return Err(StoreError::HashMismatch(key));
        }
        let (level, min_saving) = (self.level(), self.min_saving);
        let (object, level) = match self.compression {
            Compression::Zstd if data.len() >= BLOCKING_LEN => {
                let owned = data.to_vec();
                let object = tokio::task::spawn_blocking(move || encode_zstd(&owned, level, min_saving).unwrap_or_else(|| plain(&owned)))
                    .await
                    .map_err(|e| StoreError::Backend(format!("compression task failed: {e}")))?;
                (object, level)
            }
            Compression::Zstd if data.len() >= MIN_COMPRESS_LEN => (encode_zstd(data, level, min_saving).unwrap_or_else(|| plain(data)), level),
            Compression::Zstd | Compression::None => (plain(data), 0),
        };
        let physical_len = object.len() as u64;
        self.backend.put_raw(&key.path(), Bytes::from(object)).await?;
        Ok(Written { physical_len, level })
    }

    /// Reads a blob and checks it. A blob whose bytes no longer match its ID is an error, never served.
    pub async fn get(&self, key: BlobKey) -> Result<Bytes, StoreError> {
        self.get_sized(key).await.map(|(data, _)| data)
    }

    /// Like [`Store::get`], and also says how many bytes the blob is stored as in the metadata: its plain length plus the
    /// format byte, whatever format the object has (the `blobs.stored_size` that quotas count). What the object takes on
    /// the backend is [`Store::stored_size`].
    pub async fn get_sized(&self, key: BlobKey) -> Result<(Bytes, u64), StoreError> {
        let object = self.read_object(key).await?;
        let data = decode_object(key, object).await?;
        let logical = u64::try_from(data.len()).unwrap_or(u64::MAX).saturating_add(1);
        Ok((data, logical))
    }

    /// The object as stored, read within the most any object of the kind can take (a longer one is cut off, and fails the
    /// checks of [`decode_object`]).
    async fn read_object(&self, key: BlobKey) -> Result<Bytes, StoreError> {
        let max_raw = max_len(key.kind).saturating_add(1);
        self.backend.get_raw_limited(&key.path(), max_raw).await?.ok_or(StoreError::NotFound(key))
    }

    /// Returns the size of the stored object (the format byte included) without materializing it. For a compressed
    /// blob that is less than the `stored_size` of its metadata row.
    pub async fn stored_size(&self, key: BlobKey) -> Result<u64, StoreError> {
        self.backend.size_raw(&key.path()).await?.ok_or(StoreError::NotFound(key))
    }

    /// The size of the stored object, or `None` when there is none; reads no content, and checks nothing.
    pub async fn object_len(&self, key: BlobKey) -> Result<Option<u64>, StoreError> {
        self.backend.size_raw(&key.path()).await
    }

    /// Rewrites the object at zstd `level` when that makes it at least [`RECOMPRESS_MIN_GAIN_PERCENT`] smaller, and
    /// never otherwise. The stored object is read and verified first (a damaged one is an error and is left alone),
    /// the new one is decompressed and checked against the ID before anything is written, and the write is the
    /// backend's atomic replace. The ID never changes, so a reader sees the old or the new object, both holding the
    /// same bytes. With `write` off nothing is written (a dry run). Blobs under 4 KiB are left as they are.
    pub async fn recompress(&self, key: BlobKey, level: u8, write: bool) -> Result<Recompressed, StoreError> {
        let path = key.path();
        let object = self.read_object(key).await?;
        let before = object.len() as u64;
        let kept = Recompressed { before, after: before, rewritten: false };
        let data = decode_object(key, object).await?;
        if data.len() < MIN_COMPRESS_LEN {
            return Ok(kept);
        }
        let min_saving = self.min_saving;
        let level = level.clamp(MIN_ZSTD_LEVEL, MAX_ZSTD_LEVEL);
        let candidate = tokio::task::spawn_blocking(move || -> Result<Option<Vec<u8>>, StoreError> {
            let Some(candidate) = encode_zstd(&data, level, min_saving) else { return Ok(None) };
            // The check before the replace: the candidate must decompress to bytes whose hash is the ID.
            let back = decode_zstd(key, candidate.get(1..).unwrap_or_default())?;
            if *blake3::hash(&back).as_bytes() != key.id {
                return Err(StoreError::Corrupt { key, reason: "recompressed data does not match the ID" });
            }
            Ok(Some(candidate))
        })
        .await
        .map_err(|e| StoreError::Backend(format!("compression task failed: {e}")))??;
        let Some(candidate) = candidate else { return Ok(kept) };
        let after = candidate.len() as u64;
        // At least 1% smaller than what is stored: after * 100 <= before * 99, in u64.
        if after.saturating_mul(100) > before.saturating_mul(100 - RECOMPRESS_MIN_GAIN_PERCENT) {
            return Ok(kept);
        }
        if write {
            self.backend.put_raw(&path, Bytes::from(candidate)).await?;
        }
        Ok(Recompressed { before, after, rewritten: write })
    }

    /// Compresses the blob at each of `levels` without storing anything, for measuring what a level would save and
    /// how fast it is. `None` when there is no such blob. The blob is read and verified like any read.
    pub async fn sample_levels(&self, key: BlobKey, levels: &[u8]) -> Result<Option<Vec<LevelSample>>, StoreError> {
        let data = match self.get(key).await {
            Ok(data) => data,
            Err(StoreError::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        let levels: Vec<u8> = levels.iter().map(|level| (*level).clamp(MIN_ZSTD_LEVEL, MAX_ZSTD_LEVEL)).collect();
        let min_saving = self.min_saving;
        let samples = tokio::task::spawn_blocking(move || {
            levels
                .into_iter()
                .map(|level| {
                    let started = std::time::Instant::now();
                    let object = if data.len() >= MIN_COMPRESS_LEN { encode_zstd(&data, level, min_saving) } else { None };
                    let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
                    let stored_len = object.map_or(data.len() as u64 + 1, |o| o.len() as u64);
                    LevelSample { level, plain_len: data.len() as u64, stored_len, nanos }
                })
                .collect::<Vec<_>>()
        })
        .await
        .map_err(|e| StoreError::Backend(format!("compression task failed: {e}")))?;
        Ok(Some(samples))
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

/// The plain bytes of a stored object, checked: format known, size within the limit, hash equal to the ID.
async fn decode_object(key: BlobKey, object: Bytes) -> Result<Bytes, StoreError> {
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

/// The object for `data` stored plain.
fn plain(data: &[u8]) -> Vec<u8> {
    let mut object = Vec::with_capacity(data.len().saturating_add(1));
    object.push(FORMAT_PLAIN);
    object.extend_from_slice(data);
    object
}

/// The object for `data` compressed with zstd at `level`, or `None` when that doesn't save at least `min_saving`
/// percent (or zstd fails, which leaves the plain object as a safe answer).
fn encode_zstd(data: &[u8], level: u8, min_saving: u8) -> Option<Vec<u8>> {
    let mut object = Vec::with_capacity(data.len() / 2 + 64);
    object.push(FORMAT_ZSTD);
    zstd::stream::copy_encode(data, &mut object, i32::from(level)).ok()?;
    let compressed = object.len().saturating_sub(1) as u64;
    // At most (100 - min_saving)% of the original: compressed * 100 <= len * (100 - min_saving), in u64 so it can't
    // overflow.
    let keep = 100 - u64::from(min_saving.clamp(1, 99));
    (compressed.saturating_mul(100) <= (data.len() as u64).saturating_mul(keep)).then_some(object)
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

    #[tokio::test]
    async fn disk_read_rejects_huge_object_without_reading_it_all() {
        let root = tempfile::tempdir().unwrap();
        let key = BlobKey { kind: BlobKind::Chunk, id: [7; 32] };
        let path = root.path().join(key.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(u64::from(MAX_CHUNK_BYTES) + 1_000_000_000).unwrap();
        let store = Store::new(disk::DiskBackend::new(root.path()).unwrap());
        assert!(matches!(store.get_sized(key).await, Err(StoreError::Corrupt { reason: "larger than the limit", .. })));
    }

    #[test]
    fn levels_are_clamped_and_shared_between_clones() {
        let store = Store::new(memory::MemoryBackend::new());
        assert_eq!(store.level(), DEFAULT_ZSTD_LEVEL);
        let clone = store.clone();
        store.set_level(19);
        assert_eq!(clone.level(), 19);
        store.set_level(0);
        assert_eq!(clone.level(), MIN_ZSTD_LEVEL);
        store.set_level(200);
        assert_eq!(clone.level(), MAX_ZSTD_LEVEL);
        assert_eq!(Store::new(memory::MemoryBackend::new()).with_min_saving(0).min_saving(), 1);
        assert_eq!(Store::new(memory::MemoryBackend::new()).with_min_saving(99).min_saving(), 90);
    }

    #[test]
    fn min_saving_decides_what_counts_as_compressed() {
        let data = "some text that repeats itself, ".repeat(400).into_bytes();
        assert!(encode_zstd(&data, 3, 5).is_some());
        assert!(encode_zstd(&data, 3, 90).is_some(), "this text shrinks by more than 90%");
        // Bytes zstd can't shrink never pass, whatever the setting.
        let mut noise = vec![0u8; 8192];
        blake3::Hasher::new().update(b"noise").finalize_xof().fill(&mut noise);
        assert!(encode_zstd(&noise, 3, 1).is_none());
    }
}
