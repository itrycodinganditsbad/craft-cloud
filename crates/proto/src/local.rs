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
//!   `POST /v1/autostart` ([`AutostartRequest`]) → [`Status`]; `GET /v1/recents`, upgraded to a WebSocket of JSON
//!   text messages: [`RecentsRequest`] in, [`RecentsEvent`] out. Errors are the HTTP API's
//!   [`ErrorBody`](crate::api::ErrorBody) with its codes and [`codes`].
//! - **Versions**: [`Status::api`] is the local API version the agent speaks; a client refuses one outside
//!   [`LOCAL_API_MIN`]..=[`LOCAL_API_MAX`] when it connects.

use std::ffi::OsString;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ids::NodeId;
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

/// Error codes of the local API (the `error` field of [`ErrorBody`](crate::api::ErrorBody)), beside the HTTP API's
/// `invalid`, `unauthorized` (the token), `not_found`, `too_large` and `internal`.
pub mod codes {
    /// A `Host` that isn't `127.0.0.1:<port>`, or an `Origin` header: the request didn't come from a local program.
    pub const FORBIDDEN: &str = "forbidden";
    /// `login`: the server refused the username or password.
    pub const WRONG_PASSWORD: &str = "wrong_password";
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
}

/// The paths of the local API.
pub mod paths {
    pub const STATUS: &str = "/v1/status";
    pub const LOGIN: &str = "/v1/login";
    pub const LOGOUT: &str = "/v1/logout";
    pub const RECENTS: &str = "/v1/recents";
    /// `POST`: join with a join string ([`super::JoinRequest`]) → [`super::Status`].
    pub const JOIN: &str = "/v1/join";
    /// `POST`: a new add-a-computer code → [`super::PairingInfo`]; `GET`/`DELETE` `/v1/pairings/{id}`: its state, or
    /// cancel it.
    pub const PAIRINGS: &str = "/v1/pairings";
    /// `GET`: the account's computers; `DELETE /v1/devices/{id}`: remove one (never this one).
    pub const DEVICES: &str = "/v1/devices";
    /// `POST`: hold syncing (kept across restarts) → [`super::Status`].
    pub const PAUSE: &str = "/v1/pause";
    /// `POST`: sync again after a pause → [`super::Status`].
    pub const RESUME: &str = "/v1/resume";
    /// `POST`: finish the pass under way and exit → [`super::Status`] (as the agent stops).
    pub const QUIT: &str = "/v1/quit";
    /// `POST` ([`super::AutostartRequest`]): start, or don't start, the agent at login → [`super::Status`].
    pub const AUTOSTART: &str = "/v1/autostart";
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
    /// (see [`crate::join::JoinString`]).
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
}

impl std::fmt::Debug for JoinRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinRequest")
            .field("join", &"<redacted>")
            .field("folder", &self.folder)
            .field("device_name", &self.device_name)
            .field("adopt", &self.adopt)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
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
    /// Send `app`'s list now and after every change.
    Subscribe { app: String },
    /// Add a file to `app`'s list; answered `added` (or `error`) with the same `id`.
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

    /// A list's event says when its push stopped; without the field (an older agent) it hasn't.
    #[test]
    fn a_list_says_when_its_push_stopped() {
        let stopped = RecentsEvent::Entries { app: "wordcraft".into(), entries: Vec::new(), push_stopped: true };
        let json = serde_json::to_string(&stopped).unwrap();
        assert!(json.contains("\"push_stopped\":true"), "{json}");
        assert_eq!(serde_json::from_str::<RecentsEvent>(&json).unwrap(), stopped);
        let older: RecentsEvent = serde_json::from_str(r#"{"type":"entries","app":"wordcraft","entries":[]}"#).unwrap();
        assert_eq!(older, RecentsEvent::Entries { app: "wordcraft".into(), entries: Vec::new(), push_stopped: false });
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
        };
        let shown = format!("{join:?}");
        assert!(!shown.contains("K7M4") && !shown.contains("hunter2-secret"), "{shown}");
        let pairing = PairingInfo {
            id: uuid::Uuid::nil(),
            join: "cloud.example.com/K7M4-Q9TX".into(),
            link: "https://cloud.example.com/join/K7M4-Q9TX".into(),
            expires_ms: 1,
        };
        assert!(!format!("{pairing:?}").contains("K7M4"));
        let minimal: JoinRequest = serde_json::from_value(serde_json::json!({ "join": "x/y" })).unwrap();
        assert!(!minimal.adopt && minimal.password.is_none());
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
        let login: LoginRequest = serde_json::from_value(serde_json::json!({ "server": "s", "user": "u", "password": "p", "extra": 1 })).unwrap();
        assert!(!login.adopt && login.folder.is_none(), "missing optional fields default; unknown ones are ignored");
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
