//! The kit's widgets drawn by kittest (snapshots `kit-*` in `tests/snapshots/`, written with `UPDATE_SNAPSHOTS=1`), and what
//! they do: tags keep their size, the table and the joined cells lay out as designed, the segmented control answers clicks.

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

use super::kit::{self, Button, Column, JoinedCell, Part, Segment, Tone};

fn harness<S>(size: egui::Vec2, theme: egui::Theme, state: S, mut ui_fn: impl FnMut(&mut egui::Ui, &mut S) + 'static) -> Harness<'static, S> {
    Harness::builder().with_size(size).with_theme(theme).wgpu_setup(super::tests::shared_wgpu()).build_ui_state(
        move |ui, state: &mut S| {
            if kit::ensure(ui.ctx()) {
                egui::CentralPanel::default().show(ui, |ui| ui_fn(ui, state));
            }
        },
        state,
    )
}

fn render<S>(mut h: Harness<'static, S>, name: &str) {
    h.run_steps(4);
    super::tests::compare(h.render().unwrap(), name);
}

/// Everything of the kit on one page.
fn gallery(ui: &mut egui::Ui, selected: &mut usize) {
    ui.spacing_mut().item_spacing.y = 12.0;
    // Tags keep their size in a grid, a right-to-left layout and a wrapped row.
    egui::Grid::new("tags").num_columns(2).show(ui, |ui| {
        ui.label("Grid");
        kit::tag(ui, "Online", Tone::Good);
        ui.end_row();
    });
    ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), 40.0), egui::Layout::right_to_left(egui::Align::Center), |ui| {
        kit::tag(ui, "Admin", Tone::Accent);
        ui.add(Button::new("Row"));
    });
    ui.horizontal_wrapped(|ui| {
        kit::tag(ui, "Folder", Tone::Accent);
        kit::tag(ui, "File", Tone::Neutral);
        kit::tag(ui, "Near capacity", Tone::Warn);
        kit::tag(ui, "Disk", Tone::Bad);
    });
    ui.horizontal(|ui| {
        ui.add(Button::new("Outlined"));
        ui.add(Button::new("Primary").primary());
        ui.add(Button::new("Remove").danger());
        ui.add_enabled(false, Button::new("Disabled"));
        ui.add(Button::new("Large").primary().large());
    });
    ui.horizontal(|ui| {
        for (i, text) in ["Overview", "Trash", "Security"].into_iter().enumerate() {
            kit::tab(ui, text, i == 0);
        }
        ui.add_enabled_ui(false, |ui| {
            kit::tab(ui, "Locked", true);
            kit::tab(ui, "Other", false);
        });
    });
    let segments = [Segment::new("All").count(3), Segment::new("Folders").count(1), Segment::new("Files").count(2).enabled(false)];
    if let Some(i) = kit::segmented(ui, "Kind", &segments, *selected) {
        *selected = i;
    }
    let columns = [Column::flex("Name", 160.0), Column::fixed("Kind", 100.0), Column::fixed("", 120.0)];
    kit::table(ui, "gallery", &columns, 2, 52.0, |ui, row, column| match column {
        0 => {
            kit::truncated(
                ui,
                if row == 0 {
                    "A name that is far too long to fit into its column, with a path after it that goes on and on and on and on and on and on"
                } else {
                    "notes.txt"
                },
            );
        }
        1 => {
            kit::tag(ui, if row == 0 { "Folder" } else { "File" }, if row == 0 { Tone::Accent } else { Tone::Neutral });
        }
        _ => {
            ui.add(Button::new("Restore").primary());
        }
    });
    ui.add_space(8.0);
    joined(ui);
}

fn joined(ui: &mut egui::Ui) {
    let cells = [
        JoinedCell::new("01", Tone::Accent, "Sync"),
        JoinedCell::new("02", Tone::Warn, "Storage"),
        JoinedCell::new("03", Tone::Good, "Computers ↗").link(),
    ];
    kit::joined_cells(ui, "gallery-cells", &cells, 640.0, |ui, i, part| match (i, part) {
        (0, Part::Body) => {
            ui.label("A short body that wraps across more than one line in a narrow cell, left-aligned.");
        }
        (1, Part::Body) => {
            ui.label("Short.");
        }
        (2, Part::Body) => {
            ui.label("One");
            ui.label("Two");
            ui.label("Three");
            ui.label("Four");
        }
        (_, Part::Footer) => {
            ui.add(Button::new("Action").primary().min_size(egui::vec2(ui.available_width(), 0.0)));
        }
        _ => {}
    });
}

#[test]
fn snapshot_kit_gallery() {
    for (theme, name) in [(egui::Theme::Dark, "kit-gallery-dark"), (egui::Theme::Light, "kit-gallery-light")] {
        render(harness(egui::vec2(900.0, 900.0), theme, 0_usize, gallery), name);
    }
}

#[test]
fn snapshot_kit_joined_cells_stack_when_narrow() {
    render(harness(egui::vec2(420.0, 800.0), egui::Theme::Dark, (), |ui, ()| joined(ui)), "kit-cells-stacked");
}

#[test]
fn a_tag_keeps_its_natural_size_wherever_it_sits() {
    let mut h = harness(egui::vec2(600.0, 400.0), egui::Theme::Light, Vec::new(), |ui, sizes: &mut Vec<egui::Vec2>| {
        sizes.clear();
        egui::Grid::new("g").show(ui, |ui| {
            sizes.push(kit::tag(ui, "Online", Tone::Good).rect.size());
            ui.add(Button::new("Tall button").min_size(egui::vec2(0.0, 80.0)));
            ui.end_row();
        });
        ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), 60.0), egui::Layout::right_to_left(egui::Align::Center), |ui| {
            sizes.push(kit::tag(ui, "Online", Tone::Good).rect.size());
        });
        ui.horizontal_wrapped(|ui| sizes.push(kit::tag(ui, "Online", Tone::Good).rect.size()));
    });
    h.run_steps(4);
    let sizes = h.state().clone();
    assert_eq!(sizes.len(), 3);
    assert!(sizes.iter().all(|s| s.y > 16.0 && s.y < 26.0), "{sizes:?}");
    assert!(sizes.windows(2).all(|w| (w[0] - w[1]).abs().max_elem() < 0.5), "{sizes:?}");
}

#[test]
fn the_segmented_control_answers_clicks_and_not_on_a_disabled_segment() {
    let mut h = harness(egui::vec2(500.0, 120.0), egui::Theme::Light, 0_usize, |ui, selected| {
        let segments = [Segment::new("One"), Segment::new("Two"), Segment::new("Three").enabled(false)];
        if let Some(i) = kit::segmented(ui, "Choice", &segments, *selected) {
            *selected = i;
        }
    });
    h.run_steps(3);
    h.get_by_label("Two").click();
    h.run_steps(3);
    assert_eq!(*h.state(), 1);
    // The disabled one is a radio button without a click.
    h.get_by_label("Three").click();
    h.run_steps(3);
    assert_eq!(*h.state(), 1);
}

#[test]
fn flexible_columns_share_the_room_beyond_their_minimums() {
    let columns = [Column::flex("A", 100.0), Column::fixed("B", 50.0), Column::flex("C", 100.0).weighted(3.0)];
    assert_eq!(kit::table_min_width(&columns), 250.0);
    assert_eq!(kit::column_widths(&columns, 250.0), vec![100.0, 50.0, 100.0]);
    assert_eq!(kit::column_widths(&columns, 50.0), vec![100.0, 50.0, 100.0], "below the minimums the table keeps them");
    assert_eq!(kit::column_widths(&columns, 450.0), vec![150.0, 50.0, 250.0]);
}

#[test]
fn joined_cells_stack_below_their_width_and_sit_in_a_row_above() {
    assert_eq!(kit::joined_columns(3, 639.0, 640.0), 1);
    assert_eq!(kit::joined_columns(3, 640.0, 640.0), 3);
    assert_eq!(kit::joined_columns(0, 900.0, 640.0), 1);
}

#[test]
fn the_window_is_cleared_with_the_page_colour_in_both_appearances() {
    for (visuals, tokens) in [(egui::Visuals::dark(), kit::Tokens::DARK), (egui::Visuals::light(), kit::Tokens::LIGHT)] {
        let ctx = egui::Context::default();
        kit::apply(&ctx);
        let applied = ctx.style_of(if tokens.dark { egui::Theme::Dark } else { egui::Theme::Light }).visuals.clone();
        assert_eq!(kit::clear_color(&applied), tokens.bg.to_normalized_gamma_f32());
        assert_ne!(kit::clear_color(&visuals), tokens.bg.to_normalized_gamma_f32(), "egui's own visuals are not the kit's");
    }
}

#[test]
fn the_hairline_is_translucent_so_the_clear_colour_matters() {
    // The panel separator is drawn with the hairline, which is translucent; this is why eframe's translucent dark clear
    // colour made it near-black in the light appearance.
    assert!(kit::Tokens::LIGHT.hairline().color.a() < 255);
    assert!(kit::Tokens::DARK.hairline().color.a() < 255);
}

#[test]
fn a_plain_joined_cell_has_no_badge() {
    let cell = JoinedCell::plain(Tone::Good, "Files");
    assert!(cell.badge.is_empty());
    assert_eq!(cell.label, "Files");
    assert!(!cell.link);
}
