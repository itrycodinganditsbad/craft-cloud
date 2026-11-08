//! The home page's "On this computer": how much the files in the synced folder take here, by top-level folder and by kind.
//! These are the sizes of the files as this computer last synced them (not what the server stores for the account, which
//! counts chunks once and keeps history), so the section says so.

use craft_cloud_app::{FileKind, FolderUsageResponse};
use egui::RichText;

use super::kit::{self, Column, Tokens};
use crate::admin::format_bytes;
use crate::make_space::MakeSpaceMsg;
use crate::model::{Event, Overview};

/// The height of a row of the two tables.
const ROW_H: f32 = 40.0;
/// How many folders are listed by name; the rest, with the agent's own "other", are one row.
const SHOWN_FOLDERS: usize = 8;
/// From this width the two tables stand side by side.
const SIDE_BY_SIDE: f32 = 700.0;

/// A kind as the page names it.
pub fn kind_label(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Image => "Images",
        FileKind::Video => "Videos",
        FileKind::Audio => "Audio",
        FileKind::Document => "Documents",
        FileKind::Model3d => "3D models",
        FileKind::Archive => "Archives",
        FileKind::Code => "Code and text",
        FileKind::Other => "Other files",
    }
}

/// One row of the folders table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderRow {
    pub label: String,
    pub bytes: u64,
    pub files: u64,
    /// The folder's name in the personal folder, for a row that is one real folder ("Make a team space from this folder" is
    /// offered on it); none for the top level's loose files and for the rows that sum several folders.
    pub folder: Option<String>,
}

/// The rows of the folders table: the largest folders by name (the agent sorts them), then "Other" for every folder beyond
/// [`SHOWN_FOLDERS`] and for the ones the agent summed itself, when there are any.
pub fn folder_rows(data: &FolderUsageResponse) -> Vec<FolderRow> {
    let mut rows: Vec<FolderRow> = data
        .folders
        .iter()
        .take(SHOWN_FOLDERS)
        .map(|f| FolderRow {
            label: if f.name.is_empty() { "Top level (not in a folder)".to_owned() } else { f.name.clone() },
            bytes: f.bytes,
            files: f.files,
            folder: (!f.name.is_empty()).then(|| f.name.clone()),
        })
        .collect();
    let rest = data.folders.iter().skip(SHOWN_FOLDERS);
    let mut folders = u64::try_from(rest.clone().count()).unwrap_or(u64::MAX);
    let (mut bytes, mut files) = rest.fold((0u64, 0u64), |(b, n), f| (b.saturating_add(f.bytes), n.saturating_add(f.files)));
    if let Some(other) = data.other {
        folders = folders.saturating_add(other.folders);
        bytes = bytes.saturating_add(other.bytes);
        files = files.saturating_add(other.files);
    }
    if folders > 0 {
        let label = if folders == 1 { "Other (1 folder)".to_owned() } else { format!("Other ({folders} folders)") };
        rows.push(FolderRow { label, bytes, files, folder: None });
    }
    rows
}

/// The section: its label, what the numbers are, and the two tables (or why there are none).
pub(super) fn section(ui: &mut egui::Ui, number: usize, o: &Overview, events: &mut Vec<Event>) {
    let t = Tokens::get(ui.ctx());
    let folders = &o.folders;
    // An administrator may make a team space from a folder (the row gets a button); anyone else sees none.
    let offer = matches!(o.access, crate::admin::Access::Admin { .. });
    let can_start = o.can_make_space();
    kit::label_row(ui, &kit::numbered(number, "On this computer"), |_| {});
    ui.add_space(8.0);
    if folders.loading && folders.data.is_none() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Adding up the synced folder…");
        });
    }
    if let Some(error) = &folders.error {
        super::error_line(ui, &format!("The sizes aren't available right now. {error}"));
    }
    let Some(data) = &folders.data else { return };
    if data.total_files == 0 {
        ui.label(RichText::new("Nothing is synced into the folder here yet.").color(t.muted));
        return;
    }
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        let (amount, unit) = size_parts(data.total_bytes);
        ui.label(kit::headline_text(&amount, 28.0).color(t.ink_strong));
        ui.label(kit::headline_text(&unit, 16.0).color(t.ink_strong));
        ui.label(RichText::new(format!("in {} files on this computer", data.total_files)).color(t.muted));
    });
    ui.add_space(8.0);
    if ui.available_width() >= SIDE_BY_SIDE {
        ui.columns(2, |columns| {
            if let [left, right] = columns {
                folder_table(left, data, offer.then_some(can_start), events);
                kind_table(right, data);
            }
        });
    } else {
        folder_table(ui, data, offer.then_some(can_start), events);
        ui.add_space(12.0);
        kind_table(ui, data);
    }
    ui.add_space(8.0);
    ui.label(
        RichText::new(
            "On this computer: the sizes of the files in the synced folder as the last sync pass left them. They are not what the server \
             stores for your account, which counts shared parts once and keeps old versions.",
        )
        .small()
        .color(t.muted),
    );
}

/// A size as a number and its unit: ("1.3", "GiB").
fn size_parts(value: u64) -> (String, String) {
    let text = format_bytes(value);
    match text.split_once(' ') {
        Some((amount, unit)) => (amount.to_owned(), unit.to_owned()),
        None => (text, String::new()),
    }
}

/// The folders table. `offer`: `Some(enabled)` adds the last column with "Team space…" on each folder's row (an administrator's
/// call; disabled while the form is open or the agent is at work), `None` leaves it out.
fn folder_table(ui: &mut egui::Ui, data: &FolderUsageResponse, offer: Option<bool>, events: &mut Vec<Event>) {
    let rows = folder_rows(data);
    let mut columns = vec![Column::flex("Folder", 110.0), Column::fixed("Share", 70.0), Column::fixed("Size", 80.0), Column::fixed("Files", 56.0)];
    if offer.is_some() {
        columns.push(Column::fixed("", 132.0));
    }
    kit::table(ui, "folders-table", &columns, rows.len(), ROW_H, |ui, row, column| {
        let Some(r) = rows.get(row) else { return };
        if column == 4 {
            if let (Some(enabled), Some(folder)) = (offer, &r.folder) {
                let button = ui.add_enabled(enabled, kit::Button::new("Team space…"));
                if button.on_hover_text(format!("Make a team space from “{folder}”")).clicked() {
                    events.push(Event::MakeSpace(MakeSpaceMsg::Open(folder.clone())));
                }
            }
            return;
        }
        cell(ui, column, &r.label, r.bytes, r.files, data.total_bytes);
    });
}

fn kind_table(ui: &mut egui::Ui, data: &FolderUsageResponse) {
    let columns = [Column::flex("Kind", 110.0), Column::fixed("Share", 70.0), Column::fixed("Size", 80.0), Column::fixed("Files", 56.0)];
    kit::table(ui, "kinds-table", &columns, data.kinds.len(), ROW_H, |ui, row, column| {
        let Some(k) = data.kinds.get(row) else { return };
        cell(ui, column, kind_label(k.kind), k.bytes, k.files, data.total_bytes);
    });
}

/// A cell of either table: the name, a bar of its share of the whole, its size and its file count.
fn cell(ui: &mut egui::Ui, column: usize, label: &str, bytes: u64, files: u64, total: u64) {
    let t = Tokens::get(ui.ctx());
    match column {
        0 => {
            kit::truncated(ui, label);
        }
        1 => {
            let share = if total == 0 { 0.0 } else { (bytes as f64 / total as f64).min(1.0) as f32 };
            // Painted here, like the storage bar: egui's progress bar takes the (translucent) selection colour.
            let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 6.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, 0.0, t.line);
            ui.painter().rect_filled(egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * share, rect.height())), 0.0, t.accent);
        }
        2 => {
            ui.label(RichText::new(format_bytes(bytes)).monospace());
        }
        _ => {
            ui.label(RichText::new(files.to_string()).monospace().color(t.muted));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use craft_cloud_app::{FolderUsage, OtherFolders};

    fn usage(folders: Vec<FolderUsage>, other: Option<OtherFolders>) -> FolderUsageResponse {
        FolderUsageResponse { total_bytes: 100, total_files: 10, folders, other, kinds: Vec::new() }
    }

    fn folder(name: &str, bytes: u64) -> FolderUsage {
        FolderUsage { name: name.into(), bytes, files: 1 }
    }

    #[test]
    fn the_top_level_is_named_and_there_is_no_other_row_without_a_rest() {
        let rows = folder_rows(&usage(vec![folder("", 5), folder("Art", 3)], None));
        assert_eq!(rows.iter().map(|r| r.label.as_str()).collect::<Vec<_>>(), ["Top level (not in a folder)", "Art"]);
    }

    #[test]
    fn the_folders_beyond_the_shown_ones_and_the_agents_other_are_one_row() {
        let many = (0..SHOWN_FOLDERS + 2).map(|n| folder(&format!("f{n}"), 10)).collect();
        let rows = folder_rows(&usage(many, Some(OtherFolders { folders: 5, bytes: 7, files: 9 })));
        assert_eq!(rows.len(), SHOWN_FOLDERS + 1);
        assert_eq!(rows.last(), Some(&FolderRow { label: "Other (7 folders)".into(), bytes: 27, files: 11, folder: None }));
        let one = folder_rows(&usage((0..SHOWN_FOLDERS + 1).map(|n| folder(&format!("f{n}"), 1)).collect(), None));
        assert_eq!(one.last().map(|r| r.label.as_str()), Some("Other (1 folder)"));
    }

    #[test]
    fn every_kind_has_a_name() {
        let names: Vec<_> = FileKind::ALL.iter().map(|k| kind_label(*k)).collect();
        assert_eq!(names.len(), 8);
        assert!(names.iter().all(|n| !n.is_empty()));
    }
}
