//! Two demo windows, one per machine of an account on one fake hub, driven through their widgets as a person would:
//! a layer added on one shows on the other, typing shows there, a rename there comes back, undo takes back only
//! this window's edit, the other's cursor is marked, and saving writes the file with its save record.

use std::path::Path;
use std::time::{Duration, Instant};

use craft_cloud_app::fake::{Fake, Hub};
use craft_cloud_kit::Agent;
use craft_cloud_kit_demo::model;
use craft_cloud_kit_demo::ui::Demo;
use egui::accesskit::Role;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

fn demo(fake: &Fake, folder: &Path, machine: &str) -> Demo {
    let client = fake.client();
    Demo::open(Some(Agent::from_fn(move || Ok(client.clone()))), folder, "demo.kitdemo", machine, || {})
}

fn window(demo: Demo) -> Harness<'static, Demo> {
    Harness::builder().with_size(egui::vec2(1000.0, 700.0)).build_ui_state(|ui, d: &mut Demo| d.show(ui), demo)
}

/// Frames on both windows until `done` holds (20 s at most).
fn until(a: &mut Harness<'static, Demo>, b: &mut Harness<'static, Demo>, what: &str, done: impl Fn(&Demo, &Demo) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        a.run_steps(1);
        b.run_steps(1);
        if done(a.state(), b.state()) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn names(d: &Demo) -> Vec<String> {
    model::flatten(&d.model().layers).into_iter().map(|(_, l)| l.name.clone()).collect()
}

#[test]
fn two_windows_edit_one_live_document() {
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = window(demo(&pc, dir_a.path(), "pc"));
    let mut b = window(demo(&laptop, dir_b.path(), "laptop"));
    until(&mut a, &mut b, "both live", |a, b| a.is_live() && b.is_live());

    // A layer added on the pc shows on the laptop.
    a.get_by_label("Add layer").click();
    until(&mut a, &mut b, "the layer on the laptop", |_, b| names(b) == ["Layer 1"]);

    // Typing on the pc shows on the laptop.
    let story = a.get_by_role(Role::MultilineTextInput);
    story.focus();
    a.run_steps(1);
    a.get_by_role(Role::MultilineTextInput).type_text("Hello");
    until(&mut a, &mut b, "the text on the laptop", |_, b| b.model().story.text == "Hello");

    // The laptop renames the layer; it comes back to the pc.
    b.get_by_label("Layer 1").click();
    b.run_steps(2);
    let name = b.get_by_role(Role::TextInput);
    name.focus();
    b.run_steps(1);
    b.get_by_role(Role::TextInput).type_text(" (background)");
    b.run_steps(1);
    b.get_by_label("Rename").click();
    until(&mut a, &mut b, "the rename on the pc", |a, _| names(a) == ["Layer 1 (background)"]);

    // The laptop's cursor in the story is marked on the pc.
    let story = b.get_by_role(Role::MultilineTextInput);
    story.focus();
    until(&mut a, &mut b, "the laptop's cursor on the pc", |a, _| a.cursors().iter().any(|c| c.machine == "laptop"));

    // Undo on the pc takes back its typing (its last edit), never the laptop's rename.
    a.get_by_label("Undo").click();
    until(&mut a, &mut b, "the pc's undo on both", |a, b| a.model().story.text.is_empty() && b.model().story.text.is_empty());
    assert_eq!(names(a.state()), ["Layer 1 (background)"]);
    a.get_by_label("Redo").click();
    until(&mut a, &mut b, "the redo on both", |a, b| a.model().story.text == "Hello" && b.model().story.text == "Hello");

    // Saving writes the file, and the agent gets the save record.
    a.get_by_label("Save").click();
    a.run_steps(1);
    let written = std::fs::read(dir_a.path().join("demo.kitdemo")).unwrap();
    let back = model::from_file(&written).unwrap();
    assert_eq!(back.story.text, "Hello");
    assert_eq!(model::flatten(&back.layers).len(), 1);
    // (The save record names the content by BLAKE3, as memory blobs do.)
    use craft_cloud_kit::BlobStore;
    let hash: String = craft_cloud_kit::MemoryBlobs::default().put(&written).unwrap().hash.iter().map(|b| format!("{b:02x}")).collect();
    until(&mut a, &mut b, "the save record at the agent", |_, _| pc.saves().iter().any(|(h, _)| *h == hash));
}
