//! The Craft Cloud server: an HTTP API over the metadata store (`craft-cloud-meta`) and the blob store
//! (`craft-cloud-store`), plus the admin command line.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod auth;
pub mod config;
pub mod error;
pub mod notify;
pub mod ratelimit;
pub mod state;

pub use state::AppState;
