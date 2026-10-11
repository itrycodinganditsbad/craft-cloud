//! The HTTP API, under `/api/v1`. Everything but `info` and `auth/login` needs a device token.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::rejection::{BytesRejection, JsonRejection, PathRejection, QueryRejection};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router, middleware};
use craft_cloud_meta::{DeviceAuth, valid_username};
use craft_cloud_proto::api::{
    CommitRequest, CommitResponse, DeviceInfo, InfoResponse, LoginRequest, LoginResponse, MAX_MISSING_IDS, MissingRequest, MissingResponse,
    PROTOCOL_MAX, PROTOCOL_MIN, SpaceResponse, codes,
};
use craft_cloud_proto::limits::{MAX_BATCH_JSON_BYTES, MAX_BATCH_OPS, MAX_CHUNK_BYTES, MAX_MANIFEST_BYTES, MAX_PAGE_NODES};
use craft_cloud_proto::{ChangeSet, ChunkId, DeviceId, Manifest, ManifestId, NodeId, PageCursor};
use craft_cloud_store::{BlobKey, StoreError};
use serde::Deserialize;

use crate::auth::{new_token, require_device, verify_password};
use crate::error::ApiError;
use crate::ratelimit::{client_ip, ip_key};
use crate::state::{AppState, now_ms};

/// Chunker parameter sets the server accepts.
pub const CHUNKER_PARAMS: &[&str] = &["fastcdc-v1"];
/// Login attempts per account and per client address in five minutes.
const LOGINS_PER_USER: u32 = 10;
const LOGINS_PER_IP: u32 = 30;
/// Longest change-feed long-poll.
const MAX_WAIT_SECS: u64 = 60;
/// `Retry-After` for a refused long-poll.
const WAITERS_RETRY_SECS: u32 = 5;
/// Small JSON bodies (login and missing-chunk queries).
const SMALL_JSON_BYTES: usize = 1024 * 1024;

type Shared = Arc<AppState>;

pub fn router(state: Shared) -> Router {
    let signed_in = Router::new()
        .route("/devices", get(list_devices))
        .route("/devices/{id}", delete(revoke_device))
        .route("/chunks/missing", post(missing_chunks).layer(DefaultBodyLimit::max(SMALL_JSON_BYTES)))
        .route("/chunks/{id}", put(put_chunk).get(get_chunk).layer(DefaultBodyLimit::max(MAX_CHUNK_BYTES as usize)))
        .route("/manifests/{id}", put(put_manifest).get(get_manifest))
        .route("/commit", post(commit).layer(DefaultBodyLimit::max(MAX_BATCH_JSON_BYTES)))
        .route("/space", get(space))
        .route("/tree", get(tree))
        .route("/changes", get(changes))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_device));
    let public = Router::new().route("/info", get(info)).route("/auth/login", post(login).layer(DefaultBodyLimit::max(SMALL_JSON_BYTES)));
    Router::new()
        .nest("/api/v1", public.merge(signed_in))
        // Version 1 isn't known yet when a client first asks what the server speaks.
        .route("/api/info", get(info))
        .fallback(no_such_endpoint)
        .method_not_allowed_fallback(wrong_method)
        .with_state(state)
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
    req: Result<Json<LoginRequest>, JsonRejection>,
) -> Result<Json<LoginResponse>, ApiError> {
    let Json(req) = req?;
    if !valid_username(&req.username) {
        return Err(ApiError::invalid("usernames are 1-64 letters, digits, '.', '_' or '-'"));
    }
    if req.device_name.is_empty() || req.device_name.len() > 128 || req.device_name.chars().any(char::is_control) {
        return Err(ApiError::invalid("device names are 1-128 bytes without control characters"));
    }
    let now = Instant::now();
    let ip = client_ip(peer, &headers, &state.trusted_proxies);
    let username = req.username.to_ascii_lowercase();
    if !state.logins.allow(&ip_key(ip), LOGINS_PER_IP, now) || !state.logins.allow(&format!("user:{username}"), LOGINS_PER_USER, now) {
        return Err(ApiError::rate_limited("too many sign-in attempts; wait a few minutes", None));
    }
    if req.password.len() > crate::auth::MAX_PASSWORD_BYTES {
        return Err(ApiError::invalid("passwords are at most 1024 bytes"));
    }
    let user = state.meta.user_by_name(&username).await?;
    let hash = user.as_ref().map_or(state.dummy_hash.as_str(), |u| u.password_hash.as_str()).to_owned();
    let password = req.password.clone();
    let ok = tokio::task::spawn_blocking(move || verify_password(&password, &hash)).await.map_err(|e| {
        tracing::error!(error = %e, "password check task failed");
        ApiError::internal()
    })?;
    let (Some(user), true) = (user, ok) else {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, codes::UNAUTHORIZED, "wrong username or password"));
    };
    let (token, token_hash) = new_token()?;
    let device = state.meta.create_device(user.id, &req.device_name, &token_hash, now_ms()).await?;
    tracing::info!(user = %user.username, %device, "device signed in");
    Ok(Json(LoginResponse { device, token }))
}

async fn space(State(state): State<Shared>, Extension(auth): Extension<DeviceAuth>) -> Result<Json<SpaceResponse>, ApiError> {
    let info = state.meta.space_info(auth.space).await?;
    Ok(Json(SpaceResponse { space: auth.space, root: info.root, epoch: info.epoch, seq: info.seq, username: auth.username }))
}

async fn list_devices(State(state): State<Shared>, Extension(auth): Extension<DeviceAuth>) -> Result<Json<Vec<DeviceInfo>>, ApiError> {
    let devices = state.meta.list_devices(auth.user).await?;
    Ok(Json(
        devices
            .into_iter()
            .map(|d| DeviceInfo {
                current: d.id == auth.device,
                id: d.id,
                name: d.name,
                created_ms: d.created_ms,
                last_used_ms: d.last_used_ms,
                revoked: d.revoked,
            })
            .collect(),
    ))
}

async fn revoke_device(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    id: Result<Path<DeviceId>, PathRejection>,
) -> Result<StatusCode, ApiError> {
    let Path(id) = id?;
    state.meta.revoke_device(auth.user, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn missing_chunks(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    req: Result<Json<MissingRequest>, JsonRejection>,
) -> Result<Json<MissingResponse>, ApiError> {
    let Json(req) = req?;
    if req.ids.len() > MAX_MISSING_IDS {
        return Err(ApiError::invalid(format!("ask about at most {MAX_MISSING_IDS} chunks at once")));
    }
    Ok(Json(MissingResponse { missing: state.meta.missing_chunks(auth.space, &req.ids).await? }))
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

async fn put_chunk(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    id: Result<Path<String>, PathRejection>,
    body: Result<Bytes, BytesRejection>,
) -> Result<StatusCode, ApiError> {
    let Path(id) = id?;
    let body = body?;
    let id = parse_chunk_id(&id)?;
    if body.is_empty() {
        return Err(ApiError::invalid("a chunk can't be empty"));
    }
    // Bytes first, record second: the database never points at a blob that isn't stored.
    state.store.put(BlobKey::chunk(&id), &body).await?;
    state.meta.add_chunk(auth.space, &id, body.len() as u64 + 1, now_ms()).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_chunk(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    id: Result<Path<String>, PathRejection>,
) -> Result<Response, ApiError> {
    let Path(id) = id?;
    let id = parse_chunk_id(&id)?;
    // Only chunks this space uploaded; another space's chunk is "not found", not "forbidden".
    if !state.meta.missing_chunks(auth.space, &[id]).await?.is_empty() {
        return Err(ApiError::not_found("chunk"));
    }
    Ok(octets(stored_blob(&state, BlobKey::chunk(&id)).await?))
}

async fn manifest_permit(state: &AppState) -> Result<tokio::sync::SemaphorePermit<'_>, ApiError> {
    state.manifest_uploads.acquire().await.map_err(|e| {
        tracing::error!(error = %e, "manifest semaphore closed");
        ApiError::internal()
    })
}

/// A body that failed to arrive: too long, or the connection broke.
fn body_error(e: axum::Error) -> ApiError {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&e);
    while let Some(err) = source {
        if err.to_string().contains("length limit exceeded") {
            return ApiError::too_large(format!("the body is over the limit of {MAX_MANIFEST_BYTES} bytes"));
        }
        source = err.source();
    }
    ApiError::invalid("the request body couldn't be read")
}

async fn put_manifest(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    id: Result<Path<String>, PathRejection>,
    body: Body,
) -> Result<StatusCode, ApiError> {
    let Path(id) = id?;
    // Big manifests are real work to buffer, decode and check: only a few at a time, server-wide. The permit
    // is taken before the body is read, so the waiting requests hold no buffered bytes.
    let _permit = manifest_permit(&state).await?;
    let body = axum::body::to_bytes(body, MAX_MANIFEST_BYTES).await.map_err(body_error)?;
    let id = parse_manifest_id(&id)?;
    let manifest = Manifest::decode(&body).map_err(|e| ApiError::invalid(format!("bad manifest: {e}")))?;
    if manifest.id() != id {
        return Err(ApiError::invalid("the manifest doesn't match its ID"));
    }
    if !CHUNKER_PARAMS.contains(&manifest.chunker().as_str()) {
        return Err(ApiError::invalid(format!("unknown chunker parameter set {}", manifest.chunker().as_str())));
    }
    let ids: Vec<ChunkId> = manifest.chunks().iter().map(|c| c.id).collect();
    let mut missing = state.meta.missing_chunks(auth.space, &ids).await?;
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
    state.store.put(BlobKey::manifest(&id), &body).await?;
    state.meta.add_manifest(auth.space, &manifest, body.len() as u64 + 1, now_ms()).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_manifest(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    id: Result<Path<String>, PathRejection>,
) -> Result<Response, ApiError> {
    let Path(id) = id?;
    let id = parse_manifest_id(&id)?;
    let _permit = manifest_permit(&state).await?;
    if !state.meta.has_manifest(auth.space, &id).await? {
        return Err(ApiError::not_found("manifest"));
    }
    Ok(octets(stored_blob(&state, BlobKey::manifest(&id)).await?))
}

async fn commit(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    req: Result<Json<CommitRequest>, JsonRejection>,
) -> Result<Json<CommitResponse>, ApiError> {
    let Json(req) = req?;
    // Commit and publish run in their own task: a client that disconnects mid-request drops this handler,
    // and the publish must not be skipped for a commit that went through.
    let task_state = state.clone();
    let outcome = tokio::spawn(async move {
        let outcome = task_state.meta.commit(auth.space, auth.device, &req.ops, now_ms()).await?;
        task_state.notifier.publish(auth.space, outcome.seq);
        Ok::<_, ApiError>(outcome)
    })
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "commit task failed");
        ApiError::internal()
    })??;
    Ok(Json(CommitResponse { seq: outcome.seq, epoch: outcome.epoch }))
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
    Extension(auth): Extension<DeviceAuth>,
    q: Result<Query<PageQuery>, QueryRejection>,
) -> Result<Json<ChangeSet>, ApiError> {
    let Query(q) = q?;
    Ok(Json(state.meta.tree(auth.space, cursor(q.after_seq, q.after_node)?, q.limit.unwrap_or(MAX_PAGE_NODES)).await?))
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
    q: Result<Query<ChangesQuery>, QueryRejection>,
) -> Result<Json<ChangeSet>, ApiError> {
    let Query(q) = q?;
    let cursor = cursor(q.after_seq, q.after_node)?;
    let slot = if q.wait > 0 && cursor.is_none() {
        let slot = state.notifier.begin_wait(auth.space);
        Some(slot.ok_or_else(|| ApiError::rate_limited("too many waiting requests for this space", Some(WAITERS_RETRY_SECS)))?)
    } else {
        None
    };
    if slot.is_some() {
        // Subscribe before reading the sequence number, so a commit in between still wakes us.
        let mut updates = state.notifier.subscribe(auth.space);
        let mut stopping = state.notifier.shutdown_signal();
        if state.meta.space_info(auth.space).await?.seq == q.since {
            let deadline = Duration::from_secs(q.wait.min(MAX_WAIT_SECS));
            // Whichever comes first: a commit, the deadline, or the server shutting down.
            tokio::select! {
                _ = tokio::time::timeout(deadline, updates.wait_for(|seq| *seq > q.since)) => {}
                _ = stopping.wait_for(|stop| *stop) => {}
            }
        }
    }
    Ok(Json(state.meta.changes(auth.space, q.since, cursor, q.limit.unwrap_or(MAX_PAGE_NODES)).await?))
}
