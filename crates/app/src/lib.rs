//! What Craft apps and the launcher link to reach the Craft Cloud agent on this machine (`craft-cloud-agent run`)
//! through its local API: the account's status, signing in and out, joining with a code and adding computers (the
//! Craft Cloud app's), pausing, quitting and start-at-login, each app's recent files, shared by every machine of the
//! account, and live documents ([`live`]: files opened by path, their changes going to the other machines as they are
//! made, at the level of Loro's bytes).
//!
//! Blocking, with no async runtime: an egui app calls it and gets changes on a channel ([`RecentsWatch::events`]),
//! which it polls each frame.
//!
//! **From a UI thread** (each call is bounded by a short timeout, the agent being on this machine): [`Client::connect`]
//! and [`Client::status`] (about 2 s at most), [`Client::recents`], [`Client::add_recent`],
//! [`Client::watch_recents`] and [`RecentsWatch::add`], [`Client::pause`], [`Client::resume`], [`Client::quit`] and
//! [`Client::set_autostart`] (a few seconds at most). **Off the UI thread**, since they wait for the server (up to
//! 90 s): [`Client::login`], [`Client::join`], [`Client::logout`], and the add-a-computer and computer-list calls
//! ([`Client::pairing`], [`Client::pairing_state`], [`Client::cancel_pairing`], [`Client::devices`],
//! [`Client::remove_device`]), the admin calls ([`admin`]), and opening a live document ([`live::Documents::open`], [`live::Importer::import`]: up
//! to 30 s). A live document's own calls ([`live::Document::send`], [`live::Document::try_recv`]) don't wait for an
//! answer.
//!
//! ```no_run
//! # fn main() -> Result<(), craft_cloud_app::Error> {
//! let agent = craft_cloud_app::Client::connect()?;
//! let status = agent.status()?;
//! if status.signed_in {
//!     let recents = agent.watch_recents("wordcraft")?;
//!     recents.add(craft_cloud_app::NewRecent { path: "Letters/doc1.docx".into(), title: None, saved: true })?;
//!     for event in recents.events().try_iter() {
//!         // Redraw the list, or show "Craft Cloud isn't running" while it is unavailable.
//!         let _ = event;
//!     }
//! }
//! # Ok(()) }
//! ```
//!
//! [`Client::connect`] finds the agent as the agent finds its data folder (`$CRAFT_CLOUD_AGENT_DIR`, else the per-user
//! data folder), reads `local-api.json` there and asks the agent for its status; [`Error::NotRunning`] when the file
//! is missing or stale (its process is gone, or no agent answers on its port with its token). The app then works as
//! without Craft Cloud ("Craft Cloud isn't running"); it never blocks on it. A [`RecentsWatch`] outlives the agent:
//! when the agent stops or signs out it says so ([`RecentsEvent::Unavailable`]) and connects again by itself, with a
//! growing pause, to whichever agent runs next ([`RecentsEvent::Available`]).
//!
//! Binary content kept out of live documents goes through [`blobs`] (the agent's `/v1/blobs`).
//!
//! With the `fake` feature (tests only), [`fake`] is an agent in this process: its documents and blobs live in memory,
//! shared by every fake on one [`fake::Hub`], so two app instances can be tested together with no server.
//!
//! Dependencies: `craft-cloud-api` (the local API types; serde, uuid and Unicode tables, no postcard), `serde_json`,
//! `thiserror`, `httparse`, and `tungstenite` without TLS (with `http`, `sha1`, `rand`, `data-encoding`, `utf-8`). No
//! TLS: the agent is on this machine.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod admin;
mod audit;
pub mod blobs;
#[cfg(feature = "fake")]
pub mod fake;
mod http;
pub mod live;
mod recents;
mod spaces;

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use craft_cloud_api::api::ErrorAnswer;
use craft_cloud_api::local::{
    AutostartRequest, ChangePasswordRequest, ErrorExtras, LOCAL_API_FILE, LOCAL_API_MAX, LOCAL_API_MIN, LocalApiFile, LogoutRequest, PasswordRequest,
    RenameDeviceRequest, TotpRequest, paths,
};

pub use admin::UserQuery;
pub use audit::{AuditQuery, ExportOutcome};
pub use craft_cloud_api::DeviceId;
pub use craft_cloud_api::api::{
    AUDIT_ACTIONS, AccountInfo, AccountUsage, AdminCode, AdminCompression, AdminCompressionEstimate, AdminCompressionLevelEstimate, AdminConfirm,
    AdminCreateUser, AdminCreatedUser, AdminDevices, AdminManifestIndex, AdminPasswordReset, AdminServerInfo, AdminSetCompressionLevel,
    AdminStorageAccount, AdminStorageOverview, AdminUpdateUser, AdminUserInfo, AdminUserList, AuditEvent, AuditPage, DeviceInfo, PairingState,
    PairingStatus, RecoveryCodes, RevokedOthers, TwoFactorChallenge, TwoFactorSetupResponse, TwoFactorStatus, UsageEstimateReason, UsageEstimates,
    valid_audit_action,
};
pub use craft_cloud_api::api::{
    AddSpaceMember, AdminCreateSpace, AdminSetSpaceOwner, AdminSpaceInfo, AdminSpaceList, ChangeSpaceMember, SpaceConfirm, SpaceListItem,
    SpaceMember, SpaceMembers, SpaceRole, SpaceUsage, UpdateSpace,
};
pub use craft_cloud_api::local::{
    ApproveDeletesAnswer, ApproveDeletesRequest, FromFolderRequest, FromFolderStatus, FromFolderStep, HeldEntry, HeldLink, SpaceInfo, SpaceList,
    SpaceRun, SpaceStatus, SpaceSync, SpaceSyncRequest,
};
pub use craft_cloud_api::local::{
    ConflictCopy, ConflictsResponse, FileKind, FolderUsage, FolderUsageResponse, HistoryResponse, HistoryVersion, JoinRequest, LoginRequest,
    NewRecent, PairingInfo, RecentEntry, RestoreResponse, RunState, SecondFactorLoginRequest, Status, TrashItem, TrashNodeRequest, TrashResponse,
    TrashRestoreResponse, TwoFactorProofRequest, codes,
};
pub use craft_cloud_api::local::{KindUsage, OtherFolders};
pub use craft_cloud_api::{NodeId, SpaceId, VersionId};
pub use recents::{RecentsEvent, RecentsWatch};
pub use uuid::Uuid;

/// What went wrong talking to the agent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// No agent runs here (or not under this user): live features are off.
    #[error("Craft Cloud isn't running on this computer ({0})")]
    NotRunning(String),
    /// The agent speaks another local API version: one of the two needs an update.
    #[error("the Craft Cloud agent speaks local API version {0}, this app {LOCAL_API_MIN}-{LOCAL_API_MAX}: update the older one")]
    Version(u32),
    /// The agent refused the request: `code` is one of [`codes`] (or the HTTP API's), `message` is for people, and
    /// `name_taken` identifies a trash-restore conflict when the server provides it.
    #[error("{message}")]
    Refused { code: String, message: String, name_taken: Option<bool> },
    /// A refusal that carries more: the challenge of `two_factor_required` and/or the wait of `two_factor_locked`
    /// (`retry_after_secs`). Only answers with one of those are this; every other refusal is [`Error::Refused`], so code that
    /// matches `Refused { code, .. }` goes on working. [`Error::code`] reads the code of both.
    #[error("{message}")]
    RefusedWith { code: String, message: String, two_factor: Option<TwoFactorChallenge>, retry_after_secs: Option<u64> },
    /// The connection broke, or the agent didn't answer in time.
    #[error("the connection to the Craft Cloud agent broke: {0}")]
    Io(String),
    /// The agent answered something this crate can't read.
    #[error("the Craft Cloud agent answered something unexpected: {0}")]
    Protocol(String),
}

impl Error {
    /// The code of a refusal ([`Error::Refused`] or [`Error::RefusedWith`]); `None` for any other error.
    pub fn code(&self) -> Option<&str> {
        match self {
            Error::Refused { code, .. } | Error::RefusedWith { code, .. } => Some(code),
            _ => None,
        }
    }

    /// The challenge of a `two_factor_required` refusal.
    pub fn challenge(&self) -> Option<&TwoFactorChallenge> {
        match self {
            Error::RefusedWith { two_factor, .. } => two_factor.as_ref(),
            _ => None,
        }
    }

    /// How many seconds a `two_factor_locked` (or other) refusal says to wait.
    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            Error::RefusedWith { retry_after_secs, .. } => *retry_after_secs,
            _ => None,
        }
    }
}

/// The running agent, as `local-api.json` names it.
#[derive(Debug, Clone)]
pub struct Client {
    addr: SocketAddr,
    token: String,
    /// Where `local-api.json` is: a watch looks there again to reconnect.
    dir: PathBuf,
    /// Whether this agent knows `/v1/blobs`, once asked ([`blobs::Blobs`]): shared by the client's clones.
    blobs_known: std::sync::Arc<std::sync::OnceLock<bool>>,
}

impl Client {
    /// Finds the running agent: its data folder from `$CRAFT_CLOUD_AGENT_DIR` or the per-user data folder.
    pub fn connect() -> Result<Self, Error> {
        let dir = craft_cloud_api::local::data_dir(|name| std::env::var_os(name))
            .ok_or_else(|| Error::NotRunning("no user data folder to look in".into()))?;
        Self::connect_in(&dir)
    }

    /// Finds the agent running with the data folder `dir` (its `--data-dir`).
    pub fn connect_in(dir: &Path) -> Result<Self, Error> {
        let file: PathBuf = dir.join(LOCAL_API_FILE);
        let bytes = std::fs::read(&file).map_err(|e| Error::NotRunning(format!("{}: {e}", file.display())))?;
        let record: LocalApiFile = serde_json::from_slice(&bytes).map_err(|e| Error::NotRunning(format!("{} doesn't read: {e}", file.display())))?;
        // A file a crashed agent left behind names a port nobody, or someone else, answers on: the connection tells.
        // (Its pid isn't used for that: an app in a sandbox with a PID namespace of its own can't see the agent's.)
        let client = Self {
            addr: SocketAddr::from((Ipv4Addr::LOCALHOST, record.port)),
            token: record.token,
            dir: dir.to_owned(),
            blobs_known: Default::default(),
        };
        match client.status() {
            Ok(status) if (LOCAL_API_MIN..=LOCAL_API_MAX).contains(&status.api) => Ok(client),
            Ok(status) => Err(Error::Version(status.api)),
            Err(Error::Refused { code, .. }) if code == "unauthorized" => Err(Error::NotRunning(format!("{} is stale", file.display()))),
            Err(Error::Io(e) | Error::Protocol(e)) => Err(Error::NotRunning(format!("{} is stale ({e})", file.display()))),
            Err(e) => Err(e),
        }
    }

    pub(crate) fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    fn call<T: serde::de::DeserializeOwned>(&self, method: &str, path: &str, body: Option<Vec<u8>>, timeout: Duration) -> Result<T, Error> {
        let answer = self.answer(method, path, body, timeout)?;
        serde_json::from_slice(&answer.body).map_err(|e| Error::Protocol(e.to_string()))
    }

    /// A call with a body that is answered with no body (`204`).
    fn call_unit(&self, method: &str, path: &str, body: Option<Vec<u8>>, timeout: Duration) -> Result<(), Error> {
        self.answer(method, path, body, timeout).map(|_| ())
    }

    /// A call answered without a body (`204`).
    fn call_empty(&self, method: &str, path: &str, timeout: Duration) -> Result<(), Error> {
        self.answer(method, path, None, timeout).map(|_| ())
    }

    /// The answer if it succeeded, else the agent's refusal.
    fn answer(&self, method: &str, path: &str, body: Option<Vec<u8>>, timeout: Duration) -> Result<http::Answer, Error> {
        let answer = http::request(self.addr, &self.token, method, path, body.as_deref(), timeout)?;
        refusal(answer)
    }

    /// A call with a binary body and a binary answer of at most `max` bytes (blobs).
    pub(crate) fn binary(&self, method: &str, path: &str, body: Option<&[u8]>, max: usize, timeout: Duration) -> Result<http::Answer, Error> {
        let answer = http::request_with(self.addr, &self.token, method, path, body, "application/octet-stream", max, timeout)?;
        refusal(answer)
    }

    /// A client of the agent at `addr` with `token` (a fake agent's, in tests), whose `local-api.json` is in `dir`.
    #[cfg(feature = "fake")]
    pub(crate) fn at(addr: SocketAddr, token: String, dir: PathBuf) -> Self {
        Self { addr, token, dir, blobs_known: Default::default() }
    }

    /// Whether the agent is signed in, as whom, to which server, whether it is online and what it is doing. Quick
    /// (about 2 s at most): fine from a UI thread.
    pub fn status(&self) -> Result<Status, Error> {
        self.call("GET", paths::STATUS, None, http::QUICK_TIMEOUT)
    }

    /// Signs the agent in (the launcher's call). The password goes to the agent only. A wrong one is
    /// `Refused { code: "wrong_password" }`. Waits for the server (up to 90 s): call it off the UI thread.
    pub fn login(&self, request: &LoginRequest) -> Result<Status, Error> {
        let body = serde_json::to_vec(request).map_err(|e| Error::Protocol(e.to_string()))?;
        self.call("POST", paths::LOGIN, Some(body), http::SLOW_TIMEOUT)
    }

    /// The second step of a sign-in that answered `Refused`/`RefusedWith { code: "two_factor_required" }`: the first step's
    /// server, folder, computer name and `adopt` again, with the answer's challenge ([`Error::challenge`]) and a code from
    /// the authenticator app or a recovery code. A wrong code is `two_factor_invalid` (the challenge takes five), too many
    /// is `two_factor_locked` with [`Error::retry_after_secs`]; neither signs anything out. Waits for the server: call it off
    /// the UI thread.
    pub fn login_second_factor(&self, request: &SecondFactorLoginRequest) -> Result<Status, Error> {
        let body = serde_json::to_vec(request).map_err(|e| Error::Protocol(e.to_string()))?;
        self.call("POST", paths::LOGIN_SECOND_FACTOR, Some(body), http::SLOW_TIMEOUT)
    }

    /// Whether the signed-in account has two-factor sign-in on, how many recovery codes are left and whether a set-up is
    /// open. Asks the server: call it off the UI thread. A server from before two-factor sign-in answers `not_found`.
    pub fn two_factor_status(&self) -> Result<TwoFactorStatus, Error> {
        self.call("GET", paths::TWO_FACTOR, None, http::SLOW_TIMEOUT)
    }

    /// Starts setting two-factor sign-in up with the account's password: the secret (Base32 and as an `otpauth://` URI, for
    /// a QR code), good for 10 minutes. `wrong_password` (403) is no sign-out. The secret is shown once and never stored.
    pub fn two_factor_setup(&self, password: &str) -> Result<TwoFactorSetupResponse, Error> {
        let body = serde_json::to_vec(&PasswordRequest { password: password.to_owned() }).map_err(|e| Error::Protocol(e.to_string()))?;
        self.call("POST", paths::TWO_FACTOR_SETUP, Some(body), http::SLOW_TIMEOUT)
    }

    /// Turns two-factor sign-in on with the first code; the ten recovery codes, shown once (the account's other computers
    /// are signed out).
    pub fn two_factor_confirm(&self, totp_code: &str) -> Result<RecoveryCodes, Error> {
        let body = serde_json::to_vec(&TotpRequest { totp_code: totp_code.to_owned() }).map_err(|e| Error::Protocol(e.to_string()))?;
        self.call("POST", paths::TWO_FACTOR_CONFIRM, Some(body), http::SLOW_TIMEOUT)
    }

    /// Turns two-factor sign-in off: the password and a code or a recovery code.
    pub fn two_factor_disable(&self, request: &TwoFactorProofRequest) -> Result<(), Error> {
        let body = serde_json::to_vec(request).map_err(|e| Error::Protocol(e.to_string()))?;
        self.call_unit("POST", paths::TWO_FACTOR_DISABLE, Some(body), http::SLOW_TIMEOUT)
    }

    /// Ten new recovery codes (the old ones stop working): the password and a code from the authenticator app.
    pub fn two_factor_recovery_codes(&self, password: &str, totp_code: &str) -> Result<RecoveryCodes, Error> {
        let request = TwoFactorProofRequest { password: password.to_owned(), totp_code: Some(totp_code.to_owned()), recovery_code: None };
        let body = serde_json::to_vec(&request).map_err(|e| Error::Protocol(e.to_string()))?;
        self.call("POST", paths::TWO_FACTOR_RECOVERY_CODES, Some(body), http::SLOW_TIMEOUT)
    }

    /// Changes the account's password; the account's other computers are signed out.
    pub fn change_password(&self, current_password: &str, new_password: &str) -> Result<(), Error> {
        let request = ChangePasswordRequest { current_password: current_password.to_owned(), new_password: new_password.to_owned() };
        let body = serde_json::to_vec(&request).map_err(|e| Error::Protocol(e.to_string()))?;
        self.call_unit("POST", paths::ACCOUNT_PASSWORD, Some(body), http::SLOW_TIMEOUT)
    }

    /// Signs every other computer of the account out; how many were.
    pub fn revoke_other_devices(&self) -> Result<RevokedOthers, Error> {
        self.call("POST", paths::DEVICES_REVOKE_OTHERS, None, http::SLOW_TIMEOUT)
    }

    /// Renames one of the account's computers that isn't signed out.
    pub fn rename_device(&self, id: DeviceId, name: &str) -> Result<DeviceInfo, Error> {
        let body = serde_json::to_vec(&RenameDeviceRequest { name: name.to_owned() }).map_err(|e| Error::Protocol(e.to_string()))?;
        self.call("PATCH", &format!("{}/{id}", paths::DEVICES), Some(body), http::SLOW_TIMEOUT)
    }

    /// Signs the agent out (the files stay). `local`: without reaching the server (the device stays valid there).
    /// Waits for the server (up to 90 s): call it off the UI thread.
    pub fn logout(&self, local: bool) -> Result<Status, Error> {
        let body = serde_json::to_vec(&LogoutRequest { local }).map_err(|e| Error::Protocol(e.to_string()))?;
        self.call("POST", paths::LOGOUT, Some(body), http::SLOW_TIMEOUT)
    }

    /// Joins a Craft Cloud with a join string (an invite, or an add-a-computer code from a signed-in computer), then
    /// syncs as after [`Client::login`]. An invite to a new account without a password is
    /// `Refused { code: "password_required" }`: ask for one (twice) and call again with it. A code that doesn't work
    /// is `invalid_code`. Waits for the server (up to 90 s): call it off the UI thread.
    pub fn join(&self, request: &JoinRequest) -> Result<Status, Error> {
        let body = serde_json::to_vec(request).map_err(|e| Error::Protocol(e.to_string()))?;
        self.call("POST", paths::JOIN, Some(body), http::SLOW_TIMEOUT)
    }

    /// A new add-a-computer code of the signed-in account, to show on this computer and enter on the new one (it
    /// replaces this computer's earlier one; it works once, for 10 minutes). Asks the server: call it off the UI thread.
    pub fn pairing(&self) -> Result<PairingInfo, Error> {
        self.call("POST", paths::PAIRINGS, None, http::SLOW_TIMEOUT)
    }

    /// Where the add-a-computer code `id` stands: waiting, used (with the new computer's name) or expired. One that is
    /// gone is `Refused { code: "not_found" }`. Asks the server: call it off the UI thread (every 2 s is plenty).
    pub fn pairing_state(&self, id: Uuid) -> Result<PairingStatus, Error> {
        self.call("GET", &format!("{}/{id}", paths::PAIRINGS), None, http::SLOW_TIMEOUT)
    }

    /// Cancels the add-a-computer code `id`. Asks the server: call it off the UI thread.
    pub fn cancel_pairing(&self, id: Uuid) -> Result<(), Error> {
        self.call_empty("DELETE", &format!("{}/{id}", paths::PAIRINGS), http::SLOW_TIMEOUT)
    }

    /// The account's computers, this one marked `current`. Asks the server: call it off the UI thread.
    pub fn devices(&self) -> Result<Vec<DeviceInfo>, Error> {
        self.call("GET", paths::DEVICES, None, http::SLOW_TIMEOUT)
    }

    /// The signed-in account's storage usage and labeled estimates. Asks the server each time.
    pub fn usage(&self) -> Result<AccountUsage, Error> {
        self.call("GET", paths::USAGE, None, http::SLOW_TIMEOUT)
    }

    /// The conflict copies in the synced folder, newest first (at most 1000, then `truncated`). Answered by the agent
    /// from its saved state, with no call to the server, but it reads the whole saved state: call it off the UI thread. A name can match the conflict naming
    /// without being a copy, so the list asks for a file with the original's name beside it (see `docs/agent.md`).
    /// Signed out is `Refused { code: "not_signed_in" }`.
    pub fn conflicts(&self) -> Result<ConflictsResponse, Error> {
        self.call("GET", paths::CONFLICTS, None, http::SLOW_TIMEOUT)
    }

    /// The local logical bytes of the synced files by top-level folder and by kind, from the agent's saved state: not
    /// the server's stored bytes ([`Client::usage`]). Reads the whole saved state: call it off the UI thread. Signed out is
    /// `Refused { code: "not_signed_in" }`.
    pub fn folder_usage(&self) -> Result<FolderUsageResponse, Error> {
        self.call("GET", paths::USAGE_FOLDERS, None, http::SLOW_TIMEOUT)
    }

    /// Removes another computer from the account (its next request is refused; its files stay there), and gives the
    /// list after. This computer is `Refused { code: "cannot_remove_self" }`: that is [`Client::logout`]. Asks the
    /// server: call it off the UI thread.
    pub fn remove_device(&self, id: DeviceId) -> Result<Vec<DeviceInfo>, Error> {
        self.call("DELETE", &format!("{}/{id}", paths::DEVICES), None, http::SLOW_TIMEOUT)
    }

    /// The newest-first server history for a file in the synced folder. `cursor` is the opaque `next` value from the
    /// previous page. Asks the server: call it off the UI thread.
    pub fn file_history(&self, path: &str, cursor: Option<&str>, limit: Option<u32>) -> Result<HistoryResponse, Error> {
        let mut query = format!("path={}", query_component(path));
        if let Some(cursor) = cursor {
            query.push_str(&format!("&cursor={}", query_component(cursor)));
        }
        if let Some(limit) = limit {
            query.push_str(&format!("&limit={limit}"));
        }
        self.call("GET", &format!("{}?{query}", paths::FILE_HISTORY), None, http::SLOW_TIMEOUT)
    }

    /// Restores `version` of a file in the synced folder, if `expected_version` is still current. Asks the server:
    /// call it off the UI thread.
    pub fn restore_file(&self, path: &str, version: VersionId, expected_version: VersionId) -> Result<RestoreResponse, Error> {
        let body = serde_json::to_vec(&craft_cloud_api::local::RestoreRequest { path: path.to_owned(), version, expected_version })
            .map_err(|e| Error::Protocol(e.to_string()))?;
        self.call("POST", paths::FILE_RESTORE, Some(body), http::SLOW_TIMEOUT)
    }

    /// The server's newest-first trash page. `cursor` is the opaque `next` value from the preceding page.
    pub fn trash(&self, cursor: Option<&str>, limit: Option<u32>) -> Result<TrashResponse, Error> {
        let mut query = Vec::new();
        if let Some(cursor) = cursor {
            query.push(format!("cursor={}", query_component(cursor)));
        }
        if let Some(limit) = limit {
            query.push(format!("limit={limit}"));
        }
        let path = if query.is_empty() { paths::TRASH.to_owned() } else { format!("{}?{}", paths::TRASH, query.join("&")) };
        self.call("GET", &path, None, http::SLOW_TIMEOUT)
    }

    /// Restores a node from the server's trash. Call off the UI thread.
    pub fn restore_from_trash(&self, id: NodeId) -> Result<TrashRestoreResponse, Error> {
        let body = serde_json::to_vec(&TrashNodeRequest { id: id.to_string() }).map_err(|e| Error::Protocol(e.to_string()))?;
        self.call("POST", paths::TRASH_RESTORE, Some(body), http::SLOW_TIMEOUT)
    }

    /// Marks a node as removed from the server's trash. This does not free its stored data yet.
    pub fn remove_from_trash(&self, id: NodeId) -> Result<(), Error> {
        let body = serde_json::to_vec(&TrashNodeRequest { id: id.to_string() }).map_err(|e| Error::Protocol(e.to_string()))?;
        self.answer("POST", paths::TRASH_REMOVE, Some(body), http::SLOW_TIMEOUT).map(|_| ())
    }

    /// Pauses syncing until [`Client::resume`], also across restarts; recents keep working. Answered by the agent
    /// itself (a few seconds at most): fine from a UI thread.
    pub fn pause(&self) -> Result<Status, Error> {
        self.call("POST", paths::PAUSE, None, http::COMMAND_TIMEOUT)
    }

    /// Syncs again after [`Client::pause`]. Answered by the agent itself (a few seconds at most): fine from a UI thread.
    pub fn resume(&self) -> Result<Status, Error> {
        self.call("POST", paths::RESUME, None, http::COMMAND_TIMEOUT)
    }

    /// Stops the agent (after the pass under way): nothing syncs until it starts again (at the next login, with
    /// autostart on). Answered at once, as it stops: fine from a UI thread.
    pub fn quit(&self) -> Result<Status, Error> {
        self.call("POST", paths::QUIT, None, http::COMMAND_TIMEOUT)
    }

    /// Starts the agent by itself at login on this computer (`on`), or not any more; [`Status::autostart`] says which.
    /// Answered by the agent itself (it writes a file or a registry value): fine from a UI thread.
    pub fn set_autostart(&self, on: bool) -> Result<Status, Error> {
        let body = serde_json::to_vec(&AutostartRequest { on }).map_err(|e| Error::Protocol(e.to_string()))?;
        self.call("POST", paths::AUTOSTART, Some(body), http::COMMAND_TIMEOUT)
    }

    /// `app`'s recent files now, newest first.
    pub fn recents(&self, app: &str) -> Result<Vec<RecentEntry>, Error> {
        Ok(self.watch_recents(app)?.entries())
    }

    /// Adds a file to `app`'s recent files (kept on this machine's disk when this returns; on the others once the agent
    /// reaches the server).
    pub fn add_recent(&self, app: &str, entry: NewRecent) -> Result<RecentEntry, Error> {
        RecentsWatch::open(self, app, false)?.add(entry)
    }

    /// Follows `app`'s recent files: the list now ([`RecentsWatch::entries`]), and a [`RecentsEvent`] after every change,
    /// from this machine or another, and when the agent goes away or comes back. The connection is held by a thread of
    /// its own, until the watch is dropped. `NotRunning` if no agent runs right now.
    pub fn watch_recents(&self, app: &str) -> Result<RecentsWatch, Error> {
        RecentsWatch::open(self, app, true)
    }

    /// A connection for live documents ([`live::Documents::open`]): files of the synced folder opened by path, their
    /// changes going to and coming from the account's other machines as they are made. `NotRunning` if no agent runs
    /// right now.
    pub fn documents(&self) -> Result<live::Documents, Error> {
        live::Documents::connect(self)
    }

    /// The agent's blob store ([`blobs::Blobs`]): binary content kept out of live documents.
    pub fn blobs(&self) -> blobs::Blobs {
        blobs::Blobs::new(self.clone())
    }
}

fn query_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// `answer` if it succeeded, else the agent's refusal (its code and message).
fn refusal(answer: http::Answer) -> Result<http::Answer, Error> {
    if (200..300).contains(&answer.status) {
        Ok(answer)
    } else {
        match serde_json::from_slice::<ErrorAnswer>(&answer.body) {
            Ok(body) => {
                let extras = serde_json::from_slice::<ErrorExtras>(&answer.body).unwrap_or_default();
                // The wait is in the answer's JSON; the `Retry-After` header is the same wait, used when the JSON lacks it.
                let retry_after_secs = extras.retry_after_secs.or(answer.retry_after_secs);
                if extras.two_factor.is_none() && retry_after_secs.is_none() {
                    Err(Error::Refused { code: body.error, message: body.message, name_taken: body.name_taken })
                } else {
                    Err(Error::RefusedWith { code: body.error, message: body.message, two_factor: extras.two_factor, retry_after_secs })
                }
            }
            Err(_) => Err(Error::Protocol(format!("HTTP {}", answer.status))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn a_refusal_takes_its_wait_from_the_retry_after_header_when_the_json_has_none() {
        let body = |text: &str| text.as_bytes().to_vec();
        let only_header = http::Answer { status: 429, body: body(r#"{"error":"two_factor_locked","message":"wait"}"#), retry_after_secs: Some(900) };
        assert_eq!(refusal(only_header).unwrap_err().retry_after_secs(), Some(900));
        // The JSON wins when both are there.
        let both = http::Answer {
            status: 429,
            body: body(r#"{"error":"two_factor_locked","message":"wait","retry_after_secs":840}"#),
            retry_after_secs: Some(900),
        };
        assert_eq!(refusal(both).unwrap_err().retry_after_secs(), Some(840));
        // Neither: a plain refusal.
        let neither = http::Answer { status: 403, body: body(r#"{"error":"two_factor_invalid","message":"no"}"#), retry_after_secs: None };
        assert!(matches!(refusal(neither).unwrap_err(), Error::Refused { .. }));
    }

    /// What the fake agent was asked: method, path and body.
    type Asked = (String, String, String);

    /// A fake agent on a loopback port with `local-api.json` in a temporary folder: it answers `GET /v1/status` (the
    /// client asks it on connecting) and then each request with the next answer of `script` (status and body), and
    /// tells what it was asked.
    fn fake_agent(script: Vec<(u16, String)>) -> (tempfile::TempDir, Client, std::sync::mpsc::Receiver<Asked>) {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (asked_tx, asked) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut script = script.into_iter();
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (method, path, body) = loop {
                    let n = stream.read(&mut chunk).unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    let mut headers = [httparse::EMPTY_HEADER; 16];
                    let mut request = httparse::Request::new(&mut headers);
                    if let httparse::Status::Complete(head) = request.parse(&buf).unwrap() {
                        let length = request
                            .headers
                            .iter()
                            .find(|h| h.name.eq_ignore_ascii_case("content-length"))
                            .map_or(0, |h| std::str::from_utf8(h.value).unwrap().parse::<usize>().unwrap());
                        if buf.len() >= head + length {
                            let body = String::from_utf8(buf[head..head + length].to_vec()).unwrap();
                            break (request.method.unwrap().to_owned(), request.path.unwrap().to_owned(), body);
                        }
                    }
                    assert!(n > 0, "the client closed the connection mid-request");
                };
                let (status, answer) = if path == paths::STATUS && method == "GET" {
                    (200, serde_json::to_string(&status(false)).unwrap())
                } else {
                    let _ = asked_tx.send((method, path, body));
                    script.next().unwrap_or((500, String::new()))
                };
                let head = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n", answer.len());
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(answer.as_bytes());
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let file = serde_json::json!({ "schema": 1, "port": port, "token": "t", "pid": std::process::id() });
        std::fs::write(dir.path().join(LOCAL_API_FILE), file.to_string()).unwrap();
        let client = Client::connect_in(dir.path()).unwrap();
        (dir, client, asked)
    }

    fn status(signed_in: bool) -> Status {
        Status {
            api: 1,
            signed_in,
            account: signed_in.then(|| "ana".into()),
            server: None,
            folder: None,
            machine: None,
            online: false,
            run_state: RunState::SignedOut,
            autostart: Some(signed_in),
            last_sync_ms: None,
            removed: false,
            spaces: Vec::new(),
            from_folder: None,
        }
    }

    fn json(value: &impl serde::Serialize) -> String {
        serde_json::to_string(value).unwrap()
    }

    /// Every call sends its request to its path, and reads its answer; a refusal comes back with its code.
    #[test]
    fn each_call_sends_its_request_and_reads_its_answer() {
        let id = uuid::Uuid::now_v7();
        let device = craft_cloud_api::DeviceId::generate();
        let info = PairingInfo { id, join: "c.example/K7M4-Q9TX".into(), link: "https://c.example/join/K7M4-Q9TX".into(), expires_ms: 9 };
        let used = PairingStatus { state: PairingState::Used, device_name: Some("lptp1".into()) };
        let list = vec![DeviceInfo {
            id: device,
            name: "lptp1".into(),
            created_ms: 1,
            last_used_ms: 2,
            revoked: false,
            current: false,
            client_kind: String::new(),
            platform: String::new(),
            app_version: String::new(),
            revoked_ms: None,
            revoke_reason: None,
        }];
        let refused = json(&ErrorAnswer { error: "password_required".into(), message: "choose a password".into(), name_taken: None });
        let script = vec![
            (400, refused),
            (200, json(&status(true))),
            (200, json(&info)),
            (200, json(&used)),
            (204, String::new()),
            (200, json(&list)),
            (200, json(&list)),
            (200, json(&status(true))),
            (200, json(&status(true))),
            (200, json(&status(true))),
            (200, json(&status(true))),
        ];
        let (_dir, agent, asked) = fake_agent(script);
        let next = || asked.recv_timeout(Duration::from_secs(5)).unwrap();

        let request = JoinRequest {
            join: "c.example/K7M4-Q9TX".into(),
            folder: None,
            device_name: Some("lptp1".into()),
            adopt: true,
            password: None,
            totp_code: None,
            recovery_code: None,
        };
        let refused = agent.join(&request);
        assert!(matches!(refused, Err(Error::Refused { ref code, .. }) if code == "password_required"), "{refused:?}");
        let (method, path, body) = next();
        assert_eq!((method.as_str(), path.as_str()), ("POST", paths::JOIN));
        assert_eq!(serde_json::from_str::<JoinRequest>(&body).unwrap(), request);
        let with = JoinRequest { password: Some("a new password".into()), ..request };
        assert!(agent.join(&with).unwrap().signed_in);
        assert_eq!(serde_json::from_str::<JoinRequest>(&next().2).unwrap().password.as_deref(), Some("a new password"));

        assert_eq!(agent.pairing().unwrap(), info);
        assert_eq!(next().1, paths::PAIRINGS);
        assert_eq!(agent.pairing_state(id).unwrap(), used);
        assert_eq!(next().1, format!("{}/{id}", paths::PAIRINGS));
        agent.cancel_pairing(id).unwrap();
        assert_eq!(next(), ("DELETE".into(), format!("{}/{id}", paths::PAIRINGS), String::new()));
        assert_eq!(agent.devices().unwrap(), list);
        assert_eq!(next(), ("GET".into(), paths::DEVICES.to_owned(), String::new()));
        assert_eq!(agent.remove_device(device).unwrap(), list);
        assert_eq!(next(), ("DELETE".into(), format!("{}/{device}", paths::DEVICES), String::new()));
        for (call, path) in
            [(Client::pause as fn(&Client) -> Result<Status, Error>, paths::PAUSE), (Client::resume, paths::RESUME), (Client::quit, paths::QUIT)]
        {
            assert!(call(&agent).unwrap().signed_in);
            assert_eq!(next(), ("POST".into(), path.to_owned(), String::new()));
        }
        assert_eq!(agent.set_autostart(true).unwrap().autostart, Some(true));
        let (method, path, body) = next();
        assert_eq!((method.as_str(), path.as_str()), ("POST", paths::AUTOSTART));
        assert_eq!(serde_json::from_str::<AutostartRequest>(&body).unwrap(), AutostartRequest { on: true });
    }

    /// The two-factor calls send what the local API wants and read what it answers; a refusal keeps its challenge or its wait.
    #[test]
    fn the_two_factor_calls_send_their_requests_and_keep_what_a_refusal_carries() {
        let recovery = RecoveryCodes { recovery_codes: vec!["ABCD-EFGH-JKMN-PQRS".into(); 10] };
        let setup = TwoFactorSetupResponse { secret: "JBSWY3DPEHPK3PXP".into(), otpauth_uri: "otpauth://totp/x".into(), expires_ms: 5 };
        let state = TwoFactorStatus { enabled: true, enabled_ms: Some(3), recovery_codes_left: 9, pending: false };
        let device = craft_cloud_api::DeviceId::generate();
        let renamed = DeviceInfo {
            id: device,
            name: "new name".into(),
            created_ms: 1,
            last_used_ms: 2,
            revoked: false,
            current: false,
            client_kind: String::new(),
            platform: String::new(),
            app_version: String::new(),
            revoked_ms: None,
            revoke_reason: None,
        };
        let required =
            json(&serde_json::json!({ "error": "two_factor_required", "message": "m", "two_factor": { "challenge": "chal", "expires_ms": 9 } }));
        let locked = json(&serde_json::json!({ "error": "two_factor_locked", "message": "wait", "retry_after_secs": 840 }));
        let plain = json(&ErrorAnswer { error: "two_factor_invalid".into(), message: "wrong".into(), name_taken: None });
        let script = vec![
            (401, required),
            (200, json(&status(true))),
            (429, locked),
            (403, plain),
            (200, json(&state)),
            (200, json(&setup)),
            (200, json(&recovery)),
            (204, String::new()),
            (200, json(&recovery)),
            (204, String::new()),
            (200, json(&RevokedOthers { revoked: 2 })),
            (200, json(&renamed)),
        ];
        let (_dir, agent, asked) = fake_agent(script);
        let next = || asked.recv_timeout(Duration::from_secs(5)).unwrap();

        let login = LoginRequest {
            server: "https://c.example".into(),
            user: "ana".into(),
            password: "pw-secret-1".into(),
            folder: None,
            device_name: None,
            adopt: false,
        };
        let refused = agent.login(&login).unwrap_err();
        assert_eq!(refused.code(), Some("two_factor_required"));
        assert_eq!(refused.challenge().map(|c| (c.challenge.as_str(), c.expires_ms)), Some(("chal", 9)));
        assert!(!format!("{refused:?}").contains("\"chal\""), "the challenge is hidden in Debug");
        let _ = next();
        let second = SecondFactorLoginRequest {
            server: "https://c.example".into(),
            user: "ana".into(),
            folder: None,
            device_name: None,
            adopt: false,
            challenge: "chal".into(),
            totp_code: Some("123456".into()),
            recovery_code: None,
        };
        assert!(agent.login_second_factor(&second).unwrap().signed_in);
        let (method, path, body) = next();
        assert_eq!((method.as_str(), path.as_str()), ("POST", paths::LOGIN_SECOND_FACTOR));
        assert_eq!(serde_json::from_str::<SecondFactorLoginRequest>(&body).unwrap(), second);

        // A lock keeps its wait; a plain refusal stays a plain `Refused` (code and message only).
        let err = agent.login_second_factor(&second).unwrap_err();
        assert_eq!((err.code(), err.retry_after_secs(), err.challenge()), (Some("two_factor_locked"), Some(840), None));
        let _ = next();
        let err = agent.login_second_factor(&second).unwrap_err();
        assert!(matches!(&err, Error::Refused { code, .. } if code == "two_factor_invalid"), "{err:?}");
        let _ = next();

        assert_eq!(agent.two_factor_status().unwrap(), state);
        assert_eq!(next(), ("GET".into(), paths::TWO_FACTOR.to_owned(), String::new()));
        assert_eq!(agent.two_factor_setup("pw-secret-1").unwrap(), setup);
        let (_, path, body) = next();
        assert_eq!(
            (path.as_str(), serde_json::from_str::<PasswordRequest>(&body).unwrap().password.as_str()),
            (paths::TWO_FACTOR_SETUP, "pw-secret-1")
        );
        assert_eq!(agent.two_factor_confirm("123456").unwrap(), recovery);
        let (_, path, body) = next();
        assert_eq!((path.as_str(), serde_json::from_str::<TotpRequest>(&body).unwrap().totp_code.as_str()), (paths::TWO_FACTOR_CONFIRM, "123456"));
        let off = TwoFactorProofRequest { password: "pw-secret-1".into(), totp_code: None, recovery_code: Some("ABCD-EFGH-JKMN-PQRS".into()) };
        agent.two_factor_disable(&off).unwrap();
        let (_, path, body) = next();
        assert_eq!((path.as_str(), serde_json::from_str::<TwoFactorProofRequest>(&body).unwrap()), (paths::TWO_FACTOR_DISABLE, off));
        assert_eq!(agent.two_factor_recovery_codes("pw-secret-1", "654321").unwrap(), recovery);
        let (_, path, body) = next();
        let sent = serde_json::from_str::<TwoFactorProofRequest>(&body).unwrap();
        assert_eq!((path.as_str(), sent.totp_code.as_deref(), sent.recovery_code), (paths::TWO_FACTOR_RECOVERY_CODES, Some("654321"), None));
        agent.change_password("old-secret-pw", "new-secret-pw").unwrap();
        let (method, path, body) = next();
        assert_eq!((method.as_str(), path.as_str()), ("POST", paths::ACCOUNT_PASSWORD));
        assert_eq!(serde_json::from_str::<ChangePasswordRequest>(&body).unwrap().new_password, "new-secret-pw");
        assert_eq!(agent.revoke_other_devices().unwrap(), RevokedOthers { revoked: 2 });
        assert_eq!(next(), ("POST".into(), paths::DEVICES_REVOKE_OTHERS.to_owned(), String::new()));
        assert_eq!(agent.rename_device(device, "new name").unwrap(), renamed);
        let (method, path, body) = next();
        assert_eq!((method.as_str(), path), ("PATCH", format!("{}/{device}", paths::DEVICES)));
        assert_eq!(serde_json::from_str::<RenameDeviceRequest>(&body).unwrap().name, "new name");
    }

    /// The admin calls: each sends its request to its path with the confirmation in the body, and reads its answer; a
    /// refusal comes back with the server's code, and the wait of a `Retry-After` is kept.
    #[test]
    fn each_admin_call_sends_its_request_and_reads_its_answer() {
        let id = uuid::Uuid::now_v7();
        let device = craft_cloud_api::DeviceId::generate();
        let user = AdminUserInfo {
            id,
            username: "ana".into(),
            role: "member".into(),
            created_ms: 1,
            disabled_ms: None,
            two_factor_enabled: false,
            devices_active: 1,
            stored_bytes: 5,
            quota_bytes: None,
            invite_pending: false,
        };
        let code = AdminCode { code: "ABCDEFGHJKMNPQRS".into(), join_url: None, expires_ms: 9 };
        let list = AdminUserList { users: vec![user.clone()], next: Some("YW5h".into()) };
        let forbidden = json(&ErrorAnswer { error: "wrong_password".into(), message: "that is not your password".into(), name_taken: None });
        let script = vec![
            (200, json(&list)),
            (200, json(&list)),
            (201, json(&AdminCreatedUser { user: user.clone(), invite: code.clone() })),
            (200, json(&user)),
            (204, String::new()),
            (204, String::new()),
            (202, String::new()),
            (200, json(&code)),
            (204, String::new()),
            (200, json(&code)),
            (204, String::new()),
            (200, json(&AdminDevices { devices: Vec::new() })),
            (204, String::new()),
            (403, forbidden),
        ];
        let (_dir, agent, asked) = fake_agent(script);
        let next = || asked.recv_timeout(Duration::from_secs(5)).unwrap();
        let confirm = AdminConfirm { password: Some("admin-secret-pw".into()), ..AdminConfirm::default() };
        let password = |body: &str| serde_json::from_str::<serde_json::Value>(body).unwrap()["password"] == "admin-secret-pw";

        assert_eq!(agent.admin_users(&UserQuery::default()).unwrap(), list);
        assert_eq!(next(), ("GET".into(), "/v1/admin/users".into(), String::new()));
        let page = UserQuery { cursor: Some("YW5h".into()), limit: Some(10), query: Some("an".into()) };
        agent.admin_users(&page).unwrap();
        assert_eq!(next().1, "/v1/admin/users?cursor=YW5h&limit=10&query=an");
        let create =
            AdminCreateUser { username: "bo".into(), role: "member".into(), password: Some("admin-secret-pw".into()), ..AdminCreateUser::default() };
        assert_eq!(agent.admin_create_user(&create).unwrap().invite, code);
        let (method, path, sent) = next();
        assert_eq!((method.as_str(), path.as_str()), ("POST", "/v1/admin/users"));
        assert!(password(&sent) && sent.contains("\"bo\""), "{sent}");
        let update = AdminUpdateUser { quota_bytes: Some(None), password: Some("admin-secret-pw".into()), ..AdminUpdateUser::default() };
        assert_eq!(agent.admin_update_user(id, &update).unwrap(), user);
        let (method, path, sent) = next();
        assert_eq!((method.as_str(), path.as_str()), ("PATCH", format!("/v1/admin/users/{id}").as_str()));
        assert!(password(&sent) && sent.contains("\"quota_bytes\":null"), "{sent}");
        agent.admin_disable_user(id, &confirm).unwrap();
        assert_eq!(next().1, format!("/v1/admin/users/{id}/disable"));
        agent.admin_enable_user(id, &confirm).unwrap();
        assert_eq!(next().1, format!("/v1/admin/users/{id}/enable"));
        agent.admin_remove_user(id, &confirm).unwrap();
        let (method, path, sent) = next();
        assert_eq!((method.as_str(), path.as_str()), ("DELETE", format!("/v1/admin/users/{id}").as_str()));
        assert!(password(&sent));
        assert_eq!(agent.admin_issue_invite(id, &confirm).unwrap(), code);
        assert_eq!(next().1, format!("/v1/admin/users/{id}/invite"));
        agent.admin_revoke_invite(id, &confirm).unwrap();
        assert_eq!(next(), ("DELETE".into(), format!("/v1/admin/users/{id}/invite"), json(&confirm)));
        let reset = AdminPasswordReset {
            password: Some("admin-secret-pw".into()),
            totp_code: Some("123456".into()),
            reset_two_factor: true,
            ..AdminPasswordReset::default()
        };
        assert_eq!(agent.admin_issue_reset(id, &reset).unwrap(), code);
        let (_, path, sent) = next();
        assert_eq!(path, format!("/v1/admin/users/{id}/password-reset"));
        assert_eq!(serde_json::from_str::<AdminPasswordReset>(&sent).unwrap(), reset, "the factor and the reset flag go with it");
        agent.admin_reset_two_factor(id, &confirm).unwrap();
        assert_eq!(next().1, format!("/v1/admin/users/{id}/reset-two-factor"));
        assert!(agent.admin_devices(id).unwrap().devices.is_empty());
        assert_eq!(next(), ("GET".into(), format!("/v1/admin/users/{id}/devices"), String::new()));
        agent.admin_revoke_device(id, device, &confirm).unwrap();
        assert_eq!(next().1, format!("/v1/admin/users/{id}/devices/{device}"));
        let wrong = agent.admin_disable_user(id, &confirm);
        assert!(matches!(wrong, Err(Error::Refused { ref code, .. }) if code == "wrong_password"), "{wrong:?}");
    }

    #[test]
    fn storage_admin_calls_use_the_local_admin_routes() {
        let id = uuid::Uuid::now_v7();
        let compression = AdminCompression {
            algorithm: "zstd".into(),
            level: 19,
            config_level: 3,
            admin_level: Some(19),
            min_saving_percent: 5,
            blobs: 10,
            logical_bytes: 1000,
            known_blobs: 8,
            known_logical_bytes: 800,
            known_physical_bytes: 200,
            known_permille: 800,
            saving_permille: Some(750),
            estimate: Some(AdminCompressionEstimate {
                at_ms: 5,
                sampled_blobs: 4,
                sampled_bytes: 4000,
                current_saving_permille: 700,
                levels: vec![AdminCompressionLevelEstimate { level: 19, saving_permille: 800, kib_per_second: 5000 }],
            }),
        };
        let overview = AdminStorageOverview {
            accounts: 1,
            stored_bytes: 17,
            default_quota_bytes: Some(99),
            accounts_with_override: 0,
            accounts_near_quota: 0,
            accounts_over_quota: 0,
            top_accounts: vec![AdminStorageAccount { id, username: "ana".into(), stored_bytes: 17, quota_bytes: Some(99) }],
            manifest_index: AdminManifestIndex { complete: 3, building: 1, not_indexed: 2 },
            store_kind: Some("disk".into()),
            capacity_bytes: Some(1 << 40),
            store_fs_total_bytes: Some(2 << 40),
            store_fs_free_bytes: Some(1 << 39),
            server_fs_total_bytes: Some(1 << 38),
            server_fs_free_bytes: Some(1 << 37),
            database_bytes: Some(123_456),
            compression: Some(Box::new(compression.clone())),
        };
        let usage = AccountUsage {
            stored_bytes: 17,
            quota_bytes: Some(99),
            trash_bytes: Some(4),
            history_bytes: Some(8),
            estimates: UsageEstimates { index_complete: true, reason: None },
        };
        let (_dir, agent, asked) = fake_agent(vec![(200, json(&overview)), (200, json(&compression)), (200, json(&usage))]);
        assert_eq!(agent.admin_storage().unwrap(), overview);
        assert_eq!(asked.recv_timeout(Duration::from_secs(5)).unwrap(), ("GET".into(), "/v1/admin/storage".into(), String::new()));
        let set = AdminSetCompressionLevel { level: 19, password: Some("admin-secret-pw".into()), ..AdminSetCompressionLevel::default() };
        assert_eq!(agent.admin_set_compression_level(&set).unwrap(), compression);
        let (method, path, sent) = asked.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!((method.as_str(), path.as_str()), ("PUT", "/v1/admin/storage/compression"));
        assert_eq!(serde_json::from_str::<AdminSetCompressionLevel>(&sent).unwrap(), set, "the level and the password go with it");
        assert!(!format!("{set:?}").contains("admin-secret-pw"), "the password stays out of Debug");
        assert_eq!(agent.admin_user_usage(id).unwrap(), usage);
        assert_eq!(asked.recv_timeout(Duration::from_secs(5)).unwrap(), ("GET".into(), format!("/v1/admin/users/{id}/usage"), String::new()));
    }

    #[test]
    fn history_and_restore_use_relative_paths_and_typed_bodies() {
        let restored = RestoreResponse {
            version: VersionId::generate(),
            parent_version: VersionId::generate(),
            seq: 3,
            epoch: craft_cloud_api::Epoch::generate(),
        };
        let (_dir, agent, asked) = fake_agent(vec![
            (200, json(&HistoryResponse { versions: vec![], next: Some("7.0192f0c4-0000-7000-8000-000000000000".into()) })),
            (200, json(&restored)),
        ]);
        let history = agent.file_history("Art Folder/paint #1.craft", None, Some(25)).unwrap();
        assert_eq!(history.next.as_deref(), Some("7.0192f0c4-0000-7000-8000-000000000000"));
        let (method, path, body) = asked.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(method, "GET");
        assert_eq!(path, "/v1/files/history?path=Art%20Folder/paint%20%231.craft&limit=25");
        assert!(body.is_empty());

        let expected = VersionId::generate();
        assert_eq!(agent.restore_file("Art Folder/paint #1.craft", restored.version, expected).unwrap(), restored);
        let (method, path, body) = asked.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!((method.as_str(), path.as_str()), ("POST", paths::FILE_RESTORE));
        assert_eq!(
            serde_json::from_str::<craft_cloud_api::local::RestoreRequest>(&body).unwrap(),
            craft_cloud_api::local::RestoreRequest {
                path: "Art Folder/paint #1.craft".into(),
                version: restored.version,
                expected_version: expected,
            }
        );
    }

    #[test]
    fn conflicts_and_folder_usage_use_their_local_routes() {
        let copies = ConflictsResponse {
            copies: vec![ConflictCopy {
                path: "Art/a (conflict, pc, 2026-10-11 14-03).txt".into(),
                original_path: Some("Art/a.txt".into()),
                device: Some("pc".into()),
                at: Some("2026-10-11 14:03".into()),
                at_utc: false,
                at_ms: None,
                size: 7,
                mtime_ms: 99,
            }],
            truncated: true,
        };
        let usage = FolderUsageResponse {
            total_bytes: 12,
            total_files: 2,
            folders: vec![FolderUsage { name: "Art".into(), bytes: 12, files: 2 }],
            other: Some(OtherFolders { folders: 3, bytes: 4, files: 5 }),
            kinds: FileKind::ALL.iter().map(|kind| KindUsage { kind: *kind, bytes: 0, files: 0 }).collect(),
        };
        let (_dir, agent, asked) = fake_agent(vec![(200, json(&copies)), (200, json(&usage))]);
        assert_eq!(agent.conflicts().unwrap(), copies);
        let (method, path, body) = asked.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!((method.as_str(), path.as_str()), ("GET", paths::CONFLICTS));
        assert!(body.is_empty());
        assert_eq!(agent.folder_usage().unwrap(), usage);
        let (method, path, _) = asked.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!((method.as_str(), path.as_str()), ("GET", paths::USAGE_FOLDERS));
        // The debug output carries counts and sizes, never a file or folder name.
        let shown = format!("{copies:?} {usage:?} {:?}", usage.folders);
        assert!(!shown.contains("Art") && !shown.contains("conflict"), "{shown}");
    }

    #[test]
    fn conflicts_maps_signed_out() {
        let answer = json(&ErrorAnswer { error: "not_signed_in".into(), message: "sign in".into(), name_taken: None });
        let (_dir, agent, _asked) = fake_agent(vec![(409, answer)]);
        let result = agent.conflicts();
        assert!(matches!(result, Err(Error::Refused { ref code, .. }) if code == "not_signed_in"), "{result:?}");
    }

    #[test]
    fn trash_calls_use_paging_and_typed_node_ids() {
        let id = NodeId::generate();
        let restored = TrashRestoreResponse {
            node: id,
            parent: NodeId::generate(),
            name: "restored.txt".into(),
            renamed: true,
            version: Some(VersionId::generate()),
            seq: 4,
            epoch: craft_cloud_api::Epoch::generate(),
        };
        let item = craft_cloud_api::local::TrashItem {
            id,
            parent: restored.parent,
            name: "gone.txt".into(),
            is_dir: false,
            size: Some(3),
            deleted_ms: 10,
            purge_after_ms: 20,
            path: Some("folder/gone.txt".into()),
        };
        let (_dir, agent, asked) = fake_agent(vec![
            (200, json(&TrashResponse { items: vec![item], next: Some("cursor value".into()) })),
            (200, json(&restored)),
            (204, String::new()),
        ]);
        assert_eq!(agent.trash(None, Some(25)).unwrap().next.as_deref(), Some("cursor value"));
        let (method, path, body) = asked.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!((method.as_str(), path.as_str()), ("GET", "/v1/trash?limit=25"));
        assert!(body.is_empty());

        assert_eq!(agent.restore_from_trash(id).unwrap(), restored);
        let (method, path, body) = asked.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!((method.as_str(), path.as_str()), ("POST", paths::TRASH_RESTORE));
        assert_eq!(serde_json::from_str::<TrashNodeRequest>(&body).unwrap(), TrashNodeRequest { id: id.to_string() });

        agent.remove_from_trash(id).unwrap();
        let (method, path, body) = asked.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!((method.as_str(), path.as_str()), ("POST", paths::TRASH_REMOVE));
        assert_eq!(serde_json::from_str::<TrashNodeRequest>(&body).unwrap(), TrashNodeRequest { id: id.to_string() });
    }

    #[test]
    fn trash_cursor_with_reserved_characters_is_percent_encoded() {
        let (_dir, agent, asked) = fake_agent(vec![(200, json(&TrashResponse { items: vec![], next: None }))]);
        agent.trash(Some("a+b/c=d&e f%g"), Some(25)).unwrap();
        let (method, path, _body) = asked.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(method, "GET");
        assert_eq!(path, "/v1/trash?cursor=a%2Bb/c%3Dd%26e%20f%25g&limit=25");
        // The agent reads the query as URL-encoded pairs: decoding the value must give the cursor back.
        let value = path.split_once("cursor=").and_then(|(_, rest)| rest.split_once('&')).map(|(value, _)| value).unwrap();
        let mut decoded = Vec::new();
        let bytes = value.as_bytes();
        let mut at = 0;
        while at < bytes.len() {
            if bytes[at] == b'%' {
                decoded.push(u8::from_str_radix(&value[at + 1..at + 3], 16).unwrap());
                at += 3;
            } else {
                decoded.push(bytes[at]);
                at += 1;
            }
        }
        assert_eq!(String::from_utf8(decoded).unwrap(), "a+b/c=d&e f%g");
    }

    #[test]
    fn trash_restore_keeps_the_server_name_taken_detail() {
        let id = NodeId::generate();
        let refused = json(&ErrorAnswer { error: "conflict".into(), message: "no free conflict name".into(), name_taken: Some(true) });
        let (_dir, agent, _asked) = fake_agent(vec![(409, refused)]);
        let result = agent.restore_from_trash(id);
        assert!(matches!(result, Err(Error::Refused { ref code, name_taken: Some(true), .. }) if code == "conflict"), "{result:?}");
    }

    #[test]
    fn usage_maps_unauthorized_and_uses_the_local_route() {
        let answer = json(&ErrorAnswer { error: "unauthorized".into(), message: "not signed in".into(), name_taken: None });
        let (_dir, agent, asked) = fake_agent(vec![(401, answer)]);
        let result = agent.usage();
        assert!(matches!(result, Err(Error::Refused { ref code, .. }) if code == "unauthorized"), "{result:?}");
        let (method, path, _) = asked.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!((method.as_str(), path.as_str()), ("GET", paths::USAGE));
    }

    /// An agent that takes the connection but never answers (hung, or something else on its port) costs a
    /// UI thread at most a couple of seconds; it used to wait 90 s, the sign-in's time.
    #[test]
    fn a_silent_agent_is_given_up_on_quickly() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let held = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            std::thread::sleep(std::time::Duration::from_secs(6));
            drop(stream);
        });
        let dir = tempfile::tempdir().unwrap();
        let file = serde_json::json!({ "schema": 1, "port": port, "token": "t", "pid": std::process::id() });
        std::fs::write(dir.path().join(LOCAL_API_FILE), file.to_string()).unwrap();
        let started = std::time::Instant::now();
        let result = Client::connect_in(dir.path());
        assert!(matches!(result, Err(Error::NotRunning(_))), "{result:?}");
        assert!(started.elapsed() < std::time::Duration::from_secs(4), "took {:?}", started.elapsed());
        held.join().unwrap();
    }

    /// A pid this process can't see (an app in a sandbox with its own PID namespace, as Flatpak's) is
    /// no sign the agent stopped; the connect attempt decides.
    #[test]
    fn an_invisible_pid_leaves_it_to_the_connection() {
        let dir = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let file = serde_json::json!({ "schema": 1, "port": port, "token": "t", "pid": u32::MAX - 7 });
        std::fs::write(dir.path().join(LOCAL_API_FILE), file.to_string()).unwrap();
        let Err(Error::NotRunning(why)) = Client::connect_in(dir.path()) else { panic!("connected to nothing") };
        assert!(!why.contains("its agent stopped"), "told by the pid: {why}");
    }
    /// The space calls send their requests to their paths and read the agent's answers; an older agent's "no such call" is
    /// `agent_too_old`, while the server's own `not_found` (no such invitation) passes through.
    #[test]
    fn space_calls_send_their_requests_and_an_older_agent_is_too_old() {
        let space = SpaceId::generate();
        let info = SpaceInfo {
            space,
            kind: "team".into(),
            name: Some("Studio".into()),
            role: SpaceRole::Editor,
            state: "active".into(),
            archived: false,
            folder: Some("/home/ana/Craft Cloud Shared/Studio".into()),
            sync: SpaceSync::Off,
            run_state: None,
            last_sync_ms: None,
            read_only: false,
        };
        let item = SpaceListItem {
            id: space,
            kind: "team".into(),
            name: Some("Studio".into()),
            role: SpaceRole::Editor,
            state: "active".into(),
            archived: false,
            created_ms: 1,
            members: 2,
            root: Some(NodeId::generate()),
            epoch: None,
            joined_ms: None,
        };
        let no_such_call = json(&ErrorAnswer { error: "not_found".into(), message: "no such call".into(), name_taken: None });
        let no_invitation = json(&ErrorAnswer { error: "not_found".into(), message: "server: not found".into(), name_taken: None });
        let script = vec![
            (200, json(&SpaceList { spaces: vec![info.clone()] })),
            (200, json(&item)),
            (204, String::new()),
            (200, json(&SpaceInfo { sync: SpaceSync::On, ..info.clone() })),
            (
                200,
                json(&ApproveDeletesAnswer {
                    approved: true,
                    message: "the running agent will carry out these deletions".into(),
                    ..Default::default()
                }),
            ),
            (404, no_invitation),
            (404, no_such_call),
        ];
        let (_dir, agent, asked) = fake_agent(script);
        let next = || asked.recv_timeout(Duration::from_secs(5)).unwrap();

        assert_eq!(agent.spaces().unwrap().spaces, vec![info.clone()]);
        assert_eq!(next(), ("GET".to_owned(), "/v1/spaces".to_owned(), String::new()));
        assert_eq!(agent.accept_space(space).unwrap(), item);
        assert_eq!(next(), ("POST".to_owned(), format!("/v1/spaces/{space}/accept"), String::new()));
        agent.decline_space(space).unwrap();
        assert_eq!(next(), ("POST".to_owned(), format!("/v1/spaces/{space}/decline"), String::new()));
        let on = agent.set_space_sync(space, &SpaceSyncRequest { on: true, folder: None, adopt: true }).unwrap();
        assert_eq!(on.sync, SpaceSync::On);
        assert_eq!(next(), ("POST".to_owned(), format!("/v1/spaces/{space}/sync"), r#"{"on":true,"adopt":true}"#.to_owned()));
        let approved = agent.approve_space_deletes(space, Some("3f9a0b1c2d3e")).unwrap();
        assert!(approved.approved);
        assert_eq!(next(), ("POST".to_owned(), format!("/v1/spaces/{space}/approve-deletes"), r#"{"set":"3f9a0b1c2d3e"}"#.to_owned()));
        let gone = agent.decline_space(space).unwrap_err();
        assert!(matches!(&gone, Error::Refused { code, .. } if code == "not_found"), "the server's answer passes through: {gone:?}");
        next();
        let old = agent.spaces().unwrap_err();
        assert!(matches!(&old, Error::Refused { code, .. } if code == codes::AGENT_TOO_OLD), "{old:?}");
    }

    /// The member, usage, rename, leave and administrator calls of team spaces send their requests to their paths, with
    /// the bodies the server takes, and read what the agent answers; an older agent's "no such call" is `agent_too_old`.
    #[test]
    fn team_member_and_admin_calls_send_their_requests_and_an_older_agent_is_too_old() {
        use craft_cloud_api::api::{
            AddSpaceMember, AdminCreateSpace, AdminSetSpaceOwner, AdminSpaceInfo, AdminSpaceList, ChangeSpaceMember, SpaceConfirm, SpaceMember,
            SpaceMembers, SpaceUsage, UpdateSpace,
        };
        let space = SpaceId::generate();
        let user = Uuid::from_u128(0xBEEF);
        let member = SpaceMember { user, username: "ben".into(), role: SpaceRole::Editor, state: "invited".into(), colour: 3 };
        let info = AdminSpaceInfo {
            id: space,
            name: "Studio".into(),
            created_ms: 1,
            archived_ms: None,
            members: 2,
            owners: 1,
            stored_bytes: 10,
            quota_bytes: Some(8 << 20),
        };
        let usage = SpaceUsage {
            stored_bytes: 10,
            quota_bytes: Some(8 << 20),
            trash_bytes: None,
            history_bytes: None,
            live_bytes: 0,
            estimates: UsageEstimates { index_complete: true, reason: None },
        };
        let no_such_call = json(&ErrorAnswer { error: "not_found".into(), message: "no such call".into(), name_taken: None });
        let script = vec![
            (200, json(&SpaceMembers { members: vec![member.clone()] })),
            (200, json(&member)),
            (200, json(&SpaceMember { role: SpaceRole::Viewer, ..member.clone() })),
            (204, String::new()),
            (204, String::new()),
            (200, json(&usage)),
            (200, json(&info)),
            (200, json(&AdminSpaceList { spaces: vec![info.clone()], next: Some("c2".into()) })),
            (200, json(&info)),
            (204, String::new()),
            (204, String::new()),
            (204, String::new()),
            (404, no_such_call.clone()),
            (404, no_such_call),
        ];
        let (_dir, agent, asked) = fake_agent(script);
        let next = || asked.recv_timeout(Duration::from_secs(5)).unwrap();
        let body = |text: String| -> serde_json::Value { serde_json::from_str(&text).unwrap_or(serde_json::Value::Null) };

        assert_eq!(agent.space_members(space).unwrap().members, vec![member.clone()]);
        assert_eq!(next(), ("GET".to_owned(), format!("/v1/spaces/{space}/members"), String::new()));
        let invite = AddSpaceMember { username: "ben".into(), role: SpaceRole::Editor, confirm: SpaceConfirm::default() };
        assert_eq!(agent.add_space_member(space, &invite).unwrap(), member);
        let (method, path, sent) = next();
        assert_eq!((method.as_str(), path.as_str()), ("POST", format!("/v1/spaces/{space}/members").as_str()));
        assert_eq!(body(sent), serde_json::json!({ "username": "ben", "role": "editor" }));
        let role = ChangeSpaceMember { role: SpaceRole::Viewer, confirm: SpaceConfirm::default() };
        assert_eq!(agent.change_space_member(space, user, &role).unwrap().role, SpaceRole::Viewer);
        let (method, path, sent) = next();
        assert_eq!((method.as_str(), path.as_str()), ("PATCH", format!("/v1/spaces/{space}/members/{user}").as_str()));
        assert_eq!(body(sent), serde_json::json!({ "role": "viewer" }));
        agent.remove_space_member(space, user).unwrap();
        assert_eq!(next(), ("DELETE".to_owned(), format!("/v1/spaces/{space}/members/{user}"), String::new()));
        agent.leave_space(space).unwrap();
        assert_eq!(next(), ("POST".to_owned(), format!("/v1/spaces/{space}/leave"), String::new()));
        assert_eq!(agent.space_usage(space).unwrap(), usage);
        assert_eq!(next(), ("GET".to_owned(), format!("/v1/spaces/{space}/usage"), String::new()));
        let rename = UpdateSpace { name: Some("Studio".into()), ..UpdateSpace::default() };
        assert_eq!(agent.update_space(space, &rename).unwrap().name, "Studio");
        let (method, path, sent) = next();
        assert_eq!((method.as_str(), path.as_str()), ("PATCH", format!("/v1/spaces/{space}").as_str()));
        assert_eq!(body(sent), serde_json::json!({ "name": "Studio" }));

        let query = UserQuery { cursor: Some("c1".into()), limit: Some(20), query: Some("stu dio".into()) };
        let page = agent.admin_spaces(&query).unwrap();
        assert_eq!((page.spaces.len(), page.next.as_deref()), (1, Some("c2")));
        assert_eq!(next(), ("GET".to_owned(), "/v1/admin/spaces?cursor=c1&limit=20&query=stu%20dio".to_owned(), String::new()));
        let confirm = SpaceConfirm { password: Some("pw".into()), totp_code: None, recovery_code: None };
        let create = AdminCreateSpace { name: "Studio".into(), owner: user, quota_bytes: Some(8 << 20), confirm: confirm.clone() };
        assert_eq!(agent.admin_create_space(&create).unwrap().name, "Studio");
        let (method, path, sent) = next();
        assert_eq!((method.as_str(), path.as_str()), ("POST", "/v1/admin/spaces"));
        assert_eq!(body(sent), serde_json::json!({ "name": "Studio", "owner": user.to_string(), "quota_bytes": 8 << 20, "password": "pw" }));
        agent.admin_archive_space(space, &confirm).unwrap();
        let (method, path, sent) = next();
        assert_eq!((method.as_str(), path.as_str()), ("POST", format!("/v1/admin/spaces/{space}/archive").as_str()));
        assert_eq!(body(sent), serde_json::json!({ "password": "pw" }));
        agent.admin_unarchive_space(space, &confirm).unwrap();
        assert_eq!(next().1, format!("/v1/admin/spaces/{space}/unarchive"));
        agent.admin_set_space_owner(space, &AdminSetSpaceOwner { user, confirm }).unwrap();
        let (method, path, sent) = next();
        assert_eq!((method.as_str(), path.as_str()), ("PUT", format!("/v1/admin/spaces/{space}/owner").as_str()));
        assert_eq!(body(sent), serde_json::json!({ "user": user.to_string(), "password": "pw" }));

        // An agent from before these calls: "no such call" is `agent_too_old`, for every one of them.
        for old in [agent.space_members(space).unwrap_err(), agent.admin_spaces(&UserQuery::default()).unwrap_err()] {
            assert!(matches!(&old, Error::Refused { code, .. } if code == codes::AGENT_TOO_OLD), "{old:?}");
        }
    }

    #[test]
    fn making_a_team_space_from_a_folder_is_one_call_and_the_confirmation_goes_in_its_body() {
        use craft_cloud_api::api::SpaceConfirm;
        use craft_cloud_api::local::{FromFolderRequest, FromFolderStatus, FromFolderStep};
        let status = FromFolderStatus {
            folder: "/home/ana/Craft Cloud/Photos".into(),
            name: "Photos team".into(),
            step: FromFolderStep::SettingUp,
            space: Some(SpaceId::generate()),
            team_folder: None,
            done: 0,
            total: 0,
            failed_at: None,
            message: None,
            kept: 0,
        };
        let no_such_call = json(&ErrorAnswer { error: "not_found".into(), message: "no such call".into(), name_taken: None });
        let (_dir, agent, asked) = fake_agent(vec![(200, json(&status)), (404, no_such_call)]);
        let request = FromFolderRequest {
            folder: "/home/ana/Craft Cloud/Photos".into(),
            name: "Photos team".into(),
            quota_bytes: None,
            confirm: SpaceConfirm { password: Some("pw".into()), totp_code: None, recovery_code: None },
        };
        assert_eq!(agent.make_space_from_folder(&request).unwrap(), status);
        let (method, path, sent) = asked.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!((method.as_str(), path.as_str()), ("POST", "/v1/spaces/from-folder"));
        let sent: serde_json::Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(sent, serde_json::json!({ "folder": "/home/ana/Craft Cloud/Photos", "name": "Photos team", "password": "pw" }));
        assert!(!format!("{request:?}").contains("pw"), "the password stays out of Debug");
        // An agent from before the call: "no such call" is `agent_too_old`.
        let old = agent.make_space_from_folder(&request).unwrap_err();
        assert!(matches!(&old, Error::Refused { code, .. } if code == codes::AGENT_TOO_OLD), "{old:?}");
    }
}
