//! Live documents in team spaces over a real WebSocket (live protocol 3): two accounts on one document, a role per frame,
//! viewers (read and presence, never a write or a `compact`), presence stamped by the server, the limits per space, the
//! registry of connections and the end of a connection whose access changed (in this process at once, and a change made
//! elsewhere at the next ping or the next write), and a protocol 2 agent that sees what it always saw. SQLite and disk,
//! and Postgres and disk when `CRAFT_CLOUD_TEST_PG` is set (`CRAFT_CLOUD_REQUIRE_BACKENDS` turns a missing backend into a
//! failure).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use craft_cloud_meta::{Meta, Role, SpaceActor, UserRecord};
use craft_cloud_proto::api::{AdminSpaceInfo, LoginResponse, SPACE_HEADER, SpaceRole};
use craft_cloud_proto::live::{
    ACCESS_CHANGED, ACCESS_CHANGED_CLOSE, AppName, ClientFrame, DocKind, PROTOCOL_MISMATCH, ServerFrame, node_doc, recents_doc,
};
use craft_cloud_proto::{DocId, ManifestId, NodeId, SpaceId};
use craft_cloud_server::AppState;
use craft_cloud_server::auth::hash_password;
use craft_cloud_server::state::Limits;
use craft_cloud_store::Store;
use craft_cloud_store::disk::DiskBackend;
use futures_util::{SinkExt, StreamExt};
use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const PASSWORD: &str = "correct horse battery";
const NOW: i64 = 1_700_000_000_000;
/// What one team space's live documents may hold in these tests (the personal spaces keep their 1 GiB).
const TEAM_LIVE_BYTES: u64 = 10_000;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct World {
    base: String,
    client: Client,
    meta: Meta,
    users: Vec<(&'static str, UserRecord, String)>,
    _dirs: Vec<tempfile::TempDir>,
    _pg: Option<PgGuard>,
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

/// The limits of these tests: connections are pinged every quarter second (a change made elsewhere is found at the next
/// ping), a log is long above 3 updates, and a device that does not answer a `compact` is passed over after 100 ms.
fn limits() -> Limits {
    Limits { live_ping: Duration::from_millis(250), compact_updates: 3, compact_timeout: Duration::from_millis(100), ..Limits::default() }
}

/// The same with the default ping (30 s), which never comes within a test.
fn slow_limits() -> Limits {
    Limits { compact_updates: 3, ..Limits::default() }
}

fn app() -> AppName {
    AppName::new("wordcraft").unwrap()
}

/// What a connection said last: a frame, or the end.
enum Heard {
    Frame(ServerFrame),
    Raw(Vec<u8>),
    Closed(Option<(u16, String)>),
    Nothing,
}

async fn hear(ws: &mut Ws, within: Duration) -> Heard {
    // One deadline: the server's pings (every quarter second here) must not restart the wait.
    let deadline = Instant::now() + within;
    loop {
        match tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), ws.next()).await {
            Err(_) => return Heard::Nothing,
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
            Ok(Some(Ok(Message::Binary(bytes)))) => {
                return match ServerFrame::decode(&bytes) {
                    Ok(frame) => Heard::Frame(frame),
                    Err(_) => Heard::Raw(bytes.to_vec()),
                };
            }
            Ok(Some(Ok(Message::Close(frame)))) => return Heard::Closed(frame.map(|f| (u16::from(f.code), f.reason.to_string()))),
            Ok(Some(Ok(other))) => panic!("unexpected message {other:?}"),
            Ok(Some(Err(_)) | None) => return Heard::Closed(None),
        }
    }
}

/// The next frame, within 10 s.
async fn frame(ws: &mut Ws) -> ServerFrame {
    match hear(ws, Duration::from_secs(10)).await {
        Heard::Frame(frame) => frame,
        Heard::Raw(bytes) => panic!("a message that is no frame: {bytes:?}"),
        Heard::Closed(why) => panic!("the connection ended: {why:?}"),
        Heard::Nothing => panic!("nothing within 10 s"),
    }
}

/// The next message as the bytes the server sent.
async fn raw(ws: &mut Ws) -> Vec<u8> {
    match tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                Some(Ok(Message::Binary(bytes))) => return bytes.to_vec(),
                other => panic!("expected a binary message, got {other:?}"),
            }
        }
    })
    .await
    {
        Ok(bytes) => bytes,
        Err(_) => panic!("nothing within 10 s"),
    }
}

/// Nothing arrives for a moment (pings aside).
async fn quiet(ws: &mut Ws) {
    match hear(ws, Duration::from_millis(400)).await {
        Heard::Nothing => {}
        Heard::Frame(frame) => panic!("expected nothing, got {frame:?}"),
        Heard::Raw(bytes) => panic!("expected nothing, got {bytes:?}"),
        Heard::Closed(why) => panic!("expected nothing, the connection ended: {why:?}"),
    }
}

/// Everything up to the end of the connection, within `within`: the frames, the close (code and reason) and how long it took.
async fn until_closed(ws: &mut Ws, within: Duration) -> (Vec<ServerFrame>, Option<(u16, String)>, Duration) {
    let started = Instant::now();
    let mut frames = Vec::new();
    loop {
        let left = within.saturating_sub(started.elapsed());
        match hear(ws, left).await {
            Heard::Frame(frame) => frames.push(frame),
            Heard::Raw(bytes) => panic!("a message that is no frame: {bytes:?}"),
            Heard::Closed(why) => return (frames, why, started.elapsed()),
            Heard::Nothing => panic!("the connection was not closed within {within:?}; heard {frames:?}"),
        }
    }
}

async fn send(ws: &mut Ws, frame: ClientFrame) {
    ws.send(Message::Binary(frame.encode().unwrap().into())).await.unwrap();
}

fn error_code(frame: &ServerFrame) -> (&str, Option<DocId>, Option<u64>) {
    match frame {
        ServerFrame::Error { code, doc, client_seq, .. } => (code.as_str(), *doc, *client_seq),
        other => panic!("expected an error, got {other:?}"),
    }
}

impl World {
    fn user(&self, name: &str) -> &UserRecord {
        &self.users.iter().find(|(n, ..)| *n == name).unwrap().1
    }

    fn token(&self, name: &str) -> &str {
        &self.users.iter().find(|(n, ..)| *n == name).unwrap().2
    }

    async fn call(&self, who: &str, method: Method, path: &str, body: Option<Value>) -> reqwest::Response {
        let mut request = self.client.request(method, format!("{}{path}", self.base)).bearer_auth(self.token(who));
        if let Some(body) = body {
            request = request.json(&body);
        }
        request.send().await.unwrap()
    }

    fn confirmed(body: Value) -> Value {
        let mut body = body;
        body.as_object_mut().unwrap().insert("password".into(), json!(PASSWORD));
        body
    }

    async fn create(&self, name: &str, owner: &str) -> SpaceId {
        let res =
            self.call("boss", Method::POST, "/admin/spaces", Some(Self::confirmed(json!({ "name": name, "owner": self.user(owner).id.0 })))).await;
        assert_eq!(res.status(), StatusCode::OK, "create {name}");
        res.json::<AdminSpaceInfo>().await.unwrap().id
    }

    async fn join(&self, space: SpaceId, by: &str, who: &str, role: &str) {
        let res = self.call(by, Method::POST, &format!("/spaces/{space}/members"), Some(json!({ "username": who, "role": role }))).await;
        assert_eq!(res.status(), StatusCode::OK, "invite {who}");
        let res = self.call(who, Method::POST, &format!("/spaces/{space}/accept"), None).await;
        assert_eq!(res.status(), StatusCode::OK, "{who} accepts");
    }

    /// Opens `/live` as `who`, naming `space` if given: the socket, or the status the upgrade was refused with.
    async fn open(&self, who: &str, space: Option<SpaceId>) -> Result<Ws, StatusCode> {
        let mut request = format!("{}/live", self.base.replacen("http://", "ws://", 1)).into_client_request().unwrap();
        request.headers_mut().insert("authorization", format!("Bearer {}", self.token(who)).parse().unwrap());
        if let Some(space) = space {
            request.headers_mut().insert(SPACE_HEADER, space.to_string().parse().unwrap());
        }
        match tokio_tungstenite::connect_async(request).await {
            Ok((ws, _)) => Ok(ws),
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => Err(StatusCode::from_u16(response.status().as_u16()).unwrap()),
            Err(e) => panic!("{e}"),
        }
    }

    /// A connection to `space`, past `hello 1..3`: the socket and the welcome.
    async fn ready(&self, who: &str, space: SpaceId) -> (Ws, ServerFrame) {
        let mut ws = self.open(who, Some(space)).await.unwrap();
        send(&mut ws, ClientFrame::Hello { min: 1, max: 3 }).await;
        let welcome = frame(&mut ws).await;
        (ws, welcome)
    }

    /// `ready`, as `role`, the welcome checked.
    async fn ready_as(&self, who: &str, space: SpaceId, role: SpaceRole) -> Ws {
        let (ws, welcome) = self.ready(who, space).await;
        let epoch = self.meta.live_epoch(space).await.unwrap();
        assert_eq!(welcome, ServerFrame::WelcomeSpace { protocol: 3, epoch, space, role }, "{who}");
        ws
    }

    /// Makes the space's recents document and follows it from the start.
    async fn follow(&self, ws: &mut Ws, space: SpaceId) -> DocId {
        let doc = recents_doc(space, &app());
        send(ws, ClientFrame::Create { doc, kind: DocKind::Recents { app: app() } }).await;
        assert_eq!(frame(ws).await, ServerFrame::Created { doc });
        subscribe(ws, doc, 0).await;
        doc
    }
}

/// `subscribe` from `after`, reading up to `subscribed`: the updates of the catch-up.
async fn subscribe(ws: &mut Ws, doc: DocId, after: u64) -> Vec<(u64, Vec<u8>)> {
    send(ws, ClientFrame::Subscribe { doc, after, at: None }).await;
    let mut updates = Vec::new();
    loop {
        match frame(ws).await {
            ServerFrame::Update { seq, bytes, .. } => updates.push((seq, bytes)),
            ServerFrame::Subscribed { doc: d, .. } => {
                assert_eq!(d, doc);
                return updates;
            }
            ServerFrame::Compact { .. } => {}
            other => panic!("while catching up: {other:?}"),
        }
    }
}

async fn build(meta: Meta, dirs: Vec<tempfile::TempDir>, pg: Option<PgGuard>, limits: Limits) -> World {
    let blobs = tempfile::tempdir().unwrap();
    let store = Store::new(DiskBackend::new(blobs.path()).unwrap());
    let mut dirs = dirs;
    dirs.push(blobs);
    let meta = meta.with_team_live_bytes_cap(TEAM_LIVE_BYTES);
    let state = Arc::new(AppState::new(meta.clone(), store, vec![]).unwrap().with_limits(limits));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/api/v1", listener.local_addr().unwrap());
    let app = craft_cloud_server::api::router(state.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>()).await;
    });
    let client = Client::new();
    let hash = hash_password(PASSWORD).unwrap();
    let mut users = Vec::new();
    for (name, role) in
        [("boss", Role::Admin), ("ana", Role::Member), ("ben", Role::Member), ("cy", Role::Member), ("dee", Role::Member), ("eve", Role::Member)]
    {
        let record = meta.create_user(name, &hash, role, NOW).await.unwrap();
        let login = client
            .post(format!("{base}/auth/login"))
            .json(&json!({ "username": name, "password": PASSWORD, "device_name": "laptop" }))
            .send()
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::OK);
        let login: LoginResponse = login.json().await.unwrap();
        users.push((name, record, login.token));
    }
    World { base, client, meta, users, _dirs: dirs, _pg: pg }
}

async fn sqlite_disk(limits: Limits) -> World {
    let db = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}?mode=rwc", db.path().join("meta.db").display());
    let meta = Meta::open(&url).await.unwrap();
    build(meta, vec![db], None, limits).await
}

async fn postgres_disk(limits: Limits) -> Option<World> {
    let Ok(admin_url) = std::env::var("CRAFT_CLOUD_TEST_PG") else {
        assert!(std::env::var_os("CRAFT_CLOUD_REQUIRE_BACKENDS").is_none(), "CRAFT_CLOUD_REQUIRE_BACKENDS is set but CRAFT_CLOUD_TEST_PG is not");
        return None;
    };
    use sqlx::Connection;
    let name = format!("cc_test_{}", uuid::Uuid::now_v7().simple());
    let mut conn = sqlx::PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}"))).execute(&mut conn).await.unwrap();
    let guard = PgGuard { admin_url: admin_url.clone(), name: name.clone() };
    let (base, _) = admin_url.rsplit_once('/').unwrap();
    let url = format!("{base}/{name}");
    let meta = Meta::open(&url).await.unwrap();
    Some(build(meta, vec![], Some(guard), limits).await)
}

/// A scenario that does not finish in two minutes has hung: fail it instead of holding the build queue.
async fn within(scenario: impl std::future::Future<Output = ()>) {
    tokio::time::timeout(Duration::from_secs(120), scenario).await.expect("the scenario hung");
}

macro_rules! on_every_backend {
    ($group:ident, $limits:ident, $($scenario:ident),* $(,)?) => {
        mod $group {
            mod on_sqlite {
                $( #[tokio::test(flavor = "multi_thread", worker_threads = 4)] async fn $scenario() {
                    let w = super::super::sqlite_disk(super::super::$limits()).await;
                    super::super::within(super::super::$scenario(&w)).await;
                } )*
            }
            mod on_postgres {
                $( #[tokio::test(flavor = "multi_thread", worker_threads = 4)] async fn $scenario() {
                    let Some(w) = super::super::postgres_disk(super::super::$limits()).await else { eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set"); return };
                    super::super::within(super::super::$scenario(&w)).await;
                } )*
            }
        }
    };
}

// Most scenarios run with a quarter-second ping, so that a change made elsewhere is found within a test's patience.
on_every_backend!(
    fast_ping,
    limits,
    two_accounts_edit_one_document_and_converge,
    a_viewer_reads_and_watches_but_never_writes_or_compacts,
    presence_is_stamped_with_the_senders_account,
    a_removed_member_is_cut_off_at_once,
    a_lowered_role_and_an_archived_space_end_the_connection,
    a_disabled_account_is_cut_off,
    a_change_made_elsewhere_is_found_at_the_next_ping,
    connections_are_counted_per_space_and_in_all,
    a_team_space_has_its_own_live_bytes_cap,
    a_protocol_two_agent_sees_the_frames_it_always_saw,
);

// The default ping (30 s) never comes within these: only the check of the frame itself can end the connection.
on_every_backend!(
    default_ping,
    slow_limits, a_write_is_checked_against_the_database_before_any_ping,);

// ---- the scenarios ----

/// Ana (owner) and ben (editor) edit one document at once: each update is acked to its sender and reaches the other, and
/// both end with the same log, in the same order, which a third connection reads from the start.
async fn two_accounts_edit_one_document_and_converge(w: &World) {
    let space = w.create("Studio", "ana").await;
    w.join(space, "ana", "ben", "editor").await;
    let mut ana = w.ready_as("ana", space, SpaceRole::Owner).await;
    let mut ben = w.ready_as("ben", space, SpaceRole::Editor).await;
    let doc = w.follow(&mut ana, space).await;
    assert_eq!(w.follow(&mut ben, space).await, doc, "one document per space and app");

    // Five updates each, sent without waiting for an answer.
    for i in 1..=5u64 {
        send(&mut ana, ClientFrame::Update { doc, client_seq: i, bytes: format!("a{i}").into_bytes() }).await;
        send(&mut ben, ClientFrame::Update { doc, client_seq: i, bytes: format!("b{i}").into_bytes() }).await;
    }
    // What each one learns: its own updates from the acks, the other's from the log.
    let mut views = Vec::new();
    for (ws, mine) in [(&mut ana, "a"), (&mut ben, "b")] {
        let mut seen: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
        while seen.len() < 10 {
            match frame(ws).await {
                ServerFrame::Ack { client_seq, seq, .. } => {
                    seen.insert(seq, format!("{mine}{client_seq}").into_bytes());
                }
                ServerFrame::Update { seq, bytes, .. } => {
                    seen.insert(seq, bytes);
                }
                // The log is long: a device is asked for a snapshot; nobody answers here.
                ServerFrame::Compact { .. } => {}
                other => panic!("{other:?}"),
            }
        }
        views.push(seen);
    }
    assert_eq!(views[0], views[1], "both end with the same log");
    assert_eq!(views[0].keys().copied().collect::<Vec<_>>(), (1..=10).collect::<Vec<_>>());
    // Each one's own updates keep their order in the shared log.
    for author in ["a", "b"] {
        let theirs: Vec<&Vec<u8>> = views[0].values().filter(|bytes| bytes.starts_with(author.as_bytes())).collect();
        let want: Vec<Vec<u8>> = (1..=5).map(|i| format!("{author}{i}").into_bytes()).collect();
        assert_eq!(theirs, want.iter().collect::<Vec<_>>(), "{author}'s updates in order");
    }
    // A third connection of ana's reads the same log from the start.
    let mut third = w.ready_as("ana", space, SpaceRole::Owner).await;
    let read: BTreeMap<u64, Vec<u8>> = subscribe(&mut third, doc, 0).await.into_iter().collect();
    assert_eq!(read, views[0]);
}

/// A viewer follows the document and sends presence, and nothing else: its `create`, `update` and `snapshot` are refused
/// with `read_only` (the connection stays), nothing it sends is stored, and it is never asked to compact.
async fn a_viewer_reads_and_watches_but_never_writes_or_compacts(w: &World) {
    let space = w.create("Studio", "ana").await;
    w.join(space, "ana", "ben", "editor").await;
    w.join(space, "ana", "cy", "viewer").await;
    let mut ana = w.ready_as("ana", space, SpaceRole::Owner).await;
    let doc = w.follow(&mut ana, space).await;
    let mut cy = w.ready_as("cy", space, SpaceRole::Viewer).await;

    // Refused, and told so: the answer names the document (and the update).
    send(&mut cy, ClientFrame::Create { doc, kind: DocKind::Recents { app: app() } }).await;
    assert_eq!(error_code(&frame(&mut cy).await), ("read_only", Some(doc), None));
    // Allowed: subscribe.
    assert!(subscribe(&mut cy, doc, 0).await.is_empty());
    send(&mut cy, ClientFrame::Update { doc, client_seq: 9, bytes: b"cy was here".to_vec() }).await;
    assert_eq!(error_code(&frame(&mut cy).await), ("read_only", Some(doc), Some(9)));
    send(&mut cy, ClientFrame::Snapshot { doc, upto: 1, manifest: ManifestId::from_bytes([1; 32]) }).await;
    assert_eq!(error_code(&frame(&mut cy).await), ("read_only", Some(doc), None));
    let node = NodeId::generate();
    send(&mut cy, ClientFrame::CreateDocument { doc: node_doc(space, node), node, app: app(), snapshot: ManifestId::from_bytes([1; 32]) }).await;
    assert_eq!(error_code(&frame(&mut cy).await), ("read_only", Some(node_doc(space, node)), None));

    // Allowed: presence, both ways, with the sender's account.
    send(&mut cy, ClientFrame::Presence { doc, bytes: b"cursor".to_vec() }).await;
    assert_eq!(frame(&mut ana).await, ServerFrame::PresenceFrom { doc, bytes: b"cursor".to_vec(), user: w.user("cy").id.0 });
    send(&mut ana, ClientFrame::Presence { doc, bytes: b"owner".to_vec() }).await;
    assert_eq!(frame(&mut cy).await, ServerFrame::PresenceFrom { doc, bytes: b"owner".to_vec(), user: w.user("ana").id.0 });

    // The editor's updates reach the viewer. The fourth makes the log long: ana, who sent it, is asked to compact.
    for i in 1..=4u64 {
        send(&mut ana, ClientFrame::Update { doc, client_seq: i, bytes: format!("a{i}").into_bytes() }).await;
    }
    for i in 1..=4u64 {
        assert!(matches!(frame(&mut ana).await, ServerFrame::Ack { client_seq, .. } if client_seq == i));
    }
    assert_eq!(frame(&mut ana).await, ServerFrame::Compact { doc, upto: 4 });
    for i in 1..=4u64 {
        assert!(matches!(frame(&mut cy).await, ServerFrame::Update { seq, .. } if seq == i));
    }
    // The request to ana timed out (100 ms): another device is asked next. A viewer that catches up now is not, however
    // long the log is; an editor is.
    tokio::time::sleep(Duration::from_millis(300)).await;
    send(&mut cy, ClientFrame::Subscribe { doc, after: 4, at: None }).await;
    assert_eq!(frame(&mut cy).await, ServerFrame::Subscribed { doc, head: 4 });
    quiet(&mut cy).await;
    let mut ben = w.ready_as("ben", space, SpaceRole::Editor).await;
    send(&mut ben, ClientFrame::Subscribe { doc, after: 0, at: None }).await;
    let mut bytes = Vec::new();
    loop {
        match frame(&mut ben).await {
            ServerFrame::Update { bytes: b, .. } => bytes.push(b),
            ServerFrame::Subscribed { head, .. } => {
                assert_eq!(head, 4);
                break;
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(bytes, vec![b"a1".to_vec(), b"a2".to_vec(), b"a3".to_vec(), b"a4".to_vec()], "nothing the viewer sent was stored");
    assert_eq!(frame(&mut ben).await, ServerFrame::Compact { doc, upto: 4 }, "an editor that caught up is asked");
}

/// The account in `presence_from` is the sender's, whatever the bytes claim, and the sender's own other connections are
/// told it too.
async fn presence_is_stamped_with_the_senders_account(w: &World) {
    let space = w.create("Studio", "ana").await;
    w.join(space, "ana", "ben", "editor").await;
    let mut ana = w.ready_as("ana", space, SpaceRole::Owner).await;
    let doc = w.follow(&mut ana, space).await;
    let mut ben = w.ready_as("ben", space, SpaceRole::Editor).await;
    assert_eq!(w.follow(&mut ben, space).await, doc);
    let mut ben2 = w.ready_as("ben", space, SpaceRole::Editor).await;
    assert_eq!(w.follow(&mut ben2, space).await, doc);

    // Ben's presence claims, in its bytes, to be ana's.
    let mut claim = w.user("ana").id.0.as_bytes().to_vec();
    claim.extend(b"I am ana");
    send(&mut ben, ClientFrame::Presence { doc, bytes: claim.clone() }).await;
    let ben_id = w.user("ben").id.0;
    assert_eq!(frame(&mut ana).await, ServerFrame::PresenceFrom { doc, bytes: claim.clone(), user: ben_id });
    assert_eq!(frame(&mut ben2).await, ServerFrame::PresenceFrom { doc, bytes: claim, user: ben_id }, "also on ben's other connection");
    quiet(&mut ben).await;
}

/// Removing a member closes that member's connections at once (within a second, long before the next ping), with the
/// close reason `access_changed`; the other members' connections stay, and the member cannot connect again.
async fn a_removed_member_is_cut_off_at_once(w: &World) {
    let space = w.create("Studio", "ana").await;
    w.join(space, "ana", "ben", "editor").await;
    w.join(space, "ana", "cy", "editor").await;
    let mut ana = w.ready_as("ana", space, SpaceRole::Owner).await;
    let doc = w.follow(&mut ana, space).await;
    let mut ben = w.ready_as("ben", space, SpaceRole::Editor).await;
    let mut ben2 = w.ready_as("ben", space, SpaceRole::Editor).await;
    let mut cy = w.ready_as("cy", space, SpaceRole::Editor).await;
    w.follow(&mut ben, space).await;

    let res = w.call("ana", Method::DELETE, &format!("/spaces/{space}/members/{}", w.user("ben").id.0), None).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    for socket in [&mut ben, &mut ben2] {
        let (frames, closed, took) = until_closed(socket, Duration::from_secs(1)).await;
        assert_eq!(closed, Some((ACCESS_CHANGED_CLOSE, ACCESS_CHANGED.to_owned())));
        assert_eq!(frames.len(), 1);
        assert_eq!(error_code(&frames[0]).0, ACCESS_CHANGED);
        assert!(took < Duration::from_secs(1), "closed after {took:?}");
    }
    assert_eq!(w.open("ben", Some(space)).await.err(), Some(StatusCode::NOT_FOUND), "not a member any more");
    // Everyone else is untouched, and still works.
    send(&mut ana, ClientFrame::Update { doc, client_seq: 1, bytes: b"still here".to_vec() }).await;
    assert!(matches!(frame(&mut ana).await, ServerFrame::Ack { .. }));
    quiet(&mut cy).await;
}

/// A role lowered or a space archived ends the connections it concerns at once; the next connection gets the role it has
/// now, and a viewer's write is refused. (An archived space caps every member at viewer.)
async fn a_lowered_role_and_an_archived_space_end_the_connection(w: &World) {
    let space = w.create("Studio", "ana").await;
    w.join(space, "ana", "ben", "editor").await;
    let mut ana = w.ready_as("ana", space, SpaceRole::Owner).await;
    let doc = w.follow(&mut ana, space).await;
    let mut ben = w.ready_as("ben", space, SpaceRole::Editor).await;

    let res = w.call("ana", Method::PATCH, &format!("/spaces/{space}/members/{}", w.user("ben").id.0), Some(json!({ "role": "viewer" }))).await;
    assert!(res.status().is_success(), "{}", res.status());
    let (_, closed, took) = until_closed(&mut ben, Duration::from_secs(1)).await;
    assert_eq!(closed, Some((ACCESS_CHANGED_CLOSE, ACCESS_CHANGED.to_owned())));
    assert!(took < Duration::from_secs(1));
    let mut ben = w.ready_as("ben", space, SpaceRole::Viewer).await;
    assert!(subscribe(&mut ben, doc, 0).await.is_empty());
    send(&mut ben, ClientFrame::Update { doc, client_seq: 1, bytes: b"x".to_vec() }).await;
    assert_eq!(error_code(&frame(&mut ben).await), ("read_only", Some(doc), Some(1)));
    // The owner's connection did not end by it.
    send(&mut ana, ClientFrame::Update { doc, client_seq: 1, bytes: b"owner".to_vec() }).await;
    assert!(matches!(frame(&mut ana).await, ServerFrame::Ack { .. }));
    assert!(matches!(frame(&mut ben).await, ServerFrame::Update { seq: 1, .. }), "the viewer reads it");

    // Archiving ends every connection (their role is the capped one now); new ones read.
    let res = w.call("ana", Method::POST, &format!("/spaces/{space}/archive"), Some(json!({ "password": PASSWORD }))).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    let (_, closed, took) = until_closed(&mut ana, Duration::from_secs(1)).await;
    assert_eq!(closed, Some((ACCESS_CHANGED_CLOSE, ACCESS_CHANGED.to_owned())));
    assert!(took < Duration::from_secs(1));
    // Ben was a viewer already: his role did not change, so his connection stays.
    quiet(&mut ben).await;
    let mut ana = w.ready_as("ana", space, SpaceRole::Viewer).await;
    assert_eq!(subscribe(&mut ana, doc, 0).await.len(), 1);
    send(&mut ana, ClientFrame::Update { doc, client_seq: 2, bytes: b"archived".to_vec() }).await;
    assert_eq!(error_code(&frame(&mut ana).await), ("read_only", Some(doc), Some(2)), "an archived space is read-only for its owner too");
    send(&mut ana, ClientFrame::Create { doc, kind: DocKind::Recents { app: app() } }).await;
    assert_eq!(error_code(&frame(&mut ana).await).0, "read_only");

    // And back: unarchived, the connections of the capped roles end, and the owner is an owner again.
    let res = w.call("ana", Method::POST, &format!("/spaces/{space}/unarchive"), Some(json!({ "password": PASSWORD }))).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    let (_, closed, _) = until_closed(&mut ana, Duration::from_secs(1)).await;
    assert_eq!(closed.map(|(_, reason)| reason), Some(ACCESS_CHANGED.to_owned()));
    w.ready_as("ana", space, SpaceRole::Owner).await;
}

/// A disabled account loses its live connections (to its team spaces and its own) at once.
async fn a_disabled_account_is_cut_off(w: &World) {
    let space = w.create("Studio", "ana").await;
    w.join(space, "ana", "dee", "editor").await;
    let mut dee = w.ready_as("dee", space, SpaceRole::Editor).await;
    let mut own = w.open("dee", None).await.unwrap();
    send(&mut own, ClientFrame::Hello { min: 1, max: 2 }).await;
    assert!(matches!(frame(&mut own).await, ServerFrame::Welcome { protocol: 2, .. }));

    let res = w.call("boss", Method::POST, &format!("/admin/users/{}/disable", w.user("dee").id.0), Some(World::confirmed(json!({})))).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    for socket in [&mut dee, &mut own] {
        let (_, closed, took) = until_closed(socket, Duration::from_secs(1)).await;
        assert!(closed.is_some() || took < Duration::from_secs(1));
        assert!(took < Duration::from_secs(1), "closed after {took:?}");
    }
    assert_eq!(w.open("dee", Some(space)).await.err(), Some(StatusCode::UNAUTHORIZED), "the account cannot connect any more");
}

/// A change nobody told this process about (the command line, or a second server on the database; here the store is
/// changed behind the server's back) is found at the next ping (a quarter of a second here, 30 s by default).
async fn a_change_made_elsewhere_is_found_at_the_next_ping(w: &World) {
    let space = w.create("Studio", "ana").await;
    w.join(space, "ana", "ben", "editor").await;
    w.join(space, "ana", "cy", "editor").await;
    let mut ana = w.ready_as("ana", space, SpaceRole::Owner).await;
    w.follow(&mut ana, space).await;
    let mut ben = w.ready_as("ben", space, SpaceRole::Editor).await;
    let mut cy = w.ready_as("cy", space, SpaceRole::Editor).await;

    // Removed behind the server's back: the next ping ends the connection, with the reason.
    w.meta.remove_member(&SpaceActor::Cli, space, w.user("ben").id, NOW).await.unwrap();
    let (frames, closed, took) = until_closed(&mut ben, Duration::from_secs(2)).await;
    assert_eq!(closed, Some((ACCESS_CHANGED_CLOSE, ACCESS_CHANGED.to_owned())));
    assert_eq!(frames.len(), 1);
    assert!(took < Duration::from_secs(2), "{took:?}");

    // Lowered behind the server's back: a connection opened as an editor ends the same way.
    w.meta.change_member_role(&SpaceActor::Cli, space, w.user("cy").id, SpaceRole::Viewer, NOW).await.unwrap();
    let (_, closed, _) = until_closed(&mut cy, Duration::from_secs(2)).await;
    assert_eq!(closed, Some((ACCESS_CHANGED_CLOSE, ACCESS_CHANGED.to_owned())));
    w.ready_as("cy", space, SpaceRole::Viewer).await;
    // The owner's connection is untouched by either.
    quiet(&mut ana).await;
}

/// With no ping in reach, a write is still checked against the database when it arrives: a member removed or a role
/// lowered by another process cannot write, the connection ends, and nothing is stored.
async fn a_write_is_checked_against_the_database_before_any_ping(w: &World) {
    let space = w.create("Studio", "ana").await;
    w.join(space, "ana", "ben", "editor").await;
    w.join(space, "ana", "cy", "editor").await;
    let mut ana = w.ready_as("ana", space, SpaceRole::Owner).await;
    let doc = w.follow(&mut ana, space).await;
    let mut ben = w.ready_as("ben", space, SpaceRole::Editor).await;
    let mut cy = w.ready_as("cy", space, SpaceRole::Editor).await;
    w.follow(&mut ben, space).await;
    w.follow(&mut cy, space).await;

    w.meta.remove_member(&SpaceActor::Cli, space, w.user("ben").id, NOW).await.unwrap();
    send(&mut ben, ClientFrame::Update { doc, client_seq: 1, bytes: b"too late".to_vec() }).await;
    let (frames, closed, _) = until_closed(&mut ben, Duration::from_secs(2)).await;
    assert_eq!(error_code(&frames[0]).0, ACCESS_CHANGED);
    assert_eq!(closed.map(|(code, _)| code), Some(ACCESS_CHANGED_CLOSE));

    w.meta.change_member_role(&SpaceActor::Cli, space, w.user("cy").id, SpaceRole::Viewer, NOW).await.unwrap();
    send(&mut cy, ClientFrame::Update { doc, client_seq: 1, bytes: b"too late".to_vec() }).await;
    let (frames, closed, _) = until_closed(&mut cy, Duration::from_secs(2)).await;
    assert_eq!(error_code(&frames[0]).0, ACCESS_CHANGED);
    assert_eq!(closed.map(|(code, _)| code), Some(ACCESS_CHANGED_CLOSE));

    let mut again = w.ready_as("ana", space, SpaceRole::Owner).await;
    assert!(subscribe(&mut again, doc, 0).await.is_empty(), "neither update was stored");
}

/// A device holds 4 connections to a space and 32 in all (the registry counts them per process).
async fn connections_are_counted_per_space_and_in_all(w: &World) {
    let mut spaces = Vec::new();
    for i in 0..9 {
        spaces.push(w.create(&format!("Team {i}"), "ana").await);
    }
    let mut held: HashMap<SpaceId, Vec<Ws>> = HashMap::new();
    for space in &spaces[..8] {
        for _ in 0..4 {
            held.entry(*space).or_default().push(w.ready_as("ana", *space, SpaceRole::Owner).await);
        }
    }
    // A fifth to one space, and a first one to a ninth, are both over a limit: 4 per space, 32 in all.
    assert_eq!(w.open("ana", Some(spaces[0])).await.err(), Some(StatusCode::TOO_MANY_REQUESTS), "5 to one space");
    assert_eq!(w.open("ana", Some(spaces[8])).await.err(), Some(StatusCode::TOO_MANY_REQUESTS), "33 in all");
    // A place frees up when a connection ends.
    let mut one = held.get_mut(&spaces[0]).unwrap().pop().unwrap();
    one.close(None).await.unwrap();
    drop(one);
    let mut again = None;
    for _ in 0..50 {
        match w.open("ana", Some(spaces[8])).await {
            Ok(ws) => {
                again = Some(ws);
                break;
            }
            Err(StatusCode::TOO_MANY_REQUESTS) => tokio::time::sleep(Duration::from_millis(100)).await,
            Err(other) => panic!("{other}"),
        }
    }
    assert!(again.is_some(), "a place freed up when a connection ended");
}

/// The cap on what a space's live documents hold is per space, and a team space has its own (here 10,000 bytes; the
/// personal spaces keep theirs): an update over it is refused with `quota_exceeded`, in the team space only.
async fn a_team_space_has_its_own_live_bytes_cap(w: &World) {
    let space = w.create("Studio", "ana").await;
    let mut team = w.ready_as("ana", space, SpaceRole::Owner).await;
    let doc = w.follow(&mut team, space).await;
    send(&mut team, ClientFrame::Update { doc, client_seq: 1, bytes: vec![1; 6000] }).await;
    assert!(matches!(frame(&mut team).await, ServerFrame::Ack { seq: 1, .. }));
    send(&mut team, ClientFrame::Update { doc, client_seq: 2, bytes: vec![2; 6000] }).await;
    assert_eq!(error_code(&frame(&mut team).await), ("quota_exceeded", Some(doc), Some(2)));

    // The same updates in ana's personal space are stored.
    let personal = w.user("ana").space;
    let mut own = w.ready_as("ana", personal, SpaceRole::Owner).await;
    let doc = w.follow(&mut own, personal).await;
    for i in 1..=2u64 {
        send(&mut own, ClientFrame::Update { doc, client_seq: i, bytes: vec![1; 6000] }).await;
        assert!(matches!(frame(&mut own).await, ServerFrame::Ack { seq, .. } if seq == i));
    }
}

/// An agent of protocol 2 sees the bytes it always saw, with or without the header; a team space is not served to it; a
/// stranger is turned away before the upgrade.
async fn a_protocol_two_agent_sees_the_frames_it_always_saw(w: &World) {
    let personal = w.user("ana").space;
    let epoch = w.meta.live_epoch(personal).await.unwrap();
    let mut welcome = vec![0, 2, 16];
    welcome.extend(epoch.as_uuid().as_bytes());
    for named in [None, Some(personal)] {
        let mut ws = w.open("ana", named).await.unwrap();
        send(&mut ws, ClientFrame::Hello { min: 1, max: 2 }).await;
        assert_eq!(raw(&mut ws).await, welcome, "welcome, byte for byte (header {named:?})");
    }
    // Presence between two connections of the same account is the old `presence` frame, byte for byte.
    let doc = recents_doc(personal, &app());
    let mut one = w.open("ana", None).await.unwrap();
    let mut two = w.open("ana", Some(personal)).await.unwrap();
    for ws in [&mut one, &mut two] {
        send(ws, ClientFrame::Hello { min: 1, max: 2 }).await;
        assert_eq!(raw(ws).await, welcome);
        send(ws, ClientFrame::Create { doc, kind: DocKind::Recents { app: app() } }).await;
        let mut created = vec![1, 16];
        created.extend(doc.as_uuid().as_bytes());
        assert_eq!(raw(ws).await, created);
        send(ws, ClientFrame::Subscribe { doc, after: 0, at: None }).await;
        let mut subscribed = vec![3, 16];
        subscribed.extend(doc.as_uuid().as_bytes());
        subscribed.push(0);
        assert_eq!(raw(ws).await, subscribed);
    }
    send(&mut one, ClientFrame::Presence { doc, bytes: vec![1, 2, 3] }).await;
    let mut presence = vec![8, 16];
    presence.extend(doc.as_uuid().as_bytes());
    presence.extend([3, 1, 2, 3]);
    assert_eq!(raw(&mut two).await, presence);

    // A protocol 3 agent in the same space gets the new frames, with the personal space and an owner's role.
    let mut three = w.open("ana", Some(personal)).await.unwrap();
    send(&mut three, ClientFrame::Hello { min: 1, max: 3 }).await;
    assert_eq!(frame(&mut three).await, ServerFrame::WelcomeSpace { protocol: 3, epoch, space: personal, role: SpaceRole::Owner });
    subscribe(&mut three, doc, 0).await;
    send(&mut one, ClientFrame::Presence { doc, bytes: vec![9] }).await;
    assert_eq!(frame(&mut three).await, ServerFrame::PresenceFrom { doc, bytes: vec![9], user: w.user("ana").id.0 });
    assert_eq!(raw(&mut two).await[..2], [8, 16], "the protocol 2 connection still gets `presence`");

    // A team space needs protocol 3.
    let space = w.create("Studio", "ana").await;
    let mut ws = w.open("ana", Some(space)).await.unwrap();
    send(&mut ws, ClientFrame::Hello { min: 1, max: 2 }).await;
    let (frames, closed, _) = until_closed(&mut ws, Duration::from_secs(2)).await;
    assert_eq!(error_code(&frames[0]).0, PROTOCOL_MISMATCH);
    assert_eq!(closed, None, "closed without a reason");
    // A stranger is refused before the upgrade, as for a space that does not exist; with no header it has its own.
    assert_eq!(w.open("eve", Some(space)).await.err(), Some(StatusCode::NOT_FOUND));
    assert_eq!(w.open("eve", Some(SpaceId::generate())).await.err(), Some(StatusCode::NOT_FOUND));
    assert!(w.open("eve", None).await.is_ok());
}
