//! The filesystem as the engine sees it, and an in-memory implementation for tests and simulation.
//!
//! Real disks (the agent) implement [`Fs`] with atomic writes (temporary file + rename), stable file IDs
//! (inode / NTFS file ID) and the stability checks from the design; the engine only needs these operations.
//! File content is streamed: the engine never holds a whole file in memory.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use craft_cloud_proto::Name;

use crate::path::RelPath;

/// What a directory entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsKind {
    File(FileMeta),
    Dir {
        id: Option<u64>,
    },
    /// Symlinks, devices, sockets: never synced.
    Other,
}

/// What the engine needs to know about a file without reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileMeta {
    pub size: u64,
    pub mtime_ms: i64,
    pub exec: bool,
    /// Stable identity across renames (inode / file ID), if the platform has one.
    pub id: Option<u64>,
}

/// One entry of a directory listing. Names are raw: the engine validates them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsEntry {
    pub name: String,
    pub kind: FsKind,
}

pub trait Fs: Send {
    /// A file being written by [`Fs::begin_write`]. Dropping it without [`Fs::finish_write`] abandons the write
    /// and leaves the target untouched.
    type Writer: Write + Send;

    /// The filesystem's clock, in the same unit as file modification times (milliseconds since the Unix
    /// epoch on real disks). Used for the racy-timestamp rule.
    fn now_ms(&self) -> i64;
    /// The entries of a directory.
    fn list(&self, dir: &RelPath) -> io::Result<Vec<FsEntry>>;
    /// What is at `path`, if anything.
    fn stat(&self, path: &RelPath) -> io::Result<Option<FsKind>>;
    /// Opens a file for reading from the start.
    fn open(&self, path: &RelPath) -> io::Result<Box<dyn Read + Send + '_>>;
    /// Reads `len` bytes at `offset` (fewer only at the end of the file).
    fn read_at(&self, path: &RelPath, offset: u64, len: usize) -> io::Result<Vec<u8>>;
    /// Starts writing new content for `path`. Nothing is visible at `path` until [`Fs::finish_write`].
    fn begin_write(&mut self, path: &RelPath) -> io::Result<Self::Writer>;
    /// Puts the written content at `path` atomically (a reader sees the old content or the new, never a
    /// mix), replacing any file there, with the given executable bit and modification time.
    fn finish_write(&mut self, path: &RelPath, writer: Self::Writer, exec: bool, mtime_ms: i64) -> io::Result<FileMeta>;
    /// Reads a whole file (for tests and small files).
    fn read(&self, path: &RelPath) -> io::Result<Vec<u8>> {
        let mut buf = Vec::new();
        self.open(path)?.read_to_end(&mut buf)?;
        Ok(buf)
    }
    /// Creates or replaces a file atomically with `data`.
    fn write(&mut self, path: &RelPath, data: &[u8], exec: bool, mtime_ms: i64) -> io::Result<FileMeta> {
        let mut w = self.begin_write(path)?;
        w.write_all(data)?;
        self.finish_write(path, w, exec, mtime_ms)
    }
    fn mkdir(&mut self, path: &RelPath) -> io::Result<()>;
    fn remove_file(&mut self, path: &RelPath) -> io::Result<()>;
    /// Removes an empty directory; fails if it has entries.
    fn remove_dir(&mut self, path: &RelPath) -> io::Result<()>;
    /// Moves a file or directory; fails if `to` exists.
    fn rename(&mut self, from: &RelPath, to: &RelPath) -> io::Result<()>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MemNode {
    File {
        data: Vec<u8>,
        meta: FileMeta,
    },
    Dir {
        id: u64,
    },
    /// A symlink, junction or special file: listed, never read or followed.
    Other,
}

/// A filesystem in memory. Every write gets a later mtime; every new file or directory a new ID.
#[derive(Debug, Clone)]
pub struct MemFs {
    nodes: BTreeMap<RelPath, MemNode>,
    clock: i64,
    next_id: u64,
    locked: BTreeSet<RelPath>,
    held: BTreeSet<RelPath>,
    death: Option<Arc<Death>>,
}

/// A machine that dies partway through a pass (see [`MemFs::die_after`]).
#[derive(Debug)]
struct Death {
    /// Changes still allowed before the machine dies.
    remaining: AtomicU64,
    /// Set when it died; shared with whatever else dies with it (the network).
    dead: Arc<AtomicBool>,
}

impl Default for MemFs {
    fn default() -> Self {
        let mut nodes = BTreeMap::new();
        nodes.insert(RelPath::root(), MemNode::Dir { id: 1 });
        Self { nodes, clock: 1_000, next_id: 2, locked: BTreeSet::new(), held: BTreeSet::new(), death: None }
    }
}

fn not_found(path: &RelPath) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("{path}: not found"))
}

fn exists(path: &RelPath) -> io::Error {
    io::Error::new(io::ErrorKind::AlreadyExists, format!("{path}: already exists"))
}

impl MemFs {
    pub fn new() -> Self {
        Self::default()
    }

    fn tick(&mut self) -> i64 {
        self.clock += 1;
        self.clock
    }

    fn new_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn require_dir(&self, path: &RelPath) -> io::Result<()> {
        match self.nodes.get(path) {
            Some(MemNode::Dir { .. }) => Ok(()),
            Some(MemNode::File { .. } | MemNode::Other) => Err(io::Error::other(format!("{path}: not a directory"))),
            None => Err(not_found(path)),
        }
    }

    fn require_parent(&self, path: &RelPath) -> io::Result<()> {
        match path.parent() {
            Some(parent) => self.require_dir(&parent),
            None => Err(io::Error::other("the root has no parent")),
        }
    }

    /// Makes the machine die after `changes` more changes (writes landing, folders made, removals, renames): every
    /// filesystem call after that fails, and `dead` is set, so a transport that watches it fails too. For crash
    /// tests: the engine is cut off between two filesystem steps. [`MemFs::revive`] ends it.
    pub fn die_after(&mut self, changes: u64, dead: Arc<AtomicBool>) {
        self.death = Some(Arc::new(Death { remaining: AtomicU64::new(changes), dead }));
    }

    /// Brings a machine [`MemFs::die_after`] killed back (its folder as it was when it died).
    pub fn revive(&mut self) {
        self.death = None;
    }

    fn alive(&self) -> io::Result<()> {
        match &self.death {
            Some(d) if d.dead.load(Ordering::Relaxed) => Err(io::Error::other("the machine died")),
            _ => Ok(()),
        }
    }

    /// Spends one change; the machine dies instead when none is left.
    fn change(&self) -> io::Result<()> {
        self.alive()?;
        if let Some(d) = &self.death {
            // One engine drives a filesystem at a time, so a load and a store are enough.
            match d.remaining.load(Ordering::Relaxed).checked_sub(1) {
                Some(left) => d.remaining.store(left, Ordering::Relaxed),
                None => {
                    d.dead.store(true, Ordering::Relaxed);
                    return Err(io::Error::other("the machine died"));
                }
            }
        }
        Ok(())
    }

    /// Makes reading `path` fail with "permission denied", like a file another program holds locked.
    pub fn lock(&mut self, path: &RelPath) {
        self.locked.insert(path.clone());
    }

    pub fn unlock(&mut self, path: &RelPath) {
        self.locked.remove(path);
    }

    /// Makes removing, renaming or replacing `path` fail with "permission denied", like a file an app holds
    /// open on Windows, and listing it fail, like a folder this user may not read. Reading is unaffected
    /// (see [`MemFs::lock`]).
    pub fn hold(&mut self, path: &RelPath) {
        self.held.insert(path.clone());
    }

    pub fn release(&mut self, path: &RelPath) {
        self.held.remove(path);
    }

    /// Lets every locked and held path go.
    pub fn unlock_all(&mut self) {
        self.locked.clear();
        self.held.clear();
    }

    fn check_held(&self, path: &RelPath) -> io::Result<()> {
        if self.held.contains(path) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{path}: in use")));
        }
        Ok(())
    }

    fn data(&self, path: &RelPath) -> io::Result<&[u8]> {
        if self.locked.contains(path) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{path}: locked")));
        }
        match self.nodes.get(path) {
            Some(MemNode::File { data, .. }) => Ok(data),
            Some(MemNode::Dir { .. }) => Err(io::Error::other(format!("{path}: is a directory"))),
            Some(MemNode::Other) => Err(io::Error::other(format!("{path}: not a regular file"))),
            None => Err(not_found(path)),
        }
    }

    /// Puts a symlink (or another special file) at `path`, like a user who replaced a file or folder with a link.
    /// It lists and stats as [`FsKind::Other`] and is never read or followed; whatever was at `path` goes.
    pub fn make_link(&mut self, path: &RelPath) -> io::Result<()> {
        self.require_parent(path)?;
        let mut gone = self.below(path);
        gone.push(path.clone());
        for p in gone {
            self.nodes.remove(&p);
        }
        self.nodes.insert(path.clone(), MemNode::Other);
        Ok(())
    }

    /// Every file with its content, for comparing machines in tests.
    pub fn files(&self) -> BTreeMap<RelPath, Vec<u8>> {
        self.nodes.iter().filter_map(|(p, n)| if let MemNode::File { data, .. } = n { Some((p.clone(), data.clone())) } else { None }).collect()
    }

    /// Every directory (root excluded).
    pub fn dirs(&self) -> Vec<RelPath> {
        self.nodes.iter().filter(|(p, n)| !p.is_root() && matches!(n, MemNode::Dir { .. })).map(|(p, _)| p.clone()).collect()
    }

    /// Paths of everything below `dir` (not `dir` itself).
    fn below(&self, dir: &RelPath) -> Vec<RelPath> {
        self.nodes.keys().filter(|p| p.starts_with(dir) && *p != dir).cloned().collect()
    }
}

impl Fs for MemFs {
    type Writer = Vec<u8>;

    fn now_ms(&self) -> i64 {
        self.clock
    }

    fn list(&self, dir: &RelPath) -> io::Result<Vec<FsEntry>> {
        self.alive()?;
        self.require_dir(dir)?;
        self.check_held(dir)?;
        let depth = dir.depth() + 1;
        Ok(self
            .nodes
            .iter()
            .filter(|(p, _)| p.depth() == depth && p.starts_with(dir))
            .filter_map(|(p, n)| {
                let name = p.file_name()?.as_str().to_owned();
                let kind = match n {
                    MemNode::File { meta, .. } => FsKind::File(*meta),
                    MemNode::Dir { id } => FsKind::Dir { id: Some(*id) },
                    MemNode::Other => FsKind::Other,
                };
                Some(FsEntry { name, kind })
            })
            .collect())
    }

    fn stat(&self, path: &RelPath) -> io::Result<Option<FsKind>> {
        self.alive()?;
        Ok(self.nodes.get(path).map(|n| match n {
            MemNode::File { meta, .. } => FsKind::File(*meta),
            MemNode::Dir { id } => FsKind::Dir { id: Some(*id) },
            MemNode::Other => FsKind::Other,
        }))
    }

    fn open(&self, path: &RelPath) -> io::Result<Box<dyn Read + Send + '_>> {
        self.alive()?;
        Ok(Box::new(self.data(path)?))
    }

    fn read_at(&self, path: &RelPath, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        self.alive()?;
        let data = self.data(path)?;
        let start = usize::try_from(offset).map_or(data.len(), |o| o.min(data.len()));
        let end = start.saturating_add(len).min(data.len());
        Ok(data.get(start..end).unwrap_or_default().to_vec())
    }

    fn begin_write(&mut self, path: &RelPath) -> io::Result<Self::Writer> {
        self.alive()?;
        self.require_parent(path)?;
        if let Some(MemNode::Dir { .. }) = self.nodes.get(path) {
            return Err(io::Error::other(format!("{path}: is a directory")));
        }
        Ok(Vec::new())
    }

    fn finish_write(&mut self, path: &RelPath, writer: Self::Writer, exec: bool, mtime_ms: i64) -> io::Result<FileMeta> {
        self.change()?;
        self.require_parent(path)?;
        self.check_held(path)?;
        if let Some(MemNode::Dir { .. }) = self.nodes.get(path) {
            return Err(io::Error::other(format!("{path}: is a directory")));
        }
        // An atomic replace (temp file + rename) gives the file a new identity.
        let id = self.new_id();
        let mtime_ms = mtime_ms.max(self.tick());
        let meta = FileMeta { size: writer.len() as u64, mtime_ms, exec, id: Some(id) };
        self.nodes.insert(path.clone(), MemNode::File { data: writer, meta });
        Ok(meta)
    }

    fn mkdir(&mut self, path: &RelPath) -> io::Result<()> {
        self.change()?;
        self.require_parent(path)?;
        if self.nodes.contains_key(path) {
            return Err(exists(path));
        }
        let id = self.new_id();
        self.nodes.insert(path.clone(), MemNode::Dir { id });
        Ok(())
    }

    fn remove_file(&mut self, path: &RelPath) -> io::Result<()> {
        self.change()?;
        self.check_held(path)?;
        match self.nodes.get(path) {
            Some(MemNode::File { .. } | MemNode::Other) => {
                self.nodes.remove(path);
                Ok(())
            }
            Some(MemNode::Dir { .. }) => Err(io::Error::other(format!("{path}: is a directory"))),
            None => Err(not_found(path)),
        }
    }

    fn remove_dir(&mut self, path: &RelPath) -> io::Result<()> {
        self.change()?;
        self.require_dir(path)?;
        self.check_held(path)?;
        if path.is_root() || !self.below(path).is_empty() {
            return Err(io::Error::other(format!("{path}: not empty")));
        }
        self.nodes.remove(path);
        Ok(())
    }

    fn rename(&mut self, from: &RelPath, to: &RelPath) -> io::Result<()> {
        self.change()?;
        if !self.nodes.contains_key(from) || from.is_root() {
            return Err(not_found(from));
        }
        self.check_held(from)?;
        if self.nodes.contains_key(to) {
            return Err(exists(to));
        }
        if to.starts_with(from) {
            return Err(io::Error::other("can't move a directory into itself"));
        }
        self.require_parent(to)?;
        let mut moving = self.below(from);
        moving.push(from.clone());
        for old in moving {
            if let Some(node) = self.nodes.remove(&old) {
                let suffix = &old.names()[from.depth()..];
                let mut names = to.names().to_vec();
                names.extend_from_slice(suffix);
                self.nodes.insert(RelPath::from_names(names), node);
            }
        }
        Ok(())
    }
}

/// Builds a path from `/`-separated text; for tests and the simulation.
pub fn rel(text: &str) -> RelPath {
    RelPath::from_names(text.split('/').filter(|s| !s.is_empty()).filter_map(|s| Name::new(s).ok()).collect())
}
