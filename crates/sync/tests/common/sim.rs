//! What the random-history simulations share (`tests/simulation.rs` in process, the agent's `tests/network/` over a
//! simulated network): the random source, the users' actions on a folder, the checks after every pass, settling at
//! the end with its checks, and running many seeds in parallel. Machines are generic over the transport.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use craft_cloud_proto::{FileHash, Name, NodeId};
use craft_cloud_sync::{BaseKind, Fs, FsKind, RelPath, Report, State, SyncError, Transport, rel};

use super::{Machine, Server};

/// xorshift64*: tiny, deterministic, good enough to pick actions.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        if items.is_empty() { None } else { items.get(self.below(items.len())) }
    }

    pub fn bytes(&mut self) -> Vec<u8> {
        // Mostly small files; sometimes big enough for several chunks.
        let len = if self.below(25) == 0 { 300_000 + self.below(500_000) } else { 1 + self.below(64) };
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            out.extend_from_slice(&self.next().to_le_bytes());
        }
        out.truncate(len);
        out
    }
}

pub const NAMES: &[&str] = &["a.txt", "b.txt", "A.TXT", "c.psd", "notes"];
pub const DIRS: &[&str] = &["x", "y", "x/z", "y/w"];

pub fn tracing() -> bool {
    std::env::var("SIM_TRACE").is_ok()
}

pub fn files_of<T: Transport>(m: &Machine<T>) -> Vec<RelPath> {
    m.fs().files().into_keys().collect()
}

/// Every file with the start of its content hash, for `SIM_TRACE`.
pub fn listing<T: Transport>(m: &Machine<T>) -> String {
    m.fs().files().iter().map(|(p, d)| format!("{p}={}", blake3::hash(d).to_hex().get(..6).unwrap_or(""))).collect::<Vec<_>>().join(" ")
}

pub fn hashes_of<T: Transport>(m: &Machine<T>) -> BTreeSet<FileHash> {
    m.fs().files().values().map(|d| FileHash::from_bytes(*blake3::hash(d).as_bytes())).collect()
}

/// Whether a pass left nothing for later: no refused operation, no node skipped for a filesystem error. A machine
/// recovering from a crash has redone what the crash lost once it finishes such a pass.
pub fn settled(report: &Report) -> bool {
    report.refused.is_empty() && !report.skipped.iter().any(|(_, why)| why.ends_with("tried again next pass"))
}

/// How many finished passes after a restart a machine may still be redoing what the crash lost (passes that fail
/// partway don't count: they haven't redone it yet). The first finished pass applies the server's tree and redoes
/// the lost bookkeeping; a refused operation is resubmitted on the next pass, and a node skipped for a filesystem
/// error (a held file) is retried on the one after, so three finished passes cover a crash's own consequences.
/// Copies made later are the engine's ordinary decisions and are checked.
pub const RECOVERY_PASSES: u32 = 3;

/// The contents of the files a pass uploaded as new (at paths the machine didn't track before the pass).
pub fn new_uploads<T: Transport>(report: &Report, tracked_before: &BTreeMap<RelPath, NodeId>, m: &Machine<T>) -> Vec<FileHash> {
    let files = m.fs().files();
    report
        .uploaded
        .iter()
        .filter(|p| !tracked_before.contains_key(*p))
        .filter_map(|p| files.get(p))
        .map(|d| FileHash::from_bytes(*blake3::hash(d).as_bytes()))
        .collect()
}

/// The conflict copies that appeared on `m` since `before` (its files then), with their contents.
pub fn new_copies<T: Transport>(before: &[RelPath], m: &Machine<T>) -> Vec<(RelPath, FileHash)> {
    m.fs()
        .files()
        .into_iter()
        .filter(|(p, _)| !before.contains(p) && p.file_name().is_some_and(|n| n.as_str().contains(" (conflict, ")))
        .map(|(p, d)| (p, FileHash::from_bytes(*blake3::hash(&d).as_bytes())))
        .collect()
}

/// The engine's skip reason for a name that clashes by case with one it syncs (`Engine::scan`).
pub fn is_case_clash(reason: &str) -> bool {
    reason.starts_with("the name clashes with ") && reason.ends_with(" when case is ignored")
}

/// A conflict copy exempt from the needless-copy check: the machine that made it, where it is, and the content it
/// was made with. An edit of the copy (or a later copy at the same path) is checked again.
pub type Exempt = (usize, RelPath, FileHash);

/// The path a conflict copy was made from: `a (conflict, m0, 2026-10-11 14-03).txt` → `a.txt` in the same folder
/// (the last conflict mark, for a copy of a copy). `None` for a name that isn't a conflict copy.
pub fn original_of(copy: &RelPath) -> Option<RelPath> {
    let (parent, name) = (copy.parent()?, copy.file_name()?.as_str());
    let start = name.rfind(" (conflict, ")?;
    let end = start + name.get(start..)?.find(')')? + 1;
    Some(rel(&format!("{parent}/{}{}", name.get(..start)?, name.get(end..)?)))
}

/// The files on `m` that differ from its base by size or time, or aren't tracked: the ones the engine asks about
/// (`Fs::still_changing`) before it uploads them or a server change moves them aside.
pub fn differing<T: Transport>(m: &Machine<T>) -> Vec<RelPath> {
    let st = m.state();
    let index = st.base_index();
    files_of(m)
        .into_iter()
        .filter(|p| {
            let Ok(Some(FsKind::File(meta))) = m.fs().stat(p) else { return false };
            !index
                .get(p)
                .and_then(|id| st.base.get(id))
                .is_some_and(|e| matches!(e.kind, BaseKind::File { size, mtime_ms, .. } if size == meta.size && mtime_ms == meta.mtime_ms))
        })
        .collect()
}

/// The files on `m` that can't be read now (another program holds them locked).
pub fn unreadable<T: Transport>(m: &Machine<T>) -> BTreeSet<RelPath> {
    files_of(m).into_iter().filter(|p| m.fs().read(p).is_err()).collect()
}

/// After a user's rename of `from` to `to`, exempt copies at or below `from` are found at `to` (on every machine,
/// once it syncs), with the same content.
pub fn follow_rename(exempt: &mut BTreeSet<Exempt>, from: &RelPath, to: &RelPath) {
    let moved: Vec<Exempt> = exempt.iter().filter(|(_, p, _)| p.starts_with(from)).cloned().collect();
    for (j, p, hash) in moved {
        let mut names = to.names().to_vec();
        names.extend_from_slice(&p.names()[from.depth()..]);
        exempt.insert((j, RelPath::from_names(names), hash));
    }
}

/// A tracked file or folder a user replaced with a link (moved to another drive with a link left behind, say),
/// and what it held, to put back later. A machine has at most one at a time, so the link is the only one in its
/// folder wherever it ends up (a folder above it may be renamed or moved aside).
pub struct Linked {
    was_dir: bool,
    /// The folders and files it held, by their names below it (a file itself: one file with no names).
    dirs: Vec<Vec<Name>>,
    files: Vec<(Vec<Name>, Vec<u8>)>,
    /// The nodes the machine tracked there when the link was made: it must never delete them on the server
    /// while the link is there.
    nodes: BTreeSet<NodeId>,
    /// Tracked folders the engine made again while the link was there (missing before a pass, there after it,
    /// whether the pass finished or not; nobody acts during a pass): see the delete check in `check_pass`.
    remade: BTreeSet<RelPath>,
    /// Whether another machine has been asked to move a file out of the folder this link replaced (see
    /// `tests/simulation.rs`).
    moved_out: bool,
}

/// Replaces a tracked file or folder on `m` with a link, keeping what it held.
fn make_link<T: Transport>(rng: &mut Rng, m: &mut Machine<T>) -> Option<Linked> {
    let index = m.state().base_index();
    // (Never a live document's file: only its app touches that, see `act`.)
    let candidates: Vec<RelPath> = index
        .keys()
        .filter(|p| !p.is_root() && !p.file_name().is_some_and(|n| n.as_str().ends_with(".kitdemo")))
        .filter(|p| matches!(m.fs().stat(p), Ok(Some(FsKind::File(_) | FsKind::Dir { .. }))))
        .cloned()
        .collect();
    let at = rng.pick(&candidates)?.clone();
    let below = |p: &RelPath| p.names().get(at.depth()..).map(<[Name]>::to_vec).unwrap_or_default();
    let was_dir = matches!(m.fs().stat(&at), Ok(Some(FsKind::Dir { .. })));
    let dirs = m.fs().dirs().into_iter().filter(|d| d.starts_with(&at) && *d != at).map(|d| below(&d)).collect();
    let files = m.fs().files().into_iter().filter(|(p, _)| p.starts_with(&at)).map(|(p, d)| (below(&p), d)).collect();
    let nodes = index.iter().filter(|(p, id)| p.starts_with(&at) && on_disk_here(m, id, p)).map(|(_, id)| *id).collect();
    m.fs_mut().make_link(&at).ok()?;
    if tracing() {
        eprintln!("TRACE link {at}");
    }
    Some(Linked { was_dir, dirs, files, nodes, remade: BTreeSet::new(), moved_out: false })
}

/// Whether the tracked node `id` is itself at its base path `p` on disk: the folder or file there has the node's
/// local ID. A node the user already moved (found by its ID elsewhere) or deleted isn't behind a link made at its
/// old place later, even if something new took that place first. (A file a user's save replaced at its path is
/// left out too: the check is a little weaker, never wrong.)
fn on_disk_here<T: Transport>(m: &Machine<T>, id: &NodeId, p: &RelPath) -> bool {
    match (m.state().base.get(id).map(|e| &e.kind), m.fs().stat(p)) {
        (Some(BaseKind::Dir { local_id }), Ok(Some(FsKind::Dir { id: here }))) => local_id.is_none() || *local_id == here,
        (Some(BaseKind::File { local_id, .. }), Ok(Some(FsKind::File(meta)))) => local_id.is_none() || *local_id == meta.id,
        _ => false,
    }
}

/// Whether `path` is a link on `m`, or below one.
fn behind_link<T: Transport>(m: &Machine<T>, path: &RelPath) -> bool {
    let mut prefix = RelPath::root();
    path.names().iter().any(|name| {
        prefix = prefix.join(name);
        matches!(m.fs().stat(&prefix), Ok(Some(FsKind::Other)))
    })
}

/// Where the machine's link is now: the only link or special file in its folder.
pub fn find_link<T: Transport>(m: &Machine<T>) -> Option<RelPath> {
    let mut queue = vec![RelPath::root()];
    while let Some(dir) = queue.pop() {
        // A folder that can't be listed now is looked into another time.
        let Ok(entries) = m.fs().list(&dir) else { continue };
        for entry in entries {
            let Ok(name) = Name::new(&entry.name) else { continue };
            match entry.kind {
                FsKind::Other => return Some(dir.join(&name)),
                FsKind::Dir { .. } => queue.push(dir.join(&name)),
                FsKind::File(_) => {}
            }
        }
    }
    None
}

/// Puts back what a link replaced, wherever the link is now (the drive is mounted again). Returns whether it did:
/// a link an app holds open stays for now.
pub fn restore_link<T: Transport>(m: &mut Machine<T>, linked: &Linked) -> bool {
    let Some(at) = find_link(m) else { return false };
    if m.fs_mut().remove_file(&at).is_err() {
        return false;
    }
    let under = |names: &[Name]| RelPath::from_names(at.names().iter().chain(names).cloned().collect());
    // What comes back is new on this disk: no program has it open or locked (MemFs keeps holds and locks by path,
    // and some may be left from files that were at these paths before).
    for p in linked.dirs.iter().chain(linked.files.iter().map(|(f, _)| f)).map(|names| under(names)) {
        m.fs_mut().release(&p);
        m.fs_mut().unlock(&p);
    }
    if linked.was_dir {
        m.fs_mut().mkdir(&at).unwrap_or_else(|e| panic!("putting back {at}: {e}"));
    }
    let mut dirs = linked.dirs.clone();
    dirs.sort_by_key(Vec::len);
    for d in &dirs {
        m.fs_mut().mkdir(&under(d)).unwrap_or_else(|e| panic!("putting back {at}: {e}"));
    }
    for (f, data) in &linked.files {
        m.fs_mut().write(&under(f), data, false, 0).unwrap_or_else(|e| panic!("putting back {at}: {e}"));
    }
    if tracing() {
        eprintln!("TRACE put back {at}");
    }
    true
}

/// Puts back a folder a link replaced, makes a link on one of the files just put back, and renames the folder, with
/// no pass in between (seed 112337's no-pass order: the engine can't tell the move from a delete plus a new folder,
/// so it must hold the delete of the node behind the new link). The choices come from `extra`, a stream of their
/// own. `Err` if nothing was put back (the old link stays), else the new link (`None` if none could be made). The
/// node behind the new link is recorded by base path (the one the folder's old place tracks at the file's names, if
/// it was behind the folder's link), not by local ID: the files put back have new IDs, so the ID check would leave
/// out exactly the node that matters. (The folders above it wait with it, but only while it is below them: another
/// machine may move it out meanwhile, so they aren't recorded.)
fn put_back_and_move<T: Transport>(
    extra: &mut Rng,
    m: &mut Machine<T>,
    linked: &Linked,
    exempt: &mut BTreeSet<Exempt>,
) -> Result<Option<Linked>, ()> {
    let at = find_link(m).ok_or(())?;
    let file_names: Vec<&Vec<Name>> = linked.files.iter().map(|(f, _)| f).filter(|f| !f.is_empty()).collect();
    let names = extra.pick(&file_names).map(|f| (*f).clone()).ok_or(())?;
    if !restore_link(m, linked) {
        return Err(());
    }
    let file = RelPath::from_names(at.names().iter().chain(&names).cloned().collect());
    let Some(data) = m.fs().files().get(&file).cloned() else { return Ok(None) };
    let index = m.state().base_index();
    let nodes: BTreeSet<NodeId> = index.get(&file).copied().filter(|id| linked.nodes.contains(id)).into_iter().collect();
    if m.fs_mut().make_link(&file).is_err() {
        return Ok(None);
    }
    let mut now = at.clone();
    for _ in 0..4 {
        let to = rel(extra.pick(DIRS).copied().unwrap_or("y"));
        if m.fs_mut().rename(&at, &to).is_ok() {
            follow_rename(exempt, &at, &to);
            now = to;
            break;
        }
    }
    // Half the time a tracked folder off the gesture's path can't be listed meanwhile (review 6's P1 and P2): the
    // hold must not depend on the scan seeing everything.
    if extra.below(2) == 0 {
        let off_path = |d: &RelPath| ![&at, &now].iter().any(|g| d.starts_with(g) || g.starts_with(d));
        let others: Vec<RelPath> = m.fs().dirs().into_iter().filter(|d| index.contains_key(d) && off_path(d)).collect();
        if let Some(d) = extra.pick(&others).cloned() {
            if tracing() {
                eprintln!("TRACE hold {d} (during the put-back)");
            }
            m.fs_mut().hold(&d);
        }
    }
    if tracing() {
        eprintln!("TRACE put back {at}, link {file}, moved to {now}; behind it: {nodes:?}");
    }
    Ok(Some(Linked { was_dir: false, dirs: Vec::new(), files: vec![(Vec::new(), data)], nodes, remade: BTreeSet::new(), moved_out: false }))
}

/// One random user action on a machine's folder. Failures (target exists, parent missing) are ignored, like a
/// user's failed click. `exempt` follows the user's renames; `link` is the machine's link, if it has one. `extra`
/// is a stream of its own for the rarer compound actions, so the histories the others make stay as they were.
pub fn act<T: Transport>(
    rng: &mut Rng,
    extra: &mut Rng,
    m: &mut Machine<T>,
    destroyed: &mut BTreeSet<FileHash>,
    exempt: &mut BTreeSet<Exempt>,
    link: &mut Option<Linked>,
) {
    // The content at `p`, read straight from memory: a locked file is still overwritten or deleted by its user.
    // It counts as destroyed only once the user's write or delete went through.
    let old = |m: &Machine<T>, p: &RelPath| m.fs().files().get(p).map(|d| FileHash::from_bytes(*blake3::hash(d).as_bytes()));
    let dirs: Vec<RelPath> = m.fs().dirs();
    // A live document's file (`*.kitdemo`, the network simulation's Craft saves) is only ever saved by its app: the
    // user doesn't edit, delete or rename it outside it.
    let files: Vec<RelPath> = files_of(m).into_iter().filter(|f| !f.file_name().is_some_and(|n| n.as_str().ends_with(".kitdemo"))).collect();
    match rng.below(11) {
        0 | 1 => {
            let dir = if rng.below(3) == 0 { RelPath::root() } else { rel(rng.pick(DIRS).copied().unwrap_or("x")) };
            let name = rng.pick(NAMES).copied().unwrap_or("a.txt");
            let target = rel(&format!("{dir}/{name}"));
            let data = rng.bytes();
            // Saving onto a link would write through it, to the other drive: not something this folder sees.
            if !matches!(m.fs().stat(&target), Ok(Some(FsKind::Dir { .. } | FsKind::Other))) {
                let before = old(m, &target);
                if m.fs_mut().write(&target, &data, false, 0).is_ok() {
                    destroyed.extend(before);
                }
            }
        }
        2 | 3 => {
            if let Some(f) = rng.pick(&files).cloned() {
                let data = rng.bytes();
                let before = old(m, &f);
                if m.fs_mut().write(&f, &data, false, 0).is_ok() {
                    destroyed.extend(before);
                }
            }
        }
        4 => {
            if let Some(f) = rng.pick(&files).cloned() {
                let before = old(m, &f);
                if m.fs_mut().remove_file(&f).is_ok() {
                    destroyed.extend(before);
                }
            }
        }
        5 => {
            if let Some(f) = rng.pick(&files).cloned() {
                let dir = if rng.below(2) == 0 { RelPath::root() } else { rel(rng.pick(DIRS).copied().unwrap_or("y")) };
                let name = rng.pick(NAMES).copied().unwrap_or("b.txt");
                // A rename never replaces anything (it fails when the target exists), so it destroys nothing.
                let to = rel(&format!("{dir}/{name}"));
                if m.fs_mut().rename(&f, &to).is_ok() {
                    follow_rename(exempt, &f, &to);
                }
            }
        }
        6 => {
            let dir = rel(rng.pick(DIRS).copied().unwrap_or("x"));
            let _ = m.fs_mut().mkdir(&dir);
        }
        7 => {
            if let Some(d) = rng.pick(&dirs).cloned() {
                let _ = m.fs_mut().remove_dir(&d);
            }
        }
        8 => {
            // Another program holds a file locked against reading, or open so it can't be removed, renamed or
            // replaced; or a folder can't be listed (or any of that ends).
            match rng.below(4) {
                0 | 1 => {
                    if let Some(f) = rng.pick(&files).cloned() {
                        if rng.below(2) == 0 {
                            m.fs_mut().lock(&f);
                        } else {
                            m.fs_mut().unlock(&f);
                        }
                    }
                }
                2 => {
                    let mut paths = files.clone();
                    paths.extend(dirs.iter().cloned());
                    if let Some(p) = rng.pick(&paths).cloned() {
                        if tracing() {
                            eprintln!("TRACE hold {p}");
                        }
                        m.fs_mut().hold(&p);
                    }
                }
                _ => {
                    let mut paths = files.clone();
                    paths.extend(dirs.iter().cloned());
                    if let Some(p) = rng.pick(&paths).cloned() {
                        m.fs_mut().release(&p);
                    }
                }
            }
        }
        10 => {
            // A tracked file or folder is replaced with a link, or the link is replaced with what it held again.
            match link.take() {
                Some(linked) if rng.below(2) == 0 => {
                    // Now and then a folder put back is moved, with a link made in it first, before any pass.
                    if linked.was_dir && extra.below(2) == 0 {
                        match put_back_and_move(extra, m, &linked, exempt) {
                            Ok(new) => *link = new,
                            Err(()) => *link = Some(linked),
                        }
                    } else if !restore_link(m, &linked) {
                        *link = Some(linked);
                    }
                }
                Some(linked) => *link = Some(linked),
                None => *link = make_link(rng, m),
            }
        }
        _ => {
            if let (Some(d), Some(to)) = (rng.pick(&dirs).cloned(), rng.pick(DIRS).copied()) {
                let to = rel(to);
                if m.fs_mut().rename(&d, &to).is_ok() {
                    follow_rename(exempt, &d, &to);
                }
            }
        }
    }
}

/// What the checks after a pass need from before it.
pub struct Before {
    files: Vec<RelPath>,
    locked: BTreeSet<RelPath>,
    pub link_seen: bool,
    changing: BTreeSet<RelPath>,
    /// The local IDs of the files being written.
    changing_ids: BTreeSet<u64>,
    /// The changed files an app is writing, with their content: the pass leaves each where it is, as it is. A file
    /// that only moved here and still holds content the base recorded isn't edited (the engine decides by content),
    /// so a server version may replace it as usual.
    writing: Vec<(RelPath, Vec<u8>)>,
    tracked: BTreeMap<RelPath, NodeId>,
    /// Tracked paths with nothing on disk before the pass (a folder the user renamed away, say).
    gone: BTreeSet<RelPath>,
}

pub fn before_pass<T: Transport>(m: &Machine<T>) -> Before {
    let changing = m.fs().changing().clone();
    let files_before = m.fs().files();
    let base_hashes: BTreeSet<FileHash> = m
        .state()
        .base
        .values()
        .filter_map(|e| match e.kind {
            BaseKind::File { hash, .. } => Some(hash),
            BaseKind::Dir { .. } => None,
        })
        .collect();
    let writing: Vec<(RelPath, Vec<u8>)> = differing(m)
        .into_iter()
        .filter(|p| changing.contains(p))
        .filter_map(|p| Some((p.clone(), files_before.get(&p)?.clone())))
        .filter(|(_, d)| !base_hashes.contains(&FileHash::from_bytes(*blake3::hash(d).as_bytes())))
        .collect();
    let tracked = m.state().base_index();
    let gone = tracked.keys().filter(|p| matches!(m.fs().stat(p), Ok(None))).cloned().collect();
    let changing_ids = changing.iter().filter_map(|p| local_id(m, p)).collect();
    Before { files: files_of(m), locked: unreadable(m), link_seen: find_link(m).is_some(), changing, changing_ids, writing, tracked, gone }
}

/// The content hash of `data`.
fn hash_of(data: &[u8]) -> FileHash {
    FileHash::from_bytes(*blake3::hash(data).as_bytes())
}

/// The local ID of the file at `p`, if it is one.
fn local_id<T: Transport>(m: &Machine<T>, p: &RelPath) -> Option<u64> {
    match m.fs().stat(p) {
        Ok(Some(FsKind::File(meta))) => meta.id,
        _ => None,
    }
}

/// The run's bookkeeping that the checks after a pass read and keep up to date (see `tests/simulation.rs`).
pub struct Books<'a> {
    pub links: &'a mut [Option<Linked>],
    pub last_pending: &'a mut [Vec<NodeId>],
    pub hold_dirs: &'a mut [Option<RelPath>],
    /// A folder the machine replaced with a link that a finished pass of its saw, for another machine to move a
    /// file out of (once per link).
    pub link_dirs: &'a mut [Option<RelPath>],
    pub recovering: &'a mut [u32],
    pub exempt: &'a mut BTreeSet<Exempt>,
    pub reuploaded: &'a mut BTreeSet<FileHash>,
    pub at_crash: &'a [BTreeSet<FileHash>],
}

/// The checks after machine `i`'s pass (`crash`: the machine restarts from its saved state after it), and the
/// bookkeeping they keep.
#[allow(clippy::too_many_arguments)]
pub fn check_pass<T: Transport>(
    seed: u64,
    step: usize,
    i: usize,
    m: &Machine<T>,
    r: &Result<Report, SyncError>,
    b: &Before,
    crash: bool,
    books: &mut Books,
) {
    // A file still being written is never replaced, nor moved aside as a conflict copy itself, by a pass: its
    // content is still on the disk, and not as one of the pass's conflict copies. A server move may still
    // take it (or a folder above it) elsewhere, since only its edit waits, and a folder moved aside takes
    // what is inside with it (the design's narrower rule). A pass that failed partway has no report: a file
    // that appeared during it under a name that adds this machine's conflict mark to the file's own name
    // counts as its copy of that file (a pass's copies always get such new names; a server move may bring
    // another machine's conflict name, as in seed 120, and a file that already had a conflict name keeps it
    // when a folder above it moves, as in seed 12226: both are allowed).
    {
        let files = m.fs().files();
        let made: Vec<RelPath> = match r {
            Ok(_) => Vec::new(),
            Err(_) => new_copies(&b.files, m)
                .into_iter()
                .map(|(p, _)| p)
                .filter(|p| p.file_name().is_some_and(|n| n.as_str().contains(&format!("(conflict, m{i},"))))
                .collect(),
        };
        for (p, data) in &b.writing {
            let copies: Vec<RelPath> = match r {
                Ok(report) => report.conflict_copies.clone(),
                Err(_) => made.iter().filter(|q| q.file_name() != p.file_name()).cloned().collect(),
            };
            let kept = files.iter().any(|(q, d)| d == data && !copies.contains(q));
            assert!(kept, "seed {seed} step {step}: m{i} replaced or moved aside {p} while it was still being written");
        }
    }
    // Tracked folders this pass made again (finished or not: a failed pass leaves them too).
    if let Some(Some(linked)) = books.links.get_mut(i) {
        linked.remade.extend(b.gone.iter().filter(|p| matches!(m.fs().stat(p), Ok(Some(FsKind::Dir { .. })))).cloned());
    }
    if let Ok(report) = r {
        // A file an app is still writing never goes up, at its old path or where the pass moved it (marks
        // follow the file, a leftover `.cc-aside` file put back by recovery among them). The file is told by its
        // local ID and by its content, not its path: a pass may move it aside and bring another file to its path,
        // which may go up (seed 7887 of the network simulation: a server move of a folder onto the name of a local
        // folder holding it). By content: no uploaded file holds what a file being written held before the pass,
        // and no tracked version has that content, wherever the file went in the pass (an edit's content is
        // random, so it never comes from anywhere else). One exception: a file the pass downloaded. The server
        // had that content already (seed 710: an earlier commit applied but its answer was lost, the user renamed
        // the file and an app went on writing it; the pass downloaded the committed file at the old name).
        let files = m.fs().files();
        let writing: BTreeSet<FileHash> = b.writing.iter().map(|(_, d)| hash_of(d)).collect();
        for p in &report.uploaded {
            assert!(
                !local_id(m, p).is_some_and(|id| b.changing_ids.contains(&id)) && !m.fs().changing().contains(p),
                "seed {seed} step {step}: m{i} uploaded {p} while it was still being written"
            );
            assert!(
                !files.get(p).is_some_and(|d| writing.contains(&hash_of(d))),
                "seed {seed} step {step}: m{i} uploaded {p} with the content a file still being written held"
            );
        }
        for (id, entry) in &m.state().base {
            if let BaseKind::File { hash, .. } = entry.kind {
                let path = m.state().base_path(*id);
                assert!(
                    !writing.contains(&hash) || path.as_ref().is_some_and(|p| report.downloaded.contains(p)),
                    "seed {seed} step {step}: m{i} uploaded the content of a file still being written (now {path:?})"
                );
            }
        }
        if let Some(listed) = books.last_pending.get_mut(i) {
            listed.clone_from(&report.pending_nodes);
        }
        if let Some(h) = books.hold_dirs.get_mut(i) {
            *h = report.held_deletes.first().and_then(|(_, links)| links.first()).and_then(|link| link.parent()).filter(|p| !p.is_root());
        }
        // Nothing tracked behind this machine's link is deleted on the server by it. Unless the user moved
        // the link (with a folder holding it) into a folder that can't be listed: the engine can't see it,
        // and a tracked folder moved there looks deleted (a known gap, see the design's engine rules). Or
        // unless the user renamed a folder above the link while the server put something new in it: the
        // engine makes the folder again at its old place for that (a folder it tracked and that was missing
        // from disk before a pass, this one or an earlier one that may have failed partway; it is the tracked
        // folder there, path first), so the renamed one is a new folder to it and the link in it isn't the
        // tracked file (also a known gap: the file goes up as a new one once it is real again, and history
        // keeps the old one).
        // Deletes are looked up by node, not by path: the path reported is where the node was when its delete went,
        // which is where the pull let it follow a server move (another machine moved it while it was missing here).
        if let Some(Some(linked)) = books.links.get(i)
            && b.link_seen
        {
            assert_eq!(report.removed_remotely.len(), report.removed_nodes.len(), "seed {seed} step {step}: a delete without its node");
            for (gone, id) in report.removed_remotely.iter().zip(&report.removed_nodes) {
                // The remade-folder gap is about where the node was here before the pass.
                let was = b.tracked.iter().find(|(_, n)| *n == id).map_or(gone, |(p, _)| p);
                let remade_above = linked.remade.iter().any(|d| was.starts_with(d) && was != d);
                assert!(
                    remade_above || !linked.nodes.contains(id),
                    "seed {seed} step {step}: m{i} deleted {gone} on the server, which it tracks behind a link"
                );
            }
        }
        // A node that is a real file or folder in its place again (the server's edit downloaded after the
        // link moved away with its folder, say) no longer stands behind the link.
        if let Some(Some(linked)) = books.links.get_mut(i) {
            let st = m.state();
            linked.nodes.retain(|id| !st.base_path(*id).is_some_and(|p| on_disk_here(m, id, &p)));
            // A folder link this pass saw: another machine may now move a file out of that folder (fix round 6).
            if linked.was_dir
                && !linked.moved_out
                && b.link_seen
                && let Some(d) = books.link_dirs.get_mut(i)
            {
                linked.moved_out = true;
                *d = find_link(m);
            }
        }
    }
    let was_recovering = books.recovering.get(i).is_some_and(|left| *left > 0);
    for (p, hash) in new_copies(&b.files, m) {
        if was_recovering || original_of(&p).is_some_and(|o| b.locked.contains(&o)) {
            books.exempt.insert((i, p, hash));
        }
    }
    if let Some(left) = books.recovering.get_mut(i).filter(|left| **left > 0) {
        if let Ok(report) = r {
            let left_behind = books.at_crash.get(i).cloned().unwrap_or_default();
            books.reuploaded.extend(new_uploads(report, &b.tracked, m).into_iter().filter(|h| left_behind.contains(h)));
        }
        *left = match r {
            _ if crash => *left,
            // What the crash left behind a link the user made meanwhile (a file the crashed pass downloaded
            // into a folder that is a link now) is out of the engine's sight: it redoes that bookkeeping once
            // the link is real again, so a pass with the link in place doesn't count.
            Ok(_) if b.link_seen => *left,
            Ok(report) if settled(report) => 0,
            Ok(_) => *left - 1,
            // A pass that failed partway hasn't redone the bookkeeping yet; it doesn't count.
            Err(_) => *left,
        };
    }
}

/// After a finished pass every node the machine tracks has a place in its folder. A node skipped for a
/// filesystem error may wait one more pass for it (its folder was moved aside, say, and it couldn't be
/// looked up), so passes that skipped one aren't checked. A node whose place on the server is behind a link
/// here waits for as long as the link is there, so it may lose its place meanwhile (its old folder was
/// settled for a newcomer, say); it is looked up once the link is gone. Its place is behind the link when
/// its server path is, or when a folder above it on the server is one this machine tracks at a path that
/// is a link here (or below one): the server may have moved that folder elsewhere meanwhile, but this
/// machine can't apply the move while its folder is a link (seed 63918).
pub fn check_places<T: Transport>(seed: u64, step: usize, i: usize, m: &Machine<T>, r: &Result<Report, SyncError>) {
    if r.as_ref().is_ok_and(|r| !r.skipped.iter().any(|(_, why)| why.ends_with("tried again next pass"))) {
        let st = m.state();
        let parent_behind_link = |id: NodeId| {
            let mut cur = st.remote.get(&id).and_then(|n| n.parent);
            let mut hops = 0;
            while let Some(a) = cur
                && hops < 64
            {
                if st.base_path(a).is_some_and(|p| behind_link(m, &p)) {
                    return true;
                }
                cur = st.remote.get(&a).and_then(|n| n.parent);
                hops += 1;
            }
            false
        };
        for (id, b) in &st.base {
            assert!(
                st.base_path(*id).is_some() || st.remote_path(*id).is_some_and(|p| behind_link(m, &p)) || parent_behind_link(*id),
                "seed {seed} step {step}: m{i} tracks {id} ({:?}) under a folder it no longer tracks",
                b.name
            );
        }
    }
}

/// A pass of machine `j` outside the main stream (another machine's act around a link or hold), without faults,
/// with the same checks as every other pass; its state is saved after it.
pub async fn side_pass<T: Transport>(
    seed: u64,
    step: usize,
    j: usize,
    mj: &mut Machine<T>,
    seen: &mut BTreeSet<FileHash>,
    books: &mut Books<'_>,
    saved: &[std::sync::Arc<Mutex<State>>],
) {
    seen.extend(hashes_of(mj));
    let b = before_pass(mj);
    let r = mj.sync_once().await;
    if tracing() {
        eprintln!("TRACE step {step} m{j} sync: {r:?}\n  files {}\n  dirs {:?}", listing(mj), mj.fs().dirs());
    }
    check_pass(seed, step, j, mj, &r, &b, false, books);
    if let Some(s) = saved.get(j) {
        *s.lock().unwrap() = mj.state().clone();
    }
    check_places(seed, step, j, mj, &r);
}

/// What a run keeps for the checks at its end (see [`settle_and_check`]).
pub struct Outcome<'a> {
    /// Each machine's link, if it has one: put back before settling.
    pub links: &'a mut [Option<Linked>],
    /// Every file content that was on a disk when a pass started.
    pub seen: &'a mut BTreeSet<FileHash>,
    /// Contents a user overwrote or deleted.
    pub destroyed: &'a BTreeSet<FileHash>,
    pub recovering: &'a mut [u32],
    pub exempt: &'a mut BTreeSet<Exempt>,
    pub reuploaded: &'a mut BTreeSet<FileHash>,
    pub at_crash: &'a [BTreeSet<FileHash>],
}

/// Everyone syncs until a whole round changes nothing (deletes approved, files let go and links replaced with what
/// they held, as eventually they are), then the end checks: convergence, nothing stuck, no needless conflict copy,
/// nothing the engine saw lost, nothing live deleted (see `tests/simulation.rs`).
pub async fn settle_and_check<T: Transport>(seed: u64, server: &Server, ms: &mut [Machine<T>], out: Outcome<'_>) {
    let Outcome { links, seen, destroyed, recovering, exempt, reuploaded, at_crash } = out;
    for (i, m) in ms.iter_mut().enumerate() {
        m.fs_mut().unlock_all();
        if let Some(linked) = links.get_mut(i).and_then(Option::take) {
            assert!(restore_link(m, &linked), "seed {seed}: m{i}'s link is gone");
        }
        seen.extend(hashes_of(m));
    }
    let mut quiet = false;
    let mut skipped: Vec<BTreeSet<RelPath>> = vec![BTreeSet::new(); ms.len()];
    let mut last_skips: BTreeMap<usize, Vec<(String, String)>> = BTreeMap::new();
    for _ in 0..40 {
        let mut changed = false;
        for (i, m) in ms.iter_mut().enumerate() {
            m.approve_deletes();
            let before = files_of(m);
            let tracked_before = m.state().base_index();
            let report = m.sync_once().await.unwrap_or_else(|e| panic!("seed {seed}: {e}"));
            if let Some(left) = recovering.get_mut(i).filter(|left| **left > 0) {
                exempt.extend(new_copies(&before, m).into_iter().map(|(p, hash)| (i, p, hash)));
                let left_behind = at_crash.get(i).cloned().unwrap_or_default();
                reuploaded.extend(new_uploads(&report, &tracked_before, m).into_iter().filter(|h| left_behind.contains(h)));
                *left = if settled(&report) { 0 } else { *left - 1 };
            }
            if tracing() {
                eprintln!("TRACE settle m{i} sync: {report:?}\n  files {}\n  dirs {:?}", listing(m), m.fs().dirs());
            }
            changed |= report.changed() || !report.pending_deletes.is_empty() || !report.pending_local_deletes.is_empty();
            // Two Craft saves waiting to learn which is newer are asked about again next pass: not settled yet.
            changed |= report
                .skipped
                .iter()
                .any(|(_, why)| why.starts_with(craft_cloud_sync::SAVE_ORDER_WAITS) || why.starts_with(craft_cloud_sync::SAVE_RECORD_WAITS));
            if let Some(s) = skipped.get_mut(i) {
                *s = report.skipped.iter().filter(|(_, why)| is_case_clash(why)).map(|(p, _)| rel(p)).collect();
            }
            last_skips.insert(i, report.skipped.clone());
        }
        if !changed {
            quiet = true;
            break;
        }
    }
    assert!(quiet, "seed {seed}: the machines never settled");

    // The only skip that may remain once everything settled (no locks, deletes approved) is a name that
    // clashes by case with a synced one; it legitimately stays local. Anything else is a stuck file.
    for (i, skips) in &last_skips {
        for (path, reason) in skips {
            assert!(is_case_clash(reason), "seed {seed}: machine {i} still skips {path} after settling: {reason}");
        }
    }

    // Case twins the engine refused to sync stay local (no other skip is left, checked above).
    let synced = |i: usize, m: &Machine<T>| -> BTreeMap<RelPath, Vec<u8>> {
        let skip = skipped.get(i).cloned().unwrap_or_default();
        m.fs().files().into_iter().filter(|(p, _)| !skip.iter().any(|s| p.starts_with(s))).collect()
    };
    let reference = ms.first().map(|m| synced(0, m)).unwrap_or_default();
    let reference_dirs: Vec<RelPath> = ms.first().map(|m| m.fs().dirs()).unwrap_or_default();
    for (i, m) in ms.iter().enumerate() {
        let mine = synced(i, m);
        assert_eq!(mine.keys().collect::<Vec<_>>(), reference.keys().collect::<Vec<_>>(), "seed {seed}: machine {i} has different files");
        assert!(mine == reference, "seed {seed}: machine {i} has different contents");
        let skip = skipped.get(i).cloned().unwrap_or_default();
        let dirs: Vec<RelPath> = m.fs().dirs().into_iter().filter(|d| !skip.iter().any(|s| d.starts_with(s))).collect();
        let ref_dirs: Vec<RelPath> =
            reference_dirs.iter().filter(|d| !skipped.first().is_some_and(|s0| s0.iter().any(|s| d.starts_with(s)))).cloned().collect();
        assert_eq!(dirs, ref_dirs, "seed {seed}: machine {i} has different folders");
    }
    // A conflict copy only keeps content that differs from what is at its original name: one holding the same
    // content is a needless copy (an unchanged file the engine mistook for an edit). Random contents never
    // repeat by chance, and a rename never replaces a file, so the same content twice comes from the engine.
    // Names with a case twin are left out: a tracked file moved aside for an incoming case twin is a legitimate
    // copy of content another machine may still hold under the old name.
    for (i, m) in ms.iter().enumerate() {
        let all = m.fs().files();
        for (p, data) in &all {
            let (Some(name), Some(original)) = (p.file_name().map(|n| n.as_str()), original_of(p)) else { continue };
            let hash = FileHash::from_bytes(*blake3::hash(data).as_bytes());
            // An exempt copy (its device name is in the copy's name) at this very path, still holding the content
            // it was made with.
            if (0..ms.len()).any(|j| name.contains(&format!("(conflict, m{j},")) && exempt.contains(&(j, p.clone(), hash))) {
                continue;
            }
            if reuploaded.contains(&hash) {
                continue;
            }
            if original.file_name().is_some_and(|o| NAMES.iter().filter(|n| n.eq_ignore_ascii_case(o.as_str())).count() > 1) {
                continue;
            }
            assert!(all.get(&original) != Some(data), "seed {seed}: machine {i} has a conflict copy {p} of unchanged {original}");
        }
    }
    let mut on_disk: BTreeSet<FileHash> = BTreeSet::new();
    for m in ms.iter() {
        on_disk.extend(hashes_of(m));
    }
    let mut kept: BTreeSet<FileHash> = server.history_hashes().await;
    kept.extend(on_disk.iter().copied());
    let lost: Vec<&FileHash> = seen.difference(&kept).filter(|h| !destroyed.contains(*h)).collect();
    assert!(lost.is_empty(), "seed {seed}: file versions the engine saw were lost: {lost:?}");
    // Stricter: the server's history keeps every uploaded version, so the check above can't see an engine
    // that uploads an edit and then lets a remote delete remove it everywhere. Content no user overwrote or
    // deleted must still be a file on some machine (conflict copies and case twins are files too).
    let gone: Vec<&FileHash> = seen.difference(&on_disk).filter(|h| !destroyed.contains(*h)).collect();
    assert!(gone.is_empty(), "seed {seed}: file contents no user overwrote or deleted are on no machine: {gone:?}");
}

/// The text a panicking seed left.
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(text) => *text,
        Err(payload) => payload.downcast::<&str>().map_or_else(|_| "a seed panicked".to_owned(), |text| (*text).to_owned()),
    }
}

/// Runs `run` for the seeds the environment asks for: `SIM_SEED=<n>` alone, otherwise `SIM_SEEDS` seeds (default
/// `default_seeds`) from `SIM_FIRST` (default 0), in parallel on `SIM_JOBS` threads (default: the machine's
/// parallelism, at most 8) that take seeds from a shared counter. Each seed is independent (its own server and
/// machines) and deterministic, so the order doesn't matter. On failure the lowest failing seed is reported with its
/// own message; `SIM_SEED=<n>` replays it alone. Once a seed fails, no higher seed is started.
pub fn for_each_seed(default_seeds: u64, run: impl Fn(u64) + Sync) {
    if let Ok(seed) = std::env::var("SIM_SEED") {
        run(seed.parse().unwrap());
        return;
    }
    let seeds: u64 = std::env::var("SIM_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(default_seeds);
    let first: u64 = std::env::var("SIM_FIRST").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    let jobs: usize = std::env::var("SIM_JOBS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get().min(8)))
        .max(1);
    let end = first + seeds;
    let next = AtomicU64::new(first);
    let lowest_failure = AtomicU64::new(u64::MAX);
    let failures: Mutex<Vec<(u64, String)>> = Mutex::default();
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| {
                loop {
                    let seed = next.fetch_add(1, Ordering::Relaxed);
                    if seed >= end || seed > lowest_failure.load(Ordering::Relaxed) {
                        return;
                    }
                    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(seed))) {
                        lowest_failure.fetch_min(seed, Ordering::Relaxed);
                        failures.lock().unwrap().push((seed, panic_message(payload)));
                    }
                }
            });
        }
    });
    let mut failures = std::mem::take(&mut *failures.lock().unwrap());
    failures.sort();
    if let Some((_, message)) = failures.first() {
        let others: Vec<String> = failures.iter().skip(1).map(|(seed, _)| seed.to_string()).collect();
        let also = if others.is_empty() { String::new() } else { format!("\n(seeds {} failed too)", others.join(", ")) };
        panic!("{message}{also}");
    }
}
