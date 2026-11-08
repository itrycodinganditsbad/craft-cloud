//! Two demo windows for two accounts of one team space on one fake hub: each sees who else is in the document (initials in
//! their colours), a layer added by one account shows on the other, and a viewer's window is read-only, refusing a change
//! through the kit's read-only path.

use std::path::Path;
use std::time::{Duration, Instant};

use craft_cloud_app::fake::{Fake, Hub};
use craft_cloud_kit::Agent;
use craft_cloud_kit_demo::model;
use craft_cloud_kit_demo::ui::Demo;
use egui::accesskit::Role;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

type Window = Harness<'static, Demo>;

fn demo(fake: &Fake, folder: &Path, machine: &str) -> Demo {
    let client = fake.client();
    Demo::open(Some(Agent::from_fn(move || Ok(client.clone()))), folder, "team.kitdemo", machine, || {})
}

fn window(demo: Demo) -> Window {
    Harness::builder().with_size(egui::vec2(1000.0, 700.0)).build_ui_state(|ui, d: &mut Demo| d.show(ui), demo)
}

/// Frames on all windows until `done` holds (20 s at most).
fn until(windows: &mut [&mut Window], what: &str, done: impl Fn(&[&mut Window]) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        for w in windows.iter_mut() {
            w.run_steps(1);
        }
        if done(windows) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn names(d: &Demo) -> Vec<String> {
    model::flatten(&d.model().layers).into_iter().map(|(_, l)| l.name.clone()).collect()
}

fn shows(w: &Window, label: &str) -> bool {
    w.query_by_label(label).is_some()
}

#[test]
fn two_accounts_see_each_other_and_edit_together() {
    let hub = Hub::new();
    hub.team();
    let (ana_pc, ben_pc) = (hub.machine_as("ana-pc", "ana").unwrap(), hub.machine_as("ben-pc", "ben").unwrap());
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = window(demo(&ana_pc, dir_a.path(), "ana-pc"));
    let mut b = window(demo(&ben_pc, dir_b.path(), "ben-pc"));
    until(&mut [&mut a, &mut b], "both live", |w| w.iter().all(|w| w.state().is_live()));

    // Each window shows its own account's badge, in the list of people, and nobody else yet.
    until(&mut [&mut a, &mut b], "each shows itself", |w| shows(w[0], "AN") && shows(w[1], "BE"));
    assert!(!shows(&a, "BE") && !shows(&b, "AN"), "nobody else is in the document yet");

    // Ben's cursor in the story makes him one of the people on ana's window, and ana's on his.
    let story = b.get_by_role(Role::MultilineTextInput);
    story.focus();
    b.run_steps(1);
    let story = a.get_by_role(Role::MultilineTextInput);
    story.focus();
    until(&mut [&mut a, &mut b], "each sees the other", |w| shows(w[0], "BE") && shows(w[1], "AN"));
    let people: Vec<String> = a.state().session().map(|s| s.people().into_iter().map(|p| p.name).collect()).unwrap_or_default();
    assert_eq!(people, ["ana", "ben"], "yourself first");

    // A layer added by ana shows on ben's window: two accounts, one live document.
    a.get_by_label("Add layer").click();
    until(&mut [&mut a, &mut b], "the layer on ben's window", |w| names(w[1].state()) == ["Layer 1"]);

    // Ben leaves the space: his badge goes from ana's window.
    hub.remove_member("ben");
    until(&mut [&mut a, &mut b], "ben is gone from ana's window", |w| !shows(w[0], "BE"));
    assert!(shows(&a, "AN"));
}

#[test]
fn a_viewers_window_is_read_only() {
    let hub = Hub::new();
    hub.team();
    hub.set_read_only("cara", true);
    let (ana_pc, cara_pc) = (hub.machine_as("ana-pc", "ana").unwrap(), hub.machine_as("cara-pc", "cara").unwrap());
    let (dir_a, dir_c) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = window(demo(&ana_pc, dir_a.path(), "ana-pc"));
    until(&mut [&mut a], "ana live", |w| w[0].state().is_live());
    a.get_by_label("Add layer").click();
    a.run_steps(2);
    let mut c = window(demo(&cara_pc, dir_c.path(), "cara-pc"));
    until(&mut [&mut a, &mut c], "cara live with the layer", |w| w[1].state().is_live() && names(w[1].state()) == ["Layer 1"]);

    // The kit says she is a viewer; the window shows it, and a change from the app is refused by the session.
    assert!(c.state().session().is_some_and(|s| s.status().viewer));
    assert!(shows(&c, "Read-only"));
    assert!(shows(&c, "You can look at this document but not change it."));
    c.state_mut().add_layer("hers");
    c.run_steps(2);
    assert!(c.state().message.contains("you can only view"), "{}", c.state().message);
    assert_eq!(names(c.state()), ["Layer 1"], "the model stands");
    // Nothing of hers reaches ana.
    for _ in 0..30 {
        a.run_steps(1);
        c.run_steps(1);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(names(a.state()), ["Layer 1"]);
}
