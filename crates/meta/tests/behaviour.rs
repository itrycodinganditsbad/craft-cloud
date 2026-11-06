//! The same scenarios on every backend. SQLite always runs (a fresh temporary file per test). Postgres runs
//! when `CRAFT_CLOUD_TEST_PG` holds an admin URL, e.g. `postgres://postgres:pw@127.0.0.1:5432/postgres`; each
//! test creates and drops its own database. CI sets `CRAFT_CLOUD_REQUIRE_BACKENDS`, which turns a missing
//! `CRAFT_CLOUD_TEST_PG` into a failure instead of a skip.

use craft_cloud_meta::{BlobKind, InviteFor, Meta, MetaError, Redeemed, Role, UserRecord};
use craft_cloud_proto::api::PairingState;
use craft_cloud_proto::join::{Code, INVITE_CHARS, INVITE_TTL_MS, PAIRING_CHARS, PAIRING_TTL_MS};
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
    a_version_keeps_its_save_record,
    pages_of_save_records_stay_small,
    a_late_save_record_is_appended_once,
    manifests_need_their_chunks_and_are_per_space,
    stored_bytes_are_counted_once_per_space_and_capped,
    account_usage_estimates_history_and_trash_without_adding_them,
    account_usage_excludes_chunks_shared_with_current_version,
    account_usage_deduplicates_per_space_and_counts_pending_purge,
    account_usage_excludes_live_document_snapshot_blobs,
    account_usage_leaves_saved_versions_out_of_history,
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
    history_pages_newest_first_for_own_files_only,
    a_restore_is_a_new_version_after_the_head,
    a_restore_refuses_what_it_can_not_restore,
    concurrent_restores_let_exactly_one_win,
    a_restore_and_a_late_save_record_both_apply,
    deleting_stamps_the_time_once_and_coming_back_clears_it,
    the_trash_pages_newest_first_for_its_own_space,
    a_restore_brings_the_deleted_folders_back_with_their_ids,
    a_restore_merges_into_live_folders_and_renames_around_taken_names,
    a_restore_with_no_free_name_changes_nothing,
    the_trash_lists_a_folder_without_what_is_below_it,
    a_folder_restore_brings_back_everything_in_the_trash_below_it,
    a_folder_restore_gives_children_of_one_name_a_conflict_name,
    removing_a_folder_from_the_trash_takes_what_is_below_it,
    a_folder_restore_takes_at_most_the_cap_below_it,
    a_folder_restore_leaves_the_rest_listed_through_its_own_folder,
    removing_a_folder_takes_at_most_the_cap_below_it,
    only_nodes_in_the_trash_can_be_restored_or_removed,
    removing_from_the_trash_frees_nothing,
    a_trash_restore_and_a_commit_for_the_same_name_agree,
    a_trash_restore_and_an_edit_of_the_file_agree,
    a_restored_file_gets_a_new_version_with_its_old_content,
    a_restore_does_not_exceed_the_depth_limit,
    a_new_manifest_has_exact_edges_complete_in_its_own_transaction,
    a_long_manifest_is_indexed_in_batches_of_the_budget,
    the_index_stays_exact_through_every_write_path,
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
    a_document_is_made_by_import_once,
    concurrent_imports_of_one_file_keep_one,
    a_log_is_compacted_into_a_snapshot,
    concurrent_compactions_take_one,
    a_space_s_live_bytes_are_capped,
    documents_of_files_have_caps_of_their_own,
    a_compacted_log_still_holds_updates,
    the_import_stays_when_the_log_is_compacted,
    a_pairing_code_works_once,
    an_expired_code_is_refused,
    a_replaced_invite_s_old_code_fails,
    a_revoked_invite_fails,
    a_pairing_is_waiting_then_used_then_expired,
    a_device_has_one_open_pairing,
    another_users_pairing_is_invisible,
    a_pairing_dies_with_its_revoked_creator,
    concurrent_redeems_of_one_code_give_one_device,
    an_invite_to_an_account_with_a_password_needs_none,
    an_invite_to_a_new_account_needs_a_password,
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
/// A save record that comes after its version: added to that version (the node is in the change feed again), once;
/// a version with a record keeps it; the same call again changes nothing; another node's version is not found.
async fn a_late_save_record_is_appended_once(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = meta.create_device(u.id, "pc", &[3u8; 32], NOW).await.unwrap();
    let parent = root(meta, &u).await;
    let m = manifest(meta, &u, 4).await;
    let (node, v) = (NodeId::generate(), version(m));
    meta.commit(u.space, dev, &[Op::Create { node, parent, name: name("late.kitdemo"), version: v.clone() }], NOW).await.unwrap();
    let before = meta.space_info(u.space).await.unwrap().seq;
    let doc = craft_cloud_proto::live::node_doc(u.space, node);
    let stranger = craft_cloud_proto::SaveRecord { doc: craft_cloud_proto::DocId::generate(), frontiers: vec![(1, 5)] };
    assert!(matches!(meta.attach_save(u.space, node, v.id, &stranger).await, Err(MetaError::Invalid(_))), "another file's document");
    let unsorted = craft_cloud_proto::SaveRecord { doc, frontiers: vec![(2, 1), (1, 5)] };
    assert!(matches!(meta.attach_save(u.space, node, v.id, &unsorted).await, Err(MetaError::Invalid(_))));
    let record = craft_cloud_proto::SaveRecord { doc, frontiers: vec![(1, 5)] };
    let added = meta.attach_save(u.space, node, v.id, &record).await.unwrap();
    assert_eq!(added.seq, before + 1);
    let feed = meta.changes(u.space, before, None, MAX_PAGE_NODES).await.unwrap();
    let NodeKind::File(f) = &find(&feed.nodes, node).kind else { panic!("a file") };
    assert_eq!((f.version, f.save.clone()), (v.id, Some(record.clone())), "in the feed again, with its record");
    let other = craft_cloud_proto::SaveRecord { doc: record.doc, frontiers: vec![(2, 1)] };
    assert_eq!(meta.attach_save(u.space, node, v.id, &other).await.unwrap().seq, before + 1, "kept, nothing changed");
    assert_eq!(meta.attach_save(u.space, node, v.id, &record).await.unwrap().seq, before + 1);
    let tree = full_tree(meta, u.space).await;
    let NodeKind::File(f) = &find(&tree.nodes, node).kind else { panic!("a file") };
    assert_eq!(f.save, Some(record));
    let nowhere = NodeId::generate();
    let theirs = craft_cloud_proto::SaveRecord { doc: craft_cloud_proto::live::node_doc(u.space, nowhere), frontiers: vec![(2, 1)] };
    assert!(matches!(meta.attach_save(u.space, nowhere, v.id, &theirs).await, Err(MetaError::NotFound(_))));
    let too_many =
        craft_cloud_proto::SaveRecord { doc: other.doc, frontiers: (0..=craft_cloud_proto::ops::MAX_SAVE_HEADS as u64).map(|p| (p, 1)).collect() };
    assert!(matches!(meta.attach_save(u.space, node, v.id, &too_many).await, Err(MetaError::Invalid(_))));
}

/// Files whose versions all carry the biggest save record there can be: every page of the listing and of the change
/// feed ends before its records pass their budget, far under what an agent reads of a page, and the pages together
/// still list every file once, each with its record.
async fn pages_of_save_records_stay_small(meta: &Meta) {
    use craft_cloud_meta::MAX_PAGE_SAVE_BYTES;
    let u = user(meta, "ana").await;
    let dev = meta.create_device(u.id, "pc", &[3u8; 32], NOW).await.unwrap();
    let parent = root(meta, &u).await;
    let m = manifest(meta, &u, 4).await;
    let biggest = craft_cloud_proto::SaveRecord {
        doc: craft_cloud_proto::DocId::generate(),
        frontiers: (0..craft_cloud_proto::ops::MAX_SAVE_HEADS as u64).map(|p| (u64::MAX - 1000 + p, u32::MAX)).collect(),
    };
    let record_bytes = serde_json::to_string(&biggest).unwrap().len();
    assert!(record_bytes < 10 * 1024, "{record_bytes}");
    let files = 2 * MAX_PAGE_SAVE_BYTES / record_bytes + 10;
    let mut made = std::collections::BTreeSet::new();
    for batch in 0..files.div_ceil(500) {
        let ops: Vec<Op> = (0..500.min(files - batch * 500))
            .map(|i| {
                let node = NodeId::generate();
                made.insert(node);
                let v = NewVersion { save: Some(biggest.clone()), ..version(m) };
                Op::Create { node, parent, name: name(&format!("f{batch}-{i}.kitdemo")), version: v }
            })
            .collect();
        meta.commit(u.space, dev, &ops, NOW).await.unwrap();
    }
    for feed in [false, true] {
        let (mut after, mut seen, mut pages) = (None, std::collections::BTreeSet::new(), 0);
        loop {
            let page = if feed {
                meta.changes(u.space, 0, after, MAX_PAGE_NODES).await.unwrap()
            } else {
                meta.tree(u.space, after, MAX_PAGE_NODES).await.unwrap()
            };
            pages += 1;
            let json = serde_json::to_vec(&page).unwrap().len();
            assert!(json < MAX_PAGE_SAVE_BYTES + 2 * 1024 * 1024, "a page of {json} bytes");
            for n in &page.nodes {
                if let NodeKind::File(f) = &n.kind {
                    assert_eq!(f.save.as_ref(), Some(&biggest));
                    assert!(seen.insert(n.id), "listed twice");
                }
            }
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        assert_eq!(seen, made, "every file, once");
        assert!(pages >= 3, "cut into pages by their records: {pages}");
    }
}

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
    NewVersion { id: VersionId::generate(), manifest, mtime_ms: NOW, exec: false, save: None }
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

/// A version a Craft app saved keeps its save record (as stored, unread), in the listing and the change feed; one
/// without has none; one over the record limit is stored without it.
async fn a_version_keeps_its_save_record(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = meta.create_device(u.id, "pc", &[3u8; 32], NOW).await.unwrap();
    let parent = root(meta, &u).await;
    let record = craft_cloud_proto::SaveRecord { doc: craft_cloud_proto::DocId::generate(), frontiers: vec![(1, 5), (2, 3)] };
    let (saved, plain, huge) = (NodeId::generate(), NodeId::generate(), NodeId::generate());
    let op = |node: NodeId, name: &str, v: NewVersion| Op::Create { node, parent, name: craft_cloud_proto::Name::new(name).unwrap(), version: v };
    let m = manifest(meta, &u, 4).await;
    let too_many =
        craft_cloud_proto::SaveRecord { doc: record.doc, frontiers: (0..=craft_cloud_proto::ops::MAX_SAVE_HEADS as u64).map(|p| (p, 1)).collect() };
    let ops = [
        op(saved, "a.kitdemo", NewVersion { save: Some(record.clone()), ..version(m) }),
        op(plain, "b.txt", version(m)),
        op(huge, "c.kitdemo", NewVersion { save: Some(too_many), ..version(m) }),
    ];
    meta.commit(u.space, dev, &ops, NOW).await.unwrap();
    for listing in [full_tree(meta, u.space).await, meta.changes(u.space, 0, None, MAX_PAGE_NODES).await.unwrap()] {
        let save = |id| match &find(&listing.nodes, id).kind {
            NodeKind::File(f) => f.save.clone(),
            NodeKind::Dir => None,
        };
        assert_eq!(save(saved), Some(record.clone()));
        assert_eq!(save(plain), None);
        assert_eq!(save(huge), None, "over the limit: stored without it");
    }
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

async fn account_usage_estimates_history_and_trash_without_adding_them(meta: &Meta) {
    let ana = user(meta, "ana").await;
    let dev = device(meta, &ana).await;
    let empty = meta.storage_estimate(ana.space).await.unwrap();
    assert_eq!((empty.stored_bytes, empty.trash_bytes, empty.history_bytes), (0, Some(0), Some(0)));
    assert_eq!(empty.reason, None);
    let _asset_manifest = manifest(meta, &ana, 3).await;
    let assets = meta.storage_estimate(ana.space).await.unwrap();
    assert_eq!((assets.trash_bytes, assets.history_bytes), (Some(0), Some(0)), "unrooted app assets are not freeable");

    let (old, current) = (manifest(meta, &ana, 4).await, manifest(meta, &ana, 6).await);
    let node = NodeId::generate();
    let first = version(old);
    meta.commit(ana.space, dev, &[Op::Create { node, parent: root(meta, &ana).await, name: name("file.txt"), version: first.clone() }], NOW)
        .await
        .unwrap();
    let live = meta.storage_estimate(ana.space).await.unwrap();
    assert_eq!((live.trash_bytes, live.history_bytes), (Some(0), Some(0)));

    let latest = version(current);
    meta.commit(ana.space, dev, &[Op::Update { node, expected: first.id, version: latest.clone() }], NOW).await.unwrap();
    let historical = meta.storage_estimate(ana.space).await.unwrap();
    assert_eq!(historical.history_bytes, Some(105), "old chunk's 5 stored bytes plus its 100-byte manifest");
    assert_eq!(historical.trash_bytes, Some(0));
    assert_eq!(historical.stored_bytes, meta.stored_bytes(ana.space).await.unwrap());

    meta.commit(ana.space, dev, &[Op::Delete { node, expected: Some(latest.id) }], NOW).await.unwrap();
    let trashed = meta.storage_estimate(ana.space).await.unwrap();
    assert_eq!(trashed.trash_bytes, Some(212), "both versions' chunks and manifests belong to trash");
    assert_eq!(trashed.history_bytes, Some(0), "trashed node history is not history estimate");
    assert_eq!(trashed.stored_bytes, historical.stored_bytes, "labeled estimates never add to stored bytes");
}

async fn account_usage_excludes_chunks_shared_with_current_version(meta: &Meta) {
    let ana = user(meta, "ana").await;
    let dev = device(meta, &ana).await;
    let shared = random_chunk();
    meta.add_chunk(ana.space, &shared, 6, NOW).await.unwrap();
    let old =
        Manifest::new(ChunkerParamsId::new("fastcdc-v1").unwrap(), 5, FileHash::from_bytes([4; 32]), vec![ChunkRef { id: shared, len: 5 }]).unwrap();
    let current =
        Manifest::new(ChunkerParamsId::new("fastcdc-v1").unwrap(), 5, FileHash::from_bytes([5; 32]), vec![ChunkRef { id: shared, len: 5 }]).unwrap();
    meta.add_manifest(ana.space, &old, 100, NOW).await.unwrap();
    meta.add_manifest(ana.space, &current, 100, NOW).await.unwrap();
    let node = NodeId::generate();
    let first = version(old.id());
    meta.commit(ana.space, dev, &[Op::Create { node, parent: root(meta, &ana).await, name: name("shared.txt"), version: first.clone() }], NOW)
        .await
        .unwrap();
    meta.commit(ana.space, dev, &[Op::Update { node, expected: first.id, version: version(current.id()) }], NOW).await.unwrap();

    assert_eq!(meta.storage_estimate(ana.space).await.unwrap().history_bytes, Some(100), "the 6-byte chunk also belongs to the current version");
}

async fn account_usage_deduplicates_per_space_and_counts_pending_purge(meta: &Meta) {
    let (ana, bob) = (user(meta, "ana").await, user(meta, "bob").await);
    let (ana_device, bob_device) = (device(meta, &ana).await, device(meta, &bob).await);
    let shared = random_chunk();
    meta.add_chunk(ana.space, &shared, 6, NOW).await.unwrap();
    meta.add_chunk(bob.space, &shared, 999, NOW).await.unwrap();

    let make_manifest = |file_hash| {
        Manifest::new(ChunkerParamsId::new("fastcdc-v1").unwrap(), 5, FileHash::from_bytes(file_hash), vec![ChunkRef { id: shared, len: 5 }]).unwrap()
    };
    let (trash_manifest, live_manifest, bob_manifest) = (make_manifest([1; 32]), make_manifest([2; 32]), make_manifest([3; 32]));
    for (owner, manifest) in [(&ana, &trash_manifest), (&ana, &live_manifest), (&bob, &bob_manifest)] {
        meta.add_manifest(owner.space, manifest, 100, NOW).await.unwrap();
    }

    let (ana_trash, ana_live) = (NodeId::generate(), NodeId::generate());
    let (trash_version, live_version) = (version(trash_manifest.id()), version(live_manifest.id()));
    meta.commit(
        ana.space,
        ana_device,
        &[
            Op::Create { node: ana_trash, parent: root(meta, &ana).await, name: name("trash.txt"), version: trash_version.clone() },
            Op::Create { node: ana_live, parent: root(meta, &ana).await, name: name("live.txt"), version: live_version },
            Op::Delete { node: ana_trash, expected: Some(trash_version.id) },
        ],
        NOW,
    )
    .await
    .unwrap();
    let estimate = meta.storage_estimate(ana.space).await.unwrap();
    assert_eq!(estimate.trash_bytes, Some(100), "shared chunk is also named by a live manifest");
    assert_eq!(estimate.history_bytes, Some(0));
    meta.request_purge(ana.space, ana_trash, NOW).await.unwrap();
    assert_eq!(meta.storage_estimate(ana.space).await.unwrap().trash_bytes, Some(100), "pending purge still stores trash blobs");

    let bob_trash = NodeId::generate();
    let bob_version = version(bob_manifest.id());
    meta.commit(
        bob.space,
        bob_device,
        &[
            Op::Create { node: bob_trash, parent: root(meta, &bob).await, name: name("shared.txt"), version: bob_version.clone() },
            Op::Delete { node: bob_trash, expected: Some(bob_version.id) },
        ],
        NOW,
    )
    .await
    .unwrap();
    assert_eq!(meta.storage_estimate(bob.space).await.unwrap().trash_bytes, Some(106), "shared global chunk counts once in Bob's space");
}

async fn account_usage_excludes_live_document_snapshot_blobs(meta: &Meta) {
    let ana = user(meta, "ana").await;
    let dev = device(meta, &ana).await;
    let old = manifest(meta, &ana, 4).await;
    let current = manifest(meta, &ana, 6).await;
    let (node, first) = (NodeId::generate(), version(old));
    meta.commit(ana.space, dev, &[Op::Create { node, parent: root(meta, &ana).await, name: name("document.kitdemo"), version: first.clone() }], NOW)
        .await
        .unwrap();
    let doc = craft_cloud_proto::live::node_doc(ana.space, node);
    meta.create_live_document(ana.space, doc, "document:wordcraft", node, &old, NOW).await.unwrap();
    meta.commit(ana.space, dev, &[Op::Update { node, expected: first.id, version: version(current) }], NOW).await.unwrap();

    let estimate = meta.storage_estimate(ana.space).await.unwrap();
    assert_eq!(estimate.history_bytes, Some(0), "old version's manifest and chunk remain named by the live-document snapshot");
}

async fn account_usage_leaves_saved_versions_out_of_history(meta: &Meta) {
    let ana = user(meta, "ana").await;
    let dev = device(meta, &ana).await;
    let shared = random_chunk();
    meta.add_chunk(ana.space, &shared, 6, NOW).await.unwrap();
    let make_manifest = |file_hash| {
        Manifest::new(ChunkerParamsId::new("fastcdc-v1").unwrap(), 5, FileHash::from_bytes(file_hash), vec![ChunkRef { id: shared, len: 5 }]).unwrap()
    };
    let (saved_old, unsaved_old) = (make_manifest([7; 32]), make_manifest([8; 32]));
    for m in [&saved_old, &unsaved_old] {
        meta.add_manifest(ana.space, m, 100, NOW).await.unwrap();
    }
    let current = manifest(meta, &ana, 6).await;

    // v1 (saved) and v2 (not saved) name one chunk; v3 is current. The saved version's manifest and the chunk it shares
    // with unsaved history stay out of the estimate: only v2's own 100-byte manifest is history.
    let (node, v1) = (NodeId::generate(), version(saved_old.id()));
    meta.commit(ana.space, dev, &[Op::Create { node, parent: root(meta, &ana).await, name: name("saved.kitdemo"), version: v1.clone() }], NOW)
        .await
        .unwrap();
    let record = craft_cloud_proto::SaveRecord { doc: craft_cloud_proto::live::node_doc(ana.space, node), frontiers: vec![(1, 5)] };
    meta.attach_save(ana.space, node, v1.id, &record).await.unwrap();
    let v2 = version(unsaved_old.id());
    meta.commit(ana.space, dev, &[Op::Update { node, expected: v1.id, version: v2.clone() }], NOW).await.unwrap();
    let v3 = version(current);
    meta.commit(ana.space, dev, &[Op::Update { node, expected: v2.id, version: v3.clone() }], NOW).await.unwrap();
    assert_eq!(
        meta.storage_estimate(ana.space).await.unwrap().history_bytes,
        Some(100),
        "the saved version's manifest and the chunk it shares with unsaved history are kept"
    );

    // A saved version whose chunk nothing else names contributes nothing; an unsaved old version still counts in full.
    let (exclusive_saved, exclusive_unsaved) = (manifest(meta, &ana, 4).await, manifest(meta, &ana, 4).await);
    let (other, w1) = (NodeId::generate(), version(exclusive_saved));
    meta.commit(ana.space, dev, &[Op::Create { node: other, parent: root(meta, &ana).await, name: name("other.kitdemo"), version: w1.clone() }], NOW)
        .await
        .unwrap();
    let other_record = craft_cloud_proto::SaveRecord { doc: craft_cloud_proto::live::node_doc(ana.space, other), frontiers: vec![(1, 5)] };
    meta.attach_save(ana.space, other, w1.id, &other_record).await.unwrap();
    let w2 = version(exclusive_unsaved);
    meta.commit(ana.space, dev, &[Op::Update { node: other, expected: w1.id, version: w2.clone() }], NOW).await.unwrap();
    meta.commit(ana.space, dev, &[Op::Update { node: other, expected: w2.id, version: version(manifest(meta, &ana, 6).await) }], NOW).await.unwrap();
    assert_eq!(
        meta.storage_estimate(ana.space).await.unwrap().history_bytes,
        Some(205),
        "unsaved 100 + 105 for the second file's unsaved old version"
    );

    // In the trash a saved version still counts: purging the node removes it with the rest.
    meta.commit(ana.space, dev, &[Op::Delete { node, expected: Some(v3.id) }], NOW).await.unwrap();
    let trashed = meta.storage_estimate(ana.space).await.unwrap();
    assert_eq!(trashed.history_bytes, Some(105));
    assert_eq!(trashed.trash_bytes, Some(313), "three manifests, the shared 6-byte chunk and the current version's 7-byte chunk");
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

/// A file of `u`'s space (a node with a version), at the root under `file_name`.
async fn file(meta: &Meta, u: &UserRecord, file_name: &str) -> NodeId {
    let dev = device(meta, u).await;
    let r = root(meta, u).await;
    let m = manifest(meta, u, 10).await;
    let node = NodeId::generate();
    meta.commit(u.space, dev, &[Op::Create { node, parent: r, name: name(file_name), version: version(m) }], NOW).await.unwrap();
    node
}

/// A snapshot of `size` bytes uploaded as `u`'s (its chunks and the manifest that names them), as an agent uploads one.
async fn snapshot(meta: &Meta, u: &UserRecord, size: u64) -> craft_cloud_meta::SnapshotRef {
    let max = u64::from(craft_cloud_proto::limits::MAX_CHUNK_BYTES);
    let mut chunks = Vec::new();
    let mut left = size;
    while left > 0 {
        let len = left.min(max);
        let chunk = ChunkId::from_bytes(*blake3::hash(uuid::Uuid::now_v7().as_bytes()).as_bytes());
        meta.add_chunk(u.space, &chunk, len + 1, NOW).await.unwrap();
        chunks.push(ChunkRef { id: chunk, len: u32::try_from(len).unwrap() });
        left -= len;
    }
    let m = Manifest::new(ChunkerParamsId::new("fastcdc-v1").unwrap(), size, FileHash::from_bytes([1; 32]), chunks).unwrap();
    meta.add_manifest(u.space, &m, 100, NOW).await.unwrap();
    craft_cloud_meta::SnapshotRef { manifest: m.id(), bytes: size }
}

async fn a_document_is_made_by_import_once(meta: &Meta) {
    use craft_cloud_proto::live::node_doc;
    let ana = user(meta, "ana").await;
    let bo = user(meta, "bo").await;
    let node = file(meta, &ana, "a.txt").await;
    let doc = node_doc(ana.space, node);
    let first = snapshot(meta, &ana, 100).await;
    let before = meta.stored_bytes(ana.space).await.unwrap();
    let (made, kept) = meta.create_live_document(ana.space, doc, "document:wordcraft", node, &first.manifest, NOW).await.unwrap();
    assert!(kept, "the first import is kept");
    assert_eq!((made.node, made.base, made.has_snapshot, made.snapshot_bytes, made.head), (Some(node), 0, true, 100, 0));
    assert_eq!(meta.stored_bytes(ana.space).await.unwrap(), before, "charged when it was uploaded, not again");
    // Another machine's import of the same file (even by another app): not kept, nothing changes.
    let second = snapshot(meta, &ana, 50).await;
    let (again, kept) = meta.create_live_document(ana.space, doc, "document:gridcraft", node, &second.manifest, NOW + 1).await.unwrap();
    assert!(!kept);
    assert_eq!(again, made);
    assert_eq!(meta.live_snapshot(ana.space, doc).await.unwrap(), Some((0, first)), "the first snapshot stays");
    // Only for a file of the space that is there.
    let one = snapshot(meta, &ana, 1).await.manifest;
    let missing = NodeId::generate();
    assert!(!meta.is_live_file(ana.space, missing).await.unwrap());
    let refused = meta.create_live_document(ana.space, node_doc(ana.space, missing), "document:wordcraft", missing, &one, NOW).await;
    assert!(matches!(refused, Err(MetaError::NotFound(_))), "{refused:?}");
    let r = root(meta, &ana).await;
    assert!(!meta.is_live_file(ana.space, r).await.unwrap());
    let refused = meta.create_live_document(ana.space, node_doc(ana.space, r), "document:wordcraft", r, &one, NOW).await;
    assert!(matches!(refused, Err(MetaError::NotFound(_))), "a folder: {refused:?}");
    let bos = snapshot(meta, &bo, 1).await.manifest;
    let refused = meta.create_live_document(bo.space, node_doc(bo.space, node), "document:wordcraft", node, &bos, NOW).await;
    assert!(matches!(refused, Err(MetaError::NotFound(_))), "another account's file: {refused:?}");
    assert_eq!(meta.live_doc(bo.space, doc).await.unwrap(), None, "and another account's document");
    // Only with a snapshot the space uploaded.
    let other = file(meta, &ana, "other.txt").await;
    assert!(meta.is_live_file(ana.space, other).await.unwrap());
    let refused = meta.create_live_document(ana.space, node_doc(ana.space, other), "document:wordcraft", other, &bos, NOW).await;
    assert!(matches!(refused, Err(MetaError::NotFound(_))), "another account's manifest: {refused:?}");
    let never = ManifestId::from_bytes([9; 32]);
    let refused = meta.create_live_document(ana.space, node_doc(ana.space, other), "document:wordcraft", other, &never, NOW).await;
    assert!(matches!(refused, Err(MetaError::NotFound(_))), "a manifest never uploaded: {refused:?}");
    assert_eq!(meta.live_doc(ana.space, node_doc(ana.space, other)).await.unwrap(), None, "nothing recorded");
    // A deleted file.
    let gone = file(meta, &ana, "gone.txt").await;
    let dev = device(meta, &ana).await;
    let tree = full_tree(meta, ana.space).await;
    let current = match &find(&tree.nodes, gone).kind {
        NodeKind::File(state) => state.version,
        _ => panic!("not a file"),
    };
    meta.commit(ana.space, dev, &[Op::Delete { node: gone, expected: Some(current) }], NOW).await.unwrap();
    assert!(!meta.is_live_file(ana.space, gone).await.unwrap());
    let refused = meta.create_live_document(ana.space, node_doc(ana.space, gone), "document:wordcraft", gone, &one, NOW).await;
    assert!(matches!(refused, Err(MetaError::NotFound(_))), "a deleted file: {refused:?}");
    // Of the right kind, and with a snapshot of a snapshot's size: up to the limit, not past it.
    assert!(matches!(meta.create_live_document(ana.space, doc, "recents:wordcraft", node, &first.manifest, NOW).await, Err(MetaError::Invalid(_))));
    let limit = craft_cloud_proto::live::MAX_SNAPSHOT_BYTES as u64;
    let empty = snapshot(meta, &ana, 0).await.manifest;
    let refused = meta.create_live_document(ana.space, node_doc(ana.space, other), "document:wordcraft", other, &empty, NOW).await;
    assert!(matches!(refused, Err(MetaError::Invalid(_))), "an empty one: {refused:?}");
    let over = snapshot(meta, &ana, limit + 1).await.manifest;
    let refused = meta.create_live_document(ana.space, node_doc(ana.space, other), "document:wordcraft", other, &over, NOW).await;
    assert!(matches!(refused, Err(MetaError::Invalid(_))), "one past the limit: {refused:?}");
    let big = snapshot(meta, &ana, limit).await;
    meta.create_live_document(ana.space, node_doc(ana.space, other), "document:wordcraft", other, &big.manifest, NOW).await.unwrap();
    assert_eq!(meta.live_snapshot(ana.space, node_doc(ana.space, other)).await.unwrap(), Some((0, big)));
}

async fn concurrent_imports_of_one_file_keep_one(meta: &Meta) {
    use craft_cloud_proto::live::node_doc;
    let ana = user(meta, "ana").await;
    let node = file(meta, &ana, "a.txt").await;
    let doc = node_doc(ana.space, node);
    let mut snapshots = Vec::new();
    for i in 0..8u64 {
        snapshots.push(snapshot(meta, &ana, 64 + i).await);
    }
    let tasks: Vec<_> = snapshots
        .into_iter()
        .map(|s| {
            let meta = meta.clone();
            tokio::spawn(async move {
                let (_, kept) = meta.create_live_document(ana.space, doc, "document:wordcraft", node, &s.manifest, NOW).await.unwrap();
                (kept, s)
            })
        })
        .collect();
    let mut kept = Vec::new();
    for task in tasks {
        let (was_kept, s) = task.await.unwrap();
        if was_kept {
            kept.push(s);
        }
    }
    assert_eq!(kept.len(), 1, "exactly one import is kept");
    assert_eq!(meta.live_snapshot(ana.space, doc).await.unwrap(), Some((0, kept.remove(0))), "and it is the document's");
}

async fn a_log_is_compacted_into_a_snapshot(meta: &Meta) {
    use craft_cloud_proto::DocId;
    let meta = meta.clone().with_live_doc_cap(450);
    let ana = user(&meta, "ana").await;
    let dev = device(&meta, &ana).await;
    let doc = DocId::generate();
    meta.create_live_doc(ana.space, doc, "recents:wordcraft", None, NOW).await.unwrap();
    for i in 1..=5u64 {
        let appended = meta.append_live_update_counted(ana.space, doc, dev, i, &[u8::try_from(i).unwrap(); 100], NOW).await.unwrap();
        assert_eq!((appended.seq, appended.base, appended.above_bytes, appended.repeat), (i, 0, i * 100, false));
    }
    let repeat = meta.append_live_update_counted(ana.space, doc, dev, 2, &[2; 100], NOW).await.unwrap();
    assert_eq!((repeat.seq, repeat.repeat), (2, true));
    assert!(meta.append_live_update(ana.space, doc, dev, 6, &[6; 10], NOW).await.is_err(), "500 bytes: full");
    let s = snapshot(&meta, &ana, 40).await;
    assert!(meta.compact_live_doc(ana.space, doc, 3, &s.manifest).await.unwrap());
    let compacted = meta.live_doc(ana.space, doc).await.unwrap().unwrap();
    assert_eq!((compacted.base, compacted.above_base(), compacted.head, compacted.snapshot_bytes), (3, 200, 5, 40), "rows 1-3 no longer count");
    assert_eq!((compacted.bytes, compacted.compacted_bytes), (500, 300), "but stay in the log");
    assert_eq!(meta.live_snapshot(ana.space, doc).await.unwrap(), Some((3, s)));
    assert_eq!(meta.live_updates(ana.space, doc, 0, 10).await.unwrap().len(), 5, "the compacted rows are kept");
    // Older, equal or past the head: ignored.
    let one = snapshot(&meta, &ana, 1).await.manifest;
    for (upto, why) in [(3, "equal"), (2, "older"), (6, "past the head"), (0, "nothing")] {
        assert!(!meta.compact_live_doc(ana.space, doc, upto, &one).await.unwrap(), "{why}");
    }
    // Only with a snapshot the space uploaded.
    let refused = meta.compact_live_doc(ana.space, doc, 4, &ManifestId::from_bytes([9; 32])).await;
    assert!(matches!(refused, Err(MetaError::NotFound(_))), "{refused:?}");
    assert_eq!(meta.live_snapshot(ana.space, doc).await.unwrap(), Some((3, s)), "unchanged");
    // Under the cap again: appends go on.
    assert_eq!(meta.append_live_update(ana.space, doc, dev, 6, &[6; 10], NOW).await.unwrap(), 6);
    assert!(meta.compact_live_doc(ana.space, doc, 6, &snapshot(&meta, &ana, 50).await.manifest).await.unwrap());
    assert_eq!(meta.live_doc(ana.space, doc).await.unwrap().unwrap().above_base(), 0);
    assert!(matches!(meta.compact_live_doc(ana.space, DocId::generate(), 1, &one).await, Err(MetaError::NotFound(_))));
    // A reset empties the log and drops the snapshot.
    meta.reset_live_doc(ana.space, doc).await.unwrap();
    let reset = meta.live_doc(ana.space, doc).await.unwrap().unwrap();
    assert_eq!((reset.head, reset.base, reset.bytes, reset.has_snapshot, reset.snapshot_bytes), (0, 0, 0, false, 0));
    assert_eq!(meta.live_snapshot(ana.space, doc).await.unwrap(), None);
}

async fn concurrent_compactions_take_one(meta: &Meta) {
    use craft_cloud_proto::DocId;
    let ana = user(meta, "ana").await;
    let dev = device(meta, &ana).await;
    let doc = DocId::generate();
    meta.create_live_doc(ana.space, doc, "recents:wordcraft", None, NOW).await.unwrap();
    for i in 1..=10u64 {
        meta.append_live_update(ana.space, doc, dev, i, &[1; 10], NOW).await.unwrap();
    }
    let mut snapshots = Vec::new();
    for i in 0..6u64 {
        snapshots.push((i, snapshot(meta, &ana, 8 + i).await.manifest));
    }
    let tasks: Vec<_> = snapshots
        .into_iter()
        .map(|(i, s)| {
            let meta = meta.clone();
            let upto = 4 + (i % 2) * 4;
            tokio::spawn(async move { (upto, meta.compact_live_doc(ana.space, doc, upto, &s).await.unwrap()) })
        })
        .collect();
    let mut taken = Vec::new();
    for task in tasks {
        let (upto, ok) = task.await.unwrap();
        if ok {
            taken.push(upto);
        }
    }
    let after = meta.live_doc(ana.space, doc).await.unwrap().unwrap();
    assert!(taken.len() <= 2 && taken.iter().filter(|u| **u == 4).count() <= 1 && taken.iter().filter(|u| **u == 8).count() <= 1, "{taken:?}");
    assert_eq!(after.base, *taken.iter().max().unwrap(), "the base is the furthest one taken");
    assert_eq!(after.above_base(), (10 - after.base) * 10, "each row stopped counting once");
    assert_eq!(after.bytes, 100);
}

/// Every row of every live document of a space, compacted ones too, counts towards its live bytes: over the cap, every
/// append is refused (nothing stored), and a reset gives the document's bytes back.
async fn a_space_s_live_bytes_are_capped(meta: &Meta) {
    use craft_cloud_proto::DocId;
    let meta = meta.clone().with_live_bytes_cap(250);
    let ana = user(&meta, "ana").await;
    let bo = user(&meta, "bo").await;
    let dev = device(&meta, &ana).await;
    let (a, b) = (DocId::generate(), DocId::generate());
    for doc in [a, b] {
        meta.create_live_doc(ana.space, doc, "recents:wordcraft", None, NOW).await.unwrap();
    }
    meta.append_live_update(ana.space, a, dev, 1, &[1; 100], NOW).await.unwrap();
    meta.append_live_update(ana.space, b, dev, 1, &[2; 100], NOW).await.unwrap();
    // Compacting doesn't give bytes back: the rows are kept.
    assert!(meta.compact_live_doc(ana.space, a, 1, &snapshot(&meta, &ana, 10).await.manifest).await.unwrap());
    assert_eq!(meta.append_live_update(ana.space, a, dev, 2, &[3; 50], NOW).await.unwrap(), 2, "250: at the cap");
    let refused = meta.append_live_update(ana.space, b, dev, 2, &[4; 1], NOW).await;
    assert!(matches!(refused, Err(MetaError::LiveFull { used: 250, limit: 250 })), "{refused:?}");
    assert_eq!(meta.live_doc(ana.space, b).await.unwrap().unwrap().head, 1, "nothing stored");
    // A repeat of a stored update is still acked.
    assert_eq!(meta.append_live_update(ana.space, a, dev, 2, &[3; 50], NOW).await.unwrap(), 2);
    // Another account has room of its own.
    let bo_dev = device(&meta, &bo).await;
    let theirs = DocId::generate();
    meta.create_live_doc(bo.space, theirs, "recents:wordcraft", None, NOW).await.unwrap();
    assert_eq!(meta.append_live_update(bo.space, theirs, bo_dev, 1, &[5; 200], NOW).await.unwrap(), 1);
    // A reset gives the document's bytes back.
    meta.reset_live_doc(ana.space, a).await.unwrap();
    assert_eq!(meta.append_live_update(ana.space, b, dev, 2, &[4; 100], NOW).await.unwrap(), 2);
}

async fn documents_of_files_have_caps_of_their_own(meta: &Meta) {
    use craft_cloud_proto::DocId;
    use craft_cloud_proto::live::node_doc;
    let meta = meta.clone().with_live_docs_cap(1).with_live_doc_rows_cap(1).with_live_documents_cap(3).with_live_held_documents_cap(2);
    let ana = user(&meta, "ana").await;
    let dev = device(&meta, &ana).await;
    let recents = DocId::generate();
    meta.create_live_doc(ana.space, recents, "recents:wordcraft", None, NOW).await.unwrap();
    meta.append_live_update(ana.space, recents, dev, 1, b"x", NOW).await.unwrap();
    // Documents of files count neither as recents rows nor as recents holding updates.
    let one = snapshot(&meta, &ana, 1).await.manifest;
    let mut docs = Vec::new();
    for i in 0..3 {
        let node = file(&meta, &ana, &format!("f{i}.txt")).await;
        let doc = node_doc(ana.space, node);
        meta.create_live_document(ana.space, doc, "document:wordcraft", node, &one, NOW).await.unwrap();
        docs.push(doc);
    }
    // Holding updates: two may, the third's first update is refused (and a held one goes on).
    for doc in &docs[..2] {
        assert_eq!(meta.append_live_update(ana.space, *doc, dev, 1, b"y", NOW).await.unwrap(), 1);
    }
    let refused = meta.append_live_update(ana.space, docs[2], dev, 1, b"y", NOW).await;
    assert!(matches!(refused, Err(MetaError::TooManyDocs { limit: 2 })), "{refused:?}");
    assert_eq!(meta.append_live_update(ana.space, docs[0], dev, 2, b"z", NOW).await.unwrap(), 2);
    let fourth = file(&meta, &ana, "f3.txt").await;
    let refused = meta.create_live_document(ana.space, node_doc(ana.space, fourth), "document:wordcraft", fourth, &one, NOW).await;
    assert!(matches!(refused, Err(MetaError::TooManyDocRows { limit: 3 })), "{refused:?}");
    assert!(matches!(
        meta.create_live_doc(ana.space, DocId::generate(), "recents:other", None, NOW).await,
        Err(MetaError::TooManyDocRows { limit: 1 })
    ));
}

/// The log doesn't hold a document's import: compaction replaces the snapshot, never the import, which a machine that
/// can't read the snapshot reads with the whole log. A reset drops both.
async fn the_import_stays_when_the_log_is_compacted(meta: &Meta) {
    use craft_cloud_proto::live::node_doc;
    let ana = user(meta, "ana").await;
    let dev = device(meta, &ana).await;
    let node = file(meta, &ana, "a.txt").await;
    let doc = node_doc(ana.space, node);
    let import = snapshot(meta, &ana, 20).await;
    meta.create_live_document(ana.space, doc, "document:wordcraft", node, &import.manifest, NOW).await.unwrap();
    assert_eq!(meta.live_import_snapshot(ana.space, doc).await.unwrap(), Some(import));
    meta.append_live_update(ana.space, doc, dev, 1, b"x", NOW).await.unwrap();
    let compacted = snapshot(meta, &ana, 30).await;
    assert!(meta.compact_live_doc(ana.space, doc, 1, &compacted.manifest).await.unwrap());
    assert_eq!(meta.live_snapshot(ana.space, doc).await.unwrap(), Some((1, compacted)));
    assert_eq!(meta.live_import_snapshot(ana.space, doc).await.unwrap(), Some(import), "the import stays");
    meta.reset_live_doc(ana.space, doc).await.unwrap();
    assert_eq!(meta.live_import_snapshot(ana.space, doc).await.unwrap(), None);
    assert_eq!(meta.live_import_snapshot(ana.space, craft_cloud_proto::DocId::generate()).await.unwrap(), None);
}

/// A recents document whose whole log was compacted still holds updates: it counts against the account's cap, and
/// its next update isn't taken for a first one.
async fn a_compacted_log_still_holds_updates(meta: &Meta) {
    use craft_cloud_proto::DocId;
    let meta = meta.clone().with_live_docs_cap(1);
    let ana = user(&meta, "ana").await;
    let dev = device(&meta, &ana).await;
    let (a, b) = (DocId::generate(), DocId::generate());
    for doc in [a, b] {
        meta.create_live_doc(ana.space, doc, "recents:wordcraft", None, NOW).await.unwrap();
    }
    meta.append_live_update(ana.space, a, dev, 1, b"x", NOW).await.unwrap();
    assert!(meta.compact_live_doc(ana.space, a, 1, &snapshot(&meta, &ana, 1).await.manifest).await.unwrap());
    assert_eq!(meta.live_doc(ana.space, a).await.unwrap().unwrap().above_base(), 0);
    assert!(matches!(meta.append_live_update(ana.space, b, dev, 1, b"y", NOW).await, Err(MetaError::TooManyDocs { limit: 1 })));
    assert_eq!(meta.append_live_update(ana.space, a, dev, 2, b"z", NOW).await.unwrap(), 2, "the compacted one goes on");
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

// ---- join codes ----

fn code(chars: usize) -> Code {
    Code::generate(chars).unwrap()
}

fn token() -> [u8; 32] {
    *blake3::hash(uuid::Uuid::now_v7().as_bytes()).as_bytes()
}

async fn redeem(meta: &Meta, code: &Code, name: &str, now: i64) -> Result<Redeemed, MetaError> {
    meta.redeem(&code.hash(), name, &token(), None, now).await
}

/// A signed-in device of a new account with a password, and a pairing it made.
async fn paired(meta: &Meta, username: &str) -> (UserRecord, DeviceId, Code, uuid::Uuid) {
    let u = user(meta, username).await;
    let maker = device(meta, &u).await;
    let pairing = code(PAIRING_CHARS);
    let id = meta.create_pairing(u.id, maker, &pairing.hash(), NOW).await.unwrap();
    (u, maker, pairing, id)
}

async fn a_pairing_code_works_once(meta: &Meta) {
    let (ana, _, pairing, _) = paired(meta, "ana").await;
    let token = token();
    let joined = meta.redeem(&pairing.hash(), "lptp1", &token, None, NOW + 1).await.unwrap();
    assert_eq!(joined.user, ana.id);
    let auth = meta.device_by_token(&token).await.unwrap().unwrap();
    assert_eq!((auth.device, auth.user, auth.space), (joined.device, ana.id, ana.space));
    assert!(matches!(redeem(meta, &pairing, "again", NOW + 2).await, Err(MetaError::InvalidCode)));
    assert!(matches!(redeem(meta, &code(PAIRING_CHARS), "wrong", NOW + 2).await, Err(MetaError::InvalidCode)));
    assert_eq!(meta.list_devices(ana.id).await.unwrap().len(), 2);

    let invite = code(INVITE_CHARS);
    meta.create_invite("ana", &invite.hash(), NOW, INVITE_TTL_MS).await.unwrap();
    redeem(meta, &invite, "beast", NOW + 3).await.unwrap();
    assert!(matches!(redeem(meta, &invite, "beast", NOW + 4).await, Err(MetaError::InvalidCode)));
    assert_eq!(meta.list_devices(ana.id).await.unwrap().len(), 3);
}

async fn an_expired_code_is_refused(meta: &Meta) {
    let (ana, _, pairing, _) = paired(meta, "ana").await;
    assert!(matches!(redeem(meta, &pairing, "late", NOW + PAIRING_TTL_MS).await, Err(MetaError::InvalidCode)));
    let invite = code(INVITE_CHARS);
    meta.create_invite("ana", &invite.hash(), NOW, INVITE_TTL_MS).await.unwrap();
    assert!(matches!(redeem(meta, &invite, "late", NOW + INVITE_TTL_MS).await, Err(MetaError::InvalidCode)));
    assert_eq!(meta.list_devices(ana.id).await.unwrap().len(), 1, "only the pairing's maker");
    redeem(meta, &invite, "in time", NOW + INVITE_TTL_MS - 1).await.unwrap();
}

async fn a_replaced_invite_s_old_code_fails(meta: &Meta) {
    let first = code(INVITE_CHARS);
    let made = meta.create_invite("Ana", &first.hash(), NOW, INVITE_TTL_MS).await.unwrap();
    assert!(made.new_account);
    let second = code(INVITE_CHARS);
    let again = meta.create_invite("ana", &second.hash(), NOW + 1, INVITE_TTL_MS).await.unwrap();
    assert_eq!(again, InviteFor { user: made.user, new_account: true }, "the same account, found by its lowercase name, still new");
    assert!(matches!(meta.redeem(&first.hash(), "pc", &token(), Some("$argon2id$x"), NOW + 2).await, Err(MetaError::InvalidCode)));
    meta.redeem(&second.hash(), "pc", &token(), Some("$argon2id$x"), NOW + 2).await.unwrap();
    assert!(matches!(meta.create_invite("bad name", &code(INVITE_CHARS).hash(), NOW, INVITE_TTL_MS).await, Err(MetaError::Invalid(_))));
}

async fn a_revoked_invite_fails(meta: &Meta) {
    let invite = code(INVITE_CHARS);
    meta.create_invite("ana", &invite.hash(), NOW, INVITE_TTL_MS).await.unwrap();
    assert!(meta.revoke_invite("ANA").await.unwrap());
    assert!(!meta.revoke_invite("ana").await.unwrap(), "nothing left to revoke");
    assert!(!meta.revoke_invite("nobody").await.unwrap());
    assert!(matches!(meta.redeem(&invite.hash(), "pc", &token(), Some("$argon2id$x"), NOW + 1).await, Err(MetaError::InvalidCode)));
    assert_eq!(meta.invite_needs_password(&invite.hash(), NOW + 1).await.unwrap(), None);
}

async fn a_pairing_is_waiting_then_used_then_expired(meta: &Meta) {
    let (ana, maker, pairing, id) = paired(meta, "ana").await;
    assert_eq!(meta.pairing_status(ana.id, id, NOW + 1).await.unwrap(), Some((PairingState::Waiting, None)));
    redeem(meta, &pairing, "lptp1", NOW + 2).await.unwrap();
    assert_eq!(meta.pairing_status(ana.id, id, NOW + 3).await.unwrap(), Some((PairingState::Used, Some("lptp1".to_owned()))));
    assert_eq!(
        meta.pairing_status(ana.id, id, NOW + PAIRING_TTL_MS + 1).await.unwrap(),
        Some((PairingState::Used, Some("lptp1".to_owned()))),
        "a used pairing stays used"
    );

    let unused = meta.create_pairing(ana.id, maker, &code(PAIRING_CHARS).hash(), NOW + 10).await.unwrap();
    assert_eq!(meta.pairing_status(ana.id, unused, NOW + 10 + PAIRING_TTL_MS - 1).await.unwrap(), Some((PairingState::Waiting, None)));
    assert_eq!(meta.pairing_status(ana.id, unused, NOW + 10 + PAIRING_TTL_MS).await.unwrap(), Some((PairingState::Expired, None)));
    meta.delete_pairing(ana.id, unused).await.unwrap();
    assert_eq!(meta.pairing_status(ana.id, unused, NOW + 11).await.unwrap(), None);
    meta.delete_pairing(ana.id, unused).await.unwrap();

    // Expired rows go at the next write to the table.
    let other = device(meta, &ana).await;
    let old = meta.create_pairing(ana.id, other, &code(PAIRING_CHARS).hash(), NOW).await.unwrap();
    meta.create_pairing(ana.id, maker, &code(PAIRING_CHARS).hash(), NOW + PAIRING_TTL_MS + 1).await.unwrap();
    assert_eq!(meta.pairing_status(ana.id, old, NOW + PAIRING_TTL_MS + 1).await.unwrap(), None);
}

async fn a_device_has_one_open_pairing(meta: &Meta) {
    let (ana, maker, first, first_id) = paired(meta, "ana").await;
    let second = code(PAIRING_CHARS);
    let second_id = meta.create_pairing(ana.id, maker, &second.hash(), NOW + 1).await.unwrap();
    assert_ne!(first_id, second_id);
    assert_eq!(meta.pairing_status(ana.id, first_id, NOW + 2).await.unwrap(), None);
    assert!(matches!(redeem(meta, &first, "pc", NOW + 2).await, Err(MetaError::InvalidCode)));
    redeem(meta, &second, "pc", NOW + 2).await.unwrap();
    let bob = user(meta, "bob").await;
    assert!(
        matches!(meta.create_pairing(bob.id, maker, &code(PAIRING_CHARS).hash(), NOW).await, Err(MetaError::NotFound(_))),
        "a pairing only for the account's own device"
    );
}

async fn another_users_pairing_is_invisible(meta: &Meta) {
    let (ana, _, _, id) = paired(meta, "ana").await;
    let bob = user(meta, "bob").await;
    assert_eq!(meta.pairing_status(bob.id, id, NOW + 1).await.unwrap(), None);
    meta.delete_pairing(bob.id, id).await.unwrap();
    assert_eq!(meta.pairing_status(ana.id, id, NOW + 1).await.unwrap(), Some((PairingState::Waiting, None)), "bob can't cancel it");
}

async fn a_pairing_dies_with_its_revoked_creator(meta: &Meta) {
    let (ana, maker, pairing, id) = paired(meta, "ana").await;
    meta.revoke_device(ana.id, maker).await.unwrap();
    assert_eq!(meta.pairing_status(ana.id, id, NOW + 1).await.unwrap(), None);
    assert!(matches!(redeem(meta, &pairing, "pc", NOW + 1).await, Err(MetaError::InvalidCode)));
    assert_eq!(meta.list_devices(ana.id).await.unwrap().len(), 1);
    let fresh = device(meta, &ana).await;
    assert!(matches!(meta.create_pairing(ana.id, maker, &code(PAIRING_CHARS).hash(), NOW).await, Err(MetaError::NotFound(_))), "revoked");
    meta.create_pairing(ana.id, fresh, &code(PAIRING_CHARS).hash(), NOW).await.unwrap();
}

async fn concurrent_redeems_of_one_code_give_one_device(meta: &Meta) {
    let (ana, _, pairing, _) = paired(meta, "ana").await;
    let invite = code(INVITE_CHARS);
    meta.create_invite("bob", &invite.hash(), NOW, INVITE_TTL_MS).await.unwrap();
    for (code, password) in [(pairing, None), (invite, Some("$argon2id$bob"))] {
        let tasks: Vec<_> = (0..6)
            .map(|i| {
                let (meta, hash) = (meta.clone(), code.hash());
                tokio::spawn(async move { meta.redeem(&hash, &format!("pc{i}"), &token(), password, NOW + 1).await })
            })
            .collect();
        let mut joined = 0;
        for task in tasks {
            match task.await.unwrap() {
                Ok(_) => joined += 1,
                Err(MetaError::InvalidCode) => {}
                Err(other) => panic!("unexpected error: {other:?}"),
            }
        }
        assert_eq!(joined, 1);
    }
    assert_eq!(meta.list_devices(ana.id).await.unwrap().len(), 2);
    let bob = meta.user_by_name("bob").await.unwrap().unwrap();
    assert_eq!(meta.list_devices(bob.id).await.unwrap().len(), 1);
}

async fn an_invite_to_an_account_with_a_password_needs_none(meta: &Meta) {
    let ana = user(meta, "ana").await;
    let invite = code(INVITE_CHARS);
    assert_eq!(meta.create_invite("ana", &invite.hash(), NOW, INVITE_TTL_MS).await.unwrap(), InviteFor { user: ana.id, new_account: false });
    assert_eq!(meta.invite_needs_password(&invite.hash(), NOW).await.unwrap(), Some(false));
    let joined = meta.redeem(&invite.hash(), "pc", &token(), Some("$argon2id$ignored"), NOW).await.unwrap();
    assert_eq!(joined.user, ana.id);
    assert_eq!(meta.user_by_name("ana").await.unwrap().unwrap().password_hash, ana.password_hash, "the password stays");
}

async fn an_invite_to_a_new_account_needs_a_password(meta: &Meta) {
    let invite = code(INVITE_CHARS);
    let made = meta.create_invite("ana", &invite.hash(), NOW, INVITE_TTL_MS).await.unwrap();
    assert!(made.new_account);
    let ana = meta.user_by_name("ana").await.unwrap().unwrap();
    assert_eq!((ana.password_hash.as_str(), ana.role), ("", Role::Member), "no password until the invite is redeemed");
    assert_eq!(meta.invite_needs_password(&invite.hash(), NOW).await.unwrap(), Some(true));
    assert_eq!(meta.invite_needs_password(&code(INVITE_CHARS).hash(), NOW).await.unwrap(), None);
    assert_eq!(meta.invite_needs_password(&invite.hash(), NOW + INVITE_TTL_MS).await.unwrap(), None, "expired");

    assert!(matches!(redeem(meta, &invite, "pc", NOW).await, Err(MetaError::PasswordRequired)));
    assert!(meta.list_devices(ana.id).await.unwrap().is_empty(), "nothing changed");
    assert_eq!(meta.user_by_name("ana").await.unwrap().unwrap().password_hash, "");
    assert_eq!(meta.invite_needs_password(&invite.hash(), NOW).await.unwrap(), Some(true), "the code still works");

    let joined = meta.redeem(&invite.hash(), "pc", &token(), Some("$argon2id$new"), NOW).await.unwrap();
    assert_eq!(joined.user, ana.id);
    assert_eq!(meta.user_by_name("ana").await.unwrap().unwrap().password_hash, "$argon2id$new");
    assert_eq!(meta.list_devices(ana.id).await.unwrap().len(), 1);
    assert!(meta.create_invite("ana", &code(INVITE_CHARS).hash(), NOW, INVITE_TTL_MS).await.is_ok_and(|i| !i.new_account));
}

// ---- plain tests ----

/// Neither table holds a code in plaintext: the database's files (with the write-ahead log) hold its hash and never
/// the code, in any of the forms it is shown or sent in.
#[tokio::test]
async fn only_the_hash_of_a_code_is_stored() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}", dir.path().join("meta.db").display());
    let meta = Meta::open(&url).await.unwrap();
    let (_, _, pairing, _) = paired(&meta, "ana").await;
    let invite = code(INVITE_CHARS);
    meta.create_invite("bob", &invite.hash(), NOW, INVITE_TTL_MS).await.unwrap();
    drop(meta);
    let mut bytes = Vec::new();
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        bytes.extend(std::fs::read(entry.unwrap().path()).unwrap());
    }
    let holds = |needle: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
    for code in [&pairing, &invite] {
        assert!(holds(&code.hash()), "the hash is there");
        assert!(!holds(code.as_str().as_bytes()) && !holds(code.display().as_bytes()));
    }
}

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
        // And before live documents of files.
        sqlx::query("ALTER TABLE spaces DROP COLUMN live_bytes").execute(&mut conn).await.unwrap();
        // And before live documents.
        sqlx::query("DROP TABLE live_updates").execute(&mut conn).await.unwrap();
        sqlx::query("DROP TABLE live_docs").execute(&mut conn).await.unwrap();
        sqlx::query("DROP TABLE live_spaces").execute(&mut conn).await.unwrap();
        // And before join codes.
        sqlx::query("DROP TABLE pairings").execute(&mut conn).await.unwrap();
        sqlx::query("DROP TABLE invites").execute(&mut conn).await.unwrap();
        // And before save records.
        // And before the reference index.
        sqlx::query("DROP TABLE manifest_chunks").execute(&mut conn).await.unwrap();
        sqlx::query("DROP TABLE manifest_index").execute(&mut conn).await.unwrap();
        // And before the trash.
        sqlx::query("DROP INDEX nodes_trash").execute(&mut conn).await.unwrap();
        sqlx::query("ALTER TABLE nodes DROP COLUMN deleted_ms").execute(&mut conn).await.unwrap();
        sqlx::query("ALTER TABLE nodes DROP COLUMN purge_requested_ms").execute(&mut conn).await.unwrap();
        // And before the history index.
        sqlx::query("DROP INDEX versions_node_seq").execute(&mut conn).await.unwrap();
        sqlx::query("ALTER TABLE versions DROP COLUMN save").execute(&mut conn).await.unwrap();
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
    assert_eq!(versions, 9);
}

/// A file with `n` versions, one commit each (the first by create): its node and the versions, oldest first.
async fn file_with_versions(meta: &Meta, u: &UserRecord, dev: DeviceId, n: usize) -> (NodeId, Vec<NewVersion>) {
    let r = root(meta, u).await;
    let m = manifest(meta, u, 3).await;
    let node = NodeId::generate();
    let mut versions = vec![version(m)];
    meta.commit(u.space, dev, &[Op::Create { node, parent: r, name: name("history.txt"), version: versions[0].clone() }], NOW).await.unwrap();
    for i in 1..n {
        let m = manifest(meta, u, 3 + i as u32).await;
        let next = NewVersion { mtime_ms: NOW + i as i64, ..version(m) };
        let expected = versions[i - 1].id;
        meta.commit(u.space, dev, &[Op::Update { node, expected, version: next.clone() }], NOW + i as i64).await.unwrap();
        versions.push(next);
    }
    (node, versions)
}

async fn file_named(meta: &Meta, u: &UserRecord, dev: DeviceId, file_name: &str) -> (NodeId, NewVersion) {
    let r = root(meta, u).await;
    let node = NodeId::generate();
    let v = version(manifest(meta, u, 5).await);
    meta.commit(u.space, dev, &[Op::Create { node, parent: r, name: name(file_name), version: v.clone() }], NOW).await.unwrap();
    (node, v)
}

fn refused_restore<T: std::fmt::Debug>(result: Result<T, MetaError>) -> Vec<(usize, ConflictKind)> {
    match result {
        Err(MetaError::Rejected(list)) => list.into_iter().map(|c| (c.op, c.kind)).collect(),
        other => panic!("expected a refused restore, got {other:?}"),
    }
}

/// The history comes newest first in pages that follow each other without gaps or repeats, also across versions that
/// share one sequence number (a batch of updates); only files of the caller's own space have one; a deleted file keeps
/// its history; `pruned` is false and `save` tells whether a record hangs on the version.
async fn history_pages_newest_first_for_own_files_only(meta: &Meta) {
    use craft_cloud_proto::api::{HISTORY_MAX_LIMIT, HistoryCursor, HistoryVersion};
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let (node, mut versions) = file_with_versions(meta, &u, dev, 4).await;
    // Three more versions in one commit: they share a sequence number, so the order inside it is by ID.
    let (mut ops, mut prev) = (Vec::new(), versions[3].id);
    for i in 0..3 {
        let next = version(manifest(meta, &u, 20 + i).await);
        ops.push(Op::Update { node, expected: prev, version: next.clone() });
        prev = next.id;
        versions.push(next);
    }
    meta.commit(u.space, dev, &ops, NOW + 100).await.unwrap();
    let head = prev;
    let epoch = meta.space_info(u.space).await.unwrap().epoch;

    let all = meta.history(u.space, node, None, HISTORY_MAX_LIMIT).await.unwrap();
    assert_eq!(all.next, None);
    assert_eq!(all.versions.len(), 7);
    let key = |v: &HistoryVersion| (v.seq, *v.id.as_uuid().as_bytes());
    assert!(all.versions.windows(2).all(|w| key(&w[0]) > key(&w[1])), "strictly descending by (seq, id)");
    assert!(all.versions.iter().all(|v| !v.pruned && !v.save && v.device == dev && v.epoch == epoch));
    assert_eq!(all.versions.iter().filter(|v| v.seq == 5).count(), 3, "the batch shares one sequence number");
    let oldest = all.versions.last().unwrap();
    assert_eq!((oldest.id, oldest.parent_version, oldest.seq, oldest.size), (versions[0].id, None, 1, 3));
    let second = &all.versions[all.versions.len() - 2];
    assert_eq!((second.id, second.parent_version, second.mtime_ms, second.size), (versions[1].id, Some(versions[0].id), NOW + 1, 4));

    for limit in [1u32, 2, 3] {
        let (mut seen, mut after, mut pages) = (Vec::new(), None, 0);
        loop {
            let page = meta.history(u.space, node, after, limit).await.unwrap();
            assert!(page.versions.len() <= limit as usize);
            seen.extend(page.versions.iter().cloned());
            pages += 1;
            match page.next {
                Some(next) => {
                    let last = page.versions.last().unwrap();
                    assert_eq!(next, HistoryCursor { seq: last.seq, version: last.id });
                    after = Some(next);
                }
                None => break,
            }
        }
        assert_eq!(seen, all.versions, "limit {limit}: every version once, in order");
        assert_eq!(pages, 7usize.div_ceil(limit as usize));
    }

    // The limit is 1 to 200; a cursor past the end gives an empty last page.
    assert!(matches!(meta.history(u.space, node, None, 0).await, Err(MetaError::Invalid(_))));
    assert!(matches!(meta.history(u.space, node, None, HISTORY_MAX_LIMIT + 1).await, Err(MetaError::Invalid(_))));
    let past = meta.history(u.space, node, Some(HistoryCursor { seq: 0, version: VersionId::generate() }), 10).await.unwrap();
    assert_eq!((past.versions.len(), past.next), (0, None));

    // A record hanging on a version shows as `save`.
    let record = craft_cloud_proto::SaveRecord { doc: craft_cloud_proto::live::node_doc(u.space, node), frontiers: vec![(1, 5)] };
    meta.attach_save(u.space, node, versions[1].id, &record).await.unwrap();
    let with_save: Vec<_> = meta.history(u.space, node, None, HISTORY_MAX_LIMIT).await.unwrap().versions.into_iter().filter(|v| v.save).collect();
    assert_eq!(with_save.iter().map(|v| v.id).collect::<Vec<_>>(), vec![versions[1].id]);

    // Folders have no history, another account's node is not found, and a deleted file keeps its history.
    let r = root(meta, &u).await;
    assert!(matches!(meta.history(u.space, r, None, 10).await, Err(MetaError::Invalid(_))));
    assert!(matches!(meta.history(u.space, NodeId::generate(), None, 10).await, Err(MetaError::NotFound(_))));
    let bob = user(meta, "bob").await;
    assert!(matches!(meta.history(bob.space, node, None, 10).await, Err(MetaError::NotFound(_))));
    meta.commit(u.space, dev, &[Op::Delete { node, expected: Some(head) }], NOW + 200).await.unwrap();
    assert_eq!(meta.history(u.space, node, None, HISTORY_MAX_LIMIT).await.unwrap().versions.len(), 7);
}

/// A restore is an ordinary update after the head: a fresh version with the old content (manifest, size, mtime), the
/// head as its parent, the next sequence number, the same epoch and no save record; it shows in the change feed, and
/// the old version stays where it was.
async fn a_restore_is_a_new_version_after_the_head(meta: &Meta) {
    use craft_cloud_proto::api::HISTORY_MAX_LIMIT;
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let other = meta.create_device(u.id, "phone", &[9u8; 32], NOW).await.unwrap();
    let (node, versions) = file_with_versions(meta, &u, dev, 3).await;
    let record = craft_cloud_proto::SaveRecord { doc: craft_cloud_proto::live::node_doc(u.space, node), frontiers: vec![(1, 5)] };
    meta.attach_save(u.space, node, versions[0].id, &record).await.unwrap();
    let before = meta.space_info(u.space).await.unwrap();

    let done = meta.restore_version(u.space, other, node, versions[0].id, versions[2].id, NOW + 50).await.unwrap();
    assert_eq!((done.parent_version, done.epoch, done.seq), (versions[2].id, before.epoch, before.seq + 1));
    assert!(!versions.iter().any(|v| v.id == done.version), "a fresh version ID");
    let after = meta.space_info(u.space).await.unwrap();
    assert_eq!((after.seq, after.epoch), (before.seq + 1, before.epoch));

    let feed = meta.changes(u.space, before.seq, None, MAX_PAGE_NODES).await.unwrap();
    let NodeKind::File(f) = &find(&feed.nodes, node).kind else { panic!("a file") };
    assert_eq!(
        (f.version, f.manifest, f.mtime_ms, f.device, f.save.clone()),
        (done.version, versions[0].manifest, versions[0].mtime_ms, other, None)
    );

    let history = meta.history(u.space, node, None, HISTORY_MAX_LIMIT).await.unwrap().versions;
    assert_eq!(history.len(), 4);
    assert_eq!(history[0].id, done.version);
    assert_eq!((history[0].parent_version, history[0].size, history[0].save, history[0].seq), (Some(versions[2].id), 3, false, done.seq));
    assert!(history.iter().any(|v| v.id == versions[0].id && v.save), "the old version and its record stay");

    // The restored content is the head now: a stale `expected` is refused and changes nothing.
    let stale = meta.restore_version(u.space, dev, node, versions[1].id, versions[2].id, NOW + 60).await;
    assert_eq!(refused_restore(stale), vec![(0, ConflictKind::VersionMismatch { current: Some(done.version) })]);
    assert_eq!(meta.space_info(u.space).await.unwrap().seq, after.seq);
    // Restoring the content that is already current is allowed: it is one more version.
    let again = meta.restore_version(u.space, dev, node, done.version, done.version, NOW + 70).await.unwrap();
    assert_eq!(again.seq, after.seq + 1);
}

/// What a restore turns down, each leaving the space as it was: a folder, a deleted file, a file with a live document,
/// a node or version that is not there, a version of another file, and anything of another account.
async fn a_restore_refuses_what_it_can_not_restore(meta: &Meta) {
    let u = user(meta, "ana").await;
    let (space, dev) = (u.space, device(meta, &u).await);
    let (file, versions) = file_with_versions(meta, &u, dev, 2).await;
    let (second, second_version) = file_named(meta, &u, dev, "second.txt").await;
    let r = root(meta, &u).await;
    let seq = meta.space_info(space).await.unwrap().seq;
    let (old, head) = (versions[0].id, versions[1].id);
    let restore = |node: NodeId, source: VersionId, expected: VersionId| {
        let meta = meta.clone();
        async move { meta.restore_version(space, dev, node, source, expected, NOW + 9).await }
    };

    assert!(matches!(restore(r, old, head).await, Err(MetaError::Invalid(_))), "a folder");
    assert!(matches!(restore(NodeId::generate(), old, head).await, Err(MetaError::NotFound(_))), "no such node");
    assert!(matches!(restore(file, VersionId::generate(), head).await, Err(MetaError::NotFound(_))), "no such version");
    assert!(matches!(restore(file, second_version.id, head).await, Err(MetaError::NotFound(_))), "a version of another file");
    // The source is checked before the head: a missing source is not a conflict.
    assert!(matches!(restore(file, VersionId::generate(), VersionId::generate()).await, Err(MetaError::NotFound(_))));
    assert_eq!(refused_restore(restore(file, old, VersionId::generate()).await).len(), 1, "a head that is not current");

    let bob = user(meta, "bob").await;
    let bob_device = device(meta, &bob).await;
    assert!(matches!(meta.restore_version(bob.space, bob_device, file, old, head, NOW).await, Err(MetaError::NotFound(_))), "another space's node");

    // A file with a live document.
    let doc = craft_cloud_proto::live::node_doc(space, second);
    meta.create_live_doc(space, doc, "document:kitdemo", Some(second), NOW).await.unwrap();
    assert!(matches!(restore(second, second_version.id, second_version.id).await, Err(MetaError::Invalid(_))), "a live document");
    assert_eq!(meta.space_info(space).await.unwrap().seq, seq, "nothing changed so far");

    // A deleted file.
    meta.commit(space, dev, &[Op::Delete { node: file, expected: Some(head) }], NOW + 5).await.unwrap();
    assert!(matches!(restore(file, old, head).await, Err(MetaError::Invalid(_))), "a deleted file");
    assert_eq!(meta.space_info(space).await.unwrap().seq, seq + 1, "only the delete");
    let feed = meta.changes(space, seq, None, MAX_PAGE_NODES).await.unwrap();
    assert!(feed.nodes.len() == 1 && find(&feed.nodes, file).deleted, "still deleted");
}

/// Several restores of different old versions against one head: exactly one applies, the rest are refused with a
/// version mismatch, and the space moves one step.
async fn concurrent_restores_let_exactly_one_win(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let (node, versions) = file_with_versions(meta, &u, dev, 3).await;
    let head = versions[2].id;
    let before = meta.space_info(u.space).await.unwrap().seq;
    let mut tasks = Vec::new();
    for i in 0..8 {
        let (meta, space, source) = (meta.clone(), u.space, versions[i % 2].id);
        tasks.push(tokio::spawn(async move { meta.restore_version(space, dev, node, source, head, NOW + 20).await }));
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
    assert_eq!(meta.space_info(u.space).await.unwrap().seq, before + 1);
}

/// A restore and a late save record for the head it replaces run together: both apply under the space's one lock, the
/// record stays on the version it was meant for, and the restored version never gets a record.
async fn a_restore_and_a_late_save_record_both_apply(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let (node, versions) = file_with_versions(meta, &u, dev, 3).await;
    let (head, old) = (versions[2].id, versions[0].id);
    let record = craft_cloud_proto::SaveRecord { doc: craft_cloud_proto::live::node_doc(u.space, node), frontiers: vec![(1, 5)] };
    let (m1, m2, space) = (meta.clone(), meta.clone(), u.space);
    let save = tokio::spawn(async move { m1.attach_save(space, node, head, &record).await });
    let restore = tokio::spawn(async move { m2.restore_version(space, dev, node, old, head, NOW + 30).await });
    save.await.unwrap().unwrap();
    let restored = restore.await.unwrap().unwrap();
    let history = meta.history(space, node, None, 50).await.unwrap().versions;
    assert_eq!(history.len(), 4);
    for v in &history {
        assert_eq!(v.save, v.id == head, "only the old head has the record: {}", v.id);
    }
    assert_eq!(history[0].id, restored.version);
    // The create, two updates, the record and the restore.
    assert_eq!(meta.space_info(space).await.unwrap().seq, 5);
}

/// The database at the newest version when the code knows it: the version table holds one row per migration, and a
/// database written by a newer build is refused rather than half-read.
#[tokio::test]
async fn the_schema_version_is_recorded_and_a_newer_database_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}", dir.path().join("meta.db").display());
    drop(Meta::open(&url).await.unwrap());
    use sqlx::Connection;
    let mut conn = sqlx::SqliteConnection::connect(&url).await.unwrap();
    let latest: i64 = sqlx::query_scalar("SELECT MAX(version) FROM schema_version").fetch_one(&mut conn).await.unwrap();
    assert_eq!(latest, 9);
    // Opening again changes nothing.
    drop(Meta::open(&url).await.unwrap());
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM schema_version").fetch_one(&mut conn).await.unwrap();
    assert_eq!(rows, 9);
    sqlx::query("INSERT INTO schema_version (version) VALUES (10)").execute(&mut conn).await.unwrap();
    drop(conn);
    assert!(matches!(Meta::open(&url).await, Err(MetaError::Corrupt(_))), "a newer database is refused");
}

/// A history page is read through the index on `(space_id, node, seq, id)`, not by scanning the versions table.
#[tokio::test]
async fn a_history_page_uses_the_node_index_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}", dir.path().join("meta.db").display());
    drop(Meta::open(&url).await.unwrap());
    use sqlx::Connection;
    let mut conn = sqlx::SqliteConnection::connect(&url).await.unwrap();
    let plan = sqlx::query(
        "EXPLAIN QUERY PLAN SELECT id FROM versions WHERE space_id = ?1 AND node = ?2 AND (seq, id) < (?3, ?4) ORDER BY seq DESC, id DESC LIMIT ?5",
    )
    .bind(vec![0u8; 16])
    .bind(vec![0u8; 16])
    .bind(i64::MAX)
    .bind(vec![0xffu8; 16])
    .bind(51i64)
    .fetch_all(&mut conn)
    .await
    .unwrap();
    use sqlx::Row;
    let detail: Vec<String> = plan.iter().map(|r| r.get::<String, _>("detail")).collect();
    assert!(detail.iter().any(|d| d.contains("versions_node_seq")), "{detail:?}");
    assert!(!detail.iter().any(|d| d.contains("TEMP B-TREE")), "no sort: {detail:?}");
}

// ---- the trash ----

/// A folder `name` in `parent`: its node.
async fn folder(meta: &Meta, u: &UserRecord, dev: DeviceId, parent: NodeId, folder_name: &str, now: i64) -> NodeId {
    let node = NodeId::generate();
    meta.commit(u.space, dev, &[Op::Mkdir { node, parent, name: name(folder_name) }], now).await.unwrap();
    node
}

/// A file `name` in `parent` with one version: its node and version.
async fn file_in(meta: &Meta, u: &UserRecord, dev: DeviceId, parent: NodeId, file_name: &str, now: i64) -> (NodeId, NewVersion) {
    let node = NodeId::generate();
    let v = version(manifest(meta, u, 6).await);
    meta.commit(u.space, dev, &[Op::Create { node, parent, name: name(file_name), version: v.clone() }], now).await.unwrap();
    (node, v)
}

async fn delete_file(meta: &Meta, u: &UserRecord, dev: DeviceId, node: NodeId, current: VersionId, now: i64) {
    meta.commit(u.space, dev, &[Op::Delete { node, expected: Some(current) }], now).await.unwrap();
}

async fn delete_folder(meta: &Meta, u: &UserRecord, dev: DeviceId, node: NodeId, now: i64) {
    meta.commit(u.space, dev, &[Op::Delete { node, expected: None }], now).await.unwrap();
}

async fn trash_ids(meta: &Meta, u: &UserRecord) -> Vec<NodeId> {
    meta.trash(u.space, None, 200, NOW).await.unwrap().entries.iter().map(|e| e.id).collect()
}

/// The name a restore gives a node whose name is taken, in the minute of `NOW` (2025-10-09 08:53 UTC), from a device
/// called `laptop`.
fn conflict_of(original: &str, is_dir: bool, counter: u32) -> String {
    let stamp = craft_cloud_proto::ConflictStamp { year: 2025, month: 10, day: 9, hour: 8, minute: 53, utc: true };
    craft_cloud_proto::conflict_name(&name(original), is_dir, "laptop", stamp, counter).unwrap().as_str().to_owned()
}

async fn live_name(meta: &Meta, u: &UserRecord, node: NodeId) -> (Option<NodeId>, String, bool) {
    // The feed from the start, which has deleted nodes too.
    let feed = meta.changes(u.space, 0, None, MAX_PAGE_NODES).await.unwrap();
    let n = find(&feed.nodes, node);
    (n.parent, n.name.as_ref().map(|n| n.as_str().to_owned()).unwrap_or_default(), n.deleted)
}

/// The time in the trash starts when a live node is deleted, not when a deleted one is deleted again; coming back
/// clears it, by an edit or by a restore from the trash, and the next deletion starts afresh.
async fn deleting_stamps_the_time_once_and_coming_back_clears_it(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    let (file, v) = file_in(meta, &u, dev, r, "stamp.txt", NOW).await;
    assert!(trash_ids(meta, &u).await.is_empty());

    delete_file(meta, &u, dev, file, v.id, NOW + 1_000).await;
    let page = meta.trash(u.space, None, 10, NOW + 9_000).await.unwrap();
    assert_eq!((page.entries.len(), page.entries[0].deleted_ms, page.repaired), (1, NOW + 1_000, 0));
    // Deleting a deleted file again is no change: the time stays, and the file is not stamped twice.
    delete_file(meta, &u, dev, file, v.id, NOW + 5_000).await;
    assert_eq!(meta.trash(u.space, None, 10, NOW + 9_000).await.unwrap().entries[0].deleted_ms, NOW + 1_000);

    // An edit brings it back and leaves the trash; a later deletion starts the time again.
    let edited = version(manifest(meta, &u, 7).await);
    meta.commit(u.space, dev, &[Op::Update { node: file, expected: v.id, version: edited.clone() }], NOW + 6_000).await.unwrap();
    assert!(trash_ids(meta, &u).await.is_empty(), "an edit brings the file back");
    delete_file(meta, &u, dev, file, edited.id, NOW + 7_000).await;
    assert_eq!(meta.trash(u.space, None, 10, NOW + 9_000).await.unwrap().entries[0].deleted_ms, NOW + 7_000);

    // A restore from the trash clears it too, and the next deletion starts afresh again.
    let restored = meta.restore_from_trash(u.space, dev, file, NOW + 8_000).await.unwrap();
    assert!(trash_ids(meta, &u).await.is_empty());
    // The restore made a new current version: the next delete names it.
    delete_file(meta, &u, dev, file, restored.version.unwrap(), NOW + 8_500).await;
    assert_eq!(meta.trash(u.space, None, 10, NOW + 9_000).await.unwrap().entries[0].deleted_ms, NOW + 8_500);
}

/// The trash lists a space's deleted files and folders, newest deletion first by `(deleted_ms, id)`, in pages without
/// gaps or repeats (also for nodes deleted in one instant), with sizes of files; it lists nothing of another space and
/// nothing that was removed from the trash.
async fn the_trash_pages_newest_first_for_its_own_space(meta: &Meta) {
    use craft_cloud_proto::api::{TRASH_MAX_LIMIT, TrashCursor};
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    let mut made = Vec::new();
    for i in 0..5 {
        let (node, v) = file_in(meta, &u, dev, r, &format!("f{i}.txt"), NOW).await;
        // The last three go in one instant.
        delete_file(meta, &u, dev, node, v.id, NOW + 1_000 + i64::from(i.min(2)) * 10).await;
        made.push(node);
    }
    let dir = folder(meta, &u, dev, r, "dir", NOW).await;
    delete_folder(meta, &u, dev, dir, NOW + 5_000).await;

    let all = meta.trash(u.space, None, TRASH_MAX_LIMIT, NOW).await.unwrap();
    assert_eq!((all.entries.len(), all.next), (6, None));
    assert_eq!(all.entries[0].id, dir);
    assert!(all.entries[0].is_dir && all.entries[0].size.is_none() && all.entries[0].parent == r);
    assert!(all.entries[1..].iter().all(|e| !e.is_dir && e.size == Some(6)));
    let key = |e: &craft_cloud_meta::TrashEntry| (e.deleted_ms, *e.id.as_uuid().as_bytes());
    assert!(all.entries.windows(2).all(|w| key(&w[0]) > key(&w[1])), "strictly descending by (deleted_ms, id)");
    for limit in [1u32, 2, 4] {
        let (mut seen, mut after) = (Vec::new(), None);
        loop {
            let page = meta.trash(u.space, after, limit, NOW).await.unwrap();
            assert!(page.entries.len() <= limit as usize);
            seen.extend(page.entries.iter().cloned());
            match page.next {
                Some(next) => {
                    let last = page.entries.last().unwrap();
                    assert_eq!(next, TrashCursor { deleted_ms: last.deleted_ms, node: last.id });
                    after = Some(next);
                }
                None => break,
            }
        }
        assert_eq!(seen, all.entries, "limit {limit}");
    }
    assert!(matches!(meta.trash(u.space, None, 0, NOW).await, Err(MetaError::Invalid(_))));
    assert!(matches!(meta.trash(u.space, None, TRASH_MAX_LIMIT + 1, NOW).await, Err(MetaError::Invalid(_))));

    // Another space sees none of it. A node removed from the trash is not listed any more.
    let bob = user(meta, "bob").await;
    assert!(meta.trash(bob.space, None, 10, NOW).await.unwrap().entries.is_empty());
    meta.request_purge(u.space, dir, NOW + 6_000).await.unwrap();
    assert_eq!(trash_ids(meta, &u).await.len(), 5);
    assert!(!trash_ids(meta, &u).await.contains(&dir));
}

/// A file in deleted folders comes back with the folders, every node keeping its ID, as one change of the space.
async fn a_restore_brings_the_deleted_folders_back_with_their_ids(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    let a = folder(meta, &u, dev, r, "a", NOW).await;
    let b = folder(meta, &u, dev, a, "b", NOW).await;
    let (f, v) = file_in(meta, &u, dev, b, "f.txt", NOW).await;
    let (sibling, sv) = file_in(meta, &u, dev, b, "sibling.txt", NOW).await;
    delete_file(meta, &u, dev, f, v.id, NOW + 1).await;
    delete_file(meta, &u, dev, sibling, sv.id, NOW + 2).await;
    delete_folder(meta, &u, dev, b, NOW + 3).await;
    delete_folder(meta, &u, dev, a, NOW + 4).await;
    let before = meta.space_info(u.space).await.unwrap();

    let done = meta.restore_from_trash(u.space, dev, f, NOW).await.unwrap();
    assert_eq!((done.node, done.parent, done.name.as_str(), done.renamed), (f, b, "f.txt", false));
    assert_eq!((done.seq, done.epoch), (before.seq + 1, before.epoch));
    let after = meta.space_info(u.space).await.unwrap();
    assert_eq!((after.seq, after.epoch), (before.seq + 1, before.epoch), "one change of the space");
    assert_eq!(live_name(meta, &u, a).await, (Some(r), "a".to_owned(), false));
    assert_eq!(live_name(meta, &u, b).await, (Some(a), "b".to_owned(), false));
    assert_eq!(live_name(meta, &u, f).await, (Some(b), "f.txt".to_owned(), false));
    assert_eq!(trash_ids(meta, &u).await, vec![sibling], "only the node that was asked for and its folders come back");

    // The change feed reports exactly the nodes that came back, at the new sequence number, as live.
    let feed = meta.changes(u.space, before.seq, None, MAX_PAGE_NODES).await.unwrap();
    let mut changed: Vec<_> = feed.nodes.iter().map(|n| (n.id, n.deleted)).collect();
    changed.sort();
    let mut expected = vec![(a, false), (b, false), (f, false)];
    expected.sort();
    assert_eq!(changed, expected);
    let NodeKind::File(state) = &find(&feed.nodes, f).kind else { panic!("a file") };
    assert_eq!(state.manifest, v.manifest, "the file's content is what it was");
    assert_eq!(Some(state.version), done.version, "as a new version after the last one");
    assert_ne!(state.version, v.id);
}

/// A name taken by a file or by anything at the node itself gives a conflict name (never an overwrite); a deleted
/// folder whose name is held by a live folder is merged into it; every name tried is free, and a folder that is renamed
/// is a folder.
async fn a_restore_merges_into_live_folders_and_renames_around_taken_names(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;

    // The node's own name is taken: a conflict name, and the live file is untouched.
    let (f, fv) = file_in(meta, &u, dev, r, "plan.txt", NOW).await;
    delete_file(meta, &u, dev, f, fv.id, NOW + 1).await;
    let (squatter, _) = file_in(meta, &u, dev, r, "PLAN.txt", NOW + 2).await;
    let done = meta.restore_from_trash(u.space, dev, f, NOW).await.unwrap();
    assert_eq!((done.renamed, done.name.clone()), (true, conflict_of("plan.txt", false, 1)));
    assert_eq!(live_name(meta, &u, squatter).await, (Some(r), "PLAN.txt".to_owned(), false));
    assert_eq!(live_name(meta, &u, f).await, (Some(r), conflict_of("plan.txt", false, 1), false));

    // A second restore of the same name in the same minute takes the next counter.
    let (g, gv) = file_in(meta, &u, dev, r, "plan2.txt", NOW + 3).await;
    delete_file(meta, &u, dev, g, gv.id, NOW + 4).await;
    let (_h, _) = file_in(meta, &u, dev, r, "plan2.txt", NOW + 5).await;
    let (_i, _) = file_in(meta, &u, dev, r, &conflict_of("plan2.txt", false, 1), NOW + 6).await;
    let done = meta.restore_from_trash(u.space, dev, g, NOW).await.unwrap();
    assert_eq!(done.name, conflict_of("plan2.txt", false, 2));

    // A deleted folder held by a live folder of the same name: the live folder takes what is restored.
    let a = folder(meta, &u, dev, r, "a", NOW).await;
    let b = folder(meta, &u, dev, a, "b", NOW).await;
    let (x, xv) = file_in(meta, &u, dev, b, "x.txt", NOW).await;
    delete_file(meta, &u, dev, x, xv.id, NOW + 10).await;
    delete_folder(meta, &u, dev, b, NOW + 11).await;
    delete_folder(meta, &u, dev, a, NOW + 12).await;
    let live_a = folder(meta, &u, dev, r, "a", NOW + 13).await;
    let done = meta.restore_from_trash(u.space, dev, x, NOW).await.unwrap();
    assert_eq!((done.renamed, done.name.as_str()), (false, "x.txt"));
    assert_eq!(live_name(meta, &u, b).await, (Some(live_a), "b".to_owned(), false), "b moved into the live folder");
    assert_eq!(live_name(meta, &u, x).await, (Some(b), "x.txt".to_owned(), false));
    assert_eq!(live_name(meta, &u, a).await, (Some(r), "a".to_owned(), true), "the deleted a stays deleted");
    assert_eq!(trash_ids(meta, &u).await, vec![a]);

    // A deleted folder held by a file: the folder comes back under a conflict name, with its content below it.
    let c = folder(meta, &u, dev, r, "c", NOW).await;
    let (y, yv) = file_in(meta, &u, dev, c, "y.txt", NOW).await;
    delete_file(meta, &u, dev, y, yv.id, NOW + 20).await;
    delete_folder(meta, &u, dev, c, NOW + 21).await;
    let (_c_file, _) = file_in(meta, &u, dev, r, "c", NOW + 22).await;
    let done = meta.restore_from_trash(u.space, dev, y, NOW).await.unwrap();
    assert_eq!((done.parent, done.name.as_str()), (c, "y.txt"));
    assert_eq!(live_name(meta, &u, c).await, (Some(r), conflict_of("c", true, 1), false));

    // A deleted folder restored itself, its name held by a live folder: no merge for the node asked for.
    let d = folder(meta, &u, dev, r, "d", NOW).await;
    delete_folder(meta, &u, dev, d, NOW + 30).await;
    let live_d = folder(meta, &u, dev, r, "d", NOW + 31).await;
    let done = meta.restore_from_trash(u.space, dev, d, NOW).await.unwrap();
    assert_eq!((done.renamed, done.name.clone()), (true, conflict_of("d", true, 1)));
    assert_eq!(live_name(meta, &u, live_d).await.1, "d");
}

/// A folder in the trash is listed alone: what is in the trash below it is listed only through it, however long before
/// it was deleted, and pages never split or repeat the top items.
async fn the_trash_lists_a_folder_without_what_is_below_it(meta: &Meta) {
    use craft_cloud_proto::api::TrashCursor;
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    let mut tops = Vec::new();
    for i in 0..7i64 {
        let dir = folder(meta, &u, dev, r, &format!("d{i}"), NOW).await;
        let (child, cv) = file_in(meta, &u, dev, dir, "child.txt", NOW).await;
        let sub = folder(meta, &u, dev, dir, "sub", NOW).await;
        let (deep, dv) = file_in(meta, &u, dev, sub, "deep.txt", NOW).await;
        // The child goes long before its folder, the deep file with the folders.
        delete_file(meta, &u, dev, child, cv.id, NOW + 100 * i).await;
        delete_file(meta, &u, dev, deep, dv.id, NOW + 1_000 + 100 * i).await;
        delete_folder(meta, &u, dev, sub, NOW + 1_000 + 100 * i).await;
        delete_folder(meta, &u, dev, dir, NOW + 1_000 + 100 * i).await;
        tops.push(dir);
    }
    for i in 0..3 {
        let (f, v) = file_in(meta, &u, dev, r, &format!("loose{i}.txt"), NOW).await;
        delete_file(meta, &u, dev, f, v.id, NOW + 500 + i64::from(i)).await;
        tops.push(f);
    }
    tops.sort();
    for limit in [1u32, 2, 3, 200] {
        let (mut seen, mut after) = (Vec::new(), None::<TrashCursor>);
        loop {
            let page = meta.trash(u.space, after, limit, NOW).await.unwrap();
            assert!(page.entries.len() <= limit as usize && !page.entries.is_empty(), "limit {limit}");
            seen.extend(page.entries.iter().map(|e| e.id));
            match page.next {
                Some(next) => {
                    // A page is full whenever another follows: the hidden nodes take no room in it.
                    assert_eq!(page.entries.len(), limit as usize, "limit {limit}");
                    after = Some(next);
                }
                None => break,
            }
        }
        seen.sort();
        assert_eq!(seen, tops, "every top item exactly once, limit {limit}");
    }
}

/// Restoring a folder brings back all that is in the trash below it, whenever it was deleted, as one change of the space
/// with each node revived like a single restore; a node removed from the trash, and what hangs from it, stays deleted.
async fn a_folder_restore_brings_back_everything_in_the_trash_below_it(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    let dir = folder(meta, &u, dev, r, "dir1", NOW).await;
    let (child, cv) = file_in(meta, &u, dev, dir, "child.txt", NOW).await;
    let (early, ev) = file_in(meta, &u, dev, dir, "early.txt", NOW).await;
    let sub = folder(meta, &u, dev, dir, "sub", NOW).await;
    let sub2 = folder(meta, &u, dev, sub, "sub2", NOW).await;
    let (deep, dv) = file_in(meta, &u, dev, sub2, "deep.txt", NOW).await;
    let gone = folder(meta, &u, dev, dir, "gone", NOW).await;
    let (hung, hv) = file_in(meta, &u, dev, gone, "hung.txt", NOW).await;
    let (kept, kv) = file_in(meta, &u, dev, dir, "kept.txt", NOW).await;
    // `early` was deleted on its own long before; `gone` and `kept` are removed from the trash; the rest goes with
    // the folder.
    delete_file(meta, &u, dev, early, ev.id, NOW + 10).await;
    delete_file(meta, &u, dev, hung, hv.id, NOW + 1_000).await;
    delete_folder(meta, &u, dev, gone, NOW + 1_001).await;
    delete_file(meta, &u, dev, kept, kv.id, NOW + 1_002).await;
    delete_file(meta, &u, dev, deep, dv.id, NOW + 1_003).await;
    delete_folder(meta, &u, dev, sub2, NOW + 1_004).await;
    delete_folder(meta, &u, dev, sub, NOW + 1_005).await;
    delete_file(meta, &u, dev, child, cv.id, NOW + 1_006).await;
    delete_folder(meta, &u, dev, dir, NOW + 1_007).await;
    meta.request_purge(u.space, kept, NOW + 2_000).await.unwrap();
    meta.request_purge(u.space, gone, NOW + 2_001).await.unwrap();
    assert_eq!(trash_ids(meta, &u).await, vec![dir], "the folder alone");
    let before = meta.space_info(u.space).await.unwrap();

    let done = meta.restore_from_trash(u.space, dev, dir, NOW + 3_000).await.unwrap();
    assert_eq!((done.node, done.parent, done.name.as_str(), done.renamed, done.version), (dir, r, "dir1", false, None));
    assert_eq!(done.seq, before.seq + 1);
    assert_eq!(meta.space_info(u.space).await.unwrap().seq, before.seq + 1, "one change of the space");
    assert!(trash_ids(meta, &u).await.is_empty(), "nothing is left in the trash: the removed nodes are no longer listed");
    for (node, parent, node_name) in
        [(dir, r, "dir1"), (child, dir, "child.txt"), (early, dir, "early.txt"), (sub, dir, "sub"), (sub2, sub, "sub2"), (deep, sub2, "deep.txt")]
    {
        assert_eq!(live_name(meta, &u, node).await, (Some(parent), node_name.to_owned(), false), "{node_name}");
    }
    for node in [gone, hung, kept] {
        assert!(live_name(meta, &u, node).await.2, "a removed node stays deleted");
    }

    // The feed reports exactly the nodes that came back, each at the new sequence number; the files as new versions.
    let feed = meta.changes(u.space, before.seq, None, MAX_PAGE_NODES).await.unwrap();
    let mut changed: Vec<_> = feed.nodes.iter().map(|n| (n.id, n.deleted)).collect();
    changed.sort();
    let mut expected = vec![(dir, false), (child, false), (early, false), (sub, false), (sub2, false), (deep, false)];
    expected.sort();
    assert_eq!(changed, expected);
    for (node, old) in [(child, &cv), (early, &ev), (deep, &dv)] {
        let NodeKind::File(state) = &find(&feed.nodes, node).kind else { panic!("a file") };
        assert_eq!(state.manifest, old.manifest, "the content is what it was");
        assert_ne!(state.version, old.id, "as a new version");
    }
}

/// Two deleted children of one name come back together: the newest deletion keeps the name, the other gets a conflict
/// name, and the folder itself is renamed only where its own name is taken.
async fn a_folder_restore_gives_children_of_one_name_a_conflict_name(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    let dir = folder(meta, &u, dev, r, "dir", NOW).await;
    let (old, ov) = file_in(meta, &u, dev, dir, "a.txt", NOW).await;
    delete_file(meta, &u, dev, old, ov.id, NOW + 1).await;
    let (new, nv) = file_in(meta, &u, dev, dir, "a.txt", NOW + 2).await;
    delete_file(meta, &u, dev, new, nv.id, NOW + 3).await;
    delete_folder(meta, &u, dev, dir, NOW + 4).await;
    let (_squatter, _) = file_in(meta, &u, dev, r, "dir", NOW + 5).await;

    let done = meta.restore_from_trash(u.space, dev, dir, NOW).await.unwrap();
    assert_eq!((done.renamed, done.name.clone()), (true, conflict_of("dir", true, 1)));
    assert_eq!(live_name(meta, &u, new).await, (Some(dir), "a.txt".to_owned(), false), "the newest deletion keeps the name");
    assert_eq!(live_name(meta, &u, old).await, (Some(dir), conflict_of("a.txt", false, 1), false));
}

/// Removing a folder from the trash removes what is in the trash below it too, so nothing stays listed or restorable
/// alone (up to the cap, see below).
async fn removing_a_folder_from_the_trash_takes_what_is_below_it(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    let dir = folder(meta, &u, dev, r, "dir", NOW).await;
    let (c, cv) = file_in(meta, &u, dev, dir, "c.txt", NOW).await;
    let s = folder(meta, &u, dev, dir, "s", NOW).await;
    let (d, dv) = file_in(meta, &u, dev, s, "d.txt", NOW).await;
    delete_file(meta, &u, dev, c, cv.id, NOW + 1).await;
    delete_file(meta, &u, dev, d, dv.id, NOW + 2).await;
    delete_folder(meta, &u, dev, s, NOW + 3).await;
    delete_folder(meta, &u, dev, dir, NOW + 4).await;
    let before = meta.space_info(u.space).await.unwrap();
    meta.request_purge(u.space, dir, NOW + 5).await.unwrap();
    assert!(trash_ids(meta, &u).await.is_empty());
    assert_eq!(meta.space_info(u.space).await.unwrap().seq, before.seq, "no change of the space");
    for node in [dir, c, s, d] {
        assert!(matches!(meta.restore_from_trash(u.space, dev, node, NOW).await, Err(MetaError::NotFound(_))), "{node}");
        assert!(live_name(meta, &u, node).await.2);
    }
}

/// A folder `name` in the root with `count` files, the file `i` deleted at `base + 1 + i` and the folder at `base + 20`:
/// the folder and its files, the newest deletion last.
async fn folder_with_deleted_files(meta: &Meta, u: &UserRecord, dev: DeviceId, folder_name: &str, count: i64, base: i64) -> (NodeId, Vec<NodeId>) {
    let r = root(meta, u).await;
    let dir = folder(meta, u, dev, r, folder_name, base).await;
    let mut files = Vec::new();
    for i in 0..count {
        let (f, v) = file_in(meta, u, dev, dir, &format!("f{i}.txt"), base).await;
        delete_file(meta, u, dev, f, v.id, base + 1 + i).await;
        files.push(f);
    }
    delete_folder(meta, u, dev, dir, base + 20).await;
    (dir, files)
}

fn sorted(mut ids: Vec<NodeId>) -> Vec<NodeId> {
    ids.sort();
    ids
}

/// A folder brings back at most the cap of what is in the trash below it, the newest deletions first among equals in
/// depth; the rest stays in the trash, is listed on its own (its folder is live) and comes back by a further restore. A
/// folder with exactly the cap below it brings everything.
async fn a_folder_restore_takes_at_most_the_cap_below_it(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let small = meta.clone().with_trash_tree_cap(3);
    let (dir, files) = folder_with_deleted_files(meta, &u, dev, "flat", 5, NOW).await;
    let (exact, exact_files) = folder_with_deleted_files(meta, &u, dev, "exact", 3, NOW + 100).await;
    assert_eq!(sorted(trash_ids(meta, &u).await), sorted(vec![dir, exact]));

    let before = meta.space_info(u.space).await.unwrap();
    let done = small.restore_from_trash(u.space, dev, dir, NOW).await.unwrap();
    assert_eq!((done.node, done.seq), (dir, before.seq + 1), "one change of the space");
    assert!(!live_name(meta, &u, dir).await.2);
    for f in &files[2..] {
        assert!(!live_name(meta, &u, *f).await.2, "the three newest deletions came back");
    }
    for f in &files[..2] {
        assert!(live_name(meta, &u, *f).await.2, "the oldest two stay in the trash");
    }
    assert_eq!(sorted(trash_ids(meta, &u).await), sorted(vec![exact, files[0], files[1]]), "the rest is listed on its own");
    small.restore_from_trash(u.space, dev, files[1], NOW).await.unwrap();
    assert_eq!(sorted(trash_ids(meta, &u).await), sorted(vec![exact, files[0]]));

    small.restore_from_trash(u.space, dev, exact, NOW).await.unwrap();
    for f in &exact_files {
        assert!(!live_name(meta, &u, *f).await.2, "exactly the cap brings everything");
    }
    assert_eq!(trash_ids(meta, &u).await, vec![files[0]]);
}

/// Of the nodes below a folder the shallow ones come first, and what is left is listed only if its own folder is not in
/// the trash: a left-over folder is listed, what is in it is hidden behind it until it is restored.
async fn a_folder_restore_leaves_the_rest_listed_through_its_own_folder(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    let small = meta.clone().with_trash_tree_cap(1);
    let dir = folder(meta, &u, dev, r, "dir", NOW).await;
    let a = folder(meta, &u, dev, dir, "a", NOW).await;
    let (a1, a1v) = file_in(meta, &u, dev, a, "a1.txt", NOW).await;
    let (b, bv) = file_in(meta, &u, dev, dir, "b.txt", NOW).await;
    delete_file(meta, &u, dev, a1, a1v.id, NOW + 1).await;
    delete_folder(meta, &u, dev, a, NOW + 2).await;
    delete_file(meta, &u, dev, b, bv.id, NOW + 3).await;
    delete_folder(meta, &u, dev, dir, NOW + 4).await;
    assert_eq!(trash_ids(meta, &u).await, vec![dir]);

    small.restore_from_trash(u.space, dev, dir, NOW).await.unwrap();
    assert!(!live_name(meta, &u, dir).await.2 && !live_name(meta, &u, b).await.2, "the folder and its newest child");
    assert!(live_name(meta, &u, a).await.2 && live_name(meta, &u, a1).await.2);
    assert_eq!(trash_ids(meta, &u).await, vec![a], "the folder is listed, what is in it is not");
    small.restore_from_trash(u.space, dev, a, NOW).await.unwrap();
    assert!(!live_name(meta, &u, a).await.2 && !live_name(meta, &u, a1).await.2);
    assert!(trash_ids(meta, &u).await.is_empty());
}

/// Removing a folder marks it and at most the cap below it, shallowest first; the rest is listed on its own (its folder
/// is no longer in the trash) and goes by a further removal.
async fn removing_a_folder_takes_at_most_the_cap_below_it(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    let small = meta.clone().with_trash_tree_cap(3);
    let (dir, files) = folder_with_deleted_files(meta, &u, dev, "flat", 5, NOW).await;
    let before = meta.space_info(u.space).await.unwrap();
    small.request_purge(u.space, dir, NOW + 50).await.unwrap();
    assert_eq!(meta.space_info(u.space).await.unwrap().seq, before.seq, "no change of the space");
    assert_eq!(sorted(trash_ids(meta, &u).await), sorted(vec![files[0], files[1]]));
    for f in &files[2..] {
        assert!(matches!(meta.restore_from_trash(u.space, dev, *f, NOW).await, Err(MetaError::NotFound(_))), "removed with the folder");
    }
    small.request_purge(u.space, files[0], NOW + 51).await.unwrap();
    small.request_purge(u.space, files[1], NOW + 51).await.unwrap();
    assert!(trash_ids(meta, &u).await.is_empty());

    // A left-over folder is listed and what is in it stays hidden behind it.
    let tree = folder(meta, &u, dev, r, "tree", NOW + 100).await;
    let a = folder(meta, &u, dev, tree, "a", NOW + 100).await;
    let (a1, a1v) = file_in(meta, &u, dev, a, "a1.txt", NOW + 100).await;
    let (b, bv) = file_in(meta, &u, dev, tree, "b.txt", NOW + 100).await;
    delete_file(meta, &u, dev, a1, a1v.id, NOW + 101).await;
    delete_folder(meta, &u, dev, a, NOW + 102).await;
    delete_file(meta, &u, dev, b, bv.id, NOW + 103).await;
    delete_folder(meta, &u, dev, tree, NOW + 104).await;
    meta.clone().with_trash_tree_cap(1).request_purge(u.space, tree, NOW + 105).await.unwrap();
    assert_eq!(trash_ids(meta, &u).await, vec![a], "only the left-over folder is listed");
    meta.clone().with_trash_tree_cap(1).request_purge(u.space, a, NOW + 106).await.unwrap();
    assert!(trash_ids(meta, &u).await.is_empty());
    assert!(matches!(meta.restore_from_trash(u.space, dev, a1, NOW).await, Err(MetaError::NotFound(_))));
}

/// When every conflict name is taken too, the restore is refused as a conflict and the space is exactly as it was: no
/// sequence number, no revived node, the node still in the trash.
async fn a_restore_with_no_free_name_changes_nothing(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    let (f, fv) = file_in(meta, &u, dev, r, "busy.txt", NOW).await;
    delete_file(meta, &u, dev, f, fv.id, NOW + 1).await;
    // The name itself and its 100 conflict names, in one batch.
    let m = manifest(meta, &u, 1).await;
    let mut names = vec!["busy.txt".to_owned()];
    names.extend((1..=100).map(|n| conflict_of("busy.txt", false, n)));
    let ops: Vec<Op> = names.iter().map(|n| Op::Create { node: NodeId::generate(), parent: r, name: name(n), version: version(m) }).collect();
    meta.commit(u.space, dev, &ops, NOW + 2).await.unwrap();
    let before = meta.space_info(u.space).await.unwrap();
    let tree_before = full_tree(meta, u.space).await.nodes.len();

    let refused = meta.restore_from_trash(u.space, dev, f, NOW).await;
    let list = refused_restore(refused);
    assert!(matches!(list[0].1, ConflictKind::NameTaken { .. }), "{list:?}");
    assert_eq!(meta.space_info(u.space).await.unwrap().seq, before.seq);
    assert_eq!(trash_ids(meta, &u).await, vec![f]);
    assert_eq!(full_tree(meta, u.space).await.nodes.len(), tree_before);
    assert!(meta.changes(u.space, before.seq, None, MAX_PAGE_NODES).await.unwrap().nodes.is_empty());
}

/// What is not in the trash can't be restored or removed from it: a live node, an unknown one, another space's, one
/// already removed from the trash.
async fn only_nodes_in_the_trash_can_be_restored_or_removed(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    let (live, _) = file_in(meta, &u, dev, r, "live.txt", NOW).await;
    let (gone, gv) = file_in(meta, &u, dev, r, "gone.txt", NOW).await;
    let (kept, kv) = file_in(meta, &u, dev, r, "kept.txt", NOW).await;
    delete_file(meta, &u, dev, gone, gv.id, NOW + 1).await;
    delete_file(meta, &u, dev, kept, kv.id, NOW + 1).await;
    meta.request_purge(u.space, gone, NOW + 2).await.unwrap();
    let seq = meta.space_info(u.space).await.unwrap().seq;
    let bob = user(meta, "bob").await;
    let bob_device = device(meta, &bob).await;

    for node in [live, NodeId::generate(), r, gone] {
        assert!(matches!(meta.restore_from_trash(u.space, dev, node, NOW).await, Err(MetaError::NotFound(_))), "restore {node}");
    }
    for node in [live, NodeId::generate(), r] {
        assert!(matches!(meta.request_purge(u.space, node, NOW).await, Err(MetaError::NotFound(_))), "remove {node}");
    }
    assert!(matches!(meta.restore_from_trash(bob.space, bob_device, kept, NOW).await, Err(MetaError::NotFound(_))), "another space's node");
    assert!(matches!(meta.request_purge(bob.space, kept, NOW).await, Err(MetaError::NotFound(_))));
    assert_eq!(meta.space_info(u.space).await.unwrap().seq, seq, "nothing changed");
    assert_eq!(trash_ids(meta, &u).await, vec![kept]);
}

/// Removing from the trash only marks the node: no version, manifest or tombstone changes, the change feed and the
/// sequence number stay, the history is still readable; asking twice changes nothing; an edit that brings the node back
/// ends the mark, and the next deletion lists it again.
async fn removing_from_the_trash_frees_nothing(meta: &Meta) {
    use craft_cloud_proto::api::HISTORY_MAX_LIMIT;
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let (node, versions) = file_with_versions(meta, &u, dev, 3).await;
    delete_file(meta, &u, dev, node, versions[2].id, NOW + 100).await;
    let before = meta.space_info(u.space).await.unwrap();
    let feed_before = meta.changes(u.space, 0, None, MAX_PAGE_NODES).await.unwrap();

    meta.request_purge(u.space, node, NOW + 200).await.unwrap();
    meta.request_purge(u.space, node, NOW + 300).await.unwrap();
    assert!(trash_ids(meta, &u).await.is_empty(), "it left the trash");
    assert_eq!(meta.space_info(u.space).await.unwrap().seq, before.seq, "no change of the space");
    assert_eq!(meta.changes(u.space, 0, None, MAX_PAGE_NODES).await.unwrap(), feed_before, "the tombstone is as it was");
    let history = meta.history(u.space, node, None, HISTORY_MAX_LIMIT).await.unwrap().versions;
    assert_eq!(history.len(), 3, "every version is still there");
    for v in &versions {
        assert!(meta.has_manifest(u.space, &v.manifest).await.unwrap(), "every manifest is still the space's");
    }

    // An edit still brings the file back (an edit beats a deletion), which ends the mark.
    let edit = version(manifest(meta, &u, 9).await);
    meta.commit(u.space, dev, &[Op::Update { node, expected: versions[2].id, version: edit.clone() }], NOW + 400).await.unwrap();
    assert!(!live_name(meta, &u, node).await.2);
    delete_file(meta, &u, dev, node, edit.id, NOW + 500).await;
    let page = meta.trash(u.space, None, 10, NOW + 600).await.unwrap();
    assert_eq!((page.entries.len(), page.entries[0].deleted_ms), (1, NOW + 500));
}

/// A restore and a commit that wants the same name run together: both are applied in some order and the result is
/// consistent, never two live nodes with one name.
async fn a_trash_restore_and_a_commit_for_the_same_name_agree(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    for round in 0..4 {
        let file_name = format!("race{round}.txt");
        let (f, fv) = file_in(meta, &u, dev, r, &file_name, NOW).await;
        delete_file(meta, &u, dev, f, fv.id, NOW + 1).await;
        let (m1, m2, space) = (meta.clone(), meta.clone(), u.space);
        let rival = NodeId::generate();
        let rival_op = Op::Create { node: rival, parent: r, name: name(&file_name), version: version(manifest(meta, &u, 2).await) };
        let restore = tokio::spawn(async move { m1.restore_from_trash(space, dev, f, NOW).await });
        let create = tokio::spawn(async move { m2.commit(space, dev, &[rival_op], NOW + 2).await });
        let restored = restore.await.unwrap().unwrap();
        match create.await.unwrap() {
            // The commit came first: it holds the name, and the restore went around it.
            Ok(_) => {
                assert!(restored.renamed, "round {round}");
                assert_eq!(live_name(meta, &u, rival).await.1, file_name);
            }
            // The restore came first: the commit finds the name taken.
            Err(MetaError::Rejected(list)) => {
                assert!(matches!(list[0].kind, ConflictKind::NameTaken { .. }), "round {round}");
                assert!(!restored.renamed);
                assert_eq!(live_name(meta, &u, f).await.1, file_name);
            }
            Err(other) => panic!("unexpected error {other:?}"),
        }
    }
    // No two live nodes share a name (the unique index would have refused it; this says what is seen).
    let tree = full_tree(meta, u.space).await;
    let mut names: Vec<String> = tree.nodes.iter().filter_map(|n| n.name.as_ref().map(|n| n.fold_key().as_str().to_owned())).collect();
    let total = names.len();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), total);
}

/// A restore and an edit of the same deleted file run together: whichever is first, the file ends live with its versions
/// intact. If the edit is first it brings the file back and the restore finds nothing in the trash; if the restore is
/// first it makes a new current version, so the edit, made on the old one, is refused as a stale update.
async fn a_trash_restore_and_an_edit_of_the_file_agree(meta: &Meta) {
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    let (f, fv) = file_in(meta, &u, dev, r, "both.txt", NOW).await;
    delete_file(meta, &u, dev, f, fv.id, NOW + 1).await;
    let edit = version(manifest(meta, &u, 4).await);
    let (m1, m2, space) = (meta.clone(), meta.clone(), u.space);
    let edit_op = Op::Update { node: f, expected: fv.id, version: edit.clone() };
    let restore = tokio::spawn(async move { m1.restore_from_trash(space, dev, f, NOW).await });
    let update = tokio::spawn(async move { m2.commit(space, dev, &[edit_op], NOW + 2).await });
    let restored = restore.await.unwrap();
    let updated = update.await.unwrap();
    match (&restored, &updated) {
        // The edit came first and brought the file back: the restore finds nothing in the trash.
        (Err(MetaError::NotFound(_)), Ok(_)) => {}
        // The restore came first and made a new current version: the edit, made on the old one, is a stale update.
        (Ok(_), Err(MetaError::Rejected(list))) => assert!(matches!(list[0].kind, ConflictKind::VersionMismatch { .. })),
        other => panic!("one of them must win: {other:?}"),
    }
    assert_eq!(live_name(meta, &u, f).await, (Some(r), "both.txt".to_owned(), false));
    assert!(trash_ids(meta, &u).await.is_empty());
    assert_eq!(meta.history(u.space, f, None, 10).await.unwrap().versions.len(), 2);
}

// ---- the trash: the migration and a damaged database ----

/// A raw connection to the database `url`, for what the library never does (reading and breaking columns).
async fn raw(url: &str) -> sqlx::AnyConnection {
    use sqlx::Connection;
    sqlx::any::install_default_drivers();
    sqlx::AnyConnection::connect(url).await.unwrap()
}

/// Puts the database back to the state before the trash migration: no columns, no index, one version fewer.
async fn undo_the_trash_migration(url: &str) {
    let mut conn = raw(url).await;
    for statement in [
        "DROP TABLE manifest_chunks",
        "DROP TABLE manifest_index",
        "DROP INDEX nodes_trash",
        "ALTER TABLE nodes DROP COLUMN deleted_ms",
        "ALTER TABLE nodes DROP COLUMN purge_requested_ms",
        "DELETE FROM schema_version WHERE version >= 8",
    ] {
        sqlx::query(statement).execute(&mut conn).await.unwrap();
    }
}

fn wall_clock_ms() -> i64 {
    i64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()).unwrap()
}

/// Nodes deleted before the trash existed start their time in the trash at the migration, once: live nodes get no
/// time, and opening the database again does not stamp them again.
async fn the_trash_migration_dates_old_deletions_at_the_migration(url: &str) {
    let (ana, file, folder_node) = {
        let meta = Meta::open(url).await.unwrap();
        let ana = user(&meta, "ana").await;
        let dev = device(&meta, &ana).await;
        let r = root(&meta, &ana).await;
        let (file, v) = file_in(&meta, &ana, dev, r, "old.txt", NOW).await;
        let (_kept, _) = file_in(&meta, &ana, dev, r, "kept.txt", NOW).await;
        let dir = folder(&meta, &ana, dev, r, "olddir", NOW).await;
        delete_file(&meta, &ana, dev, file, v.id, NOW + 1).await;
        delete_folder(&meta, &ana, dev, dir, NOW + 2).await;
        (ana, file, dir)
    };
    undo_the_trash_migration(url).await;
    let before = wall_clock_ms();
    let meta = Meta::open(url).await.unwrap();
    let after = wall_clock_ms();
    let page = meta.trash(ana.space, None, 10, 0).await.unwrap();
    assert_eq!(page.repaired, 0, "the migration stamped them, nothing was left to repair");
    let mut ids: Vec<_> = page.entries.iter().map(|e| e.id).collect();
    ids.sort();
    let mut expected = vec![file, folder_node];
    expected.sort();
    assert_eq!(ids, expected, "the live file is not in the trash");
    let stamps: Vec<i64> = page.entries.iter().map(|e| e.deleted_ms).collect();
    assert!(stamps.iter().all(|t| (before - 1_000..=after + 1_000).contains(t)), "{stamps:?} not in {before}..{after}");
    assert_eq!(stamps[0], stamps[1], "one baseline for all of them");
    // Another open changes nothing.
    drop(meta);
    let again = Meta::open(url).await.unwrap().trash(ana.space, None, 10, 0).await.unwrap();
    assert_eq!(again.entries, page.entries);
    let mut conn = raw(url).await;
    let latest: i64 = sqlx::query_scalar("SELECT MAX(version) FROM schema_version").fetch_one(&mut conn).await.unwrap();
    assert_eq!(latest, 9);
    let live_with_time: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM nodes WHERE deleted = 0 AND (deleted_ms IS NOT NULL OR purge_requested_ms IS NOT NULL)")
            .fetch_one(&mut conn)
            .await
            .unwrap();
    assert_eq!(live_with_time, 0);
}

#[tokio::test]
async fn the_trash_migration_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    the_trash_migration_dates_old_deletions_at_the_migration(&format!("sqlite:{}", dir.path().join("meta.db").display())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_trash_migration_on_postgres() {
    let Some((url, _guard)) = postgres_database().await else {
        eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set");
        return;
    };
    the_trash_migration_dates_old_deletions_at_the_migration(&url).await;
}

/// A deleted node without a deletion time (it can't happen through this code) is stamped with the time of the read
/// and reported, never listed as expired; the stamp stays.
async fn a_deleted_node_without_a_time_is_stamped_when_read(url: &str) {
    let meta = Meta::open(url).await.unwrap();
    let ana = user(&meta, "ana").await;
    let dev = device(&meta, &ana).await;
    let r = root(&meta, &ana).await;
    let (file, v) = file_in(&meta, &ana, dev, r, "damaged.txt", NOW).await;
    delete_file(&meta, &ana, dev, file, v.id, NOW + 1).await;
    let mut conn = raw(url).await;
    sqlx::query("UPDATE nodes SET deleted_ms = NULL WHERE deleted = 1").execute(&mut conn).await.unwrap();
    let page = meta.trash(ana.space, None, 10, NOW + 77).await.unwrap();
    assert_eq!((page.repaired, page.entries.len(), page.entries[0].deleted_ms), (1, 1, NOW + 77));
    let later = meta.trash(ana.space, None, 10, NOW + 99).await.unwrap();
    assert_eq!((later.repaired, later.entries[0].deleted_ms), (0, NOW + 77));
}

#[tokio::test]
async fn a_damaged_deletion_time_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    a_deleted_node_without_a_time_is_stamped_when_read(&format!("sqlite:{}", dir.path().join("meta.db").display())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_damaged_deletion_time_on_postgres() {
    let Some((url, _guard)) = postgres_database().await else {
        eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set");
        return;
    };
    a_deleted_node_without_a_time_is_stamped_when_read(&url).await;
}

/// A file that comes back from the trash gets a new current version with its last content, after the last one and
/// without a save record, so a delete that was sent against the old version (and whose answer a machine never heard)
/// is refused as a version mismatch and can't delete the restored file again; a folder has no version.
async fn a_restored_file_gets_a_new_version_with_its_old_content(meta: &Meta) {
    use craft_cloud_proto::api::HISTORY_MAX_LIMIT;
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    let (node, versions) = file_with_versions(meta, &u, dev, 2).await;
    let record = craft_cloud_proto::SaveRecord { doc: craft_cloud_proto::live::node_doc(u.space, node), frontiers: vec![(1, 5)] };
    meta.attach_save(u.space, node, versions[1].id, &record).await.unwrap();
    delete_file(meta, &u, dev, node, versions[1].id, NOW + 10).await;
    let before = meta.space_info(u.space).await.unwrap();

    let done = meta.restore_from_trash(u.space, dev, node, NOW + 20).await.unwrap();
    let new = done.version.unwrap();
    assert!(!versions.iter().any(|v| v.id == new));
    assert_eq!(done.seq, before.seq + 1, "one change of the space");
    let history = meta.history(u.space, node, None, HISTORY_MAX_LIMIT).await.unwrap().versions;
    assert_eq!(history.len(), 3);
    assert_eq!(
        (history[0].id, history[0].parent_version, history[0].size, history[0].save, history[0].seq),
        (new, Some(versions[1].id), 4, false, done.seq)
    );
    assert!(history.iter().any(|v| v.id == versions[1].id && v.save), "the old version keeps its record");
    let feed = meta.changes(u.space, before.seq, None, MAX_PAGE_NODES).await.unwrap();
    let NodeKind::File(f) = &find(&feed.nodes, node).kind else { panic!("a file") };
    assert_eq!((f.version, f.manifest, f.mtime_ms, f.save.clone()), (new, versions[1].manifest, versions[1].mtime_ms, None));

    // The delete a machine sent before the restore, with the old version, is refused and the file stays.
    let stale = meta.commit(u.space, dev, &[Op::Delete { node, expected: Some(versions[1].id) }], NOW + 30).await;
    assert_eq!(refused_restore(stale), vec![(0, ConflictKind::VersionMismatch { current: Some(new) })]);
    assert!(!live_name(meta, &u, node).await.2);

    // A folder has no version.
    let dir = folder(meta, &u, dev, r, "plain-dir", NOW + 40).await;
    delete_folder(meta, &u, dev, dir, NOW + 41).await;
    assert_eq!(meta.restore_from_trash(u.space, dev, dir, NOW + 42).await.unwrap().version, None);
}

/// A restore never takes the tree past its depth limit: a folder that comes back below a live folder that has moved
/// deep meanwhile is refused as too deep, for the folder itself and for a file whose folder would come back, and
/// the refusal changes nothing; a file in a folder at the limit is fine.
async fn a_restore_does_not_exceed_the_depth_limit(meta: &Meta) {
    use craft_cloud_proto::limits::MAX_TREE_DEPTH;
    let u = user(meta, "ana").await;
    let dev = device(meta, &u).await;
    let r = root(meta, &u).await;
    let shallow = folder(meta, &u, dev, r, "shallow", NOW).await;
    let c = folder(meta, &u, dev, shallow, "c", NOW).await;
    let (f, fv) = file_in(meta, &u, dev, c, "f.txt", NOW).await;
    let spare = folder(meta, &u, dev, shallow, "spare", NOW).await;
    let (g, gv) = file_in(meta, &u, dev, spare, "g.txt", NOW).await;
    delete_file(meta, &u, dev, f, fv.id, NOW + 1).await;
    delete_folder(meta, &u, dev, c, NOW + 2).await;
    delete_file(meta, &u, dev, g, gv.id, NOW + 3).await;
    // A chain of folders down to depth MAX_TREE_DEPTH - 1, and the live folder moved to its end: depth MAX_TREE_DEPTH.
    let mut ops = Vec::new();
    let mut parent = r;
    for i in 0..MAX_TREE_DEPTH - 1 {
        let id = NodeId::generate();
        ops.push(Op::Mkdir { node: id, parent, name: name(&format!("d{i}")) });
        parent = id;
    }
    meta.commit(u.space, dev, &ops, NOW + 4).await.unwrap();
    meta.commit(u.space, dev, &[Op::Move { node: shallow, from_parent: r, from_name: name("shallow"), parent, name: name("shallow") }], NOW + 5)
        .await
        .unwrap();
    let before = meta.space_info(u.space).await.unwrap();
    let trash_before = trash_ids(meta, &u).await;

    // The deleted folder would sit one level too deep, and so would the folder of the file: refused, nothing changes.
    for node in [c, f] {
        let refused = meta.restore_from_trash(u.space, dev, node, NOW + 6).await;
        assert_eq!(refused_restore(refused), vec![(0, ConflictKind::TooDeep)], "restoring {node}");
        assert_eq!(meta.space_info(u.space).await.unwrap().seq, before.seq);
        assert_eq!(trash_ids(meta, &u).await, trash_before);
    }
    // A file may sit in the deepest folder: it comes back, in `spare`, which is live (it was never deleted).
    let done = meta.restore_from_trash(u.space, dev, g, NOW + 7).await.unwrap();
    assert_eq!((done.parent, done.name.as_str()), (spare, "g.txt"));
}

// ---- the reference index ----

/// A manifest over `chunks` (each one byte), the chunks recorded as `u`'s; the manifest itself is not recorded.
async fn manifest_over(meta: &Meta, u: &UserRecord, chunks: &[ChunkId]) -> Manifest {
    let unique: Vec<(ChunkId, u64)> = chunks.iter().map(|c| (*c, 2)).collect();
    meta.add_chunks(u.space, &unique, NOW).await.unwrap();
    let refs: Vec<ChunkRef> = chunks.iter().map(|id| ChunkRef { id: *id, len: 1 }).collect();
    Manifest::new(ChunkerParamsId::new("fastcdc-v1").unwrap(), chunks.len() as u64, FileHash::from_bytes([chunks.len() as u8; 32]), refs).unwrap()
}

fn fresh_chunks(n: usize) -> Vec<ChunkId> {
    (0..n).map(|_| random_chunk()).collect()
}

fn edge_list(chunks: &[ChunkId]) -> Vec<(u64, ChunkId)> {
    chunks.iter().enumerate().map(|(i, c)| (i as u64, *c)).collect()
}

/// The index of a manifest is written in the transaction that records it, exactly: in order, repeated chunks at each
/// of their places; recording it again (another space, the same bytes) changes nothing; a manifest the quota refuses
/// leaves no index behind; an empty file's manifest is complete with no edges.
async fn a_new_manifest_has_exact_edges_complete_in_its_own_transaction(meta: &Meta) {
    use craft_cloud_meta::{EdgeCheck, IndexState};
    let ana = user(meta, "ana").await;
    let bob = user(meta, "bob").await;
    let (a, b, c) = (random_chunk(), random_chunk(), random_chunk());
    let chunks = [a, b, a, c];
    let m = manifest_over(meta, &ana, &chunks).await;
    assert_eq!(meta.index_state(&m.id()).await.unwrap(), IndexState::Absent);
    meta.add_manifest(ana.space, &m, 100, NOW).await.unwrap();
    assert_eq!(meta.index_state(&m.id()).await.unwrap(), IndexState::Complete { total: 4 });
    assert_eq!(meta.index_edges(&m.id()).await.unwrap(), edge_list(&chunks));
    assert_eq!(meta.index_check(&m).await.unwrap(), EdgeCheck::Exact);

    // The same manifest from another space: nothing is written twice.
    let edges = meta.index_edge_count().await.unwrap();
    let again = manifest_over(meta, &bob, &chunks).await;
    meta.add_manifest(bob.space, &again, 100, NOW).await.unwrap();
    assert_eq!(meta.index_edge_count().await.unwrap(), edges);
    assert_eq!(meta.index_state(&m.id()).await.unwrap(), IndexState::Complete { total: 4 });

    // An empty file.
    let empty = Manifest::new(ChunkerParamsId::new("fastcdc-v1").unwrap(), 0, FileHash::from_bytes([0; 32]), vec![]).unwrap();
    meta.add_manifest(ana.space, &empty, 10, NOW).await.unwrap();
    assert_eq!(meta.index_state(&empty.id()).await.unwrap(), IndexState::Complete { total: 0 });
    assert_eq!(meta.index_edge_count().await.unwrap(), edges);

    // A manifest the stored-bytes cap refuses leaves no index: its transaction rolls back whole.
    let capped = meta.clone().with_stored_bytes_cap(Some(50));
    let carol = user(&capped, "carol").await;
    let chunks = fresh_chunks(3);
    let refused = manifest_over(&capped, &carol, &chunks).await;
    assert!(matches!(capped.add_manifest(carol.space, &refused, 1_000, NOW).await, Err(MetaError::QuotaExceeded { .. })));
    assert_eq!(capped.index_state(&refused.id()).await.unwrap(), IndexState::Absent);
    assert_eq!(capped.index_edge_count().await.unwrap(), edges);
    // A manifest whose chunks the space lacks is refused before anything is written.
    let lacking =
        Manifest::new(ChunkerParamsId::new("fastcdc-v1").unwrap(), 1, FileHash::from_bytes([7; 32]), vec![ChunkRef { id: random_chunk(), len: 1 }])
            .unwrap();
    assert!(matches!(meta.add_manifest(ana.space, &lacking, 100, NOW).await, Err(MetaError::MissingChunks(_))));
    assert_eq!(meta.index_state(&lacking.id()).await.unwrap(), IndexState::Absent);
}

/// A manifest longer than the budget is indexed in batches of at most that many edges, each its own transaction, and is
/// complete only after the comparison with its manifest.
async fn a_long_manifest_is_indexed_in_batches_of_the_budget(meta: &Meta) {
    use craft_cloud_meta::{IndexState, Validation};
    let meta = meta.clone().with_index_batch_rows(5);
    let ana = user(&meta, "ana").await;
    let chunks = fresh_chunks(12);
    let m = manifest_over(&meta, &ana, &chunks).await;
    let before = meta.index_edge_count().await.unwrap();
    meta.add_manifest(ana.space, &m, 100, NOW).await.unwrap();
    assert_eq!(meta.index_state(&m.id()).await.unwrap(), IndexState::Building { done: 5, total: 12 });
    assert_eq!(meta.index_edge_count().await.unwrap() - before, 5, "no more than the budget in the recording transaction");
    assert_eq!(meta.manifests_to_index(None, 10).await.unwrap(), vec![m.id()]);

    for (expected_done, expected_rows) in [(10u64, 10u64), (12, 12)] {
        let state = meta.index_stage(&m).await.unwrap();
        assert_eq!(state, IndexState::Building { done: expected_done, total: 12 });
        assert_eq!(meta.index_edge_count().await.unwrap() - before, expected_rows, "at most the budget more per stage");
    }
    // Every edge is written, and still it is not complete: only the comparison makes it so.
    assert_eq!(meta.index_state(&m.id()).await.unwrap(), IndexState::Building { done: 12, total: 12 });
    assert_eq!(meta.index_validate(&m).await.unwrap(), Validation::Complete);
    assert_eq!(meta.index_state(&m.id()).await.unwrap(), IndexState::Complete { total: 12 });
    assert_eq!(meta.index_edges(&m.id()).await.unwrap(), edge_list(&chunks));
    assert!(meta.manifests_to_index(None, 10).await.unwrap().is_empty());
    // Once complete, more stages change nothing.
    assert_eq!(meta.index_stage(&m).await.unwrap(), IndexState::Complete { total: 12 });
    assert_eq!(meta.index_edge_count().await.unwrap() - before, 12);

    // `index_complete` does all of it, and again is a no-op.
    let more = fresh_chunks(23);
    let long = manifest_over(&meta, &ana, &more).await;
    meta.add_manifest(ana.space, &long, 100, NOW).await.unwrap();
    meta.index_complete(&long).await.unwrap();
    meta.index_complete(&long).await.unwrap();
    assert_eq!(meta.index_state(&long.id()).await.unwrap(), IndexState::Complete { total: 23 });
    assert_eq!(meta.index_edges(&long.id()).await.unwrap(), edge_list(&more));
}

/// The index is built from what a manifest says, not from what is there: wrong, swapped, missing and extra edges, a
/// repeated chunk collapsed or changed, and a wrong edge written before the build started are each found by the
/// comparison, the index does not become complete, and a build that repeats the manifest's own list makes it exact.
async fn an_index_is_complete_only_when_every_edge_is_right_on(url: &str) {
    use craft_cloud_meta::{EdgeProblemKind, IndexState, Validation};
    let meta = Meta::open(url).await.unwrap().with_index_batch_rows(3);
    let ana = user(&meta, "ana").await;
    let mut conn = raw(url).await;
    let (a, b, c, d, e, f) = (random_chunk(), random_chunk(), random_chunk(), random_chunk(), random_chunk(), random_chunk());
    let spare = random_chunk();
    // `(what, chunks, damage SQL, expected first problem)`: `$1` is a chunk ID and `$2` the manifest.
    type Case<'a> = (&'a str, Vec<ChunkId>, &'a str, (u64, EdgeProblemKind));
    let cases: Vec<Case> = vec![
        (
            "wrong chunk",
            vec![a, b, c, d, e, f],
            "UPDATE manifest_chunks SET chunk = $1 WHERE manifest = $2 AND ordinal = 2",
            (2, EdgeProblemKind::WrongChunk),
        ),
        (
            "a missing edge",
            vec![a, b, c, d, e, f],
            "DELETE FROM manifest_chunks WHERE manifest = $2 AND ordinal = 4 AND chunk <> $1",
            (4, EdgeProblemKind::MissingEdge),
        ),
        (
            "an extra edge",
            vec![a, b, c, d, e, f],
            "INSERT INTO manifest_chunks (manifest, ordinal, chunk) VALUES ($2, 6, $1)",
            (6, EdgeProblemKind::ExtraEdge),
        ),
        (
            "a repeated chunk changed",
            vec![a, b, a, c],
            "UPDATE manifest_chunks SET chunk = $1 WHERE manifest = $2 AND ordinal = 2",
            (2, EdgeProblemKind::WrongChunk),
        ),
        (
            "a repeated chunk collapsed",
            vec![a, b, a, c],
            "DELETE FROM manifest_chunks WHERE manifest = $2 AND ordinal = 2 AND chunk = $1",
            (2, EdgeProblemKind::MissingEdge),
        ),
    ];
    for (what, chunks, damage, (ordinal, kind)) in cases {
        // A distinct manifest per case: its chunks differ in size by the case.
        let chunks: Vec<ChunkId> =
            chunks.iter().map(|c| ChunkId::from_bytes(*blake3::hash(&[c.as_bytes().as_slice(), what.as_bytes()].concat()).as_bytes())).collect();
        let m = manifest_over(&meta, &ana, &chunks).await;
        meta.add_manifest(ana.space, &m, 100, NOW).await.unwrap();
        while !matches!(meta.index_stage(&m).await.unwrap(), IndexState::Building { done, total } if done == total) {}
        assert!(matches!(meta.index_state(&m.id()).await.unwrap(), IndexState::Building { .. }), "{what}: all edges written, not yet compared");
        // Damage it with every edge written, before the comparison.
        let wrong = chunks.first().map(|c| *c.as_bytes()).unwrap_or(*spare.as_bytes());
        let other = if damage.starts_with("UPDATE") { *spare.as_bytes() } else { wrong };
        sqlx::query(sqlx::AssertSqlSafe(damage.to_owned())).bind(other.to_vec()).bind(m.id().as_bytes().to_vec()).execute(&mut conn).await.unwrap();
        let outcome = meta.index_validate(&m).await.unwrap();
        match outcome {
            Validation::Mismatch(problem) => assert_eq!((problem.ordinal, problem.kind), (ordinal, kind), "{what}"),
            other => panic!("{what}: expected a mismatch, got {other:?}"),
        }
        assert!(!matches!(meta.index_state(&m.id()).await.unwrap(), IndexState::Complete { .. }), "{what}: not complete");
        // A build repeats the manifest's own list and ends exact.
        meta.index_complete(&m).await.unwrap();
        assert_eq!(meta.index_state(&m.id()).await.unwrap(), IndexState::Complete { total: chunks.len() as u64 }, "{what}");
        assert_eq!(meta.index_edges(&m.id()).await.unwrap(), edge_list(&chunks), "{what}");
    }

    // A wrong edge that was there before the build began is not taken for the right one.
    let chunks = fresh_chunks(4);
    let m = manifest_over(&meta, &ana, &chunks).await;
    sqlx::query("INSERT INTO manifest_chunks (manifest, ordinal, chunk) VALUES ($1, 1, $2)")
        .bind(m.id().as_bytes().to_vec())
        .bind(spare.as_bytes().to_vec())
        .execute(&mut conn)
        .await
        .unwrap();
    meta.add_manifest(ana.space, &m, 100, NOW).await.unwrap();
    assert!(!matches!(meta.index_state(&m.id()).await.unwrap(), IndexState::Complete { .. }), "recorded with a wrong edge present");
    meta.index_complete(&m).await.unwrap();
    assert_eq!(meta.index_edges(&m.id()).await.unwrap(), edge_list(&chunks));

    // A complete index that was damaged afterwards is found by the checks that read it, not trusted.
    let chunks = fresh_chunks(3);
    let m = manifest_over(&meta, &ana, &chunks).await;
    meta.add_manifest(ana.space, &m, 100, NOW).await.unwrap();
    assert_eq!(meta.index_state(&m.id()).await.unwrap(), IndexState::Complete { total: 3 });
    sqlx::query("UPDATE manifest_chunks SET chunk = $1 WHERE manifest = $2 AND ordinal = 0")
        .bind(spare.as_bytes().to_vec())
        .bind(m.id().as_bytes().to_vec())
        .execute(&mut conn)
        .await
        .unwrap();
    assert!(matches!(meta.index_check(&m).await.unwrap(), craft_cloud_meta::EdgeCheck::Mismatch(p) if p.ordinal == 0));
    // The builder leaves a complete row alone (it is the checker's to report), and a mismatch there is an error, not a silent rewrite.
    assert_eq!(meta.index_stage(&m).await.unwrap(), IndexState::Complete { total: 3 });
}

/// A build stopped part way resumes where it was, after the database was closed and opened again, and a manifest
/// recorded before the index existed (no row at all) is found and indexed.
async fn an_index_build_resumes_after_a_restart_on(url: &str) {
    use craft_cloud_meta::IndexState;
    let chunks = fresh_chunks(10);
    let (ana, m) = {
        let meta = Meta::open(url).await.unwrap().with_index_batch_rows(4);
        let ana = user(&meta, "ana").await;
        let m = manifest_over(&meta, &ana, &chunks).await;
        meta.add_manifest(ana.space, &m, 100, NOW).await.unwrap();
        assert_eq!(meta.index_stage(&m).await.unwrap(), IndexState::Building { done: 8, total: 10 });
        (ana, m)
    };
    // The "restart": a new handle on the same database.
    let meta = Meta::open(url).await.unwrap().with_index_batch_rows(4);
    assert_eq!(meta.index_state(&m.id()).await.unwrap(), IndexState::Building { done: 8, total: 10 });
    assert_eq!(meta.manifests_to_index(None, 10).await.unwrap(), vec![m.id()]);
    assert_eq!(meta.index_edge_count().await.unwrap(), 8);
    meta.index_complete(&m).await.unwrap();
    assert_eq!(meta.index_edges(&m.id()).await.unwrap(), edge_list(&chunks));
    assert!(meta.manifests_to_index(None, 10).await.unwrap().is_empty());

    // A manifest from before the index: recorded, no row, no edges.
    let old_chunks = fresh_chunks(6);
    let old = manifest_over(&meta, &ana, &old_chunks).await;
    meta.add_manifest(ana.space, &old, 100, NOW).await.unwrap();
    let mut conn = raw(url).await;
    sqlx::query("DELETE FROM manifest_chunks WHERE manifest = $1").bind(old.id().as_bytes().to_vec()).execute(&mut conn).await.unwrap();
    sqlx::query("DELETE FROM manifest_index WHERE manifest = $1").bind(old.id().as_bytes().to_vec()).execute(&mut conn).await.unwrap();
    assert_eq!(meta.index_state(&old.id()).await.unwrap(), IndexState::Absent);
    assert_eq!(meta.manifests_to_index(None, 10).await.unwrap(), vec![old.id()]);
    let audit = meta.index_audit(5).await.unwrap();
    assert_eq!((audit.manifests, audit.complete, audit.absent), (2, 1, 1));
    meta.index_complete(&old).await.unwrap();
    assert_eq!(meta.index_edges(&old.id()).await.unwrap(), edge_list(&old_chunks));
    assert_eq!(meta.index_audit(5).await.unwrap().complete, 2);
}

/// The migration that adds the index does not pretend: manifests recorded before it have no row and are reported as not
/// indexed until a build indexes them.
async fn the_index_migration_leaves_old_manifests_unindexed_on(url: &str) {
    use craft_cloud_meta::IndexState;
    let (m, ana) = {
        let meta = Meta::open(url).await.unwrap();
        let ana = user(&meta, "ana").await;
        let m = manifest_over(&meta, &ana, &fresh_chunks(3)).await;
        meta.add_manifest(ana.space, &m, 100, NOW).await.unwrap();
        (m, ana)
    };
    let _ = ana;
    {
        let mut conn = raw(url).await;
        for statement in ["DROP TABLE manifest_chunks", "DROP TABLE manifest_index", "DELETE FROM schema_version WHERE version >= 9"] {
            sqlx::query(statement).execute(&mut conn).await.unwrap();
        }
    }
    let meta = Meta::open(url).await.unwrap();
    assert_eq!(meta.index_state(&m.id()).await.unwrap(), IndexState::Absent, "recorded before the index: not indexed");
    let audit = meta.index_audit(5).await.unwrap();
    assert_eq!((audit.manifests, audit.complete, audit.absent, audit.building), (1, 0, 1, 0));
    let mut conn = raw(url).await;
    let latest: i64 = sqlx::query_scalar("SELECT MAX(version) FROM schema_version").fetch_one(&mut conn).await.unwrap();
    assert_eq!(latest, 9);
}

/// What the database alone can say is wrong, in seeded damage: orphan edges, an index row without its manifest, an edge
/// to a chunk that is not recorded, a root naming a manifest that is not recorded; and the notices: manifests no version
/// or snapshot names (the live-document asset case) and chunks no manifest names.
async fn the_audit_names_what_is_wrong_on(url: &str) {
    let meta = Meta::open(url).await.unwrap();
    let ana = user(&meta, "ana").await;
    let dev = device(&meta, &ana).await;
    let r = root(&meta, &ana).await;
    let mut conn = raw(url).await;

    // A healthy file, an asset no version names, and a chunk no manifest names.
    let chunks = fresh_chunks(3);
    let file_manifest = manifest_over(&meta, &ana, &chunks).await;
    meta.add_manifest(ana.space, &file_manifest, 100, NOW).await.unwrap();
    let node = NodeId::generate();
    let v = version(file_manifest.id());
    meta.commit(ana.space, dev, &[Op::Create { node, parent: r, name: name("f.txt"), version: v }], NOW).await.unwrap();
    let asset = manifest_over(&meta, &ana, &fresh_chunks(2)).await;
    meta.add_manifest(ana.space, &asset, 100, NOW).await.unwrap();
    let stray = random_chunk();
    meta.add_chunk(ana.space, &stray, 10, NOW).await.unwrap();
    let healthy = meta.index_audit(5).await.unwrap();
    assert_eq!((healthy.manifests, healthy.complete, healthy.building, healthy.absent), (2, 2, 0, 0));
    assert_eq!(healthy.unrooted_manifests.count, 1, "the asset is named by no version");
    assert_eq!(healthy.unrooted_manifests.examples, vec![asset.id().to_string()]);
    assert_eq!(healthy.unreferenced_chunks.count, 1);
    for finding in [
        &healthy.index_without_manifest,
        &healthy.edges_without_index,
        &healthy.complete_with_wrong_count,
        &healthy.unknown_dependencies,
        &healthy.roots_without_manifest,
        &healthy.roots_not_indexed,
        &healthy.space_blobs_without_blob,
    ] {
        assert_eq!(finding.count, 0, "{finding:?}");
    }

    // Damage: orphan edges, an index row without a manifest, an unknown dependency, a root without its manifest.
    let ghost = ManifestId::from_bytes([0xaa; 32]);
    sqlx::query("INSERT INTO manifest_chunks (manifest, ordinal, chunk) VALUES ($1, 0, $2)")
        .bind(ghost.as_bytes().to_vec())
        .bind(stray.as_bytes().to_vec())
        .execute(&mut conn)
        .await
        .unwrap();
    let ghost_row = ManifestId::from_bytes([0xbb; 32]);
    sqlx::query("INSERT INTO manifest_index (manifest, chunk_count, indexed_ordinals, state) VALUES ($1, 0, 0, 1)")
        .bind(ghost_row.as_bytes().to_vec())
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("DELETE FROM blobs WHERE kind = 0 AND id = $1").bind(chunks[1].as_bytes().to_vec()).execute(&mut conn).await.unwrap();
    sqlx::query("DELETE FROM blobs WHERE kind = 1 AND id = $1").bind(file_manifest.id().as_bytes().to_vec()).execute(&mut conn).await.unwrap();
    let damaged = meta.index_audit(5).await.unwrap();
    assert_eq!(damaged.edges_without_index.examples, vec![ghost.to_string()]);
    assert_eq!(damaged.index_without_manifest.count, 2, "the made-up row and the manifest whose record was removed");
    assert!(damaged.index_without_manifest.examples.contains(&ghost_row.to_string()));
    assert_eq!(damaged.unknown_dependencies.count, 1);
    assert_eq!(damaged.unknown_dependencies.examples, vec![format!("{}/{}", file_manifest.id(), chunks[1])]);
    assert_eq!(damaged.roots_without_manifest.examples, vec![file_manifest.id().to_string()], "the version names a manifest that is gone");
    assert_eq!(damaged.space_blobs_without_blob.count, 2, "the memberships of the removed blobs");
    // A complete index whose edge count is not its chunk count.
    sqlx::query("DELETE FROM manifest_chunks WHERE manifest = $1 AND ordinal = 0")
        .bind(asset.id().as_bytes().to_vec())
        .execute(&mut conn)
        .await
        .unwrap();
    assert_eq!(meta.index_audit(5).await.unwrap().complete_with_wrong_count.examples, vec![asset.id().to_string()]);
}

/// Every path that records or uses a manifest leaves the index exactly as the manifests made it: commit, update,
/// restore of a version, restore from the trash, a late save record, a live document's import and compaction. Their
/// roots are always indexed, and none of them writes an edge.
async fn the_index_stays_exact_through_every_write_path(meta: &Meta) {
    use craft_cloud_proto::live::node_doc;
    let ana = user(meta, "ana").await;
    let dev = device(meta, &ana).await;
    let r = root(meta, &ana).await;
    let clean = |label: &'static str, edges: u64| async move {
        let audit = meta.index_audit(5).await.unwrap();
        assert_eq!(audit.complete, audit.manifests, "{label}: every manifest is complete");
        assert_eq!(audit.roots_not_indexed.count + audit.roots_without_manifest.count + audit.unknown_dependencies.count, 0, "{label}");
        assert_eq!(audit.edges_without_index.count + audit.index_without_manifest.count + audit.complete_with_wrong_count.count, 0, "{label}");
        assert_eq!(meta.index_edge_count().await.unwrap(), edges, "{label}: the path wrote no edge");
    };
    let (c1, c2, c3) = (fresh_chunks(3), fresh_chunks(2), fresh_chunks(4));
    let (m1, m2, m3) = (manifest_over(meta, &ana, &c1).await, manifest_over(meta, &ana, &c2).await, manifest_over(meta, &ana, &c3).await);
    for m in [&m1, &m2, &m3] {
        meta.add_manifest(ana.space, m, 100, NOW).await.unwrap();
    }
    let edges = 3 + 2 + 4;
    clean("uploads", edges).await;

    let node = NodeId::generate();
    let v1 = version(m1.id());
    meta.commit(ana.space, dev, &[Op::Create { node, parent: r, name: name("path.txt"), version: v1.clone() }], NOW).await.unwrap();
    clean("create", edges).await;
    let v2 = version(m2.id());
    meta.commit(ana.space, dev, &[Op::Update { node, expected: v1.id, version: v2.clone() }], NOW + 1).await.unwrap();
    clean("update", edges).await;
    let restored = meta.restore_version(ana.space, dev, node, v1.id, v2.id, NOW + 2).await.unwrap();
    clean("restore of a version", edges).await;
    let record = craft_cloud_proto::SaveRecord { doc: node_doc(ana.space, node), frontiers: vec![(1, 2)] };
    meta.attach_save(ana.space, node, v2.id, &record).await.unwrap();
    clean("a late save record", edges).await;
    meta.commit(ana.space, dev, &[Op::Delete { node, expected: Some(restored.version) }], NOW + 3).await.unwrap();
    clean("delete", edges).await;
    meta.restore_from_trash(ana.space, dev, node, NOW + 4).await.unwrap();
    clean("restore from the trash", edges).await;

    // A live document of a file: its import snapshot and a compaction's snapshot are roots too.
    let doc_node = file(meta, &ana, "doc.txt").await;
    let doc = node_doc(ana.space, doc_node);
    let first = snapshot(meta, &ana, 100).await;
    let edges = meta.index_edge_count().await.unwrap();
    meta.create_live_document(ana.space, doc, "document:wordcraft", doc_node, &first.manifest, NOW).await.unwrap();
    clean("a live document's import", edges).await;
    meta.append_live_update(ana.space, doc, dev, 1, &[1; 10], NOW).await.unwrap();
    let compacted = snapshot(meta, &ana, 40).await;
    let edges = meta.index_edge_count().await.unwrap();
    assert!(meta.compact_live_doc(ana.space, doc, 1, &compacted.manifest).await.unwrap());
    clean("a compaction", edges).await;
    let audit = meta.index_audit(5).await.unwrap();
    assert_eq!(audit.unrooted_manifests.count, 1, "only m3 is named by nothing");
}

/// A build runs beside commits and uploads: manifests recorded before the index (no rows) are indexed while new files
/// are committed and new manifests recorded, and the result is exact for all of them.
async fn a_build_runs_beside_commits_on(url: &str) {
    use craft_cloud_meta::IndexState;
    let meta = Meta::open(url).await.unwrap().with_index_batch_rows(3);
    let ana = user(&meta, "ana").await;
    let dev = device(&meta, &ana).await;
    let r = root(&meta, &ana).await;
    let mut conn = raw(url).await;
    let mut old = Vec::new();
    for i in 0..6 {
        let chunks = fresh_chunks(7 + i);
        let m = manifest_over(&meta, &ana, &chunks).await;
        meta.add_manifest(ana.space, &m, 100, NOW).await.unwrap();
        sqlx::query("DELETE FROM manifest_chunks WHERE manifest = $1").bind(m.id().as_bytes().to_vec()).execute(&mut conn).await.unwrap();
        sqlx::query("DELETE FROM manifest_index WHERE manifest = $1").bind(m.id().as_bytes().to_vec()).execute(&mut conn).await.unwrap();
        old.push((m, chunks));
    }
    let builder = {
        let (meta, old) = (meta.clone(), old.clone());
        tokio::spawn(async move {
            for (m, _) in &old {
                meta.index_complete(m).await.unwrap();
            }
        })
    };
    let writer = {
        let (meta, space) = (meta.clone(), ana.space);
        tokio::spawn(async move {
            let mut made = Vec::new();
            for i in 0..8 {
                let chunks = fresh_chunks(5);
                let ids: Vec<(ChunkId, u64)> = chunks.iter().map(|c| (*c, 2)).collect();
                meta.add_chunks(space, &ids, NOW).await.unwrap();
                let refs: Vec<ChunkRef> = chunks.iter().map(|id| ChunkRef { id: *id, len: 1 }).collect();
                let m = Manifest::new(ChunkerParamsId::new("fastcdc-v1").unwrap(), 5, FileHash::from_bytes([i; 32]), refs).unwrap();
                meta.add_manifest(space, &m, 100, NOW).await.unwrap();
                // As the server's upload handler does, beside the builder: the part the recording left.
                meta.index_complete(&m).await.unwrap();
                let node = NodeId::generate();
                let v = NewVersion { id: VersionId::generate(), manifest: m.id(), mtime_ms: NOW, exec: false, save: None };
                meta.commit(space, dev, &[Op::Create { node, parent: r, name: name(&format!("w{i}.txt")), version: v }], NOW).await.unwrap();
                made.push((m, chunks));
            }
            made
        })
    };
    builder.await.unwrap();
    let made = writer.await.unwrap();
    for (m, chunks) in old.iter().chain(made.iter()) {
        assert_eq!(meta.index_state(&m.id()).await.unwrap(), IndexState::Complete { total: chunks.len() as u64 });
        assert_eq!(meta.index_edges(&m.id()).await.unwrap(), edge_list(chunks));
    }
    let audit = meta.index_audit(5).await.unwrap();
    assert_eq!((audit.manifests, audit.complete), (14, 14));
    assert_eq!(audit.roots_not_indexed.count, 0);
}

#[tokio::test]
async fn an_index_is_complete_only_when_every_edge_is_right_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    an_index_is_complete_only_when_every_edge_is_right_on(&format!("sqlite:{}", dir.path().join("meta.db").display())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_index_is_complete_only_when_every_edge_is_right_on_postgres() {
    let Some((url, _guard)) = postgres_database().await else {
        eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set");
        return;
    };
    an_index_is_complete_only_when_every_edge_is_right_on(&url).await;
}

#[tokio::test]
async fn an_index_build_resumes_after_a_restart_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    an_index_build_resumes_after_a_restart_on(&format!("sqlite:{}", dir.path().join("meta.db").display())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_index_build_resumes_after_a_restart_on_postgres() {
    let Some((url, _guard)) = postgres_database().await else {
        eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set");
        return;
    };
    an_index_build_resumes_after_a_restart_on(&url).await;
}

#[tokio::test]
async fn the_index_migration_leaves_old_manifests_unindexed_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    the_index_migration_leaves_old_manifests_unindexed_on(&format!("sqlite:{}", dir.path().join("meta.db").display())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_index_migration_leaves_old_manifests_unindexed_on_postgres() {
    let Some((url, _guard)) = postgres_database().await else {
        eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set");
        return;
    };
    the_index_migration_leaves_old_manifests_unindexed_on(&url).await;
}

#[tokio::test]
async fn the_audit_names_what_is_wrong_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    the_audit_names_what_is_wrong_on(&format!("sqlite:{}", dir.path().join("meta.db").display())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_audit_names_what_is_wrong_on_postgres() {
    let Some((url, _guard)) = postgres_database().await else {
        eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set");
        return;
    };
    the_audit_names_what_is_wrong_on(&url).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_build_runs_beside_commits_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    a_build_runs_beside_commits_on(&format!("sqlite:{}", dir.path().join("meta.db").display())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_build_runs_beside_commits_on_postgres() {
    let Some((url, _guard)) = postgres_database().await else {
        eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set");
        return;
    };
    a_build_runs_beside_commits_on(&url).await;
}

/// An edge at a negative ordinal, which pagination starting at -1 once skipped, is found by the comparison: the index
/// of such a manifest is not marked complete, however it got there, and a build throws the edge away.
async fn an_edge_at_a_negative_ordinal_keeps_an_index_from_completing_on(url: &str) {
    use craft_cloud_meta::{EdgeProblemKind, IndexState, Validation};
    let meta = Meta::open(url).await.unwrap().with_index_batch_rows(3);
    let ana = user(&meta, "ana").await;
    let mut conn = raw(url).await;
    let spare = random_chunk();
    for ordinal in [-1i64, -7, i64::MIN] {
        let chunks = fresh_chunks(5);
        let m = manifest_over(&meta, &ana, &chunks).await;
        // The stray edge is there before the manifest is recorded, and stays after all its own edges were written.
        sqlx::query("INSERT INTO manifest_chunks (manifest, ordinal, chunk) VALUES ($1, $2, $3)")
            .bind(m.id().as_bytes().to_vec())
            .bind(ordinal)
            .bind(spare.as_bytes().to_vec())
            .execute(&mut conn)
            .await
            .unwrap();
        meta.add_manifest(ana.space, &m, 100, NOW).await.unwrap();
        while !matches!(meta.index_stage(&m).await.unwrap(), IndexState::Building { done, total } if done == total) {}
        match meta.index_check(&m).await.unwrap() {
            craft_cloud_meta::EdgeCheck::Mismatch(p) => {
                assert_eq!((p.ordinal, p.kind), (ordinal.unsigned_abs(), EdgeProblemKind::NegativeOrdinal), "{ordinal}")
            }
            other => panic!("{ordinal}: expected a mismatch, got {other:?}"),
        }
        assert!(matches!(meta.index_validate(&m).await.unwrap(), Validation::Mismatch(p) if p.kind == EdgeProblemKind::NegativeOrdinal), "{ordinal}");
        assert!(!matches!(meta.index_state(&m.id()).await.unwrap(), IndexState::Complete { .. }), "{ordinal}: not complete");
        // The build rebuilds from the manifest, and the stray edge is gone.
        meta.index_complete(&m).await.unwrap();
        assert_eq!(meta.index_state(&m.id()).await.unwrap(), IndexState::Complete { total: 5 }, "{ordinal}");
        assert_eq!(meta.index_edges(&m.id()).await.unwrap(), edge_list(&chunks), "{ordinal}");
    }
}

/// Throwing away the edges of an index that starts again takes batches of the index budget, each its own transaction,
/// and a run that stops between two of them leaves a marker the next one resumes from; nothing is written or marked
/// complete while edges of the old list are left.
async fn a_reset_of_an_index_goes_in_batches_and_resumes_on(url: &str) {
    use craft_cloud_meta::IndexState;
    let chunks = fresh_chunks(12);
    let spare = random_chunk();
    let m = {
        let meta = Meta::open(url).await.unwrap().with_index_batch_rows(3);
        let ana = user(&meta, "ana").await;
        let m = manifest_over(&meta, &ana, &chunks).await;
        meta.add_manifest(ana.space, &m, 100, NOW).await.unwrap();
        while !matches!(meta.index_stage(&m).await.unwrap(), IndexState::Building { done, total } if done == total) {}
        m
    };
    let mut conn = raw(url).await;
    // A wrong edge, and one at a negative ordinal; the index was not marked complete.
    sqlx::query("UPDATE manifest_chunks SET chunk = $1 WHERE manifest = $2 AND ordinal = 5")
        .bind(spare.as_bytes().to_vec())
        .bind(m.id().as_bytes().to_vec())
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO manifest_chunks (manifest, ordinal, chunk) VALUES ($1, -1, $2)")
        .bind(m.id().as_bytes().to_vec())
        .bind(spare.as_bytes().to_vec())
        .execute(&mut conn)
        .await
        .unwrap();
    // The reset starts (what a build does on a mismatch) and the process stops right after: only the marker is there.
    sqlx::query("UPDATE manifest_index SET indexed_ordinals = 0, state = 2 WHERE manifest = $1")
        .bind(m.id().as_bytes().to_vec())
        .execute(&mut conn)
        .await
        .unwrap();
    // A new handle carries on, one batch of 3 rows per transaction, and writes nothing meanwhile.
    let meta = Meta::open(url).await.unwrap().with_index_batch_rows(3);
    assert_eq!(meta.index_edge_count().await.unwrap(), 13);
    let mut left = 13u64;
    while left > 0 {
        assert_eq!(meta.index_stage(&m).await.unwrap(), IndexState::Building { done: 0, total: 12 }, "{left} edges left");
        let now = meta.index_edge_count().await.unwrap();
        assert_eq!(left - now, left.min(3), "a batch deletes at most the budget");
        assert!(!matches!(meta.index_state(&m.id()).await.unwrap(), IndexState::Complete { .. }));
        left = now;
    }
    // The last batch (fewer rows than the budget) put the row back to building, so the next stage writes edges.
    assert_eq!(meta.index_edge_count().await.unwrap(), 0);
    assert_eq!(meta.index_stage(&m).await.unwrap(), IndexState::Building { done: 3, total: 12 });
    meta.index_complete(&m).await.unwrap();
    assert_eq!(meta.index_state(&m.id()).await.unwrap(), IndexState::Complete { total: 12 });
    assert_eq!(meta.index_edges(&m.id()).await.unwrap(), edge_list(&chunks));
}

/// A build that meets a wrong edge resets the index across several batches by itself, and ends exact.
async fn a_build_resets_a_wrong_index_in_batches_on(url: &str) {
    use craft_cloud_meta::IndexState;
    let chunks = fresh_chunks(10);
    let meta = Meta::open(url).await.unwrap().with_index_batch_rows(2);
    let ana = user(&meta, "ana").await;
    let m = manifest_over(&meta, &ana, &chunks).await;
    meta.add_manifest(ana.space, &m, 100, NOW).await.unwrap();
    while !matches!(meta.index_stage(&m).await.unwrap(), IndexState::Building { done, total } if done == total) {}
    let mut conn = raw(url).await;
    sqlx::query("UPDATE manifest_chunks SET chunk = $1 WHERE manifest = $2 AND ordinal = 7")
        .bind(random_chunk().as_bytes().to_vec())
        .bind(m.id().as_bytes().to_vec())
        .execute(&mut conn)
        .await
        .unwrap();
    meta.index_complete(&m).await.unwrap();
    assert_eq!(meta.index_state(&m.id()).await.unwrap(), IndexState::Complete { total: 10 });
    assert_eq!(meta.index_edges(&m.id()).await.unwrap(), edge_list(&chunks));
}

/// An extra edge at the highest possible ordinal, `i64::MAX`, after valid ones is reported as a missing edge at the first
/// ordinal that is not there, and the report is stable.
///
/// This does not reach the `raw.checked_add(1)` that guards the page cursor in `compare`. An edge at `i64::MAX` is
/// returned as `MissingEdge` as soon as it is seen, because its ordinal is above the next expected one, and no
/// manifest has the 2^63 chunks that would make it the expected one, so the cursor step at `i64::MAX` cannot be
/// reached through the public meta API. The test pins that a high-ordinal edge is rejected without an error or an
/// overflow, not the checked addition.
async fn an_extra_edge_at_the_highest_ordinal_is_reported_as_missing_on(url: &str) {
    use craft_cloud_meta::{EdgeCheck, EdgeProblemKind};
    let meta = Meta::open(url).await.unwrap().with_index_batch_rows(2);
    let ana = user(&meta, "ana").await;
    let chunks = fresh_chunks(3);
    let m = manifest_over(&meta, &ana, &chunks).await;
    meta.add_manifest(ana.space, &m, 100, NOW).await.unwrap();
    meta.index_complete(&m).await.unwrap();
    let mut conn = raw(url).await;
    sqlx::query("INSERT INTO manifest_chunks (manifest, ordinal, chunk) VALUES ($1, $2, $3)")
        .bind(m.id().as_bytes().to_vec())
        .bind(i64::MAX)
        .bind(random_chunk().as_bytes().to_vec())
        .execute(&mut conn)
        .await
        .unwrap();

    let expected = EdgeCheck::Mismatch(craft_cloud_meta::EdgeProblem { ordinal: 3, kind: EdgeProblemKind::MissingEdge });
    assert_eq!(meta.index_check(&m).await.unwrap(), expected);
    assert_eq!(meta.index_check(&m).await.unwrap(), expected, "the report is stable");
    let edges = meta.index_edges(&m.id()).await.unwrap();
    assert_eq!(edges.len(), 4);
    assert_eq!(edges.last().unwrap().0, i64::MAX as u64, "the stored edge at the maximum ordinal is still there");
}

/// A reset of 12 edges at budget 3 takes four full batches and one empty batch before returning to building state 0.
async fn an_exact_multiple_index_reset_finishes_after_one_empty_batch_on(url: &str) {
    use craft_cloud_meta::IndexState;
    let meta = Meta::open(url).await.unwrap().with_index_batch_rows(3);
    let ana = user(&meta, "ana").await;
    let chunks = fresh_chunks(12);
    let m = manifest_over(&meta, &ana, &chunks).await;
    meta.add_manifest(ana.space, &m, 100, NOW).await.unwrap();
    meta.index_complete(&m).await.unwrap();
    let before = meta.index_edge_count().await.unwrap();
    let mut conn = raw(url).await;
    let changed = sqlx::query("UPDATE manifest_chunks SET chunk = $1 WHERE manifest = $2 AND ordinal = 5")
        .bind(random_chunk().as_bytes().to_vec())
        .bind(m.id().as_bytes().to_vec())
        .execute(&mut conn)
        .await
        .unwrap();
    assert_eq!(changed.rows_affected(), 1, "the index damage must hit one existing edge");
    sqlx::query("UPDATE manifest_index SET indexed_ordinals = 0, state = 2 WHERE manifest = $1")
        .bind(m.id().as_bytes().to_vec())
        .execute(&mut conn)
        .await
        .unwrap();

    for batch in 1..=4 {
        assert_eq!(meta.index_stage(&m).await.unwrap(), IndexState::Building { done: 0, total: 12 });
        assert_eq!(meta.index_edge_count().await.unwrap(), before - batch * 3);
    }
    let state: i64 = sqlx::query_scalar("SELECT state FROM manifest_index WHERE manifest = $1")
        .bind(m.id().as_bytes().to_vec())
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(state, 2, "four full batches leave reset state 2 until an empty batch proves completion");
    assert_eq!(meta.index_edge_count().await.unwrap(), before - 12);

    assert_eq!(meta.index_stage(&m).await.unwrap(), IndexState::Building { done: 0, total: 12 });
    let state: i64 = sqlx::query_scalar("SELECT state FROM manifest_index WHERE manifest = $1")
        .bind(m.id().as_bytes().to_vec())
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(state, 0);
    assert_eq!(meta.index_edge_count().await.unwrap(), before - 12);
    assert_eq!(meta.index_stage(&m).await.unwrap(), IndexState::Building { done: 3, total: 12 }, "the next call writes instead of looping");
    meta.index_complete(&m).await.unwrap();
    assert_eq!(meta.index_state(&m.id()).await.unwrap(), IndexState::Complete { total: 12 });
    assert_eq!(meta.index_edges(&m.id()).await.unwrap(), edge_list(&chunks));
}

/// Two independent Postgres handles finish one damaged manifest reset without duplicate or missing ordinals.
async fn concurrent_index_completes_reset_one_manifest_on(url: &str) {
    use craft_cloud_meta::IndexState;
    let left = Meta::open(url).await.unwrap().with_index_batch_rows(3);
    let right = Meta::open(url).await.unwrap().with_index_batch_rows(3);
    let ana = user(&left, "ana").await;
    let chunks = fresh_chunks(12);
    let m = manifest_over(&left, &ana, &chunks).await;
    left.add_manifest(ana.space, &m, 100, NOW).await.unwrap();
    left.index_complete(&m).await.unwrap();
    let mut conn = raw(url).await;
    let changed = sqlx::query("UPDATE manifest_chunks SET chunk = $1 WHERE manifest = $2 AND ordinal = 5")
        .bind(random_chunk().as_bytes().to_vec())
        .bind(m.id().as_bytes().to_vec())
        .execute(&mut conn)
        .await
        .unwrap();
    assert_eq!(changed.rows_affected(), 1, "the index damage must hit one existing edge");
    sqlx::query("UPDATE manifest_index SET indexed_ordinals = 0, state = 2 WHERE manifest = $1")
        .bind(m.id().as_bytes().to_vec())
        .execute(&mut conn)
        .await
        .unwrap();

    // Force the overlap: a third connection holds the row lock of the manifest's index row, both builders start and are
    // proven to wait on it, and only then is it released, so both reach the reset together.
    let mut holder = raw(url).await;
    sqlx::query("BEGIN").execute(&mut holder).await.unwrap();
    sqlx::query("SELECT 1 FROM manifest_index WHERE manifest = $1 FOR UPDATE").bind(m.id().as_bytes().to_vec()).execute(&mut holder).await.unwrap();

    let left_manifest = m.clone();
    let right_manifest = m.clone();
    let left_builder = left.clone();
    let mut first = tokio::spawn(async move { left_builder.index_complete(&left_manifest).await });
    let mut second = tokio::spawn(async move { right.index_complete(&right_manifest).await });
    let mut waiting = 0i64;
    for _ in 0..300 {
        waiting = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock' AND query LIKE '%manifest_index%FOR UPDATE%'",
        )
        .fetch_one(&mut conn)
        .await
        .unwrap();
        if waiting >= 2 || first.is_finished() || second.is_finished() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let overlapped = waiting == 2 && !first.is_finished() && !second.is_finished();
    sqlx::query("ROLLBACK").execute(&mut holder).await.unwrap();
    if !overlapped {
        first.abort();
        second.abort();
    }
    assert!(overlapped, "both builders must be blocked on the manifest's index row at once (waiting backends: {waiting})");
    let joined = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        (&mut first).await.unwrap().unwrap();
        (&mut second).await.unwrap().unwrap();
    })
    .await;
    if joined.is_err() {
        first.abort();
        second.abort();
    }
    assert!(joined.is_ok(), "concurrent index completion must finish within 30 seconds");

    assert_eq!(left.index_state(&m.id()).await.unwrap(), IndexState::Complete { total: 12 });
    assert_eq!(left.index_edges(&m.id()).await.unwrap(), edge_list(&chunks));
    let (edges, ordinals): (i64, i64) = sqlx::query_as("SELECT COUNT(*), COUNT(DISTINCT ordinal) FROM manifest_chunks WHERE manifest = $1")
        .bind(m.id().as_bytes().to_vec())
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!((edges, ordinals), (12, 12));
}

#[tokio::test]
async fn an_edge_at_a_negative_ordinal_keeps_an_index_from_completing_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    an_edge_at_a_negative_ordinal_keeps_an_index_from_completing_on(&format!("sqlite:{}", dir.path().join("meta.db").display())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_edge_at_a_negative_ordinal_keeps_an_index_from_completing_on_postgres() {
    let Some((url, _guard)) = postgres_database().await else {
        eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set");
        return;
    };
    an_edge_at_a_negative_ordinal_keeps_an_index_from_completing_on(&url).await;
}

#[tokio::test]
async fn a_reset_of_an_index_goes_in_batches_and_resumes_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    a_reset_of_an_index_goes_in_batches_and_resumes_on(&format!("sqlite:{}", dir.path().join("meta.db").display())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reset_of_an_index_goes_in_batches_and_resumes_on_postgres() {
    let Some((url, _guard)) = postgres_database().await else {
        eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set");
        return;
    };
    a_reset_of_an_index_goes_in_batches_and_resumes_on(&url).await;
}

#[tokio::test]
async fn a_build_resets_a_wrong_index_in_batches_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    a_build_resets_a_wrong_index_in_batches_on(&format!("sqlite:{}", dir.path().join("meta.db").display())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_build_resets_a_wrong_index_in_batches_on_postgres() {
    let Some((url, _guard)) = postgres_database().await else {
        eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set");
        return;
    };
    a_build_resets_a_wrong_index_in_batches_on(&url).await;
}

#[tokio::test]
async fn an_extra_edge_at_the_highest_ordinal_is_reported_as_missing_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    an_extra_edge_at_the_highest_ordinal_is_reported_as_missing_on(&format!("sqlite:{}", dir.path().join("meta.db").display())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_extra_edge_at_the_highest_ordinal_is_reported_as_missing_on_postgres() {
    let Some((url, _guard)) = postgres_database().await else {
        eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set");
        return;
    };
    an_extra_edge_at_the_highest_ordinal_is_reported_as_missing_on(&url).await;
}

#[tokio::test]
async fn an_exact_multiple_index_reset_finishes_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    an_exact_multiple_index_reset_finishes_after_one_empty_batch_on(&format!("sqlite:{}", dir.path().join("meta.db").display())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_exact_multiple_index_reset_finishes_on_postgres() {
    let Some((url, _guard)) = postgres_database().await else {
        eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set");
        return;
    };
    an_exact_multiple_index_reset_finishes_after_one_empty_batch_on(&url).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_index_completes_reset_one_manifest_on_postgres() {
    let Some((url, _guard)) = postgres_database().await else {
        eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set");
        return;
    };
    concurrent_index_completes_reset_one_manifest_on(&url).await;
}
