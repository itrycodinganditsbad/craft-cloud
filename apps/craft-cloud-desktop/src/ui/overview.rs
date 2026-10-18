//! Signed in: "fabio on cloud.shadows.ch", online or offline and the last sync, what the agent is doing, Open folder,
//! Pause, Add a computer, the computers, Sign out and Quit.

use craft_cloud_app::{RunState, Status};
use egui::RichText;

use crate::model::{Busy, Event, Overview};

/// What the sync is doing, in one line, and the Details when it is a problem.
pub fn sync_line(status: &Status) -> (String, Option<String>) {
    let server = status.server.as_deref().map(super::host).unwrap_or("the server");
    match &status.run_state {
        RunState::Idle => ("Up to date".into(), None),
        RunState::Syncing => ("Syncing…".into(), None),
        RunState::Retrying { message } if !status.online => (format!("Can't reach {server} — trying again"), Some(message.clone())),
        RunState::Retrying { message } => ("Syncing failed — trying again".into(), Some(message.clone())),
        RunState::Paused { reason } => (super::problem_line(reason), Some(reason.clone())),
        RunState::PausedByUser => ("Paused: nothing syncs until you resume".into(), None),
        RunState::Stopping => ("Stopping…".into(), None),
        RunState::SignedOut => ("Not signed in".into(), None),
    }
}

pub(super) fn show(ui: &mut egui::Ui, o: &Overview, busy: Option<Busy>, now_ms: i64, autostart_problem: Option<&str>, events: &mut Vec<Event>) {
    let status = &o.status;
    let who = status.account.as_deref().unwrap_or("Signed in");
    let server = status.server.as_deref().map(super::host).unwrap_or_default();
    super::heading(ui, &format!("{who} on {server}"));
    let online = if status.online { RichText::new("Online").color(super::good(ui)) } else { RichText::new("Offline").weak() };
    ui.horizontal(|ui| {
        ui.label(online);
        if let Some(at) = status.last_sync_ms {
            ui.label(RichText::new(format!("· last sync {}", super::ago(now_ms, at))).weak());
        }
    });
    let (line, details) = sync_line(status);
    match details {
        Some(details) if matches!(status.run_state, RunState::Paused { .. }) => super::problem::line_with_details(ui, &line, &details),
        Some(details) => {
            ui.label(&line);
            egui::CollapsingHeader::new("Details").id_salt("sync-details").show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(details).monospace().small()).selectable(true).wrap());
            });
        }
        None => {
            ui.label(&line);
        }
    }
    if let Some(problem) = autostart_problem {
        ui.add_space(6.0);
        super::error_line(ui, problem);
        if ui.button("Try again").clicked() {
            events.push(Event::RetryAutostart);
        }
    }
    ui.add_space(12.0);

    ui.horizontal(|ui| {
        if ui.button("Open folder").clicked() {
            events.push(Event::OpenFolder);
        }
        let paused = status.run_state == RunState::PausedByUser;
        let (text, event) = if paused { ("Resume", Event::Resume) } else { ("Pause", Event::Pause) };
        if ui.add_enabled(busy.is_none(), egui::Button::new(text)).clicked() {
            events.push(event);
        }
    });
    ui.add_space(8.0);
    if super::wide_button(ui, true, "Add a computer") {
        events.push(Event::AddComputer);
    }
    ui.add_space(14.0);

    super::computers::list(ui, o, now_ms, events);
    ui.add_space(14.0);

    if let Some(error) = &o.error {
        super::error_line(ui, error);
        if o.offer_local_sign_out && ui.add_enabled(busy.is_none(), egui::Button::new("Sign out on this computer only")).clicked() {
            events.push(Event::SignOutHere);
        }
        ui.add_space(8.0);
    }
    ui.separator();
    ui.horizontal(|ui| {
        if ui.add_enabled(busy.is_none(), egui::Button::new("Sign out")).on_hover_text("The files stay on this computer").clicked() {
            events.push(Event::SignOut);
        }
        if ui.add_enabled(busy != Some(Busy::Quitting), egui::Button::new("Quit")).on_hover_text("Stops syncing until the next login").clicked() {
            events.push(Event::Quit);
        }
    });
}
