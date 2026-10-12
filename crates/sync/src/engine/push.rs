//! Turning the folder's changes into uploads and batch commits.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;

use bytes::Bytes;
use craft_cloud_chunker::{FASTCDC_V1, chunk_reader};
use craft_cloud_proto::limits::{MAX_BATCH_OPS, MAX_TREE_DEPTH};
use craft_cloud_proto::{ChunkId, ChunkRef, ChunkerParamsId, Conflict, Epoch, FileHash, FileState, Manifest, NewVersion, NodeId, Op, VersionId};

use crate::fs::{FileMeta, Fs, FsKind};
use crate::path::RelPath;
use crate::state::{BaseEntry, BaseKind, RemoteNode};
use crate::transport::{Transport, TransportError};

use super::{Effect, Engine, MISSING_QUERY_IDS, Report, SyncError, per_node};

impl<F: Fs, T: Transport> Engine<F, T> {
    pub(super) async fn push(&mut self, report: &mut Report) -> Result<(), SyncError> {
        let root = self.state.root.ok_or_else(|| SyncError::Corrupt("no root".into()))?;
        let (local, unseen) = self.scan(report)?;
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
            if *id == root || matches!(local.get(path), Some(FsKind::Dir { .. })) {
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
                (BaseKind::File { .. }, Some(FsKind::File(m))) => {
                    if !self.file_matches(entry, &now, m)? {
                        modified.push((*id, now, *m));
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
        // A folder above a missing node that the user moved is found by its local ID even where a new folder took
        // its old name (so it isn't a folder move here): a link the node turned into moved with it.
        let dir_at: HashMap<u64, &RelPath> = local
            .iter()
            .filter_map(|(p, k)| match k {
                FsKind::Dir { id: Some(i) } => Some((*i, p)),
                _ => None,
            })
            .collect();
        let behind_moved_link = |id: NodeId| -> bool {
            let mut below = Vec::new();
            let mut current = id;
            for _ in 0..=MAX_TREE_DEPTH {
                let Some(BaseEntry { name: Some(name), parent: Some(parent), .. }) = self.state.base.get(&current) else { return false };
                below.push(name.clone());
                current = *parent;
                if let Some(BaseEntry { kind: BaseKind::Dir { local_id: Some(lid) }, .. }) = self.state.base.get(&current)
                    && let Some(now) = dir_at.get(lid)
                    && hidden(&below.iter().rev().fold((*now).clone(), |p, n| p.join(n)))
                {
                    return true;
                }
            }
            false
        };
        missing_files.retain(|(id, path)| !hidden(&expected(path)) && !behind_moved_link(*id));
        missing_dirs.retain(|(id, path)| !hidden(&expected(path)) && !behind_moved_link(*id));
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
        // Edits.
        for (id, path, _) in &modified {
            let Some(BaseEntry { kind: BaseKind::File { version: expected, .. }, .. }) = self.state.base.get(id).cloned() else { continue };
            let Some((version, size, meta, hash)) = self.upload(path, report).await? else { continue };
            ops.push((Op::Update { node: *id, expected, version: version.clone() }, Effect::Update { id: *id, version, size, meta, hash }));
        }
        // New files.
        for (path, kind) in &new_paths {
            if !matches!(kind, FsKind::File(_)) {
                continue;
            }
            let (Some(name), Some(parent)) = (path.file_name().cloned(), parent_id(path, &new_ids)) else { continue };
            let Some((version, size, meta, hash)) = self.upload(path, report).await? else { continue };
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

        self.commit_all(ops, report).await
    }

    /// Uploads a file's chunks and manifest. `None` when the file changed while being read (it is retried
    /// next pass).
    /// Uploads a file's content: chunks it as a stream, asks which chunks the server lacks, and sends those,
    /// read back by range and checked against their IDs. `None` when the file can't be uploaded this pass:
    /// it changed while being read (tried again next pass) or it can't be read at all (reported as skipped).
    async fn upload(&mut self, path: &RelPath, report: &mut Report) -> Result<Option<(NewVersion, u64, FileMeta, FileHash)>, SyncError> {
        let Some(FsKind::File(before)) = self.stat(path)? else { return Ok(None) };
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
            if missing.remove(id) {
                let data = match self.fs.read_at(path, *offset, *len) {
                    Ok(data) => data,
                    Err(e) => {
                        report.skipped.push((path.to_string(), format!("can't be read: {e}")));
                        return Ok(None);
                    }
                };
                // The file changed since it was chunked: try again next pass.
                if data.len() != *len || *blake3::hash(&data).as_bytes() != *id.as_bytes() {
                    return Ok(None);
                }
                self.transport.put_chunk(id, Bytes::from(data)).await?;
            }
        }
        let refs = summary.chunks.iter().map(|c| ChunkRef { id: ChunkId::from_bytes(c.hash), len: c.len }).collect();
        let params = ChunkerParamsId::new(FASTCDC_V1.name).map_err(|e| SyncError::Corrupt(e.to_string()))?;
        let manifest =
            Manifest::new(params, summary.size, FileHash::from_bytes(summary.file_hash), refs).map_err(|e| SyncError::Corrupt(e.to_string()))?;
        self.transport.put_manifest(&manifest.id(), Bytes::from(manifest.encode())).await?;
        report.uploaded.push(path.clone());
        let version = NewVersion { id: VersionId::generate(), manifest: manifest.id(), mtime_ms: before.mtime_ms, exec: before.exec };
        Ok(Some((version, summary.size, before, FileHash::from_bytes(summary.file_hash))))
    }

    /// Commits operations in batches. A refused batch is retried without its refused operations (their
    /// dependants are usually refused with them); refused operations are tried again next pass.
    async fn commit_all(&mut self, ops: Vec<(Op, Effect)>, report: &mut Report) -> Result<(), SyncError> {
        let mut queue: std::collections::VecDeque<(Op, Effect)> = ops.into();
        let mut retried: BTreeSet<NodeId> = BTreeSet::new();
        while !queue.is_empty() {
            let take = queue.len().min(MAX_BATCH_OPS);
            let mut batch: Vec<(Op, Effect)> = queue.drain(..take).collect();
            loop {
                if batch.is_empty() {
                    break;
                }
                let only_ops: Vec<Op> = batch.iter().map(|(op, _)| op.clone()).collect();
                let result = self.transport.commit(&only_ops).await;
                match result {
                    Ok(_) => {
                        for (_, effect) in batch.drain(..) {
                            self.record(effect, report);
                        }
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
                        let refused: BTreeSet<usize> = conflicts.iter().map(|c| c.op).filter(|i| !fixable.contains_key(i)).collect();
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
                        if refused.is_empty() && fixable.is_empty() {
                            return Err(SyncError::Corrupt("the server refused a batch without naming an operation in it".into()));
                        }
                    }
                    Err(e) => return Err(e.into()),
                }
            }
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
                };
                if let Some(r) = self.state.remote.get_mut(&id) {
                    r.file = Some(file.clone());
                }
                let kind = self.base_file(&file, meta, hash);
                if let Some(b) = self.state.base.get_mut(&id) {
                    b.kind = kind;
                }
            }
            Effect::Move { id, parent, name, from, to } => {
                if let Some(r) = self.state.remote.get_mut(&id) {
                    r.parent = Some(parent);
                    r.name = Some(name.clone());
                }
                if let Some(b) = self.state.base.get_mut(&id) {
                    b.parent = Some(parent);
                    b.name = Some(name);
                }
                report.moved_remotely.push((from, to));
            }
            Effect::Delete { id, path } => {
                self.state.remote.remove(&id);
                self.state.base.remove(&id);
                report.removed_remotely.push(path);
            }
        }
    }
}
