//! The Craft Cloud sync engine: keeps a folder and a space in step.
//!
//! The engine is pure logic over two traits: [`Fs`] for the folder and [`Transport`] for the server. The
//! agent supplies a real disk and HTTP; tests and the simulation supply [`MemFs`] and an in-process server.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

#[cfg(feature = "test-support")]
pub mod conformance;
pub mod engine;
pub mod fs;
pub mod path;
pub mod state;
pub mod transport;

pub use engine::{Config, DEFAULT_IGNORES, Engine, NOT_NFC_REASON, OTHER_REASON, Report, SAVE_ORDER_WAITS, STILL_CHANGING, SaveRecords, SyncError};
pub use fs::{FileMeta, Fs, FsEntry, FsKind, LandError, MemFs, ReadOnly, is_read_only, read_only_error, rel};
pub use path::RelPath;
pub use state::{BaseEntry, BaseKind, HeldLink, RemoteNode, State};
pub use transport::{Transport, TransportError};
