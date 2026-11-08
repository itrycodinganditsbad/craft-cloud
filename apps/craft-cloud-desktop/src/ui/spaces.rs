//! The Shared tab: the team spaces of the account (and the invitations to them), and one space's page with its folder, its sync
//! switch, what it stores, held deletions and its members. The state and the rules are [`crate::spaces`]; this file only draws
//! it and sends what the buttons ask for.

use craft_cloud_app::{SpaceInfo, SpaceMember, SpaceRole, SpaceRun, SpaceSync};
use egui::RichText;

use super::confirm;
use super::kit::{self, Column, Tokens, Tone};
use crate::model::Event;
use crate::spaces::{
    ARCHIVED_BANNER, Deletes, Doing, Look, NO_ACCESS_BANNER, READ_ONLY_BANNER, SharedScreen, SpacePage, SpacesMsg, SyncOffer, TOO_OLD, Used,
    folder_line, role_label, space_name, state_tags,
};

/// The height of a row of the spaces table, and of the members table.
const ROW_H: f32 = 64.0;
const MEMBER_ROW_H: f32 = 56.0;
/// The widest the panels of a space's page get.
const PAGE_W: f32 = 760.0;

fn send(events: &mut Vec<Event>, msg: SpacesMsg) {
    events.push(Event::Spaces(msg));
}

fn tone(look: Look) -> Tone {
    match look {
        Look::Good => Tone::Good,
        Look::Warn => Tone::Warn,
        Look::Bad => Tone::Bad,
        Look::Neutral => Tone::Neutral,
    }
}

pub(super) fn show(ui: &mut egui::Ui, shared: &mut SharedScreen, now_ms: i64, events: &mut Vec<Event>) {
    if shared.too_old {
        super::heading(ui, "Shared");
        too_old(ui);
        return;
    }
    let me = shared.overview.status.account.clone().unwrap_or_default();
    match shared.page().map(|p| p.space) {
        None => list(ui, shared, events),
        Some(space) => {
            let Some(info) = shared.find(space).cloned() else {
                super::heading(ui, "Shared");
                ui.label("That space isn't in the list any more.");
                return;
            };
            let used = shared.usage_of(space);
            let owner = shared.is_owner(space);
            let offer = shared.offer.clone().filter(|o| o.space == space);
            let busy = shared.doing.clone();
            let notice = shared.notice.clone();
            let Some(page) = shared.page_mut() else { return };
            space_page(ui, &info, page, &used, owner, offer.as_ref(), busy.as_ref(), notice.as_deref(), &me, now_ms, events);
        }
    }
}

/// What an agent that doesn't know team spaces gets: one line, and nothing to click.
fn too_old(ui: &mut egui::Ui) {
    let t = Tokens::get(ui.ctx());
    kit::card(ui, Some("Team spaces"), |ui| {
        ui.add(egui::Label::new(RichText::new(TOO_OLD).color(t.ink_strong)).wrap());
        ui.add_space(4.0);
        ui.add(
            egui::Label::new(
                RichText::new("The Craft Cloud program on this computer is older than this window. Install the same version of both.")
                    .small()
                    .color(t.muted),
            )
            .wrap(),
        );
    });
}

fn busy_line(ui: &mut egui::Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.spinner();
        ui.label(text);
    });
}

/// The list: invitations at the top, then the team spaces.
fn list(ui: &mut egui::Ui, shared: &SharedScreen, events: &mut Vec<Event>) {
    let t = Tokens::get(ui.ctx());
    super::heading(ui, "Shared");
    if let Some(notice) = &shared.notice {
        ui.horizontal(|ui| {
            ui.label(RichText::new(notice).color(super::good(ui)));
            if kit::text_link(ui, "OK", 11.0).clicked() {
                send(events, SpacesMsg::DismissNotice);
            }
        });
        ui.add_space(6.0);
    }
    if let Some(error) = &shared.error_action {
        super::error_line(ui, error);
        ui.add_space(6.0);
    }
    let invitations = shared.invitations();
    let joined = shared.joined();
    if !invitations.is_empty() {
        kit::label_row(ui, &kit::numbered(1, "Invitations"), |_| {});
        ui.add_space(6.0);
        for info in &invitations {
            invitation(ui, info, shared.doing.as_ref(), events);
            ui.add_space(8.0);
        }
        ui.add_space(8.0);
    }
    if let Some(offer) = shared.offer.as_ref() {
        offer_panel(ui, offer, events);
        ui.add_space(12.0);
    }
    let number = if invitations.is_empty() { 1 } else { 2 };
    kit::label_row(ui, &kit::numbered(number, "Team spaces"), |ui| {
        if ui.add_enabled(!shared.loading, kit::Button::new("Refresh")).clicked() {
            send(events, SpacesMsg::Reload);
        }
    });
    ui.add_space(8.0);
    if shared.loading && shared.spaces.is_none() {
        busy_line(ui, "Asking Craft Cloud on this computer…");
        return;
    }
    if let Some(error) = &shared.error {
        super::error_line(ui, &format!("The team spaces aren't available right now ({error})."));
        return;
    }
    if joined.is_empty() {
        let text = if invitations.is_empty() {
            "No team spaces yet. A team space is a folder you share with other people on your Craft Cloud. When someone invites you to one, the invitation shows up here."
        } else {
            "No team spaces yet. Accept an invitation above to join one."
        };
        ui.add(egui::Label::new(RichText::new(text).color(t.muted)).wrap());
        return;
    }
    let columns = [
        Column::flex("Space", 120.0).weighted(2.0),
        Column::fixed("Role", 76.0),
        Column::fixed("Sync", 66.0),
        Column::flex("State", 120.0).weighted(1.2),
        Column::flex("Used / limit", 110.0),
        Column::fixed("", 80.0),
    ];
    kit::table(ui, "shared-spaces", &columns, joined.len(), ROW_H, |ui, row, column| {
        let Some(info) = joined.get(row) else { return };
        match column {
            0 => {
                let line = folder_line(info);
                kit::two_lines(ui, ROW_H, &space_name(info), (!line.is_empty()).then_some(line.as_str()));
            }
            1 => {
                kit::tag(ui, role_label(info.role), if info.role == SpaceRole::Owner { Tone::Accent } else { Tone::Neutral });
            }
            2 => {
                // A space the account lost, or one the server can't do yet, doesn't sync here either: the State column says why.
                match info.sync {
                    SpaceSync::On => kit::tag(ui, "On", Tone::Good),
                    SpaceSync::Off | SpaceSync::Unavailable => kit::tag(ui, "Off", Tone::Neutral),
                };
            }
            3 => {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(6.0, 4.0);
                    // "Not syncing here" is the Sync column's Off.
                    for (label, look) in state_tags(info).into_iter().filter(|(label, _)| *label != "Not syncing here") {
                        kit::tag(ui, label, tone(look));
                    }
                });
            }
            4 => used_cell(ui, &shared.usage_of(info.space)),
            _ => {
                if ui.add(kit::Button::new("Open")).clicked() {
                    send(events, SpacesMsg::OpenSpace(info.space));
                }
            }
        }
    });
}

/// `1.2 GiB of 10 GiB` over a thin bar, or a dash, as one block centred in the row.
fn used_cell(ui: &mut egui::Ui, used: &Used) {
    let t = Tokens::get(ui.ctx());
    let width = ui.available_width();
    let line = 18.0;
    let block = if used.room().is_some() { line + 6.0 + 4.0 } else { line };
    ui.vertical(|ui| {
        ui.set_width(width);
        ui.spacing_mut().item_spacing.y = 6.0;
        // (A space is followed by the item spacing too.)
        ui.add_space((((ROW_H - block) / 2.0) - 6.0).max(0.0).floor());
        ui.add(egui::Label::new(RichText::new(used.text()).monospace().color(t.ink)).truncate());
        if let Some(room) = used.room() {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(width.max(20.0), 4.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, egui::CornerRadius::ZERO, t.line);
            let fill = if room.full() {
                t.bad
            } else if room.near() {
                t.warn
            } else {
                t.accent
            };
            let filled = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * room.fraction(), rect.height()));
            ui.painter().rect_filled(filled, egui::CornerRadius::ZERO, fill);
        }
    });
}

/// One invitation: what it is for and the two answers.
fn invitation(ui: &mut egui::Ui, info: &SpaceInfo, doing: Option<&Doing>, events: &mut Vec<Event>) {
    let busy = doing.is_some();
    let mine = matches!(doing, Some(Doing::Accepting(s) | Doing::Declining(s)) if *s == info.space);
    let name = space_name(info);
    let text = format!("You're invited to {name} as {}", info.role.as_str());
    kit::card(ui, None, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            kit::tag(ui, "Invitation", Tone::Accent);
            ui.label(RichText::new(text).strong());
        });
        ui.add_space(4.0);
        ui.add(egui::Label::new(RichText::new(role_explanation(info.role)).small().weak()).wrap());
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if mine {
                ui.spinner();
            }
            if ui.add_enabled(!busy, kit::Button::new("Accept").primary()).clicked() {
                send(events, SpacesMsg::Accept(info.space));
            }
            if ui.add_enabled(!busy, kit::Button::new("Decline")).clicked() {
                send(events, SpacesMsg::Decline(info.space));
            }
        });
    });
}

/// What a role may do, in a sentence.
fn role_explanation(role: SpaceRole) -> &'static str {
    match role {
        SpaceRole::Owner => "Owners change files, invite and remove people and change their roles.",
        SpaceRole::Editor => "Editors read and change the files of the space.",
        SpaceRole::Viewer => "Viewers read the files of the space; they can't change them.",
    }
}

/// "Sync to this computer": the folder the space would use, a choice of another one, and the buttons. The space stays off until
/// Sync is clicked.
fn offer_panel(ui: &mut egui::Ui, offer: &SyncOffer, events: &mut Vec<Event>) {
    let t = Tokens::get(ui.ctx());
    let title = if offer.after_accept { format!("Joined {}", offer.name) } else { format!("Sync {} to this computer", offer.name) };
    kit::card(ui, Some(&title), |ui| {
        ui.spacing_mut().item_spacing.y = 6.0;
        let lead = if offer.after_accept {
            "You joined the space. It doesn't sync to this computer until you turn it on."
        } else {
            "The space's files are copied into a folder on this computer and kept up to date."
        };
        ui.add(egui::Label::new(lead).wrap());
        kit::section_label(ui, "Folder");
        match &offer.folder {
            Some(folder) => {
                ui.add(egui::Label::new(RichText::new(folder).monospace().color(t.ink_strong)).wrap());
            }
            None => {
                ui.label(RichText::new("Craft Cloud picks a folder under Craft Cloud Shared").color(t.muted));
            }
        }
        if let Some(error) = &offer.error {
            super::error_line(ui, error);
        }
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if offer.busy {
                ui.spinner();
            }
            if offer.asks_adopt && !offer.adopt {
                if ui.add_enabled(!offer.busy, kit::Button::new("Sync anyway").primary()).clicked() {
                    send(events, SpacesMsg::OfferAdopt);
                }
            } else if ui.add_enabled(!offer.busy, kit::Button::new("Sync").primary()).clicked() {
                send(events, SpacesMsg::OfferSync);
            }
            if ui.add_enabled(!offer.busy, kit::Button::new("Choose another folder…")).clicked() {
                send(events, SpacesMsg::OfferChoose);
            }
            if ui.add_enabled(!offer.busy, kit::Button::new("Not now")).clicked() {
                send(events, SpacesMsg::OfferSkip);
            }
        });
    });
}

/// A banner across the page: a short title in a tag and what it means.
fn banner(ui: &mut egui::Ui, label: &str, look: Look, text: &str) {
    let t = Tokens::get(ui.ctx());
    kit::card(ui, None, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            kit::tag(ui, label, tone(look));
            ui.add(egui::Label::new(RichText::new(text).color(t.ink)).wrap());
        });
    });
    ui.add_space(8.0);
}

#[allow(clippy::too_many_arguments)]
fn space_page(
    ui: &mut egui::Ui,
    info: &SpaceInfo,
    page: &mut SpacePage,
    used: &Used,
    owner: bool,
    offer: Option<&SyncOffer>,
    doing: Option<&Doing>,
    notice: Option<&str>,
    me: &str,
    now_ms: i64,
    events: &mut Vec<Event>,
) {
    let name = space_name(info);
    let lost = matches!(info.run_state, Some(SpaceRun::NoAccess { .. }));
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        if kit::text_link(ui, "Shared", 12.0).on_hover_text("Back to the team spaces").clicked() {
            send(events, SpacesMsg::BackToList);
        }
        kit::section_label(ui, &format!("/ {name}"));
    });
    ui.add_space(4.0);
    title_row(ui, info, page, &name, owner, doing, events);
    ui.add_space(12.0);
    if let Some(notice) = notice {
        ui.horizontal(|ui| {
            ui.label(RichText::new(notice).color(super::good(ui)));
            if kit::text_link(ui, "OK", 11.0).clicked() {
                send(events, SpacesMsg::DismissNotice);
            }
        });
        ui.add_space(6.0);
    }
    if let Some(error) = &page.error {
        super::error_line(ui, error);
        ui.add_space(6.0);
    }
    // The confirmations share one focus slot: with none open the next one to open takes the focus.
    if page.removing.is_none() && !page.leaving && !matches!(page.deletes, Deletes::Asking { .. }) {
        confirm::idle(ui);
    }
    ui.scope(|ui| {
        ui.set_max_width(PAGE_W);
        if lost {
            banner(ui, "No access", Look::Bad, NO_ACCESS_BANNER);
        }
        if info.archived {
            banner(ui, "Archived", Look::Neutral, ARCHIVED_BANNER);
        }
        if !lost && (info.read_only || info.role == SpaceRole::Viewer) && !info.archived {
            banner(ui, "Read only", Look::Warn, READ_ONLY_BANNER);
        }
        files_panel(ui, info, used, offer, doing, lost, now_ms, events);
        if info.sync == SpaceSync::On && !info.read_only && !lost {
            ui.add_space(14.0);
            deletes_panel(ui, page, doing, events);
        }
        if !lost {
            ui.add_space(14.0);
            members_panel(ui, info, page, owner, doing, me, events);
            ui.add_space(14.0);
            leave_panel(ui, page, &name, info, doing, events);
        }
    });
}

/// The name, the role and state tags, and (for an owner) Rename.
fn title_row(ui: &mut egui::Ui, info: &SpaceInfo, page: &mut SpacePage, name: &str, owner: bool, doing: Option<&Doing>, events: &mut Vec<Event>) {
    if let Some(text) = &mut page.rename {
        ui.horizontal(|ui| {
            let busy = matches!(doing, Some(Doing::Renaming));
            let field = ui.add_enabled(!busy, kit::text_edit_large(text).desired_width(280.0));
            let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.add_enabled(!busy, kit::Button::new("Save").primary()).clicked() || (enter && !busy) {
                send(events, SpacesMsg::SaveRename);
            }
            if ui.add_enabled(!busy, kit::Button::new("Cancel")).clicked() {
                send(events, SpacesMsg::CancelRename);
            }
            if busy {
                ui.spinner();
            }
        });
    } else {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            kit::headline_large(ui, name);
            kit::tag(ui, role_label(info.role), if info.role == SpaceRole::Owner { Tone::Accent } else { Tone::Neutral });
            for (label, look) in state_tags(info) {
                kit::tag(ui, label, tone(look));
            }
            let open = !info.archived && !matches!(info.run_state, Some(SpaceRun::NoAccess { .. }));
            if owner && open && ui.add(kit::Button::new("Rename")).clicked() {
                send(events, SpacesMsg::StartRename);
            }
        });
    }
}

/// What the space stores, where it is on this computer and whether it syncs.
#[allow(clippy::too_many_arguments)]
fn files_panel(
    ui: &mut egui::Ui,
    info: &SpaceInfo,
    used: &Used,
    offer: Option<&SyncOffer>,
    doing: Option<&Doing>,
    lost: bool,
    now_ms: i64,
    events: &mut Vec<Event>,
) {
    let t = Tokens::get(ui.ctx());
    kit::card(ui, Some(&kit::numbered(1, "Files")), |ui| {
        ui.spacing_mut().item_spacing.y = 8.0;
        kit::section_label(ui, "Stored");
        used_summary(ui, used);
        if let Some(last) = info.last_sync_ms {
            ui.label(RichText::new(format!("Last synced {}", super::ago(now_ms, last))).small().color(t.muted));
        }
        ui.add_space(4.0);
        kit::section_label(ui, "On this computer");
        match (&info.folder, info.sync) {
            (Some(folder), SpaceSync::On) => {
                ui.add(egui::Label::new(RichText::new(folder).monospace().color(t.ink_strong)).wrap());
            }
            (Some(folder), _) => {
                ui.add(egui::Label::new(RichText::new(folder).monospace().color(t.muted)).wrap());
                ui.label(
                    RichText::new(if lost {
                        "Kept as it is; it no longer syncs."
                    } else {
                        "Not syncing here: nothing is copied until you turn it on."
                    })
                    .small()
                    .color(t.muted),
                );
            }
            (None, _) => {
                ui.label(RichText::new("No folder yet").color(t.muted));
            }
        }
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let has_folder = info.folder.as_deref().is_some_and(|f| !f.is_empty());
            if has_folder && ui.add(kit::Button::new("Open folder")).clicked() {
                send(events, SpacesMsg::OpenFolder(info.space));
            }
            if lost {
                return;
            }
            let syncing_call = matches!(doing, Some(Doing::Syncing(s)) if *s == info.space);
            match info.sync {
                SpaceSync::On => {
                    if ui.add_enabled(doing.is_none(), kit::Button::new("Stop syncing here")).on_hover_text("The folder and its files stay").clicked()
                    {
                        send(events, SpacesMsg::SyncOff(info.space));
                    }
                }
                SpaceSync::Off => {
                    if offer.is_none() && ui.add_enabled(doing.is_none(), kit::Button::new("Sync on this computer").primary()).clicked() {
                        send(events, SpacesMsg::SyncOn(info.space));
                    }
                }
                SpaceSync::Unavailable => {}
            }
            if syncing_call && offer.is_none() {
                ui.spinner();
            }
        });
        if let Some(offer) = offer {
            ui.add_space(4.0);
            offer_inline(ui, offer, events);
        }
    });
}

/// The offer inside the files panel: the folder (chosen or the agent's), a choice of another, Sync and Not now.
fn offer_inline(ui: &mut egui::Ui, offer: &SyncOffer, events: &mut Vec<Event>) {
    let t = Tokens::get(ui.ctx());
    kit::hairline(ui);
    ui.add_space(6.0);
    kit::section_label(ui, "Sync to this computer");
    match &offer.folder {
        Some(folder) => {
            ui.add(egui::Label::new(RichText::new(folder).monospace().color(t.ink_strong)).wrap());
        }
        None => {
            ui.label(RichText::new("Craft Cloud picks a folder under Craft Cloud Shared").color(t.muted));
        }
    }
    if let Some(error) = &offer.error {
        super::error_line(ui, error);
    }
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        if offer.busy {
            ui.spinner();
        }
        if offer.asks_adopt && !offer.adopt {
            if ui.add_enabled(!offer.busy, kit::Button::new("Sync anyway").primary()).clicked() {
                send(events, SpacesMsg::OfferAdopt);
            }
        } else if ui.add_enabled(!offer.busy, kit::Button::new("Sync").primary()).clicked() {
            send(events, SpacesMsg::OfferSync);
        }
        if ui.add_enabled(!offer.busy, kit::Button::new("Choose another folder…")).clicked() {
            send(events, SpacesMsg::OfferChoose);
        }
        if ui.add_enabled(!offer.busy, kit::Button::new("Not now")).clicked() {
            send(events, SpacesMsg::OfferSkip);
        }
    });
}

/// `1.2 GiB of 10 GiB` and a bar.
fn used_summary(ui: &mut egui::Ui, used: &Used) {
    let t = Tokens::get(ui.ctx());
    ui.label(RichText::new(used.text()).monospace().size(16.0).color(t.ink_strong));
    if let Some(room) = used.room() {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width().max(20.0), 6.0), egui::Sense::hover());
        ui.painter().rect_filled(rect, egui::CornerRadius::ZERO, t.line);
        let fill = if room.full() {
            t.bad
        } else if room.near() {
            t.warn
        } else {
            t.accent
        };
        let filled = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * room.fraction(), rect.height()));
        ui.painter().rect_filled(filled, egui::CornerRadius::ZERO, fill);
        if room.full() {
            ui.label(RichText::new("The space is full: uploads are refused until something is removed or the limit is raised.").small().color(t.bad));
        } else if room.near() {
            ui.label(RichText::new(format!("{} % used", room.percent())).small().color(t.warn));
        }
    }
}

/// Held mass deletions: "Approve deletes" looks at what the agent holds back, lists it, and asks before approving.
fn deletes_panel(ui: &mut egui::Ui, page: &SpacePage, doing: Option<&Doing>, events: &mut Vec<Event>) {
    let t = Tokens::get(ui.ctx());
    kit::card(ui, Some(&kit::numbered(2, "Held deletes")), |ui| {
        ui.spacing_mut().item_spacing.y = 6.0;
        ui.add(
            egui::Label::new(
                RichText::new("When a lot of files are deleted at once on one computer, Craft Cloud holds the deletions back until you approve them. Nothing is lost while they wait.")
                    .small()
                    .color(t.muted),
            )
            .wrap(),
        );
        let checking = matches!(doing, Some(Doing::CheckingDeletes));
        let approving = matches!(doing, Some(Doing::ApprovingDeletes));
        match &page.deletes {
            Deletes::Asking { lines, .. } => {
                let count = lines.len();
                for line in lines.iter().take(5) {
                    ui.label(RichText::new(line).monospace().small().color(t.ink));
                }
                if count > 5 {
                    ui.label(RichText::new(format!("… and {} more", count - 5)).small().color(t.muted));
                }
                ui.add_space(4.0);
                let text = if count == 1 {
                    "1 deletion is held back. Approve it?".to_owned()
                } else {
                    format!("{count} deletions are held back. Approve them?")
                };
                let panel = confirm::Panel {
                    key: egui::Id::new(("approve-deletes", count)),
                    title: "Approve the held deletes?",
                    text: &text,
                    confirm: "Approve deletes",
                    cancel: "Keep them held",
                    danger: true,
                    enabled: !approving,
                };
                match confirm::show(ui, &panel) {
                    confirm::Answer::Confirmed => send(events, SpacesMsg::ConfirmDeletes),
                    confirm::Answer::Cancelled => send(events, SpacesMsg::CancelDeletes),
                    confirm::Answer::Open => {}
                }
                if approving {
                    busy_line(ui, "Approving…");
                }
                return;
            }
            Deletes::NoneHeld => {
                ui.label("No deletions are being held back.");
            }
            Deletes::Approved(count) => {
                let text = if *count == 1 {
                    "Approved: the deletion is carried out in a moment.".to_owned()
                } else {
                    format!("Approved: the {count} deletions are carried out in a moment.")
                };
                ui.label(RichText::new(text).color(super::good(ui)));
            }
            Deletes::Idle => {}
        }
        ui.horizontal(|ui| {
            if ui.add_enabled(doing.is_none(), kit::Button::new("Approve deletes")).clicked() {
                send(events, SpacesMsg::CheckDeletes);
            }
            if checking {
                busy_line(ui, "Looking…");
            }
        });
    });
}

/// The members with their roles; an owner invites, changes roles and removes.
fn members_panel(ui: &mut egui::Ui, info: &SpaceInfo, page: &mut SpacePage, owner: bool, doing: Option<&Doing>, me: &str, events: &mut Vec<Event>) {
    let t = Tokens::get(ui.ctx());
    let number = if info.sync == SpaceSync::On && !info.read_only { 3 } else { 2 };
    kit::card(ui, Some(&kit::numbered(number, "Members")), |ui| {
        ui.spacing_mut().item_spacing.y = 8.0;
        if page.members.loading && page.members.list.is_none() {
            busy_line(ui, "Loading the members…");
        }
        if let Some(error) = &page.members.error {
            super::error_line(ui, error);
        }
        if let Some(list) = &page.members.list {
            let list = list.clone();
            members_table(ui, &list, owner, doing, me, events);
        }
        if let Some(user) = page.removing {
            let who =
                page.members.list.iter().flatten().find(|m| m.user == user).map(|m| m.username.clone()).unwrap_or_else(|| "this person".to_owned());
            let text =
                format!("{who} can't open this space any more. They keep what they already downloaded to their computers; nothing there is deleted.");
            let busy = matches!(doing, Some(Doing::Removing(u)) if *u == user);
            let title = format!("Remove {who}?");
            let panel = confirm::Panel {
                key: egui::Id::new(("remove-member", user)),
                title: &title,
                text: &text,
                confirm: "Remove",
                cancel: "Keep",
                danger: true,
                enabled: !busy,
            };
            match confirm::show(ui, &panel) {
                confirm::Answer::Confirmed => send(events, SpacesMsg::ConfirmRemove),
                confirm::Answer::Cancelled => send(events, SpacesMsg::CancelRemove),
                confirm::Answer::Open => {}
            }
        }
        if owner {
            ui.add_space(4.0);
            kit::hairline(ui);
            ui.add_space(4.0);
            invite_form(ui, page, doing, events);
        } else {
            ui.label(RichText::new("Only owners invite people and change roles.").small().color(t.muted));
        }
    });
}

fn role_picker(ui: &mut egui::Ui, id: impl std::hash::Hash + std::fmt::Debug, role: &mut SpaceRole) -> bool {
    let before = *role;
    egui::ComboBox::from_id_salt(id).selected_text(role_label(*role)).width(110.0).show_ui(ui, |ui| {
        for choice in [SpaceRole::Viewer, SpaceRole::Editor, SpaceRole::Owner] {
            ui.selectable_value(role, choice, role_label(choice));
        }
    });
    *role != before
}

fn members_table(ui: &mut egui::Ui, members: &[SpaceMember], owner: bool, doing: Option<&Doing>, me: &str, events: &mut Vec<Event>) {
    let t = Tokens::get(ui.ctx());
    let columns = [Column::flex("Account", 100.0).weighted(2.0), Column::fixed("Role", 124.0), Column::fixed("State", 84.0), Column::fixed("", 96.0)];
    kit::table(ui, "space-members", &columns, members.len(), MEMBER_ROW_H, |ui, row, column| {
        let Some(member) = members.get(row) else { return };
        match column {
            0 => {
                kit::truncated(ui, RichText::new(&member.username).strong());
                if member.username == me {
                    ui.label(RichText::new("(you)").color(t.muted));
                }
            }
            1 => {
                if owner {
                    let mut role = member.role;
                    if role_picker(ui, ("member-role", member.user), &mut role) {
                        send(events, SpacesMsg::SetRole { user: member.user, role });
                    }
                } else {
                    kit::tag(ui, role_label(member.role), if member.role == SpaceRole::Owner { Tone::Accent } else { Tone::Neutral });
                }
            }
            2 => {
                if member.state == "invited" {
                    kit::tag(ui, "Invited", Tone::Warn);
                } else {
                    ui.label(RichText::new("Member").color(t.muted));
                }
            }
            _ => {
                if owner && ui.add_enabled(doing.is_none(), kit::Button::new("Remove")).clicked() {
                    send(events, SpacesMsg::AskRemove(member.user));
                }
            }
        }
    });
}

/// "Invite by account name": the name, the role and the button.
fn invite_form(ui: &mut egui::Ui, page: &mut SpacePage, doing: Option<&Doing>, events: &mut Vec<Event>) {
    kit::section_label(ui, "Invite by account name");
    let busy = matches!(doing, Some(Doing::Inviting));
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let field = ui.add_enabled(!busy, kit::text_edit(&mut page.invite.name).hint_text("ana").desired_width(200.0));
        let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let mut role = page.invite.role;
        if role_picker(ui, "invite-role", &mut role) {
            page.invite.role = role;
        }
        if ui.add_enabled(!busy, kit::Button::new("Invite").primary()).clicked() || (enter && !busy) {
            send(events, SpacesMsg::SubmitInvite);
        }
        if busy {
            ui.spinner();
        }
    });
    if let Some(error) = &page.invite.error {
        super::error_line(ui, error);
    }
}

/// Leave: any role may; the folder stays on this computer.
fn leave_panel(ui: &mut egui::Ui, page: &SpacePage, name: &str, info: &SpaceInfo, doing: Option<&Doing>, events: &mut Vec<Event>) {
    let t = Tokens::get(ui.ctx());
    kit::card(ui, Some("Leave"), |ui| {
        ui.spacing_mut().item_spacing.y = 6.0;
        if page.leaving {
            let busy = matches!(doing, Some(Doing::Leaving));
            let folder = info.folder.as_deref().filter(|f| !f.is_empty() && info.sync == SpaceSync::On);
            let text = match folder {
                Some(folder) => {
                    format!("You can't open {name} any more. The files in {folder} stay on this computer as a plain folder and stop syncing.")
                }
                None => format!("You can't open {name} any more. Nothing on this computer is deleted."),
            };
            let title = format!("Leave {name}?");
            let panel = confirm::Panel {
                key: egui::Id::new(("leave-space", page.space)),
                title: &title,
                text: &text,
                confirm: "Leave",
                cancel: "Stay",
                danger: true,
                enabled: !busy,
            };
            match confirm::show(ui, &panel) {
                confirm::Answer::Confirmed => send(events, SpacesMsg::ConfirmLeave),
                confirm::Answer::Cancelled => send(events, SpacesMsg::CancelLeave),
                confirm::Answer::Open => {}
            }
        } else {
            ui.add(
                egui::Label::new(RichText::new("You stop being a member. The files stay on this computer as a plain folder.").small().color(t.muted))
                    .wrap(),
            );
            if ui.add_enabled(doing.is_none(), kit::Button::new("Leave this space").danger()).clicked() {
                send(events, SpacesMsg::AskLeave);
            }
        }
    });
}
