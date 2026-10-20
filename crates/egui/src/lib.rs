//! Craft Cloud's piece of a Craft app's window, on egui 0.36 as every Craft app is:
//!
//! - [`account_line`]: who is signed in where, and whether Craft Cloud is online (or not running);
//! - [`recents`]: the app's recent files, shared by every machine of the account, newest first (a click opens one);
//! - [`badge`]: a live document's state (live, waiting N, read-only, offline), next to its title.
//!
//! The widgets only draw what they are given. [`CloudWatch`] keeps it current off the UI thread (the agent's status
//! every few seconds, the recents as they change) and wakes the UI when it changed (`request_repaint`).

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use craft_cloud_app::{Client, NewRecent, RecentEntry, RecentsEvent, RecentsWatch, RunState, Status};
use craft_cloud_kit::Badge;
use egui::{Color32, RichText, Ui};

/// How often [`CloudWatch`] asks the agent how it stands.
pub const STATUS_EVERY: Duration = Duration::from_secs(3);

/// What the account line shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentState {
    /// Not known yet (the first look is under way).
    Looking,
    /// Craft Cloud isn't running on this computer (or this app can't reach it): live features are off.
    NotRunning,
    SignedOut,
    SignedIn {
        account: String,
        server: String,
        machine: String,
        online: bool,
        paused: bool,
    },
}

impl AgentState {
    pub fn from_status(status: &Status) -> Self {
        if !status.signed_in {
            return Self::SignedOut;
        }
        Self::SignedIn {
            account: status.account.clone().unwrap_or_default(),
            server: host(status.server.as_deref().unwrap_or_default()),
            machine: status.machine.clone().unwrap_or_default(),
            online: status.online,
            paused: matches!(status.run_state, RunState::PausedByUser | RunState::Paused { .. }),
        }
    }
}

/// `https://cloud.example.com/` as `cloud.example.com`.
fn host(server: &str) -> String {
    let rest = server.split_once("://").map_or(server, |(_, r)| r);
    rest.trim_end_matches('/').to_owned()
}

/// The words a badge shows.
pub fn badge_text(badge: Badge) -> String {
    match badge {
        Badge::Live => "Live".to_owned(),
        Badge::Waiting(n) => format!("Waiting {n}"),
        Badge::ReadOnly => "Read-only".to_owned(),
        Badge::Offline => "Offline".to_owned(),
        Badge::Stalled => "Stalled".to_owned(),
        Badge::Stopped => "Stopped".to_owned(),
    }
}

/// The account and status line: "Craft Cloud · ana on cloud.example.com · online".
pub fn account_line(ui: &mut Ui, state: &AgentState) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Craft Cloud").strong());
        match state {
            AgentState::Looking => {
                ui.weak("…");
            }
            AgentState::NotRunning => {
                ui.weak("isn't running on this computer");
            }
            AgentState::SignedOut => {
                ui.weak("not signed in");
            }
            AgentState::SignedIn { account, server, machine, online, paused } => {
                ui.label(format!("{account} on {server}"));
                if !machine.is_empty() {
                    ui.weak(format!("({machine})"));
                }
                let (word, color) = match (paused, online) {
                    (true, _) => ("paused", Color32::from_rgb(0xb0, 0x80, 0x10)),
                    (false, true) => ("online", Color32::from_rgb(0x2a, 0x8a, 0x3e)),
                    (false, false) => ("offline", Color32::from_rgb(0xb0, 0x40, 0x30)),
                };
                ui.colored_label(color, word);
            }
        }
    });
}

/// The recent files, newest first, as buttons: the path of the one clicked.
pub fn recents(ui: &mut Ui, entries: &[RecentEntry], now_ms: i64) -> Option<String> {
    let mut clicked = None;
    if entries.is_empty() {
        ui.weak("No recent files");
        return None;
    }
    for entry in entries {
        ui.horizontal(|ui| {
            if ui.button(&entry.title).on_hover_text(&entry.path).clicked() {
                clicked = Some(entry.path.clone());
            }
            ui.weak(format!("{} · {}", entry.machine, ago(now_ms, entry.saved_ms.unwrap_or(entry.opened_ms))));
        });
    }
    clicked
}

/// How long ago, as people say it.
pub fn ago(now_ms: i64, then_ms: i64) -> String {
    let secs = now_ms.saturating_sub(then_ms).max(0) / 1000;
    match secs {
        0..=59 => "just now".to_owned(),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86_399 => format!("{} h ago", secs / 3600),
        _ => format!("{} d ago", secs / 86_400),
    }
}

/// A live document's badge (with what it means on hover).
pub fn badge(ui: &mut Ui, status: &craft_cloud_kit::Status) {
    let b = status.badge();
    let (color, hover) = match b {
        Badge::Live => (Color32::from_rgb(0x2a, 0x8a, 0x3e), "Edits go to your other computers as you make them".to_owned()),
        Badge::Waiting(n) => (Color32::from_rgb(0xb0, 0x80, 0x10), format!("{n} changes are waiting to reach Craft Cloud")),
        Badge::ReadOnly => (
            Color32::from_rgb(0x40, 0x60, 0xb0),
            match &status.read_only {
                Some(ro) if !ro.app.is_empty() => format!("Made with a newer version (layout {}): update the app to edit it", ro.version),
                _ => "This document can't be edited by this app".to_owned(),
            },
        ),
        Badge::Offline => (Color32::from_rgb(0xb0, 0x40, 0x30), "Edits are kept and go out when Craft Cloud is back".to_owned()),
        Badge::Stalled => (
            Color32::from_rgb(0xc0, 0x20, 0x20),
            "Stalled: too much is waiting for Craft Cloud, so new changes are not kept until it is back".to_owned(),
        ),
        Badge::Stopped => (
            Color32::from_rgb(0xc0, 0x20, 0x20),
            format!("Stopped: changes no longer go out ({}); save the document to keep them", status.error.as_deref().unwrap_or("see the agent")),
        ),
    };
    ui.colored_label(color, badge_text(b)).on_hover_text(hover);
}

/// Keeps an [`AgentState`] and an app's recents current, off the UI thread.
pub struct CloudWatch {
    state: Arc<Mutex<AgentState>>,
    recents: Option<RecentsWatch>,
    entries: Vec<RecentEntry>,
    stop: Arc<AtomicBool>,
}

impl std::fmt::Debug for CloudWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudWatch").finish_non_exhaustive()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl CloudWatch {
    /// Watches the agent `connect` finds (again and again: it may start later) for `app` (its lowercase name);
    /// `wake` is called when something changed (egui's `request_repaint`).
    pub fn start(app: &str, connect: impl Fn() -> Result<Client, craft_cloud_app::Error> + Send + 'static, wake: impl Fn() + Send + 'static) -> Self {
        let state = Arc::new(Mutex::new(AgentState::Looking));
        let stop = Arc::new(AtomicBool::new(false));
        let recents = connect().ok().and_then(|c| c.watch_recents(app).ok());
        let (s, halt) = (state.clone(), stop.clone());
        let _ = std::thread::Builder::new().name("craft-cloud-status".into()).spawn(move || {
            while !halt.load(Ordering::Relaxed) {
                let now = match connect().and_then(|c| c.status()) {
                    Ok(status) => AgentState::from_status(&status),
                    Err(_) => AgentState::NotRunning,
                };
                let changed = {
                    let mut held = lock(&s);
                    let changed = *held != now;
                    *held = now;
                    changed
                };
                if changed {
                    wake();
                }
                std::thread::sleep(STATUS_EVERY);
            }
        });
        let entries = recents.as_ref().map(RecentsWatch::entries).unwrap_or_default();
        Self { state, recents, entries, stop }
    }

    /// How the agent stands now.
    pub fn state(&self) -> AgentState {
        lock(&self.state).clone()
    }

    /// The recents now (takes what changed since the last frame).
    pub fn entries(&mut self) -> &[RecentEntry] {
        if let Some(watch) = &self.recents {
            while let Ok(event) = watch.events().try_recv() {
                if let RecentsEvent::Changed(list) = event {
                    self.entries = list;
                }
            }
        }
        &self.entries
    }

    /// Notes a file this app opened or saved in its recents (on every machine).
    pub fn add_recent(&self, path: &str, saved: bool) {
        if let Some(watch) = &self.recents {
            let _ = watch.add(NewRecent { path: path.to_owned(), title: None, saved });
        }
    }
}

impl Drop for CloudWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    fn signed_in(online: bool) -> AgentState {
        AgentState::SignedIn { account: "ana".into(), server: "cloud.example.com".into(), machine: "lptp1".into(), online, paused: false }
    }

    #[test]
    fn the_account_line_says_how_craft_cloud_stands() {
        for (state, words) in [
            (signed_in(true), vec!["ana on cloud.example.com", "online", "(lptp1)"]),
            (signed_in(false), vec!["offline"]),
            (AgentState::NotRunning, vec!["isn't running on this computer"]),
            (AgentState::SignedOut, vec!["not signed in"]),
        ] {
            let mut h = Harness::new_ui_state(|ui, s: &mut AgentState| account_line(ui, s), state);
            h.run();
            for w in words {
                assert!(h.query_by_label(w).is_some(), "{w}");
            }
        }
        let status = Status {
            api: 1,
            signed_in: true,
            account: Some("ana".into()),
            server: Some("https://cloud.example.com/".into()),
            folder: None,
            machine: Some("BEAST".into()),
            online: true,
            run_state: RunState::PausedByUser,
            autostart: None,
            last_sync_ms: None,
            removed: false,
        };
        assert!(matches!(AgentState::from_status(&status), AgentState::SignedIn { paused: true, ref server, .. } if server == "cloud.example.com"));
    }

    #[test]
    fn a_recent_file_opens_on_a_click() {
        let entries = vec![
            RecentEntry {
                node: None,
                path: "Notes/a.kitdemo".into(),
                title: "a.kitdemo".into(),
                opened_ms: 0,
                saved_ms: Some(50_000),
                machine: "BEAST".into(),
            },
            RecentEntry { node: None, path: "b.kitdemo".into(), title: "b.kitdemo".into(), opened_ms: 0, saved_ms: None, machine: "lptp1".into() },
        ];
        let mut h = Harness::new_ui_state(
            |ui, (entries, clicked): &mut (Vec<RecentEntry>, Option<String>)| {
                if let Some(p) = recents(ui, entries, 120_000) {
                    *clicked = Some(p);
                }
            },
            (entries, None),
        );
        h.run();
        assert!(h.query_by_label("BEAST · 1 min ago").is_some());
        h.get_by_label("b.kitdemo").click();
        h.run();
        assert_eq!(h.state().1.as_deref(), Some("b.kitdemo"));
        let mut empty = Harness::new_ui(|ui| {
            recents(ui, &[], 0);
        });
        empty.run();
        assert!(empty.query_by_label("No recent files").is_some());
    }

    #[test]
    fn the_badge_says_live_waiting_read_only_or_offline() {
        let live = craft_cloud_kit::Status { connected: true, online: true, ..Default::default() };
        let cases = [
            (live.clone(), "Live"),
            (craft_cloud_kit::Status { waiting: 3, ..live.clone() }, "Waiting 3"),
            (craft_cloud_kit::Status { read_only: Some(craft_cloud_kit::ReadOnly { app: "demo".into(), version: 2 }), ..live.clone() }, "Read-only"),
            (craft_cloud_kit::Status { connected: false, ..live.clone() }, "Offline"),
        ];
        for (status, word) in cases {
            let mut h = Harness::new_ui_state(|ui, s: &mut craft_cloud_kit::Status| badge(ui, s), status);
            h.run();
            assert!(h.query_by_label(word).is_some(), "{word}");
        }
        assert_eq!(ago(10_000_000, 10_000_000 - 7_200_000), "2 h ago");
    }
}
