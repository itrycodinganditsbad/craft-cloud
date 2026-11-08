//! The window: a view of [`Model::screen`]. [`show`] draws the screen, lets text fields edit its forms in place and
//! returns the [`Event`]s its buttons sent, for [`Model::update`]. It reads no clock (the time comes in) and does no
//! I/O, so kittest draws every screen against a fake agent.

mod activity_filter;
mod add_computer;
mod admin;
mod audit;
mod computers;
mod confirm;
mod files;
mod folders;
mod join;
pub mod kit;
mod make_space;
mod overview;
mod problem;
mod second_factor;
mod security;
mod shell;
mod shortcuts;
mod spaces;
mod trash;
mod versions;

use egui::{Color32, RichText};

use self::kit::Tokens;

use crate::model::{Event, Model, Screen};

/// The window's size at the start (it can be resized).
pub const WINDOW_SIZE: [f32; 2] = [960.0, 640.0];
/// The smallest the window can be made: the header's tabs wrap onto a second line there.
pub const MIN_WINDOW_SIZE: [f32; 2] = [640.0, 480.0];
/// The window's title, and the tray's tooltip.
pub const TITLE: &str = "Craft Cloud";

/// Draws the window for `model` at `now_ms`, and returns what its buttons asked for.
pub fn show(ui: &mut egui::Ui, model: &mut Model, now_ms: i64) -> Vec<Event> {
    let mut events = Vec::new();
    if !kit::ensure(ui.ctx()) {
        return events;
    }
    events.extend(shortcuts::read(ui.ctx(), model));
    security::forget_secrets(ui.ctx(), &model.screen);
    stash_names(ui.ctx(), model);
    // The signed-in pages sit between a header with the tabs and a footer strip; the other screens fill the window.
    let shell = shell::Shell::of(model);
    // The first start has its own frame: a header, a footer with the step, and a sidebar of the steps in a wide window.
    let first_run = model.first_run_rail().map(|(current, rail)| join::FirstRun::new(current, rail, ui.available_width()));
    let frame = if let Some(shell) = &shell {
        shell::header(ui, shell, &mut events);
        shell::footer(ui, shell, &mut events);
        egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin::symmetric(24, 16))
    } else if let Some(run) = &first_run {
        join::header(ui);
        join::footer(ui, run);
        if run.beside {
            join::sidebar(ui, run);
        }
        // The pages keep their own margins, so the scroll bar sits at the window's edge.
        egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin::ZERO)
    } else {
        egui::Frame::central_panel(ui.style())
    };
    egui::CentralPanel::default().frame(frame).show(ui, |ui| {
        // The kit's scroll bars are solid: the bar shows whenever the page overflows and takes its own room, so it never
        // covers the content's right edge.
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(8.0);
            let busy = model.busy;
            let (autostart_problem, note) = (model.autostart_problem.clone(), model.note.clone());
            match &mut model.screen {
                Screen::Starting => starting(ui),
                Screen::NotRunning(p) | Screen::Error(p) => problem::screen(ui, p, busy, &mut events),
                Screen::Join(form) => with_first_run(&first_run, |run| join::form(ui, form, false, run, &mut events)),
                Screen::Joining { form, .. } => with_first_run(&first_run, |run| join::form(ui, form, true, run, &mut events)),
                Screen::PasswordNeeded(pw) => with_first_run(&first_run, |run| join::password(ui, pw, run, &mut events)),
                Screen::SecondFactor(form) => with_first_run(&first_run, |run| second_factor::show(ui, form, run, &mut events)),
                Screen::Security(page) => security::show(ui, page, now_ms, &mut events),
                Screen::SignedIn(o) => overview::show(ui, o, busy, now_ms, autostart_problem.as_deref(), &mut events),
                Screen::AddingComputer(adding) => add_computer::show(ui, adding, now_ms, &mut events),
                Screen::Admin(a) => admin::show(ui, a, now_ms, &mut events),
                Screen::Activity(a) => audit::activity(ui, a, now_ms, &mut events),
                Screen::Versions(versions) => self::versions::show(ui, versions, now_ms, &mut events),
                Screen::Trash(trash) => self::trash::show(ui, trash, now_ms, &mut events),
                Screen::Files(files) => self::files::show(ui, files, now_ms, &mut events),
                Screen::Shared(shared) => self::spaces::show(ui, shared, now_ms, &mut events),
                Screen::Removed => problem::removed(ui, busy, &mut events),
            }
            if let Some(note) = note {
                ui.add_space(12.0);
                ui.label(RichText::new(note).small().weak());
            }
            ui.add_space(8.0);
        });
    });
    events
}

/// Draws a page of the first start with its frame's facts (the model gives them for exactly those screens).
fn with_first_run(run: &Option<join::FirstRun>, draw: impl FnOnce(&join::FirstRun)) {
    if let Some(run) = run {
        draw(run);
    }
}

/// What names the IDs of the page being drawn: the model's, and the accounts of the admin's list.
fn stash_names(ctx: &egui::Context, model: &Model) {
    let mut names = model.names.clone();
    if let Screen::Admin(admin) = &model.screen {
        for user in &admin.list.users {
            names.learn_account(user.id, &user.username);
        }
    }
    ctx.data_mut(|d| d.insert_temp(egui::Id::new("craft-cloud-names"), names));
}

/// The names the page can use for IDs (see [`stash_names`]).
fn names(ui: &egui::Ui) -> crate::names::Names {
    ui.data(|d| d.get_temp::<crate::names::Names>(egui::Id::new("craft-cloud-names"))).unwrap_or_default()
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

/// The widest a [`wide_button`] gets, so it stays a button in a wide window.
const WIDE_BUTTON_MAX: f32 = 420.0;

/// A main button, wide.
fn wide_button(ui: &mut egui::Ui, enabled: bool, text: &str) -> bool {
    let size = egui::vec2(ui.available_width().min(WIDE_BUTTON_MAX), kit::CONTROL_H);
    ui.add_enabled_ui(enabled, |ui| ui.add_sized(size, kit::Button::new(text).primary().min_size(size))).inner.clicked()
}

/// The good-news colour (synced, joined).
fn good(ui: &egui::Ui) -> Color32 {
    Tokens::get(ui.ctx()).good
}

/// The server's address without its scheme, as people say it: `cloud.shadows.ch`.
pub fn host(server: &str) -> &str {
    let rest = ["https://", "http://"]
        .iter()
        .find_map(|scheme| server.get(..scheme.len()).filter(|start| start.eq_ignore_ascii_case(scheme)).and_then(|_| server.get(scheme.len()..)));
    rest.unwrap_or(server).trim_end_matches('/')
}

/// The agent's state in words, as the footer and the tray's menu say it: `Online · Up to date`. Empty when the status isn't known.
pub fn state_line(status: Option<&craft_cloud_app::Status>) -> String {
    status.map(|s| format!("{} · {}", if s.online { "Online" } else { "Offline" }, overview::sync_line(s).0)).unwrap_or_default()
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
mod tests_make_space;

#[cfg(test)]
mod tests_security;

#[cfg(test)]
mod tests_spaces;
