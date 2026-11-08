//! The recent files of every Craft app, on every computer of the account, with Open for the ones that are in the synced folder here.

use egui::RichText;

use super::kit::{self, Column, Segment, Tokens, Tone};
use crate::files::{Files, app_label};
use crate::model::Event;

/// The height of a row of the files table.
const ROW_H: f32 = 64.0;

pub(super) fn show(ui: &mut egui::Ui, files: &Files, now_ms: i64, events: &mut Vec<Event>) {
    let t = Tokens::get(ui.ctx());
    super::heading(ui, "Files");
    kit::label_row(ui, &kit::numbered(1, "Recent files, every app and computer"), |ui| {
        if ui.add_enabled(!files.loading, kit::Button::new("Refresh")).clicked() {
            events.push(Event::LoadFiles);
        }
    });
    ui.add_space(8.0);
    if files.list.as_ref().is_some_and(|list| !list.is_empty()) {
        filters(ui, files, events);
        ui.add_space(8.0);
    }
    if files.loading {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Loading recent files…");
        });
    }
    if let Some(error) = &files.error {
        super::error_line(ui, &format!("Recent files aren't available right now. {error}"));
    }
    if let Some(error) = &files.open_error {
        super::error_line(ui, error);
    }
    let Some(list) = &files.list else { return };
    let visible = files.visible();
    if list.is_empty() {
        ui.label(RichText::new("No recent files yet. A file opened in a Craft app on any of your computers shows up here.").color(t.muted));
    } else if visible.is_empty() {
        ui.label(RichText::new("Nothing in the list matches the filter.").color(t.muted));
    } else {
        let columns = [
            Column::flex("File", 160.0).weighted(2.0),
            Column::fixed("App", 120.0),
            Column::fixed("Computer", 100.0),
            Column::fixed("Changed", 100.0),
            Column::fixed("", 90.0),
        ];
        kit::table(ui, "files", &columns, visible.len(), ROW_H, |ui, row, column| {
            let Some(file) = visible.get(row) else { return };
            match column {
                0 => {
                    kit::item_icon(ui, false, 20.0);
                    kit::two_lines(ui, ROW_H, file.name(), Some(&file.entry.path));
                }
                1 => {
                    kit::tag(ui, app_label(&file.app), Tone::Accent);
                }
                2 => {
                    kit::truncated(ui, &file.entry.machine);
                }
                3 => {
                    ui.label(super::ago(now_ms, file.when_ms()));
                }
                _ => {
                    let open = ui.add_enabled(file.place.reason().is_none(), kit::Button::new("Open").primary());
                    let open = match file.place.reason() {
                        Some(reason) => open.on_disabled_hover_text(reason),
                        None => open,
                    };
                    if open.clicked() {
                        events.push(Event::OpenRecent { app: file.app.clone(), path: file.entry.path.clone() });
                    }
                }
            }
        });
    }
    ui.add_space(8.0);
    ui.label(
        RichText::new("The list is Craft Cloud's shared recents: a file opened in an app on one computer shows up on the others. Open starts the file in its default program.")
            .small()
            .color(t.muted),
    );
}

/// The app filter ("All 3 / PhotoCraft 1 / WordCraft 2", the apps that have files) and the search field.
fn filters(ui: &mut egui::Ui, files: &Files, events: &mut Vec<Event>) {
    let (all, apps) = files.counts();
    let mut segments = vec![Segment::new("All").count(all)];
    segments.extend(apps.iter().map(|(app, n)| Segment::new(app_label(app)).count(*n)));
    let selected = files.app.as_ref().and_then(|selected| apps.iter().position(|(app, _)| app == selected)).map_or(0, |i| i + 1);
    // The segments, then the search field beside them when it fits and under them when it doesn't.
    let segments_width: f32 = segments.iter().map(|s| kit::section_label_width(ui, &format!("{} 0", s.text.to_uppercase())) + 28.0).sum();
    let search_width = kit::section_label_width(ui, "Search") + 220.0 + 16.0;
    let beside = ui.available_width() >= segments_width + search_width;
    let mut search_field = |ui: &mut egui::Ui| {
        let mut search = files.search.clone();
        if ui.add(kit::text_edit(&mut search).hint_text("Name or path").desired_width(220.0)).changed() {
            events.push(Event::FilesSearch(search));
        }
        kit::section_label(ui, "Search");
    };
    let mut picked = None;
    if beside {
        ui.horizontal(|ui| {
            picked = kit::segmented(ui, "App", &segments, selected);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), &mut search_field);
        });
    } else {
        picked = kit::segmented(ui, "App", &segments, selected);
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), &mut search_field);
        });
    }
    if let Some(i) = picked {
        events.push(Event::FilesApp(i.checked_sub(1).and_then(|n| apps.get(n)).map(|(app, _)| app.clone())));
    }
}
