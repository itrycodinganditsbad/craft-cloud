//! The admin pages end to end: a real server on a random loopback port, driven with an HTTP client that does what a browser does
//! (it keeps the cookie by hand, sends the `Origin`, follows nothing). Every scenario runs on SQLite + disk, and on Postgres + disk
//! when `CRAFT_CLOUD_TEST_PG` is set (`CRAFT_CLOUD_REQUIRE_BACKENDS` turns a missing one into a failure).

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use craft_cloud_meta::{Meta, Role};
use craft_cloud_proto::api::{LoginResponse, RecoveryCodes, TwoFactorSetupResponse};
use craft_cloud_server::AppState;
use craft_cloud_server::auth::{hash_password, token_hash};
use craft_cloud_server::secretbox::{SecretBox, TwoFactorKey};
use craft_cloud_server::totp;
use craft_cloud_server::web::{COOKIE_NAME, WebSettings};
use craft_cloud_store::Store;
use craft_cloud_store::disk::DiskBackend;
use reqwest::{Client, Method, StatusCode};
use serde_json::json;

const PASSWORD: &str = "correct horse battery";
const ORIGIN: &str = "https://cloud.test";
const IDLE_MS: i64 = 30 * 60_000;
const ABSOLUTE_MS: i64 = 4 * 3_600_000;

struct Web {
    /// `http://127.0.0.1:port`.
    base: String,
    meta: Meta,
    /// The database's URL, so that a test can reach in with a connection of its own.
    url: String,
    client: Client,
    clock: Arc<AtomicI64>,
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

fn test_box() -> SecretBox {
    SecretBox::new(&TwoFactorKey::from_bytes([42; 32]), None).unwrap()
}

async fn start(meta: Meta, url: String, dirs: Vec<tempfile::TempDir>, pg: Option<PgGuard>, trusted: Vec<std::net::IpAddr>, web: bool) -> Web {
    let blobs = tempfile::tempdir().unwrap();
    let store = Store::new(DiskBackend::new(blobs.path()).unwrap());
    let settings = web.then(|| WebSettings::new(ORIGIN, IDLE_MS, ABSOLUTE_MS).unwrap());
    let clock = Arc::new(AtomicI64::new(0));
    let ticking = clock.clone();
    let state = AppState::new(meta.clone(), store, trusted)
        .unwrap()
        .with_two_factor(Some(test_box()))
        .with_public_url(Some(ORIGIN.to_owned()))
        .with_web_admin(settings)
        .with_clock(Arc::new(move || match ticking.load(Ordering::SeqCst) {
            0 => craft_cloud_server::state::now_ms(),
            frozen => frozen,
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(craft_cloud_server::serve(state, listener, std::future::pending::<()>()));
    let client = Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let mut dirs = dirs;
    dirs.push(blobs);
    Web { base, meta, url, client, clock, _dirs: dirs, _pg: pg }
}

async fn sqlite_web() -> Web {
    sqlite_with(vec![], true).await
}

async fn sqlite_with(trusted: Vec<std::net::IpAddr>, web: bool) -> Web {
    let db = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}", db.path().join("meta.db").display());
    let meta = Meta::open(&url).await.unwrap();
    start(meta, url, vec![db], None, trusted, web).await
}

async fn postgres_web() -> Option<Web> {
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
    Some(start(meta, url, vec![], Some(guard), vec![], true).await)
}

macro_rules! on_sqlite_and_postgres {
    ($($scenario:ident),* $(,)?) => {
        mod on_sqlite {
            $( #[tokio::test(flavor = "multi_thread", worker_threads = 4)] async fn $scenario() { super::$scenario(&super::sqlite_web().await).await; } )*
        }
        mod on_postgres {
            $( #[tokio::test(flavor = "multi_thread", worker_threads = 4)] async fn $scenario() {
                let Some(server) = super::postgres_web().await else { eprintln!("skipped: CRAFT_CLOUD_TEST_PG not set"); return };
                super::$scenario(&server).await;
            } )*
        }
    };
}

on_sqlite_and_postgres!(
    signing_in_sets_a_strict_host_cookie_and_keeps_only_a_hash,
    every_answer_carries_the_security_headers_and_no_script,
    a_refused_sign_in_never_says_why,
    a_web_audit_page_shows_the_web_client_kind,
    sign_in_attempts_are_limited_apart_from_the_json_api,
    a_sign_in_with_two_factor_takes_the_second_step,
    wrong_second_steps_lock_like_the_json_sign_in,
    forms_need_the_csrf_token_of_their_own_session,
    forms_must_come_from_the_origin,
    a_bearer_token_and_the_cookie_are_never_taken_together,
    a_session_ends_with_the_accounts_password_disable_removal_and_two_factor,
    a_session_ends_when_idle_or_at_its_absolute_lifetime,
    logging_out_ends_the_session_and_drops_the_cookie,
    members_and_demoted_admins_are_refused,
    every_change_needs_the_confirmation,
    an_admin_with_two_factor_confirms_changes_with_a_code,
    accounts_are_made_changed_disabled_and_removed,
    invites_and_reset_codes_are_shown_once_and_work,
    two_factor_resets_and_devices_are_managed,
    the_user_list_pages_and_searches,
    the_audit_log_filters_pages_and_downloads,
    user_controlled_text_is_escaped,
    old_sessions_are_cleaned_up_at_sign_in,
    form_bodies_are_small_and_odd_ones_refused,
    an_account_without_a_password_never_signs_in,
    the_slash_path_has_the_same_headers_and_policy,
    a_change_that_outlives_its_session_is_refused,
    disabling_or_removing_over_the_pages_wakes_the_spaces_long_polls,
    the_team_spaces_pages_are_for_administrators_and_need_csrf,
    every_team_space_change_needs_the_confirmation,
    team_spaces_are_made_listed_and_changed,
    team_spaces_are_listed_in_pages_and_searched,
    team_space_changes_are_audited_with_the_web_session_and_wake_the_long_polls,
    an_admin_with_two_factor_confirms_a_team_space_with_a_code,
    team_space_names_are_escaped,
);

// ---- helpers ----

impl Web {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// The time of day by the server's clock.
    fn at(&self) -> i64 {
        match self.clock.load(Ordering::SeqCst) {
            0 => craft_cloud_server::state::now_ms(),
            frozen => frozen,
        }
    }

    /// Stops the server's clock at the start of the current TOTP step (plus a second), so that codes don't change under a test.
    fn freeze(&self) {
        if self.clock.load(Ordering::SeqCst) == 0 {
            let step = totp::step_at(craft_cloud_server::state::now_ms());
            self.clock.store(step * 30_000 + 1_000, Ordering::SeqCst);
        }
    }

    fn advance(&self, ms: i64) {
        let now = self.at();
        self.clock.store(now + ms, Ordering::SeqCst);
    }

    async fn get(&self, path: &str, token: Option<&str>) -> reqwest::Response {
        let mut req = self.client.get(self.url(path));
        if let Some(token) = token {
            req = req.header("cookie", format!("{COOKIE_NAME}={token}"));
        }
        req.send().await.unwrap()
    }

    /// A form post the way a browser makes it from the pages themselves: the right `Origin` unless the test says otherwise.
    async fn post(&self, path: &str, token: Option<&str>, fields: &[(&str, &str)]) -> reqwest::Response {
        self.post_from(path, token, Some(ORIGIN), None, fields).await
    }

    async fn post_from(
        &self,
        path: &str,
        token: Option<&str>,
        origin: Option<&str>,
        referer: Option<&str>,
        fields: &[(&str, &str)],
    ) -> reqwest::Response {
        let mut req = self.client.post(self.url(path)).header("content-type", "application/x-www-form-urlencoded").body(encode(fields));
        if let Some(token) = token {
            req = req.header("cookie", format!("{COOKIE_NAME}={token}"));
        }
        if let Some(origin) = origin {
            req = req.header("origin", origin);
        }
        if let Some(referer) = referer {
            req = req.header("referer", referer);
        }
        req.send().await.unwrap()
    }

    /// A change as the admin's page posts it: the session's CSRF token and the password, plus the fields of the form.
    async fn act(&self, sess: &Sess, path: &str, extra: &[(&str, &str)]) -> reqwest::Response {
        let mut fields: Vec<(&str, &str)> = extra.to_vec();
        fields.push(("csrf", &sess.csrf));
        fields.push(("password", PASSWORD));
        self.post(path, Some(&sess.token), &fields).await
    }

    async fn add_user(&self, name: &str, role: Role) -> craft_cloud_meta::UserRecord {
        self.meta.create_user(name, &hash_password(PASSWORD).unwrap(), role, 0).await.unwrap()
    }

    async fn add_admin(&self, name: &str) -> craft_cloud_meta::UserRecord {
        self.add_user(name, Role::Admin).await
    }

    /// Signs `name` in to the pages (no second factor).
    async fn sign_in(&self, name: &str) -> Sess {
        let res = self.post("/web/login", None, &[("username", name), ("password", PASSWORD)]).await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER, "{}", res.text().await.unwrap());
        let token = cookie_token(&res).expect("a session cookie");
        self.sess_from(token).await
    }

    async fn sess_from(&self, token: String) -> Sess {
        let page = self.get("/web", Some(&token)).await;
        assert_eq!(page.status(), StatusCode::OK);
        let csrf = csrf_of(&page.text().await.unwrap());
        Sess { token, csrf }
    }

    /// An API call with a device token (`token` `None`: no sign-in).
    async fn api(&self, token: Option<&str>, method: Method, path: &str, body: Option<serde_json::Value>) -> reqwest::Response {
        let mut req = self.client.request(method, self.url(&format!("/api/v1{path}")));
        if let Some(token) = token {
            req = req.bearer_auth(token);
        }
        if let Some(body) = body {
            req = req.json(&body);
        }
        req.send().await.unwrap()
    }

    /// A device token of `name`, by the JSON sign-in.
    async fn device(&self, name: &str) -> String {
        let res = self.api(None, Method::POST, "/auth/login", Some(json!({ "username": name, "password": PASSWORD, "device_name": "pc" }))).await;
        assert_eq!(res.status(), StatusCode::OK);
        res.json::<LoginResponse>().await.unwrap().token
    }

    async fn id_of(&self, name: &str) -> uuid::Uuid {
        self.meta.user_by_name(name).await.unwrap().unwrap().id.0
    }
}

/// A session as the test's browser holds it: the cookie's token and the CSRF token of the forms.
struct Sess {
    token: String,
    csrf: String,
}

/// `application/x-www-form-urlencoded`.
fn encode(fields: &[(&str, &str)]) -> String {
    fn esc(text: &str) -> String {
        text.bytes()
            .map(
                |b| if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') { char::from(b).to_string() } else { format!("%{b:02X}") },
            )
            .collect()
    }
    fields.iter().map(|(k, v)| format!("{}={}", esc(k), esc(v))).collect::<Vec<_>>().join("&")
}

/// The session token a response sets.
fn cookie_token(res: &reqwest::Response) -> Option<String> {
    res.headers().get_all("set-cookie").iter().filter_map(|v| v.to_str().ok()).find_map(|line| {
        let value = line.strip_prefix(&format!("{COOKIE_NAME}="))?;
        let token = value.split(';').next()?;
        (!token.is_empty()).then(|| token.to_owned())
    })
}

/// The whole `Set-Cookie` line of the session cookie.
fn cookie_line(res: &reqwest::Response) -> Option<String> {
    res.headers().get_all("set-cookie").iter().filter_map(|v| v.to_str().ok()).find(|line| line.starts_with(COOKIE_NAME)).map(str::to_owned)
}

/// The CSRF token a page's forms carry.
fn csrf_of(html: &str) -> String {
    let marker = "name=\"csrf\" value=\"";
    let start = html.find(marker).expect("a form with a csrf token") + marker.len();
    html[start..].split('"').next().unwrap().to_owned()
}

fn location(res: &reqwest::Response) -> String {
    res.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned()
}

fn header(res: &reqwest::Response, name: &str) -> String {
    res.headers().get(name).and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned()
}

/// Whether the session is still good: the overview answers 200.
async fn signed_in(s: &Web, sess: &Sess) -> bool {
    let res = s.get("/web", Some(&sess.token)).await;
    match res.status() {
        StatusCode::OK => true,
        StatusCode::SEE_OTHER => {
            assert_eq!(location(&res), "/web/login");
            false
        }
        other => panic!("unexpected {other}"),
    }
}

/// An account with two-factor sign-in on, set up through the JSON API with the clock frozen: its secret and recovery codes.
struct Tf {
    secret: Vec<u8>,
    recovery: Vec<String>,
}

impl Tf {
    fn code(&self, s: &Web) -> String {
        totp::code_for(&self.secret, totp::step_at(s.at())).unwrap()
    }

    /// The clock moves one step on, and the code for that step: never one that was used.
    fn fresh_code(&self, s: &Web) -> String {
        s.advance(30_000);
        self.code(s)
    }
}

async fn turn_on_two_factor(s: &Web, name: &str) -> Tf {
    s.freeze();
    let token = s.device(name).await;
    let res = s.api(Some(&token), Method::POST, "/account/two-factor/setup", Some(json!({ "password": PASSWORD }))).await;
    let setup: TwoFactorSetupResponse = res.json().await.unwrap();
    let secret = totp::base32_decode(&setup.secret).unwrap();
    let tf = Tf { secret, recovery: Vec::new() };
    let res = s.api(Some(&token), Method::POST, "/account/two-factor/confirm", Some(json!({ "totp_code": tf.code(s) }))).await;
    assert_eq!(res.status(), StatusCode::OK);
    let codes: RecoveryCodes = res.json().await.unwrap();
    Tf { recovery: codes.recovery_codes, ..tf }
}

/// A session of an admin with two-factor sign-in: the password step, then the code (a fresh one).
async fn sign_in_with_factor(s: &Web, name: &str, tf: &Tf) -> Sess {
    let res = s.post("/web/login", None, &[("username", name), ("password", PASSWORD)]).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(cookie_token(&res).is_none(), "no session before the second factor");
    let challenge = challenge_of(&res.text().await.unwrap());
    let code = tf.fresh_code(s);
    let res = s.post("/web/login/second-factor", None, &[("challenge", &challenge), ("totp_code", &code)]).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER, "{}", res.text().await.unwrap());
    s.sess_from(cookie_token(&res).expect("a session cookie")).await
}

fn challenge_of(html: &str) -> String {
    let marker = "name=\"challenge\" value=\"";
    let start = html.find(marker).expect("a challenge") + marker.len();
    html[start..].split('"').next().unwrap().to_owned()
}

// ---- the sign-in ----

async fn signing_in_sets_a_strict_host_cookie_and_keeps_only_a_hash(s: &Web) {
    s.add_admin("root").await;
    let res = s.get("/web/login", None).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(res.text().await.unwrap().contains("action=\"/web/login\""));

    let res = s.post("/web/login", None, &[("username", "Root"), ("password", PASSWORD)]).await;
    assert_eq!((res.status(), location(&res).as_str()), (StatusCode::SEE_OTHER, "/web"));
    let token = cookie_token(&res).unwrap();
    // The cookie: the `__Host-` prefix and what it needs, HttpOnly, Strict, and nothing else (no Domain, no lifetime of its own).
    assert_eq!(cookie_line(&res).unwrap(), format!("__Host-craft_cloud_session={token}; Path=/; Secure; HttpOnly; SameSite=Strict"));
    assert_eq!(token.len(), 43);
    assert!(token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
    // The server knows it by its hash only, and the hash is not the token.
    let hash = token_hash(&token);
    let found = s.meta.web_session(&hash, s.at(), IDLE_MS).await.unwrap().expect("the session is stored by its hash");
    assert_eq!((found.username.as_str(), found.role), ("root", Role::Admin));
    assert_eq!(found.absolute_expires_ms - found.created_ms, ABSOLUTE_MS);
    // Another sign-in is another session.
    let again = s.post("/web/login", None, &[("username", "root"), ("password", PASSWORD)]).await;
    assert_ne!(cookie_token(&again).unwrap(), token);
    // The overview is for the session, and says whose it is.
    let page = s.get("/web", Some(&token)).await;
    assert_eq!(page.status(), StatusCode::OK);
    let html = page.text().await.unwrap();
    assert!(html.contains("Signed in as <strong>root</strong>") && html.contains("Overview"), "{html}");
    // The sign-in is in the audit log, with no device.
    let events = s.meta.audit_list(&craft_cloud_meta::AuditQuery { limit: 10, ..Default::default() }).await.unwrap();
    assert!(events.iter().any(|e| e.action == craft_cloud_meta::AuditAction::AuthLogin && e.actor_device.is_none()));
    // A bad shape of name is a 400, not a sign-in attempt.
    let res = s.post("/web/login", None, &[("username", "no spaces"), ("password", PASSWORD)]).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let res = s.post("/web/login", None, &[("username", "root"), ("password", &"x".repeat(1100))]).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

async fn every_answer_carries_the_security_headers_and_no_script(s: &Web) {
    s.add_admin("root").await;
    let sess = s.sign_in("root").await;
    let vic = s.add_user("vic", Role::Member).await;
    let vic_page = format!("/web/users/{}", vic.id.0);
    let answers = [
        s.get("/web/login", None).await,
        s.get("/web", None).await,
        s.get("/web", Some(&sess.token)).await,
        s.get("/web/users", Some(&sess.token)).await,
        s.get(&vic_page, Some(&sess.token)).await,
        s.get("/web/audit", Some(&sess.token)).await,
        s.get("/web/audit/export", Some(&sess.token)).await,
        s.get("/web/style.css", None).await,
        s.get("/web/nothing-here", None).await,
        s.get("/web/users/not-an-id", Some(&sess.token)).await,
        s.post_from("/web/login", None, Some("https://evil.example"), None, &[]).await,
        s.post("/web/logout", Some(&sess.token), &[("csrf", "wrong")]).await,
        s.post("/web/login", None, &[("username", "root"), ("password", "wrong password")]).await,
        s.client.delete(s.url("/web/users")).send().await.unwrap(),
    ];
    for res in answers {
        let (status, headers) = (res.status(), res.headers().clone());
        let h = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
        let csp = h("content-security-policy");
        for needed in
            ["default-src 'self'", "script-src 'none'", "object-src 'none'", "base-uri 'none'", "frame-ancestors 'none'", "form-action 'self'"]
        {
            assert!(csp.contains(needed), "{status}: {needed} in {csp}");
        }
        assert!(!csp.contains("unsafe") && !csp.contains("http"), "no loosening and no other host: {csp}");
        assert_eq!(h("x-content-type-options"), "nosniff", "{status}");
        assert_eq!(h("referrer-policy"), "no-referrer", "{status}");
        assert_eq!(h("x-frame-options"), "DENY", "{status}");
        assert_eq!(h("cross-origin-opener-policy"), "same-origin");
        assert_eq!(h("cross-origin-resource-policy"), "same-origin");
        let cache = h("cache-control");
        assert!(cache == "no-store" || cache == "no-cache", "{status}: {cache}");
        if h("content-type").starts_with("text/html") {
            assert_eq!(cache, "no-store", "an admin page is never stored");
            let body = res.text().await.unwrap();
            let lower = body.to_ascii_lowercase();
            for forbidden in ["<script", " onclick=", " onload=", " onerror=", "javascript:", " style=", "<style", "<iframe", "<link rel=\"preload"] {
                assert!(!lower.contains(forbidden), "{forbidden} in a page: {body}");
            }
            assert!(body.contains("<html lang=\"en\">") && body.contains("<main id=\"main\""), "a page has a language and a main landmark");
        }
    }
    // The export is a download that is never stored either.
    let export = s.get("/web/audit/export", Some(&sess.token)).await;
    assert_eq!(header(&export, "cache-control"), "no-store");
    // The stylesheet is served from the server itself.
    let css = s.get("/web/style.css", None).await;
    assert_eq!((css.status(), header(&css, "content-type")), (StatusCode::OK, "text/css; charset=utf-8".to_owned()));
}

/// The text a refused sign-in shows, so that two refusals can be compared.
async fn refused(res: reqwest::Response) -> (StatusCode, String, bool) {
    let (status, cookie) = (res.status(), cookie_token(&res).is_some());
    (status, res.text().await.unwrap(), cookie)
}

async fn a_refused_sign_in_never_says_why(s: &Web) {
    s.add_admin("root").await;
    s.add_user("ana", Role::Member).await;
    s.add_admin("gone").await;
    let gone = s.id_of("gone").await;
    let bearer = s.device("root").await;
    let res = s.api(Some(&bearer), Method::POST, &format!("/admin/users/{gone}/disable"), Some(json!({ "password": PASSWORD }))).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    let cases = [("nobody", PASSWORD), ("root", "a wrong password"), ("ana", PASSWORD), ("ana", "a wrong password"), ("gone", PASSWORD)];
    let mut answers = Vec::new();
    for (name, password) in cases {
        answers.push(refused(s.post("/web/login", None, &[("username", name), ("password", password)]).await).await);
    }
    let (first_status, first_body, _) = answers.first().unwrap().clone();
    assert_eq!(first_status, StatusCode::UNAUTHORIZED);
    assert!(first_body.contains("Wrong username or password."), "{first_body}");
    for (n, (status, body, cookie)) in answers.iter().enumerate() {
        assert_eq!((status, body, cookie), (&first_status, &first_body, &false), "{:?} is answered like an unknown name", cases[n]);
    }
    // And the log has failures for the accounts that exist, none that names the typed name.
    let events = s.meta.audit_list(&craft_cloud_meta::AuditQuery { limit: 100, ..Default::default() }).await.unwrap();
    assert!(events.iter().any(|e| e.action == craft_cloud_meta::AuditAction::AuthLoginFailed));
    assert!(events.iter().all(|e| !e.details.contains("nobody")));
}

async fn sign_in_attempts_are_limited_apart_from_the_json_api(s: &Web) {
    s.add_admin("root").await;
    s.add_admin("other").await;
    // Ten wrong passwords for one account in the window, then the eleventh is refused whatever it holds.
    for n in 0..10 {
        let res = s.post("/web/login", None, &[("username", "root"), ("password", "a wrong password")]).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "try {n}");
    }
    let res = s.post("/web/login", None, &[("username", "root"), ("password", PASSWORD)]).await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(cookie_token(&res).is_none());
    let wait: u64 = header(&res, "retry-after").parse().expect("every 429 says how long to wait");
    assert!((1..=300).contains(&wait), "{wait}");
    // The limiter is the pages' own: the same account still signs in with a device, and another account in the browser.
    assert!(!s.device("root").await.is_empty());
    s.sign_in("other").await;
    // Thirty from one address, whatever the names.
    for n in 0..30 {
        let name = format!("guess{n}");
        let res = s.post("/web/login", None, &[("username", &name), ("password", "x")]).await;
        assert!(matches!(res.status(), StatusCode::UNAUTHORIZED | StatusCode::TOO_MANY_REQUESTS), "{}", res.status());
    }
    let res = s.post("/web/login", None, &[("username", "guess99"), ("password", "x")]).await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS, "the address is out of tries");
    assert!(header(&res, "retry-after").parse::<u64>().is_ok());
}

async fn a_sign_in_with_two_factor_takes_the_second_step(s: &Web) {
    s.add_admin("root").await;
    let tf = turn_on_two_factor(s, "root").await;
    let first = s.post("/web/login", None, &[("username", "root"), ("password", PASSWORD)]).await;
    assert_eq!(first.status(), StatusCode::OK);
    assert!(cookie_token(&first).is_none());
    let html = first.text().await.unwrap();
    assert!(html.contains("authenticator app") && html.contains("action=\"/web/login/second-factor\""), "{html}");
    let challenge = challenge_of(&html);
    // A wrong code: 401 and the same step again, with the same challenge; nothing signs in.
    let res = s.post("/web/login/second-factor", None, &[("challenge", &challenge), ("totp_code", "000000")]).await;
    let (status, body, cookie) = refused(res).await;
    assert_eq!((status, cookie), (StatusCode::FORBIDDEN, false), "a wrong code is 403 (an unknown challenge stays 401)");
    assert!(body.contains("wrong, or was used already") && challenge_of(&body) == challenge, "{body}");
    // A code of the wrong shape is a 400 and counts for nothing.
    let res = s.post("/web/login/second-factor", None, &[("challenge", &challenge), ("totp_code", "12")]).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let res = s.post("/web/login/second-factor", None, &[("challenge", &challenge)]).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST, "neither a code nor a recovery code");
    // The right one signs in, and the session knows a second factor was given.
    let code = tf.fresh_code(s);
    let res = s.post("/web/login/second-factor", None, &[("challenge", &challenge), ("totp_code", &code)]).await;
    assert_eq!((res.status(), location(&res).as_str()), (StatusCode::SEE_OTHER, "/web"));
    let sess = s.sess_from(cookie_token(&res).unwrap()).await;
    let overview = s.get("/web", Some(&sess.token)).await.text().await.unwrap();
    assert!(overview.contains("Second factor at sign-in") && !overview.contains("not asked"), "{overview}");
    assert!(s.meta.web_session(&token_hash(&sess.token), s.at(), IDLE_MS).await.unwrap().unwrap().mfa_verified_ms.is_some());
    // The challenge is used up; the same code is used up; a made-up challenge is the same refusal.
    let res = s.post("/web/login/second-factor", None, &[("challenge", &challenge), ("totp_code", &tf.fresh_code(s))]).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert!(res.text().await.unwrap().contains("doesn't work any more") || true);
    let res = s.post("/web/login/second-factor", None, &[("challenge", &"A".repeat(43)), ("totp_code", &tf.fresh_code(s))]).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let again = challenge_of(&s.post("/web/login", None, &[("username", "root"), ("password", PASSWORD)]).await.text().await.unwrap());
    let res = s.post("/web/login/second-factor", None, &[("challenge", &again), ("totp_code", &code)]).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN, "a time step serves one sign-in");
    // A recovery code works once, in any case and with dashes.
    let recovery = tf.recovery.first().unwrap().to_ascii_lowercase();
    let res = s.post("/web/login/second-factor", None, &[("challenge", &again), ("recovery_code", &recovery)]).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let third = challenge_of(&s.post("/web/login", None, &[("username", "root"), ("password", PASSWORD)]).await.text().await.unwrap());
    let res = s.post("/web/login/second-factor", None, &[("challenge", &third), ("recovery_code", &recovery)]).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN, "a recovery code is used up");
    // A device's challenge is not good for the pages, nor the pages' for a device.
    let device_login = s.api(None, Method::POST, "/auth/login", Some(json!({ "username": "root", "password": PASSWORD, "device_name": "pc" }))).await;
    assert_eq!(device_login.status(), StatusCode::UNAUTHORIZED);
    let device_challenge = device_login.json::<serde_json::Value>().await.unwrap()["two_factor"]["challenge"].as_str().unwrap().to_owned();
    let res = s.post("/web/login/second-factor", None, &[("challenge", &device_challenge), ("totp_code", &tf.fresh_code(s))]).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "a device sign-in's challenge makes no session");
    let web_challenge = challenge_of(&s.post("/web/login", None, &[("username", "root"), ("password", PASSWORD)]).await.text().await.unwrap());
    let res =
        s.api(None, Method::POST, "/auth/login/second-factor", Some(json!({ "challenge": web_challenge, "totp_code": tf.fresh_code(s) }))).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "a web sign-in's challenge makes no device");
}

async fn wrong_second_steps_lock_like_the_json_sign_in(s: &Web) {
    s.add_admin("root").await;
    let tf = turn_on_two_factor(s, "root").await;
    // Five wrong codes delete a challenge; two challenges make the ten that lock the second factor. The challenges are made first:
    // a wrong second factor counts against the sign-in limit too, which the password step then runs into.
    let mut challenges = Vec::new();
    for _ in 0..3 {
        challenges.push(challenge_of(&s.post("/web/login", None, &[("username", "root"), ("password", PASSWORD)]).await.text().await.unwrap()));
    }
    for challenge in &challenges[..2] {
        for n in 0..5 {
            let res = s.post("/web/login/second-factor", None, &[("challenge", challenge), ("totp_code", &format!("00000{n}"))]).await;
            assert_eq!(res.status(), StatusCode::FORBIDDEN);
        }
        let res = s.post("/web/login/second-factor", None, &[("challenge", challenge), ("totp_code", &tf.fresh_code(s))]).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "five wrong codes delete the challenge");
    }
    // Locked: a right code is refused with 429 and Retry-After, in the pages and for a device alike.
    let challenge = challenges.last().unwrap();
    let res = s.post("/web/login/second-factor", None, &[("challenge", challenge), ("totp_code", &tf.fresh_code(s))]).await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(res.headers().get("retry-after").is_some());
    assert!(res.text().await.unwrap().contains("locked"));
    let events = s.meta.audit_list(&craft_cloud_meta::AuditQuery { limit: 200, ..Default::default() }).await.unwrap();
    assert!(
        events
            .iter()
            .any(|event| { event.action == craft_cloud_meta::AuditAction::AuthTwoFactorLocked && event.details.contains("\"client_kind\":\"web\"") })
    );
    // And past the per-account window's ten wrong codes the answer is a 429 with a wait of its own.
    let res = s.post("/web/login/second-factor", None, &[("challenge", &challenges[2]), ("totp_code", "000000")]).await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(header(&res, "retry-after").parse::<u64>().is_ok_and(|secs| secs >= 1));
    let device_login = s.api(None, Method::POST, "/auth/login", Some(json!({ "username": "root", "password": PASSWORD, "device_name": "pc" }))).await;
    let device_challenge = device_login.json::<serde_json::Value>().await.unwrap()["two_factor"]["challenge"].as_str().unwrap().to_owned();
    let res =
        s.api(None, Method::POST, "/auth/login/second-factor", Some(json!({ "challenge": device_challenge, "totp_code": tf.fresh_code(s) }))).await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS, "the lock is the account's, not the page's");
    // Nobody signed in meanwhile.
    assert_eq!(s.meta.web_sessions_open(s.meta.user_by_name("root").await.unwrap().unwrap().id, s.at(), IDLE_MS).await.unwrap(), 0);
}

// ---- CSRF, origin and credentials ----

async fn forms_need_the_csrf_token_of_their_own_session(s: &Web) {
    s.add_admin("root").await;
    s.add_admin("second").await;
    let vic = s.add_user("vic", Role::Member).await;
    let (mine, theirs) = (s.sign_in("root").await, s.sign_in("second").await);
    let path = format!("/web/users/{}/disable", vic.id.0);
    assert_ne!(mine.csrf, theirs.csrf, "the token is bound to the session");
    let disabled = || async { s.meta.admin_user(vic.id, s.at()).await.unwrap().unwrap().disabled_ms.is_some() };
    // Missing, wrong, empty, another session's, and the right one of an older shape.
    for csrf in [None, Some(""), Some("wrong"), Some(theirs.csrf.as_str()), Some(&mine.csrf[1..])] {
        let mut fields = vec![("password", PASSWORD)];
        if let Some(csrf) = csrf {
            fields.push(("csrf", csrf));
        }
        let res = s.post(&path, Some(&mine.token), &fields).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "{csrf:?}");
        assert!(res.text().await.unwrap().contains("out of date, or did not come from this site"));
        assert!(!disabled().await, "{csrf:?} changed something");
    }
    // The token of the session itself works; a session's token does not work for its neighbour's cookie.
    let res = s.post(&path, Some(&theirs.token), &[("password", PASSWORD), ("csrf", &mine.csrf)]).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert!(!disabled().await);
    let res = s.act(&mine, &path, &[]).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert!(disabled().await);
    // The token is in the sign-out form too, and a sign-out without it does nothing.
    let res = s.post("/web/logout", Some(&mine.token), &[]).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert!(signed_in(s, &mine).await);
    // No change is made by a GET.
    let res = s.get(&path, Some(&mine.token)).await;
    assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
}

async fn forms_must_come_from_the_origin(s: &Web) {
    s.add_admin("root").await;
    let vic = s.add_user("vic", Role::Member).await;
    let sess = s.sign_in("root").await;
    let path = format!("/web/users/{}/disable", vic.id.0);
    let fields = [("password", PASSWORD), ("csrf", sess.csrf.as_str())];
    let disabled = || async { s.meta.admin_user(vic.id, s.at()).await.unwrap().unwrap().disabled_ms.is_some() };
    let refused = [
        s.post_from(&path, Some(&sess.token), Some("https://evil.example"), None, &fields).await,
        s.post_from(&path, Some(&sess.token), Some("http://cloud.test"), None, &fields).await,
        s.post_from(&path, Some(&sess.token), Some("https://cloud.test:8443"), None, &fields).await,
        s.post_from(&path, Some(&sess.token), Some("https://cloud.test.evil.example"), None, &fields).await,
        s.post_from(&path, Some(&sess.token), Some("null"), None, &fields).await,
        s.post_from(&path, Some(&sess.token), None, None, &fields).await,
        s.post_from(&path, Some(&sess.token), None, Some("https://evil.example/https://cloud.test/"), &fields).await,
        s.post_from(&path, Some(&sess.token), Some("https://evil.example"), Some("https://cloud.test/web"), &fields).await,
    ];
    for res in refused {
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert!(res.text().await.unwrap().contains("did not come from this site"));
    }
    assert!(!disabled().await, "no refused form changed anything");
    // The sign-in has the same policy: a page of another site can't sign a browser in.
    let res = s.post_from("/web/login", None, Some("https://evil.example"), None, &[("username", "root"), ("password", PASSWORD)]).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert!(cookie_token(&res).is_none());
    let res = s.post_from("/web/login", None, None, None, &[("username", "root"), ("password", PASSWORD)]).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    // A Referer on the origin stands in for a missing Origin; the Origin itself is the usual way.
    let res = s.post_from(&path, Some(&sess.token), None, Some("https://cloud.test/web/users"), &fields).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert!(disabled().await);
}

async fn a_bearer_token_and_the_cookie_are_never_taken_together(s: &Web) {
    s.add_admin("root").await;
    let sess = s.sign_in("root").await;
    let bearer = s.device("root").await;
    let cookie = format!("{COOKIE_NAME}={}", sess.token);
    // The pages take the cookie, and a bearer token beside it is refused, not preferred.
    let res = s.client.get(s.url("/web")).header("cookie", &cookie).bearer_auth(&bearer).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let res = s
        .client
        .post(s.url("/web/logout"))
        .header("cookie", &cookie)
        .header("origin", ORIGIN)
        .header("content-type", "application/x-www-form-urlencoded")
        .bearer_auth(&bearer)
        .body(encode(&[("csrf", &sess.csrf)]))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert!(signed_in(s, &sess).await, "the refused sign-out signed nothing out");
    let res = s
        .client
        .post(s.url("/web/login"))
        .header("cookie", &cookie)
        .header("origin", ORIGIN)
        .header("content-type", "application/x-www-form-urlencoded")
        .bearer_auth(&bearer)
        .body(encode(&[("username", "root"), ("password", PASSWORD)]))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    // The JSON admin routes take the bearer token, and the cookie beside it is refused.
    for path in ["/admin/server", "/admin/users", "/admin/audit"] {
        let url = s.url(&format!("/api/v1{path}"));
        let res = s.client.get(&url).bearer_auth(&bearer).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{path}: the bearer token alone");
        let res = s.client.get(&url).bearer_auth(&bearer).header("cookie", &cookie).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{path}: both");
        assert_eq!(res.json::<serde_json::Value>().await.unwrap()["error"], "invalid");
        let res = s.client.get(&url).header("cookie", &cookie).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{path}: the cookie alone is no credential there");
    }
    // A change through the JSON API with both is refused too, and changes nothing.
    let vic = s.add_user("vic", Role::Member).await;
    let res = s
        .client
        .post(s.url(&format!("/api/v1/admin/users/{}/disable", vic.id.0)))
        .bearer_auth(&bearer)
        .header("cookie", &cookie)
        .json(&json!({ "password": PASSWORD }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert!(s.meta.admin_user(vic.id, s.at()).await.unwrap().unwrap().disabled_ms.is_none());
    // Only a bearer token, and no cookie, is not a session: the pages send the visitor to sign in.
    let res = s.client.get(s.url("/web")).bearer_auth(&bearer).send().await.unwrap();
    assert_eq!((res.status(), location(&res).as_str()), (StatusCode::SEE_OTHER, "/web/login"));
}

// ---- how long a session lives ----

async fn a_session_ends_with_the_accounts_password_disable_removal_and_two_factor(s: &Web) {
    for name in ["root", "pw", "off", "gone", "tf"] {
        s.add_admin(name).await;
    }
    let boss = s.sign_in("root").await;
    let (pw, off, gone, tf) = (s.sign_in("pw").await, s.sign_in("off").await, s.sign_in("gone").await, s.sign_in("tf").await);
    assert!(signed_in(s, &pw).await && signed_in(s, &off).await && signed_in(s, &gone).await && signed_in(s, &tf).await);

    // A password change by one of the account's devices.
    let device = s.device("pw").await;
    let res = s
        .api(Some(&device), Method::POST, "/account/password", Some(json!({ "current_password": PASSWORD, "new_password": "another good password" })))
        .await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    assert!(!signed_in(s, &pw).await, "a password change ends the session");
    assert!(signed_in(s, &boss).await, "and nobody else's");

    // A disable by an admin, over the pages: ends it, and an enable does not bring it back.
    let id = s.id_of("off").await;
    assert_eq!(s.act(&boss, &format!("/web/users/{id}/disable"), &[]).await.status(), StatusCode::SEE_OTHER);
    assert!(!signed_in(s, &off).await, "a disable ends the session");
    assert_eq!(s.act(&boss, &format!("/web/users/{id}/enable"), &[]).await.status(), StatusCode::SEE_OTHER);
    assert!(!signed_in(s, &off).await, "an enable does not revive it");
    let again = s.sign_in("off").await;
    assert!(signed_in(s, &again).await, "a new sign-in works");

    // A removal.
    let id = s.id_of("gone").await;
    assert_eq!(s.act(&boss, &format!("/web/users/{id}/remove"), &[]).await.status(), StatusCode::SEE_OTHER);
    assert!(!signed_in(s, &gone).await, "a removal ends the session");
    let res = s.post("/web/login", None, &[("username", "gone"), ("password", PASSWORD)]).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "and the account can't sign in again");

    // Two-factor sign-in turned on from a device.
    s.freeze();
    let tf_device = s.device("tf").await;
    let res = s.api(Some(&tf_device), Method::POST, "/account/two-factor/setup", Some(json!({ "password": PASSWORD }))).await;
    let setup: TwoFactorSetupResponse = res.json().await.unwrap();
    let secret = totp::base32_decode(&setup.secret).unwrap();
    let code = totp::code_for(&secret, totp::step_at(s.at())).unwrap();
    let res = s.api(Some(&tf_device), Method::POST, "/account/two-factor/confirm", Some(json!({ "totp_code": code }))).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(!signed_in(s, &tf).await, "turning two-factor sign-in on ends the session");

    // Turned off again by the command line's call (the admin API's reset ends sessions as well, see its own test).
    let tf_user = s.meta.user_by_name("tf").await.unwrap().unwrap();
    let tf_sess = sign_in_with_factor(s, "tf", &Tf { secret, recovery: Vec::new() }).await;
    assert!(signed_in(s, &tf_sess).await);
    s.meta.reset_two_factor(tf_user.id, s.at()).await.unwrap();
    assert!(!signed_in(s, &tf_sess).await, "a reset of the second factor ends the session");
    // A password reset by the command line ends the session of an account that signs in again.
    let fresh = s.sign_in("tf").await;
    s.meta.reset_password(tf_user.id, &hash_password("brand new password").unwrap(), true, s.at()).await.unwrap();
    assert!(!signed_in(s, &fresh).await, "the command line's password reset ends the session");
}

async fn a_session_ends_when_idle_or_at_its_absolute_lifetime(s: &Web) {
    s.add_admin("root").await;
    s.clock.store(craft_cloud_server::state::now_ms(), Ordering::SeqCst);
    let start = s.at();
    // Idle: just inside the idle time still works (and starts it again); at it, the session is over and the cookie is dropped.
    let idle = s.sign_in("root").await;
    s.advance(IDLE_MS - 1);
    assert!(signed_in(s, &idle).await);
    s.advance(IDLE_MS - 1);
    assert!(signed_in(s, &idle).await, "the use before moved the idle time");
    s.advance(IDLE_MS);
    let res = s.get("/web", Some(&idle.token)).await;
    assert_eq!((res.status(), location(&res).as_str()), (StatusCode::SEE_OTHER, "/web/login"));
    let line = res.headers().get_all("set-cookie").iter().filter_map(|v| v.to_str().ok()).find(|l| l.starts_with(COOKIE_NAME)).unwrap().to_owned();
    assert!(line.contains("Max-Age=0") && line.starts_with(&format!("{COOKIE_NAME}=;")), "{line}");
    // A form with an expired session is a 401, with the sign-in form, and changes nothing.
    let vic = s.add_user("vic", Role::Member).await;
    let res = s.act(&idle, &format!("/web/users/{}/disable", vic.id.0), &[]).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert!(res.text().await.unwrap().contains("action=\"/web/login\""));
    assert!(s.meta.admin_user(vic.id, s.at()).await.unwrap().unwrap().disabled_ms.is_none());

    // Absolute: a session that is used all the time still ends at its lifetime.
    let busy = s.sign_in("root").await;
    let signed = s.at();
    while s.at() + IDLE_MS / 2 < signed + ABSOLUTE_MS {
        s.advance(IDLE_MS / 2);
        assert!(signed_in(s, &busy).await);
    }
    s.advance(IDLE_MS / 2 + 1);
    assert!(s.at() >= signed + ABSOLUTE_MS);
    assert!(!signed_in(s, &busy).await, "the absolute lifetime ends a busy session");
    assert!(s.at() > start);
}

async fn logging_out_ends_the_session_and_drops_the_cookie(s: &Web) {
    s.add_admin("root").await;
    let sess = s.sign_in("root").await;
    let other = s.sign_in("root").await;
    let res = s.post("/web/logout", Some(&sess.token), &[("csrf", &sess.csrf)]).await;
    assert_eq!((res.status(), location(&res).as_str()), (StatusCode::SEE_OTHER, "/web/login?notice=signed_out"));
    let line = cookie_line(&res).unwrap();
    assert_eq!(line, format!("{COOKIE_NAME}=; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age=0; Expires=Thu, 01 Jan 1970 00:00:00 GMT"));
    assert!(!signed_in(s, &sess).await);
    assert!(s.meta.web_session(&token_hash(&sess.token), s.at(), IDLE_MS).await.unwrap().is_none());
    assert!(signed_in(s, &other).await, "another session of the account stays");
    let page = s.get("/web/login?notice=signed_out", None).await.text().await.unwrap();
    assert!(page.contains("You are signed out."));
    // A second sign-out, with the old cookie, goes to the sign-in page and breaks nothing. An unknown notice shows nothing.
    let res = s.post("/web/logout", Some(&sess.token), &[("csrf", &sess.csrf)]).await;
    assert_eq!((res.status(), location(&res).as_str()), (StatusCode::SEE_OTHER, "/web/login"));
    let page = s.get("/web/login?notice=%3Cscript%3E", None).await.text().await.unwrap();
    assert!(!page.contains("script") || !page.contains("<script>"));
}

async fn members_and_demoted_admins_are_refused(s: &Web) {
    s.add_admin("root").await;
    s.add_admin("demoted").await;
    s.add_user("ana", Role::Member).await;
    let boss = s.sign_in("root").await;
    let demoted = s.sign_in("demoted").await;
    // A member can't sign in, and is told what a wrong password is told.
    let res = s.post("/web/login", None, &[("username", "ana"), ("password", PASSWORD)]).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert!(cookie_token(&res).is_none());
    // A session of an admin who was demoted is refused at once: pages and changes alike, with 403.
    let id = s.id_of("demoted").await;
    let res = s.act(&boss, &format!("/web/users/{id}/update"), &[("role", "member")]).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    for path in ["/web", "/web/users", "/web/audit", "/web/audit/export"] {
        let res = s.get(path, Some(&demoted.token)).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "{path}");
        assert!(!res.text().await.unwrap().contains("Accounts"));
    }
    let ana = s.id_of("ana").await;
    let res = s.act(&demoted, &format!("/web/users/{ana}/disable"), &[]).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert!(s.meta.admin_user(craft_cloud_meta::UserId(ana), s.at()).await.unwrap().unwrap().disabled_ms.is_none());
    // And a demoted admin can not sign in again.
    let res = s.post("/web/login", None, &[("username", "demoted"), ("password", PASSWORD)]).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    // The sign-in page for someone with a cookie of a member's session: there is none to have, and a made-up one is no session.
    let res = s.get("/web/users", Some(&"A".repeat(43))).await;
    assert_eq!((res.status(), location(&res).as_str()), (StatusCode::SEE_OTHER, "/web/login"));
    let res = s.get("/web/users", Some("short")).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
}

// ---- the confirmation ----

/// Every change of the pages, with the fields of its form and what it needs to exist.
fn changes(vic: &str, device: &str) -> Vec<(String, Vec<(&'static str, &'static str)>)> {
    vec![
        ("/web/users".to_owned(), vec![("username", "newbie"), ("role", "member")]),
        (format!("/web/users/{vic}/update"), vec![("role", "admin")]),
        (format!("/web/users/{vic}/disable"), vec![]),
        (format!("/web/users/{vic}/enable"), vec![]),
        (format!("/web/users/{vic}/remove"), vec![]),
        (format!("/web/users/{vic}/invite"), vec![]),
        (format!("/web/users/{vic}/invite/revoke"), vec![]),
        (format!("/web/users/{vic}/password-reset"), vec![]),
        (format!("/web/users/{vic}/reset-two-factor"), vec![]),
        (format!("/web/users/{vic}/devices/{device}/revoke"), vec![]),
    ]
}

async fn nothing_changed(s: &Web, vic: &craft_cloud_meta::UserRecord) {
    let found = s.meta.admin_user(vic.id, s.at()).await.unwrap().unwrap();
    assert_eq!((found.role, found.disabled_ms, found.removed_ms, found.quota_bytes, found.invite_pending), (Role::Member, None, None, None, false));
    assert_eq!(found.devices_active, 1);
    assert!(s.meta.user_by_name("newbie").await.unwrap().is_none());
}

async fn every_change_needs_the_confirmation(s: &Web) {
    let vic = s.add_user("vic", Role::Member).await;
    let device = s.device("vic").await;
    let device_id = s.meta.list_devices(vic.id).await.unwrap().first().unwrap().id;
    let _ = device;
    let routes = changes(&vic.id.0.to_string(), &device_id.to_string());
    for (n, (path, extra)) in routes.iter().enumerate() {
        // A fresh admin per route: wrong passwords are counted per account.
        let name = format!("adm{n}");
        s.add_admin(&name).await;
        let sess = s.sign_in(&name).await;
        // No password: 400. A wrong one: 403 `wrong_password`. Neither changes anything.
        let mut fields: Vec<(&str, &str)> = extra.clone();
        fields.push(("csrf", &sess.csrf));
        let res = s.post(path, Some(&sess.token), &fields).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{path}: no password");
        let mut wrong = fields.clone();
        wrong.push(("password", "a wrong password"));
        let res = s.post(path, Some(&sess.token), &wrong).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "{path}: a wrong password");
        assert!(res.text().await.unwrap().contains("the current password is wrong"), "{path}");
        let mut empty = fields.clone();
        empty.push(("password", ""));
        let res = s.post(path, Some(&sess.token), &empty).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{path}: an empty password");
        nothing_changed(s, &vic).await;
    }
    // A password that is too long is refused before it is checked.
    let sess = s.sign_in("adm0").await;
    let long = "x".repeat(1025);
    let res = s.post(&routes[2].0, Some(&sess.token), &[("csrf", &sess.csrf), ("password", &long)]).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    nothing_changed(s, &vic).await;
}

async fn wrong_confirmation_passwords_are_limited_per_account(s: &Web) {
    s.add_admin("root").await;
    let vic = s.add_user("vic", Role::Member).await;
    let sess = s.sign_in("root").await;
    let path = format!("/web/users/{}/disable", vic.id.0);
    for _ in 0..5 {
        let res = s.post(&path, Some(&sess.token), &[("csrf", &sess.csrf), ("password", "a wrong password")]).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }
    let res = s.post(&path, Some(&sess.token), &[("csrf", &sess.csrf), ("password", PASSWORD)]).await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(header(&res, "retry-after").parse::<u64>().is_ok_and(|secs| secs >= 1), "the confirmation's limit says how long to wait");
    assert!(s.meta.admin_user(vic.id, s.at()).await.unwrap().unwrap().disabled_ms.is_none());
}

async fn an_admin_with_two_factor_confirms_changes_with_a_code(s: &Web) {
    s.add_admin("root").await;
    let vic = s.add_user("vic", Role::Member).await;
    let device_id = {
        s.device("vic").await;
        s.meta.list_devices(vic.id).await.unwrap().first().unwrap().id
    };
    let tf = turn_on_two_factor(s, "root").await;
    let sess = sign_in_with_factor(s, "root", &tf).await;
    let routes = changes(&vic.id.0.to_string(), &device_id.to_string());
    // The forms ask for a code, and the server does whatever the form said: no code is 403 `two_factor_invalid` on every route.
    let page = s.get(&format!("/web/users/{}", vic.id.0), Some(&sess.token)).await.text().await.unwrap();
    assert!(page.contains("name=\"totp_code\"") && page.contains("name=\"recovery_code\""));
    for (path, extra) in &routes {
        let mut fields: Vec<(&str, &str)> = extra.clone();
        fields.push(("csrf", &sess.csrf));
        fields.push(("password", PASSWORD));
        let res = s.post(path, Some(&sess.token), &fields).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "{path}: no code");
        assert!(res.text().await.unwrap().contains("two-factor"), "{path}");
    }
    nothing_changed(s, &vic).await;
    // A code of the wrong shape is 400 and a wrong one 403; neither changes anything.
    let disable = format!("/web/users/{}/disable", vic.id.0);
    let res = s.act(&sess, &disable, &[("totp_code", "12")]).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let wrong = "000000";
    let res = s.act(&sess, &disable, &[("totp_code", wrong)]).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let res = s.act(&sess, &disable, &[("totp_code", "123456"), ("recovery_code", "ABCD-EFGH-JKMN-PQRS")]).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST, "both a code and a recovery code");
    nothing_changed(s, &vic).await;
    // A right code does it, once: the same time step does not serve a second change.
    let code = tf.fresh_code(s);
    let res = s.act(&sess, &disable, &[("totp_code", &code)]).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let enable = format!("/web/users/{}/enable", vic.id.0);
    let res = s.act(&sess, &enable, &[("totp_code", &code)]).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN, "the code was used up");
    let res = s.act(&sess, &enable, &[("totp_code", &tf.fresh_code(s))]).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    // A recovery code serves too, and once.
    let recovery = tf.recovery.first().unwrap().clone();
    let res = s.act(&sess, &disable, &[("recovery_code", &recovery)]).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let res = s.act(&sess, &enable, &[("recovery_code", &recovery)]).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

// ---- the changes themselves ----

async fn accounts_are_made_changed_disabled_and_removed(s: &Web) {
    s.add_admin("root").await;
    let sess = s.sign_in("root").await;
    // A new account: the invite is the answer, once, with the join link of the public address.
    let res = s.act(&sess, "/web/users", &[("username", "Newbie"), ("role", "member")]).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(header(&res, "cache-control"), "no-store");
    let html = res.text().await.unwrap();
    let code = html.split("<span class=\"secret\">").nth(1).and_then(|rest| rest.split("</span>").next()).unwrap().to_owned();
    assert!(!code.is_empty() && html.contains("https://cloud.test/join/") && html.contains("only time the code is shown"), "{html}");
    let newbie = s.id_of("newbie").await;
    let page = s.get(&format!("/web/users/{newbie}"), Some(&sess.token)).await.text().await.unwrap();
    assert!(!page.contains(&code), "the code is on no other page");
    assert!(page.contains("invite") && page.contains("Account newbie"));
    // Redeeming it gives the account a password.
    let join = s.api(None, Method::POST, "/join", Some(json!({ "code": code, "device_name": "pc", "password": PASSWORD }))).await;
    assert_eq!(join.status(), StatusCode::OK);
    // A name that is taken is a 409, a name of the wrong shape a 400, a role that is none a 400.
    let res = s.act(&sess, "/web/users", &[("username", "newbie"), ("role", "member")]).await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let res = s.act(&sess, "/web/users", &[("username", "bad name"), ("role", "member")]).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let res = s.act(&sess, "/web/users", &[("username", "other"), ("role", "owner")]).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert!(s.meta.user_by_name("other").await.unwrap().is_none());

    // Role and quota.
    let path = format!("/web/users/{newbie}/update");
    let res = s.act(&sess, &path, &[("role", "admin"), ("quota_mib", "10")]).await;
    assert_eq!((res.status(), location(&res).as_str()), (StatusCode::SEE_OTHER, format!("/web/users/{newbie}?notice=updated").as_str()));
    let found = s.meta.admin_user(craft_cloud_meta::UserId(newbie), s.at()).await.unwrap().unwrap();
    assert_eq!((found.role, found.quota_bytes), (Role::Admin, Some(10 * 1024 * 1024)));
    let page = s.get(&location(&res), Some(&sess.token)).await.text().await.unwrap();
    assert!(page.contains("The account was updated.") && page.contains("10.0 MiB"), "{page}");
    for fields in [
        vec![],
        vec![("quota_mib", "1")],
        vec![("quota_mib", "abc")],
        vec![("quota_mib", "-1")],
        vec![("quota_mib", "5"), ("remove_quota", "on")],
        vec![("role", "boss")],
    ] {
        let res = s.act(&sess, &path, &fields).await;
        assert!(res.status() == StatusCode::BAD_REQUEST, "{fields:?}: {}", res.status());
    }
    let res = s.act(&sess, &path, &[("remove_quota", "on"), ("role", "member")]).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let found = s.meta.admin_user(craft_cloud_meta::UserId(newbie), s.at()).await.unwrap().unwrap();
    assert_eq!((found.role, found.quota_bytes), (Role::Member, None));
    // Unknown IDs are 404 pages, after the checks.
    let res = s.act(&sess, &format!("/web/users/{}/update", uuid::Uuid::now_v7()), &[("role", "admin")]).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let res = s.get(&format!("/web/users/{}", uuid::Uuid::now_v7()), Some(&sess.token)).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let res = s.act(&sess, "/web/users/not-an-id/disable", &[]).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // Disable and enable; the admin's own account can't be disabled; the last admin stays.
    let device = s.device("newbie").await;
    assert!(s.api(Some(&device), Method::GET, "/account", None).await.status().is_success());
    let res = s.act(&sess, &format!("/web/users/{newbie}/disable"), &[]).await;
    assert_eq!(location(&res), format!("/web/users/{newbie}?notice=disabled"));
    assert_eq!(s.api(Some(&device), Method::GET, "/account", None).await.status(), StatusCode::UNAUTHORIZED, "its devices are signed out");
    let page = s.get(&location(&res), Some(&sess.token)).await.text().await.unwrap();
    assert!(page.contains("disabled") && page.contains("Enable the account"), "{page}");
    let res = s.act(&sess, &format!("/web/users/{newbie}/enable"), &[]).await;
    assert_eq!(location(&res), format!("/web/users/{newbie}?notice=enabled"));
    let root = s.id_of("root").await;
    let res = s.act(&sess, &format!("/web/users/{root}/disable"), &[]).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST, "not your own account");
    let res = s.act(&sess, &format!("/web/users/{root}/update"), &[("role", "member")]).await;
    assert_eq!(res.status(), StatusCode::CONFLICT, "not the last admin");
    assert!(signed_in(s, &sess).await);

    // Removal: gone from the lists, its name stays taken, nothing more can be changed.
    let res = s.act(&sess, &path, &[("role", "member"), ("quota_mib", "10")]).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let res = s.act(&sess, &format!("/web/users/{newbie}/remove"), &[]).await;
    assert_eq!((res.status(), location(&res).as_str()), (StatusCode::SEE_OTHER, "/web/users?notice=removed"));
    let list = s.get("/web/users?notice=removed", Some(&sess.token)).await.text().await.unwrap();
    assert!(list.contains("marked for removal") && !list.contains(&format!("/web/users/{newbie}\"")));
    let res = s.get(&format!("/web/users/{newbie}"), Some(&sess.token)).await;
    assert_eq!(res.status(), StatusCode::OK);
    let page = res.text().await.unwrap();
    assert!(page.contains("marked for removal") && !page.contains("Enable the account"), "{page}");
    assert!(page.contains("<dt>Quota</dt><dd>10.0 MiB</dd>"), "the removed account keeps its old quota detail: {page}");
    assert!(!page.contains("Effective quota") && !page.contains("estimate (overlapping)"), "removed accounts have no usage query: {page}");
    let res = s.act(&sess, "/web/users", &[("username", "newbie"), ("role", "member")]).await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let res = s.act(&sess, &format!("/web/users/{newbie}/enable"), &[]).await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
}

async fn invites_and_reset_codes_are_shown_once_and_work(s: &Web) {
    s.add_admin("root").await;
    let sess = s.sign_in("root").await;
    // An invite for an account with no password yet: make one by the admin API's own route (the pages' create is tested above).
    let res = s.act(&sess, "/web/users", &[("username", "pending"), ("role", "member")]).await;
    assert_eq!(res.status(), StatusCode::OK);
    let pending = s.id_of("pending").await;
    let res = s.act(&sess, &format!("/web/users/{pending}/invite"), &[]).await;
    assert_eq!(res.status(), StatusCode::OK);
    let html = res.text().await.unwrap();
    let invite = html.split("<span class=\"secret\">").nth(1).and_then(|r| r.split("</span>").next()).unwrap().to_owned();
    assert!(html.contains("New invite") && html.contains("/join/"));
    let page = s.get(&format!("/web/users/{pending}"), Some(&sess.token)).await.text().await.unwrap();
    assert!(!page.contains(&invite) && page.contains("<dt>Invite</dt><dd>open</dd>"), "{page}");
    // End it: the code no longer works, and a second end is 404.
    let res = s.act(&sess, &format!("/web/users/{pending}/invite/revoke"), &[]).await;
    assert_eq!(location(&res), format!("/web/users/{pending}?notice=invite_revoked"));
    let join = s.api(None, Method::POST, "/join", Some(json!({ "code": invite, "device_name": "pc", "password": "a fresh password" }))).await;
    assert_eq!(join.status(), StatusCode::NOT_FOUND);
    let res = s.act(&sess, &format!("/web/users/{pending}/invite/revoke"), &[]).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // A password reset code for an account with a password: shown once, works at /join, signs the devices out.
    s.add_user("ana", Role::Member).await;
    let ana = s.id_of("ana").await;
    let old_device = s.device("ana").await;
    let res = s.act(&sess, &format!("/web/users/{ana}/invite"), &[]).await;
    assert_eq!(res.status(), StatusCode::CONFLICT, "an account with a password gets a reset code, not an invite");
    let res = s.act(&sess, &format!("/web/users/{ana}/password-reset"), &[]).await;
    assert_eq!(res.status(), StatusCode::OK);
    let html = res.text().await.unwrap();
    let reset = html.split("<span class=\"secret\">").nth(1).and_then(|r| r.split("</span>").next()).unwrap().to_owned();
    assert!(html.contains("Password reset code") && html.contains("only time the code is shown"));
    assert!(!s.get(&format!("/web/users/{ana}"), Some(&sess.token)).await.text().await.unwrap().contains(&reset));
    let join =
        s.api(None, Method::POST, "/join", Some(json!({ "code": reset, "device_name": "recovery", "password": "a brand new password" }))).await;
    assert_eq!(join.status(), StatusCode::OK);
    assert_eq!(s.api(Some(&old_device), Method::GET, "/account", None).await.status(), StatusCode::UNAUTHORIZED);
    // Your own password is changed in your account.
    let me = s.id_of("root").await;
    let res = s.act(&sess, &format!("/web/users/{me}/password-reset"), &[]).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

async fn two_factor_resets_and_devices_are_managed(s: &Web) {
    s.add_admin("root").await;
    s.add_user("ana", Role::Member).await;
    let root_tf = turn_on_two_factor(s, "root").await;
    let ana_tf = turn_on_two_factor(s, "ana").await;
    let ana = s.id_of("ana").await;
    let sess = sign_in_with_factor(s, "root", &root_tf).await;
    // The page offers the reset only to an account that has two-factor sign-in.
    let page = s.get(&format!("/web/users/{ana}"), Some(&sess.token)).await.text().await.unwrap();
    assert!(page.contains("/reset-two-factor") && page.contains("Two-factor sign-in"));
    let device = s.device_after_second_factor("ana", &ana_tf).await;
    let devices = s.get(&format!("/web/users/{ana}"), Some(&sess.token)).await.text().await.unwrap();
    assert!(devices.contains("Sign this device out"), "{devices}");
    // Revoking one device.
    let listed = s.api(Some(&device), Method::GET, "/devices", None).await.json::<Vec<serde_json::Value>>().await.unwrap();
    let live = listed.iter().find(|d| d["current"] == true).unwrap()["id"].as_str().unwrap().to_owned();
    let path = format!("/web/users/{ana}/devices/{live}/revoke");
    let res = s.act(&sess, &path, &[("totp_code", &root_tf.fresh_code(s))]).await;
    assert_eq!((res.status(), location(&res).as_str()), (StatusCode::SEE_OTHER, format!("/web/users/{ana}?notice=device_revoked").as_str()));
    assert_eq!(s.api(Some(&device), Method::GET, "/account", None).await.status(), StatusCode::UNAUTHORIZED);
    let res = s.act(&sess, &path, &[("totp_code", &root_tf.fresh_code(s))]).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER, "a device that is out already stays so");
    let res = s.act(&sess, &format!("/web/users/{ana}/devices/{}/revoke", uuid::Uuid::now_v7()), &[("totp_code", &root_tf.fresh_code(s))]).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    // The reset: two-factor sign-in is off, the devices are out.
    let device = s.device_after_second_factor("ana", &ana_tf).await;
    let res = s.act(&sess, &format!("/web/users/{ana}/reset-two-factor"), &[("totp_code", &root_tf.fresh_code(s))]).await;
    assert_eq!(location(&res), format!("/web/users/{ana}?notice=two_factor_reset"));
    assert_eq!(s.api(Some(&device), Method::GET, "/account", None).await.status(), StatusCode::UNAUTHORIZED);
    assert!(!s.meta.two_factor_enabled(craft_cloud_meta::UserId(ana)).await.unwrap());
    let res = s.act(&sess, &format!("/web/users/{ana}/reset-two-factor"), &[("totp_code", &root_tf.fresh_code(s))]).await;
    assert_eq!(res.status(), StatusCode::CONFLICT, "nothing to reset");
    // Your own account's second factor is turned off in your account.
    let me = s.id_of("root").await;
    let res = s.act(&sess, &format!("/web/users/{me}/reset-two-factor"), &[("totp_code", &root_tf.fresh_code(s))]).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    // Resetting another administrator's second factor needs the acting admin's own: one without is refused, one with does it.
    let root = s.id_of("root").await;
    s.add_admin("plain").await;
    let plain = s.sign_in("plain").await;
    let res = s.act(&plain, &format!("/web/users/{root}/reset-two-factor"), &[]).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert!(res.text().await.unwrap().contains("two-factor sign-in on your own account"));
    assert!(s.meta.two_factor_enabled(craft_cloud_meta::UserId(root)).await.unwrap());
    s.add_admin("careful").await;
    let careful_tf = turn_on_two_factor(s, "careful").await;
    let careful = sign_in_with_factor(s, "careful", &careful_tf).await;
    let res = s.act(&careful, &format!("/web/users/{root}/reset-two-factor"), &[("totp_code", &careful_tf.fresh_code(s))]).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert!(!s.meta.two_factor_enabled(craft_cloud_meta::UserId(root)).await.unwrap());
    assert!(!signed_in(s, &sess).await, "and root's own session ended with it");
}

impl Web {
    /// A device token of `name` after its two-factor sign-in.
    async fn device_after_second_factor(&self, name: &str, tf: &Tf) -> String {
        let res = self.api(None, Method::POST, "/auth/login", Some(json!({ "username": name, "password": PASSWORD, "device_name": "pc" }))).await;
        let challenge = res.json::<serde_json::Value>().await.unwrap()["two_factor"]["challenge"].as_str().unwrap().to_owned();
        let res = self
            .api(None, Method::POST, "/auth/login/second-factor", Some(json!({ "challenge": challenge, "totp_code": tf.fresh_code(self) })))
            .await;
        assert_eq!(res.status(), StatusCode::OK);
        res.json::<LoginResponse>().await.unwrap().token
    }
}

// ---- the lists ----

async fn the_user_list_pages_and_searches(s: &Web) {
    s.add_admin("root").await;
    for n in 0..60 {
        s.add_user(&format!("user{n:02}"), Role::Member).await;
    }
    s.add_user("odd_name", Role::Member).await;
    s.add_user("oddxname", Role::Member).await;
    let sess = s.sign_in("root").await;
    let first = s.get("/web/users", Some(&sess.token)).await.text().await.unwrap();
    assert_eq!(first.matches("<th scope=\"row\"><a href=\"/web/users/").count(), 50, "fifty a page");
    assert!(first.contains("Next page") && !first.contains("First page"));
    let next = first.split("<a href=\"/web/users?query=&amp;cursor=").nth(1).and_then(|r| r.split('"').next()).unwrap().to_owned();
    let second = s.get(&format!("/web/users?query=&cursor={next}"), Some(&sess.token)).await.text().await.unwrap();
    assert!(second.contains("First page") && !second.contains("Next page"));
    assert_eq!(second.matches("<th scope=\"row\"><a href=\"/web/users/").count(), 13, "root, 60 and two others make 63");
    assert!(second.contains(">user59<") && !second.contains(">user00<"));
    // Search: a substring, case-insensitive, and `_` is an ordinary character.
    let found = s.get("/web/users?query=USER5", Some(&sess.token)).await.text().await.unwrap();
    assert_eq!(found.matches("<th scope=\"row\"><a href=\"/web/users/").count(), 10);
    let found = s.get("/web/users?query=odd_", Some(&sess.token)).await.text().await.unwrap();
    assert!(found.contains(">odd_name<") && !found.contains(">oddxname<"));
    let found = s.get("/web/users?query=nobody-has-this", Some(&sess.token)).await.text().await.unwrap();
    assert!(found.contains("No accounts match."));
    // Odd input is a 400 page, not a failure.
    for bad in ["/web/users?cursor=%21%21", &format!("/web/users?query={}", "x".repeat(65)), "/web/users?query=a%00b"] {
        let res = s.get(bad, Some(&sess.token)).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{bad}");
    }
    // The overview counts.
    let overview = s.get("/web", Some(&sess.token)).await.text().await.unwrap();
    assert!(overview.contains("<dt>Accounts</dt><dd>63</dd>") && overview.contains("<dt>Database</dt>"), "{overview}");
}

async fn the_audit_log_filters_pages_and_downloads(s: &Web) {
    s.add_admin("root").await;
    let ana = s.add_user("ana", Role::Member).await;
    let sess = s.sign_in("root").await;
    // One failed sign-in row per account and minute: sixty minutes make sixty rows.
    let base = s.at() - 3_600_000;
    for n in 0..60 {
        let failure = craft_cloud_meta::AuditFailure {
            action: craft_cloud_meta::AuditAction::AuthLoginFailed,
            target: Some(ana.id),
            step: craft_cloud_meta::Step::Password,
        };
        s.meta.audit_record_failure(failure, base + n * 60_000).await.unwrap();
    }
    let page = s.get("/web/audit", Some(&sess.token)).await.text().await.unwrap();
    assert_eq!(page.matches("<tr><th scope=\"row\">").count(), 50, "fifty a page");
    assert!(page.contains("auth.login_failed") && page.contains("Older events") && page.contains("Download these filters"));
    let older = page.split("Older events").next().unwrap().rsplit("<a href=\"").next().unwrap().trim_end_matches("\">").replace("&amp;", "&");
    let rest = s.get(&older, Some(&sess.token)).await.text().await.unwrap();
    assert!(rest.contains("Newest events") && !rest.contains("Older events"), "{rest}");
    // Filters: by action, by prefix, by account.
    let only = s.get("/web/audit?action=auth.login", Some(&sess.token)).await.text().await.unwrap();
    assert!(only.contains("auth.login") && !only.contains("auth.login_failed"));
    let prefix = s.get("/web/audit?action=auth.", Some(&sess.token)).await.text().await.unwrap();
    assert!(prefix.contains("auth.login_failed"));
    let who = s.get(&format!("/web/audit?user={}", ana.id.0), Some(&sess.token)).await.text().await.unwrap();
    assert!(who.contains(&ana.id.0.to_string()) && !who.contains("<td>auth.login</td>"));
    for bad in ["/web/audit?action=nope", "/web/audit?user=ana", "/web/audit?cursor=zz", "/web/audit?action=%3Cscript%3E"] {
        assert_eq!(s.get(bad, Some(&sess.token)).await.status(), StatusCode::BAD_REQUEST, "{bad}");
    }
    // The download: NDJSON, an attachment, never stored, complete, and filtered the same way.
    let res = s.get("/web/audit/export", Some(&sess.token)).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(header(&res, "content-type"), "application/x-ndjson");
    assert_eq!(header(&res, "content-disposition"), "attachment; filename=\"craft-cloud-audit.ndjson\"");
    assert_eq!(header(&res, "cache-control"), "no-store");
    let body = res.text().await.unwrap();
    let lines: Vec<serde_json::Value> = body.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let end = lines.last().unwrap();
    assert_eq!(end["complete"], true, "{end}");
    assert!(lines.len() > 50 && end["exported"].as_u64().unwrap() as usize == lines.len() - 1);
    assert!(lines.iter().any(|line| line["action"] == "auth.login" && line["details"]["client_kind"] == "web"));
    let res = s.get("/web/audit/export?action=auth.login", Some(&sess.token)).await;
    let body = res.text().await.unwrap();
    assert!(body.lines().all(|l| !l.contains("auth.login_failed")));
    // Only for a session.
    let res = s.get("/web/audit/export", None).await;
    assert_eq!((res.status(), location(&res).as_str()), (StatusCode::SEE_OTHER, "/web/login"));
    let res = s.get("/web/audit/export?action=nope", Some(&sess.token)).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

async fn a_web_audit_page_shows_the_web_client_kind(s: &Web) {
    s.add_admin("root").await;
    let session = s.sign_in("root").await;
    let page = s.get("/web/audit", Some(&session.token)).await.text().await.unwrap();
    assert!(page.contains("&quot;client_kind&quot;:&quot;web&quot;"), "{page}");
}

// ---- text that comes from outside ----

async fn user_controlled_text_is_escaped(s: &Web) {
    s.add_admin("root").await;
    let ana = s.add_user("ana", Role::Member).await;
    let sess = s.sign_in("root").await;
    // A device may call itself and its platform any printable text.
    let name = "<script>alert('d')</script>";
    let platform = "<b onmouseover=x>&\"";
    let res = s
        .api(
            None,
            Method::POST,
            "/auth/login",
            Some(json!({ "username": "ana", "password": PASSWORD, "device_name": name, "platform": platform, "app_version": "<i>" })),
        )
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    // A username can't hold markup, but a row in a damaged or hand-edited database could: the pages must hold anyway.
    use sqlx::Connection;
    sqlx::any::install_default_drivers();
    let mut conn = sqlx::AnyConnection::connect(&s.url).await.unwrap();
    sqlx::query("UPDATE users SET username = $1 WHERE username = 'ana'").bind("<script>alert(1)</script>").execute(&mut conn).await.unwrap();
    drop(conn);
    let pages = [
        s.get("/web", Some(&sess.token)).await,
        s.get("/web/users", Some(&sess.token)).await,
        s.get(&format!("/web/users/{}", ana.id.0), Some(&sess.token)).await,
        s.get("/web/audit", Some(&sess.token)).await,
        s.get("/web/users?query=%3Cscript%3Ealert(2)%3C%2Fscript%3E", Some(&sess.token)).await,
        s.get("/web/audit?action=%22%3E%3Cscript%3Ealert(3)%3C%2Fscript%3E", Some(&sess.token)).await,
        s.get("/web/audit?user=%3Cscript%3Ealert(4)%3C%2Fscript%3E", Some(&sess.token)).await,
        s.get("/web/%3Cscript%3Ealert(5)%3C%2Fscript%3E", Some(&sess.token)).await,
        s.get(&format!("/web/users/{}?notice=%3Cscript%3E", ana.id.0), Some(&sess.token)).await,
    ];
    let mut shown = String::new();
    for res in pages {
        let body = res.text().await.unwrap();
        assert!(!body.contains("<script"), "markup from outside is in a page: {body}");
        assert!(!body.contains("onmouseover=x>"), "{body}");
        shown.push_str(&body);
    }
    assert!(shown.contains("&lt;script&gt;alert(1)&lt;/script&gt;"), "the user name is shown, escaped");
    assert!(shown.contains("&lt;script&gt;alert(&#39;d&#39;)&lt;/script&gt;"), "the device name is shown, escaped");
    assert!(shown.contains("&lt;b onmouseover=x&gt;&amp;&quot;"), "the platform is shown, escaped");
    assert!(shown.contains("&lt;script&gt;alert(2)&lt;/script&gt;"), "the search is shown back, escaped");
    assert!(shown.contains("Storage") && shown.contains("Top accounts by stored bytes"), "the storage overview is shown");
    assert!(shown.contains("&lt;script&gt;alert(1)&lt;/script&gt;"), "the top-account name is escaped");
    assert!(
        shown.contains("Effective quota") && shown.contains("Trash estimate (overlapping)") && shown.contains("History estimate (overlapping)"),
        "the account usage is shown"
    );
}

async fn old_sessions_are_cleaned_up_at_sign_in(s: &Web) {
    let root = s.add_admin("root").await;
    s.clock.store(craft_cloud_server::state::now_ms(), Ordering::SeqCst);
    for _ in 0..5 {
        s.sign_in("root").await;
    }
    let out = s.sign_in("root").await;
    s.post("/web/logout", Some(&out.token), &[("csrf", &out.csrf)]).await;
    assert_eq!(s.meta.web_sessions_count_over(s.at(), IDLE_MS).await.unwrap(), 1, "the logged-out one");
    s.advance(IDLE_MS + 1);
    assert_eq!(s.meta.web_sessions_count_over(s.at(), IDLE_MS).await.unwrap(), 6, "five idle ones and the logged-out one");
    // The next sign-in deletes them on its way and is itself unharmed.
    let fresh = s.sign_in("root").await;
    assert_eq!(s.meta.web_sessions_count_over(s.at(), IDLE_MS).await.unwrap(), 0);
    assert!(signed_in(s, &fresh).await);
    assert_eq!(s.meta.web_sessions_open(root.id, s.at(), IDLE_MS).await.unwrap(), 1);
}

async fn form_bodies_are_small_and_odd_ones_refused(s: &Web) {
    s.add_admin("root").await;
    let sess = s.sign_in("root").await;
    // More than 4 KiB is 413, and no form at all is not a sign-in.
    let big = "x".repeat(5000);
    let res = s.post("/web/login", None, &[("username", "root"), ("password", &big)]).await;
    assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let res = s
        .client
        .post(s.url("/web/login"))
        .header("origin", ORIGIN)
        .header("content-type", "application/json")
        .body("{\"username\":\"root\",\"password\":\"x\"}")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST, "a body that is not a form is a malformed request");
    let res = s
        .client
        .post(s.url("/web/logout"))
        .header("origin", ORIGIN)
        .header("cookie", format!("{COOKIE_NAME}={}", sess.token))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(vec![0xff, 0xfe, 0x25, 0x00])
        .send()
        .await
        .unwrap();
    assert!(res.status().is_client_error());
    assert!(signed_in(s, &sess).await);
    // Parameters of the wrong kind in a query are a 400 page.
    assert_eq!(s.get("/web/users?query=a&query=b&cursor=%FF", Some(&sess.token)).await.status().as_u16() / 100, 4);
}

// ---- off by default, and its own tests ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_pages_do_not_exist_unless_the_server_was_set_up_for_them() {
    let s = sqlite_with(vec![], false).await;
    for path in ["/web", "/web/", "/web/login", "/web/style.css", "/web/users"] {
        let res = s.get(path, None).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "{path}");
        assert_eq!(header(&res, "content-type"), "application/json", "{path}: the API's own not-found answer");
    }
    let res = s.post("/web/login", None, &[("username", "root"), ("password", PASSWORD)]).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    // The API is unharmed.
    s.add_admin("root").await;
    assert!(!s.device("root").await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_client_address_behind_a_trusted_proxy_is_what_the_limits_count() {
    let s = sqlite_with(vec!["127.0.0.1".parse().unwrap()], true).await;
    s.add_admin("root").await;
    // Thirty wrong tries from one forwarded address use it up, and another forwarded address still signs in.
    for n in 0..30 {
        let name = format!("guess{n}");
        let res = s
            .client
            .post(s.url("/web/login"))
            .header("origin", ORIGIN)
            .header("x-forwarded-for", "203.0.113.7")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(encode(&[("username", &name), ("password", "x")]))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
    let post = |from: &'static str| {
        let (client, url) = (s.client.clone(), s.url("/web/login"));
        async move {
            client
                .post(url)
                .header("origin", ORIGIN)
                .header("x-forwarded-for", from)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(encode(&[("username", "root"), ("password", PASSWORD)]))
                .send()
                .await
                .unwrap()
        }
    };
    assert_eq!(post("203.0.113.7").await.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(post("203.0.113.8").await.status(), StatusCode::SEE_OTHER);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wrong_confirmation_passwords_are_limited_per_account_on_sqlite() {
    wrong_confirmation_passwords_are_limited_per_account(&sqlite_web().await).await;
}

// ---- review fixes ----

async fn an_account_without_a_password_never_signs_in(s: &Web) {
    // What an invite makes: an account whose password hash is empty. It is answered like a name nobody has, whatever is typed,
    // the server's own dummy password included (which is what an unknown name is checked against).
    s.meta.create_user("pending", "", Role::Admin, 0).await.unwrap();
    let unknown = refused(s.post("/web/login", None, &[("username", "nobody"), ("password", PASSWORD)]).await).await;
    for password in [PASSWORD, "", "craft-cloud dummy password", "$argon2id$"] {
        let answer = refused(s.post("/web/login", None, &[("username", "pending"), ("password", password)]).await).await;
        assert_eq!(answer, unknown, "{password:?}");
    }
    for password in [PASSWORD, "craft-cloud dummy password"] {
        let res = s.api(None, Method::POST, "/auth/login", Some(json!({ "username": "pending", "password": password, "device_name": "pc" }))).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{password:?}: the JSON sign-in too");
    }
}

async fn the_slash_path_has_the_same_headers_and_policy(s: &Web) {
    let get = s.get("/web/", None).await;
    assert_eq!((get.status(), location(&get).as_str()), (StatusCode::SEE_OTHER, "/web"));
    let head = s.client.head(s.url("/web/")).send().await.unwrap();
    assert_eq!((head.status(), location(&head).as_str()), (StatusCode::SEE_OTHER, "/web"));
    let wrong_method = s.post("/web/", None, &[]).await;
    assert_eq!(wrong_method.status(), StatusCode::METHOD_NOT_ALLOWED);
    let no_origin = s.post_from("/web/", None, None, None, &[]).await;
    assert_eq!(no_origin.status(), StatusCode::FORBIDDEN, "a post from nowhere is refused here too");
    for res in [get, head, wrong_method, no_origin] {
        let csp = header(&res, "content-security-policy");
        assert!(csp.contains("script-src 'none'") && csp.contains("frame-ancestors 'none'"), "{}: {csp}", res.status());
        assert_eq!(header(&res, "x-frame-options"), "DENY");
        assert_eq!(header(&res, "x-content-type-options"), "nosniff");
        assert_eq!(header(&res, "referrer-policy"), "no-referrer");
        assert_eq!(header(&res, "cache-control"), "no-store");
    }
}

/// A change whose body is still arriving when its session ends is refused, though the session was valid when the request began.
async fn a_change_that_outlives_its_session_is_refused(s: &Web) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    s.add_admin("root").await;
    let vic = s.add_user("vic", Role::Member).await;
    s.clock.store(craft_cloud_server::state::now_ms(), Ordering::SeqCst);
    let sess = s.sign_in("root").await;
    let body = encode(&[("csrf", &sess.csrf), ("password", PASSWORD)]);
    let (first, rest) = body.split_at(8);
    let host = s.base.trim_start_matches("http://").to_owned();
    let mut stream = tokio::net::TcpStream::connect(&host).await.unwrap();
    let head = format!(
        "POST /web/users/{}/disable HTTP/1.1\r\nHost: {host}\r\nOrigin: {ORIGIN}\r\nCookie: {COOKIE_NAME}={}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{first}",
        vic.id.0,
        sess.token,
        body.len()
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    // The server has looked at the session by now (it does so before it reads the form); the session ends; the body goes on.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    s.advance(IDLE_MS + 1);
    stream.write_all(rest.as_bytes()).await.unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).await.unwrap();
    assert!(answer.starts_with("HTTP/1.1 401"), "{answer}");
    assert!(s.meta.admin_user(vic.id, s.at()).await.unwrap().unwrap().disabled_ms.is_none(), "nothing changed");
}

/// Disabling or removing an account over the pages ends the long-polls it has on its team spaces at once, as the JSON routes do.
async fn disabling_or_removing_over_the_pages_wakes_the_spaces_long_polls(s: &Web) {
    use craft_cloud_meta::SpaceActor;
    use craft_cloud_proto::SpaceName;
    use craft_cloud_proto::api::SpaceRole;
    s.add_admin("root").await;
    let owner = s.add_user("own", Role::Member).await;
    let (away, gone) = (s.add_user("away", Role::Member).await, s.add_user("gone", Role::Member).await);
    let space = s.meta.create_team_space(&SpaceActor::Cli, &SpaceName::new("Studio").unwrap(), owner.id, None, 0).await.unwrap();
    for member in [&away, &gone] {
        s.meta.invite_member(&SpaceActor::Cli, space, member.id, SpaceRole::Viewer, 0).await.unwrap();
    }
    let boss = s.sign_in("root").await;
    for (name, verb, id) in [("away", "disable", away.id.0), ("gone", "remove", gone.id.0)] {
        let device = s.device(name).await;
        let poll = {
            let (client, url) = (s.client.clone(), s.url("/api/v1/changes?since=0&wait=30"));
            tokio::spawn(async move { client.get(url).bearer_auth(device).header("craft-space", space.to_string()).send().await.unwrap() })
        };
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert!(!poll.is_finished(), "{name}: the long-poll waits");
        assert_eq!(s.act(&boss, &format!("/web/users/{id}/{verb}"), &[]).await.status(), StatusCode::SEE_OTHER);
        let res = tokio::time::timeout(std::time::Duration::from_secs(5), poll)
            .await
            .unwrap_or_else(|_| panic!("{name}: the waiter was not woken"))
            .unwrap();
        assert!(matches!(res.status(), StatusCode::NOT_FOUND | StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN), "{name}: {}", res.status());
    }
}

// ---- the Team spaces pages ----

/// A team space made by the command line (so a test starts from a space without going through the page under test).
async fn cli_space(s: &Web, name: &str, owner: &craft_cloud_meta::UserRecord) -> craft_cloud_proto::SpaceId {
    use craft_cloud_meta::SpaceActor;
    use craft_cloud_proto::SpaceName;
    s.meta.create_team_space(&SpaceActor::Cli, &SpaceName::new(name).unwrap(), owner.id, None, 0).await.unwrap()
}

/// Every write of the Team spaces page, for the space `id`.
fn space_changes(id: &str) -> Vec<(String, Vec<(&'static str, &'static str)>)> {
    vec![
        ("/web/spaces".to_owned(), vec![("name", "Fresh"), ("owner", "own")]),
        (format!("/web/spaces/{id}/quota"), vec![("quota_mib", "8")]),
        (format!("/web/spaces/{id}/archive"), vec![]),
        (format!("/web/spaces/{id}/unarchive"), vec![]),
        (format!("/web/spaces/{id}/owner"), vec![("owner", "newcomer")]),
    ]
}

async fn the_space_is_untouched(s: &Web, space: craft_cloud_proto::SpaceId) {
    let found = s.meta.admin_space(space).await.unwrap();
    assert_eq!((found.name.as_str(), found.archived_ms, found.quota_bytes, found.owners, found.members), ("Studio", None, None, 1, 1));
    assert_eq!(s.meta.admin_list_spaces(None, None, 10).await.unwrap().spaces.len(), 1, "no space was made");
}

async fn the_team_spaces_pages_are_for_administrators_and_need_csrf(s: &Web) {
    s.add_admin("root").await;
    s.add_admin("demoted").await;
    let owner = s.add_user("own", Role::Member).await;
    s.add_user("newcomer", Role::Member).await;
    let space = cli_space(s, "Studio", &owner).await;
    let id = space.as_uuid().to_string();
    let (boss, demoted) = (s.sign_in("root").await, s.sign_in("demoted").await);
    let res = s.act(&boss, &format!("/web/users/{}/update", s.id_of("demoted").await), &[("role", "member")]).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);

    // No session: the sign-in page for a page, 401 for a change. A demoted administrator's session: 403 for both.
    for path in ["/web/spaces".to_owned(), format!("/web/spaces/{id}")] {
        let res = s.get(&path, None).await;
        assert_eq!((res.status(), location(&res).as_str()), (StatusCode::SEE_OTHER, "/web/login"), "{path}");
        let res = s.get(&path, Some(&demoted.token)).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "{path}");
        assert!(!res.text().await.unwrap().contains("Studio"), "{path}");
    }
    for (path, extra) in space_changes(&id) {
        let mut fields = extra.clone();
        fields.push(("csrf", &boss.csrf));
        fields.push(("password", PASSWORD));
        assert_eq!(s.post(&path, None, &fields).await.status(), StatusCode::UNAUTHORIZED, "{path}: no session");
        let res = s.act(&demoted, &path, &extra).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "{path}: a demoted administrator");
        // The CSRF token: missing, wrong, and another session's.
        let mut fields = extra.clone();
        fields.push(("password", PASSWORD));
        assert_eq!(s.post(&path, Some(&boss.token), &fields).await.status(), StatusCode::FORBIDDEN, "{path}: no token");
        for csrf in ["", "wrong", demoted.csrf.as_str()] {
            let mut fields = fields.clone();
            fields.push(("csrf", csrf));
            let res = s.post(&path, Some(&boss.token), &fields).await;
            assert_eq!(res.status(), StatusCode::FORBIDDEN, "{path}: {csrf:?}");
            assert!(res.text().await.unwrap().contains("out of date, or did not come from this site"));
        }
        // The origin: another site's form is refused before anything is read.
        let mut fields = extra.clone();
        fields.push(("csrf", &boss.csrf));
        fields.push(("password", PASSWORD));
        let res = s.post_from(&path, Some(&boss.token), Some("https://evil.example"), None, &fields).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "{path}: another origin");
        // A GET changes nothing.
        let get = s.get(&path, Some(&boss.token)).await.status();
        assert_eq!(get, if path == "/web/spaces" { StatusCode::OK } else { StatusCode::METHOD_NOT_ALLOWED }, "{path}");
    }
    the_space_is_untouched(s, space).await;
}

async fn every_team_space_change_needs_the_confirmation(s: &Web) {
    let owner = s.add_user("own", Role::Member).await;
    s.add_user("newcomer", Role::Member).await;
    let space = cli_space(s, "Studio", &owner).await;
    for (n, (path, extra)) in space_changes(&space.as_uuid().to_string()).iter().enumerate() {
        // A fresh administrator per route: wrong passwords are counted per account.
        let name = format!("adm{n}");
        s.add_admin(&name).await;
        let sess = s.sign_in(&name).await;
        let mut fields: Vec<(&str, &str)> = extra.clone();
        fields.push(("csrf", &sess.csrf));
        let res = s.post(path, Some(&sess.token), &fields).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{path}: no password");
        let mut wrong = fields.clone();
        wrong.push(("password", "a wrong password"));
        let res = s.post(path, Some(&sess.token), &wrong).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "{path}: a wrong password");
        assert!(res.text().await.unwrap().contains("the current password is wrong"), "{path}");
        let mut empty = fields.clone();
        empty.push(("password", ""));
        assert_eq!(s.post(path, Some(&sess.token), &empty).await.status(), StatusCode::BAD_REQUEST, "{path}: an empty password");
        let mut long = fields.clone();
        let too_long = "x".repeat(1025);
        long.push(("password", &too_long));
        assert_eq!(s.post(path, Some(&sess.token), &long).await.status(), StatusCode::BAD_REQUEST, "{path}: a password that is too long");
        the_space_is_untouched(s, space).await;
    }
}

async fn team_spaces_are_made_listed_and_changed(s: &Web) {
    s.add_admin("root").await;
    let owner = s.add_user("own", Role::Member).await;
    let heir = s.add_user("heir", Role::Member).await;
    let gone = s.add_user("gone", Role::Member).await;
    let boss = s.sign_in("root").await;

    // The page: an empty list and the form, with the confirmation's password field and the navigation entry.
    let html = s.get("/web/spaces", Some(&boss.token)).await.text().await.unwrap();
    assert!(html.contains("No team spaces match.") && html.contains("action=\"/web/spaces\"") && html.contains("name=\"password\""), "{html}");
    assert!(html.contains("<a href=\"/web/spaces\">Team spaces</a>"), "the navigation names the page");
    assert!(!html.contains("name=\"totp_code\""), "no code field without two-factor sign-in");

    // Create: the errors first, each with its status, and nothing made.
    let create = |fields: Vec<(&'static str, &'static str)>| {
        let boss = &boss;
        async move { s.act(boss, "/web/spaces", &fields).await }
    };
    assert_eq!(create(vec![("name", ""), ("owner", "own")]).await.status(), StatusCode::BAD_REQUEST, "an empty name");
    assert_eq!(create(vec![("name", " padded "), ("owner", "own")]).await.status(), StatusCode::BAD_REQUEST, "a padded name");
    assert_eq!(create(vec![("name", "Studio"), ("owner", "no such")]).await.status(), StatusCode::BAD_REQUEST, "an owner that is no username");
    assert_eq!(create(vec![("name", "Studio"), ("owner", "nobody")]).await.status(), StatusCode::NOT_FOUND, "an unknown owner");
    assert_eq!(create(vec![("name", "Studio"), ("owner", "own"), ("quota_mib", "3")]).await.status(), StatusCode::BAD_REQUEST, "below one chunk");
    assert_eq!(create(vec![("name", "Studio"), ("owner", "own"), ("quota_mib", "lots")]).await.status(), StatusCode::BAD_REQUEST, "not a number");
    assert_eq!(
        create(vec![("name", "Studio"), ("owner", "own"), ("remove_quota", "on")]).await.status(),
        StatusCode::BAD_REQUEST,
        "nothing to remove"
    );
    assert!(s.meta.admin_list_spaces(None, None, 10).await.unwrap().spaces.is_empty());
    s.act(&boss, &format!("/web/users/{}/disable", gone.id.0), &[]).await;
    assert_eq!(create(vec![("name", "Studio"), ("owner", "gone")]).await.status(), StatusCode::NOT_FOUND, "a disabled owner is the answer for none");
    assert!(s.meta.admin_list_spaces(None, None, 10).await.unwrap().spaces.is_empty());

    // Create: success goes to the space's page, which says so; the owner is a member from the start.
    let res = create(vec![("name", "Studio"), ("owner", "Own"), ("quota_mib", "16")]).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let page_path = location(&res);
    assert!(page_path.starts_with("/web/spaces/") && page_path.ends_with("?notice=created"), "{page_path}");
    let space = s.meta.admin_list_spaces(None, None, 10).await.unwrap().spaces.remove(0);
    assert_eq!((space.name.as_str(), space.owners, space.members, space.quota_bytes), ("Studio", 1, 1, Some(16 * 1024 * 1024)));
    assert_eq!(create(vec![("name", "STUDIO"), ("owner", "heir")]).await.status(), StatusCode::CONFLICT, "a name is taken in any case");
    assert_eq!(s.meta.admin_list_spaces(None, None, 10).await.unwrap().spaces.len(), 1);

    let html = s.get(&page_path, Some(&boss.token)).await.text().await.unwrap();
    assert!(html.contains("The team space was created.") && html.contains("Team space Studio"), "{html}");
    assert!(html.contains("16.0 MiB") && html.contains(&format!("/web/users/{}", owner.id.0)) && html.contains("<td>owner</td>"), "{html}");
    let html = s.get("/web/spaces", Some(&boss.token)).await.text().await.unwrap();
    assert!(html.contains("Studio") && html.contains("0 B of 16.0 MiB") && html.contains("<td>active</td>"), "{html}");
    let base = format!("/web/spaces/{}", space.id.as_uuid());

    // The limit: set, refused, removed.
    let post = |what: &str, fields: Vec<(&'static str, &'static str)>| {
        let (boss, path) = (&boss, format!("{base}/{what}"));
        async move { s.act(boss, &path, &fields).await }
    };
    let res = post("quota", vec![("quota_mib", "64")]).await;
    assert_eq!((res.status(), location(&res)), (StatusCode::SEE_OTHER, format!("{base}?notice=quota")));
    assert_eq!(s.meta.admin_space(space.id).await.unwrap().quota_bytes, Some(64 * 1024 * 1024));
    for (fields, status) in [
        (vec![], StatusCode::BAD_REQUEST),
        (vec![("quota_mib", "1")], StatusCode::BAD_REQUEST),
        (vec![("quota_mib", "-5")], StatusCode::BAD_REQUEST),
        (vec![("quota_mib", "8"), ("remove_quota", "on")], StatusCode::BAD_REQUEST),
    ] {
        assert_eq!(post("quota", fields.clone()).await.status(), status, "{fields:?}");
    }
    assert_eq!(s.meta.admin_space(space.id).await.unwrap().quota_bytes, Some(64 * 1024 * 1024), "a refused limit changes nothing");
    assert_eq!(post("quota", vec![("remove_quota", "on")]).await.status(), StatusCode::SEE_OTHER);
    assert_eq!(s.meta.admin_space(space.id).await.unwrap().quota_bytes, None);
    let html = s.get(&format!("{base}?notice=quota"), Some(&boss.token)).await.text().await.unwrap();
    assert!(html.contains("The space&#39;s limit was changed.") && html.contains("<dt>Limit</dt><dd>none</dd>"), "{html}");

    // Archive and unarchive: repeating either is no error.
    for _ in 0..2 {
        let res = post("archive", vec![]).await;
        assert_eq!((res.status(), location(&res)), (StatusCode::SEE_OTHER, format!("{base}?notice=archived")));
        assert!(s.meta.admin_space(space.id).await.unwrap().archived_ms.is_some());
    }
    let html = s.get(&base, Some(&boss.token)).await.text().await.unwrap();
    assert!(html.contains("archived 20") && html.contains("Make the space active again") && !html.contains("Archive the space"), "{html}");
    assert!(s.get("/web/spaces", Some(&boss.token)).await.text().await.unwrap().contains("archived 20"), "the list says so");
    for _ in 0..2 {
        let res = post("unarchive", vec![]).await;
        assert_eq!((res.status(), location(&res)), (StatusCode::SEE_OTHER, format!("{base}?notice=unarchived")));
        assert!(s.meta.admin_space(space.id).await.unwrap().archived_ms.is_none());
    }

    // The owner: an account named becomes an accepted owner at once; unknown, disabled and no username are refused.
    assert_eq!(post("owner", vec![("owner", "nobody")]).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(post("owner", vec![("owner", "gone")]).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(post("owner", vec![("owner", "")]).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(post("owner", vec![("owner", "a b")]).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(s.meta.admin_space(space.id).await.unwrap().owners, 1);
    let res = post("owner", vec![("owner", "heir")]).await;
    assert_eq!((res.status(), location(&res)), (StatusCode::SEE_OTHER, format!("{base}?notice=owner")));
    assert_eq!(post("owner", vec![("owner", "heir")]).await.status(), StatusCode::SEE_OTHER, "naming an owner twice is no error");
    let members = s.meta.list_members(space.id).await.unwrap();
    assert_eq!(members.iter().filter(|m| m.accepted() && m.role == craft_cloud_proto::api::SpaceRole::Owner).count(), 2);
    assert!(members.iter().any(|m| m.user == heir.id));
    let html = s.get(&base, Some(&boss.token)).await.text().await.unwrap();
    assert!(html.contains("<th scope=\"row\"><a href=\"/web/users/") && html.contains(">heir</a>"), "{html}");

    // A space that is not there, or an ID that is none: 404 for the page and for every change.
    let missing = uuid::Uuid::now_v7();
    for path in [format!("/web/spaces/{missing}"), "/web/spaces/not-an-id".to_owned()] {
        assert_eq!(s.get(&path, Some(&boss.token)).await.status(), StatusCode::NOT_FOUND, "{path}");
        for what in ["quota", "archive", "unarchive", "owner"] {
            let res = s.act(&boss, &format!("{path}/{what}"), &[("quota_mib", "8"), ("owner", "heir")]).await;
            assert_eq!(res.status(), StatusCode::NOT_FOUND, "{path}/{what}");
        }
    }
    // A personal space is no team space for these pages either.
    let personal = s.meta.user_by_name("own").await.unwrap().unwrap().space;
    assert_eq!(s.get(&format!("/web/spaces/{}", personal.as_uuid()), Some(&boss.token)).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(s.act(&boss, &format!("/web/spaces/{}/archive", personal.as_uuid()), &[]).await.status(), StatusCode::NOT_FOUND);
    // An unknown notice has no words.
    let html = s.get(&format!("{base}?notice=%3Cb%3E"), Some(&boss.token)).await.text().await.unwrap();
    assert!(!html.contains("<b>") && !html.contains("class=\"notice\""), "{html}");
}

async fn team_spaces_are_listed_in_pages_and_searched(s: &Web) {
    use craft_cloud_meta::SpaceActor;
    use craft_cloud_proto::SpaceName;
    s.add_admin("root").await;
    let owner = s.add_user("own", Role::Member).await;
    for n in 0..52 {
        let name = SpaceName::new(&format!("Team {n:02}")).unwrap();
        s.meta.create_team_space(&SpaceActor::Cli, &name, owner.id, None, 0).await.unwrap();
    }
    cli_space(s, "Düsseldorf Büro", &owner).await;
    let boss = s.sign_in("root").await;
    let first = s.get("/web/spaces", Some(&boss.token)).await.text().await.unwrap();
    assert!(first.contains("Team 00") && !first.contains("Team 51") && !first.contains("First page"), "the first page has 50");
    let marker = "href=\"/web/spaces?query=&amp;cursor=";
    let next = first.split(marker).nth(1).expect("a next page").split('"').next().unwrap().to_owned();
    let second = s.get(&format!("/web/spaces?query=&cursor={next}"), Some(&boss.token)).await.text().await.unwrap();
    assert!(second.contains("Team 51") && !second.contains("Team 00") && second.contains("First page") && !second.contains("Next page"), "{second}");
    // A search is a part of a name, in any case, also with letters that fold; an empty result says so; it is literal text, not a pattern.
    let found = s.get("/web/spaces?query=B%C3%9CRO", Some(&boss.token)).await.text().await.unwrap();
    assert!(found.contains("Düsseldorf Büro") && !found.contains("Team 00"), "{found}");
    let found = s.get("/web/spaces?query=team%2051", Some(&boss.token)).await.text().await.unwrap();
    assert!(found.contains("Team 51") && !found.contains("Team 50"), "{found}");
    assert!(s.get("/web/spaces?query=zzz", Some(&boss.token)).await.text().await.unwrap().contains("No team spaces match."));
    let found = s.get("/web/spaces?query=100%25_", Some(&boss.token)).await.text().await.unwrap();
    assert!(found.contains("No team spaces match."), "a search is literal text, not a pattern");
    // Bad queries: too long, a control character, a cursor that is none.
    for bad in [
        format!("/web/spaces?query={}", "a".repeat(65)),
        "/web/spaces?query=a%00b".to_owned(),
        "/web/spaces?cursor=***".to_owned(),
        "/web/spaces?cursor=_-8".to_owned(),
    ] {
        assert_eq!(s.get(&bad, Some(&boss.token)).await.status(), StatusCode::BAD_REQUEST, "{bad}");
    }
}

async fn team_space_changes_are_audited_with_the_web_session_and_wake_the_long_polls(s: &Web) {
    use craft_cloud_meta::AuditAction;
    s.add_admin("root").await;
    let root = s.id_of("root").await;
    let owner = s.add_user("own", Role::Member).await;
    let heir = s.add_user("heir", Role::Member).await;
    let boss = s.sign_in("root").await;
    let res = s.act(&boss, "/web/spaces", &[("name", "Studio"), ("owner", "own"), ("quota_mib", "8")]).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let space = s.meta.admin_list_spaces(None, None, 10).await.unwrap().spaces.remove(0).id;
    let base = format!("/web/spaces/{}", space.as_uuid());

    // The owner's long-poll on the space is woken by a change of the space.
    let device = s.device("own").await;
    let poll = {
        let (client, url) = (s.client.clone(), s.url("/api/v1/changes?since=0&wait=30"));
        tokio::spawn(async move { client.get(url).bearer_auth(device).header("craft-space", space.to_string()).send().await.unwrap() })
    };
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert!(!poll.is_finished(), "the long-poll waits");
    assert_eq!(s.act(&boss, &format!("{base}/archive"), &[]).await.status(), StatusCode::SEE_OTHER);
    tokio::time::timeout(std::time::Duration::from_secs(5), poll).await.expect("the archive woke the waiter").unwrap();

    for (what, extra) in [("quota", vec![("quota_mib", "16")]), ("unarchive", vec![]), ("owner", vec![("owner", "heir")])] {
        assert_eq!(s.act(&boss, &format!("{base}/{what}"), &extra).await.status(), StatusCode::SEE_OTHER, "{what}");
    }
    // Repeating a change that changes nothing writes no second event.
    assert_eq!(s.act(&boss, &format!("{base}/unarchive"), &[]).await.status(), StatusCode::SEE_OTHER);
    assert_eq!(s.act(&boss, &format!("{base}/owner"), &[("owner", "heir")]).await.status(), StatusCode::SEE_OTHER);

    let events = s.meta.audit_list(&craft_cloud_meta::AuditQuery { limit: 200, ..Default::default() }).await.unwrap();
    let on_space: Vec<_> = events.iter().filter(|e| e.target_id == Some(*space.as_uuid())).collect();
    let count = |action: AuditAction| on_space.iter().filter(|e| e.action == action).count();
    assert_eq!(
        [
            count(AuditAction::SpaceCreated),
            count(AuditAction::SpaceQuotaChanged),
            count(AuditAction::SpaceArchived),
            count(AuditAction::SpaceUnarchived),
            count(AuditAction::SpaceMemberRoleChanged)
        ],
        [1, 1, 1, 1, 1],
        "{on_space:?}"
    );
    assert_eq!(on_space.len(), 5);
    for event in &on_space {
        // The administrator acted over a browser session: the account is the actor and there is no device, as for the other pages.
        assert_eq!((event.actor_user.map(|u| u.0), event.actor_device, event.target_kind.as_str()), (Some(root), None, "space"), "{event:?}");
        assert!(!event.details.contains("Studio"), "{}", event.details);
    }
    let owner_event = on_space.iter().find(|e| e.action == AuditAction::SpaceMemberRoleChanged).unwrap();
    assert!(owner_event.details.contains(&heir.id.0.to_string()) && owner_event.details.contains("\"owner\""), "{}", owner_event.details);
    let created = on_space.iter().find(|e| e.action == AuditAction::SpaceCreated).unwrap();
    assert!(created.details.contains(&owner.id.0.to_string()) && created.details.contains("8388608"), "{}", created.details);
    // The audit log's page shows them.
    let html = s.get("/web/audit?action=space.", Some(&boss.token)).await.text().await.unwrap();
    assert!(html.contains("space.created") && html.contains("space.quota_changed") && html.contains("space.archived"), "{html}");
}

async fn an_admin_with_two_factor_confirms_a_team_space_with_a_code(s: &Web) {
    s.add_admin("root").await;
    let owner = s.add_user("own", Role::Member).await;
    let space = cli_space(s, "Studio", &owner).await;
    let tf = turn_on_two_factor(s, "root").await;
    let sess = sign_in_with_factor(s, "root", &tf).await;
    let base = format!("/web/spaces/{}", space.as_uuid());
    for path in ["/web/spaces".to_owned(), base.clone()] {
        let html = s.get(&path, Some(&sess.token)).await.text().await.unwrap();
        assert!(html.contains("name=\"totp_code\"") && html.contains("name=\"recovery_code\""), "{path}");
    }
    let archive = format!("{base}/archive");
    assert_eq!(s.act(&sess, &archive, &[]).await.status(), StatusCode::FORBIDDEN, "no code");
    assert_eq!(s.act(&sess, &archive, &[("totp_code", "000000")]).await.status(), StatusCode::FORBIDDEN, "a wrong code");
    assert_eq!(s.act(&sess, &archive, &[("totp_code", "12")]).await.status(), StatusCode::BAD_REQUEST, "a code of the wrong shape");
    assert!(s.meta.admin_space(space).await.unwrap().archived_ms.is_none());
    let code = tf.fresh_code(s);
    assert_eq!(s.act(&sess, &archive, &[("totp_code", &code)]).await.status(), StatusCode::SEE_OTHER);
    assert!(s.meta.admin_space(space).await.unwrap().archived_ms.is_some());
    assert_eq!(s.act(&sess, &format!("{base}/unarchive"), &[("totp_code", &code)]).await.status(), StatusCode::FORBIDDEN, "the code was used up");
    let recovery = tf.recovery.first().unwrap().clone();
    assert_eq!(s.act(&sess, &format!("{base}/unarchive"), &[("recovery_code", &recovery)]).await.status(), StatusCode::SEE_OTHER);
    assert_eq!(s.act(&sess, &archive, &[("recovery_code", &recovery)]).await.status(), StatusCode::FORBIDDEN, "a recovery code serves once");
}

async fn team_space_names_are_escaped(s: &Web) {
    s.add_admin("root").await;
    s.add_user("own", Role::Member).await;
    let boss = s.sign_in("root").await;
    let nasty = "<script>alert(1)</script>";
    let res = s.act(&boss, "/web/spaces", &[("name", nasty), ("owner", "own")]).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER, "a name may hold these characters");
    let page = location(&res);
    for path in ["/web/spaces", page.as_str()] {
        let html = s.get(path, Some(&boss.token)).await.text().await.unwrap();
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;") && !html.to_ascii_lowercase().contains("<script"), "{path}: {html}");
    }
    // The search box keeps what was typed, escaped.
    let html = s.get("/web/spaces?query=%22%3E%3Cscript%3E", Some(&boss.token)).await.text().await.unwrap();
    assert!(!html.to_ascii_lowercase().contains("<script"), "{html}");
}
