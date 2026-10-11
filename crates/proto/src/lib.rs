//! Craft Cloud protocol types, shared by the server and the sync agent.
//!
//! Pure: no I/O. The only clock read is in the `generate()` constructors of the ID types (UUIDv7).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod conflict;
pub mod hash;
pub mod ids;
pub mod limits;
pub mod manifest;
pub mod name;

pub use conflict::{ConflictStamp, conflict_name};
pub use hash::{ChunkId, FileHash, HexIdError, ManifestId};
pub use ids::{DeviceId, Epoch, NodeId, SpaceId, VersionId};
pub use manifest::{ChunkRef, ChunkerParamsId, Manifest, ManifestError};
pub use name::{FoldKey, Name, NameError, UNICODE_VERSION};
