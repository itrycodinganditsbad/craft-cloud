//! The signed-in account: `GET /api/v1/account`, `POST /api/v1/account/password`, and the device routes that change
//! a device (`PATCH /api/v1/devices/{id}`, `POST /api/v1/devices/revoke-others`).
//!
//! - **A wrong current password is `403 wrong_password`, never 401**: on a signed-in route the agent reads every 401
//!   as "this device was revoked" and signs out. The same holds for every later route that asks for a password.
//! - **Changing the password** reads the body (at most [`TINY_JSON_BYTES`]), counts the try against the account
//!   ([`PASSWORD_CHANGES_PER_USER`] in 15 minutes, wrong passwords included), takes one of the server-wide
//!   password-check slots (every slot busy is `429 rate_limited` and the try is given back), checks the current
//!   password and the new one's rules, and hashes the new one, all in that one slot. Then one transaction sets the
//!   hash, revokes every other device and deletes the account's open invite and pairings
//!   ([`craft_cloud_meta::Meta::change_password`]); afterwards each revoked device's live connections end at once.
//!   The device that asked stays signed in.
//! - **Nothing secret is logged**: not a password, a hash or a token. The count of revoked devices is logged at
//!   `info`, the username at most at `debug`.

use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::PathRejection;
use axum::extract::{Extension, Path, Request, State};
use axum::http::StatusCode;
use craft_cloud_meta::{DeviceAuth, RevokeReason, UserId};
use craft_cloud_proto::DeviceId;
use craft_cloud_proto::api::{AccountInfo, ChangePasswordRequest, DeviceInfo, RenameDeviceRequest, RevokedOthers, codes};

use crate::api::{PASSWORD_RETRY_SECS, TINY_JSON_BYTES, device_info, read_json};
use crate::auth::{MAX_PASSWORD_BYTES, check_password_rules, hash_password, verify_password};
use crate::error::ApiError;
use crate::state::AppState;

/// Password change tries one account may make in [`AppState::password_changes`]'s window (15 minutes).
pub const PASSWORD_CHANGES_PER_USER: u32 = 5;

type Shared = Arc<AppState>;

/// `403 wrong_password`: the current password was wrong. Never 401, see the module's notes.
pub(crate) fn wrong_password() -> ApiError {
    ApiError::new(StatusCode::FORBIDDEN, codes::WRONG_PASSWORD, "the current password is wrong")
}

/// Checks the signed-in account's own password (`user`, named `username`: a device's account, or a browser session's) for a
/// route that asks for one again (a change of the two-factor settings, an admin's confirmation) and returns the password hash it was verified against, which the change then compares with the
/// stored one under the account's lock. Wrong passwords are counted per account ([`PASSWORD_CHANGES_PER_USER`] in 15
/// minutes, the window of the password change) and a right one is given back, so only a wrong password stays counted;
/// every check holds one of the server-wide password-check slots, and all busy is `429 rate_limited` that costs no try. A
/// wrong password is `403 wrong_password`, never 401.
pub(crate) async fn confirm_password(state: &AppState, user_id: UserId, username: &str, password: String) -> Result<String, ApiError> {
    if password.len() > MAX_PASSWORD_BYTES {
        return Err(ApiError::invalid("passwords are at most 1024 bytes"));
    }
    // tokio's clock: the system's monotonic clock in the server, the simulated one in a simulation.
    let now = tokio::time::Instant::now().into_std();
    let key = format!("user:{username}");
    let Some(counted) = state.password_changes.count(&key, PASSWORD_CHANGES_PER_USER, now) else {
        return Err(ApiError::rate_limited("too many wrong passwords; wait a few minutes", None));
    };
    // Each check costs 19 MiB and real CPU time: all slots busy is told to come back, not queued, and costs no try.
    let Ok(permit) = state.password_checks.clone().try_acquire_owned() else {
        state.password_changes.refund(&key, counted);
        return Err(ApiError::rate_limited("the server is busy checking other passwords; try again in a moment", Some(PASSWORD_RETRY_SECS)));
    };
    let user = match state.meta.user_by_name(username).await {
        Ok(Some(user)) if user.id == user_id => user,
        other => {
            state.password_changes.refund(&key, counted);
            other?;
            return Err(ApiError::unauthorized());
        }
    };
    let hash = user.password_hash.clone();
    let ok = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        verify_password(&password, &hash)
    })
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "password check task failed");
        ApiError::internal()
    })?;
    if !ok {
        return Err(wrong_password());
    }
    state.password_changes.refund(&key, counted);
    Ok(user.password_hash)
}

/// `GET /api/v1/account`.
pub async fn get_account(State(state): State<Shared>, Extension(auth): Extension<DeviceAuth>) -> Result<Json<AccountInfo>, ApiError> {
    let account = state.meta.account(auth.user).await?;
    Ok(Json(AccountInfo {
        user_id: account.id.0,
        username: account.username,
        role: account.role.as_str().to_owned(),
        created_ms: account.created_ms,
        password_changed_ms: account.password_changed_ms,
        two_factor_enabled: account.two_factor_enabled,
    }))
}

/// `POST /api/v1/account/password`: `204` once the password is changed and the account's other devices are signed out.
pub async fn change_password(State(state): State<Shared>, Extension(auth): Extension<DeviceAuth>, request: Request) -> Result<StatusCode, ApiError> {
    let req: ChangePasswordRequest = read_json(&state, request, TINY_JSON_BYTES).await?;
    if req.current_password.len() > MAX_PASSWORD_BYTES || req.new_password.len() > MAX_PASSWORD_BYTES {
        return Err(ApiError::invalid("passwords are at most 1024 bytes"));
    }
    // tokio's clock: the system's monotonic clock in the server, the simulated one in a simulation.
    let now = tokio::time::Instant::now().into_std();
    let key = format!("user:{}", auth.username);
    let Some(counted) = state.password_changes.count(&key, PASSWORD_CHANGES_PER_USER, now) else {
        return Err(ApiError::rate_limited("too many password change attempts; wait a few minutes", None));
    };
    // Each check costs 19 MiB and real CPU time (see the sign-in): all slots busy is told to come back, not queued,
    // and costs the account no try, since nothing was checked.
    let Ok(permit) = state.password_checks.clone().try_acquire_owned() else {
        state.password_changes.refund(&key, counted);
        return Err(ApiError::rate_limited("the server is busy checking other passwords; try again in a moment", Some(PASSWORD_RETRY_SECS)));
    };
    let user = state.meta.user_by_name(&auth.username).await?.ok_or_else(ApiError::unauthorized)?;
    let current_hash = user.password_hash.clone();
    let ChangePasswordRequest { current_password, new_password } = req;
    let new_hash = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if !verify_password(&current_password, &current_hash) {
            return Err(wrong_password());
        }
        check_password_rules(&new_password).map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, codes::WEAK_PASSWORD, e.message))?;
        if new_password == current_password {
            return Err(ApiError::new(StatusCode::BAD_REQUEST, codes::WEAK_PASSWORD, "the new password must differ from the current one"));
        }
        hash_password(&new_password)
    })
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "password change task failed");
        ApiError::internal()
    })??;
    let revoked = state.meta.change_password(auth.user, &user.password_hash, &new_hash, auth.device, state.now_ms()).await?;
    for device in &revoked {
        state.live.revoke(*device);
    }
    tracing::debug!(user = %auth.username, "password changed");
    tracing::info!(device = %auth.device, revoked = revoked.len(), "password changed");
    Ok(StatusCode::NO_CONTENT)
}

/// `PATCH /api/v1/devices/{id}`: renames one of the account's own devices that isn't revoked. Anything else is
/// `404 not_found`, so the answer never says whether another account has the device.
pub async fn rename_device(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    id: Result<Path<DeviceId>, PathRejection>,
    request: Request,
) -> Result<Json<DeviceInfo>, ApiError> {
    let Path(id) = id?;
    let req: RenameDeviceRequest = read_json(&state, request, TINY_JSON_BYTES).await?;
    crate::api::check_device_name(&req.name)?;
    let device = state.meta.rename_device(auth.user, id, &req.name).await?;
    Ok(Json(device_info(device, auth.device)))
}

/// `POST /api/v1/devices/revoke-others`: signs out every other device of the account.
pub async fn revoke_others(State(state): State<Shared>, Extension(auth): Extension<DeviceAuth>) -> Result<Json<RevokedOthers>, ApiError> {
    let revoked = state.meta.revoke_other_devices(auth.user, auth.device, RevokeReason::Others, state.now_ms()).await?;
    // Their live connections end now, not at their next request.
    for device in &revoked {
        state.live.revoke(*device);
    }
    tracing::info!(device = %auth.device, revoked = revoked.len(), "other devices signed out");
    Ok(Json(RevokedOthers { revoked: u32::try_from(revoked.len()).unwrap_or(u32::MAX) }))
}
