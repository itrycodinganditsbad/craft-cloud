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
    /// `fabio · cloud.shadows.ch`.
    account: String,
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
        let account = match status {
            Some(Status { account: Some(account), server, .. }) => {
                let server = server.as_deref().map(super::host).unwrap_or_default();
                if server.is_empty() { account.clone() } else { format!("{account} · {server}") }
            }
            _ => "Signed in".to_owned(),
        };
        let state =
            status.map(|s| format!("{} · {}", if s.online { "Online" } else { "Offline" }, super::overview::sync_line(s).0)).unwrap_or_default();
        Some(Self {
            tab,
            tabs: model.tabs(),
            locked: model.tabs_locked(),
            account,
            state,
            busy: model.busy,
            sign_out_error: model.sign_out_error.clone(),
            offer_local: model.offer_local_sign_out,
        })
    }
}

/// The room the wordmark takes in the header, and the gaps around the account line.
const WORDMARK_ROOM: f32 = 140.0 + 8.0 + 16.0;
/// The least room worth giving the account line beside the tabs; with less the tabs get a row of their own.
const MIN_ACCOUNT_ROOM: f32 = 90.0;

/// The header bar: the wordmark, the account line and the tabs. The account line is cut off with an ellipsis rather than
/// pushing a tab out of the window; when even that leaves it too little room, the tabs have a row of their own, which
/// scrolls if they don't fit.
pub(super) fn header(ui: &mut egui::Ui, shell: &Shell, events: &mut Vec<Event>) {
    let t = Tokens::get(ui.ctx());
    let frame = egui::Frame::new().fill(t.bg).inner_margin(Margin::symmetric(20, 8));
    egui::Panel::top("header").frame(frame).show(ui, |ui| {
        let tabs_width: f32 = shell.tabs.iter().map(|tab| kit::tab_width(ui, tab.label()) + 4.0).sum();
        let room = ui.available_width() - WORDMARK_ROOM - tabs_width;
        if room >= MIN_ACCOUNT_ROOM {
            ui.horizontal(|ui| {
                title(ui, shell, room);
                ui.add_space(16.0);
                tabs(ui, shell, events);
            });
        } else {
            let row = ui.available_width() - WORDMARK_ROOM;
            ui.horizontal(|ui| title(ui, shell, row));
            egui::ScrollArea::horizontal().id_salt("header-tabs").scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden).show(
                ui,
                |ui| {
                    ui.horizontal(|ui| tabs(ui, shell, events));
                },
            );
        }
    });
}

/// The wordmark and the account line, which gets at most `room`.
fn title(ui: &mut egui::Ui, shell: &Shell, room: f32) {
    kit::wordmark(ui, 22.0);
    ui.add_space(8.0);
    kit::section_label_fit(ui, &shell.account, room);
}

fn tabs(ui: &mut egui::Ui, shell: &Shell, events: &mut Vec<Event>) {
    ui.spacing_mut().item_spacing.x = 4.0;
    ui.add_enabled_ui(!shell.locked, |ui| {
        for tab in &shell.tabs {
            let response = kit::tab(ui, tab.label(), *tab == shell.tab);
            if response.clicked() {
                events.push(Event::Tab(*tab));
            }
            if shell.locked {
                response.on_disabled_hover_text("Finish or close what the page shows first");
            }
        }
    });
}

/// The footer strip: the agent's state on the left, Sign out and Quit on the right; above it, what went wrong with the
/// last Sign out.
pub(super) fn footer(ui: &mut egui::Ui, shell: &Shell, events: &mut Vec<Event>) {
    let t = Tokens::get(ui.ctx());
    let frame = egui::Frame::new().fill(t.bg).inner_margin(Margin::symmetric(20, 8));
    egui::Panel::bottom("footer").frame(frame).show(ui, |ui| {
        ui.horizontal(|ui| {
            kit::section_label(ui, &shell.state);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add_enabled(shell.busy != Some(Busy::Quitting), kit::Button::new("Quit"))
                    .on_hover_text("Stops syncing until the next login")
                    .clicked()
                {
                    events.push(Event::Quit);
                }
                if ui.add_enabled(shell.busy.is_none(), kit::Button::new("Sign out")).on_hover_text("The files stay on this computer").clicked() {
                    events.push(Event::SignOut);
                }
            });
        });
    });
    if let Some(error) = &shell.sign_out_error {
        let frame = egui::Frame::new().fill(t.bg).inner_margin(Margin::symmetric(20, 8));
        egui::Panel::bottom("sign-out-problem").frame(frame).show(ui, |ui| {
            super::error_line(ui, error);
            if shell.offer_local && ui.add_enabled(shell.busy.is_none(), kit::Button::new("Sign out on this computer only")).clicked() {
                events.push(Event::SignOutHere);
            }
        });
    }
}
