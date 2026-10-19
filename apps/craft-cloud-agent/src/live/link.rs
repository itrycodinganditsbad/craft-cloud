//! The live hub's connection to the server: `GET /api/v1/live` upgraded to a WebSocket, with the same rules as the
//! agent's HTTP requests (see [`crate::http`]): TLS with the platform's certificate store, plain `ws://` only to this
//! machine (`localhost` pinned to 127.0.0.1, never looked up), no proxies, the device token only in the
//! `Authorization` header, a generic User-Agent, a time limit on every step, and frames capped at the protocol's
//! limit.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use craft_cloud_proto::live::MAX_LARGE_FRAME_BYTES;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, USER_AGENT};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use crate::http::{DeviceToken, LOCALHOST_ADDRS, ServerUrl, USER_AGENT_VALUE};

/// A byte stream the WebSocket runs over: plain TCP (to this machine) or TLS.
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// An open live connection.
pub type LiveSocket = WebSocketStream<Box<dyn Io>>;

/// Connecting, the TLS handshake and the WebSocket upgrade each must be done within this.
const STEP_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a connection couldn't be made.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DialError {
    /// Offline, refused, timed out, a server error: tried again later.
    #[error("can't reach the live service: {0}")]
    Unavailable(String),
    /// The server no longer accepts this device (401): the sync loop says so too.
    #[error("the server refused the device token")]
    Unauthorized,
    /// The server or a proxy refused the upgrade with another status (an older server without the live service: 404).
    #[error("the server refused the live connection (HTTP {0})")]
    Refused(u16),
}

async fn step<T, E: std::fmt::Display>(what: &str, f: impl std::future::Future<Output = Result<T, E>>) -> Result<T, DialError> {
    match tokio::time::timeout(STEP_TIMEOUT, f).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => Err(DialError::Unavailable(format!("{what}: {e}"))),
        Err(_) => Err(DialError::Unavailable(format!("{what}: timed out"))),
    }
}

/// The TLS settings: the platform's verifier, aws-lc-rs (as the HTTP client uses), HTTP/1.1 only (a WebSocket upgrade).
fn tls_config() -> Result<rustls::ClientConfig, DialError> {
    use rustls_platform_verifier::BuilderVerifierExt;
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .and_then(|b| b.with_platform_verifier())
        .map_err(|e| DialError::Unavailable(format!("can't set up TLS: {e}")))?
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

/// Opens the live connection to `server`, signed in with `token`.
pub async fn dial(server: &ServerUrl, token: &DeviceToken) -> Result<LiveSocket, DialError> {
    let url = server.live_url();
    let host = url.host_str().ok_or_else(|| DialError::Unavailable("the server address has no host".into()))?.to_owned();
    let port = url.port_or_known_default().ok_or_else(|| DialError::Unavailable("the server address has no port".into()))?;
    let bare = host.trim_start_matches('[').trim_end_matches(']').to_owned();
    let tcp = if bare.eq_ignore_ascii_case("localhost") {
        let addrs: Vec<SocketAddr> = LOCALHOST_ADDRS.iter().map(|a| SocketAddr::new(a.ip(), port)).collect();
        step("connecting", TcpStream::connect(addrs.as_slice())).await?
    } else if let Ok(ip) = bare.parse::<std::net::IpAddr>() {
        step("connecting", TcpStream::connect(SocketAddr::new(ip, port))).await?
    } else {
        step("connecting", TcpStream::connect((bare.as_str(), port))).await?
    };
    let _ = tcp.set_nodelay(true);
    let stream: Box<dyn Io> = if url.scheme() == "wss" {
        let name = rustls::pki_types::ServerName::try_from(bare.clone()).map_err(|e| DialError::Unavailable(format!("the server's name: {e}")))?;
        let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_config()?));
        Box::new(step("TLS", connector.connect(name, tcp)).await?)
    } else if server.is_loopback() {
        Box::new(tcp)
    } else {
        // `ServerUrl` allows plain HTTP only to this machine; checked again here.
        return Err(DialError::Unavailable("plain ws:// is only for this machine".into()));
    };
    let mut request = url.as_str().into_client_request().map_err(|e| DialError::Unavailable(e.to_string()))?;
    let auth = token.bearer().map_err(|e| DialError::Unavailable(e.to_string()))?;
    request.headers_mut().insert(AUTHORIZATION, auth);
    request.headers_mut().insert(USER_AGENT, tokio_tungstenite::tungstenite::http::HeaderValue::from_static(USER_AGENT_VALUE));
    // Up to a snapshot's size: every frame but a snapshot is checked against the smaller limit as it is decoded.
    let config = WebSocketConfig::default().max_message_size(Some(MAX_LARGE_FRAME_BYTES)).max_frame_size(Some(MAX_LARGE_FRAME_BYTES));
    match tokio::time::timeout(STEP_TIMEOUT, tokio_tungstenite::client_async_with_config(request, stream, Some(config))).await {
        Ok(Ok((socket, _))) => Ok(socket),
        Ok(Err(tokio_tungstenite::tungstenite::Error::Http(response))) => match response.status().as_u16() {
            401 => Err(DialError::Unauthorized),
            status @ (408 | 429 | 500..=599) => Err(DialError::Unavailable(format!("the server is busy (HTTP {status})"))),
            status => Err(DialError::Refused(status)),
        },
        Ok(Err(e)) => Err(DialError::Unavailable(format!("the upgrade: {e}"))),
        Err(_) => Err(DialError::Unavailable("the upgrade: timed out".into())),
    }
}
