//! The audit log's pages: the admin's view (a filter by event and by account, pages, an export to a file) and the account's own
//! activity. Both show events as words, with the typed details the server writes and nothing else.

use egui::RichText;

use crate::admin::AdminEvent;
use crate::audit::{ActivityScreen, AuditList, AuditMsg, AuditView, ExportState, action_label, details_line, filter_choices, filter_label, who_line};
use crate::model::Event;
use craft_cloud_proto::api::AuditEvent;

fn send(events: &mut Vec<Event>, msg: AuditMsg) {
    events.push(Event::Audit(msg));
}

/// The events of a page: what happened, how long ago, and its details.
fn rows(ui: &mut egui::Ui, list: &AuditList, now_ms: i64) {
    if let Some(error) = &list.error {
        super::error_line(ui, error);
        ui.add_space(6.0);
    }
    if list.loading && !list.loaded {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Asking the server…");
        });
    } else if list.events.is_empty() && list.loaded && list.error.is_none() {
        ui.label("Nothing was recorded.");
    }
    for event in &list.events {
        event_row(ui, event, now_ms);
    }
}

fn event_row(ui: &mut egui::Ui, event: &AuditEvent, now_ms: i64) {
    ui.push_id(event.id, |ui| {
        let label = RichText::new(action_label(&event.action)).strong();
        ui.add(egui::Label::new(if event.outcome == "failure" { label.color(ui.visuals().error_fg_color) } else { label }).wrap());
        let mut line = super::ago(now_ms, event.at_ms);
        let details = details_line(event);
        if !details.is_empty() {
            line.push_str(" · ");
            line.push_str(&details);
        }
        ui.add(egui::Label::new(RichText::new(line).small().weak()).wrap());
        // Who did it and what it was about, with the whole IDs on hover.
        let (who, full) = who_line(event);
        ui.add(egui::Label::new(RichText::new(who).small().weak()).wrap().sense(egui::Sense::hover())).on_hover_text(full);
        ui.separator();
    });
}

/// "Previous / Page n / Next", sending `previous` and `next`.
fn paging(ui: &mut egui::Ui, list: &AuditList, previous: AuditMsg, next: AuditMsg, events: &mut Vec<Event>) {
    if !list.has_previous() && list.next.is_none() {
        return;
    }
    ui.horizontal(|ui| {
        if ui.add_enabled(list.has_previous() && !list.loading, egui::Button::new("Previous")).clicked() {
            send(events, previous);
        }
        ui.label(format!("Page {}", list.page()));
        if ui.add_enabled(list.next.is_some() && !list.loading, egui::Button::new("Next")).clicked() {
            send(events, next);
        }
    });
}

/// The admin's audit log.
pub(super) fn log(ui: &mut egui::Ui, view: &mut AuditView, now_ms: i64, events: &mut Vec<Event>) {
    super::heading(ui, "Audit log");
    ui.horizontal(|ui| {
        if ui.button("Back to the accounts").clicked() {
            events.push(Event::Admin(AdminEvent::BackToList));
        }
        if ui.add_enabled(!view.list.loading, egui::Button::new("Refresh")).clicked() {
            send(events, AuditMsg::Reload);
        }
    });
    ui.add_space(8.0);

    // The filters: an event (the server's closed list, by group or by name) and the account the page was opened for.
    ui.horizontal(|ui| {
        ui.label("Show");
        let shown = view.filter.action.as_deref().map_or_else(|| "All events".to_owned(), filter_label);
        egui::ComboBox::from_id_salt("audit-action").selected_text(shown).width(220.0).show_ui(ui, |ui| {
            if ui.selectable_label(view.filter.action.is_none(), "All events").clicked() && view.filter.action.is_some() {
                send(events, AuditMsg::SetAction(None));
            }
            for choice in filter_choices() {
                let selected = view.filter.action.as_deref() == Some(choice);
                let text = if choice.ends_with('.') { format!("{} (all)", filter_label(choice)) } else { filter_label(choice) };
                if ui.selectable_label(selected, text).clicked() && !selected {
                    send(events, AuditMsg::SetAction(Some(choice.to_owned())));
                }
            }
        });
    });
    if let Some((_, name)) = &view.filter.user {
        ui.horizontal(|ui| {
            ui.label(format!("Only events about {name}"));
            if ui.small_button("Show everyone").clicked() {
                send(events, AuditMsg::ClearUser);
            }
        });
    }
    ui.add_space(6.0);

    // The export.
    ui.horizontal(|ui| {
        let idle = !matches!(view.export, ExportState::Picking { .. } | ExportState::Running { .. });
        if ui.add_enabled(idle, egui::Button::new("Export…")).on_hover_text("Save what these filters show, all of it, as a file").clicked() {
            send(events, AuditMsg::Export);
        }
        match &view.export {
            ExportState::Idle => {}
            ExportState::Picking { .. } => {
                ui.label(RichText::new("Choose where to save it…").weak());
            }
            ExportState::Running { .. } => {
                ui.spinner();
                ui.label("Saving…");
            }
            ExportState::Done { .. } | ExportState::Failed(_) => {
                if ui.small_button("OK").clicked() {
                    send(events, AuditMsg::DismissExport);
                }
            }
        }
    });
    match &view.export {
        ExportState::Done { path, outcome } if outcome.truncated => {
            super::error_line(
                ui,
                &format!(
                    "Only the newest {} events were saved to {path}: the log holds more than an export can. Narrow the filters to get the rest.",
                    outcome.events
                ),
            );
        }
        ExportState::Done { path, outcome } => {
            ui.label(RichText::new(format!("Saved {} events to {path}.", outcome.events)).color(super::good(ui)));
        }
        ExportState::Failed(why) => super::error_line(ui, &format!("The export failed, and no file was kept: {why}")),
        _ => {}
    }
    ui.add_space(8.0);

    rows(ui, &view.list, now_ms);
    paging(ui, &view.list, AuditMsg::Previous, AuditMsg::Next, events);
}

/// The signed-in account's own activity.
pub(super) fn activity(ui: &mut egui::Ui, screen: &mut ActivityScreen, now_ms: i64, events: &mut Vec<Event>) {
    super::heading(ui, "Activity");
    ui.add(
        egui::Label::new(
            RichText::new("What the server recorded for your account: sign-ins, password and two-factor changes, computers signed out.")
                .small()
                .weak(),
        )
        .wrap(),
    );
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ui.button("Back").clicked() {
            send(events, AuditMsg::CloseActivity);
        }
        if ui.add_enabled(!screen.list.loading, egui::Button::new("Refresh")).clicked() {
            send(events, AuditMsg::ActivityReload);
        }
    });
    ui.add_space(8.0);
    rows(ui, &screen.list, now_ms);
    paging(ui, &screen.list, AuditMsg::ActivityPrevious, AuditMsg::ActivityNext, events);
}
