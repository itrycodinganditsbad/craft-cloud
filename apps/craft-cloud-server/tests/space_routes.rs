//! The team-space routes over HTTP (`/spaces…`, `/admin/spaces…`): invitations, members, roles, usage, rename, limit, archive,
//! the administrator's list and owner route, the audit rows each writes (once, none on a repeat), and the wake-up of a waiting
//! long-poll. SQLite and disk, and Postgres and disk when `CRAFT_CLOUD_TEST_PG` is set (`CRAFT_CLOUD_REQUIRE_BACKENDS` turns a
//! missing backend into a failure).

use std::sync::Arc;
use std::time::{Duration, Instant};

use craft_cloud_meta::{ActionFilter, AuditQuery, Meta, Role, UserRecord};
use craft_cloud_proto::SpaceId;
use craft_cloud_proto::api::{
    AdminSpaceInfo, AdminSpaceList, AuditPage, ErrorBody, LoginResponse, SPACE_HEADER, SpaceList, SpaceListItem, SpaceMember, SpaceMembers,
    SpaceResponse, SpaceRole, SpaceUsage,
};
use craft_cloud_proto::colour::member_colour;
use craft_cloud_server::AppState;
use craft_cloud_server::auth::hash_password;
use craft_cloud_store::Store;
use craft_cloud_store::disk::DiskBackend;
use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};

const PASSWORD: &str = "correct horse battery";
const NOW: i64 = 1_700_000_000_000;
const MIB: u64 = 1024 * 1024;

struct World {
    base: String,
    client: Client,
    meta: Meta,
    url: String,
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

    /// The administrator's confirmation, merged into `body`.
    fn confirmed(body: Value) -> Value {
        let mut body = body;
        body.as_object_mut().unwrap().insert("password".into(), json!(PASSWORD));
        body
    }

    /// Creates a team space as `boss` with `owner` as first owner.
    async fn create(&self, name: &str, owner: &str) -> SpaceId {
        let res =
            self.call("boss", Method::POST, "/admin/spaces", Some(Self::confirmed(json!({ "name": name, "owner": self.user(owner).id.0 })))).await;
        assert_eq!(res.status(), StatusCode::OK, "create {name}");
        res.json::<AdminSpaceInfo>().await.unwrap().id
    }

    /// Invites `who` to `space` as `role`, by its owner `by`, and has them accept.
    async fn join(&self, space: SpaceId, by: &str, who: &str, role: &str) {
        let res = self.call(by, Method::POST, &format!("/spaces/{space}/members"), Some(json!({ "username": who, "role": role }))).await;
        assert_eq!(res.status(), StatusCode::OK, "invite {who}");
        let res = self.call(who, Method::POST, &format!("/spaces/{space}/accept"), None).await;
        assert_eq!(res.status(), StatusCode::OK, "{who} accepts");
    }

    async fn spaces(&self, who: &str) -> Vec<SpaceListItem> {
        let res = self.call(who, Method::GET, "/spaces", None).await;
        assert_eq!(res.status(), StatusCode::OK);
        res.json::<SpaceList>().await.unwrap().spaces
    }

    /// The audit events whose action starts with `prefix` (a group like `space.`), oldest first.
    async fn events(&self, prefix: &str) -> Vec<String> {
        let query = AuditQuery { action: ActionFilter::parse(prefix), limit: 1000, ..AuditQuery::default() };
        let mut events = self.meta.audit_list(&query).await.unwrap();
        events.reverse();
        events.iter().map(|e| e.action.as_str().to_owned()).collect()
    }

    /// A second connection pool on the database, to put accounts in the states the API has no route for here.
    async fn pool(&self) -> sqlx::AnyPool {
        sqlx::any::install_default_drivers();
        sqlx::any::AnyPoolOptions::new().max_connections(2).connect(&self.url).await.unwrap()
    }

    async fn set_user_column(&self, who: &str, column: &str) {
        sqlx::query(sqlx::AssertSqlSafe(format!("UPDATE users SET {column} = 1 WHERE id = $1")))
            .bind(self.user(who).id.0.as_bytes().to_vec())
            .execute(&self.pool().await)
            .await
            .unwrap();
    }
}

async fn error(res: reqwest::Response) -> (StatusCode, String) {
    let status = res.status();
    let body: ErrorBody = res.json().await.unwrap();
    (status, body.error)
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
    for (name, role) in [
        ("boss", Role::Admin),
        ("ana", Role::Member),
        ("ben", Role::Member),
        ("cy", Role::Member),
        ("dee", Role::Member),
        ("eve", Role::Member),
        ("fay", Role::Member),
        ("gus", Role::Member),
    ] {
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
    World { base, client, meta, url, users, _dirs: dirs, _pg: pg }
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
    an_admin_creates_and_lists_spaces,
    invitations_are_accepted_and_declined,
    members_roles_and_the_last_owner,
    invitations_are_rate_limited,
    usage_and_the_space_endpoint,
    a_space_is_renamed_limited_and_archived,
    an_admin_names_an_owner,
    a_removed_member_ends_a_long_poll,
    the_account_activity_shows_events_about_the_account,
    review_fixes_of_the_routes_hold,
    a_member_who_is_removed_and_invited_again_is_listed_with_a_new_joined_stamp,
    the_admin_list_counts_only_owners_who_can_sign_in,
);

// ---- scenarios ----

/// The administrator's list says 0 owners for a space whose owners are disabled or marked for removal: nobody can manage it.
async fn the_admin_list_counts_only_owners_who_can_sign_in(w: &World) {
    let (ana, eve) = (w.user("ana").id.0, w.user("eve").id.0);
    for (name, owner) in [("Alpha", ana), ("Beta", eve)] {
        let res = w.call("boss", Method::POST, "/admin/spaces", Some(World::confirmed(json!({ "name": name, "owner": owner })))).await;
        assert_eq!(res.json::<AdminSpaceInfo>().await.unwrap().owners, 1);
    }
    let owners = || async {
        let list: AdminSpaceList = w.call("boss", Method::GET, "/admin/spaces", None).await.json().await.unwrap();
        list.spaces.iter().map(|s| (s.name.clone(), s.owners)).collect::<Vec<_>>()
    };
    assert_eq!(owners().await, [("Alpha".to_owned(), 1), ("Beta".to_owned(), 1)]);
    w.set_user_column("ana", "disabled_ms").await;
    assert_eq!(owners().await, [("Alpha".to_owned(), 0), ("Beta".to_owned(), 1)]);
    w.set_user_column("eve", "removed_ms").await;
    assert_eq!(owners().await, [("Alpha".to_owned(), 0), ("Beta".to_owned(), 0)]);
}

async fn an_admin_creates_and_lists_spaces(w: &World) {
    let (ana, eve) = (w.user("ana").id.0, w.user("eve").id.0);
    let create = |who: &'static str, body: Value| async move { w.call(who, Method::POST, "/admin/spaces", Some(body)).await };

    // The confirmation is needed, and checked; only an administrator may.
    assert_eq!(error(create("boss", json!({ "name": "Studio", "owner": ana })).await).await, (StatusCode::BAD_REQUEST, "invalid".into()));
    assert_eq!(
        error(create("boss", json!({ "name": "Studio", "owner": ana, "password": "wrong password!" })).await).await,
        (StatusCode::FORBIDDEN, "wrong_password".into())
    );
    assert_eq!(
        error(create("ana", World::confirmed(json!({ "name": "Studio", "owner": ana }))).await).await,
        (StatusCode::FORBIDDEN, "forbidden".into())
    );
    assert!(w.events("space.").await.is_empty(), "nothing was created");

    // A good one: the first owner is a member from the start; the answer is the list's row.
    let res = create("boss", World::confirmed(json!({ "name": "Studio", "owner": ana }))).await;
    assert_eq!(res.status(), StatusCode::OK);
    let studio: AdminSpaceInfo = res.json().await.unwrap();
    assert_eq!((studio.name.as_str(), studio.members, studio.owners, studio.quota_bytes, studio.archived_ms), ("Studio", 1, 1, None, None));
    let mine = w.spaces("ana").await;
    assert_eq!(mine.len(), 2);
    assert_eq!((mine[0].kind.as_str(), mine[0].name.clone()), ("personal", None));
    assert_eq!((mine[1].id, mine[1].kind.as_str(), mine[1].state.as_str(), mine[1].role), (studio.id, "team", "active", SpaceRole::Owner));
    assert!(mine[1].root.is_some() && mine[1].epoch.is_some());
    // The administrator has no access to the content because of that.
    assert_eq!(w.spaces("boss").await.len(), 1);
    assert_eq!(
        error(w.call("boss", Method::GET, &format!("/spaces/{}/members", studio.id), None).await).await,
        (StatusCode::NOT_FOUND, "not_found".into())
    );

    // Refusals: a taken name in any case, a bad name, an unknown owner, a limit under one chunk.
    for (body, expected) in [
        (json!({ "name": "STUDIO", "owner": ana }), (StatusCode::CONFLICT, "conflict")),
        (json!({ "name": "", "owner": ana }), (StatusCode::BAD_REQUEST, "invalid")),
        (json!({ "name": "Other", "owner": uuid::Uuid::now_v7() }), (StatusCode::NOT_FOUND, "not_found")),
        (json!({ "name": "Other", "owner": ana, "quota_bytes": 5 }), (StatusCode::BAD_REQUEST, "invalid")),
    ] {
        assert_eq!(error(create("boss", World::confirmed(body.clone())).await).await, (expected.0, expected.1.to_owned()), "{body}");
    }
    assert_eq!(w.events("space.").await, ["space.created"], "the refused ones wrote nothing");

    // A second, with a limit of its own; the list is by name, pages and searches.
    let res = create("boss", World::confirmed(json!({ "name": "Quota", "owner": eve, "quota_bytes": 8 * MIB }))).await;
    assert_eq!(res.json::<AdminSpaceInfo>().await.unwrap().quota_bytes, Some(8 * MIB));
    let list: AdminSpaceList = w.call("boss", Method::GET, "/admin/spaces", None).await.json().await.unwrap();
    assert_eq!(list.spaces.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["Quota", "Studio"]);
    assert!(list.next.is_none());
    let first: AdminSpaceList = w.call("boss", Method::GET, "/admin/spaces?limit=1", None).await.json().await.unwrap();
    assert_eq!(first.spaces.len(), 1);
    let cursor = first.next.clone().unwrap();
    let second: AdminSpaceList = w.call("boss", Method::GET, &format!("/admin/spaces?limit=1&cursor={cursor}"), None).await.json().await.unwrap();
    assert_eq!((second.spaces[0].name.as_str(), second.next), ("Studio", None));
    let found: AdminSpaceList = w.call("boss", Method::GET, "/admin/spaces?query=TUD", None).await.json().await.unwrap();
    assert_eq!(found.spaces.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["Studio"]);
    for bad in ["/admin/spaces?limit=0", "/admin/spaces?limit=201", "/admin/spaces?cursor=!!", "/admin/spaces?query=%00"] {
        assert_eq!(w.call("boss", Method::GET, bad, None).await.status(), StatusCode::BAD_REQUEST, "{bad}");
    }
    assert_eq!(w.call("ana", Method::GET, "/admin/spaces", None).await.status(), StatusCode::FORBIDDEN);
    assert_eq!(w.events("space.").await, ["space.created", "space.created"]);
}

async fn invitations_are_accepted_and_declined(w: &World) {
    let space = w.create("Studio", "ana").await;
    let invite = |who: &'static str, role: &'static str| async move {
        w.call("ana", Method::POST, &format!("/spaces/{space}/members"), Some(json!({ "username": who, "role": role }))).await
    };

    // An invitation is listed with no root and no epoch, gives no access, and is accepted or declined by its account.
    let res = invite("ben", "editor").await;
    assert_eq!(res.status(), StatusCode::OK);
    let row: SpaceMember = res.json().await.unwrap();
    assert_eq!((row.username.as_str(), row.role, row.state.as_str()), ("ben", SpaceRole::Editor, "invited"));
    let listed = w.spaces("ben").await;
    let item = listed.iter().find(|s| s.id == space).unwrap();
    assert_eq!((item.state.as_str(), item.role, item.root, item.epoch), ("invited", SpaceRole::Editor, None, None));
    assert_eq!(error(w.call("ben", Method::GET, &format!("/spaces/{space}/members"), None).await).await.0, StatusCode::NOT_FOUND);
    assert_eq!(error(w.call("ben", Method::GET, &format!("/spaces/{space}/usage"), None).await).await.0, StatusCode::NOT_FOUND);

    let res = w.call("ben", Method::POST, &format!("/spaces/{space}/accept"), None).await;
    assert_eq!(res.status(), StatusCode::OK);
    let accepted: SpaceListItem = res.json().await.unwrap();
    assert_eq!((accepted.id, accepted.state.as_str(), accepted.members), (space, "active", 2));
    assert!(accepted.root.is_some() && accepted.epoch.is_some());
    // The member list shows both, with the colour every machine derives.
    let members: SpaceMembers = w.call("ben", Method::GET, &format!("/spaces/{space}/members"), None).await.json().await.unwrap();
    assert_eq!(members.members.iter().map(|m| (m.username.as_str(), m.state.as_str())).collect::<Vec<_>>(), [("ana", "active"), ("ben", "active")]);
    for member in &members.members {
        assert_eq!(member.colour, member_colour(member.user));
        assert!(member.colour < 12);
    }

    // Declining deletes the invitation; a second decline and accepting what is gone are 404, and so is a stranger.
    assert_eq!(invite("cy", "viewer").await.status(), StatusCode::OK);
    assert_eq!(w.call("cy", Method::POST, &format!("/spaces/{space}/decline"), None).await.status(), StatusCode::NO_CONTENT);
    for (who, verb) in [("cy", "decline"), ("cy", "accept"), ("ben", "accept"), ("ben", "decline"), ("eve", "accept"), ("eve", "decline")] {
        assert_eq!(
            error(w.call(who, Method::POST, &format!("/spaces/{space}/{verb}"), None).await).await,
            (StatusCode::NOT_FOUND, "not_found".into()),
            "{who} {verb}"
        );
    }
    let nowhere = SpaceId::generate();
    assert_eq!(w.call("eve", Method::POST, &format!("/spaces/{nowhere}/accept"), None).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(w.call("eve", Method::POST, "/spaces/not-an-id/accept", None).await.status(), StatusCode::BAD_REQUEST);
    // Each event once; the repeats wrote nothing.
    assert_eq!(
        w.events("space.").await,
        ["space.created", "space.member_invited", "space.member_joined", "space.member_invited", "space.member_declined"]
    );
}

async fn members_roles_and_the_last_owner(w: &World) {
    let space = w.create("Studio", "ana").await;
    w.join(space, "ana", "ben", "editor").await;
    w.join(space, "ana", "cy", "viewer").await;
    let path = format!("/spaces/{space}/members");
    let member = |who: &str| format!("/spaces/{space}/members/{}", w.user(who).id.0);

    // Only an owner manages the members: a member is `read_only`, a stranger sees no space.
    for who in ["ben", "cy"] {
        assert_eq!(
            error(w.call(who, Method::POST, &path, Some(json!({ "username": "dee", "role": "viewer" }))).await).await,
            (StatusCode::FORBIDDEN, "read_only".into())
        );
        assert_eq!(error(w.call(who, Method::PATCH, &member("cy"), Some(json!({ "role": "owner" }))).await).await.1, "read_only");
        assert_eq!(error(w.call(who, Method::DELETE, &member("ana"), None).await).await.1, "read_only");
    }
    assert_eq!(
        error(w.call("eve", Method::POST, &path, Some(json!({ "username": "dee", "role": "viewer" }))).await).await,
        (StatusCode::NOT_FOUND, "not_found".into())
    );

    // An administrator who is not a member needs the confirmation, and the event names them.
    let by_admin = json!({ "username": "dee", "role": "viewer" });
    assert_eq!(w.call("boss", Method::POST, &path, Some(by_admin.clone())).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(w.call("boss", Method::POST, &path, Some(World::confirmed(by_admin))).await.status(), StatusCode::OK);
    assert_eq!(w.call("boss", Method::DELETE, &member("dee"), Some(World::confirmed(json!({})))).await.status(), StatusCode::NO_CONTENT);

    // Unknown, disabled and removed accounts get one answer, and so does a name that is not a username.
    w.set_user_column("fay", "disabled_ms").await;
    w.set_user_column("gus", "removed_ms").await;
    let mut answers = Vec::new();
    for name in ["nobody", "fay", "gus"] {
        let res = w.call("ana", Method::POST, &path, Some(json!({ "username": name, "role": "viewer" }))).await;
        let status = res.status();
        answers.push((status, res.text().await.unwrap()));
    }
    assert_eq!(answers[0].0, StatusCode::NOT_FOUND);
    assert!(answers.iter().all(|a| *a == answers[0]), "{answers:?}");
    assert_eq!(w.call("ana", Method::POST, &path, Some(json!({ "username": "a b", "role": "viewer" }))).await.status(), StatusCode::BAD_REQUEST);
    assert!(w.call("ana", Method::POST, &path, Some(json!({ "username": "dee", "role": "king" }))).await.status().is_client_error());
    // Already a member or invited: conflict.
    assert_eq!(
        error(w.call("ana", Method::POST, &path, Some(json!({ "username": "ben", "role": "viewer" }))).await).await,
        (StatusCode::CONFLICT, "conflict".into())
    );

    // A role change returns the row; the same role again changes nothing and writes no event.
    let before = w.events("space.").await.len();
    let row: SpaceMember = w.call("ana", Method::PATCH, &member("ben"), Some(json!({ "role": "viewer" }))).await.json().await.unwrap();
    assert_eq!(row.role, SpaceRole::Viewer);
    assert_eq!(w.events("space.").await.len(), before + 1);
    assert_eq!(w.call("ana", Method::PATCH, &member("ben"), Some(json!({ "role": "viewer" }))).await.status(), StatusCode::OK);
    assert_eq!(w.events("space.").await.len(), before + 1, "no change, no event");
    assert_eq!(
        error(w.call("ana", Method::PATCH, &member("eve"), Some(json!({ "role": "viewer" }))).await).await,
        (StatusCode::NOT_FOUND, "not_found".into())
    );

    // The last owner cannot be lowered, removed or leave; with a second one the first can.
    assert_eq!(
        error(w.call("ana", Method::PATCH, &member("ana"), Some(json!({ "role": "editor" }))).await).await,
        (StatusCode::CONFLICT, "last_owner".into())
    );
    assert_eq!(error(w.call("ana", Method::DELETE, &member("ana"), None).await).await, (StatusCode::CONFLICT, "last_owner".into()));
    assert_eq!(w.call("ana", Method::PATCH, &member("ben"), Some(json!({ "role": "owner" }))).await.status(), StatusCode::OK);
    assert_eq!(w.call("ana", Method::PATCH, &member("ana"), Some(json!({ "role": "editor" }))).await.status(), StatusCode::OK);
    assert_eq!(
        error(w.call("ana", Method::PATCH, &member("cy"), Some(json!({ "role": "editor" }))).await).await.1,
        "read_only",
        "ana is an editor now"
    );

    // Removing: by an owner, again is 404; leaving is the member's own row; a pending invitation is withdrawn, or declined by
    // its account through the same route.
    assert_eq!(w.call("ben", Method::DELETE, &member("cy"), None).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(error(w.call("ben", Method::DELETE, &member("cy"), None).await).await.0, StatusCode::NOT_FOUND);
    assert_eq!(w.call("ana", Method::DELETE, &member("ana"), None).await.status(), StatusCode::NO_CONTENT, "an editor leaves");
    assert_eq!(error(w.call("ana", Method::GET, &path, None).await).await.0, StatusCode::NOT_FOUND);
    assert_eq!(w.call("ben", Method::POST, &path, Some(json!({ "username": "dee", "role": "viewer" }))).await.status(), StatusCode::OK);
    assert_eq!(w.call("ben", Method::DELETE, &member("dee"), None).await.status(), StatusCode::NO_CONTENT, "withdrawn");
    assert_eq!(w.call("ben", Method::POST, &path, Some(json!({ "username": "dee", "role": "viewer" }))).await.status(), StatusCode::OK);
    assert_eq!(w.call("dee", Method::DELETE, &member("dee"), None).await.status(), StatusCode::NO_CONTENT, "declined through the own row");
    assert_eq!(error(w.call("dee", Method::DELETE, &member("dee"), None).await).await.0, StatusCode::NOT_FOUND);
}

async fn invitations_are_rate_limited(w: &World) {
    let space = w.create("Studio", "ana").await;
    let path = format!("/spaces/{space}/members");
    // Thirty a minute, counted when asked, whatever the answer; the next is 429 with a Retry-After.
    for i in 0..30 {
        let res = w.call("ana", Method::POST, &path, Some(json!({ "username": format!("nobody{i}"), "role": "viewer" }))).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "{i}");
    }
    let res = w.call("ana", Method::POST, &path, Some(json!({ "username": "ben", "role": "viewer" }))).await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(res.headers().contains_key("retry-after"));
    assert_eq!(res.json::<ErrorBody>().await.unwrap().error, "rate_limited");
    // Another account has its own minute.
    w.join(w.create("Other", "eve").await, "eve", "cy", "viewer").await;
}

async fn usage_and_the_space_endpoint(w: &World) {
    let space = w.create("Studio", "ana").await;
    w.join(space, "ana", "ben", "viewer").await;
    let usage: SpaceUsage = w.call("ben", Method::GET, &format!("/spaces/{space}/usage"), None).await.json().await.unwrap();
    assert_eq!((usage.stored_bytes, usage.live_bytes, usage.quota_bytes), (0, 0, None));
    assert!(usage.estimates.index_complete);
    assert_eq!(error(w.call("eve", Method::GET, &format!("/spaces/{space}/usage"), None).await).await.0, StatusCode::NOT_FOUND);
    // A personal space has no members and no `/spaces` usage.
    let own = w.user("ana").space;
    assert_eq!(w.call("ana", Method::GET, &format!("/spaces/{own}/members"), None).await.status(), StatusCode::NOT_FOUND);

    // `GET /space` says what the space is and the caller's role in it.
    let team: SpaceResponse = w
        .client
        .get(format!("{}/space", w.base))
        .bearer_auth(w.token("ben"))
        .header(SPACE_HEADER, space.to_string())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!((team.kind.as_deref(), team.name.as_deref(), team.role), (Some("team"), Some("Studio"), Some(SpaceRole::Viewer)));
    let personal: SpaceResponse = w.call("ben", Method::GET, "/space", None).await.json().await.unwrap();
    assert_eq!((personal.kind.as_deref(), personal.name, personal.role), (Some("personal"), None, Some(SpaceRole::Owner)));
}

async fn a_space_is_renamed_limited_and_archived(w: &World) {
    let space = w.create("Studio", "ana").await;
    let _quota = w.create("Quota", "eve").await;
    w.join(space, "ana", "ben", "editor").await;
    let patch = |who: &'static str, body: Value| async move { w.call(who, Method::PATCH, &format!("/spaces/{space}"), Some(body)).await };
    let events = |prefix: &'static str| async move {
        w.events(prefix).await.into_iter().filter(|e| e != "space.created" && !e.starts_with("space.member")).collect::<Vec<_>>()
    };

    // Renaming: an owner may; a taken name is a conflict; the same name changes nothing; a member is `read_only`.
    let renamed: AdminSpaceInfo = patch("ana", json!({ "name": "Studio 2" })).await.json().await.unwrap();
    assert_eq!(renamed.name, "Studio 2");
    assert_eq!(patch("ana", json!({ "name": "Studio 2" })).await.status(), StatusCode::OK);
    assert_eq!(error(patch("ana", json!({ "name": "QUOTA" })).await).await, (StatusCode::CONFLICT, "conflict".into()));
    assert_eq!(patch("ana", json!({ "name": "STUDIO 2" })).await.status(), StatusCode::OK, "a change of case is a rename");
    assert_eq!(error(patch("ben", json!({ "name": "Mine" })).await).await, (StatusCode::FORBIDDEN, "read_only".into()));
    assert_eq!(error(patch("eve", json!({ "name": "Mine" })).await).await, (StatusCode::NOT_FOUND, "not_found".into()));
    assert_eq!(patch("ana", json!({})).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(patch("ana", json!({ "name": "\u{7}" })).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(events("space.").await, ["space.renamed", "space.renamed"], "the repeat and the refusals wrote nothing");

    // The limit is an administrator's change: an owner is told so, a stranger learns nothing, an admin confirms.
    assert_eq!(error(patch("ana", json!({ "quota_bytes": 8 * MIB })).await).await, (StatusCode::FORBIDDEN, "forbidden".into()));
    assert_eq!(error(patch("eve", json!({ "quota_bytes": 8 * MIB })).await).await.0, StatusCode::NOT_FOUND);
    assert_eq!(patch("boss", json!({ "quota_bytes": 8 * MIB })).await.status(), StatusCode::BAD_REQUEST);
    let limited: AdminSpaceInfo = patch("boss", World::confirmed(json!({ "quota_bytes": 8 * MIB }))).await.json().await.unwrap();
    assert_eq!(limited.quota_bytes, Some(8 * MIB));
    assert_eq!(patch("boss", World::confirmed(json!({ "quota_bytes": 8 * MIB }))).await.status(), StatusCode::OK);
    assert_eq!(patch("boss", World::confirmed(json!({ "quota_bytes": 5 }))).await.status(), StatusCode::BAD_REQUEST);
    let cleared: AdminSpaceInfo = patch("boss", World::confirmed(json!({ "quota_bytes": null }))).await.json().await.unwrap();
    assert_eq!(cleared.quota_bytes, None);
    assert_eq!(events("space.").await, ["space.renamed", "space.renamed", "space.quota_changed", "space.quota_changed"]);

    // Archiving: an owner with their password; every other member only reads, and the space leaves their list.
    let archive = |who: &'static str, body: Option<Value>| async move { w.call(who, Method::POST, &format!("/spaces/{space}/archive"), body).await };
    assert_eq!(archive("ana", None).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(error(archive("ana", Some(json!({ "password": "wrong password!" }))).await).await, (StatusCode::FORBIDDEN, "wrong_password".into()));
    assert_eq!(error(archive("ben", Some(json!({ "password": PASSWORD }))).await).await, (StatusCode::FORBIDDEN, "read_only".into()));
    assert_eq!(archive("eve", Some(json!({ "password": PASSWORD }))).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(archive("ana", Some(json!({ "password": PASSWORD }))).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(archive("ana", Some(json!({ "password": PASSWORD }))).await.status(), StatusCode::NO_CONTENT, "archived already: no change");
    let member_view = w.spaces("ben").await;
    let seen = member_view.iter().find(|s| s.id == space).expect("an archived space stays in an accepted member's list");
    assert!(seen.archived && seen.root.is_some() && seen.state == "active", "marked archived, still theirs to read: {seen:?}");
    let owner_view = w.spaces("ana").await;
    assert!(owner_view.iter().find(|s| s.id == space).unwrap().archived, "listed for an owner, marked");
    let write = w
        .client
        .post(format!("{}/commit", w.base))
        .bearer_auth(w.token("ben"))
        .header(SPACE_HEADER, space.to_string())
        .json(&json!({ "ops": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(error(write).await, (StatusCode::FORBIDDEN, "read_only".into()), "every member only reads");
    // An owner unarchives the space that caps their own role.
    let unarchive = w.call("ana", Method::POST, &format!("/spaces/{space}/unarchive"), Some(json!({ "password": PASSWORD }))).await;
    assert_eq!(unarchive.status(), StatusCode::NO_CONTENT);
    assert!(w.spaces("ben").await.iter().any(|s| s.id == space && !s.archived));
    // The administrator's forms need the confirmation and an administrator.
    let admin_archive = |who: &'static str, verb: &'static str, body: Value| async move {
        w.call(who, Method::POST, &format!("/admin/spaces/{space}/{verb}"), Some(body)).await
    };
    assert_eq!(admin_archive("boss", "archive", json!({})).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(admin_archive("ana", "archive", World::confirmed(json!({}))).await.status(), StatusCode::FORBIDDEN);
    assert_eq!(admin_archive("boss", "archive", World::confirmed(json!({}))).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(admin_archive("boss", "archive", World::confirmed(json!({}))).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(admin_archive("boss", "unarchive", World::confirmed(json!({}))).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        events("space.").await,
        [
            "space.renamed",
            "space.renamed",
            "space.quota_changed",
            "space.quota_changed",
            "space.archived",
            "space.unarchived",
            "space.archived",
            "space.unarchived"
        ]
    );
}

async fn an_admin_names_an_owner(w: &World) {
    let space = w.create("Studio", "ana").await;
    w.join(space, "ana", "ben", "editor").await;
    assert_eq!(
        w.call("ana", Method::POST, &format!("/spaces/{space}/members"), Some(json!({ "username": "cy", "role": "viewer" }))).await.status(),
        StatusCode::OK
    );
    let put = |who: &'static str, user: &str, confirmed: bool| {
        let body = json!({ "user": w.user(user).id.0 });
        let body = if confirmed { World::confirmed(body) } else { body };
        async move { w.call(who, Method::PUT, &format!("/admin/spaces/{space}/owner"), Some(body)).await }
    };
    assert_eq!(put("boss", "ben", false).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(put("ana", "ben", true).await.status(), StatusCode::FORBIDDEN);
    // An editor becomes an owner, an invitation an accepted owner, an outsider an owner; once each.
    for who in ["ben", "cy", "eve"] {
        assert_eq!(put("boss", who, true).await.status(), StatusCode::NO_CONTENT, "{who}");
    }
    assert_eq!(put("boss", "ben", true).await.status(), StatusCode::NO_CONTENT, "an owner already: no change");
    let members: SpaceMembers = w.call("ana", Method::GET, &format!("/spaces/{space}/members"), None).await.json().await.unwrap();
    assert!(members.members.iter().filter(|m| m.role == SpaceRole::Owner && m.state == "active").count() == 4);
    let changes = w.events("space.").await.into_iter().filter(|e| e == "space.member_role_changed").count();
    assert_eq!(changes, 3);
    // An account that cannot sign in, a space that is not there and a personal space are 404.
    w.set_user_column("fay", "disabled_ms").await;
    assert_eq!(error(put("boss", "fay", true).await).await.0, StatusCode::NOT_FOUND);
    let nowhere = SpaceId::generate();
    let res =
        w.call("boss", Method::PUT, &format!("/admin/spaces/{nowhere}/owner"), Some(World::confirmed(json!({ "user": w.user("ana").id.0 })))).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let own = w.user("ana").space;
    let res = w.call("boss", Method::PUT, &format!("/admin/spaces/{own}/owner"), Some(World::confirmed(json!({ "user": w.user("ana").id.0 })))).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

async fn a_removed_member_ends_a_long_poll(w: &World) {
    let space = w.create("Studio", "ana").await;
    w.join(space, "ana", "ben", "viewer").await;
    let poll = {
        let (client, base, token) = (w.client.clone(), w.base.clone(), w.token("ben").to_owned());
        tokio::spawn(async move {
            client.get(format!("{base}/changes?since=0&wait=30")).bearer_auth(token).header(SPACE_HEADER, space.to_string()).send().await.unwrap()
        })
    };
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!poll.is_finished());
    // The route wakes the waiter itself.
    let removed = Instant::now();
    let res = w.call("ana", Method::DELETE, &format!("/spaces/{space}/members/{}", w.user("ben").id.0), None).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    let res = tokio::time::timeout(Duration::from_secs(5), poll).await.expect("the waiter was not woken").unwrap();
    assert!(removed.elapsed() < Duration::from_secs(1), "{:?}", removed.elapsed());
    assert_eq!(error(res).await, (StatusCode::NOT_FOUND, "not_found".into()));
}

async fn the_account_activity_shows_events_about_the_account(w: &World) {
    let space = w.create("Studio", "ana").await;
    w.join(space, "ana", "ben", "editor").await;
    let page = |who: &'static str| async move {
        let page: AuditPage = w.call(who, Method::GET, "/account/activity?limit=100", None).await.json().await.unwrap();
        page.events.into_iter().map(|e| e.action).collect::<Vec<_>>()
    };
    // ben never acted on the invitation's row as the inviter, but he is the `user` of it, and of his own joining.
    let ben = page("ben").await;
    assert!(ben.contains(&"space.member_invited".to_owned()) && ben.contains(&"space.member_joined".to_owned()), "{ben:?}");
    let eve = page("eve").await;
    assert!(eve.iter().all(|a| !a.starts_with("space.")), "{eve:?}");
    // The owner sees what she did.
    assert!(page("ana").await.contains(&"space.member_invited".to_owned()));
}

/// The review of the routes (P1-3): a refused limit leaves the rename undone, the invitation limit's Retry-After is never
/// short, an invitation to an archived space is not one, an administrator who owns a space acts as one when confirming.
async fn review_fixes_of_the_routes_hold(w: &World) {
    let space = w.create("Studio", "ana").await;
    let other = w.create("Other", "eve").await;
    let patch = |who: &'static str, body: Value| async move { w.call(who, Method::PATCH, &format!("/spaces/{space}"), Some(body)).await };

    // A name together with a limit that is refused changes neither.
    assert_eq!(patch("boss", World::confirmed(json!({ "name": "Renamed", "quota_bytes": 5 }))).await.status(), StatusCode::BAD_REQUEST);
    let listed: AdminSpaceList = w.call("boss", Method::GET, "/admin/spaces?query=studio", None).await.json().await.unwrap();
    assert_eq!(listed.spaces.len(), 1, "the rename did not commit");
    assert!(w.events("space.").await.iter().all(|e| e != "space.renamed" && e != "space.quota_changed"));
    // A name shaped like a space ID is refused (on create and on rename).
    let id_like = "0194a2d0-1c3e-7b5a-9d41-2f6e8a7b3c10";
    assert_eq!(patch("ana", json!({ "name": id_like })).await.status(), StatusCode::BAD_REQUEST);
    let res = w.call("boss", Method::POST, "/admin/spaces", Some(World::confirmed(json!({ "name": id_like, "owner": w.user("ana").id.0 })))).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    // The Retry-After of the invitation limit is whole seconds rounded up, at least one.
    let path = format!("/spaces/{other}/members");
    for i in 0..30 {
        w.call("eve", Method::POST, &path, Some(json!({ "username": format!("nobody{i}"), "role": "viewer" }))).await;
    }
    let res = w.call("eve", Method::POST, &path, Some(json!({ "username": "ben", "role": "viewer" }))).await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    let wait: u64 = res.headers().get("retry-after").unwrap().to_str().unwrap().parse().unwrap();
    assert!((1..=60).contains(&wait), "{wait}");

    // An invitation to an archived space: neither listed nor accepted nor declined.
    let invite = w.call("ana", Method::POST, &format!("/spaces/{space}/members"), Some(json!({ "username": "cy", "role": "viewer" }))).await;
    assert_eq!(invite.status(), StatusCode::OK);
    let archived = w.call("ana", Method::POST, &format!("/spaces/{space}/archive"), Some(json!({ "password": PASSWORD }))).await;
    assert_eq!(archived.status(), StatusCode::NO_CONTENT);
    assert!(w.spaces("cy").await.iter().all(|s| s.id != space));
    for verb in ["accept", "decline"] {
        let res = w.call("cy", Method::POST, &format!("/spaces/{space}/{verb}"), None).await;
        assert_eq!(error(res).await, (StatusCode::NOT_FOUND, "not_found".into()), "{verb}");
    }
    // An owner of the archived space is capped at viewer: the change is `read_only`, not `forbidden`.
    assert_eq!(error(patch("ana", json!({ "name": "Nope" })).await).await, (StatusCode::FORBIDDEN, "read_only".into()));
    let unarchived = w.call("ana", Method::POST, &format!("/spaces/{space}/unarchive"), Some(json!({ "password": PASSWORD }))).await;
    assert_eq!(unarchived.status(), StatusCode::NO_CONTENT);

    // An administrator who owns a space and confirms acts as an administrator; without the confirmation, as the owner.
    let own = w.create("Boss", "boss").await;
    let own_patch = |body: Value| async move { w.call("boss", Method::PATCH, &format!("/spaces/{own}"), Some(body)).await };
    assert_eq!(own_patch(json!({ "name": "Boss own" })).await.status(), StatusCode::OK, "an owner renames without a password");
    assert_eq!(own_patch(World::confirmed(json!({ "name": "Boss other" }))).await.status(), StatusCode::OK);
    assert_eq!(own_patch(World::confirmed(json!({ "quota_bytes": 8 * MIB }))).await.status(), StatusCode::OK);
    assert_eq!(
        error(own_patch(json!({ "quota_bytes": 8 * MIB * 2 })).await).await.0,
        StatusCode::BAD_REQUEST,
        "no confirmation: the limit asks for it"
    );
}

/// `GET /spaces` gives an accepted member the time they joined: it stays the same while they are a member (a role change does
/// not move it) and is new when they are removed and invited again, so an agent can tell that its access was interrupted.
/// A personal space and an invitation have none.
async fn a_member_who_is_removed_and_invited_again_is_listed_with_a_new_joined_stamp(w: &World) {
    let space = w.create("Studio", "ana").await;
    let stamp = |list: Vec<SpaceListItem>| list.iter().find(|s| s.id == space).and_then(|s| s.joined_ms);
    let invite = w.call("ana", Method::POST, &format!("/spaces/{space}/members"), Some(json!({ "username": "ben", "role": "editor" }))).await;
    assert_eq!(invite.status(), StatusCode::OK);
    assert_eq!(stamp(w.spaces("ben").await), None, "an invitation has none");
    let accepted = w.call("ben", Method::POST, &format!("/spaces/{space}/accept"), None).await;
    assert_eq!(accepted.status(), StatusCode::OK);
    let first = stamp(w.spaces("ben").await).expect("an accepted member has one");
    assert!(w.spaces("ben").await.iter().filter(|s| s.kind == "personal").all(|s| s.joined_ms.is_none()), "a personal space has none");

    let role = w.call("ana", Method::PATCH, &format!("/spaces/{space}/members/{}", w.user("ben").id.0), Some(json!({ "role": "viewer" }))).await;
    assert_eq!(role.status(), StatusCode::OK);
    assert_eq!(stamp(w.spaces("ben").await), Some(first), "a role change does not move it");

    let removed = w.call("ana", Method::DELETE, &format!("/spaces/{space}/members/{}", w.user("ben").id.0), None).await;
    assert_eq!(removed.status(), StatusCode::NO_CONTENT);
    assert_eq!(stamp(w.spaces("ben").await), None, "no longer a member");
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    w.join(space, "ana", "ben", "editor").await;
    let second = stamp(w.spaces("ben").await).expect("a member again");
    assert!(second > first, "invited and accepted again: a new stamp ({first} then {second})");
}
