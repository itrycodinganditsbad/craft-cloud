//! The session: one open live document, tying the app's model to a Loro document and the agent.
//!
//! - [`Session::open`] opens a file of the synced folder by path. A file that has never been live is imported from the
//!   app's model (the model it read from the file); otherwise the model is built from the live document.
//! - [`Session::local_change`] (the hook, at the end of each top-level command) writes the difference between the
//!   model before and after as Loro commits with this machine's origin, one undo step, and sends them.
//! - [`Session::poll`] (once per frame) takes what came from the agent and applies remote changes: the app rebuilds
//!   the touched parts ([`LiveDocument::apply_remote`]), then the kit runs [`LiveDocument::normalise`] (writing back
//!   what it repaired) and [`LiveDocument::refresh_derived`], and hands the new model over. It never blocks, and
//!   never applies anything while the app holds remote changes back ([`Session::hold_remote`]: a drag, an IME
//!   composition).
//! - Undo and redo go through Loro's undo manager ([`Session::undo`]): only this machine's changes are taken back, and
//!   the result comes back like a remote change. [`Session::group`] makes several changes one step (typing).
//! - When the agent goes, the model stays editable: changes queue in memory (at most [`MAX_QUEUE_BYTES`]; past that,
//!   [`KitError::Stalled`]) and go out when the session is back, which it tries by itself with a growing pause, off
//!   the UI thread.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use craft_cloud_api::fragments::parts;
use craft_cloud_app::Client;
use craft_cloud_app::live::{Document, Documents, Event, NotLiveReason, OpenOptions, Opened, Waker};
use loro::event::{Diff, DiffEvent};
use loro::{ContainerID, ExpandType, Index, LoroDoc, LoroValue, StyleConfig, StyleConfigMap, TreeID, UndoManager, ValueOrContainer};

use crate::mapping::{Kind, Mappings, Part, RemotePatch, Touched};
use crate::table::{TableTouch, parse_cell_key};
use crate::timeline::TimelineTouch;
use crate::tree::{TreeIndex, TreeTouch};
use crate::writer::{KIT_ORIGIN, LOCAL_ORIGIN, Updates, Writer, export_since};
use crate::{ElemId, KitError};

/// The most bytes of changes a session keeps in memory for the agent; at or past it, local changes are refused
/// ([`KitError::Stalled`]) until the agent takes them.
pub const MAX_QUEUE_BYTES: usize = 64 * 1024 * 1024;
/// How long the pause between reconnects grows to.
pub const MAX_BACKOFF: Duration = Duration::from_secs(8);
const FIRST_BACKOFF: Duration = Duration::from_millis(200);
/// Presence is sent again this often while set, and a peer's is dropped after this long without one.
pub const PRESENCE_EVERY: Duration = Duration::from_secs(3);
pub const PRESENCE_TTL: Duration = Duration::from_secs(10);
/// The largest change the kit sends (one update, in fragments past 1 MiB): a snapshot's limit. A change over it can't
/// be live: the session stops (the model stays editable; save it to the file).
pub const MAX_CHANGE_BYTES: usize = craft_cloud_api::fragments::MAX_WHOLE_BYTES;
/// Undo steps kept.
const UNDO_STEPS: usize = 500;
/// The most text (inserted and deleted, in bytes) one step may change and still be undone while live. Loro's undo of a
/// run of text takes time growing with its square: about 35 ms at 64 KiB, 2.4 s at 512 KiB, 10 s at 1 MB.
pub const MAX_UNDO_TEXT_BYTES: usize = 64 * 1024;

/// The root map of the kit's own keys: `schema` (`{app, version}`) and `import` (the hash, hex, of the file's bytes
/// the document was imported from).
const ROOT: &str = "root";
const SCHEMA: &str = "schema";
const IMPORT: &str = "import";

/// An app's model, mapped onto a live document. The app implements it once, over its own document type.
pub trait LiveDocument {
    /// The app's document model (shared by `Arc`, cloned copy-on-write).
    type Doc: Clone;
    /// The app's lowercase name, as in recents (`deckcraft`).
    const APP: &'static str;
    /// The layout version the app's mappings write: a document with a newer one opens read-only.
    const SCHEMA: u32;
    /// The parts of the model the live document holds.
    fn mappings(doc: &Self::Doc) -> Mappings<'_>;
    /// The model after a remote change: `old` with the touched parts rebuilt from `patch` (no undo step).
    fn apply_remote(old: &Arc<Self::Doc>, patch: &RemotePatch<'_>) -> Result<Arc<Self::Doc>, KitError>;
    /// Repairs what a merge can break (duplicate names, dangling references, missing cells): the result must be a
    /// model the app could have made itself. What it changes in shared parts is written back.
    fn normalise(doc: &mut Self::Doc);
    /// Recomputes what is derived from shared state and never synced (layout, caches).
    fn refresh_derived(doc: &mut Self::Doc);
}

/// How to reach the agent (again, after it went): the running agent's client, or a fake's in tests.
#[derive(Clone)]
pub struct Agent {
    connect: Arc<dyn Fn() -> Result<Client, craft_cloud_app::Error> + Send + Sync>,
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Agent")
    }
}

impl Agent {
    /// The agent running on this machine for this user ([`Client::connect`]).
    pub fn local() -> Self {
        Self::from_fn(Client::connect)
    }

    /// An agent found by `connect` each time (a fake's client in tests).
    pub fn from_fn(connect: impl Fn() -> Result<Client, craft_cloud_app::Error> + Send + Sync + 'static) -> Self {
        Self { connect: Arc::new(connect) }
    }

    pub fn client(&self) -> Result<Client, craft_cloud_app::Error> {
        (self.connect)()
    }
}

/// Why a file opened as a plain file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotLive {
    /// Craft Cloud isn't running here (or refused): the message says why.
    NoAgent(String),
    /// The agent's reason (offline before the first live open, not synced yet, …).
    Reason(NotLiveReason),
}

/// What opening a file gives.
pub enum Open<D: LiveDocument> {
    Live(Box<Session<D>>),
    /// Open and save it as a plain file, with the app's own undo.
    NotLive(NotLive),
}

/// A document made by a newer app (or another one): it is shown, but nothing is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadOnly {
    pub app: String,
    pub version: u32,
}

/// How the session stands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// Connected to the agent, with the document open.
    pub connected: bool,
    /// The agent reaches the server (edits go out to the other machines as they are made).
    pub online: bool,
    /// Changes the agent has that the server hasn't acked.
    pub waiting: u64,
    /// Changes kept here that the agent hasn't acked (while disconnected: everything since).
    pub queued: usize,
    pub queued_bytes: usize,
    /// The document needs a newer app: show "update <App>".
    pub read_only: Option<ReadOnly>,
    /// Too much waits for the agent: local changes are refused until it is back.
    pub stalled: bool,
    /// The file was deleted (the document still works).
    pub deleted: bool,
    /// Nothing more goes out: the agent stopped following the document until it restarts, or refused a change for
    /// good, or the document was replaced (the error says which). The model stays editable: save it to keep the work.
    pub stopped: bool,
    /// `document_replaced`: when the session came back, its path was another document (renamed away while the agent
    /// was down, another file in its place). It is never merged into it; save the model as a new file.
    pub replaced: bool,
    /// The file on disk wasn't saved by a Craft app from this document (edited outside): the app may bring it in as a
    /// local change, or keep the live content.
    pub changed_outside: bool,
    /// Changes another machine sent in fragments that didn't come whole on a connection (dropped, never applied), in
    /// this session: each time, the session opened the document again and caught up.
    pub gaps: u64,
    /// The last error, for the app to show.
    pub error: Option<String>,
}

/// The one-word state a document badge shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Badge {
    Live,
    Waiting(u64),
    ReadOnly,
    Offline,
    /// Too much waits for the agent: new changes are refused until it is back.
    Stalled,
    /// Nothing more goes out (`Status::error` says why): save the document to keep the work.
    Stopped,
}

impl Status {
    pub fn badge(&self) -> Badge {
        if self.read_only.is_some() {
            Badge::ReadOnly
        } else if self.stopped {
            Badge::Stopped
        } else if self.stalled {
            Badge::Stalled
        } else if !self.connected || !self.online {
            Badge::Offline
        } else if self.waiting > 0 || self.queued > 0 {
            Badge::Waiting(self.waiting.max(self.queued as u64))
        } else {
            Badge::Live
        }
    }
}

/// What a poll did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Polled {
    /// The model changed (a remote change, a reconnect): take [`Session::model`] and redraw.
    pub changed: bool,
    pub status: Status,
}

/// What undo means for the document right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UndoKind {
    /// Through the session ([`Session::undo`]): only this machine's changes.
    Live,
    /// The document is read-only: nothing to undo here.
    None,
}

/// What a save gives the agent: the content's hash and the live document's frontiers (its heads, `(peer, counter)`
/// operation IDs, sorted) it was made from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveRecord {
    pub hash: [u8; 32],
    pub frontiers: Vec<(u64, u32)>,
}

/// What a save is made from ([`Session::save_point`]): the model to write, and the live document's frontiers it
/// matches.
pub struct SavePoint<D: LiveDocument> {
    model: Arc<D::Doc>,
    frontiers: Vec<(u64, u32)>,
    /// This session's counter then: the record goes to the agent after every change up to it.
    end: i32,
}

impl<D: LiveDocument> SavePoint<D> {
    /// The model to write to the file.
    pub fn model(&self) -> &Arc<D::Doc> {
        &self.model
    }

    /// The live document's frontiers (`(peer, counter)` heads, sorted) the model matches.
    pub fn frontiers(&self) -> &[(u64, u32)] {
        &self.frontiers
    }
}

impl<D: LiveDocument> Clone for SavePoint<D> {
    fn clone(&self) -> Self {
        Self { model: self.model.clone(), frontiers: self.frontiers.clone(), end: self.end }
    }
}

impl<D: LiveDocument> std::fmt::Debug for SavePoint<D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SavePoint").field("frontiers", &self.frontiers).finish_non_exhaustive()
    }
}

/// How many save records a session keeps to tell the agent (again) when it is back.
const KEPT_RECORDS: usize = 16;
/// The name of the undo step [`Session::replace_with_file`] makes.
pub const REPLACE_WITH_FILE: &str = "Replace with the file's content";
/// At most one save that `file_behind` asks for per this long (a backstop against a loop between machines).
pub const CATCH_UP_EVERY: Duration = Duration::from_secs(5);

/// A save record kept for the agent: sent after every change it covers.
#[derive(Debug, Clone)]
struct Kept {
    record: SaveRecord,
    /// This session's counter at the save: every change up to it goes first.
    end: i32,
    /// Sent on the current connection.
    sent: bool,
    /// Announced on the current connection (`saving`: at once, before the write and before the changes go).
    announced: bool,
}

/// Another machine's (or app's) presence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub bytes: Vec<u8>,
    pub seen: Instant,
}

/// One change waiting for the agent.
struct Out {
    bytes: Vec<u8>,
    /// This peer's counter after it (what the agent has once it holds this).
    end: i32,
    /// Its number on the current connection, once sent.
    seq: Option<u64>,
}

/// A raw touch from Loro's events, sorted out after an import.
#[derive(Debug, Clone, Default)]
struct Raw {
    root: String,
    nodes: Vec<TreeID>,
    keys: Vec<String>,
    tree: bool,
    /// A text's delta (the text root's own, or a nested story's).
    text: Option<Vec<loro::TextDelta>>,
    /// The deepest tree node on the path, and the keys after it (a story nested in a node: `["text"]`).
    node: Option<TreeID>,
    under_node: Vec<String>,
    /// The keys after the root's own (a story in a root table's map cell: `["<row>:<col>", "text"]`).
    under_root: Vec<String>,
}

#[derive(Default)]
struct Collector {
    on: bool,
    raw: Vec<Raw>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn root_name(id: &ContainerID) -> Option<String> {
    match id {
        ContainerID::Root { name, .. } => Some(name.to_string()),
        ContainerID::Normal { .. } => None,
    }
}

/// What the mappings need between changes: the trees' indexes and the text styles seen.
#[derive(Default)]
pub(crate) struct Ctx {
    pub(crate) trees: HashMap<String, TreeIndex>,
    pub(crate) styles: StyleConfigMap,
}

/// The connection to the agent: the document and its documents connection (kept alive). The document goes first, so
/// the agent hears it closed before the connection ends.
struct Link {
    doc: Document,
    _docs: Documents,
}

/// Why a reopen didn't attach.
enum Reopen {
    /// Not now (the agent isn't there, the file isn't live now): tried again after a pause.
    Retry(String),
    /// The path is another document now (renamed away while the agent was down, another file in its place): never
    /// merged into or imported over.
    Replaced(String),
}

type Reopened = Result<(Documents, Document), Reopen>;

/// One open live document.
pub struct Session<D: LiveDocument> {
    doc: LoroDoc,
    undo: UndoManager,
    model: Arc<D::Doc>,
    ctx: Ctx,
    layout: Vec<(&'static str, Kind)>,
    collector: Arc<Mutex<Collector>>,
    _events: loro::Subscription,
    agent: Agent,
    path: String,
    link: Option<Link>,
    reopening: Option<Receiver<Reopened>>,
    next_try: Instant,
    backoff: Duration,
    outbox: VecDeque<Out>,
    outbox_bytes: usize,
    incoming: Vec<Vec<u8>>,
    held: bool,
    grouping: bool,
    status: Status,
    peers: HashMap<u64, Peer>,
    presence: Option<Vec<u8>>,
    presence_sent: Option<Instant>,
    waker: Option<Waker>,
    /// The agent refused a change for good (it doesn't decode, it is over the cap): nothing more is sent, and the
    /// session says it is stopped; the model stays editable, and saving to the file (without a record) keeps the work.
    refused: Option<String>,
    /// The live document this session is of (a reopen must find the same one at its path).
    doc_id: Option<craft_cloud_api::DocId>,
    /// Undos (`true`) and redos asked for while remote changes were held: run once the gesture ends.
    deferred: Vec<bool>,
    /// The text bytes of each undo step, by the counter it starts at (the undo manager's items carry that start).
    step_sizes: HashMap<i32, usize>,
    /// The start of the group of changes under way.
    group_key: Option<i32>,
    /// A rebuild failed: the live document is ahead of the model, which takes no local change until a whole rebuild
    /// succeeds.
    model_stale: bool,
    /// The agent refused a change for now (its disk): the queue goes again, in order, from this moment.
    resend_at: Instant,
    resend_backoff: Duration,
    /// The last saves' records, told the agent again on every reconnect (it may have missed them; it keeps one once),
    /// each after the changes it covers.
    records: VecDeque<Kept>,
    /// The record of the document's file as it is on disk (its frontiers): the agent's word, or this session's last
    /// save since.
    file_version: Option<Vec<(u64, u32)>>,
    /// When the last save that `file_behind` asked for was recorded ([`CATCH_UP_EVERY`]).
    caught_up_at: Option<Instant>,
    /// A local change (or undo) was made since the last save.
    changed_since_save: bool,
    /// The names of undo steps that have one ([`Session::replace_with_file`]), by the counter they start at.
    step_names: HashMap<i32, &'static str>,
}

impl<D: LiveDocument> std::fmt::Debug for Session<D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").field("path", &self.path).field("status", &self.status).finish_non_exhaustive()
    }
}

/// A fresh Loro document with the kit's text styles.
fn new_doc() -> LoroDoc {
    let doc = LoroDoc::new();
    doc.config_default_text_style(Some(StyleConfig { expand: ExpandType::After }));
    doc
}

/// The undo manager of a session's document: this machine's changes only, each step carrying where it starts, which
/// names its size (`step_sizes`).
fn new_undo(doc: &LoroDoc) -> UndoManager {
    let mut undo = UndoManager::new(doc);
    undo.add_exclude_origin_prefix(KIT_ORIGIN);
    undo.set_max_undo_steps(UNDO_STEPS);
    undo.set_on_push(Some(Box::new(|_, span, _| {
        let mut meta = loro::UndoItemMeta::new();
        meta.set_value(LoroValue::I64(i64::from(span.start)));
        meta
    })));
    undo
}

/// Writes the difference of two models (everything, when `old` is `None`) through `w`.
pub(crate) fn write_diff<D: LiveDocument>(w: &mut Writer<'_>, old: Option<&D::Doc>, new: &D::Doc, ctx: &mut Ctx) -> Result<(), KitError> {
    let new_maps = D::mappings(new);
    new_maps.layout()?;
    let old_maps = old.map(D::mappings);
    for (key, part) in &new_maps.parts {
        let old_part = old_maps.as_ref().and_then(|m| m.parts.iter().find(|(k, _)| k == key)).map(|(_, p)| p);
        match part {
            Part::Tree(n) => {
                let o = match old_part {
                    Some(Part::Tree(o)) => Some(o.as_ref()),
                    _ => None,
                };
                let index = ctx.trees.entry((*key).to_owned()).or_default();
                crate::tree::write(w, key, o, n.as_ref(), index, &mut ctx.styles)?;
            }
            Part::Text(n) => {
                let o = match old_part {
                    Some(Part::Text(o)) => Some(o.as_ref()),
                    _ => None,
                };
                crate::text::write(w, key, o, n.as_ref(), &mut ctx.styles)?;
            }
            Part::Table(n) => {
                let o = match old_part {
                    Some(Part::Table(o)) => Some(o.as_ref()),
                    _ => None,
                };
                crate::table::write(w, key, o, n.as_ref(), &mut ctx.styles)?;
            }
            Part::Timeline(n) => {
                let o = match old_part {
                    Some(Part::Timeline(o)) => Some(o.as_ref()),
                    _ => None,
                };
                crate::timeline::write(w, key, o, n.as_ref(), &mut ctx.styles)?;
            }
            Part::Opaque(n) => {
                let o = match old_part {
                    Some(Part::Opaque(o)) => Some(o.as_ref()),
                    _ => None,
                };
                crate::opaque::write(w, key, o, n.as_ref())?;
            }
        }
    }
    Ok(())
}

/// The document's schema: `(app, version)`.
fn schema(doc: &LoroDoc) -> Option<(String, u32)> {
    let ValueOrContainer::Value(LoroValue::Map(map)) = doc.get_map(ROOT).get(SCHEMA)? else { return None };
    let app = match map.get("app")? {
        LoroValue::String(s) => s.to_string(),
        _ => return None,
    };
    let version = match map.get("version")? {
        LoroValue::I64(v) => u32::try_from(*v).ok()?,
        _ => return None,
    };
    Some((app, version))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The hash (hex) of the file bytes the document was imported from, if the import said.
fn import_hash(doc: &LoroDoc) -> Option<String> {
    match doc.get_map(ROOT).get(IMPORT)? {
        ValueOrContainer::Value(LoroValue::String(s)) => Some(s.to_string()),
        _ => None,
    }
}

impl<D: LiveDocument> Session<D> {
    /// Opens the file at `path` (relative to the synced folder) live. `file` is the app's model of the file as it read
    /// it, and `file_bytes` the bytes it read (to tell whether it was changed outside Craft). Blocking (the agent may
    /// wait for the file's first sync, up to 30 s): call it where the app opens files.
    pub fn open(agent: &Agent, path: &str, file: Arc<D::Doc>, file_bytes: Option<&[u8]>) -> Result<Open<D>, KitError> {
        let not_running = |e: craft_cloud_app::Error| Ok(Open::NotLive(NotLive::NoAgent(e.to_string())));
        let client = match agent.client() {
            Ok(c) => c,
            Err(e) => return not_running(e),
        };
        let docs = match client.documents() {
            Ok(d) => d,
            Err(e) => return not_running(e),
        };
        let file_hash = file_bytes.map(|b| *blake3::hash(b).as_bytes());
        let opened = match docs.open_with(path, D::APP, &OpenOptions { file_hash }) {
            Ok(o) => o,
            Err(craft_cloud_app::Error::Refused { code, message }) => return Err(KitError::Agent(format!("{code}: {message}"))),
            Err(e) => return not_running(e),
        };
        let (document, snapshot) = match opened {
            Opened::NotLive(reason) => return Ok(Open::NotLive(NotLive::Reason(reason))),
            Opened::Live(document) => (document, None),
            Opened::NeedsImport(importer) => {
                let snapshot = Self::import_snapshot(&file, file_hash.as_ref())?;
                match importer.import(&snapshot) {
                    Ok(document) => (document, Some(snapshot)),
                    Err(craft_cloud_app::Error::Refused { code, .. }) if code == "not_live" => {
                        return Ok(Open::NotLive(NotLive::Reason(NotLiveReason::Offline)));
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        };
        let ours = snapshot.as_deref().is_some_and(|s| s == document.state()) && document.state_updates().is_empty();
        let mut session = Self::start(agent.clone(), path, file, (document.state(), document.state_updates()), ours)?;
        // Decided once, here: the agent knows every Craft save of the file (its records, the server's versions'); the
        // bytes the document was imported from are no edit either.
        session.status.changed_outside =
            snapshot.is_none() && document.changed_outside() == Some(true) && file_hash.is_some_and(|h| import_hash(&session.doc) != Some(hex(&h)));
        session.attach(Link { _docs: docs, doc: document });
        Ok(Open::Live(Box::new(session)))
    }

    /// The snapshot that makes `file` a live document: its schema, the hash of the bytes it was read from, its parts.
    fn import_snapshot(file: &D::Doc, hash: Option<&[u8; 32]>) -> Result<Vec<u8>, KitError> {
        let doc = new_doc();
        let mut schema = HashMap::new();
        schema.insert("app".to_owned(), LoroValue::from(D::APP));
        schema.insert("version".to_owned(), LoroValue::from(i64::from(D::SCHEMA)));
        doc.get_map(ROOT).insert(SCHEMA, LoroValue::from(schema))?;
        if let Some(hash) = hash {
            doc.get_map(ROOT).insert(IMPORT, LoroValue::from(hex(hash)))?;
        }
        let mut ctx = Ctx::default();
        let mut w = Writer::new(&doc, KIT_ORIGIN);
        write_diff::<D>(&mut w, None, file, &mut ctx)?;
        w.finish()?;
        Ok(doc.export(loro::ExportMode::Snapshot)?)
    }

    /// A session over the document `state` (a snapshot), for `file` (kept as the model when `ours`: the state is the
    /// import just made from it).
    fn start(agent: Agent, path: &str, file: Arc<D::Doc>, state: (&[u8], &[Vec<u8>]), ours: bool) -> Result<Self, KitError> {
        let layout = D::mappings(&file).layout()?;
        // A fresh random Loro peer of its own: never one another session may have written with.
        let doc = new_doc();
        doc.import(state.0)?;
        // The updates that are part of the state.
        let mut unread = None;
        for update in state.1 {
            if let Err(e) = import_slices(&doc, &parts(update).unwrap_or_default()) {
                unread = Some(format!("a change from another machine doesn't read: {e}"));
            }
        }
        let collector = Arc::new(Mutex::new(Collector::default()));
        let c = collector.clone();
        let events = doc.subscribe_root(Arc::new(move |event: DiffEvent<'_>| collect(&c, event)));
        let undo = new_undo(&doc);
        let mut session = Self {
            doc,
            undo,
            model: file,
            ctx: Ctx::default(),
            layout,
            collector,
            _events: events,
            agent,
            path: path.to_owned(),
            link: None,
            reopening: None,
            next_try: Instant::now(),
            backoff: FIRST_BACKOFF,
            outbox: VecDeque::new(),
            outbox_bytes: 0,
            incoming: Vec::new(),
            held: false,
            grouping: false,
            status: Status::default(),
            peers: HashMap::new(),
            presence: None,
            presence_sent: None,
            waker: None,
            refused: None,
            doc_id: None,
            deferred: Vec::new(),
            step_sizes: HashMap::new(),
            group_key: None,
            model_stale: false,
            resend_at: Instant::now(),
            resend_backoff: FIRST_BACKOFF,
            records: VecDeque::new(),
            file_version: None,
            caught_up_at: None,
            changed_since_save: false,
            step_names: HashMap::new(),
        };
        session.reindex_trees();
        session.check_schema();
        session.status.error = unread;
        if !ours {
            session.rebuild(Touched::everything())?;
        }
        Ok(session)
    }

    fn reindex_trees(&mut self) {
        for (key, kind) in &self.layout {
            if *kind == Kind::Tree {
                self.ctx.trees.insert((*key).to_owned(), TreeIndex::build(&self.doc.get_tree(*key)));
            }
        }
    }

    fn check_schema(&mut self) {
        self.status.read_only = match schema(&self.doc) {
            Some((app, version)) if app == D::APP && version <= D::SCHEMA => None,
            Some((app, version)) => Some(ReadOnly { app, version }),
            None => Some(ReadOnly { app: String::new(), version: 0 }),
        };
    }

    fn attach(&mut self, link: Link) {
        let state = link.doc.opened_state();
        self.doc_id.get_or_insert(link.doc.id());
        self.status.connected = true;
        self.status.online = state.online;
        self.status.waiting = state.waiting;
        if let Some(waker) = &self.waker {
            link.doc.set_waker(waker.clone());
        }
        // What the agent's copy has of this peer's changes: the rest goes again. (The peer is this session's alone,
        // random and never handed out, so whatever the agent has under it is this session's.)
        let ours = self.doc.peer_id();
        let held = std::iter::once(link.doc.state())
            .chain(link.doc.state_updates().iter().flat_map(|u| parts(u).unwrap_or_default()))
            .filter_map(|bytes| loro::LoroDoc::decode_import_blob_meta(bytes, false).ok()?.partial_end_vv.get(&ours).copied())
            .max()
            .unwrap_or(0);
        self.outbox.retain(|o| o.end > held);
        for out in &mut self.outbox {
            out.seq = None;
        }
        self.outbox_bytes = self.outbox.iter().map(|o| o.bytes.len()).sum();
        // Saves made while the agent was away, and the last ones again (the agent keeps a record once): each after the
        // changes it covers (announced at once).
        for kept in &mut self.records {
            kept.sent = false;
            kept.announced = false;
        }
        self.link = Some(link);
        self.backoff = FIRST_BACKOFF;
        self.send_outbox();
    }

    /// The model as it is now (after the last local change or applied remote one).
    pub fn model(&self) -> &Arc<D::Doc> {
        &self.model
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    /// The live document's path, as opened.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// What undo means now. A document that isn't live has no session: the app keeps its own undo.
    pub fn undo_kind(&self) -> UndoKind {
        if self.status.read_only.is_some() { UndoKind::None } else { UndoKind::Live }
    }

    /// Calls `waker` (from another thread) whenever something comes from the agent: an egui app passes its
    /// `request_repaint`, and polls on the next frame.
    pub fn set_waker(&mut self, waker: Waker) {
        if let Some(link) = &self.link {
            link.doc.set_waker(waker.clone());
        }
        self.waker = Some(waker);
    }

    /// While on, remote changes are kept and applied only once it is off again (at the next poll): for a gesture
    /// (a drag, an IME composition) whose `old` model must stay the session's until its command ends.
    pub fn hold_remote(&mut self, on: bool) {
        self.held = on;
    }

    /// The hook: `old` is the model before the command (the session's), `new` after. Writes the difference as one undo
    /// step and sends it. Nothing is written for a read-only or stalled document, or from an `old` that isn't the
    /// session's model ([`KitError::Stale`]: take [`Session::model`] and make the change again).
    pub fn local_change(&mut self, old: &Arc<D::Doc>, new: &Arc<D::Doc>) -> Result<(), KitError> {
        if let Some(ro) = &self.status.read_only {
            return Err(KitError::ReadOnly { app: ro.app.clone(), version: ro.version });
        }
        if self.model_stale {
            return Err(KitError::ModelStale);
        }
        if !Arc::ptr_eq(old, &self.model) {
            return Err(KitError::Stale);
        }
        if Arc::ptr_eq(old, new) {
            return Ok(());
        }
        if self.outbox_bytes >= MAX_QUEUE_BYTES {
            self.status.stalled = true;
            return Err(KitError::Stalled(self.outbox_bytes));
        }
        let before = self.doc.oplog_vv();
        self.changed_since_save = true;
        let outer = !self.grouping;
        if outer {
            let _ = self.undo.group_start();
        }
        let mut w = Writer::new(&self.doc, LOCAL_ORIGIN);
        let wrote = write_diff::<D>(&mut w, Some(old.as_ref()), new.as_ref(), &mut self.ctx);
        let text_bytes = w.text_bytes;
        let finished = w.finish();
        if outer {
            self.undo.group_end();
        }
        // The step's size, by where it starts (the undo manager's item names that start; a group's first change).
        let start = before.get(&self.doc.peer_id()).copied().unwrap_or(0);
        let key = if self.grouping { *self.group_key.get_or_insert(start) } else { start };
        *self.step_sizes.entry(key).or_default() += text_bytes;
        let updates = match finished {
            Ok((updates, _)) => updates,
            Err(_) => {
                let mut rest = Vec::new();
                let _ = export_since(&self.doc, &before, &mut rest);
                rest
            }
        };
        self.queue(updates);
        match wrote {
            Ok(()) => {
                self.model = new.clone();
                Ok(())
            }
            Err(e) => {
                // What was written stays (and goes out); the model is built again from the document, so they agree.
                if self.doc.oplog_vv() != before {
                    self.reindex_trees();
                    let _ = self.rebuild(Touched::everything());
                }
                self.status.error = Some(e.to_string());
                Err(e)
            }
        }
    }

    /// A change the app makes by itself rather than the user (a media upload that finished and now has its chunk
    /// reference, bytes read back into the model): written and sent like [`Session::local_change`], with the same
    /// rules for `old`, but never an undo step, so undo never takes it back.
    pub fn background_change(&mut self, old: &Arc<D::Doc>, new: &Arc<D::Doc>) -> Result<(), KitError> {
        if let Some(ro) = &self.status.read_only {
            return Err(KitError::ReadOnly { app: ro.app.clone(), version: ro.version });
        }
        if self.model_stale {
            return Err(KitError::ModelStale);
        }
        if !Arc::ptr_eq(old, &self.model) {
            return Err(KitError::Stale);
        }
        if Arc::ptr_eq(old, new) {
            return Ok(());
        }
        if self.outbox_bytes >= MAX_QUEUE_BYTES {
            self.status.stalled = true;
            return Err(KitError::Stalled(self.outbox_bytes));
        }
        let before = self.doc.oplog_vv();
        let mut w = Writer::new(&self.doc, KIT_ORIGIN);
        let wrote = write_diff::<D>(&mut w, Some(old.as_ref()), new.as_ref(), &mut self.ctx);
        let updates = match w.finish() {
            Ok((updates, _)) => updates,
            Err(_) => {
                let mut rest = Vec::new();
                let _ = export_since(&self.doc, &before, &mut rest);
                rest
            }
        };
        if self.doc.oplog_vv() != before {
            self.changed_since_save = true;
        }
        self.queue(updates);
        match wrote {
            Ok(()) => {
                self.model = new.clone();
                Ok(())
            }
            Err(e) => {
                if self.doc.oplog_vv() != before {
                    self.reindex_trees();
                    let _ = self.rebuild(Touched::everything());
                }
                self.status.error = Some(e.to_string());
                Err(e)
            }
        }
    }

    /// Makes every change `f` makes one undo step (typing a word, a drag's previews).
    pub fn group<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let outer = self.begin_group();
        let r = f(self);
        if outer {
            self.end_group();
        }
        r
    }

    /// Starts a group of changes that undo as one step, until [`Session::end_group`]. False if one is open already.
    pub fn begin_group(&mut self) -> bool {
        if self.grouping {
            return false;
        }
        let _ = self.undo.group_start();
        self.grouping = true;
        self.group_key = None;
        true
    }

    pub fn end_group(&mut self) {
        if self.grouping {
            self.undo.group_end();
            self.grouping = false;
            self.group_key = None;
        }
    }

    /// Whether there is a step to undo now (a step too big to undo while live isn't one: see
    /// [`Session::undo_blocked`]).
    pub fn can_undo(&self) -> bool {
        self.status.read_only.is_none() && self.undo.can_undo() && self.too_big(true).is_none()
    }

    pub fn can_redo(&self) -> bool {
        self.status.read_only.is_none() && self.undo.can_redo() && self.too_big(false).is_none()
    }

    /// When the next undo is of a step whose text changes are over [`MAX_UNDO_TEXT_BYTES`]: why it can't be undone
    /// (for the undo menu). Such a step is a barrier: nothing before it can be undone either.
    pub fn undo_blocked(&self) -> Option<String> {
        self.too_big(true).map(|bytes| KitError::UndoTooLarge { bytes }.to_string())
    }

    /// The text bytes of the top undo (or redo) step, when over the limit.
    fn too_big(&self, undo: bool) -> Option<usize> {
        let start = self.top_start(undo)?;
        self.step_sizes.get(&start).copied().filter(|bytes| *bytes > MAX_UNDO_TEXT_BYTES)
    }

    /// Where the step at the top of the undo (or redo) stack starts.
    fn top_start(&self, undo: bool) -> Option<i32> {
        match if undo { self.undo.top_undo_value() } else { self.undo.top_redo_value() }? {
            LoroValue::I64(start) => i32::try_from(start).ok(),
            _ => None,
        }
    }

    /// The name of the step undo (`true`) or redo would take back, if it has one (as "Replace with the file's
    /// content", [`Session::replace_with_file`]): for the app's Undo and Redo labels.
    pub fn step_name(&self, undo: bool) -> Option<&'static str> {
        self.top_start(undo).and_then(|s| self.step_names.get(&s)).copied()
    }

    /// The user chose to bring the file's content in (the app showed [`Status::changed_outside`] and offered it; the
    /// kit never does by itself): `file` (the app's model of the file as it read it) replaces the live content, as one
    /// ordinary local change, one undo step named [`REPLACE_WITH_FILE`], that goes to every machine. It replaces the
    /// live edits made since the file was written; undo brings them back.
    pub fn replace_with_file(&mut self, file: D::Doc) -> Result<(), KitError> {
        let mut file = file;
        D::refresh_derived(&mut file);
        let old = self.model.clone();
        let start = self.doc.oplog_vv().get(&self.doc.peer_id()).copied().unwrap_or(0);
        let key = if self.grouping { self.group_key.unwrap_or(start) } else { start };
        self.local_change(&old, &Arc::new(file))?;
        // Named only if it made a step (the file may be the live content already: then the next edit starts there).
        if self.doc.oplog_vv().get(&self.doc.peer_id()).copied().unwrap_or(0) > start {
            self.step_names.insert(key, REPLACE_WITH_FILE);
        }
        self.status.changed_outside = false;
        Ok(())
    }

    /// Takes back this machine's last change (remote changes since stay). The model changes as after a remote change:
    /// take [`Session::model`]. False if there was nothing to undo. While the app holds remote changes back
    /// ([`Session::hold_remote`]: a gesture), the undo waits: it answers false now and runs at the first poll after the
    /// gesture ends (which then says the model changed).
    pub fn undo(&mut self) -> Result<bool, KitError> {
        self.undo_redo(true)
    }

    pub fn redo(&mut self) -> Result<bool, KitError> {
        self.undo_redo(false)
    }

    fn undo_redo(&mut self, undo: bool) -> Result<bool, KitError> {
        if let Some(ro) = &self.status.read_only {
            return Err(KitError::ReadOnly { app: ro.app.clone(), version: ro.version });
        }
        if self.outbox_bytes >= MAX_QUEUE_BYTES {
            self.status.stalled = true;
            return Err(KitError::Stalled(self.outbox_bytes));
        }
        // Mid-gesture nothing changes the model under the app (its `old` must stay the session's): the undo waits for
        // the gesture's end, and runs at the first poll after it.
        if self.held {
            self.deferred.push(undo);
            return Ok(false);
        }
        self.end_group();
        // A step whose text is too big for Loro's undo while live (it takes time growing with the square of the text):
        // refused, and it stays on the stack as a barrier.
        if let Some(bytes) = self.too_big(undo) {
            return Err(KitError::UndoTooLarge { bytes });
        }
        let top = self.top_start(undo);
        let size = top.and_then(|s| self.step_sizes.get(&s)).copied().unwrap_or(0);
        let name = top.and_then(|s| self.step_names.get(&s)).copied();
        // What came meanwhile first, so the step applies to the newest document.
        self.apply_incoming()?;
        let before = self.doc.oplog_vv();
        lock(&self.collector).on = true;
        let done = if undo { self.undo.undo() } else { self.undo.redo() };
        let raw = std::mem::take(&mut lock(&self.collector).raw);
        lock(&self.collector).on = false;
        let done = done?;
        self.changed_since_save |= done;
        // The step that takes this one back (on the other stack) starts where this undo's commit does: as big, and
        // with its name.
        let start = before.get(&self.doc.peer_id()).copied().unwrap_or(0);
        self.step_sizes.insert(start, size);
        if let Some(name) = name {
            self.step_names.insert(start, name);
        }
        let mut updates = Vec::new();
        export_since(&self.doc, &before, &mut updates)?;
        self.queue(updates);
        if done {
            let touched = self.classify(raw);
            self.rebuild(touched)?;
        }
        Ok(done)
    }

    /// Takes what came from the agent and applies remote changes (unless held). Never blocks; call it once per frame.
    pub fn poll(&mut self) -> Polled {
        self.take_events();
        self.reconnect();
        let mut changed = if self.held {
            false
        } else {
            match self.apply_incoming() {
                Ok(changed) => changed,
                Err(e) => {
                    self.status.error = Some(e.to_string());
                    false
                }
            }
        };
        // A model left behind by a failed rebuild: a whole rebuild, tried again each poll.
        if self.model_stale && !self.held {
            self.reindex_trees();
            if self.rebuild(Touched::everything()).is_ok() {
                self.status.error = None;
                changed = true;
            }
        }
        // Undos asked for during a gesture, now that it is over, in order.
        if !self.held && !self.model_stale {
            for undo in std::mem::take(&mut self.deferred) {
                match self.undo_redo(undo) {
                    Ok(done) => changed |= done,
                    Err(e) => self.status.error = Some(e.to_string()),
                }
            }
        }
        self.keep_presence();
        // What a refusal for now held back goes again once its pause is over.
        if self.outbox.iter().any(|o| o.seq.is_none()) {
            self.send_outbox();
        }
        self.status.queued = self.outbox.len();
        self.status.queued_bytes = self.outbox_bytes;
        self.status.stalled = self.outbox_bytes >= MAX_QUEUE_BYTES;
        self.status.stopped |= self.refused.is_some() || self.status.replaced;
        Polled { changed, status: self.status.clone() }
    }

    fn take_events(&mut self) {
        let mut closed = None;
        let mut gap = false;
        let mut events = Vec::new();
        if let Some(link) = &self.link {
            while let Some(event) = link.doc.try_recv() {
                events.push(event);
            }
        }
        for event in events {
            match event {
                Event::Update(bytes) => self.incoming.push(bytes),
                Event::Presence(bytes) => {
                    if let Some((peer, payload)) = bytes.split_first_chunk::<8>() {
                        let peer = u64::from_be_bytes(*peer);
                        if peer != self.doc.peer_id() {
                            self.peers.insert(peer, Peer { bytes: payload.to_vec(), seen: Instant::now() });
                        }
                    }
                }
                Event::Acked(seq) => {
                    if let Some(at) = self.outbox.iter().position(|o| o.seq == Some(seq))
                        && let Some(out) = self.outbox.remove(at)
                    {
                        self.outbox_bytes = self.outbox_bytes.saturating_sub(out.bytes.len());
                        self.resend_backoff = FIRST_BACKOFF;
                    }
                }
                Event::Refused { seq, code, message } => {
                    // Never dropped: every later change depends on it.
                    let Some(at) = self.outbox.iter().position(|o| o.seq == Some(seq)) else { continue };
                    if code == "invalid" || code == "too_large" {
                        // It can never be taken: nothing more goes out (what follows needs it), and the app hears
                        // it is stopped. The model stays editable; saving the file (without a record) keeps the work.
                        let why = format!("Craft Cloud can't take this document's changes ({code}: {message}); save it to the file");
                        self.refused = Some(why.clone());
                        self.status.error = Some(why);
                    } else {
                        // For now (the agent's disk): it and everything after it go again, in order, after a pause.
                        for out in self.outbox.iter_mut().skip(at) {
                            out.seq = None;
                        }
                        self.resend_at = Instant::now() + self.resend_backoff;
                        self.resend_backoff = (self.resend_backoff * 2).min(MAX_BACKOFF);
                        self.status.error = Some(format!("Craft Cloud couldn't keep a change yet ({code}: {message}); trying again"));
                    }
                }
                Event::Status { online, waiting, deleted, stopped } => {
                    self.status.online = online;
                    self.status.waiting = waiting;
                    self.status.deleted = deleted;
                    self.status.stopped = stopped || self.refused.is_some() || self.status.replaced;
                }
                Event::File(frontiers) => self.file_version = frontiers,
                Event::Gap => gap = true,
                Event::Closed(why) => closed = Some(why),
            }
        }
        if let Some(why) = closed {
            self.drop_link(&why);
        } else if gap {
            // A change another machine sent in fragments didn't come whole: nothing of it was applied. The document
            // is opened again at once, and its state brings the change (or what of it is still coming).
            self.status.gaps += 1;
            self.drop_link("a change from another machine came in part; catching up");
            self.reconnect_now();
        }
    }

    fn drop_link(&mut self, why: &str) {
        self.link = None;
        self.status.connected = false;
        self.status.online = false;
        self.status.error = Some(format!("Craft Cloud: {why}"));
        self.next_try = Instant::now() + self.backoff;
        for out in &mut self.outbox {
            out.seq = None;
        }
        for kept in &mut self.records {
            kept.sent = false;
            kept.announced = false;
        }
    }

    /// Tries to reach the agent again at the next poll, without waiting out the pause (the app learnt the agent is
    /// back: Craft Cloud's status says so, say).
    pub fn reconnect_now(&mut self) {
        self.next_try = Instant::now();
        self.backoff = FIRST_BACKOFF;
    }

    /// Opens the document again off the UI thread, with a growing pause between tries.
    fn reconnect(&mut self) {
        if self.link.is_some() || self.status.replaced {
            return;
        }
        if let Some(rx) = &self.reopening {
            match rx.try_recv() {
                Ok(Ok((docs, doc))) => {
                    self.reopening = None;
                    self.incoming.push(doc.state().to_vec());
                    self.incoming.extend(doc.state_updates().iter().cloned());
                    self.status.error = None;
                    self.attach(Link { _docs: docs, doc });
                    if let Some(waker) = &self.waker {
                        waker();
                    }
                }
                Ok(Err(Reopen::Retry(why))) => {
                    self.reopening = None;
                    self.status.error = Some(why);
                    self.next_try = Instant::now() + self.backoff;
                    self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
                }
                Ok(Err(Reopen::Replaced(why))) => {
                    // Stopped for good: the model stays, editable, for the app to save elsewhere.
                    self.reopening = None;
                    self.status.replaced = true;
                    self.status.stopped = true;
                    self.status.error = Some(format!("document_replaced: {why}; save this document as a new file"));
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.reopening = None,
            }
            return;
        }
        if Instant::now() < self.next_try {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let (agent, path) = (self.agent.clone(), self.path.clone());
        // The document as this machine has it, in case the agent lost it (it asks for an import then).
        let snapshot = self.doc.export(loro::ExportMode::Snapshot).unwrap_or_default();
        let waker = self.waker.clone();
        let expected = self.doc_id;
        let options = OpenOptions::default();
        let spawned = std::thread::Builder::new().name("craft-cloud-kit-reopen".into()).spawn(move || {
            let result = (|| -> Reopened {
                let retry = |e: craft_cloud_app::Error| Reopen::Retry(e.to_string());
                let client = agent.client().map_err(retry)?;
                let docs = client.documents().map_err(retry)?;
                match docs.open_with(&path, D::APP, &options).map_err(retry)? {
                    // The same document, or none known yet (it was never attached).
                    Opened::Live(doc) if expected.is_none_or(|id| id == doc.id()) => Ok((docs, doc)),
                    Opened::Live(_) => Err(Reopen::Replaced(format!("{path} is another document now"))),
                    // The path's file was never live: another file took its place. Nothing is imported over it.
                    Opened::NeedsImport(_) if expected.is_some() => {
                        Err(Reopen::Replaced(format!("{path} is another file now, or Craft Cloud lost its copy of the document")))
                    }
                    Opened::NeedsImport(importer) => importer.import(&snapshot).map(|doc| (docs, doc)).map_err(retry),
                    Opened::NotLive(reason) => Err(Reopen::Retry(format!("the document can't be live now ({reason:?})"))),
                }
            })();
            let _ = tx.send(result);
            if let Some(waker) = waker {
                waker();
            }
        });
        match spawned {
            Ok(_) => self.reopening = Some(rx),
            Err(e) => {
                self.status.error = Some(e.to_string());
                self.next_try = Instant::now() + self.backoff;
            }
        }
    }

    /// Imports what came and hands the app the touched parts. True if the model changed.
    fn apply_incoming(&mut self) -> Result<bool, KitError> {
        if self.incoming.is_empty() {
            return Ok(false);
        }
        let batch = std::mem::take(&mut self.incoming);
        // One by one: Loro's batch import of much text takes time growing with its square, and one at a time also
        // means only an update Loro refuses is lost (the agent checked them all already).
        lock(&self.collector).on = true;
        for bytes in &batch {
            // (A bundle's slices one after the other: the model sees them only together, below.)
            match parts(bytes) {
                Some(updates) => {
                    if let Err(e) = import_slices(&self.doc, &updates) {
                        self.status.error = Some(format!("a change from another machine doesn't read: {e}"));
                    }
                }
                None => self.status.error = Some("a change from another machine doesn't read (a bundle cut short)".into()),
            }
        }
        let raw = std::mem::take(&mut lock(&self.collector).raw);
        lock(&self.collector).on = false;
        // (Each update is a whole change: one that came in fragments was joined before it got here.)
        let touched = self.classify(raw);
        let was_read_only = self.status.read_only.clone();
        if touched.kit {
            self.check_schema();
        }
        if !touched.any() {
            return Ok(was_read_only != self.status.read_only);
        }
        self.rebuild(touched)?;
        Ok(true)
    }

    /// Sorts Loro's raw touches out by part.
    fn classify(&mut self, raw: Vec<Raw>) -> Touched {
        let mut touched = Touched::default();
        let mut reindex = Vec::new();
        let mut nested: Vec<(&'static str, TreeID, String, Option<Vec<loro::TextDelta>>)> = Vec::new();
        for r in raw {
            if r.root == ROOT {
                touched.kit = true;
                continue;
            }
            if r.root == crate::opaque::ROOT {
                for k in r.keys {
                    if self.layout.iter().any(|(key, kind)| *key == k && *kind == Kind::Opaque) {
                        touched.opaques.insert(k);
                    }
                }
                continue;
            }
            let (part, suffix) = r.root.split_once('.').map_or((r.root.as_str(), ""), |(p, s)| (p, s));
            let Some((key, kind)) = self.layout.iter().find(|(k, _)| *k == part).copied() else { continue };
            let ids = || r.keys.iter().filter_map(|k| ElemId::from_hex(k)).collect::<Vec<_>>();
            match kind {
                Kind::Tree => {
                    let t = touched.trees.entry(key.to_owned()).or_insert_with(TreeTouch::default);
                    if r.tree {
                        t.structure = true;
                        reindex.push(key);
                    }
                    // A node the index doesn't name (deleted, or a second node carrying an element's ID) still names its
                    // element in its meta: the element is touched.
                    let tree = self.doc.get_tree(key);
                    t.nodes.extend(
                        r.nodes.iter().filter_map(|n| {
                            self.ctx.trees.get(key).and_then(|i| i.elem_of.get(n)).copied().or_else(|| crate::tree::node_id(&tree, *n))
                        }),
                    );
                    // Inside a node's child container (its story, its table): named once the index is current.
                    if let Some(tid) = r.node
                        && !r.under_node.is_empty()
                    {
                        nested.push((key, tid, r.under_node.join("/"), r.text.clone()));
                    }
                }
                Kind::Text => {
                    touched.texts.insert(key.to_owned());
                    if let Some(delta) = r.text.clone()
                        && suffix.is_empty()
                    {
                        touched.text_deltas.entry(key.to_owned()).or_default().push(delta);
                    }
                }
                Kind::Table => {
                    let t = touched.tables.entry(key.to_owned()).or_insert_with(TableTouch::default);
                    match suffix {
                        "rows" => t.rows = true,
                        "cols" => t.columns = true,
                        _ => {
                            t.cells.extend(r.keys.iter().filter_map(|k| parse_cell_key(k)));
                            // A map cell's story: the cell, then the story's key.
                            if let (Some(cell), Some(story)) = (r.under_root.first().and_then(|k| parse_cell_key(k)), r.under_root.get(1..))
                                && !story.is_empty()
                            {
                                let story = story.join("/");
                                t.stories.insert((cell.0, cell.1, story.clone()));
                                if let Some(delta) = r.text.clone() {
                                    touched
                                        .cell_text_deltas
                                        .entry(key.to_owned())
                                        .or_default()
                                        .entry((cell.0, cell.1, story))
                                        .or_default()
                                        .push(delta);
                                }
                            }
                        }
                    }
                }
                Kind::Timeline => {
                    let t = touched.timelines.entry(key.to_owned()).or_insert_with(TimelineTouch::default);
                    match suffix {
                        "tracks" => t.tracks = true,
                        "track" => t.track_props.extend(ids()),
                        "items" => t.items.extend(ids()),
                        _ => t.keyframes.extend(ids()),
                    }
                }
                Kind::Opaque => {}
            }
        }
        for key in reindex {
            let tree = self.doc.get_tree(key);
            let index = TreeIndex::build(&tree);
            if let Some(t) = touched.trees.get_mut(key) {
                // Created nodes: their IDs are known only now. And an element whose node is another one now (two nodes
                // came to carry its ID, and the other one wins): its content may be another, so it is touched.
                let old = self.ctx.trees.get(key);
                for (id, tid) in &index.tid_of {
                    if old.and_then(|o| o.tid_of.get(id)) != Some(tid) {
                        t.nodes.insert(*id);
                    }
                }
            }
            self.ctx.trees.insert(key.to_owned(), index);
        }
        for (key, tid, story, delta) in nested {
            let Some(id) = self.ctx.trees.get(key).and_then(|i| i.elem_of.get(&tid)).copied() else { continue };
            if let Some(t) = touched.trees.get_mut(key) {
                t.nodes.insert(id);
                t.stories.insert((id, story.clone()));
            }
            if let Some(delta) = delta {
                touched.node_text_deltas.entry(key.to_owned()).or_default().entry((id, story)).or_default().push(delta);
            }
        }
        touched
    }

    /// Rebuilds the model from the touched parts (see [`Session::rebuild_parts`]). If that fails, the document has
    /// moved on and the model hasn't: the model is marked stale, local changes are refused ([`KitError::ModelStale`])
    /// so no diff is ever written from it, and each poll tries a whole rebuild until one succeeds.
    fn rebuild(&mut self, touched: Touched) -> Result<(), KitError> {
        let whole = touched.all;
        match self.rebuild_parts(touched) {
            Ok(()) => {
                // Only a whole rebuild catches the model up: a part rebuilt since leaves the failed one behind.
                if whole {
                    self.model_stale = false;
                }
                Ok(())
            }
            Err(e) => {
                self.model_stale = true;
                self.status.error = Some(format!("the model couldn't take a change from the live document: {e}"));
                Err(e)
            }
        }
    }

    /// Rebuilds the model from the touched parts, normalises it (writing back what that repaired), refreshes what is
    /// derived, and makes it the session's.
    fn rebuild_parts(&mut self, touched: Touched) -> Result<(), KitError> {
        let touched_all = touched.all;
        let raw = {
            // The old parts, for the texts to be patched rather than read whole.
            let old_parts = D::mappings(&self.model);
            let patch = RemotePatch { doc: &self.doc, touched: &touched, trees: &self.ctx.trees, old: (!touched.all).then_some(&old_parts) };
            D::apply_remote(&self.model, &patch)?
        };
        let mut fixed = raw.clone();
        D::normalise(Arc::make_mut(&mut fixed));
        // A repair is written back only from a model that has the whole document: one still stale in a part not
        // rebuilt would write that part's old state over the document (deleting what the other machine added).
        if self.status.read_only.is_none() && (touched_all || !self.model_stale) {
            let mut w = Writer::new(&self.doc, KIT_ORIGIN);
            let wrote = write_diff::<D>(&mut w, Some(raw.as_ref()), fixed.as_ref(), &mut self.ctx);
            let (updates, _) = w.finish()?;
            self.queue(updates);
            wrote?;
        }
        D::refresh_derived(Arc::make_mut(&mut fixed));
        self.model = fixed;
        Ok(())
    }

    /// Keeps changes for the agent and sends them if connected.
    fn queue(&mut self, updates: Updates) {
        if updates.is_empty() || self.status.read_only.is_some() {
            return;
        }
        let end = self.doc.oplog_vv().get(&self.doc.peer_id()).copied().unwrap_or(0);
        for bytes in updates {
            if bytes.len() > MAX_CHANGE_BYTES && self.refused.is_none() {
                // It can never go (nor anything after it, which needs it): the session stops, the model stays.
                let why =
                    format!("a change of {} bytes is over what Craft Cloud takes ({MAX_CHANGE_BYTES}); save the document to the file", bytes.len());
                self.refused = Some(why.clone());
                self.status.error = Some(why);
            }
            self.outbox_bytes = self.outbox_bytes.saturating_add(bytes.len());
            self.outbox.push_back(Out { bytes, end, seq: None });
        }
        self.send_outbox();
    }

    fn send_outbox(&mut self) {
        let Some(link) = &self.link else { return };
        // Saves on their way, announced at once (before the changes they cover, which may wait): the agent's sync then
        // waits for their records rather than take the files for edits outside Craft.
        // (Never while refused for good: those records can't come, and the agent's grace ends their wait.)
        for kept in self.records.iter_mut().filter(|k| !k.announced && !k.sent && self.refused.is_none()) {
            if link.doc.saving(kept.record.hash).is_ok() {
                kept.announced = true;
            }
        }
        // Refused for good: nothing after it can be taken. Refused for now: not before the pause is over.
        if self.refused.is_some() || Instant::now() < self.resend_at {
            return;
        }
        let mut failed = None;
        for out in self.outbox.iter_mut().filter(|o| o.seq.is_none()) {
            match link.doc.send(&out.bytes) {
                Ok(seq) => out.seq = Some(seq),
                Err(e) => {
                    failed = Some(e.to_string());
                    break;
                }
            }
        }
        // Save records, on the same stream after every change they cover (the agent keeps one once those are in).
        if failed.is_none() {
            let unsent_from = self.outbox.iter().filter(|o| o.seq.is_none()).map(|o| o.end).min();
            for kept in self.records.iter_mut().filter(|k| !k.sent && unsent_from.is_none_or(|from| k.end < from)) {
                match link.doc.saved(kept.record.hash, &kept.record.frontiers) {
                    Ok(()) => kept.sent = true,
                    Err(e) => {
                        failed = Some(e.to_string());
                        break;
                    }
                }
            }
        }
        if let Some(why) = failed {
            self.drop_link(&why);
        }
    }

    /// Shows this app's cursor or selection to the document's other machines and apps (the bytes are the app's, at
    /// most 4 KiB less 8); `None` stops showing it. Sent now and again every [`PRESENCE_EVERY`].
    pub fn set_presence(&mut self, bytes: Option<Vec<u8>>) -> Result<(), KitError> {
        if let Some(b) = &bytes
            && b.len() + 8 > craft_cloud_api::limits::MAX_PRESENCE_BYTES
        {
            return Err(KitError::TooLarge { what: "a presence", len: b.len(), max: craft_cloud_api::limits::MAX_PRESENCE_BYTES - 8 });
        }
        self.presence = bytes;
        self.presence_sent = None;
        self.keep_presence();
        Ok(())
    }

    fn keep_presence(&mut self) {
        let now = Instant::now();
        self.peers.retain(|_, p| now.duration_since(p.seen) < PRESENCE_TTL);
        let (Some(link), Some(bytes)) = (&self.link, &self.presence) else { return };
        if self.presence_sent.is_some_and(|at| now.duration_since(at) < PRESENCE_EVERY) {
            return;
        }
        let mut frame = self.doc.peer_id().to_be_bytes().to_vec();
        frame.extend_from_slice(bytes);
        if link.doc.presence(&frame).is_ok() {
            self.presence_sent = Some(now);
        }
    }

    /// The other machines' and apps' presence, by their peer, as they sent it (dropped after [`PRESENCE_TTL`] quiet).
    pub fn peers(&self) -> &HashMap<u64, Peer> {
        &self.peers
    }

    /// Where a save starts: the model to write to the file and the live document's frontiers it matches. Refused
    /// ([`KitError::NoRecord`]) when a save now can't say what it holds: the document is read-only (a newer app's), the
    /// session is stalled or stopped, or the model is behind the live document (a remote change it couldn't take
    /// yet). The app then saves its own model without a record, as it would without Craft Cloud.
    ///
    /// The order of a save: `save_point`, the bytes from [`SavePoint::model`], [`Session::record`] with them, and only
    /// then the write to disk (the agent has to know the content before its sync pass sees it).
    pub fn save_point(&self) -> Result<SavePoint<D>, KitError> {
        if let Some(why) = self.no_record() {
            return Err(KitError::NoRecord(why.to_owned()));
        }
        let end = self.doc.oplog_vv().get(&self.doc.peer_id()).copied().unwrap_or(0);
        Ok(SavePoint { model: self.model.clone(), frontiers: self.frontiers(), end })
    }

    /// The live document's frontiers (its heads), sorted.
    fn frontiers(&self) -> Vec<(u64, u32)> {
        let mut heads: Vec<(u64, u32)> = self.doc.oplog_frontiers().iter().map(|id| (id.peer, u32::try_from(id.counter).unwrap_or(0))).collect();
        heads.sort_unstable();
        heads
    }

    /// Why a save now can't carry a record, if it can't.
    fn no_record(&self) -> Option<&'static str> {
        if self.status.read_only.is_some() {
            Some("the document is a newer app's (read-only here)")
        } else if self.outbox_bytes >= MAX_QUEUE_BYTES {
            Some("too much waits for Craft Cloud (stalled)")
        } else if self.status.stopped || self.refused.is_some() || self.status.replaced {
            Some("the live document stopped here")
        } else if self.model_stale {
            Some("the model is behind the live document")
        } else {
            None
        }
    }

    /// Whether the live document is strictly ahead of the file's record: everything the file was saved from is in it,
    /// and it has more.
    fn ahead_of_file(&self) -> bool {
        let Some(file) = &self.file_version else { return false };
        let vv = self.doc.oplog_vv();
        let held = file.iter().all(|(peer, counter)| i32::try_from(*counter).is_ok_and(|c| vv.includes_id(loro::ID::new(*peer, c))));
        held && *file != self.frontiers()
    }

    /// Whether the file on disk lacks edits the live document has: the record of the file as it is now (the agent's
    /// word, or this session's last save) is included in the live document's version and isn't it. An app autosaves
    /// when it is dirty or this says so, so the file catches up on its own with edits from other machines, and with
    /// its own edits after another machine's save won over this one's.
    ///
    /// False when the file's record has anything this document lacks (saved concurrently elsewhere, ahead of it, or
    /// unknown): this session is the one behind, and waits for the live updates; only the side that is ahead saves,
    /// and a file ahead of the live document is never written over by itself. False too when no record of the file is
    /// known (an edit outside Craft; not synced yet; an older agent), when a save couldn't carry a record, and for
    /// [`CATCH_UP_EVERY`] after a save it asked for (a backstop: at most one such save in that time).
    pub fn file_behind(&self) -> bool {
        if self.no_record().is_some() || self.caught_up_at.is_some_and(|at| at.elapsed() < CATCH_UP_EVERY) {
            return false;
        }
        self.ahead_of_file()
    }

    /// The app is about to write `bytes` (made from `point`'s model) to the document's file: tells the agent, on the
    /// same stream after every change the save covers (while it is away, the record is kept and told once it is back;
    /// it is added to the file's version then), so the version the agent uploads for that content carries the record.
    /// Call it before the write.
    pub fn record(&mut self, point: &SavePoint<D>, bytes: &[u8]) -> SaveRecord {
        let hash = *blake3::hash(bytes).as_bytes();
        // The file is a Craft save from now on (nothing is written to the document: the record is the agent's).
        self.status.changed_outside = false;
        if !self.changed_since_save && self.ahead_of_file() {
            // A save only the file being behind called for (no edit here since the last): the next waits a while.
            self.caught_up_at = Some(Instant::now());
        }
        self.changed_since_save = false;
        let record = SaveRecord { hash, frontiers: point.frontiers.clone() };
        // The file is this save from now on (until the agent says otherwise: the sync brought another one).
        self.file_version = Some(record.frontiers.clone());
        self.records.push_back(Kept { record: record.clone(), end: point.end, sent: false, announced: false });
        while self.records.len() > KEPT_RECORDS {
            self.records.pop_front();
        }
        self.send_outbox();
        record
    }

    /// The live document's state as a Loro snapshot (for tests and diagnostics).
    pub fn snapshot(&self) -> Result<Vec<u8>, KitError> {
        Ok(self.doc.export(loro::ExportMode::Snapshot)?)
    }

    /// For the kit's tests: an update as if it came from the agent (applied at the next poll).
    #[doc(hidden)]
    pub fn receive_for_tests(&mut self, update: Vec<u8>) {
        self.incoming.push(update);
    }

    /// This session's Loro peer.
    pub fn peer(&self) -> u64 {
        self.doc.peer_id()
    }
}

/// A slice's import this slow means Loro took its slow way: see [`import_slices`].
const SLOW_SLICE: Duration = Duration::from_millis(50);

/// Imports one change's slices (a bundle's, or the one update) one after the other. Each slice normally goes in in
/// milliseconds; but with this machine's own edits in the same text concurrent with the change, Loro (in one order
/// of the two peers) works the text out again from where the edits parted for each slice, so each costs more than the
/// last and a 1.2 MB paste took minutes. Once a slice is that slow, the rest goes in as one update (joined on a
/// detached fork, which only takes their operations), which Loro works out once (seconds for a megabyte). Undo is
/// kept either way. Every slice is read first: one that doesn't read refuses the change whole, nothing of it in.
/// Past that, the first error is returned; the slices after it are still imported.
fn import_slices(doc: &LoroDoc, slices: &[&[u8]]) -> Result<(), loro::LoroError> {
    for slice in slices {
        LoroDoc::decode_import_blob_meta(slice, true)?;
    }
    let mut first_error = None;
    let mut i = 0;
    while let Some(slice) = slices.get(i) {
        let started = Instant::now();
        if let Err(e) = doc.import(slice) {
            first_error.get_or_insert(e);
        }
        i += 1;
        let rest = slices.get(i..).unwrap_or_default();
        if rest.len() >= 2
            && started.elapsed() > SLOW_SLICE
            && let Some(one) = joined(doc, rest)
        {
            if let Err(e) = doc.import(&one) {
                first_error.get_or_insert(e);
            }
            break;
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// `updates` (which follow on from what `doc` holds) as one update, if they all read.
fn joined(doc: &LoroDoc, updates: &[&[u8]]) -> Option<Vec<u8>> {
    let fork = doc.fork();
    fork.detach();
    let before = fork.oplog_vv();
    for u in updates {
        fork.import(u).ok()?;
    }
    fork.export(loro::ExportMode::updates(&before)).ok()
}

/// Loro's events, noted while the collector is on (an import, an undo).
fn collect(collector: &Mutex<Collector>, event: DiffEvent<'_>) {
    let mut c = lock(collector);
    if !c.on {
        return;
    }
    for diff in &event.events {
        let first = diff.path.first().map(|(id, _)| id).unwrap_or(diff.target);
        let Some(root) = root_name(first) else { continue };
        let mut raw = Raw { root, ..Raw::default() };
        for (i, (_, index)) in diff.path.iter().enumerate() {
            match index {
                Index::Node(t) => {
                    raw.nodes.push(*t);
                    raw.node = Some(*t);
                    raw.under_node.clear();
                }
                Index::Key(k) => {
                    raw.keys.push(k.to_string());
                    if i > 0 {
                        raw.under_root.push(k.to_string());
                        if raw.node.is_some() {
                            raw.under_node.push(k.to_string());
                        }
                    }
                }
                Index::Seq(_) => {}
            }
        }
        match &diff.diff {
            Diff::Map(delta) => raw.keys.extend(delta.updated.keys().map(|k| k.to_string())),
            Diff::Tree(tree) => {
                raw.tree = true;
                raw.nodes.extend(tree.diff.iter().map(|item| item.target));
            }
            Diff::Text(delta) => raw.text = Some(delta.clone()),
            _ => {}
        }
        c.raw.push(raw);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// A 600 KB paste from another peer into a text this machine edited at the same moment, this machine's peer below
    /// the sender's (the order Loro is slow in: each slice costs more than the last, a minute for these): a few slices
    /// go in one by one, then the rest together, in seconds. The text is the paste and the edit, and this machine's
    /// undo step stays (it takes back only the edit).
    #[test]
    fn slices_that_turn_slow_go_in_together() {
        let base = LoroDoc::new();
        base.set_peer_id(1).unwrap();
        base.get_text("t").insert(0, "abc").unwrap();
        base.commit();
        let snapshot = base.export(loro::ExportMode::Snapshot).unwrap();
        let sender = LoroDoc::new();
        sender.set_peer_id(9).unwrap();
        sender.import(&snapshot).unwrap();
        let paste = "p".repeat(600 * 1024);
        sender.get_text("t").insert(3, &paste).unwrap();
        sender.commit();
        let end = sender.oplog_vv().get(&9).copied().unwrap();
        let mut slices = Vec::new();
        let mut at = 0;
        while at < end {
            let to = (at + craft_cloud_api::fragments::SLICE_OPS).min(end);
            slices.push(sender.export(loro::ExportMode::updates_in_range(vec![loro::IdSpan::new(9, at, to)])).unwrap());
            at = to;
        }
        let here = new_doc();
        here.set_peer_id(2).unwrap();
        here.import(&snapshot).unwrap();
        let mut undo = new_undo(&here);
        here.get_text("t").insert(0, "hello ").unwrap();
        here.commit();
        let started = Instant::now();
        import_slices(&here, &slices.iter().map(Vec::as_slice).collect::<Vec<_>>()).unwrap();
        assert!(started.elapsed() < Duration::from_secs(15), "{:?}", started.elapsed());
        assert_eq!(here.get_text("t").to_string(), format!("hello abc{paste}"));
        assert!(undo.undo().unwrap());
        assert_eq!(here.get_text("t").to_string(), format!("abc{paste}"));
    }

    /// A change with a slice that doesn't read is refused whole: nothing of its other slices goes in.
    #[test]
    fn a_change_with_a_slice_that_does_not_read_goes_in_not_at_all() {
        let sender = LoroDoc::new();
        sender.set_peer_id(9).unwrap();
        sender.get_text("t").insert(0, "abcdef").unwrap();
        sender.commit();
        let one = sender.export(loro::ExportMode::updates_in_range(vec![loro::IdSpan::new(9, 0, 3)])).unwrap();
        let two = sender.export(loro::ExportMode::updates_in_range(vec![loro::IdSpan::new(9, 3, 6)])).unwrap();
        let here = new_doc();
        assert!(import_slices(&here, &[one.as_slice(), b"not an update", two.as_slice()]).is_err());
        assert_eq!(here.get_text("t").to_string(), "", "nothing of it in");
        import_slices(&here, &[one.as_slice(), two.as_slice()]).unwrap();
        assert_eq!(here.get_text("t").to_string(), "abcdef");
    }
}
