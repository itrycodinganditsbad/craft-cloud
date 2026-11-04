//! Security: two-factor sign-in (turn it on with a secret and a QR code, the recovery codes shown once, turn it off, new
//! recovery codes), the password, and signing the other computers out.

use egui::RichText;

use crate::model::Event;
use crate::security::{CodesWhy, SecurityEvent, SecurityPage, Step, TwoFactorInfo};

fn send(events: &mut Vec<Event>, event: SecurityEvent) {
    events.push(Event::Security(event));
}

/// The id of a secret field of the page (a password or a code), by name.
pub(super) fn secret_id(name: &str) -> egui::Id {
    egui::Id::new(("security-secret", name))
}

/// Every secret field of the page and the second step of a join: its name, and the text the model holds for it in `screen`
/// (empty when the field isn't there).
fn secrets(screen: &crate::model::Screen) -> Vec<(&'static str, &str)> {
    use crate::model::Screen;
    match screen {
        Screen::Security(page) => match &page.step {
            Step::SetupPassword { password } => vec![("setup-password", password)],
            Step::SetupCode { code, .. } => vec![("setup-code", code)],
            Step::Disable { password, code, .. } => vec![("disable-password", password), ("disable-code", code)],
            Step::Regenerate { password, code } => vec![("regenerate-password", password), ("regenerate-code", code)],
            Step::Password { current, new, again } => vec![("password-current", current), ("password-new", new), ("password-again", again)],
            Step::Home | Step::Codes { .. } | Step::SignOutOthers => Vec::new(),
        },
        Screen::SecondFactor(form) => vec![("second-factor-code", &form.code)],
        // The admin's confirmation: only while it is open to be typed in.
        Screen::Admin(a) if a.code.is_none() => match &a.dialog {
            Some(d) if !d.busy => vec![(super::admin::PASSWORD_FIELD, &d.confirmation.password)],
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

const SECRET_NAMES: [&str; 11] = [
    "setup-password",
    "setup-code",
    "disable-password",
    "disable-code",
    "regenerate-password",
    "regenerate-code",
    "password-current",
    "password-new",
    "password-again",
    "second-factor-code",
    super::admin::PASSWORD_FIELD,
];

/// Drops what egui remembers of every secret field (its cursor and, above all, its undo history, which holds the text as it
/// was typed) unless the model holds something typed in that field right now. So once a password or code was sent, a step was
/// cancelled or the page left, Undo can't bring it back; called every frame, before the fields are drawn. egui keeps this state
/// in memory only (this app doesn't enable eframe's persistence, and the undo history is not serialized either).
pub(super) fn forget_secrets(ctx: &egui::Context, screen: &crate::model::Screen) {
    let typed = secrets(screen);
    for name in SECRET_NAMES {
        if !typed.iter().any(|(n, text)| *n == name && !text.is_empty()) {
            ctx.data_mut(|d| d.remove::<egui::text_edit::TextEditState>(secret_id(name)));
        }
    }
}

/// A password field.
fn password_field(ui: &mut egui::Ui, name: &str, label: &str, text: &mut String) -> egui::Response {
    ui.label(label);
    let response = ui.add(egui::TextEdit::singleline(text).id(secret_id(name)).password(true).desired_width(f32::INFINITY));
    ui.add_space(6.0);
    response
}

pub(super) fn show(ui: &mut egui::Ui, page: &mut SecurityPage, events: &mut Vec<Event>) {
    super::heading(ui, "Security");
    let (busy, error, note) = (page.busy, page.error.clone(), page.note.clone());
    let info = page.info.clone();
    match &mut page.step {
        Step::Home => home(ui, &info, busy, note.as_deref(), error.as_deref(), events),
        Step::SetupPassword { password } => {
            ui.label(
                "Two-factor sign-in asks for a code from an app on your phone (Aegis, 1Password, Google Authenticator…) as well as your password.",
            );
            ui.add_space(8.0);
            let response = password_field(ui, "setup-password", "Your password", password);
            let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            footer(ui, busy, error.as_deref(), "Continue", enter, events);
        }
        Step::SetupCode { secret, uri, expires_ms: _, code } => {
            ui.label("1. Scan this with your authenticator app, or type the secret into it.");
            ui.add_space(6.0);
            super::add_computer::qr(ui, uri, 180.0);
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add(egui::Label::new(RichText::new(secret.as_str()).monospace().size(16.0).strong()).selectable(true));
                if ui.small_button("Copy").clicked() {
                    ui.ctx().copy_text(secret.clone());
                }
            });
            ui.collapsing("The setup link", |ui| {
                ui.add(egui::Label::new(RichText::new(uri.as_str()).monospace().small()).selectable(true).wrap());
                if ui.small_button("Copy link").clicked() {
                    ui.ctx().copy_text(uri.clone());
                }
            });
            ui.add_space(10.0);
            ui.label("2. Type the six digits the app shows now.");
            let response = ui.add(egui::TextEdit::singleline(code).id(secret_id("setup-code")).hint_text("123456").desired_width(160.0));
            let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.add_space(6.0);
            footer(ui, busy, error.as_deref(), "Turn on", enter, events);
        }
        Step::Codes { codes, why } => {
            let (line, button) = match why {
                CodesWhy::TurnedOn => ("Two-factor sign-in is on.", "I have saved them"),
                CodesWhy::New => ("These are your new recovery codes.", "I have saved them"),
            };
            ui.label(RichText::new(line).strong().color(super::good(ui)));
            ui.add_space(6.0);
            ui.label("Save these recovery codes somewhere safe now. Each works once, if you lose your phone, and they are shown only this once.");
            ui.add_space(8.0);
            egui::Grid::new("recovery-codes").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
                for (n, code) in codes.iter().enumerate() {
                    ui.add(egui::Label::new(RichText::new(code.as_str()).monospace().size(15.0)).selectable(true));
                    if n % 2 == 1 {
                        ui.end_row();
                    }
                }
            });
            ui.add_space(8.0);
            if ui.button("Copy all").clicked() {
                ui.ctx().copy_text(codes.join("\n"));
            }
            ui.add_space(10.0);
            if super::wide_button(ui, true, button) {
                send(events, SecurityEvent::Home);
            }
        }
        Step::Disable { password, code, recovery } => {
            ui.label("Turning it off signs your other computers out. Type your password and a code from the app.");
            ui.add_space(8.0);
            password_field(ui, "disable-password", "Your password", password);
            ui.label(if *recovery { "A recovery code" } else { "The six digits from the app" });
            let response = ui.add(egui::TextEdit::singleline(code).id(secret_id("disable-code")).desired_width(f32::INFINITY));
            let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.small_button(if *recovery { "Use a code from the app" } else { "Use a recovery code instead" }).clicked() {
                send(events, SecurityEvent::ToggleRecovery);
            }
            ui.add_space(6.0);
            footer(ui, busy, error.as_deref(), "Turn off", enter, events);
        }
        Step::Regenerate { password, code } => {
            ui.label("New recovery codes replace the old ones at once. Type your password and a code from the app.");
            ui.add_space(8.0);
            password_field(ui, "regenerate-password", "Your password", password);
            ui.label("The six digits from the app");
            let response = ui.add(egui::TextEdit::singleline(code).id(secret_id("regenerate-code")).desired_width(160.0));
            let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.add_space(6.0);
            footer(ui, busy, error.as_deref(), "Make new codes", enter, events);
        }
        Step::Password { current, new, again } => {
            ui.label("Changing your password signs your other computers out.");
            ui.add_space(8.0);
            password_field(ui, "password-current", "Current password", current);
            password_field(ui, "password-new", "New password", new);
            let response = password_field(ui, "password-again", "The new password again", again);
            let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            footer(ui, busy, error.as_deref(), "Change password", enter, events);
        }
        Step::SignOutOthers => {
            ui.label("Every other computer of your account is signed out. Their files stay there; they join again to sync.");
            ui.add_space(8.0);
            footer(ui, busy, error.as_deref(), "Sign them out", false, events);
        }
    }
}

/// The step's error, its main button and the way back.
fn footer(ui: &mut egui::Ui, busy: bool, error: Option<&str>, button: &str, enter: bool, events: &mut Vec<Event>) {
    if let Some(error) = error {
        super::error_line(ui, error);
        ui.add_space(6.0);
    }
    if busy {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Working…");
        });
    } else if super::wide_button(ui, true, button) || enter {
        send(events, SecurityEvent::Submit);
    }
    ui.add_space(4.0);
    if ui.add_enabled(!busy, egui::Button::new("Cancel")).clicked() {
        send(events, SecurityEvent::Home);
    }
}

fn home(ui: &mut egui::Ui, info: &TwoFactorInfo, busy: bool, note: Option<&str>, error: Option<&str>, events: &mut Vec<Event>) {
    if let Some(note) = note {
        ui.label(RichText::new(note).color(super::good(ui)));
        ui.add_space(8.0);
    }
    if let Some(error) = error {
        super::error_line(ui, error);
        ui.add_space(8.0);
    }
    ui.label(RichText::new("Two-factor sign-in").strong());
    match info {
        TwoFactorInfo::Loading => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Asking the server…");
            });
        }
        TwoFactorInfo::Unsupported => {
            ui.label(crate::security::UNSUPPORTED);
            ui.label(RichText::new("Ask whoever runs your Craft Cloud server to update it.").small().weak());
        }
        TwoFactorInfo::Failed(why) => {
            super::error_line(ui, why);
            if ui.button("Try again").clicked() {
                send(events, SecurityEvent::Refresh);
            }
        }
        TwoFactorInfo::Known(status) if status.enabled => {
            ui.label(format!("On. {} recovery codes left.", status.recovery_codes_left));
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(!busy, egui::Button::new("Turn off…")).clicked() {
                    send(events, SecurityEvent::StartDisable);
                }
                if ui.add_enabled(!busy, egui::Button::new("New recovery codes…")).clicked() {
                    send(events, SecurityEvent::StartRegenerate);
                }
            });
        }
        TwoFactorInfo::Known(status) => {
            ui.label("Off. Anyone who knows your password can sign in.");
            if status.pending {
                ui.label(RichText::new("A set-up was started and isn't finished.").small().weak());
            }
            ui.add_space(4.0);
            if ui.add_enabled(!busy, egui::Button::new("Set up two-factor sign-in…")).clicked() {
                send(events, SecurityEvent::StartSetup);
            }
        }
    }
    ui.add_space(14.0);
    ui.separator();
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ui.button("Change password…").clicked() {
            send(events, SecurityEvent::StartPassword);
        }
        if ui.button("Sign out other computers…").clicked() {
            send(events, SecurityEvent::StartSignOutOthers);
        }
    });
    ui.add_space(14.0);
    if super::wide_button(ui, true, "Back") {
        send(events, SecurityEvent::Close);
    }
}
