//! Craft Cloud, the app (`craft-cloud`): how a person joins this computer to their Craft Cloud, sees the sync, adds
//! another computer, pauses and quits. It never syncs or touches the synced files itself: all of that is the agent's
//! (`craft-cloud-agent run`), which it reaches through `craft-cloud-app` and starts when none answers.
//!
//! - [`model`]: the state and every way between its screens, pure (unit-tested against a fake agent).
//! - [`agent`]: the agent's calls behind a trait, run off the UI thread.
//! - [`start_agent`]: starting the agent from this app's folder.
//! - [`single_instance`]: one app per user; a second start brings the first window forward.
//! - [`ui`]: the window, a view of the model; [`app`]: the app on eframe.
//! - [`tray`]: the tray icon where there is one; [`autostart`]: the app's own start-at-login entry.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod agent;
pub mod app;
pub mod autostart;
pub mod model;
pub mod single_instance;
pub mod start_agent;
pub mod tray;
pub mod ui;

/// Milliseconds since the Unix epoch now (0 if the clock is before it).
pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// What the join form starts with: "Craft Cloud" in the home folder, and this computer's name (empty if it can't be
/// told: the agent then picks it).
pub fn join_defaults() -> model::JoinDefaults {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(std::path::PathBuf::from).filter(|p| p.is_absolute());
    let folder = home.map(|h| h.join("Craft Cloud").display().to_string()).unwrap_or_default();
    let device_name = std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty() && name.len() <= 128 && !name.chars().any(char::is_control))
        .unwrap_or_default();
    model::JoinDefaults { folder, device_name }
}

/// Where the agent writes its log (`run.log` in its data folder), for a problem's Details.
pub fn agent_log() -> String {
    craft_cloud_proto::local::data_dir(|name| std::env::var_os(name)).map(|d| d.join("run.log").display().to_string()).unwrap_or_default()
}
