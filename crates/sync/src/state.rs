//! What the engine remembers between passes: the server's tree as of a sequence number (the remote
//! mirror), and what this machine and the server last agreed on (the base).

use std::collections::BTreeMap;

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
    /// Nodes both sides agree on, with what the local copy looked like when they agreed.
    pub base: BTreeMap<NodeId, BaseEntry>,
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

impl State {
    /// Where a node sits in the base tree.
    pub fn base_path(&self, id: NodeId) -> Option<RelPath> {
        path_with(id, self.root, |n| self.base.get(&n).map(|e| (e.parent, e.name.clone())))
    }

    /// Where a node sits in the remote tree.
    pub fn remote_path(&self, id: NodeId) -> Option<RelPath> {
        path_with(id, self.root, |n| self.remote.get(&n).map(|e| (e.parent, e.name.clone())))
    }

    /// Base path → node, for every node with a path.
    pub fn base_index(&self) -> BTreeMap<RelPath, NodeId> {
        self.base.keys().filter_map(|id| Some((self.base_path(*id)?, *id))).collect()
    }
}
