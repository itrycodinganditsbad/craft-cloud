//! The filesystem as the engine sees it, and an in-memory implementation for tests and simulation.
//!
//! Real disks (the agent) implement [`Fs`] with atomic writes (temporary file + rename), stable file IDs
//! (inode / NTFS file ID) and the stability checks from the design; the engine only needs these operations.
//! File content is streamed: the engine never holds a whole file in memory.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use craft_cloud_proto::{ConflictStamp, FileHash, Name, conflict_name};

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

/// Why [`Fs::finish_write`] didn't land a write. The target is as it was before the call, whatever happened.
pub enum LandError<W> {
    /// The target isn't what the caller expected: something is at a path that had to be empty, or the file there
    /// isn't the one the caller checked (an app saved it meanwhile). The written content comes back, so it can be
    /// kept ([`Fs::keep`]).
    Changed(W),
    /// The target is in use: a program holds it open and the system won't let it be replaced or moved (a sharing
    /// violation on Windows), or replacing it isn't permitted. The written content comes back, with the reason in words
    /// ("in use by another program", "can't be replaced here (no permission)").
    InUse(W, io::Error),
    /// Any other failure; the written content is gone.
    Io(io::Error),
}

impl<W> std::fmt::Debug for LandError<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LandError::Changed(_) => f.write_str("Changed"),
            LandError::InUse(_, e) => f.debug_tuple("InUse").field(e).finish(),
            LandError::Io(e) => f.debug_tuple("Io").field(e).finish(),
        }
    }
}

impl<W> From<io::Error> for LandError<W> {
    fn from(e: io::Error) -> Self {
        LandError::Io(e)
    }
}

impl<W> From<LandError<W>> for io::Error {
    fn from(e: LandError<W>) -> Self {
        match e {
            LandError::Changed(_) => io::Error::new(io::ErrorKind::AlreadyExists, "the target changed meanwhile"),
            LandError::InUse(_, e) => e,
            LandError::Io(e) => e,
        }
    }
}

/// Why [`Fs::remove_file`] refused a file: it is marked read-only here (the Windows attribute), which is the user
/// saying "don't change this". The engine skips the node with this reason and keeps its base, so the delete waits
/// until the mark is cleared. Make one with [`read_only_error`], tell one with [`is_read_only`].
#[derive(Debug)]
pub struct ReadOnly;

impl std::fmt::Display for ReadOnly {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("read-only here, not removed")
    }
}

impl std::error::Error for ReadOnly {}

/// The error [`Fs::remove_file`] gives for a file marked read-only here (see [`ReadOnly`]).
pub fn read_only_error() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, ReadOnly)
}

/// Whether `e` is [`read_only_error`]'s.
pub fn is_read_only(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<ReadOnly>())
}

pub trait Fs: Send {
    /// A file being written by [`Fs::begin_write`]. Dropping it without [`Fs::finish_write`] abandons the write
    /// and leaves the target untouched.
    type Writer: Write + Send;

    /// The filesystem's clock, in the same unit as file modification times (milliseconds since the Unix
    /// epoch on real disks). Used for the racy-timestamp rule.
    fn now_ms(&self) -> i64;
    /// Whether files here have an executable bit (Unix-like systems). Where they don't (Windows), every file reads
    /// as not executable, so an upload keeps the bit the base recorded instead of clearing it.
    fn has_exec_bit(&self) -> bool {
        true
    }
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
    /// Puts the written content at `path` atomically (a reader sees the old content or the new, never a mix), with
    /// the given executable bit and modification time, **only if `path` holds what the caller checked**: nothing
    /// when `expected` is `None`, otherwise a file whose [`FileMeta`] (size, time, exec bit, ID) is exactly
    /// `expected`. The comparison and the replace are one step as far as the platform allows (an exchange of the
    /// two files, then a look at the one that came out, put back if it isn't the expected one), so an app's save
    /// between the caller's check and the landing is never overwritten: [`LandError::Changed`] instead.
    fn finish_write(
        &mut self,
        path: &RelPath,
        writer: Self::Writer,
        exec: bool,
        mtime_ms: i64,
        expected: Option<FileMeta>,
    ) -> Result<FileMeta, LandError<Self::Writer>>;
    /// Keeps a finished write that didn't land ([`LandError::Changed`] or [`LandError::InUse`]) under `key`, so a
    /// later pass can land it without writing it again ([`Fs::take_kept`]). Replaces anything kept under `key`.
    /// A filesystem that can't keep writes drops it (the default).
    fn keep(&mut self, writer: Self::Writer, key: &str) -> io::Result<()> {
        drop(writer);
        let _ = key;
        Ok(())
    }
    /// Takes back the write kept under `key`, if any, and only if its content still hashes to `hash` (BLAKE3, the
    /// whole file): a kept file that changed on disk is dropped instead, and the caller writes it again. Either way it
    /// is no longer kept.
    fn take_kept(&mut self, key: &str, hash: &FileHash) -> io::Result<Option<Self::Writer>> {
        let _ = (key, hash);
        Ok(None)
    }
    /// The keys of every kept write.
    fn kept(&self) -> io::Result<Vec<String>> {
        Ok(Vec::new())
    }
    /// Drops the write kept under `key`, if any.
    fn discard_kept(&mut self, key: &str) -> io::Result<()> {
        let _ = key;
        Ok(())
    }
    /// Finishes or undoes landings a crash cut short, where a landing takes more than one step (no exchange on this
    /// platform): a file moved aside is put back where nothing took its place, or removed when the landing was
    /// complete but for that. Anything else is left where it is, as a file the next scan sees. A moved file whose
    /// folder the user moved meanwhile is found there (by the folder's ID) and handled the same way under its own
    /// name; if that name is taken, it gets a conflict name. The engine calls it at the start of every pass.
    ///
    /// Known limit: a crash after the download took the name but before the moved file was removed, followed by the
    /// user deleting the download before the next pass, puts the old file back (the user's delete of a file they
    /// may never have seen is undone; nothing is lost).
    fn recover(&mut self) -> io::Result<()> {
        Ok(())
    }
    /// What the last [`Fs::recover`] left for the user, as (what, what to do) pairs: a record of an interrupted landing
    /// it couldn't read and set aside, a file it couldn't put back. The engine adds them to every pass's
    /// [`Report::skipped`](crate::Report::skipped) until the user deals with them.
    fn notices(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    /// Paths whose content is unknown right now: the targets of landings in steps that haven't finished (a step
    /// failed and putting the moved file back failed too, so [`Fs::recover`] finishes it on the next pass). The scan
    /// treats them like a folder it can't list: a tracked file there is never taken for deleted. The moved files
    /// themselves are left out of listings.
    fn unsettled(&self) -> Vec<RelPath> {
        Vec::new()
    }
    /// Reads a whole file (for tests and small files).
    fn read(&self, path: &RelPath) -> io::Result<Vec<u8>> {
        let mut buf = Vec::new();
        self.open(path)?.read_to_end(&mut buf)?;
        Ok(buf)
    }
    /// Creates or replaces a file atomically with `data`, whatever file is at `path` now (for tests and tools: the
    /// engine always says what it expects).
    fn write(&mut self, path: &RelPath, data: &[u8], exec: bool, mtime_ms: i64) -> io::Result<FileMeta> {
        let expected = match self.stat(path)? {
            Some(FsKind::File(meta)) => Some(meta),
            None => None,
            Some(_) => return Err(io::Error::other(format!("{path}: not a regular file"))),
        };
        let mut w = self.begin_write(path)?;
        w.write_all(data)?;
        self.finish_write(path, w, exec, mtime_ms, expected).map_err(io::Error::from)
    }
    fn mkdir(&mut self, path: &RelPath) -> io::Result<()>;
    /// Removes a file. A file marked read-only here (Windows) is never removed: [`read_only_error`].
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
    /// Files marked read-only, like the Windows attribute (see [`MemFs::set_read_only`]).
    read_only: BTreeSet<RelPath>,
    death: Option<Arc<Death>>,
    /// False to act like Windows: no file is executable (see [`MemFs::without_exec_bit`]).
    exec_bit: bool,
    /// False to land writes the way Windows does, in steps, instead of with one exchange (see
    /// [`MemFs::without_swap`]).
    swap: bool,
    /// Writes kept for a later pass, by key ([`Fs::keep`]).
    kept: BTreeMap<String, Vec<u8>>,
    /// Landings in steps that haven't finished (a machine died between two steps), for [`Fs::recover`].
    asides: Vec<Aside>,
    /// Landings in steps fail after moving the file there aside, and putting it back fails too (see
    /// [`MemFs::failing_landings`]).
    fail_landings: bool,
}

/// A landing in steps under way: the file that was at `target` is at `aside` until the download is in place and
/// the moved file removed.
#[derive(Debug, Clone)]
struct Aside {
    target: RelPath,
    aside: RelPath,
    /// The file that was at `target`, as the caller expected it.
    expected: FileMeta,
    /// The file that lands at `target`.
    incoming: FileMeta,
    /// The ID of the folder both are in, so recovery finds them after the user moved that folder.
    folder: Option<u64>,
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
        Self {
            nodes,
            clock: 1_000,
            next_id: 2,
            locked: BTreeSet::new(),
            held: BTreeSet::new(),
            read_only: BTreeSet::new(),
            death: None,
            exec_bit: true,
            swap: true,
            kept: BTreeMap::new(),
            asides: Vec::new(),
            fail_landings: false,
        }
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

    /// A filesystem without an executable bit, like Windows: files are written and read as not executable.
    pub fn without_exec_bit(mut self) -> Self {
        self.exec_bit = false;
        for node in self.nodes.values_mut() {
            if let MemNode::File { meta, .. } = node {
                meta.exec = false;
            }
        }
        self
    }

    /// A filesystem that lands a write over an existing file in steps, like Windows (and disks without an exchange):
    /// the file there is moved to a name next to it, checked, the new content is put in place, and the moved file
    /// removed. Each step is a change of its own, so a machine that dies ([`MemFs::die_after`]) can stop between
    /// them; [`Fs::recover`] then puts things right. By default a write lands with one exchange (Linux, macOS).
    pub fn without_swap(mut self) -> Self {
        self.swap = false;
        self
    }

    /// Moves the node at `from` (and anything below it) to `to`, which must be free.
    fn move_node(&mut self, from: &RelPath, to: &RelPath) {
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
    }

    fn file_meta(&self, path: &RelPath) -> Option<FileMeta> {
        match self.nodes.get(path) {
            Some(MemNode::File { meta, .. }) => Some(*meta),
            _ => None,
        }
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

    /// Marks the file at `path` read-only (`true`) or clears the mark, like the Windows attribute: a read-only file is
    /// never removed ([`read_only_error`]) or replaced ([`LandError::InUse`], "read-only here, not replaced"). It can
    /// still be read and renamed (the mark goes with it).
    pub fn set_read_only(&mut self, path: &RelPath, read_only: bool) {
        if read_only {
            self.read_only.insert(path.clone());
        } else {
            self.read_only.remove(path);
        }
    }

    /// While `on`, a landing in steps ([`MemFs::without_swap`]) fails right after moving the file there aside, and
    /// putting it back fails too, as two disk errors would: the target stays empty and the landing pending until
    /// [`Fs::recover`] puts the moved file back.
    pub fn failing_landings(&mut self, on: bool) {
        self.fail_landings = on;
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

    /// Where a landing's target and moved file are now, if the moved file isn't at its recorded place but in the
    /// folder that had the landing's folder ID (the user moved that folder before recovery ran).
    fn relocated(&self, record: &Aside) -> Option<(RelPath, RelPath)> {
        if self.nodes.contains_key(&record.aside) {
            return None;
        }
        let folder = record.folder?;
        let dir = self.nodes.iter().find_map(|(p, n)| matches!(n, MemNode::Dir { id } if *id == folder).then(|| p.clone()))?;
        let (target, aside) = (dir.join(record.target.file_name()?), dir.join(record.aside.file_name()?));
        self.file_meta(&aside).is_some().then_some((target, aside))
    }

    /// A free conflict-copy name next to `path`.
    fn free_conflict_name(&self, path: &RelPath) -> Option<RelPath> {
        let (parent, name) = (path.parent()?, path.file_name()?);
        let stamp = ConflictStamp { year: 2000, month: 1, day: 1, hour: 0, minute: 0, utc: true };
        (1..=1000)
            .filter_map(|n| conflict_name(name, false, "this computer", stamp, n).ok())
            .map(|n| parent.join(&n))
            .find(|p| !self.nodes.contains_key(p))
    }
}

impl Fs for MemFs {
    type Writer = Vec<u8>;

    fn now_ms(&self) -> i64 {
        self.clock
    }

    fn has_exec_bit(&self) -> bool {
        self.exec_bit
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
            // A file moved aside by a landing that hasn't finished is recovery's, never synced.
            .filter(|(p, _)| !self.asides.iter().any(|a| &a.aside == *p))
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

    /// A held path ([`MemFs::hold`]) is in use: a write over the file there fails with [`LandError::InUse`], as a
    /// rename onto a file an app holds open does on Windows.
    fn finish_write(
        &mut self,
        path: &RelPath,
        writer: Self::Writer,
        exec: bool,
        mtime_ms: i64,
        expected: Option<FileMeta>,
    ) -> Result<FileMeta, LandError<Self::Writer>> {
        self.change()?;
        self.require_parent(path)?;
        if let Some(MemNode::Dir { .. }) = self.nodes.get(path) {
            return Err(LandError::Io(io::Error::other(format!("{path}: is a directory"))));
        }
        // An atomic replace (temp file + rename) gives the file a new identity.
        let id = self.new_id();
        let mtime_ms = mtime_ms.max(self.tick());
        let meta = FileMeta { size: writer.len() as u64, mtime_ms, exec: exec && self.exec_bit, id: Some(id) };
        let Some(expected) = expected else {
            // A rename that never replaces: one step.
            if self.nodes.contains_key(path) {
                return Err(LandError::Changed(writer));
            }
            self.nodes.insert(path.clone(), MemNode::File { data: writer, meta });
            return Ok(meta);
        };
        if self.file_meta(path) != Some(expected) {
            return Err(LandError::Changed(writer));
        }
        if self.held.contains(path) {
            return Err(LandError::InUse(writer, io::Error::new(io::ErrorKind::PermissionDenied, "in use by another program")));
        }
        if self.read_only.contains(path) {
            return Err(LandError::InUse(writer, io::Error::new(io::ErrorKind::PermissionDenied, "read-only here, not replaced")));
        }
        if self.swap {
            // One exchange, and the file that came out is the expected one: it goes.
            self.nodes.insert(path.clone(), MemNode::File { data: writer, meta });
            return Ok(meta);
        }
        // In steps. First (the change spent above) the file there moves to a name next to it, written down so a
        // crash between the steps can be put right.
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(LandError::Io(io::Error::other("the root is a folder")));
        };
        let aside = Name::new(&format!("{name}.{id}.cc-aside")).map(|n| parent.join(&n)).map_err(|e| io::Error::other(e.to_string()))?;
        if self.nodes.contains_key(&aside) {
            return Err(LandError::Io(exists(&aside)));
        }
        let folder = match self.nodes.get(&parent) {
            Some(MemNode::Dir { id }) => Some(*id),
            _ => None,
        };
        self.move_node(path, &aside);
        self.asides.push(Aside { target: path.clone(), aside: aside.clone(), expected, incoming: meta, folder });
        if self.fail_landings {
            return Err(LandError::Io(io::Error::other(format!("{path}: the landing failed and the file couldn't be put back (test)"))));
        }
        // Then the new content takes the free name (nothing can take it in between here)…
        self.change()?;
        self.nodes.insert(path.clone(), MemNode::File { data: writer, meta });
        // …and the moved file goes.
        self.change()?;
        self.nodes.remove(&aside);
        self.asides.retain(|a| a.aside != aside);
        Ok(meta)
    }

    fn keep(&mut self, writer: Self::Writer, key: &str) -> io::Result<()> {
        self.alive()?;
        self.kept.insert(key.to_owned(), writer);
        Ok(())
    }

    fn take_kept(&mut self, key: &str, hash: &FileHash) -> io::Result<Option<Self::Writer>> {
        self.alive()?;
        Ok(self.kept.remove(key).filter(|data| blake3::hash(data).as_bytes() == hash.as_bytes()))
    }

    fn kept(&self) -> io::Result<Vec<String>> {
        self.alive()?;
        Ok(self.kept.keys().cloned().collect())
    }

    fn discard_kept(&mut self, key: &str) -> io::Result<()> {
        self.alive()?;
        self.kept.remove(key);
        Ok(())
    }

    /// Like the agent's disk: a moved file whose folder the user moved since is found in that folder by the folder's
    /// ID, and handled there under its own name; if that name is taken by anything but the download, it gets a
    /// conflict name instead (never a `.cc-aside` name the scan would take for the tracked file's new place).
    fn recover(&mut self) -> io::Result<()> {
        while let Some(record) = self.asides.first().cloned() {
            let (target, aside, moved_with_folder) = match self.relocated(&record) {
                Some((target, aside)) => (target, aside, true),
                None => (record.target.clone(), record.aside.clone(), false),
            };
            match (self.file_meta(&aside), self.nodes.contains_key(&target)) {
                // Moved aside, nothing took its place: it goes back.
                (Some(_), false) => {
                    self.change()?;
                    self.move_node(&aside, &target);
                }
                // The landing was done but for removing the file it replaced.
                (Some(moved), true) if moved == record.expected && self.file_meta(&target) == Some(record.incoming) => {
                    self.change()?;
                    self.nodes.remove(&aside);
                }
                // In a folder the user moved, with its name taken: a conflict name next to it.
                (Some(_), true) if moved_with_folder => {
                    if let Some(copy) = self.free_conflict_name(&target) {
                        self.change()?;
                        self.move_node(&aside, &copy);
                    }
                }
                // Gone already, or something else is going on: the moved file stays as it is, and the scan sees it.
                _ => {}
            }
            self.asides.remove(0);
        }
        Ok(())
    }

    fn unsettled(&self) -> Vec<RelPath> {
        self.asides.iter().map(|a| a.target.clone()).collect()
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
            Some(MemNode::File { .. }) if self.read_only.contains(path) => Err(read_only_error()),
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
        self.move_node(from, to);
        // Read-only marks go with the files.
        let marked: Vec<RelPath> = self.read_only.iter().filter(|p| p.starts_with(from)).cloned().collect();
        for old in marked {
            self.read_only.remove(&old);
            let mut names = to.names().to_vec();
            names.extend_from_slice(old.names().get(from.depth()..).unwrap_or_default());
            self.read_only.insert(RelPath::from_names(names));
        }
        Ok(())
    }
}

/// Builds a path from `/`-separated text; for tests and the simulation.
pub fn rel(text: &str) -> RelPath {
    RelPath::from_names(text.split('/').filter(|s| !s.is_empty()).filter_map(|s| Name::new(s).ok()).collect())
}
