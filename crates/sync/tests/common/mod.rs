//! Test harness: an in-process server (the real metadata and blob stores, no HTTP) and machines that
//! sync against it. Each test binary uses only some of it.
#![allow(dead_code)]

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
use craft_cloud_sync::{Config, Engine, FileMeta, Fs, FsEntry, FsKind, MemFs, RelPath, Report, State, Transport, TransportError};

pub struct Server {
    pub meta: Meta,
    pub store: Store,
    _dir: tempfile::TempDir,
    db_path: std::path::PathBuf,
}

impl Server {
    pub async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("meta.db");
        let meta = Meta::open(&format!("sqlite:{}", db_path.display())).await.unwrap();
        Self { meta, store: Store::new(MemoryBackend::new()), _dir: dir, db_path }
    }

    pub async fn user(&self, name: &str) -> UserRecord {
        self.meta.create_user(name, "$argon2id$test", Role::Member, 0).await.unwrap()
    }

    /// The whole-file hash of every version the server ever stored, for "nothing was lost" checks.
    pub async fn history_hashes(&self) -> BTreeSet<FileHash> {
        use sqlx::{Connection, Row};
        let mut conn = sqlx::SqliteConnection::connect(&format!("sqlite:{}", self.db_path.display())).await.unwrap();
        let rows = sqlx::query("SELECT DISTINCT manifest FROM versions").fetch_all(&mut conn).await.unwrap();
        let mut out = BTreeSet::new();
        for row in rows {
            let bytes: Vec<u8> = row.get("manifest");
            let id = ManifestId::from_bytes(<[u8; 32]>::try_from(bytes.as_slice()).unwrap());
            let manifest = Manifest::decode(&self.store.get(BlobKey::manifest(&id)).await.unwrap()).unwrap();
            out.insert(*manifest.file_hash());
        }
        out
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
    calls: AtomicU64,
}

impl Faults {
    fn check(&self) -> Result<(), TransportError> {
        let every = self.every.load(Ordering::Relaxed);
        let n = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
        if every > 0 && n.is_multiple_of(every) { Err(TransportError::Unavailable("injected failure".into())) } else { Ok(()) }
    }
}

/// The server's API rules, in-process.
pub struct Local {
    meta: Meta,
    store: Store,
    space: SpaceId,
    device: DeviceId,
    username: String,
    page: u32,
    pub faults: Arc<Faults>,
    clock: Arc<Mutex<i64>>,
}

fn meta_err(e: MetaError) -> TransportError {
    match e {
        MetaError::Rejected(c) => TransportError::Rejected(c),
        MetaError::MissingChunks(m) => TransportError::MissingChunks(m),
        MetaError::ResyncRequired => TransportError::ResyncRequired,
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
        self.store.put(BlobKey::chunk(id), &data).await.map_err(|e| TransportError::Protocol(e.to_string()))?;
        self.meta.add_chunk(self.space, id, data.len() as u64 + 1, self.now()).await.map_err(meta_err)
    }

    async fn get_chunk(&self, id: &ChunkId) -> Result<Bytes, TransportError> {
        self.faults.check()?;
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
        let out = self.meta.commit(self.space, self.device, ops, self.now()).await.map_err(meta_err)?;
        if self.faults.lose_commit_reply.swap(false, Ordering::Relaxed) {
            return Err(TransportError::Unavailable("reply lost".into()));
        }
        Ok(CommitResponse { seq: out.seq, epoch: out.epoch })
    }
}

pub const STAMP: ConflictStamp = ConflictStamp { year: 2026, month: 10, day: 11, hour: 14, minute: 3 };

pub type Machine = Engine<MemFs, Local>;

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
        meta: server.meta.clone(),
        store: server.store.clone(),
        space: user.space,
        device,
        username: user.username.clone(),
        page,
        faults: Arc::new(Faults::default()),
        clock: Arc::new(Mutex::new(1_760_000_000_000)),
    };
    let mut config = Config::new(name, device);
    tweak(&mut config);
    Engine::new(fs, transport, State::default(), config, || STAMP).unwrap()
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

/// A [`MemFs`] where a program saves a file while the engine is writing a download to the same path: the
/// save lands when the engine starts writing (after it checked the path, before its write lands).
#[derive(Default)]
pub struct SaveDuringWrite {
    pub inner: MemFs,
    /// The path and the content the program saves, once.
    pub save: Option<(RelPath, Vec<u8>)>,
}

impl Fs for SaveDuringWrite {
    type Writer = <MemFs as Fs>::Writer;

    fn now_ms(&self) -> i64 {
        self.inner.now_ms()
    }
    fn list(&self, dir: &RelPath) -> io::Result<Vec<FsEntry>> {
        self.inner.list(dir)
    }
    fn stat(&self, path: &RelPath) -> io::Result<Option<FsKind>> {
        self.inner.stat(path)
    }
    fn open(&self, path: &RelPath) -> io::Result<Box<dyn Read + Send + '_>> {
        self.inner.open(path)
    }
    fn read_at(&self, path: &RelPath, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        self.inner.read_at(path, offset, len)
    }
    fn begin_write(&mut self, path: &RelPath) -> io::Result<Self::Writer> {
        if self.save.as_ref().is_some_and(|(p, _)| p == path)
            && let Some((p, data)) = self.save.take()
        {
            self.inner.write(&p, &data, false, 0)?;
        }
        self.inner.begin_write(path)
    }
    fn finish_write(&mut self, path: &RelPath, writer: Self::Writer, exec: bool, mtime_ms: i64) -> io::Result<FileMeta> {
        self.inner.finish_write(path, writer, exec, mtime_ms)
    }
    fn mkdir(&mut self, path: &RelPath) -> io::Result<()> {
        self.inner.mkdir(path)
    }
    fn remove_file(&mut self, path: &RelPath) -> io::Result<()> {
        self.inner.remove_file(path)
    }
    fn remove_dir(&mut self, path: &RelPath) -> io::Result<()> {
        self.inner.remove_dir(path)
    }
    fn rename(&mut self, from: &RelPath, to: &RelPath) -> io::Result<()> {
        self.inner.rename(from, to)
    }
}
