//! Request and response bodies of the HTTP API (`/api/v1/…`), shared by the server and the sync agent.
//! Batch commits use [`crate::Op`], [`crate::Conflict`] and [`crate::ChangeSet`] from `ops`.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::hash::ChunkId;
use crate::ids::{DeviceId, Epoch, NodeId, SpaceId, VersionId};
use crate::ops::{Conflict, Op};

/// What apps see of these bodies lives in `craft-cloud-api` (no postcard), re-exported here.
pub use craft_cloud_api::api::{
    AUDIT_ACTIONS, AccountInfo, AccountUsage, AddSpaceMember, AdminCode, AdminCompression, AdminCompressionEstimate, AdminCompressionLevelEstimate,
    AdminConfirm, AdminCreateSpace, AdminCreateUser, AdminCreatedUser, AdminDevices, AdminManifestIndex, AdminPasswordReset, AdminServerInfo,
    AdminSetCompressionLevel, AdminSetSpaceOwner, AdminSpaceInfo, AdminSpaceList, AdminStorageAccount, AdminStorageOverview, AdminUpdateUser,
    AdminUserInfo, AdminUserList, AuditDetails, AuditEvent, AuditExportEnd, AuditPage, ChangeSpaceMember, DeviceInfo, ErrorAnswer, PairingState,
    PairingStatus, RecoveryCodes, RevokedOthers, SpaceConfirm, SpaceList, SpaceListItem, SpaceMember, SpaceMembers, SpaceRole, SpaceUsage,
    TwoFactorChallenge, TwoFactorSetupResponse, TwoFactorStatus, UpdateSpace, UsageEstimateReason, UsageEstimates, codes, valid_audit_action,
};

/// Protocol versions this build speaks. A client and server must share at least one. Version 2 adds the `craft-space`
/// header and its echo ([`SPACE_HEADER`]) and the team-space routes; version 1 clients keep working unchanged.
pub const PROTOCOL_MIN: u32 = 1;
pub const PROTOCOL_MAX: u32 = 2;

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
    /// `personal` or `team` (appended: an older server leaves it out).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The team space's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The caller's role in the space.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<SpaceRole>,
}

/// `POST /api/v1/auth/login`. `client_kind`, `platform` and `app_version` are optional and only shown in the device
/// list, never a basis for a permission: `client_kind` is `agent`, `desktop` or `cli`, the others are free text of at
/// most 32 bytes (all without control characters). A body without them works as before.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
    /// Shown in the device list, e.g. the computer's name.
    pub device_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,
}

impl std::fmt::Debug for LoginRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginRequest")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("device_name", &self.device_name)
            .field("client_kind", &self.client_kind)
            .field("platform", &self.platform)
            .field("app_version", &self.app_version)
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
/// that has none yet. `client_kind`, `platform` and `app_version` are as in [`LoginRequest`]. `totp_code` or
/// `recovery_code` (one of them) is the second factor of an account that has two-factor sign-in: the server answers
/// `two_factor_required` without using the code up, and the same body is sent again with it. Its `Debug` output hides
/// the code, the password and the second factor.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinRequest {
    pub code: String,
    /// Shown in the device list, e.g. the computer's name.
    pub device_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totp_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_code: Option<String>,
}

impl std::fmt::Debug for JoinRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinRequest")
            .field("code", &"<redacted>")
            .field("device_name", &self.device_name)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("client_kind", &self.client_kind)
            .field("platform", &self.platform)
            .field("app_version", &self.app_version)
            .field("totp_code", &self.totp_code.as_ref().map(|_| "<redacted>"))
            .field("recovery_code", &self.recovery_code.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// `POST /api/v1/account/password` (device auth): changes the password, answered `204`. Every other device of the
/// account is signed out, and its open invite and pairings are deleted. A wrong `current_password` is `403
/// wrong_password` (never 401), a `new_password` that breaks the rules `400 weak_password`. Its `Debug` output hides
/// both passwords.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangePasswordRequest {
    pub current_password: String,
    pub new_password: String,
}

impl std::fmt::Debug for ChangePasswordRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChangePasswordRequest").field("current_password", &"<redacted>").field("new_password", &"<redacted>").finish()
    }
}

/// `PATCH /api/v1/devices/{id}` (device auth): renames one of the account's devices that isn't revoked; answered with
/// its [`DeviceInfo`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenameDeviceRequest {
    pub name: String,
}

// ---- two-factor sign-in (`/api/v1/account/two-factor/…`, `/api/v1/auth/login/second-factor`) ----
//
// The second factor is a TOTP code (`totp_code`, six digits) or a recovery code (`recovery_code`, 16 letters and
// digits, with dashes or without). A wrong one is `403 two_factor_invalid` on a signed-in route and `401` at sign-in and
// join (which are not signed in); too many wrong ones lock the account's second factor for a while (`429
// two_factor_locked` with `Retry-After`). Every type here that holds a secret hides it in `Debug`.

/// `POST /account/two-factor/setup`: the account's password starts setting two-factor sign-in up. Its `Debug` output
/// hides the password.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TwoFactorSetupRequest {
    pub password: String,
}

impl std::fmt::Debug for TwoFactorSetupRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TwoFactorSetupRequest").field("password", &"<redacted>").finish()
    }
}

/// `POST /account/two-factor/confirm`: the first code from the authenticator app turns two-factor sign-in on. Its
/// `Debug` output hides the code.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TwoFactorConfirmRequest {
    pub totp_code: String,
}

impl std::fmt::Debug for TwoFactorConfirmRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TwoFactorConfirmRequest").field("totp_code", &"<redacted>").finish()
    }
}

/// `POST /account/two-factor/disable` and `POST /account/two-factor/recovery-codes`: the password and one second
/// factor (`recovery-codes` takes `totp_code` only). Its `Debug` output hides all three.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TwoFactorProofRequest {
    pub password: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totp_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_code: Option<String>,
}

impl std::fmt::Debug for TwoFactorProofRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TwoFactorProofRequest")
            .field("password", &"<redacted>")
            .field("totp_code", &self.totp_code.as_ref().map(|_| "<redacted>"))
            .field("recovery_code", &self.recovery_code.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// `POST /auth/login/second-factor`: the challenge `two_factor_required` gave, and one second factor. Answered with a
/// [`LoginResponse`]. Its `Debug` output hides all of it.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecondFactorRequest {
    pub challenge: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totp_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_code: Option<String>,
}

impl std::fmt::Debug for SecondFactorRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecondFactorRequest")
            .field("challenge", &"<redacted>")
            .field("totp_code", &self.totp_code.as_ref().map(|_| "<redacted>"))
            .field("recovery_code", &self.recovery_code.as_ref().map(|_| "<redacted>"))
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

/// `POST /api/v1/chunks/claim`: add to the working space (the `craft-space` header) the chunks that `from_space` has, without
/// uploading their bytes. The caller needs at least read access to `from_space` and the role of an editor in the working space.
/// At most [`MAX_MISSING_IDS`] IDs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimRequest {
    pub from_space: SpaceId,
    pub ids: Vec<ChunkId>,
}

/// The answer to a claim: `claimed` are the IDs the working space has now (added by this call, or held before), `missing` the ones
/// neither space has, which the client uploads. Every asked ID is in exactly one of the two; a source space the caller may not read
/// is `404 not_found`, as for a space that does not exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimResponse {
    pub claimed: Vec<ChunkId>,
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

/// What the server knows of its own room, with `server_full`. Each number is left out when the server has none (a
/// store on S3 has no filesystem to measure; no `capacity` is set).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerRoom {
    /// Bytes still available on the store's filesystem (disk store only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_bytes: Option<u64>,
    /// Total size of the store's filesystem (disk store only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    /// What the admin said the store may hold (`capacity`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity_bytes: Option<u64>,
}

/// Every error answer. `error` is a stable code, one of [`codes`]; `message` is for people. `conflicts` is
/// set for `conflict`, `missing` for `missing_chunks`, `quota` for `quota_exceeded`, `server` for `server_full`,
/// `two_factor` for `two_factor_required`. Clients ignore codes and JSON
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
    /// Set for `server_full`: what the server knows of its room.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<ServerRoom>,
    /// Set for `two_factor_required`: the challenge to answer at `POST /auth/login/second-factor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub two_factor: Option<TwoFactorChallenge>,
}

/// Response header carrying the space's epoch on every signed-in request.
pub const EPOCH_HEADER: &str = "craft-epoch";

/// Request header naming the space a space-scoped request works on (a space UUID; absent: the device's personal space).
/// Every answer of a space-scoped route echoes it with the space the server worked on, also when the request had none.
pub const SPACE_HEADER: &str = "craft-space";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_stay_out_of_debug_output() {
        let req = LoginRequest { username: "ana".into(), password: "hunter2-secret".into(), device_name: "pc".into(), ..LoginRequest::default() };
        assert!(!format!("{req:?}").contains("hunter2-secret"));
        let res = LoginResponse { device: DeviceId::generate(), token: "tok-secret".into() };
        assert!(!format!("{res:?}").contains("tok-secret"));
        let join =
            JoinRequest { code: "K7M4Q9TX".into(), device_name: "pc".into(), password: Some("hunter2-secret".into()), ..JoinRequest::default() };
        let shown = format!("{join:?}");
        assert!(!shown.contains("K7M4Q9TX") && !shown.contains("hunter2-secret"), "{shown}");
        let created = PairingCreated { id: Uuid::nil(), code: "K7M4Q9TX".into(), expires_ms: 1 };
        assert!(!format!("{created:?}").contains("K7M4Q9TX"));
        let change = ChangePasswordRequest { current_password: "old-secret-pw".into(), new_password: "new-secret-pw".into() };
        let shown = format!("{change:?}");
        assert!(!shown.contains("old-secret-pw") && !shown.contains("new-secret-pw"), "{shown}");
        assert_eq!(shown.matches("<redacted>").count(), 2);
        let confirm = AdminConfirm {
            password: Some("admin-secret-pw".into()),
            totp_code: Some("918273".into()),
            recovery_code: Some("RECOVERYSECRET00".into()),
        };
        let shown = format!("{confirm:?}");
        assert!(!shown.contains("admin-secret-pw") && !shown.contains("918273") && !shown.contains("RECOVERYSECRET"), "{shown}");
        let reset =
            AdminPasswordReset { password: Some("admin-secret-pw".into()), totp_code: Some("918273".into()), ..AdminPasswordReset::default() };
        let shown = format!("{reset:?}");
        assert!(!shown.contains("admin-secret-pw") && !shown.contains("918273"), "{shown}");
        let create =
            AdminCreateUser { username: "ana".into(), role: "member".into(), password: Some("admin-secret-pw".into()), ..AdminCreateUser::default() };
        assert!(!format!("{create:?}").contains("admin-secret-pw"));
        let update = AdminUpdateUser { password: Some("admin-secret-pw".into()), ..AdminUpdateUser::default() };
        assert!(!format!("{update:?}").contains("admin-secret-pw"));
        let code = AdminCode { code: "ABCDEFGHJKMNPQRS".into(), join_url: Some("cloud.example.org/ABCD-EFGH-JKMN-PQRS".into()), expires_ms: 1 };
        let shown = format!("{code:?} {:?}", AdminCreatedUser { user: sample_user(), invite: code.clone() });
        assert!(!shown.contains("ABCDEFGH") && !shown.contains("cloud.example.org"), "{shown}");
    }

    #[test]
    fn two_factor_secrets_stay_out_of_debug_output() {
        let setup = TwoFactorSetupResponse {
            secret: "JBSWY3DPEHPK3PXP".into(),
            otpauth_uri: "otpauth://totp/Craft%20Cloud:ana?secret=JBSWY3DPEHPK3PXP".into(),
            expires_ms: 5,
        };
        let shown = format!("{setup:?}");
        assert!(!shown.contains("JBSWY3DP") && !shown.contains("otpauth:"), "{shown}");
        let codes = RecoveryCodes { recovery_codes: vec!["ABCD-EFGH-JKMN-PQRS".into(), "WXYZ-0123-4567-89AB".into()] };
        let shown = format!("{codes:?}");
        assert!(!shown.contains("ABCD") && !shown.contains("WXYZ"), "{shown}");
        let second =
            SecondFactorRequest { challenge: "challenge-secret".into(), totp_code: Some("123456".into()), recovery_code: Some("RC-SECRET".into()) };
        let shown = format!("{second:?}");
        assert!(!shown.contains("challenge-secret") && !shown.contains("123456") && !shown.contains("RC-SECRET"), "{shown}");
        let proof = TwoFactorProofRequest { password: "pw-secret-1".into(), totp_code: Some("654321".into()), recovery_code: None };
        let shown = format!("{proof:?}");
        assert!(!shown.contains("pw-secret-1") && !shown.contains("654321"), "{shown}");
        assert!(!format!("{:?}", TwoFactorSetupRequest { password: "pw-secret-1".into() }).contains("pw-secret-1"));
        assert!(!format!("{:?}", TwoFactorConfirmRequest { totp_code: "654321".into() }).contains("654321"));
        let join = JoinRequest {
            code: "K7M4Q9TX".into(),
            totp_code: Some("111222".into()),
            recovery_code: Some("RC-SECRET".into()),
            ..JoinRequest::default()
        };
        let shown = format!("{join:?}");
        assert!(!shown.contains("111222") && !shown.contains("RC-SECRET"), "{shown}");
        let body = ErrorBody {
            error: codes::TWO_FACTOR_REQUIRED.into(),
            message: "m".into(),
            two_factor: Some(TwoFactorChallenge { challenge: "challenge-secret".into(), expires_ms: 1 }),
            ..ErrorBody::default()
        };
        assert!(!format!("{body:?}").contains("challenge-secret"));
        // The challenge travels in the error body, and an older client that doesn't know the field ignores it.
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["two_factor"], serde_json::json!({ "challenge": "challenge-secret", "expires_ms": 1 }));
        assert_eq!(serde_json::from_value::<ErrorBody>(json).unwrap(), body);
    }

    fn sample_user() -> AdminUserInfo {
        AdminUserInfo {
            id: Uuid::nil(),
            username: "ana".into(),
            role: "member".into(),
            created_ms: 1,
            disabled_ms: None,
            two_factor_enabled: false,
            devices_active: 0,
            stored_bytes: 0,
            quota_bytes: None,
            invite_pending: false,
        }
    }

    #[test]
    fn an_update_tells_a_missing_quota_from_a_null_one() {
        let missing: AdminUpdateUser = serde_json::from_value(serde_json::json!({ "role": "admin", "password": "p" })).unwrap();
        assert_eq!(missing.quota_bytes, None);
        let null: AdminUpdateUser = serde_json::from_value(serde_json::json!({ "quota_bytes": null })).unwrap();
        assert_eq!(null.quota_bytes, Some(None));
        let set: AdminUpdateUser = serde_json::from_value(serde_json::json!({ "quota_bytes": 5_000_000 })).unwrap();
        assert_eq!(set.quota_bytes, Some(Some(5_000_000)));
        assert!(serde_json::from_value::<AdminUpdateUser>(serde_json::json!({ "quota_bytes": -1 })).is_err());
        assert_eq!(serde_json::to_value(&set).unwrap(), serde_json::json!({ "quota_bytes": 5_000_000 }));
    }

    #[test]
    fn an_admin_body_ignores_fields_it_does_not_know() {
        let confirm: AdminConfirm = serde_json::from_value(serde_json::json!({ "password": "p", "totp_code": "123456", "later": 1 })).unwrap();
        assert_eq!((confirm.password.as_deref(), confirm.totp_code.as_deref(), confirm.recovery_code), (Some("p"), Some("123456"), None));
        assert_eq!(serde_json::from_value::<AdminConfirm>(serde_json::json!({})).unwrap().password, None);
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
        assert_eq!((join.client_kind, join.platform, join.app_version), (None, None, None));
        let login = LoginRequest { username: "ana".into(), password: "pw".into(), device_name: "pc".into(), ..LoginRequest::default() };
        assert_eq!(serde_json::to_value(&login).unwrap(), serde_json::json!({ "username": "ana", "password": "pw", "device_name": "pc" }));
        assert_eq!(codes::INVALID_CODE, "invalid_code");
        assert_eq!(codes::PASSWORD_REQUIRED, "password_required");
        assert_eq!(codes::WEAK_PASSWORD, "weak_password");
    }

    #[test]
    fn audit_bodies_have_a_plain_json_form() {
        let details: AuditDetails = serde_json::from_str(r#"{"revoked":2,"a_later_field":"x","quota_bytes":null}"#).unwrap();
        assert_eq!(details.revoked, Some(2));
        assert_eq!(details.quota_bytes, Some(None));
        assert_eq!(serde_json::to_string(&details).unwrap(), r#"{"quota_bytes":null,"revoked":2}"#);
        assert_eq!(serde_json::from_str::<AuditDetails>("{}").unwrap(), AuditDetails::default());
        assert_eq!(serde_json::to_string(&AuditDetails::default()).unwrap(), "{}");
        let end = AuditExportEnd { complete: Some(true), truncated: None, exported: 3 };
        assert_eq!(serde_json::to_string(&end).unwrap(), r#"{"complete":true,"exported":3}"#);
        let event = AuditEvent {
            id: Uuid::nil(),
            at_ms: 5,
            actor_user: None,
            actor_device: None,
            action: "auth.login_failed".into(),
            target_kind: String::new(),
            target_id: None,
            outcome: "failure".into(),
            details: AuditDetails { step: Some("password".into()), ..AuditDetails::default() },
            count: 4,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["actor_user"], serde_json::Value::Null);
        assert_eq!(json["count"], 4);
        assert_eq!(serde_json::from_value::<AuditEvent>(json).unwrap(), event);
    }

    #[test]
    fn error_bodies_omit_empty_extras() {
        let body = ErrorBody { error: "not_found".into(), message: "no such chunk".into(), ..ErrorBody::default() };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json, serde_json::json!({ "error": "not_found", "message": "no such chunk" }));
        assert_eq!(serde_json::from_value::<ErrorBody>(json).unwrap(), body);
    }

    #[test]
    fn account_usage_round_trips_and_ignores_unknown_fields() {
        let response = AccountUsage {
            stored_bytes: 17,
            quota_bytes: Some(99),
            trash_bytes: None,
            history_bytes: None,
            estimates: UsageEstimates { index_complete: false, reason: Some(UsageEstimateReason::IndexIncomplete) },
        };
        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(serde_json::from_value::<AccountUsage>(value.clone()).unwrap(), response);
        let mut value = value;
        value.as_object_mut().unwrap().insert("future_field".into(), serde_json::json!(1));
        assert_eq!(serde_json::from_value::<AccountUsage>(value).unwrap(), response);
    }

    #[test]
    fn account_usage_decodes_complete_and_incomplete_estimates() {
        let complete: AccountUsage =
            serde_json::from_str(r#"{"stored_bytes":11,"quota_bytes":null,"trash_bytes":0,"history_bytes":0,"estimates":{"index_complete":true}}"#)
                .unwrap();
        assert_eq!(complete.estimates, UsageEstimates { index_complete: true, reason: None });
        let incomplete: AccountUsage = serde_json::from_str(
            r#"{"stored_bytes":11,"quota_bytes":null,"trash_bytes":null,"history_bytes":null,"estimates":{"index_complete":false,"reason":"index_incomplete"}}"#,
        ).unwrap();
        assert_eq!(incomplete.trash_bytes, None);
        assert_eq!(incomplete.history_bytes, None);
        assert_eq!(incomplete.estimates.reason, Some(UsageEstimateReason::IndexIncomplete));
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
    fn a_server_full_answer_says_what_the_server_knows_of_its_room() {
        let body = ErrorBody {
            error: codes::SERVER_FULL.into(),
            message: "m".into(),
            server: Some(ServerRoom { free_bytes: Some(5), total_bytes: Some(100), capacity_bytes: None }),
            ..ErrorBody::default()
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["server"], serde_json::json!({ "free_bytes": 5, "total_bytes": 100 }), "unknown numbers are left out");
        assert_eq!(serde_json::from_value::<ErrorBody>(json).unwrap(), body);
        // An answer without the field (an older server, or another code) reads.
        let plain: ErrorBody = serde_json::from_value(serde_json::json!({ "error": codes::SERVER_FULL, "message": "m" })).unwrap();
        assert_eq!(plain.server, None);
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
