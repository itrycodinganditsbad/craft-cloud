//! Every screen drawn by kittest against the fake agent, compared with the snapshots in `tests/snapshots/`
//! (`UPDATE_SNAPSHOTS=1` writes them anew), and the buttons sending what they should.
//!
//! kittest renders on wgpu with a CPU adapter where there is one (lavapipe on Linux, WARP on Windows). Renderers differ
//! in a few edge pixels, so a snapshot matches when at most [`MAX_DIFFERENT`] of its pixels differ by more than
//! [`TOLERANCE`] in a channel.

use std::path::PathBuf;

use craft_cloud_app::{DeviceId, DeviceInfo, PairingInfo, RunState, Status, Uuid};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

use super::*;
use crate::agent::fake::FakeAgent;
use crate::agent::{Agent, Starter, call};
use crate::model::{Effect, JoinDefaults, Model, Screen};

/// The time the screens are drawn at.
const NOW: i64 = 1_790_000_000_000;
/// The most a channel may differ for a pixel to count as the same.
const TOLERANCE: u8 = 48;
/// The share of pixels that may differ, in thousandths.
const MAX_DIFFERENT: usize = 4;

fn defaults() -> JoinDefaults {
    JoinDefaults { folder: "/home/fabio/Craft Cloud".into(), device_name: "lptp1".into() }
}

fn signed_in() -> Status {
    Status {
        api: 1,
        signed_in: true,
        account: Some("fabio".into()),
        server: Some("https://cloud.shadows.ch".into()),
        folder: Some("/home/fabio/Craft Cloud".into()),
        machine: Some("lptp1".into()),
        online: true,
        run_state: RunState::Idle,
        autostart: Some(true),
        last_sync_ms: Some(NOW - 120_000),
        removed: false,
    }
}

fn signed_out() -> Status {
    Status {
        signed_in: false,
        account: None,
        server: None,
        folder: None,
        machine: None,
        online: false,
        run_state: RunState::SignedOut,
        ..signed_in()
    }
}

fn no_start() -> Starter {
    std::sync::Arc::new(|_: &dyn Agent| Err("Craft Cloud's background service isn't there in tests".to_owned()))
}

/// The model after `events`, each effect answered by the fake (as the link does), until nothing is left.
fn after(agent: &FakeAgent, events: Vec<Event>) -> Model {
    let mut model = Model::new(defaults(), true, "/home/fabio/.local/share/craft-cloud-agent/run.log".into());
    let start = no_start();
    for event in std::iter::once(Event::Start).chain(events) {
        let mut pending = model.update(event);
        while let Some(effect) = pending.pop() {
            if let Some(answer) = call(agent, &start, effect) {
                pending.extend(model.update(answer));
            }
        }
    }
    model
}

fn devices() -> Vec<DeviceInfo> {
    vec![
        DeviceInfo { id: DeviceId::generate(), name: "lptp1".into(), created_ms: 0, last_used_ms: NOW - 5_000, revoked: false, current: true },
        DeviceInfo {
            id: DeviceId::generate(),
            name: "BEAST".into(),
            created_ms: 0,
            last_used_ms: NOW - 3 * 3_600_000,
            revoked: false,
            current: false,
        },
    ]
}

/// The harness: the window drawn for the model, its events kept.
fn harness(model: Model, theme: egui::Theme) -> Harness<'static, (Model, Vec<Event>)> {
    Harness::builder().with_size(egui::Vec2::from(WINDOW_SIZE)).with_theme(theme).wgpu().build_ui_state(
        |ui, (model, events): &mut (Model, Vec<Event>)| {
            events.extend(show(ui, model, NOW));
        },
        (model, Vec::new()),
    )
}

/// Draws `model` and compares it with the snapshot `name`.
fn snapshot(model: Model, theme: egui::Theme, name: &str) {
    let mut h = harness(model, theme);
    h.run_steps(4);
    let image = h.render().unwrap();
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

#[test]
fn snapshot_starting() {
    snapshot(Model::new(defaults(), true, String::new()), egui::Theme::Light, "starting");
}

#[test]
fn snapshot_not_running() {
    let agent = FakeAgent::new(signed_out());
    agent.gone.store(true, std::sync::atomic::Ordering::SeqCst);
    let model = after(&agent, Vec::new());
    assert!(matches!(model.screen, Screen::NotRunning(_)), "{:?}", model.screen);
    snapshot(model, egui::Theme::Light, "not-running");
}

/// The join form sent with `join` (as `Event::SubmitJoin` sends it once the join string reads; the snapshots' folder,
/// a Unix path, isn't a full path on Windows, so the form's own check is left to the model's tests).
fn sent(model: &mut Model, join: &str) {
    let Screen::Join(form) = &model.screen else { panic!("{:?}", model.screen) };
    let form = crate::model::JoinForm { join: join.into(), ..form.clone() };
    model.screen = Screen::Joining { form, password: None };
}

#[test]
fn snapshot_join() {
    let mut model = after(&FakeAgent::new(signed_out()), Vec::new());
    snapshot(model.clone(), egui::Theme::Light, "join");
    sent(&mut model, "cloud.shadows.ch/K7M4-Q9TX");
    snapshot(model.clone(), egui::Theme::Light, "joining");
    model.update(Event::Joined(Err(craft_cloud_app::Error::Refused { code: "folder_has_files".into(), message: "has files".into() })));
    snapshot(model.clone(), egui::Theme::Light, "join-folder-has-files");
    model.update(Event::SyncExistingFiles(false));
    sent(&mut model, "cloud.shadows.ch/K7M4-Q9TX");
    model.update(Event::Joined(Err(craft_cloud_app::Error::Refused { code: "invalid_code".into(), message: "x".into() })));
    snapshot(model, egui::Theme::Dark, "join-invalid-code-dark");
}

#[test]
fn snapshot_password_needed() {
    let mut model = after(&FakeAgent::new(signed_out()), Vec::new());
    sent(&mut model, "cloud.shadows.ch/K7M4-Q9TX-K7M4-Q9TX");
    model.update(Event::Joined(Err(craft_cloud_app::Error::Refused { code: "password_required".into(), message: "x".into() })));
    if let Screen::PasswordNeeded(pw) = &mut model.screen {
        pw.password = "correct horse".into();
        pw.again = "correct house".into();
    }
    model.update(Event::SubmitPassword);
    snapshot(model, egui::Theme::Light, "password-needed");
}

fn signed_in_model(status: Status) -> Model {
    let mut model = after(&FakeAgent::new(status), Vec::new());
    model.update(Event::Devices(Ok(devices())));
    model
}

#[test]
fn snapshot_signed_in() {
    snapshot(signed_in_model(signed_in()), egui::Theme::Light, "signed-in");
    snapshot(signed_in_model(signed_in()), egui::Theme::Dark, "signed-in-dark");
    let offline =
        Status { online: false, run_state: RunState::Retrying { message: "can't reach the server (connection refused)".into() }, ..signed_in() };
    snapshot(signed_in_model(offline), egui::Theme::Light, "signed-in-offline");
    let paused = Status { run_state: RunState::PausedByUser, ..signed_in() };
    snapshot(signed_in_model(paused), egui::Theme::Light, "signed-in-paused");
}

#[test]
fn snapshot_adding_a_computer() {
    let mut model = signed_in_model(signed_in());
    model.update(Event::AddComputer);
    let info = PairingInfo {
        id: Uuid::nil(),
        join: "cloud.shadows.ch/K7M4-Q9TX".into(),
        link: "https://cloud.shadows.ch/join/K7M4-Q9TX".into(),
        expires_ms: NOW + 581_000,
    };
    model.update(Event::PairingMade(Ok(info)));
    snapshot(model.clone(), egui::Theme::Light, "adding-a-computer");
    model.update(Event::PairingPolled(Ok(craft_cloud_app::PairingStatus {
        state: craft_cloud_app::PairingState::Used,
        device_name: Some("lptp1".into()),
    })));
    snapshot(model, egui::Theme::Light, "adding-a-computer-joined");
}

#[test]
fn snapshot_removed_and_error() {
    let removed = Status { removed: true, run_state: RunState::Paused { reason: "the server no longer accepts this device".into() }, ..signed_in() };
    let model = after(&FakeAgent::new(removed), Vec::new());
    assert_eq!(model.screen, Screen::Removed);
    snapshot(model, egui::Theme::Light, "removed");
    let damaged = Status {
        run_state: RunState::Paused { reason: "/home/fabio/.local/share/craft-cloud-agent/account.json is damaged (expected value at line 1 column 2); move it away. Then sign in again".into() },
        ..signed_out()
    };
    snapshot(after(&FakeAgent::new(damaged), Vec::new()), egui::Theme::Light, "error");
}

/// Clicks reach the model as the events they stand for.
#[test]
fn buttons_send_their_events() {
    let mut h = harness(signed_in_model(signed_in()), egui::Theme::Light);
    h.run_steps(2);
    h.get_by_label("Add a computer").click();
    h.run_steps(2);
    h.get_by_label("Pause").click();
    h.run_steps(2);
    h.get_by_label("Remove").click();
    h.run_steps(2);
    let events = std::mem::take(&mut h.state_mut().1);
    assert!(events.contains(&Event::AddComputer) && events.contains(&Event::Pause), "{events:?}");
    assert!(events.iter().any(|e| matches!(e, Event::RemoveDevice(_))), "{events:?}");
    // One Remove: this computer has none.
    assert_eq!(h.query_all_by_label("Remove").count(), 1);

    let mut h = harness(after(&FakeAgent::new(signed_out()), Vec::new()), egui::Theme::Light);
    h.run_steps(2);
    h.get_by_label("Change…").click();
    h.run_steps(2);
    h.get_by_label("Join").click();
    h.run_steps(2);
    let (model, events) = h.state_mut();
    assert_eq!(events.as_slice(), [Event::ChangeFolder, Event::SubmitJoin]);
    assert_eq!(model.update(Event::ChangeFolder), vec![Effect::PickFolder("/home/fabio/Craft Cloud".into())]);
}

#[test]
fn times_and_addresses_read_as_people_say_them() {
    assert_eq!(host("https://cloud.shadows.ch/"), "cloud.shadows.ch");
    assert_eq!(host("http://127.0.0.1:8080"), "127.0.0.1:8080");
    assert_eq!(ago(NOW, NOW - 10_000), "just now");
    assert_eq!(ago(NOW, NOW - 120_000), "2 min ago");
    assert_eq!(ago(NOW, NOW - 3 * 3_600_000), "3 h ago");
    assert_eq!(ago(NOW, NOW - 30 * 3_600_000), "yesterday");
    assert_eq!(ago(NOW, NOW - 5 * 86_400_000), "5 days ago");
    assert_eq!(ago(NOW, NOW + 5_000), "just now", "a clock ahead");
    let (line, _) = overview::sync_line(&Status { online: false, run_state: RunState::Retrying { message: "x".into() }, ..signed_in() });
    assert_eq!(line, "Can't reach cloud.shadows.ch — trying again");
}
