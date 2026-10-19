//! Problems: one line, the whole text under Details, and what can be done about it.

use crate::model::{Busy, Event, Problem, ProblemAction, REMOVED};

/// A problem's line and its Details (selectable, so it can be copied into a message to whoever helps).
pub(super) fn line_with_details(ui: &mut egui::Ui, line: &str, details: &str) {
    super::error_line(ui, line);
    if !details.is_empty() {
        egui::CollapsingHeader::new("Details").id_salt("details").show(ui, |ui| {
            ui.add(egui::Label::new(egui::RichText::new(details).monospace().small()).selectable(true).wrap());
        });
    }
}

/// The screen for the agent not running, or for a problem with the account here.
pub(super) fn screen(ui: &mut egui::Ui, problem: &Problem, busy: Option<Busy>, events: &mut Vec<Event>) {
    super::heading(ui, "Craft Cloud");
    line_with_details(ui, &problem.line, &problem.details);
    ui.add_space(12.0);
    match problem.action {
        ProblemAction::TryAgain => {
            if super::wide_button(ui, true, "Try again") {
                events.push(Event::TryAgain);
            }
        }
        ProblemAction::ForgetAccount => {
            ui.label("Signing out forgets the account on this computer; the files stay. Then join again.");
            ui.add_space(6.0);
            if super::wide_button(ui, busy.is_none(), "Sign out") {
                events.push(Event::ForgetAccount);
            }
        }
        ProblemAction::None => {}
    }
}

/// This computer was removed from the account on another one.
pub(super) fn removed(ui: &mut egui::Ui, busy: Option<Busy>, events: &mut Vec<Event>) {
    super::heading(ui, "Craft Cloud");
    super::error_line(ui, REMOVED);
    ui.label("The files stay on this computer.");
    ui.add_space(12.0);
    if super::wide_button(ui, busy.is_none(), "Join again") {
        events.push(Event::ForgetAccount);
    }
}
