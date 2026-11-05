//! The same behaviour from every backend. Memory and disk always run; S3 runs when
//! `CRAFT_CLOUD_TEST_S3_ENDPOINT` is set (with `_BUCKET`, `_ACCESS_KEY`, `_SECRET_KEY`), e.g. against a local
//! SeaweedFS container (Apache-2.0; run as a separate process, never linked):
//! `docker run -p 8333:8333 chrislusf/seaweedfs server -s3` then `curl -X PUT http://127.0.0.1:8333/cc-test`. CI sets `CRAFT_CLOUD_REQUIRE_BACKENDS`, which turns a
//! missing S3 configuration into a failure instead of a skip.

use bytes::Bytes;
use craft_cloud_proto::limits::MAX_CHUNK_BYTES;
use craft_cloud_proto::{ChunkId, ManifestId};
use craft_cloud_store::disk::DiskBackend;
use craft_cloud_store::memory::MemoryBackend;
use craft_cloud_store::s3::{S3Backend, S3Config};
use craft_cloud_store::{Backend, BlobKey, ListCursor, MAX_LIST_PAGE_SIZE, Store, StoreError};

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

#[tokio::test]
async fn memory_backend() {
    conformance(Store::new(MemoryBackend::new())).await;
}

#[tokio::test]
async fn disk_backend() {
    let dir = tempfile::tempdir().unwrap();
    conformance(Store::new(DiskBackend::new(dir.path()).unwrap())).await;
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
    std::fs::write(root.join("chunks").join(name), b"foreign").unwrap();
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
