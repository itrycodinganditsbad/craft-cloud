//! The Computers tab: the account's computers in a table (name, kind, platform, version, when each was last seen), Rename
//! (the row becomes a text field), Remove (this computer has none: that is Sign out), Add a computer and Sign out other
//! computers (which opens the Security page at its confirmation).

use craft_cloud_app::{DeviceId, DeviceInfo};
use egui::RichText;

use super::kit::{Tokens, Tone};
use crate::model::{Event, Overview, Rename};

/// This app's version: the agent and the app are built from one version, so a computer reporting an older one is behind.
const OWN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// What a cell shows for a detail the computer didn't send (an older agent leaves them out).
const NO_DETAIL: &str = "—";

/// The width the table keeps; a narrower window scrolls it sideways inside its own frame.
const TABLE_MIN_WIDTH: f32 = 820.0;

/// The width from which the title and the buttons share a row.
const HEADER_ONE_ROW: f32 = 720.0;

/// The least height of a row: two lines of a name, and the buttons.
const ROW_HEIGHT: f32 = 44.0;

/// The width of the rename field.
const RENAME_FIELD_WIDTH: f32 = 220.0;

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
    ui.add_space(12.0);
    notes(ui);
}

fn table(ui: &mut egui::Ui, o: &mut Overview, active: &[DeviceInfo], now_ms: i64, events: &mut Vec<Event>) {
    let t = Tokens::get(ui.ctx());
    let (online, removing) = (o.status.online, o.removing);
    let rename_open = o.renaming.is_some();
    let mut renaming = o.renaming.take();
    egui::Frame::new().stroke(t.hairline()).fill(t.bg).show(ui, |ui| {
        ui.set_width(ui.available_width());
        // A solid bar, drawn whenever the table is wider than the window (the floating one shows on hover only, so
        // nothing told that the last columns were out of view).
        ui.spacing_mut().scroll = egui::style::ScrollStyle { foreground_color: true, ..egui::style::ScrollStyle::solid() };
        egui::ScrollArea::horizontal().id_salt("computers-scroll").auto_shrink([false, true]).show(ui, |ui| {
            ui.set_min_width(TABLE_MIN_WIDTH);
            egui::Frame::new().inner_margin(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
                egui::Grid::new("computers").num_columns(6).striped(true).min_row_height(ROW_HEIGHT).spacing([20.0, 6.0]).show(ui, |ui| {
                    for head in ["Computer", "Kind", "Platform", "Version", "Last seen", ""] {
                        super::kit::section_label(ui, head);
                    }
                    ui.end_row();
                    for device in active {
                        let editing = renaming.as_mut().filter(|r| r.id == device.id);
                        row(ui, device, editing, (online, removing, rename_open), now_ms, events);
                        ui.end_row();
                    }
                });
            });
        });
    });
    o.renaming = renaming;
}

/// One computer's row. `editing` is its rename, when its name is being changed.
fn row(
    ui: &mut egui::Ui,
    device: &DeviceInfo,
    mut editing: Option<&mut Rename>,
    (online, removing, rename_open): (bool, Option<DeviceId>, bool),
    now_ms: i64,
    events: &mut Vec<Event>,
) {
    let t = Tokens::get(ui.ctx());
    let focused = egui::Id::new(("rename-focused", device.id));

    // Computer
    match editing.as_deref_mut() {
        Some(rename) => {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.set_max_width(RENAME_FIELD_WIDTH);
                    super::kit::section_label(ui, "Rename this computer");
                    let field =
                        super::kit::text_edit(&mut rename.name).id(egui::Id::new(("rename-field", device.id))).desired_width(RENAME_FIELD_WIDTH);
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
                    if let Some(error) = &rename.error {
                        super::error_line(ui, error);
                    }
                });
                if device.current {
                    // In a column of its own the tag keeps its height beside a taller field.
                    ui.vertical(|ui| super::kit::tag(ui, "This computer", Tone::Accent));
                }
            });
        }
        None => {
            ui.data_mut(|d| d.remove::<bool>(focused));
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new(&device.name).strong());
                    if device.created_ms > 0 {
                        ui.label(RichText::new(format!("signed in {}", super::trash::date(device.created_ms))).small().color(t.muted));
                    }
                });
                if device.current {
                    // In a column of its own the tag keeps its height beside a taller field.
                    ui.vertical(|ui| super::kit::tag(ui, "This computer", Tone::Accent));
                }
            });
        }
    }

    // Kind, Platform, Version
    detail(ui, &capitalised(&device.client_kind), false);
    detail(ui, &device.platform, false);
    ui.horizontal(|ui| {
        if crate::model::is_older_version(&device.app_version, OWN_VERSION) {
            ui.label(RichText::new(&device.app_version).monospace().color(t.warn));
            super::kit::tag(ui, "Older", Tone::Warn).on_hover_text(format!("This app is {OWN_VERSION}"));
        } else {
            detail(ui, &device.app_version, true);
        }
    });

    // Last seen
    if device.current {
        ui.horizontal(|ui| {
            if online {
                super::kit::tag(ui, "Online", Tone::Good);
            } else {
                super::kit::tag(ui, "Offline", Tone::Neutral);
            }
        });
    } else {
        ui.label(super::ago(now_ms, device.last_used_ms));
    }

    // Actions
    ui.horizontal(|ui| match editing {
        Some(rename) => {
            if ui.add_enabled(!rename.busy, super::kit::Button::new("Cancel")).clicked() {
                events.push(Event::CancelRename);
            }
            let save = super::kit::Button::new(if rename.busy { "Saving…" } else { "Save" }).primary();
            if ui.add_enabled(!rename.busy, save).clicked() {
                events.push(Event::SaveRename);
            }
        }
        None => {
            if ui.add_enabled(!rename_open, super::kit::Button::new("Rename")).on_hover_text(format!("Give {} another name", device.name)).clicked() {
                events.push(Event::StartRename(device.id));
            }
            if !device.current {
                let this = removing == Some(device.id);
                let button = super::kit::Button::new(if this { "Removing…" } else { "Remove" }).danger();
                let response = ui.add_enabled(removing.is_none(), button).on_hover_text("It can't sync any more; its files stay there");
                if response.clicked() {
                    events.push(Event::RemoveDevice(device.id));
                }
            }
        }
    });
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
        ui.label(RichText::new(text).color(t.muted));
    }
}

/// `text` with a capital first letter (`agent` is `Agent`).
fn capitalised(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|first| first.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// What Add a computer and Remove do, in two notes.
fn notes(ui: &mut egui::Ui) {
    let add = |ui: &mut egui::Ui| {
        super::kit::card(ui, Some("Add a computer"), |ui| {
            ui.add(egui::Label::new("Make a one-time code here and type it on the other computer. It is valid for 10 minutes.").wrap());
        });
    };
    let remove = |ui: &mut egui::Ui| {
        super::kit::card(ui, Some("Removing a computer"), |ui| {
            ui.add(egui::Label::new("It is signed out at once. Its files stay on its disk; nothing is deleted there.").wrap());
        });
    };
    if ui.available_width() >= NOTES_SIDE_BY_SIDE {
        ui.columns(2, |columns| {
            let [left, right] = columns else { return };
            add(left);
            remove(right);
        });
    } else {
        add(ui);
        ui.add_space(8.0);
        remove(ui);
    }
}
