//! The code a join or a sign-in asks for when the account has two-factor sign-in: the six digits from the authenticator app, or a recovery
//! code instead. A page of the first start's style, after the account step.

use super::join::{self, FirstRun};
use crate::model::{Event, JoinMode};
use crate::security::SecondFactorForm;

pub(super) fn show(ui: &mut egui::Ui, form: &mut SecondFactorForm, run: &FirstRun, events: &mut Vec<Event>) {
    join::page(ui, run, |ui| {
        join::head(ui, run, "Account", "Enter your code", "This account has two-factor sign-in.");
        let label = if form.recovery { "Recovery code" } else { "Code from the authenticator app" };
        let hint = if form.recovery { "ABCD-EFGH-JKMN-PQRS" } else { "123456" };
        let response = join::field(ui, label, |ui| {
            let edit = super::kit::text_edit_large(&mut form.code).id(super::security::secret_id("second-factor-code")).hint_text(hint);
            ui.add(edit.desired_width(f32::INFINITY))
        });
        let enter = join::submitted(&response);
        let toggle = if form.recovery { "Use a code from the app" } else { "Use a recovery code" };
        if ui.checkbox(&mut form.recovery, toggle).changed() {
            form.code.clear();
            form.error = None;
        }
        let signing_in = form.form.mode == JoinMode::SignIn;
        join::note(
            ui,
            if signing_in {
                "A recovery code works once. After five wrong codes this sign-in ends and you sign in again."
            } else {
                "A recovery code works once. Your invite or code isn't used up until this works."
            },
        );
        if let Some(error) = &form.error {
            super::error_line(ui, error);
        }
        if join::escape(ui) {
            events.push(Event::CancelSecondFactor);
        }
        join::nav_row(ui, Some(Event::CancelSecondFactor), enter, Event::SubmitSecondFactor, events);
    });
}
