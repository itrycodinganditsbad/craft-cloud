//! The HTTP API end to end: a real server on a random loopback port, driven with an HTTP client. Every
//! scenario runs on SQLite + disk, and on Postgres + disk when `CRAFT_CLOUD_TEST_PG` is set. Scenarios
//! also run on Postgres + S3 when `CRAFT_CLOUD_TEST_PG` and the `CRAFT_CLOUD_TEST_S3_*` variables are
//! set (`CRAFT_CLOUD_REQUIRE_BACKENDS` turns a missing backend into a failure).

use std::time::{Duration, Instant};

use craft_cloud_meta::{Meta, Role};
use craft_cloud_proto::DocId;
use craft_cloud_proto::api::{
    CommitResponse, DeviceInfo, EPOCH_HEADER, ErrorBody, InfoResponse, LoginResponse, MissingResponse, PairingCreated, PairingState, PairingStatus,
    SpaceResponse,
};
use craft_cloud_proto::join::{Code, INVITE_CHARS, INVITE_TTL_MS, PAIRING_CHARS, PAIRING_TTL_MS};
use craft_cloud_proto::live::{
    AppName, ClientFrame, DocKind, LIVE_DOCUMENTS_PROTOCOL, LIVE_PROTOCOL_MAX, LIVE_PROTOCOL_MIN, MAX_SUBSCRIPTIONS, PROTOCOL_MISMATCH, ServerFrame,
    recents_doc,
};
use craft_cloud_proto::{
    ChangeSet, ChunkId, ChunkRef, ChunkerParamsId, FileHash, Manifest, ManifestId, Name, NewVersion, NodeId, NodeKind, Op, VersionId,
};
use craft_cloud_server::AppState;
use craft_cloud_server::auth::hash_password;
use craft_cloud_server::live;
use craft_cloud_server::state::Limits;
use craft_cloud_store::Store;
use craft_cloud_store::disk::DiskBackend;
use craft_cloud_store::s3::{S3Backend, S3Config};
use reqwest::{Client, StatusCode};
use serde_json::json;

struct Server {
    base: String,
    meta: Meta,
    client: Client,
    /// The server's password-check slots, so a test can keep them busy.
    password_checks: std::sync::Arc<tokio::sync::Semaphore>,
    /// The server's clock while a test holds it still (milliseconds; 0: the real clock).
    clock: std::sync::Arc<std::sync::atomic::AtomicI64>,
    shutdown: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    _dirs: Vec<tempfile::TempDir>,
    _pg: Option<PgGuard>,
}

impl Server {
    /// Asks the server to shut down, as SIGTERM would.
    fn stop(&self) {
        if let Some(tx) = self.shutdown.lock().unwrap().take() {
            let _ = tx.send(());
        }
    }
}

struct PgGuard {
    admin_url: String,
    name: String,
}

impl Drop for PgGuard {
    fn drop(&mut self) {
        let (admin_url, name) = (self.admin_url.clone(), self.name.clone());
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

async fn start(meta: Meta, store: Store, dirs: Vec<tempfile::TempDir>, pg: Option<PgGuard>) -> Server {
    start_with(meta, store, dirs, pg, Limits::default()).await
}

async fn start_with(meta: Meta, store: Store, dirs: Vec<tempfile::TempDir>, pg: Option<PgGuard>, limits: Limits) -> Server {
    let state = AppState::new(meta.clone(), store, vec![]).unwrap().with_limits(limits).with_two_factor(Some(test_box()));
    start_state(state, meta, dirs, pg).await
}

/// A server that believes `X-Forwarded-For` from `trusted` (SQLite and disk).
async fn start_trusting(meta: Meta, store: Store, dirs: Vec<tempfile::TempDir>, trusted: Vec<std::net::IpAddr>) -> Server {
    let state = AppState::new(meta.clone(), store, trusted).unwrap().with_two_factor(Some(test_box()));
    start_state(state, meta, dirs, None).await
}

async fn start_state(state: AppState, meta: Meta, dirs: Vec<tempfile::TempDir>, pg: Option<PgGuard>) -> Server {
    // The real clock, unless a test freezes it (see `Server::freeze`).
    let clock = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
    let ticking = clock.clone();
    let state = state.with_clock(std::sync::Arc::new(move || match ticking.load(std::sync::atomic::Ordering::SeqCst) {
        0 => craft_cloud_server::state::now_ms(),
        frozen => frozen,
    }));
    let password_checks = state.password_checks.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/api/v1", listener.local_addr().unwrap());
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(craft_cloud_server::serve(state, listener, async move {
        let _ = rx.await;
    }));
    Server { base, meta, client: Client::new(), password_checks, clock, shutdown: std::sync::Mutex::new(Some(tx)), _dirs: dirs, _pg: pg }
}

async fn sqlite_disk() -> Server {
    sqlite_disk_with(Limits::default()).await
}

/// SQLite and disk, with other [`Limits`].
async fn sqlite_disk_with(limits: Limits) -> Server {
    let db = tempfile::tempdir().unwrap();
    let blobs = tempfile::tempdir().unwrap();
    let meta = Meta::open(&format!("sqlite:{}", db.path().join("meta.db").display())).await.unwrap();
    let store = Store::new(DiskBackend::new(blobs.path()).unwrap());
    start_with(meta, store, vec![db, blobs], None, limits).await
}

async fn postgres_database() -> (Meta, PgGuard) {
    use sqlx::Connection;
    let admin_url = std::env::var("CRAFT_CLOUD_TEST_PG").unwrap();
    let name = format!("cc_test_{}", uuid::Uuid::now_v7().simple());
    let mut conn = sqlx::PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}"))).execute(&mut conn).await.unwrap();
    let guard = PgGuard { admin_url: admin_url.clone(), name: name.clone() };
    let (base, _) = admin_url.rsplit_once('/').unwrap();
    let meta = Meta::open(&format!("{base}/{name}")).await.unwrap();
    (meta, guard)
}

async fn postgres_disk() -> Option<Server> {
    let required = std::env::var_os("CRAFT_CLOUD_REQUIRE_BACKENDS").is_some();
    let Ok(_) = std::env::var("CRAFT_CLOUD_TEST_PG") else {
        assert!(!required, "CRAFT_CLOUD_REQUIRE_BACKENDS is set but CRAFT_CLOUD_TEST_PG is not");
        return None;
    };
    let (meta, guard) = postgres_database().await;
    let blobs = tempfile::tempdir().unwrap();
    let store = Store::new(DiskBackend::new(blobs.path()).unwrap());
    Some(start(meta, store, vec![blobs], Some(guard)).await)
}

async fn postgres_s3() -> Option<Server> {
    let required = std::env::var_os("CRAFT_CLOUD_REQUIRE_BACKENDS").is_some();
    let (Ok(_), Ok(endpoint)) = (std::env::var("CRAFT_CLOUD_TEST_PG"), std::env::var("CRAFT_CLOUD_TEST_S3_ENDPOINT")) else {
        assert!(!required, "CRAFT_CLOUD_REQUIRE_BACKENDS is set but CRAFT_CLOUD_TEST_PG / CRAFT_CLOUD_TEST_S3_ENDPOINT are not");
        return None;
    };
    let (meta, guard) = postgres_database().await;
    let var = |n: &str| std::env::var(n).unwrap();
    let s3 = S3Config {
        bucket: var("CRAFT_CLOUD_TEST_S3_BUCKET"),
        prefix: format!("api-test-{}", uuid::Uuid::now_v7()),
        region: "us-east-1".into(),
        endpoint: Some(endpoint),
        access_key_id: var("CRAFT_CLOUD_TEST_S3_ACCESS_KEY"),
        secret_access_key: var("CRAFT_CLOUD_TEST_S3_SECRET_KEY"),
        allow_http: true,
    };
    Some(start(meta, Store::new(S3Backend::new(&s3).unwrap()), vec![], Some(guard)).await)
}

macro_rules! on_every_backend {
    ($($scenario:ident),* $(,)?) => {
        mod on_sqlite_disk {
            $( #[tokio::test(flavor = "multi_thread", worker_threads = 4)] async fn $scenario() { super::$scenario(&super::sqlite_disk().await).await; } )*
        }
        mod on_postgres_disk {
            $( #[tokio::test(flavor = "multi_thread", worker_threads = 4)] async fn $scenario() {
                let Some(server) = super::postgres_disk().await else { eprintln!("skipped: Postgres not configured"); return };
                super::$scenario(&server).await;
            } )*
        }
        mod on_postgres_s3 {
            $( #[tokio::test(flavor = "multi_thread", worker_threads = 4)] async fn $scenario() {
                let Some(server) = super::postgres_s3().await else { eprintln!("skipped: Postgres/S3 not configured"); return };
                super::$scenario(&server).await;
            } )*
        }
    };
}

on_every_backend!(
    info_needs_no_sign_in,
    signing_in_and_managing_devices,
    sign_in_attempts_are_limited,
    upload_commit_and_download,
    spaces_are_isolated,
    bad_uploads_are_refused,
    conflicts_come_back_as_a_list,
    the_change_feed_waits_for_new_commits,
    a_client_ahead_of_the_server_must_resync,
    overlong_usernames_are_refused_before_rate_limiting,
    the_change_feed_pages,
    the_history_of_a_file_pages_newest_first_for_its_owner_only,
    restoring_an_old_version_makes_a_new_one,
    parallel_restores_against_one_head_let_one_win,
    the_trash_lists_restores_and_removes_over_http,
    a_deleted_folder_comes_back_and_goes_with_what_was_deleted_in_it,
    every_rejection_is_a_json_error,
    one_request_cant_list_unbounded_missing_chunks,
    long_polls_are_capped_per_space,
    odd_usernames_and_device_names_are_refused,
    shutting_down_ends_long_polls,
    the_space_endpoint_and_unversioned_info,
    account_usage_is_scoped_to_signed_in_space,
    admin_storage_routes_are_admin_only_and_usage_hides_removed_users,
    oversized_manifests_are_refused,
    live_needs_a_device_token,
    live_hello_checks_the_version,
    live_documents_of_another_account_are_not_found,
    live_updates_are_durable_before_the_ack,
    live_updates_fan_out_to_the_accounts_other_devices,
    live_catch_up_after_a_gap,
    live_a_revoked_device_is_cut_off,
    live_documents_carry_snapshots_and_presence,
    pairings_are_made_polled_and_cancelled,
    a_pairing_code_signs_a_new_computer_in,
    wrong_expired_and_used_codes_get_the_same_answer,
    a_pairing_dies_when_its_maker_signs_out,
    successful_joins_are_not_counted,
    an_invite_to_a_new_account_sets_its_password,
    an_invite_to_an_account_with_a_password_asks_none,
    the_join_page_never_looks_at_the_code,
);

// ---- helpers ----

const PASSWORD: &str = "correct horse battery";

async fn add_user(s: &Server, name: &str) {
    s.meta.create_user(name, &hash_password(PASSWORD).unwrap(), Role::Member, 0).await.unwrap();
}

async fn login(s: &Server, name: &str) -> String {
    let res = s
        .client
        .post(format!("{}/auth/login", s.base))
        .json(&json!({ "username": name, "password": PASSWORD, "device_name": "laptop" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    res.json::<LoginResponse>().await.unwrap().token
}

async fn error(res: reqwest::Response) -> (StatusCode, ErrorBody) {
    let status = res.status();
    (status, res.json::<ErrorBody>().await.unwrap())
}

fn chunk_id(data: &[u8]) -> ChunkId {
    ChunkId::from_bytes(*blake3::hash(data).as_bytes())
}

fn manifest_for(data: &[u8]) -> Manifest {
    let len = u32::try_from(data.len()).unwrap();
    Manifest::new(
        ChunkerParamsId::new("fastcdc-v1").unwrap(),
        u64::from(len),
        FileHash::from_bytes(*blake3::hash(data).as_bytes()),
        vec![ChunkRef { id: chunk_id(data), len }],
    )
    .unwrap()
}

async fn put_chunk(s: &Server, token: &str, data: &[u8]) -> StatusCode {
    s.client.put(format!("{}/chunks/{}", s.base, chunk_id(data))).bearer_auth(token).body(data.to_vec()).send().await.unwrap().status()
}

async fn put_manifest(s: &Server, token: &str, m: &Manifest) -> reqwest::Response {
    s.client.put(format!("{}/manifests/{}", s.base, m.id())).bearer_auth(token).body(m.encode()).send().await.unwrap()
}

/// Uploads `data` as a one-chunk file and returns its manifest ID.
async fn upload(s: &Server, token: &str, data: &[u8]) -> ManifestId {
    assert_eq!(put_chunk(s, token, data).await, StatusCode::NO_CONTENT);
    let m = manifest_for(data);
    assert_eq!(put_manifest(s, token, &m).await.status(), StatusCode::NO_CONTENT);
    m.id()
}

/// A file's content through the API: its manifest, then each chunk it names.
async fn download(s: &Server, token: &str, id: &ManifestId) -> Vec<u8> {
    let get = |path: String| async move {
        let response = s.client.get(format!("{}/{path}", s.base)).bearer_auth(token).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        response.bytes().await.unwrap()
    };
    let manifest = Manifest::decode(&get(format!("manifests/{id}")).await).unwrap();
    let mut out = Vec::new();
    for chunk in manifest.chunks() {
        out.extend_from_slice(&get(format!("chunks/{}", chunk.id)).await);
    }
    out
}

async fn commit(s: &Server, token: &str, ops: &[Op]) -> reqwest::Response {
    s.client.post(format!("{}/commit", s.base)).bearer_auth(token).json(&json!({ "ops": ops })).send().await.unwrap()
}

async fn root(s: &Server, token: &str) -> NodeId {
    let tree: ChangeSet = s.client.get(format!("{}/tree", s.base)).bearer_auth(token).send().await.unwrap().json().await.unwrap();
    tree.nodes.iter().find(|n| n.parent.is_none()).unwrap().id
}

fn create(node: NodeId, parent: NodeId, name: &str, manifest: ManifestId) -> Op {
    Op::Create {
        node,
        parent,
        name: Name::new(name).unwrap(),
        version: NewVersion { id: VersionId::generate(), manifest, mtime_ms: 1, exec: false, save: None },
    }
}

// ---- scenarios ----

async fn info_needs_no_sign_in(s: &Server) {
    let info: InfoResponse = s.client.get(format!("{}/info", s.base)).send().await.unwrap().json().await.unwrap();
    assert_eq!((info.protocol_min, info.protocol_max), (1, 2));
    assert_eq!(info.chunker_params, vec!["fastcdc-v1".to_owned()]);
    assert_eq!(info.max_chunk_bytes, 4 * 1024 * 1024);
}

async fn signing_in_and_managing_devices(s: &Server) {
    add_user(s, "ana").await;
    let wrong = s
        .client
        .post(format!("{}/auth/login", s.base))
        .json(&json!({ "username": "ana", "password": "wrong password", "device_name": "x" }))
        .send()
        .await
        .unwrap();
    let unknown = s
        .client
        .post(format!("{}/auth/login", s.base))
        .json(&json!({ "username": "nobody", "password": PASSWORD, "device_name": "x" }))
        .send()
        .await
        .unwrap();
    let (a, b) = (error(wrong).await, error(unknown).await);
    assert_eq!(a.0, StatusCode::UNAUTHORIZED);
    // A wrong password and an unknown user look the same.
    assert_eq!(a, b);

    // Usernames ignore case at sign-in too.
    let token = {
        let res = s
            .client
            .post(format!("{}/auth/login", s.base))
            .json(&json!({ "username": "ANA", "password": PASSWORD, "device_name": "studio pc" }))
            .send()
            .await
            .unwrap();
        res.json::<LoginResponse>().await.unwrap().token
    };
    let res = s.client.get(format!("{}/devices", s.base)).bearer_auth(&token).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let epoch = res.headers().get(EPOCH_HEADER).unwrap().to_str().unwrap().to_owned();
    assert_eq!(epoch.len(), 36);
    let devices: Vec<DeviceInfo> = res.json().await.unwrap();
    assert_eq!(devices.len(), 1);
    assert!(devices[0].current && devices[0].name == "studio pc" && !devices[0].revoked);

    for bad in [None, Some("not-a-token"), Some("A".repeat(43).as_str())] {
        let mut req = s.client.get(format!("{}/devices", s.base));
        if let Some(t) = bad {
            req = req.bearer_auth(t);
        }
        assert_eq!(req.send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
    }

    let other = login(s, "ana").await;
    let id = devices[0].id;
    let revoke = s.client.delete(format!("{}/devices/{id}", s.base)).bearer_auth(&other).send().await.unwrap();
    assert_eq!(revoke.status(), StatusCode::NO_CONTENT);
    assert_eq!(s.client.get(format!("{}/devices", s.base)).bearer_auth(&token).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
    // Someone else's device can't be revoked.
    add_user(s, "bob").await;
    let bob = login(s, "bob").await;
    let theirs: Vec<DeviceInfo> = s.client.get(format!("{}/devices", s.base)).bearer_auth(&other).send().await.unwrap().json().await.unwrap();
    let target = theirs.iter().find(|d| d.current).unwrap().id;
    assert_eq!(s.client.delete(format!("{}/devices/{target}", s.base)).bearer_auth(&bob).send().await.unwrap().status(), StatusCode::NOT_FOUND);
}

async fn sign_in_attempts_are_limited(s: &Server) {
    add_user(s, "ana").await;
    for _ in 0..10 {
        let res = s
            .client
            .post(format!("{}/auth/login", s.base))
            .json(&json!({ "username": "ana", "password": "wrong password", "device_name": "x" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
    let right = s
        .client
        .post(format!("{}/auth/login", s.base))
        .json(&json!({ "username": "ana", "password": PASSWORD, "device_name": "x" }))
        .send()
        .await
        .unwrap();
    let (status, body) = error(right).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::TOO_MANY_REQUESTS, "rate_limited"));
}

async fn upload_commit_and_download(s: &Server) {
    add_user(s, "ana").await;
    let token = login(s, "ana").await;
    let data = format!("file contents {}", uuid::Uuid::now_v7()).into_bytes();
    let id = chunk_id(&data);

    let missing: MissingResponse = s
        .client
        .post(format!("{}/chunks/missing", s.base))
        .bearer_auth(&token)
        .json(&json!({ "ids": [id] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(missing.missing, vec![id]);
    // A manifest before its chunk: refused with the list.
    let m = manifest_for(&data);
    let (status, body) = error(put_manifest(s, &token, &m).await).await;
    assert_eq!((status, body.error.as_str(), body.missing), (StatusCode::CONFLICT, "missing_chunks", Some(vec![id])));

    assert_eq!(put_chunk(s, &token, &data).await, StatusCode::NO_CONTENT);
    assert_eq!(put_manifest(s, &token, &m).await.status(), StatusCode::NO_CONTENT);
    let r = root(s, &token).await;
    let file = NodeId::generate();
    let res = commit(s, &token, &[create(file, r, "notes.txt", m.id())]).await;
    assert_eq!(res.status(), StatusCode::OK);
    let done: CommitResponse = res.json().await.unwrap();
    assert_eq!(done.seq, 1);

    let feed: ChangeSet = s.client.get(format!("{}/changes?since=0", s.base)).bearer_auth(&token).send().await.unwrap().json().await.unwrap();
    assert_eq!((feed.seq, feed.epoch), (1, done.epoch));
    let node = feed.nodes.iter().find(|n| n.id == file).unwrap();
    let NodeKind::File(state) = &node.kind else { panic!("not a file") };
    assert_eq!((state.manifest, state.size), (m.id(), data.len() as u64));

    let got = s.client.get(format!("{}/manifests/{}", s.base, m.id())).bearer_auth(&token).send().await.unwrap().bytes().await.unwrap();
    assert_eq!(Manifest::decode(&got).unwrap(), m);
    let got = s.client.get(format!("{}/chunks/{id}", s.base)).bearer_auth(&token).send().await.unwrap().bytes().await.unwrap();
    assert_eq!(got.to_vec(), data);

    // Paging through the tree.
    let page: ChangeSet = s.client.get(format!("{}/tree?limit=1", s.base)).bearer_auth(&token).send().await.unwrap().json().await.unwrap();
    let next = page.next.unwrap();
    let rest: ChangeSet = s
        .client
        .get(format!("{}/tree?limit=1&after_seq={}&after_node={}", s.base, next.seq, next.node))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!((page.nodes.len(), rest.nodes.len(), rest.next), (1, 1, None));
}

async fn spaces_are_isolated(s: &Server) {
    add_user(s, "ana").await;
    add_user(s, "bob").await;
    let (ana, bob) = (login(s, "ana").await, login(s, "bob").await);
    let data = b"ana's private file".to_vec();
    let m = upload(s, &ana, &data).await;
    let id = chunk_id(&data);
    // Bob can't download Ana's blobs or learn that they exist.
    assert_eq!(s.client.get(format!("{}/chunks/{id}", s.base)).bearer_auth(&bob).send().await.unwrap().status(), StatusCode::NOT_FOUND);
    assert_eq!(s.client.get(format!("{}/manifests/{m}", s.base)).bearer_auth(&bob).send().await.unwrap().status(), StatusCode::NOT_FOUND);
    let missing: MissingResponse = s
        .client
        .post(format!("{}/chunks/missing", s.base))
        .bearer_auth(&bob)
        .json(&json!({ "ids": [id] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(missing.missing, vec![id]);
    // And can't commit a file that uses Ana's manifest.
    let r = root(s, &bob).await;
    let (status, body) = error(commit(s, &bob, &[create(NodeId::generate(), r, "stolen.txt", m)]).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::CONFLICT, "conflict"));
    assert_eq!(serde_json::to_value(&body.conflicts.unwrap()[0]).unwrap()["kind"], "missing_manifest");
}

async fn bad_uploads_are_refused(s: &Server) {
    add_user(s, "ana").await;
    let token = login(s, "ana").await;
    let wrong =
        s.client.put(format!("{}/chunks/{}", s.base, chunk_id(b"one thing"))).bearer_auth(&token).body(b"another".to_vec()).send().await.unwrap();
    assert_eq!(error(wrong).await.0, StatusCode::BAD_REQUEST);
    let empty = s.client.put(format!("{}/chunks/{}", s.base, chunk_id(b""))).bearer_auth(&token).body(Vec::new()).send().await.unwrap();
    assert_eq!(empty.status(), StatusCode::BAD_REQUEST);
    let bad_id = s.client.put(format!("{}/chunks/XYZ", s.base)).bearer_auth(&token).body(b"x".to_vec()).send().await.unwrap();
    assert_eq!(bad_id.status(), StatusCode::BAD_REQUEST);
    let big = vec![7u8; 4 * 1024 * 1024 + 1];
    let too_big = s.client.put(format!("{}/chunks/{}", s.base, chunk_id(&big))).bearer_auth(&token).body(big).send().await.unwrap();
    let (status, body) = error(too_big).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::PAYLOAD_TOO_LARGE, "too_large"));

    // A manifest under someone else's ID, and one cut with an unknown chunker.
    let data = b"some data".to_vec();
    assert_eq!(put_chunk(s, &token, &data).await, StatusCode::NO_CONTENT);
    let m = manifest_for(&data);
    let mismatch =
        s.client.put(format!("{}/manifests/{}", s.base, manifest_for(b"other").id())).bearer_auth(&token).body(m.encode()).send().await.unwrap();
    assert_eq!(mismatch.status(), StatusCode::BAD_REQUEST);
    let odd = Manifest::new(ChunkerParamsId::new("rabin-v9").unwrap(), m.size(), *m.file_hash(), m.chunks().to_vec()).unwrap();
    assert_eq!(put_manifest(s, &token, &odd).await.status(), StatusCode::BAD_REQUEST);
    let garbage = s.client.put(format!("{}/manifests/{}", s.base, m.id())).bearer_auth(&token).body(b"not a manifest".to_vec()).send().await.unwrap();
    assert_eq!(garbage.status(), StatusCode::BAD_REQUEST);
}

async fn conflicts_come_back_as_a_list(s: &Server) {
    add_user(s, "ana").await;
    let token = login(s, "ana").await;
    let r = root(s, &token).await;
    let m = upload(s, &token, b"x").await;
    assert_eq!(commit(s, &token, &[create(NodeId::generate(), r, "a.txt", m)]).await.status(), StatusCode::OK);
    let ops = [
        create(NodeId::generate(), r, "fine.txt", m),
        create(NodeId::generate(), r, "A.TXT", m),
        create(NodeId::generate(), NodeId::generate(), "b", m),
    ];
    let (status, body) = error(commit(s, &token, &ops).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::CONFLICT, "conflict"));
    let kinds: Vec<(usize, String)> =
        body.conflicts.unwrap().iter().map(|c| (c.op, serde_json::to_value(c).unwrap()["kind"].as_str().unwrap().to_owned())).collect();
    assert_eq!(kinds, vec![(1, "name_taken".to_owned()), (2, "no_such_parent".to_owned())]);
    // A malformed batch is a 4xx, not a crash.
    let bad = s.client.post(format!("{}/commit", s.base)).bearer_auth(&token).json(&json!({ "ops": [{ "op": "explode" }] })).send().await.unwrap();
    assert!(bad.status().is_client_error());
}

async fn the_change_feed_waits_for_new_commits(s: &Server) {
    add_user(s, "ana").await;
    let (a, b) = (login(s, "ana").await, login(s, "ana").await);
    let r = root(s, &a).await;
    // Nothing new: a short wait comes back empty after about a second.
    let t = Instant::now();
    let empty: ChangeSet = s.client.get(format!("{}/changes?since=0&wait=1", s.base)).bearer_auth(&a).send().await.unwrap().json().await.unwrap();
    assert!(empty.nodes.is_empty() && t.elapsed() >= Duration::from_millis(900));

    // Device A waits; device B commits; A wakes up well before its 20 s wait ends.
    let waiting = {
        let (client, url, token) = (s.client.clone(), format!("{}/changes?since=0&wait=20", s.base), a.clone());
        tokio::spawn(async move { client.get(url).bearer_auth(token).send().await.unwrap().json::<ChangeSet>().await.unwrap() })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t = Instant::now();
    let dir = NodeId::generate();
    assert_eq!(commit(s, &b, &[Op::Mkdir { node: dir, parent: r, name: Name::new("from b").unwrap() }]).await.status(), StatusCode::OK);
    let woke = tokio::time::timeout(Duration::from_secs(5), waiting).await.unwrap().unwrap();
    assert!(t.elapsed() < Duration::from_secs(5));
    assert!(woke.nodes.iter().any(|n| n.id == dir));
}

async fn a_client_ahead_of_the_server_must_resync(s: &Server) {
    add_user(s, "ana").await;
    let token = login(s, "ana").await;
    let (status, body) = error(s.client.get(format!("{}/changes?since=99", s.base)).bearer_auth(&token).send().await.unwrap()).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::GONE, "resync_required"));
}

async fn overlong_usernames_are_refused_before_rate_limiting(s: &Server) {
    add_user(s, "ana").await;
    for _ in 0..50 {
        let res = s
            .client
            .post(format!("{}/auth/login", s.base))
            .json(&json!({ "username": "a".repeat(65), "password": PASSWORD, "device_name": "x" }))
            .send()
            .await
            .unwrap();
        let (status, body) = error(res).await;
        assert_eq!((status, body.error.as_str()), (StatusCode::BAD_REQUEST, "invalid"));
    }
    // The refused attempts didn't use up this client's allowance.
    login(s, "ana").await;
}

async fn the_change_feed_pages(s: &Server) {
    add_user(s, "ana").await;
    let token = login(s, "ana").await;
    let r = root(s, &token).await;
    // The root exists from sequence 0, so `changes?since=0` lists only what was made after it.
    let mut made = Vec::new();
    for i in 0..5 {
        let node = NodeId::generate();
        made.push(node);
        let res = commit(s, &token, &[Op::Mkdir { node, parent: r, name: Name::new(&format!("dir {i}")).unwrap() }]).await;
        assert_eq!(res.status(), StatusCode::OK);
    }
    let mut seen = Vec::new();
    let mut url = format!("{}/changes?since=0&limit=2", s.base);
    for _ in 0..10 {
        let res = s.client.get(&url).bearer_auth(&token).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let page: ChangeSet = res.json().await.unwrap();
        assert!(page.nodes.len() <= 2);
        seen.extend(page.nodes.iter().map(|n| n.id));
        match page.next {
            Some(next) => url = format!("{}/changes?since=0&limit=2&after_seq={}&after_node={}", s.base, next.seq, next.node),
            None => break,
        }
    }
    seen.sort_by_key(ToString::to_string);
    made.sort_by_key(ToString::to_string);
    assert_eq!(seen, made);
}

async fn every_rejection_is_a_json_error(s: &Server) {
    add_user(s, "ana").await;
    let token = login(s, "ana").await;
    let bad = s
        .client
        .post(format!("{}/commit", s.base))
        .bearer_auth(&token)
        .header("content-type", "application/json")
        .body("{nope")
        .send()
        .await
        .unwrap();
    let (status, body) = error(bad).await;
    assert!(status.is_client_error() && body.error == "invalid", "{status} {}", body.error);
    let wrong_type = s.client.post(format!("{}/commit", s.base)).bearer_auth(&token).body("x").send().await.unwrap();
    assert_eq!(error(wrong_type).await.1.error, "invalid");
    let bad = s.client.post(format!("{}/commit", s.base)).bearer_auth(&token).json(&json!({ "ops": [{ "op": "explode" }] })).send().await.unwrap();
    let (status, body) = error(bad).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::UNPROCESSABLE_ENTITY, "invalid"));
    let (status, body) = error(s.client.get(format!("{}/changes", s.base)).bearer_auth(&token).send().await.unwrap()).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::BAD_REQUEST, "invalid"));
    let (status, body) = error(s.client.delete(format!("{}/devices/not-an-id", s.base)).bearer_auth(&token).send().await.unwrap()).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::BAD_REQUEST, "invalid"));

    // Unknown routes and wrong methods are JSON too.
    let root_url = s.base.trim_end_matches("/api/v1");
    let (status, body) = error(s.client.get(format!("{}/nope", s.base)).send().await.unwrap()).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::NOT_FOUND, "not_found"));
    assert_eq!(body.message, "no such endpoint");
    let (status, body) = error(s.client.post(format!("{}/info", s.base)).send().await.unwrap()).await;
    assert_eq!((status, body.error.as_str(), body.message.as_str()), (StatusCode::METHOD_NOT_ALLOWED, "invalid", "method not allowed"));
    let (status, body) = error(s.client.get(format!("{root_url}/api/v2/info")).send().await.unwrap()).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::NOT_FOUND, "not_found"));
    let (status, body) = error(s.client.post(format!("{}/tree", s.base)).bearer_auth(&token).send().await.unwrap()).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::METHOD_NOT_ALLOWED, "invalid"));
}

async fn one_request_cant_list_unbounded_missing_chunks(s: &Server) {
    add_user(s, "ana").await;
    let token = login(s, "ana").await;
    let chunks: Vec<ChunkRef> = (0u32..10_001).map(|i| ChunkRef { id: chunk_id(&i.to_le_bytes()), len: 4 }).collect();
    let m = Manifest::new(ChunkerParamsId::new("fastcdc-v1").unwrap(), 4 * 10_001, FileHash::from_bytes([1; 32]), chunks).unwrap();
    let (status, body) = error(put_manifest(s, &token, &m).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::CONFLICT, "missing_chunks"));
    assert_eq!(body.missing.unwrap().len(), 10_000);
    assert!(body.message.contains("10001"), "{}", body.message);
}

async fn long_polls_are_capped_per_space(s: &Server) {
    add_user(s, "ana").await;
    let token = login(s, "ana").await;
    let r = root(s, &token).await;
    let waiting: Vec<_> = (0..16)
        .map(|_| {
            let (client, url, token) = (s.client.clone(), format!("{}/changes?since=0&wait=10", s.base), token.clone());
            tokio::spawn(async move { client.get(url).bearer_auth(token).send().await.unwrap().json::<ChangeSet>().await.unwrap() })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(700)).await;
    let t = Instant::now();
    let seventeenth = s.client.get(format!("{}/changes?since=0&wait=10", s.base)).bearer_auth(&token).send().await.unwrap();
    assert!(t.elapsed() < Duration::from_secs(2), "the 17th wait should not have waited");
    assert_eq!(seventeenth.headers().get("retry-after").unwrap(), "5");
    let (status, body) = error(seventeenth).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::TOO_MANY_REQUESTS, "rate_limited"));
    assert_eq!(body.message, "too many waiting requests for this space");
    // Wake the sixteen.
    assert_eq!(
        commit(s, &token, &[Op::Mkdir { node: NodeId::generate(), parent: r, name: Name::new("wake").unwrap() }]).await.status(),
        StatusCode::OK
    );
    for w in waiting {
        let woke = tokio::time::timeout(Duration::from_secs(5), w).await.unwrap().unwrap();
        assert_eq!(woke.nodes.len(), 1);
    }
}

async fn odd_usernames_and_device_names_are_refused(s: &Server) {
    add_user(s, "ana").await;
    for (user, device) in [("a na", "x"), ("ana/../x", "x"), ("", "x"), ("ana", ""), ("ana", "bad\u{7}name"), ("ana", "n".repeat(129).as_str())] {
        let res = s
            .client
            .post(format!("{}/auth/login", s.base))
            .json(&json!({ "username": user, "password": PASSWORD, "device_name": device }))
            .send()
            .await
            .unwrap();
        let (status, body) = error(res).await;
        assert_eq!((status, body.error.as_str()), (StatusCode::BAD_REQUEST, "invalid"), "{user:?} {device:?}");
    }
    login(s, "ana").await;
}

async fn shutting_down_ends_long_polls(s: &Server) {
    add_user(s, "ana").await;
    let token = login(s, "ana").await;
    let (client, url) = (s.client.clone(), format!("{}/changes?since=0&wait=30", s.base));
    let poll = tokio::spawn(async move { client.get(url).bearer_auth(token).send().await.unwrap().json::<ChangeSet>().await.unwrap() });
    tokio::time::sleep(Duration::from_millis(700)).await;
    let t = Instant::now();
    s.stop();
    let answer = tokio::time::timeout(Duration::from_secs(3), poll).await.expect("the long-poll should answer at once").unwrap();
    assert!(answer.nodes.is_empty());
    assert!(t.elapsed() < Duration::from_secs(3));
}

async fn the_space_endpoint_and_unversioned_info(s: &Server) {
    add_user(s, "Ana").await;
    let token = login(s, "ana").await;
    let res = s.client.get(format!("{}/space", s.base)).bearer_auth(&token).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let space: SpaceResponse = res.json().await.unwrap();
    assert_eq!(space.username, "ana");
    assert_eq!(space.root, root(s, &token).await);
    let tree: ChangeSet = s.client.get(format!("{}/tree", s.base)).bearer_auth(&token).send().await.unwrap().json().await.unwrap();
    assert_eq!((space.epoch, space.seq), (tree.epoch, tree.seq));
    assert_eq!(s.client.get(format!("{}/space", s.base)).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);

    let versioned: InfoResponse = s.client.get(format!("{}/info", s.base)).send().await.unwrap().json().await.unwrap();
    let unversioned: InfoResponse =
        s.client.get(format!("{}/api/info", s.base.trim_end_matches("/api/v1"))).send().await.unwrap().json().await.unwrap();
    assert_eq!(versioned, unversioned);
}

async fn account_usage_is_scoped_to_signed_in_space(s: &Server) {
    add_user(s, "ana").await;
    add_user(s, "bob").await;
    let ana = login(s, "ana").await;
    let bob = login(s, "bob").await;
    s.meta.add_chunk(s.meta.user_by_name("ana").await.unwrap().unwrap().space, &chunk_id(b"ana"), 11, 1).await.unwrap();
    s.meta.add_chunk(s.meta.user_by_name("bob").await.unwrap().unwrap().space, &chunk_id(b"bob"), 29, 1).await.unwrap();

    let response = s.client.get(format!("{}/account/usage", s.base)).bearer_auth(&ana).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json: serde_json::Value = response.json().await.unwrap();
    assert_eq!(json["stored_bytes"], 11);
    assert_eq!(json["quota_bytes"], serde_json::Value::Null);
    assert_eq!(json["trash_bytes"], 0);
    assert_eq!(json["history_bytes"], 0);
    assert_eq!(json["estimates"], json!({ "index_complete": true }));

    let bob_usage: serde_json::Value =
        s.client.get(format!("{}/account/usage", s.base)).bearer_auth(&bob).send().await.unwrap().json().await.unwrap();
    assert_eq!(bob_usage["stored_bytes"], 29);
    assert_eq!(s.client.get(format!("{}/account/usage", s.base)).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
}

async fn admin_storage_routes_are_admin_only_and_usage_hides_removed_users(s: &Server) {
    let admin = s.meta.create_user("admin", &hash_password(PASSWORD).unwrap(), Role::Admin, 1).await.unwrap();
    add_user(s, "ana").await;
    let admin_token = login(s, "admin").await;
    let member_token = login(s, "ana").await;
    let target = s.meta.user_by_name("ana").await.unwrap().unwrap();
    let quota = u64::from(craft_cloud_proto::limits::MAX_CHUNK_BYTES);
    let response = s
        .client
        .patch(format!("{}/admin/users/{}", s.base, target.id.0))
        .bearer_auth(&admin_token)
        .json(&json!({ "quota_bytes": quota, "password": PASSWORD }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = s.client.get(format!("{}/admin/users/{}/usage", s.base, target.id.0)).bearer_auth(&admin_token).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let usage: serde_json::Value = response.json().await.unwrap();
    assert_eq!(usage["quota_bytes"], quota);
    assert_eq!(usage["stored_bytes"], 0);
    assert_eq!(usage["estimates"]["index_complete"], true);

    let response = s.client.get(format!("{}/admin/storage", s.base)).bearer_auth(&admin_token).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let overview: serde_json::Value = response.json().await.unwrap();
    assert_eq!(overview["accounts"], 2);
    assert_eq!(overview["stored_bytes"], 0);
    assert_eq!(overview["accounts_with_override"], 1);
    assert_eq!(overview["top_accounts"][0]["username"], "admin");
    assert_eq!(overview["top_accounts"][1]["quota_bytes"], quota);

    assert_eq!(s.client.get(format!("{}/admin/storage", s.base)).bearer_auth(&member_token).send().await.unwrap().status(), StatusCode::FORBIDDEN);
    assert_eq!(s.client.get(format!("{}/admin/storage", s.base)).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        s.client.get(format!("{}/admin/users/{}/usage", s.base, uuid::Uuid::now_v7())).bearer_auth(&admin_token).send().await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
    let removed = s
        .client
        .delete(format!("{}/admin/users/{}", s.base, target.id.0))
        .bearer_auth(&admin_token)
        .json(&json!({ "password": PASSWORD }))
        .send()
        .await
        .unwrap();
    assert_eq!(removed.status(), StatusCode::ACCEPTED);
    assert_eq!(
        s.client.get(format!("{}/admin/users/{}/usage", s.base, target.id.0)).bearer_auth(&admin_token).send().await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(admin.role, Role::Admin);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn account_usage_is_read_only_and_returns_configured_quota() {
    let db = tempfile::tempdir().unwrap();
    let blobs = tempfile::tempdir().unwrap();
    let db_path = db.path().join("meta.db");
    let meta = Meta::open(&format!("sqlite:{}", db_path.display())).await.unwrap().with_stored_bytes_cap(Some(123));
    let server = start(meta, Store::new(DiskBackend::new(blobs.path()).unwrap()), vec![db, blobs], None).await;
    add_user(&server, "ana").await;
    server.meta.create_user("admin", &hash_password(PASSWORD).unwrap(), Role::Admin, 1).await.unwrap();
    let token = login(&server, "ana").await;
    let admin_token = login(&server, "admin").await;
    let space = server.meta.user_by_name("ana").await.unwrap().unwrap().space;
    let target = server.meta.user_by_name("ana").await.unwrap().unwrap();
    server.meta.add_chunk(space, &chunk_id(b"usage"), 11, 1).await.unwrap();

    let before = sqlite_fingerprint(&db_path);
    let response = server.client.get(format!("{}/account/usage", server.base)).bearer_auth(&token).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let usage: serde_json::Value = response.json().await.unwrap();
    assert_eq!(
        usage,
        json!({
            "stored_bytes": 11,
            "quota_bytes": 123,
            "trash_bytes": 0,
            "history_bytes": 0,
            "estimates": { "index_complete": true }
        })
    );
    assert_eq!(sqlite_fingerprint(&db_path), before, "authorized usage request changed database or WAL bytes");

    let response = server.client.get(format!("{}/admin/users/{}/usage", server.base, target.id.0)).bearer_auth(&admin_token).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let usage: serde_json::Value = response.json().await.unwrap();
    assert_eq!(usage["quota_bytes"], 123);
    assert_eq!(sqlite_fingerprint(&db_path), before, "admin usage request changed database or WAL bytes");

    let response = server.client.get(format!("{}/admin/storage", server.base)).bearer_auth(&admin_token).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let overview: serde_json::Value = response.json().await.unwrap();
    assert_eq!(overview["accounts"], 2);
    assert_eq!(overview["stored_bytes"], 11);
    assert_eq!(overview["default_quota_bytes"], 123);
    assert_eq!(sqlite_fingerprint(&db_path), before, "admin storage request changed database or WAL bytes");

    let response = server.client.get(format!("{}/account/usage", server.base)).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(sqlite_fingerprint(&db_path), before, "unauthorized usage request changed database or WAL bytes");
    server.stop();
}

fn sqlite_fingerprint(path: &std::path::Path) -> [u8; 32] {
    let read = |path: std::path::PathBuf| if path.exists() { std::fs::read(path).unwrap() } else { Vec::new() };
    let (database, wal) = (read(path.to_owned()), read(path.with_extension("db-wal")));
    let mut hash = blake3::Hasher::new();
    hash.update(&database);
    hash.update(&wal);
    *hash.finalize().as_bytes()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn account_usage_is_read_only_and_fingerprints_every_postgres_table() {
    let Ok(admin_url) = std::env::var("CRAFT_CLOUD_TEST_PG") else { return };
    use sqlx::Connection;

    let name = format!("cc_usage_{}", uuid::Uuid::now_v7().simple());
    let mut conn = sqlx::PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}"))).execute(&mut conn).await.unwrap();
    let guard = PgGuard { admin_url: admin_url.clone(), name: name.clone() };
    let (base, _) = admin_url.rsplit_once('/').unwrap();
    let database_url = format!("{base}/{name}");
    let meta = Meta::open(&database_url).await.unwrap().with_stored_bytes_cap(Some(123));
    let blobs = tempfile::tempdir().unwrap();
    let server = start(meta, Store::new(DiskBackend::new(blobs.path()).unwrap()), vec![blobs], Some(guard)).await;

    add_user(&server, "ana").await;
    server.meta.create_user("admin", &hash_password(PASSWORD).unwrap(), Role::Admin, 1).await.unwrap();
    let token = login(&server, "ana").await;
    let admin_token = login(&server, "admin").await;
    let space = server.meta.user_by_name("ana").await.unwrap().unwrap().space;
    let target = server.meta.user_by_name("ana").await.unwrap().unwrap();
    server.meta.add_chunk(space, &chunk_id(b"usage"), 11, 1).await.unwrap();
    let before = postgres_fingerprint(&database_url).await;
    let response = server.client.get(format!("{}/account/usage", server.base)).bearer_auth(&token).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let usage: serde_json::Value = response.json().await.unwrap();
    assert_eq!(usage["stored_bytes"], 11);
    assert_eq!(usage["quota_bytes"], 123);
    assert_eq!(postgres_fingerprint(&database_url).await, before, "usage request changed at least one Postgres table");

    let response = server.client.get(format!("{}/admin/users/{}/usage", server.base, target.id.0)).bearer_auth(&admin_token).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let usage: serde_json::Value = response.json().await.unwrap();
    assert_eq!(usage["quota_bytes"], 123);
    assert_eq!(postgres_fingerprint(&database_url).await, before, "admin usage request changed at least one Postgres table");
    let response = server.client.get(format!("{}/admin/storage", server.base)).bearer_auth(&admin_token).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(postgres_fingerprint(&database_url).await, before, "admin storage request changed at least one Postgres table");
    server.stop();
}

async fn postgres_fingerprint(database_url: &str) -> Vec<(String, String)> {
    use sqlx::Connection;

    let mut conn = sqlx::PgConnection::connect(database_url).await.unwrap();
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public' AND table_type = 'BASE TABLE' ORDER BY table_name",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();
    let mut fingerprint = Vec::with_capacity(tables.len());
    for table in tables {
        let quoted = table.replace('"', "\"\"");
        let sql = format!("SELECT COALESCE(json_agg(to_jsonb(t) ORDER BY t.ctid)::text, '[]') FROM \"{quoted}\" AS t");
        let rows: String = sqlx::query_scalar(sqlx::AssertSqlSafe(sql)).fetch_one(&mut conn).await.unwrap();
        fingerprint.push((table, blake3::hash(rows.as_bytes()).to_hex().to_string()));
    }
    fingerprint
}

async fn oversized_manifests_are_refused(s: &Server) {
    add_user(s, "ana").await;
    let token = login(s, "ana").await;
    let data = b"some data".to_vec();
    let id = manifest_for(&data).id();
    let big = vec![0u8; craft_cloud_proto::limits::MAX_MANIFEST_BYTES + 1];
    let res = s.client.put(format!("{}/manifests/{id}", s.base)).bearer_auth(&token).body(big).send().await.unwrap();
    let (status, body) = error(res).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::PAYLOAD_TOO_LARGE, "too_large"));
    // Reading manifests still works.
    let stored = upload(s, &token, &data).await;
    let got = s.client.get(format!("{}/manifests/{stored}", s.base)).bearer_auth(&token).send().await.unwrap();
    assert_eq!(got.status(), StatusCode::OK);
}

// ---- join codes ----

async fn new_pairing(s: &Server, token: &str) -> PairingCreated {
    let res = s.client.post(format!("{}/pairings", s.base)).bearer_auth(token).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    res.json().await.unwrap()
}

async fn pairing_status(s: &Server, token: &str, id: uuid::Uuid) -> reqwest::Response {
    s.client.get(format!("{}/pairings/{id}", s.base)).bearer_auth(token).send().await.unwrap()
}

async fn join(s: &Server, code: &str, device_name: &str, password: Option<&str>) -> reqwest::Response {
    join_from(s, code, device_name, password, None).await
}

/// A join from `from` (an `X-Forwarded-For` address, believed when the server trusts this machine as a proxy).
async fn join_from(s: &Server, code: &str, device_name: &str, password: Option<&str>, from: Option<&str>) -> reqwest::Response {
    let mut body = json!({ "code": code, "device_name": device_name });
    if let Some(password) = password {
        body["password"] = json!(password);
    }
    let mut req = s.client.post(format!("{}/join", s.base)).json(&body);
    if let Some(from) = from {
        req = req.header("x-forwarded-for", from);
    }
    req.send().await.unwrap()
}

async fn joined(res: reqwest::Response) -> LoginResponse {
    assert_eq!(res.status(), StatusCode::OK);
    res.json().await.unwrap()
}

/// The device a token belongs to.
async fn current_device(s: &Server, token: &str) -> craft_cloud_proto::DeviceId {
    let devices: Vec<DeviceInfo> = s.client.get(format!("{}/devices", s.base)).bearer_auth(token).send().await.unwrap().json().await.unwrap();
    devices.iter().find(|d| d.current).unwrap().id
}

async fn pairings_are_made_polled_and_cancelled(s: &Server) {
    add_user(s, "ana").await;
    let token = login(s, "ana").await;
    let before = craft_cloud_server::state::now_ms();
    let made = new_pairing(s, &token).await;
    assert!(Code::parse(&made.code, PAIRING_CHARS).is_some_and(|c| c.as_str() == made.code), "normalised: {}", made.code);
    assert!((before + PAIRING_TTL_MS..=craft_cloud_server::state::now_ms() + PAIRING_TTL_MS).contains(&made.expires_ms));
    let status: PairingStatus = pairing_status(s, &token, made.id).await.json().await.unwrap();
    assert_eq!(status, PairingStatus { state: PairingState::Waiting, device_name: None });
    assert_eq!(s.client.post(format!("{}/pairings", s.base)).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);

    add_user(s, "bob").await;
    let bob = login(s, "bob").await;
    assert_eq!(error(pairing_status(s, &bob, made.id).await).await.0, StatusCode::NOT_FOUND, "another account's pairing");
    assert_eq!(error(pairing_status(s, &token, uuid::Uuid::now_v7()).await).await.0, StatusCode::NOT_FOUND);
    assert_eq!(error(s.client.get(format!("{}/pairings/nope", s.base)).bearer_auth(&token).send().await.unwrap()).await.0, StatusCode::BAD_REQUEST);

    let cancel = s.client.delete(format!("{}/pairings/{}", s.base, made.id)).bearer_auth(&token).send().await.unwrap();
    assert_eq!(cancel.status(), StatusCode::NO_CONTENT);
    assert_eq!(pairing_status(s, &token, made.id).await.status(), StatusCode::NOT_FOUND);
    let (status, body) = error(join(s, &made.code, "lptp1", None).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::NOT_FOUND, "invalid_code"));
}

async fn a_pairing_code_signs_a_new_computer_in(s: &Server) {
    add_user(s, "ana").await;
    let token = login(s, "ana").await;
    let made = new_pairing(s, &token).await;
    // As typed: lower case, grouped, with an O for a 0.
    let typed = Code::parse(&made.code, PAIRING_CHARS).unwrap().display().to_lowercase().replace('0', "o");
    let new = joined(join(s, &typed, "lptp1", None).await).await;
    let space: SpaceResponse = s.client.get(format!("{}/space", s.base)).bearer_auth(&new.token).send().await.unwrap().json().await.unwrap();
    assert_eq!(space.username, "ana");
    assert_eq!(current_device(s, &new.token).await, new.device);
    let status: PairingStatus = pairing_status(s, &token, made.id).await.json().await.unwrap();
    assert_eq!(status, PairingStatus { state: PairingState::Used, device_name: Some("lptp1".into()) });
}

async fn wrong_expired_and_used_codes_get_the_same_answer(s: &Server) {
    add_user(s, "ana").await;
    let token = login(s, "ana").await;
    let used = new_pairing(s, &token).await;
    joined(join(s, &used.code, "lptp1", None).await).await;
    // An expired code: one made eleven minutes ago, by the time the server reads.
    let other = login(s, "ana").await;
    let ana = s.meta.user_by_name("ana").await.unwrap().unwrap();
    let expired = Code::generate(PAIRING_CHARS).unwrap();
    let made_ms = craft_cloud_server::state::now_ms() - PAIRING_TTL_MS - 60_000;
    s.meta.create_pairing(ana.id, current_device(s, &other).await, &expired.hash(), made_ms).await.unwrap();
    let wrong = Code::generate(PAIRING_CHARS).unwrap();
    let mut answers = Vec::new();
    for code in [wrong.as_str(), expired.as_str(), used.code.as_str(), "K7M4-Q9T", "not a code at all", ""] {
        let res = join(s, code, "beast", None).await;
        assert_eq!(res.headers().get("content-type").unwrap(), "application/json");
        answers.push((res.status(), res.text().await.unwrap()));
    }
    assert_eq!(answers[0].0, StatusCode::NOT_FOUND);
    assert!(answers[0].1.contains("\"invalid_code\"") && answers[0].1.contains("Make a new one on a signed-in computer"), "{}", answers[0].1);
    assert!(answers.iter().all(|a| *a == answers[0]), "{answers:?}");
    assert_eq!(s.meta.list_devices(ana.id).await.unwrap().len(), 3);
}

async fn a_pairing_dies_when_its_maker_signs_out(s: &Server) {
    add_user(s, "ana").await;
    let token = login(s, "ana").await;
    let made = new_pairing(s, &token).await;
    let id = current_device(s, &token).await;
    // Signing out revokes the device.
    assert_eq!(s.client.delete(format!("{}/devices/{id}", s.base)).bearer_auth(&token).send().await.unwrap().status(), StatusCode::NO_CONTENT);
    let (status, body) = error(join(s, &made.code, "lptp1", None).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::NOT_FOUND, "invalid_code"));
}

async fn successful_joins_are_not_counted(s: &Server) {
    add_user(s, "ana").await;
    let token = login(s, "ana").await;
    for i in 0..12 {
        let made = new_pairing(s, &token).await;
        joined(join(s, &made.code, &format!("pc{i}"), None).await).await;
    }
}

async fn an_invite_to_a_new_account_sets_its_password(s: &Server) {
    let invite = Code::generate(INVITE_CHARS).unwrap();
    s.meta.create_invite("cleo", &invite.hash(), craft_cloud_server::state::now_ms(), INVITE_TTL_MS).await.unwrap();
    // No password can sign the new account in yet.
    let res = s
        .client
        .post(format!("{}/auth/login", s.base))
        .json(&json!({ "username": "cleo", "password": "", "device_name": "x" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let (status, body) = error(join(s, &invite.display(), "pc", None).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::BAD_REQUEST, "password_required"));
    let (status, body) = error(join(s, &invite.display(), "pc", Some("short")).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::BAD_REQUEST, "weak_password"));
    assert!(body.message.contains('8'), "{}", body.message);
    let cleo = s.meta.user_by_name("cleo").await.unwrap().unwrap();
    assert!(s.meta.list_devices(cleo.id).await.unwrap().is_empty(), "nothing created yet");

    let new = joined(join(s, &invite.display(), "pc", Some(PASSWORD)).await).await;
    assert_eq!(current_device(s, &new.token).await, new.device);
    login(s, "cleo").await;
    let (status, body) = error(join(s, &invite.display(), "pc", Some(PASSWORD)).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::NOT_FOUND, "invalid_code"), "used");
}

async fn an_invite_to_an_account_with_a_password_asks_none(s: &Server) {
    add_user(s, "ana").await;
    let invite = Code::generate(INVITE_CHARS).unwrap();
    s.meta.create_invite("ana", &invite.hash(), craft_cloud_server::state::now_ms(), INVITE_TTL_MS).await.unwrap();
    let new = joined(join(s, invite.as_str(), "lost laptop", None).await).await;
    assert_eq!(current_device(s, &new.token).await, new.device);
    login(s, "ana").await;
}

async fn the_join_page_never_looks_at_the_code(s: &Server) {
    add_user(s, "ana").await;
    let token = login(s, "ana").await;
    let made = new_pairing(s, &token).await;
    let root = s.base.trim_end_matches("/api/v1");
    let mut pages = Vec::new();
    for code in [made.code.as_str(), "K7M4-Q9TX", "garbage%20here"] {
        let res = s.client.get(format!("{root}/join/{code}")).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.headers().get("content-type").unwrap().to_str().unwrap().starts_with("text/plain"));
        pages.push(res.text().await.unwrap());
    }
    assert!(pages[0].contains("Open Craft Cloud on the computer you are adding and enter this code there."), "{}", pages[0]);
    assert!(pages.iter().all(|p| *p == pages[0]));
    joined(join(s, &made.code, "lptp1", None).await).await;
}

/// Failed joins count per client address (behind a trusted proxy, its `X-Forwarded-For`): ten in a minute, then even
/// the right code is refused from there, while another address still joins.
// These cases build custom trusted-proxy state directly; the shared backend matrix covers the default server setup.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_joins_are_limited_per_address() {
    let db = tempfile::tempdir().unwrap();
    let blobs = tempfile::tempdir().unwrap();
    let meta = Meta::open(&format!("sqlite:{}", db.path().join("meta.db").display())).await.unwrap();
    let store = Store::new(DiskBackend::new(blobs.path()).unwrap());
    let s = start_trusting(meta, store, vec![db, blobs], vec!["127.0.0.1".parse().unwrap()]).await;
    add_user(&s, "ana").await;
    let token = login(&s, "ana").await;
    let made = new_pairing(&s, &token).await;
    let wrong = Code::generate(PAIRING_CHARS).unwrap();
    for _ in 0..10 {
        let (status, body) = error(join_from(&s, wrong.as_str(), "pc", None, Some("203.0.113.9")).await).await;
        assert_eq!((status, body.error.as_str()), (StatusCode::NOT_FOUND, "invalid_code"));
    }
    let (status, body) = error(join_from(&s, &made.code, "pc", None, Some("203.0.113.9")).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::TOO_MANY_REQUESTS, "rate_limited"));
    joined(join_from(&s, &made.code, "pc", None, Some("198.51.100.7")).await).await;
}

/// Wrong codes sent all at once from one address get no more tries than sent one by one: of 40 in flight together,
/// at most 10 reach the database, and the rest are `rate_limited`.
// This concurrent trusted-proxy case uses the same dedicated SQLite fixture as the sequential variant above.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_failed_joins_are_limited_too() {
    let db = tempfile::tempdir().unwrap();
    let blobs = tempfile::tempdir().unwrap();
    let meta = Meta::open(&format!("sqlite:{}", db.path().join("meta.db").display())).await.unwrap();
    let store = Store::new(DiskBackend::new(blobs.path()).unwrap());
    let s = std::sync::Arc::new(start_trusting(meta, store, vec![db, blobs], vec!["127.0.0.1".parse().unwrap()]).await);
    let tries: Vec<_> = (0..40)
        .map(|_| {
            let s = s.clone();
            tokio::spawn(async move {
                let wrong = Code::generate(PAIRING_CHARS).unwrap();
                error(join_from(&s, wrong.as_str(), "pc", None, Some("203.0.113.9")).await).await.1.error
            })
        })
        .collect();
    let mut answers = Vec::new();
    for t in tries {
        answers.push(t.await.unwrap());
    }
    let tried = answers.iter().filter(|a| *a == "invalid_code").count();
    let limited = answers.iter().filter(|a| *a == "rate_limited").count();
    assert_eq!(tried + limited, 40, "{answers:?}");
    assert!(tried <= 10, "{tried} wrong codes reached the database");
    // A success gives its try back: the address isn't held off by its own joins.
    add_user(&s, "ana").await;
    let token = login(&s, "ana").await;
    for i in 0..12 {
        let made = new_pairing(&s, &token).await;
        joined(join_from(&s, &made.code, &format!("pc{i}"), None, Some("198.51.100.7")).await).await;
    }
}

// ---- load limits: standalone SQLite + disk fixtures for custom limits and SQLite-specific behavior ----

/// Starts a `PUT` to `path` (under `/api/v1`) whose body never finishes: the headers promise 1,000 bytes and three
/// arrive. The connection stays open until the server answers or closes it.
async fn stalled_put(s: &Server, token: &str, path: &str) -> tokio::net::TcpStream {
    use tokio::io::AsyncWriteExt;
    let addr = s.base.trim_start_matches("http://").trim_end_matches("/api/v1").to_owned();
    let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
    let head = format!("PUT /api/v1{path} HTTP/1.1\r\nhost: {addr}\r\nauthorization: Bearer {token}\r\ncontent-length: 1000\r\n\r\nabc");
    stream.write_all(head.as_bytes()).await.unwrap();
    stream
}

/// The status line of the answer on a raw connection.
async fn status_line(stream: &mut tokio::net::TcpStream) -> String {
    use tokio::io::AsyncReadExt;
    let mut answer = Vec::new();
    let mut buf = [0u8; 1024];
    while !answer.windows(2).any(|w| w == b"\r\n") {
        let n = stream.read(&mut buf).await.unwrap();
        assert!(n > 0, "the server closed the connection without an answer");
        answer.extend_from_slice(&buf[..n]);
    }
    let text = String::from_utf8_lossy(&answer);
    text.lines().next().unwrap().to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_json_body_that_never_finishes_is_cut_off_by_the_server() {
    // Uses a custom one-second body limit; the shared backend matrix covers the default limit.
    use tokio::io::AsyncWriteExt;
    let limits = Limits { body_time_base: Duration::from_secs(1), body_time_per_mib: Duration::ZERO, ..Limits::default() };
    let s = sqlite_disk_with(limits).await;
    let addr = s.base.trim_start_matches("http://").trim_end_matches("/api/v1").to_owned();
    let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
    let head =
        format!("POST /api/v1/auth/login HTTP/1.1\r\nhost: {addr}\r\ncontent-type: application/json\r\ncontent-length: 1000\r\n\r\n{{\"username\"");
    stream.write_all(head.as_bytes()).await.unwrap();
    // The proxy in front may have no body timeout: the server answers by itself once the body's time is up.
    let line = tokio::time::timeout(Duration::from_secs(20), status_line(&mut stream)).await.expect("no answer to a stalled sign-in body");
    assert!(line.starts_with("HTTP/1.1 408"), "{line}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_upload_holds_up_only_its_own_user() {
    // Bob's requests get half the time Ana's bodies have: enough for two SQLite writes on a slow CI disk, and still
    // over before Ana's slots would free up, so Bob going through shows he never waited for them.
    let limits = Limits { body_time_base: Duration::from_secs(10), body_time_per_mib: Duration::ZERO, ..Limits::default() };
    let s = sqlite_disk_with(limits).await;
    add_user(&s, "ana").await;
    add_user(&s, "bob").await;
    let ana = login(&s, "ana").await;
    let bob = login(&s, "bob").await;
    let earlier = upload(&s, &bob, b"bob's earlier file").await;

    // Ana's device sends manifest and chunk bodies that never finish: more manifests than the server once allowed in
    // flight for everyone together.
    let mut stalled = Vec::new();
    for _ in 0..5 {
        stalled.push(stalled_put(&s, &ana, &format!("/manifests/{}", manifest_for(b"ana's file").id())).await);
    }
    stalled.push(stalled_put(&s, &ana, &format!("/chunks/{}", chunk_id(b"ana's file"))).await);
    // Let the server take them in before Bob starts.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Bob uploads and downloads as if nothing happened.
    let quick = Client::builder().timeout(Duration::from_secs(5)).build().unwrap();
    let data = b"bob's new file";
    let put = quick.put(format!("{}/chunks/{}", s.base, chunk_id(data))).bearer_auth(&bob).body(data.to_vec()).send().await.unwrap();
    assert_eq!(put.status(), StatusCode::NO_CONTENT);
    let m = manifest_for(data);
    let put = quick.put(format!("{}/manifests/{}", s.base, m.id())).bearer_auth(&bob).body(m.encode()).send().await.unwrap();
    assert_eq!(put.status(), StatusCode::NO_CONTENT);
    let got = quick.get(format!("{}/manifests/{earlier}", s.base)).bearer_auth(&bob).send().await.unwrap();
    assert_eq!(got.status(), StatusCode::OK);

    // Ana's stalled bodies run out of time, and her device can upload again.
    for mut stream in stalled {
        let line = tokio::time::timeout(Duration::from_secs(30), status_line(&mut stream)).await.unwrap();
        assert!(line.starts_with("HTTP/1.1 408"), "{line}");
    }
    upload(&s, &ana, b"ana's file").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_never_waits_to_record_its_devices_use() {
    // A held `BEGIN IMMEDIATE` lock is SQLite-specific; this case cannot run on Postgres.
    use sqlx::Connection;
    let db = tempfile::tempdir().unwrap();
    let blobs = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}", db.path().join("meta.db").display());
    let meta = Meta::open(&url).await.unwrap();
    let s = start(meta, Store::new(DiskBackend::new(blobs.path()).unwrap()), vec![db, blobs], None).await;
    add_user(&s, "ana").await;
    let ana = login(&s, "ana").await;
    // Another program holds the database's write lock (a backup, say): writes wait for it, reads don't.
    let mut other = sqlx::SqliteConnection::connect(&url).await.unwrap();
    sqlx::query("BEGIN IMMEDIATE").execute(&mut other).await.unwrap();
    // The device's first use is due to be written, yet the answer comes at once.
    let quick = Client::builder().timeout(Duration::from_secs(2)).build().unwrap();
    let started = Instant::now();
    let res = quick.get(format!("{}/space", s.base)).bearer_auth(&ana).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    sqlx::query("ROLLBACK").execute(&mut other).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uploads_over_the_stored_bytes_cap_are_refused_with_the_numbers() {
    // This case counts files in the disk blob store after rejecting an upload.
    let db = tempfile::tempdir().unwrap();
    let blobs = tempfile::tempdir().unwrap();
    let meta = Meta::open(&format!("sqlite:{}", db.path().join("meta.db").display())).await.unwrap().with_stored_bytes_cap(Some(50));
    let store = Store::new(DiskBackend::new(blobs.path()).unwrap());
    let s = start(meta, store, vec![db], None).await;
    add_user(&s, "ana").await;
    let ana = login(&s, "ana").await;
    assert_eq!(put_chunk(&s, &ana, &[1u8; 30]).await, StatusCode::NO_CONTENT);
    let res = s.client.put(format!("{}/chunks/{}", s.base, chunk_id(&[2u8; 30]))).bearer_auth(&ana).body(vec![2u8; 30]).send().await.unwrap();
    let (status, body) = error(res).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::INSUFFICIENT_STORAGE, "quota_exceeded"));
    assert_eq!(body.quota, Some(craft_cloud_proto::api::QuotaUse { used: 31, limit: 50 }));
    // The refused bytes were never stored: only the first chunk is on disk.
    let stored = walk(blobs.path());
    assert_eq!(stored, 1, "{stored} blobs on disk");
    // A chunk the space has already costs nothing again.
    assert_eq!(put_chunk(&s, &ana, &[1u8; 30]).await, StatusCode::NO_CONTENT);
}

/// How many files are under `dir`.
fn walk(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).map(|p| if p.is_dir() { walk(&p) } else { 1 }).sum()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sign_ins_beyond_the_password_check_slots_are_told_to_come_back() {
    // Uses a one-slot semaphore to exercise a custom per-server limit.
    let s = sqlite_disk_with(Limits { password_checks: 1, ..Limits::default() }).await;
    add_user(&s, "ana").await;
    // One slot: a sign-in that follows a finished one finds it free again (the permit is given back).
    login(&s, "ana").await;
    login(&s, "ana").await;
    // Every slot busy (a flood): each sign-in is told to come back in a second, more times than the account's
    // window of attempts allows…
    let busy = s.password_checks.clone().try_acquire_owned().unwrap();
    for _ in 0..12 {
        let res = s
            .client
            .post(format!("{}/auth/login", s.base))
            .json(&json!({ "username": "ana", "password": PASSWORD, "device_name": "x" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.headers().get("retry-after").and_then(|v| v.to_str().ok()), Some("1"));
        let (status, body) = error(res).await;
        assert_eq!((status, body.error.as_str()), (StatusCode::TOO_MANY_REQUESTS, "rate_limited"));
    }
    // …and none of those answers cost an attempt: once a slot is free, the device signs in.
    drop(busy);
    login(&s, "ana").await;
}

// ---- the live service ----

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Opens `/api/v1/live` with `token` (if any): the socket, or the HTTP status it was refused with.
async fn open_live(s: &Server, token: Option<&str>) -> Result<Ws, StatusCode> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut request = format!("{}/live", s.base.replacen("http://", "ws://", 1)).into_client_request().unwrap();
    if let Some(token) = token {
        request.headers_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
    }
    match tokio_tungstenite::connect_async(request).await {
        Ok((ws, _)) => Ok(ws),
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => Err(StatusCode::from_u16(response.status().as_u16()).unwrap()),
        Err(e) => panic!("{e}"),
    }
}

async fn live_send(ws: &mut Ws, frame: ClientFrame) {
    use futures_util::SinkExt;
    ws.send(tokio_tungstenite::tungstenite::Message::Binary(frame.encode().unwrap().into())).await.unwrap();
}

/// The next frame from the server; `None` once the connection is closed.
async fn live_next(ws: &mut Ws) -> Option<ServerFrame> {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message;
    loop {
        match tokio::time::timeout(Duration::from_secs(10), ws.next()).await.expect("the server answers within 10 s") {
            Some(Ok(Message::Binary(bytes))) => return Some(ServerFrame::decode(&bytes).unwrap()),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            Some(Ok(Message::Close(_)) | Err(_)) | None => return None,
            Some(Ok(other)) => panic!("unexpected message {other:?}"),
        }
    }
}

/// Nothing arrives for a moment.
async fn live_quiet(ws: &mut Ws) {
    use futures_util::StreamExt;
    if let Ok(Some(Ok(message))) = tokio::time::timeout(Duration::from_millis(300), ws.next()).await {
        panic!("expected nothing, got {message:?}");
    }
}

/// A connection past `hello`.
async fn live_ready(s: &Server, token: &str) -> Ws {
    let mut ws = open_live(s, Some(token)).await.unwrap();
    // An agent of protocol 2, which is what these scenarios are about (protocol 3 is in `team_live.rs`).
    live_send(&mut ws, ClientFrame::Hello { min: LIVE_PROTOCOL_MIN, max: LIVE_DOCUMENTS_PROTOCOL }).await;
    let epoch = space_of(s, token).await.epoch;
    assert_eq!(
        live_next(&mut ws).await,
        Some(ServerFrame::Welcome { protocol: LIVE_DOCUMENTS_PROTOCOL, epoch }),
        "protocol 2, with the space's epoch"
    );
    ws
}

async fn space_of(s: &Server, token: &str) -> SpaceResponse {
    s.client.get(format!("{}/space", s.base)).bearer_auth(token).send().await.unwrap().json().await.unwrap()
}

/// Makes and subscribes to `app`'s recents document (after `after`); returns its ID, with the catch-up's frames.
async fn live_recents(s: &Server, ws: &mut Ws, token: &str, app: &str, after: u64) -> (DocId, Vec<ServerFrame>) {
    let app = AppName::new(app).unwrap();
    let doc = recents_doc(space_of(s, token).await.space, &app);
    live_send(ws, ClientFrame::Create { doc, kind: DocKind::Recents { app } }).await;
    assert_eq!(live_next(ws).await, Some(ServerFrame::Created { doc }));
    (doc, live_subscribe(ws, doc, after).await)
}

/// Subscribes and reads the catch-up up to and with `subscribed`.
async fn live_subscribe(ws: &mut Ws, doc: DocId, after: u64) -> Vec<ServerFrame> {
    live_send(ws, ClientFrame::Subscribe { doc, after, at: None }).await;
    let mut frames = Vec::new();
    loop {
        let frame = live_next(ws).await.expect("the catch-up");
        let done = matches!(frame, ServerFrame::Subscribed { .. } | ServerFrame::Error { .. });
        frames.push(frame);
        if done {
            return frames;
        }
    }
}

fn error_code(frame: Option<ServerFrame>) -> String {
    match frame {
        Some(ServerFrame::Error { code, .. }) => code,
        other => panic!("expected an error, got {other:?}"),
    }
}

async fn live_needs_a_device_token(s: &Server) {
    add_user(s, "ana").await;
    let token = login(s, "ana").await;
    assert_eq!(open_live(s, None).await.err(), Some(StatusCode::UNAUTHORIZED));
    assert_eq!(open_live(s, Some(&"x".repeat(43))).await.err(), Some(StatusCode::UNAUTHORIZED));
    // Signed in, but not a WebSocket: refused with the API's JSON error.
    let (status, body) = error(s.client.get(format!("{}/live", s.base)).bearer_auth(&token).send().await.unwrap()).await;
    assert!(status.is_client_error() && body.error == "invalid", "{status} {body:?}");
    // A device holds a few connections at most.
    let mut held = Vec::new();
    for _ in 0..live::MAX_CONNECTIONS_PER_DEVICE {
        held.push(open_live(s, Some(&token)).await.unwrap());
    }
    assert_eq!(open_live(s, Some(&token)).await.err(), Some(StatusCode::TOO_MANY_REQUESTS));
    drop(held);
}

/// Over a real WebSocket: a document of a file made by import with a snapshot bigger than an update may be (uploaded
/// through the chunk API, named by its manifest), kept once, named to another device catching up (which downloads it
/// the same way), and presence relayed between them. No message is over an update and its envelope.
async fn live_documents_carry_snapshots_and_presence(s: &Server) {
    add_user(s, "ana").await;
    let (pc_token, laptop_token) = (login(s, "ana").await, login(s, "ana").await);
    let m = upload(s, &pc_token, b"the file's bytes").await;
    let node = NodeId::generate();
    let r = root(s, &pc_token).await;
    assert_eq!(commit(s, &pc_token, &[create(node, r, "doc1.txt", m)]).await.status(), StatusCode::OK);
    let doc = craft_cloud_proto::live::node_doc(space_of(s, &pc_token).await.space, node);
    let mut pc = live_ready(s, &pc_token).await;
    let mut laptop = live_ready(s, &laptop_token).await;
    let snapshot: Vec<u8> = (0..3 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let manifest = upload(s, &pc_token, &snapshot).await;
    let app = AppName::new("wordcraft").unwrap();
    live_send(&mut pc, ClientFrame::CreateDocument { doc, node, app: app.clone(), snapshot: manifest }).await;
    assert_eq!(live_next(&mut pc).await, Some(ServerFrame::CreatedDocument { doc, kept: true }));
    let other = upload(s, &laptop_token, &[1; 10]).await;
    live_send(&mut laptop, ClientFrame::CreateDocument { doc, node, app, snapshot: other }).await;
    assert_eq!(live_next(&mut laptop).await, Some(ServerFrame::CreatedDocument { doc, kept: false }));
    let frames = live_subscribe(&mut laptop, doc, 0).await;
    assert_eq!(frames, vec![ServerFrame::Snapshot { doc, base: 0, manifest }, ServerFrame::Subscribed { doc, head: 0 }]);
    assert_eq!(download(s, &laptop_token, &manifest).await, snapshot, "the laptop gets the snapshot through the chunk API");
    live_subscribe(&mut pc, doc, 0).await;
    live_send(&mut pc, ClientFrame::Presence { doc, bytes: b"cursor at 7".to_vec() }).await;
    assert_eq!(live_next(&mut laptop).await, Some(ServerFrame::Presence { doc, bytes: b"cursor at 7".to_vec() }));
    live_quiet(&mut pc).await;
    // A message over an update and its envelope ends the connection, before `hello` or after.
    for mut ws in [laptop, open_live(s, Some(&pc_token)).await.unwrap()] {
        use futures_util::SinkExt;
        let big = vec![0u8; craft_cloud_proto::live::MAX_FRAME_BYTES + 1];
        let _ = ws.send(tokio_tungstenite::tungstenite::Message::Binary(big.into())).await;
        assert_eq!(live_next(&mut ws).await, None, "closed");
    }
}

async fn live_hello_checks_the_version(s: &Server) {
    add_user(s, "ana").await;
    let token = login(s, "ana").await;
    let mut ws = open_live(s, Some(&token)).await.unwrap();
    live_send(&mut ws, ClientFrame::Hello { min: LIVE_PROTOCOL_MAX + 1, max: LIVE_PROTOCOL_MAX + 5 }).await;
    assert_eq!(error_code(live_next(&mut ws).await), PROTOCOL_MISMATCH);
    assert_eq!(live_next(&mut ws).await, None, "closed after the mismatch");
    // Anything before `hello` is refused, and the connection ends.
    let mut ws = open_live(s, Some(&token)).await.unwrap();
    live_send(&mut ws, ClientFrame::Subscribe { doc: DocId::generate(), after: 0, at: None }).await;
    assert_eq!(error_code(live_next(&mut ws).await), "invalid");
    assert_eq!(live_next(&mut ws).await, None);
}

async fn live_documents_of_another_account_are_not_found(s: &Server) {
    add_user(s, "ana").await;
    add_user(s, "bob").await;
    let (ana, bob) = (login(s, "ana").await, login(s, "bob").await);
    let mut a = live_ready(s, &ana).await;
    let (doc, _) = live_recents(s, &mut a, &ana, "wordcraft", 0).await;
    live_send(&mut a, ClientFrame::Update { doc, client_seq: 1, bytes: b"ana's".to_vec() }).await;
    assert!(matches!(live_next(&mut a).await, Some(ServerFrame::Ack { seq: 1, .. })));

    let mut b = live_ready(s, &bob).await;
    // Ana's document and one that doesn't exist get the same answer.
    let theirs = live_subscribe(&mut b, doc, 0).await;
    let nobodys = live_subscribe(&mut b, DocId::generate(), 0).await;
    let strip = |frames: Vec<ServerFrame>| match frames.as_slice() {
        [ServerFrame::Error { code, message, .. }] => (code.clone(), message.clone()),
        other => panic!("{other:?}"),
    };
    assert_eq!(strip(theirs), strip(nobodys));
    assert_eq!(strip(live_subscribe(&mut b, doc, 0).await).0, "not_found");
    // Nor can Bob write to it, or make it in his space under its ID.
    live_send(&mut b, ClientFrame::Update { doc, client_seq: 1, bytes: b"bob's".to_vec() }).await;
    assert_eq!(error_code(live_next(&mut b).await), "not_found");
    let kind = DocKind::Recents { app: AppName::new("wordcraft").unwrap() };
    live_send(&mut b, ClientFrame::Create { doc, kind }).await;
    assert_eq!(error_code(live_next(&mut b).await), "invalid");
    // Bob's own recents document is another one, and empty.
    let (bobs, frames) = live_recents(s, &mut b, &bob, "wordcraft", 0).await;
    assert_ne!(bobs, doc);
    assert_eq!(frames, vec![ServerFrame::Subscribed { doc: bobs, head: 0 }]);
    let ana_space = space_of(s, &ana).await.space;
    assert_eq!(s.meta.live_updates(ana_space, doc, 0, 10).await.unwrap().len(), 1, "Ana's log is untouched");
}

async fn live_updates_are_durable_before_the_ack(s: &Server) {
    add_user(s, "ana").await;
    let token = login(s, "ana").await;
    let space = space_of(s, &token).await.space;
    let mut ws = live_ready(s, &token).await;
    let (doc, _) = live_recents(s, &mut ws, &token, "wordcraft", 0).await;
    for (client_seq, bytes) in [(1, &b"first"[..]), (2, &b"second"[..])] {
        live_send(&mut ws, ClientFrame::Update { doc, client_seq, bytes: bytes.to_vec() }).await;
        let Some(ServerFrame::Ack { seq, client_seq: acked, .. }) = live_next(&mut ws).await else { panic!("an ack") };
        assert_eq!((acked, seq), (client_seq, client_seq));
        // By the time the ack arrives the update is in the database (committed: another connection reads it).
        let stored = s.meta.live_updates(space, doc, seq - 1, 1).await.unwrap();
        assert_eq!(stored.first().map(|u| u.bytes.as_slice()), Some(bytes));
    }
    // Sent again after a lost ack: acked with its first number, stored once.
    live_send(&mut ws, ClientFrame::Update { doc, client_seq: 1, bytes: b"first".to_vec() }).await;
    assert!(matches!(live_next(&mut ws).await, Some(ServerFrame::Ack { client_seq: 1, seq: 1, .. })));
    assert_eq!(s.meta.live_doc(space, doc).await.unwrap().unwrap().head, 2);
    // Its own updates don't come back to the device that sent them.
    live_quiet(&mut ws).await;
    // An update to a document it didn't subscribe to is refused.
    live_send(&mut ws, ClientFrame::Unsubscribe { doc }).await;
    live_send(&mut ws, ClientFrame::Update { doc, client_seq: 3, bytes: b"third".to_vec() }).await;
    assert_eq!(error_code(live_next(&mut ws).await), "not_found");
}

async fn live_updates_fan_out_to_the_accounts_other_devices(s: &Server) {
    add_user(s, "ana").await;
    add_user(s, "bob").await;
    let (pc, laptop, bob) = (login(s, "ana").await, login(s, "ana").await, login(s, "bob").await);
    let (mut a, mut b, mut c) = (live_ready(s, &pc).await, live_ready(s, &laptop).await, live_ready(s, &bob).await);
    let (doc, _) = live_recents(s, &mut a, &pc, "wordcraft", 0).await;
    let (same, _) = live_recents(s, &mut b, &laptop, "wordcraft", 0).await;
    assert_eq!(doc, same, "one recents document per app per account");
    live_recents(s, &mut c, &bob, "wordcraft", 0).await;
    live_send(&mut a, ClientFrame::Update { doc, client_seq: 7, bytes: b"opened doc1".to_vec() }).await;
    assert_eq!(live_next(&mut a).await, Some(ServerFrame::Ack { doc, client_seq: 7, seq: 1 }));
    let Some(ServerFrame::Update { doc: got, seq, bytes, device }) = live_next(&mut b).await else { panic!("the laptop gets it") };
    assert_eq!((got, seq, bytes.as_slice()), (doc, 1, &b"opened doc1"[..]));
    let devices: Vec<DeviceInfo> = s.client.get(format!("{}/devices", s.base)).bearer_auth(&pc).send().await.unwrap().json().await.unwrap();
    assert_eq!(Some(device), devices.iter().find(|d| d.current).map(|d| d.id), "it names the device that sent it");
    live_quiet(&mut c).await;
    live_quiet(&mut a).await;
}

async fn live_catch_up_after_a_gap(s: &Server) {
    add_user(s, "ana").await;
    let (pc, laptop) = (login(s, "ana").await, login(s, "ana").await);
    let mut a = live_ready(s, &pc).await;
    let (doc, _) = live_recents(s, &mut a, &pc, "wordcraft", 0).await;
    let mut b = live_ready(s, &laptop).await;
    live_recents(s, &mut b, &laptop, "wordcraft", 0).await;
    live_send(&mut a, ClientFrame::Update { doc, client_seq: 1, bytes: vec![1] }).await;
    live_next(&mut a).await;
    assert!(matches!(live_next(&mut b).await, Some(ServerFrame::Update { seq: 1, .. })));
    // The laptop goes away; three more updates come meanwhile.
    drop(b);
    for i in 2..=4u8 {
        live_send(&mut a, ClientFrame::Update { doc, client_seq: u64::from(i), bytes: vec![i] }).await;
        live_next(&mut a).await;
    }
    // Back, it says it holds everything up to 1: exactly the three it lacks, in order, then the end of the catch-up.
    let mut b = live_ready(s, &laptop).await;
    let frames = live_subscribe(&mut b, doc, 1).await;
    let seqs: Vec<(u64, Vec<u8>)> = frames
        .iter()
        .filter_map(|f| match f {
            ServerFrame::Update { seq, bytes, .. } => Some((*seq, bytes.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(seqs, vec![(2, vec![2]), (3, vec![3]), (4, vec![4])]);
    assert_eq!(frames.last(), Some(&ServerFrame::Subscribed { doc, head: 4 }));
    // The catch-up includes the device's own updates (it may have lost them): the PC, from scratch, gets all four.
    let mut fresh = live_ready(s, &pc).await;
    let all = live_subscribe(&mut fresh, doc, 0).await;
    assert_eq!(all.iter().filter(|f| matches!(f, ServerFrame::Update { .. })).count(), 4);
    // An agent that holds more than the server (restored from a backup) is told where the log ends.
    let ahead = live_subscribe(&mut b, doc, 10).await;
    assert_eq!(ahead, vec![ServerFrame::Subscribed { doc, head: 4 }]);
    // After the catch-up, new updates follow live.
    live_send(&mut a, ClientFrame::Update { doc, client_seq: 5, bytes: vec![5] }).await;
    assert!(matches!(live_next(&mut b).await, Some(ServerFrame::Update { seq: 5, .. })));
}

/// A device revoked through the API loses its live connection at once.
async fn live_a_revoked_device_is_cut_off(s: &Server) {
    add_user(s, "ana").await;
    let (pc, laptop) = (login(s, "ana").await, login(s, "ana").await);
    let mut ws = live_ready(s, &pc).await;
    live_recents(s, &mut ws, &pc, "wordcraft", 0).await;
    let devices: Vec<DeviceInfo> = s.client.get(format!("{}/devices", s.base)).bearer_auth(&pc).send().await.unwrap().json().await.unwrap();
    let pc_id = devices.iter().find(|d| d.current).unwrap().id;
    let res = s.client.delete(format!("{}/devices/{pc_id}", s.base)).bearer_auth(&laptop).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    assert_eq!(error_code(live_next(&mut ws).await), "unauthorized");
    assert_eq!(live_next(&mut ws).await, None, "closed");
    assert_eq!(open_live(s, Some(&pc)).await.err(), Some(StatusCode::UNAUTHORIZED));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_subscriptions_are_capped_per_device() {
    // Uses a custom live burst budget to isolate the subscription cap.
    // A budget big enough for 2 × 257 frames at once: the cap, not the budget, is what this checks.
    let db = tempfile::tempdir().unwrap();
    let blobs = tempfile::tempdir().unwrap();
    let meta = Meta::open(&format!("sqlite:{}", db.path().join("meta.db").display())).await.unwrap();
    let store = Store::new(DiskBackend::new(blobs.path()).unwrap());
    let s = &start_with(meta, store, vec![db, blobs], None, Limits { live_burst: 10_000, ..Limits::default() }).await;
    add_user(s, "ana").await;
    let token = login(s, "ana").await;
    let space = space_of(s, &token).await.space;
    let mut ws = live_ready(s, &token).await;
    for i in 0..=MAX_SUBSCRIPTIONS {
        let app = AppName::new(&format!("app{i}")).unwrap();
        let doc = recents_doc(space, &app);
        live_send(&mut ws, ClientFrame::Create { doc, kind: DocKind::Recents { app } }).await;
        assert_eq!(live_next(&mut ws).await, Some(ServerFrame::Created { doc }));
        let frames = live_subscribe(&mut ws, doc, 0).await;
        if i < MAX_SUBSCRIPTIONS {
            assert_eq!(frames, vec![ServerFrame::Subscribed { doc, head: 0 }]);
        } else {
            assert_eq!(error_code(frames.into_iter().next()), "rate_limited", "the 257th");
        }
    }
    // Across the device's connections, too; one given up frees a place.
    let mut other = live_ready(s, &token).await;
    let doc = recents_doc(space, &AppName::new(&format!("app{MAX_SUBSCRIPTIONS}")).unwrap());
    assert_eq!(error_code(live_subscribe(&mut other, doc, 0).await.into_iter().next()), "rate_limited");
    live_send(&mut ws, ClientFrame::Unsubscribe { doc: recents_doc(space, &AppName::new("app0").unwrap()) }).await;
    live_quiet(&mut ws).await;
    assert_eq!(live_subscribe(&mut other, doc, 0).await, vec![ServerFrame::Subscribed { doc, head: 0 }]);
}

// ---- device sessions: the account, passwords and devices ----

macro_rules! on_sqlite_and_postgres {
    ($($scenario:ident),* $(,)?) => {
        mod accounts_on_sqlite {
            $( #[tokio::test(flavor = "multi_thread", worker_threads = 4)] async fn $scenario() { super::$scenario(&super::sqlite_disk().await).await; } )*
        }
        mod accounts_on_postgres {
            $( #[tokio::test(flavor = "multi_thread", worker_threads = 4)] async fn $scenario() {
                let Some(server) = super::postgres_disk().await else { eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set"); return };
                super::$scenario(&server).await;
            } )*
        }
    };
}

on_sqlite_and_postgres!(
    sign_ins_and_joins_may_name_their_client,
    the_account_route_describes_the_signed_in_account,
    changing_the_password_signs_out_the_other_devices,
    a_wrong_current_password_is_403_and_changes_nothing,
    password_change_tries_are_limited,
    password_bodies_are_tiny,
    a_device_is_renamed_by_its_owner_only,
    other_devices_are_signed_out_at_once,
    revoking_a_device_records_why_once,
    an_older_client_reads_the_device_list,
    a_waiting_long_poll_of_a_signed_out_device_gets_no_changes,
    every_admin_route_needs_an_admin,
    every_admin_change_needs_the_admins_password,
    wrong_confirmation_passwords_are_limited,
    refused_tries_do_not_eat_the_refund_of_a_right_password,
    a_demoted_admin_loses_the_admin_routes_at_once,
    the_last_admin_stays_and_nobody_disables_themselves,
    a_disabled_account_is_shut_out_everywhere,
    removing_an_account_signs_it_out_and_keeps_its_name,
    the_user_list_pages_and_searches,
    an_admin_makes_accounts_and_invites,
    a_password_reset_code_signs_the_redeeming_computer_in,
    the_server_overview_shows_counters_only,
    an_admin_lists_and_revokes_a_users_devices_over_http,
    an_admin_sets_quotas_that_hold_before_any_byte_is_stored,
    two_factor_is_set_up_confirmed_and_turned_off,
    a_set_up_on_one_device_is_pending_for_the_others,
    the_two_factor_routes_refuse_a_signed_out_device_and_big_or_odd_bodies,
    a_sign_in_with_two_factor_needs_the_second_step,
    a_challenge_expires_takes_five_tries_and_dies_with_a_password_change,
    wrong_codes_lock_the_second_factor_for_a_while,
    wrong_second_steps_are_counted_and_limited,
    joining_an_account_with_two_factor_needs_the_factor,
    wrong_second_factors_at_join_count_against_the_address_and_the_account,
    a_reset_code_for_a_protected_account,
    turning_two_factor_on_ends_live_connections_and_long_polls,
    an_admin_with_two_factor_confirms_changes_with_a_code,
    an_admin_resets_the_two_factor_of_an_account,
    two_factor_sign_in_and_join_are_audited,
);

async fn devices_of(s: &Server, token: &str) -> Vec<DeviceInfo> {
    let res = s.client.get(format!("{}/devices", s.base)).bearer_auth(token).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    res.json().await.unwrap()
}

async fn change_password(s: &Server, token: &str, current: &str, new: &str) -> reqwest::Response {
    s.client
        .post(format!("{}/account/password", s.base))
        .bearer_auth(token)
        .json(&json!({ "current_password": current, "new_password": new }))
        .send()
        .await
        .unwrap()
}

async fn login_with(s: &Server, name: &str, password: &str) -> reqwest::Response {
    s.client
        .post(format!("{}/auth/login", s.base))
        .json(&json!({ "username": name, "password": password, "device_name": "laptop" }))
        .send()
        .await
        .unwrap()
}

/// Whether the token is still accepted: a signed-in route answers 200, or 401 once the device was revoked.
async fn accepted(s: &Server, token: &str) -> bool {
    match s.client.get(format!("{}/devices", s.base)).bearer_auth(token).send().await.unwrap().status() {
        StatusCode::OK => true,
        StatusCode::UNAUTHORIZED => false,
        other => panic!("{other}"),
    }
}

async fn sign_ins_and_joins_may_name_their_client(s: &Server) {
    add_user(s, "ana").await;
    // The fields are shown in the device list.
    let named = s
        .client
        .post(format!("{}/auth/login", s.base))
        .json(&json!({ "username": "ana", "password": PASSWORD, "device_name": "studio", "client_kind": "agent", "platform": "windows x86_64", "app_version": "0.19.0" }))
        .send()
        .await
        .unwrap();
    assert_eq!(named.status(), StatusCode::OK);
    let named: LoginResponse = named.json().await.unwrap();
    // Without them the answer and the sign-in are as before.
    let plain = login(s, "ana").await;
    let devices = devices_of(s, &plain).await;
    let studio = devices.iter().find(|d| d.id == named.device).unwrap();
    assert_eq!((studio.client_kind.as_str(), studio.platform.as_str(), studio.app_version.as_str()), ("agent", "windows x86_64", "0.19.0"));
    let old = devices.iter().find(|d| d.current).unwrap();
    assert_eq!(
        (old.client_kind.as_str(), old.platform.as_str(), old.app_version.as_str(), old.revoked_ms, old.revoke_reason.clone()),
        ("", "", "", None, None)
    );

    // A refused body costs no attempt: more than the account's ten tries, and it still signs in.
    let long = "x".repeat(33);
    for bad in [
        json!({ "client_kind": "browser" }),
        json!({ "client_kind": "web" }),
        json!({ "platform": long }),
        json!({ "app_version": long }),
        json!({ "platform": "a\nb" }),
        json!({ "client_kind": "AGENT" }),
    ] {
        for _ in 0..3 {
            let mut body = json!({ "username": "ana", "password": PASSWORD, "device_name": "x" });
            body.as_object_mut().unwrap().extend(bad.as_object().unwrap().clone());
            let (status, answer) = error(s.client.post(format!("{}/auth/login", s.base)).json(&body).send().await.unwrap()).await;
            assert_eq!((status, answer.error.as_str()), (StatusCode::BAD_REQUEST, "invalid"), "{bad}");
        }
    }
    login(s, "ana").await;

    // The same for a join: with the fields it records them, a bad one is refused and the code is not used up.
    let made = new_pairing(s, &plain).await;
    for bad in [json!({ "client_kind": "browser" }), json!({ "client_kind": "web" }), json!({ "app_version": long })] {
        let mut body = json!({ "code": made.code, "device_name": "pc" });
        body.as_object_mut().unwrap().extend(bad.as_object().unwrap().clone());
        let (status, answer) = error(s.client.post(format!("{}/join", s.base)).json(&body).send().await.unwrap()).await;
        assert_eq!((status, answer.error.as_str()), (StatusCode::BAD_REQUEST, "invalid"), "{bad}");
    }
    let body = json!({ "code": made.code, "device_name": "mac", "client_kind": "desktop", "platform": "macos", "app_version": "1.2" });
    let joined = joined(s.client.post(format!("{}/join", s.base)).json(&body).send().await.unwrap()).await;
    let list = devices_of(s, &joined.token).await;
    let mac = list.iter().find(|d| d.current).unwrap();
    assert_eq!((mac.client_kind.as_str(), mac.platform.as_str(), mac.app_version.as_str()), ("desktop", "macos", "1.2"));
}

async fn the_account_route_describes_the_signed_in_account(s: &Server) {
    add_user(s, "ana").await;
    let token = login(s, "ana").await;
    let before = craft_cloud_server::state::now_ms();
    let res = s.client.get(format!("{}/account", s.base)).bearer_auth(&token).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let account: serde_json::Value = res.json().await.unwrap();
    let keys: std::collections::BTreeSet<_> = account.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["created_ms", "password_changed_ms", "role", "two_factor_enabled", "user_id", "username"].into(), "flat, no nested object");
    assert_eq!((account["username"].as_str(), account["role"].as_str()), (Some("ana"), Some("member")));
    assert_eq!((account["password_changed_ms"].is_null(), account["two_factor_enabled"].as_bool()), (true, Some(false)));
    assert!(account["user_id"].as_str().is_some_and(|id| id.parse::<uuid::Uuid>().is_ok()));
    assert!(account["created_ms"].as_i64().is_some());
    // Signed in only.
    assert_eq!(s.client.get(format!("{}/account", s.base)).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
    // The time of the change is the server's.
    assert_eq!(change_password(s, &token, PASSWORD, "a brand new password").await.status(), StatusCode::NO_CONTENT);
    let account: serde_json::Value = s.client.get(format!("{}/account", s.base)).bearer_auth(&token).send().await.unwrap().json().await.unwrap();
    let changed = account["password_changed_ms"].as_i64().unwrap();
    assert!((before..=craft_cloud_server::state::now_ms()).contains(&changed), "{changed}");
}

async fn changing_the_password_signs_out_the_other_devices(s: &Server) {
    add_user(s, "ana").await;
    add_user(s, "bob").await;
    let (pc, laptop, phone) = (login(s, "ana").await, login(s, "ana").await, login(s, "ana").await);
    let bob = login(s, "bob").await;
    let mut ws = live_ready(s, &laptop).await;
    let pairing = new_pairing(s, &laptop).await;
    let invite = Code::generate(INVITE_CHARS).unwrap();
    s.meta.create_invite("ana", &invite.hash(), craft_cloud_server::state::now_ms(), INVITE_TTL_MS).await.unwrap();
    let bobs_pairing = new_pairing(s, &bob).await;
    let new = "a brand new password";

    // A password that is too short, or the same one again, is refused and nothing is signed out.
    let (status, body) = error(change_password(s, &pc, PASSWORD, "short").await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::BAD_REQUEST, "weak_password"));
    let (status, body) = error(change_password(s, &pc, PASSWORD, PASSWORD).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::BAD_REQUEST, "weak_password"), "{}", body.message);
    assert!(accepted(s, &laptop).await && accepted(s, &phone).await);

    let before = craft_cloud_server::state::now_ms();
    assert_eq!(change_password(s, &pc, PASSWORD, new).await.status(), StatusCode::NO_CONTENT);
    // The other devices are signed out, the one that asked is not.
    assert!(accepted(s, &pc).await);
    assert!(!accepted(s, &laptop).await && !accepted(s, &phone).await);
    // A device that is signed in on a live connection is cut off at once.
    assert_eq!(error_code(live_next(&mut ws).await), "unauthorized");
    assert_eq!(live_next(&mut ws).await, None, "closed");
    // The account's open invite and its pairings are gone: neither code works any more.
    for code in [invite.as_str(), pairing.code.as_str()] {
        let (status, body) = error(join(s, code, "thief", None).await).await;
        assert_eq!((status, body.error.as_str()), (StatusCode::NOT_FOUND, "invalid_code"));
    }
    assert_eq!(pairing_status(s, &pc, pairing.id).await.status(), StatusCode::NOT_FOUND);
    // The old password no longer signs in, the new one does.
    assert_eq!(login_with(s, "ana", PASSWORD).await.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(login_with(s, "ana", new).await.status(), StatusCode::OK);
    // The list says why, and when.
    let devices = devices_of(s, &pc).await;
    let revoked: Vec<_> = devices.iter().filter(|d| d.revoked).collect();
    assert_eq!(revoked.len(), 2);
    for d in revoked {
        assert_eq!(d.revoke_reason.as_deref(), Some("password"));
        assert!(d.revoked_ms.is_some_and(|ms| (before..=craft_cloud_server::state::now_ms()).contains(&ms)), "{d:?}");
    }
    let me = devices.iter().find(|d| d.current).unwrap();
    assert_eq!((me.revoked, me.revoked_ms, me.revoke_reason.clone()), (false, None, None));
    // Another account is never touched.
    assert!(accepted(s, &bob).await);
    assert_eq!(pairing_status(s, &bob, bobs_pairing.id).await.status(), StatusCode::OK);
    assert_eq!(login_with(s, "bob", PASSWORD).await.status(), StatusCode::OK);
}

async fn a_wrong_current_password_is_403_and_changes_nothing(s: &Server) {
    add_user(s, "ana").await;
    let (pc, laptop) = (login(s, "ana").await, login(s, "ana").await);
    let res = change_password(s, &pc, "not my password", "a brand new password").await;
    // 403, never 401: a 401 on a signed-in route makes the agent think the device was revoked and sign out.
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let body: ErrorBody = res.json().await.unwrap();
    assert_eq!(body.error, "wrong_password");
    assert!(!body.message.contains("not my password") && !body.message.contains("a brand new password"));
    // Nothing happened: the device and the others are still signed in, the old password still works.
    assert!(accepted(s, &pc).await && accepted(s, &laptop).await);
    assert_eq!(login_with(s, "ana", PASSWORD).await.status(), StatusCode::OK);
    assert_eq!(login_with(s, "ana", "a brand new password").await.status(), StatusCode::UNAUTHORIZED);
    // A missing token is a 401 (a route that needs a device), not a password answer.
    let res = s
        .client
        .post(format!("{}/account/password", s.base))
        .json(&json!({ "current_password": PASSWORD, "new_password": "a brand new password" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    // A body that isn't what the route reads is a 400, and nothing changes either.
    let res =
        s.client.post(format!("{}/account/password", s.base)).bearer_auth(&pc).json(&json!({ "current_password": PASSWORD })).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(accepted(s, &laptop).await);
}

async fn password_change_tries_are_limited(s: &Server) {
    add_user(s, "ana").await;
    add_user(s, "bob").await;
    let (pc, laptop) = (login(s, "ana").await, login(s, "ana").await);
    for _ in 0..5 {
        assert_eq!(change_password(s, &pc, "not my password", "a brand new password").await.status(), StatusCode::FORBIDDEN);
    }
    // The sixth, even with the right password, is refused for the window.
    let (status, body) = error(change_password(s, &pc, PASSWORD, "a brand new password").await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::TOO_MANY_REQUESTS, "rate_limited"));
    assert!(accepted(s, &laptop).await, "nothing was changed");
    // Another account has its own tries.
    let bob = login(s, "bob").await;
    assert_eq!(change_password(s, &bob, "not my password", "a brand new password").await.status(), StatusCode::FORBIDDEN);
}

async fn password_bodies_are_tiny(s: &Server) {
    add_user(s, "ana").await;
    let token = login(s, "ana").await;
    let huge = "x".repeat(5000);
    let res = change_password(s, &token, &huge, "a brand new password").await;
    let (status, body) = error(res).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::PAYLOAD_TOO_LARGE, "too_large"));
    let id = current_device(s, &token).await;
    let res = s.client.patch(format!("{}/devices/{id}", s.base)).bearer_auth(&token).json(&json!({ "name": huge })).send().await.unwrap();
    let (status, body) = error(res).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::PAYLOAD_TOO_LARGE, "too_large"));
    // A password over 1024 bytes in a body that is not too big: refused as invalid, and nothing is counted.
    for _ in 0..7 {
        let (status, body) = error(change_password(s, &token, &"x".repeat(1025), "a brand new password").await).await;
        assert_eq!((status, body.error.as_str()), (StatusCode::BAD_REQUEST, "invalid"));
        let (status, body) = error(change_password(s, &token, PASSWORD, &"x".repeat(1025)).await).await;
        assert_eq!((status, body.error.as_str()), (StatusCode::BAD_REQUEST, "invalid"));
    }
    assert_eq!(change_password(s, &token, PASSWORD, "a brand new password").await.status(), StatusCode::NO_CONTENT);
}

async fn a_device_is_renamed_by_its_owner_only(s: &Server) {
    add_user(s, "ana").await;
    add_user(s, "bob").await;
    let (pc, laptop, bob) = (login(s, "ana").await, login(s, "ana").await, login(s, "bob").await);
    let (pc_id, laptop_id, bobs_id) = (current_device(s, &pc).await, current_device(s, &laptop).await, current_device(s, &bob).await);
    let patch = |token: &str, id: craft_cloud_proto::DeviceId, name: serde_json::Value| {
        let req = s.client.patch(format!("{}/devices/{id}", s.base)).bearer_auth(token).json(&json!({ "name": name }));
        async move { req.send().await.unwrap() }
    };
    // Its own, and another of the account's.
    let res = patch(&pc, pc_id, json!("studio pc")).await;
    assert_eq!(res.status(), StatusCode::OK);
    let renamed: DeviceInfo = res.json().await.unwrap();
    assert_eq!((renamed.id, renamed.name.as_str(), renamed.current), (pc_id, "studio pc", true));
    let renamed: DeviceInfo = patch(&pc, laptop_id, json!("old laptop")).await.json().await.unwrap();
    assert_eq!((renamed.name.as_str(), renamed.current), ("old laptop", false));
    let names: Vec<_> = devices_of(s, &pc).await.into_iter().map(|d| d.name).collect();
    assert_eq!(names, ["studio pc", "old laptop"]);
    // Someone else's device and one that doesn't exist are the same answer: not found, never forbidden.
    for id in [bobs_id, craft_cloud_proto::DeviceId::generate()] {
        let (status, body) = error(patch(&pc, id, json!("mine now")).await).await;
        assert_eq!((status, body.error.as_str()), (StatusCode::NOT_FOUND, "not_found"));
    }
    assert_eq!(devices_of(s, &bob).await[0].name, "laptop");
    // The name rule holds.
    for bad in [json!(""), json!("x".repeat(129)), json!("tab\there")] {
        let (status, body) = error(patch(&pc, pc_id, bad.clone()).await).await;
        assert_eq!((status, body.error.as_str()), (StatusCode::BAD_REQUEST, "invalid"), "{bad}");
    }
    // A revoked device is not renamed.
    assert_eq!(s.client.delete(format!("{}/devices/{laptop_id}", s.base)).bearer_auth(&pc).send().await.unwrap().status(), StatusCode::NO_CONTENT);
    let (status, body) = error(patch(&pc, laptop_id, json!("again")).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::NOT_FOUND, "not_found"));
    // Signed in only.
    let res = s.client.patch(format!("{}/devices/{pc_id}", s.base)).json(&json!({ "name": "x" })).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

async fn other_devices_are_signed_out_at_once(s: &Server) {
    add_user(s, "ana").await;
    add_user(s, "bob").await;
    let (pc, laptop, phone) = (login(s, "ana").await, login(s, "ana").await, login(s, "ana").await);
    let bob = login(s, "bob").await;
    let mut ws = live_ready(s, &laptop).await;
    let pairing = new_pairing(s, &phone).await;
    let post = |token: &str| s.client.post(format!("{}/devices/revoke-others", s.base)).bearer_auth(token).send();
    let before = craft_cloud_server::state::now_ms();
    let res = post(&pc).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.json::<serde_json::Value>().await.unwrap(), json!({ "revoked": 2 }));
    assert!(accepted(s, &pc).await && !accepted(s, &laptop).await && !accepted(s, &phone).await);
    assert_eq!(error_code(live_next(&mut ws).await), "unauthorized");
    assert_eq!(live_next(&mut ws).await, None, "closed");
    // A revoked device's add-a-computer code dies with it.
    let (status, body) = error(join(s, &pairing.code, "thief", None).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::NOT_FOUND, "invalid_code"));
    let devices = devices_of(s, &pc).await;
    for d in devices.iter().filter(|d| d.revoked) {
        assert_eq!(d.revoke_reason.as_deref(), Some("others"));
        assert!(d.revoked_ms.is_some_and(|ms| (before..=craft_cloud_server::state::now_ms()).contains(&ms)), "{d:?}");
    }
    assert_eq!(devices.iter().filter(|d| d.revoked).count(), 2);
    // With nobody else left it says so, and answers the same way again.
    let res = post(&pc).await.unwrap();
    assert_eq!(res.json::<serde_json::Value>().await.unwrap(), json!({ "revoked": 0 }));
    // Another account is never touched, and a request needs a device.
    assert!(accepted(s, &bob).await);
    assert_eq!(s.client.post(format!("{}/devices/revoke-others", s.base)).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
}

async fn revoking_a_device_records_why_once(s: &Server) {
    add_user(s, "ana").await;
    let (pc, laptop) = (login(s, "ana").await, login(s, "ana").await);
    let laptop_id = current_device(s, &laptop).await;
    let before = craft_cloud_server::state::now_ms();
    let delete = || s.client.delete(format!("{}/devices/{laptop_id}", s.base)).bearer_auth(&pc).send();
    assert_eq!(delete().await.unwrap().status(), StatusCode::NO_CONTENT);
    let first = devices_of(s, &pc).await.into_iter().find(|d| d.id == laptop_id).unwrap();
    assert_eq!((first.revoked, first.revoke_reason.as_deref()), (true, Some("user")));
    assert!(first.revoked_ms.is_some_and(|ms| (before..=craft_cloud_server::state::now_ms()).contains(&ms)), "{first:?}");
    // Again: still a 204, and the first time and reason stay.
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(delete().await.unwrap().status(), StatusCode::NO_CONTENT);
    assert_eq!(devices_of(s, &pc).await.into_iter().find(|d| d.id == laptop_id).unwrap(), first);
}

/// The device list of a client from before the new fields: it reads what it knows and ignores the rest. And a
/// sign-in body from such a client (no client fields) works.
async fn an_older_client_reads_the_device_list(s: &Server) {
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct OldDeviceInfo {
        id: craft_cloud_proto::DeviceId,
        name: String,
        created_ms: i64,
        last_used_ms: i64,
        revoked: bool,
        current: bool,
    }
    add_user(s, "ana").await;
    let token = login(s, "ana").await;
    let list: Vec<OldDeviceInfo> = s.client.get(format!("{}/devices", s.base)).bearer_auth(&token).send().await.unwrap().json().await.unwrap();
    assert_eq!(list.len(), 1);
    assert!(list[0].current && !list[0].revoked && list[0].name == "laptop");
    // The new fields are on the wire, and a reader without them gets what it had.
    let raw: Vec<serde_json::Value> = s.client.get(format!("{}/devices", s.base)).bearer_auth(&token).send().await.unwrap().json().await.unwrap();
    for key in ["client_kind", "platform", "app_version", "revoked_ms", "revoke_reason"] {
        assert!(raw[0].get(key).is_some(), "{key}");
    }
    // And the reverse: a list from an older server (without them) reads with empty values.
    let older = json!({ "id": list[0].id, "name": "pc", "created_ms": 1, "last_used_ms": 2, "revoked": false, "current": true });
    let parsed: DeviceInfo = serde_json::from_value(older).unwrap();
    assert_eq!((parsed.client_kind.as_str(), parsed.revoked_ms, parsed.revoke_reason), ("", None, None));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn password_changes_wait_for_a_free_check_slot_and_cost_no_try() {
    let s = sqlite_disk_with(Limits { password_checks: 1, ..Limits::default() }).await;
    add_user(&s, "ana").await;
    let (pc, laptop) = (login(&s, "ana").await, login(&s, "ana").await);
    // Every slot busy: told to come back in a second, more often than the account's five tries…
    let busy = s.password_checks.clone().try_acquire_owned().unwrap();
    for _ in 0..8 {
        let res = change_password(&s, &pc, PASSWORD, "a brand new password").await;
        assert_eq!(res.headers().get("retry-after").and_then(|v| v.to_str().ok()), Some("1"));
        let (status, body) = error(res).await;
        assert_eq!((status, body.error.as_str()), (StatusCode::TOO_MANY_REQUESTS, "rate_limited"));
    }
    assert!(accepted(&s, &laptop).await, "nothing was checked or changed");
    // …and none of them cost a try: once a slot is free the change goes through.
    drop(busy);
    assert_eq!(change_password(&s, &pc, PASSWORD, "a brand new password").await.status(), StatusCode::NO_CONTENT);
    // The slot is given back after the change.
    assert_eq!(login_with(&s, "ana", "a brand new password").await.status(), StatusCode::OK);
}

/// A device signed out while its `/changes` long-poll waits is told so (401) instead of being sent the next change
/// set: when the API signs it out (the wait ends at once) and when something else does, such as the command line in
/// another process, which wakes nothing (the next commit ends the wait, and the device is looked up again).
async fn a_waiting_long_poll_of_a_signed_out_device_gets_no_changes(s: &Server) {
    add_user(s, "ana").await;
    let other = login(s, "ana").await;
    let ana = s.meta.user_by_name("ana").await.unwrap().unwrap();
    let poll = |token: String, since: u64| {
        let (client, url) = (s.client.clone(), format!("{}/changes?since={since}&wait=30", s.base));
        tokio::spawn(async move { client.get(url).bearer_auth(token).send().await.unwrap() })
    };
    let commit_a_file = |name: &'static str| {
        let other = other.clone();
        async move {
            let data = format!("{name} {}", uuid::Uuid::now_v7()).into_bytes();
            let manifest = upload(s, &other, &data).await;
            let parent = root(s, &other).await;
            assert_eq!(commit(s, &other, &[create(NodeId::generate(), parent, name, manifest)]).await.status(), StatusCode::OK);
        }
    };

    // Signed out through the API: the wait ends at once, with 401.
    let first = login(s, "ana").await;
    let since = space_of(s, &other).await.seq;
    let waiting = poll(first.clone(), since);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!waiting.is_finished(), "the poll waits");
    let started = Instant::now();
    assert_eq!(s.client.post(format!("{}/devices/revoke-others", s.base)).bearer_auth(&other).send().await.unwrap().status(), StatusCode::OK);
    let (status, body) = error(tokio::time::timeout(Duration::from_secs(10), waiting).await.expect("the wait ends").unwrap()).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::UNAUTHORIZED, "unauthorized"));
    assert!(started.elapsed() < Duration::from_secs(10));
    commit_a_file("a.txt").await;

    // Signed out behind the server's back: the next commit wakes the poll, and it is refused all the same.
    let second = login(s, "ana").await;
    let since = space_of(s, &other).await.seq;
    let waiting = poll(second.clone(), since);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let second_id = current_device(s, &second).await;
    s.meta.revoke_device_as(ana.id, second_id, craft_cloud_meta::RevokeReason::Admin, 1).await.unwrap();
    assert!(!waiting.is_finished(), "nothing woke the poll");
    commit_a_file("b.txt").await;
    let (status, body) = error(tokio::time::timeout(Duration::from_secs(10), waiting).await.expect("the commit ends the wait").unwrap()).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::UNAUTHORIZED, "unauthorized"));

    // A device that stays signed in still gets its changes.
    let since = space_of(s, &other).await.seq;
    let waiting = poll(other.clone(), since);
    tokio::time::sleep(Duration::from_millis(300)).await;
    commit_a_file("c.txt").await;
    let res = tokio::time::timeout(Duration::from_secs(10), waiting).await.expect("the commit ends the wait").unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.json::<ChangeSet>().await.unwrap().seq, since + 1);
}

// ---- the admin API ----

async fn add_admin(s: &Server, name: &str) {
    s.meta.create_user(name, &hash_password(PASSWORD).unwrap(), Role::Admin, 0).await.unwrap();
}

async fn id_of(s: &Server, name: &str) -> uuid::Uuid {
    s.meta.user_by_name(name).await.unwrap().unwrap().id.0
}

/// A call to the API; `body` is sent as JSON. `token` `None`: no sign-in.
async fn call(s: &Server, token: Option<&str>, method: reqwest::Method, path: &str, body: Option<serde_json::Value>) -> reqwest::Response {
    let mut req = s.client.request(method, format!("{}{path}", s.base));
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    if let Some(body) = body {
        req = req.json(&body);
    }
    req.send().await.unwrap()
}

/// An admin call with the confirmation: `extra` (an object) plus the password.
async fn admin_call(s: &Server, token: &str, method: reqwest::Method, path: &str, extra: serde_json::Value) -> reqwest::Response {
    let mut body = extra;
    if !body.is_object() {
        body = json!({});
    }
    body["password"] = json!(PASSWORD);
    call(s, Some(token), method, path, Some(body)).await
}

async fn get_json<T: serde::de::DeserializeOwned>(res: reqwest::Response) -> T {
    let status = res.status();
    let text = res.text().await.unwrap();
    assert_eq!(status, StatusCode::OK, "{text}");
    serde_json::from_str(&text).unwrap()
}

/// What a refused admin call answered: its status and error code.
async fn refusal(res: reqwest::Response) -> (StatusCode, String) {
    let (status, body) = error(res).await;
    (status, body.error)
}

async fn every_admin_route_needs_an_admin(s: &Server) {
    use reqwest::Method as M;
    add_admin(s, "root").await;
    for name in ["ana", "bob", "carl"] {
        add_user(s, name).await;
    }
    s.meta.create_invite("pending", &Code::generate(INVITE_CHARS).unwrap().hash(), craft_cloud_server::state::now_ms(), INVITE_TTL_MS).await.unwrap();
    let (root, member) = (login(s, "root").await, login(s, "ana").await);
    let spare = login(s, "bob").await;
    let spare_id = current_device(s, &spare).await;
    let (ana, bob, carl, pending) = (id_of(s, "ana").await, id_of(s, "bob").await, id_of(s, "carl").await, id_of(s, "pending").await);
    let routes = vec![
        (M::GET, "/admin/server".to_owned(), None, StatusCode::OK),
        (M::GET, "/admin/storage".to_owned(), None, StatusCode::OK),
        (M::GET, "/admin/users".to_owned(), None, StatusCode::OK),
        (M::POST, "/admin/users".to_owned(), Some(json!({ "username": "made", "role": "member" })), StatusCode::OK),
        (M::PATCH, format!("/admin/users/{ana}"), Some(json!({ "quota_bytes": null })), StatusCode::OK),
        (M::GET, format!("/admin/users/{ana}/usage"), None, StatusCode::OK),
        (M::POST, format!("/admin/users/{carl}/disable"), Some(json!({})), StatusCode::NO_CONTENT),
        (M::POST, format!("/admin/users/{carl}/enable"), Some(json!({})), StatusCode::NO_CONTENT),
        (M::POST, format!("/admin/users/{pending}/invite"), Some(json!({})), StatusCode::OK),
        (M::DELETE, format!("/admin/users/{pending}/invite"), Some(json!({})), StatusCode::NO_CONTENT),
        (M::POST, format!("/admin/users/{ana}/password-reset"), Some(json!({})), StatusCode::OK),
        // Ana has no two-factor sign-in: an admin gets through to the change, which has nothing to reset.
        (M::POST, format!("/admin/users/{ana}/reset-two-factor"), Some(json!({})), StatusCode::CONFLICT),
        (M::GET, format!("/admin/users/{ana}/devices"), None, StatusCode::OK),
        (M::DELETE, format!("/admin/users/{bob}/devices/{spare_id}"), Some(json!({})), StatusCode::NO_CONTENT),
        (M::DELETE, format!("/admin/users/{carl}"), Some(json!({})), StatusCode::ACCEPTED),
    ];
    // Every `.route(` of the admin module is in the table (twelve paths): a route added without a row fails here.
    assert_eq!(include_str!("../src/admin.rs").matches(".route(\"/admin/").count(), 12);
    let distinct: std::collections::BTreeSet<_> = routes
        .iter()
        .map(|(_, p, _, _)| {
            p.replace(&ana.to_string(), "{id}")
                .replace(&carl.to_string(), "{id}")
                .replace(&pending.to_string(), "{id}")
                .replace(&bob.to_string(), "{id}")
                .replace(&spare_id.to_string(), "{device_id}")
        })
        .collect();
    assert_eq!(distinct.len(), 12, "{distinct:?}");

    for (method, path, body, ok) in routes {
        let counts = s.meta.admin_server_counts().await.unwrap();
        let with_password = body.clone().map(|mut b| {
            b["password"] = json!(PASSWORD);
            b
        });
        // No token, a token nobody has, a revoked-looking token: unauthorized. A member: forbidden, never data.
        let anonymous = call(s, None, method.clone(), &path, with_password.clone()).await;
        assert_eq!(refusal(anonymous).await, (StatusCode::UNAUTHORIZED, "unauthorized".to_owned()), "{method} {path}");
        let unknown = call(s, Some(&"x".repeat(43)), method.clone(), &path, with_password.clone()).await;
        assert_eq!(refusal(unknown).await, (StatusCode::UNAUTHORIZED, "unauthorized".to_owned()), "{method} {path}");
        let as_member = call(s, Some(&member), method.clone(), &path, with_password.clone()).await;
        assert_eq!(refusal(as_member).await, (StatusCode::FORBIDDEN, "forbidden".to_owned()), "{method} {path}");
        assert_eq!(s.meta.admin_server_counts().await.unwrap(), counts, "{method} {path}: a refused call changed something");
        // An admin gets through.
        let as_admin = call(s, Some(&root), method.clone(), &path, with_password).await;
        assert_eq!(as_admin.status(), ok, "{method} {path}: {}", as_admin.text().await.unwrap());
    }
    // An unknown path under /admin is not found, for everyone; the wrong method on a known one is a JSON 405.
    let (status, code) = refusal(call(s, Some(&root), M::GET, "/admin/nothing", None).await).await;
    assert_eq!((status, code.as_str()), (StatusCode::NOT_FOUND, "not_found"));
    let (status, code) = refusal(call(s, Some(&root), M::PUT, "/admin/users", None).await).await;
    assert_eq!((status, code.as_str()), (StatusCode::METHOD_NOT_ALLOWED, "invalid"));
}

/// The nine changes that need the admin's password, each against its own admin (wrong passwords are counted per
/// account) and its own targets.
async fn every_admin_change_needs_the_admins_password(s: &Server) {
    use reqwest::Method as M;
    let hash = hash_password(PASSWORD).unwrap();
    for kind in 0..9 {
        let admin = format!("adm{kind}");
        s.meta.create_user(&admin, &hash, Role::Admin, 0).await.unwrap();
        let token = login(s, &admin).await;
        let target = format!("target{kind}");
        let passwordless = kind == 5 || kind == 6;
        if passwordless {
            s.meta
                .create_invite(&target, &Code::generate(INVITE_CHARS).unwrap().hash(), craft_cloud_server::state::now_ms(), INVITE_TTL_MS)
                .await
                .unwrap();
        } else {
            add_user(s, &target).await;
        }
        let id = id_of(s, &target).await;
        let device =
            if kind == 8 { Some(s.meta.create_device(craft_cloud_meta::UserId(id), "pc", &[kind as u8 + 1; 32], 0).await.unwrap()) } else { None };
        let (method, path, extra, ok) = match kind {
            0 => (M::POST, "/admin/users".to_owned(), json!({ "username": "made", "role": "member" }), StatusCode::OK),
            1 => (M::PATCH, format!("/admin/users/{id}"), json!({ "role": "member" }), StatusCode::OK),
            2 => (M::POST, format!("/admin/users/{id}/disable"), json!({}), StatusCode::NO_CONTENT),
            3 => (M::POST, format!("/admin/users/{id}/enable"), json!({}), StatusCode::NO_CONTENT),
            4 => (M::DELETE, format!("/admin/users/{id}"), json!({}), StatusCode::ACCEPTED),
            5 => (M::POST, format!("/admin/users/{id}/invite"), json!({}), StatusCode::OK),
            6 => (M::DELETE, format!("/admin/users/{id}/invite"), json!({}), StatusCode::NO_CONTENT),
            7 => (M::POST, format!("/admin/users/{id}/password-reset"), json!({}), StatusCode::OK),
            _ => (M::DELETE, format!("/admin/users/{id}/devices/{}", device.unwrap()), json!({}), StatusCode::NO_CONTENT),
        };
        let at = |body: Option<serde_json::Value>| call(s, Some(&token), method.clone(), &path, body);
        let what = format!("{method} {path}");
        // A missing or malformed confirmation is a 400, whatever else is wrong: never a 401 or 403.
        let mut no_password = extra.clone();
        no_password["unrelated"] = json!(true);
        for bad in [
            None,
            Some(no_password),
            Some(json!([])),
            Some(json!({ "password": 5 })),
            Some(json!({ "password": null })),
            Some(json!({ "password": ["x"] })),
        ] {
            let mut bad = bad;
            // Keep the route's own fields valid so only the confirmation is wrong.
            if let Some(serde_json::Value::Object(map)) = bad.as_mut() {
                for (k, v) in extra.as_object().unwrap() {
                    map.entry(k.clone()).or_insert(v.clone());
                }
            }
            let sent = format!("{bad:?}");
            assert_eq!(refusal(at(bad).await).await, (StatusCode::BAD_REQUEST, "invalid".to_owned()), "{what} with {sent}");
        }
        let raw = s
            .client
            .request(method.clone(), format!("{}{path}", s.base))
            .bearer_auth(&token)
            .header("content-type", "application/json")
            .body("{nope")
            .send()
            .await
            .unwrap();
        assert_eq!(refusal(raw).await, (StatusCode::BAD_REQUEST, "invalid".to_owned()), "{what}: not JSON");
        // A wrong password is a 403 and never a 401 (the agent would take that for a revoked device).
        let mut wrong = extra.clone();
        wrong["password"] = json!("not my password");
        assert_eq!(refusal(at(Some(wrong)).await).await, (StatusCode::FORBIDDEN, "wrong_password".to_owned()), "{what}");
        // A body over 4 KiB is refused before it is read in full.
        let mut big = extra.clone();
        big["password"] = json!("a".repeat(5000));
        assert_eq!(refusal(at(Some(big)).await).await, (StatusCode::PAYLOAD_TOO_LARGE, "too_large".to_owned()), "{what}");
        // A password over the length limit is a 400 and costs no try.
        let mut long = extra.clone();
        long["password"] = json!("a".repeat(2000));
        assert_eq!(refusal(at(Some(long)).await).await, (StatusCode::BAD_REQUEST, "invalid".to_owned()), "{what}");
        // And nothing was done by any of those.
        assert!(s.meta.admin_user(craft_cloud_meta::UserId(id), 0).await.unwrap().is_some_and(|u| u.disabled_ms.is_none()), "{what}");
        // The right password, plus a field the server doesn't know (a later release's `totp_code`): done.
        let mut right = extra.clone();
        right["password"] = json!(PASSWORD);
        right["totp_code"] = json!("123456");
        let res = at(Some(right)).await;
        assert_eq!(res.status(), ok, "{what}: {}", res.text().await.unwrap());
    }
}

async fn wrong_confirmation_passwords_are_limited(s: &Server) {
    use reqwest::Method as M;
    add_admin(s, "root").await;
    add_user(s, "ana").await;
    let root = login(s, "root").await;
    let ana = id_of(s, "ana").await;
    let path = format!("/admin/users/{ana}/enable");
    // Right passwords are given back: many of them are fine.
    for _ in 0..8 {
        assert_eq!(admin_call(s, &root, M::POST, &path, json!({})).await.status(), StatusCode::NO_CONTENT);
    }
    // Five wrong ones are answered 403, the sixth try is held off, even with the right password, and on any admin call.
    for _ in 0..5 {
        let res = call(s, Some(&root), M::POST, &path, Some(json!({ "password": "wrong password!" }))).await;
        assert_eq!(refusal(res).await, (StatusCode::FORBIDDEN, "wrong_password".to_owned()));
    }
    let held = admin_call(s, &root, M::POST, &path, json!({})).await;
    assert_eq!(held.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(error(held).await.1.error, "rate_limited");
    // The account's other uses are not locked out, and another admin is not held.
    assert!(accepted(s, &root).await);
    add_admin(s, "root2").await;
    let root2 = login(s, "root2").await;
    assert_eq!(admin_call(s, &root2, M::POST, &path, json!({})).await.status(), StatusCode::NO_CONTENT);
}

/// Tries refused while the account is at its limit are not counted, so the refund of a try that was counted (and
/// passed) at the same moment gives back exactly its own place: four wrong passwords and a burst of right ones leave
/// room for exactly one more wrong one.
async fn refused_tries_do_not_eat_the_refund_of_a_right_password(s: &Server) {
    use reqwest::Method as M;
    add_admin(s, "root").await;
    add_user(s, "ana").await;
    let root = login(s, "root").await;
    let path = format!("/admin/users/{}/enable", id_of(s, "ana").await);
    let wrong = || call(s, Some(&root), M::POST, &path, Some(json!({ "password": "wrong password!" })));
    for _ in 0..4 {
        assert_eq!(refusal(wrong().await).await, (StatusCode::FORBIDDEN, "wrong_password".to_owned()));
    }
    // Eight right passwords at the same moment: one finds the one place left, the others are refused or follow
    // after it was given back; none leaves a mark.
    let burst = futures_util::future::join_all((0..8).map(|_| admin_call(s, &root, M::POST, &path, json!({})))).await;
    assert!(burst.iter().all(|r| matches!(r.status(), StatusCode::NO_CONTENT | StatusCode::TOO_MANY_REQUESTS)));
    assert!(burst.iter().any(|r| r.status() == StatusCode::NO_CONTENT));
    // Four wrong ones are on record, so one more is answered, and the one after it is held off.
    assert_eq!(refusal(wrong().await).await, (StatusCode::FORBIDDEN, "wrong_password".to_owned()));
    assert_eq!(refusal(wrong().await).await.0, StatusCode::TOO_MANY_REQUESTS);
}

async fn a_demoted_admin_loses_the_admin_routes_at_once(s: &Server) {
    use reqwest::Method as M;
    add_admin(s, "root").await;
    add_admin(s, "deputy").await;
    let (root, deputy) = (login(s, "root").await, login(s, "deputy").await);
    assert_eq!(call(s, Some(&deputy), M::GET, "/admin/server", None).await.status(), StatusCode::OK);
    let id = id_of(s, "deputy").await;
    let res = admin_call(s, &root, M::PATCH, &format!("/admin/users/{id}"), json!({ "role": "member" })).await;
    let user: craft_cloud_proto::api::AdminUserInfo = get_json(res).await;
    assert_eq!(user.role, "member");
    // The same token, the very next request.
    assert_eq!(refusal(call(s, Some(&deputy), M::GET, "/admin/server", None).await).await, (StatusCode::FORBIDDEN, "forbidden".to_owned()));
    assert_eq!(
        refusal(admin_call(s, &deputy, M::POST, &format!("/admin/users/{}/disable", id_of(s, "root").await), json!({})).await).await,
        (StatusCode::FORBIDDEN, "forbidden".to_owned())
    );
    assert!(accepted(s, &deputy).await, "still signed in, as a member");
    // Promoted again: back.
    let res = admin_call(s, &root, M::PATCH, &format!("/admin/users/{id}"), json!({ "role": "admin" })).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(call(s, Some(&deputy), M::GET, "/admin/server", None).await.status(), StatusCode::OK);
}

async fn the_last_admin_stays_and_nobody_disables_themselves(s: &Server) {
    use reqwest::Method as M;
    add_admin(s, "root").await;
    let root = login(s, "root").await;
    let me = id_of(s, "root").await;
    let me_path = format!("/admin/users/{me}");
    let (status, code) = refusal(admin_call(s, &root, M::PATCH, &me_path, json!({ "role": "member" })).await).await;
    assert_eq!((status, code.as_str()), (StatusCode::CONFLICT, "last_admin"));
    for (method, path) in [(M::POST, format!("{me_path}/disable")), (M::DELETE, me_path.clone())] {
        let (status, code) = refusal(admin_call(s, &root, method, &path, json!({})).await).await;
        assert_eq!((status, code.as_str()), (StatusCode::BAD_REQUEST, "invalid"));
    }
    assert!(accepted(s, &root).await);
    // A second admin: now an admin may step down, the other can't be disabled into an empty house.
    add_admin(s, "deputy").await;
    let deputy_id = id_of(s, "deputy").await;
    let deputy = login(s, "deputy").await;
    let res = admin_call(s, &root, M::PATCH, &me_path, json!({ "role": "member" })).await;
    assert_eq!(res.status(), StatusCode::OK);
    let (status, code) = refusal(admin_call(s, &deputy, M::PATCH, &format!("/admin/users/{deputy_id}"), json!({ "role": "member" })).await).await;
    assert_eq!((status, code.as_str()), (StatusCode::CONFLICT, "last_admin"));
    // A disabled admin doesn't count: root, promoted again, disables deputy, and is then the only admin left.
    assert_eq!(admin_call(s, &deputy, M::PATCH, &me_path, json!({ "role": "admin" })).await.status(), StatusCode::OK);
    assert_eq!(admin_call(s, &root, M::POST, &format!("/admin/users/{deputy_id}/disable"), json!({})).await.status(), StatusCode::NO_CONTENT);
    let (status, code) = refusal(admin_call(s, &root, M::PATCH, &me_path, json!({ "role": "member" })).await).await;
    assert_eq!((status, code.as_str()), (StatusCode::CONFLICT, "last_admin"), "a disabled admin is no admin left");
}

async fn a_disabled_account_is_shut_out_everywhere(s: &Server) {
    use reqwest::Method as M;
    add_admin(s, "root").await;
    add_user(s, "ana").await;
    let (root, pc, laptop) = (login(s, "root").await, login(s, "ana").await, login(s, "ana").await);
    let ana = id_of(s, "ana").await;
    let mut ws = live_ready(s, &pc).await;
    let pairing = new_pairing(s, &laptop).await;
    s.meta.create_invite("ana", &Code::generate(INVITE_CHARS).unwrap().hash(), craft_cloud_server::state::now_ms(), INVITE_TTL_MS).await.unwrap();

    let res = admin_call(s, &root, M::POST, &format!("/admin/users/{ana}/disable"), json!({})).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    // The old tokens are plain 401s, and the live connection ends at once.
    for token in [&pc, &laptop] {
        assert_eq!(refusal(call(s, Some(token), M::GET, "/devices", None).await).await, (StatusCode::UNAUTHORIZED, "unauthorized".to_owned()));
        assert_eq!(refusal(call(s, Some(token), M::POST, "/pairings", None).await).await, (StatusCode::UNAUTHORIZED, "unauthorized".to_owned()));
    }
    assert_eq!(error_code(live_next(&mut ws).await), "unauthorized");
    assert_eq!(live_next(&mut ws).await, None, "closed");
    // The right password says why; a wrong one says nothing.
    assert_eq!(refusal(login_with(s, "ana", PASSWORD).await).await, (StatusCode::FORBIDDEN, "account_disabled".to_owned()));
    assert_eq!(refusal(login_with(s, "ana", "not her password").await).await, (StatusCode::UNAUTHORIZED, "unauthorized".to_owned()));
    // Disabling twice is fine; the admin sees it.
    assert_eq!(admin_call(s, &root, M::POST, &format!("/admin/users/{ana}/disable"), json!({})).await.status(), StatusCode::NO_CONTENT);
    let list: craft_cloud_proto::api::AdminUserList = get_json(call(s, Some(&root), M::GET, "/admin/users?query=ana", None).await).await;
    assert!(list.users[0].disabled_ms.is_some() && list.users[0].devices_active == 0);
    // Joining: the pairing is gone, a valid code (the command line makes one) is refused as disabled, a wrong one as ever.
    assert_eq!(refusal(join(s, &pairing.code, "pc", None).await).await, (StatusCode::NOT_FOUND, "invalid_code".to_owned()));
    let invite = Code::generate(INVITE_CHARS).unwrap();
    s.meta.create_invite("ana", &invite.hash(), craft_cloud_server::state::now_ms(), INVITE_TTL_MS).await.unwrap();
    assert_eq!(refusal(join(s, invite.as_str(), "pc", None).await).await, (StatusCode::FORBIDDEN, "account_disabled".to_owned()));
    assert_eq!(refusal(join(s, "AAAA-BBBB-CCCC-DDDD", "pc", None).await).await, (StatusCode::NOT_FOUND, "invalid_code".to_owned()));

    // Enabling: she signs in again; the old tokens stay dead; the code made while she was off works as any would.
    assert_eq!(admin_call(s, &root, M::POST, &format!("/admin/users/{ana}/enable"), json!({})).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(refusal(call(s, Some(&pc), M::GET, "/devices", None).await).await, (StatusCode::UNAUTHORIZED, "unauthorized".to_owned()));
    let fresh = login(s, "ana").await;
    assert!(accepted(s, &fresh).await);
    joined(join(s, invite.as_str(), "again", None).await).await;
}

async fn removing_an_account_signs_it_out_and_keeps_its_name(s: &Server) {
    use reqwest::Method as M;
    add_admin(s, "root").await;
    add_user(s, "ana").await;
    let (root, ana_token) = (login(s, "root").await, login(s, "ana").await);
    let ana = id_of(s, "ana").await;
    let res = admin_call(s, &root, M::DELETE, &format!("/admin/users/{ana}"), json!({})).await;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    assert!(!accepted(s, &ana_token).await);
    let list: craft_cloud_proto::api::AdminUserList = get_json(call(s, Some(&root), M::GET, "/admin/users", None).await).await;
    assert_eq!(list.users.iter().map(|u| u.username.as_str()).collect::<Vec<_>>(), vec!["root"]);
    assert_eq!(refusal(login_with(s, "ana", PASSWORD).await).await, (StatusCode::FORBIDDEN, "account_disabled".to_owned()));
    // It can't be enabled, and its name stays taken.
    let (status, code) = refusal(admin_call(s, &root, M::POST, &format!("/admin/users/{ana}/enable"), json!({})).await).await;
    assert_eq!((status, code.as_str()), (StatusCode::CONFLICT, "conflict"));
    let (status, code) = refusal(admin_call(s, &root, M::POST, "/admin/users", json!({ "username": "Ana", "role": "member" })).await).await;
    assert_eq!((status, code.as_str()), (StatusCode::CONFLICT, "username_taken"));
    assert_eq!(s.meta.admin_server_counts().await.unwrap().users, 1);
}

async fn the_user_list_pages_and_searches(s: &Server) {
    use reqwest::Method as M;
    add_admin(s, "root").await;
    for name in ["ana", "bob", "a_b", "axb", "carol", "dora"] {
        add_user(s, name).await;
    }
    let root = login(s, "root").await;
    let list = |query: &str| {
        let path = format!("/admin/users{query}");
        let root = root.clone();
        async move { call(s, Some(&root), M::GET, &path, None).await }
    };
    let names = |l: &craft_cloud_proto::api::AdminUserList| l.users.iter().map(|u| u.username.clone()).collect::<Vec<_>>();

    // Pages by username, with an opaque cursor, until `next` is gone.
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let q = match &cursor {
            Some(c) => format!("?limit=3&cursor={c}"),
            None => "?limit=3".to_owned(),
        };
        let page: craft_cloud_proto::api::AdminUserList = get_json(list(&q).await).await;
        assert!(page.users.len() <= 3);
        seen.extend(names(&page));
        pages += 1;
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(pages, 3, "seven accounts, three a page");
    let mut sorted_seen = seen.clone();
    sorted_seen.sort();
    assert_eq!(seen, sorted_seen);
    assert_eq!(seen.len(), 7);
    // The default and the most.
    let all: craft_cloud_proto::api::AdminUserList = get_json(list("").await).await;
    assert_eq!((all.users.len(), all.next.clone()), (7, None));
    let max: craft_cloud_proto::api::AdminUserList = get_json(list("?limit=200").await).await;
    assert_eq!(max.users.len(), 7);
    for bad in [
        "?limit=201",
        "?limit=0",
        "?limit=-1",
        "?limit=x",
        "?cursor=!!!",
        "?cursor=bm90IGEgbmFtZQ",
        "?query=a%07b",
        &format!("?query={}", "x".repeat(65)),
    ] {
        assert_eq!(refusal(list(bad).await).await, (StatusCode::BAD_REQUEST, "invalid".to_owned()), "{bad}");
    }
    // A search: a substring, any case, `%` and `_` as themselves, paging as the plain list does.
    let found: craft_cloud_proto::api::AdminUserList = get_json(list("?query=A_B").await).await;
    assert_eq!(names(&found), vec!["a_b"]);
    let found: craft_cloud_proto::api::AdminUserList = get_json(list("?query=a").await).await;
    assert_eq!(names(&found), vec!["a_b", "ana", "axb", "carol", "dora"]);
    let found: craft_cloud_proto::api::AdminUserList = get_json(list("?query=%25").await).await;
    assert!(found.users.is_empty());
    let found: craft_cloud_proto::api::AdminUserList = get_json(list(&format!("?query={}", "x".repeat(64))).await).await;
    assert!(found.users.is_empty());
    let page: craft_cloud_proto::api::AdminUserList = get_json(list("?query=a&limit=2").await).await;
    assert_eq!(names(&page), vec!["a_b", "ana"]);
    let page: craft_cloud_proto::api::AdminUserList = get_json(list(&format!("?query=a&limit=2&cursor={}", page.next.unwrap())).await).await;
    assert_eq!(names(&page), vec!["axb", "carol"]);
    let empty: craft_cloud_proto::api::AdminUserList = get_json(list("?query=").await).await;
    assert_eq!(empty.users.len(), 7, "an empty query is no query");
    // What a row says.
    let root_row = all.users.iter().find(|u| u.username == "root").unwrap();
    assert_eq!((root_row.role.as_str(), root_row.two_factor_enabled, root_row.devices_active, root_row.quota_bytes), ("admin", false, 1, None));
    assert_eq!(root_row.id, id_of(s, "root").await);
}

async fn an_admin_makes_accounts_and_invites(s: &Server) {
    use reqwest::Method as M;
    add_admin(s, "root").await;
    add_user(s, "ana").await;
    let root = login(s, "root").await;
    // A new account: no password, an invite that is shown once, no address without `public_url`.
    let res = admin_call(s, &root, M::POST, "/admin/users", json!({ "username": "Newbie", "role": "admin" })).await;
    let made: craft_cloud_proto::api::AdminCreatedUser = get_json(res).await;
    assert_eq!((made.user.username.as_str(), made.user.role.as_str(), made.user.invite_pending), ("newbie", "admin", true));
    assert_eq!(made.invite.join_url, None);
    assert!(made.invite.expires_ms > craft_cloud_server::state::now_ms());
    // Taken names, in any case, bad names and bad roles.
    for (name, role, want) in [
        ("newbie", "member", (StatusCode::CONFLICT, "username_taken")),
        ("ANA", "member", (StatusCode::CONFLICT, "username_taken")),
        ("bad name", "member", (StatusCode::BAD_REQUEST, "invalid")),
        ("", "member", (StatusCode::BAD_REQUEST, "invalid")),
        ("fine", "owner", (StatusCode::BAD_REQUEST, "invalid")),
        ("fine", "", (StatusCode::BAD_REQUEST, "invalid")),
    ] {
        let (status, code) = refusal(admin_call(s, &root, M::POST, "/admin/users", json!({ "username": name, "role": role })).await).await;
        assert_eq!((status, code.as_str()), want, "{name:?} {role:?}");
    }
    let res = call(s, Some(&root), M::POST, "/admin/users", Some(json!({ "username": "fine" }))).await;
    assert_eq!(refusal(res).await, (StatusCode::BAD_REQUEST, "invalid".to_owned()), "a missing role is a malformed body");
    // The invite chooses the password, and the account is an admin: its token reaches the admin routes.
    let bad = join(s, &made.invite.code, "pc", Some("short")).await;
    assert_eq!(refusal(bad).await, (StatusCode::BAD_REQUEST, "weak_password".to_owned()));
    assert_eq!(refusal(join(s, &made.invite.code, "pc", None).await).await, (StatusCode::BAD_REQUEST, "password_required".to_owned()));
    let newbie = joined(join(s, &made.invite.code, "pc", Some("a password of mine")).await).await;
    assert_eq!(call(s, Some(&newbie.token), M::GET, "/admin/server", None).await.status(), StatusCode::OK);
    assert_eq!(
        refusal(join(s, &made.invite.code, "again", Some("a password of mine")).await).await,
        (StatusCode::NOT_FOUND, "invalid_code".to_owned())
    );

    // An invite for an account that has a password is refused: it would add a computer without the password.
    let ana = id_of(s, "ana").await;
    let (status, code) = refusal(admin_call(s, &root, M::POST, &format!("/admin/users/{ana}/invite"), json!({})).await).await;
    assert_eq!((status, code.as_str()), (StatusCode::CONFLICT, "conflict"));
    // For one without: issued, replaced by the next, revoked, and gone.
    let res = admin_call(s, &root, M::POST, "/admin/users", json!({ "username": "later", "role": "member" })).await;
    let later: craft_cloud_proto::api::AdminCreatedUser = get_json(res).await;
    let later_path = format!("/admin/users/{}/invite", later.user.id);
    let first: craft_cloud_proto::api::AdminCode = get_json(admin_call(s, &root, M::POST, &later_path, json!({})).await).await;
    let second: craft_cloud_proto::api::AdminCode = get_json(admin_call(s, &root, M::POST, &later_path, json!({})).await).await;
    assert_ne!(first.code, second.code);
    assert_eq!(refusal(join(s, &later.invite.code, "pc", Some("a password of mine")).await).await.1, "invalid_code", "the first invite was replaced");
    assert_eq!(refusal(join(s, &first.code, "pc", Some("a password of mine")).await).await.1, "invalid_code");
    assert_eq!(admin_call(s, &root, M::DELETE, &later_path, json!({})).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(refusal(join(s, &second.code, "pc", Some("a password of mine")).await).await.1, "invalid_code", "revoked");
    let (status, code) = refusal(admin_call(s, &root, M::DELETE, &later_path, json!({})).await).await;
    assert_eq!((status, code.as_str()), (StatusCode::NOT_FOUND, "not_found"), "nothing left to revoke");
    let third: craft_cloud_proto::api::AdminCode = get_json(admin_call(s, &root, M::POST, &later_path, json!({})).await).await;
    let joined_later = joined(join(s, &third.code, "pc", Some("a password of mine")).await).await;
    assert!(accepted(s, &joined_later.token).await);
}

async fn a_password_reset_code_signs_the_redeeming_computer_in(s: &Server) {
    use reqwest::Method as M;
    add_admin(s, "root").await;
    add_user(s, "ana").await;
    let (root, pc, laptop) = (login(s, "root").await, login(s, "ana").await, login(s, "ana").await);
    let ana = id_of(s, "ana").await;
    let mut ws = live_ready(s, &pc).await;
    let pairing = new_pairing(s, &laptop).await;
    let path = format!("/admin/users/{ana}/password-reset");
    // Making a code changes nothing yet.
    let reset: craft_cloud_proto::api::AdminCode = get_json(admin_call(s, &root, M::POST, &path, json!({ "reset_two_factor": false })).await).await;
    let now = craft_cloud_server::state::now_ms();
    assert!((now + 59 * 60_000..=now + 61 * 60_000).contains(&reset.expires_ms), "an hour: {}", reset.expires_ms - now);
    assert!(accepted(s, &pc).await && accepted(s, &laptop).await);
    assert_eq!(login_with(s, "ana", PASSWORD).await.status(), StatusCode::OK);

    // It always asks for a new password, though ana has one; a weak one is refused, and the code stays.
    assert_eq!(refusal(join(s, &reset.code, "recovery", None).await).await, (StatusCode::BAD_REQUEST, "password_required".to_owned()));
    assert_eq!(refusal(join(s, &reset.code, "recovery", Some("short")).await).await, (StatusCode::BAD_REQUEST, "weak_password".to_owned()));
    let new = "a password for after";
    let recovered = joined(join(s, &reset.code, "recovery", Some(new)).await).await;
    // Everything else is signed out, the live connection ends at once, the pairing is gone, and this computer is in.
    assert!(!accepted(s, &pc).await && !accepted(s, &laptop).await);
    assert_eq!(error_code(live_next(&mut ws).await), "unauthorized");
    assert_eq!(live_next(&mut ws).await, None, "closed");
    assert_eq!(refusal(join(s, &pairing.code, "late", None).await).await.1, "invalid_code");
    assert!(accepted(s, &recovered.token).await);
    assert_eq!(login_with(s, "ana", PASSWORD).await.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(login_with(s, "ana", new).await.status(), StatusCode::OK);
    let devices = devices_of(s, &recovered.token).await;
    assert_eq!(
        devices.iter().filter(|d| d.revoked && d.revoke_reason.as_deref() == Some("reset")).count(),
        3,
        "the two signed in and the one the password check made"
    );
    // Once.
    assert_eq!(refusal(join(s, &reset.code, "again", Some(new)).await).await.1, "invalid_code");

    // Not for an admin (no second factor exists to ask of the acting admin), not for oneself, not for nobody.
    add_admin(s, "boss").await;
    let boss = id_of(s, "boss").await;
    let (status, code) = refusal(admin_call(s, &root, M::POST, &format!("/admin/users/{boss}/password-reset"), json!({})).await).await;
    assert_eq!((status, code.as_str()), (StatusCode::FORBIDDEN, "forbidden"));
    let me = id_of(s, "root").await;
    let (status, code) = refusal(admin_call(s, &root, M::POST, &format!("/admin/users/{me}/password-reset"), json!({})).await).await;
    assert_eq!((status, code.as_str()), (StatusCode::BAD_REQUEST, "invalid"));
    let nobody = uuid::Uuid::now_v7();
    let (status, code) = refusal(admin_call(s, &root, M::POST, &format!("/admin/users/{nobody}/password-reset"), json!({})).await).await;
    assert_eq!((status, code.as_str()), (StatusCode::NOT_FOUND, "not_found"));
}

async fn the_server_overview_shows_counters_only(s: &Server) {
    use reqwest::Method as M;
    add_admin(s, "root").await;
    add_user(s, "ana").await;
    let (root, ana) = (login(s, "root").await, login(s, "ana").await);
    assert_eq!(put_chunk(s, &ana, &[3u8; 100]).await, StatusCode::NO_CONTENT);
    let res = call(s, Some(&root), M::GET, "/admin/server", None).await;
    assert_eq!(res.status(), StatusCode::OK);
    let text = res.text().await.unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    let keys: std::collections::BTreeSet<_> = json.as_object().unwrap().keys().cloned().collect();
    let expected: std::collections::BTreeSet<_> =
        ["version", "users", "devices_active", "stored_bytes", "live_bytes", "storage_kind", "database_kind"].into_iter().map(String::from).collect();
    assert_eq!(keys, expected, "{text}");
    assert_eq!(
        (json["users"].as_u64(), json["devices_active"].as_u64(), json["stored_bytes"].as_u64(), json["live_bytes"].as_u64()),
        (Some(2), Some(2), Some(101), Some(0))
    );
    assert_eq!(json["storage_kind"], "disk");
    assert_eq!(json["database_kind"], s.meta.database_kind());
    assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
    // Nothing that names a place on the machine or the network.
    let blobs = s._dirs.last().unwrap().path().display().to_string();
    for forbidden in [blobs.as_str(), "meta.db", "postgres://", "http://", "127.0.0.1"] {
        assert!(!text.contains(forbidden), "{forbidden} in {text}");
    }
}

async fn an_admin_lists_and_revokes_a_users_devices_over_http(s: &Server) {
    use reqwest::Method as M;
    add_admin(s, "root").await;
    add_user(s, "ana").await;
    add_user(s, "bob").await;
    let (root, pc, laptop, bob) = (login(s, "root").await, login(s, "ana").await, login(s, "ana").await, login(s, "bob").await);
    let ana_id = id_of(s, "ana").await;
    let (pc_id, bob_device) = (current_device(s, &pc).await, current_device(s, &bob).await);
    let mut ws = live_ready(s, &pc).await;
    assert_eq!(
        s.client.delete(format!("{}/devices/{}", s.base, current_device(s, &laptop).await)).bearer_auth(&pc).send().await.unwrap().status(),
        StatusCode::NO_CONTENT
    );

    // All of them, the revoked one with its reason.
    let listed: craft_cloud_proto::api::AdminDevices =
        get_json(call(s, Some(&root), M::GET, &format!("/admin/users/{ana_id}/devices"), None).await).await;
    assert_eq!(listed.devices.len(), 2);
    assert_eq!(listed.devices.iter().filter(|d| d.revoked && d.revoke_reason.as_deref() == Some("user")).count(), 1);
    assert!(listed.devices.iter().all(|d| !d.current), "none of them is the admin's own");
    // Revoking one: the token is dead at once, the live connection ends, the reason says an admin did it.
    let path = format!("/admin/users/{ana_id}/devices/{pc_id}");
    assert_eq!(admin_call(s, &root, M::DELETE, &path, json!({})).await.status(), StatusCode::NO_CONTENT);
    assert!(!accepted(s, &pc).await);
    assert_eq!(error_code(live_next(&mut ws).await), "unauthorized");
    let listed: craft_cloud_proto::api::AdminDevices =
        get_json(call(s, Some(&root), M::GET, &format!("/admin/users/{ana_id}/devices"), None).await).await;
    assert_eq!(listed.devices.iter().find(|d| d.id == pc_id).unwrap().revoke_reason.as_deref(), Some("admin"));
    assert_eq!(admin_call(s, &root, M::DELETE, &path, json!({})).await.status(), StatusCode::NO_CONTENT, "again is fine");
    // Another user's device, an unknown device, an unknown user: not found. Bad IDs: invalid.
    let wrong_owner = format!("/admin/users/{ana_id}/devices/{bob_device}");
    assert_eq!(refusal(admin_call(s, &root, M::DELETE, &wrong_owner, json!({})).await).await, (StatusCode::NOT_FOUND, "not_found".to_owned()));
    assert!(accepted(s, &bob).await);
    let unknown_device = format!("/admin/users/{ana_id}/devices/{}", uuid::Uuid::now_v7());
    assert_eq!(refusal(admin_call(s, &root, M::DELETE, &unknown_device, json!({})).await).await.1, "not_found");
    let unknown_user = format!("/admin/users/{}/devices", uuid::Uuid::now_v7());
    assert_eq!(refusal(call(s, Some(&root), M::GET, &unknown_user, None).await).await.1, "not_found");
    for bad in ["not-an-id", "123", "00000000-0000-0000-0000-00000000000g"] {
        for (method, path) in [
            (M::GET, format!("/admin/users/{bad}/devices")),
            (M::DELETE, format!("/admin/users/{ana_id}/devices/{bad}")),
            (M::PATCH, format!("/admin/users/{bad}")),
            (M::POST, format!("/admin/users/{bad}/disable")),
            (M::POST, format!("/admin/users/{bad}/enable")),
            (M::DELETE, format!("/admin/users/{bad}")),
            (M::POST, format!("/admin/users/{bad}/invite")),
            (M::DELETE, format!("/admin/users/{bad}/invite")),
            (M::POST, format!("/admin/users/{bad}/password-reset")),
        ] {
            let res = admin_call(s, &root, method.clone(), &path, json!({ "role": "member" })).await;
            assert_eq!(refusal(res).await, (StatusCode::BAD_REQUEST, "invalid".to_owned()), "{method} {path}");
        }
    }
    // The device of bob is untouched.
    assert!(devices_of(s, &bob).await.iter().all(|d| !d.revoked));
}

async fn an_admin_sets_quotas_that_hold_before_any_byte_is_stored(s: &Server) {
    use reqwest::Method as M;
    add_admin(s, "root").await;
    add_user(s, "ana").await;
    let (root, ana) = (login(s, "root").await, login(s, "ana").await);
    let ana_id = id_of(s, "ana").await;
    let path = format!("/admin/users/{ana_id}");
    let blobs = s._dirs.last().unwrap().path().to_owned();
    let chunk = vec![7u8; 4 * 1024 * 1024];
    // No quota and no cap on this server: nothing stops a chunk.
    // Bounds: a missing field changes nothing, an empty body is invalid, null removes, a number from one chunk up.
    for bad in [
        json!({}),
        json!({ "quota_bytes": 0 }),
        json!({ "quota_bytes": 4_194_303 }),
        json!({ "quota_bytes": -5 }),
        json!({ "quota_bytes": "9" }),
        json!({ "quota_bytes": 1.5 }),
        json!({ "role": "owner" }),
    ] {
        let sent = bad.to_string();
        assert_eq!(refusal(admin_call(s, &root, M::PATCH, &path, bad).await).await, (StatusCode::BAD_REQUEST, "invalid".to_owned()), "{sent}");
    }
    let set: craft_cloud_proto::api::AdminUserInfo = get_json(admin_call(s, &root, M::PATCH, &path, json!({ "quota_bytes": 4_194_304 })).await).await;
    assert_eq!(set.quota_bytes, Some(4_194_304));
    let same: craft_cloud_proto::api::AdminUserInfo = get_json(admin_call(s, &root, M::PATCH, &path, json!({ "role": "member" })).await).await;
    assert_eq!(same.quota_bytes, Some(4_194_304), "a missing quota_bytes leaves it");
    let huge: craft_cloud_proto::api::AdminUserInfo = get_json(admin_call(s, &root, M::PATCH, &path, json!({ "quota_bytes": i64::MAX })).await).await;
    assert_eq!(huge.quota_bytes, Some(i64::MAX as u64));
    assert_eq!(refusal(admin_call(s, &root, M::PATCH, &path, json!({ "quota_bytes": u64::MAX })).await).await.0, StatusCode::BAD_REQUEST);
    get_json::<craft_cloud_proto::api::AdminUserInfo>(admin_call(s, &root, M::PATCH, &path, json!({ "quota_bytes": 4_194_304 })).await).await;

    // A chunk the quota can't hold is refused in the look before storing: nothing reaches the disk.
    let res = s.client.put(format!("{}/chunks/{}", s.base, chunk_id(&chunk))).bearer_auth(&ana).body(chunk.clone()).send().await.unwrap();
    let (status, body) = error(res).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::INSUFFICIENT_STORAGE, "quota_exceeded"));
    assert_eq!(body.quota, Some(craft_cloud_proto::api::QuotaUse { used: 0, limit: 4_194_304 }));
    assert_eq!(walk(&blobs), 0, "nothing was stored");
    assert_eq!(put_chunk(s, &ana, &[1u8; 1000]).await, StatusCode::NO_CONTENT);
    // `null` removes it: the same chunk goes in.
    let cleared: craft_cloud_proto::api::AdminUserInfo = get_json(admin_call(s, &root, M::PATCH, &path, json!({ "quota_bytes": null })).await).await;
    assert_eq!(cleared.quota_bytes, None);
    let res = s.client.put(format!("{}/chunks/{}", s.base, chunk_id(&chunk))).bearer_auth(&ana).body(chunk).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
}

/// A server that knows its public address.
async fn sqlite_disk_with_public_url(public_url: &str) -> Server {
    let db = tempfile::tempdir().unwrap();
    let blobs = tempfile::tempdir().unwrap();
    let meta = Meta::open(&format!("sqlite:{}", db.path().join("meta.db").display())).await.unwrap();
    let store = Store::new(DiskBackend::new(blobs.path()).unwrap());
    let state = AppState::new(meta.clone(), store, vec![]).unwrap().with_public_url(Some(public_url.to_owned()));
    start_state(state, meta, vec![db, blobs], None).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admin_invites_carry_the_join_link_when_the_server_knows_its_address() {
    use reqwest::Method as M;
    let s = sqlite_disk_with_public_url("https://cloud.example.org").await;
    add_admin(&s, "root").await;
    let root = login(&s, "root").await;
    let made: craft_cloud_proto::api::AdminCreatedUser =
        get_json(admin_call(&s, &root, M::POST, "/admin/users", json!({ "username": "newbie", "role": "member" })).await).await;
    let link = made.invite.join_url.unwrap();
    let parsed = craft_cloud_proto::join::JoinString::parse(&link).unwrap();
    assert_eq!(parsed.server, "https://cloud.example.org");
    assert_eq!(parsed.code.as_str(), made.invite.code);
    assert!(link.starts_with("https://cloud.example.org/join/"), "{link}");
    // The code in the link is the one that works.
    joined(join(&s, &made.invite.code, "pc", Some("a password of mine")).await).await;
}

// ---- two-factor sign-in ----

use craft_cloud_proto::api::{AdminCode, RecoveryCodes, TwoFactorSetupResponse, TwoFactorStatus};
use craft_cloud_server::secretbox::{SecretBox, TwoFactorKey};
use craft_cloud_server::totp;

fn test_box() -> SecretBox {
    SecretBox::new(&TwoFactorKey::from_bytes([42; 32]), None).unwrap()
}

impl Server {
    /// The server's time of day now, in milliseconds.
    fn at(&self) -> i64 {
        match self.clock.load(std::sync::atomic::Ordering::SeqCst) {
            0 => craft_cloud_server::state::now_ms(),
            frozen => frozen,
        }
    }

    /// Stops the server's clock at the start of the current TOTP step (plus a second), so that codes don't change under a test.
    fn freeze(&self) {
        if self.clock.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            let step = totp::step_at(craft_cloud_server::state::now_ms());
            self.clock.store(step * 30_000 + 1_000, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Moves the frozen clock on.
    fn advance(&self, ms: i64) {
        let now = self.at();
        self.clock.store(now + ms, std::sync::atomic::Ordering::SeqCst);
    }
}

/// A different six digits from `code`.
fn wrong_code(code: &str) -> String {
    format!("{:06}", (code.parse::<u32>().unwrap() + 1) % 1_000_000)
}

/// An account with two-factor sign-in on: the token of the device that turned it on, its secret and recovery codes.
struct Tf {
    token: String,
    secret: Vec<u8>,
    recovery: Vec<String>,
}

impl Tf {
    /// The code for the server's current time step.
    fn code(&self, s: &Server) -> String {
        totp::code_for(&self.secret, totp::step_at(s.at())).unwrap()
    }

    /// The server's clock moves one step on, and the code for that step: never one that was used.
    fn fresh_code(&self, s: &Server) -> String {
        s.advance(30_000);
        self.code(s)
    }
}

async fn post_json(s: &Server, token: &str, path: &str, body: serde_json::Value) -> reqwest::Response {
    call(s, Some(token), reqwest::Method::POST, path, Some(body)).await
}

/// Makes `name` (an admin if `admin`) and turns two-factor sign-in on from one device, with the clock frozen: the first
/// code is the step the clock is in. Every device but the one that did it is signed out.
async fn turn_on_two_factor(s: &Server, name: &str, admin: bool) -> Tf {
    s.freeze();
    if admin {
        add_admin(s, name).await;
    } else {
        add_user(s, name).await;
    }
    let token = login(s, name).await;
    let setup: TwoFactorSetupResponse = get_json(post_json(s, &token, "/account/two-factor/setup", json!({ "password": PASSWORD })).await).await;
    let secret = totp::base32_decode(&setup.secret).unwrap();
    let tf = Tf { token, secret, recovery: Vec::new() };
    let res = post_json(s, &tf.token, "/account/two-factor/confirm", json!({ "totp_code": tf.code(s) })).await;
    let codes: RecoveryCodes = get_json(res).await;
    Tf { recovery: codes.recovery_codes, ..tf }
}

/// `POST /auth/login` for `name` with the right password, answered `two_factor_required`: the challenge.
async fn login_challenge(s: &Server, name: &str) -> craft_cloud_proto::api::TwoFactorChallenge {
    let (status, body) = error(login_with(s, name, PASSWORD).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::UNAUTHORIZED, "two_factor_required"), "{}", body.message);
    body.two_factor.expect("a challenge")
}

async fn second_step(s: &Server, challenge: &str, proof: serde_json::Value) -> reqwest::Response {
    let mut body = json!({ "challenge": challenge });
    if let (Some(extra), Some(object)) = (proof.as_object(), body.as_object_mut()) {
        object.extend(extra.clone());
    }
    call(s, None, reqwest::Method::POST, "/auth/login/second-factor", Some(body)).await
}

async fn retry_after(res: &reqwest::Response) -> Option<String> {
    res.headers().get("retry-after").and_then(|v| v.to_str().ok()).map(str::to_owned)
}

async fn status_of(s: &Server, token: &str) -> TwoFactorStatus {
    get_json(call(s, Some(token), reqwest::Method::GET, "/account/two-factor", None).await).await
}

async fn two_factor_is_set_up_confirmed_and_turned_off(s: &Server) {
    s.freeze();
    add_user(s, "ana").await;
    let (pc, laptop) = (login(s, "ana").await, login(s, "ana").await);
    assert_eq!(status_of(s, &pc).await, TwoFactorStatus { enabled: false, enabled_ms: None, recovery_codes_left: 0, pending: false });
    let account: serde_json::Value = get_json(call(s, Some(&pc), reqwest::Method::GET, "/account", None).await).await;
    assert_eq!(account["two_factor_enabled"], json!(false));

    // The password is asked again, and a wrong one is 403, never 401.
    let (status, body) = error(post_json(s, &pc, "/account/two-factor/setup", json!({ "password": "wrong password!" })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::FORBIDDEN, "wrong_password"));
    let first: TwoFactorSetupResponse = get_json(post_json(s, &pc, "/account/two-factor/setup", json!({ "password": PASSWORD })).await).await;
    assert_eq!(first.expires_ms, s.at() + 10 * 60_000);
    assert_eq!(first.secret.len(), 32);
    assert_eq!(totp::base32_decode(&first.secret).map(|s| s.len()), Some(20));
    assert_eq!(
        first.otpauth_uri,
        format!("otpauth://totp/Craft%20Cloud:ana?secret={}&issuer=Craft%20Cloud&algorithm=SHA1&digits=6&period=30", first.secret)
    );
    let status = status_of(s, &pc).await;
    assert_eq!((status.enabled, status.pending), (false, true));
    // The same device may start again; that replaces the secret.
    let second: TwoFactorSetupResponse = get_json(post_json(s, &pc, "/account/two-factor/setup", json!({ "password": PASSWORD })).await).await;
    assert_ne!(first.secret, second.secret);
    let secret = totp::base32_decode(&second.secret).unwrap();
    let right = totp::code_for(&secret, totp::step_at(s.at())).unwrap();

    // A code that isn't six digits is 400 (nothing counted), a wrong one 403.
    for bad in ["12345", "1234567", "12345a", ""] {
        let (status, body) = error(post_json(s, &pc, "/account/two-factor/confirm", json!({ "totp_code": bad })).await).await;
        assert_eq!((status, body.error.as_str()), (StatusCode::BAD_REQUEST, "invalid"), "{bad:?}");
    }
    let (status, body) = error(post_json(s, &pc, "/account/two-factor/confirm", json!({ "totp_code": wrong_code(&right) })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::FORBIDDEN, "two_factor_invalid"));
    assert_eq!(post_json(s, &pc, "/account/two-factor/confirm", json!({})).await.status(), StatusCode::BAD_REQUEST);

    let codes: RecoveryCodes = get_json(post_json(s, &pc, "/account/two-factor/confirm", json!({ "totp_code": right })).await).await;
    assert_eq!(codes.recovery_codes.len(), 10);
    let unique: std::collections::BTreeSet<_> = codes.recovery_codes.iter().collect();
    assert_eq!(unique.len(), 10);
    for code in &codes.recovery_codes {
        assert_eq!(code.len(), 19, "{code}: four groups of four");
        assert!(code.split('-').all(|g| g.len() == 4 && g.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_uppercase())), "{code}");
    }
    // The other device is signed out, with the reason; this one stays.
    assert!(!accepted(s, &laptop).await);
    let devices = devices_of(s, &pc).await;
    let out = devices.iter().find(|d| d.revoked).unwrap();
    assert_eq!(out.revoke_reason.as_deref(), Some("two_factor"));
    assert!(devices.iter().find(|d| d.current).is_some_and(|d| !d.revoked));
    let status = status_of(s, &pc).await;
    assert_eq!((status.enabled, status.enabled_ms.is_some(), status.recovery_codes_left, status.pending), (true, true, 10, false));
    let account: serde_json::Value = get_json(call(s, Some(&pc), reqwest::Method::GET, "/account", None).await).await;
    assert_eq!(account["two_factor_enabled"], json!(true));
    // On already: a new set-up and another confirmation are conflicts.
    let (status, body) = error(post_json(s, &pc, "/account/two-factor/setup", json!({ "password": PASSWORD })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::CONFLICT, "conflict"));
    let (status, body) = error(
        post_json(s, &pc, "/account/two-factor/confirm", json!({ "totp_code": totp::code_for(&secret, totp::step_at(s.at()) + 1).unwrap() })).await,
    )
    .await;
    assert_eq!((status, body.error.as_str()), (StatusCode::CONFLICT, "conflict"));

    // New recovery codes: the password and a code from the app (a step after the one that was used); the old ones die.
    let next = |offset: i64| totp::code_for(&secret, totp::step_at(s.at()) + offset).unwrap();
    let (status, body) =
        error(post_json(s, &pc, "/account/two-factor/recovery-codes", json!({ "password": "wrong password!", "totp_code": next(1) })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::FORBIDDEN, "wrong_password"));
    let (status, body) = error(post_json(s, &pc, "/account/two-factor/recovery-codes", json!({ "password": PASSWORD })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::BAD_REQUEST, "invalid"));
    let body = json!({ "password": PASSWORD, "recovery_code": codes.recovery_codes[0] });
    assert_eq!(post_json(s, &pc, "/account/two-factor/recovery-codes", body).await.status(), StatusCode::BAD_REQUEST, "a code from the app");
    let (status, body) =
        error(post_json(s, &pc, "/account/two-factor/recovery-codes", json!({ "password": PASSWORD, "totp_code": wrong_code(&next(1)) })).await)
            .await;
    assert_eq!((status, body.error.as_str()), (StatusCode::FORBIDDEN, "two_factor_invalid"));
    let (status, body) =
        error(post_json(s, &pc, "/account/two-factor/recovery-codes", json!({ "password": PASSWORD, "totp_code": next(0) })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::FORBIDDEN, "two_factor_invalid"), "the step that turned it on is used");
    let fresh: RecoveryCodes =
        get_json(post_json(s, &pc, "/account/two-factor/recovery-codes", json!({ "password": PASSWORD, "totp_code": next(1) })).await).await;
    assert_eq!(fresh.recovery_codes.len(), 10);
    assert!(fresh.recovery_codes.iter().all(|c| !codes.recovery_codes.contains(c)));
    assert_eq!(status_of(s, &pc).await.recovery_codes_left, 10);

    // Off: the password and a code or a recovery code; the old recovery codes are dead.
    let (status, body) = error(
        post_json(s, &pc, "/account/two-factor/disable", json!({ "password": "wrong password!", "recovery_code": fresh.recovery_codes[0] })).await,
    )
    .await;
    assert_eq!((status, body.error.as_str()), (StatusCode::FORBIDDEN, "wrong_password"));
    let (status, body) =
        error(post_json(s, &pc, "/account/two-factor/disable", json!({ "password": PASSWORD, "recovery_code": codes.recovery_codes[0] })).await)
            .await;
    assert_eq!((status, body.error.as_str()), (StatusCode::FORBIDDEN, "two_factor_invalid"), "a code of the first set");
    let (status, body) = error(post_json(s, &pc, "/account/two-factor/disable", json!({ "password": PASSWORD })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::BAD_REQUEST, "invalid"));
    let (status, body) =
        error(post_json(s, &pc, "/account/two-factor/disable", json!({ "password": PASSWORD, "recovery_code": "nonsense" })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::BAD_REQUEST, "invalid"));
    let pc2 = {
        // A second device, signed in before the turn-off through a code of its own: it is signed out by it.
        let challenge = login_challenge(s, "ana").await;
        let res = second_step(s, &challenge.challenge, json!({ "recovery_code": fresh.recovery_codes[1].to_lowercase().replace('-', "") })).await;
        res.json::<LoginResponse>().await.unwrap().token
    };
    assert!(accepted(s, &pc2).await);
    let res = post_json(s, &pc, "/account/two-factor/disable", json!({ "password": PASSWORD, "recovery_code": fresh.recovery_codes[0] })).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    assert!(!accepted(s, &pc2).await, "the other device is signed out");
    assert_eq!(status_of(s, &pc).await, TwoFactorStatus { enabled: false, enabled_ms: None, recovery_codes_left: 0, pending: false });
    // Off again: a password sign-in gives a token at once.
    login(s, "ana").await;
    let (status, body) = error(post_json(s, &pc, "/account/two-factor/disable", json!({ "password": PASSWORD, "totp_code": next(2) })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::CONFLICT, "conflict"));
}

async fn a_set_up_on_one_device_is_pending_for_the_others(s: &Server) {
    s.freeze();
    add_user(s, "ana").await;
    let (pc, laptop) = (login(s, "ana").await, login(s, "ana").await);
    let setup: TwoFactorSetupResponse = get_json(post_json(s, &pc, "/account/two-factor/setup", json!({ "password": PASSWORD })).await).await;
    let secret = totp::base32_decode(&setup.secret).unwrap();
    let right = totp::code_for(&secret, totp::step_at(s.at())).unwrap();
    // The laptop may neither start another set-up nor confirm this one, and its code is not tried (no wrong try is used up).
    let (status, body) = error(post_json(s, &laptop, "/account/two-factor/setup", json!({ "password": PASSWORD })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::CONFLICT, "two_factor_pending"));
    for _ in 0..7 {
        let (status, body) = error(post_json(s, &laptop, "/account/two-factor/confirm", json!({ "totp_code": right })).await).await;
        assert_eq!((status, body.error.as_str()), (StatusCode::CONFLICT, "conflict"));
    }
    assert!(status_of(s, &laptop).await.pending);
    // Five wrong codes from the device that started it use the set-up up.
    for _ in 0..5 {
        let (status, body) = error(post_json(s, &pc, "/account/two-factor/confirm", json!({ "totp_code": wrong_code(&right) })).await).await;
        assert_eq!((status, body.error.as_str()), (StatusCode::FORBIDDEN, "two_factor_invalid"));
    }
    assert!(!status_of(s, &pc).await.pending);
    let (status, body) = error(post_json(s, &pc, "/account/two-factor/confirm", json!({ "totp_code": right })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::CONFLICT, "conflict"), "start again");
    // After ten minutes an open set-up is not in the way of another device.
    post_json(s, &pc, "/account/two-factor/setup", json!({ "password": PASSWORD })).await;
    s.advance(10 * 60_000 + 1);
    assert!(!status_of(s, &pc).await.pending);
    let again: TwoFactorSetupResponse = get_json(post_json(s, &laptop, "/account/two-factor/setup", json!({ "password": PASSWORD })).await).await;
    let secret = totp::base32_decode(&again.secret).unwrap();
    let code = totp::code_for(&secret, totp::step_at(s.at())).unwrap();
    assert_eq!(post_json(s, &laptop, "/account/two-factor/confirm", json!({ "totp_code": code })).await.status(), StatusCode::OK);
    assert!(!accepted(s, &pc).await);
}

async fn the_two_factor_routes_refuse_a_signed_out_device_and_big_or_odd_bodies(s: &Server) {
    let ana = turn_on_two_factor(s, "ana", false).await;
    let other = {
        let challenge = login_challenge(s, "ana").await;
        let res = second_step(s, &challenge.challenge, json!({ "totp_code": ana.fresh_code(s) })).await;
        res.json::<LoginResponse>().await.unwrap().token
    };
    let id = current_device(s, &other).await;
    assert_eq!(s.client.delete(format!("{}/devices/{id}", s.base)).bearer_auth(&ana.token).send().await.unwrap().status(), StatusCode::NO_CONTENT);
    // A device that was signed out is 401 on each of them, as on every route; nothing else is.
    for (path, body) in [
        ("/account/two-factor/setup", json!({ "password": PASSWORD })),
        ("/account/two-factor/confirm", json!({ "totp_code": "123456" })),
        ("/account/two-factor/disable", json!({ "password": PASSWORD, "totp_code": "123456" })),
        ("/account/two-factor/recovery-codes", json!({ "password": PASSWORD, "totp_code": "123456" })),
    ] {
        let (status, body) = error(post_json(s, &other, path, body).await).await;
        assert_eq!((status, body.error.as_str()), (StatusCode::UNAUTHORIZED, "unauthorized"), "{path}");
        assert_eq!(post_json(s, "x", path, json!({})).await.status(), StatusCode::UNAUTHORIZED, "{path}: no token");
        // Bodies are tiny: over 4 KiB is refused before it is read.
        let big = json!({ "password": "x".repeat(5000), "totp_code": "123456" });
        let (status, body) = error(post_json(s, &ana.token, path, big).await).await;
        assert_eq!((status, body.error.as_str()), (StatusCode::PAYLOAD_TOO_LARGE, "too_large"), "{path}");
        // Not JSON, or the wrong shape, is 400 `invalid`.
        let res = s.client.post(format!("{}{path}", s.base)).bearer_auth(&ana.token).body("not json").send().await.unwrap();
        let (status, body) = error(res).await;
        assert_eq!((status, body.error.as_str()), (StatusCode::BAD_REQUEST, "invalid"), "{path}");
        let (status, body) = error(post_json(s, &ana.token, path, json!({ "password": 7 })).await).await;
        assert_eq!((status, body.error.as_str()), (StatusCode::BAD_REQUEST, "invalid"), "{path}");
    }
    let big = json!({ "challenge": "x".repeat(5000), "totp_code": "123456" });
    let (status, _) = error(call(s, None, reqwest::Method::POST, "/auth/login/second-factor", Some(big)).await).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    // A long password is refused without a check (400), not as a wrong one.
    let (status, body) =
        error(post_json(s, &ana.token, "/account/two-factor/disable", json!({ "password": "x".repeat(2000), "totp_code": "123456" })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::BAD_REQUEST, "invalid"));
}

async fn a_sign_in_with_two_factor_needs_the_second_step(s: &Server) {
    let ana = turn_on_two_factor(s, "ana", false).await;
    // A wrong password and an unknown user: `unauthorized`, with nothing said about two-factor sign-in.
    for (name, password) in [("ana", "wrong password!"), ("nobody", PASSWORD)] {
        let (status, body) = error(login_with(s, name, password).await).await;
        assert_eq!((status, body.error.as_str()), (StatusCode::UNAUTHORIZED, "unauthorized"));
        assert!(body.two_factor.is_none());
    }
    // The right password: a challenge, 401, no token, no device.
    let devices = devices_of(s, &ana.token).await.len();
    let res = call(
        s,
        None,
        reqwest::Method::POST,
        "/auth/login",
        Some(json!({ "username": "ana", "password": PASSWORD, "device_name": "work laptop", "client_kind": "desktop", "platform": "windows", "app_version": "1.0" })),
    )
    .await;
    let (status, body) = error(res).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::UNAUTHORIZED, "two_factor_required"));
    let challenge = body.two_factor.unwrap();
    assert_eq!(challenge.challenge.len(), 43);
    assert_eq!(challenge.expires_ms, s.at() + 5 * 60_000);
    assert_eq!(devices_of(s, &ana.token).await.len(), devices, "no device without the second factor");

    // Not a code, both, neither, a made-up challenge: nothing is spent.
    let right = ana.fresh_code(s);
    for (proof, expected) in [
        (json!({ "totp_code": "12345" }), (StatusCode::BAD_REQUEST, "invalid")),
        (json!({ "totp_code": right, "recovery_code": ana.recovery[0] }), (StatusCode::BAD_REQUEST, "invalid")),
        (json!({}), (StatusCode::BAD_REQUEST, "invalid")),
        (json!({ "recovery_code": "zzz" }), (StatusCode::BAD_REQUEST, "invalid")),
        (json!({ "totp_code": wrong_code(&right) }), (StatusCode::UNAUTHORIZED, "two_factor_invalid")),
    ] {
        let (status, body) = error(second_step(s, &challenge.challenge, proof.clone()).await).await;
        assert_eq!((status, body.error.as_str()), expected, "{proof}");
    }
    let (status, body) = error(second_step(s, &"A".repeat(43), json!({ "totp_code": right })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::UNAUTHORIZED, "unauthorized"));
    let (status, body) = error(second_step(s, "short", json!({ "totp_code": right })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::UNAUTHORIZED, "unauthorized"));

    // The right code: a device, as a sign-in makes one, with what the first call said.
    let res = second_step(s, &challenge.challenge, json!({ "totp_code": right })).await;
    assert_eq!(res.status(), StatusCode::OK);
    let token = res.json::<LoginResponse>().await.unwrap().token;
    assert!(accepted(s, &token).await);
    let made = devices_of(s, &token).await.into_iter().find(|d| d.current).unwrap();
    assert_eq!(
        (made.name.as_str(), made.client_kind.as_str(), made.platform.as_str(), made.app_version.as_str()),
        ("work laptop", "desktop", "windows", "1.0")
    );
    // Once: the challenge is used up.
    let (status, body) = error(second_step(s, &challenge.challenge, json!({ "totp_code": ana.fresh_code(s) })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::UNAUTHORIZED, "unauthorized"));
    // And the code is: on a new challenge it is `two_factor_invalid`, in the step after it works.
    let again = login_challenge(s, "ana").await;
    let (status, body) = error(second_step(s, &again.challenge, json!({ "totp_code": right })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::UNAUTHORIZED, "two_factor_invalid"));
    assert_eq!(second_step(s, &again.challenge, json!({ "totp_code": ana.fresh_code(s) })).await.status(), StatusCode::OK);
    // A recovery code, in any case and with or without dashes, once.
    let third = login_challenge(s, "ana").await;
    let typed = ana.recovery[0].to_lowercase().replace('-', " ");
    assert_eq!(second_step(s, &third.challenge, json!({ "recovery_code": typed })).await.status(), StatusCode::OK);
    let fourth = login_challenge(s, "ana").await;
    let (status, body) = error(second_step(s, &fourth.challenge, json!({ "recovery_code": ana.recovery[0] })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::UNAUTHORIZED, "two_factor_invalid"), "a recovery code works once");
    assert_eq!(status_of(s, &ana.token).await.recovery_codes_left, 9);
}

async fn a_challenge_expires_takes_five_tries_and_dies_with_a_password_change(s: &Server) {
    let ana = turn_on_two_factor(s, "ana", false).await;
    // Five minutes.
    let challenge = login_challenge(s, "ana").await;
    s.advance(5 * 60_000 + 1);
    let (status, body) = error(second_step(s, &challenge.challenge, json!({ "totp_code": ana.code(s) })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::UNAUTHORIZED, "unauthorized"));
    // Five wrong codes delete it; the right one is then too late.
    let challenge = login_challenge(s, "ana").await;
    let right = ana.fresh_code(s);
    for n in 1..=5 {
        let (status, body) = error(second_step(s, &challenge.challenge, json!({ "totp_code": wrong_code(&right) })).await).await;
        assert_eq!((status, body.error.as_str()), (StatusCode::UNAUTHORIZED, "two_factor_invalid"), "try {n}");
    }
    let (status, body) = error(second_step(s, &challenge.challenge, json!({ "totp_code": right })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::UNAUTHORIZED, "unauthorized"));
    // A challenge made before a password change gives no token afterwards (the change deletes it).
    let challenge = login_challenge(s, "ana").await;
    assert_eq!(change_password(s, &ana.token, PASSWORD, "a brand new password").await.status(), StatusCode::NO_CONTENT);
    let (status, body) = error(second_step(s, &challenge.challenge, json!({ "totp_code": ana.fresh_code(s) })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::UNAUTHORIZED, "unauthorized"));
    assert_eq!(devices_of(s, &ana.token).await.iter().filter(|d| !d.revoked).count(), 1, "no device was made");
}

async fn wrong_codes_lock_the_second_factor_for_a_while(s: &Server) {
    let ana = turn_on_two_factor(s, "ana", false).await;
    let right = ana.fresh_code(s);
    // Ten wrong codes on a route that takes the password and a code (the right password is not counted against the account).
    for n in 1..=10 {
        let (status, body) =
            error(post_json(s, &ana.token, "/account/two-factor/disable", json!({ "password": PASSWORD, "totp_code": wrong_code(&right) })).await)
                .await;
        assert_eq!((status, body.error.as_str()), (StatusCode::FORBIDDEN, "two_factor_invalid"), "wrong code {n}");
    }
    // Locked: 429 with Retry-After, on every path that checks a second factor, for the right code too.
    let res = post_json(s, &ana.token, "/account/two-factor/disable", json!({ "password": PASSWORD, "totp_code": right })).await;
    assert_eq!(retry_after(&res).await.as_deref(), Some("900"));
    let (status, body) = error(res).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::TOO_MANY_REQUESTS, "two_factor_locked"));
    let res = post_json(s, &ana.token, "/account/two-factor/recovery-codes", json!({ "password": PASSWORD, "totp_code": right })).await;
    assert_eq!((res.status(), retry_after(&res).await.as_deref()), (StatusCode::TOO_MANY_REQUESTS, Some("900")));
    let challenge = login_challenge(s, "ana").await;
    let res = second_step(s, &challenge.challenge, json!({ "recovery_code": ana.recovery[0] })).await;
    assert_eq!((res.status(), retry_after(&res).await.as_deref()), (StatusCode::TOO_MANY_REQUESTS, Some("900")));
    assert_eq!(error(second_step(s, &challenge.challenge, json!({ "totp_code": right })).await).await.1.error, "two_factor_locked");
    // The lock runs down with time (and counts down in what it says), and the next right code works and resets it.
    s.advance(60_000);
    let res = post_json(s, &ana.token, "/account/two-factor/disable", json!({ "password": PASSWORD, "totp_code": right })).await;
    assert_eq!(retry_after(&res).await.as_deref(), Some("840"));
    s.advance(14 * 60_000);
    let challenge = login_challenge(s, "ana").await;
    let res = second_step(s, &challenge.challenge, json!({ "totp_code": ana.code(s) })).await;
    assert_eq!(res.status(), StatusCode::OK, "{:?}", res.text().await);
    // The success reset the counters: one wrong code now is only one.
    let right = ana.fresh_code(s);
    let (status, _) =
        error(post_json(s, &ana.token, "/account/two-factor/disable", json!({ "password": PASSWORD, "totp_code": wrong_code(&right) })).await).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a success reset the counters: one wrong code is only one");
}

/// Ten wrong codes at the second step (in two challenges, as one takes five) lock the account; and a wrong code is also
/// counted against the address, which holds off at 30 however many accounts they are spread over.
async fn wrong_second_steps_are_counted_and_limited(s: &Server) {
    // Everything that signs in comes first: wrong codes count against the sign-in limiter as well.
    let ana = turn_on_two_factor(s, "ana", false).await;
    let ana_challenges = [login_challenge(s, "ana").await, login_challenge(s, "ana").await, login_challenge(s, "ana").await];
    let mut bobs = Vec::new();
    for n in 0..4 {
        let name = format!("bob{n}");
        let bob = turn_on_two_factor(s, &name, false).await;
        let challenges = [login_challenge(s, &name).await, login_challenge(s, &name).await];
        bobs.push((bob, challenges));
    }
    let right = ana.fresh_code(s);
    for challenge in &ana_challenges[..2] {
        for _ in 0..5 {
            let (status, body) = error(second_step(s, &challenge.challenge, json!({ "totp_code": wrong_code(&right) })).await).await;
            assert_eq!((status, body.error.as_str()), (StatusCode::UNAUTHORIZED, "two_factor_invalid"));
        }
    }
    // Ten wrong ones filled the account's window and locked it: said as a lock, with the time.
    let res = second_step(s, &ana_challenges[2].challenge, json!({ "totp_code": right })).await;
    assert_eq!((res.status(), retry_after(&res).await.as_deref()), (StatusCode::TOO_MANY_REQUESTS, Some("900")));
    assert_eq!(error(res).await.1.error, "two_factor_locked");

    // Four more accounts, 8 wrong codes each: the address has counted 10, then 8 and 8, and the 21st of these is the 31st.
    let mut answers = Vec::new();
    for (bob, challenges) in &bobs {
        let right = bob.fresh_code(s);
        for i in 0..8 {
            let challenge = &challenges[usize::from(i >= 5)];
            let (status, body) = error(second_step(s, &challenge.challenge, json!({ "totp_code": wrong_code(&right) })).await).await;
            answers.push((answers.len() + 1, status, body.error));
        }
    }
    let first_limited = answers.iter().find(|(_, _, e)| e == "rate_limited").map(|(n, ..)| *n);
    assert_eq!(first_limited, Some(21), "{answers:?}");
    assert!(answers.iter().take(20).all(|(_, status, e)| *status == StatusCode::UNAUTHORIZED && e == "two_factor_invalid"), "{answers:?}");
    assert!(answers.iter().skip(20).all(|(_, status, e)| *status == StatusCode::TOO_MANY_REQUESTS && e == "rate_limited"), "{answers:?}");
}

async fn joining_an_account_with_two_factor_needs_the_factor(s: &Server) {
    let ana = turn_on_two_factor(s, "ana", false).await;
    let invite = Code::generate(INVITE_CHARS).unwrap();
    s.meta.create_invite("ana", &invite.hash(), s.at(), INVITE_TTL_MS).await.unwrap();
    let join_with = |extra: serde_json::Value| {
        let mut body = json!({ "code": invite.as_str(), "device_name": "new pc" });
        body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        call(s, None, reqwest::Method::POST, "/join", Some(body))
    };
    // No second factor: `two_factor_required`, no challenge row (the field is empty), and the invite is not used up.
    let (status, body) = error(join_with(json!({})).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::UNAUTHORIZED, "two_factor_required"));
    assert_eq!(body.two_factor.map(|t| t.challenge), Some(String::new()));
    // Malformed ones are 400 and cost nothing.
    assert_eq!(join_with(json!({ "totp_code": "12" })).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(join_with(json!({ "totp_code": "123456", "recovery_code": ana.recovery[0] })).await.status(), StatusCode::BAD_REQUEST);
    // A wrong code is `two_factor_invalid`, 401, and the invite stays.
    let right = ana.fresh_code(s);
    let (status, body) = error(join_with(json!({ "totp_code": wrong_code(&right) })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::UNAUTHORIZED, "two_factor_invalid"));
    let (status, body) = error(join_with(json!({ "recovery_code": "0000-0000-0000-0000" })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::UNAUTHORIZED, "two_factor_invalid"));
    assert_eq!(s.meta.invite_needs_password(&invite.hash(), s.at()).await.unwrap(), Some(false), "the invite is still there");
    // The right code: a device, and the invite is used up with it.
    let device = joined(join_with(json!({ "totp_code": right })).await).await;
    assert!(accepted(s, &device.token).await);
    let (status, body) = error(join_with(json!({ "totp_code": ana.fresh_code(s) })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::NOT_FOUND, "invalid_code"));
    // A recovery code does it too.
    let invite2 = Code::generate(INVITE_CHARS).unwrap();
    s.meta.create_invite("ana", &invite2.hash(), s.at(), INVITE_TTL_MS).await.unwrap();
    let body = json!({ "code": invite2.as_str(), "device_name": "second", "recovery_code": ana.recovery[3].to_lowercase() });
    assert_eq!(call(s, None, reqwest::Method::POST, "/join", Some(body)).await.status(), StatusCode::OK);
    // A pairing code needs none: it comes from a device that is signed in.
    let pairing = new_pairing(s, &ana.token).await;
    joined(join(s, &pairing.code, "paired", None).await).await;
}

/// A wrong second factor at `/join` is not given back to the address's tries, and counts against the account.
async fn wrong_second_factors_at_join_count_against_the_address_and_the_account(s: &Server) {
    let ana = turn_on_two_factor(s, "ana", false).await;
    let invite = Code::generate(INVITE_CHARS).unwrap();
    s.meta.create_invite("ana", &invite.hash(), s.at(), INVITE_TTL_MS).await.unwrap();
    let right = ana.fresh_code(s);
    let join_with = |code: String| {
        let body = json!({ "code": invite.as_str(), "device_name": "pc", "totp_code": code });
        call(s, None, reqwest::Method::POST, "/join", Some(body))
    };
    // Ten wrong codes in a row: the tenth locks the account's second factor, whatever route they came by.
    for n in 1..=10 {
        let (status, body) = error(join_with(wrong_code(&right)).await).await;
        assert_eq!((status, body.error.as_str()), (StatusCode::UNAUTHORIZED, "two_factor_invalid"), "wrong code {n}");
    }
    // They were counted against the address (none was given back): the limiter refuses now, and says the same to everyone, a
    // made-up code, the real invite of the locked account and the right code alike, with the time until its window ends.
    let bogus = Code::generate(PAIRING_CHARS).unwrap();
    let refusals = [
        call(s, None, reqwest::Method::POST, "/join", Some(json!({ "code": bogus.as_str(), "device_name": "pc" }))).await,
        join_with(right.clone()).await,
        join_with(wrong_code(&right)).await,
    ];
    let mut seen = Vec::new();
    for res in refusals {
        let wait: u32 = retry_after(&res).await.expect("Retry-After").parse().unwrap();
        let (status, body) = error(res).await;
        seen.push((status, body.error, body.message, body.two_factor.is_none(), wait));
    }
    assert_eq!((seen[0].0, seen[0].1.as_str()), (StatusCode::TOO_MANY_REQUESTS, "rate_limited"), "{seen:?}");
    for other in &seen[1..] {
        assert_eq!((&other.0, &other.1, &other.2, other.3), (&seen[0].0, &seen[0].1, &seen[0].2, seen[0].3), "identical answers: {seen:?}");
        assert!(other.4.abs_diff(seen[0].4) <= 1 && (1..=61).contains(&other.4), "the same window: {seen:?}");
    }
    // And against the account: the lock holds on the route that takes the password and a code, with the time left.
    let res = post_json(s, &ana.token, "/account/two-factor/disable", json!({ "password": PASSWORD, "totp_code": right })).await;
    assert_eq!((res.status(), retry_after(&res).await.as_deref()), (StatusCode::TOO_MANY_REQUESTS, Some("900")));
    assert_eq!(error(res).await.1.error, "two_factor_locked");
    assert_eq!(s.meta.invite_needs_password(&invite.hash(), s.at()).await.unwrap(), Some(false), "the invite survived all of it");
}

async fn a_reset_code_for_a_protected_account(s: &Server) {
    let ana = turn_on_two_factor(s, "ana", false).await;
    add_admin(s, "root").await;
    let root = login(s, "root").await;
    let ana_id = id_of(s, "ana").await;
    // Without `reset_two_factor` the account's second factor is needed to redeem the code.
    let reset: AdminCode =
        get_json(admin_call(s, &root, reqwest::Method::POST, &format!("/admin/users/{ana_id}/password-reset"), json!({})).await).await;
    let body = json!({ "code": reset.code, "device_name": "recovery", "password": "a brand new password" });
    let (status, answer) = error(call(s, None, reqwest::Method::POST, "/join", Some(body.clone())).await).await;
    assert_eq!((status, answer.error.as_str()), (StatusCode::UNAUTHORIZED, "two_factor_required"));
    login_challenge(s, "ana").await; // the old password still stands: nothing changed
    let mut with_code = body.clone();
    with_code["totp_code"] = json!(wrong_code(&ana.fresh_code(s)));
    let (status, answer) = error(call(s, None, reqwest::Method::POST, "/join", Some(with_code)).await).await;
    assert_eq!((status, answer.error.as_str()), (StatusCode::UNAUTHORIZED, "two_factor_invalid"));
    let mut with_code = body.clone();
    with_code["totp_code"] = json!(ana.fresh_code(s));
    let first = joined(call(s, None, reqwest::Method::POST, "/join", Some(with_code)).await).await;
    assert!(!accepted(s, &ana.token).await, "every device was signed out");
    assert!(accepted(s, &first.token).await);
    assert!(status_of(s, &first.token).await.enabled, "it did not turn two-factor sign-in off");
    let (status, answer) = error(login_with(s, "ana", "a brand new password").await).await;
    assert_eq!(
        (status, answer.error.as_str()),
        (StatusCode::UNAUTHORIZED, "two_factor_required"),
        "the new password still asks for the second factor"
    );

    // With `reset_two_factor` the code takes it away: no second factor, and the account is plain again.
    let reset: AdminCode = get_json(
        admin_call(s, &root, reqwest::Method::POST, &format!("/admin/users/{ana_id}/password-reset"), json!({ "reset_two_factor": true })).await,
    )
    .await;
    let body = json!({ "code": reset.code, "device_name": "recovery 2", "password": "another new password" });
    let second = joined(call(s, None, reqwest::Method::POST, "/join", Some(body)).await).await;
    assert!(!accepted(s, &first.token).await);
    assert_eq!(status_of(s, &second.token).await, TwoFactorStatus { enabled: false, enabled_ms: None, recovery_codes_left: 0, pending: false });
    assert_eq!(login_with(s, "ana", "another new password").await.status(), StatusCode::OK);
}

/// Turning two-factor sign-in on signs the account's other devices out at once: their live socket closes and their waiting
/// change-feed long-poll is answered 401 with no changes.
async fn turning_two_factor_on_ends_live_connections_and_long_polls(s: &Server) {
    s.freeze();
    add_user(s, "ana").await;
    let (pc, laptop) = (login(s, "ana").await, login(s, "ana").await);
    let mut ws = live_ready(s, &laptop).await;
    let since = space_of(s, &laptop).await.seq;
    let poll = {
        let (client, url, token) = (s.client.clone(), format!("{}/changes?since={since}&wait=30", s.base), laptop.clone());
        tokio::spawn(async move { client.get(url).bearer_auth(token).send().await.unwrap() })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!poll.is_finished());
    let setup: TwoFactorSetupResponse = get_json(post_json(s, &pc, "/account/two-factor/setup", json!({ "password": PASSWORD })).await).await;
    let secret = totp::base32_decode(&setup.secret).unwrap();
    let code = totp::code_for(&secret, totp::step_at(s.at())).unwrap();
    assert_eq!(post_json(s, &pc, "/account/two-factor/confirm", json!({ "totp_code": code })).await.status(), StatusCode::OK);
    assert_eq!(error_code(live_next(&mut ws).await), "unauthorized");
    assert_eq!(live_next(&mut ws).await, None, "closed");
    let res = tokio::time::timeout(Duration::from_secs(10), poll).await.expect("the long-poll ends at once").unwrap();
    let (status, body) = error(res).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::UNAUTHORIZED, "unauthorized"));
    assert_eq!(open_live(s, Some(&laptop)).await.err(), Some(StatusCode::UNAUTHORIZED));
    assert!(accepted(s, &pc).await);
}

/// The admin API asks an admin with two-factor sign-in for a code as well as the password, and a time step serves one change.
async fn an_admin_with_two_factor_confirms_changes_with_a_code(s: &Server) {
    let root = turn_on_two_factor(s, "root", true).await;
    add_user(s, "ana").await;
    let ana_id = id_of(s, "ana").await;
    let path = format!("/admin/users/{ana_id}/password-reset");
    let call_with = |extra: serde_json::Value| admin_call(s, &root.token, reqwest::Method::POST, &path, extra);
    // Missing: 403 `two_factor_invalid` (the field is a credential here, not a form field); malformed: 400; wrong: 403.
    let (status, body) = error(call_with(json!({})).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::FORBIDDEN, "two_factor_invalid"));
    let right = root.fresh_code(s);
    let (status, body) = error(call_with(json!({ "totp_code": "12" })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::BAD_REQUEST, "invalid"));
    let (status, body) = error(call_with(json!({ "totp_code": wrong_code(&right) })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::FORBIDDEN, "two_factor_invalid"));
    // A wrong password is told first, whatever the code.
    let body = json!({ "password": "wrong password!", "totp_code": right });
    let (status, body) = error(call(s, Some(&root.token), reqwest::Method::POST, &path, Some(body)).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::FORBIDDEN, "wrong_password"));
    // The right code works once: the same step again is refused, the next one goes through.
    assert_eq!(call_with(json!({ "totp_code": right })).await.status(), StatusCode::OK);
    let (status, body) = error(call_with(json!({ "totp_code": right })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::FORBIDDEN, "two_factor_invalid"), "the same time step twice");
    assert_eq!(call_with(json!({ "totp_code": root.fresh_code(s) })).await.status(), StatusCode::OK);
    // The step sign-in used is spent for the admin too (they share it).
    let challenge = login_challenge(s, "root").await;
    let used = root.fresh_code(s);
    assert_eq!(second_step(s, &challenge.challenge, json!({ "totp_code": used })).await.status(), StatusCode::OK);
    let (status, body) = error(call_with(json!({ "totp_code": used })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::FORBIDDEN, "two_factor_invalid"));
    // A recovery code stands in for the code, once.
    assert_eq!(call_with(json!({ "recovery_code": root.recovery[0] })).await.status(), StatusCode::OK);
    let (status, body) = error(call_with(json!({ "recovery_code": root.recovery[0] })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::FORBIDDEN, "two_factor_invalid"));
    // The list shows who has it.
    let list: craft_cloud_proto::api::AdminUserList = get_json(call(s, Some(&root.token), reqwest::Method::GET, "/admin/users", None).await).await;
    let flags: Vec<_> = list.users.iter().map(|u| (u.username.as_str(), u.two_factor_enabled)).collect();
    assert_eq!(flags, vec![("ana", false), ("root", true)]);
    // Ten wrong codes in a row (a right one in between starts the count again) lock it, for the admin API as for every
    // other path: 429 with Retry-After, whatever the code.
    let boss = turn_on_two_factor(s, "boss", true).await;
    let boss_call = |extra: serde_json::Value| admin_call(s, &boss.token, reqwest::Method::POST, &path, extra);
    let right = boss.fresh_code(s);
    for n in 1..=10 {
        let (status, body) = error(boss_call(json!({ "totp_code": wrong_code(&right) })).await).await;
        assert_eq!((status, body.error.as_str()), (StatusCode::FORBIDDEN, "two_factor_invalid"), "wrong code {n}");
    }
    let res = boss_call(json!({ "totp_code": right })).await;
    assert_eq!((res.status(), retry_after(&res).await.as_deref()), (StatusCode::TOO_MANY_REQUESTS, Some("900")));
    assert_eq!(error(res).await.1.error, "two_factor_locked");
    // The command line lifts the lock.
    s.meta.reset_two_factor(s.meta.user_by_name("boss").await.unwrap().unwrap().id, s.at()).await.unwrap();
    let again = login(s, "boss").await;
    let res = admin_call(s, &again, reqwest::Method::POST, &path, json!({})).await;
    assert_eq!(res.status(), StatusCode::OK, "two-factor sign-in is off for boss, so the password alone does it");
}

async fn an_admin_resets_the_two_factor_of_an_account(s: &Server) {
    let root = turn_on_two_factor(s, "root", true).await;
    let ana = turn_on_two_factor(s, "ana", false).await;
    let ana_id = id_of(s, "ana").await;
    add_admin(s, "boss").await;
    let boss = login(s, "boss").await;
    let (root_id, boss_id) = (id_of(s, "root").await, id_of(s, "boss").await);
    // An admin without two-factor sign-in may not reset another admin's password or second factor.
    for path in [format!("/admin/users/{root_id}/password-reset"), format!("/admin/users/{root_id}/reset-two-factor")] {
        let (status, body) = error(admin_call(s, &boss, reqwest::Method::POST, &path, json!({})).await).await;
        assert_eq!((status, body.error.as_str()), (StatusCode::FORBIDDEN, "forbidden"), "{path}");
    }
    // The admin who has it may reset a member's: the account is plain again and every device of it is signed out.
    let path = format!("/admin/users/{ana_id}/reset-two-factor");
    let (status, body) = error(admin_call(s, &root.token, reqwest::Method::POST, &path, json!({})).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::FORBIDDEN, "two_factor_invalid"), "the admin's own code is needed");
    let res = admin_call(s, &root.token, reqwest::Method::POST, &path, json!({ "totp_code": root.fresh_code(s) })).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    assert!(!accepted(s, &ana.token).await);
    let token = login(s, "ana").await;
    assert!(!status_of(s, &token).await.enabled);
    // Nothing is left to reset; one's own account is not reset this way.
    let (status, body) = error(admin_call(s, &root.token, reqwest::Method::POST, &path, json!({ "recovery_code": root.recovery[0] })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::CONFLICT, "conflict"));
    let own = format!("/admin/users/{root_id}/reset-two-factor");
    let (status, body) = error(admin_call(s, &root.token, reqwest::Method::POST, &own, json!({ "recovery_code": root.recovery[1] })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::BAD_REQUEST, "invalid"));
    // Another admin's password: allowed to an admin who has it.
    let path = format!("/admin/users/{boss_id}/password-reset");
    let res = admin_call(s, &root.token, reqwest::Method::POST, &path, json!({ "recovery_code": root.recovery[2] })).await;
    assert_eq!(res.status(), StatusCode::OK);
    // …and so is another admin's second factor, once they have one.
    let (status, body) = error(
        admin_call(
            s,
            &root.token,
            reqwest::Method::POST,
            &format!("/admin/users/{boss_id}/reset-two-factor"),
            json!({ "recovery_code": root.recovery[3] }),
        )
        .await,
    )
    .await;
    assert_eq!((status, body.error.as_str()), (StatusCode::CONFLICT, "conflict"), "boss has none");
}

// ---- two-factor sign-in: one server each ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn setting_it_up_needs_a_key_the_server_never_makes() {
    let db = tempfile::tempdir().unwrap();
    let blobs = tempfile::tempdir().unwrap();
    let meta = Meta::open(&format!("sqlite:{}", db.path().join("meta.db").display())).await.unwrap();
    let store = Store::new(DiskBackend::new(blobs.path()).unwrap());
    // No key: the state as the server's `main` builds it for a config without `[two_factor]`.
    let s = start_state(AppState::new(meta.clone(), store, vec![]).unwrap(), meta, vec![db, blobs], None).await;
    add_user(&s, "ana").await;
    let token = login(&s, "ana").await;
    let (status, body) = error(post_json(&s, &token, "/account/two-factor/setup", json!({ "password": PASSWORD })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::SERVICE_UNAVAILABLE, "two_factor_unavailable"));
    // Unavailable is said before the password is looked at: a wrong one is not counted either.
    for _ in 0..8 {
        let (status, body) = error(post_json(&s, &token, "/account/two-factor/setup", json!({ "password": "wrong password!" })).await).await;
        assert_eq!((status, body.error.as_str()), (StatusCode::SERVICE_UNAVAILABLE, "two_factor_unavailable"));
    }
    assert_eq!(status_of(&s, &token).await, TwoFactorStatus { enabled: false, enabled_ms: None, recovery_codes_left: 0, pending: false });
    assert_eq!(
        change_password(&s, &token, PASSWORD, "a brand new password").await.status(),
        StatusCode::NO_CONTENT,
        "the password's tries are untouched"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_challenge_works_from_another_address() {
    let db = tempfile::tempdir().unwrap();
    let blobs = tempfile::tempdir().unwrap();
    let meta = Meta::open(&format!("sqlite:{}", db.path().join("meta.db").display())).await.unwrap();
    let store = Store::new(DiskBackend::new(blobs.path()).unwrap());
    let s = start_trusting(meta, store, vec![db, blobs], vec!["127.0.0.1".parse().unwrap()]).await;
    let ana = turn_on_two_factor(&s, "ana", false).await;
    let from = |addr: &'static str, body: serde_json::Value, path: &'static str| {
        let req = s.client.post(format!("{}{path}", s.base)).header("x-forwarded-for", addr).json(&body);
        async move { req.send().await.unwrap() }
    };
    let res = from("198.51.100.1", json!({ "username": "ana", "password": PASSWORD, "device_name": "pc" }), "/auth/login").await;
    let challenge = error(res).await.1.two_factor.unwrap();
    // The secret token binds the challenge, not the address: another one finishes it.
    let res = from("203.0.113.9", json!({ "challenge": challenge.challenge, "totp_code": ana.fresh_code(&s) }), "/auth/login/second-factor").await;
    assert_eq!(res.status(), StatusCode::OK);
}

// ---- history and restore ----

fn update(node: NodeId, expected: VersionId, manifest: ManifestId) -> Op {
    Op::Update { node, expected, version: NewVersion { id: VersionId::generate(), manifest, mtime_ms: 7, exec: false, save: None } }
}

async fn history_page(s: &Server, token: &str, node: NodeId, query: &str) -> reqwest::Response {
    s.client.get(format!("{}/nodes/{node}/history{query}", s.base)).bearer_auth(token).send().await.unwrap()
}

async fn restore_request(s: &Server, token: &str, node: NodeId, body: serde_json::Value) -> reqwest::Response {
    s.client.post(format!("{}/nodes/{node}/restore", s.base)).bearer_auth(token).json(&body).send().await.unwrap()
}

/// A file of `contents.len()` versions, one per entry: its node, the version IDs and manifests, oldest first.
async fn file_with_history(s: &Server, token: &str, contents: &[&[u8]]) -> (NodeId, Vec<VersionId>, Vec<ManifestId>) {
    let r = root(s, token).await;
    let node = NodeId::generate();
    let mut manifests = Vec::new();
    for data in contents {
        manifests.push(upload(s, token, data).await);
    }
    let first = create(node, r, &format!("history-{node}.txt"), manifests[0]);
    let Op::Create { version, .. } = &first else { panic!("a create") };
    let mut versions = vec![version.id];
    assert_eq!(commit(s, token, &[first]).await.status(), StatusCode::OK);
    for manifest in &manifests[1..] {
        let op = update(node, *versions.last().unwrap(), *manifest);
        let Op::Update { version, .. } = &op else { panic!("an update") };
        versions.push(version.id);
        assert_eq!(commit(s, token, &[op]).await.status(), StatusCode::OK);
    }
    (node, versions, manifests)
}

async fn the_history_of_a_file_pages_newest_first_for_its_owner_only(s: &Server) {
    use craft_cloud_proto::api::HistoryResponse;
    add_user(s, "ana").await;
    add_user(s, "bob").await;
    let (ana, bob) = (login(s, "ana").await, login(s, "bob").await);
    let tag = uuid::Uuid::now_v7();
    let contents: Vec<Vec<u8>> = (0..5).map(|i| format!("history {tag} {i}").into_bytes()).collect();
    let refs: Vec<&[u8]> = contents.iter().map(Vec::as_slice).collect();
    let (node, versions, _) = file_with_history(s, &ana, &refs).await;

    let all: HistoryResponse = history_page(s, &ana, node, "").await.json().await.unwrap();
    assert_eq!(all.next, None);
    assert_eq!(all.versions.iter().map(|v| v.id).collect::<Vec<_>>(), versions.iter().rev().copied().collect::<Vec<_>>());
    assert!(all.versions.iter().all(|v| !v.pruned && !v.save && v.size == contents[0].len() as u64));
    assert_eq!(all.versions[4].parent_version, None);
    assert_eq!(all.versions[0].parent_version, Some(versions[3]));
    assert_eq!(all.versions.iter().map(|v| v.seq).collect::<Vec<_>>(), vec![5, 4, 3, 2, 1]);

    // Pages of two follow each other through `next`.
    let (mut seen, mut query, mut pages) = (Vec::new(), "?limit=2".to_owned(), 0);
    loop {
        let page: HistoryResponse = history_page(s, &ana, node, &query).await.json().await.unwrap();
        seen.extend(page.versions.iter().map(|v| v.id));
        pages += 1;
        match page.next {
            Some(next) => query = format!("?limit=2&cursor={next}"),
            None => break,
        }
    }
    assert_eq!((pages, seen), (3, all.versions.iter().map(|v| v.id).collect::<Vec<_>>()));

    // Bad paging is `invalid`.
    for bad in ["?limit=0", "?limit=201", "?limit=-1", "?cursor=nonsense", "?cursor=1", "?cursor=%201.00000000-0000-0000-0000-000000000000"] {
        let (status, body) = error(history_page(s, &ana, node, bad).await).await;
        assert_eq!((status, body.error.as_str()), (StatusCode::BAD_REQUEST, "invalid"), "{bad}");
    }
    assert_eq!(history_page(s, &ana, node, "?limit=200").await.status(), StatusCode::OK);

    // Other accounts, unknown nodes and folders; a missing or wrong token.
    let (status, body) = error(history_page(s, &bob, node, "").await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::NOT_FOUND, "not_found"), "another account's file");
    let (status, body) = error(history_page(s, &ana, NodeId::generate(), "").await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::NOT_FOUND, "not_found"));
    let (status, body) = error(history_page(s, &ana, root(s, &ana).await, "").await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::BAD_REQUEST, "invalid"), "a folder");
    assert_eq!(history_page(s, "not-a-token", node, "").await.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(s.client.get(format!("{}/nodes/{node}/history", s.base)).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
    let (status, body) = error(s.client.get(format!("{}/nodes/not-a-uuid/history", s.base)).bearer_auth(&ana).send().await.unwrap()).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::BAD_REQUEST, "invalid"));

    // A deleted file keeps its history.
    let del = Op::Delete { node, expected: Some(*versions.last().unwrap()) };
    assert_eq!(commit(s, &ana, &[del]).await.status(), StatusCode::OK);
    let kept: HistoryResponse = history_page(s, &ana, node, "").await.json().await.unwrap();
    assert_eq!(kept.versions.len(), 5);
}

async fn restoring_an_old_version_makes_a_new_one(s: &Server) {
    use craft_cloud_proto::api::{HistoryResponse, RestoreResponse};
    add_user(s, "ana").await;
    add_user(s, "bob").await;
    let (ana, bob) = (login(s, "ana").await, login(s, "bob").await);
    let tag = uuid::Uuid::now_v7();
    let (first, second, third) = (format!("first {tag}").into_bytes(), format!("second {tag}").into_bytes(), format!("third {tag}").into_bytes());
    let (node, versions, _) = file_with_history(s, &ana, &[&first, &second, &third]).await;
    let before: SpaceResponse = space_of(s, &ana).await;

    // Another account can neither read nor restore it.
    let body = json!({ "version": versions[0], "expected_version": versions[2] });
    let (status, error_body) = error(restore_request(s, &bob, node, body.clone()).await).await;
    assert_eq!((status, error_body.error.as_str()), (StatusCode::NOT_FOUND, "not_found"));
    assert_eq!(restore_request(s, "not-a-token", node, body.clone()).await.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(space_of(s, &ana).await.seq, before.seq, "refused restores change nothing");

    let res = restore_request(s, &ana, node, body.clone()).await;
    assert_eq!(res.status(), StatusCode::OK);
    let done: RestoreResponse = res.json().await.unwrap();
    assert_eq!((done.parent_version, done.seq, done.epoch), (versions[2], before.seq + 1, before.epoch));
    assert!(!versions.contains(&done.version));

    // It is an ordinary change: in the feed, with the old content, and the old versions stay.
    let feed: ChangeSet =
        s.client.get(format!("{}/changes?since={}", s.base, before.seq)).bearer_auth(&ana).send().await.unwrap().json().await.unwrap();
    let node_state = feed.nodes.iter().find(|n| n.id == node).unwrap();
    let NodeKind::File(state) = &node_state.kind else { panic!("a file") };
    assert_eq!((state.version, state.save.clone(), state.size), (done.version, None, first.len() as u64));
    assert_eq!(download(s, &ana, &state.manifest).await, first);
    let history: HistoryResponse = history_page(s, &ana, node, "").await.json().await.unwrap();
    assert_eq!(history.versions.len(), 4);
    assert_eq!((history.versions[0].id, history.versions[0].parent_version), (done.version, Some(versions[2])));

    // The old `expected_version` is stale now: 409 with the conflict list, nothing changes.
    let (status, stale) = error(restore_request(s, &ana, node, body).await).await;
    assert_eq!((status, stale.error.as_str()), (StatusCode::CONFLICT, "conflict"));
    assert_eq!(stale.conflicts.as_ref().map(Vec::len), Some(1));
    assert_eq!(space_of(s, &ana).await.seq, before.seq + 1);

    // A version that is not this file's: 404. Garbage bodies: 400.
    let unknown = json!({ "version": VersionId::generate(), "expected_version": done.version });
    let (status, body) = error(restore_request(s, &ana, node, unknown).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::NOT_FOUND, "not_found"));
    let (other, other_versions, _) = file_with_history(s, &ana, &[format!("other {tag}").as_bytes()]).await;
    assert_ne!(other, node);
    let foreign = json!({ "version": other_versions[0], "expected_version": done.version });
    let (status, body) = error(restore_request(s, &ana, node, foreign).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::NOT_FOUND, "not_found"), "a version of another file");
    for bad in [json!({}), json!({ "version": "x", "expected_version": "y" }), json!({ "version": versions[0] })] {
        let (status, body) = error(restore_request(s, &ana, node, bad).await).await;
        assert!(status.is_client_error() && body.error == "invalid", "{status} {}", body.error);
    }
    let folder = root(s, &ana).await;
    let (status, body) = error(restore_request(s, &ana, folder, json!({ "version": versions[0], "expected_version": versions[0] })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::BAD_REQUEST, "invalid"), "a folder");

    // A deleted file is not restored from here.
    let seq = space_of(s, &ana).await.seq;
    assert_eq!(commit(s, &ana, &[Op::Delete { node, expected: Some(done.version) }]).await.status(), StatusCode::OK);
    let (status, body) = error(restore_request(s, &ana, node, json!({ "version": versions[0], "expected_version": done.version })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::BAD_REQUEST, "invalid"), "a deleted file");
    assert_eq!(space_of(s, &ana).await.seq, seq + 1, "only the delete");
}

async fn parallel_restores_against_one_head_let_one_win(s: &Server) {
    use craft_cloud_proto::api::RestoreResponse;
    add_user(s, "ana").await;
    let ana = login(s, "ana").await;
    let tag = uuid::Uuid::now_v7();
    let contents: Vec<Vec<u8>> = (0..3).map(|i| format!("parallel {tag} {i}").into_bytes()).collect();
    let refs: Vec<&[u8]> = contents.iter().map(Vec::as_slice).collect();
    let (node, versions, _) = file_with_history(s, &ana, &refs).await;
    let before = space_of(s, &ana).await.seq;
    let mut tasks = Vec::new();
    for i in 0..6 {
        let (client, url, token) = (s.client.clone(), format!("{}/nodes/{node}/restore", s.base), ana.clone());
        let body = json!({ "version": versions[i % 2], "expected_version": versions[2] });
        tasks.push(tokio::spawn(async move { client.post(url).bearer_auth(token).json(&body).send().await.unwrap() }));
    }
    let (mut won, mut conflicted) = (Vec::new(), 0);
    for task in tasks {
        let res = task.await.unwrap();
        match res.status() {
            StatusCode::OK => won.push(res.json::<RestoreResponse>().await.unwrap()),
            StatusCode::CONFLICT => conflicted += 1,
            other => panic!("unexpected status {other}"),
        }
    }
    assert_eq!((won.len(), conflicted), (1, 5));
    assert_eq!(space_of(s, &ana).await.seq, before + 1);
}

// ---- the trash ----

const DAY_MS: i64 = 86_400_000;

async fn trash_page(s: &Server, token: &str, query: &str) -> reqwest::Response {
    s.client.get(format!("{}/trash{query}", s.base)).bearer_auth(token).send().await.unwrap()
}

async fn trash_restore(s: &Server, token: &str, node: NodeId) -> reqwest::Response {
    s.client.post(format!("{}/trash/{node}/restore", s.base)).bearer_auth(token).send().await.unwrap()
}

async fn trash_remove(s: &Server, token: &str, node: NodeId) -> reqwest::Response {
    s.client.delete(format!("{}/trash/{node}", s.base)).bearer_auth(token).send().await.unwrap()
}

/// Uploads `data` and makes it the file `node` in `parent`: its first version.
async fn new_file(s: &Server, token: &str, parent: NodeId, file_name: &str, data: &[u8]) -> (NodeId, VersionId) {
    let node = NodeId::generate();
    let manifest = upload(s, token, data).await;
    let op = create(node, parent, file_name, manifest);
    let Op::Create { version, .. } = &op else { panic!("a create") };
    let id = version.id;
    assert_eq!(commit(s, token, &[op]).await.status(), StatusCode::OK);
    (node, id)
}

async fn delete_node(s: &Server, token: &str, node: NodeId, version: VersionId) {
    assert_eq!(commit(s, token, &[Op::Delete { node, expected: Some(version) }]).await.status(), StatusCode::OK);
}

async fn the_trash_lists_restores_and_removes_over_http(s: &Server) {
    use craft_cloud_proto::api::{HistoryResponse, TrashResponse, TrashRestoreResponse};
    add_user(s, "ana").await;
    add_user(s, "bob").await;
    let (ana, bob) = (login(s, "ana").await, login(s, "bob").await);
    let r = root(s, &ana).await;
    let tag = uuid::Uuid::now_v7();

    let empty: TrashResponse = trash_page(s, &ana, "").await.json().await.unwrap();
    assert_eq!((empty.items.len(), empty.next), (0, None));
    let mut files = Vec::new();
    for i in 0..3 {
        let data = format!("trash {tag} {i}").into_bytes();
        let (node, version) = new_file(s, &ana, r, &format!("t{i}.txt"), &data).await;
        delete_node(s, &ana, node, version).await;
        files.push((node, version, data));
    }

    // Newest deletion first, with the time the server will wait (30 days by default).
    let all: TrashResponse = trash_page(s, &ana, "").await.json().await.unwrap();
    assert_eq!(all.items.len(), 3);
    assert!(all.items.iter().all(|i| i.parent == r && !i.is_dir && i.purge_after_ms == i.deleted_ms + 30 * DAY_MS && i.deleted_ms > 0));
    assert!(all.items.windows(2).all(|w| (w[0].deleted_ms, w[0].id.to_string()) > (w[1].deleted_ms, w[1].id.to_string())), "newest first");
    assert_eq!(all.items.iter().find(|i| i.id == files[1].0).unwrap().size, Some(files[1].2.len() as u64));
    let (mut seen, mut query) = (Vec::new(), "?limit=2".to_owned());
    loop {
        let page: TrashResponse = trash_page(s, &ana, &query).await.json().await.unwrap();
        seen.extend(page.items.iter().map(|i| i.id));
        match page.next {
            Some(next) => query = format!("?limit=2&cursor={next}"),
            None => break,
        }
    }
    assert_eq!(seen, all.items.iter().map(|i| i.id).collect::<Vec<_>>());
    for bad in ["?limit=0", "?limit=201", "?cursor=nonsense", "?cursor=%201.00000000-0000-0000-0000-000000000000"] {
        let (status, body) = error(trash_page(s, &ana, bad).await).await;
        assert_eq!((status, body.error.as_str()), (StatusCode::BAD_REQUEST, "invalid"), "{bad}");
    }

    // Nobody else sees, restores or removes it; no token, no trash.
    let theirs: TrashResponse = trash_page(s, &bob, "").await.json().await.unwrap();
    assert!(theirs.items.is_empty());
    for res in [trash_restore(s, &bob, files[0].0).await, trash_remove(s, &bob, files[0].0).await] {
        let (status, body) = error(res).await;
        assert_eq!((status, body.error.as_str()), (StatusCode::NOT_FOUND, "not_found"));
    }
    assert_eq!(trash_page(s, "not-a-token", "").await.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(s.client.post(format!("{}/trash/{}/restore", s.base, files[0].0)).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
    assert_eq!(s.client.delete(format!("{}/trash/{}", s.base, files[0].0)).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);

    // A restore is one change: the file is live again with its content.
    let before = space_of(s, &ana).await;
    let res = trash_restore(s, &ana, files[0].0).await;
    assert_eq!(res.status(), StatusCode::OK);
    let done: TrashRestoreResponse = res.json().await.unwrap();
    assert_eq!(
        (done.node, done.parent, done.name.as_str(), done.renamed, done.seq, done.epoch),
        (files[0].0, r, "t0.txt", false, before.seq + 1, before.epoch)
    );
    let feed: ChangeSet =
        s.client.get(format!("{}/changes?since={}", s.base, before.seq)).bearer_auth(&ana).send().await.unwrap().json().await.unwrap();
    let state = feed.nodes.iter().find(|n| n.id == files[0].0).unwrap();
    assert!(!state.deleted);
    let NodeKind::File(file) = &state.kind else { panic!("a file") };
    assert_eq!(download(s, &ana, &file.manifest).await, files[0].2);
    let after: TrashResponse = trash_page(s, &ana, "").await.json().await.unwrap();
    assert_eq!(after.items.len(), 2);
    // Not in the trash any more, and not there at all.
    for node in [files[0].0, NodeId::generate(), r] {
        let (status, body) = error(trash_restore(s, &ana, node).await).await;
        assert_eq!((status, body.error.as_str()), (StatusCode::NOT_FOUND, "not_found"), "{node}");
        let (status, body) = error(trash_remove(s, &ana, node).await).await;
        assert_eq!((status, body.error.as_str()), (StatusCode::NOT_FOUND, "not_found"), "{node}");
    }
    let (status, body) = error(s.client.post(format!("{}/trash/not-a-uuid/restore", s.base)).bearer_auth(&ana).send().await.unwrap()).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::BAD_REQUEST, "invalid"));

    // The name was taken meanwhile: the restore goes around it and says so, and the new file is untouched.
    let (taken, _) = new_file(s, &ana, r, "t1.txt", format!("squatter {tag}").as_bytes()).await;
    let done: TrashRestoreResponse = trash_restore(s, &ana, files[1].0).await.json().await.unwrap();
    assert!(done.renamed && done.name.starts_with("t1 (conflict, laptop, ") && done.name.ends_with(").txt"), "{}", done.name);
    let tree: ChangeSet = s.client.get(format!("{}/tree", s.base)).bearer_auth(&ana).send().await.unwrap().json().await.unwrap();
    let names: Vec<_> =
        tree.nodes.iter().filter(|n| n.id == taken || n.id == files[1].0).map(|n| n.name.as_ref().unwrap().as_str().to_owned()).collect();
    assert!(names.contains(&"t1.txt".to_owned()) && names.contains(&done.name), "{names:?}");

    // Removing from the trash only marks it: the history and the content are still there, nothing changes in the feed.
    let seq = space_of(s, &ana).await.seq;
    assert_eq!(trash_remove(s, &ana, files[2].0).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(trash_remove(s, &ana, files[2].0).await.status(), StatusCode::NO_CONTENT, "asking again is the same");
    assert_eq!(space_of(s, &ana).await.seq, seq);
    let left: TrashResponse = trash_page(s, &ana, "").await.json().await.unwrap();
    assert!(left.items.is_empty());
    let history: HistoryResponse = history_page(s, &ana, files[2].0, "").await.json().await.unwrap();
    assert_eq!(history.versions.len(), 1);
    assert_eq!(download(s, &ana, &manifest_for(&files[2].2).id()).await, files[2].2);
    let (status, body) = error(trash_restore(s, &ana, files[2].0).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::NOT_FOUND, "not_found"));
    // The restore of a version refuses a deleted file, and points to the trash.
    let (status, body) = error(restore_request(s, &ana, files[2].0, json!({ "version": files[2].1, "expected_version": files[2].1 })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::BAD_REQUEST, "invalid"));
    assert!(body.message.contains("/trash/"), "{}", body.message);
}

/// A deleted folder is listed alone and is restored (or removed) with everything of it that is in the trash, also a
/// file that was deleted on its own before.
async fn a_deleted_folder_comes_back_and_goes_with_what_was_deleted_in_it(s: &Server) {
    use craft_cloud_proto::api::{TrashResponse, TrashRestoreResponse};
    add_user(s, "ana").await;
    let ana = login(s, "ana").await;
    let r = root(s, &ana).await;
    let (dir1, sub1, dir2, sub2) = (NodeId::generate(), NodeId::generate(), NodeId::generate(), NodeId::generate());
    for (name, dir, sub) in [("dir1", dir1, sub1), ("dir2", dir2, sub2)] {
        let ops = [
            Op::Mkdir { node: dir, parent: r, name: Name::new(name).unwrap() },
            Op::Mkdir { node: sub, parent: dir, name: Name::new("sub").unwrap() },
        ];
        assert_eq!(commit(s, &ana, &ops).await.status(), StatusCode::OK);
    }
    let mut kids = Vec::new();
    for (dir, sub) in [(dir1, sub1), (dir2, sub2)] {
        let (early, early_v) = new_file(s, &ana, dir, "early.txt", b"deleted on its own first").await;
        delete_node(s, &ana, early, early_v).await;
        let (child, child_v) = new_file(s, &ana, dir, "child.txt", b"deleted with the folder").await;
        let (deep, deep_v) = new_file(s, &ana, sub, "deep.txt", b"deep").await;
        let ops = [
            Op::Delete { node: child, expected: Some(child_v) },
            Op::Delete { node: deep, expected: Some(deep_v) },
            Op::Delete { node: sub, expected: None },
            Op::Delete { node: dir, expected: None },
        ];
        assert_eq!(commit(s, &ana, &ops).await.status(), StatusCode::OK);
        kids.push([early, child, deep, sub]);
    }
    let listed: TrashResponse = trash_page(s, &ana, "").await.json().await.unwrap();
    let mut ids: Vec<_> = listed.items.iter().map(|i| i.id).collect();
    ids.sort();
    let mut tops = vec![dir1, dir2];
    tops.sort();
    assert_eq!(ids, tops, "only the folders are listed");

    // Restoring one folder brings all that was in the trash below it, and nothing of the other.
    let before = space_of(s, &ana).await;
    let done: TrashRestoreResponse = trash_restore(s, &ana, dir1).await.json().await.unwrap();
    assert_eq!((done.node, done.name.as_str(), done.renamed, done.seq), (dir1, "dir1", false, before.seq + 1));
    let tree: ChangeSet = s.client.get(format!("{}/tree", s.base)).bearer_auth(&ana).send().await.unwrap().json().await.unwrap();
    let live: Vec<_> = tree.nodes.iter().map(|n| n.id).collect();
    for node in kids[0].iter().chain([&dir1]) {
        assert!(live.contains(node), "{node} is live again");
    }
    for node in kids[1].iter().chain([&dir2]) {
        assert!(!live.contains(node), "{node} is still deleted");
    }

    // Removing the other folder takes what is below it out of the trash too.
    assert_eq!(trash_remove(s, &ana, dir2).await.status(), StatusCode::NO_CONTENT);
    let left: TrashResponse = trash_page(s, &ana, "").await.json().await.unwrap();
    assert!(left.items.is_empty(), "{:?}", left.items);
    for node in kids[1] {
        let (status, body) = error(trash_restore(s, &ana, node).await).await;
        assert_eq!((status, body.error.as_str()), (StatusCode::NOT_FOUND, "not_found"), "{node}");
    }
}

/// A server with another `retention.trash_days` (SQLite and disk).
async fn sqlite_disk_with_trash_days(days: u32) -> Server {
    let db = tempfile::tempdir().unwrap();
    let blobs = tempfile::tempdir().unwrap();
    let meta = Meta::open(&format!("sqlite:{}", db.path().join("meta.db").display())).await.unwrap();
    let store = Store::new(DiskBackend::new(blobs.path()).unwrap());
    let state = AppState::new(meta.clone(), store, vec![]).unwrap().with_trash_days(days);
    start_state(state, meta, vec![db, blobs], None).await
}

/// Setting up, turning off and new recovery codes count wrong passwords like a password change does (5 in 15 minutes per
/// account, then `429 rate_limited`), and take one of the server-wide password-check slots (all busy: `429` with
/// `Retry-After: 1` and no try used).

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_two_factor_routes_count_wrong_passwords_and_wait_for_a_check_slot() {
    let s = sqlite_disk_with(Limits { password_checks: 1, ..Limits::default() }).await;
    // One account for each route: the tries are the account's, shared by every route that asks for the password.
    let mut cases = Vec::new();
    for (name, path, body) in [
        ("ana", "/account/two-factor/setup", json!({ "password": "wrong password!" })),
        ("bob", "/account/two-factor/disable", json!({ "password": "wrong password!", "totp_code": "123456" })),
        ("cy", "/account/two-factor/recovery-codes", json!({ "password": "wrong password!", "totp_code": "123456" })),
    ] {
        cases.push((turn_on_two_factor(&s, name, false).await, path, body));
    }
    for (account, path, body) in &cases {
        let token = &account.token;
        // A busy slot is told to come back, and costs no try however often it is asked.
        let busy = s.password_checks.clone().try_acquire_owned().unwrap();
        for _ in 0..7 {
            let res = post_json(&s, token, path, body.clone()).await;
            assert_eq!(retry_after(&res).await.as_deref(), Some("1"), "{path}");
            assert_eq!(error(res).await.1.error, "rate_limited", "{path}");
        }
        drop(busy);
        for n in 1..=5 {
            let (status, answer) = error(post_json(&s, token, path, body.clone()).await).await;
            assert_eq!((status, answer.error.as_str()), (StatusCode::FORBIDDEN, "wrong_password"), "{path} try {n}");
        }
        let (status, answer) = error(post_json(&s, token, path, body.clone()).await).await;
        assert_eq!((status, answer.error.as_str()), (StatusCode::TOO_MANY_REQUESTS, "rate_limited"), "{path}: the sixth");
    }
    // The tries are the account's: the right password is held off as well while the window lasts.
    let (account, _, _) = &cases[0];
    let (status, answer) = error(
        post_json(&s, &account.token, "/account/two-factor/disable", json!({ "password": PASSWORD, "recovery_code": account.recovery[0] })).await,
    )
    .await;
    assert_eq!((status, answer.error.as_str()), (StatusCode::TOO_MANY_REQUESTS, "rate_limited"));
}

/// What sign-in with a second factor and a join write to the audit log, over HTTP: a login for every device made (a challenge is
/// none), the wrong second steps in one row per account and minute, a challenge that is not good in one row of its own with no
/// account, a recovery code's use, and never a code, a challenge or a name.
async fn two_factor_sign_in_and_join_are_audited(s: &Server) {
    use craft_cloud_meta::AuditQuery;
    let events = |s: &Server| {
        let meta = s.meta.clone();
        async move { meta.audit_list(&AuditQuery { limit: 1000, ..AuditQuery::default() }).await.unwrap() }
    };
    let named = |all: &[craft_cloud_meta::AuditEvent], action: &str| all.iter().filter(|e| e.action.as_str() == action).count();
    let ana = turn_on_two_factor(s, "ana", false).await;
    let id = s.meta.user_by_name("ana").await.unwrap().unwrap().id;
    let all = events(s).await;
    assert_eq!(named(&all, "account.two_factor_enabled"), 1);
    let logins = named(&all, "auth.login");

    // A challenge is not a login; a wrong code is a failure row of the account, with how far it got.
    let challenge = login_challenge(s, "ana").await;
    assert_eq!(named(&events(s).await, "auth.login"), logins, "the challenge is no login");
    let right = ana.fresh_code(s);
    let (status, body) = error(second_step(s, &challenge.challenge, json!({ "totp_code": wrong_code(&right) })).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::UNAUTHORIZED, "two_factor_invalid"));
    let all = events(s).await;
    let failed: Vec<_> = all.iter().filter(|e| e.action.as_str() == "auth.second_factor_failed").collect();
    assert_eq!(failed.len(), 1);
    assert_eq!((failed[0].target_id, failed[0].actor_user, failed[0].count), (Some(id.0), None, 1));
    assert_eq!(serde_json::from_str::<serde_json::Value>(&failed[0].details).unwrap(), json!({ "step": "totp" }));
    // A challenge that is no token, and one that is not there: one more row, of no account, and the same answer.
    for bad in ["short".to_owned(), "A".repeat(43)] {
        let (status, body) = error(second_step(s, &bad, json!({ "totp_code": right })).await).await;
        assert_eq!((status, body.error.as_str()), (StatusCode::UNAUTHORIZED, "unauthorized"));
    }
    let all = events(s).await;
    let gone: Vec<_> = all.iter().filter(|e| e.action.as_str() == "auth.second_factor_failed" && e.target_id.is_none()).collect();
    assert_eq!((gone.len(), gone[0].count), (1, 2));
    assert_eq!(serde_json::from_str::<serde_json::Value>(&gone[0].details).unwrap(), json!({ "step": "challenge_expired" }));

    // The right code: a login of the new device.
    let res = second_step(s, &challenge.challenge, json!({ "totp_code": right })).await;
    assert_eq!(res.status(), StatusCode::OK);
    let all = events(s).await;
    assert_eq!(named(&all, "auth.login"), logins + 1);
    let login = all.iter().find(|e| e.action.as_str() == "auth.login" && e.actor_user == Some(id) && e.at_ms == s.at()).unwrap();
    assert!(login.actor_device.is_some());

    // A recovery code at sign-in, and at a join: the use of the code with each.
    let challenge = login_challenge(s, "ana").await;
    let res = second_step(s, &challenge.challenge, json!({ "recovery_code": ana.recovery[0] })).await;
    assert_eq!(res.status(), StatusCode::OK);
    let all = events(s).await;
    assert_eq!((named(&all, "auth.login"), named(&all, "auth.recovery_code_used")), (logins + 2, 1));
    let invite = Code::generate(INVITE_CHARS).unwrap();
    s.meta.create_invite("ana", &invite.hash(), s.at(), INVITE_TTL_MS).await.unwrap();
    let body = json!({ "code": invite.as_str(), "device_name": "new pc", "recovery_code": ana.recovery[1] });
    let device = joined(call(s, None, reqwest::Method::POST, "/join", Some(body)).await).await;
    assert!(accepted(s, &device.token).await);
    let all = events(s).await;
    assert_eq!((named(&all, "join.invite_redeemed"), named(&all, "auth.recovery_code_used")), (1, 2));

    // Nothing secret is in any row: no code, recovery code, challenge, token or name.
    let everything = serde_json::to_string(&all.iter().map(|e| (&e.details, e.action.as_str())).collect::<Vec<_>>()).unwrap();
    let secrets: [&str; 6] = [&challenge.challenge, &ana.recovery[0], &ana.recovery[1], &ana.token, &device.token, "ana"];
    for secret in secrets {
        assert!(!everything.contains(secret), "{secret}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_purge_time_follows_the_configured_trash_days() {
    use craft_cloud_proto::api::TrashResponse;
    let s = sqlite_disk_with_trash_days(7).await;
    add_user(&s, "ana").await;
    let ana = login(&s, "ana").await;
    let r = root(&s, &ana).await;
    let (node, version) = new_file(&s, &ana, r, "week.txt", b"seven days").await;
    delete_node(&s, &ana, node, version).await;
    let page: TrashResponse = trash_page(&s, &ana, "").await.json().await.unwrap();
    assert_eq!(page.items[0].purge_after_ms - page.items[0].deleted_ms, 7 * DAY_MS);
}

// ---- the reference index ----

/// A server whose metadata writes only a few index edges per transaction (SQLite and disk).
async fn sqlite_disk_with_index_budget(rows: usize) -> Server {
    let db = tempfile::tempdir().unwrap();
    let blobs = tempfile::tempdir().unwrap();
    let meta = Meta::open(&format!("sqlite:{}", db.path().join("meta.db").display())).await.unwrap().with_index_batch_rows(rows);
    let store = Store::new(DiskBackend::new(blobs.path()).unwrap());
    let state = AppState::new(meta.clone(), store, vec![]).unwrap();
    start_state(state, meta, vec![db, blobs], None).await
}

/// A manifest of `n` chunks (the first one twice), their data distinct; uploaded chunks first.
async fn upload_many(s: &Server, token: &str, n: usize, tag: &str) -> (Manifest, Vec<ChunkId>) {
    let datas: Vec<Vec<u8>> = (0..n).map(|i| format!("index {tag} {i} {}", uuid::Uuid::now_v7()).into_bytes()).collect();
    let mut order: Vec<usize> = (0..n).collect();
    order.push(0);
    for data in &datas {
        assert_eq!(put_chunk(s, token, data).await, StatusCode::NO_CONTENT);
    }
    let refs: Vec<ChunkRef> = order.iter().map(|i| ChunkRef { id: chunk_id(&datas[*i]), len: u32::try_from(datas[*i].len()).unwrap() }).collect();
    let size = refs.iter().map(|r| u64::from(r.len)).sum();
    let manifest = Manifest::new(ChunkerParamsId::new("fastcdc-v1").unwrap(), size, FileHash::from_bytes([9; 32]), refs.clone()).unwrap();
    (manifest, refs.iter().map(|r| r.id).collect())
}

/// A manifest accepted is indexed, exactly and completely, before the upload is acknowledged: a short one in the
/// transaction that records it, a longer one (past the server's budget per transaction) in the batches the handler
/// finishes. Uploading it again, or as another account, changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_uploaded_manifest_is_indexed_before_the_upload_is_acknowledged() {
    use craft_cloud_meta::IndexState;
    let s = sqlite_disk_with_index_budget(3).await;
    add_user(&s, "ana").await;
    add_user(&s, "bob").await;
    let (ana, bob) = (login(&s, "ana").await, login(&s, "bob").await);
    for (n, label) in [(2usize, "short"), (9, "long")] {
        let (manifest, chunks) = upload_many(&s, &ana, n, label).await;
        assert_eq!(put_manifest(&s, &ana, &manifest).await.status(), StatusCode::NO_CONTENT, "{label}");
        assert_eq!(s.meta.index_state(&manifest.id()).await.unwrap(), IndexState::Complete { total: n as u64 + 1 }, "{label}");
        let edges = s.meta.index_edges(&manifest.id()).await.unwrap();
        assert_eq!(edges.iter().map(|(_, c)| *c).collect::<Vec<_>>(), chunks, "{label}: in order, the first chunk at both its places");
        // Again, and as another account (which has to upload the chunks too).
        let before = s.meta.index_edge_count().await.unwrap();
        assert_eq!(put_manifest(&s, &ana, &manifest).await.status(), StatusCode::NO_CONTENT);
        for chunk in manifest.chunks() {
            let data = s.client.get(format!("{}/chunks/{}", s.base, chunk.id)).bearer_auth(&ana).send().await.unwrap().bytes().await.unwrap();
            assert_eq!(
                s.client.put(format!("{}/chunks/{}", s.base, chunk.id)).bearer_auth(&bob).body(data.to_vec()).send().await.unwrap().status(),
                StatusCode::NO_CONTENT
            );
        }
        assert_eq!(put_manifest(&s, &bob, &manifest).await.status(), StatusCode::NO_CONTENT);
        assert_eq!(s.meta.index_edge_count().await.unwrap(), before, "{label}: nothing written twice");
    }
    // A manifest naming a chunk the account lacks is refused, and leaves no index behind.
    let (manifest, _) = upload_many(&s, &ana, 2, "refused").await;
    let lacking = {
        let mut refs = manifest.chunks().to_vec();
        refs.push(ChunkRef { id: chunk_id(b"never uploaded"), len: 14 });
        let size = refs.iter().map(|r| u64::from(r.len)).sum();
        Manifest::new(ChunkerParamsId::new("fastcdc-v1").unwrap(), size, FileHash::from_bytes([8; 32]), refs).unwrap()
    };
    let (status, body) = error(put_manifest(&s, &ana, &lacking).await).await;
    assert_eq!((status, body.error.as_str()), (StatusCode::CONFLICT, "missing_chunks"));
    assert_eq!(s.meta.index_state(&lacking.id()).await.unwrap(), IndexState::Absent);
}
