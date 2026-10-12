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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{self, Write};

use craft_cloud_chunker::{Chunker, FASTCDC_V1};
use craft_cloud_proto::limits::MAX_TREE_DEPTH;
use craft_cloud_proto::{
    Conflict, ConflictStamp, DeviceId, Epoch, FileHash, FileState, Manifest, ManifestId, Name, NewVersion, NodeId, PageCursor, conflict_name,
};

use crate::fs::{FileMeta, Fs, FsKind};
use crate::path::RelPath;
use crate::state::{BaseEntry, BaseKind, RemoteNode, State};
use crate::transport::{Transport, TransportError};

mod apply;
mod push;

/// Names that never sync (matched against the raw name): exact names, `prefix*` and `*suffix`.
pub const DEFAULT_IGNORES: &[&str] = &[".craft-cloud", ".DS_Store", "Thumbs.db", "desktop.ini", "~$*", "*.tmp", "*.part", ".~lock.*"];
/// Ignored files the OS drops into folders by itself. They go along with a folder the server deleted; other
/// ignored files (an open document's lock file, say) keep the folder here.
const OS_METADATA: &[&str] = &[".DS_Store", "Thumbs.db", "desktop.ini"];
/// Why an entry that is neither a regular file nor a folder (or whose name the filesystem layer can't use) is
/// skipped.
pub const OTHER_REASON: &str = "a link, a special file, or a name this computer can't sync";
/// Most chunk IDs asked about in one missing-chunk query.
const MISSING_QUERY_IDS: usize = 10_000;

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("server: {0}")]
    Transport(#[from] TransportError),
    #[error("{path}: {source}")]
    Io { path: String, source: io::Error },
    #[error("{0}")]
    Corrupt(String),
}

fn io_err(path: &RelPath, source: io::Error) -> SyncError {
    SyncError::Io { path: path.to_string(), source }
}

/// A filesystem error while handling one node (a file an app holds open, a folder that can't be listed)
/// skips that node this pass, with the reason in the report; its base stays as it was, so the next pass
/// tries again. Any other error ends the pass. Returns whether the node was skipped.
fn per_node(result: Result<(), SyncError>, report: &mut Report) -> Result<bool, SyncError> {
    match result {
        Ok(()) => Ok(false),
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
        }
    }

    /// Whether deleting `n` files out of `synced` in one pass (either direction) needs approval.
    fn too_many_deletes(&self, n: usize, synced: usize) -> bool {
        n > self.mass_delete_files || (n >= self.mass_delete_floor && n.saturating_mul(100) > synced.saturating_mul(self.mass_delete_percent))
    }

    fn ignored(&self, name: &str) -> bool {
        self.ignore.iter().any(|pattern| {
            if let Some(prefix) = pattern.strip_suffix('*') {
                name.starts_with(prefix)
            } else if let Some(suffix) = pattern.strip_prefix('*') {
                name.ends_with(suffix)
            } else {
                name == pattern
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
    /// Counts the engine's own changes to the folder, so per-pass caches of what is where can tell they are stale.
    fs_changes: u64,
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
        Ok(Self { fs, transport, state, chunker, config, stamp: Box::new(stamp), approval: Approval::None, fs_changes: 0 })
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
    /// them.
    pub fn approve_deletes_of(&mut self, nodes: impl IntoIterator<Item = NodeId>) {
        self.approval = Approval::Only(nodes.into_iter().collect());
    }

    /// One full pass. On error the state stays consistent (the base only records what happened), so the
    /// caller can save it and simply run another pass later.
    pub async fn sync_once(&mut self) -> Result<Report, SyncError> {
        let mut report = Report::default();
        let mode = match self.pull().await? {
            Mode::Normal if self.state.reconcile_pending => Mode::Reconcile,
            mode => mode,
        };
        self.apply_remote(mode, &mut report).await?;
        self.state.reconcile_pending = false;
        self.push(&mut report).await?;
        self.approval = Approval::None;
        Ok(report)
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
    /// Also returns the folders whose contents are unknown this pass: ones that can't be listed (or vanished
    /// while the scan ran) and ones nested deeper than the tree allows. Nothing tracked inside them counts as
    /// missing. Only a sync root that can't be listed fails the scan.
    fn scan(&self, report: &mut Report) -> Result<(BTreeMap<RelPath, FsKind>, Vec<RelPath>), SyncError> {
        let tracked = self.state.base_index();
        let mut out = BTreeMap::new();
        let mut unknown = Vec::new();
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
            let is_tracked = |raw: &str| Name::new(raw).is_ok_and(|n| tracked.contains_key(&dir.join(&n)));
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
                if let Some(first) = seen.insert(name.fold_key(), name.clone()) {
                    report.skipped.push((shown, format!("the name clashes with {first} when case is ignored")));
                    continue;
                }
                let path = dir.join(&name);
                match entry.kind {
                    FsKind::Other => {
                        report.skipped.push((shown, OTHER_REASON.into()));
                        // A tracked file or folder that turned into a link or a special file (a folder moved to
                        // another drive with a link left behind), or whose name this computer now spells
                        // differently, isn't gone: its node and everything tracked below it are unknown this pass,
                        // so nothing is deleted on the server.
                        let key = name.fold_key();
                        if let Some(was) =
                            tracked.keys().find(|p| p.parent().as_ref() == Some(&dir) && p.file_name().is_some_and(|n| n.fold_key() == key))
                        {
                            unknown.push(was.clone());
                        }
                    }
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
        Ok((out, unknown))
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
            let os_file = matches!(entry.kind, FsKind::File(_)) && self.config.ignored(&entry.name) && OS_METADATA.contains(&entry.name.as_str());
            match Name::new(&entry.name) {
                Ok(name) if os_file => junk.push(dir.join(&name)),
                _ => return Ok(false),
            }
        }
        for file in &junk {
            self.remove_file_here(file)?;
        }
        self.no_link(dir)?;
        self.fs.remove_dir(dir).map_err(|e| io_err(dir, e))?;
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

    /// A free conflict-copy path next to `path`.
    fn conflict_path(&self, path: &RelPath, is_dir: bool) -> Result<RelPath, SyncError> {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(SyncError::Corrupt("the root has no conflict copy".into()));
        };
        let stamp = (self.stamp)();
        for counter in 1..=1000 {
            let candidate = conflict_name(name, is_dir, &self.config.device_name, stamp, counter).map_err(|e| SyncError::Corrupt(e.to_string()))?;
            let target = parent.join(&candidate);
            if self.stat(&target)?.is_none() {
                return Ok(target);
            }
        }
        Err(SyncError::Corrupt(format!("no free conflict name for {path}")))
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
        self.fs.rename(from, to).map_err(|e| io_err(from, e))?;
        self.fs_changes += 1;
        Ok(())
    }

    /// Removes a file, never a link or through one.
    fn remove_file_here(&mut self, path: &RelPath) -> Result<(), SyncError> {
        self.no_link(path)?;
        self.fs.remove_file(path).map_err(|e| io_err(path, e))?;
        self.fs_changes += 1;
        Ok(())
    }

    /// Makes a folder, never through a link.
    fn mkdir_here(&mut self, path: &RelPath) -> Result<(), SyncError> {
        self.no_link(path)?;
        self.fs.mkdir(path).map_err(|e| io_err(path, e))?;
        self.fs_changes += 1;
        Ok(())
    }

    /// Moves whatever is at `path` to a conflict copy name; returns the new path.
    fn move_aside(&mut self, path: &RelPath, report: &mut Report) -> Result<RelPath, SyncError> {
        let is_dir = matches!(self.stat(path)?, Some(FsKind::Dir { .. }));
        let target = self.conflict_path(path, is_dir)?;
        self.rename_here(path, &target)?;
        // A tracked file the server deleted, moved aside: it is an untracked copy now (uploaded as new).
        if let Some(other) = self.state.base_index().get(path).copied()
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

    /// Downloads a file version and writes it to `path` atomically, chunk by chunk, after checking every chunk
    /// and the whole-file hash. A damaged download is abandoned and `path` stays as it was.
    ///
    /// `expected` is what the caller found at `path` before it decided the download may replace it: the file as
    /// stat'ed *before* it was verified, or `None` for a path that must be empty. If `path` differs from that when
    /// the download is ready to land (an app saved the file while it was hashed or while the download ran), the
    /// download is abandoned and `None` returned: the base keeps the old version, so the next pass sees a local
    /// change and takes the conflict path.
    async fn download(
        &mut self,
        file: &FileState,
        path: &RelPath,
        expected: Option<FsKind>,
        report: &mut Report,
    ) -> Result<Option<(FileMeta, FileHash)>, SyncError> {
        let manifest = self.fetch_manifest(&file.manifest).await?;
        self.no_link(path)?;
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
        // Not an atomic compare-and-replace (that needs the `Fs` trait's help), but it closes the window the
        // verification and the network round trips opened.
        if self.stat(path)? != expected {
            report.skipped.push((path.to_string(), "changed here while the server's version downloaded; tried again next pass".into()));
            return Ok(None);
        }
        self.no_link(path)?;
        let meta = self.fs.finish_write(path, writer, file.exec, file.mtime_ms).map_err(|e| io_err(path, e))?;
        self.fs_changes += 1;
        report.downloaded.push(path.clone());
        Ok(Some((meta, *manifest.file_hash())))
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
        local_id.is_some_and(|lid| self.state.base.values().any(|e| matches!(e.kind, BaseKind::File { local_id: Some(l), .. } if l == lid)))
    }

    /// Whether another tracked file (not `except`) has local ID `local_id`.
    fn owns_local_file(&self, local_id: Option<u64>, except: NodeId) -> bool {
        local_id.is_some_and(|lid| {
            self.state.base.iter().any(|(id, e)| *id != except && matches!(e.kind, BaseKind::File { local_id: Some(l), .. } if l == lid))
        })
    }

    /// Whether another tracked folder (not `except`) has local ID `local_id`.
    fn owns_local_dir(&self, local_id: Option<u64>, except: NodeId) -> bool {
        local_id.is_some_and(|lid| {
            self.state.base.iter().any(|(id, e)| *id != except && matches!(e.kind, BaseKind::Dir { local_id: Some(l) } if l == lid))
        })
    }

    fn dir_id(kind: Option<FsKind>) -> Option<u64> {
        match kind {
            Some(FsKind::Dir { id }) => id,
            _ => None,
        }
    }
}
