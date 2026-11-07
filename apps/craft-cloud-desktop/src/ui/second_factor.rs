//! The code a join or a sign-in asks for when the account has two-factor sign-in: the six digits from the authenticator app, or a recovery
//! code instead.

use egui::RichText;

use crate::model::{Event, JoinMode};
use crate::security::SecondFactorForm;

pub(super) fn show(ui: &mut egui::Ui, form: &mut SecondFactorForm, events: &mut Vec<Event>) {
    super::heading(ui, "Enter your code");
    ui.label("This account has two-factor sign-in.");
    ui.add_space(8.0);
    ui.label(if form.recovery { "Recovery code" } else { "Code from the authenticator app" });
    let hint = if form.recovery { "ABCD-EFGH-JKMN-PQRS" } else { "123456" };
    let response = ui.add(
        egui::TextEdit::singleline(&mut form.code).id(super::security::secret_id("second-factor-code")).hint_text(hint).desired_width(f32::INFINITY),
    );
    let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    ui.add_space(4.0);
    let toggle = if form.recovery { "Use a code from the app" } else { "Use a recovery code" };
    if ui.checkbox(&mut form.recovery, toggle).changed() {
        form.code.clear();
        form.error = None;
    }
    let signing_in = form.form.mode == JoinMode::SignIn;
    let note = if signing_in {
        "A recovery code works once. After five wrong codes this sign-in ends and you sign in again."
    } else {
        "A recovery code works once. Your invite or code isn't used up until this works."
    };
    ui.label(RichText::new(note).small().weak());
    ui.add_space(10.0);
    if let Some(error) = &form.error {
        super::error_line(ui, error);
        ui.add_space(8.0);
    }
    if super::wide_button(ui, true, if signing_in { "Sign in" } else { "Join" }) || enter {
        events.push(Event::SubmitSecondFactor);
    }
    ui.add_space(4.0);
    if ui.add(super::kit::Button::new("Back")).clicked() {
        events.push(Event::CancelSecondFactor);
    }
}
