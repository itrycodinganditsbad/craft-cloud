//! The admin pages: the users (search, pages), a new account, one account (role, quota, disable, invites, devices),
//! the password a change asks for, and the code a change made, shown once.

use egui::RichText;

use super::kit::{self, Tokens, Tone};
use crate::admin::{
    Action, AdminEvent, AdminScreen, Basis, CodeKind, CreateForm, Dialog, RoleChoice, Room, Section, ShownCode, StorageBlock, StoreRoom, UserList,
    UserView, View, Warning, format_bytes,
};
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
        View::List => landing(
            ui,
            admin.section,
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

/// A framed panel across the available width: an optional header strip (`title` in a section label, `right` at its right
/// end) over a hairline, then the body inside `pad` points of margin.
fn panel<R>(ui: &mut egui::Ui, title: Option<&str>, right: impl FnOnce(&mut egui::Ui), pad: i8, body: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let t = Tokens::get(ui.ctx());
    egui::Frame::new()
        .stroke(t.hairline())
        .fill(t.bg)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            if let Some(title) = title {
                egui::Frame::new().inner_margin(egui::Margin::symmetric(12, 6)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        kit::section_label(ui, title);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), right);
                    });
                });
                kit::hairline(ui);
            }
            egui::Frame::new()
                .inner_margin(egui::Margin::same(pad))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.spacing_mut().item_spacing.y = 6.0;
                    body(ui)
                })
                .inner
        })
        .inner
}

/// A bar of `width` points: `room`'s share filled, with a tick at the warning share. No room: an empty track.
fn bar(ui: &mut egui::Ui, room: Option<Room>, width: f32) {
    let t = Tokens::get(ui.ctx());
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width.max(20.0), 6.0), egui::Sense::hover());
    let label = room.map_or_else(|| "no size".to_owned(), |room| format!("{} % used", room.percent()));
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::ProgressIndicator, true, &label));
    let painter = ui.painter();
    painter.rect_filled(rect, egui::CornerRadius::ZERO, t.line);
    if let Some(room) = room {
        let fill = if room.full() {
            t.bad
        } else if room.near() {
            t.warn
        } else {
            t.accent
        };
        let filled = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * room.fraction(), rect.height()));
        painter.rect_filled(filled, egui::CornerRadius::ZERO, fill);
        let x = rect.left() + rect.width() * (crate::admin::WARN_PERCENT as f32 / 100.0);
        painter.vline(x, rect.y_range(), egui::Stroke::new(2.0, t.warn));
    }
}

/// A missing number.
fn dash(ui: &mut egui::Ui) {
    let color = Tokens::get(ui.ctx()).faint;
    ui.label(RichText::new("–").color(color));
}

/// A big number with its unit after it: "1.3 GiB".
fn big(ui: &mut egui::Ui, text: &str, size: f32) {
    let t = Tokens::get(ui.ctx());
    let (number, unit) = text.split_once(' ').unwrap_or((text, ""));
    ui.spacing_mut().item_spacing.x = 4.0;
    ui.add(egui::Label::new(kit::headline_text(number, size).color(t.ink_strong)).selectable(false));
    if !unit.is_empty() {
        ui.add(egui::Label::new(kit::headline_text(unit, size * 0.55).color(t.ink_strong)).selectable(false));
    }
}

fn small_weak(ui: &mut egui::Ui, text: impl Into<String>) {
    ui.add(egui::Label::new(RichText::new(text.into()).small().weak()).wrap());
}

/// The tag for how full a room is: "OK", or the share used from the warning share on.
fn room_state(ui: &mut egui::Ui, room: Option<Room>) {
    match room {
        None => dash(ui),
        Some(room) if room.full() => {
            kit::tag(ui, &format!("{} %", room.percent()), Tone::Bad);
        }
        Some(room) if room.near() => {
            kit::tag(ui, &format!("{} %", room.percent()), Tone::Warn);
        }
        Some(_) => {
            kit::tag(ui, "OK", Tone::Good);
        }
    }
}

/// The tags of a store's state: "OK", or what is near and how far.
fn warning_tags(ui: &mut egui::Ui, warnings: &[Warning]) {
    ui.horizontal(|ui| {
        if warnings.is_empty() {
            kit::tag(ui, "OK", Tone::Good);
        }
        for warning in warnings {
            kit::tag(ui, &warning.text(), if warning.room.full() { Tone::Bad } else { Tone::Warn });
        }
    });
}

/// The admin landing page: the Accounts, Storage and Audit log tabs over the part that is shown.
fn landing(
    ui: &mut egui::Ui,
    section: Section,
    server: Option<&craft_cloud_app::AdminServerInfo>,
    storage: StorageState<'_>,
    list: &mut UserList,
    me: &str,
    events: &mut Vec<Event>,
) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        if kit::tab(ui, "Accounts", section == Section::Accounts).clicked() {
            send(events, AdminEvent::Show(Section::Accounts));
        }
        if kit::tab(ui, "Storage", section == Section::Storage).clicked() {
            send(events, AdminEvent::Show(Section::Storage));
        }
        if kit::tab(ui, "Audit log", false).on_hover_text("Sign-ins, changes and what admins did").clicked() {
            events.push(Event::Audit(crate::audit::AuditMsg::OpenAudit(None)));
        }
        ui.add_space(12.0);
        if ui.add_enabled(!list.loading, kit::Button::new("Refresh")).clicked() {
            send(events, AdminEvent::Reload);
        }
        if let Some(info) = server {
            ui.add_space(8.0);
            kit::section_label(
                ui,
                &format!(
                    "Server {} · {} accounts · {} computers · {} · {}",
                    info.version, info.users, info.devices_active, info.storage_kind, info.database_kind
                ),
            );
        }
    });
    ui.add_space(12.0);
    match section {
        Section::Accounts => accounts(ui, storage, server, list, me, events),
        Section::Storage => storage_page(ui, storage),
    }
}

/// One of the cells with the counts: a label, a big value and a note.
fn stat(ui: &mut egui::Ui, label: &str, value: &str, note: &str) {
    kit::card(ui, None, |ui| {
        kit::section_label(ui, label);
        ui.horizontal(|ui| big(ui, value, 26.0));
        small_weak(ui, note);
    });
}

/// The cells with the counts. The storage call has all four; without it (it failed, or is still asking) the server's own
/// counters give what they know: what is stored and how many accounts.
fn stat_cells(ui: &mut egui::Ui, storage: Option<&craft_cloud_app::AdminStorageOverview>, server: Option<&craft_cloud_app::AdminServerInfo>) {
    let stored = storage.map(|s| s.stored_bytes).or(server.map(|s| s.stored_bytes));
    let accounts = storage.map(|s| s.accounts).or(server.map(|s| s.users));
    let (Some(stored), Some(accounts)) = (stored, accounts) else { return };
    let accounts_note = match storage {
        Some(storage) => {
            let quota = storage.default_quota_bytes.map_or_else(|| "none".to_owned(), format_bytes);
            format!("default quota: {quota} · {} with their own", storage.accounts_with_override)
        }
        None => "on this server".to_owned(),
    };
    ui.columns(if storage.is_some() { 4 } else { 2 }, |cols| {
        let mut cells = cols.iter_mut();
        if let Some(cell) = cells.next() {
            stat(cell, "Stored", &format_bytes(stored), "files of all accounts");
        }
        if let Some(cell) = cells.next() {
            stat(cell, "Total accounts", &accounts.to_string(), &accounts_note);
        }
        if let Some(storage) = storage {
            if let Some(cell) = cells.next() {
                stat(cell, "Near quota", &storage.accounts_near_quota.to_string(), "at 90 % of their quota or more");
            }
            if let Some(cell) = cells.next() {
                stat(cell, "Over quota", &storage.accounts_over_quota.to_string(), "at or past their quota: uploads refused");
            }
        }
    });
}

/// What the storage call is doing or said; `true` when there is an overview to draw.
fn storage_status(ui: &mut egui::Ui, state: &StorageState<'_>) -> bool {
    if let Some(error) = state.error {
        super::error_line(ui, error);
        return false;
    }
    if state.overview.is_none() {
        if state.loading {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Asking the server…");
            });
        }
        return false;
    }
    true
}

/// A table in an area of its own that scrolls sideways, so a narrow window never clips it.
fn scroll_x<R>(ui: &mut egui::Ui, id: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::ScrollArea::horizontal().id_salt(id).auto_shrink([false, true]).show(ui, add).inner
}

fn accounts(
    ui: &mut egui::Ui,
    storage: StorageState<'_>,
    server: Option<&craft_cloud_app::AdminServerInfo>,
    list: &mut UserList,
    me: &str,
    events: &mut Vec<Event>,
) {
    storage_status(ui, &storage);
    if storage.overview.is_some() || server.is_some() {
        stat_cells(ui, storage.overview, server);
        ui.add_space(12.0);
    }
    ui.horizontal_wrapped(|ui| {
        kit::section_label(ui, "Who uses how much");
        ui.add_space(12.0);
        let field = ui.add(kit::text_edit(&mut list.search).hint_text("Account name").desired_width(180.0));
        if ui.add(kit::Button::new("Search")).clicked() || entered(&field, ui) {
            send(events, AdminEvent::Search);
        }
        if !list.applied.is_empty() && ui.add(kit::Button::new("Clear")).clicked() {
            send(events, AdminEvent::ClearSearch);
        }
        if ui.add(kit::Button::new("New account").primary()).clicked() {
            send(events, AdminEvent::NewUser);
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
    if !list.users.is_empty() {
        let users = list.users.clone();
        panel(
            ui,
            None,
            |_| {},
            0,
            |ui| {
                scroll_x(ui, "admin-accounts-scroll", |ui| accounts_table(ui, &users, me, events));
            },
        );
    }
    if list.has_previous() || list.next.is_some() {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.add_enabled(list.has_previous() && !list.loading, kit::Button::new("Previous")).clicked() {
                send(events, AdminEvent::PreviousPage);
            }
            ui.label(format!("Page {}", list.page()));
            if ui.add_enabled(list.next.is_some() && !list.loading, kit::Button::new("Next")).clicked() {
                send(events, AdminEvent::NextPage);
            }
        });
    }
}

/// The state of an account as tags: each that applies (a disabled account can have an invite open too), else "Active".
fn account_state(ui: &mut egui::Ui, user: &AdminUserInfo) {
    ui.horizontal(|ui| {
        if user.disabled_ms.is_some() {
            kit::tag(ui, "Disabled", Tone::Bad);
        }
        if user.invite_pending {
            kit::tag(ui, "Invite open", Tone::Warn);
        }
        if user.disabled_ms.is_none() && !user.invite_pending {
            kit::tag(ui, "Active", Tone::Good);
        }
    });
}

fn role_tag(ui: &mut egui::Ui, user: &AdminUserInfo) {
    if user.role == "admin" {
        kit::tag(ui, "Admin", Tone::Warn);
    } else {
        kit::section_label(ui, "Member");
    }
}

fn accounts_table(ui: &mut egui::Ui, users: &[AdminUserInfo], me: &str, events: &mut Vec<Event>) {
    egui::Frame::new().inner_margin(egui::Margin::symmetric(12, 6)).show(ui, |ui| {
        egui::Grid::new("admin-accounts").num_columns(7).striped(true).spacing([20.0, 10.0]).min_col_width(60.0).show(ui, |ui| {
            for head in ["Account", "Role", "Computers", "Stored / quota", "Two-factor", "State", ""] {
                kit::section_label(ui, head);
            }
            ui.end_row();
            for user in users {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&user.username).strong());
                    if user.username == me {
                        ui.label(RichText::new("(you)").weak());
                    }
                });
                role_tag(ui, user);
                ui.label(user.devices_active.to_string());
                ui.vertical(|ui| {
                    ui.set_min_width(150.0);
                    match user.quota_bytes {
                        Some(quota) => {
                            ui.label(format!("{} of {}", format_bytes(user.stored_bytes), format_bytes(quota)));
                            bar(ui, Some(Room { used: user.stored_bytes, total: quota }), 150.0);
                        }
                        None => {
                            ui.label(format!("{} · no limit", format_bytes(user.stored_bytes)));
                        }
                    }
                });
                if user.two_factor_enabled {
                    kit::tag(ui, "On", Tone::Good);
                } else {
                    ui.label(RichText::new("off").weak());
                }
                account_state(ui, user);
                if ui.add(kit::Button::new("Manage")).clicked() {
                    send(events, AdminEvent::OpenUser(user.id));
                }
                ui.end_row();
            }
        });
    });
}

/// The Storage tab: what the server has room for, the biggest accounts and how complete the estimates are.
fn storage_page(ui: &mut egui::Ui, state: StorageState<'_>) {
    if !storage_status(ui, &state) {
        return;
    }
    let Some(storage) = state.overview else { return };
    let block = StorageBlock::of(storage);
    server_storage(ui, &block);
    if !storage.top_accounts.is_empty() {
        ui.add_space(12.0);
        kit::section_label(ui, "Top accounts");
        ui.add_space(4.0);
        panel(
            ui,
            None,
            |_| {},
            0,
            |ui| {
                scroll_x(ui, "admin-top-scroll", |ui| {
                    egui::Frame::new().inner_margin(egui::Margin::symmetric(12, 6)).show(ui, |ui| {
                        egui::Grid::new("admin-storage-top").num_columns(3).striped(true).spacing([24.0, 8.0]).show(ui, |ui| {
                            for head in ["Account", "Stored", "Effective quota"] {
                                kit::section_label(ui, head);
                            }
                            ui.end_row();
                            for account in &storage.top_accounts {
                                ui.label(&account.username);
                                ui.label(format_bytes(account.stored_bytes));
                                ui.label(account.quota_bytes.map_or_else(|| "none".to_owned(), format_bytes));
                                ui.end_row();
                            }
                        });
                    });
                });
            },
        );
    }
    ui.add_space(12.0);
    small_weak(
        ui,
        format!(
            "Manifest index: {} complete · {} building · {} not indexed.",
            storage.manifest_index.complete, storage.manifest_index.building, storage.manifest_index.not_indexed
        ),
    );
    if storage.manifest_index.building > 0 || storage.manifest_index.not_indexed > 0 {
        small_weak(ui, "Storage estimates are incomplete; run `craft-cloud-server index build`.");
    }
}

/// "01 / Server storage": all storage as one number and a bar, and a row for the server's disk and one for the store.
///
/// A disk store is measured by its filesystem (real bytes). What the accounts have stored counts per account, so it can pass
/// the disk's size; it is only the measure for a store with a capacity and no disk to read (S3), and then it is said to be an
/// upper bound. A set capacity on a disk is a second measure with a warning of its own.
fn server_storage(ui: &mut egui::Ui, block: &StorageBlock) {
    let near = block.near_count();
    let title = kit::numbered(1, "Server storage");
    let warnings = block.store.warnings();
    panel(
        ui,
        Some(&title),
        |ui| {
            if near > 0 {
                kit::tag(ui, "Near capacity", Tone::Warn);
            }
        },
        0,
        |ui| {
            egui::Frame::new().inner_margin(egui::Margin::same(12)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                kit::section_label(ui, "All storage");
                ui.horizontal_wrapped(|ui| {
                    match (block.basis, block.all) {
                        (Basis::Disk, Some(room)) => {
                            big(ui, &format_bytes(room.used), 30.0);
                            ui.add_space(4.0);
                            ui.label(format!("used of {} · {} free", format_bytes(room.total), format_bytes(room.total.saturating_sub(room.used))));
                        }
                        (Basis::Accounts, Some(room)) => {
                            big(ui, &format_bytes(room.used), 30.0);
                            ui.add_space(4.0);
                            ui.label(format!(
                                "stored by the accounts, of {} · {} left (counted per account: an upper bound)",
                                format_bytes(room.total),
                                format_bytes(room.total.saturating_sub(room.used))
                            ));
                        }
                        _ => {
                            big(ui, &format_bytes(block.stored), 30.0);
                            ui.add_space(4.0);
                            ui.label(RichText::new("stored by the accounts · no size to measure it against").weak());
                        }
                    }
                    if block.all.is_some() {
                        ui.add_space(4.0);
                        warning_tags(ui, &warnings);
                    }
                });
                bar(ui, block.all, ui.available_width());
                if let (Basis::Disk, Some(capacity)) = (block.basis, block.capacity) {
                    small_weak(
                        ui,
                        format!(
                            "capacity set: {} · the accounts' files total {}, counted per account",
                            format_bytes(capacity),
                            format_bytes(block.stored)
                        ),
                    );
                }
            });
            kit::hairline(ui);
            scroll_x(ui, "admin-room-scroll", |ui| {
                egui::Frame::new().inner_margin(egui::Margin::symmetric(12, 6)).show(ui, |ui| {
                    egui::Grid::new("admin-room").num_columns(5).spacing([20.0, 10.0]).min_col_width(60.0).show(ui, |ui| {
                        for head in ["Store", "Kind", "Used of capacity", "", "State"] {
                            kit::section_label(ui, head);
                        }
                        ui.end_row();
                        server_disk_row(ui, block);
                        store_row(ui, &block.store, &warnings);
                    });
                });
            });
            kit::hairline(ui);
            egui::Frame::new().inner_margin(egui::Margin::same(12)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 4.0;
                match &block.store {
                    StoreRoom::NoCapacity { kind } if kind == "s3" => small_weak(
                        ui,
                        "Capacity not set: add capacity to the [storage] section of the server's configuration to see how full the store is. \
                         An S3 store has no size of its own.",
                    ),
                    StoreRoom::NoCapacity { .. } => small_weak(
                        ui,
                        "Capacity not set: add capacity to the [storage] section of the server's configuration to see how full the store is.",
                    ),
                    StoreRoom::NoNumbers { .. } => small_weak(ui, "The server sent no size for the disk that holds the store."),
                    StoreRoom::Unknown => small_weak(ui, "This server doesn't report its room: it may be older, or the disk could not be read."),
                    StoreRoom::Filesystem { .. } | StoreRoom::Capacity { .. } => {}
                }
                small_weak(
                    ui,
                    format!(
                        "A disk reports its own size; S3 shows the capacity set in the server's configuration. The yellow mark and the percentage \
                         warn at {} %. Accounts' files are counted per account, so they can add up to more than the store holds.",
                        crate::admin::WARN_PERCENT
                    ),
                );
            });
        },
    );
}

/// A row's name with a muted line under it.
fn name_cell(ui: &mut egui::Ui, name: &str, note: &str) {
    ui.vertical(|ui| {
        ui.set_min_width(170.0);
        ui.label(RichText::new(name).strong());
        small_weak(ui, note);
    });
}

/// A row's bar, its numbers and its state (`state` draws the tags).
fn room_cells(ui: &mut egui::Ui, room: Option<Room>, state: impl FnOnce(&mut egui::Ui)) {
    bar(ui, room, 150.0);
    match room {
        Some(room) => ui.label(format!("{} / {}", format_bytes(room.used), format_bytes(room.total))),
        None => ui.label(RichText::new("–").weak()),
    };
    state(ui);
}

fn server_disk_row(ui: &mut egui::Ui, block: &StorageBlock) {
    let note = match block.database_bytes {
        Some(bytes) => format!("database {}", format_bytes(bytes)),
        None => "database size not reported".to_owned(),
    };
    name_cell(ui, "Server disk", &note);
    kit::tag(ui, "Disk", Tone::Neutral);
    room_cells(ui, block.server_disk, |ui| room_state(ui, block.server_disk));
    ui.end_row();
}

fn store_row(ui: &mut egui::Ui, store: &StoreRoom, warnings: &[Warning]) {
    let note = match store {
        StoreRoom::Filesystem { capacity: Some(set), .. } => format!("filesystem of the store · capacity set: {}", format_bytes(set.total)),
        StoreRoom::Filesystem { capacity: None, .. } => "filesystem of the store".to_owned(),
        StoreRoom::Capacity { room, .. } => format!("accounts' total against the capacity set: {}", format_bytes(room.total)),
        StoreRoom::NoCapacity { .. } => "capacity not set".to_owned(),
        StoreRoom::NoNumbers { .. } => "no size reported".to_owned(),
        StoreRoom::Unknown => "not reported".to_owned(),
    };
    name_cell(ui, "Blob store", &note);
    match store.kind() {
        Some(kind) => {
            kit::tag(ui, kind, if kind == "s3" { Tone::Accent } else { Tone::Neutral });
        }
        None => dash(ui),
    }
    let room = store.room();
    room_cells(ui, room, |ui| match room {
        Some(_) => warning_tags(ui, warnings),
        None => dash(ui),
    });
    ui.end_row();
}

/// Member or Admin as two buttons, the chosen one inverted.
fn role_choice(ui: &mut egui::Ui, role: &mut RoleChoice) {
    ui.horizontal(|ui| {
        for choice in [RoleChoice::Member, RoleChoice::Admin] {
            if ui.add(kit::Button::new(choice.label()).selected(*role == choice).min_size(egui::vec2(96.0, 0.0))).clicked() {
                *role = choice;
            }
        }
    });
}

/// The path above a page's headline: "Admin / Accounts / ana".
fn breadcrumb(ui: &mut egui::Ui, last: &str) {
    kit::section_label(ui, &format!("Admin / Accounts / {last}"));
    ui.add_space(4.0);
}

fn big_headline(ui: &mut egui::Ui, text: &str) {
    let t = Tokens::get(ui.ctx());
    ui.add(egui::Label::new(kit::headline_text(text, 28.0).color(t.ink_strong)).selectable(false));
}

fn create(ui: &mut egui::Ui, form: &mut CreateForm, events: &mut Vec<Event>) {
    breadcrumb(ui, "New");
    big_headline(ui, "New account");
    ui.add_space(12.0);
    ui.scope(|ui| {
        ui.set_max_width(560.0);
        panel(
            ui,
            Some(&kit::numbered(1, "Who")),
            |_| {},
            12,
            |ui| {
                kit::section_label(ui, "Name");
                let name = ui.add(kit::text_edit(&mut form.username).hint_text("ana").desired_width(f32::INFINITY));
                let enter = entered(&name, ui);
                ui.add_space(4.0);
                kit::section_label(ui, "Role");
                role_choice(ui, &mut form.role);
                ui.add_space(2.0);
                small_weak(
                    ui,
                    "The account starts without a password: you get an invite code to give them. Set a quota afterwards, on the account's page.",
                );
                ui.add_space(6.0);
                if let Some(error) = &form.error {
                    super::error_line(ui, error);
                }
                if super::wide_button(ui, true, "Continue") || enter {
                    send(events, AdminEvent::SubmitCreate);
                }
                if ui.add(kit::Button::new("Cancel")).clicked() {
                    send(events, AdminEvent::BackToList);
                }
            },
        );
    });
}

fn account(ui: &mut egui::Ui, view: &mut UserView, user: Option<&AdminUserInfo>, me: &str, now_ms: i64, events: &mut Vec<Event>) {
    breadcrumb(ui, &view.username);
    ui.horizontal_wrapped(|ui| {
        big_headline(ui, &view.username);
        if let Some(user) = user {
            ui.add_space(6.0);
            role_tag(ui, user);
            account_state(ui, user);
            if user.two_factor_enabled {
                kit::tag(ui, "Two-factor on", Tone::Good);
            }
        }
    });
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        if ui.add(kit::Button::new("Back to the accounts")).clicked() {
            send(events, AdminEvent::BackToList);
        }
        if let Some(user) = user
            && ui.add(kit::Button::new("Activity of this account")).on_hover_text("The audit log, for this account").clicked()
        {
            events.push(Event::Audit(crate::audit::AuditMsg::OpenAudit(Some((user.id, user.username.clone())))));
        }
    });
    ui.add_space(10.0);
    let Some(user) = user else {
        super::error_line(ui, "This account isn't in the list any more.");
        return;
    };
    let is_me = user.username == me;

    if ui.available_width() >= 640.0 {
        ui.columns(2, |cols| {
            let [left, right] = cols else { return };
            storage_card(left, view, events);
            account_card(right, user, is_me, events);
        });
    } else {
        storage_card(ui, view, events);
        ui.add_space(8.0);
        account_card(ui, user, is_me, events);
    }
    ui.add_space(12.0);
    computers(ui, view, user, now_ms, events);
}

fn storage_card(ui: &mut egui::Ui, view: &mut UserView, events: &mut Vec<Event>) {
    let mut refresh = false;
    panel(
        ui,
        Some("Storage usage"),
        |ui| refresh = ui.add(kit::Button::new("Refresh")).clicked(),
        12,
        |ui| {
            if let Some(error) = &view.usage_error {
                super::error_line(ui, error);
            } else if let Some(usage) = &view.usage {
                ui.label(format!("Stored: {}", format_bytes(usage.stored_bytes)));
                if let Some(quota) = usage.quota_bytes {
                    bar(ui, Some(Room { used: usage.stored_bytes, total: quota }), ui.available_width());
                }
                ui.label(format!("Effective quota: {}", usage.quota_bytes.map_or_else(|| "none".to_owned(), format_bytes)));
                if usage.estimates.index_complete {
                    ui.label(format!("Trash estimate (overlapping): {}", usage.trash_bytes.map_or_else(|| "not available".to_owned(), format_bytes)));
                    ui.label(format!(
                        "History estimate (overlapping): {}",
                        usage.history_bytes.map_or_else(|| "not available".to_owned(), format_bytes)
                    ));
                } else {
                    ui.label("estimates not available yet");
                }
            } else if view.usage_loading {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Asking the server…");
                });
            }
            ui.add_space(6.0);
            kit::hairline(ui);
            ui.add_space(6.0);
            kit::section_label(ui, "Role and quota");
            role_choice(ui, &mut view.role);
            ui.horizontal(|ui| {
                ui.label("Quota (GiB)");
                ui.add(kit::text_edit(&mut view.quota).hint_text("none").desired_width(100.0));
            });
            if let Some(error) = &view.error {
                super::error_line(ui, error);
            }
            if ui.add(kit::Button::new("Save changes")).clicked() {
                send(events, AdminEvent::SubmitEdit);
            }
        },
    );
    if refresh {
        send(events, AdminEvent::RefreshUsage);
    }
}

fn account_card(ui: &mut egui::Ui, user: &AdminUserInfo, is_me: bool, events: &mut Vec<Event>) {
    let id = user.id;
    let username = user.username.clone();
    panel(
        ui,
        Some("Account"),
        |_| {},
        12,
        |ui| {
            ui.horizontal_wrapped(|ui| {
                if is_me {
                    small_weak(ui, "You can't disable or remove your own account here.");
                } else if user.disabled_ms.is_some() {
                    if ui.add(kit::Button::new("Enable")).clicked() {
                        send(events, AdminEvent::Ask(Action::Enable { id, username: username.clone() }));
                    }
                } else if ui.add(kit::Button::new("Disable")).clicked() {
                    send(events, AdminEvent::Ask(Action::Disable { id, username: username.clone() }));
                }
                if !is_me && ui.add(kit::Button::new("Remove").danger()).clicked() {
                    send(events, AdminEvent::Ask(Action::Remove { id, username: username.clone() }));
                }
                if !is_me
                    && user.two_factor_enabled
                    && ui
                        .add(kit::Button::new("Turn two-factor off"))
                        .on_hover_text("They sign in with their password alone; their computers are signed out")
                        .clicked()
                {
                    send(events, AdminEvent::Ask(Action::ResetTwoFactor { id, username: username.clone() }));
                }
            });
            ui.add_space(6.0);
            kit::hairline(ui);
            ui.add_space(6.0);
            kit::section_label(ui, "Invites and codes");
            ui.horizontal_wrapped(|ui| {
                if ui.add(kit::Button::new("New invite")).on_hover_text("For an account that has no password yet").clicked() {
                    send(events, AdminEvent::Ask(Action::IssueInvite { id, username: username.clone() }));
                }
                if ui.add(kit::Button::new("Password reset code")).on_hover_text("Good for an hour; using it signs all their computers out").clicked()
                {
                    send(events, AdminEvent::Ask(Action::IssueReset { id, username: username.clone(), reset_two_factor: false }));
                }
                if user.two_factor_enabled
                    && ui
                        .add(kit::Button::new("Reset code, two-factor off"))
                        .on_hover_text("As a reset code, and using it turns their two-factor sign-in off")
                        .clicked()
                {
                    send(events, AdminEvent::Ask(Action::IssueReset { id, username: username.clone(), reset_two_factor: true }));
                }
                // Always offered: the list says an invite is open (`invite_pending`), but not a password reset code. The server
                // answers `not_found` when none is.
                if ui
                    .add(kit::Button::new("Revoke open code").danger())
                    .on_hover_text("Ends the account's open invite or password reset code")
                    .clicked()
                {
                    send(events, AdminEvent::Ask(Action::RevokeInvite { id, username: username.clone() }));
                }
            });
        },
    );
}

fn computers(ui: &mut egui::Ui, view: &UserView, user: &AdminUserInfo, now_ms: i64, events: &mut Vec<Event>) {
    let (id, username) = (user.id, user.username.clone());
    ui.horizontal(|ui| {
        kit::section_label(ui, "Computers");
        if ui.add(kit::Button::new("Refresh")).clicked() {
            send(events, AdminEvent::RefreshDevices);
        }
    });
    ui.add_space(4.0);
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
        return;
    }
    panel(
        ui,
        None,
        |_| {},
        0,
        |ui| {
            scroll_x(ui, "admin-devices-scroll", |ui| {
                egui::Frame::new().inner_margin(egui::Margin::symmetric(12, 6)).show(ui, |ui| {
                    egui::Grid::new("admin-devices").num_columns(6).striped(true).spacing([20.0, 8.0]).show(ui, |ui| {
                        for head in ["Computer", "Kind", "Platform", "Version", "Last seen", ""] {
                            kit::section_label(ui, head);
                        }
                        ui.end_row();
                        for device in devices.iter().filter(|d| !d.revoked) {
                            ui.label(RichText::new(&device.name).strong());
                            for text in [&device.client_kind, &device.platform, &device.app_version] {
                                if text.is_empty() {
                                    dash(ui);
                                } else {
                                    ui.label(text);
                                }
                            }
                            ui.label(RichText::new(super::ago(now_ms, device.last_used_ms)).weak());
                            if ui.add(kit::Button::new("Revoke").danger()).on_hover_text("It can't sync any more; its files stay there").clicked() {
                                let action =
                                    Action::RevokeDevice { id, username: username.clone(), device: device.id, device_name: device.name.clone() };
                                send(events, AdminEvent::Ask(action));
                            }
                            ui.end_row();
                        }
                    });
                });
            });
        },
    );
}

/// The password a change asks for.
fn confirmation(ui: &mut egui::Ui, dialog: &mut Dialog, events: &mut Vec<Event>) {
    super::heading(ui, &dialog.action.title());
    ui.add(egui::Label::new(dialog.action.explain()).wrap());
    ui.add_space(10.0);
    ui.label("Your password");
    let field = ui.add_enabled(
        !dialog.busy,
        kit::text_edit(&mut dialog.confirmation.password)
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
            kit::text_edit(&mut dialog.confirmation.code)
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
    breadcrumb(ui, &code.username);
    big_headline(ui, &title);
    ui.add_space(12.0);
    ui.scope(|ui| {
        ui.set_max_width(640.0);
        panel(
            ui,
            Some("Code"),
            |_| {},
            12,
            |ui| {
                super::error_line(ui, "Note it now: it is shown once and can't be shown again.");
                let t = Tokens::get(ui.ctx());
                let well = |ui: &mut egui::Ui, text: &str, size: Option<f32>| {
                    egui::Frame::new().fill(t.sunken).stroke(t.hairline()).inner_margin(egui::Margin::same(10)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            let mut shown = RichText::new(text).monospace();
                            if let Some(size) = size {
                                shown = shown.size(size).strong();
                            }
                            ui.add(egui::Label::new(shown).selectable(true).wrap());
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.add(kit::Button::new("Copy")).clicked() {
                                    ui.ctx().copy_text(text.to_owned());
                                }
                            });
                        });
                    });
                };
                well(ui, &code.code, Some(18.0));
                if let Some(url) = &code.join_url {
                    ui.label("Or the whole join string:");
                    well(ui, url, None);
                }
                ui.label(RichText::new(format!("Stops working {}.", until(now_ms, code.expires_ms))).weak());
                ui.add_space(6.0);
                if super::wide_button(ui, true, "Done") {
                    send(events, AdminEvent::DismissCode);
                }
            },
        );
    });
}
