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

use craft_cloud_app::Client;
use craft_cloud_app::live::{Document, Documents, Event, NotLiveReason, Opened, Waker};
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
/// How many save marks a document keeps (for [`Status::changed_outside`]).
const MAX_SAVES: usize = 32;
/// Undo steps kept.
const UNDO_STEPS: usize = 500;

/// The root map of the kit's own keys: `schema` (`{app, version}`).
const ROOT: &str = "root";
const SCHEMA: &str = "schema";
/// The root map of save marks: content hash (hex) to when (ms).
const SAVES: &str = "$saves";

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
    /// The agent stopped following the document until it restarts.
    pub stopped: bool,
    /// The file on disk wasn't saved by a Craft app from this document (edited outside): the app may bring it in as a
    /// local change, or keep the live content.
    pub changed_outside: bool,
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
}

impl Status {
    pub fn badge(&self) -> Badge {
        if self.read_only.is_some() {
            Badge::ReadOnly
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

/// What a save gives the agent: the content's hash and the live document's version it was made from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveRecord {
    pub hash: [u8; 32],
    /// Loro's frontiers, encoded.
    pub frontiers: Vec<u8>,
    /// The version vector (peer, end counter), so a version can be compared with another without Loro.
    pub version: Vec<(u64, u32)>,
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
    /// A text's delta (the text root's own).
    text: Option<Vec<loro::TextDelta>>,
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

/// The connection to the agent: the documents connection (kept alive) and the document.
struct Link {
    _docs: Documents,
    doc: Document,
}

type Reopened = Result<(Documents, Document), String>;

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
    /// The hash of the file's bytes as the app read them at open (for [`Status::changed_outside`]), until saved.
    read_from: Option<[u8; 32]>,
    /// The agent refused a change for good (it doesn't decode, it is over the cap): nothing more is sent, and the
    /// session says it is stopped; the model stays editable, and saving to the file (without a record) keeps the work.
    refused: Option<String>,
    /// The agent refused a change for now (its disk): the queue goes again, in order, from this moment.
    resend_at: Instant,
    resend_backoff: Duration,
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
                crate::tree::write(w, key, o, n.as_ref(), index)?;
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
                crate::table::write(w, key, o, n.as_ref())?;
            }
            Part::Timeline(n) => {
                let o = match old_part {
                    Some(Part::Timeline(o)) => Some(o.as_ref()),
                    _ => None,
                };
                crate::timeline::write(w, key, o, n.as_ref())?;
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

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Notes a save of content `hash` in the document's save marks, keeping the newest [`MAX_SAVES`].
fn mark_saved(w: &mut Writer<'_>, hash: &[u8; 32]) -> Result<(), KitError> {
    let saves = w.doc.get_map(SAVES);
    w.set_raw(&saves, &hex(hash), LoroValue::from(now_ms()), 8)?;
    let mut all: Vec<(i64, String)> = Vec::new();
    saves.for_each(|k, v| {
        let at = match v {
            ValueOrContainer::Value(LoroValue::I64(at)) => at,
            _ => 0,
        };
        all.push((at, k.to_owned()));
    });
    if all.len() > MAX_SAVES {
        all.sort();
        let extra = all.len() - MAX_SAVES;
        for (_, k) in all.into_iter().take(extra) {
            w.delete(&saves, &k)?;
        }
    }
    Ok(())
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
        let opened = match docs.open(path, D::APP) {
            Ok(o) => o,
            Err(craft_cloud_app::Error::Refused { code, message }) => return Err(KitError::Agent(format!("{code}: {message}"))),
            Err(e) => return not_running(e),
        };
        let file_hash = file_bytes.map(|b| *blake3::hash(b).as_bytes());
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
        let ours = snapshot.as_deref().is_some_and(|s| s == document.state());
        let mut session = Self::start(agent.clone(), path, file, document.state(), ours)?;
        if let Some(hash) = file_hash
            && snapshot.is_none()
        {
            // Looked at again as the agent's waiting updates come (a save mark may be among them).
            session.read_from = Some(hash);
            session.check_outside();
        }
        session.attach(Link { _docs: docs, doc: document });
        Ok(Open::Live(Box::new(session)))
    }

    /// The snapshot that makes `file` a live document: its schema, its parts, and a save mark for its bytes.
    fn import_snapshot(file: &D::Doc, hash: Option<&[u8; 32]>) -> Result<Vec<u8>, KitError> {
        let doc = new_doc();
        let mut schema = HashMap::new();
        schema.insert("app".to_owned(), LoroValue::from(D::APP));
        schema.insert("version".to_owned(), LoroValue::from(i64::from(D::SCHEMA)));
        doc.get_map(ROOT).insert(SCHEMA, LoroValue::from(schema))?;
        let mut ctx = Ctx::default();
        let mut w = Writer::new(&doc, KIT_ORIGIN);
        write_diff::<D>(&mut w, None, file, &mut ctx)?;
        if let Some(hash) = hash {
            mark_saved(&mut w, hash)?;
        }
        w.finish()?;
        Ok(doc.export(loro::ExportMode::Snapshot)?)
    }

    /// A session over the document `state` (a snapshot), for `file` (kept as the model when `ours`: the state is the
    /// import just made from it).
    fn start(agent: Agent, path: &str, file: Arc<D::Doc>, state: &[u8], ours: bool) -> Result<Self, KitError> {
        let layout = D::mappings(&file).layout()?;
        let doc = new_doc();
        doc.import(state)?;
        let collector = Arc::new(Mutex::new(Collector::default()));
        let c = collector.clone();
        let events = doc.subscribe_root(Arc::new(move |event: DiffEvent<'_>| collect(&c, event)));
        let mut undo = UndoManager::new(&doc);
        undo.add_exclude_origin_prefix(KIT_ORIGIN);
        undo.set_max_undo_steps(UNDO_STEPS);
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
            read_from: None,
            refused: None,
            resend_at: Instant::now(),
            resend_backoff: FIRST_BACKOFF,
        };
        session.reindex_trees();
        session.check_schema();
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

    /// Whether the bytes the app read at open are a save (or the import) of this document.
    fn check_outside(&mut self) {
        if let Some(hash) = self.read_from {
            self.status.changed_outside = self.doc.get_map(SAVES).get(&hex(&hash)).is_none();
        }
    }

    fn attach(&mut self, link: Link) {
        let state = link.doc.opened_state();
        self.status.connected = true;
        self.status.online = state.online;
        self.status.waiting = state.waiting;
        if let Some(waker) = &self.waker {
            link.doc.set_waker(waker.clone());
        }
        // What the agent's copy has of this peer's changes: the rest goes again.
        let ours = self.doc.peer_id();
        let held =
            loro::LoroDoc::decode_import_blob_meta(link.doc.state(), false).ok().and_then(|m| m.partial_end_vv.get(&ours).copied()).unwrap_or(0);
        self.outbox.retain(|o| o.end > held);
        for out in &mut self.outbox {
            out.seq = None;
        }
        self.outbox_bytes = self.outbox.iter().map(|o| o.bytes.len()).sum();
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
        let outer = !self.grouping;
        if outer {
            let _ = self.undo.group_start();
        }
        let mut w = Writer::new(&self.doc, LOCAL_ORIGIN);
        let wrote = write_diff::<D>(&mut w, Some(old.as_ref()), new.as_ref(), &mut self.ctx);
        let finished = w.finish();
        if outer {
            self.undo.group_end();
        }
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
        true
    }

    pub fn end_group(&mut self) {
        if self.grouping {
            self.undo.group_end();
            self.grouping = false;
        }
    }

    pub fn can_undo(&self) -> bool {
        self.status.read_only.is_none() && self.undo.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.status.read_only.is_none() && self.undo.can_redo()
    }

    /// Takes back this machine's last change (remote changes since stay). The model changes as after a remote change:
    /// take [`Session::model`]. False if there was nothing to undo.
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
        self.end_group();
        // What came meanwhile first, so the step applies to the newest document.
        self.apply_incoming()?;
        let before = self.doc.oplog_vv();
        lock(&self.collector).on = true;
        let done = if undo { self.undo.undo() } else { self.undo.redo() };
        let raw = std::mem::take(&mut lock(&self.collector).raw);
        lock(&self.collector).on = false;
        let done = done?;
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
        let changed = if self.held {
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
        self.keep_presence();
        // What a refusal for now held back goes again once its pause is over.
        if self.outbox.iter().any(|o| o.seq.is_none()) {
            self.send_outbox();
        }
        self.status.queued = self.outbox.len();
        self.status.queued_bytes = self.outbox_bytes;
        self.status.stalled = self.outbox_bytes >= MAX_QUEUE_BYTES;
        self.status.stopped |= self.refused.is_some();
        Polled { changed, status: self.status.clone() }
    }

    fn take_events(&mut self) {
        let mut closed = None;
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
                    self.status.stopped = stopped || self.refused.is_some();
                }
                Event::Closed(why) => closed = Some(why),
            }
        }
        if let Some(why) = closed {
            self.drop_link(&why);
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
    }

    /// Tries to reach the agent again at the next poll, without waiting out the pause (the app learnt the agent is
    /// back: Craft Cloud's status says so, say).
    pub fn reconnect_now(&mut self) {
        self.next_try = Instant::now();
        self.backoff = FIRST_BACKOFF;
    }

    /// Opens the document again off the UI thread, with a growing pause between tries.
    fn reconnect(&mut self) {
        if self.link.is_some() {
            return;
        }
        if let Some(rx) = &self.reopening {
            match rx.try_recv() {
                Ok(Ok((docs, doc))) => {
                    self.reopening = None;
                    self.incoming.push(doc.state().to_vec());
                    self.status.error = None;
                    self.attach(Link { _docs: docs, doc });
                    if let Some(waker) = &self.waker {
                        waker();
                    }
                }
                Ok(Err(why)) => {
                    self.reopening = None;
                    self.status.error = Some(why);
                    self.next_try = Instant::now() + self.backoff;
                    self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
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
        let spawned = std::thread::Builder::new().name("craft-cloud-kit-reopen".into()).spawn(move || {
            let result = (|| -> Reopened {
                let client = agent.client().map_err(|e| e.to_string())?;
                let docs = client.documents().map_err(|e| e.to_string())?;
                match docs.open(&path, D::APP).map_err(|e| e.to_string())? {
                    Opened::Live(doc) => Ok((docs, doc)),
                    Opened::NeedsImport(importer) => importer.import(&snapshot).map(|doc| (docs, doc)).map_err(|e| e.to_string()),
                    Opened::NotLive(reason) => Err(format!("the document can't be live now ({reason:?})")),
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
            if let Err(e) = self.doc.import(bytes) {
                self.status.error = Some(format!("a change from another machine doesn't read: {e}"));
            }
        }
        let raw = std::mem::take(&mut lock(&self.collector).raw);
        lock(&self.collector).on = false;
        let touched = self.classify(raw);
        let (was_read_only, was_outside) = (self.status.read_only.clone(), self.status.changed_outside);
        if touched.kit {
            self.check_schema();
            self.check_outside();
        }
        if !touched.any() {
            return Ok(was_read_only != self.status.read_only || was_outside != self.status.changed_outside);
        }
        self.rebuild(touched)?;
        Ok(true)
    }

    /// Sorts Loro's raw touches out by part.
    fn classify(&mut self, raw: Vec<Raw>) -> Touched {
        let mut touched = Touched::default();
        let mut reindex = Vec::new();
        for r in raw {
            if r.root == ROOT || r.root == SAVES {
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
                    t.nodes.extend(r.nodes.iter().filter_map(|n| self.ctx.trees.get(key).and_then(|i| i.elem_of.get(n)).copied()));
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
                        _ => t.cells.extend(r.keys.iter().filter_map(|k| parse_cell_key(k))),
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
                // Created nodes: their IDs are known only now.
                for (id, _) in index.tid_of.iter().filter(|(id, _)| !self.ctx.trees.get(key).is_some_and(|old| old.tid_of.contains_key(id))) {
                    t.nodes.insert(*id);
                }
            }
            self.ctx.trees.insert(key.to_owned(), index);
        }
        touched
    }

    /// Rebuilds the model from the touched parts, normalises it (writing back what that repaired), refreshes what is
    /// derived, and makes it the session's.
    fn rebuild(&mut self, touched: Touched) -> Result<(), KitError> {
        let raw = {
            // The old parts, for the texts to be patched rather than read whole.
            let old_parts = D::mappings(&self.model);
            let patch = RemotePatch { doc: &self.doc, touched: &touched, trees: &self.ctx.trees, old: (!touched.all).then_some(&old_parts) };
            D::apply_remote(&self.model, &patch)?
        };
        let mut fixed = raw.clone();
        D::normalise(Arc::make_mut(&mut fixed));
        if self.status.read_only.is_none() {
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
            self.outbox_bytes = self.outbox_bytes.saturating_add(bytes.len());
            self.outbox.push_back(Out { bytes, end, seq: None });
        }
        self.send_outbox();
    }

    fn send_outbox(&mut self) {
        let Some(link) = &self.link else { return };
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

    /// The app saved the document's file with `bytes`: notes the save in the document (so the file isn't taken for
    /// one changed outside Craft) and tells the agent, which records it with the version it uploads.
    pub fn saved(&mut self, bytes: &[u8]) -> Result<SaveRecord, KitError> {
        let hash = *blake3::hash(bytes).as_bytes();
        if self.status.read_only.is_none() {
            let mut w = Writer::new(&self.doc, KIT_ORIGIN);
            mark_saved(&mut w, &hash)?;
            let (updates, _) = w.finish()?;
            self.queue(updates);
        }
        self.status.changed_outside = false;
        self.read_from = None;
        let version = self.doc.oplog_vv().iter().map(|(p, c)| (*p, u32::try_from(*c).unwrap_or(0))).collect();
        let record = SaveRecord { hash, frontiers: self.doc.oplog_frontiers().encode(), version };
        if let Some(link) = &self.link {
            let _ = link.doc.saved(record.hash, &record.frontiers, &record.version);
        }
        Ok(record)
    }

    /// The live document's state as a Loro snapshot (for tests and diagnostics).
    pub fn snapshot(&self) -> Result<Vec<u8>, KitError> {
        Ok(self.doc.export(loro::ExportMode::Snapshot)?)
    }

    /// This session's Loro peer.
    pub fn peer(&self) -> u64 {
        self.doc.peer_id()
    }
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
        for (_, index) in diff.path {
            match index {
                Index::Node(t) => raw.nodes.push(*t),
                Index::Key(k) => raw.keys.push(k.to_string()),
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
