//! The agent's local API: what apps and the launcher (through `craft-cloud-app`) and the agent's own CLI send to the
//! running agent (`craft-cloud-agent run`) on this machine.
//!
//! - **Where**: HTTP on `127.0.0.1` only, on a random port, with a random token per start. Both are written with the
//!   agent's pid to [`LOCAL_API_FILE`] in the agent's data folder ([`data_dir`]), owner-only, and the file is removed
//!   when the agent stops cleanly.
//! - **Hardening** against web pages and other users reaching it: the `Host` header must be exactly
//!   `127.0.0.1:<port>`; any request with an `Origin` header is refused (a browser always sends one with a
//!   WebSocket); the token is accepted only as `Authorization: Bearer <token>`, never from the query string; it is
//!   never logged.
//! - **Calls** (JSON, all at [`paths`]): `GET /v1/status` → [`Status`]; `POST /v1/login` ([`LoginRequest`]) →
//!   [`Status`]; `POST /v1/join` ([`JoinRequest`]) → [`Status`]; `POST /v1/logout` ([`LogoutRequest`]) → [`Status`];
//!   `POST /v1/pairings` → [`PairingInfo`], `GET /v1/pairings/{id}` → [`PairingStatus`](crate::api::PairingStatus),
//!   `DELETE /v1/pairings/{id}` → 204; `GET /v1/devices` → a list of [`DeviceInfo`](crate::api::DeviceInfo),
//!   `DELETE /v1/devices/{id}` → the list after; `POST /v1/pause`, `/v1/resume` and `/v1/quit` → [`Status`];
//!   `GET /v1/usage` → [`AccountUsage`](crate::api::AccountUsage) from the signed-in account's server;
//!   `POST /v1/autostart` ([`AutostartRequest`]) → [`Status`]; `GET /v1/recents`, upgraded to a WebSocket of JSON
//!   text messages: [`RecentsRequest`] in, [`RecentsEvent`] out; `GET /v1/docs`, upgraded to a WebSocket of live
//!   documents ([`docs`]: [`DocsRequest`] and binary frames in, [`DocsEvent`] and binary frames out); `PUT /v1/blobs`
//!   and `GET /v1/blobs/{hash}`: binary content kept out of live documents ([`blobs`]); the admin calls under
//!   `/v1/admin` ([`paths::ADMIN_SERVER`], [`paths::ADMIN_USERS`]), which the agent passes on to the server's admin API
//!   with the same bodies and answers (see `docs/agent.md`); `GET /v1/spaces` → [`SpaceList`] and, under
//!   `/v1/spaces/{id}`, `POST …/accept`, `…/decline` (passed on to the server) and `…/sync` ([`SpaceSyncRequest`]): the
//!   team spaces of the account and what this computer does with them. Errors are the HTTP API's error
//!   body ([`ErrorAnswer`](crate::api::ErrorAnswer) reads its code and message) with its codes and [`codes`].
//! - **Versions**: [`Status::api`] is the local API version the agent speaks; a client refuses one outside
//!   [`LOCAL_API_MIN`]..=[`LOCAL_API_MAX`] when it connects.

use std::ffi::OsString;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::api::SpaceRole;
use crate::ids::{DeviceId, Epoch, NodeId, SpaceId, VersionId};
use crate::limits::MAX_TREE_DEPTH;
use crate::name::Name;

/// The file the running agent writes its local API's address and token to, in its data folder.
pub const LOCAL_API_FILE: &str = "local-api.json";
/// The format of [`LOCAL_API_FILE`].
pub const LOCAL_API_SCHEMA: u32 = 1;
/// Local API versions this build speaks.
pub const LOCAL_API_MIN: u32 = 1;
pub const LOCAL_API_MAX: u32 = 1;
/// The environment variable that overrides the agent's data folder.
pub const DATA_DIR_ENV: &str = "CRAFT_CLOUD_AGENT_DIR";
/// The data folder's name inside the OS's per-user data folder.
pub const DATA_DIR_NAME: &str = "craft-cloud-agent";

/// Most entries one app's recents list keeps.
pub const MAX_RECENTS: usize = 100;
/// Longest recent file path (relative to the synced folder), in bytes.
pub const MAX_RECENT_PATH_BYTES: usize = 4096;
/// Longest title of a recent file, in bytes.
pub const MAX_TITLE_BYTES: usize = 256;
/// Longest machine name, in bytes (the server's device name limit).
pub const MAX_MACHINE_BYTES: usize = 128;
/// Largest JSON body or WebSocket message of the local API, either way.
pub const MAX_LOCAL_JSON_BYTES: usize = 1024 * 1024;
/// Largest body of an admin call (the server's own limit is tiny too: a confirmation and a few fields).
pub const MAX_ADMIN_JSON_BYTES: usize = 8 * 1024;

/// Error codes of the local API (the `error` field of [`ErrorAnswer`](crate::api::ErrorAnswer)), beside the HTTP API's
/// `invalid`, `unauthorized` (the token), `not_found`, `too_large` and `internal`.
pub mod codes {
    /// A `Host` that isn't `127.0.0.1:<port>`, or an `Origin` header: the request didn't come from a local program.
    pub const FORBIDDEN: &str = crate::api::codes::FORBIDDEN;
    /// `login`: the server refused the username or password.
    pub const WRONG_PASSWORD: &str = crate::api::codes::WRONG_PASSWORD;
    /// `login` while signed in: sign out first.
    pub const ALREADY_SIGNED_IN: &str = "already_signed_in";
    /// `logout`, or recents, while signed out.
    pub const NOT_SIGNED_IN: &str = "not_signed_in";
    /// The server can't be reached; nothing was changed.
    pub const OFFLINE: &str = "offline";
    /// The agent stopped for something only a person can put right (the message says what).
    pub const NEEDS_YOU: &str = "needs_you";
    /// `join`: the code is wrong, expired or used (never which); make a new one.
    pub const INVALID_CODE: &str = crate::api::codes::INVALID_CODE;
    /// `join`: an invite to a new account needs a password; send the join again with one.
    pub const PASSWORD_REQUIRED: &str = crate::api::codes::PASSWORD_REQUIRED;
    /// `join`: the new password breaks the server's rules (the message says which).
    pub const WEAK_PASSWORD: &str = crate::api::codes::WEAK_PASSWORD;
    /// `DELETE /v1/devices/{id}` for this computer: sign out instead.
    pub const CANNOT_REMOVE_SELF: &str = "cannot_remove_self";
    /// This computer doesn't trust the server's certificate (the message says how to install the server's root).
    pub const UNTRUSTED_CERTIFICATE: &str = "untrusted_certificate";
    /// `autostart`: the start-at-login entry couldn't be written or removed (the message says why), or this agent
    /// doesn't handle it here. Whatever else was asked (a join) stands.
    pub const AUTOSTART_FAILED: &str = "autostart_failed";
    /// `join` or `login` without `adopt`: the folder already has files in it. Nothing was sent to the server, so the
    /// code still works; ask whether to sync those files, and send it again with `adopt` if so.
    pub const FOLDER_HAS_FILES: &str = "folder_has_files";
    /// `GET /v1/blobs/{hash}`: neither this machine nor the account's server has that blob.
    pub const BLOB_NOT_FOUND: &str = "blob_not_found";
    /// `login` or `join`: the account has two-factor sign-in. The answer's `two_factor` (see [`ErrorExtras`](super::ErrorExtras))
    /// holds the challenge for `POST /v1/login/second-factor` (empty for `join`: send the same join again with the code).
    pub const TWO_FACTOR_REQUIRED: &str = crate::api::codes::TWO_FACTOR_REQUIRED;
    /// A code or recovery code was wrong (HTTP 401 for `login`, `login/second-factor` and `join`, 403 on the account calls).
    /// It never means the device was revoked.
    pub const TWO_FACTOR_INVALID: &str = crate::api::codes::TWO_FACTOR_INVALID;
    /// Too many wrong codes: try again after the answer's `retry_after_secs` (HTTP 429, also the `Retry-After` header).
    pub const TWO_FACTOR_LOCKED: &str = crate::api::codes::TWO_FACTOR_LOCKED;
    /// The server has no key for two-factor secrets, so it can't be set up (HTTP 503).
    pub const TWO_FACTOR_UNAVAILABLE: &str = crate::api::codes::TWO_FACTOR_UNAVAILABLE;
    /// A set-up is open on another computer (HTTP 409).
    pub const TWO_FACTOR_PENDING: &str = crate::api::codes::TWO_FACTOR_PENDING;
    /// `login/second-factor`: the challenge doesn't work any more (it lasts five minutes and takes five wrong codes, and a
    /// password change ends it): sign in again from the first step (HTTP 409; a 401 would read as a bad local token).
    pub const TWO_FACTOR_EXPIRED: &str = "two_factor_expired";
    /// A state clash on the server (two-factor already on, no set-up to confirm, a password that changed meanwhile), HTTP 409.
    pub const CONFLICT: &str = crate::api::codes::CONFLICT;
    /// Never sent by an agent: what a client reports when the agent answers a call it doesn't know (`not_found` for
    /// the call itself), as one from before that call answers it. Update Craft Cloud.
    pub const AGENT_TOO_OLD: &str = "agent_too_old";
}

/// The paths of the local API.
pub mod paths {
    pub const STATUS: &str = "/v1/status";
    pub const LOGIN: &str = "/v1/login";
    pub const LOGOUT: &str = "/v1/logout";
    pub const RECENTS: &str = "/v1/recents";
    /// `GET`, upgraded to a WebSocket: live documents ([`super::docs`]).
    pub const DOCS: &str = "/v1/docs";
    /// `POST`: join with a join string ([`super::JoinRequest`]) → [`super::Status`].
    pub const JOIN: &str = "/v1/join";
    /// `POST` ([`super::SecondFactorLoginRequest`]): the second step of a sign-in that answered `two_factor_required` →
    /// [`super::Status`].
    pub const LOGIN_SECOND_FACTOR: &str = "/v1/login/second-factor";
    /// `GET` → [`crate::api::TwoFactorStatus`]; with `/setup`, `/confirm`, `/disable` and `/recovery-codes` below it: the
    /// signed-in account's two-factor sign-in, passed on to the server ([`super::PasswordRequest`], [`super::TotpRequest`],
    /// [`super::TwoFactorProofRequest`]). Nothing is stored or logged on the way.
    pub const TWO_FACTOR: &str = "/v1/account/two-factor";
    pub const TWO_FACTOR_SETUP: &str = "/v1/account/two-factor/setup";
    pub const TWO_FACTOR_CONFIRM: &str = "/v1/account/two-factor/confirm";
    pub const TWO_FACTOR_DISABLE: &str = "/v1/account/two-factor/disable";
    pub const TWO_FACTOR_RECOVERY_CODES: &str = "/v1/account/two-factor/recovery-codes";
    /// `GET` → [`crate::api::AccountInfo`]: the signed-in account (its role, and whether it has two-factor sign-in), as the
    /// server says it.
    pub const ACCOUNT: &str = "/v1/account";
    /// `GET /v1/account/activity?cursor&before_ms&limit` → [`crate::api::AuditPage`]: the signed-in account's own security
    /// events, for every account.
    pub const ACCOUNT_ACTIVITY: &str = "/v1/account/activity";
    /// `POST` ([`super::ChangePasswordRequest`]) → 204.
    pub const ACCOUNT_PASSWORD: &str = "/v1/account/password";
    /// `POST` → [`crate::api::RevokedOthers`]: sign every other computer of the account out.
    pub const DEVICES_REVOKE_OTHERS: &str = "/v1/devices/revoke-others";
    /// `POST`: a new add-a-computer code → [`super::PairingInfo`]; `GET`/`DELETE` `/v1/pairings/{id}`: its state, or
    /// cancel it.
    pub const PAIRINGS: &str = "/v1/pairings";
    /// `GET`: the account's computers; `DELETE /v1/devices/{id}`: remove one (never this one).
    pub const DEVICES: &str = "/v1/devices";
    /// `GET`: the signed-in account's stored bytes and labeled estimates, fetched from its server.
    pub const USAGE: &str = "/v1/usage";
    /// `POST`: hold syncing (kept across restarts) → [`super::Status`].
    pub const PAUSE: &str = "/v1/pause";
    /// `POST`: sync again after a pause → [`super::Status`].
    pub const RESUME: &str = "/v1/resume";
    /// `POST`: finish the pass under way and exit → [`super::Status`] (as the agent stops).
    pub const QUIT: &str = "/v1/quit";
    /// `POST` ([`super::AutostartRequest`]): start, or don't start, the agent at login → [`super::Status`].
    pub const AUTOSTART: &str = "/v1/autostart";
    /// `PUT`: a blob → [`super::blobs::Stored`]; `GET /v1/blobs/{hash}`: its bytes ([`super::blobs`]).
    pub const BLOBS: &str = "/v1/blobs";
    /// `GET`: the server's counters and kinds (an admin's; any other account is refused with `forbidden`) →
    /// [`AdminServerInfo`](crate::api::AdminServerInfo).
    pub const ADMIN_SERVER: &str = "/v1/admin/server";
    /// `GET /v1/admin/storage` → [`AdminStorageOverview`](crate::api::AdminStorageOverview).
    pub const ADMIN_STORAGE: &str = "/v1/admin/storage";
    /// `GET /v1/admin/users?cursor&limit&query` → [`AdminUserList`](crate::api::AdminUserList); `POST` (an
    /// [`AdminCreateUser`](crate::api::AdminCreateUser)) → [`AdminCreatedUser`](crate::api::AdminCreatedUser).
    /// Under `/v1/admin/users/{id}`: `PATCH` ([`AdminUpdateUser`](crate::api::AdminUpdateUser)) → the account's
    /// [`AdminUserInfo`](crate::api::AdminUserInfo); `DELETE`, and `POST` to `…/disable`, `…/enable`, `…/invite` and
    /// `…/password-reset`, and `DELETE …/invite` and `…/devices/{device_id}`, each with an
    /// [`AdminConfirm`](crate::api::AdminConfirm) (the invite and the reset code answer an
    /// [`AdminCode`](crate::api::AdminCode), the others have no body); `GET …/devices` →
    /// [`AdminDevices`](crate::api::AdminDevices). The server's status and error code are the agent's.
    pub const ADMIN_USERS: &str = "/v1/admin/users";
    /// `GET /v1/admin/users/{id}/usage` → [`AccountUsage`](crate::api::AccountUsage).
    pub const ADMIN_USER_USAGE: &str = "/v1/admin/users";
    /// `GET /v1/admin/audit?cursor&before_ms&limit&user&action` → [`AuditPage`](crate::api::AuditPage): the whole security log,
    /// newest first, filtered (an admin's).
    pub const ADMIN_AUDIT: &str = "/v1/admin/audit";
    /// `GET /v1/admin/audit/export?before_ms&user&action`: the log as NDJSON, streamed through (chunked, never held whole by the
    /// agent): [`AuditEvent`](crate::api::AuditEvent) lines and a last [`AuditExportEnd`](crate::api::AuditExportEnd) line.
    /// A stream that ends without that last line was cut off.
    pub const ADMIN_AUDIT_EXPORT: &str = "/v1/admin/audit/export";
    /// `GET`: version history for a synced file, addressed by its path relative to the sync root.
    pub const FILE_HISTORY: &str = "/v1/files/history";
    /// `POST`: restore a prior version of a synced file, addressed by its path relative to the sync root.
    pub const FILE_RESTORE: &str = "/v1/files/restore";
    /// `GET`: the server's deleted nodes; `POST` restore and remove calls address node IDs.
    pub const TRASH: &str = "/v1/trash";
    pub const TRASH_RESTORE: &str = "/v1/trash/restore";
    pub const TRASH_REMOVE: &str = "/v1/trash/remove";
    /// `GET` → [`super::SpaceList`]: the personal space and the team spaces of the account, invitations among them, with
    /// what this computer does with each. Under `/v1/spaces/{id}`: `POST …/accept` → the space as the server lists it
    /// ([`SpaceListItem`](crate::api::SpaceListItem)), `POST …/decline` → 204 (both passed on to the server), and
    /// `POST …/sync` ([`super::SpaceSyncRequest`]) → the space as [`super::SpaceInfo`], and `POST …/approve-deletes`
    /// ([`super::ApproveDeletesRequest`]) → [`super::ApproveDeletesAnswer`].
    pub const SPACES: &str = "/v1/spaces";
}

/// One version in a file's newest-first history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryVersion {
    pub id: VersionId,
    pub parent_version: Option<VersionId>,
    pub epoch: Epoch,
    pub seq: u64,
    pub size: u64,
    pub mtime_ms: i64,
    pub device: DeviceId,
    pub created_ms: i64,
    pub save: bool,
    pub pruned: bool,
}

/// A page of a file's newest-first version history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryResponse {
    pub versions: Vec<HistoryVersion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// The path and page cursor for `GET /v1/files/history`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileHistoryRequest {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// `POST /v1/files/restore`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreRequest {
    pub path: String,
    pub version: VersionId,
    pub expected_version: VersionId,
}

/// The new version made by a successful restore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreResponse {
    pub version: VersionId,
    pub parent_version: VersionId,
    pub seq: u64,
    pub epoch: Epoch,
}

/// One deleted node from the server's trash. `path` is a local display hint and can be absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrashItem {
    pub id: NodeId,
    pub parent: NodeId,
    pub name: String,
    pub is_dir: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    pub deleted_ms: i64,
    pub purge_after_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// A page of the server's trash, newest deletion first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrashResponse {
    pub items: Vec<TrashItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// `{id}` in a local trash restore or remove request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrashNodeRequest {
    pub id: String,
}

/// The server's answer after restoring a node from trash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrashRestoreResponse {
    pub node: NodeId,
    pub parent: NodeId,
    pub name: String,
    pub renamed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<VersionId>,
    pub seq: u64,
    pub epoch: Epoch,
}

/// Binary content an app keeps out of a live document (images, audio, a font): `PUT /v1/blobs` with the bytes as the
/// body (`application/octet-stream`, 1 byte to [`MAX_BLOB_BYTES`](crate::limits::MAX_BLOB_BYTES)) is answered with
/// [`Stored`] once the blob is on the agent's disk; it goes up to the server as a file's content does (cut into
/// chunks, then a manifest), at once when online and later otherwise. `GET /v1/blobs/{hash}` answers the bytes: from
/// the agent's disk, or downloaded first when this machine doesn't have them yet (`offline` when it can't reach the
/// server; [`codes::BLOB_NOT_FOUND`](super::codes::BLOB_NOT_FOUND) when the account has no such blob).
///
/// A blob is named by its content: `hash` is the BLAKE3 hash of the manifest the bytes cut into (with `fastcdc-v1`,
/// as the file sync cuts content), so the same bytes have the same name on every machine. An agent from before blobs
/// answers both calls with the local API's `not_found` for an unknown call; a client reads that as
/// [`codes::AGENT_TOO_OLD`](super::codes::AGENT_TOO_OLD).
pub mod blobs {
    use serde::{Deserialize, Serialize};

    /// The answer to a `PUT`: the blob's name and length.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Stored {
        /// 64 lowercase hex digits.
        #[serde(with = "hex32")]
        pub hash: [u8; 32],
        pub len: u64,
    }

    /// `hash` written as 64 lowercase hex digits, for the `GET` path.
    pub fn hex(hash: &[u8; 32]) -> String {
        hash.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// 64 lowercase hex digits as a hash; `None` for anything else.
    pub fn parse_hex(text: &str) -> Option<[u8; 32]> {
        let bytes = text.as_bytes();
        if bytes.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for (slot, pair) in out.iter_mut().zip(bytes.chunks(2)) {
            let digit = |c: u8| match c {
                b'0'..=b'9' => Some(c - b'0'),
                b'a'..=b'f' => Some(c - b'a' + 10),
                _ => None,
            };
            let (hi, lo) = (digit(*pair.first()?)?, digit(*pair.get(1)?)?);
            *slot = (hi << 4) | lo;
        }
        Some(out)
    }

    mod hex32 {
        use serde::{Deserialize, Deserializer, Serializer};

        pub fn serialize<S: Serializer>(hash: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
            s.serialize_str(&super::hex(hash))
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
            let text = String::deserialize(d)?;
            super::parse_hex(&text).ok_or_else(|| serde::de::Error::custom("expected 64 lowercase hex digits"))
        }
    }
}

/// The agent's data folder: `$CRAFT_CLOUD_AGENT_DIR` if set, else [`default_data_dir`]. `env` reads an environment
/// variable. The agent's `--data-dir` flag wins over both; apps use this.
pub fn data_dir(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    env(DATA_DIR_ENV).filter(|v| !v.is_empty()).map(PathBuf::from).or_else(|| default_data_dir(env))
}

/// The OS's per-user data folder plus `craft-cloud-agent`: `%LOCALAPPDATA%` on Windows (machine-local: the state
/// describes this machine's disk, so it must not roam), `~/Library/Application Support` on macOS, `$XDG_DATA_HOME` or
/// `~/.local/share` elsewhere. Relative values are ignored. Written out rather than taken from a crate: the usual ones
/// depend on an MPL-licensed helper.
pub fn default_data_dir(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let absolute = |name: &str| env(name).map(PathBuf::from).filter(|p| p.is_absolute());
    let base = if cfg!(windows) {
        absolute("LOCALAPPDATA")
    } else if cfg!(target_vendor = "apple") {
        absolute("HOME").map(|home| home.join("Library").join("Application Support"))
    } else {
        absolute("XDG_DATA_HOME").or_else(|| absolute("HOME").map(|home| home.join(".local").join("share")))
    };
    base.map(|b| b.join(DATA_DIR_NAME))
}

/// [`LOCAL_API_FILE`]: where the running agent listens and the token it wants. Its `Debug` output hides the token.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalApiFile {
    pub schema: u32,
    pub port: u16,
    pub token: String,
    pub pid: u32,
}

impl std::fmt::Debug for LocalApiFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalApiFile").field("port", &self.port).field("token", &"<redacted>").field("pid", &self.pid).finish()
    }
}

/// What the agent is doing, in [`Status`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RunState {
    /// Not signed in: waiting for a sign-in.
    SignedOut,
    /// Signed in, waiting for something to sync.
    Idle,
    /// A sync pass is running.
    Syncing,
    /// The last pass failed for a reason that may pass by itself; it is tried again.
    Retrying { message: String },
    /// Stopped until someone puts something right.
    Paused { reason: String },
    /// Paused by the user (`POST /v1/pause`), until `POST /v1/resume`, also after a restart: no sync passes and no
    /// live connection; the local API and apps' recents on this machine keep working. Not a problem to put right,
    /// unlike [`RunState::Paused`].
    PausedByUser,
    /// The agent is stopping.
    Stopping,
}

/// `GET /v1/status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// The local API version the agent speaks.
    pub api: u32,
    pub signed_in: bool,
    /// The account's username, when signed in.
    #[serde(default)]
    pub account: Option<String>,
    /// The server's address, when signed in.
    #[serde(default)]
    pub server: Option<String>,
    /// The synced folder, when signed in.
    #[serde(default)]
    pub folder: Option<String>,
    /// This machine's name (its device name), when signed in.
    #[serde(default)]
    pub machine: Option<String>,
    /// Whether the agent reaches the server right now (its live connection is up).
    pub online: bool,
    pub run_state: RunState,
    /// Whether the agent starts by itself at login on this computer; `None` where the agent can't tell or doesn't
    /// support it (an older agent says nothing).
    #[serde(default)]
    pub autostart: Option<bool>,
    /// When the last sync pass that didn't fail finished (milliseconds since the Unix epoch), while signed in; `None`
    /// before the first one (an older agent says nothing).
    #[serde(default)]
    pub last_sync_ms: Option<i64>,
    /// The server refused this computer's device (it was removed from the account, on another computer or by an
    /// admin): nothing syncs until it signs out and joins again. Its files stay.
    #[serde(default)]
    pub removed: bool,
    /// The team spaces this computer syncs, or lost access to, one entry each (the fields above describe the personal
    /// space). An older agent says nothing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spaces: Vec<SpaceStatus>,
}

/// What this computer does about one space, in [`SpaceInfo`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpaceSync {
    /// The space syncs to a folder on this computer.
    On,
    /// It doesn't (a team space starts so on every computer: the person chooses).
    Off,
    /// It can't: the server is too old for team spaces, or this computer lost access to the space.
    Unavailable,
}

/// How the sync of one team space is going, in [`SpaceStatus`] and [`SpaceInfo`]: [`RunState`] for a space, and the two
/// ways a space stops that a whole agent doesn't. The `Debug` output leaves out the words of `NoAccess`, which name the
/// space.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum SpaceRun {
    Idle,
    Syncing,
    /// The last pass failed for a reason that may pass by itself; it is tried again.
    Retrying {
        message: String,
    },
    /// Stopped until someone puts something right (the folder is gone, say).
    Paused {
        reason: String,
    },
    /// Syncing is paused by the user for every space.
    PausedByUser,
    /// The account no longer has access (it was removed, or the invitation withdrawn; an archived space is not this: its members
    /// keep read access). The local copy is
    /// kept and no longer syncs; if access comes back it syncs again. `message` says so in words.
    NoAccess {
        message: String,
    },
    /// The server can't do team spaces yet (it needs an update).
    Unavailable {
        message: String,
    },
}

impl std::fmt::Debug for SpaceRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Idle => f.write_str("Idle"),
            Self::Syncing => f.write_str("Syncing"),
            Self::Retrying { message } => f.debug_struct("Retrying").field("message", message).finish(),
            Self::Paused { reason } => f.debug_struct("Paused").field("reason", reason).finish(),
            Self::PausedByUser => f.write_str("PausedByUser"),
            Self::NoAccess { .. } => f.debug_struct("NoAccess").finish_non_exhaustive(),
            Self::Unavailable { message } => f.debug_struct("Unavailable").field("message", message).finish(),
        }
    }
}

/// One synced team space in [`Status::spaces`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceStatus {
    pub space: SpaceId,
    pub run_state: SpaceRun,
    /// When the last pass that didn't fail finished (milliseconds since the Unix epoch).
    #[serde(default)]
    pub last_sync_ms: Option<i64>,
    /// The account is a viewer here (or the space is archived): the folder follows the space, and nothing of this
    /// computer goes up.
    #[serde(default)]
    pub read_only: bool,
}

/// One space in `GET /v1/spaces`: the server's view of it (its name, the account's role, whether it is an invitation)
/// and this computer's (the folder, whether it syncs, how that goes). The `Debug` output leaves out the name and the
/// folder.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceInfo {
    pub space: SpaceId,
    /// `personal` or `team`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub role: SpaceRole,
    /// `active`, or `invited` for an invitation not yet accepted.
    pub state: String,
    #[serde(default)]
    pub archived: bool,
    /// The folder on this computer (absolute): where the space syncs, or would.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    pub sync: SpaceSync,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_state: Option<SpaceRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sync_ms: Option<i64>,
    #[serde(default)]
    pub read_only: bool,
}

impl std::fmt::Debug for SpaceInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpaceInfo")
            .field("space", &self.space)
            .field("kind", &self.kind)
            .field("role", &self.role)
            .field("state", &self.state)
            .field("archived", &self.archived)
            .field("sync", &self.sync)
            .field("run_state", &self.run_state)
            .field("read_only", &self.read_only)
            .finish_non_exhaustive()
    }
}

/// `GET /v1/spaces`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SpaceList {
    pub spaces: Vec<SpaceInfo>,
}

/// `POST /v1/spaces/{id}/sync`: turn the sync of a team space on or off on this computer. `folder` (absolute) picks
/// another folder than the default one beside the personal folder; the folder must not lie inside another synced folder or
/// the agent's data folder. A folder that already has files in it is synced only with `adopt` (files that differ from
/// the space's become conflict copies; nothing is deleted).
#[derive(Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SpaceSyncRequest {
    pub on: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    #[serde(default)]
    pub adopt: bool,
}

impl std::fmt::Debug for SpaceSyncRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpaceSyncRequest").field("on", &self.on).field("adopt", &self.adopt).finish_non_exhaustive()
    }
}

/// `POST /v1/spaces/{id}/approve-deletes`: approve the deletions of a team space that the mass-delete guard holds back, as
/// `craft-cloud-agent sync --approve-deletes --space` does (it writes the space's `approval.json`, which the running agent
/// takes). With a `set` (the identifier the waiting list was shown with) only that set is approved if it is the one waiting;
/// without, the set is approved if it is the one last shown by this call (the first call lists it and approves nothing).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ApproveDeletesRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set: Option<String>,
}

/// The answer to [`ApproveDeletesRequest`]: whether an approval was handed to the agent, and what the command line
/// would have said. Its `Debug` output leaves out the words, which list file names.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApproveDeletesAnswer {
    pub approved: bool,
    pub message: String,
}

impl std::fmt::Debug for ApproveDeletesAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApproveDeletesAnswer").field("approved", &self.approved).finish_non_exhaustive()
    }
}

/// `POST /v1/login`: what `craft-cloud-agent login` asks for. The password goes to the agent and from there to the
/// server only; it is never written anywhere. Its `Debug` output hides the password.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginRequest {
    pub server: String,
    pub user: String,
    pub password: String,
    /// The folder to sync (absolute). Default: "Craft Cloud" in the home folder.
    #[serde(default)]
    pub folder: Option<String>,
    /// This computer's name in the device list. Default: the computer's name.
    #[serde(default)]
    pub device_name: Option<String>,
    /// Sync a folder that already has files in it (as `login --adopt`).
    #[serde(default)]
    pub adopt: bool,
}

impl std::fmt::Debug for LoginRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginRequest")
            .field("server", &self.server)
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("folder", &self.folder)
            .field("device_name", &self.device_name)
            .field("adopt", &self.adopt)
            .finish()
    }
}

/// `POST /v1/join`: join a Craft Cloud with a join string (an invite or an add-a-computer code), then sync as
/// `/v1/login` does. The join string and the password go to the agent and from there to the server only; its
/// `Debug` output hides both.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinRequest {
    /// As typed or pasted: `cloud.example.com/K7M4-Q9TX`, or the link `https://cloud.example.com/join/K7M4-Q9TX`
    /// (see `craft_cloud_proto::join::JoinString`).
    pub join: String,
    /// The folder to sync (absolute). Default: "Craft Cloud" in the home folder.
    #[serde(default)]
    pub folder: Option<String>,
    /// This computer's name in the device list. Default: the computer's name.
    #[serde(default)]
    pub device_name: Option<String>,
    /// Sync a folder that already has files in it.
    #[serde(default)]
    pub adopt: bool,
    /// The new account's password, for an invite that needs one (answered `password_required` without it).
    #[serde(default)]
    pub password: Option<String>,
    /// The second factor of an account that has two-factor sign-in (answered `two_factor_required` without it, the code not
    /// used up): a code from the authenticator app, or a recovery code instead. Send the same join again with one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totp_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_code: Option<String>,
}

impl std::fmt::Debug for JoinRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinRequest")
            .field("join", &"<redacted>")
            .field("folder", &self.folder)
            .field("device_name", &self.device_name)
            .field("adopt", &self.adopt)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("totp_code", &self.totp_code.as_ref().map(|_| "<redacted>"))
            .field("recovery_code", &self.recovery_code.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// What an error answer carries beyond `error` and `message` ([`ErrorAnswer`](crate::api::ErrorAnswer) reads those two): the
/// challenge of `two_factor_required` and the wait of `two_factor_locked` (also in the `Retry-After` header). A client reads
/// it from the same body; every field is missing from answers that have no use for it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorExtras {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub two_factor: Option<crate::api::TwoFactorChallenge>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_secs: Option<u64>,
}

/// `POST /v1/login/second-factor`: the second step of a sign-in the agent answered `two_factor_required`. The agent keeps
/// nothing between the two steps, so the first step's `server`, `folder`, `device_name` and `adopt` come again (`user` is
/// the first step's and is only checked to be sane), with the `challenge` from the answer and a code (`totp_code`) or a recovery
/// code. Answered with [`Status`]. Its `Debug` output hides the challenge and the codes.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecondFactorLoginRequest {
    pub server: String,
    pub user: String,
    #[serde(default)]
    pub folder: Option<String>,
    #[serde(default)]
    pub device_name: Option<String>,
    #[serde(default)]
    pub adopt: bool,
    pub challenge: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totp_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_code: Option<String>,
}

impl std::fmt::Debug for SecondFactorLoginRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecondFactorLoginRequest")
            .field("server", &self.server)
            .field("user", &self.user)
            .field("folder", &self.folder)
            .field("device_name", &self.device_name)
            .field("adopt", &self.adopt)
            .field("challenge", &"<redacted>")
            .field("totp_code", &self.totp_code.as_ref().map(|_| "<redacted>"))
            .field("recovery_code", &self.recovery_code.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// `POST /v1/account/two-factor/setup`: the account's password, to start setting two-factor sign-in up. Answered with
/// [`TwoFactorSetupResponse`](crate::api::TwoFactorSetupResponse). Its `Debug` output hides the password.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PasswordRequest {
    pub password: String,
}

impl std::fmt::Debug for PasswordRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PasswordRequest").field("password", &"<redacted>").finish()
    }
}

/// `POST /v1/account/two-factor/confirm`: the first code of the set-up; answered with
/// [`RecoveryCodes`](crate::api::RecoveryCodes), shown once. Its `Debug` output hides the code.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TotpRequest {
    pub totp_code: String,
}

impl std::fmt::Debug for TotpRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TotpRequest").field("totp_code", &"<redacted>").finish()
    }
}

/// `POST /v1/account/two-factor/disable` (answered 204) and `/recovery-codes` (answered with
/// [`RecoveryCodes`](crate::api::RecoveryCodes), with `totp_code` only): the password and one second factor. Its `Debug` output
/// hides all three.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// `POST /v1/account/password`: the account's password change; answered 204, and every other computer is signed out. Its
/// `Debug` output hides both passwords.
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

/// `PATCH /v1/devices/{id}`: a new name for one of the account's computers; answered with its
/// [`DeviceInfo`](crate::api::DeviceInfo).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenameDeviceRequest {
    pub name: String,
}

/// `POST /v1/pairings`: a new add-a-computer code of this account, to enter on the computer being added. It
/// replaces this computer's earlier one. Its `Debug` output hides the code.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingInfo {
    /// For `GET` and `DELETE /v1/pairings/{id}`.
    pub id: uuid::Uuid,
    /// The join string to show: `cloud.example.com/K7M4-Q9TX`.
    pub join: String,
    /// The link form, for a QR code: `https://cloud.example.com/join/K7M4-Q9TX`.
    pub link: String,
    /// When it stops working, milliseconds since the Unix epoch, by the server's clock.
    pub expires_ms: i64,
}

impl std::fmt::Debug for PairingInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingInfo").field("id", &self.id).field("join", &"<redacted>").field("expires_ms", &self.expires_ms).finish()
    }
}

/// `POST /v1/autostart`: start the agent by itself at login on this computer (`on`), or don't any more.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutostartRequest {
    pub on: bool,
}

/// `POST /v1/logout`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogoutRequest {
    /// Forget the account here without reaching the server (as `logout --local`).
    #[serde(default)]
    pub local: bool,
}

/// One recent file of an app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentEntry {
    /// The file's node, once the agent has synced it (stable through renames and moves).
    #[serde(default)]
    pub node: Option<NodeId>,
    /// Where it is, relative to the synced folder, `/`-separated.
    pub path: String,
    pub title: String,
    /// When it was last opened (or created), milliseconds since the Unix epoch, by the machine's clock.
    pub opened_ms: i64,
    /// When it was last saved, if it was.
    #[serde(default)]
    pub saved_ms: Option<i64>,
    /// The name of the machine it was opened on.
    pub machine: String,
}

/// What an app adds: the agent fills in the node, the time and the machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewRecent {
    /// Relative to the synced folder, `/`-separated.
    pub path: String,
    /// Shown in the list; default: the file's name.
    #[serde(default)]
    pub title: Option<String>,
    /// The app saved the file (rather than only opening it).
    #[serde(default)]
    pub saved: bool,
}

/// A message from a client on the recents WebSocket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum RecentsRequest {
    /// Send `app`'s list now and after every change. With a `space`, the list of that team space (it must sync on this
    /// computer); without, the personal list as before.
    Subscribe {
        app: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        space: Option<SpaceId>,
    },
    /// Add a file to `app`'s list; answered `added` (or `error`) with the same `id`. An absolute `entry.path` inside a
    /// synced team folder goes to that space's list (and is kept relative to the folder); any other path is the
    /// personal list's, as before.
    Add { id: u64, app: String, entry: NewRecent },
}

/// A message from the agent on the recents WebSocket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum RecentsEvent {
    /// `app`'s whole list, newest first: on `subscribe` and after every change (from this machine or another).
    /// `push_stopped`: after Loro failed twice on this machine's copy, it no longer sends the list up whole (new
    /// entries still go up one by one, and what the server lacks of the rest stays here): an app may say so.
    Entries {
        app: String,
        entries: Vec<RecentEntry>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        push_stopped: bool,
        /// The team space the list belongs to; absent for the personal list (and from an older agent).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        space: Option<SpaceId>,
    },
    /// The `add` with this `id` is kept (written to disk; it reaches the server when it can).
    Added { id: u64, entry: RecentEntry },
    /// A request was refused (`id` names an `add`). `code` is one of [`crate::api::codes`].
    Error {
        #[serde(default)]
        id: Option<u64>,
        code: String,
        message: String,
    },
}

/// The live documents WebSocket, `GET /v1/docs`: an app opens files of the synced folder as live documents.
///
/// Control messages are JSON text frames ([`DocsRequest`] in, [`DocsEvent`] out); document bytes are binary frames of
/// `[u8 kind][u64 id, big-endian][bytes]` ([`encode_binary`], [`decode_binary`]), so nothing is base64. `id` is the
/// number the app gave the document in its `open`; one connection may open many documents.
///
/// - `open {id, path, app}` → `opened {id, doc, state}` followed by the document's state (a binary [`STATE`] frame: a
///   Loro snapshot, then any updates still waiting for their history as [`UPDATE`] frames), or `needs_import {id}`
///   (the file has never been live: the app sends its content as a binary [`IMPORT`] frame, a Loro snapshot, and then
///   gets `opened`), or `not_live {id, reason}` (the app saves the file as it always does).
/// - A binary [`UPDATE`] from the app is its own change; it is answered `ack {id, seq}` once it is on the agent's disk
///   (`seq` counts the app's updates of that document on this connection, from 1), or `error {id, seq, …}` when the
///   agent refused it (it doesn't decode, or is over 1 MiB).
/// - From the agent: remote changes as binary [`UPDATE`] frames and other machines' presence as [`PRESENCE`] frames;
///   `status {doc, online, waiting, deleted}` when that changes.
/// - A binary [`PRESENCE`] from the app is relayed to the document's other machines and apps (at most 4 KiB).
/// - `close {doc}`: the app is done with it (the agent keeps it).
///
/// **Team spaces** (live protocol 3): the agent routes a document by the path the app opens (an absolute path inside a
/// synced team folder goes to that space's live link; a relative path is the personal space's). `opened` then names the
/// `space`, and says `read_only: true` for a viewer: an update the app sends to a read-only document is refused with
/// `error {id, seq, code: "read_only"}`, the agent's copy is unchanged, and nothing is sent to the server. An app that
/// opens with `identity: true` gets other accounts' presence as [`PRESENCE_FROM`] frames and a `members` message (the
/// space's active members with their names and palette colours) after `opened` and whenever the list changes; an app
/// that doesn't ask gets only [`PRESENCE`] frames and no `members`.
///
/// The agent tells every app with a document open in a space when the account's right to change it changes, as
/// `role {doc, read_only}` (both ways; the apps that asked for identity and those that did not alike): `opened.read_only`
/// is as of the open, `role` follows it. An app that holds changes it made as a viewer sends them again when `role` says
/// `read_only: false`, and, if it ends first, hands them to the agent as [`docs::HELD`] frames.
///
/// An app that doesn't read for 10 s is dropped; its documents stay with the agent.
pub mod docs {
    /// Binary frame kinds.
    pub const STATE: u8 = 1;
    pub const UPDATE: u8 = 2;
    pub const PRESENCE: u8 = 3;
    pub const IMPORT: u8 = 4;
    /// Presence with the sender's identity, for an app that opened the document with `identity: true`:
    /// `[u8 5][u64 id, big-endian][16 bytes account ID][the app's bytes]`. An app that did not ask gets [`PRESENCE`].
    pub const PRESENCE_FROM: u8 = 5;
    /// A change the app made while the agent refused its changes (`read_only`) and holds on to until the account may write
    /// again, sent by an app that ends with such changes unsent: `[u8 6][u64 id, big-endian][a Loro update]`, one frame
    /// per update, in order. The agent puts it into the document's journal as it does an update, but the gate keeps it from
    /// the server while the space is read-only; it goes up with the first connection of an account that may write. Never
    /// acked; if the account may write already it is an update that is not acked.
    pub const HELD: u8 = 6;
    /// The bytes of an account ID in a [`PRESENCE_FROM`] frame.
    pub const USER_BYTES: usize = 16;
    /// The header of a binary frame: kind and id.
    pub const HEADER_BYTES: usize = 9;
    /// Largest binary frame (a state or an import: a snapshot), either way.
    pub const MAX_BINARY_BYTES: usize = crate::limits::MAX_SNAPSHOT_BYTES + HEADER_BYTES;

    /// A binary frame.
    pub fn encode_binary(kind: u8, id: u64, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len().saturating_add(HEADER_BYTES));
        out.push(kind);
        out.extend_from_slice(&id.to_be_bytes());
        out.extend_from_slice(bytes);
        out
    }

    /// A binary frame's kind, id and bytes; `None` for one too short to have a header.
    pub fn decode_binary(frame: &[u8]) -> Option<(u8, u64, &[u8])> {
        let (&kind, rest) = frame.split_first()?;
        let id = u64::from_be_bytes(rest.get(..8)?.try_into().ok()?);
        Some((kind, id, rest.get(8..)?))
    }
}

/// A control message from an app on the docs WebSocket ([`docs`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum DocsRequest {
    /// Open the file at `path` (relative to the synced folder, `/`-separated, or absolute inside it) as a live document
    /// for `app` (a lowercase app name), under the number `id` on this connection. `file_hash`: the BLAKE3 hash (hex)
    /// of the file's bytes as the app read them, for `opened` to say whether a Craft app saved them
    /// ([`DocsEvent::Opened`]'s `changed_outside`).
    Open {
        id: u64,
        path: String,
        app: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file_hash: Option<String>,
        /// The app wants to know who is there: other accounts' presence comes as [`docs::PRESENCE_FROM`] frames and a
        /// `members` message follows `opened`. Without it (the default, and an older app) only [`docs::PRESENCE`] frames.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        identity: bool,
    },
    /// The app is done with the document.
    Close { doc: crate::ids::DocId },
    /// The app is about to write the document's file: its content's BLAKE3 hash (hex) and the live document's
    /// frontiers it was made from (Loro's heads, `[peer, counter]` operation IDs). Sent on the same stream after every
    /// change it covers; the agent keeps it once all those operations are in its copy (one that comes first waits for
    /// them, and is dropped if the app closes the document before), and records it with the file version it uploads
    /// for that content ([`SaveRecord`]).
    Saved { doc: crate::ids::DocId, hash: String, frontiers: Vec<(u64, u32)> },
    /// The app is about to write the document's file with content `hash` (hex), and its record (`saved`) follows once
    /// the changes it covers have gone: sent at once, before the write. Until the record comes (or the app closes the
    /// document), the agent's sync takes that content for a Craft save on its way: it doesn't upload it without its
    /// record, nor make a conflict copy of it, but waits (at most the save-order wait, 10 minutes).
    Saving { doc: crate::ids::DocId, hash: String },
}

/// Most heads a save record's frontiers may have: the live document's concurrent tips, one or two in practice
/// (a record with more is a hostile one and isn't kept). At most about 9 KiB of JSON.
pub const MAX_SAVE_HEADS: usize = 256;

/// A file version a Craft app saved from a live document: what the version's content came from. Kept with the
/// version on the server (an older server or agent ignores it), so the file sync can tell two Craft saves of one live
/// document from an edit outside Craft.
///
/// It names the live document's frontiers when saved (its heads: the last operations, as `(peer, counter)`), never a
/// version vector, so it stays small however many machines and sessions have edited the document. Whether one record
/// includes another is answered by an agent that holds the document, from its copy (see the sync's `SaveRecords`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveRecord {
    /// The live document.
    pub doc: crate::ids::DocId,
    /// The live document's frontiers when saved: `(peer, counter)` operation IDs, sorted.
    pub frontiers: Vec<(u64, u32)>,
}

impl SaveRecord {
    /// Whether the record is well formed: at most [`MAX_SAVE_HEADS`] heads, sorted and distinct.
    pub fn is_valid(&self) -> bool {
        self.frontiers.len() <= MAX_SAVE_HEADS && self.frontiers.windows(2).all(|w| matches!(w, [a, b] if a < b))
    }

    /// Reads an optional record field leniently (`#[serde(deserialize_with)]`): a record that doesn't read (an older
    /// build's form, a damaged one, an unknown later one) or isn't well formed is no record, and only the record is
    /// lost, never what holds it.
    pub fn lenient<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<SaveRecord>, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Any {
            Record(SaveRecord),
            Other(serde::de::IgnoredAny),
        }
        Ok(match Option::<Any>::deserialize(d)? {
            Some(Any::Record(r)) if r.is_valid() => Some(r),
            _ => None,
        })
    }
}

/// How a document stands, as `opened` and `status` say it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocState {
    /// The agent reaches the server and follows the document: edits go out as they are made.
    pub online: bool,
    /// Changes made on this machine the server hasn't acked yet.
    pub waiting: u64,
    /// The size of the binary state that follows `opened` (0 in `status`).
    #[serde(default)]
    pub bytes: u64,
    /// How many `update` frames follow the state as part of it (updates still waiting for their history): an app
    /// imports them with the state before it writes with its peer (0 in `status`, and from an older agent).
    #[serde(default)]
    pub updates: u64,
}

/// Why a file opens as a plain file rather than a live document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotLiveReason {
    /// It has never been live and the agent can't reach a server that has live documents.
    Offline,
    /// The file isn't uploaded yet (the agent started a sync pass for it; open it again later).
    NotSynced,
    /// The agent isn't signed in.
    NotSignedIn,
    /// The file, or its content as a document, is over the limit for live documents.
    TooLarge,
    /// The path isn't a file inside the synced folder.
    NotInFolder,
    /// The server's document was reset and its machines haven't sent it back yet: the file is what to open (it is
    /// live again once one of them has).
    Resetting,
}

/// A control message from the agent on the docs WebSocket ([`docs`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum DocsEvent {
    /// The document is live: its state follows as a binary frame of `state.bytes` bytes. `changed_outside`, for an
    /// open that named its `file_hash`: whether no Craft app's save of this document (this machine's records, or the
    /// file version's on the server) has that content: the file was edited outside Craft, and the app may offer to
    /// bring it in. None: the agent can't tell. (Every app session writes with a fresh random Loro peer of its own.)
    Opened {
        id: u64,
        doc: crate::ids::DocId,
        state: DocState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        changed_outside: Option<bool>,
        /// The account may not change this document (a viewer of its team space, or the space is archived): its updates
        /// are refused with `read_only` and the agent's copy stays as it is. Absent from an older agent.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        read_only: bool,
        /// The space the document belongs to (the personal space's ID, or a team space's). Absent from an older agent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        space: Option<SpaceId>,
    },
    /// The file has never been live: send its content as a binary import frame.
    NeedsImport { id: u64 },
    /// The file opens as a plain file.
    NotLive {
        id: u64,
        reason: NotLiveReason,
        #[serde(default)]
        message: Option<String>,
    },
    /// The app's update number `seq` of document `id` is on the agent's disk.
    Ack { id: u64, seq: u64 },
    /// How an open document stands, when that changes. `deleted`: its file was deleted on this or another machine
    /// (the document still works; a later open of the same path is a new file and a new document). `stopped`: the
    /// agent stopped following it until it restarts (Loro failed on it twice); changes made meanwhile are kept.
    Status {
        doc: crate::ids::DocId,
        online: bool,
        waiting: u64,
        #[serde(default)]
        deleted: bool,
        #[serde(default)]
        stopped: bool,
    },
    /// The account's right to change the space of an open document changed (`read_only`: a viewer, or the space is archived;
    /// both ways, also when it ends). `opened.read_only` is as of the open; this follows it, so an app that kept changes
    /// the agent refused can send them again once it says `false`.
    Role { doc: crate::ids::DocId, read_only: bool },
    /// The file of an open document, as it is on this machine's disk now: the live document's frontiers a Craft app
    /// saved it from (the record of that save, this machine's or the server's version's), or none when no record is
    /// known (edited outside Craft, not synced yet). Sent after `opened` and whenever it changes (the app saved, the
    /// sync brought another machine's save). An app whose live document is strictly ahead of it saves again.
    File {
        doc: crate::ids::DocId,
        #[serde(default)]
        frontiers: Option<Vec<(u64, u32)>>,
    },
    /// Who is in the space of the document opened as `id`: the active members, sent after `opened` and whenever the list
    /// changes, only to an app that opened the document with `identity: true`. In a personal space the list is the
    /// account alone.
    Members { id: u64, members: Vec<DocMember> },
    /// A request or a binary frame was refused (`id` and `seq` name it when they can). `code` is one of
    /// [`crate::api::codes`] or [`codes`].
    Error {
        #[serde(default)]
        id: Option<u64>,
        #[serde(default)]
        seq: Option<u64>,
        code: String,
        message: String,
    },
}

/// A member of the space of an open document, as `members` lists it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocMember {
    /// The account's ID (what [`docs::PRESENCE_FROM`] frames name).
    pub user: uuid::Uuid,
    /// The account's username.
    pub name: String,
    /// The palette index (0 to 11) the account is shown in ([`crate::colour`]), the same on every machine.
    pub colour: u8,
    /// This is the account the agent is signed in as.
    #[serde(rename = "self", default)]
    pub is_self: bool,
}

impl std::fmt::Debug for DocMember {
    /// Names the account by its ID: a username never reaches a debug print or a log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocMember")
            .field("user", &self.user)
            .field("name", &format_args!("<{} bytes>", self.name.len()))
            .field("colour", &self.colour)
            .field("is_self", &self.is_self)
            .finish()
    }
}

/// `path` as a recent file's path: `/`-separated names relative to the synced folder, each a valid [`Name`]
/// (NFC-normalised; no `.`, `..`, empty or reserved name, no drive letter), at most [`MAX_RECENT_PATH_BYTES`] and
/// [`MAX_TREE_DEPTH`] names. `None` for anything else.
pub fn recent_path(path: &str) -> Option<String> {
    if path.is_empty() || path.len() > MAX_RECENT_PATH_BYTES {
        return None;
    }
    let names = path.split('/').map(|part| Name::new(part).ok()).collect::<Option<Vec<Name>>>()?;
    if names.len() > MAX_TREE_DEPTH {
        return None;
    }
    let joined = names.iter().map(Name::as_str).collect::<Vec<_>>().join("/");
    (joined.len() <= MAX_RECENT_PATH_BYTES).then_some(joined)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A save record names frontiers, sorted and distinct, at most [`MAX_SAVE_HEADS`] of them.
    #[test]
    fn a_save_record_is_its_frontiers() {
        let doc = crate::ids::DocId::generate();
        assert!(SaveRecord { doc, frontiers: vec![(1, 5), (2, 0)] }.is_valid());
        assert!(SaveRecord { doc, frontiers: Vec::new() }.is_valid(), "an empty document");
        assert!(!SaveRecord { doc, frontiers: vec![(2, 0), (1, 5)] }.is_valid(), "unsorted");
        assert!(!SaveRecord { doc, frontiers: vec![(1, 5), (1, 5)] }.is_valid(), "twice");
        assert!(!SaveRecord { doc, frontiers: (0..=MAX_SAVE_HEADS as u64).map(|p| (p, 1)).collect() }.is_valid());
    }

    /// A list's event says when its push stopped; without the field (an older agent) it hasn't.
    #[test]
    fn a_list_says_when_its_push_stopped() {
        let stopped = RecentsEvent::Entries { app: "wordcraft".into(), entries: Vec::new(), push_stopped: true, space: None };
        let json = serde_json::to_string(&stopped).unwrap();
        assert!(json.contains("\"push_stopped\":true"), "{json}");
        assert_eq!(serde_json::from_str::<RecentsEvent>(&json).unwrap(), stopped);
        let older: RecentsEvent = serde_json::from_str(r#"{"type":"entries","app":"wordcraft","entries":[]}"#).unwrap();
        assert_eq!(older, RecentsEvent::Entries { app: "wordcraft".into(), entries: Vec::new(), push_stopped: false, space: None });
        let plain = serde_json::to_string(&older).unwrap();
        assert!(!plain.contains("push_stopped"), "{plain}");
    }

    #[test]
    fn the_data_folder_comes_from_the_variable_or_the_platform() {
        let env = |name: &str| match name {
            "HOME" => Some(OsString::from("/home/ana")),
            "XDG_DATA_HOME" => Some(OsString::from("relative/ignored")),
            "LOCALAPPDATA" => Some(OsString::from(r"C:\Users\ana\AppData\Local")),
            _ => None,
        };
        let dir = data_dir(env).unwrap();
        assert!(dir.ends_with(DATA_DIR_NAME));
        if cfg!(windows) {
            assert!(dir.starts_with(r"C:\Users\ana\AppData\Local"));
        } else if cfg!(target_vendor = "apple") {
            assert_eq!(dir, PathBuf::from("/home/ana/Library/Application Support/craft-cloud-agent"));
        } else {
            assert_eq!(dir, PathBuf::from("/home/ana/.local/share/craft-cloud-agent"));
        }
        let set = |name: &str| (name == DATA_DIR_ENV).then(|| OsString::from("/data/agent"));
        assert_eq!(data_dir(set), Some(PathBuf::from("/data/agent")));
        let empty = |name: &str| (name == DATA_DIR_ENV).then(OsString::new);
        assert_eq!(data_dir(empty), None, "an empty variable is unset; nothing else is set here");
        assert_eq!(default_data_dir(|_| None), None);
    }

    #[test]
    fn secrets_stay_out_of_debug_output() {
        let file = LocalApiFile { schema: LOCAL_API_SCHEMA, port: 4000, token: "tok-secret".into(), pid: 1 };
        assert!(!format!("{file:?}").contains("tok-secret"));
        let login = LoginRequest {
            server: "https://c.example".into(),
            user: "ana".into(),
            password: "hunter2-secret".into(),
            folder: None,
            device_name: None,
            adopt: false,
        };
        assert!(!format!("{login:?}").contains("hunter2-secret"));
        let join = JoinRequest {
            join: "cloud.example.com/K7M4-Q9TX".into(),
            folder: None,
            device_name: None,
            adopt: false,
            password: Some("hunter2-secret".into()),
            totp_code: Some("918273".into()),
            recovery_code: Some("RECOVERY-SECRET".into()),
        };
        let shown = format!("{join:?}");
        assert!(!shown.contains("K7M4") && !shown.contains("hunter2-secret") && !shown.contains("918273") && !shown.contains("RECOVERY"), "{shown}");
        let second = SecondFactorLoginRequest {
            server: "https://c.example".into(),
            user: "ana".into(),
            folder: None,
            device_name: None,
            adopt: false,
            challenge: "challenge-secret".into(),
            totp_code: Some("918273".into()),
            recovery_code: Some("RECOVERY-SECRET".into()),
        };
        let shown = format!("{second:?}");
        assert!(!shown.contains("challenge-secret") && !shown.contains("918273") && !shown.contains("RECOVERY"), "{shown}");
        let proof =
            TwoFactorProofRequest { password: "pw-secret-1".into(), totp_code: Some("918273".into()), recovery_code: Some("RECOVERY-SECRET".into()) };
        let shown = format!("{proof:?} {:?} {:?}", PasswordRequest { password: "pw-secret-1".into() }, TotpRequest { totp_code: "918273".into() });
        assert!(!shown.contains("pw-secret-1") && !shown.contains("918273") && !shown.contains("RECOVERY"), "{shown}");
        let change = format!("{:?}", ChangePasswordRequest { current_password: "old-secret-pw".into(), new_password: "new-secret-pw".into() });
        assert!(!change.contains("secret-pw"), "{change}");
        let pairing = PairingInfo {
            id: uuid::Uuid::nil(),
            join: "cloud.example.com/K7M4-Q9TX".into(),
            link: "https://cloud.example.com/join/K7M4-Q9TX".into(),
            expires_ms: 1,
        };
        assert!(!format!("{pairing:?}").contains("K7M4"));
        let minimal: JoinRequest = serde_json::from_value(serde_json::json!({ "join": "x/y" })).unwrap();
        assert!(!minimal.adopt && minimal.password.is_none() && minimal.totp_code.is_none() && minimal.recovery_code.is_none());
        // An older app's body reads, and a new body without a second factor is the old one.
        assert_eq!(
            serde_json::to_value(&minimal).unwrap(),
            serde_json::json!({ "join": "x/y", "folder": null, "device_name": null, "adopt": false, "password": null })
        );
        // The extras of an error answer are optional and ignored by a reader that doesn't know them.
        let extras: ErrorExtras = serde_json::from_value(serde_json::json!({ "error": "x", "message": "m" })).unwrap();
        assert_eq!(extras, ErrorExtras::default());
        let extras: ErrorExtras = serde_json::from_value(
            serde_json::json!({ "error": "two_factor_required", "two_factor": { "challenge": "c", "expires_ms": 5 }, "retry_after_secs": 7 }),
        )
        .unwrap();
        assert_eq!((extras.two_factor.map(|t| t.expires_ms), extras.retry_after_secs), (Some(5), Some(7)));
    }

    #[test]
    fn messages_have_a_plain_json_form() {
        let req: RecentsRequest =
            serde_json::from_value(serde_json::json!({ "type": "add", "id": 3, "app": "wordcraft", "entry": { "path": "a/b.docx" } })).unwrap();
        assert_eq!(
            req,
            RecentsRequest::Add { id: 3, app: "wordcraft".into(), entry: NewRecent { path: "a/b.docx".into(), title: None, saved: false } }
        );
        let status = Status {
            api: 1,
            signed_in: false,
            account: None,
            server: None,
            folder: None,
            machine: None,
            online: false,
            run_state: RunState::SignedOut,
            autostart: None,
            last_sync_ms: Some(1_790_000_000_000),
            removed: false,
            spaces: Vec::new(),
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["run_state"], serde_json::json!({ "kind": "signed_out" }));
        assert_eq!(serde_json::from_value::<Status>(json).unwrap(), status);
        let paused = serde_json::to_value(RunState::PausedByUser).unwrap();
        assert_eq!(paused, serde_json::json!({ "kind": "paused_by_user" }));
        let older: Status = serde_json::from_value(serde_json::json!({
            "api": 1, "signed_in": false, "online": false, "run_state": { "kind": "signed_out" }
        }))
        .unwrap();
        assert_eq!(older.autostart, None, "an older agent says nothing about autostart");
        assert_eq!(older.last_sync_ms, None, "nor about the last sync");
        assert!(!older.removed);
        assert!(older.spaces.is_empty(), "nor about team spaces");
        let login: LoginRequest = serde_json::from_value(serde_json::json!({ "server": "s", "user": "u", "password": "p", "extra": 1 })).unwrap();
        assert!(!login.adopt && login.folder.is_none(), "missing optional fields default; unknown ones are ignored");
    }

    #[test]
    fn spaces_have_a_plain_form_and_their_debug_output_names_nothing() {
        let space = SpaceId::generate();
        let info = SpaceInfo {
            space,
            kind: "team".into(),
            name: Some("Studio".into()),
            role: SpaceRole::Viewer,
            state: "active".into(),
            archived: false,
            folder: Some("/home/ana/Craft Cloud Shared/Studio".into()),
            sync: SpaceSync::On,
            run_state: Some(SpaceRun::NoAccess { message: "You no longer have access to Studio. Your local copy is kept.".into() }),
            last_sync_ms: None,
            read_only: true,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["sync"], "on");
        assert_eq!(json["run_state"]["kind"], "no_access");
        assert!(json.get("last_sync_ms").is_none());
        assert_eq!(serde_json::from_value::<SpaceInfo>(json).unwrap(), info);
        let shown = format!("{info:?}");
        assert!(!shown.contains("Studio") && !shown.contains("Craft Cloud Shared"), "{shown}");
        assert!(shown.contains("NoAccess"), "the variant is still named: {shown}");
        let status = SpaceStatus { space, run_state: info.run_state.clone().unwrap(), last_sync_ms: None, read_only: false };
        let shown = format!("{status:?}");
        assert!(!shown.contains("Studio"), "the status leaves the name out too: {shown}");
        let answer = ApproveDeletesAnswer { approved: true, message: "delete on the server: Studio/plan.txt".into() };
        assert!(!format!("{answer:?}").contains("plan.txt"));
        let older: Status = serde_json::from_value(serde_json::json!({
            "api": 1, "signed_in": true, "online": true, "run_state": { "kind": "idle" }
        }))
        .unwrap();
        assert!(older.spaces.is_empty());
        let request: SpaceSyncRequest = serde_json::from_value(serde_json::json!({ "on": true })).unwrap();
        assert_eq!(request, SpaceSyncRequest { on: true, folder: None, adopt: false });
    }

    #[test]
    fn docs_messages_have_a_plain_form() {
        let frame = docs::encode_binary(docs::UPDATE, 0x0102_0304_0506_0708, b"abc");
        assert_eq!(frame, vec![2, 1, 2, 3, 4, 5, 6, 7, 8, b'a', b'b', b'c']);
        assert_eq!(docs::decode_binary(&frame), Some((docs::UPDATE, 0x0102_0304_0506_0708, &b"abc"[..])));
        assert_eq!(docs::decode_binary(&frame[..9]), Some((docs::UPDATE, 0x0102_0304_0506_0708, &b""[..])));
        assert_eq!(docs::decode_binary(&frame[..8]), None, "too short for a header");
        assert_eq!(docs::decode_binary(&[]), None);
        let open: DocsRequest = serde_json::from_value(serde_json::json!({ "type": "open", "id": 1, "path": "a.txt", "app": "wordcraft" })).unwrap();
        assert_eq!(open, DocsRequest::Open { id: 1, path: "a.txt".into(), app: "wordcraft".into(), file_hash: None, identity: false });
        let plain = serde_json::to_value(DocsEvent::Opened {
            id: 1,
            doc: crate::ids::DocId::generate(),
            state: DocState::default(),
            changed_outside: None,
            read_only: false,
            space: None,
        })
        .unwrap();
        assert!(plain.get("changed_outside").is_none(), "none: no field (as an older agent sends it)");
        assert!(plain.get("read_only").is_none() && plain.get("space").is_none(), "the team fields are left out when they say nothing");
        let not_live = serde_json::to_value(DocsEvent::NotLive { id: 1, reason: NotLiveReason::NotSynced, message: None }).unwrap();
        assert_eq!(not_live, serde_json::json!({ "type": "not_live", "id": 1, "reason": "not_synced", "message": null }));
        let status: DocsEvent =
            serde_json::from_value(serde_json::json!({ "type": "status", "doc": uuid::Uuid::nil(), "online": true, "waiting": 2 })).unwrap();
        assert!(matches!(status, DocsEvent::Status { online: true, waiting: 2, deleted: false, .. }));
    }

    /// Team spaces on the documents socket: `identity` is asked for, `read_only` and the space come with `opened`, the member
    /// list names accounts (`self` on the wire), and nothing a debug print shows has a name in it.
    #[test]
    fn the_docs_messages_for_team_spaces() {
        let identity: DocsRequest =
            serde_json::from_value(serde_json::json!({ "type": "open", "id": 1, "path": "a.txt", "app": "wordcraft", "identity": true })).unwrap();
        assert!(matches!(identity, DocsRequest::Open { identity: true, .. }));
        assert!(serde_json::to_value(&identity).unwrap().get("identity").is_some());
        let space = SpaceId::generate();
        let opened = DocsEvent::Opened {
            id: 1,
            doc: crate::ids::DocId::generate(),
            state: DocState::default(),
            changed_outside: None,
            read_only: true,
            space: Some(space),
        };
        let json = serde_json::to_value(&opened).unwrap();
        assert_eq!(json.get("read_only"), Some(&serde_json::json!(true)));
        assert_eq!(json.get("space"), Some(&serde_json::json!(space)));
        assert_eq!(serde_json::from_value::<DocsEvent>(json).unwrap(), opened);
        // An older agent's `opened` has neither.
        let old: DocsEvent = serde_json::from_value(serde_json::json!({
            "type": "opened", "id": 1, "doc": crate::ids::DocId::generate(), "state": { "online": true, "waiting": 0 }
        }))
        .unwrap();
        assert!(matches!(old, DocsEvent::Opened { read_only: false, space: None, .. }));
        let user = uuid::Uuid::from_bytes([7; 16]);
        let members = DocsEvent::Members { id: 3, members: vec![DocMember { user, name: "ana".into(), colour: 4, is_self: true }] };
        let json = serde_json::to_value(&members).unwrap();
        assert_eq!(json["members"][0]["self"], serde_json::json!(true), "the wire word is `self`");
        assert_eq!(json["members"][0]["name"], serde_json::json!("ana"));
        assert_eq!(serde_json::from_value::<DocsEvent>(json).unwrap(), members);
        let shown = format!("{members:?}");
        assert!(!shown.contains("ana"), "a name never reaches a debug print: {shown}");
        // The binary frame of presence with an identity.
        assert_eq!(docs::PRESENCE_FROM, 5);
        let frame = docs::encode_binary(docs::PRESENCE_FROM, 9, &[user.as_bytes().as_slice(), b"cursor"].concat());
        let (kind, id, body) = docs::decode_binary(&frame).unwrap();
        assert_eq!((kind, id), (5, 9));
        assert_eq!(body.split_at(docs::USER_BYTES), (user.as_bytes().as_slice(), b"cursor".as_slice()));
        // A change an app hands over, and the role that follows `opened`.
        assert_eq!(docs::HELD, 6);
        let role = DocsEvent::Role { doc: crate::ids::DocId::generate(), read_only: false };
        let json = serde_json::to_value(&role).unwrap();
        assert_eq!(json["type"], serde_json::json!("role"));
        assert_eq!(serde_json::from_value::<DocsEvent>(json).unwrap(), role);
    }

    /// A recents subscription may name a team space; without one it is the personal list, on the wire as before.
    #[test]
    fn a_recents_subscription_may_name_a_space() {
        let personal: RecentsRequest = serde_json::from_value(serde_json::json!({ "type": "subscribe", "app": "wordcraft" })).unwrap();
        assert_eq!(personal, RecentsRequest::Subscribe { app: "wordcraft".into(), space: None });
        assert!(serde_json::to_value(&personal).unwrap().get("space").is_none());
        let space = SpaceId::generate();
        let team = RecentsRequest::Subscribe { app: "wordcraft".into(), space: Some(space) };
        assert_eq!(serde_json::from_value::<RecentsRequest>(serde_json::to_value(&team).unwrap()).unwrap(), team);
        let list = RecentsEvent::Entries { app: "wordcraft".into(), entries: Vec::new(), push_stopped: false, space: Some(space) };
        assert_eq!(serde_json::to_value(&list).unwrap()["space"], serde_json::json!(space));
        let personal_list = RecentsEvent::Entries { app: "wordcraft".into(), entries: Vec::new(), push_stopped: false, space: None };
        assert!(serde_json::to_value(&personal_list).unwrap().get("space").is_none(), "an older client sees what it saw");
    }

    #[test]
    fn recent_paths_are_relative_and_clean() {
        assert_eq!(recent_path("Docs/report.docx").as_deref(), Some("Docs/report.docx"));
        assert_eq!(recent_path("Cafe\u{301}.txt").as_deref(), Some("Café.txt"), "NFC");
        for bad in ["", "/abs.txt", "a//b", "../x", "a/./b", "C:/x", "a\\b", "x/", "con.txt", &"a/".repeat(300)] {
            assert_eq!(recent_path(bad), None, "{bad:?}");
        }
    }
}
