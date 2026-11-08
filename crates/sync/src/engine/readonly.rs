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

use std::collections::BTreeSet;

use craft_cloud_proto::NodeId;

use crate::fs::{Fs, FsKind};
use crate::state::BaseKind;
use crate::transport::Transport;

use super::{Engine, Local, Report, SyncError, per_node};

/// Why a file that is only here (new, or a conflict copy) is not uploaded in read-only mode.
pub const READ_ONLY_NEW: &str = "not uploaded: this space is read-only for you; the file stays on this computer";
/// Why an edited file is not uploaded in read-only mode.
pub const READ_ONLY_EDITED: &str = "edited here, not uploaded: this space is read-only for you; the file stays on this computer";

impl<F: Fs, T: Transport> Engine<F, T> {
    /// The push of read-only mode: puts back what was deleted here and lists what stays local. Sends nothing.
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
            if per_node(result, report)? {
                skipped.insert(id);
            }
        }
        Ok(())
    }

    async fn restore_one(&mut self, id: NodeId, report: &mut Report) -> Result<(), SyncError> {
        let (Some(entry), Some(remote), Some(path)) =
            (self.state.base.get(&id).cloned(), self.state.remote.get(&id).cloned(), self.state.base_path(id))
        else {
            return Ok(());
        };
        // Anything there (the file, a link, another file) is left alone: only what is gone comes back.
        if self.stat(&path)?.is_some() {
            return Ok(());
        }
        match (&entry.kind, &remote.file) {
            (BaseKind::Dir { .. }, None) => {
                self.make_dirs(&path, report)?;
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
        Ok(())
    }

    /// Lists, in [`Report::skipped`], every file that exists only here: edited tracked files and untracked files
    /// (new ones and conflict copies). The list is given in full when the set of such files (or a reason) differs from the
    /// one the last pass gave, and left out otherwise, so a quiet viewer's folder produces a quiet report.
    fn list_local_only(&mut self, report: &mut Report) -> Result<(), SyncError> {
        let (local, unseen) = self.scan(report)?;
        let index = self.state.base_index();
        let mut found: BTreeSet<(String, &'static str)> = BTreeSet::new();
        for (path, kind) in &local {
            if !matches!(kind, FsKind::File(_)) || unseen.unknown.iter().any(|u| path.starts_with(u)) {
                continue;
            }
            let reason = match index.get(path).and_then(|id| self.state.base.get(id)) {
                None => READ_ONLY_NEW,
                Some(entry) => {
                    let edited = self.local_state(entry, path)? == Local::Modified && !self.holds_base_content(entry, path);
                    if !edited {
                        continue;
                    }
                    READ_ONLY_EDITED
                }
            };
            found.insert((path.to_string(), reason));
        }
        if found != self.local_only_listed {
            report.skipped.extend(found.iter().map(|(path, reason)| (path.clone(), (*reason).to_owned())));
            self.local_only_listed = found;
        }
        Ok(())
    }
}
