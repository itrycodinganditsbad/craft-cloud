//! Request and response bodies of the HTTP API (`/api/v1/…`), shared by the server and the sync agent.
//! Batch commits use [`crate::Op`], [`crate::Conflict`] and [`crate::ChangeSet`] from `ops`.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::hash::ChunkId;
use crate::ids::{DeviceId, Epoch, NodeId, SpaceId, VersionId};
use crate::ops::{Conflict, Op};

/// What apps see of these bodies lives in `craft-cloud-api` (no postcard), re-exported here.
pub use craft_cloud_api::api::{DeviceInfo, ErrorAnswer, PairingState, PairingStatus, codes};

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

/// `POST /api/v1/pairings` (device auth): a new add-a-computer code for the signed-in account, which replaces the
/// device's open one. `code` is shown once, normalised (no dashes); the server keeps only its hash. Its `Debug`
/// output hides the code.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingCreated {
    pub id: Uuid,
    pub code: String,
    /// When the code stops working, milliseconds since the Unix epoch, by the server's clock.
    pub expires_ms: i64,
}

impl std::fmt::Debug for PairingCreated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingCreated").field("id", &self.id).field("code", &"<redacted>").field("expires_ms", &self.expires_ms).finish()
    }
}

/// `POST /api/v1/join` (no auth): redeem an invite or an add-a-computer code (normalised or as typed) for a new
/// device; answered with a [`LoginResponse`]. `password` is required, and only taken, for an invite to an account
/// that has none yet. Its `Debug` output hides the code and the password.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinRequest {
    pub code: String,
    /// Shown in the device list, e.g. the computer's name.
    pub device_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

impl std::fmt::Debug for JoinRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinRequest")
            .field("code", &"<redacted>")
            .field("device_name", &self.device_name)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
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

/// `POST /api/v1/saves`: a save record for a version that went up without one (its app's record reached the agent
/// late). Appended: a version that has one keeps it, and the same call again changes nothing. Answered with a
/// [`CommitResponse`] (the sequence number the node is in the change feed at, or the current one when nothing
/// changed); `not_found` when the node has no such version. An older server answers `not_found` for the path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveAttach {
    pub node: crate::NodeId,
    pub version: crate::VersionId,
    pub save: crate::SaveRecord,
}

/// Versions in one `GET /api/v1/nodes/{id}/history` page when `limit` is not given.
pub const HISTORY_DEFAULT_LIMIT: u32 = 50;
/// Most versions in one history page; a larger `limit` is `invalid`.
pub const HISTORY_MAX_LIMIT: u32 = 200;

/// Where the next history page starts: after this `(seq, version)` in the newest-first order. On the wire it is an
/// opaque string; clients pass back the `next` they were given and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryCursor {
    pub seq: u64,
    pub version: VersionId,
}

impl std::fmt::Display for HistoryCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.seq, self.version)
    }
}

/// A history cursor that does not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadHistoryCursor;

impl std::fmt::Display for BadHistoryCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not a history cursor")
    }
}

impl std::error::Error for BadHistoryCursor {}

impl std::str::FromStr for HistoryCursor {
    type Err = BadHistoryCursor;

    /// Only the exact form [`Display`](std::fmt::Display) writes is accepted, so one cursor has one spelling.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (seq, version) = s.split_once('.').ok_or(BadHistoryCursor)?;
        let cursor = HistoryCursor { seq: seq.parse().map_err(|_| BadHistoryCursor)?, version: version.parse().map_err(|_| BadHistoryCursor)? };
        if cursor.to_string() == s { Ok(cursor) } else { Err(BadHistoryCursor) }
    }
}

/// One version of a file in its history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryVersion {
    pub id: VersionId,
    pub parent_version: Option<VersionId>,
    pub epoch: Epoch,
    /// The space's change sequence number of the commit that made this version.
    pub seq: u64,
    pub size: u64,
    pub mtime_ms: i64,
    /// The device that committed it.
    pub device: DeviceId,
    pub created_ms: i64,
    /// Whether a save record (a Craft app's live-document save) hangs on this version.
    pub save: bool,
    /// Thinned out of the history: its row stays, but it can't be restored. Always `false` until thinning exists.
    pub pruned: bool,
}

/// `GET /api/v1/nodes/{id}/history?cursor&limit`: the versions of a file, newest first by `(seq, id)`. `next` is the
/// `cursor` of the following page, absent on the last.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryResponse {
    pub versions: Vec<HistoryVersion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// `POST /api/v1/nodes/{id}/restore`: make `version` the file's content again as a new version. `expected_version`
/// must still be the file's current version, else the answer is `conflict`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreRequest {
    pub version: VersionId,
    pub expected_version: VersionId,
}

/// A restore that applied: the new version, which follows `parent_version` (the one that was current).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreResponse {
    pub version: VersionId,
    pub parent_version: VersionId,
    pub seq: u64,
    pub epoch: Epoch,
}

/// Items in one `GET /api/v1/trash` page when `limit` is not given.
pub const TRASH_DEFAULT_LIMIT: u32 = 50;
/// Most items in one trash page; a larger `limit` is `invalid`.
pub const TRASH_MAX_LIMIT: u32 = 200;

/// Where the next trash page starts: after this `(deleted_ms, node)` in the newest-deleted-first order. Opaque on the
/// wire, like [`HistoryCursor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrashCursor {
    pub deleted_ms: i64,
    pub node: NodeId,
}

impl std::fmt::Display for TrashCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.deleted_ms, self.node)
    }
}

impl std::str::FromStr for TrashCursor {
    type Err = BadHistoryCursor;

    /// Only the exact form [`Display`](std::fmt::Display) writes is accepted.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (deleted_ms, node) = s.split_once('.').ok_or(BadHistoryCursor)?;
        let cursor = TrashCursor { deleted_ms: deleted_ms.parse().map_err(|_| BadHistoryCursor)?, node: node.parse().map_err(|_| BadHistoryCursor)? };
        if cursor.to_string() == s { Ok(cursor) } else { Err(BadHistoryCursor) }
    }
}

/// One deleted node in the trash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrashItem {
    pub id: NodeId,
    /// The folder it was in when it was deleted (itself deleted, perhaps).
    pub parent: NodeId,
    pub name: String,
    pub is_dir: bool,
    /// The size of a file's last version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    pub deleted_ms: i64,
    /// `deleted_ms` plus the server's `retention.trash_days`: when the server may remove the node for good. Nothing is
    /// removed yet, so this is a promise of the earliest time, not an event.
    pub purge_after_ms: i64,
}

/// `GET /api/v1/trash?cursor&limit`: the deleted nodes of your space that were not removed from the trash, newest
/// deletion first by `(deleted_ms, id)`. `next` is the `cursor` of the following page, absent on the last.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrashResponse {
    pub items: Vec<TrashItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// `POST /api/v1/trash/{id}/restore`: the node is back. `parent` and `name` say where, which differs from where it was
/// when a folder of its path was merged into a live folder of the same name, or when `renamed` is set: the name was
/// taken, so the node got a conflict name like the sync engine makes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrashRestoreResponse {
    pub node: NodeId,
    pub parent: NodeId,
    pub name: String,
    pub renamed: bool,
    /// For a file, its new current version: a copy of its last one (the same content), so a delete sent against the old
    /// version, whose answer a machine never heard, can't delete it again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<VersionId>,
    pub seq: u64,
    pub epoch: Epoch,
}

/// A commit that applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitResponse {
    pub seq: u64,
    pub epoch: Epoch,
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
        let join = JoinRequest { code: "K7M4Q9TX".into(), device_name: "pc".into(), password: Some("hunter2-secret".into()) };
        let shown = format!("{join:?}");
        assert!(!shown.contains("K7M4Q9TX") && !shown.contains("hunter2-secret"), "{shown}");
        let created = PairingCreated { id: Uuid::nil(), code: "K7M4Q9TX".into(), expires_ms: 1 };
        assert!(!format!("{created:?}").contains("K7M4Q9TX"));
    }

    #[test]
    fn join_bodies_have_a_plain_json_form() {
        let status = PairingStatus { state: PairingState::Used, device_name: Some("lptp1".into()) };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json, serde_json::json!({ "state": "used", "device_name": "lptp1" }));
        let waiting: PairingStatus = serde_json::from_value(serde_json::json!({ "state": "waiting" })).unwrap();
        assert_eq!(waiting, PairingStatus { state: PairingState::Waiting, device_name: None });
        let join: JoinRequest = serde_json::from_value(serde_json::json!({ "code": "c", "device_name": "d" })).unwrap();
        assert_eq!(join.password, None);
        assert_eq!(codes::INVALID_CODE, "invalid_code");
        assert_eq!(codes::PASSWORD_REQUIRED, "password_required");
        assert_eq!(codes::WEAK_PASSWORD, "weak_password");
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
    fn history_cursors_have_one_spelling() {
        let cursor = HistoryCursor { seq: 7, version: VersionId::generate() };
        assert_eq!(cursor.to_string().parse::<HistoryCursor>(), Ok(cursor));
        let text = cursor.to_string();
        for bad in
            ["", "7", "7.", ".x", "x.y", "+7.00000000-0000-0000-0000-000000000000", &text.to_uppercase(), &format!("{text}x"), &format!(" {text}")]
        {
            assert_eq!(bad.parse::<HistoryCursor>(), Err(BadHistoryCursor), "{bad:?}");
        }
        assert_eq!("-1.00000000-0000-0000-0000-000000000000".parse::<HistoryCursor>(), Err(BadHistoryCursor));
    }

    #[test]
    fn trash_cursors_have_one_spelling() {
        let cursor = TrashCursor { deleted_ms: 1_760_000_000_000, node: NodeId::generate() };
        assert_eq!(cursor.to_string().parse::<TrashCursor>(), Ok(cursor));
        let text = cursor.to_string();
        for bad in ["", "7", "7.", ".x", "x.y", &format!("+{text}"), &text.to_uppercase(), &format!("{text}x"), &format!(" {text}")] {
            assert_eq!(bad.parse::<TrashCursor>(), Err(BadHistoryCursor), "{bad:?}");
        }
    }

    #[test]
    fn history_and_restore_bodies_have_a_plain_json_form() {
        let restore: RestoreRequest = serde_json::from_value(serde_json::json!({
            "version": "01a12b0b-9fca-73db-b82d-c7c33683601e",
            "expected_version": "01a12b0b-9fca-73db-b82d-c7c33683601f",
            "unknown": 1
        }))
        .unwrap();
        assert_ne!(restore.version, restore.expected_version);
        let last = HistoryResponse { versions: vec![], next: None };
        assert_eq!(serde_json::to_value(&last).unwrap(), serde_json::json!({ "versions": [] }));
    }

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(codes::MISSING_CHUNKS, "missing_chunks");
        assert_eq!(codes::RESYNC_REQUIRED, "resync_required");
        assert_eq!(codes::RATE_LIMITED, "rate_limited");
    }
}
