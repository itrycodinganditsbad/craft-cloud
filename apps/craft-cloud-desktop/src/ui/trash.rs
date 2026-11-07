//! Browse deleted files and restore or remove them from the trash.

use egui::RichText;

use super::confirm;
use super::kit::{self, Column, Segment, Tokens, Tone};
use crate::admin::format_bytes;
use crate::model::{Event, Trash, TrashKind};

/// The height of a row of the trash table.
const ROW_H: f32 = 64.0;

pub(super) fn show(ui: &mut egui::Ui, trash: &mut Trash, now_ms: i64, events: &mut Vec<Event>) {
    let t = Tokens::get(ui.ctx());
    let idle = !trash.loading && trash.action.is_none();
    super::heading(ui, "Trash");
    kit::label_row(ui, &kit::numbered(1, "Deleted items"), |ui| {
        if ui.add_enabled(idle, kit::Button::new("Refresh")).clicked() {
            events.push(Event::LoadTrash);
        }
    });
    ui.add_space(8.0);
    if trash.page.is_some() {
        filters(ui, trash, events);
        ui.add_space(8.0);
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
        let visible = trash.visible();
        if page.items.is_empty() {
            ui.label("Trash is empty.");
        } else if visible.is_empty() {
            ui.label(RichText::new("Nothing in the loaded list matches the filter.").color(t.muted));
        } else {
            let columns = [
                Column::flex("Name", 190.0).weighted(2.0),
                Column::fixed("Kind", 100.0),
                Column::fixed("Size", 90.0),
                Column::fixed("Deleted", 100.0),
                Column::fixed("Removed for good", 160.0),
                Column::fixed("", 240.0),
            ];
            kit::table(ui, "trash", &columns, visible.len(), ROW_H, |ui, row, column| {
                let Some(item) = visible.get(row) else { return };
                match column {
                    0 => {
                        kit::item_icon(ui, item.is_dir, 20.0);
                        kit::two_lines(ui, ROW_H, &item.name, item.path.as_deref());
                    }
                    1 => {
                        if item.is_dir {
                            kit::tag(ui, "Folder", Tone::Accent);
                        } else {
                            kit::tag(ui, "File", Tone::Neutral);
                        }
                    }
                    2 => match item.size {
                        Some(size) => {
                            ui.label(RichText::new(format_bytes(size)).monospace());
                        }
                        None => {
                            ui.label(RichText::new("unknown").color(t.muted));
                        }
                    },
                    3 => {
                        ui.label(super::ago(now_ms, item.deleted_ms)).on_hover_text(date(item.deleted_ms));
                    }
                    4 => {
                        ui.label(RichText::new(date(item.purge_after_ms)).color(t.muted));
                    }
                    _ => {
                        // Right to left: the last one added is the leftmost.
                        let remove = ui.add_enabled(idle, kit::Button::new("Remove").danger()).on_hover_text("Remove from trash for good");
                        if remove.clicked() {
                            events.push(Event::ConfirmRemoveTrash(item.id));
                        }
                        if ui.add_enabled(idle, kit::Button::new("Restore").primary()).clicked() {
                            events.push(Event::RestoreTrash(item.id));
                        }
                    }
                }
            });
        }
        if page.next.is_some() {
            ui.add_space(8.0);
            if ui.add_enabled(idle, kit::Button::new("More")).clicked() {
                events.push(Event::NextTrash);
            }
        }
    }
    if trash.action.is_some() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Updating trash…");
        });
    }
    match trash.confirm {
        Some(id) => {
            ui.add_space(12.0);
            let name = trash.page.iter().flat_map(|p| p.items.iter()).find(|i| i.id == id).map_or("this item", |i| i.name.as_str());
            let panel = confirm::Panel {
                key: egui::Id::new(("remove-from-trash", id.to_string())),
                title: &format!("Remove {name}?"),
                text: "This item can no longer be restored from the trash.",
                confirm: "Remove",
                danger: true,
                enabled: idle,
            };
            match confirm::show(ui, &panel) {
                confirm::Answer::Confirmed => events.push(Event::RemoveTrash),
                confirm::Answer::Cancelled => events.push(Event::CancelRemoveTrash),
                confirm::Answer::Open => {}
            }
        }
        None => confirm::idle(ui),
    }
}

/// The kind filter ("All 3 / Folders 1 / Files 2") and the search field, which filter the loaded list.
fn filters(ui: &mut egui::Ui, trash: &Trash, events: &mut Vec<Event>) {
    let (all, folders, files) = trash.counts();
    let segments = [Segment::new("All").count(all), Segment::new("Folders").count(folders), Segment::new("Files").count(files)];
    let selected = match trash.kind {
        TrashKind::All => 0,
        TrashKind::Folders => 1,
        TrashKind::Files => 2,
    };
    ui.horizontal(|ui| {
        if let Some(i) = kit::segmented(ui, "Kind", &segments, selected) {
            events.push(Event::TrashKind([TrashKind::All, TrashKind::Folders, TrashKind::Files].get(i).copied().unwrap_or_default()));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let mut search = trash.search.clone();
            if ui.add(kit::text_edit(&mut search).hint_text("Name or path").desired_width(220.0)).changed() {
                events.push(Event::TrashSearch(search));
            }
            kit::section_label(ui, "Search");
        });
    });
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
