//! Request and response bodies of the HTTP API (`/api/v1/…`), shared by the server and the sync agent.
//! Batch commits use [`crate::Op`], [`crate::Conflict`] and [`crate::ChangeSet`] from `ops`.

use serde::{Deserialize, Serialize};

use crate::hash::ChunkId;
use crate::ids::{DeviceId, Epoch, NodeId, SpaceId};
use crate::ops::{Conflict, Op};

/// Protocol versions this build speaks. A client and server must share at least one.
pub const PROTOCOL_MIN: u32 = 1;
pub const PROTOCOL_MAX: u32 = 1;

/// Most chunk IDs in one `POST /chunks/missing`.
pub const MAX_MISSING_IDS: usize = 10_000;

/// `GET /api/v1/info`: what the server speaks and its limits. No sign-in needed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InfoResponse {
    pub protocol_min: u32,
    pub protocol_max: u32,
    /// Chunker parameter sets the server accepts in manifests, e.g. `fastcdc-v1`.
    pub chunker_params: Vec<String>,
    pub max_chunk_bytes: u32,
    pub max_manifest_bytes: u64,
    pub max_batch_ops: u32,
    pub max_batch_json_bytes: u64,
    pub max_page_nodes: u32,
    pub max_missing_ids: u32,
}

/// `GET /api/v1/space`: the signed-in user's space.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceResponse {
    pub space: SpaceId,
    /// The space's root directory.
    pub root: NodeId,
    pub epoch: Epoch,
    /// The space's current change sequence number.
    pub seq: u64,
    /// The account's username, in lowercase.
    pub username: String,
}

/// `POST /api/v1/auth/login`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
    /// Shown in the device list, e.g. the computer's name.
    pub device_name: String,
}

impl std::fmt::Debug for LoginRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginRequest")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("device_name", &self.device_name)
            .finish()
    }
}

/// The device token is shown once; the server keeps only its hash. Send it as `Authorization: Bearer <token>`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginResponse {
    pub device: DeviceId,
    pub token: String,
}

impl std::fmt::Debug for LoginResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginResponse").field("device", &self.device).field("token", &"<redacted>").finish()
    }
}

/// One of the signed-in user's devices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub id: DeviceId,
    pub name: String,
    pub created_ms: i64,
    pub last_used_ms: i64,
    pub revoked: bool,
    /// The device making this request.
    pub current: bool,
}

/// `POST /api/v1/chunks/missing` and its answer: which of `ids` this space doesn't have.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingRequest {
    pub ids: Vec<ChunkId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingResponse {
    pub missing: Vec<ChunkId>,
}

/// `POST /api/v1/commit`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitRequest {
    pub ops: Vec<Op>,
}

/// A commit that applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitResponse {
    pub seq: u64,
    pub epoch: Epoch,
}

/// Stable error codes, the `error` field of [`ErrorBody`].
pub mod codes {
    /// A commit's operations clash with the current state; `conflicts` lists them.
    pub const CONFLICT: &str = "conflict";
    /// Chunks must be uploaded first; `missing` lists them.
    pub const MISSING_CHUNKS: &str = "missing_chunks";
    /// The client's change counter is stale or ahead of the server: list the whole tree again.
    pub const RESYNC_REQUIRED: &str = "resync_required";
    pub const UNAUTHORIZED: &str = "unauthorized";
    pub const RATE_LIMITED: &str = "rate_limited";
    pub const INVALID: &str = "invalid";
    pub const NOT_FOUND: &str = "not_found";
    pub const TOO_LARGE: &str = "too_large";
    /// A request body took longer to arrive than the server allows for it (HTTP 408); send it again.
    pub const TIMEOUT: &str = "timeout";
    pub const USERNAME_TAKEN: &str = "username_taken";
    /// A chunk or manifest would take the account over the bytes the server lets it store (HTTP 507); `quota` says
    /// how much it stores and may store.
    pub const QUOTA_EXCEEDED: &str = "quota_exceeded";
    pub const INTERNAL: &str = "internal";
}

/// How much an account stores on the server and may store, in bytes, with `quota_exceeded`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaUse {
    pub used: u64,
    pub limit: u64,
}

/// Every error answer. `error` is a stable code, one of [`codes`]; `message` is for people. `conflicts` is
/// set for `conflict`, `missing` for `missing_chunks`, `quota` for `quota_exceeded`. Clients ignore codes and JSON
/// fields they don't know (the server may add more later), so match on the codes you handle and treat the rest by
/// HTTP status.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflicts: Option<Vec<Conflict>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing: Option<Vec<ChunkId>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<QuotaUse>,
}

/// Response header carrying the space's epoch on every signed-in request.
pub const EPOCH_HEADER: &str = "craft-epoch";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_stay_out_of_debug_output() {
        let req = LoginRequest { username: "ana".into(), password: "hunter2-secret".into(), device_name: "pc".into() };
        assert!(!format!("{req:?}").contains("hunter2-secret"));
        let res = LoginResponse { device: DeviceId::generate(), token: "tok-secret".into() };
        assert!(!format!("{res:?}").contains("tok-secret"));
    }

    #[test]
    fn error_bodies_omit_empty_extras() {
        let body = ErrorBody { error: "not_found".into(), message: "no such chunk".into(), ..ErrorBody::default() };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json, serde_json::json!({ "error": "not_found", "message": "no such chunk" }));
        assert_eq!(serde_json::from_value::<ErrorBody>(json).unwrap(), body);
    }

    #[test]
    fn a_quota_answer_says_how_much_is_used() {
        let body = ErrorBody {
            error: codes::QUOTA_EXCEEDED.into(),
            message: "m".into(),
            quota: Some(QuotaUse { used: 9, limit: 10 }),
            ..ErrorBody::default()
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["quota"], serde_json::json!({ "used": 9, "limit": 10 }));
        assert_eq!(serde_json::from_value::<ErrorBody>(json).unwrap(), body);
    }

    #[test]
    fn unknown_error_fields_are_ignored() {
        let json = serde_json::json!({ "error": "something_new", "message": "m", "retry_in": 3 });
        let body: ErrorBody = serde_json::from_value(json).unwrap();
        assert_eq!(body.error, "something_new");
    }

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(codes::MISSING_CHUNKS, "missing_chunks");
        assert_eq!(codes::RESYNC_REQUIRED, "resync_required");
        assert_eq!(codes::RATE_LIMITED, "rate_limited");
    }
}
