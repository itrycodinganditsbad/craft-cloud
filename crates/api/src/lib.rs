//! What Craft apps need of Craft Cloud's protocol types, with no binary wire encoding: IDs, names, limits, the agent's
//! local API ([`local`]) and the error codes and bodies it answers with ([`api`]).
//!
//! Pure: no I/O. `craft-cloud-app` and `craft-cloud-kit` (what apps link) depend on this crate alone; the server's and
//! the agent's wire types (the live protocol's frames, encoded with postcard, manifests, commit batches) are in
//! `craft-cloud-proto`, which re-exports everything here under the same paths.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod api;
pub mod ids;
pub mod limits;
pub mod local;
pub mod name;

pub use ids::{DeviceId, DocId, Epoch, NodeId, SpaceId, VersionId};
pub use name::{FoldKey, Name, NameError, UNICODE_VERSION};
