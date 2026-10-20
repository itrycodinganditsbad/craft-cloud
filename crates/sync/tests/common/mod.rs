//! Test harness: an in-process server (the real metadata and blob stores, no HTTP) and machines that
//! sync against it, and what the random-history simulations share ([`sim`]). Each test binary uses only some of
//! it; the agent's network simulation uses it too.
#![allow(dead_code)]

pub mod sim;

use std::collections::BTreeSet;
use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use craft_cloud_meta::{Meta, MetaError, Role, UserRecord};
use craft_cloud_proto::api::{CommitResponse, SpaceResponse};
use craft_cloud_proto::{
    ChangeSet, ChunkId, Conflict, ConflictKind, ConflictStamp, DeviceId, FileHash, Manifest, ManifestId, Op, PageCursor, SpaceId,
};
use craft_cloud_store::memory::MemoryBackend;
use craft_cloud_store::{BlobKey, Store};
use craft_cloud_sync::{Config, Engine, FileMeta, Fs, FsEntry, FsKind, LandError, MemFs, RelPath, Report, State, Transport, TransportError};

/// The server's metadata in an in-memory SQLite database (tests only: the rules are the same as on disk, and
/// nothing waits for the disk), blobs in memory. A clone is the same server.
#[derive(Clone)]
pub struct Server {
    pub meta: Meta,
    pub store: Store,
    /// The manifest of every version a commit stored, recorded as commits succeed.
    committed: Arc<Mutex<BTreeSet<ManifestId>>>,
}

impl Server {
    pub async fn new() -> Self {
        let meta = Meta::open("sqlite::memory:").await.unwrap();
        Self { meta, store: Store::new(MemoryBackend::new()), committed: Arc::default() }
    }

    pub async fn user(&self, name: &str) -> UserRecord {
        self.meta.create_user(name, "$argon2id$test", Role::Member, 0).await.unwrap()
    }

    /// The whole-file hash of every version the server ever stored, for "nothing was lost" checks.
    ///
    /// These are the versions of the commits the transport saw succeed (recorded when `Meta::commit` returns
    /// Ok, before a reply is lost on purpose; refused commits store nothing, a batch applies all or none), not a
    /// read of the server's versions table: `craft-cloud-meta` has no history listing yet. So this trusts the
    /// metadata store to keep every version it accepted (its own tests cover that). The check is about the
    /// engine: content it saw must stay on a disk or in a version the server accepted.
    pub async fn history_hashes(&self) -> BTreeSet<FileHash> {
        let ids: Vec<ManifestId> = self.committed.lock().unwrap().iter().copied().collect();
        let mut out = BTreeSet::new();
        for id in ids {
            let manifest = Manifest::decode(&self.store.get(BlobKey::manifest(&id)).await.unwrap()).unwrap();
            out.insert(*manifest.file_hash());
        }
        out
    }

    /// Records the versions a commit the server applied stored, for [`Server::history_hashes`].
    pub fn record_commit(&self, ops: &[Op]) {
        record_commit(&self.committed, ops);
    }
}

fn record_commit(committed: &Mutex<BTreeSet<ManifestId>>, ops: &[Op]) {
    let mut committed = committed.lock().unwrap();
    for op in ops {
        if let Op::Create { version, .. } | Op::Update { version, .. } = op {
            committed.insert(version.manifest);
        }
    }
}

/// Makes calls fail with `Unavailable` on a schedule, to test interrupted passes.
#[derive(Default)]
pub struct Faults {
    /// Fail one call in this many (0 = never).
    pub every: AtomicU64,
    /// Make the next commit apply on the server but report failure, as if its reply was lost.
    pub lose_commit_reply: AtomicBool,
    /// Make every commit come back refused, naming an operation the batch doesn't have.
    pub bad_conflict_index: AtomicBool,
    /// The machine died (see `MemFs::die_after`): every call fails.
    pub dead: Arc<AtomicBool>,
    /// The machine dies right after the n-th commit from now applies on the server, before its answer arrives
    /// (0 = never): that commit's reply is lost, and every call after it fails.
    pub die_on_commit: AtomicU64,
    /// How many chunks this device downloaded (counted, not a fault).
    pub chunk_downloads: AtomicU64,
    /// How many chunks this device uploaded (counted, not a fault).
    pub chunk_uploads: AtomicU64,
    /// The next this many chunk uploads are answered as stored but never stored, as by a proxy that answered for
    /// the server and lost them.
    pub lose_chunks: AtomicU64,
    calls: AtomicU64,
}

impl Faults {
    fn check(&self) -> Result<(), TransportError> {
        if self.dead.load(Ordering::Relaxed) {
            return Err(TransportError::Unavailable("the machine died".into()));
        }
        let every = self.every.load(Ordering::Relaxed);
        let n = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
        if every > 0 && n.is_multiple_of(every) { Err(TransportError::Unavailable("injected failure".into())) } else { Ok(()) }
    }
}

/// The server's API rules, in-process. A clone is the same device talking to the same server.
#[derive(Clone)]
pub struct Local {
    /// The device's name, for a restarted engine's settings.
    name: String,
    meta: Meta,
    store: Store,
    space: SpaceId,
    device: DeviceId,
    username: String,
    page: u32,
    pub faults: Arc<Faults>,
    clock: Arc<Mutex<i64>>,
    committed: Arc<Mutex<BTreeSet<ManifestId>>>,
}

fn meta_err(e: MetaError) -> TransportError {
    match e {
        MetaError::Rejected(c) => TransportError::Rejected(c),
        MetaError::MissingChunks(m) => TransportError::MissingChunks(m),
        MetaError::ResyncRequired => TransportError::ResyncRequired,
        MetaError::QuotaExceeded { used, limit } => TransportError::QuotaExceeded { used, limit },
        MetaError::NotFound(_) => TransportError::NotFound,
        other => TransportError::Protocol(other.to_string()),
    }
}

impl Local {
    fn now(&self) -> i64 {
        let mut c = self.clock.lock().unwrap();
        *c += 1;
        *c
    }
}

#[async_trait]
impl Transport for Local {
    async fn space(&self) -> Result<SpaceResponse, TransportError> {
        self.faults.check()?;
        let info = self.meta.space_info(self.space).await.map_err(meta_err)?;
        Ok(SpaceResponse { space: self.space, root: info.root, epoch: info.epoch, seq: info.seq, username: self.username.clone() })
    }

    async fn tree(&self, after: Option<PageCursor>) -> Result<ChangeSet, TransportError> {
        self.faults.check()?;
        self.meta.tree(self.space, after, self.page).await.map_err(meta_err)
    }

    async fn changes(&self, since: u64, after: Option<PageCursor>) -> Result<ChangeSet, TransportError> {
        self.faults.check()?;
        self.meta.changes(self.space, since, after, self.page).await.map_err(meta_err)
    }

    async fn missing_chunks(&self, ids: &[ChunkId]) -> Result<Vec<ChunkId>, TransportError> {
        self.faults.check()?;
        self.meta.missing_chunks(self.space, ids).await.map_err(meta_err)
    }

    async fn put_chunk(&self, id: &ChunkId, data: Bytes) -> Result<(), TransportError> {
        self.faults.check()?;
        self.faults.chunk_uploads.fetch_add(1, Ordering::Relaxed);
        // One engine drives a transport at a time, so a load and a store are enough.
        if let Some(left) = self.faults.lose_chunks.load(Ordering::Relaxed).checked_sub(1) {
            self.faults.lose_chunks.store(left, Ordering::Relaxed);
            return Ok(());
        }
        self.store.put(BlobKey::chunk(id), &data).await.map_err(|e| TransportError::Protocol(e.to_string()))?;
        self.meta.add_chunk(self.space, id, data.len() as u64 + 1, self.now()).await.map_err(meta_err)
    }

    async fn get_chunk(&self, id: &ChunkId) -> Result<Bytes, TransportError> {
        self.faults.check()?;
        self.faults.chunk_downloads.fetch_add(1, Ordering::Relaxed);
        if !self.meta.missing_chunks(self.space, &[*id]).await.map_err(meta_err)?.is_empty() {
            return Err(TransportError::NotFound);
        }
        self.store.get(BlobKey::chunk(id)).await.map_err(|e| TransportError::Protocol(e.to_string()))
    }

    async fn put_manifest(&self, id: &ManifestId, data: Bytes) -> Result<(), TransportError> {
        self.faults.check()?;
        let manifest = Manifest::decode(&data).map_err(|e| TransportError::Protocol(e.to_string()))?;
        if manifest.id() != *id {
            return Err(TransportError::Protocol("manifest ID mismatch".into()));
        }
        self.store.put(BlobKey::manifest(id), &data).await.map_err(|e| TransportError::Protocol(e.to_string()))?;
        self.meta.add_manifest(self.space, &manifest, data.len() as u64 + 1, self.now()).await.map_err(meta_err)
    }

    async fn get_manifest(&self, id: &ManifestId) -> Result<Bytes, TransportError> {
        self.faults.check()?;
        if !self.meta.has_manifest(self.space, id).await.map_err(meta_err)? {
            return Err(TransportError::NotFound);
        }
        self.store.get(BlobKey::manifest(id)).await.map_err(|e| TransportError::Protocol(e.to_string()))
    }

    async fn commit(&self, ops: &[Op]) -> Result<CommitResponse, TransportError> {
        self.faults.check()?;
        if self.faults.bad_conflict_index.load(Ordering::Relaxed) {
            return Err(TransportError::Rejected(vec![Conflict { op: ops.len() + 5, kind: ConflictKind::NoSuchNode }]));
        }
        let out = self.meta.commit(self.space, self.device, ops, self.now()).await.map_err(meta_err);
        if let (Err(TransportError::Rejected(conflicts)), Ok(_)) = (&out, std::env::var("SIM_TRACE")) {
            eprintln!("TRACE {} refused {conflicts:?} of {ops:?}", self.name);
        }
        let out = out?;
        // Every version this commit stored stays in the server's history.
        record_commit(&self.committed, ops);
        if self.faults.lose_commit_reply.swap(false, Ordering::Relaxed) {
            return Err(TransportError::Unavailable("reply lost".into()));
        }
        match self.faults.die_on_commit.load(Ordering::Relaxed) {
            0 => {}
            1 => {
                self.faults.die_on_commit.store(0, Ordering::Relaxed);
                self.faults.dead.store(true, Ordering::Relaxed);
                return Err(TransportError::Unavailable("the machine died".into()));
            }
            n => self.faults.die_on_commit.store(n - 1, Ordering::Relaxed),
        }
        Ok(CommitResponse { seq: out.seq, epoch: out.epoch })
    }

    async fn attach_save(
        &self,
        node: craft_cloud_proto::NodeId,
        version: craft_cloud_proto::VersionId,
        save: &craft_cloud_proto::SaveRecord,
    ) -> Result<(), TransportError> {
        self.faults.check()?;
        self.meta.attach_save(self.space, node, version, save).await.map(|_| ()).map_err(meta_err)
    }
}

pub const STAMP: ConflictStamp = ConflictStamp { year: 2026, month: 10, day: 11, hour: 14, minute: 3, utc: false };

/// A simulated machine: a folder in memory and a way to the server (in process by default).
pub type Machine<T = Local> = Engine<MemFs, T>;

/// A machine signed in as `user`, with an empty folder and a fresh state.
pub async fn machine(server: &Server, user: &UserRecord, name: &str) -> Machine {
    machine_with(server, user, name, MemFs::new(), 50).await
}

pub async fn machine_with(server: &Server, user: &UserRecord, name: &str, fs: MemFs, page: u32) -> Machine {
    machine_config(server, user, name, fs, page, |_| {}).await
}

/// Like [`machine_with`], with a chance to change the engine's settings.
pub async fn machine_config(server: &Server, user: &UserRecord, name: &str, fs: MemFs, page: u32, tweak: impl FnOnce(&mut Config)) -> Machine {
    machine_on(server, user, name, fs, page, tweak).await
}

/// A machine on any filesystem.
pub async fn machine_on<F: Fs>(
    server: &Server,
    user: &UserRecord,
    name: &str,
    fs: F,
    page: u32,
    tweak: impl FnOnce(&mut Config),
) -> Engine<F, Local> {
    let token = *blake3::hash(format!("{name}-{}", DeviceId::generate()).as_bytes()).as_bytes();
    let device = server.meta.create_device(user.id, name, &token, 0).await.unwrap();
    let transport = Local {
        name: name.to_owned(),
        meta: server.meta.clone(),
        store: server.store.clone(),
        space: user.space,
        device,
        username: user.username.clone(),
        page,
        faults: Arc::new(Faults::default()),
        clock: Arc::new(Mutex::new(1_760_000_000_000)),
        committed: server.committed.clone(),
    };
    let mut config = Config::new(name, device);
    tweak(&mut config);
    Engine::new(fs, transport, State::default(), config, || STAMP).unwrap()
}

/// The same machine after a crash and a restart: its folder and device as they are, a fresh engine that starts
/// from `state` (what the agent saved last), with the settings `tweak` gives.
pub fn restart(m: &Machine, state: State, tweak: impl FnOnce(&mut Config)) -> Machine {
    let transport = m.transport().clone();
    transport.faults.dead.store(false, Ordering::Relaxed);
    let mut fs = m.fs().clone();
    fs.revive();
    let mut config = Config::new(transport.name.clone(), transport.device);
    tweak(&mut config);
    Engine::new(fs, transport, state, config, || STAMP).unwrap()
}

/// The same machine, its folder and state as they are, talking to a server whose metadata handle is `meta` (for
/// a cap set after the first files went up: a handle's cap is its own).
pub fn on_meta(m: &Machine, meta: Meta, tweak: impl FnOnce(&mut Config)) -> Machine {
    let mut transport = m.transport().clone();
    transport.meta = meta;
    let mut config = Config::new(transport.name.clone(), transport.device);
    tweak(&mut config);
    Engine::new(m.fs().clone(), transport, m.state().clone(), config, || STAMP).unwrap()
}

/// Makes the machine save its state at every checkpoint of a pass into the returned slot, as the agent saves
/// `state.json` (starting with the state it has now). A crash restarts from what the slot holds.
pub fn save_checkpoints(m: &mut Machine) -> Arc<Mutex<State>> {
    let slot = Arc::new(Mutex::new(m.state().clone()));
    let saving = slot.clone();
    m.on_checkpoint(move |state| {
        *saving.lock().unwrap() = state.clone();
        Ok(())
    });
    slot
}

/// Syncs the machines in turn until a whole round changes nothing. Panics after `max_rounds`.
pub async fn settle(machines: &mut [&mut Machine], max_rounds: usize) {
    for _ in 0..max_rounds {
        let mut changed = false;
        for m in machines.iter_mut() {
            let report: Report = m.sync_once().await.unwrap();
            changed |= report.changed();
        }
        if !changed {
            return;
        }
    }
    panic!("machines didn't settle in {max_rounds} rounds");
}

pub fn path(s: &str) -> RelPath {
    craft_cloud_sync::rel(s)
}

/// When the program in [`SaveDuring`] saves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveOn {
    /// When the engine starts writing a download to the path (after it checked the path, before its write lands).
    BeginWrite,
    /// While the engine reads the path for the n-th time (1-based): that read still sees the content from before
    /// the save, as a hash that was already under way when the save landed would.
    Open(usize),
}

/// A [`MemFs`] where a program saves a file at an awkward moment for the engine.
#[derive(Default)]
pub struct SaveDuring {
    inner: Mutex<MemFs>,
    save: Mutex<Option<(RelPath, Vec<u8>, SaveOn)>>,
}

impl SaveDuring {
    /// Arms one save of `data` to `path`.
    pub fn arm(&self, path: RelPath, data: &[u8], on: SaveOn) {
        *self.save.lock().unwrap() = Some((path, data.to_vec(), on));
    }

    /// Whether the armed save has happened.
    pub fn fired(&self) -> bool {
        self.save.lock().unwrap().is_none()
    }

    pub fn files(&self) -> std::collections::BTreeMap<RelPath, Vec<u8>> {
        self.inner.lock().unwrap().files()
    }

    pub fn write(&self, path: &RelPath, data: &[u8]) {
        self.inner.lock().unwrap().write(path, data, false, 0).unwrap();
    }

    /// Lands the armed save if `now` is its moment.
    fn maybe_save(&self, inner: &mut MemFs, path: &RelPath, now: SaveOn) -> io::Result<()> {
        let mut save = self.save.lock().unwrap();
        let Some((p, data, on)) = save.as_mut() else { return Ok(()) };
        if p != path {
            return Ok(());
        }
        match (*on, now) {
            (SaveOn::BeginWrite, SaveOn::BeginWrite) => {}
            (SaveOn::Open(n), SaveOn::Open(_)) if n > 1 => {
                *on = SaveOn::Open(n - 1);
                return Ok(());
            }
            (SaveOn::Open(_), SaveOn::Open(_)) => {}
            _ => return Ok(()),
        }
        inner.write(p, data, false, 0)?;
        *save = None;
        Ok(())
    }
}

impl Fs for SaveDuring {
    type Writer = <MemFs as Fs>::Writer;

    fn now_ms(&self) -> i64 {
        self.inner.lock().unwrap().now_ms()
    }
    fn list(&self, dir: &RelPath) -> io::Result<Vec<FsEntry>> {
        self.inner.lock().unwrap().list(dir)
    }
    fn stat(&self, path: &RelPath) -> io::Result<Option<FsKind>> {
        self.inner.lock().unwrap().stat(path)
    }
    fn open(&self, path: &RelPath) -> io::Result<Box<dyn Read + Send + '_>> {
        let mut inner = self.inner.lock().unwrap();
        let data = inner.read(path)?;
        self.maybe_save(&mut inner, path, SaveOn::Open(1))?;
        Ok(Box::new(io::Cursor::new(data)))
    }
    fn read_at(&self, path: &RelPath, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        self.inner.lock().unwrap().read_at(path, offset, len)
    }
    fn begin_write(&mut self, path: &RelPath) -> io::Result<Self::Writer> {
        let mut inner = self.inner.lock().unwrap();
        self.maybe_save(&mut inner, path, SaveOn::BeginWrite)?;
        inner.begin_write(path)
    }
    fn finish_write(
        &mut self,
        path: &RelPath,
        writer: Self::Writer,
        exec: bool,
        mtime_ms: i64,
        expected: Option<FileMeta>,
    ) -> Result<FileMeta, LandError<Self::Writer>> {
        self.inner.lock().unwrap().finish_write(path, writer, exec, mtime_ms, expected)
    }
    fn keep(&mut self, writer: Self::Writer, key: &str) -> io::Result<()> {
        self.inner.lock().unwrap().keep(writer, key)
    }
    fn take_kept(&mut self, key: &str, hash: &craft_cloud_proto::FileHash) -> io::Result<Option<Self::Writer>> {
        self.inner.lock().unwrap().take_kept(key, hash)
    }
    fn kept(&self) -> io::Result<Vec<String>> {
        self.inner.lock().unwrap().kept()
    }
    fn discard_kept(&mut self, key: &str) -> io::Result<()> {
        self.inner.lock().unwrap().discard_kept(key)
    }
    fn recover(&mut self) -> io::Result<()> {
        self.inner.lock().unwrap().recover()
    }
    fn mkdir(&mut self, path: &RelPath) -> io::Result<()> {
        self.inner.lock().unwrap().mkdir(path)
    }
    fn remove_file(&mut self, path: &RelPath) -> io::Result<()> {
        self.inner.lock().unwrap().remove_file(path)
    }
    fn remove_dir(&mut self, path: &RelPath) -> io::Result<()> {
        self.inner.lock().unwrap().remove_dir(path)
    }
    fn rename(&mut self, from: &RelPath, to: &RelPath) -> io::Result<()> {
        self.inner.lock().unwrap().rename(from, to)
    }
}
