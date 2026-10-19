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
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::StatusCode;
use axum::http::header::{AUTHORIZATION, HOST, ORIGIN};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use craft_cloud_proto::DeviceId;
use craft_cloud_proto::api::{DeviceInfo, ErrorBody, PairingStatus, codes as api_codes};
use craft_cloud_proto::join::{Code, JoinString, MAX_JOIN_BYTES, PAIRING_CHARS};
use craft_cloud_proto::local::{
    AutostartRequest, JoinRequest, LOCAL_API_FILE, LOCAL_API_MAX, LOCAL_API_SCHEMA, LocalApiFile, LoginRequest, LogoutRequest, MAX_LOCAL_JSON_BYTES,
    PairingInfo, RunState, Status, codes, paths,
};
use tokio::sync::{mpsc, oneshot};

use crate::account::{Account, DataDir, random_hex, write_atomic};
use crate::autostart::{self, Roots};
use crate::cli::{AgentError, Exit};
use crate::http::HttpTransport;

/// What the local API asks the `run` loop to do, with where to send the outcome.
pub enum Command {
    Login(LoginRequest, oneshot::Sender<Result<(), AgentError>>),
    Join(JoinRequest, oneshot::Sender<Result<(), AgentError>>),
    Logout(LogoutRequest, oneshot::Sender<Result<(), AgentError>>),
    /// Hold syncing and the live connection until [`Command::Resume`], also after a restart.
    Pause(oneshot::Sender<Result<(), AgentError>>),
    Resume(oneshot::Sender<Result<(), AgentError>>),
    /// Finish the pass under way and stop the agent. Answered at once.
    Quit(oneshot::Sender<Result<(), AgentError>>),
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
}

/// The loop's side of the local API: the status it reports, and the commands it receives. Cheap to clone.
#[derive(Clone)]
pub struct Control {
    shared: Arc<Mutex<Shared>>,
    commands: mpsc::Sender<Command>,
    /// Where the start-at-login entry goes (`None`: `autostart` is refused, with `autostart_refusal`).
    autostart: Option<Roots>,
    autostart_refusal: String,
}

impl Control {
    /// A control for a loop that receives its commands from `commands`' other end.
    pub fn new(commands: mpsc::Sender<Command>) -> Self {
        let shared =
            Shared { account: None, run_state: RunState::SignedOut, online: false, hub: None, autostart: None, last_sync_ms: None, removed: false };
        let autostart_refusal = "this agent doesn't handle starting at login here".to_owned();
        Self { shared: Arc::new(Mutex::new(shared)), commands, autostart: None, autostart_refusal }
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
            }
        });
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
        })
    }
}

/// An error answer: the HTTP status and the [`ErrorBody`].
#[derive(Debug)]
pub struct LocalError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl LocalError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self { status, code, message: message.into() }
    }
}

impl IntoResponse for LocalError {
    fn into_response(self) -> Response {
        (self.status, Json(ErrorBody { error: self.code.to_owned(), message: self.message, ..ErrorBody::default() })).into_response()
    }
}

impl From<AgentError> for LocalError {
    fn from(e: AgentError) -> Self {
        let message = e.to_string();
        // A setup that failed after the code was spent answers as what failed, with the whole message.
        if let AgentError::JoinSetup(inner) = e {
            let inner = LocalError::from(*inner);
            return Self::new(inner.status, inner.code, message);
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

/// The running local API. Dropping it stops the server and removes `local-api.json`.
pub struct LocalApi {
    port: u16,
    file: PathBuf,
    task: tokio::task::JoinHandle<()>,
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
        let state = Arc::new(ApiState { port, token: token.clone(), control, data: data.path().to_owned() });
        let app = router(state);
        let (shutdown, shutting_down) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let stop = async move {
                let _ = shutting_down.await;
            };
            if let Err(e) = axum::serve(listener, app).with_graceful_shutdown(stop).await {
                crate::runlog::note(format_args!("the local API stopped: {e}"));
            }
        });
        let record = LocalApiFile { schema: LOCAL_API_SCHEMA, port, token, pid: std::process::id() };
        let json = serde_json::to_vec_pretty(&record).map_err(|e| AgentError::Io { path: file.clone(), source: std::io::Error::other(e) })?;
        if let Err(e) = write_atomic(&file, &json, true) {
            task.abort();
            return Err(io(e));
        }
        Ok(Self { port, file, task, shutdown: Some(shutdown) })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Stops the server, letting the answers under way go out first (the one to `quit` among them), for at most
    /// [`STOP_GRACE`], and removes `local-api.json`.
    pub async fn stop(mut self) {
        let _ = std::fs::remove_file(&self.file);
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
        let _ = std::fs::remove_file(&self.file);
    }
}

type Api = Arc<ApiState>;

fn router(state: Api) -> Router {
    Router::new()
        .route(paths::STATUS, get(status))
        .route(paths::LOGIN, post(login))
        .route(paths::LOGOUT, post(logout))
        .route(paths::RECENTS, get(crate::live::recents_socket))
        .route(paths::JOIN, post(join))
        .route(paths::PAIRINGS, post(create_pairing))
        .route(&format!("{}/{{id}}", paths::PAIRINGS), get(pairing_status).delete(delete_pairing))
        .route(paths::DEVICES, get(devices))
        .route(&format!("{}/{{id}}", paths::DEVICES), delete(remove_device))
        .route(paths::AUTOSTART, post(set_autostart))
        .route(paths::PAUSE, post(pause))
        .route(paths::RESUME, post(resume))
        .route(paths::QUIT, post(quit))
        .fallback(|| async { LocalError::new(StatusCode::NOT_FOUND, api_codes::NOT_FOUND, "no such call") })
        .layer(DefaultBodyLimit::max(MAX_LOCAL_JSON_BYTES))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
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

async fn logout(State(state): State<Api>, body: Result<Json<LogoutRequest>, JsonRejection>) -> Result<Json<Status>, LocalError> {
    let Json(request) = body?;
    ask(&state, |reply| Command::Logout(request, reply)).await
}

async fn join(State(state): State<Api>, body: Result<Json<JoinRequest>, JsonRejection>) -> Result<Json<Status>, LocalError> {
    let Json(request) = body?;
    if request.join.len() > MAX_JOIN_BYTES {
        return Err(LocalError::new(StatusCode::BAD_REQUEST, api_codes::INVALID, "that is too long for a join string"));
    }
    ask(&state, |reply| Command::Join(request, reply)).await
}

/// The signed-in account's server API, for the calls the local API makes itself (they touch nothing here).
fn server_api(state: &ApiState) -> Result<(Account, HttpTransport), LocalError> {
    let account = state.control.account().ok_or(AgentError::NotSignedIn)?;
    let transport = HttpTransport::new(account.server.clone(), &account.token).map_err(AgentError::from)?;
    Ok((account, transport))
}

/// A path's ID, or `400 invalid`.
fn path_id<T>(id: Result<Path<T>, PathRejection>) -> Result<T, LocalError> {
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
