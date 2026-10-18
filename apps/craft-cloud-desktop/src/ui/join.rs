//! "Join your Craft Cloud": the join string, the folder and this computer's name; and an invite's new password.

use egui::RichText;

use crate::model::{Event, JoinForm, PasswordForm};

/// The join form; while `joining`, shown as sent (nothing can be changed or sent again).
pub(super) fn form(ui: &mut egui::Ui, form: &mut JoinForm, joining: bool, events: &mut Vec<Event>) {
    super::heading(ui, "Join your Craft Cloud");
    let mut enter = false;
    ui.add_enabled_ui(!joining, |ui| {
        ui.label("Join string");
        let field = egui::TextEdit::singleline(&mut form.join).hint_text("cloud.example.com/K7M4-Q9TX").desired_width(f32::INFINITY);
        let response = ui.add(field);
        enter |= response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        ui.label(RichText::new("An invite, or a code from a computer that is signed in. Pasting the link works too.").small().weak());
        ui.add_space(10.0);

        ui.label("Folder");
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 90.0).max(120.0);
            let before = form.folder.clone();
            ui.add(egui::TextEdit::singleline(&mut form.folder).desired_width(width));
            if form.folder != before {
                form.ask_adopt = false;
            }
            if ui.button("Change…").clicked() {
                events.push(Event::ChangeFolder);
            }
        });
        ui.add_space(10.0);

        ui.label("This computer's name");
        ui.add(egui::TextEdit::singleline(&mut form.device_name).hint_text("this computer's name").desired_width(f32::INFINITY));
        ui.add_space(12.0);
    });

    if let Some(error) = &form.error {
        super::error_line(ui, error);
        ui.add_space(8.0);
    }
    if form.ask_adopt && !joining {
        ui.group(|ui| {
            ui.label(RichText::new("This folder already has files in it.").strong());
            ui.label("Sync them too? Files that differ from your cloud's are kept as copies; nothing is deleted.");
            ui.horizontal(|ui| {
                if ui.button("Sync them").clicked() {
                    events.push(Event::SyncExistingFiles(true));
                }
                if ui.button("Choose another folder").clicked() {
                    events.push(Event::SyncExistingFiles(false));
                }
            });
        });
        return;
    }
    if joining {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Joining…");
        });
    } else if super::wide_button(ui, true, "Join") || enter {
        events.push(Event::SubmitJoin);
    }
}

/// An invite to a new account: its password, twice.
pub(super) fn password(ui: &mut egui::Ui, pw: &mut PasswordForm, events: &mut Vec<Event>) {
    super::heading(ui, "Choose a password");
    ui.label("This invite is for a new account. Choose its password: you need it to sign in on the server.");
    ui.add_space(10.0);
    ui.label("Password");
    ui.add(egui::TextEdit::singleline(&mut pw.password).password(true).desired_width(f32::INFINITY));
    ui.add_space(6.0);
    ui.label("The same password again");
    let again = ui.add(egui::TextEdit::singleline(&mut pw.again).password(true).desired_width(f32::INFINITY));
    let enter = again.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    ui.add_space(12.0);
    if let Some(error) = &pw.error {
        super::error_line(ui, error);
        ui.add_space(8.0);
    }
    if super::wide_button(ui, true, "Join") || enter {
        events.push(Event::SubmitPassword);
    }
    ui.add_space(4.0);
    if ui.button("Back").clicked() {
        events.push(Event::CancelPassword);
    }
}
