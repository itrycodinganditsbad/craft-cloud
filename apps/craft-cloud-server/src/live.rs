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
//! From live protocol 2 on (a connection whose `hello` names it; one on protocol 1 never sees what follows, and is
//! sent the whole log, compacted rows too):
//!
//! - **Documents of files** are made by import (`create` with a snapshot): the server checks that the ID is the node's
//!   and that the node is a file of the space, stores the snapshot in the chunk store and keeps the first one
//!   ([`craft_cloud_meta::Meta::create_live_document`]); `created` says whether this one was kept.
//! - **Compaction**: once a document's log above its snapshot holds more than [`Limits::compact_updates`] updates or
//!   [`Limits::compact_bytes`], the server asks one device for a snapshot (`compact {doc, upto}`): the one that sent
//!   the update that crossed (its ack says it holds every update up to it once the ones below arrive), or one that just
//!   caught up (its `subscribed` says so). One request per document at a time, server-wide ([`LiveHub`]); a device
//!   that doesn't answer within [`Limits::compact_timeout`] is skipped for [`COMPACT_SKIP`] and the next device to
//!   append or catch up is asked. Only the `snapshot` a session was asked for is taken; it is stored in the chunk store
//!   first (durably), then recorded with its `base` in one metadata transaction, which takes it only past the current
//!   base. The rows below stay.
//! - **Catch-up from a snapshot**: a `subscribe` from below the snapshot (or from 0, for a document made by import) is
//!   sent `snapshot {doc, base, bytes}`, then the log from `base` on; `subscribe_log` never is.
//! - **Presence** is relayed to the document's other subscribers (of any device), never stored, at most
//!   [`Limits::presence_per_sec`] per document per connection (more are dropped).
//!
//! [`Session`] is the protocol, free of the WebSocket: the connection loop ([`connect`]) feeds it frames and
//! notifications and sends what it returns. The agent's recents simulation drives sessions directly.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, State};
use axum::response::Response;
use craft_cloud_meta::DeviceAuth;
use craft_cloud_proto::api::codes;
use craft_cloud_proto::live::{
    AppName, ClientFrame, DOCUMENT_KIND_PREFIX, DocKind, LIVE_DOCUMENTS_PROTOCOL, MAX_FRAME_BYTES, MAX_SUBSCRIPTIONS, MAX_UPDATE_BYTES,
    PROTOCOL_MISMATCH, ServerFrame, node_doc, shared_version, update_hash,
};
use craft_cloud_proto::{DeviceId, DocId, Epoch, ManifestId, NodeId, SpaceId};
use tokio::sync::broadcast;
use tokio::time::Instant;

use crate::error::ApiError;
use crate::state::{AppState, Limits};

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
/// Presence messages a connection may fall behind by; past that the oldest are dropped (presence is ephemeral).
const PRESENCE_CAPACITY: usize = 256;
/// How long a device asked to compact has to answer before another one is asked.
pub const COMPACT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a device that didn't answer a compaction request in time isn't asked again for that document.
pub const COMPACT_SKIP: Duration = Duration::from_secs(600);

/// A presence message on its way to a space's other connections.
#[derive(Debug, Clone)]
pub struct PresenceNote {
    pub doc: DocId,
    pub bytes: Arc<Vec<u8>>,
    /// The session it came from (not sent back to it).
    pub from: u64,
}

/// One document's compaction: the request out (to which session and device, for which `upto`, since when), and the
/// devices that let one time out, until when they are skipped.
#[derive(Debug, Default)]
struct Compaction {
    asked: Option<(u64, DeviceId, Instant)>,
    skipped: HashMap<DeviceId, Instant>,
}

/// Who is connected: per space, a channel naming each document that got an update, and one of presence; per device,
/// its connections and subscriptions; a channel naming each device revoked through the API (its connections end at
/// once); and the compactions under way.
pub struct LiveHub {
    spaces: Mutex<HashMap<SpaceId, broadcast::Sender<DocId>>>,
    presence: Mutex<HashMap<SpaceId, broadcast::Sender<PresenceNote>>>,
    connections: Mutex<HashMap<DeviceId, usize>>,
    subscriptions: Mutex<HashMap<DeviceId, usize>>,
    revoked: broadcast::Sender<DeviceId>,
    compactions: Mutex<HashMap<(SpaceId, DocId), Compaction>>,
    /// Each device's presence budget per document, across its connections.
    presence_budgets: Mutex<HashMap<(DeviceId, DocId), Budget>>,
}

/// Presence budgets kept at most (a full one is as good as none, and goes first).
const MAX_PRESENCE_BUDGETS: usize = 100_000;

impl Default for LiveHub {
    fn default() -> Self {
        Self {
            spaces: Mutex::default(),
            presence: Mutex::default(),
            connections: Mutex::default(),
            subscriptions: Mutex::default(),
            revoked: broadcast::channel(64).0,
            compactions: Mutex::default(),
            presence_budgets: Mutex::default(),
        }
    }
}

/// Sessions are numbered, so a presence message isn't sent back to the one it came from.
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

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

    /// A receiver of the presence messages of `space` from now on.
    pub fn listen_presence(&self, space: SpaceId) -> broadcast::Receiver<PresenceNote> {
        let mut spaces = self.presence.lock().unwrap_or_else(PoisonError::into_inner);
        spaces.entry(space).or_insert_with(|| broadcast::channel(PRESENCE_CAPACITY).0).subscribe()
    }

    /// Hands a presence message to the space's connections (never stored). A space nobody listens to is forgotten.
    pub fn publish_presence(&self, space: SpaceId, note: PresenceNote) {
        let mut spaces = self.presence.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(sender) = spaces.get(&space)
            && sender.send(note).is_err()
        {
            spaces.remove(&space);
        }
    }

    /// Whether `session` of `device` may be asked to compact `doc` now: no request is out for it (or the one out timed
    /// out, and its device is skipped for a while), and this device isn't skipped. If so, the request is recorded.
    fn ask_compaction(&self, space: SpaceId, doc: DocId, session: u64, device: DeviceId, limits: &Limits) -> bool {
        let now = Instant::now();
        let mut all = self.compactions.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = all.entry((space, doc)).or_default();
        if let Some((_, asked, at)) = entry.asked {
            if now.saturating_duration_since(at) < limits.compact_timeout {
                return false;
            }
            entry.skipped.insert(asked, now + COMPACT_SKIP);
            entry.asked = None;
        }
        entry.skipped.retain(|_, until| *until > now);
        if entry.skipped.contains_key(&device) {
            return false;
        }
        entry.asked = Some((session, device, now));
        true
    }

    /// Whether `device` may send one more presence message for `doc` now (`per_sec` a second, across its connections).
    fn spend_presence(&self, device: DeviceId, doc: DocId, per_sec: u32) -> bool {
        let mut all = self.presence_budgets.lock().unwrap_or_else(PoisonError::into_inner);
        if all.len() >= MAX_PRESENCE_BUDGETS {
            // The ones refilled to the full are no different from new ones.
            all.retain(|_, budget| !budget.is_full());
        }
        all.entry((device, doc)).or_insert_with(|| Budget::of(per_sec, per_sec)).spend()
    }

    /// `doc` was compacted: a later request may go to any device again.
    fn compacted(&self, space: SpaceId, doc: DocId) {
        self.compactions.lock().unwrap_or_else(PoisonError::into_inner).remove(&(space, doc));
    }

    /// The request out for `doc` to `session`, if any, is done with (the session ended): another device may be asked.
    fn forget_ask(&self, space: SpaceId, doc: DocId, session: u64) {
        let mut all = self.compactions.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = all.get_mut(&(space, doc)) {
            if entry.asked.is_some_and(|(s, ..)| s == session) {
                entry.asked = None;
            }
            if entry.asked.is_none() && entry.skipped.is_empty() {
                all.remove(&(space, doc));
            }
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
        Self::of(limits.live_burst, limits.live_per_sec)
    }

    fn of(burst: u32, per_sec: u32) -> Self {
        let burst = f64::from(burst);
        Self { tokens: burst, at: Instant::now(), burst, per_sec: f64::from(per_sec) }
    }

    /// Whether it has refilled to the full by now.
    fn is_full(&self) -> bool {
        self.tokens + Instant::now().saturating_duration_since(self.at).as_secs_f64() * self.per_sec >= self.burst
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
    /// The catch-up starts with the document's snapshot if the agent's `after` (this) is below it, or 0.
    snapshot_from: Option<u64>,
    /// …with the document's import instead (`subscribe_log` from 0: the whole log follows, which doesn't hold it).
    import: bool,
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
    /// The live protocol agreed at `hello` (0 before).
    protocol: u32,
    /// This session's number (presence isn't sent back to it).
    id: u64,
    /// The space's epoch named in `welcome`: a connection that outlives an epoch reset ends at its next ping.
    epoch: Option<Epoch>,
    subscriptions: BTreeMap<DocId, Subscription>,
    /// Documents with updates the agent may not have been sent yet.
    dirty: BTreeSet<DocId>,
    budget: Budget,
    /// The `upto` this session's agent was asked to compact each document at: only that snapshot is taken from it.
    asked: BTreeMap<DocId, u64>,
}

impl Drop for Session {
    fn drop(&mut self) {
        release(&self.state.live.subscriptions, self.device, self.subscriptions.len());
        for doc in self.asked.keys() {
            self.state.live.forget_ask(self.space, *doc, self.id);
        }
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
            protocol: 0,
            id: NEXT_SESSION.fetch_add(1, Ordering::Relaxed),
            epoch: None,
            subscriptions: BTreeMap::new(),
            dirty: BTreeSet::new(),
            budget,
            asked: BTreeMap::new(),
        }
    }

    /// Whether the agent speaks documents, snapshots and presence (protocol 2 on).
    fn documents(&self) -> bool {
        self.protocol >= LIVE_DOCUMENTS_PROTOCOL
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
                            self.protocol = protocol;
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
            ClientFrame::Subscribe { doc, after, at } => match self.subscribe(doc, after, at, false).await {
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
            ClientFrame::Update { doc, client_seq, bytes } => match self.update(doc, client_seq, bytes).await {
                Ok((seq, compact)) => {
                    Reply { frames: std::iter::once(ServerFrame::Ack { doc, client_seq, seq }).chain(compact).collect(), close: false }
                }
                Err(e) => one(refused(e, Some(doc), Some(client_seq))),
            },
            // The frames of documents, snapshots and presence are for a connection that speaks them.
            ClientFrame::CreateDocument { doc, .. } | ClientFrame::SubscribeLog { doc, .. } if !self.documents() => {
                one(refused(ApiError::invalid("that frame needs live protocol 2"), Some(doc), None))
            }
            ClientFrame::Snapshot { .. } | ClientFrame::Presence { .. } if !self.documents() => Reply::default(),
            ClientFrame::CreateDocument { doc, node, app, snapshot } => one(self.create_document(doc, node, app, snapshot).await),
            ClientFrame::SubscribeLog { doc, after, at } => match self.subscribe(doc, after, at, true).await {
                Ok(()) => Reply::default(),
                Err(e) => one(refused(e, Some(doc), None)),
            },
            ClientFrame::Snapshot { doc, upto, manifest } => {
                self.snapshot(doc, upto, manifest).await;
                Reply::default()
            }
            ClientFrame::Presence { doc, bytes } => {
                self.presence(doc, bytes);
                Reply::default()
            }
        }
    }

    /// `create {doc, node, app, snapshot}`: the document of a file, made by import from the snapshot `manifest` names
    /// (uploaded through the chunk API already, and charged to the account there). Recorded only for a file of the
    /// space and a manifest of the space, both checked in the same transaction (the file is looked at first too, so a
    /// made-up node costs a read). The first snapshot is kept; a document that is there already answers `kept: false`.
    async fn create_document(&mut self, doc: DocId, node: NodeId, app: AppName, manifest: ManifestId) -> ServerFrame {
        if !self.budget.spend() {
            return refused(ApiError::rate_limited("too many frames; send again in a moment", None), Some(doc), None);
        }
        if node_doc(self.space, node) != doc {
            return refused(ApiError::invalid("that document ID isn't the file's"), Some(doc), None);
        }
        match self.state.meta.live_doc(self.space, doc).await {
            Ok(Some(found)) if found.node == Some(node) && found.kind.starts_with(DOCUMENT_KIND_PREFIX) => {
                return ServerFrame::CreatedDocument { doc, kept: false };
            }
            Ok(Some(_)) => return refused(ApiError::invalid("the document exists with another kind"), Some(doc), None),
            Ok(None) => {}
            Err(e) => return refused(e.into(), Some(doc), None),
        }
        match self.state.meta.is_live_file(self.space, node).await {
            Ok(true) => {}
            Ok(false) => return refused(ApiError::not_found("file"), Some(doc), None),
            Err(e) => return refused(e.into(), Some(doc), None),
        }
        let kind = DocKind::Document { node, app }.as_text();
        match self.state.meta.create_live_document(self.space, doc, &kind, node, &manifest, self.state.now_ms()).await {
            Ok((_, kept)) => ServerFrame::CreatedDocument { doc, kept },
            Err(e) => refused(e.into(), Some(doc), None),
        }
    }

    /// `snapshot {doc, upto, manifest}`: the answer to this session's `compact`, taken only for the `upto` it was
    /// asked, and only for a manifest the account uploaded; recorded past the current base only. Never answered: a
    /// snapshot that isn't taken changes nothing, and another device is asked later.
    async fn snapshot(&mut self, doc: DocId, upto: u64, manifest: ManifestId) {
        if !self.subscriptions.contains_key(&doc) || self.asked.get(&doc) != Some(&upto) {
            return;
        }
        self.asked.remove(&doc);
        match self.state.meta.compact_live_doc(self.space, doc, upto, &manifest).await {
            Ok(true) => self.state.live.compacted(self.space, doc),
            Ok(false) => {}
            Err(e) => tracing::info!(error = %e, "a live document's snapshot wasn't recorded"),
        }
    }

    /// `presence {doc, bytes}`: relayed to the document's other subscribers, within the device's budget for the
    /// document (across its connections) and the connection's frame budget (over either, dropped).
    fn presence(&mut self, doc: DocId, bytes: Vec<u8>) {
        if !self.subscriptions.contains_key(&doc) {
            return;
        }
        if !self.state.live.spend_presence(self.device, doc, self.state.limits.presence_per_sec) || !self.budget.spend() {
            return;
        }
        self.state.live.publish_presence(self.space, PresenceNote { doc, bytes: Arc::new(bytes), from: self.id });
    }

    /// Another connection's presence message: the frame for this one's agent, if it follows the document.
    pub fn relay(&self, note: &PresenceNote) -> Option<ServerFrame> {
        (self.documents() && note.from != self.id && self.subscriptions.contains_key(&note.doc))
            .then(|| ServerFrame::Presence { doc: note.doc, bytes: note.bytes.as_ref().clone() })
    }

    /// The `compact` to ask this session's agent for, if the log above the snapshot is long (`upto - base` updates,
    /// `above` bytes) and no other device is asked already.
    fn maybe_compact(&mut self, doc: DocId, upto: u64, base: u64, above: u64) -> Option<ServerFrame> {
        let limits = &self.state.limits;
        let long = upto.saturating_sub(base) > limits.compact_updates || above > limits.compact_bytes;
        if !self.documents() || !long || self.asked.get(&doc).is_some_and(|asked| *asked >= upto) {
            return None;
        }
        if !self.state.live.ask_compaction(self.space, doc, self.id, self.device, limits) {
            return None;
        }
        self.asked.insert(doc, upto);
        Some(ServerFrame::Compact { doc, upto })
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

    /// `subscribe` (or, with `log_only`, `subscribe_log`): the catch-up from `after`, with the document's snapshot first
    /// when `after` is below it (a connection on protocol 1, and `subscribe_log`, get the whole log instead).
    async fn subscribe(&mut self, doc: DocId, after: u64, at: Option<[u8; 32]>, log_only: bool) -> Result<(), ApiError> {
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
        // A document of a file is for an agent that reads its snapshot: one on protocol 1 never made one.
        if !self.documents() && found.kind.starts_with(DOCUMENT_KIND_PREFIX) {
            if new {
                release(&self.state.live.subscriptions, self.device, 1);
            }
            return Err(ApiError::invalid("documents of files need live protocol 2"));
        }
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
        let sent = after.min(found.head);
        // From below the snapshot (or from 0): the snapshot first. The whole log (`subscribe_log`) from 0 of a document
        // made by import: its import first, which no row holds.
        let import = log_only && sent == 0 && found.kind.starts_with(DOCUMENT_KIND_PREFIX);
        let snapshot_from = (self.documents() && found.has_snapshot && (import || (!log_only && (sent < found.base || sent == 0)))).then_some(sent);
        self.subscriptions.insert(doc, Subscription { sent, catching_up: true, snapshot_from, import });
        self.dirty.insert(doc);
        Ok(())
    }

    /// Appends an update; the `compact` to ask for after its ack, if its log is long now.
    async fn update(&mut self, doc: DocId, client_seq: u64, bytes: Vec<u8>) -> Result<(u64, Option<ServerFrame>), ApiError> {
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
        let appended = tokio::spawn(async move {
            let appended = state.meta.append_live_update_counted(space, doc, device, client_seq, &bytes, state.now_ms()).await?;
            state.live.publish(space, doc);
            Ok::<_, ApiError>(appended)
        })
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "live append task failed");
            ApiError::internal()
        })??;
        // The device that sent the update that made the log long is asked first: it holds every update up to it once
        // those below have reached it.
        let compact = if appended.repeat { None } else { self.maybe_compact(doc, appended.seq, appended.base, appended.above_bytes) };
        Ok((appended.seq, compact))
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
        // The snapshot first, as it is now (a compaction since the `subscribe` only moves it on): alone in its batch,
        // then the log from its base on (the update at the base too, whose hash the agent's next `at` names).
        if let Some(after) = sub.snapshot_from.take() {
            let found = if sub.import {
                self.state.meta.live_import_snapshot(self.space, doc).await?.map(|import| (0, import))
            } else {
                self.state.meta.live_snapshot(self.space, doc).await?
            };
            if let Some((base, stored)) = found.filter(|(base, _)| after < *base || after == 0) {
                // The agent downloads it through the chunk API (one it can't get or read, it follows the whole log).
                sub.sent = base.saturating_sub(1).max(after);
                self.subscriptions.insert(doc, sub);
                return Ok(vec![ServerFrame::Snapshot { doc, base, manifest: stored.manifest }]);
            }
        }
        let (page, full) = self.state.meta.live_page(self.space, doc, sub.sent, PUMP_BYTES, PUMP_PAGE).await?;
        let mut frames = Vec::new();
        for update in page {
            sub.sent = sub.sent.max(update.seq);
            if sub.catching_up || update.device != self.device {
                frames.push(ServerFrame::Update { doc, seq: update.seq, bytes: update.bytes, device: update.device });
            }
        }
        let mut caught_up = false;
        if !full {
            self.dirty.remove(&doc);
            if sub.catching_up {
                sub.catching_up = false;
                caught_up = true;
                frames.push(ServerFrame::Subscribed { doc, head: sub.sent });
            }
        }
        self.subscriptions.insert(doc, sub);
        // An agent that just caught up holds the whole log: it may be asked to compact it.
        if caught_up
            && self.documents()
            && let Some(found) = self.state.meta.live_doc(self.space, doc).await?
            && found.head == sub.sent
        {
            frames.extend(self.maybe_compact(doc, found.head, found.base, found.above_base()));
        }
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
    // No frame is over an update and its envelope (snapshots travel through the chunk API).
    Ok(upgrade.max_message_size(MAX_FRAME_BYTES).max_frame_size(MAX_FRAME_BYTES).on_upgrade(move |socket| async move {
        let _slot = slot;
        // Listening before the session reads anything, so no update falls between a read and the notification.
        let notes = state.live.listen(auth.space);
        let presence = state.live.listen_presence(auth.space);
        serve(socket, Session::new(state, &auth), notes, presence).await;
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
async fn serve(mut socket: WebSocket, mut session: Session, mut notes: broadcast::Receiver<DocId>, mut presence: broadcast::Receiver<PresenceNote>) {
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
            note = presence.recv() => match note {
                Ok(note) => {
                    if let Some(frame) = session.relay(&note)
                        && !send(&mut socket, &frame).await
                    {
                        break;
                    }
                }
                // Presence that fell behind is dropped: the next one says where the cursor is.
                Err(broadcast::error::RecvError::Lagged(_)) => {}
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

    // ---- documents of files, snapshots, compaction and presence (protocol 2) ----

    use craft_cloud_proto::{ChunkId, ChunkRef, ChunkerParamsId, FileHash, Manifest, Name, NewVersion, Op};
    use craft_cloud_store::BlobKey;

    /// One account's state, with devices to open sessions for.
    struct Account {
        state: Arc<AppState>,
        meta: Meta,
        user: craft_cloud_meta::UserRecord,
        next_token: u8,
    }

    impl Account {
        async fn new(limits: crate::state::Limits) -> Self {
            Self::with_meta(limits, Meta::open("sqlite::memory:").await.unwrap(), "ana").await
        }

        /// Another account (`name`) in the same database.
        async fn with_meta(limits: crate::state::Limits, meta: Meta, name: &str) -> Self {
            let user = meta.create_user(name, "$argon2id$stub", Role::Member, 0).await.unwrap();
            let state = Arc::new(AppState::new(meta.clone(), Store::new(MemoryBackend::new()), vec![]).unwrap().with_limits(limits));
            Self { state, meta, user, next_token: 1 }
        }

        /// A new device of the account and a session for it, past a `hello` naming `max`.
        async fn session(&mut self, max: u32) -> (Session, DeviceAuth) {
            let token = [self.next_token; 32];
            self.next_token += 1;
            self.meta.create_device(self.user.id, "pc", &token, 0).await.unwrap();
            let auth = self.meta.device_by_token(&token).await.unwrap().unwrap();
            let mut session = Session::new(self.state.clone(), &auth);
            let reply = session.handle(ClientFrame::Hello { min: 1, max }).await;
            assert!(matches!(reply.frames.as_slice(), [ServerFrame::Welcome { protocol, .. }] if *protocol == max), "{:?}", reply.frames);
            (session, auth)
        }

        /// `bytes` uploaded as the account's through what the chunk API does (each chunk, then the manifest naming
        /// them): what an agent does with a snapshot before the frame that names it.
        async fn upload(&self, bytes: &[u8]) -> ManifestId {
            let space = self.user.space;
            let mut chunks = Vec::new();
            for piece in bytes.chunks(craft_cloud_proto::limits::MAX_CHUNK_BYTES as usize) {
                let id = ChunkId::from_bytes(*blake3::hash(piece).as_bytes());
                self.state.store.put(BlobKey::chunk(&id), piece).await.unwrap();
                self.meta.add_chunk(space, &id, piece.len() as u64 + 1, 0).await.unwrap();
                chunks.push(ChunkRef { id, len: u32::try_from(piece.len()).unwrap() });
            }
            let file_hash = FileHash::from_bytes(*blake3::hash(bytes).as_bytes());
            let manifest = Manifest::new(ChunkerParamsId::new("fastcdc-v1").unwrap(), bytes.len() as u64, file_hash, chunks).unwrap();
            self.state.store.put(BlobKey::manifest(&manifest.id()), &manifest.encode()).await.unwrap();
            self.meta.add_manifest(space, &manifest, 100, 0).await.unwrap();
            manifest.id()
        }

        /// A file of the account's space.
        async fn file(&self, auth: &DeviceAuth, name: &str) -> NodeId {
            let space = self.user.space;
            let chunk = ChunkId::from_bytes(update_hash(name.as_bytes()));
            self.meta.add_chunk(space, &chunk, 11, 0).await.unwrap();
            let manifest =
                Manifest::new(ChunkerParamsId::new("fastcdc-v1").unwrap(), 10, FileHash::from_bytes([1; 32]), vec![ChunkRef { id: chunk, len: 10 }])
                    .unwrap();
            self.meta.add_manifest(space, &manifest, 100, 0).await.unwrap();
            let root = self.meta.space_info(space).await.unwrap().root;
            let node = NodeId::generate();
            let version = NewVersion { id: craft_cloud_proto::VersionId::generate(), manifest: manifest.id(), mtime_ms: 0, exec: false };
            let op = Op::Create { node, parent: root, name: Name::new(name).unwrap(), version };
            self.meta.commit(space, auth.device, &[op], 0).await.unwrap();
            node
        }
    }

    fn app() -> AppName {
        AppName::new("wordcraft").unwrap()
    }

    /// Pumps until nothing is left: every frame the agent would be sent.
    async fn drain(s: &mut Session) -> Vec<ServerFrame> {
        let mut out = Vec::new();
        while s.has_work() {
            out.extend(s.pump().await.unwrap());
        }
        out
    }

    /// Makes the document of a new file by import from `s` (its snapshot uploaded first) and follows it from the start.
    async fn imported(a: &Account, s: &mut Session, auth: &DeviceAuth, snapshot: &[u8]) -> DocId {
        let node = a.file(auth, &format!("f{}.txt", DocId::generate())).await;
        let doc = node_doc(a.user.space, node);
        let snapshot = a.upload(snapshot).await;
        let made = s.handle(ClientFrame::CreateDocument { doc, node, app: app(), snapshot }).await;
        assert_eq!(made.frames, vec![ServerFrame::CreatedDocument { doc, kept: true }]);
        s.handle(ClientFrame::Subscribe { doc, after: 0, at: None }).await;
        drain(s).await;
        doc
    }

    fn updates(frames: &[ServerFrame]) -> Vec<u64> {
        frames.iter().filter_map(|f| if let ServerFrame::Update { seq, .. } = f { Some(*seq) } else { None }).collect()
    }

    /// The snapshot a catch-up named: its base and its manifest.
    fn snapshot_of(frames: &[ServerFrame]) -> Option<(u64, ManifestId)> {
        frames.iter().find_map(|f| if let ServerFrame::Snapshot { base, manifest, .. } = f { Some((*base, *manifest)) } else { None })
    }

    fn compacts(frames: &[ServerFrame]) -> Vec<u64> {
        frames.iter().filter_map(|f| if let ServerFrame::Compact { upto, .. } = f { Some(*upto) } else { None }).collect()
    }

    fn small_logs() -> crate::state::Limits {
        crate::state::Limits { compact_updates: 3, ..Default::default() }
    }

    /// An agent on protocol 1 (one from before documents) works on as it did: it can't make a document of a file nor
    /// follow one, presence and snapshots from it are dropped, it is never asked to compact, and it is sent a compacted
    /// log whole, its compacted rows too.
    #[tokio::test]
    async fn an_agent_on_protocol_1_never_sees_documents() {
        let mut a = Account::new(crate::state::Limits { compact_updates: 1, ..Default::default() }).await;
        let (mut old, auth) = a.session(1).await;
        let node = a.file(&auth, "a.txt").await;
        let doc = node_doc(a.user.space, node);
        let refused = old.handle(ClientFrame::CreateDocument { doc, node, app: app(), snapshot: a.upload(&[1; 10]).await }).await;
        assert_eq!(code(&refused), Some("invalid"));
        let recents = recents_doc(a.user.space, &app());
        old.handle(ClientFrame::Create { doc: recents, kind: DocKind::Recents { app: app() } }).await;
        old.handle(ClientFrame::Subscribe { doc: recents, after: 0, at: None }).await;
        drain(&mut old).await;
        for client_seq in 1..=4 {
            let reply = old.handle(ClientFrame::Update { doc: recents, client_seq, bytes: vec![client_seq as u8; 10] }).await;
            assert!(matches!(reply.frames.as_slice(), [ServerFrame::Ack { .. }]), "never a compact: {:?}", reply.frames);
        }
        assert!(old.handle(ClientFrame::Presence { doc: recents, bytes: vec![1] }).await.frames.is_empty());
        let one = a.upload(&[1]).await;
        assert!(old.handle(ClientFrame::Snapshot { doc: recents, upto: 2, manifest: one }).await.frames.is_empty());
        assert_eq!(a.meta.live_doc(a.user.space, recents).await.unwrap().unwrap().base, 0, "an unasked snapshot changes nothing");
        // Compacted by another (newer) device: the old one still gets every row, never a snapshot.
        let stored = a.upload(&[9; 20]).await;
        assert!(a.meta.compact_live_doc(a.user.space, recents, 3, &stored).await.unwrap());
        let (mut old2, _) = a.session(1).await;
        old2.handle(ClientFrame::Subscribe { doc: recents, after: 0, at: None }).await;
        let frames = drain(&mut old2).await;
        assert_eq!(updates(&frames), vec![1, 2, 3, 4]);
        assert!(snapshot_of(&frames).is_none());
        // Nor may it follow a document of a file.
        let (mut new, auth) = a.session(2).await;
        let doc = imported(&a, &mut new, &auth, &[1; 10]).await;
        assert_eq!(code(&old2.handle(ClientFrame::Subscribe { doc, after: 0, at: None }).await), Some("invalid"));
    }

    /// A document of a file is made by import: the first snapshot is kept, the next one (another machine's import of
    /// the same file) isn't, and a machine that follows it from the start gets the snapshot first.
    #[tokio::test]
    async fn a_document_is_made_by_import_and_the_first_snapshot_kept() {
        let mut a = Account::new(Default::default()).await;
        let (mut pc, pc_auth) = a.session(2).await;
        let (mut laptop, _) = a.session(2).await;
        let node = a.file(&pc_auth, "doc1.txt").await;
        let doc = node_doc(a.user.space, node);
        let (one, two) = (a.upload(&[1; 3000]).await, a.upload(&[2; 3000]).await);
        let first = pc.handle(ClientFrame::CreateDocument { doc, node, app: app(), snapshot: one }).await;
        assert_eq!(first.frames, vec![ServerFrame::CreatedDocument { doc, kept: true }]);
        let second = laptop.handle(ClientFrame::CreateDocument { doc, node, app: app(), snapshot: two }).await;
        assert_eq!(second.frames, vec![ServerFrame::CreatedDocument { doc, kept: false }]);
        laptop.handle(ClientFrame::Subscribe { doc, after: 0, at: None }).await;
        let frames = drain(&mut laptop).await;
        assert_eq!(snapshot_of(&frames), Some((0, one)), "the kept import");
        assert_eq!(frames.last(), Some(&ServerFrame::Subscribed { doc, head: 0 }));
        // The ID must be the node's, the node a file of this account's space.
        let other = a.file(&pc_auth, "doc2.txt").await;
        let wrong = pc.handle(ClientFrame::CreateDocument { doc, node: other, app: app(), snapshot: one }).await;
        assert_eq!(code(&wrong), Some("invalid"));
        let missing = NodeId::generate();
        let gone = pc.handle(ClientFrame::CreateDocument { doc: node_doc(a.user.space, missing), node: missing, app: app(), snapshot: one }).await;
        assert_eq!(code(&gone), Some("not_found"));
        let mut b = Account::with_meta(Default::default(), a.meta.clone(), "bo").await;
        b.next_token = 100;
        let (mut bo, _) = b.session(2).await;
        let bos = b.upload(&[1]).await;
        let theirs = bo.handle(ClientFrame::CreateDocument { doc: node_doc(b.user.space, node), node, app: app(), snapshot: bos }).await;
        assert_eq!(code(&theirs), Some("not_found"), "another account's file is no file of this one");
        assert_eq!(code(&bo.handle(ClientFrame::Subscribe { doc, after: 0, at: None }).await), Some("not_found"), "nor its document");
    }

    /// Snapshots never travel in frames: a `create` or `snapshot` names a manifest the account uploaded (through the
    /// chunk API, charged there), and one naming a manifest it doesn't have (never uploaded, or another account's)
    /// records nothing. A refused `create` stores nothing either: the server writes no blob for a frame.
    #[tokio::test]
    async fn a_snapshot_is_named_by_a_manifest_the_account_uploaded() {
        let meta = Meta::open("sqlite::memory:").await.unwrap().with_stored_bytes_cap(Some(1000));
        let mut a = Account::with_meta(small_logs(), meta, "ana").await;
        let (mut s, auth) = a.session(2).await;
        // Made-up nodes: refused, nothing stored, nothing charged.
        let used = a.meta.stored_bytes(a.user.space).await.unwrap();
        for i in 0..5u8 {
            let node = NodeId::generate();
            let reply = s
                .handle(ClientFrame::CreateDocument {
                    doc: node_doc(a.user.space, node),
                    node,
                    app: app(),
                    snapshot: ManifestId::from_bytes([i; 32]),
                })
                .await;
            assert_eq!(code(&reply), Some("not_found"));
        }
        assert_eq!(a.meta.stored_bytes(a.user.space).await.unwrap(), used);
        // A file, but a manifest never uploaded, or another account's: refused, nothing recorded.
        let node = a.file(&auth, "a.txt").await;
        let doc = node_doc(a.user.space, node);
        let never = ManifestId::from_bytes([3; 32]);
        assert_eq!(code(&s.handle(ClientFrame::CreateDocument { doc, node, app: app(), snapshot: never }).await), Some("not_found"));
        let mut b = Account::with_meta(Default::default(), a.meta.clone(), "bo").await;
        b.next_token = 100;
        let bos = b.upload(&[5; 10]).await;
        assert_eq!(code(&s.handle(ClientFrame::CreateDocument { doc, node, app: app(), snapshot: bos }).await), Some("not_found"));
        assert_eq!(a.meta.live_doc(a.user.space, doc).await.unwrap(), None);
        // A compaction answer naming one it doesn't have isn't taken (and the next ask goes ahead).
        let doc = imported(&a, &mut s, &auth, &[1; 10]).await;
        let mut asked = Vec::new();
        for client_seq in 1..=4u64 {
            asked.extend(compacts(&s.handle(ClientFrame::Update { doc, client_seq, bytes: vec![client_seq as u8; 10] }).await.frames));
        }
        assert_eq!(asked, vec![4]);
        s.handle(ClientFrame::Snapshot { doc, upto: 4, manifest: bos }).await;
        assert_eq!(a.meta.live_doc(a.user.space, doc).await.unwrap().unwrap().base, 0, "another account's manifest isn't taken");
    }

    /// Two machines import the same never-live file at the same moment: one import is kept, and both end on it.
    #[tokio::test]
    async fn two_imports_at_once_keep_one() {
        let mut a = Account::new(Default::default()).await;
        let (mut pc, auth) = a.session(2).await;
        let (mut laptop, _) = a.session(2).await;
        let node = a.file(&auth, "a.txt").await;
        let doc = node_doc(a.user.space, node);
        let (one, two) = (a.upload(&[1; 100]).await, a.upload(&[2; 100]).await);
        let (x, y) = tokio::join!(
            pc.handle(ClientFrame::CreateDocument { doc, node, app: app(), snapshot: one }),
            laptop.handle(ClientFrame::CreateDocument { doc, node, app: app(), snapshot: two })
        );
        let kept: Vec<bool> = [x, y]
            .iter()
            .map(|r| match r.frames.as_slice() {
                [ServerFrame::CreatedDocument { kept, .. }] => *kept,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(kept.iter().filter(|k| **k).count(), 1, "{kept:?}");
        let winner = if kept[0] { one } else { two };
        for s in [&mut pc, &mut laptop] {
            s.handle(ClientFrame::Subscribe { doc, after: 0, at: None }).await;
            assert_eq!(snapshot_of(&drain(s).await), Some((0, winner)), "both end on the kept one");
        }
    }

    /// A log that grows long is compacted by the device whose update made it long: asked once, its snapshot taken for
    /// the `upto` asked, and a machine catching up from below it then gets the snapshot and the log from its base.
    #[tokio::test]
    async fn a_long_log_is_compacted_by_the_device_that_made_it_long() {
        let mut a = Account::new(small_logs()).await;
        let (mut pc, auth) = a.session(2).await;
        let doc = imported(&a, &mut pc, &auth, &[1; 10]).await;
        let mut asked = Vec::new();
        for client_seq in 1..=5u64 {
            let reply = pc.handle(ClientFrame::Update { doc, client_seq, bytes: vec![client_seq as u8; 10] }).await;
            assert!(matches!(reply.frames.first(), Some(ServerFrame::Ack { seq, .. }) if *seq == client_seq), "{:?}", reply.frames);
            asked.extend(compacts(&reply.frames));
        }
        assert_eq!(asked, vec![4], "past 3 updates, asked once");
        // Another `upto` than asked, or the right one twice: only the asked one is taken, once.
        let (s7, s8, s9) = (a.upload(&[7; 40]).await, a.upload(&[8; 40]).await, a.upload(&[9; 41]).await);
        pc.handle(ClientFrame::Snapshot { doc, upto: 5, manifest: s7 }).await;
        assert_eq!(a.meta.live_doc(a.user.space, doc).await.unwrap().unwrap().base, 0);
        pc.handle(ClientFrame::Snapshot { doc, upto: 4, manifest: s8 }).await;
        let compacted = a.meta.live_doc(a.user.space, doc).await.unwrap().unwrap();
        assert_eq!((compacted.base, compacted.above_base(), compacted.snapshot_bytes), (4, 10, 40));
        pc.handle(ClientFrame::Snapshot { doc, upto: 4, manifest: s9 }).await;
        assert_eq!(a.meta.live_snapshot(a.user.space, doc).await.unwrap().unwrap().1.manifest, s8);
        // From the start: the snapshot, then the log from its base (the row at the base gives the next `at`).
        let (mut laptop, _) = a.session(2).await;
        laptop.handle(ClientFrame::Subscribe { doc, after: 0, at: None }).await;
        let frames = drain(&mut laptop).await;
        assert_eq!(snapshot_of(&frames), Some((4, s8)));
        assert_eq!(updates(&frames), vec![4, 5]);
        assert_eq!(frames.last(), Some(&ServerFrame::Subscribed { doc, head: 5 }));
        // From below the base, the hash checked against the kept row.
        let (mut tablet, _) = a.session(2).await;
        let wrong = tablet.handle(ClientFrame::Subscribe { doc, after: 2, at: Some(update_hash(b"other")) }).await;
        assert_eq!(code(&wrong), Some("resync_required"));
        tablet.handle(ClientFrame::Subscribe { doc, after: 2, at: Some(update_hash(&[2; 10])) }).await;
        let frames = drain(&mut tablet).await;
        assert_eq!((snapshot_of(&frames).map(|s| s.0), updates(&frames)), (Some(4), vec![4, 5]));
        // From the base or above: the log only.
        tablet.handle(ClientFrame::Subscribe { doc, after: 4, at: Some(update_hash(&[4; 10])) }).await;
        let frames = drain(&mut tablet).await;
        assert_eq!((snapshot_of(&frames), updates(&frames)), (None, vec![5]));
        // `subscribe_log` never gets the snapshot, but every row, after the import (which no row holds).
        let (mut reader, _) = a.session(2).await;
        reader.handle(ClientFrame::SubscribeLog { doc, after: 0, at: None }).await;
        let frames = drain(&mut reader).await;
        let import = a.meta.live_import_snapshot(a.user.space, doc).await.unwrap().unwrap().manifest;
        assert_eq!((snapshot_of(&frames), updates(&frames)), (Some((0, import)), vec![1, 2, 3, 4, 5]));
        reader.handle(ClientFrame::SubscribeLog { doc, after: 2, at: Some(update_hash(&[2; 10])) }).await;
        let frames = drain(&mut reader).await;
        assert_eq!((snapshot_of(&frames), updates(&frames)), (None, vec![3, 4, 5]), "from 2 it had the import");
    }

    /// A device that just caught up holds the whole log: if it is long and nobody is asked, it is.
    #[tokio::test]
    async fn a_device_that_caught_up_is_asked_when_the_log_is_long() {
        let mut a = Account::new(crate::state::Limits { compact_updates: 3, compact_timeout: Duration::ZERO, ..Default::default() }).await;
        let (mut pc, auth) = a.session(2).await;
        let doc = imported(&a, &mut pc, &auth, &[1; 10]).await;
        for client_seq in 1..=5u64 {
            pc.handle(ClientFrame::Update { doc, client_seq, bytes: vec![client_seq as u8; 10] }).await;
        }
        // The pc was asked (at 4) and never answers: its request times out at once here.
        let (mut laptop, _) = a.session(2).await;
        laptop.handle(ClientFrame::Subscribe { doc, after: 0, at: None }).await;
        let frames = drain(&mut laptop).await;
        assert_eq!(compacts(&frames), vec![5], "asked after `subscribed`");
        assert!(matches!(frames.iter().rev().nth(1), Some(ServerFrame::Subscribed { head: 5, .. })), "{frames:?}");
        laptop.handle(ClientFrame::Snapshot { doc, upto: 5, manifest: a.upload(&[5; 10]).await }).await;
        assert_eq!(a.meta.live_doc(a.user.space, doc).await.unwrap().unwrap().base, 5);
    }

    /// A device that doesn't answer in time is skipped, and the next one to append is asked; a late answer is still
    /// taken if it moves the base on, and an answer for an older `upto` than the base is ignored.
    #[tokio::test]
    async fn an_unanswered_compaction_goes_to_another_device() {
        let mut a = Account::new(crate::state::Limits { compact_updates: 1, compact_timeout: Duration::ZERO, ..Default::default() }).await;
        let (mut pc, auth) = a.session(2).await;
        let (mut laptop, _) = a.session(2).await;
        let doc = imported(&a, &mut pc, &auth, &[1; 10]).await;
        laptop.handle(ClientFrame::Subscribe { doc, after: 0, at: None }).await;
        drain(&mut laptop).await;
        let send = async |s: &mut Session, client_seq: u64| {
            compacts(&s.handle(ClientFrame::Update { doc, client_seq, bytes: vec![client_seq as u8] }).await.frames)
        };
        assert!(send(&mut pc, 1).await.is_empty());
        assert_eq!(send(&mut pc, 2).await, vec![2], "the pc is asked");
        assert_eq!(send(&mut laptop, 1).await, vec![3], "it didn't answer in time: the laptop is asked");
        assert!(send(&mut pc, 3).await.is_empty(), "the pc is skipped for a while");
        // The pc answers late: its snapshot moves the base on, so it is taken.
        pc.handle(ClientFrame::Snapshot { doc, upto: 2, manifest: a.upload(&[2; 10]).await }).await;
        assert_eq!(a.meta.live_doc(a.user.space, doc).await.unwrap().unwrap().base, 2);
        laptop.handle(ClientFrame::Snapshot { doc, upto: 3, manifest: a.upload(&[3; 10]).await }).await;
        assert_eq!(a.meta.live_doc(a.user.space, doc).await.unwrap().unwrap().base, 3, "the laptop's, past it, too");
        // After a compaction anyone may be asked again, the skipped pc too (5 is two above the base).
        assert_eq!(send(&mut pc, 4).await, vec![5]);
    }

    /// Both devices answer at once, with snapshots for different `upto`s: the further one wins, whichever came first,
    /// and a machine from the start still gets a snapshot and the rest of the log.
    #[tokio::test]
    async fn two_devices_answering_at_once_leave_the_furthest_snapshot() {
        let mut a = Account::new(crate::state::Limits { compact_updates: 1, compact_timeout: Duration::ZERO, ..Default::default() }).await;
        let (mut pc, auth) = a.session(2).await;
        let (mut laptop, _) = a.session(2).await;
        let doc = imported(&a, &mut pc, &auth, &[1; 10]).await;
        laptop.handle(ClientFrame::Subscribe { doc, after: 0, at: None }).await;
        drain(&mut laptop).await;
        pc.handle(ClientFrame::Update { doc, client_seq: 1, bytes: vec![1] }).await;
        assert_eq!(compacts(&pc.handle(ClientFrame::Update { doc, client_seq: 2, bytes: vec![2] }).await.frames), vec![2]);
        assert_eq!(compacts(&laptop.handle(ClientFrame::Update { doc, client_seq: 1, bytes: vec![3] }).await.frames), vec![3]);
        let (s3, s2) = (a.upload(&[3; 10]).await, a.upload(&[2; 10]).await);
        tokio::join!(
            laptop.handle(ClientFrame::Snapshot { doc, upto: 3, manifest: s3 }),
            pc.handle(ClientFrame::Snapshot { doc, upto: 2, manifest: s2 })
        );
        assert_eq!(a.meta.live_doc(a.user.space, doc).await.unwrap().unwrap().base, 3);
        let (mut fresh, _) = a.session(2).await;
        fresh.handle(ClientFrame::Subscribe { doc, after: 0, at: None }).await;
        let frames = drain(&mut fresh).await;
        assert_eq!((snapshot_of(&frames), updates(&frames)), (Some((3, s3)), vec![3]));
    }

    /// Past the account's cap on live bytes (every row of every live document, compacted ones too), an update is
    /// refused with `quota_exceeded`, naming it, and nothing is stored.
    #[tokio::test]
    async fn an_update_past_the_accounts_live_bytes_is_refused() {
        let meta = Meta::open("sqlite::memory:").await.unwrap().with_live_bytes_cap(25);
        let mut a = Account::with_meta(small_logs(), meta, "ana").await;
        let (mut pc, auth) = a.session(2).await;
        let doc = imported(&a, &mut pc, &auth, &[1; 10]).await;
        for client_seq in 1..=2u64 {
            let reply = pc.handle(ClientFrame::Update { doc, client_seq, bytes: vec![client_seq as u8; 10] }).await;
            assert!(matches!(reply.frames.first(), Some(ServerFrame::Ack { .. })), "{:?}", reply.frames);
        }
        let reply = pc.handle(ClientFrame::Update { doc, client_seq: 3, bytes: vec![3; 10] }).await;
        assert_eq!(code(&reply), Some("quota_exceeded"));
        assert!(matches!(reply.frames.as_slice(), [ServerFrame::Error { client_seq: Some(3), .. }]));
        assert_eq!(a.meta.live_doc(a.user.space, doc).await.unwrap().unwrap().head, 2, "nothing stored");
    }

    /// A snapshot uploaded but never named (the agent stopped between its upload and its frame) changes nothing: the
    /// document is as it was, and a later compaction works.
    #[tokio::test]
    async fn a_snapshot_uploaded_but_never_named_changes_nothing() {
        let mut a = Account::new(small_logs()).await;
        let (mut pc, auth) = a.session(2).await;
        let doc = imported(&a, &mut pc, &auth, &[1; 10]).await;
        let import = a.meta.live_import_snapshot(a.user.space, doc).await.unwrap().unwrap().manifest;
        for client_seq in 1..=4u64 {
            pc.handle(ClientFrame::Update { doc, client_seq, bytes: vec![client_seq as u8; 10] }).await;
        }
        a.upload(&[6; 50]).await;
        let (mut laptop, _) = a.session(2).await;
        laptop.handle(ClientFrame::Subscribe { doc, after: 0, at: None }).await;
        let frames = drain(&mut laptop).await;
        assert_eq!((snapshot_of(&frames), updates(&frames)), (Some((0, import)), vec![1, 2, 3, 4]), "the import and the whole log");
        pc.handle(ClientFrame::Snapshot { doc, upto: 4, manifest: a.upload(&[4; 50]).await }).await;
        assert_eq!(a.meta.live_doc(a.user.space, doc).await.unwrap().unwrap().base, 4);
    }

    /// Presence goes to the document's other subscribers, never back, never to one that doesn't follow it or speaks
    /// protocol 1, at most so many a second per document; nothing is stored.
    #[tokio::test]
    async fn presence_is_relayed_within_its_limits() {
        let mut a = Account::new(crate::state::Limits { presence_per_sec: 20, ..Default::default() }).await;
        let (mut pc, auth) = a.session(2).await;
        let (mut laptop, _) = a.session(2).await;
        let (mut stranger, _) = a.session(2).await;
        let doc = imported(&a, &mut pc, &auth, &[1; 10]).await;
        laptop.handle(ClientFrame::Subscribe { doc, after: 0, at: None }).await;
        drain(&mut laptop).await;
        let mut heard = a.state.live.listen_presence(a.user.space);
        for i in 0..25u8 {
            assert!(pc.handle(ClientFrame::Presence { doc, bytes: vec![i; 100] }).await.frames.is_empty(), "never answered");
        }
        let mut notes = Vec::new();
        while let Ok(note) = heard.try_recv() {
            notes.push(note);
        }
        assert_eq!(notes.len(), 20, "20 a second, the rest dropped");
        let note = &notes[0];
        assert_eq!(laptop.relay(note), Some(ServerFrame::Presence { doc, bytes: vec![0; 100] }));
        assert_eq!(pc.relay(note), None, "not back to its sender");
        assert_eq!(stranger.relay(note), None, "not to one that doesn't follow it");
        // Presence for a document it doesn't follow is dropped.
        stranger.handle(ClientFrame::Presence { doc, bytes: vec![1] }).await;
        assert!(heard.try_recv().is_err());
        assert_eq!(a.meta.live_doc(a.user.space, doc).await.unwrap().unwrap().head, 0, "nothing stored");
        assert!(a.meta.live_updates(a.user.space, doc, 0, 10).await.unwrap().is_empty());
    }

    /// Presence is budgeted per device and document, across the device's connections: two connections of one device
    /// share its 20 a second.
    #[tokio::test]
    async fn presence_is_budgeted_per_device_across_its_connections() {
        let mut a = Account::new(crate::state::Limits { presence_per_sec: 20, ..Default::default() }).await;
        let (mut pc, auth) = a.session(2).await;
        let doc = imported(&a, &mut pc, &auth, &[1; 10]).await;
        // A second connection of the same device.
        let mut again = Session::new(a.state.clone(), &auth);
        again.handle(ClientFrame::Hello { min: 1, max: 2 }).await;
        again.handle(ClientFrame::Subscribe { doc, after: 0, at: None }).await;
        drain(&mut again).await;
        let mut heard = a.state.live.listen_presence(a.user.space);
        for i in 0..15u8 {
            pc.handle(ClientFrame::Presence { doc, bytes: vec![i] }).await;
            again.handle(ClientFrame::Presence { doc, bytes: vec![i] }).await;
        }
        let mut relayed = 0;
        while heard.try_recv().is_ok() {
            relayed += 1;
        }
        assert_eq!(relayed, 20, "the device's 20, not 20 per connection");
    }

    /// The admin's reset drops the snapshot with the log; a machine that knew the document before (at 3, base 4 now)
    /// is sent the snapshot, and one that knew more than the log holds is told the head, below what it has.
    #[tokio::test]
    async fn restores_and_resets_with_a_snapshot() {
        let mut a = Account::new(small_logs()).await;
        let (mut pc, auth) = a.session(2).await;
        let doc = imported(&a, &mut pc, &auth, &[1; 10]).await;
        for client_seq in 1..=4u64 {
            pc.handle(ClientFrame::Update { doc, client_seq, bytes: vec![client_seq as u8; 10] }).await;
        }
        pc.handle(ClientFrame::Snapshot { doc, upto: 4, manifest: a.upload(&[4; 10]).await }).await;
        let (mut behind, _) = a.session(2).await;
        behind.handle(ClientFrame::Subscribe { doc, after: 3, at: Some(update_hash(&[3; 10])) }).await;
        let frames = drain(&mut behind).await;
        assert_eq!((snapshot_of(&frames).map(|s| s.0), updates(&frames)), (Some(4), vec![4]));
        let (mut ahead, _) = a.session(2).await;
        ahead.handle(ClientFrame::Subscribe { doc, after: 9, at: Some(update_hash(b"nine")) }).await;
        assert_eq!(drain(&mut ahead).await, vec![ServerFrame::Subscribed { doc, head: 4 }], "the head, below what it has: it starts over");
        a.meta.reset_live_doc(a.user.space, doc).await.unwrap();
        let (mut after_reset, _) = a.session(2).await;
        after_reset.handle(ClientFrame::Subscribe { doc, after: 0, at: None }).await;
        assert_eq!(
            drain(&mut after_reset).await,
            vec![ServerFrame::Subscribed { doc, head: 0 }],
            "no snapshot, no log: every machine pushes its own"
        );
    }
}
