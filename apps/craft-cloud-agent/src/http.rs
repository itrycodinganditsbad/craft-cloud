//! The server's HTTP API (`/api/v1`) as the engine's [`Transport`], plus sign-in, joining with a code ([`join`]),
//! add-a-computer codes, and listing and revoking devices.
//!
//! - **An untrusted certificate** is told apart from other failures where a sign-in or join first reaches the
//!   server ([`reach`]): rustls' certificate errors are looked for in reqwest's error chain.
//! - **HTTPS only**, except to a loopback address (`127.0.0.0/8`, `::1`, `localhost`), where a test or a
//!   server on the same machine needs no certificate. `localhost` is pinned to `127.0.0.1` and `::1` (never
//!   looked up), so a hosts file or resolver can't send plain HTTP elsewhere. TLS is rustls with the platform's
//!   certificate store.
//! - **No proxies**: `HTTP_PROXY`, `ALL_PROXY` and friends are ignored, so nothing meant for this machine (or
//!   the token) is handed to a proxy. Proxy support for HTTPS may come later.
//! - **Everything from the network is capped** before it is buffered: a chunk at [`MAX_CHUNK_BYTES`], a
//!   manifest at [`MAX_MANIFEST_BYTES`], a tree or change-feed page at [`MAX_PAGE_JSON_BYTES`] and every other
//!   answer (including error bodies) at [`MAX_SMALL_JSON_BYTES`]. Lists are checked against the API's limits.
//! - **Timeouts**: 10 s to connect, 60 s without any data arriving, and a whole-request cap of 60 s plus 15 s
//!   per MiB of the largest body the request may carry (a 4 MiB chunk gets 2 minutes: that still works at
//!   ~35 KB/s). A stalled connection fails after a minute either way.
//! - **Answers map onto [`TransportError`]** by the server's error codes first ([`codes`]), then by status:
//!   a refused batch with its conflicts, missing chunks, `resync_required` and `quota_exceeded` (with how much the
//!   account stores and may store) and `read_only` (the account's role in the space doesn't allow the write: a viewer) by code; 401 is `Unauthorized`,
//!   404 `NotFound`; 408, 429, 5xx, connection failures and timeouts are `Unavailable` (try again later);
//!   anything else, and any answer that doesn't parse, is `Protocol` (another 4xx says "the server or a proxy
//!   refused the request (HTTP <code>)": a reverse proxy in front of the server may be the one refusing).
//! - **The device token** travels only in the `Authorization` header (marked sensitive) and never appears in
//!   `Debug` output or error messages. Redirects are not followed, so it never goes anywhere else.
//!
//! The requests are built and the answers read here, over a [`Connection`] that only carries one request and its
//! answer: [`Reqwest`] in the agent (the TLS, proxy, redirect, connect and read-timeout rules above are its own),
//! a simulated network in the tests (`tests/network/`), which so runs this module's requests, error mapping,
//! body caps and long-poll over partitions and lost answers ([`info_over`] and [`login_over`] too).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use craft_cloud_proto::api::{
    AccountInfo, ChangePasswordRequest, CommitRequest, CommitResponse, DeviceInfo, ErrorBody, HistoryResponse, InfoResponse, JoinRequest,
    LoginRequest, LoginResponse, MAX_MISSING_IDS, MissingRequest, MissingResponse, PROTOCOL_MAX, PROTOCOL_MIN, PairingCreated, PairingStatus,
    RecoveryCodes, RenameDeviceRequest, RestoreRequest, RestoreResponse, RevokedOthers, SecondFactorRequest, SpaceList, SpaceListItem, SpaceResponse,
    TwoFactorChallenge, TwoFactorConfirmRequest, TwoFactorProofRequest, TwoFactorSetupRequest, TwoFactorSetupResponse, TwoFactorStatus, codes,
};
use craft_cloud_proto::limits::{MAX_BATCH_OPS, MAX_CHUNK_BYTES, MAX_MANIFEST_BYTES, MAX_PAGE_NODES};
use craft_cloud_proto::local::MAX_MACHINE_BYTES;
use craft_cloud_proto::{ChangeSet, ChunkId, DeviceId, ManifestId, NodeId, Op, PageCursor, SaveRecord, SpaceId, VersionId};
use craft_cloud_sync::{Transport, TransportError};
use reqwest::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, RETRY_AFTER, USER_AGENT};
use reqwest::{Client, Method, StatusCode, Url};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Largest tree or change-feed page, in bytes of JSON: [`MAX_PAGE_NODES`] nodes of at most about 1.5 KiB each
/// (a 255-byte name that JSON-escapes to twice that, IDs, hashes and numbers), with room to spare.
pub const MAX_PAGE_JSON_BYTES: usize = 16 * 1024 * 1024;
/// Largest other answer: a missing-chunk list of [`MAX_MISSING_IDS`] IDs is about 670 KiB.
pub const MAX_SMALL_JSON_BYTES: usize = 2 * 1024 * 1024;
/// Most devices of one account the agent reads from the server.
pub const MAX_DEVICES: usize = 1000;
/// Longest add-a-computer code the agent takes from the server, in bytes (they are 8 characters).
const MAX_CODE_BYTES: usize = 64;
/// Longest server message shown to the user, in characters.
const MAX_MESSAGE_CHARS: usize = 300;
/// The whole of an audit export, from connecting to the last byte (it is read as it comes, with the usual idle limit).
const EXPORT_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// No data at all for this long fails the request.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
const BASE_TIMEOUT_SECS: u64 = 60;
const SECS_PER_MIB: u64 = 15;
/// What the agent sends as its User-Agent: generic, nothing about the user or the machine.
const AGENT: &str = concat!("Craft-Cloud/", env!("CARGO_PKG_VERSION"));

/// A device token. Its `Debug` output is redacted; it is written only to the owner-only account file.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeviceToken(String);

impl DeviceToken {
    pub fn new(token: String) -> Self {
        Self(token)
    }

    /// `Bearer <token>`, marked sensitive (so it is never shown in a header dump).
    pub fn bearer(&self) -> Result<HeaderValue, TransportError> {
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", self.0))
            .map_err(|_| TransportError::Protocol("the saved device token is damaged; sign in again".into()))?;
        auth.set_sensitive(true);
        Ok(auth)
    }
}

/// What the agent sends as its User-Agent (generic: nothing about the user or the machine).
pub const USER_AGENT_VALUE: &str = AGENT;

/// `localhost` always means this machine (see [`ServerUrl`]): the addresses a plain-HTTP loopback server is reached at.
pub const LOCALHOST_ADDRS: [SocketAddr; 2] = LOCALHOST;

impl std::fmt::Debug for DeviceToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DeviceToken(<redacted>)")
    }
}

/// Why a server address was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UrlError {
    #[error("{0} is not a URL (expected something like https://cloud.example.com)")]
    Unparseable(String),
    #[error("the server address must start with https:// (http:// is only for this computer: localhost or 127.0.0.1)")]
    NotHttps,
    #[error("the server address can't hold a user name, password, query or fragment")]
    Extras,
}

/// A server's base address, checked and normalised: `https://host[:port][/prefix]`, without `/api/v1` and
/// without a trailing slash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ServerUrl(Url);

impl ServerUrl {
    pub fn parse(text: &str) -> Result<Self, UrlError> {
        let mut url = Url::parse(text.trim()).map_err(|_| UrlError::Unparseable(text.to_owned()))?;
        match url.scheme() {
            "https" => {}
            "http" if is_loopback(&url) => {}
            _ => return Err(UrlError::NotHttps),
        }
        if !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
            return Err(UrlError::Extras);
        }
        let path = url.path().trim_end_matches('/');
        let path = path.strip_suffix("/api/v1").unwrap_or(path).trim_end_matches('/').to_owned();
        url.set_path(&path);
        Ok(Self(url))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str().trim_end_matches('/')
    }

    /// The server's host name (or address), without the port: what its certificate names.
    pub fn host(&self) -> String {
        self.0.host_str().unwrap_or_default().to_owned()
    }

    /// Whether the server is on this machine (plain HTTP is allowed then).
    pub fn is_loopback(&self) -> bool {
        is_loopback(&self.0)
    }

    /// The live service, `/api/v1/live`, as a WebSocket address: `wss://` for `https://`, `ws://` for a server on this
    /// machine.
    pub fn live_url(&self) -> Url {
        let mut url = self.endpoint("live");
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        // Switching between the special schemes keeps everything else.
        let _ = url.set_scheme(scheme);
        url
    }

    /// The API endpoint `/api/v1/<path>`.
    fn endpoint(&self, path: &str) -> Url {
        let mut url = self.0.clone();
        let base = url.path().trim_end_matches('/').to_owned();
        url.set_path(&format!("{base}/api/v1/{path}"));
        url
    }
}

impl TryFrom<String> for ServerUrl {
    type Error = UrlError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl From<ServerUrl> for String {
    fn from(url: ServerUrl) -> Self {
        url.as_str().to_owned()
    }
}

impl std::fmt::Display for ServerUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

fn is_loopback(url: &Url) -> bool {
    match url.host_str() {
        Some(host) if host.eq_ignore_ascii_case("localhost") => true,
        Some(host) => host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()),
        None => false,
    }
}

/// `localhost` always means this machine: these addresses, whatever a resolver says (port 0: the URL's port).
const LOCALHOST: [SocketAddr; 2] = [SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0), SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 0)];

/// One request as this module builds it, for a [`Connection`] to carry. Its `Debug` output names the method, the
/// URL, the headers' names and the body's length, never a header's value or the body (a sign-in's body holds the
/// password).
#[derive(Clone)]
pub struct Request {
    /// The HTTP method.
    pub method: Method,
    /// The endpoint, with its query.
    pub url: Url,
    /// Every header the request carries (the token's is marked sensitive).
    pub headers: HeaderMap,
    /// The body (JSON or a chunk's or manifest's bytes), if the request has one.
    pub body: Option<Bytes>,
    /// The whole request, from connecting to the answer's last byte, must take no longer.
    pub timeout: Duration,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("method", &self.method)
            .field("url", &self.url.as_str())
            .field("headers", &self.headers.keys().map(|k| k.as_str()).collect::<Vec<_>>())
            .field("body_bytes", &self.body.as_ref().map(Bytes::len))
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// An answer's status and headers; its body is read piece by piece, so it is capped before it is buffered.
pub struct Answer {
    /// The HTTP status.
    pub status: StatusCode,
    /// The answer's headers (`Content-Length`, `Retry-After`, the space's epoch).
    pub headers: HeaderMap,
    /// The body, as it arrives.
    pub body: Box<dyn AnswerBody>,
}

/// The body of an [`Answer`], as it arrives.
#[async_trait]
pub trait AnswerBody: Send {
    /// The next piece, or `None` after the last one. A connection that breaks or times out meanwhile is
    /// `Unavailable`.
    async fn next(&mut self) -> Result<Option<Bytes>, TransportError>;
}

/// Carries one [`Request`] to the server and brings its [`Answer`] back: everything below HTTP's meaning. A request
/// that can't be sent, or whose answer doesn't come within its timeout, is `Unavailable`.
#[async_trait]
pub trait Connection: Send + Sync {
    async fn send(&self, request: Request) -> Result<Answer, TransportError>;
}

/// The agent's [`Connection`]: reqwest, with the TLS, proxy, redirect and timeout rules in the module docs.
#[derive(Debug, Clone)]
pub struct Reqwest(Client);

impl Reqwest {
    pub fn new(server: &ServerUrl) -> Result<Self, TransportError> {
        Self::build(server, None)
    }

    /// [`Reqwest::new`], except that HTTPS trusts only `roots` (DER certificates), not the platform's certificate
    /// store: for tests of the TLS path against a server with a test certificate. The agent itself never calls it.
    pub fn trusting_only(server: &ServerUrl, roots: &[&[u8]]) -> Result<Self, TransportError> {
        let certs = roots
            .iter()
            .map(|der| reqwest::Certificate::from_der(der))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| TransportError::Protocol(format!("a trusted certificate can't be read: {e}")))?;
        Self::build(server, Some(certs))
    }

    fn build(server: &ServerUrl, only: Option<Vec<reqwest::Certificate>>) -> Result<Self, TransportError> {
        // No proxy: `HTTP_PROXY`/`ALL_PROXY` would carry the plain-HTTP loopback requests (token, password) off the
        // machine. Proxy support for HTTPS comes later, if asked for.
        let mut builder = Client::builder().no_proxy();
        if server.0.host_str() == Some("localhost") {
            builder = builder.resolve_to_addrs("localhost", &LOCALHOST);
        }
        if let Some(roots) = only {
            builder = builder.tls_certs_only(roots);
        }
        let client = builder
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .https_only(!server.is_loopback())
            .build()
            .map_err(|e| TransportError::Protocol(format!("can't set up HTTPS: {e}")))?;
        Ok(Self(client))
    }
}

impl Reqwest {
    /// Sends `request`, keeping reqwest's own error (so its cause can be looked at).
    async fn send_reqwest(&self, request: Request) -> Result<Answer, reqwest::Error> {
        let mut req = self.0.request(request.method, request.url).headers(request.headers).timeout(request.timeout);
        if let Some(body) = request.body {
            req = req.body(body);
        }
        let resp = req.send().await?;
        Ok(Answer { status: resp.status(), headers: resp.headers().clone(), body: Box::new(ReqwestBody(resp)) })
    }
}

#[async_trait]
impl Connection for Reqwest {
    async fn send(&self, request: Request) -> Result<Answer, TransportError> {
        self.send_reqwest(request).await.map_err(network)
    }
}

/// Whether `error` (or anything that caused it) is rustls refusing the server's certificate: one this computer's trust
/// store doesn't vouch for (a server with its own certificate authority whose root isn't installed here, say).
pub fn untrusted_certificate(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut cause = Some(error);
    while let Some(e) = cause {
        if let Some(tls) = e.downcast_ref::<rustls::Error>()
            && matches!(tls, rustls::Error::InvalidCertificate(_) | rustls::Error::NoCertificatesPresented)
        {
            return true;
        }
        // An I/O error's own `source` skips the error it wraps.
        if let Some(inner) = e.downcast_ref::<std::io::Error>().and_then(std::io::Error::get_ref)
            && untrusted_certificate(inner)
        {
            return true;
        }
        cause = e.source();
    }
    false
}

/// Why [`reach`] couldn't ask the server what it speaks.
#[derive(Debug, thiserror::Error)]
pub enum ReachError {
    /// The server's certificate isn't one this computer trusts.
    #[error("the server's certificate isn't trusted on this computer")]
    UntrustedCertificate,
    #[error(transparent)]
    Transport(#[from] TransportError),
}

/// [`info`], telling an untrusted certificate apart from the other failures: what a sign-in or join asks first, so
/// the user hears which.
pub async fn reach(server: &ServerUrl) -> Result<InfoResponse, ReachError> {
    let connection = Reqwest::new(server)?;
    let req = request(Method::GET, server.endpoint("info"), None, 0);
    let answer = connection
        .send_reqwest(req)
        .await
        .map_err(|e| if untrusted_certificate(&e) { ReachError::UntrustedCertificate } else { ReachError::Transport(network(e)) })?;
    Ok(json(answer, MAX_SMALL_JSON_BYTES).await?)
}

struct ReqwestBody(reqwest::Response);

#[async_trait]
impl AnswerBody for ReqwestBody {
    async fn next(&mut self) -> Result<Option<Bytes>, TransportError> {
        self.0.chunk().await.map_err(network)
    }
}

/// The whole-request timeout for a body of up to `bytes` bytes either way.
fn timeout_for(bytes: usize) -> Duration {
    let mib = (bytes as u64).div_ceil(1024 * 1024);
    Duration::from_secs(BASE_TIMEOUT_SECS.saturating_add(mib.saturating_mul(SECS_PER_MIB)))
}

/// A network failure: try again later. Only a body that arrived but doesn't parse is a protocol error.
fn network(e: reqwest::Error) -> TransportError {
    if e.is_decode() { TransportError::Protocol(format!("the answer couldn't be read: {e}")) } else { TransportError::Unavailable(e.to_string()) }
}

/// A request to `url` with the headers every request carries: the User-Agent and, when signed in, the token.
fn request(method: Method, url: Url, auth: Option<&HeaderValue>, max_body: usize) -> Request {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(AGENT));
    if let Some(auth) = auth {
        headers.insert(AUTHORIZATION, auth.clone());
    }
    Request { method, url, headers, body: None, timeout: timeout_for(max_body) }
}

impl Request {
    /// The request with `body` as its JSON body.
    fn json<T: Serialize>(mut self, body: &T) -> Result<Self, TransportError> {
        let bytes = serde_json::to_vec(body).map_err(|e| TransportError::Protocol(format!("the request can't be written: {e}")))?;
        self.headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        self.body = Some(Bytes::from(bytes));
        Ok(self)
    }

    /// The request with `data` as its binary body.
    fn octets(mut self, data: Bytes) -> Self {
        self.headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
        self.body = Some(data);
        self
    }
}

/// A server message cut to a sane length, without control characters.
fn clean(message: &str) -> String {
    let mut out: String = message.chars().filter(|c| !c.is_control()).take(MAX_MESSAGE_CHARS).collect();
    if message.chars().count() > MAX_MESSAGE_CHARS {
        out.push('…');
    }
    out
}

/// Reads a body of at most `max` bytes; a longer one is a protocol error and is not buffered.
async fn body_capped(mut resp: Answer, max: usize) -> Result<Bytes, TransportError> {
    let too_long = || TransportError::Protocol(format!("the server's answer is over the limit of {max} bytes"));
    let declared = resp.headers.get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|s| s.parse::<u64>().ok());
    if declared.is_some_and(|len| len > max as u64) {
        return Err(too_long());
    }
    let mut buf = BytesMut::with_capacity(declared.map_or(0, |len| usize::try_from(len).unwrap_or(max).min(max)));
    while let Some(piece) = resp.body.next().await? {
        if buf.len().saturating_add(piece.len()) > max {
            return Err(too_long());
        }
        buf.extend_from_slice(&piece);
    }
    Ok(buf.freeze())
}

/// Maps an error answer onto [`TransportError`]; see the module docs.
fn error_for(status: StatusCode, body: Option<ErrorBody>) -> TransportError {
    let message = body.as_ref().map_or_else(|| status.to_string(), |b| clean(&b.message));
    let code = body.as_ref().map(|b| b.error.as_str());
    match (code, body.as_ref()) {
        (Some(codes::CONFLICT), Some(ErrorBody { conflicts: Some(conflicts), .. })) if conflicts.len() <= MAX_BATCH_OPS => {
            return TransportError::Rejected(conflicts.clone());
        }
        (Some(codes::MISSING_CHUNKS), Some(ErrorBody { missing: Some(missing), .. })) if missing.len() <= MAX_MISSING_IDS => {
            return TransportError::MissingChunks(missing.clone());
        }
        (Some(codes::RESYNC_REQUIRED), _) => return TransportError::ResyncRequired,
        (Some(codes::READ_ONLY), _) => return TransportError::ReadOnly,
        (Some(codes::QUOTA_EXCEEDED), Some(ErrorBody { quota: Some(quota), .. })) => {
            return TransportError::QuotaExceeded { used: quota.used, limit: quota.limit };
        }
        _ => {}
    }
    match status {
        StatusCode::UNAUTHORIZED => TransportError::Unauthorized,
        StatusCode::NOT_FOUND => TransportError::NotFound,
        StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS => TransportError::Unavailable(format!("the server is busy: {message}")),
        s if s.is_server_error() => TransportError::Unavailable(format!("{status}: {message}")),
        _ => TransportError::Protocol(format!("the server or a proxy refused the request (HTTP {}): {message}", status.as_u16())),
    }
}

/// The answer if it succeeded, else its error.
async fn checked(resp: Answer) -> Result<Answer, TransportError> {
    let status = resp.status;
    if status.is_success() {
        return Ok(resp);
    }
    let body = body_capped(resp, MAX_SMALL_JSON_BYTES).await.ok().and_then(|b| serde_json::from_slice::<ErrorBody>(&b).ok());
    Err(error_for(status, body))
}

async fn json<T: DeserializeOwned>(resp: Answer, max: usize) -> Result<T, TransportError> {
    let bytes = body_capped(checked(resp).await?, max).await?;
    serde_json::from_slice(&bytes).map_err(|e| TransportError::Protocol(format!("the server's answer doesn't parse: {e}")))
}

/// What the server speaks. No sign-in needed.
pub async fn info(server: &ServerUrl) -> Result<InfoResponse, TransportError> {
    info_over(&Reqwest::new(server)?, server).await
}

/// [`info`] over another connection (a simulated network, in tests).
pub async fn info_over<C: Connection>(connection: &C, server: &ServerUrl) -> Result<InfoResponse, TransportError> {
    let req = request(Method::GET, server.endpoint("info"), None, 0);
    json(connection.send(req).await?, MAX_SMALL_JSON_BYTES).await
}

/// Checks that the server speaks a protocol version and chunker this agent does.
pub fn check_compatible(info: &InfoResponse) -> Result<(), TransportError> {
    if info.protocol_max < PROTOCOL_MIN || info.protocol_min > PROTOCOL_MAX {
        return Err(TransportError::Protocol(format!(
            "the server speaks protocol {}-{}, this agent {PROTOCOL_MIN}-{PROTOCOL_MAX}: update the older one",
            info.protocol_min, info.protocol_max
        )));
    }
    if !info.chunker_params.iter().any(|p| p == "fastcdc-v1") {
        return Err(TransportError::Protocol("the server doesn't accept this agent's chunker (fastcdc-v1)".into()));
    }
    Ok(())
}

/// The most bytes the server takes of a device's `platform` or `app_version`.
const MAX_DEVICE_DETAIL_BYTES: usize = 32;

/// What this agent says about itself when it signs in or joins: `agent`, the operating system and the crate's
/// version, for the account's device list (display only). The server takes at most 32 bytes without control
/// characters per field, so a longer value is cut at a character boundary rather than refused.
fn device_details() -> (Option<String>, Option<String>, Option<String>) {
    fn limited(text: &str) -> String {
        text.chars()
            .filter(|c| !c.is_control())
            .scan(0usize, |len, c| {
                *len += c.len_utf8();
                (*len <= MAX_DEVICE_DETAIL_BYTES).then_some(c)
            })
            .collect()
    }
    (Some("agent".to_owned()), Some(limited(std::env::consts::OS)), Some(limited(env!("CARGO_PKG_VERSION"))))
}

/// Signs in and creates a device. `Unauthorized` means a wrong username or password.
pub async fn login(server: &ServerUrl, username: &str, password: &str, device_name: &str) -> Result<LoginResponse, TransportError> {
    login_over(&Reqwest::new(server)?, server, username, password, device_name).await
}

/// [`login`] over another connection (a simulated network, in tests).
pub async fn login_over<C: Connection>(
    connection: &C,
    server: &ServerUrl,
    username: &str,
    password: &str,
    device_name: &str,
) -> Result<LoginResponse, TransportError> {
    let (client_kind, platform, app_version) = device_details();
    let body = LoginRequest {
        username: username.to_owned(),
        password: password.to_owned(),
        device_name: device_name.to_owned(),
        client_kind,
        platform,
        app_version,
    };
    let req = request(Method::POST, server.endpoint("auth/login"), None, 0).json(&body)?;
    json(connection.send(req).await?, MAX_SMALL_JSON_BYTES).await
}

/// A server's refusal of a call that the agent keeps whole (its status, code and message, the challenge of
/// `two_factor_required`, the `Retry-After` of a lock), unlike [`TransportError`], which sync relies on and which has no
/// room for them. Only the calls that need it return this ([`CallError`]); the sync paths, [`error_for`] and
/// `TransportError` are as they were.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub error: String,
    pub message: String,
    pub two_factor: Option<TwoFactorChallenge>,
    pub retry_after_secs: Option<u64>,
}

/// Why a call that returns a [`Refusal`] failed: the server refused it (a wrong password or code, a conflict, a lock: never a
/// reason to think this device was revoked), or it failed as any call does (offline, a 401 on a signed-in call, a 404).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CallError {
    #[error("{}", .0.message)]
    Refused(Refusal),
    #[error(transparent)]
    Transport(TransportError),
}

impl From<TransportError> for CallError {
    fn from(e: TransportError) -> Self {
        CallError::Transport(e)
    }
}

/// The server's `Retry-After` in seconds, cut to a sane size.
fn retry_after_of(headers: &HeaderMap) -> Option<u64> {
    headers.get(RETRY_AFTER).and_then(|v| v.to_str().ok()).and_then(|s| s.trim().parse::<u64>().ok()).map(|secs| secs.min(24 * 60 * 60))
}

/// A failed answer as a [`CallError`]: a 400, 403, 409, 422 or 429 with an error body, a 401 that is `two_factor_required` or
/// `two_factor_invalid` (the not-signed-in routes' answers; on a signed-in route a 401 is only ever a revoked device) and a
/// 503 `two_factor_unavailable` are a [`Refusal`]; everything else is what [`error_for`] makes of it.
async fn failure(resp: Answer) -> CallError {
    let status = resp.status;
    let retry = retry_after_of(&resp.headers);
    let body = body_capped(resp, MAX_SMALL_JSON_BYTES).await.ok().and_then(|b| serde_json::from_slice::<ErrorBody>(&b).ok());
    if let Some(b) = &body {
        let refused = match status.as_u16() {
            400 | 403 | 409 | 422 | 429 => true,
            401 => matches!(b.error.as_str(), codes::TWO_FACTOR_REQUIRED | codes::TWO_FACTOR_INVALID),
            503 => b.error == codes::TWO_FACTOR_UNAVAILABLE,
            _ => false,
        };
        if refused {
            return CallError::Refused(Refusal {
                status: status.as_u16(),
                error: b.error.clone(),
                message: clean(&b.message),
                two_factor: b.two_factor.clone(),
                retry_after_secs: retry,
            });
        }
    }
    CallError::Transport(error_for(status, body))
}

/// The second factor a person gave: a code from the authenticator app, or a recovery code. Its `Debug` output hides both.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Factor {
    pub totp_code: Option<String>,
    pub recovery_code: Option<String>,
}

impl Factor {
    /// Reads what a person typed: six digits (spaces allowed) are a code, anything else a recovery code.
    pub fn typed(text: &str) -> Self {
        let trimmed = text.trim();
        let digits: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
        if digits.len() == 6 && digits.bytes().all(|b| b.is_ascii_digit()) {
            Self { totp_code: Some(digits), recovery_code: None }
        } else {
            Self { totp_code: None, recovery_code: Some(trimmed.to_owned()) }
        }
    }
}

impl std::fmt::Debug for Factor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Factor")
            .field("totp_code", &self.totp_code.as_ref().map(|_| "<redacted>"))
            .field("recovery_code", &self.recovery_code.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// The first step of a sign-in: a device, or the challenge of an account that has two-factor sign-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginStep {
    Device(LoginResponse),
    SecondFactor(TwoFactorChallenge),
}

/// Signs in as [`login`] does, but tells the account's second factor from a wrong password: `Unauthorized` (a wrong username
/// or password) is [`CallError::Transport`], a challenge is [`LoginStep::SecondFactor`].
pub async fn login_step(server: &ServerUrl, username: &str, password: &str, device_name: &str) -> Result<LoginStep, CallError> {
    login_step_over(&Reqwest::new(server)?, server, username, password, device_name).await
}

/// [`login_step`] over another connection (a simulated network, in tests).
pub async fn login_step_over<C: Connection>(
    connection: &C,
    server: &ServerUrl,
    username: &str,
    password: &str,
    device_name: &str,
) -> Result<LoginStep, CallError> {
    let (client_kind, platform, app_version) = device_details();
    let body = LoginRequest {
        username: username.to_owned(),
        password: password.to_owned(),
        device_name: device_name.to_owned(),
        client_kind,
        platform,
        app_version,
    };
    let req = request(Method::POST, server.endpoint("auth/login"), None, 0).json(&body)?;
    let resp = connection.send(req).await?;
    if resp.status.is_success() {
        return Ok(LoginStep::Device(json(resp, MAX_SMALL_JSON_BYTES).await?));
    }
    match failure(resp).await {
        CallError::Refused(Refusal { error, two_factor, .. }) if error == codes::TWO_FACTOR_REQUIRED => match two_factor {
            Some(challenge) => Ok(LoginStep::SecondFactor(challenge)),
            None => Err(CallError::Transport(TransportError::Protocol("the server asked for a second factor without a challenge".into()))),
        },
        other => Err(other),
    }
}

/// The second step of a sign-in: the `challenge` and the second factor make the device. A challenge that doesn't work any
/// more (unknown, expired, used, from before a password change) is `Transport(Unauthorized)` here, which is no revoked device
/// (the caller says the sign-in has to start again); a wrong code is `Refused` with `two_factor_invalid`, a lock with
/// `two_factor_locked` and its wait.
pub async fn second_factor(server: &ServerUrl, challenge: &str, factor: &Factor) -> Result<LoginResponse, CallError> {
    second_factor_over(&Reqwest::new(server)?, server, challenge, factor).await
}

/// [`second_factor`] over another connection (a simulated network, in tests).
pub async fn second_factor_over<C: Connection>(
    connection: &C,
    server: &ServerUrl,
    challenge: &str,
    factor: &Factor,
) -> Result<LoginResponse, CallError> {
    let body =
        SecondFactorRequest { challenge: challenge.to_owned(), totp_code: factor.totp_code.clone(), recovery_code: factor.recovery_code.clone() };
    let req = request(Method::POST, server.endpoint("auth/login/second-factor"), None, 0).json(&body)?;
    let resp = connection.send(req).await?;
    if resp.status.is_success() {
        return Ok(json(resp, MAX_SMALL_JSON_BYTES).await?);
    }
    Err(failure(resp).await)
}

/// Why a join ([`join`]) was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JoinError {
    /// The code is wrong, expired or used (the server never says which).
    #[error("the code doesn't work")]
    InvalidCode,
    /// An invite to a new account: send it again with a password.
    #[error("the invite needs a password for the new account")]
    PasswordRequired,
    /// The new password breaks the server's rules; the server's message says which.
    #[error("{0}")]
    WeakPassword(String),
    /// Too many failed joins from this address; the server's message.
    #[error("{0}")]
    RateLimited(String),
    /// The account has two-factor sign-in: join again with a code from its authenticator app or a recovery code. The code
    /// was not used up.
    #[error("the account has two-factor sign-in: a code from its authenticator app is needed")]
    TwoFactorRequired,
    /// The code or recovery code was wrong (the join code still works); the server's message.
    #[error("{0}")]
    TwoFactorInvalid(String),
    /// Too many wrong codes: the second factor is locked for `retry_after_secs`.
    #[error("{message}")]
    TwoFactorLocked { message: String, retry_after_secs: Option<u64> },
    #[error(transparent)]
    Transport(TransportError),
}

/// Redeems a join code (normalised) for a new device named `device_name`; `password` for an invite to a new account.
pub async fn join(server: &ServerUrl, code: &str, device_name: &str, password: Option<&str>) -> Result<LoginResponse, JoinError> {
    join_with_factor(server, code, device_name, password, None).await
}

/// [`join`] with the second factor of an account that has two-factor sign-in.
pub async fn join_with_factor(
    server: &ServerUrl,
    code: &str,
    device_name: &str,
    password: Option<&str>,
    factor: Option<&Factor>,
) -> Result<LoginResponse, JoinError> {
    join_over_with(&Reqwest::new(server).map_err(JoinError::Transport)?, server, code, device_name, password, factor).await
}

/// [`join`] over another connection (a simulated network, in tests).
pub async fn join_over<C: Connection>(
    connection: &C,
    server: &ServerUrl,
    code: &str,
    device_name: &str,
    password: Option<&str>,
) -> Result<LoginResponse, JoinError> {
    join_over_with(connection, server, code, device_name, password, None).await
}

/// [`join_over`] with a second factor.
pub async fn join_over_with<C: Connection>(
    connection: &C,
    server: &ServerUrl,
    code: &str,
    device_name: &str,
    password: Option<&str>,
    factor: Option<&Factor>,
) -> Result<LoginResponse, JoinError> {
    let (client_kind, platform, app_version) = device_details();
    let body = JoinRequest {
        code: code.to_owned(),
        device_name: device_name.to_owned(),
        password: password.map(str::to_owned),
        totp_code: factor.and_then(|f| f.totp_code.clone()),
        recovery_code: factor.and_then(|f| f.recovery_code.clone()),
        client_kind,
        platform,
        app_version,
    };
    let req = request(Method::POST, server.endpoint("join"), None, 0).json(&body).map_err(JoinError::Transport)?;
    let resp = connection.send(req).await.map_err(JoinError::Transport)?;
    if resp.status.is_success() {
        return json(resp, MAX_SMALL_JSON_BYTES).await.map_err(JoinError::Transport);
    }
    let status = resp.status;
    let retry = retry_after_of(&resp.headers);
    let body = body_capped(resp, MAX_SMALL_JSON_BYTES).await.ok().and_then(|b| serde_json::from_slice::<ErrorBody>(&b).ok());
    Err(match body.as_ref().map(|b| b.error.as_str()) {
        Some(codes::TWO_FACTOR_REQUIRED) => JoinError::TwoFactorRequired,
        Some(codes::TWO_FACTOR_INVALID) => JoinError::TwoFactorInvalid(body.as_ref().map(|b| clean(&b.message)).unwrap_or_default()),
        Some(codes::TWO_FACTOR_LOCKED) => {
            JoinError::TwoFactorLocked { message: body.as_ref().map(|b| clean(&b.message)).unwrap_or_default(), retry_after_secs: retry }
        }
        Some(codes::INVALID_CODE) => JoinError::InvalidCode,
        Some(codes::PASSWORD_REQUIRED) => JoinError::PasswordRequired,
        Some(codes::WEAK_PASSWORD) => JoinError::WeakPassword(body.as_ref().map(|b| clean(&b.message)).unwrap_or_default()),
        Some(codes::RATE_LIMITED) => JoinError::RateLimited(body.as_ref().map(|b| clean(&b.message)).unwrap_or_default()),
        _ => JoinError::Transport(error_for(status, body)),
    })
}

/// Longest `Retry-After` the long-poll honours; a bigger one is cut to this.
const MAX_RETRY_AFTER_SECS: u64 = 300;

/// Why a long-poll ([`HttpTransport::wait_for_changes`]) brought no sequence number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollError {
    /// Too many requests wait on this space (429): poll again after this long (the server's `Retry-After`).
    Busy(Option<Duration>),
    /// The feed can't continue from `since` (the server was restored or the agent's position is too old): a pass
    /// sorts it out.
    Resync,
    /// Anything else (offline, a server error, an answer that doesn't parse).
    Failed(TransportError),
}

/// The server API for one signed-in device, over a [`Connection`] (by default the agent's own, [`Reqwest`]).
#[derive(Debug, Clone)]
pub struct HttpTransport<C = Reqwest> {
    connection: C,
    server: ServerUrl,
    auth: HeaderValue,
    /// The team space every request works on (`craft-space`); `None`: the account's personal space, as before. Every
    /// answer that the server's space layer gives must echo it ([`TOO_OLD_FOR_TEAM_SPACES`]).
    space: Option<HeaderValue>,
}

/// What the agent says when a server answers a request for a team space without echoing the space, or with another one:
/// it is a server from before team spaces (it ignored the `craft-space` header and answered about the personal space).
/// The space stops syncing with this message, so two trees are never mixed.
pub const TOO_OLD_FOR_TEAM_SPACES: &str = "the server is too old for team spaces; ask the administrator to update it";

/// Whether an answer to a request for team space `asked` carries the echo the server's space layer adds to every answer
/// it produces (`craft-space` with the space it worked on). Checked on the answers that the layer or the handler behind it
/// gives (success, `403`, `404`, `409`, `410`); a `400`, `401` or `429` can come from before the layer (or from a proxy in
/// front of the server) and carries none.
fn echoed(asked: &HeaderValue, answer: &Answer) -> Result<(), TransportError> {
    let layered = answer.status.is_success() || matches!(answer.status.as_u16(), 403 | 404 | 409 | 410);
    if !layered {
        return Ok(());
    }
    match answer.headers.get(HeaderName::from_static("craft-space")) {
        Some(echo) if echo.as_bytes().eq_ignore_ascii_case(asked.as_bytes()) => Ok(()),
        _ => Err(TransportError::Protocol(TOO_OLD_FOR_TEAM_SPACES.into())),
    }
}

/// A history or restore refusal, retaining the server's HTTP status and stable error code.
#[derive(Debug)]
pub enum HistoryCallError {
    Transport(TransportError),
    Refused { status: StatusCode, body: Box<ErrorBody> },
}

async fn history_answer<T: DeserializeOwned>(answer: Answer) -> Result<T, HistoryCallError> {
    if !answer.status.is_success() {
        let status = answer.status;
        let body = body_capped(answer, MAX_SMALL_JSON_BYTES)
            .await
            .ok()
            .and_then(|bytes| serde_json::from_slice::<ErrorBody>(&bytes).ok())
            .unwrap_or_else(|| ErrorBody { error: codes::INTERNAL.into(), message: status.to_string(), ..ErrorBody::default() });
        return Err(HistoryCallError::Refused { status, body: Box::new(body) });
    }
    let bytes = body_capped(answer, MAX_SMALL_JSON_BYTES).await.map_err(HistoryCallError::Transport)?;
    serde_json::from_slice(&bytes)
        .map_err(|e| HistoryCallError::Transport(TransportError::Protocol(format!("the server's answer doesn't parse: {e}"))))
}

impl HttpTransport {
    pub fn new(server: ServerUrl, token: &DeviceToken) -> Result<Self, TransportError> {
        Self::over(Reqwest::new(&server)?, server, token)
    }
}

impl<C: Connection> HttpTransport<C> {
    /// The API over another connection (a simulated network, in tests). `server` names the endpoints.
    pub fn over(connection: C, server: ServerUrl, token: &DeviceToken) -> Result<Self, TransportError> {
        Ok(Self { connection, server, auth: token.bearer()?, space: None })
    }

    /// The same API for one space: every request names it in the `craft-space` header. Without it, the account's personal
    /// space, which is also what a server from before team spaces answers about.
    pub fn bound_to(mut self, space: SpaceId) -> Result<Self, TransportError> {
        let value = HeaderValue::from_str(&space.to_string()).map_err(|_| TransportError::Protocol("a space ID that can't be sent".into()))?;
        self.space = Some(value);
        Ok(self)
    }

    pub fn server(&self) -> &ServerUrl {
        &self.server
    }

    /// The server's newest-first history for one file node.
    pub async fn file_history(&self, node: NodeId, cursor: Option<&str>, limit: Option<u32>) -> Result<HistoryResponse, HistoryCallError> {
        let mut url = self.server.endpoint(&format!("nodes/{node}/history"));
        {
            let mut query = url.query_pairs_mut();
            if let Some(cursor) = cursor {
                query.append_pair("cursor", cursor);
            }
            if let Some(limit) = limit {
                query.append_pair("limit", &limit.to_string());
            }
        }
        let req = request(Method::GET, url, Some(&self.auth), MAX_SMALL_JSON_BYTES);
        let answer = self.send(req).await.map_err(HistoryCallError::Transport)?;
        history_answer(answer).await
    }

    /// Makes a prior version the file's content as a new version.
    pub async fn restore_file(&self, node: NodeId, version: VersionId, expected_version: VersionId) -> Result<RestoreResponse, HistoryCallError> {
        let req = self
            .request(Method::POST, &format!("nodes/{node}/restore"), MAX_SMALL_JSON_BYTES)
            .json(&RestoreRequest { version, expected_version })
            .map_err(HistoryCallError::Transport)?;
        let answer = self.send(req).await.map_err(HistoryCallError::Transport)?;
        history_answer(answer).await
    }

    /// The server's newest-first trash page.
    pub async fn trash(&self, cursor: Option<&str>, limit: Option<u32>) -> Result<craft_cloud_proto::api::TrashResponse, HistoryCallError> {
        let mut url = self.server.endpoint("trash");
        {
            let mut query = url.query_pairs_mut();
            if let Some(cursor) = cursor {
                query.append_pair("cursor", cursor);
            }
            if let Some(limit) = limit {
                query.append_pair("limit", &limit.to_string());
            }
        }
        let req = request(Method::GET, url, Some(&self.auth), MAX_SMALL_JSON_BYTES);
        let answer = self.send(req).await.map_err(HistoryCallError::Transport)?;
        history_answer(answer).await
    }

    /// Restores a deleted node from the server's trash.
    pub async fn restore_from_trash(&self, node: NodeId) -> Result<craft_cloud_proto::api::TrashRestoreResponse, HistoryCallError> {
        let req = self.request(Method::POST, &format!("trash/{node}/restore"), MAX_SMALL_JSON_BYTES);
        let answer = self.send(req).await.map_err(HistoryCallError::Transport)?;
        history_answer(answer).await
    }

    /// Marks a deleted node out of the server's trash. This does not free stored data yet.
    pub async fn remove_from_trash(&self, node: NodeId) -> Result<(), HistoryCallError> {
        let req = self.request(Method::DELETE, &format!("trash/{node}"), MAX_SMALL_JSON_BYTES);
        let answer = self.send(req).await.map_err(HistoryCallError::Transport)?;
        if answer.status.is_success() {
            return Ok(());
        }
        let status = answer.status;
        let body = body_capped(answer, MAX_SMALL_JSON_BYTES)
            .await
            .ok()
            .and_then(|bytes| serde_json::from_slice::<ErrorBody>(&bytes).ok())
            .unwrap_or_else(|| ErrorBody { error: codes::INTERNAL.into(), message: status.to_string(), ..ErrorBody::default() });
        Err(HistoryCallError::Refused { status, body: Box::new(body) })
    }

    fn request(&self, method: Method, path: &str, max_body: usize) -> Request {
        request(method, self.server.endpoint(path), Some(&self.auth), max_body)
    }

    async fn send(&self, mut req: Request) -> Result<Answer, TransportError> {
        if let Some(space) = &self.space {
            req.headers.insert(HeaderName::from_static("craft-space"), space.clone());
        }
        let answer = self.connection.send(req).await?;
        if let Some(space) = &self.space {
            echoed(space, &answer)?;
        }
        Ok(answer)
    }

    fn page_request(&self, path: &str, since: Option<u64>, after: Option<PageCursor>) -> Request {
        let mut url = self.server.endpoint(path);
        {
            let mut query = url.query_pairs_mut();
            if let Some(since) = since {
                query.append_pair("since", &since.to_string());
            }
            if let Some(after) = after {
                query.append_pair("after_seq", &after.seq.to_string());
                query.append_pair("after_node", &after.node.to_string());
            }
        }
        request(Method::GET, url, Some(&self.auth), MAX_PAGE_JSON_BYTES)
    }

    async fn page(&self, req: Request) -> Result<ChangeSet, TransportError> {
        let page: ChangeSet = json(self.send(req).await?, MAX_PAGE_JSON_BYTES).await?;
        if page.nodes.len() > MAX_PAGE_NODES as usize {
            return Err(TransportError::Protocol(format!("a page of {} nodes is over the limit of {MAX_PAGE_NODES}", page.nodes.len())));
        }
        Ok(page)
    }

    async fn no_content(&self, req: Request) -> Result<(), TransportError> {
        checked(self.send(req).await?).await?;
        Ok(())
    }

    /// Waits up to `wait` (the server holds at most 60 s) for the change feed to move past `since`, for the `run`
    /// loop; separate from the engine's paging, and only one node is asked for, since the loop only wants to know
    /// whether to run a pass. Returns the server's sequence number (equal to `since` when nothing changed).
    pub async fn wait_for_changes(&self, since: u64, wait: Duration) -> Result<u64, PollError> {
        let mut url = self.server.endpoint("changes");
        url.query_pairs_mut().append_pair("since", &since.to_string()).append_pair("limit", "1").append_pair("wait", &wait.as_secs().to_string());
        // The server answers within `wait`; the whole-request cap leaves room for a slow answer on top.
        let mut req = request(Method::GET, url, Some(&self.auth), MAX_SMALL_JSON_BYTES);
        req.timeout = wait.saturating_add(req.timeout);
        let resp = self.send(req).await.map_err(PollError::Failed)?;
        if resp.status == StatusCode::TOO_MANY_REQUESTS {
            let after = resp
                .headers
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<u64>().ok())
                .map(|secs| Duration::from_secs(secs.min(MAX_RETRY_AFTER_SECS)));
            return Err(PollError::Busy(after));
        }
        match json::<ChangeSet>(resp, MAX_SMALL_JSON_BYTES).await {
            Ok(page) => Ok(page.seq),
            Err(TransportError::ResyncRequired) => Err(PollError::Resync),
            Err(e) => Err(PollError::Failed(e)),
        }
    }

    /// A signed-in call whose refusals the agent keeps whole.
    async fn account_call<T: DeserializeOwned>(&self, req: Request) -> Result<T, CallError> {
        let resp = self.send(req).await?;
        if resp.status.is_success() {
            return Ok(json(resp, MAX_SMALL_JSON_BYTES).await?);
        }
        Err(failure(resp).await)
    }

    /// [`HttpTransport::account_call`] for an answer without a body.
    async fn account_unit(&self, req: Request) -> Result<(), CallError> {
        let resp = self.send(req).await?;
        if resp.status.is_success() {
            return Ok(());
        }
        Err(failure(resp).await)
    }

    /// Whether the account has two-factor sign-in on (`GET /account/two-factor`).
    pub async fn two_factor_status(&self) -> Result<TwoFactorStatus, CallError> {
        self.account_call(self.request(Method::GET, "account/two-factor", 0)).await
    }

    /// Starts a set-up with the account's password: the new secret, shown once.
    pub async fn two_factor_setup(&self, password: &str) -> Result<TwoFactorSetupResponse, CallError> {
        let req = self.request(Method::POST, "account/two-factor/setup", 0).json(&TwoFactorSetupRequest { password: password.to_owned() })?;
        self.account_call(req).await
    }

    /// Turns it on with the first code: the recovery codes, shown once.
    pub async fn two_factor_confirm(&self, totp_code: &str) -> Result<RecoveryCodes, CallError> {
        let req = self.request(Method::POST, "account/two-factor/confirm", 0).json(&TwoFactorConfirmRequest { totp_code: totp_code.to_owned() })?;
        self.account_call(req).await
    }

    /// Turns it off: the password and a code or recovery code.
    pub async fn two_factor_disable(&self, body: &TwoFactorProofRequest) -> Result<(), CallError> {
        self.account_unit(self.request(Method::POST, "account/two-factor/disable", 0).json(body)?).await
    }

    /// New recovery codes: the password and a code from the authenticator app.
    pub async fn two_factor_recovery_codes(&self, body: &TwoFactorProofRequest) -> Result<RecoveryCodes, CallError> {
        self.account_call(self.request(Method::POST, "account/two-factor/recovery-codes", 0).json(body)?).await
    }

    /// Changes the account's password.
    pub async fn change_password(&self, body: &ChangePasswordRequest) -> Result<(), CallError> {
        self.account_unit(self.request(Method::POST, "account/password", 0).json(body)?).await
    }

    /// Signs every other device of the account out.
    pub async fn revoke_other_devices(&self) -> Result<RevokedOthers, CallError> {
        self.account_call(self.request(Method::POST, "devices/revoke-others", 0)).await
    }

    /// Renames one of the account's devices.
    pub async fn rename_device(&self, device: DeviceId, name: &str) -> Result<DeviceInfo, CallError> {
        let req = self.request(Method::PATCH, &format!("devices/{device}"), 0).json(&RenameDeviceRequest { name: name.to_owned() })?;
        self.account_call(req).await
    }

    /// Revokes a device of this account (this one, to sign out).
    pub async fn revoke(&self, device: DeviceId) -> Result<(), TransportError> {
        self.no_content(self.request(Method::DELETE, &format!("devices/{device}"), 0)).await
    }

    /// `GET /spaces`: the personal space and the team spaces of the account, invitations among them. A server from before
    /// team spaces answers `404` here (a refusal with that status, or `NotFound`).
    pub async fn spaces(&self) -> Result<SpaceList, CallError> {
        self.account_call(self.request(Method::GET, "spaces", MAX_SMALL_JSON_BYTES)).await
    }

    /// `GET /spaces/{id}/members`: the members of a team space (invited accounts among them, marked by their state).
    pub async fn space_members(&self, space: SpaceId) -> Result<craft_cloud_proto::api::SpaceMembers, CallError> {
        self.account_call(self.request(Method::GET, &format!("spaces/{space}/members"), MAX_SMALL_JSON_BYTES)).await
    }

    /// `POST /spaces/{id}/accept`: the space as `GET /spaces` lists it.
    pub async fn accept_space(&self, space: SpaceId) -> Result<SpaceListItem, CallError> {
        self.account_call(self.request(Method::POST, &format!("spaces/{space}/accept"), MAX_SMALL_JSON_BYTES)).await
    }

    /// `POST /spaces/{id}/decline`.
    pub async fn decline_space(&self, space: SpaceId) -> Result<(), CallError> {
        self.account_unit(self.request(Method::POST, &format!("spaces/{space}/decline"), MAX_SMALL_JSON_BYTES)).await
    }

    /// The account's devices, this one marked `current`.
    pub async fn devices(&self) -> Result<Vec<DeviceInfo>, TransportError> {
        let list: Vec<DeviceInfo> = json(self.send(self.request(Method::GET, "devices", MAX_SMALL_JSON_BYTES)).await?, MAX_SMALL_JSON_BYTES).await?;
        if list.len() > MAX_DEVICES {
            return Err(TransportError::Protocol(format!("the server listed {} devices, more than the {MAX_DEVICES} this agent reads", list.len())));
        }
        Ok(list)
    }

    /// The account's stored bytes and labeled estimates, fetched without caching.
    pub async fn usage(&self) -> Result<craft_cloud_proto::api::AccountUsage, TransportError> {
        json(self.send(self.request(Method::GET, "account/usage", MAX_SMALL_JSON_BYTES)).await?, MAX_SMALL_JSON_BYTES).await
    }

    /// A new add-a-computer code from this device (replacing its open one).
    pub async fn create_pairing(&self) -> Result<PairingCreated, TransportError> {
        let created: PairingCreated = json(self.send(self.request(Method::POST, "pairings", 0)).await?, MAX_SMALL_JSON_BYTES).await?;
        if created.code.len() > MAX_CODE_BYTES {
            return Err(TransportError::Protocol("the server sent an add-a-computer code that is too long".into()));
        }
        Ok(created)
    }

    /// Where one of the account's add-a-computer codes stands (`NotFound` once it is gone).
    pub async fn pairing_status(&self, id: Uuid) -> Result<PairingStatus, TransportError> {
        let status: PairingStatus = json(self.send(self.request(Method::GET, &format!("pairings/{id}"), 0)).await?, MAX_SMALL_JSON_BYTES).await?;
        if status.device_name.as_ref().is_some_and(|name| name.len() > MAX_MACHINE_BYTES) {
            return Err(TransportError::Protocol("the server named a device with a name that is too long".into()));
        }
        Ok(status)
    }

    /// Cancels one of the account's add-a-computer codes.
    pub async fn delete_pairing(&self, id: Uuid) -> Result<(), TransportError> {
        self.no_content(self.request(Method::DELETE, &format!("pairings/{id}"), 0)).await
    }

    /// The signed-in account (`GET /account`): its role and whether it has two-factor sign-in.
    pub async fn account_info(&self) -> Result<AccountInfo, CallError> {
        self.account_call(self.request(Method::GET, "account", MAX_SMALL_JSON_BYTES)).await
    }

    /// The audit export (`GET /admin/audit/export?<query>`) as the server sends it: the good answer, whose body is read as it
    /// comes ([`AnswerBody`]), never held whole; a refusal is kept whole. The whole-request time is long (a big export over a
    /// slow link), the idle time is the usual minute.
    pub async fn admin_export_stream(&self, query: &[(&str, &str)]) -> Result<Answer, CallError> {
        let mut url = self.server.endpoint("admin/audit/export");
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        let mut req = request(Method::GET, url, Some(&self.auth), 0);
        req.timeout = EXPORT_TIMEOUT;
        let resp = self.send(req).await?;
        if resp.status.is_success() {
            return Ok(resp);
        }
        Err(failure(resp).await)
    }

    /// `GET /api/v1/<path>?<query>` of the admin API (or the account's activity), read as `T`; a refusal is kept whole.
    pub async fn admin_get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> Result<T, CallError> {
        let mut url = self.server.endpoint(path);
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        self.account_call(request(Method::GET, url, Some(&self.auth), MAX_SMALL_JSON_BYTES)).await
    }

    /// `method /api/v1/<path>` of the admin API with `body` (the confirmation among it), answered with a body read as `T`.
    pub async fn admin_json<T: DeserializeOwned, B: Serialize>(&self, method: Method, path: &str, body: &B) -> Result<T, CallError> {
        self.account_call(self.request(method, path, MAX_SMALL_JSON_BYTES).json(body)?).await
    }

    /// [`HttpTransport::admin_json`] for a call answered without a body (`204`, or `202`): the status it succeeded with.
    pub async fn admin_empty<B: Serialize>(&self, method: Method, path: &str, body: &B) -> Result<StatusCode, CallError> {
        let resp = self.send(self.request(method, path, MAX_SMALL_JSON_BYTES).json(body)?).await?;
        if resp.status.is_success() {
            return Ok(resp.status);
        }
        Err(failure(resp).await)
    }
}

#[async_trait]
impl<C: Connection> Transport for HttpTransport<C> {
    async fn space(&self) -> Result<SpaceResponse, TransportError> {
        json(self.send(self.request(Method::GET, "space", 0)).await?, MAX_SMALL_JSON_BYTES).await
    }

    async fn tree(&self, after: Option<PageCursor>) -> Result<ChangeSet, TransportError> {
        self.page(self.page_request("tree", None, after)).await
    }

    async fn changes(&self, since: u64, after: Option<PageCursor>) -> Result<ChangeSet, TransportError> {
        self.page(self.page_request("changes", Some(since), after)).await
    }

    async fn missing_chunks(&self, ids: &[ChunkId]) -> Result<Vec<ChunkId>, TransportError> {
        let mut missing = Vec::new();
        for batch in ids.chunks(MAX_MISSING_IDS) {
            let req = self.request(Method::POST, "chunks/missing", MAX_SMALL_JSON_BYTES).json(&MissingRequest { ids: batch.to_vec() })?;
            let answer: MissingResponse = json(self.send(req).await?, MAX_SMALL_JSON_BYTES).await?;
            if answer.missing.len() > batch.len() {
                return Err(TransportError::Protocol("the server named more missing chunks than were asked about".into()));
            }
            missing.extend(answer.missing);
        }
        Ok(missing)
    }

    async fn put_chunk(&self, id: &ChunkId, data: Bytes) -> Result<(), TransportError> {
        let req = self.request(Method::PUT, &format!("chunks/{id}"), data.len()).octets(data);
        self.no_content(req).await
    }

    async fn get_chunk(&self, id: &ChunkId) -> Result<Bytes, TransportError> {
        let resp = checked(self.send(self.request(Method::GET, &format!("chunks/{id}"), MAX_CHUNK_BYTES as usize)).await?).await?;
        body_capped(resp, MAX_CHUNK_BYTES as usize).await
    }

    async fn put_manifest(&self, id: &ManifestId, data: Bytes) -> Result<(), TransportError> {
        let req = self.request(Method::PUT, &format!("manifests/{id}"), data.len()).octets(data);
        self.no_content(req).await
    }

    async fn get_manifest(&self, id: &ManifestId) -> Result<Bytes, TransportError> {
        let resp = checked(self.send(self.request(Method::GET, &format!("manifests/{id}"), MAX_MANIFEST_BYTES)).await?).await?;
        body_capped(resp, MAX_MANIFEST_BYTES).await
    }

    async fn commit(&self, ops: &[Op]) -> Result<CommitResponse, TransportError> {
        let req = self.request(Method::POST, "commit", MAX_SMALL_JSON_BYTES).json(&CommitRequest { ops: ops.to_vec() })?;
        json(self.send(req).await?, MAX_SMALL_JSON_BYTES).await
    }

    async fn attach_save(&self, node: NodeId, version: VersionId, save: &SaveRecord) -> Result<(), TransportError> {
        let body = craft_cloud_proto::api::SaveAttach { node, version, save: save.clone() };
        let req = self.request(Method::POST, "saves", MAX_SMALL_JSON_BYTES).json(&body)?;
        let _: CommitResponse = json(self.send(req).await?, MAX_SMALL_JSON_BYTES).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use craft_cloud_proto::{Conflict, ConflictKind};

    #[test]
    fn a_second_factor_is_read_as_typed_and_hidden_in_debug() {
        assert_eq!(Factor::typed(" 123 456 "), Factor { totp_code: Some("123456".into()), recovery_code: None });
        assert_eq!(Factor::typed("abcd-efgh-jkmn-pqrs"), Factor { totp_code: None, recovery_code: Some("abcd-efgh-jkmn-pqrs".into()) });
        assert_eq!(Factor::typed("12345").recovery_code.as_deref(), Some("12345"), "five digits are not a code");
        let shown = format!("{:?}", Factor::typed("123456"));
        assert!(!shown.contains("123456"), "{shown}");
        let shown = format!(
            "{:?}",
            Refusal {
                status: 401,
                error: "x".into(),
                message: "m".into(),
                two_factor: Some(TwoFactorChallenge { challenge: "chal-secret".into(), expires_ms: 1 }),
                retry_after_secs: None
            }
        );
        assert!(!shown.contains("chal-secret"), "{shown}");
    }

    #[test]
    fn https_is_required_except_on_this_machine() {
        assert!(ServerUrl::parse("https://cloud.example.com").is_ok());
        assert_eq!(ServerUrl::parse("http://cloud.example.com"), Err(UrlError::NotHttps));
        assert_eq!(ServerUrl::parse("http://192.168.1.5:8080"), Err(UrlError::NotHttps));
        assert_eq!(ServerUrl::parse("ftp://cloud.example.com"), Err(UrlError::NotHttps));
        for local in ["http://127.0.0.1:8080", "http://127.1.2.3", "http://[::1]:8080", "http://localhost:8080", "http://LOCALHOST"] {
            assert!(ServerUrl::parse(local).is_ok(), "{local}");
        }
        assert_eq!(ServerUrl::parse("https://ana:pw@cloud.example.com"), Err(UrlError::Extras));
        assert_eq!(ServerUrl::parse("https://cloud.example.com/?x=1"), Err(UrlError::Extras));
        assert!(matches!(ServerUrl::parse("not a url"), Err(UrlError::Unparseable(_))));
    }

    #[test]
    fn addresses_are_normalised() {
        for (text, normal) in [
            ("https://cloud.example.com/", "https://cloud.example.com"),
            ("https://cloud.example.com/api/v1/", "https://cloud.example.com"),
            ("https://example.com/sync/", "https://example.com/sync"),
            ("https://example.com/sync/api/v1", "https://example.com/sync"),
            (" http://127.0.0.1:9000 ", "http://127.0.0.1:9000"),
        ] {
            let url = ServerUrl::parse(text).unwrap();
            assert_eq!(url.as_str(), normal, "{text}");
            assert_eq!(ServerUrl::parse(url.as_str()).unwrap(), url, "normalising is stable");
        }
        let url = ServerUrl::parse("https://example.com/sync").unwrap();
        assert_eq!(url.endpoint("chunks/missing").as_str(), "https://example.com/sync/api/v1/chunks/missing");
        let root = ServerUrl::parse("https://example.com").unwrap();
        assert_eq!(root.endpoint("space").as_str(), "https://example.com/api/v1/space");
        assert_eq!(url.live_url().as_str(), "wss://example.com/sync/api/v1/live");
        assert_eq!(ServerUrl::parse("http://127.0.0.1:9000").unwrap().live_url().as_str(), "ws://127.0.0.1:9000/api/v1/live");
    }

    #[test]
    fn tokens_stay_out_of_debug_output() {
        let token = DeviceToken::new("secret-token".into());
        assert!(!format!("{token:?}").contains("secret"));
        let transport = HttpTransport::new(ServerUrl::parse("http://127.0.0.1:1").unwrap(), &token).unwrap();
        assert!(!format!("{transport:?}").contains("secret-token"));
    }

    /// Review finding 7: a request's `Debug` output (one `{:?}` in a log line or an error) never shows its body or a
    /// header's value: a sign-in's body holds the password, the `Authorization` header the token.
    #[test]
    fn requests_keep_bodies_and_tokens_out_of_debug_output() {
        let server = ServerUrl::parse("http://127.0.0.1:1").unwrap();
        let body = LoginRequest {
            username: "ana".into(),
            password: "correct horse battery".into(),
            device_name: "laptop".into(),
            ..LoginRequest::default()
        };
        let login = request(Method::POST, server.endpoint("auth/login"), None, 0).json(&body).unwrap();
        let shown = format!("{login:?}");
        assert!(!shown.contains("correct horse") && !shown.contains("ana"), "{shown}");
        assert!(shown.contains("POST") && shown.contains("/api/v1/auth/login") && shown.contains("content-type"), "{shown}");
        let token = DeviceToken::new("secret-token".into());
        let transport = HttpTransport::new(server, &token).unwrap();
        let signed_in = transport.request(Method::GET, "space", 0);
        let shown = format!("{signed_in:?}");
        assert!(!shown.contains("secret-token") && shown.contains("authorization"), "{shown}");
    }

    /// An admin change carries the admin's password in its body, to the server only: no `Debug` output of the request
    /// or of the bodies shows it.
    #[test]
    fn admin_confirmations_stay_out_of_debug_output() {
        use craft_cloud_proto::api::{AdminConfirm, AdminCreateUser, AdminUpdateUser};
        let server = ServerUrl::parse("http://127.0.0.1:1").unwrap();
        let confirm = AdminConfirm { password: Some("admin-secret-pw".into()), totp_code: Some("918273".into()), ..AdminConfirm::default() };
        let create =
            AdminCreateUser { username: "ana".into(), role: "member".into(), password: Some("admin-secret-pw".into()), ..AdminCreateUser::default() };
        let update = AdminUpdateUser { password: Some("admin-secret-pw".into()), ..AdminUpdateUser::default() };
        let sent = request(Method::POST, server.endpoint("admin/users/x/disable"), None, 0).json(&confirm).unwrap();
        assert!(sent.body.as_deref().is_some_and(|b| String::from_utf8_lossy(b).contains("admin-secret-pw")), "the body carries it");
        let shown = format!("{sent:?} {confirm:?} {create:?} {update:?}");
        assert!(!shown.contains("admin-secret-pw") && !shown.contains("918273"), "{shown}");
    }

    fn body(code: &str) -> ErrorBody {
        ErrorBody { error: code.into(), message: "m".into(), ..ErrorBody::default() }
    }

    #[test]
    fn error_answers_map_by_code_then_status() {
        let conflict = Conflict { op: 0, kind: ConflictKind::NoSuchNode };
        let rejected = ErrorBody { conflicts: Some(vec![conflict.clone()]), ..body(codes::CONFLICT) };
        assert_eq!(error_for(StatusCode::CONFLICT, Some(rejected)), TransportError::Rejected(vec![conflict]));
        let missing = ErrorBody { missing: Some(vec![ChunkId::from_bytes([1; 32])]), ..body(codes::MISSING_CHUNKS) };
        assert_eq!(error_for(StatusCode::CONFLICT, Some(missing)), TransportError::MissingChunks(vec![ChunkId::from_bytes([1; 32])]));
        assert_eq!(error_for(StatusCode::GONE, Some(body(codes::RESYNC_REQUIRED))), TransportError::ResyncRequired);
        // A viewer's write: told apart from every other refusal, so the engine keeps the space read-only.
        assert_eq!(error_for(StatusCode::FORBIDDEN, Some(body(codes::READ_ONLY))), TransportError::ReadOnly);
        assert!(matches!(error_for(StatusCode::FORBIDDEN, Some(body(codes::FORBIDDEN))), TransportError::Protocol(_)));
        let full = ErrorBody { quota: Some(craft_cloud_proto::api::QuotaUse { used: 9, limit: 10 }), ..body(codes::QUOTA_EXCEEDED) };
        assert_eq!(error_for(StatusCode::INSUFFICIENT_STORAGE, Some(full)), TransportError::QuotaExceeded { used: 9, limit: 10 });
        // Without the numbers it is only a server error: tried again later.
        assert!(matches!(error_for(StatusCode::INSUFFICIENT_STORAGE, Some(body(codes::QUOTA_EXCEEDED))), TransportError::Unavailable(_)));
        assert_eq!(error_for(StatusCode::UNAUTHORIZED, Some(body(codes::UNAUTHORIZED))), TransportError::Unauthorized);
        assert_eq!(error_for(StatusCode::NOT_FOUND, None), TransportError::NotFound);
        assert!(matches!(error_for(StatusCode::TOO_MANY_REQUESTS, Some(body(codes::RATE_LIMITED))), TransportError::Unavailable(_)));
        assert!(matches!(error_for(StatusCode::BAD_GATEWAY, None), TransportError::Unavailable(_)));
        assert!(matches!(error_for(StatusCode::INTERNAL_SERVER_ERROR, Some(body(codes::INTERNAL))), TransportError::Unavailable(_)));
        assert!(matches!(error_for(StatusCode::BAD_REQUEST, Some(body(codes::INVALID))), TransportError::Protocol(_)));
        // A proxy in front of the server may be the one refusing (a body size limit, say): the message says so.
        let refused = error_for(StatusCode::PAYLOAD_TOO_LARGE, None).to_string();
        assert!(refused.contains("the server or a proxy refused") && refused.contains("(HTTP 413)"), "{refused}");
        // A conflict code without its list is not a refused batch the engine could act on.
        assert!(matches!(error_for(StatusCode::CONFLICT, Some(body(codes::CONFLICT))), TransportError::Protocol(_)));
        // An unknown code falls back to the status.
        assert!(matches!(error_for(StatusCode::SERVICE_UNAVAILABLE, Some(body("something_new"))), TransportError::Unavailable(_)));
    }

    #[test]
    fn server_messages_are_cleaned_and_cut() {
        assert_eq!(clean("a\u{1b}[31mb\n"), "a[31mb");
        let long = clean(&"x".repeat(1000));
        assert_eq!(long.chars().count(), MAX_MESSAGE_CHARS + 1);
    }

    #[test]
    fn bigger_bodies_get_longer() {
        assert_eq!(timeout_for(0), Duration::from_secs(60));
        assert_eq!(timeout_for(MAX_CHUNK_BYTES as usize), Duration::from_secs(120));
    }

    #[test]
    fn incompatible_servers_are_refused() {
        let mut info = InfoResponse {
            protocol_min: 1,
            protocol_max: 1,
            chunker_params: vec!["fastcdc-v1".into()],
            max_chunk_bytes: MAX_CHUNK_BYTES,
            max_manifest_bytes: MAX_MANIFEST_BYTES as u64,
            max_batch_ops: 1000,
            max_batch_json_bytes: 1 << 20,
            max_page_nodes: MAX_PAGE_NODES,
            max_missing_ids: MAX_MISSING_IDS as u32,
        };
        assert!(check_compatible(&info).is_ok());
        info.protocol_min = PROTOCOL_MAX + 1;
        info.protocol_max = PROTOCOL_MAX + 2;
        assert!(check_compatible(&info).is_err());
        info.protocol_min = 1;
        info.chunker_params = vec!["other".into()];
        assert!(check_compatible(&info).is_err());
    }
}
