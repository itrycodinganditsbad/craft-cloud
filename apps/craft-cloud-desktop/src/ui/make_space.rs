//! "Make a team space from this folder": the form that opens from a folder's row in "On this computer", and the notice of how the
//! agent's work goes (drawn from the agent's status, so it also shows after the app was reopened). The rules and the texts are
//! in [`crate::make_space`].

use egui::RichText;

use super::kit::{self, Tokens};
use crate::make_space::{ADMIN_NOTE, EXPLAIN, MAX_NAME_CHARS, MakeSpaceMsg, Notice, Tone, notice};
use crate::model::{Event, Overview, Tab};

/// The widest the form and the notice get.
const WIDTH: f32 = 560.0;
/// The room inside the frames of the notice and the form.
const NOTICE_MARGIN: f32 = 14.0;
const FORM_MARGIN: f32 = 16.0;

fn send(events: &mut Vec<Event>, msg: MakeSpaceMsg) {
    events.push(Event::MakeSpace(msg));
}

/// The notice of the agent's work, if there is one.
pub(super) fn show_notice(ui: &mut egui::Ui, o: &Overview, events: &mut Vec<Event>) {
    let Some(status) = o.status.from_folder.as_ref() else { return };
    let Some(n) = notice(status, o.from_folder_dismissed.as_deref()) else { return };
    ui.add_space(12.0);
    draw_notice(ui, &n, events);
}

fn draw_notice(ui: &mut egui::Ui, n: &Notice, events: &mut Vec<Event>) {
    let t = Tokens::get(ui.ctx());
    let stroke = match n.tone {
        Tone::Working => t.line_strong,
        Tone::Good => t.good,
        Tone::Bad => t.bad,
    };
    egui::Frame::new().fill(t.bg).stroke(egui::Stroke::new(1.0, stroke)).inner_margin(egui::Margin::same(NOTICE_MARGIN as i8)).show(ui, |ui| {
        // The frame's own margin and stroke come on top of the width it is given.
        ui.set_width((ui.available_width().min(WIDTH) - 2.0 * NOTICE_MARGIN - 2.0).max(0.0));
        ui.spacing_mut().item_spacing.y = 6.0;
        ui.horizontal(|ui| {
            if n.tone == Tone::Working {
                ui.spinner();
            }
            let tone = match n.tone {
                Tone::Working => kit::Tone::Accent,
                Tone::Good => kit::Tone::Good,
                Tone::Bad => kit::Tone::Bad,
            };
            kit::tag(
                ui,
                match n.tone {
                    Tone::Working => "In progress",
                    Tone::Good => "Done",
                    Tone::Bad => "Stopped",
                },
                tone,
            );
            ui.add(egui::Label::new(RichText::new(&n.title).strong()).truncate());
        });
        ui.add(egui::Label::new(&n.text).wrap());
        if let Some((done, total)) = n.progress
            && total > 0
        {
            let share = (done as f64 / total as f64).clamp(0.0, 1.0) as f32;
            // Painted here, like the storage bar: egui's progress bar takes the (translucent) selection colour.
            let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 6.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, 0.0, t.line);
            ui.painter().rect_filled(egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * share, rect.height())), 0.0, t.accent);
            ui.label(RichText::new(format!("{done} of {total} files")).small().color(t.muted));
        }
        if n.retry || n.dismiss || n.open_shared {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                if n.open_shared && ui.add(kit::Button::new("Open the Shared tab").primary()).clicked() {
                    events.push(Event::Tab(Tab::Shared));
                }
                if n.retry && ui.add(kit::Button::new("Try again").primary()).clicked() {
                    send(events, MakeSpaceMsg::Retry);
                }
                if n.dismiss && ui.add(kit::Button::new("Dismiss")).clicked() {
                    send(events, MakeSpaceMsg::Dismiss);
                }
            });
        }
    });
}

/// The form, when it is open.
pub(super) fn show_form(ui: &mut egui::Ui, o: &mut Overview, events: &mut Vec<Event>) {
    let Some(form) = o.make_space.as_mut() else { return };
    let t = Tokens::get(ui.ctx());
    ui.add_space(12.0);
    egui::Frame::new().fill(t.bg).stroke(egui::Stroke::new(1.0, t.line_strong)).inner_margin(egui::Margin::same(FORM_MARGIN as i8)).show(ui, |ui| {
        ui.set_width((ui.available_width().min(WIDTH) - 2.0 * FORM_MARGIN - 2.0).max(0.0));
        ui.spacing_mut().item_spacing.y = 6.0;
        kit::section_label_fit(ui, "Make a team space from this folder", ui.available_width());
        ui.add_space(4.0);
        ui.add(egui::Label::new(RichText::new(&form.folder).monospace().strong()).truncate());
        if form.resume {
            ui.add(
                egui::Label::new(format!("The team space “{}” was made already. This goes on with it; nothing is asked again.", form.name)).wrap(),
            );
        } else {
            ui.add(egui::Label::new(EXPLAIN).wrap());
        }
        ui.add_space(4.0);
        kit::section_label(ui, "Name of the team space");
        let editable = !form.busy && !form.old && !form.resume;
        let name = ui.add_enabled(
            editable,
            kit::text_edit(&mut form.name).id(egui::Id::new("make-space-name")).char_limit(MAX_NAME_CHARS).desired_width(f32::INFINITY),
        );
        let mut enter = name.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if !form.resume {
            ui.add(egui::Label::new(RichText::new(ADMIN_NOTE).small().color(t.muted)).wrap());
            kit::section_label(ui, "Your password");
            let password = ui.add_enabled(
                editable,
                kit::text_edit(&mut form.confirmation.password)
                    .id(super::security::secret_id(PASSWORD_FIELD))
                    .password(true)
                    .hint_text("your own current password")
                    .desired_width(f32::INFINITY),
            );
            enter |= password.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if form.needs_code {
                kit::section_label(ui, "Code from your authenticator app, or a recovery code");
                let code = ui.add_enabled(
                    editable,
                    kit::text_edit(&mut form.confirmation.code)
                        .id(super::security::secret_id(CODE_FIELD))
                        .password(true)
                        .hint_text("123456")
                        .desired_width(f32::INFINITY),
                );
                enter |= code.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            }
        }
        if let Some(error) = &form.error {
            ui.add_space(4.0);
            super::error_line(ui, error);
        }
        ui.add_space(6.0);
        if form.busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Asking the server…");
            });
            return;
        }
        let ready = form.ready();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if ui.add_enabled(ready, kit::Button::new("Make the team space").primary()).clicked() || (enter && ready) {
                send(events, MakeSpaceMsg::Submit);
            }
            if ui.add(kit::Button::new("Cancel")).clicked() {
                send(events, MakeSpaceMsg::Cancel);
            }
        });
    });
}

/// The names of the form's secret fields, for [`super::security::secret_id`] (their text is forgotten when the page is left).
pub(super) const PASSWORD_FIELD: &str = "make-space-password";
pub(super) const CODE_FIELD: &str = "make-space-code";
