//! The app kit's demo: a story and a layer tree in one window, live through Craft Cloud, saved to a `.kitdemo` file
//! (JSON). It shows what an app adds for live documents, and nothing more: its adapter for the kit
//! ([`model::DemoApp`]), the hook at the end of each command ([`ui::Demo::apply`]), a poll each frame, undo through
//! the session while live, and the save record when it saves. Not packaged or installed: `cargo run -p
//! craft-cloud-kit-demo -- <file in the synced folder>`.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod model;
pub mod ui;
