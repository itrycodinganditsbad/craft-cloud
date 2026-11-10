//! The window: a view of [`Model::screen`]. [`show`] draws the screen, lets text fields edit its forms in place and
//! returns the [`Event`]s its buttons sent, for [`Model::update`]. It reads no clock (the time comes in) and does no
//! I/O, so kittest draws every screen against a fake agent.

mod add_computer;
mod admin;
mod audit;
mod computers;
mod join;
pub mod kit;
mod overview;
mod problem;
mod second_factor;
mod security;
mod trash;
mod versions;

use egui::{Color32, RichText};

use self::kit::Tokens;

use crate::model::{Event, Model, Screen};

/// The window's size at the start (it can be resized).
pub const WINDOW_SIZE: [f32; 2] = [420.0, 560.0];
/// The window's title, and the tray's tooltip.
pub const TITLE: &str = "Craft Cloud";

/// Draws the window for `model` at `now_ms`, and returns what its buttons asked for.
pub fn show(ui: &mut egui::Ui, model: &mut Model, now_ms: i64) -> Vec<Event> {
    let mut events = Vec::new();
    if !kit::ensure(ui.ctx()) {
        return events;
    }
    security::forget_secrets(ui.ctx(), &model.screen);
    egui::CentralPanel::default().show(ui, |ui| {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(8.0);
            let busy = model.busy;
            let (autostart_problem, note) = (model.autostart_problem.clone(), model.note.clone());
            match &mut model.screen {
                Screen::Starting => starting(ui),
                Screen::NotRunning(p) | Screen::Error(p) => problem::screen(ui, p, busy, &mut events),
                Screen::Join(form) => join::form(ui, form, false, &mut events),
                Screen::Joining { form, .. } => join::form(ui, form, true, &mut events),
                Screen::PasswordNeeded(pw) => join::password(ui, pw, &mut events),
                Screen::SecondFactor(form) => second_factor::show(ui, form, &mut events),
                Screen::Security(page) => security::show(ui, page, &mut events),
                Screen::SignedIn(o) => overview::show(ui, o, busy, now_ms, autostart_problem.as_deref(), &mut events),
                Screen::AddingComputer(adding) => add_computer::show(ui, adding, now_ms, &mut events),
                Screen::Admin(a) => admin::show(ui, a, now_ms, &mut events),
                Screen::Activity(a) => audit::activity(ui, a, now_ms, &mut events),
                Screen::Versions(versions) => self::versions::show(ui, versions, now_ms, &mut events),
                Screen::Trash(trash) => self::trash::show(ui, trash, now_ms, &mut events),
                Screen::Removed => problem::removed(ui, busy, &mut events),
            }
            if let Some(note) = note {
                ui.add_space(12.0);
                ui.label(RichText::new(note).small().weak());
            }
        });
    });
    events
}

fn starting(ui: &mut egui::Ui) {
    wordmark(ui);
    ui.horizontal(|ui| {
        ui.spinner();
        ui.label("Starting Craft Cloud…");
    });
}

/// A screen's heading.
fn heading(ui: &mut egui::Ui, text: &str) {
    kit::headline(ui, text);
    ui.add_space(10.0);
}

/// A screen's heading that is the product's name.
fn wordmark(ui: &mut egui::Ui) {
    kit::wordmark(ui, 19.0);
    ui.add_space(10.0);
}

/// A line saying what went wrong.
fn error_line(ui: &mut egui::Ui, text: &str) {
    let color = Tokens::get(ui.ctx()).bad;
    ui.add(egui::Label::new(RichText::new(text).color(color)).wrap());
}

/// A problem's one line, from its whole text.
fn problem_line(reason: &str) -> String {
    crate::model::first_sentence(reason)
}

/// A main button, wide.
fn wide_button(ui: &mut egui::Ui, enabled: bool, text: &str) -> bool {
    let size = egui::vec2(ui.available_width(), kit::CONTROL_H);
    ui.add_enabled_ui(enabled, |ui| ui.add_sized(size, kit::Button::new(text).primary().min_size(size))).inner.clicked()
}

/// The good-news colour (synced, joined).
fn good(ui: &egui::Ui) -> Color32 {
    Tokens::get(ui.ctx()).good
}

/// The server's address without its scheme, as people say it: `cloud.shadows.ch`.
pub fn host(server: &str) -> &str {
    server.strip_prefix("https://").or_else(|| server.strip_prefix("http://")).unwrap_or(server).trim_end_matches('/')
}

/// How long ago `then_ms` was, at `now_ms`: "just now", "5 min ago", "3 h ago", "2 days ago".
pub fn ago(now_ms: i64, then_ms: i64) -> String {
    let secs = now_ms.saturating_sub(then_ms).max(0) / 1000;
    match secs {
        0..60 => "just now".into(),
        60..3_600 => format!("{} min ago", secs / 60),
        3_600..86_400 => format!("{} h ago", secs / 3_600),
        86_400..172_800 => "yesterday".into(),
        _ => format!("{} days ago", secs / 86_400),
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod tests_kit;

#[cfg(test)]
mod tests_security;
