//! "Join your Craft Cloud", the first start: a rail of the three steps (server, account, folder and name), then the account
//! step as a sign-in (server, user name, password) or a join string, and the folder and this computer's name; and an invite's
//! new password.

use egui::RichText;

use super::kit::{self, Tone};
use crate::model::{Event, JoinForm, JoinMode, PasswordForm};

/// The id name of the sign-in's password field (see `security::forget_secrets`).
pub(super) const PASSWORD_FIELD: &str = "join-password";
/// The id names of an invite's two password fields.
pub(super) const INVITE_PASSWORD_FIELD: &str = "join-invite-password";
pub(super) const INVITE_AGAIN_FIELD: &str = "join-invite-again";

/// The join form; while `joining`, shown as sent (nothing can be changed or sent again).
pub(super) fn form(ui: &mut egui::Ui, form: &mut JoinForm, joining: bool, events: &mut Vec<Event>) {
    super::heading(ui, "Join your Craft Cloud");
    rail(ui, form);
    ui.add_space(8.0);
    let signing_in = form.mode == JoinMode::SignIn;
    let mut enter = false;
    ui.add_enabled_ui(!joining, |ui| {
        ui.label(RichText::new("Sign in with the account your admin made, or use a join string.").small().weak());
        ui.add_space(4.0);
        mode_switch(ui, form.mode, events);
        ui.add_space(6.0);

        if signing_in {
            let label = kit::section_label(ui, "Server");
            let field = egui::TextEdit::singleline(&mut form.server).hint_text("cloud.example.com").desired_width(f32::INFINITY);
            enter |= submitted(ui.add(field).labelled_by(label.id));
            ui.add_space(4.0);

            let label = kit::section_label(ui, "Username");
            enter |= submitted(ui.add(egui::TextEdit::singleline(&mut form.user).desired_width(f32::INFINITY)).labelled_by(label.id));
            ui.add_space(4.0);

            let label = kit::section_label(ui, "Password");
            let field = egui::TextEdit::singleline(&mut form.password)
                .id(super::security::secret_id(PASSWORD_FIELD))
                .password(true)
                .desired_width(f32::INFINITY);
            enter |= submitted(ui.add(field).labelled_by(label.id));
            ui.label(RichText::new("If your account has two-factor sign-in, the next step asks for the code.").small().weak());
        } else {
            let label = kit::section_label(ui, "Join string");
            let field = egui::TextEdit::singleline(&mut form.join).hint_text("cloud.example.com/K7M4-Q9TX").desired_width(f32::INFINITY);
            enter |= submitted(ui.add(field).labelled_by(label.id));
            ui.label(RichText::new("An invite, or a code from a computer that is signed in. Pasting the link works too.").small().weak());
        }
        ui.add_space(6.0);

        kit::section_label(ui, "Folder");
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 90.0).max(120.0);
            let before = form.folder.clone();
            ui.add(egui::TextEdit::singleline(&mut form.folder).desired_width(width));
            if form.folder != before {
                form.ask_adopt = false;
            }
            if ui.add(kit::Button::new("Change…")).clicked() {
                events.push(Event::ChangeFolder);
            }
        });
        ui.add_space(6.0);

        kit::section_label(ui, "This computer's name");
        ui.add(egui::TextEdit::singleline(&mut form.device_name).hint_text("this computer's name").desired_width(f32::INFINITY));
        ui.add_space(8.0);
    });

    if let Some(error) = &form.error {
        super::error_line(ui, error);
        ui.add_space(8.0);
    }
    if form.ask_adopt && !joining {
        kit::card(ui, None, |ui| {
            ui.label(RichText::new("This folder already has files in it.").strong());
            ui.label("Sync them too? Files that differ from your cloud's are kept as copies; nothing is deleted.");
            ui.horizontal(|ui| {
                if ui.add(kit::Button::new("Sync them")).clicked() {
                    events.push(Event::SyncExistingFiles(true));
                }
                if ui.add(kit::Button::new("Choose another folder")).clicked() {
                    events.push(Event::SyncExistingFiles(false));
                }
            });
        });
        return;
    }
    if joining {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(if signing_in { "Signing in…" } else { "Joining…" });
        });
    } else if super::wide_button(ui, true, if signing_in { "Sign in" } else { "Join" }) || enter {
        events.push(Event::SubmitJoin);
    }
}

/// Whether Enter was pressed in the field that just lost focus.
fn submitted(response: egui::Response) -> bool {
    response.lost_focus() && response.ctx.input(|i| i.key_pressed(egui::Key::Enter))
}

/// The three steps with what is known of each; the first one not known yet is the one being asked for.
fn rail(ui: &mut egui::Ui, form: &JoinForm) {
    let steps = form.rail();
    let current = steps.iter().position(|s| !s.done);
    kit::card(ui, None, |ui| {
        for (n, step) in steps.iter().enumerate() {
            ui.horizontal(|ui| {
                let tone = if step.done {
                    Tone::Good
                } else if Some(n) == current {
                    Tone::Accent
                } else {
                    Tone::Neutral
                };
                kit::tag(ui, &format!("{:02}", n + 1), tone);
                ui.label(RichText::new(step.title).strong());
                ui.add(egui::Label::new(RichText::new(&step.detail).small().weak()).truncate());
            });
        }
    });
}

/// "Sign in" / "Join string": the chosen one drawn inverted.
fn mode_switch(ui: &mut egui::Ui, mode: JoinMode, events: &mut Vec<Event>) {
    ui.horizontal(|ui| {
        let size = egui::vec2((ui.available_width() - ui.spacing().item_spacing.x) / 2.0, kit::CONTROL_H);
        for (shown, label) in [(JoinMode::SignIn, "Sign in"), (JoinMode::JoinString, "Join string")] {
            let button = kit::Button::new(label).min_size(size).selected(mode == shown);
            if ui.add_sized(size, button).clicked() {
                events.push(Event::SetJoinMode(shown));
            }
        }
    });
}

/// An invite to a new account: its password, twice.
pub(super) fn password(ui: &mut egui::Ui, pw: &mut PasswordForm, events: &mut Vec<Event>) {
    super::heading(ui, "Choose a password");
    ui.label("This invite is for a new account. Choose its password: you need it to sign in on the server.");
    ui.add_space(10.0);
    ui.label("Password");
    let first = egui::TextEdit::singleline(&mut pw.password).id(super::security::secret_id(INVITE_PASSWORD_FIELD));
    ui.add(first.password(true).desired_width(f32::INFINITY));
    ui.add_space(6.0);
    ui.label("The same password again");
    let second = egui::TextEdit::singleline(&mut pw.again).id(super::security::secret_id(INVITE_AGAIN_FIELD));
    let again = ui.add(second.password(true).desired_width(f32::INFINITY));
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
    if ui.add(kit::Button::new("Back")).clicked() {
        events.push(Event::CancelPassword);
    }
}
