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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
/// After a write to the app failed, its frames already sent are still read for this long (until its close or EOF).
const READ_ON: Duration = Duration::from_secs(5);

/// [`READ_ON`], or what a test set.
fn read_on() -> Duration {
    match READ_ON_MS.load(Ordering::Relaxed) {
        0 => READ_ON,
        ms => Duration::from_millis(ms),
    }
}

static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

/// Tests: while set, every write to an app fails, as when the app has already shut its socket down.
#[doc(hidden)]
pub static FAIL_WRITES: AtomicBool = AtomicBool::new(false);

/// Tests: the name of an app whose connections' writes fail, as with [`FAIL_WRITES`] but for that app only.
#[doc(hidden)]
pub static FAIL_WRITES_OF: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Tests: how many milliseconds a connection whose write failed is read on (0: [`READ_ON`]).
#[doc(hidden)]
pub static READ_ON_MS: AtomicU64 = AtomicU64::new(0);

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

/// Why a message didn't reach the app.
enum Unsent {
    /// The write failed (the app shut its socket down): nothing more is written, but what it sent before is still read.
    Closed,
    /// The app didn't take the message in time: it is dropped.
    Slow,
}

/// One app connection.
struct Link {
    conn: ConnId,
    hub: Option<HubHandle>,
    /// The hub's messages for this connection (`None` until it has a hub).
    from_hub: Option<mpsc::Receiver<DocOut>>,
    /// The app's updates so far, per document number (their `seq`).
    seqs: HashMap<u64, u64>,
    /// Set when a write to the app failed: nothing is written to it any more, and what it sent before is read until
    /// this time (or its close).
    mute: Option<tokio::time::Instant>,
    /// The app named by the connection's last `open` (for the tests' [`FAIL_WRITES_OF`]).
    app: Option<String>,
}

impl Link {
    /// Whether a test made the writes to this connection's app fail.
    fn writes_fail(&self) -> bool {
        let named = FAIL_WRITES_OF.lock().ok().and_then(|n| n.clone());
        named.is_some() && named == self.app
    }

    /// A write to the app failed: stop writing, and read on for a while.
    fn mute_now(&mut self) {
        if self.mute.is_none() {
            self.mute = Some(tokio::time::Instant::now() + read_on());
            // Nothing from the hub is read any more: it learns that at once, instead of when the queue is full.
            self.from_hub = None;
        }
    }

    /// Writes `event` to the app, unless a write failed before. A write that failed because the app is gone doesn't end
    /// the connection (its earlier frames are read first); an app too slow to take it does.
    async fn reply(&mut self, socket: &mut WebSocket, event: &DocsEvent) -> Result<(), End> {
        if self.mute.is_some() {
            return Ok(());
        }
        let result = if self.writes_fail() { Err(Unsent::Closed) } else { send_json(socket, event).await };
        self.sent(result)
    }

    /// What came of a write.
    fn sent(&mut self, result: Result<(), Unsent>) -> Result<(), End> {
        match result {
            Ok(()) => Ok(()),
            Err(Unsent::Closed) => {
                self.mute_now();
                Ok(())
            }
            Err(Unsent::Slow) => Err(End::App),
        }
    }

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
    let mut link = Link { conn: NEXT_CONN.fetch_add(1, Ordering::Relaxed), hub: None, from_hub: None, seqs: HashMap::new(), mute: None, app: None };
    link.hub(&api).await;
    let _end = loop {
        tokio::select! {
            message = socket.recv() => {
                // A receive error ends the connection, muted or not: after an abrupt reset (as opposed to the app's close
                // or EOF) frames that were still unread may be lost with it. Reading on after an error is not safe to
                // rely on (the stream may keep failing), so that is a limit of what the agent can keep.
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
            () = async { match link.mute { Some(until) => tokio::time::sleep_until(until).await, None => std::future::pending().await } } => break End::App,
            out = async { match link.from_hub.as_mut() { Some(rx) => rx.recv().await, None => std::future::pending().await } }, if link.mute.is_none() => match out {
                Some(out) => {
                    let result = if link.writes_fail() { Err(Unsent::Closed) } else { send_out(&mut socket, out).await };
                    if let Err(end) = link.sent(result) {
                        break end;
                    }
                }
                // The hub let go of this connection (it didn't keep up, or the hub stopped).
                None => break End::Hub,
            },
        }
    };
    // Always, and last: the hub cleans up after every change this connection queued (it may have dropped the connection's
    // output already, which leaves its handles in place for exactly this call).
    if let Some(hub) = link.hub.as_ref() {
        let _ = hub.docs(DocsCall::Gone { conn: link.conn }).await;
    }
    if link.mute.is_none() {
        // Bounded: an app that is too slow to take messages may not take this one either.
        let _ = tokio::time::timeout(SEND_TIMEOUT, socket.send(Message::Close(None))).await;
    }
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
                    link.reply(socket, &event).await?;
                    return Ok(true);
                }
            };
            if let DocsRequest::Open { app, .. } = &request {
                link.app = Some(app.clone());
            }
            match request {
                DocsRequest::Open { id, path, app, file_hash } => match link.hub(api).await {
                    Some(hub) => hub.docs(DocsCall::Open { conn: link.conn, id, path, app, file_hash }).await.map(|()| true).map_err(|_| End::Hub),
                    None => {
                        let event = DocsEvent::NotLive {
                            id,
                            reason: NotLiveReason::NotSignedIn,
                            message: Some("the Craft Cloud agent isn't signed in".into()),
                        };
                        link.reply(socket, &event).await?;
                        Ok(true)
                    }
                },
                DocsRequest::Close { doc } => match link.hub.clone() {
                    Some(hub) => hub.docs(DocsCall::Close { conn: link.conn, doc }).await.map(|()| true).map_err(|_| End::Hub),
                    None => Ok(true),
                },
                // In order with the connection's changes: the hub keeps it once the changes it covers are in.
                DocsRequest::Saved { doc, hash, frontiers } => match link.hub.clone() {
                    Some(hub) => hub.docs(DocsCall::Saved { conn: link.conn, doc, hash, frontiers }).await.map(|()| true).map_err(|_| End::Hub),
                    None => Ok(true),
                },
                // At once, before the app writes the file: the sync waits for that save's record.
                DocsRequest::Saving { doc, hash } => match link.hub.clone() {
                    Some(hub) => hub.docs(DocsCall::Saving { conn: link.conn, doc, hash }).await.map(|()| true).map_err(|_| End::Hub),
                    None => Ok(true),
                },
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
                        let event = refuse(Some(*seq), format!("an update is at most {MAX_UPDATE_BYTES} bytes"));
                        link.reply(socket, &event).await?;
                        return Ok(true);
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
                        let event = refuse(None, format!("an import is at most {MAX_SNAPSHOT_BYTES} bytes"));
                        link.reply(socket, &event).await?;
                        return Ok(true);
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

/// Sends one of the hub's messages to the app.
async fn send_out(socket: &mut WebSocket, out: DocOut) -> Result<(), Unsent> {
    match out {
        DocOut::Opened { id, doc, state, changed_outside, snapshot, waiting } => {
            send_json(socket, &DocsEvent::Opened { id, doc, state, changed_outside }).await?;
            send_binary(socket, STATE, id, &snapshot).await?;
            for update in &waiting {
                send_binary(socket, UPDATE, id, update).await?;
            }
            Ok(())
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
        DocOut::File { doc, frontiers } => send_json(socket, &DocsEvent::File { doc, frontiers }).await,
    }
}

async fn send_json(socket: &mut WebSocket, event: &DocsEvent) -> Result<(), Unsent> {
    let Ok(text) = serde_json::to_string(event) else { return Ok(()) };
    send(socket, Message::Text(text.into())).await
}

async fn send_binary(socket: &mut WebSocket, kind: u8, id: u64, bytes: &[u8]) -> Result<(), Unsent> {
    send(socket, Message::Binary(encode_binary(kind, id, bytes).into())).await
}

async fn send(socket: &mut WebSocket, message: Message) -> Result<(), Unsent> {
    if FAIL_WRITES.load(Ordering::Relaxed) {
        return Err(Unsent::Closed);
    }
    match tokio::time::timeout(SEND_TIMEOUT, socket.send(message)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(Unsent::Closed),
        Err(_) => Err(Unsent::Slow),
    }
}
