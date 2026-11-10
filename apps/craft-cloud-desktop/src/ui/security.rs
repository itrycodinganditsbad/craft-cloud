//! Security: two-factor sign-in (turn it on with a secret and a QR code, the recovery codes shown once, turn it off, new
//! recovery codes), the password, and signing the other computers out.

use egui::RichText;

use super::kit::{Column, JoinedCell, Part, Tokens, Tone};
use crate::model::{Event, Tab};
use crate::security::{CodesWhy, PasswordFields, SecurityActivity, SecurityEvent, SecurityPage, Step, TwoFactorInfo};

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
        Screen::Security(page) => {
            let mut typed: Vec<(&'static str, &str)> = match &page.step {
                Step::SetupPassword { password } => vec![("setup-password", password)],
                Step::SetupCode { code, .. } => vec![("setup-code", code)],
                Step::Disable { password, code, .. } => vec![("disable-password", password), ("disable-code", code)],
                Step::Regenerate { password, code } => vec![("regenerate-password", password), ("regenerate-code", code)],
                Step::Home | Step::Codes { .. } | Step::SignOutOthers => Vec::new(),
            };
            // The password form of the home page is there whatever the step.
            typed.extend([
                ("password-current", page.password.current.as_str()),
                ("password-new", page.password.new.as_str()),
                ("password-again", page.password.again.as_str()),
            ]);
            typed
        }
        Screen::SecondFactor(form) => vec![("second-factor-code", &form.code)],
        // The first start's password: only while the form is open to be typed in (not while a sign-in is with the agent).
        Screen::Join(form) => vec![(super::join::PASSWORD_FIELD, &form.password)],
        Screen::PasswordNeeded(pw) => vec![(super::join::INVITE_PASSWORD_FIELD, &pw.password), (super::join::INVITE_AGAIN_FIELD, &pw.again)],
        // The admin's confirmation: only while it is open to be typed in.
        Screen::Admin(a) if a.code.is_none() => match &a.dialog {
            Some(d) if !d.busy => {
                vec![(super::admin::PASSWORD_FIELD, &d.confirmation.password), (super::admin::CODE_FIELD, &d.confirmation.code)]
            }
            _ => Vec::new(),
        },
        // The confirmation of "Make a team space from this folder": only while the form is open to be typed in.
        Screen::SignedIn(o) => match &o.make_space {
            Some(m) if !m.busy && !m.resume => {
                vec![(super::make_space::PASSWORD_FIELD, &m.confirmation.password), (super::make_space::CODE_FIELD, &m.confirmation.code)]
            }
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

const SECRET_NAMES: [&str; 17] = [
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
    super::join::PASSWORD_FIELD,
    super::join::INVITE_PASSWORD_FIELD,
    super::join::INVITE_AGAIN_FIELD,
    super::admin::PASSWORD_FIELD,
    super::admin::CODE_FIELD,
    super::make_space::PASSWORD_FIELD,
    super::make_space::CODE_FIELD,
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

/// A password field of a step page.
fn password_field(ui: &mut egui::Ui, name: &str, label: &str, text: &mut String) -> egui::Response {
    ui.label(label);
    let response = ui.add(super::kit::text_edit(text).id(secret_id(name)).password(true).desired_width(f32::INFINITY));
    ui.add_space(6.0);
    response
}

pub(super) fn show(ui: &mut egui::Ui, page: &mut SecurityPage, now_ms: i64, events: &mut Vec<Event>) {
    if matches!(page.step, Step::Home | Step::SignOutOthers) {
        return home(ui, page, now_ms, events);
    }
    super::heading(ui, "Security");
    let (busy, error) = (page.busy, page.error.clone());
    match &mut page.step {
        Step::Home | Step::SignOutOthers => {}
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
                if ui.add(super::kit::Button::new("Copy")).clicked() {
                    ui.ctx().copy_text(secret.clone());
                }
            });
            ui.collapsing("The setup link", |ui| {
                ui.add(egui::Label::new(RichText::new(uri.as_str()).monospace().small()).selectable(true).wrap());
                if ui.add(super::kit::Button::new("Copy link")).clicked() {
                    ui.ctx().copy_text(uri.clone());
                }
            });
            ui.add_space(10.0);
            ui.label("2. Type the six digits the app shows now.");
            let response = ui.add(super::kit::text_edit(code).id(secret_id("setup-code")).hint_text("123456").desired_width(160.0));
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
            if ui.add(super::kit::Button::new("Copy all")).clicked() {
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
            let response = ui.add(super::kit::text_edit(code).id(secret_id("disable-code")).desired_width(f32::INFINITY));
            let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.add(super::kit::Button::new(if *recovery { "Use a code from the app" } else { "Use a recovery code instead" })).clicked() {
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
            let response = ui.add(super::kit::text_edit(code).id(secret_id("regenerate-code")).desired_width(160.0));
            let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.add_space(6.0);
            footer(ui, busy, error.as_deref(), "Make new codes", enter, events);
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
    if ui.add_enabled(!busy, super::kit::Button::new("Cancel")).clicked() {
        send(events, SecurityEvent::Home);
    }
}

/// The width from which the home page's three cells sit side by side.
const SIDE_BY_SIDE: f32 = 840.0;

/// The height of a label/value row of a cell.
const FACT_ROW_H: f32 = 40.0;

/// The page's start: the two-factor state and its actions, the password form, the computers signed in, and under them the
/// newest security events. While the sign-out of the other computers is asked for, its confirmation sits under the cells.
fn home(ui: &mut egui::Ui, page: &mut SecurityPage, now_ms: i64, events: &mut Vec<Event>) {
    let SecurityPage { overview, info, step, password, activity, activity_filter, error, note, busy, .. } = page;
    let busy = *busy;
    super::kit::section_label(ui, &super::kit::numbered(1, "Your account"));
    ui.add_space(6.0);
    super::heading(ui, "Security");
    if let Some(note) = note.as_deref() {
        ui.label(RichText::new(note).color(super::good(ui)));
        ui.add_space(8.0);
    }
    let asking_sign_out = matches!(step, Step::SignOutOthers);
    // On the home page the only thing that fails is the password form; the confirmation shows its own error.
    let (password_error, sign_out_error) = if asking_sign_out { (None, error.as_deref()) } else { (error.as_deref(), None) };
    // The first cell's badge follows the two-factor state: on is good, off a warning.
    let two_factor_tone = match info {
        TwoFactorInfo::Known(status) if status.enabled => Tone::Good,
        TwoFactorInfo::Known(_) => Tone::Warn,
        _ => Tone::Neutral,
    };
    let cells = [
        JoinedCell::new("01", two_factor_tone, "Two-factor sign-in"),
        JoinedCell::new("02", Tone::Accent, "Password"),
        JoinedCell::new("03", Tone::Warn, "Signed-in computers"),
    ];
    super::kit::joined_cells(ui, "security-cells", &cells, SIDE_BY_SIDE, |ui, i, part| match (i, part) {
        (0, Part::Body) => two_factor_body(ui, info, events),
        (0, Part::Footer) => two_factor_footer(ui, info, busy, events),
        (1, Part::Body) => password_body(ui, password, password_error, busy, asking_sign_out, events),
        (1, Part::Footer) => password_footer(ui, busy, asking_sign_out, events),
        (2, Part::Body) => computers_body(ui, overview, now_ms),
        (2, Part::Footer) => computers_footer(ui, overview, asking_sign_out, events),
        _ => {}
    });
    if asking_sign_out {
        ui.add_space(12.0);
        if let Some(error) = sign_out_error {
            super::error_line(ui, error);
            ui.add_space(6.0);
        }
        if busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Working…");
            });
        }
        let panel = super::confirm::Panel {
            key: egui::Id::new("sign-out-others"),
            title: "Sign out other computers",
            text: "Every other computer of your account is signed out. Their files stay there; they join again to sync.",
            confirm: "Sign them out",
            cancel: "Cancel",
            danger: true,
            enabled: !busy,
        };
        match super::confirm::show(ui, &panel) {
            super::confirm::Answer::Confirmed => send(events, SecurityEvent::Submit),
            super::confirm::Answer::Cancelled if !busy => send(events, SecurityEvent::Home),
            _ => {}
        }
    } else {
        super::confirm::idle(ui);
    }
    ui.add_space(20.0);
    security_activity(ui, activity, activity_filter, now_ms, events);
    ui.add_space(14.0);
}

/// A label at the left and its value at the right of a cell, over a hairline (`last` leaves the hairline out).
fn fact_row(ui: &mut egui::Ui, label: &str, value: egui::RichText, last: bool) {
    ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), FACT_ROW_H), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.label(label);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(value);
        });
    });
    if !last {
        super::kit::hairline(ui);
    }
}

fn two_factor_body(ui: &mut egui::Ui, info: &TwoFactorInfo, events: &mut Vec<Event>) {
    let t = Tokens::get(ui.ctx());
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
            if ui.add(super::kit::Button::new("Try again")).clicked() {
                send(events, SecurityEvent::Refresh);
            }
        }
        TwoFactorInfo::Known(status) if status.enabled => {
            super::kit::tag(ui, "On", Tone::Good);
            ui.add(egui::Label::new("Signing in asks for a code from your authenticator app as well as your password.").wrap());
            let turned_on = status.enabled_ms.filter(|at| *at > 0);
            ui.scope(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                fact_row(ui, "Recovery codes", RichText::new(format!("{} left", status.recovery_codes_left)).monospace(), turned_on.is_none());
                if let Some(at) = turned_on {
                    fact_row(ui, "Turned on", RichText::new(super::trash::date(at)).color(t.muted), true);
                }
            });
        }
        TwoFactorInfo::Known(status) => {
            super::kit::tag(ui, "Off", Tone::Neutral);
            ui.add(egui::Label::new("Off. Anyone who knows your password can sign in.").wrap());
            if status.pending {
                ui.label(RichText::new("A set-up was started and isn't finished.").small().weak());
            }
        }
    }
}

/// The width the two buttons of an enabled two-factor cell need to sit on one row; in a narrower cell they stack.
const TWO_BUTTONS_ONE_ROW: f32 = 292.0;

fn two_factor_footer(ui: &mut egui::Ui, info: &TwoFactorInfo, busy: bool, events: &mut Vec<Event>) {
    let TwoFactorInfo::Known(status) = info else { return };
    let width = ui.available_width();
    if !status.enabled {
        let size = egui::vec2(width, super::kit::CONTROL_H);
        if ui.add_enabled(!busy, super::kit::Button::new("Set up two-factor sign-in").primary().min_size(size)).clicked() {
            send(events, SecurityEvent::StartSetup);
        }
    } else if width >= TWO_BUTTONS_ONE_ROW {
        ui.allocate_ui_with_layout(egui::vec2(width, super::kit::CONTROL_H), egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // "Turn off" keeps its own width; "New recovery codes" takes the rest of the row.
            if ui.add_enabled(!busy, super::kit::Button::new("Turn off").danger()).clicked() {
                send(events, SecurityEvent::StartDisable);
            }
            let size = egui::vec2(ui.available_width() - ui.spacing().item_spacing.x, super::kit::CONTROL_H);
            if ui.add_enabled(!busy, super::kit::Button::new("New recovery codes").min_size(size)).clicked() {
                send(events, SecurityEvent::StartRegenerate);
            }
        });
    } else {
        let size = egui::vec2(width, super::kit::CONTROL_H);
        if ui.add_enabled(!busy, super::kit::Button::new("New recovery codes").min_size(size)).clicked() {
            send(events, SecurityEvent::StartRegenerate);
        }
        if ui.add_enabled(!busy, super::kit::Button::new("Turn off").danger().min_size(size)).clicked() {
            send(events, SecurityEvent::StartDisable);
        }
    }
}

/// The three fields of the password form, each under its label, and what went wrong with the last try.
/// Why the password form is off while the sign-out of the other computers is asked for: the question replaces what was
/// typed, so a submit would do nothing and starting it clears the fields.
const ASKING_HOVER: &str = "Answer the question about signing out the other computers first";

fn password_body(ui: &mut egui::Ui, fields: &mut PasswordFields, error: Option<&str>, busy: bool, asking: bool, events: &mut Vec<Event>) {
    let t = Tokens::get(ui.ctx());
    ui.add(egui::Label::new(RichText::new("Changing your password signs your other computers out.").size(13.0).color(t.muted)).wrap());
    let field = |ui: &mut egui::Ui, name: &str, label: &str, text: &mut String| {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            super::kit::section_label(ui, label);
            let response =
                ui.add_enabled(!busy && !asking, super::kit::text_edit(text).id(secret_id(name)).password(true).desired_width(f32::INFINITY));
            if asking { response.on_disabled_hover_text(ASKING_HOVER) } else { response }
        })
        .inner
    };
    field(ui, "password-current", "Current password", &mut fields.current);
    field(ui, "password-new", "New password", &mut fields.new);
    let again = field(ui, "password-again", "Repeat new password", &mut fields.again);
    if !asking && again.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
        send(events, SecurityEvent::SubmitPassword);
    }
    if let Some(error) = error {
        super::error_line(ui, error);
    }
}

fn password_footer(ui: &mut egui::Ui, busy: bool, asking: bool, events: &mut Vec<Event>) {
    let size = egui::vec2(ui.available_width(), super::kit::CONTROL_H);
    let label = if busy { "Changing…" } else { "Change password" };
    let button = ui.add_enabled(!busy && !asking, super::kit::Button::new(label).primary().min_size(size));
    let button = if asking { button.on_disabled_hover_text(ASKING_HOVER) } else { button };
    if button.clicked() {
        send(events, SecurityEvent::SubmitPassword);
    }
}

/// When a computer signed in: "today" within a day, else the date.
fn signed_in_text(now_ms: i64, created_ms: i64) -> Option<String> {
    if created_ms <= 0 {
        return None;
    }
    Some(if now_ms.saturating_sub(created_ms) < 86_400_000 {
        "signed in today".to_owned()
    } else {
        format!("signed in {}", super::trash::date(created_ms))
    })
}

/// The computers signed in, from the list the Computers tab keeps.
fn computers_body(ui: &mut egui::Ui, overview: &crate::model::Overview, now_ms: i64) {
    let t = Tokens::get(ui.ctx());
    match &overview.devices {
        None => {
            ui.label(RichText::new("The Computers tab lists them.").small().color(t.muted));
        }
        Some(devices) => {
            let active: Vec<_> = devices.iter().filter(|d| !d.revoked).collect();
            ui.scope(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for (n, device) in active.iter().enumerate() {
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        ui.add(egui::Label::new(RichText::new(&device.name).family(super::kit::medium()).color(t.ink_strong)).truncate());
                        if device.current {
                            ui.label(RichText::new("(this computer)").color(t.muted));
                        }
                    });
                    ui.add_space(2.0);
                    let platform = super::computers::platform_label(&device.platform);
                    let parts: Vec<String> =
                        [(!platform.is_empty()).then_some(platform), signed_in_text(now_ms, device.created_ms)].into_iter().flatten().collect();
                    if !parts.is_empty() {
                        super::kit::caption(ui, &parts.join(" · "));
                    }
                    ui.add_space(10.0);
                    if n + 1 < active.len() {
                        super::kit::hairline(ui);
                    }
                }
            });
        }
    }
    ui.add(egui::Label::new(RichText::new("Lost a computer? Sign out the others; this one stays signed in.").size(13.0).color(t.muted)).wrap());
}

fn computers_footer(ui: &mut egui::Ui, overview: &crate::model::Overview, asking: bool, events: &mut Vec<Event>) {
    let others = overview.devices.iter().flatten().any(|d| !d.revoked && !d.current);
    let size = egui::vec2(ui.available_width(), super::kit::CONTROL_H);
    if ui.add_enabled(others && !asking, super::kit::Button::new("Sign out other computers").min_size(size)).clicked() {
        send(events, SecurityEvent::StartSignOutOthers);
    }
}

/// "02 / Security activity": a label row with a link to the Activity tab, and the newest sign-ins and account changes in a
/// When / Event / Computer / Result table; a muted line says when there are none or they couldn't be loaded.
fn security_activity(ui: &mut egui::Ui, activity: &SecurityActivity, filter: &crate::filter::ActivityFilter, now_ms: i64, events: &mut Vec<Event>) {
    let t = Tokens::get(ui.ctx());
    let names = super::names(ui);
    super::kit::label_row(ui, &super::kit::numbered(2, "Security activity"), |ui| {
        if super::kit::text_link(ui, "All activity ↗", 12.0).clicked() {
            events.push(Event::Tab(Tab::Activity));
        }
    });
    ui.add_space(8.0);
    match activity {
        SecurityActivity::Loading => {
            ui.label(RichText::new("Loading security activity…").color(t.muted));
        }
        SecurityActivity::Failed(_) => {
            ui.label(RichText::new("Security activity isn't available right now.").color(t.muted));
        }
        SecurityActivity::Loaded(list) if list.is_empty() => {
            ui.label(RichText::new("No sign-ins or account changes were recorded yet.").color(t.muted));
        }
        SecurityActivity::Loaded(list) => {
            for edit in super::activity_filter::controls(ui, "security", filter, list, &names) {
                events.push(Event::SecurityActivityFilter(edit));
            }
            let visible = filter.visible(list);
            if visible.is_empty() {
                if let Some(edit) = super::activity_filter::nothing_matches(ui) {
                    events.push(Event::SecurityActivityFilter(edit));
                }
                return;
            }
            let columns =
                [Column::fixed("When", 160.0), Column::flex("Event", 240.0), Column::fixed("Computer", 160.0), Column::fixed("Result", 120.0)];
            super::kit::table(ui, "security-activity", &columns, visible.len(), ACTIVITY_ROW_H, |ui, row, column| {
                let Some(event) = visible.get(row).copied() else { return };
                match column {
                    0 => {
                        ui.label(RichText::new(super::ago(now_ms, event.at_ms)).monospace());
                    }
                    1 => {
                        let label = crate::audit::action_label(&event.action);
                        let text = if event.count > 1 { format!("{label} ×{}", event.count) } else { label.to_owned() };
                        super::kit::truncated(ui, text);
                    }
                    2 => match event.actor_device {
                        Some(device) => {
                            let computer = names.computer(device);
                            ui.add(egui::Label::new(RichText::new(&computer.text).color(t.muted)).truncate().selectable(false))
                                .on_hover_text(&computer.full);
                        }
                        None => {
                            ui.label(RichText::new("—").color(t.muted));
                        }
                    },
                    _ => {
                        if event.outcome == "failure" {
                            super::kit::tag(ui, "Refused", Tone::Bad);
                        } else {
                            super::kit::tag(ui, "OK", Tone::Good);
                        }
                    }
                }
            });
        }
    }
}

/// The height of a row of the security activity table.
const ACTIVITY_ROW_H: f32 = 48.0;
