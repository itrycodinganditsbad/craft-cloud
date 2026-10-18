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
//! Sign-in and sign-out are the `run` loop's to carry out (it holds the data folder's lock): a call hands them over as
//! a [`Command`] and waits for the answer. Status is read from [`Control`], which the loop keeps up to date.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::StatusCode;
use axum::http::header::{AUTHORIZATION, HOST, ORIGIN};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use craft_cloud_proto::api::{ErrorBody, codes as api_codes};
use craft_cloud_proto::local::{
    LOCAL_API_FILE, LOCAL_API_MAX, LOCAL_API_SCHEMA, LocalApiFile, LoginRequest, LogoutRequest, MAX_LOCAL_JSON_BYTES, RunState, Status, codes, paths,
};
use tokio::sync::{mpsc, oneshot};

use crate::account::{Account, DataDir, random_hex, write_atomic};
use crate::cli::{AgentError, Exit};

/// What the local API asks the `run` loop to do, with where to send the outcome.
pub enum Command {
    Login(LoginRequest, oneshot::Sender<Result<(), AgentError>>),
    Logout(LogoutRequest, oneshot::Sender<Result<(), AgentError>>),
}

/// What [`Status`] reports, kept up to date by the `run` loop.
#[derive(Clone)]
struct Shared {
    account: Option<Account>,
    run_state: RunState,
    online: bool,
    /// The live hub, while signed in.
    hub: Option<crate::live::HubHandle>,
}

/// The loop's side of the local API: the status it reports, and the commands it receives. Cheap to clone.
#[derive(Clone)]
pub struct Control {
    shared: Arc<Mutex<Shared>>,
    commands: mpsc::Sender<Command>,
}

impl Control {
    /// A control for a loop that receives its commands from `commands`' other end.
    pub fn new(commands: mpsc::Sender<Command>) -> Self {
        Self { shared: Arc::new(Mutex::new(Shared { account: None, run_state: RunState::SignedOut, online: false, hub: None })), commands }
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
            }
        });
    }

    pub fn set_run_state(&self, state: RunState) {
        self.with(|s| s.run_state = state);
    }

    /// Whether the agent reaches the server right now (the live hub's connection is up).
    pub fn set_online(&self, online: bool) {
        self.with(|s| s.online = online);
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
        let (status, code) = match &e {
            AgentError::WrongPassword => (StatusCode::UNAUTHORIZED, codes::WRONG_PASSWORD),
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
}

/// The running local API. Dropping it stops the server and removes `local-api.json`.
pub struct LocalApi {
    port: u16,
    file: PathBuf,
    task: tokio::task::JoinHandle<()>,
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
        let state = Arc::new(ApiState { port, token: token.clone(), control });
        let app = router(state);
        let task = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                eprintln!("the local API stopped: {e}");
            }
        });
        let record = LocalApiFile { schema: LOCAL_API_SCHEMA, port, token, pid: std::process::id() };
        let json = serde_json::to_vec_pretty(&record).map_err(|e| AgentError::Io { path: file.clone(), source: std::io::Error::other(e) })?;
        if let Err(e) = write_atomic(&file, &json, true) {
            task.abort();
            return Err(io(e));
        }
        Ok(Self { port, file, task })
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

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
    }
}
