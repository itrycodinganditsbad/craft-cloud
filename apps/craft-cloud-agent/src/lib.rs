//! The Craft Cloud sync agent: keeps a folder on this computer in step with a Craft Cloud server.
//!
//! The sync logic is the engine (`craft-cloud-sync`); this crate gives it a real disk ([`disk::DiskFs`]) and
//! the server's HTTP API ([`http::HttpTransport`]), keeps the sign-in and the engine's state between runs
//! ([`account`]), guards the folder with a root marker ([`root`]) and runs it from the command line ([`cli`]): one
//! pass at a time (`sync`), or on its own (`run`: [`daemon`], scheduled by [`schedule`], woken by [`watch`], with
//! the stability window kept by [`settle`]).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod account;
pub mod cli;
pub mod daemon;
pub mod disk;
pub mod http;
pub mod live;
pub mod local_api;
pub mod lock;
pub mod root;
pub mod schedule;
pub mod settle;
pub mod watch;
