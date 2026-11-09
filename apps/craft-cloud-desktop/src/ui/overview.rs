//! Signed in, the Overview tab: the sync (online or offline, what the agent is doing, the last sync, the folder, Open folder,
//! Pause), the storage and a short list of the computers with Add a computer, as three joined cells; under them the newest
//! activity; and the Computers tab ([`super::computers`]). Sign out and Quit are in the footer ([`super::shell`]).

use craft_cloud_app::{AccountUsage, RunState, Status};
use egui::RichText;

use super::kit::{Column, JoinedCell, Part, Tokens, Tone};
use crate::admin::format_bytes;
use crate::audit::action_label;
use crate::model::{Busy, Event, Overview, OverviewPage, Recent, Tab};

/// What the sync is doing, in one line, and the Details when it is a problem.
pub fn sync_line(status: &Status) -> (String, Option<String>) {
    let server = status.server.as_deref().map(super::host).unwrap_or("the server");
    match &status.run_state {
        RunState::Idle => ("Up to date".into(), None),
        RunState::Syncing => ("Syncing…".into(), None),
        RunState::Retrying { message } if !status.online => (format!("Can't reach {server} — trying again"), Some(message.clone())),
        RunState::Retrying { message } => ("Syncing failed — trying again".into(), Some(message.clone())),
        // The server itself is full: not the account's quota, so the line says whose room has run out and who can fix it.
        RunState::Paused { reason, reason_code } if server_is_full(reason, reason_code.as_deref()) => {
            ("The server is full. Uploads wait until its admin frees space".into(), Some(reason.clone()))
        }
        RunState::Paused { reason, .. } => (super::problem_line(reason), Some(reason.clone())),
        RunState::PausedByUser => ("Paused: nothing syncs until you resume".into(), None),
        RunState::Stopping => ("Stopping…".into(), None),
        RunState::SignedOut => ("Not signed in".into(), None),
    }
}

/// The start of the agent's words for a pause by `server_full` ("The server is full; the admin needs to free space or raise
/// its capacity"). An agent from before the code (`reason_code`) existed says it in these words alone.
const SERVER_FULL_WORDS: &str = "The server is full";

/// Whether a pause says the server is full, which is not the account's quota: by the agent's `reason_code` when it gives one
/// (`server_full`, and not `quota_exceeded` or any other code), else by its words ([`SERVER_FULL_WORDS`]).
fn server_is_full(reason: &str, reason_code: Option<&str>) -> bool {
    match reason_code {
        Some(code) => code == craft_cloud_proto::api::codes::SERVER_FULL,
        None => reason.contains(SERVER_FULL_WORDS),
    }
}

/// "1 conflict copy", "3 conflict copies" ("1000+" when the agent left older ones out).
fn conflicts_text(count: usize, truncated: bool) -> String {
    let plus = if truncated { "+" } else { "" };
    if count == 1 && !truncated { "1 conflict copy".to_owned() } else { format!("{count}{plus} conflict copies") }
}

/// The width from which the home page's three cells sit side by side.
const SIDE_BY_SIDE: f32 = 840.0;
/// How many computers the home page's cell lists before pointing to the Computers tab.
const SHORT_LIST: usize = 4;
/// The height of a row of the recent activity table.
const ACTIVITY_ROW_H: f32 = 48.0;

pub(super) fn show(ui: &mut egui::Ui, o: &mut Overview, busy: Option<Busy>, now_ms: i64, autostart_problem: Option<&str>, events: &mut Vec<Event>) {
    match o.page {
        OverviewPage::Home => home(ui, o, busy, now_ms, autostart_problem, events),
        OverviewPage::Computers => super::computers::page(ui, o, now_ms, events),
    }
}

fn home(ui: &mut egui::Ui, o: &mut Overview, busy: Option<Busy>, now_ms: i64, autostart_problem: Option<&str>, events: &mut Vec<Event>) {
    // The label row: the section, this computer's name, and Refresh (of the storage figures).
    super::kit::label_row(ui, &super::kit::numbered(1, "This computer"), |ui| {
        if ui.add_enabled(!o.usage_loading, super::kit::Button::new("Refresh")).clicked() {
            events.push(Event::RefreshUsage);
        }
        if let Some(machine) = &o.status.machine {
            super::kit::section_label(ui, machine);
        }
    });
    ui.add_space(8.0);
    // Invitations to team spaces wait for an answer on the Shared tab.
    if o.invitations > 0 {
        ui.horizontal(|ui| {
            let text =
                if o.invitations == 1 { "1 invitation to a team space".to_owned() } else { format!("{} invitations to team spaces", o.invitations) };
            super::kit::tag(ui, &text, Tone::Accent);
            if super::kit::text_link(ui, "Shared ↗", 12.0).clicked() {
                events.push(Event::Tab(Tab::Shared));
            }
        });
        ui.add_space(8.0);
    }
    let cells = [
        JoinedCell::new("01", Tone::Accent, "Sync"),
        JoinedCell::new("02", Tone::Warn, "Storage"),
        JoinedCell::new("03", Tone::Good, "Computers ↗").link(),
    ];
    let clicked = super::kit::joined_cells(ui, "overview-cells", &cells, SIDE_BY_SIDE, |ui, i, part| match (i, part) {
        (0, Part::Body) => sync_body(ui, o, now_ms, autostart_problem, events),
        (0, Part::Footer) => sync_footer(ui, o, busy, events),
        (1, Part::Body) => storage_body(ui, o, events),
        (2, Part::Body) => computers_body(ui, o, now_ms, events),
        (2, Part::Footer) => {
            let size = egui::vec2(ui.available_width(), super::kit::CONTROL_H);
            if ui.add_sized(size, super::kit::Button::new("Add a computer").primary().min_size(size)).clicked() {
                events.push(Event::AddComputer);
            }
        }
        _ => {}
    });
    if clicked.get(2).copied().unwrap_or(false) {
        events.push(Event::Tab(Tab::Computers));
    }
    if let Some(error) = &o.error {
        ui.add_space(8.0);
        super::error_line(ui, error);
    }
    // The agent's work of making a team space from a folder, and the form that starts it.
    super::make_space::show_notice(ui, o, events);
    super::make_space::show_form(ui, o, events);
    ui.add_space(20.0);
    if o.folders.open {
        super::folders::section(ui, 2, o, events);
        ui.add_space(20.0);
    }
    recent_activity(ui, o, now_ms, events);
    ui.add_space(14.0);
}

/// The text of the Sync cell's tag: "Online · Up to date" when all is well, else just whether the agent is online.
fn sync_tag(status: &Status) -> (String, Tone) {
    match (status.online, &status.run_state) {
        (true, RunState::Idle) => ("Online · Up to date".to_owned(), Tone::Good),
        (true, _) => ("Online".to_owned(), Tone::Good),
        (false, _) => ("Offline".to_owned(), Tone::Neutral),
    }
}

fn sync_body(ui: &mut egui::Ui, o: &Overview, now_ms: i64, autostart_problem: Option<&str>, events: &mut Vec<Event>) {
    let status = &o.status;
    let t = Tokens::get(ui.ctx());
    let (tag, tone) = sync_tag(status);
    super::kit::tag(ui, &tag, tone);
    let (line, details) = sync_line(status);
    match details {
        Some(details) if matches!(status.run_state, RunState::Paused { .. }) => super::problem::line_with_details(ui, &line, &details),
        Some(details) => {
            ui.add(egui::Label::new(super::kit::headline_text(&line, 28.0).color(t.ink_strong)).wrap());
            egui::CollapsingHeader::new("Details").id_salt("sync-details").show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(details).monospace().small()).selectable(true).wrap());
            });
        }
        None => {
            ui.add(egui::Label::new(super::kit::headline_text(&line, 28.0).color(t.ink_strong)).wrap());
        }
    }
    if let Some(at) = status.last_sync_ms {
        ui.label(RichText::new(format!("Last sync {}", super::ago(now_ms, at))).color(t.muted));
    }
    if let Some(conflicts) = o.conflicts.filter(|c| c.count > 0) {
        let link = super::kit::text_link(ui, &format!("{} ↗", conflicts_text(conflicts.count, conflicts.truncated)), 12.0);
        if link.on_hover_text("Files that look like the copy kept when two computers changed a file at once").clicked() {
            events.push(Event::OpenConflicts);
        }
    }
    if let Some(folder) = &status.folder {
        // The folder's well: its icon and the path on one line, cut off with an ellipsis (the whole path on hover).
        egui::Frame::new().fill(t.sunken).stroke(t.hairline()).inner_margin(egui::Margin::symmetric(12, 10)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                super::kit::item_icon(ui, true, 16.0);
                ui.add(egui::Label::new(RichText::new(folder).font(egui::FontId::new(13.0, egui::FontFamily::Monospace))).truncate());
            });
        });
    }
    if let Some(problem) = autostart_problem {
        // Compact: the first sentence, the whole text under Details, and a way to try again.
        super::problem::line_with_details(ui, &super::problem_line(problem), problem);
        if super::kit::text_link(ui, "Try again", 11.0).clicked() {
            events.push(Event::RetryAutostart);
        }
    }
}

fn sync_footer(ui: &mut egui::Ui, o: &Overview, busy: Option<Busy>, events: &mut Vec<Event>) {
    const PAUSE_W: f32 = 110.0;
    ui.horizontal(|ui| {
        let open_w = (ui.available_width() - PAUSE_W - ui.spacing().item_spacing.x).max(80.0);
        if ui.add(super::kit::Button::new("Open folder").primary().min_size(egui::vec2(open_w, 0.0))).clicked() {
            events.push(Event::OpenFolder);
        }
        let paused = o.status.run_state == RunState::PausedByUser;
        let (text, event) = if paused { ("Resume", Event::Resume) } else { ("Pause", Event::Pause) };
        if ui.add_enabled(busy.is_none(), super::kit::Button::new(text).min_size(egui::vec2(PAUSE_W, 0.0))).clicked() {
            events.push(event);
        }
    });
}

fn storage_body(ui: &mut egui::Ui, o: &Overview, events: &mut Vec<Event>) {
    if o.usage_loading {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Loading storage usage…");
        });
    } else if let Some(error) = &o.usage_error {
        super::error_line(ui, "Storage unavailable");
        ui.label(RichText::new("Details").small().weak()).on_hover_text(error);
    } else if let Some(usage) = &o.usage {
        usage_lines(ui, usage);
    }
    let text = if o.folders.open { "Hide folders and kinds" } else { "On this computer ↗" };
    if super::kit::text_link(ui, text, 12.0).clicked() {
        events.push(Event::ToggleFolders);
    }
}

/// What a computer's line says under its name: `agent · windows · 0.1.0`, a dash for what the server doesn't say.
fn device_meta(device: &craft_cloud_app::DeviceInfo) -> String {
    let part = |text: &str| if text.trim().is_empty() { "—".to_owned() } else { text.to_owned() };
    format!("{} · {} · {}", part(&device.client_kind), part(&device.platform), part(&device.app_version))
}

fn computers_body(ui: &mut egui::Ui, o: &Overview, now_ms: i64, events: &mut Vec<Event>) {
    let t = Tokens::get(ui.ctx());
    if let Some(error) = &o.devices_error {
        super::error_line(ui, error);
    }
    match &o.devices {
        None if o.devices_error.is_none() => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Asking the server…");
            });
        }
        None => {}
        Some(devices) => {
            let active: Vec<_> = devices.iter().filter(|d| !d.revoked).collect();
            ui.spacing_mut().item_spacing.y = 0.0;
            for device in active.iter().take(SHORT_LIST) {
                super::kit::table_row(ui, |ui| {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            ui.label(RichText::new(&device.name).strong());
                            if device.current {
                                ui.label(RichText::new("(this computer)").color(t.muted));
                            }
                        });
                        super::kit::caption(ui, &device_meta(device));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if device.current {
                            if o.status.online {
                                super::kit::tag(ui, "Online", Tone::Good);
                            } else {
                                super::kit::tag(ui, "Offline", Tone::Neutral);
                            }
                        } else {
                            ui.label(RichText::new(format!("seen {}", super::ago(now_ms, device.last_used_ms))).small().color(t.muted));
                        }
                    });
                });
            }
            if active.len() > SHORT_LIST && ui.link(format!("{} more", active.len() - SHORT_LIST)).clicked() {
                events.push(Event::Tab(Tab::Computers));
            }
        }
    }
}

/// "02 / Recent activity": a label row with a link to the Activity tab, and the newest events in a When / What / Computer
/// table; a muted line says when there are none or they couldn't be loaded.
fn recent_activity(ui: &mut egui::Ui, o: &Overview, now_ms: i64, events: &mut Vec<Event>) {
    let t = Tokens::get(ui.ctx());
    let names = super::names(ui);
    super::kit::label_row(ui, &super::kit::numbered(if o.folders.open { 3 } else { 2 }, "Recent activity"), |ui| {
        if super::kit::text_link(ui, "All activity ↗", 12.0).clicked() {
            events.push(Event::Tab(Tab::Activity));
        }
        if super::kit::text_link(ui, "Recent files ↗", 12.0).clicked() {
            events.push(Event::Tab(Tab::Files));
        }
    });
    ui.add_space(8.0);
    match &o.recent {
        Recent::Loading => {
            ui.label(RichText::new("Loading recent activity…").color(t.muted));
        }
        Recent::Failed(_) => {
            ui.label(RichText::new("Recent activity isn't available right now.").color(t.muted));
        }
        Recent::Loaded(list) if list.is_empty() => {
            ui.label(RichText::new("Nothing was recorded yet.").color(t.muted));
        }
        Recent::Loaded(list) => {
            for edit in super::activity_filter::controls(ui, "recent", &o.recent_filter, list, &names) {
                events.push(Event::RecentFilter(edit));
            }
            let visible = o.recent_filter.visible(list);
            if visible.is_empty() {
                if let Some(edit) = super::activity_filter::nothing_matches(ui) {
                    events.push(Event::RecentFilter(edit));
                }
                return;
            }
            let columns = [Column::fixed("When", 160.0), Column::flex("What", 240.0), Column::fixed("Computer", 160.0)];
            super::kit::table(ui, "recent-activity", &columns, visible.len(), ACTIVITY_ROW_H, |ui, row, column| {
                let Some(event) = visible.get(row).copied() else { return };
                match column {
                    0 => {
                        ui.label(RichText::new(super::ago(now_ms, event.at_ms)).monospace());
                    }
                    1 => {
                        let text = RichText::new(action_label(&event.action));
                        let text = if event.outcome == "failure" { text.color(t.bad) } else { text };
                        super::kit::truncated(ui, text);
                    }
                    _ => match event.actor_device {
                        Some(device) => {
                            let computer = names.computer(device);
                            let text = RichText::new(&computer.text).color(t.muted);
                            ui.add(egui::Label::new(text).truncate().selectable(false)).on_hover_text(&computer.full);
                        }
                        None => {
                            ui.label(RichText::new("—").color(t.muted));
                        }
                    },
                }
            });
        }
    }
}

fn usage_lines(ui: &mut egui::Ui, usage: &AccountUsage) {
    let t = Tokens::get(ui.ctx());
    let total = displayed_total_bytes(usage);
    let (amount, unit) = size_parts(total);
    // The big figure with its unit and the limit, on one baseline.
    ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), 50.0), egui::Layout::left_to_right(egui::Align::Max), |ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        ui.label(super::kit::headline_text(&amount, 40.0).color(t.ink_strong));
        ui.label(super::kit::headline_text(&unit, 20.0).color(t.ink_strong));
        match usage.quota_bytes {
            Some(quota) => ui.label(RichText::new(format!("of {}", format_bytes(quota))).color(t.muted)),
            None => ui.label(RichText::new("used · no limit").color(t.muted)),
        };
    });
    if let Some(quota) = usage.quota_bytes {
        let share = if quota == 0 { 0.0 } else { (total as f64 / quota as f64).min(1.0) as f32 };
        // Painted here: egui's progress bar takes the (translucent) selection colour.
        let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 6.0), egui::Sense::hover());
        ui.painter().rect_filled(rect, 0.0, t.line);
        ui.painter().rect_filled(egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * share, rect.height())), 0.0, t.accent);
    }
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        row(ui, "Your files", &format_bytes(total), false);
        if let (true, Some(trash), Some(history)) = (usage.estimates.index_complete, usage.trash_bytes, usage.history_bytes) {
            row(ui, "In the trash", &format!("about {}", format_bytes(trash)), true);
            row(ui, "Old versions", &format!("about {}", format_bytes(history)), true);
        }
    });
    if usage.estimates.index_complete && usage.trash_bytes.is_some() && usage.history_bytes.is_some() {
        ui.label(RichText::new("Trash and old versions are shown apart, not added to the total.").size(12.0).color(t.muted))
            .on_hover_text("Trash and old-version estimates overlap; they are not added to stored bytes.");
    } else {
        ui.label("Estimates not available yet");
    }
}

/// A label at the left and a value at the right, over a hairline.
fn row(ui: &mut egui::Ui, label: &str, value: &str, muted: bool) {
    let t = Tokens::get(ui.ctx());
    let color = if muted { t.muted } else { t.ink };
    super::kit::table_row(ui, |ui| {
        ui.label(RichText::new(label).color(color));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(value).monospace().color(color));
        });
    });
}

fn displayed_total_bytes(usage: &AccountUsage) -> u64 {
    usage.stored_bytes
}

/// A size as a number and its unit, for the big figure: ("1.3", "GiB"); no forced decimal ("5", "GiB").
fn size_parts(value: u64) -> (String, String) {
    let text = format_bytes(value);
    match text.split_once(' ') {
        Some((amount, unit)) => (amount.to_owned(), unit.to_owned()),
        None => (text, String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(online: bool, run_state: RunState) -> Status {
        Status {
            api: 1,
            signed_in: true,
            account: Some("fabio".into()),
            server: Some("https://cloud.shadows.ch".into()),
            folder: Some("/home/fabio/Craft Cloud".into()),
            machine: Some("lptp1".into()),
            online,
            run_state,
            autostart: Some(true),
            last_sync_ms: None,
            removed: false,
            spaces: Vec::new(),
            from_folder: None,
        }
    }

    #[test]
    fn trash_and_history_are_never_added_to_the_stored_total() {
        let usage = AccountUsage {
            stored_bytes: 11,
            quota_bytes: None,
            trash_bytes: Some(20),
            history_bytes: Some(30),
            estimates: craft_cloud_proto::api::UsageEstimates { index_complete: true, reason: None },
        };
        assert_eq!(displayed_total_bytes(&usage), 11);
    }

    #[test]
    fn sizes_read_as_a_number_and_a_unit_without_a_forced_decimal() {
        assert_eq!(size_parts(11), ("11".to_owned(), "B".to_owned()));
        assert_eq!(size_parts(1_395_864_371), ("1.3".to_owned(), "GiB".to_owned()));
        assert_eq!(size_parts(5 * 1_073_741_824), ("5".to_owned(), "GiB".to_owned()));
        assert_eq!(format_bytes(119 * 1_048_576), "119 MiB");
    }

    #[test]
    fn a_computer_line_has_a_dash_for_what_is_missing() {
        let mut device = craft_cloud_app::DeviceInfo {
            id: craft_cloud_app::DeviceId::generate(),
            name: "lptp1".into(),
            created_ms: 0,
            last_used_ms: 0,
            revoked: false,
            current: false,
            client_kind: "agent".into(),
            platform: "windows".into(),
            app_version: "0.1.0".into(),
            revoked_ms: None,
            revoke_reason: None,
        };
        assert_eq!(device_meta(&device), "agent · windows · 0.1.0");
        device.platform.clear();
        device.app_version = "  ".into();
        assert_eq!(device_meta(&device), "agent · — · —");
    }

    #[test]
    fn a_full_server_is_said_as_the_servers_not_the_accounts() {
        let words = "server: The server is full; the admin needs to free space or raise its capacity";
        assert!(words.contains(SERVER_FULL_WORDS));
        // From an agent with the code, and from an older one that has only the words.
        for reason_code in [Some("server_full".to_owned()), None] {
            let full = RunState::Paused { reason: words.into(), reason_code };
            let (line, details) = sync_line(&status(true, full));
            assert!(line.starts_with("The server is full") && !line.contains("account") && !line.contains("quota"), "{line}");
            assert!(details.is_some_and(|d| d.contains("raise its capacity")));
        }
        let quota_words = "server: the account's storage on the server is full or nearly (10 of 10 bytes used); the admin can allow more";
        for reason_code in [Some("quota_exceeded".to_owned()), None] {
            let quota = RunState::Paused { reason: quota_words.into(), reason_code };
            assert!(sync_line(&status(true, quota)).0.contains("account"));
        }
    }

    /// The code decides when there is one: its words may change, and a code that is not `server_full` is not it.
    #[test]
    fn the_code_is_matched_before_the_words() {
        assert!(server_is_full("whatever the agent says now", Some("server_full")));
        assert!(!server_is_full("The server is full; the admin needs to free space", Some("quota_exceeded")));
        assert!(!server_is_full("The server is full; the admin needs to free space", Some("something_new")));
        assert!(server_is_full("server: The server is full; the admin needs to free space", None));
        assert!(!server_is_full("the folder is missing", None));
    }

    #[test]
    fn conflict_copies_are_counted_in_words() {
        assert_eq!(conflicts_text(1, false), "1 conflict copy");
        assert_eq!(conflicts_text(3, false), "3 conflict copies");
        assert_eq!(conflicts_text(1000, true), "1000+ conflict copies");
    }

    #[test]
    fn the_sync_tag_says_the_state() {
        assert_eq!(sync_tag(&status(true, RunState::Idle)), ("Online · Up to date".to_owned(), Tone::Good));
        assert_eq!(sync_tag(&status(false, RunState::Idle)).0, "Offline");
        assert_eq!(sync_tag(&status(true, RunState::Syncing)).0, "Online");
    }
}
