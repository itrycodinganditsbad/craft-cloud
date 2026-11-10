//! The same behaviour from every backend. Memory and disk always run; S3 runs when
//! `CRAFT_CLOUD_TEST_S3_ENDPOINT` is set (with `_BUCKET`, `_ACCESS_KEY`, `_SECRET_KEY`), e.g. against a local
//! SeaweedFS container (Apache-2.0; run as a separate process, never linked):
//! `docker run -p 8333:8333 chrislusf/seaweedfs server -s3` then `curl -X PUT http://127.0.0.1:8333/cc-test`. CI sets `CRAFT_CLOUD_REQUIRE_BACKENDS`, which turns a
//! missing S3 configuration into a failure instead of a skip.
//!
//! With the `sftp` feature, the SFTP backend runs when `CRAFT_CLOUD_TEST_SFTP_HOST` is set (with `_PORT`, `_USER`,
//! `_KEY_FILE`, `_HOST_KEY` (a `SHA256:` fingerprint) and `_ROOT`, a directory the account may write in; every test uses
//! a fresh directory below it), e.g. against an OpenSSH container (`.github/workflows/ci.yml` starts one). Like S3 it fails
//! instead of skipping when `CRAFT_CLOUD_REQUIRE_BACKENDS` is set.

use bytes::Bytes;
use craft_cloud_proto::limits::{MAX_CHUNK_BYTES, MAX_MANIFEST_BYTES};
use craft_cloud_proto::{ChunkId, ManifestId};
use craft_cloud_store::disk::DiskBackend;
use craft_cloud_store::memory::MemoryBackend;
use craft_cloud_store::s3::{S3Backend, S3Config};
use craft_cloud_store::{Backend, BlobKey, Compression, ListCursor, MAX_LIST_PAGE_SIZE, Store, StoreError};

fn chunk_key(data: &[u8]) -> BlobKey {
    BlobKey::chunk(&ChunkId::from_bytes(*blake3::hash(data).as_bytes()))
}

async fn conformance(store: Store) {
    let empty = store.list(craft_cloud_store::BlobKind::Chunk, None, 3).await.unwrap();
    assert!(empty.blobs.is_empty() && empty.foreign.is_empty() && empty.next_cursor.is_none());
    let mut expected = Vec::new();
    for n in 0..8 {
        let data = format!("listing conformance {n} {}", uuid::Uuid::now_v7()).into_bytes();
        let key = chunk_key(&data);
        store.put(key, &data).await.unwrap();
        expected.push(key);
    }
    expected.sort_by_key(BlobKey::path);
    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = store.list(craft_cloud_store::BlobKind::Chunk, cursor, 3).await.unwrap();
        assert!(page.foreign.is_empty());
        assert!(page.blobs.len() <= 3);
        seen.extend(page.blobs);
        cursor = page.next_cursor.map(|cursor| ListCursor::from_token(cursor.as_token().to_owned()));
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(seen, expected);

    // Round trip, both kinds. A unique payload per run keeps S3 runs independent.
    let data = format!("chunk bytes {}", uuid::Uuid::now_v7()).into_bytes();
    let key = chunk_key(&data);
    store.put(key, &data).await.unwrap();
    assert_eq!(store.get(key).await.unwrap(), Bytes::from(data.clone()));
    let manifest = format!("manifest bytes {}", uuid::Uuid::now_v7()).into_bytes();
    let mkey = BlobKey::manifest(&ManifestId::from_bytes(*blake3::hash(&manifest).as_bytes()));
    store.put(mkey, &manifest).await.unwrap();
    assert_eq!(store.get(mkey).await.unwrap(), Bytes::from(manifest));

    // Same bytes again: fine.
    store.put(key, &data).await.unwrap();

    // Wrong hash: refused, nothing stored.
    let wrong = chunk_key(b"something else entirely");
    assert!(matches!(store.put(wrong, &data).await, Err(StoreError::HashMismatch(_))));
    assert!(matches!(store.get(wrong).await, Err(StoreError::NotFound(_))));

    // Too large: refused before hashing.
    let big = vec![0u8; MAX_CHUNK_BYTES as usize + 1];
    assert!(matches!(store.put(chunk_key(&big), &big).await, Err(StoreError::TooLarge { .. })));

    // Delete, twice.
    store.delete(key).await.unwrap();
    assert!(matches!(store.get(key).await, Err(StoreError::NotFound(_))));
    store.delete(key).await.unwrap();
}

/// A compressible blob of `len` bytes, unique per run (S3 runs share a bucket).
fn compressible(len: usize) -> Vec<u8> {
    let mut data = format!("{} ", uuid::Uuid::now_v7()).into_bytes();
    while data.len() < len {
        data.extend_from_slice(b"the same words over and over, as text and code have them. ");
    }
    data.truncate(len);
    data
}

/// Bytes zstd can't shrink (BLAKE3's output stream, so no extra dependency), unique per run.
fn incompressible(len: usize) -> Vec<u8> {
    let mut data = vec![0u8; len];
    blake3::Hasher::new().update(uuid::Uuid::now_v7().as_bytes()).finalize_xof().fill(&mut data);
    data
}

/// The object as stored: format byte first.
async fn raw(backend: &dyn Backend, key: BlobKey) -> Bytes {
    backend.get_raw(&key.path()).await.unwrap().unwrap()
}

/// Writes the object `[format] + payload` straight to the backend under `key`, as damage or an older server would.
async fn plant(backend: &dyn Backend, key: BlobKey, format: u8, payload: &[u8]) {
    let mut object = vec![format];
    object.extend_from_slice(payload);
    backend.put_raw(&key.path(), Bytes::from(object)).await.unwrap();
}

fn zstd(data: &[u8]) -> Vec<u8> {
    zstd::bulk::compress(data, 3).unwrap()
}

/// Text built from a small vocabulary in pseudo-random order: it compresses, and higher levels find more in it
/// than level 1. Unique per run.
fn wordy(len: usize) -> Vec<u8> {
    const WORDS: [&str; 24] = [
        "layer", "mask", "curve", "brush", "export", "canvas", "node", "group", "alpha", "blend", "filter", "frame", "path", "stroke", "fill",
        "style", "color", "scale", "rotate", "offset", "guide", "grid", "snap", "undo",
    ];
    let mut rnd = vec![0u8; len];
    blake3::Hasher::new().update(uuid::Uuid::now_v7().as_bytes()).finalize_xof().fill(&mut rnd);
    let mut text = Vec::with_capacity(len + 16);
    for b in rnd {
        text.extend_from_slice(WORDS[usize::from(b) % WORDS.len()].as_bytes());
        text.push(b' ');
        if text.len() >= len {
            break;
        }
    }
    text.truncate(len);
    text
}

/// Writing at levels 1, 3 and 19 round-trips and reports what it wrote; recompressing rewrites only when that is
/// smaller, checks before it replaces, is idempotent, leaves damaged objects alone, and a dry run changes nothing.
async fn levels_and_recompress(store: Store, backend: &dyn Backend) {
    let data = wordy(512 * 1024);
    let mut sizes = Vec::new();
    for level in [1u8, 3, 19] {
        let leveled = store.clone().with_level(level);
        let blob = wordy(512 * 1024);
        let key = chunk_key(&blob);
        let written = leveled.put(key, &blob).await.unwrap();
        assert_eq!(written.level, level);
        let object = raw(backend, key).await;
        assert_eq!((object[0], written.physical_len), (1, object.len() as u64));
        assert_eq!(leveled.get(key).await.unwrap(), Bytes::from(blob));
        assert_eq!(store.object_len(key).await.unwrap(), Some(object.len() as u64));
        sizes.push(object.len());
    }
    assert!(sizes[2] < sizes[0], "level 19 should beat level 1 on this text: {sizes:?}");
    assert_eq!(store.object_len(chunk_key(b"never stored")).await.unwrap(), None);

    // Stored at 1, recompressed at 19: smaller, still readable, ID unchanged.
    let key = chunk_key(&data);
    store.clone().with_level(1).put(key, &data).await.unwrap();
    let before = raw(backend, key).await;
    let dry = store.recompress(key, 19, false).await.unwrap();
    assert!(!dry.rewritten && dry.after < dry.before, "{dry:?}");
    assert_eq!(raw(backend, key).await, before, "a dry run writes nothing");
    let done = store.recompress(key, 19, true).await.unwrap();
    assert!(done.rewritten && done.before == before.len() as u64 && done.after < done.before, "{done:?}");
    let after = raw(backend, key).await;
    assert_eq!((after[0], after.len() as u64), (1, done.after));
    assert_eq!(store.get(key).await.unwrap(), Bytes::from(data.clone()));
    // Idempotent: nothing more to gain, nothing written.
    let again = store.recompress(key, 19, true).await.unwrap();
    assert!(!again.rewritten && again.before == again.after, "{again:?}");
    assert_eq!(raw(backend, key).await, after);
    // A lower level never makes it bigger again.
    let lower = store.recompress(key, 3, true).await.unwrap();
    assert!(!lower.rewritten);
    assert_eq!(raw(backend, key).await, after);

    // Plain because zstd could not shrink it, plain because it is tiny: left alone.
    let noise = incompressible(64 * 1024);
    store.put(chunk_key(&noise), &noise).await.unwrap();
    assert!(!store.recompress(chunk_key(&noise), 19, true).await.unwrap().rewritten);
    assert_eq!(raw(backend, chunk_key(&noise)).await[0], 0);
    let tiny = compressible(1024);
    store.clone().with_compression(Compression::None).put(chunk_key(&tiny), &tiny).await.unwrap();
    assert!(!store.recompress(chunk_key(&tiny), 19, true).await.unwrap().rewritten);

    // Plain but compressible (written with compression off): becomes smaller.
    let old = wordy(256 * 1024);
    store.clone().with_compression(Compression::None).put(chunk_key(&old), &old).await.unwrap();
    let up = store.recompress(chunk_key(&old), 3, true).await.unwrap();
    assert!(up.rewritten && up.after < up.before / 2, "{up:?}");
    assert_eq!(raw(backend, chunk_key(&old)).await[0], 1);
    assert_eq!(store.get(chunk_key(&old)).await.unwrap(), Bytes::from(old));

    // Damaged, wrong and missing objects are errors, and nothing is written over them.
    let bad = wordy(64 * 1024);
    let bad_key = chunk_key(&bad);
    plant(backend, bad_key, 1, &zstd(&wordy(64 * 1024))).await;
    let planted = raw(backend, bad_key).await;
    assert!(matches!(store.recompress(bad_key, 19, true).await, Err(StoreError::Corrupt { .. })));
    assert_eq!(raw(backend, bad_key).await, planted, "left as found");
    plant(backend, bad_key, 0, b"not the bytes").await;
    assert!(matches!(store.recompress(bad_key, 19, true).await, Err(StoreError::Corrupt { .. })));
    assert_eq!(&raw(backend, bad_key).await[1..], b"not the bytes", "left as found");
    assert!(matches!(store.recompress(chunk_key(b"absent"), 19, true).await, Err(StoreError::NotFound(_))));

    // Levels outside 1 to 19 are clamped, not passed to zstd.
    assert_eq!(store.clone().with_level(0).level(), 1);
    assert_eq!(store.clone().with_level(22).level(), 19);

    // sample_levels measures without storing anything.
    let sample = store.sample_levels(key, &[1, 19]).await.unwrap().unwrap();
    assert_eq!(sample.len(), 2);
    assert!(sample[1].stored_len <= sample[0].stored_len && sample[0].plain_len == data.len() as u64);
    assert_eq!(raw(backend, key).await, after);
    assert!(store.sample_levels(chunk_key(b"absent"), &[3]).await.unwrap().is_none());
}

/// A level 19 object of the largest chunk reads back under the reader's 8 MiB window cap.
async fn level_19_reads_within_the_window_cap(store: Store) {
    let big = wordy(MAX_CHUNK_BYTES as usize);
    let key = chunk_key(&big);
    store.clone().with_level(19).put(key, &big).await.unwrap();
    assert_eq!(store.get(key).await.unwrap(), Bytes::from(big));
}

/// How blobs are stored and read back on one backend: `store` writes with the default compression, `plain` with
/// [`Compression::None`], and `backend` reaches the same objects raw.
async fn formats(store: Store, plain: Store, backend: &dyn Backend) {
    // Compressible: format 1, much smaller, read back byte for byte.
    let data = compressible(256 * 1024);
    let key = chunk_key(&data);
    store.put(key, &data).await.unwrap();
    let object = raw(backend, key).await;
    assert_eq!(object[0], 1);
    assert!(object.len() < data.len() / 4, "{} bytes stored for {}", object.len(), data.len());
    assert_eq!(store.get(key).await.unwrap(), Bytes::from(data.clone()));
    // Under 64 KiB it is compressed on the async thread; the result is the same. Manifests too.
    let small = compressible(16 * 1024);
    store.put(chunk_key(&small), &small).await.unwrap();
    assert_eq!(raw(backend, chunk_key(&small)).await[0], 1);
    assert_eq!(store.get(chunk_key(&small)).await.unwrap(), Bytes::from(small));
    let manifest = compressible(8 * 1024);
    let mkey = BlobKey::manifest(&ManifestId::from_bytes(*blake3::hash(&manifest).as_bytes()));
    store.put(mkey, &manifest).await.unwrap();
    assert_eq!(raw(backend, mkey).await[0], 1);
    assert_eq!(store.get(mkey).await.unwrap(), Bytes::from(manifest));

    // Incompressible: plain.
    let noise = incompressible(256 * 1024);
    store.put(chunk_key(&noise), &noise).await.unwrap();
    let object = raw(backend, chunk_key(&noise)).await;
    assert_eq!((object[0], object.len()), (0, noise.len() + 1));
    assert_eq!(store.get(chunk_key(&noise)).await.unwrap(), Bytes::from(noise));

    // Under 4 KiB: plain without trying, however compressible.
    let tiny = compressible(4 * 1024 - 1);
    store.put(chunk_key(&tiny), &tiny).await.unwrap();
    assert_eq!(raw(backend, chunk_key(&tiny)).await[0], 0);
    assert_eq!(store.get(chunk_key(&tiny)).await.unwrap(), Bytes::from(tiny));

    // An object written plain before compression existed still reads.
    let old = compressible(64 * 1024);
    plant(backend, chunk_key(&old), 0, &old).await;
    assert_eq!(store.get(chunk_key(&old)).await.unwrap(), Bytes::from(old));

    // Compression off: plain only, and what is stored compressed still reads.
    let off = compressible(128 * 1024);
    plain.put(chunk_key(&off), &off).await.unwrap();
    assert_eq!(raw(backend, chunk_key(&off)).await[0], 0);
    assert_eq!(plain.get(chunk_key(&off)).await.unwrap(), Bytes::from(off));
    assert_eq!(plain.get(key).await.unwrap(), Bytes::from(data));
}

#[tokio::test]
async fn memory_backend() {
    conformance(Store::new(MemoryBackend::new())).await;
    let backend = MemoryBackend::new();
    formats(Store::new(backend.clone()), Store::new(backend.clone()).with_compression(Compression::None), &backend).await;
    levels_and_recompress(Store::new(backend.clone()), &backend).await;
    level_19_reads_within_the_window_cap(Store::new(backend.clone())).await;
}

#[tokio::test]
async fn disk_backend() {
    let dir = tempfile::tempdir().unwrap();
    conformance(Store::new(DiskBackend::new(dir.path()).unwrap())).await;
    let formats_dir = tempfile::tempdir().unwrap();
    let disk = || DiskBackend::new(formats_dir.path()).unwrap();
    formats(Store::new(disk()), Store::new(disk()).with_compression(Compression::None), &disk()).await;
    levels_and_recompress(Store::new(disk()), &disk()).await;
}

#[tokio::test]
async fn s3_backend() {
    let Ok(endpoint) = std::env::var("CRAFT_CLOUD_TEST_S3_ENDPOINT") else {
        assert!(
            std::env::var_os("CRAFT_CLOUD_REQUIRE_BACKENDS").is_none(),
            "CRAFT_CLOUD_REQUIRE_BACKENDS is set but CRAFT_CLOUD_TEST_S3_ENDPOINT is not"
        );
        eprintln!("skipped: CRAFT_CLOUD_TEST_S3_ENDPOINT not set");
        return;
    };
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set with CRAFT_CLOUD_TEST_S3_ENDPOINT"));
    let config = S3Config {
        bucket: var("CRAFT_CLOUD_TEST_S3_BUCKET"),
        prefix: format!("test-{}/a//b%bar", uuid::Uuid::now_v7()),
        region: "us-east-1".into(),
        endpoint: Some(endpoint),
        access_key_id: var("CRAFT_CLOUD_TEST_S3_ACCESS_KEY"),
        secret_access_key: var("CRAFT_CLOUD_TEST_S3_SECRET_KEY"),
        allow_http: true,
    };
    let store = Store::new(S3Backend::new(&config).unwrap());
    conformance(store.clone()).await;
    let raw = S3Backend::new(&config).unwrap();
    raw.put_raw("chunks/foreign-object", Bytes::new()).await.unwrap();
    let page = store.list(craft_cloud_store::BlobKind::Chunk, None, 10).await.unwrap();
    assert_eq!(page.foreign, vec!["chunks/foreign-object"]);
    let sibling_config = S3Config { prefix: format!("{}-sibling", config.prefix), ..config.clone() };
    let sibling = Store::new(S3Backend::new(&sibling_config).unwrap());
    let data = b"sibling prefix object";
    let key = chunk_key(data);
    sibling.put(key, data).await.unwrap();
    let page = store.list(craft_cloud_store::BlobKind::Chunk, None, 10).await.unwrap();
    assert!(!page.blobs.contains(&key));
    assert!(!page.foreign.iter().any(|path| path.starts_with("chunks-sibling/")));
    let s3 = || S3Backend::new(&config).unwrap();
    formats(Store::new(s3()), Store::new(s3()).with_compression(Compression::None), &s3()).await;
    levels_and_recompress(Store::new(s3()), &s3()).await;
}

#[tokio::test]
async fn damaged_objects_are_errors_not_data() {
    let backend = MemoryBackend::new();
    let store = Store::new(backend.clone());
    let data = b"some chunk".to_vec();
    let key = chunk_key(&data);
    store.put(key, &data).await.unwrap();

    let mut flipped = vec![0u8];
    flipped.extend_from_slice(b"some chunK");
    backend.overwrite_raw(&key.path(), Bytes::from(flipped)).unwrap();
    assert!(matches!(store.get(key).await, Err(StoreError::Corrupt { reason: "hash mismatch", .. })));

    let mut unknown_format = vec![7u8];
    unknown_format.extend_from_slice(&data);
    backend.overwrite_raw(&key.path(), Bytes::from(unknown_format)).unwrap();
    assert!(matches!(store.get(key).await, Err(StoreError::Corrupt { reason: "unknown format byte", .. })));

    backend.overwrite_raw(&key.path(), Bytes::new()).unwrap();
    assert!(matches!(store.get(key).await, Err(StoreError::Corrupt { reason: "empty object", .. })));
}

#[tokio::test]
async fn memory_list_is_prefix_scoped_and_paginated() {
    let backend = MemoryBackend::new();
    let store = Store::new(backend.clone());
    let key = BlobKey::chunk(&ChunkId::from_bytes([0x11; 32]));
    backend.overwrite_raw(&key.path(), Bytes::new()).unwrap();
    backend.overwrite_raw("chunks//a", Bytes::new()).unwrap();
    backend.overwrite_raw("chunksx/sibling-object", Bytes::new()).unwrap();

    let first = store.list(craft_cloud_store::BlobKind::Chunk, None, 1).await.unwrap();
    assert!(first.blobs.is_empty());
    assert_eq!(first.foreign, vec!["chunks//a"]);
    let second = store.list(craft_cloud_store::BlobKind::Chunk, first.next_cursor, 1).await.unwrap();
    assert_eq!(second.blobs, vec![key]);
    assert!(second.foreign.is_empty());
    assert!(second.next_cursor.is_none());
}

#[tokio::test]
async fn list_empty_store_returns_empty_first_page() {
    let store = Store::new(MemoryBackend::new());
    let page = store.list(craft_cloud_store::BlobKind::Manifest, None, 3).await.unwrap();
    assert!(page.blobs.is_empty() && page.foreign.is_empty() && page.next_cursor.is_none());
}

#[tokio::test]
async fn list_rejects_invalid_cursor_and_zero_limit() {
    let store = Store::new(MemoryBackend::new());
    assert!(
        matches!(store.list(craft_cloud_store::BlobKind::Chunk, Some(ListCursor::from_token("bad")), 1).await, Err(StoreError::Backend(message)) if message == "invalid listing cursor")
    );
    assert!(
        matches!(store.list(craft_cloud_store::BlobKind::Chunk, None, 0).await, Err(StoreError::Backend(message)) if message == "listing limit must be greater than zero")
    );
}

#[tokio::test]
async fn list_clamps_large_limit() {
    let backend = MemoryBackend::new();
    let store = Store::new(backend.clone());
    for n in 0..=MAX_LIST_PAGE_SIZE {
        backend.overwrite_raw(&format!("chunks/foreign-{n:04}"), Bytes::new()).unwrap();
    }
    let page = store.list(craft_cloud_store::BlobKind::Chunk, None, MAX_LIST_PAGE_SIZE + 40).await.unwrap();
    assert_eq!(page.foreign.len(), MAX_LIST_PAGE_SIZE);
    assert!(page.next_cursor.is_some());
}

#[cfg(unix)]
#[tokio::test]
async fn disk_list_skips_temps_and_symlinks_outside_root() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("escaped"), b"outside").unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("chunks")).unwrap();
    std::fs::write(root.join("chunks").join(format!(".tmp-{}", uuid::Uuid::now_v7())), b"temp").unwrap();
    std::fs::write(root.join("chunks/foreign-object"), b"foreign").unwrap();
    symlink(outside.path(), root.join("chunks/link")).unwrap();
    let store = Store::new(DiskBackend::new(&root).unwrap());
    let page = store.list(craft_cloud_store::BlobKind::Chunk, None, 10).await.unwrap();
    assert!(page.blobs.is_empty());
    assert_eq!(page.foreign, vec!["chunks/foreign-object"]);
}

#[tokio::test]
async fn disk_list_rejects_prefix_outside_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(dir.path().join("outside"), b"outside").unwrap();
    let backend = DiskBackend::new(&root).unwrap();
    assert!(matches!(backend.list_raw("../", None, 10).await, Err(StoreError::Backend(_))));
}

#[cfg(unix)]
#[tokio::test]
async fn disk_list_does_not_follow_symlink_prefix() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(outside.join("sub")).unwrap();
    std::fs::write(outside.join("sub/leak"), b"outside").unwrap();
    symlink(&outside, root.join("link")).unwrap();
    let backend = DiskBackend::new(&root).unwrap();
    assert!(backend.list_raw("link", None, 10).await.unwrap().is_empty());
    assert!(backend.list_raw("link/sub", None, 10).await.unwrap().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn disk_list_reports_non_utf8_names() {
    use std::os::unix::ffi::OsStrExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("chunks")).unwrap();
    let name = std::ffi::OsStr::from_bytes(b"bad-\xff");
    if let Err(error) = std::fs::write(root.join("chunks").join(name), b"foreign") {
        // Some file systems (APFS) refuse a name that is not valid UTF-8 (EILSEQ), so no such name can be listed there: the
        // test has nothing to check.
        #[cfg(target_os = "linux")]
        const EILSEQ: i32 = 84;
        #[cfg(target_os = "macos")]
        const EILSEQ: i32 = 92;
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        const EILSEQ: i32 = -1;
        let refused = error.kind() == std::io::ErrorKind::InvalidFilename || error.raw_os_error() == Some(EILSEQ);
        assert!(refused, "unexpected error creating the file: {error}");
        eprintln!("skipped: this file system refuses names that are not valid UTF-8 ({error})");
        return;
    }
    let store = Store::new(DiskBackend::new(&root).unwrap());
    assert!(
        matches!(store.list(craft_cloud_store::BlobKind::Chunk, None, 10).await, Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::InvalidData && error.to_string() == "store path is not valid UTF-8")
    );
}

#[tokio::test]
async fn disk_list_includes_foreign_and_skips_temporary_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("chunks")).unwrap();
    std::fs::write(root.join("chunks").join(format!(".tmp-{}", uuid::Uuid::now_v7())), b"temp").unwrap();
    std::fs::write(root.join("chunks/foreign-object"), b"foreign").unwrap();
    let store = Store::new(DiskBackend::new(&root).unwrap());
    let page = store.list(craft_cloud_store::BlobKind::Chunk, None, 10).await.unwrap();
    assert!(page.blobs.is_empty());
    assert_eq!(page.foreign, vec!["chunks/foreign-object"]);
}

#[tokio::test]
async fn damaged_compressed_objects_are_errors_not_data() {
    let backend = MemoryBackend::new();
    let store = Store::new(backend.clone());
    let data = compressible(32 * 1024);
    let key = chunk_key(&data);

    // Not zstd at all, a truncated stream, and an empty one: errors, never a panic or data.
    plant(&backend, key, 1, b"definitely not a zstd stream").await;
    assert!(matches!(store.get(key).await, Err(StoreError::Corrupt { reason: "undecodable compressed data", .. })));
    let stream = zstd(&data);
    plant(&backend, key, 1, &stream[..stream.len() / 2]).await;
    assert!(matches!(store.get(key).await, Err(StoreError::Corrupt { .. })));
    plant(&backend, key, 1, &[]).await;
    assert!(matches!(store.get(key).await, Err(StoreError::Corrupt { .. })));

    // A valid stream of other bytes: they don't match the ID once unpacked.
    plant(&backend, key, 1, &zstd(&compressible(32 * 1024))).await;
    assert!(matches!(store.get(key).await, Err(StoreError::Corrupt { reason: "hash mismatch", .. })));

    // A stream that unpacks past the largest blob of its kind is refused (a zip bomb), for chunks and manifests.
    let bomb = vec![0u8; MAX_CHUNK_BYTES as usize + 1];
    plant(&backend, chunk_key(&bomb), 1, &zstd(&bomb)).await;
    assert!(matches!(store.get(chunk_key(&bomb)).await, Err(StoreError::Corrupt { reason: "larger than the limit", .. })));
    let bomb = vec![0u8; MAX_MANIFEST_BYTES + 1];
    let mkey = BlobKey::manifest(&ManifestId::from_bytes(*blake3::hash(&bomb).as_bytes()));
    plant(&backend, mkey, 1, &zstd(&bomb)).await;
    assert!(matches!(store.get(mkey).await, Err(StoreError::Corrupt { reason: "larger than the limit", .. })));

    // Format 2 and up are still refused, whatever follows.
    plant(&backend, key, 2, &zstd(&data)).await;
    assert!(matches!(store.get(key).await, Err(StoreError::Corrupt { reason: "unknown format byte", .. })));

    // The real thing reads.
    store.put(key, &data).await.unwrap();
    assert_eq!(store.get(key).await.unwrap(), Bytes::from(data));
}

#[tokio::test]
async fn disk_puts_into_new_shard_directories_survive() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("fresh");
    // Two chunks whose first shard bytes differ, so they land in different directories that don't exist yet.
    let first_data = b"shard one".to_vec();
    let first_key = chunk_key(&first_data);
    let mut n = 0u32;
    let (second_data, second_key) = loop {
        let data = format!("shard two {n}").into_bytes();
        let key = chunk_key(&data);
        if key.path().split('/').nth(1) != first_key.path().split('/').nth(1) {
            break (data, key);
        }
        n += 1;
    };

    let store = Store::new(DiskBackend::new(&root).unwrap());
    store.put(first_key, &first_data).await.unwrap();
    store.put(second_key, &second_data).await.unwrap();
    drop(store);

    // A fresh backend and store on the same root read both back.
    let reopened = Store::new(DiskBackend::new(&root).unwrap());
    assert_eq!(reopened.get(first_key).await.unwrap(), Bytes::from(first_data));
    assert_eq!(reopened.get(second_key).await.unwrap(), Bytes::from(second_data));
}

#[tokio::test]
async fn disk_writes_leave_no_temporary_files() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(DiskBackend::new(dir.path()).unwrap());
    let data = b"chunk".to_vec();
    let key = chunk_key(&data);
    store.put(key, &data).await.unwrap();
    let shard = dir.path().join(key.path()).parent().unwrap().to_path_buf();
    let names: Vec<String> = std::fs::read_dir(shard).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    assert_eq!(names, vec![key.path().rsplit('/').next().unwrap().to_owned()]);
}

/// Walks a whole listing with `limit` per page through the backend and returns every path in order.
async fn list_all_raw(backend: &impl Backend, prefix: &str, limit: usize) -> Vec<String> {
    let mut all: Vec<String> = Vec::new();
    loop {
        let page = backend.list_raw(prefix, all.last().map(String::as_str), limit).await.unwrap();
        if page.is_empty() {
            return all;
        }
        assert!(page.len() <= limit);
        all.extend(page);
    }
}

#[tokio::test]
async fn disk_list_page_does_not_walk_finished_directories_again() {
    let dir = tempfile::tempdir().unwrap();
    let backend = DiskBackend::new(dir.path()).unwrap();
    let mut paths = Vec::new();
    for n in 0..60 {
        let path = chunk_key(format!("walk cost {n}").as_bytes()).path();
        backend.put_raw(&path, Bytes::from_static(b"\0x")).await.unwrap();
        paths.push(path);
    }
    paths.sort();
    let shards: std::collections::BTreeSet<&str> = paths.iter().filter_map(|path| path.split('/').nth(1)).collect();
    assert!(shards.len() >= 40, "{} shard directories", shards.len());

    // Page 1 ends in the middle of the store; page 2 resumes there.
    let page_one = backend.list_raw("chunks", None, 30).await.unwrap();
    assert_eq!(page_one, paths[..30]);
    let before = backend.directories_read();
    let page_two = backend.list_raw("chunks", page_one.last().map(String::as_str), 2).await.unwrap();
    let read = backend.directories_read() - before;
    assert_eq!(page_two, paths[30..32]);
    // The root, the directories on the way to the cursor and the ones holding the next two objects. The
    // shard directories that sort before the cursor are not opened.
    assert!(read <= 8, "page 2 opened {read} directories");

    // Paging by small steps still yields everything exactly once, in order.
    assert_eq!(list_all_raw(&backend, "chunks", 7).await, paths);
}

#[tokio::test]
async fn disk_and_memory_list_in_the_same_byte_order() {
    // `a.b` sorts before `a/x` (`.` is 0x2e, `/` is 0x2f) although the directory name `a` sorts before `a.b`.
    let names = ["chunks/a.b", "chunks/a/x", "chunks/a/y/z", "chunks/a-b", "chunks/a0", "chunks/B", "chunks/é"];
    let dir = tempfile::tempdir().unwrap();
    let disk = DiskBackend::new(dir.path()).unwrap();
    let memory = MemoryBackend::new();
    for name in &names {
        disk.put_raw(name, Bytes::from_static(b"\0")).await.unwrap();
        memory.put_raw(name, Bytes::from_static(b"\0")).await.unwrap();
    }
    let mut expected: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
    expected.sort();
    for limit in [1, 2, 3, 100] {
        assert_eq!(list_all_raw(&disk, "chunks", limit).await, expected, "disk, limit {limit}");
        assert_eq!(list_all_raw(&memory, "chunks", limit).await, expected, "memory, limit {limit}");
    }
}

#[tokio::test]
async fn disk_list_skips_only_the_writers_temporary_names() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("chunks")).unwrap();
    let writer_name = format!(".tmp-{}", uuid::Uuid::now_v7());
    std::fs::write(root.join("chunks").join(&writer_name), b"in progress").unwrap();
    let foreign_names = [
        ".tmp-not-a-uuid",
        ".tmp-",
        ".tmp-0190a1b2-c3d4-7e5f-8a6b-7c8d9e0f1a2bx",
        ".tmp-0190A1B2-C3D4-7E5F-8A6B-7C8D9E0F1A2B",
        ".tmp-6ba7b810-9dad-11d1-80b4-00c04fd430c8",
    ];
    for foreign in foreign_names {
        std::fs::write(root.join("chunks").join(foreign), b"foreign").unwrap();
    }
    let store = Store::new(DiskBackend::new(&root).unwrap());
    let page = store.list(craft_cloud_store::BlobKind::Chunk, None, 20).await.unwrap();
    let mut expected: Vec<String> = foreign_names.iter().map(|name| format!("chunks/{name}")).collect();
    expected.sort();
    assert_eq!(page.foreign, expected);
    assert!(!page.foreign.iter().any(|path| path.ends_with(&writer_name)));
}

#[tokio::test]
async fn memory_list_ignores_other_namespaces() {
    let backend = MemoryBackend::new();
    let store = Store::new(backend.clone());
    let chunk = BlobKey::chunk(&ChunkId::from_bytes([0x22; 32]));
    let manifest = BlobKey::manifest(&ManifestId::from_bytes([0x33; 32]));
    for path in ["a/before", "chunks/zzz-foreign", "chunksx/sibling", "manifests/zzz-foreign", "zzz/after"] {
        backend.overwrite_raw(path, Bytes::new()).unwrap();
    }
    backend.overwrite_raw(&chunk.path(), Bytes::new()).unwrap();
    backend.overwrite_raw(&manifest.path(), Bytes::new()).unwrap();

    let first = store.list(craft_cloud_store::BlobKind::Chunk, None, 1).await.unwrap();
    assert_eq!(first.blobs, vec![chunk]);
    let second = store.list(craft_cloud_store::BlobKind::Chunk, first.next_cursor, 1).await.unwrap();
    assert_eq!(second.foreign, vec!["chunks/zzz-foreign"]);
    assert!(second.next_cursor.is_none());
    let manifests = store.list(craft_cloud_store::BlobKind::Manifest, None, 10).await.unwrap();
    assert_eq!(manifests.blobs, vec![manifest]);
    assert_eq!(manifests.foreign, vec!["manifests/zzz-foreign"]);
    // A cursor from before the namespace starts at the namespace; one past it finds nothing.
    assert_eq!(backend.list_raw("chunks", Some("a/before"), 10).await.unwrap().len(), 2);
    assert!(backend.list_raw("chunks", Some("zzz/after"), 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn list_cursor_is_checked_before_it_is_decoded() {
    let invalid = |result: Result<craft_cloud_store::ListPage, StoreError>| matches!(result, Err(StoreError::Backend(message)) if message == "invalid listing cursor");
    let store = Store::new(MemoryBackend::new());
    let kind = craft_cloud_store::BlobKind::Chunk;
    // Otherwise valid cursors (`chunks/` and then filler) that are far longer than any real object path.
    let hex = |path: &str| path.bytes().map(|byte| format!("{byte:02x}")).collect::<String>();
    let huge = format!("1:c:{}{}", hex("chunks/"), "61".repeat(4 * 1024 * 1024));
    assert!(invalid(store.list(kind, Some(ListCursor::from_token(huge)), 1).await));
    let just_over = format!("1:c:{}{}", hex("chunks/"), "61".repeat(4096 - "chunks/".len() + 1));
    assert!(invalid(store.list(kind, Some(ListCursor::from_token(just_over)), 1).await));
    let longest = format!("1:c:{}{}", hex("chunks/"), "61".repeat(4096 - "chunks/".len()));
    assert!(store.list(kind, Some(ListCursor::from_token(longest)), 1).await.is_ok());
    for garbage in
        ["", ":", "1:c:", "1:c:zz", "1:c:6", "1:c:ff", "2:c:6368756e6b732f61", "1:x:6368756e6b732f61", "1:c:6d616e69666573742f61", "not a cursor"]
    {
        assert!(invalid(store.list(kind, Some(ListCursor::from_token(garbage)), 1).await), "{garbage:?}");
    }
    // A real cursor of one kind is refused for the other kind.
    let backend = MemoryBackend::new();
    let store = Store::new(backend.clone());
    backend.overwrite_raw("chunks/a", Bytes::new()).unwrap();
    backend.overwrite_raw("chunks/b", Bytes::new()).unwrap();
    let cursor = store.list(kind, None, 1).await.unwrap().next_cursor.unwrap();
    assert!(invalid(store.list(craft_cloud_store::BlobKind::Manifest, Some(cursor.clone()), 1).await));
    assert!(store.list(kind, Some(cursor), 1).await.is_ok());
}

#[cfg(feature = "sftp")]
mod sftp {
    use std::time::Duration;

    use bytes::Bytes;
    use craft_cloud_store::sftp::{SftpBackend, SftpConfig};
    use craft_cloud_store::{Backend, BlobKind, Compression, Store, StoreError};

    use super::{chunk_key, conformance, formats, levels_and_recompress};

    /// A public key that is not the test server's.
    const OTHER_HOST_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPAV72SkMYdWcs+Y19sIQJFahyXAm1LZbNGMuu76zEl4";

    /// An Ed25519 test key made for this test and trusted nowhere.
    const UNKNOWN_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACABtJ8d7MWcrbihu4BaI7Xvh4qjXNozoWDfjPdQSki2KAAAAJiSCUw1kglM
NQAAAAtzc2gtZWQyNTUxOQAAACABtJ8d7MWcrbihu4BaI7Xvh4qjXNozoWDfjPdQSki2KA
AAAECLlfRNjLumfzdJ/Oo5S0kirR/b0tRMEW99eZQZXdP9mgG0nx3sxZytuKG7gFojte+H
iqNc2jOhYN+M91BKSLYoAAAAEGNyYWZ0LWNsb3VkLXRlc3QBAgMEBQ==
-----END OPENSSH PRIVATE KEY-----
";

    /// The settings of the test server, or `None` (a skip) when none is configured. A fresh root directory per call.
    fn config() -> Option<SftpConfig> {
        let Ok(host) = std::env::var("CRAFT_CLOUD_TEST_SFTP_HOST") else {
            assert!(
                std::env::var_os("CRAFT_CLOUD_REQUIRE_BACKENDS").is_none(),
                "CRAFT_CLOUD_REQUIRE_BACKENDS is set but CRAFT_CLOUD_TEST_SFTP_HOST is not"
            );
            eprintln!("skipped: CRAFT_CLOUD_TEST_SFTP_HOST not set");
            return None;
        };
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set with CRAFT_CLOUD_TEST_SFTP_HOST"));
        Some(SftpConfig {
            host,
            port: var("CRAFT_CLOUD_TEST_SFTP_PORT").parse().unwrap(),
            user: var("CRAFT_CLOUD_TEST_SFTP_USER"),
            root: format!("{}/test-{}", var("CRAFT_CLOUD_TEST_SFTP_ROOT").trim_end_matches('/'), uuid::Uuid::now_v7()),
            key_file: var("CRAFT_CLOUD_TEST_SFTP_KEY_FILE").into(),
            key_passphrase: None,
            host_key: Some(var("CRAFT_CLOUD_TEST_SFTP_HOST_KEY")),
            known_hosts: None,
            connections: 4,
            timeout: Duration::from_secs(20),
            retries: 2,
        })
    }

    #[tokio::test]
    async fn sftp_backend() {
        let Some(config) = config() else { return };
        let connect = || async { SftpBackend::connect(&config).await.unwrap() };
        let store = Store::new(connect().await);
        conformance(store.clone()).await;
        let raw = connect().await;
        assert_eq!(raw.kind(), "sftp");
        raw.put_raw("chunks/foreign-object", Bytes::new()).await.unwrap();
        let page = store.list(BlobKind::Chunk, None, 100).await.unwrap();
        assert_eq!(page.foreign, vec!["chunks/foreign-object"]);
        // A new backend on the same data (a restart) sees what the first wrote.
        let data = b"written before the restart";
        let key = chunk_key(data);
        store.put(key, data).await.unwrap();
        let restarted = Store::new(connect().await);
        assert_eq!(&restarted.get(key).await.unwrap()[..], data);
        formats(Store::new(connect().await), Store::new(connect().await).with_compression(Compression::None), &connect().await).await;
        levels_and_recompress(Store::new(connect().await), &connect().await).await;
    }

    #[tokio::test]
    async fn sftp_objects_replace_atomically_and_round_trip() {
        let Some(config) = config() else { return };
        let backend = SftpBackend::connect(&config).await.unwrap();
        let path = "chunks/ab/cd/replaced";
        assert_eq!(backend.get_raw(path).await.unwrap(), None);
        assert_eq!(backend.size_raw(path).await.unwrap(), None);
        backend.put_raw(path, Bytes::from_static(b"first")).await.unwrap();
        backend.put_raw(path, Bytes::from_static(b"the second one is longer")).await.unwrap();
        assert_eq!(backend.get_raw(path).await.unwrap().unwrap(), Bytes::from_static(b"the second one is longer"));
        assert_eq!(backend.size_raw(path).await.unwrap(), Some(24));
        backend.put_raw(path, Bytes::from_static(b"short")).await.unwrap();
        assert_eq!(backend.get_raw(path).await.unwrap().unwrap(), Bytes::from_static(b"short"), "a shorter replacement leaves no tail");
        // Bounded reads stop at the limit plus one byte.
        let big: Vec<u8> = (0..4 * 1024 * 1024 + 123).map(|n| u8::try_from(n % 251).unwrap()).collect();
        backend.put_raw("chunks/ab/cd/big", Bytes::from(big.clone())).await.unwrap();
        assert_eq!(backend.get_raw("chunks/ab/cd/big").await.unwrap().unwrap(), Bytes::from(big.clone()));
        assert_eq!(backend.get_raw_limited("chunks/ab/cd/big", 100).await.unwrap().unwrap().len(), 101);
        assert_eq!(backend.size_raw("chunks/ab/cd/big").await.unwrap(), Some(u64::try_from(big.len()).unwrap()));
        backend.put_raw("chunks/ab/cd/empty", Bytes::new()).await.unwrap();
        assert_eq!(backend.get_raw("chunks/ab/cd/empty").await.unwrap().unwrap().len(), 0);
        // A directory is not an object.
        assert_eq!(backend.size_raw("chunks/ab").await.unwrap(), None);
        backend.delete_raw(path).await.unwrap();
        backend.delete_raw(path).await.unwrap();
        assert_eq!(backend.get_raw(path).await.unwrap(), None);
    }

    #[tokio::test]
    async fn sftp_parallel_writes_share_a_small_pool() {
        let Some(config) = config() else { return };
        let store = Store::new(SftpBackend::connect(&config).await.unwrap());
        let mut tasks = Vec::new();
        for n in 0..24 {
            let store = store.clone();
            tasks.push(tokio::spawn(async move {
                let data = format!("parallel {n} {}", uuid::Uuid::now_v7()).into_bytes();
                let key = chunk_key(&data);
                store.put(key, &data).await.unwrap();
                assert_eq!(&store.get(key).await.unwrap()[..], &data[..]);
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn sftp_refuses_paths_outside_the_layout() {
        let Some(config) = config() else { return };
        let backend = SftpBackend::connect(&config).await.unwrap();
        for bad in ["../escape", "chunks/../../escape", "/etc/passwd", "other/x", "chunks//x", "chunks/ab\\cd"] {
            assert!(backend.put_raw(bad, Bytes::from_static(b"x")).await.is_err(), "{bad}");
            assert!(backend.get_raw(bad).await.is_err(), "{bad}");
            assert!(backend.delete_raw(bad).await.is_err(), "{bad}");
        }
        assert!(backend.list_raw("..", None, 10).await.is_err());
        assert!(backend.list_raw("", None, 10).await.is_err());
    }

    #[tokio::test]
    async fn sftp_refuses_a_host_key_that_is_not_pinned() {
        let Some(config) = config() else { return };
        for wrong in [OTHER_HOST_KEY.to_owned(), "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_owned()] {
            let wrong = SftpConfig { host_key: Some(wrong), retries: 0, ..config.clone() };
            let Err(StoreError::Backend(message)) = SftpBackend::connect(&wrong).await else { panic!("must be refused") };
            assert!(message.contains("host key") || message.contains("fingerprint"), "{message}");
        }
        // No pin at all is refused too, before any connection.
        let none = SftpConfig { host_key: None, ..config.clone() };
        let Err(StoreError::Backend(message)) = SftpBackend::connect(&none).await else { panic!("must be refused") };
        assert!(message.contains("pinned host key"), "{message}");
    }

    /// Fault test: the server restarts between two requests. Needs control of the server, so it is not part of the normal
    /// run: `CRAFT_CLOUD_TEST_SFTP_CONTAINER=<docker container name> cargo test … -- --ignored sftp_reconnects`.
    #[tokio::test]
    #[ignore = "restarts the test container named by CRAFT_CLOUD_TEST_SFTP_CONTAINER"]
    async fn sftp_reconnects_after_the_server_restarts() {
        let Some(config) = config() else { return };
        let container = std::env::var("CRAFT_CLOUD_TEST_SFTP_CONTAINER").expect("CRAFT_CLOUD_TEST_SFTP_CONTAINER names the container to restart");
        let store = Store::new(SftpBackend::connect(&SftpConfig { retries: 8, ..config }).await.unwrap());
        let data = b"before the restart";
        let key = chunk_key(data);
        store.put(key, data).await.unwrap();
        assert_eq!(&store.get(key).await.unwrap()[..], data, "a pooled connection is open now");
        let status = std::process::Command::new("docker").args(["restart", "-t", "1", &container]).status().unwrap();
        assert!(status.success());
        // The pooled connections are dead; the request retries on a fresh one, with backoff, until the server is back.
        assert_eq!(&store.get(key).await.unwrap()[..], data);
        let after = b"after the restart";
        let key = chunk_key(after);
        store.put(key, after).await.unwrap();
        assert_eq!(&store.get(key).await.unwrap()[..], after);
    }

    #[tokio::test]
    async fn sftp_listing_resumes_after_a_foreign_name_with_odd_characters() {
        let Some(config) = config() else { return };
        let backend = SftpBackend::connect(&config).await.unwrap();
        backend.put_raw("chunks/ab/cd/plain", Bytes::from_static(b"x")).await.unwrap();
        // A cursor is whatever name an earlier page ended with, so it may hold a backslash or a control character (a
        // foreign file can be named so); it is only compared with names. NUL and absurd sizes are still refused.
        let page = backend.list_raw("chunks", Some("chunks/ab/cd/we\\ird\tname"), 10).await.unwrap();
        assert!(page.is_empty() || page.iter().all(|p| p.as_str() > "chunks/ab/cd/we\\ird\tname"));
        assert!(backend.list_raw("chunks", Some("chunks/ab/\0"), 10).await.is_err());
        assert!(backend.list_raw("chunks", Some(&format!("chunks/{}", "a".repeat(5000))), 10).await.is_err());
        assert!(backend.list_raw("chunks", Some(""), 10).await.is_err());
    }

    #[tokio::test]
    async fn sftp_sweeps_only_old_temporary_files_of_its_own_form() {
        use uuid::{ContextV7, Timestamp, Uuid};
        let Some(config) = config() else { return };
        let backend = SftpBackend::connect(&config).await.unwrap();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let at = |secs: u64| Uuid::new_v7(Timestamp::from_unix(ContextV7::new(), secs, 0));
        let old = format!("chunks/ab/cd/.tmp-{}", at(now - 3 * 3600).hyphenated());
        let fresh = format!("chunks/ab/cd/.tmp-{}", at(now).hyphenated());
        let foreign = "chunks/ab/cd/.tmp-not-ours";
        for path in [old.as_str(), fresh.as_str(), foreign] {
            backend.put_raw(path, Bytes::from_static(b"left behind")).await.unwrap();
        }
        // The listing hides the backend's own temporary names, so nothing else would ever find these.
        let listed = backend.list_raw("chunks", None, 100).await.unwrap();
        assert!(listed.contains(&foreign.to_owned()) && !listed.contains(&old) && !listed.contains(&fresh), "{listed:?}");
        let report = backend.remove_stale_temp("chunks", Duration::from_secs(3600)).await.unwrap();
        assert_eq!((report.removed, report.younger, report.foreign), (1, 1, 1), "{report:?}");
        assert_eq!(backend.size_raw(&old).await.unwrap(), None);
        assert_eq!(backend.size_raw(&fresh).await.unwrap(), Some(11));
        assert_eq!(backend.size_raw(foreign).await.unwrap(), Some(11), "a name that is not exactly ours is somebody else's file");
        let again = backend.remove_stale_temp("chunks", Duration::from_secs(3600)).await.unwrap();
        assert_eq!((again.removed, again.younger, again.foreign), (0, 1, 1), "{again:?}");
        assert!(backend.remove_stale_temp("", Duration::from_secs(3600)).await.is_err(), "the blob store names chunks or manifests");
    }

    /// The temporary files of a killed server are made by the server, but an operator's tools (or a test) may make them in any
    /// directory and at any time: several in one directory, next to a real object, in a directory that is not a shard name.
    #[tokio::test]
    #[ignore = "plants files in the test container named by CRAFT_CLOUD_TEST_SFTP_CONTAINER"]
    async fn sftp_sweeps_every_old_temporary_file_in_any_directory() {
        use uuid::{ContextV7, Timestamp, Uuid};
        let Some(config) = config() else { return };
        let container = std::env::var("CRAFT_CLOUD_TEST_SFTP_CONTAINER").expect("CRAFT_CLOUD_TEST_SFTP_CONTAINER names the container");
        let backend = SftpBackend::connect(&SftpConfig { retries: 0, ..config.clone() }).await.unwrap();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let at = |secs: u64| format!(".tmp-{}", Uuid::new_v7(Timestamp::from_unix(ContextV7::new(), secs, 0)).hyphenated());
        let dirs = ["chunks/24/3f", "chunks/ab/g1", "chunks/ab/g2", "chunks/cd/ef", "manifests/00/01"];
        let mut old = Vec::new();
        for dir in dirs {
            // The real object first, so that the directory has been listed with it, then three temporary files and a stranger.
            backend
                .put_raw(&format!("{dir}/243fa556f943c64a98c42711dea2cd8fbcbf0f9c5ea8c71dee4ff19dab5cf897"), Bytes::from_static(b"real"))
                .await
                .unwrap();
            let (stale, half_an_hour, fresh) = (at(now - 2 * 86_400), at(now - 1800), at(now));
            let path = format!("{}/{dir}", config.root);
            let script =
                format!("cd {path} && touch {stale} {half_an_hour} {fresh} .tmp-notauuid && chmod 644 {stale} {half_an_hour} {fresh} .tmp-notauuid");
            let status = std::process::Command::new("docker").args(["exec", "-u", "craft", &container, "sh", "-c", &script]).status().unwrap();
            assert!(status.success());
            old.push(format!("{dir}/{stale}"));
        }
        // A leftover that is a link (by an operator's hand) has our exact name: it is removed as the link it is.
        backend.put_raw("chunks/ab/g9/seed", Bytes::from_static(b"x")).await.unwrap();
        let link = format!("{}/chunks/ab/g9/{}", config.root, at(now - 2 * 86_400 - 5));
        let made =
            std::process::Command::new("docker").args(["exec", "-u", "craft", &container, "ln", "-s", "/etc/hostname", &link]).status().unwrap();
        assert!(made.success());
        let removed = backend.remove_stale_temp("chunks", Duration::from_secs(3600)).await.unwrap().removed
            + backend.remove_stale_temp("manifests", Duration::from_secs(3600)).await.unwrap().removed;
        let left: Vec<_> = {
            let mut left = Vec::new();
            for path in &old {
                if backend.size_raw(path).await.unwrap().is_some() {
                    left.push(path.clone());
                }
            }
            left
        };
        assert!(left.is_empty(), "removed {removed}, still there: {left:?}");
        assert_eq!(removed, 6, "five old files and the link");
        let still = std::process::Command::new("docker").args(["exec", "-u", "craft", &container, "test", "-L", &link]).status().unwrap();
        assert!(!still.success(), "the link is gone");
    }

    #[tokio::test]
    async fn sftp_refuses_to_download_more_than_was_expected() {
        let Some(config) = config() else { return };
        let backend = SftpBackend::connect_for_backup(&config).await.unwrap();
        let data = vec![7u8; 1_000_000];
        backend.put_raw("generations/x/blob", Bytes::from(data.clone())).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out");
        let refused = backend.get_file_raw("generations/x/blob", &target, 999_999).await;
        assert!(matches!(&refused, Err(StoreError::Backend(m)) if m.contains("more than")), "{refused:?}");
        assert!(!target.exists() && !dir.path().join("out.part").exists(), "nothing is written for a refused download");
        assert!(backend.get_file_raw("generations/x/blob", &target, 1_000_000).await.unwrap());
        assert_eq!(std::fs::read(&target).unwrap(), data);
        assert!(!backend.get_file_raw("generations/x/missing", &dir.path().join("none"), 10).await.unwrap());
    }

    /// Fault test: a pooled connection died while it was idle, and `sftp_retries` is 0. The request still succeeds, because a
    /// connection that was taken from the pool and found dead costs a fresh one, not a retry.
    #[tokio::test]
    #[ignore = "restarts the test container named by CRAFT_CLOUD_TEST_SFTP_CONTAINER"]
    async fn sftp_a_dead_pooled_connection_costs_no_retry() {
        let Some(config) = config() else { return };
        let container = std::env::var("CRAFT_CLOUD_TEST_SFTP_CONTAINER").expect("CRAFT_CLOUD_TEST_SFTP_CONTAINER names the container to restart");
        let addr = format!("{}:{}", config.host, config.port);
        let store = Store::new(SftpBackend::connect(&SftpConfig { retries: 0, connections: 4, ..config }).await.unwrap());
        let data = b"read through a connection that will die";
        let key = chunk_key(data);
        store.put(key, data).await.unwrap();
        // Four requests at once leave several pooled connections behind, all of which die with the server. The one request
        // after the restart must not run into a second dead one on its free recovery attempt.
        let (a, b, c, d) = tokio::join!(store.get(key), store.get(key), store.get(key), store.get(key));
        for read in [a, b, c, d] {
            assert_eq!(&read.unwrap()[..], data);
        }
        let status = std::process::Command::new("docker").args(["restart", "-t", "1", &container]).status().unwrap();
        assert!(status.success());
        // Wait until the server answers again, so that the only thing in the way is the dead connection in the pool.
        for _ in 0..60 {
            if tokio::net::TcpStream::connect(&addr).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(&store.get(key).await.unwrap()[..], data);
    }

    #[tokio::test]
    async fn sftp_refuses_a_key_the_server_does_not_know() {
        let Some(config) = config() else { return };
        let dir = tempfile::tempdir().unwrap();
        let key_file = dir.path().join("other-key");
        std::fs::write(&key_file, UNKNOWN_KEY).unwrap();
        let other = SftpConfig { key_file, retries: 0, ..config };
        assert!(SftpBackend::connect(&other).await.is_err());
    }
}
