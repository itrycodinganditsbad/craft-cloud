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
use reqwest::header::{AUTHORIZATION, HeaderValue};
use reqwest::{Client, Method, RequestBuilder, Response, StatusCode, Url};
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
const USER_AGENT: &str = concat!("Craft-Cloud/", env!("CARGO_PKG_VERSION"));

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

fn client(server: &ServerUrl) -> Result<Client, TransportError> {
    // No proxy: `HTTP_PROXY`/`ALL_PROXY` would carry the plain-HTTP loopback requests (token, password) off the
    // machine. Proxy support for HTTPS comes later, if asked for.
    let mut builder = Client::builder().no_proxy();
    if server.0.host_str() == Some("localhost") {
        builder = builder.resolve_to_addrs("localhost", &LOCALHOST);
    }
    builder
        .user_agent(USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .https_only(!server.is_loopback())
        .build()
        .map_err(|e| TransportError::Protocol(format!("can't set up HTTPS: {e}")))
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

/// A server message cut to a sane length, without control characters.
fn clean(message: &str) -> String {
    let mut out: String = message.chars().filter(|c| !c.is_control()).take(MAX_MESSAGE_CHARS).collect();
    if message.chars().count() > MAX_MESSAGE_CHARS {
        out.push('…');
    }
    out
}

/// Reads a body of at most `max` bytes; a longer one is a protocol error and is not buffered.
async fn body_capped(mut resp: Response, max: usize) -> Result<Bytes, TransportError> {
    let too_long = || TransportError::Protocol(format!("the server's answer is over the limit of {max} bytes"));
    let declared = resp.content_length();
    if declared.is_some_and(|len| len > max as u64) {
        return Err(too_long());
    }
    let mut buf = BytesMut::with_capacity(declared.map_or(0, |len| usize::try_from(len).unwrap_or(max).min(max)));
    while let Some(piece) = resp.chunk().await.map_err(network)? {
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
async fn checked(resp: Response) -> Result<Response, TransportError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = body_capped(resp, MAX_SMALL_JSON_BYTES).await.ok().and_then(|b| serde_json::from_slice::<ErrorBody>(&b).ok());
    Err(error_for(status, body))
}

async fn json<T: DeserializeOwned>(resp: Response, max: usize) -> Result<T, TransportError> {
    let bytes = body_capped(checked(resp).await?, max).await?;
    serde_json::from_slice(&bytes).map_err(|e| TransportError::Protocol(format!("the server's answer doesn't parse: {e}")))
}

async fn send(req: RequestBuilder) -> Result<Response, TransportError> {
    req.send().await.map_err(network)
}

/// What the server speaks. No sign-in needed.
pub async fn info(server: &ServerUrl) -> Result<InfoResponse, TransportError> {
    let req = client(server)?.get(server.endpoint("info")).timeout(timeout_for(0));
    json(send(req).await?, MAX_SMALL_JSON_BYTES).await
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
    let body = LoginRequest { username: username.to_owned(), password: password.to_owned(), device_name: device_name.to_owned() };
    let req = client(server)?.post(server.endpoint("auth/login")).json(&body).timeout(timeout_for(0));
    json(send(req).await?, MAX_SMALL_JSON_BYTES).await
}

/// The server API for one signed-in device.
#[derive(Debug, Clone)]
pub struct HttpTransport {
    client: Client,
    server: ServerUrl,
    auth: HeaderValue,
}

impl HttpTransport {
    pub fn new(server: ServerUrl, token: &DeviceToken) -> Result<Self, TransportError> {
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", token.0))
            .map_err(|_| TransportError::Protocol("the saved device token is damaged; sign in again".into()))?;
        auth.set_sensitive(true);
        Ok(Self { client: client(&server)?, server, auth })
    }

    pub fn server(&self) -> &ServerUrl {
        &self.server
    }

    fn request(&self, method: Method, path: &str, max_body: usize) -> RequestBuilder {
        self.client.request(method, self.server.endpoint(path)).header(AUTHORIZATION, self.auth.clone()).timeout(timeout_for(max_body))
    }

    fn page_request(&self, path: &str, since: Option<u64>, after: Option<PageCursor>) -> RequestBuilder {
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
        self.client.get(url).header(AUTHORIZATION, self.auth.clone()).timeout(timeout_for(MAX_PAGE_JSON_BYTES))
    }

    async fn page(&self, req: RequestBuilder) -> Result<ChangeSet, TransportError> {
        let page: ChangeSet = json(send(req).await?, MAX_PAGE_JSON_BYTES).await?;
        if page.nodes.len() > MAX_PAGE_NODES as usize {
            return Err(TransportError::Protocol(format!("a page of {} nodes is over the limit of {MAX_PAGE_NODES}", page.nodes.len())));
        }
        Ok(page)
    }

    async fn no_content(&self, req: RequestBuilder) -> Result<(), TransportError> {
        checked(send(req).await?).await?;
        Ok(())
    }

    /// Revokes a device of this account (this one, to sign out).
    pub async fn revoke(&self, device: DeviceId) -> Result<(), TransportError> {
        self.no_content(self.request(Method::DELETE, &format!("devices/{device}"), 0)).await
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn space(&self) -> Result<SpaceResponse, TransportError> {
        json(send(self.request(Method::GET, "space", 0)).await?, MAX_SMALL_JSON_BYTES).await
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
            let req = self.request(Method::POST, "chunks/missing", MAX_SMALL_JSON_BYTES).json(&MissingRequest { ids: batch.to_vec() });
            let answer: MissingResponse = json(send(req).await?, MAX_SMALL_JSON_BYTES).await?;
            if answer.missing.len() > batch.len() {
                return Err(TransportError::Protocol("the server named more missing chunks than were asked about".into()));
            }
            missing.extend(answer.missing);
        }
        Ok(missing)
    }

    async fn put_chunk(&self, id: &ChunkId, data: Bytes) -> Result<(), TransportError> {
        let req = self
            .request(Method::PUT, &format!("chunks/{id}"), data.len())
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(data);
        self.no_content(req).await
    }

    async fn get_chunk(&self, id: &ChunkId) -> Result<Bytes, TransportError> {
        let resp = checked(send(self.request(Method::GET, &format!("chunks/{id}"), MAX_CHUNK_BYTES as usize)).await?).await?;
        body_capped(resp, MAX_CHUNK_BYTES as usize).await
    }

    async fn put_manifest(&self, id: &ManifestId, data: Bytes) -> Result<(), TransportError> {
        let req = self
            .request(Method::PUT, &format!("manifests/{id}"), data.len())
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(data);
        self.no_content(req).await
    }

    async fn get_manifest(&self, id: &ManifestId) -> Result<Bytes, TransportError> {
        let resp = checked(send(self.request(Method::GET, &format!("manifests/{id}"), MAX_MANIFEST_BYTES)).await?).await?;
        body_capped(resp, MAX_MANIFEST_BYTES).await
    }

    async fn commit(&self, ops: &[Op]) -> Result<CommitResponse, TransportError> {
        let req = self.request(Method::POST, "commit", MAX_SMALL_JSON_BYTES).json(&CommitRequest { ops: ops.to_vec() });
        json(send(req).await?, MAX_SMALL_JSON_BYTES).await
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
