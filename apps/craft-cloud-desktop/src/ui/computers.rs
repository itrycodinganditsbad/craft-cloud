//! The Computers tab: the account's computers in full, when each was last seen, Add a computer, and Remove (this computer
//! has none: that is Sign out).

use egui::RichText;

use super::kit::{Tokens, Tone};
use crate::model::{Event, Overview};

pub(super) fn page(ui: &mut egui::Ui, o: &Overview, now_ms: i64, events: &mut Vec<Event>) {
    let t = Tokens::get(ui.ctx());
    let active: Vec<_> = o.devices.iter().flatten().filter(|d| !d.revoked).collect();
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            super::kit::section_label(ui, &super::kit::numbered(1, "Computers on this account"));
            if o.devices.is_some() {
                super::heading(ui, &if active.len() == 1 { "1 computer".to_owned() } else { format!("{} computers", active.len()) });
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.add(super::kit::Button::new("Add a computer").primary()).clicked() {
                events.push(Event::AddComputer);
            }
            if ui.add(super::kit::Button::new("Refresh")).clicked() {
                events.push(Event::RefreshDevices);
            }
        });
    });
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
    egui::Frame::new().stroke(t.hairline()).inner_margin(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        egui::Grid::new("computers").num_columns(6).striped(true).spacing([20.0, 10.0]).show(ui, |ui| {
            for head in ["Computer", "Kind", "Platform", "Version", "Last seen", ""] {
                super::kit::section_label(ui, head);
            }
            ui.end_row();
            for device in &active {
                if device.current {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&device.name).strong());
                        super::kit::tag(ui, "This computer", Tone::Accent);
                    });
                } else {
                    ui.label(RichText::new(&device.name).strong());
                }
                ui.label(RichText::new(&device.client_kind).monospace().color(t.muted));
                ui.label(RichText::new(&device.platform).color(t.muted));
                ui.label(RichText::new(&device.app_version).monospace().color(t.muted));
                if device.current {
                    if o.status.online {
                        super::kit::tag(ui, "Online", Tone::Good);
                    } else {
                        super::kit::tag(ui, "Offline", Tone::Neutral);
                    }
                    ui.label("");
                } else {
                    ui.label(super::ago(now_ms, device.last_used_ms));
                    let removing = o.removing == Some(device.id);
                    let button = super::kit::Button::new(if removing { "Removing…" } else { "Remove" }).danger();
                    let response = ui.add_enabled(o.removing.is_none(), button).on_hover_text("It can't sync any more; its files stay there");
                    if response.clicked() {
                        events.push(Event::RemoveDevice(device.id));
                    }
                }
                ui.end_row();
            }
        });
    });
    ui.add_space(12.0);
    ui.add(
        egui::Label::new(
            RichText::new(
                "Add a computer makes a one-time code to type on the other computer. A removed computer is signed out at once; its files stay on its disk.",
            )
            .small()
            .color(t.muted),
        )
        .wrap(),
    );
}
