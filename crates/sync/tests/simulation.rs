//! Deterministic simulation: three machines share one account and do random things to their folders
//! (create, edit, delete, rename, move files and folders, hold files locked or open, make folders unlistable,
//! replace a tracked file or folder with a link and later put it back, or put a folder back, make a link on a file
//! in it and move it before any pass, keep a file "still being written" for a while), sync at random moments,
//! sometimes with
//! the connection failing mid-pass, sometimes approving held-back deletes (all of them, or some of the ones the
//! last pass listed), and sometimes crash during a pass and restart from the state saved last (after the pass before,
//! or at a checkpoint of the crashed pass).
//! Then every link is put back, everyone syncs until nothing changes, and we check:
//!
//! 1. **Convergence**: every machine ends with exactly the same files and folders.
//! 2. **Nothing the engine saw is lost**: every file content that was on a disk when a pass started is still
//!    on some disk or in the server's version history, unless a user overwrote or deleted it themselves.
//! 3. **Nothing live is deleted**: stricter, every such content is still a file on some machine's disk (history
//!    alone doesn't count), so a remote delete never wins over an edit.
//! 4. **Nothing is stuck**: after settling, the only files left out of sync are case twins of a synced name.
//!
//! It also checks after every finished pass that each node a machine tracks still has a place in its folder, and
//! that a machine never deletes on the server a node it tracked behind a link it has, or uploads a file that is
//! still being written. After every pass, finished or not, a changed file that was still being written when it
//! started is still on the disk with the same content, and not as one of the pass's conflict copies.
//!
//! Each seed is reproducible: `SIM_SEED=17 cargo test -p craft-cloud-sync --test simulation`. By default 40
//! seeds run; `SIM_SEEDS=1000 SIM_FIRST=2000` runs seeds 2000 to 2999 (use `--release`). Seeds run in parallel on
//! `SIM_JOBS` workers (default: the machine's parallelism, at most 8). `SIM_TRACE=1` prints every pass's report
//! and each machine's folder after every step.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use common::{Machine, Server, machine_config, restart, save_checkpoints};
use craft_cloud_proto::{FileHash, Name, NodeId};
use craft_cloud_sync::{BaseKind, Fs, FsKind, MemFs, RelPath, Report, State, SyncError, rel};

/// xorshift64*: tiny, deterministic, good enough to pick actions.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        if items.is_empty() { None } else { items.get(self.below(items.len())) }
    }

    fn bytes(&mut self) -> Vec<u8> {
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

const NAMES: &[&str] = &["a.txt", "b.txt", "A.TXT", "c.psd", "notes"];
const DIRS: &[&str] = &["x", "y", "x/z", "y/w"];

fn files_of(m: &Machine) -> Vec<RelPath> {
    m.fs().files().into_keys().collect()
}

/// Every file with the start of its content hash, for `SIM_TRACE`.
fn listing(m: &Machine) -> String {
    m.fs().files().iter().map(|(p, d)| format!("{p}={}", blake3::hash(d).to_hex().get(..6).unwrap_or(""))).collect::<Vec<_>>().join(" ")
}

fn hashes_of(m: &Machine) -> BTreeSet<FileHash> {
    m.fs().files().values().map(|d| FileHash::from_bytes(*blake3::hash(d).as_bytes())).collect()
}

/// Whether a pass left nothing for later: no refused operation, no node skipped for a filesystem error. A machine
/// recovering from a crash has redone what the crash lost once it finishes such a pass.
fn settled(report: &Report) -> bool {
    report.refused.is_empty() && !report.skipped.iter().any(|(_, why)| why.ends_with("tried again next pass"))
}

/// How many finished passes after a restart a machine may still be redoing what the crash lost (passes that fail
/// partway don't count: they haven't redone it yet). The first finished pass applies the server's tree and redoes
/// the lost bookkeeping; a refused operation is resubmitted on the next pass, and a node skipped for a filesystem
/// error (a held file) is retried on the one after, so three finished passes cover a crash's own consequences.
/// Copies made later are the engine's ordinary decisions and are checked.
const RECOVERY_PASSES: u32 = 3;

/// The contents of the files a pass uploaded as new (at paths the machine didn't track before the pass).
fn new_uploads(report: &Report, tracked_before: &BTreeMap<RelPath, craft_cloud_proto::NodeId>, m: &Machine) -> Vec<FileHash> {
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
fn new_copies(before: &[RelPath], m: &Machine) -> Vec<(RelPath, FileHash)> {
    m.fs()
        .files()
        .into_iter()
        .filter(|(p, _)| !before.contains(p) && p.file_name().is_some_and(|n| n.as_str().contains(" (conflict, ")))
        .map(|(p, d)| (p, FileHash::from_bytes(*blake3::hash(&d).as_bytes())))
        .collect()
}

/// The engine's skip reason for a name that clashes by case with one it syncs (`Engine::scan`).
fn is_case_clash(reason: &str) -> bool {
    reason.starts_with("the name clashes with ") && reason.ends_with(" when case is ignored")
}

/// A conflict copy exempt from the needless-copy check: the machine that made it, where it is, and the content it
/// was made with. An edit of the copy (or a later copy at the same path) is checked again.
type Exempt = (usize, RelPath, FileHash);

/// The path a conflict copy was made from: `a (conflict, m0, 2026-10-11 14-03).txt` → `a.txt` in the same folder
/// (the last conflict mark, for a copy of a copy). `None` for a name that isn't a conflict copy.
fn original_of(copy: &RelPath) -> Option<RelPath> {
    let (parent, name) = (copy.parent()?, copy.file_name()?.as_str());
    let start = name.rfind(" (conflict, ")?;
    let end = start + name.get(start..)?.find(')')? + 1;
    Some(rel(&format!("{parent}/{}{}", name.get(..start)?, name.get(end..)?)))
}

/// The files on `m` that differ from its base by size or time, or aren't tracked: the ones the engine asks about
/// (`Fs::still_changing`) before it uploads them or a server change moves them aside.
fn differing(m: &Machine) -> Vec<RelPath> {
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
fn unreadable(m: &Machine) -> BTreeSet<RelPath> {
    files_of(m).into_iter().filter(|p| m.fs().read(p).is_err()).collect()
}

/// After a user's rename of `from` to `to`, exempt copies at or below `from` are found at `to` (on every machine,
/// once it syncs), with the same content.
fn follow_rename(exempt: &mut BTreeSet<Exempt>, from: &RelPath, to: &RelPath) {
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
struct Linked {
    was_dir: bool,
    /// The folders and files it held, by their names below it (a file itself: one file with no names).
    dirs: Vec<Vec<Name>>,
    files: Vec<(Vec<Name>, Vec<u8>)>,
    /// The nodes the machine tracked there when the link was made: it must never delete them on the server
    /// while the link is there.
    nodes: BTreeSet<NodeId>,
    /// Tracked folders the engine made again while the link was there (missing before a pass, there after it,
    /// whether the pass finished or not; nobody acts during a pass): see the delete check in `run`.
    remade: BTreeSet<RelPath>,
    /// Whether another machine has been asked to move a file out of the folder this link replaced (see `run`).
    moved_out: bool,
}

/// Replaces a tracked file or folder on `m` with a link, keeping what it held.
fn make_link(rng: &mut Rng, m: &mut Machine) -> Option<Linked> {
    let index = m.state().base_index();
    let candidates: Vec<RelPath> =
        index.keys().filter(|p| !p.is_root() && matches!(m.fs().stat(p), Ok(Some(FsKind::File(_) | FsKind::Dir { .. })))).cloned().collect();
    let at = rng.pick(&candidates)?.clone();
    let below = |p: &RelPath| p.names().get(at.depth()..).map(<[Name]>::to_vec).unwrap_or_default();
    let was_dir = matches!(m.fs().stat(&at), Ok(Some(FsKind::Dir { .. })));
    let dirs = m.fs().dirs().into_iter().filter(|d| d.starts_with(&at) && *d != at).map(|d| below(&d)).collect();
    let files = m.fs().files().into_iter().filter(|(p, _)| p.starts_with(&at)).map(|(p, d)| (below(&p), d)).collect();
    let nodes = index.iter().filter(|(p, id)| p.starts_with(&at) && on_disk_here(m, id, p)).map(|(_, id)| *id).collect();
    m.fs_mut().make_link(&at).ok()?;
    if std::env::var("SIM_TRACE").is_ok() {
        eprintln!("TRACE link {at}");
    }
    Some(Linked { was_dir, dirs, files, nodes, remade: BTreeSet::new(), moved_out: false })
}

/// Whether the tracked node `id` is itself at its base path `p` on disk: the folder or file there has the node's
/// local ID. A node the user already moved (found by its ID elsewhere) or deleted isn't behind a link made at its
/// old place later, even if something new took that place first. (A file a user's save replaced at its path is
/// left out too: the check is a little weaker, never wrong.)
fn on_disk_here(m: &Machine, id: &NodeId, p: &RelPath) -> bool {
    match (m.state().base.get(id).map(|e| &e.kind), m.fs().stat(p)) {
        (Some(BaseKind::Dir { local_id }), Ok(Some(FsKind::Dir { id: here }))) => local_id.is_none() || *local_id == here,
        (Some(BaseKind::File { local_id, .. }), Ok(Some(FsKind::File(meta)))) => local_id.is_none() || *local_id == meta.id,
        _ => false,
    }
}

/// Whether `path` is a link on `m`, or below one.
fn behind_link(m: &Machine, path: &RelPath) -> bool {
    let mut prefix = RelPath::root();
    path.names().iter().any(|name| {
        prefix = prefix.join(name);
        matches!(m.fs().stat(&prefix), Ok(Some(FsKind::Other)))
    })
}

/// Where the machine's link is now: the only link or special file in its folder.
fn find_link(m: &Machine) -> Option<RelPath> {
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
fn restore_link(m: &mut Machine, linked: &Linked) -> bool {
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
    if std::env::var("SIM_TRACE").is_ok() {
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
fn put_back_and_move(extra: &mut Rng, m: &mut Machine, linked: &Linked, exempt: &mut BTreeSet<Exempt>) -> Result<Option<Linked>, ()> {
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
            if std::env::var("SIM_TRACE").is_ok() {
                eprintln!("TRACE hold {d} (during the put-back)");
            }
            m.fs_mut().hold(&d);
        }
    }
    if std::env::var("SIM_TRACE").is_ok() {
        eprintln!("TRACE put back {at}, link {file}, moved to {now}; behind it: {nodes:?}");
    }
    Ok(Some(Linked { was_dir: false, dirs: Vec::new(), files: vec![(Vec::new(), data)], nodes, remade: BTreeSet::new(), moved_out: false }))
}

/// One random user action on a machine's folder. Failures (target exists, parent missing) are ignored, like a
/// user's failed click. `exempt` follows the user's renames; `link` is the machine's link, if it has one. `extra`
/// is a stream of its own for the rarer compound actions, so the histories the others make stay as they were.
fn act(
    rng: &mut Rng,
    extra: &mut Rng,
    m: &mut Machine,
    destroyed: &mut BTreeSet<FileHash>,
    exempt: &mut BTreeSet<Exempt>,
    link: &mut Option<Linked>,
) {
    // The content at `p`, read straight from memory: a locked file is still overwritten or deleted by its user.
    // It counts as destroyed only once the user's write or delete went through.
    let old = |m: &Machine, p: &RelPath| m.fs().files().get(p).map(|d| FileHash::from_bytes(*blake3::hash(d).as_bytes()));
    let dirs: Vec<RelPath> = m.fs().dirs();
    let files = files_of(m);
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
                        if std::env::var("SIM_TRACE").is_ok() {
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
struct Before {
    files: Vec<RelPath>,
    locked: BTreeSet<RelPath>,
    link_seen: bool,
    changing: BTreeSet<RelPath>,
    /// The changed files an app is writing, with their content: the pass leaves each where it is, as it is. A file
    /// that only moved here and still holds content the base recorded isn't edited (the engine decides by content),
    /// so a server version may replace it as usual.
    writing: Vec<(RelPath, Vec<u8>)>,
    tracked: BTreeMap<RelPath, NodeId>,
    /// Tracked paths with nothing on disk before the pass (a folder the user renamed away, say).
    gone: BTreeSet<RelPath>,
}

fn before_pass(m: &Machine) -> Before {
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
    Before { files: files_of(m), locked: unreadable(m), link_seen: find_link(m).is_some(), changing, writing, tracked, gone }
}

/// The run's bookkeeping that the checks after a pass read and keep up to date (see `run`).
struct Books<'a> {
    links: &'a mut [Option<Linked>],
    last_pending: &'a mut [Vec<NodeId>],
    hold_dirs: &'a mut [Option<RelPath>],
    /// A folder the machine replaced with a link that a finished pass of its saw, for another machine to move a
    /// file out of (once per link).
    link_dirs: &'a mut [Option<RelPath>],
    recovering: &'a mut [u32],
    exempt: &'a mut BTreeSet<Exempt>,
    reuploaded: &'a mut BTreeSet<FileHash>,
    at_crash: &'a [BTreeSet<FileHash>],
}

/// The checks after machine `i`'s pass (`crash`: the machine restarts from its saved state after it), and the
/// bookkeeping they keep.
#[allow(clippy::too_many_arguments)]
fn check_pass(seed: u64, step: usize, i: usize, m: &Machine, r: &Result<Report, SyncError>, b: &Before, crash: bool, books: &mut Books) {
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
        // follow the file, a leftover `.cc-aside` file put back by recovery among them).
        for p in &report.uploaded {
            assert!(
                !b.changing.contains(p) && !m.fs().changing().contains(p),
                "seed {seed} step {step}: m{i} uploaded {p} while it was still being written"
            );
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
fn check_places(seed: u64, step: usize, i: usize, m: &Machine, r: &Result<Report, SyncError>) {
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
async fn side_pass(
    seed: u64,
    step: usize,
    j: usize,
    mj: &mut Machine,
    seen: &mut BTreeSet<FileHash>,
    books: &mut Books<'_>,
    saved: &[Arc<Mutex<State>>],
) {
    seen.extend(hashes_of(mj));
    let b = before_pass(mj);
    let r = mj.sync_once().await;
    if std::env::var("SIM_TRACE").is_ok() {
        eprintln!("TRACE step {step} m{j} sync: {r:?}\n  files {}\n  dirs {:?}", listing(mj), mj.fs().dirs());
    }
    check_pass(seed, step, j, mj, &r, &b, false, books);
    if let Some(s) = saved.get(j) {
        *s.lock().unwrap() = mj.state().clone();
    }
    check_places(seed, step, j, mj, &r);
}

async fn run(seed: u64, steps: usize) {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    // MemFs clocks tick once per operation, so the real-disk racy window would hash every file. Odd seeds
    // turn the window off to exercise the size-and-time fast path (and the replaced-file check).
    let racy = if seed.is_multiple_of(2) { 2_000 } else { 0 };
    let mut ms = Vec::new();
    // What each machine's agent saved last: after its last pass, or at a checkpoint during the pass it is in (a
    // crash restarts from it).
    let mut saved: Vec<Arc<Mutex<State>>> = Vec::new();
    // m0 lands downloads with one exchange (Linux, macOS), m1 in steps (Windows: the old file moved aside, the new
    // one put in place, the old one removed; a crash can fall between them), m2 one way or the other by seed. Files
    // a program holds open (`MemFs::hold`) can't be replaced either way: the download is kept for a later pass.
    for (name, swap) in [("m0", true), ("m1", false), ("m2", seed % 4 < 2)] {
        let fs = if swap { MemFs::new() } else { MemFs::new().without_swap() };
        let mut m = machine_config(&server, &ana, name, fs, 50, |c| c.racy_window_ms = racy).await;
        saved.push(save_checkpoints(&mut m));
        ms.push(m);
    }
    let mut rng = Rng::new(seed);
    let mut seen: BTreeSet<FileHash> = BTreeSet::new();
    let mut destroyed: BTreeSet<FileHash> = BTreeSet::new();
    // A machine is recovering from a crash for at most RECOVERY_PASSES passes after its restart, less if a pass
    // finishes with nothing left for later. Conflict copies it makes meanwhile may hold content that is already at
    // the original name: the crashed pass downloaded or moved files and lost the bookkeeping, and when the user
    // then locked, moved or edited them the engine can't tell its own work from the user's. A copy is the safe
    // answer (nothing is lost), so exactly those copies (by machine, path and the content they were made with) are
    // exempt from the needless-copy check below; every other check still holds, for every file. Keyed by content
    // too, so a later copy at the same path, or an edit of the copy, is checked again.
    //
    // Copies made while the original couldn't be read (another program held it locked) are exempt the same way:
    // a file that can't be read never counts as unchanged, so a new server version of it always keeps the local
    // file as a copy. That copy only holds the same content as the new version when the server got the same
    // content twice, which takes a crash (a recovering machine uploads a file whose new version the crashed pass
    // had already committed, as an Update), and it may land on any machine where the file is locked then.
    let mut recovering: Vec<u32> = vec![0; ms.len()];
    let mut exempt: BTreeSet<Exempt> = BTreeSet::new();
    // Contents a recovering machine uploaded as new files: files it no longer recognised as tracked (the crash lost
    // that bookkeeping) go up as new nodes, so the server holds the same content twice, and another machine may
    // later keep a copy of one in the other's way. Copies of exactly that content are exempt too, and only content
    // that was already on that machine's disk when it crashed (what the crash left behind, not anything new).
    let mut reuploaded: BTreeSet<FileHash> = BTreeSet::new();
    let mut at_crash: Vec<BTreeSet<FileHash>> = vec![BTreeSet::new(); ms.len()];
    // Each machine's link, if it has one, and the held-back deletes its last finished pass listed.
    let mut links: Vec<Option<Linked>> = (0..ms.len()).map(|_| None).collect();
    let mut last_pending: Vec<Vec<NodeId>> = vec![Vec::new(); ms.len()];

    // Apps writing files for a while ("still changing") come from a stream of their own, so the histories the other
    // actions make stay the same as before it was added.
    let mut writes = Rng::new(seed ^ 0x57AB_1E5E_ED00_0001);
    // The rarer compound actions (a folder put back and moved with a link made in it) likewise.
    let mut extra = Rng::new(seed ^ 0xC04B_0ED0_0000_0002);
    // Another machine renaming the folder a held delete's link is in comes from a fourth stream: the folder each
    // machine's last pass reported a hold's link in, until another machine has renamed it.
    let mut renames = Rng::new(seed ^ 0x5E5E_0000_0000_0003);
    let mut hold_dirs: Vec<Option<RelPath>> = vec![None; ms.len()];
    // Another machine moving a file out of a folder this machine replaced with a link comes from a fifth stream.
    let mut moves = Rng::new(seed ^ 0x6060_0000_0000_0004);
    let mut link_dirs: Vec<Option<RelPath>> = vec![None; ms.len()];
    for step in 0..steps {
        // As soon as a pass reports a hold, another machine syncs, renames the link's folder (if it has it) to a fresh
        // name and syncs again, so the holder's next pass pulls the rename (fix round 5). That machine's two passes
        // run without faults, with the same checks as every other pass, and its state is saved after each.
        if let Some((k, d)) = hold_dirs.iter_mut().enumerate().find_map(|(k, d)| d.take().map(|d| (k, d))) {
            let others: Vec<usize> = (0..ms.len()).filter(|j| *j != k).collect();
            if let Some(j) = renames.pick(&others).copied()
                && let Some(mj) = ms.get_mut(j)
            {
                for again in [false, true] {
                    if again && matches!(mj.fs().stat(&d), Ok(Some(FsKind::Dir { .. }))) {
                        let to = ["r5a", "r5b", "r5c"].into_iter().map(rel).find(|to| matches!(mj.fs().stat(to), Ok(None)));
                        if let Some(to) = to
                            && mj.fs_mut().rename(&d, &to).is_ok()
                        {
                            follow_rename(&mut exempt, &d, &to);
                            if std::env::var("SIM_TRACE").is_ok() {
                                eprintln!("TRACE step {step} m{j} renamed {d} to {to} (m{k}'s held link is in it)");
                            }
                        }
                    }
                    let mut books = Books {
                        links: &mut links,
                        last_pending: &mut last_pending,
                        hold_dirs: &mut hold_dirs,
                        link_dirs: &mut link_dirs,
                        recovering: &mut recovering,
                        exempt: &mut exempt,
                        reuploaded: &mut reuploaded,
                        at_crash: &at_crash,
                    };
                    side_pass(seed, step, j, mj, &mut seen, &mut books, &saved).await;
                }
            }
        }
        // Once a pass has seen a folder a machine replaced with a link, half the time another machine moves a file
        // out of that folder to the top and syncs, so the holder's next pass pulls a server move of a node it is
        // missing (review 6's P4: the folder is later put back and moved, with a link made in it). That pass runs
        // without faults, with the same checks as every other pass.
        if let Some((k, d)) = link_dirs.iter_mut().enumerate().find_map(|(k, d)| d.take().map(|d| (k, d)))
            && moves.below(2) == 0
        {
            let others: Vec<usize> = (0..ms.len()).filter(|j| *j != k).collect();
            if let Some(j) = moves.pick(&others).copied()
                && let Some(mj) = ms.get_mut(j)
            {
                let inside: Vec<RelPath> = mj.fs().files().into_keys().filter(|p| p.starts_with(&d) && *p != d).collect();
                let to = ["r6a.txt", "r6b.txt", "r6c.txt"].into_iter().map(rel).find(|to| matches!(mj.fs().stat(to), Ok(None)));
                if let (Some(f), Some(to)) = (moves.pick(&inside).cloned(), to)
                    && mj.fs_mut().rename(&f, &to).is_ok()
                {
                    follow_rename(&mut exempt, &f, &to);
                    if std::env::var("SIM_TRACE").is_ok() {
                        eprintln!("TRACE step {step} m{j} moved {f} to {to} (m{k}'s link replaced {d})");
                    }
                    let mut books = Books {
                        links: &mut links,
                        last_pending: &mut last_pending,
                        hold_dirs: &mut hold_dirs,
                        link_dirs: &mut link_dirs,
                        recovering: &mut recovering,
                        exempt: &mut exempt,
                        reuploaded: &mut reuploaded,
                        at_crash: &at_crash,
                    };
                    side_pass(seed, step, j, mj, &mut seen, &mut books, &saved).await;
                }
            }
        }
        let i = rng.below(ms.len());
        let Some(m) = ms.get_mut(i) else { continue };
        if writes.below(10) == 0
            && let Some(f) = writes.pick(&files_of(m)).cloned()
        {
            let on = !m.fs().changing().contains(&f);
            m.fs_mut().set_changing(&f, on);
            if std::env::var("SIM_TRACE").is_ok() {
                eprintln!("TRACE step {step} m{i} {f} {}", if on { "is being written" } else { "is written" });
            }
        }
        if rng.below(3) == 0 {
            seen.extend(hashes_of(m));
            // A crash: the agent dies during this pass, before it saves the state at its end. A quarter of the time the
            // pass is cut short at a random server call, a quarter of the time the machine dies after a few filesystem
            // changes (between two steps of the pass), a quarter of the time it dies right after a commit applied on
            // the server, before the answer arrives (between the commit and the checkpoint that records it), otherwise
            // the pass runs to the end. It restarts from the state saved last (at the pass's last checkpoint, or after
            // the pass before), with its folder and the server as the crashed pass left them.
            // `SIM_NO_CRASH=1` runs the same history with the crashed passes kept (no restart), to tell crash bugs apart.
            let crash_roll = rng.below(8) == 0;
            let crash = crash_roll && std::env::var("SIM_NO_CRASH").is_err();
            let how = if crash_roll { rng.below(4) } else { usize::MAX };
            if how == 3 {
                m.transport().faults.die_on_commit.store(1 + rng.below(2) as u64, Ordering::Relaxed);
            }
            let every = if how == 0 {
                2 + rng.below(12) as u64
            } else if rng.below(4) == 0 {
                2 + rng.below(6) as u64
            } else {
                0
            };
            m.transport().faults.every.store(every, Ordering::Relaxed);
            if how == 1 {
                let dead = m.transport().faults.dead.clone();
                let changes = rng.below(8) as u64;
                m.fs_mut().die_after(changes, dead);
            }
            // Held-back deletes are approved now and then: all of them, or some of the ones the last pass listed.
            match rng.below(4) {
                0 | 1 => m.approve_deletes(),
                2 => {
                    let listed = last_pending.get(i).cloned().unwrap_or_default();
                    let some: Vec<NodeId> = listed.into_iter().filter(|_| rng.below(2) == 0).collect();
                    m.approve_deletes_of(some);
                }
                _ => {}
            }
            // Now and then an app is writing a file this machine changed just as its pass runs (from the writes
            // stream), so the apply side's checks meet files still being written, not only the push's.
            if writes.below(3) == 0
                && let Some(f) = writes.pick(&differing(m)).cloned()
            {
                m.fs_mut().set_changing(&f, true);
                if std::env::var("SIM_TRACE").is_ok() {
                    eprintln!("TRACE step {step} m{i} {f} is being written (pass)");
                }
            }
            let b = before_pass(m);
            let r = m.sync_once().await;
            if std::env::var("SIM_TRACE").is_ok() {
                eprintln!("TRACE step {step} m{i} sync: {r:?}\n  files {}\n  dirs {:?}", listing(m), m.fs().dirs());
            }
            let mut books = Books {
                links: &mut links,
                last_pending: &mut last_pending,
                hold_dirs: &mut hold_dirs,
                link_dirs: &mut link_dirs,
                recovering: &mut recovering,
                exempt: &mut exempt,
                reuploaded: &mut reuploaded,
                at_crash: &at_crash,
            };
            check_pass(seed, step, i, m, &r, &b, crash, &mut books);
            // Without a restart (`SIM_NO_CRASH`), a machine that "died" right after a commit applied runs on with the
            // commit's answer lost: the engine doesn't know the commit applied, so it has the same bookkeeping to redo
            // as after a crash, and is recovering the same way.
            if !crash && how == 3 && m.transport().faults.dead.load(Ordering::Relaxed) {
                if let Some(left_behind) = at_crash.get_mut(i) {
                    *left_behind = hashes_of(m);
                }
                if let Some(left) = recovering.get_mut(i) {
                    *left = RECOVERY_PASSES;
                }
            }
            m.transport().faults.every.store(0, Ordering::Relaxed);
            m.transport().faults.die_on_commit.store(0, Ordering::Relaxed);
            m.fs_mut().revive();
            m.transport().faults.dead.store(false, Ordering::Relaxed);
            if crash {
                if let Some(left_behind) = at_crash.get_mut(i) {
                    *left_behind = hashes_of(m);
                }
                let state = saved.get(i).map(|s| s.lock().unwrap().clone()).unwrap_or_default();
                let mut fresh = restart(m, state, |c| c.racy_window_ms = racy);
                if let Some(s) = saved.get_mut(i) {
                    *s = save_checkpoints(&mut fresh);
                }
                *m = fresh;
                if let Some(left) = recovering.get_mut(i) {
                    *left = RECOVERY_PASSES;
                }
                if std::env::var("SIM_TRACE").is_ok() {
                    eprintln!("TRACE step {step} m{i} crashed and restarted from its last saved state");
                }
                continue;
            }
            // The agent saves the state after every pass, finished or not.
            if let Some(s) = saved.get(i) {
                *s.lock().unwrap() = m.state().clone();
            }
            check_places(seed, step, i, m, &r);
        } else {
            let mut no_link = None;
            let link = links.get_mut(i).unwrap_or(&mut no_link);
            act(&mut rng, &mut extra, m, &mut destroyed, &mut exempt, link);
            if std::env::var("SIM_TRACE").is_ok() {
                eprintln!("TRACE step {step} m{i} act\n  files {}\n  dirs {:?}", listing(m), m.fs().dirs());
            }
        }
    }

    // Everyone syncs until a whole round changes nothing (deletes approved, files let go and links replaced with
    // what they held, as eventually they are).
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
            if std::env::var("SIM_TRACE").is_ok() {
                eprintln!("TRACE settle m{i} sync: {report:?}\n  files {}\n  dirs {:?}", listing(m), m.fs().dirs());
            }
            changed |= report.changed() || !report.pending_deletes.is_empty() || !report.pending_local_deletes.is_empty();
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
    let synced = |i: usize, m: &Machine| -> BTreeMap<RelPath, Vec<u8>> {
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
    for m in &ms {
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

fn runtime(threads: usize) -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().worker_threads(threads).enable_all().build().unwrap()
}

/// The text a panicking seed left.
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(text) => *text,
        Err(payload) => payload.downcast::<&str>().map_or_else(|_| "a seed panicked".to_owned(), |text| (*text).to_owned()),
    }
}

/// Seeds run in parallel: `SIM_JOBS` workers (default: the machine's parallelism, at most 8) take seeds from a
/// shared counter. Each seed is independent (its own server and machines) and deterministic, so the order doesn't
/// matter. On failure the lowest failing seed is reported with its own message; `SIM_SEED=<n>` replays it alone.
/// Once a seed fails, no higher seed is started.
#[test]
fn random_histories_converge_without_losing_data() {
    if let Ok(seed) = std::env::var("SIM_SEED") {
        runtime(2).block_on(run(seed.parse().unwrap(), 120));
        return;
    }
    let seeds: u64 = std::env::var("SIM_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(40);
    let first: u64 = std::env::var("SIM_FIRST").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    let jobs: usize = std::env::var("SIM_JOBS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get().min(8)))
        .max(1);
    let end = first + seeds;
    let next = Arc::new(AtomicU64::new(first));
    let lowest_failure = Arc::new(AtomicU64::new(u64::MAX));
    let failures: Arc<Mutex<Vec<(u64, String)>>> = Arc::default();
    runtime(jobs).block_on(async {
        let workers: Vec<_> = (0..jobs)
            .map(|_| {
                let (next, lowest_failure, failures) = (next.clone(), lowest_failure.clone(), failures.clone());
                tokio::spawn(async move {
                    loop {
                        let seed = next.fetch_add(1, Ordering::Relaxed);
                        if seed >= end || seed > lowest_failure.load(Ordering::Relaxed) {
                            return;
                        }
                        if let Err(e) = tokio::spawn(run(seed, 120)).await {
                            let message = if e.is_panic() { panic_message(e.into_panic()) } else { format!("seed {seed}: {e}") };
                            lowest_failure.fetch_min(seed, Ordering::Relaxed);
                            failures.lock().unwrap().push((seed, message));
                        }
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.await.unwrap();
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
