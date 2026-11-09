//! Every screen drawn by kittest against the fake agent, compared with the snapshots in `tests/snapshots/`
//! (`UPDATE_SNAPSHOTS=1` writes them anew), and the buttons sending what they should.
//!
//! kittest renders on wgpu with a CPU adapter where there is one (lavapipe on Linux, WARP on Windows). Renderers differ
//! in a few edge pixels, so a snapshot matches when at most [`MAX_DIFFERENT`] of its pixels differ by more than
//! [`TOLERANCE`] in a channel.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use craft_cloud_app::{AccountUsage, Error};
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
        spaces: Vec::new(),
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
/// torn down while other threads render. Every kittest harness of the app, in any test module, must render with it through [`serialized`].
pub(super) fn shared_wgpu() -> eframe::egui_wgpu::WgpuSetup {
    static SHARED: OnceLock<eframe::egui_wgpu::WgpuSetupExisting> = OnceLock::new();
    let shared = SHARED.get_or_init(|| {
        let state =
            egui_kittest::wgpu::create_render_state(egui_kittest::wgpu::default_wgpu_setup(), eframe::egui_wgpu::RendererOptions::PREDICTABLE);
        eframe::egui_wgpu::WgpuSetupExisting {
            instance: state.instance.clone(),
            adapter: state.adapter.clone(),
            device: state.device.clone(),
            queue: state.queue.clone(),
        }
    });
    eframe::egui_wgpu::WgpuSetup::Existing(shared.clone())
}

/// Held while a test touches the GPU. A panicking test poisons it, which says nothing about the GPU, so the poison is ignored.
static GPU: Mutex<()> = Mutex::new(());

fn gpu() -> MutexGuard<'static, ()> {
    GPU.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A kittest renderer on the shared device that does its GPU work one test at a time.
pub(super) struct Serialized(Option<egui_kittest::wgpu::WgpuTestRenderer>);

impl Serialized {
    fn inner(&mut self) -> &mut egui_kittest::wgpu::WgpuTestRenderer {
        self.0.as_mut().expect("the renderer lives until the drop")
    }
}

impl egui_kittest::TestRenderer for Serialized {
    fn handle_delta(&mut self, delta: &mut egui::TexturesDelta) {
        let _gpu = gpu();
        self.inner().handle_delta(delta);
    }

    fn render(&mut self, ctx: &egui::Context, output: &egui::FullOutput) -> Result<image::RgbaImage, String> {
        let _gpu = gpu();
        self.inner().render(ctx, output)
    }
}

impl Drop for Serialized {
    fn drop(&mut self) {
        let _gpu = gpu();
        drop(self.0.take());
    }
}

/// The renderer every kittest harness of the app uses: the shared device, with creating, drawing and dropping the
/// renderer done one test at a time. The DX12 backend of wgpu hands out shader blobs from a cache whose reference
/// counting is not thread-safe, so renderers made on parallel threads corrupt the heap (gfx-rs/wgpu#10547), and drawing
/// in parallel on WARP crashes in its JIT (emilk/egui#8772). The lock can go once a wgpu release with the fix
/// (gfx-rs/wgpu#10805) is in use.
pub(super) fn serialized() -> Serialized {
    let _gpu = gpu();
    Serialized(Some(egui_kittest::wgpu::WgpuTestRenderer::from_setup(shared_wgpu())))
}

/// The harness: the window drawn for the model, its events kept.
fn harness(model: Model, theme: egui::Theme) -> Harness<'static, (Model, Vec<Event>)> {
    Harness::builder().with_size(egui::Vec2::from(WINDOW_SIZE)).with_theme(theme).renderer(serialized()).build_ui_state(
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
    h.query_all_by_label("Remove from trash").next().unwrap().click();
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

/// A member never sees the way in.
#[test]
fn a_member_has_no_administration_button() {
    let agent = admin_agent();
    agent.admin.store(false, std::sync::atomic::Ordering::SeqCst);
    let model = after(&agent, vec![Event::Devices(Ok(devices()))]);
    assert!(!model.is_admin());
    let mut h = harness(model, egui::Theme::Light);
    h.run_steps(2);
    assert_eq!(h.query_all_by_label("Administration").count(), 0);
}

/// The admin pages' buttons send the events they stand for.
#[test]
fn admin_buttons_send_their_events() {
    let agent = admin_agent();
    let model = after(&agent, vec![Event::Devices(Ok(devices()))]);
    let mut h = harness(model, egui::Theme::Light);
    h.run_steps(2);
    h.get_by_label("Administration").click();
    h.run_steps(2);
    assert_eq!(std::mem::take(&mut h.state_mut().1), [Event::Admin(AdminEvent::Open)]);

    let model = on_admin_pages(&agent, Vec::new());
    let ana = id_of(&model, "ana");
    let fabio = id_of(&model, "fabio");
    let mut h = harness(model, egui::Theme::Light);
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
    h.get_by_label("Back").click();
    h.run_steps(2);
    let events = std::mem::take(&mut h.state_mut().1);
    assert_eq!(
        events,
        [
            Event::Admin(AdminEvent::NewUser),
            Event::Admin(AdminEvent::Search),
            Event::Admin(AdminEvent::Reload),
            Event::Admin(AdminEvent::OpenUser(ana)),
            Event::Admin(AdminEvent::Close),
        ]
    );

    // Ana is enabled and has a computer: the buttons ask for the password of their change.
    let model = on_admin_pages(&agent, vec![Event::Admin(AdminEvent::OpenUser(ana))]);
    let mut h = harness(model, egui::Theme::Light);
    h.run_steps(2);
    for label in ["Save changes", "Disable", "Remove", "New invite", "Password reset code", "Revoke open code", "Revoke"] {
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
    let mut h = harness(model, egui::Theme::Light);
    h.run_steps(2);
    assert_eq!(h.query_all_by_label("Disable").count() + h.query_all_by_label("Remove").count(), 0);

    // The password page: the confirm button and Cancel.
    let mut model = on_admin_pages(&agent, Vec::new());
    model.update(Event::Admin(AdminEvent::Ask(Action::Enable { id: ana, username: "ana".into() })));
    password_typed(&mut model, "pw");
    let mut h = harness(model, egui::Theme::Light);
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
    let mut h = harness(model, egui::Theme::Light);
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
    // The signed-in view has an Activity button for everyone; the activity page goes back.
    let member = FakeAgent::new(signed_in());
    let model = after(&member, vec![Event::Devices(Ok(devices()))]);
    let mut h = harness(model, egui::Theme::Light);
    h.run_steps(2);
    assert_eq!(h.query_all_by_label("Administration").count(), 0);
    h.get_by_label("Activity").click();
    h.run_steps(2);
    assert_eq!(std::mem::take(&mut h.state_mut().1), [Event::Audit(AuditMsg::OpenActivity)]);
    let model = after(&member, vec![Event::Devices(Ok(devices())), Event::Audit(AuditMsg::OpenActivity)]);
    let mut h = harness(model, egui::Theme::Light);
    h.run_steps(2);
    h.get_by_label("Back").click();
    h.run_steps(2);
    assert_eq!(std::mem::take(&mut h.state_mut().1), [Event::Audit(AuditMsg::CloseActivity)]);
}
