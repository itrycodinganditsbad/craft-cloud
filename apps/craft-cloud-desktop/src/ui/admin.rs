//! The admin pages: the users (search, pages), a new account, one account (role, quota, disable, invites, devices),
//! the password a change asks for, and the code a change made, shown once.

use egui::RichText;

use crate::admin::{
    Access, Action, AdminEvent, AdminScreen, CodeKind, CreateForm, Dialog, RoleChoice, ShownCode, UserList, UserView, View, format_bytes,
};
use craft_cloud_app::AdminUserInfo;

use crate::model::Event;

pub(super) fn show(ui: &mut egui::Ui, admin: &mut AdminScreen, now_ms: i64, events: &mut Vec<Event>) {
    let me = admin.overview.status.account.clone().unwrap_or_default();
    // A code just made comes first, then the password a change asks for, then the page.
    if let Some(code) = &admin.code {
        return shown_code(ui, code, now_ms, events);
    }
    if let Some(dialog) = &mut admin.dialog {
        return confirmation(ui, dialog, events);
    }
    let access = admin.overview.access.clone();
    match &mut admin.view {
        View::List => users(ui, &access, &mut admin.list, &me, events),
        View::Create(form) => create(ui, form, events),
        View::User(view) => {
            let user = admin.list.users.iter().find(|u| u.id == view.id).cloned();
            account(ui, view, user.as_ref(), &me, now_ms, events);
        }
    }
}

fn send(events: &mut Vec<Event>, event: AdminEvent) {
    events.push(Event::Admin(event));
}

/// "Enter" in the field just edited.
fn entered(response: &egui::Response, ui: &egui::Ui) -> bool {
    response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
}

/// What a user is, in a line: role, computers, what they store, and the state of the account.
fn describe(user: &AdminUserInfo) -> String {
    let mut parts = vec![if user.role == "admin" { "Admin".to_owned() } else { "Member".to_owned() }];
    parts.push(match user.devices_active {
        1 => "1 computer".to_owned(),
        n => format!("{n} computers"),
    });
    parts.push(match user.quota_bytes {
        Some(quota) => format!("{} of {}", format_bytes(user.stored_bytes), format_bytes(quota)),
        None => format_bytes(user.stored_bytes),
    });
    if user.disabled_ms.is_some() {
        parts.push("disabled".to_owned());
    }
    if user.invite_pending {
        parts.push("invite open".to_owned());
    }
    parts.join(" · ")
}

fn users(ui: &mut egui::Ui, access: &Access, list: &mut UserList, me: &str, events: &mut Vec<Event>) {
    super::heading(ui, "Administration");
    if let Access::Admin(info) = access {
        ui.label(
            RichText::new(format!(
                "Craft Cloud server {} · {} accounts · {} computers · {} stored ({}, {})",
                info.version,
                info.users,
                info.devices_active,
                format_bytes(info.stored_bytes),
                info.storage_kind,
                info.database_kind
            ))
            .small()
            .weak(),
        );
        ui.add_space(8.0);
    }
    ui.horizontal(|ui| {
        if ui.button("Back").clicked() {
            send(events, AdminEvent::Close);
        }
        if ui.button("New account").clicked() {
            send(events, AdminEvent::NewUser);
        }
        if ui.add_enabled(!list.loading, egui::Button::new("Refresh")).clicked() {
            send(events, AdminEvent::Reload);
        }
    });
    ui.add_space(8.0);

    ui.horizontal(|ui| {
        let width = (ui.available_width() - 130.0).max(120.0);
        let field = ui.add(egui::TextEdit::singleline(&mut list.search).hint_text("Search accounts").desired_width(width));
        if ui.button("Search").clicked() || entered(&field, ui) {
            send(events, AdminEvent::Search);
        }
        if !list.applied.is_empty() && ui.button("Clear").clicked() {
            send(events, AdminEvent::ClearSearch);
        }
    });
    ui.add_space(8.0);

    if let Some(error) = &list.error {
        super::error_line(ui, error);
        ui.add_space(6.0);
    }
    if list.loading && !list.loaded {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Asking the server…");
        });
    } else if list.users.is_empty() && list.loaded && list.error.is_none() {
        ui.label(if list.applied.is_empty() { "No accounts." } else { "No account has that in its name." });
    }
    for user in list.users.clone() {
        ui.push_id(user.id, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    let name = if user.username == me { format!("{} (you)", user.username) } else { user.username.clone() };
                    ui.label(RichText::new(name).strong());
                    ui.add(egui::Label::new(RichText::new(describe(&user)).small().weak()).wrap());
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Manage").clicked() {
                        send(events, AdminEvent::OpenUser(user.id));
                    }
                });
            });
            ui.separator();
        });
    }
    if list.has_previous() || list.next.is_some() {
        ui.horizontal(|ui| {
            if ui.add_enabled(list.has_previous() && !list.loading, egui::Button::new("Previous")).clicked() {
                send(events, AdminEvent::PreviousPage);
            }
            ui.label(format!("Page {}", list.page()));
            if ui.add_enabled(list.next.is_some() && !list.loading, egui::Button::new("Next")).clicked() {
                send(events, AdminEvent::NextPage);
            }
        });
    }
}

fn role_choice(ui: &mut egui::Ui, role: &mut RoleChoice) {
    ui.horizontal(|ui| {
        ui.radio_value(role, RoleChoice::Member, RoleChoice::Member.label());
        ui.radio_value(role, RoleChoice::Admin, RoleChoice::Admin.label());
    });
}

fn create(ui: &mut egui::Ui, form: &mut CreateForm, events: &mut Vec<Event>) {
    super::heading(ui, "New account");
    ui.label("Name");
    let name = ui.add(egui::TextEdit::singleline(&mut form.username).hint_text("ana").desired_width(f32::INFINITY));
    let enter = entered(&name, ui);
    ui.add_space(8.0);
    ui.label("Role");
    role_choice(ui, &mut form.role);
    ui.add_space(6.0);
    ui.add(
        egui::Label::new(
            RichText::new(
                "The account starts without a password: you get an invite code to give them. Set a quota afterwards, on the account's page.",
            )
            .small()
            .weak(),
        )
        .wrap(),
    );
    ui.add_space(10.0);
    if let Some(error) = &form.error {
        super::error_line(ui, error);
        ui.add_space(6.0);
    }
    if super::wide_button(ui, true, "Continue") || enter {
        send(events, AdminEvent::SubmitCreate);
    }
    ui.add_space(4.0);
    if ui.button("Cancel").clicked() {
        send(events, AdminEvent::BackToList);
    }
}

fn account(ui: &mut egui::Ui, view: &mut UserView, user: Option<&AdminUserInfo>, me: &str, now_ms: i64, events: &mut Vec<Event>) {
    super::heading(ui, &view.username);
    if ui.button("Back to the accounts").clicked() {
        send(events, AdminEvent::BackToList);
    }
    ui.add_space(8.0);
    let Some(user) = user else {
        super::error_line(ui, "This account isn't in the list any more.");
        return;
    };
    ui.label(RichText::new(describe(user)).small().weak());
    ui.add_space(10.0);
    let is_me = user.username == me;

    ui.label(RichText::new("Role and quota").strong());
    role_choice(ui, &mut view.role);
    ui.horizontal(|ui| {
        ui.label("Quota (GiB)");
        ui.add(egui::TextEdit::singleline(&mut view.quota).hint_text("none").desired_width(100.0));
    });
    if let Some(error) = &view.error {
        super::error_line(ui, error);
    }
    if ui.button("Save changes").clicked() {
        send(events, AdminEvent::SubmitEdit);
    }
    ui.add_space(12.0);

    ui.label(RichText::new("Account").strong());
    let id = user.id;
    let username = user.username.clone();
    ui.horizontal_wrapped(|ui| {
        if is_me {
            ui.label(RichText::new("You can't disable or remove your own account here.").small().weak());
        } else if user.disabled_ms.is_some() {
            if ui.button("Enable").clicked() {
                send(events, AdminEvent::Ask(Action::Enable { id, username: username.clone() }));
            }
        } else if ui.button("Disable").clicked() {
            send(events, AdminEvent::Ask(Action::Disable { id, username: username.clone() }));
        }
        if !is_me && ui.button("Remove").clicked() {
            send(events, AdminEvent::Ask(Action::Remove { id, username: username.clone() }));
        }
    });
    ui.add_space(12.0);

    ui.label(RichText::new("Invites and codes").strong());
    ui.horizontal_wrapped(|ui| {
        if ui.button("New invite").on_hover_text("For an account that has no password yet").clicked() {
            send(events, AdminEvent::Ask(Action::IssueInvite { id, username: username.clone() }));
        }
        if ui.button("Password reset code").on_hover_text("Good for an hour; using it signs all their computers out").clicked() {
            send(events, AdminEvent::Ask(Action::IssueReset { id, username: username.clone() }));
        }
        if user.invite_pending && ui.button("Revoke open code").clicked() {
            send(events, AdminEvent::Ask(Action::RevokeInvite { id, username: username.clone() }));
        }
    });
    ui.add_space(12.0);

    ui.horizontal(|ui| {
        ui.label(RichText::new("Computers").strong());
        if ui.small_button("Refresh").clicked() {
            send(events, AdminEvent::RefreshDevices);
        }
    });
    if let Some(error) = &view.devices_error {
        super::error_line(ui, error);
    }
    let Some(devices) = &view.devices else {
        if view.devices_error.is_none() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Asking the server…");
            });
        }
        return;
    };
    if devices.iter().all(|d| d.revoked) {
        ui.label(RichText::new("No computers signed in.").weak());
    }
    egui::Grid::new("admin-devices").num_columns(3).striped(true).spacing([12.0, 6.0]).show(ui, |ui| {
        for device in devices.iter().filter(|d| !d.revoked) {
            ui.label(&device.name);
            ui.label(RichText::new(format!("last seen {}", super::ago(now_ms, device.last_used_ms))).weak());
            if ui.button("Revoke").on_hover_text("It can't sync any more; its files stay there").clicked() {
                let action = Action::RevokeDevice { id, username: username.clone(), device: device.id, device_name: device.name.clone() };
                send(events, AdminEvent::Ask(action));
            }
            ui.end_row();
        }
    });
}

/// The password a change asks for.
fn confirmation(ui: &mut egui::Ui, dialog: &mut Dialog, events: &mut Vec<Event>) {
    super::heading(ui, &dialog.action.title());
    ui.add(egui::Label::new(dialog.action.explain()).wrap());
    ui.add_space(10.0);
    ui.label("Your password");
    let field = ui.add_enabled(
        !dialog.busy,
        egui::TextEdit::singleline(&mut dialog.confirmation.password)
            .password(true)
            .hint_text("your own current password")
            .desired_width(f32::INFINITY),
    );
    let enter = entered(&field, ui);
    ui.add_space(10.0);
    if let Some(error) = &dialog.error {
        super::error_line(ui, error);
        ui.add_space(6.0);
    }
    if dialog.busy {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Asking the server…");
        });
        return;
    }
    if super::wide_button(ui, true, dialog.action.button()) || enter {
        send(events, AdminEvent::Confirm);
    }
    ui.add_space(4.0);
    if ui.button("Cancel").clicked() {
        send(events, AdminEvent::CancelDialog);
    }
}

/// How long until `at_ms`, from `now_ms`: "in 7 days", "in 59 min".
fn until(now_ms: i64, at_ms: i64) -> String {
    let secs = at_ms.saturating_sub(now_ms).max(0) / 1000;
    match secs {
        0..60 => "in under a minute".into(),
        60..3_600 => format!("in {} min", secs / 60),
        3_600..86_400 => format!("in {} h", secs / 3_600),
        _ => format!("in {} days", secs / 86_400),
    }
}

/// A code the server made: shown once, to copy and hand over.
fn shown_code(ui: &mut egui::Ui, code: &ShownCode, now_ms: i64, events: &mut Vec<Event>) {
    let title = match code.kind {
        CodeKind::Invite => format!("Invite for {}", code.username),
        CodeKind::PasswordReset => format!("Password reset code for {}", code.username),
    };
    super::heading(ui, &title);
    super::error_line(ui, "Note it now: it is shown once and can't be shown again.");
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.add(egui::Label::new(RichText::new(&code.code).monospace().size(18.0).strong()).selectable(true));
        if ui.small_button("Copy").clicked() {
            ui.ctx().copy_text(code.code.clone());
        }
    });
    if let Some(url) = &code.join_url {
        ui.add_space(8.0);
        ui.label("Or the whole join string:");
        ui.horizontal(|ui| {
            ui.add(egui::Label::new(RichText::new(url).monospace()).selectable(true).wrap());
            if ui.small_button("Copy").clicked() {
                ui.ctx().copy_text(url.clone());
            }
        });
    }
    ui.add_space(8.0);
    ui.label(RichText::new(format!("Stops working {}.", until(now_ms, code.expires_ms))).weak());
    ui.add_space(14.0);
    if super::wide_button(ui, true, "Done") {
        send(events, AdminEvent::DismissCode);
    }
}
