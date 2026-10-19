//! The account's computers: when each was last seen, and Remove (this computer has none: that is Sign out).

use egui::RichText;

use crate::model::{Event, Overview};

pub(super) fn list(ui: &mut egui::Ui, o: &Overview, now_ms: i64, events: &mut Vec<Event>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Computers").strong());
        if ui.small_button("Refresh").clicked() {
            events.push(Event::RefreshDevices);
        }
    });
    if let Some(error) = &o.devices_error {
        super::error_line(ui, error);
    }
    let Some(devices) = &o.devices else {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Asking the server…");
        });
        return;
    };
    egui::Grid::new("computers").num_columns(3).striped(true).spacing([12.0, 6.0]).show(ui, |ui| {
        for device in devices.iter().filter(|d| !d.revoked) {
            if device.current {
                ui.label(RichText::new(format!("{} (this computer)", device.name)).strong());
            } else {
                ui.label(&device.name);
            }
            ui.label(RichText::new(format!("last seen {}", super::ago(now_ms, device.last_used_ms))).weak());
            if device.current {
                ui.label("");
            } else {
                let removing = o.removing == Some(device.id);
                let button = egui::Button::new(if removing { "Removing…" } else { "Remove" });
                let response = ui.add_enabled(o.removing.is_none(), button).on_hover_text("It can't sync any more; its files stay there");
                if response.clicked() {
                    events.push(Event::RemoveDevice(device.id));
                }
            }
            ui.end_row();
        }
    });
}
