//! The admin pages: the users (search, pages), a new account, one account (role, quota, disable, invites, devices),
//! the password a change asks for, and the code a change made, shown once.

use egui::RichText;

use crate::admin::{Action, AdminEvent, AdminScreen, CodeKind, CreateForm, Dialog, RoleChoice, ShownCode, UserList, UserView, View, format_bytes};
use craft_cloud_app::AdminUserInfo;

use crate::model::Event;

struct StorageState<'a> {
    overview: Option<&'a craft_cloud_app::AdminStorageOverview>,
    loading: bool,
    error: Option<&'a str>,
}

/// The name of the confirmation page's password field, for [`super::security::secret_id`] and the forgetting of its text.
pub(super) const PASSWORD_FIELD: &str = "admin-password";
/// The name of its field for a code (from the authenticator app, or a recovery code).
pub(super) const CODE_FIELD: &str = "admin-code";

pub(super) fn show(ui: &mut egui::Ui, admin: &mut AdminScreen, now_ms: i64, events: &mut Vec<Event>) {
    let me = admin.overview.status.account.clone().unwrap_or_default();
    // A code just made comes first, then the password a change asks for, then the page.
    if let Some(code) = &admin.code {
        return shown_code(ui, code, now_ms, events);
    }
    if let Some(dialog) = &mut admin.dialog {
        return confirmation(ui, dialog, events);
    }
    let server = admin.server.clone();
    match &mut admin.view {
        View::List => users(
            ui,
            server.as_ref(),
            StorageState { overview: admin.storage.as_ref(), loading: admin.storage_loading, error: admin.storage_error.as_deref() },
            &mut admin.list,
            &me,
            events,
        ),
        View::Create(form) => create(ui, form, events),
        View::Audit(view) => super::audit::log(ui, view, now_ms, false, events),
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
    if user.two_factor_enabled {
        parts.push("two-factor on".to_owned());
    }
    parts.join(" · ")
}

fn users(
    ui: &mut egui::Ui,
    server: Option<&craft_cloud_app::AdminServerInfo>,
    storage: StorageState<'_>,
    list: &mut UserList,
    me: &str,
    events: &mut Vec<Event>,
) {
    super::heading(ui, "Administration");
    if let Some(info) = server {
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
    storage_overview(ui, storage);
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if ui.add(super::kit::Button::new("New account")).clicked() {
            send(events, AdminEvent::NewUser);
        }
        if ui.add_enabled(!list.loading, super::kit::Button::new("Refresh")).clicked() {
            send(events, AdminEvent::Reload);
        }
        if ui.add(super::kit::Button::new("Audit log")).on_hover_text("Sign-ins, changes and what admins did").clicked() {
            events.push(Event::Audit(crate::audit::AuditMsg::OpenAudit(None)));
        }
    });
    ui.add_space(8.0);

    ui.horizontal(|ui| {
        let width = (ui.available_width() - 130.0).max(120.0);
        let field = ui.add(super::kit::text_edit(&mut list.search).hint_text("Search accounts").desired_width(width));
        if ui.add(super::kit::Button::new("Search")).clicked() || entered(&field, ui) {
            send(events, AdminEvent::Search);
        }
        if !list.applied.is_empty() && ui.add(super::kit::Button::new("Clear")).clicked() {
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
                    if ui.add(super::kit::Button::new("Manage")).clicked() {
                        send(events, AdminEvent::OpenUser(user.id));
                    }
                });
            });
            ui.separator();
        });
    }
    if list.has_previous() || list.next.is_some() {
        ui.horizontal(|ui| {
            if ui.add_enabled(list.has_previous() && !list.loading, super::kit::Button::new("Previous")).clicked() {
                send(events, AdminEvent::PreviousPage);
            }
            ui.label(format!("Page {}", list.page()));
            if ui.add_enabled(list.next.is_some() && !list.loading, super::kit::Button::new("Next")).clicked() {
                send(events, AdminEvent::NextPage);
            }
        });
    }
}

fn storage_overview(ui: &mut egui::Ui, state: StorageState<'_>) {
    super::kit::section_label(ui, "Storage");
    if let Some(error) = state.error {
        super::error_line(ui, error);
        return;
    }
    let Some(storage) = state.overview else {
        if state.loading {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Asking the server…");
            });
        }
        return;
    };
    ui.label(format!(
        "{} stored · {} accounts · default quota {}",
        format_bytes(storage.stored_bytes),
        storage.accounts,
        storage.default_quota_bytes.map_or_else(|| "none".to_owned(), format_bytes)
    ));
    ui.label(format!(
        "{} near quota · {} over quota · {} with a quota of their own",
        storage.accounts_near_quota, storage.accounts_over_quota, storage.accounts_with_override
    ));
    ui.label(
        RichText::new(format!(
            "Manifest index: {} complete · {} building · {} not indexed",
            storage.manifest_index.complete, storage.manifest_index.building, storage.manifest_index.not_indexed
        ))
        .small()
        .weak(),
    );
    if storage.manifest_index.building > 0 || storage.manifest_index.not_indexed > 0 {
        ui.label(RichText::new("Storage estimates are incomplete; run `craft-cloud-server index build`. ").small().weak());
    }
    if !storage.top_accounts.is_empty() {
        super::kit::section_label(ui, "Top accounts");
        egui::Grid::new("admin-storage-top").num_columns(3).striped(true).spacing([12.0, 4.0]).show(ui, |ui| {
            ui.label(RichText::new("Account").small().weak());
            ui.label(RichText::new("Stored").small().weak());
            ui.label(RichText::new("Effective quota").small().weak());
            ui.end_row();
            for account in &storage.top_accounts {
                ui.label(&account.username);
                ui.label(format_bytes(account.stored_bytes));
                ui.label(account.quota_bytes.map_or_else(|| "none".to_owned(), format_bytes));
                ui.end_row();
            }
        });
    }
    ui.separator();
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
    let name = ui.add(super::kit::text_edit(&mut form.username).hint_text("ana").desired_width(f32::INFINITY));
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
    if ui.add(super::kit::Button::new("Cancel")).clicked() {
        send(events, AdminEvent::BackToList);
    }
}

fn account(ui: &mut egui::Ui, view: &mut UserView, user: Option<&AdminUserInfo>, me: &str, now_ms: i64, events: &mut Vec<Event>) {
    super::heading(ui, &view.username);
    if ui.add(super::kit::Button::new("Back to the accounts")).clicked() {
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

    ui.horizontal(|ui| {
        super::kit::section_label(ui, "Storage usage");
        if ui.add(super::kit::Button::new("Refresh")).clicked() {
            send(events, AdminEvent::RefreshUsage);
        }
    });
    if let Some(error) = &view.usage_error {
        super::error_line(ui, error);
    } else if let Some(usage) = &view.usage {
        ui.label(format!("Stored: {}", format_bytes(usage.stored_bytes)));
        ui.label(format!("Effective quota: {}", usage.quota_bytes.map_or_else(|| "none".to_owned(), format_bytes)));
        if usage.estimates.index_complete {
            ui.label(format!("Trash estimate (overlapping): {}", usage.trash_bytes.map_or_else(|| "not available".to_owned(), format_bytes)));
            ui.label(format!("History estimate (overlapping): {}", usage.history_bytes.map_or_else(|| "not available".to_owned(), format_bytes)));
        } else {
            ui.label("estimates not available yet");
        }
    } else if view.usage_loading {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Asking the server…");
        });
    }
    ui.add_space(8.0);

    if ui.add(super::kit::Button::new("Activity of this account")).on_hover_text("The audit log, for this account").clicked() {
        events.push(Event::Audit(crate::audit::AuditMsg::OpenAudit(Some((user.id, user.username.clone())))));
    }
    ui.add_space(8.0);

    super::kit::section_label(ui, "Role and quota");
    role_choice(ui, &mut view.role);
    ui.horizontal(|ui| {
        ui.label("Quota (GiB)");
        ui.add(super::kit::text_edit(&mut view.quota).hint_text("none").desired_width(100.0));
    });
    if let Some(error) = &view.error {
        super::error_line(ui, error);
    }
    if ui.add(super::kit::Button::new("Save changes")).clicked() {
        send(events, AdminEvent::SubmitEdit);
    }
    ui.add_space(12.0);

    super::kit::section_label(ui, "Account");
    let id = user.id;
    let username = user.username.clone();
    ui.horizontal_wrapped(|ui| {
        if is_me {
            ui.label(RichText::new("You can't disable or remove your own account here.").small().weak());
        } else if user.disabled_ms.is_some() {
            if ui.add(super::kit::Button::new("Enable")).clicked() {
                send(events, AdminEvent::Ask(Action::Enable { id, username: username.clone() }));
            }
        } else if ui.add(super::kit::Button::new("Disable")).clicked() {
            send(events, AdminEvent::Ask(Action::Disable { id, username: username.clone() }));
        }
        if !is_me && ui.add(super::kit::Button::new("Remove").danger()).clicked() {
            send(events, AdminEvent::Ask(Action::Remove { id, username: username.clone() }));
        }
        if !is_me
            && user.two_factor_enabled
            && ui
                .add(super::kit::Button::new("Turn two-factor off"))
                .on_hover_text("They sign in with their password alone; their computers are signed out")
                .clicked()
        {
            send(events, AdminEvent::Ask(Action::ResetTwoFactor { id, username: username.clone() }));
        }
    });
    ui.add_space(12.0);

    super::kit::section_label(ui, "Invites and codes");
    ui.horizontal_wrapped(|ui| {
        if ui.add(super::kit::Button::new("New invite")).on_hover_text("For an account that has no password yet").clicked() {
            send(events, AdminEvent::Ask(Action::IssueInvite { id, username: username.clone() }));
        }
        if ui.add(super::kit::Button::new("Password reset code")).on_hover_text("Good for an hour; using it signs all their computers out").clicked()
        {
            send(events, AdminEvent::Ask(Action::IssueReset { id, username: username.clone(), reset_two_factor: false }));
        }
        if user.two_factor_enabled
            && ui
                .add(super::kit::Button::new("Reset code, two-factor off"))
                .on_hover_text("As a reset code, and using it turns their two-factor sign-in off")
                .clicked()
        {
            send(events, AdminEvent::Ask(Action::IssueReset { id, username: username.clone(), reset_two_factor: true }));
        }
        // Always offered: the list says an invite is open (`invite_pending`), but not a password reset code. The server
        // answers `not_found` when none is.
        if ui
            .add(super::kit::Button::new("Revoke open code").danger())
            .on_hover_text("Ends the account's open invite or password reset code")
            .clicked()
        {
            send(events, AdminEvent::Ask(Action::RevokeInvite { id, username: username.clone() }));
        }
    });
    ui.add_space(12.0);

    ui.horizontal(|ui| {
        super::kit::section_label(ui, "Computers");
        if ui.add(super::kit::Button::new("Refresh")).clicked() {
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
            if ui.add(super::kit::Button::new("Revoke").danger()).on_hover_text("It can't sync any more; its files stay there").clicked() {
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
        super::kit::text_edit(&mut dialog.confirmation.password)
            .id(super::security::secret_id(PASSWORD_FIELD))
            .password(true)
            .hint_text("your own current password")
            .desired_width(f32::INFINITY),
    );
    let mut enter = entered(&field, ui);
    if dialog.needs_code {
        ui.add_space(6.0);
        ui.label("Code from your authenticator app, or a recovery code");
        let code = ui.add_enabled(
            !dialog.busy,
            super::kit::text_edit(&mut dialog.confirmation.code)
                .id(super::security::secret_id(CODE_FIELD))
                .password(true)
                .hint_text("123456")
                .desired_width(f32::INFINITY),
        );
        enter |= entered(&code, ui);
    }
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
    if ui.add(super::kit::Button::new("Cancel")).clicked() {
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
        if ui.add(super::kit::Button::new("Copy")).clicked() {
            ui.ctx().copy_text(code.code.clone());
        }
    });
    if let Some(url) = &code.join_url {
        ui.add_space(8.0);
        ui.label("Or the whole join string:");
        ui.horizontal(|ui| {
            ui.add(egui::Label::new(RichText::new(url).monospace()).selectable(true).wrap());
            if ui.add(super::kit::Button::new("Copy")).clicked() {
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
