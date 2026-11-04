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

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use craft_cloud_proto::DeviceId;
use craft_cloud_proto::api::{
    AdminCode, AdminConfirm, AdminCreateUser, AdminCreatedUser, AdminDevices, AdminPasswordReset, AdminServerInfo, AdminUpdateUser, AdminUserInfo,
    AdminUserList, codes as api_codes,
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
        .route(paths::ADMIN_SERVER, get(server))
        .route(paths::ADMIN_USERS, get(list_users).post(create_user).layer(small()))
        .route(&user, axum::routing::patch(update_user).delete(remove_user).layer(small()))
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
