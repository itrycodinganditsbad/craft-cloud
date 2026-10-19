//! Add a computer: the join string to type on the other computer, a QR code of its link, the 10 minutes it is good
//! for, and "waiting…" until it says who joined.

use egui::{Color32, RichText};
use qrcodegen::{QrCode, QrCodeEcc};

use crate::model::{AddState, Adding, Event};

pub(super) fn show(ui: &mut egui::Ui, adding: &Adding, now_ms: i64, events: &mut Vec<Event>) {
    super::heading(ui, "Add a computer");
    match (&adding.state, &adding.info) {
        (AddState::Asking, _) | (_, None) => {
            if let AddState::Failed(why) = &adding.state {
                super::error_line(ui, why);
            } else {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Making a code…");
                });
            }
        }
        (state, Some(info)) => {
            ui.label("On the other computer, open Craft Cloud and enter:");
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add(egui::Label::new(RichText::new(&info.join).monospace().size(18.0).strong()).selectable(true));
                if ui.small_button("Copy").clicked() {
                    ui.ctx().copy_text(info.join.clone());
                }
            });
            ui.add_space(8.0);
            ui.label(RichText::new("Or scan this with the other computer's camera:").weak());
            qr(ui, &info.link, 180.0);
            ui.add_space(8.0);
            match state {
                AddState::Waiting => {
                    let left = info.expires_ms.saturating_sub(now_ms).max(0) / 1000;
                    ui.label(format!("Works once, for {}:{:02} more", left / 60, left % 60));
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Waiting…");
                    });
                }
                AddState::Joined(name) => {
                    ui.label(RichText::new(format!("{name} joined")).strong().color(super::good(ui)));
                }
                AddState::Expired => super::error_line(ui, "This code expired: make a new one."),
                AddState::Asking | AddState::Failed(_) => {}
            }
        }
    }
    ui.add_space(14.0);
    let done = matches!(adding.state, AddState::Joined(_));
    if super::wide_button(ui, true, if done { "Done" } else { "Cancel" }) {
        events.push(Event::CloseAdding);
    }
    if matches!(adding.state, AddState::Expired | AddState::Failed(_)) {
        ui.add_space(4.0);
        if ui.button("Make a new code").clicked() {
            events.push(Event::CloseAdding);
            events.push(Event::AddComputer);
        }
    }
}

/// `text` as a QR code `side` points wide: dark modules on white with the standard quiet zone, whatever the theme
/// (cameras read it that way round).
fn qr(ui: &mut egui::Ui, text: &str, side: f32) {
    let Ok(code) = QrCode::encode_text(text, QrCodeEcc::Medium) else {
        return;
    };
    let size = code.size();
    let quiet = 4;
    let cells = size + 2 * quiet;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, Color32::WHITE);
    // A QR code is at most 177 modules wide: exact as a float.
    let cell = side / cells as f32;
    for y in 0..size {
        for x in 0..size {
            if code.get_module(x, y) {
                let min = rect.min + egui::vec2((x + quiet) as f32 * cell, (y + quiet) as f32 * cell);
                painter.rect_filled(egui::Rect::from_min_size(min, egui::vec2(cell, cell)), 0.0, Color32::BLACK);
            }
        }
    }
}
