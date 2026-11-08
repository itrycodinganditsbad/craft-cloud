//! `GET /v1/docs`: the local API's WebSocket for live documents (the messages are in
//! `craft-cloud-proto::local::docs`). One connection per app connection; it may open many documents. The connection
//! turns the app's messages into [`DocsCall`]s for the hub and the hub's [`DocOut`]s into messages for the app.
//!
//! A document is routed by the path the app opens: an absolute path inside a synced team folder goes to that space's hub
//! (its own live link), any other path to the personal hub. The connection joins a hub when it first uses it, remembers
//! which hub each open number and each document belongs to, and sends the app's later messages to that hub. A team space
//! that stops syncing meanwhile answers the opens that were routed to it as plain files; the app's other documents go on.
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
use craft_cloud_proto::local::docs::{IMPORT, MAX_BINARY_BYTES, PRESENCE, PRESENCE_FROM, STATE, UPDATE, decode_binary, encode_binary};
use craft_cloud_proto::local::{DocsEvent, DocsRequest, MAX_LOCAL_JSON_BYTES, NotLiveReason};
use craft_cloud_proto::{DocId, SpaceId};
use tokio::sync::mpsc;

use super::hub::{ConnId, DOCS_QUEUE, DocOut, DocsCall, HubHandle};
use crate::local_api::{ApiState, LocalError};

/// A message the app doesn't take within this long drops it.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// Most open numbers one connection may have routed (an app opens a few documents; a hostile one is cut off here).
const MAX_OPENS: usize = 4096;

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

/// Which hub: `None` is the personal space's, `Some` a team space's.
type Key = Option<SpaceId>;

/// One app connection.
struct Link {
    conn: ConnId,
    /// The hubs this connection joined, by space.
    hubs: Vec<(Key, HubHandle)>,
    /// Where every hub's messages for this connection come from (`None` until it has joined a hub). The sender is held
    /// weakly: the channel closes when the last hub lets go of the connection.
    from_hub: Option<mpsc::Receiver<DocOut>>,
    to_conn: Option<mpsc::WeakSender<DocOut>>,
    /// The app's updates so far, per document number (their `seq`).
    seqs: HashMap<u64, u64>,
    /// The hub each open number was routed to.
    opens: HashMap<u64, Key>,
    /// The hub each opened document belongs to (for `saved`, `saving` and `close`, which name the document).
    docs: HashMap<DocId, Key>,
    /// The document each answered open number is (so a `close` can let go of its numbers).
    open_docs: HashMap<u64, DocId>,
}

impl Link {
    /// The hub for `key`, joined on first use (the personal hub: signed in after the app connected, it is there now). `None`
    /// when there is no such hub (signed out, or the space doesn't sync here).
    async fn hub(&mut self, api: &ApiState, key: Key) -> Option<HubHandle> {
        if let Some((_, hub)) = self.hubs.iter().find(|(k, _)| *k == key) {
            return Some(hub.clone());
        }
        let hub = match key {
            None => api.control.hub()?,
            Some(space) => api.control.team_hub(space)?.handle,
        };
        let out = match &self.to_conn {
            Some(weak) => weak.upgrade()?,
            None => {
                let (out, from_hub) = mpsc::channel(DOCS_QUEUE);
                self.to_conn = Some(out.downgrade());
                self.from_hub = Some(from_hub);
                out
            }
        };
        hub.docs(DocsCall::Connect { conn: self.conn, out }).await.ok()?;
        self.hubs.push((key, hub.clone()));
        Some(hub)
    }

    /// The hub an open number was routed to, if this connection joined it.
    fn hub_of(&self, id: u64) -> Option<(Key, HubHandle)> {
        let key = self.opens.get(&id)?;
        self.hubs.iter().find(|(k, _)| k == key).map(|(k, h)| (*k, h.clone()))
    }

    /// The hub a document belongs to, if this connection joined it.
    fn hub_of_doc(&self, doc: DocId) -> Option<(Key, HubHandle)> {
        let key = self.docs.get(&doc)?;
        self.hubs.iter().find(|(k, _)| k == key).map(|(k, h)| (*k, h.clone()))
    }

    /// A team hub that is gone (its space stopped syncing): forgotten, with the routes that led to it.
    fn forget(&mut self, key: Key) {
        self.hubs.retain(|(k, _)| *k != key);
        self.opens.retain(|_, k| *k != key);
        self.docs.retain(|_, k| *k != key);
        self.open_docs.retain(|id, _| self.opens.contains_key(id));
    }

    /// Hands `call` to the hub `key`. A personal hub that is gone ends the connection; a team hub that is gone is forgotten
    /// (`Ok(false)` says so: the call did not arrive).
    async fn call(&mut self, hub: &HubHandle, key: Key, call: DocsCall) -> Result<bool, End> {
        match hub.docs(call).await {
            Ok(()) => Ok(true),
            Err(_) if key.is_none() => Err(End::Hub),
            Err(_) => {
                self.forget(key);
                Ok(false)
            }
        }
    }
}

async fn serve(mut socket: WebSocket, api: Arc<ApiState>) {
    let mut link = Link {
        conn: NEXT_CONN.fetch_add(1, Ordering::Relaxed),
        hubs: Vec::new(),
        from_hub: None,
        to_conn: None,
        seqs: HashMap::new(),
        opens: HashMap::new(),
        docs: HashMap::new(),
        open_docs: HashMap::new(),
    };
    link.hub(&api, None).await;
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
                    match &out {
                        DocOut::Opened { id, doc, .. } => {
                            if let Some(key) = link.opens.get(id).copied() {
                                link.docs.insert(*doc, key);
                                link.open_docs.insert(*id, *doc);
                            }
                        }
                        // Not a live document: its number is free again.
                        DocOut::NotLive { id, .. } => {
                            link.opens.remove(id);
                        }
                        _ => {}
                    }
                    if !send_out(&mut socket, out).await {
                        break End::App;
                    }
                }
                // Every hub let go of this connection (it didn't keep up, or the hubs stopped).
                None => break End::Hub,
            },
        }
    };
    if let End::App = end {
        for (_, hub) in &link.hubs {
            let _ = hub.docs(DocsCall::Gone { conn: link.conn }).await;
        }
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
                DocsRequest::Open { id, path, app, file_hash, identity } => {
                    if link.opens.len() >= MAX_OPENS && !link.opens.contains_key(&id) {
                        let event = DocsEvent::Error {
                            id: Some(id),
                            seq: None,
                            code: api_codes::TOO_LARGE.into(),
                            message: "too many documents open".into(),
                        };
                        return Ok(send_json(socket, &event).await);
                    }
                    // An absolute path inside a synced team folder is that space's; any other is the personal space's.
                    let key = api.control.route_path(&path).map(|team| team.space);
                    let not_live = |reason, message: &str| DocsEvent::NotLive { id, reason, message: Some(message.into()) };
                    match link.hub(api, key).await {
                        Some(hub) => {
                            link.opens.entry(id).or_insert(key);
                            let call = DocsCall::Open { conn: link.conn, id, path, app, file_hash, identity };
                            if link.call(&hub, key, call).await? {
                                Ok(true)
                            } else {
                                Ok(send_json(socket, &not_live(NotLiveReason::Offline, "the space stopped syncing on this computer")).await)
                            }
                        }
                        None if key.is_some() => {
                            Ok(send_json(socket, &not_live(NotLiveReason::Offline, "the space doesn't sync on this computer")).await)
                        }
                        None => Ok(send_json(socket, &not_live(NotLiveReason::NotSignedIn, "the Craft Cloud agent isn't signed in")).await),
                    }
                }
                DocsRequest::Close { doc } => match link.hub_of_doc(doc) {
                    Some((key, hub)) => {
                        link.docs.remove(&doc);
                        let numbers: Vec<u64> = link.open_docs.iter().filter(|(_, d)| **d == doc).map(|(id, _)| *id).collect();
                        for id in numbers {
                            link.open_docs.remove(&id);
                            link.opens.remove(&id);
                        }
                        link.call(&hub, key, DocsCall::Close { conn: link.conn, doc }).await.map(|_| true)
                    }
                    None => Ok(true),
                },
                // In order with the connection's changes: the hub keeps it once the changes it covers are in.
                DocsRequest::Saved { doc, hash, frontiers } => match link.hub_of_doc(doc) {
                    Some((key, hub)) => link.call(&hub, key, DocsCall::Saved { conn: link.conn, doc, hash, frontiers }).await.map(|_| true),
                    None => Ok(true),
                },
                // At once, before the app writes the file: the sync waits for that save's record.
                DocsRequest::Saving { doc, hash } => match link.hub_of_doc(doc) {
                    Some((key, hub)) => link.call(&hub, key, DocsCall::Saving { conn: link.conn, doc, hash }).await.map(|_| true),
                    None => Ok(true),
                },
            }
        }
        Message::Binary(bytes) => {
            // A frame without a header, or of a kind an app never sends: the app is broken or hostile.
            let Some((kind, id, body)) = decode_binary(&bytes) else { return Ok(false) };
            let Some((key, hub)) = link.hub_of(id) else {
                // No hub for it: signed out, or never opened (or its space stopped syncing); a kind an app sends is let be.
                return Ok(kind == UPDATE || kind == PRESENCE || kind == IMPORT);
            };
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
            link.call(&hub, key, call).await.map(|_| true)
        }
        Message::Ping(_) | Message::Pong(_) => Ok(true),
        Message::Close(_) => Ok(false),
    }
}

/// Sends one of the hub's messages to the app; false when the app is gone (or too slow).
async fn send_out(socket: &mut WebSocket, out: DocOut) -> bool {
    match out {
        DocOut::Opened { id, doc, state, changed_outside, read_only, space, snapshot, waiting } => {
            send_json(socket, &DocsEvent::Opened { id, doc, state, changed_outside, read_only, space: Some(space) }).await
                && send_binary(socket, STATE, id, &snapshot).await
                && {
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
        DocOut::PresenceFrom { id, user, bytes } => {
            // `[16 bytes account ID][the app's bytes]` after the header.
            let mut body = Vec::with_capacity(bytes.len().saturating_add(craft_cloud_proto::local::docs::USER_BYTES));
            body.extend_from_slice(user.as_bytes());
            body.extend_from_slice(&bytes);
            send_binary(socket, PRESENCE_FROM, id, &body).await
        }
        DocOut::Members { id, members } => send_json(socket, &DocsEvent::Members { id, members: members.as_ref().clone() }).await,
        DocOut::Status { doc, online, waiting, deleted, stopped } => {
            send_json(socket, &DocsEvent::Status { doc, online, waiting, deleted, stopped }).await
        }
        DocOut::File { doc, frontiers } => send_json(socket, &DocsEvent::File { doc, frontiers }).await,
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
