//! Browse and restore versions of a file inside the sync folder.

use egui::RichText;

use crate::model::{Event, Versions};

pub(super) fn show(ui: &mut egui::Ui, versions: &mut Versions, now_ms: i64, events: &mut Vec<Event>) {
    super::heading(ui, "File versions");
    if ui.add(super::kit::Button::new("Back")).clicked() {
        events.push(Event::BackVersions);
    }
    ui.add_space(8.0);
    ui.label("Path inside the sync folder");
    let mut path = versions.path.clone();
    let response = ui.add_enabled_ui(!versions.restoring, |ui| ui.add(egui::TextEdit::singleline(&mut path).hint_text("Projects/file.txt"))).inner;
    if response.changed() {
        events.push(Event::VersionsPath(path));
    }
    if super::wide_button(ui, !versions.loading && !versions.restoring, "Load versions") {
        events.push(Event::LoadVersions);
    }
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
        ui.add_space(8.0);
        for version in &page.versions {
            let current = Some(version.id) == versions.current_version;
            super::kit::card(ui, None, |ui| {
                ui.label(RichText::new(if current { "Current version" } else { "Saved version" }).strong());
                ui.label(format!("{} · {} · {} bytes", date(version.created_ms), super::ago(now_ms, version.created_ms), version.size));
                ui.label(format!("Device {}", version.device));
                if version.pruned {
                    ui.label("This version can no longer be restored");
                } else if !current
                    && ui.add_enabled(!versions.loading && !versions.restoring, super::kit::Button::new("Restore this version")).clicked()
                {
                    events.push(Event::ConfirmRestore(version.id));
                }
            });
            ui.add_space(4.0);
        }
        if page.versions.is_empty() {
            ui.label("No versions found.");
        }
        if page.next.is_some() && super::wide_button(ui, !versions.loading && !versions.restoring, "More versions") {
            events.push(Event::NextVersions);
        }
    }
    if let Some(version) = versions.confirm {
        egui::Window::new("Restore version?").collapsible(false).resizable(false).show(ui.ctx(), |ui| {
            ui.label("This makes the selected version the current file. Sync will update this computer.");
            ui.horizontal(|ui| {
                if ui.add_enabled(!versions.restoring && !versions.loading, super::kit::Button::new("Restore this version")).clicked() {
                    events.push(Event::RestoreVersion);
                }
                if ui.add(super::kit::Button::new("Cancel")).clicked() {
                    events.push(Event::CancelRestore);
                }
            });
            let _ = version;
        });
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
