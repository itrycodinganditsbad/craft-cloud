//! Signed in: "fabio on cloud.shadows.ch", online or offline and the last sync, what the agent is doing, Open folder,
//! Pause, Add a computer, the computers, Sign out and Quit.

use craft_cloud_app::{AccountUsage, RunState, Status};
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
    storage(ui, o, events);
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

fn storage(ui: &mut egui::Ui, o: &Overview, events: &mut Vec<Event>) {
    ui.group(|ui| {
        ui.horizontal(|ui| {
            ui.strong("Storage");
            if ui.add_enabled(!o.usage_loading, egui::Button::new("Refresh")).clicked() {
                events.push(Event::RefreshUsage);
            }
        });
        if o.usage_loading {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Loading storage usage…");
            });
        } else if let Some(error) = &o.usage_error {
            super::error_line(ui, "Storage unavailable");
            ui.label(egui::RichText::new("Details").small().weak()).on_hover_text(error);
        } else if let Some(usage) = &o.usage {
            usage_lines(ui, usage);
        }
    });
}

fn usage_lines(ui: &mut egui::Ui, usage: &AccountUsage) {
    match usage.quota_bytes {
        Some(quota) => {
            ui.label(format!("{} of {} used", bytes(displayed_total_bytes(usage)), bytes(quota)));
            ui.add(
                egui::ProgressBar::new(if quota == 0 { 0.0 } else { (displayed_total_bytes(usage) as f64 / quota as f64).min(1.0) as f32 })
                    .show_percentage(),
            );
        }
        None => {
            ui.label(format!("{} used · no limit", bytes(displayed_total_bytes(usage))));
        }
    };
    if let (true, Some(trash), Some(history)) = (usage.estimates.index_complete, usage.trash_bytes, usage.history_bytes) {
        ui.label(format!("about {} in trash", bytes(trash)));
        ui.label(format!("about {} in old versions", bytes(history)))
            .on_hover_text("Trash and old-version estimates overlap; they are not added to stored bytes.");
    } else {
        ui.label("Estimates not available yet");
    }
}

fn displayed_total_bytes(usage: &AccountUsage) -> u64 {
    usage.stored_bytes
}

fn bytes(value: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut amount = value as f64;
    let mut unit = 0;
    while amount >= 1024.0 && unit < UNITS.len() - 1 {
        amount /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{value} B") } else { format!("{amount:.1} {}", UNITS[unit]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trash_and_history_are_never_added_to_the_stored_total() {
        let usage = AccountUsage {
            stored_bytes: 11,
            quota_bytes: None,
            trash_bytes: Some(20),
            history_bytes: Some(30),
            estimates: craft_cloud_proto::api::UsageEstimates { index_complete: true, reason: None },
        };
        assert_eq!(displayed_total_bytes(&usage), 11);
    }
}
