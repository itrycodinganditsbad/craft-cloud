//! The Computers tab: the account's computers in a table (name, kind, platform, version, when each was last seen), Rename
//! (the row becomes a text field), Remove (this computer has none: that is Sign out), Add a computer and Sign out other
//! computers (which opens the Security page at its confirmation).

use craft_cloud_app::{DeviceId, DeviceInfo};
use egui::RichText;

use super::kit::{Column, JoinedCell, Part, Tokens, Tone};
use crate::model::{Event, Overview, Rename};

/// This app's version: the agent and the app are built from one version, so a computer reporting an older one is behind.
const OWN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// What a cell shows for a detail the computer didn't send (an older agent leaves them out).
const NO_DETAIL: &str = "—";

/// The width from which the title and the buttons share a row.
const HEADER_ONE_ROW: f32 = 720.0;

/// The narrowest the Computer column gets (a long name ends with "…").
const COMPUTER_MIN_WIDTH: f32 = 280.0;

/// The height of a row: a name over its date, or the rename field under its label. One height for both, so opening the
/// rename field doesn't move the rows below it.
const ROW_HEIGHT: f32 = 64.0;

/// The width from which the two notes under the table sit side by side.
const NOTES_SIDE_BY_SIDE: f32 = 560.0;

pub(super) fn page(ui: &mut egui::Ui, o: &mut Overview, now_ms: i64, events: &mut Vec<Event>) {
    let active: Vec<DeviceInfo> = o.devices.iter().flatten().filter(|d| !d.revoked).cloned().collect();
    let others = active.iter().any(|d| !d.current);
    let title = |ui: &mut egui::Ui| {
        ui.vertical(|ui| {
            super::kit::section_label(ui, &super::kit::numbered(1, "Computers on this account"));
            if o.devices.is_some() {
                super::heading(ui, &if active.len() == 1 { "1 computer".to_owned() } else { format!("{} computers", active.len()) });
            }
        });
    };
    let buttons = |ui: &mut egui::Ui, events: &mut Vec<Event>| {
        if ui.add(super::kit::Button::new("Add a computer").primary()).clicked() {
            events.push(Event::AddComputer);
        }
        if ui
            .add_enabled(others, super::kit::Button::new("Sign out other computers"))
            .on_hover_text("Asks once more; this computer stays signed in")
            .clicked()
        {
            events.push(Event::SignOutOtherComputers);
        }
        if ui.add(super::kit::Button::new("Refresh")).clicked() {
            events.push(Event::RefreshDevices);
        }
    };
    if ui.available_width() >= HEADER_ONE_ROW {
        ui.horizontal(|ui| {
            title(ui);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| buttons(ui, events));
        });
    } else {
        title(ui);
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| buttons(ui, events));
        });
    }
    if let Some(error) = &o.devices_error {
        super::error_line(ui, error);
    }
    if o.devices.is_none() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Asking the server…");
        });
        return;
    }
    ui.add_space(8.0);
    table(ui, o, &active, now_ms, events);
    ui.add_space(16.0);
    notes(ui);
}

fn table(ui: &mut egui::Ui, o: &mut Overview, active: &[DeviceInfo], now_ms: i64, events: &mut Vec<Event>) {
    let (online, removing) = (o.status.online, o.removing);
    let rename_open = o.renaming.is_some();
    // The error of a rename sits under the table, so the row keeps its height.
    let rename_error = o.renaming.as_ref().and_then(|r| r.error.clone());
    let columns = [
        Column::flex("Computer", COMPUTER_MIN_WIDTH),
        Column::fixed("Kind", 84.0),
        Column::fixed("Platform", 104.0),
        Column::fixed("Version", 136.0),
        Column::fixed("Last seen", 124.0),
        Column::fixed("", 214.0),
    ];
    let renaming = &mut o.renaming;
    super::kit::table(ui, "computers", &columns, active.len(), ROW_HEIGHT, |ui, row, column| {
        let Some(device) = active.get(row) else { return };
        let editing = renaming.as_mut().filter(|r| r.id == device.id);
        cell(ui, device, column, editing, (online, removing, rename_open), now_ms, events);
    });
    if let Some(error) = rename_error {
        ui.add_space(6.0);
        super::error_line(ui, &error);
    }
}

/// One cell of a computer's row. `editing` is its rename, when its name is being changed.
fn cell(
    ui: &mut egui::Ui,
    device: &DeviceInfo,
    column: usize,
    editing: Option<&mut Rename>,
    (online, removing, rename_open): (bool, Option<DeviceId>, bool),
    now_ms: i64,
    events: &mut Vec<Event>,
) {
    let t = Tokens::get(ui.ctx());
    match column {
        0 => match editing {
            Some(rename) => rename_field(ui, device, rename, events),
            None => {
                let focused = egui::Id::new(("rename-focused", device.id));
                ui.data_mut(|d| d.remove::<bool>(focused));
                let name = if device.current { format!("{} (this computer)", device.name) } else { device.name.clone() };
                let date = (device.created_ms > 0).then(|| format!("signed in {}", super::trash::date(device.created_ms)));
                super::kit::two_lines(ui, ROW_HEIGHT, &name, date.as_deref());
            }
        },
        1 => {
            let kind = device.client_kind.trim();
            if kind.is_empty() {
                ui.label(RichText::new(NO_DETAIL).color(t.faint));
            } else {
                ui.label(RichText::new(kind.to_uppercase()).monospace().size(12.0).extra_letter_spacing(1.0));
            }
        }
        2 => detail(ui, &platform_label(&device.platform), false),
        3 => {
            if crate::model::is_older_version(&device.app_version, OWN_VERSION) {
                ui.label(RichText::new(&device.app_version).monospace().color(t.warn));
                super::kit::tag(ui, "Older", Tone::Warn).on_hover_text(format!("This app is {OWN_VERSION}"));
            } else {
                detail(ui, &device.app_version, true);
            }
        }
        4 => {
            if device.current {
                if online {
                    super::kit::tag(ui, "Online", Tone::Good);
                } else {
                    super::kit::tag(ui, "Offline", Tone::Neutral);
                }
            } else {
                super::kit::truncated(ui, super::ago(now_ms, device.last_used_ms));
            }
        }
        _ => actions(ui, device, editing, (removing, rename_open), events),
    }
}

/// The rename field in the Computer column: a label over a field that fills the column.
fn rename_field(ui: &mut egui::Ui, device: &DeviceInfo, rename: &mut Rename, events: &mut Vec<Event>) {
    let focused = egui::Id::new(("rename-focused", device.id));
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 4.0;
        super::kit::section_label_fit(ui, "Rename this computer", ui.available_width());
        let field = super::kit::text_edit(&mut rename.name).id(egui::Id::new(("rename-field", device.id))).desired_width(ui.available_width());
        let response = ui.add_enabled(!rename.busy, field);
        // The field has the keyboard when the row opens, once.
        if !ui.data(|d| d.get_temp::<bool>(focused).unwrap_or(false)) {
            response.request_focus();
            ui.data_mut(|d| d.insert_temp(focused, true));
        }
        // egui drops the focus on Escape before the field reports it, so a field that lost it this frame counts.
        if (response.has_focus() || response.lost_focus()) && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            events.push(Event::CancelRename);
        } else if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            events.push(Event::SaveRename);
        }
    });
}

/// The last column: Rename and Remove, or Cancel and Save while the name is being changed (the rightmost is added first).
fn actions(
    ui: &mut egui::Ui,
    device: &DeviceInfo,
    editing: Option<&mut Rename>,
    (removing, rename_open): (Option<DeviceId>, bool),
    events: &mut Vec<Event>,
) {
    match editing {
        Some(rename) => {
            let save = super::kit::Button::new(if rename.busy { "Saving…" } else { "Save" }).primary();
            if ui.add_enabled(!rename.busy, save).clicked() {
                events.push(Event::SaveRename);
            }
            if ui.add_enabled(!rename.busy, super::kit::Button::new("Cancel")).clicked() {
                events.push(Event::CancelRename);
            }
        }
        None => {
            if !device.current {
                let this = removing == Some(device.id);
                let button = super::kit::Button::new(if this { "Removing…" } else { "Remove" }).danger();
                let response = ui.add_enabled(removing.is_none(), button).on_hover_text("It can't sync any more; its files stay there");
                if response.clicked() {
                    events.push(Event::RemoveDevice(device.id));
                }
            }
            if ui.add_enabled(!rename_open, super::kit::Button::new("Rename")).on_hover_text(format!("Give {} another name", device.name)).clicked() {
                events.push(Event::StartRename(device.id));
            }
        }
    }
}

/// A detail cell: the text in the muted colour, or a dash when the computer didn't send it.
fn detail(ui: &mut egui::Ui, text: &str, mono: bool) {
    let t = Tokens::get(ui.ctx());
    let text = text.trim();
    if text.is_empty() {
        ui.label(RichText::new(NO_DETAIL).color(t.faint));
    } else if mono {
        ui.label(RichText::new(text).monospace().color(t.muted));
    } else {
        super::kit::truncated(ui, RichText::new(text).color(t.muted));
    }
}

/// The platform as people write it: `windows` is `Windows`, `macos` is `macOS`, `linux` is `Linux`; anything else as the
/// computer sent it, and nothing (shown as a dash) when it sent nothing.
pub(super) fn platform_label(platform: &str) -> String {
    let platform = platform.trim();
    match platform.to_ascii_lowercase().as_str() {
        "windows" => "Windows".to_owned(),
        "macos" => "macOS".to_owned(),
        "linux" => "Linux".to_owned(),
        _ => platform.to_owned(),
    }
}

/// What Add a computer and Remove do, in one strip of two cells.
fn notes(ui: &mut egui::Ui) {
    let cells = [JoinedCell::new("01", Tone::Accent, "Add a computer"), JoinedCell::new("02", Tone::Warn, "Removing a computer")];
    super::kit::joined_cells(ui, "computers-notes", &cells, NOTES_SIDE_BY_SIDE, |ui, i, part| {
        if part != Part::Body {
            return;
        }
        let text = match i {
            0 => "Make a one-time code here and type it on the other computer. It is valid for 10 minutes.",
            _ => "It is signed out at once. Its files stay on its disk; nothing is deleted there.",
        };
        ui.add(egui::Label::new(text).wrap());
    });
}

#[cfg(test)]
mod tests {
    use super::platform_label;

    #[test]
    fn platforms_are_written_as_people_write_them() {
        assert_eq!(platform_label("windows"), "Windows");
        assert_eq!(platform_label(" macos "), "macOS");
        assert_eq!(platform_label("Linux"), "Linux");
        assert_eq!(platform_label("freebsd"), "freebsd");
        assert_eq!(platform_label("  "), "");
    }
}
