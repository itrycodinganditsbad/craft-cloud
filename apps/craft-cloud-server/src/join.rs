//! Join codes over HTTP: a signed-in device's add-a-computer codes (`/api/v1/pairings`), redeeming a code for a new
//! device (`POST /api/v1/join`), and the page a join link opens in a browser (`GET /join/{code}`).
//!
//! - **One answer for every code that doesn't work**: wrong, expired, used, malformed, or a pairing whose maker has
//!   signed out are all `404 invalid_code` with the same message, so an answer never says which.
//! - **Failed redeems are limited per client address** (behind trusted proxies, as sign-ins are): [`JOINS_PER_IP`]
//!   in a minute, then `429 rate_limited`, even for a code that works. Only `invalid_code` counts. A successful
//!   redeem doesn't, and neither does `password_required` or `weak_password`: they are only answered for a real,
//!   unused invite, so they tell a guesser nothing a success wouldn't. Every try is counted before the code is
//!   looked at, and given back once it turns out not to be `invalid_code`, so tries sent at the same moment are held
//!   to the limit as well.
//! - **An invite to a new account** must carry a password (the server's password rules), hashed with Argon2id under
//!   the same server-wide slots as sign-ins; it becomes the account's in the transaction that creates the device.
//! - **Codes are never logged**, and neither are tokens or passwords; the database keeps only codes' hashes.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::PathRejection;
use axum::extract::{ConnectInfo, Extension, Path, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use craft_cloud_meta::DeviceAuth;
use craft_cloud_proto::api::{JoinRequest, LoginResponse, PairingCreated, PairingStatus, codes};
use craft_cloud_proto::join::{Code, PAIRING_CHARS, PAIRING_TTL_MS};
use uuid::Uuid;

use crate::api::{PASSWORD_RETRY_SECS, SMALL_JSON_BYTES, check_device_name, read_json};
use crate::auth::{check_password_rules, hash_password, new_token};
use crate::error::ApiError;
use crate::ratelimit::{client_ip, ip_key};
use crate::state::AppState;

/// Failed redeems one client address may make in a minute.
pub const JOINS_PER_IP: u32 = 10;
/// What a join link shows in a browser, whatever its code.
pub const JOIN_PAGE: &str = "Craft Cloud\n\nOpen Craft Cloud on the computer you are adding and enter this code there.\n";

type Shared = Arc<AppState>;

/// `POST /api/v1/pairings`: a new add-a-computer code from the signed-in device, replacing its open one.
pub async fn create_pairing(State(state): State<Shared>, Extension(auth): Extension<DeviceAuth>) -> Result<Json<PairingCreated>, ApiError> {
    let code = Code::generate(PAIRING_CHARS).map_err(|e| {
        tracing::error!(error = %e, "the system random source failed");
        ApiError::internal()
    })?;
    let now = state.now_ms();
    let id = state.meta.create_pairing(auth.user, auth.device, &code.hash(), now).await?;
    tracing::info!(device = %auth.device, "add-a-computer code created");
    Ok(Json(PairingCreated { id, code: code.as_str().to_owned(), expires_ms: now.saturating_add(PAIRING_TTL_MS) }))
}

/// `GET /api/v1/pairings/{id}`: one of the account's pairings. A pairing that is gone is `404 not_found`.
pub async fn pairing_status(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    id: Result<Path<Uuid>, PathRejection>,
) -> Result<Json<PairingStatus>, ApiError> {
    let Path(id) = id?;
    let (state, device_name) = state.meta.pairing_status(auth.user, id, state.now_ms()).await?.ok_or_else(|| ApiError::not_found("pairing"))?;
    Ok(Json(PairingStatus { state, device_name }))
}

/// `DELETE /api/v1/pairings/{id}`: cancels one of the account's pairings (one that is gone already is fine).
pub async fn delete_pairing(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    id: Result<Path<Uuid>, PathRejection>,
) -> Result<StatusCode, ApiError> {
    let Path(id) = id?;
    state.meta.delete_pairing(auth.user, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/join` (no sign-in): redeems a code for a new device, answered as a sign-in is.
pub async fn join(
    State(state): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    request: Request,
) -> Result<Json<LoginResponse>, ApiError> {
    let req: JoinRequest = read_json(&state, request, SMALL_JSON_BYTES).await?;
    check_device_name(&req.device_name)?;
    // tokio's clock: the system's monotonic clock in the server, the simulated one in a simulation.
    let now = tokio::time::Instant::now().into_std();
    let key = ip_key(client_ip(peer, &headers, &state.trusted_proxies));
    // Counted before the redeem, so tries made at the same moment are held to the limit too; given back unless the
    // code didn't work.
    if !state.joins.allow(&key, JOINS_PER_IP, now) {
        return Err(ApiError::rate_limited("too many tries with codes that don't work; wait a minute", None));
    }
    let result = redeem(&state, &req).await;
    if !result.as_ref().is_err_and(|e| e.code == codes::INVALID_CODE) {
        state.joins.refund(&key, now);
    }
    result.map(Json)
}

async fn redeem(state: &AppState, req: &JoinRequest) -> Result<LoginResponse, ApiError> {
    let code = Code::parse_any(&req.code).ok_or_else(ApiError::invalid_code)?;
    let code_hash = code.hash();
    let now_ms = state.now_ms();
    let new_password_hash = match state.meta.invite_needs_password(&code_hash, now_ms).await? {
        Some(true) => {
            let password = req.password.as_deref().ok_or_else(ApiError::password_required)?;
            check_password_rules(password).map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, codes::WEAK_PASSWORD, e.message))?;
            Some(hash_in_a_slot(state, password).await?)
        }
        Some(false) | None => None,
    };
    let (token, token_hash) = new_token()?;
    let redeemed = state.meta.redeem(&code_hash, &req.device_name, &token_hash, new_password_hash.as_deref(), now_ms).await?;
    tracing::info!(device = %redeemed.device, "device joined with a code");
    Ok(LoginResponse { device: redeemed.device, token })
}

/// Hashes a new password in one of the server's password-check slots, off the async threads; every slot busy is
/// `429 rate_limited`, to come back in a second (as a sign-in is told).
async fn hash_in_a_slot(state: &AppState, password: &str) -> Result<String, ApiError> {
    let permit = state
        .password_checks
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::rate_limited("the server is busy checking other sign-ins; try again in a moment", Some(PASSWORD_RETRY_SECS)))?;
    let password = password.to_owned();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        hash_password(&password)
    })
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "password hashing task failed");
        ApiError::internal()
    })?
}

/// `GET /join/{code}`: the page a join link opens in a browser. The same text whatever the code: it is never looked
/// up, so the page can't be used to test codes.
pub async fn join_page() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        JOIN_PAGE,
    )
}
