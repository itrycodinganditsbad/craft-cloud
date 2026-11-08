//! The local API (`craft-cloud-proto::local`): what apps, the launcher and the agent's own CLI ask the running agent,
//! on `127.0.0.1` only.
//!
//! [`LocalApi::start`] binds a random loopback port, makes a random token and writes both with the pid to
//! `local-api.json` in the data folder (owner-only, atomically); dropping it stops the server and removes the file.
//! Every request passes [`guard`] first: the `Host` header must be exactly `127.0.0.1:<port>` (a web page that rebinds
//! a name to 127.0.0.1 sends its own name), any `Origin` header is refused (browsers send one with every
//! cross-origin request and every WebSocket), and the token is taken only from `Authorization: Bearer`, never from
//! the query string; it is compared in constant time and never logged or printed.
//!
//! Sign-in and sign-out, pause, resume and quit are the `run` loop's to carry out (it holds the data folder's lock): a
//! call hands them over as a [`Command`] and waits for the answer. Status is read from [`Control`], which the loop
//! keeps up to date.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::StatusCode;
use axum::http::header::{AUTHORIZATION, HOST, ORIGIN};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use craft_cloud_proto::DeviceId;
use craft_cloud_proto::api::{
    AccountUsage, DeviceInfo, HistoryResponse, PairingStatus, RecoveryCodes, RestoreResponse, RevokedOthers, TwoFactorChallenge,
    TwoFactorSetupResponse, TwoFactorStatus, codes as api_codes,
};
use craft_cloud_proto::join::{Code, JoinString, MAX_JOIN_BYTES, PAIRING_CHARS};
use craft_cloud_proto::local::{
    ApproveDeletesAnswer, ApproveDeletesRequest, AutostartRequest, ChangePasswordRequest, ConflictsResponse, FolderUsageResponse, FromFolderRequest,
    FromFolderStatus, JoinRequest, LOCAL_API_FILE, LOCAL_API_MAX, LOCAL_API_SCHEMA, LocalApiFile, LoginRequest, LogoutRequest, MAX_LOCAL_JSON_BYTES,
    PairingInfo, PasswordRequest, RenameDeviceRequest, RunState, SecondFactorLoginRequest, SpaceInfo, SpaceList, SpaceRun, SpaceStatus, SpaceSync,
    SpaceSyncRequest, Status, TotpRequest, TwoFactorProofRequest, codes, paths,
};
use tokio::sync::{mpsc, oneshot};

use craft_cloud_proto::limits::MAX_BLOB_BYTES;
use craft_cloud_proto::local::blobs::{Stored, parse_hex};

use crate::account::{Account, DataDir, random_hex, write_atomic};
use crate::autostart::{self, Roots};
use crate::cli::{AgentError, Exit};
use crate::http::HttpTransport;
use crate::live::app_blobs::{self, BlobError};
use crate::spaces::SpacesHandle;

/// What the local API asks the `run` loop to do, with where to send the outcome.
pub enum Command {
    Login(LoginRequest, oneshot::Sender<Result<(), AgentError>>),
    Join(JoinRequest, oneshot::Sender<Result<(), AgentError>>),
    /// The second step of a sign-in that answered `two_factor_required`.
    LoginSecondFactor(SecondFactorLoginRequest, oneshot::Sender<Result<(), AgentError>>),
    Logout(LogoutRequest, oneshot::Sender<Result<(), AgentError>>),
    /// Hold syncing and the live connection until [`Command::Resume`], also after a restart.
    Pause(oneshot::Sender<Result<(), AgentError>>),
    Resume(oneshot::Sender<Result<(), AgentError>>),
    /// Finish the pass under way and stop the agent. Answered at once.
    Quit(oneshot::Sender<Result<(), AgentError>>),
    /// This computer was renamed on the server: keep the new name in the saved account, the status and the live hub.
    /// Another computer's id changes nothing.
    RenameDevice(DeviceId, String, oneshot::Sender<Result<(), AgentError>>),
    /// Run a sync pass soon (an app opened a file that isn't uploaded yet, to make it live). Not answered.
    SyncSoon,
}

/// What [`Status`] reports, kept up to date by the `run` loop.
#[derive(Clone)]
struct Shared {
    account: Option<Account>,
    run_state: RunState,
    online: bool,
    /// The live hub, while signed in.
    hub: Option<crate::live::HubHandle>,
    /// Whether the agent starts at login, as last looked at (`None`: not known, or not handled here).
    autostart: Option<bool>,
    /// When the last pass that didn't fail finished, while signed in.
    last_sync_ms: Option<i64>,
    /// The last pass found this device refused by the server.
    removed: bool,
    /// The team spaces, as the spaces task last saw them (`GET /v1/spaces`, after the personal space), and the ones that sync
    /// or lost access (`GET /v1/status`).
    spaces: Vec<SpaceInfo>,
    space_status: Vec<SpaceStatus>,
    spaces_handle: Option<SpacesHandle>,
    /// The work of making a team space from a folder, under way or how the last one ended.
    from_folder: Option<FromFolderStatus>,
}

/// The running work of making a team space from a folder: its task, and the flag that tells it its session has ended.
pub struct FromFolderJob {
    pub cancel: Arc<std::sync::atomic::AtomicBool>,
    pub task: tokio::task::JoinHandle<()>,
}

/// The loop's side of the local API: the status it reports, and the commands it receives. Cheap to clone.
#[derive(Clone)]
pub struct Control {
    shared: Arc<Mutex<Shared>>,
    /// The task of the work of making a team space from a folder (not part of what the status copies).
    from_job: Arc<Mutex<Option<FromFolderJob>>>,
    commands: mpsc::Sender<Command>,
    /// Where the start-at-login entry goes (`None`: `autostart` is refused, with `autostart_refusal`).
    autostart: Option<Roots>,
    autostart_refusal: String,
}

impl Control {
    /// A control for a loop that receives its commands from `commands`' other end.
    pub fn new(commands: mpsc::Sender<Command>) -> Self {
        let shared = Shared {
            account: None,
            run_state: RunState::SignedOut,
            online: false,
            hub: None,
            autostart: None,
            last_sync_ms: None,
            removed: false,
            spaces: Vec::new(),
            space_status: Vec::new(),
            spaces_handle: None,
            from_folder: None,
        };
        let autostart_refusal = "this agent doesn't handle starting at login here".to_owned();
        Self { shared: Arc::new(Mutex::new(shared)), from_job: Arc::default(), commands, autostart: None, autostart_refusal }
    }

    /// The control with the start-at-login entry at `roots`, for the agent with data folder `data`. The entry is
    /// handled only by the agent it belongs to ([`Roots::owns`]); for any other, or with no roots, `autostart` is
    /// refused and the status says nothing about it.
    pub fn with_autostart(mut self, roots: Option<Roots>, data: &std::path::Path) -> Self {
        match roots {
            Some(roots) if roots.owns(data) => self.autostart = Some(roots),
            Some(_) => self.autostart_refusal = autostart::not_ours(data),
            None => {}
        }
        self
    }

    /// Where the start-at-login entry goes, if it is handled here.
    pub fn autostart_roots(&self) -> Option<Roots> {
        self.autostart.clone()
    }

    /// Looks again whether the agent starts at login (off the async threads: on Linux it asks `systemctl`), for
    /// [`Status::autostart`].
    pub async fn refresh_autostart(&self) {
        let registered = match self.autostart.clone() {
            Some(roots) => tokio::task::spawn_blocking(move || autostart::is_registered(&roots).ok()).await.ok().flatten(),
            None => None,
        };
        self.with(|s| s.autostart = registered);
    }

    fn with<T>(&self, f: impl FnOnce(&mut Shared) -> T) -> T {
        f(&mut self.shared.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Signed in as `account` (or signed out: `None`).
    pub fn set_account(&self, account: Option<&Account>) {
        self.with(|s| {
            s.account = account.cloned();
            if account.is_none() {
                s.run_state = RunState::SignedOut;
                s.online = false;
                s.last_sync_ms = None;
                s.removed = false;
                s.spaces.clear();
                s.space_status.clear();
                s.from_folder = None;
            }
        });
    }

    /// What the spaces task sees: the team spaces (for `GET /v1/spaces`) and the status of those that sync.
    pub fn set_spaces(&self, spaces: Vec<SpaceInfo>, status: Vec<SpaceStatus>) {
        self.with(|s| {
            s.spaces = spaces;
            s.space_status = status;
        });
    }

    /// The spaces task's handle, while signed in.
    pub fn set_spaces_handle(&self, handle: Option<SpacesHandle>) {
        self.with(|s| s.spaces_handle = handle);
    }

    pub fn spaces_handle(&self) -> Option<SpacesHandle> {
        self.with(|s| s.spaces_handle.clone())
    }

    /// The work of making a team space from a folder (`Status::from_folder`).
    pub fn from_folder(&self) -> Option<FromFolderStatus> {
        self.with(|s| s.from_folder.clone())
    }

    pub fn set_from_folder(&self, status: Option<FromFolderStatus>) {
        self.with(|s| s.from_folder = status);
    }

    pub fn set_from_folder_job(&self, job: Option<FromFolderJob>) {
        *self.from_job.lock().unwrap_or_else(PoisonError::into_inner) = job;
    }

    pub fn take_from_folder_job(&self) -> Option<FromFolderJob> {
        self.from_job.lock().unwrap_or_else(PoisonError::into_inner).take()
    }

    /// Starts the work's status, unless other work is still under way (`false`).
    pub fn begin_from_folder(&self, status: FromFolderStatus) -> bool {
        self.with(|s| {
            if s.from_folder.as_ref().is_some_and(FromFolderStatus::running) {
                return false;
            }
            s.from_folder = Some(status);
            true
        })
    }

    /// A team space as the spaces task last listed it.
    pub fn space_info(&self, space: craft_cloud_proto::SpaceId) -> Option<SpaceInfo> {
        self.with(|s| s.spaces.iter().find(|i| i.space == space).cloned())
    }

    /// Whether the server refused this device at the last pass (it was removed from the account).
    pub fn set_removed(&self, removed: bool) {
        self.with(|s| s.removed = removed);
    }

    pub fn set_run_state(&self, state: RunState) {
        self.with(|s| s.run_state = state);
    }

    /// When the last pass that didn't fail finished.
    pub fn set_last_sync(&self, at_ms: Option<i64>) {
        self.with(|s| s.last_sync_ms = at_ms);
    }

    /// Whether the agent reaches the server right now (the live hub's connection is up).
    pub fn set_online(&self, online: bool) {
        self.with(|s| s.online = online);
    }

    /// Asks the `run` loop for a sync pass soon (never waits: a loop that is busy takes it when it can).
    pub fn request_pass(&self) {
        let _ = self.commands.try_send(Command::SyncSoon);
    }

    /// The live hub that serves recents (while signed in).
    pub fn set_hub(&self, hub: Option<crate::live::HubHandle>) {
        self.with(|s| s.hub = hub);
    }

    pub fn hub(&self) -> Option<crate::live::HubHandle> {
        self.with(|s| s.hub.clone())
    }

    /// The account signed in, if any.
    pub fn account(&self) -> Option<Account> {
        self.with(|s| s.account.clone())
    }

    /// What `GET /v1/status` answers.
    pub fn status(&self) -> Status {
        self.with(|s| Status {
            api: LOCAL_API_MAX,
            signed_in: s.account.is_some(),
            account: s.account.as_ref().map(|a| a.username.clone()),
            server: s.account.as_ref().map(|a| a.server.to_string()),
            folder: s.account.as_ref().map(|a| a.folder.display().to_string()),
            machine: s.account.as_ref().map(|a| a.device_name.clone()),
            online: s.online,
            run_state: s.run_state.clone(),
            autostart: s.autostart,
            last_sync_ms: s.last_sync_ms,
            removed: s.account.is_some() && s.removed,
            spaces: if s.account.is_some() { s.space_status.clone() } else { Vec::new() },
            from_folder: if s.account.is_some() { s.from_folder.clone() } else { None },
        })
    }
}

/// An error answer: the HTTP status, the typed local API code and message, and optional details such as a two-factor
/// challenge, a lock wait, or a trash-restore name conflict.
#[derive(Debug)]
pub struct LocalError {
    pub status: StatusCode,
    pub code: String,
    pub message: String,
    pub two_factor: Option<TwoFactorChallenge>,
    pub retry_after_secs: Option<u64>,
    pub name_taken: Option<bool>,
}

impl LocalError {
    pub fn new(status: StatusCode, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { status, code: code.into(), message: message.into(), two_factor: None, retry_after_secs: None, name_taken: None }
    }

    fn with_code(status: StatusCode, code: String, message: impl Into<String>) -> Self {
        Self { status, code, message: message.into(), two_factor: None, retry_after_secs: None, name_taken: None }
    }
}

impl IntoResponse for LocalError {
    fn into_response(self) -> Response {
        let extras = craft_cloud_proto::local::ErrorExtras { two_factor: self.two_factor, retry_after_secs: self.retry_after_secs };
        let mut body = serde_json::json!({ "error": self.code, "message": self.message });
        if let (Some(object), Ok(serde_json::Value::Object(more))) = (body.as_object_mut(), serde_json::to_value(&extras)) {
            object.extend(more);
        }
        if let Some(name_taken) = self.name_taken
            && let Some(object) = body.as_object_mut()
        {
            object.insert("name_taken".to_owned(), serde_json::Value::Bool(name_taken));
        }
        let mut response = (self.status, Json(body)).into_response();
        if let Some(secs) = self.retry_after_secs {
            response.headers_mut().insert(axum::http::header::RETRY_AFTER, axum::http::HeaderValue::from(secs));
        }
        response
    }
}

impl From<AgentError> for LocalError {
    fn from(e: AgentError) -> Self {
        let message = e.to_string();
        // A setup that failed after the code was spent answers as what failed, with the whole message.
        if let AgentError::JoinSetup(inner) = e {
            let inner = LocalError::from(*inner);
            return Self::with_code(inner.status, inner.code, message);
        }
        // The second factor: a challenge, a wrong code, a lock and a server's refusal keep what they carry. None of them is a
        // revoked device (that is `Revoked`, from a 401 on a signed-in call).
        match &e {
            AgentError::TwoFactorRequired(challenge) => {
                let mut error = Self::new(StatusCode::UNAUTHORIZED, codes::TWO_FACTOR_REQUIRED, message);
                // `join` has no challenge to hand on: the same join goes again with the code.
                error.two_factor = Some(challenge.clone());
                return error;
            }
            AgentError::TwoFactorInvalid(_) => return Self::new(StatusCode::UNAUTHORIZED, codes::TWO_FACTOR_INVALID, message),
            AgentError::ChallengeGone => return Self::new(StatusCode::CONFLICT, codes::TWO_FACTOR_EXPIRED, message),
            AgentError::TwoFactorLocked { retry_after_secs, .. } => {
                let mut error = Self::new(StatusCode::TOO_MANY_REQUESTS, codes::TWO_FACTOR_LOCKED, message);
                error.retry_after_secs = *retry_after_secs;
                return error;
            }
            AgentError::Refused { status, code, retry_after_secs, .. } => {
                // The server's own status and code (never a 401: that is `Revoked`).
                let status =
                    StatusCode::from_u16(*status).ok().filter(|s| (s.is_client_error() || s.is_server_error()) && *s != StatusCode::UNAUTHORIZED);
                let mut error = Self::new(status.unwrap_or(StatusCode::BAD_REQUEST), code.clone(), message);
                error.retry_after_secs = *retry_after_secs;
                return error;
            }
            _ => {}
        }
        let (status, code) = match &e {
            AgentError::WrongPassword => (StatusCode::UNAUTHORIZED, codes::WRONG_PASSWORD),
            AgentError::InvalidCode => (StatusCode::NOT_FOUND, codes::INVALID_CODE),
            AgentError::PasswordRequired => (StatusCode::BAD_REQUEST, codes::PASSWORD_REQUIRED),
            AgentError::WeakPassword(_) => (StatusCode::BAD_REQUEST, codes::WEAK_PASSWORD),
            AgentError::TooManyTries(_) => (StatusCode::TOO_MANY_REQUESTS, api_codes::RATE_LIMITED),
            AgentError::UntrustedCertificate { .. } => (StatusCode::SERVICE_UNAVAILABLE, codes::UNTRUSTED_CERTIFICATE),
            AgentError::Unreachable { .. } => (StatusCode::SERVICE_UNAVAILABLE, codes::OFFLINE),
            AgentError::CannotRemoveSelf => (StatusCode::CONFLICT, codes::CANNOT_REMOVE_SELF),
            AgentError::FolderHasFiles(_) => (StatusCode::BAD_REQUEST, codes::FOLDER_HAS_FILES),
            AgentError::Server(craft_cloud_sync::TransportError::NotFound) => (StatusCode::NOT_FOUND, api_codes::NOT_FOUND),
            AgentError::NotSignedIn => (StatusCode::CONFLICT, codes::NOT_SIGNED_IN),
            AgentError::Offline(_) | AgentError::LogoutOffline(_) => (StatusCode::SERVICE_UNAVAILABLE, codes::OFFLINE),
            AgentError::Usage(m) if m.starts_with("already signed in") => (StatusCode::CONFLICT, codes::ALREADY_SIGNED_IN),
            AgentError::Usage(m) if m == crate::spaces::CHANGING => (StatusCode::CONFLICT, codes::CONFLICT),
            AgentError::Usage(_) => (StatusCode::BAD_REQUEST, api_codes::INVALID),
            other if other.exit() == Exit::NeedsYou => (StatusCode::CONFLICT, codes::NEEDS_YOU),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, api_codes::INTERNAL),
        };
        Self::new(status, code, message)
    }
}

impl From<JsonRejection> for LocalError {
    fn from(e: JsonRejection) -> Self {
        let code = if e.status() == StatusCode::PAYLOAD_TOO_LARGE { api_codes::TOO_LARGE } else { api_codes::INVALID };
        Self::new(e.status(), code, e.body_text())
    }
}

/// What every handler can reach.
pub struct ApiState {
    port: u16,
    token: String,
    pub control: Control,
    /// The agent's data folder (a start-at-login entry names it when it isn't the default one).
    data: PathBuf,
}

impl ApiState {
    /// The agent's data folder.
    pub fn data(&self) -> &std::path::Path {
        &self.data
    }
}

/// The running local API. Dropping it stops the server and removes `local-api.json`.
pub struct LocalApi {
    port: u16,
    file: PathBuf,
    task: tokio::task::JoinHandle<()>,
    /// The task that uploads blobs put while offline.
    retry: tokio::task::JoinHandle<()>,
    shutdown: Option<oneshot::Sender<()>>,
}

impl LocalApi {
    /// Serves the local API on a random loopback port and writes `local-api.json`.
    pub async fn start(data: &DataDir, control: Control) -> Result<Self, AgentError> {
        let file = data.path().join(LOCAL_API_FILE);
        let io = |source| AgentError::Io { path: file.clone(), source };
        // Never another address: loopback, IPv4, as the `Host` check expects.
        let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await.map_err(io)?;
        let port = listener.local_addr().map_err(io)?.port();
        let token = random_hex(32).map_err(io)?;
        let retry = tokio::spawn(retry_blobs(control.clone(), data.path().to_owned()));
        let state = Arc::new(ApiState { port, token: token.clone(), control, data: data.path().to_owned() });
        let app = router(state);
        let (shutdown, shutting_down) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let stop = async move {
                let _ = shutting_down.await;
            };
            // No Nagle delay: a live document's update to an app goes out at once.
            use axum::serve::ListenerExt;
            let listener = listener.tap_io(|tcp| {
                let _ = tcp.set_nodelay(true);
            });
            if let Err(e) = axum::serve(listener, app).with_graceful_shutdown(stop).await {
                crate::runlog::note(format_args!("the local API stopped: {e}"));
            }
        });
        let record = LocalApiFile { schema: LOCAL_API_SCHEMA, port, token, pid: std::process::id() };
        let json = serde_json::to_vec_pretty(&record).map_err(|e| AgentError::Io { path: file.clone(), source: std::io::Error::other(e) })?;
        if let Err(e) = write_atomic(&file, &json, true) {
            task.abort();
            retry.abort();
            return Err(io(e));
        }
        Ok(Self { port, file, task, retry, shutdown: Some(shutdown) })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Stops the server, letting the answers under way go out first (the one to `quit` among them), for at most
    /// [`STOP_GRACE`], and removes `local-api.json`.
    pub async fn stop(mut self) {
        let _ = std::fs::remove_file(&self.file);
        self.retry.abort();
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = tokio::time::timeout(STOP_GRACE, &mut self.task).await;
    }
}

/// How long [`LocalApi::stop`] waits for the answers under way.
const STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

impl Drop for LocalApi {
    fn drop(&mut self) {
        self.task.abort();
        self.retry.abort();
        let _ = std::fs::remove_file(&self.file);
    }
}

pub(crate) type Api = Arc<ApiState>;

fn router(state: Api) -> Router {
    Router::new()
        .route(paths::STATUS, get(status))
        .route(paths::LOGIN, post(login))
        .route(paths::LOGIN_SECOND_FACTOR, post(login_second_factor).layer(DefaultBodyLimit::max(TINY_JSON_BYTES)))
        .route(paths::LOGOUT, post(logout))
        .route(paths::TWO_FACTOR, get(two_factor_status))
        .route(paths::TWO_FACTOR_SETUP, post(two_factor_setup).layer(DefaultBodyLimit::max(TINY_JSON_BYTES)))
        .route(paths::TWO_FACTOR_CONFIRM, post(two_factor_confirm).layer(DefaultBodyLimit::max(TINY_JSON_BYTES)))
        .route(paths::TWO_FACTOR_DISABLE, post(two_factor_disable).layer(DefaultBodyLimit::max(TINY_JSON_BYTES)))
        .route(paths::TWO_FACTOR_RECOVERY_CODES, post(two_factor_recovery_codes).layer(DefaultBodyLimit::max(TINY_JSON_BYTES)))
        .route(paths::ACCOUNT_PASSWORD, post(change_password).layer(DefaultBodyLimit::max(TINY_JSON_BYTES)))
        .route(paths::DEVICES_REVOKE_OTHERS, post(revoke_other_devices))
        .route(paths::RECENTS, get(crate::live::recents_socket))
        .route(paths::DOCS, get(crate::live::docs::docs_socket))
        .route(paths::JOIN, post(join))
        .route(paths::PAIRINGS, post(create_pairing))
        .route(&format!("{}/{{id}}", paths::PAIRINGS), get(pairing_status).delete(delete_pairing))
        .route(paths::DEVICES, get(devices))
        .route(paths::USAGE, get(usage))
        .route(paths::USAGE_FOLDERS, get(usage_folders))
        .route(paths::CONFLICTS, get(conflicts))
        .route(&format!("{}/{{id}}", paths::DEVICES), delete(remove_device).patch(rename_device).layer(DefaultBodyLimit::max(TINY_JSON_BYTES)))
        .route(paths::SPACES, get(spaces))
        .route(paths::SPACES_FROM_FOLDER, post(from_folder).layer(DefaultBodyLimit::max(craft_cloud_proto::local::MAX_ADMIN_JSON_BYTES)))
        .route(&format!("{}/{{id}}/accept", paths::SPACES), post(accept_space))
        .route(&format!("{}/{{id}}/decline", paths::SPACES), post(decline_space))
        .route(&format!("{}/{{id}}/sync", paths::SPACES), post(sync_space).layer(DefaultBodyLimit::max(TINY_JSON_BYTES)))
        .route(&format!("{}/{{id}}/approve-deletes", paths::SPACES), post(approve_space_deletes).layer(DefaultBodyLimit::max(TINY_JSON_BYTES)))
        .route(paths::AUTOSTART, post(set_autostart))
        .route(paths::PAUSE, post(pause))
        .route(paths::RESUME, post(resume))
        .route(paths::QUIT, post(quit))
        .merge(crate::admin::routes())
        .route(paths::FILE_HISTORY, get(file_history))
        .route(paths::FILE_RESTORE, post(file_restore))
        .route(paths::TRASH, get(trash))
        .route(paths::TRASH_RESTORE, post(trash_restore))
        .route(paths::TRASH_REMOVE, post(trash_remove))
        .route(paths::BLOBS, axum::routing::put(put_blob).layer(DefaultBodyLimit::max(MAX_BLOB_BYTES)))
        .route(&format!("{}/{{hash}}", paths::BLOBS), get(get_blob))
        .fallback(|| async { LocalError::new(StatusCode::NOT_FOUND, api_codes::NOT_FOUND, "no such call") })
        .layer(DefaultBodyLimit::max(MAX_LOCAL_JSON_BYTES))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

#[derive(serde::Deserialize)]
struct HistoryQuery {
    path: String,
    cursor: Option<String>,
    limit: Option<u32>,
}

fn file_node(state: &ApiState, path: &str) -> Result<craft_cloud_proto::NodeId, LocalError> {
    if path.is_empty() || path.starts_with('/') || path.contains('\\') || path.contains(':') || path.as_bytes().contains(&0) {
        return Err(LocalError::new(StatusCode::BAD_REQUEST, api_codes::INVALID, "path must be relative to the sync root"));
    }
    let names = path
        .split('/')
        .map(craft_cloud_proto::Name::new)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| LocalError::new(StatusCode::BAD_REQUEST, api_codes::INVALID, "path contains an invalid name"))?;
    let saved =
        DataDir::new(&state.data).load_state().map_err(|e| LocalError::new(StatusCode::INTERNAL_SERVER_ERROR, api_codes::INTERNAL, e.to_string()))?;
    let rel = craft_cloud_sync::RelPath::from_names(names);
    let node =
        saved.state.base_at(&rel).ok_or_else(|| LocalError::new(StatusCode::NOT_FOUND, api_codes::NOT_FOUND, "no synced file at that path"))?;
    if !matches!(saved.state.base.get(&node).map(|entry| &entry.kind), Some(craft_cloud_sync::BaseKind::File { .. })) {
        return Err(LocalError::new(StatusCode::BAD_REQUEST, api_codes::INVALID, "history is only available for files"));
    }
    Ok(node)
}

fn history_error(error: crate::http::HistoryCallError) -> LocalError {
    match error {
        crate::http::HistoryCallError::Refused { status: StatusCode::UNAUTHORIZED, body } => {
            LocalError::new(StatusCode::CONFLICT, codes::NEEDS_YOU, body.message)
        }
        crate::http::HistoryCallError::Refused { status, body } => {
            let name_taken = body
                .conflicts
                .as_ref()
                .map(|conflicts| conflicts.iter().any(|conflict| matches!(conflict.kind, craft_cloud_proto::ConflictKind::NameTaken { .. })));
            LocalError { status, code: body.error, message: body.message, name_taken, two_factor: None, retry_after_secs: None }
        }
        crate::http::HistoryCallError::Transport(craft_cloud_sync::TransportError::NotFound) => {
            LocalError::new(StatusCode::NOT_FOUND, api_codes::NOT_FOUND, "no such file or version")
        }
        crate::http::HistoryCallError::Transport(craft_cloud_sync::TransportError::Unauthorized) => {
            LocalError::new(StatusCode::CONFLICT, codes::NEEDS_YOU, "sign in again to continue")
        }
        crate::http::HistoryCallError::Transport(error) => AgentError::from(error).into(),
    }
}

async fn file_history(
    State(state): State<Api>,
    query: Result<Query<HistoryQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<HistoryResponse>, LocalError> {
    let Query(query) = query.map_err(|e| LocalError::new(StatusCode::BAD_REQUEST, api_codes::INVALID, e.body_text()))?;
    let node = file_node(&state, &query.path)?;
    let (_, transport) = server_api(&state)?;
    Ok(Json(transport.file_history(node, query.cursor.as_deref(), query.limit).await.map_err(history_error)?))
}

async fn file_restore(
    State(state): State<Api>,
    body: Result<Json<craft_cloud_proto::local::RestoreRequest>, JsonRejection>,
) -> Result<Json<RestoreResponse>, LocalError> {
    let Json(request) = body?;
    let node = file_node(&state, &request.path)?;
    let (_, transport) = server_api(&state)?;
    let answer = transport.restore_file(node, request.version, request.expected_version).await.map_err(history_error)?;
    state.control.request_pass();
    Ok(Json(answer))
}

#[derive(serde::Deserialize)]
struct TrashQuery {
    cursor: Option<String>,
    limit: Option<u32>,
}

fn trash_path_hint(saved: Option<&crate::account::SavedState>, parent: craft_cloud_proto::NodeId, name: &str) -> Option<String> {
    let saved = saved?;
    let parent_path = saved.state.base_path(parent)?;
    if parent_path.is_root() { Some(name.to_owned()) } else { Some(format!("{parent_path}/{name}")) }
}

async fn trash(
    State(state): State<Api>,
    query: Result<Query<TrashQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<craft_cloud_proto::local::TrashResponse>, LocalError> {
    let Query(query) = query.map_err(|e| LocalError::new(StatusCode::BAD_REQUEST, api_codes::INVALID, e.body_text()))?;
    let (_, transport) = server_api(&state)?;
    let response = transport.trash(query.cursor.as_deref(), query.limit).await.map_err(history_error)?;
    let saved = DataDir::new(&state.data).load_state().ok();
    Ok(Json(craft_cloud_proto::local::TrashResponse {
        items: response
            .items
            .into_iter()
            .map(|item| craft_cloud_proto::local::TrashItem {
                path: trash_path_hint(saved.as_ref(), item.parent, &item.name),
                id: item.id,
                parent: item.parent,
                name: item.name,
                is_dir: item.is_dir,
                size: item.size,
                deleted_ms: item.deleted_ms,
                purge_after_ms: item.purge_after_ms,
            })
            .collect(),
        next: response.next,
    }))
}

async fn trash_restore(
    State(state): State<Api>,
    body: Result<Json<craft_cloud_proto::local::TrashNodeRequest>, JsonRejection>,
) -> Result<Json<craft_cloud_proto::api::TrashRestoreResponse>, LocalError> {
    let Json(request) = body?;
    let id = request.id.parse().map_err(|_| LocalError::new(StatusCode::BAD_REQUEST, api_codes::INVALID, "id must be a node ID"))?;
    let (_, transport) = server_api(&state)?;
    let answer = transport.restore_from_trash(id).await.map_err(history_error)?;
    state.control.request_pass();
    Ok(Json(answer))
}

async fn trash_remove(
    State(state): State<Api>,
    body: Result<Json<craft_cloud_proto::local::TrashNodeRequest>, JsonRejection>,
) -> Result<StatusCode, LocalError> {
    let Json(request) = body?;
    let id = request.id.parse().map_err(|_| LocalError::new(StatusCode::BAD_REQUEST, api_codes::INVALID, "id must be a node ID"))?;
    let (_, transport) = server_api(&state)?;
    transport.remove_from_trash(id).await.map_err(history_error)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Equal strings, compared in time that doesn't depend on where they differ.
fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The hardening every request passes (see the module docs).
async fn guard(State(state): State<Api>, request: Request, next: Next) -> Result<Response, LocalError> {
    let expected = format!("127.0.0.1:{}", state.port);
    let host_ok = request.headers().get(HOST).and_then(|v| v.to_str().ok()) == Some(expected.as_str());
    let authority_ok = request.uri().authority().is_none_or(|a| a.as_str() == expected);
    if !host_ok || !authority_ok {
        return Err(LocalError::new(StatusCode::FORBIDDEN, codes::FORBIDDEN, format!("the Host must be {expected}")));
    }
    if request.headers().contains_key(ORIGIN) {
        return Err(LocalError::new(StatusCode::FORBIDDEN, codes::FORBIDDEN, "requests from web pages are refused"));
    }
    let token = request.headers().get(AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
    if !token.is_some_and(|t| same_secret(t, &state.token)) {
        return Err(LocalError::new(
            StatusCode::UNAUTHORIZED,
            api_codes::UNAUTHORIZED,
            format!("the token from {LOCAL_API_FILE} is missing or wrong (send it as `Authorization: Bearer <token>`)"),
        ));
    }
    Ok(next.run(request).await)
}

async fn status(State(state): State<Api>) -> Json<Status> {
    Json(state.control.status())
}

/// Hands `make(reply)` to the `run` loop and waits for its answer.
async fn ask(state: &ApiState, make: impl FnOnce(oneshot::Sender<Result<(), AgentError>>) -> Command) -> Result<Json<Status>, LocalError> {
    let (tx, rx) = oneshot::channel();
    let gone = || LocalError::new(StatusCode::SERVICE_UNAVAILABLE, api_codes::INTERNAL, "the agent is stopping");
    state.control.commands.send(make(tx)).await.map_err(|_| gone())?;
    rx.await.map_err(|_| gone())??;
    Ok(Json(state.control.status()))
}

async fn login(State(state): State<Api>, body: Result<Json<LoginRequest>, JsonRejection>) -> Result<Json<Status>, LocalError> {
    let Json(request) = body?;
    ask(&state, |reply| Command::Login(request, reply)).await
}

/// Most bytes of the small bodies (a password, a code, a name): far less than a join string's megabyte.
const TINY_JSON_BYTES: usize = 4 * 1024;

async fn login_second_factor(
    State(state): State<Api>,
    body: Result<Json<SecondFactorLoginRequest>, JsonRejection>,
) -> Result<Json<Status>, LocalError> {
    let Json(request) = body?;
    ask(&state, |reply| Command::LoginSecondFactor(request, reply)).await
}

/// A call of the signed-in account to the server, its refusals kept whole: never mistaken for a revoked device.
fn account_call_error(e: crate::http::CallError) -> LocalError {
    AgentError::from(e).into()
}

async fn two_factor_status(State(state): State<Api>) -> Result<Json<TwoFactorStatus>, LocalError> {
    let (_, transport) = server_api(&state)?;
    Ok(Json(transport.two_factor_status().await.map_err(account_call_error)?))
}

async fn two_factor_setup(
    State(state): State<Api>,
    body: Result<Json<PasswordRequest>, JsonRejection>,
) -> Result<Json<TwoFactorSetupResponse>, LocalError> {
    let Json(request) = body?;
    let (_, transport) = server_api(&state)?;
    Ok(Json(transport.two_factor_setup(&request.password).await.map_err(account_call_error)?))
}

async fn two_factor_confirm(State(state): State<Api>, body: Result<Json<TotpRequest>, JsonRejection>) -> Result<Json<RecoveryCodes>, LocalError> {
    let Json(request) = body?;
    let (_, transport) = server_api(&state)?;
    Ok(Json(transport.two_factor_confirm(&request.totp_code).await.map_err(account_call_error)?))
}

async fn two_factor_disable(State(state): State<Api>, body: Result<Json<TwoFactorProofRequest>, JsonRejection>) -> Result<StatusCode, LocalError> {
    let Json(request) = body?;
    let (_, transport) = server_api(&state)?;
    transport
        .two_factor_disable(&craft_cloud_proto::api::TwoFactorProofRequest {
            password: request.password,
            totp_code: request.totp_code,
            recovery_code: request.recovery_code,
        })
        .await
        .map_err(account_call_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn two_factor_recovery_codes(
    State(state): State<Api>,
    body: Result<Json<TwoFactorProofRequest>, JsonRejection>,
) -> Result<Json<RecoveryCodes>, LocalError> {
    let Json(request) = body?;
    let (_, transport) = server_api(&state)?;
    let sent = craft_cloud_proto::api::TwoFactorProofRequest {
        password: request.password,
        totp_code: request.totp_code,
        recovery_code: request.recovery_code,
    };
    Ok(Json(transport.two_factor_recovery_codes(&sent).await.map_err(account_call_error)?))
}

async fn change_password(State(state): State<Api>, body: Result<Json<ChangePasswordRequest>, JsonRejection>) -> Result<StatusCode, LocalError> {
    let Json(request) = body?;
    let (_, transport) = server_api(&state)?;
    let sent = craft_cloud_proto::api::ChangePasswordRequest { current_password: request.current_password, new_password: request.new_password };
    transport.change_password(&sent).await.map_err(account_call_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn revoke_other_devices(State(state): State<Api>) -> Result<Json<RevokedOthers>, LocalError> {
    let (_, transport) = server_api(&state)?;
    Ok(Json(transport.revoke_other_devices().await.map_err(account_call_error)?))
}

async fn rename_device(
    State(state): State<Api>,
    id: Result<Path<DeviceId>, PathRejection>,
    body: Result<Json<RenameDeviceRequest>, JsonRejection>,
) -> Result<Json<DeviceInfo>, LocalError> {
    let id = path_id(id)?;
    let Json(request) = body?;
    let (_, transport) = server_api(&state)?;
    let renamed = transport.rename_device(id, &request.name).await.map_err(|e| match e {
        // A device that isn't there (or isn't ours, or is signed out): the server's `not_found`.
        crate::http::CallError::Transport(craft_cloud_sync::TransportError::NotFound) => {
            LocalError::new(StatusCode::NOT_FOUND, api_codes::NOT_FOUND, "no such computer in this cloud")
        }
        other => account_call_error(other),
    })?;
    // The server has the new name. This computer's own name is kept here too (the agent owns the saved account), so the
    // status and the hub say the same. A rename of another computer changes nothing here.
    if state.control.account().is_some_and(|account| account.device == renamed.id) {
        let (tx, rx) = oneshot::channel();
        // An agent that is stopping has nothing left to update.
        if state.control.commands.send(Command::RenameDevice(renamed.id, renamed.name.clone(), tx)).await.is_ok() {
            let _ = rx.await;
        }
    }
    Ok(Json(renamed))
}

async fn logout(State(state): State<Api>, body: Result<Json<LogoutRequest>, JsonRejection>) -> Result<Json<Status>, LocalError> {
    let Json(request) = body?;
    ask(&state, |reply| Command::Logout(request, reply)).await
}

fn blob_error(e: BlobError) -> LocalError {
    let message = e.to_string();
    match e {
        BlobError::Size => LocalError::new(StatusCode::PAYLOAD_TOO_LARGE, api_codes::TOO_LARGE, message),
        BlobError::BadName => LocalError::new(StatusCode::BAD_REQUEST, api_codes::INVALID, message),
        BlobError::NotFound => LocalError::new(StatusCode::NOT_FOUND, codes::BLOB_NOT_FOUND, message),
        BlobError::Offline(_) => LocalError::new(StatusCode::SERVICE_UNAVAILABLE, codes::OFFLINE, message),
        BlobError::Failed(_) => LocalError::new(StatusCode::INTERNAL_SERVER_ERROR, api_codes::INTERNAL, message),
    }
}

/// `PUT /v1/blobs`: keeps the body as a blob, answers its name, and uploads it (now if it can, later otherwise).
async fn put_blob(
    State(state): State<Api>,
    body: Result<axum::body::Bytes, axum::extract::rejection::BytesRejection>,
) -> Result<Json<Stored>, LocalError> {
    let body = body.map_err(|e| LocalError::new(e.status(), api_codes::TOO_LARGE, e.body_text()))?;
    let account = state.control.account().ok_or(AgentError::NotSignedIn)?;
    let dir = app_blobs::dir(&state.data, account.space);
    let put_dir = dir.clone();
    let stored = tokio::task::spawn_blocking(move || app_blobs::put(&put_dir, &body))
        .await
        .map_err(|e| LocalError::new(StatusCode::INTERNAL_SERVER_ERROR, api_codes::INTERNAL, e.to_string()))?
        .map_err(blob_error)?;
    if let Ok(transport) = HttpTransport::new(account.server.clone(), &account.token) {
        tokio::spawn(async move {
            // This one, and any put while offline before it. Offline, they wait for the next try.
            app_blobs::upload_pending(&dir, &transport).await;
        });
    }
    Ok(Json(stored))
}

/// `GET /v1/blobs/{hash}`: the blob's bytes, downloaded first if this machine doesn't have them.
async fn get_blob(State(state): State<Api>, hash: Result<Path<String>, PathRejection>) -> Result<Response, LocalError> {
    let hash = parse_hex(&path_id(hash)?).ok_or_else(|| blob_error(BlobError::BadName))?;
    let account = state.control.account().ok_or(AgentError::NotSignedIn)?;
    let transport = HttpTransport::new(account.server.clone(), &account.token).ok();
    let bytes = app_blobs::get(app_blobs::dir(&state.data, account.space), hash, transport.as_ref()).await.map_err(blob_error)?;
    Ok(([(axum::http::header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
}

/// Uploads, every [`app_blobs::RETRY_EVERY`], the blobs put while the server couldn't be reached.
async fn retry_blobs(control: Control, data: PathBuf) {
    let mut every = tokio::time::interval(app_blobs::RETRY_EVERY);
    loop {
        every.tick().await;
        let Some(account) = control.account() else { continue };
        let Ok(transport) = HttpTransport::new(account.server.clone(), &account.token) else { continue };
        app_blobs::upload_pending(&app_blobs::dir(&data, account.space), &transport).await;
    }
}

async fn join(State(state): State<Api>, body: Result<Json<JoinRequest>, JsonRejection>) -> Result<Json<Status>, LocalError> {
    let Json(request) = body?;
    if request.join.len() > MAX_JOIN_BYTES {
        return Err(LocalError::new(StatusCode::BAD_REQUEST, api_codes::INVALID, "that is too long for a join string"));
    }
    ask(&state, |reply| Command::Join(request, reply)).await
}

/// The signed-in account's server API, for the calls the local API makes itself (they touch nothing here).
pub(crate) fn server_api(state: &ApiState) -> Result<(Account, HttpTransport), LocalError> {
    let account = state.control.account().ok_or(AgentError::NotSignedIn)?;
    let transport = HttpTransport::new(account.server.clone(), &account.token).map_err(AgentError::from)?;
    Ok((account, transport))
}

/// A path's ID, or `400 invalid`.
pub(crate) fn path_id<T>(id: Result<Path<T>, PathRejection>) -> Result<T, LocalError> {
    id.map(|Path(id)| id).map_err(|e| LocalError::new(StatusCode::BAD_REQUEST, api_codes::INVALID, e.body_text()))
}

/// The server's `not_found` for an add-a-computer code: it is gone.
fn pairing_gone(e: craft_cloud_sync::TransportError) -> LocalError {
    match e {
        craft_cloud_sync::TransportError::NotFound => LocalError::new(
            StatusCode::NOT_FOUND,
            api_codes::NOT_FOUND,
            "that add-a-computer code is gone (used, cancelled, replaced by a newer one, or expired)",
        ),
        other => AgentError::from(other).into(),
    }
}

async fn create_pairing(State(state): State<Api>) -> Result<Json<PairingInfo>, LocalError> {
    let (account, transport) = server_api(&state)?;
    let created = transport.create_pairing().await.map_err(AgentError::from)?;
    let code = Code::parse(&created.code, PAIRING_CHARS).ok_or_else(|| {
        AgentError::from(craft_cloud_sync::TransportError::Protocol("the server sent an add-a-computer code that doesn't read".into()))
    })?;
    let join = JoinString::new(account.server.as_str(), code);
    Ok(Json(PairingInfo { id: created.id, join: join.join(), link: join.link(), expires_ms: created.expires_ms }))
}

async fn pairing_status(State(state): State<Api>, id: Result<Path<uuid::Uuid>, PathRejection>) -> Result<Json<PairingStatus>, LocalError> {
    let id = path_id(id)?;
    let (_, transport) = server_api(&state)?;
    Ok(Json(transport.pairing_status(id).await.map_err(pairing_gone)?))
}

async fn delete_pairing(State(state): State<Api>, id: Result<Path<uuid::Uuid>, PathRejection>) -> Result<StatusCode, LocalError> {
    let id = path_id(id)?;
    let (_, transport) = server_api(&state)?;
    transport.delete_pairing(id).await.map_err(pairing_gone)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn devices(State(state): State<Api>) -> Result<Json<Vec<DeviceInfo>>, LocalError> {
    let (_, transport) = server_api(&state)?;
    Ok(Json(transport.devices().await.map_err(AgentError::from)?))
}

/// `GET /v1/usage`: asks the signed-in account's server each time; nothing is cached.
async fn usage(State(state): State<Api>) -> Result<Json<AccountUsage>, LocalError> {
    let (_, transport) = server_api(&state)?;
    Ok(Json(transport.usage().await.map_err(AgentError::from)?))
}

/// The saved sync state, read off the async threads. The state file is replaced atomically and read without any lock, so
/// this never waits for a pass.
async fn saved_state(state: &ApiState) -> Result<crate::account::SavedState, LocalError> {
    let data = DataDir::new(&state.data);
    tokio::task::spawn_blocking(move || data.load_state())
        .await
        .map_err(|e| LocalError::new(StatusCode::INTERNAL_SERVER_ERROR, api_codes::INTERNAL, e.to_string()))?
        .map_err(|e| LocalError::new(StatusCode::INTERNAL_SERVER_ERROR, api_codes::INTERNAL, e.to_string()))
}

/// `GET /v1/conflicts`: the conflict copies in the synced folder, from the saved state. Signed out is `409 not_signed_in`,
/// as for the other calls about the account's files.
async fn conflicts(State(state): State<Api>) -> Result<Json<ConflictsResponse>, LocalError> {
    state.control.account().ok_or(AgentError::NotSignedIn)?;
    let saved = saved_state(&state).await?;
    let list = tokio::task::spawn_blocking(move || crate::local_state::conflicts(&saved))
        .await
        .map_err(|e| LocalError::new(StatusCode::INTERNAL_SERVER_ERROR, api_codes::INTERNAL, e.to_string()))?;
    Ok(Json(list))
}

/// `GET /v1/usage/folders`: local logical bytes by top-level folder and kind, from the saved state (not the server's
/// stored bytes, which are `GET /v1/usage`).
async fn usage_folders(State(state): State<Api>) -> Result<Json<FolderUsageResponse>, LocalError> {
    state.control.account().ok_or(AgentError::NotSignedIn)?;
    let saved = saved_state(&state).await?;
    let usage = tokio::task::spawn_blocking(move || crate::local_state::folder_usage(&saved))
        .await
        .map_err(|e| LocalError::new(StatusCode::INTERNAL_SERVER_ERROR, api_codes::INTERNAL, e.to_string()))?;
    Ok(Json(usage))
}

/// Removes (revokes) another computer of the account; answered with the list as it is then. This computer can't be
/// removed this way: that is signing out.
async fn remove_device(State(state): State<Api>, id: Result<Path<DeviceId>, PathRejection>) -> Result<Json<Vec<DeviceInfo>>, LocalError> {
    let id = path_id(id)?;
    let (account, transport) = server_api(&state)?;
    if id == account.device {
        return Err(AgentError::CannotRemoveSelf.into());
    }
    transport.revoke(id).await.map_err(|e| match e {
        craft_cloud_sync::TransportError::NotFound => LocalError::new(StatusCode::NOT_FOUND, api_codes::NOT_FOUND, "no such computer in this cloud"),
        other => AgentError::from(other).into(),
    })?;
    Ok(Json(transport.devices().await.map_err(AgentError::from)?))
}

/// The personal space as `GET /v1/spaces` lists it: always syncing (to the account's folder), the account its owner.
fn personal_space(control: &Control, account: &Account) -> SpaceInfo {
    let status = control.status();
    SpaceInfo {
        space: account.space,
        kind: "personal".into(),
        name: None,
        role: craft_cloud_proto::api::SpaceRole::Owner,
        state: "active".into(),
        archived: false,
        folder: Some(account.folder.display().to_string()),
        sync: SpaceSync::On,
        run_state: Some(match status.run_state {
            RunState::Retrying { message } => SpaceRun::Retrying { message },
            RunState::Paused { reason, reason_code } => SpaceRun::Paused { reason, reason_code },
            RunState::PausedByUser => SpaceRun::PausedByUser,
            RunState::Syncing => SpaceRun::Syncing,
            RunState::SignedOut | RunState::Idle | RunState::Stopping => SpaceRun::Idle,
        }),
        last_sync_ms: status.last_sync_ms,
        read_only: false,
    }
}

/// `GET /v1/spaces`: the personal space, then the team spaces (invitations among them) as the agent last saw the server's list.
async fn spaces(State(state): State<Api>) -> Result<Json<SpaceList>, LocalError> {
    let account = state.control.account().ok_or(AgentError::NotSignedIn)?;
    let mut list = vec![personal_space(&state.control, &account)];
    list.extend(state.control.with(|s| s.spaces.clone()));
    Ok(Json(SpaceList { spaces: list }))
}

/// `POST /v1/spaces/from-folder`: makes a team space from a folder (see [`crate::from_folder`]).
async fn from_folder(State(state): State<Api>, body: Result<Json<FromFolderRequest>, JsonRejection>) -> Result<Json<FromFolderStatus>, LocalError> {
    let Json(request) = body?;
    let start = crate::from_folder::Start { control: state.control.clone(), data: DataDir::new(&state.data) };
    Ok(Json(crate::from_folder::start(start, request).await?))
}

/// `POST /v1/spaces/{id}/accept`: passed on to the server; the agent looks at its list again before it answers.
async fn accept_space(
    State(state): State<Api>,
    id: Result<Path<craft_cloud_proto::SpaceId>, PathRejection>,
) -> Result<Json<craft_cloud_proto::api::SpaceListItem>, LocalError> {
    let id = path_id(id)?;
    let (_, transport) = server_api(&state)?;
    let accepted = transport.accept_space(id).await?;
    if let Some(spaces) = state.control.spaces_handle() {
        spaces.refresh().await;
    }
    Ok(Json(accepted))
}

/// `POST /v1/spaces/{id}/decline`: passed on to the server.
async fn decline_space(State(state): State<Api>, id: Result<Path<craft_cloud_proto::SpaceId>, PathRejection>) -> Result<StatusCode, LocalError> {
    let id = path_id(id)?;
    let (_, transport) = server_api(&state)?;
    transport.decline_space(id).await?;
    if let Some(spaces) = state.control.spaces_handle() {
        spaces.refresh().await;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/spaces/{id}/sync`: turns the sync of a team space on this computer on or off.
async fn sync_space(
    State(state): State<Api>,
    id: Result<Path<craft_cloud_proto::SpaceId>, PathRejection>,
    body: Result<Json<SpaceSyncRequest>, JsonRejection>,
) -> Result<Json<SpaceInfo>, LocalError> {
    let id = path_id(id)?;
    let Json(request) = body?;
    let spaces = state.control.spaces_handle().ok_or(AgentError::NotSignedIn)?;
    Ok(Json(spaces.sync(id, request).await?))
}

/// `POST /v1/spaces/{id}/approve-deletes`: hands the approval of a synced team space's waiting deletions to this agent
/// (`spaces/<id>/approval.json`), by the rules of `sync --approve-deletes --space`: with a `set`, that set if it is the
/// one waiting; without, the set the previous call listed.
async fn approve_space_deletes(
    State(state): State<Api>,
    id: Result<Path<craft_cloud_proto::SpaceId>, PathRejection>,
    body: Result<Json<ApproveDeletesRequest>, JsonRejection>,
) -> Result<Json<ApproveDeletesAnswer>, LocalError> {
    let id = path_id(id)?;
    let Json(request) = body?;
    let approve = match request.set.as_deref().map(str::trim) {
        None => crate::cli::Approve::Listed,
        Some("") => {
            return Err(LocalError::new(
                StatusCode::BAD_REQUEST,
                api_codes::INVALID,
                "the set is empty; leave it out to approve the set last listed",
            ));
        }
        Some(set) => crate::cli::Approve::Set(set.to_ascii_lowercase()),
    };
    let data = DataDir::new(&state.data);
    let answer = tokio::task::spawn_blocking(move || {
        let account = data.load_account()?.ok_or(AgentError::NotSignedIn)?;
        let setting = account.spaces.iter().find(|s| s.space == id && s.sync);
        let Some(setting) = setting else { return Ok(None) };
        let mut out = Vec::new();
        let (_, summary, approved) = crate::cli::approve_team(&data, setting, &approve, &mut out)?;
        Ok::<_, AgentError>(Some(held_answer(&summary, approved, String::from_utf8_lossy(&out).trim())))
    })
    .await
    .map_err(|e| LocalError::new(StatusCode::INTERNAL_SERVER_ERROR, api_codes::INTERNAL, e.to_string()))??;
    answer.map(Json).ok_or_else(|| LocalError::new(StatusCode::NOT_FOUND, api_codes::NOT_FOUND, "no such team space syncing on this computer"))
}

/// The answer to an approve-deletes call from what the pass last found: the waiting set with its paths (the first
/// [`MAX_HELD_ENTRIES`], at most [`MAX_HELD_BYTES`] of paths), the deletions held behind a link, and the command line's words.
pub(crate) fn held_answer(summary: &crate::account::PassSummary, approved: bool, words: &str) -> ApproveDeletesAnswer {
    use craft_cloud_proto::local::{HeldEntry, HeldLink, MAX_HELD_BYTES, MAX_HELD_ENTRIES};
    let all = summary.pending_deletes.iter().map(|p| (p, "server")).chain(summary.pending_local_deletes.iter().map(|p| (p, "here")));
    let (mut entries, mut bytes, mut total) = (Vec::new(), 0usize, 0u64);
    for (path, side) in all {
        total += 1;
        if entries.len() < MAX_HELD_ENTRIES && bytes.saturating_add(path.len()) <= MAX_HELD_BYTES {
            bytes += path.len();
            entries.push(HeldEntry { path: path.clone(), side: side.to_owned() });
        }
    }
    let truncated = (entries.len() as u64) < total;
    let held_total = summary.held_deletes.len() as u64;
    let held: Vec<HeldLink> = summary
        .held_deletes
        .iter()
        .take(100)
        .map(|(path, links)| HeldLink { path: path.clone(), links: links.iter().take(5).cloned().collect() })
        .collect();
    // The words are for the command line; an app reads the fields above.
    let mut message = words.to_owned();
    if message.len() > 16 * 1024 {
        let mut end = 16 * 1024;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
        message.push_str(
            "
…",
        );
    }
    ApproveDeletesAnswer { approved, message, set: summary.pending_set(), total, truncated, entries, held_set: summary.held_set(), held_total, held }
}

/// Registers this agent to start at login, or removes the entry; the status after says which.
async fn set_autostart(State(state): State<Api>, body: Result<Json<AutostartRequest>, JsonRejection>) -> Result<Json<Status>, LocalError> {
    let Json(AutostartRequest { on }) = body?;
    let roots = state
        .control
        .autostart_roots()
        .ok_or_else(|| LocalError::new(StatusCode::CONFLICT, codes::AUTOSTART_FAILED, state.control.autostart_refusal.clone()))?;
    let data = state.data.clone();
    let done = tokio::task::spawn_blocking(move || if on { set_on(&roots, &data) } else { autostart::unregister(&roots) }).await;
    state.control.refresh_autostart().await;
    match done {
        Ok(Ok(())) => Ok(Json(state.control.status())),
        Ok(Err(e)) => Err(LocalError::new(StatusCode::INTERNAL_SERVER_ERROR, codes::AUTOSTART_FAILED, e.to_string())),
        Err(e) => Err(LocalError::new(StatusCode::INTERNAL_SERVER_ERROR, codes::AUTOSTART_FAILED, e.to_string())),
    }
}

/// Registers this agent, with the data folder `data`, to start at login at `roots`.
pub fn set_on(roots: &Roots, data: &std::path::Path) -> Result<(), autostart::AutostartError> {
    autostart::register(roots, &autostart::this_agent(data)?)
}

async fn pause(State(state): State<Api>) -> Result<Json<Status>, LocalError> {
    ask(&state, Command::Pause).await
}

async fn resume(State(state): State<Api>) -> Result<Json<Status>, LocalError> {
    ask(&state, Command::Resume).await
}

async fn quit(State(state): State<Api>) -> Result<Json<Status>, LocalError> {
    ask(&state, Command::Quit).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The answer to a look at held deletions lists up to the cap, counts them all, names the set and says it was cut; the
    /// words are cut too, and a short list is whole.
    #[test]
    fn a_held_answer_is_capped_counted_and_names_its_set() {
        use crate::account::PassSummary;
        let many = PassSummary {
            pending_deletes: (0..1500).map(|n| format!("Shots/{n:05}.psd")).collect(),
            pending_local_deletes: (0..1000).map(|n| format!("Local/{n:05}.txt")).collect(),
            held_deletes: vec![("Linked/a.txt".into(), vec!["Links/one".into(), "Links/two".into()])],
            ..PassSummary::default()
        };
        let words = "x".repeat(40_000);
        let answer = held_answer(&many, false, &words);
        assert_eq!((answer.total, answer.entries.len(), answer.truncated), (2500, 1000, true));
        assert!(answer.entries.iter().all(|e| e.side == "server"), "the first 1000 are the server's");
        assert!(answer.set.as_deref().is_some_and(|s| s.len() == 12), "{:?}", answer.set);
        assert!(answer.message.len() < 17 * 1024 && answer.message.ends_with('…'));
        assert_eq!((answer.held_total, answer.held.len()), (1, 1));
        assert!(serde_json::to_vec(&answer).unwrap().len() < 200 * 1024, "far under the local API's limit");
        let few = PassSummary { pending_deletes: vec!["a".into()], pending_local_deletes: vec!["b".into()], ..PassSummary::default() };
        let answer = held_answer(&few, true, "ok");
        assert_eq!((answer.approved, answer.total, answer.truncated, answer.entries.len()), (true, 2, false, 2));
        assert_eq!(answer.entries.iter().map(|e| e.side.as_str()).collect::<Vec<_>>(), ["server", "here"]);
        let none = held_answer(&PassSummary::default(), false, "nothing was waiting for approval");
        assert_eq!((none.set, none.total, none.held_total), (None, 0, 0));
        // The bytes of paths are capped too.
        let long = PassSummary { pending_deletes: (0..400).map(|n| format!("{n:04}{}", "d".repeat(4000))).collect(), ..PassSummary::default() };
        let answer = held_answer(&long, false, "");
        assert!(answer.truncated && answer.entries.len() < 400 && answer.total == 400);
    }

    #[test]
    fn secrets_compare_whole() {
        assert!(same_secret("abc", "abc"));
        assert!(!same_secret("abc", "abd"));
        assert!(!same_secret("abc", "ab"));
        assert!(!same_secret("", "a"));
    }

    #[test]
    fn agent_errors_map_to_stable_codes() {
        let code = |e: AgentError| LocalError::from(e).code;
        assert_eq!(code(AgentError::WrongPassword), codes::WRONG_PASSWORD);
        assert_eq!(code(AgentError::NotSignedIn), codes::NOT_SIGNED_IN);
        assert_eq!(code(AgentError::Offline("x".into())), codes::OFFLINE);
        assert_eq!(code(AgentError::LogoutOffline("x".into())), codes::OFFLINE);
        assert_eq!(code(AgentError::Usage("already signed in as ana".into())), codes::ALREADY_SIGNED_IN);
        assert_eq!(code(AgentError::Usage("not a URL".into())), api_codes::INVALID);
        assert_eq!(code(AgentError::Usage(crate::spaces::CHANGING.into())), codes::CONFLICT);
        assert_eq!(code(AgentError::Revoked), codes::NEEDS_YOU);
        assert_eq!(code(AgentError::InvalidCode), codes::INVALID_CODE);
        assert_eq!(code(AgentError::PasswordRequired), codes::PASSWORD_REQUIRED);
        assert_eq!(code(AgentError::WeakPassword("x".into())), codes::WEAK_PASSWORD);
        assert_eq!(code(AgentError::CannotRemoveSelf), codes::CANNOT_REMOVE_SELF);
        assert_eq!(code(AgentError::FolderHasFiles(PathBuf::from("f"))), codes::FOLDER_HAS_FILES);
        assert_eq!(code(AgentError::NeedsYou("account.json is damaged".into())), codes::NEEDS_YOU);
        assert_eq!(code(AgentError::UntrustedCertificate { host: "h".into() }), codes::UNTRUSTED_CERTIFICATE);
        assert_eq!(code(AgentError::Unreachable { server: "s".into(), message: "m".into() }), codes::OFFLINE);
        let after = LocalError::from(AgentError::JoinSetup(Box::new(AgentError::Revoked)));
        assert_eq!(after.code, codes::NEEDS_YOU, "as what failed");
        assert!(after.message.contains("make a new one"), "{}", after.message);
    }
}
