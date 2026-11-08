//! The same scenarios on every backend. SQLite always runs (a fresh temporary file per test). Postgres runs
//! when `CRAFT_CLOUD_TEST_PG` holds an admin URL, e.g. `postgres://postgres:pw@127.0.0.1:5432/postgres`; each
//! test creates and drops its own database. CI sets `CRAFT_CLOUD_REQUIRE_BACKENDS`, which turns a missing
//! `CRAFT_CLOUD_TEST_PG` into a failure instead of a skip.

use craft_cloud_meta::{
    AdminActor, AdminUser, AdminVia, BlobKind, DeviceActor, DeviceClient, DeviceRecord, InviteFor, MIN_QUOTA_BYTES, Meta, MetaError, NewChallenge,
    NewWebSession, Proof, RECOVERY_CODES, RESET_TTL_MS, Redeemed, RevokeReason, Role, SignIn, UnreadableSecret, UserId, UserRecord, WebSignIn,
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
    account_usage_estimates_history_and_trash_without_adding_them,
    account_usage_excludes_chunks_shared_with_current_version,
    account_usage_deduplicates_per_space_and_counts_pending_purge,
    admin_storage_overview_is_aggregated_and_orders_ties,
    admin_storage_overview_handles_an_empty_server,
    admin_storage_estimates_hide_removed_and_unknown_accounts,
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
    physical_sizes_are_unknown_until_recorded_and_totals_say_so,
    blobs_are_walked_in_pages_and_picked_by_id,
    server_values_are_set_and_replaced,
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
    set_up_confirmed_and_listed,
    a_set_up_belongs_to_one_device_expires_and_takes_five_tries,
    changes_of_the_settings_recheck_the_device_and_the_password,
    two_confirmations_at_once_turn_it_on_once,
    a_time_step_is_accepted_once_and_only_later_ones_after_it,
    recovery_codes_work_once_and_new_ones_replace_them,
    wrong_proofs_lock_for_a_while_double_and_reset_on_success,
    wrong_proofs_at_the_same_moment_all_count,
    a_challenge_is_made_counted_used_once_and_carries_the_device,
    an_account_keeps_five_open_challenges_and_expired_ones_go,
    a_challenge_dies_with_every_change_of_the_account,
    two_completions_of_one_challenge_give_one_device,
    completions_and_changes_of_the_account_at_the_same_moment_leave_no_stray_device,
    turning_two_factor_on_and_off_at_the_same_moment_as_a_sign_in,
    an_invite_to_a_protected_account_needs_the_second_factor_in_the_same_transaction,
    a_reset_code_for_a_protected_account_needs_the_factor_unless_it_turns_it_off,
    admins_two_factor_is_checked_in_the_change_and_resets_need_it,
    the_command_line_resets_two_factor_and_the_account_shows_it,
    secrets_are_listed_and_replaced_only_if_unchanged,
    parallel_wrong_proofs_on_one_challenge_count_exactly_five,
    parallel_wrong_proofs_lock_for_exactly_fifteen_minutes,
    two_factor_changes_write_their_events,
    two_factor_sign_in_writes_login_and_counts_failures,
    the_lock_writes_one_event_when_it_is_set,
    a_join_writes_its_two_factor_events,
    resets_and_confirmations_write_their_events,
    web_sessions_are_made_looked_up_expire_and_log_out,
    a_web_session_ends_with_every_change_of_the_accounts_generation,
    a_web_sign_in_with_two_factor_needs_its_own_challenge,
    web_sessions_are_pruned_in_bounded_batches,
    a_web_actor_changes_accounts_until_its_session_ends,
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

async fn physical_sizes_are_unknown_until_recorded_and_totals_say_so(meta: &Meta) {
    let ana = user(meta, "ana").await;
    let (old, new) = (random_chunk(), random_chunk());
    meta.add_chunk(ana.space, &old, 1000, NOW).await.unwrap();
    meta.add_chunk(ana.space, &new, 3000, NOW).await.unwrap();
    // Nothing is known about either yet: unknown, not zero, and the totals say none of it is known.
    let rows = meta.blob_page(None, 10).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.physical_size.is_none() && r.level.is_none()));
    let totals = meta.storage_totals().await.unwrap();
    assert_eq!((totals.blobs, totals.logical_bytes, totals.known_blobs, totals.known_logical_bytes, totals.known_physical_bytes), (2, 4000, 0, 0, 0));

    meta.set_physical(BlobKind::Chunk, new.as_bytes(), 600, 3).await.unwrap();
    let totals = meta.storage_totals().await.unwrap();
    assert_eq!((totals.known_blobs, totals.known_logical_bytes, totals.known_physical_bytes, totals.logical_bytes), (1, 3000, 600, 4000));
    let row = meta.blob_from(BlobKind::Chunk, new.as_bytes()).await.unwrap().unwrap();
    assert_eq!((row.id, row.stored_size, row.physical_size, row.level), (*new.as_bytes(), 3000, Some(600), Some(3)));
    // A measured size leaves the level alone; recording again replaces both.
    meta.set_measured_size(BlobKind::Chunk, old.as_bytes(), 900).await.unwrap();
    let row = meta.blob_from(BlobKind::Chunk, old.as_bytes()).await.unwrap().unwrap();
    assert_eq!((row.physical_size, row.level), (Some(900), None));
    meta.set_physical(BlobKind::Chunk, new.as_bytes(), 500, 19).await.unwrap();
    let row = meta.blob_from(BlobKind::Chunk, new.as_bytes()).await.unwrap().unwrap();
    assert_eq!((row.physical_size, row.level), (Some(500), Some(19)));
    // A rewrite records its result only if the row still holds the size it started from. An upload that replaced the
    // object between the rewrite's read and its record has recorded its own size and level: that stays.
    assert!(!meta.set_physical_if(BlobKind::Chunk, new.as_bytes(), 777, 100, 19).await.unwrap());
    let row = meta.blob_from(BlobKind::Chunk, new.as_bytes()).await.unwrap().unwrap();
    assert_eq!((row.physical_size, row.level), (Some(500), Some(19)));
    assert!(meta.set_physical_if(BlobKind::Chunk, new.as_bytes(), 500, 450, 22).await.unwrap());
    let row = meta.blob_from(BlobKind::Chunk, new.as_bytes()).await.unwrap().unwrap();
    assert_eq!((row.physical_size, row.level), (Some(450), Some(22)));
    // The interleaving itself: the pass read 450, an upload at level 3 recorded 700, then the pass records.
    meta.set_physical(BlobKind::Chunk, new.as_bytes(), 700, 3).await.unwrap();
    assert!(!meta.set_physical_if(BlobKind::Chunk, new.as_bytes(), 450, 300, 19).await.unwrap());
    let row = meta.blob_from(BlobKind::Chunk, new.as_bytes()).await.unwrap().unwrap();
    assert_eq!((row.physical_size, row.level), (Some(700), Some(3)));
    // A row with no size yet is taken (nothing was recorded that could be lost), and an absent row changes nothing.
    let unmeasured = random_chunk();
    meta.add_chunk(ana.space, &unmeasured, 10, NOW).await.unwrap();
    assert!(meta.set_physical_if(BlobKind::Chunk, unmeasured.as_bytes(), 9, 8, 5).await.unwrap());
    assert!(!meta.set_physical_if(BlobKind::Manifest, &[9u8; 32], 1, 1, 1).await.unwrap());
    meta.set_physical(BlobKind::Chunk, new.as_bytes(), 500, 19).await.unwrap();
    // Unknown blobs and absent rows are no error and change nothing; the logical size is never touched.
    meta.set_physical(BlobKind::Manifest, &[9u8; 32], 5, 1).await.unwrap();
    assert_eq!(meta.storage_totals().await.unwrap().blobs, 3);
    assert_eq!(meta.stored_bytes(ana.space).await.unwrap(), 4010);
    // A chunk uploaded again keeps its row (and the size recorded last stays).
    meta.add_chunk(ana.space, &new, 3000, NOW).await.unwrap();
    assert_eq!(meta.blob_from(BlobKind::Chunk, new.as_bytes()).await.unwrap().unwrap().physical_size, Some(500));
}

async fn blobs_are_walked_in_pages_and_picked_by_id(meta: &Meta) {
    let ana = user(meta, "ana").await;
    assert!(meta.blob_page(None, 10).await.unwrap().is_empty());
    assert!(meta.blob_from(BlobKind::Chunk, &[0u8; 32]).await.unwrap().is_none());
    let mut ids: Vec<ChunkId> = (0..7).map(|_| random_chunk()).collect();
    for id in &ids {
        meta.add_chunk(ana.space, id, 100, NOW).await.unwrap();
    }
    ids.sort_by_key(|id| *id.as_bytes());
    // Pages of 3 walk all of them once, in ID order.
    let mut walked = Vec::new();
    let mut after = None;
    loop {
        let page = meta.blob_page(after, 3).await.unwrap();
        let Some(last) = page.last() else { break };
        after = Some((last.kind, last.id));
        assert!(page.len() <= 3);
        walked.extend(page.iter().map(|r| r.id));
    }
    assert_eq!(walked, ids.iter().map(|id| *id.as_bytes()).collect::<Vec<_>>());
    // Picking by ID: the first at or after, and the first of the kind when there is none after.
    let first = *ids[0].as_bytes();
    let last = *ids[6].as_bytes();
    assert_eq!(meta.blob_from(BlobKind::Chunk, &[0u8; 32]).await.unwrap().unwrap().id, first);
    assert_eq!(meta.blob_from(BlobKind::Chunk, &last).await.unwrap().unwrap().id, last);
    assert_eq!(meta.blob_from(BlobKind::Chunk, &[0xffu8; 32]).await.unwrap().unwrap().id, first, "wraps around");
    assert!(meta.blob_from(BlobKind::Manifest, &[0u8; 32]).await.unwrap().is_none(), "kinds are separate");
}

async fn server_values_are_set_and_replaced(meta: &Meta) {
    assert_eq!(meta.server_value("compression_estimate").await.unwrap(), None);
    meta.set_server_value("compression_estimate", "{\"a\":1}", 10).await.unwrap();
    assert_eq!(meta.server_value("compression_estimate").await.unwrap(), Some(("{\"a\":1}".to_owned(), 10)));
    meta.set_server_value("compression_estimate", "two", 20).await.unwrap();
    meta.set_server_value("other", "x", 30).await.unwrap();
    assert_eq!(meta.server_value("compression_estimate").await.unwrap(), Some(("two".to_owned(), 20)));
    assert_eq!(meta.server_value("other").await.unwrap(), Some(("x".to_owned(), 30)));
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

async fn admin_storage_overview_is_aggregated_and_orders_ties(meta: &Meta) {
    let (_, actor) = admin_actor(meta, "root").await;
    let mut accounts = Vec::new();
    for index in 0..13 {
        accounts.push(user(meta, &format!("a{index:02}")).await);
    }
    let cap = 10_000_000;
    let quota = 12_000_000;
    meta.admin_update_user(&actor, accounts[0].id, None, Some(Some(quota)), NOW).await.unwrap();
    for (index, stored) in [quota, 8_999_999, 9_000_000, cap, 10, 10, 10, 10, 10, 10, 10, 10, 10].into_iter().enumerate() {
        meta.add_chunk(accounts[index].space, &ChunkId::from_bytes([index as u8 + 1; 32]), stored, NOW).await.unwrap();
    }

    let report = meta.clone().with_stored_bytes_cap(Some(cap)).admin_storage_overview().await.unwrap();
    assert_eq!(report.accounts, 14);
    assert_eq!(report.stored_bytes, 40_000_089);
    assert_eq!(report.default_quota_bytes, Some(cap));
    assert_eq!(report.accounts_with_override, 1);
    assert_eq!(report.accounts_near_quota, 3, "exactly 90% counts as near");
    assert_eq!(report.accounts_over_quota, 2, "exactly 100% counts as over");
    assert_eq!(
        report.top_accounts.iter().map(|account| account.username.as_str()).collect::<Vec<_>>(),
        ["a00", "a03", "a02", "a01", "a04", "a05", "a06", "a07", "a08", "a09"]
    );
    assert_eq!(report.top_accounts[0].quota_bytes, Some(quota), "account override beats cap");
    let capped = meta.clone().with_stored_bytes_cap(Some(cap));
    assert_eq!(capped.storage_estimate(accounts[0].space).await.unwrap().quota_bytes, Some(quota));
    assert_eq!(capped.storage_estimate(accounts[1].space).await.unwrap().quota_bytes, Some(cap));
    assert_eq!(meta.storage_estimate(accounts[1].space).await.unwrap().quota_bytes, None);
    assert_eq!(report.manifest_index.complete, 0);
    assert_eq!(report.manifest_index.building, 0);
    assert_eq!(report.manifest_index.not_indexed, 0);
}

async fn admin_storage_overview_handles_an_empty_server(meta: &Meta) {
    let report = meta.admin_storage_overview().await.unwrap();
    assert_eq!(report.accounts, 0);
    assert_eq!(report.stored_bytes, 0);
    assert_eq!(report.accounts_with_override, 0);
    assert_eq!(report.accounts_near_quota, 0);
    assert_eq!(report.accounts_over_quota, 0);
    assert!(report.top_accounts.is_empty());
    assert_eq!(report.manifest_index.complete, 0);
    assert_eq!(report.manifest_index.building, 0);
    assert_eq!(report.manifest_index.not_indexed, 0);
}

async fn admin_storage_estimates_hide_removed_and_unknown_accounts(meta: &Meta) {
    let (_, actor) = admin_actor(meta, "root").await;
    let target = user(meta, "target").await;
    let estimate = meta.admin_storage_estimate(target.id).await.unwrap();
    assert_eq!(estimate.quota_bytes, None);
    assert_eq!(estimate.stored_bytes, 0);
    assert!(matches!(meta.admin_storage_estimate(UserId(uuid::Uuid::now_v7())).await, Err(MetaError::NotFound("user"))));
    meta.admin_remove_user(&actor, target.id, NOW).await.unwrap();
    assert!(matches!(meta.admin_storage_estimate(target.id).await, Err(MetaError::NotFound("user"))));
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
        let mut conn = raw(&url).await;
        undo_team_spaces(&mut conn).await;
        // Before the audit log.
        sqlx::query("DROP TABLE web_sessions").execute(&mut conn).await.unwrap();
        sqlx::query("DROP TABLE audit_events").execute(&mut conn).await.unwrap();
        sqlx::query("DROP TABLE login_challenges").execute(&mut conn).await.unwrap();
        sqlx::query("DROP TABLE user_recovery_codes").execute(&mut conn).await.unwrap();
        sqlx::query("DROP TABLE user_two_factor").execute(&mut conn).await.unwrap();
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
        // And before stored sizes were tracked.
        sqlx::query("ALTER TABLE blobs DROP COLUMN physical_size").execute(&mut conn).await.unwrap();
        sqlx::query("ALTER TABLE blobs DROP COLUMN compress_level").execute(&mut conn).await.unwrap();
        sqlx::query("DROP TABLE server_state").execute(&mut conn).await.unwrap();
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
    assert_eq!(versions, 16);
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
    drop_history_trash_and_manifest_index(&mut conn).await;
    // What migrations 0010, 0009 and 0008 added goes first: they run again with 0007 on the next open.
    sqlx::query("DROP TABLE web_sessions").execute(&mut conn).await.unwrap();
    sqlx::query("DROP TABLE audit_events").execute(&mut conn).await.unwrap();
    sqlx::query("DROP TABLE login_challenges").execute(&mut conn).await.unwrap();
    sqlx::query("DROP TABLE user_recovery_codes").execute(&mut conn).await.unwrap();
    sqlx::query("DROP TABLE user_two_factor").execute(&mut conn).await.unwrap();
    sqlx::query("ALTER TABLE invites DROP COLUMN reset_two_factor").execute(&mut conn).await.unwrap();
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
    assert_eq!(schema_version(url).await, 16);
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
    sqlx::query("INSERT INTO schema_version (version) VALUES (16)").execute(&mut conn).await.unwrap();
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
    let actor = AdminActor { user: u.id, via: AdminVia::Device(device), password_hash: ADMIN_HASH.to_owned(), two_factor: false };
    (u, actor)
}

/// The device a device-borne actor acts with.
fn device_of(actor: &AdminActor) -> DeviceId {
    match actor.via {
        AdminVia::Device(device) => device,
        AdminVia::Web { .. } => panic!("a device actor was expected"),
    }
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
    meta.admin_issue_reset(&actor, bob.id, &code(INVITE_CHARS).hash(), false, NOW, RESET_TTL_MS).await.unwrap();
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
    let ana_actor =
        AdminActor { user: ana.id, via: AdminVia::Device(device(meta, &ana).await), password_hash: "$argon2id$stub".to_owned(), two_factor: false };
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
    meta.revoke_device(signed_out.id, device_of(&signed_out_actor)).await.unwrap();
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
    let borrowed = AdminActor { via: AdminVia::Device(device(meta, &victim).await), ..boss.clone() };
    assert!(matches!(issue(borrowed).await, Err(MetaError::DeviceRevoked)));
    // A member who is no admin at all.
    let member_actor = AdminActor {
        user: victim.id,
        via: AdminVia::Device(device(meta, &victim).await),
        password_hash: "$argon2id$stub".to_owned(),
        two_factor: false,
    };
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
    assert!(matches!(meta.admin_issue_reset(&stale_actor, victim.id, &hash, false, NOW, RESET_TTL_MS).await, Err(MetaError::Forbidden(_))));
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
    assert!(matches!(
        meta.admin_issue_reset(&actor, bob.id, &code(INVITE_CHARS).hash(), false, NOW, RESET_TTL_MS).await,
        Err(MetaError::Conflict(_))
    ));
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
    meta.admin_issue_reset(&actor, ana.id, &reset.hash(), false, NOW + 1, RESET_TTL_MS).await.unwrap();
    assert_eq!(meta.invite_needs_password(&invite.hash(), NOW + 2).await.unwrap(), None);
    assert_eq!(meta.invite_needs_password(&reset.hash(), NOW + 2).await.unwrap(), Some(true), "a reset always needs a password, though ana has one");
    let invite_again = code(INVITE_CHARS);
    meta.create_invite("ana", &invite_again.hash(), NOW + 3, INVITE_TTL_MS).await.unwrap();
    assert_eq!(meta.invite_needs_password(&reset.hash(), NOW + 4).await.unwrap(), None, "the command line's invite replaces a reset code");
    assert_eq!(meta.invite_needs_password(&invite_again.hash(), NOW + 4).await.unwrap(), Some(false));
    meta.admin_issue_reset(&actor, ana.id, &reset.hash(), false, NOW + 5, RESET_TTL_MS).await.unwrap();
    meta.admin_revoke_invite(&actor, ana.id, NOW).await.unwrap();
    assert_eq!(meta.invite_needs_password(&reset.hash(), NOW + 6).await.unwrap(), None, "a revoke ends a reset code too");

    // Not oneself, and not another admin.
    assert!(matches!(meta.admin_issue_reset(&actor, root.id, &reset.hash(), false, NOW, RESET_TTL_MS).await, Err(MetaError::Invalid(_))));
    let (boss, _) = admin_actor(meta, "boss").await;
    let result = meta.admin_issue_reset(&actor, boss.id, &reset.hash(), false, NOW, RESET_TTL_MS).await;
    assert!(matches!(result, Err(MetaError::Forbidden(_))), "{result:?}");
    assert_eq!(meta.invite_needs_password(&reset.hash(), NOW + 6).await.unwrap(), None);
    // Not a disabled account.
    meta.admin_disable_user(&actor, ana.id, NOW).await.unwrap();
    assert!(matches!(meta.admin_issue_reset(&actor, ana.id, &reset.hash(), false, NOW, RESET_TTL_MS).await, Err(MetaError::Conflict(_))));
    assert!(matches!(
        meta.admin_issue_reset(&actor, UserId(uuid::Uuid::now_v7()), &reset.hash(), false, NOW, RESET_TTL_MS).await,
        Err(MetaError::NotFound(_))
    ));
}

async fn a_reset_code_sets_a_password_and_signs_everything_out(meta: &Meta) {
    let (_, actor) = admin_actor(meta, "root").await;
    let (ana, maker, pairing, pairing_id) = paired(meta, "ana").await;
    let other = device(meta, &ana).await;
    let reset = code(INVITE_CHARS);
    meta.admin_issue_reset(&actor, ana.id, &reset.hash(), false, NOW, RESET_TTL_MS).await.unwrap();
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
    meta.admin_issue_reset(&actor, ana.id, &late.hash(), false, NOW + 10, RESET_TTL_MS).await.unwrap();
    let result = meta.redeem(&late.hash(), "late", &token(), Some("$argon2id$x"), NOW + 10 + RESET_TTL_MS).await;
    assert!(matches!(result, Err(MetaError::InvalidCode)), "{result:?}");
    assert_eq!(RESET_TTL_MS, 3_600_000);
    meta.redeem(&late.hash(), "in time", &token(), Some("$argon2id$x"), NOW + 10 + RESET_TTL_MS - 1).await.unwrap();

    // Disabling the account deletes the reset code it still held.
    let disabled_code = code(INVITE_CHARS);
    meta.admin_issue_reset(&actor, ana.id, &disabled_code.hash(), false, NOW + 20, RESET_TTL_MS).await.unwrap();
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

/// An account, its invite and the audit log survive the two-factor migration (0010, which runs after the audit log's 0009).
async fn migrating_from_0009_keeps_the_rows(url: &str) {
    use sqlx::{Connection, Row};
    sqlx::any::install_default_drivers();
    let (ana, invite, audit_rows) = {
        let meta = Meta::open(url).await.unwrap();
        let ana = user(&meta, "ana").await;
        let invite = code(INVITE_CHARS);
        meta.create_invite("ana", &invite.hash(), NOW, INVITE_TTL_MS).await.unwrap();
        meta.sign_in_device(ana.id, (&ana.password_hash, ana.auth_gen), "pc", &DeviceClient::default(), &token(), NOW).await.unwrap().unwrap();
        let events = all_events(&meta).await;
        assert!(!events.is_empty(), "the audit log has rows to keep");
        (ana, invite, events.len())
    };
    let mut conn = sqlx::AnyConnection::connect(url).await.unwrap();
    drop_history_trash_and_manifest_index(&mut conn).await;
    sqlx::query("DROP TABLE web_sessions").execute(&mut conn).await.unwrap();
    sqlx::query("DROP TABLE login_challenges").execute(&mut conn).await.unwrap();
    sqlx::query("DROP TABLE user_recovery_codes").execute(&mut conn).await.unwrap();
    sqlx::query("DROP TABLE user_two_factor").execute(&mut conn).await.unwrap();
    sqlx::query("ALTER TABLE invites DROP COLUMN reset_two_factor").execute(&mut conn).await.unwrap();
    sqlx::query("DELETE FROM schema_version WHERE version >= 10").execute(&mut conn).await.unwrap();
    drop(conn);
    assert_eq!(schema_version(url).await, 9, "the audit log's migration stays");
    let meta = Meta::open(url).await.unwrap();
    assert_eq!(schema_version(url).await, 16);
    assert_eq!(all_events(&meta).await.len(), audit_rows, "the audit rows are kept");
    assert_eq!(meta.user_by_name("ana").await.unwrap().unwrap().id, ana.id);
    assert_eq!(meta.invite_needs_password(&invite.hash(), NOW + 1).await.unwrap(), Some(false));
    let mut conn = sqlx::AnyConnection::connect(url).await.unwrap();
    let row = sqlx::query("SELECT reset_two_factor FROM invites").fetch_one(&mut conn).await.unwrap();
    assert_eq!(row.try_get::<i64, _>("reset_two_factor").unwrap(), 0);
}

#[tokio::test]
async fn migrating_from_0009_keeps_the_rows_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    migrating_from_0009_keeps_the_rows(&format!("sqlite:{}", dir.path().join("meta.db").display())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn migrating_from_0009_keeps_the_rows_on_postgres() {
    let Some((url, _guard)) = postgres_database().await else {
        eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set");
        return;
    };
    migrating_from_0009_keeps_the_rows(&url).await;
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
    assert_eq!(latest, 16);
    // Opening again changes nothing.
    drop(Meta::open(&url).await.unwrap());
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM schema_version").fetch_one(&mut conn).await.unwrap();
    assert_eq!(rows, 16);
    sqlx::query("INSERT INTO schema_version (version) VALUES (16)").execute(&mut conn).await.unwrap();
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

fn sorted_nodes(mut ids: Vec<NodeId>) -> Vec<NodeId> {
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
    assert_eq!(sorted_nodes(trash_ids(meta, &u).await), sorted_nodes(vec![dir, exact]));

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
    assert_eq!(sorted_nodes(trash_ids(meta, &u).await), sorted_nodes(vec![exact, files[0], files[1]]), "the rest is listed on its own");
    small.restore_from_trash(u.space, dev, files[1], NOW).await.unwrap();
    assert_eq!(sorted_nodes(trash_ids(meta, &u).await), sorted_nodes(vec![exact, files[0]]));

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
    assert_eq!(sorted_nodes(trash_ids(meta, &u).await), sorted_nodes(vec![files[0], files[1]]));
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

/// Undoes migration 0016 (team spaces): the `spaces` table goes back to its shape in 0015, which older migration tests start
/// from. The version row is the caller's to delete. SQLite rebuilds the table, as the migration did.
async fn undo_team_spaces(conn: &mut sqlx::AnyConnection) {
    let sqlite = sqlx::query("SELECT 1 FROM sqlite_master LIMIT 1").execute(&mut *conn).await.is_ok();
    let statements: &[&str] = if sqlite {
        &[
            "PRAGMA foreign_keys = OFF",
            "DROP TABLE space_members",
            "CREATE TABLE spaces_old (id BLOB PRIMARY KEY, owner BLOB NOT NULL UNIQUE REFERENCES users (id), epoch BLOB NOT NULL, seq BIGINT NOT NULL,              oldest_seq BIGINT NOT NULL, root BLOB NOT NULL, stored_bytes BIGINT NOT NULL DEFAULT 0, live_bytes BIGINT NOT NULL DEFAULT 0, quota_bytes BIGINT)",
            "INSERT INTO spaces_old SELECT id, owner, epoch, seq, oldest_seq, root, stored_bytes, live_bytes, quota_bytes FROM spaces",
            "DROP TABLE spaces",
            "ALTER TABLE spaces_old RENAME TO spaces",
            "PRAGMA foreign_keys = ON",
        ]
    } else {
        &[
            "DROP TABLE space_members",
            "DROP INDEX spaces_personal_owner",
            "DROP INDEX spaces_team_name",
            "ALTER TABLE spaces DROP CONSTRAINT spaces_owner_by_kind",
            "ALTER TABLE spaces DROP CONSTRAINT spaces_name_by_kind",
            "ALTER TABLE spaces DROP COLUMN kind",
            "ALTER TABLE spaces DROP COLUMN name",
            "ALTER TABLE spaces DROP COLUMN name_fold",
            "ALTER TABLE spaces DROP COLUMN created_ms",
            "ALTER TABLE spaces DROP COLUMN archived_ms",
            "ALTER TABLE spaces ALTER COLUMN owner SET NOT NULL",
            "ALTER TABLE spaces ADD CONSTRAINT spaces_owner_key UNIQUE (owner)",
        ]
    };
    for statement in statements.iter().copied() {
        sqlx::query(statement).execute(&mut *conn).await.unwrap();
    }
}

/// Removes the last three migrations and their version rows, so older migration tests can start from the version they name.
async fn drop_history_trash_and_manifest_index(conn: &mut sqlx::AnyConnection) {
    undo_team_spaces(conn).await;
    for statement in [
        "DROP TABLE manifest_chunks",
        "DROP TABLE manifest_index",
        "DROP INDEX nodes_trash",
        "ALTER TABLE nodes DROP COLUMN deleted_ms",
        "ALTER TABLE nodes DROP COLUMN purge_requested_ms",
        "ALTER TABLE blobs DROP COLUMN physical_size",
        "ALTER TABLE blobs DROP COLUMN compress_level",
        "DROP TABLE server_state",
        "DROP INDEX versions_node_seq",
        "DELETE FROM schema_version WHERE version >= 12",
    ] {
        sqlx::query(statement).execute(&mut *conn).await.unwrap();
    }
}

/// Removes the trash and manifest-index migrations but retains the version-history migration.
async fn drop_trash_and_manifest_index(conn: &mut sqlx::AnyConnection) {
    undo_team_spaces(conn).await;
    for statement in [
        "DROP TABLE manifest_chunks",
        "DROP TABLE manifest_index",
        "DROP INDEX nodes_trash",
        "ALTER TABLE nodes DROP COLUMN deleted_ms",
        "ALTER TABLE blobs DROP COLUMN physical_size",
        "ALTER TABLE blobs DROP COLUMN compress_level",
        "DROP TABLE server_state",
        "ALTER TABLE nodes DROP COLUMN purge_requested_ms",
        "DELETE FROM schema_version WHERE version >= 13",
    ] {
        sqlx::query(statement).execute(&mut *conn).await.unwrap();
    }
}

/// Puts the database back to the state before the trash migration: no columns, no index, one version fewer.
async fn undo_the_trash_migration(url: &str) {
    let mut conn = raw(url).await;
    drop_trash_and_manifest_index(&mut conn).await;
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
    assert_eq!(latest, 16);
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

/// Drops what migration 0008 added and forgets it ran, which leaves the database as 0007 left it (with its rows).
async fn back_to_0007(url: &str) {
    use sqlx::Connection;
    let mut conn = sqlx::AnyConnection::connect(url).await.unwrap();
    drop_history_trash_and_manifest_index(&mut conn).await;
    // The audit log (migration 0009) and two-factor sign-in (migration 0010) go first: they run again with 0008 on the next open.
    sqlx::query("DROP TABLE web_sessions").execute(&mut conn).await.unwrap();
    sqlx::query("DROP TABLE audit_events").execute(&mut conn).await.unwrap();
    sqlx::query("DROP TABLE login_challenges").execute(&mut conn).await.unwrap();
    sqlx::query("DROP TABLE user_recovery_codes").execute(&mut conn).await.unwrap();
    sqlx::query("DROP TABLE user_two_factor").execute(&mut conn).await.unwrap();
    sqlx::query("ALTER TABLE invites DROP COLUMN reset_two_factor").execute(&mut conn).await.unwrap();
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
    assert_eq!(schema_version(url).await, 16);
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
    sqlx::query("INSERT INTO schema_version (version) VALUES (16)").execute(&mut conn).await.unwrap();
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
        (AuditAction::AdminUserCreated, Some(root.id), Some(device_of(&actor)), Some(newbie.id.0))
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

    // The server-wide compression level: one event with both levels and no target. The same level again writes none (the
    // value is stored anyway); outside 1 to 19 is refused; a stale confirmation changes nothing.
    meta.admin_set_compression_level(&actor, 3, 19, NOW).await.unwrap();
    let e = seen.one().await;
    assert_eq!(
        (e.action, e.actor_user, e.target_id, details(&e)),
        (AuditAction::AdminCompressionChanged, Some(root.id), None, json(r#"{"level_from":3,"level_to":19}"#))
    );
    meta.admin_set_compression_level(&actor, 19, 19, NOW).await.unwrap();
    assert!(seen.actions().await.is_empty());
    assert!(matches!(meta.admin_set_compression_level(&actor, 19, 20, NOW).await, Err(MetaError::Invalid(_))));
    assert!(matches!(meta.admin_set_compression_level(&actor, 19, 0, NOW).await, Err(MetaError::Invalid(_))));
    let stale = AdminActor { password_hash: "$argon2id$old".to_owned(), ..actor.clone() };
    assert!(matches!(meta.admin_set_compression_level(&stale, 19, 5, NOW).await, Err(MetaError::Conflict(_))));
    assert_eq!(meta.server_value("compression_level").await.unwrap().map(|v| v.0), Some("19".to_owned()));
    assert!(seen.actions().await.is_empty());

    // A reset code, and its redemption: the devices it signs out are counted in the one event.
    let reset = code(INVITE_CHARS);
    meta.admin_issue_reset(&actor, ana.id, &reset.hash(), false, NOW, RESET_TTL_MS).await.unwrap();
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
        (AuditAction::AdminDeviceRevoked, Some(root.id), Some(device_of(&actor)), "user", Some(ana.id.0))
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
    meta.admin_issue_reset(&actor, ana.id, &reset.hash(), false, NOW, RESET_TTL_MS).await.unwrap();
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

/// A sign-in step that failed is one row per account, action, client kind and minute, whose count goes up and whose identity, time and
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
    meta.audit_record_web_failure(login(Some(ana.id)), minute + 1_000).await.unwrap();
    let rows = all_events(meta).await;
    assert_eq!(rows.len(), 2, "device and web failure aggregates stay separate");
    assert!(rows.iter().any(|event| details(event) == json(r#"{"client_kind":"web","step":"password"}"#)));
    // The same minute: the count goes up, nothing else changes.
    meta.audit_record_failure(login(Some(ana.id)), minute + 59_000).await.unwrap();
    meta.audit_record_failure(login(Some(ana.id)), minute + 59_999).await.unwrap();
    let rows = all_events(meta).await;
    assert_eq!(rows.len(), 2);
    let device_row = rows.iter().find(|event| event.id == first.id).unwrap();
    assert_eq!((device_row.id, device_row.at_ms, device_row.count, &device_row.details), (first.id, minute + 1_000, 3, &first.details));
    // The next minute is a new row; another account, another action, another row.
    meta.audit_record_failure(login(Some(ana.id)), minute + 60_000).await.unwrap();
    meta.audit_record_failure(login(Some(bob.id)), minute + 1_000).await.unwrap();
    meta.audit_record_failure(AuditFailure { action: AuditAction::AuthSecondFactorFailed, target: Some(ana.id), step: Step::Totp }, minute + 1_000)
        .await
        .unwrap();
    let rows = all_events(meta).await;
    assert_eq!(rows.len(), 5);
    let mut counts: Vec<(&str, u64)> = rows.iter().map(|e| (e.action.as_str(), e.count)).collect();
    counts.sort_unstable();
    assert_eq!(
        counts,
        [("auth.login_failed", 1), ("auth.login_failed", 1), ("auth.login_failed", 1), ("auth.login_failed", 3), ("auth.second_factor_failed", 1)]
    );
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
    is_db(meta.admin_issue_reset(&actor, ana.id, &code(INVITE_CHARS).hash(), false, NOW, RESET_TTL_MS).await.map(|_| ()));
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

// ---- the migration from 0008 to 0009 (and on to 0010) ----

/// Drops what migrations 0009 (the audit log) and 0010 (two-factor sign-in) added and forgets they ran, which leaves the database as 0008 left it (with its rows).
async fn back_to_0008(url: &str) {
    use sqlx::Connection;
    let mut conn = sqlx::AnyConnection::connect(url).await.unwrap();
    drop_history_trash_and_manifest_index(&mut conn).await;
    sqlx::query("DROP TABLE web_sessions").execute(&mut conn).await.unwrap();
    sqlx::query("DROP TABLE audit_events").execute(&mut conn).await.unwrap();
    sqlx::query("DROP TABLE login_challenges").execute(&mut conn).await.unwrap();
    sqlx::query("DROP TABLE user_recovery_codes").execute(&mut conn).await.unwrap();
    sqlx::query("DROP TABLE user_two_factor").execute(&mut conn).await.unwrap();
    sqlx::query("ALTER TABLE invites DROP COLUMN reset_two_factor").execute(&mut conn).await.unwrap();
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
    assert_eq!(schema_version(url).await, 16, "all migrations after the audit log run");
    assert_eq!(meta.device_by_token(&old_token).await.unwrap().map(|a| a.user), Some(ana.id));
    assert!(all_events(&meta).await.is_empty(), "a new log is empty");
    meta.sign_in_device(ana.id, (&ana.password_hash, ana.auth_gen), "pc", &DeviceClient::default(), &token(), NOW).await.unwrap().unwrap();
    meta.audit_record_failure(AuditFailure { action: AuditAction::AuthLoginFailed, target: Some(ana.id), step: Step::Password }, NOW).await.unwrap();
    assert_eq!(all_events(&meta).await.len(), 2);

    use sqlx::Connection;
    let mut conn = sqlx::AnyConnection::connect(url).await.unwrap();
    sqlx::query("INSERT INTO schema_version (version) VALUES (16)").execute(&mut conn).await.unwrap();
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

// ---- two-factor sign-in (migration 0010) ----

type Check = Box<dyn Fn(UserId, &[u8]) -> Result<Option<i64>, UnreadableSecret> + Send + Sync>;

/// A TOTP check that says every code is right for time step `step` (the crypto is the server's; here only the order of
/// operations and what the database does with the answer are under test).
fn right(step: i64) -> Check {
    Box::new(move |_, _| Ok(Some(step)))
}

/// A TOTP check that says every code is wrong.
fn wrong() -> Check {
    Box::new(|_, _| Ok(None))
}

fn totp(check: &Check) -> Proof<'_> {
    Proof::Totp(&**check)
}

/// The admin's confirmation with a TOTP check that says `step` (`None`: a wrong code).
async fn confirm_with(meta: &Meta, user: UserId, step: Option<i64>, now: i64) -> Result<bool, MetaError> {
    let check = step.map_or_else(wrong, right);
    meta.confirm_second_factor(user, Some(&totp(&check)), now).await
}

async fn redeem_with_factor(meta: &Meta, hash: &[u8; 32], proof: Option<&Proof<'_>>, now: i64) -> Result<DeviceId, MetaError> {
    meta.redeem_with_second_factor(hash, "new pc", &DeviceClient::default(), &token(), None, proof, now).await.map(|r| r.device)
}

const FIRST_STEP: i64 = 1_000;
const SETUP_AT: i64 = NOW + 1_000;
const MINUTE: i64 = 60_000;

/// An account with a password, two devices (`first` asked for the set-up and stays signed in, `second` is signed out by
/// turning it on), and two-factor sign-in on from [`SETUP_AT`] (its first code was step [`FIRST_STEP`]).
struct Protected {
    user: UserRecord,
    first: DeviceId,
    second: DeviceId,
    codes: Vec<Code>,
}

impl Protected {
    fn actor(&self) -> DeviceActor {
        DeviceActor { user: self.user.id, device: self.first, password_hash: "$argon2id$stub".to_owned() }
    }

    fn hashes(&self) -> Vec<[u8; 32]> {
        self.codes.iter().map(Code::hash).collect()
    }
}

async fn protected(meta: &Meta, username: &str) -> Protected {
    let user = user(meta, username).await;
    let first = device(meta, &user).await;
    let second = device(meta, &user).await;
    let codes: Vec<Code> = (0..10).map(|_| code(INVITE_CHARS)).collect();
    let me = Protected { user, first, second, codes };
    meta.two_factor_begin_setup(&me.actor(), b"cipher-of-the-secret", SETUP_AT).await.unwrap();
    let revoked = meta.two_factor_confirm(me.user.id, me.first, &*right(FIRST_STEP), &me.hashes(), SETUP_AT + 1).await.unwrap();
    assert_eq!(revoked, vec![me.second]);
    me
}

fn new_challenge() -> NewChallenge {
    NewChallenge { id_hash: token(), client_key: "ip:203.0.113.7".to_owned() }
}

async fn live_devices(meta: &Meta, user: &UserRecord) -> Vec<DeviceId> {
    meta.list_devices(user.id).await.unwrap().into_iter().filter(|d| !d.revoked).map(|d| d.id).collect()
}

async fn set_up_confirmed_and_listed(meta: &Meta) {
    let ana = user(meta, "ana").await;
    let (first, second) = (device(meta, &ana).await, device(meta, &ana).await);
    let info = meta.two_factor_info(ana.id, SETUP_AT).await.unwrap();
    assert_eq!((info.enabled_ms, info.recovery_codes_left, info.pending), (None, 0, false));
    assert!(!meta.two_factor_enabled(ana.id).await.unwrap());
    meta.create_invite("ana", &code(INVITE_CHARS).hash(), NOW, INVITE_TTL_MS).await.unwrap();
    let pairing = code(PAIRING_CHARS);
    meta.create_pairing(ana.id, second, &pairing.hash(), NOW).await.unwrap();

    let actor = DeviceActor { user: ana.id, device: first, password_hash: "$argon2id$stub".to_owned() };
    let expires = meta.two_factor_begin_setup(&actor, b"cipher-of-the-secret", SETUP_AT).await.unwrap();
    assert_eq!(expires, SETUP_AT + 10 * MINUTE);
    let info = meta.two_factor_info(ana.id, SETUP_AT + 1).await.unwrap();
    assert_eq!((info.enabled_ms, info.pending), (None, true), "a set-up is not two-factor sign-in");
    assert!(!meta.two_factor_enabled(ana.id).await.unwrap());
    // A password sign-in still gives a device while it is only set up.
    let generation = gen_of(meta, "ana").await;
    let signed =
        meta.sign_in(ana.id, ("$argon2id$stub", generation), "pc", &DeviceClient::default(), &token(), &new_challenge(), SETUP_AT).await.unwrap();
    assert!(matches!(signed, Some(SignIn::Device(_))));
    let extra = live_devices(meta, &ana).await.len();
    assert_eq!(extra, 3);

    // A wrong first code is refused and the set-up stays.
    assert!(matches!(meta.two_factor_confirm(ana.id, first, &*wrong(), &[], SETUP_AT + 2).await, Err(MetaError::TwoFactorInvalid)));
    assert!(!meta.two_factor_enabled(ana.id).await.unwrap());
    let hashes: Vec<[u8; 32]> = (0..RECOVERY_CODES).map(|_| code(INVITE_CHARS).hash()).collect();
    let before = gen_of(meta, "ana").await;
    let revoked = meta.two_factor_confirm(ana.id, first, &*right(FIRST_STEP), &hashes, SETUP_AT + 3).await.unwrap();
    assert_eq!(revoked.len(), 2, "every other device is signed out");
    assert_eq!(live_devices(meta, &ana).await, vec![first]);
    let devices = meta.list_devices(ana.id).await.unwrap();
    assert!(devices.iter().filter(|d| d.revoked).all(|d| d.revoke_reason == Some(RevokeReason::TwoFactor)));
    assert_revoke_columns_agree(&devices);
    assert_eq!(gen_of(meta, "ana").await, before + 1, "what a sign-in checked before no longer counts");
    assert!(!meta.revoke_invite("ana").await.unwrap(), "the invite is gone");
    assert!(matches!(meta.redeem(&pairing.hash(), "x", &token(), None, SETUP_AT + 4).await, Err(MetaError::InvalidCode)), "pairings are deleted");
    let info = meta.two_factor_info(ana.id, SETUP_AT + 5).await.unwrap();
    assert_eq!((info.enabled_ms, info.recovery_codes_left, info.pending), (Some(SETUP_AT + 3), 10, false));
    assert!(meta.two_factor_enabled(ana.id).await.unwrap());
    assert!(meta.account(ana.id).await.unwrap().two_factor_enabled);
    // It is on: a set-up is a conflict, and so is a second confirmation.
    assert!(matches!(meta.two_factor_begin_setup(&actor, b"other", SETUP_AT + 6).await, Err(MetaError::Conflict(_))));
    assert!(matches!(meta.two_factor_confirm(ana.id, first, &*right(FIRST_STEP + 5), &hashes, SETUP_AT + 7).await, Err(MetaError::Conflict(_))));
}

async fn a_set_up_belongs_to_one_device_expires_and_takes_five_tries(meta: &Meta) {
    let ana = user(meta, "ana").await;
    let (first, second) = (device(meta, &ana).await, device(meta, &ana).await);
    let actor = |device| DeviceActor { user: ana.id, device, password_hash: "$argon2id$stub".to_owned() };
    meta.two_factor_begin_setup(&actor(first), b"one", SETUP_AT).await.unwrap();
    // Open on the first device: the second may not start, nor confirm (and its code is not tried at all).
    assert!(matches!(meta.two_factor_begin_setup(&actor(second), b"two", SETUP_AT + 1).await, Err(MetaError::TwoFactorPending)));
    assert!(matches!(meta.two_factor_confirm(ana.id, second, &*right(FIRST_STEP), &[], SETUP_AT + 1).await, Err(MetaError::Conflict(_))));
    // The first may start again; that replaces the secret and gives it five new tries.
    for round in 0..2 {
        meta.two_factor_begin_setup(&actor(first), b"again", SETUP_AT + 2).await.unwrap();
        for _ in 0..4 {
            assert!(
                matches!(meta.two_factor_confirm(ana.id, first, &*wrong(), &[], SETUP_AT + 3).await, Err(MetaError::TwoFactorInvalid)),
                "round {round}"
            );
        }
        assert!(meta.two_factor_info(ana.id, SETUP_AT + 3).await.unwrap().pending, "four wrong codes leave it open");
    }
    // The fifth wrong code uses it up: even the right code then is a conflict, and the set-up has to start again.
    assert!(matches!(meta.two_factor_confirm(ana.id, first, &*wrong(), &[], SETUP_AT + 3).await, Err(MetaError::TwoFactorInvalid)));
    assert!(!meta.two_factor_info(ana.id, SETUP_AT + 3).await.unwrap().pending);
    assert!(matches!(meta.two_factor_confirm(ana.id, first, &*right(FIRST_STEP), &[], SETUP_AT + 4).await, Err(MetaError::Conflict(_))));
    // After the ten minutes it ran out; another device may start one.
    meta.two_factor_begin_setup(&actor(first), b"fresh", SETUP_AT + 5).await.unwrap();
    assert!(matches!(
        meta.two_factor_confirm(ana.id, first, &*right(FIRST_STEP), &[], SETUP_AT + 5 + 10 * MINUTE).await,
        Err(MetaError::Conflict(_))
    ));
    assert!(!meta.two_factor_info(ana.id, SETUP_AT + 5 + 10 * MINUTE).await.unwrap().pending);
    meta.two_factor_begin_setup(&actor(second), b"second's", SETUP_AT + 6 + 10 * MINUTE).await.unwrap();
    assert!(!meta.two_factor_enabled(ana.id).await.unwrap(), "nothing was ever turned on");
}

/// Set-up, confirm, disable and new recovery codes check, under the account's lock, that the device is still signed in and
/// that the password that was verified is still the account's.
async fn changes_of_the_settings_recheck_the_device_and_the_password(meta: &Meta) {
    let me = protected(meta, "ana").await;
    let revoked_actor = DeviceActor { device: me.second, ..me.actor() };
    let stale_actor = DeviceActor { password_hash: "$argon2id$other".to_owned(), ..me.actor() };
    let hashes = me.hashes();
    for (name, actor, expect_gone) in [("revoked device", &revoked_actor, true), ("changed password", &stale_actor, false)] {
        let results = [
            meta.two_factor_begin_setup(actor, b"x", SETUP_AT + 10).await.map(|_| ()),
            meta.two_factor_disable(actor, &Proof::Recovery(hashes[0]), SETUP_AT + 10).await.map(|_| ()),
            meta.two_factor_new_recovery_codes(actor, &totp(&right(FIRST_STEP + 50)), &hashes, SETUP_AT + 10).await,
        ];
        for result in results {
            if expect_gone {
                assert!(matches!(result, Err(MetaError::DeviceRevoked)), "{name}: {result:?}");
            } else {
                assert!(matches!(result, Err(MetaError::Conflict(_))), "{name}: {result:?}");
            }
        }
    }
    // Confirming from a device that was signed out is refused too.
    assert!(matches!(meta.two_factor_confirm(me.user.id, me.second, &*right(FIRST_STEP), &[], SETUP_AT + 11).await, Err(MetaError::DeviceRevoked)));
    // Nothing changed: it is still on, with all ten codes, and no proof was spent.
    let info = meta.two_factor_info(me.user.id, SETUP_AT + 12).await.unwrap();
    assert_eq!((info.enabled_ms.is_some(), info.recovery_codes_left), (true, 10));
    // A disabled account's device is gone as well.
    meta.reset_password(me.user.id, "$argon2id$stub", false, SETUP_AT + 13).await.unwrap();
    assert!(matches!(meta.two_factor_begin_setup(&me.actor(), b"x", SETUP_AT + 14).await, Err(MetaError::DeviceRevoked)));
}

async fn two_confirmations_at_once_turn_it_on_once(meta: &Meta) {
    for round in 0..8 {
        let ana = user(meta, &format!("ana{round}")).await;
        let first = device(meta, &ana).await;
        let actor = DeviceActor { user: ana.id, device: first, password_hash: "$argon2id$stub".to_owned() };
        meta.two_factor_begin_setup(&actor, b"secret", SETUP_AT).await.unwrap();
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let (meta, id) = (meta.clone(), ana.id);
            let hashes: Vec<[u8; 32]> = (0..RECOVERY_CODES).map(|_| code(INVITE_CHARS).hash()).collect();
            tasks.push(tokio::spawn(async move { meta.two_factor_confirm(id, first, &*right(FIRST_STEP), &hashes, SETUP_AT + 1).await }));
        }
        let mut turned_on = 0;
        for task in tasks {
            match task.await.unwrap() {
                Ok(_) => turned_on += 1,
                Err(MetaError::Conflict(_)) => {}
                other => panic!("round {round}: {other:?}"),
            }
        }
        assert_eq!(turned_on, 1, "round {round}");
        assert_eq!(meta.two_factor_info(ana.id, SETUP_AT + 2).await.unwrap().recovery_codes_left, 10, "one set of codes, round {round}");
    }
}

/// The step of a code is accepted once, and only a later one after it. The admin's confirmation and the sign-in share it.
async fn a_time_step_is_accepted_once_and_only_later_ones_after_it(meta: &Meta) {
    let me = protected(meta, "ana").await;
    let id = me.user.id;
    let at = SETUP_AT + 100;
    // The first code of the set-up was step 1000: it can't be used again, nor an earlier step.
    for step in [FIRST_STEP, FIRST_STEP - 1, 0] {
        assert!(matches!(confirm_with(meta, id, Some(step), at).await, Err(MetaError::TwoFactorInvalid)), "step {step}");
    }
    assert!(matches!(confirm_with(meta, id, Some(FIRST_STEP + 1), at).await, Ok(true)));
    assert!(matches!(confirm_with(meta, id, Some(FIRST_STEP + 1), at).await, Err(MetaError::TwoFactorInvalid)), "the same step twice");
    assert!(matches!(confirm_with(meta, id, None, at).await, Err(MetaError::TwoFactorInvalid)));
    // Eight requests with one code at once: exactly one gets through.
    for round in 0..6 {
        let step = FIRST_STEP + 10 + round;
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let meta = meta.clone();
            tasks.push(tokio::spawn(async move { confirm_with(&meta, id, Some(step), at).await }));
        }
        let mut accepted = 0;
        for task in tasks {
            match task.await.unwrap() {
                Ok(true) => accepted += 1,
                Err(MetaError::TwoFactorInvalid | MetaError::TwoFactorLocked { .. }) => {}
                other => panic!("round {round}: {other:?}"),
            }
        }
        assert_eq!(accepted, 1, "round {round}: one time step, one use");
    }
    // An account without two-factor sign-in asks for nothing, and a missing proof for one with it is refused unspent.
    let bob = user(meta, "bob").await;
    assert!(matches!(meta.confirm_second_factor(bob.id, None, at).await, Ok(false)));
    assert!(matches!(confirm_with(meta, bob.id, Some(1), at).await, Ok(false)));
    assert!(matches!(meta.confirm_second_factor(id, None, at).await, Err(MetaError::TwoFactorInvalid)));
}

async fn recovery_codes_work_once_and_new_ones_replace_them(meta: &Meta) {
    let me = protected(meta, "ana").await;
    let (id, hashes) = (me.user.id, me.hashes());
    let at = SETUP_AT + 100;
    assert!(matches!(meta.confirm_second_factor(id, Some(&Proof::Recovery(hashes[0])), at).await, Ok(true)));
    assert!(matches!(meta.confirm_second_factor(id, Some(&Proof::Recovery(hashes[0])), at).await, Err(MetaError::TwoFactorInvalid)));
    assert!(matches!(meta.confirm_second_factor(id, Some(&Proof::Recovery(code(INVITE_CHARS).hash())), at).await, Err(MetaError::TwoFactorInvalid)));
    assert_eq!(meta.two_factor_info(id, at).await.unwrap().recovery_codes_left, 9);
    // The same code at the same moment: one wins.
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let (meta, hash) = (meta.clone(), hashes[1]);
        tasks.push(tokio::spawn(async move { meta.confirm_second_factor(id, Some(&Proof::Recovery(hash)), at).await }));
    }
    let mut accepted = 0;
    for task in tasks {
        if matches!(task.await.unwrap(), Ok(true)) {
            accepted += 1;
        }
    }
    assert_eq!(accepted, 1);
    // New codes: the old ones, used or not, are dead, and the new ones all live.
    let new: Vec<[u8; 32]> = (0..RECOVERY_CODES).map(|_| code(INVITE_CHARS).hash()).collect();
    meta.two_factor_new_recovery_codes(&me.actor(), &totp(&right(FIRST_STEP + 1)), &new, at).await.unwrap();
    assert_eq!(meta.two_factor_info(id, at).await.unwrap().recovery_codes_left, 10);
    assert!(matches!(meta.confirm_second_factor(id, Some(&Proof::Recovery(hashes[5])), at).await, Err(MetaError::TwoFactorInvalid)));
    assert!(matches!(meta.confirm_second_factor(id, Some(&Proof::Recovery(new[3])), at).await, Ok(true)));
    // A repeated code can't be stored (the primary key), and then nothing changes: the old set is still there.
    let again = vec![new[0]; 2];
    assert!(meta.two_factor_new_recovery_codes(&me.actor(), &totp(&right(FIRST_STEP + 2)), &again, at).await.is_err());
    assert_eq!(meta.two_factor_info(id, at).await.unwrap().recovery_codes_left, 9);
    assert!(matches!(meta.confirm_second_factor(id, Some(&Proof::Recovery(new[7])), at).await, Ok(true)));
    // A wrong proof for new codes changes none.
    assert!(matches!(meta.two_factor_new_recovery_codes(&me.actor(), &totp(&wrong()), &new, at).await, Err(MetaError::TwoFactorInvalid)));
}

/// Ten wrong proofs in a row lock the second factor for 15 minutes; each wrong one after that doubles it, to 24 hours; a
/// right one resets all of it. While locked even a right proof is refused and nothing is counted.
async fn wrong_proofs_lock_for_a_while_double_and_reset_on_success(meta: &Meta) {
    let me = protected(meta, "ana").await;
    let id = me.user.id;
    let mut now = SETUP_AT + 100;
    for n in 1..=9 {
        assert!(matches!(confirm_with(meta, id, None, now).await, Err(MetaError::TwoFactorInvalid)), "wrong proof {n}");
    }
    // The tenth locks.
    assert!(matches!(confirm_with(meta, id, None, now).await, Err(MetaError::TwoFactorInvalid)));
    let locked = confirm_with(meta, id, Some(FIRST_STEP + 1), now + 1_000).await;
    assert!(matches!(locked, Err(MetaError::TwoFactorLocked { retry_after_ms }) if retry_after_ms == 15 * MINUTE - 1_000), "{locked:?}");
    // Every path refuses while it lasts: recovery codes, disable, new codes, a challenge.
    let actor = me.actor();
    assert!(matches!(meta.confirm_second_factor(id, Some(&Proof::Recovery(me.hashes()[0])), now + 2).await, Err(MetaError::TwoFactorLocked { .. })));
    assert!(matches!(meta.two_factor_disable(&actor, &Proof::Recovery(me.hashes()[1]), now + 2).await, Err(MetaError::TwoFactorLocked { .. })));
    assert!(matches!(
        meta.two_factor_new_recovery_codes(&actor, &totp(&right(5)), &me.hashes(), now + 2).await,
        Err(MetaError::TwoFactorLocked { .. })
    ));
    let generation = gen_of(meta, "ana").await;
    let chal = new_challenge();
    meta.sign_in(id, ("$argon2id$stub", generation), "pc", &DeviceClient::default(), &token(), &chal, now + 2).await.unwrap();
    assert!(matches!(
        meta.complete_second_factor(&chal.id_hash, &Proof::Recovery(me.hashes()[2]), &token(), now + 3).await,
        Err(MetaError::TwoFactorLocked { .. })
    ));
    assert_eq!(meta.two_factor_info(id, now + 2).await.unwrap().recovery_codes_left, 10, "a refused check uses nothing up");
    // Over, a wrong proof locks again for twice as long: 30 minutes, then 60.
    now += 15 * MINUTE + 1;
    assert!(matches!(confirm_with(meta, id, None, now).await, Err(MetaError::TwoFactorInvalid)));
    let locked = confirm_with(meta, id, Some(FIRST_STEP + 1), now + 1).await;
    assert!(matches!(locked, Err(MetaError::TwoFactorLocked { retry_after_ms }) if retry_after_ms == 30 * MINUTE - 1), "{locked:?}");
    now += 30 * MINUTE + 1;
    assert!(matches!(confirm_with(meta, id, None, now).await, Err(MetaError::TwoFactorInvalid)));
    assert!(matches!(
        confirm_with(meta, id, Some(FIRST_STEP + 1), now + 1).await,
        Err(MetaError::TwoFactorLocked { retry_after_ms }) if retry_after_ms == 60 * MINUTE - 1
    ));
    // A right proof after it ran out resets the counters: nine more wrong ones do not lock.
    now += 60 * MINUTE + 1;
    assert!(matches!(confirm_with(meta, id, Some(FIRST_STEP + 1), now).await, Ok(true)));
    for n in 1..=9 {
        assert!(matches!(confirm_with(meta, id, None, now).await, Err(MetaError::TwoFactorInvalid)), "after the reset, wrong proof {n}");
    }
    assert!(matches!(confirm_with(meta, id, Some(FIRST_STEP + 2), now).await, Ok(true)), "nine are not ten");
    // The command line lifts any lock.
    for _ in 0..10 {
        let _ = confirm_with(meta, id, None, now).await;
    }
    assert!(matches!(confirm_with(meta, id, Some(FIRST_STEP + 3), now + 1).await, Err(MetaError::TwoFactorLocked { .. })));
    meta.reset_two_factor(id, now + 2).await.unwrap();
    assert!(matches!(confirm_with(meta, id, Some(FIRST_STEP + 3), now + 3).await, Ok(false)), "off, and nothing is locked");
}

/// The counters of wrong proofs are written outside the transaction that refused them, so requests at the same moment all
/// count: twenty wrong proofs lock it.
async fn wrong_proofs_at_the_same_moment_all_count(meta: &Meta) {
    let me = protected(meta, "ana").await;
    let (id, now) = (me.user.id, SETUP_AT + 100);
    let mut tasks = Vec::new();
    for _ in 0..12 {
        let meta = meta.clone();
        tasks.push(tokio::spawn(async move { confirm_with(&meta, id, None, now).await }));
    }
    for task in tasks {
        assert!(matches!(task.await.unwrap(), Err(MetaError::TwoFactorInvalid | MetaError::TwoFactorLocked { .. })));
    }
    assert!(matches!(confirm_with(meta, id, Some(FIRST_STEP + 1), now + 1).await, Err(MetaError::TwoFactorLocked { .. })));
}

async fn a_challenge_is_made_counted_used_once_and_carries_the_device(meta: &Meta) {
    let me = protected(meta, "ana").await;
    let id = me.user.id;
    let generation = gen_of(meta, "ana").await;
    let client = DeviceClient::new(Some("desktop"), Some("linux"), Some("1.2")).unwrap();
    let chal = new_challenge();
    let made = meta.sign_in(id, ("$argon2id$stub", generation), "work laptop", &client, &token(), &chal, SETUP_AT + 10).await.unwrap();
    assert_eq!(made, Some(SignIn::Challenge { expires_ms: SETUP_AT + 10 + 5 * MINUTE }));
    assert_eq!(live_devices(meta, &me.user).await, vec![me.first], "no device without the second factor");
    // The old call, for a caller that doesn't know, refuses an account that has two-factor sign-in.
    assert!(matches!(
        meta.sign_in_device(id, ("$argon2id$stub", generation), "x", &client, &token(), SETUP_AT + 10).await,
        Err(MetaError::TwoFactorRequired)
    ));
    assert_eq!(meta.challenge_owner(&chal.id_hash, SETUP_AT + 11).await.unwrap(), Some((id, "ana".to_owned())));
    // Four wrong codes are counted, the challenge stays, and the right one then works (a fifth wrong would delete it).
    for _ in 0..4 {
        let result = meta.complete_second_factor(&chal.id_hash, &totp(&wrong()), &token(), SETUP_AT + 12).await;
        assert!(matches!(result, Err(MetaError::TwoFactorInvalid)), "{result:?}");
    }
    let tok = token();
    let done = meta.complete_second_factor(&chal.id_hash, &totp(&right(FIRST_STEP + 1)), &tok, SETUP_AT + 13).await.unwrap();
    assert_eq!(done.user, id);
    let auth = meta.device_by_token(&tok).await.unwrap().unwrap();
    assert_eq!(auth.device, done.device);
    let made = meta.list_devices(id).await.unwrap().into_iter().find(|d| d.id == done.device).unwrap();
    assert_eq!((made.name.as_str(), made.client), ("work laptop", client.clone()), "the device is what the first call said");
    // Once: the same challenge again is gone, for a right proof as for a wrong one.
    for proof in [totp(&right(FIRST_STEP + 2)), totp(&wrong())] {
        assert!(matches!(meta.complete_second_factor(&chal.id_hash, &proof, &token(), SETUP_AT + 14).await, Err(MetaError::ChallengeGone)));
    }
    assert!(meta.challenge_owner(&chal.id_hash, SETUP_AT + 14).await.unwrap().is_none());

    // Five wrong codes delete the challenge: the right one is too late.
    let chal = new_challenge();
    meta.sign_in(id, ("$argon2id$stub", generation), "pc", &client, &token(), &chal, SETUP_AT + 20).await.unwrap();
    for _ in 0..5 {
        assert!(matches!(
            meta.complete_second_factor(&chal.id_hash, &totp(&wrong()), &token(), SETUP_AT + 21).await,
            Err(MetaError::TwoFactorInvalid)
        ));
    }
    assert!(matches!(
        meta.complete_second_factor(&chal.id_hash, &totp(&right(FIRST_STEP + 3)), &token(), SETUP_AT + 22).await,
        Err(MetaError::ChallengeGone)
    ));
    // One that ran out is as gone, and an unknown one.
    let chal = new_challenge();
    meta.sign_in(id, ("$argon2id$stub", generation), "pc", &client, &token(), &chal, SETUP_AT + 30).await.unwrap();
    assert!(matches!(
        meta.complete_second_factor(&chal.id_hash, &totp(&right(FIRST_STEP + 3)), &token(), SETUP_AT + 30 + 5 * MINUTE).await,
        Err(MetaError::ChallengeGone)
    ));
    assert!(matches!(
        meta.complete_second_factor(&token(), &totp(&right(FIRST_STEP + 3)), &token(), SETUP_AT + 30).await,
        Err(MetaError::ChallengeGone)
    ));
    // The recovery code finishes a sign-in too, once.
    let chal = new_challenge();
    meta.sign_in(id, ("$argon2id$stub", generation), "pc", &client, &token(), &chal, SETUP_AT + 40).await.unwrap();
    meta.complete_second_factor(&chal.id_hash, &Proof::Recovery(me.hashes()[4]), &token(), SETUP_AT + 41).await.unwrap();
    let chal = new_challenge();
    meta.sign_in(id, ("$argon2id$stub", generation), "pc", &client, &token(), &chal, SETUP_AT + 42).await.unwrap();
    assert!(matches!(
        meta.complete_second_factor(&chal.id_hash, &Proof::Recovery(me.hashes()[4]), &token(), SETUP_AT + 43).await,
        Err(MetaError::TwoFactorInvalid)
    ));
}

/// Each account keeps at most five open challenges (the oldest goes), and expired ones are deleted by the next sign-in.
async fn an_account_keeps_five_open_challenges_and_expired_ones_go(meta: &Meta) {
    let me = protected(meta, "ana").await;
    let generation = gen_of(meta, "ana").await;
    let mut made = Vec::new();
    for n in 0..7 {
        let chal = new_challenge();
        meta.sign_in(me.user.id, ("$argon2id$stub", generation), "pc", &DeviceClient::default(), &token(), &chal, SETUP_AT + 10 + n).await.unwrap();
        made.push(chal);
    }
    for (n, chal) in made.iter().enumerate() {
        let open = meta.challenge_owner(&chal.id_hash, SETUP_AT + 20).await.unwrap().is_some();
        assert_eq!(open, n >= 2, "challenge {n}: the two oldest were replaced");
    }
    // The next sign-in after they ran out deletes them (nothing else would).
    let later = SETUP_AT + 10 + 5 * MINUTE + 100;
    let fresh = new_challenge();
    meta.sign_in(me.user.id, ("$argon2id$stub", generation), "pc", &DeviceClient::default(), &token(), &fresh, later).await.unwrap();
    assert!(meta.challenge_owner(&fresh.id_hash, later).await.unwrap().is_some());
    // A stale password hash makes neither a challenge nor a device.
    let stale = new_challenge();
    assert!(
        meta.sign_in(me.user.id, ("$argon2id$old", generation), "pc", &DeviceClient::default(), &token(), &stale, later).await.unwrap().is_none()
    );
    assert!(
        meta.sign_in(me.user.id, ("$argon2id$stub", generation - 1), "pc", &DeviceClient::default(), &token(), &stale, later)
            .await
            .unwrap()
            .is_none()
    );
    assert!(meta.challenge_owner(&stale.id_hash, later).await.unwrap().is_none());
    assert_eq!(live_devices(meta, &me.user).await, vec![me.first]);
}

/// A challenge made before a password change, a reset, a turn-off, a reset of the second factor or a disable gives nothing after.
async fn a_challenge_dies_with_every_change_of_the_account(meta: &Meta) {
    let (_, actor) = admin_actor(meta, "root").await;
    for round in 0..6 {
        let me = protected(meta, &format!("ana{round}")).await;
        let generation = gen_of(meta, &format!("ana{round}")).await;
        let chal = new_challenge();
        meta.sign_in(me.user.id, ("$argon2id$stub", generation), "pc", &DeviceClient::default(), &token(), &chal, SETUP_AT + 10).await.unwrap();
        let at = SETUP_AT + 11;
        match round {
            0 => drop(meta.change_password(me.user.id, "$argon2id$stub", "$argon2id$new", me.first, at).await.unwrap()),
            1 => drop(meta.reset_password(me.user.id, "$argon2id$new", true, at).await.unwrap()),
            2 => drop(meta.two_factor_disable(&me.actor(), &Proof::Recovery(me.hashes()[0]), at).await.unwrap()),
            3 => drop(meta.reset_two_factor(me.user.id, at).await.unwrap()),
            4 => drop(meta.admin_disable_user(&actor, me.user.id, at).await.unwrap()),
            _ => drop(meta.admin_reset_two_factor(&actor, me.user.id, at).await.unwrap()),
        }
        assert!(meta.challenge_owner(&chal.id_hash, at).await.unwrap().is_none(), "round {round}");
        let result = meta.complete_second_factor(&chal.id_hash, &Proof::Recovery(me.hashes()[1]), &token(), at + 1).await;
        assert!(matches!(result, Err(MetaError::ChallengeGone)), "round {round}: {result:?}");
        assert!(live_devices(meta, &me.user).await.iter().all(|d| *d == me.first), "round {round}: no new device");
    }
}

async fn two_completions_of_one_challenge_give_one_device(meta: &Meta) {
    for round in 0..8 {
        let me = protected(meta, &format!("ana{round}")).await;
        let generation = gen_of(meta, &format!("ana{round}")).await;
        let chal = new_challenge();
        meta.sign_in(me.user.id, ("$argon2id$stub", generation), "pc", &DeviceClient::default(), &token(), &chal, SETUP_AT + 10).await.unwrap();
        let mut tasks = Vec::new();
        for job in 0..6 {
            let (meta, hash, recovery) = (meta.clone(), chal.id_hash, me.hashes()[job % 2]);
            tasks.push(tokio::spawn(async move {
                // The same TOTP step for some, two recovery codes for the others: a code is no way past the challenge.
                if job % 2 == 0 {
                    meta.complete_second_factor(&hash, &totp(&right(FIRST_STEP + 1)), &token(), SETUP_AT + 11).await
                } else {
                    meta.complete_second_factor(&hash, &Proof::Recovery(recovery), &token(), SETUP_AT + 11).await
                }
            }));
        }
        let mut done = 0;
        for task in tasks {
            match task.await.unwrap() {
                Ok(_) => done += 1,
                Err(MetaError::ChallengeGone | MetaError::TwoFactorInvalid) => {}
                other => panic!("round {round}: {other:?}"),
            }
        }
        assert_eq!(done, 1, "round {round}");
        assert_eq!(live_devices(meta, &me.user).await.len(), 2, "round {round}: the first device and one more");
    }
}

/// Completing a sign-in and a password change, a reset, a turn-off or the command line's reset at the same moment, over and
/// over: no database error, and once the change is done no device of the account but the one that asked is signed in,
/// whichever came first.
async fn completions_and_changes_of_the_account_at_the_same_moment_leave_no_stray_device(meta: &Meta) {
    for round in 0..12 {
        let name = format!("ana{round}");
        let me = protected(meta, &name).await;
        let generation = gen_of(meta, &name).await;
        let chal = new_challenge();
        meta.sign_in(me.user.id, ("$argon2id$stub", generation), "pc", &DeviceClient::default(), &token(), &chal, SETUP_AT + 10).await.unwrap();
        // A password change keeps the device that asked; a reset of the second factor or of the password signs out all.
        let all_gone = round % 3 != 0;
        let (completing, changing) = {
            let (m1, m2) = (meta.clone(), meta.clone());
            let (hash, id, first, recovery) = (chal.id_hash, me.user.id, me.first, me.hashes());
            let completing =
                tokio::spawn(
                    async move { m1.complete_second_factor(&hash, &Proof::Recovery(recovery[0]), &token(), SETUP_AT + 11).await.map(|_| ()) },
                );
            let changing = tokio::spawn(async move {
                match round % 3 {
                    0 => m2.change_password(id, "$argon2id$stub", "$argon2id$new", first, SETUP_AT + 12).await.map(|_| ()),
                    1 => m2.reset_two_factor(id, SETUP_AT + 12).await.map(|_| ()),
                    _ => m2.reset_password(id, "$argon2id$new", false, SETUP_AT + 12).await.map(|_| ()),
                }
            });
            (completing, changing)
        };
        let (completed, changed) = (completing.await.unwrap(), changing.await.unwrap());
        assert!(matches!(completed, Ok(()) | Err(MetaError::ChallengeGone)), "round {round}: {completed:?}");
        changed.unwrap_or_else(|e| panic!("round {round}: {e:?}"));
        let live = live_devices(meta, &me.user).await;
        if all_gone {
            assert!(live.is_empty(), "round {round}: {live:?}");
        } else {
            assert!(live.iter().all(|d| *d == me.first), "round {round}: {live:?}");
        }
        assert_revoke_columns_agree(&meta.list_devices(me.user.id).await.unwrap());
    }
}

/// Turning two-factor sign-in on or off while a password sign-in is under way: a device made without a second factor
/// before it is signed out by the turn-on, and one checked before it makes no device at all.
async fn turning_two_factor_on_and_off_at_the_same_moment_as_a_sign_in(meta: &Meta) {
    for round in 0..8 {
        let name = format!("on{round}");
        let ana = user(meta, &name).await;
        let first = device(meta, &ana).await;
        let actor = DeviceActor { user: ana.id, device: first, password_hash: "$argon2id$stub".to_owned() };
        meta.two_factor_begin_setup(&actor, b"cipher", SETUP_AT).await.unwrap();
        let generation = gen_of(meta, &name).await;
        let (m1, m2) = (meta.clone(), meta.clone());
        let id = ana.id;
        let signing = tokio::spawn(async move {
            m1.sign_in(id, ("$argon2id$stub", generation), "late", &DeviceClient::default(), &token(), &new_challenge(), SETUP_AT + 5).await
        });
        let hashes: Vec<[u8; 32]> = (0..RECOVERY_CODES).map(|_| code(INVITE_CHARS).hash()).collect();
        let enabling = tokio::spawn(async move { m2.two_factor_confirm(id, first, &*right(FIRST_STEP), &hashes, SETUP_AT + 5).await });
        let (signed, enabled) = (signing.await.unwrap(), enabling.await.unwrap());
        enabled.unwrap();
        assert!(matches!(signed, Ok(None | Some(SignIn::Device(_)))), "round {round}: a sign-in that checked before the turn-on makes no challenge");
        assert_eq!(live_devices(meta, &ana).await, vec![first], "round {round}: only the device that turned it on");

        // And off: a challenge made under it works once, but a device it made is signed out by the turn-off.
        let generation = gen_of(meta, &name).await;
        let chal = new_challenge();
        meta.sign_in(id, ("$argon2id$stub", generation), "x", &DeviceClient::default(), &token(), &chal, SETUP_AT + 6).await.unwrap();
        let (m1, m2) = (meta.clone(), meta.clone());
        let (hash, other_code) = (chal.id_hash, code(INVITE_CHARS).hash());
        let completing =
            tokio::spawn(async move { m1.complete_second_factor(&hash, &Proof::Recovery(other_code), &token(), SETUP_AT + 7).await.map(|_| ()) });
        let actor = DeviceActor { user: id, device: first, password_hash: "$argon2id$stub".to_owned() };
        let disabling = tokio::spawn(async move { m2.two_factor_disable(&actor, &totp(&right(FIRST_STEP + 1)), SETUP_AT + 7).await.map(|_| ()) });
        let completed = completing.await.unwrap();
        disabling.await.unwrap().unwrap();
        assert!(matches!(completed, Err(MetaError::TwoFactorInvalid | MetaError::ChallengeGone)), "round {round}: {completed:?}");
        assert_eq!(live_devices(meta, &ana).await, vec![first]);
    }
}

async fn an_invite_to_a_protected_account_needs_the_second_factor_in_the_same_transaction(meta: &Meta) {
    let me = protected(meta, "ana").await;
    let id = me.user.id;
    let invite = code(INVITE_CHARS);
    meta.create_invite("ana", &invite.hash(), SETUP_AT + 10, INVITE_TTL_MS).await.unwrap();
    // No second factor: refused and the invite is kept (not used up), and nothing is counted.
    assert!(matches!(redeem_with_factor(meta, &invite.hash(), None, SETUP_AT + 11).await, Err(MetaError::TwoFactorRequired)));
    let old_call = meta.redeem_with(&invite.hash(), "x", &DeviceClient::default(), &token(), None, SETUP_AT + 11).await;
    assert!(matches!(old_call, Err(MetaError::TwoFactorRequired)));
    // Wrong factors: refused, the invite stays, and nine of them leave the counters just short of the lock.
    for n in 1..=9 {
        let result = redeem_with_factor(meta, &invite.hash(), Some(&totp(&wrong())), SETUP_AT + 12).await;
        assert!(matches!(result, Err(MetaError::TwoFactorInvalid)), "wrong factor {n}: {result:?}");
    }
    assert_eq!(meta.invite_needs_password(&invite.hash(), SETUP_AT + 12).await.unwrap(), Some(false), "the invite is still there");
    // The tenth locks, with the invite still there, and a right one is refused while it lasts.
    assert!(matches!(redeem_with_factor(meta, &invite.hash(), Some(&totp(&wrong())), SETUP_AT + 12).await, Err(MetaError::TwoFactorInvalid)));
    let locked = redeem_with_factor(meta, &invite.hash(), Some(&totp(&right(FIRST_STEP + 1))), SETUP_AT + 13).await;
    assert!(matches!(locked, Err(MetaError::TwoFactorLocked { .. })), "{locked:?}");
    // A step that was used is wrong, even after the lock.
    let after = SETUP_AT + 12 + 15 * MINUTE + 1;
    let replay = redeem_with_factor(meta, &invite.hash(), Some(&totp(&right(FIRST_STEP))), after).await;
    assert!(matches!(replay, Err(MetaError::TwoFactorInvalid)), "the set-up's own step: {replay:?}");
    assert_eq!(meta.invite_needs_password(&invite.hash(), after).await.unwrap(), Some(false), "still there");
    assert_eq!(live_devices(meta, &me.user).await, vec![me.first], "no device was made by any of it");
    let _ = id;

    // A fresh account: a new step is right, and the invite, the step and the device go together.
    let bea = protected(meta, "bea").await;
    let invite = code(INVITE_CHARS);
    meta.create_invite("bea", &invite.hash(), SETUP_AT + 10, INVITE_TTL_MS).await.unwrap();
    let device = redeem_with_factor(meta, &invite.hash(), Some(&totp(&right(FIRST_STEP + 1))), SETUP_AT + 20).await.unwrap();
    assert_eq!(live_devices(meta, &bea.user).await.len(), 2);
    assert!(live_devices(meta, &bea.user).await.contains(&device));
    assert_eq!(meta.invite_needs_password(&invite.hash(), SETUP_AT + 21).await.unwrap(), None, "used up with the device");
    // Nobody else can use that step now (sign-in and join share the counter).
    assert!(matches!(confirm_with(meta, bea.user.id, Some(FIRST_STEP + 1), SETUP_AT + 22).await, Err(MetaError::TwoFactorInvalid)));
    // A pairing needs no second factor: only a signed-in device can make one.
    let pairing = code(PAIRING_CHARS);
    meta.create_pairing(bea.user.id, bea.first, &pairing.hash(), SETUP_AT + 30).await.unwrap();
    meta.redeem_with(&pairing.hash(), "paired", &DeviceClient::default(), &token(), None, SETUP_AT + 31).await.unwrap();
}

async fn a_reset_code_for_a_protected_account_needs_the_factor_unless_it_turns_it_off(meta: &Meta) {
    let (_, admin) = admin_actor(meta, "root").await;
    let admin = AdminActor { two_factor: false, ..admin };
    let me = protected(meta, "ana").await;
    let (id, stale) = (me.user.id, me.hashes());
    let reset = code(INVITE_CHARS);
    meta.admin_issue_reset(&admin, id, &reset.hash(), false, SETUP_AT + 10, RESET_TTL_MS).await.unwrap();
    assert!(matches!(
        meta.redeem_with(&reset.hash(), "pc", &DeviceClient::default(), &token(), Some("$argon2id$fresh"), SETUP_AT + 11).await,
        Err(MetaError::TwoFactorRequired)
    ));
    assert_eq!(meta.user_by_name("ana").await.unwrap().unwrap().password_hash, "$argon2id$stub", "nothing changed");
    assert!(matches!(
        meta.redeem_with_second_factor(
            &reset.hash(),
            "pc",
            &DeviceClient::default(),
            &token(),
            Some("$argon2id$fresh"),
            Some(&totp(&wrong())),
            SETUP_AT + 11
        )
        .await,
        Err(MetaError::TwoFactorInvalid)
    ));
    assert_eq!(live_devices(meta, &me.user).await, vec![me.first], "the failed redeem signed nothing out");
    let first_done = meta
        .redeem_with_second_factor(
            &reset.hash(),
            "pc",
            &DeviceClient::default(),
            &token(),
            Some("$argon2id$fresh"),
            Some(&Proof::Recovery(stale[0])),
            SETUP_AT + 12,
        )
        .await
        .unwrap();
    let done = &first_done;
    assert_eq!(done.revoked, vec![me.first]);
    assert!(meta.two_factor_enabled(id).await.unwrap(), "the code did not turn two-factor sign-in off");
    assert_eq!(meta.user_by_name("ana").await.unwrap().unwrap().password_hash, "$argon2id$fresh");

    // A code made to turn it off asks for no second factor, turns it off and signs everything out.
    let reset = code(INVITE_CHARS);
    meta.admin_issue_reset(&admin, id, &reset.hash(), true, SETUP_AT + 20, RESET_TTL_MS).await.unwrap();
    let done = meta.redeem_with(&reset.hash(), "pc2", &DeviceClient::default(), &token(), Some("$argon2id$newer"), SETUP_AT + 21).await.unwrap();
    assert_eq!(done.revoked, vec![first_done.device], "the computer of the first redeem is signed out with the rest");
    assert!(!meta.two_factor_enabled(id).await.unwrap());
    assert_eq!(meta.two_factor_info(id, SETUP_AT + 22).await.unwrap().recovery_codes_left, 0);
    assert_eq!(live_devices(meta, &me.user).await, vec![done.device], "the redeeming computer is the only one signed in");
    // An account without two-factor sign-in redeems a reset code as it always did, whatever the flag.
    let plain = user(meta, "plain").await;
    let reset = code(INVITE_CHARS);
    meta.admin_issue_reset(&admin, plain.id, &reset.hash(), false, SETUP_AT + 30, RESET_TTL_MS).await.unwrap();
    meta.redeem_with(&reset.hash(), "pc", &DeviceClient::default(), &token(), Some("$argon2id$fresh"), SETUP_AT + 31).await.unwrap();
}

/// The admin's own two-factor state is compared again in every change, and resetting someone's two-factor sign-in, or another
/// admin's password, needs the acting admin's own.
async fn admins_two_factor_is_checked_in_the_change_and_resets_need_it(meta: &Meta) {
    let (root, plain_actor) = admin_actor(meta, "root").await;
    let (other, _) = admin_actor(meta, "other").await;
    let member = protected(meta, "member").await;
    let (boss_admin, boss_actor) = {
        let (u, a) = admin_actor(meta, "boss").await;
        (u, a)
    };
    // Turn two-factor sign-in on for the acting admin "boss" (device `device_of(&boss_actor)`).
    let actor_for = DeviceActor { user: boss_admin.id, device: device_of(&boss_actor), password_hash: ADMIN_HASH.to_owned() };
    meta.two_factor_begin_setup(&actor_for, b"cipher", SETUP_AT).await.unwrap();
    meta.two_factor_confirm(boss_admin.id, device_of(&boss_actor), &*right(FIRST_STEP), &[code(INVITE_CHARS).hash()], SETUP_AT + 1).await.unwrap();
    // A confirmation that was given when it was off, and a change after it turned on, conflict (and the other way round).
    let hash = code(INVITE_CHARS).hash();
    let result = meta.admin_issue_invite(&boss_actor, member.user.id, &hash, SETUP_AT + 2, INVITE_TTL_MS).await;
    assert!(matches!(result, Err(MetaError::Conflict(_))), "{result:?}");
    let with_factor = AdminActor { two_factor: true, ..boss_actor.clone() };
    let result = meta.admin_issue_invite(&with_factor, member.user.id, &hash, SETUP_AT + 2, INVITE_TTL_MS).await;
    assert!(matches!(result, Err(MetaError::Conflict(_))), "it has a password: {result:?}");
    let result =
        meta.admin_issue_invite(&AdminActor { two_factor: true, ..plain_actor.clone() }, member.user.id, &hash, SETUP_AT + 2, INVITE_TTL_MS).await;
    assert!(matches!(result, Err(MetaError::Conflict(_))), "root has none: {result:?}");

    // Resetting a member's two-factor sign-in: without the admin's own it works for a member, not for an admin.
    let result = meta.admin_reset_two_factor(&plain_actor, other.id, SETUP_AT + 3).await;
    assert!(matches!(result, Err(MetaError::Forbidden(_))), "another admin, no two-factor of one's own: {result:?}");
    let result = meta.admin_issue_reset(&plain_actor, other.id, &hash, false, SETUP_AT + 3, RESET_TTL_MS).await;
    assert!(matches!(result, Err(MetaError::Forbidden(_))), "{result:?}");
    assert!(matches!(meta.admin_reset_two_factor(&plain_actor, root.id, SETUP_AT + 3).await, Err(MetaError::Invalid(_))), "not one's own");
    assert!(matches!(meta.admin_reset_two_factor(&plain_actor, UserId(uuid::Uuid::now_v7()), SETUP_AT + 3).await, Err(MetaError::NotFound(_))));
    // An admin with it may reset another admin's password, and that admin's two-factor sign-in (once they have one).
    meta.admin_issue_reset(&with_factor, other.id, &hash, false, SETUP_AT + 4, RESET_TTL_MS).await.unwrap();
    let result = meta.admin_reset_two_factor(&with_factor, other.id, SETUP_AT + 4).await;
    assert!(matches!(result, Err(MetaError::Conflict(_))), "the other admin has none: {result:?}");
    // A member's: everything of it goes, every device is signed out, the lock goes with it.
    let revoked = meta.admin_reset_two_factor(&plain_actor, member.user.id, SETUP_AT + 5).await.unwrap();
    assert_eq!(revoked, vec![member.first]);
    assert!(!meta.two_factor_enabled(member.user.id).await.unwrap());
    assert!(live_devices(meta, &member.user).await.is_empty());
    let devices = meta.list_devices(member.user.id).await.unwrap();
    assert!(devices.iter().any(|d| d.revoke_reason == Some(RevokeReason::TwoFactor)));
    assert!(matches!(meta.admin_reset_two_factor(&plain_actor, member.user.id, SETUP_AT + 6).await, Err(MetaError::Conflict(_))), "twice");
    let row = admin_view(meta, boss_admin.id).await;
    assert!(row.two_factor_enabled);
    assert!(!admin_view(meta, member.user.id).await.two_factor_enabled);
}

async fn the_command_line_resets_two_factor_and_the_account_shows_it(meta: &Meta) {
    let me = protected(meta, "ana").await;
    let id = me.user.id;
    assert!(meta.account(id).await.unwrap().two_factor_enabled);
    assert!(admin_view(meta, id).await.two_factor_enabled);
    let generation = gen_of(meta, "ana").await;
    meta.sign_in(id, ("$argon2id$stub", generation), "pc", &DeviceClient::default(), &token(), &new_challenge(), SETUP_AT + 5).await.unwrap();
    meta.create_invite("ana", &code(INVITE_CHARS).hash(), SETUP_AT + 5, INVITE_TTL_MS).await.unwrap();
    let revoked = meta.reset_two_factor(id, SETUP_AT + 6).await.unwrap();
    assert_eq!(revoked, vec![me.first]);
    assert!(!meta.account(id).await.unwrap().two_factor_enabled);
    assert!(!admin_view(meta, id).await.two_factor_enabled);
    assert!(!meta.revoke_invite("ana").await.unwrap());
    assert!(matches!(meta.reset_two_factor(id, SETUP_AT + 7).await, Err(MetaError::Conflict(_))), "nothing to reset: nothing is signed out");
    assert!(matches!(meta.reset_two_factor(UserId(uuid::Uuid::now_v7()), SETUP_AT + 7).await, Err(MetaError::NotFound(_))));
    // After it a password sign-in gives a device again, and the account can set it up anew.
    let generation = gen_of(meta, "ana").await;
    let signed =
        meta.sign_in(id, ("$argon2id$stub", generation), "pc", &DeviceClient::default(), &token(), &new_challenge(), SETUP_AT + 8).await.unwrap();
    assert!(matches!(signed, Some(SignIn::Device(_))));
}

/// The key tools read the ciphertexts and replace one only if it is still the one they read.
async fn secrets_are_listed_and_replaced_only_if_unchanged(meta: &Meta) {
    let me = protected(meta, "ana").await;
    let other = user(meta, "bob").await;
    let dev = device(meta, &other).await;
    let actor = DeviceActor { user: other.id, device: dev, password_hash: "$argon2id$stub".to_owned() };
    meta.two_factor_begin_setup(&actor, b"cipher-pending", SETUP_AT).await.unwrap();
    let listed = meta.two_factor_secrets(SETUP_AT + 1).await.unwrap();
    assert_eq!(listed.len(), 2);
    let ana = listed.iter().find(|s| s.user == me.user.id).unwrap();
    assert_eq!((ana.active.as_deref(), ana.pending.as_deref()), (Some(b"cipher-of-the-secret".as_slice()), None));
    let bob = listed.iter().find(|s| s.user == other.id).unwrap();
    assert_eq!((bob.active.as_deref(), bob.pending.as_deref()), (None, Some(b"cipher-pending".as_slice())));
    let later = meta.two_factor_secrets(SETUP_AT + 11 * MINUTE).await.unwrap();
    assert!(later.iter().find(|s| s.user == other.id).is_some_and(|s| s.pending.is_none()), "an expired set-up is not listed");
    assert!(meta.two_factor_replace_ciphertext(me.user.id, false, b"cipher-of-the-secret", b"new-cipher").await.unwrap());
    assert!(
        !meta.two_factor_replace_ciphertext(me.user.id, false, b"cipher-of-the-secret", b"newer").await.unwrap(),
        "no longer the one that was read"
    );
    assert!(meta.two_factor_replace_ciphertext(other.id, true, b"cipher-pending", b"new-pending").await.unwrap());
    let listed = meta.two_factor_secrets(SETUP_AT + 1).await.unwrap();
    assert_eq!(listed.iter().find(|s| s.user == me.user.id).unwrap().active.as_deref(), Some(b"new-cipher".as_slice()));
    // A secret that isn't the account's is never touched.
    assert!(!meta.two_factor_replace_ciphertext(UserId(uuid::Uuid::now_v7()), false, b"x", b"y").await.unwrap());
}

/// Wrong proofs are counted under the lock that checked them: of many at once on one challenge exactly five are counted (the
/// fifth deletes it, the rest find it gone), and they all count against the account's ten.
async fn parallel_wrong_proofs_on_one_challenge_count_exactly_five(meta: &Meta) {
    let me = protected(meta, "ana").await;
    let id = me.user.id;
    let generation = gen_of(meta, "ana").await;
    for round in 0..4 {
        let chal = new_challenge();
        meta.sign_in(id, ("$argon2id$stub", generation), "pc", &DeviceClient::default(), &token(), &chal, SETUP_AT + 10 + round).await.unwrap();
        let mut tasks = Vec::new();
        for _ in 0..14 {
            let (meta, hash) = (meta.clone(), chal.id_hash);
            tasks.push(tokio::spawn(async move { meta.complete_second_factor(&hash, &totp(&wrong()), &token(), SETUP_AT + 20).await.map(|_| ()) }));
        }
        let (mut counted, mut gone) = (0, 0);
        for task in tasks {
            match task.await.unwrap() {
                Err(MetaError::TwoFactorInvalid) => counted += 1,
                Err(MetaError::ChallengeGone) => gone += 1,
                Err(MetaError::TwoFactorLocked { .. }) => {}
                other => panic!("round {round}: {other:?}"),
            }
        }
        assert!(meta.challenge_owner(&chal.id_hash, SETUP_AT + 21).await.unwrap().is_none(), "round {round}: five wrong proofs delete it");
        // Locked rounds stop counting; until then exactly five per challenge, never six.
        if round == 0 {
            assert_eq!((counted, gone), (5, 9), "round {round}");
        }
        assert!(counted <= 5, "round {round}: {counted} proofs were checked against one challenge");
        if meta.two_factor_locked_for(id, SETUP_AT + 21).await.unwrap().is_some() {
            break;
        }
    }
}

/// Many wrong proofs at once, crossing ten: the lock is set by exactly the tenth, for exactly 15 minutes, and the ones after it
/// find it locked and count nothing (so it never jumps to 30).
async fn parallel_wrong_proofs_lock_for_exactly_fifteen_minutes(meta: &Meta) {
    for round in 0..6 {
        let me = protected(meta, &format!("ana{round}")).await;
        let (id, now) = (me.user.id, SETUP_AT + 100);
        let mut tasks = Vec::new();
        for _ in 0..24 {
            let meta = meta.clone();
            tasks.push(tokio::spawn(async move { confirm_with(&meta, id, None, now).await }));
        }
        let (mut wrong_ones, mut locked) = (0, 0);
        for task in tasks {
            match task.await.unwrap() {
                Err(MetaError::TwoFactorInvalid) => wrong_ones += 1,
                Err(MetaError::TwoFactorLocked { .. }) => locked += 1,
                other => panic!("round {round}: {other:?}"),
            }
        }
        assert_eq!((wrong_ones, locked), (10, 14), "round {round}");
        assert_eq!(meta.two_factor_locked_for(id, now).await.unwrap(), Some(15 * MINUTE), "round {round}: 15 minutes, not 30");
    }
}

// ---- the audit log of two-factor sign-in ----

/// The events of `action` in the log, oldest first (an aggregated row counts as one).
async fn events_of(meta: &Meta, action: AuditAction) -> Vec<AuditEvent> {
    let mut events: Vec<AuditEvent> = all_events(meta).await.into_iter().filter(|e| e.action == action).collect();
    events.reverse();
    events
}

/// Turning it on, new recovery codes and turning it off each write their event in their own transaction, as the account's own
/// act from the device that asked; a wrong proof on these routes writes nothing, and a recovery code that is used up writes
/// `auth.recovery_code_used` with the change it served.
async fn two_factor_changes_write_their_events(meta: &Meta) {
    let ana = user(meta, "ana").await;
    let (first, _second) = (device(meta, &ana).await, device(meta, &ana).await);
    let actor = DeviceActor { user: ana.id, device: first, password_hash: "$argon2id$stub".to_owned() };
    let mut seen = Seen::new(meta).await;
    let hashes: Vec<[u8; 32]> = (0..10).map(|_| code(INVITE_CHARS).hash()).collect();

    meta.two_factor_begin_setup(&actor, b"cipher-of-the-secret", SETUP_AT).await.unwrap();
    assert!(seen.actions().await.is_empty(), "a set-up is not a change");
    assert!(matches!(meta.two_factor_confirm(ana.id, first, &*wrong(), &hashes, SETUP_AT + 1).await, Err(MetaError::TwoFactorInvalid)));
    assert!(seen.actions().await.is_empty(), "a wrong first code writes nothing");
    meta.two_factor_confirm(ana.id, first, &*right(FIRST_STEP), &hashes, SETUP_AT + 2).await.unwrap();
    let e = seen.one().await;
    assert_eq!(
        (e.action, e.outcome, e.actor_user, e.actor_device, e.target_id, e.at_ms),
        (AuditAction::AccountTwoFactorEnabled, AuditOutcome::Success, Some(ana.id), Some(first), Some(ana.id.0), SETUP_AT + 2)
    );
    assert_eq!(e.target_kind.as_str(), "user");
    assert_eq!(details(&e), json("{}"));

    // New recovery codes: a wrong code writes nothing (the lock it can cause would), a right one the event.
    let result = meta.two_factor_new_recovery_codes(&actor, &totp(&wrong()), &hashes, SETUP_AT + 3).await;
    assert!(matches!(result, Err(MetaError::TwoFactorInvalid)));
    assert!(seen.actions().await.is_empty(), "a wrong proof on a signed-in route is not written one by one");
    meta.two_factor_new_recovery_codes(&actor, &totp(&right(FIRST_STEP + 1)), &hashes, SETUP_AT + 4).await.unwrap();
    let e = seen.one().await;
    assert_eq!(
        (e.action, e.actor_user, e.actor_device, e.target_id),
        (AuditAction::AccountRecoveryCodesRegenerated, Some(ana.id), Some(first), Some(ana.id.0))
    );
    assert_eq!(details(&e), json("{}"));

    // Turning it off with a recovery code: the change and the code's use, from the same device.
    meta.two_factor_disable(&actor, &Proof::Recovery(hashes[0]), SETUP_AT + 5).await.unwrap();
    assert_eq!(seen.actions().await, ["account.two_factor_disabled", "auth.recovery_code_used"]);
    let used = &events_of(meta, AuditAction::AuthRecoveryCodeUsed).await[0];
    assert_eq!((used.actor_user, used.actor_device, used.target_id, used.at_ms), (Some(ana.id), Some(first), Some(ana.id.0), SETUP_AT + 5));
    assert_eq!(details(used), json("{}"), "never the code");
    let off = &events_of(meta, AuditAction::AccountTwoFactorDisabled).await[0];
    assert_eq!((off.actor_user, off.actor_device), (Some(ana.id), Some(first)));
    // Off: nothing to turn off, nothing written.
    assert!(matches!(meta.two_factor_disable(&actor, &totp(&right(FIRST_STEP + 2)), SETUP_AT + 6).await, Err(MetaError::Conflict(_))));
    assert!(seen.actions().await.is_empty());
}

/// A sign-in that made a device is `auth.login` whichever way it went (no second factor, a code, a recovery code); the
/// challenge writes nothing; a wrong second step is one aggregated `auth.second_factor_failed` row per account and minute,
/// written after the answer is decided.
async fn two_factor_sign_in_writes_login_and_counts_failures(meta: &Meta) {
    let me = protected(meta, "ana").await;
    let plain = user(meta, "bob").await;
    let mut seen = Seen::new(meta).await;
    let client = DeviceClient::new(Some("desktop"), Some("Windows"), Some("1.0")).unwrap();
    let now = SETUP_AT + 100;

    // No two-factor sign-in: a login at once.
    let generation = gen_of(meta, "bob").await;
    let signed = meta.sign_in(plain.id, ("$argon2id$stub", generation), "pc", &client, &token(), &new_challenge(), now).await.unwrap();
    let Some(SignIn::Device(bobs)) = signed else { panic!("{signed:?}") };
    let e = seen.one().await;
    assert_eq!(
        (e.action, e.outcome, e.actor_user, e.actor_device, e.target_id),
        (AuditAction::AuthLogin, AuditOutcome::Success, Some(plain.id), Some(bobs), Some(*bobs.as_uuid()))
    );
    assert_eq!(details(&e), json(r#"{"client_kind":"desktop","platform":"windows"}"#));

    // Two-factor sign-in: the challenge is no login.
    let generation = gen_of(meta, "ana").await;
    let chal = new_challenge();
    let signed = meta.sign_in(me.user.id, ("$argon2id$stub", generation), "pc", &client, &token(), &chal, now).await.unwrap();
    assert!(matches!(signed, Some(SignIn::Challenge { .. })));
    assert!(seen.actions().await.is_empty(), "no one is signed in yet");
    // A wrong code: one failure row, no login, the actor unknown (nobody proved who they were).
    let result = meta.complete_second_factor(&chal.id_hash, &totp(&wrong()), &token(), now + 1).await;
    assert!(matches!(result, Err(MetaError::TwoFactorInvalid)));
    let e = seen.one().await;
    assert_eq!(
        (e.action, e.outcome, e.actor_user, e.actor_device, e.target_id, e.count),
        (AuditAction::AuthSecondFactorFailed, AuditOutcome::Failure, None, None, Some(me.user.id.0), 1)
    );
    assert_eq!(details(&e), json(r#"{"step":"totp"}"#));
    // More in the same minute make the same row count up (a recovery code that is wrong too).
    let result = meta.complete_second_factor(&chal.id_hash, &Proof::Recovery(code(INVITE_CHARS).hash()), &token(), now + 2).await;
    assert!(matches!(result, Err(MetaError::TwoFactorInvalid)));
    assert!(seen.actions().await.is_empty(), "the row was written already; only its count goes up");
    let rows = events_of(meta, AuditAction::AuthSecondFactorFailed).await;
    assert_eq!((rows.len(), rows[0].count), (1, 2));
    // A right code: the login of the new device.
    let done = meta.complete_second_factor(&chal.id_hash, &totp(&right(FIRST_STEP + 1)), &token(), now + 3).await.unwrap();
    let e = seen.one().await;
    assert_eq!(
        (e.action, e.actor_user, e.actor_device, e.target_id),
        (AuditAction::AuthLogin, Some(me.user.id), Some(done.device), Some(*done.device.as_uuid()))
    );
    assert_eq!(details(&e), json(r#"{"client_kind":"desktop","platform":"windows"}"#));

    // A recovery code: the login and the code's use, the device the same.
    let chal = new_challenge();
    let generation = gen_of(meta, "ana").await;
    meta.sign_in(me.user.id, ("$argon2id$stub", generation), "pc", &client, &token(), &chal, now + 4).await.unwrap();
    let done = meta.complete_second_factor(&chal.id_hash, &Proof::Recovery(me.hashes()[0]), &token(), now + 5).await.unwrap();
    assert_eq!(seen.actions().await, ["auth.login", "auth.recovery_code_used"]);
    let used = &events_of(meta, AuditAction::AuthRecoveryCodeUsed).await[0];
    assert_eq!((used.actor_user, used.actor_device, used.target_id), (Some(me.user.id), Some(done.device), Some(me.user.id.0)));
    // The same code again is a wrong recovery code: a failure, not a use.
    let chal = new_challenge();
    meta.sign_in(me.user.id, ("$argon2id$stub", generation), "pc", &client, &token(), &chal, now + 6).await.unwrap();
    let result = meta.complete_second_factor(&chal.id_hash, &Proof::Recovery(me.hashes()[0]), &token(), now + 7).await;
    assert!(matches!(result, Err(MetaError::TwoFactorInvalid)));
    assert_eq!(events_of(meta, AuditAction::AuthRecoveryCodeUsed).await.len(), 1);
    assert_eq!(events_of(meta, AuditAction::AuthSecondFactorFailed).await[0].count, 3);
}

/// The lock writes `auth.two_factor_locked` once, when it is set or lengthened, and never for a refusal while it holds; wrong
/// proofs on a signed-in route or an admin's confirmation count towards it without an event of their own.
async fn the_lock_writes_one_event_when_it_is_set(meta: &Meta) {
    let me = protected(meta, "ana").await;
    let id = me.user.id;
    let mut seen = Seen::new(meta).await;
    let mut now = SETUP_AT + 100;
    for n in 1..=9 {
        assert!(matches!(confirm_with(meta, id, None, now).await, Err(MetaError::TwoFactorInvalid)), "wrong proof {n}");
    }
    assert!(seen.actions().await.is_empty(), "nine wrong proofs are not written one by one");
    assert!(matches!(confirm_with(meta, id, None, now).await, Err(MetaError::TwoFactorInvalid)));
    let e = seen.one().await;
    assert_eq!(
        (e.action, e.outcome, e.actor_user, e.actor_device, e.target_id, e.at_ms),
        (AuditAction::AuthTwoFactorLocked, AuditOutcome::Success, None, None, Some(id.0), now)
    );
    assert_eq!(details(&e), json("{}"));
    // Refused while it holds, by every path: no further event.
    for offset in 1..4 {
        let locked = confirm_with(meta, id, Some(FIRST_STEP + 1), now + offset).await;
        assert!(matches!(locked, Err(MetaError::TwoFactorLocked { .. })), "{locked:?}");
    }
    assert!(matches!(meta.two_factor_disable(&me.actor(), &Proof::Recovery(me.hashes()[1]), now + 5).await, Err(MetaError::TwoFactorLocked { .. })));
    assert!(seen.actions().await.is_empty(), "a refusal under the lock writes nothing");
    // Over, one more wrong proof sets a longer lock: a new event.
    now += 15 * MINUTE + 1;
    assert!(matches!(confirm_with(meta, id, None, now).await, Err(MetaError::TwoFactorInvalid)));
    let e = seen.one().await;
    assert_eq!((e.action, e.at_ms), (AuditAction::AuthTwoFactorLocked, now));
    assert_eq!(events_of(meta, AuditAction::AuthTwoFactorLocked).await.len(), 2);
    assert!(events_of(meta, AuditAction::AuthSecondFactorFailed).await.is_empty(), "these are not sign-in steps");
}

/// The second factor at a join: a wrong one is one failure row and the invite stays; a recovery code used for it is written
/// with the redemption; a reset code made with `reset_two_factor` takes it away and the redemption says so, by the account that
/// redeemed it (no admin acts at that moment).
async fn a_join_writes_its_two_factor_events(meta: &Meta) {
    let me = protected(meta, "ana").await;
    let id = me.user.id;
    let (_, admin) = admin_actor(meta, "root").await;
    let mut seen = Seen::new(meta).await;
    let now = SETUP_AT + 100;

    let invite = code(INVITE_CHARS);
    meta.create_invite("ana", &invite.hash(), now, INVITE_TTL_MS).await.unwrap();
    assert!(matches!(redeem_with_factor(meta, &invite.hash(), None, now + 1).await, Err(MetaError::TwoFactorRequired)));
    assert!(seen.actions().await.is_empty(), "asking for the factor writes nothing");
    assert!(matches!(redeem_with_factor(meta, &invite.hash(), Some(&totp(&wrong())), now + 2).await, Err(MetaError::TwoFactorInvalid)));
    let e = seen.one().await;
    assert_eq!(
        (e.action, e.outcome, e.actor_user, e.target_id, e.count),
        (AuditAction::AuthSecondFactorFailed, AuditOutcome::Failure, None, Some(id.0), 1)
    );
    assert_eq!(details(&e), json(r#"{"step":"totp"}"#));
    // With a recovery code: the redemption and the code's use, by the new device.
    let device = redeem_with_factor(meta, &invite.hash(), Some(&Proof::Recovery(me.hashes()[0])), now + 3).await.unwrap();
    assert_eq!(seen.actions().await, ["auth.recovery_code_used", "join.invite_redeemed"]);
    let used = &events_of(meta, AuditAction::AuthRecoveryCodeUsed).await[0];
    assert_eq!((used.actor_user, used.actor_device), (Some(id), Some(device)));

    // A reset code without the removal of two-factor: the right code is needed; the event has no such detail.
    let reset = code(INVITE_CHARS);
    meta.admin_issue_reset(&admin, id, &reset.hash(), false, now + 4, RESET_TTL_MS).await.unwrap();
    assert_eq!(seen.actions().await, ["admin.password_reset_issued"]);
    let redeemed = meta
        .redeem_with_second_factor(
            &reset.hash(),
            "new pc",
            &DeviceClient::default(),
            &token(),
            Some("$argon2id$new"),
            Some(&totp(&right(FIRST_STEP + 1))),
            now + 5,
        )
        .await
        .unwrap();
    let e = seen.one().await;
    assert_eq!((e.action, e.actor_user, e.actor_device), (AuditAction::JoinResetRedeemed, Some(id), Some(redeemed.device)));
    assert_eq!(details(&e), json(&format!(r#"{{"revoked":{}}}"#, redeemed.revoked.len())), "two-factor sign-in is still on");
    assert!(meta.two_factor_enabled(id).await.unwrap());

    // A reset code made with `reset_two_factor`: it takes the second factor away and the event says so.
    let reset = code(INVITE_CHARS);
    meta.admin_issue_reset(&admin, id, &reset.hash(), true, now + 6, RESET_TTL_MS).await.unwrap();
    assert_eq!(seen.actions().await, ["admin.password_reset_issued"]);
    let redeemed = meta
        .redeem_with_second_factor(&reset.hash(), "newer pc", &DeviceClient::default(), &token(), Some("$argon2id$newer"), None, now + 7)
        .await
        .unwrap();
    let e = seen.one().await;
    assert_eq!(
        (e.action, e.actor_user, e.actor_device, e.target_id),
        (AuditAction::JoinResetRedeemed, Some(id), Some(redeemed.device), Some(*redeemed.device.as_uuid()))
    );
    assert_eq!(details(&e), json(&format!(r#"{{"revoked":{},"two_factor_removed":true}}"#, redeemed.revoked.len())));
    assert!(!meta.two_factor_enabled(id).await.unwrap());
    assert!(events_of(meta, AuditAction::AdminTwoFactorReset).await.is_empty(), "no admin is made up for it");
}

/// An admin's reset of another account's two-factor sign-in and the command line's each write their event; the admin's
/// confirmation by recovery code writes the code's use, for the admin's account, even if the change it confirms fails.
async fn resets_and_confirmations_write_their_events(meta: &Meta) {
    let (_, admin) = admin_actor(meta, "root").await;
    let member = protected(meta, "member").await;
    let other = protected(meta, "other").await;
    let mut seen = Seen::new(meta).await;
    let now = SETUP_AT + 100;

    let revoked = meta.admin_reset_two_factor(&admin, member.user.id, now).await.unwrap();
    let e = seen.one().await;
    assert_eq!(
        (e.action, e.outcome, e.actor_user, e.actor_device, e.target_id),
        (AuditAction::AdminTwoFactorReset, AuditOutcome::Success, Some(admin.user), Some(device_of(&admin)), Some(member.user.id.0))
    );
    assert_eq!(details(&e), json("{}"));
    assert_eq!(revoked.len(), 1);
    assert!(matches!(meta.admin_reset_two_factor(&admin, member.user.id, now + 1).await, Err(MetaError::Conflict(_))));
    assert!(seen.actions().await.is_empty(), "nothing to reset: nothing written");

    let revoked = meta.reset_two_factor(other.user.id, now + 2).await.unwrap();
    let e = seen.one().await;
    assert_eq!((e.action, e.actor_user, e.actor_device, e.target_id), (AuditAction::CliTwoFactorReset, None, None, Some(other.user.id.0)));
    assert_eq!(details(&e), json(&format!(r#"{{"revoked":{}}}"#, revoked.len())));
    assert!(matches!(meta.reset_two_factor(other.user.id, now + 3).await, Err(MetaError::Conflict(_))));
    assert!(seen.actions().await.is_empty());

    // An account's confirmation by recovery code, as an admin gives it: the code's use, no device.
    let bea = protected(meta, "bea").await;
    let mut seen = Seen::new(meta).await;
    assert!(meta.confirm_second_factor(bea.user.id, Some(&Proof::Recovery(bea.hashes()[3])), now + 4).await.unwrap());
    let e = seen.one().await;
    assert_eq!(
        (e.action, e.actor_user, e.actor_device, e.target_id),
        (AuditAction::AuthRecoveryCodeUsed, Some(bea.user.id), None, Some(bea.user.id.0))
    );
    // A wrong proof, or the same code again: nothing written.
    assert!(matches!(confirm_with(meta, bea.user.id, None, now + 5).await, Err(MetaError::TwoFactorInvalid)));
    assert!(matches!(
        meta.confirm_second_factor(bea.user.id, Some(&Proof::Recovery(bea.hashes()[3])), now + 6).await,
        Err(MetaError::TwoFactorInvalid)
    ));
    assert!(seen.actions().await.is_empty());
}

/// An error in the log fails every two-factor change and changes nothing; a wrong second step is still answered as wrong, since
/// its event is written after the answer is decided and only reported.
async fn an_error_in_the_log_fails_every_two_factor_change(url: &str) {
    sqlx::any::install_default_drivers();
    let meta = Meta::open(url).await.unwrap();
    let me = protected(&meta, "ana").await;
    let (_, admin) = admin_actor(&meta, "root").await;
    let setup = user(&meta, "setup").await;
    let setup_device = device(&meta, &setup).await;
    let setup_actor = DeviceActor { user: setup.id, device: setup_device, password_hash: "$argon2id$stub".to_owned() };
    meta.two_factor_begin_setup(&setup_actor, b"cipher", SETUP_AT).await.unwrap();
    let plain = user(&meta, "plain").await;
    let locker = protected(&meta, "locker").await;
    let events_before = all_events(&meta).await.len();
    let now = SETUP_AT + 100;
    let generation = gen_of(&meta, "ana").await;
    let chal = new_challenge();
    meta.sign_in(me.user.id, ("$argon2id$stub", generation), "pc", &DeviceClient::default(), &token(), &chal, now).await.unwrap();
    let invite = code(INVITE_CHARS);
    meta.create_invite("ana", &invite.hash(), now, INVITE_TTL_MS).await.unwrap();

    break_audit(url).await;
    let is_db = |r: Result<(), MetaError>| assert!(matches!(r, Err(MetaError::Db(_))), "{r:?}");
    let hashes = me.hashes();
    is_db(meta.two_factor_confirm(setup.id, setup_device, &*right(FIRST_STEP), &hashes, now).await.map(|_| ()));
    is_db(meta.two_factor_new_recovery_codes(&me.actor(), &totp(&right(FIRST_STEP + 1)), &hashes, now).await);
    is_db(meta.two_factor_disable(&me.actor(), &Proof::Recovery(hashes[0]), now).await.map(|_| ()));
    is_db(meta.confirm_second_factor(me.user.id, Some(&Proof::Recovery(hashes[1])), now).await.map(|_| ()));
    is_db(meta.complete_second_factor(&chal.id_hash, &totp(&right(FIRST_STEP + 1)), &token(), now).await.map(|_| ()));
    is_db(redeem_with_factor(&meta, &invite.hash(), Some(&Proof::Recovery(hashes[2])), now).await.map(|_| ()));
    is_db(meta.sign_in(plain.id, ("$argon2id$stub", 0), "pc", &DeviceClient::default(), &token(), &new_challenge(), now).await.map(|_| ()));
    is_db(meta.admin_reset_two_factor(&admin, me.user.id, now).await.map(|_| ()));
    is_db(meta.reset_two_factor(me.user.id, now).await.map(|_| ()));
    // A wrong second step: the answer is the same, whether or not the log works.
    let result = meta.complete_second_factor(&chal.id_hash, &totp(&wrong()), &token(), now).await;
    assert!(matches!(result, Err(MetaError::TwoFactorInvalid)), "{result:?}");
    // The lock is set by the tenth wrong proof although its event can't be written: failing closed there would let a fault in
    // the log undo the lock. The answer is the same and the counters and the lock are committed.
    for n in 1..=10 {
        let result = confirm_with(&meta, locker.user.id, None, now).await;
        assert!(matches!(result, Err(MetaError::TwoFactorInvalid)), "wrong proof {n}: {result:?}");
    }
    assert_eq!(meta.two_factor_locked_for(locker.user.id, now).await.unwrap(), Some(15 * MINUTE), "the lock holds");
    repair_audit(url).await;

    // Nothing was changed by any of it: still on, no code used, nobody signed out, the set-up still open.
    assert_eq!(all_events(&meta).await.len(), events_before, "no event, no change (the lock's event was lost, only reported)");
    assert!(meta.two_factor_enabled(me.user.id).await.unwrap());
    assert_eq!(meta.two_factor_info(me.user.id, now).await.unwrap().recovery_codes_left, 10);
    assert_eq!(live_devices(&meta, &me.user).await, vec![me.first]);
    assert!(meta.two_factor_info(setup.id, now).await.unwrap().pending);
    assert!(!meta.two_factor_enabled(setup.id).await.unwrap());
    assert_eq!(meta.invite_needs_password(&invite.hash(), now).await.unwrap(), Some(false), "the invite is still there");
    // And the same changes work once the log does.
    meta.two_factor_new_recovery_codes(&me.actor(), &totp(&right(FIRST_STEP + 2)), &hashes, now + 1).await.unwrap();
}

#[tokio::test]
async fn an_error_in_the_log_fails_every_two_factor_change_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    an_error_in_the_log_fails_every_two_factor_change(&format!("sqlite:{}", dir.path().join("meta.db").display())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_error_in_the_log_fails_every_two_factor_change_on_postgres() {
    let Some((url, _guard)) = postgres_database().await else {
        eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set");
        return;
    };
    an_error_in_the_log_fails_every_two_factor_change(&url).await;
}

// ---- web sessions (migration 0011) ----

const WEB_IDLE_MS: i64 = 8 * 60 * MINUTE;
const WEB_ABSOLUTE_MS: i64 = 7 * 24 * 60 * MINUTE;

fn new_session() -> NewWebSession {
    NewWebSession { id_hash: token(), absolute_ms: WEB_ABSOLUTE_MS }
}

/// An administrator with a password hash the web sign-ins below check against.
async fn web_admin(meta: &Meta, username: &str) -> UserRecord {
    meta.create_user(username, ADMIN_HASH, Role::Admin, NOW).await.unwrap()
}

/// A web sign-in of `user` as the handler would make it after verifying the password.
async fn web_sign_in(meta: &Meta, user: &UserRecord, session: &NewWebSession, now: i64) -> Option<WebSignIn> {
    let generation = gen_of(meta, &user.username).await;
    meta.web_sign_in(user.id, (&user.password_hash, generation), &new_challenge(), session, now).await.unwrap()
}

async fn session_is_valid(meta: &Meta, session: &NewWebSession, now: i64) -> bool {
    meta.web_session(&session.id_hash, now, WEB_IDLE_MS).await.unwrap().is_some()
}

/// Only administrators get a session; it is found by its token hash, ends by idle time, by its absolute lifetime and by
/// logout, and the sign-in is written to the log without a device.
async fn web_sessions_are_made_looked_up_expire_and_log_out(meta: &Meta) {
    let root = web_admin(meta, "root").await;
    let member = user(meta, "ana").await;
    // A member with the right password gets nothing, the same as a wrong generation or a wrong hash.
    assert_eq!(web_sign_in(meta, &member, &new_session(), NOW).await, None);
    let stale = meta.web_sign_in(root.id, (&root.password_hash, 99), &new_challenge(), &new_session(), NOW).await.unwrap();
    assert_eq!(stale, None, "the password was checked against another generation");
    let wrong_hash = meta.web_sign_in(root.id, ("$argon2id$other", root.auth_gen), &new_challenge(), &new_session(), NOW).await.unwrap();
    assert_eq!(wrong_hash, None);
    assert_eq!(meta.web_sessions_open(member.id, NOW, WEB_IDLE_MS).await.unwrap(), 0);

    let session = new_session();
    assert_eq!(web_sign_in(meta, &root, &session, NOW).await, Some(WebSignIn::Session));
    let found = meta.web_session(&session.id_hash, NOW + 1, WEB_IDLE_MS).await.unwrap().unwrap();
    assert_eq!((found.user, found.username.as_str(), found.role, found.mfa_verified_ms), (root.id, "root", Role::Admin, None));
    assert_eq!((found.created_ms, found.last_seen_ms, found.absolute_expires_ms), (NOW, NOW, NOW + WEB_ABSOLUTE_MS));
    assert!(meta.web_session(&token(), NOW + 1, WEB_IDLE_MS).await.unwrap().is_none(), "an unknown token");
    let events = all_events(meta).await;
    let login = events.iter().find(|e| e.action == AuditAction::AuthLogin).unwrap();
    assert_eq!((login.actor_user, login.actor_device), (Some(root.id), None));
    assert_eq!(details(login), json(r#"{"client_kind":"web"}"#));
    assert!(DeviceClient::new(Some("web"), None, None).is_err(), "devices cannot claim web");

    // Idle: valid just inside the idle time, over at it, and a use moves it.
    assert!(session_is_valid(meta, &session, NOW + WEB_IDLE_MS - 1).await);
    assert!(!session_is_valid(meta, &session, NOW + WEB_IDLE_MS).await);
    meta.web_session_seen(&session.id_hash, NOW + WEB_IDLE_MS - 1).await.unwrap();
    assert!(session_is_valid(meta, &session, NOW + 2 * WEB_IDLE_MS - 2).await, "a use starts the idle time again");
    // The time never moves back.
    meta.web_session_seen(&session.id_hash, NOW + 5).await.unwrap();
    assert_eq!(meta.web_session(&session.id_hash, NOW + WEB_IDLE_MS, WEB_IDLE_MS).await.unwrap().unwrap().last_seen_ms, NOW + WEB_IDLE_MS - 1);

    // Absolute: keeping it busy does not help.
    let busy = new_session();
    assert_eq!(web_sign_in(meta, &root, &busy, NOW).await, Some(WebSignIn::Session));
    let mut at = NOW;
    while at + WEB_IDLE_MS / 2 < NOW + WEB_ABSOLUTE_MS {
        at += WEB_IDLE_MS / 2;
        meta.web_session_seen(&busy.id_hash, at).await.unwrap();
        assert!(session_is_valid(meta, &busy, at).await);
    }
    assert!(!session_is_valid(meta, &busy, NOW + WEB_ABSOLUTE_MS).await, "the absolute lifetime ends it");

    // Logout ends it at once, and a second logout, or one of an unknown session, is no error.
    let out = new_session();
    assert_eq!(web_sign_in(meta, &root, &out, NOW + 10).await, Some(WebSignIn::Session));
    assert!(session_is_valid(meta, &out, NOW + 11).await);
    meta.web_logout(&out.id_hash, NOW + 12).await.unwrap();
    meta.web_logout(&out.id_hash, NOW + 13).await.unwrap();
    meta.web_logout(&token(), NOW + 13).await.unwrap();
    assert!(!session_is_valid(meta, &out, NOW + 14).await);
    meta.web_session_seen(&out.id_hash, NOW + 15).await.unwrap();
    assert!(!session_is_valid(meta, &out, NOW + 16).await, "a use does not revive a logged-out session");
}

/// An administrator with two-factor sign-in on, and a session made with it (the sign-in's second step). `step` is the time
/// step of the code that made the session.
async fn web_admin_with_factor(meta: &Meta, username: &str, step: i64, now: i64) -> (UserRecord, DeviceId, NewWebSession) {
    let me = web_admin(meta, username).await;
    let first = device(meta, &me).await;
    let hashes: Vec<[u8; 32]> = (0..RECOVERY_CODES).map(|_| code(INVITE_CHARS).hash()).collect();
    let setup = DeviceActor { user: me.id, device: first, password_hash: ADMIN_HASH.to_owned() };
    meta.two_factor_begin_setup(&setup, b"cipher-of-the-secret", now).await.unwrap();
    meta.two_factor_confirm(me.id, first, &*right(step), &hashes, now + 1).await.unwrap();
    let (chal, session) = (new_challenge(), new_session());
    let generation = gen_of(meta, username).await;
    let made = meta.web_sign_in(me.id, (ADMIN_HASH, generation), &chal, &session, now + 2).await.unwrap();
    assert!(matches!(made, Some(WebSignIn::Challenge { .. })));
    meta.web_complete_second_factor(&chal.id_hash, &totp(&right(step + 1)), &session, now + 3).await.unwrap();
    assert!(session_is_valid(meta, &session, now + 4).await);
    (me, first, session)
}

/// Every change that raises `users.auth_gen` ends the account's sessions: a password change and reset, a disable, an enable
/// after it, a removal, and two-factor sign-in turned on, off, or reset by an admin or the command line.
async fn a_web_session_ends_with_every_change_of_the_accounts_generation(meta: &Meta) {
    // The acting admin has two-factor sign-in, which resetting another administrator's needs.
    let (boss, plain) = admin_actor(meta, "boss").await;
    let boss_device = device_of(&plain);
    let hashes: Vec<[u8; 32]> = (0..RECOVERY_CODES).map(|_| code(INVITE_CHARS).hash()).collect();
    let setup = DeviceActor { user: boss.id, device: boss_device, password_hash: ADMIN_HASH.to_owned() };
    meta.two_factor_begin_setup(&setup, b"cipher-of-the-secret", NOW).await.unwrap();
    meta.two_factor_confirm(boss.id, boss_device, &*right(FIRST_STEP), &hashes, NOW + 1).await.unwrap();
    let actor = AdminActor { two_factor: true, ..plain };
    let mut now = NOW + 10;

    // A password reset by the command line.
    let root = web_admin(meta, "root").await;
    let s = new_session();
    assert_eq!(web_sign_in(meta, &root, &s, now).await, Some(WebSignIn::Session));
    meta.reset_password(root.id, "$argon2id$new", true, now + 1).await.unwrap();
    assert!(!session_is_valid(meta, &s, now + 2).await, "a password reset ends the session");
    now += 10;

    // A password change by a device.
    let (ana, ana_actor) = admin_actor(meta, "ana").await;
    let s = new_session();
    assert_eq!(web_sign_in(meta, &ana, &s, now).await, Some(WebSignIn::Session));
    meta.change_password(ana.id, ADMIN_HASH, "$argon2id$changed", device_of(&ana_actor), now + 1).await.unwrap();
    assert!(!session_is_valid(meta, &s, now + 2).await, "a password change ends the session");
    now += 10;

    // Disable ends it, and an enable afterwards does not bring it back.
    let target = web_admin(meta, "target").await;
    let s = new_session();
    assert_eq!(web_sign_in(meta, &target, &s, now).await, Some(WebSignIn::Session));
    meta.admin_disable_user(&actor, target.id, now + 1).await.unwrap();
    assert!(!session_is_valid(meta, &s, now + 2).await, "a disabled account's session is over");
    meta.admin_enable_user(&actor, target.id, now + 3).await.unwrap();
    assert!(!session_is_valid(meta, &s, now + 4).await, "enabling does not revive it");
    now += 10;

    // A removal.
    let doomed = web_admin(meta, "doomed").await;
    let s = new_session();
    assert_eq!(web_sign_in(meta, &doomed, &s, now).await, Some(WebSignIn::Session));
    meta.admin_remove_user(&actor, doomed.id, now + 1).await.unwrap();
    assert!(!session_is_valid(meta, &s, now + 2).await, "a removed account's session is over");
    now += 10;

    // Two-factor sign-in turned on: a session from before it is over (a set-up that is not confirmed changes nothing).
    let guarded = web_admin(meta, "guarded").await;
    let guarded_device = device(meta, &guarded).await;
    let s = new_session();
    assert_eq!(web_sign_in(meta, &guarded, &s, now).await, Some(WebSignIn::Session));
    let setup = DeviceActor { user: guarded.id, device: guarded_device, password_hash: ADMIN_HASH.to_owned() };
    meta.two_factor_begin_setup(&setup, b"cipher-of-the-secret", now + 1).await.unwrap();
    assert!(session_is_valid(meta, &s, now + 2).await, "an open set-up is no change");
    let hashes: Vec<[u8; 32]> = (0..RECOVERY_CODES).map(|_| code(INVITE_CHARS).hash()).collect();
    meta.two_factor_confirm(guarded.id, guarded_device, &*right(FIRST_STEP), &hashes, now + 3).await.unwrap();
    assert!(!session_is_valid(meta, &s, now + 4).await, "turning two-factor sign-in on ends the session");
    now += 10;

    // Turned off by the account itself.
    let (own, own_device, s) = web_admin_with_factor(meta, "own", FIRST_STEP, now).await;
    let setup = DeviceActor { user: own.id, device: own_device, password_hash: ADMIN_HASH.to_owned() };
    meta.two_factor_disable(&setup, &totp(&right(FIRST_STEP + 5)), now + 10).await.unwrap();
    assert!(!session_is_valid(meta, &s, now + 11).await, "turning two-factor sign-in off ends the session");
    now += 20;

    // Reset by an admin, and by the command line.
    let (by_admin, _, s) = web_admin_with_factor(meta, "byadmin", FIRST_STEP, now).await;
    meta.admin_reset_two_factor(&actor, by_admin.id, now + 10).await.unwrap();
    assert!(!session_is_valid(meta, &s, now + 11).await, "an admin's reset ends the session");
    now += 20;
    let (by_cli, _, s) = web_admin_with_factor(meta, "bycli", FIRST_STEP, now).await;
    meta.reset_two_factor(by_cli.id, now + 10).await.unwrap();
    assert!(!session_is_valid(meta, &s, now + 11).await, "the command line's reset ends the session");

    // Another account's session is untouched by all of that.
    let bystander = web_admin(meta, "bystander").await;
    let s = new_session();
    assert_eq!(web_sign_in(meta, &bystander, &s, now + 30).await, Some(WebSignIn::Session));
    meta.reset_password(root.id, "$argon2id$again", true, now + 31).await.unwrap();
    assert!(session_is_valid(meta, &s, now + 32).await);
}

/// An admin with two-factor sign-in gets a challenge of kind `web`, which no device sign-in can use and which makes no
/// device, and completes it with a code, once. The second factor's counters and lock are the device sign-in's.
async fn a_web_sign_in_with_two_factor_needs_its_own_challenge(meta: &Meta) {
    let me = web_admin(meta, "root").await;
    let first = device(meta, &me).await;
    let hashes: Vec<[u8; 32]> = (0..RECOVERY_CODES).map(|_| code(INVITE_CHARS).hash()).collect();
    let setup = DeviceActor { user: me.id, device: first, password_hash: ADMIN_HASH.to_owned() };
    meta.two_factor_begin_setup(&setup, b"cipher-of-the-secret", SETUP_AT).await.unwrap();
    meta.two_factor_confirm(me.id, first, &*right(FIRST_STEP), &hashes, SETUP_AT + 1).await.unwrap();

    let generation = gen_of(meta, "root").await;
    let chal = new_challenge();
    let session = new_session();
    let made = meta.web_sign_in(me.id, (ADMIN_HASH, generation), &chal, &session, SETUP_AT + 10).await.unwrap();
    assert_eq!(made, Some(WebSignIn::Challenge { expires_ms: SETUP_AT + 10 + 5 * MINUTE }));
    assert!(!session_is_valid(meta, &session, SETUP_AT + 11).await, "no session without the second factor");
    assert_eq!(live_devices(meta, &me).await, vec![first], "and no device");
    // The challenge is seen as a web one only.
    assert_eq!(meta.web_challenge_owner(&chal.id_hash, SETUP_AT + 11).await.unwrap(), Some((me.id, "root".to_owned())));
    assert_eq!(meta.challenge_owner(&chal.id_hash, SETUP_AT + 11).await.unwrap(), None, "a device sign-in does not see it");
    let as_device = meta.complete_second_factor(&chal.id_hash, &totp(&right(FIRST_STEP + 1)), &token(), SETUP_AT + 12).await;
    assert!(matches!(as_device, Err(MetaError::ChallengeGone)), "{as_device:?}");
    assert_eq!(live_devices(meta, &me).await, vec![first], "still no new device, and the code was not used");
    // And the other way round.
    let device_chal = new_challenge();
    let made = meta.sign_in(me.id, (ADMIN_HASH, generation), "pc", &DeviceClient::default(), &token(), &device_chal, SETUP_AT + 13).await.unwrap();
    assert!(matches!(made, Some(SignIn::Challenge { .. })));
    let as_web = meta.web_complete_second_factor(&device_chal.id_hash, &totp(&right(FIRST_STEP + 1)), &session, SETUP_AT + 14).await;
    assert!(matches!(as_web, Err(MetaError::ChallengeGone)), "{as_web:?}");
    assert!(!session_is_valid(meta, &session, SETUP_AT + 15).await);

    // Wrong codes are counted (four, and the fifth would delete the challenge), then the right one makes the session, once.
    for _ in 0..4 {
        let result = meta.web_complete_second_factor(&chal.id_hash, &totp(&wrong()), &session, SETUP_AT + 20).await;
        assert!(matches!(result, Err(MetaError::TwoFactorInvalid)), "{result:?}");
    }
    let done = meta.web_complete_second_factor(&chal.id_hash, &totp(&right(FIRST_STEP + 1)), &session, SETUP_AT + 21).await.unwrap();
    assert_eq!(done, me.id);
    let found = meta.web_session(&session.id_hash, SETUP_AT + 22, WEB_IDLE_MS).await.unwrap().unwrap();
    assert_eq!(found.mfa_verified_ms, Some(SETUP_AT + 21), "the session says when the second factor was given");
    assert_eq!(live_devices(meta, &me).await, vec![first], "a web sign-in makes no device");
    let login = all_events(meta).await.into_iter().find(|event| event.action == AuditAction::AuthLogin).unwrap();
    assert_eq!(details(&login), json(r#"{"client_kind":"web"}"#));
    let failed = all_events(meta).await.into_iter().find(|event| event.action == AuditAction::AuthSecondFactorFailed).unwrap();
    assert_eq!(details(&failed), json(r#"{"client_kind":"web","step":"totp"}"#));
    let again = meta.web_complete_second_factor(&chal.id_hash, &totp(&right(FIRST_STEP + 2)), &new_session(), SETUP_AT + 23).await;
    assert!(matches!(again, Err(MetaError::ChallengeGone)), "once: {again:?}");

    // The same time step is not accepted again (the counter is the device sign-in's and the admin confirmation's).
    let generation = gen_of(meta, "root").await;
    let replay = new_challenge();
    meta.web_sign_in(me.id, (ADMIN_HASH, generation), &replay, &new_session(), SETUP_AT + 24).await.unwrap();
    let replayed = meta.web_complete_second_factor(&replay.id_hash, &totp(&right(FIRST_STEP + 1)), &new_session(), SETUP_AT + 25).await;
    assert!(matches!(replayed, Err(MetaError::TwoFactorInvalid)), "{replayed:?}");
    // Wrong proofs lock the second factor for every way of using it: a device sign-in's step too.
    for n in 0..12 {
        let _ = confirm_with(meta, me.id, None, SETUP_AT + 30 + n).await;
    }
    let locked = meta.web_complete_second_factor(&replay.id_hash, &totp(&right(FIRST_STEP + 9)), &new_session(), SETUP_AT + 50).await;
    assert!(matches!(locked, Err(MetaError::TwoFactorLocked { .. })), "{locked:?}");
    let locked = meta.complete_second_factor(&device_chal.id_hash, &totp(&right(FIRST_STEP + 9)), &token(), SETUP_AT + 51).await;
    assert!(matches!(locked, Err(MetaError::TwoFactorLocked { .. })), "{locked:?}");
}

/// Pruning deletes sessions that are over (logged out, expired, idle), never a valid one, and at most the batch asked for.
async fn web_sessions_are_pruned_in_bounded_batches(meta: &Meta) {
    let root = web_admin(meta, "root").await;
    let late = NOW + WEB_ABSOLUTE_MS - 2 * WEB_IDLE_MS;
    let now = NOW + WEB_ABSOLUTE_MS - WEB_IDLE_MS;
    let keep = new_session();
    assert_eq!(web_sign_in(meta, &root, &keep, now - MINUTE).await, Some(WebSignIn::Session));
    for n in 0..5 {
        assert_eq!(web_sign_in(meta, &root, &new_session(), NOW + n).await, Some(WebSignIn::Session));
    }
    let out = new_session();
    assert_eq!(web_sign_in(meta, &root, &out, late).await, Some(WebSignIn::Session));
    meta.web_logout(&out.id_hash, late + 1).await.unwrap();

    assert_eq!(meta.web_sessions_count_over(now, WEB_IDLE_MS).await.unwrap(), 6, "five idle ones and the logged-out one");
    assert_eq!(meta.web_sessions_prune(now, WEB_IDLE_MS, 4).await.unwrap(), 4, "a batch is bounded");
    assert_eq!(meta.web_sessions_count_over(now, WEB_IDLE_MS).await.unwrap(), 2);
    assert_eq!(meta.web_sessions_prune(now, WEB_IDLE_MS, 4).await.unwrap(), 2);
    assert_eq!(meta.web_sessions_prune(now, WEB_IDLE_MS, 4).await.unwrap(), 0, "nothing is left to delete");
    assert!(session_is_valid(meta, &keep, now).await, "a valid session is never pruned");
    // Past the absolute lifetime the last one goes too.
    let end = NOW + 2 * WEB_ABSOLUTE_MS;
    assert_eq!(meta.web_sessions_prune(end, WEB_IDLE_MS, 100).await.unwrap(), 1);
    assert_eq!(meta.web_sessions_count_over(end, WEB_IDLE_MS).await.unwrap(), 0);
}

/// An admin change made on a browser session is audited without a device, and a session that was logged out, or whose account
/// changed, can no longer make one.
async fn a_web_actor_changes_accounts_until_its_session_ends(meta: &Meta) {
    // The store judges the session by its own clock, which follows the time each call below is made at.
    let clock = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(NOW));
    let ticking = clock.clone();
    let meta = &meta.clone().with_clock(std::sync::Arc::new(move || ticking.load(std::sync::atomic::Ordering::SeqCst)));
    let at_ms = |t: i64| {
        clock.store(t, std::sync::atomic::Ordering::SeqCst);
        t
    };
    let root = web_admin(meta, "root").await;
    let session = new_session();
    assert_eq!(web_sign_in(meta, &root, &session, NOW).await, Some(WebSignIn::Session));
    let actor = AdminActor {
        user: root.id,
        via: AdminVia::Web { id_hash: session.id_hash, idle_ms: WEB_IDLE_MS },
        password_hash: ADMIN_HASH.to_owned(),
        two_factor: false,
    };
    assert!(!format!("{actor:?}").contains(&format!("{:?}", session.id_hash)), "the session's hash is not in Debug");
    let ana = user(meta, "ana").await;
    meta.admin_update_user(&actor, ana.id, None, Some(Some(MIN_QUOTA_BYTES)), at_ms(NOW + 1)).await.unwrap();
    let events = all_events(meta).await;
    let changed = events.iter().find(|e| e.action == AuditAction::AdminQuotaChanged).unwrap();
    assert_eq!((changed.actor_user, changed.actor_device), (Some(root.id), None), "no device acted");
    // Another account's session is not this admin's.
    let other = web_admin(meta, "other").await;
    let theirs = new_session();
    assert_eq!(web_sign_in(meta, &other, &theirs, NOW).await, Some(WebSignIn::Session));
    let borrowed = AdminActor { via: AdminVia::Web { id_hash: theirs.id_hash, idle_ms: WEB_IDLE_MS }, ..actor.clone() };
    assert!(matches!(meta.admin_update_user(&borrowed, ana.id, None, Some(None), at_ms(NOW + 2)).await, Err(MetaError::DeviceRevoked)));
    // A session that does not exist.
    let unknown = AdminActor { via: AdminVia::Web { id_hash: token(), idle_ms: WEB_IDLE_MS }, ..actor.clone() };
    assert!(matches!(meta.admin_update_user(&unknown, ana.id, None, Some(None), at_ms(NOW + 3)).await, Err(MetaError::DeviceRevoked)));
    // Logged out: refused, and nothing changed.
    meta.web_logout(&session.id_hash, NOW + 4).await.unwrap();
    assert!(matches!(meta.admin_update_user(&actor, ana.id, None, Some(None), at_ms(NOW + 5)).await, Err(MetaError::DeviceRevoked)));
    assert_eq!(admin_view(meta, ana.id).await.quota_bytes, Some(MIN_QUOTA_BYTES));
    // A session that expired between the request's start and the change (the handler's check was passed, the transaction runs
    // later): refused by the idle time and by the absolute lifetime, and nothing changed.
    let slow = new_session();
    assert_eq!(web_sign_in(meta, &root, &slow, NOW + 20).await, Some(WebSignIn::Session));
    let slow_actor = AdminActor { via: AdminVia::Web { id_hash: slow.id_hash, idle_ms: WEB_IDLE_MS }, ..actor.clone() };
    let idle = meta.admin_update_user(&slow_actor, ana.id, None, Some(None), at_ms(NOW + 20 + WEB_IDLE_MS)).await;
    assert!(matches!(idle, Err(MetaError::DeviceRevoked)), "idle: {idle:?}");
    meta.admin_update_user(&slow_actor, ana.id, None, Some(None), at_ms(NOW + 20 + WEB_IDLE_MS - 1)).await.unwrap();
    let busy = new_session();
    assert_eq!(web_sign_in(meta, &root, &busy, NOW + 30).await, Some(WebSignIn::Session));
    let busy_actor = AdminActor { via: AdminVia::Web { id_hash: busy.id_hash, idle_ms: WEB_IDLE_MS }, ..actor.clone() };
    let mut at = NOW + 30;
    while at + WEB_IDLE_MS / 2 < NOW + 30 + WEB_ABSOLUTE_MS {
        at += WEB_IDLE_MS / 2;
        meta.web_session_seen(&busy.id_hash, at).await.unwrap();
    }
    meta.admin_update_user(&busy_actor, ana.id, None, Some(None), at_ms(at)).await.unwrap();
    let absolute = meta.admin_update_user(&busy_actor, ana.id, None, Some(Some(MIN_QUOTA_BYTES)), at_ms(NOW + 30 + WEB_ABSOLUTE_MS)).await;
    assert!(matches!(absolute, Err(MetaError::DeviceRevoked)), "absolute: {absolute:?}");
    assert_eq!(admin_view(meta, ana.id).await.quota_bytes, None);
    // A session of a changed account: the generation moved on.
    let late = new_session();
    assert_eq!(web_sign_in(meta, &root, &late, NOW + 6).await, Some(WebSignIn::Session));
    let late_actor = AdminActor { via: AdminVia::Web { id_hash: late.id_hash, idle_ms: WEB_IDLE_MS }, ..actor.clone() };
    meta.admin_update_user(&late_actor, ana.id, None, Some(None), at_ms(NOW + 7)).await.unwrap();
    meta.reset_password(root.id, "$argon2id$new", true, NOW + 8).await.unwrap();
    let after = meta.admin_update_user(&late_actor, ana.id, None, Some(Some(MIN_QUOTA_BYTES)), at_ms(NOW + 9)).await;
    assert!(matches!(after, Err(MetaError::DeviceRevoked)), "{after:?}");
    assert_eq!(admin_view(meta, ana.id).await.quota_bytes, None);
}

// ---- the migration from 0010 to 0011 ----

/// Drops what migration 0011 added and forgets it ran, which leaves the database as 0010 left it (with its rows).
async fn back_to_0010(url: &str) {
    use sqlx::Connection;
    let mut conn = sqlx::AnyConnection::connect(url).await.unwrap();
    drop_history_trash_and_manifest_index(&mut conn).await;
    sqlx::query("DROP TABLE web_sessions").execute(&mut conn).await.unwrap();
    sqlx::query("DELETE FROM schema_version WHERE version >= 11").execute(&mut conn).await.unwrap();
}

/// A database from the release before the web admin keeps its accounts, devices and audit log, gets an empty session table
/// that works (also after the migration is rolled back and run again), and a database newer than the program is still refused.
async fn migrating_from_0010_adds_the_web_sessions(url: &str) {
    sqlx::any::install_default_drivers();
    let (root, old_token, events) = {
        let meta = Meta::open(url).await.unwrap();
        let root = web_admin(&meta, "root").await;
        let old_token = token();
        meta.create_device(root.id, "old pc", &old_token, NOW).await.unwrap();
        meta.sign_in_device(root.id, (&root.password_hash, root.auth_gen), "pc", &DeviceClient::default(), &token(), NOW).await.unwrap().unwrap();
        let events = all_events(&meta).await.len();
        (root, old_token, events)
    };
    back_to_0010(url).await;
    assert_eq!(schema_version(url).await, 10);

    let meta = Meta::open(url).await.unwrap();
    assert_eq!(schema_version(url).await, 16, "the current schema includes history, trash and the reference index after web sessions");
    assert_eq!(meta.device_by_token(&old_token).await.unwrap().map(|a| a.user), Some(root.id));
    assert_eq!(all_events(&meta).await.len(), events, "the audit rows are kept");
    let session = new_session();
    assert_eq!(web_sign_in(&meta, &root, &session, NOW + 5).await, Some(WebSignIn::Session));
    assert!(session_is_valid(&meta, &session, NOW + 6).await);

    // The rollback: the table goes, the version is forgotten, and opening again makes the table again.
    drop(meta);
    back_to_0010(url).await;
    assert_eq!(schema_version(url).await, 10);
    let meta = Meta::open(url).await.unwrap();
    assert_eq!(schema_version(url).await, 16);
    assert!(!session_is_valid(&meta, &session, NOW + 6).await, "the table is new and empty");
    drop(meta);

    // A database from a newer Craft Cloud is still refused.
    use sqlx::Connection;
    let mut conn = sqlx::AnyConnection::connect(url).await.unwrap();
    sqlx::query("INSERT INTO schema_version (version) VALUES (16)").execute(&mut conn).await.unwrap();
    drop(conn);
    assert!(matches!(Meta::open(url).await, Err(MetaError::Corrupt(_))));
}

#[tokio::test]
async fn migrating_from_0010_adds_the_web_sessions_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    migrating_from_0010_adds_the_web_sessions(&format!("sqlite:{}", dir.path().join("meta.db").display())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn migrating_from_0010_adds_the_web_sessions_on_postgres() {
    let Some((url, _guard)) = postgres_database().await else {
        eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set");
        return;
    };
    migrating_from_0010_adds_the_web_sessions(&url).await;
}

// ---- a change that waits for its locks while its session ends ----

/// The time a change judges its session by is read after the change has its locks: one that waited while the session went idle
/// is refused, and changes nothing.
async fn a_session_that_ends_while_a_change_waits_for_its_locks_does_not_authorise_it(url: &str) {
    use sqlx::Connection;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};
    sqlx::any::install_default_drivers();
    let now = Arc::new(AtomicI64::new(NOW));
    let ticking = now.clone();
    let meta = Meta::open(url).await.unwrap().with_clock(Arc::new(move || ticking.load(Ordering::SeqCst)));
    let root = web_admin(&meta, "root").await;
    let ana = user(&meta, "ana").await;
    let session = new_session();
    assert_eq!(web_sign_in(&meta, &root, &session, NOW).await, Some(WebSignIn::Session));
    let actor = AdminActor {
        user: root.id,
        via: AdminVia::Web { id_hash: session.id_hash, idle_ms: WEB_IDLE_MS },
        password_hash: ADMIN_HASH.to_owned(),
        two_factor: false,
    };
    // Another connection holds the write lock (SQLite) or the admin's row (Postgres) until the test lets go.
    let mut holder = sqlx::AnyConnection::connect(url).await.unwrap();
    if url.starts_with("sqlite:") {
        sqlx::query("BEGIN IMMEDIATE").execute(&mut holder).await.unwrap();
    } else {
        sqlx::query("BEGIN").execute(&mut holder).await.unwrap();
        sqlx::query("UPDATE users SET username = username WHERE username = 'root'").execute(&mut holder).await.unwrap();
    }
    let change = {
        let (meta, actor, ana) = (meta.clone(), actor.clone(), ana.id);
        tokio::spawn(async move { meta.admin_update_user(&actor, ana, None, Some(Some(MIN_QUOTA_BYTES)), NOW + 1).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert!(!change.is_finished(), "the change is waiting for the lock");
    now.store(NOW + WEB_IDLE_MS + 5, Ordering::SeqCst);
    sqlx::query("COMMIT").execute(&mut holder).await.unwrap();
    let result = change.await.unwrap();
    assert!(matches!(result, Err(MetaError::DeviceRevoked)), "{result:?}");
    assert_eq!(admin_view(&meta, ana.id).await.quota_bytes, None, "nothing changed");
    // The same change while the session is valid goes through.
    now.store(NOW + 2, Ordering::SeqCst);
    meta.admin_update_user(&actor, ana.id, None, Some(Some(MIN_QUOTA_BYTES)), NOW + 2).await.unwrap();
}

#[tokio::test]
async fn a_session_that_ends_while_a_change_waits_for_its_locks_does_not_authorise_it_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    a_session_that_ends_while_a_change_waits_for_its_locks_does_not_authorise_it(&format!("sqlite:{}", dir.path().join("meta.db").display())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_that_ends_while_a_change_waits_for_its_locks_does_not_authorise_it_on_postgres() {
    let Some((url, _guard)) = postgres_database().await else {
        eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set");
        return;
    };
    a_session_that_ends_while_a_change_waits_for_its_locks_does_not_authorise_it(&url).await;
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
        undo_team_spaces(&mut conn).await;
        for statement in [
            "DROP TABLE manifest_chunks",
            "DROP TABLE manifest_index",
            "ALTER TABLE blobs DROP COLUMN physical_size",
            "ALTER TABLE blobs DROP COLUMN compress_level",
            "DROP TABLE server_state",
            "DELETE FROM schema_version WHERE version >= 14",
        ] {
            sqlx::query(statement).execute(&mut conn).await.unwrap();
        }
    }
    let meta = Meta::open(url).await.unwrap();
    assert_eq!(meta.index_state(&m.id()).await.unwrap(), IndexState::Absent, "recorded before the index: not indexed");
    let audit = meta.index_audit(5).await.unwrap();
    assert_eq!((audit.manifests, audit.complete, audit.absent, audit.building), (1, 0, 1, 0));
    let mut conn = raw(url).await;
    let latest: i64 = sqlx::query_scalar("SELECT MAX(version) FROM schema_version").fetch_one(&mut conn).await.unwrap();
    assert_eq!(latest, 16);
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

/// A database from before stored sizes were tracked keeps its blobs, with the size unknown (never zero or guessed), and the
/// physical sizes recorded afterwards stay.
#[tokio::test]
async fn migrating_from_0014_leaves_stored_sizes_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}", dir.path().join("meta.db").display());
    let (chunk, other) = (random_chunk(), random_chunk());
    {
        let meta = Meta::open(&url).await.unwrap();
        let ana = user(&meta, "ana").await;
        meta.add_chunk(ana.space, &chunk, 500, NOW).await.unwrap();
        meta.add_chunk(ana.space, &other, 700, NOW).await.unwrap();
        meta.set_physical(BlobKind::Chunk, other.as_bytes(), 100, 3).await.unwrap();
    }
    {
        let mut conn = raw(&url).await;
        undo_team_spaces(&mut conn).await;
        for statement in [
            "ALTER TABLE blobs DROP COLUMN physical_size",
            "ALTER TABLE blobs DROP COLUMN compress_level",
            "DROP TABLE server_state",
            "DELETE FROM schema_version WHERE version >= 15",
        ] {
            sqlx::query(statement).execute(&mut conn).await.unwrap();
        }
    }
    assert_eq!(schema_version(&url).await, 14);
    let meta = Meta::open(&url).await.unwrap();
    assert_eq!(schema_version(&url).await, 16);
    let rows = meta.blob_page(None, 10).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.physical_size.is_none() && r.level.is_none()), "unknown, not zero: {rows:?}");
    assert_eq!(meta.stored_bytes(meta.all_spaces().await.unwrap()[0]).await.unwrap(), 1200, "the logical sizes are as they were");
    let totals = meta.storage_totals().await.unwrap();
    assert_eq!((totals.blobs, totals.known_blobs), (2, 0));
    meta.set_physical(BlobKind::Chunk, chunk.as_bytes(), 90, 3).await.unwrap();
    assert_eq!(meta.storage_totals().await.unwrap().known_blobs, 1);
    assert_eq!(meta.server_value("compression_level").await.unwrap(), None, "the table is there and empty");
}
