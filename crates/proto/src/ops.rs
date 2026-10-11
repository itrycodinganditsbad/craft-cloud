//! Batch commits: the operations an agent sends, the conflicts the server answers with, and the node states
//! that the tree listing and the change feed return.
//!
//! A batch is applied in one transaction: every operation applies or none does. When some operations
//! conflict, the server lists every one of them (by index into the batch), not just the first.

use serde::{Deserialize, Serialize};

use crate::hash::ManifestId;
use crate::ids::{DeviceId, Epoch, NodeId, VersionId};
use crate::name::Name;

/// A new file version. Its size comes from the manifest, which must already be uploaded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewVersion {
    pub id: VersionId,
    pub manifest: ManifestId,
    /// Modification time on the uploading device, milliseconds since the Unix epoch. Display only.
    pub mtime_ms: i64,
    pub exec: bool,
}

/// One operation in a batch commit. Node IDs are made by the client (UUIDv7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// Create a directory.
    Mkdir { node: NodeId, parent: NodeId, name: Name },
    /// Create a file with its first version.
    Create { node: NodeId, parent: NodeId, name: Name, version: NewVersion },
    /// A new version of a file; `expected` must be its current version.
    Update { node: NodeId, expected: VersionId, version: NewVersion },
    /// Rename and/or move; `from_parent`/`from_name` must be where the node is now.
    Move { node: NodeId, from_parent: NodeId, from_name: Name, parent: NodeId, name: Name },
    /// Delete a file (`expected` = its current version) or an empty directory (`expected` = none).
    Delete { node: NodeId, expected: Option<VersionId> },
}

impl Op {
    /// The node the operation acts on.
    pub fn node(&self) -> NodeId {
        match self {
            Op::Mkdir { node, .. } | Op::Create { node, .. } | Op::Update { node, .. } | Op::Move { node, .. } | Op::Delete { node, .. } => *node,
        }
    }
}

/// Why one operation of a batch was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConflictKind {
    /// The node ID is already used in this space.
    NodeExists,
    /// The version ID is already used in this space.
    VersionExists,
    NoSuchNode,
    NoSuchParent,
    /// The target parent is a file.
    NotADirectory,
    /// The operation needs a file but the node is a directory, or the other way round.
    WrongKind,
    /// Another node in the target directory has the same name (compared by case fold).
    NameTaken {
        existing: NodeId,
    },
    /// The file's current version is not the expected one.
    VersionMismatch {
        current: Option<VersionId>,
    },
    /// The node is no longer where the move said it was.
    Moved {
        parent: Option<NodeId>,
        name: Option<Name>,
    },
    /// The move would put a directory inside itself.
    Cycle,
    /// The directory still has children.
    NotEmpty,
    /// The root directory can't be moved or deleted.
    Root,
    /// The version's manifest hasn't been uploaded to this space.
    MissingManifest {
        manifest: ManifestId,
    },
}

/// One refused operation: `op` is its index in the batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    pub op: usize,
    #[serde(flatten)]
    pub kind: ConflictKind,
}

/// A file's current version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileState {
    pub version: VersionId,
    pub manifest: ManifestId,
    pub size: u64,
    pub mtime_ms: i64,
    pub exec: bool,
    pub device: DeviceId,
    /// The space epoch the version was committed in.
    pub epoch: Epoch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeKind {
    Dir,
    File(FileState),
}

/// A node as the tree and the change feed report it. The root has no parent and no name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeState {
    pub id: NodeId,
    pub parent: Option<NodeId>,
    pub name: Option<Name>,
    pub deleted: bool,
    #[serde(flatten)]
    pub kind: NodeKind,
}

/// Nodes changed after some sequence number (or every live node, for a full listing), with the space's
/// epoch and the sequence number to ask from next time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeSet {
    pub epoch: Epoch,
    pub seq: u64,
    pub nodes: Vec<NodeState>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn node(n: u128) -> NodeId {
        NodeId::from_uuid(Uuid::from_u128(n))
    }

    #[test]
    fn ops_are_tagged_json() {
        let op = Op::Mkdir { node: node(1), parent: node(2), name: Name::new("Shots").unwrap() };
        let json = serde_json::to_value(&op).unwrap();
        assert_eq!(json["op"], "mkdir");
        assert_eq!(json["name"], "Shots");
        assert_eq!(serde_json::from_value::<Op>(json).unwrap(), op);
    }

    #[test]
    fn invalid_names_in_ops_are_rejected_on_the_way_in() {
        let json = serde_json::json!({ "op": "mkdir", "node": node(1), "parent": node(2), "name": "a/b" });
        assert!(serde_json::from_value::<Op>(json).is_err());
    }

    #[test]
    fn conflicts_are_flat_json() {
        let c = Conflict { op: 3, kind: ConflictKind::NameTaken { existing: node(9) } };
        let json = serde_json::to_value(&c).unwrap();
        assert_eq!(json["op"], 3);
        assert_eq!(json["kind"], "name_taken");
        assert_eq!(serde_json::from_value::<Conflict>(json).unwrap(), c);
    }

    #[test]
    fn node_states_round_trip() {
        let dir = NodeState { id: node(1), parent: None, name: None, deleted: false, kind: NodeKind::Dir };
        let json = serde_json::to_value(&dir).unwrap();
        assert_eq!(json["type"], "dir");
        assert_eq!(serde_json::from_value::<NodeState>(json).unwrap(), dir);
    }

    #[test]
    fn op_node_is_the_acted_on_node() {
        assert_eq!(Op::Delete { node: node(5), expected: None }.node(), node(5));
    }
}
