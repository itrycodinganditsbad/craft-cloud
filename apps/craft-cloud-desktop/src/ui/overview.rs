//! Signed in, the Overview tab: the sync (online or offline, what the agent is doing, the last sync, the folder, Open folder,
//! Pause), the storage and a short list of the computers with Add a computer; and the Computers tab ([`super::computers`]).
//! Sign out and Quit are in the footer ([`super::shell`]).

use craft_cloud_app::{AccountUsage, RunState, Status};
use egui::RichText;

use super::kit::{Tokens, Tone};
use crate::model::{Busy, Event, Overview, OverviewPage, Tab};

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

/// The width from which the home page's three cells sit side by side.
const SIDE_BY_SIDE: f32 = 840.0;
/// How many computers the home page's cell lists before pointing to the Computers tab.
const SHORT_LIST: usize = 4;

pub(super) fn show(ui: &mut egui::Ui, o: &Overview, busy: Option<Busy>, now_ms: i64, autostart_problem: Option<&str>, events: &mut Vec<Event>) {
    match o.page {
        OverviewPage::Home => home(ui, o, busy, now_ms, autostart_problem, events),
        OverviewPage::Computers => super::computers::page(ui, o, now_ms, events),
    }
}

fn home(ui: &mut egui::Ui, o: &Overview, busy: Option<Busy>, now_ms: i64, autostart_problem: Option<&str>, events: &mut Vec<Event>) {
    ui.horizontal(|ui| {
        super::kit::section_label(ui, &super::kit::numbered(1, "This computer"));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if let Some(machine) = &o.status.machine {
                super::kit::section_label(ui, machine);
            }
        });
    });
    ui.add_space(8.0);
    if ui.available_width() >= SIDE_BY_SIDE {
        ui.columns(3, |columns| {
            let [sync_column, storage_column, computers_column] = columns else { return };
            sync_cell(sync_column, o, busy, now_ms, autostart_problem, events);
            storage_cell(storage_column, o, events);
            computers_cell(computers_column, o, now_ms, events);
        });
    } else {
        sync_cell(ui, o, busy, now_ms, autostart_problem, events);
        ui.add_space(12.0);
        storage_cell(ui, o, events);
        ui.add_space(12.0);
        computers_cell(ui, o, now_ms, events);
    }
    ui.add_space(14.0);
    if let Some(error) = &o.error {
        super::error_line(ui, error);
    }
}

/// One cell of the home page: a numbered header strip with `right` at its right end, and the body under a hairline.
fn cell(ui: &mut egui::Ui, number: &str, tone: Tone, right: impl FnOnce(&mut egui::Ui), body: impl FnOnce(&mut egui::Ui)) {
    let t = Tokens::get(ui.ctx());
    egui::Frame::new().stroke(t.hairline()).fill(t.bg).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 0.0;
        egui::Frame::new().inner_margin(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                super::kit::badge(ui, number, tone);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), right);
            });
        });
        super::kit::hairline(ui);
        egui::Frame::new().inner_margin(egui::Margin::same(16)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 10.0;
            body(ui);
        });
    });
}

fn sync_cell(ui: &mut egui::Ui, o: &Overview, busy: Option<Busy>, now_ms: i64, autostart_problem: Option<&str>, events: &mut Vec<Event>) {
    let status = &o.status;
    let t = Tokens::get(ui.ctx());
    cell(
        ui,
        "01",
        Tone::Accent,
        |ui| {
            super::kit::section_label(ui, "Sync");
        },
        |ui| {
            if status.online {
                super::kit::tag(ui, "Online", Tone::Good);
            } else {
                super::kit::tag(ui, "Offline", Tone::Neutral);
            }
            let (line, details) = sync_line(status);
            match details {
                Some(details) if matches!(status.run_state, RunState::Paused { .. }) => super::problem::line_with_details(ui, &line, &details),
                Some(details) => {
                    ui.add(egui::Label::new(super::kit::headline_text(&line, 22.0).color(t.ink_strong)).wrap());
                    egui::CollapsingHeader::new("Details").id_salt("sync-details").show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(details).monospace().small()).selectable(true).wrap());
                    });
                }
                None => {
                    ui.add(egui::Label::new(super::kit::headline_text(&line, 22.0).color(t.ink_strong)).wrap());
                }
            }
            if let Some(at) = status.last_sync_ms {
                ui.label(RichText::new(format!("Last sync {}", super::ago(now_ms, at))).color(t.muted));
            }
            if let Some(folder) = &status.folder {
                egui::Frame::new().fill(t.sunken).stroke(t.hairline()).inner_margin(egui::Margin::symmetric(12, 10)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.add(egui::Label::new(RichText::new(folder).monospace()).wrap());
                });
            }
            if let Some(problem) = autostart_problem {
                super::error_line(ui, problem);
                if ui.add(super::kit::Button::new("Try again")).clicked() {
                    events.push(Event::RetryAutostart);
                }
            }
            ui.horizontal(|ui| {
                if ui.add(super::kit::Button::new("Open folder").primary()).clicked() {
                    events.push(Event::OpenFolder);
                }
                let paused = status.run_state == RunState::PausedByUser;
                let (text, event) = if paused { ("Resume", Event::Resume) } else { ("Pause", Event::Pause) };
                if ui.add_enabled(busy.is_none(), super::kit::Button::new(text)).clicked() {
                    events.push(event);
                }
            });
        },
    );
}

fn storage_cell(ui: &mut egui::Ui, o: &Overview, events: &mut Vec<Event>) {
    cell(
        ui,
        "02",
        Tone::Warn,
        |ui| {
            super::kit::section_label(ui, "Storage");
        },
        |ui| {
            if o.usage_loading {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Loading storage usage…");
                });
            } else if let Some(error) = &o.usage_error {
                super::error_line(ui, "Storage unavailable");
                ui.label(RichText::new("Details").small().weak()).on_hover_text(error);
            } else if let Some(usage) = &o.usage {
                usage_lines(ui, usage);
            }
            if ui.add_enabled(!o.usage_loading, super::kit::Button::new("Refresh")).clicked() {
                events.push(Event::RefreshUsage);
            }
        },
    );
}

fn computers_cell(ui: &mut egui::Ui, o: &Overview, now_ms: i64, events: &mut Vec<Event>) {
    let t = Tokens::get(ui.ctx());
    let mut open_computers = false;
    cell(
        ui,
        "03",
        Tone::Good,
        |ui| open_computers = ui.link("All computers").clicked(),
        |ui| {
            if let Some(error) = &o.devices_error {
                super::error_line(ui, error);
            }
            match &o.devices {
                None if o.devices_error.is_none() => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Asking the server…");
                    });
                }
                None => {}
                Some(devices) => {
                    let active: Vec<_> = devices.iter().filter(|d| !d.revoked).collect();
                    for device in active.iter().take(SHORT_LIST) {
                        super::kit::table_row(ui, |ui| {
                            ui.vertical(|ui| {
                                ui.label(RichText::new(&device.name).strong());
                                if device.current {
                                    ui.label(RichText::new("this computer").small().color(t.muted));
                                }
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if device.current {
                                    if o.status.online {
                                        super::kit::tag(ui, "Online", Tone::Good);
                                    } else {
                                        super::kit::tag(ui, "Offline", Tone::Neutral);
                                    }
                                } else {
                                    ui.label(RichText::new(format!("seen {}", super::ago(now_ms, device.last_used_ms))).small().color(t.muted));
                                }
                            });
                        });
                    }
                    if active.len() > SHORT_LIST && ui.link(format!("{} more", active.len() - SHORT_LIST)).clicked() {
                        events.push(Event::Tab(Tab::Computers));
                    }
                }
            }
            if super::wide_button(ui, true, "Add a computer") {
                events.push(Event::AddComputer);
            }
        },
    );
    if open_computers {
        events.push(Event::Tab(Tab::Computers));
    }
}

fn usage_lines(ui: &mut egui::Ui, usage: &AccountUsage) {
    let t = Tokens::get(ui.ctx());
    let (amount, unit) = bytes_parts(displayed_total_bytes(usage));
    ui.horizontal_wrapped(|ui| {
        ui.label(super::kit::headline_text(&amount, 36.0).color(t.ink_strong));
        ui.label(super::kit::headline_text(unit, 18.0).color(t.ink_strong));
        match usage.quota_bytes {
            Some(quota) => ui.label(RichText::new(format!("of {}", bytes(quota))).color(t.muted)),
            None => ui.label(RichText::new("used · no limit").color(t.muted)),
        };
    });
    if let Some(quota) = usage.quota_bytes {
        let share = if quota == 0 { 0.0 } else { (displayed_total_bytes(usage) as f64 / quota as f64).min(1.0) as f32 };
        // Painted here: egui's progress bar takes the (translucent) selection colour.
        let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 6.0), egui::Sense::hover());
        ui.painter().rect_filled(rect, 0.0, t.line);
        ui.painter().rect_filled(egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * share, rect.height())), 0.0, t.accent);
    }
    row(ui, "Your files", &bytes(displayed_total_bytes(usage)), false);
    if let (true, Some(trash), Some(history)) = (usage.estimates.index_complete, usage.trash_bytes, usage.history_bytes) {
        row(ui, "In the trash", &format!("about {}", bytes(trash)), true);
        row(ui, "Old versions", &format!("about {}", bytes(history)), true);
        ui.label(RichText::new("Trash and old versions are shown apart, not added to the total.").small().color(t.muted))
            .on_hover_text("Trash and old-version estimates overlap; they are not added to stored bytes.");
    } else {
        ui.label("Estimates not available yet");
    }
}

/// A label at the left and a value at the right, over a hairline.
fn row(ui: &mut egui::Ui, label: &str, value: &str, muted: bool) {
    let t = Tokens::get(ui.ctx());
    let color = if muted { t.muted } else { t.ink };
    super::kit::table_row(ui, |ui| {
        ui.label(RichText::new(label).color(color));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(value).monospace().color(color));
        });
    });
}

fn displayed_total_bytes(usage: &AccountUsage) -> u64 {
    usage.stored_bytes
}

/// `value` as a number and its unit, for the big figure: ("1.3", "GiB").
fn bytes_parts(value: u64) -> (String, &'static str) {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut amount = value as f64;
    let mut unit = 0;
    while amount >= 1024.0 && unit < UNITS.len() - 1 {
        amount /= 1024.0;
        unit += 1;
    }
    let name = UNITS.get(unit).copied().unwrap_or("GiB");
    if unit == 0 { (value.to_string(), name) } else { (format!("{amount:.1}"), name) }
}

fn bytes(value: u64) -> String {
    let (amount, unit) = bytes_parts(value);
    format!("{amount} {unit}")
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

    #[test]
    fn sizes_read_as_a_number_and_a_unit() {
        assert_eq!(bytes_parts(11), ("11".to_owned(), "B"));
        assert_eq!(bytes_parts(1_395_864_371), ("1.3".to_owned(), "GiB"));
        assert_eq!(bytes(5 * 1_073_741_824), "5.0 GiB");
    }
}
