//! The admin pages: the users (search, pages), a new account, one account (role, quota, disable, invites, devices),
//! the password a change asks for, and the code a change made, shown once. The Accounts, Storage and Audit log parts
//! share one switcher (a segmented control); the audit log opens inside these pages with the switcher over it.

use egui::{Align, Layout, RichText};

use super::kit::{self, Column, JoinedCell, Part, Segment, Tokens, Tone};
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

/// The height of a row of the accounts table, and of the storage block's table.
const ROW_H: f32 = 64.0;
/// The widest the panels of the confirmation and the code pages get.
const NARROW_PANEL: f32 = 560.0;
/// Below this width two panels stack instead of standing side by side.
const PANELS_SIDE_BY_SIDE: f32 = 640.0;

pub(super) fn show(ui: &mut egui::Ui, admin: &mut AdminScreen, now_ms: i64, events: &mut Vec<Event>) {
    let me = admin.overview.status.account.clone().unwrap_or_default();
    // A code just made comes first, then the password a change asks for, then the page.
    if let Some(code) = &admin.code {
        let user = admin.list.users.iter().find(|u| u.username == code.username);
        let server = admin.overview.status.server.as_deref().map(super::host);
        return shown_code(ui, code, user, server, now_ms, events);
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
        View::Audit(view) => {
            switcher(ui, 2, events);
            ui.add_space(12.0);
            super::audit::log(ui, view, now_ms, true, events);
        }
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
/// end, both centred on a row as high as a button) over a hairline, then the body. The body has no margin and no spacing
/// of its own: it is built from [`section`]s and [`kv`] rows. The frame is at least `min_h` high; the second value is its
/// height.
fn panel_h<R>(
    ui: &mut egui::Ui,
    min_h: f32,
    title: Option<&str>,
    right: impl FnOnce(&mut egui::Ui),
    body: impl FnOnce(&mut egui::Ui) -> R,
) -> (R, f32) {
    let t = Tokens::get(ui.ctx());
    let out = egui::Frame::new().stroke(t.hairline()).fill(t.bg).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.set_min_height((min_h - 2.0).max(0.0));
        ui.spacing_mut().item_spacing.y = 0.0;
        if let Some(title) = title {
            egui::Frame::new().inner_margin(egui::Margin::symmetric(16, 2)).show(ui, |ui| {
                kit::label_row(ui, title, right);
            });
            kit::hairline(ui);
        }
        body(ui)
    });
    (out.inner, out.response.rect.height())
}

fn panel<R>(ui: &mut egui::Ui, title: Option<&str>, right: impl FnOnce(&mut egui::Ui), body: impl FnOnce(&mut egui::Ui) -> R) -> R {
    panel_h(ui, 0.0, title, right, body).0
}

/// A block of a panel's body: 16 points of margin, 10 between its lines.
fn section<R>(ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(16, 16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 10.0;
            body(ui)
        })
        .inner
}

/// A row of a panel: a muted `label` on the left, the value `add` draws on the right, a hairline under it.
fn kv(ui: &mut egui::Ui, label: &str, value: impl FnOnce(&mut egui::Ui)) {
    let t = Tokens::get(ui.ctx());
    egui::Frame::new().inner_margin(egui::Margin::symmetric(16, 10)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.add(egui::Label::new(RichText::new(label).color(t.muted)).selectable(false));
            ui.with_layout(Layout::right_to_left(Align::Center), value);
        });
    });
    kit::hairline(ui);
}

/// Two panels of one height: side by side when there is room, else stacked. `left` and `right` draw one panel each and
/// are given the height to be at least (0 when stacked) and return the height they came to. The heights are kept for the
/// next frame, so a change of content takes one more pass.
fn equal_panels(ui: &mut egui::Ui, key: &str, left: impl FnOnce(&mut egui::Ui, f32) -> f32, right: impl FnOnce(&mut egui::Ui, f32) -> f32) {
    let id = ui.id().with(("admin-panels", key));
    if ui.available_width() < PANELS_SIDE_BY_SIDE {
        left(ui, 0.0);
        ui.add_space(12.0);
        right(ui, 0.0);
        return;
    }
    let stored: f32 = ui.data(|d| d.get_temp(id)).unwrap_or(0.0);
    let mut heights = [0.0_f32; 2];
    ui.columns(2, |cols| {
        let [l, r] = cols else { return };
        heights[0] = left(l, stored);
        heights[1] = right(r, stored);
    });
    let tallest = heights[0].max(heights[1]);
    if (tallest - stored).abs() > 0.5 {
        ui.data_mut(|d| d.insert_temp(id, tallest));
        ui.ctx().request_discard("admin panels measured");
        ui.ctx().request_repaint();
    }
}

/// A bar of `width` points and `height` high: `room`'s share filled (red when full, yellow from the warning share, else the
/// accent), with a tick at the warning share when `tick`. No room: an empty track.
fn bar_sized(ui: &mut egui::Ui, room: Option<Room>, width: f32, height: f32, tick: bool) {
    let t = Tokens::get(ui.ctx());
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width.max(20.0), height), egui::Sense::hover());
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
        if tick {
            let x = rect.left() + rect.width() * (crate::admin::WARN_PERCENT as f32 / 100.0);
            painter.vline(x, rect.y_range(), egui::Stroke::new(2.0, t.warn));
        }
    }
}

/// A 6 points high bar with the warning tick.
fn bar(ui: &mut egui::Ui, room: Option<Room>, width: f32) {
    bar_sized(ui, room, width, 6.0, true);
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

/// The tags of a store's state: "OK", or what is near and how far. For a right-aligned table cell, which adds the
/// rightmost first.
fn warning_tags(ui: &mut egui::Ui, warnings: &[Warning]) {
    if warnings.is_empty() {
        kit::tag(ui, "OK", Tone::Good);
    }
    for warning in warnings.iter().rev() {
        kit::tag(ui, &warning.text(), if warning.room.full() { Tone::Bad } else { Tone::Warn });
    }
}

/// The switcher of the admin pages: Accounts, Storage and Audit log as one segmented control. `selected` is the index of
/// the part that is shown; a click on another part sends the event that shows it.
fn switcher(ui: &mut egui::Ui, selected: usize, events: &mut Vec<Event>) {
    let segments = [Segment::new("Accounts"), Segment::new("Storage"), Segment::new("Audit log")];
    match kit::segmented(ui, "Admin section", &segments, selected) {
        Some(i) if i != selected => match i {
            0 => send(events, AdminEvent::Show(Section::Accounts)),
            1 => send(events, AdminEvent::Show(Section::Storage)),
            _ => events.push(Event::Audit(crate::audit::AuditMsg::OpenAudit(None))),
        },
        _ => {}
    }
}

/// The admin landing page: the switcher over the part that is shown.
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
        ui.spacing_mut().item_spacing.x = 12.0;
        switcher(ui, if section == Section::Accounts { 0 } else { 1 }, events);
        if ui.add_enabled(!list.loading, kit::Button::new("Refresh")).clicked() {
            send(events, AdminEvent::Reload);
        }
        if let Some(info) = server {
            kit::caption(
                ui,
                &format!(
                    "Server {} · {} accounts · {} computers · {} · {}",
                    info.version, info.users, info.devices_active, info.storage_kind, info.database_kind
                ),
            );
        }
    });
    ui.add_space(18.0);
    match section {
        Section::Accounts => accounts(ui, storage, server, list, me, events),
        Section::Storage => storage_page(ui, storage),
    }
}

/// The cells with the counts, as one row of joined cells. The storage call has all four; without it (it failed, or is still
/// asking) the server's own counters give what they know: what is stored and how many accounts.
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
    let mut items: Vec<(&str, String, String)> =
        vec![("Stored", format_bytes(stored), "files of all accounts".to_owned()), ("Total accounts", accounts.to_string(), accounts_note)];
    if let Some(storage) = storage {
        items.push(("Near quota", storage.accounts_near_quota.to_string(), "at 90 % of their quota or more".to_owned()));
        items.push(("Over quota", storage.accounts_over_quota.to_string(), "at or past their quota: uploads refused".to_owned()));
    }
    const BADGES: [&str; 4] = ["01", "02", "03", "04"];
    // Four cells of at least 190 points stand in one row; narrower, they are two rows of two (and two cells stack below 380).
    let per_row = if items.len() > 2 && ui.available_width() < items.len() as f32 * 190.0 { 2 } else { items.len() };
    for (n, chunk) in items.chunks(per_row).enumerate() {
        let first = n * per_row;
        let cells: Vec<JoinedCell<'_>> = chunk
            .iter()
            .enumerate()
            .map(|(k, (label, ..))| JoinedCell::new(BADGES.get(first + k).copied().unwrap_or("00"), Tone::Accent, label))
            .collect();
        kit::joined_cells(ui, ("admin-stats", n), &cells, chunk.len() as f32 * 190.0, |ui, i, part| {
            let (Part::Body, Some((_, value, note))) = (part, chunk.get(i)) else { return };
            ui.horizontal(|ui| big(ui, value, 34.0));
            small_weak(ui, note.as_str());
        });
    }
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

/// The search row's pieces, in the order of the row: its label, the field, the Search button, Clear (while a search is
/// applied) and New account. `rtl`: for a right-to-left layout, which adds the rightmost first.
fn search_row(ui: &mut egui::Ui, list: &mut UserList, events: &mut Vec<Event>, rtl: bool) {
    let mut piece = |ui: &mut egui::Ui, n: usize| match n {
        0 => {
            kit::section_label(ui, "Search");
        }
        1 => {
            let field = ui.add(kit::text_edit(&mut list.search).hint_text("Account name").desired_width(220.0));
            if entered(&field, ui) {
                send(events, AdminEvent::Search);
            }
        }
        2 => {
            if ui.add(kit::Button::new("Find")).clicked() {
                send(events, AdminEvent::Search);
            }
        }
        3 => {
            if !list.applied.is_empty() && ui.add(kit::Button::new("Clear")).clicked() {
                send(events, AdminEvent::ClearSearch);
            }
        }
        _ => {
            if ui.add(kit::Button::new("New account").primary()).clicked() {
                send(events, AdminEvent::NewUser);
            }
        }
    };
    if rtl {
        for n in (0..5).rev() {
            piece(ui, n);
        }
    } else {
        for n in 0..5 {
            piece(ui, n);
        }
    }
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
        ui.add_space(18.0);
    }
    // The label at the left and the search at the right of one row; in a narrow window the search wraps under the label.
    // The search stays a button as well as Enter: the list is the server's page of 20, found by the server's query, so
    // filtering the loaded page as you type would show a different (partial) answer.
    let title = kit::numbered(2, "Who uses how much");
    if ui.available_width() >= 760.0 {
        kit::label_row(ui, &title, |ui| search_row(ui, list, events, true));
    } else {
        kit::section_label(ui, &title);
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| search_row(ui, list, events, false));
    }
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
        accounts_table(ui, &users, me, events);
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

/// The role: a tag for an admin, a muted mono word for a member.
fn role_tag(ui: &mut egui::Ui, user: &AdminUserInfo) {
    if user.role == "admin" {
        kit::tag(ui, "Admin", Tone::Warn);
    } else {
        kit::section_label(ui, "Member");
    }
}

/// The role as a bordered tag in the muted tone (the account page's title).
fn role_tag_bordered(ui: &mut egui::Ui, user: &AdminUserInfo) {
    if user.role == "admin" {
        kit::tag(ui, "Admin", Tone::Warn);
    } else {
        kit::tag(ui, "Member", Tone::Neutral);
    }
}

fn accounts_table(ui: &mut egui::Ui, users: &[AdminUserInfo], me: &str, events: &mut Vec<Event>) {
    let t = Tokens::get(ui.ctx());
    let columns = [
        Column::flex("Account", 150.0).weighted(2.0),
        Column::fixed("Role", 90.0),
        Column::fixed("Computers", 112.0),
        Column::flex("Stored / quota", 160.0).weighted(1.4),
        Column::fixed("Two-factor", 124.0),
        Column::fixed("State", 140.0),
        Column::fixed("", 110.0),
    ];
    kit::table(ui, "admin-accounts", &columns, users.len(), ROW_H, |ui, row, column| {
        let Some(user) = users.get(row) else { return };
        match column {
            0 => {
                let you = user.username == me;
                ui.scope(|ui| {
                    if you {
                        ui.set_max_width((ui.available_width() - 44.0).max(40.0));
                    }
                    kit::truncated(ui, RichText::new(&user.username).strong());
                });
                if you {
                    ui.label(RichText::new("(you)").color(t.muted));
                }
            }
            1 => role_tag(ui, user),
            2 => {
                ui.label(RichText::new(user.devices_active.to_string()).monospace());
            }
            3 => {
                let width = ui.available_width();
                // The text row (18 points) and the thin bar, as one block centred in the row.
                let line = 18.0;
                let block = if user.quota_bytes.is_some() { line + 6.0 + 4.0 } else { line };
                ui.vertical(|ui| {
                    ui.set_width(width);
                    ui.spacing_mut().item_spacing.y = 6.0;
                    // (A space is followed by the item spacing too.)
                    ui.add_space((((ROW_H - block) / 2.0) - 6.0).max(0.0).floor());
                    ui.allocate_ui_with_layout(egui::vec2(width, line), Layout::left_to_right(Align::Center), |ui| {
                        ui.label(RichText::new(format_bytes(user.stored_bytes)).monospace());
                        match user.quota_bytes {
                            Some(quota) => {
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    ui.label(RichText::new(format!("of {}", format_bytes(quota))).color(t.muted));
                                });
                            }
                            None => {
                                ui.label(RichText::new("· no limit").color(t.muted));
                            }
                        }
                    });
                    if let Some(quota) = user.quota_bytes {
                        bar_sized(ui, Some(Room { used: user.stored_bytes, total: quota }), width, 4.0, false);
                    }
                });
            }
            4 => {
                if user.two_factor_enabled {
                    kit::tag(ui, "On", Tone::Good);
                } else {
                    ui.label(RichText::new("off").color(t.muted));
                }
            }
            5 => account_state(ui, user),
            _ => {
                if ui.add(kit::Button::new("Manage")).clicked() {
                    send(events, AdminEvent::OpenUser(user.id));
                }
            }
        }
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
        ui.add_space(18.0);
        kit::section_label(ui, "Top accounts");
        ui.add_space(8.0);
        let columns = [Column::flex("Account", 200.0), Column::fixed("Stored", 140.0), Column::fixed("Effective quota", 180.0)];
        kit::table(ui, "admin-storage-top", &columns, storage.top_accounts.len(), 48.0, |ui, row, column| {
            let Some(account) = storage.top_accounts.get(row) else { return };
            match column {
                0 => {
                    kit::truncated(ui, RichText::new(&account.username).strong());
                }
                1 => {
                    ui.label(RichText::new(format_bytes(account.stored_bytes)).monospace());
                }
                _ => {
                    ui.label(RichText::new(account.quota_bytes.map_or_else(|| "none".to_owned(), format_bytes)).monospace());
                }
            }
        });
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
/// upper bound. A set capacity on a disk is a second measure with a warning of its own. The header's tag follows the
/// measures: red from a full room on, yellow from the warning share; the bars follow their own room.
fn server_storage(ui: &mut egui::Ui, block: &StorageBlock) {
    let near = block.near_count();
    let title = kit::numbered(1, "Server storage");
    let warnings = block.store.warnings();
    let over = block.server_disk.is_some_and(Room::full) || warnings.iter().any(|w| w.room.full());
    panel(
        ui,
        Some(&title),
        |ui| {
            if over {
                kit::tag(ui, "Over capacity", Tone::Bad);
            } else if near > 0 {
                kit::tag(ui, "Near capacity", Tone::Warn);
            }
        },
        |ui| {
            section(ui, |ui| {
                kit::section_label(ui, "All storage");
                ui.horizontal_wrapped(|ui| {
                    match (block.basis, block.all) {
                        (Basis::Disk, Some(room)) => {
                            big(ui, &format_bytes(room.used), 44.0);
                            ui.add_space(4.0);
                            ui.label(format!("used of {} · {} free", format_bytes(room.total), format_bytes(room.total.saturating_sub(room.used))));
                        }
                        (Basis::Accounts, Some(room)) => {
                            big(ui, &format_bytes(room.used), 44.0);
                            ui.add_space(4.0);
                            ui.label(format!(
                                "stored by the accounts, of {} · {} left (counted per account: an upper bound)",
                                format_bytes(room.total),
                                format_bytes(room.total.saturating_sub(room.used))
                            ));
                        }
                        _ => {
                            big(ui, &format_bytes(block.stored), 44.0);
                            ui.add_space(4.0);
                            ui.label(RichText::new("stored by the accounts · no size to measure it against").weak());
                        }
                    }
                    if block.all.is_some() {
                        ui.add_space(4.0);
                        ui.horizontal(|ui| warning_tags_left(ui, &warnings));
                    }
                });
                bar_sized(ui, block.all, ui.available_width(), 10.0, true);
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
            egui::Frame::new().inner_margin(egui::Margin::same(12)).show(ui, |ui| {
                let columns = [
                    Column::flex("Store", 200.0).weighted(1.4),
                    Column::fixed("Kind", 110.0),
                    Column::flex("Used of capacity", 160.0).weighted(2.0),
                    Column::fixed("", 170.0),
                    Column::fixed("State", 150.0),
                ];
                kit::table(ui, "admin-room", &columns, 2, ROW_H, |ui, row, column| {
                    if row == 0 { server_disk_cell(ui, block, column) } else { store_cell(ui, &block.store, &warnings, column) }
                });
            });
            kit::hairline(ui);
            section(ui, |ui| {
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

/// The tags of a store's state, left to right.
fn warning_tags_left(ui: &mut egui::Ui, warnings: &[Warning]) {
    if warnings.is_empty() {
        kit::tag(ui, "OK", Tone::Good);
    }
    for warning in warnings {
        kit::tag(ui, &warning.text(), if warning.room.full() { Tone::Bad } else { Tone::Warn });
    }
}

/// A row's bar (as wide as its column), its numbers and (in the next column) its state are drawn by the cells below.
fn room_bar_cell(ui: &mut egui::Ui, room: Option<Room>) {
    let width = ui.available_width();
    bar(ui, room, width);
}

fn room_numbers_cell(ui: &mut egui::Ui, room: Option<Room>) {
    match room {
        Some(room) => {
            ui.label(RichText::new(format!("{} / {}", format_bytes(room.used), format_bytes(room.total))).monospace());
        }
        None => dash(ui),
    }
}

/// A cell of the server disk's row.
fn server_disk_cell(ui: &mut egui::Ui, block: &StorageBlock, column: usize) {
    match column {
        0 => {
            let note = match block.database_bytes {
                Some(bytes) => format!("database {}", format_bytes(bytes)),
                None => "database size not reported".to_owned(),
            };
            kit::two_lines(ui, ROW_H, "Server disk", Some(&note));
        }
        1 => {
            kit::tag(ui, "Disk", Tone::Neutral);
        }
        2 => room_bar_cell(ui, block.server_disk),
        3 => room_numbers_cell(ui, block.server_disk),
        _ => room_state(ui, block.server_disk),
    }
}

/// A cell of the store's row.
fn store_cell(ui: &mut egui::Ui, store: &StoreRoom, warnings: &[Warning], column: usize) {
    let room = store.room();
    match column {
        0 => {
            let note = match store {
                StoreRoom::Filesystem { capacity: Some(set), .. } => format!("filesystem of the store · capacity set: {}", format_bytes(set.total)),
                StoreRoom::Filesystem { capacity: None, .. } => "filesystem of the store".to_owned(),
                StoreRoom::Capacity { room, .. } => format!("accounts' total against the capacity set: {}", format_bytes(room.total)),
                StoreRoom::NoCapacity { .. } => "capacity not set".to_owned(),
                StoreRoom::NoNumbers { .. } => "no size reported".to_owned(),
                StoreRoom::Unknown => "not reported".to_owned(),
            };
            kit::two_lines(ui, ROW_H, "Blob store", Some(&note));
        }
        1 => match store.kind() {
            Some(kind) => {
                kit::tag(ui, kind, if kind == "s3" { Tone::Accent } else { Tone::Neutral });
            }
            None => dash(ui),
        },
        2 => room_bar_cell(ui, room),
        3 => room_numbers_cell(ui, room),
        _ => match room {
            Some(_) => warning_tags(ui, warnings),
            None => dash(ui),
        },
    }
}

/// Member or Admin as a segmented control.
fn role_choice(ui: &mut egui::Ui, role: &mut RoleChoice) {
    let segments = [Segment::new("Member"), Segment::new("Admin")];
    let selected = usize::from(*role == RoleChoice::Admin);
    if let Some(i) = kit::segmented(ui, "Role", &segments, selected) {
        *role = if i == 1 { RoleChoice::Admin } else { RoleChoice::Member };
    }
}

/// The path above a page's headline, "Admin / Accounts / ana", as plain text.
fn breadcrumb(ui: &mut egui::Ui, parts: &str) {
    kit::section_label(ui, parts);
    ui.add_space(4.0);
}

/// "Admin / Accounts / ana" with "Accounts" a link back to the list.
fn breadcrumb_back(ui: &mut egui::Ui, last: &str, events: &mut Vec<Event>) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        kit::section_label(ui, "Admin /");
        if kit::text_link(ui, "Accounts", 12.0).on_hover_text("Back to the accounts").clicked() {
            send(events, AdminEvent::BackToList);
        }
        kit::section_label(ui, &format!("/ {last}"));
    });
    ui.add_space(4.0);
}

fn big_headline(ui: &mut egui::Ui, text: &str) {
    kit::headline_large(ui, text);
}

/// A button as wide as the space, `height` high (primary, red outlined, or outlined).
fn full_button(ui: &mut egui::Ui, text: &str, style: ButtonStyle, height: f32) -> egui::Response {
    let size = egui::vec2(ui.available_width(), height);
    let button = kit::Button::new(text).min_size(size);
    let button = match style {
        ButtonStyle::Primary => button.primary(),
        ButtonStyle::Outlined => button,
    };
    ui.add_sized(size, button)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ButtonStyle {
    Primary,
    Outlined,
}

fn create(ui: &mut egui::Ui, form: &mut CreateForm, events: &mut Vec<Event>) {
    breadcrumb_back(ui, "New", events);
    big_headline(ui, "New account");
    ui.add_space(18.0);
    equal_panels(
        ui,
        "create",
        |ui, min_h| {
            let who = kit::numbered(1, "Who");
            panel_h(ui, min_h, Some(&who), |_| {}, |ui| {
                section(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 6.0;
                    kit::section_label(ui, "Username");
                    let name = ui.add(kit::text_edit_large(&mut form.username).hint_text("ana").desired_width(f32::INFINITY));
                    let enter = entered(&name, ui);
                    ui.add_space(10.0);
                    kit::section_label(ui, "Role");
                    role_choice(ui, &mut form.role);
                    small_weak(
                        ui,
                        "The account starts without a password: you get an invite code to give them. Set a quota afterwards, on the account's page.",
                    );
                    if let Some(error) = &form.error {
                        ui.add_space(4.0);
                        super::error_line(ui, error);
                    }
                    ui.add_space(14.0);
                    if full_button(ui, "Continue", ButtonStyle::Primary, kit::CONTROL_H_LARGE).clicked() || enter {
                        send(events, AdminEvent::SubmitCreate);
                    }
                    if full_button(ui, "Cancel", ButtonStyle::Outlined, kit::CONTROL_H_LARGE).clicked() {
                        send(events, AdminEvent::BackToList);
                    }
                });
            })
            .1
        },
        |ui, min_h| {
            let invite = kit::numbered(2, "Invite");
            panel_h(
                ui,
                min_h,
                Some(&invite),
                |_| {},
                |ui| {
                    section(ui, |ui| {
                        small_weak(ui, "The invite code shows here once the account is created. It is shown once.");
                    });
                },
            )
            .1
        },
    );
}

/// The date of `ms` (UTC) as 2026-11-14.
pub(super) fn date(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000) + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_part = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = month_part + if month_part < 10 { 3 } else { -9 };
    let year = year + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// The date and time of `ms` (UTC): 2026-11-14 16:40 UTC.
pub(super) fn date_time(ms: i64) -> String {
    let secs = ms.div_euclid(1000).rem_euclid(86_400);
    format!("{} {:02}:{:02} UTC", date(ms), secs / 3_600, secs % 3_600 / 60)
}

/// The account's page: the title row, the Storage and Account panels, the computers.
fn account(ui: &mut egui::Ui, view: &mut UserView, user: Option<&AdminUserInfo>, me: &str, now_ms: i64, events: &mut Vec<Event>) {
    // The title (with the path over it) at the left, the actions at the right of the same row; below this width the
    // actions wrap under the title.
    const ACTIONS_W: f32 = 420.0;
    let is_me = user.is_some_and(|u| u.username == me);
    let actions = user.map(|u| title_actions(u, is_me)).unwrap_or_default();
    let available = ui.available_width();
    let wide = available >= 640.0 && !actions.is_empty();
    let left_w = if wide { available - ACTIONS_W } else { available };
    let origin = ui.cursor().min;
    let title = ui.allocate_ui_with_layout(egui::vec2(left_w, 0.0), Layout::top_down(Align::Min), |ui| {
        breadcrumb_back(ui, &view.username, events);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            big_headline(ui, &view.username);
            if let Some(user) = user {
                role_tag_bordered(ui, user);
                account_state(ui, user);
                if user.two_factor_enabled {
                    kit::tag(ui, "Two-factor on", Tone::Good);
                }
            }
        });
    });
    if wide {
        let rect = egui::Rect::from_min_max(
            egui::pos2(origin.x + left_w, title.response.rect.top()),
            egui::pos2(origin.x + available, title.response.rect.bottom()),
        );
        ui.scope_builder(egui::UiBuilder::new().max_rect(rect).layout(Layout::right_to_left(Align::Max)), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            for action in actions.iter().rev() {
                action.show(ui, events);
            }
        });
    } else if !actions.is_empty() {
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            for action in &actions {
                action.show(ui, events);
            }
        });
    }
    ui.add_space(18.0);
    let Some(user) = user else {
        super::error_line(ui, "This account isn't in the list any more.");
        return;
    };
    let devices = view.devices.clone();
    let (mut left_events, mut right_events) = (Vec::new(), Vec::new());
    equal_panels(
        ui,
        "account",
        |ui, min_h| storage_card(ui, min_h, view, user, &mut left_events),
        |ui, min_h| account_card(ui, min_h, user, devices.as_deref(), now_ms, is_me, &mut right_events),
    );
    events.append(&mut left_events);
    events.append(&mut right_events);
    ui.add_space(18.0);
    computers(ui, view, user, now_ms, events);
}

/// A button of the account page's title row.
struct TitleAction {
    label: &'static str,
    hover: &'static str,
    danger: bool,
    event: Event,
}

impl TitleAction {
    fn show(&self, ui: &mut egui::Ui, events: &mut Vec<Event>) {
        let button = if self.danger { kit::Button::new(self.label).danger() } else { kit::Button::new(self.label) };
        if ui.add(button).on_hover_text(self.hover).clicked() {
            events.push(self.event.clone());
        }
    }
}

/// The buttons of the title row, left to right: a password reset code, the account's activity and disable or enable.
fn title_actions(user: &AdminUserInfo, is_me: bool) -> Vec<TitleAction> {
    let (id, username) = (user.id, user.username.clone());
    let mut actions = vec![
        TitleAction {
            label: "Reset code",
            hover: "A password reset code, good for an hour; using it signs all their computers out",
            danger: false,
            event: Event::Admin(AdminEvent::Ask(Action::IssueReset { id, username: username.clone(), reset_two_factor: false })),
        },
        TitleAction {
            label: "Activity",
            hover: "The audit log, for this account",
            danger: false,
            event: Event::Audit(crate::audit::AuditMsg::OpenAudit(Some((id, username.clone())))),
        },
    ];
    if !is_me {
        actions.push(if user.disabled_ms.is_some() {
            TitleAction {
                label: "Enable",
                hover: "They can sign in again",
                danger: false,
                event: Event::Admin(AdminEvent::Ask(Action::Enable { id, username })),
            }
        } else {
            TitleAction {
                label: "Disable",
                hover: "Their computers are signed out and they can't sign in until you enable the account",
                danger: true,
                event: Event::Admin(AdminEvent::Ask(Action::Disable { id, username })),
            }
        });
    }
    actions
}

fn storage_card(ui: &mut egui::Ui, min_h: f32, view: &mut UserView, user: &AdminUserInfo, events: &mut Vec<Event>) -> f32 {
    let t = Tokens::get(ui.ctx());
    let mut refresh = false;
    let (_, height) = panel_h(
        ui,
        min_h,
        Some("Storage"),
        |ui| {
            refresh = ui.add(kit::Button::new("Refresh")).clicked();
            ui.add(egui::Label::new(RichText::new("this account").size(11.0).color(t.muted)).selectable(false));
        },
        |ui| {
            // The figure and its bar; the list's numbers until the account's own usage comes.
            let stored = view.usage.as_ref().map_or(user.stored_bytes, |u| u.stored_bytes);
            let quota = view.usage.as_ref().map_or(user.quota_bytes, |u| u.quota_bytes);
            section(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    big(ui, &format_bytes(stored), 36.0);
                    ui.add_space(6.0);
                    match quota {
                        Some(quota) => {
                            let percent = Room { used: stored, total: quota }.percent();
                            ui.label(RichText::new(format!("of {} quota · {percent} %", format_bytes(quota))).color(t.muted));
                        }
                        None => {
                            ui.label(RichText::new("no quota").color(t.muted));
                        }
                    }
                });
                bar_sized(ui, quota.map(|total| Room { used: stored, total }), ui.available_width(), 8.0, false);
                if let Some(error) = &view.usage_error {
                    super::error_line(ui, error);
                } else if view.usage.is_none() && view.usage_loading {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Asking the server…");
                    });
                }
            });
            kit::hairline(ui);
            let estimate = |bytes: Option<u64>| bytes.map_or_else(|| "not available".to_owned(), |b| format!("about {}", format_bytes(b)));
            match &view.usage {
                Some(usage) if usage.estimates.index_complete => {
                    kv(ui, "In the trash", |ui| {
                        ui.label(RichText::new(estimate(usage.trash_bytes)).monospace());
                    });
                    kv(ui, "Old versions", |ui| {
                        ui.label(RichText::new(estimate(usage.history_bytes)).monospace());
                    });
                }
                Some(_) => {
                    kv(ui, "In the trash", |ui| {
                        ui.label(RichText::new("not available yet").monospace());
                    });
                    kv(ui, "Old versions", |ui| {
                        ui.label(RichText::new("not available yet").monospace());
                    });
                }
                None => {}
            }
            section(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 6.0;
                kit::section_label(ui, "Role");
                role_choice(ui, &mut view.role);
                ui.add_space(6.0);
                kit::section_label(ui, "Quota");
                let width = ui.available_width();
                let (save_w, default_w, gap) = (90.0, 150.0, 8.0);
                let has_override = !view.quota_shown.trim().is_empty();
                let mut save = false;
                let mut reset = false;
                let buttons = |ui: &mut egui::Ui| -> (bool, bool) {
                    let save = ui.add(kit::Button::new("Save").primary().min_size(egui::vec2(save_w, 0.0))).clicked();
                    let reset = ui
                        .add_enabled(has_override, kit::Button::new("Use default").min_size(egui::vec2(default_w, 0.0)))
                        .on_hover_text("Remove this account's own quota: the server's default applies")
                        .clicked();
                    (save, reset)
                };
                if width >= 420.0 {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = gap;
                        let field = ui.add(kit::text_edit(&mut view.quota).hint_text("none").desired_width(width - save_w - default_w - 2.0 * gap));
                        save |= entered(&field, ui);
                        let (s, r) = buttons(ui);
                        save |= s;
                        reset |= r;
                    });
                } else {
                    let field = ui.add(kit::text_edit(&mut view.quota).hint_text("none").desired_width(f32::INFINITY));
                    save |= entered(&field, ui);
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = gap;
                        let (s, r) = buttons(ui);
                        save |= s;
                        reset |= r;
                    });
                }
                small_weak(ui, "Quota in GiB; empty means none. Every change asks for your password.");
                if let Some(error) = &view.error {
                    super::error_line(ui, error);
                }
                if reset {
                    view.quota.clear();
                }
                if save || reset {
                    send(events, AdminEvent::SubmitEdit);
                }
            });
        },
    );
    if refresh {
        send(events, AdminEvent::RefreshUsage);
    }
    height
}

fn account_card(
    ui: &mut egui::Ui,
    min_h: f32,
    user: &AdminUserInfo,
    devices: Option<&[craft_cloud_app::DeviceInfo]>,
    now_ms: i64,
    is_me: bool,
    events: &mut Vec<Event>,
) -> f32 {
    let t = Tokens::get(ui.ctx());
    let id = user.id;
    let username = user.username.clone();
    let last = devices.and_then(|d| d.iter().filter(|d| !d.revoked).max_by_key(|d| d.last_used_ms));
    panel_h(
        ui,
        min_h,
        Some("Account"),
        |_| {},
        |ui| {
            kv(ui, "Created", |ui| {
                ui.label(date(user.created_ms));
            });
            kv(ui, "Last sign-in", |ui| match last {
                Some(device) => {
                    kit::truncated(ui, format!("{} · {}", super::ago(now_ms, device.last_used_ms), device.name));
                }
                None => dash(ui),
            });
            kv(ui, "Two-factor", |ui| {
                ui.label(if user.two_factor_enabled { "on" } else { "off" });
            });
            kv(ui, "Role", |ui| {
                ui.label(if user.role == "admin" { "Admin" } else { "Member" });
            });
            section(ui, |ui| {
                kit::section_label(ui, "Locking");
                ui.add(
                    egui::Label::new(if is_me {
                        RichText::new("You can't disable or remove your own account here.").color(t.muted)
                    } else {
                        RichText::new(
                            "A disabled account is signed out on every computer at once. Its files stay; enabling it lets it sign in again.",
                        )
                    })
                    .wrap(),
                );
                if !is_me {
                    ui.horizontal_wrapped(|ui| {
                        if ui.add(kit::Button::new("Remove").danger()).clicked() {
                            send(events, AdminEvent::Ask(Action::Remove { id, username: username.clone() }));
                        }
                        if user.two_factor_enabled
                            && ui
                                .add(kit::Button::new("Turn two-factor off"))
                                .on_hover_text("They sign in with their password alone; their computers are signed out")
                                .clicked()
                        {
                            send(events, AdminEvent::Ask(Action::ResetTwoFactor { id, username: username.clone() }));
                        }
                    });
                }
            });
            kit::hairline(ui);
            section(ui, |ui| {
                kit::section_label(ui, "Invites and codes");
                ui.horizontal_wrapped(|ui| {
                    if ui.add(kit::Button::new("New invite")).on_hover_text("For an account that has no password yet").clicked() {
                        send(events, AdminEvent::Ask(Action::IssueInvite { id, username: username.clone() }));
                    }
                    if user.two_factor_enabled
                        && ui
                            .add(kit::Button::new("Reset code, two-factor off"))
                            .on_hover_text("As a reset code, and using it turns their two-factor sign-in off")
                            .clicked()
                    {
                        send(events, AdminEvent::Ask(Action::IssueReset { id, username: username.clone(), reset_two_factor: true }));
                    }
                    // Always offered: the list says an invite is open (`invite_pending`), but not a password reset code. The
                    // server answers `not_found` when none is.
                    if ui
                        .add(kit::Button::new("Revoke open code").danger())
                        .on_hover_text("Ends the account's open invite or password reset code")
                        .clicked()
                    {
                        send(events, AdminEvent::Ask(Action::RevokeInvite { id, username: username.clone() }));
                    }
                });
            });
        },
    )
    .1
}

fn computers(ui: &mut egui::Ui, view: &UserView, user: &AdminUserInfo, now_ms: i64, events: &mut Vec<Event>) {
    let t = Tokens::get(ui.ctx());
    let (id, username) = (user.id, user.username.clone());
    kit::label_row(ui, &format!("Computers of {}", user.username), |ui| {
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
    let live: Vec<&craft_cloud_app::DeviceInfo> = devices.iter().filter(|d| !d.revoked).collect();
    if live.is_empty() {
        ui.label(RichText::new("No computers signed in.").weak());
        return;
    }
    let columns = [
        Column::flex("Computer", 200.0).weighted(1.4),
        Column::fixed("Kind", 110.0),
        Column::fixed("Platform", 120.0),
        Column::fixed("Version", 110.0),
        Column::fixed("Last seen", 150.0),
        Column::fixed("", 130.0),
    ];
    kit::table(ui, "admin-devices", &columns, live.len(), 56.0, |ui, row, column| {
        let Some(device) = live.get(row) else { return };
        match column {
            0 => {
                kit::truncated(ui, RichText::new(&device.name).strong());
            }
            1..=3 => {
                let text = match column {
                    1 => &device.client_kind,
                    2 => &device.platform,
                    _ => &device.app_version,
                };
                if text.is_empty() {
                    dash(ui);
                } else if column == 2 {
                    ui.label(text);
                } else {
                    ui.label(RichText::new(text).monospace());
                }
            }
            4 => {
                ui.label(RichText::new(super::ago(now_ms, device.last_used_ms)).color(t.muted));
            }
            _ => {
                if ui.add(kit::Button::new("Revoke").danger()).on_hover_text("It can't sync any more; its files stay there").clicked() {
                    let action = Action::RevokeDevice { id, username: username.clone(), device: device.id, device_name: device.name.clone() };
                    send(events, AdminEvent::Ask(action));
                }
            }
        }
    });
}

/// The password a change asks for, in a panel under a breadcrumb.
fn confirmation(ui: &mut egui::Ui, dialog: &mut Dialog, events: &mut Vec<Event>) {
    breadcrumb(ui, &format!("Admin / Accounts / {} / Confirm", dialog.action.username()));
    big_headline(ui, &dialog.action.title());
    ui.add_space(18.0);
    ui.scope(|ui| {
        ui.set_max_width(NARROW_PANEL);
        let destructive = dialog.action.destructive();
        panel(
            ui,
            Some("Confirm"),
            |_| {},
            |ui| {
                section(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 6.0;
                    ui.add(egui::Label::new(dialog.action.explain()).wrap());
                    ui.add_space(8.0);
                    kit::section_label(ui, "Your password");
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
                        kit::section_label(ui, "Code from your authenticator app, or a recovery code");
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
                    ui.add_space(8.0);
                    if let Some(error) = &dialog.error {
                        super::error_line(ui, error);
                        ui.add_space(4.0);
                    }
                    if dialog.busy {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Asking the server…");
                        });
                        return;
                    }
                    // Cancel and the confirming button share the panel's width; a removal confirms in red, outlined.
                    let half = egui::vec2(((ui.available_width() - 8.0) / 2.0).floor(), kit::CONTROL_H);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        if ui.add_sized(half, kit::Button::new("Cancel").min_size(half)).clicked() {
                            send(events, AdminEvent::CancelDialog);
                        }
                        let button = kit::Button::new(dialog.action.button()).min_size(half);
                        let button = if destructive { button.danger() } else { button.primary() };
                        if ui.add_sized(half, button).clicked() || enter {
                            send(events, AdminEvent::Confirm);
                        }
                    });
                });
            },
        );
    });
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

/// `code` in groups of four joined by dashes, like the join string (`K7M4-Q9TX`); a code that has dashes is shown as it is.
pub(super) fn grouped(code: &str) -> String {
    if code.contains(['-', ' ']) {
        return code.to_owned();
    }
    let chars: Vec<char> = code.chars().collect();
    chars.chunks(4).map(|group| group.iter().collect::<String>()).collect::<Vec<_>>().join("-")
}

/// The part of a join link that names the server: `cloud.example.org` of `https://cloud.example.org/join/CODE`.
fn server_of(url: &str) -> Option<&str> {
    let host = super::host(url).split('/').next().unwrap_or_default();
    (!host.is_empty()).then_some(host)
}

/// A code the server made: shown once, to copy and hand over. An invite stands next to the account it is for.
fn shown_code(ui: &mut egui::Ui, code: &ShownCode, user: Option<&AdminUserInfo>, server: Option<&str>, now_ms: i64, events: &mut Vec<Event>) {
    let t = Tokens::get(ui.ctx());
    let title = match code.kind {
        CodeKind::Invite => format!("Invite for {}", code.username),
        CodeKind::PasswordReset => format!("Password reset code for {}", code.username),
    };
    breadcrumb(ui, &format!("Admin / Accounts / {}", code.username));
    big_headline(ui, &title);
    ui.add_space(18.0);
    let invite_panel = |ui: &mut egui::Ui, min_h: f32, number: usize, events: &mut Vec<Event>| -> f32 {
        let label = if code.kind == CodeKind::Invite { kit::numbered(number, "Invite") } else { "Code".to_owned() };
        panel_h(
            ui,
            min_h,
            Some(&label),
            |ui| {
                kit::tag(ui, "Created", Tone::Good);
            },
            |ui| {
                section(ui, |ui| {
                    // A notice, not an error: normal ink.
                    ui.add(egui::Label::new("Note it now: it is shown once and can't be shown again.").wrap());
                    let well = |ui: &mut egui::Ui, text: &str, shown: &str, size: Option<f32>| {
                        egui::Frame::new().fill(t.sunken).stroke(egui::Stroke::new(1.0, t.line_strong)).inner_margin(egui::Margin::same(16)).show(
                            ui,
                            |ui| {
                                ui.set_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    let mut mono = RichText::new(shown).monospace();
                                    if let Some(size) = size {
                                        mono = mono.size(size).extra_letter_spacing(size * 0.12);
                                    }
                                    ui.add(egui::Label::new(mono).selectable(true).wrap());
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        if ui.add(kit::Button::new("Copy")).clicked() {
                                            ui.ctx().copy_text(text.to_owned());
                                        }
                                    });
                                });
                            },
                        );
                    };
                    well(ui, &code.code, &grouped(&code.code), Some(22.0));
                    if let Some(url) = &code.join_url {
                        ui.label("Or the whole join string:");
                        well(ui, url, url, None);
                    }
                });
                kit::hairline(ui);
                kv(ui, "Valid until", |ui| {
                    ui.label(date_time(code.expires_ms));
                });
                let server = server.or_else(|| code.join_url.as_deref().and_then(server_of));
                if let Some(server) = server {
                    kv(ui, "Server", |ui| {
                        ui.label(RichText::new(server).monospace());
                    });
                }
                if let Some(user) = user {
                    kv(ui, "Quota", |ui| {
                        ui.label(user.quota_bytes.map_or_else(|| "Server default".to_owned(), format_bytes));
                    });
                }
                section(ui, |ui| {
                    ui.label(RichText::new(format!("Stops working {}.", until(now_ms, code.expires_ms))).color(t.muted));
                    // Open the account or make another one, when the account is in the list.
                    if code.kind == CodeKind::Invite
                        && let Some(user) = user
                    {
                        let half = egui::vec2(((ui.available_width() - 8.0) / 2.0).floor(), kit::CONTROL_H);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            if ui.add_sized(half, kit::Button::new("Open account").min_size(half)).clicked() {
                                send(events, AdminEvent::DismissCode);
                                send(events, AdminEvent::OpenUser(user.id));
                            }
                            if ui.add_sized(half, kit::Button::new("Another account").min_size(half)).clicked() {
                                send(events, AdminEvent::DismissCode);
                                send(events, AdminEvent::NewUser);
                            }
                        });
                    }
                    if full_button(ui, "Done", ButtonStyle::Primary, kit::CONTROL_H).clicked() {
                        send(events, AdminEvent::DismissCode);
                    }
                });
            },
        )
        .1
    };
    if code.kind == CodeKind::Invite {
        // Who the invite is for stands beside it, as in the new account's page.
        let mut events_cell = Some(events);
        equal_panels(
            ui,
            "code",
            |ui, min_h| {
                let who = kit::numbered(1, "Who");
                panel_h(
                    ui,
                    min_h,
                    Some(&who),
                    |_| {},
                    |ui| {
                        kv(ui, "Username", |ui| {
                            ui.label(RichText::new(&code.username).strong());
                        });
                        kv(ui, "Role", |ui| match user {
                            Some(user) => {
                                ui.label(if user.role == "admin" { "Admin" } else { "Member" });
                            }
                            None => dash(ui),
                        });
                        section(ui, |ui| {
                            small_weak(ui, "The account has no password yet: they choose theirs with this code on their first computer.");
                        });
                    },
                )
                .1
            },
            |ui, min_h| match events_cell.take() {
                Some(events) => invite_panel(ui, min_h, 2, events),
                None => 0.0,
            },
        );
    } else {
        ui.scope(|ui| {
            ui.set_max_width(NARROW_PANEL + 80.0);
            invite_panel(ui, 0.0, 1, events);
        });
    }
}
