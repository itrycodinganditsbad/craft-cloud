//! Turning the folder's changes into uploads and batch commits.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io;

use bytes::Bytes;
use craft_cloud_chunker::{FASTCDC_V1, chunk_reader};
use craft_cloud_proto::limits::{MAX_BATCH_JSON_BYTES, MAX_BATCH_OPS, MAX_TREE_DEPTH};

/// What an operation adds to a commit body, at most: a longest operation without a save record is at most 1/1000 of
/// the body limit (`craft_cloud_proto::ops` tests that a full batch of them fits), and a record adds its document and
/// under 48 bytes per head of its frontiers. Without records a batch is the full [`MAX_BATCH_OPS`].
fn op_weight(op: &Op) -> usize {
    let save = match op {
        Op::Create { version, .. } | Op::Update { version, .. } => {
            version.save.as_ref().map_or(0, |s| s.frontiers.len().saturating_mul(48).saturating_add(64))
        }
        _ => 0,
    };
    (MAX_BATCH_JSON_BYTES / MAX_BATCH_OPS).saturating_add(save)
}
use craft_cloud_proto::{
    ChunkId, ChunkRef, ChunkerParamsId, Conflict, Epoch, FileHash, FileState, FoldKey, Manifest, Name, NewVersion, NodeId, Op, VersionId,
};

use crate::fs::{FileMeta, Fs, FsKind};
use crate::path::RelPath;
use crate::state::{BaseEntry, BaseKind, HeldLink, RemoteNode};
use crate::transport::{Transport, TransportError};

use super::{Effect, Engine, MISSING_QUERY_IDS, Report, SAVE_RECORD_WAITS, STILL_CHANGING, SyncError, per_node};

/// Why a file of `size` bytes is skipped when files may be at most `limit` bytes.
fn too_big(size: u64, limit: u64) -> String {
    format!("too big to sync: {size} bytes, and files can be at most {limit} bytes")
}

impl<F: Fs, T: Transport> Engine<F, T> {
    pub(super) async fn push(&mut self, report: &mut Report) -> Result<(), SyncError> {
        let root = self.state.root.ok_or_else(|| SyncError::Corrupt("no root".into()))?;
        let (local, mut unseen) = self.scan(report)?;
        // Inside a folder that couldn't be listed, or at or below a link, nothing is known, so nothing tracked there
        // is missing. Judged where the node is expected now: a link moves with a folder the user renamed.
        let hidden = |path: &RelPath| unseen.hides(path);
        let unknown = &unseen.unknown;
        let index = self.state.base_index();
        // Folders that moved: a missing base folder whose local ID turns up at an untracked path.
        let untracked_dirs: HashMap<u64, RelPath> = local
            .iter()
            .filter(|(p, _)| !index.contains_key(*p))
            .filter_map(|(p, k)| match k {
                FsKind::Dir { id: Some(i) } => Some((*i, p.clone())),
                _ => None,
            })
            .collect();
        let mut dir_moves: Vec<(NodeId, RelPath, RelPath)> = Vec::new();
        for (path, id) in &index {
            let Some(BaseEntry { kind: BaseKind::Dir { local_id: Some(lid) }, .. }) = self.state.base.get(id) else { continue };
            // A folder the server deleted (still tracked here while a node in it waits) can't be moved there.
            if *id == root || matches!(local.get(path), Some(FsKind::Dir { .. })) || !self.state.remote.contains_key(id) {
                continue;
            }
            if let Some(to) = untracked_dirs.get(lid)
                && !dir_moves.iter().any(|(_, from, _)| path.starts_with(from))
            {
                dir_moves.push((*id, path.clone(), to.clone()));
            }
        }
        // Where a base path is expected now, after folder moves.
        let expected = |path: &RelPath| -> RelPath {
            for (_, from, to) in &dir_moves {
                if path.starts_with(from) {
                    let mut names = to.names().to_vec();
                    names.extend_from_slice(&path.names()[from.depth()..]);
                    return RelPath::from_names(names);
                }
            }
            path.clone()
        };
        let mut expected_index: BTreeMap<RelPath, NodeId> = BTreeMap::new();
        for (path, id) in &index {
            expected_index.insert(expected(path), *id);
        }

        // Classify base files and folders.
        let mut modified: Vec<(NodeId, RelPath, FileMeta)> = Vec::new();
        let mut missing_files: Vec<(NodeId, RelPath)> = Vec::new();
        let mut missing_dirs: Vec<(NodeId, RelPath)> = Vec::new();
        for (path, id) in &index {
            if *id == root {
                continue;
            }
            let Some(entry) = self.state.base.get(id) else { continue };
            let now = expected(path);
            match (&entry.kind, local.get(&now)) {
                (BaseKind::File { local_id, .. }, Some(FsKind::File(m))) if m.id != *local_id && self.owns_local_file(m.id, *id) => {
                    // Another tracked file is here now; this one moved (or was moved aside).
                    missing_files.push((*id, path.clone()));
                }
                (BaseKind::File { local_id, hash, .. }, Some(FsKind::File(m))) => {
                    if !self.file_matches(entry, &now, m)? {
                        if m.id != *local_id && self.state.held_deletes.contains_key(id) && self.content_hash(&now) != Some(*hash) {
                            // A held node's place holds another file (another local ID, other content): not the node.
                            // Another machine may have moved the node's folder onto a folder of this machine's own (seed
                            // 95497 of fix round 5 moved the node itself; such a move now moves the file aside, see
                            // `apply_missing`). It stays missing, so the hold stands while its link does; that file waits.
                            missing_files.push((*id, path.clone()));
                        } else {
                            modified.push((*id, now, *m));
                        }
                    }
                }
                (BaseKind::File { .. }, _) => missing_files.push((*id, path.clone())),
                (BaseKind::Dir { .. }, Some(FsKind::Dir { .. })) => {}
                (BaseKind::Dir { .. }, _) => {
                    if !dir_moves.iter().any(|(_, from, _)| path.starts_with(from)) {
                        missing_dirs.push((*id, path.clone()));
                    }
                }
            }
        }

        // Every folder by its local ID.
        let dir_at = Self::dirs_by_id(&local);
        self.refresh_dir_ids(&index, &local, &unseen, &expected);

        // File moves: a missing base file whose local ID turns up at an untracked path.
        let untracked_files: HashMap<u64, (RelPath, FileMeta)> = local
            .iter()
            .filter(|(p, _)| !expected_index.contains_key(*p))
            .filter_map(|(p, k)| match k {
                FsKind::File(m) => m.id.map(|i| (i, (p.clone(), *m))),
                _ => None,
            })
            .collect();
        let mut file_moves: Vec<(NodeId, RelPath, RelPath, FileMeta)> = Vec::new();
        missing_files.retain(|(id, path)| {
            let Some(BaseEntry { kind: BaseKind::File { local_id: Some(lid), .. }, .. }) = self.state.base.get(id) else { return true };
            match untracked_files.get(lid) {
                Some((to, meta)) => {
                    file_moves.push((*id, path.clone(), to.clone(), *meta));
                    false
                }
                None => true,
            }
        });
        // Every node not found here: at its place, moved by its local ID, or in a folder that moved. A hold on any
        // other node is dropped: that node turned up again, so the link isn't it.
        let not_found: HashSet<NodeId> = missing_files.iter().chain(&missing_dirs).map(|(id, _)| *id).collect();
        // A folder above a missing node that the user moved is found by its local ID even where a new folder took
        // its old name (so it isn't a folder move here): a link the node turned into moved with it.
        let behind_moved_link = |id: NodeId| -> bool {
            let mut below = Vec::new();
            let mut current = id;
            for _ in 0..=MAX_TREE_DEPTH {
                let Some(BaseEntry { name: Some(name), parent: Some(parent), .. }) = self.state.base.get(&current) else { return false };
                below.push(name.clone());
                current = *parent;
                if let Some(BaseEntry { kind: BaseKind::Dir { local_id: Some(lid) }, .. }) = self.state.base.get(&current)
                    && let Some(now) = dir_at.get(lid)
                    && hidden(&below.iter().rev().fold(now.clone(), |p, n| p.join(n)))
                {
                    return true;
                }
            }
            false
        };
        missing_files.retain(|(id, path)| !hidden(&expected(path)) && !behind_moved_link(*id));
        missing_dirs.retain(|(id, path)| !hidden(&expected(path)) && !behind_moved_link(*id));
        // A node the server already deleted (still tracked here while something waits for it) needs no delete.
        missing_files.retain(|(id, _)| self.state.remote.contains_key(id));
        missing_dirs.retain(|(id, _)| self.state.remote.contains_key(id));
        // Deletes held behind a link (seed 112337's no-pass order). A link or special file at a path where the last
        // finished push saw none appeared since. If it carries a name from a missing node's last local path (the
        // node's own name or a folder's on its path, where this machine last had it: before any server move the pull
        // let it follow, see `State::left_from`), it may be that node, moved with its folder: a folder put back from
        // another drive and moved before any pass saw it has a new local ID, so nothing tells the move apart from a
        // delete plus a new folder. The node's delete is then held by node ID with the paths of every such link (an
        // unrelated one with the same name may appear in the same window; `State::held_deletes`), saved before this
        // pass commits anything. No folder identity goes into this, so no
        // folder gesture can make such a link look old (a new folder at the old place, another folder renamed onto
        // it, a deleted folder's place taken, a folder that can't be listed, a server move). A link that was there at
        // the last finished push never holds anything.
        //
        // A hold lasts while any of its links stands where the hold last saw it (or can't be seen, or the scan
        // couldn't see everything), or a link with that one's name stands at a path the last finished push saw no
        // link at (the link's folder moved: the user's rename, another machine's rename pulled here, the engine moving
        // it aside, put back under a new name; every such path), and while the node isn't found here again. Once none
        // is left (the user removed each link, renamed the link itself, or made it a real file again, which goes up
        // as a new one) the node is judged afresh. The folders above a held node wait with it. A hold on a node the
        // server no longer has, or this machine no longer tracks, is dropped: there is nothing left to delete. A hold
        // the user released by naming its node in an approval is dropped too, and isn't made again this pass: its
        // delete goes.
        let mut holds = std::mem::take(&mut self.state.held_deletes);
        // A hold with no link (only from a hand-edited or damaged state; the agent drops those at load with a line) is
        // never kept: it would hold the delete without ever being shown.
        holds.retain(|id, link| {
            !link.paths.is_empty()
                && self.state.base.contains_key(id)
                && self.state.remote.contains_key(id)
                && not_found.contains(id)
                && !self.release.contains(id)
        });
        // The links that appeared since the last finished push, by folded name (paths in order). Only needed while
        // something is missing or held: a synced project with thousands of links costs nothing on an ordinary pass.
        let mut new_links: HashMap<FoldKey, Vec<&RelPath>> = HashMap::new();
        if !(missing_files.is_empty() && missing_dirs.is_empty() && holds.is_empty()) {
            for link in unseen.link_paths.iter().filter(|p| !self.state.known_links.contains(*p)) {
                if let Some(name) = link.file_name() {
                    new_links.entry(name.fold_key()).or_default().push(link);
                }
            }
        }
        let blind = !unseen.unknown.is_empty();
        holds.retain(|_, link| {
            if blind {
                return true;
            }
            // Each candidate stands where it was, or follows by name to every new link (none: it is gone).
            link.paths = std::mem::take(&mut link.paths)
                .into_iter()
                .flat_map(|path| {
                    if unseen.hides(&path) {
                        return vec![path];
                    }
                    path.file_name()
                        .and_then(|n| new_links.get(&n.fold_key()))
                        .map(|moved| moved.iter().map(|p| (*p).clone()).collect())
                        .unwrap_or_default()
                })
                .collect();
            !link.paths.is_empty()
        });
        // The folded names on a node's path: in the base, and where this machine last had it or a folder above it
        // before the pull let it follow a server move.
        let names_of = |id: NodeId| -> HashSet<FoldKey> {
            let mut keys = HashSet::new();
            let mut current = id;
            for _ in 0..=MAX_TREE_DEPTH {
                if let Some(was) = self.state.left_from.get(&current) {
                    keys.extend(was.names().iter().map(Name::fold_key));
                }
                let Some(BaseEntry { name: Some(name), parent: Some(parent), .. }) = self.state.base.get(&current) else { break };
                keys.insert(name.fold_key());
                current = *parent;
            }
            keys
        };
        // Every candidate: an unrelated link with the same name may appear in the same window as the real one.
        let held = |id: NodeId| -> Option<HeldLink> {
            if new_links.is_empty() {
                return None;
            }
            let paths: BTreeSet<RelPath> = names_of(id).iter().filter_map(|k| new_links.get(k)).flatten().map(|p| (*p).clone()).collect();
            (!paths.is_empty()).then_some(HeldLink { paths })
        };
        // Every held path and every folder above one, so the folders that wait with them are found by a lookup (not
        // by comparing every missing folder with every hold).
        let mut kept_by_link: HashSet<RelPath> = HashSet::new();
        let mut new_holds = false;
        for list in [&mut missing_files, &mut missing_dirs] {
            list.retain(|(id, path)| {
                if !holds.contains_key(id) {
                    if self.release.contains(id) {
                        return true;
                    }
                    let Some(link) = held(*id) else { return true };
                    holds.insert(*id, link);
                    new_holds = true;
                }
                // The path and its folders up to the root; a folder already in the set has its own above it.
                let mut at = Some(path.clone());
                while let Some(p) = at {
                    at = p.parent();
                    if !kept_by_link.insert(p) {
                        break;
                    }
                }
                false
            });
        }
        missing_dirs.retain(|(_, path)| !kept_by_link.contains(path));
        self.state.held_deletes = holds;
        // A new hold is saved before any commit of this pass (the link stays new until the push finishes, so a crash
        // before that would find it again; saving it first doesn't depend on that).
        if new_holds {
            self.checkpoint()?;
        }
        let moved_targets: BTreeSet<RelPath> =
            file_moves.iter().map(|(_, _, to, _)| to.clone()).chain(dir_moves.iter().map(|(_, _, to)| to.clone())).collect();

        // New local folders and files, parents first.
        let mut new_paths: Vec<(&RelPath, &FsKind)> = local
            .iter()
            .filter(|(p, _)| {
                !expected_index.contains_key(*p)
                    && !moved_targets.contains(*p)
                    && !moved_targets.iter().any(|t| p.starts_with(t) && *p != t)
                    // A new folder that can't be listed waits until it can.
                    && !unknown.contains(*p)
            })
            .collect();
        new_paths.sort_by_key(|(p, _)| p.depth());

        let mut ops: Vec<(Op, Effect)> = Vec::new();
        let mut new_ids: HashMap<RelPath, NodeId> = HashMap::new();
        let parent_id = |path: &RelPath, new_ids: &HashMap<RelPath, NodeId>| -> Option<NodeId> {
            let parent = path.parent()?;
            if parent.is_root() {
                return Some(root);
            }
            new_ids.get(&parent).copied().or_else(|| expected_index.get(&parent).copied())
        };

        // Folder moves and new folders, shallowest target first, so every parent exists before its children.
        enum DirOp<'a> {
            Move(NodeId, &'a RelPath, &'a RelPath),
            New(&'a RelPath, Option<u64>),
        }
        let mut dir_ops: Vec<(usize, DirOp)> = Vec::new();
        for (id, from, to) in &dir_moves {
            dir_ops.push((to.depth(), DirOp::Move(*id, from, to)));
        }
        for (path, kind) in &new_paths {
            // A new folder holding only ignored files (one the server deleted, kept here for a lock file) stays local.
            if let FsKind::Dir { id: local_id } = kind
                && !self.holds_only_ignored(path)?
            {
                dir_ops.push((path.depth(), DirOp::New(path, *local_id)));
            }
        }
        dir_ops.sort_by_key(|(depth, _)| *depth);
        for (_, op) in dir_ops {
            match op {
                DirOp::Move(id, from, to) => {
                    let (Some(entry), Some(name), Some(parent)) = (self.state.base.get(&id), to.file_name(), parent_id(to, &new_ids)) else {
                        continue;
                    };
                    // Name where the server has the node now: if it moved there too, our move is redone on top.
                    let (from_parent, from_name) =
                        self.state.remote.get(&id).map_or((entry.parent, entry.name.clone()), |r| (r.parent, r.name.clone()));
                    let (Some(from_parent), Some(from_name)) = (from_parent, from_name) else { continue };
                    ops.push((
                        Op::Move { node: id, from_parent, from_name, parent, name: name.clone() },
                        Effect::Move { id, parent, name: name.clone(), from: from.clone(), to: to.clone() },
                    ));
                }
                DirOp::New(path, local_id) => {
                    let (Some(name), Some(parent)) = (path.file_name(), parent_id(path, &new_ids)) else { continue };
                    let id = NodeId::generate();
                    new_ids.insert(path.clone(), id);
                    ops.push((
                        Op::Mkdir { node: id, parent, name: name.clone() },
                        Effect::Mkdir { id, parent, name: name.clone(), local_id, path: path.clone() },
                    ));
                }
            }
        }
        // File moves (and edits that came with them).
        for (id, from, to, meta) in &file_moves {
            let (Some(entry), Some(name), Some(parent)) = (self.state.base.get(id).cloned(), to.file_name(), parent_id(to, &new_ids)) else {
                continue;
            };
            // Name where the server has the node now: if it moved there too, our move is redone on top.
            let (from_parent, from_name) = self.state.remote.get(id).map_or((entry.parent, entry.name.clone()), |r| (r.parent, r.name.clone()));
            let (Some(from_parent), Some(from_name)) = (from_parent, from_name) else { continue };
            ops.push((
                Op::Move { node: *id, from_parent, from_name, parent, name: name.clone() },
                Effect::Move { id: *id, parent, name: name.clone(), from: from.clone(), to: to.clone() },
            ));
            if !self.file_matches(&entry, to, meta)? {
                modified.push((*id, to.clone(), *meta));
            }
        }
        // Edits. One that may still be being written waits (the base keeps the old version, so it isn't lost or
        // overwritten meanwhile) and is looked at again later.
        for (id, path, meta) in &modified {
            if self.fs.still_changing(path, meta) {
                report.skipped.push((path.to_string(), format!("{STILL_CHANGING}; synced once it stops")));
                continue;
            }
            // Two Craft saves waiting to learn which is newer (see `SaveRecords::includes`): not sent over the other.
            if self.deferred.contains(id) {
                continue;
            }
            let Some(BaseEntry { kind: BaseKind::File { version: expected, exec, size: base_size, .. }, .. }) = self.state.base.get(id).cloned()
            else {
                continue;
            };
            let Some((version, size, meta, hash)) = self.upload(path, exec, base_size, report).await? else { continue };
            // Content a Craft app is saving whose record hasn't come: never sent without it (the other machines would
            // take it for an edit outside Craft). The next pass, once the record is here, sends it with it.
            if self.save_pending(*id, &hash) {
                report.skipped.push((path.to_string(), format!("{SAVE_RECORD_WAITS}; tried again next pass")));
                continue;
            }
            // Content a Craft app saved from the file's live document carries its record.
            let version = NewVersion { save: self.save_record(*id, &hash), ..version };
            ops.push((Op::Update { node: *id, expected, version: version.clone() }, Effect::Update { id: *id, version, size, meta, hash }));
        }
        // New files.
        for (path, kind) in &new_paths {
            let FsKind::File(meta) = kind else { continue };
            if self.fs.still_changing(path, meta) {
                report.skipped.push((path.to_string(), format!("{STILL_CHANGING}; synced once it stops")));
                continue;
            }
            let (Some(name), Some(parent)) = (path.file_name().cloned(), parent_id(path, &new_ids)) else { continue };
            let Some((version, size, meta, hash)) = self.upload(path, false, 0, report).await? else { continue };
            let id = NodeId::generate();
            ops.push((
                Op::Create { node: id, parent, name: name.clone(), version: version.clone() },
                Effect::File { id, parent, name, version, size, meta, hash },
            ));
        }
        // Deletes, files first then folders deepest first, unless the guard holds them back.
        let synced_files = self.state.base.values().filter(|e| matches!(e.kind, BaseKind::File { .. })).count();
        // Above the guard's thresholds only approved deletions go; the rest (and the folders) wait.
        let guarded = self.config.too_many_deletes(missing_files.len(), synced_files);
        let (going, waiting): (Vec<_>, Vec<_>) = missing_files.iter().partition(|(id, _)| !guarded || self.approval.covers(*id));
        for (id, path) in &waiting {
            report.pending_deletes.push(path.clone());
            report.pending_nodes.push(*id);
        }
        for (id, path) in going {
            if let Some(BaseEntry { kind: BaseKind::File { version, .. }, .. }) = self.state.base.get(id) {
                ops.push((Op::Delete { node: *id, expected: Some(*version) }, Effect::Delete { id: *id, path: path.clone() }));
            }
        }
        if waiting.is_empty() {
            missing_dirs.sort_by_key(|(_, p)| std::cmp::Reverse(p.depth()));
            for (id, path) in &missing_dirs {
                ops.push((Op::Delete { node: *id, expected: None }, Effect::Delete { id: *id, path: path.clone() }));
            }
        }

        let seen_links = std::mem::take(&mut unseen.link_paths);
        self.commit_all(ops, report, seen_links).await?;
        // Every hold, where the node is now, for the pass summary and `status`.
        let held: Vec<(NodeId, RelPath, Vec<RelPath>)> = self
            .state
            .held_deletes
            .iter()
            .filter_map(|(id, link)| Some((*id, self.state.base_path(*id)?, link.paths.iter().cloned().collect())))
            .collect();
        for (id, node, links) in held {
            report.held_nodes.push(id);
            report.held_deletes.push((node, links));
        }
        Ok(())
    }

    /// Uploads a file's content: chunks it as a stream, asks which chunks the server lacks, and sends those,
    /// read back by range and checked against their IDs. `None` when the file can't be uploaded this pass:
    /// it changed while being read (tried again next pass), it can't be read at all, or it is over the protocol's
    /// limits (both reported as skipped, with the reason, every pass until it changes; everything else keeps
    /// syncing).
    ///
    /// The version is executable as the file is here, or, on a filesystem without an executable bit (Windows), as
    /// `base_exec` says (the base's bit for an edit, false for a new file): an edit there never clears the bit a
    /// Unix machine gave the file.
    ///
    /// A file the server refuses because the account would store more than it may is skipped too, while the account
    /// has room left for at least [`Config::quota_floor`](super::Config::quota_floor) bytes (smaller files may still
    /// fit). With less room than that, the refusal ends the pass: the account counts as full, and the agent pauses
    /// and says so. After a refusal, a later file this pass is skipped before it is read when it could not fit even
    /// if all of its base version's content were reused (`size > room + base_size`; `base_size` is 0 for a new
    /// file). So a nearly full account does not have its backlog read on every pass: a file bigger than the room is
    /// read at most once per pass, and files within a chunk's bookkeeping of the room (the server counts a stored
    /// chunk one byte over its size, and the manifest) may still be read and refused. An edit that fits against its
    /// own base is always read. The one gap: a new file bigger than the room whose chunks are all stored already
    /// (a copy of a synced file) is skipped too; it goes up once the room grows or on a pass that meets no refusal
    /// before it.
    ///
    /// The chunks a refused file stored before the refusal stay stored and counted until garbage collection (M4)
    /// removes what no version references.
    async fn upload(
        &mut self,
        path: &RelPath,
        base_exec: bool,
        base_size: u64,
        report: &mut Report,
    ) -> Result<Option<(NewVersion, u64, FileMeta, FileHash)>, SyncError> {
        let skip = |used: u64, limit: u64| {
            format!("too big for the account's storage on the server ({used} of {limit} bytes used); the admin can allow more")
        };
        if let Some((used, limit)) = self.quota
            && let Some(FsKind::File(meta)) = self.stat(path)?
            && meta.size > limit.saturating_sub(used).saturating_add(base_size)
        {
            report.skipped.push((path.to_string(), skip(used, limit)));
            return Ok(None);
        }
        match self.upload_content(path, base_exec, report).await {
            Err(SyncError::Transport(TransportError::QuotaExceeded { used, limit })) if limit.saturating_sub(used) >= self.config.quota_floor => {
                self.quota = Some((used, limit));
                report.skipped.push((path.to_string(), skip(used, limit)));
                Ok(None)
            }
            other => other,
        }
    }

    /// [`Engine::upload`], but for the quota.
    async fn upload_content(
        &mut self,
        path: &RelPath,
        base_exec: bool,
        report: &mut Report,
    ) -> Result<Option<(NewVersion, u64, FileMeta, FileHash)>, SyncError> {
        let Some(FsKind::File(before)) = self.stat(path)? else { return Ok(None) };
        // Too big to go up: said before a byte is read.
        if before.size > self.config.max_file_bytes {
            report.skipped.push((path.to_string(), too_big(before.size, self.config.max_file_bytes)));
            return Ok(None);
        }
        let mut pieces: Vec<(ChunkId, u64, usize)> = Vec::new();
        let chunked = self.fs.open(path).and_then(|reader| {
            chunk_reader::<_, io::Error, _>(&self.chunker, reader, |chunk| {
                pieces.push((ChunkId::from_bytes(chunk.hash), chunk.offset, chunk.data.len()));
                Ok(())
            })
        });
        let summary = match chunked {
            Ok(summary) => summary,
            Err(e) => {
                report.skipped.push((path.to_string(), format!("can't be read: {e}")));
                return Ok(None);
            }
        };
        if !matches!(self.stat(path)?, Some(FsKind::File(after)) if after == before) {
            return Ok(None);
        }
        let mut unique: Vec<ChunkId> = Vec::new();
        let mut seen = BTreeSet::new();
        for (id, _, _) in &pieces {
            if seen.insert(*id) {
                unique.push(*id);
            }
        }
        let mut missing = BTreeSet::new();
        for batch in unique.chunks(MISSING_QUERY_IDS) {
            missing.extend(self.transport.missing_chunks(batch).await?);
        }
        for (id, offset, len) in &pieces {
            if missing.remove(id) && !self.send_chunk(path, id, *offset, *len, report).await? {
                return Ok(None);
            }
        }
        let refs = summary.chunks.iter().map(|c| ChunkRef { id: ChunkId::from_bytes(c.hash), len: c.len }).collect();
        let params = ChunkerParamsId::new(FASTCDC_V1.name).map_err(|e| SyncError::Corrupt(e.to_string()))?;
        // A file over the protocol's limits (too big, too many chunks) is the file's problem, not damaged data: it
        // is skipped with the reason, and everything else keeps syncing.
        let manifest = match Manifest::new(params, summary.size, FileHash::from_bytes(summary.file_hash), refs) {
            Ok(manifest) => manifest,
            Err(e) => {
                report.skipped.push((path.to_string(), format!("can't be synced: {e}")));
                return Ok(None);
            }
        };
        let encoded = Bytes::from(manifest.encode());
        match self.transport.put_manifest(&manifest.id(), encoded.clone()).await {
            // The server lacks chunks it took (a proxy answered for it and lost them, say): exactly those go up once,
            // and the manifest once more. A second refusal fails the pass, which is tried again later. The server lists
            // at most `MAX_MISSING_IDS` of them, so a proxy that lost more converges over several passes.
            Err(TransportError::MissingChunks(lost)) => {
                let at: HashMap<ChunkId, (u64, usize)> = pieces.iter().map(|(id, offset, len)| (*id, (*offset, *len))).collect();
                let mut sent = BTreeSet::new();
                for id in lost {
                    let Some((offset, len)) = at.get(&id) else {
                        return Err(TransportError::Protocol(format!("the server asked for chunk {id}, which the manifest doesn't list")).into());
                    };
                    if sent.insert(id) && !self.send_chunk(path, &id, *offset, *len, report).await? {
                        return Ok(None);
                    }
                }
                self.transport.put_manifest(&manifest.id(), encoded).await?;
            }
            other => other?,
        }
        report.uploaded.push(path.clone());
        let exec = if self.fs.has_exec_bit() { before.exec } else { base_exec };
        let version = NewVersion { id: VersionId::generate(), manifest: manifest.id(), mtime_ms: before.mtime_ms, exec, save: None };
        Ok(Some((version, summary.size, before, FileHash::from_bytes(summary.file_hash))))
    }

    /// Sends the chunk `id` of the file at `path` (`len` bytes at `offset`), read back and checked against its ID.
    /// False when it can't be sent this pass: the file can't be read (reported as skipped), or it changed since it
    /// was chunked (tried again next pass).
    ///
    /// Takes `&mut self` like the rest of a pass: its future then needs only `Engine: Send`, never `Sync`.
    async fn send_chunk(&mut self, path: &RelPath, id: &ChunkId, offset: u64, len: usize, report: &mut Report) -> Result<bool, SyncError> {
        let data = match self.fs.read_at(path, offset, len) {
            Ok(data) => data,
            Err(e) => {
                report.skipped.push((path.to_string(), format!("can't be read: {e}")));
                return Ok(false);
            }
        };
        if data.len() != len || *blake3::hash(&data).as_bytes() != *id.as_bytes() {
            return Ok(false);
        }
        self.transport.put_chunk(id, Bytes::from(data)).await?;
        Ok(true)
    }

    /// A push finished: the links it saw (`links`) are known from now on, since anything a new one could be is held
    /// by now, and no node's last local place needs remembering (see `State::known_links`, `State::left_from`).
    /// Returns whether that changed the state.
    fn finish_push(&mut self, links: BTreeSet<RelPath>) -> bool {
        let changed = self.state.known_links != links || !self.state.left_from.is_empty();
        self.state.known_links = links;
        self.state.left_from.clear();
        changed
    }

    /// Commits operations in batches. A refused batch is retried without its refused operations (their
    /// dependants are usually refused with them); refused operations are tried again next pass. Once the last batch
    /// is answered the push has finished: `links` (what its scan saw) are recorded in the checkpoint after that
    /// commit, or in one of their own when there was nothing to commit.
    async fn commit_all(&mut self, ops: Vec<(Op, Effect)>, report: &mut Report, links: BTreeSet<RelPath>) -> Result<(), SyncError> {
        let mut finished = Some(links);
        let mut queue: std::collections::VecDeque<(Op, Effect)> = ops.into();
        let mut retried: BTreeSet<NodeId> = BTreeSet::new();
        while !queue.is_empty() {
            // At most the batch limit, and within the body limit (save records make operations bigger).
            let mut take = 0usize;
            let mut bytes = 0usize;
            for (op, _) in queue.iter().take(MAX_BATCH_OPS) {
                let weight = op_weight(op);
                if take > 0 && bytes.saturating_add(weight) > MAX_BATCH_JSON_BYTES {
                    break;
                }
                bytes = bytes.saturating_add(weight);
                take += 1;
            }
            let mut batch: Vec<(Op, Effect)> = queue.drain(..take).collect();
            loop {
                if batch.is_empty() {
                    break;
                }
                let only_ops: Vec<Op> = batch.iter().map(|(op, _)| op.clone()).collect();
                // The files this batch deletes are written down before it goes: if the answer never arrives (a crash,
                // a cut connection), the next pass knows those deletes are its own (see `State::sent_deletes`).
                let deletes: Vec<(NodeId, VersionId)> = only_ops
                    .iter()
                    .filter_map(|op| match op {
                        Op::Delete { node, expected: Some(version) } => Some((*node, *version)),
                        _ => None,
                    })
                    .collect();
                if !deletes.is_empty() {
                    self.state.sent_deletes.extend(deletes.iter().copied());
                    self.checkpoint()?;
                }
                let result = self.transport.commit(&only_ops).await;
                // Applied or refused as a whole, the batch's deletes are no longer in doubt; any other answer leaves
                // them written down.
                if matches!(result, Ok(_) | Err(TransportError::Rejected(_))) {
                    for (node, _) in &deletes {
                        self.state.sent_deletes.remove(node);
                    }
                }
                match result {
                    Ok(_) => {
                        for (_, effect) in batch.drain(..) {
                            self.record(effect, report);
                        }
                        if queue.is_empty()
                            && let Some(links) = finished.take()
                        {
                            self.finish_push(links);
                        }
                        self.checkpoint()?;
                        break;
                    }
                    Err(TransportError::Rejected(conflicts)) => {
                        // A conflict naming an operation the batch doesn't have is the server's mistake: ignored.
                        let conflicts: Vec<Conflict> = conflicts.into_iter().filter(|c| c.op < batch.len()).collect();
                        // An edit to a file the server deleted wins, but the server needs its current version
                        // as `expected` (it may have been edited elsewhere before the delete, which the change
                        // feed only reports as the final tombstone). Retry those Updates once with it.
                        let mut fixable: HashMap<usize, VersionId> = HashMap::new();
                        for c in &conflicts {
                            if let craft_cloud_proto::ConflictKind::VersionMismatch { current: Some(current) } = &c.kind
                                && let Some((Op::Update { node, .. }, _)) = batch.get(c.op)
                                && !self.state.remote.contains_key(node)
                                && retried.insert(*node)
                            {
                                fixable.insert(c.op, *current);
                            }
                        }
                        // A move onto a name the server already uses: move our copy aside, so the next pass moves
                        // it to the conflict name instead.
                        for c in &conflicts {
                            if let craft_cloud_proto::ConflictKind::NameTaken { .. } = c.kind
                                && let Some((Op::Move { .. }, Effect::Move { to, .. })) = batch.get(c.op)
                                && self.stat(to)?.is_some()
                            {
                                let to = to.clone();
                                let result = self.move_aside(&to, report).map(|_| ());
                                per_node(result, report)?;
                            }
                        }
                        let mut refused: BTreeSet<usize> = conflicts.iter().map(|c| c.op).filter(|i| !fixable.contains_key(i)).collect();
                        // A refused delete keeps its node tracked, so the folders above it stay too: their deletes
                        // are dropped with it (in this batch and later ones), or the node would be left tracked
                        // under a folder the server no longer has. They go once the node is settled.
                        let mut kept_dirs: BTreeSet<NodeId> = BTreeSet::new();
                        for i in &refused {
                            if let Some((Op::Delete { node, .. }, _)) = batch.get(*i) {
                                let mut current = self.state.base.get(node).and_then(|e| e.parent);
                                for _ in 0..=MAX_TREE_DEPTH {
                                    let Some(parent) = current else { break };
                                    if !kept_dirs.insert(parent) {
                                        break;
                                    }
                                    current = self.state.base.get(&parent).and_then(|e| e.parent);
                                }
                            }
                        }
                        let keeps = |op: &Op| matches!(op, Op::Delete { node, expected: None } if kept_dirs.contains(node));
                        refused.extend(batch.iter().enumerate().filter(|(_, (op, _))| keeps(op)).map(|(i, _)| i));
                        queue.retain(|(op, _)| !keeps(op));
                        // An edit to a file the server deleted that still can't go up (its folder can't come back
                        // where it was, say), or a move of such a file (which can never apply): forget the node, and
                        // the next pass uploads the file as a new one.
                        let mut forgot = false;
                        for i in &refused {
                            let node = match batch.get(*i) {
                                Some((Op::Update { node, .. }, _)) => Some(node),
                                Some((Op::Move { node, .. }, _))
                                    if matches!(self.state.base.get(node).map(|e| &e.kind), Some(BaseKind::File { .. })) =>
                                {
                                    Some(node)
                                }
                                _ => None,
                            };
                            if let Some(node) = node
                                && !self.state.remote.contains_key(node)
                            {
                                self.state.base.remove(node);
                                forgot = true;
                            }
                        }
                        if forgot {
                            self.forget_dead_dirs();
                        }
                        report.refused.extend(conflicts.into_iter().filter(|c| !fixable.contains_key(&c.op)));
                        let mut index = 0;
                        batch.retain_mut(|(op, _)| {
                            if let (Some(current), Op::Update { expected, .. }) = (fixable.get(&index), &mut *op) {
                                *expected = *current;
                            }
                            let keep = !refused.contains(&index);
                            index += 1;
                            keep
                        });
                        // Nothing to drop or fix: resubmitting would be refused the same way forever.
                        // That is the server speaking out of turn (not damaged data): a protocol error.
                        if refused.is_empty() && fixable.is_empty() {
                            return Err(TransportError::Protocol("the server refused a batch without naming an operation in it".into()).into());
                        }
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }
        if let Some(links) = finished.take()
            && self.finish_push(links)
        {
            self.checkpoint()?;
        }
        Ok(())
    }

    /// Forgets folders the server deleted that no longer hold a tracked node, so the push creates them afresh.
    fn forget_dead_dirs(&mut self) {
        loop {
            let parents: BTreeSet<NodeId> = self.state.base.values().filter_map(|e| e.parent).collect();
            let dead: Vec<NodeId> = self
                .state
                .base
                .iter()
                .filter(|(id, e)| Some(**id) != self.state.root && matches!(e.kind, BaseKind::Dir { .. }))
                .filter(|(id, _)| !self.state.remote.contains_key(id) && !parents.contains(id))
                .map(|(id, _)| *id)
                .collect();
            if dead.is_empty() {
                return;
            }
            for id in dead {
                self.state.base.remove(&id);
            }
        }
    }

    /// Records a committed operation in the base and the remote mirror.
    fn record(&mut self, effect: Effect, report: &mut Report) {
        match effect {
            Effect::Mkdir { id, parent, name, local_id, path } => {
                self.state.remote.insert(id, RemoteNode { parent: Some(parent), name: Some(name.clone()), file: None });
                self.state.base.insert(id, BaseEntry { parent: Some(parent), name: Some(name), kind: BaseKind::Dir { local_id } });
                report.created_dirs.push(path);
            }
            Effect::File { id, parent, name, version, size, meta, hash } => {
                let epoch = self.state.epoch.unwrap_or_else(Epoch::generate);
                let file = FileState {
                    version: version.id,
                    manifest: version.manifest,
                    size,
                    mtime_ms: version.mtime_ms,
                    exec: version.exec,
                    device: self.config.device,
                    epoch,
                    save: version.save.clone(),
                };
                self.state.remote.insert(id, RemoteNode { parent: Some(parent), name: Some(name.clone()), file: Some(file.clone()) });
                self.state.base.insert(id, BaseEntry { parent: Some(parent), name: Some(name), kind: self.base_file(&file, meta, hash) });
            }
            Effect::Update { id, version, size, meta, hash } => {
                let epoch = self.state.epoch.unwrap_or_else(Epoch::generate);
                let file = FileState {
                    version: version.id,
                    manifest: version.manifest,
                    size,
                    mtime_ms: version.mtime_ms,
                    exec: version.exec,
                    device: self.config.device,
                    epoch,
                    save: version.save.clone(),
                };
                if let Some(r) = self.state.remote.get_mut(&id) {
                    r.file = Some(file.clone());
                }
                let kind = self.base_file(&file, meta, hash);
                self.state.base.update(&id, |b| b.kind = kind);
            }
            Effect::Move { id, parent, name, from, to } => {
                if let Some(r) = self.state.remote.get_mut(&id) {
                    r.parent = Some(parent);
                    r.name = Some(name.clone());
                }
                self.state.base.update(&id, |b| {
                    b.parent = Some(parent);
                    b.name = Some(name);
                });
                report.moved_remotely.push((from, to));
            }
            Effect::Delete { id, path } => {
                self.state.remote.remove(&id);
                self.state.base.remove(&id);
                report.removed_remotely.push(path);
                report.removed_nodes.push(id);
            }
        }
    }
}
