//! "Join your Craft Cloud", the first start, as the design has it: a header with the wordmark, a sidebar of the three steps (server,
//! account, folder and name; a line above the page in a narrow window), a page with Back and Continue for the step, and a footer
//! that says which step it is. The account step is a sign-in (user name and password) or a join string. The code of a two-factor
//! sign-in and an invite's new password are pages of the same style after the account step.

use egui::{Color32, Margin, RichText};

use super::kit::{self, Tokens};
use crate::model::{Event, JoinForm, JoinMode, JoinStep, PasswordForm, RailState, RailStep};

/// The id name of the sign-in's password field (see `security::forget_secrets`).
pub(super) const PASSWORD_FIELD: &str = "join-password";
/// The id names of an invite's two password fields.
pub(super) const INVITE_PASSWORD_FIELD: &str = "join-invite-password";
pub(super) const INVITE_AGAIN_FIELD: &str = "join-invite-again";

/// The widest a page's content gets, so it stays a form in a wide window.
const COLUMN_MAX: f32 = 640.0;
/// From this width on the steps sit in a sidebar left of the page; below it they are one line above it.
const SIDEBAR_FROM: f32 = 800.0;
/// The sidebar takes this share of the window, within [`SIDEBAR_MIN`] and [`SIDEBAR_MAX`].
const SIDEBAR_SHARE: f32 = 0.3;
const SIDEBAR_MIN: f32 = 280.0;
const SIDEBAR_MAX: f32 = 380.0;
/// The height of the header and of the footer strip.
const HEADER_H: f32 = 56.0;
const FOOTER_H: f32 = 36.0;
/// The width the number of a step takes, whichever way it is drawn.
const MARK_W: f32 = 34.0;
/// The room between a page's groups of fields.
const GROUP_GAP: f32 = 22.0;

/// What the screens of the first start know about the frame around them.
pub(super) struct FirstRun {
    /// The step being asked for (0 to 2).
    pub current: usize,
    pub rail: [RailStep; 3],
    /// The steps are in a sidebar (the window is wide enough), not in a line above the page.
    pub beside: bool,
}

impl FirstRun {
    pub(super) fn new(current: usize, rail: [RailStep; 3], window_width: f32) -> Self {
        Self { current, rail, beside: window_width >= SIDEBAR_FROM }
    }
}

/// The header: the wordmark and "First start" over a hairline.
pub(super) fn header(ui: &mut egui::Ui) {
    let frame = egui::Frame::new().fill(Tokens::get(ui.ctx()).bg);
    egui::Panel::top("first-run-header").frame(frame).show_separator_line(false).show(ui, |ui| {
        ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), HEADER_H), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.add_space(20.0);
            kit::wordmark(ui, 22.0);
            ui.add_space(8.0);
            kit::section_label(ui, "First start");
        });
        kit::hairline(ui);
    });
}

/// The footer strip: which step this is.
pub(super) fn footer(ui: &mut egui::Ui, run: &FirstRun) {
    let frame = egui::Frame::new().fill(Tokens::get(ui.ctx()).bg);
    egui::Panel::bottom("first-run-footer").frame(frame).show_separator_line(false).show(ui, |ui| {
        kit::hairline(ui);
        ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), FOOTER_H), egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(20.0);
            kit::caption(ui, &format!("Step {} of {}", run.current + 1, run.rail.len()));
        });
    });
}

/// The sidebar of the steps: full height, a hairline on its right, a row for each step with a hairline under it.
pub(super) fn sidebar(ui: &mut egui::Ui, run: &FirstRun) {
    let t = Tokens::get(ui.ctx());
    let width = (ui.available_width() * SIDEBAR_SHARE).clamp(SIDEBAR_MIN, SIDEBAR_MAX);
    let frame = egui::Frame::new().fill(t.bg);
    egui::Panel::left("first-run-steps").exact_size(width).resizable(false).show_separator_line(false).frame(frame).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        for (n, step) in run.rail.iter().enumerate() {
            rail_row(ui, n, step);
        }
        let rect = ui.max_rect();
        ui.painter().vline(rect.right() - 0.5, rect.y_range(), t.hairline());
    });
}

/// The number of a step: filled green when passed, filled accent when current, outlined and muted when to come.
fn step_mark(ui: &mut egui::Ui, n: usize, state: RailState) -> egui::Response {
    let text = format!("{:02}", n + 1);
    match state {
        RailState::Done => kit::badge(ui, &text, kit::Tone::Good),
        RailState::Current => kit::badge(ui, &text, kit::Tone::Accent),
        RailState::Later => kit::tag(ui, &text, kit::Tone::Neutral),
    }
}

fn rail_row(ui: &mut egui::Ui, n: usize, step: &RailStep) {
    let t = Tokens::get(ui.ctx());
    let fill = if step.state == RailState::Current { t.sunken } else { Color32::TRANSPARENT };
    egui::Frame::new().fill(fill).inner_margin(Margin::same(16)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            let mark = step_mark(ui, n, step.state);
            // The outlined mark is narrower than the filled ones: the titles line up all the same.
            ui.add_space((MARK_W - mark.rect.width()).max(0.0));
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                let title = RichText::new(step.title).strong();
                ui.label(if step.state == RailState::Later { title.color(t.muted) } else { title.color(t.ink_strong) });
                ui.add(egui::Label::new(RichText::new(&step.detail).size(13.0).color(t.muted)).truncate());
            });
        });
    });
    kit::hairline(ui);
}

/// The steps in one line, for a window too narrow for the sidebar.
fn rail_line(ui: &mut egui::Ui, run: &FirstRun) {
    let t = Tokens::get(ui.ctx());
    ui.horizontal_wrapped(|ui| {
        for (n, step) in run.rail.iter().enumerate() {
            step_mark(ui, n, step.state);
            let title = RichText::new(step.title).strong();
            ui.label(if step.state == RailState::Later { title.color(t.muted) } else { title.color(t.ink_strong) });
            ui.add_space(8.0);
        }
    });
}

/// A page of the first start: [`COLUMN_MAX`] wide at most, left-aligned, with the room the design gives it (and the line of
/// steps above it when there is no sidebar). Its groups are [`GROUP_GAP`] apart.
pub(super) fn page(ui: &mut egui::Ui, run: &FirstRun, add: impl FnOnce(&mut egui::Ui)) {
    let margin = if run.beside { Margin::symmetric(48, 24) } else { Margin::symmetric(24, 16) };
    egui::Frame::new().inner_margin(margin).show(ui, |ui| {
        ui.set_max_width(ui.available_width().min(COLUMN_MAX));
        if !run.beside {
            rail_line(ui, run);
            ui.add_space(12.0);
        }
        ui.spacing_mut().item_spacing.y = GROUP_GAP;
        add(ui);
    });
}

/// What a page starts with: the step's eyebrow (`02 / Account`), its headline and a sentence under it.
pub(super) fn head(ui: &mut egui::Ui, run: &FirstRun, name: &str, title: &str, lead: &str) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 8.0;
        kit::section_label(ui, &kit::numbered(run.current + 1, name));
        kit::headline_large(ui, title);
        ui.add(egui::Label::new(RichText::new(lead).color(Tokens::get(ui.ctx()).muted)));
    });
}

/// A field with its label (upper-case mono, 6 px above it): the label names the field for screen readers.
pub(super) fn field(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui) -> egui::Response) -> egui::Response {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 6.0;
        let name = kit::section_label(ui, label);
        add(ui).labelled_by(name.id)
    })
    .inner
}

/// A small note under a field.
pub(super) fn note(ui: &mut egui::Ui, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).size(13.0).color(Tokens::get(ui.ctx()).muted)));
}

/// Back (outlined, when `back` is given) and the primary button `label` (Continue; Sign in or Join on the last page), filling the row, on one row. `enter` is Enter pressed in a
/// field of the page.
pub(super) fn nav_row(ui: &mut egui::Ui, back: Option<Event>, enter: bool, label: &str, next: Event, events: &mut Vec<Event>) {
    ui.horizontal(|ui| {
        if let Some(back) = back
            && ui.add(kit::Button::new("Back").large()).clicked()
        {
            events.push(back);
        }
        let size = egui::vec2(ui.available_width(), kit::CONTROL_H_LARGE);
        if ui.add_sized(size, kit::Button::new(label).primary().large().min_size(size)).clicked() || enter {
            events.push(next);
        }
    });
}

/// Whether Esc was pressed.
pub(super) fn escape(ui: &egui::Ui) -> bool {
    ui.input(|i| i.key_pressed(egui::Key::Escape))
}

/// Whether Enter was pressed in the field that just lost focus.
pub(super) fn submitted(response: &egui::Response) -> bool {
    response.lost_focus() && response.ctx.input(|i| i.key_pressed(egui::Key::Enter))
}

/// The join form's page for its step; while `joining`, shown as sent (nothing can be changed or sent again).
pub(super) fn form(ui: &mut egui::Ui, form: &mut JoinForm, joining: bool, run: &FirstRun, events: &mut Vec<Event>) {
    page(ui, run, |ui| {
        let signing_in = form.mode == JoinMode::SignIn;
        let step = form.step;
        let (name, title, lead) = match step {
            JoinStep::Server => ("Server", "Find your Craft Cloud", "Type the address of the server your Craft Cloud lives on."),
            JoinStep::Account if signing_in => (
                "Account",
                "Join your Craft Cloud",
                "Sign in with the account your admin made for you. A join string from another computer works too.",
            ),
            JoinStep::Account => ("Account", "Join your Craft Cloud", "Paste the join string you were given. Signing in with an account works too."),
            JoinStep::Folder => ("Folder", "Folder and name", "Where your files live on this computer, and what this computer is called."),
        };
        head(ui, run, name, title, lead);
        let mut enter = false;
        ui.add_enabled_ui(!joining, |ui| match step {
            JoinStep::Server => enter |= server_step(ui, form, events),
            JoinStep::Account => enter |= account_step(ui, form, events),
            JoinStep::Folder => enter |= folder_step(ui, form, events),
        });

        if let Some(error) = &form.error {
            super::error_line(ui, error);
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
            return;
        }
        let back = (step != JoinStep::Server).then_some(Event::JoinBack);
        if back.is_some() && escape(ui) {
            events.push(Event::JoinBack);
        }
        let label = match (step, signing_in) {
            (JoinStep::Folder, true) => "Sign in",
            (JoinStep::Folder, false) => "Join",
            _ => "Continue",
        };
        nav_row(ui, back, enter, label, Event::JoinNext, events);
    });
}

/// 01: the server's address, or (with a join string, which names its own server) a way to the other form.
fn server_step(ui: &mut egui::Ui, form: &mut JoinForm, events: &mut Vec<Event>) -> bool {
    if form.mode == JoinMode::SignIn {
        let response =
            field(ui, "Server", |ui| ui.add(kit::text_edit_large(&mut form.server).hint_text("cloud.example.com").desired_width(f32::INFINITY)));
        let enter = submitted(&response);
        ui.horizontal(|ui| {
            note(ui, "No account? A join string from your admin has the address in it.");
            if kit::text_link(ui, "Use a join string", 11.0).clicked() {
                events.extend([Event::SetJoinMode(JoinMode::JoinString), Event::JoinNext]);
            }
        });
        enter
    } else {
        note(ui, "A join string has your server's address in it, so there is nothing to type here.");
        if kit::text_link(ui, "Sign in with a user name instead", 11.0).clicked() {
            events.push(Event::SetJoinMode(JoinMode::SignIn));
        }
        false
    }
}

/// 02: the switch between signing in and a join string, and the fields of the chosen one.
fn account_step(ui: &mut egui::Ui, form: &mut JoinForm, events: &mut Vec<Event>) -> bool {
    let chosen = usize::from(form.mode == JoinMode::JoinString);
    let segments = [kit::Segment::new("Sign in"), kit::Segment::new("Join string")];
    if let Some(index) = kit::segmented(ui, "How to join", &segments, chosen) {
        let mode = if index == 0 { JoinMode::SignIn } else { JoinMode::JoinString };
        events.push(Event::SetJoinMode(mode));
    }
    let mut enter = false;
    if form.mode == JoinMode::SignIn {
        let response = field(ui, "Username", |ui| ui.add(kit::text_edit_large(&mut form.user).desired_width(f32::INFINITY)));
        enter |= submitted(&response);
        let response = field(ui, "Password", |ui| {
            let edit = kit::text_edit_large(&mut form.password).id(super::security::secret_id(PASSWORD_FIELD)).password(true);
            ui.add(edit.desired_width(f32::INFINITY))
        });
        enter |= submitted(&response);
        note(ui, "If your account has two-factor sign-in, the next step asks for the code.");
    } else {
        let response = field(ui, "Join string", |ui| {
            ui.add(kit::text_edit_large(&mut form.join).hint_text("cloud.example.com/K7M4-Q9TX").desired_width(f32::INFINITY))
        });
        enter |= submitted(&response);
        note(ui, "An invite, or a code from a computer that is signed in. Pasting the link works too.");
    }
    enter
}

/// 03: the folder (its Change… button measured, not a fixed width) and this computer's name.
fn folder_step(ui: &mut egui::Ui, form: &mut JoinForm, events: &mut Vec<Event>) -> bool {
    let mut enter = false;
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 6.0;
        let name = kit::section_label(ui, "Folder");
        let before = form.folder.clone();
        ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), kit::CONTROL_H_LARGE), egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.add(kit::Button::new("Change…").large()).clicked() {
                events.push(Event::ChangeFolder);
            }
            let width = ui.available_width();
            let response = ui.add(kit::text_edit_large(&mut form.folder).desired_width(width)).labelled_by(name.id);
            enter |= submitted(&response);
        });
        if form.folder != before {
            form.ask_adopt = false;
        }
    });
    let response = field(ui, "This computer's name", |ui| {
        ui.add(kit::text_edit_large(&mut form.device_name).hint_text("this computer's name").desired_width(f32::INFINITY))
    });
    enter | submitted(&response)
}

/// An invite to a new account: its password, twice.
pub(super) fn password(ui: &mut egui::Ui, pw: &mut PasswordForm, run: &FirstRun, events: &mut Vec<Event>) {
    page(ui, run, |ui| {
        head(ui, run, "Account", "Choose a password", "This invite is for a new account. Choose its password: you need it to sign in on the server.");
        let response = field(ui, "Password", |ui| {
            let edit = kit::text_edit_large(&mut pw.password).id(super::security::secret_id(INVITE_PASSWORD_FIELD));
            ui.add(edit.password(true).desired_width(f32::INFINITY))
        });
        let mut enter = submitted(&response);
        let response = field(ui, "The same password again", |ui| {
            let edit = kit::text_edit_large(&mut pw.again).id(super::security::secret_id(INVITE_AGAIN_FIELD));
            ui.add(edit.password(true).desired_width(f32::INFINITY))
        });
        enter |= submitted(&response);
        if let Some(error) = &pw.error {
            super::error_line(ui, error);
        }
        if escape(ui) {
            events.push(Event::CancelPassword);
        }
        nav_row(ui, Some(Event::CancelPassword), enter, "Join", Event::SubmitPassword, events);
    });
}
