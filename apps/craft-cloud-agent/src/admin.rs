//! The local API's admin calls (`/v1/admin/…`): the server's admin API (`/api/v1/admin/…`, `craft-cloud-server`'s
//! `admin.rs`) for the Craft Cloud app's admin pages, passed on with the signed-in device's token.
//!
//! - **Nothing is decided here.** The agent checks no role and keeps no confirmation: the acting admin's password
//!   travels in the body of each change, from the app through this agent to the server, and is dropped with the
//!   request. Bodies are read as the shared types (so the `Debug` output of every one hides the password), at most
//!   [`MAX_ADMIN_JSON_BYTES`] long, and written to the server again; a field this agent doesn't know is dropped.
//!   Nothing of a body is logged.
//! - **The server's answer is the answer**: the same body and, for a refusal, the same HTTP status, error code and
//!   message, and `Retry-After` for a `429`. A `403` (`wrong_password`, `forbidden`, `account_disabled`) stays a `403`,
//!   a `409` (`last_admin`) a `409`. The one exception is the server's `401`: for a signed-in route it means this
//!   device was revoked, and is told as every other call tells it (`409 needs_you`), not as `401`, which on the local
//!   API means a wrong token.
//! - The same `Host`, `Origin` and token checks as every local call (`local_api::guard`) run first.

use axum::body::Body;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use craft_cloud_proto::DeviceId;
use craft_cloud_proto::api::{
    AccountInfo, AccountUsage, AdminCode, AdminCompression, AdminConfirm, AdminCreateUser, AdminCreatedUser, AdminDevices, AdminPasswordReset,
    AdminServerInfo, AdminSetCompressionLevel, AdminStorageOverview, AdminUpdateUser, AdminUserInfo, AdminUserList, AuditPage, codes as api_codes,
    valid_audit_action,
};
use craft_cloud_proto::local::{MAX_ADMIN_JSON_BYTES, paths};
use reqwest::Method;
use serde::Deserialize;
use uuid::Uuid;

use crate::cli::AgentError;
use crate::http::{CallError, MAX_DEVICES};
use crate::local_api::{Api, LocalError, path_id, server_api};

/// Most accounts one page of the user list may hold (the server's own limit; a longer page is refused).
const MAX_PAGE_USERS: usize = 200;

impl From<CallError> for LocalError {
    /// A refusal keeps the server's status, code and message and its `Retry-After` (never a 401: that is a revoked device,
    /// told as every signed-in call tells it); anything else is what the sync paths make of it.
    fn from(e: CallError) -> Self {
        AgentError::from(e).into()
    }
}

type Answer<T> = Result<T, LocalError>;

/// The admin calls, to be merged into the local API's router (behind its `guard`).
pub(crate) fn routes() -> Router<Api> {
    let small = || DefaultBodyLimit::max(MAX_ADMIN_JSON_BYTES);
    let user = format!("{}/{{id}}", paths::ADMIN_USERS);
    Router::new()
        .route(paths::ACCOUNT, get(account))
        .route(paths::ACCOUNT_ACTIVITY, get(activity))
        .route(paths::ADMIN_AUDIT, get(audit))
        .route(paths::ADMIN_AUDIT_EXPORT, get(audit_export))
        .route(paths::ADMIN_SERVER, get(server))
        .route(paths::ADMIN_STORAGE, get(storage))
        .route(paths::ADMIN_STORAGE_COMPRESSION, axum::routing::put(set_compression).layer(small()))
        .route(paths::ADMIN_USERS, get(list_users).post(create_user).layer(small()))
        .route(&user, axum::routing::patch(update_user).delete(remove_user).layer(small()))
        .route(&format!("{user}/usage"), get(user_usage))
        .route(&format!("{user}/disable"), post(disable_user).layer(small()))
        .route(&format!("{user}/enable"), post(enable_user).layer(small()))
        .route(&format!("{user}/invite"), post(issue_invite).delete(revoke_invite).layer(small()))
        .route(&format!("{user}/password-reset"), post(issue_reset).layer(small()))
        .route(&format!("{user}/reset-two-factor"), post(reset_two_factor).layer(small()))
        .route(&format!("{user}/devices"), get(list_devices))
        .route(&format!("{user}/devices/{{device_id}}"), axum::routing::delete(revoke_device).layer(small()))
}

async fn server(State(state): State<Api>) -> Answer<Json<AdminServerInfo>> {
    let (_, transport) = server_api(&state)?;
    Ok(Json(transport.admin_get("admin/server", &[]).await?))
}

async fn storage(State(state): State<Api>) -> Answer<Json<AdminStorageOverview>> {
    let (_, transport) = server_api(&state)?;
    Ok(Json(transport.admin_get("admin/storage", &[]).await?))
}

async fn set_compression(State(state): State<Api>, body: Result<Json<AdminSetCompressionLevel>, JsonRejection>) -> Answer<Json<AdminCompression>> {
    let Json(body) = body?;
    let (_, transport) = server_api(&state)?;
    Ok(Json(transport.admin_json(Method::PUT, "admin/storage/compression", &body).await?))
}

async fn user_usage(State(state): State<Api>, id: Result<Path<Uuid>, PathRejection>) -> Answer<Json<AccountUsage>> {
    let id = path_id(id)?;
    let (_, transport) = server_api(&state)?;
    Ok(Json(transport.admin_get(&format!("admin/users/{id}/usage"), &[]).await?))
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    cursor: Option<String>,
    limit: Option<u32>,
    query: Option<String>,
}

async fn list_users(State(state): State<Api>, q: Result<Query<ListQuery>, QueryRejection>) -> Answer<Json<AdminUserList>> {
    let Query(q) = q.map_err(|e| LocalError::new(StatusCode::BAD_REQUEST, api_codes::INVALID, e.body_text()))?;
    let (_, transport) = server_api(&state)?;
    let limit = q.limit.map(|l| l.to_string());
    let mut pairs: Vec<(&str, &str)> = Vec::new();
    if let Some(cursor) = q.cursor.as_deref().filter(|c| !c.is_empty()) {
        pairs.push(("cursor", cursor));
    }
    if let Some(limit) = limit.as_deref() {
        pairs.push(("limit", limit));
    }
    if let Some(query) = q.query.as_deref().filter(|s| !s.is_empty()) {
        pairs.push(("query", query));
    }
    let list: AdminUserList = transport.admin_get("admin/users", &pairs).await?;
    if list.users.len() > MAX_PAGE_USERS {
        return Err(AgentError::from(craft_cloud_sync::TransportError::Protocol(format!(
            "the server listed {} accounts on a page, more than the {MAX_PAGE_USERS} this agent reads",
            list.users.len()
        )))
        .into());
    }
    Ok(Json(list))
}

async fn create_user(State(state): State<Api>, body: Result<Json<AdminCreateUser>, JsonRejection>) -> Answer<Json<AdminCreatedUser>> {
    let Json(body) = body?;
    let (_, transport) = server_api(&state)?;
    Ok(Json(transport.admin_json(Method::POST, "admin/users", &body).await?))
}

async fn update_user(
    State(state): State<Api>,
    id: Result<Path<Uuid>, PathRejection>,
    body: Result<Json<AdminUpdateUser>, JsonRejection>,
) -> Answer<Json<AdminUserInfo>> {
    let id = path_id(id)?;
    let Json(body) = body?;
    let (_, transport) = server_api(&state)?;
    Ok(Json(transport.admin_json(Method::PATCH, &format!("admin/users/{id}"), &body).await?))
}

/// A change with only the confirmation as its body, answered without one: the server's own status (`204`, or `202`).
async fn confirmed(state: &Api, method: Method, path: String, body: Result<Json<AdminConfirm>, JsonRejection>) -> Answer<StatusCode> {
    let Json(body) = body?;
    let (_, transport) = server_api(state)?;
    let status = transport.admin_empty(method, &path, &body).await?;
    Ok(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::NO_CONTENT))
}

async fn disable_user(
    State(state): State<Api>,
    id: Result<Path<Uuid>, PathRejection>,
    body: Result<Json<AdminConfirm>, JsonRejection>,
) -> Answer<StatusCode> {
    confirmed(&state, Method::POST, format!("admin/users/{}/disable", path_id(id)?), body).await
}

async fn enable_user(
    State(state): State<Api>,
    id: Result<Path<Uuid>, PathRejection>,
    body: Result<Json<AdminConfirm>, JsonRejection>,
) -> Answer<StatusCode> {
    confirmed(&state, Method::POST, format!("admin/users/{}/enable", path_id(id)?), body).await
}

async fn remove_user(
    State(state): State<Api>,
    id: Result<Path<Uuid>, PathRejection>,
    body: Result<Json<AdminConfirm>, JsonRejection>,
) -> Answer<StatusCode> {
    confirmed(&state, Method::DELETE, format!("admin/users/{}", path_id(id)?), body).await
}

async fn revoke_invite(
    State(state): State<Api>,
    id: Result<Path<Uuid>, PathRejection>,
    body: Result<Json<AdminConfirm>, JsonRejection>,
) -> Answer<StatusCode> {
    confirmed(&state, Method::DELETE, format!("admin/users/{}/invite", path_id(id)?), body).await
}

async fn revoke_device(
    State(state): State<Api>,
    ids: Result<Path<(Uuid, DeviceId)>, PathRejection>,
    body: Result<Json<AdminConfirm>, JsonRejection>,
) -> Answer<StatusCode> {
    let (user, device) = path_id(ids)?;
    confirmed(&state, Method::DELETE, format!("admin/users/{user}/devices/{device}"), body).await
}

/// A new invite: shown to the admin once, by the app; the agent keeps nothing of it and says nothing of it in its log.
async fn issue_invite(
    State(state): State<Api>,
    id: Result<Path<Uuid>, PathRejection>,
    body: Result<Json<AdminConfirm>, JsonRejection>,
) -> Answer<Json<AdminCode>> {
    let Json(body) = body?;
    let path = format!("admin/users/{}/invite", path_id(id)?);
    let (_, transport) = server_api(&state)?;
    Ok(Json(transport.admin_json(Method::POST, &path, &body).await?))
}

/// A password reset code, with the confirmation and whether redeeming it also turns the account's two-factor sign-in off
/// ([`AdminPasswordReset::reset_two_factor`]): shown once, kept and logged by nobody.
async fn issue_reset(
    State(state): State<Api>,
    id: Result<Path<Uuid>, PathRejection>,
    body: Result<Json<AdminPasswordReset>, JsonRejection>,
) -> Answer<Json<AdminCode>> {
    let Json(body) = body?;
    let path = format!("admin/users/{}/password-reset", path_id(id)?);
    let (_, transport) = server_api(&state)?;
    Ok(Json(transport.admin_json(Method::POST, &path, &body).await?))
}

async fn reset_two_factor(
    State(state): State<Api>,
    id: Result<Path<Uuid>, PathRejection>,
    body: Result<Json<AdminConfirm>, JsonRejection>,
) -> Answer<StatusCode> {
    confirmed(&state, Method::POST, format!("admin/users/{}/reset-two-factor", path_id(id)?), body).await
}

async fn list_devices(State(state): State<Api>, id: Result<Path<Uuid>, PathRejection>) -> Answer<Json<AdminDevices>> {
    let id = path_id(id)?;
    let (_, transport) = server_api(&state)?;
    let list: AdminDevices = transport.admin_get(&format!("admin/users/{id}/devices"), &[]).await?;
    if list.devices.len() > MAX_DEVICES {
        return Err(AgentError::from(craft_cloud_sync::TransportError::Protocol(format!(
            "the server listed {} devices, more than the {MAX_DEVICES} this agent reads",
            list.devices.len()
        )))
        .into());
    }
    Ok(Json(list))
}

// ---- the account and the audit log ----

/// The most events a page of the account's own activity holds, and of the admin's audit list (the server's limits).
const ACCOUNT_MAX_PAGE: u32 = 100;
const ADMIN_MAX_PAGE: u32 = 200;
/// The longest cursor passed on, in bytes (the server's are a few dozen).
const MAX_CURSOR_BYTES: usize = 256;

/// `GET /v1/account`: the signed-in account as the server says it (its role, and whether it has two-factor sign-in).
async fn account(State(state): State<Api>) -> Answer<Json<AccountInfo>> {
    let (_, transport) = server_api(&state)?;
    Ok(Json(transport.account_info().await?))
}

/// The query of the audit routes. Unknown parameters are ignored.
#[derive(Debug, Deserialize)]
struct AuditParams {
    cursor: Option<String>,
    before_ms: Option<i64>,
    limit: Option<u32>,
    user: Option<String>,
    action: Option<String>,
}

fn invalid(message: &str) -> LocalError {
    LocalError::new(StatusCode::BAD_REQUEST, api_codes::INVALID, message)
}

/// The query as pairs for the server, after checking what is checked here (the server checks it all again): a cursor is passed
/// on as it came, but bounded and made of the characters of the server's own; `user` is an account ID, `action` one of the
/// closed list or a prefix of it, `before_ms` a time, `limit` 1 to `max_limit`. `admin`: whether `user` and `action` are read
/// (an account's own activity has neither).
fn audit_pairs(params: &AuditParams, max_limit: u32, admin: bool, list: bool) -> Result<Vec<(&'static str, String)>, LocalError> {
    let mut pairs = Vec::new();
    if let Some(cursor) = params.cursor.as_deref().filter(|c| !c.is_empty()) {
        if !list || cursor.len() > MAX_CURSOR_BYTES || !cursor.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')) {
            return Err(invalid("the cursor is not one this server gave"));
        }
        pairs.push(("cursor", cursor.to_owned()));
    }
    if let Some(before) = params.before_ms {
        if before < 0 {
            return Err(invalid("before_ms is a time in milliseconds"));
        }
        pairs.push(("before_ms", before.to_string()));
    }
    if let Some(limit) = params.limit {
        if !list || !(1..=max_limit).contains(&limit) {
            return Err(invalid(&format!("limit is 1 to {max_limit}")));
        }
        pairs.push(("limit", limit.to_string()));
    }
    if admin {
        if let Some(user) = params.user.as_deref().filter(|u| !u.is_empty()) {
            let id = Uuid::parse_str(user).map_err(|_| invalid("user is an account ID"))?;
            pairs.push(("user", id.to_string()));
        }
        if let Some(action) = params.action.as_deref().filter(|a| !a.is_empty()) {
            if !valid_audit_action(action) {
                return Err(invalid("action is an event name like auth.login, or a prefix like auth."));
            }
            pairs.push(("action", action.to_owned()));
        }
    }
    Ok(pairs)
}

fn as_refs<'a>(pairs: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    pairs.iter().map(|(k, v)| (*k, v.as_str())).collect()
}

/// `GET /v1/account/activity?cursor&before_ms&limit`: the account's own security events (any account; `user` and `action` are
/// not read).
async fn activity(State(state): State<Api>, params: Result<Query<AuditParams>, QueryRejection>) -> Answer<Json<AuditPage>> {
    let Query(params) = params.map_err(|e| invalid(&e.body_text()))?;
    let pairs = audit_pairs(&params, ACCOUNT_MAX_PAGE, false, true)?;
    let (_, transport) = server_api(&state)?;
    Ok(Json(transport.admin_get("account/activity", &as_refs(&pairs)).await?))
}

/// `GET /v1/admin/audit?cursor&before_ms&limit&user&action`: the whole log, filtered; the server refuses a member.
async fn audit(State(state): State<Api>, params: Result<Query<AuditParams>, QueryRejection>) -> Answer<Json<AuditPage>> {
    let Query(params) = params.map_err(|e| invalid(&e.body_text()))?;
    let pairs = audit_pairs(&params, ADMIN_MAX_PAGE, true, true)?;
    let (_, transport) = server_api(&state)?;
    Ok(Json(transport.admin_get("admin/audit", &as_refs(&pairs)).await?))
}

/// `GET /v1/admin/audit/export?before_ms&user&action`: the server's NDJSON export passed through as it comes, chunk by chunk:
/// the agent never holds more than the chunk in hand, and a connection that breaks in the middle ends this one without its
/// last chunk, so the client sees a broken stream. The last line (`complete` or `truncated`) is the server's.
async fn audit_export(State(state): State<Api>, params: Result<Query<AuditParams>, QueryRejection>) -> Answer<Response> {
    let Query(params) = params.map_err(|e| invalid(&e.body_text()))?;
    let pairs = audit_pairs(&params, ADMIN_MAX_PAGE, true, false)?;
    let (_, transport) = server_api(&state)?;
    let answer = transport.admin_export_stream(&as_refs(&pairs)).await?;
    let stream = futures_util::stream::unfold(Some(answer.body), |body| async move {
        let mut body = body?;
        match body.next().await {
            Ok(Some(bytes)) => Some((Ok::<_, std::io::Error>(bytes), Some(body))),
            Ok(None) => None,
            // The error ends the stream with it: the connection breaks, and nothing is made up for the missing end.
            Err(e) => Some((Err(std::io::Error::other(e.to_string())), None)),
        }
    });
    let mut response = Response::new(Body::from_stream(stream));
    let headers = response.headers_mut();
    headers.insert(axum::http::header::CONTENT_TYPE, axum::http::HeaderValue::from_static("application/x-ndjson"));
    headers.insert(axum::http::header::CACHE_CONTROL, axum::http::HeaderValue::from_static("no-store"));
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Refusal;
    use axum::response::IntoResponse;
    use craft_cloud_proto::local::codes;
    use craft_cloud_sync::TransportError;

    fn refused(status: u16, code: &str, retry: Option<u64>) -> LocalError {
        LocalError::from(CallError::Refused(Refusal { status, error: code.into(), message: "m".into(), two_factor: None, retry_after_secs: retry }))
    }

    #[test]
    fn a_refusal_keeps_its_status_code_and_retry_after() {
        let wrong = refused(403, "wrong_password", None);
        assert_eq!((wrong.status, wrong.code.as_str()), (StatusCode::FORBIDDEN, "wrong_password"));
        let invalid = refused(403, "two_factor_invalid", None);
        assert_eq!((invalid.status, invalid.code.as_str()), (StatusCode::FORBIDDEN, "two_factor_invalid"));
        let last = refused(409, "last_admin", None);
        assert_eq!((last.status, last.code.as_str()), (StatusCode::CONFLICT, "last_admin"));
        let locked = refused(429, "two_factor_locked", Some(30)).into_response();
        assert_eq!(locked.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(locked.headers().get(axum::http::header::RETRY_AFTER).and_then(|v| v.to_str().ok()), Some("30"));
    }

    /// The server's `401` (this device was revoked) is never passed on as a `401`: on the local API that says the
    /// token is wrong.
    #[test]
    fn a_revoked_device_is_not_a_wrong_token() {
        let failure = LocalError::from(CallError::Transport(TransportError::Unauthorized));
        assert_ne!(failure.status, StatusCode::UNAUTHORIZED);
        assert_eq!(failure.code, codes::NEEDS_YOU);
    }
}
