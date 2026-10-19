//! Batch commits: the operations an agent sends, the conflicts the server answers with, and the node states
//! that the tree listing and the change feed return.
//!
//! A batch is applied in one transaction: every operation applies or none does. When some operations
//! conflict, the server lists every one of them (by index into the batch), not just the first.

use serde::{Deserialize, Serialize};

use crate::hash::ManifestId;
use crate::ids::{DeviceId, Epoch, NodeId, VersionId};
use crate::name::Name;

pub use craft_cloud_api::local::{MAX_SAVE_HEADS, SaveRecord};

/// A new file version. Its size comes from the manifest, which must already be uploaded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewVersion {
    pub id: VersionId,
    pub manifest: ManifestId,
    /// Modification time on the uploading device, milliseconds since the Unix epoch. Display only.
    pub mtime_ms: i64,
    pub exec: bool,
    /// A Craft app saved this content from a live document: which, and its version then. Appended: an older agent
    /// sends none, an older server ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save: Option<SaveRecord>,
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
    /// The result would nest folders deeper than the limit.
    TooDeep,
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
    /// The version's save record, when a Craft app saved it from a live document (see [`NewVersion::save`]).
    /// Appended: an older server sends none, an older agent ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save: Option<SaveRecord>,
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

/// Where the next page of a listing starts: just after the node with this change sequence number and ID
/// (listings are ordered by `(seq, node)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageCursor {
    pub seq: u64,
    pub node: NodeId,
}

/// Nodes changed after some sequence number (or every live node, for a full listing), with the space's
/// epoch and the sequence number to ask from next time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeSet {
    pub epoch: Epoch,
    pub seq: u64,
    pub nodes: Vec<NodeState>,
    /// Set when more nodes follow; pass it as `after` to get the next page.
    pub next: Option<PageCursor>,
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

    /// Clients split commits by operation count only, so the largest batch they can send must fit the server's
    /// body limit: `MAX_BATCH_OPS` of the largest operation (a move naming two longest names; names need no JSON
    /// escapes, since `"`, `\` and control characters are refused), with the largest numbers, as compact JSON.
    #[test]
    fn a_full_batch_of_the_largest_ops_fits_the_body_limit() {
        use crate::api::CommitRequest;
        use crate::limits::{MAX_BATCH_JSON_BYTES, MAX_BATCH_OPS, MAX_NAME_BYTES};
        // JSON carries a name's UTF-8 bytes as they are, so any longest name is as long as JSON.
        let name = Name::new(&"x".repeat(MAX_NAME_BYTES)).unwrap();
        let mv =
            Op::Move { node: node(u128::MAX), from_parent: node(u128::MAX), from_name: name.clone(), parent: node(u128::MAX), name: name.clone() };
        let version = NewVersion {
            id: VersionId::from_uuid(Uuid::from_u128(u128::MAX)),
            manifest: crate::ManifestId::from_bytes([0xff; 32]),
            mtime_ms: i64::MIN,
            exec: false,
            save: None,
        };
        let create = Op::Create { node: node(u128::MAX), parent: node(u128::MAX), name, version };
        let largest = [mv, create].into_iter().max_by_key(|op| serde_json::to_vec(op).unwrap().len()).unwrap();
        let body = serde_json::to_vec(&CommitRequest { ops: vec![largest; MAX_BATCH_OPS] }).unwrap();
        assert!(body.len() <= MAX_BATCH_JSON_BYTES, "{} bytes", body.len());
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
    fn too_deep_conflict_round_trips() {
        let c = Conflict { op: 0, kind: ConflictKind::TooDeep };
        let json = serde_json::to_value(&c).unwrap();
        assert_eq!(json["kind"], "too_deep");
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
    fn change_sets_round_trip_with_a_cursor() {
        let set = ChangeSet {
            epoch: Epoch::from_uuid(Uuid::from_u128(7)),
            seq: 12,
            nodes: vec![NodeState { id: node(1), parent: None, name: None, deleted: false, kind: NodeKind::Dir }],
            next: Some(PageCursor { seq: 11, node: node(4) }),
        };
        let json = serde_json::to_value(&set).unwrap();
        assert_eq!(json["next"]["seq"], 11);
        assert_eq!(serde_json::from_value::<ChangeSet>(json).unwrap(), set);
        let last = ChangeSet { next: None, ..set };
        assert_eq!(serde_json::from_value::<ChangeSet>(serde_json::to_value(&last).unwrap()).unwrap(), last);
    }

    #[test]
    fn file_node_states_round_trip() {
        let file = NodeState {
            id: node(2),
            parent: Some(node(1)),
            name: Some(Name::new("a.txt").unwrap()),
            deleted: true,
            kind: NodeKind::File(FileState {
                version: VersionId::from_uuid(Uuid::from_u128(3)),
                manifest: ManifestId::from_bytes([5; 32]),
                size: 10,
                mtime_ms: -5,
                exec: true,
                device: DeviceId::from_uuid(Uuid::from_u128(4)),
                epoch: Epoch::from_uuid(Uuid::from_u128(6)),
                save: None,
            }),
        };
        let json = serde_json::to_value(&file).unwrap();
        assert_eq!(json["type"], "file");
        assert_eq!(json["size"], 10);
        assert_eq!(serde_json::from_value::<NodeState>(json).unwrap(), file);
    }

    #[test]
    fn conflicts_with_missing_details_round_trip() {
        for kind in [ConflictKind::Moved { parent: None, name: None }, ConflictKind::VersionMismatch { current: None }] {
            let c = Conflict { op: 1, kind };
            let json = serde_json::to_value(&c).unwrap();
            assert_eq!(serde_json::from_value::<Conflict>(json).unwrap(), c);
        }
    }

    #[test]
    fn op_node_is_the_acted_on_node() {
        assert_eq!(Op::Delete { node: node(5), expected: None }.node(), node(5));
    }

    /// A save record is appended: a version without one has the JSON it always had (an older peer reads it), one
    /// with it reads back, and an older peer's JSON (no field) reads as none.
    #[test]
    fn save_records_are_appended() {
        let plain = NewVersion {
            id: VersionId::from_uuid(Uuid::from_u128(1)),
            manifest: ManifestId::from_bytes([1; 32]),
            mtime_ms: 1,
            exec: false,
            save: None,
        };
        let json = serde_json::to_value(&plain).unwrap();
        assert!(json.get("save").is_none(), "{json}");
        let doc = crate::DocId::from_uuid(Uuid::from_u128(9));
        let saved = NewVersion { save: Some(SaveRecord { doc, frontiers: vec![(1, 5), (2, 3)] }), ..plain.clone() };
        let json = serde_json::to_value(&saved).unwrap();
        assert_eq!(serde_json::from_value::<NewVersion>(json).unwrap(), saved);
        let older: NewVersion = serde_json::from_value(
            serde_json::json!({ "id": Uuid::from_u128(1), "manifest": ManifestId::from_bytes([1; 32]), "mtime_ms": 1, "exec": false }),
        )
        .unwrap();
        assert_eq!(older, plain);
        // An agent from before save records reads a file state that has one (the field is unknown to it).
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct OlderFileState {
            version: VersionId,
            manifest: ManifestId,
            size: u64,
            mtime_ms: i64,
            exec: bool,
            device: DeviceId,
            epoch: Epoch,
        }
        let state = FileState {
            version: plain.id,
            manifest: plain.manifest,
            size: 1,
            mtime_ms: 1,
            exec: false,
            device: DeviceId::from_uuid(Uuid::from_u128(2)),
            epoch: Epoch::from_uuid(Uuid::from_u128(3)),
            save: saved.save.clone(),
        };
        let json = serde_json::to_value(&state).unwrap();
        assert!(json.get("save").is_some());
        assert!(serde_json::from_value::<OlderFileState>(json).is_ok());
        // A record from before frontiers (a version vector) reads as no record.
        let vector = serde_json::json!({ "doc": doc, "version": [[1, 5]] });
        assert!(serde_json::from_value::<SaveRecord>(vector).is_err());
    }
}
