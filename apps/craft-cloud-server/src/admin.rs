//! The admin API, under `/api/v1/admin`: server counters, accounts (list, create, change role and quota, disable,
//! enable, mark for removal), invites, password reset codes, and a user's devices.
//!
//! - **Every route is behind [`require_admin`](crate::auth::require_admin)** (after `require_device`): a member gets `403
//!   forbidden`, a bad token `401`. The role is read from the database for each request, so a demoted admin is out with
//!   the next one.
//! - **Every change needs a fresh confirmation in its body**: the acting admin's current `password`. A device token never
//!   expires, so a stolen admin token alone must not be enough to make an account an admin's or to sign others out. A
//!   missing or mistyped field is `400 invalid`, a wrong password `403 wrong_password` (never 401: on a signed-in route
//!   the agent reads 401 as "this device was revoked"). Tries are counted per account in
//!   [`AppState::password_changes`], the same window the password change uses, but only a wrong password stays counted;
//!   every check holds a server-wide password-check slot. An admin whose account has two-factor sign-in adds `totp_code`
//!   (or `recovery_code`): missing or wrong is `403 two_factor_invalid`, too many wrong ones `429 two_factor_locked`. The
//!   code is used up by the confirmation (one time step serves one change, as at sign-in), and the check shares its
//!   counters and limiter with sign-in.
//! - **Bodies are tiny** ([`TINY_JSON_BYTES`]), the `DELETE` routes carry one too, and are read under the same deadline
//!   as every JSON body.
//! - **Nothing secret is logged**: not a password, a code or a join link. Events are logged at `info` with the kind and
//!   the target's ID, never a username.
//! - **Live connections** of devices an admin signs out end at once in this process (after the commit); another server
//!   process, or the command line, finds out at the next ping (about 30 seconds).

use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, Extension, FromRequest, Path, Query, Request, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Router, middleware};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use craft_cloud_meta::{AdminActor, AdminUser, AdminVia, DeviceAuth, Role, UserId, valid_username};
use craft_cloud_proto::DeviceId;
use craft_cloud_proto::api::{
    AccountUsage, AdminCode, AdminConfirm, AdminCreateUser, AdminCreatedUser, AdminDevices, AdminManifestIndex, AdminPasswordReset, AdminServerInfo,
    AdminStorageAccount, AdminStorageOverview, AdminUpdateUser, AdminUserInfo, AdminUserList, UsageEstimateReason, UsageEstimates,
};
use craft_cloud_proto::join::{Code, INVITE_CHARS, INVITE_TTL_MS, JoinString};
use serde::Deserialize;
use uuid::Uuid;

use crate::account::confirm_password;
use crate::api::{TINY_JSON_BYTES, body_too_slow, device_info};
use crate::auth::require_admin;
use crate::error::ApiError;
use crate::state::AppState;

type Shared = Arc<AppState>;

/// Accounts one page of `GET /admin/users` holds when `limit` is not given.
pub(crate) const DEFAULT_PAGE: u32 = 50;
/// The most accounts a page may hold.
pub(crate) const MAX_PAGE: u32 = 200;
/// The longest `query` of the user list, in bytes.
pub(crate) const MAX_QUERY_BYTES: usize = craft_cloud_meta::MAX_SEARCH_BYTES;

/// The admin routes, all behind `require_admin`. `router` merges them into the signed-in routes, so `require_device`
/// wraps them too and runs first.
pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/admin/server", get(server))
        .route("/admin/storage", get(storage))
        .route("/admin/users", get(list_users).post(create_user).layer(DefaultBodyLimit::max(TINY_JSON_BYTES)))
        .route("/admin/users/{id}", axum::routing::patch(update_user).delete(remove_user).layer(DefaultBodyLimit::max(TINY_JSON_BYTES)))
        .route("/admin/users/{id}/usage", get(user_usage))
        .route("/admin/users/{id}/disable", post(disable_user).layer(DefaultBodyLimit::max(TINY_JSON_BYTES)))
        .route("/admin/users/{id}/enable", post(enable_user).layer(DefaultBodyLimit::max(TINY_JSON_BYTES)))
        .route("/admin/users/{id}/invite", post(issue_invite).delete(revoke_invite).layer(DefaultBodyLimit::max(TINY_JSON_BYTES)))
        .route("/admin/users/{id}/password-reset", post(issue_reset).layer(DefaultBodyLimit::max(TINY_JSON_BYTES)))
        .route("/admin/users/{id}/reset-two-factor", post(reset_two_factor).layer(DefaultBodyLimit::max(TINY_JSON_BYTES)))
        .route("/admin/users/{id}/devices", get(list_devices))
        .route("/admin/users/{id}/devices/{device_id}", axum::routing::delete(revoke_device).layer(DefaultBodyLimit::max(TINY_JSON_BYTES)))
        .route_layer(middleware::from_fn(require_admin))
        .route_layer(middleware::from_fn(crate::web::reject_mixed_credentials))
}

// ---- helpers ----

/// Reads a JSON body of at most [`TINY_JSON_BYTES`] under the usual deadline. Whatever is wrong with its shape (not JSON,
/// a missing or mistyped field, an unknown role) is `400 invalid`; too long is `413 too_large`.
async fn read_body<T: serde::de::DeserializeOwned>(state: &AppState, request: Request) -> Result<T, ApiError> {
    let bytes = match tokio::time::timeout(state.limits.body_time(TINY_JSON_BYTES), Bytes::from_request(request, &())).await {
        Ok(bytes) => bytes?,
        Err(_) => return Err(body_too_slow()),
    };
    serde_json::from_slice(&bytes)
        .map_err(|_| ApiError::invalid("the body must be a JSON object with your current password in `password`, and the fields of this call"))
}

/// What an admin's body gives for a confirmation: the password, and the second factor when the account has one.
pub(crate) struct Confirmation {
    password: Option<String>,
    totp_code: Option<String>,
    recovery_code: Option<String>,
}

impl Confirmation {
    pub(crate) fn new(password: Option<String>, totp_code: Option<String>, recovery_code: Option<String>) -> Self {
        Self { password, totp_code, recovery_code }
    }
}

/// Checks the acting admin's confirmation and returns the actor to hand to the change: the password ([`confirm_password`]:
/// one wrong password stays counted against the account, a right one is given back), then, when the admin's account has
/// two-factor sign-in, the second factor (`totp_code`, or `recovery_code`), which is used up here: the time step or the
/// recovery code can't serve a second change, and wrong ones are counted and can lock ([`crate::two_factor`]). A password
/// that is wrong is told before anything about the second factor.
async fn confirm(state: &AppState, auth: &DeviceAuth, given: Confirmation) -> Result<AdminActor, ApiError> {
    confirm_actor(state, auth.user, &auth.username, AdminVia::Device(auth.device), given).await
}

/// [`confirm`] for any admin who acts: `via` says whether a device or a browser session does (the web pages call this with
/// the same confirmation, the same counters and the same statuses as the JSON routes above).
pub(crate) async fn confirm_actor(
    state: &AppState,
    user: UserId,
    username: &str,
    via: AdminVia,
    given: Confirmation,
) -> Result<AdminActor, ApiError> {
    let password = given.password.ok_or_else(|| ApiError::invalid("this change needs your current password in `password`"))?;
    // The shape of the second factor is checked before anything is counted.
    let proof = crate::two_factor::parse_proof(given.totp_code, given.recovery_code)?;
    let password_hash = confirm_password(state, user, username, password).await?;
    let two_factor = crate::two_factor::confirm_admin_second_factor(state, user, username, proof).await?;
    Ok(AdminActor { user, via, password_hash, two_factor })
}

fn user_id(id: Result<Path<Uuid>, PathRejection>) -> Result<UserId, ApiError> {
    let Path(id) = id?;
    Ok(UserId(id))
}

pub(crate) fn role_of(name: &str) -> Result<Role, ApiError> {
    match name {
        "admin" => Ok(Role::Admin),
        "member" => Ok(Role::Member),
        _ => Err(ApiError::invalid("role is admin or member")),
    }
}

fn user_info(u: AdminUser) -> AdminUserInfo {
    AdminUserInfo {
        id: u.id.0,
        username: u.username,
        role: u.role.as_str().to_owned(),
        created_ms: u.created_ms,
        disabled_ms: u.disabled_ms,
        two_factor_enabled: u.two_factor_enabled,
        devices_active: u.devices_active,
        stored_bytes: u.stored_bytes,
        quota_bytes: u.quota_bytes,
        invite_pending: u.invite_pending,
    }
}

/// A fresh invite or reset code, as the admin gets it: shown once, never logged.
pub(crate) fn new_code() -> Result<Code, ApiError> {
    Code::generate(INVITE_CHARS).map_err(|e| {
        tracing::error!(error = %e, "the system random source failed");
        ApiError::internal()
    })
}

pub(crate) fn admin_code(state: &AppState, code: Code, expires_ms: i64) -> AdminCode {
    let shown = code.as_str().to_owned();
    let join_url = state.public_url.as_ref().map(|server| JoinString::new(server.clone(), code).link());
    AdminCode { code: shown, join_url, expires_ms }
}

pub(crate) fn end_live_connections(state: &AppState, devices: &[DeviceId]) {
    for device in devices {
        state.live.revoke(*device);
    }
}

// ---- reads ----

/// `GET /admin/server`: counters and kinds only.
async fn server(State(state): State<Shared>) -> Result<Json<AdminServerInfo>, ApiError> {
    let counts = state.meta.admin_server_counts().await?;
    Ok(Json(AdminServerInfo {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        users: counts.users,
        devices_active: counts.devices_active,
        stored_bytes: counts.stored_bytes,
        live_bytes: counts.live_bytes,
        storage_kind: state.store.kind().to_owned(),
        database_kind: state.meta.database_kind().to_owned(),
    }))
}

/// `GET /admin/storage`: bounded account and manifest-index counts, plus the ten largest accounts.
async fn storage(State(state): State<Shared>) -> Result<Json<AdminStorageOverview>, ApiError> {
    let overview = state.meta.admin_storage_overview().await?;
    Ok(Json(AdminStorageOverview {
        accounts: overview.accounts,
        stored_bytes: overview.stored_bytes,
        default_quota_bytes: overview.default_quota_bytes,
        accounts_with_override: overview.accounts_with_override,
        accounts_near_quota: overview.accounts_near_quota,
        accounts_over_quota: overview.accounts_over_quota,
        top_accounts: overview
            .top_accounts
            .into_iter()
            .map(|account| AdminStorageAccount {
                id: account.id.0,
                username: account.username,
                stored_bytes: account.stored_bytes,
                quota_bytes: account.quota_bytes,
            })
            .collect(),
        manifest_index: AdminManifestIndex {
            complete: overview.manifest_index.complete,
            building: overview.manifest_index.building,
            not_indexed: overview.manifest_index.not_indexed,
        },
    }))
}

/// `GET /admin/users/{id}/usage`: the account's stored bytes and labeled estimates.
async fn user_usage(State(state): State<Shared>, id: Result<Path<Uuid>, PathRejection>) -> Result<Json<AccountUsage>, ApiError> {
    let estimate = state.meta.admin_storage_estimate(user_id(id)?).await?;
    let index_complete = estimate.reason.is_none();
    Ok(Json(AccountUsage {
        stored_bytes: estimate.stored_bytes,
        quota_bytes: estimate.quota_bytes,
        trash_bytes: estimate.trash_bytes,
        history_bytes: estimate.history_bytes,
        estimates: UsageEstimates { index_complete, reason: estimate.reason.map(|_| UsageEstimateReason::IndexIncomplete) },
    }))
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    cursor: Option<String>,
    limit: Option<u32>,
    query: Option<String>,
}

/// The username a cursor stands for. A cursor is the last username of a page, in base64url: opaque to clients, and
/// checked like any username, since it is hostile input.
pub(crate) fn decode_cursor(cursor: &str) -> Result<String, ApiError> {
    let bad = || ApiError::invalid("the cursor is not one this server gave");
    let bytes = URL_SAFE_NO_PAD.decode(cursor).map_err(|_| bad())?;
    let name = String::from_utf8(bytes).map_err(|_| bad())?;
    if valid_username(&name) { Ok(name) } else { Err(bad()) }
}

/// `GET /admin/users?cursor&limit&query`: accounts by username, 50 a page (at most 200). `query` finds a username
/// containing it (case does not matter, at most 64 bytes of printable characters).
async fn list_users(State(state): State<Shared>, q: Result<Query<ListQuery>, QueryRejection>) -> Result<Json<AdminUserList>, ApiError> {
    let Query(q) = q?;
    let limit = q.limit.unwrap_or(DEFAULT_PAGE);
    if !(1..=MAX_PAGE).contains(&limit) {
        return Err(ApiError::invalid(format!("limit is 1 to {MAX_PAGE}")));
    }
    let after = q.cursor.as_deref().map(decode_cursor).transpose()?;
    let search = q.query.as_deref().filter(|s| !s.is_empty());
    if let Some(search) = search
        && (search.len() > MAX_QUERY_BYTES || search.chars().any(char::is_control))
    {
        return Err(ApiError::invalid(format!("query is at most {MAX_QUERY_BYTES} bytes of printable characters")));
    }
    let page = state.meta.admin_list_users(after.as_deref(), search, limit, state.now_ms()).await?;
    Ok(Json(AdminUserList {
        users: page.users.into_iter().map(user_info).collect(),
        next: page.next_after.map(|name| URL_SAFE_NO_PAD.encode(name.as_bytes())),
    }))
}

/// `GET /admin/users/{id}/devices`: every device of the account, revoked ones with when and why.
async fn list_devices(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    id: Result<Path<Uuid>, PathRejection>,
) -> Result<Json<AdminDevices>, ApiError> {
    let devices = state.meta.admin_devices(user_id(id)?).await?;
    Ok(Json(AdminDevices { devices: devices.into_iter().map(|d| device_info(d, auth.device)).collect() }))
}

// ---- changes ----

/// `POST /admin/users`: a new account without a password and its invite.
async fn create_user(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    request: Request,
) -> Result<Json<AdminCreatedUser>, ApiError> {
    let req: AdminCreateUser = read_body(&state, request).await?;
    if !valid_username(&req.username) {
        return Err(ApiError::invalid("usernames are 1-64 letters, digits, '.', '_' or '-'"));
    }
    let role = role_of(&req.role)?;
    let actor = confirm(&state, &auth, Confirmation::new(req.password, req.totp_code, req.recovery_code)).await?;
    let code = new_code()?;
    let now = state.now_ms();
    let (user, issued) = state.meta.admin_create_user(&actor, &req.username, role, &code.hash(), now, INVITE_TTL_MS).await?;
    tracing::info!(actor = %auth.device, target = %user.id.0, "admin: account created");
    Ok(Json(AdminCreatedUser { user: user_info(user), invite: admin_code(&state, code, issued.expires_ms) }))
}

/// `PATCH /admin/users/{id}`: a new role and/or quota.
async fn update_user(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    id: Result<Path<Uuid>, PathRejection>,
    request: Request,
) -> Result<Json<AdminUserInfo>, ApiError> {
    let target = user_id(id)?;
    let req: AdminUpdateUser = read_body(&state, request).await?;
    let role = req.role.as_deref().map(role_of).transpose()?;
    if role.is_none() && req.quota_bytes.is_none() {
        return Err(ApiError::invalid("give a role or a quota_bytes to change"));
    }
    let actor = confirm(&state, &auth, Confirmation::new(req.password, req.totp_code, req.recovery_code)).await?;
    let user = state.meta.admin_update_user(&actor, target, role, req.quota_bytes, state.now_ms()).await?;
    tracing::info!(actor = %auth.device, target = %target.0, role_changed = role.is_some(), quota_changed = req.quota_bytes.is_some(), "admin: account changed");
    Ok(Json(user_info(user)))
}

/// `POST /admin/users/{id}/disable`: 204.
async fn disable_user(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    id: Result<Path<Uuid>, PathRejection>,
    request: Request,
) -> Result<StatusCode, ApiError> {
    let target = user_id(id)?;
    let req: AdminConfirm = read_body(&state, request).await?;
    let actor = confirm(&state, &auth, Confirmation::new(req.password, req.totp_code, req.recovery_code)).await?;
    let revoked = state.meta.admin_disable_user(&actor, target, state.now_ms()).await?;
    end_live_connections(&state, &revoked);
    crate::spaces::wake_spaces_of(&state, target).await;
    tracing::info!(actor = %auth.device, target = %target.0, revoked = revoked.len(), "admin: account disabled");
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /admin/users/{id}/enable`: 204. Devices that were signed out stay signed out.
async fn enable_user(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    id: Result<Path<Uuid>, PathRejection>,
    request: Request,
) -> Result<StatusCode, ApiError> {
    let target = user_id(id)?;
    let req: AdminConfirm = read_body(&state, request).await?;
    let actor = confirm(&state, &auth, Confirmation::new(req.password, req.totp_code, req.recovery_code)).await?;
    state.meta.admin_enable_user(&actor, target, state.now_ms()).await?;
    tracing::info!(actor = %auth.device, target = %target.0, "admin: account enabled");
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /admin/users/{id}`: 202. Marks the account for removal; nothing it stored is deleted.
async fn remove_user(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    id: Result<Path<Uuid>, PathRejection>,
    request: Request,
) -> Result<StatusCode, ApiError> {
    let target = user_id(id)?;
    let req: AdminConfirm = read_body(&state, request).await?;
    let actor = confirm(&state, &auth, Confirmation::new(req.password, req.totp_code, req.recovery_code)).await?;
    let revoked = state.meta.admin_remove_user(&actor, target, state.now_ms()).await?;
    end_live_connections(&state, &revoked);
    crate::spaces::wake_spaces_of(&state, target).await;
    tracing::info!(actor = %auth.device, target = %target.0, revoked = revoked.len(), "admin: account marked for removal");
    Ok(StatusCode::ACCEPTED)
}

/// `POST /admin/users/{id}/invite`: a new invite for an account that has no password yet.
async fn issue_invite(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    id: Result<Path<Uuid>, PathRejection>,
    request: Request,
) -> Result<Json<AdminCode>, ApiError> {
    let target = user_id(id)?;
    let req: AdminConfirm = read_body(&state, request).await?;
    let actor = confirm(&state, &auth, Confirmation::new(req.password, req.totp_code, req.recovery_code)).await?;
    let code = new_code()?;
    let issued = state.meta.admin_issue_invite(&actor, target, &code.hash(), state.now_ms(), INVITE_TTL_MS).await?;
    tracing::info!(actor = %auth.device, target = %target.0, "admin: invite issued");
    Ok(Json(admin_code(&state, code, issued.expires_ms)))
}

/// `DELETE /admin/users/{id}/invite`: 204. Ends the account's open invite or reset code.
async fn revoke_invite(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    id: Result<Path<Uuid>, PathRejection>,
    request: Request,
) -> Result<StatusCode, ApiError> {
    let target = user_id(id)?;
    let req: AdminConfirm = read_body(&state, request).await?;
    let actor = confirm(&state, &auth, Confirmation::new(req.password, req.totp_code, req.recovery_code)).await?;
    state.meta.admin_revoke_invite(&actor, target, state.now_ms()).await?;
    tracing::info!(actor = %auth.device, target = %target.0, "admin: invite revoked");
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /admin/users/{id}/password-reset`: a code good for an hour. Redeemed at `POST /join`, it sets a new password,
/// signs every device of the account out, and signs the redeeming computer in.
async fn issue_reset(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    id: Result<Path<Uuid>, PathRejection>,
    request: Request,
) -> Result<Json<AdminCode>, ApiError> {
    let target = user_id(id)?;
    let req: AdminPasswordReset = read_body(&state, request).await?;
    let actor = confirm(&state, &auth, Confirmation::new(req.password, req.totp_code, req.recovery_code)).await?;
    let code = new_code()?;
    let issued =
        state.meta.admin_issue_reset(&actor, target, &code.hash(), req.reset_two_factor, state.now_ms(), craft_cloud_meta::RESET_TTL_MS).await?;
    tracing::info!(actor = %auth.device, target = %target.0, reset_two_factor = req.reset_two_factor, "admin: password reset code issued");
    Ok(Json(admin_code(&state, code, issued.expires_ms)))
}

/// `POST /admin/users/{id}/reset-two-factor`: 204. Takes the account's two-factor sign-in away and signs every device of
/// it out (for someone who lost their authenticator and their recovery codes).
async fn reset_two_factor(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    id: Result<Path<Uuid>, PathRejection>,
    request: Request,
) -> Result<StatusCode, ApiError> {
    let target = user_id(id)?;
    let req: AdminConfirm = read_body(&state, request).await?;
    let actor = confirm(&state, &auth, Confirmation::new(req.password, req.totp_code, req.recovery_code)).await?;
    let revoked = state.meta.admin_reset_two_factor(&actor, target, state.now_ms()).await?;
    end_live_connections(&state, &revoked);
    tracing::info!(actor = %auth.device, target = %target.0, revoked = revoked.len(), "admin: two-factor sign-in reset");
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /admin/users/{id}/devices/{device_id}`: 204.
async fn revoke_device(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    ids: Result<Path<(Uuid, DeviceId)>, PathRejection>,
    request: Request,
) -> Result<StatusCode, ApiError> {
    let Path((target, device)) = ids?;
    let req: AdminConfirm = read_body(&state, request).await?;
    let actor = confirm(&state, &auth, Confirmation::new(req.password, req.totp_code, req.recovery_code)).await?;
    state.meta.admin_revoke_device(&actor, UserId(target), device, state.now_ms()).await?;
    state.live.revoke(device);
    tracing::info!(actor = %auth.device, target = %target, %device, "admin: device revoked");
    Ok(StatusCode::NO_CONTENT)
}
