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
    /// `fabio · cloud.shadows.ch`.
    account: String,
    /// `Online · Up to date`.
    state: String,
    busy: Option<Busy>,
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
        Some(Self { tab, tabs: model.tabs(), account, state, busy: model.busy })
    }
}

/// The width from which the header is one row; below it the tabs have a row of their own, which scrolls if they don't fit.
const ONE_ROW: f32 = 900.0;

/// The header bar: the wordmark, the account line and the tabs.
pub(super) fn header(ui: &mut egui::Ui, shell: &Shell, events: &mut Vec<Event>) {
    let t = Tokens::get(ui.ctx());
    let frame = egui::Frame::new().fill(t.bg).inner_margin(Margin::symmetric(20, 8));
    egui::Panel::top("header").frame(frame).show(ui, |ui| {
        if ui.available_width() >= ONE_ROW {
            ui.horizontal(|ui| {
                title(ui, shell);
                ui.add_space(16.0);
                tabs(ui, shell, events);
            });
        } else {
            ui.horizontal(|ui| title(ui, shell));
            egui::ScrollArea::horizontal().id_salt("header-tabs").scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden).show(
                ui,
                |ui| {
                    ui.horizontal(|ui| tabs(ui, shell, events));
                },
            );
        }
    });
}

/// The wordmark and the account line.
fn title(ui: &mut egui::Ui, shell: &Shell) {
    kit::wordmark(ui, 22.0);
    ui.add_space(8.0);
    kit::section_label(ui, &shell.account);
}

fn tabs(ui: &mut egui::Ui, shell: &Shell, events: &mut Vec<Event>) {
    ui.spacing_mut().item_spacing.x = 4.0;
    for tab in &shell.tabs {
        if kit::tab(ui, tab.label(), *tab == shell.tab).clicked() {
            events.push(Event::Tab(*tab));
        }
    }
}

/// The footer strip: the agent's state on the left, Sign out and Quit on the right.
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
}
