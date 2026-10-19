//! The live service: `GET /api/v1/live`, upgraded to a WebSocket (the protocol is in `craft-cloud-proto::live`).
//!
//! - **Sign-in**: the device token, checked like every request before the upgrade; a device keeps at most
//!   [`MAX_CONNECTIONS_PER_DEVICE`] live connections. A device revoked through the API loses them at once
//!   ([`LiveHub::revoke`]); one revoked by another process (the admin CLI) at its next ping, when every connection
//!   checks its device again.
//! - **`welcome`** names the space's epoch, so an agent sees a restore from a backup even after the log grew back; a
//!   connection open across an epoch reset ends at its next ping (`resync_required`). Before the reset, `subscribe`'s
//!   `at` catches a restore: the update at `after` isn't the one the agent has there.
//! - **Documents and log**: in the metadata database (`live_docs`, `live_updates`); an update is acked only once its
//!   transaction committed. The server never parses an update.
//! - **Fan-out**: an append wakes the space's connections ([`LiveHub::publish`]); each one reads what its agent lacks
//!   from the log, in order, from where it stopped, in pages of at most [`PUMP_BYTES`] (and [`PUMP_PAGE`] updates).
//!   Nothing is held per update in memory, so a slow connection never loses an update: it reads it later. The device
//!   that sent an update gets the ack instead of the update.
//! - **Limits**: an update at most 1 MiB (also the WebSocket's message limit), at most
//!   [`MAX_SUBSCRIPTIONS`] documents per device, one catch-up per document at a time, and a budget of
//!   [`UPDATE_BURST`] frames (updates, creates and subscribes) refilled at [`UPDATES_PER_SEC`] per connection; over it
//!   the answer is `rate_limited`, and the agent sends again later. A document's log holds at most
//!   [`craft_cloud_meta::MAX_LIVE_DOC_BYTES`] (16 MiB); once it holds that, every update is `quota_exceeded` (one
//!   way) until the admin empties it (`live reset-doc`, which also raises the space's live generation: the live epoch
//!   in `welcome` changes, so every connection of the space ends at its next ping however far the log grew back; the
//!   file sync's epoch doesn't).
//! - A document of another account is `not_found`, exactly like one that doesn't exist.
//!
//! [`Session`] is the protocol, free of the WebSocket: the connection loop ([`connect`]) feeds it frames and
//! notifications and sends what it returns. The agent's recents simulation drives sessions directly.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, State};
use axum::response::Response;
use craft_cloud_meta::DeviceAuth;
use craft_cloud_proto::api::codes;
use craft_cloud_proto::live::{
    ClientFrame, DocKind, MAX_FRAME_BYTES, MAX_SUBSCRIPTIONS, MAX_UPDATE_BYTES, PROTOCOL_MISMATCH, ServerFrame, shared_version, update_hash,
};
use craft_cloud_proto::{DeviceId, DocId, Epoch, SpaceId};
use tokio::sync::broadcast;
use tokio::time::Instant;

use crate::error::ApiError;
use crate::state::AppState;

/// Live connections one device may hold at once (a reconnect may overlap the old connection for a moment).
pub const MAX_CONNECTIONS_PER_DEVICE: usize = 4;
/// Updates (and creates) a connection may send at once before it is refilled…
pub const UPDATE_BURST: u32 = 400;
/// …at this many per second.
pub const UPDATES_PER_SEC: u32 = 200;
/// Updates read from the log for one frame batch at most: a subscription catching up sends its log in pages, so a
/// long log never sits in memory at once…
pub const PUMP_PAGE: u32 = 256;
/// …and at most this many bytes of them (always at least one update): a page of big updates stays a few MiB.
pub const PUMP_BYTES: u64 = 4 * 1024 * 1024;
/// The agent must say `hello` this soon after connecting.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// The server pings this often…
const PING_EVERY: Duration = Duration::from_secs(30);
/// …and closes a connection it heard nothing from (not even a pong) for this long.
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// A frame the agent doesn't read within this long closes the connection.
const SEND_TIMEOUT: Duration = Duration::from_secs(60);
/// Notifications a connection may fall behind by before it rereads all its documents.
const NOTIFY_CAPACITY: usize = 256;

/// Who is connected: per space, a channel naming each document that got an update; per device, its connections and
/// subscriptions; and a channel naming each device revoked through the API (its connections end at once).
pub struct LiveHub {
    spaces: Mutex<HashMap<SpaceId, broadcast::Sender<DocId>>>,
    connections: Mutex<HashMap<DeviceId, usize>>,
    subscriptions: Mutex<HashMap<DeviceId, usize>>,
    revoked: broadcast::Sender<DeviceId>,
}

impl Default for LiveHub {
    fn default() -> Self {
        Self { spaces: Mutex::default(), connections: Mutex::default(), subscriptions: Mutex::default(), revoked: broadcast::channel(64).0 }
    }
}

/// One of a device's live connections; frees its slot when dropped.
pub struct ConnectionSlot {
    hub: Arc<AppState>,
    device: DeviceId,
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        release(&self.hub.live.connections, self.device, 1);
    }
}

fn release(counts: &Mutex<HashMap<DeviceId, usize>>, device: DeviceId, n: usize) {
    let mut counts = counts.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(count) = counts.get_mut(&device) {
        *count = count.saturating_sub(n);
        if *count == 0 {
            counts.remove(&device);
        }
    }
}

/// Takes one of `max` slots of `device` in `counts`; false when all are taken.
fn take(counts: &Mutex<HashMap<DeviceId, usize>>, device: DeviceId, max: usize) -> bool {
    let mut counts = counts.lock().unwrap_or_else(PoisonError::into_inner);
    let count = counts.entry(device).or_insert(0);
    if *count >= max {
        return false;
    }
    *count += 1;
    true
}

impl LiveHub {
    /// `device` was revoked: its live connections end. (One revoked by another process, the admin CLI, is found by the
    /// check every connection makes at each ping.)
    pub fn revoke(&self, device: DeviceId) {
        let _ = self.revoked.send(device);
    }

    /// A receiver of the devices revoked from now on.
    pub fn revocations(&self) -> broadcast::Receiver<DeviceId> {
        self.revoked.subscribe()
    }

    /// A receiver of the documents of `space` that get an update from now on.
    pub fn listen(&self, space: SpaceId) -> broadcast::Receiver<DocId> {
        let mut spaces = self.spaces.lock().unwrap_or_else(PoisonError::into_inner);
        spaces.entry(space).or_insert_with(|| broadcast::channel(NOTIFY_CAPACITY).0).subscribe()
    }

    /// Wakes the space's connections: `doc` has a new update. A space nobody listens to is forgotten.
    pub fn publish(&self, space: SpaceId, doc: DocId) {
        let mut spaces = self.spaces.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(sender) = spaces.get(&space)
            && sender.send(doc).is_err()
        {
            spaces.remove(&space);
        }
    }
}

/// A token bucket: `burst` at once, refilled at `per_sec`.
struct Budget {
    tokens: f64,
    at: Instant,
    burst: f64,
    per_sec: f64,
}

impl Budget {
    fn new(limits: &crate::state::Limits) -> Self {
        let burst = f64::from(limits.live_burst);
        Self { tokens: burst, at: Instant::now(), burst, per_sec: f64::from(limits.live_per_sec) }
    }

    fn spend(&mut self) -> bool {
        let now = Instant::now();
        let refill = now.saturating_duration_since(self.at).as_secs_f64() * self.per_sec;
        self.tokens = (self.tokens + refill).min(self.burst);
        self.at = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// A subscription: what the connection has sent of the document's log.
#[derive(Debug, Clone, Copy)]
struct Subscription {
    /// Every update up to here was sent (or is the agent's own, acked to it).
    sent: u64,
    /// The catch-up after `subscribe` is under way: the agent's own updates are sent too (it may have lost them), and
    /// `subscribed` follows once the log is read to its end.
    catching_up: bool,
}

/// What [`Session::handle`] answers.
#[derive(Debug, Default)]
pub struct Reply {
    pub frames: Vec<ServerFrame>,
    /// The connection ends after these frames (a version mismatch, a frame before `hello`).
    pub close: bool,
}

/// One live connection's protocol state: its device, its subscriptions and what it has sent of each.
pub struct Session {
    state: Arc<AppState>,
    space: SpaceId,
    device: DeviceId,
    welcomed: bool,
    /// The space's epoch named in `welcome`: a connection that outlives an epoch reset ends at its next ping.
    epoch: Option<Epoch>,
    subscriptions: BTreeMap<DocId, Subscription>,
    /// Documents with updates the agent may not have been sent yet.
    dirty: BTreeSet<DocId>,
    budget: Budget,
}

impl Drop for Session {
    fn drop(&mut self) {
        release(&self.state.live.subscriptions, self.device, self.subscriptions.len());
    }
}

fn refused(e: ApiError, doc: Option<DocId>, client_seq: Option<u64>) -> ServerFrame {
    ServerFrame::error(e.code, e.message, doc, client_seq)
}

impl Session {
    /// A session for the signed-in device `auth`. The caller listens to the space ([`LiveHub::listen`]) before the
    /// session reads anything, and hands it what it hears ([`Session::note`], [`Session::lagged`]).
    pub fn new(state: Arc<AppState>, auth: &DeviceAuth) -> Self {
        let budget = Budget::new(&state.limits);
        Self {
            state,
            space: auth.space,
            device: auth.device,
            welcomed: false,
            epoch: None,
            subscriptions: BTreeMap::new(),
            dirty: BTreeSet::new(),
            budget,
        }
    }

    /// Handles one frame from the agent.
    pub async fn handle(&mut self, frame: ClientFrame) -> Reply {
        let one = |frame| Reply { frames: vec![frame], close: false };
        if !self.welcomed {
            return match frame {
                ClientFrame::Hello { min, max } => match shared_version(min, max) {
                    Some(protocol) => match self.state.meta.live_epoch(self.space).await {
                        Ok(epoch) => {
                            self.welcomed = true;
                            self.epoch = Some(epoch);
                            one(ServerFrame::Welcome { protocol, epoch })
                        }
                        Err(e) => {
                            let e = ApiError::from(e);
                            Reply { frames: vec![ServerFrame::error(e.code, e.message, None, None)], close: true }
                        }
                    },
                    None => Reply {
                        frames: vec![ServerFrame::error(
                            PROTOCOL_MISMATCH,
                            format!(
                                "the server speaks live protocol {}-{}, the agent {min}-{max}: update the older one",
                                craft_cloud_proto::live::LIVE_PROTOCOL_MIN,
                                craft_cloud_proto::live::LIVE_PROTOCOL_MAX
                            ),
                            None,
                            None,
                        )],
                        close: true,
                    },
                },
                _ => Reply { frames: vec![ServerFrame::error(codes::INVALID, "say hello first", None, None)], close: true },
            };
        }
        match frame {
            ClientFrame::Hello { .. } => one(ServerFrame::error(codes::INVALID, "hello was said already", None, None)),
            ClientFrame::Create { doc, kind } => one(self.create(doc, kind).await),
            ClientFrame::Subscribe { doc, after, at } => match self.subscribe(doc, after, at).await {
                Ok(()) => Reply::default(),
                Err(e) => one(refused(e, Some(doc), None)),
            },
            ClientFrame::Unsubscribe { doc } => {
                if self.subscriptions.remove(&doc).is_some() {
                    release(&self.state.live.subscriptions, self.device, 1);
                }
                self.dirty.remove(&doc);
                Reply::default()
            }
            ClientFrame::Update { doc, client_seq, bytes } => one(match self.update(doc, client_seq, bytes).await {
                Ok(seq) => ServerFrame::Ack { doc, client_seq, seq },
                Err(e) => refused(e, Some(doc), Some(client_seq)),
            }),
            ClientFrame::CreateDocument { doc, .. }
            | ClientFrame::Snapshot { doc, .. }
            | ClientFrame::Presence { doc, .. }
            | ClientFrame::SubscribeLog { doc, .. } => one(refused(ApiError::invalid("this server doesn't take that frame yet"), Some(doc), None)),
        }
    }

    async fn create(&mut self, doc: DocId, kind: DocKind) -> ServerFrame {
        if !self.budget.spend() {
            return refused(ApiError::rate_limited("too many frames; send again in a moment", None), Some(doc), None);
        }
        // Today every kind fixes its document's ID (a recents document's is derived from the space and the app), so
        // one made offline on two machines is one document.
        if kind.fixed_id(self.space) != Some(doc) {
            return refused(ApiError::invalid("that document ID doesn't belong to that kind"), Some(doc), None);
        }
        match self.state.meta.create_live_doc(self.space, doc, &kind.as_text(), None, self.state.now_ms()).await {
            Ok(_) => ServerFrame::Created { doc },
            Err(e) => refused(e.into(), Some(doc), None),
        }
    }

    async fn subscribe(&mut self, doc: DocId, after: u64, at: Option<[u8; 32]>) -> Result<(), ApiError> {
        // A subscribe costs a read of the log: it comes out of the same budget as updates.
        if !self.budget.spend() {
            return Err(ApiError::rate_limited("too many frames; send again in a moment", None));
        }
        // One catch-up per document at a time: a subscribe repeated meanwhile would restart it, again and again.
        if self.subscriptions.get(&doc).is_some_and(|s| s.catching_up) {
            return Err(ApiError::invalid("this document's catch-up is under way; subscribe again after `subscribed`"));
        }
        let new = !self.subscriptions.contains_key(&doc);
        if new && !take(&self.state.live.subscriptions, self.device, MAX_SUBSCRIPTIONS) {
            return Err(ApiError::rate_limited(format!("a device may follow at most {MAX_SUBSCRIPTIONS} documents"), None));
        }
        let found = match self.state.meta.live_doc(self.space, doc).await {
            Ok(found) => found,
            Err(e) => {
                if new {
                    release(&self.state.live.subscriptions, self.device, 1);
                }
                return Err(e.into());
            }
        };
        let Some(found) = found else {
            if new {
                release(&self.state.live.subscriptions, self.device, 1);
            }
            return Err(ApiError::not_found("document"));
        };
        // The update the agent has at `after` must be the one the log has there: if it isn't (the server was restored
        // from a backup and its epoch not reset yet), the agent catches up from the start and sends its whole document.
        if let Some(at) = at
            && after > 0
            && after <= found.head
        {
            let ours = match self.state.meta.live_page(self.space, doc, after - 1, 0, 1).await {
                Ok((page, _)) => page.first().filter(|u| u.seq == after).map(|u| update_hash(&u.bytes)),
                Err(e) => {
                    if new {
                        release(&self.state.live.subscriptions, self.device, 1);
                    }
                    return Err(e.into());
                }
            };
            if ours != Some(at) {
                if new {
                    release(&self.state.live.subscriptions, self.device, 1);
                }
                return Err(ApiError::new(
                    axum::http::StatusCode::GONE,
                    codes::RESYNC_REQUIRED,
                    "the server's log isn't the one this machine read (it was restored); catch up from the start",
                ));
            }
        }
        // An agent that holds more than the log (the server was restored from a backup) is told the log's end in
        // `subscribed` and sends its own state up again.
        self.subscriptions.insert(doc, Subscription { sent: after.min(found.head), catching_up: true });
        self.dirty.insert(doc);
        Ok(())
    }

    async fn update(&mut self, doc: DocId, client_seq: u64, bytes: Vec<u8>) -> Result<u64, ApiError> {
        if !self.subscriptions.contains_key(&doc) {
            return Err(ApiError::not_found("subscribed document"));
        }
        if bytes.is_empty() || bytes.len() > MAX_UPDATE_BYTES {
            return Err(ApiError::too_large(format!("an update is 1 to {MAX_UPDATE_BYTES} bytes")));
        }
        if !self.budget.spend() {
            return Err(ApiError::rate_limited("too many updates; send again in a moment", None));
        }
        // Appended and published in a task of its own: a connection that drops meanwhile must not skip the publish of
        // an update that was stored.
        let (state, space, device) = (self.state.clone(), self.space, self.device);
        tokio::spawn(async move {
            let seq = state.meta.append_live_update(space, doc, device, client_seq, &bytes, state.now_ms()).await?;
            state.live.publish(space, doc);
            Ok::<_, ApiError>(seq)
        })
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "live append task failed");
            ApiError::internal()
        })?
    }

    /// `doc` got an update: if it is subscribed, the next [`Session::pump`]s send what the agent lacks.
    pub fn note(&mut self, doc: DocId) {
        if self.subscriptions.contains_key(&doc) {
            self.dirty.insert(doc);
        }
    }

    /// Notifications were missed (the connection fell behind): every subscribed document is read again.
    pub fn lagged(&mut self) {
        self.dirty.extend(self.subscriptions.keys().copied());
    }

    /// The check every connection makes at each ping: `Some(the frame to close with)` when the connection must end. A
    /// device revoked by another process (the admin CLI) is found here, at the latest one ping later; so is a space
    /// whose live epoch changed (a restore from a backup, or a `live reset-doc`) while the connection was open: the agent
    /// connects again and learns the new one in `welcome`.
    pub async fn check(&mut self) -> Option<ServerFrame> {
        if !self.state.meta.device_active(self.device).await.unwrap_or(true) {
            return Some(ServerFrame::error(codes::UNAUTHORIZED, "this device was signed out", None, None));
        }
        if let Some(epoch) = self.epoch
            && let Ok(now) = self.state.meta.live_epoch(self.space).await
            && now != epoch
        {
            return Some(ServerFrame::error(
                codes::RESYNC_REQUIRED,
                "the server was restored from a backup, or a live document reset; connect again",
                None,
                None,
            ));
        }
        None
    }

    /// Whether a document has updates the agent may lack ([`Session::pump`] sends them).
    pub fn has_work(&self) -> bool {
        !self.dirty.is_empty()
    }

    /// The next page of updates the agent lacks, of one document: its own updates are skipped once it caught up (it
    /// got their acks), and `subscribed` follows the end of a catch-up. Empty when nothing is left to send.
    pub async fn pump(&mut self) -> Result<Vec<ServerFrame>, ApiError> {
        let Some(doc) = self.dirty.first().copied() else { return Ok(Vec::new()) };
        let Some(mut sub) = self.subscriptions.get(&doc).copied() else {
            self.dirty.remove(&doc);
            return Ok(Vec::new());
        };
        let (page, full) = self.state.meta.live_page(self.space, doc, sub.sent, PUMP_BYTES, PUMP_PAGE).await?;
        let mut frames = Vec::new();
        for update in page {
            sub.sent = sub.sent.max(update.seq);
            if sub.catching_up || update.device != self.device {
                frames.push(ServerFrame::Update { doc, seq: update.seq, bytes: update.bytes, device: update.device });
            }
        }
        if !full {
            self.dirty.remove(&doc);
            if sub.catching_up {
                sub.catching_up = false;
                frames.push(ServerFrame::Subscribed { doc, head: sub.sent });
            }
        }
        self.subscriptions.insert(doc, sub);
        Ok(frames)
    }
}

/// `GET /api/v1/live`: signed in already (the device token, before the upgrade), the request becomes a WebSocket.
pub async fn connect(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<DeviceAuth>,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Result<Response, ApiError> {
    let upgrade = upgrade.map_err(|e| ApiError::new(e.status(), codes::INVALID, e.body_text()))?;
    if !take(&state.live.connections, auth.device, MAX_CONNECTIONS_PER_DEVICE) {
        return Err(ApiError::rate_limited(format!("a device may hold at most {MAX_CONNECTIONS_PER_DEVICE} live connections"), Some(5)));
    }
    let slot = ConnectionSlot { hub: state.clone(), device: auth.device };
    Ok(upgrade.max_message_size(MAX_FRAME_BYTES).max_frame_size(MAX_FRAME_BYTES).on_upgrade(move |socket| async move {
        let _slot = slot;
        // Listening before the session reads anything, so no update falls between a read and the notification.
        let notes = state.live.listen(auth.space);
        serve(socket, Session::new(state, &auth), notes).await;
    }))
}

/// Sends one frame, within [`SEND_TIMEOUT`]; false when the connection is done.
async fn send(socket: &mut WebSocket, frame: &ServerFrame) -> bool {
    let bytes = match frame.encode() {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::error!(error = %e, "a live frame couldn't be encoded");
            return false;
        }
    };
    matches!(tokio::time::timeout(SEND_TIMEOUT, socket.send(Message::Binary(bytes.into()))).await, Ok(Ok(())))
}

async fn send_all(socket: &mut WebSocket, frames: &[ServerFrame]) -> bool {
    for frame in frames {
        if !send(socket, frame).await {
            return false;
        }
    }
    true
}

/// The connection loop: frames in, frames out, the log pumped while a document has news, pings, and an end on
/// shutdown, silence or a broken connection.
async fn serve(mut socket: WebSocket, mut session: Session, mut notes: broadcast::Receiver<DocId>) {
    let mut stopping = session.state.notifier.shutdown_signal();
    let mut revocations = session.state.live.revocations();
    let mut last_heard = Instant::now();
    let mut ping = tokio::time::interval_at(Instant::now() + PING_EVERY, PING_EVERY);
    let hello_by = Instant::now() + HELLO_TIMEOUT;
    loop {
        let quiet_until = if session.welcomed { last_heard + IDLE_TIMEOUT } else { hello_by };
        tokio::select! {
            biased;
            () = async { drop(stopping.wait_for(|stop| *stop).await) } => break,
            message = socket.recv() => {
                last_heard = Instant::now();
                let frame = match message {
                    Some(Ok(Message::Binary(bytes))) => ClientFrame::decode(&bytes),
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    Some(Ok(Message::Text(_))) => Err(craft_cloud_proto::live::FrameError::Invalid("frames are binary")),
                    Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                };
                let reply = match frame {
                    Ok(frame) => session.handle(frame).await,
                    Err(e) => Reply { frames: vec![ServerFrame::error(codes::INVALID, e.to_string(), None, None)], close: true },
                };
                if !send_all(&mut socket, &reply.frames).await || reply.close {
                    break;
                }
            }
            note = notes.recv() => match note {
                Ok(doc) => session.note(doc),
                Err(broadcast::error::RecvError::Lagged(_)) => session.lagged(),
                // The hub keeps the sender while anyone listens to the space, and this connection does.
                Err(broadcast::error::RecvError::Closed) => break,
            },
            revoked = revocations.recv() => {
                let ours = match revoked {
                    Ok(device) => device == session.device,
                    // Missed some: look the device up.
                    Err(broadcast::error::RecvError::Lagged(_)) => !session.state.meta.device_active(session.device).await.unwrap_or(true),
                    Err(broadcast::error::RecvError::Closed) => false,
                };
                if ours {
                    let _ = send(&mut socket, &ServerFrame::error(codes::UNAUTHORIZED, "this device was signed out", None, None)).await;
                    break;
                }
            }
            _ = ping.tick() => {
                if let Some(frame) = session.check().await {
                    let _ = send(&mut socket, &frame).await;
                    break;
                }
                if tokio::time::timeout(SEND_TIMEOUT, socket.send(Message::Ping(Vec::new().into()))).await.map_or(true, |sent| sent.is_err()) {
                    break;
                }
            }
            () = tokio::time::sleep_until(quiet_until) => break,
            // Lowest priority: send what a subscribed document's log holds that the agent lacks, a page at a time.
            () = std::future::ready(()), if session.has_work() => match session.pump().await {
                Ok(frames) => {
                    if !send_all(&mut socket, &frames).await {
                        break;
                    }
                }
                Err(e) => {
                    let _ = send(&mut socket, &ServerFrame::error(e.code, e.message, None, None)).await;
                    break;
                }
            },
        }
    }
    let _ = socket.send(Message::Close(None)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use craft_cloud_meta::{Meta, Role};
    use craft_cloud_proto::live::{AppName, recents_doc};
    use craft_cloud_store::Store;
    use craft_cloud_store::memory::MemoryBackend;

    /// A session of a fresh account's device, past `hello`, with its recents document for `wordcraft` made.
    async fn session(limits: crate::state::Limits) -> (Session, DocId, Meta, DeviceAuth) {
        session_capped(limits, craft_cloud_meta::MAX_LIVE_DOC_BYTES).await
    }

    async fn session_capped(limits: crate::state::Limits, cap: u64) -> (Session, DocId, Meta, DeviceAuth) {
        session_on(limits, Meta::open("sqlite::memory:").await.unwrap().with_live_doc_cap(cap)).await
    }

    async fn session_on(limits: crate::state::Limits, meta: Meta) -> (Session, DocId, Meta, DeviceAuth) {
        let user = meta.create_user("ana", "$argon2id$stub", Role::Member, 0).await.unwrap();
        meta.create_device(user.id, "pc", &[7; 32], 0).await.unwrap();
        let auth = meta.device_by_token(&[7; 32]).await.unwrap().unwrap();
        let state = Arc::new(AppState::new(meta.clone(), Store::new(MemoryBackend::new()), vec![]).unwrap().with_limits(limits));
        let mut session = Session::new(state, &auth);
        session.handle(ClientFrame::Hello { min: 1, max: 1 }).await;
        let app = AppName::new("wordcraft").unwrap();
        let doc = recents_doc(auth.space, &app);
        let reply = session.handle(ClientFrame::Create { doc, kind: DocKind::Recents { app } }).await;
        assert_eq!(reply.frames, vec![ServerFrame::Created { doc }]);
        (session, doc, meta, auth)
    }

    fn code(reply: &Reply) -> Option<&str> {
        match reply.frames.as_slice() {
            [ServerFrame::Error { code, .. }] => Some(code.as_str()),
            _ => None,
        }
    }

    /// A subscribe repeated while its catch-up runs is refused (it would restart the catch-up again and
    /// again), and subscribes come out of the frame budget like updates.
    #[tokio::test]
    async fn subscribes_are_budgeted_and_one_catch_up_runs_at_a_time() {
        let (mut s, doc, ..) = session(crate::state::Limits { live_burst: 3, live_per_sec: 0, ..Default::default() }).await;
        // The create took one of three.
        assert!(s.handle(ClientFrame::Subscribe { doc, after: 0, at: None }).await.frames.is_empty());
        let again = s.handle(ClientFrame::Subscribe { doc, after: 0, at: None }).await;
        assert_eq!(code(&again), Some("invalid"), "while the catch-up runs");
        assert_eq!(s.pump().await.unwrap(), vec![ServerFrame::Subscribed { doc, head: 0 }]);
        let budget = s.handle(ClientFrame::Subscribe { doc, after: 0, at: None }).await;
        assert_eq!(code(&budget), Some("rate_limited"), "three frames, then the budget is spent");
    }

    /// A catch-up page is cut at about 4 MiB, not at a number of updates (64 updates of 1 MiB would be 64 MiB in memory
    /// at once).
    #[tokio::test]
    async fn a_catch_up_page_is_bounded_by_bytes() {
        let (mut s, doc, meta, auth) = session(crate::state::Limits::default()).await;
        for i in 1..=6u64 {
            meta.append_live_update(auth.space, doc, auth.device, i, &vec![1; MAX_UPDATE_BYTES], 0).await.unwrap();
        }
        // Another device subscribes from the start (its own updates would be skipped live, never in a catch-up).
        s.handle(ClientFrame::Subscribe { doc, after: 0, at: None }).await;
        let first = s.pump().await.unwrap();
        let bytes: usize = first.iter().map(|f| if let ServerFrame::Update { bytes, .. } = f { bytes.len() } else { 0 }).sum();
        assert_eq!(first.len(), 4, "four 1 MiB updates fit in 4 MiB");
        assert!(bytes as u64 <= PUMP_BYTES);
        let rest = s.pump().await.unwrap();
        assert_eq!(rest.len(), 3, "the other two, then `subscribed`");
        assert_eq!(rest.last(), Some(&ServerFrame::Subscribed { doc, head: 6 }));
    }

    /// The epoch is checked at each ping, not only at `hello`: a connection open across an epoch reset ends.
    #[tokio::test]
    async fn an_epoch_reset_ends_open_connections_at_the_next_ping() {
        let (mut s, _, meta, auth) = session(crate::state::Limits::default()).await;
        assert_eq!(s.check().await, None);
        meta.reset_epoch(auth.space).await.unwrap();
        let Some(ServerFrame::Error { code, .. }) = s.check().await else { panic!("the connection goes on") };
        assert_eq!(code, "resync_required");
    }

    /// `subscribe` names the hash of the update the agent has at `after`;
    /// a log with another update there (restored, its epoch not reset yet) answers `resync_required`.
    #[tokio::test]
    async fn subscribe_checks_the_update_at_after() {
        let (mut s, doc, meta, auth) = session(crate::state::Limits::default()).await;
        meta.append_live_update(auth.space, doc, auth.device, 1, b"one", 0).await.unwrap();
        meta.append_live_update(auth.space, doc, auth.device, 2, b"two", 0).await.unwrap();
        let wrong = s.handle(ClientFrame::Subscribe { doc, after: 2, at: Some(update_hash(b"other")) }).await;
        assert_eq!(code(&wrong), Some("resync_required"));
        assert!(s.handle(ClientFrame::Subscribe { doc, after: 2, at: Some(update_hash(b"two")) }).await.frames.is_empty(), "the same");
        assert_eq!(s.pump().await.unwrap(), vec![ServerFrame::Subscribed { doc, head: 2 }]);
    }

    /// A document's log is capped by bytes, and an update over it is refused as
    /// `quota_exceeded`, naming the update.
    #[tokio::test]
    async fn a_full_document_refuses_updates_with_quota_exceeded() {
        let (mut s, doc, ..) = session_capped(crate::state::Limits::default(), 1500).await;
        s.handle(ClientFrame::Subscribe { doc, after: 0, at: None }).await;
        for client_seq in 1..=2 {
            let ok = s.handle(ClientFrame::Update { doc, client_seq, bytes: vec![1; 1000] }).await;
            assert!(matches!(ok.frames.as_slice(), [ServerFrame::Ack { .. }]), "{ok:?}");
        }
        let full = s.handle(ClientFrame::Update { doc, client_seq: 3, bytes: vec![2; 10] }).await;
        let [ServerFrame::Error { code, message, client_seq: Some(3), .. }] = full.frames.as_slice() else { panic!("{full:?}") };
        assert_eq!(code, "quota_exceeded");
        assert!(message.contains("full"), "{message}");
    }

    /// The first update to a document over the account's cap on documents holding updates is refused as
    /// `quota_exceeded`, naming the document and the update (the shape the agent's full-document handling reads); the
    /// documents that hold updates go on.
    #[tokio::test]
    async fn an_update_to_one_document_too_many_is_refused_naming_the_update() {
        let meta = Meta::open("sqlite::memory:").await.unwrap().with_live_docs_cap(1);
        let (mut s, first, ..) = session_on(crate::state::Limits::default(), meta).await;
        let app = AppName::new("sheetcraft").unwrap();
        let second = recents_doc(s.space, &app);
        let made = s.handle(ClientFrame::Create { doc: second, kind: DocKind::Recents { app } }).await;
        assert_eq!(made.frames, vec![ServerFrame::Created { doc: second }], "a create isn't capped by it");
        for doc in [first, second] {
            s.handle(ClientFrame::Subscribe { doc, after: 0, at: None }).await;
        }
        let ok = s.handle(ClientFrame::Update { doc: first, client_seq: 1, bytes: vec![1; 10] }).await;
        assert!(matches!(ok.frames.as_slice(), [ServerFrame::Ack { client_seq: 1, seq: 1, .. }]), "{ok:?}");
        let refused = s.handle(ClientFrame::Update { doc: second, client_seq: 1, bytes: vec![2; 10] }).await;
        let [ServerFrame::Error { code, doc: Some(named), client_seq: Some(1), .. }] = refused.frames.as_slice() else { panic!("{refused:?}") };
        assert_eq!((code.as_str(), *named), ("quota_exceeded", second));
        let more = s.handle(ClientFrame::Update { doc: first, client_seq: 2, bytes: vec![1; 10] }).await;
        assert!(matches!(more.frames.as_slice(), [ServerFrame::Ack { seq: 2, .. }]), "{more:?}");
    }

    /// A create over the account's cap on documents is refused as `quota_exceeded`, naming the document and no update.
    #[tokio::test]
    async fn a_create_over_the_row_cap_is_refused_naming_only_the_document() {
        let meta = Meta::open("sqlite::memory:").await.unwrap().with_live_doc_rows_cap(1);
        let (mut s, ..) = session_on(crate::state::Limits::default(), meta).await;
        let app = AppName::new("sheetcraft").unwrap();
        let second = recents_doc(s.space, &app);
        let refused = s.handle(ClientFrame::Create { doc: second, kind: DocKind::Recents { app } }).await;
        let [ServerFrame::Error { code, doc: Some(named), client_seq: None, .. }] = refused.frames.as_slice() else { panic!("{refused:?}") };
        assert_eq!((code.as_str(), *named), ("quota_exceeded", second));
    }

    /// The admin's reset empties a full document's log; a connection that followed it ends at its next ping, so its
    /// agent connects again and sends up what the log lacks.
    #[tokio::test]
    async fn a_reset_document_ends_the_connections_that_follow_it() {
        let (mut s, doc, meta, auth) = session(crate::state::Limits::default()).await;
        meta.append_live_update(auth.space, doc, auth.device, 1, b"one", 0).await.unwrap();
        s.handle(ClientFrame::Subscribe { doc, after: 0, at: None }).await;
        while s.has_work() {
            s.pump().await.unwrap();
        }
        assert_eq!(s.check().await, None);
        meta.reset_live_doc(auth.space, doc).await.unwrap();
        let Some(ServerFrame::Error { code, .. }) = s.check().await else { panic!("the connection goes on") };
        assert_eq!(code, "resync_required");
    }

    /// A reset is seen at the next ping even when the log grew back past what the connection was sent: it is a new
    /// live generation, which the ping compares.
    #[tokio::test]
    async fn a_reset_is_seen_after_the_log_grew_back() {
        let (mut s, doc, meta, auth) = session(crate::state::Limits::default()).await;
        meta.append_live_update(auth.space, doc, auth.device, 1, b"one", 0).await.unwrap();
        s.handle(ClientFrame::Subscribe { doc, after: 0, at: None }).await;
        while s.has_work() {
            s.pump().await.unwrap();
        }
        meta.reset_live_doc(auth.space, doc).await.unwrap();
        for i in 2..=4 {
            meta.append_live_update(auth.space, doc, auth.device, i, &[i as u8; 3], 0).await.unwrap();
        }
        let Some(ServerFrame::Error { code, .. }) = s.check().await else { panic!("the reset went unseen") };
        assert_eq!(code, "resync_required");
    }
}
