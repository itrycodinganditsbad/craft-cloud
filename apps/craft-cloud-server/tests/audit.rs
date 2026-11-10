//! The security audit log over HTTP: a real server on a random loopback port, driven with an HTTP client. Every scenario runs
//! on SQLite + disk, and on Postgres + disk when `CRAFT_CLOUD_TEST_PG` is set (`CRAFT_CLOUD_REQUIRE_BACKENDS` turns a missing
//! one into a failure). Blob storage plays no part here, so Postgres runs with disk storage and needs no S3.

use std::collections::BTreeMap;

use craft_cloud_meta::{AuditAction, AuditFailure, AuditQuery, Meta, Role, Step};
use craft_cloud_proto::api::{AdminCode, AdminCreatedUser, AuditEvent, AuditExportEnd, AuditPage, ErrorBody, LoginResponse, PairingCreated};
use craft_cloud_server::AppState;
use craft_cloud_server::audit::AuditExportLimits;
use craft_cloud_server::auth::hash_password;
use craft_cloud_store::Store;
use craft_cloud_store::disk::DiskBackend;
use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";
const MINUTE: i64 = 60_000;

struct Server {
    base: String,
    meta: Meta,
    /// The database's URL, for a test that breaks the log.
    url: String,
    client: Client,
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

async fn start(url: String, dirs: Vec<tempfile::TempDir>, pg: Option<PgGuard>, export: AuditExportLimits) -> Server {
    let meta = Meta::open(&url).await.unwrap();
    let blobs = tempfile::tempdir().unwrap();
    let store = Store::new(DiskBackend::new(blobs.path()).unwrap());
    let state =
        AppState::new(meta.clone(), store, vec![]).unwrap().with_public_url(Some("https://cloud.example.org".to_owned())).with_audit_export(export);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/api/v1", listener.local_addr().unwrap());
    tokio::spawn(craft_cloud_server::serve(state, listener, std::future::pending::<()>()));
    let mut dirs = dirs;
    dirs.push(blobs);
    Server { base, meta, url, client: Client::new(), _dirs: dirs, _pg: pg }
}

async fn sqlite_disk(export: AuditExportLimits) -> Server {
    let db = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}", db.path().join("meta.db").display());
    start(url, vec![db], None, export).await
}

async fn postgres_disk(export: AuditExportLimits) -> Option<Server> {
    let Ok(admin_url) = std::env::var("CRAFT_CLOUD_TEST_PG") else {
        assert!(std::env::var_os("CRAFT_CLOUD_REQUIRE_BACKENDS").is_none(), "CRAFT_CLOUD_REQUIRE_BACKENDS is set but CRAFT_CLOUD_TEST_PG is not");
        return None;
    };
    use sqlx::Connection;
    let name = format!("cc_test_{}", Uuid::now_v7().simple());
    let mut conn = sqlx::PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}"))).execute(&mut conn).await.unwrap();
    let guard = PgGuard { admin_url: admin_url.clone(), name: name.clone() };
    let (base, _) = admin_url.rsplit_once('/').unwrap();
    Some(start(format!("{base}/{name}"), vec![], Some(guard), export).await)
}

/// The scenarios of `$group` on every backend, each with a server whose export reads `$limits`.
macro_rules! on_every_backend {
    ($group:ident, $limits:expr; $($scenario:ident),* $(,)?) => {
        mod $group {
            mod on_sqlite_disk {
                $( #[tokio::test(flavor = "multi_thread", worker_threads = 4)] async fn $scenario() {
                    crate::$scenario(&crate::sqlite_disk($limits).await).await;
                } )*
            }
            mod on_postgres_disk {
                $( #[tokio::test(flavor = "multi_thread", worker_threads = 4)] async fn $scenario() {
                    let Some(server) = crate::postgres_disk($limits).await else { eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set"); return };
                    crate::$scenario(&server).await;
                } )*
            }
        }
    };
}

// ---- helpers ----

async fn add_user(s: &Server, name: &str, role: Role) -> Uuid {
    s.meta.create_user(name, &hash_password(PASSWORD).unwrap(), role, 0).await.unwrap().id.0
}

struct Signed {
    token: String,
    device: Uuid,
}

async fn sign_in_with(s: &Server, name: &str, password: &str, extra: Value) -> reqwest::Response {
    let mut body = json!({ "username": name, "password": password, "device_name": "laptop" });
    body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    s.client.post(format!("{}/auth/login", s.base)).json(&body).send().await.unwrap()
}

async fn login(s: &Server, name: &str, extra: Value) -> Signed {
    let res = sign_in_with(s, name, PASSWORD, extra).await;
    assert_eq!(res.status(), StatusCode::OK);
    let body: LoginResponse = res.json().await.unwrap();
    Signed { token: body.token, device: *body.device.as_uuid() }
}

async fn call(s: &Server, token: Option<&str>, method: Method, path: &str, body: Option<Value>) -> reqwest::Response {
    let mut request = s.client.request(method, format!("{}{path}", s.base));
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    request.send().await.unwrap()
}

/// An admin change, confirmed with the admin's password.
async fn admin(s: &Server, token: &str, method: Method, path: &str, mut body: Value) -> reqwest::Response {
    body["password"] = json!(PASSWORD);
    call(s, Some(token), method, path, Some(body)).await
}

async fn page(s: &Server, token: &str, path: &str) -> AuditPage {
    let res = call(s, Some(token), Method::GET, path, None).await;
    let status = res.status();
    let text = res.text().await.unwrap();
    assert_eq!(status, StatusCode::OK, "{path}: {text}");
    serde_json::from_str(&text).unwrap()
}

async fn refusal(res: reqwest::Response) -> (StatusCode, String) {
    let status = res.status();
    (status, res.json::<ErrorBody>().await.unwrap().error)
}

/// Every event the admin's list holds, by paging through it.
async fn whole_log(s: &Server, token: &str) -> Vec<AuditEvent> {
    let mut events = Vec::new();
    let mut path = "/admin/audit?limit=200".to_owned();
    loop {
        let got = page(s, token, &path).await;
        events.extend(got.events);
        match got.next {
            Some(next) => path = format!("/admin/audit?limit=200&cursor={next}"),
            None => return events,
        }
    }
}

fn by_action(events: &[AuditEvent]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for event in events {
        *counts.entry(event.action.clone()).or_insert(0) += 1;
    }
    counts
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

/// `count` failed sign-ins at minutes of their own (so many rows), in the past.
async fn seed(s: &Server, count: i64, target: Uuid) {
    for i in 0..count {
        let failure = AuditFailure { action: AuditAction::AuthLoginFailed, target: Some(craft_cloud_meta::UserId(target)), step: Step::Password };
        s.meta.audit_record_failure(failure, 1_000_000 + i * MINUTE).await.unwrap();
    }
}

/// Makes every use of the log fail, as an error of the database in it would.
async fn break_log(s: &Server) {
    use sqlx::Connection;
    let mut conn = sqlx::AnyConnection::connect(&s.url).await.unwrap();
    sqlx::query("ALTER TABLE audit_events RENAME TO audit_events_away").execute(&mut conn).await.unwrap();
}

async fn repair_log(s: &Server) {
    use sqlx::Connection;
    let mut conn = sqlx::AnyConnection::connect(&s.url).await.unwrap();
    sqlx::query("ALTER TABLE audit_events_away RENAME TO audit_events").execute(&mut conn).await.unwrap();
}

// ---- scenarios ----

/// One session through everything that writes an event: each change leaves its event and nothing more, a repeat leaves none,
/// failed sign-ins are counted in rows of their own (a name no account has is counted but never kept), and nowhere in the
/// log, the account's activity or the export is there an address, a device name, a name typed by someone, a code, a token
/// or a password.
async fn a_session_leaves_its_events_and_no_secret(s: &Server) {
    let root = add_user(s, "rootuser", Role::Admin).await;
    let quincy = add_user(s, "quincy", Role::Member).await;
    let mut secrets: Vec<String> = vec![PASSWORD.to_owned()];

    // Sign-ins: a client that calls its platform an address, then wrong passwords for an account and for a name nobody has.
    let hostile = json!({ "device_name": "quincy-secret-laptop", "client_kind": "agent", "platform": "10.0.0.1", "app_version": "6.6.6-leaky" });
    let q1 = login(s, "quincy", hostile).await;
    secrets.push(q1.token.clone());
    for (name, password) in
        [("quincy", "a wrong password"), ("quincy", "a wrong password"), ("ghostuser", "a ghost password"), ("ghostuser", "a ghost password")]
    {
        let res = sign_in_with(s, name, password, json!({})).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
    let q2 = login(s, "quincy", json!({ "platform": "Linux", "client_kind": "cli" })).await;
    let q3 = login(s, "quincy", json!({})).await;
    secrets.extend([q2.token.clone(), q3.token.clone()]);

    // Devices: one signed out (and again, which changes nothing), then every other device.
    let path = format!("/devices/{}", q2.device);
    assert_eq!(call(s, Some(&q1.token), Method::DELETE, &path, None).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(call(s, Some(&q1.token), Method::DELETE, &path, None).await.status(), StatusCode::NO_CONTENT);
    let others: Value = call(s, Some(&q1.token), Method::POST, "/devices/revoke-others", None).await.json().await.unwrap();
    assert_eq!(others["revoked"], 1);
    let again: Value = call(s, Some(&q1.token), Method::POST, "/devices/revoke-others", None).await.json().await.unwrap();
    assert_eq!(again["revoked"], 0);
    // A password change, and a sign-in with the new password on a device an admin signs out later.
    let new_password = "a brand new password";
    secrets.push(new_password.to_owned());
    let changed =
        call(s, Some(&q1.token), Method::POST, "/account/password", Some(json!({ "current_password": PASSWORD, "new_password": new_password })))
            .await;
    assert_eq!(changed.status(), StatusCode::NO_CONTENT);
    let res = sign_in_with(s, "quincy", new_password, json!({})).await;
    assert_eq!(res.status(), StatusCode::OK);
    let q4: LoginResponse = res.json().await.unwrap();
    secrets.push(q4.token.clone());

    // The administrator's changes.
    let r = login(s, "rootuser", json!({ "platform": "macos" })).await;
    secrets.push(r.token.clone());
    let made: AdminCreatedUser =
        admin(s, &r.token, Method::POST, "/admin/users", json!({ "username": "newbiexyz", "role": "member" })).await.json().await.unwrap();
    secrets.extend([made.invite.code.clone(), made.invite.join_url.clone().unwrap()]);
    let newbie = made.user.id;
    let invitee: AdminCreatedUser =
        admin(s, &r.token, Method::POST, "/admin/users", json!({ "username": "inviteexyz", "role": "member" })).await.json().await.unwrap();
    secrets.extend([invitee.invite.code.clone(), invitee.invite.join_url.clone().unwrap()]);
    // Role and quota in one request, and the same again.
    for _ in 0..2 {
        let res = admin(s, &r.token, Method::PATCH, &format!("/admin/users/{newbie}"), json!({ "role": "admin", "quota_bytes": 5_000_000 })).await;
        assert_eq!(res.status(), StatusCode::OK);
    }
    // The new account joins with its invite, reports itself as a Windows desktop.
    let joined = s
        .client
        .post(format!("{}/join", s.base))
        .json(&json!({ "code": made.invite.code, "device_name": "newbie-pc", "password": "newbie password 1", "client_kind": "desktop", "platform": "windows" }))
        .send()
        .await
        .unwrap();
    assert_eq!(joined.status(), StatusCode::OK);
    secrets.extend(["newbie password 1".to_owned(), joined.json::<LoginResponse>().await.unwrap().token]);
    // Disable (twice), enable (twice).
    for (what, status) in [
        ("disable", StatusCode::NO_CONTENT),
        ("disable", StatusCode::NO_CONTENT),
        ("enable", StatusCode::NO_CONTENT),
        ("enable", StatusCode::NO_CONTENT),
    ] {
        assert_eq!(admin(s, &r.token, Method::POST, &format!("/admin/users/{newbie}/{what}"), json!({})).await.status(), status);
    }
    // An admin signs out one of quincy's devices (twice), and makes a reset code for him, redeemed on a new computer.
    let path = format!("/admin/users/{quincy}/devices/{}", q4.device);
    for _ in 0..2 {
        assert_eq!(admin(s, &r.token, Method::DELETE, &path, json!({})).await.status(), StatusCode::NO_CONTENT);
    }
    let reset: AdminCode = admin(s, &r.token, Method::POST, &format!("/admin/users/{quincy}/password-reset"), json!({})).await.json().await.unwrap();
    secrets.extend([reset.code.clone(), reset.join_url.clone().unwrap()]);
    let reset_password = "quincys reset password";
    secrets.push(reset_password.to_owned());
    let joined = s
        .client
        .post(format!("{}/join", s.base))
        .json(&json!({ "code": reset.code, "device_name": "quincy-recovery", "password": reset_password }))
        .send()
        .await
        .unwrap();
    assert_eq!(joined.status(), StatusCode::OK);
    let recovered: LoginResponse = joined.json().await.unwrap();
    secrets.push(recovered.token.clone());
    // An invite revoked and issued again; a pairing redeemed.
    let invitee_id = invitee.user.id;
    assert_eq!(admin(s, &r.token, Method::DELETE, &format!("/admin/users/{invitee_id}/invite"), json!({})).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(admin(s, &r.token, Method::DELETE, &format!("/admin/users/{invitee_id}/invite"), json!({})).await.status(), StatusCode::NOT_FOUND);
    let reissued: AdminCode = admin(s, &r.token, Method::POST, &format!("/admin/users/{invitee_id}/invite"), json!({})).await.json().await.unwrap();
    secrets.extend([reissued.code.clone(), reissued.join_url.clone().unwrap()]);
    let pairing: PairingCreated = call(s, Some(&r.token), Method::POST, "/pairings", None).await.json().await.unwrap();
    secrets.push(pairing.code.clone());
    let paired =
        s.client.post(format!("{}/join", s.base)).json(&json!({ "code": pairing.code, "device_name": "root-second-pc" })).send().await.unwrap();
    assert_eq!(paired.status(), StatusCode::OK);
    secrets.push(paired.json::<LoginResponse>().await.unwrap().token);
    // Marked for removal.
    assert_eq!(admin(s, &r.token, Method::DELETE, &format!("/admin/users/{newbie}"), json!({})).await.status(), StatusCode::ACCEPTED);
    // Reading the log is no event.
    let events = whole_log(s, &r.token).await;

    // The matrix: exactly these events, one for each change, none for the repeats.
    let wanted: BTreeMap<String, usize> = [
        ("auth.login", 5),
        ("device.revoked", 1),
        ("device.revoked_others", 1),
        ("account.password_changed", 1),
        ("admin.user_created", 2),
        ("admin.role_changed", 1),
        ("admin.quota_changed", 1),
        ("join.invite_redeemed", 1),
        ("admin.user_disabled", 1),
        ("admin.user_enabled", 1),
        ("admin.password_reset_issued", 1),
        ("join.reset_redeemed", 1),
        ("admin.device_revoked", 1),
        ("admin.invite_revoked", 1),
        ("admin.invite_issued", 1),
        ("join.pairing_redeemed", 1),
        ("admin.user_removed", 1),
    ]
    .into_iter()
    .map(|(name, count)| (name.to_owned(), count))
    .collect();
    let mut seen = by_action(&events);
    // The failed sign-ins: counted, in a row for each account and minute (a minute may turn between two tries).
    let failed: Vec<&AuditEvent> = events.iter().filter(|e| e.action == "auth.login_failed").collect();
    assert!((2..=4).contains(&failed.len()), "{}", failed.len());
    assert_eq!(failed.iter().map(|e| e.count).sum::<u64>(), 4);
    assert!(failed.iter().all(|e| e.outcome == "failure" && e.actor_user.is_none() && e.details.step.as_deref() == Some("password")));
    assert_eq!(failed.iter().filter(|e| e.target_id == Some(quincy)).map(|e| e.count).sum::<u64>(), 2);
    assert_eq!(
        failed.iter().filter(|e| e.target_id.is_none()).map(|e| e.count).sum::<u64>(),
        2,
        "the ghost's tries are counted without a name or an account"
    );
    seen.remove("auth.login_failed");
    assert_eq!(seen, wanted);

    // Who did what: the details are typed, and the client shows as the closed lists.
    let one = |action: &str| events.iter().find(|e| e.action == action).unwrap();
    let first_login = events.iter().filter(|e| e.action == "auth.login" && e.actor_user == Some(quincy)).min_by_key(|e| (e.at_ms, e.id)).unwrap();
    assert_eq!(first_login.actor_device, Some(craft_cloud_proto::DeviceId::from_uuid(q1.device)));
    assert_eq!((first_login.details.client_kind.as_deref(), first_login.details.platform.as_deref()), (Some("agent"), Some("other")));
    assert_eq!(one("device.revoked_others").details.revoked, Some(1));
    assert_eq!(one("account.password_changed").details.revoked, Some(0));
    let role = one("admin.role_changed");
    assert_eq!(
        (role.actor_user, role.target_id, role.details.role_from.as_deref(), role.details.role_to.as_deref()),
        (Some(root), Some(newbie), Some("member"), Some("admin"))
    );
    assert_eq!(one("admin.quota_changed").details.quota_bytes, Some(Some(5_000_000)));
    let redeemed = one("join.reset_redeemed");
    assert_eq!((redeemed.actor_user, redeemed.details.revoked), (Some(quincy), Some(1)), "the one device that was still signed in");
    let join = one("join.invite_redeemed");
    assert_eq!((join.details.client_kind.as_deref(), join.details.platform.as_deref()), (Some("desktop"), Some("windows")));
    assert_eq!(one("admin.user_disabled").details.revoked, Some(1));

    // No address, name, code, token or password anywhere: not in the list, the account's activity or the export.
    let list = call(s, Some(&r.token), Method::GET, "/admin/audit?limit=200", None).await.text().await.unwrap();
    let activity = call(s, Some(&recovered.token), Method::GET, "/account/activity", None).await.text().await.unwrap();
    let activity_root = call(s, Some(&r.token), Method::GET, "/account/activity", None).await.text().await.unwrap();
    let export = call(s, Some(&r.token), Method::GET, "/admin/audit/export", None).await.text().await.unwrap();
    secrets.extend(
        [
            "127.0.0.1",
            "localhost",
            "10.0.0.1",
            "quincy",
            "ghostuser",
            "newbiexyz",
            "inviteexyz",
            "rootuser",
            "laptop",
            "recovery",
            "root-second-pc",
            "newbie-pc",
            "leaky",
            "6.6.6",
            "a wrong password",
            "a ghost password",
        ]
        .map(str::to_owned),
    );
    for (what, text) in [("list", &list), ("activity", &activity), ("activity of root", &activity_root), ("export", &export)] {
        let lower = text.to_ascii_lowercase();
        for secret in &secrets {
            assert!(!lower.contains(&secret.to_ascii_lowercase()), "{secret:?} in the {what}: {text}");
        }
    }
    // And the export holds the same events, ending in `complete`.
    let lines: Vec<&str> = export.lines().collect();
    let end: AuditExportEnd = serde_json::from_str(lines.last().unwrap()).unwrap();
    assert_eq!((end.complete, end.truncated, end.exported as usize), (Some(true), None, lines.len() - 1));
    // 17 kinds of event with their repeats, and the failed sign-ins' rows, plus the exports and reads, which are no events.
    assert!(lines.len() > events.len(), "the export has at least what the list had, and its last line");
}

/// The account's own activity: its events and the ones done to it, newest first, in pages by an exclusive cursor, with the
/// device of someone else's act hidden, and nobody else's events.
async fn the_account_sees_only_its_own_activity(s: &Server) {
    let root = add_user(s, "rootuser", Role::Admin).await;
    let quincy = add_user(s, "quincy", Role::Member).await;
    add_user(s, "bobby", Role::Member).await;
    let q = login(s, "quincy", json!({ "platform": "linux" })).await;
    let _b = login(s, "bobby", json!({})).await;
    let r = login(s, "rootuser", json!({})).await;
    // An admin acts on quincy (from a device of the admin's), and the failed guess at quincy's password is about quincy too.
    assert_eq!(admin(s, &r.token, Method::POST, &format!("/admin/users/{quincy}/password-reset"), json!({})).await.status(), StatusCode::OK);
    assert_eq!(sign_in_with(s, "quincy", "not the password", json!({})).await.status(), StatusCode::UNAUTHORIZED);
    let _second = login(s, "quincy", json!({})).await;

    let mine = page(s, &q.token, "/account/activity").await;
    assert!(mine.next.is_none());
    let mut names: Vec<&str> = mine.events.iter().map(|e| e.action.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["admin.password_reset_issued", "auth.login", "auth.login", "auth.login_failed"]);
    assert!(mine.events.iter().all(|e| e.actor_user == Some(quincy) || e.target_id == Some(quincy)), "nobody else's events");
    let order: Vec<_> = mine.events.iter().map(|e| (e.at_ms, e.id)).collect();
    assert!(order.windows(2).all(|w| w[0] >= w[1]), "newest first");
    // The admin's act on the account: who did it is shown, but not the admin's device.
    let reset = mine.events.iter().find(|e| e.action == "admin.password_reset_issued").unwrap();
    assert_eq!((reset.actor_user, reset.actor_device), (Some(root), None));
    // An event of the account's own act shows its device.
    assert!(mine.events.iter().filter(|e| e.action == "auth.login").all(|e| e.actor_device.is_some()));

    // Pages of one through `next`: every event once, the last page without a `next`.
    let mut paged = Vec::new();
    let mut path = "/account/activity?limit=1".to_owned();
    for _ in 0..10 {
        let got = page(s, &q.token, &path).await;
        assert!(got.events.len() <= 1);
        paged.extend(got.events);
        let Some(next) = got.next else { break };
        path = format!("/account/activity?limit=1&cursor={next}");
    }
    assert_eq!(paged, mine.events);
    // The cursor wins over `before_ms`, which on its own limits the first page.
    let first = page(s, &q.token, "/account/activity?limit=1").await;
    let cursor = first.next.unwrap();
    let second = page(s, &q.token, &format!("/account/activity?limit=1&before_ms=1&cursor={cursor}")).await;
    assert_eq!(second.events.len(), 1);
    assert!(page(s, &q.token, "/account/activity?before_ms=1").await.events.is_empty());
    assert_eq!(page(s, &q.token, &format!("/account/activity?before_ms={}", now_ms() + 60_000)).await.events.len(), 4);
    // The admin's filters are not the account's to use.
    let filtered = page(s, &q.token, &format!("/account/activity?user={root}&action=admin.")).await;
    assert_eq!(filtered.events, mine.events);

    // Bad parameters are `400 invalid`.
    for bad in ["limit=0", "limit=101", "limit=-1", "limit=x", "cursor=garbage", "cursor=", "before_ms=-5", "before_ms=x"] {
        let (status, code) = refusal(call(s, Some(&q.token), Method::GET, &format!("/account/activity?{bad}"), None).await).await;
        assert_eq!((status, code.as_str()), (StatusCode::BAD_REQUEST, "invalid"), "{bad}");
    }
    assert_eq!(page(s, &q.token, "/account/activity?limit=100").await.events.len(), 4);
    // Not signed in is 401.
    let (status, code) = refusal(call(s, None, Method::GET, "/account/activity", None).await).await;
    assert_eq!((status, code.as_str()), (StatusCode::UNAUTHORIZED, "unauthorized"));
}

/// A former administrator keeps the events of what they did, but no longer sees the details of what they did to others.
async fn a_demoted_admin_sees_no_details_of_their_acts_on_others(s: &Server) {
    let root = add_user(s, "rootuser", Role::Admin).await;
    add_user(s, "seconduser", Role::Admin).await;
    let quincy = add_user(s, "quincy", Role::Member).await;
    let q = login(s, "quincy", json!({})).await;
    let r = login(s, "rootuser", json!({})).await;
    let t = login(s, "seconduser", json!({})).await;
    let path = format!("/admin/users/{quincy}/devices/{}", q.device);
    assert_eq!(admin(s, &r.token, Method::DELETE, &path, json!({})).await.status(), StatusCode::NO_CONTENT);
    let before = page(s, &r.token, "/account/activity").await;
    assert!(before.events.iter().any(|e| e.action == "admin.device_revoked" && e.details.device.is_none()), "{before:?}");
    assert_eq!(admin(s, &t.token, Method::PATCH, &format!("/admin/users/{root}"), json!({ "role": "member" })).await.status(), StatusCode::OK);
    let mine = page(s, &r.token, "/account/activity").await;
    let revoked = mine.events.iter().find(|e| e.action == "admin.device_revoked").unwrap();
    assert_eq!((revoked.actor_user, revoked.target_id), (Some(root), Some(quincy)));
    assert_eq!(revoked.details, Default::default(), "no device of someone else");
    let text = serde_json::to_string(&mine).unwrap();
    assert!(!text.contains(&q.device.to_string()), "{text}");
    // An admin still sees all of it.
    let all = page(s, &t.token, "/admin/audit").await;
    let full = all.events.iter().find(|e| e.action == "admin.device_revoked").unwrap();
    assert_eq!(full.details.device.map(|d| *d.as_uuid()), Some(q.device));
}

/// The admin's list and export are for administrators: a member is `403 forbidden`, no token `401`; filters, limits and the
/// cursor are checked.
async fn the_admin_list_is_for_admins_and_checks_its_parameters(s: &Server) {
    let root = add_user(s, "rootuser", Role::Admin).await;
    let quincy = add_user(s, "quincy", Role::Member).await;
    let q = login(s, "quincy", json!({})).await;
    let r = login(s, "rootuser", json!({})).await;
    for path in ["/admin/audit", "/admin/audit/export"] {
        let (status, code) = refusal(call(s, Some(&q.token), Method::GET, path, None).await).await;
        assert_eq!((status, code.as_str()), (StatusCode::FORBIDDEN, "forbidden"), "{path}");
        let (status, code) = refusal(call(s, None, Method::GET, path, None).await).await;
        assert_eq!((status, code.as_str()), (StatusCode::UNAUTHORIZED, "unauthorized"), "{path}");
    }
    for n in 0..5 {
        let role = if n % 2 == 0 { "admin" } else { "member" };
        assert_eq!(admin(s, &r.token, Method::PATCH, &format!("/admin/users/{quincy}"), json!({ "role": role })).await.status(), StatusCode::OK);
    }
    let all = whole_log(s, &r.token).await;
    assert_eq!(by_action(&all), [("admin.role_changed".to_owned(), 5), ("auth.login".to_owned(), 2)].into_iter().collect());
    // Filters.
    assert_eq!(page(s, &r.token, &format!("/admin/audit?user={quincy}")).await.events.len(), 6, "his sign-in and the five changes to him");
    assert_eq!(page(s, &r.token, &format!("/admin/audit?user={root}")).await.events.len(), 6, "his sign-in and the five changes he made");
    assert_eq!(page(s, &r.token, "/admin/audit?action=admin.role_changed").await.events.len(), 5);
    assert_eq!(page(s, &r.token, "/admin/audit?action=auth.").await.events.len(), 2);
    assert_eq!(page(s, &r.token, "/admin/audit?action=auth.login").await.events.len(), 2);
    assert_eq!(page(s, &r.token, "/admin/audit?action=auth.login_failed").await.events.len(), 0);
    assert_eq!(page(s, &r.token, &format!("/admin/audit?user={quincy}&action=admin.")).await.events.len(), 5);
    assert_eq!(page(s, &r.token, &format!("/admin/audit?user={}", Uuid::now_v7())).await.events.len(), 0);
    // The filter belongs to the cursor: pages of two with one filter hold exactly what the whole list does.
    let mut paged = Vec::new();
    let mut path = "/admin/audit?limit=2&action=admin.".to_owned();
    loop {
        let got = page(s, &r.token, &path).await;
        paged.extend(got.events);
        let Some(next) = got.next else { break };
        path = format!("/admin/audit?limit=2&action=admin.&cursor={next}");
    }
    assert_eq!(paged.len(), 5);
    let mut ids: Vec<Uuid> = paged.iter().map(|e| e.id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 5, "no event twice");
    // Limits: 1 to 200, 50 by default.
    assert_eq!(page(s, &r.token, "/admin/audit?limit=200").await.events.len(), 7);
    assert_eq!(page(s, &r.token, "/admin/audit?limit=1").await.events.len(), 1);
    for bad in [
        "limit=0",
        "limit=201",
        "limit=x",
        "cursor=garbage",
        "user=quincy",
        "user=",
        "action=nope",
        "action=auth",
        "action=auth.%25",
        "action=%27%3B",
        "before_ms=-1",
    ] {
        let (status, code) = refusal(call(s, Some(&r.token), Method::GET, &format!("/admin/audit?{bad}"), None).await).await;
        assert_eq!((status, code.as_str()), (StatusCode::BAD_REQUEST, "invalid"), "{bad}");
    }
    let (status, code) = refusal(call(s, Some(&r.token), Method::GET, "/admin/audit/export?user=quincy", None).await).await;
    assert_eq!((status, code.as_str()), (StatusCode::BAD_REQUEST, "invalid"));
    // A cursor from another time is only a position: it still parses, and one for the future gives everything.
    let future = craft_cloud_meta::AuditCursor { at_ms: i64::MAX, id: Uuid::from_u128(u128::MAX) };
    use base64::Engine;
    let cursor = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(future.to_bytes());
    assert_eq!(page(s, &r.token, &format!("/admin/audit?cursor={cursor}")).await.events.len(), 7);
}

async fn export_text(s: &Server, token: &str, query: &str) -> (reqwest::header::HeaderMap, String) {
    let res = call(s, Some(token), Method::GET, &format!("/admin/audit/export{query}"), None).await;
    assert_eq!(res.status(), StatusCode::OK);
    let headers = res.headers().clone();
    (headers, res.text().await.unwrap())
}

/// The export: NDJSON, in pages (of five here, through a channel of one), ending in a line that says it is complete, holding
/// what existed when it began, in the order of the list and with its filters.
async fn the_export_streams_ndjson_and_ends_with_complete(s: &Server) {
    let root = add_user(s, "rootuser", Role::Admin).await;
    let quincy = add_user(s, "quincy", Role::Member).await;
    let r = login(s, "rootuser", json!({})).await;
    seed(s, 12, quincy).await;
    // Events dated in the future are not part of an export that begins now.
    let future = now_ms() + 24 * 60 * MINUTE;
    for i in 0..2 {
        let failure = AuditFailure { action: AuditAction::AuthLoginFailed, target: Some(craft_cloud_meta::UserId(root)), step: Step::Password };
        s.meta.audit_record_failure(failure, future + i * MINUTE).await.unwrap();
    }
    let expected =
        s.meta.audit_list(&AuditQuery { until_ms: Some(now_ms() + 60 * MINUTE), limit: 1000, ..AuditQuery::default() }).await.unwrap().len();
    assert_eq!(expected, 13, "twelve seeded, and the administrator's sign-in");

    let (headers, text) = export_text(s, &r.token, "").await;
    assert_eq!(headers[reqwest::header::CONTENT_TYPE], "application/x-ndjson");
    assert_eq!(headers[reqwest::header::CONTENT_DISPOSITION], "attachment; filename=\"craft-cloud-audit.ndjson\"");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), expected + 1);
    let events: Vec<AuditEvent> = lines[..expected].iter().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert!(events.iter().all(|e| e.at_ms < future), "a future event is not in an export that began before it");
    let end: AuditExportEnd = serde_json::from_str(lines[expected]).unwrap();
    assert_eq!((end.complete, end.truncated, end.exported), (Some(true), None, expected as u64));
    // Newest first, by (at_ms, id), as the list has them.
    assert!(events.windows(2).all(|w| (w[0].at_ms, w[0].id) > (w[1].at_ms, w[1].id)));
    // The same filters as the list.
    let (_, text) = export_text(s, &r.token, &format!("?user={quincy}&action=auth.")).await;
    assert_eq!(text.lines().count(), 13, "twelve events and the end");
    // `before_ms` limits an export as it does a list, besides its fixed end; a negative one is refused.
    let (_, text) = export_text(s, &r.token, "?before_ms=1").await;
    assert_eq!(text.trim(), r#"{"complete":true,"exported":0}"#);
    let (_, text) = export_text(s, &r.token, &format!("?before_ms={}", 1_000_000 + 5 * MINUTE)).await;
    assert_eq!(text.lines().count(), 6, "five seeded events and the end");
    let (status, code) = refusal(call(s, Some(&r.token), Method::GET, "/admin/audit/export?before_ms=-1", None).await).await;
    assert_eq!((status, code.as_str()), (StatusCode::BAD_REQUEST, "invalid"));
    let (_, text) = export_text(s, &r.token, "?action=admin.").await;
    assert_eq!(text.trim(), r#"{"complete":true,"exported":0}"#, "nothing to export is still complete");
}

/// More events than the export's limit (seven here): it holds the limit's worth, newest first, and ends with `truncated`;
/// as many as the limit, or fewer, end with `complete`.
async fn a_longer_log_is_cut_and_says_so(s: &Server) {
    let root = add_user(s, "rootuser", Role::Admin).await;
    let quincy = add_user(s, "quincy", Role::Member).await;
    let bobby = add_user(s, "bobby", Role::Member).await;
    let r = login(s, "rootuser", json!({})).await;
    seed(s, 11, quincy).await;
    seed(s, 7, bobby).await;
    // Nineteen events, at most seven in the export.
    let (_, text) = export_text(s, &r.token, "").await;
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 8);
    assert_eq!(lines[7], r#"{"truncated":true,"exported":7}"#);
    for line in &lines[..7] {
        serde_json::from_str::<AuditEvent>(line).unwrap();
    }
    let (_, text) = export_text(s, &r.token, &format!("?user={quincy}")).await;
    assert_eq!(text.lines().last().unwrap(), r#"{"truncated":true,"exported":7}"#, "eleven events of his");
    // Exactly as many as the limit: nothing is left out, so it is complete.
    let (_, text) = export_text(s, &r.token, &format!("?user={bobby}")).await;
    assert_eq!((text.lines().count(), text.lines().last().unwrap()), (8, r#"{"complete":true,"exported":7}"#));
    let (_, text) = export_text(s, &r.token, &format!("?user={root}")).await;
    assert_eq!(text.lines().last().unwrap(), r#"{"complete":true,"exported":1}"#);
}

/// A database error in the middle of the stream ends the connection in error: what arrived is there, and there is no last
/// line, so no client can take it for a whole export.
async fn a_database_error_in_the_middle_ends_the_stream_without_a_last_line(s: &Server) {
    add_user(s, "rootuser", Role::Admin).await;
    let quincy = add_user(s, "quincy", Role::Member).await;
    let r = login(s, "rootuser", json!({})).await;
    seed(s, 40, quincy).await;
    // A row the server can't read, in the middle of the log: the pages before it are fine.
    {
        use sqlx::Connection;
        let mut conn = sqlx::AnyConnection::connect(&s.url).await.unwrap();
        sqlx::query("INSERT INTO audit_events (id, at_ms, action, outcome) VALUES ($1, $2, 'bogus.action', 'success')")
            .bind(Uuid::now_v7().as_bytes().to_vec())
            .bind(1_000_000i64 + 10 * MINUTE + 1)
            .execute(&mut conn)
            .await
            .unwrap();
    }
    let mut res = call(s, Some(&r.token), Method::GET, "/admin/audit/export", None).await;
    assert_eq!(res.status(), StatusCode::OK);
    let mut received = Vec::new();
    let broke = loop {
        match res.chunk().await {
            Ok(Some(chunk)) => received.extend_from_slice(&chunk),
            Ok(None) => break false,
            Err(_) => break true,
        }
    };
    assert!(broke, "the stream ended as if it were whole");
    let text = String::from_utf8(received).unwrap();
    assert!(text.lines().count() >= 10, "the pages before the bad row arrived: {}", text.lines().count());
    assert!(!text.contains("complete") && !text.contains("truncated"), "{text}");
    for line in text.lines() {
        serde_json::from_str::<AuditEvent>(line).unwrap();
    }
}

/// Nothing writes an event but a change, and a change fails with its event: with the log broken a change is refused and
/// changes nothing, and a sign-in is refused, while a wrong password is still answered as one, and the limit on tries
/// still holds.
async fn a_broken_log_fails_the_changes_and_not_the_abuse_protection(s: &Server) {
    let root = add_user(s, "rootuser", Role::Admin).await;
    let quincy = add_user(s, "quincy", Role::Member).await;
    add_user(s, "guessed", Role::Member).await;
    let q = login(s, "quincy", json!({})).await;
    let r = login(s, "rootuser", json!({})).await;
    let events_before = whole_log(s, &r.token).await.len();

    break_log(s).await;
    // A change is refused and does nothing.
    let (status, code) = refusal(admin(s, &r.token, Method::POST, &format!("/admin/users/{quincy}/disable"), json!({})).await).await;
    assert_eq!((status, code.as_str()), (StatusCode::INTERNAL_SERVER_ERROR, "internal"));
    let (status, _) = refusal(call(s, Some(&q.token), Method::DELETE, &format!("/devices/{}", q.device), None).await).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let (status, _) = refusal(
        call(
            s,
            Some(&q.token),
            Method::POST,
            "/account/password",
            Some(json!({ "current_password": PASSWORD, "new_password": "never set password" })),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let (status, _) = refusal(admin(s, &r.token, Method::PATCH, &format!("/admin/users/{quincy}"), json!({ "role": "admin" })).await).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    // A sign-in that would work needs its event, and is refused.
    assert_eq!(sign_in_with(s, "quincy", PASSWORD, json!({})).await.status(), StatusCode::INTERNAL_SERVER_ERROR);
    // A wrong password is answered as ever, and the tries are counted as ever: the tenth is still the last.
    for n in 0..10 {
        let res = sign_in_with(s, "guessed", "a wrong password", json!({})).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "try {n}");
    }
    let (status, code) = refusal(sign_in_with(s, "guessed", "a wrong password", json!({})).await).await;
    assert_eq!((status, code.as_str()), (StatusCode::TOO_MANY_REQUESTS, "rate_limited"));
    repair_log(s).await;

    // Nothing changed, and nothing was written.
    assert_eq!(whole_log(s, &r.token).await.len(), events_before);
    let devices: Vec<Value> = call(s, Some(&q.token), Method::GET, "/devices", None).await.json().await.unwrap();
    assert!(devices.iter().all(|d| d["revoked"] == false));
    let account: Value = call(s, Some(&q.token), Method::GET, "/account", None).await.json().await.unwrap();
    assert_eq!(account["role"], "member");
    assert_eq!(account["password_changed_ms"], Value::Null);
    assert_eq!(s.meta.admin_user(craft_cloud_meta::UserId(quincy), now_ms()).await.unwrap().unwrap().disabled_ms, None);
    // And with the log back, the same changes work, and are written.
    assert_eq!(admin(s, &r.token, Method::POST, &format!("/admin/users/{quincy}/disable"), json!({})).await.status(), StatusCode::NO_CONTENT);
    let log = whole_log(s, &r.token).await;
    assert_eq!(log.len(), events_before + 1);
    assert!(log.iter().any(|e| e.action == "admin.user_disabled" && e.actor_user == Some(root)));
}

on_every_backend!(
    paged_export,
    craft_cloud_server::audit::AuditExportLimits { page: 5, channel: 1, max_rows: 100_000 };
    a_session_leaves_its_events_and_no_secret,
    the_account_sees_only_its_own_activity,
    the_admin_list_is_for_admins_and_checks_its_parameters,
    a_demoted_admin_sees_no_details_of_their_acts_on_others,
    the_export_streams_ndjson_and_ends_with_complete,
    a_database_error_in_the_middle_ends_the_stream_without_a_last_line,
    a_broken_log_fails_the_changes_and_not_the_abuse_protection,
);

on_every_backend!(
    short_export,
    craft_cloud_server::audit::AuditExportLimits { page: 5, channel: 1, max_rows: 7 };
    a_longer_log_is_cut_and_says_so,
);
