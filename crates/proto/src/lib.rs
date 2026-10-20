//! Craft Cloud protocol types, shared by the server and the sync agent.
//!
//! Pure: no I/O. The only clock read is in the `generate()` constructors of the ID types (UUIDv7). The live protocol
//! ([`live`], agent ↔ server) lives here. What apps need too (IDs, names, limits, the agent's local API [`local`])
//! lives in `craft-cloud-api`, which has no postcard, and is re-exported here under the same paths, so the server, the
//! agent and `craft-cloud-app` share one definition.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod api;
pub mod conflict;
pub mod hash;
pub mod join;
pub mod live;
pub mod manifest;
pub mod ops;

pub use craft_cloud_api::{ids, limits, local, name};

pub use conflict::{ConflictStamp, conflict_name};
pub use hash::{BlobKind, ChunkId, FileHash, HexIdError, ManifestId};
pub use ids::{DeviceId, DocId, Epoch, NodeId, SpaceId, VersionId};
pub use manifest::{ChunkRef, ChunkerParamsId, Manifest, ManifestError};
pub use name::{FoldKey, Name, NameError, UNICODE_VERSION};
pub use ops::{ChangeSet, Conflict, ConflictKind, FileState, NewVersion, NodeKind, NodeState, Op, PageCursor, SaveRecord};
