//! The admin pages in the browser, under `/web`: server-rendered HTML in Rust, with no script, no build step and no
//! file but one stylesheet. They are an adapter over the same operations as the JSON admin API ([`crate::admin`],
//! [`crate::audit`]): a form is read, checked, and handed to the same `craft_cloud_meta` call with the same confirmation, so
//! the rules (last administrator, who may reset whom, quotas) have one home. They are off unless `[web_admin] enabled = true`.
//!
//! - **Browser sessions** ([`craft_cloud_meta::NewWebSession`]): `POST /web/login` takes username and password and, for an
//!   account with two-factor sign-in, `POST /web/login/second-factor` the code, with the JSON sign-in's counters, locks and
//!   password-check slots (and limiters of their own, [`AppState::web_logins`]). Only administrators get a session. Every
//!   refusal of the password step reads the same ("wrong username or password") whether the name exists, the password is
//!   wrong, the account is a member's or is disabled. The session is a random 32-byte token in the cookie
//!   `__Host-craft_cloud_session` (`Secure`, `HttpOnly`, `SameSite=Strict`, `Path=/`, no `Domain`, no lifetime of its own:
//!   the server decides); the database keeps only its BLAKE3 hash. Idle time and absolute lifetime come from the config.
//! - **One credential at a time.** The pages know only the cookie, the JSON admin routes only the bearer token. A request
//!   to either with both is `400 invalid`, so a client can't be confused about which one acted ([`reject_mixed_credentials`]).
//! - **CSRF**: every form carries `csrf`, derived from the session token with a key-derivation context of its own
//!   (so the stored hash can't make it) and compared in constant time. A missing, wrong or another session's token is `403`.
//!   `SameSite=Strict` is a second line, not the first. No `GET` changes anything.
//! - **Origin**: every request that is not a `GET` or `HEAD`, the sign-in's too, must carry an `Origin` equal to the origin of
//!   `public_url`, or, when it has none, a `Referer` of that origin. Anything else (including `Origin: null`) is `403`.
//! - **Headers**: every answer under `/web` has a CSP that allows no script and no other host (`default-src 'self'; script-src
//!   'none'; …; frame-ancestors 'none'`), `nosniff`, `Referrer-Policy: no-referrer`, `no-store` and the framing and
//!   cross-origin protections ([`secure_headers`]).
//! - **Statuses**: `401` for a missing, expired or unknown session (a `GET` is sent to the sign-in page instead), for a wrong
//!   username or password at sign-in and for a sign-in step that is unknown or used up; `403` for a member, a wrong second factor,
//!   a wrong confirmation password or code, a bad CSRF token or Origin; `400` for a malformed form (a body that is not a form
//!   too); `429` for limits and locks, always with `Retry-After`. `docs/design/sync.md` has the contract.
//! - **Every change asks for the confirmation** in the form (the admin's password, and a code when their account has
//!   two-factor sign-in): [`crate::admin::confirm_actor`], the JSON routes' own function.
//! - **Nothing secret is logged**: not a password, a code, a token, a CSRF token or a cookie. Events are logged at `info`
//!   with the kind and the target's ID. Codes the admin makes are shown once in the answer to the `POST` and kept nowhere.
//! - **Behind a proxy**: `public_url` is the pages' origin, the cookie is always `Secure`, and the client address that the
//!   limiters count is the one `trusted_proxies` yields ([`crate::ratelimit::client_ip`]), as for the JSON sign-in.

mod html;
mod pages;
mod team;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Form, FromRequest, Request, State};
use axum::http::header::{
    AUTHORIZATION, CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, COOKIE, LOCATION, ORIGIN, REFERER, REFERRER_POLICY, RETRY_AFTER, SET_COOKIE,
    X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use craft_cloud_meta::{MetaError, NewChallenge, NewWebSession, Role, WebSession, WebSignIn, valid_username};
use serde::Deserialize;
use subtle::ConstantTimeEq;

use crate::api::{LOGINS_PER_IP, LOGINS_PER_USER, PASSWORD_RETRY_SECS, TINY_JSON_BYTES, body_too_slow};
use crate::audit;
use crate::auth::{MAX_PASSWORD_BYTES, new_token, token_hash, verify_password};
use crate::error::ApiError;
use crate::ratelimit::{client_ip, ip_key};
use crate::state::AppState;
use crate::two_factor::{SECOND_FACTORS_PER_IP, SECOND_FACTORS_PER_USER, challenge_hash, parse_proof};
use html::{Ctx, heading, notice, page};

pub(crate) type Shared = Arc<AppState>;

/// The session cookie. The `__Host-` prefix makes a browser take it only with `Secure`, `Path=/` and no `Domain`, and only
/// from a response of the host itself.
pub const COOKIE_NAME: &str = "__Host-craft_cloud_session";
/// A session's last-seen time is written at most this often (every write takes the SQLite write lock).
const TOUCH_INTERVAL_MS: i64 = 60_000;
/// Expired sessions one sign-in deletes on the way (the rest waits for the next one or `web-sessions prune`).
const OPPORTUNISTIC_PRUNE_ROWS: u32 = 100;
/// The message of every refused password step.
const WRONG_LOGIN: &str = "Wrong username or password.";
const CSRF_CONTEXT: &str = "craft-cloud web csrf v1";
/// Content Security Policy of every page: only this host, no script at all, no framing, forms only back to this host.
const CSP: &str = "default-src 'self'; script-src 'none'; style-src 'self'; img-src 'self'; connect-src 'none'; object-src 'none'; \
                   base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/// How the pages know their address and the sessions' lifetimes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSettings {
    /// A session ends when it was not used for this long.
    pub idle_ms: i64,
    /// A session ends this long after sign-in at the latest.
    pub absolute_ms: i64,
    origin: String,
}

impl WebSettings {
    /// The settings for pages at `public_url`, an `https://host[:port]` address without a path (the config checks the same).
    pub fn new(public_url: &str, idle_ms: i64, absolute_ms: i64) -> Result<Self, String> {
        let rest = public_url.strip_prefix("https://").map(|rest| rest.trim_end_matches('/')).filter(|rest| !rest.is_empty() && !rest.contains('/'));
        let Some(rest) = rest else {
            return Err(format!("the web admin needs public_url = \"https://host[:port]\" without a path, not {public_url:?}"));
        };
        // A browser leaves out the port of the scheme, so the origin it sends has none either.
        let host = rest.strip_suffix(":443").unwrap_or(rest).to_ascii_lowercase();
        Ok(Self { idle_ms, absolute_ms, origin: format!("https://{host}") })
    }

    /// The origin forms must come from.
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// Whether `headers` come from this origin: an `Origin` that is exactly it, or, without one, a `Referer` on it.
    fn allows(&self, headers: &HeaderMap) -> bool {
        let mut origins = headers.get_all(ORIGIN).iter();
        match (origins.next(), origins.next()) {
            (Some(origin), None) => origin.to_str().is_ok_and(|origin| origin == self.origin),
            (Some(_), Some(_)) => false,
            (None, _) => {
                headers.get_all(REFERER).iter().count() == 1 && headers.get(REFERER).and_then(|v| v.to_str().ok()).is_some_and(|r| self.on_origin(r))
            }
        }
    }

    fn on_origin(&self, url: &str) -> bool {
        let Some(rest) = url.strip_prefix(self.origin.as_str()) else { return false };
        rest.is_empty() || rest.starts_with(['/', '?', '#'])
    }
}

// ---- errors and answers ----

/// A failed request, answered as a page. Built from [`ApiError`]s too, so a refusal reads like the JSON API's, with its message.
#[derive(Debug)]
pub(crate) struct WebError {
    status: StatusCode,
    message: String,
    retry_after: Option<u32>,
    /// A `GET` that needs a sign-in is sent to the sign-in page instead of being told 401.
    to_login: bool,
    clear_cookie: bool,
}

impl WebError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self { status, message: message.into(), retry_after: None, to_login: false, clear_cookie: false }
    }

    /// No session, or one that is over or unknown: 401, and the cookie is dropped.
    fn signed_out() -> Self {
        Self { clear_cookie: true, ..Self::new(StatusCode::UNAUTHORIZED, "Sign in to continue.") }
    }

    /// For a `GET`: a missing session goes to the sign-in page.
    fn for_get(mut self) -> Self {
        self.to_login = self.status == StatusCode::UNAUTHORIZED;
        self
    }

    fn csrf() -> Self {
        Self::new(StatusCode::FORBIDDEN, "This form is out of date, or did not come from this site. Reload the page and try again.")
    }

    pub(crate) fn not_found(what: &str) -> Self {
        Self::new(StatusCode::NOT_FOUND, format!("No such {what}."))
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
}

impl From<ApiError> for WebError {
    fn from(e: ApiError) -> Self {
        let mut error = Self::new(e.status, e.message);
        error.retry_after = e.retry_after;
        // A 401 inside a signed-in page means the session's account is gone.
        error.clear_cookie = e.status == StatusCode::UNAUTHORIZED;
        error
    }
}

impl From<MetaError> for WebError {
    fn from(e: MetaError) -> Self {
        ApiError::from(e).into()
    }
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        let mut response = if self.to_login {
            redirect("/web/login")
        } else if self.status == StatusCode::UNAUTHORIZED {
            let body = format!("{}{}{}", heading("Sign in"), notice(&self.message, true), pages::login_form());
            html_response(self.status, &page(None, "Sign in", &body))
        } else {
            let body = format!(
                "{}{}<p><a href=\"/web\">Back to the admin pages</a></p>",
                heading(&format!("{} {}", self.status.as_u16(), self.status.canonical_reason().unwrap_or("Error"))),
                notice(&self.message, true)
            );
            html_response(self.status, &page(None, "Error", &body))
        };
        if let Some(secs) = self.retry_after {
            response.headers_mut().insert(RETRY_AFTER, HeaderValue::from(secs));
        }
        if self.clear_cookie {
            response.headers_mut().append(SET_COOKIE, clear_cookie());
        }
        response
    }
}

pub(crate) fn html_response(status: StatusCode, body: &str) -> Response {
    let mut response = Response::new(Body::from(body.to_owned()));
    *response.status_mut() = status;
    response.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    response
}

/// `303 See Other` to a path of this code's making.
pub(crate) fn redirect(to: &str) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::SEE_OTHER;
    if let Ok(value) = HeaderValue::from_str(to) {
        response.headers_mut().insert(LOCATION, value);
    }
    response
}

fn set_cookie(token: &str) -> Result<HeaderValue, WebError> {
    // No lifetime: the browser keeps it until it closes, the server ends it by its own clock.
    HeaderValue::from_str(&format!("{COOKIE_NAME}={token}; Path=/; Secure; HttpOnly; SameSite=Strict")).map_err(|_| ApiError::internal().into())
}

fn clear_cookie() -> HeaderValue {
    HeaderValue::from_static(
        "__Host-craft_cloud_session=; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age=0; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
    )
}

// ---- the headers and the origin ----

/// Puts the protective headers on every answer under `/web`, errors and redirects included.
async fn secure_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(HeaderName::from_static("cross-origin-opener-policy"), HeaderValue::from_static("same-origin"));
    headers.insert(HeaderName::from_static("cross-origin-resource-policy"), HeaderValue::from_static("same-origin"));
    headers.insert(HeaderName::from_static("permissions-policy"), HeaderValue::from_static("camera=(), microphone=(), geolocation=()"));
    if !headers.contains_key(CACHE_CONTROL) {
        headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

/// Refuses every request that is not a `GET` or `HEAD` unless it comes from the pages' own origin.
async fn same_origin_posts(State(state): State<Shared>, request: Request, next: Next) -> Response {
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        let allowed = state.web.as_ref().is_some_and(|web| web.allows(request.headers()));
        if !allowed {
            tracing::info!("web admin: a form from another origin was refused");
            return WebError::new(StatusCode::FORBIDDEN, "This request did not come from this site's own pages, so it was refused.").into_response();
        }
    }
    next.run(request).await
}

// ---- the session ----

/// Whether the request has a cookie of the session's name, valid or not.
pub(crate) fn has_session_cookie(headers: &HeaderMap) -> bool {
    cookie_values(headers).next().is_some()
}

fn cookie_values(headers: &HeaderMap) -> impl Iterator<Item = &str> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|line| line.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .filter(|(name, _)| *name == COOKIE_NAME)
        .map(|(_, value)| value)
}

/// The session token of the request: the value of exactly one cookie of the session's name, shaped like a token. Two cookies
/// of that name, or one of another shape, are no session.
fn session_token(headers: &HeaderMap) -> Option<&str> {
    let mut values = cookie_values(headers);
    let (first, second) = (values.next()?, values.next());
    let is_token = first.len() == 43 && first.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    (second.is_none() && is_token).then_some(first)
}

/// The JSON admin routes' guard: a bearer token and the session cookie together are `400 invalid`.
pub(crate) async fn reject_mixed_credentials(request: Request, next: Next) -> Result<Response, ApiError> {
    if request.headers().contains_key(AUTHORIZATION) && has_session_cookie(request.headers()) {
        return Err(ApiError::invalid("send either the bearer token or the session cookie, not both"));
    }
    Ok(next.run(request).await)
}

/// The CSRF token of a session: derived from its token under a context of its own, so the hash in the database can't make it.
fn csrf_token(session_token: &str) -> String {
    URL_SAFE_NO_PAD.encode(blake3::derive_key(CSRF_CONTEXT, session_token.as_bytes()))
}

fn csrf_matches(expected: &str, given: &str) -> bool {
    expected.len() == given.len() && bool::from(expected.as_bytes().ct_eq(given.as_bytes()))
}

/// A signed-in administrator.
pub(crate) struct Authed {
    pub session: WebSession,
    /// The hash of the session token (what the database knows it by).
    pub hash: [u8; 32],
    csrf: String,
}

impl Authed {
    pub(crate) fn ctx(&self) -> Ctx {
        Ctx { username: self.session.username.clone(), csrf: self.csrf.clone() }
    }

    pub(crate) fn check_csrf(&self, given: &str) -> Result<(), WebError> {
        if csrf_matches(&self.csrf, given) { Ok(()) } else { Err(WebError::csrf()) }
    }
}

/// Signs the request in from its cookie: `400` for a bearer token beside it, `401` without a valid session, `403` for one
/// of an account that is no (longer an) administrator. The session's last-seen time is moved when it is a minute old.
pub(crate) async fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<Authed, WebError> {
    let settings = state.web.as_ref().ok_or_else(|| WebError::not_found("page"))?;
    if headers.contains_key(AUTHORIZATION) && has_session_cookie(headers) {
        return Err(WebError::invalid("Send either a bearer token or the session cookie, not both."));
    }
    let token = session_token(headers).ok_or_else(WebError::signed_out)?;
    let hash = token_hash(token);
    let now = state.now_ms();
    let session = state.meta.web_session(&hash, now, settings.idle_ms).await?.ok_or_else(WebError::signed_out)?;
    if session.role != Role::Admin {
        return Err(WebError::new(StatusCode::FORBIDDEN, "This needs an administrator account."));
    }
    if now.saturating_sub(session.last_seen_ms) >= TOUCH_INTERVAL_MS
        && let Err(e) = state.meta.web_session_seen(&hash, now).await
    {
        tracing::warn!(error = %e, "couldn't record the use of a web session");
    }
    Ok(Authed { csrf: csrf_token(token), session, hash })
}

/// Reads a form of at most [`TINY_JSON_BYTES`] under the usual deadline. One that is not a form is `400`, too long `413`.
pub(crate) async fn read_form<T: serde::de::DeserializeOwned>(state: &AppState, request: Request) -> Result<T, WebError> {
    match tokio::time::timeout(state.limits.body_time(TINY_JSON_BYTES), Form::<T>::from_request(request, &())).await {
        Ok(Ok(Form(form))) => Ok(form),
        // A body that is not a form is a malformed request (`400`), not a media-type negotiation: the pages have one contract.
        Ok(Err(rejection)) => {
            let status = if rejection.status() == StatusCode::UNSUPPORTED_MEDIA_TYPE { StatusCode::BAD_REQUEST } else { rejection.status() };
            Err(WebError::new(status, "The form could not be read."))
        }
        Err(_) => Err(body_too_slow().into()),
    }
}

/// The query of a `GET`, read from the URI. One that does not fit is `400`.
pub(crate) fn read_query<T: serde::de::DeserializeOwned>(request: &Request) -> Result<T, WebError> {
    axum::extract::Query::<T>::try_from_uri(request.uri())
        .map(|query| query.0)
        .map_err(|_| WebError::invalid("The address has parameters this page does not understand."))
}

// ---- the sign-in ----

#[derive(Deserialize, Default)]
#[serde(default)]
struct LoginForm {
    username: String,
    password: String,
}

impl std::fmt::Debug for LoginForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginForm").field("username", &"<redacted>").field("password", &"<redacted>").finish()
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SecondFactorForm {
    challenge: String,
    totp_code: String,
    recovery_code: String,
}

impl std::fmt::Debug for SecondFactorForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecondFactorForm")
            .field("challenge", &"<redacted>")
            .field("totp_code", &"<redacted>")
            .field("recovery_code", &"<redacted>")
            .finish()
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CsrfForm {
    csrf: String,
}

#[derive(Deserialize, Default)]
struct NoticeQuery {
    notice: Option<String>,
}

/// The sign-in page again, with `status` and a message (a sign-in that did not work).
fn login_answer(status: StatusCode, message: &str, retry_after: Option<u32>) -> Response {
    let body = format!("{}{}{}", heading("Sign in"), notice(message, true), pages::login_form());
    let mut response = html_response(status, &page(None, "Sign in", &body));
    if let Some(secs) = retry_after {
        response.headers_mut().insert(RETRY_AFTER, HeaderValue::from(secs));
    }
    response
}

/// The second step's page, with the challenge the first step made.
fn second_factor_answer(status: StatusCode, challenge: &str, message: Option<&str>, retry_after: Option<u32>) -> Response {
    let note = message.map(|message| notice(message, true)).unwrap_or_default();
    let body = format!("{}{note}{}", heading("Second factor"), pages::second_factor_form(challenge));
    let mut response = html_response(status, &page(None, "Second factor", &body));
    if let Some(secs) = retry_after {
        response.headers_mut().insert(RETRY_AFTER, HeaderValue::from(secs));
    }
    response
}

/// `GET /web/login`.
async fn login_page(request: Request) -> Result<Response, WebError> {
    let query: NoticeQuery = read_query(&request)?;
    let note = match query.notice.as_deref() {
        Some("signed_out") => notice("You are signed out.", false),
        _ => String::new(),
    };
    Ok(html_response(StatusCode::OK, &page(None, "Sign in", &format!("{}{note}{}", heading("Sign in"), pages::login_form()))))
}

/// A session for `user` made and the browser sent on to the overview with its cookie. Expired sessions are cleaned up on the way,
/// a few at a time and without letting a failure change the sign-in.
async fn start_session(state: &AppState, token: &str) -> Result<Response, WebError> {
    if let Some(settings) = &state.web
        && let Err(e) = state.meta.web_sessions_prune(state.now_ms(), settings.idle_ms, OPPORTUNISTIC_PRUNE_ROWS).await
    {
        tracing::warn!(error = %e, "couldn't clean up web sessions");
    }
    let mut response = redirect("/web");
    response.headers_mut().append(SET_COOKIE, set_cookie(token)?);
    Ok(response)
}

/// `POST /web/login`: username and password. The same checks as the JSON sign-in, in the same order.
async fn login(State(state): State<Shared>, ConnectInfo(peer): ConnectInfo<SocketAddr>, request: Request) -> Result<Response, WebError> {
    let settings = state.web.clone().ok_or_else(|| WebError::not_found("page"))?;
    if request.headers().contains_key(AUTHORIZATION) && has_session_cookie(request.headers()) {
        return Err(WebError::invalid("Send either a bearer token or the session cookie, not both."));
    }
    let headers = request.headers().clone();
    let form: LoginForm = read_form(&state, request).await?;
    if !valid_username(&form.username) {
        return Ok(login_answer(StatusCode::BAD_REQUEST, "Usernames are 1-64 letters, digits, '.', '_' or '-'.", None));
    }
    if form.password.len() > MAX_PASSWORD_BYTES {
        return Ok(login_answer(StatusCode::BAD_REQUEST, "Passwords are at most 1024 bytes.", None));
    }
    // tokio's clock: the system's monotonic clock in the server, the simulated one in a simulation.
    let now = tokio::time::Instant::now().into_std();
    let ip = client_ip(peer, &headers, &state.trusted_proxies);
    let username = form.username.to_ascii_lowercase();
    // As in the JSON sign-in: the slot first, so an answer that checked nothing costs the client nothing, then the counts.
    let Ok(permit) = state.password_checks.clone().try_acquire_owned() else {
        return Ok(login_answer(
            StatusCode::TOO_MANY_REQUESTS,
            "The server is busy checking other sign-ins. Try again in a moment.",
            Some(PASSWORD_RETRY_SECS),
        ));
    };
    let (ip_name, user_name) = (ip_key(ip), format!("user:{username}"));
    if !state.web_logins.allow(&ip_name, LOGINS_PER_IP, now) || !state.web_logins.allow(&user_name, LOGINS_PER_USER, now) {
        let wait = retry_secs(&[(&state.web_logins, &ip_name), (&state.web_logins, &user_name)], now).unwrap_or(60);
        return Ok(login_answer(StatusCode::TOO_MANY_REQUESTS, "Too many sign-in attempts. Wait a few minutes.", Some(wait)));
    }
    let user = state.meta.user_by_name(&username).await?;
    let known = user.as_ref().map(|u| u.id);
    // An account without a password (an open invite) costs the same Argon2id check as a name nobody has, and never signs in.
    let usable = user.as_ref().is_some_and(|u| crate::auth::usable_hash(&u.password_hash));
    let hash = user.as_ref().filter(|_| usable).map_or(state.dummy_hash.as_str(), |u| u.password_hash.as_str()).to_owned();
    let password = form.password;
    let ok = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        verify_password(&password, &hash)
    })
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "password check task failed");
        ApiError::internal()
    })?;
    let (Some(user), true) = (user, ok && usable) else {
        audit::web_login_failed(&state, known).await;
        return Ok(login_answer(StatusCode::UNAUTHORIZED, WRONG_LOGIN, None));
    };
    let (token, id_hash) = new_token()?;
    let (challenge_token, challenge_hash) = new_token()?;
    let challenge = NewChallenge { id_hash: challenge_hash, client_key: ip_key(ip) };
    let session = NewWebSession { id_hash, absolute_ms: settings.absolute_ms };
    // A member, a disabled account and a password that changed meanwhile are all `None`: answered like a wrong password.
    match state.meta.web_sign_in(user.id, (&user.password_hash, user.auth_gen), &challenge, &session, state.now_ms()).await? {
        None => {
            audit::web_login_failed(&state, known).await;
            Ok(login_answer(StatusCode::UNAUTHORIZED, WRONG_LOGIN, None))
        }
        Some(WebSignIn::Session) => {
            tracing::info!("web admin signed in");
            start_session(&state, &token).await
        }
        Some(WebSignIn::Challenge { .. }) => {
            tracing::info!("web admin sign-in needs a second factor");
            Ok(second_factor_answer(StatusCode::OK, &challenge_token, None, None))
        }
    }
}

/// `POST /web/login/second-factor`: the challenge the first step made and a code or recovery code. The JSON second step's
/// counters, limits and lock, and its single answer for every challenge that no longer works.
async fn second_factor(State(state): State<Shared>, ConnectInfo(peer): ConnectInfo<SocketAddr>, request: Request) -> Result<Response, WebError> {
    let settings = state.web.clone().ok_or_else(|| WebError::not_found("page"))?;
    if request.headers().contains_key(AUTHORIZATION) && has_session_cookie(request.headers()) {
        return Err(WebError::invalid("Send either a bearer token or the session cookie, not both."));
    }
    let headers = request.headers().clone();
    let form: SecondFactorForm = read_form(&state, request).await?;
    let nonempty = |text: String| if text.is_empty() { None } else { Some(text) };
    let challenge_text = form.challenge.clone();
    let again = |status: StatusCode, message: &str, retry: Option<u32>| second_factor_answer(status, &challenge_text, Some(message), retry);
    let proof = match parse_proof(nonempty(form.totp_code), nonempty(form.recovery_code)) {
        Ok(Some(proof)) => proof,
        Ok(None) => return Ok(again(StatusCode::BAD_REQUEST, "Enter the code from your authenticator app, or a recovery code.", None)),
        Err(e) => return Ok(again(StatusCode::BAD_REQUEST, &e.message, None)),
    };
    let now = tokio::time::Instant::now().into_std();
    let now_ms = state.now_ms();
    let ip = ip_key(client_ip(peer, &headers, &state.trusted_proxies));
    let gone = || login_answer(StatusCode::UNAUTHORIZED, "This sign-in step doesn't work any more. Sign in again.", None);
    let Some(ip_counted) = state.second_factors.count(&ip, SECOND_FACTORS_PER_IP, now) else {
        let wait = retry_secs(&[(&state.second_factors, &ip)], now).unwrap_or(60);
        return Ok(again(StatusCode::TOO_MANY_REQUESTS, "Too many wrong codes from this address. Wait a few minutes.", Some(wait)));
    };
    let Some(hash) = challenge_hash(&form.challenge) else {
        audit::web_challenge_failed(&state, None).await;
        return Ok(gone());
    };
    let Some((user, username)) = state.meta.web_challenge_owner(&hash, now_ms).await? else {
        audit::web_challenge_failed(&state, None).await;
        return Ok(gone());
    };
    let user_key = format!("user:{username}");
    let Some(user_counted) = state.second_factors.count(&user_key, SECOND_FACTORS_PER_USER, now) else {
        state.second_factors.refund(&ip, ip_counted);
        if let Some(locked_ms) = state.meta.two_factor_locked_for(user, now_ms).await? {
            return Ok(again(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many wrong codes: this account's second factor is locked for a while.",
                Some(craft_cloud_meta::retry_after_secs(locked_ms)),
            ));
        }
        let wait = retry_secs(&[(&state.second_factors, &user_key)], now).unwrap_or(60);
        return Ok(again(StatusCode::TOO_MANY_REQUESTS, "Too many wrong codes for this account. Wait a few minutes.", Some(wait)));
    };
    let (token, id_hash) = new_token()?;
    let session = NewWebSession { id_hash, absolute_ms: settings.absolute_ms };
    let prepared = proof.prepare(&state, now_ms);
    match state.meta.web_complete_second_factor(&hash, &prepared.proof(), &session, now_ms).await {
        Ok(_) => {
            state.second_factors.refund(&ip, ip_counted);
            state.second_factors.refund(&user_key, user_counted);
            tracing::info!("web admin signed in with a second factor");
            start_session(&state, &token).await
        }
        Err(MetaError::TwoFactorInvalid) => {
            // A wrong second factor counts against sign-in too.
            let _ = state.web_logins.allow(&ip, LOGINS_PER_IP, now);
            let _ = state.web_logins.allow(&user_key, LOGINS_PER_USER, now);
            // A wrong second factor is a refusal of someone who is signed in as far as the password goes: 403, as on every route
            // that asks for a code. An unknown or used-up challenge stays 401.
            Ok(again(StatusCode::FORBIDDEN, "The code is wrong, or was used already.", None))
        }
        Err(MetaError::TwoFactorLocked { retry_after_ms }) => {
            state.second_factors.refund(&ip, ip_counted);
            state.second_factors.refund(&user_key, user_counted);
            Ok(again(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many wrong codes: this account's second factor is locked for a while.",
                Some(craft_cloud_meta::retry_after_secs(retry_after_ms)),
            ))
        }
        Err(MetaError::ChallengeGone) => {
            audit::web_challenge_failed(&state, Some(user)).await;
            Ok(gone())
        }
        Err(e) => Err(e.into()),
    }
}

/// `POST /web/logout`: ends the session and drops the cookie. Someone who is signed out already is sent to the sign-in page.
async fn logout(State(state): State<Shared>, request: Request) -> Result<Response, WebError> {
    let authed = match authenticate(&state, request.headers()).await {
        Ok(authed) => authed,
        Err(e) if e.status == StatusCode::UNAUTHORIZED => return Ok(e.for_get().into_response()),
        Err(e) => return Err(e),
    };
    let form: CsrfForm = read_form(&state, request).await?;
    authed.check_csrf(&form.csrf)?;
    state.meta.web_logout(&authed.hash, state.now_ms()).await?;
    tracing::info!("web admin signed out");
    let mut response = redirect("/web/login?notice=signed_out");
    response.headers_mut().append(SET_COOKIE, clear_cookie());
    Ok(response)
}

async fn stylesheet() -> Response {
    let mut response = Response::new(Body::from(include_str!("style.css")));
    response.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("text/css; charset=utf-8"));
    response.headers_mut().insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

async fn no_such_page() -> WebError {
    WebError::not_found("page")
}

async fn wrong_method() -> WebError {
    WebError::new(StatusCode::METHOD_NOT_ALLOWED, "This page does not take that kind of request.")
}

async fn slash(method: Method) -> Response {
    if matches!(method, Method::GET | Method::HEAD) { redirect("/web") } else { wrong_method().await.into_response() }
}

/// The pages, to be nested under `/web`. They are mounted only when the server is set up for them ([`AppState::web`]).
pub fn routes(state: Shared) -> Router<Shared> {
    Router::new()
        .route("/", get(pages::overview))
        .route("/login", get(login_page).post(login))
        .route("/login/second-factor", post(second_factor))
        .route("/logout", post(logout))
        .route("/style.css", get(stylesheet))
        .route("/users", get(pages::users).post(pages::create_user))
        .route("/users/{id}", get(pages::user))
        .route("/users/{id}/update", post(pages::update_user))
        .route("/users/{id}/disable", post(pages::disable_user))
        .route("/users/{id}/enable", post(pages::enable_user))
        .route("/users/{id}/remove", post(pages::remove_user))
        .route("/users/{id}/invite", post(pages::issue_invite))
        .route("/users/{id}/invite/revoke", post(pages::revoke_invite))
        .route("/users/{id}/password-reset", post(pages::issue_reset))
        .route("/users/{id}/reset-two-factor", post(pages::reset_two_factor))
        .route("/users/{id}/devices/{device}/revoke", post(pages::revoke_device))
        .route("/spaces", get(team::spaces).post(team::create_space))
        .route("/spaces/{id}", get(team::space))
        .route("/spaces/{id}/quota", post(team::set_quota))
        .route("/spaces/{id}/archive", post(team::archive))
        .route("/spaces/{id}/unarchive", post(team::unarchive))
        .route("/spaces/{id}/owner", post(team::set_owner))
        .route("/audit", get(pages::audit))
        .route("/audit/export", get(pages::audit_export))
        .fallback(no_such_page)
        .method_not_allowed_fallback(wrong_method)
        .layer(DefaultBodyLimit::max(TINY_JSON_BYTES))
        .layer(middleware::from_fn_with_state(state, same_origin_posts))
        .layer(middleware::from_fn(secure_headers))
}

/// `/web/` is `/web`. It has the same protective headers and origin check as every other path under `/web`, and answers a
/// method it does not take like the pages do.
pub fn trailing_slash(state: Shared) -> Router<Shared> {
    Router::new()
        .route("/web/", any(slash))
        .layer(middleware::from_fn_with_state(state, same_origin_posts))
        .layer(middleware::from_fn(secure_headers))
}

/// How long a refused caller should wait: the longest end of the windows of `keys`, rounded up, at least a second. `None` when
/// none of them has a window running.
pub(crate) fn retry_secs(keys: &[(&crate::ratelimit::Limiter, &str)], now: std::time::Instant) -> Option<u32> {
    let longest = keys.iter().map(|(limiter, key)| limiter.retry_after(key, now)).max()?;
    (!longest.is_zero()).then(|| u32::try_from(longest.as_secs().saturating_add(1)).unwrap_or(u32::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> WebSettings {
        WebSettings::new("https://Cloud.Example.com", 1, 2).unwrap()
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(HeaderName::from_bytes(name.as_bytes()).unwrap(), HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn the_origin_is_the_public_url_without_its_default_port() {
        assert_eq!(settings().origin(), "https://cloud.example.com");
        assert_eq!(WebSettings::new("https://cloud.example.com:443/", 1, 2).unwrap().origin(), "https://cloud.example.com");
        assert_eq!(WebSettings::new("https://cloud.example.com:8443", 1, 2).unwrap().origin(), "https://cloud.example.com:8443");
        for bad in ["http://cloud.example.com", "https://", "https://x/path", "cloud.example.com", ""] {
            assert!(WebSettings::new(bad, 1, 2).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_form_must_come_from_the_origin() {
        let s = settings();
        assert!(s.allows(&headers(&[("origin", "https://cloud.example.com")])));
        for origin in [
            "https://evil.example",
            "http://cloud.example.com",
            "https://cloud.example.com:8443",
            "null",
            "https://cloud.example.com/",
            "https://cloud.example.com.evil.example",
            "",
        ] {
            assert!(!s.allows(&headers(&[("origin", origin)])), "{origin:?}");
        }
        assert!(!s.allows(&headers(&[])), "neither header");
        assert!(!s.allows(&headers(&[("origin", "https://cloud.example.com"), ("origin", "https://cloud.example.com")])), "two origins");
        // Without an Origin, a Referer on the origin does; an Origin that is wrong is not rescued by a good Referer.
        assert!(s.allows(&headers(&[("referer", "https://cloud.example.com/web/users?x=1")])));
        assert!(s.allows(&headers(&[("referer", "https://cloud.example.com")])));
        assert!(!s.allows(&headers(&[("referer", "https://cloud.example.com.evil.example/web")])));
        assert!(!s.allows(&headers(&[("referer", "https://evil.example/https://cloud.example.com")])));
        assert!(!s.allows(&headers(&[("origin", "https://evil.example"), ("referer", "https://cloud.example.com/web")])));
    }

    #[test]
    fn only_one_well_shaped_cookie_is_a_session() {
        let token = "a".repeat(43);
        let one = headers(&[("cookie", &format!("theme=dark; {COOKIE_NAME}={token}; other=1"))]);
        assert_eq!(session_token(&one), Some(token.as_str()));
        assert!(has_session_cookie(&one));
        assert_eq!(session_token(&headers(&[("cookie", &format!("{COOKIE_NAME}={token}; {COOKIE_NAME}={token}"))])), None, "two of the name");
        assert_eq!(session_token(&headers(&[("cookie", &format!("{COOKIE_NAME}={token}")), ("cookie", &format!("{COOKIE_NAME}={token}"))])), None);
        assert_eq!(session_token(&headers(&[("cookie", &format!("{COOKIE_NAME}=short"))])), None, "a wrong shape");
        assert!(has_session_cookie(&headers(&[("cookie", &format!("{COOKIE_NAME}=short"))])), "but it is there");
        assert_eq!(session_token(&headers(&[("cookie", &format!("x{COOKIE_NAME}={token}"))])), None, "another name");
        assert!(!has_session_cookie(&headers(&[("cookie", "a=b")])));
    }

    #[test]
    fn the_csrf_token_is_bound_to_the_session_and_not_its_hash() {
        let (a, b) = ("a".repeat(43), "b".repeat(43));
        assert_eq!(csrf_token(&a), csrf_token(&a));
        assert_ne!(csrf_token(&a), csrf_token(&b));
        assert_eq!(csrf_token(&a).len(), 43);
        let stored = URL_SAFE_NO_PAD.encode(token_hash(&a));
        assert_ne!(csrf_token(&a), stored, "the stored hash is not the token");
        assert!(csrf_matches(&csrf_token(&a), &csrf_token(&a)));
        assert!(!csrf_matches(&csrf_token(&a), &csrf_token(&b)));
        assert!(!csrf_matches(&csrf_token(&a), ""));
        assert!(!csrf_matches(&csrf_token(&a), &csrf_token(&a)[1..]));
    }

    #[test]
    fn forms_with_secrets_print_nothing() {
        let form = LoginForm { username: "ana".into(), password: "hunter2hunter2".into() };
        let shown = format!("{form:?}");
        assert!(!shown.contains("hunter2") && !shown.contains("ana"), "{shown}");
        let second = SecondFactorForm { challenge: "challenge-secret".into(), totp_code: "123456".into(), recovery_code: "ABCD".into() };
        let shown = format!("{second:?}");
        assert!(!shown.contains("challenge-secret") && !shown.contains("123456") && !shown.contains("ABCD"), "{shown}");
    }
}
