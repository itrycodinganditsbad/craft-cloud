//! One sync pass: pull the server's changes, apply them to the folder, then push the folder's changes.
//!
//! The engine keeps two trees in [`State`]: the remote mirror (the server's tree at `seq`) and the base
//! (what this machine and the server last agreed on). A pass:
//!
//! 1. **Pull**: read the change feed into the mirror, or the whole tree on a first sync, after the server's
//!    epoch changed (it was restored), or when the feed can't continue.
//! 2. **Apply remote**: for every node the server changed, in depth order, change the folder to match,
//!    unless the local copy changed too. A file changed on both sides keeps both: the local one is renamed to
//!    a conflict copy, the server's lands at the original name. A local edit to a file the server deleted
//!    wins (it is uploaded and the server brings it back).
//! 3. **Push**: rescan the folder and turn every difference from the base into batch operations: new
//!    folders and files, edits, moves (matched by file ID) and deletes (held back above the mass-delete
//!    threshold). Content is uploaded in content-defined chunks first; only missing chunks travel.
//!
//! Nothing local is ever deleted unless the base says the server deleted exactly what is on disk.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{self, Write};
use std::sync::{Mutex, PoisonError};

use craft_cloud_chunker::{Chunker, FASTCDC_V1};
use craft_cloud_proto::limits::{MAX_CHUNK_BYTES, MAX_FILE_BYTES, MAX_TREE_DEPTH};
use craft_cloud_proto::{
    Conflict, ConflictStamp, DeviceId, Epoch, FileHash, FileState, FoldKey, Manifest, ManifestId, Name, NewVersion, NodeId, PageCursor, conflict_name,
};

use crate::fs::{FileMeta, Fs, FsKind, LandError};
use crate::path::RelPath;
use crate::state::{BaseEntry, BaseKind, RemoteNode, State};
use crate::transport::{Transport, TransportError};

mod apply;
mod push;

/// Names that never sync (matched against the raw name, ignoring ASCII case): exact names, `prefix*` and `*suffix`.
pub const DEFAULT_IGNORES: &[&str] = &[".craft-cloud", ".DS_Store", "Thumbs.db", "desktop.ini", "~$*", "*.tmp", "*.part", ".~lock.*"];
/// Ignored files the OS drops into folders by itself. They go along with a folder the server deleted; other
/// ignored files (an open document's lock file, say) keep the folder here.
const OS_METADATA: &[&str] = &[".DS_Store", "Thumbs.db", "desktop.ini"];
/// Why an entry that is neither a regular file nor a folder (or whose name the filesystem layer can't use) is
/// skipped.
pub const OTHER_REASON: &str = "a link, a special file, or a name this computer can't sync";
/// Why an entry whose name isn't in Unicode's composed form (NFC, which Craft Cloud syncs) is skipped where the
/// filesystem keeps the two forms apart (Linux, Windows): a folder copied from a Mac often has such names.
pub const NOT_NFC_REASON: &str =
    "the name isn't in the composed Unicode form (NFC) Craft Cloud syncs, as a copy from a Mac can leave it; renaming it here fixes that";
/// Why a changed file that may still be being written ([`Fs::still_changing`]) is skipped. Every skip for it starts
/// with these words, so a caller can tell that a pass should look again once the file had time to settle.
pub const STILL_CHANGING: &str = "still changing";
/// Why a file both sides saved from its live document waits (see [`SaveRecords::includes`]).
pub const SAVE_ORDER_WAITS: &str = "two Craft saves wait to learn which is newer";
/// Why a file a Craft app is saving waits (see [`SaveRecords::pending`]).
pub const SAVE_RECORD_WAITS: &str = "a Craft save waits for its record";
/// What the report says about a download kept because the file it replaces is in use or may not be replaced.
const KEPT_WAITS: &str = "the server's version is downloaded and kept, and lands once it can";
/// Most chunk IDs asked about in one missing-chunk query.
const MISSING_QUERY_IDS: usize = 10_000;
/// Conflict-copy names tried for one name in one minute (`… 2)`, `… 3)`, …).
const MAX_CONFLICT_COUNTER: u32 = 1000;

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("server: {0}")]
    Transport(#[from] TransportError),
    #[error("{path}: {source}")]
    Io { path: String, source: io::Error },
    #[error("{0}")]
    Corrupt(String),
    /// The state couldn't be saved at a checkpoint (see [`Engine::on_checkpoint`]); the pass stops there.
    #[error("can't save the sync state: {0}")]
    Checkpoint(io::Error),
}

/// Saves the engine's state during a pass (see [`Engine::on_checkpoint`]).
pub type Checkpoint = Box<dyn FnMut(&State) -> io::Result<()> + Send + Sync>;

fn io_err(path: &RelPath, source: io::Error) -> SyncError {
    SyncError::Io { path: path.to_string(), source }
}

/// The key a download of `file`, a version of node `id`, is kept under ([`Fs::keep`]): node, version and manifest,
/// so a kept file is only ever landed as exactly that version.
fn kept_key(id: NodeId, file: &FileState) -> String {
    format!("{id}.{}.{}", file.version, file.manifest)
}

/// A filesystem error while handling one node (a file an app holds open, a folder that can't be listed)
/// skips that node this pass, with the reason in the report; its base stays as it was, so the next pass
/// tries again. Any other error ends the pass. Returns whether the node was skipped.
fn per_node(result: Result<(), SyncError>, report: &mut Report) -> Result<bool, SyncError> {
    match result {
        Ok(()) => Ok(false),
        // A file marked read-only here is the user saying "don't change this": the server's delete waits (the base
        // keeps the node) until the mark is cleared.
        Err(SyncError::Io { path, source }) if crate::fs::is_read_only(&source) => {
            report.skipped.push((path, format!("{source}; waits until it is no longer read-only")));
            Ok(true)
        }
        Err(SyncError::Io { path, source }) => {
            report.skipped.push((path, format!("{source}; tried again next pass")));
            Ok(true)
        }
        Err(e) => Err(e),
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Shown in conflict copy names.
    pub device_name: String,
    /// This device, recorded on versions it commits.
    pub device: DeviceId,
    /// More file deletions than this in one pass wait for approval…
    pub mass_delete_files: usize,
    /// …and so do more than this percentage of the synced files, once at least `mass_delete_floor` are deleted.
    pub mass_delete_percent: usize,
    pub mass_delete_floor: usize,
    pub ignore: Vec<String>,
    /// A file whose modification time is this close to when it was recorded (or later) can't be trusted by
    /// size and time alone, and is hashed. Covers coarse timestamps (FAT: 2 s).
    pub racy_window_ms: i64,
    /// Larger files are skipped (listed with the reason) before they are read: the protocol's limit,
    /// [`MAX_FILE_BYTES`](craft_cloud_proto::limits::MAX_FILE_BYTES). Tests lower it.
    pub max_file_bytes: u64,
    /// An account with less room than this left under the server's stored-bytes cap counts as full: the refusal
    /// ends the pass and the agent pauses, instead of chunking every new file again only to have it refused. One
    /// chunk, [`MAX_CHUNK_BYTES`](craft_cloud_proto::limits::MAX_CHUNK_BYTES): with less room than that, no file
    /// of consequence fits. Tests lower it.
    pub quota_floor: u64,
    /// How long two Craft saves of one file may wait for the agent to learn which is newer ([`SaveRecords::includes`])
    /// before they are a conflict copy, as any two edits are (10 minutes, counted on a monotonic clock: on Linux and
    /// macOS that is time awake, so a laptop that sleeps through it doesn't end it early; on Windows std's clock may
    /// count sleep too). Tests lower it.
    pub save_order_wait_ms: i64,
}

impl Config {
    pub fn new(device_name: impl Into<String>, device: DeviceId) -> Self {
        Self {
            device_name: device_name.into(),
            device,
            mass_delete_files: 500,
            mass_delete_percent: 20,
            mass_delete_floor: 10,
            ignore: DEFAULT_IGNORES.iter().map(|s| (*s).to_owned()).collect(),
            racy_window_ms: 2_000,
            max_file_bytes: MAX_FILE_BYTES,
            quota_floor: u64::from(MAX_CHUNK_BYTES),
            save_order_wait_ms: 10 * 60 * 1000,
        }
    }

    /// Whether deleting `n` files out of `synced` in one pass (either direction) needs approval.
    fn too_many_deletes(&self, n: usize, synced: usize) -> bool {
        n > self.mass_delete_files || (n >= self.mass_delete_floor && n.saturating_mul(100) > synced.saturating_mul(self.mass_delete_percent))
    }

    /// Whether `name` matches an ignore pattern, ignoring ASCII case: Windows and macOS treat `RENDER.TMP` and
    /// `render.tmp` as one name, and apps write either. Only ASCII letters fold (the default patterns are ASCII, and
    /// a fold that is predictable byte for byte never makes a pattern match more than it says).
    fn ignored(&self, name: &str) -> bool {
        self.ignore.iter().any(|pattern| {
            if let Some(prefix) = pattern.strip_suffix('*') {
                name.get(..prefix.len()).is_some_and(|head| head.eq_ignore_ascii_case(prefix))
            } else if let Some(suffix) = pattern.strip_prefix('*') {
                name.len().checked_sub(suffix.len()).and_then(|start| name.get(start..)).is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
            } else {
                name.eq_ignore_ascii_case(pattern)
            }
        })
    }
}

/// What a pass did. `changed()` is false when a pass found nothing to do.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Report {
    pub downloaded: Vec<RelPath>,
    pub uploaded: Vec<RelPath>,
    pub removed_locally: Vec<RelPath>,
    pub removed_remotely: Vec<RelPath>,
    /// The nodes behind `removed_remotely`, in the same order. A path there is where the node was when its delete
    /// went (after this pass's pull let it follow a server move), so a check of what was deleted goes by node.
    pub removed_nodes: Vec<NodeId>,
    pub moved_locally: Vec<(RelPath, RelPath)>,
    pub moved_remotely: Vec<(RelPath, RelPath)>,
    pub created_dirs: Vec<RelPath>,
    pub conflict_copies: Vec<RelPath>,
    /// Local entries that can't sync, with the reason.
    pub skipped: Vec<(String, String)>,
    /// Deletions on the server held back by the mass-delete guard until [`Engine::approve_deletes`].
    pub pending_deletes: Vec<RelPath>,
    /// Files the server deleted whose removal here the mass-delete guard holds back until
    /// [`Engine::approve_deletes`]. They stay on disk and tracked.
    pub pending_local_deletes: Vec<RelPath>,
    /// The nodes behind `pending_deletes` and `pending_local_deletes`, for [`Engine::approve_deletes_of`].
    pub pending_nodes: Vec<NodeId>,
    /// Server deletes held behind a link after this pass (see [`State::held_deletes`]): the node's path and the
    /// paths of all its links, in order. Each is held until all its links are gone (the user removes, renames or
    /// replaces them), the node is found here, or the user approves its delete by naming it.
    pub held_deletes: Vec<(RelPath, Vec<RelPath>)>,
    /// The nodes behind `held_deletes`, in the same order: naming one in [`Engine::approve_deletes_of`] lets its
    /// delete go.
    pub held_nodes: Vec<NodeId>,
    /// Operations the server refused this pass; they are retried next pass.
    pub refused: Vec<Conflict>,
}

impl Report {
    pub fn changed(&self) -> bool {
        !(self.downloaded.is_empty()
            && self.uploaded.is_empty()
            && self.removed_locally.is_empty()
            && self.removed_remotely.is_empty()
            && self.moved_locally.is_empty()
            && self.moved_remotely.is_empty()
            && self.created_dirs.is_empty()
            && self.conflict_copies.is_empty()
            && self.refused.is_empty())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// The base is trusted. On a first sync it is empty, so a local file at a path the server has is a
    /// conflict copy unless it holds the same content.
    Normal,
    /// The server was restored (new epoch): the base can't be trusted; never replace local files.
    Reconcile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Local {
    Unchanged,
    Modified,
    Missing,
}

/// What a scan couldn't see into this pass. Nothing tracked there counts as missing, so the push never deletes it
/// on the server.
struct Unseen {
    /// Folders whose contents are unknown: ones that can't be listed (or vanished while the scan ran) and ones
    /// nested deeper than the tree allows. A new folder among them isn't created on the server until it can be.
    unknown: Vec<RelPath>,
    /// Links and special files, by folder and folded name. A tracked file or folder that turned into one (a folder
    /// moved to another drive with a link left behind), or whose name this computer now spells differently, isn't
    /// gone, and neither is anything tracked below it.
    links: HashSet<(RelPath, FoldKey)>,
    /// The same links and special files by path, as the scan saw them (see [`State::known_links`]).
    link_paths: BTreeSet<RelPath>,
}

impl Unseen {
    /// Whether `path` (where a tracked node is expected now, after the folder moves the scan found) is in a folder
    /// whose contents are unknown, or is a link or below one.
    fn hides(&self, path: &RelPath) -> bool {
        if self.unknown.iter().any(|u| path.starts_with(u)) {
            return true;
        }
        let mut prefix = RelPath::root();
        for name in path.names() {
            if self.links.contains(&(prefix.clone(), name.fold_key())) {
                return true;
            }
            prefix = prefix.join(name);
        }
        false
    }
}

/// A folder's names by folded name, each with whether it is a link or special file: what the twin checks need of a
/// listing (see [`Engine::twins_in`]).
type Twins = HashMap<FoldKey, BTreeMap<Name, bool>>;

/// What one of the engine's own changes did at a path (see [`Engine::saw_change`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    /// A file or folder is there now, under the name asked for.
    Made,
    /// What was there is gone.
    Gone,
    /// The change failed: what is there is unknown.
    Unknown,
}

impl Seen {
    fn made_if<T, E>(result: &Result<T, E>) -> Self {
        if result.is_ok() { Seen::Made } else { Seen::Unknown }
    }

    fn gone_if<T, E>(result: &Result<T, E>) -> Self {
        if result.is_ok() { Seen::Gone } else { Seen::Unknown }
    }
}

/// What to record when an operation commits.
#[derive(Debug, Clone)]
enum Effect {
    Mkdir {
        id: NodeId,
        parent: NodeId,
        name: Name,
        local_id: Option<u64>,
        path: RelPath,
    },
    File {
        id: NodeId,
        parent: NodeId,
        name: Name,
        version: NewVersion,
        size: u64,
        meta: FileMeta,
        hash: FileHash,
    },
    /// A new version of an existing file: its place doesn't change (moves are their own effect).
    Update {
        id: NodeId,
        version: NewVersion,
        size: u64,
        meta: FileMeta,
        hash: FileHash,
    },
    Move {
        id: NodeId,
        parent: NodeId,
        name: Name,
        from: RelPath,
        to: RelPath,
    },
    Delete {
        id: NodeId,
        path: RelPath,
    },
}

pub struct Engine<F: Fs, T: Transport> {
    fs: F,
    transport: T,
    state: State,
    chunker: Chunker,
    config: Config,
    stamp: Box<dyn Fn() -> ConflictStamp + Send + Sync>,
    approval: Approval,
    /// Held deletes the user approved by naming their nodes (see [`Engine::approve_deletes_of`]): the next pass
    /// releases these holds and lets the deletes go.
    release: BTreeSet<NodeId>,
    /// Counts the engine's own changes to the folder, so per-pass caches of what is where can tell they are stale.
    fs_changes: u64,
    /// Folders' names for the twin checks, listed once a pass and kept up to date with the engine's own changes
    /// (see [`Engine::twins_in`]). Behind a lock because the checks only read the engine.
    listings: Mutex<BTreeMap<RelPath, Twins>>,
    checkpoint: Option<Checkpoint>,
    /// The new folder IDs the last look at the start of a pass found but didn't take (see [`Engine::learn_dir_ids`]).
    declined_dir_ids: BTreeSet<(NodeId, u64)>,
    /// The account's storage on the server as the last refusal this pass gave it (`used`, `limit`): a later file
    /// bigger than the room left is skipped before it is read (see [`Engine::upload`]). Forgotten at the start of
    /// every pass, since the admin may have raised the cap or the room may have grown.
    quota: Option<(u64, u64)>,
    /// What Craft apps saved from live documents ([`Engine::set_save_records`]).
    saves: Option<std::sync::Arc<dyn SaveRecords>>,
    /// Versions a late save record was sent for this run (see [`Engine::attach_late_saves`]).
    attached: BTreeSet<(NodeId, craft_cloud_proto::VersionId)>,
    /// Files both sides saved from their live document where it isn't known yet which save is newer: neither side is
    /// taken this pass (the push leaves them too). Cleared at the start of every pass.
    deferred: BTreeSet<NodeId>,
    /// Since when each such file has waited, across passes (a monotonic clock: on Linux and macOS it stops while the
    /// machine sleeps; on Windows it may not).
    unknown_since: HashMap<NodeId, std::time::Instant>,
}

/// What Craft apps saved from live documents, as the agent learnt it from them (`craft_cloud_proto::SaveRecord`).
///
/// - A new version of a file whose content is one an app saved carries the save record.
/// - When a file changed here and on the server, and both this content and the server's version carry a record of
///   the same live document, there is no conflict copy: the live document holds both sides' edits. The server's
///   version wins if its record includes this one's (it was saved later, from a document with everything this one
///   had); otherwise this one goes up over it (it is the newer upload, and the next save writes everything anyway).
/// - Whether one record includes another is the agent's to say, from its copy of the document ([`Self::includes`]).
///   When it can't tell yet (it lacks the operations), the pair waits: neither side is taken this pass. Still unknown
///   after [`Config::save_order_wait_ms`], it is a conflict copy as always. A winner is never guessed.
/// - Without a record on either side (a file edited outside Craft), it is a conflict as always.
pub trait SaveRecords: Send + Sync {
    /// The record of the save that wrote content `hash` to `node`'s file, if an app saved exactly that.
    fn record(&self, node: NodeId, hash: &FileHash) -> Option<craft_cloud_proto::SaveRecord>;
    /// Whether the live document as `a` names it (its frontiers) holds every operation `b`'s does; `None` while that
    /// can't be told (the operations aren't here yet).
    fn includes(&self, a: &craft_cloud_proto::SaveRecord, b: &craft_cloud_proto::SaveRecord) -> Option<bool>;
    /// Whether an app announced a save of exactly content `hash` to `node`'s file whose record hasn't come yet (an app
    /// tells the agent before it writes, and sends the record after the changes it covers). Such content is never
    /// taken for an edit outside Craft: it isn't uploaded without its record, nor made a conflict copy, but waits.
    fn pending(&self, node: NodeId, hash: &FileHash) -> bool {
        let _ = (node, hash);
        false
    }
}

/// Which deletions the mass-delete guard lets through this pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum Approval {
    #[default]
    None,
    All,
    /// Exactly these nodes (the ones a user saw listed); any others still wait.
    Only(BTreeSet<NodeId>),
}

impl Approval {
    fn covers(&self, id: NodeId) -> bool {
        match self {
            Approval::None => false,
            Approval::All => true,
            Approval::Only(ids) => ids.contains(&id),
        }
    }
}

impl<F: Fs, T: Transport> Engine<F, T> {
    /// `stamp` gives the local time for conflict copy names.
    pub fn new(
        fs: F,
        transport: T,
        state: State,
        config: Config,
        stamp: impl Fn() -> ConflictStamp + Send + Sync + 'static,
    ) -> Result<Self, SyncError> {
        let chunker = Chunker::new(FASTCDC_V1).map_err(|e| SyncError::Corrupt(e.to_string()))?;
        Ok(Self {
            fs,
            transport,
            state,
            chunker,
            config,
            stamp: Box::new(stamp),
            approval: Approval::None,
            release: BTreeSet::new(),
            fs_changes: 0,
            listings: Mutex::new(BTreeMap::new()),
            checkpoint: None,
            declined_dir_ids: BTreeSet::new(),
            quota: None,
            saves: None,
            attached: BTreeSet::new(),
            deferred: BTreeSet::new(),
            unknown_since: HashMap::new(),
        })
    }

    /// Where the engine learns what Craft apps saved from live documents: a version uploaded with content an app
    /// saved carries its record, and two versions of a live-linked file that both do never make a conflict copy
    /// (see [`SaveRecords`]).
    pub fn set_save_records(&mut self, saves: std::sync::Arc<dyn SaveRecords>) {
        self.saves = Some(saves);
    }

    /// The save record of `node`'s content `hash`, if well formed.
    pub(crate) fn save_record(&self, node: NodeId, hash: &FileHash) -> Option<craft_cloud_proto::SaveRecord> {
        self.saves.as_ref()?.record(node, hash).filter(craft_cloud_proto::SaveRecord::is_valid)
    }

    /// Whether a Craft save of `node`'s content `hash` is on its way: announced, with no record yet.
    pub(crate) fn save_pending(&self, node: NodeId, hash: &FileHash) -> bool {
        self.save_record(node, hash).is_none() && self.saves.as_ref().is_some_and(|s| s.pending(node, hash))
    }

    /// Whether save `a` includes save `b`, as the agent can tell; `None` while it can't.
    pub(crate) fn save_includes(&self, a: &craft_cloud_proto::SaveRecord, b: &craft_cloud_proto::SaveRecord) -> Option<bool> {
        if a == b {
            return Some(true);
        }
        self.saves.as_ref()?.includes(a, b)
    }

    /// Saves the state at safe points during a pass, so a crash loses less than the whole pass: once the server's
    /// changes are applied here, before a commit that deletes files (recording them in [`State::sent_deletes`]),
    /// and after every commit is recorded. The state is consistent at each of them, as it is when a pass fails. An
    /// error from `save` stops the pass ([`SyncError::Checkpoint`]); before a commit that means it isn't sent.
    /// The caller still saves the state after the pass.
    pub fn on_checkpoint(&mut self, save: impl FnMut(&State) -> io::Result<()> + Send + Sync + 'static) {
        self.checkpoint = Some(Box::new(save));
    }

    fn checkpoint(&mut self) -> Result<(), SyncError> {
        match &mut self.checkpoint {
            Some(save) => save(&self.state).map_err(SyncError::Checkpoint),
            None => Ok(()),
        }
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn fs(&self) -> &F {
        &self.fs
    }

    pub fn fs_mut(&mut self) -> &mut F {
        &mut self.fs
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// Lets the next pass carry out deletions the mass-delete guard held back.
    pub fn approve_deletes(&mut self) {
        self.approval = Approval::All;
    }

    /// Lets the next pass carry out exactly these held-back deletions (both directions), as listed in an earlier
    /// report's [`Report::pending_nodes`]. Deletions of other nodes still wait for approval if the guard holds
    /// them. Adds to an approval given since the last pass (several calls approve all the nodes they name; after
    /// [`Engine::approve_deletes`] everything stays approved).
    ///
    /// A node named here whose delete is held behind a link right now (see [`State::held_deletes`] and
    /// [`Report::held_nodes`]) is released: the next pass sends its delete. Only naming it does that;
    /// [`Engine::approve_deletes`] never releases a hold, and neither does naming a node that is held only later.
    pub fn approve_deletes_of(&mut self, nodes: impl IntoIterator<Item = NodeId>) {
        let nodes: Vec<NodeId> = nodes.into_iter().collect();
        self.release.extend(nodes.iter().filter(|id| self.state.held_deletes.contains_key(*id)));
        match &mut self.approval {
            Approval::All => {}
            Approval::Only(ids) => ids.extend(nodes),
            Approval::None => self.approval = Approval::Only(nodes.into_iter().collect()),
        }
    }

    /// One full pass. On error the state stays consistent (the base only records what happened), so the
    /// caller can save it and simply run another pass later.
    pub async fn sync_once(&mut self) -> Result<Report, SyncError> {
        let mut report = Report::default();
        // Anything may have changed in the folder since the last pass.
        self.listings.get_mut().unwrap_or_else(PoisonError::into_inner).clear();
        self.quota = None;
        self.deferred.clear();
        // A landing a crash cut short is put right before anything looks at the folder. Until it is, the pass fails
        // (a filesystem error, so it is retried): a scan could take the moved file for a new one or the target for
        // deleted. The error names the landing's target.
        self.fs.recover().map_err(|e| SyncError::Io {
            path: "an interrupted download".into(),
            source: io::Error::new(e.kind(), format!("couldn't be put right ({e}); tried again next pass")),
        })?;
        // What recovery set aside for the user (a record it can't read, a file it couldn't put back).
        report.skipped.extend(self.fs.notices());
        // Before anything can fail (an offline server, a download), so a pass that fails still leaves it behind.
        self.learn_dir_ids()?;
        let mode = match self.pull().await? {
            Mode::Normal if self.state.reconcile_pending => Mode::Reconcile,
            mode => mode,
        };
        self.apply_remote(mode, &mut report).await?;
        self.state.reconcile_pending = false;
        self.discard_stale_downloads();
        self.checkpoint()?;
        self.push(&mut report).await?;
        self.approval = Approval::None;
        self.release.clear();
        self.attach_late_saves().await;
        Ok(report)
    }

    /// Save records that came after their version went up (an app saved while the agent was away, and told it once
    /// back): each is added to its version on the server (appended; the same again changes nothing), so the version
    /// counts as a Craft save everywhere. Only a file whose server version is the one this machine agreed on, with the
    /// content the record names, and none yet. Sent once per version per run; an older server (`NotFound`) is let be,
    /// anything else tried again next pass.
    async fn attach_late_saves(&mut self) {
        if self.saves.is_none() {
            return;
        }
        let mut late = Vec::new();
        for (id, entry) in self.state.base.iter() {
            let BaseKind::File { version, hash, .. } = &entry.kind else { continue };
            let Some(remote) = self.state.remote.get(id).and_then(|r| r.file.as_ref()) else { continue };
            if remote.version != *version || remote.save.is_some() || self.attached.contains(&(*id, *version)) {
                continue;
            }
            if let Some(record) = self.save_record(*id, hash) {
                late.push((*id, *version, record));
            }
        }
        for (node, version, record) in late {
            self.attached.insert((node, version));
            match self.transport.attach_save(node, version, &record).await {
                Ok(()) | Err(TransportError::NotFound) => {}
                Err(_) => {
                    self.attached.remove(&(node, version));
                }
            }
        }
    }

    // ---------------------------------------------------------------- pull

    async fn pull(&mut self) -> Result<Mode, SyncError> {
        if self.state.space.is_none() {
            let space = self.transport.space().await?;
            // The space is recorded only once the tree is in: a first sync cut short starts over as a first sync.
            self.relist().await?;
            self.state.space = Some(space.space);
            self.state.root = Some(space.root);
            self.state.base.clear();
            self.state.base.insert(space.root, BaseEntry { parent: None, name: None, kind: BaseKind::Dir { local_id: None } });
            return Ok(Mode::Normal);
        }
        match self.read_changes().await {
            Ok((epoch, seq, nodes)) if Some(epoch) == self.state.epoch => {
                for node in nodes {
                    if node.deleted {
                        self.state.remote.remove(&node.id);
                    } else {
                        self.state.remote.insert(node.id, RemoteNode::from_state(&node));
                    }
                }
                self.state.seq = seq;
                Ok(Mode::Normal)
            }
            Ok(_) => self.restart_after_restore().await,
            Err(SyncError::Transport(TransportError::ResyncRequired)) => {
                let old_epoch = self.state.epoch;
                self.relist().await?;
                if self.state.epoch == old_epoch { Ok(Mode::Normal) } else { self.forget_base() }
            }
            Err(e) => Err(e),
        }
    }

    async fn restart_after_restore(&mut self) -> Result<Mode, SyncError> {
        self.relist().await?;
        self.forget_base()
    }

    fn forget_base(&mut self) -> Result<Mode, SyncError> {
        let root = self.state.root.ok_or_else(|| SyncError::Corrupt("no root".into()))?;
        self.state.base.retain(|id, _| *id == root);
        self.state.reconcile_pending = true;
        Ok(Mode::Reconcile)
    }

    async fn read_changes(&self) -> Result<(Epoch, u64, Vec<craft_cloud_proto::NodeState>), SyncError> {
        let mut after = None;
        let mut nodes = Vec::new();
        let mut first: Option<(Epoch, u64)> = None;
        loop {
            let page = self.transport.changes(self.state.seq, after).await?;
            first.get_or_insert((page.epoch, page.seq));
            nodes.extend(page.nodes);
            match page.next {
                Some(cursor) => after = Some(cursor),
                None => break,
            }
        }
        let (epoch, seq) = first.ok_or_else(|| SyncError::Corrupt("empty change feed".into()))?;
        Ok((epoch, seq, nodes))
    }

    async fn relist(&mut self) -> Result<(), SyncError> {
        let mut after: Option<PageCursor> = None;
        let mut remote = BTreeMap::new();
        let mut first: Option<(Epoch, u64)> = None;
        loop {
            let page = self.transport.tree(after).await?;
            first.get_or_insert((page.epoch, page.seq));
            for node in &page.nodes {
                if !node.deleted {
                    remote.insert(node.id, RemoteNode::from_state(node));
                }
            }
            match page.next {
                Some(cursor) => after = Some(cursor),
                None => break,
            }
        }
        let (epoch, seq) = first.ok_or_else(|| SyncError::Corrupt("empty listing".into()))?;
        self.state.remote = remote;
        self.state.epoch = Some(epoch);
        self.state.seq = seq;
        Ok(())
    }

    // ---------------------------------------------------------------- local helpers

    /// The folder as a map from path to entry, without ignored, invalid, clashing or special entries.
    /// When two names in a folder clash by case, the tracked one wins and the other is skipped.
    ///
    /// Also returns what the scan couldn't see into (see [`Unseen`]). Only a sync root that can't be listed fails
    /// the scan.
    fn scan(&self, report: &mut Report) -> Result<(BTreeMap<RelPath, FsKind>, Unseen), SyncError> {
        let mut out = BTreeMap::new();
        let mut unknown = Vec::new();
        let mut links = HashSet::new();
        let mut link_paths = BTreeSet::new();
        let mut queue = vec![RelPath::root()];
        while let Some(dir) = queue.pop() {
            let mut entries = match self.fs.list(&dir) {
                Ok(entries) => entries,
                Err(e) if dir.is_root() => return Err(io_err(&dir, e)),
                Err(e) => {
                    report.skipped.push((dir.to_string(), format!("can't be listed: {e}; tried again next pass")));
                    unknown.push(dir);
                    continue;
                }
            };
            let is_tracked = |raw: &str| Name::new(raw).is_ok_and(|n| self.state.base_has_path(&dir.join(&n)));
            entries.sort_by(|a, b| is_tracked(&b.name).cmp(&is_tracked(&a.name)).then_with(|| a.name.cmp(&b.name)));
            let mut seen = HashMap::new();
            for entry in entries {
                if self.config.ignored(&entry.name) {
                    continue;
                }
                let shown = if dir.is_root() { entry.name.clone() } else { format!("{dir}/{}", entry.name) };
                let Ok(name) = Name::new(&entry.name) else {
                    report.skipped.push((shown, "the name can't exist on every system Craft Cloud supports".into()));
                    continue;
                };
                // A link hides what is tracked at its name even when a case twin wins the name (in a folder the user
                // renamed, neither name is tracked yet): the tracked file behind it is never taken for deleted.
                if entry.kind == FsKind::Other {
                    links.insert((dir.clone(), name.fold_key()));
                    link_paths.insert(dir.join(&name));
                }
                if let Some(first) = seen.insert(name.fold_key(), name.clone()) {
                    report.skipped.push((shown, format!("the name clashes with {first} when case is ignored")));
                    continue;
                }
                let path = dir.join(&name);
                match entry.kind {
                    // A name that isn't NFC is one the filesystem layer can't use as it stands (on Linux and Windows
                    // that spelling is another file): said as such, not as a link.
                    FsKind::Other if name.as_str() != entry.name => report.skipped.push((shown, NOT_NFC_REASON.into())),
                    FsKind::Other => report.skipped.push((shown, OTHER_REASON.into())),
                    FsKind::Dir { .. } if path.depth() > MAX_TREE_DEPTH => {
                        report.skipped.push((shown, "folders nest too deep".into()));
                        unknown.push(path);
                    }
                    FsKind::Dir { .. } => {
                        queue.push(path.clone());
                        out.insert(path, entry.kind);
                    }
                    FsKind::File(_) => {
                        out.insert(path, entry.kind);
                    }
                }
            }
        }
        // The target of a landing that hasn't finished (its file is moved aside until recovery puts it back) is
        // unknown too: the tracked file there is never taken for deleted.
        for path in self.fs.unsettled() {
            if !out.contains_key(&path) {
                report.skipped.push((path.to_string(), "an interrupted download is put right first; tried again next pass".into()));
            }
            unknown.push(path);
        }
        Ok((out, Unseen { unknown, links, link_paths }))
    }

    /// Every folder of a scan by its local ID.
    fn dirs_by_id(local: &BTreeMap<RelPath, FsKind>) -> HashMap<u64, RelPath> {
        local
            .iter()
            .filter_map(|(p, k)| match k {
                FsKind::Dir { id: Some(i) } => Some((*i, p.clone())),
                _ => None,
            })
            .collect()
    }

    /// A tracked folder found at its place is that folder (path first). When its recorded local ID is gone from the
    /// folder (it was put back from another drive, or made again), or is on a folder at an untracked place with no
    /// link in it (the user renamed it and made a new one under the old name: the renamed one goes up as a new
    /// folder), it takes the ID it has now, so a later rename of it is sent as a move and a link in it is still found
    /// through it. A renamed folder holding a link keeps the ID: the link that went with it is found through it. Not
    /// while any folder can't be listed (the old ID may be in there), and never an ID another tracked folder has.
    ///
    /// `index` is the base by path, `local` and `unseen` a scan, and `expected` where a base path is now after the
    /// folder moves the scan found.
    fn refresh_dir_ids(
        &mut self,
        index: &BTreeMap<RelPath, NodeId>,
        local: &BTreeMap<RelPath, FsKind>,
        unseen: &Unseen,
        expected: &dyn Fn(&RelPath) -> RelPath,
    ) {
        if !unseen.unknown.is_empty() {
            return;
        }
        let root = self.state.root;
        let dir_at = Self::dirs_by_id(local);
        let refreshed: Vec<(NodeId, Option<u64>)> = index
            .iter()
            .filter_map(|(path, id)| match (&self.state.base.get(id)?.kind, local.get(&expected(path))?) {
                (BaseKind::Dir { local_id }, FsKind::Dir { id: here })
                    if Some(*id) != root
                        && here.is_some()
                        && *local_id != *here
                        && local_id.is_none_or(|old| {
                            dir_at.get(&old).is_none_or(|at| !index.contains_key(at) && !unseen.links.iter().any(|(d, _)| d.starts_with(at)))
                        })
                        && !self.owns_local_dir(*here, *id) =>
                {
                    Some((*id, *here))
                }
                _ => None,
            })
            .collect();
        for (id, here) in refreshed {
            self.state.base.update(&id, |entry| {
                if let BaseKind::Dir { local_id } = &mut entry.kind {
                    *local_id = here;
                }
            });
        }
    }

    /// Folder IDs are learnt at the start of every pass, before anything can fail: a tracked folder at its place
    /// under a new local ID (a link that became a real folder again, a folder put back from another drive) takes it
    /// as the push would ([`Engine::refresh_dir_ids`]). Otherwise a pass that fails first (offline, a download that
    /// breaks off) leaves the old ID behind, and if the user then moves the folder, the next pass can't find it by
    /// ID: the move looks like a delete plus a new folder, and a node behind a link inside it would be deleted on the
    /// server (seed 112337). The whole folder is scanned only when a quick look at every tracked folder's place finds
    /// one with an ID it didn't record, and not when it finds only IDs the last such scan declined (a folder that
    /// couldn't be listed, a renamed folder holding a link, an ID another tracked folder has): those would cost a
    /// second full scan on every pass until resolved, and the push's own refresh takes them once they can be taken.
    fn learn_dir_ids(&mut self) -> Result<(), SyncError> {
        let Some(root) = self.state.root else { return Ok(()) };
        let stale: BTreeSet<(NodeId, u64)> = self
            .state
            .base
            .iter()
            .filter_map(|(id, entry)| {
                let BaseKind::Dir { local_id } = entry.kind else { return None };
                if *id == root {
                    return None;
                }
                match self.fs.stat(&self.state.base_path(*id)?) {
                    Ok(Some(FsKind::Dir { id: Some(here) })) if Some(here) != local_id => Some((*id, here)),
                    _ => None,
                }
            })
            .collect();
        if stale.is_subset(&self.declined_dir_ids) {
            self.declined_dir_ids = stale;
            return Ok(());
        }
        // The push's scan reports what it skips; this one is only a look.
        let (local, unseen) = self.scan(&mut Report::default())?;
        let index = self.state.base_index();
        self.refresh_dir_ids(&index, &local, &unseen, &|p| p.clone());
        self.declined_dir_ids = stale
            .into_iter()
            .filter(
                |(id, here)| matches!(self.state.base.get(id), Some(BaseEntry { kind: BaseKind::Dir { local_id }, .. }) if *local_id != Some(*here)),
            )
            .collect();
        Ok(())
    }

    /// Fails as a filesystem error on the node (it is skipped and tried again next pass) if the file at `path` may
    /// still be being written ([`Fs::still_changing`]): it is never moved aside as a conflict copy, or replaced, while
    /// an app may be writing it. Asked only about a file that differs from the base.
    fn wait_if_changing(&self, path: &RelPath) -> Result<(), SyncError> {
        match self.stat(path)? {
            Some(FsKind::File(meta)) if self.fs.still_changing(path, &meta) => Err(io_err(path, io::Error::other(STILL_CHANGING))),
            _ => Ok(()),
        }
    }

    fn stat(&self, path: &RelPath) -> Result<Option<FsKind>, SyncError> {
        self.fs.stat(path).map_err(|e| io_err(path, e))
    }

    /// Whether a local file matches what the base recorded. Size and time are the quick check; when they
    /// match but can't be trusted (the file was replaced, so its ID changed, or its time is within the racy
    /// window of when it was recorded), the content is hashed.
    fn file_matches(&self, entry: &BaseEntry, at: &RelPath, m: &FileMeta) -> Result<bool, SyncError> {
        let BaseKind::File { size, mtime_ms, local_id, hash, seen_ms, .. } = &entry.kind else { return Ok(false) };
        if m.size != *size || m.mtime_ms != *mtime_ms {
            return Ok(false);
        }
        let suspect = m.id != *local_id || m.mtime_ms.saturating_add(self.config.racy_window_ms) >= *seen_ms;
        if !suspect {
            return Ok(true);
        }
        Ok(self.content_hash(at) == Some(*hash))
    }

    /// The content hash of a local file, streamed. `None` when it can't be read (locked by another program,
    /// gone): an unreadable file never counts as unchanged, so it is never overwritten or deleted.
    fn content_hash(&self, path: &RelPath) -> Option<FileHash> {
        let mut reader = self.fs.open(path).ok()?;
        let mut hasher = blake3::Hasher::new();
        io::copy(&mut reader, &mut hasher).ok()?;
        Some(FileHash::from_bytes(*hasher.finalize().as_bytes()))
    }

    fn local_state(&self, entry: &BaseEntry, at: &RelPath) -> Result<Local, SyncError> {
        let current = self.stat(at)?;
        Ok(match (&entry.kind, current) {
            // Another tracked file sits here (this one was moved aside or away): this one is missing here.
            (BaseKind::File { local_id, .. }, Some(FsKind::File(m))) if m.id != *local_id && self.tracks_local_file(m.id) => Local::Missing,
            (BaseKind::File { .. }, Some(FsKind::File(m))) if self.file_matches(entry, at, &m)? => Local::Unchanged,
            (BaseKind::File { .. }, Some(FsKind::File(_))) => Local::Modified,
            (BaseKind::Dir { .. }, Some(FsKind::Dir { .. })) => Local::Unchanged,
            _ => Local::Missing,
        })
    }

    /// Whether the local file at `at` still holds exactly the content both sides agreed on. Size and time
    /// are only a quick filter (they can match by accident); before anything overwrites or deletes a local
    /// file, its content is hashed and compared.
    fn really_unchanged(&self, entry: &BaseEntry, at: &RelPath) -> Result<bool, SyncError> {
        if self.local_state(entry, at)? != Local::Unchanged {
            return Ok(false);
        }
        match &entry.kind {
            BaseKind::File { hash, .. } => Ok(self.content_hash(at) == Some(*hash)),
            BaseKind::Dir { .. } => Ok(true),
        }
    }

    /// Removes the folder `dir` if it is empty or holds only OS metadata files (removed with it). Returns
    /// whether it went.
    fn remove_dir_if_bare(&mut self, dir: &RelPath) -> Result<bool, SyncError> {
        let mut junk = Vec::new();
        for entry in self.fs.list(dir).map_err(|e| io_err(dir, e))? {
            let os_file = matches!(entry.kind, FsKind::File(_))
                && self.config.ignored(&entry.name)
                && OS_METADATA.iter().any(|junk| junk.eq_ignore_ascii_case(&entry.name));
            match Name::new(&entry.name) {
                Ok(name) if os_file => junk.push(dir.join(&name)),
                _ => return Ok(false),
            }
        }
        for file in &junk {
            self.remove_file_here(file)?;
        }
        self.no_link(dir)?;
        let removed = self.fs.remove_dir(dir);
        self.saw_change(dir, Seen::gone_if(&removed));
        removed.map_err(|e| io_err(dir, e))?;
        self.fs_changes += 1;
        Ok(true)
    }

    /// Whether the folder `dir` holds something, but nothing that syncs: only ignored names, and folders like
    /// that. The push doesn't create such a folder on the server.
    fn holds_only_ignored(&self, dir: &RelPath) -> Result<bool, SyncError> {
        if dir.depth() > MAX_TREE_DEPTH {
            return Ok(false);
        }
        // A folder that can't be listed may hold something that syncs.
        let Ok(entries) = self.fs.list(dir) else { return Ok(false) };
        if entries.is_empty() {
            return Ok(false);
        }
        for entry in entries {
            if self.config.ignored(&entry.name) {
                continue;
            }
            let (FsKind::Dir { .. }, Ok(name)) = (entry.kind, Name::new(&entry.name)) else { return Ok(false) };
            if !self.holds_only_ignored(&dir.join(&name))? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// A free conflict-copy path next to `path`: nothing is there, and no tracked node has it as its place (one the
    /// user moved away, say), so settling that node later never acts on the copy.
    ///
    /// Finding none (every one of the 1,000 names for this minute is taken, or the name can't be made) is a problem
    /// of that one node, not damaged data: a filesystem error on it, so it is skipped and tried again next pass (a
    /// minute later the names are new) while everything else keeps syncing.
    fn conflict_path(&self, path: &RelPath, is_dir: bool) -> Result<RelPath, SyncError> {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(SyncError::Corrupt("the root has no conflict copy".into()));
        };
        let stamp = (self.stamp)();
        for counter in 1..=MAX_CONFLICT_COUNTER {
            let candidate =
                conflict_name(name, is_dir, &self.config.device_name, stamp, counter).map_err(|e| io_err(path, io::Error::other(e.to_string())))?;
            let target = parent.join(&candidate);
            if !self.state.base_has_path(&target) && self.stat(&target)?.is_none() {
                return Ok(target);
            }
        }
        Err(io_err(path, io::Error::other("no free conflict-copy name this minute")))
    }

    /// The engine never renames, removes or writes onto or through a link: every change it makes to the folder
    /// first checks that neither the path nor any folder above it is a link or special file here. If one is, the
    /// change fails as a filesystem error on that node, so the node is skipped this pass (with the link reason)
    /// and waits, and the link stays exactly as it is.
    pub(super) fn no_link(&self, path: &RelPath) -> Result<(), SyncError> {
        let mut prefix = RelPath::root();
        for name in path.names() {
            prefix = prefix.join(name);
            match self.stat(&prefix)? {
                Some(FsKind::Other) => return Err(io_err(&prefix, io::Error::other(OTHER_REASON))),
                Some(FsKind::Dir { .. }) => {}
                // A file or nothing: there is nothing further down to go through.
                _ => return Ok(()),
            }
        }
        Ok(())
    }

    /// Renames, never onto or through a link (see [`Engine::no_link`]).
    fn rename_here(&mut self, from: &RelPath, to: &RelPath) -> Result<(), SyncError> {
        self.no_link(from)?;
        self.no_link(to)?;
        let renamed = self.fs.rename(from, to);
        self.saw_change(from, Seen::gone_if(&renamed));
        self.saw_change(to, Seen::made_if(&renamed));
        renamed.map_err(|e| io_err(from, e))?;
        self.fs_changes += 1;
        Ok(())
    }

    /// Removes a file, never a link or through one.
    fn remove_file_here(&mut self, path: &RelPath) -> Result<(), SyncError> {
        self.no_link(path)?;
        let removed = self.fs.remove_file(path);
        self.saw_change(path, Seen::gone_if(&removed));
        removed.map_err(|e| io_err(path, e))?;
        self.fs_changes += 1;
        Ok(())
    }

    /// Makes a folder, never through a link.
    fn mkdir_here(&mut self, path: &RelPath) -> Result<(), SyncError> {
        self.no_link(path)?;
        let made = self.fs.mkdir(path);
        self.saw_change(path, Seen::made_if(&made));
        made.map_err(|e| io_err(path, e))?;
        self.fs_changes += 1;
        Ok(())
    }

    /// The names in the folder `dir`, by folded name, for the twin checks ([`Engine::case_twin`] and
    /// [`Engine::linked_twin`]): listed once a pass, then kept up to date with the engine's own changes
    /// ([`Engine::saw_change`]), so a first sync of a folder with thousands of files lists it once, not once per file.
    /// What an app does to the folder during the pass isn't in it; that is safe because the twin checks only plan a
    /// change: the filesystem refuses one that meets something unexpected (a write lands only where the caller said
    /// what is there, a rename never replaces), and the node waits for the next pass.
    fn twins_in<R>(&self, dir: &RelPath, read: impl FnOnce(&Twins) -> R) -> io::Result<R> {
        let mut listings = self.listings.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(twins) = listings.get(dir) {
            return Ok(read(twins));
        }
        let mut twins = Twins::new();
        for entry in self.fs.list(dir)? {
            if let Ok(name) = Name::new(&entry.name) {
                // Two spellings of one name (NFC and not, on Linux): a link if either is one.
                *twins.entry(name.fold_key()).or_default().entry(name).or_default() |= entry.kind == FsKind::Other;
            }
        }
        Ok(read(listings.entry(dir.clone()).or_insert(twins)))
    }

    /// The engine changed what is at `path`: the listing kept for its folder ([`Engine::twins_in`]) follows, and
    /// listings of `path` and anything below it are dropped (listed afresh when needed). The engine only ever makes
    /// files and folders, and under exactly the name it asked for, unless a case twin was there: then (and when the
    /// change failed, so what is there is unknown) the folder's listing is dropped too.
    fn saw_change(&mut self, path: &RelPath, seen: Seen) {
        let listings = self.listings.get_mut().unwrap_or_else(PoisonError::into_inner);
        // `path` and everything below it sort together, right where `path` would be.
        let below: Vec<RelPath> = listings.range(path.clone()..).map(|(dir, _)| dir).take_while(|dir| dir.starts_with(path)).cloned().collect();
        for dir in below {
            listings.remove(&dir);
        }
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else { return };
        let Some(twins) = listings.get_mut(&parent) else { return };
        let key = name.fold_key();
        let names = twins.entry(key.clone()).or_default();
        let unclear = match seen {
            Seen::Made => {
                // Another spelling of the name that was there (and may be a link) stays as it was.
                names.entry(name.clone()).or_default();
                names.len() > 1
            }
            Seen::Gone => {
                names.remove(name);
                !names.is_empty()
            }
            Seen::Unknown => true,
        };
        let empty = names.is_empty();
        if unclear {
            listings.remove(&parent);
        } else if empty {
            twins.remove(&key);
        }
    }

    /// Moves whatever is at `path` to a conflict copy name; returns the new path.
    fn move_aside(&mut self, path: &RelPath, report: &mut Report) -> Result<RelPath, SyncError> {
        let is_dir = matches!(self.stat(path)?, Some(FsKind::Dir { .. }));
        let target = self.conflict_path(path, is_dir)?;
        self.rename_here(path, &target)?;
        // A tracked file the server deleted, moved aside: it is an untracked copy now (uploaded as new).
        if let Some(other) = self.state.base_at(path)
            && !self.state.remote.contains_key(&other)
            && matches!(self.state.base.get(&other).map(|e| &e.kind), Some(BaseKind::File { .. }))
        {
            self.state.base.remove(&other);
        }
        report.conflict_copies.push(target.clone());
        Ok(target)
    }

    async fn fetch_manifest(&self, id: &ManifestId) -> Result<Manifest, SyncError> {
        let bytes = self.transport.get_manifest(id).await?;
        let manifest = Manifest::decode(&bytes).map_err(|e| SyncError::Corrupt(format!("manifest {id}: {e}")))?;
        if manifest.id() != *id {
            return Err(SyncError::Corrupt(format!("manifest {id} doesn't match its ID")));
        }
        Ok(manifest)
    }

    /// Downloads a version of the file node `id` and writes it to `path` atomically, chunk by chunk, after checking
    /// every chunk and the whole-file hash. A damaged download is abandoned and `path` stays as it was.
    ///
    /// `expected` is what the caller found at `path` before it decided the download may replace it: the file as
    /// stat'ed *before* it was verified, or `None` for a path that must be empty. The download lands only on exactly
    /// that, in one step ([`Fs::finish_write`]): if an app saved the file while it was hashed or while the download
    /// ran, `None` is returned and the base keeps the old version, so the next pass sees a local change and takes the
    /// conflict path. If a program holds the file open so it can't be replaced, the node is skipped this pass (a
    /// filesystem error on it). Either way the download is kept ([`Fs::keep`]), and a later landing of the same
    /// version takes it instead of downloading again, as long as that version is current.
    async fn download(
        &mut self,
        id: NodeId,
        file: &FileState,
        path: &RelPath,
        expected: Option<FsKind>,
        report: &mut Report,
    ) -> Result<Option<(FileMeta, FileHash)>, SyncError> {
        let key = kept_key(id, file);
        // A download of this version is kept already, and the file it would replace still can't be replaced: skipped
        // as cheaply as possible (no manifest, no move or hash of the kept file), until it can.
        if matches!(expected, Some(FsKind::File(_)))
            && let Some(why) = self.fs.replace_blocked(path)
            && self.fs.kept().is_ok_and(|keys| keys.contains(&key))
        {
            return Err(io_err(path, io::Error::new(io::ErrorKind::PermissionDenied, format!("{why}; {KEPT_WAITS}"))));
        }
        let manifest = self.fetch_manifest(&file.manifest).await?;
        self.no_link(path)?;
        let changed = |report: &mut Report| {
            report.skipped.push((path.to_string(), "changed here while the server's version downloaded; tried again next pass".into()));
        };
        let expected = match expected {
            None => None,
            Some(FsKind::File(meta)) => Some(meta),
            // Something that isn't a file can't be replaced by one.
            Some(_) => {
                changed(report);
                return Ok(None);
            }
        };
        // A kept download of this very version lands without downloading again, once its content is hashed again
        // against the version's file hash; one that changed on disk, or can't be taken back, is downloaded again.
        let writer = match self.fs.take_kept(&key, manifest.file_hash()) {
            Ok(Some(writer)) => writer,
            Ok(None) | Err(_) => {
                let mut writer = self.fs.begin_write(path).map_err(|e| io_err(path, e))?;
                let mut hasher = blake3::Hasher::new();
                for chunk in manifest.chunks() {
                    let bytes = self.transport.get_chunk(&chunk.id).await?;
                    if *blake3::hash(&bytes).as_bytes() != *chunk.id.as_bytes() || bytes.len() as u64 != u64::from(chunk.len) {
                        return Err(SyncError::Corrupt(format!("chunk {} of {path} is damaged", chunk.id)));
                    }
                    hasher.update(&bytes);
                    writer.write_all(&bytes).map_err(|e| io_err(path, e))?;
                }
                if *hasher.finalize().as_bytes() != *manifest.file_hash().as_bytes() {
                    return Err(SyncError::Corrupt(format!("{path} doesn't match its file hash")));
                }
                writer
            }
        };
        self.no_link(path)?;
        let landed = self.fs.finish_write(path, writer, file.exec, file.mtime_ms, expected);
        // A landing in steps changes the folder even when it doesn't land.
        self.fs_changes += 1;
        match &landed {
            Ok(_) => self.saw_change(path, Seen::Made),
            // The target is as it was.
            Err(LandError::Changed(_) | LandError::InUse(..)) => {}
            Err(LandError::Io(_)) => self.saw_change(path, Seen::Unknown),
        }
        match landed {
            Ok(meta) => {
                report.downloaded.push(path.clone());
                Ok(Some((meta, *manifest.file_hash())))
            }
            Err(LandError::Changed(writer)) => {
                // Kept for the next pass, which lands it next to the user's file (a conflict copy) or on it.
                let _ = self.fs.keep(writer, &key);
                changed(report);
                Ok(None)
            }
            Err(LandError::InUse(writer, why)) => {
                // The server's version is downloaded and kept here; it lands once the file can be replaced.
                let kept = self.fs.keep(writer, &key).is_ok();
                let what = if kept { KEPT_WAITS } else { "not replaced" };
                Err(io_err(path, io::Error::new(why.kind(), format!("{why}; {what}"))))
            }
            Err(LandError::Io(e)) => Err(io_err(path, e)),
        }
    }

    /// Drops kept downloads ([`Fs::keep`]) of versions that are no longer current, or already here.
    fn discard_stale_downloads(&mut self) {
        let Ok(keys) = self.fs.kept() else { return };
        if keys.is_empty() {
            return;
        }
        let wanted: HashSet<String> = self
            .state
            .remote
            .iter()
            .filter_map(|(id, r)| {
                let file = r.file.as_ref()?;
                let here = matches!(self.state.base.get(id).map(|e| &e.kind), Some(BaseKind::File { version, .. }) if *version == file.version);
                (!here).then(|| kept_key(*id, file))
            })
            .collect();
        for key in keys {
            if !wanted.contains(&key) {
                let _ = self.fs.discard_kept(&key);
            }
        }
    }

    /// The local file at `path`, if it holds exactly the content of the server's version `file`. The file is
    /// stat'ed before it is hashed, so a save during the hash shows up as a change next pass.
    async fn same_content(&self, file: &FileState, path: &RelPath) -> Result<Option<(FileMeta, FileHash)>, SyncError> {
        let manifest = self.fetch_manifest(&file.manifest).await?;
        let Some(FsKind::File(meta)) = self.stat(path)? else { return Ok(None) };
        let hash = *manifest.file_hash();
        Ok((meta.size == manifest.size() && self.content_hash(path) == Some(hash)).then_some((meta, hash)))
    }

    fn base_file(&self, file: &FileState, meta: FileMeta, hash: FileHash) -> BaseKind {
        BaseKind::File {
            version: file.version,
            manifest: file.manifest,
            size: meta.size,
            mtime_ms: meta.mtime_ms,
            exec: file.exec,
            local_id: meta.id,
            hash,
            seen_ms: self.fs.now_ms(),
        }
    }

    /// Whether a local file ID belongs to any tracked file.
    fn tracks_local_file(&self, local_id: Option<u64>) -> bool {
        local_id.is_some_and(|lid| self.state.base.files_with_local_id(lid).next().is_some())
    }

    /// Whether a tracked file the server still has, other than `except` (the node adopting), owns local ID `local_id`:
    /// such a file is that node's (the user moved it), never adopted by another node. One the server deleted is let
    /// go this pass anyway.
    fn live_node_owns(&self, local_id: Option<u64>, except: NodeId) -> bool {
        local_id.is_some_and(|lid| self.state.base.files_with_local_id(lid).any(|id| id != except && self.state.remote.contains_key(&id)))
    }

    /// Whether another tracked file (not `except`) has local ID `local_id`.
    fn owns_local_file(&self, local_id: Option<u64>, except: NodeId) -> bool {
        local_id.is_some_and(|lid| self.state.base.files_with_local_id(lid).any(|id| id != except))
    }

    /// Whether another tracked folder (not `except`) has local ID `local_id`.
    fn owns_local_dir(&self, local_id: Option<u64>, except: NodeId) -> bool {
        local_id.is_some_and(|lid| self.state.base.dirs_with_local_id(lid).any(|id| id != except))
    }

    fn dir_id(kind: Option<FsKind>) -> Option<u64> {
        match kind {
            Some(FsKind::Dir { id }) => id,
            _ => None,
        }
    }
}
