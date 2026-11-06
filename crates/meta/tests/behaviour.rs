//! The same scenarios on every backend. SQLite always runs (a fresh temporary file per test). Postgres runs
//! when `CRAFT_CLOUD_TEST_PG` holds an admin URL, e.g. `postgres://postgres:pw@127.0.0.1:5432/postgres`; each
//! test creates and drops its own database. CI sets `CRAFT_CLOUD_REQUIRE_BACKENDS`, which turns a missing
//! `CRAFT_CLOUD_TEST_PG` into a failure instead of a skip.

use craft_cloud_meta::{
    AdminActor, AdminUser, BlobKind, DeviceClient, DeviceRecord, InviteFor, MIN_QUOTA_BYTES, Meta, MetaError, RESET_TTL_MS, Redeemed, RevokeReason,
    Role, UserId, UserRecord,
};
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
    a_device_records_its_client,
    only_own_live_devices_are_renamed,
    every_revoke_path_sets_flag_time_and_reason_together,
    revoking_others_keeps_the_caller_and_ends_their_pairings,
    a_password_change_is_one_step,
    an_admin_reset_signs_out_every_device_unless_kept,
    concurrent_password_changes_and_sign_outs_never_fail_the_database,
    a_sign_in_checked_against_an_old_password_creates_no_device,
    redeems_sign_ins_and_resets_at_the_same_moment_leave_no_live_device,
    the_admin_list_pages_searches_and_counts,
    a_sign_in_checked_before_a_disable_and_an_enable_creates_no_device,
    an_admin_makes_an_account_with_a_role_and_an_invite,
    an_admin_changes_a_role_and_a_quota,
    the_acting_admins_state_is_checked_in_the_transaction,
    disabling_signs_everything_out_and_enabling_brings_back_nothing,
    issuing_access_while_an_account_is_disabled_leaves_no_usable_token,
    invites_and_reset_codes_replace_each_other,
    a_reset_code_sets_a_password_and_signs_everything_out,
    a_quota_overrides_the_server_wide_cap,
    an_admin_lists_and_revokes_a_users_devices,
    two_demotions_at_once_keep_an_admin,
    two_self_demotions_at_once_keep_an_admin,
    two_disables_at_once_keep_an_admin,
    a_demotion_and_a_disable_at_once_keep_an_admin,
    a_removal_and_a_demotion_at_once_keep_an_admin,
    a_self_demotion_and_a_disable_of_the_other_at_once_keep_an_admin,
    every_operation_writes_exactly_its_events,
    a_refused_change_writes_no_event,
    failed_sign_ins_count_up_in_one_row_per_account_and_minute,
    concurrent_failures_with_one_key_make_one_row_with_every_count,
    audit_lists_filter_and_page_by_cursor,
    pruning_works_in_batches_and_keeps_its_time_until_done,
    a_removed_accounts_events_stay,
    no_event_holds_a_name_or_a_secret,
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

    meta.reset_password(ana.id, "$argon2id$new", true, NOW + 1).await.unwrap();
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
        // Before the audit log.
        sqlx::query("DROP TABLE audit_events").execute(&mut conn).await.unwrap();
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
        sqlx::query("ALTER TABLE versions DROP COLUMN save").execute(&mut conn).await.unwrap();
        // And before the admin API (its `invites` column went with the table).
        sqlx::query("ALTER TABLE spaces DROP COLUMN quota_bytes").execute(&mut conn).await.unwrap();
        sqlx::query("ALTER TABLE users DROP COLUMN removed_ms").execute(&mut conn).await.unwrap();
        sqlx::query("ALTER TABLE users DROP COLUMN auth_gen").execute(&mut conn).await.unwrap();
        // And before device sessions.
        for column in ["client_kind", "platform", "app_version", "revoked_ms", "revoke_reason"] {
            sqlx::query(sqlx::AssertSqlSafe(format!("ALTER TABLE devices DROP COLUMN {column}"))).execute(&mut conn).await.unwrap();
        }
        for column in ["password_changed_ms", "disabled_ms"] {
            sqlx::query(sqlx::AssertSqlSafe(format!("ALTER TABLE users DROP COLUMN {column}"))).execute(&mut conn).await.unwrap();
        }
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

// ---- device sessions (migration 0007) ----

/// A device's record, read back from the list.
async fn record(meta: &Meta, user: &UserRecord, id: DeviceId) -> DeviceRecord {
    meta.list_devices(user.id).await.unwrap().into_iter().find(|d| d.id == id).unwrap()
}

/// The three columns of a revoke always agree: all set or none.
fn assert_revoke_columns_agree(devices: &[DeviceRecord]) {
    for d in devices {
        assert_eq!((d.revoked, d.revoked_ms.is_some(), d.revoke_reason.is_some()), (d.revoked, d.revoked, d.revoked), "{d:?}");
    }
}

fn sorted(mut ids: Vec<DeviceId>) -> Vec<DeviceId> {
    ids.sort_by_key(|id| *id.as_uuid());
    ids
}

async fn a_device_records_its_client(meta: &Meta) {
    let ana = user(meta, "ana").await;
    let plain = device(meta, &ana).await;
    let client = DeviceClient::new(Some("agent"), Some("windows x86_64"), Some("0.19.0")).unwrap();
    let agent = meta.create_device_with(ana.id, "studio", &client, &token(), NOW + 1).await.unwrap();
    assert_eq!(record(meta, &ana, agent).await.client, client);
    let old = record(meta, &ana, plain).await;
    assert_eq!((old.client, old.revoked_ms, old.revoke_reason), (DeviceClient::default(), None, None), "no word on the client: empty");

    // What a sign-in may say: a known kind or none, short free text without control characters.
    assert_eq!(DeviceClient::new(None, None, None).unwrap(), DeviceClient::default());
    assert!(DeviceClient::new(Some(""), Some(&"x".repeat(32)), Some(&"y".repeat(32))).is_ok());
    let long = "x".repeat(33);
    for (kind, platform, version) in [
        (Some("browser"), None, None),
        (Some("AGENT"), None, None),
        (None, Some(long.as_str()), None),
        (None, None, Some(long.as_str())),
        (None, Some("a\nb"), None),
        (None, None, Some("1.0\u{7}")),
    ] {
        assert!(matches!(DeviceClient::new(kind, platform, version), Err(MetaError::Invalid(_))), "{kind:?} {platform:?} {version:?}");
    }
    // The store checks too, for a caller that built the value by hand: nothing is created.
    let bad = DeviceClient { kind: "browser".into(), ..DeviceClient::default() };
    assert!(matches!(meta.create_device_with(ana.id, "x", &bad, &token(), NOW).await, Err(MetaError::Invalid(_))));
    assert_eq!(meta.list_devices(ana.id).await.unwrap().len(), 2);

    // A join says it too.
    let (_, _, pairing, _) = paired(meta, "bob").await;
    let desktop = DeviceClient::new(Some("desktop"), Some("macos"), Some("1.2")).unwrap();
    let joined = meta.redeem_with(&pairing.hash(), "mac", &desktop, &token(), None, NOW + 1).await.unwrap();
    let bob = meta.user_by_name("bob").await.unwrap().unwrap();
    assert_eq!(record(meta, &bob, joined.device).await.client, desktop);
}

async fn only_own_live_devices_are_renamed(meta: &Meta) {
    let (ana, bob) = (user(meta, "ana").await, user(meta, "bob").await);
    let (mine, theirs) = (device(meta, &ana).await, device(meta, &bob).await);
    let renamed = meta.rename_device(ana.id, mine, "studio pc").await.unwrap();
    assert_eq!((renamed.id, renamed.name.as_str()), (mine, "studio pc"));
    assert_eq!(record(meta, &ana, mine).await.name, "studio pc");
    // Another account's device, and one that isn't there, are one answer.
    assert!(matches!(meta.rename_device(ana.id, theirs, "mine now").await, Err(MetaError::NotFound(_))));
    assert!(matches!(meta.rename_device(ana.id, DeviceId::generate(), "x").await, Err(MetaError::NotFound(_))));
    assert_eq!(record(meta, &bob, theirs).await.name, "laptop");
    // The name rule holds.
    for bad in ["", "tab\there", &"n".repeat(129)] {
        assert!(matches!(meta.rename_device(ana.id, mine, bad).await, Err(MetaError::Invalid(_))), "{bad:?}");
    }
    assert_eq!(record(meta, &ana, mine).await.name, "studio pc");
    // A revoked device keeps its name.
    meta.revoke_device_as(ana.id, mine, RevokeReason::User, NOW + 1).await.unwrap();
    assert!(matches!(meta.rename_device(ana.id, mine, "again").await, Err(MetaError::NotFound(_))));
    assert_eq!(record(meta, &ana, mine).await.name, "studio pc");
}

async fn every_revoke_path_sets_flag_time_and_reason_together(meta: &Meta) {
    // One device revoked by its user, and revoked again later: the first time and reason stay.
    let ana = user(meta, "ana").await;
    let (a1, a2, a3, a4) = (device(meta, &ana).await, device(meta, &ana).await, device(meta, &ana).await, device(meta, &ana).await);
    meta.revoke_device_as(ana.id, a1, RevokeReason::User, NOW + 10).await.unwrap();
    let first = record(meta, &ana, a1).await;
    assert_eq!((first.revoked, first.revoked_ms, first.revoke_reason), (true, Some(NOW + 10), Some(RevokeReason::User)));
    meta.revoke_device_as(ana.id, a1, RevokeReason::Admin, NOW + 20).await.unwrap();
    assert_eq!(record(meta, &ana, a1).await, first, "a second revoke changes nothing");
    assert!(matches!(meta.revoke_device_as(user(meta, "bob").await.id, a1, RevokeReason::User, NOW).await, Err(MetaError::NotFound(_))));
    // The others, but one: the device revoked before keeps its own reason, the kept one stays.
    let revoked = meta.revoke_other_devices(ana.id, a2, RevokeReason::Others, NOW + 30).await.unwrap();
    assert_eq!(sorted(revoked), sorted(vec![a3, a4]), "only the live others");
    for id in [a3, a4] {
        let d = record(meta, &ana, id).await;
        assert_eq!((d.revoked, d.revoked_ms, d.revoke_reason), (true, Some(NOW + 30), Some(RevokeReason::Others)));
    }
    assert_eq!(record(meta, &ana, a1).await, first);
    let kept = record(meta, &ana, a2).await;
    assert_eq!((kept.revoked, kept.revoked_ms, kept.revoke_reason), (false, None, None));
    assert!(meta.revoke_other_devices(ana.id, a2, RevokeReason::Others, NOW + 40).await.unwrap().is_empty(), "nobody left to sign out");
    // Every device (the admin's command).
    let bob = meta.user_by_name("bob").await.unwrap().unwrap();
    let (b1, b2) = (device(meta, &bob).await, device(meta, &bob).await);
    let revoked = meta.revoke_all_devices(bob.id, RevokeReason::Admin, NOW + 50).await.unwrap();
    assert_eq!(sorted(revoked), sorted(vec![b1, b2]));
    assert!(meta.revoke_all_devices(bob.id, RevokeReason::Admin, NOW + 60).await.unwrap().is_empty());
    for id in [b1, b2] {
        let d = record(meta, &bob, id).await;
        assert_eq!((d.revoked, d.revoked_ms, d.revoke_reason), (true, Some(NOW + 50), Some(RevokeReason::Admin)));
    }
    // A password change and a reset.
    let cy = user(meta, "cy").await;
    let (c1, c2) = (device(meta, &cy).await, device(meta, &cy).await);
    meta.change_password(cy.id, "$argon2id$stub", "$argon2id$new", c1, NOW + 70).await.unwrap();
    let d = record(meta, &cy, c2).await;
    assert_eq!((d.revoked, d.revoked_ms, d.revoke_reason), (true, Some(NOW + 70), Some(RevokeReason::Password)));
    let dee = user(meta, "dee").await;
    let d1 = device(meta, &dee).await;
    meta.reset_password(dee.id, "$argon2id$new", false, NOW + 80).await.unwrap();
    let d = record(meta, &dee, d1).await;
    assert_eq!((d.revoked, d.revoked_ms, d.revoke_reason), (true, Some(NOW + 80), Some(RevokeReason::Reset)));
    // The plain call (no clock of its own) is the user's act, with a time.
    let eve = user(meta, "eve").await;
    let e1 = device(meta, &eve).await;
    meta.revoke_device(eve.id, e1).await.unwrap();
    let d = record(meta, &eve, e1).await;
    assert_eq!((d.revoked, d.revoke_reason), (true, Some(RevokeReason::User)));
    assert!(d.revoked_ms.is_some_and(|ms| ms > NOW));
    for u in [&ana, &bob, &cy, &dee, &eve] {
        assert_revoke_columns_agree(&meta.list_devices(u.id).await.unwrap());
    }
}

async fn revoking_others_keeps_the_caller_and_ends_their_pairings(meta: &Meta) {
    let (ana, keep, keep_pairing, keep_id) = paired(meta, "ana").await;
    let other = device(meta, &ana).await;
    let other_id = meta.create_pairing(ana.id, other, &code(PAIRING_CHARS).hash(), NOW).await.unwrap();
    let (bob, bobs, _, bobs_pairing) = paired(meta, "bob").await;
    let revoked = meta.revoke_other_devices(ana.id, keep, RevokeReason::Others, NOW + 1).await.unwrap();
    assert_eq!(revoked, vec![other]);
    assert!(meta.device_active(keep).await.unwrap() && !meta.device_active(other).await.unwrap());
    assert_eq!(meta.pairing_status(ana.id, keep_id, NOW + 2).await.unwrap(), Some((PairingState::Waiting, None)), "the caller's own stays");
    assert_eq!(meta.pairing_status(ana.id, other_id, NOW + 2).await.unwrap(), None, "a revoked device's pairing dies");
    redeem(meta, &keep_pairing, "pc", NOW + 3).await.unwrap();
    // Another account is never touched.
    assert!(meta.device_active(bobs).await.unwrap());
    assert!(meta.pairing_status(bob.id, bobs_pairing, NOW + 2).await.unwrap().is_some());
}

async fn a_password_change_is_one_step(meta: &Meta) {
    let (ana, keep, _, pairing_id) = paired(meta, "ana").await;
    let other = device(meta, &ana).await;
    let other_pairing = meta.create_pairing(ana.id, other, &code(PAIRING_CHARS).hash(), NOW).await.unwrap();
    let invite = code(INVITE_CHARS);
    meta.create_invite("ana", &invite.hash(), NOW, INVITE_TTL_MS).await.unwrap();
    let (bob, bobs, _, bobs_pairing) = paired(meta, "bob").await;
    let bobs_invite = code(INVITE_CHARS);
    meta.create_invite("bob", &bobs_invite.hash(), NOW, INVITE_TTL_MS).await.unwrap();

    let account = meta.account(ana.id).await.unwrap();
    assert_eq!((account.username.as_str(), account.role, account.created_ms, account.password_changed_ms), ("ana", Role::Member, NOW, None));
    assert!(matches!(meta.account(UserId(uuid::Uuid::now_v7())).await, Err(MetaError::NotFound(_))));

    // A password that is not the one the caller verified (it changed meanwhile): nothing at all changes.
    let stale = meta.change_password(ana.id, "$argon2id$stale", "$argon2id$new", keep, NOW + 5).await;
    assert!(matches!(stale, Err(MetaError::Conflict(_))), "{stale:?}");
    assert_eq!(meta.user_by_name("ana").await.unwrap().unwrap().password_hash, "$argon2id$stub");
    assert_eq!(meta.account(ana.id).await.unwrap().password_changed_ms, None);
    assert!(meta.device_active(keep).await.unwrap() && meta.device_active(other).await.unwrap());
    assert!(meta.pairing_status(ana.id, pairing_id, NOW + 6).await.unwrap().is_some());
    assert!(meta.pairing_status(ana.id, other_pairing, NOW + 6).await.unwrap().is_some());
    assert_eq!(meta.invite_needs_password(&invite.hash(), NOW + 6).await.unwrap(), Some(false));

    // The change: new hash and time, the other device signed out, invite and pairings gone, the caller stays.
    let revoked = meta.change_password(ana.id, "$argon2id$stub", "$argon2id$new", keep, NOW + 7).await.unwrap();
    assert_eq!(revoked, vec![other]);
    assert_eq!(meta.user_by_name("ana").await.unwrap().unwrap().password_hash, "$argon2id$new");
    assert_eq!(meta.account(ana.id).await.unwrap().password_changed_ms, Some(NOW + 7));
    assert!(meta.device_active(keep).await.unwrap() && !meta.device_active(other).await.unwrap());
    let d = record(meta, &ana, other).await;
    assert_eq!((d.revoked_ms, d.revoke_reason), (Some(NOW + 7), Some(RevokeReason::Password)));
    assert_eq!(meta.pairing_status(ana.id, pairing_id, NOW + 8).await.unwrap(), None);
    assert_eq!(meta.pairing_status(ana.id, other_pairing, NOW + 8).await.unwrap(), None);
    assert_eq!(meta.invite_needs_password(&invite.hash(), NOW + 8).await.unwrap(), None);
    assert!(matches!(redeem(meta, &invite, "pc", NOW + 8).await, Err(MetaError::InvalidCode)));
    // The old hash no longer matches, so the same change again (a replayed request) does nothing.
    assert!(matches!(meta.change_password(ana.id, "$argon2id$stub", "$argon2id$third", keep, NOW + 9).await, Err(MetaError::Conflict(_))));
    // A caller that was signed out meanwhile changes nothing either.
    assert!(
        matches!(meta.change_password(ana.id, "$argon2id$new", "$argon2id$third", other, NOW + 9).await, Err(MetaError::DeviceRevoked)),
        "revoked"
    );
    assert!(
        matches!(
            meta.change_password(ana.id, "$argon2id$new", "$argon2id$third", DeviceId::generate(), NOW + 9).await,
            Err(MetaError::DeviceRevoked)
        ),
        "unknown"
    );
    let bobs_device = device(meta, &user(meta, "cy").await).await;
    assert!(
        matches!(meta.change_password(ana.id, "$argon2id$new", "$argon2id$third", bobs_device, NOW + 9).await, Err(MetaError::DeviceRevoked)),
        "another account's"
    );
    assert_eq!(meta.user_by_name("ana").await.unwrap().unwrap().password_hash, "$argon2id$new");
    // Another account is never touched.
    assert_eq!(meta.user_by_name("bob").await.unwrap().unwrap().password_hash, "$argon2id$stub");
    assert!(meta.device_active(bobs).await.unwrap());
    assert!(meta.pairing_status(bob.id, bobs_pairing, NOW + 8).await.unwrap().is_some());
    assert_eq!(meta.invite_needs_password(&bobs_invite.hash(), NOW + 8).await.unwrap(), Some(false));
}

async fn an_admin_reset_signs_out_every_device_unless_kept(meta: &Meta) {
    let (ana, maker, _, pairing_id) = paired(meta, "ana").await;
    let other = device(meta, &ana).await;
    let invite = code(INVITE_CHARS);
    meta.create_invite("ana", &invite.hash(), NOW, INVITE_TTL_MS).await.unwrap();
    let revoked = meta.reset_password(ana.id, "$argon2id$reset", false, NOW + 1).await.unwrap();
    assert_eq!(sorted(revoked), sorted(vec![maker, other]));
    assert!(!meta.device_active(maker).await.unwrap() && !meta.device_active(other).await.unwrap());
    assert_eq!(record(meta, &ana, other).await.revoke_reason, Some(RevokeReason::Reset));
    assert_eq!(meta.pairing_status(ana.id, pairing_id, NOW + 2).await.unwrap(), None);
    assert_eq!(meta.invite_needs_password(&invite.hash(), NOW + 2).await.unwrap(), None);
    assert_eq!(meta.user_by_name("ana").await.unwrap().unwrap().password_hash, "$argon2id$reset");
    assert_eq!(meta.account(ana.id).await.unwrap().password_changed_ms, Some(NOW + 1));

    // `--keep-devices`: the devices stay, the password changes, and codes made before it are still deleted.
    let (bob, bobs, _, bobs_pairing) = paired(meta, "bob").await;
    assert!(meta.reset_password(bob.id, "$argon2id$reset", true, NOW + 3).await.unwrap().is_empty());
    assert!(meta.device_active(bobs).await.unwrap());
    assert_eq!(record(meta, &bob, bobs).await.revoke_reason, None);
    assert_eq!(meta.pairing_status(bob.id, bobs_pairing, NOW + 4).await.unwrap(), None);
    assert_eq!(meta.user_by_name("bob").await.unwrap().unwrap().password_hash, "$argon2id$reset");

    // A user that isn't there changes nothing.
    assert!(matches!(meta.reset_password(UserId(uuid::Uuid::now_v7()), "$argon2id$x", false, NOW).await, Err(MetaError::NotFound(_))));
}

/// A password change from one device, a sign-out of the others from another and an admin reset, at the same moment
/// and over and over: each ends as a success or as a conflict or a signed-out caller, never as a database error (on
/// Postgres, two transactions taking the same rows in different orders would deadlock and one would fail).
async fn concurrent_password_changes_and_sign_outs_never_fail_the_database(meta: &Meta) {
    for round in 0..15 {
        let ana = user(meta, &format!("ana{round}")).await;
        let (a, b) = (device(meta, &ana).await, device(meta, &ana).await);
        let mut tasks = Vec::new();
        for job in 0..6 {
            let meta = meta.clone();
            tasks.push(tokio::spawn(async move {
                match job {
                    0 => meta.change_password(ana.id, "$argon2id$stub", "$argon2id$one", a, NOW + 1).await.map(|_| ()),
                    1 => meta.change_password(ana.id, "$argon2id$stub", "$argon2id$two", b, NOW + 1).await.map(|_| ()),
                    2 => meta.revoke_other_devices(ana.id, b, RevokeReason::Others, NOW + 1).await.map(|_| ()),
                    3 => meta.revoke_device_as(ana.id, a, RevokeReason::User, NOW + 1).await,
                    4 => meta.reset_password(ana.id, "$argon2id$reset", false, NOW + 1).await.map(|_| ()),
                    _ => meta.revoke_all_devices(ana.id, RevokeReason::Admin, NOW + 1).await.map(|_| ()),
                }
            }));
        }
        for task in tasks {
            let result = task.await.unwrap();
            assert!(matches!(result, Ok(()) | Err(MetaError::Conflict(_) | MetaError::DeviceRevoked)), "round {round}: {result:?}");
        }
        assert_revoke_columns_agree(&meta.list_devices(ana.id).await.unwrap());
    }
}

/// A device is created after a password sign-in only if the password that was checked is still the account's.
async fn a_sign_in_checked_against_an_old_password_creates_no_device(meta: &Meta) {
    let ana = user(meta, "ana").await;
    let client = DeviceClient::default();
    let first = meta.sign_in_device(ana.id, ("$argon2id$stub", 0), "pc", &client, &token(), NOW).await.unwrap();
    assert!(first.is_some_and(|d| d != DeviceId::generate()));
    assert!(meta.sign_in_device(ana.id, ("$argon2id$other", 0), "pc", &client, &token(), NOW).await.unwrap().is_none(), "not the stored hash");
    assert!(
        meta.sign_in_device(UserId(uuid::Uuid::now_v7()), ("$argon2id$stub", 0), "pc", &client, &token(), NOW).await.unwrap().is_none(),
        "no such user"
    );
    assert!(matches!(meta.sign_in_device(ana.id, ("$argon2id$stub", 0), "", &client, &token(), NOW).await, Err(MetaError::Invalid(_))));
    // The password was checked, then reset: the account's devices are signed out and the late sign-in adds none.
    meta.reset_password(ana.id, "$argon2id$reset", false, NOW + 1).await.unwrap();
    assert!(meta.sign_in_device(ana.id, ("$argon2id$stub", 0), "late", &client, &token(), NOW + 2).await.unwrap().is_none());
    let devices = meta.list_devices(ana.id).await.unwrap();
    assert_eq!(devices.len(), 1);
    assert!(devices.iter().all(|d| d.revoked));
    // With the new hash it works.
    assert!(meta.sign_in_device(ana.id, ("$argon2id$reset", gen_of(meta, "ana").await), "new", &client, &token(), NOW + 3).await.unwrap().is_some());
}

/// Redeeming an invite or a pairing, a password sign-in and a reset at the same moment, over and over: no database
/// error (on Postgres, the invite and the user taken in different orders would deadlock), and once the reset is
/// done no device of the account is signed in, whichever of them came first.
async fn redeems_sign_ins_and_resets_at_the_same_moment_leave_no_live_device(meta: &Meta) {
    for round in 0..15 {
        let (ana, maker, pairing, _) = paired(meta, &format!("ana{round}")).await;
        let invite = code(INVITE_CHARS);
        meta.create_invite(&format!("ana{round}"), &invite.hash(), NOW, INVITE_TTL_MS).await.unwrap();
        let mut tasks = Vec::new();
        for job in 0..5 {
            let (meta, invite, pairing) = (meta.clone(), invite.hash(), pairing.hash());
            tasks.push(tokio::spawn(async move {
                match job {
                    0 => meta.redeem(&invite, "invited", &token(), None, NOW + 1).await.map(|_| ()),
                    1 => meta.redeem(&pairing, "paired", &token(), None, NOW + 1).await.map(|_| ()),
                    2 => {
                        meta.sign_in_device(ana.id, ("$argon2id$stub", 0), "signed in", &DeviceClient::default(), &token(), NOW + 1).await.map(|_| ())
                    }
                    3 => meta.create_pairing(ana.id, maker, &code(PAIRING_CHARS).hash(), NOW + 1).await.map(|_| ()),
                    _ => meta.reset_password(ana.id, "$argon2id$reset", false, NOW + 2).await.map(|_| ()),
                }
            }));
        }
        for task in tasks {
            let result = task.await.unwrap();
            assert!(matches!(result, Ok(()) | Err(MetaError::InvalidCode | MetaError::NotFound(_))), "round {round}: {result:?}");
        }
        let devices = meta.list_devices(ana.id).await.unwrap();
        assert!(devices.iter().all(|d| d.revoked), "round {round}: {devices:?}");
        assert_revoke_columns_agree(&devices);
    }
}

// ---- the migration from 0006 to 0007 ----

/// Drops what migration 0007 added and forgets it ran, which leaves the database as 0006 left it (with its rows).
async fn back_to_0006(url: &str) {
    use sqlx::Connection;
    let mut conn = sqlx::AnyConnection::connect(url).await.unwrap();
    // What migrations 0009 and 0008 added goes first: they run again with 0007 on the next open.
    sqlx::query("DROP TABLE audit_events").execute(&mut conn).await.unwrap();
    sqlx::query("ALTER TABLE invites DROP COLUMN purpose").execute(&mut conn).await.unwrap();
    sqlx::query("ALTER TABLE spaces DROP COLUMN quota_bytes").execute(&mut conn).await.unwrap();
    sqlx::query("ALTER TABLE users DROP COLUMN removed_ms").execute(&mut conn).await.unwrap();
    sqlx::query("ALTER TABLE users DROP COLUMN auth_gen").execute(&mut conn).await.unwrap();
    for column in ["client_kind", "platform", "app_version", "revoked_ms", "revoke_reason"] {
        sqlx::query(sqlx::AssertSqlSafe(format!("ALTER TABLE devices DROP COLUMN {column}"))).execute(&mut conn).await.unwrap();
    }
    for column in ["password_changed_ms", "disabled_ms"] {
        sqlx::query(sqlx::AssertSqlSafe(format!("ALTER TABLE users DROP COLUMN {column}"))).execute(&mut conn).await.unwrap();
    }
    sqlx::query("DELETE FROM schema_version WHERE version >= 7").execute(&mut conn).await.unwrap();
}

async fn schema_version(url: &str) -> i64 {
    use sqlx::Connection;
    let mut conn = sqlx::AnyConnection::connect(url).await.unwrap();
    sqlx::query_scalar("SELECT MAX(version) FROM schema_version").fetch_one(&mut conn).await.unwrap()
}

/// Existing users and devices (live and revoked) survive the migration, read with empty client fields and no revoke
/// time or reason, still work (a live one is found by its token, a revoked one is not), and take the new columns;
/// a database newer than the program is refused.
async fn migrating_from_0006_keeps_the_rows(url: &str) {
    sqlx::any::install_default_drivers();
    let (ana, live, revoked, old_token) = {
        let meta = Meta::open(url).await.unwrap();
        let ana = user(&meta, "ana").await;
        let old_token = token();
        let live = meta.create_device(ana.id, "old pc", &old_token, NOW).await.unwrap();
        let revoked = device(&meta, &ana).await;
        meta.revoke_device(ana.id, revoked).await.unwrap();
        (ana, live, revoked, old_token)
    };
    back_to_0006(url).await;
    assert_eq!(schema_version(url).await, 6);

    let meta = Meta::open(url).await.unwrap();
    assert_eq!(schema_version(url).await, 9);
    let old = record(&meta, &ana, live).await;
    assert_eq!(
        (old.name.as_str(), old.revoked, old.client, old.revoked_ms, old.revoke_reason),
        ("old pc", false, DeviceClient::default(), None, None)
    );
    let gone = record(&meta, &ana, revoked).await;
    assert_eq!((gone.revoked, gone.client, gone.revoked_ms, gone.revoke_reason), (true, DeviceClient::default(), None, None));
    assert_eq!(meta.device_by_token(&old_token).await.unwrap().map(|a| a.device), Some(live));
    assert_eq!(meta.account(ana.id).await.unwrap().password_changed_ms, None);
    // The new columns take values.
    let client = DeviceClient::new(Some("cli"), Some("linux"), Some("1")).unwrap();
    let fresh = meta.create_device_with(ana.id, "new", &client, &token(), NOW + 1).await.unwrap();
    assert_eq!(record(&meta, &ana, fresh).await.client, client);
    meta.change_password(ana.id, "$argon2id$stub", "$argon2id$new", fresh, NOW + 2).await.unwrap();
    assert_eq!(meta.account(ana.id).await.unwrap().password_changed_ms, Some(NOW + 2));
    assert_eq!(record(&meta, &ana, live).await.revoke_reason, Some(RevokeReason::Password));

    // A database from a newer Craft Cloud is still refused.
    use sqlx::Connection;
    let mut conn = sqlx::AnyConnection::connect(url).await.unwrap();
    sqlx::query("INSERT INTO schema_version (version) VALUES (10)").execute(&mut conn).await.unwrap();
    drop(conn);
    assert!(matches!(Meta::open(url).await, Err(MetaError::Corrupt(_))));
}

#[tokio::test]
async fn migrating_from_0006_keeps_the_rows_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    migrating_from_0006_keeps_the_rows(&format!("sqlite:{}", dir.path().join("meta.db").display())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn migrating_from_0006_keeps_the_rows_on_postgres() {
    let Some((url, _guard)) = postgres_database().await else {
        eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set");
        return;
    };
    migrating_from_0006_keeps_the_rows(&url).await;
}

// ---- a failure in the middle of a transaction ----

/// A trigger that makes every delete from `pairings` fail, which is the step after the password and the devices
/// were already written in `change_password` and `reset_password`. Test only: the migrations carry no trigger.
async fn break_pairing_deletes(url: &str, postgres: bool) {
    use sqlx::Connection;
    let mut conn = sqlx::AnyConnection::connect(url).await.unwrap();
    let statements: &[&str] = if postgres {
        &[
            "CREATE FUNCTION cc_test_boom() RETURNS trigger AS $$ BEGIN RAISE EXCEPTION 'boom'; END $$ LANGUAGE plpgsql",
            "CREATE TRIGGER cc_test_boom BEFORE DELETE ON pairings FOR EACH STATEMENT EXECUTE FUNCTION cc_test_boom()",
        ]
    } else {
        &["CREATE TRIGGER cc_test_boom BEFORE DELETE ON pairings BEGIN SELECT RAISE(ABORT, 'boom'); END"]
    };
    for statement in statements {
        sqlx::query(sqlx::AssertSqlSafe((*statement).to_owned())).execute(&mut conn).await.unwrap();
    }
}

async fn mend_pairing_deletes(url: &str) {
    use sqlx::Connection;
    let mut conn = sqlx::AnyConnection::connect(url).await.unwrap();
    sqlx::query("DROP TRIGGER cc_test_boom ON pairings").execute(&mut conn).await.ok();
    sqlx::query("DROP TRIGGER cc_test_boom").execute(&mut conn).await.ok();
    sqlx::query("DROP FUNCTION cc_test_boom()").execute(&mut conn).await.ok();
}

/// A step that fails after others were written undoes them all: the hash, the time, the devices, the pairings and
/// the invite are as they were, for a password change and for an admin reset; once the step works, the same call
/// goes through.
async fn a_failure_in_the_middle_changes_nothing(url: &str, postgres: bool) {
    let meta = Meta::open(url).await.unwrap();
    let (ana, keep, _, pairing_id) = paired(&meta, "ana").await;
    let other = device(&meta, &ana).await;
    let other_pairing = meta.create_pairing(ana.id, other, &code(PAIRING_CHARS).hash(), NOW).await.unwrap();
    let invite = code(INVITE_CHARS);
    meta.create_invite("ana", &invite.hash(), NOW, INVITE_TTL_MS).await.unwrap();
    break_pairing_deletes(url, postgres).await;

    let invite_hash = invite.hash();
    let unchanged = || async {
        assert_eq!(meta.user_by_name("ana").await.unwrap().unwrap().password_hash, "$argon2id$stub");
        assert_eq!(meta.account(ana.id).await.unwrap().password_changed_ms, None);
        let devices = meta.list_devices(ana.id).await.unwrap();
        assert!(devices.iter().all(|d| !d.revoked && d.revoked_ms.is_none() && d.revoke_reason.is_none()), "{devices:?}");
        assert!(meta.pairing_status(ana.id, pairing_id, NOW + 1).await.unwrap().is_some());
        assert!(meta.pairing_status(ana.id, other_pairing, NOW + 1).await.unwrap().is_some());
        assert_eq!(meta.invite_needs_password(&invite_hash, NOW + 1).await.unwrap(), Some(false));
    };
    let failed = meta.change_password(ana.id, "$argon2id$stub", "$argon2id$new", keep, NOW + 2).await;
    assert!(matches!(failed, Err(MetaError::Db(_))), "{failed:?}");
    unchanged().await;
    let failed = meta.reset_password(ana.id, "$argon2id$new", false, NOW + 2).await;
    assert!(matches!(failed, Err(MetaError::Db(_))), "{failed:?}");
    unchanged().await;
    let failed = meta.revoke_other_devices(ana.id, keep, RevokeReason::Others, NOW + 2).await;
    assert!(matches!(failed, Err(MetaError::Db(_))), "{failed:?}");
    unchanged().await;

    mend_pairing_deletes(url).await;
    assert_eq!(meta.change_password(ana.id, "$argon2id$stub", "$argon2id$new", keep, NOW + 3).await.unwrap(), vec![other]);
}

#[tokio::test]
async fn a_failure_in_the_middle_changes_nothing_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    a_failure_in_the_middle_changes_nothing(&format!("sqlite:{}", dir.path().join("meta.db").display()), false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failure_in_the_middle_changes_nothing_on_postgres() {
    let Some((url, _guard)) = postgres_database().await else {
        eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set");
        return;
    };
    a_failure_in_the_middle_changes_nothing(&url, true).await;
}

// ---- the admin API (migration 0008) ----

const ADMIN_HASH: &str = "$argon2id$admin";

/// An admin account with a device, and the actor an admin request on its behalf would carry.
async fn admin_actor(meta: &Meta, username: &str) -> (UserRecord, AdminActor) {
    let u = meta.create_user(username, ADMIN_HASH, Role::Admin, NOW).await.unwrap();
    let device = device(meta, &u).await;
    let actor = AdminActor { user: u.id, device, password_hash: ADMIN_HASH.to_owned() };
    (u, actor)
}

/// The generation a sign-in would read for `username` now.
async fn gen_of(meta: &Meta, username: &str) -> i64 {
    meta.user_by_name(username).await.unwrap().unwrap().auth_gen
}

async fn admin_view(meta: &Meta, user: UserId) -> AdminUser {
    meta.admin_user(user, NOW).await.unwrap().unwrap()
}

async fn users_page(meta: &Meta, after: Option<&str>, search: Option<&str>, limit: u32) -> (Vec<String>, Option<String>) {
    let page = meta.admin_list_users(after, search, limit, NOW).await.unwrap();
    (page.users.into_iter().map(|u| u.username).collect(), page.next_after)
}

async fn the_admin_list_pages_searches_and_counts(meta: &Meta) {
    let (_, actor) = admin_actor(meta, "root").await;
    for name in ["ana", "bob", "a_b", "axb", "carol"] {
        user(meta, name).await;
    }
    // By username, in pages; the last page has no continuation.
    let (first, next) = users_page(meta, None, None, 3).await;
    assert_eq!(first.len(), 3);
    let (second, next2) = users_page(meta, next.as_deref(), None, 3).await;
    assert_eq!(second.len(), 3);
    assert_eq!(next2, None);
    let mut all: Vec<_> = first.into_iter().chain(second).collect();
    let mut sorted_all = all.clone();
    sorted_all.sort();
    assert_eq!(all, sorted_all, "pages follow one order");
    all.dedup();
    assert_eq!(all.len(), 6, "no account twice, none missing");
    assert_eq!(users_page(meta, None, None, 6).await.1, None, "exactly a full page has no continuation");
    // A search finds a substring, ignores case, and takes `%` and `_` literally.
    assert_eq!(users_page(meta, None, Some("A_B"), 50).await.0, vec!["a_b"]);
    assert_eq!(users_page(meta, None, Some("a"), 50).await.0, vec!["a_b", "ana", "axb", "carol"]);
    assert_eq!(users_page(meta, None, Some("%"), 50).await.0, Vec::<String>::new());
    assert_eq!(users_page(meta, None, Some("_"), 50).await.0, vec!["a_b"]);
    assert_eq!(users_page(meta, None, Some("zzz"), 50).await.0, Vec::<String>::new());
    // A search pages too.
    let (one, next) = users_page(meta, None, Some("a"), 1).await;
    assert_eq!(one, vec!["a_b"]);
    assert_eq!(users_page(meta, next.as_deref(), Some("a"), 2).await.0, vec!["ana", "axb"]);
    assert!(matches!(meta.admin_list_users(None, Some(&"x".repeat(65)), 10, NOW).await, Err(MetaError::Invalid(_))));
    meta.admin_list_users(None, Some(&"x".repeat(64)), 10, NOW).await.unwrap();

    // What a row says.
    let ana = meta.user_by_name("ana").await.unwrap().unwrap();
    let dev = device(meta, &ana).await;
    meta.add_chunk(ana.space, &random_chunk(), 70, NOW).await.unwrap();
    meta.create_invite("ana", &code(INVITE_CHARS).hash(), NOW, INVITE_TTL_MS).await.unwrap();
    let row = admin_view(meta, ana.id).await;
    assert_eq!((row.role, row.devices_active, row.stored_bytes, row.quota_bytes, row.invite_pending), (Role::Member, 1, 70, None, true));
    meta.revoke_device(ana.id, dev).await.unwrap();
    assert_eq!(admin_view(meta, ana.id).await.devices_active, 0);
    // An expired invite is not pending.
    assert!(!meta.admin_user(ana.id, NOW + INVITE_TTL_MS).await.unwrap().unwrap().invite_pending);
    // A reset code is not an invite.
    let bob = meta.user_by_name("bob").await.unwrap().unwrap();
    meta.admin_issue_reset(&actor, bob.id, &code(INVITE_CHARS).hash(), NOW, RESET_TTL_MS).await.unwrap();
    assert!(!admin_view(meta, bob.id).await.invite_pending);

    // The counters: accounts, live devices, bytes.
    let counts = meta.admin_server_counts().await.unwrap();
    assert_eq!((counts.users, counts.devices_active, counts.stored_bytes, counts.live_bytes), (6, 1, 70, 0), "only root's device is live");
    meta.admin_remove_user(&actor, ana.id, NOW).await.unwrap();
    assert_eq!(meta.admin_server_counts().await.unwrap().users, 5);
    assert!(!users_page(meta, None, None, 50).await.0.contains(&"ana".to_owned()), "a removed account is not listed");
    assert!(meta.admin_user(ana.id, NOW).await.unwrap().is_some(), "but it can still be looked up");
}

async fn an_admin_makes_an_account_with_a_role_and_an_invite(meta: &Meta) {
    let (_, actor) = admin_actor(meta, "root").await;
    let invite = code(INVITE_CHARS);
    let (made, issued) = meta.admin_create_user(&actor, "Newbie", Role::Admin, &invite.hash(), NOW, INVITE_TTL_MS).await.unwrap();
    assert_eq!((made.username.as_str(), made.role, made.devices_active, made.invite_pending), ("newbie", Role::Admin, 0, true));
    assert_eq!(issued.expires_ms, NOW + INVITE_TTL_MS);
    // A name that exists, in any case, any role, is taken, and nothing is changed.
    for name in ["newbie", "NEWBIE", "root"] {
        let again = meta.admin_create_user(&actor, name, Role::Member, &code(INVITE_CHARS).hash(), NOW, INVITE_TTL_MS).await;
        assert!(matches!(again, Err(MetaError::UsernameTaken)), "{name}: {again:?}");
    }
    assert!(matches!(
        meta.admin_create_user(&actor, "bad name", Role::Member, &code(INVITE_CHARS).hash(), NOW, INVITE_TTL_MS).await,
        Err(MetaError::Invalid(_))
    ));
    // It has no password: the invite asks for one, and sets it. The account keeps its role and has its own space.
    assert_eq!(meta.invite_needs_password(&invite.hash(), NOW + 1).await.unwrap(), Some(true));
    assert!(matches!(meta.redeem(&invite.hash(), "pc", &token(), None, NOW + 1).await, Err(MetaError::PasswordRequired)));
    let joined = meta.redeem(&invite.hash(), "pc", &token(), Some("$argon2id$chosen"), NOW + 2).await.unwrap();
    assert_eq!(joined.user, made.id);
    let record = meta.user_by_name("newbie").await.unwrap().unwrap();
    assert_eq!((record.role, record.password_hash.as_str()), (Role::Admin, "$argon2id$chosen"));
    assert!(meta.space_info(record.space).await.is_ok());
    assert!(!admin_view(meta, made.id).await.invite_pending);
}

async fn an_admin_changes_a_role_and_a_quota(meta: &Meta) {
    let (root, actor) = admin_actor(meta, "root").await;
    let ana = user(meta, "ana").await;
    // Nothing to change is invalid.
    assert!(matches!(meta.admin_update_user(&actor, ana.id, None, None, NOW).await, Err(MetaError::Invalid(_))));
    let promoted = meta.admin_update_user(&actor, ana.id, Some(Role::Admin), None, NOW).await.unwrap();
    assert_eq!((promoted.role, promoted.quota_bytes), (Role::Admin, None));
    // Role and quota together; a missing quota leaves it, `Some(None)` removes it.
    let both = meta.admin_update_user(&actor, ana.id, Some(Role::Member), Some(Some(MIN_QUOTA_BYTES)), NOW).await.unwrap();
    assert_eq!((both.role, both.quota_bytes), (Role::Member, Some(MIN_QUOTA_BYTES)));
    let role_only = meta.admin_update_user(&actor, ana.id, Some(Role::Member), None, NOW).await.unwrap();
    assert_eq!(role_only.quota_bytes, Some(MIN_QUOTA_BYTES), "a missing quota leaves it");
    let cleared = meta.admin_update_user(&actor, ana.id, None, Some(None), NOW).await.unwrap();
    assert_eq!(cleared.quota_bytes, None);
    // Bounds: a chunk up to i64::MAX.
    for bad in [0, 1, MIN_QUOTA_BYTES - 1, i64::MAX as u64 + 1, u64::MAX] {
        let result = meta.admin_update_user(&actor, ana.id, None, Some(Some(bad)), NOW).await;
        assert!(matches!(result, Err(MetaError::Invalid(_))), "{bad}: {result:?}");
    }
    for good in [MIN_QUOTA_BYTES, i64::MAX as u64] {
        assert_eq!(meta.admin_update_user(&actor, ana.id, None, Some(Some(good)), NOW).await.unwrap().quota_bytes, Some(good));
    }
    // Unknown users, and accounts marked for removal.
    assert!(matches!(meta.admin_update_user(&actor, UserId(uuid::Uuid::now_v7()), Some(Role::Admin), None, NOW).await, Err(MetaError::NotFound(_))));
    // The last administrator can't be demoted, even by themselves; with another one they can.
    let result = meta.admin_update_user(&actor, root.id, Some(Role::Member), None, NOW).await;
    assert!(matches!(result, Err(MetaError::LastAdmin)), "{result:?}");
    assert_eq!(admin_view(meta, root.id).await.role, Role::Admin, "and nothing was changed");
    meta.admin_update_user(&actor, ana.id, Some(Role::Admin), None, NOW).await.unwrap();
    meta.admin_update_user(&actor, root.id, Some(Role::Member), None, NOW).await.unwrap();
    // The demoted admin's next change is refused: they are no admin any more.
    let result = meta.admin_update_user(&actor, ana.id, Some(Role::Member), None, NOW).await;
    assert!(matches!(result, Err(MetaError::Forbidden(_))), "{result:?}");
    let ana_actor = AdminActor { user: ana.id, device: device(meta, &ana).await, password_hash: "$argon2id$stub".to_owned() };
    meta.admin_remove_user(&ana_actor, root.id, NOW).await.unwrap();
    let result = meta.admin_update_user(&ana_actor, root.id, Some(Role::Admin), None, NOW).await;
    assert!(matches!(result, Err(MetaError::Conflict(_))), "{result:?}");
}

/// The acting admin's state is read again inside each change (the role and the password check the handler did can be
/// stale by then): a demotion, a sign-out, a password change or a disable since lets nothing through, and nothing is
/// changed.
async fn the_acting_admins_state_is_checked_in_the_transaction(meta: &Meta) {
    let (_, boss) = admin_actor(meta, "boss").await;
    let victim = user(meta, "victim").await;
    let issue = |actor: AdminActor| {
        let meta = meta.clone();
        async move {
            meta.admin_create_user(
                &actor,
                &format!("made{}", uuid::Uuid::now_v7().simple()),
                Role::Member,
                &code(INVITE_CHARS).hash(),
                NOW,
                INVITE_TTL_MS,
            )
            .await
            .map(|_| ())
        }
    };
    let users_before = meta.admin_server_counts().await.unwrap().users;

    // A member (a demoted admin) is forbidden.
    let (demoted, demoted_actor) = admin_actor(meta, "demoted").await;
    meta.admin_update_user(&boss, demoted.id, Some(Role::Member), None, NOW).await.unwrap();
    assert!(matches!(issue(demoted_actor).await, Err(MetaError::Forbidden(_))));
    // A signed-out device is unauthorized.
    let (signed_out, signed_out_actor) = admin_actor(meta, "signedout").await;
    meta.revoke_device(signed_out.id, signed_out_actor.device).await.unwrap();
    assert!(matches!(issue(signed_out_actor).await, Err(MetaError::DeviceRevoked)));
    // A password changed since it was checked is a conflict.
    let (changed, changed_actor) = admin_actor(meta, "changed").await;
    meta.reset_password(changed.id, "$argon2id$other", true, NOW).await.unwrap();
    assert!(matches!(issue(changed_actor).await, Err(MetaError::Conflict(_))));
    // A disabled admin is unauthorized (their devices are revoked, and the check says it even for a device kept).
    let (off, off_actor) = admin_actor(meta, "off").await;
    meta.admin_disable_user(&boss, off.id, NOW).await.unwrap();
    assert!(matches!(issue(off_actor).await, Err(MetaError::DeviceRevoked)));
    // Another account's device is not the actor's.
    let borrowed = AdminActor { device: device(meta, &victim).await, ..boss.clone() };
    assert!(matches!(issue(borrowed).await, Err(MetaError::DeviceRevoked)));
    // A member who is no admin at all.
    let member_actor = AdminActor { user: victim.id, device: device(meta, &victim).await, password_hash: "$argon2id$stub".to_owned() };
    assert!(matches!(meta.admin_disable_user(&member_actor, demoted.id, NOW).await, Err(MetaError::Forbidden(_))));
    assert_eq!(meta.admin_server_counts().await.unwrap().users, users_before + 4, "only the four admins made here, no account from a refused change");
    // Every kind of change checks, not only the first: one of each against a stale actor.
    let (stale_user, stale_actor) = admin_actor(meta, "stale").await;
    meta.admin_update_user(&boss, stale_user.id, Some(Role::Member), None, NOW).await.unwrap();
    assert!(matches!(meta.admin_update_user(&stale_actor, victim.id, Some(Role::Admin), None, NOW).await, Err(MetaError::Forbidden(_))));
    assert!(matches!(meta.admin_disable_user(&stale_actor, victim.id, NOW).await, Err(MetaError::Forbidden(_))));
    assert!(matches!(meta.admin_enable_user(&stale_actor, victim.id, NOW).await, Err(MetaError::Forbidden(_))));
    assert!(matches!(meta.admin_remove_user(&stale_actor, victim.id, NOW).await, Err(MetaError::Forbidden(_))));
    let hash = code(INVITE_CHARS).hash();
    assert!(matches!(meta.admin_issue_invite(&stale_actor, victim.id, &hash, NOW, INVITE_TTL_MS).await, Err(MetaError::Forbidden(_))));
    assert!(matches!(meta.admin_revoke_invite(&stale_actor, victim.id, NOW).await, Err(MetaError::Forbidden(_))));
    assert!(matches!(meta.admin_issue_reset(&stale_actor, victim.id, &hash, NOW, RESET_TTL_MS).await, Err(MetaError::Forbidden(_))));
    let victims_device = device(meta, &victim).await;
    assert!(matches!(meta.admin_revoke_device(&stale_actor, victim.id, victims_device, NOW).await, Err(MetaError::Forbidden(_))));
    assert!(meta.device_active(victims_device).await.unwrap());
    assert_eq!(admin_view(meta, victim.id).await.role, Role::Member);
}

/// A disabled account has nothing: its devices are signed out (with the reason), its codes gone, and no token, sign-in,
/// join or pairing works; enabling it brings back the account but never a device.
async fn disabling_signs_everything_out_and_enabling_brings_back_nothing(meta: &Meta) {
    let (root, actor) = admin_actor(meta, "root").await;
    let (ana, maker, pairing, pairing_id) = paired(meta, "ana").await;
    let other = device(meta, &ana).await;
    let other_token = token();
    let third = meta.create_device(ana.id, "third", &other_token, NOW).await.unwrap();
    let invite = code(INVITE_CHARS);
    meta.create_invite("ana", &invite.hash(), NOW, INVITE_TTL_MS).await.unwrap();

    // Not oneself.
    assert!(matches!(meta.admin_disable_user(&actor, root.id, NOW).await, Err(MetaError::Invalid(_))));
    assert!(matches!(meta.admin_remove_user(&actor, root.id, NOW).await, Err(MetaError::Invalid(_))));
    let revoked = meta.admin_disable_user(&actor, ana.id, NOW + 1).await.unwrap();
    assert_eq!(sorted(revoked), sorted(vec![maker, other, third]));
    let view = admin_view(meta, ana.id).await;
    assert_eq!((view.disabled_ms, view.devices_active, view.invite_pending), (Some(NOW + 1), 0, false));
    assert_eq!(meta.device_by_token(&other_token).await.unwrap(), None);
    for d in meta.list_devices(ana.id).await.unwrap() {
        assert_eq!((d.revoked, d.revoked_ms, d.revoke_reason), (true, Some(NOW + 1), Some(RevokeReason::Disabled)));
    }
    assert_eq!(meta.pairing_status(ana.id, pairing_id, NOW + 2).await.unwrap(), None);
    // Disabling again is harmless.
    assert!(meta.admin_disable_user(&actor, ana.id, NOW + 5).await.unwrap().is_empty());
    assert_eq!(admin_view(meta, ana.id).await.disabled_ms, Some(NOW + 1), "the first time stays");

    // Nothing gets in: a sign-in that checked the right password, a device, a code the command line makes, a pairing.
    let sign_in = meta.sign_in_device(ana.id, ("$argon2id$stub", gen_of(meta, "ana").await), "pc", &DeviceClient::default(), &token(), NOW + 3).await;
    assert!(matches!(sign_in, Err(MetaError::AccountDisabled)), "{sign_in:?}");
    assert!(matches!(meta.create_device(ana.id, "pc", &token(), NOW + 3).await, Err(MetaError::AccountDisabled)));
    let late_invite = code(INVITE_CHARS);
    meta.create_invite("ana", &late_invite.hash(), NOW + 3, INVITE_TTL_MS).await.unwrap();
    assert!(matches!(redeem(meta, &late_invite, "pc", NOW + 4).await, Err(MetaError::AccountDisabled)));
    assert!(matches!(redeem(meta, &pairing, "pc", NOW + 4).await, Err(MetaError::InvalidCode)), "its pairing is gone");
    assert!(meta.create_pairing(ana.id, maker, &code(PAIRING_CHARS).hash(), NOW + 4).await.is_err());
    assert_eq!(meta.list_devices(ana.id).await.unwrap().len(), 3, "no device was made");

    // Enabling: sign-in works again; what was signed out stays signed out.
    meta.admin_enable_user(&actor, ana.id, NOW).await.unwrap();
    assert_eq!(admin_view(meta, ana.id).await.disabled_ms, None);
    assert!(meta.list_devices(ana.id).await.unwrap().iter().all(|d| d.revoked));
    assert_eq!(meta.device_by_token(&other_token).await.unwrap(), None);
    assert!(
        meta.sign_in_device(ana.id, ("$argon2id$stub", gen_of(meta, "ana").await), "pc", &DeviceClient::default(), &token(), NOW + 6)
            .await
            .unwrap()
            .is_some()
    );
    // The code made while the account was disabled works now, as an invite made for an enabled account would.
    redeem(meta, &late_invite, "pc2", NOW + 7).await.unwrap();

    // Marked for removal: disabled, not listed, signed out, can't be enabled, and the name stays taken.
    let (bob, bobs, _, _) = paired(meta, "bob").await;
    assert_eq!(meta.admin_remove_user(&actor, bob.id, NOW + 8).await.unwrap(), vec![bobs]);
    let removed = admin_view(meta, bob.id).await;
    assert_eq!((removed.disabled_ms, removed.devices_active), (Some(NOW + 8), 0));
    assert!(matches!(meta.admin_enable_user(&actor, bob.id, NOW).await, Err(MetaError::Conflict(_))));
    assert!(matches!(meta.create_user("bob", "x", Role::Member, NOW).await, Err(MetaError::UsernameTaken)));
    assert!(matches!(
        meta.admin_create_user(&actor, "bob", Role::Member, &code(INVITE_CHARS).hash(), NOW, INVITE_TTL_MS).await,
        Err(MetaError::UsernameTaken)
    ));
    assert!(matches!(meta.admin_issue_reset(&actor, bob.id, &code(INVITE_CHARS).hash(), NOW, RESET_TTL_MS).await, Err(MetaError::Conflict(_))));
    let sign_in = meta.sign_in_device(bob.id, ("$argon2id$stub", gen_of(meta, "bob").await), "pc", &DeviceClient::default(), &token(), NOW + 9).await;
    assert!(matches!(sign_in, Err(MetaError::AccountDisabled)), "{sign_in:?}");
    assert!(meta.admin_remove_user(&actor, bob.id, NOW + 10).await.unwrap().is_empty(), "removing again is harmless");
    assert_eq!(meta.admin_server_counts().await.unwrap().users, 2);
}

/// A sign-in, a join with an invite, a join with a pairing, a new pairing and a plain device, all at the moment an
/// admin disables the account: each either finishes before and is signed out by the disable, or finds the account
/// disabled. Either way no device that is signed in is left, and enabling afterwards brings none back.
async fn issuing_access_while_an_account_is_disabled_leaves_no_usable_token(meta: &Meta) {
    let (_, actor) = admin_actor(meta, "root").await;
    for round in 0..12 {
        let name = format!("ana{round}");
        let (ana, maker, pairing, _) = paired(meta, &name).await;
        let invite = code(INVITE_CHARS);
        meta.create_invite(&name, &invite.hash(), NOW, INVITE_TTL_MS).await.unwrap();
        let mut tasks = Vec::new();
        for job in 0..6 {
            let (meta, actor, invite, pairing) = (meta.clone(), actor.clone(), invite.hash(), pairing.hash());
            tasks.push(tokio::spawn(async move {
                match job {
                    0 => meta.redeem(&invite, "invited", &token(), None, NOW + 1).await.map(|_| ()),
                    1 => meta.redeem(&pairing, "paired", &token(), None, NOW + 1).await.map(|_| ()),
                    2 => {
                        meta.sign_in_device(ana.id, ("$argon2id$stub", 0), "signed in", &DeviceClient::default(), &token(), NOW + 1).await.map(|_| ())
                    }
                    3 => meta.create_pairing(ana.id, maker, &code(PAIRING_CHARS).hash(), NOW + 1).await.map(|_| ()),
                    4 => meta.create_device(ana.id, "plain", &token(), NOW + 1).await.map(|_| ()),
                    _ => meta.admin_disable_user(&actor, ana.id, NOW + 2).await.map(|_| ()),
                }
            }));
        }
        for task in tasks {
            let result = task.await.unwrap();
            assert!(
                matches!(
                    result,
                    Ok(()) | Err(MetaError::InvalidCode | MetaError::NotFound(_) | MetaError::AccountDisabled | MetaError::DeviceRevoked)
                ),
                "round {round}: {result:?}"
            );
        }
        meta.admin_enable_user(&actor, ana.id, NOW).await.unwrap();
        let devices = meta.list_devices(ana.id).await.unwrap();
        assert!(devices.iter().all(|d| d.revoked), "round {round}: {devices:?}");
        assert_revoke_columns_agree(&devices);
    }
}

/// A sign-in reads the hash and generation, verifies the password (slowly), and asks to create the device. A disable and
/// an enable in between leave the hash as it was but not the generation: the device is not created. So are a password
/// change and a removal. The interleaving is played out by calling the API in that order.
async fn a_sign_in_checked_before_a_disable_and_an_enable_creates_no_device(meta: &Meta) {
    let (_, actor) = admin_actor(meta, "root").await;
    let ana = user(meta, "ana").await;
    let bob = user(meta, "bob").await;
    let client = DeviceClient::default();
    for (name, id) in [("ana", ana.id), ("bob", bob.id)] {
        let read = meta.user_by_name(name).await.unwrap().unwrap();
        if name == "ana" {
            meta.admin_disable_user(&actor, id, NOW).await.unwrap();
            meta.admin_enable_user(&actor, id, NOW).await.unwrap();
        } else {
            meta.admin_remove_user(&actor, id, NOW).await.unwrap();
        }
        let late = meta.sign_in_device(id, (&read.password_hash, read.auth_gen), "pc", &client, &token(), NOW + 1).await;
        match name {
            "ana" => assert!(matches!(late, Ok(None)), "{late:?}"),
            _ => assert!(matches!(late, Ok(None) | Err(MetaError::AccountDisabled)), "{late:?}"),
        }
        assert!(meta.list_devices(id).await.unwrap().is_empty(), "{name}: no device was made");
    }
    // A sign-in that read the state after the enable goes through.
    let fresh = meta.user_by_name("ana").await.unwrap().unwrap();
    assert!(fresh.auth_gen >= 2);
    assert!(meta.sign_in_device(ana.id, (&fresh.password_hash, fresh.auth_gen), "pc", &client, &token(), NOW + 2).await.unwrap().is_some());
    // A password change moves the generation too.
    let before = gen_of(meta, "ana").await;
    meta.reset_password(ana.id, "$argon2id$new", true, NOW + 3).await.unwrap();
    assert!(gen_of(meta, "ana").await > before);
}

async fn invites_and_reset_codes_replace_each_other(meta: &Meta) {
    let (root, actor) = admin_actor(meta, "root").await;
    let (fresh, _) = meta.admin_create_user(&actor, "fresh", Role::Member, &code(INVITE_CHARS).hash(), NOW, INVITE_TTL_MS).await.unwrap();
    let ana = user(meta, "ana").await;
    let (first, second, reset) = (code(INVITE_CHARS), code(INVITE_CHARS), code(INVITE_CHARS));

    // An invite only for an account without a password; it replaces the open one, and can be revoked.
    meta.admin_issue_invite(&actor, fresh.id, &first.hash(), NOW, INVITE_TTL_MS).await.unwrap();
    meta.admin_issue_invite(&actor, fresh.id, &second.hash(), NOW + 1, INVITE_TTL_MS).await.unwrap();
    assert_eq!(meta.invite_needs_password(&first.hash(), NOW + 2).await.unwrap(), None);
    assert_eq!(meta.invite_needs_password(&second.hash(), NOW + 2).await.unwrap(), Some(true));
    let conflict = meta.admin_issue_invite(&actor, ana.id, &code(INVITE_CHARS).hash(), NOW, INVITE_TTL_MS).await;
    assert!(matches!(conflict, Err(MetaError::Conflict(_))), "an account with a password gets none: {conflict:?}");
    assert_eq!(meta.invite_needs_password(&second.hash(), NOW + 2).await.unwrap(), Some(true));
    meta.admin_revoke_invite(&actor, fresh.id, NOW).await.unwrap();
    assert_eq!(meta.invite_needs_password(&second.hash(), NOW + 2).await.unwrap(), None);
    assert!(matches!(meta.admin_revoke_invite(&actor, fresh.id, NOW).await, Err(MetaError::NotFound(_))), "nothing left to revoke");
    assert!(matches!(meta.admin_revoke_invite(&actor, UserId(uuid::Uuid::now_v7()), NOW).await, Err(MetaError::NotFound(_))));

    // A reset code replaces an invite and the other way round, and the account has one row either way.
    let invite = code(INVITE_CHARS);
    meta.create_invite("ana", &invite.hash(), NOW, INVITE_TTL_MS).await.unwrap();
    meta.admin_issue_reset(&actor, ana.id, &reset.hash(), NOW + 1, RESET_TTL_MS).await.unwrap();
    assert_eq!(meta.invite_needs_password(&invite.hash(), NOW + 2).await.unwrap(), None);
    assert_eq!(meta.invite_needs_password(&reset.hash(), NOW + 2).await.unwrap(), Some(true), "a reset always needs a password, though ana has one");
    let invite_again = code(INVITE_CHARS);
    meta.create_invite("ana", &invite_again.hash(), NOW + 3, INVITE_TTL_MS).await.unwrap();
    assert_eq!(meta.invite_needs_password(&reset.hash(), NOW + 4).await.unwrap(), None, "the command line's invite replaces a reset code");
    assert_eq!(meta.invite_needs_password(&invite_again.hash(), NOW + 4).await.unwrap(), Some(false));
    meta.admin_issue_reset(&actor, ana.id, &reset.hash(), NOW + 5, RESET_TTL_MS).await.unwrap();
    meta.admin_revoke_invite(&actor, ana.id, NOW).await.unwrap();
    assert_eq!(meta.invite_needs_password(&reset.hash(), NOW + 6).await.unwrap(), None, "a revoke ends a reset code too");

    // Not oneself, and not another admin.
    assert!(matches!(meta.admin_issue_reset(&actor, root.id, &reset.hash(), NOW, RESET_TTL_MS).await, Err(MetaError::Invalid(_))));
    let (boss, _) = admin_actor(meta, "boss").await;
    let result = meta.admin_issue_reset(&actor, boss.id, &reset.hash(), NOW, RESET_TTL_MS).await;
    assert!(matches!(result, Err(MetaError::Forbidden(_))), "{result:?}");
    assert_eq!(meta.invite_needs_password(&reset.hash(), NOW + 6).await.unwrap(), None);
    // Not a disabled account.
    meta.admin_disable_user(&actor, ana.id, NOW).await.unwrap();
    assert!(matches!(meta.admin_issue_reset(&actor, ana.id, &reset.hash(), NOW, RESET_TTL_MS).await, Err(MetaError::Conflict(_))));
    assert!(matches!(
        meta.admin_issue_reset(&actor, UserId(uuid::Uuid::now_v7()), &reset.hash(), NOW, RESET_TTL_MS).await,
        Err(MetaError::NotFound(_))
    ));
}

async fn a_reset_code_sets_a_password_and_signs_everything_out(meta: &Meta) {
    let (_, actor) = admin_actor(meta, "root").await;
    let (ana, maker, pairing, pairing_id) = paired(meta, "ana").await;
    let other = device(meta, &ana).await;
    let reset = code(INVITE_CHARS);
    meta.admin_issue_reset(&actor, ana.id, &reset.hash(), NOW, RESET_TTL_MS).await.unwrap();
    // Nothing changes until it is redeemed.
    assert!(meta.device_active(maker).await.unwrap());
    assert_eq!(meta.user_by_name("ana").await.unwrap().unwrap().password_hash, "$argon2id$stub");

    // A reset without a new password is refused, and the code stays.
    assert!(matches!(meta.redeem(&reset.hash(), "pc", &token(), None, NOW + 1).await, Err(MetaError::PasswordRequired)));
    assert!(matches!(meta.redeem(&reset.hash(), "pc", &token(), Some(""), NOW + 1).await, Err(MetaError::PasswordRequired)));
    assert!(meta.device_active(maker).await.unwrap());
    // With one: the password changes (though the account had one), every device is signed out, the pairings are gone,
    // and the redeeming computer is the one device signed in.
    let new_token = token();
    let joined = meta.redeem(&reset.hash(), "recovery pc", &new_token, Some("$argon2id$fresh"), NOW + 2).await.unwrap();
    assert_eq!(sorted(joined.revoked.clone()), sorted(vec![maker, other]));
    assert_eq!(meta.user_by_name("ana").await.unwrap().unwrap().password_hash, "$argon2id$fresh");
    assert_eq!(meta.account(ana.id).await.unwrap().password_changed_ms, Some(NOW + 2));
    let devices = meta.list_devices(ana.id).await.unwrap();
    for d in &devices {
        if d.id == joined.device {
            assert!(!d.revoked);
        } else {
            assert_eq!((d.revoked, d.revoke_reason), (true, Some(RevokeReason::Reset)));
        }
    }
    assert_revoke_columns_agree(&devices);
    assert_eq!(meta.device_by_token(&new_token).await.unwrap().map(|a| a.device), Some(joined.device));
    assert_eq!(meta.pairing_status(ana.id, pairing_id, NOW + 3).await.unwrap(), None);
    assert!(matches!(redeem(meta, &pairing, "late", NOW + 3).await, Err(MetaError::InvalidCode)));
    // Once only.
    assert!(matches!(meta.redeem(&reset.hash(), "again", &token(), Some("$argon2id$x"), NOW + 4).await, Err(MetaError::InvalidCode)));

    // It lasts an hour.
    let late = code(INVITE_CHARS);
    meta.admin_issue_reset(&actor, ana.id, &late.hash(), NOW + 10, RESET_TTL_MS).await.unwrap();
    let result = meta.redeem(&late.hash(), "late", &token(), Some("$argon2id$x"), NOW + 10 + RESET_TTL_MS).await;
    assert!(matches!(result, Err(MetaError::InvalidCode)), "{result:?}");
    assert_eq!(RESET_TTL_MS, 3_600_000);
    meta.redeem(&late.hash(), "in time", &token(), Some("$argon2id$x"), NOW + 10 + RESET_TTL_MS - 1).await.unwrap();

    // Disabling the account deletes the reset code it still held.
    let disabled_code = code(INVITE_CHARS);
    meta.admin_issue_reset(&actor, ana.id, &disabled_code.hash(), NOW + 20, RESET_TTL_MS).await.unwrap();
    meta.admin_disable_user(&actor, ana.id, NOW + 21).await.unwrap();
    assert_eq!(meta.invite_needs_password(&disabled_code.hash(), NOW + 22).await.unwrap(), None, "disabling deletes it");
}

/// The quota of a space is its limit when set, with or without the server-wide cap, in the look before the bytes are
/// stored and in the count that follows.
async fn a_quota_overrides_the_server_wide_cap(meta: &Meta) {
    let (_, actor) = admin_actor(meta, "root").await;
    let ana = user(meta, "ana").await;
    let bob = user(meta, "bob").await;
    let quota = MIN_QUOTA_BYTES;
    meta.admin_update_user(&actor, ana.id, None, Some(Some(quota)), NOW).await.unwrap();
    let (c1, c2, c3) = (random_chunk(), random_chunk(), random_chunk());

    // No cap at all: the quota still holds, and a space without one is unlimited.
    meta.check_room(ana.space, BlobKind::Chunk, c1.as_bytes(), quota).await.unwrap();
    let over = meta.check_room(ana.space, BlobKind::Chunk, c1.as_bytes(), quota + 1).await;
    assert!(matches!(over, Err(MetaError::QuotaExceeded { used: 0, limit }) if limit == quota), "{over:?}");
    meta.check_room(bob.space, BlobKind::Chunk, c1.as_bytes(), quota * 10).await.unwrap();
    meta.add_chunk(ana.space, &c1, quota - 10, NOW).await.unwrap();
    let over = meta.add_chunk(ana.space, &c2, 11, NOW).await;
    assert!(matches!(over, Err(MetaError::QuotaExceeded { limit, .. }) if limit == quota), "{over:?}");
    assert_eq!(meta.stored_bytes(ana.space).await.unwrap(), quota - 10, "nothing was recorded");
    assert!(matches!(meta.add_chunks(ana.space, &[(c2, 6), (c3, 6)], NOW).await, Err(MetaError::QuotaExceeded { .. })));
    meta.add_chunks(ana.space, &[(c2, 5), (c3, 5)], NOW).await.unwrap();
    assert_eq!(meta.stored_bytes(ana.space).await.unwrap(), quota);
    meta.add_chunk(bob.space, &c1, quota * 3, NOW).await.unwrap();
    // What the space has already costs nothing to look at.
    meta.check_room(ana.space, BlobKind::Chunk, c1.as_bytes(), quota * 5).await.unwrap();

    // A cap larger than the quota: the quota wins. A cap smaller than the quota: the quota wins, too (it is the
    // account's limit, whichever way it differs).
    let big_cap = meta.clone().with_stored_bytes_cap(Some(quota * 100));
    assert!(matches!(big_cap.check_room(ana.space, BlobKind::Chunk, random_chunk().as_bytes(), 1).await, Err(MetaError::QuotaExceeded { .. })));
    assert!(matches!(big_cap.add_chunk(ana.space, &random_chunk(), 1, NOW).await, Err(MetaError::QuotaExceeded { .. })));
    let small_cap = meta.clone().with_stored_bytes_cap(Some(100));
    meta.admin_update_user(&actor, bob.id, None, Some(Some(quota * 10)), NOW).await.unwrap();
    small_cap.check_room(bob.space, BlobKind::Chunk, random_chunk().as_bytes(), quota).await.unwrap();
    small_cap.add_chunk(bob.space, &random_chunk(), quota, NOW).await.unwrap();
    // Removing the quota gives the cap back; a quota under the use deletes nothing and refuses new bytes.
    meta.admin_update_user(&actor, ana.id, None, Some(None), NOW).await.unwrap();
    big_cap.add_chunk(ana.space, &random_chunk(), 1, NOW).await.unwrap();
    assert!(matches!(small_cap.add_chunk(ana.space, &random_chunk(), 1, NOW).await, Err(MetaError::QuotaExceeded { .. })));
    meta.admin_update_user(&actor, bob.id, None, Some(Some(MIN_QUOTA_BYTES)), NOW).await.unwrap();
    assert!(meta.stored_bytes(bob.space).await.unwrap() > MIN_QUOTA_BYTES);
    assert!(matches!(meta.add_chunk(bob.space, &random_chunk(), 1, NOW).await, Err(MetaError::QuotaExceeded { .. })));
}

async fn an_admin_lists_and_revokes_a_users_devices(meta: &Meta) {
    let (_, actor) = admin_actor(meta, "root").await;
    let (ana, maker, _, pairing_id) = paired(meta, "ana").await;
    let bob = user(meta, "bob").await;
    let bobs = device(meta, &bob).await;
    let other = device(meta, &ana).await;
    meta.revoke_device(ana.id, other).await.unwrap();
    // Every device, revoked ones with their reason.
    let listed = meta.admin_devices(ana.id).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed.iter().find(|d| d.id == other).unwrap().revoke_reason, Some(RevokeReason::User));
    assert!(matches!(meta.admin_devices(UserId(uuid::Uuid::now_v7())).await, Err(MetaError::NotFound(_))));

    meta.admin_revoke_device(&actor, ana.id, maker, NOW + 1).await.unwrap();
    let revoked = record(meta, &ana, maker).await;
    assert_eq!((revoked.revoked, revoked.revoked_ms, revoked.revoke_reason), (true, Some(NOW + 1), Some(RevokeReason::Admin)));
    assert_eq!(meta.pairing_status(ana.id, pairing_id, NOW + 2).await.unwrap(), None, "its pairing dies with it");
    // Again: the first time and reason stay. Another user's device, an unknown device or user: not found, nothing changed.
    meta.admin_revoke_device(&actor, ana.id, maker, NOW + 5).await.unwrap();
    assert_eq!(record(meta, &ana, maker).await.revoked_ms, Some(NOW + 1));
    assert_eq!(record(meta, &ana, other).await.revoke_reason, Some(RevokeReason::User));
    assert!(matches!(meta.admin_revoke_device(&actor, ana.id, bobs, NOW).await, Err(MetaError::NotFound(_))));
    assert!(matches!(meta.admin_revoke_device(&actor, ana.id, DeviceId::generate(), NOW).await, Err(MetaError::NotFound(_))));
    assert!(matches!(meta.admin_revoke_device(&actor, UserId(uuid::Uuid::now_v7()), bobs, NOW).await, Err(MetaError::NotFound(_))));
    assert!(meta.device_active(bobs).await.unwrap());
}

// Two admins at the same moment, each taking the other (or oneself) away: whatever the order, one enabled admin is
// left, and no statement fails inside the database (on Postgres, rows taken in different orders would deadlock).

#[derive(Clone, Copy, Debug)]
enum Act {
    Demote(usize),
    Disable(usize),
    Remove(usize),
}

/// Runs `acts` (who acts, what) at the same moment on a survivor and a new admin; exactly one enabled admin must be
/// left, and the second change of two must fail with a reason the rules give. Returns the admin left.
async fn race_two_admins(meta: &Meta, round: usize, survivor: (UserRecord, AdminActor), acts: [(usize, Act); 2]) -> (UserRecord, AdminActor) {
    let fresh = admin_actor(meta, &format!("fresh{round}")).await;
    let both = [survivor, fresh];
    let mut tasks = Vec::new();
    for (who, act) in acts {
        let (meta, actor) = (meta.clone(), both[who].1.clone());
        let target = both[match act {
            Act::Demote(t) | Act::Disable(t) | Act::Remove(t) => t,
        }]
        .0
        .id;
        tasks.push(tokio::spawn(async move {
            match act {
                Act::Demote(_) => meta.admin_update_user(&actor, target, Some(Role::Member), None, NOW).await.map(|_| ()),
                Act::Disable(_) => meta.admin_disable_user(&actor, target, NOW).await.map(|_| ()),
                Act::Remove(_) => meta.admin_remove_user(&actor, target, NOW).await.map(|_| ()),
            }
        }));
    }
    for (task, (_, act)) in tasks.into_iter().zip(acts) {
        let result = task.await.unwrap();
        assert!(
            matches!(result, Ok(()) | Err(MetaError::LastAdmin | MetaError::Forbidden(_) | MetaError::DeviceRevoked | MetaError::Invalid(_))),
            "round {round}, {act:?}: {result:?}"
        );
    }
    let page = meta.admin_list_users(None, None, 200, NOW).await.unwrap();
    let left: Vec<_> = page.users.iter().filter(|u| u.role == Role::Admin && u.disabled_ms.is_none()).collect();
    assert_eq!(left.len(), 1, "round {round}, {acts:?}: {left:?}");
    let left = left[0].id;
    let [a, b] = both;
    if a.0.id == left { a } else { b }
}

async fn two_admins_racing_keep_one(meta: &Meta, combos: &[[(usize, Act); 2]]) {
    let mut survivor = admin_actor(meta, "first").await;
    for round in 0..8 {
        for (i, combo) in combos.iter().enumerate() {
            survivor = race_two_admins(meta, round * 10 + i, survivor, *combo).await;
        }
    }
}

async fn two_demotions_at_once_keep_an_admin(meta: &Meta) {
    two_admins_racing_keep_one(meta, &[[(0, Act::Demote(1)), (1, Act::Demote(0))]]).await;
}

/// Each admin steps down at the same moment: their rows are different, so only the `last_admin` lock keeps one of them.
async fn two_self_demotions_at_once_keep_an_admin(meta: &Meta) {
    two_admins_racing_keep_one(meta, &[[(0, Act::Demote(0)), (1, Act::Demote(1))]]).await;
}

async fn two_disables_at_once_keep_an_admin(meta: &Meta) {
    two_admins_racing_keep_one(meta, &[[(0, Act::Disable(1)), (1, Act::Disable(0))]]).await;
}

async fn a_demotion_and_a_disable_at_once_keep_an_admin(meta: &Meta) {
    two_admins_racing_keep_one(meta, &[[(0, Act::Demote(1)), (1, Act::Disable(0))], [(0, Act::Disable(1)), (1, Act::Demote(0))]]).await;
}

async fn a_removal_and_a_demotion_at_once_keep_an_admin(meta: &Meta) {
    two_admins_racing_keep_one(meta, &[[(0, Act::Remove(1)), (1, Act::Demote(0))], [(0, Act::Demote(1)), (1, Act::Remove(0))]]).await;
}

async fn a_self_demotion_and_a_disable_of_the_other_at_once_keep_an_admin(meta: &Meta) {
    two_admins_racing_keep_one(meta, &[[(0, Act::Demote(0)), (0, Act::Disable(1))], [(1, Act::Demote(1)), (1, Act::Disable(0))]]).await;
}

// ---- the migration from 0007 to 0008 ----

/// Drops what migration 0008 added and forgets it ran, which leaves the database as 0007 left it (with its rows).
async fn back_to_0007(url: &str) {
    use sqlx::Connection;
    let mut conn = sqlx::AnyConnection::connect(url).await.unwrap();
    // The audit log (migration 0009) goes first: it runs again with 0008 on the next open.
    sqlx::query("DROP TABLE audit_events").execute(&mut conn).await.unwrap();
    sqlx::query("ALTER TABLE invites DROP COLUMN purpose").execute(&mut conn).await.unwrap();
    sqlx::query("ALTER TABLE spaces DROP COLUMN quota_bytes").execute(&mut conn).await.unwrap();
    sqlx::query("ALTER TABLE users DROP COLUMN removed_ms").execute(&mut conn).await.unwrap();
    sqlx::query("ALTER TABLE users DROP COLUMN auth_gen").execute(&mut conn).await.unwrap();
    sqlx::query("DELETE FROM schema_version WHERE version >= 8").execute(&mut conn).await.unwrap();
}

/// A database from the release before the admin API keeps its accounts, devices and invites: an old invite reads as an
/// invite and still works, no space has a quota and no user a removal mark, and the new calls work on it.
async fn migrating_from_0007_keeps_the_rows(url: &str) {
    sqlx::any::install_default_drivers();
    let (ana, old_token, invite) = {
        let meta = Meta::open(url).await.unwrap();
        let ana = user(&meta, "ana").await;
        let old_token = token();
        meta.create_device(ana.id, "old pc", &old_token, NOW).await.unwrap();
        meta.add_chunk(ana.space, &random_chunk(), 42, NOW).await.unwrap();
        let invite = code(INVITE_CHARS);
        meta.create_invite("ana", &invite.hash(), NOW, INVITE_TTL_MS).await.unwrap();
        (ana, old_token, invite)
    };
    back_to_0007(url).await;
    assert_eq!(schema_version(url).await, 7);

    let meta = Meta::open(url).await.unwrap();
    assert_eq!(schema_version(url).await, 9);
    assert_eq!(meta.device_by_token(&old_token).await.unwrap().map(|a| a.user), Some(ana.id));
    let row = admin_view(&meta, ana.id).await;
    assert_eq!((row.disabled_ms, row.quota_bytes, row.stored_bytes, row.devices_active, row.invite_pending), (None, None, 42, 1, true));
    assert_eq!(meta.invite_needs_password(&invite.hash(), NOW + 1).await.unwrap(), Some(false), "an old invite is an invite");
    let (_, actor) = admin_actor(&meta, "root").await;
    meta.admin_update_user(&actor, ana.id, None, Some(Some(MIN_QUOTA_BYTES)), NOW).await.unwrap();
    meta.admin_disable_user(&actor, ana.id, NOW).await.unwrap();
    assert_eq!(meta.device_by_token(&old_token).await.unwrap(), None);

    use sqlx::Connection;
    let mut conn = sqlx::AnyConnection::connect(url).await.unwrap();
    sqlx::query("INSERT INTO schema_version (version) VALUES (10)").execute(&mut conn).await.unwrap();
    drop(conn);
    assert!(matches!(Meta::open(url).await, Err(MetaError::Corrupt(_))));
}

#[tokio::test]
async fn migrating_from_0007_keeps_the_rows_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    migrating_from_0007_keeps_the_rows(&format!("sqlite:{}", dir.path().join("meta.db").display())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn migrating_from_0007_keeps_the_rows_on_postgres() {
    let Some((url, _guard)) = postgres_database().await else {
        eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set");
        return;
    };
    migrating_from_0007_keeps_the_rows(&url).await;
}

// ---- the audit log (migration 0009) ----

use craft_cloud_meta::{
    ActionFilter, AuditAction, AuditCursor, AuditEvent, AuditFailure, AuditOutcome, AuditQuery, Origin, PRUNE_BATCH, PRUNE_EVERY_MS, Step,
};

/// Every event, newest first.
async fn all_events(meta: &Meta) -> Vec<AuditEvent> {
    meta.audit_list(&AuditQuery { limit: 1000, ..AuditQuery::default() }).await.unwrap()
}

/// Remembers which events a test has looked at, and hands out what was written since, oldest first.
struct Seen<'a> {
    meta: &'a Meta,
    ids: std::collections::HashSet<uuid::Uuid>,
}

impl<'a> Seen<'a> {
    async fn new(meta: &'a Meta) -> Self {
        let mut seen = Self { meta, ids: Default::default() };
        seen.fresh().await;
        seen
    }

    async fn fresh(&mut self) -> Vec<AuditEvent> {
        let Self { meta, ids } = self;
        let mut fresh: Vec<AuditEvent> = all_events(meta).await.into_iter().filter(|e| ids.insert(e.id)).collect();
        fresh.reverse();
        fresh
    }

    /// The names of the events written since the last look, sorted (two events of one change have no order to rely on).
    async fn actions(&mut self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = self.fresh().await.iter().map(|e| e.action.as_str()).collect();
        names.sort_unstable();
        names
    }

    /// The one event written since the last look.
    async fn one(&mut self) -> AuditEvent {
        let mut fresh = self.fresh().await;
        assert_eq!(fresh.len(), 1, "{:?}", fresh.iter().map(|e| e.action.as_str()).collect::<Vec<_>>());
        fresh.remove(0)
    }
}

fn details(e: &AuditEvent) -> serde_json::Value {
    serde_json::from_str(&e.details).unwrap()
}

/// An event of the day-to-day, shorthand for the matrix below.
fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap()
}

/// Every operation of the matrix in `docs/design/sync.md` writes exactly the events it names, with the actor, target and
/// typed details it names; a repeat or a no-op writes none; a bulk sign-out writes one event with the count. Creating
/// an account or a device by the test seams (`create_user`, `create_device`) writes none.
async fn every_operation_writes_exactly_its_events(meta: &Meta) {
    let (root, actor) = admin_actor(meta, "root").await;
    let ana = user(meta, "ana").await;
    let mut seen = Seen::new(meta).await;
    assert!(all_events(meta).await.is_empty(), "the test seams write no events");

    // A sign-in: the account, the new device, and the client reduced to the closed lists.
    let client = DeviceClient::new(Some("agent"), Some("Linux"), Some("1.2.3")).unwrap();
    let dev = meta.sign_in_device(ana.id, (&ana.password_hash, ana.auth_gen), "pc", &client, &token(), NOW).await.unwrap().unwrap();
    let e = seen.one().await;
    assert_eq!(
        (e.action, e.outcome, e.actor_user, e.actor_device, e.count),
        (AuditAction::AuthLogin, AuditOutcome::Success, Some(ana.id), Some(dev), 1)
    );
    assert_eq!((e.target_kind.as_str(), e.target_id), ("device", Some(*dev.as_uuid())));
    assert_eq!(details(&e), json(r#"{"client_kind":"agent","platform":"linux"}"#));
    assert_eq!(e.at_ms, NOW);
    // A sign-in that is refused writes nothing.
    assert!(meta.sign_in_device(ana.id, ("$argon2id$other", 0), "pc", &client, &token(), NOW).await.unwrap().is_none());
    assert!(seen.actions().await.is_empty());

    // Signing out one device: from a device, then the same again (nothing changed), then as the account without a device.
    let (d2, d3) = (device(meta, &ana).await, device(meta, &ana).await);
    meta.revoke_device_by(ana.id, d2, RevokeReason::User, Origin::Device(dev), NOW).await.unwrap();
    let e = seen.one().await;
    assert_eq!((e.action, e.actor_user, e.actor_device, e.target_id), (AuditAction::DeviceRevoked, Some(ana.id), Some(dev), Some(*d2.as_uuid())));
    assert_eq!(details(&e), json(r#"{"reason":"user"}"#), "the device is the event's target");
    meta.revoke_device_by(ana.id, d2, RevokeReason::User, Origin::Device(dev), NOW).await.unwrap();
    assert!(seen.actions().await.is_empty(), "a device that is out already: no change, no event");
    meta.revoke_device(ana.id, d3).await.unwrap();
    let e = seen.one().await;
    assert_eq!((e.action, e.actor_user, e.actor_device), (AuditAction::DeviceRevoked, Some(ana.id), None));

    // Signing out the others: one event with the count; with nobody to sign out, none.
    let (_e1, _e2) = (device(meta, &ana).await, device(meta, &ana).await);
    meta.revoke_other_devices(ana.id, dev, RevokeReason::Others, NOW).await.unwrap();
    let e = seen.one().await;
    assert_eq!((e.action, e.actor_device, e.target_id), (AuditAction::DeviceRevokedOthers, Some(dev), Some(ana.id.0)));
    assert_eq!(details(&e), json(r#"{"reason":"others","revoked":2}"#));
    meta.revoke_other_devices(ana.id, dev, RevokeReason::Others, NOW).await.unwrap();
    assert!(seen.actions().await.is_empty());

    // A password change: one event, the devices signed out with it are only in `revoked`.
    let (_f1, _f2) = (device(meta, &ana).await, device(meta, &ana).await);
    meta.change_password(ana.id, &ana.password_hash, "$argon2id$new", dev, NOW).await.unwrap();
    let e = seen.one().await;
    assert_eq!((e.action, e.actor_user, e.actor_device), (AuditAction::AccountPasswordChanged, Some(ana.id), Some(dev)));
    assert_eq!(details(&e), json(r#"{"revoked":2}"#));
    // A change that fails writes nothing: the password it checked is not the account's.
    assert!(matches!(meta.change_password(ana.id, "$argon2id$wrong", "$argon2id$x", dev, NOW).await, Err(MetaError::Conflict(_))));
    assert!(seen.actions().await.is_empty());

    // Accounts: created with a role; the invite that goes with it is not an event of its own.
    let invite = code(INVITE_CHARS);
    let (newbie, _) = meta.admin_create_user(&actor, "newbie", Role::Member, &invite.hash(), NOW, INVITE_TTL_MS).await.unwrap();
    let e = seen.one().await;
    assert_eq!(
        (e.action, e.actor_user, e.actor_device, e.target_id),
        (AuditAction::AdminUserCreated, Some(root.id), Some(actor.device), Some(newbie.id.0))
    );
    assert_eq!(details(&e), json(r#"{"role_to":"member"}"#));
    // Invites: issued, revoked (a second revoke has nothing to revoke).
    meta.admin_issue_invite(&actor, newbie.id, &code(INVITE_CHARS).hash(), NOW, INVITE_TTL_MS).await.unwrap();
    assert_eq!(seen.actions().await, ["admin.invite_issued"]);
    meta.admin_revoke_invite(&actor, newbie.id, NOW).await.unwrap();
    assert_eq!(seen.actions().await, ["admin.invite_revoked"]);
    assert!(matches!(meta.admin_revoke_invite(&actor, newbie.id, NOW).await, Err(MetaError::NotFound(_))));
    assert!(seen.actions().await.is_empty());

    // A role and a quota in one request: one event for each, in the same transaction. The same again changes nothing.
    meta.admin_update_user(&actor, ana.id, Some(Role::Admin), Some(Some(MIN_QUOTA_BYTES)), NOW).await.unwrap();
    let both = seen.fresh().await;
    let mut names: Vec<&str> = both.iter().map(|e| e.action.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["admin.quota_changed", "admin.role_changed"]);
    for e in &both {
        assert_eq!((e.actor_user, e.target_id), (Some(root.id), Some(ana.id.0)));
        match e.action {
            AuditAction::AdminRoleChanged => assert_eq!(details(e), json(r#"{"role_from":"member","role_to":"admin"}"#)),
            _ => assert_eq!(details(e), json(&format!(r#"{{"quota_bytes":{MIN_QUOTA_BYTES}}}"#))),
        }
    }
    meta.admin_update_user(&actor, ana.id, Some(Role::Admin), Some(Some(MIN_QUOTA_BYTES)), NOW).await.unwrap();
    assert!(seen.actions().await.is_empty(), "the role and quota it has already: no change, no event");
    meta.admin_update_user(&actor, ana.id, Some(Role::Member), None, NOW).await.unwrap();
    let e = seen.one().await;
    assert_eq!((e.action, details(&e)), (AuditAction::AdminRoleChanged, json(r#"{"role_from":"admin","role_to":"member"}"#)));
    meta.admin_update_user(&actor, ana.id, None, Some(None), NOW).await.unwrap();
    let e = seen.one().await;
    assert_eq!((e.action, details(&e)), (AuditAction::AdminQuotaChanged, json(r#"{"quota_bytes":null}"#)), "removing the quota says so");
    meta.admin_update_user(&actor, ana.id, None, Some(None), NOW).await.unwrap();
    assert!(seen.actions().await.is_empty());

    // A reset code, and its redemption: the devices it signs out are counted in the one event.
    let reset = code(INVITE_CHARS);
    meta.admin_issue_reset(&actor, ana.id, &reset.hash(), NOW, RESET_TTL_MS).await.unwrap();
    assert_eq!(seen.actions().await, ["admin.password_reset_issued"]);
    let active = meta.list_devices(ana.id).await.unwrap().iter().filter(|d| !d.revoked).count();
    let redeemed = meta.redeem_with(&reset.hash(), "recovery", &client, &token(), Some("$argon2id$reset"), NOW).await.unwrap();
    assert_eq!(redeemed.revoked.len(), active);
    let e = seen.one().await;
    assert_eq!(
        (e.action, e.actor_user, e.actor_device, e.target_id),
        (AuditAction::JoinResetRedeemed, Some(ana.id), Some(redeemed.device), Some(*redeemed.device.as_uuid()))
    );
    assert_eq!(details(&e), json(&format!(r#"{{"client_kind":"agent","platform":"linux","revoked":{active}}}"#)));

    // An invite redeemed with a password for the new account, and a pairing redeemed from its device.
    meta.admin_issue_invite(&actor, newbie.id, &invite.hash(), NOW, INVITE_TTL_MS).await.unwrap();
    seen.fresh().await;
    let joined = meta.redeem(&invite.hash(), "newbie pc", &token(), Some("$argon2id$newbie"), NOW).await.unwrap();
    let e = seen.one().await;
    assert_eq!((e.action, e.actor_user, e.actor_device), (AuditAction::JoinInviteRedeemed, Some(newbie.id), Some(joined.device)));
    assert_eq!(details(&e), json("{}"), "no client named: no client in the event");
    let pairing = code(PAIRING_CHARS);
    meta.create_pairing(newbie.id, joined.device, &pairing.hash(), NOW).await.unwrap();
    assert!(seen.actions().await.is_empty(), "making a code is not an event");
    let paired = meta.redeem(&pairing.hash(), "second pc", &token(), None, NOW).await.unwrap();
    let e = seen.one().await;
    assert_eq!((e.action, e.actor_user, e.actor_device), (AuditAction::JoinPairingRedeemed, Some(newbie.id), Some(paired.device)));
    // A code that does not work writes nothing.
    assert!(matches!(meta.redeem(&pairing.hash(), "again", &token(), None, NOW).await, Err(MetaError::InvalidCode)));
    assert!(seen.actions().await.is_empty());

    // Disable (the devices go with it), a second disable, enable, a second enable.
    let (_g1, _g2) = (device(meta, &ana).await, device(meta, &ana).await);
    let signed_out = meta.admin_disable_user(&actor, ana.id, NOW).await.unwrap().len();
    let e = seen.one().await;
    assert_eq!((e.action, e.actor_user, e.target_id), (AuditAction::AdminUserDisabled, Some(root.id), Some(ana.id.0)));
    assert_eq!(details(&e), json(&format!(r#"{{"revoked":{signed_out}}}"#)));
    meta.admin_disable_user(&actor, ana.id, NOW).await.unwrap();
    assert!(seen.actions().await.is_empty(), "disabled already: no change, no event");
    meta.admin_enable_user(&actor, ana.id, NOW).await.unwrap();
    assert_eq!(seen.actions().await, ["admin.user_enabled"]);
    meta.admin_enable_user(&actor, ana.id, NOW).await.unwrap();
    assert!(seen.actions().await.is_empty());

    // An admin signs out one device: about the account, naming the device; again, nothing.
    let anas = device(meta, &ana).await;
    meta.admin_revoke_device(&actor, ana.id, anas, NOW).await.unwrap();
    let e = seen.one().await;
    assert_eq!(
        (e.action, e.actor_user, e.actor_device, e.target_kind.as_str(), e.target_id),
        (AuditAction::AdminDeviceRevoked, Some(root.id), Some(actor.device), "user", Some(ana.id.0))
    );
    assert_eq!(details(&e), json(&format!(r#"{{"device":"{anas}","reason":"admin"}}"#)));
    meta.admin_revoke_device(&actor, ana.id, anas, NOW).await.unwrap();
    assert!(seen.actions().await.is_empty());

    // Marked for removal, once.
    let _ = device(meta, &ana).await;
    meta.admin_remove_user(&actor, ana.id, NOW).await.unwrap();
    let e = seen.one().await;
    assert_eq!((e.action, e.target_id), (AuditAction::AdminUserRemoved, Some(ana.id.0)));
    assert_eq!(details(&e), json(r#"{"revoked":1}"#));
    meta.admin_remove_user(&actor, ana.id, NOW).await.unwrap();
    assert!(seen.actions().await.is_empty());

    // The command line acts as no account.
    let cara = user(meta, "cara").await;
    let (_h1, _h2) = (device(meta, &cara).await, device(meta, &cara).await);
    meta.reset_password(cara.id, "$argon2id$cli", false, NOW).await.unwrap();
    let e = seen.one().await;
    assert_eq!((e.action, e.actor_user, e.actor_device, e.target_id), (AuditAction::CliPasswordReset, None, None, Some(cara.id.0)));
    assert_eq!(details(&e), json(r#"{"revoked":2}"#));
    let one = device(meta, &cara).await;
    meta.revoke_device_by(cara.id, one, RevokeReason::Admin, Origin::Cli, NOW).await.unwrap();
    let e = seen.one().await;
    assert_eq!((e.action, e.actor_user, e.actor_device, e.target_id), (AuditAction::CliDeviceRevoked, None, None, Some(cara.id.0)));
    assert_eq!(details(&e), json(&format!(r#"{{"device":"{one}","reason":"admin"}}"#)));
    meta.revoke_device_by(cara.id, one, RevokeReason::Admin, Origin::Cli, NOW).await.unwrap();
    assert!(seen.actions().await.is_empty());
    let (_i1, _i2, _i3) = (device(meta, &cara).await, device(meta, &cara).await, device(meta, &cara).await);
    meta.revoke_all_devices(cara.id, RevokeReason::Admin, NOW).await.unwrap();
    let e = seen.one().await;
    assert_eq!((e.action, e.actor_user, e.target_id), (AuditAction::CliDevicesRevokedAll, None, Some(cara.id.0)));
    assert_eq!(details(&e), json(r#"{"revoked":3}"#));
    meta.revoke_all_devices(cara.id, RevokeReason::Admin, NOW).await.unwrap();
    assert!(seen.actions().await.is_empty());
    // A reset that keeps the devices counts none.
    meta.reset_password(cara.id, "$argon2id$cli2", true, NOW).await.unwrap();
    assert_eq!(details(&seen.one().await), json(r#"{"revoked":0}"#));
}

/// A change that fails, or is refused, leaves no event behind, and the last administrator stays one.
async fn a_refused_change_writes_no_event(meta: &Meta) {
    let (_, actor) = admin_actor(meta, "root").await;
    let ana = user(meta, "ana").await;
    let mut seen = Seen::new(meta).await;
    // The only administrator cannot demote or disable themselves out of the job, and the role stays.
    assert!(matches!(meta.admin_update_user(&actor, actor.user, Some(Role::Member), None, NOW).await, Err(MetaError::LastAdmin)));
    assert!(matches!(meta.admin_disable_user(&actor, actor.user, NOW).await, Err(MetaError::Invalid(_))));
    // An actor whose role is gone, one whose password changed since, and a target that is not there.
    let stale = AdminActor { password_hash: "$argon2id$old".to_owned(), ..actor.clone() };
    assert!(matches!(meta.admin_disable_user(&stale, ana.id, NOW).await, Err(MetaError::Conflict(_))));
    assert!(matches!(meta.admin_update_user(&actor, UserId(uuid::Uuid::now_v7()), Some(Role::Admin), None, NOW).await, Err(MetaError::NotFound(_))));
    // A reset code without the new password, a name that is taken, a device of someone else.
    let reset = code(INVITE_CHARS);
    meta.admin_issue_reset(&actor, ana.id, &reset.hash(), NOW, RESET_TTL_MS).await.unwrap();
    seen.fresh().await;
    assert!(matches!(meta.redeem(&reset.hash(), "pc", &token(), None, NOW).await, Err(MetaError::PasswordRequired)));
    assert!(matches!(
        meta.admin_create_user(&actor, "ana", Role::Member, &code(INVITE_CHARS).hash(), NOW, INVITE_TTL_MS).await,
        Err(MetaError::UsernameTaken)
    ));
    assert!(matches!(meta.revoke_device_as(ana.id, DeviceId::generate(), RevokeReason::User, NOW).await, Err(MetaError::NotFound(_))));
    assert!(seen.actions().await.is_empty());
    assert_eq!(meta.account(actor.user).await.unwrap().role, Role::Admin);
}

/// A sign-in step that failed is one row per account, action and minute, whose count goes up and whose identity, time and
/// details stay those of the first try; names no account has share one row per minute; an event that is not a failed step
/// cannot be aggregated.
async fn failed_sign_ins_count_up_in_one_row_per_account_and_minute(meta: &Meta) {
    let (ana, bob) = (user(meta, "ana").await, user(meta, "bob").await);
    let minute = NOW - NOW % 60_000;
    let login = |target: Option<UserId>| AuditFailure { action: AuditAction::AuthLoginFailed, target, step: Step::Password };
    meta.audit_record_failure(login(Some(ana.id)), minute + 1_000).await.unwrap();
    let first = all_events(meta).await;
    assert_eq!(first.len(), 1);
    let first = first[0].clone();
    assert_eq!(
        (first.action, first.outcome, first.count, first.actor_user, first.target_id),
        (AuditAction::AuthLoginFailed, AuditOutcome::Failure, 1, None, Some(ana.id.0))
    );
    assert_eq!((first.target_kind.as_str(), details(&first)), ("user", json(r#"{"step":"password"}"#)));
    // The same minute: the count goes up, nothing else changes.
    meta.audit_record_failure(login(Some(ana.id)), minute + 59_000).await.unwrap();
    meta.audit_record_failure(login(Some(ana.id)), minute + 59_999).await.unwrap();
    let rows = all_events(meta).await;
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].id, rows[0].at_ms, rows[0].count, &rows[0].details), (first.id, minute + 1_000, 3, &first.details));
    // The next minute is a new row; another account, another action, another row.
    meta.audit_record_failure(login(Some(ana.id)), minute + 60_000).await.unwrap();
    meta.audit_record_failure(login(Some(bob.id)), minute + 1_000).await.unwrap();
    meta.audit_record_failure(AuditFailure { action: AuditAction::AuthSecondFactorFailed, target: Some(ana.id), step: Step::Totp }, minute + 1_000)
        .await
        .unwrap();
    let rows = all_events(meta).await;
    assert_eq!(rows.len(), 4);
    let mut counts: Vec<(&str, u64)> = rows.iter().map(|e| (e.action.as_str(), e.count)).collect();
    counts.sort_unstable();
    assert_eq!(counts, [("auth.login_failed", 1), ("auth.login_failed", 1), ("auth.login_failed", 3), ("auth.second_factor_failed", 1)]);
    // Names nobody has: one row a minute whatever was typed, with no account in it.
    for _ in 0..4 {
        meta.audit_record_failure(login(None), minute + 2_000).await.unwrap();
    }
    meta.audit_record_failure(login(None), minute + 62_000).await.unwrap();
    let unknown: Vec<AuditEvent> = all_events(meta).await.into_iter().filter(|e| e.target_id.is_none()).collect();
    assert_eq!(unknown.len(), 2);
    assert!(unknown.iter().all(|e| e.actor_user.is_none() && e.target_kind.is_empty() && e.actor_device.is_none()));
    let mut counts: Vec<u64> = unknown.iter().map(|e| e.count).collect();
    counts.sort_unstable();
    assert_eq!(counts, [1, 4]);
    // Only the failed steps are aggregated.
    for action in [AuditAction::AuthLogin, AuditAction::AdminUserCreated, AuditAction::CliPasswordReset] {
        assert!(matches!(
            meta.audit_record_failure(AuditFailure { action, target: None, step: Step::Password }, NOW).await,
            Err(MetaError::Invalid(_))
        ));
    }
    // Other events have no key, so any number of them is allowed.
    let (_, actor) = admin_actor(meta, "root").await;
    for i in 0..3 {
        meta.admin_update_user(&actor, ana.id, Some(if i % 2 == 0 { Role::Admin } else { Role::Member }), None, NOW).await.unwrap();
    }
    assert_eq!(all_events(meta).await.iter().filter(|e| e.action == AuditAction::AdminRoleChanged).count(), 3);
}

/// Many tries at one moment on one key: exactly one row, counting every one, and the first row's identity and time stay.
/// Run on both databases (on Postgres the collisions really are concurrent).
async fn concurrent_failures_with_one_key_make_one_row_with_every_count(meta: &Meta) {
    let ana = user(meta, "ana").await;
    let minute = NOW - NOW % 60_000;
    const TRIES: u64 = 24;
    for round in 0..3i64 {
        let at = minute + round * 60_000;
        let tries: Vec<_> = (0..TRIES)
            .map(|i| {
                let meta = meta.clone();
                let failure = AuditFailure { action: AuditAction::AuthLoginFailed, target: Some(ana.id), step: Step::Password };
                tokio::spawn(async move { meta.audit_record_failure(failure, at + i as i64).await })
            })
            .collect();
        for t in tries {
            t.await.unwrap().unwrap();
        }
    }
    let rows = all_events(meta).await;
    assert_eq!(rows.len(), 3, "one row for each minute");
    assert!(rows.iter().all(|e| e.count == TRIES), "{:?}", rows.iter().map(|e| e.count).collect::<Vec<_>>());
    // Each row kept the time of the try that made it: one of the first moment's tries, within its minute.
    for (round, row) in rows.iter().rev().enumerate() {
        let start = minute + round as i64 * 60_000;
        assert!((start..start + TRIES as i64).contains(&row.at_ms), "{} not in {}..", row.at_ms, start);
    }
}

/// Lists: newest first in `(at_ms, id)` order, filters by account and by name or group of names, `before_ms` and an end
/// time, and a cursor that is exclusive so that events with one timestamp are neither repeated nor lost.
async fn audit_lists_filter_and_page_by_cursor(meta: &Meta) {
    let (root, actor) = admin_actor(meta, "root").await;
    let (ana, bob) = (user(meta, "ana").await, user(meta, "bob").await);
    // Seven events at one moment.
    for i in 0..7 {
        meta.admin_update_user(&actor, ana.id, Some(if i % 2 == 0 { Role::Admin } else { Role::Member }), None, NOW).await.unwrap();
    }
    meta.sign_in_device(bob.id, (&bob.password_hash, bob.auth_gen), "pc", &DeviceClient::default(), &token(), NOW + 5).await.unwrap().unwrap();
    let minute = NOW - NOW % 60_000 + 60_000;
    meta.audit_record_failure(AuditFailure { action: AuditAction::AuthLoginFailed, target: Some(bob.id), step: Step::Password }, minute)
        .await
        .unwrap();
    meta.audit_record_failure(AuditFailure { action: AuditAction::AuthLoginFailed, target: None, step: Step::Password }, minute + 60_000)
        .await
        .unwrap();

    let all = all_events(meta).await;
    assert_eq!(all.len(), 10);
    let order: Vec<_> = all.iter().map(|e| (e.at_ms, e.id)).collect();
    assert!(order.windows(2).all(|w| w[0] > w[1]), "newest first, by (at_ms, id) descending");

    // Pages of three through the cursor of the last event of each: all ten once, in order.
    let mut paged = Vec::new();
    let mut after: Option<AuditCursor> = None;
    for _ in 0..10 {
        let page = meta.audit_list(&AuditQuery { after, limit: 3, ..AuditQuery::default() }).await.unwrap();
        let Some(last) = page.last() else { break };
        after = Some(last.cursor());
        paged.extend(page);
    }
    assert_eq!(paged, all);
    // A cursor behind everything gives nothing; a cursor at an event never gives that event.
    assert!(
        meta.audit_list(&AuditQuery { after: Some(AuditCursor { at_ms: 0, id: uuid::Uuid::nil() }), limit: 10, ..AuditQuery::default() })
            .await
            .unwrap()
            .is_empty()
    );
    let sevens: Vec<&AuditEvent> = all.iter().filter(|e| e.at_ms == NOW).collect();
    assert_eq!(sevens.len(), 7);
    let page = meta.audit_list(&AuditQuery { after: Some(sevens[2].cursor()), limit: 100, ..AuditQuery::default() }).await.unwrap();
    assert_eq!(page.len(), 4, "the four behind the third of the seven that share a time");

    // Times: before is exclusive, the end time inclusive.
    let before = |ms| AuditQuery { before_ms: Some(ms), limit: 100, ..AuditQuery::default() };
    assert_eq!(meta.audit_list(&before(NOW)).await.unwrap().len(), 0);
    assert_eq!(meta.audit_list(&before(NOW + 1)).await.unwrap().len(), 7);
    assert_eq!(meta.audit_list(&AuditQuery { until_ms: Some(NOW), limit: 100, ..AuditQuery::default() }).await.unwrap().len(), 7);
    assert_eq!(meta.audit_list(&AuditQuery { until_ms: Some(NOW - 1), limit: 100, ..AuditQuery::default() }).await.unwrap().len(), 0);

    // Accounts: the actor or the target.
    let by_user = |u: UserId| AuditQuery { user: Some(u), limit: 100, ..AuditQuery::default() };
    assert_eq!(meta.audit_list(&by_user(ana.id)).await.unwrap().len(), 7);
    assert_eq!(meta.audit_list(&by_user(root.id)).await.unwrap().len(), 7);
    assert_eq!(meta.audit_list(&by_user(bob.id)).await.unwrap().len(), 2, "his sign-in, and the guess about his password");
    assert_eq!(meta.audit_list(&by_user(UserId(uuid::Uuid::now_v7()))).await.unwrap().len(), 0);

    // Names: exact, or a group.
    let by_action = |text: &str| AuditQuery { action: ActionFilter::parse(text), limit: 100, ..AuditQuery::default() };
    assert_eq!(meta.audit_list(&by_action("admin.role_changed")).await.unwrap().len(), 7);
    assert_eq!(meta.audit_list(&by_action("auth.login")).await.unwrap().len(), 1, "exact: not auth.login_failed");
    assert_eq!(meta.audit_list(&by_action("auth.")).await.unwrap().len(), 3);
    assert_eq!(meta.audit_list(&by_action("admin.")).await.unwrap().len(), 7);
    assert_eq!(meta.audit_list(&by_action("cli.")).await.unwrap().len(), 0);
    // Both at once, and with a page.
    let both = AuditQuery { user: Some(bob.id), action: ActionFilter::parse("auth."), limit: 1, ..AuditQuery::default() };
    assert_eq!(meta.audit_list(&both).await.unwrap().len(), 1);
}

/// Pruning deletes what is older than the limit and nothing else, a batch at a time; a pruning that leaves a backlog
/// does not count as done, so the next write goes on; one that finished holds off the next for an hour; an error is not
/// done either.
async fn pruning_works_in_batches_and_keeps_its_time_until_done(meta: &Meta) {
    let ana = user(meta, "ana").await;
    let day = 24 * 60 * 60 * 1000i64;
    let old = NOW - 3 * day;
    // More old events than one batch holds, each in a minute of its own.
    let n = PRUNE_BATCH + 100;
    let writers: Vec<_> = (0..8)
        .map(|w| {
            let meta = meta.clone();
            tokio::spawn(async move {
                let mut i = w;
                while i < n {
                    let failure = AuditFailure { action: AuditAction::AuthLoginFailed, target: None, step: Step::Password };
                    meta.audit_record_failure(failure, old + i * 60_000).await.unwrap();
                    i += 8;
                }
            })
        })
        .collect();
    for w in writers {
        w.await.unwrap();
    }
    // Recent events, which pruning leaves.
    for i in 0..3 {
        let failure = AuditFailure { action: AuditAction::AuthLoginFailed, target: Some(ana.id), step: Step::Password };
        meta.audit_record_failure(failure, NOW - day / 2 + i * 60_000).await.unwrap();
    }
    let limit = NOW - day;
    assert_eq!(meta.audit_count_before(limit).await.unwrap(), n as u64);
    assert_eq!(meta.audit_count_before(old).await.unwrap(), 0, "the limit is exclusive");

    // Not configured: a write prunes nothing.
    assert!(!meta.audit_prune_if_due(NOW).await.unwrap());
    // A second handle that prunes when asked. The writes below go through `meta`, which never starts a pruning of its own.
    let pruner = meta.clone().with_audit_retention_days(1);
    // One batch, then the rest: the first leaves a backlog and so does not hold off the next.
    assert!(pruner.audit_prune_if_due_in(NOW, 1).await.unwrap());
    assert_eq!(meta.audit_count_before(limit).await.unwrap(), 100, "one batch of {PRUNE_BATCH}");
    assert!(pruner.audit_prune_if_due_in(NOW + 1, 1).await.unwrap(), "a backlog is not done");
    assert_eq!(meta.audit_count_before(limit).await.unwrap(), 0);
    assert_eq!(all_events(meta).await.len(), 3, "the recent events stay");
    // Done: the next hour is left alone, then it runs again.
    let failure = AuditFailure { action: AuditAction::AuthLoginFailed, target: None, step: Step::Password };
    meta.audit_record_failure(failure, old).await.unwrap();
    assert!(!pruner.audit_prune_if_due_in(NOW + 2, 1).await.unwrap());
    assert_eq!(meta.audit_count_before(limit).await.unwrap(), 1);
    assert!(pruner.audit_prune_if_due_in(NOW + 1 + PRUNE_EVERY_MS, 1).await.unwrap());
    assert_eq!(meta.audit_count_before(limit).await.unwrap(), 0);

    // `audit_prune` itself takes everything before the limit in batches, and reports the count.
    for i in 0..(PRUNE_BATCH + 5) {
        let failure = AuditFailure { action: AuditAction::AuthLoginFailed, target: None, step: Step::Password };
        meta.audit_record_failure(failure, old + i * 60_000).await.unwrap();
    }
    assert_eq!(meta.audit_prune(limit).await.unwrap(), (PRUNE_BATCH + 5) as u64);
    assert_eq!(meta.audit_prune(limit).await.unwrap(), 0);
    assert_eq!(all_events(meta).await.len(), 3);
}

/// Events outlive the account they are about: marking it for removal deletes nothing, and the account's ID still finds them.
async fn a_removed_accounts_events_stay(meta: &Meta) {
    let (_, actor) = admin_actor(meta, "root").await;
    let ana = user(meta, "ana").await;
    meta.sign_in_device(ana.id, (&ana.password_hash, ana.auth_gen), "pc", &DeviceClient::default(), &token(), NOW).await.unwrap().unwrap();
    meta.audit_record_failure(AuditFailure { action: AuditAction::AuthLoginFailed, target: Some(ana.id), step: Step::Password }, NOW).await.unwrap();
    meta.admin_remove_user(&actor, ana.id, NOW + 1).await.unwrap();
    let mine = meta.audit_list(&AuditQuery { user: Some(ana.id), limit: 100, ..AuditQuery::default() }).await.unwrap();
    let mut names: Vec<&str> = mine.iter().map(|e| e.action.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["admin.user_removed", "auth.login", "auth.login_failed"]);
}

/// Nothing a client typed or a secret is in any event, whatever the account, the device or the client called itself.
async fn no_event_holds_a_name_or_a_secret(meta: &Meta) {
    let (_, actor) = admin_actor(meta, "root").await;
    let hash = "$argon2id$SECRETHASH";
    let zelda = meta.create_user("zelda", hash, Role::Member, NOW).await.unwrap();
    // A platform may be any printable text of 32 bytes: here an address, a file name, and 32 bytes of multi-byte characters.
    let hostile =
        ["10.0.0.1", "secret.psd", "fe80::1%eth0", "\u{20ac}\u{20ac}\u{20ac}\u{20ac}\u{20ac}\u{20ac}\u{20ac}\u{20ac}\u{20ac}\u{20ac}", "windows 11"];
    let mut clock = NOW;
    for platform in hostile {
        let client = DeviceClient::new(Some("desktop"), Some(platform), Some("9.9.9-secretbuild")).unwrap();
        clock += 1;
        meta.sign_in_device(zelda.id, (hash, 0), "zelda-private-laptop", &client, &token(), clock).await.unwrap().unwrap();
        let e = all_events(meta).await.remove(0);
        assert_eq!(details(&e), json(r#"{"client_kind":"desktop","platform":"other"}"#), "{platform:?}");
    }
    let known = DeviceClient::new(Some("cli"), Some("MacOS"), None).unwrap();
    meta.sign_in_device(zelda.id, (hash, 0), "zelda-private-laptop", &known, &token(), NOW + 100).await.unwrap().unwrap();
    assert_eq!(details(&all_events(meta).await.remove(0)), json(r#"{"client_kind":"cli","platform":"macos"}"#));
    let no_platform = DeviceClient::new(Some("agent"), None, None).unwrap();
    meta.sign_in_device(zelda.id, (hash, 0), "zelda-private-laptop", &no_platform, &token(), NOW + 200).await.unwrap().unwrap();
    assert_eq!(details(&all_events(meta).await.remove(0)), json(r#"{"client_kind":"agent"}"#), "no platform: no field");
    // A few more changes, and the same scan over everything written.
    meta.change_password(zelda.id, hash, "$argon2id$NEWSECRETHASH", meta.list_devices(zelda.id).await.unwrap()[0].id, NOW).await.unwrap();
    meta.admin_update_user(&actor, zelda.id, Some(Role::Admin), Some(Some(MIN_QUOTA_BYTES)), NOW).await.unwrap();
    meta.audit_record_failure(AuditFailure { action: AuditAction::AuthLoginFailed, target: None, step: Step::Password }, NOW).await.unwrap();
    let events = all_events(meta).await;
    assert!(events.len() > 8);
    let shown = format!("{events:?}");
    for secret in ["zelda", "SECRETHASH", "secretbuild", "10.0.0.1", "secret.psd", "fe80", "127.0.0.1", "\u{20ac}", "argon2", "laptop", "ZELDA"] {
        assert!(!shown.contains(secret), "{secret:?} in {shown}");
    }
    // Every `details` is an object of the allowed words and numbers only.
    for e in &events {
        let value = details(e);
        let object = value.as_object().unwrap();
        for key in object.keys() {
            assert!(
                ["client_kind", "platform", "reason", "step", "role_from", "role_to", "quota_bytes", "revoked", "device", "truncated"]
                    .contains(&key.as_str()),
                "{key}"
            );
        }
        assert!(e.details.len() <= 2048);
    }
}

/// The log is append-only in the code: the sources of this crate update and delete in `audit_events` in exactly the two
/// places the design names (the counter of an aggregated failure, the pruning).
#[test]
fn the_sources_change_audit_rows_only_to_count_and_to_prune() {
    fn sources(dir: &std::path::Path, out: &mut Vec<(std::path::PathBuf, String)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                sources(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).unwrap();
                out.push((path, text));
            }
        }
    }
    let mut files = Vec::new();
    sources(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
    assert!(files.len() > 5, "the sources were found");
    let count = |needle: &str| -> Vec<String> {
        files
            .iter()
            .flat_map(|(path, text)| {
                // Comments may name statements, only code counts.
                text.lines()
                    .filter(|l| !l.trim_start().starts_with("//") && l.contains(needle))
                    .map(move |_| path.file_name().unwrap().to_string_lossy().into_owned())
            })
            .collect()
    };
    assert!(count("UPDATE audit_events").is_empty(), "no statement updates the log");
    assert!(count("audit_events SET").is_empty());
    assert_eq!(count("DELETE FROM audit_events"), ["audit.rs"], "exactly one deletion, the pruning");
    assert_eq!(count("DO UPDATE SET agg_count = audit_events.agg_count + 1"), ["audit.rs"], "exactly one update, the counter");
    assert!(count("TRUNCATE").is_empty() && count("DROP TABLE audit").is_empty());
}

// ---- an error in the log ----

/// Makes every use of the log fail (the table is renamed away), which stands for any error of the database in the log
/// and needs no code in the product to cause it.
async fn break_audit(url: &str) {
    use sqlx::Connection;
    let mut conn = sqlx::AnyConnection::connect(url).await.unwrap();
    sqlx::query("ALTER TABLE audit_events RENAME TO audit_events_away").execute(&mut conn).await.unwrap();
}

async fn repair_audit(url: &str) {
    use sqlx::Connection;
    let mut conn = sqlx::AnyConnection::connect(url).await.unwrap();
    sqlx::query("ALTER TABLE audit_events_away RENAME TO audit_events").execute(&mut conn).await.unwrap();
}

/// A change whose event cannot be written is not made (fail closed), whichever it is; a failed sign-in step's event is
/// only reported to its caller, which ignores it; pruning that fails is not done, so the next write tries again.
async fn an_error_in_the_log_fails_every_change_and_changes_nothing(url: &str) {
    sqlx::any::install_default_drivers();
    let meta = Meta::open(url).await.unwrap();
    let pruner = meta.clone().with_audit_retention_days(1);
    let (_, actor) = admin_actor(&meta, "root").await;
    let ana = user(&meta, "ana").await;
    let dev = device(&meta, &ana).await;
    let others = [device(&meta, &ana).await, device(&meta, &ana).await];
    let invite = code(INVITE_CHARS);
    let fresh = meta.admin_create_user(&actor, "fresh", Role::Member, &invite.hash(), NOW, INVITE_TTL_MS).await.unwrap().0;
    let events_before = all_events(&meta).await.len();
    let active = |meta: &Meta, user: UserId| {
        let meta = meta.clone();
        async move { meta.list_devices(user).await.unwrap().iter().filter(|d| !d.revoked).count() }
    };
    assert_eq!(active(&meta, ana.id).await, 3);

    break_audit(url).await;
    let is_db = |r: Result<(), MetaError>| assert!(matches!(r, Err(MetaError::Db(_))), "{r:?}");
    is_db(meta.sign_in_device(ana.id, (&ana.password_hash, 0), "new", &DeviceClient::default(), &token(), NOW).await.map(|_| ()));
    is_db(meta.revoke_device_by(ana.id, others[0], RevokeReason::User, Origin::Device(dev), NOW).await);
    is_db(meta.revoke_other_devices(ana.id, dev, RevokeReason::Others, NOW).await.map(|_| ()));
    is_db(meta.change_password(ana.id, &ana.password_hash, "$argon2id$new", dev, NOW).await.map(|_| ()));
    is_db(meta.reset_password(ana.id, "$argon2id$cli", false, NOW).await.map(|_| ()));
    is_db(meta.revoke_all_devices(ana.id, RevokeReason::Admin, NOW).await.map(|_| ()));
    is_db(meta.admin_update_user(&actor, ana.id, Some(Role::Admin), Some(Some(MIN_QUOTA_BYTES)), NOW).await.map(|_| ()));
    is_db(meta.admin_disable_user(&actor, ana.id, NOW).await.map(|_| ()));
    is_db(meta.admin_remove_user(&actor, ana.id, NOW).await.map(|_| ()));
    is_db(meta.admin_issue_reset(&actor, ana.id, &code(INVITE_CHARS).hash(), NOW, RESET_TTL_MS).await.map(|_| ()));
    is_db(meta.admin_revoke_device(&actor, ana.id, dev, NOW).await);
    is_db(meta.admin_issue_invite(&actor, fresh.id, &code(INVITE_CHARS).hash(), NOW, INVITE_TTL_MS).await.map(|_| ()));
    is_db(meta.admin_revoke_invite(&actor, fresh.id, NOW).await);
    is_db(meta.admin_create_user(&actor, "nobody", Role::Member, &code(INVITE_CHARS).hash(), NOW, INVITE_TTL_MS).await.map(|_| ()));
    is_db(meta.redeem(&invite.hash(), "pc", &token(), Some("$argon2id$x"), NOW).await.map(|_| ()));
    // A sign-in step that failed has no change to undo: the error is the caller's, and nothing else is touched.
    is_db(meta.audit_record_failure(AuditFailure { action: AuditAction::AuthLoginFailed, target: Some(ana.id), step: Step::Password }, NOW).await);
    assert!(pruner.audit_prune_if_due(NOW).await.is_err(), "pruning with the log broken is an error");
    repair_audit(url).await;

    // Nothing was changed by any of it.
    assert_eq!(all_events(&meta).await.len(), events_before, "no event, no change");
    assert_eq!(active(&meta, ana.id).await, 3, "no device was signed out or added");
    let row = meta.admin_user(ana.id, NOW).await.unwrap().unwrap();
    assert_eq!((row.role, row.disabled_ms, row.quota_bytes), (Role::Member, None, None));
    assert_eq!(meta.user_by_name("ana").await.unwrap().unwrap().password_hash, ana.password_hash);
    assert!(meta.user_by_name("nobody").await.unwrap().is_none(), "the account was not made");
    assert!(meta.admin_user(fresh.id, NOW).await.unwrap().unwrap().invite_pending, "the invite is still there");
    // And the same changes work once the log does, the invite included.
    meta.redeem(&invite.hash(), "pc", &token(), Some("$argon2id$x"), NOW).await.unwrap();
    meta.admin_update_user(&actor, ana.id, Some(Role::Admin), None, NOW).await.unwrap();
    // A pruning that failed left no time behind: the next write's goes ahead at once.
    assert!(pruner.audit_prune_if_due(NOW + 1).await.unwrap());
}

#[tokio::test]
async fn an_error_in_the_log_fails_every_change_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    an_error_in_the_log_fails_every_change_and_changes_nothing(&format!("sqlite:{}", dir.path().join("meta.db").display())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_error_in_the_log_fails_every_change_on_postgres() {
    let Some((url, _guard)) = postgres_database().await else {
        eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set");
        return;
    };
    an_error_in_the_log_fails_every_change_and_changes_nothing(&url).await;
}

/// A write prunes old events after its commit, in the background, when the store is told how long to keep them.
#[tokio::test]
async fn a_write_prunes_old_events_after_its_commit() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}", dir.path().join("meta.db").display());
    let plain = Meta::open(&url).await.unwrap();
    let ana = user(&plain, "ana").await;
    let day = 24 * 60 * 60 * 1000i64;
    for i in 0..4 {
        let failure = AuditFailure { action: AuditAction::AuthLoginFailed, target: None, step: Step::Password };
        plain.audit_record_failure(failure, NOW - 40 * day + i * 60_000).await.unwrap();
    }
    // Without a retention a write leaves them.
    plain.sign_in_device(ana.id, (&ana.password_hash, ana.auth_gen), "pc", &DeviceClient::default(), &token(), NOW).await.unwrap().unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(plain.audit_count_before(NOW - 30 * day).await.unwrap(), 4);
    // With one, the next write starts the pruning.
    let meta = plain.clone().with_audit_retention_days(30);
    meta.sign_in_device(ana.id, (&ana.password_hash, ana.auth_gen), "pc", &DeviceClient::default(), &token(), NOW + 1).await.unwrap().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while meta.audit_count_before(NOW - 30 * day).await.unwrap() > 0 {
        assert!(std::time::Instant::now() < deadline, "the old events were not pruned");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(all_events(&meta).await.len(), 2, "the two sign-ins stay");
}

// ---- the migration from 0008 to 0009 ----

/// Drops what migration 0009 added and forgets it ran, which leaves the database as 0008 left it (with its rows).
async fn back_to_0008(url: &str) {
    use sqlx::Connection;
    let mut conn = sqlx::AnyConnection::connect(url).await.unwrap();
    sqlx::query("DROP TABLE audit_events").execute(&mut conn).await.unwrap();
    sqlx::query("DELETE FROM schema_version WHERE version >= 9").execute(&mut conn).await.unwrap();
}

/// A database from the release before the audit log keeps its accounts and devices, gets an empty log that works, and a
/// database newer than the program is still refused.
async fn migrating_from_0008_adds_the_audit_log(url: &str) {
    sqlx::any::install_default_drivers();
    let (ana, old_token) = {
        let meta = Meta::open(url).await.unwrap();
        let ana = user(&meta, "ana").await;
        let old_token = token();
        meta.create_device(ana.id, "old pc", &old_token, NOW).await.unwrap();
        meta.sign_in_device(ana.id, (&ana.password_hash, ana.auth_gen), "pc", &DeviceClient::default(), &token(), NOW).await.unwrap().unwrap();
        assert_eq!(all_events(&meta).await.len(), 1);
        (ana, old_token)
    };
    back_to_0008(url).await;
    assert_eq!(schema_version(url).await, 8);

    let meta = Meta::open(url).await.unwrap();
    assert_eq!(schema_version(url).await, 9);
    assert_eq!(meta.device_by_token(&old_token).await.unwrap().map(|a| a.user), Some(ana.id));
    assert!(all_events(&meta).await.is_empty(), "a new log is empty");
    meta.sign_in_device(ana.id, (&ana.password_hash, ana.auth_gen), "pc", &DeviceClient::default(), &token(), NOW).await.unwrap().unwrap();
    meta.audit_record_failure(AuditFailure { action: AuditAction::AuthLoginFailed, target: Some(ana.id), step: Step::Password }, NOW).await.unwrap();
    assert_eq!(all_events(&meta).await.len(), 2);

    use sqlx::Connection;
    let mut conn = sqlx::AnyConnection::connect(url).await.unwrap();
    sqlx::query("INSERT INTO schema_version (version) VALUES (10)").execute(&mut conn).await.unwrap();
    drop(conn);
    assert!(matches!(Meta::open(url).await, Err(MetaError::Corrupt(_))));
}

#[tokio::test]
async fn migrating_from_0008_adds_the_audit_log_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    migrating_from_0008_adds_the_audit_log(&format!("sqlite:{}", dir.path().join("meta.db").display())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn migrating_from_0008_adds_the_audit_log_on_postgres() {
    let Some((url, _guard)) = postgres_database().await else {
        eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set");
        return;
    };
    migrating_from_0008_adds_the_audit_log(&url).await;
}
