//! The recent files of every Craft app, on every computer of the account, with Open for the ones that are in the synced folder here;
//! and, beside them, the conflict copies in the synced folder, with Open and Show original.

use egui::RichText;

use super::kit::{self, Column, Segment, Tokens, Tone};
use crate::admin::format_bytes;
use crate::files::{ConflictFile, Files, FilesView, Place, app_label};
use crate::model::Event;

/// The height of a row of the files table.
const ROW_H: f32 = 64.0;

pub(super) fn show(ui: &mut egui::Ui, files: &Files, now_ms: i64, events: &mut Vec<Event>) {
    super::heading(ui, "Files");
    views(ui, files, events);
    ui.add_space(8.0);
    match files.view {
        FilesView::Recent => recent(ui, files, now_ms, events),
        FilesView::Conflicts => conflicts(ui, files, now_ms, events),
    }
}

/// The switch between the recent files and the conflict copies; the copies' count shows once they are listed.
fn views(ui: &mut egui::Ui, files: &Files, events: &mut Vec<Event>) {
    let copies = Segment::new("Conflict copies");
    let copies = match &files.conflicts.list {
        Some(list) => copies.count(list.files.len()),
        None => copies,
    };
    let segments = [Segment::new("Recent files"), copies];
    let selected = usize::from(files.view == FilesView::Conflicts);
    match kit::segmented(ui, "Files view", &segments, selected) {
        Some(0) if selected != 0 => events.push(Event::FilesView(FilesView::Recent)),
        Some(1) if selected != 1 => events.push(Event::FilesView(FilesView::Conflicts)),
        _ => {}
    }
}

fn recent(ui: &mut egui::Ui, files: &Files, now_ms: i64, events: &mut Vec<Event>) {
    let t = Tokens::get(ui.ctx());
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
        // The columns for a team space and for the people who have a file open appear only when some file has them.
        let (team, people) = (files.has_team(), files.has_open_now());
        let mut columns = vec![Column::flex("File", 130.0).weighted(2.0)];
        let space_at = team.then(|| {
            columns.push(Column::fixed("Space", 90.0));
            columns.len() - 1
        });
        columns.push(Column::fixed("App", 112.0));
        let app_at = columns.len() - 1;
        columns.push(Column::fixed("Computer", 90.0));
        let people_at = people.then(|| {
            columns.push(Column::fixed("Open now", 112.0));
            columns.len() - 1
        });
        columns.push(Column::fixed("Changed", 90.0));
        let changed_at = columns.len() - 1;
        columns.push(Column::fixed("", 146.0));
        kit::table(ui, "files", &columns, visible.len(), ROW_H, |ui, row, column| {
            let Some(file) = visible.get(row) else { return };
            match column {
                0 => {
                    kit::item_icon(ui, false, 20.0);
                    kit::two_lines(ui, ROW_H, file.name(), Some(&file.entry.path));
                }
                c if Some(c) == space_at => {
                    match &file.space {
                        Some(space) => kit::truncated(ui, &space.name),
                        None => kit::truncated(ui, RichText::new("Personal").color(t.muted)),
                    };
                }
                c if c == app_at => {
                    kit::tag(ui, app_label(&file.app), Tone::Accent);
                }
                c if Some(c) == people_at => super::people::open_now(ui, &file.entry.open_now),
                c if c == changed_at => {
                    ui.label(super::ago(now_ms, file.when_ms()));
                }
                c if c == app_at + 1 => {
                    kit::truncated(ui, &file.entry.machine);
                }
                _ => {
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        let open = ui.add_enabled(file.place.reason().is_none(), kit::Button::new("Open").primary());
                        let open = match file.place.reason() {
                            Some(reason) => open.on_disabled_hover_text(reason),
                            None => open,
                        };
                        if open.clicked() {
                            events.push(Event::OpenRecent {
                                app: file.app.clone(),
                                path: file.entry.path.clone(),
                                space: file.space.as_ref().map(|s| s.id),
                            });
                        }
                        // A file that is here can always be shown in its folder, also one that isn't opened from the page.
                        let here = matches!(file.place, Place::Here | Place::NotOpenable);
                        let show = ui.add_enabled_ui(here, |ui| kit::icon_button(ui, "Show in folder", kit::Icon::Folder)).inner;
                        let show = if here { show } else { show.on_disabled_hover_text("Show in folder: not on this computer") };
                        if show.clicked() {
                            events.push(Event::ShowRecent {
                                app: file.app.clone(),
                                path: file.entry.path.clone(),
                                space: file.space.as_ref().map(|s| s.id),
                            });
                        }
                    });
                }
            }
        });
    }
    ui.add_space(8.0);
    ui.label(
        RichText::new("The list is Craft Cloud's shared recents: a file opened in an app on one computer shows up on the others. Open starts the file in its default program; the folder button shows the file in its folder.")
            .small()
            .color(t.muted),
    );
}

/// The conflict copies: one row each with its original, computer, time and size, Open and Show original.
fn conflicts(ui: &mut egui::Ui, files: &Files, now_ms: i64, events: &mut Vec<Event>) {
    let t = Tokens::get(ui.ctx());
    let state = &files.conflicts;
    kit::label_row(ui, &kit::numbered(1, "Conflict copies on this computer"), |ui| {
        if ui.add_enabled(!files.loading && !state.loading, kit::Button::new("Refresh")).clicked() {
            events.push(Event::LoadFiles);
        }
    });
    ui.add_space(8.0);
    if state.loading {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Looking for conflict copies…");
        });
    }
    if let Some(error) = &state.error {
        super::error_line(ui, &format!("Conflict copies aren't available right now. {error}"));
    }
    if let Some(error) = &state.open_error {
        super::error_line(ui, error);
    }
    let Some(list) = &state.list else { return };
    if list.files.is_empty() {
        ui.label(
            RichText::new(
                "No conflict copies. When two computers change the same file at the same time, Craft Cloud keeps both and the copy shows up here.",
            )
            .color(t.muted),
        );
    } else {
        let columns = [
            Column::flex("Copy", 100.0).weighted(2.0),
            Column::fixed("Computer, time", 138.0),
            Column::fixed("Size", 76.0),
            Column::fixed("", 236.0),
        ];
        kit::table(ui, "conflicts", &columns, list.files.len(), ROW_H, |ui, row, column| {
            let Some(file) = list.files.get(row) else { return };
            match column {
                0 => {
                    kit::item_icon(ui, false, 20.0);
                    kit::two_lines(ui, ROW_H, file.name(), Some(&looks_like(file)));
                }
                1 => {
                    let when = file.made_at().unwrap_or_else(|| super::ago(now_ms, file.copy.mtime_ms));
                    kit::two_lines(ui, ROW_H, file.copy.device.as_deref().unwrap_or("—"), Some(&when));
                }
                2 => {
                    ui.label(RichText::new(format_bytes(file.copy.size)).monospace());
                }
                _ => {
                    let open = ui.add_enabled(file.place.reason().is_none(), kit::Button::new("Open").primary());
                    let open = match file.place.reason() {
                        Some(reason) => open.on_disabled_hover_text(reason),
                        None => open,
                    };
                    if open.clicked() {
                        events.push(Event::OpenConflictCopy(file.copy.path.clone()));
                    }
                    // The original's folder is shown also when the original is not a kind of file that is opened from the page.
                    let show = ui.add_enabled(matches!(file.original, Some(Place::Here | Place::NotOpenable)), kit::Button::new("Show original"));
                    let show = match file.original {
                        Some(Place::Here | Place::NotOpenable) => show,
                        Some(place) => show.on_disabled_hover_text(place.reason().unwrap_or("The original isn't here")),
                        None => show.on_disabled_hover_text("The copy's name doesn't say which file it is a copy of"),
                    };
                    if show.clicked() {
                        events.push(Event::ShowConflictOriginal(file.copy.path.clone()));
                    }
                }
            }
        });
        if list.truncated {
            ui.add_space(4.0);
            ui.label(RichText::new(format!("Showing the newest {}", list.files.len())).small().color(t.muted));
        }
    }
    ui.add_space(8.0);
    ui.label(
        RichText::new(
            "A file is listed when its name looks like the copy Craft Cloud keeps when two computers change a file at the same time, so a file you named that way yourself shows up too. The list reflects the last finished sync pass. Open starts the copy in its default program; Show original opens the folder of the file it is a copy of. Nothing here is deleted.",
        )
        .small()
        .color(t.muted),
    );
}

/// A conflict copy's second line: what the name says it is, and of which file.
fn looks_like(file: &ConflictFile) -> String {
    match &file.copy.original_path {
        Some(original) => format!("Looks like a conflict copy of {original}"),
        None => "Looks like a conflict copy".to_owned(),
    }
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
        if ui.add(kit::text_edit(&mut search).id(super::shortcuts::search_id()).hint_text("Name or path").desired_width(220.0)).changed() {
            events.push(Event::FilesSearch(search));
        }
        kit::section_label(ui, "Search");
    };
    // The segments when they fit the window's width, else a menu of the same choices (a dozen apps don't fit 640 points).
    let segments_fit = segments_width <= ui.available_width();
    let mut picked = None;
    let mut choose = |ui: &mut egui::Ui| {
        if segments_fit {
            picked = kit::segmented(ui, "App", &segments, selected);
            return;
        }
        let name = |segment: &Segment<'_>| segment.count.map_or_else(|| segment.text.to_owned(), |n| format!("{} ({n})", segment.text));
        let shown = segments.get(selected).map(name).unwrap_or_default();
        egui::ComboBox::from_id_salt("files-app").selected_text(shown).width(220.0).show_ui(ui, |ui| {
            for (i, segment) in segments.iter().enumerate() {
                if ui.selectable_label(i == selected, name(segment)).clicked() && i != selected {
                    picked = Some(i);
                }
            }
        });
    };
    if beside && segments_fit {
        ui.horizontal(|ui| {
            choose(ui);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), &mut search_field);
        });
    } else {
        choose(ui);
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), &mut search_field);
        });
    }
    if let Some(i) = picked {
        events.push(Event::FilesApp(i.checked_sub(1).and_then(|n| apps.get(n)).map(|(app, _)| app.clone())));
    }
}
