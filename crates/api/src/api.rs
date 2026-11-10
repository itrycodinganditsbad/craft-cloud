//! The parts of the HTTP API's bodies (`/api/v1/…`) that apps see through the agent's local API: the error codes, the
//! error answer's code and message, the account's devices and add-a-computer codes, and the admin API's bodies (which the
//! agent passes through for the Craft Cloud app's admin pages). The rest of the HTTP API (commit
//! batches, manifests, the full error body) is the server's and the agent's, in `craft_cloud_proto::api`, which
//! re-exports these.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ids::{DeviceId, Epoch, NodeId, SpaceId};

/// Stable error codes, the `error` field of an error answer ([`ErrorAnswer`]).
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
    /// `join`: the code is wrong, expired or used (never which).
    pub const INVALID_CODE: &str = "invalid_code";
    /// `join`: an invite to an account without a password needs one; send it again with `password`.
    pub const PASSWORD_REQUIRED: &str = "password_required";
    /// `join`: the new password breaks the server's rules (the message says which).
    pub const WEAK_PASSWORD: &str = "weak_password";
    /// A signed-in route that asks for the account's password (changing it) got a wrong one (HTTP 403, never 401: a
    /// 401 on a signed-in route means the device was revoked, and the agent signs out on it). The local API's
    /// `login` uses the same code when the server refuses the username or password.
    pub const WRONG_PASSWORD: &str = "wrong_password";
    /// The device is signed in but may not do this (HTTP 403): a member calling an `/admin` route, or an admin doing
    /// what their account may not. The local API refuses with it a request that didn't come from a local program.
    pub const FORBIDDEN: &str = "forbidden";
    /// A sign-in or a join with the right password or code, for an account an admin disabled or marked for removal
    /// (HTTP 403). Said only after the password or code was right, never to a guesser.
    pub const ACCOUNT_DISABLED: &str = "account_disabled";
    /// An admin change would leave the server without an enabled administrator (HTTP 409).
    pub const LAST_ADMIN: &str = "last_admin";
    /// A change to a team space's members would leave it without an owner (HTTP 409).
    pub const LAST_OWNER: &str = "last_owner";
    /// The account is a member of the team space but its role does not allow this (HTTP 403): a viewer writing, an editor
    /// removing from the trash. The agent switches that space to read-only on it. Never 401, which reads as "signed out".
    pub const READ_ONLY: &str = "read_only";
    /// A password sign-in (or the redeeming of an invite or reset code for an account with two-factor sign-in) was
    /// right, but needs a second factor (HTTP 401, no token). The answer's `two_factor` carries the challenge.
    pub const TWO_FACTOR_REQUIRED: &str = "two_factor_required";
    /// A code or recovery code was wrong, or a code was used before (HTTP 401 at sign-in and join, which are not
    /// signed in, and 403 on a signed-in route, where a 401 would read as "this device was revoked").
    pub const TWO_FACTOR_INVALID: &str = "two_factor_invalid";
    /// The server has no key for two-factor secrets (`[two_factor] key_file`), so it can't set it up (HTTP 503).
    pub const TWO_FACTOR_UNAVAILABLE: &str = "two_factor_unavailable";
    /// A set-up is running on another device (HTTP 409).
    pub const TWO_FACTOR_PENDING: &str = "two_factor_pending";
    /// Too many wrong codes: every check of this account's second factor is refused until `Retry-After` is over
    /// (HTTP 429). A lock never lasts for good.
    pub const TWO_FACTOR_LOCKED: &str = "two_factor_locked";
    pub const INTERNAL: &str = "internal";
}

/// What a client reads of every error answer: `error`, a stable code (one of [`codes`], or the local API's), and
/// `message`, for people. The local trash restore call also keeps `name_taken` when a server `conflict` includes that
/// cause; any other server-only fields are ignored (the full body is `craft_cloud_proto::api::ErrorBody`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ErrorAnswer {
    pub error: String,
    pub message: String,
    /// Whether the server's conflict details say that no available restored name remains.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_taken: Option<bool>,
}

/// The second step of a sign-in: what `two_factor_required` carries. `challenge` is a secret, good once and until
/// `expires_ms` (milliseconds since the Unix epoch, by the server's clock); the code or recovery code goes with it to
/// `POST /auth/login/second-factor`. Its `Debug` output hides the challenge. For `POST /join` there is no challenge
/// (the code is simply sent again with the second factor): `challenge` is empty there.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TwoFactorChallenge {
    pub challenge: String,
    pub expires_ms: i64,
}

impl std::fmt::Debug for TwoFactorChallenge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TwoFactorChallenge").field("challenge", &"<redacted>").field("expires_ms", &self.expires_ms).finish()
    }
}

/// The answer to a set-up: the secret as Base32 without padding and as an `otpauth://` URI (for a QR code), good until
/// `expires_ms`. Shown once. Its `Debug` output hides both.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TwoFactorSetupResponse {
    pub secret: String,
    pub otpauth_uri: String,
    pub expires_ms: i64,
}

impl std::fmt::Debug for TwoFactorSetupResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TwoFactorSetupResponse")
            .field("secret", &"<redacted>")
            .field("otpauth_uri", &"<redacted>")
            .field("expires_ms", &self.expires_ms)
            .finish()
    }
}

/// Ten recovery codes, shown once (in groups of four, like `ABCD-EFGH-JKMN-PQRS`; any case, with or without dashes is
/// accepted back). Each works once. Its `Debug` output hides them.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryCodes {
    pub recovery_codes: Vec<String>,
}

impl std::fmt::Debug for RecoveryCodes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecoveryCodes").field("recovery_codes", &format_args!("<{} redacted>", self.recovery_codes.len())).finish()
    }
}

/// `GET /account/two-factor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TwoFactorStatus {
    pub enabled: bool,
    /// When it was turned on.
    #[serde(default)]
    pub enabled_ms: Option<i64>,
    /// Recovery codes not used yet (0 while it is off).
    pub recovery_codes_left: u32,
    /// A set-up is running (it ends after ten minutes or five wrong codes).
    pub pending: bool,
}

/// `POST /api/v1/devices/revoke-others` (device auth): how many other devices were signed out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevokedOthers {
    pub revoked: u32,
}

/// Where an add-a-computer code stands, in [`PairingStatus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairingState {
    /// Not used yet, and still good.
    Waiting,
    /// A computer joined with it; [`PairingStatus::device_name`] names it.
    Used,
    /// Its time ran out unused.
    Expired,
}

/// `GET /api/v1/pairings/{id}` (device auth, the account's own pairings only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingStatus {
    pub state: PairingState,
    /// The device that joined, once `used`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
}

/// One of the signed-in user's devices. The fields after `current` are newer than the first release of the API: an
/// older server leaves them out, and a client that doesn't know them ignores them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub id: DeviceId,
    pub name: String,
    pub created_ms: i64,
    pub last_used_ms: i64,
    pub revoked: bool,
    /// The device making this request.
    pub current: bool,
    /// What the device said about its client when it signed in: `agent`, `desktop` or `cli`, or empty. Display only.
    #[serde(default)]
    pub client_kind: String,
    /// The platform the device said it runs on, free text, empty when it didn't say.
    #[serde(default)]
    pub platform: String,
    /// The version of the client the device said it runs, free text, empty when it didn't say.
    #[serde(default)]
    pub app_version: String,
    /// When the device was revoked, milliseconds since the Unix epoch, by the server's clock.
    #[serde(default)]
    pub revoked_ms: Option<i64>,
    /// Why it was revoked: `user`, `others`, `password`, `admin`, `disabled`, `two_factor`, `idle` or `reset`. A client
    /// treats a reason it doesn't know as "revoked".
    #[serde(default)]
    pub revoke_reason: Option<String>,
}

/// `GET /api/v1/account` (device auth): the signed-in account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountInfo {
    pub user_id: Uuid,
    /// The username, in lowercase.
    pub username: String,
    /// `admin` or `member`.
    pub role: String,
    pub created_ms: i64,
    /// When the password last changed through the server; `None` while it is still the first one.
    #[serde(default)]
    pub password_changed_ms: Option<i64>,
    /// Whether signing in asks for a second factor.
    #[serde(default)]
    pub two_factor_enabled: bool,
}

// ---- the admin API (`/api/v1/admin/…`, an admin's device token) ----
//
// Every change asks for a fresh confirmation in its body: the acting admin's `password`. A device token never expires,
// so a stolen admin token alone must not be enough. A missing or mistyped field is `400 invalid`, a wrong password `403
// wrong_password` (never 401). An admin with two-factor sign-in adds `totp_code` (or `recovery_code`) to the same
// bodies; a body without them keeps working for an admin without it, and fields a server does not know are ignored.

/// The body of an admin change that needs nothing but the confirmation: `POST …/disable`, `POST …/enable`,
/// `DELETE /admin/users/{id}`, `POST` and `DELETE …/invite`, `POST …/reset-two-factor` and `DELETE …/devices/{id}`. Its
/// `Debug` output hides the password and the second factor.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminConfirm {
    /// The acting admin's current password. Missing: `400 invalid`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// The acting admin's code, when their account has two-factor sign-in. Missing or wrong then: `403
    /// two_factor_invalid`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totp_code: Option<String>,
    /// A recovery code instead of `totp_code`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_code: Option<String>,
}

impl std::fmt::Debug for AdminConfirm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminConfirm")
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("totp_code", &self.totp_code.as_ref().map(|_| "<redacted>"))
            .field("recovery_code", &self.recovery_code.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// `POST /admin/users/{id}/password-reset`: the confirmation as in [`AdminConfirm`], and whether redeeming the code also
/// turns the account's two-factor sign-in off (`reset_two_factor`, default `false`: an account that has it then needs
/// its second factor to redeem the code). Its `Debug` output hides the password and the second factor.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminPasswordReset {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totp_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_code: Option<String>,
    #[serde(default)]
    pub reset_two_factor: bool,
}

impl std::fmt::Debug for AdminPasswordReset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminPasswordReset")
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("totp_code", &self.totp_code.as_ref().map(|_| "<redacted>"))
            .field("recovery_code", &self.recovery_code.as_ref().map(|_| "<redacted>"))
            .field("reset_two_factor", &self.reset_two_factor)
            .finish()
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

/// `GET /admin/storage`: server-wide storage counts and the ten largest accounts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminStorageOverview {
    pub accounts: u64,
    pub stored_bytes: u64,
    pub default_quota_bytes: Option<u64>,
    pub accounts_with_override: u64,
    /// Accounts at or above 90% of their effective quota.
    pub accounts_near_quota: u64,
    /// Accounts at or above their effective quota.
    pub accounts_over_quota: u64,
    pub top_accounts: Vec<AdminStorageAccount>,
    pub manifest_index: AdminManifestIndex,
}

/// One account in [`AdminStorageOverview`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminStorageAccount {
    pub id: Uuid,
    pub username: String,
    pub stored_bytes: u64,
    pub quota_bytes: Option<u64>,
}

/// Global manifest-index counts from the metadata database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminManifestIndex {
    pub complete: u64,
    pub building: u64,
    pub not_indexed: u64,
}

/// `GET /account/usage` and `GET /admin/users/{id}/usage`: stored bytes and labeled estimates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountUsage {
    pub stored_bytes: u64,
    pub quota_bytes: Option<u64>,
    pub trash_bytes: Option<u64>,
    pub history_bytes: Option<u64>,
    pub estimates: UsageEstimates,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageEstimates {
    pub index_complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<UsageEstimateReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageEstimateReason {
    IndexIncomplete,
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
    /// Whether the account has two-factor sign-in on.
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totp_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_code: Option<String>,
}

impl std::fmt::Debug for AdminCreateUser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminCreateUser")
            .field("username", &self.username)
            .field("role", &self.role)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("totp_code", &self.totp_code.as_ref().map(|_| "<redacted>"))
            .field("recovery_code", &self.recovery_code.as_ref().map(|_| "<redacted>"))
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totp_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_code: Option<String>,
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
            .field("totp_code", &self.totp_code.as_ref().map(|_| "<redacted>"))
            .field("recovery_code", &self.recovery_code.as_ref().map(|_| "<redacted>"))
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
    /// `admin` or `member` for an account's role, `viewer`, `editor` or `owner` for a role in a team space.
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
    /// Set (to `true`) when redeeming a password reset code also turned the account's two-factor sign-in off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub two_factor_removed: Option<bool>,
    /// The account a team space event is about (the invited, changed or removed member, or the first owner).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<Uuid>,
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

/// What an account may do in a team space, from least to most: a viewer reads, an editor also writes, an owner also
/// manages the members. The order is the permission order, so `role >= SpaceRole::Editor` reads as "may write".
/// A personal space's owner has `Owner` in it implicitly. The words are the ones the `space_members` table and the
/// API use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpaceRole {
    Viewer,
    Editor,
    Owner,
}

impl SpaceRole {
    /// Every role, least permission first.
    pub const ALL: [SpaceRole; 3] = [SpaceRole::Viewer, SpaceRole::Editor, SpaceRole::Owner];

    pub fn as_str(self) -> &'static str {
        match self {
            SpaceRole::Viewer => "viewer",
            SpaceRole::Editor => "editor",
            SpaceRole::Owner => "owner",
        }
    }

    /// The role with this exact word.
    pub fn parse(word: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|role| role.as_str() == word)
    }

    /// Whether the role may change files (commit, restore, upload).
    pub fn may_write(self) -> bool {
        self >= SpaceRole::Editor
    }
}

/// The confirmation an administrator's change carries: the password, and the second factor when the account has one (see
/// [`AdminConfirm`]). The space routes flatten it into their bodies, where an owner acting by their own role leaves it out.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceConfirm {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totp_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_code: Option<String>,
}

impl std::fmt::Debug for SpaceConfirm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpaceConfirm")
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("totp_code", &self.totp_code.as_ref().map(|_| "<redacted>"))
            .field("recovery_code", &self.recovery_code.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// One space in `GET /spaces`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceListItem {
    pub id: SpaceId,
    /// `personal` or `team`.
    pub kind: String,
    /// The team space's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub role: SpaceRole,
    /// `active`, or `invited` for an invitation not yet accepted.
    pub state: String,
    pub archived: bool,
    pub created_ms: i64,
    /// Accepted members.
    pub members: u64,
    /// Not given for an invitation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<NodeId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<Epoch>,
    /// When the account accepted its membership of a team space (milliseconds). Someone who is removed and invited again
    /// gets a new one, so a client can tell that its access was interrupted. Not given for a personal space or an
    /// invitation, nor by older servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub joined_ms: Option<i64>,
}

/// `GET /spaces`: the personal space first, then the team spaces by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceList {
    pub spaces: Vec<SpaceListItem>,
}

/// A member of a team space, or an account invited to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceMember {
    pub user: Uuid,
    pub username: String,
    pub role: SpaceRole,
    /// `active` or `invited`.
    pub state: String,
    /// The palette index (0 to 11) the account is shown in, the same on every machine.
    pub colour: u8,
}

/// `GET /spaces/{id}/members`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceMembers {
    pub members: Vec<SpaceMember>,
}

/// `POST /spaces/{id}/members`: invite an account. An owner leaves the confirmation out; an administrator who is not an
/// owner gives it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddSpaceMember {
    pub username: String,
    pub role: SpaceRole,
    #[serde(flatten)]
    pub confirm: SpaceConfirm,
}

/// `PATCH /spaces/{id}/members/{user}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeSpaceMember {
    pub role: SpaceRole,
    #[serde(flatten)]
    pub confirm: SpaceConfirm,
}

/// `GET /spaces/{id}/usage`: as `/account/usage` for the space, with what its live documents hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceUsage {
    pub stored_bytes: u64,
    pub quota_bytes: Option<u64>,
    pub trash_bytes: Option<u64>,
    pub history_bytes: Option<u64>,
    pub live_bytes: u64,
    pub estimates: UsageEstimates,
}

/// `PATCH /spaces/{id}`: a new name (an owner, or an administrator with the confirmation) and/or a new limit (an
/// administrator, with the confirmation; `null` removes the space's own limit).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateSpace {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub quota_bytes: Option<Option<u64>>,
    #[serde(flatten)]
    pub confirm: SpaceConfirm,
}

/// `POST /spaces/{id}/archive` and `/unarchive` with the owner's password, or an administrator's confirmation.
pub type SpaceArchive = SpaceConfirm;

/// `POST /admin/spaces`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminCreateSpace {
    pub name: String,
    /// The first owner's account ID: added as an accepted owner.
    pub owner: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_bytes: Option<u64>,
    #[serde(flatten)]
    pub confirm: SpaceConfirm,
}

/// `PUT /admin/spaces/{id}/owner`: make an account an accepted owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminSetSpaceOwner {
    pub user: Uuid,
    #[serde(flatten)]
    pub confirm: SpaceConfirm,
}

/// A team space in the administrator's list: counts and sizes, never content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminSpaceInfo {
    pub id: SpaceId,
    pub name: String,
    pub created_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_ms: Option<i64>,
    pub members: u64,
    pub owners: u64,
    pub stored_bytes: u64,
    pub quota_bytes: Option<u64>,
}

/// `GET /admin/spaces`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminSpaceList {
    pub spaces: Vec<AdminSpaceInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// The names of the security events, the closed list the server writes and `GET /admin/audit` filters by (`docs/design/sync.md`,
/// "Security audit log"). A filter is one of these, or a prefix of some of them that ends in a dot, like `auth.`.
pub const AUDIT_ACTIONS: [&str; 45] = [
    "auth.login",
    "auth.login_failed",
    "auth.second_factor_failed",
    "auth.recovery_code_used",
    "auth.two_factor_locked",
    "device.revoked",
    "device.revoked_others",
    "account.password_changed",
    "account.two_factor_enabled",
    "account.two_factor_disabled",
    "account.recovery_codes_regenerated",
    "admin.user_created",
    "admin.user_disabled",
    "admin.user_enabled",
    "admin.user_removed",
    "admin.role_changed",
    "admin.quota_changed",
    "admin.invite_issued",
    "admin.invite_revoked",
    "admin.password_reset_issued",
    "admin.two_factor_reset",
    "admin.device_revoked",
    "join.invite_redeemed",
    "join.reset_redeemed",
    "join.pairing_redeemed",
    "cli.password_reset",
    "cli.two_factor_reset",
    "cli.device_revoked",
    "cli.devices_revoked_all",
    "cli.user_removed",
    "space.created",
    "space.renamed",
    "space.archived",
    "space.unarchived",
    "space.quota_changed",
    "space.member_invited",
    "space.member_joined",
    "space.member_declined",
    "space.member_role_changed",
    "space.member_removed",
    "space.member_left",
    "cli.space_created",
    "cli.space_member_changed",
    "cli.space_archived",
    "cli.live_reset",
];

/// Whether `text` is an audit filter the server takes for `action`: an event name, or a prefix ending in a dot that at least
/// one name starts with. Anything else is refused by the server as `400 invalid`; checking here keeps it from being sent.
pub fn valid_audit_action(text: &str) -> bool {
    AUDIT_ACTIONS.contains(&text)
        || (text.ends_with('.')
            && text.bytes().all(|b| b.is_ascii_lowercase() || b == b'_' || b == b'.')
            && AUDIT_ACTIONS.iter().any(|a| a.starts_with(text)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_device_from_an_older_server_reads_with_empty_new_fields() {
        let id = DeviceId::generate();
        let json = serde_json::json!({ "id": id, "name": "pc", "created_ms": 1, "last_used_ms": 2, "revoked": false, "current": true });
        let device: DeviceInfo = serde_json::from_value(json).unwrap();
        assert_eq!((device.client_kind.as_str(), device.platform.as_str(), device.app_version.as_str()), ("", "", ""));
        assert_eq!((device.revoked_ms, device.revoke_reason), (None, None));
    }

    #[test]
    fn the_new_error_codes_are_stable() {
        assert_eq!(codes::WRONG_PASSWORD, "wrong_password");
        assert_eq!(codes::FORBIDDEN, "forbidden");
        assert_eq!(codes::ACCOUNT_DISABLED, "account_disabled");
        assert_eq!(codes::LAST_ADMIN, "last_admin");
        assert_eq!(codes::LAST_OWNER, "last_owner");
        assert_eq!(codes::READ_ONLY, "read_only");
    }

    #[test]
    fn the_two_factor_codes_are_stable_and_a_challenge_hides_itself() {
        assert_eq!(codes::TWO_FACTOR_REQUIRED, "two_factor_required");
        assert_eq!(codes::TWO_FACTOR_INVALID, "two_factor_invalid");
        assert_eq!(codes::TWO_FACTOR_UNAVAILABLE, "two_factor_unavailable");
        assert_eq!(codes::TWO_FACTOR_PENDING, "two_factor_pending");
        assert_eq!(codes::TWO_FACTOR_LOCKED, "two_factor_locked");
        let shown = format!("{:?}", TwoFactorChallenge { challenge: "challenge-secret".into(), expires_ms: 5 });
        assert!(!shown.contains("challenge-secret") && shown.contains("<redacted>"), "{shown}");
    }

    #[test]
    fn an_error_answer_reads_any_error_body() {
        let json = serde_json::json!({ "error": "conflict", "message": "m", "conflicts": [{ "op": 0, "kind": "root" }], "quota": { "used": 1, "limit": 2 } });
        let answer: ErrorAnswer = serde_json::from_value(json).unwrap();
        assert_eq!(answer, ErrorAnswer { error: "conflict".into(), message: "m".into(), name_taken: None });
    }

    #[test]
    fn an_error_answer_keeps_the_optional_trash_name_conflict() {
        let answer: ErrorAnswer = serde_json::from_value(serde_json::json!({
            "error": "conflict", "message": "no free conflict name", "name_taken": true
        }))
        .unwrap();
        assert_eq!(answer.name_taken, Some(true));
        let old: ErrorAnswer = serde_json::from_value(serde_json::json!({ "error": "conflict", "message": "m" })).unwrap();
        assert_eq!(old.name_taken, None, "older agents omit the optional field");
    }

    #[test]
    fn admin_bodies_carry_the_second_factor_and_hide_every_secret() {
        let confirm =
            AdminConfirm { password: Some("admin-secret-pw".into()), totp_code: Some("918273".into()), recovery_code: Some("ABCD-EFGH".into()) };
        let shown = format!("{confirm:?}");
        assert!(!shown.contains("admin-secret-pw") && !shown.contains("918273") && !shown.contains("ABCD-EFGH"), "{shown}");
        assert_eq!(shown.matches("<redacted>").count(), 3);
        let reset = AdminPasswordReset {
            password: Some("admin-secret-pw".into()),
            totp_code: Some("918273".into()),
            reset_two_factor: true,
            ..AdminPasswordReset::default()
        };
        let shown = format!("{reset:?}");
        assert!(!shown.contains("admin-secret-pw") && !shown.contains("918273") && shown.contains("reset_two_factor: true"), "{shown}");
        let create =
            AdminCreateUser { username: "ana".into(), role: "member".into(), totp_code: Some("918273".into()), ..AdminCreateUser::default() };
        assert!(!format!("{create:?}").contains("918273"));
        let update = AdminUpdateUser { recovery_code: Some("ABCD-EFGH".into()), ..AdminUpdateUser::default() };
        assert!(!format!("{update:?}").contains("ABCD-EFGH"));
        // Every factor field is optional and absent from a body that doesn't have it; the reset flag defaults to false.
        assert_eq!(
            serde_json::to_value(AdminConfirm { password: Some("p".into()), ..AdminConfirm::default() }).unwrap(),
            serde_json::json!({ "password": "p" })
        );
        let reset: AdminPasswordReset = serde_json::from_value(serde_json::json!({ "password": "p" })).unwrap();
        assert!(!reset.reset_two_factor && reset.totp_code.is_none());
        let user: AdminUserInfo = serde_json::from_value(serde_json::json!({
            "id": Uuid::nil(), "username": "ana", "role": "member", "created_ms": 1, "disabled_ms": null,
            "two_factor_enabled": true, "devices_active": 0, "stored_bytes": 0, "quota_bytes": null, "invite_pending": false
        }))
        .unwrap();
        assert!(user.two_factor_enabled);
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
    }

    #[test]
    fn audit_bodies_read_what_the_server_writes_and_ignore_what_is_newer() {
        let details: AuditDetails = serde_json::from_str(r#"{"revoked":2,"a_later_field":"x","quota_bytes":null}"#).unwrap();
        assert_eq!((details.revoked, details.quota_bytes), (Some(2), Some(None)));
        let end: AuditExportEnd = serde_json::from_str(r#"{"complete":true,"exported":3}"#).unwrap();
        assert_eq!((end.complete, end.truncated, end.exported), (Some(true), None, 3));
        let page: AuditPage = serde_json::from_str(r#"{"events":[]}"#).unwrap();
        assert_eq!(page.next, None);
    }

    #[test]
    fn space_roles_are_ordered_by_permission_and_keep_their_words() {
        assert!(SpaceRole::Viewer < SpaceRole::Editor && SpaceRole::Editor < SpaceRole::Owner);
        assert!(!SpaceRole::Viewer.may_write() && SpaceRole::Editor.may_write() && SpaceRole::Owner.may_write());
        for role in SpaceRole::ALL {
            assert_eq!(SpaceRole::parse(role.as_str()), Some(role));
            assert_eq!(serde_json::to_string(&role).unwrap(), format!("\"{}\"", role.as_str()));
        }
        for bad in ["", "Viewer", "admin", "owner "] {
            assert_eq!(SpaceRole::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn audit_filters_are_the_closed_list_or_a_prefix_of_it() {
        assert_eq!(AUDIT_ACTIONS.len(), 45);
        assert!(AUDIT_ACTIONS.iter().all(|a| valid_audit_action(a)));
        for prefix in ["auth.", "admin.", "cli.", "join.", "device.", "account.", "space."] {
            assert!(valid_audit_action(prefix), "{prefix}");
        }
        for bad in ["", "auth", "auth.log", "nope.", "AUTH.", "auth.login ", "admin.user_created.", "x' OR 1=1 --", "a.b."] {
            assert!(!valid_audit_action(bad), "{bad:?}");
        }
    }
}
