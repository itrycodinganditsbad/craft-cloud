//! The HTTP API, under `/api/v1`. Everything but `info`, `auth/login`, `auth/login/second-factor` and `join` needs a device
//! token. The account and its devices' routes (`account`, `account/password`, renaming and signing out devices) are in
//! [`crate::account`], two-factor sign-in's in [`crate::two_factor`]. `GET /live`
//! becomes the live service's WebSocket ([`crate::live`]); join codes (`pairings`, `join`, and the `/join/{code}` page
//! outside the API) are in [`crate::join`].

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Extension, FromRequest, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, delete, get, post, put};
use axum::{Json, Router, middleware};
use craft_cloud_meta::{BlobKind, DeviceAuth, DeviceClient, DeviceRecord, NewChallenge, Origin, RevokeReason, SignIn, valid_username};
use craft_cloud_proto::api::{
    AccountUsage, ClaimRequest, ClaimResponse, CommitRequest, CommitResponse, DeviceInfo, HISTORY_DEFAULT_LIMIT, HistoryCursor, HistoryResponse,
    InfoResponse, LoginRequest, LoginResponse, MAX_MISSING_IDS, MissingRequest, MissingResponse, PROTOCOL_MAX, PROTOCOL_MIN, RestoreRequest,
    RestoreResponse, SpaceResponse, SpaceRole, TRASH_DEFAULT_LIMIT, TrashCursor, TrashItem, TrashResponse, TrashRestoreResponse, UsageEstimateReason,
    UsageEstimates, codes,
};
use craft_cloud_proto::limits::{MAX_BATCH_JSON_BYTES, MAX_BATCH_OPS, MAX_CHUNK_BYTES, MAX_MANIFEST_BYTES, MAX_PAGE_NODES};
use craft_cloud_proto::{ChangeSet, ChunkId, DeviceId, Manifest, ManifestId, NodeId, PageCursor};
use craft_cloud_store::{BlobKey, StoreError};
use serde::Deserialize;

use crate::account;
use crate::audit;
use crate::auth::{new_token, require_device, verify_password};
use crate::error::ApiError;
use crate::join;
use crate::ratelimit::{client_ip, ip_key};
use crate::space::{SpaceAccess, SpaceGate, require_space};
use crate::state::AppState;
use crate::two_factor;

/// Chunker parameter sets the server accepts.
pub const CHUNKER_PARAMS: &[&str] = &["fastcdc-v1"];
/// Login attempts per account and per client address in five minutes.
pub(crate) const LOGINS_PER_USER: u32 = 10;
pub(crate) const LOGINS_PER_IP: u32 = 30;
/// Longest change-feed long-poll.
const MAX_WAIT_SECS: u64 = 60;
/// `Retry-After` for a refused long-poll.
const WAITERS_RETRY_SECS: u32 = 5;
/// Small JSON bodies (login, join and missing-chunk queries).
pub(crate) const SMALL_JSON_BYTES: usize = 1024 * 1024;
/// Tiny JSON bodies (a password change, a device's new name): a few short strings, so far less than a small body.
pub(crate) const TINY_JSON_BYTES: usize = 4 * 1024;
/// `Retry-After` for a sign-in refused because every password check slot is busy.
pub(crate) const PASSWORD_RETRY_SECS: u32 = 1;

type Shared = Arc<AppState>;

pub fn router(state: Shared) -> Router {
    let signed_in = Router::new()
        .route("/account", get(account::get_account))
        .route("/account/password", post(account::change_password).layer(DefaultBodyLimit::max(TINY_JSON_BYTES)))
        .route("/account/activity", get(audit::account_activity))
        .route("/account/two-factor", get(two_factor::get_status))
        .route("/account/two-factor/setup", post(two_factor::setup).layer(DefaultBodyLimit::max(TINY_JSON_BYTES)))
        .route("/account/two-factor/confirm", post(two_factor::confirm).layer(DefaultBodyLimit::max(TINY_JSON_BYTES)))
        .route("/account/two-factor/disable", post(two_factor::disable).layer(DefaultBodyLimit::max(TINY_JSON_BYTES)))
        .route("/account/two-factor/recovery-codes", post(two_factor::recovery_codes).layer(DefaultBodyLimit::max(TINY_JSON_BYTES)))
        .route("/devices", get(list_devices))
        .route("/devices/revoke-others", post(account::revoke_others))
        .route("/devices/{id}", delete(revoke_device).patch(account::rename_device).layer(DefaultBodyLimit::max(TINY_JSON_BYTES)))
        .route("/pairings", post(join::create_pairing))
        .route("/pairings/{id}", get(join::pairing_status).delete(join::delete_pairing))
        .route("/account/usage", get(account_usage))
        .merge(space_routes(&state))
        .merge(crate::spaces::routes())
        // The admin routes carry `require_admin` themselves; `require_device` below wraps them too and runs first.
        .merge(crate::admin::routes())
        .merge(audit::admin_routes())
        .merge(crate::spaces::admin_routes())
        .route_layer(middleware::from_fn_with_state(state.clone(), require_device));
    let public = Router::new()
        .route("/info", get(info))
        .route("/auth/login", post(login).layer(DefaultBodyLimit::max(SMALL_JSON_BYTES)))
        .route("/auth/login/second-factor", post(two_factor::second_factor).layer(DefaultBodyLimit::max(TINY_JSON_BYTES)))
        .route("/join", post(join::join).layer(DefaultBodyLimit::max(SMALL_JSON_BYTES)));
    let app = Router::new()
        .nest("/api/v1", public.merge(signed_in))
        // Version 1 isn't known yet when a client first asks what the server speaks.
        .route("/api/info", get(info))
        // A join link opened in a browser.
        .route("/join/{code}", get(join::join_page))
        .fallback(no_such_endpoint)
        .method_not_allowed_fallback(wrong_method);
    // The admin pages exist only on a server that was set up for them.
    let app =
        if state.web.is_some() { app.nest("/web", crate::web::routes(state.clone())).merge(crate::web::trailing_slash(state.clone())) } else { app };
    app.with_state(state)
}

/// A route group's role gate: the layer that refuses a caller whose role in the request's space is below `min` (see
/// [`crate::space`]). Applied per method, so one path can need different roles for different methods.
fn gated(state: &Shared, min: SpaceRole, routes: MethodRouter<Shared>) -> MethodRouter<Shared> {
    routes.route_layer(middleware::from_fn_with_state(SpaceGate { state: state.clone(), min }, require_space))
}

/// The space-scoped routes, each with the least role it needs (`docs/design/collaboration.md`, 3.2). A route not listed here
/// has no space layer and takes no `craft-space` header. A new space-scoped route is added here, with its role.
fn space_routes(state: &Shared) -> Router<Shared> {
    use SpaceRole::{Editor, Owner, Viewer};
    Router::new()
        .route("/chunks/missing", gated(state, Editor, post(missing_chunks)).layer(DefaultBodyLimit::max(SMALL_JSON_BYTES)))
        .route("/chunks/claim", gated(state, Editor, post(claim_chunks)).layer(DefaultBodyLimit::max(SMALL_JSON_BYTES)))
        .route(
            "/chunks/{id}",
            gated(state, Viewer, get(get_chunk)).merge(gated(state, Editor, put(put_chunk))).layer(DefaultBodyLimit::max(MAX_CHUNK_BYTES as usize)),
        )
        .route(
            "/manifests/{id}",
            gated(state, Viewer, get(get_manifest)).merge(gated(state, Editor, put(put_manifest))).layer(DefaultBodyLimit::max(MAX_MANIFEST_BYTES)),
        )
        .route("/commit", gated(state, Editor, post(commit)).layer(DefaultBodyLimit::max(MAX_BATCH_JSON_BYTES)))
        .route("/saves", gated(state, Editor, post(attach_save)).layer(DefaultBodyLimit::max(SMALL_JSON_BYTES)))
        .route("/nodes/{id}/history", gated(state, Viewer, get(history)))
        .route("/nodes/{id}/restore", gated(state, Editor, post(restore)).layer(DefaultBodyLimit::max(SMALL_JSON_BYTES)))
        .route("/trash", gated(state, Viewer, get(trash)))
        .route("/trash/{id}/restore", gated(state, Editor, post(restore_from_trash)))
        .route("/trash/{id}", gated(state, Owner, delete(remove_from_trash)))
        .route("/space", gated(state, Viewer, get(space)))
        .route("/tree", gated(state, Viewer, get(tree)))
        .route("/changes", gated(state, Viewer, get(changes)))
        .route("/live", gated(state, Viewer, get(crate::live::connect)))
}

async fn no_such_endpoint() -> ApiError {
    ApiError::not_found("endpoint")
}

async fn wrong_method() -> ApiError {
    ApiError::new(StatusCode::METHOD_NOT_ALLOWED, codes::INVALID, "method not allowed")
}

async fn info() -> Json<InfoResponse> {
    Json(InfoResponse {
        protocol_min: PROTOCOL_MIN,
        protocol_max: PROTOCOL_MAX,
        chunker_params: CHUNKER_PARAMS.iter().map(|s| (*s).to_owned()).collect(),
        max_chunk_bytes: MAX_CHUNK_BYTES,
        max_manifest_bytes: MAX_MANIFEST_BYTES as u64,
        max_batch_ops: MAX_BATCH_OPS as u32,
        max_batch_json_bytes: MAX_BATCH_JSON_BYTES as u64,
        max_page_nodes: MAX_PAGE_NODES,
        max_missing_ids: MAX_MISSING_IDS as u32,
    })
}

async fn login(
    State(state): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    request: Request,
) -> Result<Json<LoginResponse>, ApiError> {
    let req: LoginRequest = read_json(&state, request, SMALL_JSON_BYTES).await?;
    if !valid_username(&req.username) {
        return Err(ApiError::invalid("usernames are 1-64 letters, digits, '.', '_' or '-'"));
    }
    check_device_name(&req.device_name)?;
    let client = device_client(req.client_kind.as_deref(), req.platform.as_deref(), req.app_version.as_deref())?;
    // tokio's clock: the system's monotonic clock in the server, the simulated one in a simulation.
    let now = tokio::time::Instant::now().into_std();
    let ip = client_ip(peer, &headers, &state.trusted_proxies);
    let username = req.username.to_ascii_lowercase();
    // Each check costs 19 MiB and real CPU time: only a few run at once, server-wide. A sign-in that finds them
    // all busy is told to come back, rather than queued (a flood would otherwise queue without end). The permit is
    // taken before the attempt is counted, so an answer that never checked the password costs the client nothing
    // (a flood can't use up a legitimate device's attempts); a sign-in the window then refuses holds it only for
    // that look. The slot is also held across the user lookup, so a slow database gives busy answers instead of a
    // queue. The permit goes into the check, so it is held until the check is done even if the client goes away.
    let permit = state
        .password_checks
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::rate_limited("the server is busy checking other sign-ins; try again in a moment", Some(PASSWORD_RETRY_SECS)))?;
    if !state.logins.allow(&ip_key(ip), LOGINS_PER_IP, now) || !state.logins.allow(&format!("user:{username}"), LOGINS_PER_USER, now) {
        return Err(ApiError::rate_limited("too many sign-in attempts; wait a few minutes", None));
    }
    if req.password.len() > crate::auth::MAX_PASSWORD_BYTES {
        return Err(ApiError::invalid("passwords are at most 1024 bytes"));
    }
    let user = state.meta.user_by_name(&username).await?;
    let known = user.as_ref().map(|u| u.id);
    // An account without a password (an open invite) costs the same Argon2id check as a name nobody has, and never signs in.
    let usable = user.as_ref().is_some_and(|u| crate::auth::usable_hash(&u.password_hash));
    let hash = user.as_ref().filter(|_| usable).map_or(state.dummy_hash.as_str(), |u| u.password_hash.as_str()).to_owned();
    let password = req.password.clone();
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
        // Written after the answer is decided and in a transaction of its own: a log that cannot be written changes
        // neither the answer nor the limits above. A name no account has is never put in the log.
        audit::login_failed(&state, known).await;
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, codes::UNAUTHORIZED, "wrong username or password"));
    };
    let (token, token_hash) = new_token()?;
    // A challenge is made only if the account needs a second factor; the token the client gets for it is a secret of its own.
    let (challenge_token, challenge_hash) = new_token()?;
    let challenge = NewChallenge { id_hash: challenge_hash, client_key: ip_key(ip) };
    // Created only if the password just checked is still the account's: one changed meanwhile (and the devices
    // signed out with it) must not be followed by a device that the old password let in. Answered as a wrong password. The
    // account's two-factor state is read under the same lock: with it on, there is a challenge and no device.
    let signed_in =
        state.meta.sign_in(user.id, (&user.password_hash, user.auth_gen), &req.device_name, &client, &token_hash, &challenge, state.now_ms()).await?;
    let device = match signed_in {
        None => return Err(ApiError::new(StatusCode::UNAUTHORIZED, codes::UNAUTHORIZED, "wrong username or password")),
        Some(SignIn::Device(device)) => device,
        Some(SignIn::Challenge { expires_ms }) => {
            tracing::info!("sign-in needs a second factor");
            return Err(ApiError::two_factor_required(challenge_token, expires_ms));
        }
    };
    // The username is the only personal identifier the server could log here: kept to debug.
    tracing::debug!(user = %user.username, "sign-in");
    tracing::info!(%device, "device signed in");
    Ok(Json(LoginResponse { device, token }))
}

/// The device-name rule of sign-ins and joins, checked before anything is counted: 1-128 bytes without control
/// characters.
pub(crate) fn check_device_name(name: &str) -> Result<(), ApiError> {
    if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
        return Err(ApiError::invalid("device names are 1-128 bytes without control characters"));
    }
    Ok(())
}

/// The client fields of a sign-in or join, checked before anything is counted: an unknown `client_kind`, a field over
/// 32 bytes or a control character is `400 invalid`. The fields are only shown in the device list.
pub(crate) fn device_client(kind: Option<&str>, platform: Option<&str>, app_version: Option<&str>) -> Result<DeviceClient, ApiError> {
    Ok(DeviceClient::new(kind, platform, app_version)?)
}

/// A device as the API shows it; `current` is the device making the request.
pub(crate) fn device_info(d: DeviceRecord, current: DeviceId) -> DeviceInfo {
    DeviceInfo {
        current: d.id == current,
        id: d.id,
        name: d.name,
        created_ms: d.created_ms,
        last_used_ms: d.last_used_ms,
        revoked: d.revoked,
        client_kind: d.client.kind,
        platform: d.client.platform,
        app_version: d.client.app_version,
        revoked_ms: d.revoked_ms,
        revoke_reason: d.revoke_reason.map(|r| r.as_str().to_owned()),
    }
}

async fn space(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    Extension(access): Extension<SpaceAccess>,
) -> Result<Json<SpaceResponse>, ApiError> {
    let info = state.meta.space_info(access.space).await?;
    let name = if access.personal { None } else { state.meta.team_space_name(access.space).await? };
    Ok(Json(SpaceResponse {
        space: access.space,
        root: info.root,
        epoch: info.epoch,
        seq: info.seq,
        username: auth.username,
        kind: Some(if access.personal { "personal" } else { "team" }.to_owned()),
        name,
        role: Some(access.role),
    }))
}

async fn account_usage(State(state): State<Shared>, Extension(auth): Extension<DeviceAuth>) -> Result<Json<AccountUsage>, ApiError> {
    let estimate = state.meta.storage_estimate(auth.personal_space()).await?;
    let index_complete = estimate.reason.is_none();
    Ok(Json(AccountUsage {
        stored_bytes: estimate.stored_bytes,
        quota_bytes: estimate.quota_bytes,
        trash_bytes: estimate.trash_bytes,
        history_bytes: estimate.history_bytes,
        estimates: UsageEstimates { index_complete, reason: estimate.reason.map(|_| UsageEstimateReason::IndexIncomplete) },
    }))
}

async fn list_devices(State(state): State<Shared>, Extension(auth): Extension<DeviceAuth>) -> Result<Json<Vec<DeviceInfo>>, ApiError> {
    let devices = state.meta.list_devices(auth.user).await?;
    Ok(Json(devices.into_iter().map(|d| device_info(d, auth.device)).collect()))
}

async fn revoke_device(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    id: Result<Path<DeviceId>, PathRejection>,
) -> Result<StatusCode, ApiError> {
    let Path(id) = id?;
    state.meta.revoke_device_by(auth.user, id, RevokeReason::User, Origin::Device(auth.device), state.now_ms()).await?;
    // Its live connections end now, not at their next request.
    state.live.revoke(id);
    Ok(StatusCode::NO_CONTENT)
}

async fn missing_chunks(
    State(state): State<Shared>,
    Extension(access): Extension<SpaceAccess>,
    request: Request,
) -> Result<Json<MissingResponse>, ApiError> {
    let req: MissingRequest = read_json(&state, request, SMALL_JSON_BYTES).await?;
    if req.ids.len() > MAX_MISSING_IDS {
        return Err(ApiError::invalid(format!("ask about at most {MAX_MISSING_IDS} chunks at once")));
    }
    Ok(Json(MissingResponse { missing: state.meta.missing_chunks(access.space, &req.ids).await? }))
}

/// `POST /chunks/claim`: the chunks of `from_space` that the working space lacks are added to it, without their bytes (see
/// [`Meta::claim_chunks`]). The write is guarded like an upload, so an editor removed meanwhile is refused.
async fn claim_chunks(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    Extension(access): Extension<SpaceAccess>,
    request: Request,
) -> Result<Json<ClaimResponse>, ApiError> {
    let req: ClaimRequest = read_json(&state, request, SMALL_JSON_BYTES).await?;
    if req.ids.len() > MAX_MISSING_IDS {
        return Err(ApiError::invalid(format!("claim at most {MAX_MISSING_IDS} chunks at once")));
    }
    let claim = access.meta(&state, auth.user).claim_chunks(auth.user, access.space, req.from_space, &req.ids).await?;
    Ok(Json(ClaimResponse { claimed: claim.claimed, missing: claim.missing }))
}

fn parse_chunk_id(id: &str) -> Result<ChunkId, ApiError> {
    ChunkId::from_hex(id).map_err(|e| ApiError::invalid(format!("bad chunk ID: {e}")))
}

fn parse_manifest_id(id: &str) -> Result<ManifestId, ApiError> {
    ManifestId::from_hex(id).map_err(|e| ApiError::invalid(format!("bad manifest ID: {e}")))
}

fn octets(bytes: Bytes) -> Response {
    ([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response()
}

/// Reads a blob the database says this space has. A missing blob then is lost data, not a client mistake.
async fn stored_blob(state: &AppState, key: BlobKey) -> Result<Bytes, ApiError> {
    match state.store.get(key).await {
        Err(StoreError::NotFound(k)) => {
            tracing::error!(blob = ?k, "a stored blob is missing from the blob store");
            Err(ApiError::internal())
        }
        other => Ok(other?),
    }
}

/// Reads a chunk or manifest body: at most the route's limit (`max_bytes`, set with [`DefaultBodyLimit`]: more
/// is `413 too_large`), within [`Limits::body_time`](crate::state::Limits::body_time) (longer is `408 timeout`).
/// The caller holds one of the user's body slots ([`AppState::bodies`]) meanwhile.
async fn read_body(state: &AppState, request: Request, max_bytes: usize) -> Result<Bytes, ApiError> {
    match tokio::time::timeout(state.limits.body_time(max_bytes), Bytes::from_request(request, &())).await {
        Ok(body) => Ok(body?),
        Err(_) => Err(body_too_slow()),
    }
}

/// Reads a JSON body (sign-in, missing-chunk query, commit) under the same deadline as [`read_body`], so a body that
/// trickles in is cut off by the server itself rather than left to a proxy's body timeout. Rejections (no JSON
/// content type, bad JSON, over the route's [`DefaultBodyLimit`]) are answered as before. No slot is held: a JSON
/// body costs one connection and at most `max_bytes`.
pub(crate) async fn read_json<T: serde::de::DeserializeOwned>(state: &AppState, request: Request, max_bytes: usize) -> Result<T, ApiError> {
    match tokio::time::timeout(state.limits.body_time(max_bytes), Json::<T>::from_request(request, &())).await {
        Ok(json) => Ok(json?.0),
        Err(_) => Err(body_too_slow()),
    }
}

/// The most of a refused upload's body that is read and thrown away before the refusal is answered: the biggest manifest and
/// a megabyte over it, so a body just over a route's limit is taken whole.
const DRAIN_BYTES: usize = MAX_MANIFEST_BYTES + 1024 * 1024;
/// The longest that takes.
const DRAIN_TIME: std::time::Duration = std::time::Duration::from_secs(10);

/// Answers a refusal that was decided by the request's headers, before its body was read. A client that is still sending the
/// body when the server answers and closes sees the connection reset (on Windows before it has read the answer), so the
/// server's `413` or `507` would arrive as a network error. The rest of the body is therefore read and dropped first, up to
/// [`DRAIN_BYTES`] and [`DRAIN_TIME`]: a body that is a little over the limit gets its answer; one that is hugely over it, or
/// slow, is cut off when the bound is reached (the body slot is held meanwhile, as for any upload).
async fn refuse(request: Request, error: ApiError) -> ApiError {
    use futures_util::StreamExt;
    let mut body = request.into_body().into_data_stream();
    let drain = async {
        let mut seen = 0usize;
        while let Some(Ok(piece)) = body.next().await {
            seen = seen.saturating_add(piece.len());
            if seen >= DRAIN_BYTES {
                break;
            }
        }
    };
    let _ = tokio::time::timeout(DRAIN_TIME, drain).await;
    error
}

/// The body size a request declares (`Content-Length`), 0 when it declares none.
fn declared_len(request: &Request) -> u64 {
    request.headers().get(axum::http::header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok()).unwrap_or(0)
}

pub(crate) fn body_too_slow() -> ApiError {
    ApiError::new(StatusCode::REQUEST_TIMEOUT, codes::TIMEOUT, "the request body took too long to arrive")
}

/// Keeps what the object really takes on the backend next to the blob's logical size. A statistic, not data: when it
/// can't be written, the upload still succeeded and the size just stays unknown (or as it was).
async fn record_physical(state: &Shared, kind: BlobKind, id: &[u8; 32], written: craft_cloud_store::Written) {
    if let Err(e) = state.meta.set_physical(kind, id, written.physical_len, written.level).await {
        tracing::warn!(error = %e, "couldn't record a blob's stored size");
    }
}

async fn put_chunk(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    Extension(access): Extension<SpaceAccess>,
    id: Result<Path<String>, PathRejection>,
    request: Request,
) -> Result<StatusCode, ApiError> {
    let Path(id) = id?;
    let id = parse_chunk_id(&id)?;
    // Held until the chunk is stored: the user's buffered bodies are bounded.
    let _body = state.bodies.acquire(auth.user).await?;
    // A body over the route's limit is refused before anything else is asked of the server, by what the request declares.
    let declared = declared_len(&request);
    if declared > u64::from(MAX_CHUNK_BYTES) {
        return Err(refuse(request, ApiError::too_large("the chunk is over the size limit")).await);
    }
    // A chunk the space already has costs the server nothing (as for `check_room`), so a full server still takes it.
    let known = state.meta.missing_chunks(access.space, &[id]).await?.is_empty();
    // A full server says so before the body is read, by what the request declares, and again with the real size.
    if !known && let Err(e) = state.check_server_room(declared).await {
        return Err(refuse(request, e).await);
    }
    let body = read_body(&state, request, MAX_CHUNK_BYTES as usize).await?;
    if body.is_empty() {
        return Err(ApiError::invalid("a chunk can't be empty"));
    }
    if !known {
        state.check_server_room(body.len() as u64 + 1).await?;
    }
    // A space at its cap doesn't get to store bytes it would then be refused for.
    state.meta.check_room(access.space, BlobKind::Chunk, id.as_bytes(), body.len() as u64 + 1).await?;
    // Bytes first, record second: the database never points at a blob that isn't stored.
    let written = state.store.put(BlobKey::chunk(&id), &body).await?;
    access.meta(&state, auth.user).add_chunk(access.space, &id, body.len() as u64 + 1, state.now_ms()).await?;
    if !known {
        state.note_stored(body.len() as u64 + 1);
    }
    record_physical(&state, BlobKind::Chunk, id.as_bytes(), written).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_chunk(
    State(state): State<Shared>,
    Extension(access): Extension<SpaceAccess>,
    id: Result<Path<String>, PathRejection>,
) -> Result<Response, ApiError> {
    let Path(id) = id?;
    let id = parse_chunk_id(&id)?;
    // Only chunks this space uploaded; another space's chunk is "not found", not "forbidden".
    if !state.meta.missing_chunks(access.space, &[id]).await?.is_empty() {
        return Err(ApiError::not_found("chunk"));
    }
    Ok(octets(stored_blob(&state, BlobKey::chunk(&id)).await?))
}

async fn put_manifest(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    Extension(access): Extension<SpaceAccess>,
    id: Result<Path<String>, PathRejection>,
    request: Request,
) -> Result<StatusCode, ApiError> {
    let Path(id) = id?;
    let id = parse_manifest_id(&id)?;
    // A body slot while the body arrives and until the manifest is stored, so the user's buffered bodies are
    // bounded; then, with the body in, one of the user's manifest slots for the real work (decoding and checking a
    // big manifest). A body that is slow to arrive holds only its own user's body slot, and only until its time is up.
    let _body = state.bodies.acquire(auth.user).await?;
    let declared = declared_len(&request);
    if declared > MAX_MANIFEST_BYTES as u64 {
        return Err(refuse(request, ApiError::too_large("the manifest is over the size limit")).await);
    }
    // A manifest the space already has costs the server nothing, so a full server still takes it again.
    let known = state.meta.has_manifest(access.space, &id).await?;
    if !known && let Err(e) = state.check_server_room(declared).await {
        return Err(refuse(request, e).await);
    }
    let body = read_body(&state, request, MAX_MANIFEST_BYTES).await?;
    let _work = state.manifests.acquire(auth.user).await?;
    let manifest = Manifest::decode(&body).map_err(|e| ApiError::invalid(format!("bad manifest: {e}")))?;
    if manifest.id() != id {
        return Err(ApiError::invalid("the manifest doesn't match its ID"));
    }
    if !CHUNKER_PARAMS.contains(&manifest.chunker().as_str()) {
        return Err(ApiError::invalid(format!("unknown chunker parameter set {}", manifest.chunker().as_str())));
    }
    let ids: Vec<ChunkId> = manifest.chunks().iter().map(|c| c.id).collect();
    let mut missing = state.meta.missing_chunks(access.space, &ids).await?;
    if !missing.is_empty() {
        let total = missing.len();
        missing.truncate(MAX_MISSING_IDS);
        let listed = missing.len();
        let mut err = ApiError::missing_chunks(missing);
        if total > listed {
            err.message = format!("{total} chunk(s) must be uploaded first; the first {listed} are listed");
        }
        return Err(err);
    }
    if !known {
        state.check_server_room(body.len() as u64 + 1).await?;
    }
    state.meta.check_room(access.space, BlobKind::Manifest, id.as_bytes(), body.len() as u64 + 1).await?;
    let written = state.store.put(BlobKey::manifest(&id), &body).await?;
    access.meta(&state, auth.user).add_manifest(access.space, &manifest, body.len() as u64 + 1, state.now_ms()).await?;
    if !known {
        state.note_stored(body.len() as u64 + 1);
    }
    // The index of its chunks is complete before the upload is acknowledged: a manifest of up to the index budget was indexed
    // in the transaction above, a longer one is finished here in batches (and by `index build` if this is cut short).
    state.meta.index_complete(&manifest).await?;
    record_physical(&state, BlobKind::Manifest, id.as_bytes(), written).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_manifest(
    State(state): State<Shared>,
    Extension(access): Extension<SpaceAccess>,
    id: Result<Path<String>, PathRejection>,
) -> Result<Response, ApiError> {
    let Path(id) = id?;
    let id = parse_manifest_id(&id)?;
    // No permit: one indexed query and one blob read. (Uploads' slots are per user and never block this.)
    if !state.meta.has_manifest(access.space, &id).await? {
        return Err(ApiError::not_found("manifest"));
    }
    Ok(octets(stored_blob(&state, BlobKey::manifest(&id)).await?))
}

async fn commit(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    Extension(access): Extension<SpaceAccess>,
    request: Request,
) -> Result<Json<CommitResponse>, ApiError> {
    let req: CommitRequest = read_json(&state, request, MAX_BATCH_JSON_BYTES).await?;
    // Commit and publish run in their own task: a client that disconnects mid-request drops this handler,
    // and the publish must not be skipped for a commit that went through.
    let task_state = state.clone();
    let meta = access.meta(&state, auth.user);
    let outcome = tokio::spawn(async move {
        let outcome = meta.commit(access.space, auth.device, &req.ops, task_state.now_ms()).await?;
        task_state.notifier.publish(access.space, outcome.seq);
        Ok::<_, ApiError>(outcome)
    })
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "commit task failed");
        ApiError::internal()
    })??;
    Ok(Json(CommitResponse { seq: outcome.seq, epoch: outcome.epoch }))
}

/// `POST /saves`: a save record for a version that went up without one ([`craft_cloud_proto::api::SaveAttach`]).
async fn attach_save(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    Extension(access): Extension<SpaceAccess>,
    request: Request,
) -> Result<Json<CommitResponse>, ApiError> {
    let req: craft_cloud_proto::api::SaveAttach = read_json(&state, request, SMALL_JSON_BYTES).await?;
    let task_state = state.clone();
    let meta = access.meta(&state, auth.user);
    // As a commit: the publish is never skipped for a change that went through.
    let outcome = tokio::spawn(async move {
        let outcome = meta.attach_save(access.space, req.node, req.version, &req.save).await?;
        task_state.notifier.publish(access.space, outcome.seq);
        Ok::<_, ApiError>(outcome)
    })
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "save attach task failed");
        ApiError::internal()
    })??;
    Ok(Json(CommitResponse { seq: outcome.seq, epoch: outcome.epoch }))
}

#[derive(Debug, Deserialize)]
struct HistoryQuery {
    cursor: Option<String>,
    limit: Option<u32>,
}

/// `GET /nodes/{id}/history`: the versions of a file of the caller's own space, newest first. The node is looked
/// up in the space of the device token, so another account's node is `not_found`.
async fn history(
    State(state): State<Shared>,
    Extension(access): Extension<SpaceAccess>,
    id: Result<Path<NodeId>, PathRejection>,
    q: Result<Query<HistoryQuery>, QueryRejection>,
) -> Result<Json<HistoryResponse>, ApiError> {
    let Path(node) = id?;
    let Query(q) = q?;
    let after = q.cursor.as_deref().map(str::parse::<HistoryCursor>).transpose().map_err(|_| ApiError::invalid("bad history cursor"))?;
    let page = state.meta.history(access.space, node, after, q.limit.unwrap_or(HISTORY_DEFAULT_LIMIT)).await?;
    Ok(Json(HistoryResponse { versions: page.versions, next: page.next.map(|c| c.to_string()) }))
}

/// `POST /nodes/{id}/restore`: a new version with the content of an old one, as an ordinary update of the file.
async fn restore(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    Extension(access): Extension<SpaceAccess>,
    id: Result<Path<NodeId>, PathRejection>,
    request: Request,
) -> Result<Json<RestoreResponse>, ApiError> {
    let Path(node) = id?;
    let req: RestoreRequest = read_json(&state, request, SMALL_JSON_BYTES).await?;
    // As a commit: the publish is never skipped for a change that went through.
    let task_state = state.clone();
    let meta = access.meta(&state, auth.user);
    let outcome = tokio::spawn(async move {
        let outcome = meta.restore_version(access.space, auth.device, node, req.version, req.expected_version, task_state.now_ms()).await?;
        task_state.notifier.publish(access.space, outcome.seq);
        Ok::<_, ApiError>(outcome)
    })
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "restore task failed");
        ApiError::internal()
    })??;
    Ok(Json(RestoreResponse { version: outcome.version, parent_version: outcome.parent_version, seq: outcome.seq, epoch: outcome.epoch }))
}

const MS_PER_DAY: i64 = 86_400_000;

#[derive(Debug, Deserialize)]
struct TrashQuery {
    cursor: Option<String>,
    limit: Option<u32>,
}

/// `GET /trash`: the deleted nodes of the caller's own space, newest deletion first.
async fn trash(
    State(state): State<Shared>,
    Extension(access): Extension<SpaceAccess>,
    q: Result<Query<TrashQuery>, QueryRejection>,
) -> Result<Json<TrashResponse>, ApiError> {
    let Query(q) = q?;
    let after = q.cursor.as_deref().map(str::parse::<TrashCursor>).transpose().map_err(|_| ApiError::invalid("bad trash cursor"))?;
    let page = state.meta.trash(access.space, after, q.limit.unwrap_or(TRASH_DEFAULT_LIMIT), state.now_ms()).await?;
    if page.repaired > 0 {
        tracing::warn!(count = page.repaired, "deleted nodes without a deletion time were stamped with the current time");
    }
    let keep = i64::from(state.trash_days).saturating_mul(MS_PER_DAY);
    let items = page
        .entries
        .into_iter()
        .map(|e| TrashItem {
            id: e.id,
            parent: e.parent,
            name: e.name,
            is_dir: e.is_dir,
            size: e.size,
            deleted_ms: e.deleted_ms,
            purge_after_ms: e.deleted_ms.saturating_add(keep),
        })
        .collect();
    Ok(Json(TrashResponse { items, next: page.next.map(|c| c.to_string()) }))
}

/// `POST /trash/{id}/restore`: a deleted node of the caller's space comes back, in one change.
async fn restore_from_trash(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    Extension(access): Extension<SpaceAccess>,
    id: Result<Path<NodeId>, PathRejection>,
) -> Result<Json<TrashRestoreResponse>, ApiError> {
    let Path(node) = id?;
    // As a commit: the publish is never skipped for a change that went through.
    let task_state = state.clone();
    let meta = access.meta(&state, auth.user);
    let outcome = tokio::spawn(async move {
        let outcome = meta.restore_from_trash(access.space, auth.device, node, task_state.now_ms()).await?;
        task_state.notifier.publish(access.space, outcome.seq);
        Ok::<_, ApiError>(outcome)
    })
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "trash restore task failed");
        ApiError::internal()
    })??;
    Ok(Json(TrashRestoreResponse {
        node: outcome.node,
        parent: outcome.parent,
        name: outcome.name,
        renamed: outcome.renamed,
        version: outcome.version,
        seq: outcome.seq,
        epoch: outcome.epoch,
    }))
}

/// `DELETE /trash/{id}`: takes a deleted node out of the trash. Only marks it: nothing is freed.
async fn remove_from_trash(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    Extension(access): Extension<SpaceAccess>,
    id: Result<Path<NodeId>, PathRejection>,
) -> Result<StatusCode, ApiError> {
    let Path(node) = id?;
    access.meta(&state, auth.user).request_purge(access.space, node, state.now_ms()).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn cursor(after_seq: Option<u64>, after_node: Option<NodeId>) -> Result<Option<PageCursor>, ApiError> {
    match (after_seq, after_node) {
        (Some(seq), Some(node)) => Ok(Some(PageCursor { seq, node })),
        (None, None) => Ok(None),
        _ => Err(ApiError::invalid("after_seq and after_node go together")),
    }
}

#[derive(Debug, Deserialize)]
struct PageQuery {
    after_seq: Option<u64>,
    after_node: Option<NodeId>,
    limit: Option<u32>,
}

async fn tree(
    State(state): State<Shared>,
    Extension(access): Extension<SpaceAccess>,
    q: Result<Query<PageQuery>, QueryRejection>,
) -> Result<Json<ChangeSet>, ApiError> {
    let Query(q) = q?;
    Ok(Json(state.meta.tree(access.space, cursor(q.after_seq, q.after_node)?, q.limit.unwrap_or(MAX_PAGE_NODES)).await?))
}

/// Resolves when `device` is revoked (or some revocations were missed, so that it must be looked up).
async fn revoked(revocations: &mut tokio::sync::broadcast::Receiver<DeviceId>, device: DeviceId) {
    use tokio::sync::broadcast::error::RecvError;
    loop {
        match revocations.recv().await {
            Ok(revoked) if revoked == device => return,
            Ok(_) => {}
            Err(RecvError::Lagged(_)) => return,
            Err(RecvError::Closed) => std::future::pending::<()>().await,
        }
    }
}

// The paging fields are written out here rather than flattened from `PageQuery`: serde's `flatten` buffers
// values as strings, so numbers in a query string would never parse.
#[derive(Debug, Deserialize)]
struct ChangesQuery {
    since: u64,
    after_seq: Option<u64>,
    after_node: Option<NodeId>,
    limit: Option<u32>,
    #[serde(default)]
    wait: u64,
}

/// Changes after `since`. With `wait` (seconds, up to 60) and nothing new yet, the request is held open until
/// a commit lands, the time runs out or the server shuts down. `wait` is ignored when a paging cursor is
/// given. When the space already has `MAX_WAITERS_PER_SPACE` requests waiting, the answer is 429.
async fn changes(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    Extension(access): Extension<SpaceAccess>,
    q: Result<Query<ChangesQuery>, QueryRejection>,
) -> Result<Json<ChangeSet>, ApiError> {
    let Query(q) = q?;
    let cursor = cursor(q.after_seq, q.after_node)?;
    let slot = if q.wait > 0 && cursor.is_none() {
        let slot = state.notifier.begin_wait(access.space);
        Some(slot.ok_or_else(|| ApiError::rate_limited("too many waiting requests for this space", Some(WAITERS_RETRY_SECS)))?)
    } else {
        None
    };
    if slot.is_some() {
        // Subscribe before reading the sequence number, so a commit in between still wakes us.
        let mut updates = state.notifier.subscribe(access.space);
        let mut members = state.notifier.subscribe_members(access.space);
        let mut stopping = state.notifier.shutdown_signal();
        // Listening for revocations first, then looking the device up: a revoke before the lookup is seen by it, one
        // after it ends the wait, so none is missed between the sign-in check and the wait.
        let mut revocations = state.live.revocations();
        if !state.meta.device_active(auth.device).await? {
            return Err(ApiError::unauthorized());
        }
        if state.meta.space_info(access.space).await?.seq == q.since {
            let deadline = Duration::from_secs(q.wait.min(MAX_WAIT_SECS));
            // Whichever comes first: a commit, the deadline, the server shutting down, or this device signed out.
            tokio::select! {
                _ = tokio::time::timeout(deadline, updates.wait_for(|seq| *seq > q.since)) => {}
                _ = stopping.wait_for(|stop| *stop) => {}
                () = revoked(&mut revocations, auth.device) => {}
                // A change to the space's members: the access is looked at again below, at once.
                _ = members.changed() => {}
            }
        }
    }
    // A team space's access can end while the request waits (a member removed, the role lowered, an account disabled): the
    // wait is over by now, and the answer is the one a request from outside the space gets. Nothing is read for the
    // caller's own space on a request without the header.
    if slot.is_some() && !access.personal {
        match state.meta.space_access(auth.user, access.space).await? {
            Some(role) if role >= SpaceRole::Viewer => {}
            _ => return Err(ApiError::not_found("space")),
        }
    }
    let changes = state.meta.changes(access.space, q.since, cursor, q.limit.unwrap_or(MAX_PAGE_NODES)).await?;
    // A long-poll looks the device up once more after the fetch, the last thing before the answer: a device signed
    // out while it waited or while the change set was read (by the API, or by the command line in another process,
    // which wakes nothing) gets no change feed. A revoke that commits after this lookup is a sign-out that happened
    // after the answer was decided.
    if slot.is_some() && !state.meta.device_active(auth.device).await? {
        return Err(ApiError::unauthorized());
    }
    Ok(Json(changes))
}
