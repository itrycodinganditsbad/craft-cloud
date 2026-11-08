//! The signed-in window's frame: the header (the wordmark, the account line and the tabs of the pages) and the footer (what
//! the agent is doing, Sign out and Quit). The page itself is drawn between them by [`super::show`].

use craft_cloud_app::Status;
use egui::{Align, Layout, Margin};

use super::kit::{self, Tokens};
use crate::model::{Busy, Event, Model, Tab};

/// What the header and the footer show, read off the model before the page borrows it.
pub(super) struct Shell {
    tab: Tab,
    tabs: Vec<Tab>,
    /// The tabs can't be used now: a call or a one-time code of the page would be lost.
    locked: bool,
    /// `fabio`, or `Signed in` when the agent's status isn't known yet.
    account: String,
    /// `cloud.shadows.ch`.
    host: Option<String>,
    /// `Online · Up to date`.
    state: String,
    busy: Option<Busy>,
    /// What went wrong with the last Sign out, and whether to offer signing out on this computer only.
    sign_out_error: Option<String>,
    offer_local: bool,
}

impl Shell {
    /// The frame for the page `model` shows, or `None` for a screen outside the signed-in window.
    pub(super) fn of(model: &Model) -> Option<Self> {
        let tab = model.tab()?;
        let status = model.status.as_ref();
        let (account, host) = match status {
            Some(Status { account: Some(account), server, .. }) => {
                let server = server.as_deref().map(super::host).unwrap_or_default();
                (account.clone(), (!server.is_empty()).then(|| server.to_owned()))
            }
            _ => ("Signed in".to_owned(), None),
        };
        let state =
            status.map(|s| format!("{} · {}", if s.online { "Online" } else { "Offline" }, super::overview::sync_line(s).0)).unwrap_or_default();
        Some(Self {
            tab,
            tabs: model.tabs(),
            locked: model.tabs_locked(),
            account,
            host,
            state,
            busy: model.busy,
            sign_out_error: model.sign_out_error.clone(),
            offer_local: model.offer_local_sign_out,
        })
    }
}

/// The height of the header's row.
const HEADER_H: f32 = 56.0;
/// The room around the wordmark and the nav, and between the blocks and their content.
const BLOCK_PAD: f32 = 16.0;
/// The least room worth giving the account line beside the tabs; with less the tabs get a row of their own.
const MIN_ACCOUNT_ROOM: f32 = 90.0;
/// The least room the user name gets beside the host; with less, only the host is shown.
const MIN_NAME_ROOM: f32 = 24.0;
/// The height of the tabs' own row at narrow widths.
const TABS_ROW_H: f32 = 44.0;

/// The header bar, the mockups' way: the wordmark in a block with a hairline on its right, the account line taking the free
/// space, the nav in a block with a hairline on its left. The account line is cut off with an ellipsis (the user name goes,
/// the host stays) rather than pushing a tab out of the window; when even that leaves it too little room, the tabs have a
/// row of their own under a hairline, which scrolls if they don't fit.
pub(super) fn header(ui: &mut egui::Ui, shell: &Shell, events: &mut Vec<Event>) {
    let t = Tokens::get(ui.ctx());
    let frame = egui::Frame::new().fill(t.bg);
    egui::Panel::top("header").frame(frame).show(ui, |ui| {
        let width = ui.available_width();
        let origin = ui.cursor().min;
        let block = |x: f32, w: f32, y: f32, h: f32, layout: Layout| {
            egui::UiBuilder::new().max_rect(egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w.max(0.0), h))).layout(layout)
        };
        let centred = Layout::left_to_right(Align::Center);
        let mut mark = ui.new_child(block(origin.x + 20.0, 400.0, origin.y, HEADER_H, centred));
        kit::wordmark(&mut mark, 22.0);
        let mark_w = mark.min_rect().width() + 40.0;
        let tabs_width: f32 = shell.tabs.iter().map(|tab| kit::tab_width(ui, tab.label()) + 4.0).sum();
        let nav_w = tabs_width + 2.0 * BLOCK_PAD;
        let line = t.hairline();
        let painter = ui.painter().clone();
        if width - mark_w - nav_w >= MIN_ACCOUNT_ROOM {
            let account_w = width - mark_w - nav_w - 2.0 * BLOCK_PAD;
            let mut account = ui.new_child(block(origin.x + mark_w + BLOCK_PAD, account_w, origin.y, HEADER_H, centred));
            title(&mut account, shell, account_w);
            let nav_x = origin.x + width - nav_w;
            let mut nav = ui.new_child(block(nav_x + BLOCK_PAD, tabs_width, origin.y, HEADER_H, centred));
            tabs(&mut nav, shell, events);
            painter.vline(origin.x + mark_w - 0.5, origin.y..=origin.y + HEADER_H, line);
            painter.vline(nav_x + 0.5, origin.y..=origin.y + HEADER_H, line);
            ui.allocate_space(egui::vec2(width, HEADER_H));
        } else {
            let account_w = width - mark_w - 2.0 * BLOCK_PAD;
            let mut account = ui.new_child(block(origin.x + mark_w + BLOCK_PAD, account_w, origin.y, HEADER_H, centred));
            title(&mut account, shell, account_w);
            painter.vline(origin.x + mark_w - 0.5, origin.y..=origin.y + HEADER_H, line);
            painter.hline(origin.x..=origin.x + width, origin.y + HEADER_H - 0.5, line);
            let mut row = ui.new_child(block(origin.x + BLOCK_PAD, width - 2.0 * BLOCK_PAD, origin.y + HEADER_H, TABS_ROW_H, centred));
            egui::ScrollArea::horizontal().id_salt("header-tabs").scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden).show(
                &mut row,
                |ui| {
                    ui.horizontal_centered(|ui| tabs(ui, shell, events));
                },
            );
            ui.allocate_space(egui::vec2(width, HEADER_H + TABS_ROW_H));
        }
    });
}

/// The account line: the user name (cut off with an ellipsis, whole as its hover text, when `room` is short) and `· host`.
fn title(ui: &mut egui::Ui, shell: &Shell, room: f32) {
    ui.spacing_mut().item_spacing.x = 6.0;
    match &shell.host {
        Some(host) => {
            // The host keeps its room (a long one gives up some of it to the name); the name takes what is left.
            let tail = format!("· {host}");
            let gap = ui.spacing().item_spacing.x;
            let (name_w, tail_w) = (kit::section_label_width(ui, &shell.account), kit::section_label_width(ui, &tail));
            if name_w + gap + tail_w <= room {
                kit::section_label(ui, &shell.account);
                kit::section_label(ui, &tail);
                return;
            }
            let tail_room = tail_w.min((room * 0.6).max(60.0));
            let name_room = room - tail_room - gap;
            if name_room >= MIN_NAME_ROOM {
                kit::section_label_fit(ui, &shell.account, name_room);
                kit::section_label_fit(ui, &tail, tail_room);
            } else {
                kit::section_label_fit(ui, &tail, room);
            }
        }
        None => {
            kit::section_label_fit(ui, &shell.account, room);
        }
    }
}

fn tabs(ui: &mut egui::Ui, shell: &Shell, events: &mut Vec<Event>) {
    ui.spacing_mut().item_spacing.x = 4.0;
    ui.add_enabled_ui(!shell.locked, |ui| {
        for (index, tab) in shell.tabs.iter().enumerate() {
            let response = kit::tab(ui, tab.label(), *tab == shell.tab);
            if response.clicked() {
                events.push(Event::Tab(*tab));
            }
            if shell.locked {
                response.on_disabled_hover_text(LOCKED_HOVER);
            } else if let Some(shortcut) = super::shortcuts::tab_shortcut(ui.ctx(), index) {
                response.on_hover_text(format!("{} ({shortcut})", tab.label()));
            }
        }
    });
}

/// The height of the footer strip.
const FOOTER_H: f32 = 36.0;
/// The room between Sign out and Quit.
const FOOTER_LINK_GAP: f32 = 20.0;
/// Why the tabs, Sign out and Quit are off while the page holds a one-time code or waits for a call.
const LOCKED_HOVER: &str = "Finish or close what the page shows first";

/// The footer strip: the agent's state on the left, Sign out and Quit as accent text links on the right, 20 px apart; above
/// it, what went wrong with the last Sign out.
pub(super) fn footer(ui: &mut egui::Ui, shell: &Shell, events: &mut Vec<Event>) {
    let t = Tokens::get(ui.ctx());
    let frame = egui::Frame::new().fill(t.bg).inner_margin(Margin::symmetric(20, 0));
    egui::Panel::bottom("footer").frame(frame).show(ui, |ui| {
        // Left to right, so the tab order is the visual one: the caption (cut off when it doesn't fit), the room, Sign out and
        // Quit. The links are measured first and keep their place; the caption gets what is left.
        ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), FOOTER_H), Layout::left_to_right(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            let (sign_out_w, quit_w) = (kit::text_link_width(ui, "Sign out", 11.0), kit::text_link_width(ui, "Quit", 11.0));
            let links_w = sign_out_w + FOOTER_LINK_GAP + quit_w;
            kit::caption_fit(ui, &shell.state, ui.available_width() - links_w - FOOTER_LINK_GAP);
            ui.add_space((ui.available_width() - links_w).max(0.0));
            let sign_out = ui.add_enabled_ui(shell.busy.is_none() && !shell.locked, |ui| kit::text_link(ui, "Sign out", 11.0)).inner;
            let sign_out =
                if shell.locked { sign_out.on_disabled_hover_text(LOCKED_HOVER) } else { sign_out.on_hover_text("The files stay on this computer") };
            if sign_out.clicked() {
                events.push(Event::SignOut);
            }
            ui.add_space(FOOTER_LINK_GAP);
            let quit = ui.add_enabled_ui(shell.busy != Some(Busy::Quitting) && !shell.locked, |ui| kit::text_link(ui, "Quit", 11.0)).inner;
            let quit =
                if shell.locked { quit.on_disabled_hover_text(LOCKED_HOVER) } else { quit.on_hover_text("Stops syncing until the next login") };
            if quit.clicked() {
                events.push(Event::Quit);
            }
        });
    });
    if let Some(error) = &shell.sign_out_error {
        let frame = egui::Frame::new().fill(t.bg).inner_margin(Margin::symmetric(20, 8));
        egui::Panel::bottom("sign-out-problem").frame(frame).show(ui, |ui| {
            super::error_line(ui, error);
            if shell.offer_local
                && ui.add_enabled(shell.busy.is_none() && !shell.locked, kit::Button::new("Sign out on this computer only")).clicked()
            {
                events.push(Event::SignOutHere);
            }
        });
    }
}
