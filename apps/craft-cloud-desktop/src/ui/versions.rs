//! Browse and restore versions of a file inside the sync folder.

use egui::RichText;

use super::confirm;
use super::kit::{self, Column, Tokens, Tone};
use crate::admin::format_bytes;
use crate::model::{Event, Versions};

/// The height of a row of the versions table.
const ROW_H: f32 = 60.0;
/// The width of the Load versions button, so the path field can take the rest of the row.
const LOAD_W: f32 = 170.0;

pub(super) fn show(ui: &mut egui::Ui, versions: &mut Versions, now_ms: i64, events: &mut Vec<Event>) {
    let t = Tokens::get(ui.ctx());
    let names = super::names(ui);
    let idle = !versions.loading && !versions.restoring;
    super::heading(ui, "File versions");
    kit::section_label(ui, &kit::numbered(1, "Path inside the sync folder"));
    ui.add_space(6.0);
    let mut path = versions.path.clone();
    ui.horizontal(|ui| {
        let width = (ui.available_width() - LOAD_W - ui.spacing().item_spacing.x).max(120.0);
        let response =
            ui.add_enabled_ui(!versions.restoring, |ui| ui.add(kit::text_edit(&mut path).hint_text("Projects/file.txt").desired_width(width))).inner;
        if response.changed() {
            events.push(Event::VersionsPath(path.clone()));
        }
        let load = kit::Button::new("Load versions").primary().min_size(egui::vec2(LOAD_W, kit::CONTROL_H));
        if ui.add_enabled(idle, load).clicked() {
            events.push(Event::LoadVersions);
        }
    });
    ui.add_space(8.0);
    if versions.loading {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Loading versions…");
        });
    }
    if let Some(error) = &versions.error {
        super::error_line(ui, error);
    }
    if let Some(notice) = &versions.notice {
        ui.label(RichText::new(notice).color(super::good(ui)));
    }
    if let Some(page) = &versions.page {
        if page.versions.is_empty() {
            ui.label("No versions found.");
        } else {
            let columns = [
                Column::flex("Saved", 200.0).weighted(2.0),
                Column::fixed("Kind", 110.0),
                Column::fixed("Size", 110.0),
                Column::flex("Computer", 140.0),
                Column::fixed("", 220.0),
            ];
            kit::table(ui, "versions", &columns, page.versions.len(), ROW_H, |ui, row, column| {
                let Some(version) = page.versions.get(row) else { return };
                let current = Some(version.id) == versions.current_version;
                match column {
                    0 => kit::two_lines(ui, ROW_H, &date(version.created_ms), Some(&super::ago(now_ms, version.created_ms))),
                    1 => {
                        if current {
                            kit::tag(ui, "Current", Tone::Good);
                        } else {
                            kit::tag(ui, "Saved", Tone::Neutral);
                        }
                    }
                    2 => {
                        ui.label(RichText::new(format_bytes(version.size)).monospace());
                    }
                    3 => {
                        let computer = names.computer(version.device);
                        let color = if computer.known { t.ink } else { t.muted };
                        ui.add(egui::Label::new(RichText::new(&computer.text).color(color)).truncate().selectable(false))
                            .on_hover_text(&computer.full);
                    }
                    _ => {
                        if version.pruned {
                            ui.label(RichText::new("can no longer be restored").color(t.muted));
                        } else if !current && ui.add_enabled(idle, kit::Button::new("Restore this version")).clicked() {
                            events.push(Event::ConfirmRestore(version.id));
                        }
                    }
                }
            });
        }
        if page.next.is_some() {
            ui.add_space(8.0);
            if ui.add_enabled(idle, kit::Button::new("More versions")).clicked() {
                events.push(Event::NextVersions);
            }
        }
    }
    match versions.confirm {
        Some(version) => {
            ui.add_space(12.0);
            let panel = confirm::Panel {
                key: egui::Id::new(("restore-version", version.to_string())),
                title: "Restore this version?",
                text: "This makes the selected version the current file. Sync will update this computer.",
                confirm: "Restore this version",
                cancel: "Cancel",
                danger: false,
                enabled: idle,
            };
            match confirm::show(ui, &panel) {
                confirm::Answer::Confirmed => events.push(Event::RestoreVersion),
                confirm::Answer::Cancelled => events.push(Event::CancelRestore),
                confirm::Answer::Open => {}
            }
        }
        None => confirm::idle(ui),
    }
}

fn date(timestamp_ms: i64) -> String {
    let days = timestamp_ms.div_euclid(86_400_000) + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_part = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = month_part + if month_part < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn dates_use_utc_calendar_days() {
        assert_eq!(super::date(0), "1970-01-01");
        assert_eq!(super::date(1_790_000_000_000), "2026-09-21");
    }
}
