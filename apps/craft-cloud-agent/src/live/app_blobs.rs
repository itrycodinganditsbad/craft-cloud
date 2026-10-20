//! Apps' blobs (`/v1/blobs`, [`craft_cloud_proto::local::blobs`]): binary content an app keeps out of a live
//! document, named by the manifest it cuts into and kept in `<data folder>/live/<space>/blobs/`.
//!
//! - **Put**: the bytes are cut as a file's content is ([`super::blobs::cut_content`]), a `<hash>.pending` mark is
//!   written, then the bytes (both atomically), and the app is answered. The upload (the chunks the server lacks, then
//!   the manifest, through the chunk API) follows at once when online; the mark goes once it is done. Offline, or when
//!   it fails, the mark stays and [`upload_pending`] tries again later (the local API runs it every
//!   [`RETRY_EVERY`]). A mark whose bytes never got written (a crash between the two) is dropped.
//! - **Get**: from the disk, checked against its name on every read; else downloaded (each chunk checked, the whole
//!   against the manifest), kept, and answered.

use std::path::{Path, PathBuf};
use std::time::Duration;

use craft_cloud_proto::limits::MAX_BLOB_BYTES;
use craft_cloud_proto::local::blobs::{Stored, hex, parse_hex};
use craft_cloud_proto::{ManifestId, SpaceId};
use craft_cloud_sync::transport::{Transport, TransportError};

use super::blobs::{cut_content, download_content, upload_content};
use crate::account::write_atomic;

/// How often the local API tries again to upload blobs put while offline.
pub const RETRY_EVERY: Duration = Duration::from_secs(30);

/// Why a blob call failed.
#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("a blob is 1 byte to {MAX_BLOB_BYTES} bytes")]
    Size,
    #[error("not a blob's name")]
    BadName,
    #[error("no such blob")]
    NotFound,
    #[error("the server can't be reached to fetch the blob: {0}")]
    Offline(String),
    #[error("{0}")]
    Failed(String),
}

/// Where a space's blobs are kept.
pub fn dir(data: &Path, space: SpaceId) -> PathBuf {
    data.join("live").join(space.to_string()).join("blobs")
}

fn content_path(dir: &Path, hash: &[u8; 32]) -> PathBuf {
    dir.join(hex(hash))
}

fn pending_path(dir: &Path, hash: &[u8; 32]) -> PathBuf {
    dir.join(format!("{}.pending", hex(hash)))
}

fn io(e: std::io::Error) -> BlobError {
    BlobError::Failed(e.to_string())
}

/// The name `bytes` have: the ID of the manifest they cut into.
pub fn name(bytes: &[u8]) -> Result<[u8; 32], BlobError> {
    if bytes.is_empty() || bytes.len() > MAX_BLOB_BYTES {
        return Err(BlobError::Size);
    }
    let (manifest, _) = cut_content(bytes).map_err(BlobError::Failed)?;
    Ok(*manifest.id().as_bytes())
}

/// Keeps `bytes` in `dir`, marked for upload (blocking: run it off the async threads).
pub fn put(dir: &Path, bytes: &[u8]) -> Result<Stored, BlobError> {
    let hash = name(bytes)?;
    let stored = Stored { hash, len: bytes.len() as u64 };
    std::fs::create_dir_all(dir).map_err(io)?;
    let content = content_path(dir, &hash);
    if content.exists() && !pending_path(dir, &hash).exists() {
        // Kept already (uploaded, or downloaded): nothing to send.
        return Ok(stored);
    }
    write_atomic(&pending_path(dir, &hash), b"", true).map_err(io)?;
    if !content.exists() {
        write_atomic(&content, bytes, true).map_err(io)?;
    }
    Ok(stored)
}

/// The bytes named `hash` if this machine has them, checked against the name (blocking).
pub fn read_local(dir: &Path, hash: &[u8; 32]) -> Result<Option<Vec<u8>>, BlobError> {
    let bytes = match std::fs::read(content_path(dir, hash)) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io(e)),
    };
    if name(&bytes).ok().as_ref() != Some(hash) {
        // A damaged copy is never served: it is fetched again.
        let _ = std::fs::remove_file(content_path(dir, hash));
        return Ok(None);
    }
    Ok(Some(bytes))
}

/// Keeps a downloaded blob (no upload mark: the server has it).
pub fn keep(dir: &Path, hash: &[u8; 32], bytes: &[u8]) -> Result<(), BlobError> {
    std::fs::create_dir_all(dir).map_err(io)?;
    write_atomic(&content_path(dir, hash), bytes, true).map_err(io)
}

/// The bytes named `hash`: from `dir`, else downloaded through `transport` (if any: `None` is offline) and kept.
pub async fn get<T: Transport + ?Sized>(dir: PathBuf, hash: [u8; 32], transport: Option<&T>) -> Result<Vec<u8>, BlobError> {
    let local_dir = dir.clone();
    let local = tokio::task::spawn_blocking(move || read_local(&local_dir, &hash)).await.map_err(|e| BlobError::Failed(e.to_string()))??;
    if let Some(bytes) = local {
        return Ok(bytes);
    }
    let Some(transport) = transport else { return Err(BlobError::Offline("not signed in".into())) };
    let bytes = match download_content(transport, &ManifestId::from_bytes(hash), MAX_BLOB_BYTES as u64).await {
        Ok(bytes) => bytes,
        Err(TransportError::NotFound) => return Err(BlobError::NotFound),
        Err(TransportError::Unavailable(e)) => return Err(BlobError::Offline(e)),
        Err(e) => return Err(BlobError::Failed(e.to_string())),
    };
    let (keep_dir, kept) = (dir, bytes.clone());
    // Failing to keep it only means fetching it again next time.
    let _ = tokio::task::spawn_blocking(move || keep(&keep_dir, &hash, &kept)).await;
    Ok(bytes)
}

/// The blobs in `dir` still marked for upload (a mark without bytes is dropped).
fn pending(dir: &Path) -> Vec<[u8; 32]> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let file = entry.file_name();
        let Some(hash) = file.to_str().and_then(|n| n.strip_suffix(".pending")).and_then(parse_hex) else { continue };
        if content_path(dir, &hash).exists() {
            out.push(hash);
        } else {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    out
}

/// Uploads one kept blob and drops its mark.
pub async fn upload_one<T: Transport + ?Sized>(dir: &Path, hash: [u8; 32], transport: &T) -> Result<(), BlobError> {
    let path = content_path(dir, &hash);
    let cut = tokio::task::spawn_blocking(move || -> Result<_, BlobError> {
        let bytes = std::fs::read(&path).map_err(io)?;
        cut_content(&bytes).map_err(BlobError::Failed)
    })
    .await
    .map_err(|e| BlobError::Failed(e.to_string()))??;
    let (manifest, chunks) = cut;
    if *manifest.id().as_bytes() != hash {
        let _ = std::fs::remove_file(content_path(dir, &hash));
        let _ = std::fs::remove_file(pending_path(dir, &hash));
        return Err(BlobError::Failed("a kept blob was damaged on disk".into()));
    }
    match upload_content(transport, &manifest, &chunks).await {
        Ok(()) => {
            let _ = std::fs::remove_file(pending_path(dir, &hash));
            Ok(())
        }
        Err(TransportError::Unavailable(e)) => Err(BlobError::Offline(e)),
        Err(e) => Err(BlobError::Failed(e.to_string())),
    }
}

/// Uploads every blob in `dir` still marked; returns how many went up. Stops at the first that can't (offline).
pub async fn upload_pending<T: Transport + ?Sized>(dir: &Path, transport: &T) -> usize {
    let scan = dir.to_owned();
    let hashes = tokio::task::spawn_blocking(move || pending(&scan)).await.unwrap_or_default();
    let mut done = 0;
    for hash in hashes {
        match upload_one(dir, hash, transport).await {
            Ok(()) => done += 1,
            Err(BlobError::Offline(_)) => break,
            Err(e) => crate::runlog::note(format_args!("a blob didn't upload: {e}")),
        }
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A put is kept with its mark (the mark first); the same bytes have the same name; a damaged copy is never
    /// served; a mark without bytes is dropped.
    #[test]
    fn blobs_are_kept_named_and_checked() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("blobs");
        let bytes: Vec<u8> = (0..3_000_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8).collect();
        let stored = put(&dir, &bytes).unwrap();
        assert_eq!(stored.len, bytes.len() as u64);
        assert_eq!(put(&dir, &bytes).unwrap(), stored, "the same name every time");
        assert!(pending_path(&dir, &stored.hash).exists());
        assert_eq!(pending(&dir), vec![stored.hash]);
        assert_eq!(read_local(&dir, &stored.hash).unwrap(), Some(bytes.clone()));
        std::fs::write(content_path(&dir, &stored.hash), b"damaged").unwrap();
        assert_eq!(read_local(&dir, &stored.hash).unwrap(), None, "a damaged copy isn't served");
        assert!(pending(&dir).is_empty(), "its mark goes with it");
        assert!(matches!(put(&dir, &[]), Err(BlobError::Size)));
        assert!(matches!(put(&dir, &vec![0; MAX_BLOB_BYTES + 1]), Err(BlobError::Size)));
        assert!(put(&dir, &vec![1; MAX_BLOB_BYTES]).is_ok(), "the limit itself is fine");
    }
}
