//! Applying the server's changes to the folder.

use std::collections::{BTreeSet, HashMap};

use craft_cloud_proto::limits::MAX_TREE_DEPTH;
use craft_cloud_proto::{FileState, FoldKey, Name, NodeId};

use crate::fs::{Fs, FsKind};
use crate::path::RelPath;
use crate::state::{BaseEntry, BaseKind, RemoteNode};
use crate::transport::Transport;

use super::{Engine, Local, Mode, OTHER_REASON, Report, SyncError, io_err, per_node};

impl<F: Fs, T: Transport> Engine<F, T> {
    pub(super) async fn apply_remote(&mut self, mode: Mode, report: &mut Report) -> Result<(), SyncError> {
        let root = self.state.root.ok_or_else(|| SyncError::Corrupt("no root".into()))?;
        // Changed or new remote nodes, parents first.
        let mut work: Vec<(usize, NodeId)> =
            self.state.remote.keys().filter(|id| **id != root).filter_map(|id| Some((self.state.remote_path(*id)?.depth(), *id))).collect();
        work.sort();
        // Nodes skipped this pass for a filesystem error; everything under one waits for it too.
        let mut skipped: BTreeSet<NodeId> = BTreeSet::new();
        // Whether a path is a link or special file here (stat'ed at most once per pass).
        let mut other: HashMap<RelPath, bool> = HashMap::new();
        let mut cached_at = self.fs_changes;
        let mut reported: BTreeSet<RelPath> = BTreeSet::new();
        for (_, id) in work {
            if self.state.remote.get(&id).and_then(|r| r.parent).is_some_and(|p| skipped.contains(&p)) {
                skipped.insert(id);
                continue;
            }
            // A change that would touch a link (a tracked file or folder the user replaced with one, or a folder
            // above it) waits: the link is never moved aside or written through, and the node keeps its base, so
            // the push doesn't take it for deleted. The change lands once it is a real file or folder again.
            if cached_at != self.fs_changes {
                other.clear();
                cached_at = self.fs_changes;
            }
            let waits = if self.changes_here(id) { self.link_on_the_way(id, &mut other) } else { Ok(None) };
            // A folder on the way that can't be looked into (held open) is a filesystem error on this node.
            let waits = match waits {
                Ok(waits) => waits,
                Err(e) => {
                    per_node(Err(e), report)?;
                    skipped.insert(id);
                    continue;
                }
            };
            if let Some(link) = waits {
                if reported.insert(link.clone()) {
                    report
                        .skipped
                        .push((link.to_string(), format!("{OTHER_REASON}; the server's change waits until it is a real file or folder again")));
                }
                skipped.insert(id);
                continue;
            }
            let result =
                if self.state.base.contains_key(&id) { self.apply_changed(id, report).await } else { self.apply_new(id, mode, report).await };
            if per_node(result, report)? {
                skipped.insert(id);
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
        // The mass-delete guard, this direction: too many tracked files to remove here wait for approval. They
        // stay on disk and in the base, unchanged, so nothing goes back up; their folders stay with them.
        let removable: Vec<(NodeId, RelPath)> = gone
            .iter()
            .filter(|(is_dir, _, _)| !is_dir)
            .filter_map(|(_, _, id)| {
                let path = self.state.base_path(*id)?;
                let entry = self.state.base.get(id)?;
                if self.no_link(&path).is_err() {
                    return None;
                }
                let local = self.local_state(entry, &path).ok()?;
                (local == Local::Unchanged || (local == Local::Modified && self.holds_base_content(entry, &path))).then_some((*id, path))
            })
            .collect();
        let synced_files = self.state.base.values().filter(|e| matches!(e.kind, BaseKind::File { .. })).count();
        let mut held: BTreeSet<NodeId> = BTreeSet::new();
        if self.config.too_many_deletes(removable.len(), synced_files) {
            for (id, path) in removable {
                if !self.approval.covers(id) {
                    held.insert(id);
                    report.pending_local_deletes.push(path);
                    report.pending_nodes.push(id);
                }
            }
        }
        for (_, _, id) in gone {
            if held.contains(&id) {
                continue;
            }
            // A delete under a link waits too: the node keeps its base entry (it isn't forgotten), so the delete
            // lands here once the folder is real again, instead of the old file coming back as new.
            if cached_at != self.fs_changes {
                other.clear();
                cached_at = self.fs_changes;
            }
            let waits = match self.link_on_the_way(id, &mut other) {
                Ok(waits) => waits,
                Err(e) => {
                    per_node(Err(e), report)?;
                    continue;
                }
            };
            if let Some(link) = waits {
                if reported.insert(link.clone()) {
                    report
                        .skipped
                        .push((link.to_string(), format!("{OTHER_REASON}; the server's change waits until it is a real file or folder again")));
                }
                continue;
            }
            let result = self.apply_deleted(id, report);
            per_node(result, report)?;
        }
        Ok(())
    }

    /// Whether applying the server's state of `id` would change anything here: a new node, or a tracked one the
    /// server moved or gave a new version.
    fn changes_here(&self, id: NodeId) -> bool {
        let (Some(entry), Some(remote)) = (self.state.base.get(&id), self.state.remote.get(&id)) else { return true };
        let new_version = match (&entry.kind, &remote.file) {
            (BaseKind::File { version, .. }, Some(f)) => *version != f.version,
            _ => false,
        };
        new_version || self.state.base_path(id) != self.state.remote_path(id)
    }

    /// The first link or special file (an `Other` entry) at or above the node's base path or its path on the
    /// server, if any.
    fn link_on_the_way(&self, id: NodeId, cache: &mut HashMap<RelPath, bool>) -> Result<Option<RelPath>, SyncError> {
        for path in [self.state.base_path(id), self.state.remote_path(id)].into_iter().flatten() {
            let mut prefix = RelPath::root();
            for name in path.names() {
                prefix = prefix.join(name);
                let kind = match cache.get(&prefix) {
                    Some(is_other) => *is_other,
                    None => {
                        let here = self.stat(&prefix)?;
                        let is_other = matches!(here, Some(FsKind::Other));
                        cache.insert(prefix.clone(), is_other);
                        if here.is_none() {
                            // Nothing here, so nothing below it either.
                            break;
                        }
                        is_other
                    }
                };
                if kind {
                    return Ok(Some(prefix));
                }
            }
        }
        // Landing at the server's path would move a linked case twin of it (or of a folder above it) aside.
        if let Some(rpath) = self.state.remote_path(id)
            && let Some(link) = self.link_toward(&rpath)?
        {
            return Ok(Some(link));
        }
        Ok(None)
    }

    /// A link or special file that taking `to` would have to act on: `to` itself, a folder above it, or a case
    /// twin of any of them. Checked before the first step of a change that takes `to`, so a change that can't
    /// complete never starts (no file is left halfway, at a temporary name).
    fn link_toward(&self, to: &RelPath) -> Result<Option<RelPath>, SyncError> {
        let mut prefix = RelPath::root();
        for name in to.names() {
            prefix = prefix.join(name);
            if let Some(twin) = self.linked_twin(&prefix)? {
                return Ok(Some(twin));
            }
            match self.stat(&prefix)? {
                Some(FsKind::Other) => return Ok(Some(prefix)),
                Some(FsKind::Dir { .. }) => {}
                // A file or nothing: nothing further down exists to check.
                _ => return Ok(None),
            }
        }
        Ok(None)
    }

    /// A link or special file in `path`'s folder whose name differs from `path`'s only by case or normalisation.
    fn linked_twin(&self, path: &RelPath) -> Result<Option<RelPath>, SyncError> {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else { return Ok(None) };
        if !matches!(self.stat(&parent)?, Some(FsKind::Dir { .. })) {
            return Ok(None);
        }
        let key = name.fold_key();
        // A folder that can't be listed hides no twin from this check: whatever then acts on it lists it too, and
        // fails on this node.
        let Ok(entries) = self.fs.list(&parent) else { return Ok(None) };
        for entry in entries {
            if entry.kind == FsKind::Other
                && let Ok(other) = Name::new(&entry.name)
                && other != *name
                && other.fold_key() == key
            {
                return Ok(Some(parent.join(&other)));
            }
        }
        Ok(None)
    }

    /// Fails as a filesystem error on the node (it is skipped and waits) if taking `to` would act on a link.
    fn refuse_link_toward(&self, to: &RelPath) -> Result<(), SyncError> {
        match self.link_toward(to)? {
            Some(link) => Err(io_err(&link, std::io::Error::other(OTHER_REASON))),
            None => Ok(()),
        }
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
        // What the file looked like before it was verified: a download may only replace exactly that (a save
        // that lands while the file is hashed changes it).
        let checked = self.stat(&bpath)?;
        let mut local = self.local_state(&entry, &bpath)?;
        if local == Local::Unchanged && new_version && !self.really_unchanged(&entry, &bpath)? {
            local = Local::Modified;
        }
        let mut new_entry = BaseEntry { parent: remote.parent, name: remote.name.clone(), kind: entry.kind.clone() };
        match local {
            Local::Unchanged | Local::Modified if new_version && local == Local::Modified => {
                // The same content on both sides (our own commit whose reply was lost, or the same edit made on
                // two machines): no conflict, just agree on the server's version.
                if let Some(file) = &remote.file
                    && let Some((meta, hash)) = self.same_content(file, &bpath).await?
                {
                    if moved {
                        self.settle_occupants(id, &rpath, Some(&bpath), report).await?;
                        self.relocate(&bpath, &rpath, report)?;
                    }
                    new_entry.kind = self.base_file(file, meta, hash);
                    self.state.base.insert(id, new_entry);
                    return Ok(());
                }
                // Both changed: keep ours as a conflict copy, take theirs.
                self.settle_occupants(id, &rpath, Some(&bpath), report).await?;
                self.move_aside(&bpath, report)?;
                self.ensure_free(&rpath, report)?;
                // Saved here meanwhile: the base keeps the old version at the server's place, so the next pass
                // sees an edit on both sides again (and the conflict copy goes up as a new file).
                if let Some(file) = &remote.file
                    && let Some((meta, hash)) = self.download(file, &rpath, None, report).await?
                {
                    new_entry.kind = self.base_file(file, meta, hash);
                }
            }
            Local::Unchanged | Local::Modified => {
                if moved {
                    self.settle_occupants(id, &rpath, Some(&bpath), report).await?;
                    self.relocate(&bpath, &rpath, report)?;
                    // Recorded at once, so a download that fails below doesn't leave the base pointing where the
                    // file no longer is.
                    self.state.base.insert(id, new_entry.clone());
                }
                // Saved here meanwhile: the base keeps the old version (at its new place, where the file is), so
                // the next pass sees an edit on both sides. A rename keeps the file's ID and time, so the checked
                // state still describes it after a relocate.
                if new_version
                    && let Some(file) = &remote.file
                    && let Some((meta, hash)) = self.download(file, &rpath, checked, report).await?
                {
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
                self.settle_occupants(id, &rpath, Some(&found), report).await?;
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
            } else if let Some(link) = self.link_in_place_of(id)? {
                // It turned into a link, and the user moved its folder (the link went with it): it isn't gone,
                // and the server's move waits until it is a real file or folder again.
                return Err(io_err(&link, std::io::Error::other(OTHER_REASON)));
            }
            // Otherwise it was deleted here too, and the push sends the delete; the base follows the server's
            // location so it stays reachable if its old parent goes away.
            self.state.base.insert(id, new_entry);
            return Ok(());
        }
        let Some(file) = &remote.file else { return Ok(()) };
        // Edited there, moved here: the file (found by local ID, or through a folder the user moved) takes the
        // server's version. If the server moved it too, the server's move stands; otherwise it stays where the
        // user put it and the push sends the user's move.
        if let Some(found) = self.locate(id)? {
            let target = if moved { rpath.clone() } else { found.clone() };
            // What the file looked like before it was verified: a download may only replace exactly that.
            let checked = self.stat(&found)?;
            if self.really_unchanged(&entry, &found)? {
                if moved {
                    self.settle_occupants(id, &rpath, Some(&found), report).await?;
                    self.relocate(&found, &rpath, report)?;
                    self.state.base.insert(id, new_entry.clone());
                }
                // A rename keeps the file's ID and time, so the checked state still describes it.
                if let Some((meta, hash)) = self.download(file, &target, checked, report).await? {
                    new_entry.kind = self.base_file(file, meta, hash);
                }
            } else if let Some((meta, hash)) = self.same_content(file, &found).await? {
                // The same edit on both sides: no conflict.
                if moved {
                    self.settle_occupants(id, &rpath, Some(&found), report).await?;
                    self.relocate(&found, &rpath, report)?;
                }
                new_entry.kind = self.base_file(file, meta, hash);
            } else {
                // Edited on both sides: ours stays next to where the user put it, as a conflict copy.
                self.settle_occupants(id, &target, Some(&found), report).await?;
                self.move_aside(&found, report)?;
                self.ensure_free(&target, report)?;
                if let Some((meta, hash)) = self.download(file, &target, None, report).await? {
                    new_entry.kind = self.base_file(file, meta, hash);
                }
            }
            self.state.base.insert(id, new_entry);
            return Ok(());
        }
        // Edited there, deleted here: the edit wins.
        self.settle_occupants(id, &rpath, None, report).await?;
        // Already at the server's place with the server's content, untracked (the same move and edit made on both
        // sides, or put there again after a pass that crashed before saving its state): adopted, not copied.
        if !self.state.base_index().contains_key(&rpath)
            && let Some((meta, hash)) = self.same_content(file, &rpath).await?
        {
            if let Some(twin) = self.case_twin(&rpath)? {
                self.move_aside(&twin, report)?;
            }
            new_entry.kind = self.base_file(file, meta, hash);
            self.state.base.insert(id, new_entry);
            return Ok(());
        }
        self.ensure_free(&rpath, report)?;
        // Saved here meanwhile: the old version stays in the base, so the next pass sees an edit on both sides.
        if let Some((meta, hash)) = self.download(file, &rpath, None, report).await? {
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
    /// (its own move puts it in place later). "Unchanged" is decided by content, as for a plain delete: a file
    /// that only got a new time or file ID goes. A file the server deleted that already holds the newcomer's
    /// content (`incoming`; a pass that crashed before saving its state downloaded it) is let go and left in
    /// place, so the newcomer adopts it instead of making a copy. Only a newcomer that adopts what it finds at
    /// `path` passes `incoming`: anything left in place at a path that is renamed or written onto would be moved
    /// aside right after, a needless copy of content the base already agreed on.
    async fn release_tracked(
        &mut self,
        path: &RelPath,
        newcomer: NodeId,
        incoming: Option<&FileState>,
        report: &mut Report,
    ) -> Result<(), SyncError> {
        let Some(other) = self.state.base_index().get(path).copied() else { return Ok(()) };
        if other == newcomer || Some(other) == self.state.root {
            return Ok(());
        }
        let Some(entry) = self.state.base.get(&other).cloned() else { return Ok(()) };
        let local = self.local_state(&entry, path)?;
        if local == Local::Missing {
            return Ok(());
        }
        // A tracked folder holding a tracked file or folder that turned into a link isn't moved aside or let go:
        // the node behind the link would lose its place. The newcomer waits until it is a real file or folder again.
        if matches!(entry.kind, BaseKind::Dir { .. })
            && let Some(link) = self.tracked_link_inside(path)?
        {
            return Err(io_err(&link, std::io::Error::other(OTHER_REASON)));
        }
        if self.state.remote.contains_key(&other) {
            self.move_aside(path, report)?;
            return Ok(());
        }
        match (&entry.kind, local) {
            (BaseKind::File { .. }, Local::Unchanged) if self.really_unchanged(&entry, path)? => {
                self.remove_file_here(path)?;
                report.removed_locally.push(path.clone());
            }
            (BaseKind::File { .. }, Local::Modified)
                if let Some(file) = incoming
                    && self.same_content(file, path).await?.is_some() => {}
            // Only touched here (a new time or file ID, the same content): deleting is decided by content, so it
            // goes as it would for a plain delete.
            (BaseKind::File { .. }, Local::Modified) if self.holds_base_content(&entry, path) => {
                self.remove_file_here(path)?;
                report.removed_locally.push(path.clone());
            }
            // An empty folder (or one with only OS metadata files) goes, as when the server deletes it; the guard
            // removes it. Any other folder is moved aside below.
            (BaseKind::Dir { .. }, Local::Unchanged) if self.remove_dir_if_bare(path)? => {
                report.removed_locally.push(path.clone());
            }
            _ => {
                self.move_aside(path, report)?;
            }
        }
        self.state.base.remove(&other);
        Ok(())
    }

    /// Before the tracked node `id` takes `to` (a rename of its local copy at `from`, or a download), settles any
    /// other tracked node that holds `to` or a case twin of it (see [`Engine::release_tracked`]): one the server
    /// deleted that is unchanged here goes, instead of being moved aside as a copy by the rename or the download.
    /// The path the node is renamed from (or a folder holding it) is never an occupant: it is the node itself.
    /// Checked first, as the rest of the change: nothing is settled if taking `to` would act on a link.
    async fn settle_occupants(&mut self, id: NodeId, to: &RelPath, from: Option<&RelPath>, report: &mut Report) -> Result<(), SyncError> {
        self.refuse_link_toward(to)?;
        if let Some(from) = from {
            self.no_link(from)?;
        }
        let holds_source = |occupant: &RelPath| from.is_some_and(|f| f.starts_with(occupant));
        if let Some(twin) = self.case_twin(to)?
            && !holds_source(&twin)
        {
            self.release_tracked(&twin, id, None, report).await?;
        }
        if !holds_source(to) {
            self.release_tracked(to, id, None, report).await?;
        }
        Ok(())
    }

    /// A link or special file inside the folder `dir` (at any depth) whose name, ignoring case, is where the base
    /// has a tracked node: a tracked file or folder that turned into a link.
    fn tracked_link_inside(&self, dir: &RelPath) -> Result<Option<RelPath>, SyncError> {
        let tracked: BTreeSet<(RelPath, FoldKey)> = self
            .state
            .base_index()
            .into_keys()
            .filter(|p| p.starts_with(dir) && p != dir)
            .filter_map(|p| Some((p.parent()?, p.file_name()?.fold_key())))
            .collect();
        if tracked.is_empty() {
            return Ok(None);
        }
        let mut queue = vec![dir.clone()];
        while let Some(folder) = queue.pop() {
            for entry in self.fs.list(&folder).map_err(|e| io_err(&folder, e))? {
                let Ok(name) = Name::new(&entry.name) else { continue };
                match entry.kind {
                    FsKind::Other if tracked.contains(&(folder.clone(), name.fold_key())) => return Ok(Some(folder.join(&name))),
                    FsKind::Dir { .. } if folder.depth() < MAX_TREE_DEPTH => queue.push(folder.join(&name)),
                    _ => {}
                }
            }
        }
        Ok(None)
    }

    /// Moves `from` to `to`, moving aside anything at `to`. When one path contains the other (the target is
    /// an untracked folder holding the source, or the reverse), the source hops through a temporary
    /// conflict-copy name first.
    fn relocate(&mut self, from: &RelPath, to: &RelPath, report: &mut Report) -> Result<(), SyncError> {
        if from == to {
            return Ok(());
        }
        // Everything the second step needs is checked before the first (the hop through a temporary name).
        self.refuse_link_toward(to)?;
        let mut source = from.clone();
        // A case-only rename (`a.txt` → `A.TXT`): the source is the target's case twin, so it must not be
        // moved aside as an occupant.
        let case_only = from.parent() == to.parent() && from.file_name().map(Name::fold_key) == to.file_name().map(Name::fold_key);
        if from.starts_with(to) || to.starts_with(from) || case_only {
            // The hop sits next to the shallower of the two paths, outside both.
            let outer = if from.starts_with(to) && !case_only { to } else { from };
            let is_dir = matches!(self.stat(from)?, Some(FsKind::Dir { .. }));
            let hop = self.conflict_path(outer, is_dir)?;
            self.rename_here(from, &hop)?;
            source = hop;
        }
        self.ensure_free(to, report)?;
        self.rename_here(&source, to)?;
        report.moved_locally.push((from.clone(), to.clone()));
        Ok(())
    }

    /// Where the tracked node `id` is now, if it isn't at its base path: found by its own local ID or, failing
    /// that (a file an atomic save replaced, a platform without file IDs), through the nearest folder above it
    /// that is found by its local ID, plus the rest of the node's path. A path another tracked node holds
    /// belongs to that node (path first, then file ID), so it doesn't count.
    fn locate(&self, id: NodeId) -> Result<Option<RelPath>, SyncError> {
        let Some(node) = self.state.base.get(&id) else { return Ok(None) };
        let is_dir = matches!(node.kind, BaseKind::Dir { .. });
        let index = self.state.base_index();
        let mut below: Vec<Name> = Vec::new();
        let mut current = id;
        for _ in 0..=MAX_TREE_DEPTH {
            if Some(current) == self.state.root {
                return Ok(None);
            }
            let Some(entry) = self.state.base.get(&current) else { return Ok(None) };
            let local_id = match entry.kind {
                BaseKind::Dir { local_id } | BaseKind::File { local_id, .. } => local_id,
            };
            if let Some(lid) = local_id
                && let Some(found) = self.find_local(lid)?
            {
                let path = below.iter().rev().fold(found, |p, n| p.join(n));
                let fits = match self.stat(&path)? {
                    Some(FsKind::Dir { .. }) => is_dir,
                    Some(FsKind::File(_)) => !is_dir,
                    _ => false,
                };
                let taken = index.get(&path).is_some_and(|other| *other != id);
                return Ok((fits && !taken).then_some(path));
            }
            let (Some(name), Some(parent)) = (entry.name.clone(), entry.parent) else { return Ok(None) };
            below.push(name);
            current = parent;
        }
        Ok(None)
    }

    /// The link or special file the tracked node `id` turned into, if any, found like [`Engine::locate`] does
    /// through the nearest folder above it that is found by its local ID (the user moved that folder, and the link
    /// went with it). Its own place is checked before a change is applied (see `link_on_the_way`).
    fn link_in_place_of(&self, id: NodeId) -> Result<Option<RelPath>, SyncError> {
        let mut below: Vec<Name> = Vec::new();
        let mut current = id;
        for _ in 0..=MAX_TREE_DEPTH {
            let Some(BaseEntry { name: Some(name), parent: Some(parent), .. }) = self.state.base.get(&current) else { return Ok(None) };
            below.push(name.clone());
            current = *parent;
            let Some(BaseEntry { kind: BaseKind::Dir { local_id: Some(lid) }, .. }) = self.state.base.get(&current) else { continue };
            let Some(mut path) = self.find_local(*lid)? else { continue };
            for name in below.iter().rev() {
                path = path.join(name);
                match self.stat(&path)? {
                    Some(FsKind::Other) => return Ok(Some(path)),
                    Some(FsKind::Dir { .. }) => {}
                    _ => return Ok(None),
                }
            }
            return Ok(None);
        }
        Ok(None)
    }

    /// Where the file or folder with local ID `local_id` is now, if anywhere in the folder.
    fn find_local(&self, local_id: u64) -> Result<Option<RelPath>, SyncError> {
        let mut queue = vec![RelPath::root()];
        while let Some(dir) = queue.pop() {
            for entry in self.fs.list(&dir).map_err(|e| io_err(&dir, e))? {
                // Ignored names are out of sync's sight, as in the scan: a file renamed to one (an app's
                // temp name mid-save) isn't the tracked file moved, and push couldn't send that move.
                if self.config.ignored(&entry.name) {
                    continue;
                }
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
            self.mkdir_here(&current)?;
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
        // Checked before anything is made or moved aside.
        self.refuse_link_toward(path)?;
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
            // No `incoming`: the twin is never adopted (it has another name), it is moved aside below if it stays.
            self.release_tracked(&twin, id, None, report).await?;
            if self.stat(&twin)?.is_some() {
                self.move_aside(&twin, report)?;
            }
        }
        self.release_tracked(&rpath, id, remote.file.as_ref(), report).await?;
        let here = self.stat(&rpath)?;
        let kind = match (&remote.file, here) {
            // A folder already here is adopted, unless it is another tracked folder that the user moved here
            // (same local ID): that one is moved aside so its own move can be sent.
            (None, Some(FsKind::Dir { id: local_id })) if !self.owns_local_dir(local_id, id) => BaseKind::Dir { local_id },
            (None, other) => {
                if other.is_some() {
                    self.move_aside(&rpath, report)?;
                }
                self.mkdir_here(&rpath)?;
                report.created_dirs.push(rpath.clone());
                BaseKind::Dir { local_id: Self::dir_id(self.stat(&rpath)?) }
            }
            (Some(file), None) => {
                // Saved here meanwhile: not taken this pass; the next one meets the file as a local copy.
                let Some((meta, hash)) = self.download(file, &rpath, None, report).await? else { return Ok(()) };
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
                    // Saved here meanwhile: not taken this pass; the next one meets the file as a local copy.
                    let Some((meta, hash)) = self.download(file, &rpath, None, report).await? else { return Ok(()) };
                    self.base_file(file, meta, hash)
                }
            }
            (Some(file), Some(_)) => {
                self.move_aside(&rpath, report)?;
                // Saved here meanwhile: not taken this pass; the next one meets the file as a local copy.
                let Some((meta, hash)) = self.download(file, &rpath, None, report).await? else { return Ok(()) };
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
            // A folder that still holds a tracked node stays tracked until that node is settled: a node whose own
            // change waits (it turned into a link in this folder, which the user moved) keeps its place through it.
            // The push sends nothing for a node the server deleted.
            Local::Missing if matches!(entry.kind, BaseKind::Dir { .. }) && self.state.base.values().any(|e| e.parent == Some(id)) => {}
            Local::Missing => {
                self.state.base.remove(&id);
            }
            Local::Unchanged => match entry.kind {
                // Content edited here after all (same size and time by accident): the edit wins.
                BaseKind::File { .. } if !self.really_unchanged(&entry, &bpath)? => {}
                BaseKind::File { .. } => {
                    self.remove_file_here(&bpath)?;
                    self.state.base.remove(&id);
                    report.removed_locally.push(bpath);
                }
                BaseKind::Dir { .. } => {
                    // Only an empty folder goes (with any OS metadata files in it); one with new local files
                    // stays, and the push brings it back.
                    let only_ignored = self.fs.list(&bpath).map_err(|e| io_err(&bpath, e))?.iter().all(|e| self.config.ignored(&e.name));
                    if only_ignored && self.remove_dir_if_bare(&bpath)? {
                        self.state.base.remove(&id);
                        report.removed_locally.push(bpath);
                    } else if !self.state.base.values().any(|e| e.parent == Some(id)) {
                        // Only new files inside: forget the folder, and the push creates it afresh. (One that
                        // still holds an edited tracked file stays; that file's upload brings the folder back.)
                        // Only ignored files inside (an open document's lock file): it stays here, forgotten, and
                        // the push leaves it alone.
                        self.state.base.remove(&id);
                    }
                }
            },
            // Only touched here (a new time, a copy put back, the same content): not an edit, so the delete goes.
            Local::Modified if self.holds_base_content(&entry, &bpath) => {
                self.remove_file_here(&bpath)?;
                self.state.base.remove(&id);
                report.removed_locally.push(bpath);
            }
            // Edited here, deleted there: the edit wins (the push uploads it, the server restores the node).
            Local::Modified => {}
        }
        Ok(())
    }

    /// Whether the file at `at` holds exactly the content the base recorded (hashed; an unreadable file doesn't).
    fn holds_base_content(&self, entry: &BaseEntry, at: &RelPath) -> bool {
        match &entry.kind {
            BaseKind::File { hash, .. } => self.content_hash(at) == Some(*hash),
            BaseKind::Dir { .. } => false,
        }
    }
}
