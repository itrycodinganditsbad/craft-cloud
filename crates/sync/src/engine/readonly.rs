//! The push of a space that is read-only for the account (a viewer; see [`Engine::set_read_only`]).
//!
//! Nothing goes to the server. The folder is only ever changed to match the server, and only where that loses
//! nothing the user made:
//!
//! - A local edit stays where it is. When the server's version of that file changes too, the pull side already keeps
//!   both (the server's version lands under the original name, the edit moves to a conflict copy).
//! - A new local file or folder stays local.
//! - A file or folder deleted here that the server still has is downloaded again. Deleting it on the server is not on
//!   offer, and leaving the folder different from the space without saying so would hide that. The report names what
//!   came back ([`Report::restored`]).
//! - A file moved or renamed here is the same case: the server's copy comes back at its old place, and the moved file
//!   stays as a new local file.
//!
//! Everything that stays only here is listed in [`Report::skipped`] with the reason, on every pass.

use std::collections::{BTreeMap, BTreeSet};

use craft_cloud_proto::{FileHash, NodeId};

use crate::fs::{FileMeta, Fs, FsKind};
use crate::path::RelPath;
use crate::state::{BaseEntry, BaseKind};
use crate::transport::Transport;

use super::{Engine, Local, Report, SyncError, per_node};

/// Why a file that is only here (new, or a conflict copy) is not uploaded in read-only mode.
pub const READ_ONLY_NEW: &str = "not uploaded: this space is read-only for you; the file stays on this computer";
/// Why an edited file is not uploaded in read-only mode.
pub const READ_ONLY_EDITED: &str = "edited here, not uploaded: this space is read-only for you; the file stays on this computer";

impl<F: Fs, T: Transport> Engine<F, T> {
    /// The push of read-only mode: puts back what was deleted here and lists what stays local. Sends nothing.
    ///
    /// Known limit, kept on purpose: `finish_push` (which records the links the push saw and forgets where nodes were
    /// before they moved) never runs in this mode, so right after the role is raised to editor the first push may hold
    /// back a delete of a user's that it would have sent a pass later. A delete held is never a lost file; it is sent
    /// by a following pass, once that push has finished.
    pub(super) async fn push_read_only(&mut self, report: &mut Report) -> Result<(), SyncError> {
        let before = report.restored.len();
        self.restore_deleted(report).await?;
        if report.restored.len() != before {
            self.checkpoint()?;
        }
        self.list_local_only(report)
    }

    /// Downloads again every tracked node the server still has that is missing here, parents first.
    async fn restore_deleted(&mut self, report: &mut Report) -> Result<(), SyncError> {
        let root = self.state.root.ok_or_else(|| SyncError::Corrupt("no root".into()))?;
        let mut nodes: Vec<(usize, NodeId)> = self
            .state
            .base
            .iter()
            .filter(|(id, _)| **id != root && self.state.remote.contains_key(*id))
            .filter_map(|(id, _)| Some((self.state.base_path(*id)?.depth(), *id)))
            .collect();
        nodes.sort();
        // Nodes skipped this pass (a filesystem error); what is below one waits for it too.
        let mut skipped: BTreeSet<NodeId> = BTreeSet::new();
        for (_, id) in nodes {
            if self.state.base.get(&id).and_then(|e| e.parent).is_some_and(|p| skipped.contains(&p)) {
                skipped.insert(id);
                continue;
            }
            let result = self.restore_one(id, report).await;
            // `Ok(true)`: something that is not the node sits where it was (a folder's place held by a file or a link):
            // left alone, and what was below it waits. An error skips it the same way.
            let held = match result {
                Ok(held) => held,
                other => per_node(other.map(drop), report)?,
            };
            if held {
                skipped.insert(id);
            }
        }
        Ok(())
    }

    /// Puts back one node that is gone from here. Returns whether the node's own place is held by something else that
    /// stays (the nodes below it are then left alone too).
    async fn restore_one(&mut self, id: NodeId, report: &mut Report) -> Result<bool, SyncError> {
        let (Some(entry), Some(remote), Some(path)) =
            (self.state.base.get(&id).cloned(), self.state.remote.get(&id).cloned(), self.state.base_path(id))
        else {
            return Ok(false);
        };
        // Anything there (the file, a link, another file) is left alone: only what is gone comes back. A folder's place
        // held by anything but a folder holds back what was in it: a file or a link there is the user's, and the
        // server's files go nowhere near it.
        if let Some(here) = self.stat(&path)? {
            return Ok(matches!(entry.kind, BaseKind::Dir { .. }) && !matches!(here, FsKind::Dir { .. }));
        }
        // `stat` of a path below a file or a link says "nothing there": looked at before anything is made or moved
        // aside, or the file or link would be taken for something in the way.
        if self.held_above(&path)? {
            return Ok(true);
        }
        match (&entry.kind, &remote.file) {
            (BaseKind::Dir { .. }, None) => {
                self.refuse_link_toward(&path)?;
                let made = report.created_dirs.len();
                self.make_dirs(&path, report)?;
                // Reported once, as restored (not as a folder the pass made as well).
                let mut kept = report.created_dirs.drain(made..).filter(|dir| *dir != path).collect();
                report.created_dirs.append(&mut kept);
                report.restored.push(path);
            }
            (BaseKind::File { .. }, Some(file)) => {
                self.ensure_free(&path, report)?;
                // Saved here meanwhile: not taken this pass; the next one finds the file and keeps it.
                if let Some((meta, hash)) = self.download(id, file, &path, None, report).await? {
                    let kind = self.base_file(file, meta, hash);
                    self.state.base.update(&id, |base| base.kind = kind);
                    report.restored.push(path);
                }
            }
            // A node can't change kind; treat it as damaged state and leave it.
            _ => {}
        }
        Ok(false)
    }

    /// Whether a folder above `path` is not a folder here (a file, or a link or other entry): nothing below it is put
    /// back, and nothing is moved aside to make room.
    fn held_above(&self, path: &RelPath) -> Result<bool, SyncError> {
        let Some(parent) = path.parent() else { return Ok(false) };
        let mut current = RelPath::root();
        for name in parent.names() {
            current = current.join(name);
            if matches!(self.stat(&current)?, Some(FsKind::File(_) | FsKind::Other)) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Lists, in [`Report::skipped`], every file that exists only here: edited tracked files and untracked files
    /// (new ones and conflict copies). A tracked file that was swapped for another tracked file counts as missing here
    /// ([`Local::Missing`]), so it is put back and not listed (a listing gap only: both files stay). The list is given in full when the set of such files (or a reason) differs from the
    /// one the last pass gave, and left out otherwise, so a quiet viewer's folder produces a quiet report.
    fn list_local_only(&mut self, report: &mut Report) -> Result<(), SyncError> {
        let (local, unseen) = self.scan(report)?;
        let index = self.state.base_index();
        let mut found: BTreeSet<(String, &'static str)> = BTreeSet::new();
        let mut looked_at: BTreeMap<RelPath, (FileMeta, FileHash, bool)> = BTreeMap::new();
        for (path, kind) in &local {
            let FsKind::File(meta) = kind else { continue };
            if unseen.unknown.iter().any(|u| path.starts_with(u)) {
                continue;
            }
            let reason = match index.get(path).and_then(|id| self.state.base.get(id)).cloned() {
                None => READ_ONLY_NEW,
                Some(entry) => {
                    if !self.edited_here(&entry, path, *meta, &mut looked_at)? {
                        continue;
                    }
                    READ_ONLY_EDITED
                }
            };
            found.insert((path.to_string(), reason));
        }
        self.read_only_verdicts = looked_at;
        if found != self.local_only_listed {
            report.skipped.extend(found.iter().map(|(path, reason)| (path.clone(), (*reason).to_owned())));
            self.local_only_listed = found;
        }
        Ok(())
    }

    /// Whether the tracked file at `path` holds something other than what both sides agreed on. A different size says so
    /// without reading the file; the same size needs its content hashed, and the answer is kept for as long as the file
    /// (its size, time and ID) and the agreed content stay the same, so a quiet viewer's pass does not read every edited
    /// file again. Kept answers are carried in `looked_at`, which replaces the cache after the pass (files that went
    /// are forgotten).
    fn edited_here(
        &self,
        entry: &BaseEntry,
        path: &RelPath,
        meta: FileMeta,
        looked_at: &mut BTreeMap<RelPath, (FileMeta, FileHash, bool)>,
    ) -> Result<bool, SyncError> {
        if self.local_state(entry, path)? != Local::Modified {
            return Ok(false);
        }
        let BaseKind::File { size, hash, .. } = &entry.kind else { return Ok(false) };
        if meta.size != *size {
            return Ok(true);
        }
        if let Some((seen, base, edited)) = self.read_only_verdicts.get(path)
            && *seen == meta
            && base == hash
        {
            looked_at.insert(path.clone(), (meta, *hash, *edited));
            return Ok(*edited);
        }
        let edited = !self.holds_base_content(entry, path);
        looked_at.insert(path.clone(), (meta, *hash, edited));
        Ok(edited)
    }
}
