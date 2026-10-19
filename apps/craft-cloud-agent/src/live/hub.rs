//! The live hub's task: owns the account's [`Replica`], answers the local API's recents and documents calls, and keeps
//! the live connection to the server up while the agent is signed in (connecting again with backoff, from 1 s doubling
//! to 5 minutes with jitter; every 10 minutes for a server that speaks another live protocol). Offline it keeps
//! working: adds and edits are kept on disk and reach the server once the connection is back. While syncing is paused
//! the connection is held the same way (closed, and not dialed until resumed).
//!
//! **Documents of files** ([`DocsCall`]): an app opens a path; the hub finds its node in the sync state (a file not
//! uploaded yet gets a sync pass started for it, and the open waits up to [`NODE_WAIT`] for its node), and opens its
//! document through the replica. Its state goes to the app, and then every remote change and presence as they come,
//! and to the document's other apps on this machine every change one of them makes. A document's status (online, how
//! many changes wait, whether its file was deleted) goes to its apps when it changes. Nothing the hub sends an app ever
//! waits for the app: a connection whose queue is full is dropped.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use craft_cloud_proto::DocId;
use craft_cloud_proto::live::{AppName, ClientFrame, MAX_SNAPSHOT_BYTES, MAX_SUBSCRIPTIONS, PRESENCE_PER_SEC, ServerFrame};
use craft_cloud_proto::local::{DocState, MAX_MACHINE_BYTES, MAX_TITLE_BYTES, NewRecent, NotLiveReason, RecentEntry, recent_path};
use craft_cloud_proto::{NodeId, SpaceId};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;

use super::blobs::{Upload, download, upload};
use super::link::{DialError, LiveSocket, dial};
use super::replica::{AddError, DocEvent, LinkError, LocalError, Opening, Replica};
use super::store::DiskStore;
use crate::account::DataDir;
use crate::http::{DeviceToken, HttpTransport, ServerUrl};
use crate::local_api::Control;
use craft_cloud_proto::ManifestId;

/// What the hub needs to know about the account.
#[derive(Debug, Clone)]
pub struct HubConfig {
    pub data: DataDir,
    pub space: SpaceId,
    pub server: ServerUrl,
    pub token: DeviceToken,
    /// This machine's name in the entries it adds (the device name).
    pub machine: String,
    /// The synced folder (the paths apps open documents by are in it).
    pub folder: PathBuf,
}

/// How long an open of a file that isn't uploaded yet waits for its node (a sync pass is started for it).
pub const NODE_WAIT: Duration = Duration::from_secs(5);
/// How often the hub looks again for the nodes opens wait for.
const NODE_LOOK_EVERY: Duration = Duration::from_millis(100);
/// How often documents of files are written whole (their local changes are in their journals at once).
const DOCUMENTS_FLUSH_EVERY: Duration = Duration::from_secs(2);
/// Messages one docs connection may have queued before it is dropped (an app that doesn't read).
pub const DOCS_QUEUE: usize = 1024;

/// One app connection of the docs WebSocket.
pub type ConnId = u64;

/// What the hub sends a docs connection, in order.
#[derive(Debug, Clone)]
pub enum DocOut {
    /// The document opened as `id` is live: its state (a snapshot, then updates still waiting for their history), and
    /// whether the file the app read was edited outside Craft.
    Opened { id: u64, doc: DocId, state: DocState, changed_outside: Option<bool>, snapshot: Vec<u8>, waiting: Vec<Vec<u8>> },
    /// Send the file's content as an import.
    NeedsImport { id: u64 },
    /// The file opens as a plain file.
    NotLive { id: u64, reason: NotLiveReason, message: String },
    /// The app's update `seq` of `id` is on disk.
    Ack { id: u64, seq: u64 },
    /// A request was refused.
    Refused { id: Option<u64>, seq: Option<u64>, code: &'static str, message: String },
    /// A change from another machine or app.
    Update { id: u64, bytes: Arc<Vec<u8>> },
    /// Presence from another machine or app.
    Presence { id: u64, bytes: Arc<Vec<u8>> },
    /// How the document stands now.
    Status { doc: DocId, online: bool, waiting: u64, deleted: bool, stopped: bool },
    /// The record of the document's file as it is on disk now (the live document's frontiers it was saved from).
    File { doc: DocId, frontiers: Option<Vec<(u64, u32)>> },
}

/// What a docs connection asks the hub.
#[derive(Debug)]
pub enum DocsCall {
    /// A new connection: what the hub sends it goes to `out`.
    Connect {
        conn: ConnId,
        out: mpsc::Sender<DocOut>,
    },
    Open {
        conn: ConnId,
        id: u64,
        path: String,
        app: String,
        /// The hash (hex) of the file's bytes as the app read them.
        file_hash: Option<String>,
    },
    /// The app is about to write `doc`'s file: content `hash` (hex), from the document at `frontiers`.
    Saved {
        conn: ConnId,
        doc: DocId,
        hash: String,
        frontiers: Vec<(u64, u32)>,
    },
    Import {
        conn: ConnId,
        id: u64,
        bytes: Vec<u8>,
    },
    Update {
        conn: ConnId,
        id: u64,
        seq: u64,
        bytes: Vec<u8>,
    },
    Presence {
        conn: ConnId,
        id: u64,
        bytes: Vec<u8>,
    },
    Close {
        conn: ConnId,
        doc: DocId,
    },
    /// The connection ended.
    Gone {
        conn: ConnId,
    },
}

/// Why a recents call was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HubError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Failed(String),
    #[error("the agent stopped following recents (signed out, or stopping)")]
    Gone,
}

/// An app's list as its followers see it: the entries, and whether its push stopped ([`Replica::push_stopped`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    pub entries: Vec<RecentEntry>,
    pub push_stopped: bool,
}

enum Request {
    Follow {
        app: AppName,
        reply: oneshot::Sender<Result<watch::Receiver<Listing>, HubError>>,
    },
    Add {
        app: AppName,
        entry: NewRecent,
        reply: oneshot::Sender<Result<RecentEntry, HubError>>,
    },
    /// Hold the live connection (closed, and not dialed) or let it run again; answered once done.
    Pause(bool, oneshot::Sender<()>),
    /// A docs connection's request (answered on its own channel).
    Docs(DocsCall),
}

/// How the local API reaches the hub. Cheap to clone.
#[derive(Clone)]
pub struct HubHandle {
    tx: mpsc::Sender<Request>,
}

impl HubHandle {
    /// `app`'s list now and after every change. The receiver closes when the hub stops.
    pub async fn follow(&self, app: AppName) -> Result<watch::Receiver<Listing>, HubError> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(Request::Follow { app, reply }).await.map_err(|_| HubError::Gone)?;
        rx.await.map_err(|_| HubError::Gone)?
    }

    /// Adds a file to `app`'s list: on disk when this returns.
    pub async fn add(&self, app: AppName, entry: NewRecent) -> Result<RecentEntry, HubError> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(Request::Add { app, entry, reply }).await.map_err(|_| HubError::Gone)?;
        rx.await.map_err(|_| HubError::Gone)?
    }

    /// Holds the live connection while syncing is paused (`true`): it is closed and not dialed again until `false`.
    /// Follows and adds keep working meanwhile; adds reach the server once it runs again.
    pub async fn set_paused(&self, paused: bool) -> Result<(), HubError> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(Request::Pause(paused, reply)).await.map_err(|_| HubError::Gone)?;
        rx.await.map_err(|_| HubError::Gone)
    }

    /// Hands a docs connection's request to the hub (its answers come on the connection's channel).
    pub async fn docs(&self, call: DocsCall) -> Result<(), HubError> {
        self.tx.send(Request::Docs(call)).await.map_err(|_| HubError::Gone)
    }
}

/// The running hub; dropping it stops it (what it was told is on disk already). It runs on a thread of its own with
/// its own runtime, so its disk writes (each add is written and flushed before it is answered) never hold up the
/// `run` loop or the local API.
pub struct Hub {
    handle: HubHandle,
    stop: Option<oneshot::Sender<()>>,
}

impl Hub {
    /// Opens the account's documents and starts the hub, with its live connection held if `paused` (see
    /// [`HubHandle::set_paused`]). `control` hears whether the live connection is up.
    pub fn start(config: HubConfig, control: Control, paused: bool) -> std::io::Result<Self> {
        let dir = config.data.path().join("live").join(config.space.to_string());
        let replica = Replica::open(DiskStore::open(&dir)?, config.space, super::replica::random_peers())?;
        let (tx, rx) = mpsc::channel(64);
        let (stop, stopped) = oneshot::channel();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        std::thread::Builder::new()
            .name("craft-cloud-live".into())
            .spawn(move || runtime.block_on(run(config, replica, rx, control, stopped, paused)))?;
        Ok(Self { handle: HubHandle { tx }, stop: Some(stop) })
    }

    pub fn handle(&self) -> HubHandle {
        self.handle.clone()
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        // The hub's thread ends after the request it is on (handles held by open sockets don't keep it).
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

/// Finds a file's node in the sync state (`state.json`, read again only when it changed).
struct NodeLookup {
    data: DataDir,
    seen: Option<(std::time::SystemTime, u64)>,
    state: Option<craft_cloud_sync::State>,
}

impl NodeLookup {
    /// The sync state, read again if `state.json` changed.
    fn state(&mut self) -> Option<&craft_cloud_sync::State> {
        let file = self.data.path().join(crate::account::STATE_FILE);
        let stamp = std::fs::metadata(&file).ok().and_then(|m| Some((m.modified().ok()?, m.len())));
        if stamp != self.seen {
            self.seen = stamp;
            self.state = self.data.load_state().ok().map(|s| s.state);
        }
        self.state.as_ref()
    }

    fn node(&mut self, path: &str) -> Option<NodeId> {
        self.state()?.base_at(&craft_cloud_sync::rel(path))
    }

    /// The node of the file at `path`, if the sync knows one there (a folder is none).
    fn file_node(&mut self, path: &str) -> Option<NodeId> {
        let state = self.state()?;
        let node = state.base_at(&craft_cloud_sync::rel(path))?;
        matches!(state.base.get(&node).map(|e| &e.kind), Some(craft_cloud_sync::BaseKind::File { .. })).then_some(node)
    }

    /// Whether the sync still has the file node `node` (`false` once it was deleted, here or elsewhere).
    fn has_file(&mut self, node: NodeId) -> bool {
        // (Unknown: no state to look in. Not said to be deleted.)
        self.state().is_none_or(|state| state.base.contains_key(&node))
    }
}

/// A token bucket for one app's presence in one document: [`PRESENCE_PER_SEC`] a second.
struct PresenceBudget {
    tokens: f64,
    at: Instant,
}

impl PresenceBudget {
    fn new() -> Self {
        Self { tokens: f64::from(PRESENCE_PER_SEC), at: Instant::now() }
    }

    fn spend(&mut self) -> bool {
        let now = Instant::now();
        let rate = f64::from(PRESENCE_PER_SEC);
        self.tokens = (self.tokens + now.saturating_duration_since(self.at).as_secs_f64() * rate).min(rate);
        self.at = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// One docs connection: where its messages go, and the documents it has open (by its own numbers).
struct DocsConn {
    out: mpsc::Sender<DocOut>,
    handles: BTreeMap<u64, DocId>,
    presence: HashMap<u64, PresenceBudget>,
}

/// An open waiting for its file's node.
struct Lookup {
    conn: ConnId,
    id: u64,
    path: String,
    app: AppName,
    until: Instant,
}

/// The hub's documents of files: connections, opens waiting, imports asked for, and the status each was last told.
#[derive(Default)]
struct Docs {
    conns: HashMap<ConnId, DocsConn>,
    /// Opens waiting for a document's join, in order.
    waiting: BTreeMap<DocId, Vec<(ConnId, u64)>>,
    /// The open asked for a document's import.
    importer: BTreeMap<DocId, (ConnId, u64)>,
    lookups: Vec<Lookup>,
    told: BTreeMap<DocId, (bool, u64, bool, bool)>,
    /// Opens not answered yet: the hash (hex) of the file each read.
    opens: HashMap<(ConnId, u64), Option<String>>,
    /// Each open document's file as last looked at: its length and modification time then, its content's hash, and
    /// the record its apps were last told.
    files: HashMap<DocId, FileSeen>,
    /// Save records that came before the changes they cover: kept once those are in this machine's copy, dropped when
    /// their connection closes the document first.
    early: Vec<EarlyRecord>,
}

/// A save record waiting for its operations ([`Docs::early`]).
struct EarlyRecord {
    conn: ConnId,
    doc: DocId,
    hash: String,
    frontiers: Vec<(u64, u32)>,
}

/// A document's file as last looked at ([`State::tell_files`]).
#[derive(Default)]
struct FileSeen {
    stamp: Option<(u64, std::time::SystemTime)>,
    hash: Option<craft_cloud_proto::FileHash>,
    told: Option<Option<Vec<(u64, u32)>>>,
}

/// How often the files of open documents are looked at (whether a save, here or synced from elsewhere, changed them).
const FILES_LOOK_EVERY: Duration = Duration::from_secs(1);

/// `text` cut to `max` bytes at a character boundary, without control characters.
fn clean(text: &str, max: usize) -> String {
    let mut out = String::new();
    for c in text.chars().filter(|c| !c.is_control()) {
        if out.len() + c.len_utf8() > max {
            break;
        }
        out.push(c);
    }
    out
}

fn random_key() -> std::io::Result<[u8; 16]> {
    let mut key = [0u8; 16];
    getrandom::fill(&mut key).map_err(|e| std::io::Error::other(format!("the system random source failed: {e}")))?;
    Ok(key)
}

fn jitter(delay: Duration) -> Duration {
    let mut bytes = [0u8; 8];
    let _ = getrandom::fill(&mut bytes);
    let half = u64::try_from((delay / 2).as_millis()).unwrap_or(u64::MAX).max(1);
    delay / 2 + Duration::from_millis(u64::from_le_bytes(bytes) % half)
}

const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(300);
const RETRY_MISMATCH: Duration = Duration::from_secs(600);
/// While updates wait for an ack, the ones refused for a moment are sent again this often.
const RESEND_EVERY: Duration = Duration::from_secs(2);
const PING_EVERY: Duration = Duration::from_secs(30);
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// What a snapshot's upload or download says when it is done (for the connection `generation` it was for).
enum BlobDone {
    Uploaded { generation: u64, doc: DocId, manifest: ManifestId, result: Result<(), String> },
    Downloaded { generation: u64, doc: DocId, manifest: ManifestId, result: Result<Vec<u8>, String> },
}

/// Snapshots on their way up or down through the chunk API, and the frames waiting for them.
struct Blobs {
    /// The chunk API (`None` if it couldn't be set up: every upload and download then fails, and is said).
    transport: Option<Arc<HttpTransport>>,
    done: mpsc::UnboundedSender<BlobDone>,
    /// The connection the transfers under way are for: one that ended makes their results no news.
    generation: u64,
    /// Uploads under way, by document: the document's frames wait behind them, in order.
    uploading: BTreeMap<DocId, Vec<ManifestId>>,
    held: BTreeMap<DocId, Vec<ClientFrame>>,
}

/// The document a frame is about, if any.
fn frame_doc(frame: &ClientFrame) -> Option<DocId> {
    match frame {
        ClientFrame::Hello { .. } => None,
        ClientFrame::Create { doc, .. }
        | ClientFrame::Subscribe { doc, .. }
        | ClientFrame::Unsubscribe { doc }
        | ClientFrame::Update { doc, .. }
        | ClientFrame::Snapshot { doc, .. }
        | ClientFrame::Presence { doc, .. }
        | ClientFrame::SubscribeLog { doc, .. }
        | ClientFrame::CreateDocument { doc, .. } => Some(*doc),
    }
}

/// Whether `frame` names the snapshot `manifest`.
fn names(frame: &ClientFrame, manifest: ManifestId) -> bool {
    matches!(frame, ClientFrame::Snapshot { manifest: m, .. } | ClientFrame::CreateDocument { snapshot: m, .. } if *m == manifest)
}

struct State {
    config: HubConfig,
    replica: Replica<DiskStore>,
    blobs: Blobs,
    lookup: NodeLookup,
    lists: BTreeMap<AppName, watch::Sender<Listing>>,
    socket: Option<Conn>,
    control: Control,
    said: Option<String>,
    /// When to dial again, and the backoff that grows with every connection lost (reset once welcomed).
    retry_at: Instant,
    delay: Duration,
    /// Syncing is paused: no connection is dialed (see [`HubHandle::set_paused`]).
    paused: bool,
    docs: Docs,
    /// What apps on this machine saved (for whether a file opened was edited outside Craft).
    saves: super::saves::SaveBook,
}

/// A refusal sent to an app.
fn refusal(id: Option<u64>, seq: Option<u64>, code: &'static str, message: impl Into<String>) -> DocOut {
    DocOut::Refused { id, seq, code, message: message.into() }
}

impl State {
    /// Waits `wait` (or the backoff) before dialing again, and grows the backoff.
    fn back_off(&mut self, wait: Option<Duration>) {
        self.retry_at = Instant::now() + wait.unwrap_or_else(|| jitter(self.delay));
        self.delay = self.delay.saturating_mul(2).min(RETRY_MAX);
    }
    /// Tells the user `line` once (not again until something else was said).
    fn say(&mut self, line: String) {
        if self.said.as_ref() != Some(&line) {
            crate::runlog::note(format_args!("{line}"));
            self.said = Some(line);
        }
    }

    /// Hands the changed lists to their followers and what happened to documents to their apps, says the notes and
    /// writes what changed. Frames to send (an event may call for some).
    fn settle(&mut self) -> Vec<ClientFrame> {
        for (app, entries) in self.replica.take_changed() {
            if let Some(list) = self.lists.get(&app) {
                let push_stopped = self.replica.push_stopped(&app);
                list.send_replace(Listing { entries, push_stopped });
            }
        }
        let frames = self.doc_events();
        // Remote changes may be what a record waits for.
        self.take_early();
        self.tell_status();
        for note in self.replica.take_notes() {
            self.say(note);
        }
        if let Err(e) = self.replica.flush() {
            self.say(format!("the live documents couldn't be written to disk: {e}"));
        }
        self.control.set_online(self.replica.is_up());
        frames
    }

    /// Sends `message` to docs connection `conn`; one that can't take it (it doesn't read, or it is gone) is dropped.
    fn tell(&mut self, conn: ConnId, message: DocOut) -> Vec<ClientFrame> {
        let Some(c) = self.docs.conns.get(&conn) else { return Vec::new() };
        if c.out.try_send(message).is_ok() {
            return Vec::new();
        }
        self.gone(conn)
    }

    /// The opens of `doc`: every connection and number it is open as.
    fn handles_of(&self, doc: DocId) -> Vec<(ConnId, u64)> {
        let mut out = Vec::new();
        for (conn, c) in &self.docs.conns {
            out.extend(c.handles.iter().filter(|(_, d)| **d == doc).map(|(id, _)| (*conn, *id)));
        }
        out.sort_unstable();
        out
    }

    /// Hands what happened to documents of files to the apps that have them open or wait for them.
    fn doc_events(&mut self) -> Vec<ClientFrame> {
        let mut frames = Vec::new();
        for event in self.replica.take_events() {
            match event {
                DocEvent::Joined(doc) => {
                    self.docs.importer.remove(&doc);
                    for (conn, id) in self.docs.waiting.remove(&doc).unwrap_or_default() {
                        frames.extend(self.deliver(conn, id, doc));
                    }
                }
                DocEvent::NeedsImport(doc) => frames.extend(self.ask_import(doc)),
                DocEvent::NotLive(doc, reason, message) => {
                    self.docs.importer.remove(&doc);
                    for (conn, id) in self.docs.waiting.remove(&doc).unwrap_or_default() {
                        frames.extend(self.tell(conn, DocOut::NotLive { id, reason, message: message.clone() }));
                    }
                }
                DocEvent::Remote(doc, bytes) => {
                    let bytes = Arc::new(bytes);
                    for (conn, id) in self.handles_of(doc) {
                        frames.extend(self.tell(conn, DocOut::Update { id, bytes: bytes.clone() }));
                    }
                }
                DocEvent::Presence(doc, bytes) => {
                    let bytes = Arc::new(bytes);
                    for (conn, id) in self.handles_of(doc) {
                        frames.extend(self.tell(conn, DocOut::Presence { id, bytes: bytes.clone() }));
                    }
                }
            }
        }
        frames
    }

    /// The first open waiting for `doc` is asked for its import; with none left, the join is given up on.
    fn ask_import(&mut self, doc: DocId) -> Vec<ClientFrame> {
        let first = self.docs.waiting.get(&doc).and_then(|w| w.first().copied());
        match first {
            Some((conn, id)) => {
                self.docs.importer.insert(doc, (conn, id));
                self.tell(conn, DocOut::NeedsImport { id })
            }
            None => {
                self.docs.importer.remove(&doc);
                self.replica.abandon_join(doc)
            }
        }
    }

    /// Opens `doc`, live here, as `id` on `conn`: its state goes to the app.
    fn deliver(&mut self, conn: ConnId, id: u64, doc: DocId) -> Vec<ClientFrame> {
        let Some((snapshot, waiting)) = self.replica.document_state(doc) else {
            return self.tell(conn, DocOut::NotLive { id, reason: NotLiveReason::Offline, message: "the document can't be read here".into() });
        };
        let file_hash = self.docs.opens.remove(&(conn, id)).flatten();
        let changed_outside = file_hash.and_then(|hash| self.changed_outside(doc, &hash));
        let Some(c) = self.docs.conns.get_mut(&conn) else { return Vec::new() };
        c.handles.insert(id, doc);
        self.replica.set_open(doc, true);
        let (online, waiting_n) = self.replica.document_status(doc).unwrap_or((false, 0));
        let state = DocState { online, waiting: waiting_n, bytes: snapshot.len() as u64, updates: waiting.len() as u64 };
        let mut frames = self.tell(conn, DocOut::Opened { id, doc, state, changed_outside, snapshot, waiting });
        // The file's record, to this open now (and to the others if it changed).
        let frontiers = self.file_record(doc);
        frames.extend(self.tell(conn, DocOut::File { doc, frontiers }));
        self.tell_files();
        frames
    }

    /// Whether the file content `hash` (hex) an app read for `doc` was edited outside Craft (see
    /// [`super::saves::changed_outside`]); none when the sync doesn't know the file.
    fn changed_outside(&mut self, doc: DocId, hash: &str) -> Option<bool> {
        let node = self.replica.document_node(doc)?;
        let saved_here = self.saves.find(doc, hash).is_some();
        let state = self.lookup.state()?;
        let base = state.base.get(&node).and_then(|e| match &e.kind {
            craft_cloud_sync::BaseKind::File { version, hash, .. } => Some((*version, *hash)),
            craft_cloud_sync::BaseKind::Dir { .. } => None,
        });
        let remote = state.remote.get(&node).and_then(|r| r.file.as_ref());
        super::saves::changed_outside(doc, hash, saved_here, base, remote)
    }

    /// Tells the apps of each open document the record of its file as it is on disk now, when that changed (after
    /// an open, every [`FILES_LOOK_EVERY`]).
    fn tell_files(&mut self) {
        let open: Vec<DocId> = {
            let mut docs: Vec<DocId> = self.docs.conns.values().flat_map(|c| c.handles.values().copied()).collect();
            docs.sort_unstable();
            docs.dedup();
            docs
        };
        self.docs.files.retain(|doc, _| open.contains(doc));
        for doc in open {
            let version = self.file_record(doc);
            let seen = self.docs.files.entry(doc).or_default();
            if seen.told.as_ref() == Some(&version) {
                continue;
            }
            seen.told = Some(version.clone());
            let mut conns: Vec<ConnId> = self.handles_of(doc).into_iter().map(|(conn, _)| conn).collect();
            conns.dedup();
            for conn in conns {
                let _ = self.tell(conn, DocOut::File { doc, frontiers: version.clone() });
            }
        }
    }

    /// The record of `doc`'s file as it is on disk: a save this machine's apps made of that content, or the server's
    /// version's record when the file is that version. None when no record is known (or the file can't be read).
    fn file_record(&mut self, doc: DocId) -> Option<Vec<(u64, u32)>> {
        let node = self.replica.document_node(doc)?;
        let folder = self.config.folder.clone();
        let state = self.lookup.state()?;
        let rel = state.base_path(node)?;
        let (base, remote) = match (&state.base.get(&node)?.kind, state.remote.get(&node).and_then(|r| r.file.as_ref())) {
            (craft_cloud_sync::BaseKind::File { version, hash, .. }, remote) => ((*version, *hash), remote.cloned()),
            _ => return None,
        };
        let path = rel.names().iter().fold(folder, |p, name| p.join(name.as_str()));
        let meta = std::fs::metadata(&path).ok()?;
        let stamp = Some((meta.len(), meta.modified().ok()?));
        let seen = self.docs.files.entry(doc).or_default();
        if seen.stamp != stamp || seen.hash.is_none() {
            // Read again only when the file changed (an unchanged file of the agreed size is mostly the agreed
            // content, but only reading it says so).
            if meta.len() > MAX_SNAPSHOT_BYTES as u64 {
                return None;
            }
            seen.hash = std::fs::read(&path).ok().map(|bytes| craft_cloud_proto::FileHash::from_bytes(*blake3::hash(&bytes).as_bytes()));
            seen.stamp = stamp;
        }
        let hash = seen.hash?;
        let hex: String = hash.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
        if let Some(record) = self.saves.find(doc, &hex) {
            return Some(record.frontiers);
        }
        let (version, agreed) = base;
        let remote = remote?;
        if hash != agreed || remote.version != version {
            return None;
        }
        remote.save.filter(|s| s.doc == doc).map(|s| s.frontiers)
    }

    /// An app is about to write `doc`'s file with content `hash` from the document at `frontiers`: kept in the save
    /// book once every operation the frontiers name is in this machine's copy (the changes come first on the app's
    /// stream; one that hasn't come yet holds the record back until it does).
    fn saved(&mut self, conn: ConnId, doc: DocId, hash: String, frontiers: Vec<(u64, u32)>) -> Vec<ClientFrame> {
        if let Err(message) = super::saves::SaveBook::check(&hash, &frontiers) {
            return self.tell(conn, refusal(None, None, craft_cloud_proto::api::codes::INVALID, message));
        }
        if self.replica.document_holds(doc, &frontiers) {
            self.keep_record(doc, &hash, frontiers);
        } else {
            self.docs.early.push(EarlyRecord { conn, doc, hash, frontiers });
        }
        Vec::new()
    }

    /// Writes a record to the save book, asks for a sync pass (a record that came after its upload is added to that
    /// version), and has the file's record told again (the app took this save for the file's; a write that then
    /// failed shows by the agent's next word).
    fn keep_record(&mut self, doc: DocId, hash: &str, frontiers: Vec<(u64, u32)>) {
        let at = crate::cli::now_ms();
        if let Err(e) = self.saves.add(doc, hash, frontiers, at) {
            self.say(format!("a save record couldn't be kept: {e}"));
            return;
        }
        self.control.request_pass();
        if let Some(seen) = self.docs.files.get_mut(&doc) {
            seen.told = None;
        }
    }

    /// Keeps the early records whose operations have come.
    fn take_early(&mut self) {
        if self.docs.early.is_empty() {
            return;
        }
        let early = std::mem::take(&mut self.docs.early);
        for r in early {
            if self.replica.document_holds(r.doc, &r.frontiers) {
                self.keep_record(r.doc, &r.hash, r.frontiers);
            } else {
                self.docs.early.push(r);
            }
        }
    }

    /// A docs connection ended (or was dropped): its documents are closed, its waiting opens dropped, an import it was
    /// asked for asked of the next open waiting (or the join given up on).
    fn gone(&mut self, conn: ConnId) -> Vec<ClientFrame> {
        let Some(c) = self.docs.conns.remove(&conn) else { return Vec::new() };
        for doc in c.handles.values() {
            self.replica.set_open(*doc, false);
        }
        // Its records still waiting for their changes: those never came (the version, if any, has no record).
        self.docs.early.retain(|r| r.conn != conn);
        self.docs.opens.retain(|(c, _), _| *c != conn);
        self.docs.lookups.retain(|l| l.conn != conn);
        for waiting in self.docs.waiting.values_mut() {
            waiting.retain(|(c, _)| *c != conn);
        }
        let orphaned: Vec<DocId> = self.docs.importer.iter().filter(|(_, (c, _))| *c == conn).map(|(doc, _)| *doc).collect();
        let mut frames = Vec::new();
        for doc in orphaned {
            frames.extend(self.ask_import(doc));
        }
        self.docs.waiting.retain(|_, w| !w.is_empty());
        frames
    }

    /// The path an app named, relative to the synced folder (one given absolute must be inside it).
    fn relative(&self, path: &str) -> Option<String> {
        let given = std::path::Path::new(path);
        if given.is_absolute() {
            let inside = given.strip_prefix(&self.config.folder).ok()?;
            let names: Option<Vec<&str>> = inside.components().map(|c| c.as_os_str().to_str()).collect();
            return recent_path(&names?.join("/"));
        }
        recent_path(path)
    }

    /// An app opens `path` as `id`.
    fn open(&mut self, conn: ConnId, id: u64, path: &str, app: &str, file_hash: Option<String>) -> Vec<ClientFrame> {
        let not_live = |reason, message: &str| DocOut::NotLive { id, reason, message: message.to_owned() };
        let Some(c) = self.docs.conns.get(&conn) else { return Vec::new() };
        if c.handles.contains_key(&id)
            || self.docs.waiting.values().any(|w| w.contains(&(conn, id)))
            || self.docs.lookups.iter().any(|l| l.conn == conn && l.id == id)
        {
            return self.tell(conn, refusal(Some(id), None, craft_cloud_proto::api::codes::INVALID, "that number is in use on this connection"));
        }
        let Ok(app) = AppName::new(app) else {
            return self
                .tell(conn, refusal(Some(id), None, craft_cloud_proto::api::codes::INVALID, "app names are 1-32 lowercase letters, digits or '-'"));
        };
        let Some(rel) = self.relative(path) else {
            return self.tell(conn, not_live(NotLiveReason::NotInFolder, "that isn't a path inside the synced folder"));
        };
        match std::fs::metadata(self.config.folder.join(&rel)) {
            Ok(m) if m.is_file() && m.len() > MAX_SNAPSHOT_BYTES as u64 => {
                return self.tell(conn, not_live(NotLiveReason::TooLarge, "the file is over the limit for live documents"));
            }
            Ok(m) if m.is_file() => {}
            _ => return self.tell(conn, not_live(NotLiveReason::NotInFolder, "there is no such file in the synced folder")),
        }
        self.docs.opens.insert((conn, id), file_hash);
        match self.lookup.file_node(&rel) {
            Some(node) => self.open_node(conn, id, node, &app),
            None => {
                // Not uploaded yet: a pass for it, and a few seconds for its node.
                self.control.request_pass();
                self.docs.lookups.push(Lookup { conn, id, path: rel, app, until: Instant::now() + NODE_WAIT });
                Vec::new()
            }
        }
    }

    /// Opens the document of the file node `node` as `id` on `conn`.
    fn open_node(&mut self, conn: ConnId, id: u64, node: NodeId, app: &AppName) -> Vec<ClientFrame> {
        let (doc, opening, mut frames) = self.replica.open_document(node, app);
        match opening {
            Opening::Live => frames.extend(self.deliver(conn, id, doc)),
            Opening::Joining => {
                self.docs.waiting.entry(doc).or_default().push((conn, id));
                // The server lacks it and the open asked for its import is gone: this one is asked.
                if self.replica.needs_import(doc) && !self.docs.importer.contains_key(&doc) {
                    frames.extend(self.ask_import(doc));
                }
            }
            Opening::NotLive(reason, message) => frames.extend(self.tell(conn, DocOut::NotLive { id, reason, message })),
        }
        frames
    }

    /// Looks again for the nodes opens wait for: found, opened; past their wait, the file opens as a plain file.
    fn look_again(&mut self) -> Vec<ClientFrame> {
        let now = Instant::now();
        let mut frames = Vec::new();
        for l in std::mem::take(&mut self.docs.lookups) {
            match self.lookup.file_node(&l.path) {
                Some(node) => frames.extend(self.open_node(l.conn, l.id, node, &l.app)),
                None if now >= l.until => frames.extend(self.tell(
                    l.conn,
                    DocOut::NotLive {
                        id: l.id,
                        reason: NotLiveReason::NotSynced,
                        message: "the file isn't uploaded yet; open it again in a moment".into(),
                    },
                )),
                None => self.docs.lookups.push(l),
            }
        }
        frames
    }

    /// Tells the apps of each open document how it stands, when that changed.
    fn tell_status(&mut self) {
        let open: Vec<DocId> = {
            let mut docs: Vec<DocId> = self.docs.conns.values().flat_map(|c| c.handles.values().copied()).collect();
            docs.sort_unstable();
            docs.dedup();
            docs
        };
        self.docs.told.retain(|doc, _| open.contains(doc));
        for doc in open {
            let (online, waiting) = self.replica.document_status(doc).unwrap_or((false, 0));
            let deleted = self.replica.document_node(doc).is_some_and(|node| !self.lookup.has_file(node));
            let stopped = self.replica.is_stopped(doc);
            let now = (online, waiting, deleted, stopped);
            if self.docs.told.get(&doc) == Some(&now) {
                continue;
            }
            self.docs.told.insert(doc, now);
            let mut conns: Vec<ConnId> = self.handles_of(doc).into_iter().map(|(conn, _)| conn).collect();
            conns.dedup();
            for conn in conns {
                let _ = self.tell(conn, DocOut::Status { doc, online, waiting, deleted, stopped });
            }
        }
    }

    /// A docs connection's request.
    fn docs_call(&mut self, call: DocsCall) -> Vec<ClientFrame> {
        match call {
            DocsCall::Connect { conn, out } => {
                self.docs.conns.insert(conn, DocsConn { out, handles: BTreeMap::new(), presence: HashMap::new() });
                Vec::new()
            }
            DocsCall::Open { conn, id, path, app, file_hash } => self.open(conn, id, &path, &app, file_hash),
            DocsCall::Saved { conn, doc, hash, frontiers } => self.saved(conn, doc, hash, frontiers),
            DocsCall::Import { conn, id, bytes } => {
                let doc = self.docs.importer.iter().find(|(_, who)| **who == (conn, id)).map(|(doc, _)| *doc);
                let Some(doc) = doc else {
                    return self.tell(conn, refusal(Some(id), None, craft_cloud_proto::api::codes::INVALID, "no import was asked for"));
                };
                match self.replica.import_document(doc, &bytes) {
                    Ok(frames) => frames,
                    Err((reason, message)) => {
                        self.docs.importer.remove(&doc);
                        let mut frames = Vec::new();
                        for (conn, id) in self.docs.waiting.remove(&doc).unwrap_or_default() {
                            frames.extend(self.tell(conn, DocOut::NotLive { id, reason, message: message.clone() }));
                        }
                        frames
                    }
                }
            }
            DocsCall::Update { conn, id, seq, bytes } => {
                let Some(doc) = self.docs.conns.get(&conn).and_then(|c| c.handles.get(&id)).copied() else {
                    return self.tell(conn, refusal(Some(id), Some(seq), craft_cloud_proto::api::codes::NOT_FOUND, "that document isn't open here"));
                };
                match self.replica.local_update(doc, &bytes) {
                    Ok(mut frames) => {
                        frames.extend(self.tell(conn, DocOut::Ack { id, seq }));
                        // A record waiting for this change.
                        self.take_early();
                        // The document's other apps on this machine get it at once.
                        let bytes = Arc::new(bytes);
                        for (other, other_id) in self.handles_of(doc) {
                            if (other, other_id) != (conn, id) {
                                frames.extend(self.tell(other, DocOut::Update { id: other_id, bytes: bytes.clone() }));
                            }
                        }
                        frames
                    }
                    Err(LocalError::Invalid(message)) => {
                        self.tell(conn, refusal(Some(id), Some(seq), craft_cloud_proto::api::codes::INVALID, message))
                    }
                    Err(LocalError::Io(e)) => {
                        let message = format!("the change couldn't be written to disk: {e}");
                        self.tell(conn, refusal(Some(id), Some(seq), craft_cloud_proto::api::codes::INTERNAL, message))
                    }
                }
            }
            DocsCall::Presence { conn, id, bytes } => {
                let Some(c) = self.docs.conns.get_mut(&conn) else { return Vec::new() };
                let Some(doc) = c.handles.get(&id).copied() else { return Vec::new() };
                // Over the budget: dropped (the next one says where the cursor is).
                if !c.presence.entry(id).or_insert_with(PresenceBudget::new).spend() {
                    return Vec::new();
                }
                let mut frames = self.replica.presence(doc, &bytes);
                let bytes = Arc::new(bytes);
                for (other, other_id) in self.handles_of(doc) {
                    if (other, other_id) != (conn, id) {
                        frames.extend(self.tell(other, DocOut::Presence { id: other_id, bytes: bytes.clone() }));
                    }
                }
                frames
            }
            DocsCall::Close { conn, doc } => {
                if let Some(c) = self.docs.conns.get_mut(&conn) {
                    let ids: Vec<u64> = c.handles.iter().filter(|(_, d)| **d == doc).map(|(id, _)| *id).collect();
                    for id in ids {
                        c.handles.remove(&id);
                        c.presence.remove(&id);
                        self.replica.set_open(doc, false);
                    }
                }
                // Closed before the changes its records cover came: those records are dropped.
                self.docs.early.retain(|r| !(r.conn == conn && r.doc == doc));
                Vec::new()
            }
            DocsCall::Gone { conn } => self.gone(conn),
        }
    }

    /// Starts the snapshot uploads and downloads the replica asks for: each in a task of its own, done when it says so
    /// ([`State::blob_done`]). While a document's upload is under way, its frames wait behind it.
    fn start_blobs(&mut self) {
        let uploads: Vec<Upload> = self.replica.take_uploads();
        let fetches = self.replica.take_fetches();
        if self.socket.is_none() {
            return;
        }
        let generation = self.blobs.generation;
        for up in uploads {
            let (doc, manifest) = (up.doc, up.id());
            self.blobs.uploading.entry(doc).or_default().push(manifest);
            let (transport, done) = (self.blobs.transport.clone(), self.blobs.done.clone());
            tokio::spawn(async move {
                let result = match transport {
                    Some(t) => upload(t.as_ref(), &up).await.map_err(|e| e.to_string()),
                    None => Err("the server's API can't be reached".into()),
                };
                let _ = done.send(BlobDone::Uploaded { generation, doc, manifest, result });
            });
        }
        for (doc, manifest) in fetches {
            let (transport, done) = (self.blobs.transport.clone(), self.blobs.done.clone());
            tokio::spawn(async move {
                let result = match transport {
                    Some(t) => download(t.as_ref(), &manifest).await.map_err(|e| e.to_string()),
                    None => Err("the server's API can't be reached".into()),
                };
                let _ = done.send(BlobDone::Downloaded { generation, doc, manifest, result });
            });
        }
    }

    /// A snapshot's upload or download is done: an upload's document's frames go (the one naming a snapshot that
    /// didn't go up is dropped, and the replica told), a download goes to the replica. Frames to send.
    fn blob_done(&mut self, done: BlobDone) -> Vec<ClientFrame> {
        match done {
            BlobDone::Uploaded { generation, .. } | BlobDone::Downloaded { generation, .. } if generation != self.blobs.generation => Vec::new(),
            BlobDone::Uploaded { doc, manifest, result, .. } => {
                if let Err(e) = &result {
                    self.say(format!("a live document's snapshot couldn't go up to the server: {e}"));
                    self.replica.upload_failed(doc, e);
                    if let Some(held) = self.blobs.held.get_mut(&doc) {
                        held.retain(|f| !names(f, manifest));
                    }
                }
                let left = self.blobs.uploading.get_mut(&doc).map(|list| {
                    if let Some(i) = list.iter().position(|m| *m == manifest) {
                        list.remove(i);
                    }
                    list.len()
                });
                if left == Some(0) {
                    self.blobs.uploading.remove(&doc);
                    return self.blobs.held.remove(&doc).unwrap_or_default();
                }
                Vec::new()
            }
            BlobDone::Downloaded { doc, manifest, result, .. } => self.replica.on_snapshot(doc, manifest, result),
        }
    }

    /// Sends frames; false when the connection broke (it is dropped).
    async fn send(&mut self, frames: Vec<ClientFrame>) -> bool {
        self.start_blobs();
        if self.socket.is_none() {
            return true;
        }
        let mut encoded = Vec::with_capacity(frames.len());
        for frame in frames {
            // Behind an upload of its document: it goes once that is done.
            if let Some(doc) = frame_doc(&frame)
                && self.blobs.uploading.contains_key(&doc)
            {
                self.blobs.held.entry(doc).or_default().push(frame);
                continue;
            }
            match frame.encode() {
                Ok(bytes) => encoded.push(bytes),
                Err(e) => self.say(format!("a live frame couldn't be encoded: {e}")),
            }
        }
        let Some(socket) = self.socket.as_mut() else { return true };
        for bytes in encoded {
            if !socket.send(Message::Binary(bytes.into())) {
                self.drop_socket();
                return false;
            }
        }
        true
    }

    /// The connection is gone (for whatever reason): dialed again after the backoff, never at once.
    fn drop_socket(&mut self) {
        if self.socket.take().is_some() {
            self.back_off(None);
        }
        self.replica.disconnected();
        self.control.set_online(false);
        self.new_generation();
    }

    /// Transfers under way and the frames held behind them are for a connection that ended: no news any more.
    fn new_generation(&mut self) {
        self.blobs.generation += 1;
        self.blobs.uploading.clear();
        self.blobs.held.clear();
    }

    fn entry(&mut self, app: &AppName, new: &NewRecent) -> Result<RecentEntry, HubError> {
        let path = recent_path(&new.path).ok_or_else(|| HubError::Invalid(format!("{:?} isn't a path inside the synced folder", new.path)))?;
        let name = path.rsplit('/').next().unwrap_or(&path).to_owned();
        let title = clean(new.title.as_deref().unwrap_or(&name), MAX_TITLE_BYTES);
        let now = crate::cli::now_ms();
        let mut entry = RecentEntry {
            node: self.lookup.node(&path),
            path,
            title,
            opened_ms: now,
            saved_ms: None,
            machine: clean(&self.config.machine, MAX_MACHINE_BYTES),
        };
        entry.saved_ms = if new.saved { Some(now) } else { self.replica.current(app, &entry).and_then(|e| e.saved_ms) };
        Ok(entry)
    }

    async fn handle(&mut self, request: Request) {
        match request {
            // The run loop's to handle (it owns the dialing).
            Request::Pause(..) => {}
            Request::Docs(call) => {
                let frames = self.docs_call(call);
                self.send(frames).await;
            }
            Request::Follow { app, reply } => {
                if !self.lists.contains_key(&app) && self.lists.len() >= MAX_SUBSCRIPTIONS {
                    let _ = reply.send(Err(HubError::Invalid(format!("the agent follows at most {MAX_SUBSCRIPTIONS} apps' recents"))));
                    return;
                }
                let frames = self.replica.follow(&app);
                let entries = self.replica.entries(&app);
                let push_stopped = self.replica.push_stopped(&app);
                let list = self.lists.entry(app).or_insert_with(|| watch::channel(Listing { entries, push_stopped }).0);
                let _ = reply.send(Ok(list.subscribe()));
                self.send(frames).await;
            }
            Request::Add { app, entry, reply } => {
                if !self.lists.contains_key(&app) && self.lists.len() >= MAX_SUBSCRIPTIONS {
                    let _ = reply.send(Err(HubError::Invalid(format!("the agent follows at most {MAX_SUBSCRIPTIONS} apps' recents"))));
                    return;
                }
                let added = match (self.entry(&app, &entry), random_key()) {
                    (Ok(entry), Ok(key)) => self.replica.add(&app, key, entry.clone()).map(|frames| (entry, frames)).map_err(|e| match e {
                        AddError::Invalid(m) => HubError::Invalid(m),
                        AddError::Io(e) => HubError::Failed(format!("the recents couldn't be written to disk: {e}")),
                    }),
                    (Err(e), _) => Err(e),
                    (_, Err(e)) => Err(HubError::Failed(e.to_string())),
                };
                if !self.lists.contains_key(&app) {
                    let entries = self.replica.entries(&app);
                    let push_stopped = self.replica.push_stopped(&app);
                    self.lists.insert(app, watch::channel(Listing { entries, push_stopped }).0);
                }
                match added {
                    Ok((entry, frames)) => {
                        let _ = reply.send(Ok(entry));
                        self.send(frames).await;
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
        }
    }
}

async fn run(
    config: HubConfig,
    replica: Replica<DiskStore>,
    mut requests: mpsc::Receiver<Request>,
    control: Control,
    mut stop: oneshot::Receiver<()>,
    paused: bool,
) {
    let lookup = NodeLookup { data: config.data.clone(), seen: None, state: None };
    let (blob_tx, mut blob_rx) = mpsc::unbounded_channel();
    let transport = HttpTransport::new(config.server.clone(), &config.token).ok().map(Arc::new);
    let blobs = Blobs { transport, done: blob_tx, generation: 0, uploading: BTreeMap::new(), held: BTreeMap::new() };
    let saves = super::saves::SaveBook::new(config.data.path(), config.space);
    // The sync engine's questions: which of two saves of a document is newer (this copy's operations say).
    let (_orders, mut orders) = super::saves::Orders::serve(config.data.path(), config.space);
    let mut state = State {
        saves,
        config,
        replica,
        blobs,
        lookup,
        lists: BTreeMap::new(),
        socket: None,
        control,
        said: None,
        retry_at: Instant::now(),
        delay: RETRY_FIRST,
        paused,
        docs: Docs::default(),
    };
    state.settle();
    let mut dialing: Option<tokio::task::JoinHandle<Result<LiveSocket, DialError>>> = None;
    let mut last_heard = Instant::now();
    let mut ping = tokio::time::interval(PING_EVERY);
    let mut resend = tokio::time::interval(RESEND_EVERY);
    let mut look = tokio::time::interval(NODE_LOOK_EVERY);
    let mut write_documents = tokio::time::interval(DOCUMENTS_FLUSH_EVERY);
    let mut look_files = tokio::time::interval(FILES_LOOK_EVERY);
    loop {
        if state.socket.is_none() && dialing.is_none() && !state.paused && Instant::now() >= state.retry_at {
            let (server, token) = (state.config.server.clone(), state.config.token.clone());
            dialing = Some(tokio::spawn(async move { dial(&server, &token).await }));
        }
        let (up, retry_at) = (state.socket.is_some(), state.retry_at);
        let writer_gone = state.socket.as_ref().map(|s| s.gone.clone());
        tokio::select! {
            _ = &mut stop => {
                if let Some(socket) = state.socket.take() {
                    socket.close().await;
                }
                if let Err(e) = state.replica.flush_documents() {
                    state.say(format!("the live documents couldn't be written to disk: {e}"));
                }
                return;
            }
            request = requests.recv() => match request {
                Some(Request::Pause(paused, done)) => {
                    state.paused = paused;
                    if paused {
                        if let Some(task) = dialing.take() {
                            task.abort();
                        }
                        if let Some(socket) = state.socket.take() {
                            socket.close().await;
                        }
                        state.replica.disconnected();
                        state.new_generation();
                        // Offline before the caller hears it, so the status it reads next says so.
                        state.control.set_online(false);
                    } else {
                        // Dialed at once: nothing was wrong with the connection.
                        state.retry_at = Instant::now();
                        state.delay = RETRY_FIRST;
                    }
                    let _ = done.send(());
                }
                Some(request) => state.handle(request).await,
                None => return,
            },
            dialed = async { match dialing.as_mut() { Some(task) => task.await, None => std::future::pending().await } } => {
                dialing = None;
                match dialed {
                    Ok(Ok(socket)) => {
                        state.new_generation();
                        state.socket = Some(Conn::new(socket));
                        last_heard = Instant::now();
                        let hello = state.replica.connected();
                        state.send(hello).await;
                    }
                    Ok(Err(e)) => {
                        state.say(format!("recents stay on this machine for now: {e}"));
                        state.back_off(None);
                    }
                    Err(_) => state.back_off(None),
                }
            }
            Some(order) = orders.recv() => {
                let _ = order.reply.send(state.replica.document_includes(order.doc, &order.a, &order.b));
                continue;
            }
            Some(done) = blob_rx.recv() => {
                let frames = state.blob_done(done);
                state.send(frames).await;
            }
            // The writer gave up (the server took nothing for the send timeout): the connection is gone.
            () = async { match &writer_gone { Some(gone) => gone.notified().await, None => std::future::pending().await } } => {
                state.drop_socket();
            }
            message = async { match state.socket.as_mut() { Some(socket) => socket.incoming.next().await, None => std::future::pending().await } } => {
                last_heard = Instant::now();
                let frame = match message {
                    Some(Ok(Message::Binary(bytes))) => ServerFrame::decode(&bytes).map_err(|e| LinkError::Retry(format!("a live frame doesn't decode: {e}"))),
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => continue,
                    Some(Ok(Message::Text(_))) => Err(LinkError::Retry("the server sent a text frame".into())),
                    Some(Ok(Message::Close(_)) | Err(_)) | None => Err(LinkError::Retry("the live connection closed".into())),
                };
                match frame.and_then(|frame| state.replica.on_frame(frame)) {
                    Ok(frames) => {
                        if state.replica.is_up() {
                            state.delay = RETRY_FIRST;
                            state.said = None;
                        }
                        state.send(frames).await;
                    }
                    Err(e) => {
                        if let Some(socket) = state.socket.take() {
                            socket.close().await;
                        }
                        state.drop_socket();
                        state.back_off(None);
                        if let LinkError::Mismatch(m) = &e {
                            state.say(format!("recents stay on this machine: {m}"));
                            state.back_off(Some(RETRY_MISMATCH));
                        }
                    }
                }
            }
            _ = ping.tick(), if up => {
                if last_heard.elapsed() > IDLE_TIMEOUT {
                    state.drop_socket();
                } else if let Some(socket) = state.socket.as_mut()
                    && !socket.send(Message::Ping(Vec::new().into()))
                {
                    state.drop_socket();
                }
            }
            _ = resend.tick(), if up && state.replica.wants_tick() => {
                let frames = state.replica.resend();
                state.send(frames).await;
            }
            _ = look.tick(), if !state.docs.lookups.is_empty() => {
                let frames = state.look_again();
                state.send(frames).await;
            }
            _ = look_files.tick(), if !state.docs.files.is_empty() || state.docs.conns.values().any(|c| !c.handles.is_empty()) => {
                state.tell_files();
            }
            _ = write_documents.tick() => {
                if let Err(e) = state.replica.flush_documents() {
                    state.say(format!("the live documents couldn't be written to disk: {e}"));
                }
            }
            () = tokio::time::sleep_until(retry_at), if !up && dialing.is_none() && !state.paused => {}
        }
        let frames = state.settle();
        state.send(frames).await;
    }
}

/// How long closing the live connection may take before it is simply dropped.
pub const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// The live connection: what comes in is read by the hub's loop, what goes out is written by a task of its own, so
/// the hub keeps reading while a long send (many changes made offline, a push, a snapshot) waits for the server to
/// read. (If both sides only wrote, each waiting for the other to read, the connection would stall until a send
/// timeout ended it, again and again.) A writer that can't send within [`SEND_TIMEOUT`] ends, and with it the
/// connection.
struct Conn {
    out: mpsc::UnboundedSender<Message>,
    incoming: futures_util::stream::SplitStream<LiveSocket>,
    writer: tokio::task::JoinHandle<()>,
    /// Told when the writer gave up.
    gone: Arc<tokio::sync::Notify>,
}

impl Conn {
    fn new(socket: LiveSocket) -> Self {
        let (mut sink, incoming) = socket.split();
        let (out, mut queue) = mpsc::unbounded_channel::<Message>();
        let gone = Arc::new(tokio::sync::Notify::new());
        let tell = gone.clone();
        let writer = tokio::spawn(async move {
            while let Some(message) = queue.recv().await {
                if !matches!(tokio::time::timeout(SEND_TIMEOUT, sink.send(message)).await, Ok(Ok(()))) {
                    tell.notify_one();
                    return;
                }
            }
            close_within(&mut sink, CLOSE_TIMEOUT).await;
        });
        Self { out, incoming, writer, gone }
    }

    /// Queues a message; false when the writer is gone (the connection with it).
    fn send(&self, message: Message) -> bool {
        !self.writer.is_finished() && self.out.send(message).is_ok()
    }

    /// Closes the connection: what is queued goes first, then the close, all within [`CLOSE_TIMEOUT`].
    async fn close(self) {
        let Self { out, incoming, mut writer, .. } = self;
        drop(out);
        drop(incoming);
        if tokio::time::timeout(CLOSE_TIMEOUT, &mut writer).await.is_err() {
            writer.abort();
        }
    }
}

/// Closes the live connection, waiting at most `limit`: on a half-open connection with a full send buffer (a laptop
/// that just changed networks) the close frame may never go out, and the hub must not hang on it. The connection is
/// dropped by the caller either way.
pub async fn close_within<S: futures_util::Sink<Message> + Unpin>(socket: &mut S, limit: Duration) {
    let _ = tokio::time::timeout(limit, socket.close()).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// A connection whose close never completes.
    struct Stuck;

    impl futures_util::Sink<Message> for Stuck {
        type Error = std::io::Error;
        fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn start_send(self: Pin<&mut Self>, _: Message) -> Result<(), Self::Error> {
            Ok(())
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Pending
        }
        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Pending
        }
    }

    #[tokio::test]
    async fn a_close_that_never_completes_is_given_up_on() {
        let started = std::time::Instant::now();
        let closed = tokio::time::timeout(Duration::from_secs(3), close_within(&mut Stuck, Duration::from_millis(200))).await;
        assert!(closed.is_ok(), "the close hung");
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
