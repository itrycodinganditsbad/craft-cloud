//! The space layer (`craft-space` header, `require_space`, roles per route, `read_only`) over HTTP: a real server on a random
//! loopback port with a team space and accounts in every state that matters. Runs on SQLite and disk, and on Postgres and
//! disk when `CRAFT_CLOUD_TEST_PG` is set (`CRAFT_CLOUD_REQUIRE_BACKENDS` turns a missing backend into a failure).

use std::sync::Arc;
use std::time::{Duration, Instant};

use craft_cloud_meta::{Meta, Role, SpaceActor, UserRecord};
use craft_cloud_proto::api::{EPOCH_HEADER, ErrorBody, InfoResponse, LoginResponse, SPACE_HEADER, SpaceResponse, SpaceRole};
use craft_cloud_proto::{ChunkId, NodeId, SpaceId, SpaceName};
use craft_cloud_server::AppState;
use craft_cloud_server::auth::hash_password;
use craft_cloud_store::Store;
use craft_cloud_store::disk::DiskBackend;
use reqwest::{Client, Method, StatusCode};
use serde_json::json;

const PASSWORD: &str = "correct horse battery";
const NOW: i64 = 1_700_000_000_000;

struct World {
    base: String,
    client: Client,
    meta: Meta,
    state: Arc<AppState>,
    url: String,
    /// The accounts, by name.
    users: Vec<(&'static str, UserRecord, String)>,
    team: SpaceId,
    /// A second team space, archived, where `ben` is an editor and `ana` an owner.
    archived: SpaceId,
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

impl World {
    fn user(&self, name: &str) -> &UserRecord {
        &self.users.iter().find(|(n, ..)| *n == name).unwrap().1
    }

    fn token(&self, name: &str) -> &str {
        &self.users.iter().find(|(n, ..)| *n == name).unwrap().2
    }

    /// A request as `who`, naming `space` in the header if given.
    async fn send(&self, who: &str, method: Method, path: &str, space: Option<SpaceId>, body: Body) -> reqwest::Response {
        let mut request = self.client.request(method, format!("{}{path}", self.base)).bearer_auth(self.token(who));
        if let Some(space) = space {
            request = request.header(SPACE_HEADER, space.to_string());
        }
        match body {
            Body::None => {}
            Body::Json(value) => request = request.json(&value),
            Body::Bytes(bytes) => request = request.body(bytes),
        }
        request.send().await.unwrap()
    }

    async fn raw(&self) -> sqlx::AnyPool {
        sqlx::any::install_default_drivers();
        sqlx::any::AnyPoolOptions::new().max_connections(2).connect(&self.url).await.unwrap()
    }

    async fn set_user_column(&self, name: &str, column: &str) {
        let pool = self.raw().await;
        sqlx::query(sqlx::AssertSqlSafe(format!("UPDATE users SET {column} = 1 WHERE id = $1")))
            .bind(self.user(name).id.0.as_bytes().to_vec())
            .execute(&pool)
            .await
            .unwrap();
    }
}

enum Body {
    None,
    Json(serde_json::Value),
    Bytes(Vec<u8>),
}

async fn build(url: String, meta: Meta, dirs: Vec<tempfile::TempDir>, pg: Option<PgGuard>) -> World {
    let blobs = tempfile::tempdir().unwrap();
    let store = Store::new(DiskBackend::new(blobs.path()).unwrap());
    let mut dirs = dirs;
    dirs.push(blobs);
    let state = Arc::new(AppState::new(meta.clone(), store, vec![]).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/api/v1", listener.local_addr().unwrap());
    let app = craft_cloud_server::api::router(state.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>()).await;
    });
    let client = Client::new();

    let hash = hash_password(PASSWORD).unwrap();
    let mut users = Vec::new();
    // ana owns the team space; ben, cy are editor and viewer; dee has a pending invitation; eve is nowhere; fay is disabled
    // after joining; gus is removed after joining; hal's device is signed out.
    for name in ["ana", "ben", "cy", "dee", "eve", "fay", "gus", "hal"] {
        let record = meta.create_user(name, &hash, Role::Member, NOW).await.unwrap();
        let login = client
            .post(format!("{base}/auth/login"))
            .json(&json!({ "username": name, "password": PASSWORD, "device_name": "laptop" }))
            .send()
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::OK);
        let login: LoginResponse = login.json().await.unwrap();
        if name == "hal" {
            meta.revoke_device(record.id, login.device).await.unwrap();
        }
        users.push((name, record, login.token));
    }
    let id = |name: &str| users.iter().find(|(n, ..)| *n == name).unwrap().1.id;
    let name = |text: &str| SpaceName::new(text).unwrap();
    let team = meta.create_team_space(&SpaceActor::Cli, &name("Studio"), id("ana"), None, NOW).await.unwrap();
    for (who, role) in
        [("ben", SpaceRole::Editor), ("cy", SpaceRole::Viewer), ("fay", SpaceRole::Editor), ("gus", SpaceRole::Editor), ("hal", SpaceRole::Editor)]
    {
        meta.invite_member(&SpaceActor::Cli, team, id(who), role, NOW + 1).await.unwrap();
    }
    meta.invite_member(&SpaceActor::User { user: id("ana"), device: None }, team, id("dee"), SpaceRole::Editor, NOW + 1).await.unwrap();
    let archived = meta.create_team_space(&SpaceActor::Cli, &name("Old"), id("ana"), None, NOW).await.unwrap();
    meta.invite_member(&SpaceActor::Cli, archived, id("ben"), SpaceRole::Editor, NOW + 1).await.unwrap();

    let world = World { base, client, meta, state, url, users, team, archived, _dirs: dirs, _pg: pg };
    world.set_user_column("fay", "disabled_ms").await;
    world.set_user_column("gus", "removed_ms").await;
    let pool = world.raw().await;
    sqlx::query("UPDATE spaces SET archived_ms = 1 WHERE id = $1").bind(world.archived.as_uuid().as_bytes().to_vec()).execute(&pool).await.unwrap();
    world
}

async fn sqlite_disk() -> World {
    let db = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}?mode=rwc", db.path().join("meta.db").display());
    let meta = Meta::open(&url).await.unwrap();
    build(url, meta, vec![db], None).await
}

async fn postgres_disk() -> Option<World> {
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
    Some(build(url, meta, vec![], Some(guard)).await)
}

macro_rules! on_every_backend {
    ($($scenario:ident),* $(,)?) => {
        mod on_sqlite {
            $( #[tokio::test(flavor = "multi_thread", worker_threads = 4)] async fn $scenario() { let w = super::sqlite_disk().await; super::$scenario(&w).await; } )*
        }
        mod on_postgres {
            $( #[tokio::test(flavor = "multi_thread", worker_threads = 4)] async fn $scenario() {
                let Some(w) = super::postgres_disk().await else { eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set"); return };
                super::$scenario(&w).await;
            } )*
        }
    };
}

on_every_backend!(
    no_header_is_the_personal_space_and_every_answer_echoes_the_space,
    the_header_is_checked,
    every_route_by_role_in_a_team_space,
    every_route_in_a_personal_space,
    viewers_read_and_cannot_write,
    a_chunk_of_one_space_is_not_readable_through_another,
    a_long_poll_on_a_team_space_ends_with_404_when_the_member_is_removed,
    live_documents_of_a_team_space_are_served_to_its_members,
    an_old_protocol_client_sees_the_server_it_always_saw,
);

// ---- helpers ----

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Seen {
    /// The route ran (whatever it then answered).
    Through,
    /// `403 read_only`.
    ReadOnly,
    /// `404 not_found` for the space.
    NoSpace,
    /// `401`.
    Unauthorized,
}

async fn seen(res: reqwest::Response) -> (Seen, StatusCode, Option<String>) {
    let status = res.status();
    let echo = res.headers().get(SPACE_HEADER).map(|v| v.to_str().unwrap().to_owned());
    let bytes = res.bytes().await.unwrap();
    let error: Option<ErrorBody> = serde_json::from_slice(&bytes).ok();
    let seen = match (status, error) {
        (StatusCode::UNAUTHORIZED, _) => Seen::Unauthorized,
        (StatusCode::FORBIDDEN, Some(e)) if e.error == "read_only" => Seen::ReadOnly,
        (StatusCode::NOT_FOUND, Some(e)) if e.error == "not_found" && e.message == "no such space" => Seen::NoSpace,
        _ => Seen::Through,
    };
    (seen, status, echo)
}

struct Route {
    name: &'static str,
    method: Method,
    path: String,
    /// The least role the route needs.
    min: SpaceRole,
    body: fn() -> Body,
}

fn routes() -> Vec<Route> {
    let (chunk, node) = (ChunkId::from_bytes([7; 32]), NodeId::generate());
    let route = |name, method, path: String, min, body: fn() -> Body| Route { name, method, path, min, body };
    use SpaceRole::{Editor, Owner, Viewer};
    vec![
        route("GET /space", Method::GET, "/space".into(), Viewer, || Body::None),
        route("GET /tree", Method::GET, "/tree".into(), Viewer, || Body::None),
        route("GET /changes", Method::GET, "/changes?since=0".into(), Viewer, || Body::None),
        route("GET /chunks/{id}", Method::GET, format!("/chunks/{chunk}"), Viewer, || Body::None),
        route("GET /manifests/{id}", Method::GET, format!("/manifests/{chunk}"), Viewer, || Body::None),
        route("POST /chunks/missing", Method::POST, "/chunks/missing".into(), Editor, || Body::Json(json!({ "ids": [] }))),
        route("PUT /chunks/{id}", Method::PUT, format!("/chunks/{chunk}"), Editor, || Body::Bytes(b"not the chunk".to_vec())),
        route("PUT /manifests/{id}", Method::PUT, format!("/manifests/{chunk}"), Editor, || Body::Bytes(b"not a manifest".to_vec())),
        route("POST /commit", Method::POST, "/commit".into(), Editor, || Body::Json(json!({ "ops": [] }))),
        route("POST /saves", Method::POST, "/saves".into(), Editor, || Body::Json(json!({}))),
        route("GET /nodes/{id}/history", Method::GET, format!("/nodes/{node}/history"), Viewer, || Body::None),
        route("POST /nodes/{id}/restore", Method::POST, format!("/nodes/{node}/restore"), Editor, || Body::Json(json!({}))),
        route("GET /trash", Method::GET, "/trash".into(), Viewer, || Body::None),
        route("POST /trash/{id}/restore", Method::POST, format!("/trash/{node}/restore"), Editor, || Body::None),
        route("DELETE /trash/{id}", Method::DELETE, format!("/trash/{node}"), Owner, || Body::None),
    ]
}

// ---- scenarios ----

async fn no_header_is_the_personal_space_and_every_answer_echoes_the_space(w: &World) {
    let ana = w.user("ana");
    // Without the header: the personal space, echoed, with its epoch, exactly the body a Phase 1 server answers.
    let res = w.send("ana", Method::GET, "/space", None, Body::None).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers().get(SPACE_HEADER).unwrap().to_str().unwrap(), ana.space.to_string());
    let personal_epoch = w.meta.space_info(ana.space).await.unwrap().epoch.to_string();
    assert_eq!(res.headers().get(EPOCH_HEADER).unwrap().to_str().unwrap(), personal_epoch);
    let raw: serde_json::Value = res.json().await.unwrap();
    let mut keys: Vec<_> = raw.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, ["epoch", "kind", "role", "root", "seq", "space", "username"], "the old fields, and `kind` and `role` appended");
    assert_eq!(raw["kind"], "personal");
    assert_eq!(raw["role"], "owner");
    let body: SpaceResponse = serde_json::from_value(raw).unwrap();
    assert_eq!((body.space, body.username.as_str()), (ana.space, "ana"));

    // Naming the personal space is the same answer.
    let named: SpaceResponse = w.send("ana", Method::GET, "/space", Some(ana.space), Body::None).await.json().await.unwrap();
    assert_eq!(named, body);

    // A team space: its own ID, root and epoch, echoed.
    let res = w.send("ana", Method::GET, "/space", Some(w.team), Body::None).await;
    assert_eq!(res.headers().get(SPACE_HEADER).unwrap().to_str().unwrap(), w.team.to_string());
    let team_epoch = w.meta.space_info(w.team).await.unwrap().epoch.to_string();
    assert_ne!(team_epoch, personal_epoch);
    assert_eq!(res.headers().get(EPOCH_HEADER).unwrap().to_str().unwrap(), team_epoch);
    let team: SpaceResponse = res.json().await.unwrap();
    assert_eq!(team.space, w.team);
    assert_eq!(team.root, w.meta.space_info(w.team).await.unwrap().root);

    // Routes that are about the account answer about the account, and ignore the header, even a bad one.
    for path in ["/devices", "/account", "/account/usage"] {
        let res = w.client.get(format!("{}{path}", w.base)).bearer_auth(w.token("ana")).header(SPACE_HEADER, "not a space").send().await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{path}");
        assert!(res.headers().get(SPACE_HEADER).is_none(), "{path} is not space-scoped");
    }
}

async fn the_header_is_checked(w: &World) {
    let bad = |value: String| {
        let (w, value) = (w, value);
        async move {
            let res = w.client.get(format!("{}/space", w.base)).bearer_auth(w.token("ana")).header(SPACE_HEADER, value).send().await.unwrap();
            let status = res.status();
            let body: ErrorBody = res.json().await.unwrap();
            (status, body.error)
        }
    };
    let team = w.team.to_string();
    for value in ["nope".to_owned(), String::new(), team[..35].to_owned(), format!("{{{team}}}"), format!("urn:uuid:{team}"), team.replace('-', "")] {
        assert_eq!(bad(value.clone()).await, (StatusCode::BAD_REQUEST, "invalid".to_owned()), "{value:?}");
    }
    // Two headers are not one space.
    let res = w
        .client
        .get(format!("{}/space", w.base))
        .bearer_auth(w.token("ana"))
        .header(SPACE_HEADER, &team)
        .header(SPACE_HEADER, w.user("ana").space.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    // Upper case is the same ID.
    let res = w.client.get(format!("{}/space", w.base)).bearer_auth(w.token("ana")).header(SPACE_HEADER, team.to_uppercase()).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    // A space nobody has is 404, echoed like any other answer.
    let unknown = SpaceId::generate();
    let (seen, status, echo) = seen_of(w, "ana", Some(unknown)).await;
    assert_eq!((seen, status, echo), (Seen::NoSpace, StatusCode::NOT_FOUND, Some(unknown.to_string())));
    // ... and it is the same answer as for a space the caller has no access to.
    let (outsider, status, _) = seen_of(w, "eve", Some(w.team)).await;
    assert_eq!((outsider, status), (Seen::NoSpace, StatusCode::NOT_FOUND));
}

async fn seen_of(w: &World, who: &str, space: Option<SpaceId>) -> (Seen, StatusCode, Option<String>) {
    seen(w.send(who, Method::GET, "/space", space, Body::None).await).await
}

/// What a request of `who` to a route that needs `min` is let through as in the team space `Studio` (or the archived one).
fn expected(who: &str, min: SpaceRole, archived: bool) -> Seen {
    let role = match who {
        "ana" => Some(SpaceRole::Owner),
        "ben" => Some(SpaceRole::Editor),
        "cy" => Some(SpaceRole::Viewer),
        // Invited and not accepted, not a member, an account marked for removal.
        "dee" | "eve" | "gus" => None,
        // Disabled account, signed-out device: the device token does not sign in at all.
        "fay" | "hal" => return Seen::Unauthorized,
        other => panic!("{other}"),
    };
    let Some(role) = role else { return Seen::NoSpace };
    let role = if archived { role.min(SpaceRole::Viewer) } else { role };
    if role >= min { Seen::Through } else { Seen::ReadOnly }
}

async fn every_route_by_role_in_a_team_space(w: &World) {
    for (space, archived, who_list) in
        [(w.team, false, &["ana", "ben", "cy", "dee", "eve", "fay", "gus", "hal"][..]), (w.archived, true, &["ana", "ben", "eve"][..])]
    {
        for route in routes() {
            for who in who_list {
                let res = w.send(who, route.method.clone(), &route.path, Some(space), (route.body)()).await;
                let (seen, status, echo) = seen(res).await;
                assert_eq!(
                    seen,
                    expected(who, route.min, archived),
                    "{} as {who} in {} space: {status}",
                    route.name,
                    if archived { "the archived" } else { "the team" }
                );
                if seen != Seen::Unauthorized {
                    assert_eq!(echo, Some(space.to_string()), "{} as {who}: the echo", route.name);
                }
            }
        }
    }
}

async fn every_route_in_a_personal_space(w: &World) {
    let (ana, ben) = (w.user("ana").space, w.user("ben").space);
    for route in routes() {
        // No header, and the header naming the caller's own space: the owner may do everything.
        for named in [None, Some(ana)] {
            let (seen, status, echo) = seen(w.send("ana", route.method.clone(), &route.path, named, (route.body)()).await).await;
            assert_eq!(seen, Seen::Through, "{} as ana, {named:?}: {status}", route.name);
            assert_eq!(echo, Some(ana.to_string()), "{}: the echo is the personal space even without the header", route.name);
        }
        // Another account's personal space is nobody else's: no member rows, 404, the answer for a space that is not there.
        for (who, space) in [("ben", ana), ("ana", ben)] {
            let (seen, ..) = seen(w.send(who, route.method.clone(), &route.path, Some(space), (route.body)()).await).await;
            assert_eq!(seen, Seen::NoSpace, "{} as {who} in the other's personal space", route.name);
        }
    }
}

async fn viewers_read_and_cannot_write(w: &World) {
    let data = b"team file";
    let chunk = ChunkId::from_bytes(*blake3::hash(data).as_bytes());
    // An editor uploads a chunk to the team space; the viewer can read it, and cannot upload another.
    let put = |who: &'static str, bytes: Vec<u8>, id: ChunkId| async move {
        w.send(who, Method::PUT, &format!("/chunks/{id}"), Some(w.team), Body::Bytes(bytes)).await
    };
    assert_eq!(put("ben", data.to_vec(), chunk).await.status(), StatusCode::NO_CONTENT);
    let read = w.send("cy", Method::GET, &format!("/chunks/{chunk}"), Some(w.team), Body::None).await;
    assert_eq!(read.status(), StatusCode::OK);
    assert_eq!(read.bytes().await.unwrap().as_ref(), data);
    let other = b"viewer file";
    let other_id = ChunkId::from_bytes(*blake3::hash(other).as_bytes());
    let refused = put("cy", other.to_vec(), other_id).await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert_eq!(refused.headers().get(SPACE_HEADER).unwrap().to_str().unwrap(), w.team.to_string());
    let body: ErrorBody = refused.json().await.unwrap();
    assert_eq!(body.error, "read_only");
    // Nothing was stored: not even through the editor's view of the space.
    let gone = w.send("ana", Method::GET, &format!("/chunks/{other_id}"), Some(w.team), Body::None).await;
    assert_eq!(gone.status(), StatusCode::NOT_FOUND);
    // An editor may not remove from the trash either; an owner may (here: a node that is not there, which is a plain 404).
    let node = NodeId::generate();
    for (who, expected) in [("ben", Seen::ReadOnly), ("ana", Seen::Through)] {
        let (seen, ..) = seen(w.send(who, Method::DELETE, &format!("/trash/{node}"), Some(w.team), Body::None).await).await;
        assert_eq!(seen, expected, "{who} removing from the trash");
    }
    // The role is read from the database on every request: lowered, the editor is read-only from the next request.
    let (ben, ana) = (w.user("ben").id, w.user("ana").id);
    assert_eq!(put("ben", b"again".to_vec(), ChunkId::from_bytes(*blake3::hash(b"again").as_bytes())).await.status(), StatusCode::NO_CONTENT);
    w.meta.change_member_role(&SpaceActor::User { user: ana, device: None }, w.team, ben, SpaceRole::Viewer, NOW + 9).await.unwrap();
    assert_eq!(put("ben", b"later".to_vec(), ChunkId::from_bytes(*blake3::hash(b"later").as_bytes())).await.status(), StatusCode::FORBIDDEN);
    w.meta.remove_member(&SpaceActor::Cli, w.team, ben, NOW + 10).await.unwrap();
    let (seen, ..) = seen_of(w, "ben", Some(w.team)).await;
    assert_eq!(seen, Seen::NoSpace);
}

async fn a_chunk_of_one_space_is_not_readable_through_another(w: &World) {
    let data = b"only in the personal space";
    let id = ChunkId::from_bytes(*blake3::hash(data).as_bytes());
    let ana_space = w.user("ana").space;
    let up = w.send("ana", Method::PUT, &format!("/chunks/{id}"), None, Body::Bytes(data.to_vec())).await;
    assert_eq!(up.status(), StatusCode::NO_CONTENT);
    // The store has it; the personal space can read it; the team space (which the same account owns) cannot, and the
    // answer is the one for a chunk that is not there.
    let read = |space: Option<SpaceId>| async move { w.send("ana", Method::GET, &format!("/chunks/{id}"), space, Body::None).await };
    assert_eq!(read(None).await.status(), StatusCode::OK);
    assert_eq!(read(Some(ana_space)).await.status(), StatusCode::OK);
    let res = read(Some(w.team)).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let body: ErrorBody = res.json().await.unwrap();
    assert_eq!(body.message, "no such chunk");
    // `chunks/missing` is answered per space as well.
    let missing = |space: Option<SpaceId>| async move {
        let res = w.send("ana", Method::POST, "/chunks/missing", space, Body::Json(json!({ "ids": [id] }))).await;
        let body: craft_cloud_proto::api::MissingResponse = res.json().await.unwrap();
        body.missing.len()
    };
    assert_eq!(missing(None).await, 0);
    assert_eq!(missing(Some(w.team)).await, 1);
}

async fn a_long_poll_on_a_team_space_ends_with_404_when_the_member_is_removed(w: &World) {
    let poll = {
        let (client, base, token, team) = (w.client.clone(), w.base.clone(), w.token("cy").to_owned(), w.team);
        tokio::spawn(async move {
            client.get(format!("{base}/changes?since=0&wait=30")).bearer_auth(token).header(SPACE_HEADER, team.to_string()).send().await.unwrap()
        })
    };
    // Let the request settle into its wait, then remove the member the way a member route does: change, then wake.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!poll.is_finished(), "the poll waits while nothing changes");
    w.meta.remove_member(&SpaceActor::Cli, w.team, w.user("cy").id, NOW + 20).await.unwrap();
    let removed = Instant::now();
    w.state.notifier.members_changed(w.team);
    let res = tokio::time::timeout(Duration::from_secs(5), poll).await.expect("the waiter was not woken").unwrap();
    assert!(removed.elapsed() < Duration::from_secs(1), "answered {:?} after the removal", removed.elapsed());
    let (seen, status, echo) = seen(res).await;
    assert_eq!((seen, status, echo), (Seen::NoSpace, StatusCode::NOT_FOUND, Some(w.team.to_string())));
}

async fn live_documents_of_a_team_space_are_served_to_its_members(w: &World) {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let open = |who: &'static str, space: Option<SpaceId>| async move {
        let mut request = format!("{}/live", w.base.replacen("http://", "ws://", 1)).into_client_request().unwrap();
        request.headers_mut().insert("authorization", format!("Bearer {}", w.token(who)).parse().unwrap());
        if let Some(space) = space {
            request.headers_mut().insert(SPACE_HEADER, space.to_string().parse().unwrap());
        }
        match tokio_tungstenite::connect_async(request).await {
            Ok(_) => StatusCode::SWITCHING_PROTOCOLS,
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => StatusCode::from_u16(response.status().as_u16()).unwrap(),
            Err(e) => panic!("{e}"),
        }
    };
    // The personal space works as before, with and without the header.
    assert_eq!(open("ana", None).await, StatusCode::SWITCHING_PROTOCOLS);
    assert_eq!(open("ana", Some(w.user("ana").space)).await, StatusCode::SWITCHING_PROTOCOLS);
    // A team space is served to its members, whatever their role (a viewer reads; the role per frame is in `team_live.rs`)…
    for who in ["ana", "ben", "cy"] {
        assert_eq!(open(who, Some(w.team)).await, StatusCode::SWITCHING_PROTOCOLS, "{who}");
    }
    // …and a stranger gets the answer for a space that does not exist, before the upgrade.
    assert_eq!(open("eve", Some(w.team)).await, StatusCode::NOT_FOUND);
}

async fn an_old_protocol_client_sees_the_server_it_always_saw(w: &World) {
    // The server speaks 1 and 2; a client of protocol 1 sends no header and reads no echo.
    let info: InfoResponse = w.client.get(format!("{}/info", w.base)).send().await.unwrap().json().await.unwrap();
    assert_eq!((info.protocol_min, info.protocol_max), (1, 2));
    let info: InfoResponse = w.client.get(w.base.replace("/api/v1", "/api/info")).send().await.unwrap().json().await.unwrap();
    assert_eq!((info.protocol_min, info.protocol_max), (1, 2));
    // With a team space around (ana is its owner), every route a Phase 1 client uses still works on her personal space
    // when it names none: the tree is her own root, a commit lands in her own change feed and not the team's.
    let res = w.send("ana", Method::GET, "/tree", None, Body::None).await;
    let tree: craft_cloud_proto::ChangeSet = res.json().await.unwrap();
    assert_eq!(tree.nodes.len(), 1);
    assert_eq!(tree.nodes[0].id, w.meta.space_info(w.user("ana").space).await.unwrap().root);
    let team_seq_before = w.meta.space_info(w.team).await.unwrap().seq;
    let own_before = w.meta.space_info(w.user("ana").space).await.unwrap().seq;
    let root = tree.nodes[0].id;
    let (data, dir) = (b"old client", NodeId::generate());
    let put = w
        .send("ana", Method::PUT, &format!("/chunks/{}", ChunkId::from_bytes(*blake3::hash(data).as_bytes())), None, Body::Bytes(data.to_vec()))
        .await;
    assert_eq!(put.status(), StatusCode::NO_CONTENT);
    let op = craft_cloud_proto::Op::Mkdir { node: dir, parent: root, name: craft_cloud_proto::Name::new("docs").unwrap() };
    let res = w.send("ana", Method::POST, "/commit", None, Body::Json(json!({ "ops": [op] }))).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(w.meta.space_info(w.user("ana").space).await.unwrap().seq, own_before + 1);
    assert_eq!(w.meta.space_info(w.team).await.unwrap().seq, team_seq_before, "the team space is untouched");
}

// ---- the grep test ----

/// No handler takes its space from the device token: the space comes from the space layer's `SpaceAccess`. The token's own
/// space is read only through `DeviceAuth::personal_space`, and only in the places that are about the account itself.
#[test]
fn no_handler_reads_the_device_tokens_space() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    let mut stack = vec![src];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                files.push(path);
            }
        }
    }
    assert!(files.len() > 15, "{} files found", files.len());
    let mut personal = Vec::new();
    for path in files {
        let text = std::fs::read_to_string(&path).unwrap();
        // Only the code, not the tests that follow it.
        let code = text.split("#[cfg(test)]").next().unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        for (n, line) in code.lines().enumerate() {
            let line = line.trim_start();
            if line.starts_with("//") {
                continue;
            }
            assert!(!line.contains("auth.space") && !line.contains("device.space"), "{name}:{}: reads the token's space: {line}", n + 1);
            if line.contains("personal_space()") {
                personal.push(name.clone());
            }
        }
    }
    personal.sort();
    // The space layer (no header; the personal check), the device layer's epoch, the account usage, and `Session::new`
    // (a session of the device's own space, used by the simulations).
    assert_eq!(personal, ["api.rs", "auth.rs", "live.rs", "space.rs", "space.rs"], "a new reader of the token's space needs a reason");
}
