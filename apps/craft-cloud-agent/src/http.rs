//! The server's HTTP API (`/api/v1`) as the engine's [`Transport`], plus sign-in and device revocation.
//!
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
//!   a refused batch with its conflicts, missing chunks and `resync_required` by code; 401 is `Unauthorized`,
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
    CommitRequest, CommitResponse, ErrorBody, InfoResponse, LoginRequest, LoginResponse, MAX_MISSING_IDS, MissingRequest, MissingResponse,
    PROTOCOL_MAX, PROTOCOL_MIN, SpaceResponse, codes,
};
use craft_cloud_proto::limits::{MAX_BATCH_OPS, MAX_CHUNK_BYTES, MAX_MANIFEST_BYTES, MAX_PAGE_NODES};
use craft_cloud_proto::{ChangeSet, ChunkId, DeviceId, ManifestId, Op, PageCursor};
use craft_cloud_sync::{Transport, TransportError};
use reqwest::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HeaderMap, HeaderValue, RETRY_AFTER, USER_AGENT};
use reqwest::{Client, Method, StatusCode, Url};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Largest tree or change-feed page, in bytes of JSON: [`MAX_PAGE_NODES`] nodes of at most about 1.5 KiB each
/// (a 255-byte name that JSON-escapes to twice that, IDs, hashes and numbers), with room to spare.
pub const MAX_PAGE_JSON_BYTES: usize = 16 * 1024 * 1024;
/// Largest other answer: a missing-chunk list of [`MAX_MISSING_IDS`] IDs is about 670 KiB.
pub const MAX_SMALL_JSON_BYTES: usize = 2 * 1024 * 1024;
/// Longest server message shown to the user, in characters.
const MAX_MESSAGE_CHARS: usize = 300;
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
}

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

    /// Whether the server is on this machine (plain HTTP is allowed then).
    pub fn is_loopback(&self) -> bool {
        is_loopback(&self.0)
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
        // No proxy: `HTTP_PROXY`/`ALL_PROXY` would carry the plain-HTTP loopback requests (token, password) off the
        // machine. Proxy support for HTTPS comes later, if asked for.
        let mut builder = Client::builder().no_proxy();
        if server.0.host_str() == Some("localhost") {
            builder = builder.resolve_to_addrs("localhost", &LOCALHOST);
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

#[async_trait]
impl Connection for Reqwest {
    async fn send(&self, request: Request) -> Result<Answer, TransportError> {
        let mut req = self.0.request(request.method, request.url).headers(request.headers).timeout(request.timeout);
        if let Some(body) = request.body {
            req = req.body(body);
        }
        let resp = req.send().await.map_err(network)?;
        Ok(Answer { status: resp.status(), headers: resp.headers().clone(), body: Box::new(ReqwestBody(resp)) })
    }
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
    let body = LoginRequest { username: username.to_owned(), password: password.to_owned(), device_name: device_name.to_owned() };
    let req = request(Method::POST, server.endpoint("auth/login"), None, 0).json(&body)?;
    json(connection.send(req).await?, MAX_SMALL_JSON_BYTES).await
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
}

impl HttpTransport {
    pub fn new(server: ServerUrl, token: &DeviceToken) -> Result<Self, TransportError> {
        Self::over(Reqwest::new(&server)?, server, token)
    }
}

impl<C: Connection> HttpTransport<C> {
    /// The API over another connection (a simulated network, in tests). `server` names the endpoints.
    pub fn over(connection: C, server: ServerUrl, token: &DeviceToken) -> Result<Self, TransportError> {
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", token.0))
            .map_err(|_| TransportError::Protocol("the saved device token is damaged; sign in again".into()))?;
        auth.set_sensitive(true);
        Ok(Self { connection, server, auth })
    }

    pub fn server(&self) -> &ServerUrl {
        &self.server
    }

    fn request(&self, method: Method, path: &str, max_body: usize) -> Request {
        request(method, self.server.endpoint(path), Some(&self.auth), max_body)
    }

    async fn send(&self, req: Request) -> Result<Answer, TransportError> {
        self.connection.send(req).await
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

    /// Revokes a device of this account (this one, to sign out).
    pub async fn revoke(&self, device: DeviceId) -> Result<(), TransportError> {
        self.no_content(self.request(Method::DELETE, &format!("devices/{device}"), 0)).await
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use craft_cloud_proto::{Conflict, ConflictKind};

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
        let body = LoginRequest { username: "ana".into(), password: "correct horse battery".into(), device_name: "laptop".into() };
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

    fn body(code: &str) -> ErrorBody {
        ErrorBody { error: code.into(), message: "m".into(), conflicts: None, missing: None }
    }

    #[test]
    fn error_answers_map_by_code_then_status() {
        let conflict = Conflict { op: 0, kind: ConflictKind::NoSuchNode };
        let rejected = ErrorBody { conflicts: Some(vec![conflict.clone()]), ..body(codes::CONFLICT) };
        assert_eq!(error_for(StatusCode::CONFLICT, Some(rejected)), TransportError::Rejected(vec![conflict]));
        let missing = ErrorBody { missing: Some(vec![ChunkId::from_bytes([1; 32])]), ..body(codes::MISSING_CHUNKS) };
        assert_eq!(error_for(StatusCode::CONFLICT, Some(missing)), TransportError::MissingChunks(vec![ChunkId::from_bytes([1; 32])]));
        assert_eq!(error_for(StatusCode::GONE, Some(body(codes::RESYNC_REQUIRED))), TransportError::ResyncRequired);
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
        info.protocol_min = 2;
        info.protocol_max = 3;
        assert!(check_compatible(&info).is_err());
        info.protocol_min = 1;
        info.chunker_params = vec!["other".into()];
        assert!(check_compatible(&info).is_err());
    }
}
