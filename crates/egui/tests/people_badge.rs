//! The people badge as kittest draws it, light and dark, compared with the snapshots in `tests/snapshots/`
//! (`UPDATE_SNAPSHOTS=1` writes them anew). kittest renders on wgpu with a CPU adapter where there is one; renderers
//! differ in a few edge pixels, so a snapshot matches when at most [`MAX_DIFFERENT`] thousandths of its pixels differ by
//! more than [`TOLERANCE`] in a channel.

use std::path::PathBuf;

use craft_cloud_egui::{PEOPLE_SHOWN, people_badge};
use craft_cloud_kit::{PaletteColour, Person};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

/// The most a channel may differ for a pixel to count as the same.
const TOLERANCE: u8 = 48;
/// The share of pixels that may differ, in thousandths.
const MAX_DIFFERENT: usize = 4;

const RED: PaletteColour = PaletteColour { name: "red", light: [0xc6, 0x28, 0x28], dark: [0xef, 0x7a, 0x7a] };
const GREEN: PaletteColour = PaletteColour { name: "green", light: [0x1b, 0x7a, 0x3e], dark: [0x6e, 0xd1, 0x8f] };
const BLUE: PaletteColour = PaletteColour { name: "blue", light: [0x1f, 0x4f, 0xc4], dark: [0x7f, 0xa6, 0xff] };
const PURPLE: PaletteColour = PaletteColour { name: "purple", light: [0x8a, 0x2f, 0xb8], dark: [0xcf, 0x93, 0xf0] };
const PINK: PaletteColour = PaletteColour { name: "pink", light: [0xc2, 0x1f, 0x7a], dark: [0xf5, 0x8a, 0xc4] };
const SLATE: PaletteColour = PaletteColour { name: "slate", light: [0x4a, 0x5a, 0x6a], dark: [0xa8, 0xb6, 0xc4] };

fn person(name: &str, colour: PaletteColour, is_self: bool) -> Person {
    Person::new(name, colour, is_self, Vec::new())
}

/// Three people, then six (two more than are shown).
fn crowds() -> (Vec<Person>, Vec<Person>) {
    let three = vec![person("ana", RED, true), person("Ben Okafor", GREEN, false), person("cara-lee", BLUE, false)];
    let mut six = three.clone();
    six.extend([person("dmitri", PURPLE, false), person("Eva", PINK, false), person("fatima_k", SLATE, false)]);
    (three, six)
}

fn compare(image: &image::RgbaImage, name: &str) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("snapshots");
    let path = dir.join(format!("{name}.png"));
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::create_dir_all(&dir).unwrap();
        image.save(&path).unwrap();
        return;
    }
    let new = dir.join(format!("{name}.new.png"));
    let Ok(old) = image::open(&path) else {
        image.save(&new).unwrap();
        panic!("no snapshot {} (UPDATE_SNAPSHOTS=1 writes it; this run's is {})", path.display(), new.display());
    };
    let old = old.into_rgba8();
    let same_size = old.dimensions() == image.dimensions();
    let different = old.pixels().zip(image.pixels()).filter(|(a, b)| a.0.iter().zip(b.0.iter()).any(|(x, y)| x.abs_diff(*y) > TOLERANCE)).count();
    let allowed = (image.width() * image.height()) as usize * MAX_DIFFERENT / 1000;
    if !same_size || different > allowed {
        image.save(&new).unwrap();
        panic!("{name}: {different} pixels differ (at most {allowed}); this run's is {}", new.display());
    }
    let _ = std::fs::remove_file(new);
}

fn draw(theme: egui::Theme) -> Harness<'static, (Vec<Person>, Vec<Person>)> {
    Harness::builder().with_size(egui::vec2(300.0, 110.0)).with_theme(theme).wgpu().build_ui_state(
        |ui, (three, six): &mut (Vec<Person>, Vec<Person>)| {
            ui.vertical(|ui| {
                ui.label("Three people");
                people_badge(ui, three);
                ui.label("Six people");
                people_badge(ui, six);
            });
        },
        crowds(),
    )
}

/// The badge in light and in dark: initials in each account's colour for that background, "+2" for the rest. (One test
/// renders both, so only one wgpu device is made in the process at a time.)
#[test]
fn the_people_badge_in_light_and_dark() {
    for (theme, name) in [(egui::Theme::Light, "people-badge-light"), (egui::Theme::Dark, "people-badge-dark")] {
        let mut h = draw(theme);
        h.run_steps(4);
        // What a screen reader says: each pill is the person's name ("(you)" after your own), "+N" the names behind it.
        for named in ["ana (you)", "Ben Okafor", "cara-lee", "dmitri"] {
            assert!(h.query_all_by_label(named).count() >= 1, "{named} in {name}");
        }
        assert!(h.query_by_label("2 more: Eva, fatima_k").is_some(), "the two not shown are counted, with their names, in {name}");
        assert_eq!(PEOPLE_SHOWN, 4);
        for hidden in ["Eva", "fatima_k"] {
            assert!(h.query_by_label(hidden).is_none(), "only {PEOPLE_SHOWN} are shown in {name}");
        }
        let image = h.render().unwrap();
        compare(&image, name);
    }
}

/// An empty list draws nothing (an agent that doesn't say who is there).
#[test]
fn nobody_draws_nothing() {
    let mut h = Harness::new_ui(|ui| people_badge(ui, &[]));
    h.run();
    assert!(h.query_all_by_label_contains("more").count() == 0);
}
