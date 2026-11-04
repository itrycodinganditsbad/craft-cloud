//! Request and response bodies of the HTTP API (`/api/v1/…`), shared by the server and the sync agent.
//! Batch commits use [`crate::Op`], [`crate::Conflict`] and [`crate::ChangeSet`] from `ops`.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::hash::ChunkId;
use crate::ids::{DeviceId, Epoch, NodeId, SpaceId};
use crate::ops::{Conflict, Op};

/// What apps see of these bodies lives in `craft-cloud-api` (no postcard), re-exported here.
pub use craft_cloud_api::api::{AccountInfo, DeviceInfo, ErrorAnswer, PairingState, PairingStatus, codes};

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
/// that has none yet. `client_kind`, `platform` and `app_version` are as in [`LoginRequest`]. Its `Debug` output hides
/// the code and the password.
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

/// `POST /api/v1/devices/revoke-others` (device auth): how many other devices were signed out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevokedOthers {
    pub revoked: u32,
}

// ---- the admin API (`/api/v1/admin/…`, an admin's device token) ----
//
// Every change asks for a fresh confirmation in its body: the acting admin's `password`. A device token never expires,
// so a stolen admin token alone must not be enough. A missing or mistyped field is `400 invalid`, a wrong password `403
// wrong_password` (never 401). A later release adds optional `totp_code` and `recovery_code` fields to the same bodies
// for an admin with two-factor sign-in; a body without them keeps working for an admin without it, and fields a server
// does not know are ignored, so nothing here has to change for that.

/// The body of an admin change that needs nothing but the confirmation: `POST …/disable`, `POST …/enable`,
/// `DELETE /admin/users/{id}`, `POST` and `DELETE …/invite`, `POST …/password-reset` and `DELETE …/devices/{id}`. Its
/// `Debug` output hides the password.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminConfirm {
    /// The acting admin's current password. Missing: `400 invalid`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

impl std::fmt::Debug for AdminConfirm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminConfirm").field("password", &self.password.as_ref().map(|_| "<redacted>")).finish()
    }
}

/// `GET /admin/server`: counters and kinds, nothing about paths, addresses or settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminServerInfo {
    pub version: String,
    /// Accounts that are not marked for removal.
    pub users: u64,
    /// Devices that are not revoked.
    pub devices_active: u64,
    pub stored_bytes: u64,
    pub live_bytes: u64,
    /// `disk`, `s3` or `memory`.
    pub storage_kind: String,
    /// `sqlite` or `postgres`.
    pub database_kind: String,
}

/// An account in the admin API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminUserInfo {
    pub id: Uuid,
    pub username: String,
    /// `admin` or `member`.
    pub role: String,
    pub created_ms: i64,
    /// When an admin disabled it (`null`: enabled).
    pub disabled_ms: Option<i64>,
    /// Always `false` until the server has two-factor sign-in.
    pub two_factor_enabled: bool,
    /// Devices that are not revoked.
    pub devices_active: u64,
    pub stored_bytes: u64,
    /// The quota an admin set for the account (`null`: the server-wide cap, if any, applies).
    pub quota_bytes: Option<u64>,
    /// An unexpired invite is open.
    pub invite_pending: bool,
}

/// `GET /admin/users?cursor&limit&query`: a page of accounts by username.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminUserList {
    pub users: Vec<AdminUserInfo>,
    /// Opaque; give it back as `cursor` for the next page. Missing on the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// `POST /admin/users`: a new account without a password, for an invite. Its `Debug` output hides the password.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminCreateUser {
    pub username: String,
    /// `admin` or `member`.
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

impl std::fmt::Debug for AdminCreateUser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminCreateUser")
            .field("username", &self.username)
            .field("role", &self.role)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// A join code the admin made, shown once. `join_url` is the whole join string when the server knows its public
/// address, and missing otherwise. Its `Debug` output hides the code and the join string.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminCode {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join_url: Option<String>,
    pub expires_ms: i64,
}

impl std::fmt::Debug for AdminCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminCode")
            .field("code", &"<redacted>")
            .field("join_url", &self.join_url.as_ref().map(|_| "<redacted>"))
            .field("expires_ms", &self.expires_ms)
            .finish()
    }
}

/// The answer to `POST /admin/users`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminCreatedUser {
    pub user: AdminUserInfo,
    pub invite: AdminCode,
}

/// `PATCH /admin/users/{id}`: a new role and/or quota. At least one of them; a missing `quota_bytes` leaves the quota,
/// `null` removes it. Its `Debug` output hides the password.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminUpdateUser {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub quota_bytes: Option<Option<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

/// A field that is there, `null` or not: `Some(None)` for `null`, `Some(Some(x))` for a value (a missing field is
/// `None`, from `#[serde(default)]`).
fn present<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

impl std::fmt::Debug for AdminUpdateUser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminUpdateUser")
            .field("role", &self.role)
            .field("quota_bytes", &self.quota_bytes)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// `GET /admin/users/{id}/devices`: every device of the account, revoked ones with when and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminDevices {
    pub devices: Vec<DeviceInfo>,
}

/// The typed values of a security event (`details`). Every field is a closed list of words or a number: nothing a client
/// typed is ever in it. Fields an event does not have are missing, and fields a later release adds are ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditDetails {
    /// `agent`, `desktop` or `cli`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_kind: Option<String>,
    /// `windows`, `macos`, `linux`, `android`, `ios` or `other`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// Why a device was signed out: `user`, `others`, `password`, `admin`, `disabled`, `two_factor`, `idle` or `reset`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// How far a failed sign-in got: `password`, `totp`, `recovery` or `challenge_expired`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    /// `admin` or `member`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_to: Option<String>,
    /// The quota an admin set: `null` when they removed it, missing when the event is not about a quota.
    #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub quota_bytes: Option<Option<u64>>,
    /// How many devices an event signed out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked: Option<u32>,
    /// The device an admin or the command line signed out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<DeviceId>,
    /// Set when the values were too long for the log and whole fields were left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
}

/// One security event, as `GET /account/activity`, `GET /admin/audit` and the export show it. No address, name of a file or
/// device, or secret is ever in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEvent {
    pub id: Uuid,
    pub at_ms: i64,
    /// The account that acted; `null` for the command line, and for a sign-in that failed (nobody proved who they were).
    pub actor_user: Option<Uuid>,
    pub actor_device: Option<DeviceId>,
    /// A name from the closed list in `docs/design/sync.md`, like `auth.login`.
    pub action: String,
    /// `user` or `device`, or empty.
    pub target_kind: String,
    pub target_id: Option<Uuid>,
    /// `success` or `failure`.
    pub outcome: String,
    pub details: AuditDetails,
    /// How many tries the event stands for: failed sign-ins are one event per account and minute. 1 for every other event.
    pub count: u64,
}

/// A page of `GET /account/activity` or `GET /admin/audit`, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditPage {
    pub events: Vec<AuditEvent>,
    /// Opaque; give it back as `cursor` for the next (older) page. Missing on the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// The last line of `GET /admin/audit/export` (the lines before it are [`AuditEvent`]s). `complete` says the export holds
/// every event up to the moment it started; `truncated` says there were more than the export holds. A stream that ends
/// without either line was cut off and is not a complete export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditExportEnd {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub complete: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    /// The events before this line.
    pub exported: u64,
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
        let confirm = AdminConfirm { password: Some("admin-secret-pw".into()) };
        assert!(!format!("{confirm:?}").contains("admin-secret-pw"));
        let create = AdminCreateUser { username: "ana".into(), role: "member".into(), password: Some("admin-secret-pw".into()) };
        assert!(!format!("{create:?}").contains("admin-secret-pw"));
        let update = AdminUpdateUser { password: Some("admin-secret-pw".into()), ..AdminUpdateUser::default() };
        assert!(!format!("{update:?}").contains("admin-secret-pw"));
        let code = AdminCode { code: "ABCDEFGHJKMNPQRS".into(), join_url: Some("cloud.example.org/ABCD-EFGH-JKMN-PQRS".into()), expires_ms: 1 };
        let shown = format!("{code:?} {:?}", AdminCreatedUser { user: sample_user(), invite: code.clone() });
        assert!(!shown.contains("ABCDEFGH") && !shown.contains("cloud.example.org"), "{shown}");
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
        // A later release adds `totp_code` here, and today's server reads such a body as it reads one without it.
        let confirm: AdminConfirm = serde_json::from_value(serde_json::json!({ "password": "p", "totp_code": "123456" })).unwrap();
        assert_eq!(confirm.password.as_deref(), Some("p"));
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
