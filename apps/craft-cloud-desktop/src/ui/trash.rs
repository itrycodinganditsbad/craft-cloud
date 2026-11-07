//! Browse deleted files and restore or remove them from the trash.

use egui::RichText;

use crate::model::{Event, Trash};

pub(super) fn show(ui: &mut egui::Ui, trash: &mut Trash, now_ms: i64, events: &mut Vec<Event>) {
    super::heading(ui, "Trash");
    if super::wide_button(ui, !trash.loading && trash.action.is_none(), "Refresh") {
        events.push(Event::LoadTrash);
    }
    if trash.loading {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Loading trash…");
        });
    }
    if let Some(error) = &trash.error {
        super::error_line(ui, error);
    }
    if let Some(notice) = &trash.notice {
        ui.label(RichText::new(notice).color(super::good(ui)));
    }
    if let Some(page) = &trash.page {
        for item in &page.items {
            super::kit::card(ui, None, |ui| {
                ui.label(RichText::new(&item.name).strong());
                if let Some(path) = &item.path {
                    ui.label(path);
                }
                let kind = if item.is_dir { "Folder" } else { "File" };
                ui.label(match item.size {
                    Some(size) => format!("{kind} · {size} bytes"),
                    None => format!("{kind} · size unavailable"),
                });
                ui.label(format!("Deleted {} · {}", super::ago(now_ms, item.deleted_ms), date(item.deleted_ms)));
                ui.label(format!("Removed for good after {}", date(item.purge_after_ms)));
                ui.horizontal(|ui| {
                    if ui.add_enabled(!trash.loading && trash.action.is_none(), super::kit::Button::new("Restore")).clicked() {
                        events.push(Event::RestoreTrash(item.id));
                    }
                    if ui.add_enabled(!trash.loading && trash.action.is_none(), super::kit::Button::new("Remove from trash").danger()).clicked() {
                        events.push(Event::ConfirmRemoveTrash(item.id));
                    }
                });
            });
            ui.add_space(4.0);
        }
        if page.items.is_empty() {
            ui.label("Trash is empty.");
        }
        if page.next.is_some() && super::wide_button(ui, !trash.loading && trash.action.is_none(), "More") {
            events.push(Event::NextTrash);
        }
    }
    if trash.action.is_some() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Updating trash…");
        });
    }
    if trash.confirm.is_some() {
        egui::Window::new("Remove from trash?").collapsible(false).resizable(false).show(ui.ctx(), |ui| {
            ui.label("This item can no longer be restored from the trash.");
            ui.horizontal(|ui| {
                if ui.add_enabled(!trash.loading && trash.action.is_none(), super::kit::Button::new("Remove from trash").danger()).clicked() {
                    events.push(Event::RemoveTrash);
                }
                if ui.add(super::kit::Button::new("Cancel")).clicked() {
                    events.push(Event::CancelRemoveTrash);
                }
            });
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
        assert_eq!(super::date(-1), "1969-12-31");
    }
}
