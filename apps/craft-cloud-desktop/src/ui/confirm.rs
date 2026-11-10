//! The confirmation of a change that can't be undone (Remove from trash) or that replaces a file (Restore this version),
//! drawn in the page under what it is about: a panel at most 380 px wide, at the right, with a mono title, the text, and
//! Cancel before the confirming button. Esc cancels, and the keyboard focus moves into the panel when it opens.

use egui::{Align, Key, Layout, Modifiers};

use super::kit::{self, Tokens};

/// What the person did with the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Answer {
    /// Nothing yet.
    Open,
    Confirmed,
    Cancelled,
}

/// What the panel says and offers.
pub(super) struct Panel<'a> {
    /// Tells this panel from another one asked for in the same place: the focus moves in when it changes.
    pub key: egui::Id,
    pub title: &'a str,
    pub text: &'a str,
    pub confirm: &'a str,
    /// The confirming button is red and outlined (a removal) instead of the primary one.
    pub danger: bool,
    /// Whether the confirming button can be used (not while a call is under way).
    pub enabled: bool,
}

/// The widest the panel gets.
const WIDTH: f32 = 380.0;

fn slot() -> egui::Id {
    egui::Id::new("craft-cloud-confirm-key")
}

/// No panel is shown: the next one takes the focus when it opens.
pub(super) fn idle(ui: &egui::Ui) {
    ui.data_mut(|d| {
        d.remove::<egui::Id>(slot());
        d.remove::<u8>(slot());
    });
}

/// Draws the panel at the right under what is above it, and says what was done with it.
pub(super) fn show(ui: &mut egui::Ui, panel: &Panel<'_>) -> Answer {
    let t = Tokens::get(ui.ctx());
    let opened = ui.data(|d| d.get_temp::<egui::Id>(slot())) != Some(panel.key);
    let width = ui.available_width().min(WIDTH);
    let right = ui.max_rect().right();
    let top = ui.cursor().top();
    let area = egui::Rect::from_min_size(egui::pos2(right - width, top), egui::vec2(width, 10_000.0));
    let mut child = ui.new_child(egui::UiBuilder::new().id_salt(("confirm", panel.key)).max_rect(area).layout(Layout::top_down(Align::Min)));
    let mut answer = Answer::Open;
    egui::Frame::new().fill(t.bg).stroke(egui::Stroke::new(1.0, t.line_strong)).show(&mut child, |ui| {
        ui.set_width(width - 2.0);
        ui.spacing_mut().item_spacing.y = 0.0;
        egui::Frame::new().inner_margin(egui::Margin::symmetric(16, 12)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            kit::section_label_fit(ui, panel.title, ui.available_width());
        });
        kit::hairline(ui);
        egui::Frame::new().inner_margin(egui::Margin::same(16)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(egui::Label::new(panel.text).wrap());
        });
        egui::Frame::new().inner_margin(egui::Margin { left: 16, right: 16, top: 0, bottom: 16 }).show(ui, |ui| {
            ui.set_width(ui.available_width());
            let button = kit::Button::new(panel.confirm);
            let button = if panel.danger { button.danger() } else { button.primary() };
            let cancel = kit::Button::new("Cancel");
            let width = cancel.width(ui) + ui.spacing().item_spacing.x + button.width(ui);
            // Left to right, so that Tab goes Cancel, then the confirmation, as they are drawn.
            kit::trailing(ui, width, |ui| {
                let cancel = ui.add(cancel);
                let confirm = ui.add_enabled(panel.enabled, button);
                if confirm.clicked() {
                    answer = Answer::Confirmed;
                }
                if cancel.clicked() {
                    answer = Answer::Cancelled;
                }
                if opened {
                    // The safe choice takes the focus: Enter right away never removes anything.
                    cancel.request_focus();
                }
            });
        });
    });
    let used = child.min_rect();
    ui.allocate_space(egui::vec2(ui.available_width(), used.height()));
    if opened {
        ui.data_mut(|d| {
            d.insert_temp(slot(), panel.key);
            d.insert_temp(slot(), 0_u8);
        });
    }
    // The page is scrolled to the panel when it opens. The scroll area only learns how tall the page has grown in the
    // frame after, so this is repeated for a few frames.
    let scrolled = ui.data(|d| d.get_temp::<u8>(slot())).unwrap_or(0);
    if scrolled < 3 {
        ui.data_mut(|d| d.insert_temp(slot(), scrolled + 1));
        ui.scroll_to_rect(used, None);
        ui.ctx().request_repaint();
    }
    if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        answer = Answer::Cancelled;
    }
    answer
}
