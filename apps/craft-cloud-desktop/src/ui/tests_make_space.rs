//! "Make a team space from this folder" as kittest draws it (snapshots `make-space-*` and `overview-folders-admin*` in
//! `tests/snapshots/`, written with `UPDATE_SNAPSHOTS=1`), and that its buttons send what they should and are not there for an
//! account that is no administrator. The state and the rules are tested where they live ([`crate::make_space`]).

use craft_cloud_app::{Error, FolderUsage, FolderUsageResponse, FromFolderStatus, FromFolderStep, OtherFolders, SpaceId};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

use super::*;
use crate::admin::tests::{agent as admin_agent, run, signed_in};
use crate::agent::fake::FakeAgent;
use crate::make_space::MakeSpaceMsg;
use crate::model::{Model, Tab};

/// The time the screens are drawn at.
const NOW: i64 = 1_790_000_000_000;
/// A window tall enough for the whole page at the smallest width.
const SMALL: [f32; 2] = [crate::ui::MIN_WINDOW_SIZE[0], 1900.0];
const NORMAL: [f32; 2] = [WINDOW_SIZE[0], 1250.0];

fn harness_sized(model: Model, theme: egui::Theme, size: [f32; 2]) -> Harness<'static, (Model, Vec<Event>)> {
    Harness::builder().with_size(egui::Vec2::from(size)).with_theme(theme).wgpu_setup(super::tests::shared_wgpu()).build_ui_state(
        |ui, (model, events): &mut (Model, Vec<Event>)| {
            events.extend(show(ui, model, NOW));
        },
        (model, Vec::new()),
    )
}

fn shot(model: &Model, theme: egui::Theme, name: &str, size: [f32; 2]) {
    let mut h = harness_sized(model.clone(), theme, size);
    h.run_steps(4);
    let image = h.render().unwrap();
    super::tests::compare(image, name);
}

/// Light and dark at the usual width, and light at the smallest.
fn shots(model: &Model, name: &str) {
    shot(model, egui::Theme::Light, name, NORMAL);
    shot(model, egui::Theme::Dark, &format!("{name}-dark"), NORMAL);
    shot(model, egui::Theme::Light, &format!("{name}-small"), SMALL);
}

fn sizes() -> FolderUsageResponse {
    FolderUsageResponse {
        total_bytes: 1_170_090_000,
        total_files: 679,
        folders: vec![
            FolderUsage { name: "Photos".into(), bytes: 640_000_000, files: 412 },
            FolderUsage { name: "Film".into(), bytes: 410_000_000, files: 6 },
            FolderUsage { name: "Letters".into(), bytes: 100_000_000, files: 200 },
            FolderUsage { name: String::new(), bytes: 20_090_000, files: 61 },
        ],
        other: Some(OtherFolders { folders: 3, bytes: 10_000_000, files: 12 }),
        kinds: Vec::new(),
    }
}

/// The home page of `agent`'s account with the folder list open.
fn with_folders(agent: &FakeAgent) -> Model {
    *agent.folder_sizes.lock().unwrap() = Some(Ok(sizes()));
    let mut model = signed_in(agent);
    run(&mut model, agent, vec![Event::ToggleFolders]);
    model
}

fn admin_home() -> (FakeAgent, Model) {
    let agent = admin_agent(true, 0);
    let model = with_folders(&agent);
    (agent, model)
}

fn status_at(step: FromFolderStep) -> FromFolderStatus {
    FromFolderStatus {
        folder: "/home/fabio/Craft Cloud/Photos".into(),
        name: "Photos team".into(),
        step,
        space: Some(SpaceId::from_uuid(craft_cloud_app::Uuid::from_u128(7))),
        team_folder: Some("/home/fabio/Craft Cloud Shared/Photos team".into()),
        done: 0,
        total: 0,
        failed_at: None,
        message: None,
        kept: 0,
    }
}

fn set_status(model: &mut Model, status: FromFolderStatus) {
    model.overview_mut().unwrap().status.from_folder = Some(status);
}

fn events_of(h: &mut Harness<'static, (Model, Vec<Event>)>) -> Vec<Event> {
    h.run_steps(2);
    std::mem::take(&mut h.state_mut().1)
}

// ---- the screens ----

#[test]
fn snapshot_the_folder_list_offers_a_team_space_to_an_administrator() {
    let (_, model) = admin_home();
    shot(&model, egui::Theme::Light, "overview-folders-admin", NORMAL);
    shot(&model, egui::Theme::Light, "overview-folders-admin-small", SMALL);
}

#[test]
fn snapshot_the_form() {
    let (_, mut model) = admin_home();
    model.update(Event::MakeSpace(MakeSpaceMsg::Open("Photos".into())));
    model.overview_mut().unwrap().make_space.as_mut().unwrap().confirmation.password = "a password".into();
    shots(&model, "make-space-form");
    // With two-factor sign-in the code is asked for too.
    let (_, mut two) = admin_home();
    two.overview_mut().unwrap().access = crate::admin::Access::Admin { two_factor: true };
    two.update(Event::MakeSpace(MakeSpaceMsg::Open("Photos".into())));
    shot(&two, egui::Theme::Light, "make-space-form-code", NORMAL);
}

#[test]
fn snapshot_the_form_refused_and_too_old() {
    let (_, mut model) = admin_home();
    model.update(Event::MakeSpace(MakeSpaceMsg::Open("Photos".into())));
    {
        let form = model.overview_mut().unwrap().make_space.as_mut().unwrap();
        form.error = Some("a team space with this name exists".into());
    }
    shot(&model, egui::Theme::Light, "make-space-form-refused", NORMAL);
    {
        let form = model.overview_mut().unwrap().make_space.as_mut().unwrap();
        form.error = Some(crate::spaces::SERVER_TOO_OLD.into());
        form.old = true;
    }
    shot(&model, egui::Theme::Light, "make-space-form-too-old", NORMAL);
    shot(&model, egui::Theme::Light, "make-space-form-too-old-small", SMALL);
    // Under way: the call is with the agent.
    let mut busy = admin_home().1;
    busy.update(Event::MakeSpace(MakeSpaceMsg::Open("Photos".into())));
    busy.overview_mut().unwrap().make_space.as_mut().unwrap().busy = true;
    shot(&busy, egui::Theme::Light, "make-space-form-busy", NORMAL);
}

#[test]
fn snapshot_the_work_under_way() {
    let (_, mut model) = admin_home();
    let mut status = status_at(FromFolderStep::Copying);
    status.done = 188;
    status.total = 412;
    set_status(&mut model, status);
    shots(&model, "make-space-working");
    let mut syncing = status_at(FromFolderStep::Syncing);
    syncing.done = 400;
    syncing.total = 412;
    set_status(&mut model, syncing);
    shot(&model, egui::Theme::Light, "make-space-syncing", NORMAL);
    set_status(&mut model, status_at(FromFolderStep::Trashing));
    shot(&model, egui::Theme::Light, "make-space-trashing", NORMAL);
}

#[test]
fn snapshot_the_work_done() {
    let (_, mut model) = admin_home();
    set_status(&mut model, status_at(FromFolderStep::Done));
    shots(&model, "make-space-done");
    let mut kept = status_at(FromFolderStep::Done);
    kept.kept = 2;
    set_status(&mut model, kept);
    shot(&model, egui::Theme::Light, "make-space-done-kept", NORMAL);
}

#[test]
fn snapshot_the_work_stopped() {
    let (_, mut model) = admin_home();
    let mut failed = status_at(FromFolderStep::Failed);
    failed.failed_at = Some(FromFolderStep::Syncing);
    failed.message = Some("the team space has taken 188 of 412 files and no more for 10 minutes".into());
    set_status(&mut model, failed);
    shots(&model, "make-space-failed");
    let mut cut = status_at(FromFolderStep::Interrupted);
    cut.failed_at = Some(FromFolderStep::Copying);
    cut.message =
        Some("The agent stopped before this was finished. Your old folder was not changed; ask again to go on with the same team space.".into());
    set_status(&mut model, cut);
    shot(&model, egui::Theme::Light, "make-space-interrupted", NORMAL);
    // Asking again goes on with the space: no password.
    model.update(Event::MakeSpace(MakeSpaceMsg::Retry));
    shot(&model, egui::Theme::Light, "make-space-retry", NORMAL);
}

// ---- what the buttons send ----

#[test]
fn a_folders_button_opens_the_form_and_the_form_sends_its_buttons() {
    let (_, model) = admin_home();
    let mut h = harness_sized(model, egui::Theme::Light, NORMAL);
    h.run_steps(3);
    // Three folders have a button; the top level's loose files and the rows that sum several folders have none.
    assert_eq!(h.query_all_by_label("Team space…").count(), 3);
    h.query_all_by_label("Team space…").next().unwrap().click();
    assert_eq!(events_of(&mut h), vec![Event::MakeSpace(MakeSpaceMsg::Open("Photos".into()))]);
    // The model opens the form (the harness does not run the model, so do it by hand), and its buttons send.
    h.state_mut().0.update(Event::MakeSpace(MakeSpaceMsg::Open("Photos".into())));
    h.state_mut().0.overview_mut().unwrap().make_space.as_mut().unwrap().confirmation.password = "pw".into();
    h.run_steps(3);
    assert_eq!(h.query_all_by_label("Team space…").count(), 3, "the buttons stay while the form is open (the model refuses a second one)");
    h.get_by_label("Make the team space").click();
    h.run_steps(2);
    h.get_by_label("Cancel").click();
    let events = events_of(&mut h);
    assert_eq!(events, vec![Event::MakeSpace(MakeSpaceMsg::Submit), Event::MakeSpace(MakeSpaceMsg::Cancel)]);
}

#[test]
fn the_form_cannot_be_sent_without_a_password_and_nothing_is_offered_to_anyone_else() {
    let (_, mut model) = admin_home();
    model.update(Event::MakeSpace(MakeSpaceMsg::Open("Photos".into())));
    let mut h = harness_sized(model, egui::Theme::Light, NORMAL);
    h.run_steps(3);
    h.get_by_label("Make the team space").click();
    assert!(events_of(&mut h).is_empty(), "no password, nothing is sent");
    // An account that is no administrator sees no button and no form.
    let member = admin_agent(false, 0);
    let plain = with_folders(&member);
    let mut h = harness_sized(plain, egui::Theme::Light, NORMAL);
    h.run_steps(3);
    assert_eq!(h.query_all_by_label("Team space…").count(), 0);
}

#[test]
fn the_notice_buttons_send_what_they_say_and_the_folders_wait_while_the_agent_works() {
    let (_, mut model) = admin_home();
    let mut failed = status_at(FromFolderStep::Failed);
    failed.failed_at = Some(FromFolderStep::Syncing);
    set_status(&mut model, failed);
    let mut h = harness_sized(model.clone(), egui::Theme::Light, NORMAL);
    h.run_steps(3);
    h.get_by_label("Try again").click();
    h.get_by_label("Dismiss").click();
    assert_eq!(events_of(&mut h), vec![Event::MakeSpace(MakeSpaceMsg::Retry), Event::MakeSpace(MakeSpaceMsg::Dismiss)]);

    set_status(&mut model, status_at(FromFolderStep::Done));
    let mut h = harness_sized(model.clone(), egui::Theme::Light, NORMAL);
    h.run_steps(3);
    h.get_by_label("Open the Shared tab").click();
    assert_eq!(events_of(&mut h), vec![Event::Tab(Tab::Shared)]);

    // While the agent works there is no way to start another, and the work has no buttons.
    set_status(&mut model, status_at(FromFolderStep::Copying));
    let mut h = harness_sized(model, egui::Theme::Light, NORMAL);
    h.run_steps(3);
    for button in h.query_all_by_label("Team space…") {
        button.click();
    }
    assert!(events_of(&mut h).is_empty(), "a disabled button sends nothing");
    assert!(h.query_by_label("Dismiss").is_none() && h.query_by_label("Try again").is_none());
    let _: Option<Error> = None;
}
