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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::{IntoResponse, Response};
use craft_cloud_proto::api::codes as api_codes;
use craft_cloud_proto::live::{MAX_PRESENCE_BYTES, MAX_SNAPSHOT_BYTES, MAX_UPDATE_BYTES};
use craft_cloud_proto::local::docs::{HELD, IMPORT, MAX_BINARY_BYTES, PRESENCE, PRESENCE_FROM, STATE, UPDATE, decode_binary, encode_binary};
use craft_cloud_proto::local::{DocsEvent, DocsRequest, MAX_LOCAL_JSON_BYTES, NotLiveReason};
use craft_cloud_proto::{DocId, SpaceId};
use tokio::sync::mpsc;

use super::hub::{ConnId, DOCS_QUEUE, DocOut, DocsCall, HubHandle};
use crate::local_api::{ApiState, LocalError};

/// A message the app doesn't take within this long drops it.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// Most open numbers one connection may have routed (an app opens a few documents; a hostile one is cut off here).
const MAX_OPENS: usize = 4096;
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
    /// Set when a write to the app failed: nothing is written to it any more, and what it sent before is read until
    /// this time (or its close).
    mute: Option<tokio::time::Instant>,
    /// The app named by the connection's last `open` (for the tests' [`FAIL_WRITES_OF`]).
    app: Option<String>,
}

impl Link {
    fn new(conn: ConnId) -> Self {
        Self {
            conn,
            hubs: Vec::new(),
            from_hub: None,
            to_conn: None,
            seqs: HashMap::new(),
            opens: HashMap::new(),
            docs: HashMap::new(),
            open_docs: HashMap::new(),
            mute: None,
            app: None,
        }
    }

    /// Whether a test made the writes to this connection's app fail.
    fn writes_fail(&self) -> bool {
        let named = FAIL_WRITES_OF.lock().ok().and_then(|n| n.clone());
        named.is_some() && named == self.app
    }

    /// A write to the app failed: stop writing, and read on for a while.
    fn mute_now(&mut self) {
        if self.mute.is_none() {
            self.mute = Some(tokio::time::Instant::now() + read_on());
            // Nothing from the hubs is read any more: they learn that at once, instead of when the queue is full.
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

    /// Routes the open number `id` to the hub `key`; false when the number is in use on this connection (it is never routed
    /// twice: the first open keeps its hub).
    fn begin_open(&mut self, id: u64, key: Key) -> bool {
        if self.opens.contains_key(&id) {
            return false;
        }
        self.opens.insert(id, key);
        true
    }

    /// The open `id` was refused, or answered `not_live`: its number is free again (a number that opened a document is not).
    fn end_open(&mut self, id: u64) {
        if !self.open_docs.contains_key(&id) {
            self.opens.remove(&id);
        }
    }

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
    let mut link = Link::new(NEXT_CONN.fetch_add(1, Ordering::Relaxed));
    link.hub(&api, None).await;
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
                    match &out {
                        DocOut::Opened { id, doc, .. } => {
                            if let Some(key) = link.opens.get(id).copied() {
                                link.docs.insert(*doc, key);
                                link.open_docs.insert(*id, *doc);
                            }
                        }
                        // Not a live document, or the open was refused: its number is free again.
                        DocOut::NotLive { id, .. } | DocOut::Refused { id: Some(id), seq: None, .. } => link.end_open(*id),
                        _ => {}
                    }
                    let result = if link.writes_fail() { Err(Unsent::Closed) } else { send_out(&mut socket, out).await };
                    if let Err(end) = link.sent(result) {
                        break end;
                    }
                }
                // Every hub let go of this connection (it didn't keep up, or the hubs stopped).
                None => break End::Hub,
            },
        }
    };
    // Always, and last: every hub cleans up after every change this connection queued (it may have dropped the connection's
    // output already, which leaves its handles in place for exactly this call).
    for (_, hub) in &link.hubs {
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
                DocsRequest::Open { id, path, app, file_hash, identity } => {
                    if link.opens.contains_key(&id) || link.opens.len() >= MAX_OPENS {
                        let (code, message) = if link.opens.contains_key(&id) {
                            (api_codes::INVALID, "that number is in use on this connection")
                        } else {
                            (api_codes::TOO_LARGE, "too many documents open")
                        };
                        let event = DocsEvent::Error { id: Some(id), seq: None, code: code.into(), message: message.into() };
                        link.reply(socket, &event).await?;
                        return Ok(true);
                    }
                    // An absolute path inside a synced team folder is that space's; any other is the personal space's.
                    let key = api.control.route_path(&path).map(|team| team.space);
                    let not_live = |reason, message: &str| DocsEvent::NotLive { id, reason, message: Some(message.into()) };
                    // (A handle this connection kept of a hub that has stopped since fails once; the space may have a new hub.)
                    for attempt in 0..2 {
                        let Some(hub) = link.hub(api, key).await else {
                            let event = match key {
                                Some(_) => not_live(NotLiveReason::Offline, "the space doesn't sync on this computer"),
                                None => not_live(NotLiveReason::NotSignedIn, "the Craft Cloud agent isn't signed in"),
                            };
                            link.reply(socket, &event).await?;
                            return Ok(true);
                        };
                        link.begin_open(id, key);
                        let call =
                            DocsCall::Open { conn: link.conn, id, path: path.clone(), app: app.clone(), file_hash: file_hash.clone(), identity };
                        if link.call(&hub, key, call).await? {
                            return Ok(true);
                        }
                        if attempt == 1 {
                            break;
                        }
                    }
                    link.reply(socket, &not_live(NotLiveReason::Offline, "the space stopped syncing on this computer")).await?;
                    Ok(true)
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
                    Some((key, hub)) => {
                        if link.call(&hub, key, DocsCall::Saved { conn: link.conn, doc, hash, frontiers }).await? {
                            Ok(true)
                        } else {
                            link.reply(socket, &stopped_event(None, None)).await?;
                            Ok(true)
                        }
                    }
                    None => Ok(true),
                },
                // At once, before the app writes the file: the sync waits for that save's record.
                DocsRequest::Saving { doc, hash } => match link.hub_of_doc(doc) {
                    Some((key, hub)) => {
                        if link.call(&hub, key, DocsCall::Saving { conn: link.conn, doc, hash }).await? {
                            Ok(true)
                        } else {
                            link.reply(socket, &stopped_event(None, None)).await?;
                            Ok(true)
                        }
                    }
                    None => Ok(true),
                },
            }
        }
        Message::Binary(bytes) => {
            // A frame without a header, or of a kind an app never sends: the app is broken or hostile.
            let Some((kind, id, body)) = decode_binary(&bytes) else { return Ok(false) };
            let Some((key, hub)) = link.hub_of(id) else {
                // No hub for it: signed out, or never opened (or its space stopped syncing); a kind an app sends is let be.
                if kind == HELD {
                    crate::runlog::note(format_args!("a change an app handed over was dropped: no hub follows its document any more"));
                }
                return Ok(kind == UPDATE || kind == PRESENCE || kind == IMPORT || kind == HELD);
            };
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
                HELD => {
                    let seq = link.seqs.entry(id).or_insert(0);
                    *seq += 1;
                    if body.is_empty() || body.len() > MAX_UPDATE_BYTES {
                        let event = refuse(Some(*seq), format!("a held change is 1 to {MAX_UPDATE_BYTES} bytes"));
                        link.reply(socket, &event).await?;
                        return Ok(true);
                    }
                    DocsCall::Hold { conn: link.conn, id, seq: *seq, bytes: body.to_vec() }
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
            let update_seq = if let DocsCall::Update { seq, .. } | DocsCall::Hold { seq, .. } = &call { Some(*seq) } else { None };
            let ephemeral = matches!(call, DocsCall::Presence { .. });
            let held = matches!(call, DocsCall::Hold { .. });
            if link.call(&hub, key, call).await? {
                return Ok(true);
            }
            if held {
                // Nothing keeps it now (the space stopped syncing): said in the log, without names.
                crate::runlog::note(format_args!("a change an app handed over was dropped: the space of its document stopped syncing"));
            }
            if ephemeral {
                return Ok(true);
            }
            // The space's hub has stopped (it stopped syncing): the change was not kept, and the app is told.
            link.reply(socket, &stopped_event(Some(id), update_seq)).await?;
            Ok(true)
        }
        Message::Ping(_) | Message::Pong(_) => Ok(true),
        Message::Close(_) => Ok(false),
    }
}

/// What an app is told when it sends to a document of a space whose hub has stopped (nothing is kept).
fn stopped_event(id: Option<u64>, seq: Option<u64>) -> DocsEvent {
    DocsEvent::Error { id, seq, code: api_codes::NOT_FOUND.into(), message: "the space stopped syncing on this computer: nothing was kept".into() }
}

/// Sends one of the hub's messages to the app.
async fn send_out(socket: &mut WebSocket, out: DocOut) -> Result<(), Unsent> {
    match out {
        DocOut::Opened { id, doc, state, changed_outside, read_only, space, snapshot, waiting } => {
            send_json(socket, &DocsEvent::Opened { id, doc, state, changed_outside, read_only, space: Some(space) }).await?;
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
        DocOut::Role { doc, read_only } => send_json(socket, &DocsEvent::Role { doc, read_only }).await,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// An open number is routed once: a second open under it is refused before it can change the route, a refused or
    /// `not_live` open frees its number, and a number that opened a document stays.
    #[test]
    fn open_numbers_are_routed_once_and_freed_when_refused() {
        let (team, other) = (Some(SpaceId::generate()), Some(SpaceId::generate()));
        let mut link = Link::new(1);
        assert!(link.begin_open(5, team));
        assert!(!link.begin_open(5, other), "in use");
        assert_eq!(link.opens.get(&5), Some(&team), "the first route stays");
        link.end_open(5);
        assert!(link.begin_open(5, None), "a refused open frees its number");
        assert_eq!(link.opens.get(&5), Some(&None));
        let doc = DocId::generate();
        link.open_docs.insert(5, doc);
        link.end_open(5);
        assert!(link.opens.contains_key(&5), "a number that opened a document is kept until it is closed");
    }

    #[test]
    fn a_stopped_space_answers_with_an_error_naming_the_change() {
        let event = stopped_event(Some(3), Some(7));
        assert!(matches!(event, DocsEvent::Error { id: Some(3), seq: Some(7), ref code, .. } if code == api_codes::NOT_FOUND));
    }
}
