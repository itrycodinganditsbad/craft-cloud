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

/// The widest the first-start forms get, so they stay a form in a wide window.
const COLUMN_MAX: f32 = 520.0;
/// From this width on the step rail sits left of the form (as in the design), below it the rail is one line above it.
const RAIL_BESIDE_FROM: f32 = 900.0;
const RAIL_WIDTH: f32 = 260.0;
const RAIL_GAP: f32 = 40.0;

/// `add` in a column of at most `width`, centred in the space there is.
pub(super) fn centred(ui: &mut egui::Ui, width: f32, add: impl FnOnce(&mut egui::Ui)) {
    let width = width.min(ui.available_width());
    let side = ((ui.available_width() - width) / 2.0).max(0.0);
    let spacing = ui.spacing().item_spacing.x;
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.add_space(side);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.x = spacing;
            ui.set_width(width);
            add(ui);
        });
    });
}

/// [`COLUMN_MAX`] wide, for the other first-start screens (the code, an invite's password).
pub(super) fn column(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    centred(ui, COLUMN_MAX, add);
}

/// The first start's main button, as wide as its column.
pub(super) fn main_button(ui: &mut egui::Ui, text: &str) -> bool {
    let size = egui::vec2(ui.available_width(), kit::CONTROL_H);
    ui.add_sized(size, kit::Button::new(text).primary().min_size(size)).clicked()
}

/// The join form; while `joining`, shown as sent (nothing can be changed or sent again). In a wide window the rail of steps
/// sits left of the form, otherwise one line above it.
pub(super) fn form(ui: &mut egui::Ui, form: &mut JoinForm, joining: bool, events: &mut Vec<Event>) {
    if ui.available_width() >= RAIL_BESIDE_FROM {
        centred(ui, RAIL_WIDTH + RAIL_GAP + COLUMN_MAX, |ui| {
            let spacing = ui.spacing().item_spacing.x;
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.x = spacing;
                    ui.set_width(RAIL_WIDTH);
                    rail(ui, form);
                });
                ui.add_space(RAIL_GAP);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.x = spacing;
                    ui.set_width(COLUMN_MAX);
                    body(ui, form, joining, events, false);
                });
            });
        });
    } else {
        column(ui, |ui| body(ui, form, joining, events, true));
    }
}

fn body(ui: &mut egui::Ui, form: &mut JoinForm, joining: bool, events: &mut Vec<Event>, rail_above: bool) {
    super::heading(ui, "Join your Craft Cloud");
    if rail_above {
        rail_line(ui, form);
        ui.add_space(8.0);
    }
    let signing_in = form.mode == JoinMode::SignIn;
    let mut enter = false;
    ui.add_enabled_ui(!joining, |ui| {
        ui.label(RichText::new("Sign in with the account your admin made, or use a join string.").small().weak());
        ui.add_space(4.0);
        mode_switch(ui, form.mode, events);
        ui.add_space(6.0);

        if signing_in {
            let label = kit::section_label(ui, "Server");
            let field = kit::text_edit(&mut form.server).hint_text("cloud.example.com").desired_width(f32::INFINITY);
            enter |= submitted(ui.add(field).labelled_by(label.id));
            ui.add_space(4.0);

            let label = kit::section_label(ui, "Username");
            enter |= submitted(ui.add(kit::text_edit(&mut form.user).desired_width(f32::INFINITY)).labelled_by(label.id));
            ui.add_space(4.0);

            let label = kit::section_label(ui, "Password");
            let field = kit::text_edit(&mut form.password).id(super::security::secret_id(PASSWORD_FIELD)).password(true).desired_width(f32::INFINITY);
            enter |= submitted(ui.add(field).labelled_by(label.id));
            ui.label(RichText::new("If your account has two-factor sign-in, the next step asks for the code.").small().weak());
        } else {
            let label = kit::section_label(ui, "Join string");
            let field = kit::text_edit(&mut form.join).hint_text("cloud.example.com/K7M4-Q9TX").desired_width(f32::INFINITY);
            enter |= submitted(ui.add(field).labelled_by(label.id));
            ui.label(RichText::new("An invite, or a code from a computer that is signed in. Pasting the link works too.").small().weak());
        }
        ui.add_space(6.0);

        kit::section_label(ui, "Folder");
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 90.0).max(120.0);
            let before = form.folder.clone();
            ui.add(kit::text_edit(&mut form.folder).desired_width(width));
            if form.folder != before {
                form.ask_adopt = false;
            }
            if ui.add(kit::Button::new("Change…")).clicked() {
                events.push(Event::ChangeFolder);
            }
        });
        ui.add_space(6.0);

        kit::section_label(ui, "This computer's name");
        ui.add(kit::text_edit(&mut form.device_name).hint_text("this computer's name").desired_width(f32::INFINITY));
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
    } else if main_button(ui, if signing_in { "Sign in" } else { "Join" }) || enter {
        events.push(Event::SubmitJoin);
    }
}

/// Whether Enter was pressed in the field that just lost focus.
fn submitted(response: egui::Response) -> bool {
    response.lost_focus() && response.ctx.input(|i| i.key_pressed(egui::Key::Enter))
}

/// How a step is marked: known, the one being asked for, or still to come.
fn step_tone(n: usize, done: bool, current: Option<usize>) -> Tone {
    if done {
        Tone::Good
    } else if Some(n) == current {
        Tone::Accent
    } else {
        Tone::Neutral
    }
}

/// The three steps with what is known of each; the first one not known yet is the one being asked for.
fn rail(ui: &mut egui::Ui, form: &JoinForm) {
    let steps = form.rail();
    let current = steps.iter().position(|s| !s.done);
    kit::card(ui, None, |ui| {
        for (n, step) in steps.iter().enumerate() {
            ui.horizontal(|ui| {
                kit::tag(ui, &format!("{:02}", n + 1), step_tone(n, step.done, current));
                ui.label(RichText::new(step.title).strong());
            });
            ui.add(egui::Label::new(RichText::new(&step.detail).small().weak()).truncate());
            if n + 1 < steps.len() {
                ui.add_space(4.0);
            }
        }
    });
}

/// The steps in one line, for a window too narrow for the rail beside the form.
fn rail_line(ui: &mut egui::Ui, form: &JoinForm) {
    let steps = form.rail();
    let current = steps.iter().position(|s| !s.done);
    ui.horizontal_wrapped(|ui| {
        for (n, step) in steps.iter().enumerate() {
            kit::tag(ui, &format!("{:02}", n + 1), step_tone(n, step.done, current));
            ui.label(RichText::new(step.title).strong());
            ui.add_space(8.0);
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
    let first = kit::text_edit(&mut pw.password).id(super::security::secret_id(INVITE_PASSWORD_FIELD));
    ui.add(first.password(true).desired_width(f32::INFINITY));
    ui.add_space(6.0);
    ui.label("The same password again");
    let second = kit::text_edit(&mut pw.again).id(super::security::secret_id(INVITE_AGAIN_FIELD));
    let again = ui.add(second.password(true).desired_width(f32::INFINITY));
    let enter = again.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    ui.add_space(12.0);
    if let Some(error) = &pw.error {
        super::error_line(ui, error);
        ui.add_space(8.0);
    }
    if main_button(ui, "Join") || enter {
        events.push(Event::SubmitPassword);
    }
    ui.add_space(4.0);
    if ui.add(kit::Button::new("Back")).clicked() {
        events.push(Event::CancelPassword);
    }
}
