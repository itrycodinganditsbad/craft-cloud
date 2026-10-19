//! `GET /v1/docs`: the local API's WebSocket for live documents (the messages are in
//! `craft-cloud-proto::local::docs`). One connection per app connection; it may open many documents. The connection
//! turns the app's messages into [`DocsCall`]s for the hub and the hub's [`DocOut`]s into messages for the app.
//!
//! Everything from an app is hostile: a text message that isn't a request is answered `error`; a binary frame without
//! a header or of a kind an app doesn't send ends the connection; an update over 1 MiB, presence over 4 KiB or an
//! import over the snapshot limit is refused (and never reaches the hub). An app that doesn't take a message within 10
//! s is dropped, and so is one whose queue at the hub fills up; its documents stay with the agent. An app that goes
//! away mid-import leaves nothing behind: the import goes up only once it is whole. Signed out, every `open` is
//! answered `not_live {not_signed_in}`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::{IntoResponse, Response};
use craft_cloud_proto::api::codes as api_codes;
use craft_cloud_proto::live::{MAX_PRESENCE_BYTES, MAX_SNAPSHOT_BYTES, MAX_UPDATE_BYTES};
use craft_cloud_proto::local::docs::{IMPORT, MAX_BINARY_BYTES, PRESENCE, STATE, UPDATE, decode_binary, encode_binary};
use craft_cloud_proto::local::{DocsEvent, DocsRequest, MAX_LOCAL_JSON_BYTES, NotLiveReason};
use tokio::sync::mpsc;

use super::hub::{ConnId, DOCS_QUEUE, DocOut, DocsCall, HubHandle};
use crate::local_api::{ApiState, LocalError};

/// A message the app doesn't take within this long drops it.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

/// `GET /v1/docs`: the WebSocket of an app's live documents.
pub async fn docs_socket(State(api): State<Arc<ApiState>>, upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>) -> Response {
    let upgrade = match upgrade {
        Ok(upgrade) => upgrade,
        Err(e) => return LocalError::new(e.status(), api_codes::INVALID, e.body_text()).into_response(),
    };
    upgrade
        .max_message_size(MAX_BINARY_BYTES.max(MAX_LOCAL_JSON_BYTES))
        .max_frame_size(MAX_BINARY_BYTES.max(MAX_LOCAL_JSON_BYTES))
        .on_upgrade(move |socket| serve(socket, api))
}

/// Why the connection ends.
enum End {
    /// The app went, or broke the rules, or is too slow.
    App,
    /// The hub dropped the connection (or stopped: signed out).
    Hub,
}

/// One app connection.
struct Link {
    conn: ConnId,
    hub: Option<HubHandle>,
    /// The hub's messages for this connection (`None` until it has a hub).
    from_hub: Option<mpsc::Receiver<DocOut>>,
    /// The app's updates so far, per document number (their `seq`).
    seqs: HashMap<u64, u64>,
}

impl Link {
    /// The hub, connected to on first use (signed in after the app connected: it is there now).
    async fn hub(&mut self, api: &ApiState) -> Option<HubHandle> {
        if self.hub.is_none()
            && let Some(hub) = api.control.hub()
        {
            let (out, from_hub) = mpsc::channel(DOCS_QUEUE);
            if hub.docs(DocsCall::Connect { conn: self.conn, out }).await.is_ok() {
                self.hub = Some(hub);
                self.from_hub = Some(from_hub);
            }
        }
        self.hub.clone()
    }
}

async fn serve(mut socket: WebSocket, api: Arc<ApiState>) {
    let mut link = Link { conn: NEXT_CONN.fetch_add(1, Ordering::Relaxed), hub: None, from_hub: None, seqs: HashMap::new() };
    link.hub(&api).await;
    let end = loop {
        tokio::select! {
            message = socket.recv() => {
                let message = match message {
                    Some(Ok(message)) => message,
                    Some(Err(_)) | None => break End::App,
                };
                match on_message(&mut socket, &mut link, &api, message).await {
                    Ok(true) => {}
                    Ok(false) => break End::App,
                    Err(end) => break end,
                }
            }
            out = async { match link.from_hub.as_mut() { Some(rx) => rx.recv().await, None => std::future::pending().await } } => match out {
                Some(out) => {
                    if !send_out(&mut socket, out).await {
                        break End::App;
                    }
                }
                // The hub let go of this connection (it didn't keep up, or the hub stopped).
                None => break End::Hub,
            },
        }
    };
    if let (End::App, Some(hub)) = (end, link.hub.as_ref()) {
        let _ = hub.docs(DocsCall::Gone { conn: link.conn }).await;
    }
    let _ = socket.send(Message::Close(None)).await;
}

/// One message from the app: `Ok(false)` ends the connection (the app is gone, or broke the rules), `Err` too, for a
/// hub that stopped.
async fn on_message(socket: &mut WebSocket, link: &mut Link, api: &ApiState, message: Message) -> Result<bool, End> {
    match message {
        Message::Text(text) => {
            let request = match serde_json::from_str::<DocsRequest>(&text) {
                Ok(request) => request,
                Err(e) => {
                    let event =
                        DocsEvent::Error { id: None, seq: None, code: api_codes::INVALID.into(), message: format!("not a docs request: {e}") };
                    return Ok(send_json(socket, &event).await);
                }
            };
            match request {
                DocsRequest::Open { id, path, app } => match link.hub(api).await {
                    Some(hub) => hub.docs(DocsCall::Open { conn: link.conn, id, path, app }).await.map(|()| true).map_err(|_| End::Hub),
                    None => {
                        let event = DocsEvent::NotLive {
                            id,
                            reason: NotLiveReason::NotSignedIn,
                            message: Some("the Craft Cloud agent isn't signed in".into()),
                        };
                        Ok(send_json(socket, &event).await)
                    }
                },
                DocsRequest::Close { doc } => match link.hub.clone() {
                    Some(hub) => hub.docs(DocsCall::Close { conn: link.conn, doc }).await.map(|()| true).map_err(|_| End::Hub),
                    None => Ok(true),
                },
                DocsRequest::Saved { .. } => Ok(true),
            }
        }
        Message::Binary(bytes) => {
            // A frame without a header, or of a kind an app never sends: the app is broken or hostile.
            let Some((kind, id, body)) = decode_binary(&bytes) else { return Ok(false) };
            let Some(hub) = link.hub.clone() else { return Ok(kind == UPDATE || kind == PRESENCE || kind == IMPORT) };
            let refuse = |seq: Option<u64>, message: String| DocsEvent::Error { id: Some(id), seq, code: api_codes::TOO_LARGE.into(), message };
            let call = match kind {
                UPDATE => {
                    let seq = link.seqs.entry(id).or_insert(0);
                    *seq += 1;
                    if body.len() > MAX_UPDATE_BYTES {
                        return Ok(send_json(socket, &refuse(Some(*seq), format!("an update is at most {MAX_UPDATE_BYTES} bytes"))).await);
                    }
                    DocsCall::Update { conn: link.conn, id, seq: *seq, bytes: body.to_vec() }
                }
                PRESENCE => {
                    if body.is_empty() || body.len() > MAX_PRESENCE_BYTES {
                        return Ok(true);
                    }
                    DocsCall::Presence { conn: link.conn, id, bytes: body.to_vec() }
                }
                IMPORT => {
                    if body.len() > MAX_SNAPSHOT_BYTES {
                        return Ok(send_json(socket, &refuse(None, format!("an import is at most {MAX_SNAPSHOT_BYTES} bytes"))).await);
                    }
                    DocsCall::Import { conn: link.conn, id, bytes: body.to_vec() }
                }
                _ => return Ok(false),
            };
            hub.docs(call).await.map(|()| true).map_err(|_| End::Hub)
        }
        Message::Ping(_) | Message::Pong(_) => Ok(true),
        Message::Close(_) => Ok(false),
    }
}

/// Sends one of the hub's messages to the app; false when the app is gone (or too slow).
async fn send_out(socket: &mut WebSocket, out: DocOut) -> bool {
    match out {
        DocOut::Opened { id, doc, state, snapshot, waiting } => {
            send_json(socket, &DocsEvent::Opened { id, doc, state }).await && send_binary(socket, STATE, id, &snapshot).await && {
                let mut ok = true;
                for update in &waiting {
                    ok = ok && send_binary(socket, UPDATE, id, update).await;
                }
                ok
            }
        }
        DocOut::NeedsImport { id } => send_json(socket, &DocsEvent::NeedsImport { id }).await,
        DocOut::NotLive { id, reason, message } => send_json(socket, &DocsEvent::NotLive { id, reason, message: Some(message) }).await,
        DocOut::Ack { id, seq } => send_json(socket, &DocsEvent::Ack { id, seq }).await,
        DocOut::Refused { id, seq, code, message } => send_json(socket, &DocsEvent::Error { id, seq, code: code.into(), message }).await,
        DocOut::Update { id, bytes } => send_binary(socket, UPDATE, id, &bytes).await,
        DocOut::Presence { id, bytes } => send_binary(socket, PRESENCE, id, &bytes).await,
        DocOut::Status { doc, online, waiting, deleted, stopped } => {
            send_json(socket, &DocsEvent::Status { doc, online, waiting, deleted, stopped }).await
        }
    }
}

async fn send_json(socket: &mut WebSocket, event: &DocsEvent) -> bool {
    let Ok(text) = serde_json::to_string(event) else { return true };
    matches!(tokio::time::timeout(SEND_TIMEOUT, socket.send(Message::Text(text.into()))).await, Ok(Ok(())))
}

async fn send_binary(socket: &mut WebSocket, kind: u8, id: u64, bytes: &[u8]) -> bool {
    let frame = encode_binary(kind, id, bytes);
    matches!(tokio::time::timeout(SEND_TIMEOUT, socket.send(Message::Binary(frame.into()))).await, Ok(Ok(())))
}
