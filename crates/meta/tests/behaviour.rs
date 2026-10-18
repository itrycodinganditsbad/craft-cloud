//! The same scenarios on every backend. SQLite always runs (a fresh temporary file per test). Postgres runs
//! when `CRAFT_CLOUD_TEST_PG` holds an admin URL, e.g. `postgres://postgres:pw@127.0.0.1:5432/postgres`; each
//! test creates and drops its own database. CI sets `CRAFT_CLOUD_REQUIRE_BACKENDS`, which turns a missing
//! `CRAFT_CLOUD_TEST_PG` into a failure instead of a skip.

use craft_cloud_meta::{BlobKind, Meta, MetaError, Role, UserRecord};
use craft_cloud_proto::limits::MAX_PAGE_NODES;
use craft_cloud_proto::{
    ChunkId, ChunkRef, ChunkerParamsId, ConflictKind, DeviceId, FileHash, Manifest, ManifestId, Name, NewVersion, NodeId, NodeKind, NodeState, Op,
    VersionId,
};

struct Db {
    meta: Meta,
    _guard: Guard,
}

enum Guard {
    Sqlite(#[allow(dead_code)] tempfile::TempDir),
    Postgres { admin_url: String, name: String },
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Guard::Postgres { admin_url, name } = self {
            let (admin_url, name) = (admin_url.clone(), name.clone());
            // Drop the test database from a separate runtime; the test's runtime may be gone.
            let _ = std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                rt.block_on(async {
                    use sqlx::Connection;
                    if let Ok(mut conn) = sqlx::PgConnection::connect(&admin_url).await {
                        let _ = sqlx::query(sqlx::AssertSqlSafe(format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))).execute(&mut conn).await;
                    }
                });
            })
            .join();
        }
    }
}

async fn sqlite() -> Db {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}", dir.path().join("meta.db").display());
    Db { meta: Meta::open(&url).await.unwrap(), _guard: Guard::Sqlite(dir) }
}

/// A fresh, empty Postgres database: its URL, and the guard that drops it. The guard exists before the
/// caller opens the database, so the database is dropped even when opening it fails.
async fn postgres_database() -> Option<(String, Guard)> {
    let Ok(admin_url) = std::env::var("CRAFT_CLOUD_TEST_PG") else {
        assert!(std::env::var_os("CRAFT_CLOUD_REQUIRE_BACKENDS").is_none(), "CRAFT_CLOUD_REQUIRE_BACKENDS is set but CRAFT_CLOUD_TEST_PG is not");
        return None;
    };
    use sqlx::Connection;
    let name = format!("cc_test_{}", uuid::Uuid::now_v7().simple());
    let mut conn = sqlx::PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}"))).execute(&mut conn).await.unwrap();
    let (base, _) = admin_url.rsplit_once('/').unwrap();
    let url = format!("{base}/{name}");
    Some((url, Guard::Postgres { admin_url, name }))
}

async fn postgres() -> Option<Db> {
    let (url, guard) = postgres_database().await?;
    Some(Db { meta: Meta::open(&url).await.unwrap(), _guard: guard })
}

macro_rules! on_every_backend {
    ($($scenario:ident),* $(,)?) => {
        mod on_sqlite {
            $( #[tokio::test(flavor = "multi_thread", worker_threads = 4)] async fn $scenario() { let db = super::sqlite().await; super::$scenario(&db.meta).await; } )*
        }
        mod on_postgres {
            $( #[tokio::test(flavor = "multi_thread", worker_threads = 4)] async fn $scenario() {
                let Some(db) = super::postgres().await else { eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set"); return };
                super::$scenario(&db.meta).await;
            } )*
        }
    };
}

on_every_backend!(
    users_and_devices,
    a_new_space_has_only_its_root,
    manifests_need_their_chunks_and_are_per_space,
    stored_bytes_are_counted_once_per_space_and_capped,
    concurrent_chunk_batches_charge_a_shared_chunk_once,
    concurrent_signups_with_one_name_give_username_taken,
    an_offline_batch_creates_a_folder_and_files_in_it,
    updates_need_the_current_version,
    names_collide_by_case_fold,
    a_refused_batch_lists_every_conflict_and_changes_nothing,
    moves_and_renames,
    deletes,
    an_edit_brings_back_a_deleted_file_and_its_folders,
    a_restore_is_blocked_when_the_name_was_taken,
    a_version_needs_a_manifest_from_its_own_space,
    ids_are_never_reused,
    the_change_feed_and_epochs,
    concurrent_commits_on_one_file_let_exactly_one_win,
    batch_size_is_limited,
    folders_nest_at_most_max_tree_depth,
    creating_or_moving_into_a_deleted_folder_brings_it_back,
    names_can_be_juggled_within_one_batch,
    a_failed_restore_walk_is_not_repeated_for_every_operation,
    big_manifests_are_checked_in_batches,
    the_tree_and_the_feed_come_in_pages,
    usernames_are_unique_ignoring_ascii_case,
    all_spaces_lists_every_users_space,
    live_documents_number_their_updates_per_space,
    concurrent_live_appends_get_one_number_each,
    concurrent_live_creates_make_one_document,
    live_pages_stop_at_their_byte_limit,
    a_live_document_is_capped_by_bytes,
    live_documents_holding_updates_are_capped,
    live_document_rows_are_capped,
);

// ---- helpers ----

const NOW: i64 = 1_760_000_000_000;

async fn user(meta: &Meta, username: &str) -> UserRecord {
    meta.create_user(username, "$argon2id$stub", Role::Member, NOW).await.unwrap()
}

async fn root(meta: &Meta, u: &UserRecord) -> NodeId {
    meta.space_info(u.space).await.unwrap().root
}

fn name(s: &str) -> Name {
    Name::new(s).unwrap()
}

async fn device(meta: &Meta, u: &UserRecord) -> DeviceId {
    let token = *blake3::hash(uuid::Uuid::now_v7().as_bytes()).as_bytes();
    meta.create_device(u.id, "laptop", &token, NOW).await.unwrap()
}

/// Uploads one random chunk and a manifest for it to the user's space; returns the manifest ID.
async fn manifest(meta: &Meta, u: &UserRecord, len: u32) -> ManifestId {
    let chunk = ChunkId::from_bytes(*blake3::hash(uuid::Uuid::now_v7().as_bytes()).as_bytes());
    meta.add_chunk(u.space, &chunk, u64::from(len) + 1, NOW).await.unwrap();
    let m =
        Manifest::new(ChunkerParamsId::new("fastcdc-v1").unwrap(), u64::from(len), FileHash::from_bytes([1; 32]), vec![ChunkRef { id: chunk, len }])
            .unwrap();
    meta.add_manifest(u.space, &m, 100, NOW).await.unwrap();
    m.id()
}

fn version(manifest: ManifestId) -> NewVersion {
    NewVersion { id: VersionId::generate(), manifest, mtime_ms: NOW, exec: false }
}

async fn full_tree(meta: &Meta, space: craft_cloud_proto::SpaceId) -> craft_cloud_proto::ChangeSet {
    meta.tree(space, None, MAX_PAGE_NODES).await.unwrap()
}

fn find(nodes: &[NodeState], id: NodeId) -> &NodeState {
    nodes.iter().find(|n| n.id == id).unwrap()
}

fn conflicts(result: Result<craft_cloud_meta::CommitOutcome, MetaError>) -> Vec<(usize, ConflictKind)> {
    match result {
        Err(MetaError::Rejected(list)) => list.into_iter().map(|c| (c.op, c.kind)).collect(),
        other => panic!("expected a refused batch, got {other:?}"),
    }
}

// ---- scenarios ----

async fn users_and_devices(meta: &Meta) {
    let ana = user(meta, "ana").await;
    assert!(matches!(meta.create_user("ana", "x", Role::Admin, NOW).await, Err(MetaError::UsernameTaken)));
    assert!(matches!(meta.create_user("bad name", "x", Role::Admin, NOW).await, Err(MetaError::Invalid(_))));
    assert_eq!(meta.user_by_name("ana").await.unwrap(), Some(ana.clone()));
    assert_eq!(meta.user_by_name("bob").await.unwrap(), None);

    meta.set_password_hash(ana.id, "$argon2id$new").await.unwrap();
    assert_eq!(meta.user_by_name("ana").await.unwrap().unwrap().password_hash, "$argon2id$new");

    let token = [7u8; 32];
    let dev = meta.create_device(ana.id, "studio pc", &token, NOW).await.unwrap();
    let auth = meta.device_by_token(&token).await.unwrap().unwrap();
    assert_eq!((auth.device, auth.user, auth.space, auth.role), (dev, ana.id, ana.space, Role::Member));
    assert_eq!(auth.username, "ana");
    assert_eq!(meta.device_by_token(&[8u8; 32]).await.unwrap(), None);

    meta.touch_device(dev, NOW + 5).await.unwrap();
    let list = meta.list_devices(ana.id).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!((list[0].name.as_str(), list[0].last_used_ms, list[0].revoked), ("studio pc", NOW + 5, false));

    let bob = user(meta, "bob").await;
    assert!(matches!(meta.revoke_device(bob.id, dev).await, Err(MetaError::NotFound(_))));
    meta.revoke_device(ana.id, dev).await.unwrap();
    assert_eq!(meta.device_by_token(&token).await.unwrap(), None);
    assert!(meta.list_devices(ana.id).await.unwrap()[0].revoked);
}

async fn a_new_space_has_only_its_root(meta: &Meta) {
    let u = user(meta, "ana").await;
    let tree = full_tree(meta, u.space).await;
    assert_eq!(tree.seq, 0);
    assert_eq!(tree.nodes.len(), 1);
    let r = &tree.nodes[0];
    assert_eq!((r.parent, r.name.clone(), r.kind.clone(), r.deleted), (None, None, NodeKind::Dir, false));
    assert_eq!(r.id, root(meta, &u).await);
}

async fn manifests_need_their_chunks_and_are_per_space(meta: &Meta) {
    let ana = user(meta, "ana").await;
    let bob = user(meta, "bob").await;
    let chunk = ChunkId::from_bytes([9; 32]);
    assert_eq!(meta.missing_chunks(ana.space, &[chunk, chunk]).await.unwrap(), vec![chunk]);
    let m =
        Manifest::new(ChunkerParamsId::new("fastcdc-v1").unwrap(), 5, FileHash::from_bytes([0; 32]), vec![ChunkRef { id: chunk, len: 5 }]).unwrap();
    assert!(matches!(meta.add_manifest(ana.space, &m, 77, NOW).await, Err(MetaError::MissingChunks(list)) if list == vec![chunk]));

    meta.add_chunk(ana.space, &chunk, 6, NOW).await.unwrap();
    meta.add_chunk(ana.space, &chunk, 6, NOW).await.unwrap();
    assert!(meta.missing_chunks(ana.space, &[chunk]).await.unwrap().is_empty());
    // Bob can't learn that Ana has this chunk.
    assert_eq!(meta.missing_chunks(bob.space, &[chunk]).await.unwrap(), vec![chunk]);

    meta.add_manifest(ana.space, &m, 77, NOW).await.unwrap();
    assert!(meta.has_manifest(ana.space, &m.id()).await.unwrap());
    assert!(!meta.has_manifest(bob.space, &m.id()).await.unwrap());
}

fn random_chunk() -> ChunkId {
    ChunkId::from_bytes(*blake3::hash(uuid::Uuid::now_v7().as_bytes()).as_bytes())
}

async fn stored_bytes_are_counted_once_per_space_and_capped(meta: &Meta) {
    let ana = user(meta, "ana").await;
    let bob = user(meta, "bob").await;
    let capped = meta.clone().with_stored_bytes_cap(Some(100));
    let (c1, c2, c3) = (random_chunk(), random_chunk(), random_chunk());
    capped.add_chunk(ana.space, &c1, 60, NOW).await.unwrap();
    // Again: nothing new to count.
    capped.add_chunk(ana.space, &c1, 60, NOW).await.unwrap();
    assert_eq!(meta.stored_bytes(ana.space).await.unwrap(), 60);

    // Over the cap: refused, and nothing recorded.
    let over = capped.add_chunk(ana.space, &c2, 50, NOW).await;
    assert!(matches!(over, Err(MetaError::QuotaExceeded { used: 60, limit: 100 })), "{over:?}");
    assert_eq!(meta.missing_chunks(ana.space, &[c2]).await.unwrap(), vec![c2]);
    assert_eq!(meta.stored_bytes(ana.space).await.unwrap(), 60);
    // The look before storing says the same, and lets through what the space has or what fits.
    assert!(matches!(capped.check_room(ana.space, BlobKind::Chunk, c2.as_bytes(), 50).await, Err(MetaError::QuotaExceeded { .. })));
    capped.check_room(ana.space, BlobKind::Chunk, c1.as_bytes(), 60).await.unwrap();
    capped.check_room(ana.space, BlobKind::Chunk, c3.as_bytes(), 40).await.unwrap();
    // Without a cap there is nothing to check.
    meta.check_room(ana.space, BlobKind::Chunk, c2.as_bytes(), u64::MAX).await.unwrap();

    // Each space counts its own; a blob another space stored first counts at the size it is stored as.
    capped.add_chunk(bob.space, &c2, 50, NOW).await.unwrap();
    capped.add_chunks(bob.space, &[(c3, 30), (c3, 30)], NOW).await.unwrap();
    assert_eq!(meta.stored_bytes(bob.space).await.unwrap(), 80);
    let over = capped.add_chunks(bob.space, &[(c1, 60)], NOW).await;
    assert!(matches!(over, Err(MetaError::QuotaExceeded { used: 80, limit: 100 })), "{over:?}");
    meta.add_chunk(bob.space, &c1, 999, NOW).await.unwrap();
    assert_eq!(meta.stored_bytes(bob.space).await.unwrap(), 140, "c1 counts as Ana stored it");

    // Manifests count too.
    let m = Manifest::new(ChunkerParamsId::new("fastcdc-v1").unwrap(), 5, FileHash::from_bytes([0; 32]), vec![ChunkRef { id: c1, len: 5 }]).unwrap();
    capped.add_manifest(ana.space, &m, 30, NOW).await.unwrap();
    assert_eq!(meta.stored_bytes(ana.space).await.unwrap(), 90);
    let m2 = Manifest::new(ChunkerParamsId::new("fastcdc-v1").unwrap(), 6, FileHash::from_bytes([1; 32]), vec![ChunkRef { id: c1, len: 6 }]).unwrap();
    assert!(matches!(capped.add_manifest(ana.space, &m2, 30, NOW).await, Err(MetaError::QuotaExceeded { used: 90, limit: 100 })));
    assert!(!meta.has_manifest(ana.space, &m2.id()).await.unwrap());
}

/// Review finding 3 of M3c: two batches racing with an overlapping chunk charge it once (on Postgres both used to
/// see it as new before inserting). SQLite runs one write at a time, so there it pins the result.
async fn concurrent_chunk_batches_charge_a_shared_chunk_once(meta: &Meta) {
    let ana = user(meta, "ana").await;
    let shared: Vec<(ChunkId, u64)> = (0..50).map(|_| (random_chunk(), 10)).collect();
    let tasks: Vec<_> = (0..4)
        .map(|_| {
            let (meta, mut batch) = (meta.clone(), shared.clone());
            // Each batch has the shared chunks and one of its own, in another order.
            batch.reverse();
            batch.push((random_chunk(), 1));
            tokio::spawn(async move { meta.add_chunks(ana.space, &batch, NOW).await })
        })
        .collect();
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert_eq!(meta.stored_bytes(ana.space).await.unwrap(), 50 * 10 + 4);
    // Again, with duplicates in one batch: nothing new.
    let again: Vec<(ChunkId, u64)> = shared.iter().chain(&shared).copied().collect();
    meta.add_chunks(ana.space, &again, NOW).await.unwrap();
    assert_eq!(meta.stored_bytes(ana.space).await.unwrap(), 50 * 10 + 4);
}

async fn concurrent_signups_with_one_name_give_username_taken(meta: &Meta) {
    let tasks: Vec<_> = (0..6)
        .map(|_| {
            let meta = meta.clone();
            tokio::spawn(async move { meta.create_user("ana", "x", Role::Member, NOW).await })
        })
        .collect();
    let mut created = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => created += 1,
            Err(MetaError::UsernameTaken) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
    assert_eq!(created, 1);
}

async fn an_offline_batch_creates_a_folder_and_files_in_it(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    let m = manifest(meta, &u, 42).await;
    let (shots, a, b) = (NodeId::generate(), NodeId::generate(), NodeId::generate());
    let ops = vec![
        Op::Mkdir { node: shots, parent: r, name: name("Shots") },
        Op::Create { node: a, parent: shots, name: name("010.psd"), version: version(m) },
        Op::Create { node: b, parent: shots, name: name("020.psd"), version: version(m) },
    ];
    let out = meta.commit(u.space, dev, &ops, NOW).await.unwrap();
    assert_eq!(out.seq, 1);
    let tree = full_tree(meta, u.space).await;
    assert_eq!((tree.seq, tree.nodes.len()), (1, 4));
    let file = find(&tree.nodes, a);
    assert_eq!((file.parent, file.name.clone()), (Some(shots), Some(name("010.psd"))));
    let NodeKind::File(state) = &file.kind else { panic!("not a file") };
    assert_eq!((state.manifest, state.size, state.device, state.epoch), (m, 42, dev, out.epoch));
}

async fn updates_need_the_current_version(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    let (m1, m2) = (manifest(meta, &u, 1).await, manifest(meta, &u, 2).await);
    let (file, v1) = (NodeId::generate(), version(m1));
    meta.commit(u.space, dev, &[Op::Create { node: file, parent: r, name: name("a.txt"), version: v1.clone() }], NOW).await.unwrap();

    let v2 = version(m2);
    meta.commit(u.space, dev, &[Op::Update { node: file, expected: v1.id, version: v2.clone() }], NOW).await.unwrap();
    // A second device still thinks v1 is current.
    let stale = meta.commit(u.space, dev, &[Op::Update { node: file, expected: v1.id, version: version(m1) }], NOW).await;
    assert_eq!(conflicts(stale), vec![(0, ConflictKind::VersionMismatch { current: Some(v2.id) })]);
    let tree = full_tree(meta, u.space).await;
    let NodeKind::File(state) = &find(&tree.nodes, file).kind else { panic!("not a file") };
    assert_eq!((state.version, state.size), (v2.id, 2));
}

async fn names_collide_by_case_fold(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    let m = manifest(meta, &u, 1).await;
    let first = NodeId::generate();
    meta.commit(u.space, dev, &[Op::Create { node: first, parent: r, name: name("Straße.txt"), version: version(m) }], NOW).await.unwrap();
    let second =
        meta.commit(u.space, dev, &[Op::Create { node: NodeId::generate(), parent: r, name: name("STRASSE.TXT"), version: version(m) }], NOW).await;
    assert_eq!(conflicts(second), vec![(0, ConflictKind::NameTaken { existing: first })]);
    // Same name in another folder is fine.
    let dir = NodeId::generate();
    meta.commit(
        u.space,
        dev,
        &[
            Op::Mkdir { node: dir, parent: r, name: name("sub") },
            Op::Create { node: NodeId::generate(), parent: dir, name: name("straße.txt"), version: version(m) },
        ],
        NOW,
    )
    .await
    .unwrap();
}

async fn a_refused_batch_lists_every_conflict_and_changes_nothing(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    let m = manifest(meta, &u, 1).await;
    let ok_dir = NodeId::generate();
    let ops = vec![
        Op::Mkdir { node: ok_dir, parent: r, name: name("fine") },
        Op::Mkdir { node: NodeId::generate(), parent: NodeId::generate(), name: name("orphan") },
        Op::Update { node: NodeId::generate(), expected: VersionId::generate(), version: version(m) },
    ];
    assert_eq!(conflicts(meta.commit(u.space, dev, &ops, NOW).await), vec![(1, ConflictKind::NoSuchParent), (2, ConflictKind::NoSuchNode)]);
    let tree = full_tree(meta, u.space).await;
    assert_eq!((tree.seq, tree.nodes.len()), (0, 1));
}

async fn moves_and_renames(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    let (a, b, f) = (NodeId::generate(), NodeId::generate(), NodeId::generate());
    let m = manifest(meta, &u, 1).await;
    meta.commit(
        u.space,
        dev,
        &[
            Op::Mkdir { node: a, parent: r, name: name("A") },
            Op::Mkdir { node: b, parent: a, name: name("B") },
            Op::Create { node: f, parent: r, name: name("f.txt"), version: version(m) },
        ],
        NOW,
    )
    .await
    .unwrap();

    // Case-only rename and a move into a folder.
    meta.commit(u.space, dev, &[Op::Move { node: f, from_parent: r, from_name: name("f.txt"), parent: r, name: name("F.txt") }], NOW).await.unwrap();
    meta.commit(u.space, dev, &[Op::Move { node: f, from_parent: r, from_name: name("F.txt"), parent: b, name: name("F.txt") }], NOW).await.unwrap();
    let tree = full_tree(meta, u.space).await;
    assert_eq!((find(&tree.nodes, f).parent, find(&tree.nodes, f).name.clone()), (Some(b), Some(name("F.txt"))));

    // A move from a stale location.
    let stale = meta.commit(u.space, dev, &[Op::Move { node: f, from_parent: r, from_name: name("F.txt"), parent: a, name: name("x") }], NOW).await;
    assert_eq!(conflicts(stale), vec![(0, ConflictKind::Moved { parent: Some(b), name: Some(name("F.txt")) })]);
    // A into its own child, and into itself.
    let cycle = meta.commit(u.space, dev, &[Op::Move { node: a, from_parent: r, from_name: name("A"), parent: b, name: name("A") }], NOW).await;
    assert_eq!(conflicts(cycle), vec![(0, ConflictKind::Cycle)]);
    let into_self = meta.commit(u.space, dev, &[Op::Move { node: a, from_parent: r, from_name: name("A"), parent: a, name: name("A") }], NOW).await;
    assert_eq!(conflicts(into_self), vec![(0, ConflictKind::Cycle)]);
    // The root stays put; a file can't be a parent.
    let root_move = meta.commit(u.space, dev, &[Op::Move { node: r, from_parent: r, from_name: name("x"), parent: a, name: name("x") }], NOW).await;
    assert_eq!(conflicts(root_move), vec![(0, ConflictKind::Root)]);
    let into_file = meta.commit(u.space, dev, &[Op::Move { node: b, from_parent: a, from_name: name("B"), parent: f, name: name("B") }], NOW).await;
    assert_eq!(conflicts(into_file), vec![(0, ConflictKind::NotADirectory)]);
}

async fn deletes(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    let m = manifest(meta, &u, 1).await;
    let (dir, file, v) = (NodeId::generate(), NodeId::generate(), version(m));
    meta.commit(
        u.space,
        dev,
        &[Op::Mkdir { node: dir, parent: r, name: name("d") }, Op::Create { node: file, parent: dir, name: name("f"), version: v.clone() }],
        NOW,
    )
    .await
    .unwrap();

    assert_eq!(conflicts(meta.commit(u.space, dev, &[Op::Delete { node: dir, expected: None }], NOW).await), vec![(0, ConflictKind::NotEmpty)]);
    assert_eq!(conflicts(meta.commit(u.space, dev, &[Op::Delete { node: file, expected: None }], NOW).await), vec![(0, ConflictKind::WrongKind)]);
    assert_eq!(conflicts(meta.commit(u.space, dev, &[Op::Delete { node: r, expected: None }], NOW).await), vec![(0, ConflictKind::Root)]);

    let out =
        meta.commit(u.space, dev, &[Op::Delete { node: file, expected: Some(v.id) }, Op::Delete { node: dir, expected: None }], NOW).await.unwrap();
    assert_eq!(full_tree(meta, u.space).await.nodes.len(), 1);
    let feed = meta.changes(u.space, out.seq - 1, None, MAX_PAGE_NODES).await.unwrap();
    assert!(feed.nodes.iter().all(|n| n.deleted) && feed.nodes.len() == 2);
    // Deleting what is already deleted is not a conflict.
    meta.commit(u.space, dev, &[Op::Delete { node: file, expected: Some(v.id) }], NOW).await.unwrap();
}

async fn an_edit_brings_back_a_deleted_file_and_its_folders(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    let m = manifest(meta, &u, 1).await;
    let (outer, inner, file, v1) = (NodeId::generate(), NodeId::generate(), NodeId::generate(), version(m));
    meta.commit(
        u.space,
        dev,
        &[
            Op::Mkdir { node: outer, parent: r, name: name("outer") },
            Op::Mkdir { node: inner, parent: outer, name: name("inner") },
            Op::Create { node: file, parent: inner, name: name("f"), version: v1.clone() },
        ],
        NOW,
    )
    .await
    .unwrap();
    // One device deletes everything...
    meta.commit(
        u.space,
        dev,
        &[Op::Delete { node: file, expected: Some(v1.id) }, Op::Delete { node: inner, expected: None }, Op::Delete { node: outer, expected: None }],
        NOW,
    )
    .await
    .unwrap();
    // ...another, offline, edited the file. The edit wins.
    meta.commit(u.space, dev, &[Op::Update { node: file, expected: v1.id, version: version(m) }], NOW).await.unwrap();
    let tree = full_tree(meta, u.space).await;
    assert_eq!(tree.nodes.len(), 4);
    assert!(!find(&tree.nodes, outer).deleted && !find(&tree.nodes, inner).deleted && !find(&tree.nodes, file).deleted);
}

async fn a_restore_is_blocked_when_the_name_was_taken(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    let m = manifest(meta, &u, 1).await;
    let (dir, file, v1) = (NodeId::generate(), NodeId::generate(), version(m));
    meta.commit(
        u.space,
        dev,
        &[Op::Mkdir { node: dir, parent: r, name: name("d") }, Op::Create { node: file, parent: dir, name: name("f"), version: v1.clone() }],
        NOW,
    )
    .await
    .unwrap();
    meta.commit(u.space, dev, &[Op::Delete { node: file, expected: Some(v1.id) }, Op::Delete { node: dir, expected: None }], NOW).await.unwrap();
    let newcomer = NodeId::generate();
    meta.commit(u.space, dev, &[Op::Mkdir { node: newcomer, parent: r, name: name("D") }], NOW).await.unwrap();
    let edit = meta.commit(u.space, dev, &[Op::Update { node: file, expected: v1.id, version: version(m) }], NOW).await;
    assert_eq!(conflicts(edit), vec![(0, ConflictKind::NameTaken { existing: newcomer })]);
}

async fn a_version_needs_a_manifest_from_its_own_space(meta: &Meta) {
    let ana = user(meta, "ana").await;
    let bob = user(meta, "bob").await;
    let m = manifest(meta, &ana, 5).await;
    // Bob can't commit a version using Ana's manifest.
    let dev = device(meta, &bob).await;
    let r = root(meta, &bob).await;
    let refused = meta.commit(bob.space, dev, &[Op::Create { node: NodeId::generate(), parent: r, name: name("x"), version: version(m) }], NOW).await;
    assert_eq!(conflicts(refused), vec![(0, ConflictKind::MissingManifest { manifest: m })]);
}

async fn ids_are_never_reused(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    let m = manifest(meta, &u, 1).await;
    let (node, v) = (NodeId::generate(), version(m));
    meta.commit(u.space, dev, &[Op::Create { node, parent: r, name: name("a"), version: v.clone() }], NOW).await.unwrap();
    let same_node = meta.commit(u.space, dev, &[Op::Mkdir { node, parent: r, name: name("b") }], NOW).await;
    assert_eq!(conflicts(same_node), vec![(0, ConflictKind::NodeExists)]);
    let same_version = meta.commit(u.space, dev, &[Op::Create { node: NodeId::generate(), parent: r, name: name("c"), version: v }], NOW).await;
    assert_eq!(conflicts(same_version), vec![(0, ConflictKind::VersionExists)]);
}

async fn the_change_feed_and_epochs(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    let (a, b) = (NodeId::generate(), NodeId::generate());
    meta.commit(u.space, dev, &[Op::Mkdir { node: a, parent: r, name: name("a") }], NOW).await.unwrap();
    let second = meta.commit(u.space, dev, &[Op::Mkdir { node: b, parent: r, name: name("b") }], NOW).await.unwrap();
    let feed = meta.changes(u.space, 1, None, MAX_PAGE_NODES).await.unwrap();
    assert_eq!((feed.seq, feed.nodes.iter().map(|n| n.id).collect::<Vec<_>>()), (2, vec![b]));
    assert!(meta.changes(u.space, 2, None, MAX_PAGE_NODES).await.unwrap().nodes.is_empty());
    // A client that is ahead of the server (the server was restored) must resync.
    assert!(matches!(meta.changes(u.space, 3, None, MAX_PAGE_NODES).await, Err(MetaError::ResyncRequired)));

    let new_epoch = meta.reset_epoch(u.space).await.unwrap();
    assert_ne!(new_epoch, second.epoch);
    assert_eq!(meta.changes(u.space, 0, None, MAX_PAGE_NODES).await.unwrap().epoch, new_epoch);
    let m = manifest(meta, &u, 1).await;
    let file = NodeId::generate();
    meta.commit(u.space, dev, &[Op::Create { node: file, parent: r, name: name("f"), version: version(m) }], NOW).await.unwrap();
    let tree = full_tree(meta, u.space).await;
    let NodeKind::File(state) = &find(&tree.nodes, file).kind else { panic!("not a file") };
    assert_eq!(state.epoch, new_epoch);
}

async fn all_spaces_lists_every_users_space(meta: &Meta) {
    assert!(meta.all_spaces().await.unwrap().is_empty());
    let (ana, bob) = (user(meta, "ana").await, user(meta, "bob").await);
    let mut expected = vec![ana.space, bob.space];
    expected.sort_by_key(|s| *s.as_uuid());
    assert_eq!(meta.all_spaces().await.unwrap(), expected);
}

async fn live_documents_number_their_updates_per_space(meta: &Meta) {
    use craft_cloud_proto::DocId;
    let (ana, bob) = (user(meta, "ana").await, user(meta, "bob").await);
    let (pc, laptop) = (device(meta, &ana).await, device(meta, &ana).await);
    let doc = DocId::generate();
    let made = meta.create_live_doc(ana.space, doc, "recents:wordcraft", None, NOW).await.unwrap();
    assert_eq!((made.head, made.kind.as_str(), made.node), (0, "recents:wordcraft", None));
    // Made again (another machine, offline): the same document.
    assert_eq!(meta.create_live_doc(ana.space, doc, "recents:wordcraft", None, NOW + 5).await.unwrap(), made);
    assert!(matches!(meta.create_live_doc(ana.space, doc, "recents:gridcraft", None, NOW).await, Err(MetaError::Invalid(_))));
    // Another space doesn't see it, and may have a document with the same ID of its own.
    assert_eq!(meta.live_doc(bob.space, doc).await.unwrap(), None);
    let bob_dev = device(meta, &bob).await;
    assert!(matches!(meta.append_live_update(bob.space, doc, bob_dev, 1, b"x", NOW).await, Err(MetaError::NotFound(_))));
    assert!(meta.live_updates(bob.space, doc, 0, 10).await.unwrap().is_empty());

    assert_eq!(meta.append_live_update(ana.space, doc, pc, 1, b"one", NOW).await.unwrap(), 1);
    assert_eq!(meta.append_live_update(ana.space, doc, laptop, 1, b"two", NOW).await.unwrap(), 2, "another device's client_seq 1");
    // Sent again after a lost ack: the first number, stored once.
    assert_eq!(meta.append_live_update(ana.space, doc, pc, 1, b"one", NOW).await.unwrap(), 1);
    // The same client_seq with other bytes (a device that lost its state and counts from 1 again) is a new update.
    assert_eq!(meta.append_live_update(ana.space, doc, pc, 1, b"three", NOW).await.unwrap(), 3);
    assert_eq!(meta.live_doc(ana.space, doc).await.unwrap().unwrap().head, 3);
    let all = meta.live_updates(ana.space, doc, 0, 10).await.unwrap();
    let seen: Vec<(u64, &[u8], DeviceId)> = all.iter().map(|u| (u.seq, u.bytes.as_slice(), u.device)).collect();
    assert_eq!(seen, vec![(1, &b"one"[..], pc), (2, &b"two"[..], laptop), (3, &b"three"[..], pc)]);
    let page = meta.live_updates(ana.space, doc, 1, 1).await.unwrap();
    assert_eq!(page.iter().map(|u| u.seq).collect::<Vec<_>>(), vec![2]);
    assert!(meta.live_updates(ana.space, doc, 3, 10).await.unwrap().is_empty());
    assert!(matches!(meta.append_live_update(ana.space, DocId::generate(), pc, 9, b"x", NOW).await, Err(MetaError::NotFound(_))));
    assert!(matches!(meta.append_live_update(ana.space, doc, pc, u64::MAX, b"x", NOW).await, Err(MetaError::Invalid(_))));
}

/// A catch-up page is bounded by bytes, not only by a count (64 updates of 1 MiB would be 64 MiB at once).
async fn live_pages_stop_at_their_byte_limit(meta: &Meta) {
    use craft_cloud_proto::DocId;
    let ana = user(meta, "ana").await;
    let dev = device(meta, &ana).await;
    let doc = DocId::generate();
    meta.create_live_doc(ana.space, doc, "recents:wordcraft", None, NOW).await.unwrap();
    for i in 1..=6u64 {
        meta.append_live_update(ana.space, doc, dev, i, &vec![u8::try_from(i).unwrap(); 1000], NOW).await.unwrap();
    }
    let seqs = |(page, more): (Vec<craft_cloud_meta::LiveUpdate>, bool)| (page.iter().map(|u| u.seq).collect::<Vec<_>>(), more);
    assert_eq!(seqs(meta.live_page(ana.space, doc, 0, 2500, 100).await.unwrap()), (vec![1, 2], true), "2 KB fit, the third doesn't");
    assert_eq!(seqs(meta.live_page(ana.space, doc, 2, 10, 100).await.unwrap()), (vec![3], true), "at least one, however big");
    assert_eq!(seqs(meta.live_page(ana.space, doc, 3, 1_000_000, 2).await.unwrap()), (vec![4, 5], true), "and at most so many");
    assert_eq!(seqs(meta.live_page(ana.space, doc, 3, 1_000_000, 10).await.unwrap()), (vec![4, 5, 6], false), "the end");
    assert_eq!(seqs(meta.live_page(ana.space, doc, 6, 1_000_000, 10).await.unwrap()), (vec![], false));
    assert_eq!(meta.live_page(ana.space, doc, 0, 1500, 10).await.unwrap().0[0].bytes, vec![1u8; 1000]);
    assert!(meta.device_active(dev).await.unwrap());
    meta.revoke_device(ana.id, dev).await.unwrap();
    assert!(!meta.device_active(dev).await.unwrap(), "revoked");
}

/// A document's log is capped by bytes, one way: the update that crosses the cap is
/// stored, and from then on every append is refused, so no smaller later update is stored past a refused one it
/// depends on. A repeat of an update already stored is still acked; the admin's reset empties the log.
async fn a_live_document_is_capped_by_bytes(meta: &Meta) {
    use craft_cloud_proto::DocId;
    assert_eq!(craft_cloud_meta::MAX_LIVE_DOC_BYTES, 16 * 1024 * 1024);
    let meta = meta.clone().with_live_doc_cap(2500);
    let ana = user(&meta, "ana").await;
    let dev = device(&meta, &ana).await;
    let (doc, other) = (DocId::generate(), DocId::generate());
    for d in [doc, other] {
        meta.create_live_doc(ana.space, d, "recents:wordcraft", None, NOW).await.unwrap();
    }
    for i in 1..=3u64 {
        meta.append_live_update(ana.space, doc, dev, i, &[1; 1000], NOW).await.unwrap();
    }
    assert_eq!(meta.live_doc(ana.space, doc).await.unwrap().unwrap().bytes, 3000, "the third crossed the cap and is stored");
    let full = meta.append_live_update(ana.space, doc, dev, 4, &[2; 10], NOW).await;
    assert!(matches!(full, Err(MetaError::DocFull { used: 3000, limit: 2500 })), "{full:?}");
    assert_eq!(meta.live_doc(ana.space, doc).await.unwrap().unwrap().head, 3, "nothing stored, however small");
    assert_eq!(meta.append_live_update(ana.space, doc, dev, 2, &[1; 1000], NOW).await.unwrap(), 2, "a repeat is acked");
    assert_eq!(meta.append_live_update(ana.space, other, dev, 1, &[3; 2000], NOW).await.unwrap(), 1, "per document");
    let (epoch, live) = (meta.space_info(ana.space).await.unwrap().epoch, meta.live_epoch(ana.space).await.unwrap());
    assert_eq!(live, epoch, "no reset yet: the live epoch is the space's");
    meta.reset_live_doc(ana.space, doc).await.unwrap();
    let reset = meta.live_doc(ana.space, doc).await.unwrap().unwrap();
    assert_eq!((reset.head, reset.bytes), (0, 0));
    assert_eq!(meta.space_info(ana.space).await.unwrap().epoch, epoch, "the file sync's epoch is untouched");
    let after = meta.live_epoch(ana.space).await.unwrap();
    assert_ne!(after, live, "a new live generation no agent can miss");
    meta.reset_live_doc(ana.space, doc).await.unwrap();
    assert_ne!(meta.live_epoch(ana.space).await.unwrap(), after, "and again");
    assert!(meta.live_updates(ana.space, doc, 0, 10).await.unwrap().is_empty());
    assert_eq!(meta.append_live_update(ana.space, doc, dev, 4, &[2; 10], NOW).await.unwrap(), 1, "open again");
    assert!(matches!(meta.reset_live_doc(ana.space, DocId::generate()).await, Err(MetaError::NotFound(_))));
}

async fn concurrent_live_appends_get_one_number_each(meta: &Meta) {
    use craft_cloud_proto::DocId;
    let ana = user(meta, "ana").await;
    let dev = device(meta, &ana).await;
    let doc = DocId::generate();
    meta.create_live_doc(ana.space, doc, "recents:wordcraft", None, NOW).await.unwrap();
    let tasks: Vec<_> = (0..16u64)
        .map(|i| {
            let meta = meta.clone();
            // Every update twice, as a device that sends again after a lost ack.
            tokio::spawn(async move { meta.append_live_update(ana.space, doc, dev, i % 8, &[u8::try_from(i % 8).unwrap()], NOW).await.unwrap() })
        })
        .collect();
    let mut seqs = Vec::new();
    for task in tasks {
        seqs.push(task.await.unwrap());
    }
    seqs.sort_unstable();
    seqs.dedup();
    assert_eq!(seqs, (1..=8).collect::<Vec<_>>(), "each update stored once, numbered without gaps");
    assert_eq!(meta.live_doc(ana.space, doc).await.unwrap().unwrap().head, 8);
}

async fn concurrent_live_creates_make_one_document(meta: &Meta) {
    use craft_cloud_proto::DocId;
    let ana = user(meta, "ana").await;
    let doc = DocId::generate();
    // Each create carries its own time, so the document's created_ms shows which create stored it.
    let tasks: Vec<_> = (0..8i64)
        .map(|i| {
            let meta = meta.clone();
            tokio::spawn(async move { meta.create_live_doc(ana.space, doc, "recents:wordcraft", None, NOW + i).await })
        })
        .collect();
    let mut created = Vec::new();
    for task in tasks {
        created.push(task.await.unwrap().expect("a create of an existing document is not an error"));
    }
    assert!(created.iter().all(|d| d.created_ms == created[0].created_ms), "every create returns the one document");
    assert_eq!(meta.live_doc(ana.space, doc).await.unwrap().unwrap().created_ms, created[0].created_ms);
    assert!(matches!(meta.create_live_doc(ana.space, doc, "recents:other", None, NOW).await, Err(MetaError::Invalid(_))));
}

async fn live_documents_holding_updates_are_capped(meta: &Meta) {
    use craft_cloud_proto::DocId;
    let meta = meta.clone().with_live_docs_cap(2);
    let ana = user(&meta, "ana").await;
    let bo = user(&meta, "bo").await;
    let (dev, bo_dev) = (device(&meta, &ana).await, device(&meta, &bo).await);
    let docs: Vec<DocId> = (0..3).map(|_| DocId::generate()).collect();
    for doc in &docs {
        meta.create_live_doc(ana.space, *doc, "recents:wordcraft", None, NOW).await.unwrap();
    }
    assert_eq!(meta.append_live_update(ana.space, docs[0], dev, 1, b"one", NOW).await.unwrap(), 1);
    assert_eq!(meta.append_live_update(ana.space, docs[1], dev, 1, b"two", NOW).await.unwrap(), 1);
    assert!(matches!(meta.append_live_update(ana.space, docs[2], dev, 1, b"three", NOW).await, Err(MetaError::TooManyDocs { limit: 2 })));
    let third = meta.live_doc(ana.space, docs[2]).await.unwrap().unwrap();
    assert_eq!((third.head, third.bytes), (0, 0), "nothing was stored, and the number went back");
    assert!(meta.live_updates(ana.space, docs[2], 0, 10).await.unwrap().is_empty());
    assert_eq!(meta.append_live_update(ana.space, docs[0], dev, 2, b"more", NOW).await.unwrap(), 2, "a document that holds updates goes on");
    assert_eq!(meta.append_live_update(ana.space, docs[0], dev, 1, b"one", NOW).await.unwrap(), 1, "a repeat is acked with its number");
    // Another account has its own count.
    let other = DocId::generate();
    meta.create_live_doc(bo.space, other, "recents:wordcraft", None, NOW).await.unwrap();
    assert_eq!(meta.append_live_update(bo.space, other, bo_dev, 1, b"x", NOW).await.unwrap(), 1);
}

async fn live_document_rows_are_capped(meta: &Meta) {
    use craft_cloud_proto::DocId;
    let meta = meta.clone().with_live_doc_rows_cap(3);
    let ana = user(&meta, "ana").await;
    let bo = user(&meta, "bo").await;
    let docs: Vec<DocId> = (0..3).map(|_| DocId::generate()).collect();
    for doc in &docs {
        meta.create_live_doc(ana.space, *doc, "recents:wordcraft", None, NOW).await.unwrap();
    }
    let extra = DocId::generate();
    assert!(matches!(meta.create_live_doc(ana.space, extra, "recents:wordcraft", None, NOW).await, Err(MetaError::TooManyDocRows { limit: 3 })));
    assert!(meta.live_doc(ana.space, extra).await.unwrap().is_none(), "the refused document was not stored");
    assert_eq!(
        meta.create_live_doc(ana.space, docs[0], "recents:wordcraft", None, NOW).await.unwrap().id,
        docs[0],
        "re-creating one of them still works"
    );
    meta.create_live_doc(bo.space, extra, "recents:wordcraft", None, NOW).await.unwrap();
}

async fn concurrent_commits_on_one_file_let_exactly_one_win(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    let m = manifest(meta, &u, 1).await;
    let (file, v1) = (NodeId::generate(), version(m));
    meta.commit(u.space, dev, &[Op::Create { node: file, parent: r, name: name("f"), version: v1.clone() }], NOW).await.unwrap();
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let (meta, space, v) = (meta.clone(), u.space, version(m));
        tasks.push(tokio::spawn(async move { meta.commit(space, dev, &[Op::Update { node: file, expected: v1.id, version: v }], NOW).await }));
    }
    let mut wins = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => wins += 1,
            Err(MetaError::Rejected(list)) => assert!(matches!(list[0].kind, ConflictKind::VersionMismatch { .. })),
            Err(other) => panic!("unexpected error {other:?}"),
        }
    }
    assert_eq!(wins, 1);
    assert_eq!(meta.space_info(u.space).await.unwrap().seq, 2);
}

async fn batch_size_is_limited(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    assert!(matches!(meta.commit(u.space, dev, &[], NOW).await, Err(MetaError::Invalid(_))));
    let ops: Vec<Op> = (0..1001).map(|i| Op::Mkdir { node: NodeId::generate(), parent: r, name: name(&format!("d{i}")) }).collect();
    assert!(matches!(meta.commit(u.space, dev, &ops, NOW).await, Err(MetaError::Invalid(_))));
    meta.commit(u.space, dev, &ops[..1000], NOW).await.unwrap();
    assert_eq!(full_tree(meta, u.space).await.nodes.len(), 1001);
}

async fn folders_nest_at_most_max_tree_depth(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    let m = manifest(meta, &u, 1).await;
    let mut parent = r;
    let mut chain = Vec::new();
    let mut ops = Vec::new();
    for i in 0..craft_cloud_proto::limits::MAX_TREE_DEPTH {
        let node = NodeId::generate();
        ops.push(Op::Mkdir { node, parent, name: name(&format!("d{i}")) });
        chain.push(node);
        parent = node;
        if ops.len() == 1000 {
            meta.commit(u.space, dev, &ops, NOW).await.unwrap();
            ops.clear();
        }
    }
    if !ops.is_empty() {
        meta.commit(u.space, dev, &ops, NOW).await.unwrap();
    }
    let deepest = parent;
    let too_deep = meta.commit(u.space, dev, &[Op::Mkdir { node: NodeId::generate(), parent: deepest, name: name("x") }], NOW).await;
    assert_eq!(conflicts(too_deep), vec![(0, ConflictKind::TooDeep)]);

    // A file at the deepest level is fine, and a file can be moved there.
    let f = NodeId::generate();
    meta.commit(u.space, dev, &[Op::Create { node: f, parent: r, name: name("f"), version: version(m) }], NOW).await.unwrap();
    meta.commit(u.space, dev, &[Op::Move { node: f, from_parent: r, from_name: name("f"), parent: deepest, name: name("f") }], NOW).await.unwrap();

    let top = NodeId::generate();
    meta.commit(u.space, dev, &[Op::Mkdir { node: top, parent: r, name: name("top") }], NOW).await.unwrap();
    let moved =
        meta.commit(u.space, dev, &[Op::Move { node: top, from_parent: r, from_name: name("top"), parent: deepest, name: name("top") }], NOW).await;
    assert_eq!(conflicts(moved), vec![(0, ConflictKind::TooDeep)]);
}

async fn creating_or_moving_into_a_deleted_folder_brings_it_back(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    let m = manifest(meta, &u, 1).await;
    let (d1, d2, f) = (NodeId::generate(), NodeId::generate(), NodeId::generate());
    meta.commit(
        u.space,
        dev,
        &[
            Op::Mkdir { node: d1, parent: r, name: name("one") },
            Op::Mkdir { node: d2, parent: r, name: name("two") },
            Op::Create { node: f, parent: r, name: name("f"), version: version(m) },
        ],
        NOW,
    )
    .await
    .unwrap();
    meta.commit(u.space, dev, &[Op::Delete { node: d1, expected: None }, Op::Delete { node: d2, expected: None }], NOW).await.unwrap();

    let created = NodeId::generate();
    meta.commit(u.space, dev, &[Op::Create { node: created, parent: d1, name: name("new"), version: version(m) }], NOW).await.unwrap();
    meta.commit(u.space, dev, &[Op::Move { node: f, from_parent: r, from_name: name("f"), parent: d2, name: name("f") }], NOW).await.unwrap();
    let tree = full_tree(meta, u.space).await;
    assert!(!find(&tree.nodes, d1).deleted && !find(&tree.nodes, d2).deleted);
    assert_eq!(find(&tree.nodes, created).parent, Some(d1));
    assert_eq!(find(&tree.nodes, f).parent, Some(d2));
}

async fn names_can_be_juggled_within_one_batch(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    let m = manifest(meta, &u, 1).await;
    let (a, b) = (NodeId::generate(), NodeId::generate());
    let (va, vb) = (version(m), version(m));
    meta.commit(
        u.space,
        dev,
        &[
            Op::Create { node: a, parent: r, name: name("a"), version: va.clone() },
            Op::Create { node: b, parent: r, name: name("b"), version: vb.clone() },
        ],
        NOW,
    )
    .await
    .unwrap();
    meta.commit(
        u.space,
        dev,
        &[
            Op::Move { node: a, from_parent: r, from_name: name("a"), parent: r, name: name("tmp") },
            Op::Move { node: b, from_parent: r, from_name: name("b"), parent: r, name: name("a") },
            Op::Move { node: a, from_parent: r, from_name: name("tmp"), parent: r, name: name("b") },
        ],
        NOW,
    )
    .await
    .unwrap();
    let tree = full_tree(meta, u.space).await;
    assert_eq!(find(&tree.nodes, a).name, Some(name("b")));
    assert_eq!(find(&tree.nodes, b).name, Some(name("a")));

    // Delete then re-create under the same name, in one batch.
    let c = NodeId::generate();
    meta.commit(
        u.space,
        dev,
        &[Op::Delete { node: b, expected: Some(vb.id) }, Op::Create { node: c, parent: r, name: name("a"), version: version(m) }],
        NOW,
    )
    .await
    .unwrap();
    let tree = full_tree(meta, u.space).await;
    assert_eq!(find(&tree.nodes, c).name, Some(name("a")));
    assert!(tree.nodes.iter().all(|n| n.id != b));
}

async fn a_failed_restore_walk_is_not_repeated_for_every_operation(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    let m = manifest(meta, &u, 1).await;
    let mut parent = r;
    let mut chain = Vec::new();
    let mut ops = Vec::new();
    for i in 0..200 {
        let node = NodeId::generate();
        ops.push(Op::Mkdir { node, parent, name: name(&format!("d{i}")) });
        chain.push((node, parent, name(&format!("d{i}"))));
        parent = node;
    }
    meta.commit(u.space, dev, &ops, NOW).await.unwrap();
    let deepest = parent;
    let deletes: Vec<Op> = chain.iter().rev().map(|(node, _, _)| Op::Delete { node: *node, expected: None }).collect();
    meta.commit(u.space, dev, &deletes, NOW).await.unwrap();
    let taker = NodeId::generate();
    meta.commit(u.space, dev, &[Op::Mkdir { node: taker, parent: r, name: name("d0") }], NOW).await.unwrap();

    let creates: Vec<Op> =
        (0..1000).map(|i| Op::Create { node: NodeId::generate(), parent: deepest, name: name(&format!("f{i}")), version: version(m) }).collect();
    let started = std::time::Instant::now();
    let refused = conflicts(meta.commit(u.space, dev, &creates, NOW).await);
    let took = started.elapsed();
    eprintln!("1000 refused creates into a 200-deep deleted chain: {took:?}");
    assert_eq!(refused.len(), 1000);
    assert!(refused.iter().all(|(_, kind)| *kind == ConflictKind::NameTaken { existing: taker }));
    assert!(took < std::time::Duration::from_secs(5), "took {took:?}");
}

/// A manifest of `count` distinct chunks (1 byte each), and the chunk IDs in order.
fn big_manifest(count: u32) -> (Manifest, Vec<ChunkId>) {
    let ids: Vec<ChunkId> = (0..count)
        .map(|i| ChunkId::from_bytes(*blake3::hash(&[b"big-", uuid::Uuid::now_v7().as_bytes().as_slice(), &i.to_le_bytes()].concat()).as_bytes()))
        .collect();
    let chunks = ids.iter().map(|id| ChunkRef { id: *id, len: 1 }).collect();
    let m = Manifest::new(ChunkerParamsId::new("fastcdc-v1").unwrap(), u64::from(count), FileHash::from_bytes([2; 32]), chunks).unwrap();
    (m, ids)
}

async fn big_manifests_are_checked_in_batches(meta: &Meta) {
    let u = user(meta, "ana").await;

    // 100,000 chunks, all uploaded first: the manifest is accepted quickly.
    let (m, ids) = big_manifest(100_000);
    let setup = std::time::Instant::now();
    let uploads: Vec<(ChunkId, u64)> = ids.iter().map(|id| (*id, 2)).collect();
    meta.add_chunks(u.space, &uploads, NOW).await.unwrap();
    eprintln!("uploading 100000 chunks: {:?}", setup.elapsed());
    let started = std::time::Instant::now();
    meta.add_manifest(u.space, &m, 100, NOW).await.unwrap();
    let took = started.elapsed();
    eprintln!("add_manifest with 100000 present chunks: {took:?}");
    assert!(took < std::time::Duration::from_secs(10), "took {took:?}");
    assert!(meta.has_manifest(u.space, &m.id()).await.unwrap());

    // 50,000 chunks, none uploaded: all reported, in order.
    let (m, ids) = big_manifest(50_000);
    let started = std::time::Instant::now();
    match meta.add_manifest(u.space, &m, 100, NOW).await {
        Err(MetaError::MissingChunks(missing)) => {
            // Listed up to the API's limit, in order.
            assert_eq!(missing.len(), craft_cloud_proto::api::MAX_MISSING_IDS);
            assert_eq!(missing, ids[..craft_cloud_proto::api::MAX_MISSING_IDS]);
        }
        other => panic!("expected MissingChunks, got {other:?}"),
    }
    let took = started.elapsed();
    eprintln!("add_manifest with 50000 missing chunks: {took:?}");
    assert!(took < std::time::Duration::from_secs(10), "took {took:?}");
    let started = std::time::Instant::now();
    assert_eq!(meta.missing_chunks(u.space, &ids).await.unwrap(), ids);
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}

async fn the_tree_and_the_feed_come_in_pages(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    // 25 nodes over several commits, so the pages cross sequence numbers.
    for batch in 0..5 {
        let ops: Vec<Op> = (0..5).map(|i| Op::Mkdir { node: NodeId::generate(), parent: r, name: name(&format!("d{batch}-{i}")) }).collect();
        meta.commit(u.space, dev, &ops, NOW).await.unwrap();
    }

    let mut seen = Vec::new();
    let mut pages = 0;
    let mut after = None;
    let first_seq = meta.space_info(u.space).await.unwrap().seq;
    loop {
        let page = meta.tree(u.space, after, 7).await.unwrap();
        assert_eq!(page.seq, first_seq);
        assert!(page.nodes.len() <= 7);
        seen.extend(page.nodes.iter().map(|n| n.id));
        pages += 1;
        match page.next {
            Some(next) => {
                assert_eq!(page.nodes.len(), 7);
                after = Some(next);
            }
            None => break,
        }
    }
    assert_eq!(pages, 4);
    assert_eq!(seen.len(), 26);
    assert_eq!(seen.iter().collect::<std::collections::HashSet<_>>().len(), 26);
    assert!(seen.contains(&r));

    let mut seen = Vec::new();
    let (mut pages, mut after) = (0, None);
    loop {
        let page = meta.changes(u.space, 0, after, 7).await.unwrap();
        seen.extend(page.nodes.iter().map(|n| n.id));
        pages += 1;
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    assert_eq!(pages, 4);
    assert_eq!(seen.len(), 25);
    assert_eq!(seen.iter().collect::<std::collections::HashSet<_>>().len(), 25);
    // Resuming from a later sequence number pages only what changed after it.
    let later = meta.changes(u.space, 3, None, 10).await.unwrap();
    assert_eq!(later.nodes.len(), 10);
    assert!(later.next.is_none());

    for bad in [0, MAX_PAGE_NODES + 1] {
        assert!(matches!(meta.tree(u.space, None, bad).await, Err(MetaError::Invalid(_))));
        assert!(matches!(meta.changes(u.space, 0, None, bad).await, Err(MetaError::Invalid(_))));
    }
}

async fn usernames_are_unique_ignoring_ascii_case(meta: &Meta) {
    let ana = user(meta, "Ana").await;
    assert_eq!(ana.username, "ana");
    assert!(matches!(meta.create_user("ana", "x", Role::Member, NOW).await, Err(MetaError::UsernameTaken)));
    assert!(matches!(meta.create_user("ANA", "x", Role::Member, NOW).await, Err(MetaError::UsernameTaken)));
    assert_eq!(meta.user_by_name("ANA").await.unwrap(), Some(ana));
}

// ---- plain tests ----

#[tokio::test]
async fn user_records_do_not_print_their_password_hash() {
    let meta = Meta::open("sqlite::memory:").await.unwrap();
    let ana = meta.create_user("ana", "$argon2id$secret-hash", Role::Member, NOW).await.unwrap();
    let shown = format!("{ana:?}");
    assert!(!shown.contains("secret-hash"), "{shown}");
    assert!(shown.contains("<redacted>") && shown.contains("ana"));
}

#[tokio::test]
async fn in_memory_sqlite_works() {
    let meta = Meta::open("sqlite::memory:").await.unwrap();
    let ana = user(&meta, "ana").await;
    assert_eq!(meta.user_by_name("ana").await.unwrap(), Some(ana));
}

#[tokio::test]
async fn reopening_a_migrated_database_is_fine() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}", dir.path().join("meta.db").display());
    let ana = {
        let meta = Meta::open(&url).await.unwrap();
        user(&meta, "ana").await
    };
    let meta = Meta::open(&url).await.unwrap();
    assert_eq!(meta.user_by_name("ana").await.unwrap(), Some(ana));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_write_waits_for_a_lock_another_program_holds() {
    use sqlx::Connection;
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}", dir.path().join("meta.db").display());
    let meta = Meta::open(&url).await.unwrap();
    let ana = user(&meta, "ana").await;
    let mut other = sqlx::SqliteConnection::connect(&url).await.unwrap();
    sqlx::query("BEGIN IMMEDIATE").execute(&mut other).await.unwrap();
    let release = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        sqlx::query("ROLLBACK").execute(&mut other).await.unwrap();
    });
    // Waits (up to `SQLITE_BUSY_TIMEOUT`) instead of failing with "database is locked".
    meta.add_chunk(ana.space, &random_chunk(), 10, NOW).await.unwrap();
    release.await.unwrap();
    assert!(craft_cloud_meta::SQLITE_BUSY_TIMEOUT >= std::time::Duration::from_secs(1));
}

#[tokio::test]
async fn upgrading_counts_what_each_space_already_stores() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}", dir.path().join("meta.db").display());
    let ana = {
        let meta = Meta::open(&url).await.unwrap();
        let ana = user(&meta, "ana").await;
        meta.add_chunk(ana.space, &random_chunk(), 60, NOW).await.unwrap();
        meta.add_chunk(ana.space, &random_chunk(), 40, NOW).await.unwrap();
        ana
    };
    {
        // The database as it was before stored bytes were counted.
        use sqlx::Connection;
        let mut conn = sqlx::SqliteConnection::connect(&url).await.unwrap();
        sqlx::query("ALTER TABLE spaces DROP COLUMN stored_bytes").execute(&mut conn).await.unwrap();
        // And before live documents.
        sqlx::query("DROP TABLE live_updates").execute(&mut conn).await.unwrap();
        sqlx::query("DROP TABLE live_docs").execute(&mut conn).await.unwrap();
        sqlx::query("DROP TABLE live_spaces").execute(&mut conn).await.unwrap();
        sqlx::query("DELETE FROM schema_version WHERE version >= 2").execute(&mut conn).await.unwrap();
    }
    let meta = Meta::open(&url).await.unwrap();
    assert_eq!(meta.stored_bytes(ana.space).await.unwrap(), 100);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_opens_migrate_once_on_postgres() {
    let Some((url, _guard)) = postgres_database().await else {
        eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set");
        return;
    };
    let opens: Vec<_> = (0..4)
        .map(|_| {
            let url = url.clone();
            tokio::spawn(async move { Meta::open(&url).await })
        })
        .collect();
    for open in opens {
        open.await.unwrap().unwrap();
    }
    use sqlx::Connection;
    let mut conn = sqlx::PgConnection::connect(&url).await.unwrap();
    let versions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM schema_version").fetch_one(&mut conn).await.unwrap();
    assert_eq!(versions, 3);
}
