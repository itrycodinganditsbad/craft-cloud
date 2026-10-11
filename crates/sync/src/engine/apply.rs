//! Applying the server's changes to the folder.

use craft_cloud_proto::limits::MAX_TREE_DEPTH;
use craft_cloud_proto::{Name, NodeId};

use crate::fs::{Fs, FsKind};
use crate::path::RelPath;
use crate::state::{BaseEntry, BaseKind, RemoteNode};
use crate::transport::Transport;

use super::{Engine, Local, Mode, Report, SyncError, io_err};

impl<F: Fs, T: Transport> Engine<F, T> {
    pub(super) async fn apply_remote(&mut self, mode: Mode, report: &mut Report) -> Result<(), SyncError> {
        let root = self.state.root.ok_or_else(|| SyncError::Corrupt("no root".into()))?;
        // Changed or new remote nodes, parents first.
        let mut work: Vec<(usize, NodeId)> =
            self.state.remote.keys().filter(|id| **id != root).filter_map(|id| Some((self.state.remote_path(*id)?.depth(), *id))).collect();
        work.sort();
        for (_, id) in work {
            if self.state.base.contains_key(&id) {
                self.apply_changed(id, report).await?;
            } else {
                self.apply_new(id, mode, report).await?;
            }
        }
        // Nodes the server deleted: files first, then folders deepest first.
        let mut gone: Vec<(bool, usize, NodeId)> = self
            .state
            .base
            .iter()
            .filter(|(id, _)| **id != root && !self.state.remote.contains_key(id))
            .map(|(id, e)| (matches!(e.kind, BaseKind::Dir { .. }), self.state.base_path(*id).map_or(0, |p| p.depth()), *id))
            .collect();
        gone.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        for (_, _, id) in gone {
            self.apply_deleted(id, report)?;
        }
        Ok(())
    }

    async fn apply_changed(&mut self, id: NodeId, report: &mut Report) -> Result<(), SyncError> {
        let (Some(entry), Some(remote)) = (self.state.base.get(&id).cloned(), self.state.remote.get(&id).cloned()) else { return Ok(()) };
        let Some(rpath) = self.state.remote_path(id) else { return Ok(()) };
        // No base path: the folder it was tracked in was let go this pass (moved aside as a conflict copy, say).
        // It is then looked up by local ID, like a node moved on both sides.
        let base_path = self.state.base_path(id);
        let new_version = match (&entry.kind, &remote.file) {
            (BaseKind::File { version, .. }, Some(f)) => *version != f.version,
            (BaseKind::Dir { .. }, None) => false,
            // A node can't change kind; treat it as damaged state and leave it.
            _ => return Ok(()),
        };
        let moved = base_path.as_ref() != Some(&rpath);
        if !moved && !new_version {
            return Ok(());
        }
        let Some(bpath) = base_path else { return self.apply_missing(id, entry, remote, rpath, new_version, report).await };
        let mut local = self.local_state(&entry, &bpath)?;
        if local == Local::Unchanged && new_version && !self.really_unchanged(&entry, &bpath)? {
            local = Local::Modified;
        }
        let mut new_entry = BaseEntry { parent: remote.parent, name: remote.name.clone(), kind: entry.kind.clone() };
        match local {
            Local::Unchanged | Local::Modified if new_version && local == Local::Modified => {
                // Both changed: keep ours as a conflict copy, take theirs.
                self.move_aside(&bpath, report)?;
                self.ensure_free(&rpath, report)?;
                if let Some(file) = &remote.file {
                    let (meta, hash) = self.download(file, &rpath, report).await?;
                    new_entry.kind = self.base_file(file, meta, hash);
                }
            }
            Local::Unchanged | Local::Modified => {
                if moved {
                    self.relocate(&bpath, &rpath, report)?;
                }
                if new_version && let Some(file) = &remote.file {
                    let (meta, hash) = self.download(file, &rpath, report).await?;
                    new_entry.kind = self.base_file(file, meta, hash);
                }
            }
            Local::Missing => return self.apply_missing(id, entry, remote, rpath, new_version, report).await,
        }
        self.state.base.insert(id, new_entry);
        Ok(())
    }

    /// A tracked node changed on the server and isn't where the base has it here (or the base lost its place).
    async fn apply_missing(
        &mut self,
        id: NodeId,
        entry: BaseEntry,
        remote: RemoteNode,
        rpath: RelPath,
        new_version: bool,
        report: &mut Report,
    ) -> Result<(), SyncError> {
        let moved = self.state.base_path(id).as_ref() != Some(&rpath);
        let mut new_entry = BaseEntry { parent: remote.parent, name: remote.name.clone(), kind: entry.kind.clone() };
        if !(new_version && remote.file.is_some()) {
            // Moved on both sides: the server's move stands, so our copy goes where the server has it.
            let local_id = match entry.kind {
                BaseKind::Dir { local_id } | BaseKind::File { local_id, .. } => local_id,
            };
            if moved
                && let Some(lid) = local_id
                && let Some(found) = self.find_local(lid)?
            {
                self.relocate(&found, &rpath, report)?;
            } else if moved
                && let BaseKind::Dir { .. } = entry.kind
                && let Some(FsKind::Dir { id: here }) = self.stat(&rpath)?
                && !self.owns_local_dir(here, id)
            {
                // A folder sits where the server moved this one, and no other tracked folder claims it:
                // it is this folder (folders hold no content of their own).
                new_entry.kind = BaseKind::Dir { local_id: here };
            } else if !moved {
                // Deleted here: the push sends the delete.
                return Ok(());
            }
            // Otherwise it was deleted here too, and the push sends the delete; the base follows the server's
            // location so it stays reachable if its old parent goes away.
            self.state.base.insert(id, new_entry);
            return Ok(());
        }
        // Edited there, deleted here: the edit wins.
        if let Some(file) = &remote.file {
            self.ensure_free(&rpath, report)?;
            let (meta, hash) = self.download(file, &rpath, report).await?;
            new_entry.kind = self.base_file(file, meta, hash);
        }
        self.state.base.insert(id, new_entry);
        Ok(())
    }

    /// Makes sure nothing untracked sits at `path` (moving it aside) and that its parent exists.
    /// Something in `path`'s folder that differs from `path` only by case or normalisation. On Windows and
    /// macOS that is the same file, so writing `path` would overwrite it.
    fn case_twin(&self, path: &RelPath) -> Result<Option<RelPath>, SyncError> {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else { return Ok(None) };
        if !matches!(self.stat(&parent)?, Some(FsKind::Dir { .. })) {
            return Ok(None);
        }
        let key = name.fold_key();
        for entry in self.fs.list(&parent).map_err(|e| io_err(&parent, e))? {
            if let Ok(other) = Name::new(&entry.name)
                && other != *name
                && other.fold_key() == key
            {
                return Ok(Some(parent.join(&other)));
            }
        }
        Ok(None)
    }

    /// If another tracked node (not `newcomer`) still occupies `path`, settle its claim before the newcomer
    /// takes the path: deleted on the server and unchanged here → removed; deleted there but edited here →
    /// kept as a conflict copy (uploaded as a new file); still alive elsewhere on the server → moved aside
    /// (its own move puts it in place later).
    fn release_tracked(&mut self, path: &RelPath, newcomer: NodeId, report: &mut Report) -> Result<(), SyncError> {
        let Some(other) = self.state.base_index().get(path).copied() else { return Ok(()) };
        if other == newcomer || Some(other) == self.state.root {
            return Ok(());
        }
        let Some(entry) = self.state.base.get(&other).cloned() else { return Ok(()) };
        let local = self.local_state(&entry, path)?;
        if local == Local::Missing {
            return Ok(());
        }
        if self.state.remote.contains_key(&other) {
            self.move_aside(path, report)?;
            return Ok(());
        }
        match (&entry.kind, local) {
            (BaseKind::File { .. }, Local::Unchanged) if self.really_unchanged(&entry, path)? => {
                self.fs.remove_file(path).map_err(|e| io_err(path, e))?;
                report.removed_locally.push(path.clone());
            }
            _ => {
                self.move_aside(path, report)?;
            }
        }
        self.state.base.remove(&other);
        Ok(())
    }

    /// Moves `from` to `to`, moving aside anything at `to`. When one path contains the other (the target is
    /// an untracked folder holding the source, or the reverse), the source hops through a temporary
    /// conflict-copy name first.
    fn relocate(&mut self, from: &RelPath, to: &RelPath, report: &mut Report) -> Result<(), SyncError> {
        if from == to {
            return Ok(());
        }
        let mut source = from.clone();
        // A case-only rename (`a.txt` → `A.TXT`): the source is the target's case twin, so it must not be
        // moved aside as an occupant.
        let case_only = from.parent() == to.parent() && from.file_name().map(Name::fold_key) == to.file_name().map(Name::fold_key);
        if from.starts_with(to) || to.starts_with(from) || case_only {
            // The hop sits next to the shallower of the two paths, outside both.
            let outer = if from.starts_with(to) && !case_only { to } else { from };
            let is_dir = matches!(self.stat(from)?, Some(FsKind::Dir { .. }));
            let hop = self.conflict_path(outer, is_dir)?;
            self.fs.rename(from, &hop).map_err(|e| io_err(from, e))?;
            source = hop;
        }
        self.ensure_free(to, report)?;
        self.fs.rename(&source, to).map_err(|e| io_err(&source, e))?;
        report.moved_locally.push((from.clone(), to.clone()));
        Ok(())
    }

    /// Where the file or folder with local ID `local_id` is now, if anywhere in the folder.
    fn find_local(&self, local_id: u64) -> Result<Option<RelPath>, SyncError> {
        let mut queue = vec![RelPath::root()];
        while let Some(dir) = queue.pop() {
            for entry in self.fs.list(&dir).map_err(|e| io_err(&dir, e))? {
                let Ok(name) = Name::new(&entry.name) else { continue };
                let path = dir.join(&name);
                match entry.kind {
                    FsKind::Dir { id } => {
                        if id == Some(local_id) {
                            return Ok(Some(path));
                        }
                        if path.depth() <= MAX_TREE_DEPTH {
                            queue.push(path);
                        }
                    }
                    FsKind::File(m) if m.id == Some(local_id) => return Ok(Some(path)),
                    _ => {}
                }
            }
        }
        Ok(None)
    }

    /// Creates `dir` and any missing folders above it, moving aside anything in the way.
    fn make_dirs(&mut self, dir: &RelPath, report: &mut Report) -> Result<(), SyncError> {
        let mut current = RelPath::root();
        for name in dir.names() {
            current = current.join(name);
            match self.stat(&current)? {
                Some(FsKind::Dir { .. }) => continue,
                Some(_) => {
                    self.move_aside(&current, report)?;
                }
                None => {
                    if let Some(twin) = self.case_twin(&current)? {
                        self.move_aside(&twin, report)?;
                    }
                }
            }
            self.fs.mkdir(&current).map_err(|e| io_err(&current, e))?;
            report.created_dirs.push(current.clone());
            // If a tracked folder lived here, it is this folder again: remember its new local ID.
            let new_id = Self::dir_id(self.stat(&current)?);
            if let Some(node) = self.state.base_index().get(&current).copied()
                && let Some(BaseEntry { kind: BaseKind::Dir { local_id }, .. }) = self.state.base.get_mut(&node)
            {
                *local_id = new_id;
            }
        }
        Ok(())
    }

    /// Makes sure nothing sits at `path`, not even a name that differs only by case: whatever is there is
    /// moved aside as a conflict copy.
    fn ensure_free(&mut self, path: &RelPath, report: &mut Report) -> Result<(), SyncError> {
        if let Some(parent) = path.parent() {
            self.make_dirs(&parent, report)?;
        }
        if let Some(twin) = self.case_twin(path)? {
            self.move_aside(&twin, report)?;
        }
        if self.stat(path)?.is_some() {
            self.move_aside(path, report)?;
        }
        Ok(())
    }

    async fn apply_new(&mut self, id: NodeId, mode: Mode, report: &mut Report) -> Result<(), SyncError> {
        let Some(remote) = self.state.remote.get(&id).cloned() else { return Ok(()) };
        let Some(rpath) = self.state.remote_path(id) else { return Ok(()) };
        let Some(parent_path) = rpath.parent() else { return Ok(()) };
        // New content inside a folder deleted here brings the folder back (a create wins over a delete).
        self.make_dirs(&parent_path, report)?;
        if let Some(twin) = self.case_twin(&rpath)? {
            self.release_tracked(&twin, id, report)?;
            if self.stat(&twin)?.is_some() {
                self.move_aside(&twin, report)?;
            }
        }
        self.release_tracked(&rpath, id, report)?;
        let here = self.stat(&rpath)?;
        let kind = match (&remote.file, here) {
            // A folder already here is adopted, unless it is another tracked folder that the user moved here
            // (same local ID): that one is moved aside so its own move can be sent.
            (None, Some(FsKind::Dir { id: local_id })) if !self.owns_local_dir(local_id, id) => BaseKind::Dir { local_id },
            (None, other) => {
                if other.is_some() {
                    self.move_aside(&rpath, report)?;
                }
                self.fs.mkdir(&rpath).map_err(|e| io_err(&rpath, e))?;
                report.created_dirs.push(rpath.clone());
                BaseKind::Dir { local_id: Self::dir_id(self.stat(&rpath)?) }
            }
            (Some(file), None) => {
                let (meta, hash) = self.download(file, &rpath, report).await?;
                self.base_file(file, meta, hash)
            }
            (Some(file), Some(FsKind::File(meta))) => {
                let manifest = self.fetch_manifest(&file.manifest).await?;
                if self.content_hash(&rpath) == Some(*manifest.file_hash()) {
                    // Same content already here: adopt it.
                    self.base_file(file, meta, *manifest.file_hash())
                } else if mode == Mode::Reconcile && Some(file.epoch) != self.state.epoch {
                    // After a restore, a file the server only knows from before keeps our newer copy:
                    // recorded as modified, so the push uploads it as the next version.
                    BaseKind::File {
                        version: file.version,
                        manifest: file.manifest,
                        size: meta.size,
                        mtime_ms: i64::MIN,
                        exec: file.exec,
                        local_id: meta.id,
                        hash: *manifest.file_hash(),
                        seen_ms: self.fs.now_ms(),
                    }
                } else {
                    self.move_aside(&rpath, report)?;
                    let (meta, hash) = self.download(file, &rpath, report).await?;
                    self.base_file(file, meta, hash)
                }
            }
            (Some(file), Some(_)) => {
                self.move_aside(&rpath, report)?;
                let (meta, hash) = self.download(file, &rpath, report).await?;
                self.base_file(file, meta, hash)
            }
        };
        self.state.base.insert(id, BaseEntry { parent: remote.parent, name: remote.name, kind });
        Ok(())
    }

    fn apply_deleted(&mut self, id: NodeId, report: &mut Report) -> Result<(), SyncError> {
        let Some(entry) = self.state.base.get(&id).cloned() else { return Ok(()) };
        let Some(bpath) = self.state.base_path(id) else {
            self.state.base.remove(&id);
            return Ok(());
        };
        match self.local_state(&entry, &bpath)? {
            Local::Missing => {
                self.state.base.remove(&id);
            }
            Local::Unchanged => match entry.kind {
                // Content edited here after all (same size and time by accident): the edit wins.
                BaseKind::File { .. } if !self.really_unchanged(&entry, &bpath)? => {}
                BaseKind::File { .. } => {
                    self.fs.remove_file(&bpath).map_err(|e| io_err(&bpath, e))?;
                    self.state.base.remove(&id);
                    report.removed_locally.push(bpath);
                }
                BaseKind::Dir { .. } => {
                    // Only an empty folder goes; one with new local files stays, and the push brings it back.
                    let empty = self.fs.list(&bpath).map_err(|e| io_err(&bpath, e))?.iter().all(|e| self.config.ignored(&e.name));
                    if empty {
                        self.fs.remove_dir(&bpath).map_err(|e| io_err(&bpath, e))?;
                        self.state.base.remove(&id);
                        report.removed_locally.push(bpath);
                    } else if !self.state.base.values().any(|e| e.parent == Some(id)) {
                        // Only new files inside: forget the folder, and the push creates it afresh. (One that
                        // still holds an edited tracked file stays; that file's upload brings the folder back.)
                        self.state.base.remove(&id);
                    }
                }
            },
            // Edited here, deleted there: the edit wins (the push uploads it, the server restores the node).
            Local::Modified => {}
        }
        Ok(())
    }
}
