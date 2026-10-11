//! The HTTP API, under `/api/v1`. Everything but `info` and `auth/login` needs a device token.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router, middleware};
use craft_cloud_meta::DeviceAuth;
use craft_cloud_proto::api::{
    CommitRequest, CommitResponse, DeviceInfo, InfoResponse, LoginRequest, LoginResponse, MAX_MISSING_IDS, MissingRequest, MissingResponse,
    PROTOCOL_MAX, PROTOCOL_MIN,
};
use craft_cloud_proto::limits::{MAX_BATCH_JSON_BYTES, MAX_BATCH_OPS, MAX_CHUNK_BYTES, MAX_MANIFEST_BYTES, MAX_PAGE_NODES};
use craft_cloud_proto::{ChangeSet, ChunkId, DeviceId, Manifest, ManifestId, NodeId, PageCursor};
use craft_cloud_store::BlobKey;
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
/// Small JSON bodies (login, missing-chunk queries, device names).
const SMALL_JSON_BYTES: usize = 1024 * 1024;

type Shared = Arc<AppState>;

pub fn router(state: Shared) -> Router {
    let signed_in = Router::new()
        .route("/devices", get(list_devices))
        .route("/devices/{id}", delete(revoke_device))
        .route("/chunks/missing", post(missing_chunks).layer(DefaultBodyLimit::max(SMALL_JSON_BYTES)))
        .route("/chunks/{id}", put(put_chunk).get(get_chunk).layer(DefaultBodyLimit::max(MAX_CHUNK_BYTES as usize)))
        .route("/manifests/{id}", put(put_manifest).get(get_manifest).layer(DefaultBodyLimit::max(MAX_MANIFEST_BYTES)))
        .route("/commit", post(commit).layer(DefaultBodyLimit::max(MAX_BATCH_JSON_BYTES)))
        .route("/tree", get(tree))
        .route("/changes", get(changes))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_device));
    let public = Router::new().route("/info", get(info)).route("/auth/login", post(login).layer(DefaultBodyLimit::max(SMALL_JSON_BYTES)));
    Router::new().nest("/api/v1", public.merge(signed_in)).with_state(state)
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
    Json(req): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, ApiError> {
    if req.username.len() > 64 {
        return Err(ApiError::invalid("usernames are at most 64 characters"));
    }
    let now = Instant::now();
    let ip = client_ip(peer, &headers, &state.trusted_proxies);
    let username = req.username.to_ascii_lowercase();
    if !state.logins.allow(&ip_key(ip), LOGINS_PER_IP, now) || !state.logins.allow(&format!("user:{username}"), LOGINS_PER_USER, now) {
        return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "too many sign-in attempts; wait a few minutes"));
    }
    if req.password.len() > crate::auth::MAX_PASSWORD_BYTES {
        return Err(ApiError::invalid("passwords are at most 1024 bytes"));
    }
    let user = state.meta.user_by_name(&username).await?;
    let hash = user.as_ref().map_or(state.dummy_hash.as_str(), |u| u.password_hash.as_str()).to_owned();
    let password = req.password.clone();
    let ok = tokio::task::spawn_blocking(move || verify_password(&password, &hash)).await.map_err(|_| ApiError::internal())?;
    let (Some(user), true) = (user, ok) else {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", "wrong username or password"));
    };
    let (token, token_hash) = new_token()?;
    let device = state.meta.create_device(user.id, &req.device_name, &token_hash, now_ms()).await?;
    tracing::info!(user = %user.username, %device, "device signed in");
    Ok(Json(LoginResponse { device, token }))
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
    Path(id): Path<DeviceId>,
) -> Result<StatusCode, ApiError> {
    state.meta.revoke_device(auth.user, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn missing_chunks(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    Json(req): Json<MissingRequest>,
) -> Result<Json<MissingResponse>, ApiError> {
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

async fn put_chunk(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let id = parse_chunk_id(&id)?;
    if body.is_empty() {
        return Err(ApiError::invalid("a chunk can't be empty"));
    }
    // Bytes first, record second: the database never points at a blob that isn't stored.
    state.store.put(BlobKey::chunk(&id), &body).await?;
    state.meta.add_chunk(auth.space, &id, body.len() as u64 + 1, now_ms()).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_chunk(State(state): State<Shared>, Extension(auth): Extension<DeviceAuth>, Path(id): Path<String>) -> Result<Response, ApiError> {
    let id = parse_chunk_id(&id)?;
    // Only chunks this space uploaded; another space's chunk is "not found", not "forbidden".
    if !state.meta.missing_chunks(auth.space, &[id]).await?.is_empty() {
        return Err(ApiError::not_found("chunk"));
    }
    Ok(octets(state.store.get(BlobKey::chunk(&id)).await?))
}

async fn put_manifest(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let id = parse_manifest_id(&id)?;
    let manifest = Manifest::decode(&body).map_err(|e| ApiError::invalid(format!("bad manifest: {e}")))?;
    if manifest.id() != id {
        return Err(ApiError::invalid("the manifest doesn't match its ID"));
    }
    if !CHUNKER_PARAMS.contains(&manifest.chunker().as_str()) {
        return Err(ApiError::invalid(format!("unknown chunker parameter set {}", manifest.chunker().as_str())));
    }
    let ids: Vec<ChunkId> = manifest.chunks().iter().map(|c| c.id).collect();
    let missing = state.meta.missing_chunks(auth.space, &ids).await?;
    if !missing.is_empty() {
        return Err(ApiError::missing_chunks(missing));
    }
    state.store.put(BlobKey::manifest(&id), &body).await?;
    state.meta.add_manifest(auth.space, &manifest, body.len() as u64 + 1, now_ms()).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_manifest(State(state): State<Shared>, Extension(auth): Extension<DeviceAuth>, Path(id): Path<String>) -> Result<Response, ApiError> {
    let id = parse_manifest_id(&id)?;
    if !state.meta.has_manifest(auth.space, &id).await? {
        return Err(ApiError::not_found("manifest"));
    }
    Ok(octets(state.store.get(BlobKey::manifest(&id)).await?))
}

async fn commit(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    Json(req): Json<CommitRequest>,
) -> Result<Json<CommitResponse>, ApiError> {
    let outcome = state.meta.commit(auth.space, auth.device, &req.ops, now_ms()).await?;
    state.notifier.publish(auth.space, outcome.seq);
    Ok(Json(CommitResponse { seq: outcome.seq, epoch: outcome.epoch }))
}

#[derive(Debug, Deserialize)]
struct PageQuery {
    after_seq: Option<u64>,
    after_node: Option<NodeId>,
    limit: Option<u32>,
}

impl PageQuery {
    fn cursor(&self) -> Result<Option<PageCursor>, ApiError> {
        match (self.after_seq, self.after_node) {
            (Some(seq), Some(node)) => Ok(Some(PageCursor { seq, node })),
            (None, None) => Ok(None),
            _ => Err(ApiError::invalid("after_seq and after_node go together")),
        }
    }

    fn limit(&self) -> u32 {
        self.limit.unwrap_or(MAX_PAGE_NODES)
    }
}

async fn tree(State(state): State<Shared>, Extension(auth): Extension<DeviceAuth>, Query(q): Query<PageQuery>) -> Result<Json<ChangeSet>, ApiError> {
    Ok(Json(state.meta.tree(auth.space, q.cursor()?, q.limit()).await?))
}

#[derive(Debug, Deserialize)]
struct ChangesQuery {
    since: u64,
    #[serde(flatten)]
    page: PageQuery,
    #[serde(default)]
    wait: u64,
}

/// Changes after `since`. With `wait` (seconds, up to 60) and nothing new yet, the request is held open until
/// a commit lands or the time runs out.
async fn changes(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    Query(q): Query<ChangesQuery>,
) -> Result<Json<ChangeSet>, ApiError> {
    let cursor = q.page.cursor()?;
    if q.wait > 0 && cursor.is_none() {
        // Subscribe before reading the sequence number, so a commit in between still wakes us.
        let mut updates = state.notifier.subscribe(auth.space);
        if state.meta.space_info(auth.space).await?.seq == q.since {
            let deadline = Duration::from_secs(q.wait.min(MAX_WAIT_SECS));
            let _ = tokio::time::timeout(deadline, updates.wait_for(|seq| *seq > q.since)).await;
        }
    }
    Ok(Json(state.meta.changes(auth.space, q.since, cursor, q.page.limit()).await?))
}
