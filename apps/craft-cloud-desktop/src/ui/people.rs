//! The people who have a recent file open right now: each one's initials in their palette colour (the account's colour is the
//! same on every computer, `craft_cloud_proto::colour`), at most [`SHOWN`], then "+N" for the rest, and every name in a tooltip.
//!
//! The agent says who has the file open, not what they do with it, so the words are "open now", never "editing". This is a
//! small widget of the desktop's own: it only draws what it is given.

use craft_cloud_app::RecentPerson;
use craft_cloud_proto::colour::palette_colour;
use egui::{Color32, RichText, Ui};

/// How many people are drawn before the rest are counted.
pub const SHOWN: usize = 2;

/// Two letters at most to tell an account by: the first letters of its first two words, or the first two letters of a single
/// word.
pub fn initials(name: &str) -> String {
    let words: Vec<&str> = name.split(|c: char| c.is_whitespace() || matches!(c, '-' | '_' | '.')).filter(|w| !w.is_empty()).collect();
    let letters: Vec<char> = match words.as_slice() {
        [] => Vec::new(),
        [one] => one.chars().take(2).collect(),
        [first, second, ..] => first.chars().take(1).chain(second.chars().take(1)).collect(),
    };
    letters.iter().flat_map(|c| c.to_uppercase()).collect()
}

/// The tooltip: every name.
pub fn tooltip(people: &[RecentPerson]) -> String {
    let names: Vec<&str> = people.iter().map(|p| p.name.as_str()).collect();
    format!("Open now: {}", names.join(", "))
}

/// A person's colour for the current background (light or dark).
fn colour(ui: &Ui, person: &RecentPerson) -> Color32 {
    let palette = palette_colour(person.colour);
    let rgb = if ui.visuals().dark_mode { palette.dark } else { palette.light };
    Color32::from_rgb(rgb[0], rgb[1], rgb[2])
}

/// Who has the file open: draws nothing for an empty list.
pub fn open_now(ui: &mut Ui, people: &[RecentPerson]) {
    if people.is_empty() {
        return;
    }
    let tip = tooltip(people);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for person in people.iter().take(SHOWN) {
            let colour = colour(ui, person);
            let pill = egui::Frame::new().stroke(egui::Stroke::new(1.0, colour)).corner_radius(8).inner_margin(egui::Margin::symmetric(5, 1)).show(
                ui,
                |ui| {
                    let label = ui.label(RichText::new(initials(&person.name)).strong().color(colour));
                    label.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, person.name.clone()));
                },
            );
            pill.response.on_hover_text(&tip);
        }
        let rest = people.len().saturating_sub(SHOWN);
        if rest > 0 {
            let names: Vec<&str> = people.iter().skip(SHOWN).map(|p| p.name.as_str()).collect();
            let more = ui.label(format!("+{rest}"));
            more.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, format!("{rest} more: {}", names.join(", "))));
            more.on_hover_text(&tip);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_are_two_letters_at_most() {
        assert_eq!(initials("ana"), "AN");
        assert_eq!(initials("ben lee"), "BL");
        assert_eq!(initials("cy-dee_x"), "CD");
        assert_eq!(initials(""), "");
        assert_eq!(initials("élan"), "ÉL");
    }

    #[test]
    fn the_tooltip_names_everybody() {
        let people: Vec<RecentPerson> = ["ana", "ben", "cy", "dee", "eli"].iter().map(|n| RecentPerson { name: (*n).into(), colour: 1 }).collect();
        assert_eq!(tooltip(&people), "Open now: ana, ben, cy, dee, eli");
    }
}
