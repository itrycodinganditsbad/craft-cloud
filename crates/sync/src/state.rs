//! What the engine remembers between passes: the server's tree as of a sequence number (the remote
//! mirror), and what this machine and the server last agreed on (the base).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use craft_cloud_proto::{Epoch, FileHash, FileState, ManifestId, Name, NodeId, NodeKind, NodeState, SpaceId, VersionId};
use serde::{Deserialize, Serialize};

use crate::path::RelPath;

/// The engine's memory. The caller persists it (the agent writes it to disk after every pass).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub space: Option<SpaceId>,
    pub root: Option<NodeId>,
    pub epoch: Option<Epoch>,
    /// The change-feed position the remote mirror is at.
    pub seq: u64,
    /// Live server nodes as of `seq`.
    pub remote: BTreeMap<NodeId, RemoteNode>,
    /// Nodes both sides agree on, with what the local copy looked like when they agreed. Read it as a map; change it
    /// through [`Base`]'s methods, which keep its indexes (by path, by parent, by local ID) up to date.
    pub base: Base,
    /// The base was forgotten for a new epoch and no pass has applied the server's tree since: passes
    /// reconcile (never replacing local files) until one does.
    #[serde(default)]
    pub reconcile_pending: bool,
    /// Files this machine asked the server to delete in a commit whose answer it never recorded (it crashed, or the
    /// connection broke, after sending it), with the version each delete expected. When the server's tree then
    /// lacks one, or has it at another version (another machine's edit brought it back after the delete), the delete
    /// went through and is this machine's own, not one from elsewhere: the push sent it because the file was missing
    /// here, so whatever is at its path now isn't that file. The node is let go and nothing local is removed or
    /// replaced for it (a node brought back arrives as a new one at its own place). Cleared once a pass has applied
    /// the server's changes.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sent_deletes: BTreeMap<NodeId, VersionId>,
    /// Server deletes held behind a link: tracked nodes missing here while a link or special file that appeared
    /// since the last finished push carries a name from the node's last local path, so it may be the node, moved with
    /// its folder (seed 112337's no-pass order; see the push). Each is held for as long as any of its links stands
    /// (where the hold last saw it, or anywhere under its name at a path the last finished push didn't see a link at:
    /// its folder moved) and the node isn't found here again, and is judged afresh once either changes. Saved before the pass
    /// commits anything, so a crash can't lose it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub held_deletes: BTreeMap<NodeId, HeldLink>,
    /// Every link and special file the last finished push saw, by path. A link at any other path appeared since, and
    /// only such a link can hold a delete (see `held_deletes`). Replaced only once a push has finished, in the
    /// checkpoint after its last commit, so a pass that fails or crashes leaves the links it saw new to the next one.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub known_links: BTreeSet<RelPath>,
    /// Tracked nodes missing here whose base the pull let follow a server move since the last finished push, and
    /// folders the pull moved here (the one found at the place may be a new folder that took the name), with the path
    /// this machine last had them at. A hold matches names on that path (another machine may have moved the node or a
    /// folder above it while this machine's user was moving its folder). Cleared by a finished push.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub left_from: BTreeMap<NodeId, RelPath>,
}

/// The links or special files holding a delete: every new link that carried a name from the node's path when the
/// hold was made (several may: the real one and unrelated ones with the same name), each where the hold last saw it.
/// The hold lasts while any of them stands. One that is gone follows by name to every path the last finished push
/// didn't see a link at (its folder renamed or moved here, put back under a new name, or moved aside by the engine);
/// renaming the link itself counts as removing it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "HeldLinkRecord")]
pub struct HeldLink {
    pub paths: BTreeSet<RelPath>,
}

impl HeldLink {
    pub fn at(path: RelPath) -> Self {
        Self { paths: BTreeSet::from([path]) }
    }

    /// The first of its links, by path (the one a report names).
    pub fn first(&self) -> Option<&RelPath> {
        self.paths.first()
    }
}

/// A saved hold: every candidate link's path (now); one link's path in a map (fix rounds 4–6; rounds 4–5 also wrote
/// the link's folder ID and name, no longer needed and ignored); or the bare path (fix round 3). Every form loads
/// and keeps holding.
#[derive(Deserialize)]
#[serde(untagged)]
enum HeldLinkRecord {
    Links { paths: BTreeSet<RelPath> },
    Link { path: RelPath },
    Path(RelPath),
}

impl From<HeldLinkRecord> for HeldLink {
    fn from(record: HeldLinkRecord) -> Self {
        match record {
            HeldLinkRecord::Links { paths } => Self { paths },
            HeldLinkRecord::Link { path } | HeldLinkRecord::Path(path) => Self::at(path),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteNode {
    pub parent: Option<NodeId>,
    pub name: Option<Name>,
    /// `None` for a directory.
    pub file: Option<FileState>,
}

impl RemoteNode {
    pub fn from_state(n: &NodeState) -> Self {
        let file = match &n.kind {
            NodeKind::Dir => None,
            NodeKind::File(f) => Some(f.clone()),
        };
        Self { parent: n.parent, name: n.name.clone(), file }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BaseEntry {
    pub parent: Option<NodeId>,
    pub name: Option<Name>,
    pub kind: BaseKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BaseKind {
    Dir {
        local_id: Option<u64>,
    },
    File {
        version: VersionId,
        manifest: ManifestId,
        size: u64,
        mtime_ms: i64,
        exec: bool,
        local_id: Option<u64>,
        /// The content both sides agreed on. Checked before a local file is overwritten or deleted.
        hash: FileHash,
        /// The filesystem clock when this was recorded (racy-timestamp rule).
        seen_ms: i64,
    },
}

/// Deepest path walked before giving up (cycles in damaged state can't loop forever).
const MAX_DEPTH: usize = 4096;

fn path_with(id: NodeId, root: Option<NodeId>, link: impl Fn(NodeId) -> Option<(Option<NodeId>, Option<Name>)>) -> Option<RelPath> {
    let mut names = Vec::new();
    let mut current = id;
    for _ in 0..MAX_DEPTH {
        if Some(current) == root {
            names.reverse();
            return Some(RelPath::from_names(names));
        }
        let (parent, name) = link(current)?;
        names.push(name?);
        current = parent?;
    }
    None
}

impl BaseKind {
    /// The local file or folder ID recorded for the node.
    pub fn local_id(&self) -> Option<u64> {
        match self {
            BaseKind::Dir { local_id } | BaseKind::File { local_id, .. } => *local_id,
        }
    }
}

/// The base: a map from node to [`BaseEntry`], with indexes kept up to date as it changes, so the engine never
/// rebuilds a whole path table per node (a first sync of a big library would cost the square of its size). Reads go
/// through the map ([`Base`] dereferences to it); every change goes through [`Base::insert`], [`Base::update`],
/// [`Base::remove`], [`Base::retain`] or [`Base::clear`].
///
/// Kept beside the map: each node's children; tracked files and folders by local ID; and, once asked for, every
/// node's path from a root with the nodes at each path ([`State::base_path`], [`State::base_at`]). A change moves
/// the paths of the changed node and of everything below it, and nothing else. Saved and compared as the bare map.
#[derive(Default)]
pub struct Base {
    map: BTreeMap<NodeId, BaseEntry>,
    /// Every node's children, by the `parent` its entry names (whether or not that parent is tracked).
    children: HashMap<NodeId, BTreeSet<NodeId>>,
    /// Tracked files by local ID, and tracked folders by local ID.
    files_by_local_id: HashMap<u64, BTreeSet<NodeId>>,
    dirs_by_local_id: HashMap<u64, BTreeSet<NodeId>>,
    /// Built on first use for the root that use names (and again for another root), then kept up to date. Behind a
    /// lock only because it is built on a read; changes reach it through `&mut self` without locking.
    paths: Mutex<Option<Paths>>,
    /// How many times `paths` was built from scratch (for tests that pin a pass's cost).
    builds: Arc<AtomicU64>,
}

impl Clone for Base {
    fn clone(&self) -> Self {
        let paths = self.paths.lock().unwrap_or_else(PoisonError::into_inner).clone();
        Base {
            map: self.map.clone(),
            children: self.children.clone(),
            files_by_local_id: self.files_by_local_id.clone(),
            dirs_by_local_id: self.dirs_by_local_id.clone(),
            paths: Mutex::new(paths),
            builds: self.builds.clone(),
        }
    }
}

/// Every base node's path from `root`, as [`State::base_path`] defines it, and the nodes at each path.
#[derive(Debug, Clone)]
struct Paths {
    root: Option<NodeId>,
    of: HashMap<NodeId, RelPath>,
    at: BTreeMap<RelPath, BTreeSet<NodeId>>,
}

impl Base {
    /// The entry of `id`, changed by `change`; returns false (and changes nothing) when `id` isn't tracked.
    pub fn update(&mut self, id: &NodeId, change: impl FnOnce(&mut BaseEntry)) -> bool {
        let Some(mut entry) = self.map.get(id).cloned() else { return false };
        change(&mut entry);
        self.insert(*id, entry);
        true
    }

    /// Tracks `id` with `entry`, replacing any entry it had, which is returned.
    pub fn insert(&mut self, id: NodeId, entry: BaseEntry) -> Option<BaseEntry> {
        let (parent, name, kind) = (entry.parent, entry.name.clone(), entry.kind.clone());
        let old = self.map.insert(id, entry);
        if let Some(old) = &old {
            self.unlink(id, old);
        }
        self.link(id, parent, &kind);
        let moved = old.as_ref().is_none_or(|o| o.parent != parent || o.name != name);
        if moved {
            self.repath(id);
        }
        old
    }

    /// Stops tracking `id`; returns its entry. Nodes below it keep their entries (and get their paths back if it
    /// is tracked again).
    pub fn remove(&mut self, id: &NodeId) -> Option<BaseEntry> {
        let old = self.map.remove(id)?;
        self.unlink(*id, &old);
        self.repath(*id);
        Some(old)
    }

    /// Keeps only the entries `keep` says yes to.
    pub fn retain(&mut self, mut keep: impl FnMut(&NodeId, &BaseEntry) -> bool) {
        let before = self.map.len();
        self.map.retain(|id, e| keep(id, e));
        if self.map.len() != before {
            self.reindex();
        }
    }

    /// Forgets every entry.
    pub fn clear(&mut self) {
        self.map.clear();
        self.reindex();
    }

    /// Whether any tracked node has `id` as its parent.
    pub fn has_children(&self, id: &NodeId) -> bool {
        self.children.get(id).is_some_and(|c| !c.is_empty())
    }

    /// Tracked files with local ID `local_id`.
    pub fn files_with_local_id(&self, local_id: u64) -> impl Iterator<Item = NodeId> + '_ {
        self.files_by_local_id.get(&local_id).into_iter().flatten().copied()
    }

    /// Tracked folders with local ID `local_id`.
    pub fn dirs_with_local_id(&self, local_id: u64) -> impl Iterator<Item = NodeId> + '_ {
        self.dirs_by_local_id.get(&local_id).into_iter().flatten().copied()
    }

    /// How many times the path index was built from scratch: once on first use, and again only after
    /// [`Base::retain`], [`Base::clear`] or a new root. Shared by clones (a test reads it from the engine's state).
    pub fn path_builds(&self) -> u64 {
        self.builds.load(Ordering::Relaxed)
    }

    /// Rebuilds the children and local-ID indexes from the map, and drops the paths (built again when asked for).
    fn reindex(&mut self) {
        self.children.clear();
        self.files_by_local_id.clear();
        self.dirs_by_local_id.clear();
        let entries: Vec<(NodeId, Option<NodeId>, BaseKind)> = self.map.iter().map(|(id, e)| (*id, e.parent, e.kind.clone())).collect();
        for (id, parent, kind) in entries {
            self.link(id, parent, &kind);
        }
        *self.paths.get_mut().unwrap_or_else(PoisonError::into_inner) = None;
    }

    fn link(&mut self, id: NodeId, parent: Option<NodeId>, kind: &BaseKind) {
        if let Some(parent) = parent {
            self.children.entry(parent).or_default().insert(id);
        }
        let by_id = match kind {
            BaseKind::File { .. } => &mut self.files_by_local_id,
            BaseKind::Dir { .. } => &mut self.dirs_by_local_id,
        };
        if let Some(lid) = kind.local_id() {
            by_id.entry(lid).or_default().insert(id);
        }
    }

    fn unlink(&mut self, id: NodeId, old: &BaseEntry) {
        if let Some(parent) = old.parent
            && let Some(siblings) = self.children.get_mut(&parent)
        {
            siblings.remove(&id);
            if siblings.is_empty() {
                self.children.remove(&parent);
            }
        }
        let by_id = match old.kind {
            BaseKind::File { .. } => &mut self.files_by_local_id,
            BaseKind::Dir { .. } => &mut self.dirs_by_local_id,
        };
        if let Some(lid) = old.kind.local_id()
            && let Some(ids) = by_id.get_mut(&lid)
        {
            ids.remove(&id);
            if ids.is_empty() {
                by_id.remove(&lid);
            }
        }
    }

    /// `read` of the paths from `root`, built first if they aren't yet (or were built from another root).
    ///
    /// The lock on the paths is held while `read` runs, so `read` must not reach the paths again (through
    /// [`State::base_path`], [`State::base_at`] or this): that would deadlock. Every caller only copies out what it
    /// needs.
    fn with_paths<R>(&self, root: Option<NodeId>, read: impl FnOnce(&Paths) -> R) -> R {
        let mut paths = self.paths.lock().unwrap_or_else(PoisonError::into_inner);
        let built = match paths.take() {
            Some(built) if built.root == root => built,
            _ => {
                self.builds.fetch_add(1, Ordering::Relaxed);
                let mut built = Paths { root, of: HashMap::new(), at: BTreeMap::new() };
                if let Some(root) = root {
                    built.walk(root, &self.map, &self.children);
                }
                built
            }
        };
        read(paths.insert(built))
    }

    /// Recomputes the paths of `id` and everything below it, if the paths are built.
    fn repath(&mut self, id: NodeId) {
        let Some(paths) = self.paths.get_mut().unwrap_or_else(PoisonError::into_inner).as_mut() else { return };
        paths.walk(id, &self.map, &self.children);
    }
}

impl Paths {
    /// Where `id` is, from its parent's path (already up to date) and its name: as [`path_with`] walks it.
    fn place(&self, id: NodeId, map: &BTreeMap<NodeId, BaseEntry>) -> Option<RelPath> {
        let entry = map.get(&id)?;
        if Some(id) == self.root {
            return Some(RelPath::root());
        }
        let parent = entry.parent?;
        let above = if Some(parent) == self.root { RelPath::root() } else { self.of.get(&parent)?.clone() };
        let path = above.join(entry.name.as_ref()?);
        (path.depth() < MAX_DEPTH).then_some(path)
    }

    /// Puts `from` and everything below it where they are now. A node whose path didn't change has nothing below it
    /// that changed either, so the walk stops there.
    fn walk(&mut self, from: NodeId, map: &BTreeMap<NodeId, BaseEntry>, children: &HashMap<NodeId, BTreeSet<NodeId>>) {
        // The node the walk starts from is always looked below: a fresh build starts with no paths at all, and the
        // children of an untracked root have paths of their own.
        let mut stack = vec![(from, true)];
        while let Some((id, start)) = stack.pop() {
            let now = self.place(id, map);
            let was = self.of.get(&id).cloned();
            if now == was && !start {
                continue;
            }
            if let Some(was) = was {
                if let Some(ids) = self.at.get_mut(&was) {
                    ids.remove(&id);
                    if ids.is_empty() {
                        self.at.remove(&was);
                    }
                }
                self.of.remove(&id);
            }
            if let Some(now) = now {
                self.at.entry(now.clone()).or_default().insert(id);
                self.of.insert(id, now);
            }
            // The root is never below anything (a damaged state could name a parent for it).
            stack.extend(children.get(&id).into_iter().flatten().filter(|c| Some(**c) != self.root).map(|c| (*c, false)));
        }
    }
}

impl std::ops::Deref for Base {
    type Target = BTreeMap<NodeId, BaseEntry>;

    fn deref(&self) -> &Self::Target {
        &self.map
    }
}

impl<'a> IntoIterator for &'a Base {
    type Item = (&'a NodeId, &'a BaseEntry);
    type IntoIter = std::collections::btree_map::Iter<'a, NodeId, BaseEntry>;

    fn into_iter(self) -> Self::IntoIter {
        self.map.iter()
    }
}

impl FromIterator<(NodeId, BaseEntry)> for Base {
    fn from_iter<I: IntoIterator<Item = (NodeId, BaseEntry)>>(entries: I) -> Self {
        let mut base = Base { map: entries.into_iter().collect(), ..Base::default() };
        base.reindex();
        base
    }
}

impl PartialEq for Base {
    fn eq(&self, other: &Self) -> bool {
        self.map == other.map
    }
}

impl std::fmt::Debug for Base {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.map.fmt(f)
    }
}

impl Serialize for Base {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.map.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Base {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(BTreeMap::<NodeId, BaseEntry>::deserialize(deserializer)?.into_iter().collect())
    }
}

impl State {
    /// Where a node sits in the base tree.
    pub fn base_path(&self, id: NodeId) -> Option<RelPath> {
        // The root is the root, tracked or not.
        if Some(id) == self.root {
            return Some(RelPath::root());
        }
        self.base.with_paths(self.root, |paths| paths.of.get(&id).cloned())
    }

    /// Where a node sits in the remote tree.
    pub fn remote_path(&self, id: NodeId) -> Option<RelPath> {
        path_with(id, self.root, |n| self.remote.get(&n).map(|e| (e.parent, e.name.clone())))
    }

    /// The node whose base path is `path` (of several, the one with the greatest ID, as [`State::base_index`] has it).
    pub fn base_at(&self, path: &RelPath) -> Option<NodeId> {
        self.base.with_paths(self.root, |paths| paths.at.get(path).and_then(|ids| ids.last()).copied())
    }

    /// Whether a node's base path is `path`.
    pub fn base_has_path(&self, path: &RelPath) -> bool {
        self.base_at(path).is_some()
    }

    /// Base path → node, for every node with a path (of several at one path, the one with the greatest ID). A copy of
    /// the whole index: for a look at every node; one path is [`State::base_at`].
    pub fn base_index(&self) -> BTreeMap<RelPath, NodeId> {
        self.base.with_paths(self.root, |paths| paths.at.iter().filter_map(|(path, ids)| Some((path.clone(), *ids.last()?))).collect())
    }

    /// The base paths below `dir` (not `dir` itself), in order.
    pub fn base_paths_below(&self, dir: &RelPath) -> Vec<RelPath> {
        self.base.with_paths(self.root, |paths| {
            paths.at.range(dir.clone()..).skip_while(|(p, _)| *p == dir).take_while(|(p, _)| p.starts_with(dir)).map(|(p, _)| p.clone()).collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The base's indexes say exactly what walking the map says, through random changes: moves of folders with
    /// things in them, two nodes at one path, nodes whose folder is gone, loops, the root changing.
    #[test]
    fn the_indexes_agree_with_the_map_through_any_changes() {
        let ids: Vec<NodeId> = (0..12).map(|_| NodeId::generate()).collect();
        let names: Vec<Name> = ["a", "b", "c"].iter().map(|n| Name::new(n).unwrap()).collect();
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move |n: usize| -> usize {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        let mut state = State { root: Some(ids[0]), ..State::default() };
        let mut builds = 0;
        for step in 0..3000 {
            let id = ids[next(ids.len())];
            let entry = || BaseEntry { parent: None, name: None, kind: BaseKind::Dir { local_id: None } };
            match next(10) {
                0..=4 => {
                    let mut e = entry();
                    e.parent = (next(8) != 0).then(|| ids[next(ids.len())]);
                    e.name = (next(8) != 0).then(|| names[next(names.len())].clone());
                    let lid = (next(3) != 0).then(|| next(4) as u64);
                    e.kind = if next(2) == 0 { BaseKind::Dir { local_id: lid } } else { file(lid) };
                    state.base.insert(id, e);
                }
                5 | 6 => {
                    state.base.remove(&id);
                }
                7 => {
                    let lid = (next(2) == 0).then(|| next(4) as u64);
                    state.base.update(&id, |e| {
                        e.kind = match e.kind {
                            BaseKind::Dir { .. } => BaseKind::Dir { local_id: lid },
                            BaseKind::File { .. } => file(lid),
                        }
                    });
                }
                8 => {
                    let keep = next(ids.len());
                    state.base.retain(|i, _| *i != ids[keep]);
                }
                _ => {
                    if next(20) == 0 {
                        state.root = Some(ids[next(3)]);
                    }
                    if next(50) == 0 {
                        state.base.clear();
                    }
                }
            }
            // Asked only now and then, so the paths are kept up to date in between as often as they are built.
            if next(3) != 0 {
                continue;
            }
            builds += 1;
            let slow = |id: NodeId| path_with(id, state.root, |n| state.base.get(&n).map(|e| (e.parent, e.name.clone())));
            let slow_index: BTreeMap<RelPath, NodeId> = state.base.keys().filter_map(|id| Some((slow(*id)?, *id))).collect();
            assert_eq!(state.base_index(), slow_index, "step {step}");
            for id in &ids {
                assert_eq!(state.base_path(*id), slow(*id), "step {step}, node {id}");
                assert_eq!(state.base.has_children(id), state.base.values().any(|e| e.parent == Some(*id)), "step {step}");
            }
            for (path, id) in &slow_index {
                assert_eq!(state.base_at(path), Some(*id), "step {step}");
                let below: Vec<RelPath> = slow_index.keys().filter(|p| p.starts_with(path) && *p != path).cloned().collect();
                assert_eq!(state.base_paths_below(path), below, "step {step}");
            }
            for lid in 0..4 {
                let files: BTreeSet<NodeId> = state.base.files_with_local_id(lid).collect();
                let dirs: BTreeSet<NodeId> = state.base.dirs_with_local_id(lid).collect();
                let scan = |dir: bool| -> BTreeSet<NodeId> {
                    state
                        .base
                        .iter()
                        .filter(|(_, e)| matches!(e.kind, BaseKind::Dir { .. }) == dir && e.kind.local_id() == Some(lid))
                        .map(|(i, _)| *i)
                        .collect()
                };
                assert_eq!((files, dirs), (scan(false), scan(true)), "step {step}");
            }
        }
        assert!(builds > 600);
        // Saved and loaded, it is the same map with the same indexes.
        let saved = serde_json::to_string(&state).unwrap();
        let loaded: State = serde_json::from_str(&saved).unwrap();
        assert_eq!(loaded, state);
        assert_eq!(loaded.base_index(), state.base_index());
    }

    fn file(local_id: Option<u64>) -> BaseKind {
        BaseKind::File {
            version: VersionId::generate(),
            manifest: ManifestId::from_bytes([0; 32]),
            size: 0,
            mtime_ms: 0,
            exec: false,
            local_id,
            hash: FileHash::from_bytes([0; 32]),
            seen_ms: 0,
        }
    }
}
