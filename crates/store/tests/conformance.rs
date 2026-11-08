//! The same behaviour from every backend. Memory and disk always run; S3 runs when
//! `CRAFT_CLOUD_TEST_S3_ENDPOINT` is set (with `_BUCKET`, `_ACCESS_KEY`, `_SECRET_KEY`), e.g. against a local
//! SeaweedFS container (Apache-2.0; run as a separate process, never linked):
//! `docker run -p 8333:8333 chrislusf/seaweedfs server -s3` then `curl -X PUT http://127.0.0.1:8333/cc-test`. CI sets `CRAFT_CLOUD_REQUIRE_BACKENDS`, which turns a
//! missing S3 configuration into a failure instead of a skip.

use bytes::Bytes;
use craft_cloud_proto::limits::{MAX_CHUNK_BYTES, MAX_MANIFEST_BYTES};
use craft_cloud_proto::{ChunkId, ManifestId};
use craft_cloud_store::disk::DiskBackend;
use craft_cloud_store::memory::MemoryBackend;
use craft_cloud_store::s3::{S3Backend, S3Config};
use craft_cloud_store::{Backend, BlobKey, Compression, Store, StoreError};

fn chunk_key(data: &[u8]) -> BlobKey {
    BlobKey::chunk(&ChunkId::from_bytes(*blake3::hash(data).as_bytes()))
}

async fn conformance(store: Store) {
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
        prefix: format!("test-{}", uuid::Uuid::now_v7()),
        region: "us-east-1".into(),
        endpoint: Some(endpoint),
        access_key_id: var("CRAFT_CLOUD_TEST_S3_ACCESS_KEY"),
        secret_access_key: var("CRAFT_CLOUD_TEST_S3_SECRET_KEY"),
        allow_http: true,
    };
    conformance(Store::new(S3Backend::new(&config).unwrap())).await;
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
