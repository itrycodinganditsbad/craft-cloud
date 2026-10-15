//! The HTTP API end to end: a real server on a random loopback port, driven with an HTTP client. Every
//! scenario runs on SQLite + disk, and on Postgres + S3 when `CRAFT_CLOUD_TEST_PG` and the
//! `CRAFT_CLOUD_TEST_S3_*` variables are set (`CRAFT_CLOUD_REQUIRE_BACKENDS` turns a missing one into a failure).

use std::time::{Duration, Instant};

use craft_cloud_meta::{Meta, Role};
use craft_cloud_proto::api::{CommitResponse, DeviceInfo, EPOCH_HEADER, ErrorBody, InfoResponse, LoginResponse, MissingResponse, SpaceResponse};
use craft_cloud_proto::{
    ChangeSet, ChunkId, ChunkRef, ChunkerParamsId, FileHash, Manifest, ManifestId, Name, NewVersion, NodeId, NodeKind, Op, VersionId,
};
use craft_cloud_server::AppState;
use craft_cloud_server::auth::hash_password;
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
    let state = AppState::new(meta.clone(), store, vec![]).unwrap().with_limits(limits);
    let password_checks = state.password_checks.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/api/v1", listener.local_addr().unwrap());
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(craft_cloud_server::serve(state, listener, async move {
        let _ = rx.await;
    }));
    Server { base, meta, client: Client::new(), password_checks, shutdown: std::sync::Mutex::new(Some(tx)), _dirs: dirs, _pg: pg }
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

async fn postgres_s3() -> Option<Server> {
    let required = std::env::var_os("CRAFT_CLOUD_REQUIRE_BACKENDS").is_some();
    let (Ok(admin_url), Ok(endpoint)) = (std::env::var("CRAFT_CLOUD_TEST_PG"), std::env::var("CRAFT_CLOUD_TEST_S3_ENDPOINT")) else {
        assert!(!required, "CRAFT_CLOUD_REQUIRE_BACKENDS is set but CRAFT_CLOUD_TEST_PG / CRAFT_CLOUD_TEST_S3_ENDPOINT are not");
        return None;
    };
    use sqlx::Connection;
    let name = format!("cc_test_{}", uuid::Uuid::now_v7().simple());
    let mut conn = sqlx::PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}"))).execute(&mut conn).await.unwrap();
    let guard = PgGuard { admin_url: admin_url.clone(), name: name.clone() };
    let (base, _) = admin_url.rsplit_once('/').unwrap();
    let meta = Meta::open(&format!("{base}/{name}")).await.unwrap();
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
    every_rejection_is_a_json_error,
    one_request_cant_list_unbounded_missing_chunks,
    long_polls_are_capped_per_space,
    odd_usernames_and_device_names_are_refused,
    shutting_down_ends_long_polls,
    the_space_endpoint_and_unversioned_info,
    oversized_manifests_are_refused,
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

async fn commit(s: &Server, token: &str, ops: &[Op]) -> reqwest::Response {
    s.client.post(format!("{}/commit", s.base)).bearer_auth(token).json(&json!({ "ops": ops })).send().await.unwrap()
}

async fn root(s: &Server, token: &str) -> NodeId {
    let tree: ChangeSet = s.client.get(format!("{}/tree", s.base)).bearer_auth(token).send().await.unwrap().json().await.unwrap();
    tree.nodes.iter().find(|n| n.parent.is_none()).unwrap().id
}

fn create(node: NodeId, parent: NodeId, name: &str, manifest: ManifestId) -> Op {
    Op::Create { node, parent, name: Name::new(name).unwrap(), version: NewVersion { id: VersionId::generate(), manifest, mtime_ms: 1, exec: false } }
}

// ---- scenarios ----

async fn info_needs_no_sign_in(s: &Server) {
    let info: InfoResponse = s.client.get(format!("{}/info", s.base)).send().await.unwrap().json().await.unwrap();
    assert_eq!((info.protocol_min, info.protocol_max), (1, 1));
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

// ---- load limits (the same on every backend, so SQLite + disk only) ----

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
