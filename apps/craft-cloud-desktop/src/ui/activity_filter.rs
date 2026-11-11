//! The controls over an activity table (Recent activity, Security activity): a menu of the kinds of event, a menu of the
//! computers the loaded events name, a search field, and under them what they do: they filter the events that are loaded.

use egui::RichText;

use super::kit::{self, Tokens};
use crate::filter::{ActivityFilter, ActivityKind, FilterEdit, HINT, computers};
use crate::names::Names;
use craft_cloud_proto::api::AuditEvent;

/// The widths of the two menus.
const KIND_W: f32 = 150.0;
const COMPUTER_W: f32 = 190.0;

/// Draws the controls for the table that shows `events`; `scope` tells this table's menus from the other's. The edits made are
/// returned for the caller to send.
pub(super) fn controls(ui: &mut egui::Ui, scope: &str, filter: &ActivityFilter, events: &[AuditEvent], names: &Names) -> Vec<FilterEdit> {
    let mut edits = Vec::new();
    // Controls of a row are as high as a button, so the menus and the field line up.
    ui.spacing_mut().interact_size.y = kit::CONTROL_H;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        egui::ComboBox::from_id_salt(("activity-kind", scope)).selected_text(filter.kind.label()).width(KIND_W).show_ui(ui, |ui| {
            for kind in ActivityKind::ALL {
                if ui.selectable_label(filter.kind == kind, kind.label()).clicked() && filter.kind != kind {
                    edits.push(FilterEdit::Kind(kind));
                }
            }
        });
        let listed = computers(events, filter.computer, names);
        let chosen = filter.computer.and_then(|id| listed.iter().find(|(known, _)| *known == id)).map_or("All computers", |(_, name)| name.as_str());
        egui::ComboBox::from_id_salt(("activity-computer", scope)).selected_text(chosen).width(COMPUTER_W).show_ui(ui, |ui| {
            if ui.selectable_label(filter.computer.is_none(), "All computers").clicked() && filter.computer.is_some() {
                edits.push(FilterEdit::Computer(None));
            }
            for (id, name) in &listed {
                if ui.selectable_label(filter.computer == Some(*id), name).clicked() && filter.computer != Some(*id) {
                    edits.push(FilterEdit::Computer(Some(*id)));
                }
            }
        });
        let mut search = filter.search.clone();
        if ui.add(kit::text_edit(&mut search).id(super::shortcuts::search_id()).hint_text("Search the events").desired_width(200.0)).changed() {
            edits.push(FilterEdit::Search(search));
        }
    });
    ui.add_space(4.0);
    let t = Tokens::get(ui.ctx());
    ui.label(RichText::new(HINT).small().color(t.muted));
    ui.add_space(6.0);
    edits
}

/// The line that stands for a table whose events are all filtered out, with the way to show them again.
pub(super) fn nothing_matches(ui: &mut egui::Ui) -> Option<FilterEdit> {
    let t = Tokens::get(ui.ctx());
    let mut edit = None;
    ui.horizontal(|ui| {
        ui.label(RichText::new("None of the loaded events match the filters.").color(t.muted));
        if kit::text_link(ui, "Clear filters", 12.0).clicked() {
            edit = Some(FilterEdit::Clear);
        }
    });
    edit
}
