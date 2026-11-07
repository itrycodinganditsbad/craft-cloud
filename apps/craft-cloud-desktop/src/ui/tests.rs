//! Every screen drawn by kittest against the fake agent, compared with the snapshots in `tests/snapshots/`
//! (`UPDATE_SNAPSHOTS=1` writes them anew), and the buttons sending what they should.
//!
//! kittest renders on wgpu with a CPU adapter where there is one (lavapipe on Linux, WARP on Windows). Renderers differ
//! in a few edge pixels, so a snapshot matches when at most [`MAX_DIFFERENT`] of its pixels differ by more than
//! [`TOLERANCE`] in a channel.

use std::path::PathBuf;
use std::sync::OnceLock;

use craft_cloud_app::{AccountUsage, Error};
use craft_cloud_app::{DeviceId, DeviceInfo, PairingInfo, RunState, Status, Uuid};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

use super::*;
use crate::agent::fake::FakeAgent;
use crate::agent::{Agent, Starter, call};
use crate::model::{Effect, JoinDefaults, Model, Screen, Tab, TrashKind};

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
        DeviceInfo {
            id: DeviceId::generate(),
            name: "lptp1".into(),
            created_ms: 0,
            last_used_ms: NOW - 5_000,
            revoked: false,
            current: true,
            client_kind: String::new(),
            platform: String::new(),
            app_version: String::new(),
            revoked_ms: None,
            revoke_reason: None,
        },
        DeviceInfo {
            id: DeviceId::generate(),
            name: "BEAST".into(),
            created_ms: 0,
            last_used_ms: NOW - 3 * 3_600_000,
            revoked: false,
            current: false,
            client_kind: String::new(),
            platform: String::new(),
            app_version: String::new(),
            revoked_ms: None,
            revoke_reason: None,
        },
    ]
}

/// The one wgpu instance, adapter, device and queue every test renders with. Tests run on parallel threads, and
/// creating and dropping a D3D12 device (WARP) per harness at the same time crashed the test process with an access
/// violation. Each harness still gets a renderer of its own on the shared device. It is never dropped, so no device is
/// torn down while other threads render. Every kittest harness of the app, in any test module, must render with it.
pub(super) fn shared_wgpu() -> eframe::egui_wgpu::WgpuSetup {
    static SHARED: OnceLock<eframe::egui_wgpu::WgpuSetupExisting> = OnceLock::new();
    let shared = SHARED.get_or_init(|| {
        let mut setup = egui_kittest::wgpu::default_wgpu_setup();
        // On Windows the snapshots render on WARP (D3D12). Enumerating the OpenGL (EGL) adapter as well makes
        // wgpu-hal lock its context, which panicked with "Could not lock adapter context" while other test threads
        // were busy; no test uses it, so it is not created.
        #[cfg(windows)]
        if let eframe::egui_wgpu::WgpuSetup::CreateNew(create) = &mut setup {
            create.instance_descriptor.backends.remove(eframe::wgpu::Backends::GL);
        }
        let state = egui_kittest::wgpu::create_render_state(setup, eframe::egui_wgpu::RendererOptions::PREDICTABLE);
        eframe::egui_wgpu::WgpuSetupExisting {
            instance: state.instance.clone(),
            adapter: state.adapter.clone(),
            device: state.device.clone(),
            queue: state.queue.clone(),
        }
    });
    eframe::egui_wgpu::WgpuSetup::Existing(shared.clone())
}

/// The harness: the window drawn for the model, its events kept.
fn harness(model: Model, theme: egui::Theme) -> Harness<'static, (Model, Vec<Event>)> {
    harness_sized(model, theme, WINDOW_SIZE)
}

/// [`harness`] for a window of another size.
fn harness_sized(model: Model, theme: egui::Theme, size: [f32; 2]) -> Harness<'static, (Model, Vec<Event>)> {
    Harness::builder().with_size(egui::Vec2::from(size)).with_theme(theme).wgpu_setup(shared_wgpu()).build_ui_state(
        |ui, (model, events): &mut (Model, Vec<Event>)| {
            events.extend(show(ui, model, NOW));
        },
        (model, Vec::new()),
    )
}

/// The harness in a window tall enough for a whole page without scrolling: kittest clicks only what is on screen, and the
/// kit's controls are taller than they were.
fn tall_harness(model: Model, theme: egui::Theme) -> Harness<'static, (Model, Vec<Event>)> {
    Harness::builder().with_size(egui::vec2(WINDOW_SIZE[0], 1400.0)).with_theme(theme).wgpu_setup(shared_wgpu()).build_ui_state(
        |ui, (model, events): &mut (Model, Vec<Event>)| {
            events.extend(show(ui, model, NOW));
        },
        (model, Vec::new()),
    )
}

/// Draws `model` and compares it with the snapshot `name`.
fn snapshot(model: Model, theme: egui::Theme, name: &str) {
    snapshot_sized(model, theme, name, WINDOW_SIZE);
}

/// [`snapshot`] of a window of another size.
fn snapshot_sized(model: Model, theme: egui::Theme, name: &str, size: [f32; 2]) {
    let mut h = harness_sized(model, theme, size);
    h.run_steps(4);
    let image = h.render().unwrap();
    compare(image, name);
}

/// Compares `image` with the snapshot `name` (or writes it with `UPDATE_SNAPSHOTS=1`).
pub(super) fn compare(image: image::RgbaImage, name: &str) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("snapshots");
    let path = dir.join(format!("{name}.png"));
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        if std::env::var("UPDATE_SNAPSHOT_ONLY").ok().is_some_and(|names| !names.split(',').any(|allowed| allowed == name)) {
            return;
        }
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
    model.update(Event::Joined(Err(craft_cloud_app::Error::Refused {
        code: "folder_has_files".into(),
        message: "has files".into(),
        name_taken: None,
    })));
    snapshot(model.clone(), egui::Theme::Light, "join-folder-has-files");
    model.update(Event::SyncExistingFiles(false));
    sent(&mut model, "cloud.shadows.ch/K7M4-Q9TX");
    model.update(Event::Joined(Err(craft_cloud_app::Error::Refused { code: "invalid_code".into(), message: "x".into(), name_taken: None })));
    snapshot(model, egui::Theme::Dark, "join-invalid-code-dark");
}

#[test]
fn snapshot_password_needed() {
    let mut model = after(&FakeAgent::new(signed_out()), Vec::new());
    sent(&mut model, "cloud.shadows.ch/K7M4-Q9TX-K7M4-Q9TX");
    model.update(Event::Joined(Err(craft_cloud_app::Error::Refused { code: "password_required".into(), message: "x".into(), name_taken: None })));
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

fn usage(stored_bytes: u64, quota_bytes: Option<u64>, complete: bool) -> AccountUsage {
    AccountUsage {
        stored_bytes,
        quota_bytes,
        trash_bytes: complete.then_some(20),
        history_bytes: complete.then_some(30),
        estimates: craft_cloud_proto::api::UsageEstimates {
            index_complete: complete,
            reason: (!complete).then_some(craft_cloud_proto::api::UsageEstimateReason::IndexIncomplete),
        },
    }
}

#[test]
fn snapshot_storage_states() {
    let with_quota = signed_in_with_usage(Ok(usage(11, Some(100), true)));
    snapshot(with_quota, egui::Theme::Light, "storage-with-quota");

    let no_quota = signed_in_with_usage(Ok(usage(11, None, true)));
    snapshot(no_quota, egui::Theme::Light, "storage-no-quota");

    let estimates_unavailable = signed_in_with_usage(Ok(usage(11, Some(100), false)));
    snapshot(estimates_unavailable, egui::Theme::Light, "storage-estimates-unavailable");

    let error = signed_in_with_usage(Err(Error::Refused { code: "offline".into(), message: "can't reach the server".into(), name_taken: None }));
    snapshot(error, egui::Theme::Light, "storage-error");
}

fn signed_in_with_usage(result: Result<AccountUsage, Error>) -> Model {
    let agent = FakeAgent::new(signed_in());
    *agent.usage_result.lock().unwrap() = Some(result);
    after(&agent, Vec::new())
}

#[test]
fn storage_refresh_requests_a_fresh_server_answer() {
    let mut model = signed_in_model(signed_in());
    assert_eq!(model.update(Event::RefreshUsage), vec![Effect::Usage(2)]);
    let Screen::SignedIn(overview) = model.screen else { panic!("signed-in screen expected") };
    assert!(overview.usage_loading);
    assert_eq!(overview.usage_error, None);
}

#[test]
fn storage_refresh_click_sends_a_request_and_loading_blocks_duplicates() {
    let mut h = harness(signed_in_model(signed_in()), egui::Theme::Light);
    h.run_steps(2);
    h.query_all_by_label("Refresh").next().unwrap().click();
    h.run_steps(2);
    let (model, events) = h.state_mut();
    assert!(events.contains(&Event::RefreshUsage));
    assert_eq!(model.update(Event::RefreshUsage), vec![Effect::Usage(2)]);
    assert!(model.update(Event::RefreshUsage).is_empty(), "already loading blocks repeat request");
}

#[test]
fn stale_refresh_answer_cannot_replace_a_newer_answer() {
    let mut model = Model::new(defaults(), true, String::new());
    let first = model
        .update(Event::Found(Ok(signed_in())))
        .into_iter()
        .find_map(|effect| match effect {
            Effect::Usage(id) => Some(id),
            _ => None,
        })
        .expect("signed-in overview asks for usage");
    model.update(Event::Usage { request_id: first, result: Ok(usage(11, None, true)) });
    let second = model
        .update(Event::RefreshUsage)
        .into_iter()
        .find_map(|effect| match effect {
            Effect::Usage(id) => Some(id),
            _ => None,
        })
        .expect("refresh asks again");
    model.update(Event::Usage { request_id: second, result: Ok(usage(25, None, true)) });
    model.update(Event::Usage {
        request_id: first,
        result: Err(Error::Refused { code: "offline".into(), message: "old error".into(), name_taken: None }),
    });
    let Screen::SignedIn(overview) = &model.screen else { panic!("signed-in screen expected") };
    assert!(!overview.usage_loading);
    assert_eq!(overview.usage_error, None);
    assert_eq!(overview.usage.as_ref().map(|value| value.stored_bytes), Some(25));
}

#[test]
fn usage_answer_from_previous_sign_in_cannot_replace_new_account_state() {
    let mut model = Model::new(defaults(), true, String::new());
    let old_request = model
        .update(Event::Found(Ok(signed_in())))
        .into_iter()
        .find_map(|effect| match effect {
            Effect::Usage(id) => Some(id),
            _ => None,
        })
        .expect("signed-in overview asks for usage");
    model.update(Event::Polled(Ok(signed_out())));
    let new_request = model
        .update(Event::Polled(Ok(signed_in())))
        .into_iter()
        .find_map(|effect| match effect {
            Effect::Usage(id) => Some(id),
            _ => None,
        })
        .expect("new sign-in asks for usage");
    model.update(Event::Usage { request_id: new_request, result: Ok(usage(20, None, true)) });
    model.update(Event::Usage { request_id: old_request, result: Ok(usage(999, None, true)) });
    let Screen::SignedIn(overview) = &model.screen else { panic!("signed-in screen expected") };
    assert!(!overview.usage_loading);
    assert_eq!(overview.usage.as_ref().map(|value| value.stored_bytes), Some(20));
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

fn versions_model(loaded: bool) -> Model {
    let mut model = signed_in_model(signed_in());
    model.update(Event::OpenVersions);
    model.update(Event::VersionsPath("Projects/painting.png".into()));
    if loaded {
        let versions = vec![
            craft_cloud_app::HistoryVersion {
                id: craft_cloud_app::VersionId::from_uuid(Uuid::nil()),
                parent_version: None,
                epoch: craft_cloud_proto::Epoch::from_uuid(Uuid::nil()),
                seq: 9,
                size: 42_000,
                mtime_ms: NOW,
                device: DeviceId::from_uuid(Uuid::nil()),
                created_ms: NOW - 30_000,
                save: false,
                pruned: false,
            },
            craft_cloud_app::HistoryVersion {
                id: craft_cloud_app::VersionId::from_uuid(Uuid::from_u128(1)),
                parent_version: None,
                epoch: craft_cloud_proto::Epoch::from_uuid(Uuid::nil()),
                seq: 8,
                size: 38_000,
                mtime_ms: NOW - 60_000,
                device: DeviceId::from_uuid(Uuid::from_u128(1)),
                created_ms: NOW - 60_000,
                save: true,
                pruned: false,
            },
        ];
        model.update(Event::History { ticket: 1, result: Ok(craft_cloud_app::HistoryResponse { versions, next: Some("next-page".into()) }) });
    }
    model
}

#[test]
fn snapshot_versions() {
    snapshot(versions_model(false), egui::Theme::Light, "versions-empty");
    snapshot(versions_model(true), egui::Theme::Light, "versions-loaded");
    let mut model = versions_model(true);
    if let Screen::Versions(versions) = &mut model.screen {
        versions.confirm = versions.page.as_ref().and_then(|page| page.versions.get(1)).map(|version| version.id);
    }
    snapshot(model, egui::Theme::Light, "versions-confirm");
}

fn trash_model(loaded: bool) -> Model {
    let mut model = signed_in_model(signed_in());
    let effects = model.update(Event::OpenTrash);
    let ticket = match effects.first() {
        Some(Effect::Trash { ticket, .. }) => *ticket,
        _ => 0,
    };
    let items = if loaded {
        vec![
            craft_cloud_app::TrashItem {
                id: craft_cloud_app::NodeId::from_uuid(Uuid::nil()),
                parent: craft_cloud_app::NodeId::from_uuid(Uuid::from_u128(1)),
                name: "painting.png".into(),
                is_dir: false,
                size: Some(42_000),
                deleted_ms: NOW - 30_000,
                purge_after_ms: NOW + 14 * 86_400_000,
                path: Some("Projects/painting.png".into()),
            },
            craft_cloud_app::TrashItem {
                id: craft_cloud_app::NodeId::from_uuid(Uuid::from_u128(2)),
                parent: craft_cloud_app::NodeId::from_uuid(Uuid::from_u128(3)),
                name: "Old exports".into(),
                is_dir: true,
                size: None,
                deleted_ms: NOW - 86_400_000,
                purge_after_ms: NOW + 13 * 86_400_000,
                path: None,
            },
        ]
    } else {
        Vec::new()
    };
    model.update(Event::TrashPage { ticket, result: Ok(craft_cloud_app::TrashResponse { items, next: loaded.then(|| "trash-next".into()) }) });
    model
}

#[test]
fn snapshot_trash() {
    snapshot(trash_model(false), egui::Theme::Light, "trash-empty");
    snapshot(trash_model(true), egui::Theme::Light, "trash-loaded");
    let mut model = trash_model(true);
    let id = match &model.screen {
        Screen::Trash(t) => t.page.as_ref().and_then(|p| p.items.first()).map(|i| i.id),
        _ => None,
    };
    if let Some(id) = id {
        model.update(Event::ConfirmRemoveTrash(id));
        snapshot(model.clone(), egui::Theme::Light, "trash-confirm");
        model.update(Event::CancelRemoveTrash);
        let effect = model.update(Event::RestoreTrash(id));
        let ticket = match effect.first() {
            Some(Effect::RestoreTrash { ticket, .. }) => *ticket,
            _ => 0,
        };
        model.update(Event::TrashRestored {
            ticket,
            result: Err(craft_cloud_app::Error::Refused { code: "conflict".into(), message: "name taken".into(), name_taken: Some(true) }),
        });
        snapshot(model.clone(), egui::Theme::Light, "trash-name-taken");
        let restored = craft_cloud_app::TrashRestoreResponse {
            node: id,
            parent: craft_cloud_app::NodeId::from_uuid(Uuid::from_u128(1)),
            name: "painting (1).png".into(),
            renamed: true,
            version: None,
            seq: 2,
            epoch: craft_cloud_proto::Epoch::from_uuid(Uuid::nil()),
        };
        let effect = model.update(Event::RestoreTrash(id));
        let ticket = match effect.first() {
            Some(Effect::RestoreTrash { ticket, .. }) => *ticket,
            _ => 0,
        };
        model.update(Event::TrashRestored { ticket, result: Ok(restored) });
        snapshot(model, egui::Theme::Light, "trash-restored");
    } else {
        snapshot(model, egui::Theme::Light, "trash-confirm");
    }
}

#[test]
fn trash_buttons_send_restore_and_confirmation_events() {
    let mut h = harness(trash_model(true), egui::Theme::Light);
    h.run_steps(2);
    h.query_all_by_label("Restore").next().unwrap().click();
    h.run_steps(2);
    h.query_all_by_label("Remove").next().unwrap().click();
    h.run_steps(2);
    let events = std::mem::take(&mut h.state_mut().1);
    assert!(events.iter().any(|e| matches!(e, Event::RestoreTrash(_))), "{events:?}");
    assert!(events.contains(&Event::ConfirmRemoveTrash(craft_cloud_app::NodeId::from_uuid(Uuid::nil()))), "{events:?}");
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
    let mut h = tall_harness(signed_in_model(signed_in()), egui::Theme::Light);
    h.run_steps(2);
    h.get_by_label("Add a computer").click();
    h.run_steps(2);
    h.get_by_label("Pause").click();
    h.run_steps(2);
    let events = std::mem::take(&mut h.state_mut().1);
    assert!(events.contains(&Event::AddComputer) && events.contains(&Event::Pause), "{events:?}");
    // The overview lists the computers; Remove is on the Computers tab.
    assert_eq!(h.query_all_by_label("Remove").count(), 0);
    let mut computers = signed_in_model(signed_in());
    computers.update(Event::Tab(Tab::Computers));
    let mut h = harness(computers, egui::Theme::Light);
    h.run_steps(2);
    h.get_by_label("Remove").click();
    h.run_steps(2);
    let events = std::mem::take(&mut h.state_mut().1);
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

// ---- the admin pages ----

use crate::admin::{Action, AdminEvent, CodeKind, RoleChoice, View};
use crate::agent::fake::sample_user;
use craft_cloud_app::AdminUserInfo;

/// An agent signed in as `fabio`, an admin, with a few more accounts.
fn admin_agent() -> FakeAgent {
    let agent = FakeAgent::new(signed_in());
    agent.admin.store(true, std::sync::atomic::Ordering::SeqCst);
    let account = |name: &str, f: &dyn Fn(&mut AdminUserInfo)| {
        let mut user = sample_user(name, Uuid::now_v7());
        f(&mut user);
        user
    };
    *agent.users.lock().unwrap() = vec![
        account("fabio", &|u| {
            u.role = "admin".into();
            u.devices_active = 2;
            u.stored_bytes = 3_400_000_000;
        }),
        account("ana", &|u| {
            u.devices_active = 1;
            u.stored_bytes = 640_000_000;
            u.quota_bytes = Some(5 * 1_073_741_824);
        }),
        account("ben", &|u| {
            u.devices_active = 0;
            u.disabled_ms = Some(1_789_000_000_000);
        }),
        account("cleo.m", &|u| {
            u.devices_active = 0;
            u.invite_pending = true;
        }),
    ];
    agent
}

fn on_admin_pages(agent: &FakeAgent, more: Vec<Event>) -> Model {
    let mut events = vec![Event::Devices(Ok(devices())), Event::Admin(AdminEvent::Open)];
    events.extend(more);
    after(agent, events)
}

fn id_of(model: &Model, name: &str) -> Uuid {
    model.admin_screen().unwrap().list.users.iter().find(|u| u.username == name).unwrap().id
}

fn password_typed(model: &mut Model, password: &str) {
    let Screen::Admin(admin) = &mut model.screen else { panic!("{:?}", model.screen) };
    admin.dialog.as_mut().unwrap().confirmation.password = password.to_owned();
}

/// The model after the change asked for was sent and the fake answered it.
fn confirm_sent(model: &mut Model, agent: &FakeAgent) {
    let effects = model.update(Event::Admin(AdminEvent::Confirm));
    let answer = call(agent, &no_start(), effects.into_iter().next().unwrap()).unwrap();
    model.update(answer);
}

#[test]
fn snapshot_admin_pages() {
    let agent = admin_agent();
    // The signed-in view offers the pages to an admin only.
    let model = after(&agent, vec![Event::Devices(Ok(devices()))]);
    assert!(model.is_admin());
    snapshot(model, egui::Theme::Light, "admin-signed-in");

    let mut model = on_admin_pages(&agent, Vec::new());
    snapshot(model.clone(), egui::Theme::Light, "admin-users");
    snapshot(model.clone(), egui::Theme::Dark, "admin-users-dark");
    snapshot(model.clone(), egui::Theme::Light, "admin-storage");

    model.update(Event::Admin(AdminEvent::NewUser));
    if let Screen::Admin(admin) = &mut model.screen
        && let View::Create(form) = &mut admin.view
    {
        form.username = "dora".into();
        form.role = RoleChoice::Member;
    }
    snapshot(model, egui::Theme::Light, "admin-new-account");

    let ana = id_of(&on_admin_pages(&agent, Vec::new()), "ana");
    let mut model = on_admin_pages(&agent, vec![Event::Admin(AdminEvent::OpenUser(ana))]);
    snapshot(model.clone(), egui::Theme::Light, "admin-account");
    snapshot(model.clone(), egui::Theme::Light, "admin-account-usage");
    if let Screen::Admin(admin) = &mut model.screen
        && let View::User(view) = &mut admin.view
    {
        view.usage = Some(craft_cloud_app::AccountUsage {
            stored_bytes: 640_000_000,
            quota_bytes: Some(5 * 1_073_741_824),
            trash_bytes: None,
            history_bytes: None,
            estimates: craft_cloud_app::UsageEstimates { index_complete: false, reason: Some(craft_cloud_app::UsageEstimateReason::IndexIncomplete) },
        });
        view.usage_loading = false;
    }
    snapshot(model.clone(), egui::Theme::Light, "admin-account-usage-unavailable");

    // A change asks for the password; the wrong one is said in the words of the server's refusal.
    model.update(Event::Admin(AdminEvent::Ask(Action::Disable { id: ana, username: "ana".into() })));
    password_typed(&mut model, "not-my-password");
    snapshot(model.clone(), egui::Theme::Light, "admin-confirm");
    let error = craft_cloud_app::Error::Refused { code: "wrong_password".into(), message: "the password is wrong".into(), name_taken: None };
    agent.admin_fails.lock().unwrap().push(error);
    confirm_sent(&mut model, &agent);
    snapshot(model, egui::Theme::Light, "admin-confirm-wrong-password");

    // The invite a new account got, shown once.
    let mut model = on_admin_pages(&agent, vec![Event::Admin(AdminEvent::NewUser)]);
    if let Screen::Admin(admin) = &mut model.screen
        && let View::Create(form) = &mut admin.view
    {
        form.username = "dora".into();
    }
    model.update(Event::Admin(AdminEvent::SubmitCreate));
    password_typed(&mut model, "admin-secret-pw");
    confirm_sent(&mut model, &agent);
    let code = model.admin_screen().unwrap().code.clone().unwrap();
    assert_eq!((code.username.as_str(), code.kind), ("dora", CodeKind::Invite));
    snapshot(model, egui::Theme::Light, "admin-invite-code");
}

#[test]
fn account_estimates_are_not_added_to_stored_total() {
    let agent = admin_agent();
    let ana = id_of(&on_admin_pages(&agent, Vec::new()), "ana");
    let model = on_admin_pages(&agent, vec![Event::Admin(AdminEvent::OpenUser(ana))]);
    let mut h = harness(model, egui::Theme::Light);
    h.run_steps(4);
    assert_eq!(h.query_all_by_label("Stored: 610 MiB").count(), 1, "estimates stay separate from stored bytes");
    assert_eq!(h.query_all_by_label("Trash estimate (overlapping): 76.3 MiB").count(), 1);
    assert_eq!(h.query_all_by_label("History estimate (overlapping): 229 MiB").count(), 1);
}

/// A member never sees the Admin tab.
#[test]
fn a_member_has_no_admin_tab() {
    let agent = admin_agent();
    agent.admin.store(false, std::sync::atomic::Ordering::SeqCst);
    let model = after(&agent, vec![Event::Devices(Ok(devices()))]);
    assert!(!model.is_admin());
    let mut h = harness(model, egui::Theme::Light);
    h.run_steps(2);
    assert_eq!(h.query_all_by_label("Admin").count(), 0);
    assert_eq!(h.query_all_by_label("Activity").count(), 1, "everyone has the other tabs");
}

/// The admin pages' buttons send the events they stand for.
#[test]
fn admin_buttons_send_their_events() {
    let agent = admin_agent();
    let model = after(&agent, vec![Event::Devices(Ok(devices()))]);
    let mut h = tall_harness(model, egui::Theme::Light);
    h.run_steps(2);
    h.get_by_label("Admin").click();
    h.run_steps(2);
    assert_eq!(std::mem::take(&mut h.state_mut().1), [Event::Tab(Tab::Admin)]);

    let model = on_admin_pages(&agent, Vec::new());
    let ana = id_of(&model, "ana");
    let fabio = id_of(&model, "fabio");
    let mut h = tall_harness(model, egui::Theme::Light);
    h.run_steps(2);
    h.get_by_label("New account").click();
    h.run_steps(2);
    h.get_by_label("Search").click();
    h.run_steps(2);
    h.get_by_label("Refresh").click();
    h.run_steps(2);
    // One Manage per account.
    assert_eq!(h.query_all_by_label("Manage").count(), 4);
    h.query_all_by_label("Manage").nth(1).unwrap().click();
    h.run_steps(2);
    // The Overview tab is the way out; the list has no Back of its own.
    h.get_by_label("Overview").click();
    h.run_steps(2);
    let events = std::mem::take(&mut h.state_mut().1);
    assert_eq!(
        events,
        [
            Event::Admin(AdminEvent::NewUser),
            Event::Admin(AdminEvent::Search),
            Event::Admin(AdminEvent::Reload),
            Event::Admin(AdminEvent::OpenUser(ana)),
            Event::Tab(Tab::Overview),
        ]
    );

    // Ana is enabled and has a computer: the buttons ask for the password of their change.
    let model = on_admin_pages(&agent, vec![Event::Admin(AdminEvent::OpenUser(ana))]);
    let mut h = tall_harness(model, egui::Theme::Light);
    h.run_steps(2);
    for label in ["Save changes", "Disable", "Remove", "New invite", "Password reset code", "Revoke open code", "Revoke"] {
        // The page is taller than the window now that the header and the footer take their share: scroll to the button.
        h.get_by_label(label).scroll_to_me();
        h.run_steps(5);
        h.get_by_label(label).click();
        h.run_steps(2);
    }
    let events = std::mem::take(&mut h.state_mut().1);
    let asked: Vec<&Action> = events.iter().filter_map(|e| if let Event::Admin(AdminEvent::Ask(a)) = e { Some(a) } else { None }).collect();
    assert!(
        matches!(
            asked.as_slice(),
            [
                Action::Disable { .. },
                Action::Remove { .. },
                Action::IssueInvite { .. },
                Action::IssueReset { .. },
                Action::RevokeInvite { .. },
                Action::RevokeDevice { .. }
            ]
        ),
        "{events:?}"
    );
    assert!(events.contains(&Event::Admin(AdminEvent::SubmitEdit)));
    // Your own account has no Disable or Remove.
    let model = on_admin_pages(&agent, vec![Event::Admin(AdminEvent::OpenUser(fabio))]);
    let mut h = tall_harness(model, egui::Theme::Light);
    h.run_steps(2);
    assert_eq!(h.query_all_by_label("Disable").count() + h.query_all_by_label("Remove").count(), 0);

    // The password page: the confirm button and Cancel.
    let mut model = on_admin_pages(&agent, Vec::new());
    model.update(Event::Admin(AdminEvent::Ask(Action::Enable { id: ana, username: "ana".into() })));
    password_typed(&mut model, "pw");
    let mut h = tall_harness(model, egui::Theme::Light);
    h.run_steps(2);
    h.get_by_label("Enable").click();
    h.run_steps(2);
    h.get_by_label("Cancel").click();
    h.run_steps(2);
    assert_eq!(std::mem::take(&mut h.state_mut().1), [Event::Admin(AdminEvent::Confirm), Event::Admin(AdminEvent::CancelDialog)]);

    // A code shown: Done forgets it.
    let mut model = on_admin_pages(&agent, Vec::new());
    model.update(Event::Admin(AdminEvent::Ask(Action::IssueReset { id: ana, username: "ana".into(), reset_two_factor: false })));
    password_typed(&mut model, "pw");
    confirm_sent(&mut model, &agent);
    let mut h = tall_harness(model, egui::Theme::Light);
    h.run_steps(2);
    h.get_by_label("Done").click();
    h.run_steps(2);
    assert_eq!(std::mem::take(&mut h.state_mut().1), [Event::Admin(AdminEvent::DismissCode)]);
}

/// egui keeps a text field's undo history, which holds what was typed: for the password field it is dropped once the
/// password was sent, or the page cancelled or left, so Undo can't bring a password back to a field opened later.
#[test]
fn a_typed_password_is_not_left_in_eguis_undo_history() {
    use egui::accesskit::Role;
    use egui::text_edit::TextEditState;

    let agent = admin_agent();
    let model = on_admin_pages(&agent, Vec::new());
    let ana = id_of(&model, "ana");
    let id = super::security::secret_id(super::admin::PASSWORD_FIELD);
    let ask = |h: &mut Harness<'static, (Model, Vec<Event>)>| {
        h.state_mut().0.update(Event::Admin(AdminEvent::Ask(Action::Disable { id: ana, username: "ana".into() })));
        h.run_steps(3);
    };
    // Whether egui could still bring typed text back: its state for the field has an undo history for an empty field.
    let remembered = |h: &Harness<'static, (Model, Vec<Event>)>| {
        TextEditState::load(&h.ctx, id).is_some_and(|state| state.undoer().undo(&(egui::text::CCursorRange::default(), String::new())).is_some())
    };
    let password =
        |h: &Harness<'static, (Model, Vec<Event>)>| h.state().0.admin_screen().unwrap().dialog.as_ref().unwrap().confirmation.password.clone();
    let undo = |h: &mut Harness<'static, (Model, Vec<Event>)>| {
        h.get_by_role(Role::PasswordInput).focus();
        h.run_steps(2);
        h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Z);
        h.run_steps(3);
    };

    let mut h = harness(model, egui::Theme::Light);
    h.run_steps(2);
    assert!(!remembered(&h));
    ask(&mut h);
    h.get_by_role(Role::PasswordInput).focus();
    h.get_by_role(Role::PasswordInput).type_text("hunter2-secret");
    // Long enough for egui to settle the typing into an undo point.
    h.run_steps(8);
    assert_eq!(password(&h), "hunter2-secret");
    assert!(remembered(&h), "while it is typed, egui has the field's state (the test sees what it guards against)");

    // Sent, and refused as a wrong password: the dialog is open again, empty, and egui has forgotten the text.
    let effects = h.state_mut().0.update(Event::Admin(AdminEvent::Confirm));
    assert!(matches!(effects.as_slice(), [Effect::AdminDo { .. }]));
    h.run_steps(2);
    assert!(!remembered(&h), "once sent");
    let refused = craft_cloud_app::Error::Refused { code: "wrong_password".into(), message: "no".into(), name_taken: None };
    h.state_mut().0.update(Event::Admin(AdminEvent::Done(Err(refused))));
    h.run_steps(3);
    undo(&mut h);
    assert_eq!(password(&h), "", "Undo brings nothing back");

    // Typed again and cancelled; the same change asked for again: Undo still brings nothing back.
    h.get_by_role(Role::PasswordInput).type_text("another-secret");
    h.run_steps(8);
    assert!(remembered(&h));
    h.state_mut().0.update(Event::Admin(AdminEvent::CancelDialog));
    h.run_steps(2);
    assert!(!remembered(&h), "once cancelled");
    ask(&mut h);
    undo(&mut h);
    assert_eq!(password(&h), "");

    // Left with the password typed: the same.
    h.get_by_role(Role::PasswordInput).type_text("third-secret");
    h.run_steps(3);
    h.state_mut().0.update(Event::Admin(AdminEvent::Close));
    h.run_steps(2);
    assert!(!remembered(&h), "once the pages are left");
}

/// The same for the confirmation's code field (a code from the app, or a recovery code), beside the password's: both are
/// forgotten once the confirmation was sent, cancelled or left, so Undo can't bring either back into a field opened later.
#[test]
fn a_typed_code_is_not_left_in_eguis_undo_history() {
    use egui::accesskit::Role;
    use egui::text_edit::TextEditState;

    let agent = admin_agent();
    agent.two_factor.store(true, std::sync::atomic::Ordering::SeqCst);
    let model = on_admin_pages(&agent, Vec::new());
    assert!(matches!(model.overview().unwrap().access, crate::admin::Access::Admin { two_factor: true }));
    let ana = id_of(&model, "ana");
    let ids = [super::security::secret_id(super::admin::PASSWORD_FIELD), super::security::secret_id(super::admin::CODE_FIELD)];
    let ask = |h: &mut Harness<'static, (Model, Vec<Event>)>| {
        h.state_mut().0.update(Event::Admin(AdminEvent::Ask(Action::Disable { id: ana, username: "ana".into() })));
        h.run_steps(3);
    };
    let remembered = |h: &Harness<'static, (Model, Vec<Event>)>, which: usize| {
        TextEditState::load(&h.ctx, ids[which])
            .is_some_and(|state| state.undoer().undo(&(egui::text::CCursorRange::default(), String::new())).is_some())
    };
    let typed = |h: &Harness<'static, (Model, Vec<Event>)>| {
        let c = &h.state().0.admin_screen().unwrap().dialog.as_ref().unwrap().confirmation;
        (c.password.clone(), c.code.clone())
    };
    let undo = |h: &mut Harness<'static, (Model, Vec<Event>)>, which: usize| {
        h.get_all_by_role(Role::PasswordInput).nth(which).unwrap().focus();
        h.run_steps(2);
        h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Z);
        h.run_steps(3);
    };

    let mut h = harness(model, egui::Theme::Light);
    h.run_steps(2);
    ask(&mut h);
    assert_eq!(h.query_all_by_role(Role::PasswordInput).count(), 2, "a password field and a code field");
    h.get_all_by_role(Role::PasswordInput).next().unwrap().focus();
    h.get_all_by_role(Role::PasswordInput).next().unwrap().type_text("hunter2-secret");
    h.run_steps(2);
    h.get_all_by_role(Role::PasswordInput).nth(1).unwrap().focus();
    h.get_all_by_role(Role::PasswordInput).nth(1).unwrap().type_text("918273");
    h.run_steps(8);
    assert_eq!(typed(&h), ("hunter2-secret".to_owned(), "918273".to_owned()));
    assert!(TextEditState::load(&h.ctx, ids[0]).is_some() && remembered(&h, 1), "while they are typed, egui has their state");

    // Sent and refused (a wrong code): both fields are empty and egui has forgotten both texts; Undo brings nothing back.
    let effects = h.state_mut().0.update(Event::Admin(AdminEvent::Confirm));
    assert!(matches!(effects.as_slice(), [Effect::AdminDo { .. }]));
    h.run_steps(2);
    assert!(!remembered(&h, 0) && !remembered(&h, 1), "once sent");
    let refused = craft_cloud_app::Error::Refused { code: "two_factor_invalid".into(), message: "no".into(), name_taken: None };
    h.state_mut().0.update(Event::Admin(AdminEvent::Done(Err(refused))));
    h.run_steps(3);
    for which in [0, 1] {
        undo(&mut h, which);
        assert_eq!(typed(&h), (String::new(), String::new()), "Undo of field {which} brings nothing back");
    }

    // Typed again and cancelled: forgotten; the same change asked for again: nothing comes back.
    h.get_all_by_role(Role::PasswordInput).nth(1).unwrap().focus();
    h.get_all_by_role(Role::PasswordInput).nth(1).unwrap().type_text("ABCD-EFGH-JKMN-PQRS");
    h.run_steps(8);
    assert!(remembered(&h, 1));
    h.state_mut().0.update(Event::Admin(AdminEvent::CancelDialog));
    h.run_steps(2);
    assert!(!remembered(&h, 1), "once cancelled");
    ask(&mut h);
    undo(&mut h, 1);
    assert_eq!(typed(&h).1, "");
}

// ---- the audit log, the account's own activity and the second factor's confirmation ----

use crate::audit::{AuditMsg, ExportState};
use craft_cloud_proto::api::{AuditDetails, AuditEvent};

/// Events as the server writes them, newest first, with the variety the page tells in words.
fn audit_events() -> Vec<AuditEvent> {
    let event = |n: u64, action: &str, outcome: &str, count: u64, details: AuditDetails| AuditEvent {
        outcome: outcome.into(),
        count,
        details,
        at_ms: NOW - (n as i64) * 3_600_000 - 5_000,
        actor_user: (!action.starts_with("cli.") && !action.ends_with("login_failed"))
            .then(|| Uuid::from_u128(0x0123_4567_89ab_cdef_0000_0000_0000_0000 + u128::from(n))),
        target_kind: if action.starts_with("admin.") { "user".into() } else { String::new() },
        target_id: action.starts_with("admin.").then(|| Uuid::from_u128(0xfedc_ba98_7654_3210_0000_0000_0000_0000 + u128::from(n))),
        ..FakeAgent::sample_event(n, action)
    };
    vec![
        event(
            0,
            "admin.role_changed",
            "success",
            1,
            AuditDetails { role_from: Some("member".into()), role_to: Some("admin".into()), ..Default::default() },
        ),
        event(1, "auth.login_failed", "failure", 4, AuditDetails { step: Some("password".into()), ..Default::default() }),
        event(
            2,
            "auth.login",
            "success",
            1,
            AuditDetails { client_kind: Some("desktop".into()), platform: Some("windows".into()), ..Default::default() },
        ),
        event(3, "device.revoked_others", "success", 1, AuditDetails { revoked: Some(2), ..Default::default() }),
        event(4, "admin.quota_changed", "success", 1, AuditDetails { quota_bytes: Some(Some(5_368_709_120)), ..Default::default() }),
        event(5, "account.two_factor_enabled", "success", 1, AuditDetails::default()),
    ]
}

fn audit_model(agent: &FakeAgent, more: Vec<Event>) -> Model {
    *agent.audit_events.lock().unwrap() = audit_events();
    let mut events = vec![Event::Devices(Ok(devices())), Event::Admin(AdminEvent::Open), Event::Audit(AuditMsg::OpenAudit(None))];
    events.extend(more);
    after(agent, events)
}

fn view_of(model: &Model) -> &crate::audit::AuditView {
    match &model.admin_screen().unwrap().view {
        View::Audit(view) => view,
        other => panic!("{other:?}"),
    }
}

#[test]
fn snapshot_audit_and_activity_pages() {
    let agent = admin_agent();
    let mut model = audit_model(&agent, Vec::new());
    snapshot(model.clone(), egui::Theme::Light, "audit-log");
    snapshot(model.clone(), egui::Theme::Dark, "audit-log-dark");

    // Narrowed to one account, with an export that the server cut at its limit, and one that failed.
    let ana = id_of(&model, "ana");
    let mut narrowed = audit_model(&agent, vec![Event::Audit(AuditMsg::OpenAudit(Some((ana, "ana".into()))))]);
    narrowed.update(Event::Audit(AuditMsg::Export));
    crate::audit::tests::picked(&mut narrowed, Some("C:/Users/fabio/craft-cloud-audit.ndjson"));
    snapshot(narrowed.clone(), egui::Theme::Light, "audit-log-exporting");
    crate::audit::tests::finished(&mut narrowed, Ok(craft_cloud_app::ExportOutcome { events: 100_000, truncated: true }));
    assert!(matches!(view_of(&narrowed).export, ExportState::Done { .. }));
    snapshot(narrowed.clone(), egui::Theme::Light, "audit-log-export-truncated");
    narrowed.update(Event::Audit(AuditMsg::DismissExport));
    narrowed.update(Event::Audit(AuditMsg::Export));
    crate::audit::tests::picked(&mut narrowed, Some("out.ndjson"));
    crate::audit::tests::finished(&mut narrowed, Err(craft_cloud_app::Error::Io("the export broke off: connection reset".into())));
    snapshot(narrowed, egui::Theme::Light, "audit-log-export-failed");

    // The account's own activity, for a member too.
    model.update(Event::Admin(AdminEvent::Close));
    *agent.audit_events.lock().unwrap() = audit_events();
    let member = FakeAgent::new(signed_in());
    *member.audit_events.lock().unwrap() = audit_events();
    let activity = after(&member, vec![Event::Devices(Ok(devices())), Event::Audit(AuditMsg::OpenActivity)]);
    assert!(activity.activity_screen().is_some());
    snapshot(activity, egui::Theme::Light, "activity");
}

/// An account with two-factor sign-in: its page says so and offers to turn it off; confirming asks for a code too.
#[test]
fn snapshot_two_factor_in_the_admin_pages() {
    let agent = admin_agent();
    agent.two_factor.store(true, std::sync::atomic::Ordering::SeqCst);
    agent.users.lock().unwrap()[1].two_factor_enabled = true;
    let ana = {
        let model = on_admin_pages(&agent, Vec::new());
        assert!(matches!(model.overview().unwrap().access, crate::admin::Access::Admin { two_factor: true }));
        id_of(&model, "ana")
    };
    let mut model = on_admin_pages(&agent, vec![Event::Admin(AdminEvent::OpenUser(ana))]);
    snapshot(model.clone(), egui::Theme::Light, "admin-account-two-factor");
    model.update(Event::Admin(AdminEvent::Ask(Action::ResetTwoFactor { id: ana, username: "ana".into() })));
    password_typed(&mut model, "not-my-password");
    if let Screen::Admin(admin) = &mut model.screen {
        admin.dialog.as_mut().unwrap().confirmation.code = "123456".into();
    }
    snapshot(model, egui::Theme::Light, "admin-confirm-two-factor");
}

/// The buttons of the audit and activity pages send what they stand for.
#[test]
fn audit_buttons_send_their_events() {
    let agent = admin_agent();
    let model = audit_model(&agent, Vec::new());
    let mut h = harness(model, egui::Theme::Light);
    h.run_steps(2);
    h.get_by_label("Refresh").click();
    h.run_steps(2);
    h.get_by_label("Export…").click();
    h.run_steps(2);
    h.get_by_label("Back to the accounts").click();
    h.run_steps(2);
    assert_eq!(
        std::mem::take(&mut h.state_mut().1),
        [Event::Audit(AuditMsg::Reload), Event::Audit(AuditMsg::Export), Event::Admin(AdminEvent::BackToList)]
    );
    // A page of the account list opens the log, and an account's page opens its own part of it.
    let model = on_admin_pages(&agent, Vec::new());
    let mut h = harness(model, egui::Theme::Light);
    h.run_steps(2);
    h.get_by_label("Audit log").click();
    h.run_steps(2);
    assert_eq!(std::mem::take(&mut h.state_mut().1), [Event::Audit(AuditMsg::OpenAudit(None))]);
    let ana = id_of(&h.state().0, "ana");
    let model = on_admin_pages(&agent, vec![Event::Admin(AdminEvent::OpenUser(ana))]);
    let mut h = harness(model, egui::Theme::Light);
    h.run_steps(2);
    h.get_by_label("Activity of this account").click();
    h.run_steps(2);
    assert_eq!(std::mem::take(&mut h.state_mut().1), [Event::Audit(AuditMsg::OpenAudit(Some((ana, "ana".into()))))]);
    // The Activity tab is for everyone; the Overview tab leaves the activity page.
    let member = FakeAgent::new(signed_in());
    let model = after(&member, vec![Event::Devices(Ok(devices()))]);
    let mut h = harness(model, egui::Theme::Light);
    h.run_steps(2);
    assert_eq!(h.query_all_by_label("Admin").count(), 0);
    h.get_by_label("Activity").click();
    h.run_steps(2);
    assert_eq!(std::mem::take(&mut h.state_mut().1), [Event::Tab(Tab::Activity)]);
    let model = after(&member, vec![Event::Devices(Ok(devices())), Event::Audit(AuditMsg::OpenActivity)]);
    let mut h = harness(model, egui::Theme::Light);
    h.run_steps(2);
    h.get_by_label("Overview").click();
    h.run_steps(2);
    assert_eq!(std::mem::take(&mut h.state_mut().1), [Event::Tab(Tab::Overview)]);
}

// ---- the tabs ----

/// The overview of an account with 1.3 GiB of 5 GiB, a trash and old versions, and two computers.
fn overview_model() -> Model {
    let usage = AccountUsage {
        stored_bytes: 1_395_864_371,
        quota_bytes: Some(5 * 1_073_741_824),
        trash_bytes: Some(124_780_544),
        history_bytes: Some(349_175_808),
        estimates: craft_cloud_proto::api::UsageEstimates { index_complete: true, reason: None },
    };
    let mut model = signed_in_with_usage(Ok(usage));
    model.update(Event::Devices(Ok(devices())));
    model
}

/// Every tab's page, in the light and in the dark appearance.
#[test]
fn snapshot_tabs() {
    let member = FakeAgent::new(signed_in());
    *member.audit_events.lock().unwrap() = audit_events();
    let mut computers = overview_model();
    computers.update(Event::Tab(Tab::Computers));
    let pages = [
        ("overview", overview_model()),
        ("versions", versions_model(true)),
        ("trash", trash_model(true)),
        ("computers", computers),
        ("security", after(&FakeAgent::new(signed_in()), vec![Event::Devices(Ok(devices())), Event::OpenSecurity])),
        ("activity", after(&member, vec![Event::Devices(Ok(devices())), Event::Tab(Tab::Activity)])),
        ("admin", on_admin_pages(&admin_agent(), Vec::new())),
    ];
    for (name, model) in pages {
        let expected = match name {
            "overview" => Tab::Overview,
            "versions" => Tab::Versions,
            "trash" => Tab::Trash,
            "computers" => Tab::Computers,
            "security" => Tab::Security,
            "activity" => Tab::Activity,
            _ => Tab::Admin,
        };
        assert_eq!(model.tab(), Some(expected), "{name}");
        snapshot(model.clone(), egui::Theme::Light, &format!("tab-{name}-light"));
        snapshot(model, egui::Theme::Dark, &format!("tab-{name}-dark"));
    }
}

/// The tab bar draws the pages' names, the chosen one among them, and a click sends the tab.
#[test]
fn the_header_has_the_tabs_the_account_line_and_the_footer_its_buttons() {
    let mut h = harness(overview_model(), egui::Theme::Light);
    h.run_steps(2);
    for label in ["Overview", "Versions", "Trash", "Computers", "Security", "Activity"] {
        assert_eq!(h.query_all_by_label(label).count(), 1, "{label}");
    }
    assert_eq!(h.query_all_by_label("Admin").count(), 0);
    h.get_by_label("Trash").click();
    h.run_steps(2);
    h.get_by_label("Sign out").click();
    h.run_steps(2);
    h.get_by_label("Quit").click();
    h.run_steps(2);
    assert_eq!(std::mem::take(&mut h.state_mut().1), [Event::Tab(Tab::Trash), Event::SignOut, Event::Quit]);
    // The footer and the header stay on the other pages.
    let mut h = harness(trash_model(true), egui::Theme::Light);
    h.run_steps(2);
    assert_eq!(h.query_all_by_label("Sign out").count(), 1);
    assert_eq!(h.query_all_by_label("Overview").count(), 1);
}

/// In the smallest window the header's tabs have a row of their own and the overview's cells sit one under the other.
#[test]
fn snapshot_smallest_window() {
    snapshot_sized(overview_model(), egui::Theme::Light, "tab-overview-smallest-light", MIN_WINDOW_SIZE);
    let admin = on_admin_pages(&admin_agent(), Vec::new());
    snapshot_sized(admin, egui::Theme::Dark, "tab-admin-smallest-dark", MIN_WINDOW_SIZE);
}

/// A long warning with an unbreakable path in the sync cell stays left-aligned: the cells sit in `ui.columns`, whose
/// justified layout once spread the words and, on a long path, single letters across the line.
#[test]
fn snapshot_overview_long_autostart_warning() {
    let mut model = overview_model();
    model.autostart_problem = Some(format!(
        "This computer won't sync after a restart: starting at login couldn't be turned on (this agent uses its own data folder ({}): only the agent with the default data folder starts at login, so its entry is left as it is)",
        r"C:\Users\Mattis\AppData\Local\Temp\craft-cloud-codex-ui2-20261107\profile-A\agent"
    ));
    snapshot_sized(model.clone(), egui::Theme::Dark, "tab-overview-long-warning-960-dark", WINDOW_SIZE);
    snapshot_sized(model, egui::Theme::Dark, "tab-overview-long-warning-640-dark", MIN_WINDOW_SIZE);
}

/// A long state line is cut off in the footer rather than running under Sign out and Quit.
#[test]
fn snapshot_footer_with_a_long_state_at_the_smallest_width() {
    let mut status = signed_in();
    status.online = false;
    status.server = Some("https://a-very-long-server-name.example-company.org".into());
    status.run_state = RunState::Retrying { message: "no route".into() };
    snapshot_sized(signed_in_model(status), egui::Theme::Light, "footer-long-state-640", MIN_WINDOW_SIZE);
}

/// The footer's Sign out works on the pages that don't carry the signed-in view, too.
#[test]
fn the_footers_sign_out_works_on_trash_and_versions() {
    for model in [trash_model(true), versions_model(true)] {
        let mut h = harness(model, egui::Theme::Light);
        h.run_steps(2);
        h.get_by_label("Sign out").click();
        h.run_steps(2);
        let (model, events) = h.state_mut();
        assert_eq!(std::mem::take(events), [Event::SignOut]);
        assert_eq!(model.update(Event::SignOut), vec![Effect::Logout { local: false }]);
    }
}

/// A failed Sign out is said on every page, with the offer to sign out on this computer only.
#[test]
fn a_failed_sign_out_is_said_on_the_security_page_and_offers_this_computer_only() {
    let mut model = after(&FakeAgent::new(signed_in()), vec![Event::Devices(Ok(devices())), Event::OpenSecurity]);
    model.update(Event::SignOut);
    let why = "can't reach the server to sign this device out";
    model.update(Event::SignedOut(Err(craft_cloud_app::Error::Refused { code: "offline".into(), message: why.into(), name_taken: None })));
    assert!(matches!(model.screen, Screen::Security(_)));
    let mut h = harness(model, egui::Theme::Light);
    h.run_steps(2);
    let shown = h.state().0.sign_out_error.clone().unwrap();
    assert_eq!(h.query_all_by_label(&shown).count(), 1, "{shown}");
    h.get_by_label("Sign out on this computer only").click();
    h.run_steps(2);
    assert_eq!(std::mem::take(&mut h.state_mut().1), [Event::SignOutHere]);
    // The Computers page has them too.
    let mut model = overview_model();
    model.update(Event::Tab(Tab::Computers));
    model.update(Event::SignOut);
    model.update(Event::SignedOut(Err(craft_cloud_app::Error::Refused { code: "offline".into(), message: why.into(), name_taken: None })));
    let mut h = harness(model, egui::Theme::Light);
    h.run_steps(2);
    assert_eq!(h.query_all_by_label("Sign out on this computer only").count(), 1);
}

/// While a change waits for the server's answer, or a one-time code is shown, the admin pages keep the window: the tabs
/// don't leave them.
#[test]
fn the_tabs_wait_while_the_admin_pages_hold_an_answer_or_a_code() {
    let agent = admin_agent();
    let ana = id_of(&on_admin_pages(&agent, Vec::new()), "ana");
    let mut model = on_admin_pages(&agent, Vec::new());
    model.update(Event::Admin(AdminEvent::Ask(Action::Disable { id: ana, username: "ana".into() })));
    password_typed(&mut model, "pw");
    assert!(!model.tabs_locked(), "a dialog waiting for typing is not a call");
    let effects = model.update(Event::Admin(AdminEvent::Confirm));
    assert!(matches!(effects.as_slice(), [Effect::AdminDo { .. }]));
    assert!(model.tabs_locked());
    let mut h = harness(model, egui::Theme::Light);
    h.run_steps(2);
    h.get_by_label("Overview").click();
    h.run_steps(2);
    let (model, events) = h.state_mut();
    assert!(events.is_empty(), "a disabled tab sends nothing: {events:?}");
    assert!(model.update(Event::Tab(Tab::Overview)).is_empty());
    assert!(model.admin_screen().is_some());

    let mut model = on_admin_pages(&agent, Vec::new());
    model.update(Event::Admin(AdminEvent::Ask(Action::IssueReset { id: ana, username: "ana".into(), reset_two_factor: false })));
    password_typed(&mut model, "pw");
    confirm_sent(&mut model, &agent);
    assert!(model.admin_screen().unwrap().code.is_some());
    assert!(model.tabs_locked());
    assert!(model.update(Event::Tab(Tab::Trash)).is_empty());
    assert!(model.admin_screen().unwrap().code.is_some(), "the code is still there");
    model.update(Event::Admin(AdminEvent::DismissCode));
    assert!(!model.tabs_locked());
}

/// A long account line is cut off with an ellipsis so that no tab leaves the window.
#[test]
fn snapshot_a_long_account_line() {
    let status = Status {
        account: Some("a-rather-long-user-name".into()),
        server: Some("https://cloud.a-rather-long-server-name.example.org".into()),
        ..signed_in()
    };
    let mut model = signed_in_model(status);
    model.update(Event::Devices(Ok(devices())));
    model.overview_mut().unwrap().access = crate::admin::Access::Admin { two_factor: false };
    model.access = crate::admin::Access::Admin { two_factor: false };
    assert!(model.is_admin());
    snapshot(model.clone(), egui::Theme::Light, "tab-overview-long-account-light");
    snapshot_sized(model, egui::Theme::Dark, "tab-overview-long-account-narrow-dark", [760.0, 480.0]);
}

/// The overview with its computers (named `lptp1` and `BEAST`) and the newest activity.
fn overview_with_recent() -> (Model, Vec<DeviceInfo>) {
    let mut model = overview_model();
    let list = devices();
    model.update(Event::Devices(Ok(list.clone())));
    let session = model.overview().expect("the overview").session;
    let mut events = audit_events();
    events.truncate(5);
    for (n, event) in events.iter_mut().enumerate() {
        event.actor_device = list.get(n % 2).map(|d| d.id);
    }
    model.update(Event::Recent { session, result: Ok(craft_cloud_app::AuditPage { events, next: None }) });
    (model, list)
}

/// "02 / Recent activity" under the cells: the newest events with the computers' names, and a muted line without events or when
/// they couldn't be loaded.
#[test]
fn snapshot_recent_activity() {
    let (model, _) = overview_with_recent();
    snapshot_sized(model.clone(), egui::Theme::Light, "overview-recent-light", [WINDOW_SIZE[0], 1100.0]);
    snapshot_sized(model.clone(), egui::Theme::Dark, "overview-recent-dark", [WINDOW_SIZE[0], 1100.0]);
    // At the smallest window and maximized.
    snapshot_sized(model.clone(), egui::Theme::Light, "overview-recent-smallest-light", MIN_WINDOW_SIZE);
    snapshot_sized(model, egui::Theme::Dark, "overview-recent-maximized-dark", [1600.0, 900.0]);
    let mut empty = overview_model();
    let session = empty.overview().expect("the overview").session;
    empty.update(Event::Recent { session, result: Ok(craft_cloud_app::AuditPage { events: Vec::new(), next: None }) });
    snapshot_sized(empty, egui::Theme::Light, "overview-recent-empty", [WINDOW_SIZE[0], 900.0]);
    let mut failed = overview_model();
    failed.update(Event::Recent { session, result: Err(Error::Io("the server didn't answer".into())) });
    snapshot_sized(failed, egui::Theme::Light, "overview-recent-failed", [WINDOW_SIZE[0], 900.0]);
}

/// The page scrolls when it is taller than the window, and its bar sits beside the content, not over its right edge.
#[test]
fn snapshot_overview_scroll_bar_beside_the_content() {
    let (model, _) = overview_with_recent();
    snapshot_sized(model, egui::Theme::Light, "overview-scrolling-960", WINDOW_SIZE);
}

/// "All activity ↗" and the computers link switch tabs.
#[test]
fn the_overviews_links_open_the_tabs() {
    let (model, _) = overview_with_recent();
    let mut h = harness_sized(model, egui::Theme::Light, [WINDOW_SIZE[0], 1100.0]);
    h.run_steps(3);
    h.get_by_label("All activity ↗").click();
    h.run_steps(2);
    h.get_by_label("Computers ↗").click();
    h.run_steps(2);
    let events = std::mem::take(&mut h.state_mut().1);
    assert_eq!(events, [Event::Tab(Tab::Activity), Event::Tab(Tab::Computers)]);
}

/// The activity page names the computers and the account instead of showing their IDs.
#[test]
fn snapshot_activity_with_names() {
    let member = FakeAgent::new(signed_in());
    let list = devices();
    let me = Uuid::from_u128(0x0123_4567_89ab_cdef_0000_0000_0000_0000);
    let mut events = audit_events();
    for (n, event) in events.iter_mut().enumerate() {
        event.actor_user = event.actor_user.map(|_| me);
        // The last one comes from a computer that was removed: its ID stays short.
        event.actor_device = Some(if n == 5 { DeviceId::generate() } else { list[n % 2].id });
    }
    *member.audit_events.lock().unwrap() = events;
    let mut model = after(&member, vec![Event::Devices(Ok(list))]);
    let session = model.overview().expect("the overview").session;
    model.update(Event::Admin(AdminEvent::Account {
        session,
        result: Ok(craft_cloud_proto::api::AccountInfo {
            user_id: me,
            username: "fabio".into(),
            role: "member".into(),
            created_ms: 0,
            password_changed_ms: None,
            two_factor_enabled: false,
        }),
    }));
    let start = no_start();
    let mut pending = model.update(Event::Audit(AuditMsg::OpenActivity));
    while let Some(effect) = pending.pop() {
        if let Some(answer) = call(&member, &start, effect) {
            pending.extend(model.update(answer));
        }
    }
    assert!(model.activity_screen().is_some());
    snapshot_sized(model, egui::Theme::Light, "activity-names", [WINDOW_SIZE[0], 900.0]);
}

/// The trash as a table, with the kind filter and the search field narrowing the loaded list.
#[test]
fn snapshot_trash_table_with_filter_and_search() {
    let mut model = trash_model(true);
    snapshot(model.clone(), egui::Theme::Dark, "trash-table-dark");
    model.update(Event::TrashKind(TrashKind::Files));
    snapshot(model.clone(), egui::Theme::Light, "trash-filter-files");
    model.update(Event::TrashKind(TrashKind::All));
    model.update(Event::TrashSearch("old".into()));
    snapshot(model.clone(), egui::Theme::Light, "trash-search");
    model.update(Event::TrashSearch("zzz".into()));
    snapshot(model, egui::Theme::Light, "trash-search-no-match");
}

fn confirming_trash() -> Model {
    let mut model = trash_model(true);
    let id = match &model.screen {
        Screen::Trash(t) => t.page.as_ref().and_then(|p| p.items.first()).map(|i| i.id),
        _ => None,
    };
    model.update(Event::ConfirmRemoveTrash(id.expect("an item")));
    model
}

fn confirming_restore() -> Model {
    let mut versions = versions_model(true);
    if let Screen::Versions(v) = &mut versions.screen {
        v.confirm = v.page.as_ref().and_then(|page| page.versions.get(1)).map(|version| version.id);
    }
    versions
}

/// The inline confirmations, in the dark appearance (the light ones are `trash-confirm` and `versions-confirm`).
#[test]
fn snapshot_inline_confirmations() {
    snapshot(confirming_trash(), egui::Theme::Dark, "trash-confirm-dark");
    snapshot(confirming_restore(), egui::Theme::Dark, "versions-confirm-dark");
}

/// Esc cancels an open confirmation, and the keyboard focus is inside it from the start (on Cancel, the safe choice).
#[test]
fn escape_cancels_the_inline_confirmations() {
    let mut h = harness(confirming_trash(), egui::Theme::Light);
    h.run_steps(4);
    assert!(h.get_by_label("Cancel").is_focused(), "the focus moves into the panel");
    h.key_press(egui::Key::Escape);
    h.run_steps(3);
    assert!(std::mem::take(&mut h.state_mut().1).contains(&Event::CancelRemoveTrash));

    let mut h = harness(confirming_restore(), egui::Theme::Light);
    h.run_steps(4);
    assert!(h.get_by_label("Cancel").is_focused());
    h.key_press(egui::Key::Escape);
    h.run_steps(3);
    assert!(std::mem::take(&mut h.state_mut().1).contains(&Event::CancelRestore));
}

/// Cancel and the confirming button send their events.
#[test]
fn the_inline_confirmation_buttons_send_their_events() {
    let mut h = harness_sized(confirming_trash(), egui::Theme::Light, [WINDOW_SIZE[0], 1100.0]);
    h.run_steps(3);
    // The table's rows have a Remove each; the panel has one more (the last on the page).
    let removes: Vec<_> = h.query_all_by_label("Remove").collect();
    assert_eq!(removes.len(), 3, "two rows and the panel's");
    removes[2].click();
    h.run_steps(2);
    let events = std::mem::take(&mut h.state_mut().1);
    assert!(events.contains(&Event::RemoveTrash), "{events:?}");
    h.get_by_label("Cancel").click();
    h.run_steps(2);
    assert!(std::mem::take(&mut h.state_mut().1).contains(&Event::CancelRemoveTrash));
}

/// The first run's form in the smallest window overflows: its scroll bar is there and the Join button can be reached.
#[test]
fn snapshot_first_run_in_the_smallest_window_scrolls() {
    let model = after(&FakeAgent::new(signed_out()), Vec::new());
    snapshot_sized(model, egui::Theme::Light, "join-smallest", MIN_WINDOW_SIZE);
}

// ---- the Computers and Security pages ----

/// Three computers as the Computers page shows them: this one, a current agent, and an older one on another platform; and one
/// whose agent sent nothing about itself.
fn computers_with_details() -> Vec<DeviceInfo> {
    let detail = |name: &str, current: bool, platform: &str, version: &str, created_ms: i64, seen_ms: i64| DeviceInfo {
        id: DeviceId::generate(),
        name: name.into(),
        created_ms,
        last_used_ms: seen_ms,
        revoked: false,
        current,
        client_kind: if version.is_empty() { String::new() } else { "agent".into() },
        platform: platform.into(),
        app_version: version.into(),
        revoked_ms: None,
        revoke_reason: None,
    };
    vec![
        detail("Studio PC", true, "Windows", env!("CARGO_PKG_VERSION"), NOW - 3 * 86_400_000, NOW - 5_000),
        detail("dev-B", false, "Windows", env!("CARGO_PKG_VERSION"), NOW - 86_400_000, NOW - 120_000),
        detail("studio-mac", false, "macOS", "0.0.9", NOW - 9 * 86_400_000, NOW - 30 * 3_600_000),
        detail("old-laptop", false, "", "", 0, NOW - 20 * 86_400_000),
    ]
}

fn computers_model() -> Model {
    let mut model = overview_model();
    model.update(Event::Devices(Ok(computers_with_details())));
    model.update(Event::Tab(Tab::Computers));
    model
}

#[test]
fn snapshot_computers() {
    snapshot(computers_model(), egui::Theme::Light, "computers-light");
    snapshot(computers_model(), egui::Theme::Dark, "computers-dark");
    // The row of the second computer is a text field.
    let mut editing = computers_model();
    let id = computers_with_details_ids(&editing)[1];
    editing.update(Event::StartRename(id));
    snapshot(editing.clone(), egui::Theme::Light, "computers-renaming-light");
    // The same row, refused.
    if let Some(rename) = editing.overview_mut().and_then(|o| o.renaming.as_mut()) {
        rename.name = "x".repeat(200);
    }
    editing.update(Event::SaveRename);
    snapshot(editing, egui::Theme::Dark, "computers-renaming-error-dark");
}

/// The ids of the model's listed computers, in order.
fn computers_with_details_ids(model: &Model) -> Vec<DeviceId> {
    model.overview().and_then(|o| o.devices.as_ref()).map(|list| list.iter().map(|d| d.id).collect()).unwrap_or_default()
}

#[test]
fn snapshot_computers_in_the_smallest_window() {
    snapshot_sized(computers_model(), egui::Theme::Light, "computers-smallest-light", MIN_WINDOW_SIZE);
    let mut editing = computers_model();
    let id = computers_with_details_ids(&editing)[0];
    editing.update(Event::StartRename(id));
    snapshot_sized(editing, egui::Theme::Dark, "computers-renaming-smallest-dark", MIN_WINDOW_SIZE);
}

#[test]
fn missing_details_show_a_dash_and_only_an_older_version_gets_the_tag() {
    let mut h = harness(computers_model(), egui::Theme::Light);
    h.run_steps(2);
    // Kind, platform and version of the computer that sent nothing.
    assert_eq!(h.query_all_by_label("—").count(), 3);
    assert_eq!(h.query_all_by_label("Older").count(), 1);
    assert_eq!(h.query_all_by_label("This computer").count(), 1);
    assert_eq!(h.query_all_by_label("Online").count(), 1);
    assert!(h.query_by_label("signed in 1970-01-01").is_none(), "no date is not 1970");
}

#[test]
fn rename_is_found_by_its_label_and_sends_the_events_of_a_rename() {
    let agent = FakeAgent::new(signed_in());
    *agent.devices.lock().unwrap() = computers_with_details();
    let mut model = after(&agent, vec![Event::Tab(Tab::Computers), Event::RefreshDevices]);
    let ids = computers_with_details_ids(&model);
    assert_eq!(ids.len(), 4);
    let mut h = harness(std::mem::replace(&mut model, Model::new(defaults(), true, String::new())), egui::Theme::Light);
    h.run_steps(2);
    // One Rename per computer, one Remove for each but this one.
    assert_eq!(h.query_all_by_label("Rename").count(), 4);
    assert_eq!(h.query_all_by_label("Remove").count(), 3);
    assert!(h.query_by_label("Save").is_none());
    h.get_all_by_label("Rename").nth(1).unwrap().click();
    h.run_steps(2);
    let events = std::mem::take(&mut h.state_mut().1);
    assert_eq!(events, [Event::StartRename(ids[1])]);
    // The model opens the field; the row shows Cancel and Save, and the other rows lose nothing.
    for event in events {
        h.state_mut().0.update(event);
    }
    h.run_steps(3);
    assert!(h.query_by_label("Save").is_some() && h.query_by_label("Cancel").is_some());
    assert_eq!(h.query_all_by_label("Rename").count(), 3);
    assert!(h.query_by_label("Rename this computer").is_some());
    // Cancel.
    h.get_by_label("Cancel").click();
    h.run_steps(2);
    let events = std::mem::take(&mut h.state_mut().1);
    assert_eq!(events, [Event::CancelRename]);
    for event in events {
        h.state_mut().0.update(event);
    }
    h.run_steps(3);
    assert!(h.query_by_label("Save").is_none());
    // Rename again, type, Save: the effect goes through the fake agent and the list takes the answer.
    h.get_all_by_label("Rename").nth(1).unwrap().click();
    h.run_steps(2);
    let events = std::mem::take(&mut h.state_mut().1);
    for event in events {
        h.state_mut().0.update(event);
    }
    h.run_steps(3);
    if let Some(rename) = h.state_mut().0.overview_mut().and_then(|o| o.renaming.as_mut()) {
        rename.name = "Build box".into();
    }
    h.run_steps(2);
    h.get_by_label("Save").click();
    h.run_steps(2);
    let events = std::mem::take(&mut h.state_mut().1);
    assert_eq!(events, [Event::SaveRename]);
    let start = no_start();
    let mut pending = Vec::new();
    for event in events {
        pending.extend(h.state_mut().0.update(event));
    }
    assert_eq!(pending, vec![Effect::RenameDevice { id: ids[1], name: "Build box".into(), ticket: 1 }]);
    while let Some(effect) = pending.pop() {
        if let Some(answer) = call(&agent, &start, effect) {
            pending.extend(h.state_mut().0.update(answer));
        }
    }
    h.run_steps(3);
    assert!(h.query_by_label("Save").is_none());
    assert!(h.query_by_label("Build box").is_some());
    assert!(h.query_by_label("dev-B").is_none());
    assert_eq!(agent.renames.lock().unwrap().as_slice(), [(ids[1], "Build box".to_owned())]);
}

#[test]
fn sign_out_other_computers_is_a_button_of_the_page_and_off_without_another_computer() {
    let mut h = harness(computers_model(), egui::Theme::Light);
    h.run_steps(2);
    h.get_by_label("Sign out other computers").click();
    h.run_steps(2);
    assert_eq!(std::mem::take(&mut h.state_mut().1), [Event::SignOutOtherComputers]);
    let mut alone = overview_model();
    alone.update(Event::Devices(Ok(computers_with_details().into_iter().take(1).collect())));
    alone.update(Event::Tab(Tab::Computers));
    let mut h = harness(alone, egui::Theme::Light);
    h.run_steps(2);
    h.get_by_label("Sign out other computers").click();
    h.run_steps(2);
    assert!(h.state_mut().1.is_empty(), "disabled");
}

#[test]
fn snapshot_security_pages() {
    let on = FakeAgent::new(signed_in());
    on.factor.lock().unwrap().enabled = true;
    let events = |_: ()| vec![Event::Devices(Ok(computers_with_details())), Event::OpenSecurity];
    snapshot(after(&on, events(())), egui::Theme::Light, "security-two-factor-on-light");
    snapshot(after(&on, events(())), egui::Theme::Dark, "security-two-factor-on-dark");
    let off = FakeAgent::new(signed_in());
    snapshot_sized(after(&off, events(())), egui::Theme::Light, "security-two-factor-off-smallest-light", MIN_WINDOW_SIZE);
    let mut password = after(&on, events(()));
    password.update(Event::Security(crate::security::SecurityEvent::StartPassword));
    snapshot(password, egui::Theme::Light, "security-password-light");
}

#[test]
fn escape_cancels_a_rename_and_the_other_rows_wait_while_one_is_open() {
    let mut model = computers_model();
    let ids = computers_with_details_ids(&model);
    model.update(Event::StartRename(ids[1]));
    let mut h = harness(model, egui::Theme::Light);
    h.run_steps(4);
    // The field took the keyboard when the row opened: Escape is Cancel.
    h.key_press(egui::Key::Escape);
    h.run_steps(3);
    let events = std::mem::take(&mut h.state_mut().1);
    assert!(events.contains(&Event::CancelRename), "{events:?}");
    // Another row's Rename is off while this one is open.
    for rename in h.get_all_by_label("Rename") {
        rename.click();
    }
    h.run_steps(2);
    let events = std::mem::take(&mut h.state_mut().1);
    assert!(!events.iter().any(|e| matches!(e, Event::StartRename(_))), "{events:?}");
}

#[test]
fn the_security_pages_sign_out_button_is_off_without_another_computer() {
    let alone = FakeAgent::new(signed_in());
    let events = vec![Event::Devices(Ok(computers_with_details().into_iter().take(1).collect())), Event::OpenSecurity];
    let mut h = harness(after(&alone, events), egui::Theme::Light);
    h.run_steps(3);
    h.get_by_label("Sign out other computers…").click();
    h.run_steps(2);
    assert!(h.state_mut().1.is_empty(), "disabled");
    let with_others = FakeAgent::new(signed_in());
    let events = vec![Event::Devices(Ok(computers_with_details())), Event::OpenSecurity];
    let mut h = harness(after(&with_others, events), egui::Theme::Light);
    h.run_steps(3);
    h.get_by_label("Sign out other computers…").click();
    h.run_steps(2);
    assert_eq!(std::mem::take(&mut h.state_mut().1), [Event::Security(crate::security::SecurityEvent::StartSignOutOthers)]);
}
